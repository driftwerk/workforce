//! The worker profile: its tools, model tier, rounds and prompt.

use crate::serve::{CURATION_TOOL_PATTERNS, WORKER_EXCLUDED_READS};

/// Pattern allowlist for a curation registry: [`CURATION_TOOL_PATTERNS`] plus
/// every compiled-in adapter's read-only tools (minus [`WORKER_EXCLUDED_READS`]),
/// so metabase/jira/etc. reads are reachable — but without the `memory`/`notes`/
/// `context.*` write-and-recall surface a worker carries.
pub(crate) fn curation_tool_patterns() -> Vec<String> {
    CURATION_TOOL_PATTERNS
        .iter()
        .copied()
        .chain(
            workforce_skills::adapters::read_only_adapter_tools()
                .into_iter()
                .filter(|t| !WORKER_EXCLUDED_READS.contains(t)),
        )
        .map(str::to_string)
        .collect()
}

/// Whether a built-in adapter contributes read-only tools a worker may use —
/// i.e. at least one of its static tool names is in
/// [`read_only_adapter_tools`](workforce_skills::adapters::read_only_adapter_tools)
/// and therefore survives [`worker_tool_patterns`]. Decides which configured
/// adapters [`AgentService::ensure_worker_adapters_loaded_as`] pre-loads before a
/// worker's registry is snapshotted. Deliberately keyed on the read-only
/// *service* adapter set (linear/jira/datadog/metabase/postmark/sendgrid), not the full
/// worker allowlist: heavyweight or non-service tools the worker patterns also
/// permit (e.g. `browser.*`) stay deferred rather than being eagerly spun up
/// for every research worker.
pub(crate) fn adapter_contributes_worker_reads(skill: &str) -> bool {
    let worker_reads: std::collections::HashSet<&str> =
        workforce_skills::adapters::read_only_adapter_tools()
            .into_iter()
            .collect();
    crate::skill_loader::static_skill_tool_names(skill)
        .iter()
        .any(|t| worker_reads.contains(t))
}

/// Model tier a worker sub-agent defaults to.
pub(crate) const WORKER_MODEL_TIER: &str = "cheap";

/// Round cap for a worker sub-agent — enough for a few rounds of research plus
/// recording findings, without letting one worker run away.
pub(crate) const WORKER_MAX_ROUNDS: u32 = 16;

/// Evict old tool results every N rounds in a Curator turn (keep last 3).
pub(crate) const CURATOR_EVICTION_INTERVAL: u32 = 8;

/// System prompt for a worker sub-agent, replacing the full enhanced identity.
pub(crate) const WORKER_SYSTEM_PROMPT: &str = concat!(
    "You are a focused research worker, spawned to investigate a single task in isolation. Use the read-only tools available to you to gather evidence — be economical: a handful of targeted searches/reads, not an exhaustive crawl. As you find things, call `report_findings` to record each finding — a claim, your confidence from 0 to 100, and its citations — these are collated into a queryable artifact for the agent that spawned you. A citation is ",
    crate::findings::citation_forms!(),
    ". A finding you cannot cite is held back as uncited and is not reported as fact; if `report_findings` refuses a citation, record the claim again with one it accepts. Record findings as you go rather than saving them for the end, and finish with a concise summary. Work autonomously: do not ask questions or wait for input, and do not attempt to message anyone or take any action beyond investigation — you have read-only access by design."
);

/// Appended to [`WORKER_SYSTEM_PROMPT`] only when `[worker_read_sources]` is
/// enabled, so a worker is told about `read_source.*` exactly when it has them.
pub(crate) const WORKER_READ_SOURCES_HINT: &str = " If a task needs to read from a system not in your current toolset (e.g. AWS, Slack, GitHub, Google Drive, Notion), call `read_source.list` to see what's available in this workspace, then `read_source.enable` with that source name to load its read-only tools — they become callable on your next step.";
