//! Whose part of Desk an overseer oversees: everything ungrouped (and every
//! sidebar group with no boss of its own), or one sidebar group. In
//! `workforce_core` because the hub's `[oversight] group`
//! (`workforce-common`) and Desk's rule for what a scope sees
//! (`workforce-desk`'s `scope`) both name it.

use serde::{Deserialize, Serialize};

/// One overseer's part of Desk.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Default, Serialize, Deserialize)]
#[serde(tag = "kind", content = "group", rename_all = "snake_case")]
pub enum Scope {
    /// Everything in no group with a boss of its own: the main overseer's.
    #[default]
    Ungrouped,
    /// One sidebar group, by its id: that group's boss's.
    Group(String),
}

impl Scope {
    /// The scope of `group`'s boss, or the main overseer's for none.
    pub fn of(group: Option<&str>) -> Self {
        match group {
            Some(g) => Self::Group(g.to_string()),
            None => Self::Ungrouped,
        }
    }

    /// The group it is, if one.
    pub fn group(&self) -> Option<&str> {
        match self {
            Self::Ungrouped => None,
            Self::Group(g) => Some(g),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_scope_is_ungrouped_or_one_group_by_id() {
        assert_eq!(Scope::of(None), Scope::Ungrouped);
        assert_eq!(Scope::of(Some("g-acme")).group(), Some("g-acme"));
        assert_eq!(Scope::default(), Scope::Ungrouped);
        assert_eq!(
            serde_json::to_value(Scope::of(Some("g-acme"))).unwrap(),
            serde_json::json!({ "kind": "group", "group": "g-acme" })
        );
        assert_eq!(
            serde_json::to_value(Scope::Ungrouped).unwrap(),
            serde_json::json!({ "kind": "ungrouped" })
        );
    }
}
