//! `predict-cli doctor`: environment checks with a fix-it command per failure.

use std::process::Command;

use anyhow::Result;
use owo_colors::OwoColorize;

use crate::config::QUOTE_TYPE;
use crate::format::fmt_usd;
use crate::rpc::Rpc;
use crate::server;

/// Minimum SUI (MIST) for a single mutating PTB. 0.05 SUI is conservative;
/// typical mint gas is well under this.
const MIN_SUI_GAS_MIST: u128 = 50_000_000;

#[derive(Clone, Copy)]
enum Status {
    Ok,
    Warn,
    Fail,
}

struct Check {
    name: &'static str,
    status: Status,
    detail: String,
    /// Exact command the user should run next, if applicable.
    next: Option<String>,
}

pub async fn run() -> Result<()> {
    println!("{}", "predict-cli doctor".bold());
    println!();

    let mut checks: Vec<Check> = Vec::new();

    let sui_installed = check_sui_installed();
    let have_sui = matches!(sui_installed.status, Status::Ok);
    checks.push(sui_installed);

    let mut addr: Option<String> = None;
    if have_sui {
        checks.push(check_active_env());
        let (addr_check, parsed) = check_active_address();
        addr = parsed;
        checks.push(addr_check);
    } else {
        checks.push(skipped("active env", "install sui first"));
        checks.push(skipped("active address", "install sui first"));
    }

    if let Some(a) = addr.as_deref() {
        let rpc = Rpc::new();
        checks.push(check_sui_balance(&rpc, a).await);
        checks.push(check_dusdc_balance(&rpc, a).await);
        checks.push(check_manager(a).await);
    } else {
        checks.push(skipped("sui gas", "set an active address first"));
        checks.push(skipped("dusdc", "set an active address first"));
        checks.push(skipped("manager", "set an active address first"));
    }

    for c in &checks {
        let icon = match c.status {
            Status::Ok => "✓".green().to_string(),
            Status::Warn => "!".yellow().to_string(),
            Status::Fail => "✗".red().to_string(),
        };
        println!("  {}  {}", icon, c.name.bold());
        println!("     {}", c.detail);
        if let Some(cmd) = &c.next {
            println!("     {} {}", "→".dimmed(), cmd.cyan());
        }
    }
    println!();

    let fails = checks
        .iter()
        .filter(|c| matches!(c.status, Status::Fail))
        .count();
    let warns = checks
        .iter()
        .filter(|c| matches!(c.status, Status::Warn))
        .count();

    if fails == 0 && warns == 0 {
        println!("{}", "ready.".green());
    } else if fails == 0 {
        println!(
            "{}",
            format!("{warns} warning(s); proceed if you only need read paths.").yellow()
        );
    } else {
        println!(
            "{}",
            format!("{fails} blocking; fix the ✗ items and re-run.").red()
        );
    }

    Ok(())
}

fn skipped(name: &'static str, why: &str) -> Check {
    Check {
        name,
        status: Status::Warn,
        detail: format!("skipped: {why}"),
        next: None,
    }
}

fn check_sui_installed() -> Check {
    match Command::new("sui").arg("--version").output() {
        Ok(out) if out.status.success() => {
            let v = String::from_utf8_lossy(&out.stdout).trim().to_string();
            Check {
                name: "sui binary",
                status: Status::Ok,
                detail: if v.is_empty() {
                    "sui found on PATH".into()
                } else {
                    v
                },
                next: None,
            }
        }
        _ => Check {
            name: "sui binary",
            status: Status::Fail,
            detail: "sui not found on PATH".into(),
            next: Some(
                "Install: https://docs.sui.io/guides/developer/getting-started/sui-install".into(),
            ),
        },
    }
}

fn check_active_env() -> Check {
    match Command::new("sui").args(["client", "active-env"]).output() {
        Ok(out) if out.status.success() => {
            let env = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if env == "testnet" {
                Check {
                    name: "active env",
                    status: Status::Ok,
                    detail: format!("env = {env}"),
                    next: None,
                }
            } else {
                Check {
                    name: "active env",
                    status: Status::Fail,
                    detail: format!("env = {env} (need testnet)"),
                    next: Some("sui client switch --env testnet".into()),
                }
            }
        }
        _ => Check {
            name: "active env",
            status: Status::Fail,
            detail: "could not read `sui client active-env`. Is sui configured?".into(),
            next: Some("sui client new-env --alias testnet --rpc https://fullnode.testnet.sui.io && sui client switch --env testnet".into()),
        },
    }
}

fn check_active_address() -> (Check, Option<String>) {
    match Command::new("sui")
        .args(["client", "active-address"])
        .output()
    {
        Ok(out) if out.status.success() => {
            let s = String::from_utf8_lossy(&out.stdout).trim().to_string();
            if s.starts_with("0x") {
                (
                    Check {
                        name: "active address",
                        status: Status::Ok,
                        detail: s.clone(),
                        next: None,
                    },
                    Some(s),
                )
            } else {
                (
                    Check {
                        name: "active address",
                        status: Status::Fail,
                        detail: format!("could not parse: {s}"),
                        next: Some("sui client new-address ed25519".into()),
                    },
                    None,
                )
            }
        }
        _ => (
            Check {
                name: "active address",
                status: Status::Fail,
                detail: "no active address".into(),
                next: Some("sui client new-address ed25519".into()),
            },
            None,
        ),
    }
}

async fn check_sui_balance(rpc: &Rpc, addr: &str) -> Check {
    match rpc.get_balance(addr, None).await {
        Ok(mist) => {
            let sui = (mist as f64) / 1_000_000_000.0;
            if mist >= MIN_SUI_GAS_MIST {
                Check {
                    name: "sui gas",
                    status: Status::Ok,
                    detail: format!("{sui:.4} SUI"),
                    next: None,
                }
            } else {
                Check {
                    name: "sui gas",
                    status: Status::Fail,
                    detail: format!("{sui:.4} SUI (low; mints need gas)"),
                    next: Some(format!(
                        "curl -X POST https://faucet.testnet.sui.io/gas -H 'content-type: application/json' -d '{{\"FixedAmountRequest\":{{\"recipient\":\"{addr}\"}}}}'"
                    )),
                }
            }
        }
        Err(e) => Check {
            name: "sui gas",
            status: Status::Warn,
            detail: format!("could not fetch SUI balance: {e}"),
            next: None,
        },
    }
}

async fn check_dusdc_balance(rpc: &Rpc, addr: &str) -> Check {
    match rpc.get_balance(addr, Some(QUOTE_TYPE)).await {
        Ok(units) => {
            let dusdc = (units as f64) / 1_000_000.0;
            if units > 0 {
                Check {
                    name: "dusdc (wallet)",
                    status: Status::Ok,
                    detail: fmt_usd(dusdc),
                    next: None,
                }
            } else {
                Check {
                    name: "dusdc (wallet)",
                    status: Status::Warn,
                    detail: "0 DUSDC (you need DUSDC to mint or supply)".into(),
                    next: Some("predict-cli faucet".into()),
                }
            }
        }
        Err(e) => Check {
            name: "dusdc (wallet)",
            status: Status::Warn,
            detail: format!("could not fetch DUSDC balance: {e}"),
            next: None,
        },
    }
}

async fn check_manager(addr: &str) -> Check {
    match server::find_manager_for(addr).await {
        Ok(Some(m)) => Check {
            name: "predict manager",
            status: Status::Ok,
            detail: m.manager_id,
            next: None,
        },
        Ok(None) => Check {
            name: "predict manager",
            status: Status::Warn,
            detail: "no manager yet (required for mint/redeem; not for read/quote)".into(),
            next: Some("predict-cli manager --create".into()),
        },
        Err(e) => Check {
            name: "predict manager",
            status: Status::Warn,
            detail: format!("could not query predict-server: {e}"),
            next: None,
        },
    }
}
