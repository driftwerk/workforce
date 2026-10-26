//! What the curator may look things up in: the repository, read-only, plus
//! any generated source the page reads from (the CLI's own help). Every
//! change the curator makes cites a quote from here, and the quote must be
//! there, word for word.

use std::path::{Path, PathBuf};

use tokio::process::Command;

/// Lines returned by one `read_source` call at most.
const MAX_READ_LINES: usize = 300;
/// Matching lines returned by one `search_sources` call at most.
const MAX_SEARCH_LINES: usize = 80;
/// Lines of the real file shown either side of a cited quote.
const QUOTE_CONTEXT_LINES: usize = 12;

pub struct SourceIndex {
    repo_root: PathBuf,
    /// Generated sources by the name the curator cites them with.
    generated: Vec<(String, String)>,
}

impl SourceIndex {
    pub fn new(repo_root: &Path, generated: Vec<(String, String)>) -> Self {
        Self {
            repo_root: repo_root.to_path_buf(),
            generated,
        }
    }

    fn generated(&self, path: &str) -> Option<&str> {
        self.generated
            .iter()
            .find(|(name, _)| name == path)
            .map(|(_, content)| content.as_str())
    }

    /// A file of the repository, by its repo-relative path. Paths that leave
    /// the repository, or reach into `.git` or build output, are refused.
    fn file(&self, path: &str) -> Result<String, String> {
        if let Some(content) = self.generated(path) {
            return Ok(content.to_string());
        }
        let relative = Path::new(path);
        let escapes = relative.is_absolute()
            || relative.components().any(|c| {
                matches!(
                    c,
                    std::path::Component::ParentDir | std::path::Component::Prefix(_)
                )
            });
        let first = relative
            .components()
            .next()
            .map(|c| c.as_os_str().to_string_lossy());
        if escapes || matches!(first.as_deref(), Some(".git" | "target" | "node_modules")) {
            return Err(format!("`{path}` is outside the sources you may read"));
        }
        std::fs::read_to_string(self.repo_root.join(relative))
            .map_err(|e| format!("can't read `{path}`: {e}"))
    }

    /// Lines `start..=end` (1-based) of a file, numbered, at most
    /// [`MAX_READ_LINES`] of them.
    pub fn read(
        &self,
        path: &str,
        start: Option<usize>,
        end: Option<usize>,
    ) -> Result<String, String> {
        let content = self.file(path)?;
        let lines: Vec<&str> = content.lines().collect();
        let start = start.unwrap_or(1).max(1);
        if start > lines.len() {
            return Err(format!("`{path}` has {} lines", lines.len()));
        }
        let end = end
            .unwrap_or(lines.len())
            .min(lines.len())
            .min(start + MAX_READ_LINES - 1);
        let mut out = format!("{path} lines {start}–{end} of {}:\n", lines.len());
        for (n, line) in lines[start - 1..end].iter().enumerate() {
            out.push_str(&format!("{:>6}  {line}\n", start + n));
        }
        Ok(out)
    }

    /// Lines matching `pattern` (an extended regular expression), across
    /// the repository's tracked files or those matching `path_glob`, and
    /// across the generated sources.
    pub async fn search(&self, pattern: &str, path_glob: Option<&str>) -> Result<String, String> {
        let mut found: Vec<String> = Vec::new();
        let regex = regex::Regex::new(pattern).map_err(|e| format!("bad pattern: {e}"))?;
        for (name, content) in &self.generated {
            if path_glob.is_some_and(|g| g != name) {
                continue;
            }
            for (n, line) in content.lines().enumerate() {
                if regex.is_match(line) {
                    found.push(format!("{name}:{}: {line}", n + 1));
                }
            }
        }
        let mut args = vec!["grep", "-n", "-I", "-E", "-e", pattern, "--"];
        if let Some(glob) = path_glob {
            args.push(glob);
        }
        let out = Command::new("git")
            .args(&args)
            .current_dir(&self.repo_root)
            .output()
            .await
            .map_err(|e| format!("search failed: {e}"))?;
        // `git grep` exits 1 when nothing matches.
        if !out.status.success() && out.status.code() != Some(1) {
            return Err(format!(
                "search failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            ));
        }
        found.extend(
            String::from_utf8_lossy(&out.stdout)
                .lines()
                .map(str::to_string),
        );
        if found.is_empty() {
            return Ok(format!("no line matches `{pattern}`"));
        }
        let total = found.len();
        let mut text: String = found
            .into_iter()
            .take(MAX_SEARCH_LINES)
            .map(|l| l.chars().take(300).collect::<String>() + "\n")
            .collect();
        if total > MAX_SEARCH_LINES {
            text.push_str(&format!(
                "… {} more; narrow the pattern or the path\n",
                total - MAX_SEARCH_LINES
            ));
        }
        Ok(text)
    }

    /// Where `quote` stands in `path`, with the lines around it, or why it
    /// isn't there. Whitespace runs compare equal, so a quote may span lines
    /// and differ in indentation; nothing else may differ.
    pub fn cited(&self, path: &str, quote: &str) -> Result<String, String> {
        let content = self.file(path)?;
        let wanted = normalise(quote);
        if wanted.is_empty() {
            return Err("the quote is empty".to_string());
        }
        let lines: Vec<&str> = content.lines().collect();
        let (normalised, line_of) = normalise_with_lines(&content);
        let at = normalised.find(&wanted).ok_or_else(|| {
            format!("`{path}` doesn't contain the quote; copy it exactly from the file")
        })?;
        let first = line_of[at];
        let last = line_of[(at + wanted.len()).saturating_sub(1).min(line_of.len() - 1)];
        let from = first.saturating_sub(QUOTE_CONTEXT_LINES);
        let to = (last + QUOTE_CONTEXT_LINES + 1).min(lines.len());
        let mut out = format!("{path} lines {}–{}:\n", from + 1, to);
        for (n, line) in lines[from..to].iter().enumerate() {
            out.push_str(&format!("{:>6}  {line}\n", from + n + 1));
        }
        Ok(out)
    }
}

fn normalise(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

/// `text` with whitespace runs collapsed to one space, and for each byte of
/// the result the 0-based line of `text` it came from.
fn normalise_with_lines(text: &str) -> (String, Vec<usize>) {
    let mut out = String::with_capacity(text.len());
    let mut line_of = Vec::with_capacity(text.len());
    let mut line = 0;
    let mut pending_space: Option<usize> = None;
    for c in text.chars() {
        if c.is_whitespace() {
            if !out.is_empty() && pending_space.is_none() {
                pending_space = Some(line);
            }
            if c == '\n' {
                line += 1;
            }
            continue;
        }
        if let Some(space_line) = pending_space.take() {
            out.push(' ');
            line_of.push(space_line);
        }
        let start = out.len();
        out.push(c);
        line_of.extend(std::iter::repeat_n(line, out.len() - start));
    }
    (out, line_of)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index() -> (tempfile::TempDir, SourceIndex) {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(dir.path().join("src")).unwrap();
        std::fs::write(
            dir.path().join("src/authz.rs"),
            "pub enum Capability {\n    /// Manage agents.\n    AgentsManage,\n    AgentsRead,\n}\n",
        )
        .unwrap();
        let index = SourceIndex::new(
            dir.path(),
            vec![(
                "workforce --help".to_string(),
                "## workforce shadow promote\n\nUsage: workforce shadow promote <STORE>\n"
                    .to_string(),
            )],
        );
        (dir, index)
    }

    #[test]
    fn a_quote_must_be_in_the_file_word_for_word() {
        let (_dir, index) = index();
        let excerpt = index
            .cited("src/authz.rs", "/// Manage agents.\n AgentsManage,")
            .unwrap();
        assert!(excerpt.contains("     3      AgentsManage,"), "{excerpt}");
        assert!(index
            .cited("src/authz.rs", "Agent.manage")
            .unwrap_err()
            .contains("doesn't contain"));
        assert!(index.cited("src/authz.rs", "  ").is_err());
    }

    #[test]
    fn a_generated_source_can_be_cited_and_read() {
        let (_dir, index) = index();
        index
            .cited(
                "workforce --help",
                "Usage: workforce shadow promote <STORE>",
            )
            .unwrap();
        assert!(index
            .read("workforce --help", Some(3), Some(3))
            .unwrap()
            .contains("<STORE>"));
    }

    #[tokio::test]
    async fn search_covers_tracked_files_and_generated_sources() {
        let (dir, index) = index();
        let git = |args: &[&str]| {
            std::process::Command::new("git")
                .args(args)
                .current_dir(dir.path())
                .output()
                .unwrap()
        };
        git(&["init", "-q"]);
        git(&["add", "src/authz.rs"]);
        let found = index
            .search("AgentsManage|shadow promote", None)
            .await
            .unwrap();
        assert!(found.contains("src/authz.rs:3:"), "{found}");
        assert!(found.contains("workforce --help:3:"), "{found}");
        let none = index.search("Agent\\.manage", None).await.unwrap();
        assert!(none.starts_with("no line matches"), "{none}");
    }

    #[test]
    fn reads_stay_inside_the_repository() {
        let (_dir, index) = index();
        for path in ["../etc/passwd", "/etc/passwd", ".git/config", "target/x"] {
            assert!(index.read(path, None, None).is_err(), "{path}");
        }
        let read = index.read("src/authz.rs", Some(2), Some(3)).unwrap();
        assert!(read.contains("     2      /// Manage agents."), "{read}");
        assert!(!read.contains("AgentsRead"));
    }
}
