#!/usr/bin/env bash
# Runs its arguments held to the runner's memory less 3 GB; anything beyond
# that goes to swap, so a build can't starve the runner itself, which then
# loses contact and the job with it. sudo's secure_path resets PATH, so the
# caller's is passed on with env.
set -euo pipefail
total_mb=$(($(awk '/^MemTotal:/ { print $2 }' /proc/meminfo) / 1024))
exec sudo --preserve-env systemd-run --scope --quiet --uid="$(id -u)" --gid="$(id -g)" \
  -p MemoryMax="$((total_mb - 3072))M" -p MemorySwapMax=infinity \
  env "PATH=$PATH" "$@"
