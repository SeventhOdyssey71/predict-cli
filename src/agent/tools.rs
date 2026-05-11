//! Read-only tools the agent can call mid-conversation.
//!
//! Every tool is side-effect free with respect to the chain: it can read the
//! predict-server, the on-chain oracle state, and the local position store, but
//! it never submits a transaction. The model's final output is still a
//! [`StructuredIntent`] (asset/side/tenor/risk) — never a `Plan`. The local
//! planner produces the `Plan` and the validator gates it before any tx.
//!
//! Each tool exposes:
//!   - a name (`list_oracles`, etc),
//!   - a JSON-Schema description for the LLM,
//!   - an async executor that returns JSON.

use anyhow::{anyhow, Result};
use serde::Deserialize;
use serde_json::{json, Value};

use crate::agent::store::Store;
use crate::server;

/// JSON-Schema-style tool definitions for the OpenAI / OpenRouter / etc tools
/// array. The shape matches OpenAI's `tools[].function` field.
pub fn definitions() -> Vec<Value> {
    vec![
        json!({
            "type": "function",
            "function": {
                "name": "list_oracles",
                "description":
                    "List active DeepBook Predict oracles (rolling expiries). \
                     Returns each oracle's id, underlying asset, status, expiry \
                     (ms since epoch), and tick size. Use this when the user \
                     mentions a market or asset and you need to know what's live.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "asset": {
                            "type": "string",
                            "description":
                                "Optional asset filter (e.g. BTC, ETH, SUI). \
                                 Case-insensitive. Omit to return all active oracles.",
                        },
                    },
                    "additionalProperties": false,
                },
            },
        }),
        json!({
            "type": "function",
            "function": {
                "name": "read_oracle",
                "description":
                    "Read one oracle's live state. Returns spot price (USD), \
                     status, settlement price if any, expiry, and underlying \
                     asset. Use this when you've already chosen an oracle id \
                     (from list_oracles) and want details before sizing.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "oracle_id": {
                            "type": "string",
                            "description": "Sui object id (0x-prefixed) of the oracle.",
                        },
                    },
                    "required": ["oracle_id"],
                    "additionalProperties": false,
                },
            },
        }),
        json!({
            "type": "function",
            "function": {
                "name": "list_my_positions",
                "description":
                    "List the user's managed positions from the local store. \
                     Returns id, status, view, and how long each has been open. \
                     Use this when the user references an existing position or \
                     when you want to avoid stomping a tag that's already taken.",
                "parameters": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false,
                },
            },
        }),
    ]
}

/// Dispatch a tool call by name. Arguments come in as a JSON object (the
/// `arguments` field from an OpenAI tool call). Returns the result as a JSON
/// string ready to be echoed back to the model.
pub async fn execute(name: &str, arguments: &Value) -> Result<String> {
    let body = match name {
        "list_oracles" => list_oracles(arguments).await?,
        "read_oracle" => read_oracle(arguments).await?,
        "list_my_positions" => list_my_positions().await?,
        other => return Err(anyhow!("unknown tool `{other}`")),
    };
    Ok(serde_json::to_string(&body).unwrap_or_else(|_| "{}".into()))
}

#[derive(Deserialize, Default)]
struct ListOraclesArgs {
    asset: Option<String>,
}

async fn list_oracles(arguments: &Value) -> Result<Value> {
    let args: ListOraclesArgs = serde_json::from_value(arguments.clone()).unwrap_or_default();
    let mut oracles = server::list_oracles().await?;
    let now_ms = chrono::Utc::now().timestamp_millis() as u64;
    oracles.retain(|o| o.status.eq_ignore_ascii_case("active") && o.expiry > now_ms);
    if let Some(asset) = args.asset.as_deref() {
        let u = asset.to_uppercase();
        oracles.retain(|o| o.underlying_asset.to_uppercase() == u);
    }
    let items: Vec<Value> = oracles
        .iter()
        .map(|o| {
            json!({
                "oracle_id": o.oracle_id,
                "asset": o.underlying_asset,
                "status": o.status,
                "expiry_ms": o.expiry,
                "expires_in_minutes": (o.expiry as i64 - now_ms as i64).max(0) / 60_000,
                "tick_size": o.tick_size,
                "min_strike": o.min_strike,
            })
        })
        .collect();
    Ok(json!({ "oracles": items }))
}

#[derive(Deserialize)]
struct ReadOracleArgs {
    oracle_id: String,
}

async fn read_oracle(arguments: &Value) -> Result<Value> {
    let args: ReadOracleArgs = serde_json::from_value(arguments.clone())
        .map_err(|e| anyhow!("read_oracle: arguments did not match schema: {e}"))?;

    let oracles = server::list_oracles().await?;
    let now_ms = chrono::Utc::now().timestamp_millis() as u64;
    let server_view = oracles.iter().find(|o| o.oracle_id == args.oracle_id);

    let spot_usd = crate::agent::intent::read_spot(&args.oracle_id).await.ok();

    let body = match server_view {
        Some(o) => json!({
            "oracle_id": o.oracle_id,
            "asset": o.underlying_asset,
            "status": o.status,
            "expiry_ms": o.expiry,
            "expires_in_minutes": (o.expiry as i64 - now_ms as i64).max(0) / 60_000,
            "settlement_price": o.settlement_price,
            "tick_size": o.tick_size,
            "min_strike": o.min_strike,
            "spot_usd": spot_usd,
        }),
        None => json!({
            "oracle_id": args.oracle_id,
            "error": "oracle not found in predict-server snapshot",
            "spot_usd": spot_usd,
        }),
    };
    Ok(body)
}

async fn list_my_positions() -> Result<Value> {
    let store = Store::load()?;
    let items: Vec<Value> = store
        .positions
        .iter()
        .map(|p| {
            let age_min = (chrono::Utc::now() - p.opened_at).num_minutes().max(0);
            json!({
                "id": p.id,
                "status": format!("{:?}", p.status).to_ascii_lowercase(),
                "view": p.plan.directional_view,
                "budget_usdc": p.plan.max_total_spend_usdc,
                "age_minutes": age_min,
            })
        })
        .collect();
    Ok(json!({ "positions": items, "count": items.len() }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn definitions_have_three_tools() {
        let defs = definitions();
        assert_eq!(defs.len(), 3);
        let names: Vec<&str> = defs
            .iter()
            .map(|d| d["function"]["name"].as_str().unwrap())
            .collect();
        assert!(names.contains(&"list_oracles"));
        assert!(names.contains(&"read_oracle"));
        assert!(names.contains(&"list_my_positions"));
    }

    #[test]
    fn each_definition_has_function_block() {
        for def in definitions() {
            assert_eq!(def["type"], "function");
            assert!(def["function"]["name"].is_string());
            assert!(def["function"]["description"].is_string());
            assert!(def["function"]["parameters"].is_object());
        }
    }

    #[tokio::test]
    async fn execute_rejects_unknown_tool() {
        let result = execute("not_a_tool", &json!({})).await;
        assert!(result.is_err());
    }
}
