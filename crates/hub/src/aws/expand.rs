//! Pure key-set expansion for [`super::collect`]. Turns the tool's two input
//! modes — an explicit `keys` list or a `template` crossed with `dimensions` —
//! into one `(metadata_columns, targets)` shape the producer consumes. No I/O
//! and no SDK types, so the expansion logic is unit-tested in isolation.

use std::collections::HashMap;

use eyre::{bail, Result};
use serde_json::Value;

/// Hard ceiling on the expanded key set. Over this is an error, never a silent
/// truncation: dropping keys would read as "covered everything" when it didn't.
pub const MAX_COLLECT_KEYS: usize = 50_000;

/// The always-present first metadata column carrying each object's full key.
pub const FILE_PATH_COLUMN: &str = "file_path";

/// One object to fetch plus its metadata cells, aligned to
/// [`Expansion::metadata_columns`]. The first cell is always the key itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    pub key: String,
    pub metadata: Vec<Value>,
}

/// The shared metadata header (`file_path` first) and the expanded targets.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Expansion {
    pub metadata_columns: Vec<String>,
    pub targets: Vec<Target>,
}

/// Resolve the tool's optional inputs into one [`Expansion`], selecting the
/// mode and validating it. Exactly one of `keys` / `template` must be present.
pub fn expand(
    keys: Option<Vec<String>>,
    path_columns: Vec<String>,
    template: Option<String>,
    dimensions: HashMap<String, Vec<String>>,
) -> Result<Expansion> {
    match (keys, template) {
        (Some(_), Some(_)) => bail!("provide either `keys` or `template`, not both"),
        (None, None) => bail!("provide `keys` (explicit) or `template` + `dimensions`"),
        (Some(keys), None) => {
            if !dimensions.is_empty() {
                bail!("`dimensions` applies to `template` mode, not `keys`");
            }
            expand_keys(keys, path_columns)
        }
        (None, Some(template)) => {
            if !path_columns.is_empty() {
                bail!("`path_columns` applies to `keys` mode, not `template`");
            }
            expand_template(&template, dimensions)
        }
    }
}

/// Explicit-keys mode: each `path_columns[i]` names the `i`-th `/`-separated
/// segment of every key (the basename falls at the last index, typically
/// unnamed). A key with fewer segments than columns yields empty cells.
fn expand_keys(keys: Vec<String>, path_columns: Vec<String>) -> Result<Expansion> {
    if keys.is_empty() {
        bail!("`keys` must be non-empty");
    }
    if keys.len() > MAX_COLLECT_KEYS {
        bail!(
            "{} keys exceeds the {MAX_COLLECT_KEYS} limit; narrow the request (e.g. one month at a time)",
            keys.len()
        );
    }
    let targets =
        keys.into_iter()
            .map(|key| {
                let segments: Vec<&str> = key.split('/').collect();
                let mut metadata = Vec::with_capacity(path_columns.len() + 1);
                metadata.push(Value::String(key.clone()));
                metadata.extend(path_columns.iter().enumerate().map(|(i, _)| {
                    Value::String(segments.get(i).copied().unwrap_or("").to_string())
                }));
                Target { key, metadata }
            })
            .collect();
    Ok(Expansion {
        metadata_columns: with_file_path(path_columns),
        targets,
    })
}

/// Template mode: the `{placeholder}` tokens (in first-appearance order) name
/// the metadata columns and must correspond one-to-one with `dimensions`. Keys
/// are the Cartesian product of the dimension values substituted in.
fn expand_template(template: &str, dimensions: HashMap<String, Vec<String>>) -> Result<Expansion> {
    let placeholders = parse_placeholders(template)?;
    if placeholders.is_empty() {
        bail!("`template` contains no {{placeholder}} tokens");
    }
    for name in &placeholders {
        match dimensions.get(name) {
            None => bail!("template placeholder `{name}` has no matching dimension"),
            Some(values) if values.is_empty() => bail!("dimension `{name}` has no values"),
            Some(_) => {}
        }
    }
    for name in dimensions.keys() {
        if !placeholders.contains(name) {
            bail!("dimension `{name}` is not used in the template");
        }
    }

    let value_lists: Vec<&[String]> = placeholders
        .iter()
        .map(|p| dimensions[p].as_slice())
        .collect();
    let total = value_lists
        .iter()
        .try_fold(1usize, |acc, list| acc.checked_mul(list.len()))
        .filter(|n| *n <= MAX_COLLECT_KEYS)
        .ok_or_else(|| {
            eyre::eyre!(
                "the Cartesian product exceeds the {MAX_COLLECT_KEYS} key limit; narrow the request (e.g. one month at a time)"
            )
        })?;

    let mut targets = Vec::with_capacity(total);
    for combo in cartesian(&value_lists) {
        let key = placeholders
            .iter()
            .zip(&combo)
            .fold(template.to_string(), |acc, (placeholder, value)| {
                acc.replace(&format!("{{{placeholder}}}"), value)
            });
        let mut metadata = Vec::with_capacity(combo.len() + 1);
        metadata.push(Value::String(key.clone()));
        metadata.extend(combo.into_iter().map(|v| Value::String(v.to_string())));
        targets.push(Target { key, metadata });
    }
    Ok(Expansion {
        metadata_columns: with_file_path(placeholders),
        targets,
    })
}

fn with_file_path(mut columns: Vec<String>) -> Vec<String> {
    columns.insert(0, FILE_PATH_COLUMN.to_string());
    columns
}

/// Extract `{token}` names from a template in first-appearance order, deduped.
fn parse_placeholders(template: &str) -> Result<Vec<String>> {
    let mut names: Vec<String> = Vec::new();
    let mut rest = template;
    while let Some(open) = rest.find('{') {
        let after = &rest[open + 1..];
        let close = after
            .find('}')
            .ok_or_else(|| eyre::eyre!("unclosed '{{' in template"))?;
        let name = &after[..close];
        if name.is_empty() {
            bail!("empty {{}} placeholder in template");
        }
        if !names.iter().any(|n| n == name) {
            names.push(name.to_string());
        }
        rest = &after[close + 1..];
    }
    Ok(names)
}

/// Cartesian product of the value lists, preserving list order; the leftmost
/// list varies slowest, matching a nested-loop reading of the dimensions.
fn cartesian<'a>(lists: &[&'a [String]]) -> Vec<Vec<&'a str>> {
    lists.iter().fold(vec![Vec::new()], |acc, list| {
        acc.iter()
            .flat_map(|prefix| {
                list.iter().map(move |value| {
                    let mut next = prefix.clone();
                    next.push(value.as_str());
                    next
                })
            })
            .collect()
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn dims(pairs: &[(&str, &[&str])]) -> HashMap<String, Vec<String>> {
        pairs
            .iter()
            .map(|(k, vs)| (k.to_string(), vs.iter().map(|s| s.to_string()).collect()))
            .collect()
    }

    #[test]
    fn keys_mode_derives_positional_path_columns_and_file_path() {
        let exp = expand(
            Some(vec!["2026-02-01/NORTH/Sales/main.csv".to_string()]),
            vec![
                "file_date".to_string(),
                "file_region".to_string(),
                "file_team".to_string(),
            ],
            None,
            HashMap::new(),
        )
        .unwrap();
        assert_eq!(
            exp.metadata_columns,
            vec!["file_path", "file_date", "file_region", "file_team"]
        );
        assert_eq!(
            exp.targets[0].metadata,
            vec![
                json!("2026-02-01/NORTH/Sales/main.csv"),
                json!("2026-02-01"),
                json!("NORTH"),
                json!("Sales"),
            ]
        );
    }

    #[test]
    fn keys_mode_missing_segments_yield_empty_cells() {
        let exp = expand(
            Some(vec!["only-one".to_string()]),
            vec!["a".to_string(), "b".to_string()],
            None,
            HashMap::new(),
        )
        .unwrap();
        assert_eq!(
            exp.targets[0].metadata,
            vec![json!("only-one"), json!("only-one"), json!("")]
        );
    }

    #[test]
    fn keys_mode_with_no_path_columns_still_carries_file_path() {
        let exp = expand(
            Some(vec!["a/b.csv".to_string()]),
            Vec::new(),
            None,
            HashMap::new(),
        )
        .unwrap();
        assert_eq!(exp.metadata_columns, vec!["file_path"]);
        assert_eq!(exp.targets[0].metadata, vec![json!("a/b.csv")]);
    }

    #[test]
    fn template_mode_cartesian_product_and_ordering() {
        let exp = expand(
            None,
            Vec::new(),
            Some("{file_date}/{file_region}/main.csv".to_string()),
            dims(&[
                ("file_date", &["d1", "d2"]),
                ("file_region", &["NORTH", "SOUTH"]),
            ]),
        )
        .unwrap();
        assert_eq!(
            exp.metadata_columns,
            vec!["file_path", "file_date", "file_region"]
        );
        let keys: Vec<&str> = exp.targets.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(
            keys,
            vec![
                "d1/NORTH/main.csv",
                "d1/SOUTH/main.csv",
                "d2/NORTH/main.csv",
                "d2/SOUTH/main.csv",
            ]
        );
        assert_eq!(
            exp.targets[1].metadata,
            vec![json!("d1/SOUTH/main.csv"), json!("d1"), json!("SOUTH")]
        );
    }

    #[test]
    fn template_column_order_follows_template_not_map() {
        let exp = expand(
            None,
            Vec::new(),
            Some("{b}/{a}/main.csv".to_string()),
            dims(&[("a", &["1"]), ("b", &["2"])]),
        )
        .unwrap();
        assert_eq!(exp.metadata_columns, vec!["file_path", "b", "a"]);
        assert_eq!(exp.targets[0].key, "2/1/main.csv");
    }

    #[test]
    fn template_repeated_placeholder_is_single_column_substituted_everywhere() {
        let exp = expand(
            None,
            Vec::new(),
            Some("{x}/sub/{x}.csv".to_string()),
            dims(&[("x", &["a", "b"])]),
        )
        .unwrap();
        assert_eq!(exp.metadata_columns, vec!["file_path", "x"]);
        let keys: Vec<&str> = exp.targets.iter().map(|t| t.key.as_str()).collect();
        assert_eq!(keys, vec!["a/sub/a.csv", "b/sub/b.csv"]);
    }

    #[test]
    fn both_modes_is_an_error() {
        assert!(expand(
            Some(vec!["k".to_string()]),
            Vec::new(),
            Some("{a}".to_string()),
            HashMap::new(),
        )
        .is_err());
    }

    #[test]
    fn neither_mode_is_an_error() {
        assert!(expand(None, Vec::new(), None, HashMap::new()).is_err());
    }

    #[test]
    fn empty_keys_is_an_error() {
        assert!(expand(Some(Vec::new()), Vec::new(), None, HashMap::new()).is_err());
    }

    #[test]
    fn path_columns_in_template_mode_is_an_error() {
        assert!(expand(
            None,
            vec!["x".to_string()],
            Some("{a}".to_string()),
            dims(&[("a", &["1"])]),
        )
        .is_err());
    }

    #[test]
    fn dimensions_in_keys_mode_is_an_error() {
        assert!(expand(
            Some(vec!["k".to_string()]),
            Vec::new(),
            None,
            dims(&[("a", &["1"])]),
        )
        .is_err());
    }

    #[test]
    fn unbound_placeholder_is_an_error() {
        assert!(expand(
            None,
            Vec::new(),
            Some("{a}/{b}".to_string()),
            dims(&[("a", &["1"])]),
        )
        .is_err());
    }

    #[test]
    fn unused_dimension_is_an_error() {
        assert!(expand(
            None,
            Vec::new(),
            Some("{a}".to_string()),
            dims(&[("a", &["1"]), ("b", &["2"])]),
        )
        .is_err());
    }

    #[test]
    fn empty_dimension_values_is_an_error() {
        assert!(expand(
            None,
            Vec::new(),
            Some("{a}".to_string()),
            dims(&[("a", &[])]),
        )
        .is_err());
    }

    #[test]
    fn template_with_no_placeholders_is_an_error() {
        assert!(expand(
            None,
            Vec::new(),
            Some("static/main.csv".to_string()),
            HashMap::new(),
        )
        .is_err());
    }

    #[test]
    fn unclosed_brace_is_an_error() {
        assert!(expand(
            None,
            Vec::new(),
            Some("{a/main.csv".to_string()),
            dims(&[("a", &["1"])]),
        )
        .is_err());
    }

    #[test]
    fn over_cap_cartesian_is_an_error() {
        let big: Vec<String> = (0..1000).map(|i| i.to_string()).collect();
        let dimensions = HashMap::from([("a".to_string(), big.clone()), ("b".to_string(), big)]);
        let err = expand(None, Vec::new(), Some("{a}/{b}".to_string()), dimensions).unwrap_err();
        assert!(err.to_string().contains("limit"), "{err}");
    }

    #[test]
    fn keys_over_cap_is_an_error() {
        let keys: Vec<String> = (0..MAX_COLLECT_KEYS + 1).map(|i| format!("k{i}")).collect();
        assert!(expand(Some(keys), Vec::new(), None, HashMap::new()).is_err());
    }

    #[test]
    fn parse_placeholders_orders_and_dedups() {
        assert_eq!(
            parse_placeholders("{a}/{b}/{a}/x").unwrap(),
            vec!["a".to_string(), "b".to_string()]
        );
    }
}
