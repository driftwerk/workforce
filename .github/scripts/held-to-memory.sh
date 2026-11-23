#!/usr/bin/env bash
# Runs its arguments held to the runner's memory less 3 GB; anything beyond
# that goes to swap, so a build can't starve the runner itself, which then
# loses contact and the job with it. sudo resets the environment (the
# runner's sudo ignores --preserve-env), so the caller's is passed on whole:
# a build made without the job's CARGO_INCREMENTAL or POSTGRESQL_VERSION is
# built again by the next cargo command that has them. It goes through a
# file only the caller can read, never on a command line, where its tokens
# would show in the process list and the journal.
set -euo pipefail
total_mb=$(($(awk '/^MemTotal:/ { print $2 }' /proc/meminfo) / 1024))
environment=$(mktemp)
trap 'rm -f "$environment"' EXIT
env -0 >"$environment"
sudo systemd-run --scope --quiet --uid="$(id -u)" --gid="$(id -g)" \
  -p MemoryMax="$((total_mb - 3072))M" -p MemorySwapMax=infinity \
  env -i /bin/bash -c '
    while IFS= read -r -d "" pair; do export -- "$pair" 2>/dev/null || true; done <"$1"
    rm -f "$1"
    shift
    exec "$@"' held-to-memory "$environment" "$@"
