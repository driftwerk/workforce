//! Code locations: a file a code search hit that the agent then read, and
//! the symbol the hit was in. Only the repo, the path and the symbol's name
//! are kept, each checked against the indexed checkout and its code graph;
//! never the search pattern, a matched line or anything from the file.

use std::collections::{HashMap, HashSet};
use std::path::{Component, Path};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use parking_lot::Mutex;
use serde_json::Value;
use workforce_codebase_knowledge::graph::entities::EntityKind;
use workforce_codebase_knowledge::graph::CodeGraph;

use super::{Entry, Kind, KnowHow, Learned, Observation, Revision, Turn};
use crate::knowledge::{IndexTarget, RepoCatalog, RepoResolver};

/// The searches whose hits a read can follow.
pub(super) const SEARCH_TOOLS: &[&str] = &["repo.grep", "repo.glob"];
/// The read that confirms a hit was what the agent wanted.
pub const READ_TOOL: &str = "repo.read";
/// How long the hub's repo catalog is reused before it is read again.
const CATALOG_TTL: Duration = Duration::from_secs(30);

/// A symbol the code graph has in a file, with its lines (1-based,
/// inclusive).
#[derive(Debug, Clone, PartialEq)]
pub struct Symbol {
    pub name: String,
    pub start: usize,
    pub end: usize,
}

/// The indexed checkouts and their code graph, as know-how checks them.
#[async_trait]
pub trait CodeIndex: Send + Sync {
    /// The indexed repo (`owner/repo`) a code tool's `repo` argument names:
    /// the only one indexed when it names none.
    fn resolve(&self, repo: Option<&str>) -> Option<String>;
    /// Every indexed repo.
    fn repos(&self) -> HashSet<String>;
    /// Whether `path` is a file of the repo's checkout; `None` when the repo
    /// is not indexed.
    fn has_file(&self, repo: &str, path: &str) -> Option<bool>;
    /// The symbols the code graph has in `path`.
    async fn symbols(&self, repo: &str, path: &str) -> Vec<Symbol>;
    /// The files the code graph has `name` defined in.
    async fn defining(&self, repo: &str, name: &str) -> Vec<String>;
}

static CODE_INDEX: OnceLock<Arc<dyn CodeIndex>> = OnceLock::new();

/// Install the process's code index (once the hub's repo catalog exists).
/// `false` if one was already installed.
pub fn set_code_index(index: Arc<dyn CodeIndex>) -> bool {
    CODE_INDEX.set(index).is_ok()
}

pub fn code_index() -> Option<Arc<dyn CodeIndex>> {
    CODE_INDEX.get().cloned()
}

/// The repos of `index` the person in scope (`CURRENT_USER_ID`) reads
/// through an agent's code tools: every indexed one when the agent has
/// `repo.read` and the permission cascade does not deny it to them, none
/// otherwise. Which files of them the agent's policy opens is
/// [`policy_opens`]'s to say; nothing decides per person per repo.
pub fn readable_repos(
    index: Option<Arc<dyn CodeIndex>>,
    tools: &workforce_agent::tools::registry::ToolRegistry,
    evaluator: Option<Arc<workforce_permissions::cache::PermissionEvaluator>>,
    agent: &str,
) -> HashSet<String> {
    let Some(index) = index else {
        return HashSet::new();
    };
    let allowed = tools.has(READ_TOOL)
        && evaluator.is_none_or(|ev| {
            crate::knowledge::KnowledgeAccessGuard::new(ev, agent, READ_TOOL).check()
                == crate::knowledge::GuardOutcome::Allowed
        });
    match allowed {
        true => index.repos(),
        false => HashSet::new(),
    }
}

/// Whether the agent's policy lets `repo.read` open `path` of `repo` without
/// an approval, with the repo named either way a call may name it
/// (`owner/repo` or bare).
pub(crate) fn policy_opens(
    policy: &workforce_policy::engine::PolicyEngine,
    agent: &str,
    repo: &str,
    path: &str,
) -> bool {
    [repo, repo.rsplit('/').next().unwrap_or(repo)]
        .iter()
        .all(|name| {
            policy
                .evaluate(&workforce_core::policy::PolicyRequest {
                    agent_id: workforce_core::types::AgentId(agent.to_string()),
                    tool_name: READ_TOOL.to_string(),
                    parameters: serde_json::json!({ "repo": name, "path": path }),
                    context: workforce_core::policy::PolicyContext::empty(),
                })
                .decision
                .is_allowed()
        })
}

/// The key of a file, and the name a turn reading it uses (for scoring).
pub(crate) fn file_key(repo: &str, path: &str) -> String {
    format!("code:{repo}:{path}")
}

fn symbol_key(repo: &str, path: &str, symbol: &str) -> String {
    format!("{}#{symbol}", file_key(repo, path))
}

/// A repo-relative path in one spelling (`a/b.rs`), refusing anything that
/// leaves the repo or names its root.
fn clean(path: &str) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    for c in Path::new(path.trim()).components() {
        match c {
            Component::Normal(p) => parts.push(p.to_str()?),
            Component::CurDir => {}
            _ => return None,
        }
    }
    (!parts.is_empty()).then(|| parts.join("/"))
}

/// A search's hits as repo-relative paths, with the line for a grep match.
/// Hit paths are relative to the searched `path`, so it is joined back on.
fn hits(inv: &workforce_agent::runner::ToolInvocation) -> Vec<(String, Option<usize>)> {
    let base = inv
        .input
        .get("path")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim_matches('/');
    let join = |p: &str| match (base.is_empty(), p.is_empty()) {
        (true, _) => clean(p),
        (false, true) => clean(base),
        (false, false) => clean(&format!("{base}/{p}")),
    };
    let out = &inv.output;
    let grep = out
        .get("matches")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|m| {
            let path = join(m.get("path")?.as_str()?)?;
            let line = m.get("line_number").and_then(Value::as_u64);
            Some((path, line.map(|l| l as usize)))
        });
    let glob = out
        .get("paths")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|p| Some((join(p.as_str()?)?, None)));
    grep.chain(glob).collect()
}

/// The lines a read covered (1-based, inclusive), when it asked for a range.
fn read_range(inv: &workforce_agent::runner::ToolInvocation) -> Option<(usize, usize)> {
    let offset = inv.input.get("offset").and_then(Value::as_u64);
    let limit = inv.input.get("limit").and_then(Value::as_u64);
    match (offset, limit) {
        (None, None) => None,
        (o, l) => {
            let start = o.unwrap_or(0) as usize + 1;
            Some((start, l.map_or(usize::MAX, |l| start + l as usize - 1)))
        }
    }
}

/// The innermost symbol around the first hit line the read covered.
fn symbol_at(
    symbols: &[Symbol],
    range: Option<(usize, usize)>,
    lines: &[Option<usize>],
) -> Option<String> {
    let line = lines
        .iter()
        .flatten()
        .copied()
        .find(|l| range.is_none_or(|(a, b)| (a..=b).contains(l)))?;
    symbols
        .iter()
        .filter(|s| s.start <= line && line <= s.end)
        .min_by_key(|s| s.end - s.start)
        .map(|s| s.name.clone())
}

impl KnowHow {
    /// Code locations from a turn: a successful read of a file an earlier
    /// successful search in the turn hit, in a repo the index has, the file
    /// still there. One entry for the file and one for the symbol the hit
    /// was in, so people reading the same file for different reasons still
    /// confirm the file.
    pub(super) async fn learn_code(&self, turn: &Turn<'_>, person: &str) -> Learned {
        let Some(index) = self.code() else {
            return Learned::default();
        };
        let mut searched: HashMap<(String, String), Vec<Option<usize>>> = HashMap::new();
        let mut out = Learned::default();
        for inv in turn.invocations.iter().filter(|i| i.success) {
            let name = inv.tool_name.as_str();
            if !SEARCH_TOOLS.contains(&name) && name != READ_TOOL {
                continue;
            }
            let Some(repo) = index.resolve(inv.input.get("repo").and_then(Value::as_str)) else {
                continue;
            };
            if name != READ_TOOL {
                for (path, line) in hits(inv) {
                    searched.entry((repo.clone(), path)).or_default().push(line);
                }
                continue;
            }
            let Some(path) = inv
                .input
                .get("path")
                .and_then(Value::as_str)
                .and_then(clean)
            else {
                continue;
            };
            out.used.insert(file_key(&repo, &path));
            let Some(lines) = searched.get(&(repo.clone(), path.clone())) else {
                continue;
            };
            if index.has_file(&repo, &path) != Some(true) {
                continue;
            }
            let symbol = symbol_at(&index.symbols(&repo, &path).await, read_range(inv), lines);
            let at = |key: String, symbol: Option<String>| Observation {
                repo: Some(repo.clone()),
                path: Some(path.clone()),
                symbol,
                ..Observation::of(Kind::CodeLocation, key, person.to_string())
            };
            out.found.push(at(file_key(&repo, &path), None));
            if let Some(s) = symbol {
                out.found.push(at(symbol_key(&repo, &path, &s), Some(s)));
            }
        }
        out
    }
}

/// Where a code location points now: unchanged, its symbol followed to the
/// one file that defines it, or stale when its repo, its file or its symbol
/// is gone. `None` when nothing changed.
pub(super) async fn recheck(e: &Entry, index: &dyn CodeIndex) -> Option<Revision> {
    let gone = || super::stale::revise(e, Vec::new(), None, true);
    let (Some(repo), Some(path)) = (e.repo.as_deref(), e.path.as_deref()) else {
        return gone();
    };
    let Some(exists) = index.has_file(repo, path) else {
        return gone();
    };
    let Some(symbol) = &e.symbol else {
        return super::stale::revise(e, Vec::new(), None, !exists);
    };
    if exists
        && index
            .symbols(repo, path)
            .await
            .iter()
            .any(|s| s.name == *symbol)
    {
        return super::stale::revise(e, Vec::new(), None, false);
    }
    match index.defining(repo, symbol).await.as_slice() {
        [moved] if index.has_file(repo, moved) == Some(true) => super::stale::revise(
            e,
            Vec::new(),
            Some((symbol_key(repo, moved, symbol), moved.clone())),
            false,
        ),
        _ => gone(),
    }
}

/// The hub's code index: the repo catalog the code tools resolve against,
/// and the code graph the maintenance pass keeps.
pub struct HubCodeIndex {
    resolver: Arc<RepoResolver>,
    graph: Arc<CodeGraph>,
    catalog: Mutex<Option<(Instant, Arc<RepoCatalog>)>>,
}

impl HubCodeIndex {
    pub fn new(resolver: Arc<RepoResolver>, graph: Arc<CodeGraph>) -> Self {
        Self {
            resolver,
            graph,
            catalog: Mutex::new(None),
        }
    }

    fn catalog(&self) -> Option<Arc<RepoCatalog>> {
        if let Some((at, c)) = self.catalog.lock().as_ref() {
            if at.elapsed() < CATALOG_TTL {
                return Some(Arc::clone(c));
            }
        }
        let catalog = match self.resolver.catalog() {
            Ok(c) => Arc::new(c),
            Err(e) => {
                tracing::warn!(target: "know_how", error = %e, "reading the repo catalog failed");
                return None;
            }
        };
        *self.catalog.lock() = Some((Instant::now(), Arc::clone(&catalog)));
        Some(catalog)
    }

    fn target(&self, repo: &str) -> Option<IndexTarget> {
        self.catalog()?.resolve_target(repo).ok().cloned()
    }

    fn repo_id(&self, repo: &str) -> Option<String> {
        self.target(repo)
            .map(|t| crate::knowledge::KnowledgeService::repo_id_for(&t.dest))
    }
}

#[async_trait]
impl CodeIndex for HubCodeIndex {
    fn resolve(&self, repo: Option<&str>) -> Option<String> {
        let catalog = self.catalog()?;
        let target = match repo {
            Some(name) => catalog.resolve_target(name).ok()?,
            None => catalog.sole_target()?,
        };
        Some(target.source.canonical_slug())
    }

    fn repos(&self) -> HashSet<String> {
        self.catalog()
            .map(|c| {
                c.targets()
                    .iter()
                    .map(|t| t.source.canonical_slug())
                    .collect()
            })
            .unwrap_or_default()
    }

    fn has_file(&self, repo: &str, path: &str) -> Option<bool> {
        let dest = self.target(repo)?.dest;
        let dest = std::fs::canonicalize(&dest).unwrap_or(dest);
        Some(
            workforce_agent::tools::filesystem::resolve_within(Path::new(path), &[dest])
                .is_some_and(|p| p.is_file()),
        )
    }

    async fn symbols(&self, repo: &str, path: &str) -> Vec<Symbol> {
        let Some(repo_id) = self.repo_id(repo) else {
            return Vec::new();
        };
        match self.graph.find_entities_in_file(&repo_id, path).await {
            Ok(entities) => entities
                .into_iter()
                .filter(|e| !matches!(e.kind, EntityKind::File | EntityKind::Import))
                .map(|e| Symbol {
                    name: e.name,
                    start: e.start_line,
                    end: e.end_line,
                })
                .collect(),
            Err(e) => {
                tracing::warn!(target: "know_how", error = %e, "reading symbols failed");
                Vec::new()
            }
        }
    }

    async fn defining(&self, repo: &str, name: &str) -> Vec<String> {
        let Some(repo_id) = self.repo_id(repo) else {
            return Vec::new();
        };
        let mut files: Vec<String> = self
            .graph
            .find_entities_by_name(&repo_id, name, 50)
            .await
            .unwrap_or_default()
            .into_iter()
            .filter(|e| e.name == name && !matches!(e.kind, EntityKind::File | EntityKind::Import))
            .map(|e| e.file_path)
            .collect();
        files.sort();
        files.dedup();
        files
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Two;

    #[async_trait]
    impl CodeIndex for Two {
        fn resolve(&self, _: Option<&str>) -> Option<String> {
            None
        }
        fn repos(&self) -> HashSet<String> {
            HashSet::from(["acme/app".to_string(), "acme/payroll".to_string()])
        }
        fn has_file(&self, _: &str, _: &str) -> Option<bool> {
            None
        }
        async fn symbols(&self, _: &str, _: &str) -> Vec<Symbol> {
            Vec::new()
        }
        async fn defining(&self, _: &str, _: &str) -> Vec<String> {
            Vec::new()
        }
    }

    #[test]
    fn only_an_agent_with_repo_read_reads_its_repos() {
        let tools = workforce_agent::tools::registry::ToolRegistry::new();
        let index = || Some(Arc::new(Two) as Arc<dyn CodeIndex>);
        assert!(
            readable_repos(index(), &tools, None, "atlas").is_empty(),
            "an agent without repo.read reads nothing"
        );
        tools.register(Arc::new(crate::knowledge::RepoReadTool::new(
            std::path::PathBuf::from("/nonexistent"),
        )));
        assert_eq!(readable_repos(index(), &tools, None, "atlas").len(), 2);
        assert!(readable_repos(None, &tools, None, "atlas").is_empty());
    }

    #[test]
    fn the_policy_decides_each_repo_and_file_a_read_opens() {
        let policy = workforce_policy::engine::PolicyEngine::from_yaml(
            r#"
rules:
  - name: "no-payroll-no-secrets"
    scope: global
    tool: "repo.read"
    action: allow
    conditions:
      - field: "repo"
        operator: equals
        value: "payroll"
        action: deny
      - field: "path"
        operator: starts_with
        value: "secrets/"
        action: deny
"#,
        )
        .unwrap();
        assert!(policy_opens(&policy, "atlas", "acme/app", "src/a.rs"));
        assert!(
            !policy_opens(&policy, "atlas", "acme/payroll", "src/a.rs"),
            "a repo the policy denies by its bare name"
        );
        assert!(
            !policy_opens(&policy, "atlas", "acme/app", "secrets/keys.rs"),
            "a file the policy denies"
        );
    }

    #[test]
    fn paths_are_spelt_one_way_and_never_leave_the_repo() {
        assert_eq!(clean("./src/a.rs").as_deref(), Some("src/a.rs"));
        assert_eq!(clean("src//a.rs").as_deref(), Some("src/a.rs"));
        assert_eq!(clean("../x"), None);
        assert_eq!(clean("/etc/passwd"), None);
        assert_eq!(clean("."), None);
    }

    #[test]
    fn the_symbol_is_the_innermost_around_a_hit_the_read_covered() {
        let symbols = [
            Symbol {
                name: "Orders".into(),
                start: 1,
                end: 100,
            },
            Symbol {
                name: "fill_rate".into(),
                start: 10,
                end: 20,
            },
        ];
        assert_eq!(
            symbol_at(&symbols, None, &[Some(12)]).as_deref(),
            Some("fill_rate")
        );
        assert_eq!(
            symbol_at(&symbols, Some((30, 60)), &[Some(12), Some(40)]).as_deref(),
            Some("Orders"),
            "the hit the read covered"
        );
        assert_eq!(
            symbol_at(&symbols, None, &[None]),
            None,
            "a glob hit has no line"
        );
    }
}
