use std::str::FromStr;

use colored::*;
use solana_program::pubkey::Pubkey;

use crate::{args::ClaimArgs, Miner};

impl Miner {
    pub async fn claim(&self, _args: ClaimArgs) {
        let _pubkey = match self.address {
            Some(ref address) => Pubkey::from_str(address).expect("Failed to parse address"),
            None => Err("No address provided").expect("Failed to parse address"),
        };

        // TODO: fetch pending balance from network

        println!(
            "{}",
            format!(
                "{}",
                "Rewards will be automatically sent to your miner address once every hour if your pending balance is greater than 0.02 ORE."
            )
            .green().bold()
        );
    }
}
