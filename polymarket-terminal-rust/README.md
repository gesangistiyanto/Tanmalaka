# Polymarket Terminal Rust

A high-performance Polymarket trading bot written in Rust. Includes two strategies:

- **Maker MM** — passive market-making for maker rebates on binary markets
- **Conviction** — directional trading in the final minutes of a market slot using price momentum, liquidity depth, and panic signals

Both strategies feature a real-time TUI dashboard (terminal UI) built with [ratatui](https://github.com/ratatui-org/ratatui).

---

## Table of Contents

- [Strategies](#strategies)
- [Requirements](#requirements)
- [Installation](#installation)
  - [macOS](#macos)
  - [Linux](#linux)
  - [Windows](#windows)
- [Configuration](#configuration)
- [Running](#running)
- [Building for Other Platforms](#building-for-other-platforms)

---

## Strategies

### Maker MM (`maker`)

Places GTC limit buy orders on both the YES and NO sides of a binary market simultaneously. When both sides fill, the combined position merges to USDC, capturing the spread as a maker rebate.

**Flow:**
1. Detect upcoming markets for configured assets
2. Wait for bid prices to fall in the configured range and combined ≤ `MAX_COMBINED`
3. Place limit buy orders on both YES and NO (maker orders, top of book)
4. Monitor fills via on-chain balance checks + WebSocket fill events
5. When both sides fill → merge YES+NO → redeem USDC
6. Handles ghost fills, one-sided fills, cut-loss, and re-entry

### Conviction (`conviction`)

Monitors the final 2 minutes of each market slot and enters a directional position when a high-confidence signal fires. Only one primary entry per market, with an optional second entry (opposite side) if a panic signal fires after the primary position is locked.

**Signals (in priority order):**

| Signal | Trigger | Direction |
|--------|---------|-----------|
| **LIQUIDITY** | YES bid USD > NO bid USD by ≥ 2× at 2-min window open | Follow dominant side |
| **PRICE MOVE** | Chainlink price moved ≥ +0.035% from `resolution_ts` reference | UP if above ref, DOWN if below |
| **PANIC** | Price jumped > 5 ticks in one tick AND previous price < 40c | Fade (buy opposite) |
| **MOMENTUM** | Net price rise ≥ 5 ticks over 30s window AND price 50–70c | Follow momentum |

**Dual entry:** If a primary position is already locked and a PANIC signal fires on the *opposite* side with > 20 seconds remaining, a second entry is taken independently.

**Resolution:** After market close, a background task polls every 30 seconds. In live mode, WIN/LOSS is only confirmed after `redeem_positions` succeeds on-chain. In simulation mode, a strict price threshold (≥ 0.95) is used.

**Staleness guard:** If the WebSocket orderbook has not updated in > 10 seconds (internet lag), price history is cleared and signals are suppressed until fresh data arrives. This prevents false spike signals on reconnect.

---

## Requirements

- **Rust** 1.75 or later (see installation below)
- A Polygon wallet with USDC.e and POL (for gas) — only needed for live mode
- A Polymarket account with an approved wallet

---

## Installation

### macOS

**1. Install Rust**

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

**2. Verify**

```bash
rustc --version   # should print rustc 1.75+
cargo --version
```

**3. Clone and build**

```bash
git clone https://github.com/mulhamna/terminal_polymarket.git
cd terminal_polymarket
make build
```

Binaries will be at `target/release/maker` and `target/release/conviction`.

---

### Linux

**1. Install Rust**

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source "$HOME/.cargo/env"
```

**2. Install system dependencies** (required for TLS and build tools)

Ubuntu / Debian:
```bash
sudo apt update
sudo apt install -y build-essential pkg-config libssl-dev
```

Fedora / RHEL:
```bash
sudo dnf install -y gcc pkg-config openssl-devel
```

Arch:
```bash
sudo pacman -S base-devel pkg-config openssl
```

**3. Clone and build**

```bash
git clone https://github.com/mulhamna/terminal_polymarket.git
cd terminal_polymarket
make build
```

Binaries will be at `target/release/maker` and `target/release/conviction`.

---

### Windows

**1. Install Rust**

Download and run the installer from [rustup.rs](https://rustup.rs).

When prompted, select the default installation. Rustup will also install the required Visual Studio C++ build tools (MSVC toolchain).

> If the MSVC build tools prompt appears, click **Install** and wait for it to complete before continuing.

**2. Clone and build**

Open **PowerShell** or **Command Prompt**:

```powershell
git clone https://github.com/mulhamna/terminal_polymarket.git
cd terminal_polymarket
cargo build --release
```

Binaries will be at `target\release\maker.exe` and `target\release\conviction.exe`.

> **TUI on Windows:** The interactive dashboard requires **Windows Terminal** or **VS Code terminal** for proper rendering. The classic `cmd.exe` may have display issues.

---

## Configuration

Copy `.env.example` to `.env` and fill in your values:

```bash
cp .env.example .env
```

### Required

| Variable | Description |
|----------|-------------|
| `PRIVATE_KEY` | EOA private key (hex, with or without `0x` prefix) |

### Wallet Mode

| Variable | Default | Description |
|----------|---------|-------------|
| `WALLET_MODE` | `eoa` | `eoa` — wallet holds funds directly. `proxy` — EOA signs for a Polymarket Gnosis Safe proxy |
| `PROXY_WALLET_ADDRESS` | — | Required only when `WALLET_MODE=proxy`. Your Polymarket deposit address |

### API Credentials

| Variable | Default | Description |
|----------|---------|-------------|
| `CLOB_API_KEY` | *(auto)* | Leave blank — derived automatically from `PRIVATE_KEY` on first run |
| `CLOB_API_SECRET` | *(auto)* | Same as above |
| `CLOB_API_PASSPHRASE` | *(auto)* | Same as above |

### Endpoints

| Variable | Default | Description |
|----------|---------|-------------|
| `CLOB_HOST` | `https://clob.polymarket.com` | Polymarket CLOB API endpoint |
| `GAMMA_HOST` | `https://gamma-api.polymarket.com` | Polymarket Gamma (market data) API |
| `POLYGON_RPC_URL` | `https://polygon-bor-rpc.publicnode.com` | Polygon RPC for on-chain reads/writes |

### Strategy

| Variable | Default | Description |
|----------|---------|-------------|
| `MAKER_MM_ASSETS` | `btc` | Comma-separated assets: `btc`, `eth`, `sol`, `xrp` |
| `MAKER_MM_DURATION` | `15m` | Slot duration to target: `5m` or `15m` |
| `MAKER_MM_TRADE_SIZE` | `5` | Shares per side (minimum 5 — CLOB enforces) |
| `MAKER_MM_MAX_COMBINED` | `0.98` | Max combined bid (YES+NO) to enter. Spread = `1.00 - MAX_COMBINED` |
| `MAKER_MM_MIN_PRICE` | `0.30` | Minimum bid price on the cheap side to qualify for entry |
| `MAKER_MM_MAX_PRICE` | `0.69` | Maximum bid price on the cheap side to qualify for entry |
| `MAKER_MM_CUT_LOSS_TIME` | `60` | Seconds before market close to force-exit open positions |
| `MAKER_MM_ENTRY_WINDOW` | `45` | Seconds after market open to allow new entries |
| `MAKER_MM_REENTRY_ENABLED` | `true` | Allow re-entry after a completed cycle |
| `MAKER_MM_REENTRY_DELAY` | `30` | Delay between re-entry cycles (seconds) |
| `MAKER_MM_CANCEL_CHEAP_ON_EXP_FILL` | `false` | Cancel cheap side when expensive fills first, hold and redeem |
| `MAKER_MM_POLL_INTERVAL` | `5` | Market detector poll interval (seconds) |
| `MAKER_MM_POLL_SEC` | `3` | Orderbook check interval while waiting for entry (seconds) |

### Simulation & Logging

| Variable | Default | Description |
|----------|---------|-------------|
| `DRY_RUN` | `true` | `true` — simulate only, no orders submitted or on-chain transactions |
| `SIM_INITIAL_BALANCE` | `100.0` | Starting USDC balance for simulation P&L tracking |
| `RUST_LOG` | `info` | Log level: `info`, `debug`, or `warn` |

---

## Running

Run `make` (no arguments) to see all available commands.

### Simulation mode (safe to test, DRY_RUN=true)

```bash
make maker-tui          # Maker MM with TUI
make conviction-tui     # Conviction with TUI

make maker              # Maker MM, plain logs
make conviction         # Conviction, plain logs
```

### Live mode (DRY_RUN=false)

Make sure your wallet has:
- USDC.e on Polygon (for placing orders)
- POL (for gas fees on redemption transactions)

```bash
make maker-live-tui          # Maker MM live with TUI
make conviction-live-tui     # Conviction live with TUI

make maker-live              # Maker MM live, plain logs
make conviction-live         # Conviction live, plain logs
```

> `DRY_RUN` is overridden by the Makefile target — no need to edit `.env` to switch between sim and live.

### Using pre-built binaries

After `make build`:

```bash
# macOS / Linux
./target/release/maker --tui
./target/release/conviction --tui

# Windows
.\target\release\maker.exe --tui
.\target\release\conviction.exe --tui
```

---

## Building for Other Platforms

### Cross-compile to Linux (from macOS)

```bash
# Install zig (cross-linker)
brew install zig
cargo install cargo-zigbuild

rustup target add x86_64-unknown-linux-gnu
cargo zigbuild --release --target x86_64-unknown-linux-gnu --bin conviction
cargo zigbuild --release --target x86_64-unknown-linux-gnu --bin maker
```

Output: `target/x86_64-unknown-linux-gnu/release/`

### Cross-compile to Windows (from macOS)

```bash
# Install cross (uses Docker)
cargo install cross

rustup target add x86_64-pc-windows-gnu
cross build --release --target x86_64-pc-windows-gnu --bin conviction
cross build --release --target x86_64-pc-windows-gnu --bin maker
```

Output: `target/x86_64-pc-windows-gnu/release/`

> Docker Desktop must be running for `cross` to work.
