# Hank — common developer commands.
# Commands mirror the "Build, Test, Run" section of AGENTS.md; keep the two in sync.

.DEFAULT_GOAL := help

.PHONY: help build release run snapshot test test-integration test-all \
        fmt fmt-check clippy check install clean

help: ## Show this help
	@awk 'BEGIN {FS = ":.*## "} /^[a-zA-Z_-]+:.*## / {printf "  \033[1m%-18s\033[0m %s\n", $$1, $$2}' $(MAKEFILE_LIST)

build: ## Debug build
	cargo build

release: ## Optimized release build
	cargo build --release

run: ## Launch the TUI (bare hank)
	cargo run

snapshot: ## Headless ready list (hank snapshot)
	cargo run -- snapshot

test: ## Unit + render tests (green without bd)
	cargo test

test-integration: ## Gated e2e suite (skips per-test without bd)
	cargo test --test bd_integration

test-all: test test-integration ## All tests

fmt: ## Format code in place
	cargo fmt

fmt-check: ## Quality gate: formatting
	cargo fmt --check

clippy: ## Quality gate: lints (warnings are errors)
	cargo clippy --all-targets -- -D warnings

check: fmt-check clippy test ## All quality gates: fmt, clippy, unit tests

install: ## Install hank to ~/.cargo/bin
	cargo install --path . --locked

clean: ## Remove build artifacts
	cargo clean
