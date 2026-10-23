//! The tier a claim is verified at.

use serde::Deserialize;
use serde::Serialize;

/// How well-grounded a claim is against the source.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Tier {
    Verified,
    Contradicted,
    Unverified,
}

/// A tier as a model wrote it. Anything unrecognised fails safe to
/// [`Tier::Unverified`] — never a silent verified.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum RawTier {
    Verified,
    Contradicted,
    Unverified,
    #[default]
    #[serde(other)]
    Unknown,
}

impl From<RawTier> for Tier {
    fn from(raw: RawTier) -> Self {
        match raw {
            RawTier::Verified => Tier::Verified,
            RawTier::Contradicted => Tier::Contradicted,
            RawTier::Unverified | RawTier::Unknown => Tier::Unverified,
        }
    }
}
