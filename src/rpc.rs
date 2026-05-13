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

    /// Read the DUSDC sitting inside a PredictManager's inner BalanceManager.
    /// DUSDC there is held as a `Balance<DUSDC>` inside a dynamic-field table,
    /// not as a `Coin`, so `suix_getBalance(manager_id, …)` returns 0 even
    /// when there's real money in the manager. This walks the table directly.
    ///
    /// `quote_type` is the fully-qualified type, e.g. crate::config::QUOTE_TYPE.
    /// Returns 0 if the table is empty or the DUSDC field doesn't exist yet.
    pub async fn predict_manager_balance(
        &self,
        predict_manager_id: &str,
        quote_type: &str,
    ) -> Result<u64> {
        let mgr = self.get_object(predict_manager_id).await?;

        // The inner BalanceManager's `balances` field is a Sui Table whose
        // dynamic fields key by BalanceKey<T>. The Table's UID lives at:
        let table_id = pluck(
            &mgr,
            &[
                "data",
                "content",
                "fields",
                "balance_manager",
                "fields",
                "balances",
                "fields",
                "id",
                "id",
            ],
        )
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("manager has no balance_manager.balances table"))?;

        let mut cursor: Option<String> = None;
        loop {
            let params = match &cursor {
                Some(c) => json!([table_id, c, 50]),
                None => json!([table_id, Value::Null, 50]),
            };
            let page = self.call("suix_getDynamicFields", params).await?;
            let data = page
                .get("data")
                .and_then(|d| d.as_array())
                .ok_or_else(|| anyhow!("getDynamicFields: missing data"))?;

            for entry in data {
                let object_type = entry
                    .get("objectType")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default();
                // Match the typed Balance<DUSDC> field, e.g.
                //   ...::balance_manager::BalanceKey<...::dusdc::DUSDC>,
                //   ...::balance::Balance<...::dusdc::DUSDC>>
                if object_type.contains("balance::Balance") && object_type.contains(quote_type) {
                    let field_id = entry
                        .get("objectId")
                        .and_then(|s| s.as_str())
                        .ok_or_else(|| anyhow!("dynamic field missing objectId"))?;
                    let field_obj = self.get_object(field_id).await?;
                    // The Balance struct is a single-u64 newtype, so Sui RPC
                    // inlines it as a raw string at `.fields.value` rather
                    // than wrapping it as `{ type, fields: { value: ... } }`.
                    let amount =
                        u64_str(pluck(&field_obj, &["data", "content", "fields", "value"]));
                    return Ok(amount);
                }
            }

            if !page
                .get("hasNextPage")
                .and_then(|v| v.as_bool())
                .unwrap_or(false)
            {
                return Ok(0);
            }
            cursor = page
                .get("nextCursor")
                .and_then(|v| v.as_str())
                .map(String::from);
            if cursor.is_none() {
                return Ok(0);
            }
        }
    }

    /// Query transaction blocks sent by `sender`, descending by checkpoint.
    /// Returns up to `limit` per page; pass `cursor` to continue.
    ///
    /// `options` controls how much per-tx data the RPC includes — we ask for
    /// events + balance changes + input objects so the history renderer can
    /// classify and price each tx without further round-trips.
    pub async fn query_transactions(
        &self,
        sender: &str,
        cursor: Option<&str>,
        limit: u32,
    ) -> Result<Value> {
        let query = json!({
            "filter": { "FromAddress": sender },
            "options": {
                // Events carry all the cost/payout info we need; computing
                // amounts from balance_changes is tempting but the RPC errors
                // on the entire page when any one tx in it has null effects
                // ("unable to derive balance/object changes because effect is
                // empty"). Reading events directly is faster and safer.
                "showInput": false,
                "showEvents": true,
                "showBalanceChanges": false,
                "showEffects": false,
                "showRawInput": false,
                "showObjectChanges": false,
            },
        });
        let cursor_v: Value = match cursor {
            Some(c) => Value::String(c.into()),
            None => Value::Null,
        };
        // suix_queryTransactionBlocks(query, cursor, limit, descending_order)
        self.call(
            "suix_queryTransactionBlocks",
            json!([query, cursor_v, limit, true]),
        )
        .await
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
        coins.sort_by_key(|c| std::cmp::Reverse(c.balance));

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
