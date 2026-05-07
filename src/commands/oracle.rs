//! `predict-cli oracle <ID>` — read the live OracleSVI shared object.

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

    if json {
        println!("{}", serde_json::to_string_pretty(fields)?);
        return Ok(());
    }

    let underlying = fields
        .get("underlying_asset")
        .and_then(|v| v.as_str())
        .unwrap_or("?");
    let active = fields
        .get("active")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let expiry = u64_str(fields.get("expiry"));
    let timestamp = u64_str(fields.get("timestamp"));

    let prices = pluck(fields, &["prices", "fields"])
        .cloned()
        .unwrap_or(Value::Null);
    let spot = u64_str(prices.get("spot"));
    let forward = u64_str(prices.get("forward"));

    let svi = pluck(fields, &["svi", "fields"])
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

    println!(
        "{} {} {} {}",
        "Oracle".bold(),
        underlying.bold().cyan(),
        "·".dimmed(),
        if active {
            "active".green().to_string()
        } else {
            "inactive".dimmed().to_string()
        }
    );
    println!("  {} {}", label("id"), oracle_id);
    println!();
    println!("  {} {}", label("expiry"), fmt_expiry(expiry));
    println!("  {} {}", label("countdown"), fmt_countdown(now_ms, expiry));
    println!(
        "  {} {} ({}ms ago)",
        label("last update"),
        fmt_expiry(timestamp),
        if timestamp > 0 {
            now_ms.saturating_sub(timestamp)
        } else {
            0
        }
    );
    println!();
    println!("  {}", "Prices".bold());
    println!("    {} {}", label("spot"), fmt_strike(spot_f, underlying));
    println!(
        "    {} {}",
        label("forward"),
        fmt_strike(forward_f, underlying)
    );
    if forward_f > 0.0 && spot_f > 0.0 {
        let basis = forward_f / spot_f;
        let bp = (basis - 1.0) * 10_000.0;
        println!("    {} {:.5}  ({:+.2} bps)", label("basis"), basis, bp);
    }
    println!();
    println!("  {}", "SVI".bold());
    println!("    {} {:.6}", label("a"), a);
    println!("    {} {:.6}", label("b"), b);
    println!("    {} {:+.6}", label("rho"), rho_signed);
    println!("    {} {:+.6}", label("m"), m_signed);
    println!("    {} {:.6}", label("sigma"), sigma);
    let days = ((expiry.saturating_sub(now_ms) as f64) / 86_400_000.0).max(0.0001);
    let svi = SviParams {
        a,
        b,
        rho: rho_signed,
        m: m_signed,
        sigma,
    };
    println!(
        "    {} {:.1}%  (annualized, ATM)",
        label("implied vol"),
        atm_vol(svi, days)
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
