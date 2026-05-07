//! Predict-server REST client.
//! https://predict-server.testnet.mystenlabs.com — exposes /oracles, /managers.

use anyhow::Result;
use serde::Deserialize;

use crate::config;

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ServerOracle {
    pub predict_id: String,
    pub oracle_id: String,
    pub oracle_cap_id: String,
    pub underlying_asset: String,
    pub expiry: u64,
    pub min_strike: u64,
    pub tick_size: u64,
    pub status: String,
    pub settlement_price: Option<u64>,
    pub settled_at: Option<u64>,
    pub activated_at: Option<u64>,
    pub created_checkpoint: Option<u64>,
}

#[derive(Debug, Clone, Deserialize, serde::Serialize)]
pub struct ServerManager {
    pub manager_id: String,
    pub owner: String,
    pub digest: String,
    pub checkpoint: u64,
    pub checkpoint_timestamp_ms: u64,
}

pub async fn list_oracles() -> Result<Vec<ServerOracle>> {
    let url = format!("{}/oracles", config::predict_server());
    let res = reqwest::get(&url)
        .await?
        .json::<Vec<ServerOracle>>()
        .await?;
    Ok(res)
}

pub async fn list_managers() -> Result<Vec<ServerManager>> {
    let url = format!("{}/managers", config::predict_server());
    let res = reqwest::get(&url)
        .await?
        .json::<Vec<ServerManager>>()
        .await?;
    Ok(res)
}

pub async fn find_manager_for(owner: &str) -> Result<Option<ServerManager>> {
    let all = list_managers().await?;
    Ok(all.into_iter().rev().find(|m| m.owner == owner))
}
