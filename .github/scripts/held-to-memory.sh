#!/usr/bin/env bash
# Runs its arguments held to the runner's memory less 3 GB; anything beyond
# that goes to swap, so a build can't starve the runner itself, which then
# loses contact and the job with it. sudo resets the environment (the
# runner's sudo ignores --preserve-env), so the caller's is passed on whole
# with env: a build made without the job's CARGO_INCREMENTAL or
# POSTGRESQL_VERSION is built again by the next cargo command that has them.
set -euo pipefail
total_mb=$(($(awk '/^MemTotal:/ { print $2 }' /proc/meminfo) / 1024))
mapfile -d '' environment < <(env -0)
exec sudo systemd-run --scope --quiet --uid="$(id -u)" --gid="$(id -g)" \
  -p MemoryMax="$((total_mb - 3072))M" -p MemorySwapMax=infinity \
  env -i "${environment[@]}" "$@"
