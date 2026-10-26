//! Parser and resolver for `end-user-docs/docs-watch.toml`.
//!
//! Pure logic — all IO at the edges. Globs use the `glob` crate's
//! `Pattern::matches`, which understands `**` across path segments.
//!
//! Note the asymmetry between `glob::Pattern::matches` (used in
//! `pages_for_changed_files`) and `glob::glob` (used in
//! `sources_for_page`): the former matches `foo/**` against
//! `foo/a/b.rs`, the latter does NOT — `glob::glob`'s trailing `**`
//! only yields directories. Patterns in `docs-watch.toml` therefore
//! use `**/*` so both code paths agree.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};
use glob::Pattern;
use serde::Deserialize;

#[derive(Debug, Default, Deserialize)]
pub struct WatchConfig {
    #[serde(default, rename = "watch")]
    pub entries: Vec<WatchEntry>,
    /// Per-page settings, keyed by page path (`configuration/index.mdx`).
    #[serde(default)]
    pub pages: std::collections::BTreeMap<String, PageSettings>,
}

/// How a page's examples are checked.
#[derive(Debug, Default, Deserialize, Clone)]
#[serde(deny_unknown_fields)]
pub struct PageSettings {
    /// The file the page's untitled TOML blocks show, e.g.
    /// `"workforce.toml"`. A block titled with a file name is that file.
    /// Unset: untitled blocks aren't checked.
    #[serde(default)]
    pub toml: Option<String>,
}

impl WatchConfig {
    /// The schema a page's untitled TOML blocks are checked against.
    pub fn toml_schema(&self, page: &str) -> super::check::TomlSchema {
        match self.pages.get(page).and_then(|p| p.toml.as_deref()) {
            Some("workforce.toml") => super::check::TomlSchema::Workforce,
            _ => super::check::TomlSchema::Unchecked,
        }
    }
}

#[derive(Debug, Deserialize, Clone)]
pub struct WatchEntry {
    pub paths: Vec<String>,
    pub pages: Vec<String>,
}

pub fn load_watch_config(path: &Path) -> Result<WatchConfig> {
    let text =
        std::fs::read_to_string(path).wrap_err_with(|| format!("read {}", path.display()))?;
    toml::from_str(&text).wrap_err_with(|| format!("parse {}", path.display()))
}

pub fn all_pages(cfg: &WatchConfig) -> BTreeSet<String> {
    cfg.entries
        .iter()
        .flat_map(|e| e.pages.iter().cloned())
        .collect()
}

pub fn sources_for_page(cfg: &WatchConfig, page: &str, repo_root: &Path) -> Result<Vec<PathBuf>> {
    let mut out: BTreeSet<PathBuf> = BTreeSet::new();
    for entry in &cfg.entries {
        if !entry.pages.iter().any(|p| p == page) {
            continue;
        }
        for raw in &entry.paths {
            let full = repo_root.join(raw);
            let pattern = full.to_string_lossy().to_string();
            for hit in glob::glob(&pattern).wrap_err_with(|| format!("invalid glob: {raw}"))? {
                let p = hit.wrap_err("glob walk")?;
                if p.is_file() {
                    out.insert(p);
                }
            }
        }
    }
    Ok(out.into_iter().collect())
}

pub fn pages_for_changed_files(
    cfg: &WatchConfig,
    changed_files: &[String],
) -> Result<BTreeSet<String>> {
    let mut affected = BTreeSet::new();
    for entry in &cfg.entries {
        let patterns: Vec<Pattern> = entry
            .paths
            .iter()
            .map(|p| Pattern::new(p))
            .collect::<std::result::Result<Vec<_>, _>>()
            .wrap_err("compile watch pattern")?;
        let triggered = changed_files
            .iter()
            .any(|f| patterns.iter().any(|p: &Pattern| p.matches(f)));
        if triggered {
            affected.extend(entry.pages.iter().cloned());
        }
    }
    Ok(affected)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with(entries: &[(&[&str], &[&str])]) -> WatchConfig {
        WatchConfig {
            entries: entries
                .iter()
                .map(|(paths, pages)| WatchEntry {
                    paths: paths.iter().map(|s| s.to_string()).collect(),
                    pages: pages.iter().map(|s| s.to_string()).collect(),
                })
                .collect(),
            ..WatchConfig::default()
        }
    }

    #[test]
    fn parses_minimal_config() {
        let cfg: WatchConfig = toml::from_str(
            r#"
            [[watch]]
            paths = ["crates/skills/src/adapters/slack/**"]
            pages = ["adapters/slack.mdx"]
            "#,
        )
        .unwrap();
        assert_eq!(cfg.entries.len(), 1);
        assert_eq!(cfg.entries[0].pages, vec!["adapters/slack.mdx"]);
    }

    #[test]
    fn parses_empty_config() {
        let cfg: WatchConfig = toml::from_str("").unwrap();
        assert!(cfg.entries.is_empty());
    }

    #[test]
    fn all_pages_dedups() {
        let cfg = config_with(&[
            (&["a/**"], &["x.mdx", "y.mdx"]),
            (&["b/**"], &["y.mdx", "z.mdx"]),
        ]);
        let pages = all_pages(&cfg);
        assert_eq!(pages.len(), 3);
        assert!(pages.contains("x.mdx"));
        assert!(pages.contains("y.mdx"));
        assert!(pages.contains("z.mdx"));
    }

    #[test]
    fn changed_files_triggers_matching_pages() {
        let cfg = config_with(&[
            (
                &["crates/skills/src/adapters/slack/**"],
                &["adapters/slack.mdx"],
            ),
            (
                &["crates/skills/src/adapters/jira/**"],
                &["adapters/jira.mdx"],
            ),
        ]);
        let changed = vec![
            "crates/skills/src/adapters/slack/lib.rs".to_string(),
            "README.md".to_string(),
        ];
        let pages = pages_for_changed_files(&cfg, &changed).unwrap();
        assert_eq!(pages.len(), 1);
        assert!(pages.contains("adapters/slack.mdx"));
    }

    #[test]
    fn ignores_unmatched_files() {
        let cfg = config_with(&[(
            &["crates/skills/src/adapters/slack/**"],
            &["adapters/slack.mdx"],
        )]);
        let changed = vec!["README.md".to_string(), "ui/package.json".to_string()];
        let pages = pages_for_changed_files(&cfg, &changed).unwrap();
        assert!(pages.is_empty());
    }

    #[test]
    fn recursive_glob_matches_nested_paths() {
        let cfg = config_with(&[(&["crates/**/*.rs"], &["reference/tools.mdx"])]);
        let changed = vec!["crates/a/b/c/deep.rs".to_string()];
        let pages = pages_for_changed_files(&cfg, &changed).unwrap();
        assert!(pages.contains("reference/tools.mdx"));
    }

    #[test]
    fn multiple_entries_can_trigger_same_page() {
        let cfg = config_with(&[
            (&["crates/a/**"], &["agents.mdx"]),
            (&["crates/b/**"], &["agents.mdx"]),
        ]);
        let changed = vec!["crates/b/x.rs".to_string()];
        let pages = pages_for_changed_files(&cfg, &changed).unwrap();
        assert_eq!(pages.len(), 1);
        assert!(pages.contains("agents.mdx"));
    }

    #[test]
    fn sources_for_page_walks_recursively_with_star_star_slash_star() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src/inner/deep")).unwrap();
        std::fs::write(root.join("src/top.rs"), "fn t() {}").unwrap();
        std::fs::write(root.join("src/inner/mid.rs"), "fn m() {}").unwrap();
        std::fs::write(root.join("src/inner/deep/bottom.rs"), "fn b() {}").unwrap();

        let cfg = config_with(&[(&["src/**/*"], &["page.mdx"])]);
        let sources = sources_for_page(&cfg, "page.mdx", root).unwrap();

        let names: Vec<String> = sources
            .iter()
            .map(|p| p.strip_prefix(root).unwrap().display().to_string())
            .collect();
        assert!(
            names.contains(&"src/top.rs".to_string()),
            "missing top.rs in {names:?}"
        );
        assert!(names.contains(&"src/inner/mid.rs".to_string()));
        assert!(names.contains(&"src/inner/deep/bottom.rs".to_string()));
    }

    #[test]
    fn sources_for_page_returns_empty_for_unmatched_page() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = config_with(&[(&["src/**/*"], &["page.mdx"])]);
        let sources = sources_for_page(&cfg, "different.mdx", dir.path()).unwrap();
        assert!(sources.is_empty());
    }

    #[test]
    fn trailing_double_star_alone_is_a_footgun() {
        // Documents the asymmetry between Pattern::matches and glob::glob.
        // `crates/cli/src/**` matches via Pattern::matches but yields no
        // files from glob::glob — hence the `**/*` convention in
        // docs-watch.toml.
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("src")).unwrap();
        std::fs::write(root.join("src/top.rs"), "").unwrap();

        let cfg = config_with(&[(&["src/**"], &["page.mdx"])]);
        let sources = sources_for_page(&cfg, "page.mdx", root).unwrap();
        assert!(
            sources.is_empty(),
            "footgun: trailing `**` returns no files"
        );

        let cfg = config_with(&[(&["src/**/*"], &["page.mdx"])]);
        let sources = sources_for_page(&cfg, "page.mdx", root).unwrap();
        assert_eq!(sources.len(), 1);
    }
}
