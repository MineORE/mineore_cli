use crate::mine::find_bus;
use crate::send_and_confirm::ComputeBudget;
use crate::utils::{get_config, proof_pubkey};
use crate::Miner;
use crate::{args::MineDistributedArgs, utils::get_proof_with_authority};
use drillx::{
    equix::{self},
    Hash, Solution,
};
use ore_api::state::Proof;
use rand::Rng;
use serde::{Deserialize, Serialize};
use solana_program::pubkey::Pubkey;
use solana_sdk::signer::Signer;
use std::collections::HashMap;
use std::str::FromStr;
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::sync::Mutex;
use tokio::task;
use tokio::time::sleep;

#[derive(Serialize, Deserialize)]
struct WorkerResult {
    pub difficulty: u32,
    pub solution: Solution,
}

#[derive(Serialize, Deserialize, Clone)]
struct WorkerRequest {
    pub proof: SerializableProof,
    pub cutoff_time: u64,
}

#[derive(Serialize, Deserialize)]
struct AuthRequest {
    pub pubkey: String,
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

impl Miner {
    pub async fn mine_distributed(self: Arc<Self>, args: MineDistributedArgs) {
        match args.role.as_str() {
            "coordinator" => self.coordinate(args).await,
            "worker" => self.work(args).await,
            _ => println!("Invalid role. Choose 'coordinator' or 'worker'."),
        }
    }

    async fn coordinate(self: Arc<Self>, args: MineDistributedArgs) {
        let listener = TcpListener::bind("0.0.0.0:8080").await.unwrap();
        let workers: Arc<Mutex<HashMap<String, mpsc::Sender<WorkerRequest>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let signer = self.signer();

        // Channel for collecting results
        let (result_sender, mut result_receiver) = mpsc::channel(100);

        // Spawn a task to handle new connections
        let worker_handler = Arc::clone(&self);
        let workers_clone = Arc::clone(&workers);
        let result_sender_clone = result_sender.clone();
        task::spawn(async move {
            worker_handler
                .handle_new_connections(listener, workers_clone, result_sender_clone)
                .await;
        });

        loop {
            // Main mining loop
            let proof = get_proof_with_authority(&self.rpc_client, signer.pubkey()).await;
            let cutoff_time = self.get_cutoff(proof, args.buffer_time).await;
            let serializable_proof = SerializableProof::from(&proof);

            let request = WorkerRequest {
                proof: serializable_proof,
                cutoff_time,
            };

            // Send the request to all current workers
            for tx in workers.lock().await.values() {
                tx.send(request.clone()).await.unwrap();
            }

            // Wait for cutoff time to end
            let wait_duration = Duration::from_secs(cutoff_time);
            sleep(wait_duration).await;

            // Collect results
            let mut best_result: Option<WorkerResult> = None;
            while let Ok(result) = result_receiver.try_recv() {
                if best_result.is_none()
                    || result.difficulty > best_result.as_ref().unwrap().difficulty
                {
                    best_result = Some(result);
                }
            }

            // Submit best result
            if let Some(result) = best_result {
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
                    find_bus(),
                    solution,
                ));
                self.send_and_confirm(&ixs, ComputeBudget::Fixed(compute_budget), false)
                    .await
                    .ok();
            }
        }
    }

    async fn handle_new_connections(
        self: Arc<Self>,
        listener: TcpListener,
        workers: Arc<Mutex<HashMap<String, mpsc::Sender<WorkerRequest>>>>,
        result_sender: mpsc::Sender<WorkerResult>,
    ) {
        loop {
            match listener.accept().await {
                Ok((mut socket, addr)) => {
                    let addr_str = addr.to_string();
                    println!("New connection from: {}", addr_str);

                    // Authenticate worker
                    if !self.authenticate_worker(&mut socket).await {
                        println!("Authentication failed for worker: {}", addr_str);
                        continue;
                    }
                    println!("Worker authenticated: {}", addr_str);

                    let (tx, mut rx) = mpsc::channel(10);
                    workers.lock().await.insert(addr_str.clone(), tx);

                    let workers_clone = Arc::clone(&workers);
                    let result_sender_clone = result_sender.clone();

                    // Spawn a task to handle this worker
                    task::spawn(async move {
                        while let Some(request) = rx.recv().await {
                            let request_bytes = bincode::serialize(&request).unwrap();
                            if let Err(e) = socket.write_all(&request_bytes).await {
                                println!("Failed to send request to worker {}: {}", addr_str, e);
                                break;
                            }

                            // Wait for and forward the result
                            let mut buf = Vec::new();
                            if let Err(e) = socket.read_to_end(&mut buf).await {
                                println!("Failed to read result from worker {}: {}", addr_str, e);
                                break;
                            }
                            let result: WorkerResult = bincode::deserialize(&buf).unwrap();
                            result_sender_clone.send(result).await.unwrap();
                        }
                        workers_clone.lock().await.remove(&addr_str);
                    });
                }
                Err(e) => {
                    println!("Failed to accept connection: {}", e);
                }
            }
        }
    }

    async fn work(self: Arc<Self>, args: MineDistributedArgs) {
        let mut stream = TcpStream::connect(args.coordinator.unwrap()).await.unwrap();
        println!("Connected to coordinator");

        // Authenticate with the server
        if !self.authenticate_with_server(&mut stream).await {
            println!("Authentication with server failed");
            return;
        }
        println!("Authentication successful");

        loop {
            println!("Waiting for next request from coordinator...");
            // Receive proof from coordinator
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await.unwrap();
            let worker_request: WorkerRequest = bincode::deserialize(&buf).unwrap();
            let proof = worker_request.proof.to_proof();
            let config = get_config(&self.rpc_client).await;

            println!(
                "Received new mining request. Cutoff time: {} seconds",
                worker_request.cutoff_time
            );

            // Mine using existing parallel mining code
            let (solution, best_difficulty) = Self::find_hash_par(
                proof,
                worker_request.cutoff_time,
                args.threads,
                config.min_difficulty as u32,
            )
            .await;

            println!("Mining completed. Best difficulty: {}", best_difficulty);

            // Send result back to coordinator
            let result = WorkerResult {
                difficulty: best_difficulty,
                solution,
            };
            let result_bytes = bincode::serialize(&result).unwrap();
            stream.write_all(&result_bytes).await.unwrap();
            println!("Sent mining result to coordinator");
        }
    }

    async fn authenticate_worker(&self, socket: &mut TcpStream) -> bool {
        let mut buf = Vec::new();
        socket.read_to_end(&mut buf).await.unwrap();
        let auth_request: AuthRequest = bincode::deserialize(&buf).unwrap();

        // Implement your authentication logic here
        // For this example, we'll just check if the pubkey is not empty
        let is_authentic = !auth_request.pubkey.is_empty();

        let response = AuthResponse {
            success: is_authentic,
            message: if is_authentic {
                "Authentication successful".to_string()
            } else {
                "Authentication failed".to_string()
            },
        };

        let response_bytes = bincode::serialize(&response).unwrap();
        socket.write_all(&response_bytes).await.unwrap();

        is_authentic
    }

    async fn authenticate_with_server(&self, stream: &mut TcpStream) -> bool {
        let auth_request = AuthRequest {
            pubkey: self.signer().pubkey().to_string(),
        };
        let auth_bytes = bincode::serialize(&auth_request).unwrap();
        stream.write_all(&auth_bytes).await.unwrap();

        let mut buf = Vec::new();
        stream.read_to_end(&mut buf).await.unwrap();
        let auth_response: AuthResponse = bincode::deserialize(&buf).unwrap();

        auth_response.success
    }
}
