//! Why Desk didn't send a boss's order, and of what kind: what acts on a
//! refusal reads its kind, never its words. The words are the boss's (the
//! tool result it reads, unchanged); the person, when it reaches them, is
//! told [`Refusal::for_person`].

/// Why Desk didn't send a boss's order: its words for the boss, and its
/// kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Refused {
    pub kind: Refusal,
    pub why: String,
}

/// What kind of refusal it was.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Refusal {
    /// The picture moved since it was decided (`super::picture`): decided
    /// again on it.
    Stale,
    /// The person took the session back: it is theirs, and they know.
    TakenBack,
    /// It looks like it starts something that can't be taken back, and it
    /// declares no job.
    NoJob,
    /// Its job is said wrongly, or not fully: its kind or key missing, its
    /// repository unknown, a move that can't be, an assignment not open.
    JobUnclear,
    /// It looks like a job another session holds, or would move one that
    /// may have started.
    JobHeld,
    /// Desk couldn't record who holds its job just now.
    Unrecorded,
    /// Anything else: the session gone, its line refusing it, Desk failing.
    Other,
}

impl Refusal {
    /// Whether the boss can fix it itself, saying the order again with
    /// what its full turn knows (its job, a move, or simply once more):
    /// never the person's to hear first.
    pub(crate) fn boss_fixes(self) -> bool {
        matches!(
            self,
            Self::NoJob | Self::JobUnclear | Self::JobHeld | Self::Unrecorded
        )
    }

    /// Why it wasn't sent, in the person's words: no tool, no field, nothing
    /// for the boss to do. Said after "it wasn't sent: ".
    pub(crate) fn for_person(self) -> &'static str {
        match self {
            Self::NoJob => "it looks like it starts something that can't be taken back, and I couldn't say which job it is, so Desk held it back to keep two sessions from both doing it",
            Self::JobUnclear => "I couldn't say for sure which job it is, so Desk held it back to keep two sessions from both doing it",
            Self::JobHeld => "it looks like work another session already has, so Desk held it back to keep two sessions from both doing it",
            Self::Unrecorded => "Desk couldn't record who has the work just now",
            Self::Stale | Self::TakenBack | Self::Other => "Desk wouldn't send it",
        }
    }
}

impl Refused {
    pub(crate) fn new(kind: Refusal, why: impl Into<String>) -> Self {
        Self {
            kind,
            why: why.into(),
        }
    }

    pub(crate) fn other(why: impl Into<String>) -> Self {
        Self::new(Refusal::Other, why)
    }
}

impl std::fmt::Display for Refused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.why)
    }
}

/// The boss reads it as it always did.
impl From<Refused> for String {
    fn from(r: Refused) -> Self {
        r.why
    }
}

#[cfg(test)]
mod tests {
    use super::Refusal;

    #[test]
    fn the_person_is_told_plain_words_and_only_the_bosses_own_are_its_to_fix() {
        let all = [
            Refusal::Stale,
            Refusal::TakenBack,
            Refusal::NoJob,
            Refusal::JobUnclear,
            Refusal::JobHeld,
            Refusal::Unrecorded,
            Refusal::Other,
        ];
        for k in all {
            let said = k.for_person();
            assert!(
                !said.contains('{') && !said.contains("job:") && !said.contains("crew."),
                "{k:?}: {said}"
            );
        }
        let fixes: Vec<Refusal> = all.into_iter().filter(|k| k.boss_fixes()).collect();
        assert_eq!(
            fixes,
            [
                Refusal::NoJob,
                Refusal::JobUnclear,
                Refusal::JobHeld,
                Refusal::Unrecorded
            ]
        );
    }
}
