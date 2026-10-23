# driftwerk/homebrew-tap

Homebrew tap for [`workforce-local-mcp`](https://github.com/driftwerk/homebrew-tap/releases)
— a local MCP server that gives Claude Code a fast code graph over
your repos, durable cross-session memory, and adapter tools for
Jira / Linear / Datadog / Metabase.

The source lives in a private repo; signed + notarized release
tarballs are mirrored here for public distribution.

## Install

```bash
brew tap driftwerk/tap
brew install workforce-local-mcp
```

Then follow the post-install `caveats` shown by brew — one
`claude mcp add …` command, three `ln -sfn` lines to wire up the
bundled SKILL.md files, and optional adapter credential setup.

## Supported platforms

- **macOS Apple Silicon** (`aarch64-apple-darwin`) — signed and
  notarized with Apple Developer ID.
- **Intel macOS** — not currently packaged. The embedded ONNX
  runtime doesn't ship prebuilt x86_64 macOS binaries and
  cross-compiling from source wasn't worth the CI fragility for the
  shrinking Intel audience.
- **Linux / Windows** — out of scope for this tap. The underlying
  Rust code is cross-platform in principle (keyring uses Secret
  Service / Credential Locker) but not exercised in CI.

## Releases

Tarballs attached to each GitHub Release here are byte-identical
copies of the ones produced by the upstream workforce repo's
release workflow. They're signed with
`Developer ID Application: <name> (<team id>)` and
notarized by Apple; verify with:

```bash
codesign --verify --verbose $(brew --prefix workforce-local-mcp)/bin/workforce-local-mcp
spctl  --assess --type execute --verbose $(brew --prefix workforce-local-mcp)/bin/workforce-local-mcp
# Expected: "accepted  source=Notarized Developer ID"
```

## Uninstall

```bash
brew uninstall workforce-local-mcp
brew untap driftwerk/tap
# Optional: also remove the MCP server registration
claude mcp remove workforce-local
# Optional: nuke all local data (code graphs, memories, keychain creds)
rm -rf ~/Library/Application\ Support/workforce-local
for p in skills/jira skills/linear skills/datadog skills/metabase; do
  security delete-generic-password -s workforce-local-mcp -a "$p" 2>/dev/null || true
done
```
