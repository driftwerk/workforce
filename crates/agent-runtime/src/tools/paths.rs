//! Path anchoring shared by every tool that accepts a path from the agent.
//!
//! One rule, one implementation: a relative path resolves against the agent's
//! persistent workspace (the first root), and the result must stay inside the
//! allowed roots. Tools that resolved against the process CWD instead wrote
//! into the container filesystem, which a deploy discards — that is how a
//! WASM skill's source was lost while its installed binary kept running
//!.

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};

use workforce_common::private_fs::{
    is_own_entry, is_shared_dated_note, is_withheld_from_tools, TOOL_REFUSED,
};

/// Resolve `.` and `..` without touching the filesystem, so containment holds
/// for paths that do not exist yet (a directory about to be created).
fn normalize_lexically(path: &Path) -> PathBuf {
    path.components()
        .fold(PathBuf::new(), |mut acc, component| {
            match component {
                Component::CurDir => {}
                Component::ParentDir => {
                    if !acc.pop() {
                        acc.push(Component::ParentDir);
                    }
                }
                other => acc.push(other),
            }
            acc
        })
}

/// The real location `path` denotes: the deepest existing ancestor
/// canonicalised, with the not-yet-created remainder folded onto it.
///
/// The filesystem is consulted *before* `..` is folded, because folding first
/// erases the very component that escapes — `{root}/link/..` is the parent of
/// the symlink's target, not `{root}`. An agent that can create a symlink in
/// its workspace could otherwise name any path on the host.
///
/// Canonicalising only whole existing paths is not enough either: a directory
/// about to be created under a symlinked mount (`/var`, an FSx path) would
/// keep the symlinked spelling while its root canonicalised to the real one,
/// and every such path would look like an escape.
pub fn resolve_real(path: &Path) -> PathBuf {
    let mut existing = path;
    let mut tail: Vec<&std::ffi::OsStr> = Vec::new();
    loop {
        if let Ok(real) = existing.canonicalize() {
            // The tail names components that do not exist, so folding `..`
            // here cannot cross a symlink.
            return tail.iter().rev().fold(real, |acc, name| {
                if *name == OsStr::new("..") {
                    let parent = acc.parent().map(Path::to_path_buf);
                    parent.unwrap_or(acc)
                } else if *name == OsStr::new(".") {
                    acc
                } else {
                    acc.join(name)
                }
            });
        }
        match (existing.parent(), existing.file_name()) {
            (Some(parent), Some(name)) => {
                tail.push(name);
                existing = parent;
            }
            _ => return normalize_lexically(path),
        }
    }
}

/// Resolve a relative path against the first root; an absolute path is its own
/// resolution.
pub fn resolve_against_root(path: &Path, roots: &[PathBuf]) -> PathBuf {
    match (path.is_relative(), roots.first()) {
        (true, Some(root)) => root.join(path),
        _ => path.to_path_buf(),
    }
}

/// Allowed roots with their real locations resolved once.
///
/// Containment is decided against these, so a caller that checks many paths —
/// `filesystem.grep` walks every file in a tree — resolves the roots once
/// instead of once per candidate. The positions the hub keeps apart for each
/// person are not inside any root a tool is given
/// ([`workforce_common::private_fs::withhold_from_tools`]).
#[derive(Debug, Clone)]
pub struct CanonicalRoots {
    roots: Vec<PathBuf>,
    withholds: Withholding,
}

/// What a set of roots keeps a tool out of.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Withholding {
    Nothing,
    /// The positions the hub keeps for each person: the roots of a turn run
    /// for nobody.
    PerPerson,
    /// Those but the person's own notes and profile, and the shared dated
    /// notes: the roots of a turn run for that person.
    ForPerson(String),
}

impl CanonicalRoots {
    /// The roots of the turn in scope, read now: a walk that runs on a
    /// blocking thread, which the turn's identity does not reach, is handed
    /// roots made before it.
    pub fn new(roots: &[PathBuf]) -> Self {
        let person = crate::runner::CURRENT_USER_ID
            .try_with(|user| user.clone())
            .ok()
            .filter(|user| !user.is_empty());
        Self {
            roots: roots.iter().map(|root| resolve_real(root)).collect(),
            withholds: person.map_or(Withholding::PerPerson, Withholding::ForPerson),
        }
    }

    /// Roots the hub writes a person's own state through (their notes and
    /// profile), which nothing is withheld from.
    pub fn for_hub_state(roots: &[PathBuf]) -> Self {
        Self {
            withholds: Withholding::Nothing,
            ..Self::new(roots)
        }
    }

    /// Whether these roots keep a tool out of `real`.
    fn withheld(&self, real: &Path) -> bool {
        match &self.withholds {
            Withholding::Nothing => false,
            Withholding::PerPerson => is_withheld_from_tools(real),
            Withholding::ForPerson(person) => {
                (is_withheld_from_tools(real) && !is_own_entry(real, person))
                    || is_shared_dated_note(real)
            }
        }
    }

    /// No roots means unrestricted — the single-tenant local default.
    fn is_unrestricted(&self) -> bool {
        self.roots.is_empty()
    }

    /// The agent's workspace: the first root, against which relative paths
    /// resolve.
    pub fn workspace(&self) -> Option<&Path> {
        self.roots.first().map(PathBuf::as_path)
    }

    /// Whether `real`, a location already resolved, lies inside the roots.
    pub fn holds(&self, real: &Path) -> bool {
        (self.is_unrestricted() || self.roots.iter().any(|root| real.starts_with(root)))
            && !self.withheld(real)
    }

    pub fn contains(&self, path: &Path) -> bool {
        self.resolve(path).is_some()
    }

    /// The real location of `path` when it stays inside the roots, else
    /// `None`. Containment is decided on the real location, and that same
    /// location is what callers get back — checking one path and using
    /// another is how a symlink escapes.
    pub fn resolve(&self, path: &Path) -> Option<PathBuf> {
        let real = resolve_real(path);
        self.holds(&real).then_some(real)
    }
}

/// Whether `resolved` stays inside `roots`. No roots means unrestricted — the
/// single-tenant local default.
pub fn is_within_roots(resolved: &Path, roots: &[PathBuf]) -> bool {
    CanonicalRoots::new(roots).contains(resolved)
}

/// Resolve `path` and return it only when it stays inside `roots`; `None` on
/// escape.
pub fn resolve_within(path: &Path, roots: &[PathBuf]) -> Option<PathBuf> {
    CanonicalRoots::new(roots).resolve(&resolve_against_root(path, roots))
}

/// Whether `path`, wherever the links along it lead, is withheld from the
/// tools of the turn in scope: a position the hub keeps for each person
/// (save, in a turn run for a person, their own notes and profile), or, in a
/// turn run for a person, a shared dated note mixing whoever spoke that day. For a tool that lists or searches a tree, which leaves such entries
/// out.
pub fn is_withheld(path: &Path) -> bool {
    CanonicalRoots::new(&[]).withheld(&resolve_real(path))
}

/// Why a tool may not use `path`, given as `shown`: it leads into a position
/// withheld from tools, or outside the roots.
pub fn refusal(shown: &str, path: &Path) -> String {
    match is_withheld(path) {
        true => format!("Access denied: '{shown}': {TOOL_REFUSED}"),
        false => format!("Access denied: path '{shown}' is outside allowed directories"),
    }
}

/// Resolve an agent-supplied path, refusing an escape with an error that names
/// the roots — the agent learns the contract from the failure rather than
/// discovering it as a lost file.
pub fn resolve_in_roots(raw: &str, roots: &[PathBuf]) -> Result<PathBuf, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("Path must not be empty".to_string());
    }
    let Some(workspace) = roots.first() else {
        return Err("No workspace is configured for path resolution".to_string());
    };
    let resolved = resolve_against_root(Path::new(raw), roots);
    if let Some(real) = CanonicalRoots::new(roots).resolve(&resolved) {
        return Ok(real);
    }
    if is_withheld(&resolved) {
        return Err(format!("'{raw}': {TOOL_REFUSED}"));
    }
    let allowed = roots
        .iter()
        .map(|r| r.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!(
        "Path '{}' resolves to {}, outside the allowed directories [{}]. \
         Use a relative path — it resolves against your workspace at {}.",
        raw,
        resolved.display(),
        allowed,
        workspace.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Roots are canonical in production (`serve.rs` canonicalises the
    /// workspace), and so is everything these functions return.
    fn workspace() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().canonicalize().unwrap();
        (tmp, root)
    }

    fn roots(dir: &Path) -> Vec<PathBuf> {
        vec![dir.to_path_buf()]
    }

    #[test]
    fn relative_paths_resolve_against_the_first_root() {
        let (_tmp, root) = workspace();
        let resolved = resolve_in_roots("wasm-skills/demo", &roots(&root)).unwrap();
        assert_eq!(resolved, root.join("wasm-skills/demo"));
    }

    /// The bug this module exists to close: a relative path must land under
    /// the workspace, never under wherever the hub process happens to run
    /// (`std::env::current_dir` is deliberately never consulted).
    #[test]
    fn resolution_is_anchored_to_the_workspace_not_the_cwd() {
        let (_tmp, root) = workspace();
        let resolved = resolve_in_roots("skills/x", &roots(&root)).unwrap();
        assert_eq!(resolved, root.join("skills/x"));
    }

    #[test]
    fn absolute_paths_inside_a_root_are_accepted() {
        let (_tmp, root) = workspace();
        let inside = root.join("nested/file.toml");
        let resolved = resolve_in_roots(inside.to_str().unwrap(), &roots(&root)).unwrap();
        assert_eq!(resolved, inside);
    }

    #[test]
    fn absolute_paths_outside_every_root_are_refused() {
        let (_tmp, root) = workspace();
        let err = resolve_in_roots("/etc/passwd", &roots(&root)).unwrap_err();
        assert!(err.contains("/etc/passwd"), "{err}");
        assert!(err.contains(&root.display().to_string()), "{err}");
    }

    /// A `..` escape on a path that does not exist yet cannot be caught by
    /// canonicalisation — only the lexical fold sees it.
    #[test]
    fn parent_traversal_out_of_the_workspace_is_refused() {
        let (_tmp, root) = workspace();
        assert!(resolve_in_roots("wasm-skills/../../../etc/evil", &roots(&root)).is_err());
    }

    /// THE containment hole: an agent can create a symlink in its own
    /// workspace, and `..` through it lands in the *target's* parent — so the
    /// filesystem must be consulted before `..` is folded away.
    #[test]
    #[cfg(unix)]
    fn parent_traversal_through_an_agent_created_symlink_is_refused() {
        let (_tmp, root) = workspace();
        let (_outside_tmp, outside) = workspace();
        std::fs::write(outside.join("secret.txt"), "classified").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();

        for escape in [
            "link/../secret.txt".to_string(),
            format!("link/..{}/secret.txt", outside.display()),
            "link/secret.txt".to_string(),
        ] {
            let result = resolve_in_roots(&escape, &roots(&root));
            assert!(result.is_err(), "{escape} escaped containment: {result:?}");
        }
    }

    /// The same escape through the `Option`-returning entry the filesystem
    /// tools use — this one guarded it before the shared module existed.
    #[test]
    #[cfg(unix)]
    fn resolve_within_refuses_a_symlink_escape() {
        let (_tmp, root) = workspace();
        let (_outside_tmp, outside) = workspace();
        std::fs::write(outside.join("secret.txt"), "classified").unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        assert!(resolve_within(Path::new("link/../secret.txt"), &roots(&root)).is_none());
        assert!(resolve_within(Path::new("link/secret.txt"), &roots(&root)).is_none());
    }

    /// What is validated is what is returned: a caller that writes to the
    /// returned path must not reach a different file than the one checked.
    #[test]
    fn the_returned_path_is_the_validated_one() {
        let (_tmp, root) = workspace();
        std::fs::create_dir_all(root.join("a")).unwrap();
        let resolved = resolve_in_roots("a/../b", &roots(&root)).unwrap();
        assert_eq!(resolved, root.join("b"));
        std::fs::write(&resolved, "usable").unwrap();
    }

    /// A root reached through a symlink (a `/var` temp dir, an FSx mount)
    /// must still accept paths that do not exist yet.
    #[test]
    #[cfg(unix)]
    fn a_not_yet_created_path_under_a_symlinked_root_is_accepted() {
        let (_real_tmp, real) = workspace();
        let (_link_tmp, link_parent) = workspace();
        let link = link_parent.join("workspace-link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let resolved = resolve_in_roots("wasm-skills/demo", &roots(&link)).unwrap();
        assert_eq!(resolved, real.join("wasm-skills/demo"));
    }

    #[test]
    fn a_second_root_is_honoured() {
        let (_ws_tmp, workspace_root) = workspace();
        let (_wt_tmp, worktrees) = workspace();
        let roots = vec![workspace_root, worktrees.clone()];
        let inside = worktrees.join("repo/Skill.toml");
        assert!(resolve_in_roots(inside.to_str().unwrap(), &roots).is_ok());
    }

    /// A sibling whose name merely starts with the root's must not pass.
    #[test]
    fn a_prefix_sibling_of_a_root_is_refused() {
        let (_tmp, root) = workspace();
        let sibling = root.with_file_name(format!(
            "{}-evil",
            root.file_name().unwrap().to_string_lossy()
        ));
        std::fs::create_dir_all(&sibling).unwrap();
        assert!(resolve_in_roots(sibling.to_str().unwrap(), &roots(&root)).is_err());
    }

    /// A write target whose parent does not exist yet must still be checked
    /// on its real location: falling back to a raw string prefix admitted
    /// `a/b/../../../etc/x` whenever `a` was missing.
    #[test]
    fn a_traversal_through_missing_directories_is_refused() {
        let (_tmp, root) = workspace();
        assert!(!is_within_roots(
            &root.join("a/b/../../../etc/evil"),
            &roots(&root)
        ));
        assert!(is_within_roots(&root.join("a/b/../c/file"), &roots(&root)));
    }

    /// The reusable form must decide exactly as the one-shot functions do —
    /// it is the same rule, resolved once instead of per candidate.
    #[test]
    #[cfg(unix)]
    fn canonical_roots_agrees_with_the_one_shot_check() {
        let (_tmp, root) = workspace();
        let (_outside_tmp, outside) = workspace();
        std::fs::create_dir_all(root.join("real")).unwrap();
        std::os::unix::fs::symlink(&outside, root.join("link")).unwrap();
        let roots = roots(&root);
        let canonical = CanonicalRoots::new(&roots);

        for candidate in [
            root.join("real/file"),
            root.join("missing/deep/file"),
            root.join("link/../escaped"),
            root.join("a/b/../../../etc/evil"),
            outside.join("file"),
            PathBuf::from("/etc/passwd"),
        ] {
            assert_eq!(
                canonical.contains(&candidate),
                is_within_roots(&candidate, &roots),
                "disagreement on {}",
                candidate.display()
            );
        }
    }

    /// Roots are resolved once, so a path created after construction must
    /// still be judged correctly.
    #[test]
    fn canonical_roots_handles_paths_created_after_construction() {
        let (_tmp, root) = workspace();
        let canonical = CanonicalRoots::new(&roots(&root));
        let created = root.join("made-later/file.txt");
        std::fs::create_dir_all(created.parent().unwrap()).unwrap();
        std::fs::write(&created, "x").unwrap();
        assert_eq!(canonical.resolve(&created), Some(created));
    }

    #[test]
    fn canonical_roots_without_roots_is_unrestricted() {
        assert!(CanonicalRoots::new(&[]).contains(Path::new("/etc/passwd")));
    }

    #[test]
    fn empty_input_and_missing_roots_are_refused() {
        let (_tmp, root) = workspace();
        assert!(resolve_in_roots("   ", &roots(&root)).is_err());
        assert!(resolve_in_roots("skills/x", &[]).is_err());
    }

    /// Surrounding whitespace is trimmed rather than creating a directory
    /// whose name begins with a space.
    #[test]
    fn surrounding_whitespace_is_trimmed() {
        let (_tmp, root) = workspace();
        let resolved = resolve_in_roots("  wasm-skills/demo  ", &roots(&root)).unwrap();
        assert_eq!(resolved, root.join("wasm-skills/demo"));
    }
}
