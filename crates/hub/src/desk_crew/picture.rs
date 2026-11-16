//! The bosses' one live picture on the hub's side
//! ([`workforce_desk::picture`]; designs/desk-boss-one-picture.md): what
//! the person says to a boss in its DM (or another boss relays as theirs),
//! each instruction any boss's turn sends a session, and the person's
//! standing orders, written to the picture as they happen. Desk writes the
//! rest: the person's words into a session, and what a session says on
//! its line.

use uuid::Uuid;
use workforce_desk::picture::{Note, What};

use super::CrewHub;

impl CrewHub {
    /// The person's own words to `boss` in its DM, as they reach it:
    /// before its turn on them starts, so every turn of the boss's already
    /// running on what it knew before is decided again.
    pub(super) fn picture_person_told(&self, boss: &str, words: &str) {
        if words.trim().is_empty() {
            return;
        }
        self.desk
            .picture_note(Note::new(What::PersonSaid, words).boss(boss));
    }

    /// `boss` told `session` `text` from its turn `turn` (none: not from a
    /// turn of its own, as when the person accepts a suggestion).
    pub(super) fn picture_instruction(
        &self,
        boss: &str,
        session: Uuid,
        text: &str,
        turn: Option<String>,
    ) {
        self.desk.picture_note(
            Note::new(What::Instruction, text)
                .boss(boss)
                .session(session)
                .turn(turn),
        );
    }

    /// One of the person's standing orders was kept, changed, paused or
    /// forgotten (`what`, in plain words): every boss's every session.
    pub(crate) fn picture_order(&self, what: &str, turn: Option<String>) {
        self.desk
            .picture_note(Note::new(What::Order, what).turn(turn));
    }

    /// The turn of the boss's this runs in: its runner session's id.
    pub(super) fn current_turn() -> Option<String> {
        workforce_agent::runner::CURRENT_SESSION_ID
            .try_with(Clone::clone)
            .ok()
    }
}
