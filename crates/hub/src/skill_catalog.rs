//! Skill catalog: list and install skills from the curated Git repos in `[[skills.catalogs]]`.
//! One orchestration serves the management API/UI, the agent tools and the CLI. Inside the hub an
//! install is live on every agent that can use it at once; the CLI, running beside the hub, writes
//! to disk for the hub's next start.
//!
//! Both kinds are handled:
//! - **external** (`SKILL.md`) → cloned into the skills directory, authenticated with the
//!   catalog's vault token.
//! - **wasm** (`Skill.toml` + `.wasm`) → the prebuilt binary is fetched and its `sha256` pin
//!   verified — or, for an entry without an artefact, the skill is cloned at the catalog ref and
//!   compiled here — then written to the WASM store.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use eyre::Report;
use serde::Serialize;
use workforce_agent::tools::exec_runner::RunnerEndpoint;
use workforce_common::settings::{CatalogSource, WorkforceConfig};
use workforce_skills::catalog::{self, CatalogEntry, CatalogManifest, SkillKind};
use workforce_skills::installer::{self, InstallOptions, Installed, ManifestKind, SkillSource};
use workforce_wasm_runtime::manifest::SkillManifest;
use workforce_wasm_runtime::{InstalledWasmSkill, SkillOwnership, WasmSkillEngine, WasmSkillStore};

use crate::external_skills::{ExternalSkill, ExternalSkills};
use crate::skill_access::SkillCaller;
use crate::vault::Vault;
use crate::wasm_skill_tools::{FetchedSkill, SkillHosts, WasmInstallPolicy};

/// One catalog skill annotated with whether it is already installed.
#[derive(Debug, Serialize)]
pub struct CatalogSkillView {
    pub catalog: String,
    pub name: String,
    pub kind: SkillKind,
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub requires_bins: Vec<String>,
    pub installed: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub installed_version: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub built_from_source: bool,
}

/// Result of listing a single catalog — its skills, or an error if the manifest
/// could not be fetched (one unreachable catalog never fails the whole list).
#[derive(Debug, Serialize)]
pub struct CatalogResult {
    pub name: String,
    pub repo: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub skills: Vec<CatalogSkillView>,
}

/// The full catalog listing across every configured source.
#[derive(Debug, Serialize)]
pub struct CatalogListing {
    pub catalogs: Vec<CatalogResult>,
}

/// Outcome of installing one catalog skill.
#[derive(Debug, Serialize)]
pub struct InstallOutcome {
    pub catalog: String,
    pub skill: String,
    pub kind: SkillKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    pub installed: bool,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub replaced: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_version: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub built_from_source: bool,
    /// Whether every agent on this hub can use the skill now.
    pub active: bool,
    /// Human-readable note on when the skill becomes active.
    pub activation: String,
}

/// Where an install lands and how it takes effect.
pub enum SkillStores {
    /// This hub process: installs are live on every agent at once.
    Live(Arc<ExternalSkills>),
    /// A process beside the hub, such as the CLI: installs are written for the hub's next start.
    Offline {
        skills_dir: PathBuf,
        wasm_store_dir: PathBuf,
    },
}

/// Who is installing, which decides the checks a source-built WASM entry must pass.
pub enum InstallAuthority {
    /// The management API, UI or CLI: the catalog and its ref are the operator's trust decision.
    Operator,
    /// An agent turn: the caller's standing and `[skills.wasm]` policy apply, as for
    /// `wasm_skill.install`.
    Agent {
        caller: SkillCaller,
        policy: WasmInstallPolicy,
    },
}

/// The configured catalogs, the vault holding their tokens, and where installs land.
pub struct SkillCatalog {
    catalogs: Vec<CatalogSource>,
    vault: Option<Arc<Vault>>,
    runner: Option<RunnerEndpoint>,
    stores: SkillStores,
}

const LIVE: &str = "live on every agent now";
const STAGED: &str = "written from outside the hub, so a running hub loads it when it next starts";
const NOT_LISTED: &str = "installed, but not in the hub's live skill list yet; see the hub log";

/// A catalog entry resolved against its catalog, ready to install.
struct ResolvedEntry<'a> {
    source: &'a CatalogSource,
    owner_repo: String,
    repo_url: String,
    token: Option<String>,
    entry: CatalogEntry,
}

struct WasmTarget {
    store: Arc<WasmSkillStore>,
    engine: Option<Arc<WasmSkillEngine>>,
    /// The agents a live install is activated on.
    hosts: SkillHosts,
}

impl SkillStores {
    fn skills_dir(&self) -> &Path {
        match self {
            Self::Live(external) => external.skills_dir(),
            Self::Offline { skills_dir, .. } => skills_dir,
        }
    }

    /// The hub's store and engine are looked up per call: the tool context is built before the
    /// serve path opens the store, and a test gateway has none.
    fn wasm(&self) -> eyre::Result<WasmTarget> {
        match self {
            Self::Live(_) => crate::wasm_skill_tools::global_skill_store()
                .and_then(|store| {
                    crate::wasm_skill_tools::global_skill_engine().map(|engine| WasmTarget {
                        store,
                        engine: Some(engine),
                        hosts: SkillHosts::process(),
                    })
                })
                .ok_or_else(|| eyre::eyre!("WASM skills are not available on this hub")),
            Self::Offline { wasm_store_dir, .. } => Ok(WasmTarget {
                store: Arc::new(WasmSkillStore::open(wasm_store_dir)?),
                engine: None,
                hosts: SkillHosts::default(),
            }),
        }
    }

    /// Swap a located external skill into the skills directory for the whole org, keeping the
    /// owner and sharing of a skill someone owns, and refreshing the live set whatever the outcome
    /// when this is the hub.
    async fn place_external(&self, located: SkillSource) -> eyre::Result<Installed> {
        match self {
            Self::Live(external) => external
                .install(SkillCaller::local(None, true), located, None, None)
                .await
                .map_err(Report::new),
            Self::Offline { skills_dir, .. } => {
                let skills_dir = skills_dir.clone();
                tokio::task::spawn_blocking(move || {
                    crate::external_skills::place_for_org(&skills_dir, &located)
                })
                .await?
            }
        }
    }

    /// The external skills agents see now, or `None` outside the hub.
    fn live(&self) -> Option<Arc<[ExternalSkill]>> {
        match self {
            Self::Live(external) => Some(external.snapshot()),
            Self::Offline { .. } => None,
        }
    }
}

impl InstallAuthority {
    fn caller(&self) -> SkillCaller {
        match self {
            Self::Operator => SkillCaller::local(None, true),
            Self::Agent { caller, .. } => caller.clone(),
        }
    }

    /// Whether this authority may have the hub compile `skill` from the catalog's source. The
    /// host-compile check is repeated here because the capability gate only sees the tool input,
    /// which never says that an entry builds from source.
    fn permit_source_build(&self, skill: &str, git_ref: &str) -> Result<(), String> {
        let Self::Agent { caller, policy } = self else {
            return Ok(());
        };
        if !policy.allow_build_on_install {
            return Err(format!(
                "'{skill}' is built from source when installed, and this hub does not build on \
                 install ([skills.wasm] allow_build_on_install = false). An operator can install \
                 it from the Skills page, or the catalog can ship a prebuilt .wasm with its sha256."
            ));
        }
        if policy.require_sha256_pin && !installer::is_commit_sha(git_ref) {
            return Err(format!(
                "'{skill}' is built from source at the catalog's ref '{git_ref}', which is not a \
                 commit, and this hub requires every install to be pinned ([skills.wasm] \
                 require_sha256_pin = true). Pin the catalog's ref to a 40-hex commit, or ship a \
                 prebuilt .wasm with its sha256."
            ));
        }
        crate::wasm_skill_access::require_host_compile(
            caller,
            crate::wasm_skill_access::build_isolation(),
        )
    }
}

impl SkillCatalog {
    pub fn live(
        config: &WorkforceConfig,
        vault: Option<Arc<Vault>>,
        external: Arc<ExternalSkills>,
    ) -> Self {
        Self::with_stores(config, vault, SkillStores::Live(external))
    }

    pub fn offline(config: &WorkforceConfig, vault: Option<Arc<Vault>>) -> Self {
        let stores = SkillStores::Offline {
            skills_dir: config.skills.resolved_skills_dir(&config.hub.data_dir),
            wasm_store_dir: PathBuf::from(&config.hub.data_dir).join("wasm-skills"),
        };
        Self::with_stores(config, vault, stores)
    }

    fn with_stores(
        config: &WorkforceConfig,
        vault: Option<Arc<Vault>>,
        stores: SkillStores,
    ) -> Self {
        Self {
            catalogs: config.skills.catalogs.clone(),
            vault,
            runner: RunnerEndpoint::from_config(&config.exec),
            stores,
        }
    }

    /// Every skill in `catalog`, or in every catalog when `None`, annotated with install state.
    /// A catalog whose manifest can't be fetched is reported with an error rather than failing
    /// the whole listing.
    pub async fn list(&self, catalog: Option<&str>) -> eyre::Result<CatalogListing> {
        let sources: Vec<&CatalogSource> = match catalog {
            None => self.catalogs.iter().collect(),
            Some(name) => vec![select_catalog(&self.catalogs, Some(name)).map_err(Report::msg)?],
        };
        let wasm_store = self.stores.wasm().ok().map(|target| target.store);
        let wasm_store = wasm_store.as_deref();
        let catalogs = futures::future::join_all(sources.into_iter().map(|source| async move {
            let token = resolve_token(self.vault.as_ref(), &source.name);
            let (error, skills) = match fetch_manifest(source, token.as_deref()).await {
                Ok(manifest) => (
                    None,
                    annotate(
                        &source.name,
                        &manifest,
                        self.stores.skills_dir(),
                        wasm_store,
                    ),
                ),
                Err(e) => (Some(e.to_string()), Vec::new()),
            };
            CatalogResult {
                name: source.name.clone(),
                repo: source.repo.clone(),
                error,
                skills,
            }
        }))
        .await;
        Ok(CatalogListing { catalogs })
    }

    /// Install `skill` from `catalog` (optional when only one is configured), replacing an
    /// installed copy with the catalog's version.
    pub async fn install(
        &self,
        authority: &InstallAuthority,
        catalog: Option<&str>,
        skill: &str,
    ) -> eyre::Result<InstallOutcome> {
        let resolved = self.resolve(catalog, skill).await?;
        match resolved.entry.kind {
            SkillKind::External => self.install_external(resolved).await,
            SkillKind::Wasm => self.install_wasm(authority, resolved).await,
        }
    }

    async fn resolve(&self, catalog: Option<&str>, skill: &str) -> eyre::Result<ResolvedEntry<'_>> {
        let source = select_catalog(&self.catalogs, catalog).map_err(Report::msg)?;
        let owner_repo = catalog::github_owner_repo(&source.repo)?;
        let token = resolve_token(self.vault.as_ref(), &source.name);
        let manifest = fetch_manifest(source, token.as_deref()).await?;
        let entry = manifest
            .find(skill)
            .cloned()
            .ok_or_else(|| eyre::eyre!("skill '{skill}' not found in catalog '{}'", source.name))?;
        Ok(ResolvedEntry {
            source,
            repo_url: format!("https://github.com/{owner_repo}"),
            owner_repo,
            token,
            entry,
        })
    }

    /// Clone and check the skill before anything is written, so a bad entry never disturbs a
    /// working install, then swap it into place and refresh the live set whatever the outcome.
    async fn install_external(&self, resolved: ResolvedEntry<'_>) -> eyre::Result<InstallOutcome> {
        let ResolvedEntry {
            source,
            repo_url,
            token,
            entry,
            ..
        } = resolved;
        let origin = entry.external_source(&repo_url);
        let (git_ref, name) = (source.git_ref.clone(), entry.name.clone());
        let located = tokio::task::spawn_blocking(move || {
            let located = installer::locate_skill(
                &origin,
                &InstallOptions {
                    token: token.as_deref(),
                    git_ref: Some(&git_ref),
                },
            )?;
            eyre::ensure!(
                located.kind() == ManifestKind::Markdown,
                "catalog entry '{name}' is listed as external, but its directory has no SKILL.md"
            );
            ensure_identity(&name, located.name()).map(|()| located)
        })
        .await??;

        let installed = self.stores.place_external(located).await?;
        let (active, activation) =
            external_activation(self.stores.live().as_deref(), &installed.path);

        Ok(InstallOutcome {
            catalog: source.name.clone(),
            skill: entry.name,
            kind: SkillKind::External,
            version: installer::installed_version(&installed.path),
            installed: true,
            replaced: installed.replaced,
            previous_version: installed.previous_version,
            built_from_source: false,
            active,
            activation,
        })
    }

    async fn install_wasm(
        &self,
        authority: &InstallAuthority,
        resolved: ResolvedEntry<'_>,
    ) -> eyre::Result<InstallOutcome> {
        if resolved.entry.builds_from_source() {
            authority
                .permit_source_build(&resolved.entry.name, &resolved.source.git_ref)
                .map_err(Report::msg)?;
        }
        let target = self.stores.wasm()?;
        let caller = authority.caller();
        // Refuse a name this caller cannot claim before the fetch, which can be a long build.
        caller
            .may_install(&target.store, &resolved.entry.name)
            .map_err(Report::msg)?;
        let fetched = fetch_wasm(&resolved, self.runner.as_ref()).await?;
        // Ask again after it, so the record replaced is the one on disk now and a config change
        // made meanwhile is kept — and so one install records one claim.
        let previous = caller
            .require_install(&target.store, &resolved.entry.name)
            .map_err(Report::msg)?;
        stage_wasm(&target, previous, &resolved, fetched).await
    }
}

/// Pick the catalog `name` names, or the only one configured when it names none.
pub(crate) fn select_catalog<'a>(
    catalogs: &'a [CatalogSource],
    name: Option<&str>,
) -> Result<&'a CatalogSource, String> {
    let names = || {
        catalogs
            .iter()
            .map(|c| c.name.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
    match (name, catalogs) {
        (_, []) => Err("No [[skills.catalogs]] is configured on this hub.".to_string()),
        (Some(wanted), _) => catalogs
            .iter()
            .find(|c| c.name == wanted)
            .ok_or_else(|| format!("Unknown catalog '{wanted}'. Configured: {}", names())),
        (None, [only]) => Ok(only),
        (None, _) => Err(format!(
            "Several catalogs are configured ({}); pass `catalog` to choose one.",
            names()
        )),
    }
}

/// The skill's manifest and binary: raw-fetched for a prebuilt entry, or cloned at the catalog
/// ref and compiled for one without an artefact.
async fn fetch_wasm(
    resolved: &ResolvedEntry<'_>,
    runner: Option<&RunnerEndpoint>,
) -> eyre::Result<FetchedSkill> {
    let ResolvedEntry {
        source,
        owner_repo,
        token,
        entry,
        ..
    } = resolved;
    match entry.wasm_repo_path() {
        Some(wasm_path) => {
            let manifest_url =
                catalog::raw_url(owner_repo, &source.git_ref, &entry.manifest_repo_path());
            let wasm_url = catalog::raw_url(owner_repo, &source.git_ref, &wasm_path);
            let toml = catalog::fetch_text(&manifest_url, token.as_deref()).await?;
            Ok(FetchedSkill {
                manifest: SkillManifest::from_toml(&toml)?,
                wasm_bytes: catalog::fetch_bytes(&wasm_url, token.as_deref()).await?,
                commit: None,
            })
        }
        None => {
            crate::wasm_skill_tools::fetch_from_github(
                &source_build_ref(owner_repo, &source.git_ref, &entry.subpath),
                token.as_deref(),
                runner,
                true,
            )
            .await
        }
    }
}

/// Write a fetched skill to the store, keeping the previous version's config, and activate it on
/// every agent when the target has an engine. An owner and sharing already on the record are kept
/// by the store; the operator ownership here applies only to a first install.
async fn stage_wasm(
    target: &WasmTarget,
    previous: Option<InstalledWasmSkill>,
    resolved: &ResolvedEntry<'_>,
    fetched: FetchedSkill,
) -> eyre::Result<InstallOutcome> {
    let ResolvedEntry {
        source,
        owner_repo,
        entry,
        ..
    } = resolved;
    ensure_identity(&entry.name, &fetched.manifest.name)?;
    let FetchedSkill {
        manifest,
        wasm_bytes,
        commit,
    } = fetched;
    let config = previous
        .as_ref()
        .map(|p| p.config.clone())
        .unwrap_or_default();
    let installed = target.store.install_prebuilt(
        manifest,
        &wasm_bytes,
        entry.sha256.as_deref(),
        wasm_provenance(&source.name, owner_repo, &entry.subpath, commit.as_deref()),
        config.clone(),
        SkillOwnership::operator(),
    )?;
    let active = match &target.engine {
        Some(engine) => crate::wasm_skill_tools::activate_stored_skill(
            &target.hosts,
            engine,
            previous.as_ref().map(|p| p.binary_version),
            &installed.manifest,
            installed.binary_version,
            &wasm_bytes,
            &config,
        )
        .await
        .map(|_| true)
        .map_err(Report::msg)?,
        None => false,
    };
    let when = if active { LIVE } else { STAGED };
    let activation = match &commit {
        Some(commit) => format!("built from source at {commit}, {when}"),
        None => when.to_string(),
    };
    Ok(InstallOutcome {
        catalog: source.name.clone(),
        skill: installed.manifest.name,
        kind: SkillKind::Wasm,
        version: Some(installed.manifest.version),
        installed: true,
        replaced: previous.is_some(),
        previous_version: previous.map(|p| p.manifest.version),
        built_from_source: commit.is_some(),
        active,
        activation,
    })
}

/// A catalog entry's name is the install identity, so it must be the skill's own manifest name.
fn ensure_identity(entry: &str, manifest: &str) -> eyre::Result<()> {
    eyre::ensure!(
        entry == manifest,
        "catalog entry '{entry}' does not match the skill's manifest name '{manifest}' — fix the \
         catalog so they match"
    );
    Ok(())
}

/// Whether agents can use the external skill installed at `path`, read from the live set after
/// the refresh (`None` outside the hub).
fn external_activation(live: Option<&[ExternalSkill]>, path: &Path) -> (bool, String) {
    match live.map(|skills| skills.iter().find(|skill| skill.discovered.path == path)) {
        None => (false, STAGED.to_string()),
        Some(None) => (false, NOT_LISTED.to_string()),
        Some(Some(skill)) => match skill.discovered.unavailable_reason() {
            None => (true, LIVE.to_string()),
            Some(reason) => (
                false,
                format!("installed, but agents cannot use it until this is resolved: {reason}"),
            ),
        },
    }
}

/// The `owner/repo@ref#subpath=dir` reference a source-built catalog entry
/// hands to the shared GitHub clone-and-build path.
fn source_build_ref(owner_repo: &str, git_ref: &str, subpath: &str) -> String {
    format!("{owner_repo}@{git_ref}#subpath={subpath}")
}

/// Install provenance recorded in the WASM store: the catalog, repo and subpath,
/// plus the exact commit when the hub compiled the skill itself.
fn wasm_provenance(catalog: &str, owner_repo: &str, subpath: &str, commit: Option<&str>) -> String {
    let at = commit.map(|c| format!("@{c}")).unwrap_or_default();
    format!("catalog:{catalog}:{owner_repo}{at}#{subpath}")
}

/// Resolve the token for reading a catalog's repo. This is a dedicated
/// infrastructure credential at `skills/catalog/{name}/token` — deliberately
/// separate from `skills/github/*` (where users' own GitHub tokens live) so a
/// catalog read can never pick up, or be confused with, a user's token. Read
/// from the shared vault — a catalog repo is an org credential, not per-user.
fn resolve_token(vault: Option<&Arc<Vault>>, catalog_name: &str) -> Option<String> {
    vault?.get(&format!("skills/catalog/{catalog_name}/token"))
}

/// Annotate manifest entries with installed state from disk. Pure given the
/// stores — the network fetch happens in the caller. `entry.name` is the
/// install identity (enforced to equal the skill's manifest name at install),
/// so it keys both the skills-dir and WASM-store lookups. A `None` store (open
/// failed) reports wasm skills as not-installed.
fn annotate(
    catalog: &str,
    manifest: &CatalogManifest,
    skills_dir: &Path,
    wasm_store: Option<&WasmSkillStore>,
) -> Vec<CatalogSkillView> {
    manifest
        .skills
        .iter()
        .map(|entry| {
            let (installed, installed_version) = match entry.kind {
                SkillKind::External => {
                    let dir = skills_dir.join(&entry.name);
                    (
                        dir.join("SKILL.md").is_file(),
                        installer::installed_version(&dir),
                    )
                }
                SkillKind::Wasm => match wasm_store.and_then(|s| s.get_installed(&entry.name)) {
                    Some(i) => (true, Some(i.manifest.version)),
                    None => (false, None),
                },
            };
            CatalogSkillView {
                catalog: catalog.to_string(),
                name: entry.name.clone(),
                kind: entry.kind,
                description: entry.description.clone(),
                version: entry.version.clone(),
                requires_bins: entry.requires_bins.clone(),
                installed,
                installed_version,
                built_from_source: entry.builds_from_source(),
            }
        })
        .collect()
}

/// Fetch and parse a catalog's manifest at the catalog's ref.
async fn fetch_manifest(
    source: &CatalogSource,
    token: Option<&str>,
) -> eyre::Result<CatalogManifest> {
    catalog::fetch_manifest(&source.repo, &source.git_ref, &source.manifest, token).await
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use std::process::Command;

    use workforce_agent::tools::registry::ToolRegistry;

    use crate::skill_state::SkillStateTracker;

    const EXTERNAL_SUBPATH: &str = "workforce/external/hello-world";
    pub(crate) const COMMIT: &str = "0123456789abcdef0123456789abcdef01234567";

    fn manifest() -> CatalogManifest {
        CatalogManifest::parse(
            r#"{"skills":[
                {"name":"ext","kind":"external","subpath":"workforce/external/ext"},
                {"name":"ext-missing","kind":"external","subpath":"workforce/external/ext-missing"},
                {"name":"w","kind":"wasm","subpath":"workforce/wasm/w","wasm_path":"w.wasm","sha256":"d"},
                {"name":"src","kind":"wasm","subpath":"workforce/wasm/src"}
            ]}"#,
        )
        .unwrap()
    }

    fn catalog_source(name: &str) -> CatalogSource {
        CatalogSource {
            name: name.into(),
            repo: "https://github.com/acme/skills".into(),
            git_ref: "main".into(),
            manifest: "workforce/catalog.json".into(),
        }
    }

    pub(crate) fn skill_md(name: &str, version: &str) -> String {
        format!("---\nname: {name}\nversion: {version}\ndescription: Says hello\n---\nSay hello.\n")
    }

    #[test]
    fn annotate_reports_install_state() {
        let tmp = tempfile::tempdir().unwrap();
        let skills_dir = tmp.path().join("skills");
        std::fs::create_dir_all(skills_dir.join("ext")).unwrap();
        std::fs::write(
            skills_dir.join("ext").join("SKILL.md"),
            skill_md("ext", "1.0.0"),
        )
        .unwrap();
        let store = WasmSkillStore::open(&tmp.path().join("wasm")).unwrap();

        let views = annotate("example", &manifest(), &skills_dir, Some(&store));
        let view = |name: &str| views.iter().find(|v| v.name == name).unwrap();
        assert!(
            view("ext").installed,
            "external skill with a matching dir is installed"
        );
        assert_eq!(view("ext").installed_version.as_deref(), Some("1.0.0"));
        assert!(
            !view("ext-missing").installed,
            "external skill with no dir is not installed"
        );
        assert!(
            !view("w").installed,
            "wasm skill absent from the store is not installed"
        );
    }

    #[test]
    fn annotate_without_store_reports_wasm_uninstalled() {
        let tmp = tempfile::tempdir().unwrap();
        let views = annotate("example", &manifest(), &tmp.path().join("skills"), None);
        assert!(!views.iter().find(|v| v.name == "w").unwrap().installed);
    }

    #[test]
    fn annotate_flags_source_built_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let views = annotate("example", &manifest(), &tmp.path().join("skills"), None);
        let built = |name: &str| {
            views
                .iter()
                .find(|v| v.name == name)
                .unwrap()
                .built_from_source
        };
        assert!(built("src"));
        assert!(!built("w"));
        assert!(!built("ext"));
    }

    #[test]
    fn source_build_ref_names_ref_and_subpath() {
        assert_eq!(
            source_build_ref("o/r", "a7a73719", "workforce/wasm/src"),
            "o/r@a7a73719#subpath=workforce/wasm/src"
        );
    }

    #[test]
    fn wasm_provenance_records_the_built_commit() {
        assert_eq!(
            wasm_provenance("example", "o/r", "workforce/wasm/w", None),
            "catalog:example:o/r#workforce/wasm/w"
        );
        assert_eq!(
            wasm_provenance("example", "o/r", "workforce/wasm/src", Some("abc123")),
            "catalog:example:o/r@abc123#workforce/wasm/src"
        );
    }

    #[test]
    fn resolve_token_none_without_vault() {
        assert_eq!(resolve_token(None, "o"), None);
    }

    #[test]
    fn catalog_selection_rules() {
        assert!(select_catalog(&[], None)
            .unwrap_err()
            .contains("No [[skills.catalogs]]"));
        let one = [catalog_source("a")];
        assert_eq!(select_catalog(&one, None).unwrap().name, "a");
        let two = [catalog_source("a"), catalog_source("b")];
        assert!(select_catalog(&two, None).unwrap_err().contains("a, b"));
        assert_eq!(select_catalog(&two, Some("b")).unwrap().name, "b");
        assert!(select_catalog(&two, Some("c"))
            .unwrap_err()
            .contains("Unknown catalog 'c'"));
    }

    #[test]
    fn permit_source_build_applies_policy_only_to_agents() {
        let closed = WasmInstallPolicy {
            allow_build_on_install: false,
            require_sha256_pin: true,
        };
        let pinned = WasmInstallPolicy {
            allow_build_on_install: true,
            require_sha256_pin: true,
        };
        let admin = SkillCaller::local(Some("admin".into()), true);
        let agent = |caller: &SkillCaller, policy| InstallAuthority::Agent {
            caller: caller.clone(),
            policy,
        };

        assert!(InstallAuthority::Operator
            .permit_source_build("hello-world", "main")
            .is_ok());
        let refused = agent(&admin, closed)
            .permit_source_build("hello-world", COMMIT)
            .unwrap_err();
        assert!(refused.contains("allow_build_on_install"), "got: {refused}");
        let unpinned = agent(&admin, pinned)
            .permit_source_build("hello-world", "main")
            .unwrap_err();
        assert!(unpinned.contains("require_sha256_pin"), "got: {unpinned}");
        assert!(agent(&admin, pinned)
            .permit_source_build("hello-world", COMMIT)
            .is_ok());

        let member = SkillCaller::local(Some("member".into()), false);
        assert_eq!(
            agent(&member, pinned).permit_source_build("hello-world", COMMIT),
            Err(crate::wasm_skill_access::HOST_COMPILE_REFUSED.to_string())
        );
    }

    fn discovered(path: &Path, gating_passed: bool) -> ExternalSkill {
        ExternalSkill {
            discovered: workforce_skills::scanner::DiscoveredSkill {
                skill: workforce_skills::skill_file::SkillFile::parse(&skill_md(
                    "hello-world",
                    "1.0.0",
                ))
                .unwrap(),
                path: path.to_path_buf(),
                gating: workforce_skills::gating::GatingResult {
                    passed: gating_passed,
                    checks: Vec::new(),
                },
                config_status: workforce_skills::config_loader::ConfigStatus::Unconfigured,
                security_findings: Vec::new(),
            },
            access: crate::external_skills::AccessRecord::org_wide(),
        }
    }

    #[test]
    fn external_activation_explains_each_state() {
        let path = Path::new("/srv/skills/hello-world");
        assert_eq!(external_activation(None, path), (false, STAGED.into()));
        assert_eq!(
            external_activation(
                Some(&[discovered(Path::new("/srv/skills/other"), true)]),
                path
            ),
            (false, NOT_LISTED.into())
        );
        assert_eq!(
            external_activation(Some(&[discovered(path, true)]), path),
            (true, LIVE.into())
        );
        let (active, note) = external_activation(Some(&[discovered(path, false)]), path);
        assert!(!active);
        assert!(
            note.starts_with("installed, but agents cannot use it until this is resolved:"),
            "got: {note}"
        );
    }

    pub(crate) fn git(dir: &Path, args: &[&str]) -> String {
        let out = Command::new("git")
            .arg("-C")
            .arg(dir)
            .args([
                "-c",
                "user.name=test",
                "-c",
                "user.email=test@example.com",
                "-c",
                "commit.gpgsign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "git {args:?}: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Write `files` (repo-relative path, content) into `repo` and commit them on `main`.
    fn commit_files(repo: &Path, files: &[(&str, &str)]) {
        for (path, content) in files {
            let file = repo.join(path);
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(file, content).unwrap();
        }
        git(repo, &["add", "-A"]);
        git(repo, &["commit", "-q", "-m", "update"]);
    }

    /// A local catalog repo. Its path ends in `.git`, so the installer clones it like a remote.
    pub(crate) fn catalog_repo(parent: &Path, files: &[(&str, &str)]) -> PathBuf {
        let repo = parent.join("acme-skills.git");
        std::fs::create_dir_all(&repo).unwrap();
        git(&repo, &["init", "-q", "-b", "main"]);
        commit_files(&repo, files);
        repo
    }

    fn external_entry() -> CatalogEntry {
        CatalogEntry {
            name: "hello-world".into(),
            kind: SkillKind::External,
            subpath: EXTERNAL_SUBPATH.into(),
            version: None,
            description: String::new(),
            requires_bins: vec![],
            wasm_path: None,
            sha256: None,
        }
    }

    fn resolved<'a>(
        source: &'a CatalogSource,
        repo: &Path,
        entry: CatalogEntry,
    ) -> ResolvedEntry<'a> {
        ResolvedEntry {
            source,
            owner_repo: "acme/skills".into(),
            repo_url: repo.display().to_string(),
            token: None,
            entry,
        }
    }

    fn catalog_over(stores: SkillStores) -> SkillCatalog {
        SkillCatalog {
            catalogs: vec![catalog_source("example")],
            vault: None,
            runner: None,
            stores,
        }
    }

    fn live_catalog(skills_dir: &Path) -> (Arc<ExternalSkills>, SkillCatalog) {
        let external = Arc::new(ExternalSkills::new(
            skills_dir.to_path_buf(),
            vec![skills_dir.to_path_buf()],
        ));
        let catalog = catalog_over(SkillStores::Live(Arc::clone(&external)));
        (external, catalog)
    }

    fn skill_file() -> String {
        format!("{EXTERNAL_SUBPATH}/SKILL.md")
    }

    #[tokio::test]
    async fn external_install_goes_live_and_reinstall_updates_in_place() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = catalog_repo(
            tmp.path(),
            &[(&skill_file(), &skill_md("hello-world", "1.0.0"))],
        );
        let skills_dir = tmp.path().join("skills");
        let (external, catalog) = live_catalog(&skills_dir);
        let source = catalog_source("example");
        let installed = skills_dir.join("hello-world");

        let first = catalog
            .install_external(resolved(&source, &repo, external_entry()))
            .await
            .unwrap();
        assert!(first.active, "{}", first.activation);
        assert_eq!(first.activation, LIVE);
        assert_eq!(first.version.as_deref(), Some("1.0.0"));
        assert!(!first.replaced);
        assert!(external
            .snapshot()
            .iter()
            .any(|s| s.discovered.path == installed));

        std::fs::write(installed.join("config.toml"), "greeting = \"hi\"\n").unwrap();
        commit_files(&repo, &[(&skill_file(), &skill_md("hello-world", "2.0.0"))]);
        let second = catalog
            .install_external(resolved(&source, &repo, external_entry()))
            .await
            .unwrap();
        assert!(second.active, "{}", second.activation);
        assert!(second.replaced);
        assert_eq!(second.previous_version.as_deref(), Some("1.0.0"));
        assert_eq!(second.version.as_deref(), Some("2.0.0"));
        assert_eq!(
            std::fs::read_to_string(installed.join("config.toml")).unwrap(),
            "greeting = \"hi\"\n"
        );
        let live = external.snapshot();
        let skill = live
            .iter()
            .find(|s| s.discovered.path == installed)
            .unwrap();
        assert_eq!(skill.discovered.skill.meta.version, "2.0.0");
        assert_eq!(std::fs::read_dir(&skills_dir).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn a_catalog_install_updates_a_skill_installed_from_its_repository() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = catalog_repo(
            tmp.path(),
            &[(&skill_file(), &skill_md("hello-world", "1.0.0"))],
        );
        let skills_dir = tmp.path().join("skills");
        let (external, catalog) = live_catalog(&skills_dir);
        let source = catalog_source("example");
        let from_repo = format!("{}#subpath={EXTERNAL_SUBPATH}", repo.display());

        let located = tokio::task::spawn_blocking(move || {
            installer::locate_skill(&from_repo, &InstallOptions::default())
        })
        .await
        .unwrap()
        .unwrap();
        let first = external
            .install(SkillCaller::local(None, true), located, None, None)
            .await
            .unwrap();
        assert!(!first.replaced);
        std::fs::write(first.path.join("config.toml"), "greeting = \"hi\"\n").unwrap();
        commit_files(&repo, &[(&skill_file(), &skill_md("hello-world", "2.0.0"))]);

        let updated = catalog
            .install_external(resolved(&source, &repo, external_entry()))
            .await
            .unwrap();
        assert!(updated.replaced);
        assert_eq!(updated.previous_version.as_deref(), Some("1.0.0"));
        assert_eq!(updated.version.as_deref(), Some("2.0.0"));
        assert_eq!(
            std::fs::read_to_string(first.path.join("config.toml")).unwrap(),
            "greeting = \"hi\"\n"
        );
        assert_eq!(std::fs::read_dir(&skills_dir).unwrap().count(), 1);
    }

    #[tokio::test]
    async fn external_install_refuses_a_mismatched_manifest_name_before_writing() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = catalog_repo(
            tmp.path(),
            &[(&skill_file(), &skill_md("someone-else", "2.0.0"))],
        );
        let skills_dir = tmp.path().join("skills");
        let installed = skills_dir.join("hello-world");
        std::fs::create_dir_all(&installed).unwrap();
        std::fs::write(installed.join("SKILL.md"), skill_md("hello-world", "1.0.0")).unwrap();
        let (external, catalog) = live_catalog(&skills_dir);
        let source = catalog_source("example");

        let err = catalog
            .install_external(resolved(&source, &repo, external_entry()))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("does not match"), "got: {err}");
        assert_eq!(
            installer::installed_version(&installed).as_deref(),
            Some("1.0.0")
        );
        assert_eq!(std::fs::read_dir(&skills_dir).unwrap().count(), 1);
        assert!(external
            .snapshot()
            .iter()
            .any(|s| s.discovered.path == installed));
    }

    #[tokio::test]
    async fn external_install_refuses_an_entry_without_skill_md() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_toml = format!("{EXTERNAL_SUBPATH}/Skill.toml");
        let repo = catalog_repo(
            tmp.path(),
            &[(
                &skill_toml,
                "[skill]\nname = \"hello-world\"\nversion = \"1.0.0\"\ndescription = \"d\"\n",
            )],
        );
        let skills_dir = tmp.path().join("skills");
        let (_external, catalog) = live_catalog(&skills_dir);
        let source = catalog_source("example");

        let err = catalog
            .install_external(resolved(&source, &repo, external_entry()))
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("has no SKILL.md"), "got: {err}");
        assert!(!skills_dir.exists());
    }

    #[tokio::test]
    async fn offline_external_install_is_written_for_the_next_start() {
        let tmp = tempfile::tempdir().unwrap();
        let repo = catalog_repo(
            tmp.path(),
            &[(&skill_file(), &skill_md("hello-world", "1.0.0"))],
        );
        let skills_dir = tmp.path().join("skills");
        let catalog = catalog_over(SkillStores::Offline {
            skills_dir: skills_dir.clone(),
            wasm_store_dir: tmp.path().join("wasm-skills"),
        });
        let source = catalog_source("example");

        let outcome = catalog
            .install_external(resolved(&source, &repo, external_entry()))
            .await
            .unwrap();
        assert!(!outcome.active);
        assert_eq!(outcome.activation, STAGED);
        assert!(skills_dir.join("hello-world").join("SKILL.md").is_file());
    }

    fn echo_fixture() -> (SkillManifest, Vec<u8>) {
        let bytes = std::fs::read(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../wasm-runtime/tests/fixtures/echo_skill.wasm"
        ))
        .unwrap();
        let toml = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../wasm-runtime/tests/fixtures/echo-skill/Skill.toml"
        ))
        .unwrap()
        .replace("name = \"echo-skill\"", "name = \"catalog-echo\"");
        (SkillManifest::from_toml(&toml).unwrap(), bytes)
    }

    fn wasm_entry(sha256: &str) -> CatalogEntry {
        CatalogEntry {
            name: "catalog-echo".into(),
            kind: SkillKind::Wasm,
            subpath: "workforce/wasm/catalog-echo".into(),
            version: None,
            description: String::new(),
            requires_bins: vec![],
            wasm_path: Some("catalog-echo.wasm".into()),
            sha256: Some(sha256.into()),
        }
    }

    fn fetched(manifest: &SkillManifest, wasm_bytes: &[u8]) -> FetchedSkill {
        FetchedSkill {
            manifest: manifest.clone(),
            wasm_bytes: wasm_bytes.to_vec(),
            commit: None,
        }
    }

    fn wasm_target(dir: &Path, engine: Option<Arc<WasmSkillEngine>>) -> WasmTarget {
        WasmTarget {
            store: Arc::new(WasmSkillStore::open(dir).unwrap()),
            engine,
            hosts: SkillHosts::default(),
        }
    }

    #[tokio::test]
    async fn wasm_stage_activates_on_every_agent_and_keeps_config_on_reinstall() {
        let tmp = tempfile::tempdir().unwrap();
        let (manifest, bytes) = echo_fixture();
        let registry = Arc::new(ToolRegistry::new());
        let skill_state = Arc::new(SkillStateTracker::new());
        let target = wasm_target(tmp.path(), crate::wasm_skill_tools::global_skill_engine());
        target.hosts.register(
            "catalog-test",
            &registry,
            &skill_state,
            None,
            Arc::default(),
        );
        let source = catalog_source("example");
        let pin = workforce_wasm_runtime::store::sha256_hex(&bytes);
        let entry = || resolved(&source, tmp.path(), wasm_entry(&pin));

        let first = stage_wasm(&target, None, &entry(), fetched(&manifest, &bytes))
            .await
            .unwrap();
        assert!(first.active);
        assert_eq!(first.activation, LIVE);
        assert!(!first.replaced);
        assert!(registry.has("catalog-echo.echo"));
        assert!(skill_state.is_loaded("catalog-echo"));

        let mut record = target.store.get_installed("catalog-echo").unwrap();
        record.config.insert("greeting".into(), "hi".into());
        target
            .store
            .update_metadata("catalog-echo", &record)
            .unwrap();

        let previous = target.store.get_installed("catalog-echo");
        let second = stage_wasm(&target, previous, &entry(), fetched(&manifest, &bytes))
            .await
            .unwrap();
        assert!(second.active);
        assert!(second.replaced);
        assert_eq!(second.previous_version.as_deref(), Some("0.1.0"));
        let reinstalled = target.store.get_installed("catalog-echo").unwrap();
        assert_eq!(
            reinstalled.config.get("greeting").map(String::as_str),
            Some("hi")
        );
        assert_eq!(reinstalled.binary_version, 2);
        assert!(registry.has("catalog-echo.echo"));
    }

    #[tokio::test]
    async fn wasm_stage_rejects_a_mismatched_name_and_a_bad_pin_without_touching_the_store() {
        let tmp = tempfile::tempdir().unwrap();
        let (manifest, bytes) = echo_fixture();
        let target = wasm_target(tmp.path(), None);
        let source = catalog_source("example");
        let pin = workforce_wasm_runtime::store::sha256_hex(&bytes);

        let renamed = SkillManifest {
            name: "someone-else".into(),
            ..manifest.clone()
        };
        let mismatch = stage_wasm(
            &target,
            None,
            &resolved(&source, tmp.path(), wasm_entry(&pin)),
            fetched(&renamed, &bytes),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(mismatch.contains("does not match"), "got: {mismatch}");

        let bad_pin = stage_wasm(
            &target,
            None,
            &resolved(&source, tmp.path(), wasm_entry(&"0".repeat(64))),
            fetched(&manifest, &bytes),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(bad_pin.contains("sha256 mismatch"), "got: {bad_pin}");
        assert!(target.store.list_installed().is_empty());
    }

    #[tokio::test]
    async fn offline_wasm_stage_writes_the_store_without_activating() {
        let tmp = tempfile::tempdir().unwrap();
        let (manifest, bytes) = echo_fixture();
        let target = wasm_target(tmp.path(), None);
        let source = catalog_source("example");
        let pin = workforce_wasm_runtime::store::sha256_hex(&bytes);

        let outcome = stage_wasm(
            &target,
            None,
            &resolved(&source, tmp.path(), wasm_entry(&pin)),
            fetched(&manifest, &bytes),
        )
        .await
        .unwrap();
        assert!(!outcome.active);
        assert_eq!(outcome.activation, STAGED);
        assert_eq!(outcome.version.as_deref(), Some("0.1.0"));
        assert!(target.store.get_installed("catalog-echo").is_some());
    }
}
