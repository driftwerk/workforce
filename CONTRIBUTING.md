# Contributing to Workforce

Thanks for helping. This page covers how to get a working build, what we
expect from a pull request, and the licence terms contributions are accepted
under.

## Before you start

- For anything larger than a small fix, open an issue first so we can agree
  the approach before you write the code.
- Security problems go through [SECURITY.md](SECURITY.md), never a public
  issue.
- Everyone taking part follows the [Code of Conduct](CODE_OF_CONDUCT.md).

## Licence

A contribution is licensed under the licence of the part of the repository it
changes: the Elastic License 2.0, except `crates/wasm-skill-sdk`, which is MIT
(see the [README](README.md#licence)). By submitting a contribution you also
grant the licensor a perpetual, worldwide, royalty-free, irrevocable licence to
use, modify, sublicense and distribute it under any terms, including in
commercial products and services.

## Development setup

You need:

- Rust — the toolchain in `rust-toolchain.toml` is installed automatically by
  `rustup`.
- Node.js 20+ and npm, for the web UI in `ui/`.
- Optional: Docker, for running Postgres, NATS or Weaviate locally.

```bash
make ui                       # build the web UI into ui/dist (embedded in the hub)
cargo build -p workforce-cli  # the `workforce` binary
```

`cargo build` works without `make ui`; the hub then answers UI routes with
"UI not built".

On Linux, linking the larger binaries is much faster with
[mold](https://github.com/rui314/mold):

```bash
RUSTFLAGS="-C link-arg=-fuse-ld=mold" cargo build
```

More detail — crate layout, test conventions, how to add a tool or skill — is
in [docs/DEVELOPMENT.md](docs/DEVELOPMENT.md) and
[docs/ARCHITECTURE.md](docs/ARCHITECTURE.md).

## Before you open a pull request

Run what CI runs:

```bash
cargo fmt --all -- --check
cargo clippy --all-targets --all-features --no-deps -- -D warnings
WORKFORCE_SKIP_POSTGRES_TESTS=1 cargo test --all-features
cargo deny check licenses
(cd ui && npm ci && npx tsc --noEmit && npx vitest run)
```

In the pull request:

- Keep it to one change, with tests for the behaviour it adds or fixes.
- Describe what changes for a user or operator, not just what the code does.
- Don't add a dependency whose licence isn't in `deny.toml`'s allow-list.
- Don't commit credentials, customer data or real names in fixtures; use
  `example.com` hosts and obviously fake identifiers.

CI for pull requests runs on GitHub-hosted runners with read-only
permissions and no secrets. Workflows from first-time contributors need a
maintainer's approval before they run.
