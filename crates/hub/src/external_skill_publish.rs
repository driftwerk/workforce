//! `system.publish_external_skill` — open a catalog pull request holding an
//! installed external (SKILL.md) skill's files and its `catalog.json` entry,
//! through [`crate::skill_publish`].
//!
//! A SKILL.md skill's files cannot be enumerated the way a WASM project's can,
//! so what it publishes is everything in its installed directory except what
//! must never leave the hub: hidden entries at any depth (`.git`, the access
//! record, `.env`), `target/` directories, symbolic links, and the operator's
//! `config.toml` at the top of the directory.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::{bail, Report, Result};
use workforce_agent::tools::{Tool, ToolResult};
use workforce_core::access::AccessLevel;
use workforce_skills::catalog::SkillKind;
use workforce_skills::installer;
use workforce_skills::skill_file::SkillFile;

use crate::external_skills::ExternalSkill;
use crate::skill_access::SkillCaller;
use crate::skill_publish::{
    self, copy_files, publish_input_schema, source_line, version_line, Publication, PublishInput,
    PublishPlan, Published, Release, RemoteFactory, TreeDiff,
};
use crate::system_tool::SystemToolContext;

/// The files an external skill publishes, relative to its directory.
pub fn external_skill_files(dir: &Path) -> Result<Vec<(PathBuf, u64)>> {
    Ok(workforce_wasm_runtime::store::visible_files(dir)?
        .into_iter()
        .filter(|(path, _)| path != Path::new("config.toml"))
        .collect())
}

struct ExternalPublication<'a> {
    skill: &'a ExternalSkill,
}

impl Publication for ExternalPublication<'_> {
    const TOOL: &'static str = "system.publish_external_skill";
    const MANIFEST: &'static str = "SKILL.md";

    fn release(&self) -> Release {
        let meta = &self.skill.discovered.skill.meta;
        Release {
            kind: SkillKind::External,
            name: meta.name.clone(),
            version: meta.version.clone(),
            description: meta.description.clone(),
            requires_bins: Some(meta.requires.bins.clone()),
        }
    }

    fn governed_files(dir: &Path) -> Result<Vec<(PathBuf, u64)>> {
        external_skill_files(dir)
    }

    fn source_tree(&self, scratch: &Path) -> Result<PathBuf> {
        let dir = &self.skill.discovered.path;
        let files: Vec<PathBuf> = external_skill_files(dir)?
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        copy_files(dir, scratch, &files)?;
        Ok(scratch.to_path_buf())
    }

    fn read_published(&self, manifest: &Path) -> Result<Published> {
        SkillFile::parse(&std::fs::read_to_string(manifest)?).map(|file| Published {
            version: file.meta.version,
            contract_changed: false,
        })
    }

    fn body_lines(&self, plan: &PublishPlan, diff: &TreeDiff) -> Vec<String> {
        vec![version_line(&self.release(), plan), source_line(diff)]
    }
}

pub struct SystemPublishExternalSkillTool {
    ctx: SystemToolContext,
    remote_for: RemoteFactory,
}

impl SystemPublishExternalSkillTool {
    pub fn new(ctx: SystemToolContext, remote_for: RemoteFactory) -> Self {
        Self { ctx, remote_for }
    }

    /// The production tool: publishes to GitHub with the caller's credential.
    pub fn github(ctx: SystemToolContext) -> Self {
        Self::new(ctx, Arc::new(skill_publish::github_remote))
    }

    async fn publish(&self, params: PublishInput) -> Result<serde_json::Value> {
        let skill = SkillCaller::current(&self.ctx.skill_managers, &self.ctx.teams)
            .require(
                self.ctx.discovered_skills.as_ref(),
                &params.skill,
                AccessLevel::Manage,
            )
            .map_err(Report::msg)?;
        let meta = &skill.discovered.skill.meta;
        if !installer::is_directory_name(&meta.name) {
            bail!(
                "'{}' cannot name a catalog directory: a skill's name must be one directory \
                 name, with no `/` or `\\` and not `.` or `..`. Rename the skill in its SKILL.md, \
                 install it again and publish again.",
                meta.name
            );
        }
        if semver::Version::parse(&meta.version).is_err() {
            bail!(
                "'{}' has version '{}' in its SKILL.md, which is not semver (such as 1.0.0), so \
                 the catalog cannot compare it. Set a semver version, install it again and \
                 publish again.",
                meta.name,
                meta.version
            );
        }
        let token_for = |owner: &str| {
            crate::user_config::resolve_github_pat(
                Some(self.ctx.vault.as_ref()),
                self.ctx.global_vault.as_deref(),
                owner,
            )
        };
        skill_publish::publish(
            &ExternalPublication { skill: &skill },
            &self.ctx.config.skills.catalogs,
            &self.remote_for,
            &token_for,
            &params,
        )
        .await
    }
}

#[async_trait]
impl Tool for SystemPublishExternalSkillTool {
    fn name(&self) -> &str {
        "system.publish_external_skill"
    }

    fn description(&self) -> &str {
        "Open a pull request against a configured skill catalog repo containing an installed \
         external (SKILL.md) skill's files and its catalog.json entry, so a skill written in chat \
         lands under source control. Deterministic: publishes exactly what is installed, leaving \
         out config.toml and hidden files. Needs manage access to the skill, and uses your GitHub \
         token for the catalog repo's owner. Refused when the installed version is already in \
         the catalog with different files — bump version in SKILL.md, install it again, publish \
         again. A WASM skill publishes with wasm_skill.publish."
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
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Mutex;

    use super::*;
    use crate::external_skills::tests::share;
    use crate::skill_access::SkillRegistry;
    use crate::skill_publish::tests::{catalog_source, seed_origin, sh_git, LocalRemote};
    use crate::skill_publish::{github_remote, PublishRemote};
    use workforce_common::settings::WorkforceConfig;
    use workforce_core::access::{GrantLevel, Visibility};
    use workforce_skills::catalog::CatalogManifest;
    use workforce_skills::installer::InstallOptions;

    fn skill_md(version: &str) -> String {
        format!(
            "---\nname: hello-world\nversion: {version}\ndescription: Says hello\nrequires:\n  \
             bins: [git]\n---\nSay hello.\n"
        )
    }

    #[test]
    fn external_files_leave_out_config_hidden_entries_target_and_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path();
        std::fs::create_dir_all(dir.join("docs/.cache")).unwrap();
        std::fs::create_dir_all(dir.join("target")).unwrap();
        std::fs::write(dir.join("SKILL.md"), skill_md("1.0.0")).unwrap();
        std::fs::write(dir.join("config.toml"), "token = \"x\"\n").unwrap();
        std::fs::write(dir.join("docs/config.toml"), "example = true\n").unwrap();
        std::fs::write(dir.join("docs/.cache/entry"), "cache").unwrap();
        std::fs::write(dir.join(".env"), "SECRET=1").unwrap();
        std::fs::write(dir.join(installer::ACCESS_FILE), "{}").unwrap();
        std::fs::write(dir.join("target/out.bin"), "bin").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", dir.join("docs/link.md")).unwrap();

        let mut files: Vec<PathBuf> = external_skill_files(dir)
            .unwrap()
            .into_iter()
            .map(|(path, _)| path)
            .collect();
        files.sort();
        assert_eq!(
            files,
            [PathBuf::from("SKILL.md"), PathBuf::from("docs/config.toml")]
        );
    }

    struct Harness {
        root: tempfile::TempDir,
        bare: PathBuf,
        ctx: SystemToolContext,
        remote: Arc<LocalRemote>,
        remotes_built: Arc<AtomicUsize>,
    }

    impl Harness {
        async fn new() -> Self {
            let root = tempfile::tempdir().unwrap();
            let bare = seed_origin(root.path()).await;
            let mut config = WorkforceConfig::default();
            config.skills.catalogs = vec![catalog_source("test")];
            let ctx = SystemToolContext {
                skill_managers: crate::authz::never_admin(),
                ..crate::system_tool::tests::test_context(root.path(), config)
            };
            let remote = Arc::new(LocalRemote {
                url: format!("file://{}", bare.display()),
                opened: Mutex::new(Vec::new()),
            });
            Self {
                root,
                bare,
                ctx,
                remote,
                remotes_built: Arc::default(),
            }
        }

        fn tool(&self) -> SystemPublishExternalSkillTool {
            let (remote, built) = (Arc::clone(&self.remote), Arc::clone(&self.remotes_built));
            SystemPublishExternalSkillTool::new(
                self.ctx.clone(),
                Arc::new(move |_, _| {
                    built.fetch_add(1, Ordering::SeqCst);
                    Ok(Arc::clone(&remote) as Arc<dyn PublishRemote>)
                }),
            )
        }

        /// Install `hello-world` for alice from a source holding `files` beside its SKILL.md.
        async fn install(&self, version: &str, files: &[(&str, &str)]) {
            self.install_from(version, &skill_md(version), files).await
        }

        /// Install for alice from a `hello-world` directory under `label` holding `skill_md` and
        /// `files`.
        async fn install_from(&self, label: &str, skill_md: &str, files: &[(&str, &str)]) {
            let source = self
                .root
                .path()
                .join("sources")
                .join(label)
                .join("hello-world");
            std::fs::create_dir_all(&source).unwrap();
            std::fs::write(source.join("SKILL.md"), skill_md).unwrap();
            for (path, content) in files {
                let file = source.join(path);
                std::fs::create_dir_all(file.parent().unwrap()).unwrap();
                std::fs::write(file, content).unwrap();
            }
            let located =
                installer::locate_skill(source.to_str().unwrap(), &InstallOptions::default())
                    .unwrap();
            self.ctx
                .discovered_skills
                .install(
                    SkillCaller::local(Some("alice".into()), false),
                    located,
                    None,
                    None,
                )
                .await
                .unwrap();
        }

        async fn publish(&self, uid: &str, input: serde_json::Value) -> ToolResult {
            workforce_agent::runner::CURRENT_USER_ID
                .scope(uid.to_string(), self.tool().execute(input))
                .await
                .unwrap()
        }

        async fn show(&self, git_ref: &str, path: &str) -> String {
            sh_git(&self.bare, &["show", &format!("{git_ref}:{path}")]).await
        }

        async fn has(&self, git_ref: &str, path: &str) -> bool {
            hardened_ls_tree(&self.bare, git_ref)
                .await
                .iter()
                .any(|listed| listed == path)
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

    async fn hardened_ls_tree(bare: &Path, git_ref: &str) -> Vec<String> {
        sh_git(bare, &["ls-tree", "-r", "--name-only", git_ref])
            .await
            .lines()
            .map(str::to_string)
            .collect()
    }

    const EXTRA_FILES: &[(&str, &str)] = &[
        ("config.toml", "token = \"x\"\n"),
        (".env", "SECRET=1"),
        ("docs/guide.md", "How to say hello."),
    ];

    #[tokio::test]
    async fn publishing_pushes_the_installed_files_and_entry_and_opens_a_pr() {
        let h = Harness::new().await;
        h.install("1.0.0", EXTRA_FILES).await;

        let result = h
            .publish(
                "alice",
                serde_json::json!({ "skill": "hello-world", "summary": "first cut" }),
            )
            .await;
        assert!(result.success, "{:?}", result.error);
        let out = &result.output;
        let branch = "skill/hello-world-v1.0.0";
        assert_eq!(out["published"], true);
        assert_eq!(out["branch"], branch);
        assert_eq!(out["subpath"], "workforce/external/hello-world");
        assert_eq!(out["files"]["added"], 2);
        assert!(out.get("contract_changed").is_none());

        assert_eq!(
            h.show(branch, "workforce/external/hello-world/SKILL.md")
                .await,
            skill_md("1.0.0").trim_end()
        );
        assert!(
            h.has(branch, "workforce/external/hello-world/docs/guide.md")
                .await
        );
        for left_out in ["config.toml", ".env", installer::ACCESS_FILE] {
            assert!(
                !h.has(
                    branch,
                    &format!("workforce/external/hello-world/{left_out}")
                )
                .await,
                "{left_out} was published"
            );
        }
        let catalog =
            CatalogManifest::parse(&h.show(branch, "workforce/catalog.json").await).unwrap();
        let entry = catalog.find("hello-world").unwrap();
        assert_eq!(entry.kind, SkillKind::External);
        assert_eq!(entry.version.as_deref(), Some("1.0.0"));
        assert_eq!(entry.description, "Says hello");
        assert_eq!(entry.requires_bins, vec!["git".to_string()]);

        let opened = h.remote.opened.lock().unwrap();
        assert_eq!(opened.len(), 1);
        assert_eq!(
            opened[0].body,
            "Publishes `hello-world` v1.0.0 from the workforce hub \
             (`system.publish_external_skill`).\n\n- Version: new skill → v1.0.0\n- Source: 2 \
             added, 0 updated, 0 removed\n\n## Notes\n\nfirst cut\n"
        );
    }

    #[tokio::test]
    async fn publishing_needs_manage_access() {
        let h = Harness::new().await;
        h.install("1.0.0", &[]).await;
        h.ctx
            .discovered_skills
            .set_access(
                "hello-world",
                workforce_core::access::AccessWrite::kept(
                    Visibility::Private,
                    vec![share("bob", GrantLevel::Run)],
                ),
            )
            .unwrap();

        let refused = h
            .publish("bob", serde_json::json!({ "skill": "hello-world" }))
            .await;
        assert!(!refused.success);
        assert!(refused
            .error
            .as_deref()
            .unwrap()
            .contains("belongs to another user"));
        assert_eq!(h.remotes_built.load(Ordering::SeqCst), 0);
        assert!(h.remote.opened.lock().unwrap().is_empty());
        assert_eq!(sh_git(&h.bare, &["branch", "--list", "skill/*"]).await, "");
    }

    #[tokio::test]
    async fn a_merged_version_with_changed_files_is_refused_until_bumped() {
        let h = Harness::new().await;
        h.install("1.0.0", &[("docs/guide.md", "v1")]).await;
        assert!(
            h.publish("alice", serde_json::json!({ "skill": "hello-world" }))
                .await
                .success
        );
        h.merge_into_main("skill/hello-world-v1.0.0").await;

        h.install("1.0.0", &[("docs/guide.md", "v1 edited")]).await;
        let drifted = h
            .publish("alice", serde_json::json!({ "skill": "hello-world" }))
            .await;
        assert_eq!(
            drifted.error.as_deref(),
            Some(
                "'hello-world' v1.0.0 is already in the catalog with different files. Bump \
                 version in SKILL.md, install it again (system.install_external_skill) and \
                 publish again."
            )
        );

        h.install("1.1.0", &[("docs/guide.md", "v1 edited")]).await;
        let bumped = h
            .publish("alice", serde_json::json!({ "skill": "hello-world" }))
            .await;
        assert!(bumped.success, "{:?}", bumped.error);
        assert_eq!(bumped.output["previous_version"], "1.0.0");
        assert_eq!(bumped.output["files"]["updated"], 2);
    }

    #[tokio::test]
    async fn republishing_identical_files_reports_unchanged() {
        let h = Harness::new().await;
        h.install("1.0.0", EXTRA_FILES).await;
        assert!(
            h.publish("alice", serde_json::json!({ "skill": "hello-world" }))
                .await
                .success
        );
        h.merge_into_main("skill/hello-world-v1.0.0").await;

        let again = h
            .publish("alice", serde_json::json!({ "skill": "hello-world" }))
            .await;
        assert!(again.success, "{:?}", again.error);
        assert_eq!(again.output["published"], false);
        assert_eq!(h.remote.opened.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn an_existing_branch_is_refused() {
        let h = Harness::new().await;
        h.install("1.0.0", &[]).await;
        assert!(
            h.publish("alice", serde_json::json!({ "skill": "hello-world" }))
                .await
                .success
        );

        let again = h
            .publish("alice", serde_json::json!({ "skill": "hello-world" }))
            .await;
        assert!(again
            .error
            .as_deref()
            .unwrap()
            .contains("Branch skill/hello-world-v1.0.0 already exists"));
        assert_eq!(h.remote.opened.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_catalog_entry_of_the_other_kind_is_refused() {
        let h = Harness::new().await;
        let seed = h.root.path().join("seed");
        let mut catalog = CatalogManifest::parse(crate::skill_publish::tests::CATALOG).unwrap();
        catalog.upsert(workforce_skills::catalog::CatalogEntry {
            name: "hello-world".into(),
            kind: SkillKind::Wasm,
            subpath: "workforce/wasm/hello-world".into(),
            version: Some("0.1.0".into()),
            description: "A WASM skill".into(),
            requires_bins: vec![],
            wasm_path: None,
            sha256: None,
        });
        std::fs::write(
            seed.join("workforce/catalog.json"),
            catalog.to_json_pretty().unwrap(),
        )
        .unwrap();
        sh_git(&seed, &["commit", "-q", "-am", "list a WASM skill"]).await;
        sh_git(&seed, &["push", "-q", "origin", "main"]).await;
        h.install("1.0.0", &[]).await;

        let refused = h
            .publish("alice", serde_json::json!({ "skill": "hello-world" }))
            .await;
        assert_eq!(
            refused.error.as_deref(),
            Some(
                "The catalog lists 'hello-world' as a WASM skill; an external (SKILL.md) skill \
                 cannot take its entry."
            )
        );
        assert!(h.remote.opened.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn a_non_semver_version_is_refused_before_the_remote() {
        let h = Harness::new().await;
        h.install("latest", &[]).await;

        let refused = h
            .publish("alice", serde_json::json!({ "skill": "hello-world" }))
            .await;
        assert_eq!(
            refused.error.as_deref(),
            Some(
                "'hello-world' has version 'latest' in its SKILL.md, which is not semver (such as \
                 1.0.0), so the catalog cannot compare it. Set a semver version, install it \
                 again and publish again."
            )
        );
        assert_eq!(h.remotes_built.load(Ordering::SeqCst), 0);
    }

    #[tokio::test]
    async fn a_name_that_is_not_one_directory_is_refused_before_the_remote() {
        let h = Harness::new().await;
        let escaped = format!("escaped-{}", uuid::Uuid::new_v4());
        let name = format!("../../../../{escaped}");
        h.install_from(
            "escape",
            &format!("---\nname: {name}\nversion: 1.0.0\ndescription: Escapes\n---\nBody\n"),
            &[],
        )
        .await;

        let refused = h
            .publish("alice", serde_json::json!({ "skill": name }))
            .await;
        assert_eq!(
            refused.error,
            Some(format!(
                "'{name}' cannot name a catalog directory: a skill's name must be one directory \
                 name, with no `/` or `\\` and not `.` or `..`. Rename the skill in its SKILL.md, \
                 install it again and publish again."
            ))
        );
        assert_eq!(h.remotes_built.load(Ordering::SeqCst), 0);
        assert!(!std::env::temp_dir().join(&escaped).exists());
    }

    #[tokio::test]
    async fn github_remote_needs_the_owners_token_through_the_resolver() {
        let h = Harness::new().await;
        h.install("1.0.0", &[]).await;
        let tool = |ctx: SystemToolContext| {
            SystemPublishExternalSkillTool::new(
                ctx,
                Arc::new(|source, token_for| {
                    github_remote(source, token_for)
                        .and_then(|_| Err("resolved a token for acme".to_string()))
                }),
            )
        };
        let publish = |ctx: SystemToolContext| async move {
            workforce_agent::runner::CURRENT_USER_ID
                .scope(
                    "alice".to_string(),
                    tool(ctx).execute(serde_json::json!({ "skill": "hello-world" })),
                )
                .await
                .unwrap()
                .error
                .unwrap_or_default()
        };

        assert_eq!(
            publish(h.ctx.clone()).await,
            "No GitHub token to publish to acme/skills. Store one at \
             skills/github/workspaces/acme/token (or skills/github/token): publishing writes to \
             the catalog repo with your own credential, not the catalog's read token."
        );
        h.ctx
            .vault
            .set("skills/github/workspaces/acme/token", "acme-token")
            .unwrap();
        assert_eq!(publish(h.ctx.clone()).await, "resolved a token for acme");
    }
}
