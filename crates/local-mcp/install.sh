#!/usr/bin/env bash
# Install workforce-local-mcp from an extracted release tarball.
#
# Steps:
#   1. Copy the binary to a sensible bin dir on PATH.
#   2. Symlink the bundled SKILL.md directories into ~/.claude/skills/
#      (the directory Claude Code reads skills from).
#   3. Print the `claude mcp add` command so the user can wire up the
#      stdio server with one command.
#
# Usage:
#   cd workforce-local-mcp-<version>-<target>
#   ./install.sh
#
# Env:
#   INSTALL_DIR       Destination for the binary. Default: first writable
#                     among ~/.local/bin, /usr/local/bin. Override to pick
#                     your own.
#   CLAUDE_SKILLS_DIR Destination for SKILL symlinks. Default: ~/.claude/skills.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

die() { echo "error: $*" >&2; exit 1; }

# ── 1. Install the binary ────────────────────────────────────────────────────

BIN_SRC="$SCRIPT_DIR/workforce-local-mcp"
[[ -x "$BIN_SRC" ]] || die "binary not found next to install.sh (expected $BIN_SRC)"

if [[ -z "${INSTALL_DIR:-}" ]]; then
    for candidate in "$HOME/.local/bin" "/usr/local/bin"; do
        if [[ -d "$candidate" && -w "$candidate" ]] || mkdir -p "$candidate" 2>/dev/null; then
            INSTALL_DIR="$candidate"
            break
        fi
    done
fi
[[ -n "${INSTALL_DIR:-}" ]] || die "could not find a writable install dir; set INSTALL_DIR"

mkdir -p "$INSTALL_DIR"
install -m 0755 "$BIN_SRC" "$INSTALL_DIR/workforce-local-mcp"
echo "installed workforce-local-mcp → $INSTALL_DIR/workforce-local-mcp"

# ── 2. Symlink SKILL.md directories ──────────────────────────────────────────

SKILLS_SRC="$SCRIPT_DIR/skills"
SKILLS_DST="${CLAUDE_SKILLS_DIR:-$HOME/.claude/skills}"

if [[ -d "$SKILLS_SRC" ]]; then
    mkdir -p "$SKILLS_DST"
    for skill in "$SKILLS_SRC"/*/; do
        [[ -d "$skill" ]] || continue
        name="$(basename "$skill")"
        dst="$SKILLS_DST/$name"
        [[ -L "$dst" || -e "$dst" ]] && rm -rf "$dst"
        # Copy rather than symlink: the tarball extraction directory may be
        # deleted after install, which would break symlinks.
        cp -R "$skill" "$dst"
        echo "installed skill $name → $dst"
    done
fi

# ── 3. macOS quarantine strip (release binaries download with the flag) ──────

if [[ "$(uname)" == "Darwin" ]]; then
    xattr -d com.apple.quarantine "$INSTALL_DIR/workforce-local-mcp" 2>/dev/null || true
fi

# ── 4. Print next steps ──────────────────────────────────────────────────────

cat <<EOF

Done. Next steps:

  1. Make sure $INSTALL_DIR is on your PATH. If not, add this to your shell rc:
       export PATH="$INSTALL_DIR:\$PATH"

  2. Register the MCP server with Claude Code (user scope so it's
     available in every project, not just the current directory):
       claude mcp add workforce-local --scope user -- $INSTALL_DIR/workforce-local-mcp

  3. (Optional) Configure adapter credentials — only what you want:
       workforce-local-mcp config jira     --email you@x.com --site https://you.atlassian.net
       workforce-local-mcp config linear
       workforce-local-mcp config datadog  --site eu1
       workforce-local-mcp config metabase --site-url https://mb.example.com
       workforce-local-mcp config postmark  # prompts for server token (+ optional account token)
       workforce-local-mcp config sendgrid  # prompts for API key (--from-email sets the default sender)

  4. Start a new Claude Code session and type /mcp to see the tools.

See the bundled README.md for the full guide.
EOF
