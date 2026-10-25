#!/usr/bin/env bash
# Fetch a Weaviate server binary from Weaviate's GitHub release, without
# Docker, checked against a pinned sha256 and cached for the runner.
#
#   fetch-weaviate-binary.sh <version> <sha256 of the linux-amd64 archive> <dir>
#
# Writes <dir>/weaviate and prints its path. The archive is kept under
# ~/.cache/weaviate-ci, so that a runner downloads each version once; a
# cached archive is checked again before every use, and one whose sha256 is
# not the pinned one is refused, never used. linux/amd64 only: what the
# runners are.
#
# The pinned sha256 is the one Weaviate publishes beside the archive; it
# is pinned here rather than fetched, so that what is tested cannot change
# under the same version.
set -euo pipefail

usage="usage: fetch-weaviate-binary.sh <version> <sha256> <dir>"
version="${1:?$usage}"
want="${2:?$usage}"
dir="${3:?$usage}"
case "$want" in
  [0-9a-f][0-9a-f]*) [ "${#want}" = 64 ] || { echo "::error::$want is not a sha256" >&2; exit 1; } ;;
  *) echo "::error::$want is not a sha256" >&2; exit 1 ;;
esac

name="weaviate-v${version}-linux-amd64.tar.gz"
cache="$HOME/.cache/weaviate-ci"
archive="$cache/$name"
mkdir -p "$cache" "$dir"

sha() { python3 -c 'import hashlib, sys; print(hashlib.sha256(open(sys.argv[1], "rb").read()).hexdigest())' "$1"; }

if [ ! -f "$archive" ]; then
  curl -fsSL --retry 3 -o "$archive.part.$$" \
    "https://github.com/weaviate/weaviate/releases/download/v${version}/${name}"
  mv "$archive.part.$$" "$archive"
fi
got="$(sha "$archive")"
if [ "$got" != "$want" ]; then
  rm -f "$archive"
  echo "::error::$name is $got, not the pinned $want: refusing it" >&2
  exit 1
fi

rm -f "$dir/weaviate"
tar -xzf "$archive" -C "$dir" weaviate
chmod +x "$dir/weaviate"
echo "$dir/weaviate"
