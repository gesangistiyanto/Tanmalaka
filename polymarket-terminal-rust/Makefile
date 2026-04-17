.PHONY: help build build-debug clean check fmt lint test \
        maker maker-tui maker-live maker-live-tui \
        conviction conviction-tui conviction-live conviction-live-tui \
        header-test hmac-test

# Default target
help:
	@echo ""
	@echo "  Polymarket Terminal Bot"
	@echo ""
	@echo "  Build"
	@echo "    make build              Build release binaries"
	@echo "    make build-debug        Build debug binaries"
	@echo "    make clean              Remove build artifacts"
	@echo ""
	@echo "  Maker MM  (DRY_RUN=true)"
	@echo "    make maker              Run maker (no TUI)"
	@echo "    make maker-tui          Run maker with TUI"
	@echo ""
	@echo "  Maker MM  (DRY_RUN=false / LIVE)"
	@echo "    make maker-live         Run maker live (no TUI)"
	@echo "    make maker-live-tui     Run maker live with TUI"
	@echo ""
	@echo "  Conviction  (DRY_RUN=true)"
	@echo "    make conviction         Run conviction (no TUI)"
	@echo "    make conviction-tui     Run conviction with TUI"
	@echo ""
	@echo "  Conviction  (DRY_RUN=false / LIVE)"
	@echo "    make conviction-live         Run conviction live (no TUI)"
	@echo "    make conviction-live-tui     Run conviction live with TUI"
	@echo ""
	@echo "  Dev"
	@echo "    make check             cargo check"
	@echo "    make fmt               cargo fmt"
	@echo "    make lint              cargo clippy"
	@echo "    make test              cargo test"
	@echo "    make header-test       Test HMAC header signing"
	@echo "    make hmac-test         Validate API authentication"
	@echo ""

# ── Build ────────────────────────────────────────────────────────────────────

build:
	cargo build --release

build-debug:
	cargo build

clean:
	cargo clean

# ── Maker MM ─────────────────────────────────────────────────────────────────

maker:
	DRY_RUN=true cargo run --release --bin maker

maker-tui:
	DRY_RUN=true cargo run --release --bin maker -- --tui

maker-live:
	DRY_RUN=false cargo run --release --bin maker

maker-live-tui:
	DRY_RUN=false cargo run --release --bin maker -- --tui

# ── Conviction ───────────────────────────────────────────────────────────────

conviction:
	DRY_RUN=true cargo run --release --bin conviction

conviction-tui:
	DRY_RUN=true cargo run --release --bin conviction -- --tui

conviction-live:
	DRY_RUN=false cargo run --release --bin conviction

conviction-live-tui:
	DRY_RUN=false cargo run --release --bin conviction -- --tui

# ── Dev tools ────────────────────────────────────────────────────────────────

check:
	cargo check

fmt:
	cargo fmt

lint:
	cargo clippy

test:
	cargo test

header-test:
	cargo run --bin header_test

hmac-test:
	cargo run --bin hmac_test
