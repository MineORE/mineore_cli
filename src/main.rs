mod args;
mod balance;
mod benchmark;
mod claim;
mod config;
mod mine;
mod mine_pool;
mod rewards;
mod utils;

use std::sync::Arc;

use args::*;
use clap::{command, Parser, Subcommand};
use solana_rpc_client::nonblocking::rpc_client::RpcClient;
use solana_sdk::commitment_config::CommitmentConfig;

struct Miner {
    pub rpc_client: Arc<RpcClient>,
    pub address: Option<String>,
}

#[derive(Subcommand, Debug)]
enum Commands {
    #[command(about = "Fetch an account balance")]
    Balance(BalanceArgs),

    #[command(about = "Benchmark your hashpower")]
    Benchmark(BenchmarkArgs),

    #[command(about = "Claim your mining rewards")]
    Claim(ClaimArgs),

    #[command(about = "Fetch the program config")]
    Config(ConfigArgs),

    #[command(about = "Fetch the current reward rate for each difficulty level")]
    Rewards(RewardsArgs),

    #[command(about = "Mining distributed")]
    MineDistributed(MineDistributedArgs),

    #[command(about = "Pool status of the miner")]
    Status(StatusArgs),
}

#[derive(Parser, Debug)]
#[command(about, version)]
struct Args {
    #[arg(
        long,
        value_name = "NETWORK_URL",
        help = "Network address of your RPC provider",
        global = true
    )]
    rpc: Option<String>,
    #[arg(
        long,
        short,
        value_name = "ADDRESS",
        help = "The address of the miner to use for commands."
    )]
    address: Option<String>,

    #[command(subcommand)]
    command: Commands,
}

#[tokio::main]
async fn main() {
    let args = Args::parse();

    // Load the config file from custom path, the default path, or use default config values
    let cli_config = solana_cli_config::Config::default();
    let cluster = args.rpc.unwrap_or(cli_config.json_rpc_url);
    let rpc_client = RpcClient::new_with_commitment(cluster, CommitmentConfig::confirmed());

    let miner = Arc::new(Miner::new(Arc::new(rpc_client), args.address));

    // Execute user command.
    match args.command {
        Commands::Balance(args) => {
            miner.balance(args).await;
        }
        Commands::Benchmark(args) => {
            miner.benchmark(args).await;
        }
        Commands::Claim(args) => {
            miner.claim(args).await;
        }
        Commands::Config(_) => {
            miner.config().await;
        }
        Commands::Rewards(_) => {
            miner.rewards().await;
        }
        Commands::MineDistributed(args) => {
            miner.work(args).await;
        }
        Commands::Status(args) => {
            miner.get_status(args).await;
        }
    }
}

impl Miner {
    pub fn new(rpc_client: Arc<RpcClient>, address: Option<String>) -> Self {
        Self {
            rpc_client,
            address,
        }
    }
}
