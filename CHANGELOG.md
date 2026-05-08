# Changelog

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
