//! A boss's standing verdicts at Act fully: one call per session gives a
//! verdict for each class of ordinary step it may ask
//! ([`Class::CACHEABLE`]), and a request of a class it allows is allowed
//! at once, with no call of its own. A verdict stands while the person's
//! directives for the session read the same ([`CrewHub::directive_words`]),
//! no word of theirs to it is newer, and the boss isn't in a turn with
//! them; a held session, a directive that is conditional or timed, and
//! every class never cached go to the boss's own decision every time.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use uuid::Uuid;
use workforce_common::oversight::Oversight;
use workforce_desk::computer::boss::Class;
use workforce_desk::decision::{BossDecision, How, Need};
use workforce_desk::model::SessionView;

use super::remote_decide::Decider;
use super::CrewHub;

/// Each boss's standing verdicts on each session, as of the directives
/// they were given under.
#[derive(Default)]
pub(super) struct Standings(Mutex<HashMap<(String, Uuid), Entry>>);

enum Entry {
    /// Being asked for, under the directives of this hash.
    Asking(u64),
    Given(Standing),
}

#[derive(Debug, Clone)]
struct Standing {
    /// The hash of the directive words it was given under.
    version: u64,
    at: DateTime<Utc>,
    /// The classes it allows, and why; any other is asked.
    allows: HashMap<Class, String>,
}

/// Words that make a directive hold only sometimes: what it allows then
/// can't stand for every request.
const CONDITIONAL: &[&str] = &[
    "unless",
    "until",
    "except",
    "only if",
    "after",
    "before",
    "today",
    "tonight",
    "tomorrow",
    "this week",
    "weekend",
    "monday",
    "tuesday",
    "wednesday",
    "thursday",
    "friday",
    "saturday",
    "sunday",
    "o'clock",
];

/// Whether one of the person's own directives in `words` (each a `- `
/// line) holds only sometimes.
fn conditional(words: &str) -> bool {
    words
        .lines()
        .filter(|l| l.trim_start().starts_with("- "))
        .map(str::to_lowercase)
        .any(|l| {
            let padded = format!(
                " {} ",
                l.replace(|c: char| !c.is_alphanumeric() && c != '\'', " ")
            );
            CONDITIONAL
                .iter()
                .any(|w| padded.contains(&format!(" {w} ")))
                || padded.split(' ').any(|t| {
                    t.strip_suffix("am")
                        .or_else(|| t.strip_suffix("pm"))
                        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
                })
        })
}

fn version_of(words: &str) -> u64 {
    let mut h = std::collections::hash_map::DefaultHasher::new();
    words.hash(&mut h);
    h.finish()
}

/// The prompt for `view`'s standing verdicts.
fn standing_prompt(session: &serde_json::Value, words: &str) -> String {
    let classes = Class::CACHEABLE
        .iter()
        .map(|c| c.as_str())
        .collect::<Vec<_>>()
        .join(", ");
    format!(
        "{} Give your standing verdict on each class of step this session may ask to go past its \
         sandbox, for as long as the person's directives below read as they do: each is allowed at \
         once, with no call of its own, when you allow its class. The classes: {classes}. \
         own_branch_push is the session's own branch to origin; commit, local_git and fetch_pull \
         keep work and branches; gh_read only reads; gh_pr_create opens a PR from its own branch; \
         test_build and package_install run the workspace's or a registry's code.\n\n\
         Answer one line per class, nothing else: `<class>: allow <why>`, or `<class>: ask <the \
         directive it rests on>` where a directive of the person's bears on it. A directive that \
         holds only sometimes (unless, until, only if, a time) is ask for every class it could \
         touch.\n\n{}{words}",
        super::decide::MARK,
        super::watcher::reported(&session.to_string(), "the session"),
    )
}

/// The classes `text` allows, and why: a class not said, or said
/// otherwise, is asked.
fn allows_of(text: &str) -> HashMap<Class, String> {
    text.lines()
        .filter_map(|l| {
            let (class, rest) = l.trim().trim_matches('`').split_once(':')?;
            let class = Class::parse(class).filter(|c| c.cacheable(true))?;
            let why = rest
                .trim()
                .strip_prefix("allow")
                .filter(|w| w.is_empty() || w.starts_with(char::is_whitespace))?
                .trim();
            Some((class, why.to_string()))
        })
        .collect()
}

impl CrewHub {
    /// The person's directives for `boss` on `session`, as every decision
    /// on it reads them: their standing orders and preferences, in their
    /// words.
    pub(super) async fn directive_words(&self, boss: &str, session: Uuid, o: &Oversight) -> String {
        let prefs = self
            .preferences_context(
                &workforce_desk::preference::boss_scope_of(o.scope.group()),
                Some(session),
                true,
            )
            .await
            .map(|p| format!("\n\n{}\n{}", p.listed, p.how))
            .unwrap_or_default();
        let orders = self
            .orders_words(boss, Some(session))
            .await
            .map(|w| format!("\n\n{w}"))
            .unwrap_or_default();
        format!("{orders}{prefs}")
    }

    /// Why `boss`'s standing verdict allows `view`'s request of `class`:
    /// none when its class is never answered so, the session is held, the
    /// person is in a turn with `boss`, or no verdict stands that allows
    /// it. With none standing under the directives as they read now, one
    /// is asked for, beside this request's own decision.
    pub(super) async fn standing_allow(
        self: &Arc<Self>,
        decider: &Decider,
        view: &SessionView,
        class: Class,
        boss: &str,
    ) -> Option<String> {
        if !class.cacheable(true) || self.minds.person_in_turn(boss) {
            return None;
        }
        if !matches!(self.hold_on(view.id).await, Ok(None)) {
            return None;
        }
        let words = self
            .directive_words(boss, view.id, &decider.oversight)
            .await;
        if conditional(&words) {
            return None;
        }
        let version = version_of(&words);
        let spoke = self.desk.person_spoke_at(view.id);
        let key = (boss.to_string(), view.id);
        let standing = {
            let mut all = self.standings.0.lock();
            match all.get(&key) {
                Some(Entry::Given(s))
                    if s.version == version && spoke.is_none_or(|t| t <= s.at) =>
                {
                    Some(s.clone())
                }
                Some(Entry::Asking(v)) if *v == version => None,
                _ => {
                    all.insert(key.clone(), Entry::Asking(version));
                    let crew = Arc::clone(self);
                    let (view, boss) = (view.clone(), boss.to_string());
                    tokio::spawn(async move {
                        let given = crew.ask_standing(&view, &boss, version, &words).await;
                        let mut all = crew.standings.0.lock();
                        if matches!(all.get(&key), Some(Entry::Asking(v)) if *v == version) {
                            match given {
                                Some(s) => all.insert(key, Entry::Given(s)),
                                None => all.remove(&key),
                            };
                        }
                    });
                    None
                }
            }
        }?;
        let why = standing.allows.get(&class)?;
        Some(format!("my standing verdict on {}: {why}", class.as_str()))
    }

    /// `boss`'s standing verdicts on `view`, under the directive `words`
    /// of hash `version`, recorded: none when its call fails.
    async fn ask_standing(
        &self,
        view: &SessionView,
        boss: &str,
        version: u64,
        words: &str,
    ) -> Option<Standing> {
        let decider = self.decider_of(boss, view)?;
        let (session, _) = self.session_brief(boss, view, &decider.oversight).await;
        let prompt = standing_prompt(&session, words);
        let at = Utc::now();
        let text = match tokio::time::timeout(
            super::decide::WITHIN,
            self.decide_by(&decider, &prompt),
        )
        .await
        {
            Ok(Ok(text)) => text,
            Ok(Err(e)) => {
                tracing::warn!(session = %view.id, %boss, why = %e.why, "a boss's standing verdicts couldn't be had");
                return None;
            }
            Err(_) => {
                tracing::warn!(session = %view.id, %boss, "a boss's standing verdicts took too long");
                return None;
            }
        };
        let s = Standing {
            version,
            at,
            allows: allows_of(&text),
        };
        self.record_standing(boss, view, &s).await;
        Some(s)
    }

    async fn record_standing(&self, boss: &str, view: &SessionView, s: &Standing) {
        let (allowed, asked): (Vec<Class>, Vec<Class>) = Class::CACHEABLE
            .into_iter()
            .partition(|c| s.allows.contains_key(c));
        let names = |cs: &[Class]| match cs.is_empty() {
            true => "none".to_string(),
            false => cs.iter().map(|c| c.as_str()).collect::<Vec<_>>().join(", "),
        };
        let what = format!(
            "standing verdicts on {}: allows {}; asks {}",
            view.label(),
            names(&allowed),
            names(&asked)
        );
        let mut d = BossDecision::new(
            boss,
            Need::Approval,
            &what,
            "the person's directives as they read now",
            How::Own,
        );
        d.session = Some(view.id);
        self.record(&mut d).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_directive_that_holds_only_sometimes_stands_for_nothing() {
        for words in [
            "- “push freely unless it's a release” (at 10:02)",
            "- “no deploys until Friday” (at 10:02)",
            "- “merge only if CI is green” (at 10:02)",
            "- “don't push after 6pm” (at 10:02)",
            "- “hold installs this week” (at 10:02)",
        ] {
            assert!(
                conditional(&format!("The person's standing orders:\n{words}")),
                "{words}"
            );
        }
        assert!(!conditional(
            "The person's standing orders, in their own words (they are the rule, both ways):\n\
             - “push your own branches and open PRs” (at 10:02)\n\
             Directives not answered yet: releases. When a decision falls under one, decide it"
        ));
    }

    #[test]
    fn a_class_stands_allowed_only_when_said_so() {
        let allows = allows_of(
            "own_branch_push: allow ordinary work\ncommit: allow\n`test_build: ask “no builds”`\n\
             force_push: allow\nlocal_git: allowed? maybe",
        );
        assert_eq!(
            allows
                .keys()
                .copied()
                .collect::<std::collections::BTreeSet<_>>(),
            [Class::OwnBranchPush, Class::Commit].into(),
            "never a class that isn't cached, nor one not plainly allowed"
        );
        assert_eq!(version_of("a"), version_of("a"));
        assert_ne!(version_of("a"), version_of("a "));
    }
}
