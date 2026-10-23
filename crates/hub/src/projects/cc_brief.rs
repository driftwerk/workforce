//! Per-Slack-thread "task brief" persistence for `/cc`.
//!
//! When `/cc` opens a thread the user's initial ask is captured here, and
//! when a session is reset (transcript wiped → `session_lost` recovery,
//! or rotation → `session_rotated`) the brief is surfaced back so the
//! user doesn't have to re-state context. The retrospective in
//! `~/.claude/plans/what-happened-in-this-goofy-willow.md` documents the
//! same user pasting the same task three times in one thread because
//! every fresh UUID told them their context was gone.
//!
//! Storage layout:
//!  `{data_dir}/cc-briefs/{slugified_session_key}.md`
//!
//! The session key (Slack workspace + channel + thread) is unique per
//! /cc thread, so it alone is sufficient to identify a brief. We
//! deliberately do NOT key on agent_id — save and load happen in
//! different code paths and reconciling the agent identifier between
//! them is a coordination hazard with no upside.
//!
//! Markdown is a deliberate choice over JSON: humans (operators tailing
//! the data dir) can `cat` the file and read what an agent thought it
//! was doing. The header carries minimal structured metadata as YAML
//! front-matter for the loader.
//!
//! Writes are atomic via tempfile-rename. The full path is bounded by
//! the slugifier, which constrains output to `[A-Za-z0-9_-]` plus a
//! length cap, so a malicious session key can't escape its containing
//! directory.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use eyre::{eyre, Result};

/// Captured per-session task brief. Loaded back into the dispatcher
/// when a fresh UUID is minted so we can recap the original ask
/// instead of asking the user to re-state it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Brief {
    /// Free-form task text the user provided. May be the initial `/cc`
    /// argument or an update captured later in the thread.
    pub task: String,
    /// When the brief was first captured. Useful when the operator is
    /// reading these on disk; not consulted by the dispatcher.
    pub captured_at: DateTime<Utc>,
    /// Latest update timestamp. Equal to `captured_at` until the brief
    /// is replaced.
    pub updated_at: DateTime<Utc>,
}

impl Brief {
    /// One-line summary of the brief, safe to render into a Slack card
    /// header. Truncates at `max` chars and elides whitespace runs.
    pub fn summary(&self, max: usize) -> String {
        let collapsed: String = self.task.split_whitespace().collect::<Vec<_>>().join(" ");
        truncate_chars(&collapsed, max)
    }
}

/// Filesystem-backed brief store rooted at `{data_dir}`. Cheap to
/// clone; holds only a `PathBuf`.
#[derive(Debug, Clone)]
pub struct BriefStore {
    root: PathBuf,
}

impl BriefStore {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            root: data_dir.into(),
        }
    }

    /// Path to the brief file for `session_key`. Public so callers can
    /// log it; do not write directly — go through [`Self::save`] for
    /// atomicity.
    pub fn path_for(&self, session_key: &str) -> PathBuf {
        self.root
            .join("cc-briefs")
            .join(format!("{}.md", slugify(session_key)))
    }

    /// Persist `task` for `session_key`. Idempotent: a second call with
    /// the same key updates `updated_at` while preserving `captured_at`.
    /// Atomic: a partial write can never be observed because we write
    /// to a sibling tempfile and rename.
    pub async fn save(&self, session_key: &str, task: &str) -> Result<()> {
        if task.trim().is_empty() {
            return Err(eyre!(
                "refusing to persist empty brief for session '{session_key}'"
            ));
        }
        let path = self.path_for(session_key);
        let parent = path
            .parent()
            .ok_or_else(|| eyre!("brief path has no parent: {}", path.display()))?;
        workforce_common::private_fs::ensure_private_dir(parent)?;

        let now = Utc::now();
        let captured_at = match self.load(session_key).await {
            Some(prior) => prior.captured_at,
            None => now,
        };

        let body = render_markdown(task, captured_at, now);

        // Per-writer-unique tempfile so concurrent saves don't race on
        // the same `.brief.md.tmp` name (the rename would succeed for
        // one writer and ENOENT the other).
        let tmp = parent.join(format!(
            ".{}.{}.tmp",
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("brief.md"),
            uuid::Uuid::new_v4(),
        ));
        tokio::fs::write(&tmp, body.as_bytes()).await?;
        // rename is atomic within a filesystem on POSIX, which our
        // data dir always is.
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }

    /// Load the brief for `session_key` if one exists. Corrupt files
    /// return `None` and are logged at debug — a brief is a hint,
    /// never load-bearing for correctness.
    pub async fn load(&self, session_key: &str) -> Option<Brief> {
        let path = self.path_for(session_key);
        let raw = match tokio::fs::read_to_string(&path).await {
            Ok(s) => s,
            Err(_) => return None,
        };
        match parse_markdown(&raw) {
            Ok(b) => Some(b),
            Err(e) => {
                tracing::debug!(
                    target: "cc",
                    path = %path.display(),
                    error = %e,
                    "cc brief parse failed; treating as missing"
                );
                None
            }
        }
    }

    /// Remove the brief for `session_key` if it exists. Missing-file
    /// is not an error — callers expect "absent now" after a delete
    /// regardless of starting state.
    pub async fn delete(&self, session_key: &str) -> Result<()> {
        let path = self.path_for(session_key);
        match tokio::fs::remove_file(&path).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(eyre!("delete brief {}: {e}", path.display())),
        }
    }
}

/// Cap a string at `max` *characters* (not bytes), appending '…' if
/// truncation occurred. Mirrors `slack_blocks::truncate` so brief
/// summaries render consistently with other card content.
fn truncate_chars(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

/// Sanitize a key into something safe for a filesystem path component.
/// Allows ASCII alphanumerics + `-`/`_`; everything else collapses to
/// `_`. Length-capped at 120 to keep total path length well under
/// every filesystem's NAME_MAX.
fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(120));
    for c in s.chars().take(120) {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

fn render_markdown(task: &str, captured_at: DateTime<Utc>, updated_at: DateTime<Utc>) -> String {
    // Front-matter is the source of truth for timestamps; the body is
    // the user's verbatim task text, stored without any escaping so
    // operators reading the file see exactly what the user typed.
    format!(
        "---\n\
         schema: cc-brief/v1\n\
         captured_at: {}\n\
         updated_at: {}\n\
         ---\n\
         {}\n",
        captured_at.to_rfc3339(),
        updated_at.to_rfc3339(),
        task,
    )
}

fn parse_markdown(raw: &str) -> Result<Brief> {
    // Front matter is bounded by `^---$` lines. Scan line-by-line so a
    // markdown HR (`---`) inside the user's task — which itself looks
    // identical to the closing fence — doesn't terminate the header
    // parse early and silently truncate the body.
    let mut lines = raw.lines();
    if lines.next() != Some("---") {
        return Err(eyre!("missing front-matter open"));
    }
    let mut header_lines: Vec<&str> = Vec::new();
    let mut closed = false;
    for line in lines.by_ref() {
        if line == "---" {
            closed = true;
            break;
        }
        header_lines.push(line);
    }
    if !closed {
        return Err(eyre!("missing front-matter close"));
    }
    let body = lines.collect::<Vec<_>>().join("\n");

    let mut captured_at: Option<DateTime<Utc>> = None;
    let mut updated_at: Option<DateTime<Utc>> = None;
    let mut schema: Option<&str> = None;
    for line in header_lines {
        let Some((key, val)) = line.split_once(':') else {
            continue;
        };
        let (key, val) = (key.trim(), val.trim());
        match key {
            "schema" => schema = Some(val),
            "captured_at" => {
                captured_at = Some(
                    DateTime::parse_from_rfc3339(val)
                        .map_err(|e| eyre!("bad captured_at '{val}': {e}"))?
                        .with_timezone(&Utc),
                )
            }
            "updated_at" => {
                updated_at = Some(
                    DateTime::parse_from_rfc3339(val)
                        .map_err(|e| eyre!("bad updated_at '{val}': {e}"))?
                        .with_timezone(&Utc),
                )
            }
            _ => {}
        }
    }
    if schema != Some("cc-brief/v1") {
        return Err(eyre!("unknown brief schema: {:?}", schema));
    }
    Ok(Brief {
        task: body.trim_end_matches('\n').to_string(),
        captured_at: captured_at.ok_or_else(|| eyre!("missing captured_at"))?,
        updated_at: updated_at.ok_or_else(|| eyre!("missing updated_at"))?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[cfg(unix)]
    #[tokio::test]
    async fn briefs_live_where_only_the_hub_can_read_them() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let briefs = tmp.path().join("cc-briefs");
        std::fs::create_dir(&briefs).unwrap();
        std::fs::set_permissions(&briefs, std::fs::Permissions::from_mode(0o2775)).unwrap();

        BriefStore::new(tmp.path())
            .save("slack:T1:C1:msg1", "the user's task")
            .await
            .unwrap();

        let mode = std::fs::metadata(&briefs).unwrap().permissions().mode();
        assert_eq!(mode & 0o077, 0);
    }

    #[tokio::test]
    async fn save_then_load_roundtrips_task_text() {
        let tmp = TempDir::new().unwrap();
        let store = BriefStore::new(tmp.path());
        store
            .save("slack:T1:C1:msg1", "summarise the cost-tracking work")
            .await
            .unwrap();
        let b = store.load("slack:T1:C1:msg1").await.unwrap();
        assert_eq!(b.task, "summarise the cost-tracking work");
    }

    #[tokio::test]
    async fn save_is_atomic_under_concurrent_writers() {
        // Two concurrent saves: whichever wins, the file must contain
        // a fully parseable Brief — never the half-written state of
        // either side. Atomicity comes from rename-after-write; this
        // test catches a regression that bypasses it.
        let tmp = TempDir::new().unwrap();
        let store = BriefStore::new(tmp.path());
        let s1 = store.clone();
        let s2 = store.clone();
        let h1 = tokio::spawn(async move {
            s1.save("k", &"a".repeat(50_000)).await.unwrap();
        });
        let h2 = tokio::spawn(async move {
            s2.save("k", &"b".repeat(50_000)).await.unwrap();
        });
        h1.await.unwrap();
        h2.await.unwrap();
        let b = store.load("k").await.unwrap();
        // Whoever won, it's all-a or all-b; never a mixed prefix.
        assert!(b.task == "a".repeat(50_000) || b.task == "b".repeat(50_000));
    }

    #[tokio::test]
    async fn second_save_preserves_captured_at_and_advances_updated_at() {
        let tmp = TempDir::new().unwrap();
        let store = BriefStore::new(tmp.path());
        store.save("k", "first").await.unwrap();
        let first = store.load("k").await.unwrap();
        // Sleep 50ms so the millisecond resolution Utc::now() returns
        // is guaranteed to advance between the two saves.
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        store.save("k", "second").await.unwrap();
        let second = store.load("k").await.unwrap();
        assert_eq!(second.task, "second");
        assert_eq!(second.captured_at, first.captured_at);
        assert!(second.updated_at >= first.updated_at);
    }

    #[tokio::test]
    async fn load_returns_none_when_no_brief_was_saved() {
        let tmp = TempDir::new().unwrap();
        let store = BriefStore::new(tmp.path());
        assert!(store.load("missing").await.is_none());
    }

    #[tokio::test]
    async fn load_returns_none_for_corrupted_file() {
        let tmp = TempDir::new().unwrap();
        let store = BriefStore::new(tmp.path());
        let path = store.path_for("k");
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, b"garbage without front-matter")
            .await
            .unwrap();
        assert!(store.load("k").await.is_none());
    }

    #[tokio::test]
    async fn delete_is_noop_when_file_missing() {
        let tmp = TempDir::new().unwrap();
        let store = BriefStore::new(tmp.path());
        store.delete("missing").await.unwrap();
    }

    #[tokio::test]
    async fn delete_removes_existing_file() {
        let tmp = TempDir::new().unwrap();
        let store = BriefStore::new(tmp.path());
        store.save("k", "x").await.unwrap();
        assert!(store.load("k").await.is_some());
        store.delete("k").await.unwrap();
        assert!(store.load("k").await.is_none());
    }

    #[tokio::test]
    async fn refuses_to_persist_empty_brief() {
        // A `/cc` invocation without an initial task should not save a
        // brief — there's nothing to recap. The dispatcher relies on
        // `load → None` semantics in that case.
        let tmp = TempDir::new().unwrap();
        let store = BriefStore::new(tmp.path());
        assert!(store.save("k", "   ").await.is_err());
        assert!(store.load("k").await.is_none());
    }

    #[test]
    fn slugify_strips_path_traversal_and_specials() {
        // Each non-alnum/`-`/`_` char collapses to one `_` — so
        // `:T1/C1/../etc/passwd` → `_T1_C1_..._etc_passwd` with each of
        // the four `/.` boundary chars becoming an underscore.
        assert_eq!(
            slugify("slack:T1/C1/../etc/passwd"),
            "slack_T1_C1____etc_passwd"
        );
        // Empty string slugifies to a non-empty placeholder so we
        // never produce a bare `.md` filename.
        assert_eq!(slugify(""), "_");
        // Length is capped to bound total path length.
        assert!(slugify(&"x".repeat(500)).len() <= 120);
    }

    #[test]
    fn brief_summary_collapses_whitespace_and_truncates() {
        let b = Brief {
            task: "investigate \n\n  every    place that calls llm".into(),
            captured_at: Utc::now(),
            updated_at: Utc::now(),
        };
        assert_eq!(b.summary(80), "investigate every place that calls llm");
        let long = Brief {
            task: "x ".repeat(200),
            captured_at: Utc::now(),
            updated_at: Utc::now(),
        };
        let s = long.summary(20);
        assert!(s.chars().count() == 20);
        assert!(s.ends_with('…'));
    }

    /// A user's task containing a markdown horizontal rule (`---` on
    /// its own line) used to truncate the round-trip at the HR — the
    /// parser closed the front-matter at the first `\n---\n` and lost
    /// everything after. Now bounded by line-anchored `^---$` so the
    /// body is preserved verbatim.
    #[tokio::test]
    async fn task_containing_markdown_horizontal_rule_round_trips_intact() {
        let tmp = TempDir::new().unwrap();
        let store = BriefStore::new(tmp.path());
        let task = "context\n\n---\n\nactual ask after the HR";
        store.save("k", task).await.unwrap();
        let b = store.load("k").await.unwrap();
        assert_eq!(b.task, task);
    }

    #[tokio::test]
    async fn save_path_is_anchored_under_data_dir() {
        // The session_key is user-influenced (Slack constructs it from
        // workspace+channel+thread) — so a malicious key must not
        // escape the data dir. Slugification handles that; this test
        // pins the property.
        let tmp = TempDir::new().unwrap();
        let store = BriefStore::new(tmp.path());
        let evil = "../../../etc/passwd";
        store.save(evil, "x").await.unwrap();
        let path = store.path_for(evil);
        let canon_root = tmp.path().canonicalize().unwrap();
        let canon_parent = path.parent().unwrap().canonicalize().unwrap();
        assert!(
            canon_parent.starts_with(&canon_root),
            "brief path {} escaped data dir {}",
            canon_parent.display(),
            canon_root.display()
        );
    }
}
