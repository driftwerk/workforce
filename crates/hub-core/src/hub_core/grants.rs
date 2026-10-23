//! The standing of a skill grant, as the gates and the pipelines read it.

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum GrantStatus {
    Valid,
    Stale,
    Missing,
}
