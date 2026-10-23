//! Host functions exposed to WASM skill guests.
//!
//! Each host function is registered on the Wasmtime `Linker` and becomes
//! callable from guest code via the `workforce-skill-sdk` FFI bindings.
//! Capability enforcement is checked per-call based on the skill's declared
//! capabilities in its manifest.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use base64::Engine;
use eyre::{bail, Result};
use wasmtime::*;
use workforce_agent::artifacts::{
    download_filename, download_marker, resolve_bytes_input, ArtifactBody, ArtifactDescriptor,
    ArtifactId, ArtifactStore, BytesRequest, ContentType, ATTACHMENT_TTL,
};
use workforce_agent::tools::chart::{draw as draw_chart, request as chart_request};
use workforce_common::settings::SecretStore;

use crate::manifest::ManifestPermission;
use crate::types::{
    Capability, CredentialScope, SkillCallContext, SkillDocumentStore, SkillToolInvoker,
};

/// State accessible by host functions during WASM execution.
pub struct HostState {
    pub skill_name: String,
    pub capabilities: HashSet<Capability>,
    pub tool_name: String,
    /// Agent ID making the request. Used in audit logs.
    pub agent_id: String,
    /// User ID making the request. `None` when the call is system-initiated
    /// without an explicit `with_system_identity` scope. The host hard-fails
    /// any per-user vault read in that case rather than silently fall through.
    pub user_id: Option<String>,
    /// Vault backend. Reads and writes are scoped to
    /// `wasm-skills/{skill}/{users/{uid}|shared}/{key}` — guest code never
    /// sees the prefix.
    pub vault_store: Option<Arc<dyn SecretStore>>,
    /// Per-credential scope as declared in the skill's manifest.
    /// Looked up by the bare `key` the guest passes to `wf_vault_get`.
    /// Keys not in this map default to [`CredentialScope::User`].
    pub credential_scopes: HashMap<String, CredentialScope>,
    /// Hostnames the skill is allowed to reach via `wf_http_fetch`. Empty
    /// means no outbound HTTP is permitted.
    pub allowed_hosts: Vec<String>,
    /// Build fingerprint of the installed skill (binary + canonical
    /// manifest, [`crate::types::grant_fingerprint`]) — validates legacy
    /// grant records and feeds the audit log.
    pub grant_fingerprint: String,
    /// Consent-contract fingerprint of the installed manifest
    /// ([`crate::types::contract_fingerprint`]). Grants stay valid while
    /// their record pins this value, so pure-logic rebuilds never need
    /// re-approval — only consent-surface edits do.
    pub contract_fingerprint: String,
    /// Platform-tool permissions declared in the manifest, keyed by tool
    /// name. `wf_tool_invoke` refuses anything not in this map.
    pub permissions: HashMap<String, ManifestPermission>,
    /// Bridge into the platform's governed tool dispatch. `None` when the
    /// runtime is embedded without one — `wf_tool_invoke` then fails with a
    /// notice.
    pub tool_invoker: Option<Arc<dyn SkillToolInvoker>>,
    /// Tool-invocation attempts consumed this execution (bounded by
    /// [`MAX_TOOL_CALLS`]).
    pub tool_calls_used: u32,
    /// HTTP fetch attempts consumed this execution (bounded by
    /// [`MAX_HTTP_FETCHES`]).
    pub http_fetches_used: u32,
    /// Skill-scoped non-secret config (read via `wf_config_get`).
    pub config: HashMap<String, String>,
    /// Artifact store backing `wf_read_artifact` / `wf_stage_artifact`. The
    /// engine injects the process-global store; `None` when one isn't
    /// configured (the host functions then fail gracefully). Reads and the
    /// session/user scoping ride the ambient task-locals the engine
    /// re-establishes across `spawn_blocking` (see `engine::execute_tool`).
    pub artifact_store: Option<Arc<dyn ArtifactStore>>,
    /// The hub's document store behind `wf_store_call`; `None` when the
    /// runtime is embedded without one (the call then fails with a notice).
    pub document_store: Option<Arc<dyn SkillDocumentStore>>,
    /// Most-recent host response buffer; populated by host fns and read
    /// back by the guest via `wf_get_host_response`.
    pub host_response: Option<String>,
    /// Host-authored remedy for a policy denial during this call (ungranted
    /// or stale credential grant, guest write to a declared credential,
    /// disallowed host). The engine copies it onto the `WasmToolResponse`
    /// after the guest returns, so the agent always sees the instructions
    /// regardless of how the guest reports the failure.
    pub policy_notice: Option<String>,
    /// Per-instance wasmtime resource limits (memory cap) — bounds guest
    /// `memory.grow` so a single skill can't OOM the host (audit W3).
    pub store_limits: wasmtime::StoreLimits,
}

/// Why a per-user vault path could not be constructed. Distinguished
/// from generic errors so the host can return a specific error code
/// (`-3`) and audit log a structured rejection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum VaultPathError {
    /// `User` scope was requested but no `CURRENT_USER_ID` is in scope.
    /// The host returns a hard error rather than falling back to a shared
    /// bucket — that would leak one user's secret to another.
    NoUserIdentity,
    /// `user_id` contained a path separator, traversal segment, or other
    /// character that would let the caller escape the per-user
    /// namespace. Identity-aware callers must pass an opaque token (a
    /// UUID, Slack user ID, etc.) — never a structured path.
    UnsafeUserId,
    /// The guest-supplied credential `key` contained a `..`/`.` traversal
    /// segment or a NUL — it must not escape its skill/user namespace
    /// (audit W5).
    UnsafeKey,
}

/// Reject path segments that aren't safe to embed in a vault key:
/// empty, contains `/` or `\`, equals `..` or `.`, or contains a NUL.
fn is_safe_path_segment(s: &str) -> bool {
    !s.is_empty()
        && s != ".."
        && s != "."
        && !s.contains('/')
        && !s.contains('\\')
        && !s.contains('\0')
}

impl HostState {
    /// Check if the skill has a required capability.
    fn require_capability(&self, cap: Capability) -> bool {
        if self.capabilities.contains(&cap) {
            return true;
        }
        tracing::warn!(
            skill = %self.skill_name,
            tool = %self.tool_name,
            capability = %cap,
            "WASM skill called host function without required capability"
        );
        false
    }

    /// Compute the full vault path for a credential `key`, applying the
    /// scope declared in the manifest (default `User`).
    ///
    /// - `User`  → `wasm-skills/{skill}/users/{uid}/{key}`. Hard-fails
    ///   when `user_id` is `None` or path-unsafe.
    /// - `Shared` → `wasm-skills/{skill}/shared/{key}`. No identity needed.
    fn scoped_vault_path(&self, key: &str) -> std::result::Result<String, VaultPathError> {
        self.path_under(key, "")
    }

    /// Where the user's consent record for a declared credential lives. The
    /// `grants/` subtree is a sibling of `users/` and `shared/`, so guest keys
    /// (which always map under one of those two) can never reach it, and the
    /// agent's `vault.*` is denied all of `wasm-skills/*`.
    fn grant_record_path(&self, key: &str) -> std::result::Result<String, VaultPathError> {
        self.path_under(key, "grants/")
    }

    fn path_under(&self, key: &str, prefix: &str) -> std::result::Result<String, VaultPathError> {
        // Defense-in-depth (audit W5): the guest-supplied key must not traverse
        // out of its namespace. Allow hierarchical ('a/b') and dotted keys, but
        // reject empty/NUL or any '..'/'.' path segment.
        if key.is_empty()
            || key.contains('\0')
            || key.split(['/', '\\']).any(|seg| seg == ".." || seg == ".")
        {
            return Err(VaultPathError::UnsafeKey);
        }
        let scope = self
            .credential_scopes
            .get(key)
            .copied()
            .unwrap_or(CredentialScope::User);
        match scope {
            CredentialScope::User => {
                let uid = self
                    .user_id
                    .as_deref()
                    .ok_or(VaultPathError::NoUserIdentity)?;
                if !is_safe_path_segment(uid) {
                    return Err(VaultPathError::UnsafeUserId);
                }
                Ok(format!(
                    "wasm-skills/{}/{}users/{}/{}",
                    self.skill_name, prefix, uid, key
                ))
            }
            CredentialScope::Shared => Ok(format!(
                "wasm-skills/{}/{}shared/{}",
                self.skill_name, prefix, key
            )),
        }
    }

    fn is_declared_credential(&self, key: &str) -> bool {
        self.credential_scopes.contains_key(key)
    }

    /// The identity + build context handed to the [`SkillToolInvoker`].
    fn skill_call_context(&self) -> SkillCallContext {
        SkillCallContext {
            skill_name: self.skill_name.clone(),
            grant_fingerprint: self.grant_fingerprint.clone(),
            agent_id: self.agent_id.clone(),
            user_id: self.user_id.clone(),
        }
    }

    /// Whether the record stored at `path` (if any) still covers this build's
    /// consent contract — see [`crate::types::grant_record_is_current`]. A
    /// legacy record that validates is upgraded in place to the dual-field
    /// form (provably consent-equivalent — see
    /// [`crate::types::upgraded_legacy_record`]), so it survives future
    /// pure-logic rebuilds without another re-grant ceremony. The write is
    /// idempotent under concurrency and non-fatal on failure; stale or
    /// corrupt records are never touched — re-consent stays human.
    fn grant_at(&self, store: &dyn SecretStore, path: &str) -> Option<bool> {
        let raw = store.get(path)?;
        let current = crate::types::grant_record_is_current(
            &raw,
            &self.grant_fingerprint,
            &self.contract_fingerprint,
        );
        if current {
            if let Some(upgraded) =
                crate::types::upgraded_legacy_record(&raw, &self.contract_fingerprint)
            {
                // Skip the write if a revoke or re-pin landed since the read.
                // The remaining get→set window is microseconds, open only for
                // not-yet-migrated legacy records — a population that shrinks
                // to zero — and a lost race costs one revoke re-run at worst.
                if store.get(path).as_deref() == Some(raw.as_str()) {
                    match store.set_with_actor(
                        path,
                        &upgraded,
                        workforce_common::settings::VaultActor::System("wasm-grant-migration"),
                    ) {
                        Ok(()) => tracing::info!(
                            target: "sentinel.audit",
                            skill = %self.skill_name,
                            path = %path,
                            contract = %self.contract_fingerprint,
                            "legacy wasm skill grant migrated to contract pinning"
                        ),
                        Err(e) => tracing::warn!(
                            skill = %self.skill_name,
                            path = %path,
                            error = %e,
                            "legacy grant migration write failed; grant still honored"
                        ),
                    }
                }
            }
        }
        Some(current)
    }

    /// `None` when a tools grant covers the installed contract — the caller's
    /// own per-user `_tools` record, or the admin-written shared one covering
    /// every user. A caller with no identity is refused even under a shared
    /// grant: every skill tool call must be attributable to a user.
    fn tool_grant_error(&self, store: &dyn SecretStore) -> Option<String> {
        let user_path = match self.grant_record_path(TOOLS_GRANT_KEY) {
            Ok(p) => p,
            Err(_) => {
                return Some(format!(
                    "Tool invocation by skill '{}' requires a user identity — none is in \
                     scope for this call",
                    self.skill_name
                ))
            }
        };
        let shared_path = format!(
            "wasm-skills/{}/grants/shared/{TOOLS_GRANT_KEY}",
            self.skill_name
        );
        let user = self.grant_at(store, &user_path);
        let shared = self.grant_at(store, &shared_path);
        if user == Some(true) || shared == Some(true) {
            return None;
        }
        let skill = &self.skill_name;
        Some(match (user.is_some(), shared.is_some()) {
            (true, true) => format!(
                "Both this user's tools grant and the org-wide shared grant for skill \
                 '{skill}' are stale: the skill's consent contract (capabilities, network \
                 hosts, credentials, or permissions) changed after they were approved. \
                 The user re-approves themselves with `cmd: skill grant {skill} tools`; \
                 an admin re-approves every user at once with `cmd: skill grant {skill} \
                 tools shared`."
            ),
            (true, false) => format!(
                "This user's tools grant for skill '{skill}' is stale: the skill's \
                 consent contract (capabilities, network hosts, credentials, or \
                 permissions) changed after it was approved. Ask the user to re-approve \
                 with `cmd: skill grant {skill} tools`."
            ),
            (false, true) => format!(
                "The org-wide shared tools grant for skill '{skill}' is stale: the \
                 skill's consent contract (capabilities, network hosts, credentials, or \
                 permissions) changed after it was approved. An admin re-approves every \
                 user at once with `cmd: skill grant {skill} tools shared`, or this user \
                 can approve just themselves with `cmd: skill grant {skill} tools`."
            ),
            (false, false) => format!(
                "Skill '{skill}' has not been granted tool access. The user approves it by \
                 typing `cmd: skill grant {skill} tools` in chat, or an admin approves it \
                 for every user with `cmd: skill grant {skill} tools shared` — either shows \
                 every declared tool and stays valid until the skill's consent contract \
                 changes (pure code rebuilds keep it)."
            ),
        })
    }

    /// `None` when `key` is not a declared credential, or when its grant
    /// record covers the installed consent contract. Otherwise the remedy
    /// the agent must relay to the user. Fails closed: an unreadable or
    /// corrupt grant record counts as stale.
    fn credential_consent_error(&self, store: &dyn SecretStore, key: &str) -> Option<String> {
        if !self.is_declared_credential(key) {
            return None;
        }
        let grant_path = match self.grant_record_path(key) {
            Ok(p) => p,
            Err(_) => {
                return Some(format!(
                    "Credential '{key}' of skill '{}' could not be checked for user consent",
                    self.skill_name
                ))
            }
        };
        match self.grant_at(store, &grant_path) {
            Some(true) => None,
            Some(false) => Some(format!(
                "The user's grant for credential '{key}' is stale: skill '{skill}' changed \
                 its consent contract (capabilities, network hosts, credentials, or \
                 permissions) after they approved it. Ask the user to re-approve by typing \
                 `cmd: skill grant {skill} {key}` in chat — the stored value is kept, so \
                 they do not need to retype the secret.",
                skill = self.skill_name
            )),
            None => Some(format!(
                "Credential '{key}' of skill '{skill}' has not been granted by the user. \
                 Only the user can provision it, by typing two chat commands: \
                 `cmd: secret {key} <value>` and then `cmd: skill grant {skill} {key}`. \
                 The agent cannot set it via vault tools or tool arguments.",
                skill = self.skill_name
            )),
        }
    }
}

/// Maximum byte length for any string read from WASM guest memory.
/// Prevents OOM from a malicious guest passing i32::MAX as a length.
const MAX_GUEST_STRING_LEN: usize = 16 * 1024 * 1024; // 16 MiB

/// Tool-invocation attempts allowed per execution — every attempt counts,
/// including denied ones, so a loop of refused calls still terminates.
pub const MAX_TOOL_CALLS: u32 = 32;

/// Largest serialised tool `output` handed into guest memory inline; a larger
/// one is offloaded to the artifact store and handed back as a marker.
pub const MAX_INLINE_RESULT_BYTES: usize = 512 * 1024;

/// Largest tool `output` the runtime will offload. The read-back costs the
/// guest the base64 string, its decoded bytes and the parsed value at once,
/// which at this size stays well inside the 256 MiB guest memory and under
/// [`MAX_READ_ARTIFACT_BYTES`].
pub const MAX_SPILLED_RESULT_BYTES: usize = 12 * 1024 * 1024;

/// `kind` of the marker that replaces an offloaded tool `output`.
pub const SPILLED_RESULT_KIND: &str = "skill-tool-result";

/// HTTP fetch attempts allowed per execution. Together with the per-fetch
/// 30s request timeout this keeps total wall time finite even though the
/// guest's epoch deadline re-arms after each fetch.
pub const MAX_HTTP_FETCHES: u32 = 64;

/// Reserved grant-record key for the per-user tools approval. The leading
/// underscore cannot collide with credential names (which must start with a
/// lowercase letter), and manifest validation additionally rejects a
/// credential literally named `tools` so `cmd: skill grant <skill> tools`
/// stays unambiguous.
pub const TOOLS_GRANT_KEY: &str = "_tools";

/// Validate a length parameter from the WASM guest. Returns false if the
/// length is negative or exceeds the safety bound.
fn valid_guest_len(len: i32) -> bool {
    len >= 0 && (len as usize) <= MAX_GUEST_STRING_LEN
}

/// Helper to convert wasmtime errors to eyre.
fn wt(err: wasmtime::Error) -> eyre::Report {
    eyre::eyre!("{}", err)
}

/// Host-call I/O carries its own timeouts and budgets; re-arm the guest's
/// epoch deadline afterwards so the 30s bounds guest compute, not host wall
/// time. [`MAX_TOOL_CALLS`] / [`MAX_HTTP_FETCHES`] keep the total finite.
fn rearm_execution_deadline(caller: &mut Caller<'_, HostState>) {
    caller
        .as_context_mut()
        .set_epoch_deadline(crate::engine::EXECUTION_TIMEOUT_SECS);
}

/// The platform envelope for a batch element that was refused or failed —
/// mirrors the invoker's success envelope shape so guests branch uniformly.
fn denial_envelope(error: String) -> serde_json::Value {
    serde_json::json!({"success": false, "output": serde_json::Value::Null, "error": error})
}

/// The envelope as the guest receives it: an `output` over
/// [`MAX_INLINE_RESULT_BYTES`] is written to the artifact store under the
/// skill's agent and the ambient session/user, and replaced by a
/// [`SPILLED_RESULT_KIND`] marker carrying the store's preview. Nothing is
/// truncated: a body that cannot be offloaded fails loudly instead.
fn hand_back(
    store: Option<&dyn ArtifactStore>,
    agent_id: &str,
    skill_name: &str,
    tool: &str,
    mut envelope: serde_json::Value,
) -> serde_json::Value {
    let bytes = match envelope.get("output") {
        Some(output) if !output.is_null() => output.to_string().len(),
        _ => return envelope,
    };
    if bytes <= MAX_INLINE_RESULT_BYTES {
        return envelope;
    }
    let refuse = |cause: String| {
        denial_envelope(format!(
            "{tool} result is {bytes} bytes, over the {MAX_INLINE_RESULT_BYTES}-byte inline limit, and {cause}"
        ))
    };
    if bytes > MAX_SPILLED_RESULT_BYTES {
        return refuse(format!(
            "over the {MAX_SPILLED_RESULT_BYTES}-byte offload limit"
        ));
    }
    let Some(store) = store else {
        return refuse("no artifact store is configured to offload it".to_string());
    };
    let descriptor = ArtifactDescriptor::from_context(
        spill_producer(skill_name),
        ContentType::ApplicationJson,
        agent_id,
    );
    let body = ArtifactBody::Json(envelope["output"].take());
    match store.write(agent_id, descriptor, body) {
        Ok(handle) => {
            envelope["output"] = serde_json::json!({
                "kind": SPILLED_RESULT_KIND,
                "artifact_id": handle.id.to_string(),
                "bytes": bytes,
                "mime": ContentType::ApplicationJson.as_str(),
                "preview": handle.preview,
                "ttl_seconds": handle.descriptor.ttl_seconds,
            });
            envelope
        }
        Err(e) => refuse(format!("offloading it failed: {e}")),
    }
}

/// Producer stamped on a skill's offloaded results.
fn spill_producer(skill_name: &str) -> String {
    format!("skill:{skill_name}")
}

/// Whether `artifact_id` is a result the runtime offloaded for this very
/// skill: reading it back is not a new privilege, since the skill already
/// received the data through a granted tool, so it needs no `artifacts`
/// capability.
fn is_own_spill(
    store: &dyn ArtifactStore,
    agent_id: &str,
    skill_name: &str,
    artifact_id: &str,
) -> bool {
    ArtifactId::parse(artifact_id)
        .ok()
        .and_then(|id| store.metadata(agent_id, &id).ok().flatten())
        .is_some_and(|descriptor| descriptor.producer == spill_producer(skill_name))
}

/// [`hand_back`] against the host state of the executing skill.
fn hand_back_from(state: &HostState, tool: &str, envelope: serde_json::Value) -> serde_json::Value {
    hand_back(
        state.artifact_store.as_deref(),
        &state.agent_id,
        &state.skill_name,
        tool,
        envelope,
    )
}

/// Parse a `wf_tool_invoke_many` payload: a non-empty JSON array of
/// `{"tool": name, "args": object}` entries (`args` defaults to `{}`).
/// `None` for anything else — batch shape errors are a guest bug, not a
/// per-element failure.
fn parse_batch_calls(payload: &str) -> Option<Vec<(String, serde_json::Value)>> {
    let entries = serde_json::from_str::<Vec<serde_json::Value>>(payload).ok()?;
    if entries.is_empty() {
        return None;
    }
    entries
        .iter()
        .map(|entry| {
            let name = entry.get("tool")?.as_str()?.to_string();
            let args = entry
                .get("args")
                .cloned()
                .unwrap_or_else(|| serde_json::json!({}));
            args.is_object().then_some((name, args))
        })
        .collect()
}

/// Largest artifact `wf_read_artifact` hands a guest. base64 inflates the
/// payload ~1.33× into the 256 MiB guest heap, so 32 MiB raw stays well clear;
/// a larger artifact is an explicit error rather than a guest OOM trap.
const MAX_READ_ARTIFACT_BYTES: usize = 32 * 1024 * 1024;

/// Read a UTF-8 string argument from guest linear memory, bounded by
/// [`MAX_GUEST_STRING_LEN`]. Backs the artifact host functions.
fn read_guest_string(caller: &mut Caller<'_, HostState>, ptr: i32, len: i32) -> Option<String> {
    if !valid_guest_len(len) {
        return None;
    }
    let memory = caller.get_export("memory").and_then(|e| e.into_memory())?;
    let mut buf = vec![0u8; len as usize];
    memory.read(&*caller, ptr as usize, &mut buf).ok()?;
    String::from_utf8(buf).ok()
}

/// Resolve a staged artifact's bytes for a guest, base64-encoded with its mime
/// and size. Runs under the ambient session/user task-locals the engine
/// re-establishes across `spawn_blocking`, so it enforces the same
/// same-session/same-user access as every other reader-by-id.
fn read_artifact_value(
    store: &dyn ArtifactStore,
    agent_id: &str,
    artifact_id: &str,
) -> Result<serde_json::Value> {
    let resolved = resolve_bytes_input(
        store,
        agent_id,
        BytesRequest {
            artifact_id: Some(artifact_id),
            ..Default::default()
        },
    )?;
    if resolved.bytes.len() > MAX_READ_ARTIFACT_BYTES {
        bail!(
            "artifact {artifact_id} is {} bytes, over the {MAX_READ_ARTIFACT_BYTES}-byte read limit",
            resolved.bytes.len()
        );
    }
    Ok(serde_json::json!({
        "bytes_base64": base64::engine::general_purpose::STANDARD.encode(&resolved.bytes),
        "mime": resolved.mime.as_str(),
        "size": resolved.bytes.len(),
    }))
}

/// Stage guest-produced bytes as an octet-stream artifact and return its
/// delivery marker. `from_context` stamps the ambient session/user so the
/// staged file is itself scoped on any later read; the sanitised filename and
/// media type are stamped too, so a serving route can name the download.
fn stage_artifact_value(
    store: &dyn ArtifactStore,
    agent_id: &str,
    producer: &str,
    bytes: Vec<u8>,
    media_type: &str,
    filename: &str,
) -> Result<serde_json::Value> {
    if bytes.is_empty() {
        bail!("cannot stage an empty artifact");
    }
    let size = bytes.len() as u64;
    let (stem, ext) = filename.rsplit_once('.').unwrap_or((filename, "bin"));
    let safe = download_filename(if stem.is_empty() { "file" } else { stem }, ext);
    let descriptor = ArtifactDescriptor::from_context(producer, ContentType::OctetStream, agent_id)
        .with_ttl(ATTACHMENT_TTL)
        .with_file_meta(safe.clone(), media_type);
    let handle = store.write(agent_id, descriptor, ArtifactBody::Bytes(bytes))?;
    Ok(download_marker(&handle.id, media_type, &safe, size))
}

/// Render a guest's chart request on the host and stage the image. Rendering
/// is async (PNG shells out to `typst`); host functions run on a blocking
/// thread, so the runtime handle drives it to completion here.
fn render_chart_request(
    store: &dyn ArtifactStore,
    agent_id: &str,
    producer: &str,
    payload_json: &str,
) -> Result<serde_json::Value> {
    let payload: serde_json::Value = serde_json::from_str(payload_json)?;
    let chart = chart_request(&payload).map_err(|e| eyre::eyre!(e))?;
    let bytes = tokio::runtime::Handle::current().block_on(draw_chart(&chart, None))?;
    stage_artifact_value(
        store,
        agent_id,
        producer,
        bytes,
        chart.format.media_type(),
        &format!("{}.{}", chart.title, chart.format.extension()),
    )
}

/// Parse a `wf_stage_artifact` guest payload (`{bytes_base64, media_type,
/// filename}`) and stage it. Split from the linker shell so the decode +
/// stage path is unit-testable from a JSON string.
fn stage_request(
    store: &dyn ArtifactStore,
    agent_id: &str,
    producer: &str,
    payload_json: &str,
) -> Result<serde_json::Value> {
    let payload: serde_json::Value = serde_json::from_str(payload_json)?;
    let b64 = payload
        .get("bytes_base64")
        .and_then(|v| v.as_str())
        .ok_or_else(|| eyre::eyre!("stage payload missing bytes_base64"))?;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(b64)
        .map_err(|e| eyre::eyre!("stage payload bytes_base64 invalid: {e}"))?;
    let media_type = payload
        .get("media_type")
        .and_then(|v| v.as_str())
        .unwrap_or("application/octet-stream");
    let filename = payload
        .get("filename")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .unwrap_or("file");
    stage_artifact_value(store, agent_id, producer, bytes, media_type, filename)
}

/// Register all host functions on the linker.
pub fn register_host_functions(linker: &mut Linker<HostState>) -> Result<()> {
    // env::wf_log — write a log message from the guest
    linker
        .func_wrap(
            "env",
            "wf_log",
            |mut caller: Caller<'_, HostState>, level: i32, msg_ptr: i32, msg_len: i32| {
                if !valid_guest_len(msg_len) {
                    return;
                }
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                if let Some(memory) = memory {
                    let mut buf = vec![0u8; msg_len as usize];
                    if memory.read(&caller, msg_ptr as usize, &mut buf).is_ok() {
                        if let Ok(msg) = std::str::from_utf8(&buf) {
                            let skill = &caller.data().skill_name;
                            match level {
                                0 => tracing::error!(skill = %skill, "[wasm-skill] {}", msg),
                                1 => tracing::warn!(skill = %skill, "[wasm-skill] {}", msg),
                                2 => tracing::info!(skill = %skill, "[wasm-skill] {}", msg),
                                _ => tracing::debug!(skill = %skill, "[wasm-skill] {}", msg),
                            }
                        }
                    }
                }
            },
        )
        .map_err(wt)?;

    // env::wf_set_response — guest sets its response JSON
    linker
        .func_wrap(
            "env",
            "wf_set_response",
            |mut caller: Caller<'_, HostState>, ptr: i32, len: i32| {
                if !valid_guest_len(len) {
                    return;
                }
                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                if let Some(memory) = memory {
                    let mut buf = vec![0u8; len as usize];
                    if memory.read(&caller, ptr as usize, &mut buf).is_ok() {
                        if let Ok(s) = String::from_utf8(buf) {
                            caller.data_mut().host_response = Some(s);
                        }
                    }
                }
            },
        )
        .map_err(wt)?;

    // env::wf_get_host_response_len — get length of host response buffer
    linker
        .func_wrap(
            "env",
            "wf_get_host_response_len",
            |caller: Caller<'_, HostState>| -> i32 {
                match &caller.data().host_response {
                    Some(resp) => resp.len() as i32,
                    None => 0,
                }
            },
        )
        .map_err(wt)?;

    // env::wf_get_host_response — guest reads the result from a host call
    linker
        .func_wrap(
            "env",
            "wf_get_host_response",
            |mut caller: Caller<'_, HostState>, buf_ptr: i32, buf_len: i32| -> i32 {
                let response = caller.data().host_response.clone();
                match response {
                    Some(resp) => {
                        let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                        if let Some(memory) = memory {
                            let bytes = resp.as_bytes();
                            let write_len = bytes.len().min(buf_len as usize);
                            if memory
                                .write(&mut caller, buf_ptr as usize, &bytes[..write_len])
                                .is_ok()
                            {
                                write_len as i32
                            } else {
                                -1
                            }
                        } else {
                            -1
                        }
                    }
                    None => 0,
                }
            },
        )
        .map_err(wt)?;

    // env::wf_generate_uuid — generate a UUID v4
    linker
        .func_wrap(
            "env",
            "wf_generate_uuid",
            |mut caller: Caller<'_, HostState>| -> i32 {
                let id = uuid::Uuid::new_v4().to_string();
                caller.data_mut().host_response = Some(id);
                0
            },
        )
        .map_err(wt)?;

    // env::wf_current_time — get current UTC time as RFC3339
    linker
        .func_wrap(
            "env",
            "wf_current_time",
            |mut caller: Caller<'_, HostState>| -> i32 {
                let now = chrono::Utc::now().to_rfc3339();
                caller.data_mut().host_response = Some(now);
                0
            },
        )
        .map_err(wt)?;

    // env::wf_vault_get — read a secret from the vault.
    //
    // The guest passes a bare `key` (e.g. "api_key"); the host computes the
    // full path including the credential's manifest-declared scope. Per-user
    // credentials hard-fail when no `CURRENT_USER_ID` is in scope — the host
    // never silently falls through to a shared bucket.
    linker
        .func_wrap(
            "env",
            "wf_vault_get",
            |mut caller: Caller<'_, HostState>, key_ptr: i32, key_len: i32| -> i32 {
                if !caller.data().require_capability(Capability::Vault) {
                    return -2; // permission denied
                }
                if !valid_guest_len(key_len) {
                    return -1;
                }

                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let memory = match memory {
                    Some(m) => m,
                    None => return -1,
                };

                let mut key_buf = vec![0u8; key_len as usize];
                if memory
                    .read(&caller, key_ptr as usize, &mut key_buf)
                    .is_err()
                {
                    return -1;
                }
                let key = match String::from_utf8(key_buf) {
                    Ok(s) => s,
                    Err(_) => return -1,
                };

                let scoped_path = match caller.data().scoped_vault_path(&key) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            skill = %caller.data().skill_name,
                            tool = %caller.data().tool_name,
                            key = %key,
                            error = ?e,
                            "wf_vault_get rejected: per-user vault path could not be constructed"
                        );
                        return -3;
                    }
                };
                let store = match caller.data().vault_store.as_ref() {
                    Some(s) => Arc::clone(s),
                    None => {
                        tracing::warn!(
                            skill = %caller.data().skill_name,
                            "Vault not configured"
                        );
                        return -1;
                    }
                };

                if let Some(notice) = caller.data().credential_consent_error(store.as_ref(), &key) {
                    tracing::warn!(
                        skill = %caller.data().skill_name,
                        tool = %caller.data().tool_name,
                        key = %key,
                        "wf_vault_get rejected: credential not granted for this contract"
                    );
                    caller.data_mut().policy_notice = Some(notice);
                    return -4;
                }

                match store.get(&scoped_path) {
                    Some(val) => {
                        caller.data_mut().host_response = Some(val);
                        0
                    }
                    None => {
                        caller.data_mut().host_response = None;
                        0
                    }
                }
            },
        )
        .map_err(wt)?;

    // env::wf_vault_get_redacted — read a secret and return it as
    // `abcd…1234` (first 4 / last 4 chars) for use in agent-visible output
    // like "is X configured?" tools. Skills that need cleartext for HTTP
    // headers must use `wf_vault_get` and never embed the result in their
    // `WasmToolResponse`.
    linker
        .func_wrap(
            "env",
            "wf_vault_get_redacted",
            |mut caller: Caller<'_, HostState>, key_ptr: i32, key_len: i32| -> i32 {
                if !caller.data().require_capability(Capability::Vault) {
                    return -2;
                }
                if !valid_guest_len(key_len) {
                    return -1;
                }

                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let memory = match memory {
                    Some(m) => m,
                    None => return -1,
                };

                let mut key_buf = vec![0u8; key_len as usize];
                if memory
                    .read(&caller, key_ptr as usize, &mut key_buf)
                    .is_err()
                {
                    return -1;
                }
                let key = match String::from_utf8(key_buf) {
                    Ok(s) => s,
                    Err(_) => return -1,
                };

                let scoped_path = match caller.data().scoped_vault_path(&key) {
                    Ok(p) => p,
                    Err(_) => return -3,
                };
                let store = match caller.data().vault_store.as_ref() {
                    Some(s) => Arc::clone(s),
                    None => return -1,
                };

                if let Some(notice) = caller.data().credential_consent_error(store.as_ref(), &key) {
                    caller.data_mut().policy_notice = Some(notice);
                    return -4;
                }

                match store.get(&scoped_path) {
                    Some(val) => {
                        caller.data_mut().host_response =
                            Some(workforce_common::redact::redact_value(&val));
                        0
                    }
                    None => {
                        caller.data_mut().host_response = None;
                        0
                    }
                }
            },
        )
        .map_err(wt)?;

    // env::wf_vault_set — write a secret to the vault (scoped to skill)
    linker
        .func_wrap(
            "env",
            "wf_vault_set",
            |mut caller: Caller<'_, HostState>,
             key_ptr: i32,
             key_len: i32,
             val_ptr: i32,
             val_len: i32|
             -> i32 {
                if !caller.data().require_capability(Capability::Vault) {
                    return -2;
                }
                if !valid_guest_len(key_len) || !valid_guest_len(val_len) {
                    return -1;
                }

                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let memory = match memory {
                    Some(m) => m,
                    None => return -1,
                };

                let mut key_buf = vec![0u8; key_len as usize];
                if memory
                    .read(&caller, key_ptr as usize, &mut key_buf)
                    .is_err()
                {
                    return -1;
                }
                let key = match String::from_utf8(key_buf) {
                    Ok(s) => s,
                    Err(_) => return -1,
                };

                let mut val_buf = vec![0u8; val_len as usize];
                if memory
                    .read(&caller, val_ptr as usize, &mut val_buf)
                    .is_err()
                {
                    return -1;
                }
                let val = match String::from_utf8(val_buf) {
                    Ok(s) => s,
                    Err(_) => return -1,
                };

                // A declared credential is read-only to guest code: the value
                // the user consented to stays authoritative, and a skill that
                // "self-provisions" from tool arguments would store whatever
                // text it received — including an unsubstituted secret marker.
                if caller.data().is_declared_credential(&key) {
                    let notice = format!(
                        "Credential '{key}' is declared in Skill.toml, so skill code cannot \
                         write it. The user provisions it with `cmd: secret {key} <value>` \
                         and `cmd: skill grant {skill} {key}`.",
                        skill = caller.data().skill_name
                    );
                    tracing::warn!(
                        skill = %caller.data().skill_name,
                        tool = %caller.data().tool_name,
                        key = %key,
                        "wf_vault_set rejected: declared credentials are platform-provisioned"
                    );
                    caller.data_mut().policy_notice = Some(notice);
                    return -4;
                }

                let scoped_path = match caller.data().scoped_vault_path(&key) {
                    Ok(p) => p,
                    Err(e) => {
                        tracing::warn!(
                            skill = %caller.data().skill_name,
                            tool = %caller.data().tool_name,
                            key = %key,
                            error = ?e,
                            "wf_vault_set rejected: per-user vault path could not be constructed"
                        );
                        return -3;
                    }
                };
                let store = match caller.data().vault_store.as_ref() {
                    Some(s) => Arc::clone(s),
                    None => {
                        tracing::warn!(
                            skill = %caller.data().skill_name,
                            "Vault not configured"
                        );
                        return -1;
                    }
                };

                match store.set(&scoped_path, &val) {
                    Ok(()) => 0,
                    Err(e) => {
                        tracing::error!(
                            skill = %caller.data().skill_name,
                            "[wasm-skill] vault_set error: {}",
                            e
                        );
                        -1
                    }
                }
            },
        )
        .map_err(wt)?;

    // env::wf_store_call — one JSON request against the hub's document
    // store, in the calling agent's (and user's) scope. The reply is the
    // same shape the `store.*` platform tools return; a store error comes
    // back inside it as `{"ok": false, "code", "error"}` so the guest can
    // branch on `version_conflict` or `claimed`. Only a policy denial is a
    // host error (-4, with the notice).
    linker
        .func_wrap(
            "env",
            "wf_store_call",
            |mut caller: Caller<'_, HostState>, req_ptr: i32, req_len: i32| -> i32 {
                if !caller.data().require_capability(Capability::Store) {
                    return -2;
                }
                let Some(raw) = read_guest_string(&mut caller, req_ptr, req_len) else {
                    return -1;
                };
                if raw.contains("[[secret:") || raw.contains("[[vault:") {
                    caller.data_mut().policy_notice = Some(
                        "Store requests may not contain secret or vault markers — the store \
                         is not the vault"
                            .to_string(),
                    );
                    return -4;
                }
                let request = match serde_json::from_str::<serde_json::Value>(&raw) {
                    Ok(v) if v.is_object() => v,
                    _ => return -1,
                };
                let Some(store) = caller.data().document_store.as_ref().map(Arc::clone) else {
                    caller.data_mut().policy_notice =
                        Some("The document store is not available on this runtime".to_string());
                    return -1;
                };
                let ctx = caller.data().skill_call_context();
                let outcome = store.call(&ctx, &request);
                rearm_execution_deadline(&mut caller);
                match outcome {
                    Ok(reply) => {
                        caller.data_mut().host_response = Some(reply.to_string());
                        0
                    }
                    Err(notice) => {
                        tracing::warn!(
                            skill = %caller.data().skill_name,
                            tool = %caller.data().tool_name,
                            "wf_store_call denied"
                        );
                        caller.data_mut().policy_notice = Some(notice);
                        -4
                    }
                }
            },
        )
        .map_err(wt)?;

    // env::wf_config_get — read a skill-scoped config value
    linker
        .func_wrap(
            "env",
            "wf_config_get",
            |mut caller: Caller<'_, HostState>, key_ptr: i32, key_len: i32| -> i32 {
                if !caller.data().require_capability(Capability::Config) {
                    return -2;
                }
                if !valid_guest_len(key_len) {
                    return -1;
                }

                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let memory = match memory {
                    Some(m) => m,
                    None => return -1,
                };

                let mut key_buf = vec![0u8; key_len as usize];
                if memory
                    .read(&caller, key_ptr as usize, &mut key_buf)
                    .is_err()
                {
                    return -1;
                }
                let key = match String::from_utf8(key_buf) {
                    Ok(s) => s,
                    Err(_) => return -1,
                };

                let val = caller.data().config.get(&key).cloned();
                match val {
                    Some(v) => {
                        caller.data_mut().host_response = Some(v);
                        0
                    }
                    None => {
                        caller.data_mut().host_response = None;
                        0
                    }
                }
            },
        )
        .map_err(wt)?;

    // env::wf_tool_invoke — invoke a declared platform tool through the
    // governed dispatch pipeline. The host enforces the manifest declaration,
    // the per-user tools grant, the per-execution budget, and the secret-
    // marker ban; the injected invoker then runs the capability gate under
    // the calling user, the policy engine, and audit before executing.
    linker
        .func_wrap(
            "env",
            "wf_tool_invoke",
            |mut caller: Caller<'_, HostState>,
             name_ptr: i32,
             name_len: i32,
             args_ptr: i32,
             args_len: i32|
             -> i32 {
                if !caller.data().require_capability(Capability::Tools) {
                    return -2;
                }
                let Some(name) = read_guest_string(&mut caller, name_ptr, name_len) else {
                    return -1;
                };
                let Some(args_raw) = read_guest_string(&mut caller, args_ptr, args_len) else {
                    return -1;
                };

                if caller.data().tool_calls_used >= MAX_TOOL_CALLS {
                    caller.data_mut().policy_notice = Some(format!(
                        "Skill '{}' exhausted its tool-call budget ({MAX_TOOL_CALLS} per \
                         invocation)",
                        caller.data().skill_name
                    ));
                    return -5;
                }
                caller.data_mut().tool_calls_used += 1;

                // A guest must never spend session secrets through a tool it
                // chose — markers resolve to cleartext at tool boundaries.
                if args_raw.contains("[[secret:") || args_raw.contains("[[vault:") {
                    caller.data_mut().policy_notice = Some(
                        "Skill tool arguments may not contain secret or vault markers — \
                         a skill uses its own granted credentials, never session secrets"
                            .to_string(),
                    );
                    return -4;
                }
                let args = match serde_json::from_str::<serde_json::Value>(&args_raw) {
                    Ok(v) if v.is_object() => v,
                    _ => return -1,
                };

                let Some(permission) = caller.data().permissions.get(&name).cloned() else {
                    let declared: Vec<&str> = caller
                        .data()
                        .permissions
                        .keys()
                        .map(String::as_str)
                        .collect();
                    caller.data_mut().policy_notice = Some(format!(
                        "Tool '{name}' is not declared in this skill's [[permissions]]. \
                         Declared: [{}]. Add a [[permissions]] entry to Skill.toml, \
                         rebuild, and have the user re-run `cmd: skill grant`.",
                        declared.join(", ")
                    ));
                    return -4;
                };
                if crate::manifest::is_denied_tool(&name) {
                    caller.data_mut().policy_notice = Some(format!(
                        "Tool '{name}' is in a family that can never be granted to a skill"
                    ));
                    return -4;
                }
                let Some(store) = caller.data().vault_store.as_ref().map(Arc::clone) else {
                    return -1;
                };
                if let Some(notice) = caller.data().tool_grant_error(store.as_ref()) {
                    tracing::warn!(
                        skill = %caller.data().skill_name,
                        tool = %name,
                        "wf_tool_invoke rejected: tools grant missing or stale"
                    );
                    caller.data_mut().policy_notice = Some(notice);
                    return -4;
                }
                let Some(invoker) = caller.data().tool_invoker.as_ref().map(Arc::clone) else {
                    caller.data_mut().policy_notice = Some(
                        "Platform tool invocation is not available on this runtime".to_string(),
                    );
                    return -1;
                };

                let ctx = caller.data().skill_call_context();
                let outcome = invoker.invoke(&ctx, &permission, &args);
                rearm_execution_deadline(&mut caller);
                match outcome {
                    Ok(result) => {
                        let handed = hand_back_from(caller.data(), &name, result);
                        caller.data_mut().host_response = Some(handed.to_string());
                        0
                    }
                    Err(notice) => {
                        tracing::warn!(
                            skill = %caller.data().skill_name,
                            tool = %name,
                            "wf_tool_invoke denied by dispatch pipeline"
                        );
                        caller.data_mut().policy_notice = Some(notice);
                        -4
                    }
                }
            },
        )
        .map_err(wt)?;

    // env::wf_tool_invoke_many — invoke a batch of declared platform tools
    // through the governed dispatch pipeline with bounded per-service
    // concurrency. Batch-level failures mirror the single-call codes;
    // per-element denials and failures come back as platform envelopes in
    // order, and the first denial also sets the policy notice so the agent
    // sees the remedy.
    linker
        .func_wrap(
            "env",
            "wf_tool_invoke_many",
            |mut caller: Caller<'_, HostState>, payload_ptr: i32, payload_len: i32| -> i32 {
                if !caller.data().require_capability(Capability::Tools) {
                    return -2;
                }
                let Some(payload_raw) = read_guest_string(&mut caller, payload_ptr, payload_len)
                else {
                    return -1;
                };
                let Some(calls) = parse_batch_calls(&payload_raw) else {
                    return -1;
                };

                // Charge before any refusal, matching the single-call rule:
                // every attempt counts, so a loop of refused batches still
                // exhausts the budget.
                let batch_size = calls.len() as u32;
                if caller.data().tool_calls_used.saturating_add(batch_size) > MAX_TOOL_CALLS {
                    caller.data_mut().policy_notice = Some(format!(
                        "Skill '{}' cannot invoke {batch_size} tools: only {} of its \
                         {MAX_TOOL_CALLS}-call budget remain for this invocation",
                        caller.data().skill_name,
                        MAX_TOOL_CALLS.saturating_sub(caller.data().tool_calls_used),
                    ));
                    return -5;
                }
                caller.data_mut().tool_calls_used += batch_size;

                if payload_raw.contains("[[secret:") || payload_raw.contains("[[vault:") {
                    caller.data_mut().policy_notice = Some(
                        "Skill tool arguments may not contain secret or vault markers — \
                         a skill uses its own granted credentials, never session secrets"
                            .to_string(),
                    );
                    return -4;
                }

                let Some(store) = caller.data().vault_store.as_ref().map(Arc::clone) else {
                    return -1;
                };
                if let Some(notice) = caller.data().tool_grant_error(store.as_ref()) {
                    tracing::warn!(
                        skill = %caller.data().skill_name,
                        batch = batch_size,
                        "wf_tool_invoke_many rejected: tools grant missing or stale"
                    );
                    caller.data_mut().policy_notice = Some(notice);
                    return -4;
                }
                let Some(invoker) = caller.data().tool_invoker.as_ref().map(Arc::clone) else {
                    caller.data_mut().policy_notice = Some(
                        "Platform tool invocation is not available on this runtime".to_string(),
                    );
                    return -1;
                };

                let mut envelopes: Vec<Option<serde_json::Value>> = vec![None; calls.len()];
                let mut first_denial: Option<String> = None;
                let mut deny = |slot: &mut Option<serde_json::Value>, message: String| {
                    if first_denial.is_none() {
                        first_denial = Some(message.clone());
                    }
                    *slot = Some(denial_envelope(message));
                };
                let mut dispatched: Vec<(usize, String)> = Vec::new();
                let mut dispatch_calls: Vec<(ManifestPermission, serde_json::Value)> = Vec::new();
                for (index, (name, args)) in calls.into_iter().enumerate() {
                    match caller.data().permissions.get(&name) {
                        None => deny(
                            &mut envelopes[index],
                            format!(
                                "Tool '{name}' is not declared in this skill's [[permissions]]"
                            ),
                        ),
                        Some(_) if crate::manifest::is_denied_tool(&name) => deny(
                            &mut envelopes[index],
                            format!(
                                "Tool '{name}' is in a family that can never be granted to a \
                                 skill"
                            ),
                        ),
                        Some(permission) => {
                            dispatched.push((index, name));
                            dispatch_calls.push((permission.clone(), args));
                        }
                    }
                }

                let ctx = caller.data().skill_call_context();
                let outcomes = invoker.invoke_many(&ctx, dispatch_calls);
                rearm_execution_deadline(&mut caller);
                for ((index, tool), outcome) in dispatched.into_iter().zip(outcomes) {
                    match outcome {
                        Ok(envelope) => {
                            envelopes[index] = Some(hand_back_from(caller.data(), &tool, envelope));
                        }
                        Err(message) => deny(&mut envelopes[index], message),
                    }
                }

                if let Some(notice) = first_denial {
                    caller.data_mut().policy_notice = Some(notice);
                }
                // Positional contract with the guest: a slot an invoker
                // failed to answer becomes a loud failure envelope, never a
                // silent shift.
                let response = serde_json::Value::Array(
                    envelopes
                        .into_iter()
                        .map(|slot| {
                            slot.unwrap_or_else(|| {
                                denial_envelope(
                                    "the tool invoker returned no result for this element"
                                        .to_string(),
                                )
                            })
                        })
                        .collect(),
                );
                caller.data_mut().host_response = Some(response.to_string());
                0
            },
        )
        .map_err(wt)?;

    // env::wf_http_fetch — make an outbound HTTP request
    linker
        .func_wrap(
            "env",
            "wf_http_fetch",
            |mut caller: Caller<'_, HostState>,
             method_ptr: i32,
             method_len: i32,
             url_ptr: i32,
             url_len: i32,
             headers_ptr: i32,
             headers_len: i32,
             body_ptr: i32,
             body_len: i32|
             -> i32 {
                if !caller.data().require_capability(Capability::Http) {
                    return -2;
                }
                if !valid_guest_len(method_len)
                    || !valid_guest_len(url_len)
                    || !valid_guest_len(headers_len)
                    || !valid_guest_len(body_len)
                {
                    return -1;
                }
                if caller.data().http_fetches_used >= MAX_HTTP_FETCHES {
                    caller.data_mut().policy_notice = Some(format!(
                        "Skill '{}' exhausted its HTTP budget ({MAX_HTTP_FETCHES} fetches \
                         per invocation)",
                        caller.data().skill_name
                    ));
                    return -5;
                }
                caller.data_mut().http_fetches_used += 1;

                let memory = caller.get_export("memory").and_then(|e| e.into_memory());
                let memory = match memory {
                    Some(m) => m,
                    None => return -1,
                };

                // Read method
                let mut method_buf = vec![0u8; method_len as usize];
                if memory
                    .read(&caller, method_ptr as usize, &mut method_buf)
                    .is_err()
                {
                    return -1;
                }
                let method = match String::from_utf8(method_buf) {
                    Ok(s) => s,
                    Err(_) => return -1,
                };

                // Read URL
                let mut url_buf = vec![0u8; url_len as usize];
                if memory
                    .read(&caller, url_ptr as usize, &mut url_buf)
                    .is_err()
                {
                    return -1;
                }
                let url = match String::from_utf8(url_buf) {
                    Ok(s) => s,
                    Err(_) => return -1,
                };

                // Enforce manifest's [network] allowed_hosts. An empty
                // allowlist means "no outbound HTTP allowed" — a skill
                // that needs network access must declare its hosts.
                let parsed_url = match reqwest::Url::parse(&url) {
                    Ok(u) => u,
                    Err(_) => {
                        tracing::warn!(
                            skill = %caller.data().skill_name,
                            url = %url,
                            "wf_http_fetch rejected: malformed URL"
                        );
                        return -1;
                    }
                };
                let host = match parsed_url.host_str().map(|h| h.to_string()) {
                    Some(h) => h,
                    None => {
                        tracing::warn!(
                            skill = %caller.data().skill_name,
                            url = %url,
                            "wf_http_fetch rejected: URL has no host"
                        );
                        return -1;
                    }
                };
                if !crate::manifest::host_matches_allowlist(&caller.data().allowed_hosts, &host) {
                    tracing::warn!(
                        skill = %caller.data().skill_name,
                        tool = %caller.data().tool_name,
                        host = %host,
                        "wf_http_fetch rejected: host not in manifest [network] allowed_hosts"
                    );
                    let notice = format!(
                        "Skill '{}' may not reach host '{host}': it is not in the \
                         Skill.toml [network] allowed_hosts list. Add it there and \
                         rebuild with wasm_skill.build.",
                        caller.data().skill_name
                    );
                    caller.data_mut().policy_notice = Some(notice);
                    return -2;
                }

                // SSRF guard (audit W1): the manifest allowlist is skill-authored,
                // so it is not a boundary against a malicious skill. Resolve the
                // host and reject any internal/metadata address, then pin the
                // connection to the vetted addresses so a second DNS answer can't
                // rebind to an internal IP. Redirects are disabled on the client
                // below so a public host can't 30x us into the metadata endpoint.
                let port = parsed_url.port_or_known_default().unwrap_or(443);
                let resolved: Vec<std::net::SocketAddr> =
                    match std::net::ToSocketAddrs::to_socket_addrs(&(host.as_str(), port)) {
                        Ok(addrs) => addrs.collect(),
                        Err(_) => {
                            tracing::warn!(
                                skill = %caller.data().skill_name,
                                host = %host,
                                "wf_http_fetch rejected: DNS resolution failed"
                            );
                            return -2;
                        }
                    };
                if resolved.is_empty()
                    || resolved
                        .iter()
                        .any(|a| crate::manifest::ip_is_disallowed(a.ip()))
                {
                    tracing::warn!(
                        skill = %caller.data().skill_name,
                        host = %host,
                        "wf_http_fetch rejected: host resolves to a disallowed/internal address"
                    );
                    return -2;
                }
                let http_client = match reqwest::Client::builder()
                    .redirect(reqwest::redirect::Policy::none())
                    .resolve_to_addrs(&host, &resolved)
                    .build()
                {
                    Ok(c) => c,
                    Err(e) => {
                        tracing::warn!(
                            skill = %caller.data().skill_name,
                            error = %e,
                            "wf_http_fetch rejected: HTTP client build failed"
                        );
                        return -1;
                    }
                };

                // Read headers JSON
                let mut headers_buf = vec![0u8; headers_len as usize];
                if memory
                    .read(&caller, headers_ptr as usize, &mut headers_buf)
                    .is_err()
                {
                    return -1;
                }
                let headers_json = match String::from_utf8(headers_buf) {
                    Ok(s) => s,
                    Err(_) => return -1,
                };
                let headers: HashMap<String, String> =
                    serde_json::from_str(&headers_json).unwrap_or_default();

                // Read body
                let body = if body_len > 0 {
                    let mut body_buf = vec![0u8; body_len as usize];
                    if memory
                        .read(&caller, body_ptr as usize, &mut body_buf)
                        .is_err()
                    {
                        return -1;
                    }
                    Some(body_buf)
                } else {
                    None
                };

                let result = std::thread::scope(|_| {
                    let rt = tokio::runtime::Handle::current();
                    rt.block_on(async {
                        let client = http_client;
                        let mut req_builder = match method.to_uppercase().as_str() {
                            "GET" => client.get(&url),
                            "POST" => client.post(&url),
                            "PUT" => client.put(&url),
                            "DELETE" => client.delete(&url),
                            "PATCH" => client.patch(&url),
                            "HEAD" => client.head(&url),
                            _ => return Err(eyre::eyre!("Unsupported HTTP method: {}", method)),
                        };

                        for (key, value) in &headers {
                            req_builder = req_builder.header(key, value);
                        }

                        if let Some(body_bytes) = body {
                            req_builder = req_builder.body(body_bytes);
                        }

                        let response = req_builder
                            .timeout(std::time::Duration::from_secs(30))
                            .send()
                            .await?;
                        let status = response.status().as_u16() as i32;
                        let resp_headers: HashMap<String, String> = response
                            .headers()
                            .iter()
                            .map(|(k, v)| {
                                (k.as_str().to_string(), v.to_str().unwrap_or("").to_string())
                            })
                            .collect();

                        let content_type = resp_headers
                            .get("content-type")
                            .map(|s| s.as_str())
                            .unwrap_or("");
                        let is_binary = content_type.starts_with("image/")
                            || content_type.starts_with("audio/")
                            || content_type.starts_with("video/")
                            || content_type == "application/octet-stream"
                            || content_type == "application/pdf";

                        let (resp_body, body_encoding) = if is_binary {
                            use base64::Engine;
                            let bytes = response.bytes().await?;
                            (
                                base64::engine::general_purpose::STANDARD.encode(&bytes),
                                "base64",
                            )
                        } else {
                            (response.text().await?, "utf8")
                        };

                        let response_obj = serde_json::json!({
                            "status": status,
                            "body": resp_body,
                            "body_encoding": body_encoding,
                            "headers": resp_headers,
                        });

                        Ok::<String, eyre::Report>(serde_json::to_string(&response_obj)?)
                    })
                });

                rearm_execution_deadline(&mut caller);
                match result {
                    Ok(json_str) => {
                        caller.data_mut().host_response = Some(json_str);
                        0
                    }
                    Err(e) => {
                        tracing::error!(
                            skill = %caller.data().skill_name,
                            "[wasm-skill] http_fetch error: {}",
                            e
                        );
                        -1
                    }
                }
            },
        )
        .map_err(wt)?;

    // env::wf_read_artifact — read a staged artifact's bytes by id
    linker
        .func_wrap(
            "env",
            "wf_read_artifact",
            |mut caller: Caller<'_, HostState>, id_ptr: i32, id_len: i32| -> i32 {
                let Some(artifact_id) = read_guest_string(&mut caller, id_ptr, id_len) else {
                    return -1;
                };
                let Some(store) = caller.data().artifact_store.clone() else {
                    tracing::warn!(
                        skill = %caller.data().skill_name,
                        "wf_read_artifact: no artifact store configured"
                    );
                    return -1;
                };
                let agent_id = caller.data().agent_id.clone();
                let own_spill = is_own_spill(
                    store.as_ref(),
                    &agent_id,
                    &caller.data().skill_name,
                    &artifact_id,
                );
                if !own_spill && !caller.data().require_capability(Capability::Artifacts) {
                    return -2;
                }
                match read_artifact_value(store.as_ref(), &agent_id, &artifact_id) {
                    Ok(value) => match serde_json::to_string(&value) {
                        Ok(json) => {
                            caller.data_mut().host_response = Some(json);
                            0
                        }
                        Err(_) => {
                            caller.data_mut().host_response = None;
                            -1
                        }
                    },
                    Err(e) => {
                        tracing::warn!(
                            skill = %caller.data().skill_name,
                            tool = %caller.data().tool_name,
                            artifact = %artifact_id,
                            error = %e,
                            "wf_read_artifact failed"
                        );
                        caller.data_mut().host_response = None;
                        -1
                    }
                }
            },
        )
        .map_err(wt)?;

    // env::wf_render_chart — draw a wf-chart spec on the host and stage the image
    linker
        .func_wrap(
            "env",
            "wf_render_chart",
            |mut caller: Caller<'_, HostState>, payload_ptr: i32, payload_len: i32| -> i32 {
                if !caller.data().require_capability(Capability::Artifacts) {
                    return -2;
                }
                let Some(payload) = read_guest_string(&mut caller, payload_ptr, payload_len) else {
                    return -1;
                };
                let Some(store) = caller.data().artifact_store.clone() else {
                    tracing::warn!(
                        skill = %caller.data().skill_name,
                        "wf_render_chart: no artifact store configured"
                    );
                    return -1;
                };
                let agent_id = caller.data().agent_id.clone();
                let producer = format!("wasm-skill:{}", caller.data().tool_name);
                match render_chart_request(store.as_ref(), &agent_id, &producer, &payload) {
                    Ok(value) => match serde_json::to_string(&value) {
                        Ok(json) => {
                            caller.data_mut().host_response = Some(json);
                            0
                        }
                        Err(_) => {
                            caller.data_mut().host_response = None;
                            -1
                        }
                    },
                    Err(e) => {
                        tracing::warn!(
                            skill = %caller.data().skill_name,
                            tool = %caller.data().tool_name,
                            error = %e,
                            "wf_render_chart failed"
                        );
                        caller.data_mut().host_response = None;
                        -1
                    }
                }
            },
        )
        .map_err(wt)?;

    // env::wf_stage_artifact — stage guest bytes as a deliverable artifact
    linker
        .func_wrap(
            "env",
            "wf_stage_artifact",
            |mut caller: Caller<'_, HostState>, payload_ptr: i32, payload_len: i32| -> i32 {
                if !caller.data().require_capability(Capability::Artifacts) {
                    return -2;
                }
                let Some(payload) = read_guest_string(&mut caller, payload_ptr, payload_len) else {
                    return -1;
                };
                let Some(store) = caller.data().artifact_store.clone() else {
                    tracing::warn!(
                        skill = %caller.data().skill_name,
                        "wf_stage_artifact: no artifact store configured"
                    );
                    return -1;
                };
                let agent_id = caller.data().agent_id.clone();
                let producer = format!("wasm-skill:{}", caller.data().tool_name);
                match stage_request(store.as_ref(), &agent_id, &producer, &payload) {
                    Ok(value) => match serde_json::to_string(&value) {
                        Ok(json) => {
                            caller.data_mut().host_response = Some(json);
                            0
                        }
                        Err(_) => {
                            caller.data_mut().host_response = None;
                            -1
                        }
                    },
                    Err(e) => {
                        tracing::warn!(
                            skill = %caller.data().skill_name,
                            tool = %caller.data().tool_name,
                            error = %e,
                            "wf_stage_artifact failed"
                        );
                        caller.data_mut().host_response = None;
                        -1
                    }
                }
            },
        )
        .map_err(wt)?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host_state(user_id: Option<&str>, scopes: &[(&str, CredentialScope)]) -> HostState {
        HostState {
            skill_name: "skill".into(),
            capabilities: HashSet::new(),
            tool_name: "tool".into(),
            agent_id: "agent".into(),
            user_id: user_id.map(str::to_string),
            vault_store: None,
            credential_scopes: scopes.iter().map(|(k, s)| (k.to_string(), *s)).collect(),
            allowed_hosts: Vec::new(),
            grant_fingerprint: "fp-current".into(),
            contract_fingerprint: "contract-current".into(),
            permissions: HashMap::new(),
            tool_invoker: None,
            tool_calls_used: 0,
            http_fetches_used: 0,
            config: HashMap::new(),
            artifact_store: None,
            document_store: None,
            host_response: None,
            policy_notice: None,
            store_limits: wasmtime::StoreLimitsBuilder::new().build(),
        }
    }

    struct MapStore(parking_lot::Mutex<HashMap<String, String>>);

    impl MapStore {
        fn with(entries: &[(&str, &str)]) -> Self {
            Self(parking_lot::Mutex::new(
                entries
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            ))
        }
    }

    impl SecretStore for MapStore {
        fn get(&self, path: &str) -> Option<String> {
            self.0.lock().get(path).cloned()
        }
        fn set(&self, path: &str, value: &str) -> eyre::Result<()> {
            self.0.lock().insert(path.to_string(), value.to_string());
            Ok(())
        }
        fn delete(&self, path: &str) -> eyre::Result<bool> {
            Ok(self.0.lock().remove(path).is_some())
        }
        fn list(&self, prefix: Option<&str>) -> Vec<String> {
            self.0
                .lock()
                .keys()
                .filter(|k| prefix.is_none_or(|p| k.starts_with(p)))
                .cloned()
                .collect()
        }
    }

    fn grant_json(fingerprint: &str) -> String {
        serde_json::json!({ "fingerprint": fingerprint }).to_string()
    }

    fn dual_grant_json(fingerprint: &str, contract: &str) -> String {
        serde_json::json!({ "fingerprint": fingerprint, "contract_fingerprint": contract })
            .to_string()
    }

    #[test]
    fn contract_matched_grant_survives_a_pure_logic_rebuild() {
        let state = host_state(Some("alice"), &[("api_key", CredentialScope::User)]);
        let store = MapStore::with(&[(
            "wasm-skills/skill/grants/users/alice/api_key",
            &dual_grant_json("fp-previous-build", "contract-current"),
        )]);
        assert_eq!(state.credential_consent_error(&store, "api_key"), None);
    }

    #[test]
    fn contract_change_stales_a_dual_field_grant() {
        let state = host_state(Some("alice"), &[("api_key", CredentialScope::User)]);
        let store = MapStore::with(&[(
            "wasm-skills/skill/grants/users/alice/api_key",
            &dual_grant_json("fp-current", "contract-previous"),
        )]);
        let msg = state.credential_consent_error(&store, "api_key").unwrap();
        assert!(msg.contains("consent contract"));
        assert!(msg.contains("cmd: skill grant skill api_key"));
    }

    #[test]
    fn tools_grant_survives_rebuild_when_contract_matches() {
        let state = host_state(Some("alice"), &[]);
        let store = MapStore::with(&[(
            "wasm-skills/skill/grants/users/alice/_tools",
            &dual_grant_json("fp-previous", "contract-current"),
        )]);
        assert_eq!(state.tool_grant_error(&store), None);
    }

    #[test]
    fn valid_legacy_grant_is_migrated_in_place_and_only_once() {
        let state = host_state(Some("alice"), &[("api_key", CredentialScope::User)]);
        let path = "wasm-skills/skill/grants/users/alice/api_key";
        let store = MapStore::with(&[(path, &grant_json("fp-current"))]);

        assert_eq!(state.credential_consent_error(&store, "api_key"), None);
        let migrated = store.get(path).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(&migrated).unwrap();
        assert_eq!(parsed["fingerprint"], "fp-current");
        assert_eq!(parsed["contract_fingerprint"], "contract-current");
        assert!(parsed["migrated_at"].is_string());

        // A second validation leaves the migrated record untouched.
        assert_eq!(state.credential_consent_error(&store, "api_key"), None);
        assert_eq!(store.get(path).unwrap(), migrated);

        // The migrated record now survives a pure-logic rebuild.
        let rebuilt = HostState {
            grant_fingerprint: "fp-after-rebuild".into(),
            ..host_state(Some("alice"), &[("api_key", CredentialScope::User)])
        };
        assert_eq!(rebuilt.credential_consent_error(&store, "api_key"), None);
    }

    #[test]
    fn valid_legacy_shared_tools_grant_migrates_for_every_user() {
        let state = host_state(Some("alice"), &[]);
        let shared_path = "wasm-skills/skill/grants/shared/_tools";
        let store = MapStore::with(&[(shared_path, &grant_json("fp-current"))]);

        assert_eq!(state.tool_grant_error(&store), None);
        let migrated: serde_json::Value =
            serde_json::from_str(&store.get(shared_path).unwrap()).unwrap();
        assert_eq!(migrated["contract_fingerprint"], "contract-current");

        // The org-wide record now covers a different user across a rebuild.
        let bob_after_rebuild = HostState {
            user_id: Some("bob".into()),
            grant_fingerprint: "fp-after-rebuild".into(),
            ..host_state(Some("bob"), &[])
        };
        assert_eq!(bob_after_rebuild.tool_grant_error(&store), None);
    }

    #[test]
    fn stale_and_corrupt_grants_are_never_rewritten() {
        let state = host_state(Some("alice"), &[("api_key", CredentialScope::User)]);
        for record in [grant_json("fp-previous-build"), "not-json".to_string()] {
            let path = "wasm-skills/skill/grants/users/alice/api_key";
            let store = MapStore::with(&[(path, &record)]);
            assert!(state.credential_consent_error(&store, "api_key").is_some());
            assert_eq!(store.get(path).unwrap(), record);
        }
    }

    #[test]
    fn tools_grant_remedies_name_the_stale_scope() {
        let state = host_state(Some("alice"), &[]);
        let user_path = "wasm-skills/skill/grants/users/alice/_tools";
        let shared_path = "wasm-skills/skill/grants/shared/_tools";
        let stale = grant_json("fp-previous");

        let user_only = MapStore::with(&[(user_path, &stale)]);
        let msg = state.tool_grant_error(&user_only).unwrap();
        assert!(msg.starts_with("This user's tools grant"), "{msg}");
        assert!(!msg.contains("tools shared"), "{msg}");

        let shared_only = MapStore::with(&[(shared_path, &stale)]);
        let msg = state.tool_grant_error(&shared_only).unwrap();
        assert!(msg.starts_with("The org-wide shared tools grant"), "{msg}");
        assert!(msg.contains("tools shared"), "{msg}");

        let both = MapStore::with(&[(user_path, &stale), (shared_path, &stale)]);
        let msg = state.tool_grant_error(&both).unwrap();
        assert!(msg.starts_with("Both this user's tools grant"), "{msg}");
        assert!(msg.contains("tools shared"), "{msg}");
    }

    #[test]
    fn batch_calls_parse_and_reject_malformed_shapes() {
        let calls =
            parse_batch_calls(r#"[{"tool": "a.b", "args": {"x": 1}}, {"tool": "c.d"}]"#).unwrap();
        assert_eq!(calls.len(), 2);
        assert_eq!(calls[0].0, "a.b");
        assert_eq!(calls[0].1["x"], 1);
        assert_eq!(calls[1], ("c.d".to_string(), serde_json::json!({})));

        for bad in [
            "not-json",
            "[]",
            r#"{"tool": "a.b"}"#,
            r#"[{"args": {}}]"#,
            r#"[{"tool": 7}]"#,
            r#"[{"tool": "a.b", "args": [1]}]"#,
        ] {
            assert!(parse_batch_calls(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn undeclared_key_needs_no_consent() {
        let state = host_state(Some("alice"), &[]);
        let store = MapStore::with(&[]);
        assert_eq!(state.credential_consent_error(&store, "scratch"), None);
    }

    #[test]
    fn declared_key_without_grant_is_refused_with_recipe() {
        let state = host_state(Some("alice"), &[("api_key", CredentialScope::User)]);
        let store = MapStore::with(&[]);
        let msg = state.credential_consent_error(&store, "api_key").unwrap();
        assert!(msg.contains("cmd: secret api_key"));
        assert!(msg.contains("cmd: skill grant skill api_key"));
    }

    #[test]
    fn declared_key_with_matching_grant_resolves() {
        let state = host_state(Some("alice"), &[("api_key", CredentialScope::User)]);
        let store = MapStore::with(&[(
            "wasm-skills/skill/grants/users/alice/api_key",
            &grant_json("fp-current"),
        )]);
        assert_eq!(state.credential_consent_error(&store, "api_key"), None);
    }

    #[test]
    fn stale_grant_after_rebuild_is_refused() {
        let state = host_state(Some("alice"), &[("api_key", CredentialScope::User)]);
        let store = MapStore::with(&[(
            "wasm-skills/skill/grants/users/alice/api_key",
            &grant_json("fp-previous-build"),
        )]);
        let msg = state.credential_consent_error(&store, "api_key").unwrap();
        assert!(msg.contains("stale"));
        assert!(msg.contains("cmd: skill grant skill api_key"));
    }

    #[test]
    fn corrupt_grant_record_fails_closed() {
        let state = host_state(Some("alice"), &[("api_key", CredentialScope::User)]);
        let store = MapStore::with(&[("wasm-skills/skill/grants/users/alice/api_key", "not-json")]);
        assert!(state.credential_consent_error(&store, "api_key").is_some());
    }

    #[test]
    fn tools_grant_missing_stale_and_valid() {
        let state = host_state(Some("alice"), &[]);
        let store = MapStore::with(&[]);
        let msg = state.tool_grant_error(&store).unwrap();
        assert!(msg.contains("cmd: skill grant skill tools"));

        store
            .set(
                "wasm-skills/skill/grants/users/alice/_tools",
                &grant_json("fp-previous"),
            )
            .unwrap();
        let msg = state.tool_grant_error(&store).unwrap();
        assert!(msg.contains("stale"));

        store
            .set(
                "wasm-skills/skill/grants/users/alice/_tools",
                &grant_json("fp-current"),
            )
            .unwrap();
        assert_eq!(state.tool_grant_error(&store), None);
    }

    #[test]
    fn tools_grant_without_identity_fails_closed() {
        let state = host_state(None, &[]);
        let store = MapStore::with(&[
            (
                "wasm-skills/skill/grants/users/alice/_tools",
                &grant_json("fp-current"),
            ),
            (
                "wasm-skills/skill/grants/shared/_tools",
                &grant_json("fp-current"),
            ),
        ]);
        assert!(state.tool_grant_error(&store).is_some());
    }

    #[test]
    fn shared_tools_grant_covers_a_user_without_their_own() {
        let state = host_state(Some("alice"), &[]);
        let store = MapStore::with(&[(
            "wasm-skills/skill/grants/shared/_tools",
            &grant_json("fp-current"),
        )]);
        assert_eq!(state.tool_grant_error(&store), None);
    }

    #[test]
    fn stale_shared_grant_is_refused_and_names_both_routes() {
        let state = host_state(Some("alice"), &[]);
        let store = MapStore::with(&[(
            "wasm-skills/skill/grants/shared/_tools",
            &grant_json("fp-previous"),
        )]);
        let msg = state.tool_grant_error(&store).unwrap();
        assert!(msg.contains("stale"));
        assert!(msg.contains("tools shared"));
    }

    #[test]
    fn a_current_user_grant_wins_over_a_stale_shared_one() {
        let state = host_state(Some("alice"), &[]);
        let store = MapStore::with(&[
            (
                "wasm-skills/skill/grants/users/alice/_tools",
                &grant_json("fp-current"),
            ),
            (
                "wasm-skills/skill/grants/shared/_tools",
                &grant_json("fp-previous"),
            ),
        ]);
        assert_eq!(state.tool_grant_error(&store), None);
    }

    #[test]
    fn shared_scope_grant_lives_under_shared_grants_path() {
        let state = host_state(None, &[("token", CredentialScope::Shared)]);
        let store = MapStore::with(&[(
            "wasm-skills/skill/grants/shared/token",
            &grant_json("fp-current"),
        )]);
        assert_eq!(state.credential_consent_error(&store, "token"), None);
        assert_eq!(
            state.grant_record_path("token").unwrap(),
            "wasm-skills/skill/grants/shared/token"
        );
    }

    #[test]
    fn scoped_vault_path_user_default() {
        let state = host_state(Some("alice"), &[]);
        assert_eq!(
            state.scoped_vault_path("api_key").unwrap(),
            "wasm-skills/skill/users/alice/api_key"
        );
    }

    #[test]
    fn scoped_vault_path_shared_when_declared() {
        let state = host_state(Some("alice"), &[("api_key", CredentialScope::Shared)]);
        assert_eq!(
            state.scoped_vault_path("api_key").unwrap(),
            "wasm-skills/skill/shared/api_key"
        );
    }

    #[test]
    fn scoped_vault_path_user_without_identity_fails() {
        let state = host_state(None, &[]);
        assert_eq!(
            state.scoped_vault_path("api_key"),
            Err(VaultPathError::NoUserIdentity)
        );
    }

    #[test]
    fn scoped_vault_path_shared_without_identity_succeeds() {
        let state = host_state(None, &[("api_key", CredentialScope::Shared)]);
        assert!(state.scoped_vault_path("api_key").is_ok());
    }

    #[test]
    fn scoped_vault_path_rejects_unsafe_user_id() {
        for unsafe_uid in ["", "..", ".", "alice/bob", "back\\slash", "nul\0byte"] {
            let state = host_state(Some(unsafe_uid), &[]);
            assert_eq!(
                state.scoped_vault_path("api_key"),
                Err(VaultPathError::UnsafeUserId),
                "uid {unsafe_uid:?} should be rejected"
            );
        }
    }

    #[test]
    fn is_safe_path_segment_classification() {
        assert!(is_safe_path_segment("alice"));
        assert!(is_safe_path_segment("user-123"));
        assert!(is_safe_path_segment("UUID_xyz"));
        assert!(!is_safe_path_segment(""));
        assert!(!is_safe_path_segment(".."));
        assert!(!is_safe_path_segment("."));
        assert!(!is_safe_path_segment("a/b"));
        assert!(!is_safe_path_segment("a\\b"));
        assert!(!is_safe_path_segment("a\0b"));
    }

    // ── artifact bridge ───────────────────────────────────────────────

    use workforce_agent::artifacts::{ArtifactId, FsArtifactStore};

    fn temp_store() -> (Arc<dyn ArtifactStore>, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().unwrap();
        let store: Arc<dyn ArtifactStore> = FsArtifactStore::new(dir.path().to_path_buf()).unwrap();
        (store, dir)
    }

    fn put_bytes(store: &dyn ArtifactStore, agent: &str, bytes: Vec<u8>) -> ArtifactId {
        let descriptor = ArtifactDescriptor::from_context("test", ContentType::OctetStream, agent);
        store
            .write(agent, descriptor, ArtifactBody::Bytes(bytes))
            .unwrap()
            .id
    }

    fn decode(b64: &str) -> Vec<u8> {
        base64::engine::general_purpose::STANDARD
            .decode(b64)
            .unwrap()
    }

    #[test]
    fn read_artifact_round_trips_bytes_mime_size() {
        let (store, _g) = temp_store();
        let id = put_bytes(store.as_ref(), "agent-a", b"PK\x03\x04xlsx-bytes".to_vec());
        let value = read_artifact_value(store.as_ref(), "agent-a", id.as_str()).unwrap();

        assert_eq!(value["size"], 14);
        assert_eq!(value["mime"], "application/octet-stream");
        assert_eq!(
            decode(value["bytes_base64"].as_str().unwrap()),
            b"PK\x03\x04xlsx-bytes"
        );
    }

    fn envelope_with(output: serde_json::Value) -> serde_json::Value {
        serde_json::json!({ "success": true, "output": output, "error": null })
    }

    fn big_output(rows: usize) -> serde_json::Value {
        serde_json::Value::Array(
            (0..rows)
                .map(|i| serde_json::json!({ "row": i, "text": "x".repeat(1000) }))
                .collect(),
        )
    }

    #[test]
    fn a_small_result_is_handed_back_unchanged() {
        let (store, _g) = temp_store();
        let envelope = envelope_with(serde_json::json!({ "hello": "world" }));
        let handed = hand_back(
            Some(store.as_ref()),
            "agent-a",
            "s",
            "jira.search",
            envelope.clone(),
        );
        assert_eq!(handed, envelope);
        let failure = denial_envelope("nope".into());
        assert_eq!(
            hand_back(None, "agent-a", "s", "jira.search", failure.clone()),
            failure
        );
    }

    #[test]
    fn an_oversized_result_is_offloaded_and_reads_back_whole() {
        let (store, _g) = temp_store();
        let output = big_output(1000);
        let handed = hand_back(
            Some(store.as_ref()),
            "agent-a",
            "fieldops",
            "jira.api",
            envelope_with(output.clone()),
        );

        assert_eq!(handed["success"], true);
        let marker = &handed["output"];
        assert_eq!(marker["kind"], SPILLED_RESULT_KIND);
        assert_eq!(marker["mime"], "application/json");
        assert_eq!(marker["bytes"], output.to_string().len());
        assert!(marker["preview"]["rows"]
            .as_array()
            .is_some_and(|rows| !rows.is_empty()));
        assert_eq!(marker["ttl_seconds"], 3600);

        let id = marker["artifact_id"].as_str().unwrap();
        let read = read_artifact_value(store.as_ref(), "agent-a", id).unwrap();
        assert_eq!(read["mime"], "application/json");
        let body: serde_json::Value =
            serde_json::from_slice(&decode(read["bytes_base64"].as_str().unwrap())).unwrap();
        assert_eq!(body, output);
        assert!(is_own_spill(store.as_ref(), "agent-a", "fieldops", id));
        assert!(!is_own_spill(store.as_ref(), "agent-a", "other", id));
        assert!(!is_own_spill(
            store.as_ref(),
            "agent-a",
            "fieldops",
            "art_missing"
        ));
    }

    #[test]
    fn an_offloaded_result_is_readable_only_from_the_session_that_produced_it() {
        use workforce_agent::runner::CURRENT_SESSION_ID;

        let (store, _g) = temp_store();
        let handed = CURRENT_SESSION_ID.sync_scope("sess-a".to_string(), || {
            hand_back(
                Some(store.as_ref()),
                "agent-a",
                "s",
                "jira.api",
                envelope_with(big_output(1000)),
            )
        });
        let id = handed["output"]["artifact_id"]
            .as_str()
            .unwrap()
            .to_string();
        let denied = CURRENT_SESSION_ID.sync_scope("sess-b".to_string(), || {
            read_artifact_value(store.as_ref(), "agent-a", &id)
        });
        assert!(denied.is_err());
        let allowed = CURRENT_SESSION_ID.sync_scope("sess-a".to_string(), || {
            read_artifact_value(store.as_ref(), "agent-a", &id)
        });
        assert!(allowed.is_ok());
    }

    #[test]
    fn an_oversized_result_fails_loudly_when_it_cannot_be_offloaded() {
        let handed = hand_back(
            None,
            "agent-a",
            "s",
            "jira.api",
            envelope_with(big_output(1000)),
        );
        assert_eq!(handed["success"], false);
        let error = handed["error"].as_str().unwrap();
        assert!(
            error.contains("jira.api result is") && error.contains("no artifact store"),
            "{error}"
        );

        let (store, _g) = temp_store();
        let huge = serde_json::json!({ "blob": "y".repeat(MAX_SPILLED_RESULT_BYTES + 1) });
        let handed = hand_back(
            Some(store.as_ref()),
            "agent-a",
            "s",
            "jira.api",
            envelope_with(huge),
        );
        assert_eq!(handed["success"], false);
        assert!(handed["error"].as_str().unwrap().contains("offload limit"));
    }

    #[test]
    fn read_artifact_missing_id_errors() {
        let (store, _g) = temp_store();
        assert!(
            read_artifact_value(store.as_ref(), "agent-a", ArtifactId::fresh().as_str()).is_err()
        );
    }

    #[test]
    fn read_artifact_over_limit_errors() {
        let (store, _g) = temp_store();
        let id = put_bytes(
            store.as_ref(),
            "agent-a",
            vec![0u8; MAX_READ_ARTIFACT_BYTES + 1],
        );
        let err = read_artifact_value(store.as_ref(), "agent-a", id.as_str()).unwrap_err();
        assert!(err.to_string().contains("read limit"));
    }

    #[test]
    fn stage_request_persists_bytes_and_returns_marker() {
        let (store, _g) = temp_store();
        let payload = serde_json::json!({
            "bytes_base64": base64::engine::general_purpose::STANDARD.encode(b"reshaped"),
            "media_type": "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet",
            "filename": "weird name!.xlsx",
        })
        .to_string();

        let marker =
            stage_request(store.as_ref(), "agent-a", "wasm-skill:x.reshape", &payload).unwrap();
        assert_eq!(marker["bytes"], 8);
        assert_eq!(
            marker["media_type"],
            "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet"
        );
        assert_eq!(marker["filename"], "weird-name-.xlsx");

        let id = ArtifactId::parse(marker["artifact_id"].as_str().unwrap()).unwrap();
        match store.read("agent-a", &id).unwrap() {
            ArtifactBody::Bytes(b) => assert_eq!(b, b"reshaped"),
            other => panic!("expected bytes, got {other:?}"),
        }
    }

    #[test]
    fn stage_request_stamps_filename_and_media_type_on_the_descriptor() {
        let (store, _g) = temp_store();
        let payload = serde_json::json!({
            "bytes_base64": base64::engine::general_purpose::STANDARD.encode(b"x"),
            "media_type": "text/csv",
            "filename": "out.csv",
        })
        .to_string();
        let marker = stage_request(store.as_ref(), "agent-a", "p", &payload).unwrap();
        let id = ArtifactId::parse(marker["artifact_id"].as_str().unwrap()).unwrap();
        let descriptor = store.metadata("agent-a", &id).unwrap().expect("stored");
        assert_eq!(descriptor.filename.as_deref(), Some("out.csv"));
        assert_eq!(descriptor.media_type.as_deref(), Some("text/csv"));
    }

    #[test]
    fn render_chart_request_rejects_an_invalid_payload_before_drawing() {
        let (store, _g) = temp_store();
        let err = render_chart_request(store.as_ref(), "agent-a", "p", "not json").unwrap_err();
        assert!(err.to_string().contains("expected"), "{err}");
        let err = render_chart_request(
            store.as_ref(),
            "agent-a",
            "p",
            &serde_json::json!({ "spec": { "kind": "donut" } }).to_string(),
        )
        .unwrap_err();
        assert!(err.to_string().contains("kind must be one of"), "{err}");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn render_chart_request_stages_an_svg_marker() {
        let (store, _g) = temp_store();
        let payload = serde_json::json!({
            "spec": { "kind": "bar", "title": "Signups", "labels": ["a"], "series": [{ "data": [1] }] },
            "format": "svg"
        })
        .to_string();
        let (marker, store) = tokio::task::spawn_blocking(move || {
            render_chart_request(store.as_ref(), "agent-a", "wasm-skill:x.chart", &payload)
                .map(|marker| (marker, store))
        })
        .await
        .unwrap()
        .unwrap();
        assert_eq!(marker["media_type"], "image/svg+xml");
        assert_eq!(marker["filename"], "Signups.svg");
        let id = ArtifactId::parse(marker["artifact_id"].as_str().unwrap()).unwrap();
        match store.read("agent-a", &id).unwrap() {
            ArtifactBody::Bytes(b) => assert!(b.starts_with(b"<svg")),
            other => panic!("expected bytes, got {other:?}"),
        }
    }

    #[test]
    fn stage_request_rejects_empty_bytes() {
        let (store, _g) = temp_store();
        let payload = serde_json::json!({ "bytes_base64": "", "filename": "f.bin" }).to_string();
        assert!(stage_request(store.as_ref(), "agent-a", "p", &payload).is_err());
    }

    #[test]
    fn stage_request_rejects_invalid_base64() {
        let (store, _g) = temp_store();
        let payload = serde_json::json!({ "bytes_base64": "not base64!!!" }).to_string();
        assert!(stage_request(store.as_ref(), "agent-a", "p", &payload).is_err());
    }

    #[test]
    fn stage_request_sanitises_separator_in_filename_extension() {
        let (store, _g) = temp_store();
        let payload = serde_json::json!({
            "bytes_base64": base64::engine::general_purpose::STANDARD.encode(b"x"),
            "filename": "report.x/../../y",
        })
        .to_string();
        let marker = stage_request(store.as_ref(), "agent-a", "p", &payload).unwrap();
        let filename = marker["filename"].as_str().unwrap();
        assert!(
            !filename.contains(['/', '\\']),
            "guest-supplied extension must be sanitised: {filename}"
        );
        assert!(filename.starts_with("report"));
    }

    #[test]
    fn artifact_read_is_session_scoped() {
        use workforce_agent::runner::CURRENT_SESSION_ID;

        let (store, _g) = temp_store();
        // Stage under session A so the descriptor is stamped with it.
        let payload = serde_json::json!({
            "bytes_base64": base64::engine::general_purpose::STANDARD.encode(b"secret"),
            "filename": "a.bin",
        })
        .to_string();
        let marker = CURRENT_SESSION_ID.sync_scope("sess-a".to_string(), || {
            stage_request(store.as_ref(), "agent-shared", "p", &payload).unwrap()
        });
        let id = marker["artifact_id"].as_str().unwrap().to_string();

        // Session B is denied; session A succeeds. An agent-scope-only impl
        // would let B read A's artifact and fail this test.
        let denied = CURRENT_SESSION_ID.sync_scope("sess-b".to_string(), || {
            read_artifact_value(store.as_ref(), "agent-shared", &id)
        });
        assert!(denied.is_err());
        let allowed = CURRENT_SESSION_ID.sync_scope("sess-a".to_string(), || {
            read_artifact_value(store.as_ref(), "agent-shared", &id)
        });
        assert!(allowed.is_ok());
    }

    #[test]
    fn artifact_read_is_user_scoped() {
        use workforce_agent::runner::CURRENT_USER_ID;

        let (store, _g) = temp_store();
        let payload = serde_json::json!({
            "bytes_base64": base64::engine::general_purpose::STANDARD.encode(b"secret"),
            "filename": "a.bin",
        })
        .to_string();
        // Stage as user A; the descriptor is stamped with that user.
        let marker = CURRENT_USER_ID.sync_scope("user-a".to_string(), || {
            stage_request(store.as_ref(), "agent-shared", "p", &payload).unwrap()
        });
        let id = marker["artifact_id"].as_str().unwrap().to_string();

        // A different user on the same shared agent is denied; the owner is not.
        let denied = CURRENT_USER_ID.sync_scope("user-b".to_string(), || {
            read_artifact_value(store.as_ref(), "agent-shared", &id)
        });
        assert!(denied.is_err());
        let allowed = CURRENT_USER_ID.sync_scope("user-a".to_string(), || {
            read_artifact_value(store.as_ref(), "agent-shared", &id)
        });
        assert!(allowed.is_ok());
    }
}
