#!/usr/bin/env bash
set -e
cargo fmt -- --check
cargo clippy --all-targets --all-features --no-deps -- -D warnings
