//! Wrapper around the local `sui` binary for write-side operations.
//!
//! For read operations we use direct JSON-RPC. For *signing* and *submitting*
//! transactions we shell out to `sui client ptb`, which uses the user's
//! existing keystore at ~/.sui — no env-var key juggling, no BCS encoders.

use anyhow::{anyhow, Context, Result};
use std::process::{Command, Stdio};

const MERGE_GAS_BUDGET: &str = "100000000";

/// Verify that `sui` is on PATH and the active env is testnet.
pub fn check() -> Result<String> {
    let out = Command::new("sui")
        .args(["client", "active-env"])
        .output()
        .context("running `sui client active-env`. Is the Sui CLI installed?")?;
    let env = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if env != "testnet" {
        return Err(anyhow!(
            "active sui env is `{env}`, expected `testnet`. Run: sui client switch --env testnet"
        ));
    }
    Ok(env)
}

/// Get the active sui address.
pub fn active_address() -> Result<String> {
    let out = Command::new("sui")
        .args(["client", "active-address"])
        .output()
        .context("running `sui client active-address`")?;
    let addr = String::from_utf8_lossy(&out.stdout).trim().to_string();
    if !addr.starts_with("0x") {
        return Err(anyhow!("could not parse active address: {addr}"));
    }
    Ok(addr)
}

/// Build a `sui client ptb` invocation and run it. Each arg is appended
/// as-is. Returns stdout on success, stderr+exit code on failure.
pub fn run_ptb(args: Vec<String>, gas_budget: u64) -> Result<String> {
    let mut cmd = Command::new("sui");
    cmd.arg("client").arg("ptb");
    for a in &args {
        cmd.arg(a);
    }
    cmd.arg("--gas-budget").arg(gas_budget.to_string());

    let out = cmd
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("invoking sui client ptb")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(anyhow!(
            "sui client ptb failed (status {:?}):\n{stderr}\n{stdout}",
            out.status.code()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

/// Merge one coin into another through the Sui CLI. The primary coin ID remains stable.
pub fn merge_coin(primary_coin: &str, coin_to_merge: &str) -> Result<String> {
    let out = Command::new("sui")
        .args([
            "client",
            "merge-coin",
            "--primary-coin",
            primary_coin,
            "--coin-to-merge",
            coin_to_merge,
            "--gas-budget",
            MERGE_GAS_BUDGET,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .context("invoking sui client merge-coin")?;

    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let stdout = String::from_utf8_lossy(&out.stdout);
        return Err(anyhow!(
            "sui client merge-coin failed (status {:?}):\n{stderr}\n{stdout}",
            out.status.code()
        ));
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}
