#!/usr/bin/env bash
# Runs its arguments held to the runner's memory less 3 GB, the rest of
# what they use swapped: a build can't starve the runner itself, which then
# loses contact and the job with it.
set -euo pipefail
total_mb=$(($(awk '/^MemTotal:/ { print $2 }' /proc/meminfo) / 1024))
exec sudo --preserve-env systemd-run --scope --quiet --uid="$(id -u)" --gid="$(id -g)" \
  -p MemoryMax="$((total_mb - 3072))M" -p MemorySwapMax=infinity \
  env "PATH=$PATH" "$@"
