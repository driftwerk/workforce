# WASM Skills

Build, distribute, and install sandboxed skill extensions for workforce agents
using WebAssembly (WASM). Skill authors write Rust code using the
`workforce-skill-sdk` crate, compile to `wasm32-unknown-unknown`, and
distribute via GitHub repositories. Agents install skills at runtime and can
call their tools like any built-in tool.

## Quick Start

```bash
# 1. Have an agent scaffold a new skill
#    (or manually create the project structure below)

# 2. Create the project
cargo new my-skill --lib
cd my-skill
```

Create `Skill.toml`:

```toml
[skill]
name = "my-skill"
version = "0.1.0"
description = "My custom skill"

[capabilities]
http = true

[[tools]]
name = "greet"
description = "Greet someone by name"
input_schema = { type = "object", properties = { name = { type = "string" } }, required = ["name"] }
```

Create `Cargo.toml`:

```toml
[package]
name = "my-skill"
version = "0.1.0"
edition = "2021"

[workspace]

[lib]
name = "my_skill"
crate-type = ["cdylib"]

[dependencies]
workforce-skill-sdk = "0.1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"

[profile.release]
opt-level = "s"
lto = true
strip = true
```

Create `src/lib.rs`:

```rust
use workforce_skill_sdk::prelude::*;

workforce_skill_sdk::init!();

workforce_skill_sdk::tool!(greet, |input: ToolInput| {
    let name = input.get_str("name").unwrap_or("world");
    ToolOutput::success(json!({"greeting": format!("Hello, {}!", name)}))
});
```

Build and install:

```bash
# Build
cargo build --target wasm32-unknown-unknown --release

# Install via agent tool
# wasm_skill.install({ "project_path": "wasm-skills/my-skill" })
```

## Skill.toml Reference

The manifest file declares everything about your skill — its identity,
capabilities, tools, credential requirements, and configuration schema.

```toml
[skill]
name = "weather-lookup"          # kebab-case, unique identifier
version = "0.2.0"                # semver
description = "Weather forecasts and alerts"
author = "your-name"             # optional
license = "MIT"                  # optional
min_workforce_version = "0.1.0"  # optional, minimum compatible version

# Capabilities gate access to host functions.
# Only declare what your skill actually needs.
[capabilities]
http = true      # Outbound HTTP requests
vault = true     # Read/write secrets (scoped to your skill)
config = true    # Read skill configuration values
artifacts = true # Read staged files by id and stage new ones for delivery
# memory = true  # Agent memory store (not yet implemented)
# nats = true    # NATS message bus (not yet implemented)

# Each [[tools]] entry becomes a callable tool for the agent.
# Registered as "{skill_name}.{tool_name}" (e.g., "weather-lookup.get_forecast").
[[tools]]
name = "get_forecast"
description = "Get weather forecast for a city"
handler = "get_forecast"  # optional, defaults to the tool name
input_schema = { type = "object", properties = { city = { type = "string" } }, required = ["city"] }

[[tools]]
name = "get_alerts"
description = "Get weather alerts for a region"
handler = "fetch_alerts"  # explicit handler function name
input_schema = { type = "object", properties = { region = { type = "string" } }, required = ["region"] }

# A tool that must never run without a human decision. See "Tools That Require
# Approval" below; `approval_summary` is required whenever the gate is on.
[[tools]]
name = "raise_alert"
description = "Raise a weather alert to a list of subscribers"
requires_approval = true
approval_summary = "raise a `{{severity}}` alert to {{subscribers|count}} subscriber(s)"
input_schema = { type = "object", properties = { severity = { type = "string" }, subscribers = { type = "array" } } }

# Credentials stored in the workforce vault.
# Scoped to wasm-skills/{skill_name}/{credential_name}.
[[credentials]]
name = "api_key"
description = "OpenWeatherMap API key"
required = true

# Non-secret configuration values.
[[config]]
name = "units"
type = "string"
description = "Temperature units (metric or imperial)"
required = false
default = "metric"
```

## SDK API Reference

### Macros

#### `workforce_skill_sdk::init!()`

**Required.** Call once at the top of `lib.rs`. Exports the memory allocator
that the runtime needs.

```rust
workforce_skill_sdk::init!();
```

#### `workforce_skill_sdk::tool!(name, |input: ToolInput| { ... })`

Define a tool handler. The function name must match the `handler` field in
`Skill.toml` (or the tool `name` if `handler` is omitted).

```rust
workforce_skill_sdk::tool!(get_forecast, |input: ToolInput| {
    let city = input.get_str("city").unwrap_or("London");
    ToolOutput::success(json!({"city": city, "temp": 22}))
});
```

### Types

#### `ToolInput`

Received by every tool handler. Wraps the agent's input parameters.

| Method | Returns | Description |
|--------|---------|-------------|
| `input.get_str("key")` | `Option<&str>` | String parameter |
| `input.get_i64("key")` | `Option<i64>` | Integer parameter |
| `input.get_f64("key")` | `Option<f64>` | Float parameter |
| `input.get_bool("key")` | `Option<bool>` | Boolean parameter |
| `input.get("key")` | `Option<&Value>` | Any JSON value |
| `input.raw()` | `&Value` | Raw JSON input |
| `input.tool_name` | `String` | Qualified tool name |
| `input.agent_id` | `String` | Agent making the request |

#### `ToolOutput`

Returned by every tool handler.

```rust
// Success with data
ToolOutput::success(json!({"result": "data"}))

// Failure with message
ToolOutput::error("Something went wrong")
```

### Modules

All modules are available via `use workforce_skill_sdk::prelude::*;`.

#### `log` — Structured Logging

Messages appear in the hub logs prefixed with `[wasm-skill]`.

```rust
log::error("critical failure");
log::warn("something unexpected");
log::info("operation completed");
log::debug("detailed trace info");
```

#### `http` — Outbound HTTP Requests

**Requires:** `http` capability.

```rust
// GET
let resp = http::get("https://api.example.com/data", &[])?;

// POST with headers and body
let resp = http::post(
    "https://api.example.com/submit",
    &[("Authorization", "Bearer token123")],
    &serde_json::to_string(&payload).unwrap(),
);

// Check response
if let Some(resp) = resp {
    if resp.is_success() {
        let data = resp.json();  // Option<Value>
    }
    // resp.status: i32
    // resp.body: String
    // resp.headers: HashMap<String, String>
    // resp.is_base64(): bool (for binary content)
}
```

Available methods: `http::get`, `http::post`, `http::put`, `http::delete`,
`http::patch`, `http::fetch` (generic).

#### `vault` — Secret Storage

**Requires:** `vault` capability.

All paths are automatically scoped to `wasm-skills/{your-skill-name}/`. You
only provide the key name.

```rust
// Read a secret
if let Some(api_key) = vault::get("api_key") {
    // Use the key
}

// Write a secret
vault::set("refresh_token", "new-token-value");
```

#### `config` — Configuration Values

**Requires:** `config` capability.

Reads non-secret config values set when the skill is installed.

```rust
let units = config::get("units").unwrap_or_else(|| "metric".to_string());
```

#### `artifact` — Staged Files

**Requires:** `artifacts` capability.

Read the bytes of a staged file (an upload, or any artifact id the agent passes
in), transform them in-process, and stage the result for delivery. The bytes
never enter the agent's LLM context — the model only sees the `artifact_id` going
in and a small download handle coming out.

```rust
// Read an uploaded file by the artifact_id the agent passes as a tool argument.
let Some(input) = artifact::read(id) else {
    return ToolOutput::error("artifact not found");
};
// input.bytes: Vec<u8>, input.mime: String, input.size: usize

// ... transform input.bytes into out_bytes in plain Rust ...

// Stage the result and have it delivered to the conversation as a download.
let Some(staged) = artifact::stage(&out_bytes, "text/csv", "result.csv") else {
    return ToolOutput::error("could not stage result");
};
ToolOutput::success(artifact::attach(json!({ "rows": n }), &[staged]))
```

Reads and writes are scoped to the calling agent's session and user: a skill
cannot read another session's or user's artifact on a shared agent, and a staged
result is itself session-scoped. Reads are capped at 32 MiB and stages at ~12 MiB
(the guest payload limit). `attach` sets the file-delivery key on your result;
without it a staged artifact is created but never delivered.

#### `chart` — Charts

**Requires:** `artifacts` capability.

Draw a chart from data without shipping any plotting code: hand the host a
`wf-chart` spec — the same JSON the chat widget renders — and it comes back as a
staged PNG (or SVG) you deliver like any other artifact.

```rust
let spec = json!({
    "kind": "bar", "title": "Signups by plan", "unit": "£",
    "labels": ["Free", "Pro", "Team"],
    "series": [{ "name": "March", "data": [420, 180, 60] },
               { "name": "April", "data": [510, 220, 90] }]
});
let Some(image) = chart::render(&spec) else {
    return ToolOutput::error("could not render chart");
};
// To this conversation:
ToolOutput::success(artifact::attach(json!({ "ok": true }), &[image]))
// …or somewhere else, by id (declare the tool in [[permissions]]):
// tools::invoke("messaging.send_file", json!({ "artifact_id": image.artifact_id, "channel": "C123" }))
// tools::invoke("sendgrid.email_send", json!({ "to": "a@b.c", "subject": "Q3", "html": "<p>See chart.</p>",
//     "attach_artifact_ids": [{ "artifact_id": image.artifact_id, "inline": true }] }))
```

`kind` is `pie`, `bar`, `line`, `area` or `scatter`; `series[].data` are numbers
aligned with `labels` (`[x, y]` pairs for scatter); optional `stacked`,
`horizontal`, `x_label`, `y_label`, `unit`. At most 8 series. Invalid specs
return `None` and log the reason. `chart::render_as(&spec, "svg")` gives the
vector original; `chart::render_with(&spec, &chart::Style { theme: Some("slate".into()),
colors: vec!["#2a78d6".into()], ..Default::default() })` picks a theme or an
explicit series palette.

#### `util` — Utilities

Always available (no capability required).

```rust
let uuid = util::generate_uuid();     // UUID v4 string
let time = util::current_time();      // RFC3339 timestamp
```

## Capabilities

Skills must declare which host functions they need. Calls to host functions
without the required capability return an error.

| Capability | Host Functions | Purpose |
|------------|---------------|---------|
| `http` | `http::get`, `http::post`, etc. | Outbound HTTP requests |
| `vault` | `vault::get`, `vault::set` | Scoped secret storage |
| `config` | `config::get` | Skill configuration |
| `artifacts` | `artifact::read`, `artifact::stage`, `chart::render` | Read/stage staged files by id; draw charts |
| `memory` | *(planned)* | Agent memory store |
| `nats` | *(planned)* | NATS message bus |

## Tools That Require Approval

A tool declares `requires_approval = true` when it must not run without a
human decision, whatever the hub's policy says. It then also has to declare
`approval_summary`, the one-line template the **host** renders into the
approval prompt from that call's own arguments.

Summary grammar:

| Form | Renders |
|------|---------|
| `{{field}}` / `{{a.b.c}}` | a dotted, lowercase path from the input |
| `{{field\|count}}` | how many items the value holds (`count` is the only filter) |
| a string / number / boolean | itself |
| an array | `N item(s)` |
| an object | `(object)` |
| `null` | `(none)` |
| an absent path | `(missing)` |

Values are capped at 40 characters and the whole line at 200. The guest never
runs to produce the summary: it is asked for on every tool call in every
round, so a guest round-trip would put a WASM instantiation on the dispatch
hot path, and a guest that composes its own sentence could describe something
other than what is about to execute. A template that does not parse simply
falls back to the generic parameter-shape summary.

What the gate does and does not do:

- **It only ever tightens.** It is composed after policy, the capability gate
  and the permission cascade, so it can add a prompt but never remove one, and
  it never offers "allow every time". A member's saved Always-Allow cannot
  defeat it.
- **It asks that *a* human decides, not *which*.** Four-eyes remains an
  operator's permission rule with designated `approvers` and
  `allow_self_approval = false`.
- **Where nobody can be asked, nothing runs.** A workflow tool step and the
  job runner both refuse a gated tool; another skill could never invoke it
  anyway. Saving a workflow whose tool step names a gated tool returns a
  notice, not a rejection, because a skill can gain a gate long after the
  workflow was written.
- **`wasm_skill.test` still dry-runs it**, which is what keeps the gate
  testable instead of pushing authors to try it in production.

Adding or removing a gate changes the skill's consent contract, so grants go
stale and users re-run `cmd: skill grant`. Rewording the summary does not — it
is a legibility aid, not a security boundary. Pinning the gate set makes a
removal visible; what actually stops a call is that `requires_approval` is
read from the live installed manifest on every dispatch.

Both fields are omitted from the canonical `Skill.toml` when unset, so an
existing ungated skill's fingerprint is unchanged and no grant is invalidated
by this feature shipping. A manifest that uses them needs a hub at or after
this release: an older hub rejects the unknown keys outright, which is the
fail-closed direction.

## Vault Scoping

WASM skills get **namespaced vault access**. A skill named `weather-lookup`
can only access keys under `wasm-skills/weather-lookup/`. When your code
calls `vault::get("api_key")`, the runtime reads from
`wasm-skills/weather-lookup/api_key` in the actual vault.

This means:
- Skills cannot read other skills' secrets
- Skills cannot read built-in skill credentials (e.g., `skills/github/token`)
- The agent can set credentials for a skill before or after installation:
  `vault.set("wasm-skills/weather-lookup/api_key", "sk-...")`

## Agent Tools

Agents have these tools for managing WASM skills:

| Tool | Description |
|------|-------------|
| `wasm_skill.guide` | Return the complete authoring guide |
| `wasm_skill.scaffold` | Create a new skill project from template |
| `wasm_skill.build` | Compile a local skill project to `.wasm` |
| `wasm_skill.test` | Dry-run an installed skill's tool against mocks |
| `wasm_skill.install` | Install, or reinstall, from a `.wasm` file, a project directory, or a GitHub repo |
| `wasm_skill.update` | Move a skill installed from a GitHub release to a newer release |
| `wasm_skill.uninstall` | Remove an installed skill |
| `wasm_skill.list` | List the WASM skills the caller can use |
| `wasm_skill.restore_source` | Copy a skill's stored source snapshot back into the workspace |
| `wasm_skill.export` / `wasm_skill.import` | Move a skill between installs as a `.wfskill.zip` |
| `wasm_skill.publish` | Open a pull request adding an installed skill to a catalog (shares `crates/hub/src/skill_publish.rs` with `system.publish_external_skill`) |

Ownership, sharing and visibility (`cmd: skill shares|share|unshare|visibility`, `skill.*`) also govern external (SKILL.md) skills; see SKILLS.md.

### Install Sources

```jsonc
// From a pre-built .wasm file (Skill.toml must be alongside it)
wasm_skill.install({ "wasm_path": "skills/weather/weather_skill.wasm" })

// From a project directory in the workspace (builds before installing)
wasm_skill.install({ "project_path": "wasm-skills/weather-skill" })

// From GitHub, cloned and built at an exact commit
wasm_skill.install({ "github_repo": "acme/skills@<40-hex-commit>#subpath=weather-skill" })

// From a published GitHub release asset, pinned by its digest
wasm_skill.install({ "github_repo": "acme/weather-skill@v1.0.0", "sha256": "<hex digest of the .wasm>" })

// With config values
wasm_skill.install({ "github_repo": "acme/skills@<40-hex-commit>#subpath=weather-skill", "config": { "units": "metric" } })
```

A `github_repo` install needs a supply-chain pin: an exact 40-hex commit ref, which is cloned, verified and built on the hub (so the caller needs permission to build there), or, for a skill at the repository root, the `sha256` of a published release `.wasm`. Private repositories are read with the caller's GitHub token, `skills/github/workspaces/<owner>/token`, else `skills/github/token`.

Installing a skill that is already installed replaces it, and replaces its config with the `config` passed: an omitted `config` clears it, so pass the whole set again.

An external (`SKILL.md`) skill installs with `system.install_external_skill`, which takes the same `github_repo`, and a skill a configured catalog lists installs by name with `system.install_catalog_skill`. See [Choosing an install tool](SKILLS.md#choosing-an-install-tool).

### Update Flow

```jsonc
// Update to latest GitHub release
wasm_skill.update({ "skill": "weather-skill" })

// Update to specific version
wasm_skill.update({ "skill": "weather-skill", "version": "v2.0.0" })
```

Updates preserve the skill's existing configuration.

`wasm_skill.update` looks up the repository's latest release tag, so it serves release-tag installs. Update a skill installed from a commit by installing the new commit with `wasm_skill.install`, passing its config again.

## GitHub Distribution

### Repository Structure

```
my-skill/
├── Skill.toml
├── Cargo.toml
├── .cargo/
│   └── config.toml          # Sets wasm32-unknown-unknown as default target
├── src/
│   └── lib.rs
├── .github/
│   └── workflows/
│       └── release.yml       # Auto-builds .wasm on tag push
└── .gitignore
```

### Publishing a Release

```bash
git tag v1.0.0
git push origin v1.0.0
```

The CI workflow (generated by `wasm_skill.scaffold`) builds the `.wasm` and
attaches it to the GitHub Release along with `Skill.toml`. This enables fast
installation: `wasm_skill.install` with `github_repo = "owner/repo@v1.0.0"` and
the `sha256` of that `.wasm` downloads the pre-built binary instead of cloning
and building from source (see [Install Sources](#install-sources)).

### Version Management

- Versions follow [semver](https://semver.org/) (`MAJOR.MINOR.PATCH`)
- Git tags should be prefixed with `v` (e.g., `v1.0.0`)
- `wasm_skill.update` compares semver to determine if an update is available
- The `min_workforce_version` field in `Skill.toml` declares compatibility

## Local Development Workflow

### Using Agent Tools

```
Human: "Build me a skill that queries our Grafana instance"

Agent: wasm_skill.scaffold({
    name: "grafana-query",
    description: "Query Grafana dashboards and panels",
    capabilities: ["http", "vault"],
    tools: [
        { name: "query", description: "Run a Grafana query" },
        { name: "list_dashboards", description: "List available dashboards" }
    ]
})

Agent: [edits src/lib.rs to implement the tools]

Agent: wasm_skill.build({
    project_path: "./grafana-query",
    install: true,
    config: { "base_url": "https://grafana.internal" }
})

Agent: grafana-query.list_dashboards({})
→ WASM executes, returns dashboard list