//! `predict-cli quote` — preview the fair-value price of a binary or vertical-range
//! position. v2 reads from `MarketOracle` (SVI + Block-Scholes prices +
//! settlement) and optionally `PythSource` (real-time spot). Accepts either an
//! ExpiryMarket ID (preferred) or a MarketOracle ID.
//!
//! The fair value shown is the spread-free price; the contract adds a
//! utilization fee on mint.

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

/// Quote-side market state. Sourced from the MarketOracle (forward, SVI,
/// settlement) and optionally enriched with PythSource spot.
struct ResolvedMarket {
    market_oracle_id: String,
    forward: f64,
    spot: f64,
    expiry: u64,
    svi: SviParams,
    settlement: Option<f64>,
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
    if args.strike.is_none() && args.lower.is_none() && args.upper.is_none() {
        bail!("provide either --strike with --up/--down, or --lower/--upper");
    }

    let market = resolve_market(&args.oracle_id).await?;

    let now_ms = chrono::Utc::now().timestamp_millis() as u64;
    let days = ((market.expiry.saturating_sub(now_ms) as f64) / 86_400_000.0).max(0.0001);
    let vol = atm_vol(market.svi, days);

    let (kind, fair, summary) = match (args.strike, args.lower, args.upper) {
        (Some(strike), _, _) => {
            let is_up = args
                .is_up
                .ok_or_else(|| anyhow!("binary quote requires exactly one of --up or --down"))?;
            let p = if let Some(s) = market.settlement {
                settled_binary_price(s, strike, is_up)
            } else {
                binary_price(market.forward, strike, is_up, market.svi)
            };
            (
                "binary",
                p,
                format!("{} ${}", if is_up { "ABOVE" } else { "BELOW" }, strike,),
            )
        }
        (_, Some(lo), Some(hi)) if hi > lo => {
            let lo_scaled = to_scaled("--lower", lo)?;
            let hi_scaled = to_scaled("--upper", hi)?;
            let p = if let Some(s) = market.settlement {
                settled_range_price(s, lo_scaled, hi_scaled, FLOAT_SCALING as f64)
            } else {
                range_price(
                    market.forward,
                    lo_scaled,
                    hi_scaled,
                    FLOAT_SCALING as f64,
                    market.svi,
                )
            };
            ("range", p, format!("${}–${}", lo, hi))
        }
        (_, Some(lo), None) => {
            let lo_scaled = to_scaled("--lower", lo)?;
            let p = if let Some(s) = market.settlement {
                settled_range_price(s, lo_scaled, POS_INF, FLOAT_SCALING as f64)
            } else {
                range_price(
                    market.forward,
                    lo_scaled,
                    POS_INF,
                    FLOAT_SCALING as f64,
                    market.svi,
                )
            };
            ("range", p, format!("ABOVE ${}", lo))
        }
        (_, None, Some(hi)) => {
            let hi_scaled = to_scaled("--upper", hi)?;
            let p = if let Some(s) = market.settlement {
                settled_range_price(s, NEG_INF, hi_scaled, FLOAT_SCALING as f64)
            } else {
                range_price(
                    market.forward,
                    NEG_INF,
                    hi_scaled,
                    FLOAT_SCALING as f64,
                    market.svi,
                )
            };
            ("range", p, format!("BELOW ${}", hi))
        }
        _ => bail!("provide --strike with --up/--down, or --lower/--upper"),
    };

    let prob_pct = (fair * 100.0).round();
    let payout = if fair >= 0.01 { args.stake / fair } else { 0.0 };

    if args.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&serde_json::json!({
                "oracle": market.market_oracle_id,
                "input": args.oracle_id,
                "spot": market.spot,
                "forward": market.forward,
                "settlement": market.settlement,
                "atm_vol_pct": vol,
                "days_to_expiry": days,
                "kind": kind,
                "summary": summary,
                "fair_price_per_unit": fair,
                "implied_probability_pct": prob_pct,
                "stake_usdc": args.stake,
                "payout_if_win_usdc": payout,
                "note": if market.settlement.is_some() {
                    "settled payout fraction"
                } else {
                    "fair value only — contract adds a utilization fee on mint"
                },
            }))?
        );
        return Ok(());
    }

    println!("{}", "Quote".bold());
    println!("  {} {}", label("oracle"), market.market_oracle_id);
    println!(
        "  {} {}",
        label("market"),
        if let Some(s) = market.settlement {
            format!(
                "spot {} · fwd {} · settled {}",
                fmt_strike(market.spot, "?"),
                fmt_strike(market.forward, "?"),
                fmt_strike(s, "?")
            )
        } else {
            format!(
                "spot {} · fwd {}",
                fmt_strike(market.spot, "?"),
                fmt_strike(market.forward, "?")
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
    if market.settlement.is_none() {
        println!();
        println!(
            "  {}",
            "note: fair value only. The contract adds a utilization fee at mint time.".dimmed()
        );
    }

    Ok(())
}

/// Resolve the user-supplied object id into a fully-populated quote view.
///
/// The id may be an `ExpiryMarket` (preferred) or a `MarketOracle` directly.
/// `ExpiryMarket` is detected by the presence of `market_oracle_id`; we then
/// read the paired oracle. PythSource is optionally read to override the
/// (possibly stale) Block-Scholes spot with a fresher number.
async fn resolve_market(id: &str) -> Result<ResolvedMarket> {
    let rpc = Rpc::new();
    let primary = rpc.get_object(id).await?;
    let fields = pluck(&primary, &["data", "content", "fields"])
        .ok_or_else(|| anyhow!("object content missing — wrong id or not visible?"))?;

    let (oracle_id, oracle_fields, pyth_id) =
        if let Some(paired) = fields.get("market_oracle_id").and_then(|v| v.as_str()) {
            // ExpiryMarket input: read paired oracle.
            let oracle = rpc.get_object(paired).await?;
            let of = pluck(&oracle, &["data", "content", "fields"])
                .ok_or_else(|| anyhow!("market oracle: object content missing"))?
                .clone();
            let pyth = of
                .get("pyth_source_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            (paired.to_string(), of, pyth)
        } else if fields.get("block_scholes_forward").is_some() {
            let pyth = fields
                .get("pyth_source_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            (id.to_string(), fields.clone(), pyth)
        } else {
            bail!(
                "object {id} is neither an ExpiryMarket nor a MarketOracle — \
                 wrong id?"
            );
        };

    let expiry = u64_str(oracle_fields.get("expiry"));
    let forward_scaled = u64_str(oracle_fields.get("block_scholes_forward"));
    let bs_spot_scaled = u64_str(oracle_fields.get("block_scholes_spot"));
    let svi_node = pluck(&oracle_fields, &["block_scholes_svi", "fields"])
        .cloned()
        .unwrap_or_default();
    let settlement = oracle_fields
        .get("settlement_price")
        .and_then(option_u64)
        .map(|v| v as f64 / FLOAT_SCALING as f64);

    // Prefer the PythSource's live spot if available — it's the freshest
    // number on-chain. Fall back to the Block-Scholes spot snapshot otherwise.
    let mut spot_scaled = bs_spot_scaled;
    if let Some(pid) = pyth_id.as_deref() {
        if let Ok(p) = rpc.get_object(pid).await {
            if let Some(pf) = pluck(&p, &["data", "content", "fields"]) {
                let live = u64_str(pf.get("spot"));
                if live > 0 {
                    spot_scaled = live;
                }
            }
        }
    }

    Ok(ResolvedMarket {
        market_oracle_id: oracle_id,
        forward: forward_scaled as f64 / FLOAT_SCALING as f64,
        spot: spot_scaled as f64 / FLOAT_SCALING as f64,
        expiry,
        svi: SviParams {
            a: u64_str(svi_node.get("a")) as f64 / FLOAT_SCALING as f64,
            b: u64_str(svi_node.get("b")) as f64 / FLOAT_SCALING as f64,
            rho: signed_i64(svi_node.get("rho")),
            m: signed_i64(svi_node.get("m")),
            sigma: u64_str(svi_node.get("sigma")) as f64 / FLOAT_SCALING as f64,
        },
        settlement,
    })
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
