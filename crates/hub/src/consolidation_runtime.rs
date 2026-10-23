//! Consolidation-template runtime — definition store + hub-wide resolver.
//!
//! Consolidation templates are install-agnostic, member-owned JSON blobs (the
//! canonical statement lines plus a per-entity account map; see
//! [`workforce_skills::adapters::xero::report::ConsolidationTemplate`]) that the
//! Xero export tool consolidates several entities' reports against. They share
//! the owned-defs store with workflows/playbooks/monitors under a new
//! `kind = "consolidation_template"`, inheriting per-user ownership, visibility
//! and revisions.
//!
//! Unlike the playbook runtime this keeps no in-memory cache: there's no hot
//! path (templates are read at export time, not per chat turn), so every method
//! delegates straight to the store. That is less code and strictly more correct
//! for out-of-band DB edits — there is no stale cache to reload.

pub use crate::hub_core::workflows::ConsolidationTemplateSummary;
use std::sync::{Arc, OnceLock};

use eyre::WrapErr;
use serde_json::Value;
use workforce_core::access::Visibility;
use workforce_orchestrator::workflow_store::{
    DefRevision, EditContext, RunAsWrite, StoredDef, WorkflowStore,
};

use crate::resource_access::DefListing;

/// Consolidation templates live in `workflow_defs` under this `kind`.
pub const KIND: &str = "consolidation_template";

/// Reads a stored template's display name from its raw JSON source, falling back
/// to the id when absent or unparseable — templates carry no embedded id and an
/// optional `name`, so the listing derives both from the row + blob.
fn summary_of(row: &StoredDef) -> ConsolidationTemplateSummary {
    let name = serde_json::from_str::<Value>(&row.toml_source)
        .ok()
        .and_then(|v| v.get("name").and_then(Value::as_str).map(str::to_string))
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| row.id.clone());
    ConsolidationTemplateSummary {
        id: row.id.clone(),
        name,
        owner: row.owner.clone(),
        visibility: row.visibility,
    }
}

/// Validate a template's JSON source against the exact shape the Xero export
/// consumer deserializes, so a template that would later fail at export-time is
/// rejected at save-time with a real parse error (rather than silently storing
/// and surfacing a misleading "not found"). Extra fields (e.g. `name`) are kept.
fn validate_template(source: &str) -> eyre::Result<Value> {
    use workforce_skills::adapters::xero::report::ConsolidationTemplate;
    let value: Value =
        serde_json::from_str(source).wrap_err("consolidation template is not valid JSON")?;
    if !value.get("lines").map(Value::is_array).unwrap_or(false) {
        return Err(eyre::eyre!(
            "consolidation template must be a JSON object with a `lines` array"
        ));
    }
    serde_json::from_value::<ConsolidationTemplate>(value.clone()).wrap_err(
        "invalid consolidation template: each line needs {label, kind: header|line|subtotal}",
    )?;
    Ok(value)
}

/// Owned-defs runtime for consolidation templates: CRUD + revisions, all keyed
/// by `(id, owner)` and delegated to the shared [`WorkflowStore`]. Requires the
/// database store — templates are member-owned, which the flat-file fallback
/// cannot namespace.
#[derive(Default)]
pub struct ConsolidationTemplateRuntime {
    store: OnceLock<Arc<WorkflowStore>>,
}

impl ConsolidationTemplateRuntime {
    pub fn new() -> Self {
        Self::default()
    }

    /// Attach the DB definition store. Call once at startup.
    pub fn set_store(&self, store: Arc<WorkflowStore>) {
        let _ = self.store.set(store);
    }

    fn def_store(&self) -> eyre::Result<&Arc<WorkflowStore>> {
        self.store.get().ok_or_else(|| {
            eyre::eyre!("consolidation templates require a database store; this deployment uses the filesystem")
        })
    }

    /// Templates visible to `caller`: an admin sees all; everyone else sees
    /// global templates plus their own. Mirrors the playbook visibility filter.
    pub async fn list_definitions_visible(
        &self,
        listing: &DefListing,
    ) -> eyre::Result<Vec<ConsolidationTemplateSummary>> {
        let mut out: Vec<_> = self
            .def_store()?
            .list(KIND)
            .await?
            .iter()
            .filter(|r| listing.sees(r.owner.as_deref(), r.visibility, &r.id))
            .map(summary_of)
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        Ok(out)
    }

    /// Resolve a `(bare id, owner)` pair to the stored template's `(owner,
    /// visibility)`, if present — backs the gateway's access checks. `owner ==
    /// None` addresses a system definition (the `__system__` namespace).
    pub async fn definition_ownership(
        &self,
        id: &str,
        owner: Option<&str>,
    ) -> Option<(Option<String>, Visibility)> {
        let store = self.store.get()?;
        store
            .list(KIND)
            .await
            .ok()?
            .into_iter()
            .find(|r| r.id == id && r.owner.as_deref() == owner)
            .map(|r| (r.owner, r.visibility))
    }

    /// The raw JSON source of a template owned by `owner` (`None` = system).
    pub async fn get_definition_source(
        &self,
        id: &str,
        owner: Option<&str>,
    ) -> eyre::Result<String> {
        self.def_store()?
            .get(id, KIND, owner)
            .await?
            .ok_or_else(|| eyre::eyre!("consolidation template '{}' not found", id))
    }

    /// Validate and persist a template under `(id, owner)` with `visibility`,
    /// recording who/where in the revision history. The stored blob carries no
    /// embedded id, so there is no id-mismatch check.
    pub async fn save_definition_source(
        &self,
        id: &str,
        source: &str,
        owner: Option<&str>,
        visibility: Visibility,
        edit: EditContext<'_>,
    ) -> eyre::Result<()> {
        let parsed = validate_template(source)?;
        self.def_store()?
            .save(
                id,
                KIND,
                source,
                Some(parsed),
                owner,
                visibility,
                RunAsWrite::Unchanged,
                edit,
            )
            .await
    }

    /// Delete a template owned by `owner` (`None` = system), recording a
    /// tombstone revision.
    pub async fn delete_definition(
        &self,
        id: &str,
        owner: Option<&str>,
        edit: EditContext<'_>,
    ) -> eyre::Result<()> {
        self.def_store()?.delete(id, KIND, owner, edit).await
    }

    /// Revision history (newest first, content omitted) for one template.
    pub async fn list_revisions(
        &self,
        id: &str,
        owner: Option<&str>,
    ) -> eyre::Result<Vec<DefRevision>> {
        self.def_store()?.list_revisions(KIND, id, owner).await
    }

    /// One revision with its full content.
    pub async fn get_revision(
        &self,
        id: &str,
        owner: Option<&str>,
        rev: i32,
    ) -> eyre::Result<Option<DefRevision>> {
        self.def_store()?.get_revision(KIND, id, owner, rev).await
    }
}

/// Hub-side [`ConsolidationTemplateResolver`](workforce_agent::consolidation::ConsolidationTemplateResolver)
/// over the owned-defs store. The Xero export tool resolves a template id at
/// export time through the hub-wide getter; this returns the user's own copy,
/// falling back to a shared/global one of the same id.
pub struct StoreConsolidationResolver {
    store: Arc<WorkflowStore>,
}

impl StoreConsolidationResolver {
    pub fn new(store: Arc<WorkflowStore>) -> Self {
        Self { store }
    }

    /// Bridge the sync resolver trait onto the async store. The export tool runs
    /// inside a tokio worker, so a current-thread `block_on` would panic — use
    /// `block_in_place` + the current runtime handle (mirrors the playbook
    /// runtime's `block_on`).
    fn block_on<F: std::future::Future>(fut: F) -> F::Output {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }
}

impl workforce_agent::consolidation::ConsolidationTemplateResolver for StoreConsolidationResolver {
    fn resolve(&self, id: &str, user: Option<&str>) -> Option<String> {
        Self::block_on(async {
            // The caller's own template shadows a shared one of the same id; when
            // they have none, fall back to the global/system copy (owner = None).
            if let Some(u) = user {
                if let Ok(Some(source)) = self.store.get(id, KIND, Some(u)).await {
                    return Some(source);
                }
            }
            self.store.get(id, KIND, None).await.ok().flatten()
        })
    }
}

/// Consolidation-template runtime (owned defs for the Xero export) +
/// hub-wide resolver. Member-owned JSON templates share the workflow_defs
/// store and need Postgres; the resolver lets the Xero export tool (in the
/// skills crate, which can't reach this store) fetch a template by id.
pub(crate) async fn wire(
    service: &Arc<crate::serve::AgentService>,
    config: &workforce_common::settings::WorkforceConfig,
) {
    let consolidation_runtime = Arc::new(ConsolidationTemplateRuntime::new());
    let _ = service
        .consolidation_runtime
        .set(Arc::clone(&consolidation_runtime) as Arc<dyn crate::hosts::ConsolidationHost>);
    if let Some(store) = crate::workflow_runtime::shared_workflow_store(config).await {
        consolidation_runtime.set_store(Arc::clone(&store));
        // Install the resolver over the SAME store the runtime uses so the
        // Xero export tool resolves a template id (own copy, then global).
        workforce_agent::consolidation::set_consolidation_resolver(Arc::new(
            StoreConsolidationResolver::new(store),
        ));
    }
}

#[async_trait::async_trait]
impl crate::hosts::ConsolidationHost for ConsolidationTemplateRuntime {
    async fn list_definitions_visible(
        &self,
        listing: &DefListing,
    ) -> eyre::Result<Vec<ConsolidationTemplateSummary>> {
        ConsolidationTemplateRuntime::list_definitions_visible(self, listing).await
    }

    async fn definition_ownership(
        &self,
        id: &str,
        owner: Option<&str>,
    ) -> Option<(Option<String>, Visibility)> {
        ConsolidationTemplateRuntime::definition_ownership(self, id, owner).await
    }

    async fn get_definition_source(&self, id: &str, owner: Option<&str>) -> eyre::Result<String> {
        ConsolidationTemplateRuntime::get_definition_source(self, id, owner).await
    }

    async fn save_definition_source(
        &self,
        id: &str,
        source: &str,
        owner: Option<&str>,
        visibility: Visibility,
        edit: EditContext<'_>,
    ) -> eyre::Result<()> {
        ConsolidationTemplateRuntime::save_definition_source(
            self, id, source, owner, visibility, edit,
        )
        .await
    }

    async fn delete_definition(
        &self,
        id: &str,
        owner: Option<&str>,
        edit: EditContext<'_>,
    ) -> eyre::Result<()> {
        ConsolidationTemplateRuntime::delete_definition(self, id, owner, edit).await
    }

    async fn list_revisions(
        &self,
        id: &str,
        owner: Option<&str>,
    ) -> eyre::Result<Vec<DefRevision>> {
        ConsolidationTemplateRuntime::list_revisions(self, id, owner).await
    }

    async fn get_revision(
        &self,
        id: &str,
        owner: Option<&str>,
        rev: i32,
    ) -> eyre::Result<Option<DefRevision>> {
        ConsolidationTemplateRuntime::get_revision(self, id, owner, rev).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_core::access::Visibility;

    #[test]
    fn validate_requires_lines_array() {
        assert!(validate_template(r#"{"lines":[]}"#).is_ok());
        assert!(validate_template(r#"{"lines":[{"label":"X","kind":"line"}]}"#).is_ok());
        // Not JSON, no `lines`, or `lines` not an array → rejected.
        assert!(validate_template("not json").is_err());
        assert!(validate_template(r#"{"map":{}}"#).is_err());
        assert!(validate_template(r#"{"lines":"nope"}"#).is_err());
        // Strict element validation — must match what the export consumer reads,
        // so a bad line is rejected at save (not silently stored then failing).
        assert!(validate_template(r#"{"lines":[{"label":"X","kind":"weird"}]}"#).is_err());
        assert!(validate_template(r#"{"lines":[{"label":"X"}]}"#).is_err());
    }

    #[test]
    fn summary_reads_name_else_id() {
        let with_name = StoredDef {
            id: "consol".into(),
            toml_source: r#"{"name":"Group P&L","lines":[]}"#.into(),
            owner: Some("alice".into()),
            last_editor: Some("alice".into()),
            run_as: None,
            visibility: Visibility::Global,
        };
        let s = summary_of(&with_name);
        assert_eq!(s.name, "Group P&L");
        assert_eq!(s.owner.as_deref(), Some("alice"));

        // No `name` (or blank / unparseable) → the id is the display name.
        let no_name = StoredDef {
            id: "consol".into(),
            toml_source: r#"{"lines":[]}"#.into(),
            owner: None,
            last_editor: None,
            run_as: None,
            visibility: Visibility::Global,
        };
        assert_eq!(summary_of(&no_name).name, "consol");
        let bad = StoredDef {
            toml_source: "{not json".into(),
            ..no_name.clone()
        };
        assert_eq!(summary_of(&bad).name, "consol");
    }

    // ── Store-backed tests (resolver fallback, CRUD) need Postgres ──────────
    // `block_in_place` requires a multi-thread runtime; the store needs the PG
    // test harness. These run only where Postgres is reachable.
    use workforce_agent::consolidation::ConsolidationTemplateResolver;

    async fn with_runtime<F, Fut>(test: F)
    where
        F: FnOnce(ConsolidationTemplateRuntime, Arc<WorkflowStore>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        workforce_postgres::test_support::with_test_pool(
            WorkflowStore::build_pool,
            |pool| async move {
                let store = WorkflowStore::new(pool).await.expect("store");
                let runtime = ConsolidationTemplateRuntime::new();
                runtime.set_store(Arc::clone(&store));
                test(runtime, store).await;
            },
        )
        .await;
    }

    fn edit() -> EditContext<'static> {
        EditContext::new("test", Some("alice"))
    }

    const TEMPLATE: &str =
        r#"{"lines":[{"label":"Revenue","kind":"line","components":[]}],"map":{}}"#;

    #[tokio::test(flavor = "multi_thread")]
    async fn resolver_prefers_user_then_falls_back_to_global() {
        with_runtime(|runtime, store| async move {
            // A user-owned template and a different system/global one of the
            // same id coexist (composite PK is `(id, owner)`).
            runtime
                .save_definition_source(
                    "group",
                    TEMPLATE,
                    Some("alice"),
                    Visibility::Private,
                    edit(),
                )
                .await
                .unwrap();
            let global = r#"{"name":"Shared","lines":[],"map":{}}"#;
            runtime
                .save_definition_source("group", global, None, Visibility::Global, edit())
                .await
                .unwrap();

            let resolver = StoreConsolidationResolver::new(store);
            // The owner gets their own copy …
            assert_eq!(
                resolver.resolve("group", Some("alice")).as_deref(),
                Some(TEMPLATE)
            );
            // … a different user (no own copy) falls back to the global …
            assert_eq!(
                resolver.resolve("group", Some("bob")).as_deref(),
                Some(global)
            );
            // … no user resolves the global directly …
            assert_eq!(resolver.resolve("group", None).as_deref(), Some(global));
            // … and an unknown id resolves to nothing.
            assert!(resolver.resolve("nope", Some("alice")).is_none());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn crud_and_visibility_round_trip() {
        with_runtime(|runtime, _store| async move {
            runtime
                .save_definition_source("g", TEMPLATE, Some("alice"), Visibility::Private, edit())
                .await
                .unwrap();
            // Owner reads their source back; ownership resolves to (alice, private).
            assert_eq!(
                runtime
                    .get_definition_source("g", Some("alice"))
                    .await
                    .unwrap(),
                TEMPLATE
            );
            assert_eq!(
                runtime.definition_ownership("g", Some("alice")).await,
                Some((Some("alice".to_string()), Visibility::Private))
            );

            // Alice sees her private template; Bob (member) does not; admin does.
            let alice = runtime
                .list_definitions_visible(&DefListing::local(Some("alice"), false))
                .await
                .unwrap();
            assert_eq!(alice.len(), 1);
            assert_eq!(alice[0].id, "g");
            assert!(runtime
                .list_definitions_visible(&DefListing::local(Some("bob"), false))
                .await
                .unwrap()
                .is_empty());
            assert_eq!(
                runtime
                    .list_definitions_visible(&DefListing::local(None, true))
                    .await
                    .unwrap()
                    .len(),
                1
            );

            // Invalid JSON is rejected before it reaches the store.
            assert!(runtime
                .save_definition_source("g", "{bad", Some("alice"), Visibility::Private, edit())
                .await
                .is_err());

            // Delete removes it and leaves a tombstone in the revisions.
            runtime
                .delete_definition("g", Some("alice"), edit())
                .await
                .unwrap();
            assert!(runtime
                .definition_ownership("g", Some("alice"))
                .await
                .is_none());
            let revs = runtime.list_revisions("g", Some("alice")).await.unwrap();
            assert!(revs.iter().any(|r| r.deleted));
        })
        .await;
    }
}
