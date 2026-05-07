//! Pretty-printers for terminal output.

use anyhow::{bail, Result};
use chrono::{DateTime, Utc};
use owo_colors::OwoColorize;

use crate::config::{FLOAT_SCALING, QUOTE_DECIMALS};

/// human float → 1e9-scaled u64.
pub fn to_scaled(name: &str, v: f64) -> Result<u64> {
    to_units(name, v, FLOAT_SCALING as f64)
}

/// human float (USDC) → 6-decimal u64.
pub fn to_quote(name: &str, v: f64) -> Result<u64> {
    to_units(name, v, 10f64.powi(QUOTE_DECIMALS as i32))
}

fn to_units(name: &str, v: f64, scale: f64) -> Result<u64> {
    if !v.is_finite() || v <= 0.0 {
        bail!("{name} must be a positive finite number, got {v}");
    }
    let scaled = v * scale;
    if !scaled.is_finite() || scaled > u64::MAX as f64 {
        bail!("{name} is too large to encode as u64 at this precision");
    }
    let rounded = scaled.round();
    if rounded < 1.0 {
        bail!(
            "{name} is too small; minimum encodable amount is {}",
            1.0 / scale
        );
    }
    Ok(rounded as u64)
}

pub fn fmt_usd(v: f64) -> String {
    if v >= 1000.0 {
        format!("${:.2}", v)
    } else {
        format!("${:.4}", v)
    }
}

pub fn fmt_strike(v: f64, asset: &str) -> String {
    let _ = asset;
    if v >= 1000.0 {
        format!("${:.0}", v)
    } else {
        format!("${:.2}", v)
    }
}

pub fn fmt_expiry(ms: u64) -> String {
    let dt = DateTime::<Utc>::from_timestamp_millis(ms as i64)
        .unwrap_or_else(|| DateTime::<Utc>::from_timestamp(0, 0).unwrap());
    dt.format("%Y-%m-%d %H:%M UTC").to_string()
}

pub fn fmt_countdown(now_ms: u64, expiry_ms: u64) -> String {
    if expiry_ms <= now_ms {
        return "expired".into();
    }
    let s = (expiry_ms - now_ms) / 1000;
    let d = s / 86_400;
    let h = (s % 86_400) / 3600;
    let m = (s % 3600) / 60;
    if d > 0 {
        format!("{}d {}h", d, h)
    } else if h > 0 {
        format!("{}h {}m", h, m)
    } else {
        format!("{}m {}s", m, s % 60)
    }
}

pub fn shorten(addr: &str) -> String {
    if addr.len() < 14 {
        return addr.to_string();
    }
    format!("{}…{}", &addr[..8], &addr[addr.len() - 4..])
}

pub fn label(s: &str) -> String {
    format!("{}", s.dimmed())
}

/* --------------------------------------------------------------------- tests */

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn to_scaled_rejects_nan_inf_negative() {
        assert!(to_scaled("x", f64::NAN).is_err());
        assert!(to_scaled("x", f64::INFINITY).is_err());
        assert!(to_scaled("x", -1.0).is_err());
        assert!(to_scaled("x", 0.0).is_err());
    }

    #[test]
    fn to_scaled_round_trip() {
        assert_eq!(to_scaled("x", 1.0).unwrap(), 1_000_000_000);
        assert_eq!(to_scaled("x", 82_000.0).unwrap(), 82_000_000_000_000);
    }

    #[test]
    fn to_scaled_rejects_overflow_and_dust() {
        assert!(to_scaled("x", u64::MAX as f64).is_err());
        assert!(to_scaled("x", 0.000_000_000_1).is_err());
    }

    #[test]
    fn to_quote_micro_dusdc() {
        assert_eq!(to_quote("x", 1.0).unwrap(), 1_000_000);
        assert_eq!(to_quote("x", 0.000001).unwrap(), 1);
        assert!(to_quote("x", -1.0).is_err());
        assert!(to_quote("x", f64::NAN).is_err());
        assert!(to_quote("x", 0.0000001).is_err());
    }

    #[test]
    fn shorten_safe_for_short_strings() {
        assert_eq!(shorten("short"), "short");
        let long = "0x".to_string() + &"a".repeat(60);
        let s = shorten(&long);
        assert!(s.contains('…'));
        assert!(s.starts_with("0x"));
        assert!(s.ends_with("aaaa"));
    }

    #[test]
    fn fmt_countdown_basic() {
        assert_eq!(fmt_countdown(100, 50), "expired");
        let now = 0;
        let in_one_min = 60_000;
        assert_eq!(fmt_countdown(now, in_one_min), "1m 0s");
    }
}
