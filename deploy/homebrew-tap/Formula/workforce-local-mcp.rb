class WorkforceLocalMcp < Formula
  desc "Local MCP server giving Claude Code a code graph, memory, and adapter tools"
  homepage "https://github.com/driftwerk/homebrew-tap"
  version "0.1.2"
  license "MIT"

  # Platform coverage:
  #   * macOS Apple Silicon — signed + notarized with Developer ID.
  #     Intel macOS is not currently packaged: `fastembed` → `ort` has no
  #     prebuilt x86_64 macOS binary, and cross-compiling the ONNX runtime
  #     from source wasn't worth the CI fragility for the shrinking Intel-Mac
  #     audience.
  #   * Linux x86_64 — primarily for WSL2 devs and CI shells.
  on_macos do
    on_arm do
      url "https://github.com/driftwerk/homebrew-tap/releases/download/local-mcp-v#{version}/workforce-local-mcp-#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "2ef83c7e91d2017d4e7be15d43eb2db083f3606d7f410fef6080e5f7d5a4b317"
    end
  end

  on_linux do
    on_intel do
      url "https://github.com/driftwerk/homebrew-tap/releases/download/local-mcp-v#{version}/workforce-local-mcp-#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "REPLACE_WITH_LINUX_SHA256"
    end
  end

  def install
    bin.install "workforce-local-mcp"
    (pkgshare/"skills").install Dir["skills/*"]
  end

  def caveats
    <<~EOS
      Register the MCP server with Claude Code (user scope so every session
      sees it):

        claude mcp add workforce-local --scope user -- #{opt_bin}/workforce-local-mcp

      Symlink the bundled SKILL.md directories into Claude Code's skills dir
      so it knows when to reach for the tools:

        mkdir -p ~/.claude/skills
        ln -sfn #{opt_pkgshare}/skills/codebase-knowledge ~/.claude/skills/codebase-knowledge
        ln -sfn #{opt_pkgshare}/skills/project-memory     ~/.claude/skills/project-memory
        ln -sfn #{opt_pkgshare}/skills/adapter-integrations ~/.claude/skills/adapter-integrations

      Configure adapter credentials as needed (all optional — each
      adapter activates only when its creds are set):

        workforce-local-mcp config jira     --email you@example.com --site https://you.atlassian.net
        workforce-local-mcp config linear
        workforce-local-mcp config datadog  --site eu1
        workforce-local-mcp config metabase --site-url https://mb.yourcompany.com

      Storage backend by platform:
        * macOS: credentials live in the login Keychain (service
          "workforce-local-mcp"). Keychain Access.app can inspect them.
        * Linux (incl. WSL): credentials live in a mode-0600 TOML file
          at $XDG_DATA_HOME/workforce-local-mcp/secrets.toml
          (default ~/.local/share/workforce-local-mcp/secrets.toml).
          Override the path with WORKFORCE_LOCAL_MCP_SECRETS.

      Then restart Claude Code; '/mcp' in a session will show the
      workforce-local server connected with up to 107 tools.
    EOS
  end

  test do
    assert_match "workforce-local-mcp",
                 shell_output("#{bin}/workforce-local-mcp --help")
  end
end
