//! Which messages a call means: uids it names, or a search the server runs,
//! so a bulk action over a whole mailbox never lists its messages to the model.

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::uids::UidSet;
use crate::adapters::utils::coerce_to_u64;

/// A search over one folder, matched on the server. Every field narrows it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Criteria {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub to: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub after: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub unread: Option<bool>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub has_attachment: bool,
}

/// An IMAP date (`15-Jan-2026`) from `"7d"` (days before today) or
/// `"2026-01-15"`.
pub fn imap_date(when: &str) -> Result<String, String> {
    let trimmed = when.trim();
    let relative = trimmed
        .strip_suffix('d')
        .and_then(|days| days.parse::<i64>().ok());
    let date = match relative {
        Some(days) if days <= 0 => {
            return Err(format!(
                "Invalid relative date '{when}': days must be positive"
            ))
        }
        Some(days) => Some(chrono::Utc::now().date_naive() - chrono::Duration::days(days)),
        None => NaiveDate::parse_from_str(trimmed, "%Y-%m-%d").ok(),
    };
    date.map(|d| d.format("%d-%b-%Y").to_string()).ok_or_else(|| {
        format!(
            "Invalid date format '{when}'. Use relative (e.g. '7d', '30d') or ISO date (e.g. '2026-01-15')."
        )
    })
}

/// `text` as an IMAP quoted string.
pub fn quoted(text: &str) -> Result<String, String> {
    match text.contains(['\r', '\n']) {
        true => Err("A search can't contain a line break.".to_string()),
        false => Ok(format!(
            "\"{}\"",
            text.replace('\\', "\\\\").replace('"', "\\\"")
        )),
    }
}

fn given(field: &Option<String>) -> Option<&str> {
    field.as_deref().map(str::trim).filter(|s| !s.is_empty())
}

impl Criteria {
    /// The `UID SEARCH` keys for this search; Gmail (`X-GM-EXT-1`) matches
    /// attachments with its own search, which sees them where headers don't.
    pub fn to_imap(&self, gmail: bool) -> Result<String, String> {
        let keyed = [
            ("FROM", given(&self.from)),
            ("TO", given(&self.to)),
            ("SUBJECT", given(&self.subject)),
            ("TEXT", given(&self.text)),
        ];
        let mut keys = keyed
            .into_iter()
            .filter_map(|(key, value)| value.map(|v| quoted(v).map(|q| format!("{key} {q}"))))
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(before) = given(&self.before) {
            keys.push(format!("BEFORE {}", imap_date(before)?));
        }
        if let Some(after) = given(&self.after) {
            keys.push(format!("SINCE {}", imap_date(after)?));
        }
        match self.unread {
            Some(true) => keys.push("UNSEEN".to_string()),
            Some(false) => keys.push("SEEN".to_string()),
            None => {}
        }
        if self.has_attachment {
            keys.push(match gmail {
                true => "X-GM-RAW \"has:attachment\"".to_string(),
                false => "OR HEADER Content-Type \"multipart/mixed\" HEADER Content-Disposition \"attachment\"".to_string(),
            });
        }
        match keys.is_empty() {
            true => Err(
                "An empty search would match every message; say what to match (from, to, subject, text, before, after, unread or has_attachment)."
                    .to_string(),
            ),
            false => Ok(keys.join(" ")),
        }
    }

    /// The search in words, for a summary or an approval card.
    pub fn describe(&self) -> String {
        let quoted_words = [
            ("from", given(&self.from)),
            ("to", given(&self.to)),
            ("with subject", given(&self.subject)),
            ("mentioning", given(&self.text)),
            ("before", given(&self.before)),
            ("since", given(&self.after)),
        ];
        let mut words: Vec<String> = quoted_words
            .into_iter()
            .filter_map(|(word, value)| value.map(|v| format!("{word} {v}")))
            .collect();
        match self.unread {
            Some(true) => words.push("unread".to_string()),
            Some(false) => words.push("read".to_string()),
            None => {}
        }
        if self.has_attachment {
            words.push("with an attachment".to_string());
        }
        words.join(", ")
    }
}

/// The messages a bulk call acts on in its folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Selection {
    Uids(UidSet),
    Search(Criteria),
}

impl Selection {
    /// `uid`, `uids` or `search` from a call's input.
    pub fn from_input(input: &Value) -> Result<Selection, String> {
        let search = input.get("search").filter(|s| !s.is_null());
        let uids = uid_list(input)?;
        match (uids, search) {
            (Some(_), Some(_)) => Err("Give either uids or a search, not both.".to_string()),
            (Some(uids), None) => Ok(Selection::Uids(uids)),
            (None, Some(search)) => serde_json::from_value::<Criteria>(search.clone())
                .map(Selection::Search)
                .map_err(|e| format!("Invalid search: {e}")),
            (None, None) => Err(
                "Say which messages: 'uid', 'uids', or a 'search' (from, to, subject, text, before, after, unread, has_attachment)."
                    .to_string(),
            ),
        }
    }

    pub fn describe(&self) -> String {
        match self {
            Selection::Uids(uids) if uids.len() == 1 => "1 message".to_string(),
            Selection::Uids(uids) => format!("{} messages", uids.len()),
            Selection::Search(criteria) => format!("messages {}", criteria.describe()),
        }
    }
}

fn uid_list(input: &Value) -> Result<Option<UidSet>, String> {
    let positive = |v: &Value| {
        coerce_to_u64(v)
            .filter(|u| (1..=u64::from(u32::MAX)).contains(u))
            .map(|u| u as u32)
            .ok_or_else(|| format!("Invalid UID value: {v} — expected a positive integer"))
    };
    match (input.get("uid").filter(|v| !v.is_null()), input.get("uids")) {
        (Some(uid), _) => positive(uid).map(|u| Some(std::iter::once(u).collect())),
        (None, Some(Value::Array(list))) if list.is_empty() => {
            Err("'uids' array must not be empty".to_string())
        }
        (None, Some(Value::Array(list))) => list
            .iter()
            .map(positive)
            .collect::<Result<UidSet, _>>()
            .map(Some),
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn search(v: Value) -> Criteria {
        serde_json::from_value(v).unwrap()
    }

    #[test]
    fn every_field_narrows_the_search() {
        let c = search(json!({
            "from": "news@shop.com", "to": "me@x.com", "subject": "Sale",
            "text": "unsubscribe", "before": "2026-01-15", "after": "2025-12-01",
            "unread": true
        }));
        assert_eq!(
            c.to_imap(false).unwrap(),
            "FROM \"news@shop.com\" TO \"me@x.com\" SUBJECT \"Sale\" TEXT \"unsubscribe\" \
             BEFORE 15-Jan-2026 SINCE 01-Dec-2025 UNSEEN"
        );
        assert_eq!(
            search(json!({"unread": false})).to_imap(false).unwrap(),
            "SEEN"
        );
    }

    #[test]
    fn quotes_and_backslashes_are_escaped_and_line_breaks_refused() {
        let c = search(json!({"subject": "a \"b\" \\ c"}));
        assert_eq!(c.to_imap(false).unwrap(), "SUBJECT \"a \\\"b\\\" \\\\ c\"");
        let injected = search(json!({"from": "x\r\nA1 DELETE INBOX"}));
        assert!(injected.to_imap(false).is_err());
    }

    #[test]
    fn gmail_matches_attachments_with_its_own_search() {
        let c = search(json!({"has_attachment": true}));
        assert_eq!(c.to_imap(true).unwrap(), "X-GM-RAW \"has:attachment\"");
        assert!(c
            .to_imap(false)
            .unwrap()
            .starts_with("OR HEADER Content-Type"));
    }

    #[test]
    fn an_empty_search_never_means_everything() {
        assert!(search(json!({})).to_imap(false).is_err());
        assert!(search(json!({"from": "  "})).to_imap(false).is_err());
        assert!(serde_json::from_value::<Criteria>(json!({"all": true})).is_err());
    }

    #[test]
    fn a_selection_is_uids_or_a_search_never_both() {
        assert_eq!(
            Selection::from_input(&json!({"uids": [3, 1, "2"]})).unwrap(),
            Selection::Uids([1, 2, 3].into_iter().collect())
        );
        assert_eq!(
            Selection::from_input(&json!({"uid": 9})).unwrap(),
            Selection::Uids(std::iter::once(9).collect())
        );
        assert!(matches!(
            Selection::from_input(&json!({"search": {"from": "a"}})).unwrap(),
            Selection::Search(_)
        ));
        assert!(Selection::from_input(&json!({"uid": 1, "search": {"from": "a"}})).is_err());
        assert!(Selection::from_input(&json!({"uids": []})).is_err());
        assert!(Selection::from_input(&json!({"uids": [0]})).is_err());
        assert!(Selection::from_input(&json!({})).is_err());
    }

    #[test]
    fn dates_are_relative_or_iso() {
        assert_eq!(imap_date("2026-01-15").unwrap(), "15-Jan-2026");
        assert!(imap_date("7d").is_ok());
        assert!(imap_date("0d").is_err());
        assert!(imap_date("yesterday").is_err());
    }

    #[test]
    fn a_search_reads_as_words() {
        let c = search(json!({"from": "news@x", "unread": true, "has_attachment": true}));
        assert_eq!(c.describe(), "from news@x, unread, with an attachment");
    }
}
