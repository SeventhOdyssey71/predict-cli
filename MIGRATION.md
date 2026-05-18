# Predict CLI — Expiry-Market Refactor (v2)

This branch (`predict-cli/expiry-market-refactor`) migrates the CLI from the
monolithic `predict::*` module (v1, currently deployed at
`0xf5ea2b3749...`) to the new DUSDC-backed parallel-expiry architecture that
landed upstream in commits #1001 and #1008.

**Status:** the new Move package is on `origin/main` of `MystenLabs/deepbookv3`
but is **not deployed to testnet** at time of writing. Every object ID below
that's marked `TODO_v2` must be filled in after the deploy lands. Until then,
`cargo check` passes; live calls will fail with `OBJECT_NOT_FOUND`.

## Module map: old → new

| Old | New |
|---|---|
| `predict_predict::predict` (one file, ~1100 LOC) | Gone — split into `expiry_market` + `plp` + `registry` |
| `vault::vault` | Gone — replaced by `plp::PoolVault` |
| `vault::plp` | Moved to top-level `plp` |
| `oracle::OracleSVI` | Renamed to `market_oracle::MarketOracle` |
| `market_key::up` / `down` | **Removed.** Use `expiry_market::range_key(market, l, h)` |
| `range_key::new` (public) | Now `public(package)`. Use `expiry_market::range_key(...)`. |

## New shared objects (the CLI now needs IDs for)

| Object | Cardinality | Owner module | CLI const |
|---|---|---|---|
| `Registry` | 1 globally | `registry` | `PREDICT_REGISTRY` (reused) |
| `AdminCap` | 1, admin-held | `registry` | n/a (admin only) |
| `ProtocolConfig` | 1 globally | `protocol_config` | `PROTOCOL_CONFIG` (new) |
| `PoolVault` | 1 globally (DUSDC) | `plp` | `POOL_VAULT` (new) |
| `PythSource` | 1 per Pyth Lazer feed | `pyth_source` | `PYTH_SOURCE_BTC`, etc. (new) |
| `MarketOracle` | 1 per (asset, expiry) | `market_oracle` | dynamic — per market |
| `ExpiryMarket` | 1 per (asset, expiry) | `expiry_market` | dynamic — per market |
| `Clock` | `0x6` | system | `CLOCK_ID` (reused) |

## Quote currency

DUSDC is hardcoded everywhere. **All type-args dropped from entries.**

- `predict_manager::deposit(self, coin, ctx)` (was `deposit<T>`)
- `predict_manager::withdraw(self, amount, ctx) → Coin<DUSDC>` (was `withdraw<T> → Coin<T>`)
- `expiry_market::mint(...)` — no type-arg
- `plp::supply(...) → Coin<PLP>` — no type-arg

`QUOTE_TYPE` constant is gone. `PLP` type stays for parsing PLP coin objects.

## Entry-by-entry mapping

### `mint_binary` (UP/DOWN at a strike)

**Before:**
```
move-call predict_manager::deposit<QUOTE> (mgr, deposit_coin)
move-call market_key::up|down (oracle, expiry, strike) -> key
move-call predict::mint<QUOTE> (predict, mgr, oracle, key, qty, clock)
```

**After:**
```
move-call predict_manager::deposit (mgr, deposit_coin)
move-call expiry_market::range_key (market, lower, higher) -> key
move-call expiry_market::mint (market, config, mgr, oracle, pyth, key, qty, clock, ctx)
```

For binary positions:
- UP @ strike → `range_key(market, strike, POS_INF)`  (POS_INF = `u64::MAX`)
- DOWN @ strike → `range_key(market, NEG_INF, strike)`  (NEG_INF = `0`)

### `mint_range` (between two strikes)

**Before:**
```
move-call range_key::new (oracle, expiry, lower, upper) -> rkey
move-call predict::mint_range<QUOTE> (predict, mgr, oracle, rkey, qty, clock)
```

**After:**
```
move-call expiry_market::range_key (market, lower, upper) -> rkey
move-call expiry_market::mint (market, config, mgr, oracle, pyth, rkey, qty, clock, ctx)
```

Note: same `expiry_market::mint` entry — range and binary differ only in the key.

### `redeem_binary` / `redeem_range`

**After:**
```
move-call expiry_market::range_key (market, lower, higher) -> key
move-call expiry_market::redeem (market, config, mgr, oracle, pyth, key, qty, clock, ctx)
```

`redeem` is permissioned for live markets (manager must be owner) and
permissionless for settled/compacted markets. The CLI no longer needs a
separate `redeem_permissionless` entry.

### `supply` (LP DUSDC → PLP)

**Before:** single move call.

**After:** multi-step PTB.

```
move-call plp::start_valuation (vault, config) -> valuation

# For each active expiry market in vault.active_expiry_markets:
move-call expiry_market::read_valuation (market_i, config, oracle_i, pyth_i, clock) -> ev_i
move-call plp::add_expiry_valuation (valuation, ev_i)

split-coins funding_coin [amount] -> [supply_coin]
move-call plp::supply (vault, config, valuation, supply_coin, ctx) -> plp_coin
transfer-objects [plp_coin] sender
```

The CLI must read `PoolVault.active_expiry_markets` dynamically and, for each
listed market, also fetch `MarketOracle.pyth_source_id` to chain the right
PythSource object.

### `withdraw` (PLP → DUSDC)

Same valuation prefix as supply.

```
move-call plp::start_valuation (vault, config) -> valuation
# loop read_valuation + add_expiry_valuation per active market
move-call plp::withdraw (vault, config, valuation, plp_coin, ctx) -> dusdc_coin
transfer-objects [dusdc_coin] sender
```

### `create-manager`

**Before:** `predict::create_manager<QUOTE>(predict, ctx)` then share.

**After:** `registry::create_and_share_manager(registry, ctx)` — single entry fn, shares automatically. Or `registry::create_manager(registry, ctx) -> PredictManager` plus `predict_manager::share(mgr)`.

### `manager-withdraw`

**Before:** `predict_manager::withdraw<QUOTE>(mgr, amount, ctx) -> Coin<QUOTE>`

**After:** `predict_manager::withdraw(mgr, amount, ctx) -> Coin<DUSDC>` — no type-arg.

## Data-model reads

| CLI command | v1 source | v2 source |
|---|---|---|
| `oracle <id>` | `OracleSVI` shared object fields | `MarketOracle` + `PythSource` (two object reads) |
| `quote <id>` | `OracleSVI.svi_params`, `OracleSVI.last_spot_price` | `MarketOracle.block_scholes_svi`, `MarketOracle.block_scholes_forward`, `PythSource.spot` |
| `list` | `predict-server /oracles` endpoint | Same endpoint (may now return expiry-market IDs) **or** read `PoolVault.active_expiry_markets` and decode each ExpiryMarket |
| `vault` | `Predict<Quote>` fields | `PoolVault.idle_balance / total_supply / total_allocated_capital / active_expiry_markets` |
| `doctor` | one-shot Predict check | Registry + ProtocolConfig + PoolVault + per-feed PythSource |

## What the CLI does NOT need to call

- `registry::create_pyth_source` — admin only
- `registry::create_expiry_market` — admin only
- `registry::create_market_oracle_cap` — admin only
- Anything taking `&AdminCap`

## Configuration placeholders

`src/config.rs` will introduce these constants — all `TODO_v2` until deploy:

```rust
pub const PREDICT_PACKAGE_V2: &str = "0xTODO_v2_package";
pub const PREDICT_REGISTRY_V2: &str = "0xTODO_v2_registry";
pub const PROTOCOL_CONFIG: &str = "0xTODO_v2_protocol_config";
pub const POOL_VAULT: &str = "0xTODO_v2_pool_vault";

// Per-feed PythSource objects (filled after admin runs create_pyth_source)
pub const PYTH_SOURCE_BTC: &str = "0xTODO_v2_pyth_btc";
pub const PYTH_SOURCE_ETH: &str = "0xTODO_v2_pyth_eth";
pub const PYTH_SOURCE_SUI: &str = "0xTODO_v2_pyth_sui";
```

These migrate to real IDs by:
1. Watching `git log packages/predict` for the deploy commit (Move.lock gains `published-at` for `deepbook_predict`)
2. Reading the deploy tx events for the shared object IDs
3. Or reading them from the new `docs.sui.io/onchain-finance/deepbook-predict/contract-information` page

## Behavioral implications for users

- Existing PLP holders on v1 must **withdraw before mass migration**; v1 PLP is not redeemable on v2.
- Existing PredictManagers on v1 are tied to v1 positions; v2 needs a fresh `create_and_share_manager` call.
- Active v1 positions settle on v1 — they don't move.
- DUSDC custody is now via DeepBook `BalanceManager` (composability win — same custody primitive used by margin).
