//! Session recall on Desk's line: `recall` and `recall_read`, for a session
//! to search what Desk remembers within its own reach.
//!
//! Its reader is built here from the line's caller and the registry's
//! record, never from the tool's input: its group (as the harvest stamps
//! it), its project (its repository's main folder, as the harvest stamps
//! it) and its work (itself until handoff gives sessions a lineage). What
//! it asks can only narrow that, and the person's `memory.session_reach`
//! can only narrow the install's.
//!
//! The memory switches govern what is kept, not what is read: a session
//! whose own memory is off, in a group whose harvest is off, or recalling
//! while memory is paused still searches what was kept within its reach.
//! Forgetting is what takes rows out of reach.
//!
//! On a machine's engine (`[desk] agents = false`) both are refused: the
//! groups that bound a session's reach are kept by the Desk that drives
//! it, not there, until groups are granted to machines.
//!
//! Every answer reaches the model wrapped as untrusted content
//! ([`crate::memory::render`]); nothing of what was recalled is outside
//! the wrapping. Every read is kept in `memory_reads` as `session:<id>`.

use serde_json::{json, Value};
use uuid::Uuid;

use super::memory_recall::Recall;
use super::{session_item, Desk, DeskError, UNGROUPED};
use crate::memory::passage::PassageKind;
use crate::memory::reach::{bound, may_see, one_group, Reach, Reader, Row};
use crate::memory::record::Asker;
use crate::memory::render::{render, Rendered};
use crate::memory_index_store::RecallRow;
use crate::memory_store::KeptExchange;

/// How much of a hit `recall` shows, in characters.
pub const EXCERPT: usize = 400;

/// What a session reads for a ref it may not read, whether the ref is
/// malformed, unknown, or another's: the same words, so nothing can be
/// learnt by trying.
fn not_yours(reference: &str) -> String {
    format!("Nothing you may read has the ref {reference:?}; use a ref recall gave you.")
}

const UNREACHABLE: &str = "Desk can't be reached right now; carry on without it.";

/// How `recall`'s answer names a reach.
fn reach_word(r: Reach) -> &'static str {
    match r {
        Reach::Work => "work",
        Reach::Project => "project",
        Reach::Group => "group",
    }
}

/// `text` cut to `max` characters, ending with "…" when it was cut.
fn excerpt(text: &str, max: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    match text.chars().count() > max {
        true => format!(
            "{}…",
            text.chars().take(max.saturating_sub(1)).collect::<String>()
        ),
        false => text,
    }
}

/// A recalled row's kind, as a model reads it.
fn kind_word(row: &RecallRow) -> &'static str {
    match (&row.item, row.compaction) {
        (Some(i), _) => i.kind.as_str(),
        (None, true) => "compaction",
        (None, false) => "exchange",
    }
}

/// What a recalled row says, in one block.
fn row_text(row: &RecallRow) -> String {
    match (&row.item, row.compaction) {
        (Some(_), _) => row.answer.clone(),
        (None, true) => format!("Summary: {}", row.answer),
        (None, false) => format!("Asked: {}  Answered: {}", row.ask, row.answer),
    }
}

/// Who asked, as a full exchange says it.
fn asked_by(a: Asker) -> &'static str {
    match a {
        Asker::Person => "Asked by the person",
        Asker::Desk => "Asked by Desk",
        Asker::Session => "Asked by another session",
        Asker::Unknown => "Asked",
    }
}

/// An exchange in full, as `recall_read` gives it.
fn full_text(x: &KeptExchange) -> String {
    let mut out = format!("{}: {}\n", asked_by(x.asker), x.ask);
    if !x.steps.is_empty() {
        out.push_str("Steps:\n");
        for s in &x.steps {
            let failed = if s.failed { " (failed)" } else { "" };
            out.push_str(&format!(
                "- {} {} → {}{failed}\n",
                s.tool, s.input, s.result
            ));
        }
    }
    for (what, list) in [
        ("Files", &x.files),
        ("Commands", &x.commands),
        ("Errors", &x.errors),
    ] {
        if !list.is_empty() {
            out.push_str(&format!("{what}: {}\n", list.join(", ")));
        }
    }
    match x.compaction {
        true => out.push_str(&format!("Summary: {}", x.answer)),
        false => out.push_str(&format!("Answered: {}", x.answer)),
    }
    if x.redacted > 0 {
        out.push_str(&format!("\n({} secrets hidden)", x.redacted));
    }
    out
}

/// A fresh nonce for one answer's wrapping.
fn nonce() -> String {
    Uuid::new_v4().simple().to_string()[..16].to_string()
}

/// A session and engine's names, by session.
type Names = std::collections::HashMap<Uuid, (String, String)>;

impl Desk {
    /// The session `caller` as a reader of session memory, from the
    /// registry: its group and project as the harvest stamps them, and its
    /// work. The hub's own runs read none.
    pub(super) async fn session_reader(&self, caller: Uuid) -> Result<Reader, String> {
        // On a machine's engine every session looks ungrouped (their groups
        // are kept by the Desk that drives it), so a group bound here would
        // be no client boundary.
        if self.0.opts.memory.machine {
            return Err(crate::memory::MACHINE_RECALL.into());
        }
        let r = self
            .0
            .state
            .lock()
            .records
            .get(&caller)
            .cloned()
            .ok_or_else(|| "Desk doesn't know this session.".to_string())?;
        if self.0.opts.hub_internal.holds(std::path::Path::new(&r.cwd))
            || crate::ledger::agent_turn(&r.cwd, &r.name)
        {
            return Err("What Desk remembers of sessions isn't open to the hub's own runs.".into());
        }
        let group = self
            .group_of_member(&session_item(caller))
            .filter(|g| g != UNGROUPED);
        let project = self.project_of(&r).await;
        Ok(Reader::Session {
            id: caller,
            group,
            project,
            // Its lineage, once handoff gives it one; until then, itself.
            work: vec![caller],
        })
    }

    /// The names of `sessions` and their engines, as a hit shows them.
    async fn hit_names(&self, sessions: &[Uuid]) -> Result<Names, DeskError> {
        Ok(self
            .0
            .store
            .memory_names(sessions)
            .await?
            .into_iter()
            .map(|(id, name, engine)| (id, (name, engine.title().to_string())))
            .collect())
    }

    /// `recall`, for the session `caller`.
    pub(super) async fn recall_call(
        &self,
        caller: Uuid,
        query: String,
        reach: Option<Reach>,
        kinds: Option<Vec<PassageKind>>,
        limit: usize,
    ) -> Result<Value, String> {
        let reader = self.session_reader(caller).await?;
        let setting = self.session_reach().await;
        let used = reach.map_or(setting, |a| a.min(setting));
        let found = self
            .memory_recall(
                &reader,
                &Recall {
                    q: query,
                    reach,
                    kinds,
                    limit,
                    tool: "recall",
                    ..Default::default()
                },
            )
            .await
            .map_err(|e| self.line_memory_failed(caller, e))?;
        let mut notes: Vec<String> = Vec::new();
        if reach.is_some_and(|a| a > setting) {
            notes.push(format!(
                "You asked to look across your {}; Desk lets you look only as far as your {}, so that is what was searched.",
                reach.map_or("", reach_word),
                reach_word(setting)
            ));
        }
        notes.extend(found.notice.map(str::to_string));
        let reach = reach_word(used);
        if found.hits.is_empty() {
            notes.push(format!(
                "Nothing Desk remembers within your {reach} matches."
            ));
            return Ok(json!({ "found": 0, "reach": reach, "note": notes.join(" ") }));
        }
        let mut sessions: Vec<Uuid> = found.hits.iter().map(|(r, _)| r.session).collect();
        sessions.sort();
        sessions.dedup();
        let names = self
            .hit_names(&sessions)
            .await
            .map_err(|e| self.line_memory_failed(caller, e))?;
        let refs: Vec<String> = found.hits.iter().map(|(r, _)| reference_of(r)).collect();
        let texts: Vec<String> = found
            .hits
            .iter()
            .map(|(r, _)| excerpt(&row_text(r), EXCERPT))
            .collect();
        let hits: Vec<Rendered> = found
            .hits
            .iter()
            .zip(refs.iter().zip(&texts))
            .map(|((r, _), (reference, text))| rendered(r, &names, reference, text))
            .collect();
        Ok(json!({
            "recalled": render(&hits, &nonce()),
            "found": hits.len(),
            "reach": reach,
            "note": (!notes.is_empty()).then(|| notes.join(" ")),
        }))
    }

    /// `recall_read`, for the session `caller`: the exchange or the note
    /// `reference` names, when it is within the session's reach.
    pub(super) async fn recall_read_call(
        &self,
        caller: Uuid,
        reference: String,
    ) -> Result<Value, String> {
        let reader = self.session_reader(caller).await?;
        let known = self
            .0
            .store
            .memory_groups()
            .await
            .map_err(|e| self.line_memory_failed(caller, e.into()))?;
        let b = bound(&reader, self.session_reach().await, None, &known);
        let Some((is_item, id)) = parse_reference(&reference) else {
            return Err(not_yours(&reference));
        };
        let sees = |group: Option<&str>, session: Uuid, project: &str, at| {
            may_see(
                &reader,
                &b,
                &Row {
                    group,
                    session,
                    project,
                    at,
                },
            )
        };
        let failed = |e: eyre::Report| self.line_memory_failed(caller, e.into());
        // What is shown, first the one asked for: each a row's facts, its
        // kind, flags and text.
        struct Shown {
            id: Uuid,
            reference: String,
            session: Uuid,
            group: Option<String>,
            at: chrono::DateTime<chrono::Utc>,
            kind: &'static str,
            noted: bool,
            flagged: bool,
            superseded: bool,
            text: String,
        }
        let mut shown: Vec<Shown> = Vec::new();
        if is_item {
            let item = self
                .0
                .store
                .memory_item(id)
                .await
                .map_err(failed)?
                .filter(|i| sees(i.group.as_deref(), i.session, &i.project, i.valid_from))
                .ok_or_else(|| not_yours(&reference))?;
            let mut text = item.text.clone();
            if !item.source_kept {
                text.push_str("\n(The conversation it came from is no longer kept.)");
            }
            shown.push(Shown {
                id: item.id,
                reference: reference.clone(),
                session: item.session,
                group: item.group.clone(),
                at: item.valid_from,
                kind: item.kind.as_str(),
                noted: item.by != "person",
                flagged: false,
                superseded: item.valid_to.is_some(),
                text,
            });
            let sources = self
                .0
                .store
                .recall_rows(&item.sources)
                .await
                .map_err(failed)?;
            for s in sources
                .into_iter()
                .filter(|s| s.item.is_none())
                .filter(|s| sees(s.group.as_deref(), s.session, &s.project, s.at))
            {
                shown.push(Shown {
                    id: s.id,
                    reference: reference_of(&s),
                    session: s.session,
                    group: s.group.clone(),
                    at: s.at,
                    kind: kind_word(&s),
                    noted: false,
                    flagged: s.flagged,
                    superseded: false,
                    text: format!("Source. {}", row_text(&s)),
                });
            }
        } else {
            let x = self
                .0
                .store
                .memory_exchange(id)
                .await
                .map_err(failed)?
                .filter(|x| sees(x.group.as_deref(), x.session, &x.project, x.exchange.at))
                .ok_or_else(|| not_yours(&reference))?;
            shown.push(Shown {
                id: x.id,
                reference: reference.clone(),
                session: x.session,
                group: x.group.clone(),
                at: x.exchange.at,
                kind: match x.exchange.compaction {
                    true => "compaction",
                    false => "exchange",
                },
                noted: false,
                flagged: x.flagged,
                superseded: false,
                text: full_text(&x.exchange),
            });
        }
        let mut sessions: Vec<Uuid> = shown.iter().map(|s| s.session).collect();
        sessions.sort();
        sessions.dedup();
        let names = self
            .hit_names(&sessions)
            .await
            .map_err(|e| self.line_memory_failed(caller, e))?;
        let ids: Vec<Uuid> = shown.iter().map(|s| s.id).collect();
        let group = one_group(shown.iter().map(|s| s.group.as_deref()));
        self.0
            .store
            .memory_read(
                &reader.label(),
                "recall_read",
                Some(&reference),
                &ids,
                group.as_deref(),
            )
            .await
            .map_err(failed)?;
        let hits: Vec<Rendered> = shown
            .iter()
            .map(|s| {
                let (session, engine) = name_of(&names, s.session);
                Rendered {
                    kind: s.kind,
                    noted: s.noted,
                    session,
                    engine,
                    at: s.at,
                    reference: &s.reference,
                    flagged: s.flagged,
                    superseded: s.superseded,
                    text: &s.text,
                }
            })
            .collect();
        Ok(json!({ "recalled": render(&hits, &nonce()) }))
    }

    /// The words a session reads when session memory fails it, logged.
    fn line_memory_failed(&self, caller: Uuid, e: DeskError) -> String {
        match e {
            DeskError::Invalid(said) => said,
            e => {
                tracing::warn!(session = %caller, error = %e, "a session's recall failed");
                UNREACHABLE.into()
            }
        }
    }
}

/// The ref `recall_read` takes for `row`: `i-` a note's, `x-` an
/// exchange's.
fn reference_of(row: &RecallRow) -> String {
    match row.item {
        Some(_) => format!("i-{}", row.id),
        None => format!("x-{}", row.id),
    }
}

/// A ref read back: whether it is a note's, and its id.
fn parse_reference(r: &str) -> Option<(bool, Uuid)> {
    let r = r.trim();
    let (is_item, id) = match r.split_at_checked(2)? {
        ("i-", id) => (true, id),
        ("x-", id) => (false, id),
        _ => return None,
    };
    Some((is_item, Uuid::parse_str(id).ok()?))
}

/// The names `names` holds for `session`.
fn name_of(names: &Names, session: Uuid) -> (&str, &str) {
    names
        .get(&session)
        .map_or(("a session", "unknown engine"), |(n, e)| {
            (n.as_str(), e.as_str())
        })
}

/// One recalled row as a model reads it.
fn rendered<'a>(
    row: &'a RecallRow,
    names: &'a Names,
    reference: &'a str,
    text: &'a str,
) -> Rendered<'a> {
    let (session, engine) = name_of(names, row.session);
    Rendered {
        kind: kind_word(row),
        noted: row.item.as_ref().is_some_and(|i| i.noted),
        session,
        engine,
        at: row.at,
        reference,
        flagged: row.flagged,
        superseded: row.item.as_ref().is_some_and(|i| i.superseded),
        text,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refs_read_back_strictly() {
        let id = Uuid::from_u128(7);
        assert_eq!(parse_reference(&format!("x-{id}")), Some((false, id)));
        assert_eq!(parse_reference(&format!(" i-{id} ")), Some((true, id)));
        for bad in ["", "x-", "x-nope", &format!("y-{id}"), &id.to_string(), "é"] {
            assert_eq!(parse_reference(bad), None, "{bad:?}");
        }
    }

    #[test]
    fn an_excerpt_is_cut_by_characters() {
        let long = "é".repeat(EXCERPT + 5);
        let cut = excerpt(&long, EXCERPT);
        assert_eq!(cut.chars().count(), EXCERPT);
        assert!(cut.ends_with('…'));
        assert_eq!(excerpt("a\n  b", EXCERPT), "a b");
    }
}
