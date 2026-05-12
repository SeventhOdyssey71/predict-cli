# Changelog

## [0.3.0] — 2026-05-12

First release where the engine is proven end-to-end on Sui testnet. Live trade:
mint tx `BMaohVzCLnXzazwEpVQYbhnqrH1UbHoG9y76nHjKnBB2`, settled-permissionless-
redeem tx `5ujz89qR4y3Swz5e1nvLkN4NLtpRE552b1xun2EhjuKP`, withdraw-from-manager
tx `DAKwQ1p6UbLozpUR9b7ZqYPQopAYY8YVBS2c2QjuCE5d`.

### Added — agent surface (M6)

- **OpenAI-compatible provider with tool-using iteration.** `predict-cli agent ask` now supports any model behind an OpenAI-compatible chat-completions endpoint. Works with OpenAI, OpenRouter (any model), Groq, Mistral, DeepSeek, xAI/Grok, Together, Ollama, LM Studio, vLLM.
- **Read-only tool surface for the agent.** `list_oracles(asset?)`, `read_oracle(oracle_id)`, `list_my_positions()`. The agent can call these mid-conversation to gather context before producing intent; tools never submit transactions.
- **New CLI flags on `agent ask`:** `--provider {none|anthropic|openai-compat}`, `--base-url`, `--model`, `--api-key-env <VAR>`. Provider precedence: flag > `PREDICT_AGENT_PROVIDER` env var > auto-detect from `OPENAI_*` / `ANTHROPIC_API_KEY` > offline regex.
- **Iteration loop** in `src/agent/openai.rs` with a hard cap (`MAX_ITERATIONS = 6`) so a stuck conversation fails loudly rather than draining tokens.

### Added — manager polish

- **`predict-cli manager --withdraw <amount>`** pulls DUSDC from the manager back into your wallet. Needed to collect winnings after settlement; without it, redeemed funds sit in the manager forever.
- **Correct manager balance display.** `predict-cli manager` and the spend preview in `mint` now read DUSDC sitting inside the BalanceManager's dynamic-field table directly (not via `suix_getBalance`, which only counts Coin objects and always reported 0).

### Fixed — first-live-trade bugs

- **`agent` planner reads the right spot field.** `intent::read_spot` walked `data.content.fields.spot`; the oracle stores spot at `data.content.fields.prices.fields.spot`. Every `agent open` against a live oracle previously aborted with `spot = 0`.
- **Mint and redeem quantity is microDUSDC (1e6), not 1e9.** `quote_mint_amounts` in the on-chain contract returns `(principal, fee)` in DUSDC-native units; the CLI was scaling `--qty` at 1e9 so every mint asked for 1000× the user's intended amount and aborted at `EBalanceManagerBalanceTooLow`. Fixed in all four trade paths (mint_binary, mint_range, redeem_binary, redeem_range).
- **Planner deposits 100% of risk, not 70%.** The earlier "30% headroom" was never deposited and routinely tripped the same error. Deposit now equals max_cost; quantity uses a 0.6 divisor to leave room for the utilization fee.

### Changed

- `Provider` enum carries an `OpenAICompatConfig` for the openai-compat variant. The system prompt was extracted to `src/agent/system_prompt.txt` and is now shared between the Anthropic and openai-compat backends.

### Internal

- 5 new tests (70/70 passing). The model still produces only `StructuredIntent` JSON; the local planner builds the `Plan` and `Plan::validate` gates it before any tx. That boundary is unchanged.

## [0.2.0] — feat/predict-agentic-cli

The agentic-perps surface, M1 through M5. See [`docs/agentic-perps.md`](docs/agentic-perps.md) for the design and [`docs/mobile-companion.md`](docs/mobile-companion.md) for the gamified mobile companion that pairs with this engine.

### Added

- `predict-cli agent open --side --asset --tenor --risk` — structured intent → validated `Plan` → executed via the existing trade commands. Plan-then-confirm UX; `--yes` skips for scripting.
- `predict-cli agent ask "<prompt>"` — natural-language intent. Default backend is the offline regex parser (no API key); `--provider anthropic` calls Claude. Same `Plan` validation runs locally either way.
- `predict-cli agent positions / close <id> / inspect <id>` — list, owner-redeem, and inspect managed positions. Backed by an atomic JSON store under `$XDG_CONFIG_HOME/predict-cli/positions.json` (or `PREDICT_CLI_STORE` for tests).
- `predict-cli agent watch [--interval] [--once] [--only id]` — settlement daemon. Polls predict-server for oracle status, redeems matured legs per each plan's `exit_policy`, transitions positions to `Settled` when no further work fits.
- Auto-roll: when a leg's `RollingPolicy` is `AutoOnSettlement`, the daemon plans + submits a fresh leg into the next active oracle for the same asset, bounded by remaining budget and tenor. Strike snaps to current spot; band width preserved for ranges.
- `--notify-webhook <url>` — POST each `CycleEvent` as JSON to a URL (Discord/Telegram/Slack incoming webhooks, custom servers, ntfy.sh).
- `--notify-cmd <shell>` — run a shell command per event with `EVENT_KIND`, `POSITION_ID`, `LEG_INDEX`, `DETAIL` injected as env vars. Useful for `osascript`, `notify-send`, etc.
- `docs/agentic-perps.md`, `docs/mobile-companion.md` — design documents covering the engine and the gamified mobile companion.

### Changed

- `Cargo.toml` version bumped to `0.2.0`.
- `agentic-perps.md` roadmap updated: M0–M5 marked complete.

### Internal

- `src/agent/{mod,plan,store,intent,exec,roll,watch,llm}.rs` — eight new modules, 1 800+ lines of Rust, 65/65 tests passing on stable.
- All Plan invariants enforced locally by `Plan::validate` before any tx. The LLM never produces `Plan` JSON directly — it produces the lower-trust `StructuredIntent` which the local planner translates.

## [0.1.0] — initial release

Read paths (`config`, `list`, `oracle`, `vault`, `quote`), manager + writes (`deposit`, `mint`, `mint-range`, `redeem`, `redeem-range`, `supply`, `withdraw`), `doctor` onboarding, friendly aliases (`buy-binary`, `add-liquidity`, …), and a tag-driven release pipeline.
