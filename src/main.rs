//! DeepBook Predict CLI for Sui testnet.

use anyhow::{bail, Result};
use clap::{Parser, Subcommand};

mod agent;
mod commands;
mod config;
mod format;
mod pricing;
mod rpc;
mod server;
mod sui_cli;

/// DeepBook Predict CLI (testnet).
#[derive(Parser)]
#[command(name = "predict-cli", version, about, long_about = None)]
struct Cli {
    /// Output raw JSON instead of a human-friendly view.
    #[arg(long, global = true)]
    json: bool,

    #[command(subcommand)]
    command: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    /// Show the testnet config (package, registry, predict object, server).
    Config,

    /// List active markets (oracles) from the predict-server.
    List {
        /// Include settled oracles too.
        #[arg(long)]
        all: bool,
    },

    /// Inspect one oracle's live state (spot, forward, SVI, settlement).
    Oracle {
        /// Oracle object ID (0x…).
        oracle_id: String,
    },

    /// Show the Predict shared object's vault aggregates.
    Vault,

    /// Get a price preview for a binary or vertical-range position.
    /// Provide either `--strike X --up|--down` or `--lower L --upper U`.
    Quote {
        oracle_id: String,
        #[arg(long)]
        strike: Option<f64>,
        #[arg(long, conflicts_with = "down")]
        up: bool,
        #[arg(long, conflicts_with = "up")]
        down: bool,
        #[arg(long)]
        lower: Option<f64>,
        #[arg(long)]
        upper: Option<f64>,
        /// Stake in DUSDC for payout calculation.
        #[arg(long, default_value_t = 10.0)]
        stake: f64,
    },

    /// Show or create your PredictManager.
    Manager {
        /// Create a new shared PredictManager.
        #[arg(long)]
        create: bool,
    },

    /// Deposit DUSDC into your PredictManager.
    Deposit {
        /// Amount of DUSDC.
        #[arg(long)]
        amount: f64,
    },

    /// Mint a binary position (UP or DOWN at a strike).
    /// `--deposit` is funded into your manager from your wallet; the contract
    /// then pulls from the manager's *aggregate* balance based on live mint
    /// pricing. By default, mint aborts if the manager already holds DUSDC.
    #[command(visible_alias = "buy-binary")]
    Mint {
        oracle_id: String,
        #[arg(long)]
        strike: f64,
        #[arg(long, conflicts_with = "down")]
        up: bool,
        #[arg(long, conflicts_with = "up")]
        down: bool,
        /// Quantity in float-scaled units (1.0 = 1 USDC payout per unit).
        #[arg(long)]
        qty: f64,
        /// DUSDC to deposit into the manager before the mint.
        #[arg(long)]
        deposit: f64,
        /// Optional hard ceiling for manager funds available to this mint.
        #[arg(long)]
        max_cost: Option<f64>,
        /// Allow mint to spend DUSDC that was already in the manager.
        #[arg(long)]
        allow_manager_balance: bool,
    },

    /// Mint a vertical-range position (between K_lo and K_hi).
    #[command(visible_alias = "buy-range")]
    MintRange {
        oracle_id: String,
        #[arg(long)]
        lower: f64,
        #[arg(long)]
        upper: f64,
        #[arg(long)]
        qty: f64,
        /// DUSDC to deposit into the manager before the mint.
        #[arg(long)]
        deposit: f64,
        /// Optional hard ceiling for manager funds available to this mint.
        #[arg(long)]
        max_cost: Option<f64>,
        /// Allow mint to spend DUSDC that was already in the manager.
        #[arg(long)]
        allow_manager_balance: bool,
    },

    /// Redeem a binary position. Use --permissionless after settlement.
    #[command(visible_alias = "sell-binary")]
    Redeem {
        oracle_id: String,
        #[arg(long)]
        strike: f64,
        #[arg(long, conflicts_with = "down")]
        up: bool,
        #[arg(long, conflicts_with = "up")]
        down: bool,
        #[arg(long)]
        qty: f64,
        #[arg(long)]
        permissionless: bool,
    },

    /// Redeem a range position (owner only).
    #[command(visible_alias = "sell-range")]
    RedeemRange {
        oracle_id: String,
        #[arg(long)]
        lower: f64,
        #[arg(long)]
        upper: f64,
        #[arg(long)]
        qty: f64,
    },

    /// LP-supply DUSDC into the vault, mint PLP shares.
    #[command(visible_alias = "add-liquidity")]
    Supply {
        #[arg(long)]
        amount: f64,
    },

    /// LP-withdraw: burn a PLP coin, return DUSDC.
    #[command(visible_alias = "remove-liquidity")]
    Withdraw {
        /// PLP coin object id (0x…).
        plp: String,
    },

    /// How to get DUSDC + SUI for testnet.
    Faucet,

    /// Run environment checks (sui installed, testnet env, gas, DUSDC, manager).
    /// Prints the exact next command for anything missing.
    #[command(visible_alias = "setup")]
    Doctor,

    /// Manage agent-driven "perp" positions: open, list, close, inspect.
    /// M1 surface is structured-DSL only; `agent ask "..."` arrives in M4.
    Agent {
        #[command(subcommand)]
        sub: AgentCmd,
    },
}

#[derive(Subcommand)]
enum AgentCmd {
    /// Open a managed position from a structured intent.
    Open {
        /// up | down | range
        #[arg(long)]
        side: String,
        /// Underlying asset: BTC (more soon).
        #[arg(long, default_value = "BTC")]
        asset: String,
        /// Tenor: e.g. 30m, 1h, 90m. Picks the active oracle whose remaining
        /// time best matches.
        #[arg(long, default_value = "1h")]
        tenor: String,
        /// Risk budget in DUSDC. Caps total spend for the entire position.
        #[arg(long)]
        risk: f64,
        /// Stable id for this position. Defaults to a generated `p-XXXXXXXX`.
        #[arg(long)]
        tag: Option<String>,
        /// Rolling policy: none (single-shot) | auto (M3+; ignored at M1).
        #[arg(long, default_value = "none")]
        rolling: String,
        /// Skip the confirm prompt.
        #[arg(long, short = 'y')]
        yes: bool,
    },
    /// List managed positions and their current state.
    Positions,
    /// Close a managed position. Owner-redeems every still-open leg.
    Close {
        /// Position id (`tag` from `agent open` or the generated `p-…`).
        id: String,
    },
    /// Print the full record of a single position.
    Inspect { id: String },

    /// Open a position from a free-text intent. Default provider is the offline
    /// regex parser (no API key required). Set --provider anthropic (or
    /// PREDICT_AGENT_PROVIDER=anthropic) plus ANTHROPIC_API_KEY for an LLM-backed
    /// parser. Either way the same Plan validation runs locally before any tx.
    Ask {
        /// Free-text intent, e.g. "long BTC for 1h, $50".
        prompt: String,
        /// Provider override: none | anthropic. Falls back to
        /// PREDICT_AGENT_PROVIDER, then to none.
        #[arg(long)]
        provider: Option<String>,
        /// Rolling policy: none | auto.
        #[arg(long, default_value = "none")]
        rolling: String,
        /// Skip the confirm prompt.
        #[arg(long, short = 'y')]
        yes: bool,
    },

    /// Watch open positions and redeem them automatically when their oracles
    /// settle. With AutoOnSettlement rolling, a fresh leg is also opened in the
    /// next expiry as long as budget and tenor permit (M3).
    Watch {
        /// Poll cadence in seconds.
        #[arg(long, default_value_t = 30)]
        interval: u64,
        /// Run a single cycle and exit. Useful for cron and CI smoke tests.
        #[arg(long)]
        once: bool,
        /// Only watch one position by id.
        #[arg(long)]
        only: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let _ = dotenvy::dotenv();
    let cli = Cli::parse();

    match cli.command {
        Cmd::Config => commands::config::run(cli.json).await,
        Cmd::List { all } => commands::list::run(cli.json, all).await,
        Cmd::Oracle { oracle_id } => commands::oracle::run(&oracle_id, cli.json).await,
        Cmd::Vault => commands::vault::run(cli.json).await,
        Cmd::Quote {
            oracle_id,
            strike,
            up,
            down,
            lower,
            upper,
            stake,
        } => {
            if strike.is_some() && (lower.is_some() || upper.is_some()) {
                bail!("quote accepts either --strike or --lower/--upper, not both");
            }
            let is_up = quote_direction(strike.is_some(), up, down)?;
            commands::quote::run(commands::quote::Args {
                oracle_id,
                strike,
                is_up,
                lower,
                upper,
                stake,
                json: cli.json,
            })
            .await
        }
        Cmd::Manager { create } => commands::manager::run(create, cli.json).await,
        Cmd::Deposit { amount } => commands::trade::deposit(amount).await,
        Cmd::Mint {
            oracle_id,
            strike,
            up,
            down,
            qty,
            deposit,
            max_cost,
            allow_manager_balance,
        } => {
            commands::trade::mint_binary(commands::trade::MintBinary {
                oracle_id,
                strike,
                is_up: required_direction(up, down)?,
                quantity: qty,
                deposit,
                max_cost,
                allow_manager_balance,
            })
            .await
        }
        Cmd::MintRange {
            oracle_id,
            lower,
            upper,
            qty,
            deposit,
            max_cost,
            allow_manager_balance,
        } => {
            commands::trade::mint_range(commands::trade::MintRange {
                oracle_id,
                lower,
                upper,
                quantity: qty,
                deposit,
                max_cost,
                allow_manager_balance,
            })
            .await
        }
        Cmd::Redeem {
            oracle_id,
            strike,
            up,
            down,
            qty,
            permissionless,
        } => {
            commands::trade::redeem_binary(commands::trade::RedeemBinary {
                oracle_id,
                strike,
                is_up: required_direction(up, down)?,
                quantity: qty,
                permissionless,
            })
            .await
        }
        Cmd::RedeemRange {
            oracle_id,
            lower,
            upper,
            qty,
        } => {
            commands::trade::redeem_range(commands::trade::RedeemRange {
                oracle_id,
                lower,
                upper,
                quantity: qty,
            })
            .await
        }
        Cmd::Supply { amount } => commands::trade::supply(amount).await,
        Cmd::Withdraw { plp } => commands::trade::withdraw(&plp).await,
        Cmd::Faucet => commands::faucet::run().await,
        Cmd::Doctor => commands::doctor::run().await,
        Cmd::Agent { sub } => dispatch_agent(sub, cli.json).await,
    }
}

async fn dispatch_agent(sub: AgentCmd, json: bool) -> Result<()> {
    match sub {
        AgentCmd::Open {
            side,
            asset,
            tenor,
            risk,
            tag,
            rolling,
            yes,
        } => {
            commands::agent::open(
                commands::agent::OpenArgs {
                    side,
                    asset,
                    tenor,
                    risk,
                    tag,
                    rolling,
                    yes,
                },
                json,
            )
            .await
        }
        AgentCmd::Ask {
            prompt,
            provider,
            rolling,
            yes,
        } => {
            commands::agent::ask(
                commands::agent::AskArgs {
                    prompt,
                    provider,
                    rolling,
                    yes,
                },
                json,
            )
            .await
        }
        AgentCmd::Positions => commands::agent::positions(json).await,
        AgentCmd::Close { id } => commands::agent::close(&id, json).await,
        AgentCmd::Inspect { id } => commands::agent::inspect(&id).await,
        AgentCmd::Watch {
            interval,
            once,
            only,
        } => {
            commands::agent::watch(
                commands::agent::WatchArgs {
                    interval,
                    once,
                    only,
                },
                json,
            )
            .await
        }
    }
}

fn required_direction(up: bool, down: bool) -> Result<bool> {
    match (up, down) {
        (true, false) => Ok(true),
        (false, true) => Ok(false),
        _ => bail!("choose exactly one direction: --up or --down"),
    }
}

fn quote_direction(is_binary: bool, up: bool, down: bool) -> Result<Option<bool>> {
    if is_binary {
        required_direction(up, down).map(Some)
    } else if up || down {
        bail!("--up/--down only apply when quoting with --strike")
    } else {
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn required_direction_requires_exactly_one_side() {
        assert!(required_direction(true, false).unwrap());
        assert!(!required_direction(false, true).unwrap());
        assert!(required_direction(false, false).is_err());
    }

    #[test]
    fn quote_direction_rejects_range_direction_flags() {
        assert!(quote_direction(false, true, false).is_err());
        assert!(quote_direction(false, false, false).unwrap().is_none());
    }
}
