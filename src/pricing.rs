//! Pricing math for DeepBook Predict positions.
//!
//! Mirrors `oracle::compute_price` and `oracle::compute_range_price` from the
//! testnet branch:
//!
//!   For an unsettled oracle and strike K:
//!     k    = ln(K / F)               (log-moneyness against the forward)
//!     w(k) = a + b · ( ρ·(k − m) + √((k − m)² + σ²) )      (SVI raw form)
//!     d₂   = −k / √w(k)  −  √w(k) / 2
//!     N(d₂)                          ← probability that S_T > K
//!
//! For sentinels: K = neg_inf → 1.0, K = pos_inf → 0.
//! Range price is the difference of two binary-call prices.
//!
//! This is the *fair* price (no fee). The contract's all-in price adds a
//! utilization-dependent spread; without devInspect we cannot reproduce it
//! exactly. Fees are typically a few basis points to a couple of cents.

use crate::config::{NEG_INF, POS_INF};

/// Standard normal CDF — Abramowitz & Stegun 7.1.26.
fn ncdf(z: f64) -> f64 {
    let a1 = 0.254829592;
    let a2 = -0.284496736;
    let a3 = 1.421413741;
    let a4 = -1.453152027;
    let a5 = 1.061405429;
    let p = 0.3275911;
    let sign = if z < 0.0 { -1.0 } else { 1.0 };
    let x = z.abs() / 2f64.sqrt();
    let t = 1.0 / (1.0 + p * x);
    let y = 1.0 - (((((a5 * t + a4) * t) + a3) * t + a2) * t + a1) * t * (-x * x).exp();
    0.5 * (1.0 + sign * y)
}

/// SVI total variance at log-moneyness k.
/// Raw form: w(k) = a + b · ( ρ·(k − m) + √((k − m)² + σ²) )
fn svi_total_variance(k: f64, a: f64, b: f64, rho: f64, m: f64, sigma: f64) -> f64 {
    let dx = k - m;
    let term = rho * dx + (dx * dx + sigma * sigma).sqrt();
    let w = a + b * term;
    w.max(0.0)
}

/// Binary-call price at strike K_human, given forward F_human and SVI params.
/// Returns probability that S_T > K, in [0, 1].
pub fn binary_call_price(
    forward: f64,
    strike: f64,
    a: f64,
    b: f64,
    rho: f64,
    m: f64,
    sigma: f64,
) -> f64 {
    if strike <= 0.0 || forward <= 0.0 {
        return 1.0;
    }
    let k = (strike / forward).ln();
    let w = svi_total_variance(k, a, b, rho, m, sigma);
    if w <= 0.0 {
        // Variance collapsed — settled-like behavior.
        return if forward > strike { 1.0 } else { 0.0 };
    }
    let sqrt_w = w.sqrt();
    let d2 = -k / sqrt_w - sqrt_w / 2.0;
    ncdf(d2)
}

/// Wrapped over float scaling for binary positions (UP / DOWN).
pub fn binary_price(forward: f64, strike: f64, is_up: bool, svi: SviParams) -> f64 {
    let p_above = binary_call_price(forward, strike, svi.a, svi.b, svi.rho, svi.m, svi.sigma);
    if is_up {
        p_above
    } else {
        1.0 - p_above
    }
}

/// Settled binary payout fraction. The contract treats UP as `settlement > strike`.
pub fn settled_binary_price(settlement: f64, strike: f64, is_up: bool) -> f64 {
    let up_wins = settlement > strike;
    if up_wins == is_up {
        1.0
    } else {
        0.0
    }
}

/// Range price: difference of two binary calls. Honors neg_inf / pos_inf sentinels.
pub fn range_price(
    forward: f64,
    lower_scaled: u64,
    upper_scaled: u64,
    float_scaling: f64,
    svi: SviParams,
) -> f64 {
    let p_above_lower = if lower_scaled == NEG_INF {
        1.0
    } else {
        let lower = lower_scaled as f64 / float_scaling;
        binary_call_price(forward, lower, svi.a, svi.b, svi.rho, svi.m, svi.sigma)
    };
    let p_above_upper = if upper_scaled == POS_INF {
        0.0
    } else {
        let upper = upper_scaled as f64 / float_scaling;
        binary_call_price(forward, upper, svi.a, svi.b, svi.rho, svi.m, svi.sigma)
    };
    (p_above_lower - p_above_upper).clamp(0.0, 1.0)
}

/// Settled range payout fraction. The contract range is `(lower, higher]`.
pub fn settled_range_price(
    settlement: f64,
    lower_scaled: u64,
    upper_scaled: u64,
    scale: f64,
) -> f64 {
    let above_lower = if lower_scaled == NEG_INF {
        true
    } else {
        settlement > lower_scaled as f64 / scale
    };
    let at_or_below_upper = if upper_scaled == POS_INF {
        true
    } else {
        settlement <= upper_scaled as f64 / scale
    };
    if above_lower && at_or_below_upper {
        1.0
    } else {
        0.0
    }
}

/// SVI surface parameters in human (post-1e9-divide) units.
#[derive(Debug, Clone, Copy)]
pub struct SviParams {
    pub a: f64,
    pub b: f64,
    pub rho: f64,
    pub m: f64,
    pub sigma: f64,
}

/// ATM annualized implied vol (%) — for display only. Uses the SVI total
/// variance at log-moneyness k=0 (forward, not spot) and a time-to-expiry.
pub fn atm_vol(svi: SviParams, days: f64) -> f64 {
    let t = (days / 365.0).max(1.0 / 24.0 / 365.0);
    let w0 = svi_total_variance(0.0, svi.a, svi.b, svi.rho, svi.m, svi.sigma);
    let var_annual = if w0 > 0.0 { w0 / t } else { 0.0 };
    var_annual.sqrt() * 100.0
}

/* -------------------------------------------------------------------- tests */

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ncdf_known_values() {
        assert!((ncdf(0.0) - 0.5).abs() < 1e-7);
        assert!((ncdf(1.0) - 0.8413447).abs() < 1e-6);
        assert!((ncdf(-1.0) - 0.1586553).abs() < 1e-6);
        assert!((ncdf(2.0) - 0.9772499).abs() < 1e-6);
    }

    #[test]
    fn binary_atm_zero_dte_is_half_when_at_forward() {
        // F = K, very small w → d2 ≈ 0, N(0) = 0.5
        let p = binary_call_price(80_000.0, 80_000.0, 1e-9, 1e-9, 0.0, 0.0, 1e-9);
        assert!((p - 0.5).abs() < 0.01);
    }

    #[test]
    fn binary_deep_in_money_is_one() {
        let p = binary_call_price(80_000.0, 50_000.0, 0.0, 0.01, 0.0, 0.0, 0.01);
        assert!(p > 0.99, "deep ITM call should be ≈ 1, got {p}");
    }

    #[test]
    fn binary_deep_out_of_money_is_zero() {
        let p = binary_call_price(80_000.0, 200_000.0, 0.0, 0.01, 0.0, 0.0, 0.01);
        assert!(p < 0.01, "deep OTM call should be ≈ 0, got {p}");
    }

    #[test]
    fn range_above_below_complementary() {
        // Range (K, +∞) plus range (-∞, K) sums to 1.
        let scaling = 1e9;
        let svi = SviParams {
            a: 0.04,
            b: 0.4,
            rho: -0.2,
            m: 0.0,
            sigma: 0.1,
        };
        let k_scaled = (80_000.0 * scaling) as u64;
        let p_above = range_price(80_000.0, k_scaled, POS_INF, scaling, svi);
        let p_below = range_price(80_000.0, NEG_INF, k_scaled, scaling, svi);
        assert!((p_above + p_below - 1.0).abs() < 1e-6);
    }

    #[test]
    fn range_full_real_line_is_one() {
        let svi = SviParams {
            a: 0.04,
            b: 0.4,
            rho: -0.2,
            m: 0.0,
            sigma: 0.1,
        };
        let p = range_price(80_000.0, NEG_INF, POS_INF, 1e9, svi);
        assert!((p - 1.0).abs() < 1e-6);
    }

    #[test]
    fn range_same_strike_is_zero() {
        let svi = SviParams {
            a: 0.04,
            b: 0.4,
            rho: -0.2,
            m: 0.0,
            sigma: 0.1,
        };
        let k = (80_000.0 * 1e9) as u64;
        let p = range_price(80_000.0, k, k, 1e9, svi);
        assert!(p.abs() < 1e-9);
    }

    #[test]
    fn settled_binary_uses_strict_up_boundary() {
        assert_eq!(settled_binary_price(80_001.0, 80_000.0, true), 1.0);
        assert_eq!(settled_binary_price(80_000.0, 80_000.0, true), 0.0);
        assert_eq!(settled_binary_price(80_000.0, 80_000.0, false), 1.0);
    }

    #[test]
    fn settled_range_is_open_lower_closed_upper() {
        let scale = 1e9;
        let lo = (80_000.0 * scale) as u64;
        let hi = (82_000.0 * scale) as u64;
        assert_eq!(settled_range_price(80_000.0, lo, hi, scale), 0.0);
        assert_eq!(settled_range_price(82_000.0, lo, hi, scale), 1.0);
        assert_eq!(settled_range_price(82_000.01, lo, hi, scale), 0.0);
    }
}
