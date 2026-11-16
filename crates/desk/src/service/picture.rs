//! The bosses' one live picture ([`crate::picture`]) as the desk keeps it:
//! each row added in memory the moment it happens (the person's prompt
//! into a session is there before its hook returns), then written to
//! Postgres; read back once after a start.

use chrono::Utc;
use uuid::Uuid;

use super::Desk;
use crate::picture::{Happened, Note, What};

impl Desk {
    /// Adds `note` to the picture now; the row it became.
    pub fn picture_note(&self, note: Note) -> Option<Happened> {
        let (row, write) = {
            let mut p = self.0.picture.lock();
            let row = p.push(note, Utc::now())?;
            (row, p.loaded())
        };
        if write {
            self.picture_write(vec![row.clone()]);
        }
        Some(row)
    }

    fn picture_write(&self, rows: Vec<Happened>) {
        let Ok(rt) = tokio::runtime::Handle::try_current() else {
            return;
        };
        let store = self.0.store.clone();
        rt.spawn(async move {
            for r in rows {
                if let Err(e) = store.picture_put(&r).await {
                    tracing::warn!(
                        error = format!("{e:#}"),
                        seq = r.seq,
                        "couldn't keep a picture row"
                    );
                }
            }
        });
    }

    /// Reads back what Postgres kept of the picture, once: what was said
    /// here before is numbered after it and written. False when it
    /// couldn't be read: nothing is written until it is (call again).
    /// One writer per database: the numbers are this process's.
    pub async fn picture_load(&self) -> bool {
        if self.0.picture.lock().loaded() {
            return true;
        }
        match self.0.store.picture_load(Utc::now()).await {
            Ok((rows, top)) => {
                let pending = {
                    let mut p = self.0.picture.lock();
                    if p.loaded() {
                        return true;
                    }
                    p.load(rows, top)
                };
                self.picture_write(pending);
                true
            }
            Err(e) => {
                tracing::warn!(
                    error = format!("{e:#}"),
                    "couldn't read the bosses' picture back"
                );
                false
            }
        }
    }

    /// The newest row's number: what a boss's turn starting now decides on.
    pub fn picture_seq(&self) -> u64 {
        self.0.picture.lock().seq()
    }

    /// `session`'s version for `boss`.
    pub fn picture_version(&self, boss: &str, session: Uuid) -> u64 {
        self.0.picture.lock().version(boss, session)
    }

    /// What touched `session` for `boss` after `basis`, leaving out what
    /// the turn `turn` did itself.
    pub fn picture_since(
        &self,
        boss: &str,
        session: Uuid,
        basis: u64,
        turn: Option<&str>,
    ) -> Vec<Happened> {
        self.0.picture.lock().since(boss, session, basis, turn)
    }

    /// The newest `n` rows touching `session` for `boss`.
    pub fn picture_recent(&self, boss: &str, session: Uuid, n: usize) -> Vec<Happened> {
        self.0.picture.lock().recent(boss, session, n)
    }

    /// The newest `n` rows of `boss`'s across its scope (its DM, facts,
    /// orders).
    pub fn picture_recent_for(&self, boss: &str, n: usize) -> Vec<Happened> {
        self.0.picture.lock().recent_for(boss, n)
    }

    /// The last instruction `boss` sent `session`, while kept.
    pub fn picture_last_instruction(&self, boss: &str, session: Uuid) -> Option<Happened> {
        self.0.picture.lock().last_instruction(boss, session)
    }

    /// Whether `m`, a boss's message waiting to go into its session, was
    /// overtaken by the person's own words there since it was written: it
    /// is withdrawn, never delivered, and the boss decides again on what
    /// they said (its picture has it).
    pub(super) async fn line_overtaken(&self, m: &crate::line::LineMessage) -> bool {
        let Some(s) = m.to_session() else {
            return false;
        };
        if m.from.kind != crate::crew::PartyKind::Manager
            || m.kind != crate::line::LineKind::Tell
            || !self.0.picture.lock().person_since(s, m.at)
        {
            return false;
        }
        match self.line_withdraw(m.id).await {
            Ok(Some(_)) => {
                tracing::info!(session = %s, message = %m.id, "a boss's message the person's own words overtook was withdrawn");
                // Its boss knows it never went in.
                self.picture_note(
                    Note::new(What::Withdrawn, m.text.clone())
                        .boss(&m.from.id)
                        .session(s),
                );
                true
            }
            Ok(None) => false,
            Err(e) => {
                tracing::warn!(session = %s, error = %e, "couldn't withdraw an overtaken message");
                false
            }
        }
    }

    /// The person's own words into session `id` (a prompt, a message from
    /// Desk, an answer to its question).
    pub(super) fn picture_person_said(&self, id: Uuid, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        self.picture_note(Note::new(What::PersonSaid, text).session(id));
    }

    /// The person answered session `id`'s question from Desk.
    pub(super) fn picture_person_answered(&self, id: Uuid, text: &str) {
        if text.trim().is_empty() {
            return;
        }
        self.picture_note(Note::new(What::PersonAnswered, text).session(id));
    }

    /// Session `id` wrote to its boss, or asked something.
    pub(super) fn picture_session_said(&self, id: Uuid, what: What, text: &str) {
        self.picture_note(Note::new(what, text).session(id));
    }
}

#[cfg(test)]
#[path = "picture_tests.rs"]
mod tests;
