//! Skill catalog — a curated manifest in a Git repo that lists installable
//! Workforce skills, so an install can browse and pull from it. The manifest
//! lives at a known path inside the source repo (e.g. `workforce/catalog.json`);
//! each entry points at a repo subdirectory holding that skill. Both kinds of
//! Workforce skill are listable: external (`SKILL.md`) and compiled WASM
//! (`Skill.toml` + either a prebuilt, sha256-pinned `.wasm` or source the
//! installing hub compiles at the catalog ref).

use serde::{Deserialize, Serialize};

/// Which kind of skill a catalog entry installs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SkillKind {
    /// A `SKILL.md` instruction skill, installed into the skills directory.
    External,
    /// A compiled WASM skill, installed into the WASM skill store.
    Wasm,
}

impl SkillKind {
    /// The directory, beside a catalog's manifest, that new skills of this kind are published under.
    pub fn catalog_dir(self) -> &'static str {
        match self {
            SkillKind::External => "external",
            SkillKind::Wasm => "wasm",
        }
    }
}

/// One installable skill listed in a catalog manifest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CatalogEntry {
    pub name: String,
    pub kind: SkillKind,
    /// Repo-relative directory holding the skill. Validated against traversal.
    pub subpath: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    #[serde(default)]
    pub description: String,
    /// Binaries an external skill needs on PATH — surfaced before install so
    /// operators see prerequisites.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub requires_bins: Vec<String>,
    /// WASM only: the prebuilt `.wasm`, relative to `subpath`. Omitted together
    /// with `sha256`, the skill is built from source at install.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wasm_path: Option<String>,
    /// WASM only: hex SHA-256 pin of the prebuilt `.wasm`, required alongside
    /// `wasm_path` — the remote bytes are otherwise unverified.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sha256: Option<String>,
}

/// A parsed catalog manifest (`workforce/catalog.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CatalogManifest {
    #[serde(default)]
    pub name: String,
    /// Manifest schema version.
    #[serde(default)]
    pub version: u32,
    pub skills: Vec<CatalogEntry>,
}

impl CatalogManifest {
    /// Parse and validate a manifest from JSON.
    pub fn parse(json: &str) -> eyre::Result<Self> {
        let manifest: Self = serde_json::from_str(json)
            .map_err(|e| eyre::eyre!("invalid catalog manifest JSON: {e}"))?;
        manifest.validate()?;
        Ok(manifest)
    }

    /// Look up an entry by name.
    pub fn find(&self, name: &str) -> Option<&CatalogEntry> {
        self.skills.iter().find(|s| s.name == name)
    }

    /// Replace the entry with `entry.name` in place, or append it.
    pub fn upsert(&mut self, entry: CatalogEntry) {
        match self.skills.iter_mut().find(|s| s.name == entry.name) {
            Some(existing) => *existing = entry,
            None => self.skills.push(entry),
        }
    }

    /// The manifest as it is kept in the repo: pretty-printed, trailing newline.
    pub fn to_json_pretty(&self) -> eyre::Result<String> {
        Ok(format!("{}\n", serde_json::to_string_pretty(self)?))
    }

    fn validate(&self) -> eyre::Result<()> {
        let mut seen = std::collections::HashSet::new();
        for entry in &self.skills {
            entry.validate()?;
            if !seen.insert(entry.name.as_str()) {
                return Err(eyre::eyre!("duplicate skill '{}' in catalog", entry.name));
            }
        }
        Ok(())
    }
}

impl CatalogEntry {
    fn validate(&self) -> eyre::Result<()> {
        if self.name.trim().is_empty() {
            return Err(eyre::eyre!("catalog entry has an empty name"));
        }
        crate::installer::validate_subpath(&self.subpath)
            .map_err(|e| eyre::eyre!("skill '{}': subpath {e}", self.name))?;
        match self.kind {
            SkillKind::External => {}
            SkillKind::Wasm => match (self.wasm_path.as_deref(), self.sha256_pin()) {
                (None, None) => {}
                (Some(wasm_path), Some(_)) => {
                    crate::installer::validate_subpath(wasm_path)
                        .map_err(|e| eyre::eyre!("skill '{}': wasm_path {e}", self.name))?;
                }
                (Some(_), None) => {
                    return Err(eyre::eyre!(
                        "wasm skill '{}' names a `wasm_path` without its `sha256` pin",
                        self.name
                    ));
                }
                (None, Some(_)) => {
                    return Err(eyre::eyre!(
                        "wasm skill '{}' has a `sha256` pin but no `wasm_path`",
                        self.name
                    ));
                }
            },
        }
        Ok(())
    }

    fn sha256_pin(&self) -> Option<&str> {
        self.sha256
            .as_deref()
            .map(str::trim)
            .filter(|pin| !pin.is_empty())
    }

    /// A WASM entry with no prebuilt artefact: the installing hub compiles the
    /// skill's `subpath` at the catalog ref.
    pub fn builds_from_source(&self) -> bool {
        self.kind == SkillKind::Wasm && self.wasm_path.is_none()
    }

    /// The `<repo>#subpath=<dir>` argument for [`crate::installer::locate_skill`]
    /// (external skills clone the repo and copy this subdir).
    pub fn external_source(&self, repo: &str) -> String {
        format!("{}#subpath={}", repo.trim_end_matches('/'), self.subpath)
    }

    /// Repo-relative path of the skill's `Skill.toml` (WASM).
    pub fn manifest_repo_path(&self) -> String {
        join_repo_path(&self.subpath, "Skill.toml")
    }

    /// Repo-relative path of the prebuilt `.wasm` (WASM); `None` for external.
    pub fn wasm_repo_path(&self) -> Option<String> {
        self.wasm_path
            .as_deref()
            .map(|w| join_repo_path(&self.subpath, w))
    }
}

fn join_repo_path(dir: &str, rel: &str) -> String {
    format!("{}/{}", dir.trim_matches('/'), rel.trim_matches('/'))
}

/// Extract `owner/repo` from a GitHub repo URL or an already-bare `owner/repo`.
pub fn github_owner_repo(repo: &str) -> eyre::Result<String> {
    let trimmed = repo.trim().trim_end_matches('/');
    let tail = trimmed
        .strip_prefix("https://github.com/")
        .or_else(|| trimmed.strip_prefix("http://github.com/"))
        .or_else(|| trimmed.strip_prefix("git@github.com:"))
        .unwrap_or(trimmed);
    let tail = tail.strip_suffix(".git").unwrap_or(tail);
    let parts: Vec<&str> = tail.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() != 2 {
        return Err(eyre::eyre!(
            "catalog repo '{repo}' is not a github.com owner/repo reference"
        ));
    }
    Ok(format!("{}/{}", parts[0], parts[1]))
}

/// Raw-content URL for a repo-relative path at a ref.
pub fn raw_url(owner_repo: &str, git_ref: &str, path: &str) -> String {
    format!(
        "https://raw.githubusercontent.com/{}/{}/{}",
        owner_repo,
        git_ref,
        path.trim_start_matches('/')
    )
}

#[cfg(feature = "github")]
async fn authed_get(url: &str, token: Option<&str>) -> eyre::Result<reqwest::Response> {
    let mut req = reqwest::Client::new()
        .get(url)
        .header("User-Agent", "workforce-skill-catalog");
    if let Some(t) = token {
        req = req.header("Authorization", format!("Bearer {t}"));
    }
    let resp = req.send().await?;
    if !resp.status().is_success() {
        return Err(eyre::eyre!("HTTP {} fetching {url}", resp.status()));
    }
    Ok(resp)
}

/// Fetch the text body at a raw-content URL, authenticating when a token is set.
#[cfg(feature = "github")]
pub async fn fetch_text(url: &str, token: Option<&str>) -> eyre::Result<String> {
    Ok(authed_get(url, token).await?.text().await?)
}

/// Fetch the raw bytes at a URL, authenticating when a token is set.
#[cfg(feature = "github")]
pub async fn fetch_bytes(url: &str, token: Option<&str>) -> eyre::Result<Vec<u8>> {
    Ok(authed_get(url, token).await?.bytes().await?.to_vec())
}

/// Fetch and parse a catalog manifest from a GitHub repo.
#[cfg(feature = "github")]
pub async fn fetch_manifest(
    repo: &str,
    git_ref: &str,
    manifest_path: &str,
    token: Option<&str>,
) -> eyre::Result<CatalogManifest> {
    let owner_repo = github_owner_repo(repo)?;
    let url = raw_url(&owner_repo, git_ref, manifest_path);
    let text = fetch_text(&url, token).await?;
    CatalogManifest::parse(&text)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "name": "acme-workforce-skills",
      "version": 1,
      "skills": [
        {
          "name": "support-triage",
          "kind": "external",
          "subpath": "workforce/external/support-triage",
          "version": "1.0.0",
          "description": "Triage worklist.",
          "requires_bins": ["python3"]
        },
        {
          "name": "orbit",
          "kind": "wasm",
          "subpath": "workforce/wasm/orbit",
          "wasm_path": "orbit.wasm",
          "sha256": "abc123"
        }
      ]
    }"#;

    #[test]
    fn kinds_name_their_catalog_directory() {
        assert_eq!(SkillKind::External.catalog_dir(), "external");
        assert_eq!(SkillKind::Wasm.catalog_dir(), "wasm");
    }

    #[test]
    fn parses_mixed_manifest() {
        let m = CatalogManifest::parse(SAMPLE).expect("parses");
        assert_eq!(m.skills.len(), 2);
        let ext = m.find("support-triage").unwrap();
        assert_eq!(ext.kind, SkillKind::External);
        assert_eq!(ext.requires_bins, vec!["python3"]);
        let wasm = m.find("orbit").unwrap();
        assert_eq!(wasm.kind, SkillKind::Wasm);
        assert_eq!(
            wasm.wasm_repo_path().as_deref(),
            Some("workforce/wasm/orbit/orbit.wasm")
        );
        assert_eq!(wasm.manifest_repo_path(), "workforce/wasm/orbit/Skill.toml");
    }

    #[test]
    fn wasm_path_requires_sha256() {
        let json = r#"{"skills":[{"name":"x","kind":"wasm","subpath":"a","wasm_path":"x.wasm"}]}"#;
        let err = CatalogManifest::parse(json).unwrap_err().to_string();
        assert!(err.contains("sha256"), "got: {err}");
    }

    #[test]
    fn sha256_requires_wasm_path() {
        let json = r#"{"skills":[{"name":"x","kind":"wasm","subpath":"a","sha256":"d"}]}"#;
        let err = CatalogManifest::parse(json).unwrap_err().to_string();
        assert!(err.contains("wasm_path"), "got: {err}");
    }

    #[test]
    fn wasm_without_artefact_builds_from_source() {
        let json = r#"{"skills":[
            {"name":"src","kind":"wasm","subpath":"workforce/wasm/src"},
            {"name":"bin","kind":"wasm","subpath":"workforce/wasm/bin","wasm_path":"bin.wasm","sha256":"d"},
            {"name":"ext","kind":"external","subpath":"workforce/external/ext"}
        ]}"#;
        let manifest = CatalogManifest::parse(json).unwrap();
        let entry = |name: &str| manifest.find(name).unwrap();
        assert!(entry("src").builds_from_source());
        assert_eq!(entry("src").wasm_repo_path(), None);
        assert!(!entry("bin").builds_from_source());
        assert!(!entry("ext").builds_from_source());
    }

    #[test]
    fn blank_sha256_counts_as_absent() {
        let json = r#"{"skills":[{"name":"x","kind":"wasm","subpath":"a","wasm_path":"x.wasm","sha256":"  "}]}"#;
        let err = CatalogManifest::parse(json).unwrap_err().to_string();
        assert!(err.contains("sha256"), "got: {err}");
    }

    #[test]
    fn rejects_traversal_subpath() {
        let json = r#"{"skills":[{"name":"x","kind":"external","subpath":"../etc"}]}"#;
        assert!(CatalogManifest::parse(json).is_err());
    }

    #[test]
    fn rejects_duplicate_names() {
        let json = r#"{"skills":[
            {"name":"x","kind":"external","subpath":"a"},
            {"name":"x","kind":"external","subpath":"b"}
        ]}"#;
        let err = CatalogManifest::parse(json).unwrap_err().to_string();
        assert!(err.contains("duplicate"), "got: {err}");
    }

    #[test]
    fn external_source_appends_subpath() {
        let entry = CatalogEntry {
            name: "x".into(),
            kind: SkillKind::External,
            subpath: "workforce/external/x".into(),
            version: None,
            description: String::new(),
            requires_bins: vec![],
            wasm_path: None,
            sha256: None,
        };
        assert_eq!(
            entry.external_source("https://github.com/o/r/"),
            "https://github.com/o/r#subpath=workforce/external/x"
        );
    }

    #[test]
    fn owner_repo_from_various_forms() {
        assert_eq!(github_owner_repo("https://github.com/o/r").unwrap(), "o/r");
        assert_eq!(
            github_owner_repo("https://github.com/o/r.git").unwrap(),
            "o/r"
        );
        assert_eq!(github_owner_repo("https://github.com/o/r/").unwrap(), "o/r");
        assert_eq!(github_owner_repo("git@github.com:o/r.git").unwrap(), "o/r");
        assert_eq!(github_owner_repo("o/r").unwrap(), "o/r");
        assert!(github_owner_repo("https://github.com/o").is_err());
    }

    fn wasm_entry(name: &str, version: &str) -> CatalogEntry {
        CatalogEntry {
            name: name.into(),
            kind: SkillKind::Wasm,
            subpath: format!("workforce/wasm/{name}"),
            version: Some(version.into()),
            description: String::new(),
            requires_bins: vec![],
            wasm_path: None,
            sha256: None,
        }
    }

    #[test]
    fn upsert_replaces_in_place_or_appends() {
        let mut manifest = CatalogManifest::parse(SAMPLE).unwrap();
        let names =
            |m: &CatalogManifest| m.skills.iter().map(|s| s.name.clone()).collect::<Vec<_>>();
        let before = names(&manifest);

        manifest.upsert(wasm_entry("support-triage", "9.9.9"));
        assert_eq!(names(&manifest), before);
        assert_eq!(
            manifest.find("support-triage").unwrap().version.as_deref(),
            Some("9.9.9")
        );

        manifest.upsert(wasm_entry("brand-new", "0.1.0"));
        assert_eq!(
            names(&manifest).last().map(String::as_str),
            Some("brand-new")
        );
        assert_eq!(manifest.skills.len(), before.len() + 1);
    }

    #[test]
    fn pretty_json_round_trips_through_parse_without_absent_fields() {
        let mut manifest = CatalogManifest::parse(SAMPLE).unwrap();
        manifest.upsert(wasm_entry("built-from-source", "0.1.0"));
        let json = manifest.to_json_pretty().unwrap();
        assert!(json.ends_with("}\n"));
        assert!(json.contains("\n  \"skills\": ["));
        assert!(!json.contains("null"), "{json}");
        assert!(!json.contains("\"requires_bins\": []"), "{json}");
        let reparsed = CatalogManifest::parse(&json).unwrap();
        assert_eq!(reparsed.skills, manifest.skills);
    }

    #[test]
    fn raw_url_builds_expected() {
        assert_eq!(
            raw_url("o/r", "main", "workforce/catalog.json"),
            "https://raw.githubusercontent.com/o/r/main/workforce/catalog.json"
        );
    }
}
