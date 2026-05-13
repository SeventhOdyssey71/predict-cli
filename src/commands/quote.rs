//! `predict-cli quote` — preview the fair-value price of a binary or vertical-range
//! position. Reads the live OracleSVI shared object and applies the same SVI →
//! N(d₂) formula the contract uses (`oracle::compute_price`). The all-in price
//! the contract would actually charge includes a utilization-dependent fee on
//! top; we display the spread-free fair value here.

use anyhow::{anyhow, bail, Result};
use owo_colors::OwoColorize;
use serde_json::Value;

use crate::config::{FLOAT_SCALING, NEG_INF, POS_INF};
use crate::format::{fmt_strike, fmt_usd, label, to_scaled};
use crate::pricing::{
    atm_vol, binary_price, range_price, settled_binary_price, settled_range_price, SviParams,
};
use crate::rpc::{option_u64, pluck, u64_str, Rpc};

pub struct Args {
    pub oracle_id: String,
    pub strike: Option<f64>,
    pub is_up: Option<bool>,
    pub lower: Option<f64>,
    pub upper: Option<f64>,
    pub stake: f64,
    pub json: bool,
}

pub async fn run(args: Args) -> Result<()> {
    validate_pos("--stake", args.stake)?;
    if let Some(s) = args.strike {
        validate_pos("--strike", s)?;
    }
    if let Some(l) = args.lower {
        if l < 0.0 || !l.is_finite() {
            bail!("--lower must be ≥ 0 and finite, got {l}");
        }
    }
    if let Some(u) = args.upper {
        validate_pos("--upper", u)?;
    }

    // Fail fast on the no-mode case before doing any RPC work.
    if args.strike.is_none() && args.lower.is_none() && args.upper.is_none() {
        bail!("provide either --strike with --up/--down, or --lower/--upper");
    }

    let rpc = Rpc::new();
    let resp = rpc.get_object(&args.oracle_id).await?;
    let fields = pluck(&resp, &["data", "content", "fields"])
        .ok_or_else(|| anyhow!("oracle: object content missing — wrong id?"))?;

    let underlying = fields
        .get("underlying_asset")
        .and_then(|v| v.as_str())
        .unwrap_or("?")
        .to_string();
    let expiry = u64_str(fields.get("expiry"));
    let prices = pluck(fields, &["prices", "fields"])
        .cloned()
        .unwrap_or_default();
    let svi_node = pluck(fields, &["svi", "fields"])
        .cloned()
        .unwrap_or_default();

    let spot_scaled = u64_str(prices.get("spot"));
    let forward_scaled = u64_str(prices.get("forward"));
    let spot = spot_scaled as f64 / FLOAT_SCALING as f64;
    let forward = forward_scaled as f64 / FLOAT_SCALING as f64;
    let settlement = fields
        .get("settlement_price")
        .and_then(option_u64)
        .map(|v| v as f64 / FLOAT_SCALING as f64);

    let svi = SviParams {
        a: u64_str(svi_node.get("a")) as f64 / FLOAT_SCALING as f64,
        b: u64_str(svi_node.get("b")) as f64 / FLOAT_SCALING as f64,
        rho: signed_i64(svi_node.get("rho")),
        m: signed_i64(svi_node.get("m")),
        sigma: u64_str(svi_node.get("sigma")) as f64 / FLOAT_SCALING as f64,
    };

    let now_ms = chrono::Utc::now().timestamp_millis() as u64;
    let days = ((expiry.saturating_sub(now_ms) as f64) / 86_400_000.0).max(0.0001);
    let vol = atm_vol(svi, days);

    let (kind, fair, summary) = match (args.strike, args.lower, args.upper) {
        (Some(strike), _, _) => {
            let is_up = args
                .is_up
                .ok_or_else(|| anyhow!("binary quote requires exactly one of --up or --down"))?;
            let p = if let Some(s) = settlement {
                settled_binary_price(s, strike, is_up)
            } else {
                binary_price(forward, strike, is_up, svi)
            };
            (
                "binary",
                p,
                format!(
                    "{} {} ${}",
                    if is_up { "ABOVE" } else { "BELOW" },
                    underlying,
                    strike,
                ),
            )
        }
        (_, Some(lo), Some(hi)) if hi > lo => {
            let lo_scaled = to_scaled("--lower", lo)?;
            let hi_scaled = to_scaled("--upper", hi)?;
            let p = if let Some(s) = settlement {
                settled_range_price(s, lo_scaled, hi_scaled, FLOAT_SCALING as f64)
            } else {
                range_price(forward, lo_scaled, hi_scaled, FLOAT_SCALING as f64, svi)
            };
            ("range", p, format!("{} ${}–${}", underlying, lo, hi))
        }
        (_, Some(lo), None) => {
            let lo_scaled = to_scaled("--lower", lo)?;
            let p = if let Some(s) = settlement {
                settled_range_price(s, lo_scaled, POS_INF, FLOAT_SCALING as f64)
            } else {
                range_price(forward, lo_scaled, POS_INF, FLOAT_SCALING as f64, svi)
            };
            ("range", p, format!("{} ABOVE ${}", underlying, lo))
        }
        (_, None, Some(hi)) => {
            let hi_scaled = to_scaled("--upper", hi)?;
            let p = if let Some(s) = settlement {
                settled_range_price(s, NEG_INF, hi_scaled, FLOAT_SCALING as f64)
            } else {
                range_price(forward, NEG_INF, hi_scaled, FLOAT_SCALING as f64, svi)
            };
            ("range", p, format!("{} BELOW ${}", underlying, hi))
        }
        _ => bail!("provide --strike with --up/--down, or --lower/--upper"),
    };

    let prob_pct = (fair * 100.0).round();
    let payout = if fair >= 0.01 { args.stake / fair } else { 0.0 };

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "oracle": args.oracle_id,
                "underlying": underlying,
                "spot": spot,
                "forward": forward,
                "settlement": settlement,
                "atm_vol_pct": vol,
                "days_to_expiry": days,
                "kind": kind,
                "summary": summary,
                "fair_price_per_unit": fair,
                "implied_probability_pct": prob_pct,
                "stake_usdc": args.stake,
                "payout_if_win_usdc": payout,
                "note": if settlement.is_some() {
                    "settled payout fraction"
                } else {
                    "fair value only — contract adds a utilization fee on mint"
                },
            }))?
        );
        return Ok(());
    }

    println!("{}", "Quote".bold());
    println!("  {} {}", label("oracle"), args.oracle_id);
    println!(
        "  {} {} {}",
        label("market"),
        underlying,
        if let Some(s) = settlement {
            format!(
                "· spot {} · fwd {} · settled {}",
                fmt_strike(spot, &underlying),
                fmt_strike(forward, &underlying),
                fmt_strike(s, &underlying)
            )
        } else {
            format!(
                "· spot {} · fwd {}",
                fmt_strike(spot, &underlying),
                fmt_strike(forward, &underlying)
            )
        }
        .dimmed()
    );
    println!("  {} {}", label("position"), summary);
    println!(
        "  {} {:.2} days  ({} ATM vol {:.1}%)",
        label("expiry in"),
        days,
        "·".dimmed(),
        vol
    );
    println!();
    println!(
        "  {} {:.4}  ({:.0}¢ per unit · implied {:.0}%)",
        label("fair price"),
        fair,
        fair * 100.0,
        prob_pct
    );
    if payout > 0.0 {
        println!(
            "  {} {} → {} if win",
            label("stake"),
            fmt_usd(args.stake),
            fmt_usd(payout).bold()
        );
    } else {
        println!(
            "  {} {} → {} (effectively unsellable; price < 1¢)",
            label("stake"),
            fmt_usd(args.stake),
            "—".bold()
        );
    }
    if settlement.is_none() {
        println!();
        println!(
            "  {}",
            "note: fair value only. The contract adds a utilization fee at mint time.".dimmed()
        );
    }

    Ok(())
}

fn validate_pos(name: &str, v: f64) -> Result<f64> {
    if !v.is_finite() || v <= 0.0 {
        bail!("{name} must be a positive finite number, got {v}");
    }
    Ok(v)
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
