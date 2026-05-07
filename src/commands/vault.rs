//! `predict-cli vault` — read the Predict shared object's vault aggregates.

use anyhow::{anyhow, Result};
use owo_colors::OwoColorize;

use crate::config::{PREDICT_OBJECT, QUOTE_DECIMALS};
use crate::format::{fmt_usd, label};
use crate::rpc::{pluck, u64_str, Rpc};

pub async fn run(json: bool) -> Result<()> {
    let rpc = Rpc::new();
    let resp = rpc.get_object(PREDICT_OBJECT).await?;
    let fields = pluck(&resp, &["data", "content", "fields"])
        .ok_or_else(|| anyhow!("predict object: content missing"))?;

    if json {
        println!("{}", serde_json::to_string_pretty(fields)?);
        return Ok(());
    }

    let vault =
        pluck(fields, &["vault", "fields"]).ok_or_else(|| anyhow!("vault fields missing"))?;
    let balance = u64_str(vault.get("balance"));
    let total_mtm = u64_str(vault.get("total_mtm"));
    let total_max_payout = u64_str(vault.get("total_max_payout"));
    let trading_paused = fields
        .get("trading_paused")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    let div = 10f64.powi(QUOTE_DECIMALS as i32);
    let bal_f = (balance as f64) / div;
    let mtm_f = (total_mtm as f64) / div;
    let mp_f = (total_max_payout as f64) / div;
    let value_f = (bal_f - mtm_f).max(0.0);
    let avail_f = (bal_f - mp_f).max(0.0);
    let util_pct = if bal_f > 0.0 {
        (mp_f / bal_f) * 100.0
    } else {
        0.0
    };

    println!("{}", "Predict vault".bold());
    println!("  {} {}", label("predict object"), PREDICT_OBJECT);
    println!();
    println!("  {} {}", label("balance"), fmt_usd(bal_f));
    println!("  {} {}", label("total mtm"), fmt_usd(mtm_f));
    println!("  {} {}", label("total max payout"), fmt_usd(mp_f));
    println!();
    println!("  {} {}", label("vault value"), fmt_usd(value_f));
    println!("  {} {}", label("available withdrawal"), fmt_usd(avail_f));
    println!("  {} {:.2}%", label("utilization"), util_pct);
    println!();
    println!(
        "  {} {}",
        label("trading paused"),
        if trading_paused {
            "yes".red().to_string()
        } else {
            "no".green().to_string()
        }
    );

    Ok(())
}
