//! Hardcoded testnet configuration for DeepBook Predict.
//! Source: docs.sui.io/onchain-finance/deepbook-predict/contract-information

use std::env;

pub const NETWORK: &str = "testnet";

pub const PREDICT_PACKAGE: &str =
    "0xf5ea2b3749c65d6e56507cc35388719aadb28f9cab873696a2f8687f5c785138";

pub const PREDICT_REGISTRY: &str =
    "0x43af14fed5480c20ff77e2263d5f794c35b9fab7e2212903127062f4fe2a6e64";

pub const PREDICT_OBJECT: &str =
    "0xc8736204d12f0a7277c86388a68bf8a194b0a14c5538ad13f22cbd8e2a38028a";

pub const QUOTE_TYPE: &str =
    "0xe95040085976bfd54a1a07225cd46c8a2b4e8e2b6732f140a0fc49850ba73e1a::dusdc::DUSDC";

pub const DUSDC_CURRENCY_ID: &str =
    "0xf3000dff421833d4bb8ed58fac146d691a3aaba2785aa1989af65a7089ca3e9c";

pub const CLOCK_ID: &str = "0x6";

/// 1e9 — float_scaling from `helper/constants.move`.
pub const FLOAT_SCALING: u128 = 1_000_000_000;

/// DUSDC decimals.
pub const QUOTE_DECIMALS: u32 = 6;

/// u64::MAX — used as the +∞ sentinel for unbounded ranges.
pub const POS_INF: u64 = u64::MAX;
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
