use crate::mine::format_duration;
use crate::send_and_confirm::{log_error, ComputeBudget};
use crate::utils::{get_config, proof_pubkey};
use crate::Miner;
use crate::{args::MineDistributedArgs, utils::get_proof_with_authority};
use drillx::{equix, Hash, Solution};
use ore_api::state::Proof;
use rand::Rng;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use solana_program::pubkey::Pubkey;
use solana_rpc_client::spinner;
use solana_sdk::signer::Signer;
use std::collections::{HashMap, HashSet};
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::task;
use tokio::time::{sleep, Instant};

#[derive(Serialize, Deserialize)]
struct WorkerResult {
    pub difficulty: u32,
    pub solution: Solution,
    pub total_hashes: u64,
    pub worker_addr: String,
}

#[derive(Serialize, Deserialize, Clone)]
struct WorkerRequest {
    pub proof: SerializableProof,
    pub cutoff_time: u64,
    pub core_offset: u64,
    pub total_cores: u64,
}

#[derive(Serialize, Deserialize)]
struct AuthRequest {
    pub pubkey: String,
    pub cores_count: u64,
    pub worker_name: String,
}

#[derive(Serialize, Deserialize)]
struct AuthResponse {
    pub success: bool,
    pub message: String,
}

#[derive(Serialize, Deserialize, Clone)]
struct SerializableProof {
    pub authority: String,
    pub balance: u64,
    pub challenge: [u8; 32],
    pub last_hash: [u8; 32],
    pub last_hash_at: i64,
    pub last_stake_at: i64,
    pub miner: String,
    pub total_hashes: u64,
    pub total_rewards: u64,
}
#[derive(Clone)]
struct AuthenticatedUser {
    pub pubkey: Pubkey,
    pub cores_count: u64,
    pub worker_name: String,
}

struct WorkerStats {
    pub pubkey: Pubkey,
    pub cores_count: u64,
    pub worker_name: String,
    pub hash_power: f64,
}

impl From<&Proof> for SerializableProof {
    fn from(proof: &Proof) -> Self {
        SerializableProof {
            authority: proof.authority.to_string(),
            balance: proof.balance,
            challenge: proof.challenge,
            last_hash: proof.last_hash,
            last_hash_at: proof.last_hash_at,
            last_stake_at: proof.last_stake_at,
            miner: proof.miner.to_string(),
            total_hashes: proof.total_hashes,
            total_rewards: proof.total_rewards,
        }
    }
}

impl SerializableProof {
    fn to_proof(&self) -> Proof {
        Proof {
            authority: Pubkey::from_str(&self.authority).unwrap(),
            balance: self.balance,
            challenge: self.challenge,
            last_hash: self.last_hash,
            last_hash_at: self.last_hash_at,
            last_stake_at: self.last_stake_at,
            miner: Pubkey::from_str(&self.miner).unwrap(),
            total_hashes: self.total_hashes,
            total_rewards: self.total_rewards,
        }
    }
}

#[derive(Serialize, Deserialize, Debug)]
enum MessageType {
    AuthRequest,
    AuthResponse,
    WorkerRequest,
    WorkerResult,
}

#[derive(Serialize, Deserialize, Debug)]
struct MessageHeader {
    message_type: MessageType,
    payload_size: u32,
}

async fn send_message<T: Serialize>(
    stream: &mut TcpStream,
    message_type: MessageType,
    payload: &T,
) -> std::io::Result<()> {
    let payload_bytes = bincode::serialize(payload).unwrap();
    let header = MessageHeader {
        message_type,
        payload_size: payload_bytes.len() as u32,
    };
    let header_bytes = bincode::serialize(&header).unwrap();

    stream
        .write_all(&(header_bytes.len() as u32).to_be_bytes())
        .await?;
    stream.write_all(&header_bytes).await?;
    stream.write_all(&payload_bytes).await?;
    stream.flush().await?;

    Ok(())
}

async fn receive_message<T: DeserializeOwned>(stream: &mut TcpStream) -> std::io::Result<T> {
    let mut header_size_bytes = [0u8; 4];
    stream.read_exact(&mut header_size_bytes).await?;
    let header_size = u32::from_be_bytes(header_size_bytes) as usize;

    let mut header_bytes = vec![0u8; header_size];
    stream.read_exact(&mut header_bytes).await?;
    let header: MessageHeader = bincode::deserialize(&header_bytes).unwrap();

    let mut payload_bytes = vec![0u8; header.payload_size as usize];
    stream.read_exact(&mut payload_bytes).await?;

    let payload: T = bincode::deserialize(&payload_bytes).unwrap();

    Ok(payload)
}

impl Miner {
    pub async fn mine_distributed(self: Arc<Self>, args: MineDistributedArgs) {
        match args.role.as_str() {
            "coordinator" => self.coordinate(args).await,
            "worker" => self.work(args).await,
            _ => println!("Invalid role. Choose 'coordinator' or 'worker'."),
        }
    }

    async fn coordinate(self: Arc<Self>, args: MineDistributedArgs) {
        let listener = TcpListener::bind(format!(
            "0.0.0.0:{}",
            args.coordinator_port.unwrap_or("8080".to_string())
        ))
        .await
        .unwrap();
        let workers: Arc<
            Mutex<HashMap<String, (mpsc::Sender<WorkerRequest>, AuthenticatedUser, u32, f64)>>,
        > = Arc::new(Mutex::new(HashMap::new()));
        let total_cores: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));
        let global_hash_power: Arc<Mutex<f64>> = Arc::new(Mutex::new(0.0));
        let banned_pubkeys: Arc<Mutex<HashSet<Pubkey>>> = Arc::new(Mutex::new(HashSet::new()));

        let signer = self.signer();

        // Channel for collecting results
        let (result_sender, mut result_receiver) = mpsc::channel(100);

        // Spawn a task to handle new connections
        let worker_handler = Arc::clone(&self);
        let workers_clone = Arc::clone(&workers);
        let result_sender_clone = result_sender.clone();
        let total_cores_clone = Arc::clone(&total_cores);
        let banned_pubkeys_clone = Arc::clone(&banned_pubkeys);
        let global_hash_power_clone = Arc::clone(&global_hash_power);
        task::spawn(async move {
            worker_handler
                .handle_new_connections(
                    listener,
                    workers_clone,
                    result_sender_clone,
                    total_cores_clone,
                    banned_pubkeys_clone,
                    global_hash_power_clone,
                )
                .await;
        });
        let mut previous_proof = Proof {
            authority: Pubkey::default(),
            balance: 0,
            challenge: [0u8; 32],
            last_hash: [0u8; 32],
            last_hash_at: 0,
            last_stake_at: 0,
            miner: Pubkey::default(),
            total_hashes: 0,
            total_rewards: 0,
        };

        loop {
            let progress_bar = spinner::new_progress_bar();
            // Main mining loop
            let proof = get_proof_with_authority(&self.rpc_client, signer.pubkey()).await;
            let cutoff_time = self.get_cutoff(proof, args.buffer_time).await;
            // Reset hash power for all workers at the start of each mining session
            self.reset_hash_powers(&workers).await;

            if proof.eq(&previous_proof) {
                progress_bar
                    .finish_with_message(format!("Proof has not changed. Skipping mining..."));
                sleep(Duration::from_secs(3)).await;
                continue;
            }

            let serializable_proof = SerializableProof::from(&proof);

            let mut worker_offsets = Vec::new();
            let current_total_cores;

            {
                let workers_lock = workers.lock().await;
                let mut offset = 0;
                for (_, (_, authenticated_user, _, _)) in workers_lock.iter() {
                    worker_offsets.push(offset);
                    offset += authenticated_user.cores_count;
                }
                current_total_cores = *total_cores.lock().await;
            }

            let mut offset_index = 0;
            for (_addr, (tx, _, offset, _)) in workers.lock().await.iter_mut() {
                let request = WorkerRequest {
                    proof: serializable_proof.clone(),
                    cutoff_time,
                    core_offset: worker_offsets[offset_index],
                    total_cores: current_total_cores,
                };
                tx.send(request).await.unwrap();
                *offset = worker_offsets[offset_index].try_into().unwrap();
                offset_index += 1;
            }

            progress_bar.println(format!(
                "Distributed mining task to {} workers with a total of {} cores",
                workers.lock().await.len(),
                current_total_cores
            ));

            // Wait for cutoff time to end
            let wait_duration = Duration::from_secs(cutoff_time);
            progress_bar.println(format!("Waiting for {} seconds...", cutoff_time));
            sleep(wait_duration + Duration::from_secs(3)).await; // Add 3 seconds buffer

            // Collect results
            let mut best_result: Option<WorkerResult> = None;
            let mut dishonest_workers = Vec::new();
            while let Ok(result) = result_receiver.try_recv() {
                // verify the result difficulty
                let hash_diff = result.solution.to_hash().difficulty();
                if hash_diff != result.difficulty {
                    progress_bar.println(format!(
                        "Received result with incorrect difficulty from worker {}. Expected: {}, Actual: {}. Worker will be removed.",
                        result.worker_addr, result.difficulty, hash_diff
                    ));
                    dishonest_workers.push(result.worker_addr.clone());
                    continue;
                }
                if best_result.is_none()
                    || result.difficulty > best_result.as_ref().unwrap().difficulty
                {
                    best_result = Some(result);
                }
            }

            self.update_hash_powers(&workers, &global_hash_power, cutoff_time as f64)
                .await;

            // Calculate and display worker statistics
            let worker_stats = self.calculate_worker_stats(&workers).await;
            self.display_worker_stats(&worker_stats, *global_hash_power.lock().await);

            // Remove and ban dishonest workers
            for worker_addr in dishonest_workers {
                self.remove_and_ban_worker(&worker_addr, &workers, &total_cores, &banned_pubkeys)
                    .await;
            }

            // Submit best result
            if let Some(result) = best_result {
                progress_bar.println(format!(
                    "Best result received. Best difficulty: {}. Submitting to network...",
                    result.difficulty
                ));
                let config = get_config(&self.rpc_client).await;
                let solution = result.solution;
                // Submit solution to the network
                let mut compute_budget = 500_000;
                let mut ixs = vec![ore_api::instruction::auth(proof_pubkey(signer.pubkey()))];
                if self.should_reset(config).await && rand::thread_rng().gen_range(0..100).eq(&0) {
                    compute_budget += 100_000;
                    ixs.push(ore_api::instruction::reset(signer.pubkey()));
                }
                ixs.push(ore_api::instruction::mine(
                    signer.pubkey(),
                    signer.pubkey(),
                    self.as_ref().find_bus().await,
                    solution,
                ));
                match self
                    .send_and_confirm(
                        &ixs,
                        ComputeBudget::Fixed(compute_budget),
                        false,
                        if args.hack_rpc { proof.last_hash_at } else { 0 },
                    )
                    .await
                {
                    Ok(_) => {
                        // Save proof to compare with next proof, if it's the same proof, then something wrong with RPC
                        progress_bar.finish_with_message("Solution submitted successfully");
                        previous_proof = proof;
                    }
                    Err(e) => {
                        log_error(&progress_bar, &e.kind().to_string(), true);
                    }
                }
            } else {
                println!("No results received. Retrying in 3 seconds...");
                sleep(Duration::from_secs(3)).await;
            }
        }
    }

    async fn reset_hash_powers(
        &self,
        workers: &Arc<
            Mutex<HashMap<String, (mpsc::Sender<WorkerRequest>, AuthenticatedUser, u32, f64)>>,
        >,
    ) {
        let mut workers_lock = workers.lock().await;
        for (_, (_, _, _, hash_power)) in workers_lock.iter_mut() {
            *hash_power = 0.0;
        }
    }

    async fn update_hash_powers(
        &self,
        workers: &Arc<
            Mutex<HashMap<String, (mpsc::Sender<WorkerRequest>, AuthenticatedUser, u32, f64)>>,
        >,
        global_hash_power: &Arc<Mutex<f64>>,
        elapsed_time: f64,
    ) {
        let mut workers_lock = workers.lock().await;
        let mut total_hash_power = 0.0;

        for (_, (_, _, _, hash_power)) in workers_lock.iter_mut() {
            *hash_power = *hash_power / elapsed_time;
            total_hash_power += *hash_power;
        }

        let mut global_power = global_hash_power.lock().await;
        *global_power = total_hash_power;
    }

    async fn calculate_worker_stats(
        &self,
        workers: &Arc<
            Mutex<HashMap<String, (mpsc::Sender<WorkerRequest>, AuthenticatedUser, u32, f64)>>,
        >,
    ) -> Vec<WorkerStats> {
        let workers_lock = workers.lock().await;
        let mut stats: Vec<WorkerStats> = workers_lock
            .iter()
            .map(|(_, (_, user, _, hash_power))| WorkerStats {
                pubkey: user.pubkey,
                cores_count: user.cores_count,
                worker_name: user.worker_name.clone(),
                hash_power: *hash_power,
            })
            .collect();

        stats.sort_by(|a, b| b.hash_power.partial_cmp(&a.hash_power).unwrap());
        stats
    }

    fn display_worker_stats(&self, stats: &[WorkerStats], global_hash_power: f64) {
        println!("Global Hash Power: {:.2} H/s", global_hash_power);
        println!("Worker Statistics:");
        println!(
            "| {:<20} | {:<10} | {:<15} | {:<15} |",
            "Worker Name", "Cores", "Pubkey", "Hash Power (H/s)"
        );
        println!("{:-<70}", "");
        for stat in stats {
            println!(
                "| {:<20} | {:<10} | {:<15} | {:<15.2} |",
                stat.worker_name,
                stat.cores_count,
                stat.pubkey.to_string()[..15].to_string(),
                stat.hash_power
            );
        }
        println!();
    }

    async fn handle_worker_disconnection(
        addr_str: &str,
        workers: &Arc<
            Mutex<HashMap<String, (mpsc::Sender<WorkerRequest>, AuthenticatedUser, u32, f64)>>,
        >,
        total_cores: &Arc<Mutex<u64>>,
        global_hash_power: &Arc<Mutex<f64>>,
    ) {
        let mut workers_lock = workers.lock().await;
        if let Some((_, authenticated_user, _, hash_power)) = workers_lock.remove(addr_str) {
            let mut total = total_cores.lock().await;
            *total -= authenticated_user.cores_count;
            let mut global_power = global_hash_power.lock().await;
            *global_power -= hash_power;
            println!(
                "Worker {} disconnected. Removed {} cores. Total cores: {}, Global Hash Power: {:.2} H/s",
                authenticated_user.worker_name, authenticated_user.cores_count, *total, *global_power
            );
        }
    }

    async fn remove_and_ban_worker(
        &self,
        worker_addr: &str,
        workers: &Arc<
            Mutex<HashMap<String, (mpsc::Sender<WorkerRequest>, AuthenticatedUser, u32, f64)>>,
        >,
        total_cores: &Arc<Mutex<u64>>,
        banned_pubkeys: &Arc<Mutex<HashSet<Pubkey>>>,
    ) {
        let mut workers_lock = workers.lock().await;
        if let Some((_, authenticated_user, _, _)) = workers_lock.remove(worker_addr) {
            let mut total = total_cores.lock().await;
            *total -= authenticated_user.cores_count;

            // Ban the pubkey
            let mut banned = banned_pubkeys.lock().await;
            banned.insert(authenticated_user.pubkey);

            println!(
                "Dishonest worker {} removed and banned. Pubkey: {}. Subtracted {} cores. Total cores: {}",
                worker_addr, authenticated_user.pubkey, authenticated_user.cores_count, *total
            );
        }
    }

    async fn handle_new_connections(
        self: Arc<Self>,
        listener: TcpListener,
        workers: Arc<
            Mutex<HashMap<String, (mpsc::Sender<WorkerRequest>, AuthenticatedUser, u32, f64)>>,
        >,
        result_sender: mpsc::Sender<WorkerResult>,
        total_cores: Arc<Mutex<u64>>,
        banned_pubkeys: Arc<Mutex<HashSet<Pubkey>>>,
        global_hash_power: Arc<Mutex<f64>>,
    ) {
        loop {
            match listener.accept().await {
                Ok((mut socket, addr)) => {
                    let addr_str = addr.to_string();
                    println!("New connection from: {}", addr_str);

                    // Authenticate worker
                    let authenticated_user =
                        match self.authenticate_worker(&mut socket, &banned_pubkeys).await {
                            Some(user) => user,
                            None => {
                                println!("Authentication failed for worker: {}", addr_str);
                                continue;
                            }
                        };
                    println!(
                        "Worker authenticated: {} with {} cores",
                        addr_str, authenticated_user.cores_count
                    );

                    let (tx, mut rx) = mpsc::channel(10);
                    workers
                        .lock()
                        .await
                        .insert(addr_str.clone(), (tx, authenticated_user.clone(), 0, 0.0));

                    // Increase total cores
                    {
                        let mut total = total_cores.lock().await;
                        *total += authenticated_user.cores_count;
                    }

                    let workers_clone = Arc::clone(&workers);
                    let result_sender_clone = result_sender.clone();
                    let total_cores_clone = Arc::clone(&total_cores);
                    let global_hash_power = Arc::clone(&global_hash_power);

                    // Spawn a task to handle this worker
                    task::spawn(async move {
                        while let Some(request) = rx.recv().await {
                            if let Err(e) =
                                send_message(&mut socket, MessageType::WorkerRequest, &request)
                                    .await
                            {
                                println!("Failed to send request to worker {}: {}", addr_str, e);
                                break;
                            }

                            match receive_message::<WorkerResult>(&mut socket).await {
                                Ok(mut result) => {
                                    result.worker_addr = addr_str.clone();
                                    // Update worker's hash power for this session
                                    let mut workers_lock = workers_clone.lock().await;
                                    if let Some((_, _, _, hash_power)) =
                                        workers_lock.get_mut(&addr_str)
                                    {
                                        *hash_power = result.total_hashes as f64;
                                    }
                                    result_sender_clone.send(result).await.unwrap();
                                }
                                Err(e) => {
                                    println!(
                                        "Failed to read result from worker {}: {}",
                                        addr_str, e
                                    );
                                    break;
                                }
                            }
                        }
                        // Worker disconnected, remove it and decrease total cores
                        Self::handle_worker_disconnection(
                            &addr_str,
                            &workers_clone,
                            &total_cores_clone,
                            &global_hash_power,
                        )
                        .await;
                    });
                }
                Err(e) => {
                    println!("Failed to accept connection: {}", e);
                }
            }
        }
    }

    pub async fn find_hash_par_worker(
        proof: Proof,
        cutoff_time: u64,
        cores: u64,
        min_difficulty: u32,
        core_offset: u64,
        total_cores: u64,
    ) -> (Solution, u32, u64) {
        // Dispatch job to each core
        let progress_bar = Arc::new(spinner::new_progress_bar());
        let global_best_difficulty = Arc::new(RwLock::new(0u32));
        progress_bar.set_message("Mining...");
        let core_ids = core_affinity::get_core_ids().unwrap();
        let handles: Vec<_> = core_ids
            .into_iter()
            .map(|i| {
                let global_best_difficulty = Arc::clone(&global_best_difficulty);
                std::thread::spawn({
                    let proof = proof.clone();
                    let progress_bar = progress_bar.clone();
                    let mut memory = equix::SolverMemory::new();
                    move || {
                        // Return if core should not be used
                        if (i.id as u64).ge(&cores) {
                            return (0, 0, Hash::default(), 0);
                        }

                        // Pin to core
                        let _ = core_affinity::set_for_current(i);

                        let timer = Instant::now();
                        let global_core_id = core_offset + i.id as u64;
                        let mut nonce = u64::MAX
                            .saturating_div(total_cores)
                            .saturating_mul(global_core_id);
                        let first_nonce = nonce;
                        let mut best_nonce = nonce;
                        let mut best_difficulty = 0;
                        let mut best_hash = Hash::default();
                        loop {
                            // Create hash
                            if let Ok(hx) = drillx::hash_with_memory(
                                &mut memory,
                                &proof.challenge,
                                &nonce.to_le_bytes(),
                            ) {
                                let difficulty = hx.difficulty();
                                if difficulty.gt(&best_difficulty) {
                                    best_nonce = nonce;
                                    best_difficulty = difficulty;
                                    best_hash = hx;
                                    // {{ edit_1 }}
                                    if best_difficulty.gt(&*global_best_difficulty.read().unwrap())
                                    {
                                        *global_best_difficulty.write().unwrap() = best_difficulty;
                                    }
                                    // {{ edit_1 }}
                                }
                            }

                            // Exit if time has elapsed
                            if nonce % 100 == 0 {
                                let global_best_difficulty =
                                    *global_best_difficulty.read().unwrap();
                                if timer.elapsed().as_secs().ge(&cutoff_time) {
                                    if i.id == 0 {
                                        progress_bar.set_message(format!(
                                            "Mining... (difficulty {})",
                                            global_best_difficulty,
                                        ));
                                    }
                                    if global_best_difficulty.ge(&min_difficulty) {
                                        // Mine until min difficulty has been met
                                        break;
                                    }
                                } else if i.id == 0 {
                                    progress_bar.set_message(format!(
                                        "Mining... (difficulty {}, time {})",
                                        global_best_difficulty,
                                        format_duration(
                                            cutoff_time.saturating_sub(timer.elapsed().as_secs())
                                                as u32
                                        ),
                                    ));
                                }
                            }

                            // Increment nonce
                            nonce += 1;
                        }

                        // Return the best nonce
                        (best_nonce, best_difficulty, best_hash, nonce - first_nonce)
                    }
                })
            })
            .collect();

        // Join handles and return best nonce
        let mut best_nonce = 0;
        let mut best_difficulty = 0;
        let mut best_hash = Hash::default();
        let mut total_nonces = 0;
        for h in handles {
            if let Ok((nonce, difficulty, hash, count)) = h.join() {
                if difficulty > best_difficulty {
                    best_difficulty = difficulty;
                    best_nonce = nonce;
                    best_hash = hash;
                    total_nonces += count;
                }
            }
        }

        // Update log
        progress_bar.finish_with_message(format!(
            "Best hash: {} (difficulty: {})",
            bs58::encode(best_hash.h).into_string(),
            best_difficulty
        ));

        (
            Solution::new(best_hash.d, best_nonce.to_le_bytes()),
            best_difficulty,
            total_nonces,
        )
    }

    async fn work(self: Arc<Self>, args: MineDistributedArgs) {
        let worker_name = args
            .worker_name
            .unwrap_or_else(|| format!("Worker-{}", rand::thread_rng().gen_range(0..1000)));
        let mut stream =
            TcpStream::connect(args.coordinator.expect("No coordinator URL specified!"))
                .await
                .unwrap();
        println!("Connected to coordinator");

        // Authenticate with the server
        if !self
            .authenticate_with_server(&mut stream, args.cores, &worker_name)
            .await
        {
            println!("Authentication with server failed");
            return;
        }
        println!("Authentication successful");

        // Check num cores
        self.check_num_cores(args.cores);

        loop {
            println!("Waiting for next request from coordinator...");
            // Receive proof from coordinator
            let worker_request: WorkerRequest = match receive_message(&mut stream).await {
                Ok(request) => request,
                Err(e) => {
                    println!("Error receiving worker request: {}", e);
                    break;
                }
            };
            let proof = worker_request.proof.to_proof();
            let config = get_config(&self.rpc_client).await;

            println!(
                "Received new mining request. Cutoff time: {} seconds, core offset: {}, Total cores: {}",
                worker_request.cutoff_time, worker_request.core_offset, worker_request.total_cores
            );

            // Mine using existing parallel mining code
            let (solution, best_difficulty, total_nonces) = Self::find_hash_par_worker(
                proof,
                worker_request.cutoff_time,
                args.cores,
                config.min_difficulty as u32,
                worker_request.core_offset,
                worker_request.total_cores,
            )
            .await;

            println!("Mining completed. Best difficulty: {}", best_difficulty);

            // Send result back to coordinator
            let result = WorkerResult {
                difficulty: best_difficulty,
                solution,
                total_hashes: total_nonces,
                worker_addr: "".to_string(),
            };
            if let Err(e) = send_message(&mut stream, MessageType::WorkerResult, &result).await {
                println!("Error sending mining result to coordinator: {}", e);
                break;
            }
            println!("Sent mining result to coordinator");
        }
    }

    async fn authenticate_worker(
        &self,
        socket: &mut TcpStream,
        banned_pubkeys: &Arc<Mutex<HashSet<Pubkey>>>,
    ) -> Option<AuthenticatedUser> {
        let auth_request: Result<AuthRequest, std::io::Error> = receive_message(socket).await;

        if let Err(e) = auth_request {
            println!(
                "Failed to receive authentication request from worker: {}",
                e
            );
            return None;
        }

        let auth_request = auth_request.unwrap();

        println!("Received authentication request from worker");

        let pubkey = Pubkey::from_str(&auth_request.pubkey).ok()?;

        // Check if the pubkey is banned
        if banned_pubkeys.lock().await.contains(&pubkey) {
            let response = AuthResponse {
                success: false,
                message: "Authentication failed: Pubkey is banned".to_string(),
            };
            send_message(socket, MessageType::AuthResponse, &response)
                .await
                .unwrap();
            return None;
        }

        let is_authentic = !auth_request.pubkey.is_empty() && auth_request.cores_count > 0;
        let response = AuthResponse {
            success: is_authentic,
            message: if is_authentic {
                "Authentication successful".to_string()
            } else {
                "Authentication failed".to_string()
            },
        };

        send_message(socket, MessageType::AuthResponse, &response)
            .await
            .unwrap();

        if is_authentic {
            Some(AuthenticatedUser {
                pubkey,
                cores_count: auth_request.cores_count,
                worker_name: auth_request.worker_name,
            })
        } else {
            None
        }
    }

    async fn authenticate_with_server(
        &self,
        stream: &mut TcpStream,
        cores_count: u64,
        worker_name: &str,
    ) -> bool {
        let auth_request = AuthRequest {
            pubkey: self.signer().pubkey().to_string(),
            cores_count,
            worker_name: worker_name.to_string(),
        };

        send_message(stream, MessageType::AuthRequest, &auth_request)
            .await
            .unwrap();

        println!("Sent authentication request to server");

        let auth_response: AuthResponse = receive_message(stream).await.unwrap();

        auth_response.success
    }
}
