use std::path::{Path, PathBuf};
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;

use super::config_guard::{self, WriteError};
use super::{Tool, ToolResult};

use super::paths::{
    is_within_roots as check_allowed, refusal, resolve_against_root as resolve_against_workspace,
    CanonicalRoots,
};

/// Resolve `path` against `allowed_paths` and return it only when it stays
/// inside them — the shared "open a file in the sandbox, or refuse" check the
/// read tools use. `None` on escape. Lets callers outside this module (e.g. the
/// hub's PDF `repo.read`) apply the same containment without re-deriving it.
pub use super::paths::resolve_within;

/// Tool for reading file contents.
pub struct ReadFile {
    allowed_paths: Vec<PathBuf>,
}

impl ReadFile {
    pub fn new(allowed_paths: Vec<PathBuf>) -> Self {
        Self { allowed_paths }
    }

    fn resolve(&self, path: &Path) -> PathBuf {
        resolve_against_workspace(path, &self.allowed_paths)
    }

    fn is_path_allowed(&self, path: &Path) -> bool {
        check_allowed(path, &self.allowed_paths)
    }
}

#[derive(Deserialize)]
struct ReadFileInput {
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &str {
        "filesystem.read"
    }

    fn description(&self) -> &str {
        "Read the contents of a file. Supports offset and limit for large files."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to read" },
                "offset": { "type": "integer", "description": "Line number to start reading from (0-indexed)" },
                "limit": { "type": "integer", "description": "Maximum number of lines to read" }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: ReadFileInput =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid input: {}", e))?;

        let resolved = self.resolve(Path::new(&input.path));
        if !self.is_path_allowed(&resolved) {
            return Ok(ToolResult::failure(
                refusal(&input.path, &resolved),
                start.elapsed().as_millis() as u64,
            ));
        }

        match tokio::fs::read_to_string(&resolved).await {
            Ok(content) => {
                let lines: Vec<&str> = content.lines().collect();
                let offset = input.offset.unwrap_or(0);
                let limit = input.limit.unwrap_or(lines.len());
                let selected: Vec<&str> = lines.into_iter().skip(offset).take(limit).collect();
                let result = selected.join("\n");

                Ok(ToolResult::success(
                    serde_json::json!({
                        "content": result,
                        "path": input.path,
                        "lines": selected.len(),
                    }),
                    start.elapsed().as_millis() as u64,
                ))
            }
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to read '{}': {}", input.path, e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

/// Tool for writing file contents.
pub struct WriteFile {
    allowed_paths: Vec<PathBuf>,
}

impl WriteFile {
    pub fn new(allowed_paths: Vec<PathBuf>) -> Self {
        Self { allowed_paths }
    }

    /// The real location of `path` inside the roots, which is what is
    /// written. The shared check folds a not-yet-created tail onto the
    /// deepest existing ancestor, so `a/b/../../../etc/x` is refused whether
    /// or not `a` exists.
    fn resolve(&self, roots: &CanonicalRoots, path: &Path) -> Option<PathBuf> {
        roots.resolve(&resolve_against_workspace(path, &self.allowed_paths))
    }
}

#[derive(Deserialize)]
struct WriteFileInput {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &str {
        "filesystem.write"
    }

    fn description(&self) -> &str {
        "Write content to file, creates parent dirs if needed"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to write to" },
                "content": { "type": "string", "description": "Content to write" }
            },
            "required": ["path", "content"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let WriteFileInput { path, content } =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid input: {}", e))?;

        let roots = CanonicalRoots::new(&self.allowed_paths);
        let Some(real) = self.resolve(&roots, Path::new(&path)) else {
            return Ok(ToolResult::failure(
                refusal(
                    &path,
                    &resolve_against_workspace(Path::new(&path), &self.allowed_paths),
                ),
                start.elapsed().as_millis() as u64,
            ));
        };

        let bytes_written = content.len();
        let written = tokio::task::spawn_blocking(move || {
            if let Some(parent) = real.parent() {
                config_guard::create_dirs(&roots, parent)?;
            }
            config_guard::write(&roots, &real, content.as_bytes())
        })
        .await
        .map_err(|e| eyre::eyre!("write task failed: {e}"))?;
        match written {
            Ok(()) => Ok(ToolResult::success(
                serde_json::json!({
                    "path": path,
                    "bytes_written": bytes_written,
                }),
                start.elapsed().as_millis() as u64,
            )),
            Err(WriteError::Config) => Ok(ToolResult::failure(
                WriteError::Config.to_string(),
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to write '{}': {}", path, e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

/// Tool for surgical text edits (find and replace).
pub struct EditFile {
    allowed_paths: Vec<PathBuf>,
}

impl EditFile {
    pub fn new(allowed_paths: Vec<PathBuf>) -> Self {
        Self { allowed_paths }
    }

    fn resolve(&self, roots: &CanonicalRoots, path: &Path) -> Option<PathBuf> {
        roots.resolve(&resolve_against_workspace(path, &self.allowed_paths))
    }
}

#[derive(Deserialize)]
struct EditFileInput {
    path: String,
    old_string: String,
    new_string: String,
}

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &str {
        "filesystem.edit"
    }

    fn description(&self) -> &str {
        "Edit file by exact string find-and-replace"
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Path to the file to edit" },
                "old_string": { "type": "string", "description": "Exact text to find" },
                "new_string": { "type": "string", "description": "Replacement text" }
            },
            "required": ["path", "old_string", "new_string"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: EditFileInput =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid input: {}", e))?;

        let roots = CanonicalRoots::new(&self.allowed_paths);
        let Some(resolved) = self.resolve(&roots, Path::new(&input.path)) else {
            return Ok(ToolResult::failure(
                refusal(
                    &input.path,
                    &resolve_against_workspace(Path::new(&input.path), &self.allowed_paths),
                ),
                start.elapsed().as_millis() as u64,
            ));
        };
        if let Err(refused) = config_guard::check(&roots, &resolved) {
            return Ok(ToolResult::failure(
                refused.to_string(),
                start.elapsed().as_millis() as u64,
            ));
        }

        let content = match tokio::fs::read_to_string(&resolved).await {
            Ok(c) => c,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Failed to read '{}': {}", input.path, e),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        let occurrences = content.matches(&input.old_string).count();
        if occurrences == 0 {
            return Ok(ToolResult::failure(
                format!("old_string not found in '{}'", input.path),
                start.elapsed().as_millis() as u64,
            ));
        }
        if occurrences > 1 {
            return Ok(ToolResult::failure(
                format!(
                    "old_string found {} times in '{}' — must be unique for safe editing",
                    occurrences, input.path
                ),
                start.elapsed().as_millis() as u64,
            ));
        }

        let new_content = content.replacen(&input.old_string, &input.new_string, 1);
        let written = tokio::task::spawn_blocking(move || {
            config_guard::write(&roots, &resolved, new_content.as_bytes())
        })
        .await
        .map_err(|e| eyre::eyre!("write task failed: {e}"))?;
        match written {
            Ok(()) => Ok(ToolResult::success(
                serde_json::json!({
                    "path": input.path,
                    "replacements": 1,
                }),
                start.elapsed().as_millis() as u64,
            )),
            Err(WriteError::Config) => Ok(ToolResult::failure(
                WriteError::Config.to_string(),
                start.elapsed().as_millis() as u64,
            )),
            Err(e) => Ok(ToolResult::failure(
                format!("Failed to write '{}': {}", input.path, e),
                start.elapsed().as_millis() as u64,
            )),
        }
    }
}

/// Tool for listing directory contents.
pub struct ListDir {
    allowed_paths: Vec<PathBuf>,
}

impl ListDir {
    pub fn new(allowed_paths: Vec<PathBuf>) -> Self {
        Self { allowed_paths }
    }

    fn resolve(&self, path: &Path) -> PathBuf {
        resolve_against_workspace(path, &self.allowed_paths)
    }

    fn is_path_allowed(&self, path: &Path) -> bool {
        check_allowed(path, &self.allowed_paths)
    }
}

#[derive(Deserialize)]
struct ListDirInput {
    path: String,
}

#[async_trait]
impl Tool for ListDir {
    fn name(&self) -> &str {
        "filesystem.list"
    }

    fn description(&self) -> &str {
        "List the contents of a directory."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "path": { "type": "string", "description": "Directory path to list" }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: ListDirInput =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid input: {}", e))?;

        let resolved = self.resolve(Path::new(&input.path));
        if !self.is_path_allowed(&resolved) {
            return Ok(ToolResult::failure(
                refusal(&input.path, &resolved),
                start.elapsed().as_millis() as u64,
            ));
        }

        let mut entries = Vec::new();
        let mut dir = match tokio::fs::read_dir(&resolved).await {
            Ok(d) => d,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Failed to list '{}': {}", input.path, e),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };

        while let Some(entry) = dir.next_entry().await? {
            let metadata = entry.metadata().await?;
            entries.push(serde_json::json!({
                "name": entry.file_name().to_string_lossy(),
                "type": if metadata.is_dir() { "dir" } else { "file" },
                "size": metadata.len(),
            }));
        }

        entries.sort_by(|a, b| {
            let a_type = a["type"].as_str().unwrap_or("");
            let b_type = b["type"].as_str().unwrap_or("");
            let a_name = a["name"].as_str().unwrap_or("");
            let b_name = b["name"].as_str().unwrap_or("");
            a_type.cmp(b_type).then(a_name.cmp(b_name))
        });

        Ok(ToolResult::success(
            serde_json::json!({
                "path": input.path,
                "entries": entries,
                "count": entries.len(),
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

/// Tool for searching file contents by regex across the workspace.
pub struct Grep {
    allowed_paths: Vec<PathBuf>,
}

impl Grep {
    pub fn new(allowed_paths: Vec<PathBuf>) -> Self {
        Self {
            allowed_paths: canonicalize_allowed(allowed_paths),
        }
    }
}

#[derive(Deserialize)]
struct GrepInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    glob: Option<String>,
    #[serde(default)]
    case_insensitive: Option<bool>,
    #[serde(default)]
    max_matches: Option<usize>,
}

const GREP_DEFAULT_MAX_MATCHES: usize = 200;

#[async_trait]
impl Tool for Grep {
    fn name(&self) -> &str {
        "filesystem.grep"
    }

    fn description(&self) -> &str {
        "Search file contents in the workspace by regex. Respects .gitignore, \
         skips binary files. Returns a list of {path, line_number, line} matches."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Regex pattern to search for" },
                "path": { "type": "string", "description": "Subdirectory under the workspace to search (default: workspace root)" },
                "glob": { "type": "string", "description": "File-name glob to filter (e.g. '**/*.rs')" },
                "case_insensitive": { "type": "boolean", "description": "Match case-insensitively" },
                "max_matches": { "type": "integer", "description": "Cap on total matches (default 200)" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: GrepInput =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid input: {}", e))?;
        let root = self.resolve_root(input.path.as_deref());
        if !check_allowed(&root, &self.allowed_paths) {
            return Ok(ToolResult::failure(
                refusal(&root.display().to_string(), &root),
                start.elapsed().as_millis() as u64,
            ));
        }
        let regex = match build_regex(&input.pattern, input.case_insensitive.unwrap_or(false)) {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Invalid regex: {e}"),
                    start.elapsed().as_millis() as u64,
                ));
            }
        };
        let limit = input.max_matches.unwrap_or(GREP_DEFAULT_MAX_MATCHES);
        let allowed = self.allowed_paths.clone();
        let roots = CanonicalRoots::new(&allowed);
        let glob = input.glob.clone();
        let elapsed_root = root.clone();
        let (matches, truncated) = tokio::task::spawn_blocking(move || {
            grep_walk(
                &elapsed_root,
                &allowed,
                &roots,
                glob.as_deref(),
                &regex,
                limit,
            )
        })
        .await
        .map_err(|e| eyre::eyre!("grep task failed: {e}"))?;
        Ok(ToolResult::success(
            serde_json::json!({
                "matches": matches,
                "count": matches.len(),
                "truncated": truncated,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

impl Grep {
    fn resolve_root(&self, requested: Option<&str>) -> PathBuf {
        let raw = requested.map(Path::new).unwrap_or(Path::new("."));
        resolve_against_workspace(raw, &self.allowed_paths)
    }
}

/// Tool for enumerating workspace files by glob pattern.
pub struct Glob {
    allowed_paths: Vec<PathBuf>,
}

impl Glob {
    pub fn new(allowed_paths: Vec<PathBuf>) -> Self {
        Self {
            allowed_paths: canonicalize_allowed(allowed_paths),
        }
    }
}

#[derive(Deserialize)]
struct GlobInput {
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
}

const GLOB_DEFAULT_MAX_RESULTS: usize = 500;

#[async_trait]
impl Tool for Glob {
    fn name(&self) -> &str {
        "filesystem.glob"
    }

    fn description(&self) -> &str {
        "List files in the workspace matching a glob pattern (e.g. '**/*.rs'). \
         Respects .gitignore. Returns sorted relative paths."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "pattern": { "type": "string", "description": "Glob pattern (e.g. '**/*.rs')" },
                "path": { "type": "string", "description": "Subdirectory under the workspace to search (default: workspace root)" },
                "max_results": { "type": "integer", "description": "Cap on total results (default 500)" }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: GlobInput =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid input: {}", e))?;
        let root = self.resolve_root(input.path.as_deref());
        if !check_allowed(&root, &self.allowed_paths) {
            return Ok(ToolResult::failure(
                refusal(&root.display().to_string(), &root),
                start.elapsed().as_millis() as u64,
            ));
        }
        let limit = input.max_results.unwrap_or(GLOB_DEFAULT_MAX_RESULTS);
        let allowed = self.allowed_paths.clone();
        let roots = CanonicalRoots::new(&allowed);
        let pattern = input.pattern.clone();
        let elapsed_root = root.clone();
        let (paths, truncated) = tokio::task::spawn_blocking(move || {
            glob_walk(&elapsed_root, &allowed, &roots, &pattern, limit)
        })
        .await
        .map_err(|e| eyre::eyre!("glob task failed: {e}"))?;
        Ok(ToolResult::success(
            serde_json::json!({
                "paths": paths,
                "count": paths.len(),
                "truncated": truncated,
            }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

impl Glob {
    fn resolve_root(&self, requested: Option<&str>) -> PathBuf {
        let raw = requested.map(Path::new).unwrap_or(Path::new("."));
        resolve_against_workspace(raw, &self.allowed_paths)
    }
}

fn canonicalize_allowed(paths: Vec<PathBuf>) -> Vec<PathBuf> {
    paths
        .into_iter()
        .map(|p| p.canonicalize().unwrap_or(p))
        .collect()
}

fn build_regex(pattern: &str, case_insensitive: bool) -> Result<regex::Regex, regex::Error> {
    regex::RegexBuilder::new(pattern)
        .case_insensitive(case_insensitive)
        .build()
}

fn build_walker(
    root: &Path,
    allowed: &[PathBuf],
    glob: Option<&str>,
) -> Result<ignore::Walk, eyre::Error> {
    let mut builder = ignore::WalkBuilder::new(root);
    builder.standard_filters(true).follow_links(false);
    for extra in allowed.iter().skip(1) {
        builder.add(extra);
    }
    if let Some(g) = glob {
        let mut overrides = ignore::overrides::OverrideBuilder::new(root);
        overrides
            .add(g)
            .map_err(|e| eyre::eyre!("Invalid glob: {e}"))?;
        builder.overrides(
            overrides
                .build()
                .map_err(|e| eyre::eyre!("Invalid glob: {e}"))?,
        );
    }
    Ok(builder.build())
}

fn grep_walk(
    root: &Path,
    allowed: &[PathBuf],
    roots: &CanonicalRoots,
    glob: Option<&str>,
    regex: &regex::Regex,
    limit: usize,
) -> (Vec<serde_json::Value>, bool) {
    use std::io::{BufRead, BufReader};
    let walker = match build_walker(root, allowed, glob) {
        Ok(w) => w,
        Err(_) => return (Vec::new(), false),
    };
    let mut matches = Vec::with_capacity(limit.min(64));
    let mut truncated = false;
    'walk: for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if !roots.contains(path) {
            continue;
        }
        let file = match std::fs::File::open(path) {
            Ok(f) => f,
            Err(_) => continue,
        };
        let mut reader = BufReader::with_capacity(8 * 1024, file);
        let peek_has_null = reader
            .fill_buf()
            .map(|buf| buf.iter().take(4096).any(|&b| b == 0))
            .unwrap_or(true);
        if peek_has_null {
            continue;
        }
        let rel = path
            .strip_prefix(root)
            .unwrap_or(path)
            .display()
            .to_string();
        for (idx, line) in reader.lines().enumerate() {
            let line = match line {
                Ok(l) => l,
                Err(_) => break,
            };
            if regex.is_match(&line) {
                if matches.len() >= limit {
                    truncated = true;
                    break 'walk;
                }
                matches.push(serde_json::json!({
                    "path": rel,
                    "line_number": idx + 1,
                    "line": line,
                }));
            }
        }
    }
    (matches, truncated)
}

fn glob_walk(
    root: &Path,
    allowed: &[PathBuf],
    roots: &CanonicalRoots,
    pattern: &str,
    limit: usize,
) -> (Vec<String>, bool) {
    let walker = match build_walker(root, allowed, Some(pattern)) {
        Ok(w) => w,
        Err(_) => return (Vec::new(), false),
    };
    let mut out = Vec::with_capacity(limit.min(64));
    let mut truncated = false;
    for entry in walker.flatten() {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        let path = entry.path();
        if !roots.contains(path) {
            continue;
        }
        if out.len() >= limit {
            truncated = true;
            break;
        }
        out.push(
            path.strip_prefix(root)
                .unwrap_or(path)
                .display()
                .to_string(),
        );
    }
    out.sort();
    (out, truncated)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[tokio::test]
    async fn read_file_works() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "line 1\nline 2\nline 3\n").unwrap();

        let tool = ReadFile::new(vec![]);
        let result = tool
            .execute(serde_json::json!({"path": file_path.to_str().unwrap()}))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output["lines"], 3);
    }

    #[tokio::test]
    async fn read_file_with_offset_and_limit() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "a\nb\nc\nd\ne\n").unwrap();

        let tool = ReadFile::new(vec![]);
        let result = tool
            .execute(serde_json::json!({
                "path": file_path.to_str().unwrap(),
                "offset": 1,
                "limit": 2
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output["content"], "b\nc");
        assert_eq!(result.output["lines"], 2);
    }

    #[tokio::test]
    async fn read_file_path_restriction() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("test.txt");
        fs::write(&file_path, "secret").unwrap();

        let tool = ReadFile::new(vec![PathBuf::from("/nonexistent/allowed")]);
        let result = tool
            .execute(serde_json::json!({"path": file_path.to_str().unwrap()}))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("Access denied"));
    }

    #[tokio::test]
    async fn write_file_works() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("output.txt");

        let tool = WriteFile::new(vec![]);
        let result = tool
            .execute(serde_json::json!({
                "path": file_path.to_str().unwrap(),
                "content": "hello world"
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "hello world");
    }

    /// A turn cannot change the agent's configuration through the file
    /// tools, by any spelling, while its own files stay writable.
    #[tokio::test]
    async fn the_file_tools_refuse_the_agents_configuration() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        fs::write(root.join("config.toml"), "[access]\nmode = \"allowlist\"\n").unwrap();
        fs::create_dir(root.join("notes")).unwrap();
        let roots = vec![root.clone()];
        let write = WriteFile::new(roots.clone());
        let edit = EditFile::new(roots);

        for path in [
            "config.toml".to_string(),
            "notes/../config.toml".to_string(),
            root.join("config.toml").display().to_string(),
            "AGENTS.md".to_string(),
            "learning/plan.md".to_string(),
        ] {
            let result = write
                .execute(
                    serde_json::json!({ "path": path, "content": "[access]\nmode = \"open\"\n" }),
                )
                .await
                .unwrap();
            assert!(!result.success, "{path} was written");
            assert_eq!(
                result.error.as_deref(),
                Some(workforce_common::config_files::WRITE_REFUSED),
                "{path}"
            );
        }
        let result = edit
            .execute(serde_json::json!({
                "path": "config.toml",
                "old_string": "allowlist",
                "new_string": "open",
            }))
            .await
            .unwrap();
        assert_eq!(
            result.error.as_deref(),
            Some(workforce_common::config_files::WRITE_REFUSED)
        );
        assert!(fs::read_to_string(root.join("config.toml"))
            .unwrap()
            .contains("allowlist"));
        assert!(!root.join("learning").exists());

        let own = write
            .execute(serde_json::json!({ "path": "notes/today.md", "content": "draft" }))
            .await
            .unwrap();
        assert!(own.success);
        let own = edit
            .execute(serde_json::json!({
                "path": "notes/today.md",
                "old_string": "draft",
                "new_string": "final",
            }))
            .await
            .unwrap();
        assert!(own.success);
        assert_eq!(
            fs::read_to_string(root.join("notes/today.md")).unwrap(),
            "final"
        );
    }

    #[tokio::test]
    async fn write_file_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("sub/dir/output.txt");

        let tool = WriteFile::new(vec![]);
        let result = tool
            .execute(serde_json::json!({
                "path": file_path.to_str().unwrap(),
                "content": "nested"
            }))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(fs::read_to_string(&file_path).unwrap(), "nested");
    }

    #[tokio::test]
    async fn edit_file_replaces_exact_match() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("code.rs");
        fs::write(&file_path, "fn old_name() {\n    println!(\"hello\");\n}\n").unwrap();

        let tool = EditFile::new(vec![]);
        let result = tool
            .execute(serde_json::json!({
                "path": file_path.to_str().unwrap(),
                "old_string": "fn old_name()",
                "new_string": "fn new_name()"
            }))
            .await
            .unwrap();

        assert!(result.success);
        let content = fs::read_to_string(&file_path).unwrap();
        assert!(content.contains("fn new_name()"));
        assert!(!content.contains("fn old_name()"));
    }

    #[tokio::test]
    async fn edit_file_rejects_ambiguous_match() {
        let dir = tempfile::tempdir().unwrap();
        let file_path = dir.path().join("code.rs");
        fs::write(&file_path, "let x = 1;\nlet x = 2;\n").unwrap();

        let tool = EditFile::new(vec![]);
        let result = tool
            .execute(serde_json::json!({
                "path": file_path.to_str().unwrap(),
                "old_string": "let x",
                "new_string": "let y"
            }))
            .await
            .unwrap();

        assert!(!result.success);
        assert!(result.error.unwrap().contains("2 times"));
    }

    #[tokio::test]
    async fn list_dir_works() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "").unwrap();
        fs::write(dir.path().join("b.txt"), "").unwrap();
        fs::create_dir(dir.path().join("subdir")).unwrap();

        let tool = ListDir::new(vec![]);
        let result = tool
            .execute(serde_json::json!({"path": dir.path().to_str().unwrap()}))
            .await
            .unwrap();

        assert!(result.success);
        assert_eq!(result.output["count"], 3);
    }

    /// A position the hub keeps for each person is outside every root the
    /// file tools are given: not read, listed, written, edited, searched or
    /// globbed, through a link either, while the rest of the workspace is.
    #[tokio::test]
    async fn a_withheld_position_is_out_of_every_file_tool_s_reach() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        fs::create_dir_all(ws.join("sessions")).unwrap();
        fs::write(ws.join("sessions/a.jsonl"), "needle from a conversation\n").unwrap();
        fs::write(ws.join("notes.md"), "needle in the open\n").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(ws.join("sessions"), ws.join("link")).unwrap();
        workforce_common::private_fs::withhold_from_tools(&ws.join("sessions"));
        let roots = vec![ws.clone()];
        let refused = |result: ToolResult| {
            assert!(!result.success, "{result:?}");
            assert!(
                result
                    .error
                    .as_deref()
                    .is_some_and(|e| e.contains(workforce_common::private_fs::TOOL_REFUSED)),
                "{result:?}"
            );
        };

        for path in ["sessions/a.jsonl", "link/a.jsonl"] {
            refused(
                ReadFile::new(roots.clone())
                    .execute(serde_json::json!({ "path": path }))
                    .await
                    .unwrap(),
            );
        }
        refused(
            ListDir::new(roots.clone())
                .execute(serde_json::json!({ "path": "sessions" }))
                .await
                .unwrap(),
        );
        refused(
            WriteFile::new(roots.clone())
                .execute(serde_json::json!({ "path": "sessions/b.jsonl", "content": "x" }))
                .await
                .unwrap(),
        );
        refused(
            EditFile::new(roots.clone())
                .execute(serde_json::json!({
                    "path": "sessions/a.jsonl",
                    "old_string": "needle",
                    "new_string": "pin",
                }))
                .await
                .unwrap(),
        );
        assert!(!ws.join("sessions/b.jsonl").exists());

        let grep = Grep::new(roots.clone())
            .execute(serde_json::json!({ "pattern": "needle" }))
            .await
            .unwrap();
        assert_eq!(grep.output["count"], 1, "{grep:?}");
        assert_eq!(grep.output["matches"][0]["path"], "notes.md");
        let glob = Glob::new(roots.clone())
            .execute(serde_json::json!({ "pattern": "**/*" }))
            .await
            .unwrap();
        assert_eq!(glob.output["paths"], serde_json::json!(["notes.md"]));
        assert!(
            ReadFile::new(roots)
                .execute(serde_json::json!({ "path": "notes.md" }))
                .await
                .unwrap()
                .success
        );
    }

    /// A person's own notes and profile stay theirs to read, write and
    /// search in their own turn; another person's, and a turn bound to
    /// nobody, are kept out.
    #[tokio::test]
    async fn a_person_s_own_notes_and_profile_are_open_to_their_own_turn() {
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().canonicalize().unwrap();
        for (file, words) in [
            ("memory/users/ada/notes.md", "needle: ada's note"),
            ("memory/users/bob/notes.md", "needle: bob's note"),
            ("users/ada.md", "needle: ada's profile"),
            ("users/bob.md", "needle: bob's profile"),
        ] {
            fs::create_dir_all(ws.join(file).parent().unwrap()).unwrap();
            fs::write(ws.join(file), words).unwrap();
        }
        workforce_common::private_fs::withhold_per_person(&ws.join("memory/users"));
        workforce_common::private_fs::withhold_per_person(&ws.join("users"));
        let roots = vec![ws.clone()];
        let reader = ReadFile::new(roots.clone());
        let read = |path: &'static str| reader.execute(serde_json::json!({ "path": path }));
        let as_ada = |fut| crate::runner::CURRENT_USER_ID.scope("ada".to_string(), fut);

        for own in ["memory/users/ada/notes.md", "users/ada.md"] {
            assert!(as_ada(read(own)).await.unwrap().success, "{own}");
            assert!(!read(own).await.unwrap().success, "{own}: bound to nobody");
        }
        for other in ["memory/users/bob/notes.md", "users/bob.md"] {
            assert!(!as_ada(read(other)).await.unwrap().success, "{other}");
        }
        let writer = WriteFile::new(roots.clone());
        let write = writer
            .execute(serde_json::json!({ "path": "memory/users/ada/todo.md", "content": "x" }));
        let written = as_ada(write).await.unwrap();
        assert!(written.success, "{written:?}");
        let grep =
            as_ada(Grep::new(roots.clone()).execute(serde_json::json!({ "pattern": "needle" })))
                .await
                .unwrap();
        let mut found: Vec<String> = grep.output["matches"]
            .as_array()
            .unwrap()
            .iter()
            .map(|m| m["path"].as_str().unwrap().to_string())
            .collect();
        found.sort();
        assert_eq!(found, ["memory/users/ada/notes.md", "users/ada.md"]);
    }

    /// A shared dated note mixes whoever spoke that day: a turn run for a
    /// person reads, lists and searches past it, a turn bound to nobody (which
    /// still writes there) keeps it, and the shared notes beside it stay open.
    #[tokio::test]
    async fn a_shared_dated_note_is_withheld_from_a_person_s_turn() {
        let dir = tempfile::tempdir().unwrap();
        let memory = dir.path().canonicalize().unwrap().join("memory");
        fs::create_dir(&memory).unwrap();
        fs::write(memory.join("2026-03-14.md"), "needle: bob said\n").unwrap();
        fs::write(memory.join("active-task.md"), "needle: shared\n").unwrap();
        workforce_common::private_fs::withhold_dated_notes(&memory);
        let roots = vec![memory.parent().unwrap().to_path_buf()];
        let reader = ReadFile::new(roots.clone());
        let read = |path: &'static str| reader.execute(serde_json::json!({ "path": path }));
        let as_ada = |fut| crate::runner::CURRENT_USER_ID.scope("ada".to_string(), fut);

        let dated = as_ada(read("memory/2026-03-14.md")).await.unwrap();
        assert!(!dated.success);
        assert!(as_ada(read("memory/active-task.md")).await.unwrap().success);
        let grep =
            as_ada(Grep::new(roots.clone()).execute(serde_json::json!({ "pattern": "needle" })))
                .await
                .unwrap();
        assert_eq!(grep.output["count"], 1, "{grep:?}");
        assert!(
            read("memory/2026-03-14.md").await.unwrap().success,
            "bound to nobody"
        );
    }

    #[tokio::test]
    async fn grep_finds_matches_with_line_numbers() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "fn foo() {}\nfn bar() {}\n").unwrap();
        fs::write(dir.path().join("b.rs"), "// noop\nfn foo() {}\n").unwrap();
        let tool = Grep::new(vec![dir.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({ "pattern": r"fn foo\(\)" }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["count"], 2);
        let matches: Vec<serde_json::Value> = result.output["matches"].as_array().unwrap().to_vec();
        assert!(matches
            .iter()
            .all(|m| m["line"].as_str().unwrap().contains("foo")));
        assert!(matches
            .iter()
            .any(|m| m["path"].as_str().unwrap().ends_with("a.rs") && m["line_number"] == 1));
    }

    #[tokio::test]
    async fn grep_respects_max_matches_truncation() {
        let dir = tempfile::tempdir().unwrap();
        let body: String = (0..50).map(|_| "needle\n").collect();
        fs::write(dir.path().join("big.txt"), body).unwrap();
        let tool = Grep::new(vec![dir.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({ "pattern": "needle", "max_matches": 10 }))
            .await
            .unwrap();
        assert_eq!(result.output["count"], 10);
        assert_eq!(result.output["truncated"], true);
    }

    #[tokio::test]
    async fn grep_rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.txt"), "x").unwrap();
        let tool = Grep::new(vec![dir.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({ "pattern": "x", "path": "/etc" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Access denied"));
    }

    #[tokio::test]
    async fn grep_invalid_regex_returns_error() {
        let dir = tempfile::tempdir().unwrap();
        let tool = Grep::new(vec![dir.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({ "pattern": "[" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Invalid regex"));
    }

    #[tokio::test]
    async fn grep_skips_binary_files() {
        let dir = tempfile::tempdir().unwrap();
        let mut bin = vec![0u8; 16];
        bin.extend_from_slice(b"needle");
        fs::write(dir.path().join("blob.bin"), bin).unwrap();
        fs::write(dir.path().join("ok.txt"), "needle\n").unwrap();
        let tool = Grep::new(vec![dir.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({ "pattern": "needle" }))
            .await
            .unwrap();
        assert_eq!(result.output["count"], 1);
        let path = result.output["matches"][0]["path"].as_str().unwrap();
        assert!(path.ends_with("ok.txt"));
    }

    #[tokio::test]
    async fn glob_matches_basic_patterns() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("a.rs"), "").unwrap();
        fs::write(dir.path().join("b.rs"), "").unwrap();
        fs::write(dir.path().join("c.txt"), "").unwrap();
        let tool = Glob::new(vec![dir.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({ "pattern": "**/*.rs" }))
            .await
            .unwrap();
        assert!(result.success);
        assert_eq!(result.output["count"], 2);
        let paths: Vec<String> = result.output["paths"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p.as_str().unwrap().to_string())
            .collect();
        assert!(paths.iter().all(|p| p.ends_with(".rs")));
    }

    #[tokio::test]
    async fn glob_rejects_path_traversal() {
        let dir = tempfile::tempdir().unwrap();
        let tool = Glob::new(vec![dir.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({ "pattern": "**/*", "path": "/etc" }))
            .await
            .unwrap();
        assert!(!result.success);
        assert!(result.error.unwrap().contains("Access denied"));
    }

    #[tokio::test]
    async fn glob_respects_max_results() {
        let dir = tempfile::tempdir().unwrap();
        for i in 0..20 {
            fs::write(dir.path().join(format!("f{i}.txt")), "").unwrap();
        }
        let tool = Glob::new(vec![dir.path().to_path_buf()]);
        let result = tool
            .execute(serde_json::json!({ "pattern": "**/*.txt", "max_results": 5 }))
            .await
            .unwrap();
        assert_eq!(result.output["count"], 5);
        assert_eq!(result.output["truncated"], true);
    }
}
