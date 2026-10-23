//! WASM skill engine — compiles, caches, and executes skill modules.
//!
//! Adapted from orbit's WasmEngine but tailored for workforce's skill system.
//! Modules are identified by `(skill_name, binary_version)` and cached in memory.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use eyre::{bail, Result};
use tokio::sync::RwLock;
use tracing::{info, info_span};
use wasmtime::*;
use workforce_agent::artifacts::ArtifactStore;
use workforce_agent::runner::SessionContext;
use workforce_common::settings::SecretStore;

use crate::host::{self, HostState};
use crate::manifest::SkillManifest;
use crate::types::{Capability, CredentialScope, WasmToolRequest, WasmToolResponse};

/// A compiled WASM module cached in memory.
#[derive(Clone)]
struct CompiledSkillModule {
    module: Module,
    /// Length of the WASM source bytes this module was compiled from.
    /// Tracked here so eviction can keep `cache_source_bytes` in sync.
    source_size: u64,
    /// SHA-256 of the source bytes — half of the credential-consent
    /// fingerprint ([`crate::types::grant_fingerprint`]).
    source_sha256: String,
}

/// The WASM skill engine — manages module compilation, caching, and execution.
#[derive(Clone)]
pub struct WasmSkillEngine {
    engine: Engine,
    /// `(skill_name, version)` -> compiled module.
    modules: Arc<RwLock<HashMap<(String, u32), CompiledSkillModule>>>,
    /// Running total of WASM source bytes loaded into the module cache.
    /// Decremented when modules are removed.
    cache_source_bytes: Arc<AtomicU64>,
}

/// Snapshot of the WASM module cache, for memory observability.
#[derive(Debug, Clone, Copy, Default)]
pub struct WasmCacheStats {
    pub modules: u64,
    /// Sum of source `wasm_bytes.len()` for every cached module. This is
    /// not the on-heap `wasmtime::Module` size (which is larger and engine-
    /// specific), but a stable proxy that grows linearly with what's cached.
    pub source_bytes: u64,
}

/// Maximum execution time for a single WASM handler call.
pub(crate) const EXECUTION_TIMEOUT_SECS: u64 = 30;

/// Maximum size for request JSON written into WASM guest memory (16 MiB).
const MAX_REQUEST_SIZE: usize = 16 * 1024 * 1024;

/// Per-instance linear-memory cap for a WASM skill (256 MiB). Bounds guest
/// `memory.grow` so a single skill can't OOM the host (audit W3). The 30s
/// epoch deadline only bounds CPU, not allocation.
const MAX_GUEST_MEMORY_BYTES: usize = 256 * 1024 * 1024;

impl WasmSkillEngine {
    /// Create a new WASM skill engine.
    ///
    /// Enables epoch-based interruption and spawns a background tokio task
    /// that increments the engine's epoch counter once per second. WASM
    /// handlers are given a deadline of [`EXECUTION_TIMEOUT_SECS`] epoch
    /// ticks — if a handler runs longer, the engine traps it.
    pub fn new() -> Result<Self> {
        let mut config = Config::new();
        config.wasm_component_model(false);
        config.async_support(false);
        config.epoch_interruption(true);
        // wasmtime's macOS mach exception thread aborts the process when a
        // signal interrupts it; this has no effect elsewhere.
        config.macos_use_mach_ports(false);

        let engine =
            Engine::new(&config).map_err(|e| eyre::eyre!("Wasmtime engine init: {}", e))?;

        // Spawn a background task that increments the epoch once per second.
        // This drives the timeout mechanism: a store with
        // set_epoch_deadline(30) will trap after ~30 seconds of execution.
        let engine_for_ticker = engine.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(1));
            loop {
                interval.tick().await;
                engine_for_ticker.increment_epoch();
            }
        });

        Ok(Self {
            engine,
            modules: Arc::new(RwLock::new(HashMap::new())),
            cache_source_bytes: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Snapshot the module cache for memory observability.
    pub async fn cache_stats(&self) -> WasmCacheStats {
        WasmCacheStats {
            modules: self.modules.read().await.len() as u64,
            source_bytes: self.cache_source_bytes.load(Ordering::Relaxed),
        }
    }

    /// Compile and cache a WASM binary for a skill.
    ///
    /// **Version replacement (F7, 2026-05-15).** Installing `(skill, v)`
    /// evicts every other cached entry for the same `skill_name` —
    /// `(skill, v-1)`, `(skill, v-2)`, etc. Operator intent for "install
    /// the new version" is "the old one is dead code"; without eviction,
    /// repeated version churn grew the cache unboundedly. In-flight
    /// `execute_tool` calls hold their own `Module` clone, so evicting a
    /// stale entry does not interrupt running executions.
    pub async fn load_module(
        &self,
        skill_name: &str,
        version: u32,
        wasm_bytes: &[u8],
    ) -> Result<()> {
        let module = Module::new(&self.engine, wasm_bytes).map_err(|e| {
            eyre::eyre!(
                "Failed to compile WASM for skill '{}' v{}: {}",
                skill_name,
                version,
                e
            )
        })?;

        let source_size = wasm_bytes.len() as u64;
        let compiled = CompiledSkillModule {
            module,
            source_size,
            source_sha256: crate::store::sha256_hex(wasm_bytes),
        };

        let mut modules = self.modules.write().await;

        // Evict any prior versions of the same skill.
        let stale_keys: Vec<(String, u32)> = modules
            .keys()
            .filter(|(name, v)| name == skill_name && *v != version)
            .cloned()
            .collect();
        let mut evicted_bytes: u64 = 0;
        let mut evicted_versions: Vec<u32> = Vec::new();
        for key in stale_keys {
            if let Some(old) = modules.remove(&key) {
                evicted_bytes += old.source_size;
                evicted_versions.push(key.1);
            }
        }

        let previous = modules.insert((skill_name.to_string(), version), compiled);
        drop(modules);

        if let Some(old) = previous {
            evicted_bytes += old.source_size;
        }
        if evicted_bytes > 0 {
            self.cache_source_bytes
                .fetch_sub(evicted_bytes, Ordering::Relaxed);
        }
        self.cache_source_bytes
            .fetch_add(source_size, Ordering::Relaxed);

        info!(
            skill = %skill_name,
            version = version,
            size_bytes = wasm_bytes.len(),
            evicted_versions = ?evicted_versions,
            "WASM skill module compiled and cached"
        );

        Ok(())
    }

    /// Check if a module is cached.
    pub async fn has_module(&self, skill_name: &str, version: u32) -> bool {
        self.modules
            .read()
            .await
            .contains_key(&(skill_name.to_string(), version))
    }

    /// Remove a cached module.
    pub async fn remove_module(&self, skill_name: &str, version: u32) {
        if let Some(removed) = self
            .modules
            .write()
            .await
            .remove(&(skill_name.to_string(), version))
        {
            self.cache_source_bytes
                .fetch_sub(removed.source_size, Ordering::Relaxed);
        }
    }

    /// Execute a tool handler in a WASM skill module.
    ///
    /// This is the core execution path: create a fresh store + linker per call,
    /// instantiate the module, serialise the request into WASM memory, call the
    /// handler, and read the response back.
    /// `vault_store` and `tool_invoker` are per-call because they are
    /// per-agent (each agent's skill tools carry its own locker and dispatch
    /// bridge) while the engine — a compile cache — is shared process-wide.
    pub async fn execute_tool(
        &self,
        skill_name: &str,
        version: u32,
        manifest: &SkillManifest,
        request: &WasmToolRequest,
        config: &HashMap<String, String>,
        vault_store: Option<Arc<dyn SecretStore>>,
        tool_invoker: Option<Arc<dyn crate::types::SkillToolInvoker>>,
    ) -> Result<WasmToolResponse> {
        let modules = self.modules.read().await;
        let compiled = modules
            .get(&(skill_name.to_string(), version))
            .ok_or_else(|| {
                eyre::eyre!(
                    "WASM module for skill '{}' v{} not loaded",
                    skill_name,
                    version
                )
            })?;

        // The process-global artifact store backs `wf_read_artifact` /
        // `wf_stage_artifact`; `None` when one isn't installed.
        let artifact_store = workforce_agent::artifacts::global_store();
        // Capture identity here, in the async tool task where the `CURRENT_*`
        // task-locals are applied; `sync_scope` re-applies them across the
        // `spawn_blocking` boundary below (task-locals don't cross it), which is
        // what keeps artifact session/user scoping correct inside the guest.
        let session_ctx = SessionContext::capture();

        let module = compiled.module.clone();
        let grant_fingerprint = crate::types::grant_fingerprint(&compiled.source_sha256, manifest)?;
        // The execution below holds its own Module clone; releasing the map
        // guard here keeps a long-running guest (tool retries can sleep for
        // minutes) from queueing a `load_module` write — and, the lock being
        // fair, every other skill execution behind it.
        drop(modules);
        let contract_fingerprint = crate::types::contract_fingerprint(manifest);
        let engine = self.engine.clone();
        let capabilities: HashSet<Capability> = manifest.capabilities.clone();
        let skill_name_owned = skill_name.to_string();
        let tool_name = request.tool_name.clone();
        let agent_id = request.agent_id.clone();
        let user_id = request.user_id.clone();
        let handler_name = request.handler.clone();
        let request_json = serde_json::to_string(request)?;
        let config = config.clone();

        // Build the per-credential scope map from the manifest. Keys not
        // declared default to `User` at lookup time in `host.rs`.
        let credential_scopes: HashMap<String, CredentialScope> = manifest
            .credentials
            .iter()
            .map(|c| (c.name.clone(), c.scope))
            .collect();
        let allowed_hosts = manifest.allowed_hosts.clone();
        let permissions: HashMap<String, crate::manifest::ManifestPermission> = manifest
            .permissions
            .iter()
            .map(|p| (p.tool.clone(), p.clone()))
            .collect();

        // WASM execution is synchronous (no async support in wasmtime config).
        // Run it on a blocking thread to avoid blocking the tokio runtime.
        let response = tokio::task::spawn_blocking(move || {
            session_ctx.sync_scope(|| {
                execute_handler_sync(
                    &engine,
                    &module,
                    &skill_name_owned,
                    &tool_name,
                    &agent_id,
                    user_id.as_deref(),
                    &handler_name,
                    &request_json,
                    capabilities,
                    vault_store,
                    artifact_store,
                    credential_scopes,
                    allowed_hosts,
                    grant_fingerprint,
                    contract_fingerprint,
                    permissions,
                    tool_invoker,
                    config,
                )
            })
        })
        .await
        .map_err(|e| eyre::eyre!("WASM execution task panicked: {}", e))??;

        Ok(response)
    }

    /// Get the number of cached modules.
    pub async fn module_count(&self) -> usize {
        self.modules.read().await.len()
    }
}

/// Synchronous handler execution on a blocking thread.
#[allow(clippy::too_many_arguments)]
fn execute_handler_sync(
    engine: &Engine,
    module: &Module,
    skill_name: &str,
    tool_name: &str,
    agent_id: &str,
    user_id: Option<&str>,
    handler_name: &str,
    request_json: &str,
    capabilities: HashSet<Capability>,
    vault_store: Option<Arc<dyn SecretStore>>,
    artifact_store: Option<Arc<dyn ArtifactStore>>,
    credential_scopes: HashMap<String, CredentialScope>,
    allowed_hosts: Vec<String>,
    grant_fingerprint: String,
    contract_fingerprint: String,
    permissions: HashMap<String, crate::manifest::ManifestPermission>,
    tool_invoker: Option<Arc<dyn crate::types::SkillToolInvoker>>,
    config: HashMap<String, String>,
) -> Result<WasmToolResponse> {
    let _span = info_span!(
        "wasm_skill_execute",
        skill = %skill_name,
        tool = %tool_name,
        handler = %handler_name,
    )
    .entered();

    let start = Instant::now();

    let host_state = HostState {
        skill_name: skill_name.to_string(),
        capabilities,
        tool_name: tool_name.to_string(),
        agent_id: agent_id.to_string(),
        user_id: user_id.map(|s| s.to_string()),
        vault_store,
        credential_scopes,
        allowed_hosts,
        grant_fingerprint,
        contract_fingerprint,
        permissions,
        tool_invoker,
        tool_calls_used: 0,
        http_fetches_used: 0,
        config,
        artifact_store,
        document_store: crate::types::skill_document_store(),
        host_response: None,
        policy_notice: None,
        store_limits: wasmtime::StoreLimitsBuilder::new()
            .memory_size(MAX_GUEST_MEMORY_BYTES)
            .build(),
    };

    let mut store = Store::new(engine, host_state);

    // audit W3: enforce the per-instance memory cap so a guest `memory.grow`
    // can't exhaust host RAM. (Epoch deadline below bounds CPU, not memory.)
    store.limiter(|s| &mut s.store_limits);

    // Set execution timeout. The background epoch ticker increments once
    // per second, so the deadline in epoch ticks equals seconds.
    store.set_epoch_deadline(EXECUTION_TIMEOUT_SECS);

    let mut linker = Linker::new(engine);
    host::register_host_functions(&mut linker)?;

    let instance = linker
        .instantiate(&mut store, module)
        .map_err(|e| eyre::eyre!("WASM instantiation failed: {}", e))?;

    // Get guest memory.
    let memory = instance
        .get_memory(&mut store, "memory")
        .ok_or_else(|| eyre::eyre!("WASM module has no exported memory"))?;

    // Get the guest allocator.
    let alloc = instance
        .get_typed_func::<i32, i32>(&mut store, "alloc")
        .or_else(|_| instance.get_typed_func::<i32, i32>(&mut store, "__alloc"))
        .map_err(|e| eyre::eyre!("WASM module has no alloc function: {}", e))?;

    // Allocate space for the request JSON in guest memory.
    let request_bytes = request_json.as_bytes();
    if request_bytes.len() > MAX_REQUEST_SIZE {
        bail!(
            "Request JSON too large ({} bytes, max {})",
            request_bytes.len(),
            MAX_REQUEST_SIZE
        );
    }
    let request_len = request_bytes.len() as i32;
    let request_ptr = alloc
        .call(&mut store, request_len)
        .map_err(|e| eyre::eyre!("alloc failed: {}", e))?;

    // Write request into guest memory.
    memory.write(&mut store, request_ptr as usize, request_bytes)?;

    // Call the handler: handler(ptr, len) -> response_ptr
    let handler_fn = instance
        .get_typed_func::<(i32, i32), i32>(&mut store, handler_name)
        .map_err(|_| {
            eyre::eyre!(
                "Handler '{}' not found in WASM module for skill '{}'",
                handler_name,
                skill_name
            )
        })?;

    let response_ptr = match handler_fn.call(&mut store, (request_ptr, request_len)) {
        Ok(ptr) => ptr,
        Err(e) => {
            // A guest that panics on a policy denial would otherwise swallow
            // the remedy — carry it into the trap error.
            let notice = store
                .data_mut()
                .policy_notice
                .take()
                .map(|n| format!(" — {n}"))
                .unwrap_or_default();
            bail!(
                "WASM handler '{}' in skill '{}' trapped: {}{}",
                handler_name,
                skill_name,
                e,
                notice
            );
        }
    };

    // Read response from guest memory: first 4 bytes = length, then data.
    let mut len_buf = [0u8; 4];
    memory.read(&store, response_ptr as usize, &mut len_buf)?;
    let response_len = u32::from_le_bytes(len_buf) as usize;

    if response_len == 0 {
        bail!(
            "WASM handler '{}' in skill '{}' returned empty response",
            handler_name,
            skill_name
        );
    }

    // Guard against unreasonable response sizes (16 MiB).
    if response_len > 16 * 1024 * 1024 {
        bail!(
            "WASM handler '{}' in skill '{}' returned oversized response ({} bytes)",
            handler_name,
            skill_name,
            response_len
        );
    }

    let mut response_buf = vec![0u8; response_len];
    memory.read(&store, (response_ptr as usize) + 4, &mut response_buf)?;

    let response_str = String::from_utf8(response_buf)
        .map_err(|_| eyre::eyre!("WASM handler returned invalid UTF-8"))?;

    let elapsed = start.elapsed();
    tracing::debug!(
        skill = %skill_name,
        handler = %handler_name,
        duration_ms = elapsed.as_millis() as u64,
        "WASM handler execution complete"
    );

    // Parse the response. The guest writes a WasmToolResponse JSON.
    let mut response: WasmToolResponse = serde_json::from_str(&response_str).map_err(|e| {
        eyre::eyre!(
            "WASM handler '{}' returned invalid JSON: {} (raw: {})",
            handler_name,
            e,
            &response_str[..response_str.len().min(200)]
        )
    })?;
    // Host-authoritative: overwrite whatever the guest put there, so a skill
    // can never fabricate a platform notice.
    response.policy_notice = store.data_mut().policy_notice.take();

    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn engine_creation() {
        let engine = WasmSkillEngine::new().unwrap();
        assert_eq!(engine.module_count().await, 0);
    }

    #[tokio::test]
    async fn load_invalid_wasm_fails() {
        let engine = WasmSkillEngine::new().unwrap();
        let result = engine.load_module("bad-skill", 1, b"not valid wasm").await;
        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Failed to compile"));
    }

    #[tokio::test]
    async fn has_module_after_load() {
        let engine = WasmSkillEngine::new().unwrap();
        assert!(!engine.has_module("test", 1).await);

        // We can't easily create valid WASM in a unit test without a toolchain,
        // so we just verify the API works for the negative case.
    }

    #[tokio::test]
    async fn remove_module() {
        let engine = WasmSkillEngine::new().unwrap();
        // Remove nonexistent is a no-op
        engine.remove_module("test", 1).await;
        assert_eq!(engine.module_count().await, 0);
    }

    /// Minimal valid WASM binary: magic + version header, no sections.
    /// Wasmtime compiles this as an empty module — enough to exercise the
    /// cache plumbing without dragging in a WAT toolchain at test time.
    const EMPTY_WASM: &[u8] = &[
        0x00, 0x61, 0x73, 0x6d, // magic: \0asm
        0x01, 0x00, 0x00, 0x00, // version: 1
    ];

    // ── F7: version-replacement eviction ──────────────────────────────

    #[tokio::test]
    async fn installing_new_version_evicts_old() {
        let engine = WasmSkillEngine::new().unwrap();
        engine.load_module("jira", 1, EMPTY_WASM).await.unwrap();
        engine.load_module("jira", 2, EMPTY_WASM).await.unwrap();

        assert_eq!(engine.module_count().await, 1);
        assert!(!engine.has_module("jira", 1).await);
        assert!(engine.has_module("jira", 2).await);
        // Source-bytes accounting reflects exactly one cached module.
        assert_eq!(
            engine.cache_stats().await.source_bytes,
            EMPTY_WASM.len() as u64
        );
    }

    #[tokio::test]
    async fn different_skills_coexist() {
        let engine = WasmSkillEngine::new().unwrap();
        engine.load_module("jira", 1, EMPTY_WASM).await.unwrap();
        engine.load_module("slack", 1, EMPTY_WASM).await.unwrap();

        assert_eq!(engine.module_count().await, 2);
        assert!(engine.has_module("jira", 1).await);
        assert!(engine.has_module("slack", 1).await);
        assert_eq!(
            engine.cache_stats().await.source_bytes,
            (EMPTY_WASM.len() * 2) as u64
        );
    }

    #[tokio::test]
    async fn reinstalling_same_version_keeps_one_entry() {
        let engine = WasmSkillEngine::new().unwrap();
        engine.load_module("jira", 1, EMPTY_WASM).await.unwrap();
        // Same key — existing replacement path (still desired).
        engine.load_module("jira", 1, EMPTY_WASM).await.unwrap();

        assert_eq!(engine.module_count().await, 1);
        assert_eq!(
            engine.cache_stats().await.source_bytes,
            EMPTY_WASM.len() as u64
        );
    }

    #[tokio::test]
    async fn many_versions_collapse_to_latest() {
        let engine = WasmSkillEngine::new().unwrap();
        for v in 1..=10 {
            engine.load_module("jira", v, EMPTY_WASM).await.unwrap();
        }
        // Only v10 survives.
        assert_eq!(engine.module_count().await, 1);
        assert!(engine.has_module("jira", 10).await);
        for v in 1..=9 {
            assert!(!engine.has_module("jira", v).await);
        }
        assert_eq!(
            engine.cache_stats().await.source_bytes,
            EMPTY_WASM.len() as u64
        );
    }

    #[tokio::test]
    async fn execute_without_loaded_module_fails() {
        let engine = WasmSkillEngine::new().unwrap();
        let manifest = SkillManifest {
            name: "test".into(),
            version: "1.0.0".into(),
            description: "test".into(),
            author: String::new(),
            license: String::new(),
            min_workforce_version: None,
            capabilities: HashSet::new(),
            tools: vec![],
            credentials: vec![],
            config_schema: vec![],
            allowed_hosts: vec![],
            permissions: vec![],
        };

        let request = WasmToolRequest {
            tool_name: "test.ping".into(),
            handler: "ping".into(),
            input: serde_json::json!({}),
            agent_id: "agent-1".into(),
            user_id: None,
            task_id: None,
        };

        let result = engine
            .execute_tool("test", 1, &manifest, &request, &HashMap::new(), None, None)
            .await;
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("not loaded"));
    }
}
