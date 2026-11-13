//! The person's preferences for their boss (the boss understands sessions,
//! H9): exact rules ("when a session wants to merge with CI red, make it
//! wait for green"), kept per person and per boss scope, that the boss
//! applies in place of asking. Unlike lessons or agent memory (fuzzy
//! recall, written by a model), each is a row that can be listed exactly,
//! changed or retired by id, and cited by the decision it made: "per your
//! preference from 11/12".
//!
//! When two apply, the more specific scope wins (a repository, then a
//! group, then every session), then the newer; the decision names both.
//! The hub checks only that a cited preference is there and active, so
//! what the person reads is true: it gates nothing.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::model::text_enum;

text_enum!(PrefSource {
    // An "always" answer to one of the boss's asks.
    Asked => "asked",
    // Said outright ("from now on, always …").
    Said => "said",
    // Learned from an Undo.
    Undone => "undone",
});

text_enum!(PrefState {
    Active => "active",
    // Set aside (by the person, or after two overrules) until resumed.
    Paused => "paused",
    // Forgotten: kept only so decisions that cite it still read true.
    Retired => "retired",
});

/// The boss scope a main boss's preferences are kept under; a group's
/// boss keeps them under the group's id.
pub const MAIN: &str = "main";

/// The boss scope of the boss of `group` (none: the main boss).
pub fn boss_scope_of(group: Option<&str>) -> String {
    group.unwrap_or(MAIN).to_string()
}

/// What a preference covers.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum PrefScope {
    /// Every session in the boss's scope.
    All,
    /// One sidebar group's sessions, by its id.
    Group(String),
    /// One repository's sessions, by its `owner/name`, lowercased.
    Repo(String),
}

impl PrefScope {
    /// `all` (or nothing), `group:<id>` or `repo:<owner/name>` (a remote's
    /// address is read as the repository it names).
    pub fn parse(text: &str) -> Result<Self, String> {
        let t = text.trim();
        if t.is_empty() || t.eq_ignore_ascii_case("all") {
            return Ok(Self::All);
        }
        if let Some(g) = t.strip_prefix("group:").map(str::trim) {
            return match g.is_empty() {
                true => Err("A group scope names the group: group:<id>.".into()),
                false => Ok(Self::Group(g.to_string())),
            };
        }
        if let Some(r) = t.strip_prefix("repo:").map(str::trim) {
            return crate::charge::repo_name(r)
                .map(|r| Self::Repo(r.path))
                .ok_or_else(|| "A repository scope names it: repo:<owner/name>.".to_string());
        }
        Err("scope is all, group:<id> or repo:<owner/name>.".into())
    }

    /// As it is kept and given back to the boss.
    pub fn text(&self) -> String {
        match self {
            Self::All => "all".into(),
            Self::Group(g) => format!("group:{g}"),
            Self::Repo(r) => format!("repo:{r}"),
        }
    }

    /// How specific it is: a repository over a group over everything.
    pub fn rank(&self) -> u8 {
        match self {
            Self::All => 0,
            Self::Group(_) => 1,
            Self::Repo(_) => 2,
        }
    }

    /// Whether it covers a session in `group` working in `repo`
    /// (`owner/name`); a session nobody knows the repository of is covered
    /// only by a scope that doesn't name one.
    pub fn covers(&self, group: Option<&str>, repo: Option<&str>) -> bool {
        match self {
            Self::All => true,
            Self::Group(g) => group == Some(g.as_str()),
            Self::Repo(r) => repo.is_some_and(|x| x.eq_ignore_ascii_case(r)),
        }
    }

    /// As the person reads it.
    pub fn said(&self) -> String {
        match self {
            Self::All => "every session".into(),
            Self::Group(g) => format!("group {g}"),
            Self::Repo(r) => format!("repo {r}"),
        }
    }
}

/// One of the person's preferences.
#[derive(Debug, Clone, PartialEq)]
pub struct Preference {
    pub id: Uuid,
    pub owner: String,
    /// [`MAIN`], or the group whose boss it is for.
    pub boss_scope: String,
    /// The case: "a session wants to merge with CI red".
    pub when_text: String,
    /// What to do then: "make it wait for green".
    pub then_text: String,
    pub scope: PrefScope,
    pub source: PrefSource,
    /// The open item it came from, when one.
    pub from_item: Option<Uuid>,
    pub created_at: DateTime<Utc>,
    pub changed_at: DateTime<Utc>,
    pub applied_count: i32,
    pub last_applied_at: Option<DateTime<Utc>>,
    pub overruled_count: i32,
    pub state: PrefState,
}

impl Preference {
    /// A new, active preference of the person's, now.
    pub fn new(
        boss_scope: &str,
        when: &str,
        then: &str,
        scope: PrefScope,
        source: PrefSource,
    ) -> Self {
        let now = crate::crew::now();
        Self {
            id: Uuid::new_v4(),
            owner: crate::rooms::YOU.to_string(),
            boss_scope: boss_scope.to_string(),
            when_text: plain_when(when),
            then_text: then.trim().trim_end_matches('.').to_string(),
            scope,
            source,
            from_item: None,
            created_at: now,
            changed_at: now,
            applied_count: 0,
            last_applied_at: None,
            overruled_count: 0,
            state: PrefState::Active,
        }
    }

    /// The day it was made, as the person reads it: "11/12".
    pub fn date(&self, tz: chrono_tz::Tz) -> String {
        self.created_at
            .with_timezone(&tz)
            .format("%m/%d")
            .to_string()
    }

    /// The rule in a few words: "a session wants to merge with CI red → make it wait for green".
    pub fn rule(&self) -> String {
        format!("{} → {}", self.when_text, self.then_text)
    }

    /// One line of the list the boss is given and reads out.
    pub fn line(&self, tz: chrono_tz::Tz) -> String {
        let used = match self.applied_count {
            0 => "not used yet".to_string(),
            1 => "used once".to_string(),
            n => format!("used {n} times"),
        };
        let state = match self.state {
            PrefState::Active => String::new(),
            s => format!(", {}", s.as_str()),
        };
        let overruled = match self.overruled_count {
            0 => String::new(),
            n => format!(", undone by you {n}×"),
        };
        format!(
            "- When {}: {} (preference {}, from {}, for {}, {used}{overruled}{state})",
            self.when_text,
            self.then_text,
            self.id,
            self.date(tz),
            self.scope.said(),
        )
    }

    /// Whether it wins over `other` when both apply: the more specific
    /// scope, then the newer.
    pub fn outranks(&self, other: &Preference) -> bool {
        (self.scope.rank(), self.created_at) > (other.scope.rank(), other.created_at)
    }
}

/// "When a session wants …" said as the case alone: "a session wants …".
pub fn plain_when(when: &str) -> String {
    let w = when.trim().trim_end_matches(['.', ':', ',']);
    let lower = w.to_lowercase();
    let w = match ["when ", "whenever ", "if "]
        .iter()
        .find(|p| lower.starts_with(*p))
    {
        Some(p) => w[p.len()..].trim_start(),
        None => w,
    };
    w.to_string()
}

/// `prefs` in the order they win: the most specific scope first, then the
/// newest.
pub fn ranked(mut prefs: Vec<Preference>) -> Vec<Preference> {
    prefs.sort_by(|a, b| {
        (b.scope.rank(), b.created_at, b.id).cmp(&(a.scope.rank(), a.created_at, a.id))
    });
    prefs
}

/// What a decision made by `applied` says of it, `close` when the case was
/// a close match and not the preference's own, and naming each of `over`,
/// the others that also applied and that it was chosen over.
pub fn cite(applied: &Preference, close: bool, over: &[Preference], tz: chrono_tz::Tz) -> String {
    let mut s = match close {
        false => format!(
            "per your preference from {} ({})",
            applied.date(tz),
            applied.rule()
        ),
        true => format!(
            "per your preference from {} ({}), as a close match: I read this case as covered; shout if wrong",
            applied.date(tz),
            applied.rule()
        ),
    };
    for o in over.iter().filter(|o| o.id != applied.id) {
        let how = if applied.outranks(o) {
            match applied.scope.rank() > o.scope.rank() {
                true => "over the broader one",
                false => "over the older one",
            }
        } else {
            match o.scope.rank() > applied.scope.rank() {
                true => "setting aside the more specific one",
                false => "setting aside the newer one",
            }
        };
        s.push_str(&format!(", {how} from {} ({})", o.date(tz), o.rule()));
    }
    s
}

/// The list the boss is given and reads out, in the order the preferences
/// win; at most `max` characters, the rest counted.
pub fn listed(prefs: &[Preference], tz: chrono_tz::Tz, max: usize) -> String {
    let mut out = String::new();
    let mut left = 0usize;
    for (i, p) in prefs.iter().enumerate() {
        let l = p.line(tz);
        // Cut in the order they win: a later one never shows in place of
        // one that outranks it.
        if out.len() + l.len() + 1 > max {
            left = prefs.len() - i;
            break;
        }
        out.push_str(&l);
        out.push('\n');
    }
    if left > 0 {
        out.push_str(&format!(
            "- and {left} more (crew.preferences lists them)\n"
        ));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pref(when: &str, scope: PrefScope, days_ago: i64) -> Preference {
        let mut p = Preference::new(
            MAIN,
            when,
            "make it wait for green",
            scope,
            PrefSource::Said,
        );
        p.created_at = Utc::now() - chrono::Duration::days(days_ago);
        p
    }

    #[test]
    fn a_scope_reads_all_a_group_or_a_repository() {
        assert_eq!(PrefScope::parse("").unwrap(), PrefScope::All);
        assert_eq!(PrefScope::parse("ALL").unwrap(), PrefScope::All);
        assert_eq!(
            PrefScope::parse("group:g-acme").unwrap(),
            PrefScope::Group("g-acme".into())
        );
        assert_eq!(
            PrefScope::parse("repo:git@github.com:Acme/Acme-Web.git").unwrap(),
            PrefScope::Repo("acme/acme-web".into())
        );
        assert!(PrefScope::parse("repo:").is_err());
        assert!(PrefScope::parse("everything").is_err());
        let r = PrefScope::Repo("acme/web".into());
        assert_eq!(PrefScope::parse(&r.text()).unwrap(), r);
        assert!(r.covers(None, Some("Acme/Web")));
        assert!(!r.covers(None, None));
        assert!(PrefScope::Group("g".into()).covers(Some("g"), None));
        assert!(!PrefScope::Group("g".into()).covers(None, None));
        assert!(PrefScope::All.covers(None, None));
    }

    #[test]
    fn the_most_specific_wins_then_the_newest() {
        let all_new = pref("any key", PrefScope::All, 1);
        let all_old = pref("any key", PrefScope::All, 9);
        let group = pref("a key", PrefScope::Group("g".into()), 20);
        let repo = pref("the staging key", PrefScope::Repo("acme/web".into()), 30);
        let order = ranked(vec![
            all_old.clone(),
            group.clone(),
            all_new.clone(),
            repo.clone(),
        ]);
        let ids: Vec<Uuid> = order.iter().map(|p| p.id).collect();
        assert_eq!(ids, vec![repo.id, group.id, all_new.id, all_old.id]);
        assert!(repo.outranks(&all_new), "specific over new");
        assert!(all_new.outranks(&all_old), "then newer");
        assert!(!all_old.outranks(&all_new));
    }

    #[test]
    fn a_decision_names_the_preference_and_the_one_it_won_over() {
        let tz = chrono_tz::UTC;
        let mut staging = pref(
            "When a session runs staging tests",
            PrefScope::Repo("acme/web".into()),
            1,
        );
        staging.then_text = "use the staging key".into();
        let mut any = pref("a session wants a key", PrefScope::All, 9);
        any.then_text = "ask before using any key".into();
        let line = cite(&staging, false, std::slice::from_ref(&any), tz);
        assert!(
            line.starts_with(&format!(
                "per your preference from {} (a session runs staging tests → use the staging key)",
                staging.date(tz)
            )),
            "{line}"
        );
        assert!(
            line.contains(&format!(
                "over the broader one from {} (a session wants a key → ask before using any key)",
                any.date(tz)
            )),
            "{line}"
        );
        // Chosen against the rule: said as it is, never hidden.
        let against = cite(&any, false, std::slice::from_ref(&staging), tz);
        assert!(
            against.contains("setting aside the more specific one"),
            "{against}"
        );
        let mut older = any.clone();
        older.id = Uuid::new_v4();
        older.created_at = any.created_at - chrono::Duration::days(3);
        assert!(cite(&any, false, &[older], tz).contains("over the older one"));
        let close = cite(&staging, true, &[], tz);
        assert!(
            close.starts_with(&format!("per your preference from {}", staging.date(tz))),
            "{close}"
        );
        assert!(close.contains("as a close match"), "{close}");
        assert!(close.contains("shout if wrong"), "{close}");
    }

    #[test]
    fn the_list_says_each_rule_its_id_and_use_and_counts_what_doesnt_fit() {
        let mut p = pref(
            "When a session wants to merge with CI red.",
            PrefScope::All,
            0,
        );
        p.applied_count = 3;
        let line = p.line(chrono_tz::UTC);
        assert!(
            line.starts_with("- When a session wants to merge with CI red: make it wait for green"),
            "{line}"
        );
        assert!(line.contains(&p.id.to_string()) && line.contains("used 3 times"));
        let many: Vec<Preference> = (0..50)
            .map(|i| pref(&format!("case {i}"), PrefScope::All, 0))
            .collect();
        let text = listed(&many, chrono_tz::UTC, 1_000);
        assert!(text.len() < 1_100, "{}", text.len());
        assert!(
            text.contains("more (crew.preferences lists them)"),
            "{text}"
        );
    }
}
