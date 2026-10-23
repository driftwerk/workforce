//! Post-edit verification: `npm run build` (the real fumadocs-mdx +
//! `tsc` + `vite` pipeline, which is the only thing that actually
//! *compiles* each MDX body) + `docs:verify-links` (link integrity).
//!
//! We run the full build rather than the standalone `fumadocs-mdx`
//! generate step because the latter only builds the collection index;
//! it does not catch MDX body syntax errors (e.g. an unescaped `<`
//! before a digit), which surface only at `vite build` time. Letting
//! those through is how an un-buildable page reaches `main` and breaks
//! the docs-site CI.
//!
//! The outcome carries per-page failure attribution parsed out of the
//! subprocess output, so the orchestrator can flag only the PRs whose
//! pages actually broke rather than poisoning the whole batch. Build
//! failures unrelated to docs (e.g. a `tsc` error) carry no
//! `end-user-docs/<page>` marker, so they attribute to no page and
//! never falsely flag a curated PR.

use std::collections::BTreeMap;
use std::path::Path;

use eyre::{Context, Result};
use tokio::process::Command;

#[derive(Debug, Default)]
pub struct VerificationOutcome {
    pub broken_links_by_page: BTreeMap<String, Vec<String>>,
    pub mdx_errors_by_page: BTreeMap<String, String>,
    /// `npm run build` exited non-zero. Crucially this is true even when the
    /// failure can't be attributed to a specific edited page — e.g. `vite`
    /// stops at the FIRST bad MDX so later broken pages in the same batch carry
    /// no marker, or a pre-existing broken page on `main` fails the build before
    /// this run's pages are reached. In those cases `mdx_errors_by_page` misses
    /// pages, so the build never being green is the only safe signal.
    pub build_failed: bool,
    pub raw_output: String,
}

impl VerificationOutcome {
    pub fn any_failure(&self) -> bool {
        self.build_failed
            || !self.broken_links_by_page.is_empty()
            || !self.mdx_errors_by_page.is_empty()
    }

    /// Failure note for a single page, or None if it passed.
    pub fn page_failure(&self, page: &str) -> Option<String> {
        let mut sections: Vec<String> = Vec::new();
        if let Some(err) = self.mdx_errors_by_page.get(page) {
            sections.push(format!("MDX error:\n{err}"));
        }
        if let Some(links) = self.broken_links_by_page.get(page) {
            let lines = links
                .iter()
                .map(|l| format!("  - {l}"))
                .collect::<Vec<_>>()
                .join("\n");
            sections.push(format!("Broken /docs links:\n{lines}"));
        }
        if sections.is_empty() {
            None
        } else {
            Some(sections.join("\n\n"))
        }
    }

    /// Failure note for a bundle of pages, grouped by page, or None
    /// if every page in the bundle passed.
    pub fn failure_summary(&self, pages: &[&str]) -> Option<String> {
        let entries: Vec<String> = pages
            .iter()
            .filter_map(|p| self.page_failure(p).map(|note| format!("### {p}\n{note}")))
            .collect();
        if entries.is_empty() {
            None
        } else {
            Some(entries.join("\n\n"))
        }
    }
}

pub async fn verify(ui_dir: &Path) -> Result<VerificationOutcome> {
    let build = run_npm_script(ui_dir, "build").await?;
    let links = run_npm_script(ui_dir, "docs:verify-links").await?;
    Ok(VerificationOutcome {
        broken_links_by_page: parse_broken_links(&links.combined_output),
        mdx_errors_by_page: parse_mdx_errors(&build.combined_output),
        build_failed: !build.success,
        raw_output: join_outputs(&build.combined_output, &links.combined_output),
    })
}

struct StepResult {
    success: bool,
    combined_output: String,
}

async fn run_npm_script(workdir: &Path, script: &str) -> Result<StepResult> {
    let out = Command::new("npm")
        .args(["run", script])
        .current_dir(workdir)
        .output()
        .await
        .wrap_err_with(|| format!("spawn npm run {script}"))?;
    Ok(StepResult {
        success: out.status.success(),
        combined_output: combine_output(&out.stdout, &out.stderr),
    })
}

fn combine_output(stdout: &[u8], stderr: &[u8]) -> String {
    let so = String::from_utf8_lossy(stdout);
    let se = String::from_utf8_lossy(stderr);
    match (so.trim().is_empty(), se.trim().is_empty()) {
        (true, true) => String::new(),
        (false, true) => so.into_owned(),
        (true, false) => se.into_owned(),
        (false, false) => format!("stdout:\n{so}\nstderr:\n{se}"),
    }
}

fn join_outputs(a: &str, b: &str) -> String {
    match (a.trim().is_empty(), b.trim().is_empty()) {
        (true, true) => String::new(),
        (false, true) => a.to_string(),
        (true, false) => b.to_string(),
        (false, false) => format!("=== npm run build ===\n{a}\n\n=== docs:verify-links ===\n{b}"),
    }
}

/// Walk verify-docs-links output, extract `<page>: <href>` entries
/// listed after each `Found N broken /docs link(s):` header. Multiple
/// such headers in one buffer are handled (a previous run's output
/// concatenated with the current one is parsed in full).
fn parse_broken_links(output: &str) -> BTreeMap<String, Vec<String>> {
    let mut out: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let mut in_section = false;
    for line in output.lines() {
        if line.trim_start().starts_with("Found ") && line.contains("broken /docs link") {
            in_section = true;
            continue;
        }
        if !in_section {
            continue;
        }
        let Some((page, href)) = line.trim_start().split_once(": ") else {
            in_section = false;
            continue;
        };
        if !href.starts_with("/docs") {
            in_section = false;
            continue;
        }
        out.entry(page.to_string())
            .or_default()
            .push(href.to_string());
    }
    out
}

/// Walk fumadocs-mdx output, collect error blocks attributed by
/// `file: /abs/path/to/end-user-docs/<page>?...` markers.
fn parse_mdx_errors(output: &str) -> BTreeMap<String, String> {
    let mut out = BTreeMap::new();
    let mut accumulator: Vec<&str> = Vec::new();
    for line in output.lines() {
        let trimmed = line.trim_start();
        if let Some(rest) = trimmed.strip_prefix("file:") {
            let path = rest.trim().split('?').next().unwrap_or("").trim();
            if let Some(idx) = path.find("end-user-docs/") {
                let page = &path[idx + "end-user-docs/".len()..];
                if !page.is_empty() {
                    let msg = accumulator.join("\n").trim().to_string();
                    out.insert(page.to_string(), msg);
                }
            }
            accumulator.clear();
        } else {
            accumulator.push(line);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_outcome_is_clean() {
        let o = VerificationOutcome::default();
        assert!(!o.any_failure());
        assert!(o.page_failure("anything").is_none());
        assert!(o.failure_summary(&["x.mdx"]).is_none());
    }

    #[test]
    fn combine_output_handles_all_quadrants() {
        assert_eq!(combine_output(b"", b""), "");
        assert_eq!(combine_output(b"out\n", b""), "out\n");
        assert_eq!(combine_output(b"", b"err\n"), "err\n");
        let both = combine_output(b"out", b"err");
        assert!(both.contains("stdout:\nout"));
        assert!(both.contains("stderr:\nerr"));
    }

    #[test]
    fn parses_broken_links_per_page() {
        let output = "\
> workforce-ui@0.1.0 docs:verify-links
> node scripts/verify-docs-links.mjs


Found 3 broken /docs link(s):
  adapters/drive.mdx: /docs/adapters/google
  adapters/slack.mdx: /docs/skills/oauth
  adapters/drive.mdx: /docs/configuration/google-setup
";
        let map = parse_broken_links(output);
        assert_eq!(map.len(), 2);
        assert_eq!(
            map["adapters/drive.mdx"],
            vec![
                "/docs/adapters/google".to_string(),
                "/docs/configuration/google-setup".to_string()
            ]
        );
        assert_eq!(
            map["adapters/slack.mdx"],
            vec!["/docs/skills/oauth".to_string()]
        );
    }

    #[test]
    fn parses_broken_links_clean_run_yields_empty() {
        let output = "verify-docs-links: 24 pages, no broken /docs/ links.\n";
        assert!(parse_broken_links(output).is_empty());
    }

    #[test]
    fn parses_broken_links_ignores_non_docs_paths() {
        let output = "Found 1 broken /docs link(s):\n  page.mdx: http://example.com\n";
        assert!(parse_broken_links(output).is_empty());
    }

    #[test]
    fn parses_broken_links_handles_multiple_sections() {
        let output = "\
Found 1 broken /docs link(s):
  first.mdx: /docs/a

unrelated noise
Found 2 broken /docs link(s):
  second.mdx: /docs/b
  third.mdx: /docs/c
";
        let map = parse_broken_links(output);
        assert_eq!(map.len(), 3);
        assert_eq!(map["first.mdx"], vec!["/docs/a".to_string()]);
        assert_eq!(map["second.mdx"], vec!["/docs/b".to_string()]);
        assert_eq!(map["third.mdx"], vec!["/docs/c".to_string()]);
    }

    #[test]
    fn parses_mdx_errors_per_file() {
        let output = "\
[fumadocs-mdx] [MDX] invalid frontmatter in /repo/end-user-docs/adapters/slack.mdx:
- title: Invalid input: expected string, received undefined
file: /repo/end-user-docs/adapters/slack.mdx?collection=docs
[fumadocs-mdx] Unexpected character `!` (U+0021) before name
file: /repo/end-user-docs/concepts/agents.mdx?collection=docs
";
        let map = parse_mdx_errors(output);
        assert_eq!(map.len(), 2);
        assert!(map["adapters/slack.mdx"].contains("invalid frontmatter"));
        assert!(map["adapters/slack.mdx"].contains("title: Invalid input"));
        assert!(map["concepts/agents.mdx"].contains("Unexpected character"));
    }

    #[test]
    fn parses_mdx_errors_clean_run_yields_empty() {
        let output = "[MDX] generated files in 0.8ms\n";
        assert!(parse_mdx_errors(output).is_empty());
    }

    #[test]
    fn parses_vite_build_body_error_to_page() {
        // The real failure class that `npx fumadocs-mdx` alone missed:
        // an unescaped `<` before a digit only blows up at `vite build`.
        let output = "\
vite v7.3.3 building client environment for production...
[MDX] generated files in 2.4ms
transforming...
error during build:
[fumadocs-mdx] Unexpected character `2` (U+0032) before name, expected a character that can start a name
file: /home/runner/_work/workforce/workforce/end-user-docs/reference/agent-types.mdx?collection=docs
";
        let map = parse_mdx_errors(output);
        assert_eq!(map.len(), 1);
        assert!(map["reference/agent-types.mdx"].contains("Unexpected character"));
    }

    #[test]
    fn parses_mdx_errors_ignores_tsc_failure_with_no_docs_marker() {
        // A `tsc` error unrelated to docs has no `end-user-docs/<page>`
        // marker, so it must attribute to no page (no false flagging).
        let output = "\
src/app.tsx(12,7): error TS2322: Type 'string' is not assignable to type 'number'.
error during build: tsc -b failed
";
        assert!(parse_mdx_errors(output).is_empty());
    }

    #[test]
    fn page_failure_returns_none_for_clean_page() {
        let mut o = VerificationOutcome::default();
        o.broken_links_by_page
            .insert("other.mdx".into(), vec!["/docs/x".into()]);
        assert!(o.page_failure("clean.mdx").is_none());
    }

    #[test]
    fn page_failure_combines_broken_links_and_mdx_error() {
        let mut o = VerificationOutcome::default();
        o.broken_links_by_page.insert(
            "p.mdx".into(),
            vec!["/docs/missing".into(), "/docs/other".into()],
        );
        o.mdx_errors_by_page
            .insert("p.mdx".into(), "title missing".into());
        let note = o.page_failure("p.mdx").unwrap();
        assert!(note.contains("MDX error:"));
        assert!(note.contains("title missing"));
        assert!(note.contains("Broken /docs links:"));
        assert!(note.contains("/docs/missing"));
        assert!(note.contains("/docs/other"));
    }

    #[test]
    fn any_failure_reflects_either_map() {
        let mut o = VerificationOutcome::default();
        assert!(!o.any_failure());
        o.broken_links_by_page.insert("p.mdx".into(), vec![]);
        assert!(o.any_failure());
        let mut o2 = VerificationOutcome::default();
        o2.mdx_errors_by_page.insert("p.mdx".into(), "x".into());
        assert!(o2.any_failure());
    }

    #[test]
    fn build_failure_without_page_attribution_still_fails() {
        // The cascade case: vite stops at the first bad page, so a later
        // broken page (or a good page in a batch with a pre-existing broken
        // page) has no marker. build_failed is the only safe signal.
        let mut o = VerificationOutcome {
            build_failed: true,
            ..Default::default()
        };
        assert!(o.any_failure());
        assert!(o.page_failure("untouched.mdx").is_none()); // no per-page note…
        o.build_failed = false;
        assert!(!o.any_failure());
    }

    #[test]
    fn failure_summary_groups_by_page() {
        let mut o = VerificationOutcome::default();
        o.broken_links_by_page
            .insert("a.mdx".into(), vec!["/docs/x".into()]);
        o.mdx_errors_by_page
            .insert("b.mdx".into(), "title missing".into());
        let pages = ["a.mdx", "b.mdx", "c.mdx"];
        let summary = o.failure_summary(&pages).unwrap();
        assert!(summary.contains("### a.mdx"));
        assert!(summary.contains("### b.mdx"));
        assert!(!summary.contains("### c.mdx"));
    }

    #[test]
    fn failure_summary_returns_none_when_all_pages_clean() {
        let o = VerificationOutcome::default();
        assert!(o.failure_summary(&["a.mdx", "b.mdx"]).is_none());
    }
}
