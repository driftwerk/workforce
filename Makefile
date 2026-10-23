SHELL := /bin/bash

.PHONY: ui-install ui ui-dev build clean

## Install UI dependencies
ui-install:
	cd ui && npm ci

## Build the UI for production (outputs to ui/dist/)
ui: ui-install
	cd ui && npm run build

## Run UI dev server with hot reload
ui-dev:
	cd ui && npm run dev

## Full build: UI then Rust
build: ui
	cargo build --release -p workforce-cli

## Clean everything
clean:
	rm -rf ui/dist ui/node_modules
	cargo clean
