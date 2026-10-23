//! `wasm_skill.publish` — open a catalog pull request holding an installed
//! skill's stored source snapshot and its `catalog.json` entry
//!, through [`crate::skill_publish`].

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::{bail, Report, Result};
use workforce_agent::tools::{Tool, ToolResult};
use workforce_core::access::AccessLevel;
use workforce_skills::catalog::SkillKind;
use workforce_wasm_runtime::manifest::SkillManifest;
use workforce_wasm_runtime::store::project_source_files;
use workforce_wasm_runtime::types::{contract_fingerprint, InstalledWasmSkill};
use workforce_wasm_runtime::WasmSkillStore;

use crate::skill_publish::{
    self, publish_input_schema, source_line, version_line, Publication, PublishInput, PublishPlan,
    Published, Release, RemoteFactory, TreeDiff,
};
use crate::wasm_skill_tools::WasmSkillContext;

/// An installed WASM skill as a publish ships it: the source stored when it
/// was built, and its consent contract against the published one.
struct WasmPublication<'a> {
    installed: &'a InstalledWasmSkill,
    store: &'a WasmSkillStore,
}

impl WasmPublication<'_> {
    fn published(&self, manifest: &SkillManifest) -> Published {
        Published {
            version: manifest.version.clone(),
            contract_changed: contract_fingerprint(manifest)
                != self.installed.contract_fingerprint(),
        }
    }
}

impl Publication for WasmPublication<'_> {
    const TOOL: &'static str = "wasm_skill.publish";
    const MANIFEST: &'static str = "Skill.toml";

    fn release(&self) -> Release {
        let manifest = &self.installed.manifest;
        Release {
            kind: SkillKind::Wasm,
            name: manifest.name.clone(),
            version: manifest.version.clone(),
            description: manifest.description.clone(),
            requires_bins: None,
        }
    }

    fn governed_files(dir: &Path) -> Result<Vec<(PathBuf, u64)>> {
        project_source_files(dir)
    }

    fn source_tree(&self, scratch: &Path) -> Result<PathBuf> {
        self.store.restore_source(
            &self.installed.manifest.name,
            scratch,
            &workforce_agent::tools::paths::CanonicalRoots::new(&[]),
        )?;
        Ok(scratch.to_path_buf())
    }

    fn read_published(&self, manifest: &Path) -> Result<Published> {
        SkillManifest::from_file(manifest).map(|manifest| self.published(&manifest))
    }

    fn body_lines(&self, plan: &PublishPlan, diff: &TreeDiff) -> Vec<String> {
        let contract = match plan.contract_changed {
            true => "changed — grants are re-requested on install",
            false => "unchanged",
        };
        vec![
            version_line(&self.release(), plan),
            format!("- Consent contract: {contract}"),
            source_line(diff),
            format!("- Installed binary sha256: `{}`", self.installed.sha256),
        ]
    }
}

pub struct WasmSkillPublishTool {
    ctx: WasmSkillContext,
    remote_for: RemoteFactory,
}

impl WasmSkillPublishTool {
    pub fn new(ctx: WasmSkillContext, remote_for: RemoteFactory) -> Self {
        Self { ctx, remote_for }
    }

    /// The production tool: publishes to GitHub with the caller's credential.
    pub fn github(ctx: WasmSkillContext) -> Self {
        Self::new(ctx, Arc::new(skill_publish::github_remote))
    }

    async fn publish(&self, params: PublishInput) -> Result<serde_json::Value> {
        let installed = self
            .ctx
            .caller()
            .require(&self.ctx.store, &params.skill, AccessLevel::Manage)
            .map_err(Report::msg)?;
        let name = installed.manifest.name.clone();
        if installed.sha256.is_empty() {
            bail!(
                "Skill '{name}' predates build fingerprinting — rebuild it once \
                 (wasm_skill.build) and publish again."
            );
        }
        if !self.ctx.store.has_source_snapshot(&name) {
            bail!(
                "No source snapshot for '{name}': only skills built from a project \
                 (wasm_skill.build) carry source to publish."
            );
        }
        if !self.ctx.store.source_snapshot_matches_binary(&name) {
            bail!(
                "The stored source for '{name}' predates the installed binary; rebuild from \
                 the project you edited (wasm_skill.build) and publish again."
            );
        }
        skill_publish::publish(
            &WasmPublication {
                installed: &installed,
                store: &self.ctx.store,
            },
            &self.ctx.catalogs,
            &self.remote_for,
            &|owner: &str| self.ctx.resolve_github_token(Some(owner)),
            &params,
        )
        .await
    }
}

#[async_trait]
impl Tool for WasmSkillPublishTool {
    fn name(&self) -> &str {
        "wasm_skill.publish"
    }

    fn description(&self) -> &str {
        "Open a pull request against a configured skill catalog repo containing an installed \
         WASM skill's stored source and its catalog.json entry, so a skill written in chat \
         lands under source control. Deterministic: publishes exactly what is installed. \
         Refused when the installed version is already in the catalog with different source \
         or a different consent contract — bump [skill] version, rebuild, publish again."
    }

    fn input_schema(&self) -> serde_json::Value {
        publish_input_schema()
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: PublishInput = serde_json::from_value(input)?;
        let outcome = self.publish(params).await;
        let duration = start.elapsed().as_millis() as u64;
        Ok(match outcome {
            Ok(output) => ToolResult::success(output, duration),
            Err(refusal) => ToolResult::failure(format!("{refusal:#}"), duration),
        })
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;
    use std::sync::Mutex;

    use super::*;
    use crate::skill_publish::tests::{catalog_source, seed_origin, sh_git, LocalRemote};
    use crate::skill_publish::{
        apply_tree_diff, branch_name, diff_trees, entry_subpath, github_remote, plan_release,
        pr_body, PublishRemote, Refusal,
    };
    use workforce_agent::tools::registry::ToolRegistry;
    use workforce_common::settings::CatalogSource;
    use workforce_skills::catalog::{CatalogEntry, CatalogManifest};
    use workforce_wasm_runtime::WasmSkillEngine;

    /// A manifest whose consent contract is its `[[permissions]]` list.
    fn skill_toml(name: &str, version: &str, tools: &[&str]) -> String {
        let capabilities = match tools.is_empty() {
            true => String::new(),
            false => "[capabilities]\ntools = true\n".to_string(),
        };
        let permissions = tools
            .iter()
            .map(|t| format!("[[permissions]]\ntool = \"{t}\"\ndescription = \"use {t}\"\n"))
            .collect::<String>();
        format!(
            "[skill]\nname = \"{name}\"\nversion = \"{version}\"\ndescription = \"A {name}\"\n\
             author = \"test\"\nlicense = \"MIT\"\n{capabilities}\
             [[tools]]\nname = \"ping\"\ndescription = \"pong\"\n\
             input_schema = {{ type = \"object\" }}\n{permissions}"
        )
    }

    fn test_ctx(
        workspace: &Path,
        store_dir: &Path,
        catalogs: Vec<CatalogSource>,
    ) -> WasmSkillContext {
        WasmSkillContext {
            engine: Arc::new(WasmSkillEngine::new().expect("engine")),
            store: Arc::new(WasmSkillStore::open(store_dir).expect("store")),
            tools: Arc::new(ToolRegistry::new()),
            skill_state: Arc::new(crate::skill_state::SkillStateTracker::new()),
            hosts: crate::wasm_skill_tools::SkillHosts::default(),
            data_dir: workspace.to_path_buf(),
            policy: crate::wasm_skill_tools::WasmInstallPolicy::default(),
            agent_vault: None,
            global_vault: None,
            runner: None,
            path_roots: vec![workspace.to_path_buf()],
            admin: crate::authz::always_admin(),
            teams: Arc::new(parking_lot::RwLock::new(None)),
            catalogs,
        }
    }

    fn install(store: &WasmSkillStore, manifest: &str, bytes: &[u8]) -> InstalledWasmSkill {
        store
            .install_prebuilt(
                SkillManifest::from_toml(manifest).expect("manifest parses"),
                bytes,
                None,
                "test".into(),
                HashMap::new(),
                workforce_wasm_runtime::SkillOwnership::operator(),
            )
            .expect("installs")
    }

    /// A project directory shaped like a scaffolded skill.
    fn write_project(dir: &Path, manifest: &str, lib_rs: &str) {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("Skill.toml"), manifest).unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"demo\"\n").unwrap();
        std::fs::write(dir.join("src/lib.rs"), lib_rs).unwrap();
    }

    fn wasm_entry(name: &str, version: Option<&str>) -> CatalogEntry {
        CatalogEntry {
            name: name.into(),
            kind: SkillKind::Wasm,
            subpath: format!("workforce/wasm/{name}"),
            version: version.map(Into::into),
            description: format!("A {name}"),
            requires_bins: vec![],
            wasm_path: None,
            sha256: None,
        }
    }

    // ─── the plan ────────────────────────────────────────────────────────

    struct Gate {
        installed: InstalledWasmSkill,
        store: WasmSkillStore,
        _store: tempfile::TempDir,
    }

    impl Gate {
        fn publication(&self) -> WasmPublication<'_> {
            WasmPublication {
                installed: &self.installed,
                store: &self.store,
            }
        }

        fn plan(
            &self,
            subpath: &str,
            existing: Option<&CatalogEntry>,
            published: Option<&SkillManifest>,
            tree_identical: bool,
        ) -> Result<Option<PublishPlan>, Refusal> {
            let publication = self.publication();
            plan_release(
                &publication.release(),
                subpath,
                existing,
                published.map(|m| publication.published(m)).as_ref(),
                tree_identical,
            )
        }

        fn pr_body(&self, plan: &PublishPlan, diff: &TreeDiff, summary: Option<&str>) -> String {
            let publication = self.publication();
            pr_body(
                &publication.release(),
                WasmPublication::TOOL,
                &publication.body_lines(plan, diff),
                summary,
            )
        }
    }

    fn gate(version: &str, tools: &[&str]) -> Gate {
        let store_dir = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(store_dir.path()).unwrap();
        let installed = install(&store, &skill_toml("demo", version, tools), b"wasm-1");
        Gate {
            installed,
            store,
            _store: store_dir,
        }
    }

    fn published(version: &str, tools: &[&str]) -> SkillManifest {
        SkillManifest::from_toml(&skill_toml("demo", version, tools)).unwrap()
    }

    #[test]
    fn new_skill_publishes_to_the_default_subpath() {
        let g = gate("0.1.0", &[]);
        let subpath = entry_subpath(None, "workforce/catalog.json", "demo", SkillKind::Wasm);
        assert_eq!(subpath, "workforce/wasm/demo");
        assert_eq!(
            entry_subpath(None, "catalog.json", "demo", SkillKind::Wasm),
            "wasm/demo"
        );
        let plan = g.plan(&subpath, None, None, false).unwrap();
        assert_eq!(
            plan,
            Some(PublishPlan {
                previous_version: None,
                contract_changed: false,
                entry: wasm_entry("demo", Some("0.1.0")),
            })
        );
    }

    #[test]
    fn existing_entry_keeps_its_subpath_and_bins() {
        let mut entry = wasm_entry("demo", Some("0.1.0"));
        entry.subpath = "custom/place".into();
        entry.requires_bins = vec!["jq".into()];
        let subpath = entry_subpath(
            Some(&entry),
            "workforce/catalog.json",
            "demo",
            SkillKind::Wasm,
        );
        assert_eq!(subpath, "custom/place");
        let g = gate("0.2.0", &[]);
        let plan = g
            .plan(
                &subpath,
                Some(&entry),
                Some(&published("0.1.0", &[])),
                false,
            )
            .unwrap()
            .expect("a publish");
        assert_eq!(plan.previous_version.as_deref(), Some("0.1.0"));
        assert_eq!(plan.entry.subpath, "custom/place");
        assert_eq!(plan.entry.requires_bins, vec!["jq".to_string()]);
    }

    #[test]
    fn same_version_with_identical_tree_and_entry_is_unchanged() {
        let g = gate("0.1.0", &[]);
        let entry = wasm_entry("demo", Some("0.1.0"));
        let plan = g
            .plan(
                &entry.subpath,
                Some(&entry),
                Some(&published("0.1.0", &[])),
                true,
            )
            .unwrap();
        assert_eq!(plan, None);
    }

    #[test]
    fn same_version_with_identical_tree_but_stale_entry_republishes_the_entry() {
        let g = gate("0.1.0", &[]);
        let plan = g
            .plan(
                "workforce/wasm/demo",
                None,
                Some(&published("0.1.0", &[])),
                true,
            )
            .unwrap();
        assert_eq!(plan.unwrap().previous_version.as_deref(), Some("0.1.0"));
    }

    #[test]
    fn same_version_with_different_source_is_refused() {
        let g = gate("0.1.0", &[]);
        let entry = wasm_entry("demo", Some("0.1.0"));
        let refusal = g
            .plan(
                &entry.subpath,
                Some(&entry),
                Some(&published("0.1.0", &[])),
                false,
            )
            .unwrap_err();
        assert_eq!(
            refusal,
            Refusal::SameVersionDrift {
                kind: SkillKind::Wasm,
                name: "demo".into(),
                version: "0.1.0".into(),
                contract_changed: false,
            }
        );
        assert!(refusal.to_string().contains("different source"));
    }

    #[test]
    fn same_version_with_changed_contract_says_so() {
        let g = gate("0.1.0", &["jira.search"]);
        let entry = wasm_entry("demo", Some("0.1.0"));
        let refusal = g
            .plan(
                &entry.subpath,
                Some(&entry),
                Some(&published("0.1.0", &[])),
                false,
            )
            .unwrap_err();
        assert!(matches!(
            refusal,
            Refusal::SameVersionDrift {
                kind: SkillKind::Wasm,
                contract_changed: true,
                ..
            }
        ));
        assert!(refusal.to_string().contains("consent contract"));
    }

    #[test]
    fn catalog_ahead_is_refused() {
        let g = gate("0.1.0", &[]);
        let entry = wasm_entry("demo", Some("0.2.0"));
        let refusal = g
            .plan(&entry.subpath, Some(&entry), None, false)
            .unwrap_err();
        assert_eq!(
            refusal,
            Refusal::CatalogAhead {
                kind: SkillKind::Wasm,
                name: "demo".into(),
                installed: "0.1.0".into(),
                published: "0.2.0".into(),
            }
        );
    }

    #[test]
    fn prebuilt_and_external_entries_are_refused() {
        let g = gate("0.1.0", &[]);
        let mut prebuilt = wasm_entry("demo", Some("0.0.1"));
        prebuilt.wasm_path = Some("demo.wasm".into());
        prebuilt.sha256 = Some("ab".repeat(32));
        assert_eq!(
            g.plan("x", Some(&prebuilt), None, false).unwrap_err(),
            Refusal::Prebuilt {
                name: "demo".into()
            }
        );
        let mut external = wasm_entry("demo", None);
        external.kind = SkillKind::External;
        assert_eq!(
            g.plan("x", Some(&external), None, false).unwrap_err(),
            Refusal::ListedAsExternal {
                name: "demo".into()
            }
        );
    }

    #[test]
    fn unparseable_catalog_version_is_refused() {
        let g = gate("0.1.0", &[]);
        let entry = wasm_entry("demo", Some("latest"));
        assert_eq!(
            g.plan("x", Some(&entry), None, false).unwrap_err(),
            Refusal::UnparseablePublishedVersion {
                name: "demo".into(),
                version: "latest".into(),
            }
        );
    }

    // ─── selection ───────────────────────────────────────────────────────

    #[test]
    fn branch_and_pr_body_name_the_transition() {
        assert_eq!(branch_name("demo", "1.2.3"), "skill/demo-v1.2.3");
        let g = gate("1.2.3", &[]);
        let plan = PublishPlan {
            previous_version: Some("1.2.2".into()),
            contract_changed: true,
            entry: wasm_entry("demo", Some("1.2.3")),
        };
        let diff = TreeDiff {
            added: vec!["src/new.rs".into()],
            updated: vec!["Skill.toml".into(), "src/lib.rs".into()],
            removed: vec![],
        };
        let body = g.pr_body(&plan, &diff, Some("  tightened the JQL  "));
        assert!(body.contains("v1.2.2 → v1.2.3"));
        assert!(body.contains("changed — grants are re-requested"));
        assert!(body.contains("1 added, 2 updated, 0 removed"));
        assert!(body.contains("## Notes\n\ntightened the JQL"));
        let quiet = g.pr_body(&plan, &diff, None);
        assert!(!quiet.contains("## Notes"));
    }

    #[tokio::test]
    async fn wasm_refusals_and_pr_body_read_exactly_as_before() {
        let name = || "hello-counter".to_string();
        for (refusal, expected) in [
            (
                Refusal::ListedAsExternal { name: name() },
                "The catalog lists 'hello-counter' as an external (SKILL.md) skill; a WASM skill \
                 cannot take its entry.",
            ),
            (
                Refusal::Prebuilt { name: name() },
                "The catalog entry for 'hello-counter' is prebuilt (wasm_path + sha256). \
                 Publishing ships source that the hub builds on install; drop those two fields \
                 from the entry and publish again.",
            ),
            (
                Refusal::UnparseablePublishedVersion {
                    name: name(),
                    version: "latest".into(),
                },
                "The catalog's version 'latest' for 'hello-counter' is not semver, so the \
                 installed version cannot be compared with it; fix the entry first.",
            ),
            (
                Refusal::SameVersionDrift {
                    kind: SkillKind::Wasm,
                    name: name(),
                    version: "1.0.0".into(),
                    contract_changed: true,
                },
                "'hello-counter' v1.0.0 is already in the catalog with a different consent \
                 contract (tools, permissions, credentials or network): installing it over the \
                 published one would silently re-request every grant. Bump [skill] version in \
                 Skill.toml, rebuild (wasm_skill.build) and publish again.",
            ),
            (
                Refusal::SameVersionDrift {
                    kind: SkillKind::Wasm,
                    name: name(),
                    version: "1.0.0".into(),
                    contract_changed: false,
                },
                "'hello-counter' v1.0.0 is already in the catalog with different source. Bump \
                 [skill] version in Skill.toml, rebuild (wasm_skill.build) and publish again.",
            ),
            (
                Refusal::CatalogAhead {
                    kind: SkillKind::Wasm,
                    name: name(),
                    installed: "1.0.0".into(),
                    published: "1.1.0".into(),
                },
                "The catalog holds 'hello-counter' v1.1.0, newer than the installed v1.0.0. \
                 Install the catalog version, or bump past it and rebuild.",
            ),
        ] {
            assert_eq!(refusal.to_string(), expected);
        }

        let g = gate("1.2.3", &[]);
        let plan = PublishPlan {
            previous_version: Some("1.2.2".into()),
            contract_changed: false,
            entry: wasm_entry("demo", Some("1.2.3")),
        };
        let diff = TreeDiff {
            added: vec!["src/new.rs".into()],
            updated: vec!["Skill.toml".into(), "src/lib.rs".into()],
            removed: vec!["src/old.rs".into()],
        };
        assert_eq!(
            g.pr_body(&plan, &diff, Some("  tightened the query  ")),
            format!(
                "Publishes `demo` v1.2.3 from the workforce hub (`wasm_skill.publish`).\n\n\
                 - Version: v1.2.2 → v1.2.3\n\
                 - Consent contract: unchanged\n\
                 - Source: 1 added, 2 updated, 1 removed\n\
                 - Installed binary sha256: `{}`\n\
                 \n## Notes\n\ntightened the query\n",
                g.installed.sha256
            )
        );
        let fresh = PublishPlan {
            previous_version: None,
            contract_changed: true,
            ..plan
        };
        assert_eq!(
            g.pr_body(&fresh, &TreeDiff::default(), None),
            format!(
                "Publishes `demo` v1.2.3 from the workforce hub (`wasm_skill.publish`).\n\n\
                 - Version: new skill → v1.2.3\n\
                 - Consent contract: changed — grants are re-requested on install\n\
                 - Source: 0 added, 0 updated, 0 removed\n\
                 - Installed binary sha256: `{}`\n",
                g.installed.sha256
            )
        );

        let workspace = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let ctx = test_ctx(workspace.path(), store.path(), vec![]);
        assert_eq!(
            github_remote(&catalog_source("a"), &|owner| {
                ctx.resolve_github_token(Some(owner))
            })
            .err()
            .unwrap(),
            "No GitHub token to publish to acme/skills. Store one at \
             skills/github/workspaces/acme/token (or skills/github/token): publishing writes to \
             the catalog repo with your own credential, not the catalog's read token."
        );
    }

    // ─── the tree ────────────────────────────────────────────────────────

    #[test]
    fn tree_diff_governs_only_snapshot_files() {
        let tmp = tempfile::tempdir().unwrap();
        let snapshot = tmp.path().join("snapshot");
        let target = tmp.path().join("target");
        write_project(&snapshot, "[skill]\nname = \"demo\"\n", "new body");
        std::fs::write(snapshot.join("src/extra.rs"), "extra").unwrap();
        write_project(&target, "[skill]\nname = \"demo\"\n", "old body");
        std::fs::write(target.join("src/gone.rs"), "gone").unwrap();
        std::fs::write(target.join("README.md"), "keep me").unwrap();
        std::fs::create_dir_all(target.join("target")).unwrap();
        std::fs::write(target.join("target/demo.wasm"), "bin").unwrap();

        let diff = diff_trees(&snapshot, &target, project_source_files).unwrap();
        assert_eq!(
            diff,
            TreeDiff {
                added: vec!["src/extra.rs".into()],
                updated: vec!["src/lib.rs".into()],
                removed: vec!["src/gone.rs".into()],
            }
        );
        apply_tree_diff(&snapshot, &target, &diff).unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("src/lib.rs")).unwrap(),
            "new body"
        );
        assert_eq!(
            std::fs::read_to_string(target.join("src/extra.rs")).unwrap(),
            "extra"
        );
        assert!(!target.join("src/gone.rs").exists());
        assert_eq!(
            std::fs::read_to_string(target.join("README.md")).unwrap(),
            "keep me"
        );
        assert!(target.join("target/demo.wasm").exists());
        assert!(diff_trees(&snapshot, &target, project_source_files)
            .unwrap()
            .is_empty());
    }

    #[test]
    fn tree_diff_against_a_missing_target_adds_everything() {
        let tmp = tempfile::tempdir().unwrap();
        let snapshot = tmp.path().join("snapshot");
        write_project(&snapshot, "[skill]\n", "body");
        let diff = diff_trees(&snapshot, &tmp.path().join("absent"), project_source_files).unwrap();
        assert_eq!(diff.added.len(), 3);
        assert!(diff.updated.is_empty() && diff.removed.is_empty());
    }

    // ─── the remote ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn github_remote_needs_the_owners_token() {
        let workspace = tempfile::tempdir().unwrap();
        let store = tempfile::tempdir().unwrap();
        let ctx = test_ctx(workspace.path(), store.path(), vec![]);
        let err = github_remote(&catalog_source("a"), &|owner| {
            ctx.resolve_github_token(Some(owner))
        })
        .err()
        .expect("no token, no remote");
        assert!(err.contains("skills/github/workspaces/acme/token"), "{err}");
    }

    // ─── end to end against a bare repository ────────────────────────────

    struct Harness {
        root: tempfile::TempDir,
        bare: PathBuf,
        ctx: WasmSkillContext,
        remote: Arc<LocalRemote>,
    }

    impl Harness {
        async fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let bare = seed_origin(root.path()).await;
            let ctx = test_ctx(
                &root.path().join("workspace"),
                &root.path().join("store"),
                vec![catalog_source("test")],
            );
            let remote = Arc::new(LocalRemote {
                url: format!("file://{}", bare.display()),
                opened: Mutex::new(Vec::new()),
            });
            Self {
                root,
                bare,
                ctx,
                remote,
            }
        }

        fn tool(&self) -> WasmSkillPublishTool {
            let remote = Arc::clone(&self.remote);
            WasmSkillPublishTool::new(
                self.ctx.clone(),
                Arc::new(move |_, _| Ok(Arc::clone(&remote) as Arc<dyn PublishRemote>)),
            )
        }

        /// Install `demo` from a project and snapshot it, as `wasm_skill.build` does.
        fn install_from_project(&self, version: &str, lib_rs: &str, bytes: &[u8]) {
            let project = self.root.path().join(format!("project-{version}"));
            write_project(&project, &skill_toml("demo", version, &[]), lib_rs);
            let installed = install(&self.ctx.store, &skill_toml("demo", version, &[]), bytes);
            self.ctx
                .store
                .snapshot_source("demo", &project, &installed)
                .unwrap();
        }

        async fn publish(&self, input: serde_json::Value) -> ToolResult {
            self.tool().execute(input).await.unwrap()
        }

        async fn show(&self, git_ref: &str, path: &str) -> String {
            sh_git(&self.bare, &["show", &format!("{git_ref}:{path}")]).await
        }

        async fn merge_into_main(&self, branch: &str) {
            sh_git(
                &self.bare,
                &[
                    "update-ref",
                    "refs/heads/main",
                    &format!("refs/heads/{branch}"),
                ],
            )
            .await;
            sh_git(
                &self.bare,
                &["update-ref", "-d", &format!("refs/heads/{branch}")],
            )
            .await;
        }
    }

    #[tokio::test]
    async fn publish_pushes_snapshot_and_catalog_entry_and_opens_a_pr() {
        let h = Harness::new().await;
        h.install_from_project("1.2.0", "// v1", b"wasm-1");

        let result = h
            .publish(serde_json::json!({ "skill": "demo", "summary": "first cut" }))
            .await;
        assert!(result.success, "{:?}", result.error);
        let out = &result.output;
        assert_eq!(out["published"], true);
        assert_eq!(out["branch"], "skill/demo-v1.2.0");
        assert_eq!(out["base"], "main");
        assert_eq!(out["subpath"], "workforce/wasm/demo");
        assert_eq!(out["pr_number"], 1);
        assert_eq!(out["pr_url"], "local://pr/1");
        assert_eq!(out["previous_version"], serde_json::Value::Null);
        assert_eq!(out["files"]["added"], 3);

        let branch = "skill/demo-v1.2.0";
        assert_eq!(
            h.show(branch, "workforce/wasm/demo/src/lib.rs").await,
            "// v1"
        );
        assert!(h
            .show(branch, "workforce/wasm/demo/Skill.toml")
            .await
            .contains("version = \"1.2.0\""));
        let catalog =
            CatalogManifest::parse(&h.show(branch, "workforce/catalog.json").await).unwrap();
        assert_eq!(
            catalog.find("demo"),
            Some(&wasm_entry("demo", Some("1.2.0")))
        );
        assert!(catalog.find("hello").is_some());
        assert_eq!(
            sh_git(&h.bare, &["log", "-1", "--format=%s", branch]).await,
            "skill(demo): publish v1.2.0"
        );
        assert_eq!(out["commit"], sh_git(&h.bare, &["rev-parse", branch]).await);

        let opened = h.remote.opened.lock().unwrap();
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].head, branch);
        assert_eq!(opened[0].base, "main");
        assert!(opened[0].body.contains("new skill → v1.2.0"));
        assert!(opened[0].body.contains("first cut"));
    }

    #[tokio::test]
    async fn publishing_a_version_whose_branch_exists_is_refused() {
        let h = Harness::new().await;
        h.install_from_project("1.2.0", "// v1", b"wasm-1");
        assert!(
            h.publish(serde_json::json!({ "skill": "demo" }))
                .await
                .success
        );

        let again = h.publish(serde_json::json!({ "skill": "demo" })).await;
        assert!(!again.success);
        assert!(again
            .error
            .as_deref()
            .unwrap()
            .contains("skill/demo-v1.2.0 already exists"));
        assert_eq!(h.remote.opened.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn merged_version_with_identical_source_is_reported_unchanged() {
        let h = Harness::new().await;
        h.install_from_project("1.2.0", "// v1", b"wasm-1");
        assert!(
            h.publish(serde_json::json!({ "skill": "demo" }))
                .await
                .success
        );
        h.merge_into_main("skill/demo-v1.2.0").await;

        let again = h.publish(serde_json::json!({ "skill": "demo" })).await;
        assert!(again.success, "{:?}", again.error);
        assert_eq!(again.output["published"], false);
        assert_eq!(h.remote.opened.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn merged_version_with_drifted_source_is_refused_until_bumped() {
        let h = Harness::new().await;
        h.install_from_project("1.2.0", "// v1", b"wasm-1");
        assert!(
            h.publish(serde_json::json!({ "skill": "demo" }))
                .await
                .success
        );
        h.merge_into_main("skill/demo-v1.2.0").await;

        h.install_from_project("1.2.0", "// v1 edited", b"wasm-2");
        let drifted = h.publish(serde_json::json!({ "skill": "demo" })).await;
        assert!(!drifted.success);
        assert!(drifted
            .error
            .as_deref()
            .unwrap()
            .contains("Bump [skill] version"));

        h.install_from_project("1.3.0", "// v1 edited", b"wasm-3");
        let bumped = h.publish(serde_json::json!({ "skill": "demo" })).await;
        assert!(bumped.success, "{:?}", bumped.error);
        assert_eq!(bumped.output["previous_version"], "1.2.0");
        assert_eq!(bumped.output["files"]["updated"], 2);
        assert_eq!(
            h.show("skill/demo-v1.3.0", "workforce/wasm/demo/src/lib.rs")
                .await,
            "// v1 edited"
        );
    }

    #[tokio::test]
    async fn a_stale_snapshot_is_refused_before_touching_the_remote() {
        let h = Harness::new().await;
        h.install_from_project("1.2.0", "// v1", b"wasm-1");
        install(
            &h.ctx.store,
            &skill_toml("demo", "1.2.0", &[]),
            b"wasm-rebuilt-elsewhere",
        );

        let result = h.publish(serde_json::json!({ "skill": "demo" })).await;
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap()
            .contains("predates the installed binary"));
        assert!(h.remote.opened.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_skill_without_source_is_refused() {
        let h = Harness::new().await;
        install(&h.ctx.store, &skill_toml("demo", "1.0.0", &[]), b"wasm-1");
        let result = h.publish(serde_json::json!({ "skill": "demo" })).await;
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap()
            .contains("No source snapshot"));
    }
}
