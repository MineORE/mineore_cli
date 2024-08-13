use crate::args::MineDistributedArgs;
use crate::mine::format_duration;
use crate::utils::get_config;
use crate::Miner;
use drillx::{equix, Hash, Solution};
use ore_api::state::Proof;
use rand::Rng;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use solana_program::pubkey::Pubkey;
use solana_rpc_client::spinner;
use solana_sdk::signer::Signer;
use std::str::FromStr;
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::time::Instant;

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
    pub async fn work(self: Arc<Self>, args: MineDistributedArgs) {
        let worker_name = args
            .worker_name
            .unwrap_or_else(|| format!("Worker-{}", rand::thread_rng().gen_range(0..1000000)));
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

            println!(
                "Mining completed. Best difficulty: {}, total nonces found: {}",
                best_difficulty, total_nonces
            );

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
                        let first_nonce = u64::MAX
                            .saturating_div(total_cores)
                            .saturating_mul(global_core_id);
                        let mut nonce = first_nonce;
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
                                    if best_difficulty.gt(&*global_best_difficulty.read().unwrap())
                                    {
                                        *global_best_difficulty.write().unwrap() = best_difficulty;
                                    }
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
                }
                total_nonces += count;
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
}
