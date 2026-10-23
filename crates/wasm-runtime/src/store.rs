//! Filesystem-backed store for WASM skill binaries and manifests.
//!
//! Layout on disk:
//! ```text
//! {base_dir}/
//!   {skill_name}/
//!     manifest.toml       — cached Skill.toml
//!     v{version}.wasm     — compiled WASM binary
//!     installed.json      — InstalledWasmSkill metadata
//!     source/             — snapshot of the project the binary was built from
//!     source.json         — which binary that snapshot was taken for
//! ```
//!
//! A snapshot is published by rename, via `source.tmp` (assembling) and
//! `source.old` (the copy being replaced), so no crash window leaves a skill
//! without recoverable source.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::Utc;
use eyre::{bail, Result, WrapErr};
use parking_lot::RwLock;
use sha2::{Digest, Sha256};
use tracing::info;
use workforce_agent::tools::config_guard;
use workforce_agent::tools::paths::CanonicalRoots;
use workforce_core::access::AccessWrite;

use crate::manifest::SkillManifest;
use crate::types::{InstalledWasmSkill, SkillOwnership, WasmSkillState};

/// Hex-encoded SHA-256 of a blob — the digest used to pin installed `.wasm`
/// bytes against a supply-chain substitution.
pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

/// Ceiling on a stored source snapshot. A skill project is source text; a
/// larger tree means vendored artefacts that do not belong in the store.
const SOURCE_SNAPSHOT_MAX_BYTES: u64 = 2 * 1024 * 1024;

/// Files a snapshot copied, and their total size.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SourceSnapshot {
    pub files: usize,
    pub bytes: u64,
}

/// Which installed binary a stored snapshot was taken for. Without this a
/// restored project looks authoritative even when a later binary-only install
/// replaced what is running.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SourceSnapshotMeta {
    pub binary_version: u32,
    pub sha256: String,
    pub files: usize,
    pub bytes: u64,
}

/// Project entries worth preserving; everything else (`target/`, other hidden
/// entries, symlinks) is build output or an escape hatch, not source.
/// `.cargo/config.toml` pins the wasm target, so a project restored without it
/// silently builds a host binary.
const SOURCE_ROOT_FILES: [&str; 6] = [
    "Skill.toml",
    "Cargo.toml",
    "Cargo.lock",
    ".cargo/config.toml",
    ".gitignore",
    ".github/workflows/release.yml",
];
const SOURCE_ROOT_DIRS: [&str; 1] = ["src"];

/// Collect regular files under `root/rel` as `(path relative to root, size)`.
/// Symlinks are never followed. `Scan::Project` additionally skips hidden
/// entries and `target/` — build output and escape hatches are not source —
/// while `Scan::Stored` takes a snapshot the store itself wrote verbatim,
/// including the dotfiles it deliberately preserved.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Scan {
    Project,
    Stored,
}

fn collect_under(root: &Path, rel: &Path, scan: Scan, out: &mut Vec<(PathBuf, u64)>) -> Result<()> {
    for entry in std::fs::read_dir(root.join(rel))? {
        let entry = entry?;
        let meta = entry.metadata()?;
        let name = entry.file_name();
        let label = name.to_string_lossy();
        let excluded = scan == Scan::Project && (label.starts_with('.') || label == "target");
        if meta.is_symlink() || excluded {
            continue;
        }
        let child = rel.join(&name);
        if meta.is_dir() {
            collect_under(root, &child, scan, out)?;
        } else if meta.is_file() {
            out.push((child, meta.len()));
        }
    }
    Ok(())
}

/// The project files a snapshot preserves — and, by the same rule, the files
/// a catalog checkout of the skill is governed by.
pub fn project_source_files(project_dir: &Path) -> Result<Vec<(PathBuf, u64)>> {
    if !project_dir.is_dir() {
        bail!("project directory {} does not exist", project_dir.display());
    }
    let roots = SOURCE_ROOT_FILES
        .iter()
        .filter_map(|file| {
            let meta = std::fs::symlink_metadata(project_dir.join(file)).ok()?;
            meta.is_file().then(|| (PathBuf::from(file), meta.len()))
        })
        .collect();
    SOURCE_ROOT_DIRS
        .iter()
        .filter(|dir| {
            // `is_dir` follows symlinks: a `src` symlinked at any directory
            // would copy that directory into the store, and out again into
            // the workspace via `restore_source`.
            std::fs::symlink_metadata(project_dir.join(dir))
                .map(|meta| meta.is_dir())
                .unwrap_or(false)
        })
        .try_fold(roots, |mut acc, dir| {
            collect_under(project_dir, Path::new(dir), Scan::Project, &mut acc)?;
            Ok(acc)
        })
}

/// Every regular file under `root` a project scan keeps — no hidden entries at
/// any depth, no `target/`, no symlinks — relative to `root`.
pub fn visible_files(root: &Path) -> Result<Vec<(PathBuf, u64)>> {
    let mut files = Vec::new();
    collect_under(root, Path::new(""), Scan::Project, &mut files)?;
    Ok(files)
}

/// Every file held in a stored snapshot.
fn collect_tree(root: &Path) -> Result<Vec<(PathBuf, u64)>> {
    let mut out = Vec::new();
    collect_under(root, Path::new(""), Scan::Stored, &mut out)?;
    Ok(out)
}

/// Manages on-disk storage for WASM skill binaries and metadata.
pub struct WasmSkillStore {
    base_dir: PathBuf,
    /// In-memory index of installed skills.
    installed: RwLock<HashMap<String, InstalledWasmSkill>>,
    /// Serializes the read-version → remove → write install sequence so
    /// concurrent installs of the same skill can't interleave versions.
    install_mutex: parking_lot::Mutex<()>,
}

impl WasmSkillStore {
    /// Open or create a WASM skill store at the given directory.
    pub fn open(base_dir: &Path) -> Result<Self> {
        std::fs::create_dir_all(base_dir).wrap_err_with(|| {
            format!(
                "Failed to create WASM skill store at {}",
                base_dir.display()
            )
        })?;

        let store = Self {
            base_dir: base_dir.to_path_buf(),
            installed: RwLock::new(HashMap::new()),
            install_mutex: parking_lot::Mutex::new(()),
        };

        // Load existing installations from disk
        store.load_index()?;

        Ok(store)
    }

    /// Base directory for the store.
    pub fn base_dir(&self) -> &Path {
        &self.base_dir
    }

    /// Directory for a specific skill.
    fn skill_dir(&self, name: &str) -> PathBuf {
        self.base_dir.join(name)
    }

    /// Path to the WASM binary for a skill version.
    fn binary_path(&self, name: &str, version: u32) -> PathBuf {
        self.skill_dir(name).join(format!("v{}.wasm", version))
    }

    /// Path to the cached manifest.
    fn manifest_path(&self, name: &str) -> PathBuf {
        self.skill_dir(name).join("manifest.toml")
    }

    /// Path to the installed metadata JSON.
    fn metadata_path(&self, name: &str) -> PathBuf {
        self.skill_dir(name).join("installed.json")
    }

    /// Directory holding the source snapshot for a skill.
    fn source_dir(&self, name: &str) -> PathBuf {
        self.skill_dir(name).join("source")
    }

    /// Staging directory a snapshot is assembled in before it replaces the
    /// live one, so a crash mid-copy never leaves a torn snapshot as the only
    /// copy. Never a valid skill dir: `load_index` skips it (no metadata).
    fn source_staging_dir(&self, name: &str) -> PathBuf {
        self.skill_dir(name).join("source.tmp")
    }

    /// The previous snapshot, held until the replacement is in place.
    fn source_retired_dir(&self, name: &str) -> PathBuf {
        self.skill_dir(name).join("source.old")
    }

    /// Settle whatever a previous publish left behind: a retired snapshot with
    /// no live one is the only complete copy, so it is adopted; anything else
    /// is scratch.
    ///
    /// Only ever called with `install_mutex` held. Another instance of this
    /// store may be open on the same directory — the CLI's catalog commands
    /// open their own — and clearing scratch out from under an in-flight
    /// publish would fail it, so this never runs on a read path.
    fn reconcile_source_dirs(&self, name: &str) -> Result<()> {
        let (live, retired, staging) = (
            self.source_dir(name),
            self.source_retired_dir(name),
            self.source_staging_dir(name),
        );
        if retired.is_dir() {
            match live.is_dir() {
                true => std::fs::remove_dir_all(&retired)?,
                false => std::fs::rename(&retired, &live)?,
            }
        }
        if staging.exists() {
            std::fs::remove_dir_all(&staging)?;
        }
        Ok(())
    }

    /// Path to the snapshot's provenance record.
    fn source_meta_path(&self, name: &str) -> PathBuf {
        self.skill_dir(name).join("source.json")
    }

    /// The directory holding this skill's snapshot: the live one, or the
    /// retired copy when a publish was interrupted before it could be
    /// replaced. Readers must see the retired copy — it is a complete
    /// snapshot, and reporting "no source" for it is how a recoverable skill
    /// looks lost.
    fn stored_source_dir(&self, name: &str) -> Option<PathBuf> {
        [self.source_dir(name), self.source_retired_dir(name)]
            .into_iter()
            .find(|dir| dir.is_dir())
    }

    /// Whether a source snapshot is stored for this skill.
    pub fn has_source_snapshot(&self, name: &str) -> bool {
        self.stored_source_dir(name).is_some()
    }

    /// The stored snapshot's provenance, when one was recorded.
    pub fn source_snapshot_meta(&self, name: &str) -> Option<SourceSnapshotMeta> {
        self.has_source_snapshot(name)
            .then(|| std::fs::read_to_string(self.source_meta_path(name)).ok())
            .flatten()
            .and_then(|json| serde_json::from_str(&json).ok())
    }

    /// Whether the stored snapshot is the source of the installed binary.
    /// A `false` here is the difference between recovering the skill that is
    /// running and recovering an older one.
    pub fn source_snapshot_matches_binary(&self, name: &str) -> bool {
        match (self.source_snapshot_meta(name), self.get_installed(name)) {
            (Some(meta), Some(installed)) => {
                meta.binary_version == installed.binary_version && meta.sha256 == installed.sha256
            }
            _ => false,
        }
    }

    /// Copy a skill project's source into the store beside its binary, so the
    /// source is recoverable for as long as the skill is installed. Replaces
    /// any previous snapshot atomically.
    pub fn snapshot_source(
        &self,
        name: &str,
        project_dir: &Path,
        installed: &InstalledWasmSkill,
    ) -> Result<SourceSnapshot> {
        let files = project_source_files(project_dir)?;
        if files.is_empty() {
            bail!(
                "no source files found under {} — nothing to snapshot",
                project_dir.display()
            );
        }
        let bytes = files.iter().map(|(_, size)| size).sum::<u64>();
        if bytes > SOURCE_SNAPSHOT_MAX_BYTES {
            bail!(
                "skill source is {bytes} bytes, over the {SOURCE_SNAPSHOT_MAX_BYTES}-byte \
                 snapshot budget; remove vendored artefacts from the project"
            );
        }

        let _install_guard = self.install_mutex.lock();
        self.reconcile_source_dirs(name)?;
        let staging = self.source_staging_dir(name);
        for (relative, _) in &files {
            let target = staging.join(relative);
            if let Some(parent) = target.parent() {
                std::fs::create_dir_all(parent)?;
            }
            std::fs::copy(project_dir.join(relative), &target).wrap_err_with(|| {
                format!(
                    "Failed to copy {} into the source snapshot",
                    relative.display()
                )
            })?;
        }

        // Retire rather than delete: between the two renames both copies
        // exist, so no window leaves the skill without recoverable source.
        let final_dir = self.source_dir(name);
        let retired = self.source_retired_dir(name);
        if final_dir.is_dir() {
            std::fs::rename(&final_dir, &retired)?;
        }
        std::fs::rename(&staging, &final_dir).wrap_err_with(|| {
            format!(
                "Failed to publish source snapshot to {}",
                final_dir.display()
            )
        })?;
        if retired.is_dir() {
            std::fs::remove_dir_all(&retired)?;
        }

        let snapshot = SourceSnapshot {
            files: files.len(),
            bytes,
        };
        std::fs::write(
            self.source_meta_path(name),
            serde_json::to_string_pretty(&SourceSnapshotMeta {
                binary_version: installed.binary_version,
                sha256: installed.sha256.clone(),
                files: snapshot.files,
                bytes: snapshot.bytes,
            })?,
        )?;
        info!(
            skill = %name,
            files = snapshot.files,
            bytes = snapshot.bytes,
            "WASM skill source snapshot stored"
        );
        Ok(snapshot)
    }

    /// Copy a stored source snapshot back out to `dest`, which must not be an
    /// existing non-empty directory — restoring over live edits would destroy
    /// exactly the work the caller is trying to recover. Every file is
    /// written through [`config_guard`] within `roots`: `dest` is in the
    /// agent's tree, which the agent uid can add links to as it fills.
    pub fn restore_source(
        &self,
        name: &str,
        dest: &Path,
        roots: &CanonicalRoots,
    ) -> Result<SourceSnapshot> {
        let Some(source) = self.stored_source_dir(name) else {
            bail!("no source snapshot is stored for skill '{name}'");
        };
        if dest.is_dir() && std::fs::read_dir(dest)?.next().is_some() {
            bail!(
                "destination {} already exists and is not empty",
                dest.display()
            );
        }
        let files = collect_tree(&source)?;
        let bytes = files.iter().map(|(_, size)| size).sum::<u64>();
        for (relative, _) in &files {
            let target = dest.join(relative);
            let real = roots
                .resolve(&target)
                .ok_or_else(|| eyre::eyre!("{} leads outside the workspace", target.display()))?;
            if let Some(parent) = real.parent() {
                config_guard::create_dirs(roots, parent)?;
            }
            let mut file = std::fs::File::open(source.join(relative))?;
            config_guard::write_from(roots, &real, &mut file).wrap_err_with(|| {
                format!("Failed to restore {} from the snapshot", relative.display())
            })?;
        }
        info!(skill = %name, files = files.len(), dest = %dest.display(), "WASM skill source restored");
        Ok(SourceSnapshot {
            files: files.len(),
            bytes,
        })
    }

    /// Run `f` with any stored source snapshot moved out of the skill
    /// directory, then put it back. Re-versioning wipes that directory, and a
    /// snapshot from the previous build still recovers more than nothing.
    fn with_carried_source<R>(&self, name: &str, f: impl FnOnce() -> Result<R>) -> Result<R> {
        let source = self.stored_source_dir(name);
        let carried = source
            .as_ref()
            .map(|_| self.base_dir.join(format!(".carry-{name}")));
        if let (Some(from), Some(aside)) = (source.as_ref(), carried.as_ref()) {
            if aside.exists() {
                std::fs::remove_dir_all(aside)?;
            }
            std::fs::rename(from, aside)?;
        }

        let outcome = f();

        if let Some(aside) = carried {
            // A failure after the skill directory was removed leaves the skill
            // uninstalled; putting the snapshot back would resurrect a
            // directory `load_index` ignores, so `list` and `restore_source`
            // would disagree about whether the skill exists.
            let restore = outcome.is_ok() || self.get_installed(name).is_some();
            let live = self.source_dir(name);
            let result = match restore {
                true => live
                    .parent()
                    .map(std::fs::create_dir_all)
                    .transpose()
                    .and_then(|_| std::fs::rename(&aside, &live)),
                false => std::fs::remove_dir_all(&aside),
            };
            if let Err(e) = result {
                tracing::warn!(
                    skill = %name,
                    error = %e,
                    carried = %aside.display(),
                    restored = restore,
                    "Failed to settle the carried source snapshot after re-versioning"
                );
            }
        }
        outcome
    }

    /// Store a WASM binary and its manifest. Returns the binary version number.
    pub fn store_binary(
        &self,
        name: &str,
        wasm_bytes: &[u8],
        manifest: &SkillManifest,
        installed: &InstalledWasmSkill,
    ) -> Result<u32> {
        let skill_dir = self.skill_dir(name);
        std::fs::create_dir_all(&skill_dir).wrap_err_with(|| {
            format!("Failed to create skill directory {}", skill_dir.display())
        })?;

        let version = installed.binary_version;

        // Write WASM binary
        let bin_path = self.binary_path(name, version);
        std::fs::write(&bin_path, wasm_bytes)
            .wrap_err_with(|| format!("Failed to write WASM binary to {}", bin_path.display()))?;

        // Write manifest in canonical Skill.toml format
        let manifest_toml = manifest.to_toml()?;
        let manifest_file = self.manifest_path(name);
        std::fs::write(&manifest_file, manifest_toml)
            .wrap_err_with(|| format!("Failed to write manifest to {}", manifest_file.display()))?;

        // Write metadata
        let metadata_json = serde_json::to_string_pretty(installed)
            .wrap_err("Failed to serialise installation metadata")?;
        std::fs::write(self.metadata_path(name), metadata_json)?;

        // Update in-memory index
        self.installed
            .write()
            .insert(name.to_string(), installed.clone());

        info!(
            skill = %name,
            version = version,
            size_bytes = wasm_bytes.len(),
            "WASM skill binary stored"
        );

        Ok(version)
    }

    /// Install a prebuilt `.wasm` into the store: validate the manifest, verify
    /// the optional SHA-256 pin, allocate the next binary version (removing any
    /// prior on-disk copy), and persist the binary + manifest + record.
    ///
    /// Disk-only — it neither compiles nor registers the skill. Shared by the
    /// agent `wasm_skill.install` tool (which then compiles + registers for live
    /// use) and the catalog installer (which activates it on every agent, or
    /// stages it when run outside the hub).
    /// `ownership` applies only to a first install: re-installing a skill that
    /// already has an owner keeps that owner, its visibility, and its grantees,
    /// so an update can neither reassign a skill nor silently unshare it.
    pub fn install_prebuilt(
        &self,
        manifest: SkillManifest,
        wasm_bytes: &[u8],
        expected_sha256: Option<&str>,
        source: String,
        config: HashMap<String, String>,
        ownership: SkillOwnership,
    ) -> Result<InstalledWasmSkill> {
        manifest
            .validate()
            .map_err(|e| eyre::eyre!("invalid skill manifest: {e}"))?;

        let _install_guard = self.install_mutex.lock();
        let computed = sha256_hex(wasm_bytes);
        if let Some(expected) = expected_sha256 {
            let normalized = expected.trim().to_ascii_lowercase();
            let normalized = normalized.strip_prefix("sha256:").unwrap_or(&normalized);
            if normalized != computed {
                tracing::warn!(
                    target: "sentinel.audit",
                    skill = %manifest.name,
                    expected = %normalized,
                    actual = %computed,
                    "wasm skill install rejected: sha256 mismatch"
                );
                bail!(
                    "sha256 mismatch for skill '{}' — expected {}, got {}. \
                     The .wasm bytes don't match the supplied pin; refusing to install.",
                    manifest.name,
                    normalized,
                    computed
                );
            }
        }

        self.with_carried_source(&manifest.name.clone(), move || {
            let previous = self.get_installed(&manifest.name);
            let binary_version = match &previous {
                Some(existing) => {
                    let next = existing.binary_version + 1;
                    self.remove(&manifest.name)?;
                    next
                }
                None => 1,
            };
            let (owner, visibility, shares) = match previous.filter(|p| p.owner.is_some()) {
                Some(kept) => (kept.owner, kept.visibility, kept.shares),
                None => (ownership.owner, ownership.visibility, Vec::new()),
            };

            let installed = InstalledWasmSkill {
                manifest: manifest.clone(),
                state: WasmSkillState::Active,
                installed_at: Utc::now(),
                source,
                config,
                binary_version,
                sha256: computed,
                owner,
                visibility,
                shares,
            };
            self.store_binary(&manifest.name, wasm_bytes, &manifest, &installed)?;
            Ok(installed)
        })
    }

    /// Load a WASM binary from disk.
    pub fn load_binary(&self, name: &str, version: u32) -> Result<Vec<u8>> {
        let path = self.binary_path(name, version);
        std::fs::read(&path).wrap_err_with(|| {
            format!(
                "Failed to read WASM binary for skill '{}' v{} at {}",
                name,
                version,
                path.display()
            )
        })
    }

    /// Load the cached manifest from disk.
    pub fn load_manifest(&self, name: &str) -> Result<SkillManifest> {
        SkillManifest::from_file(&self.manifest_path(name))
    }

    /// Get the installed metadata for a skill.
    pub fn get_installed(&self, name: &str) -> Option<InstalledWasmSkill> {
        self.installed.read().get(name).cloned()
    }

    /// List all installed skills.
    pub fn list_installed(&self) -> Vec<InstalledWasmSkill> {
        self.installed.read().values().cloned().collect()
    }

    /// Remove a skill from the store (deletes files and metadata).
    pub fn remove(&self, name: &str) -> Result<bool> {
        let existed = self.installed.write().remove(name).is_some();
        let skill_dir = self.skill_dir(name);
        if skill_dir.is_dir() {
            std::fs::remove_dir_all(&skill_dir).wrap_err_with(|| {
                format!("Failed to remove skill directory {}", skill_dir.display())
            })?;
            info!(skill = %name, "WASM skill removed from store");
        }
        Ok(existed)
    }

    /// Replace who owns a skill and who may use it in one write. Returns the
    /// updated record.
    pub fn set_access(&self, name: &str, write: AccessWrite) -> Result<InstalledWasmSkill> {
        let mut installed = self
            .get_installed(name)
            .ok_or_else(|| eyre::eyre!("Skill '{name}' is not installed"))?;
        installed.owner = write.owner_after(installed.owner.as_deref());
        installed.visibility = write.visibility;
        installed.shares = write.grants;
        self.update_metadata(name, &installed)?;
        Ok(installed)
    }

    /// Update the installed metadata (e.g., state change).
    pub fn update_metadata(&self, name: &str, installed: &InstalledWasmSkill) -> Result<()> {
        let metadata_json = serde_json::to_string_pretty(installed)
            .wrap_err("Failed to serialise installation metadata")?;

        let path = self.metadata_path(name);
        if !path.parent().map(|p| p.is_dir()).unwrap_or(false) {
            bail!("Skill '{}' is not installed", name);
        }

        std::fs::write(&path, metadata_json)?;
        self.installed
            .write()
            .insert(name.to_string(), installed.clone());
        Ok(())
    }

    /// Scan the store directory and load all installed skill metadata.
    fn load_index(&self) -> Result<()> {
        let mut installed = self.installed.write();

        let entries = match std::fs::read_dir(&self.base_dir) {
            Ok(entries) => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(e) => {
                return Err(e)
                    .wrap_err_with(|| format!("Failed to read {}", self.base_dir.display()))
            }
        };

        for entry in entries {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }

            let name = entry.file_name().to_string_lossy().to_string();
            let metadata_path = self.metadata_path(&name);

            if metadata_path.is_file() {
                match std::fs::read_to_string(&metadata_path) {
                    Ok(json) => match serde_json::from_str::<InstalledWasmSkill>(&json) {
                        Ok(meta) => {
                            info!(skill = %name, version = %meta.manifest.version, "Loaded installed WASM skill from store");
                            installed.insert(name, meta);
                        }
                        Err(e) => {
                            tracing::warn!(
                                skill = %name,
                                error = %e,
                                "Skipping skill with invalid metadata"
                            );
                        }
                    },
                    Err(e) => {
                        tracing::warn!(
                            skill = %name,
                            error = %e,
                            "Skipping skill with unreadable metadata"
                        );
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::WasmSkillState;
    use chrono::Utc;
    use std::collections::HashSet;
    use workforce_core::access::{Grant, GrantLevel, Visibility};

    fn test_manifest() -> SkillManifest {
        SkillManifest {
            name: "test-skill".into(),
            version: "1.0.0".into(),
            description: "A test skill".into(),
            author: "test".into(),
            license: "MIT".into(),
            min_workforce_version: None,
            capabilities: HashSet::new(),
            tools: vec![crate::manifest::ManifestTool {
                name: "ping".into(),
                description: "Respond with pong".into(),
                requires_approval: false,
                approval_summary: None,
                input_schema: serde_json::json!({"type": "object"}),
                handler: None,
            }],
            credentials: vec![],
            config_schema: vec![],
            allowed_hosts: vec![],
            permissions: vec![],
        }
    }

    fn test_installed(manifest: &SkillManifest) -> InstalledWasmSkill {
        InstalledWasmSkill {
            manifest: manifest.clone(),
            state: WasmSkillState::Installed,
            installed_at: Utc::now(),
            source: "test".into(),
            config: HashMap::new(),
            sha256: String::new(),
            binary_version: 1,
            owner: None,
            visibility: Visibility::Private,
            shares: Vec::new(),
        }
    }

    /// Build a minimal skill project on disk.
    fn write_project(dir: &Path) {
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("Skill.toml"), "[skill]\nname = \"test-skill\"\n").unwrap();
        std::fs::write(dir.join("Cargo.toml"), "[package]\nname = \"test-skill\"\n").unwrap();
        std::fs::write(dir.join("src/lib.rs"), "pub fn real() -> u8 { 42 }").unwrap();
    }

    #[test]
    fn source_snapshot_round_trips_the_project() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        let project = tmp.path().join("project");
        write_project(&project);
        std::fs::write(project.join("Cargo.lock"), "# lock").unwrap();
        std::fs::create_dir_all(project.join("src/nested")).unwrap();
        std::fs::write(project.join("src/nested/mod.rs"), "// nested").unwrap();

        assert!(!store.has_source_snapshot("test-skill"));
        let snapshot = store
            .snapshot_source("test-skill", &project, &test_installed(&test_manifest()))
            .unwrap();
        assert_eq!(snapshot.files, 5);
        assert!(store.has_source_snapshot("test-skill"));

        let dest = tmp.path().join("restored");
        let restored = store
            .restore_source("test-skill", &dest, &CanonicalRoots::new(&[]))
            .unwrap();
        assert_eq!(restored.files, snapshot.files);
        for file in [
            "Skill.toml",
            "Cargo.toml",
            "Cargo.lock",
            "src/lib.rs",
            "src/nested/mod.rs",
        ] {
            assert_eq!(
                std::fs::read(project.join(file)).unwrap(),
                std::fs::read(dest.join(file)).unwrap(),
                "{file} differs after restore"
            );
        }
    }

    #[test]
    fn source_snapshot_skips_build_output_hidden_entries_and_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        let project = tmp.path().join("project");
        write_project(&project);
        std::fs::create_dir_all(project.join("src/target")).unwrap();
        std::fs::write(project.join("src/target/huge.bin"), vec![0u8; 4096]).unwrap();
        std::fs::write(project.join("src/.hidden"), "secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", project.join("src/link.rs")).unwrap();

        store
            .snapshot_source("test-skill", &project, &test_installed(&test_manifest()))
            .unwrap();
        let restored = tmp.path().join("restored");
        store
            .restore_source("test-skill", &restored, &CanonicalRoots::new(&[]))
            .unwrap();

        assert!(restored.join("src/lib.rs").is_file());
        assert!(!restored.join("src/target").exists());
        assert!(!restored.join("src/.hidden").exists());
        assert!(!restored.join("src/link.rs").exists());
    }

    #[test]
    fn visible_files_skip_hidden_entries_target_and_symlinks() {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path();
        std::fs::create_dir_all(root.join("docs/.cache")).unwrap();
        std::fs::create_dir_all(root.join("target")).unwrap();
        std::fs::create_dir_all(root.join("nested/target")).unwrap();
        std::fs::create_dir_all(root.join(".git")).unwrap();
        std::fs::write(root.join("SKILL.md"), "skill").unwrap();
        std::fs::write(root.join("config.toml"), "key = 1").unwrap();
        std::fs::write(root.join("docs/guide.md"), "guide").unwrap();
        std::fs::write(root.join("docs/.cache/entry"), "cache").unwrap();
        std::fs::write(root.join(".env"), "SECRET=1").unwrap();
        std::fs::write(root.join(".git/config"), "[remote]").unwrap();
        std::fs::write(root.join("target/out.bin"), "bin").unwrap();
        std::fs::write(root.join("nested/target/out.bin"), "bin").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink("/etc/passwd", root.join("docs/link.md")).unwrap();

        let mut files = visible_files(root).unwrap();
        files.sort();
        assert_eq!(
            files,
            vec![
                (PathBuf::from("SKILL.md"), 5),
                (PathBuf::from("config.toml"), 7),
                (PathBuf::from("docs/guide.md"), 5),
            ]
        );
    }

    #[test]
    fn oversized_source_is_refused_and_leaves_the_previous_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        let project = tmp.path().join("project");
        write_project(&project);
        store
            .snapshot_source("test-skill", &project, &test_installed(&test_manifest()))
            .unwrap();

        std::fs::write(
            project.join("src/huge.rs"),
            vec![b'x'; SOURCE_SNAPSHOT_MAX_BYTES as usize + 1],
        )
        .unwrap();
        let err = store
            .snapshot_source("test-skill", &project, &test_installed(&test_manifest()))
            .unwrap_err()
            .to_string();
        assert!(err.contains("snapshot budget"), "{err}");
        assert!(store.has_source_snapshot("test-skill"));
        assert!(!store.source_staging_dir("test-skill").exists());
        let restored = tmp.path().join("restored");
        store
            .restore_source("test-skill", &restored, &CanonicalRoots::new(&[]))
            .unwrap();
        assert!(
            !restored.join("src/huge.rs").exists(),
            "old snapshot intact"
        );
    }

    #[test]
    fn re_snapshot_replaces_rather_than_merges_and_clears_stale_staging() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        let project = tmp.path().join("project");
        write_project(&project);
        std::fs::write(project.join("src/gone.rs"), "// removed later").unwrap();
        store
            .snapshot_source("test-skill", &project, &test_installed(&test_manifest()))
            .unwrap();

        // A crash between staging and publish leaves this behind.
        let staging = store.source_staging_dir("test-skill");
        std::fs::create_dir_all(&staging).unwrap();
        std::fs::write(staging.join("junk.rs"), "junk").unwrap();

        std::fs::remove_file(project.join("src/gone.rs")).unwrap();
        store
            .snapshot_source("test-skill", &project, &test_installed(&test_manifest()))
            .unwrap();

        let restored = tmp.path().join("restored");
        store
            .restore_source("test-skill", &restored, &CanonicalRoots::new(&[]))
            .unwrap();
        assert!(restored.join("src/lib.rs").is_file());
        assert!(
            !restored.join("src/gone.rs").exists(),
            "stale file merged in"
        );
        assert!(
            !restored.join("junk.rs").exists(),
            "stale staging merged in"
        );
        assert!(!staging.exists());
    }

    /// The destination was checked, then the agent uid swapped it for a link
    /// into a directory of configuration: nothing lands there.
    #[cfg(unix)]
    #[test]
    fn restore_writes_nothing_through_a_link_into_configuration() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        let project = tmp.path().join("project");
        write_project(&project);
        store
            .snapshot_source("test-skill", &project, &test_installed(&test_manifest()))
            .unwrap();
        let workspace = tmp.path().join("workspace");
        std::fs::create_dir_all(workspace.join("learning")).unwrap();
        std::os::unix::fs::symlink(workspace.join("learning"), workspace.join("recovered"))
            .unwrap();

        let roots = CanonicalRoots::new(std::slice::from_ref(&workspace));
        assert!(store
            .restore_source("test-skill", &workspace.join("recovered"), &roots)
            .is_err());
        assert_eq!(
            std::fs::read_dir(workspace.join("learning"))
                .unwrap()
                .count(),
            0
        );
    }

    #[test]
    fn restore_refuses_a_non_empty_destination_and_a_missing_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        let project = tmp.path().join("project");
        write_project(&project);
        store
            .snapshot_source("test-skill", &project, &test_installed(&test_manifest()))
            .unwrap();

        let occupied = tmp.path().join("occupied");
        std::fs::create_dir_all(&occupied).unwrap();
        std::fs::write(occupied.join("mine.rs"), "my edits").unwrap();
        let err = store
            .restore_source("test-skill", &occupied, &CanonicalRoots::new(&[]))
            .unwrap_err()
            .to_string();
        assert!(err.contains("not empty"), "{err}");
        assert_eq!(
            std::fs::read_to_string(occupied.join("mine.rs")).unwrap(),
            "my edits"
        );

        let err = store
            .restore_source(
                "never-installed",
                &tmp.path().join("out"),
                &CanonicalRoots::new(&[]),
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("no source snapshot"), "{err}");
    }

    /// A binary-only reinstall re-versions the skill, which wipes its
    /// directory — the snapshot must survive so the source stays recoverable.
    #[test]
    fn reinstall_carries_the_source_snapshot_across_and_uninstall_removes_it() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        let project = tmp.path().join("project");
        write_project(&project);
        let manifest = test_manifest();

        store
            .install_prebuilt(
                manifest.clone(),
                b"v1",
                None,
                "test".into(),
                HashMap::new(),
                SkillOwnership::operator(),
            )
            .unwrap();
        store
            .snapshot_source("test-skill", &project, &test_installed(&test_manifest()))
            .unwrap();

        let reinstalled = store
            .install_prebuilt(
                manifest,
                b"v2",
                None,
                "test".into(),
                HashMap::new(),
                SkillOwnership::operator(),
            )
            .unwrap();
        assert_eq!(reinstalled.binary_version, 2);
        assert!(
            store.has_source_snapshot("test-skill"),
            "snapshot lost across re-version"
        );
        assert_eq!(store.load_binary("test-skill", 2).unwrap(), b"v2");

        store.remove("test-skill").unwrap();
        assert!(!store.has_source_snapshot("test-skill"));
    }

    /// A snapshot must not be able to read a directory outside the project:
    /// `src` symlinked elsewhere would be copied into the store and then out
    /// again into the workspace by `restore_source`.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_src_directory_is_not_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        let secrets = tmp.path().join("secrets");
        std::fs::create_dir_all(&secrets).unwrap();
        std::fs::write(secrets.join("credentials.toml"), "token = 'sensitive'").unwrap();

        let project = tmp.path().join("project");
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("Skill.toml"), "[skill]\n").unwrap();
        std::fs::write(project.join("Cargo.toml"), "[package]\n").unwrap();
        std::os::unix::fs::symlink(&secrets, project.join("src")).unwrap();

        let snapshot = store
            .snapshot_source("test-skill", &project, &test_installed(&test_manifest()))
            .unwrap();
        assert_eq!(snapshot.files, 2, "only the real root files may be stored");

        let restored = tmp.path().join("restored");
        store
            .restore_source("test-skill", &restored, &CanonicalRoots::new(&[]))
            .unwrap();
        assert!(!restored.join("credentials.toml").exists());
        assert!(!restored.join("src").exists());
    }

    /// The scaffold's `.cargo/config.toml` pins the wasm target; a project
    /// restored without it builds a host binary instead.
    #[test]
    fn snapshot_preserves_the_scaffolded_dotfiles() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        let project = tmp.path().join("project");
        write_project(&project);
        std::fs::create_dir_all(project.join(".cargo")).unwrap();
        std::fs::write(
            project.join(".cargo/config.toml"),
            "[build]\ntarget = \"wasm32-unknown-unknown\"\n",
        )
        .unwrap();
        std::fs::write(project.join(".gitignore"), "target/\n").unwrap();

        store
            .snapshot_source("test-skill", &project, &test_installed(&test_manifest()))
            .unwrap();
        let restored = tmp.path().join("restored");
        store
            .restore_source("test-skill", &restored, &CanonicalRoots::new(&[]))
            .unwrap();
        assert!(restored.join(".cargo/config.toml").is_file());
        assert!(restored.join(".gitignore").is_file());
    }

    /// A snapshot names the binary it was taken for, so a later binary-only
    /// install is detectable rather than silently implied to match.
    #[test]
    fn snapshot_provenance_tracks_the_installed_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        let project = tmp.path().join("project");
        write_project(&project);
        let manifest = test_manifest();

        let installed = store
            .install_prebuilt(
                manifest.clone(),
                b"v1",
                None,
                "test".into(),
                HashMap::new(),
                SkillOwnership::operator(),
            )
            .unwrap();
        store
            .snapshot_source("test-skill", &project, &installed)
            .unwrap();
        assert!(store.source_snapshot_matches_binary("test-skill"));
        assert_eq!(
            store
                .source_snapshot_meta("test-skill")
                .unwrap()
                .binary_version,
            installed.binary_version
        );

        store
            .install_prebuilt(
                manifest,
                b"v2-different",
                None,
                "test".into(),
                HashMap::new(),
                SkillOwnership::operator(),
            )
            .unwrap();
        assert!(store.has_source_snapshot("test-skill"), "still recoverable");
        assert!(
            !store.source_snapshot_matches_binary("test-skill"),
            "the stored source no longer built what is installed"
        );
    }

    /// A reinstall that fails after the skill directory is gone must not
    /// leave a snapshot behind for a skill that is no longer installed.
    #[test]
    fn a_failed_reinstall_does_not_strand_a_snapshot() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        let project = tmp.path().join("project");
        write_project(&project);
        let manifest = test_manifest();
        let installed = store
            .install_prebuilt(
                manifest.clone(),
                b"v1",
                None,
                "test".into(),
                HashMap::new(),
                SkillOwnership::operator(),
            )
            .unwrap();
        store
            .snapshot_source("test-skill", &project, &installed)
            .unwrap();

        let outcome: Result<()> = store.with_carried_source("test-skill", || {
            store.remove("test-skill")?;
            eyre::bail!("install failed after the directory was removed")
        });
        assert!(outcome.is_err());
        assert!(!store.has_source_snapshot("test-skill"));
        assert!(!tmp.path().join(".carry-test-skill").exists());
        assert!(store.get_installed("test-skill").is_none());
    }

    /// A publish interrupted after the retire step leaves the old snapshot as
    /// the only complete copy — readers must see it rather than report the
    /// skill as having no recoverable source, and the next publish settles it.
    #[test]
    fn an_interrupted_publish_stays_readable_and_settles_on_next_publish() {
        let tmp = tempfile::tempdir().unwrap();
        let manifest = test_manifest();
        {
            let store = WasmSkillStore::open(tmp.path()).unwrap();
            let project = tmp.path().join("project");
            write_project(&project);
            let installed = store
                .install_prebuilt(
                    manifest.clone(),
                    b"v1",
                    None,
                    "test".into(),
                    HashMap::new(),
                    SkillOwnership::operator(),
                )
                .unwrap();
            store
                .snapshot_source("test-skill", &project, &installed)
                .unwrap();
            // Crash between retire and publish.
            std::fs::rename(
                store.source_dir("test-skill"),
                store.source_retired_dir("test-skill"),
            )
            .unwrap();
        }

        let store = WasmSkillStore::open(tmp.path()).unwrap();
        assert!(
            store.has_source_snapshot("test-skill"),
            "retired copy is a complete snapshot"
        );
        assert!(!store.source_dir("test-skill").exists());
        let restored = tmp.path().join("restored");
        store
            .restore_source("test-skill", &restored, &CanonicalRoots::new(&[]))
            .unwrap();
        assert!(restored.join("src/lib.rs").is_file());

        // Opening the store must not touch the directories: another instance
        // may be publishing into them.
        assert!(store.source_retired_dir("test-skill").is_dir());

        let project = tmp.path().join("project2");
        write_project(&project);
        store
            .snapshot_source(
                "test-skill",
                &project,
                &store.get_installed("test-skill").unwrap(),
            )
            .unwrap();
        assert!(store.source_dir("test-skill").is_dir());
        assert!(!store.source_retired_dir("test-skill").exists());
    }

    /// A re-version wipes the skill directory; a retired-only snapshot is
    /// still the skill's source and must survive it.
    #[test]
    fn a_retired_only_snapshot_survives_a_reinstall() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        let project = tmp.path().join("project");
        write_project(&project);
        let manifest = test_manifest();
        let installed = store
            .install_prebuilt(
                manifest.clone(),
                b"v1",
                None,
                "test".into(),
                HashMap::new(),
                SkillOwnership::operator(),
            )
            .unwrap();
        store
            .snapshot_source("test-skill", &project, &installed)
            .unwrap();
        std::fs::rename(
            store.source_dir("test-skill"),
            store.source_retired_dir("test-skill"),
        )
        .unwrap();

        store
            .install_prebuilt(
                manifest,
                b"v2",
                None,
                "test".into(),
                HashMap::new(),
                SkillOwnership::operator(),
            )
            .unwrap();
        assert!(store.has_source_snapshot("test-skill"));
        let restored = tmp.path().join("restored");
        store
            .restore_source("test-skill", &restored, &CanonicalRoots::new(&[]))
            .unwrap();
        assert!(restored.join("src/lib.rs").is_file());
    }

    /// A retired copy beside a live one is scratch from a completed publish.
    #[test]
    fn a_retired_snapshot_beside_a_live_one_is_discarded() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        let project = tmp.path().join("project");
        write_project(&project);
        let installed = store
            .install_prebuilt(
                test_manifest(),
                b"v1",
                None,
                "test".into(),
                HashMap::new(),
                SkillOwnership::operator(),
            )
            .unwrap();
        store
            .snapshot_source("test-skill", &project, &installed)
            .unwrap();

        let retired = store.source_retired_dir("test-skill");
        std::fs::create_dir_all(&retired).unwrap();
        std::fs::write(retired.join("stale.rs"), "stale").unwrap();

        std::fs::write(project.join("src/lib.rs"), "// current").unwrap();
        store
            .snapshot_source("test-skill", &project, &installed)
            .unwrap();

        assert!(!retired.exists());
        assert!(!store.source_staging_dir("test-skill").exists());
        let restored = tmp.path().join("restored");
        store
            .restore_source("test-skill", &restored, &CanonicalRoots::new(&[]))
            .unwrap();
        assert_eq!(
            std::fs::read_to_string(restored.join("src/lib.rs")).unwrap(),
            "// current"
        );
        assert!(!restored.join("stale.rs").exists());
    }

    #[test]
    fn snapshot_refuses_a_project_with_no_source() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        let empty = tmp.path().join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        assert!(store
            .snapshot_source("test-skill", &empty, &test_installed(&test_manifest()))
            .is_err());
        assert!(store
            .snapshot_source(
                "test-skill",
                &tmp.path().join("nope"),
                &test_installed(&test_manifest())
            )
            .is_err());
    }

    #[test]
    fn store_and_load_binary() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();

        let manifest = test_manifest();
        let installed = test_installed(&manifest);
        let wasm_bytes = b"fake-wasm-binary";

        store
            .store_binary("test-skill", wasm_bytes, &manifest, &installed)
            .unwrap();

        let loaded = store.load_binary("test-skill", 1).unwrap();
        assert_eq!(loaded, wasm_bytes);
    }

    #[test]
    fn install_prebuilt_verifies_pin_and_versions() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        let manifest = test_manifest();
        let bytes = b"prebuilt-wasm";
        let pin = sha256_hex(bytes);

        let installed = store
            .install_prebuilt(
                manifest.clone(),
                bytes,
                Some(&pin),
                "catalog:test".into(),
                HashMap::new(),
                SkillOwnership::operator(),
            )
            .expect("installs with matching pin");
        assert_eq!(installed.binary_version, 1);
        assert_eq!(installed.sha256, pin);
        assert_eq!(store.load_binary("test-skill", 1).unwrap(), bytes);

        let reinstalled = store
            .install_prebuilt(
                manifest,
                bytes,
                Some(&pin),
                "catalog:test".into(),
                HashMap::new(),
                SkillOwnership::operator(),
            )
            .expect("reinstalls");
        assert_eq!(reinstalled.binary_version, 2);
    }

    #[test]
    fn install_prebuilt_stamps_then_preserves_ownership() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        let manifest = test_manifest();
        let owned = || SkillOwnership::owned("alice", Visibility::Global);

        let first = store
            .install_prebuilt(
                manifest.clone(),
                b"v1",
                None,
                "test".into(),
                HashMap::new(),
                owned(),
            )
            .unwrap();
        assert_eq!(first.owner.as_deref(), Some("alice"));
        assert_eq!(first.visibility, Visibility::Global);

        store
            .set_access(
                "test-skill",
                AccessWrite::kept(
                    Visibility::Private,
                    vec![Grant::user("bob", GrantLevel::Run)],
                ),
            )
            .unwrap();

        // A rebuild must not reassign the skill nor drop its grantees.
        let updated = store
            .install_prebuilt(
                manifest,
                b"v2",
                None,
                "test".into(),
                HashMap::new(),
                SkillOwnership::owned("mallory", Visibility::Global),
            )
            .unwrap();
        assert_eq!(updated.owner.as_deref(), Some("alice"));
        assert_eq!(updated.visibility, Visibility::Private);
        assert_eq!(updated.shares.len(), 1);
        assert_eq!(updated.binary_version, 2);
    }

    /// Consent pins the manifest and the binary, never who reaches the skill —
    /// so re-scoping a skill, including to the new `team` reach, must leave
    /// every grant a user has already given exactly where it was.
    #[test]
    fn re_scoping_a_skill_moves_neither_fingerprint() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        let manifest = test_manifest();
        let installed = store
            .install_prebuilt(
                manifest.clone(),
                b"bytes",
                None,
                "test".into(),
                HashMap::new(),
                SkillOwnership::owned("alice", Visibility::Private),
            )
            .unwrap();
        let build = installed.grant_fingerprint().unwrap();
        let contract = installed.contract_fingerprint();
        let toml = manifest.to_toml().unwrap();

        let rescoped = store
            .set_access(
                "test-skill",
                AccessWrite::kept(
                    Visibility::Team,
                    vec![Grant::team("platform", GrantLevel::Run)],
                ),
            )
            .unwrap();
        assert_eq!(rescoped.visibility, Visibility::Team);
        assert_eq!(rescoped.grant_fingerprint().unwrap(), build);
        assert_eq!(rescoped.contract_fingerprint(), contract);
        assert_eq!(rescoped.manifest.to_toml().unwrap(), toml);

        let reread = store.get_installed("test-skill").unwrap();
        assert_eq!(reread.grant_fingerprint().unwrap(), build);
        assert_eq!(reread.contract_fingerprint(), contract);
    }

    /// An ownerless (legacy / operator) record is claimable by the next
    /// install, which is how an operator skill becomes an owned one.
    #[test]
    fn reinstalling_an_ownerless_skill_takes_the_new_ownership() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        let manifest = test_manifest();
        store
            .install_prebuilt(
                manifest.clone(),
                b"v1",
                None,
                "test".into(),
                HashMap::new(),
                SkillOwnership::operator(),
            )
            .unwrap();
        let claimed = store
            .install_prebuilt(
                manifest,
                b"v2",
                None,
                "test".into(),
                HashMap::new(),
                SkillOwnership::owned("alice", Visibility::Private),
            )
            .unwrap();
        assert_eq!(claimed.owner.as_deref(), Some("alice"));
        assert_eq!(claimed.visibility, Visibility::Private);
    }

    #[test]
    fn set_access_persists_to_disk_and_index() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        store
            .install_prebuilt(
                test_manifest(),
                b"v1",
                None,
                "test".into(),
                HashMap::new(),
                SkillOwnership::owned("alice", Visibility::Private),
            )
            .unwrap();
        let shares = vec![Grant::user("bob", GrantLevel::Manage)];
        store
            .set_access(
                "test-skill",
                AccessWrite::kept(Visibility::Global, shares.clone()),
            )
            .unwrap();

        assert_eq!(
            store.get_installed("test-skill").unwrap().visibility,
            Visibility::Global
        );
        let reopened = WasmSkillStore::open(tmp.path()).unwrap();
        let record = reopened.get_installed("test-skill").unwrap();
        assert_eq!(record.visibility, Visibility::Global);
        assert_eq!(record.shares, shares);
        assert_eq!(record.owner.as_deref(), Some("alice"));

        assert!(reopened
            .set_access("missing", AccessWrite::kept(Visibility::Global, Vec::new()))
            .is_err());
    }

    #[test]
    fn install_prebuilt_rejects_pin_mismatch() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        let err = store
            .install_prebuilt(
                test_manifest(),
                b"bytes",
                Some("deadbeef"),
                "s".into(),
                HashMap::new(),
                SkillOwnership::operator(),
            )
            .unwrap_err()
            .to_string();
        assert!(err.contains("sha256 mismatch"), "got: {err}");
        assert!(store.get_installed("test-skill").is_none());
    }

    #[test]
    fn store_and_load_manifest() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();

        let manifest = test_manifest();
        let installed = test_installed(&manifest);

        store
            .store_binary("test-skill", b"wasm", &manifest, &installed)
            .unwrap();

        let loaded = store.load_manifest("test-skill").unwrap();
        assert_eq!(loaded.name, "test-skill");
        assert_eq!(loaded.version, "1.0.0");
    }

    #[test]
    fn list_and_get_installed() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();

        let manifest = test_manifest();
        let installed = test_installed(&manifest);

        store
            .store_binary("test-skill", b"wasm", &manifest, &installed)
            .unwrap();

        let list = store.list_installed();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].manifest.name, "test-skill");

        let got = store.get_installed("test-skill").unwrap();
        assert_eq!(got.manifest.name, "test-skill");
        assert!(store.get_installed("nonexistent").is_none());
    }

    #[test]
    fn remove_skill() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();

        let manifest = test_manifest();
        let installed = test_installed(&manifest);

        store
            .store_binary("test-skill", b"wasm", &manifest, &installed)
            .unwrap();

        assert!(store.remove("test-skill").unwrap());
        assert!(!store.remove("test-skill").unwrap());
        assert!(store.list_installed().is_empty());
        assert!(!store.skill_dir("test-skill").exists());
    }

    #[test]
    fn reload_index_from_disk() {
        let tmp = tempfile::tempdir().unwrap();

        // Store a skill
        {
            let store = WasmSkillStore::open(tmp.path()).unwrap();
            let manifest = test_manifest();
            let installed = test_installed(&manifest);
            store
                .store_binary("test-skill", b"wasm", &manifest, &installed)
                .unwrap();
        }

        // Open a new store and verify index is loaded from disk
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        let list = store.list_installed();
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].manifest.name, "test-skill");
    }

    #[test]
    fn update_metadata() {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();

        let manifest = test_manifest();
        let mut installed = test_installed(&manifest);

        store
            .store_binary("test-skill", b"wasm", &manifest, &installed)
            .unwrap();

        installed.state = WasmSkillState::Active;
        store.update_metadata("test-skill", &installed).unwrap();

        let got = store.get_installed("test-skill").unwrap();
        assert_eq!(got.state, WasmSkillState::Active);
    }
}
