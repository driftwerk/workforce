#!/usr/bin/env bash
# Run the hub tests that need a vellev node, and only those: each by its
# full path, exactly. Fails unless every name in vellev-node-tests.txt is
# one test, and exactly that many tests ran and passed. Extra arguments go
# to `cargo test` (before the `--`).
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
names=()
while IFS= read -r name; do
  names+=("$name")
done < <(grep -v '^#' "$here/vellev-node-tests.txt" | grep .)
listed="$(cargo test "$@" -p workforce-hub --lib -- --list --format terse | sed -n 's/: test$//p')"
paths=()
for name in "${names[@]}"; do
  found="$(grep -E "(^|::)${name}\$" <<<"$listed" || true)"
  count="$(grep -c . <<<"$found" || true)"
  if [ "$count" != 1 ]; then
    echo "::error::'$name' in vellev-node-tests.txt names $count hub tests, not one"
    exit 1
  fi
  paths+=("$found")
done
out="$(mktemp)"
trap 'rm -f "$out"' EXIT
cargo test "$@" -p workforce-hub --lib -- --exact "${paths[@]}" | tee "$out"
want="test result: ok. ${#paths[@]} passed; 0 failed; 0 ignored;"
if ! grep -qF "$want" "$out"; then
  echo "::error::expected '${want}' of the hub's node tests"
  exit 1
fi
