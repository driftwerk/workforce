#!/usr/bin/env bash
set -e
cargo fmt
cargo clippy --all-targets --all-features --no-deps -- -D warnings
