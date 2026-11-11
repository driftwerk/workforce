//! What a boss served from a grant may see of the group's sessions, by the
//! grant's Sees (remote-bosses design §8.2): what each part of a crew
//! reading is, and whether it leaves this engine. Pure; the hub's served
//! readings ask it of every part they put in, and redact and cap what
//! goes.

use crate::grant::Sees;

/// A part of what a crew reading says of a session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Part {
    /// Its name, state, folder, goal, since when, who runs it.
    Status,
    /// That it asks something, and what: the person's or the boss's to
    /// answer.
    Question,
    /// Its last prompt and last reply.
    Reply,
    /// Its terminal's text.
    Terminal,
    /// The activity's reasons and texts.
    Words,
}

/// Whether `part` is shown at `sees`.
pub fn shows(sees: Sees, part: Part) -> bool {
    match part {
        Part::Status => true,
        Part::Question | Part::Reply | Part::Words => {
            matches!(sees, Sees::Replies | Sees::Everything)
        }
        Part::Terminal => sees == Sees::Everything,
    }
}

/// The most characters one served result's text may hold.
pub const MAX_CHARS: usize = 16_000;
/// The most terminal rows a served result may hold.
pub const MAX_ROWS: usize = 200;

/// `text` cut to its last `MAX_ROWS` rows.
pub fn last_rows(text: &str) -> String {
    let rows: Vec<&str> = text.lines().collect();
    rows[rows.len().saturating_sub(MAX_ROWS)..].join("\n")
}

/// `text` cut to `max` characters, said so.
pub fn capped(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let kept: String = text.chars().take(max.saturating_sub(40)).collect();
    format!("{kept}\n[cut: more than {max} characters]")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_shows_only_status_and_everything_alone_shows_the_terminal() {
        for p in [Part::Question, Part::Reply, Part::Words, Part::Terminal] {
            assert!(!shows(Sees::Status, p), "{p:?}");
        }
        assert!(shows(Sees::Status, Part::Status));
        assert!(shows(Sees::Replies, Part::Reply));
        assert!(!shows(Sees::Replies, Part::Terminal));
        assert!(shows(Sees::Everything, Part::Terminal));
    }

    #[test]
    fn terminals_and_texts_are_capped() {
        let t: String = (0..500).map(|i| format!("row {i}\n")).collect();
        let kept = last_rows(&t);
        assert_eq!(kept.lines().count(), MAX_ROWS);
        assert!(kept.ends_with("row 499"));
        let long = "x".repeat(MAX_CHARS + 10);
        let c = capped(&long, MAX_CHARS);
        assert!(c.chars().count() <= MAX_CHARS + 1);
        assert!(c.contains("[cut:"));
        assert_eq!(capped("short", MAX_CHARS), "short");
    }
}
