//! Per-user relationship tracking — an optional, private subsystem that scores
//! how each human reacts to an agent over time and surfaces it back to the agent
//! for acknowledgement.
//!
//! The pure [`scoring`] floor scores each turn from the user's own words —
//! reactions directed at the agent, never the emotional charge of the topic.
//! [`capture`] persists noteworthy turns; [`backfill`] is the periodic
//! catch-up; [`summary`] rolls events up with wall-clock decay; [`tool`]
//! surfaces a caller's own summary on demand.

pub mod backfill;
pub mod capture;
pub mod scoring;
pub mod summary;
pub mod tool;

pub use capture::capture_turn;
pub use scoring::{score, Reaction, RelationshipRead};
pub use summary::{format_relationship_for_prompt, summarise, RelationshipSummary};
pub use tool::RelationshipStatusTool;
