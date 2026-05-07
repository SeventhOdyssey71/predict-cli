//! Thin Sui JSON-RPC client.
//! Just the read-side methods we need: getObject, getBalance, getCoins, queryEvents.

use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::config;

#[derive(Clone)]
pub struct Rpc {
    client: reqwest::Client,
    url: String,
}

impl Rpc {
    pub fn new() -> Self {
        Self {
            client: reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(15))
                .build()
                .unwrap(),
            url: config::rpc_url(),
        }
    }

    async fn call(&self, method: &str, params: Value) -> Result<Value> {
        let body = json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": method,
            "params": params,
        });
        let res: Value = self
            .client
            .post(&self.url)
            .json(&body)
            .send()
            .await?
            .json()
            .await?;
        if let Some(err) = res.get("error") {
            return Err(anyhow!("rpc error: {}", err));
        }
        res.get("result")
            .cloned()
            .ok_or_else(|| anyhow!("rpc: missing result"))
    }

    pub async fn get_object(&self, id: &str) -> Result<Value> {
        self.call(
            "sui_getObject",
            json!([id, { "showContent": true, "showType": true, "showOwner": true }]),
        )
        .await
    }

    pub async fn get_balance(&self, owner: &str, coin_type: Option<&str>) -> Result<u128> {
        let params = match coin_type {
            Some(t) => json!([owner, t]),
            None => json!([owner]),
        };
        let r = self.call("suix_getBalance", params).await?;
        let v = r
            .get("totalBalance")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow!("balance: missing totalBalance"))?;
        Ok(v.parse::<u128>()?)
    }

    /// Fetch all coin objects of `coin_type` owned by `owner`, paginated.
    pub async fn get_all_coins(&self, owner: &str, coin_type: &str) -> Result<Vec<Coin>> {
        let mut cursor: Option<String> = None;
        let mut out: Vec<Coin> = Vec::new();
        loop {
            let cursor_v: Value = match &cursor {
                Some(c) => Value::String(c.clone()),
                None => Value::Null,
            };
            let r = self
                .call("suix_getCoins", json!([owner, coin_type, cursor_v, 100]))
                .await?;
            let data = r
                .get("data")
                .and_then(|d| d.as_array())
                .ok_or_else(|| anyhow!("getCoins: missing data"))?;
            for c in data {
                let bal = c
                    .get("balance")
                    .and_then(|s| s.as_str())
                    .unwrap_or("0")
                    .parse::<u64>()
                    .unwrap_or(0);
                let id = c
                    .get("coinObjectId")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string();
                if !id.is_empty() {
                    out.push(Coin { id, balance: bal });
                }
            }
            let has_next = r
                .get("hasNextPage")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            if !has_next {
                break;
            }
            cursor = r
                .get("nextCursor")
                .and_then(|v| v.as_str())
                .map(String::from);
            if cursor.is_none() {
                break;
            }
        }
        Ok(out)
    }

    /// Select the smallest high-balance set whose aggregate can fund `amount`.
    pub async fn select_coins_for_amount(
        &self,
        owner: &str,
        coin_type: &str,
        amount: u64,
    ) -> Result<Vec<Coin>> {
        let mut coins = self.get_all_coins(owner, coin_type).await?;
        coins.sort_by(|a, b| b.balance.cmp(&a.balance));

        let mut selected = Vec::new();
        let mut total = 0u128;
        for coin in coins {
            total += coin.balance as u128;
            selected.push(coin);
            if total >= amount as u128 {
                return Ok(selected);
            }
        }
        Ok(Vec::new())
    }
}

/// Helper: walk a JSON path and pull a string field, or default.
pub fn pluck<'a>(v: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cur = v;
    for p in path {
        cur = cur.get(p)?;
    }
    Some(cur)
}

/// Parse a u64-as-string (Sui RPC returns numbers as strings).
pub fn u64_str(v: Option<&Value>) -> u64 {
    v.and_then(|x| x.as_str())
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0)
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Coin {
    pub id: String,
    pub balance: u64,
}

/// Decode an Option<u64> Move field as encoded by sui_getObject:
/// either { fields: { vec: [] } } or { fields: { vec: ["123"] } }.
pub fn option_u64(v: &Value) -> Option<u64> {
    let vec = pluck(v, &["fields", "vec"])?.as_array()?;
    let s = vec.first()?.as_str()?;
    s.parse::<u64>().ok()
}
