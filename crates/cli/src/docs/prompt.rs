//! Build the curator user prompt — pure function.
//!
//! The system prompt is `include_str!`'d in `mod.rs` and passed as
//! `append_system_prompt`; this module owns the per-page user prompt
//! and the helpers that compute the inputs to it.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use eyre::{Result, WrapErr};

pub struct PromptInput {
    pub page: String,
    pub current_content: Option<String>,
    pub sources: Vec<SourceFile>,
    /// Every `/docs/<slug>` path that resolves to a real MDX file in
    /// `end-user-docs/`. Injected so the curator only links to pages
    /// that exist — the prompt's "no invented links" rule is enforced
    /// by giving the model the actual allowlist.
    pub valid_doc_paths: Vec<String>,
}

#[derive(Clone)]
pub struct SourceFile {
    pub repo_relative: PathBuf,
    pub content: String,
}

pub fn read_source_files(paths: &[PathBuf], repo_root: &Path) -> Result<Vec<SourceFile>> {
    paths
        .iter()
        .map(|p| {
            let content = std::fs::read_to_string(p)
                .wrap_err_with(|| format!("read source {}", p.display()))?;
            let repo_relative = p
                .strip_prefix(repo_root)
                .map(Path::to_path_buf)
                .unwrap_or_else(|_| p.clone());
            Ok(SourceFile {
                repo_relative,
                content,
            })
        })
        .collect()
}

pub fn read_page(page_full_path: &Path) -> Result<Option<String>> {
    match std::fs::read_to_string(page_full_path) {
        Ok(content) => Ok(Some(content)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => {
            Err(eyre::Report::new(e).wrap_err(format!("read page {}", page_full_path.display())))
        }
    }
}

/// Walk `end-user-docs/` and return every valid `/docs/<slug>` path.
/// Mirrors `scripts/verify-docs-links.mjs` so what the curator sees
/// matches what verify will accept: `index.mdx` collapses to the
/// folder slug, everything else uses the full path minus `.mdx`.
pub fn collect_valid_doc_paths(docs_root: &Path) -> Result<Vec<String>> {
    let mut out: BTreeSet<String> = BTreeSet::from(["/docs".to_string()]);
    walk_mdx(docs_root, docs_root, &mut out)?;
    Ok(out.into_iter().collect())
}

fn walk_mdx(root: &Path, dir: &Path, out: &mut BTreeSet<String>) -> Result<()> {
    for entry in std::fs::read_dir(dir).wrap_err_with(|| format!("read_dir {}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        let ft = entry.file_type()?;
        if ft.is_dir() {
            walk_mdx(root, &path, out)?;
        } else if ft.is_file() {
            let name = entry.file_name();
            let s = name.to_string_lossy();
            if !s.ends_with(".mdx") && !s.ends_with(".md") {
                continue;
            }
            let rel = path
                .strip_prefix(root)
                .map_err(|_| eyre::eyre!("path {} not under root", path.display()))?
                .with_extension("");
            let rel = rel.to_string_lossy().replace('\\', "/");
            let segments: Vec<&str> = rel.split('/').filter(|s| *s != "index").collect();
            let slug = if segments.is_empty() {
                "/docs".to_string()
            } else {
                format!("/docs/{}", segments.join("/"))
            };
            out.insert(slug);
        }
    }
    Ok(())
}

pub fn build_prompt(input: &PromptInput) -> String {
    let mut p = build_prompt_header(input);
    if input.sources.is_empty() {
        p.push_str("## Source files\n(none — refuse and explain why)\n");
    } else {
        p.push_str("## Source files\n");
        append_source_blocks(&mut p, &input.sources);
    }
    p
}

/// User prompt for a page whose sources are too large to include: the page
/// and the list of its source files, which the curator looks up with its
/// tools as it needs them.
pub fn build_lookup_prompt(input: &PromptInput) -> String {
    let mut p = build_prompt_header(input);
    p.push_str(
        "## Source files\n\
         These are the page's sources. They are too large to include here: look up what \
         you need with `search_sources` and `read_source` before you write it.\n\n",
    );
    for s in &input.sources {
        p.push_str(&format!(
            "- `{}` ({} lines)\n",
            s.repo_relative.display(),
            s.content.lines().count()
        ));
    }
    p
}

fn build_prompt_header(input: &PromptInput) -> String {
    let mut p = String::with_capacity(4096);
    p.push_str(&format!("# Curate `end-user-docs/{}`\n\n", input.page));
    p.push_str(
        "Change the page with the tools, never by writing it out: `edit`\n\
         replaces one exact passage, `append` adds to the end. End with\n\
         `finish`, or call `keep_page` alone if the page is already\n\
         accurate. Apply every rule in the system prompt.\n\n",
    );
    p.push_str(&format!(
        "## Target page\n`end-user-docs/{}`\n\n",
        input.page
    ));
    match &input.current_content {
        Some(curr) => {
            // Markers, not a fence: the page has fences of its own, and
            // `edit` quotes it byte for byte.
            p.push_str(
                "## Current content of the target page\n\
                 Everything between the two marker lines, exactly as it is on disk.\n\n\
                 <<<PAGE\n",
            );
            p.push_str(curr);
            if !curr.ends_with('\n') {
                p.push('\n');
            }
            p.push_str("PAGE>>>\n\n");
        }
        None => {
            p.push_str(
                "## Current content\n(file does not exist yet — write it with `append`, \
                 a section at a time, starting with the frontmatter)\n\n",
            );
        }
    }
    p.push_str(
        "## Valid `/docs/` link targets\n\
         These are the ONLY `/docs/...` paths that exist. Any link you write\n\
         to a path not in this list will fail the build and the PR will be\n\
         marked Draft. If you want to reference a concept that has no page,\n\
         mention it in prose without a link.\n\n",
    );
    for path in &input.valid_doc_paths {
        p.push_str(&format!("- `{path}`\n"));
    }
    p.push('\n');
    p
}

fn append_source_blocks(p: &mut String, sources: &[SourceFile]) {
    for s in sources {
        p.push_str(&format!("\n### `{}`\n```\n", s.repo_relative.display()));
        p.push_str(&s.content);
        if !s.content.ends_with('\n') {
            p.push('\n');
        }
        p.push_str("```\n");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input_with_sources(sources: Vec<(&str, &str)>) -> PromptInput {
        PromptInput {
            page: "adapters/slack.mdx".to_string(),
            current_content: Some("# Slack adapter\n".to_string()),
            sources: sources
                .into_iter()
                .map(|(p, c)| SourceFile {
                    repo_relative: PathBuf::from(p),
                    content: c.to_string(),
                })
                .collect(),
            valid_doc_paths: vec!["/docs".into(), "/docs/adapters/slack".into()],
        }
    }

    #[test]
    fn includes_target_and_current_content() {
        let input = input_with_sources(vec![]);
        let prompt = build_prompt(&input);
        assert!(prompt.contains("end-user-docs/adapters/slack.mdx"));
        assert!(prompt.contains("# Slack adapter"));
    }

    #[test]
    fn embeds_source_files_with_fence_headers() {
        let input = input_with_sources(vec![
            ("crates/skills/src/adapters/slack/lib.rs", "fn main() {}"),
            ("crates/skills/src/adapters/slack/types.rs", "struct Msg;"),
        ]);
        let prompt = build_prompt(&input);
        assert!(prompt.contains("### `crates/skills/src/adapters/slack/lib.rs`"));
        assert!(prompt.contains("fn main() {}"));
        assert!(prompt.contains("### `crates/skills/src/adapters/slack/types.rs`"));
        assert!(prompt.contains("struct Msg;"));
    }

    #[test]
    fn current_page_is_quoted_between_markers_exactly() {
        let mut input = input_with_sources(vec![]);
        input.current_content = Some("# Slack\n\n```sh\nworkforce serve\n```\n".to_string());
        let prompt = build_prompt(&input);
        assert!(prompt.contains("<<<PAGE\n# Slack\n\n```sh\nworkforce serve\n```\nPAGE>>>\n"));
        assert!(
            !prompt.contains("```mdx"),
            "the page must not be wrapped in a fence"
        );
    }

    #[test]
    fn the_prompt_asks_for_edits_not_the_whole_page() {
        let prompt = build_prompt(&input_with_sources(vec![]));
        assert!(prompt.contains("never by writing it out"));
        assert!(!prompt.contains("complete MDX page"));
    }

    #[test]
    fn missing_target_file_says_does_not_exist() {
        let mut input = input_with_sources(vec![]);
        input.current_content = None;
        let prompt = build_prompt(&input);
        assert!(prompt.contains("file does not exist yet"));
    }

    #[test]
    fn no_sources_directs_refusal() {
        let input = input_with_sources(vec![]);
        let prompt = build_prompt(&input);
        assert!(prompt.contains("(none — refuse and explain why)"));
    }

    #[test]
    fn embeds_valid_doc_paths_allowlist() {
        let input = input_with_sources(vec![]);
        let prompt = build_prompt(&input);
        assert!(prompt.contains("Valid `/docs/` link targets"));
        assert!(prompt.contains("- `/docs`"));
        assert!(prompt.contains("- `/docs/adapters/slack`"));
    }

    #[test]
    fn a_lookup_prompt_lists_the_sources_instead_of_including_them() {
        let input = input_with_sources(vec![(
            "crates/skills/src/adapters/slack/lib.rs",
            "fn main() {}\nfn other() {}\n",
        )]);
        let p = build_lookup_prompt(&input);
        assert!(p.contains("- `crates/skills/src/adapters/slack/lib.rs` (2 lines)"));
        assert!(!p.contains("fn main() {}"), "sources must not be inlined");
        assert!(p.contains("search_sources"));
        assert!(
            p.contains("# Slack adapter"),
            "the page itself is still included"
        );
    }

    #[test]
    fn collect_valid_doc_paths_handles_index_and_nested() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir_all(root.join("adapters")).unwrap();
        std::fs::create_dir_all(root.join("getting-started")).unwrap();
        std::fs::write(root.join("index.mdx"), "---\ntitle: A\n---\n").unwrap();
        std::fs::write(root.join("cli.mdx"), "---\ntitle: B\n---\n").unwrap();
        std::fs::write(root.join("adapters/slack.mdx"), "---\ntitle: C\n---\n").unwrap();
        std::fs::write(
            root.join("getting-started/index.mdx"),
            "---\ntitle: D\n---\n",
        )
        .unwrap();
        std::fs::write(root.join("adapters/notes.txt"), "ignored").unwrap();

        let paths = collect_valid_doc_paths(root).unwrap();
        assert!(paths.contains(&"/docs".to_string()));
        assert!(paths.contains(&"/docs/cli".to_string()));
        assert!(paths.contains(&"/docs/adapters/slack".to_string()));
        assert!(paths.contains(&"/docs/getting-started".to_string()));
        assert!(
            !paths.iter().any(|p| p.contains("notes")),
            "non-mdx file leaked: {paths:?}",
        );
    }
}
