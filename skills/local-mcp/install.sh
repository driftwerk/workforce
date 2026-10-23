#!/usr/bin/env bash
# Install workforce-local-mcp skills into the user's Claude Code skills dir.
# Idempotent: re-running replaces the symlinks.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
TARGET_DIR="${CLAUDE_SKILLS_DIR:-$HOME/.claude/skills}"

mkdir -p "$TARGET_DIR"

for skill in codebase-knowledge project-memory adapter-integrations workforce-remote; do
    src="$SCRIPT_DIR/$skill"
    dst="$TARGET_DIR/$skill"

    if [[ ! -d "$src" ]]; then
        echo "error: expected skill dir $src not found" >&2
        exit 1
    fi

    if [[ -L "$dst" || -e "$dst" ]]; then
        rm -rf "$dst"
    fi
    ln -s "$src" "$dst"
    echo "linked $dst -> $src"
done

cat <<EOF

Skills installed. Next step: register the MCP server with Claude Code:

  claude mcp add workforce-local -- $REPO_ROOT/target/release/workforce-local-mcp

(or point at the debug binary at $REPO_ROOT/target/debug/workforce-local-mcp
during development).

Optionally set:
  export WORKFORCE_LOCAL_WORKSPACE=/path/to/your/repo
  export WORKFORCE_LOCAL_DATA_DIR=/custom/data/path
EOF
