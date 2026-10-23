//! Re-exports the trust taxonomy from `workforce-common`. The canonical
//! source lives there so persistence types (`SessionMessage`) can reference
//! `TrustLevel` without dragging the agent runtime into the common crate.

pub use workforce_common::trust::{
    classify_trust, classify_trust_with_role, ContentEnvelope, ContentSource, TrustLevel,
};
