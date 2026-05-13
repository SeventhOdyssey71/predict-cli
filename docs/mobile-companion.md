# Predict — Gamified Mobile Companion

Design notes for the consumer-facing mobile/PWA surface that pairs with `predict-cli agent`.

This is the "anything that surfaces a behavior the canonical pro UI won't" track of the Sui Overflow 2026 brief. The CLI is the engine; this app is the surface most users will actually touch.

## North star

Open the app, tap three times, and you have a live position. Watch it like you'd watch a friend's bet. Get a buzz when it settles. Feel like you're playing, even though under the hood it's a real DeepBook Predict mint, on chain, settling against an SVI surface.

Three principles:

1. **One position = one card.** No options vocabulary on the home screen. Ever.
2. **Settlement is a moment, not a transaction.** Treat the resolution like a slot-machine reveal, because emotionally that is what it is.
3. **Earned visibility.** Streaks, badges, and leaderboards are the only social/share affordances. The app does not ask anyone to post screenshots.

## Stack (suggested)

- **Shell:** Next.js 16 PWA, installable; React Native wrap for iOS/Android later. The PWA is the v0 hackathon ship — it's mobile-feel without app-store friction.
- **Auth:** zkLogin via Enoki (Google sign-in). 30-second onboarding, no seed phrase, sponsored gas for the first N positions.
- **State:** Zustand or Jotai locally; positions hydrated from a small `predict-companion-api` server that wraps `predict-cli agent`.
- **Animations:** Framer Motion for transitions, Lottie for settlement reveal.
- **Push:** PWA web push (Notifications API) for v0; APNs/FCM via React Native later.
- **Charts:** Recharts (P&L sparkline) and a tiny custom canvas component for the settlement reveal.

## Visual identity

Tone is **calm casino**: clean dark canvas, one accent color per position state, generous spacing, no candy-color gradients. The hero image we already shipped (`docs/banner.png`) sets the palette.

| Token | Value | Use |
|-------|-------|-----|
| `bg.0` | `#0B0D10` | App canvas |
| `bg.1` | `#15181D` | Card surface |
| `bg.2` | `#1F232A` | Card hover / pressed |
| `accent.up` | `#3B82F6` | Long / UP positions |
| `accent.down` | `#F472B6` | Short / DOWN positions |
| `accent.win` | `#22C55E` | Won settlement |
| `accent.loss` | `#EF4444` | Lost settlement |
| `text.0` | `#F5F7FA` | Primary text |
| `text.1` | `#9CA3AF` | Secondary text |
| `text.2` | `#525964` | Disabled / hint |

Type: Inter for everything. 28/700 for the hero number on a card, 13/500 for chrome, 16/400 for body. No serifs, no script.

Motion: 200ms `easeOut` for state transitions; settlement reveal uses a 600ms spring. Nothing slower than 600ms anywhere.

## Information architecture

Five tabs, bottom nav:

```
┌──────┬──────┬──────┬──────┬──────┐
│ Home │ Open │ Pulse│ Board│ You  │
└──────┴──────┴──────┴──────┴──────┘
```

- **Home.** Active positions as cards, sorted by time-to-settle. One pinned card if you have a streak in progress.
- **Open.** The intent screen. Voice mic, text input, and three quick-pick lanes.
- **Pulse.** Live oracle/market overview. The SVI surface lives here for the curious; default view is just "BTC, current implied vol, time to next expiry."
- **Board.** Leaderboards and tournaments. Weekly P&L, longest streak, most accurate predictor.
- **You.** Profile, badges, history, settings, withdraw.

## Key screens

### Onboarding (≤30 seconds)

1. Splash with the hero artwork. Single button: "Start playing".
2. zkLogin (Google) → silent address derivation.
3. "Welcome, ___" → ask for first deposit. Three preset chips: $10, $25, $50, plus "I'll do this later". Sponsored gas for the first deposit. A `PredictManager` is created behind the scenes if needed.
4. Drop into Home with a tutorial overlay: "Your first position will show up here. Tap **Open** to make one."

No seed phrase. No "what is a wallet". No gas concepts. Total time-to-first-position target: 90 seconds from app launch.

### Open (intent)

The single most important screen. Three lanes, top to bottom, each with three big chips:

```
ASSET    [ BTC ] [ ETH soon ] [ SUI soon ]
VIEW     [  UP  ] [  DOWN  ] [  RANGE  ]
SIZE     [ $10 ] [ $25 ] [ $50 ]   (long-press for custom)
```

Below the chips, a single line: **"by 14:24 UTC (in 47 minutes)"** — the current rolling expiry. Long-press to switch to a longer tenor.

Bottom: a **Confirm-slide** ("Slide to open") with the worst case ("you can lose at most $10") and the fair-value payout ("$13.86 if BTC > 82,000 by 14:24") rendered as small dimmed text above. Swipe right to confirm; the screen morphs into the new position card.

For users who don't want chips, a microphone icon top-right opens the natural-language input. Speech-to-text on device, then `agent ask "..."` to the backend, returns a plan to confirm.

### Position card

The home-screen unit. Each card is a self-contained widget:

```
┌─────────────────────────────────────────┐
│  long BTC                       ↗ +12%  │
│  $10 → $11.20   (expected $13.86)       │
│  ─────────────────────────────────  ░░  │  (sparkline + countdown)
│  47m 23s  to settlement                 │
└─────────────────────────────────────────┘
```

State changes:
- **Live.** Accent stripe at left edge in `accent.up` / `accent.down`. P&L number color-shifts with value.
- **Settling.** Pulses gently. Countdown freezes 5s before the settlement; then the reveal animation runs.
- **Won.** `accent.win` border, confetti emit from the card center, payout swaps in. "Tap to claim" CTA appears for permissionless redeem (or auto if user opted in).
- **Lost.** `accent.loss` border, no confetti, a "stoic" subtitle ("the ladder cycles. 4 losses → 1 hit covers all 4"). Card collapses to a small history row after 5 seconds.

Tap a card: full-page detail with all underlying legs (the perp-of-binaries decomposition), the digest of every tx, and a "View on SuiVision" link. Power users who want to see the actual mints can drop down to that view.

### Settlement reveal

The single most important moment for retention. When a leg settles:

1. Card pulses for 600ms.
2. The strike line on a tiny rendered chart slides from "in flight" to its settled position.
3. A 1-frame "result bar" snaps into place (green / red).
4. Number tickers count up to the final P&L over 800ms.
5. On a win: 3-second confetti, haptic "win" pattern, ticker sound (mute by default).
6. The result is briefly shareable via system share sheet — image card, no auto-post.

This is fitness-app energy: the ring closes, the medal lands, the user feels something. The CLI can't do this; the app must.

### Pulse (markets)

A simplified Pulse view by default — just the hero stat:

```
   BTC 1h         next expiry in 47m
   spot 82,257     IV  37%
   ──────────────────────────────
   [ open a position on this market ]
```

Tap "more" and the SVI surface appears as a 2D heatmap (strike × expiry). Power users get the full vol surface; everyone else gets a single number.

### Board (leaderboards / tournaments)

Three tabs: **Week**, **All-time**, **Tournaments**.

- **Week:** P&L, win rate, longest active streak. User's row is pinned to the top with their rank.
- **All-time:** lifetime P&L, badge count, longest-ever streak.
- **Tournaments:** time-bounded brackets (e.g. "Friday 4–5pm BTC bracket"). Buy-in fee → vault → split among top N. Shows your bracket and rank.

A leaderboard row is one line: `🏆 #4   sui_user_1234   +$847   12-streak`.

### You (profile)

Profile picture from zkLogin, address, total deposited, total withdrawn, lifetime P&L, badge wall, position history, settings (notifications, auto-redeem, theme), withdraw (which routes to `predict-cli agent close all` semantics under the hood + balance withdraw).

## Game mechanics

### Streaks

- One streak point per "live position closes in profit." Reset on a loss.
- Streak ≥ 3 → home-screen pinned card with a flame icon and the count.
- Streak ≥ 7 → "Hot Streak" badge unlocks; longer flame, slow pulse.
- Streak ≥ 30 → "Legendary" badge; profile background tint.

Streaks are per-position-closed, not per-leg. If the agent is auto-rolling a position, the streak ticks on the position's outcome, not on each underlying leg. (This matters: it keeps the metric tied to the user's perceived bet, not the engine's mechanics.)

### XP and levels

- 1 XP per dollar of notional opened (capped at $500/day to prevent farm).
- 10 XP per closed position.
- 50 XP per badge unlocked.
- Level curve: standard exponential. Level 10 unlocks longer tenors. Level 25 unlocks tournaments. Level 50 unlocks vault strategies (PLP supply, range ladder).

The progressive unlock is intentional — it both teaches the protocol gradually and rewards retention. Level-50 features are the same plan templates power users get out of the box on the CLI.

### Badges

| Badge | Unlock |
|-------|--------|
| `First Position` | open your first |
| `Hot 7` | 7-streak |
| `Hot 30` | 30-streak |
| `Range Master` | 10 winning range positions |
| `Vault Operator` | first PLP supply |
| `Settler` | first permissionless redeem of someone else's position |
| `Pioneer` | open a position in the first hour of mainnet launch |
| `Vol Whisperer` | profitable in 5 different oracles in a week |

Each badge is an on-chain object (a soulbound NFT) so it's actually portable across other Sui apps. The mobile app reads them via dApp-kit; the user can flex on Suiscan or any wallet UI.

### Tournaments

Time-bounded brackets, fixed buy-in, fixed payout schedule. Six users to a bracket. Single-elimination by P&L over the window. Winners take 70/20/10 of the pot minus a 5% protocol fee that goes to PLP. The mechanics need a tiny on-chain object (`Tournament`) tracking entrants and the settlement — out of scope for v0 but a clean follow-up.

### Copy-trading

Public profiles can be **followed**. When a followed user opens a position, your app surfaces it as a card with a "copy this" button (opens the same intent pre-filled). No automatic mirror trading in v0; we want the user in the loop.

### Push notifications

Three categories, opt-in independently:

- **Settlement.** "Your BTC long settled +$3.86." (Default on.)
- **Streak risk.** "Your 7-streak settles in 2 minutes." (Default on.)
- **Tournaments / leaderboards.** "Weekly board closes in 1h, you're rank 6." (Default off.)

## Backend bridge

The mobile app does not run the planner or executor itself. It calls a thin **`predict-companion-api`** that wraps the same `predict-cli agent` engine.

Endpoints (proposal):

```
POST /v1/intent                  → returns Plan (does not execute)
POST /v1/positions               → confirms and executes a Plan
GET  /v1/positions               → list (for Home)
POST /v1/positions/:id/close     → close (redeem all legs)
GET  /v1/leaderboard?window=week → leaderboard data
GET  /v1/oracles/active          → for the Pulse tab
POST /v1/users/me/notifications  → manage push topics
```

The server holds no keys. Signing happens client-side via Enoki (zkLogin) and PTBs are sent through Shinami (sponsored gas). The server's only job is to translate app events to plan JSON and submit through the user's session.

This means: **the mobile app and the CLI both produce the same plan JSON**. A power user can open a position on the CLI and see it on their phone, or vice versa, because the position store is keyed on the user's address.

## Hackathon scope

For a 2–3 week hackathon build, I'd cut to:

- **Tabs:** Home, Open, You only. Pulse and Board are post-hackathon.
- **Mechanics:** Streaks and badges only. No XP/levels, no tournaments, no copy-trading.
- **Auth:** zkLogin only.
- **Position model:** single-leg (one mint, one redeem). No auto-rolling yet — that comes from `agent watch`. v0 user closes manually; settlement reveal still works because permissionless redeem fires server-side.
- **Notifications:** Settlement only.

That's a tight product, demo-able in 90 seconds:

1. Sign in with Google.
2. Tap "Start playing", tap "BTC", tap "UP", tap "$10", slide to confirm.
3. Watch the card live for the duration of a sub-hour expiry.
4. Settlement reveal animation, P&L lands, badge unlocks.

The on-chain pieces are exactly the existing Predict contract calls. The new code is the React shell, the API server, and the mechanics ledger (a small table tracking streaks/badges per address).

## Why this complements the CLI branch

The CLI agent and the mobile app share the **plan format**, the **position store**, and the **executor**. They are two surfaces over one engine. That's the whole point of doing the M1 work in the CLI first: build the engine, then put any number of skins on it. The mobile app is one skin; a Telegram bot is another (~200 lines mapping `/up 70k 15m 100usdc` to the same plan); a watch complication is a third.

When mainnet ships, none of these surfaces have to be rewritten — only the `src/config.rs` IDs swap.
