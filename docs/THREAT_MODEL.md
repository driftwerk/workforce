# Workforce Threat Model

**Status:** initial draft (DW-529). Requires engineering review and operator signoff before being treated as authoritative.

This document states what workforce isolates, what it doesn't, and what the operator is responsible for. The companion audit at [`SECURITY_AUDIT_USER_ISOLATION.md`](SECURITY_AUDIT_USER_ISOLATION.md) lists the concrete bugs the May 2026 user-isolation work closed; this document captures the post-fix posture.

---

## 1. Assets

| Asset | Where it lives | Sensitivity |
|---|---|---|
| User chat transcripts | Per-session JSONL under `data_dir/sessions/`, optional postgres mirror | High — contains user input verbatim |
| Memory store (episodic, semantic, team, organisation) | SQLite + Weaviate, keyed by `(agent_id, identity_id)` | High — extracted long-term context |
| Per-user secrets / OAuth tokens | Encrypted vault under `users/{identity_id}/...` | **Critical** — credentials |
| Identity records (UUID, email, links) | Postgres-backed `IdentityStore` | High — PII |
| Code-graph index | SQLite per agent workspace | Medium — code metadata |
| Agent identity files (SOUL, AGENTS, MEMORY, IDENTITY) | Filesystem under agent dir | Medium — operator-authored config |
| Audit log | Telemetry store | Medium — derived from sensitive accesses |

## 2. Actors

| Actor | How they reach the system | Trust |
|---|---|---|
| Authenticated end user | iOS WS, web, Slack DM/mention | Owner of their own data |
| Operator / admin | Admin token, admin UI | Cross-user visibility, no act-as |
| Agent (LLM) | Tool dispatch inside the runner | Acts on behalf of *one* user at a time, scoped by `CURRENT_USER_ID` |
| External MCP server | Authenticated via per-user vault | Treated as untrusted external; `Sentinel` wraps responses |
| Webhook source | GitHub / Linear / etc. with HMAC | Treated as untrusted; runs under `system:webhook` identity |
| Background scheduler | Internal process | Runs as documented `system:*` identities (no per-user data access) |

## 3. Trust boundaries

```
   ┌────────── End-user network ──────────┐
   │                                       │
   │   iOS / browser / Slack client        │
   │                                       │
   └──────────┬─────────────────┬──────────┘
              │ JWT / paired    │ admin token
              ▼                 ▼
   ┌──────────────────────────────────────┐
   │  Gateway (axum) — auth + caller ctx  │
   ├──────────────────────────────────────┤
   │  AgentService — CURRENT_USER_ID set  │
   │  per request before any tool / recall│
   ├──────────────────────────────────────┤
   │  Memory store (SQLite + Weaviate)    │
   │  identity_id-scoped at every entry   │
   └──────────────────────────────────────┘
```

The gateway is the **only** principal-resolution boundary. Once a request reaches `AgentService`, identity is encoded in the tokio task-local `CURRENT_USER_ID` and is propagated by `SessionContext::capture` into spawned tool tasks.

## 4. Isolation guarantees

Each guarantee links to the test or audit-log query that verifies it.

### 4.1 Cross-user memory recall is impossible without an explicit system opt-in

- Every `MemoryStore::recall|list|count` call rejects (returns empty / zero) when the filter has no `identity_id` and no `system_unscoped` flag (`crates/memory/src/store.rs::is_identity_scope_authorised`).
- The `system_unscoped` flag is reserved for documented sentinel identities (`system:consolidation`, `system:webhook`); operator review of every new use is required.
- **Verified by:**
  - `crates/memory/src/in_memory.rs::tests::recall_fails_closed_when_identity_unset_and_not_system_unscoped`
  - `crates/hub/tests/concurrency_isolation.rs::concurrent_recalls_never_leak_across_users`
  - `crates/hub/tests/concurrency_isolation.rs::concurrent_recalls_with_no_identity_return_nothing`

### 4.2 Cross-user session data is not surfaced via the chat prompt

- `build_cross_session_summary` filters `s.owner == current_session.owner`.
- **Verified by:** `crates/hub/src/serve.rs::tests::cross_session_filter_excludes_other_owners` and siblings.

### 4.3 REST session endpoints require auth and ownership

- `/api/chat`, `/api/sessions/{id}/transcript`, `/api/sessions/{id}/stats`, `/api/sessions/{id}/export`, `/api/sessions/search` all gate on `CallerContext::is_authenticated()` and `caller.can_see_owner(session.owner)`.
- **Verified by:** `crates/hub/src/gateway.rs::session_visibility_tests::*`.

### 4.4 No agent-tool path can pin globally-visible memory

- `memory.pin scope:"all"` is unconditionally rejected by the agent-callable tool. Operators wanting agent-wide pins must use a separate admin path (not yet exposed).
- **Verified by:** `crates/hub/src/memory_tool.rs::tests::pin_rejects_global_scope_from_agent_tool`.

### 4.5 Cross-origin requests cannot abuse authenticated cookies

- `tower_http::cors::CorsLayer::permissive()` was removed; the gateway now reads an explicit `cors_allowed_origins` list. Wildcard `*` disables credentials per CORS spec.
- **Verified by:** `crates/hub/src/gateway.rs::cors_layer_tests::*`.

### 4.6 Webhook-driven workflows cannot read per-user memory

- Webhook dispatch runs under `system:webhook` (`crates/hub/src/webhooks.rs::WEBHOOK_SYSTEM_IDENTITY`); recall against any real user's identity_id matches nothing.

### 4.7 Local-mcp data dirs are bound to a single OS principal

- `assert_single_user_data_dir` writes a marker on first init and refuses to start when the marker mismatches the current OS principal (`USER@HOME`).
- **Verified by:** `crates/local-mcp/src/state.rs::tests::*`.

### 4.8 Runtime tool invocations honour the user-scoped permission cascade

- Tool dispatch goes through PolicyEngine first, then the cascade gate (`workforce_permissions::cascade::evaluate`) when `[permissions] enabled = true`. The cascade can only further restrict the PolicyEngine decision: a PolicyEngine `Allow` can be tightened to `Ask`, `AlwaysAsk`, or `Deny`; a PolicyEngine `Deny` is unchanged. The combiner is a pure function (`crates/agent-runtime/src/runner.rs::combine_with_cascade`) with seven exhaustive test cases.
- Write-time validation matches what runtime accepts: User-scope rule writes go through `cascade::user_choice_is_legal`, so an admin cannot smuggle a row that the runtime would silently ignore (`crates/hub/src/api_handlers/permissions.rs::validate_user_scope_target`).
- Cache invalidation is multi-node-safe via a DB generation counter — a write on node A surfaces in node B's next evaluation without NATS pub/sub (`crates/permissions/src/cache.rs`).
- Runtime "Always Allow" and "Always Deny" choices persist as User-scope rows through `CascadePermissionWriter`, so a future invocation by the same user honours the saved decision without re-prompting (`crates/hub/src/permissions_writer.rs`).
- Read-side knowledge tools (`knowledge.{ask,explain,impact,overview,graph,symbol}`) each carry a `KnowledgeAccessGuard` defence-in-depth gate that short-circuits before touching the shared index when the cascade returns `Deny`.
- Team membership at gate time: `CascadePermissionGate` resolves the calling user's `team_ids` from the `TeamStore` and caches the result per-user with a 60-second TTL — Team-scope rules apply at the runner gate the same way Agent and User rules do. Cache miss / TeamStore failure falls back to an empty `team_ids` rather than upgrading to Deny, since the cascade is itself an additive restriction over PolicyEngine (`crates/hub/src/permissions_writer.rs`).
- **Verified by:** `crates/permissions/src/cascade.rs::tests::*`, `crates/permissions/tests/concurrent_evaluator.rs` (parallel decisions never swap users), `crates/agent-runtime/src/runner.rs::tests::cascade_*` (combiner matrix) + `runner::tests::apply_cascade_*` (wrapper paths), and the unit tests in `crates/hub/src/api_handlers/permissions.rs`, `crates/hub/src/knowledge/access_guard.rs`, and `crates/hub/src/permissions_writer.rs::tests::team_lookup_*`.

## 5. Operator responsibilities (out of scope for code)

These are properties workforce **does not** enforce; the operator must.

- **Network position.** Until DW-516 is fully shipped to all deployments, the gateway must not be exposed beyond a trusted network. Even after it is shipped, exposing the admin port (8090) outside the operator's network is unsupported.
- **Vault key custody.** The vault encryption key is per-deployment and is the only thing that protects per-user secrets at rest.
- **JWT signing key rotation.** Workforce honours JWTs signed with the configured key; key rotation is the operator's responsibility.
- **Identity provisioning.** The operator chooses how identities are created (manual, OIDC auto-provision, paired-device). Workforce trusts whatever the chosen provider asserts.
- **External MCP credentials.** When the operator configures *one* shared OAuth/PAT per external MCP server, every workforce user inherits that credential's blast radius. Per-user credentials must be configured under `users/{identity_id}/...` in the vault.
- **Backups.** Postgres + SQLite + Weaviate. Backup encryption is the operator's choice.

## 6. Explicitly out of scope

The following classes of attack are not in this threat model:

- **Host compromise.** If the operator's host is rooted, all in-process secrets are reachable.
- **Side channels** (timing, cache, power) on shared hosts.
- **LLM hallucination as data.** The agent may *say* something untrue based on its prompt. This document covers data exfiltration, not factual accuracy.
- **Prompt injection from trusted sources.** The Sentinel layer (`crates/agent-runtime/src/security`) wraps untrusted external content, but injection from operator-controlled identity files is by design.
- **Quantum-cryptography-resistant** transport. We use TLS 1.2+ where the operator configures it.
- **Denial-of-service.** Rate limiting is in place for login (`crates/hub/src/rate_limit.rs`), not for general chat throughput.

## 7. Verifiable claims vs. marketing

We do not claim workforce is "100% bulletproof." We claim:

- A documented set of isolation guarantees (§4) each backed by a regression test.
- A fail-closed default at every memory and session read.
- A single, named trust boundary at the gateway, with admin-vs-user roles.
- A continuous audit log (DW-527, in flight) that lets future regressions be detected without customer escalation.

When the audit log work lands, this section will replace "in flight" with the dashboard query operators can run to confirm cross-user reads stay near zero in production.

## 8. Review cadence

This document is reviewed:

1. After every multi-ticket security workstream (next: when DW-525, DW-526, DW-527 land).
2. After every reportable incident.
3. At least once per quarter.

Sign-offs (one per row, dated):

| Reviewer | Date | Notes |
|---|---|---|
| _engineering_ | _pending_ | |
| _security_ | _pending_ | |
| _operations_ | _pending_ | |
