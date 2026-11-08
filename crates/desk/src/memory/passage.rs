//! What session memory puts in its search index: one collection per group,
//! so no filter can cross a client boundary, and each exchange as passages
//! short enough for the node's model to read whole. Pure: the index worker
//! reads the rows and writes what this makes.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use super::record::Step;

/// The most characters in one passage: about 400 tokens, under the 512
/// bge-base reads.
pub const PASSAGE_MAX: usize = 1600;
/// How many characters a passage repeats of the one before, so a sentence
/// cut between two is whole in one of them.
pub const PASSAGE_OVERLAP: usize = 200;
/// What every collection of session memory is named from.
pub const COLLECTION_PREFIX: &str = "DeskWork_";
/// The collection of ungrouped sessions' memory.
pub const UNGROUPED_COLLECTION: &str = "DeskWork_u";

/// The collection a row stamped with `group` is kept in: `DeskWork_u` for
/// ungrouped, else `DeskWork_` and the first 16 hex of the group id's
/// sha256.
pub fn collection_of(group: Option<&str>) -> String {
    match group {
        None => UNGROUPED_COLLECTION.to_string(),
        Some(g) => {
            let digest = Sha256::digest(g.as_bytes());
            format!("{COLLECTION_PREFIX}{}", &hex::encode(digest)[..16])
        }
    }
}

/// Whether `name` is one of session memory's collections (the tenant also
/// holds agents' memories, artifacts and code).
pub fn is_ours(name: &str) -> bool {
    name.starts_with(COLLECTION_PREFIX)
}

/// What a passage is of, as a search filters and ranks it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PassageKind {
    Exchange,
    /// An engine's own summary of what came before.
    Compaction,
}

impl PassageKind {
    pub fn as_str(self) -> &'static str {
        match self {
            PassageKind::Exchange => "exchange",
            PassageKind::Compaction => "compaction",
        }
    }
}

/// One object of the index.
#[derive(Debug, Clone, PartialEq)]
pub struct Passage {
    /// `<ref>#<n>`: what the object's id is made from, and what reconcile
    /// compares by.
    pub key: String,
    /// The exchange (or, from SM3, the item) it is of.
    pub of: Uuid,
    /// Its place among its exchange's passages.
    pub n: u32,
    pub kind: PassageKind,
    pub session: Uuid,
    pub project: String,
    pub at: DateTime<Utc>,
    /// Not superseded; every exchange is.
    pub valid: bool,
    pub text: String,
    /// The text's hash: what tells a passage the index holds from one that
    /// agrees with Postgres.
    pub hash: String,
}

/// An exchange as the index worker reads it from Postgres.
#[derive(Debug, Clone, PartialEq)]
pub struct IndexRow {
    pub id: Uuid,
    pub session: Uuid,
    pub group: Option<String>,
    pub project: String,
    pub at: DateTime<Utc>,
    pub ask: String,
    pub answer: String,
    pub steps: Vec<Step>,
    pub compaction: bool,
}

/// The text one exchange is searched by: what was asked, what was
/// answered, and a line per step.
pub fn exchange_text(ask: &str, answer: &str, steps: &[Step]) -> String {
    let mut text = String::new();
    if !ask.trim().is_empty() {
        text.push_str("Asked: ");
        text.push_str(ask.trim());
        text.push('\n');
    }
    if !answer.trim().is_empty() {
        text.push_str("Answered: ");
        text.push_str(answer.trim());
        text.push('\n');
    }
    for s in steps {
        text.push_str(&s.tool);
        if !s.input.trim().is_empty() {
            text.push_str(": ");
            text.push_str(s.input.trim());
        }
        if s.failed {
            text.push_str(" (failed)");
        }
        text.push('\n');
    }
    text.trim_end().to_string()
}

/// `text` cut into pieces of at most `max` characters, each repeating the
/// last `overlap` of the one before. A cut falls at the last whitespace in
/// the piece's final quarter, when there is one, so words are kept whole.
pub fn split(text: &str, max: usize, overlap: usize) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    if chars.is_empty() {
        return Vec::new();
    }
    let max = max.max(1);
    let overlap = overlap.min(max / 2);
    let mut out = Vec::new();
    let mut start = 0;
    loop {
        let hard_end = (start + max).min(chars.len());
        if hard_end == chars.len() {
            out.push(chars[start..].iter().collect());
            return out;
        }
        let soft_from = start + max * 3 / 4;
        let end = (soft_from..hard_end)
            .rev()
            .find(|&i| chars[i].is_whitespace())
            .map(|i| i + 1)
            .unwrap_or(hard_end);
        out.push(chars[start..end].iter().collect());
        // Always forward, by at least what isn't repeated.
        start = end.saturating_sub(overlap).max(start + 1);
    }
}

/// What a passage's text hashes to: 16 hex of its sha256.
pub fn hash_of(text: &str) -> String {
    hex::encode(&Sha256::digest(text.as_bytes())[..8])
}

/// `row`'s passages, in order.
pub fn passages_of(row: &IndexRow) -> Vec<Passage> {
    let kind = match row.compaction {
        true => PassageKind::Compaction,
        false => PassageKind::Exchange,
    };
    split(
        &exchange_text(&row.ask, &row.answer, &row.steps),
        PASSAGE_MAX,
        PASSAGE_OVERLAP,
    )
    .into_iter()
    .enumerate()
    .map(|(n, text)| {
        let n = u32::try_from(n).unwrap_or(u32::MAX);
        Passage {
            key: passage_key(row.id, n),
            of: row.id,
            n,
            kind,
            session: row.session,
            project: row.project.clone(),
            at: row.at,
            valid: true,
            hash: hash_of(&text),
            text,
        }
    })
    .collect()
}

/// The key of passage `n` of `of`.
pub fn passage_key(of: Uuid, n: u32) -> String {
    format!("{of}#{n}")
}

/// The ref a passage's key is of.
pub fn ref_of_key(key: &str) -> Option<Uuid> {
    key.split_once('#')?.0.parse().ok()
}

/// What an index deletion takes out of one collection, as
/// `memory_purges.filter` keeps it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "of", rename_all = "snake_case")]
pub enum Purge {
    /// Every passage of one session.
    Session { session: Uuid },
    /// Every passage of these exchanges or items.
    Refs { refs: Vec<Uuid> },
    /// The whole collection, dropped: a rebuild.
    All,
}

/// The most refs one purge row names.
pub const PURGE_REFS: usize = 500;

/// `refs` as purges of at most [`PURGE_REFS`] each.
pub fn ref_purges(refs: &[Uuid]) -> Vec<Purge> {
    refs.chunks(PURGE_REFS)
        .map(|c| Purge::Refs { refs: c.to_vec() })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_group_has_its_own_collection_and_ungrouped_one_too() {
        assert_eq!(collection_of(None), "DeskWork_u");
        let acme = collection_of(Some("g-acme"));
        assert_eq!(acme.len(), "DeskWork_".len() + 16);
        assert!(acme.starts_with("DeskWork_"));
        assert!(acme["DeskWork_".len()..]
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
        assert_ne!(acme, collection_of(Some("g-globex")));
        assert_eq!(acme, collection_of(Some("g-acme")), "the same each time");
        // A vellev name: a letter first, then letters, digits and `_`.
        for name in [collection_of(None), acme] {
            assert!(name.chars().next().unwrap().is_ascii_alphabetic());
            assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'));
            assert!(is_ours(&name));
        }
        assert!(!is_ours("Artifacts"));
    }

    #[test]
    fn a_long_exchange_is_cut_into_overlapping_passages_none_too_long() {
        let words: String = (0..900).map(|i| format!("word{i} ")).collect();
        let pieces = split(&words, PASSAGE_MAX, PASSAGE_OVERLAP);
        assert!(pieces.len() > 3);
        for p in &pieces {
            assert!(p.chars().count() <= PASSAGE_MAX, "{}", p.chars().count());
        }
        for pair in pieces.windows(2) {
            let tail: String = {
                let c: Vec<char> = pair[0].chars().collect();
                c[c.len() - PASSAGE_OVERLAP..].iter().collect()
            };
            assert!(
                pair[1].starts_with(&tail),
                "each repeats the last 200 of the one before"
            );
        }
        // Words are kept whole at each cut.
        for p in &pieces[..pieces.len() - 1] {
            assert!(p.ends_with(' '), "{:?}", &p[p.len() - 10..]);
        }
        let short = split("fix the cookie flag", PASSAGE_MAX, PASSAGE_OVERLAP);
        assert_eq!(short, ["fix the cookie flag"]);
        assert!(split("", PASSAGE_MAX, PASSAGE_OVERLAP).is_empty());
        // No whitespace at all: cut hard, still forward.
        let solid = "x".repeat(4000);
        let pieces = split(&solid, PASSAGE_MAX, PASSAGE_OVERLAP);
        assert_eq!(pieces[0].len(), PASSAGE_MAX);
        assert!(pieces.iter().all(|p| p.len() <= PASSAGE_MAX));
        // Multi-byte text is cut by characters.
        let wide = "é".repeat(3000);
        assert!(split(&wide, PASSAGE_MAX, PASSAGE_OVERLAP)
            .iter()
            .all(|p| p.chars().count() <= PASSAGE_MAX));
    }

    #[test]
    fn an_exchanges_passages_carry_what_the_index_filters_by() {
        let row = IndexRow {
            id: Uuid::new_v4(),
            session: Uuid::new_v4(),
            group: Some("g-acme".into()),
            project: "/dev/acme-web".into(),
            at: Utc::now(),
            ask: "Where is the session token kept?".into(),
            answer: "In an HttpOnly cookie.".into(),
            steps: vec![Step {
                id: "c1".into(),
                tool: "Read".into(),
                input: "src/auth.ts".into(),
                result: "…".into(),
                failed: false,
            }],
            compaction: false,
        };
        let ps = passages_of(&row);
        assert_eq!(ps.len(), 1);
        let p = &ps[0];
        assert_eq!(p.key, format!("{}#0", row.id));
        assert_eq!(ref_of_key(&p.key), Some(row.id));
        assert_eq!(
            (p.session, p.project.as_str(), p.kind),
            (row.session, "/dev/acme-web", PassageKind::Exchange)
        );
        assert!(p.valid);
        assert_eq!(
            p.text,
            "Asked: Where is the session token kept?\nAnswered: In an HttpOnly cookie.\nRead: src/auth.ts"
        );
        assert_eq!(p.hash, hash_of(&p.text));
        assert_eq!(p.hash.len(), 16);
        let summary = IndexRow {
            compaction: true,
            ..row
        };
        assert_eq!(passages_of(&summary)[0].kind, PassageKind::Compaction);
    }

    #[test]
    fn purges_are_kept_as_plain_json() {
        let s = Uuid::new_v4();
        let kept = serde_json::to_value(Purge::Session { session: s }).unwrap();
        assert_eq!(kept, serde_json::json!({"of": "session", "session": s}));
        assert_eq!(
            serde_json::to_value(Purge::All).unwrap(),
            serde_json::json!({"of": "all"})
        );
        let refs: Vec<Uuid> = (0..1200).map(|_| Uuid::new_v4()).collect();
        let purges = ref_purges(&refs);
        assert_eq!(purges.len(), 3);
        assert!(ref_purges(&[]).is_empty());
    }
}
