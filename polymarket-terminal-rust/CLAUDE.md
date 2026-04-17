# CLAUDE.md — Polymarket Terminal Bot

## Project Overview

High-performance Polymarket trading bot in Rust targeting binary options (Up-Down) markets on BTC, ETH, SOL, XRP. Two strategies:

- **maker** (`src/main.rs`): Passive market-making — places simultaneous GTC limit buy on YES+NO, waits for both fills, merges positions back to USDC, repeats (re-entry support).
- **conviction** (`src/bin/conviction.rs`): Directional entry in final 2 minutes of each slot based on 4 signals: LIQUIDITY, PRICE_MOVE, PANIC, MOMENTUM.

Both strategies have a real-time TUI dashboard (`--tui` flag) built with ratatui + crossterm.

---

## Build & Run

A `Makefile` is provided — run `make` with no args to see all targets.

```bash
make build              # cargo build --release
make build-debug        # cargo build (debug)
make clean              # cargo clean

# Simulation (DRY_RUN=true)
make maker-tui          # Maker MM + TUI
make conviction-tui     # Conviction + TUI
make maker              # Maker MM, plain logs
make conviction         # Conviction, plain logs

# Live (DRY_RUN=false)
make maker-live-tui
make conviction-live-tui
make maker-live
make conviction-live
```

> `DRY_RUN` is overridden by the Makefile target — no need to edit `.env` to switch modes.

**Toolchain**: stable Rust via rustup.

---

## Dev Commands

```bash
make check              # cargo check
make fmt                # cargo fmt
make lint               # cargo clippy
make test               # cargo test
make header-test        # test HMAC header signing
make hmac-test          # validate API authentication
```

---

## Project Structure

```
src/
├── main.rs                  # Maker MM entry — asset loop, re-entry queuing
├── lib.rs                   # Public module re-exports
├── config.rs                # All env-var parsing (Config struct, WalletMode)
├── bin/
│   └── conviction.rs        # Conviction entry — asset loop, market task handoff
├── clob/
│   ├── mod.rs               # CLOB REST client (order place/cancel/status)
│   ├── auth.rs              # L1/L2 EIP-712 signing, HMAC headers
│   └── types.rs             # Order/response structs
├── market/
│   ├── detector.rs          # Gamma API poller — emits new Up-Down markets per asset
│   └── types.rs             # Market struct (condition_id, token IDs, timings)
├── strategy/
│   ├── executor.rs          # Maker MM state machine (wait → entry → fill → merge)
│   ├── conviction.rs        # Signal detection & execution (liquidity/panic/momentum)
│   └── position.rs          # Position state (YES/NO fills, P&L)
├── ws/
│   ├── orderbook.rs         # WS orderbook snapshots/deltas + REST bootstrap fallback
│   ├── price.rs             # Chainlink price history (reference_price() for resolution)
│   └── fills.rs             # Fill event streaming from CLOB WS
├── onchain/
│   └── ctf.rs               # ConditionalTokens contract (merge, redeem, balances)
└── tui/
    └── mod.rs               # TuiState, TuiHandle, TuiLogLayer, ratatui rendering loop
```

---

## Architecture Patterns

### Async concurrency
- `tokio::spawn` per asset for strategy loops, market detector, WS services, TUI
- `mpsc` for detector emissions → strategy tasks
- `broadcast` channels for WS orderbook/price update notifications

### Shared state
- `Arc<DashMap>` — orderbook books, price history (lock-free)
- `Arc<RwLock>` — TUI state
- `Arc<Mutex>` — per-asset running/pending sets (prevent duplicate execution)

### Dual HTTP clients
- `reqwest` (async) — general REST
- `ureq` (sync) — CLOB order placement specifically, because Polymarket API requires **uppercase** HTTP header names (reqwest lowercases them)

### Config flow
Single `Config` struct parsed from `.env` at startup, cloned into all tasks.

---

## Configuration (.env)

Copy `.env.example` to `.env`. Key variables:

| Variable | Purpose |
|----------|---------|
| `PRIVATE_KEY` | Hex private key (with/without 0x) |
| `WALLET_MODE` | `eoa` or `proxy` (Gnosis Safe) |
| `PROXY_WALLET_ADDRESS` | Required if proxy mode |
| `CLOB_API_KEY/SECRET/PASSPHRASE` | Auto-derived from PK if absent |
| `DRY_RUN` | `true` (sim) / `false` (live), **default true** |
| `MAKER_MM_ASSETS` | `btc,eth,sol,xrp` |
| `MAKER_MM_DURATION` | `5m` or `15m` |
| `MAKER_MM_TRADE_SIZE` | Shares per side (min 5) |
| `MAKER_MM_MAX_COMBINED` | Max combined bid (e.g. `0.98` = 2¢ spread) |
| `CONVICTION_STOP_WINDOWS` | UTC ranges to pause conviction (e.g. `19:40-22:40`) |
| `RUST_LOG` | `info` / `debug` / `warn` |

---

## Important Notes

- **Never deploy with `DRY_RUN=false`** without USDC.e on the wallet and POL for gas (Polygon mainnet).
- **Wallet needs USDC.e** for orders, **POL** for gas fees on merge/redeem.
- Maker rebate logic depends on `fee_rate_bps` set in the order — see `clob/mod.rs`.
- Conviction monitors the **last 2 minutes** of each market slot; slot timing is parsed from Gamma API.
- The `staleness guard` in conviction clears price history if orderbook hasn't updated in >10s — prevents false signals on WS reconnect.
- `reference_price()` in `ws/price.rs` matches Chainlink price within ±10s of `resolution_ts`.

---

## Blockchain / On-Chain

- **Network**: Polygon mainnet
- **Contracts**: ConditionalTokens framework (CTF) — `src/onchain/ctf.rs`
  - `mergePositions()` — return YES+NO tokens → USDC.e
  - `redeemPositions()` — claim winning tokens post-resolution
- **Gnosis Safe support**: Routes txs via proxy wallet when `WALLET_MODE=proxy`
- **Alloy** used for all EVM interactions (v0.9)

---

## Utility Binaries

```bash
cargo run --bin header_test   # Test HMAC signing
cargo run --bin hmac_test     # Validate API authentication
```
