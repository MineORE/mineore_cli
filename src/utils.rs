use cached::proc_macro::cached;
use colored::Colorize;
use ore_api::{
    consts::{CONFIG_ADDRESS, MINT_ADDRESS, PROOF, TOKEN_DECIMALS, TREASURY_ADDRESS},
    state::{Config, Proof, Treasury},
};
use ore_utils::AccountDeserialize;
use solana_client::nonblocking::rpc_client::RpcClient;
use solana_program::pubkey::Pubkey;
use spl_associated_token_account::get_associated_token_address;

pub async fn _get_treasury(client: &RpcClient) -> Treasury {
    let data = client
        .get_account_data(&TREASURY_ADDRESS)
        .await
        .expect("Failed to get treasury account");
    *Treasury::try_from_bytes(&data).expect("Failed to parse treasury account")
}

pub async fn get_config(client: &RpcClient) -> Config {
    let data = client
        .get_account_data(&CONFIG_ADDRESS)
        .await
        .expect("Failed to get config account");
    *Config::try_from_bytes(&data).expect("Failed to parse config account")
}

pub async fn get_proof_with_authority(client: &RpcClient, authority: Pubkey) -> Proof {
    let proof_address = proof_pubkey(authority);
    get_proof(client, proof_address).await
}

pub async fn get_proof(client: &RpcClient, address: Pubkey) -> Proof {
    let data = client
        .get_account_data(&address)
        .await
        .expect("Failed to get proof account");
    *Proof::try_from_bytes(&data).expect("Failed to parse proof account")
}

pub fn amount_u64_to_string(amount: u64) -> String {
    amount_u64_to_f64(amount).to_string()
}

pub fn amount_u64_to_f64(amount: u64) -> f64 {
    (amount as f64) / 10f64.powf(TOKEN_DECIMALS as f64)
}

#[cached]
pub fn proof_pubkey(authority: Pubkey) -> Pubkey {
    Pubkey::find_program_address(&[PROOF, authority.as_ref()], &ore_api::ID).0
}

#[cached]
pub fn treasury_tokens_pubkey() -> Pubkey {
    get_associated_token_address(&TREASURY_ADDRESS, &MINT_ADDRESS)
}

pub fn log_error(error_message: &str, is_fatal: bool) {
    let formatted_message = if is_fatal {
        format!("FATAL ERROR: {}", error_message).red().bold()
    } else {
        format!("ERROR: {}", error_message).red().bold()
    };

    eprintln!("{}", formatted_message);
}

pub fn log_info(message: &str) {
    let formatted_message = format!("INFO: {}", message).cyan().bold();
    println!("{}", formatted_message);
}

pub fn format_duration_ms(milliseconds: u128) -> String {
    let total_seconds = milliseconds / 1000;
    let minutes = total_seconds / 60;
    let remaining_seconds = total_seconds % 60;
    format!("{:02}:{:02}", minutes, remaining_seconds)
}
