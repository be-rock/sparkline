.DEFAULT_GOAL := help
SHELL := bash

# rustup's --no-modify-path install leaves cargo off PATH; find it anyway.
export PATH := $(HOME)/.cargo/bin:$(PATH)

VERSION := $(shell sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -1)
VENV := .spark-venv

.PHONY: help build release-build install test test-live test-tui test-all lint fmt server package set-version release clean

help: ## Show this help
	@echo "sparkline $(VERSION)"
	@echo 'Usage: make <target>'
	@grep -E '^[a-zA-Z_-]+:.*## ' $(MAKEFILE_LIST) | sed 's/:.*## /\t/' | column -t -s "$$(printf '\t')"

build: ## Debug build
	cargo build

release-build: ## Optimized build: target/release/sparkline
	cargo build --release --locked

install: ## Install sparkline into ~/.cargo/bin
	cargo install --locked --path crates/sparkline

test: ## Unit tests (no Spark needed)
	cargo test --locked

test-live: ## Protocol tests against Spark Connect (starts a local server if none is on :15002)
	dev/with-server.sh cargo test --locked -p sparkline-core --test live

$(VENV)/.pyte:
	@[ -x $(VENV)/bin/python ] || uv venv -q --python 3.12 $(VENV)
	uv pip install -q --python $(VENV)/bin/python pyte
	@touch $@

test-tui: build $(VENV)/.pyte ## Drive the REPL in a pseudo-terminal and check the screens
	dev/with-server.sh $(VENV)/bin/python -u dev/drive_tui.py target/debug/sparkline

test-all: lint test test-live test-tui ## Everything CI runs

lint: ## rustfmt check + clippy (warnings are errors)
	cargo fmt --all --check
	cargo clippy --locked --all-targets -- -D warnings

fmt: ## Format the code
	cargo fmt --all

server: ## Run a local Spark Connect server in the foreground
	dev/spark-connect-server.sh

package: ## Package a release tarball for this machine into dist/
	dev/package.sh

set-version: ## Bump the version: make set-version V=0.2.0
	@[ -n "$(V)" ] || { echo "usage: make set-version V=x.y.z" >&2; exit 2; }
	sed -i.bak 's/^version = "$(VERSION)"/version = "$(V)"/' Cargo.toml && rm Cargo.toml.bak
	cargo update --workspace -q
	@echo "version $(VERSION) -> $(V); commit, then 'make release'"

release: ## Tag the current version on main and push it; GitHub Actions builds and publishes the release
	@[ -z "$$(git status --porcelain)" ] || { echo "working tree is not clean" >&2; exit 1; }
	@[ "$$(git rev-parse --abbrev-ref HEAD)" = main ] || { echo "releases are tagged from main" >&2; exit 1; }
	@! git rev-parse -q --verify "refs/tags/v$(VERSION)" >/dev/null || { echo "tag v$(VERSION) exists; run make set-version V=..." >&2; exit 1; }
	$(MAKE) lint test
	git tag -a "v$(VERSION)" -m "sparkline v$(VERSION)"
	git push origin main "v$(VERSION)"
	@echo "Pushed v$(VERSION). Follow the build with: gh run watch"

clean: ## Remove build output and packages
	cargo clean
	rm -rf dist
