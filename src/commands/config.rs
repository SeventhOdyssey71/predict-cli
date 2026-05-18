//! `predict-cli config` — print the testnet configuration we're targeting.

use anyhow::Result;
use owo_colors::OwoColorize;

use crate::config;
use crate::format::label;

pub async fn run(json: bool) -> Result<()> {
    if json {
        let v = serde_json::json!({
            "network": config::NETWORK,
            "v2_deploy_pending": config::is_v2_deploy_pending(),
            "rpc_url": config::rpc_url(),
            "predict_server": config::predict_server(),
            "package": config::PREDICT_PACKAGE,
            "registry": config::PREDICT_REGISTRY,
            "protocol_config": config::PROTOCOL_CONFIG,
            "pool_vault": config::POOL_VAULT,
            "pyth_source_btc": config::PYTH_SOURCE_BTC,
            "pyth_source_eth": config::PYTH_SOURCE_ETH,
            "pyth_source_sui": config::PYTH_SOURCE_SUI,
            "quote_type": config::QUOTE_TYPE,
            "plp_type": config::plp_type(),
            "dusdc_currency": config::DUSDC_CURRENCY_ID,
            "clock": config::CLOCK_ID,
            "float_scaling": config::FLOAT_SCALING.to_string(),
            "quote_decimals": config::QUOTE_DECIMALS,
        });
        println!("{}", serde_json::to_string_pretty(&v)?);
        return Ok(());
    }

    println!("{}", "DeepBook Predict — testnet config (v2)".bold());
    if config::is_v2_deploy_pending() {
        println!(
            "  {} predict v2 deploy is pending; some IDs are placeholders.",
            "note".yellow()
        );
    }
    println!();
    let kv = |k: &str, v: &str| println!("  {} {}", label(&format!("{:<20}", k)), v);
    kv("network", config::NETWORK);
    kv("rpc", &config::rpc_url());
    kv("predict-server", &config::predict_server());
    println!();
    kv("package", config::PREDICT_PACKAGE);
    kv("registry", config::PREDICT_REGISTRY);
    kv("protocol config", config::PROTOCOL_CONFIG);
    kv("pool vault", config::POOL_VAULT);
    kv("pyth btc", config::PYTH_SOURCE_BTC);
    kv("pyth eth", config::PYTH_SOURCE_ETH);
    kv("pyth sui", config::PYTH_SOURCE_SUI);
    kv("quote type", config::QUOTE_TYPE);
    kv("plp type", &config::plp_type());
    kv("dusdc currency", config::DUSDC_CURRENCY_ID);
    println!();
    kv("float scaling", &config::FLOAT_SCALING.to_string());
    kv("quote decimals", &config::QUOTE_DECIMALS.to_string());
    Ok(())
}
