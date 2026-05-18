//! Hardcoded testnet configuration for DeepBook Predict v2 (DUSDC-backed
//! parallel expiry markets).
//!
//! The v1 monolithic `Predict<Quote>` object is gone. v2 splits state across:
//!   - one Registry (shared)
//!   - one ProtocolConfig (shared)
//!   - one PoolVault (shared, DUSDC-only)
//!   - one PythSource per Pyth Lazer feed (BTC, ETH, SUI, ...)
//!   - one MarketOracle per (asset, expiry) shared object
//!   - one ExpiryMarket per (asset, expiry) shared object
//!
//! At time of writing, the v2 contracts are merged to MystenLabs/deepbookv3
//! `main` but **not deployed**. Every TODO_V2 placeholder below must be
//! filled in once the deploy lands and the new shared object IDs are
//! published. See MIGRATION.md at the predict-cli root for the upgrade plan.

use std::env;

pub const NETWORK: &str = "testnet";

// === v2 package + globally-shared state ===

pub const PREDICT_PACKAGE: &str = "0xTODO_V2_PACKAGE";
pub const PREDICT_REGISTRY: &str = "0xTODO_V2_REGISTRY";
pub const PROTOCOL_CONFIG: &str = "0xTODO_V2_PROTOCOL_CONFIG";
pub const POOL_VAULT: &str = "0xTODO_V2_POOL_VAULT";

// === Per-feed PythSource shared objects ===
// Created by admin via registry::create_pyth_source(feed_id). One per feed.
// Pyth Lazer feed IDs are the u32 channel identifiers Pyth publishes — these
// will be filled in after the admin runs create_pyth_source for each asset.

pub const PYTH_SOURCE_BTC: &str = "0xTODO_V2_PYTH_BTC";
pub const PYTH_SOURCE_ETH: &str = "0xTODO_V2_PYTH_ETH";
pub const PYTH_SOURCE_SUI: &str = "0xTODO_V2_PYTH_SUI";

// === Quote currency (DUSDC) ===
// DUSDC is now hardcoded across all entries. No more generic <T> type-arg.
// We keep the coin type around for `--split-coins` / coin-picking logic.

pub const QUOTE_TYPE: &str =
    "0xe95040085976bfd54a1a07225cd46c8a2b4e8e2b6732f140a0fc49850ba73e1a::dusdc::DUSDC";

pub const DUSDC_CURRENCY_ID: &str =
    "0xf3000dff421833d4bb8ed58fac146d691a3aaba2785aa1989af65a7089ca3e9c";

pub const CLOCK_ID: &str = "0x6";

/// 1e9 — float_scaling from `helper/constants.move`.
pub const FLOAT_SCALING: u128 = 1_000_000_000;

/// DUSDC decimals.
pub const QUOTE_DECIMALS: u32 = 6;

/// `u64::MAX` — the +∞ sentinel used by `expiry_market::range_key`.
pub const POS_INF: u64 = u64::MAX;

/// `0` — the -∞ sentinel used by `expiry_market::range_key`.
pub const NEG_INF: u64 = 0;

pub fn rpc_url() -> String {
    env::var("RPC_URL").unwrap_or_else(|_| "https://fullnode.testnet.sui.io".into())
}

pub fn predict_server() -> String {
    env::var("PREDICT_SERVER")
        .unwrap_or_else(|_| "https://predict-server.testnet.mystenlabs.com".into())
}

pub fn plp_type() -> String {
    format!("{}::plp::PLP", PREDICT_PACKAGE)
}

/// Returns true if the static config still has unresolved v2 placeholders.
/// Commands that hit the network use this to print a friendly error before
/// the RPC fails with `OBJECT_NOT_FOUND`.
pub fn is_v2_deploy_pending() -> bool {
    PREDICT_PACKAGE.starts_with("0xTODO")
        || PREDICT_REGISTRY.starts_with("0xTODO")
        || PROTOCOL_CONFIG.starts_with("0xTODO")
        || POOL_VAULT.starts_with("0xTODO")
}
