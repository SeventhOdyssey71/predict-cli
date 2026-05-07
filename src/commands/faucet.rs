//! `predict-cli faucet` — instructions for getting DUSDC + SUI on testnet.

use anyhow::Result;
use owo_colors::OwoColorize;

use crate::sui_cli;

pub async fn run() -> Result<()> {
    println!("{}", "Getting test funds for DeepBook Predict".bold());
    println!();

    println!("{}", "1. SUI for gas".bold());
    println!("   curl -s -X POST https://faucet.testnet.sui.io/gas \\");
    println!("     -H 'content-type: application/json' \\");
    println!("     -d '{{\"FixedAmountRequest\":{{\"recipient\":\"YOUR_ADDR\"}}}}'");
    println!();

    println!("{}", "2. DUSDC (the protocol's quote asset)".bold());
    println!("   DUSDC has no public mint. Only the deployer's TreasuryCap can issue it.");
    println!("   Ask the team in Sui Discord → #deepbook channel.");
    println!();

    if let Ok(addr) = sui_cli::active_address() {
        println!("   Your active address (paste in Discord):");
        println!("     {}", addr.cyan());
        println!();
        println!("   Suggested message:");
        println!(
            "     {}",
            "Trying out DeepBook Predict on testnet — can someone mint me some DUSDC?".italic()
        );
        println!("     {}", format!("Address: {}", addr).italic());
        println!("     {}", "Amount: 10,000 DUSDC is plenty.".italic());
    } else {
        println!(
            "   (Could not read your active sui address — set one up first with `sui client`.)"
        );
    }

    Ok(())
}
