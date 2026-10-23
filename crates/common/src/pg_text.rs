//! Helpers for values bound to Postgres `text`/`jsonb` parameters, which
//! reject NUL (U+0000). Binary tool output occasionally smuggles NULs into
//! message content; stripping them at the bind boundary keeps both the live
//! store's saves and the migration backfill from failing on such rows.

use std::borrow::Cow;

use serde_json::Value;

/// Strip NULs from a string, borrowing when the string is already clean.
pub fn strip_nuls_str(s: &str) -> Cow<'_, str> {
    if s.contains('\0') {
        Cow::Owned(s.replace('\0', ""))
    } else {
        Cow::Borrowed(s)
    }
}

/// Recursively strip NULs from every string in a JSON value — object keys
/// included. Clean values are left untouched.
pub fn strip_nuls_json(value: &mut Value) {
    match value {
        Value::String(s) => {
            if s.contains('\0') {
                *s = s.replace('\0', "");
            }
        }
        Value::Array(items) => items.iter_mut().for_each(strip_nuls_json),
        Value::Object(map) => {
            map.values_mut().for_each(strip_nuls_json);
            if map.keys().any(|k| k.contains('\0')) {
                *map = std::mem::take(map)
                    .into_iter()
                    .map(|(k, v)| (k.replace('\0', ""), v))
                    .collect();
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn clean_string_borrows() {
        assert!(matches!(strip_nuls_str("hello"), Cow::Borrowed("hello")));
        assert_eq!(strip_nuls_str("a\0b"), "ab");
    }

    #[test]
    fn scrubs_nested_values_and_keys() {
        let mut v = json!({
            "ok": "clean",
            "bad\0key": ["x\0y", {"inner": "p\0q"}, 7, null],
        });
        strip_nuls_json(&mut v);
        assert_eq!(
            v,
            json!({"ok": "clean", "badkey": ["xy", {"inner": "pq"}, 7, null]})
        );
    }

    #[test]
    fn clean_value_unchanged() {
        let mut v = json!({"a": ["b", {"c": 1}]});
        let before = v.clone();
        strip_nuls_json(&mut v);
        assert_eq!(v, before);
    }
}
