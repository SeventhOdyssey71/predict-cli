# Agentic Perps

A design proposal for evolving `predict-cli` from an imperative trading tool into an agent-driven consumer surface for DeepBook Predict.

This doc is a working proposal, not a spec. It's a starting point for the work on the `predict-agentic-cli` branch.

## Background

DeepBook Predict is live on Sui testnet (mainnet planned). It ships things prediction-market venues today don't: every-strike-every-expiry pricing against a live SVI volatility surface, sub-hour rolling oracles, a vault (`PLP`) that takes the other side of every trade, and primitives that compose with `deepbook_margin` and `iron_bank` already on mainnet.

The Sui Overflow 2026 Predict track explicitly asks for **consumer**, **social**, and **chat-based** surfaces — "anything that surfaces a behavior the canonical pro UI won't." Consumers do not think in binaries, ranges, SVI total variance, or `PredictManager` aggregate balances. They think:

> "I'm bullish on BTC."
> "I want to fade this rally."
> "Park $200 and tell me when it's done."

The pro CLI we shipped (`mint`, `mint-range`, `redeem`, `--max-cost`, etc.) is correct and complete, and it stays. What's missing is a translation layer from intent to that surface, plus a managed position abstraction that mimics the only directional-exposure metaphor a retail user already understands: **a perp**.

## What "Agentic Perps" means

A perp is one mental object. You open it, you watch P&L, you close it. Under the hood DeepBook Predict can mint + roll a sequence of short-expiry binaries (or ranges) that approximate that exposure, with each expiry settled and the next opened atomically. The user never sees the rolls; they see one position and a continuous P&L curve.

"Agentic" means the orchestration layer is an LLM-backed agent that:

1. Translates a natural-language or structured intent into a concrete plan (which oracle, strike, side, quantity, expiry budget, rolling policy).
2. Surfaces the plan for user confirmation, with explicit cost and worst-case exposure stated upfront.
3. Executes the plan against the existing `predict-cli` write paths (deposit, mint, mint-range), reusing every safety invariant we already enforce.
4. Maintains the position over time — redeeming on settlement, rolling into the next expiry, optionally hedging — until the user closes it or its budget is spent.
5. Reports P&L and state in a way that reads like a single perp position, not a sequence of options trades.

Three properties make this fit the hackathon's "consumer" framing:

- **One intent, one position.** No options vocabulary required at the surface. The agent owns the translation.
- **Bounded risk by construction.** Every intent has a maximum spend and a maximum tenor. The agent cannot violate those, period.
- **Composable underneath.** The agent emits standard `predict-cli` PTBs. Power users can inspect them, fork them, or take over manually at any time.

## Architecture

```
        ┌──────────────────────────────────────────────────┐
        │  predict-cli agent  (new surface)                │
        │  ┌────────────┐    ┌──────────────────────────┐  │
        │  │  intent    │ →  │  planner (LLM + tools)   │  │
        │  │  parser    │    │  ─ market lookup         │  │
        │  └────────────┘    │  ─ quote / fair value    │  │
        │                    │  ─ size + risk cap calc  │  │
        │                    │  ─ rolling policy        │  │
        │                    └──────────────────────────┘  │
        │                              ↓                   │
        │                    ┌──────────────────────────┐  │
        │                    │  position store (local)  │  │
        │                    │  ~/.config/predict-cli/  │  │
        │                    │     positions.json       │  │
        │                    └──────────────────────────┘  │
        │                              ↓                   │
        │                    ┌──────────────────────────┐  │
        │                    │  executor                │  │
        │                    │  uses existing commands  │  │
        │                    │  (mint, redeem, deposit) │  │
        │                    └──────────────────────────┘  │
        └──────────────────────────────────────────────────┘
                                      ↓
        ┌──────────────────────────────────────────────────┐
        │  predict-cli core (unchanged)                    │
        │  list / oracle / quote / mint / redeem / supply  │
        │  → sui_cli.rs → `sui client ptb` → keystore      │
        └──────────────────────────────────────────────────┘
```

### Layers

**Intent surface.** Two entry points:
- `predict-cli agent ask "<prompt>"` for one-shot ("I want $50 short BTC for the next 30 minutes")
- `predict-cli agent` for an interactive REPL with rolling context

Plus a structured intent DSL for power users and scripting:

```bash
predict-cli agent open --side long --asset BTC --tenor 1h --risk 50usdc --tag morning-bias
predict-cli agent open --view "BTC stays in 80k-84k for the next 90m" --risk 25usdc
predict-cli agent close morning-bias
```

**Planner.** An LLM with a tightly scoped tool surface:
- `list_active_oracles(asset?)` → calls existing `list` command
- `read_oracle(id)` → calls existing `oracle` command
- `quote(oracle, strike, side | lower, upper)` → calls existing `quote`
- `manager_state()` → balance + active positions
- `propose_plan(plan)` → returns the plan to the user for confirmation; does **not** execute

The model never directly invokes a write path. It can only propose plans. The executor runs them.

**Plan format.** A plan is a JSON object the model emits. Strict schema, validated locally before any tx:

```json
{
  "intent_id": "morning-bias",
  "directional_view": "long_BTC",
  "rationale": "User stated bullish bias; mint UP binaries at near-ATM strikes with 1h tenor.",
  "max_total_spend_usdc": 50.0,
  "max_total_tenor_minutes": 60,
  "legs": [
    {
      "kind": "mint_binary",
      "oracle_id": "0xed58…380b",
      "strike": 82000.0,
      "side": "up",
      "quantity": 50.0,
      "deposit": 35.0,
      "max_cost": 40.0,
      "rolling_policy": "auto_on_settlement"
    }
  ],
  "exit_policy": {
    "on_settlement": "redeem_permissionless",
    "on_user_close": "redeem_now",
    "on_budget_exhausted": "stop"
  }
}
```

**Executor.** Iterates the plan's legs and shells through to the existing CLI commands. Every single tx is just `mint` / `mint-range` / `redeem` / `redeem-range` / `deposit` invoked with the plan's parameters. The wallet's spend caps still apply (`--max-cost`, empty-manager-by-default), so a misbehaving plan can't drain funds.

**Position store.** A local JSON file under `~/.config/predict-cli/positions.json` (or `$XDG_CONFIG_HOME` if set). One record per managed position:

```json
{
  "id": "morning-bias",
  "intent": "long BTC, $50, 1h",
  "opened_at": "2026-05-08T13:24:00Z",
  "tenor_ends_at": "2026-05-08T14:24:00Z",
  "budget_usdc": 50.0,
  "spent_usdc": 35.0,
  "active_legs": [
    {"oracle_id": "0xed58…380b", "side": "up", "strike": 82000.0, "qty": 50.0, "tx": "0xabc…"}
  ],
  "history": [...]
}
```

This is what `predict-cli agent positions` reads from. It is the source of truth for the perp abstraction.

**Daemon mode** (`predict-cli agent watch`): a long-running loop that wakes on `OracleSVIUpdated` / settlement events from the predict-server, redeems matured positions, and either rolls (per the plan) or stops. Emits desktop notifications by default; can post to Discord/Telegram via webhook.

## User flows

### Flow 1: Conversational open

```
$ predict-cli agent ask "I think BTC is going to 85k by tonight. $50 risk."

Plan
  view              long BTC, target 85k, expiry today end-of-day
  oracle            BTC 1h rolling (0xed58…380b)
  legs              mint 50 UP @ 82000 strike, $35 deposit, $40 cap
                    auto-roll on settlement until tenor or budget exhausted
  worst case        -$50.00 (full budget loss across rolls)
  fair value        $0.7213 → $13.86 if win this leg

  Confirm? [y/N]
```

User says `y`, the agent shells through `predict-cli mint` with the cap, records the position, returns. From this point on `predict-cli agent positions` shows it as a single line:

```
  morning-bias  long BTC  spent $35 / $50  P&L +$2.40  expires in 47m
```

### Flow 2: Structured intent

For users who don't want to talk to a model, the same workflow with explicit args:

```bash
predict-cli agent open \
  --side long \
  --asset BTC \
  --tenor 1h \
  --risk 50 \
  --strike-policy near-atm \
  --rolling auto
```

This skips the LLM and goes straight to the planner. Same plan format, same executor, same position store.

### Flow 3: Daemon

```bash
predict-cli agent watch &
```

Watches every position in the store. Emits notifications:

```
[14:24] morning-bias settled: leg won $13.86, rolling into next expiry
[15:24] morning-bias settled: leg lost, 1 roll remaining
[16:00] morning-bias closed: budget exhausted, total P&L -$8.30
```

### Flow 4: Power-user inspection

Every plan and tx the agent produced is in the position store with the digest. `predict-cli agent inspect <id>` dumps the JSON. Users can `git`-track the file, replay past intents, or feed them to a backtester.

## Strategy primitives the agent can express

Mapping the hackathon idea bank onto the agent's plan vocabulary:

| Idea | Plan shape |
|---|---|
| Long / short directional | One leg per expiry: `mint_binary` UP / DOWN at near-ATM, auto-roll |
| Range bet ("BTC stays in 80k–84k") | One leg per expiry: `mint_range` lower/upper, auto-roll |
| Range Ladder Vault | Multiple legs at fixed bps offsets: `mint_range` × N around ATM |
| PLP+Hedge | Two parallel positions: one `supply` to PLP, one `mint_binary` OOM as left-tail hedge |
| BTC-collateral Predict | Pre-leg: route BTC → dUSDC via DeepBook spot; then standard plan; settlement-day swap-back |
| Vol-arb (Predict ↔ Polymarket) | Plan + external feed: agent monitors Polymarket smile, opens Predict legs when spread > threshold |
| Settled-redeem keeper | No new positions; just `redeem_permissionless` on every settled oracle the user has un-redeemed exposure to |

The point is: every "strategy" is just a plan in the same JSON shape. New strategies are new plan templates, not new code paths.

## Safety

Three layers, in order of "harder to bypass":

1. **Wallet spend caps (existing).** `--max-cost`, empty-manager-by-default, the auto-merge-then-split coin handling — all of it is still in force. The agent uses the existing CLI commands, so it inherits these for free.
2. **Plan-level caps.** Every plan declares `max_total_spend_usdc` and `max_total_tenor_minutes`. The executor refuses to spend or roll past them.
3. **Plan-then-confirm UX.** The default mode is plan → user confirms → execute. Auto-execute is gated behind `--auto-confirm` and is meaningful only in `agent watch` daemon mode for redeem/roll operations on positions the user already opened with confirmation.

The model does not hold keys, does not call write paths directly, and cannot exceed the user's declared risk envelope. Worst-case "rogue model" outcome: it proposes a bad plan, the user rejects it.

## Implementation outline

Modules to add. Roughly in order:

1. `src/agent/mod.rs`, `src/agent/plan.rs` — the plan schema (`serde` types) and validation (no leg exceeds the budget, expiries are in the future, oracle is mintable, etc.). No LLM yet.
2. `src/agent/store.rs` — position store: load/save `~/.config/predict-cli/positions.json`, lookup by id, append to history. Atomic writes via tmpfile + rename.
3. `src/agent/exec.rs` — executor. Iterates plan legs, calls existing `commands::trade::mint_binary` / `mint_range` / `redeem_*`, records results in the store.
4. `src/commands/agent.rs` — clap subcommand wiring: `agent open`, `agent ask`, `agent positions`, `agent close`, `agent inspect`, `agent watch`.
5. `src/agent/llm.rs` — LLM provider interface. Initial backends: Anthropic (via `claude-sonnet-4-6`) and OpenAI (`gpt-4o-mini`). Pluggable via `PREDICT_AGENT_PROVIDER=anthropic|openai|none`. The `none` backend just dispatches to the structured DSL — useful for CI and for users who don't want a model at all.
6. `src/agent/watch.rs` — daemon mode: poll the predict-server `/oracles` for settlement transitions, react per the position's exit policy.

Tests:

- Plan schema round-trip + validation rejects.
- Executor smoke test using mocked `commands::trade` (a feature-gated test backend that records calls instead of submitting).
- Position store concurrency: two writers don't corrupt the file.
- LLM backend `none` end-to-end: structured `agent open` produces a plan, executor runs it, store contains it.

## Milestones

The branch should evolve in roughly this shape:

- **M0 — design** (this doc). Lock the plan format. Lock the safety story. Decide the file layout.
- **M1 — structured intent only.** `agent open --side --asset --tenor --risk` works end-to-end with no LLM. Position store works. `agent positions` and `agent close` work. This is already shippable for the hackathon.
- **M2 — settlement daemon.** `agent watch` redeems matured positions automatically. No new positions get opened by the daemon yet.
- **M3 — auto-roll.** Daemon can roll a position into the next expiry under the plan's policy, bounded by the budget.
- **M4 — natural-language intent.** `agent ask "..."` with the LLM planner. Same plan format underneath; the model just produces the JSON.
- **M5 — polish.** Notifications, copy-trade, multi-position dashboards, telegram-bot bridge.

Each milestone is a stand-alone product. We do not need M4 to ship something useful.

## What this is *not*

- It is not a managed-funds product. The agent never custodies user funds; everything routes through the user's keystore via `sui client ptb`, identical to today.
- It is not market-making. The vault (PLP) is the market maker. The agent is a price-taker with a managed position book.
- It is not a backtester. A separate tool can replay plans against historical SVI data, but that lives outside this branch.
- It is not a trustless guarantee. The model can produce bad plans; the safety layers exist precisely because we don't trust the model.

## Open questions

- **LLM costs.** A chatty REPL on `claude-sonnet-4-6` is a real cost. Should we cache common intents? Use a smaller model for parsing and reserve the larger one for planning?
- **Local-first vs server-backed.** Position store is local for the v1 because it's the simplest secure thing. For multi-device users we'd want a sync server. Out of scope until we know users want it.
- **Roll vs roll-down.** When a leg loses, does the agent roll the same strike or shift toward ATM? Default policy is "same strike until tenor ends," but this is a knob.
- **Hedge legs.** PLP+Hedge requires `supply` and `mint_binary` in one plan. How do we represent two-side positions in the store and report a single P&L? Probably as a "composite position" wrapping multiple atomic positions; needs more design.
- **Telegram bridge.** The hackathon idea bank lists a Telegram bot prominently. Once `agent open` exists, a thin Telegram adapter is ~200 lines that maps `/up 70k 15m 100usdc` to `agent open`. Worth building separately, not in this CLI.

## Why this fits the brief

The Notion problem statement asks for surfaces that turn Predict from a quant primitive into a piece of crypto market structure that real consumers can use. The agentic perp metaphor is the cheapest way to do that without dumbing down the protocol:

- Consumers get a familiar interface (open / hold / close / P&L).
- The protocol's real superpowers (every strike, every expiry, vol-surface pricing, PLP liquidity) stay reachable through the structured DSL.
- The composability the brief emphasises — `deepbook_margin`, `iron_bank`, structured vaults — sits one plan-template away. A Three-Protocol Margin Loop is just a plan with a pre-leg and a settlement-leg.
- Mainnet day one this same CLI works against mainnet IDs with a config swap; the architecture has no testnet-specific assumptions outside `src/config.rs`.

That's the pitch.
