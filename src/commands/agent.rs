//! `predict-cli agent` subcommand surface.
//!
//! M1 only exposes structured-DSL commands: open / positions / close / inspect.
//! M4 will add `agent ask "..."` for natural-language intent.

use anyhow::{anyhow, bail, Result};
use chrono::Utc;
use owo_colors::OwoColorize;

use std::time::Duration;

use crate::agent::exec;
use crate::agent::intent::{intent_to_plan, IntentSide, StructuredIntent};
use crate::agent::llm::{self, Provider};
use crate::agent::plan::RollingPolicy;
use crate::agent::store::{PositionStatus, Store};
use crate::agent::watch::{self, WatchConfig};
use crate::format::label;

#[derive(Debug, Clone)]
pub struct OpenArgs {
    pub side: String,
    pub asset: String,
    pub tenor: String,
    pub risk: f64,
    pub tag: Option<String>,
    pub rolling: String,
    pub yes: bool,
}

pub async fn open(args: OpenArgs, json: bool) -> Result<()> {
    let intent = StructuredIntent {
        side: parse_side(&args.side)?,
        asset: args.asset.to_uppercase(),
        tenor_minutes: parse_tenor(&args.tenor)?,
        risk_usdc: args.risk,
        tag: args.tag,
        rolling: parse_rolling(&args.rolling)?,
    };

    let mut store = Store::load()?;
    if let Some(tag) = intent.tag.as_deref() {
        if store.id_in_use(tag) {
            bail!(
                "tag `{tag}` is already in use. Pick another, or close it first with \
                 `predict-cli agent close {tag}`."
            );
        }
    }

    let plan = intent_to_plan(&intent).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(());
    }

    print_plan_summary(&plan);

    if !args.yes && !confirm()? {
        println!("aborted.");
        return Ok(());
    }

    let position = exec::execute(plan).await?;
    store.upsert(position.clone());
    store.save()?;

    println!();
    println!("{} {}", "✓".green(), "position opened".bold());
    println!("  id    {}", position.id);
    println!("  view  {}", position.plan.directional_view);
    Ok(())
}

pub async fn positions(json: bool) -> Result<()> {
    let store = Store::load()?;

    if json {
        println!("{}", serde_json::to_string_pretty(&store.positions)?);
        return Ok(());
    }

    if store.positions.is_empty() {
        println!("no positions yet. open one with `predict-cli agent open`.");
        return Ok(());
    }

    println!("{}", "Positions".bold());
    println!(
        "  {:<14} {:<8} {:<10} {}",
        "id".dimmed(),
        "status".dimmed(),
        "spend".dimmed(),
        "view".dimmed()
    );
    for p in &store.positions {
        let status = format_status(p.status);
        let spend = format!("${:.2}", p.plan.total_declared_deposit());
        println!(
            "  {:<14} {:<8} {:<10} {}",
            truncate(&p.id, 14),
            status,
            spend,
            p.plan.directional_view
        );
    }
    Ok(())
}

pub async fn close(id: &str, json: bool) -> Result<()> {
    let mut store = Store::load()?;
    let mut pos = store
        .find(id)
        .cloned()
        .ok_or_else(|| anyhow!("no position with id `{id}`"))?;

    if matches!(
        pos.status,
        PositionStatus::ClosedByUser | PositionStatus::Settled
    ) {
        bail!("position {id} is already {:?}", pos.status);
    }

    exec::close(&mut pos).await?;
    store.upsert(pos.clone());
    store.save()?;

    if json {
        println!("{}", serde_json::to_string_pretty(&pos)?);
    } else {
        println!();
        println!("{} closed {}", "✓".green(), id);
    }
    Ok(())
}

#[derive(Debug, Clone)]
pub struct AskArgs {
    pub prompt: String,
    pub provider: Option<String>,
    pub yes: bool,
    pub rolling: String,
}

pub async fn ask(args: AskArgs, json: bool) -> Result<()> {
    let provider = Provider::from_str_or_env(args.provider.as_deref())?;

    if !json {
        println!("{}", "agent ask".bold());
        println!("  provider  {}", provider.label());
        println!("  prompt    {}", args.prompt);
        println!();
    }

    let mut intent = llm::resolve(&args.prompt, provider).await?;
    intent.rolling = parse_rolling(&args.rolling)?;

    let mut store = Store::load()?;
    if let Some(tag) = intent.tag.as_deref() {
        if store.id_in_use(tag) {
            bail!(
                "tag `{tag}` is already in use. Pick another, or close it first with \
                 `predict-cli agent close {tag}`."
            );
        }
    }

    let plan = intent_to_plan(&intent).await?;

    if json {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(());
    }

    print_plan_summary(&plan);

    if !args.yes && !confirm()? {
        println!("aborted.");
        return Ok(());
    }

    let position = exec::execute(plan).await?;
    store.upsert(position.clone());
    store.save()?;

    println!();
    println!("{} {}", "✓".green(), "position opened".bold());
    println!("  id    {}", position.id);
    println!("  view  {}", position.plan.directional_view);
    Ok(())
}

#[derive(Debug, Clone)]
pub struct WatchArgs {
    pub interval: u64,
    pub once: bool,
    pub only: Option<String>,
    pub notify_webhook: Option<String>,
    pub notify_cmd: Option<String>,
}

pub async fn watch(args: WatchArgs, json: bool) -> Result<()> {
    let cfg = WatchConfig {
        interval: Duration::from_secs(args.interval.max(1)),
        once: args.once,
        only: args.only,
        json,
        notify_webhook: args.notify_webhook,
        notify_cmd: args.notify_cmd,
    };
    watch::run(cfg).await
}

pub async fn inspect(id: &str) -> Result<()> {
    let store = Store::load()?;
    let pos = store
        .find(id)
        .ok_or_else(|| anyhow!("no position with id `{id}`"))?;

    println!("{} {}", "Position".bold(), pos.id);
    println!("  {} {:?}", label("status"), pos.status);
    println!(
        "  {} {}",
        label("opened"),
        pos.opened_at.format("%Y-%m-%d %H:%M:%S UTC")
    );
    if let Some(closed) = pos.closed_at {
        println!(
            "  {} {}",
            label("closed"),
            closed.format("%Y-%m-%d %H:%M:%S UTC")
        );
    }
    let elapsed = (Utc::now() - pos.opened_at).num_minutes();
    println!("  {} {}m elapsed", label("age"), elapsed);
    println!();
    println!("  {}", "plan".bold());
    println!("  {} {}", label("view"), pos.plan.directional_view);
    println!("  {} {}", label("rationale"), pos.plan.rationale);
    println!(
        "  {} ${:.2} (max), {}m tenor",
        label("budget"),
        pos.plan.max_total_spend_usdc,
        pos.plan.max_total_tenor_minutes
    );
    println!();
    println!("  {}", "legs".bold());
    for (i, leg) in pos.plan.legs.iter().enumerate() {
        let state = pos.legs.get(i).map(|l| l.status.as_str()).unwrap_or("?");
        println!("    [{i}] {} ({state})", describe_leg(leg));
    }

    Ok(())
}

fn describe_leg(leg: &crate::agent::plan::Leg) -> String {
    use crate::agent::plan::Leg::*;
    match leg {
        MintBinary {
            oracle_id,
            strike,
            side,
            quantity,
            deposit,
            ..
        } => format!(
            "mint binary {} ${strike:.0} qty={quantity:.2} deposit=${deposit:.2} on {}",
            if side.is_up() { "UP" } else { "DOWN" },
            crate::format::shorten(oracle_id)
        ),
        MintRange {
            oracle_id,
            lower,
            upper,
            quantity,
            deposit,
            ..
        } => format!(
            "mint range ${lower:.0}–${upper:.0} qty={quantity:.2} deposit=${deposit:.2} on {}",
            crate::format::shorten(oracle_id)
        ),
    }
}

fn print_plan_summary(plan: &crate::agent::plan::Plan) {
    println!("{}", "Plan".bold());
    println!("  intent     {}", plan.intent_id);
    println!("  view       {}", plan.directional_view);
    println!("  legs       {}", plan.legs.len());
    println!(
        "  spend cap  ${:.2}  ({} per leg, max)",
        plan.max_total_spend_usdc,
        plan.legs.len()
    );
    println!("  worst case -${:.2}", plan.max_total_spend_usdc);
    println!();
    for (i, leg) in plan.legs.iter().enumerate() {
        println!("  [{i}] {}", describe_leg(leg));
    }
    println!();
}

fn confirm() -> Result<bool> {
    use std::io::{self, Write};
    print!("Confirm? [y/N] ");
    io::stdout().flush().ok();
    let mut s = String::new();
    io::stdin().read_line(&mut s)?;
    Ok(matches!(
        s.trim().to_ascii_lowercase().as_str(),
        "y" | "yes"
    ))
}

fn parse_side(s: &str) -> Result<IntentSide> {
    match s.to_ascii_lowercase().as_str() {
        "up" | "long" => Ok(IntentSide::Up),
        "down" | "short" => Ok(IntentSide::Down),
        "range" => Ok(IntentSide::Range),
        other => bail!("unknown --side `{other}` (expected up/down/range)"),
    }
}

fn parse_rolling(s: &str) -> Result<RollingPolicy> {
    match s.to_ascii_lowercase().as_str() {
        "none" | "off" | "false" => Ok(RollingPolicy::None),
        "auto" | "on" | "true" => Ok(RollingPolicy::AutoOnSettlement),
        other => bail!("unknown --rolling `{other}` (expected none/auto)"),
    }
}

/// Parse "30m", "1h", "90m" into minutes.
fn parse_tenor(s: &str) -> Result<u64> {
    let s = s.trim().to_ascii_lowercase();
    if let Some(rest) = s.strip_suffix('h') {
        let h: u64 = rest.parse().map_err(|_| anyhow!("invalid hours: {s}"))?;
        Ok(h.checked_mul(60).ok_or_else(|| anyhow!("tenor overflow"))?)
    } else if let Some(rest) = s.strip_suffix('m') {
        rest.parse().map_err(|_| anyhow!("invalid minutes: {s}"))
    } else {
        s.parse()
            .map_err(|_| anyhow!("expected `<n>m` or `<n>h`, got `{s}`"))
    }
}

fn format_status(s: PositionStatus) -> String {
    match s {
        PositionStatus::Pending => "pending".yellow().to_string(),
        PositionStatus::Open => "open".green().to_string(),
        PositionStatus::ClosedByUser => "closed".dimmed().to_string(),
        PositionStatus::Settled => "settled".cyan().to_string(),
        PositionStatus::Failed => "failed".red().to_string(),
    }
}

fn truncate(s: &str, width: usize) -> String {
    if s.chars().count() <= width {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(width.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tenor_parses_minutes_and_hours() {
        assert_eq!(parse_tenor("30m").unwrap(), 30);
        assert_eq!(parse_tenor("1h").unwrap(), 60);
        assert_eq!(parse_tenor("2h").unwrap(), 120);
        assert_eq!(parse_tenor("45").unwrap(), 45);
        assert!(parse_tenor("abc").is_err());
        assert!(parse_tenor("").is_err());
    }

    #[test]
    fn side_aliases() {
        assert_eq!(parse_side("up").unwrap(), IntentSide::Up);
        assert_eq!(parse_side("LONG").unwrap(), IntentSide::Up);
        assert_eq!(parse_side("down").unwrap(), IntentSide::Down);
        assert_eq!(parse_side("short").unwrap(), IntentSide::Down);
        assert_eq!(parse_side("range").unwrap(), IntentSide::Range);
        assert!(parse_side("sideways").is_err());
    }

    #[test]
    fn truncate_respects_width() {
        assert_eq!(truncate("short", 8), "short");
        assert_eq!(truncate("abcdefghij", 5), "abcd…");
    }
}
