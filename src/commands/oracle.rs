//! `predict-cli oracle <ID>` — read the v2 `MarketOracle` shared object.
//!
//! In v2 the live state is split:
//!   - `MarketOracle` holds SVI + Black-Scholes spot/forward + settlement.
//!   - `PythSource` (one per Pyth Lazer feed) holds the real-time spot used
//!     for live pricing freshness checks.
//!
//! The CLI accepts EITHER a MarketOracle ID OR an ExpiryMarket ID. ExpiryMarket
//! inputs are transparently resolved to the paired MarketOracle.

use anyhow::{anyhow, Result};
use owo_colors::OwoColorize;
use serde_json::Value;

use crate::config::FLOAT_SCALING;
use crate::format::{fmt_countdown, fmt_expiry, fmt_strike, fmt_usd, label};
use crate::pricing::{atm_vol, SviParams};
use crate::rpc::{option_u64, pluck, u64_str, Rpc};

pub async fn run(oracle_id: &str, json: bool) -> Result<()> {
    let rpc = Rpc::new();
    let resp = rpc.get_object(oracle_id).await?;
    let fields = pluck(&resp, &["data", "content", "fields"])
        .ok_or_else(|| anyhow!("oracle: object content missing — wrong id or not visible"))?;

    // ExpiryMarket has `market_oracle_id`; resolve to the paired oracle.
    if let Some(paired) = fields
        .get("market_oracle_id")
        .and_then(|v| v.as_str())
        .map(str::to_string)
    {
        println!(
            "  {} ID resolved as ExpiryMarket; reading paired MarketOracle…",
            "ℹ".cyan()
        );
        return Box::pin(run(&paired, json)).await;
    }

    if json {
        println!("{}", serde_json::to_string_pretty(fields)?);
        return Ok(());
    }

    let expiry = u64_str(fields.get("expiry"));
    let spot = u64_str(fields.get("block_scholes_spot"));
    let forward = u64_str(fields.get("block_scholes_forward"));
    let svi_source_ts = u64_str(fields.get("block_scholes_svi_source_timestamp_ms"));
    let svi_update_ts = u64_str(fields.get("block_scholes_svi_update_timestamp_ms"));
    let price_source_ts = u64_str(fields.get("block_scholes_price_source_timestamp_ms"));
    let price_update_ts = u64_str(fields.get("block_scholes_price_update_timestamp_ms"));
    let pyth_source_id = fields
        .get("pyth_source_id")
        .and_then(|v| v.as_str())
        .unwrap_or("?");

    let svi = pluck(fields, &["block_scholes_svi", "fields"])
        .cloned()
        .unwrap_or(Value::Null);
    let a = u64_str(svi.get("a")) as f64 / FLOAT_SCALING as f64;
    let b = u64_str(svi.get("b")) as f64 / FLOAT_SCALING as f64;
    let sigma = u64_str(svi.get("sigma")) as f64 / FLOAT_SCALING as f64;
    let m_signed = signed_i64(svi.get("m"));
    let rho_signed = signed_i64(svi.get("rho"));

    let settlement = fields
        .get("settlement_price")
        .and_then(option_u64)
        .map(|v| (v as f64) / (FLOAT_SCALING as f64));

    let now_ms = chrono::Utc::now().timestamp_millis() as u64;
    let spot_f = (spot as f64) / (FLOAT_SCALING as f64);
    let forward_f = (forward as f64) / (FLOAT_SCALING as f64);

    let active = settlement.is_none() && expiry > now_ms;
    println!(
        "{} {} {}",
        "MarketOracle".bold(),
        "·".dimmed(),
        if active {
            "active".green().to_string()
        } else if settlement.is_some() {
            "settled".dimmed().to_string()
        } else {
            "pending settlement".yellow().to_string()
        }
    );
    println!("  {} {}", label("id"), oracle_id);
    println!("  {} {}", label("pyth source"), pyth_source_id);
    println!();
    println!("  {} {}", label("expiry"), fmt_expiry(expiry));
    println!("  {} {}", label("countdown"), fmt_countdown(now_ms, expiry));
    println!();
    println!("  {}", "Block-Scholes prices".bold());
    println!("    {} {}", label("spot"), fmt_strike(spot_f, "?"));
    println!("    {} {}", label("forward"), fmt_strike(forward_f, "?"));
    if forward_f > 0.0 && spot_f > 0.0 {
        let basis = forward_f / spot_f;
        let bp = (basis - 1.0) * 10_000.0;
        println!("    {} {:.5}  ({:+.2} bps)", label("basis"), basis, bp);
    }
    println!(
        "    {} src {} · upd {}",
        label("price ts"),
        if price_source_ts > 0 {
            fmt_expiry(price_source_ts)
        } else {
            "—".into()
        },
        if price_update_ts > 0 {
            fmt_expiry(price_update_ts)
        } else {
            "—".into()
        }
    );
    println!();
    println!("  {}", "SVI".bold());
    println!("    {} {:.6}", label("a"), a);
    println!("    {} {:.6}", label("b"), b);
    println!("    {} {:+.6}", label("rho"), rho_signed);
    println!("    {} {:+.6}", label("m"), m_signed);
    println!("    {} {:.6}", label("sigma"), sigma);
    println!(
        "    {} src {} · upd {}",
        label("svi ts"),
        if svi_source_ts > 0 {
            fmt_expiry(svi_source_ts)
        } else {
            "—".into()
        },
        if svi_update_ts > 0 {
            fmt_expiry(svi_update_ts)
        } else {
            "—".into()
        }
    );
    let days = ((expiry.saturating_sub(now_ms) as f64) / 86_400_000.0).max(0.0001);
    let svi_params = SviParams {
        a,
        b,
        rho: rho_signed,
        m: m_signed,
        sigma,
    };
    println!(
        "    {} {:.1}%  (annualized, ATM)",
        label("implied vol"),
        atm_vol(svi_params, days)
    );
    if let Some(s) = settlement {
        println!();
        println!("  {} {}", "Settled at".bold(), fmt_usd(s));
    }
    Ok(())
}

fn signed_i64(v: Option<&Value>) -> f64 {
    let Some(v) = v else { return 0.0 };
    let neg = pluck(v, &["fields", "is_negative"])
        .and_then(|x| x.as_bool())
        .unwrap_or(false);
    let mag = pluck(v, &["fields", "magnitude"])
        .and_then(|x| x.as_str())
        .and_then(|s| s.parse::<i128>().ok())
        .unwrap_or(0);
    let f = (mag as f64) / (FLOAT_SCALING as f64);
    if neg {
        -f
    } else {
        f
    }
}
