//! Shared machinery for the golden corpora that pin a Rust port to the widget
//! engine it was ported from (`diagram`, `chart`).

use std::path::PathBuf;

use serde_json::Value;

/// Every `<name>.json` under `fixtures/<dir>`, sorted, as `(name, body)`;
/// panics below `at_least` files so a missing corpus fails loudly.
pub(crate) fn corpus(dir: &str, at_least: usize) -> Vec<(String, Value)> {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!("../../fixtures/{dir}"));
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("reading {}: {e}", dir.display()))
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            (path.extension()? == "json").then_some(path)
        })
        .collect();
    files.sort();
    assert!(
        files.len() >= at_least,
        "expected the committed corpus, found {} files in {}",
        files.len(),
        dir.display()
    );
    files
        .into_iter()
        .map(|file| {
            let name = file
                .file_stem()
                .expect("named fixture")
                .to_string_lossy()
                .to_string();
            let body = serde_json::from_str(
                &std::fs::read_to_string(&file).unwrap_or_else(|e| panic!("{name}: {e}")),
            )
            .unwrap_or_else(|e| panic!("{name}: {e}"));
            (name, body)
        })
        .collect()
}

/// Compare numbers as f64 throughout. `serde_json` distinguishes `46` from
/// `46.0`, which is an encoding difference between the two languages and not a
/// difference in the picture.
pub(crate) fn canonical(value: &Value) -> Value {
    match value {
        Value::Number(n) => {
            let f = n.as_f64().expect("golden numbers are finite");
            Value::String(format!("{f:?}"))
        }
        Value::Array(a) => Value::Array(a.iter().map(canonical).collect()),
        Value::Object(o) => {
            Value::Object(o.iter().map(|(k, v)| (k.clone(), canonical(v))).collect())
        }
        other => other.clone(),
    }
}

/// Where two documents first disagree, as a readable path — a whole-document
/// diff is unreadable.
pub(crate) fn first_difference(a: &Value, b: &Value, path: &str) -> Option<String> {
    match (a, b) {
        (Value::Object(x), Value::Object(y)) => {
            let mut keys: Vec<&String> = x.keys().chain(y.keys()).collect();
            keys.sort();
            keys.dedup();
            keys.into_iter().find_map(|k| match (x.get(k), y.get(k)) {
                (Some(xv), Some(yv)) => first_difference(xv, yv, &format!("{path}.{k}")),
                (None, Some(v)) => Some(format!("{path}.{k}: missing in rust, ts has {v}")),
                (Some(v), None) => Some(format!("{path}.{k}: rust has {v}, missing in ts")),
                (None, None) => None,
            })
        }
        (Value::Array(x), Value::Array(y)) => {
            if x.len() != y.len() {
                return Some(format!(
                    "{path}: rust len {} vs ts len {}",
                    x.len(),
                    y.len()
                ));
            }
            x.iter()
                .zip(y)
                .enumerate()
                .find_map(|(i, (xv, yv))| first_difference(xv, yv, &format!("{path}[{i}]")))
        }
        _ if a == b => None,
        _ => Some(format!("{path}: rust {a} vs ts {b}")),
    }
}
