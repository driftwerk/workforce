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
    /// Comparing the repo's entities with what a store already holding
    /// them has, a page at a time, and writing only what differs.
    AdoptEntities,
    /// Removing what the store holds of the repo's entities that the graph
    /// has not.
    AdoptStrayEntities,
    /// As [`Self::AdoptEntities`], of summaries.
    AdoptSummaries,
    /// As [`Self::AdoptStrayEntities`], of summaries.
    AdoptStraySummaries,
}

impl FillStage {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Entities => "entities",
            Self::Summaries => "summaries",
            Self::Sweep => "sweep",
            Self::Done => "done",
            Self::AdoptEntities => "adopt_entities",
            Self::AdoptStrayEntities => "adopt_stray_entities",
            Self::AdoptSummaries => "adopt_summaries",
            Self::AdoptStraySummaries => "adopt_stray_summaries",
        }
    }

    pub fn parse(stage: &str) -> Option<Self> {
        [
            Self::Entities,
            Self::Summaries,
            Self::Sweep,
            Self::Done,
            Self::AdoptEntities,
            Self::AdoptStrayEntities,
            Self::AdoptSummaries,
            Self::AdoptStraySummaries,
        ]
        .into_iter()
        .find(|known| known.as_str() == stage)
    }

    /// Whether this is a stage of an adoption: a store already holding
    /// the repo compared with the graph and mended, never swept.
    pub fn adopting(self) -> bool {
        matches!(
            self,
            Self::AdoptEntities
                | Self::AdoptStrayEntities
                | Self::AdoptSummaries
                | Self::AdoptStraySummaries
        )
    }

    /// Whether this is a stage of a fill whose sweep removes, when it
    /// ends, whatever of the repo lacks its generation.
    pub fn sweeps(self) -> bool {
        matches!(self, Self::Entities | Self::Summaries | Self::Sweep)
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
    /// Entities and summaries stored so far; of an adoption, the graph's
    /// compared so far.
    pub done: u64,
    /// Entities and summaries the graph had when the fill, or adoption,
    /// began.
    pub total: u64,
}

/// A file noted as pending, and the note's mark.
pub type PendingFile = (String, i64);

#[cfg(test)]
mod tests {
    use super::FillStage;

    const ALL: [FillStage; 8] = [
        FillStage::Entities,
        FillStage::Summaries,
        FillStage::Sweep,
        FillStage::Done,
        FillStage::AdoptEntities,
        FillStage::AdoptStrayEntities,
        FillStage::AdoptSummaries,
        FillStage::AdoptStraySummaries,
    ];

    /// Every stage is read back as it is written; the four of an adoption
    /// are adopting and sweep nothing, and only a fill's own sweep.
    #[test]
    fn fill_stages_round_trip() {
        for stage in ALL {
            assert_eq!(FillStage::parse(stage.as_str()), Some(stage));
            assert_eq!(
                serde_json::to_value(stage).unwrap(),
                serde_json::json!(stage.as_str())
            );
        }
        let adopting: Vec<FillStage> = ALL.into_iter().filter(|s| s.adopting()).collect();
        assert_eq!(adopting, &ALL[4..]);
        let sweeps: Vec<FillStage> = ALL.into_iter().filter(|s| s.sweeps()).collect();
        assert_eq!(sweeps, &ALL[..3]);
        assert_eq!(FillStage::parse("adopt"), None);
    }
}
