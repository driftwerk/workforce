//! What a collection asks the store for.
//!
//! A collection with no descriptor is `small`: it shares the default partition,
//! uses the indexes every collection shares, and declares nothing. That is what
//! a ten-row app wants and it costs nothing.
//!
//! A collection declared `large` is given a partition of its own, and the
//! indexes it names are built on that partition alone — so its write cost and
//! its planning cost belong to it, and no other collection pays them.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{validate_field, StoreError};

/// How many indexes one collection may declare. Each one is paid for on every
/// write to that collection, so the cap is what stops a descriptor quietly
/// making writes arbitrarily expensive.
pub const MAX_INDEXES: usize = 8;

/// How many fields a text search may cover, for the same reason.
pub const MAX_SEARCH_FIELDS: usize = 8;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Scale {
    /// Shares the default partition and the indexes every collection shares.
    #[default]
    Small,
    /// Gets a partition of its own, and its declared indexes are built there.
    Large,
}

/// What an index is for. `Ordered` serves comparisons and ordering, which is
/// what paging needs; `Contains` serves `array-contains` on one field rather
/// than over the whole body.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum IndexKind {
    #[default]
    Ordered,
    Contains,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IndexSpec {
    pub field: String,
    #[serde(default, rename = "type", skip_serializing_if = "is_default")]
    pub kind: IndexKind,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TextSearch {
    pub fields: Vec<String>,
    /// A Postgres text search configuration name. Validated against the
    /// server's own catalogue when the descriptor is applied, never
    /// interpolated into DDL unquoted.
    #[serde(default = "default_language")]
    pub language: String,
}

fn default_language() -> String {
    "english".to_string()
}

fn is_default<T: Default + PartialEq>(v: &T) -> bool {
    *v == T::default()
}

#[derive(Debug, Clone, PartialEq, Default, Serialize, Deserialize)]
pub struct Descriptor {
    #[serde(default, skip_serializing_if = "is_default")]
    pub scale: Scale,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub indexes: Vec<IndexSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text_search: Option<TextSearch>,
    /// Documents in this collection expire this long after their last write.
    /// A per-write `ttl_secs` still wins where one is given.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ttl_secs: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_doc_bytes: Option<usize>,
}

impl Descriptor {
    pub fn from_json(v: &Value) -> Result<Descriptor, StoreError> {
        let descriptor: Descriptor = serde_json::from_value(v.clone())
            .map_err(|e| StoreError::Invalid(format!("descriptor: {e}")))?;
        descriptor.validate()?;
        Ok(descriptor)
    }

    /// Every field named anywhere in the descriptor, for the caller that has
    /// to turn them into indexes.
    pub fn indexed_fields(&self) -> impl Iterator<Item = &IndexSpec> {
        self.indexes.iter()
    }

    pub fn searches(&self) -> Option<&TextSearch> {
        self.text_search.as_ref()
    }

    fn validate(&self) -> Result<(), StoreError> {
        if self.indexes.len() > MAX_INDEXES {
            return Err(StoreError::Invalid(format!(
                "a collection may declare at most {MAX_INDEXES} indexes; every write pays for each"
            )));
        }
        for spec in &self.indexes {
            validate_field(&spec.field)?;
        }
        let mut seen: Vec<(&str, IndexKind)> = Vec::new();
        for spec in &self.indexes {
            let key = (spec.field.as_str(), spec.kind);
            if seen.contains(&key) {
                return Err(StoreError::Invalid(format!(
                    "index on `{}` is declared twice",
                    spec.field
                )));
            }
            seen.push(key);
        }
        if let Some(search) = &self.text_search {
            if search.fields.is_empty() {
                return Err(StoreError::Invalid(
                    "text_search needs at least one field".into(),
                ));
            }
            if search.fields.len() > MAX_SEARCH_FIELDS {
                return Err(StoreError::Invalid(format!(
                    "text_search covers at most {MAX_SEARCH_FIELDS} fields"
                )));
            }
            for field in &search.fields {
                validate_field(field)?;
            }
            if search.language.is_empty() {
                return Err(StoreError::Invalid(
                    "text_search language must be named".into(),
                ));
            }
        }
        if self.ttl_secs.is_some_and(|t| t <= 0) {
            return Err(StoreError::Invalid("ttl_secs must be positive".into()));
        }
        if self
            .max_doc_bytes
            .is_some_and(|b| b == 0 || b > super::MAX_BODY_BYTES)
        {
            return Err(StoreError::Invalid(format!(
                "max_doc_bytes must be between 1 and {}",
                super::MAX_BODY_BYTES
            )));
        }
        Ok(())
    }
}

/// A collection and the descriptor it was given.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CollectionInfo {
    pub collection: String,
    pub descriptor: Descriptor,
    pub revision: i64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn an_absent_descriptor_is_a_small_collection_that_declares_nothing() {
        let d = Descriptor::from_json(&json!({})).unwrap();
        assert_eq!(d.scale, Scale::Small);
        assert!(d.indexes.is_empty());
        assert!(d.text_search.is_none());
    }

    #[test]
    fn a_descriptor_round_trips_only_what_it_declares() {
        let d = Descriptor::from_json(&json!({
            "scale": "large",
            "indexes": [{"field": "status"}, {"field": "tags", "type": "contains"}],
            "text_search": {"fields": ["title"]},
            "ttl_secs": 86400
        }))
        .unwrap();
        assert_eq!(d.scale, Scale::Large);
        assert_eq!(d.indexes[1].kind, IndexKind::Contains);
        assert_eq!(d.searches().unwrap().language, "english");
        assert_eq!(
            serde_json::to_value(&d).unwrap(),
            json!({
                "scale": "large",
                "indexes": [
                    {"field": "status"},
                    {"field": "tags", "type": "contains"}
                ],
                "text_search": {"fields": ["title"], "language": "english"},
                "ttl_secs": 86400
            }),
            "a small collection's descriptor must not gain fields it never set"
        );
    }

    #[test]
    fn descriptors_that_would_make_writes_expensive_are_refused() {
        let many = (0..MAX_INDEXES + 1)
            .map(|i| json!({"field": format!("f{i}")}))
            .collect::<Vec<_>>();
        assert!(Descriptor::from_json(&json!({"indexes": many})).is_err());
        assert!(Descriptor::from_json(&json!({
            "text_search": {"fields": (0..MAX_SEARCH_FIELDS + 1).map(|i| format!("f{i}")).collect::<Vec<_>>()}
        }))
        .is_err());
    }

    #[test]
    fn descriptors_naming_nonsense_are_refused() {
        for bad in [
            json!({"indexes": [{"field": "a b"}]}),
            json!({"indexes": [{"field": "x"}, {"field": "x"}]}),
            json!({"text_search": {"fields": []}}),
            json!({"text_search": {"fields": ["ok"], "language": ""}}),
            json!({"ttl_secs": 0}),
            json!({"max_doc_bytes": 0}),
            json!({"scale": "enormous"}),
        ] {
            assert!(
                Descriptor::from_json(&bad).is_err(),
                "should have been refused: {bad}"
            );
        }
    }

    #[test]
    fn the_same_field_may_carry_both_an_ordered_and_a_contains_index() {
        let d = Descriptor::from_json(&json!({
            "indexes": [{"field": "tags"}, {"field": "tags", "type": "contains"}]
        }))
        .unwrap();
        assert_eq!(d.indexes.len(), 2);
    }
}
