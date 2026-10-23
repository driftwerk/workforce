//! Live, read-only text search over the indexed repository checkouts.
//!
//! The symbol graph (`knowledge.ask` and friends) only knows code *symbols* —
//! functions, classes, types. It cannot see prose, naming conventions, config,
//! design docs, specs, comments, or any file content that isn't a named symbol.
//! That blind spot is why an agent could fail to find, say, an integration
//! file-naming convention that lives in a `README`/`docs/specs` file even though
//! the repo is fully indexed.
//!
//! These tools give the agent the move a developer makes locally: grep, read,
//! and glob the *actual current files* of an indexed repo. They are strictly
//! read-only and operate on the indexer's canonical checkout
//! (`{data_dir}/knowledge/checkouts/repos/{owner}/{repo}`), resolved by repo
//! name via the [`RepoResolver`]. They reuse the agent runtime's existing
//! ripgrep/read engine (`filesystem::{Grep, Glob, ReadFile}`) so the regex
//! syntax, gitignore handling, binary-skip and caps match `filesystem.grep`
//! exactly — no second engine to learn or diverge. Every result is stamped with
//! the checkout's current commit so the agent (and the user) can see precisely
//! which version was read — provenance that backs up grounded answers.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde::Deserialize;
use serde_json::json;

use workforce_agent::tools::filesystem::{resolve_within, Glob, Grep, ReadFile};
use workforce_agent::tools::{Tool, ToolResult};

use super::access_guard::{maybe_guard, KnowledgeAccessGuard};
use super::RepoResolver;
use crate::git_hardening::hardened_git;

/// Resolve the canonical checkout directory for `repo`. With a hub resolver,
/// `repo` names an indexed repo and resolves to its canonical checkout; without
/// one (single-agent setups) we fall back to the agent workspace, which is
/// itself the single indexed repo.
pub(crate) fn resolve_checkout(
    resolver: Option<&Arc<RepoResolver>>,
    agent_workspace: &Path,
    repo: &Option<String>,
) -> Result<(PathBuf, String), String> {
    let (dir, name) = match resolver {
        Some(r) => (
            r.resolve_dest(repo.as_deref())?,
            repo.clone().unwrap_or_else(|| "(default)".to_string()),
        ),
        None => (
            agent_workspace.to_path_buf(),
            repo.clone().unwrap_or_else(|| ".".to_string()),
        ),
    };
    // Canonicalize the checkout root. `ReadFile` does not canonicalize its
    // allowed paths, but `check_allowed` canonicalizes the resolved target, so
    // a symlink component anywhere in the path (macOS `/var`→`/private/var`,
    // some EFS mounts) would otherwise make every `repo.read` fail the
    // prefix check. Best-effort: fall back to the raw path if it can't resolve.
    let dir = std::fs::canonicalize(&dir).unwrap_or(dir);
    Ok((dir, name))
}

/// Best-effort short HEAD commit of a checkout, for provenance. Returns `None`
/// when the directory isn't a git repo or git is unavailable — provenance is
/// informative, never required, so a missing commit must not fail the search.
pub(crate) async fn head_commit(dir: &Path) -> Option<String> {
    let out = hardened_git()
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .await
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let sha = String::from_utf8_lossy(&out.stdout).trim().to_string();
    (!sha.is_empty()).then_some(sha)
}

/// Attach `repo` + `commit` provenance to a successful inner-tool result so the
/// agent can cite exactly what it read (e.g. `core-api@6385b08`).
fn stamp(mut result: ToolResult, repo: &str, commit: &Option<String>) -> ToolResult {
    if result.success {
        if let serde_json::Value::Object(map) = &mut result.output {
            map.insert("repo".to_string(), json!(repo));
            if let Some(c) = commit {
                map.insert("commit".to_string(), json!(c));
            }
        }
    }
    result
}

/// Cap on extracted PDF text returned inline (~100k tokens). Specs run long;
/// beyond this we truncate and flag it so the agent can page with offset/limit.
const MAX_PDF_CHARS: usize = 400_000;

/// True when `path` names a PDF — extracted rather than read, since a UTF-8
/// read of a binary PDF returns garbage.
fn is_pdf(path: &str) -> bool {
    Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| e.eq_ignore_ascii_case("pdf"))
        .unwrap_or(false)
}

/// Read a PDF inside `dir` and return its extracted text, with the same
/// containment and `offset`/`limit` line slicing as `filesystem::ReadFile`.
/// Extraction runs on a blocking thread: `pdf_extract` is sync, CPU-bound, and
/// can panic on malformed input — `spawn_blocking` isolates that into a clean
/// tool failure instead of taking down the hub.
async fn read_pdf(
    dir: &Path,
    rel: &str,
    offset: Option<usize>,
    limit: Option<usize>,
    start: Instant,
) -> ToolResult {
    let elapsed = |s: Instant| s.elapsed().as_millis() as u64;
    // Same containment as every other read tool (`filesystem::ReadFile` et al.).
    let allowed = [dir.to_path_buf()];
    let Some(path) = resolve_within(Path::new(rel), &allowed) else {
        return ToolResult::failure(
            format!("Access denied: path '{rel}' is outside allowed directories"),
            elapsed(start),
        );
    };
    let extracted = tokio::task::spawn_blocking(move || pdf_extract::extract_text(&path)).await;
    let text = match extracted {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            return ToolResult::failure(
                format!("Failed to extract PDF text from '{rel}': {e}"),
                elapsed(start),
            )
        }
        // spawn_blocking returns Err on a panic in `pdf_extract` — treat as a
        // parse failure (commonly a scanned-image PDF with no text layer).
        Err(_) => {
            return ToolResult::failure(
                format!(
                    "Could not parse PDF '{rel}' — it may be malformed or a scanned image with no \
                     extractable text layer."
                ),
                elapsed(start),
            )
        }
    };
    let lines: Vec<&str> = text.lines().collect();
    let total = lines.len();
    let offset = offset.unwrap_or(0);
    let limit = limit.unwrap_or(total);
    let selected: Vec<&str> = lines.into_iter().skip(offset).take(limit).collect();
    let mut content = selected.join("\n");
    let mut truncated = false;
    if content.len() > MAX_PDF_CHARS {
        // Truncate on a char boundary — `String::truncate` panics otherwise.
        let mut end = MAX_PDF_CHARS;
        while end > 0 && !content.is_char_boundary(end) {
            end -= 1;
        }
        content.truncate(end);
        truncated = true;
    }
    ToolResult::success(
        json!({
            "content": content,
            "path": rel,
            "lines": selected.len(),
            "total_lines": total,
            "extracted_from": "pdf",
            "truncated": truncated,
        }),
        elapsed(start),
    )
}

/// The shared top-level directory of a set of **repo-relative** hit paths — used
/// to scope the nearby-docs manifest to the module/service the agent is actually
/// searching (e.g. all hits under `esr-integration/…` → scope `esr-integration`,
/// whose `docs/specs/*.pdf` then surface). `None` when the hits span multiple
/// top-level dirs or sit at the repo root, in which case the manifest falls back
/// to whole-repo, high-signal docs.
///
/// Paths MUST be repo-relative. `repo.grep` matches are relative to the *searched
/// subdir* when `path` is given, so the caller re-joins `path` first — otherwise
/// the scope is computed off `src/…` and points nowhere.
fn common_top_dir<'a>(paths: impl IntoIterator<Item = &'a str>) -> Option<String> {
    let mut common: Option<String> = None;
    for path in paths {
        // A root-level file (no '/') gives no useful scope.
        let (seg, _) = path.split_once('/')?;
        match &common {
            None => common = Some(seg.to_string()),
            Some(c) if c != seg => return None,
            _ => {}
        }
    }
    common
}

/// Best-effort manifest of the authoritative **docs** under `scope` (a
/// repo-relative subdirectory, or the whole repo when `None`).
///
/// The symbol graph sees code symbols; it does not see the prose where teams
/// state intent — design docs, integration specs, naming conventions, READMEs.
/// When the agent satisfices on a plausible code answer it never reads them, and
/// loses to anyone who just opens the doc. This surfaces those docs on the path
/// the agent is already on (`repo.grep` results, `knowledge.ask` answers), with
/// the filenames in front of it — they often name the answer outright — and the
/// full text one `repo.read` away (PDFs are extracted to text). General across
/// formats (`.pdf`/`.md`/`.adoc`/`.rst`/`README`) and repos.
///
/// Returns `None` when the repo can't be resolved or no docs are found; never
/// fails the caller; bounded by a timeout so a large checkout can't stall it.
pub(crate) async fn nearby_docs_manifest(
    resolver: Option<&Arc<RepoResolver>>,
    agent_workspace: &Path,
    repo: &Option<String>,
    scope: Option<&str>,
) -> Option<String> {
    let (dir, _name) = resolve_checkout(resolver, agent_workspace, repo).ok()?;
    let globber = Glob::new(vec![dir]);
    // Scoped to a subtree (a grep landed there) → list the full doc set, it's
    // small and relevant. Whole-repo (no scope) → high-signal only (PDFs +
    // READMEs), or a big monorepo would flood the result with every `.md`.
    let patterns: &[&str] = if scope.is_some() {
        &["**/*.pdf", "**/*.md", "**/*.adoc", "**/*.rst", "**/README*"]
    } else {
        &["**/*.pdf", "**/README*"]
    };
    let mut seen = std::collections::BTreeSet::<String>::new();
    let gather = async {
        for pat in patterns {
            let mut input = serde_json::Map::new();
            input.insert("pattern".to_string(), json!(pat));
            input.insert("max_results".to_string(), json!(40));
            if let Some(s) = scope {
                input.insert("path".to_string(), json!(s));
            }
            if let Ok(res) = globber.execute(serde_json::Value::Object(input)).await {
                if let Some(arr) = res.output.get("paths").and_then(|p| p.as_array()) {
                    for p in arr.iter().filter_map(|p| p.as_str()) {
                        // Glob strips the scoped root, so re-prefix the scope to
                        // keep every listed path repo-relative and `repo.read`-able.
                        seen.insert(match scope {
                            Some(s) => format!("{s}/{p}"),
                            None => p.to_string(),
                        });
                    }
                }
            }
        }
    };
    // Best-effort + bounded: a timeout just means whatever we gathered so far.
    let _ = tokio::time::timeout(std::time::Duration::from_secs(3), gather).await;
    if seen.is_empty() {
        return None;
    }
    const SHOW: usize = 25;
    let mut paths: Vec<&String> = seen.iter().collect();
    // PDFs first: they're the binary blind spot neither the symbol graph nor a
    // code grep can read, so they must never be the entries dropped by the cap.
    paths.sort_by_key(|p| (!p.to_ascii_lowercase().ends_with(".pdf"), p.as_str()));
    let shown = paths
        .iter()
        .take(SHOW)
        .map(|p| format!("- {p}"))
        .collect::<Vec<_>>()
        .join("\n");
    let more = if paths.len() > SHOW {
        format!("\n…and {} more", paths.len() - SHOW)
    } else {
        String::new()
    };
    let scope_note = scope.map(|s| format!(" under `{s}/`")).unwrap_or_default();
    Some(format!(
        "Authoritative docs{scope_note} the symbol index doesn't capture — design docs, specs, \
         READMEs (PDFs included; `repo.read` extracts their text). Intent, conventions, file \
         formats and \"how it works\" are stated here, and the filename alone often names the \
         answer. If your question is about how something works or is named, read the relevant one \
         with `repo.read` before answering from code alone:\n{shown}{more}"
    ))
}

// ─── repo.grep ──────────────────────────────────────────────────────────────

/// Search the live file content of an indexed repo by regex.
pub struct RepoGrepTool {
    agent_workspace: PathBuf,
    resolver: Option<Arc<RepoResolver>>,
    access_guard: Option<KnowledgeAccessGuard>,
}

#[derive(Deserialize)]
struct RepoGrepInput {
    #[serde(default)]
    repo: Option<String>,
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

impl RepoGrepTool {
    pub fn new(agent_workspace: PathBuf) -> Self {
        Self {
            agent_workspace,
            resolver: None,
            access_guard: None,
        }
    }

    pub fn with_resolver(mut self, resolver: Arc<RepoResolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }

    pub fn with_access_guard(mut self, guard: KnowledgeAccessGuard) -> Self {
        self.access_guard = Some(guard);
        self
    }
}

#[async_trait]
impl Tool for RepoGrepTool {
    fn name(&self) -> &str {
        "repo.grep"
    }

    fn description(&self) -> &str {
        "Search the ACTUAL CURRENT FILE CONTENT of an indexed repository by regex — the \
         live-grep a developer would run. Unlike `knowledge.ask` (which only knows code \
         symbols — functions, classes, types), this sees everything in the files: prose, \
         READMEs, docs/specs, naming conventions, config, comments, test fixtures. Reach for \
         it whenever the answer is text rather than a named symbol, or whenever `knowledge.ask` \
         comes back empty. Returns {path, line_number, line} matches, stamped with the repo's \
         current commit, plus `docs_nearby`: the authoritative docs/specs/READMEs sitting next to \
         your hits (often where the convention is actually stated — read them with `repo.read`, \
         PDFs included). Follow a hit with `repo.read` to read the file in full. Pass `repo` \
         naming the codebase (e.g. \"core-api\"); use `repo.list` if unsure which are indexed."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "Indexed repo to search (e.g. \"core-api\"). Omit only when a single repo is indexed; use repo.list to see options." },
                "pattern": { "type": "string", "description": "Regex to search for (same syntax as filesystem.grep)." },
                "path": { "type": "string", "description": "Optional subdirectory to limit the search (e.g. \"esr-integration\")." },
                "glob": { "type": "string", "description": "Optional filename glob filter (e.g. \"**/*.md\")." },
                "case_insensitive": { "type": "boolean", "description": "Match case-insensitively." },
                "max_matches": { "type": "integer", "description": "Cap on total matches (default 200)." }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: RepoGrepInput =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid input: {}", e))?;
        if let Some(denied) = maybe_guard(self.access_guard.as_ref(), start) {
            return Ok(denied);
        }
        let (dir, repo) =
            match resolve_checkout(self.resolver.as_ref(), &self.agent_workspace, &input.repo) {
                Ok(v) => v,
                Err(msg) => {
                    return Ok(ToolResult::failure(msg, start.elapsed().as_millis() as u64))
                }
            };
        let commit = head_commit(&dir).await;
        let grep = Grep::new(vec![dir]);
        let sub = json!({
            "pattern": input.pattern,
            "path": input.path,
            "glob": input.glob,
            "case_insensitive": input.case_insensitive,
            "max_matches": input.max_matches,
        });
        let mut result = grep.execute(sub).await?;
        // When the search lands hits, surface the authoritative docs in the same
        // module — the specs/READMEs the symbol graph misses and the agent would
        // otherwise infer from code. Grep paths are relative to the searched
        // subdir, so re-join `input.path` before inferring the module, else the
        // scope resolves to `src` and points nowhere.
        let base = input.path.as_deref().map(|p| p.trim_end_matches('/'));
        let full_paths: Vec<String> = result
            .output
            .get("matches")
            .and_then(|m| m.as_array())
            .into_iter()
            .flatten()
            .filter_map(|m| m.get("path").and_then(|p| p.as_str()))
            .map(|p| match base {
                Some(b) if !b.is_empty() => format!("{b}/{p}"),
                _ => p.to_string(),
            })
            .collect();
        if !full_paths.is_empty() {
            let scope = common_top_dir(full_paths.iter().map(String::as_str));
            if let Some(manifest) = nearby_docs_manifest(
                self.resolver.as_ref(),
                &self.agent_workspace,
                &input.repo,
                scope.as_deref(),
            )
            .await
            {
                if let serde_json::Value::Object(map) = &mut result.output {
                    map.insert("docs_nearby".to_string(), json!(manifest));
                }
            }
        }
        Ok(stamp(result, &repo, &commit))
    }
}

// ─── repo.read ──────────────────────────────────────────────────────────────

/// Read a file from an indexed repo's checkout (optionally a line range).
pub struct RepoReadTool {
    agent_workspace: PathBuf,
    resolver: Option<Arc<RepoResolver>>,
    access_guard: Option<KnowledgeAccessGuard>,
}

#[derive(Deserialize)]
struct RepoReadInput {
    #[serde(default)]
    repo: Option<String>,
    path: String,
    #[serde(default)]
    offset: Option<usize>,
    #[serde(default)]
    limit: Option<usize>,
}

impl RepoReadTool {
    pub fn new(agent_workspace: PathBuf) -> Self {
        Self {
            agent_workspace,
            resolver: None,
            access_guard: None,
        }
    }

    pub fn with_resolver(mut self, resolver: Arc<RepoResolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }

    pub fn with_access_guard(mut self, guard: KnowledgeAccessGuard) -> Self {
        self.access_guard = Some(guard);
        self
    }
}

#[async_trait]
impl Tool for RepoReadTool {
    fn name(&self) -> &str {
        "repo.read"
    }

    fn description(&self) -> &str {
        "Read a file from an indexed repository's current checkout, by repo-relative path \
         (e.g. path=\"esr-integration/README.md\"). The natural follow-up to a `repo.grep` hit: \
         read the file (or a line range via offset/limit) to get the full answer in context. \
         Reads real current content the symbol index can't show you, including PDFs — design \
         docs, specs and integration specs (e.g. ESR `docs/specs/*.pdf`) are extracted to text \
         automatically, so the convention/spec the symbol graph can't see becomes readable. \
         Result is stamped with the repo's commit. Other binary files are not returned as text."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "Indexed repo (e.g. \"core-api\"). Omit only when a single repo is indexed." },
                "path": { "type": "string", "description": "Repo-relative path to the file (e.g. \"esr-integration/README.md\")." },
                "offset": { "type": "integer", "description": "Line number to start reading from (0-indexed)." },
                "limit": { "type": "integer", "description": "Maximum number of lines to read." }
            },
            "required": ["path"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: RepoReadInput =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid input: {}", e))?;
        if let Some(denied) = maybe_guard(self.access_guard.as_ref(), start) {
            return Ok(denied);
        }
        let (dir, repo) =
            match resolve_checkout(self.resolver.as_ref(), &self.agent_workspace, &input.repo) {
                Ok(v) => v,
                Err(msg) => {
                    return Ok(ToolResult::failure(msg, start.elapsed().as_millis() as u64))
                }
            };
        let commit = head_commit(&dir).await;
        // PDFs are binary: a plain text read returns garbage, so extract instead.
        if is_pdf(&input.path) {
            let result = read_pdf(&dir, &input.path, input.offset, input.limit, start).await;
            return Ok(stamp(result, &repo, &commit));
        }
        let reader = ReadFile::new(vec![dir]);
        let sub = json!({
            "path": input.path,
            "offset": input.offset,
            "limit": input.limit,
        });
        let result = reader.execute(sub).await?;
        Ok(stamp(result, &repo, &commit))
    }
}

// ─── repo.glob ──────────────────────────────────────────────────────────────

/// Enumerate files in an indexed repo's checkout by filename glob.
pub struct RepoGlobTool {
    agent_workspace: PathBuf,
    resolver: Option<Arc<RepoResolver>>,
    access_guard: Option<KnowledgeAccessGuard>,
}

#[derive(Deserialize)]
struct RepoGlobInput {
    #[serde(default)]
    repo: Option<String>,
    pattern: String,
    #[serde(default)]
    path: Option<String>,
    #[serde(default)]
    max_results: Option<usize>,
}

impl RepoGlobTool {
    pub fn new(agent_workspace: PathBuf) -> Self {
        Self {
            agent_workspace,
            resolver: None,
            access_guard: None,
        }
    }

    pub fn with_resolver(mut self, resolver: Arc<RepoResolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }

    pub fn with_access_guard(mut self, guard: KnowledgeAccessGuard) -> Self {
        self.access_guard = Some(guard);
        self
    }
}

#[async_trait]
impl Tool for RepoGlobTool {
    fn name(&self) -> &str {
        "repo.glob"
    }

    fn description(&self) -> &str {
        "List files in an indexed repository's checkout by filename glob (e.g. \
         pattern=\"**/docs/specs/*\" or \"**/*.md\"). Use it to discover where docs, specs, or \
         config live before reading them with `repo.read`. Returns repo-relative paths stamped \
         with the repo's commit."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({
            "type": "object",
            "properties": {
                "repo": { "type": "string", "description": "Indexed repo (e.g. \"core-api\"). Omit only when a single repo is indexed." },
                "pattern": { "type": "string", "description": "Filename glob (e.g. \"**/*.md\")." },
                "path": { "type": "string", "description": "Optional subdirectory to limit the listing." },
                "max_results": { "type": "integer", "description": "Cap on results (default 500)." }
            },
            "required": ["pattern"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let input: RepoGlobInput =
            serde_json::from_value(input).map_err(|e| eyre::eyre!("Invalid input: {}", e))?;
        if let Some(denied) = maybe_guard(self.access_guard.as_ref(), start) {
            return Ok(denied);
        }
        let (dir, repo) =
            match resolve_checkout(self.resolver.as_ref(), &self.agent_workspace, &input.repo) {
                Ok(v) => v,
                Err(msg) => {
                    return Ok(ToolResult::failure(msg, start.elapsed().as_millis() as u64))
                }
            };
        let commit = head_commit(&dir).await;
        let globber = Glob::new(vec![dir]);
        let sub = json!({
            "pattern": input.pattern,
            "path": input.path,
            "max_results": input.max_results,
        });
        let result = globber.execute(sub).await?;
        Ok(stamp(result, &repo, &commit))
    }
}

// ─── repo.list ──────────────────────────────────────────────────────────────

/// List the repositories available to `repo.grep`/`repo.read`/`repo.glob`.
pub struct RepoListTool {
    resolver: Option<Arc<RepoResolver>>,
    access_guard: Option<KnowledgeAccessGuard>,
}

impl RepoListTool {
    pub fn new(_agent_workspace: PathBuf) -> Self {
        Self {
            resolver: None,
            access_guard: None,
        }
    }

    pub fn with_resolver(mut self, resolver: Arc<RepoResolver>) -> Self {
        self.resolver = Some(resolver);
        self
    }

    pub fn with_access_guard(mut self, guard: KnowledgeAccessGuard) -> Self {
        self.access_guard = Some(guard);
        self
    }
}

#[async_trait]
impl Tool for RepoListTool {
    fn name(&self) -> &str {
        "repo.list"
    }

    fn description(&self) -> &str {
        "List the indexed repositories you can search with `repo.grep`, read with `repo.read`, \
         or enumerate with `repo.glob`. Call this first if you're unsure which codebase holds \
         what you're looking for."
    }

    fn input_schema(&self) -> serde_json::Value {
        json!({ "type": "object", "properties": {} })
    }

    async fn execute(&self, _input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        if let Some(denied) = maybe_guard(self.access_guard.as_ref(), start) {
            return Ok(denied);
        }
        let repos = self
            .resolver
            .as_ref()
            .map(|r| r.indexed_names())
            .unwrap_or_default();
        Ok(ToolResult::success(
            json!({ "repos": repos, "count": repos.len() }),
            start.elapsed().as_millis() as u64,
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(dir: &Path, args: &[&str]) {
        let out = std::process::Command::new("git")
            .arg("-C")
            .arg(dir)
            .args(args)
            .output()
            .expect("git available");
        assert!(out.status.success(), "git {args:?} failed");
    }

    fn init_committed_repo(dir: &Path, files: &[(&str, &str)]) {
        git(dir, &["init", "-q"]);
        git(dir, &["config", "user.email", "t@example.com"]);
        git(dir, &["config", "user.name", "t"]);
        for (rel, body) in files {
            let path = dir.join(rel);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, body).unwrap();
        }
        git(dir, &["add", "-A"]);
        git(dir, &["commit", "-q", "-m", "init"]);
    }

    fn out_str(result: &ToolResult) -> String {
        serde_json::to_string(&result.output).unwrap()
    }

    // The flagship case: the answer lives in prose a symbol index never sees.
    // repo.grep (resolver-less → searches the agent workspace, which is the
    // repo) must surface it, and stamp the commit it read.
    #[tokio::test]
    async fn repo_grep_finds_prose_the_symbol_graph_cannot_see() {
        let dir = tempfile::tempdir().unwrap();
        init_committed_repo(
            dir.path(),
            &[(
                "esr-integration/README.md",
                "ESR file naming: inbound files use prefix AB_ / TA_; \
                 outbound files use prefix GO_ (Generic Outbound).\n",
            )],
        );

        let tool = RepoGrepTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(json!({ "pattern": "outbound" }))
            .await
            .unwrap();

        assert!(result.success, "grep failed: {:?}", result.error);
        let body = out_str(&result);
        assert!(
            body.contains("GO_"),
            "should surface the convention: {body}"
        );
        assert!(
            result.output.get("commit").is_some(),
            "commit provenance must be stamped: {body}"
        );
    }

    #[tokio::test]
    async fn repo_read_returns_full_file_content() {
        let dir = tempfile::tempdir().unwrap();
        init_committed_repo(dir.path(), &[("notes.txt", "alpha line\nbravo line\n")]);

        let tool = RepoReadTool::new(dir.path().to_path_buf());
        let result = tool.execute(json!({ "path": "notes.txt" })).await.unwrap();

        assert!(result.success, "read failed: {:?}", result.error);
        let body = out_str(&result);
        assert!(body.contains("bravo line"), "content missing: {body}");
        assert_eq!(
            result.output.get("repo").and_then(|r| r.as_str()),
            Some(".")
        );
    }

    #[tokio::test]
    async fn repo_glob_lists_files() {
        let dir = tempfile::tempdir().unwrap();
        init_committed_repo(
            dir.path(),
            &[("docs/spec.md", "x"), ("src/main.rs", "fn main() {}")],
        );

        let tool = RepoGlobTool::new(dir.path().to_path_buf());
        let result = tool.execute(json!({ "pattern": "**/*.md" })).await.unwrap();

        assert!(result.success, "glob failed: {:?}", result.error);
        let body = out_str(&result);
        assert!(
            body.contains("spec.md"),
            "glob should list the md file: {body}"
        );
    }

    #[tokio::test]
    async fn repo_grep_missing_pattern_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let tool = RepoGrepTool::new(dir.path().to_path_buf());
        let res = tool.execute(json!({})).await;
        assert!(res.is_err(), "missing required `pattern` must error");
    }

    /// A minimal, fully-valid PDF whose text layer reads
    /// "Inbound files use TA_ and AB_; outbound use GO_" — the ESR convention,
    /// in the one format the symbol graph and grep both go blind on.
    const MIN_PDF_B64: &str = "JVBERi0xLjQKMSAwIG9iago8PCAvVHlwZSAvQ2F0YWxvZyAvUGFnZXMgMiAwIFIgPj4KZW5kb2JqCjIgMCBvYmoKPDwgL1R5cGUgL1BhZ2VzIC9LaWRzIFszIDAgUl0gL0NvdW50IDEgPj4KZW5kb2JqCjMgMCBvYmoKPDwgL1R5cGUgL1BhZ2UgL1BhcmVudCAyIDAgUiAvTWVkaWFCb3ggWzAgMCA2MTIgNzkyXSAvQ29udGVudHMgNCAwIFIgL1Jlc291cmNlcyA8PCAvRm9udCA8PCAvRjEgNSAwIFIgPj4gPj4gPj4KZW5kb2JqCjQgMCBvYmoKPDwgL0xlbmd0aCA3OCA+PgpzdHJlYW0KQlQgL0YxIDE4IFRmIDcyIDcwMCBUZCAoSW5ib3VuZCBmaWxlcyB1c2UgVEFfIGFuZCBBQl87IG91dGJvdW5kIHVzZSBHT18pIFRqIEVUCmVuZHN0cmVhbQplbmRvYmoKNSAwIG9iago8PCAvVHlwZSAvRm9udCAvU3VidHlwZSAvVHlwZTEgL0Jhc2VGb250IC9IZWx2ZXRpY2EgPj4KZW5kb2JqCnhyZWYKMCA2CjAwMDAwMDAwMDAgNjU1MzUgZiAKMDAwMDAwMDAwOSAwMDAwMCBuIAowMDAwMDAwMDU4IDAwMDAwIG4gCjAwMDAwMDAxMTUgMDAwMDAgbiAKMDAwMDAwMDI0MSAwMDAwMCBuIAowMDAwMDAwMzY5IDAwMDAwIG4gCnRyYWlsZXIKPDwgL1NpemUgNiAvUm9vdCAxIDAgUiA+PgpzdGFydHhyZWYKNDM5CiUlRU9G";

    fn write_min_pdf(path: &Path) {
        use base64::Engine;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(MIN_PDF_B64)
            .unwrap();
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(path, bytes).unwrap();
    }

    // The whole point of this round: `repo.read` on a spec PDF must return the
    // PDF's *text*, not the binary the symbol graph and a plain read go blind on.
    #[tokio::test]
    async fn repo_read_extracts_pdf_text() {
        let dir = tempfile::tempdir().unwrap();
        init_committed_repo(dir.path(), &[("README.md", "see specs")]);
        write_min_pdf(&dir.path().join("docs/specs/esr-ta-inbound.pdf"));

        let tool = RepoReadTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(json!({ "path": "docs/specs/esr-ta-inbound.pdf" }))
            .await
            .unwrap();

        assert!(result.success, "pdf read failed: {:?}", result.error);
        let body = out_str(&result);
        assert!(
            body.contains("Inbound") && body.contains("GO_"),
            "extracted PDF text must carry the convention: {body}"
        );
        assert_eq!(
            result.output.get("extracted_from").and_then(|v| v.as_str()),
            Some("pdf"),
            "result must flag PDF extraction: {body}"
        );
    }

    // A real PDF that exists *outside* the checkout must be refused by the
    // containment check (canonicalize succeeds → prefix check fails), not merely
    // by a downstream read error.
    #[tokio::test]
    async fn repo_read_pdf_path_escape_is_denied() {
        let dir = tempfile::tempdir().unwrap();
        init_committed_repo(dir.path(), &[("README.md", "x")]);
        let outside = tempfile::tempdir().unwrap();
        let secret = outside.path().join("secret.pdf");
        write_min_pdf(&secret);

        let tool = RepoReadTool::new(dir.path().to_path_buf());
        // Absolute path straight to the external file.
        let abs = tool
            .execute(json!({ "path": secret.to_string_lossy() }))
            .await
            .unwrap();
        assert!(
            !abs.success,
            "absolute path outside the checkout must be denied"
        );
        // ...and a `../` traversal to it.
        let rel = tool
            .execute(json!({ "path": "../../../../../../../../tmp/nope.pdf" }))
            .await
            .unwrap();
        assert!(!rel.success, "`../` escape must be denied");
    }

    // A corrupt/garbage `.pdf` must fail cleanly — never panic the hub.
    #[tokio::test]
    async fn repo_read_garbage_pdf_fails_cleanly() {
        let dir = tempfile::tempdir().unwrap();
        init_committed_repo(
            dir.path(),
            &[("docs/broken.pdf", "%PDF-1.4 not really a pdf at all\n")],
        );

        let tool = RepoReadTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(json!({ "path": "docs/broken.pdf" }))
            .await
            .unwrap();
        assert!(!result.success, "garbage PDF should fail, not succeed");
        // The call returned a ToolResult at all → no panic escaped.
    }

    // The general manifest surfaces ALL doc formats, not just PDFs — scoped to a
    // subtree it lists the design docs/specs/READMEs that hold the answer.
    #[tokio::test]
    async fn nearby_docs_manifest_surfaces_all_doc_formats() {
        let dir = tempfile::tempdir().unwrap();
        init_committed_repo(
            dir.path(),
            &[
                ("svc/README.md", "service overview"),
                ("svc/docs/architecture.md", "# Architecture"),
                ("svc/docs/api.adoc", "= API"),
                ("svc/src/main.rs", "fn main() {}"),
            ],
        );
        write_min_pdf(&dir.path().join("svc/docs/specs/TA Inbound Spec v4.pdf"));

        // Scoped to the module: lists md + adoc + pdf + README.
        let manifest = nearby_docs_manifest(None, dir.path(), &None, Some("svc"))
            .await
            .expect("a subtree with docs must yield a manifest");
        for needle in [
            "TA Inbound Spec v4.pdf",
            "architecture.md",
            "api.adoc",
            "README.md",
        ] {
            assert!(
                manifest.contains(needle),
                "manifest missing {needle}: {manifest}"
            );
        }
    }

    // No docs → no manifest noise.
    #[tokio::test]
    async fn nearby_docs_manifest_is_none_without_docs() {
        let dir = tempfile::tempdir().unwrap();
        init_committed_repo(dir.path(), &[("src/main.rs", "fn main() {}")]);
        assert!(nearby_docs_manifest(None, dir.path(), &None, None)
            .await
            .is_none());
    }

    // Scope inference: hits sharing a top-level module → that module; spanning
    // modules or at the root → whole-repo (None).
    #[test]
    fn common_top_dir_picks_shared_module() {
        assert_eq!(
            common_top_dir([
                "esr-integration/src/Foo.kt",
                "esr-integration/src/test/Bar.kt"
            ])
            .as_deref(),
            Some("esr-integration")
        );
        // Hits spanning modules → no single scope.
        assert_eq!(
            common_top_dir(["esr-integration/src/Foo.kt", "audit-service/src/Bar.kt"]),
            None
        );
        // Root-level file → no useful scope.
        assert_eq!(common_top_dir(["README.md"]), None);
    }

    // THE general fix end-to-end: a code grep that lands in a module must hand
    // back `docs_nearby` listing that module's docs — so the agent reads intent
    // instead of inferring it. Mirrors the ESR shape (code under src/, spec PDF
    // under docs/specs/) without hard-coding ESR.
    #[tokio::test]
    async fn repo_grep_surfaces_nearby_docs() {
        let dir = tempfile::tempdir().unwrap();
        init_committed_repo(
            dir.path(),
            &[(
                "esr-integration/src/main/kotlin/EsrFileName.kt",
                "enum class InterfaceType(val sentToEsr: Boolean) { GO(false), TA(true) }\n",
            )],
        );
        write_min_pdf(
            &dir.path()
                .join("esr-integration/docs/specs/TA Inbound Spec v4.pdf"),
        );

        let tool = RepoGrepTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(json!({ "pattern": "InterfaceType" }))
            .await
            .unwrap();

        assert!(result.success, "grep failed: {:?}", result.error);
        let docs = result
            .output
            .get("docs_nearby")
            .and_then(|d| d.as_str())
            .unwrap_or("");
        // Grepped a CODE symbol, yet the spec PDF in the same module is surfaced —
        // discovery is pattern-independent, on the path the agent actually used.
        // Path must be repo-relative (scope re-prefixed) so `repo.read` can open it.
        assert!(
            docs.contains("esr-integration/docs/specs/TA Inbound Spec v4.pdf"),
            "grep must surface the nearby spec doc by repo-relative path: {docs}"
        );
    }

    // A grep with no hits must not attach a manifest (nothing to scope to).
    #[tokio::test]
    async fn repo_grep_no_hits_no_docs_nearby() {
        let dir = tempfile::tempdir().unwrap();
        init_committed_repo(dir.path(), &[("svc/src/main.rs", "fn main() {}")]);
        write_min_pdf(&dir.path().join("svc/docs/spec.pdf"));

        let tool = RepoGrepTool::new(dir.path().to_path_buf());
        let result = tool
            .execute(json!({ "pattern": "nonexistent_zzz" }))
            .await
            .unwrap();
        assert!(result.success);
        assert!(
            result.output.get("docs_nearby").is_none(),
            "no hits → no docs_nearby"
        );
    }
}
