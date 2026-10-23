//! Learning plan file system.
//!
//! Learning plans are stored as individual markdown files with TOML
//! frontmatter under `{workspace}/learning/`, the same `+++`-delimited
//! shape as heartbeat checks. Each file defines a topic the agent should
//! research on a schedule, the named read-only sources to research it
//! under, and the identity whose credentials those sources resolve from.
//!
//! File format:
//! ```markdown
//! +++
//! name = "Product & billing knowledge"
//! enabled = true
//! schedule = "0 6 * * 1"
//! run_as_identity = "<identity-uuid>"
//! priority = 100
//!
//! [[sources]]
//! skill = "notion"
//! scope = "Billing, Pricing"
//! +++
//!
//! ...objectives prompt for the agent...
//! ```

use std::fs;
use std::path::{Path, PathBuf};

use eyre::{bail, eyre, Result};
use serde::{Deserialize, Serialize};

use crate::heartbeat_check::split_toml_frontmatter;

fn default_true() -> bool {
    true
}

/// What a learning plan's curation turn produces. `DeepExpertise` (the default)
/// deepens a hierarchical knowledge tree node-by-node from real source
/// (`deep_expertise.record_node`); `ExperientialExpertise` deepens the same tree
/// from the agent's own recent session transcripts, distilling operational
/// lessons cited to `session_id:turn_index`; `Items` is the legacy flat,
/// atomic-fact path (`self_learning.record`). Absent in a plan file ⇒
/// [`CurationMode::DeepExpertise`].
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CurationMode {
    Items,
    #[default]
    DeepExpertise,
    ExperientialExpertise,
}

/// A read-only source a learning plan researches under. `instance` pins
/// a multi-instance skill to one environment/workspace; `scope` is a
/// free-text hint handed to the agent.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LearningSource {
    pub skill: String,
    #[serde(default)]
    pub instance: Option<String>,
    #[serde(default)]
    pub scope: Option<String>,
}

/// TOML frontmatter of a learning plan file.
///
/// `sources` must remain the last field: it serialises as a TOML
/// array-of-tables (`[[sources]]`), and `toml::to_string` rejects a
/// scalar field appearing after a table-typed one.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LearningPlanMeta {
    /// Human-readable name.
    pub name: String,
    /// Whether this plan is enabled.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Cron expression for scheduling. Mutually-alternative with `interval_secs`.
    #[serde(default)]
    pub schedule: Option<String>,
    /// Interval-based scheduling alternative to `schedule`, in seconds.
    #[serde(default)]
    pub interval_secs: Option<u64>,
    /// Identity whose credentials the plan's sources resolve under.
    /// `None` ⇒ the plan never runs (no authorised reader).
    #[serde(default)]
    pub run_as_identity: Option<String>,
    /// Priority: lower number = higher priority.
    #[serde(default)]
    pub priority: i32,
    /// Per-plan budget guard on items recorded per run; falls back to the
    /// global cap when omitted.
    #[serde(default)]
    pub max_items_per_run: Option<u32>,
    /// Default TTL (days) for items this plan produces.
    #[serde(default)]
    pub freshness_days: Option<u32>,
    /// Per-plan curation model, overriding `[self_learning] model`. A tier
    /// name, concrete id, or `provider:tier` spec, resolved against the agent's
    /// provider tier. `None` ⇒ the section default, then the agent's model.
    #[serde(default)]
    pub model: Option<String>,
    /// Per-plan curation backend, overriding `[self_learning] source`.
    /// `None` ⇒ the section default. See [`crate::settings::CurationSource`].
    #[serde(default)]
    pub source: Option<crate::settings::CurationSource>,
    /// What this plan's curation turn produces: a deep-expertise knowledge tree
    /// (default) or flat atomic items. `None` ⇒ [`CurationMode::DeepExpertise`].
    /// Must precede `sources` (a scalar may not follow the array-of-tables in TOML).
    #[serde(default)]
    pub mode: Option<CurationMode>,
    /// For [`CurationMode::ExperientialExpertise`]: how far back (seconds) to mine
    /// session transcripts. Overrides the global
    /// `[self_learning] experiential_lookback_secs`; `None` ⇒ that default. Must
    /// precede `sources` (a scalar may not follow the array-of-tables in TOML).
    #[serde(default)]
    pub transcript_lookback_secs: Option<u64>,
    /// Named read-only sources to research under. Must be the last field.
    #[serde(default)]
    pub sources: Vec<LearningSource>,
}

impl LearningPlanMeta {
    /// The resolved curation mode — absent ⇒ [`CurationMode::DeepExpertise`], the
    /// primary self-learning surface. A plan that wants the legacy atomic-item
    /// path must set `mode = "items"` explicitly.
    pub fn resolved_mode(&self) -> CurationMode {
        self.mode.unwrap_or_default()
    }
}

/// A parsed learning plan file — frontmatter (TOML) + body (markdown).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct LearningPlan {
    /// The file-stem ID (e.g. "billing" from "billing.md").
    pub id: String,
    /// Parsed frontmatter.
    pub meta: LearningPlanMeta,
    /// Markdown body — the objectives prompt for the agent.
    pub body: String,
}

/// Return the learning-plans directory for a workspace.
pub fn learning_plans_dir(workspace: &Path) -> PathBuf {
    workspace.join("learning")
}

/// Parse a learning plan from raw content + an ID.
pub fn parse_learning_plan(id: &str, content: &str) -> Result<LearningPlan> {
    let (frontmatter, body) = split_toml_frontmatter(content)?;
    let meta: LearningPlanMeta = toml::from_str(&frontmatter)
        .map_err(|e| eyre!("Failed to parse learning plan '{}' frontmatter: {}", id, e))?;
    Ok(LearningPlan {
        id: id.to_string(),
        meta,
        body,
    })
}

/// Serialize a learning plan back to the `+++` frontmatter format.
pub fn serialize_learning_plan(plan: &LearningPlan) -> Result<String> {
    let frontmatter = toml::to_string_pretty(&plan.meta)
        .map_err(|e| eyre!("Failed to serialize learning plan '{}': {}", plan.id, e))?;
    Ok(format!(
        "+++\n{}\n+++\n\n{}\n",
        frontmatter.trim(),
        plan.body
    ))
}

/// List all learning plans from disk, sorted by priority then ID.
/// Invalid files are skipped with a warning.
pub fn list_learning_plans(workspace: &Path) -> Result<Vec<LearningPlan>> {
    let dir = learning_plans_dir(workspace);
    if !dir.exists() {
        return Ok(Vec::new());
    }

    let mut plans = Vec::new();
    for entry in fs::read_dir(&dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) == Some("md") {
            let id = path
                .file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or_default()
                .to_string();
            let content = fs::read_to_string(&path)?;
            match parse_learning_plan(&id, &content) {
                Ok(plan) => plans.push(plan),
                Err(e) => {
                    tracing::warn!("Skipping invalid learning plan {}: {}", path.display(), e);
                }
            }
        }
    }

    plans.sort_by(|a, b| a.meta.priority.cmp(&b.meta.priority).then(a.id.cmp(&b.id)));
    Ok(plans)
}

/// Write a learning plan to disk, creating the learning directory if needed.
pub fn write_learning_plan(workspace: &Path, plan: &LearningPlan) -> Result<()> {
    validate_plan_id(&plan.id)?;
    let dir = learning_plans_dir(workspace);
    fs::create_dir_all(&dir)?;
    let path = dir.join(format!("{}.md", plan.id));
    let content = serialize_learning_plan(plan)?;
    fs::write(&path, content)?;
    Ok(())
}

/// Load a single learning plan by id, or `Ok(None)` if it does not exist.
pub fn load_learning_plan(workspace: &Path, id: &str) -> Result<Option<LearningPlan>> {
    validate_plan_id(id)?;
    let path = learning_plans_dir(workspace).join(format!("{id}.md"));
    match fs::read_to_string(&path) {
        Ok(content) => parse_learning_plan(id, &content).map(Some),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(e.into()),
    }
}

/// Delete a learning plan file. Returns whether a file was removed.
pub fn delete_learning_plan(workspace: &Path, id: &str) -> Result<bool> {
    validate_plan_id(id)?;
    let path = learning_plans_dir(workspace).join(format!("{id}.md"));
    match fs::remove_file(&path) {
        Ok(()) => Ok(true),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(e) => Err(e.into()),
    }
}

/// A plan id is a file stem, so it must be a simple slug — no path separators or
/// `..` that could escape the learning directory.
pub fn validate_plan_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        || id.contains("..")
    {
        bail!("invalid plan id '{id}': use letters, digits, '-', '_' or '.'");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plan(id: &str, meta: LearningPlanMeta, body: &str) -> LearningPlan {
        LearningPlan {
            id: id.to_string(),
            meta,
            body: body.to_string(),
        }
    }

    fn meta(name: &str) -> LearningPlanMeta {
        LearningPlanMeta {
            name: name.to_string(),
            enabled: true,
            schedule: None,
            interval_secs: None,
            run_as_identity: None,
            priority: 0,
            max_items_per_run: None,
            freshness_days: None,
            model: None,
            source: None,
            mode: None,
            transcript_lookback_secs: None,
            sources: Vec::new(),
        }
    }

    #[test]
    fn mode_defaults_to_deep_expertise_when_absent() {
        assert_eq!(meta("M").resolved_mode(), CurationMode::DeepExpertise);
    }

    #[test]
    fn parse_mode_experiential_expertise() {
        let content = "+++\nname = \"E\"\nmode = \"experiential_expertise\"\n+++\n\nbody";
        let p = parse_learning_plan("e", content).unwrap();
        assert_eq!(p.meta.mode, Some(CurationMode::ExperientialExpertise));
        assert_eq!(p.meta.resolved_mode(), CurationMode::ExperientialExpertise);
    }

    #[test]
    fn parse_transcript_lookback_secs() {
        let content =
            "+++\nname = \"E\"\nmode = \"experiential_expertise\"\ntranscript_lookback_secs = 86400\n+++\n\nbody";
        let p = parse_learning_plan("e", content).unwrap();
        assert_eq!(p.meta.transcript_lookback_secs, Some(86400));
    }

    #[test]
    fn round_trip_preserves_transcript_lookback_with_sources() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = meta("E");
        m.mode = Some(CurationMode::ExperientialExpertise);
        m.transcript_lookback_secs = Some(259200);
        m.sources = vec![LearningSource {
            skill: "notion".into(),
            instance: None,
            scope: None,
        }];
        write_learning_plan(dir.path(), &plan("e", m, "body")).unwrap();
        let listed = list_learning_plans(dir.path()).unwrap();
        assert_eq!(
            listed[0].meta.mode,
            Some(CurationMode::ExperientialExpertise)
        );
        assert_eq!(listed[0].meta.transcript_lookback_secs, Some(259200));
    }

    #[test]
    fn parse_mode_deep_expertise() {
        let content = "+++\nname = \"D\"\nmode = \"deep_expertise\"\n+++\n\nbody";
        let p = parse_learning_plan("d", content).unwrap();
        assert_eq!(p.meta.mode, Some(CurationMode::DeepExpertise));
        assert_eq!(p.meta.resolved_mode(), CurationMode::DeepExpertise);
    }

    #[test]
    fn omitted_mode_is_none_and_resolves_deep_expertise() {
        let p = parse_learning_plan("m", "+++\nname = \"M\"\n+++\n\nbody").unwrap();
        assert!(p.meta.mode.is_none());
        assert_eq!(p.meta.resolved_mode(), CurationMode::DeepExpertise);
    }

    #[test]
    fn round_trip_preserves_mode() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = meta("Deep");
        m.mode = Some(CurationMode::DeepExpertise);
        write_learning_plan(dir.path(), &plan("deep", m, "body")).unwrap();
        let listed = list_learning_plans(dir.path()).unwrap();
        assert_eq!(listed[0].meta.mode, Some(CurationMode::DeepExpertise));
    }

    #[test]
    fn parse_minimal_defaults_enabled() {
        let content = "+++\nname = \"Minimal\"\n+++\n\nResearch the thing.";
        let p = parse_learning_plan("min", content).unwrap();
        assert_eq!(p.meta.name, "Minimal");
        assert!(p.meta.enabled);
        assert_eq!(p.meta.priority, 0);
        assert!(p.meta.schedule.is_none());
        assert!(p.meta.interval_secs.is_none());
        assert!(p.meta.sources.is_empty());
        assert_eq!(p.body, "Research the thing.");
    }

    #[test]
    fn parse_cron_schedule() {
        let content = "+++\nname = \"Cron\"\nschedule = \"0 6 * * 1\"\n+++\n\nbody";
        let p = parse_learning_plan("cron", content).unwrap();
        assert_eq!(p.meta.schedule.as_deref(), Some("0 6 * * 1"));
        assert!(p.meta.interval_secs.is_none());
    }

    #[test]
    fn parse_interval_schedule() {
        let content = "+++\nname = \"Interval\"\ninterval_secs = 86400\n+++\n\nbody";
        let p = parse_learning_plan("interval", content).unwrap();
        assert_eq!(p.meta.interval_secs, Some(86400));
        assert!(p.meta.schedule.is_none());
    }

    #[test]
    fn parse_full_with_sources() {
        let content = "+++\n\
            name = \"Product & billing knowledge\"\n\
            enabled = true\n\
            schedule = \"0 6 * * 1\"\n\
            run_as_identity = \"id-uuid\"\n\
            priority = 100\n\
            max_items_per_run = 30\n\
            freshness_days = 30\n\n\
            [[sources]]\n\
            skill = \"notion\"\n\
            instance = \"acme\"\n\
            scope = \"Billing, Pricing\"\n\n\
            [[sources]]\n\
            skill = \"metabase\"\n\
            scope = \"Revenue dashboards\"\n\
            +++\n\n\
            Learn how billing works.";
        let p = parse_learning_plan("billing", content).unwrap();
        assert_eq!(p.meta.run_as_identity.as_deref(), Some("id-uuid"));
        assert_eq!(p.meta.priority, 100);
        assert_eq!(p.meta.max_items_per_run, Some(30));
        assert_eq!(p.meta.freshness_days, Some(30));
        assert_eq!(p.meta.sources.len(), 2);
        assert_eq!(p.meta.sources[0].skill, "notion");
        assert_eq!(p.meta.sources[0].instance.as_deref(), Some("acme"));
        assert_eq!(p.meta.sources[0].scope.as_deref(), Some("Billing, Pricing"));
        assert_eq!(p.meta.sources[1].skill, "metabase");
        assert!(p.meta.sources[1].instance.is_none());
        assert!(p.body.contains("Learn how billing works"));
    }

    #[test]
    fn parse_per_plan_model_and_source() {
        use crate::settings::CurationSource;
        let content = "+++\nname = \"M\"\nmodel = \"cheap\"\nsource = \"claude_code\"\n+++\n\nbody";
        let p = parse_learning_plan("m", content).unwrap();
        assert_eq!(p.meta.model.as_deref(), Some("cheap"));
        assert_eq!(p.meta.source, Some(CurationSource::ClaudeCode));
    }

    #[test]
    fn omitted_model_and_source_are_none() {
        let p = parse_learning_plan("m", "+++\nname = \"M\"\n+++\n\nbody").unwrap();
        assert!(p.meta.model.is_none());
        assert!(p.meta.source.is_none());
    }

    #[test]
    fn round_trip_preserves_model_and_source() {
        use crate::settings::CurationSource;
        let dir = tempfile::tempdir().unwrap();
        let mut m = meta("CC");
        m.model = Some("powerful".into());
        m.source = Some(CurationSource::ClaudeCode);
        write_learning_plan(dir.path(), &plan("cc", m, "body")).unwrap();
        let listed = list_learning_plans(dir.path()).unwrap();
        assert_eq!(listed[0].meta.model.as_deref(), Some("powerful"));
        assert_eq!(listed[0].meta.source, Some(CurationSource::ClaudeCode));
    }

    #[test]
    fn write_then_list_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let mut m = meta("Billing");
        m.schedule = Some("0 6 * * 1".into());
        m.run_as_identity = Some("id-uuid".into());
        m.priority = 5;
        m.sources = vec![LearningSource {
            skill: "notion".into(),
            instance: Some("acme".into()),
            scope: Some("Billing".into()),
        }];
        let p = plan("billing", m, "Learn billing.");

        write_learning_plan(dir.path(), &p).unwrap();

        let listed = list_learning_plans(dir.path()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0], p);
    }

    #[test]
    fn list_sorted_by_priority_then_id() {
        let dir = tempfile::tempdir().unwrap();
        let mut low = meta("Low");
        low.priority = 10;
        let mut high = meta("High");
        high.priority = 1;
        let mut tie_a = meta("TieA");
        tie_a.priority = 1;
        write_learning_plan(dir.path(), &plan("low", low, "x")).unwrap();
        write_learning_plan(dir.path(), &plan("high", high, "x")).unwrap();
        write_learning_plan(dir.path(), &plan("aaa", tie_a, "x")).unwrap();

        let listed = list_learning_plans(dir.path()).unwrap();
        let ids: Vec<&str> = listed.iter().map(|p| p.id.as_str()).collect();
        // priority 1 before 10; within priority 1, id "aaa" before "high".
        assert_eq!(ids, vec!["aaa", "high", "low"]);
    }

    #[test]
    fn malformed_file_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let good = plan("good", meta("Good"), "body");
        write_learning_plan(dir.path(), &good).unwrap();
        // A file without frontmatter must be skipped, not fail the whole list.
        fs::write(
            learning_plans_dir(dir.path()).join("broken.md"),
            "no frontmatter here",
        )
        .unwrap();

        let listed = list_learning_plans(dir.path()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].id, "good");
    }

    #[test]
    fn list_empty_dir() {
        let dir = tempfile::tempdir().unwrap();
        assert!(list_learning_plans(dir.path()).unwrap().is_empty());
    }

    #[test]
    fn load_and_delete_single_plan() {
        let dir = tempfile::tempdir().unwrap();
        let p = plan("billing", meta("Billing"), "Learn billing.");
        write_learning_plan(dir.path(), &p).unwrap();

        assert_eq!(load_learning_plan(dir.path(), "billing").unwrap(), Some(p));
        assert!(load_learning_plan(dir.path(), "missing").unwrap().is_none());

        assert!(delete_learning_plan(dir.path(), "billing").unwrap());
        assert!(load_learning_plan(dir.path(), "billing").unwrap().is_none());
        // Deleting an absent plan reports false, not an error.
        assert!(!delete_learning_plan(dir.path(), "billing").unwrap());
    }

    #[test]
    fn plan_id_rejects_path_traversal() {
        assert!(validate_plan_id("ok-plan_1.2").is_ok());
        for bad in ["", "../escape", "a/b", "..", "with space", "tab\t"] {
            assert!(validate_plan_id(bad).is_err(), "{bad} should be rejected");
        }
        // A traversal id must never resolve/delete outside the learning dir.
        let dir = tempfile::tempdir().unwrap();
        assert!(load_learning_plan(dir.path(), "../../etc/passwd").is_err());
        assert!(delete_learning_plan(dir.path(), "../../etc/passwd").is_err());
    }
}
