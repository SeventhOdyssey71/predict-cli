//! `predict-cli config` — print the testnet configuration we're targeting.

use anyhow::Result;
use owo_colors::OwoColorize;

use crate::config;
use crate::format::label;

pub async fn run(json: bool) -> Result<()> {
    if json {
        let v = serde_json::json!({
            "network": config::NETWORK,
            "rpc_url": config::rpc_url(),
            "predict_server": config::predict_server(),
            "package": config::PREDICT_PACKAGE,
            "registry": config::PREDICT_REGISTRY,
            "predict_object": config::PREDICT_OBJECT,
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

    println!("{}", "DeepBook Predict — testnet config".bold());
    println!();
    let kv = |k: &str, v: &str| println!("  {} {}", label(&format!("{:<20}", k)), v);
    kv("network", config::NETWORK);
    kv("rpc", &config::rpc_url());
    kv("predict-server", &config::predict_server());
    println!();
    kv("package", config::PREDICT_PACKAGE);
    kv("registry", config::PREDICT_REGISTRY);
    kv("predict object", config::PREDICT_OBJECT);
    kv("quote type", config::QUOTE_TYPE);
    kv("plp type", &config::plp_type());
    kv("dusdc currency", config::DUSDC_CURRENCY_ID);
    println!();
    kv("float scaling", &config::FLOAT_SCALING.to_string());
    kv("quote decimals", &config::QUOTE_DECIMALS.to_string());
    Ok(())
}
