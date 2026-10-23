//! What the graph keeps of a repo's vectors in the vector store: how far a
//! fill of them has got, and which files' vectors may be behind.

use serde::Serialize;

/// A summary as the graph stores it, with the file its entity is in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredSummary {
    pub entity_id: String,
    pub granularity: String,
    pub file_path: String,
    pub text: String,
    pub is_stale: bool,
}

impl StoredSummary {
    /// Where a page of summaries after this one starts.
    pub fn cursor(&self) -> String {
        format!("{}{CURSOR_PARTS}{}", self.entity_id, self.granularity)
    }
}

/// What parts a summary cursor's entity id from its granularity.
const CURSOR_PARTS: char = '\u{1}';

/// A summary cursor's entity id and granularity; both empty for the start.
pub(crate) fn summary_cursor(cursor: &str) -> (&str, &str) {
    cursor.split_once(CURSOR_PARTS).unwrap_or((cursor, ""))
}

/// What a fill is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FillStage {
    /// Storing the repo's entities, a page at a time in id order.
    Entities,
    /// Storing its summaries, likewise.
    Summaries,
    /// Removing what the store holds of the repo from before the fill.
    Sweep,
    /// Finished.
    Done,
}

impl FillStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Entities => "entities",
            Self::Summaries => "summaries",
            Self::Sweep => "sweep",
            Self::Done => "done",
        }
    }

    pub fn parse(stage: &str) -> Option<Self> {
        [Self::Entities, Self::Summaries, Self::Sweep, Self::Done]
            .into_iter()
            .find(|known| known.as_str() == stage)
    }
}

/// How far the fill of a repo's vectors has got: what a fill stopped part
/// of the way goes on from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VectorFill {
    /// Stamped on every vector written for the repo since the fill began.
    pub generation: String,
    /// The making of the store's collection being filled.
    pub collection: String,
    pub stage: FillStage,
    /// The last entity, or summary, of the stage stored; empty before the
    /// first.
    pub cursor: String,
    /// Entities and summaries stored so far.
    pub done: u64,
    /// Entities and summaries there were when the fill began.
    pub total: u64,
}

/// A file noted as pending, and the note's sequence.
pub type PendingFile = (String, i64);
