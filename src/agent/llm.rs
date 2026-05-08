//! LLM provider abstraction for `agent ask`.
//!
//! Three backends:
//! - `none`       offline regex parser, no API key, default.
//! - `anthropic`  calls Claude (`claude-sonnet-4-6` by default).
//! - `openai`     reserved for M5+; not implemented yet.
//!
//! Every backend returns the same `StructuredIntent` shape, which the local
//! planner then turns into a validated `Plan`. The LLM never sees the position
//! store, never proposes oracle ids, and never produces a `Plan` directly —
//! that boundary is the whole point.

use anyhow::{anyhow, bail, Context, Result};
use serde::Deserialize;

use crate::agent::intent::{IntentSide, StructuredIntent};
use crate::agent::plan::RollingPolicy;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Provider {
    None,
    Anthropic,
}

impl Provider {
    pub fn from_str_or_env(explicit: Option<&str>) -> Result<Self> {
        let pick = explicit
            .map(|s| s.to_string())
            .or_else(|| std::env::var("PREDICT_AGENT_PROVIDER").ok())
            .unwrap_or_else(|| "none".into());
        match pick.to_ascii_lowercase().as_str() {
            "none" | "regex" | "offline" => Ok(Self::None),
            "anthropic" | "claude" => Ok(Self::Anthropic),
            other => bail!("unknown provider `{other}` (expected none|anthropic)"),
        }
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::None => "none (offline regex)",
            Self::Anthropic => "anthropic (claude)",
        }
    }
}

/// Resolve a free-text prompt to a [`StructuredIntent`] using the chosen
/// backend.
pub async fn resolve(prompt: &str, provider: Provider) -> Result<StructuredIntent> {
    match provider {
        Provider::None => parse_regex(prompt),
        Provider::Anthropic => resolve_anthropic(prompt).await,
    }
}

/* ────────────────────────────────────────────────────────────── regex backend */

/// Parse a free-text intent without any external service. Recognises:
/// - asset: BTC | ETH | SUI (case-insensitive)
/// - direction: long/up | short/down | range/between
/// - amount: $50, 50usdc, 50 dollars, 50.5
/// - tenor: 30m, 1h, 90m, 2 hours, 90 minutes
/// - tag: `tag=...` or `name=...`
pub fn parse_regex(prompt: &str) -> Result<StructuredIntent> {
    let s = prompt.to_ascii_lowercase();

    let asset = pick_asset(&s).ok_or_else(|| {
        anyhow!(
            "could not infer asset from `{prompt}` (try BTC, ETH, or SUI). \
             For an exact spec, use `agent open` instead."
        )
    })?;
    let side = pick_side(&s).ok_or_else(|| {
        anyhow!(
            "could not infer direction from `{prompt}` \
             (try `long`, `short`, `up`, `down`, or `range`)"
        )
    })?;
    let risk_usdc = pick_amount(&s).ok_or_else(|| {
        anyhow!("could not infer risk amount from `{prompt}` (e.g. `\\$50` or `50 usdc`)")
    })?;
    let tenor_minutes = pick_tenor(&s).unwrap_or(60);
    let tag = pick_tag(prompt);

    Ok(StructuredIntent {
        side,
        asset,
        tenor_minutes,
        risk_usdc,
        tag,
        rolling: RollingPolicy::None,
    })
}

fn pick_asset(s: &str) -> Option<String> {
    for token in ["btc", "ethereum", "eth", "sui"] {
        if word_contains(s, token) {
            return Some(if token == "ethereum" {
                "ETH".into()
            } else {
                token.to_uppercase()
            });
        }
    }
    None
}

fn pick_side(s: &str) -> Option<IntentSide> {
    if word_contains(s, "long")
        || word_contains(s, "bullish")
        || word_contains(s, "up")
        || word_contains(s, "above")
    {
        return Some(IntentSide::Up);
    }
    if word_contains(s, "short")
        || word_contains(s, "bearish")
        || word_contains(s, "down")
        || word_contains(s, "below")
    {
        return Some(IntentSide::Down);
    }
    if word_contains(s, "range")
        || word_contains(s, "between")
        || word_contains(s, "stays")
        || word_contains(s, "chop")
    {
        return Some(IntentSide::Range);
    }
    None
}

fn pick_amount(s: &str) -> Option<f64> {
    // Match $50, $50.5, 50 usdc, 50 dollars, 50 bucks. We scan for a number
    // followed by a currency cue, OR a $-prefixed number.
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c == '$' {
            if let Some((val, end)) = read_number(&s[i + 1..]) {
                return Some(val).filter(|v| v.is_finite() && *v > 0.0).or_else(|| {
                    let _ = end;
                    None
                });
            }
        }
        if c.is_ascii_digit() {
            if let Some((val, end)) = read_number(&s[i..]) {
                let after = s[i + end..].trim_start();
                let has_currency = after.starts_with("usdc")
                    || after.starts_with("dusdc")
                    || after.starts_with("usd")
                    || after.starts_with("dollar")
                    || after.starts_with("buck");
                if has_currency && val.is_finite() && val > 0.0 {
                    return Some(val);
                }
                i += end;
                continue;
            }
        }
        i += 1;
    }
    None
}

fn pick_tenor(s: &str) -> Option<u64> {
    let bytes = s.as_bytes();
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i] as char;
        if c.is_ascii_digit() {
            if let Some((val, end)) = read_number(&s[i..]) {
                let after = s[i + end..].trim_start();
                if after.starts_with("minute") || after.starts_with("min") || after.starts_with('m')
                {
                    return Some(val.round() as u64);
                }
                if after.starts_with("hour") || after.starts_with("hr") || after.starts_with('h') {
                    return Some((val * 60.0).round() as u64);
                }
                i += end;
                continue;
            }
        }
        i += 1;
    }
    None
}

fn pick_tag(prompt: &str) -> Option<String> {
    for prefix in ["tag=", "name=", "id="] {
        if let Some(rest) = prompt.find(prefix) {
            let tail = &prompt[rest + prefix.len()..];
            let stop = tail
                .find(|c: char| c.is_whitespace() || c == ',')
                .unwrap_or(tail.len());
            let tag = tail[..stop].trim();
            if !tag.is_empty() {
                return Some(tag.to_string());
            }
        }
    }
    None
}

fn read_number(s: &str) -> Option<(f64, usize)> {
    let mut end = 0;
    let mut seen_dot = false;
    for (i, c) in s.char_indices() {
        if c.is_ascii_digit() {
            end = i + 1;
        } else if c == '.' && !seen_dot {
            seen_dot = true;
            end = i + 1;
        } else {
            break;
        }
    }
    if end == 0 {
        return None;
    }
    s[..end].parse::<f64>().ok().map(|v| (v, end))
}

fn word_contains(haystack: &str, needle: &str) -> bool {
    // Word-boundary-ish match: prefix or surrounded by non-letter chars.
    let bytes = haystack.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() {
        return false;
    }
    let mut i = 0;
    while i + n.len() <= bytes.len() {
        if &bytes[i..i + n.len()] == n {
            let before = if i == 0 {
                None
            } else {
                Some(bytes[i - 1] as char)
            };
            let after = bytes.get(i + n.len()).map(|b| *b as char);
            let left_ok = before.is_none_or(|c| !c.is_ascii_alphanumeric());
            let right_ok = after.is_none_or(|c| !c.is_ascii_alphanumeric());
            if left_ok && right_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

/* ────────────────────────────────────────────────────────── anthropic backend */

const ANTHROPIC_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

#[derive(Debug, Deserialize)]
struct AnthropicResponse {
    content: Vec<AnthropicContentBlock>,
}

#[derive(Debug, Deserialize)]
struct AnthropicContentBlock {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: String,
}

async fn resolve_anthropic(prompt: &str) -> Result<StructuredIntent> {
    let api_key = std::env::var("ANTHROPIC_API_KEY")
        .context("ANTHROPIC_API_KEY is unset; required for --provider anthropic")?;
    let model = std::env::var("PREDICT_AGENT_MODEL").unwrap_or_else(|_| "claude-sonnet-4-6".into());

    let system = SYSTEM_PROMPT;
    let body = serde_json::json!({
        "model": model,
        "max_tokens": 512,
        "system": system,
        "messages": [{
            "role": "user",
            "content": prompt,
        }],
    });

    let client = reqwest::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .build()?;

    let res: AnthropicResponse = client
        .post(ANTHROPIC_URL)
        .header("x-api-key", api_key)
        .header("anthropic-version", ANTHROPIC_VERSION)
        .header("content-type", "application/json")
        .json(&body)
        .send()
        .await
        .context("calling Anthropic API")?
        .error_for_status()
        .context("Anthropic API returned an error status")?
        .json()
        .await
        .context("parsing Anthropic response as JSON")?;

    let raw_text = res
        .content
        .iter()
        .filter(|b| b.kind == "text")
        .map(|b| b.text.as_str())
        .collect::<Vec<_>>()
        .join("");

    parse_intent_json(&raw_text)
}

const SYSTEM_PROMPT: &str = "You are a strict intent parser for a prediction-market CLI.\n\
The user will state a directional view in natural language. You return ONLY a JSON object \
matching this schema, with no surrounding prose, no markdown fences, no commentary:\n\
\n\
{\n\
  \"side\": \"up\" | \"down\" | \"range\",\n\
  \"asset\": \"BTC\" | \"ETH\" | \"SUI\",\n\
  \"tenor_minutes\": <integer between 5 and 1440>,\n\
  \"risk_usdc\": <number > 0, the user's stated dollar risk>,\n\
  \"tag\": <optional string id, omit if not stated>,\n\
  \"rationale\": <one-sentence summary of the user's view>\n\
}\n\
\n\
Map common phrasings: long/bullish/UP/above → \"up\"; short/bearish/DOWN/below → \"down\"; \
range/between/stays → \"range\". If the user does not state a tenor, default to 60. If the user \
does not state an asset, default to \"BTC\". If the user does not state a risk amount, REFUSE \
by responding with {\"error\": \"missing risk amount\"}. Never invent oracle ids or strikes.";

/// Parse the model's textual reply as a [`StructuredIntent`]. Tolerates the model
/// wrapping JSON in ```json ... ``` fences even though the prompt forbids it.
pub(crate) fn parse_intent_json(text: &str) -> Result<StructuredIntent> {
    let cleaned = strip_fences(text.trim());

    #[derive(Deserialize)]
    struct Raw {
        #[serde(default)]
        side: Option<String>,
        #[serde(default)]
        asset: Option<String>,
        #[serde(default)]
        tenor_minutes: Option<u64>,
        #[serde(default)]
        risk_usdc: Option<f64>,
        #[serde(default)]
        tag: Option<String>,
        #[serde(default)]
        error: Option<String>,
    }
    let raw: Raw = serde_json::from_str(cleaned)
        .with_context(|| format!("model did not return valid JSON. raw: {text}"))?;

    if let Some(err) = raw.error {
        bail!("model declined to plan: {err}");
    }

    let side = match raw.side.as_deref() {
        Some(s) => match s.to_ascii_lowercase().as_str() {
            "up" | "long" => IntentSide::Up,
            "down" | "short" => IntentSide::Down,
            "range" => IntentSide::Range,
            other => bail!("model returned unknown side `{other}`"),
        },
        None => bail!("model omitted `side`"),
    };

    let asset = raw.asset.unwrap_or_else(|| "BTC".into()).to_uppercase();
    let tenor_minutes = raw.tenor_minutes.unwrap_or(60);
    let risk_usdc = raw
        .risk_usdc
        .ok_or_else(|| anyhow!("model omitted `risk_usdc`"))?;

    Ok(StructuredIntent {
        side,
        asset,
        tenor_minutes,
        risk_usdc,
        tag: raw.tag,
        rolling: RollingPolicy::None,
    })
}

fn strip_fences(s: &str) -> &str {
    let s = s.trim();
    let stripped = s
        .strip_prefix("```json")
        .or_else(|| s.strip_prefix("```"))
        .unwrap_or(s);
    let stripped = stripped.strip_suffix("```").unwrap_or(stripped);
    stripped.trim()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn regex_parses_long_btc_50_1h() {
        let i = parse_regex("long BTC for 1h, $50").unwrap();
        assert_eq!(i.side, IntentSide::Up);
        assert_eq!(i.asset, "BTC");
        assert_eq!(i.tenor_minutes, 60);
        assert!((i.risk_usdc - 50.0).abs() < 1e-9);
    }

    #[test]
    fn regex_parses_short_eth_30m_25usdc() {
        let i = parse_regex("short eth 30m 25 usdc").unwrap();
        assert_eq!(i.side, IntentSide::Down);
        assert_eq!(i.asset, "ETH");
        assert_eq!(i.tenor_minutes, 30);
        assert!((i.risk_usdc - 25.0).abs() < 1e-9);
    }

    #[test]
    fn regex_parses_range_btc_90m_dollars() {
        let i = parse_regex("BTC range for 90 minutes, 10 dollars").unwrap();
        assert_eq!(i.side, IntentSide::Range);
        assert_eq!(i.tenor_minutes, 90);
        assert!((i.risk_usdc - 10.0).abs() < 1e-9);
    }

    #[test]
    fn regex_picks_up_phrasings() {
        assert_eq!(parse_regex("bullish BTC $50").unwrap().side, IntentSide::Up);
        assert_eq!(
            parse_regex("BTC stays between 80k and 90k for 1h, $20")
                .unwrap()
                .side,
            IntentSide::Range
        );
    }

    #[test]
    fn regex_defaults_tenor_to_one_hour() {
        let i = parse_regex("long btc $5").unwrap();
        assert_eq!(i.tenor_minutes, 60);
    }

    #[test]
    fn regex_rejects_missing_risk() {
        assert!(parse_regex("long btc 1h").is_err());
    }

    #[test]
    fn regex_rejects_missing_asset() {
        assert!(parse_regex("long $50 1h").is_err());
    }

    #[test]
    fn regex_rejects_missing_direction() {
        assert!(parse_regex("BTC $50 1h").is_err());
    }

    #[test]
    fn regex_picks_tag() {
        let i = parse_regex("long BTC $50 1h tag=morning-bias").unwrap();
        assert_eq!(i.tag.as_deref(), Some("morning-bias"));
    }

    #[test]
    fn parse_intent_json_round_trips() {
        let s =
            r#"{"side":"up","asset":"BTC","tenor_minutes":45,"risk_usdc":12.5,"rationale":"x"}"#;
        let i = parse_intent_json(s).unwrap();
        assert_eq!(i.side, IntentSide::Up);
        assert_eq!(i.asset, "BTC");
        assert_eq!(i.tenor_minutes, 45);
        assert!((i.risk_usdc - 12.5).abs() < 1e-9);
    }

    #[test]
    fn parse_intent_json_strips_fences() {
        let s = "```json\n{\"side\":\"down\",\"asset\":\"ETH\",\"tenor_minutes\":30,\"risk_usdc\":7}\n```";
        let i = parse_intent_json(s).unwrap();
        assert_eq!(i.side, IntentSide::Down);
        assert_eq!(i.asset, "ETH");
    }

    #[test]
    fn parse_intent_json_propagates_model_error() {
        let s = r#"{"error":"missing risk amount"}"#;
        let err = parse_intent_json(s).unwrap_err().to_string();
        assert!(err.contains("missing risk amount"));
    }

    #[test]
    fn parse_intent_json_rejects_garbage() {
        assert!(parse_intent_json("not json").is_err());
    }

    // Tests below mutate the process-wide PREDICT_AGENT_PROVIDER env var.
    // Cargo's default test runner is multi-threaded, so we serialize them
    // via this mutex to avoid one test's env_setvar racing another's
    // remove_var. Any new env-touching test must take this lock.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn provider_from_str_or_env_picks_explicit_first() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("PREDICT_AGENT_PROVIDER", "anthropic");
        assert_eq!(
            Provider::from_str_or_env(Some("none")).unwrap(),
            Provider::None
        );
        std::env::remove_var("PREDICT_AGENT_PROVIDER");
    }

    #[test]
    fn provider_from_str_or_env_falls_back_to_env() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::set_var("PREDICT_AGENT_PROVIDER", "anthropic");
        assert_eq!(
            Provider::from_str_or_env(None).unwrap(),
            Provider::Anthropic
        );
        std::env::remove_var("PREDICT_AGENT_PROVIDER");
    }

    #[test]
    fn provider_default_is_none() {
        let _g = ENV_LOCK.lock().unwrap();
        std::env::remove_var("PREDICT_AGENT_PROVIDER");
        assert_eq!(Provider::from_str_or_env(None).unwrap(), Provider::None);
    }
}
