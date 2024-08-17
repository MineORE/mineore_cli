use crate::args::MineDistributedArgs;
use crate::mine::format_duration_ms;
use crate::utils::{self, get_config, log_error, log_info};
use crate::Miner;
use drillx::{equix, Hash, Solution};
use rand::Rng;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use solana_rpc_client::spinner;
use std::fmt::Write;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::time::{interval, sleep, Instant};

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(25);

#[derive(Serialize, Deserialize)]
struct WorkerResult {
    pub difficulty: u32,
    pub solution: Solution,
    pub total_hashes: u64,
    pub worker_addr: String,
    pub round_id: i32,
}

#[derive(Serialize, Deserialize, Clone, Debug)]
struct WorkerRequest {
    pub challenge: [u8; 32],
    pub cutoff_time: u64,
    pub core_offset: u64,
    pub total_cores: u64,
    pub round_id: i32,
}

#[derive(Serialize, Deserialize)]
struct AuthRequest {
    pub pubkey: String,
    pub cores_count: u64,
    pub worker_name: String,
    pub invitation_code: String,
}

#[derive(Serialize, Deserialize)]
struct AuthResponse {
    pub success: bool,
    pub message: String,
}

#[derive(Serialize, Deserialize, Debug)]
struct StatusMessage {
    pending_reward: u64,
    estimated_reward_per_hour: u64,
    average_hash_rate: f64,
}

#[derive(Serialize, Deserialize, Debug)]
struct MessageHeader {
    payload_size: u32,
}

#[derive(Serialize, Deserialize)]
enum ServerMessage {
    WorkerRequest(WorkerRequest),
    StopMining,
    Status(StatusMessage),
    AuthResponse(AuthResponse),
    Heartbeat,
    LateSubmissionWarning(Duration),
}

#[derive(Serialize, Deserialize)]
enum WorkerMessage {
    WorkerResult(WorkerResult),
    AuthRequest(AuthRequest),
    Heartbeat,
}

/**
 * Packet visualization:
 * | Header size (4 bytes) | Header | Payload |
 */
async fn send_message<T: Serialize>(
    write_half: &Arc<Mutex<OwnedWriteHalf>>,
    payload: &T,
) -> std::io::Result<()> {
    let mut write_guard = write_half.lock().await;
    let payload_bytes = bincode::serialize(payload).unwrap();
    let header = MessageHeader {
        payload_size: payload_bytes.len() as u32,
    };
    let header_bytes = bincode::serialize(&header).unwrap();

    write_guard
        .write_all(&(header_bytes.len() as u32).to_be_bytes())
        .await?;
    write_guard.write_all(&header_bytes).await?;
    write_guard.write_all(&payload_bytes).await?;
    write_guard.flush().await?;

    Ok(())
}

async fn receive_message<T: DeserializeOwned>(
    read_half: &Arc<Mutex<OwnedReadHalf>>,
) -> std::io::Result<T> {
    let mut read_guard = read_half.lock().await;
    let mut header_size_bytes = [0u8; 4];
    read_guard.read_exact(&mut header_size_bytes).await?;
    let header_size = u32::from_be_bytes(header_size_bytes) as usize;

    let mut header_bytes = vec![0u8; header_size];
    read_guard.read_exact(&mut header_bytes).await?;
    let header: MessageHeader = bincode::deserialize(&header_bytes).unwrap();

    let mut payload_bytes = vec![0u8; header.payload_size as usize];
    read_guard.read_exact(&mut payload_bytes).await?;

    let payload: T = bincode::deserialize(&payload_bytes).unwrap();

    Ok(payload)
}

fn _debug_hex_print(label: &str, bytes: &[u8]) {
    const BYTES_PER_LINE: usize = 16;
    println!("{}:", label);
    println!("Length: {} bytes", bytes.len());
    println!("Hex dump:");

    for (i, chunk) in bytes.chunks(BYTES_PER_LINE).enumerate() {
        let mut hex_line = String::new();
        let mut ascii_line = String::new();

        for &byte in chunk {
            write!(&mut hex_line, "{:02X} ", byte).unwrap();
            ascii_line.push(if byte.is_ascii_graphic() {
                byte as char
            } else {
                '.'
            });
        }

        // Pad the hex line if it's shorter than BYTES_PER_LINE
        if chunk.len() < BYTES_PER_LINE {
            for _ in 0..(BYTES_PER_LINE - chunk.len()) {
                hex_line.push_str("   ");
            }
        }

        println!(
            "{:04X}: {:48} |{}|",
            i * BYTES_PER_LINE,
            hex_line,
            ascii_line
        );
    }
    println!();
}

impl Miner {
    pub async fn work(self: Arc<Self>, args: MineDistributedArgs) {
        let worker_name = args
            .worker_name
            .clone()
            .unwrap_or_else(|| format!("Worker-{}", rand::thread_rng().gen_range(0..1000000)));
        let invitation_code = args.invitation_code.clone().unwrap_or("123456".to_string());

        // Cutoff time offset (use when network is slow)
        let cutoff_time_offset: Arc<Mutex<u64>> = Arc::new(Mutex::new(0));

        loop {
            match self
                .clone()
                .connect_and_work(&args, &worker_name, &invitation_code, &cutoff_time_offset)
                .await
            {
                Ok(_) => {
                    println!("Connection closed. Attempting to reconnect in 15 seconds...");
                }
                Err(e) => {
                    println!(
                        "Error occurred: {}. Attempting to reconnect in 15 seconds...",
                        e
                    );
                }
            }
            sleep(Duration::from_secs(15)).await;
        }
    }

    async fn connect_and_work(
        self: Arc<Self>,
        args: &MineDistributedArgs,
        worker_name: &str,
        invitation_code: &str,
        cutoff_time_offset: &Arc<Mutex<u64>>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let stream =
            TcpStream::connect(args.pool.as_ref().ok_or("No Pool URL specified!")?).await?;
        println!("Connected to coordinator");

        let (read_half, write_half) = stream.into_split();
        let read_half = Arc::new(Mutex::new(read_half));
        let write_half = Arc::new(Mutex::new(write_half));

        // Authenticate with the server
        if !self
            .authenticate_with_server(
                Arc::clone(&read_half),
                Arc::clone(&write_half),
                args.cores,
                worker_name,
                invitation_code,
            )
            .await
        {
            return Err("Authentication with server failed".into());
        }
        println!("Authentication successful");

        // Check num cores
        self.check_num_cores(args.cores);

        // Create a channel for server messages
        let (tx, mut rx) = mpsc::channel::<ServerMessage>(100);

        // Spawn a task to handle incoming messages
        let read_half_clone = Arc::clone(&read_half);
        let message_handler = tokio::spawn(async move {
            Self::handle_server_messages(read_half_clone, tx).await;
        });

        let mut current_mining_task: Option<tokio::task::JoinHandle<()>> = None;
        let mut heartbeat_interval = interval(HEARTBEAT_INTERVAL);

        let cores = args.cores;

        loop {
            tokio::select! {
                _ = heartbeat_interval.tick() => {
                    if let Err(e) = send_message(&write_half, &WorkerMessage::Heartbeat).await {
                        println!("Error sending heartbeat to pool: {}", e);
                        break;
                    }
                }
                Some(message) = rx.recv() => {
                    match message {
                        ServerMessage::WorkerRequest(worker_request) => {
                            println!(
                                "Received new mining request. Cutoff time: {} seconds, core offset: {}, Total cores: {}",
                                worker_request.cutoff_time, worker_request.core_offset, worker_request.total_cores
                            );

                            // If there's an ongoing mining task, abort it
                            if let Some(task) = current_mining_task.take() {
                                task.abort();
                                println!("Aborted previous mining task");
                            }

                            // Start a new mining task
                            let miner = Arc::clone(&self);
                            let write_half = Arc::clone(&write_half);
                            let cutoff_time_offset_clone = Arc::clone(&cutoff_time_offset);
                            current_mining_task = Some(tokio::spawn(async move {
                                let result = Self::perform_mining(miner, worker_request, cores, cutoff_time_offset_clone).await;
                                if let Err(e) = send_message(&write_half, &WorkerMessage::WorkerResult(result)).await {
                                    println!("Error sending mining result to pool: {}", e);
                                } else {
                                    println!("Sent mining result to pool");
                                }
                            }));
                        },
                        ServerMessage::StopMining => {
                            println!("Received stop mining command. Stopping mining process.");
                            if let Some(task) = current_mining_task.take() {
                                task.abort();
                            }
                        },
                        ServerMessage::Status(status) => {
                            log_info("Received status update:");
                            log_info(format!("Pending reward: {} ORE", utils::amount_u64_to_string(status.pending_reward)).as_str());
                            log_info(format!("Estimated reward per hour: {} ORE", utils::amount_u64_to_string(status.estimated_reward_per_hour)).as_str());
                            log_info(format!("Average weight in the network (hourly): {} %", status.average_hash_rate).as_str());
                        },
                        ServerMessage::Heartbeat => {
                            // Respond to server's heartbeat
                            if let Err(e) = send_message(&write_half, &WorkerMessage::Heartbeat).await {
                                println!("Error sending heartbeat response to server: {}", e);
                                break;
                            }
                        },
                        ServerMessage::LateSubmissionWarning(delay) => {
                            log_error("Received late submission warning", false);
                            log_info("Increase the cutoff time to avoid late submissions");

                            // Update the cutoff time offset
                            let mut cutoff_time_offset = cutoff_time_offset.lock().await;
                            *cutoff_time_offset = delay.as_millis() as u64;

                            log_info(format!("Cutoff time offset increased by {} ms", delay.as_millis()).as_str());
                        },
                        _ => {},
                    }
                },
                else => break,
            }
        }

        // Ensure any running mining task is aborted when exiting the loop
        if let Some(task) = current_mining_task.take() {
            task.abort();
            println!("Aborted final mining task before exiting");
        }

        // Cancel the message handler task
        message_handler.abort();

        Ok(())
    }

    async fn handle_server_messages(
        read_half: Arc<Mutex<OwnedReadHalf>>,
        tx: mpsc::Sender<ServerMessage>,
    ) {
        loop {
            match receive_message::<ServerMessage>(&read_half).await {
                Ok(message_type) => match message_type {
                    ServerMessage::WorkerRequest(request) => {
                        tx.send(ServerMessage::WorkerRequest(request))
                            .await
                            .unwrap();
                    }
                    ServerMessage::StopMining => {
                        tx.send(ServerMessage::StopMining).await.unwrap();
                    }
                    ServerMessage::Status(status) => {
                        tx.send(ServerMessage::Status(status)).await.unwrap();
                    }
                    _ => {}
                },
                Err(e) => {
                    println!("Error receiving message from server: {}", e);
                    break;
                }
            }
        }
    }

    async fn perform_mining(
        self: Arc<Self>,
        worker_request: WorkerRequest,
        cores: u64,
        cutoff_time_offset: Arc<Mutex<u64>>,
    ) -> WorkerResult {
        let config = get_config(&self.rpc_client).await;

        let cutoff_time_offset = *cutoff_time_offset.lock().await;
        let cutoff_time = if worker_request.cutoff_time > 0 {
            Duration::from_secs(worker_request.cutoff_time)
                - Duration::from_millis(cutoff_time_offset)
        } else {
            Duration::ZERO
        };

        let (solution, best_difficulty, total_nonces) = Self::find_hash_par_worker(
            worker_request.challenge,
            cutoff_time.as_millis(),
            cores,
            config.min_difficulty as u32,
            worker_request.core_offset,
            worker_request.total_cores,
        )
        .await;

        println!(
            "Mining completed. Best difficulty: {}, total nonces found: {}",
            best_difficulty, total_nonces
        );

        WorkerResult {
            difficulty: best_difficulty,
            solution,
            total_hashes: total_nonces,
            worker_addr: "".to_string(),
            round_id: worker_request.round_id,
        }
    }

    async fn authenticate_with_server(
        &self,
        read_half: Arc<Mutex<OwnedReadHalf>>,
        write_half: Arc<Mutex<OwnedWriteHalf>>,
        cores_count: u64,
        worker_name: &str,
        invitation_code: &str,
    ) -> bool {
        let self_address = match self.address {
            Some(ref address) => address,
            None => {
                println!("No miner address specified");
                return false;
            }
        };
        let auth_request = AuthRequest {
            pubkey: self_address.to_string(),
            cores_count,
            worker_name: worker_name.to_string(),
            invitation_code: invitation_code.to_string(),
        };

        send_message(&write_half, &WorkerMessage::AuthRequest(auth_request))
            .await
            .unwrap();

        println!("Sent authentication request to server");

        let auth_response: ServerMessage =
            receive_message::<ServerMessage>(&read_half).await.unwrap();

        match auth_response {
            ServerMessage::AuthResponse(response) => {
                if !response.success {
                    log_error(&response.message, false);
                } else {
                    log_info(&response.message);
                }
                response.success
            }
            _ => false,
        }
    }

    pub async fn find_hash_par_worker(
        challenge: [u8; 32],
        cutoff_time: u128,
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
                                &challenge,
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
                                if timer.elapsed().as_millis().ge(&cutoff_time) {
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
                                        "Mining... (difficulty {}, time {} ms)",
                                        global_best_difficulty,
                                        format_duration_ms(
                                            cutoff_time.saturating_sub(timer.elapsed().as_millis())
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
