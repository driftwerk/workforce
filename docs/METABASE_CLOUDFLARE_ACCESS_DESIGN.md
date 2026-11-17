# Metabase: Cloudflare Access service tokens

## Goal

Let the Metabase adapter reach an instance that sits behind Cloudflare Access
by sending a Cloudflare Access service token with every request, alongside the
existing `X-API-KEY`.

Two optional per-instance settings:

| Setting                | Header sent               | Secret |
|------------------------|---------------------------|--------|
| `access_client_id`     | `CF-Access-Client-Id`     | no     |
| `access_client_secret` | `CF-Access-Client-Secret` | yes    |

- Both set: every request carries both headers.
- Neither set: nothing extra is sent, and the behaviour is byte-for-byte what it is today.
- Exactly one set: neither header is sent, and a warning is logged (see
  [Half a pair](#half-a-pair-a-warning-not-an-error)).

## Where the code lives

| Concern | Location |
|---|---|
| Hub adapter config | `crates/skills/src/adapters/metabase/config.rs` |
| HTTP client and headers | `crates/skills/src/adapters/metabase/client.rs` |
| Skill construction | `crates/skills/src/adapters/metabase/mod.rs` |
| Hub vault loader and credential hint | `crates/hub/src/skill_loader.rs` (`metabase_vault_config`, `BuiltinSkillMeta` for metabase) |
| Desk Connections catalog and Test | `crates/hub/src/desk_connections/catalog.rs`, `check.rs`, `mod.rs` (`kept`) |
| Local MCP CLI (`config metabase`, `config show`) | `crates/local-mcp/src/cli.rs` |
| Local MCP keychain loader | `crates/local-mcp/src/adapters.rs` |
| Help page | `end-user-docs/adapters/metabase.mdx` |
| Operator docs | `docs/METABASE.md`, `docs/SKILLS.md`, `crates/skills/src/adapters/metabase/SKILL.md`, `crates/local-mcp/README.md` |

## Vault and keychain key layout

These follow the existing per-instance fields exactly. `field_paths("metabase", "instances", <name>, <field>)` already yields both forms:

```
skills/metabase/instances/<name>/access_client_id      # multi-instance
skills/metabase/instances/<name>/access_client_secret
skills/metabase/access_client_id                       # single-instance (flat)
skills/metabase/access_client_secret
```

The local MCP stores them through `paths::workspace_field` and `paths::legacy_field`, like every other Metabase field:

```
skills/metabase/workspaces/<name>/access_client_{id,secret}
skills/metabase/access_client_{id,secret}
```

## Design

### 1. The pair is one value: `AccessToken`

The new code in `client.rs` is a single pure function:

```rust
pub(crate) struct AccessToken { id: HeaderValue, secret: HeaderValue }

enum AccessPair { Unset, Token(AccessToken), Missing(&'static str) }

fn access_pair(id: Option<&str>, secret: Option<&str>) -> Result<AccessPair>
```

- Values are trimmed. An empty or whitespace-only value counts as unset.
- `(Some, Some)`: `Token`. The secret's `HeaderValue` is marked `set_sensitive(true)`, so it never appears in reqwest or hyper debug output.
- `(None, None)`: `Unset`.
- `(Some, None)`: `Missing("access_client_secret")`. `(None, Some)`: `Missing("access_client_id")`.
- A value that isn't a valid header value is an `Err("Invalid Cloudflare Access client id/secret")`, without the value. This is the same handling an invalid `api_key` gets today.

Static clients and vault-backed clients both go through this function, so both paths apply the same rule.

### 2. Sending: in `build_request`, and nowhere else

Every Metabase request (`get`, `post`, `put`, and through them the tools and the schema sync) already builds its headers in `MetabaseClient::build_request`. That function gains one step: resolve the access token, and when there is one, insert both headers next to `x-api-key`. No other request path exists. `sync.rs` and `hub/src/metabase_sync.rs` only call the client's methods.

### 3. Static clients (config file, local MCP)

`MetabaseClient::new` keeps its signature. A builder, shaped like `with_schema_store`, carries the pair:

```rust
pub fn with_access(self, id: Option<&str>, secret: Option<&str>) -> Result<Self>
```

It stores `Option<AccessToken>` in a new `static_access` field. `MetabaseSkill::new` chains it from `MetabaseInstanceConfig`. Half a pair logs the warning once here, at construction.

### 4. Vault-backed clients (hub)

In store mode the client already resolves `api_key` and its site per request from the caller's layered vault. The access pair resolves there too, so a per-user or rotated token takes effect on the next call. That matches how `api_key` behaves.

The rule this must keep, from `adapters/auth.rs`: *a credential is only ever sent to a host chosen by an authority over it.*

The access secret is a credential in its own right, and it may come from a different tier than the API key. Pairing it with the API key's site alone would be wrong. With a per-user key and site but a shared access token, the shared Cloudflare secret would go to a host one user wrote. So resolution is anchored on the secret:

1. `secret = resolve_credential(store, <ws>/access_client_secret, <flat>/access_client_secret)` gives a `Resolved` with its tier.
2. `id = companion_for(store, &secret, <ws>/access_client_id, <flat>/access_client_id)`, so the id travels with the secret.
3. `pair_site = companion_for(store, &secret, <ws>/site_url, <flat>/site_url).map(api_base).or(self.base_url)` is the site an authority over the secret chose.
4. The token is sent only if `pair_site` equals the request's `base_url`, which was already resolved from the API key. Otherwise nothing is sent, and a once-per-client warning says the instance's access token is not paired with its site. The warning names the instance only.

Half a pair, as the caller can see it:
- No secret resolves: the id is checked with `resolve_credential`. If it resolves, the result is `Missing("access_client_secret")`; otherwise it is `Unset`.
- A secret resolves but no id travels with it: `Missing("access_client_id")`.

Both cases go through `access_pair`.

The loader in `metabase_vault_config` builds templates whose credentials the store-backed `make` ignores, because it reads the vault per request. It gains no access fields, so no dead plumbing is added. The skill's vault metadata in `skill_loader.rs` gains two optional `WorkspaceCredentialMeta` entries, and the hint text describes the new paths.

### 5. Desk Connections

`catalog.rs` lists Metabase's extra per-instance settings in `extras`. Adding both fields there means four things:
- Disconnecting an instance removes them (`held_at`).
- Adopting a flat single-instance connection into a named instance carries them along.
- The catalog consistency test requires them to be in the vault editor's known fields, which item 4 covers.

- Testing a kept connection again reads its extras with its fields, and the Metabase Test sends the pair (through the skill's `access_headers`, so the both-or-neither rule still lives in one place). Without that, a working instance behind Access would test as refused.

The Connections form does not ask for them. They are set through the vault, like `max_query_rows`, so a first Connect of an instance behind Access still fails its Test.

### 6. Config struct and redaction

`MetabaseInstanceConfig` and `MetabaseSkillConfig` (flat form) gain:

```rust
#[serde(default)] pub access_client_id: Option<String>,
#[serde(default)] pub access_client_secret: Option<Secret<String>>,
```

`Secret` (from `workforce_common::secret`) prints as `Secret(***)` in `Debug` and as `***` in `Display`, so a config logged with `?config` never carries the secret. No code path serializes these configs. They are deserialized from config files and built in code, so `Secret`'s null serialization is never hit. `resolve()` copies both fields into the flat `default` instance. `default_toml()` shows the commented example.

The local CLI's `config show` already redacts by spec. `access_client_secret` joins `METABASE.secret_fields` and prints as `****`. `access_client_id` is shown, like `site_url`.

### 7. Local MCP CLI

- `METABASE.fields` gains `access_client_id` and `access_client_secret`. `show`, `clear` and workspace clearing iterate the spec, so they handle the new fields with no further code.
- `config metabase` gains `--access-client-id <ID>` and `--access-client-secret <SECRET>`, following the existing optional-secret pattern (Postmark's `--account-token`):
  - Without `--access-client-id` nothing is prompted for, so today's flow is unchanged.
  - With `--access-client-id` and no `--access-client-secret`, the secret is prompted for hidden, like `api_key`, so it stays out of shell history.
  - Supplied values are trimmed. Empty values are skipped, as `resolve_optional_value` does.
  - The `--set-default`-only short-circuit also checks the two new flags.
- `adapters.rs` (`read_metabase_workspace` and `read_metabase_legacy`) reads both fields with `store.get` into the config. There are no environment-variable fallbacks: keys live in the keychain or vault only.

### Half a pair: a warning, not an error

Exactly one setting is a misconfiguration. Rejecting it would still be worse than warning:

- **Requirement.** "When either is missing, nothing extra is sent and behaviour is exactly as today." An instance that works today, such as one not behind Access with a stray leftover id, must keep working.
- **Blast radius.** A static construction error fails the whole Metabase skill. The local MCP's `build()` turns that into "adapter init failed" and every instance disappears, not just the misconfigured one.
- **Rotation.** Setting the two values is two vault writes. Between them, requests continue exactly as before instead of failing.
- **Still visible.** The warning is logged once per client at `warn`. It names the instance and the missing field (`Metabase instance 'prod' has access_client_id but no access_client_secret; Cloudflare Access headers not sent`). If the instance really is behind Access, the request then fails at Cloudflare (403 or a redirect to its login, which `same_origin_redirects` stops), and the warning explains why.

The once-per-client latch is an `AtomicBool` on the client. It stops per-request resolution in store mode from flooding the log.

### Secret hygiene

- The secret is never formatted into a log line, error or hint. Warnings name the field, never a value.
- Its header value is marked sensitive.
- In config, `Secret<String>` redacts `Debug` and `Display`.
- `config show` prints `****`.

## Tests

`client.rs`, using wiremock and asserting on `server.received_requests()` instead of matchers, so absence can be checked:
- `access_headers_are_sent_when_both_are_set`: both headers carry the exact values, and `x-api-key` is still present.
- `no_access_headers_when_neither_is_set`: neither header name is present.
- `half_a_pair_sends_neither_header_and_warns`: run once with only the id and once with only the secret. Neither header is sent, the request still succeeds, a `WARN` naming the missing field is captured, and the captured logs don't contain the secret.
- `blank_access_values_count_as_unset`.
- `access_pair` unit tests for every arm, including an invalid header value whose error text omits the value.
- Store mode (`LayeredInMem` / `InMem`):
  - A shared key, site and pair are sent.
  - A per-user key with a shared site and shared pair is sent.
  - `a_shared_access_secret_is_never_sent_to_a_site_the_user_chose`: a per-user key and site with a shared pair sends no access headers and warns.
  - A pair set at the flat path serves a named instance, by the same fallback as `api_key`.

`config.rs`:
- Deserialization picks up both fields at the flat and instance levels.
- `resolve()` carries them to `default`.
- `format!("{config:?}")` does not contain the secret.

Log capture: a small `MakeWriter` buffer like the one in `desk/src/service/accounts_tests.rs`, scoped with `tracing::subscriber::set_default`. It needs `tracing-subscriber` (workspace) as a dev-dependency of `workforce-skills`.

`local-mcp/src/cli.rs`:
- `adapter_spec_is_secret_exhaustive` asserts `access_client_secret` is secret and `access_client_id` is not.
- `format_fields(&METABASE, ...)` redacts the secret.
- Saving with both flags writes both paths, and a snapshot then shows them.

`local-mcp/src/adapters.rs`:
- `build_metabase_config` reads both fields from a named workspace and from the legacy flat form.

`hub`:
- The catalog consistency test (`catalog.rs`) covers the new `extras` against the vault metadata.

All literals are obviously fake (`test-client-id.access`, `not-a-real-secret`) so they don't trip push protection.

## Constraints on the implementation

- No TODOs, stubs or partial paths. Every item above lands in one change.
- No comments beyond what a reader truly needs. The existing doc-comment style on public items is kept.
- No duplication: `access_pair` is the only place the rule lives, and `build_request` is the only place the headers are inserted.
- Functional style: `Option` and `Result` combinators, with no mutable accumulation where an iterator chain reads clearly.
- Checks run only on the changed crates: `workforce-skills`, `workforce-local-mcp`, and `workforce-hub` (filtered to metabase and connections tests). Each gets `fmt`, `clippy -D warnings` and tests.
