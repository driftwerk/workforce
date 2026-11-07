//! Which Anthropic account a Claude credential is: this computer's own CLI
//! sign-in, or one of Desk's accounts. The person has a few accounts; the
//! CLI's sign-in is whichever of them it is logged in to, never one more.
//!
//! An identity is a hash of the account's organization and email, kept with
//! the email to show. Only Claude Code says them, and only for a claude.ai
//! sign-in (`claude auth status --json`, `authMethod: "claude.ai"`). A Desk
//! account's long-lived token (`claude setup-token`) is inference-only: its
//! status names no account (verified on 2.1.319), and Desk never calls the
//! API with it to ask. So a Desk account's identity is the person's word (a
//! manual link) or, failing that, its usage: its plan readings and the CLI
//! sign-in's are the same plan's when both report the same windows
//! resetting at the same second, used as far ([`Prints`]).

use std::collections::VecDeque;

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::usage::LimitWindow;

/// An Anthropic account: its key (a hash of its organization and email,
/// never either), and the email to show.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Identity {
    pub key: String,
    pub email: Option<String>,
}

impl Identity {
    /// The identity of the account `org` and `email` name.
    pub fn of(org: &str, email: &str) -> Option<Self> {
        let (org, email) = (org.trim(), email.trim());
        if org.is_empty() || email.is_empty() {
            return None;
        }
        let mut hash = Sha256::new();
        hash.update(b"claude-identity");
        hash.update([0]);
        hash.update(org.as_bytes());
        hash.update([0]);
        hash.update(email.to_lowercase().as_bytes());
        Some(Self {
            key: hex::encode(&hash.finalize()[..12]),
            email: Some(email.to_string()),
        })
    }

    /// The id its plan readings are kept by in the hub, the same whichever
    /// credential of it reported them.
    pub fn plan_id(&self) -> String {
        format!("identity:{}", self.key)
    }
}

/// How a Desk account came to be known as an identity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum LinkedBy {
    /// The person said so: kept until they change it, never overruled.
    Manual,
    /// Its readings matched the CLI sign-in's ([`Prints`]).
    Usage,
}

impl LinkedBy {
    pub fn as_str(self) -> &'static str {
        match self {
            LinkedBy::Manual => "manual",
            LinkedBy::Usage => "usage",
        }
    }

    pub fn parse(s: &str) -> Option<Self> {
        match s {
            "manual" => Some(LinkedBy::Manual),
            "usage" => Some(LinkedBy::Usage),
            _ => None,
        }
    }
}

/// A Desk account known as an identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Link {
    pub identity: Identity,
    pub by: LinkedBy,
    pub at: DateTime<Utc>,
}

/// How far apart two readings may be and still be read as one moment.
const TOGETHER: Duration = Duration::minutes(2);
/// How many distinct readings on each side must agree to link an account
/// by its usage.
pub const AGREEING: usize = 2;
/// How many of a side's readings are kept to compare.
const KEPT: usize = 8;

/// A reading's windows as compared: the 5-hour and the weekly window's
/// reset to the second, and how far each is used, as precisely as the
/// reading said. Resets are not on the hour (a real one reset at 13:20
/// UTC), so two plans rarely share both by chance.
#[derive(Debug, Clone, Copy, PartialEq, Serialize, Deserialize)]
pub struct Print {
    five_hour: (i64, f64),
    weekly: (i64, f64),
    pub at: DateTime<Utc>,
}

impl Print {
    /// The print of `windows`, read at `at`: only one that has both
    /// windows, each saying when it resets, is one.
    pub fn of(windows: &[LimitWindow], at: DateTime<Utc>) -> Option<Self> {
        let window = |minutes: u32| {
            let w = windows.iter().find(|w| w.minutes == minutes)?;
            Some((w.resets_at?.timestamp(), w.used_percent))
        };
        Some(Self {
            five_hour: window(300)?,
            weekly: window(10_080)?,
            at,
        })
    }

    /// Something to tell plans apart by: a window part used. Empty plans
    /// and full ones all look alike.
    fn telling(&self) -> bool {
        [self.five_hour.1, self.weekly.1]
            .iter()
            .any(|u| *u > 0.0 && *u < 100.0)
    }
}

/// What two prints read together say of their plans.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    /// One plan: the same resets to the second, used exactly as far.
    Same,
    /// Two plans: a window resets at another moment.
    Different,
    /// Nothing to go by: too far apart in time, or used differently while
    /// resetting together (one may have moved on since).
    Unsure,
}

pub fn compare(a: &Print, b: &Print) -> Verdict {
    if (a.at - b.at).abs() > TOGETHER {
        return Verdict::Unsure;
    }
    if a.five_hour.0 != b.five_hour.0 || a.weekly.0 != b.weekly.0 {
        return Verdict::Different;
    }
    let same = |x: f64, y: f64| (x - y).abs() < 1e-9;
    if same(a.five_hour.1, b.five_hour.1) && same(a.weekly.1, b.weekly.1) {
        Verdict::Same
    } else {
        Verdict::Unsure
    }
}

/// One side's recent prints, and the pairs of readings (the CLI sign-in's,
/// this side's) that agreed. Kept across restarts.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Prints {
    pub recent: VecDeque<Print>,
    /// Each agreeing pair, once: the CLI's reading's time, this side's.
    pub agreed: Vec<(DateTime<Utc>, DateTime<Utc>)>,
}

impl Prints {
    pub fn keep(&mut self, p: Print) {
        self.recent.push_back(p);
        while self.recent.len() > KEPT {
            self.recent.pop_front();
        }
    }

    /// `own`, the CLI sign-in's print, against this side's: whether they
    /// are the same plan now, two, or nothing can be said. An agreeing pair
    /// counts only when one of them has a window part used.
    pub fn against(&mut self, own: &Print) -> Verdict {
        let mut verdict = Verdict::Unsure;
        let mut agreeing = Vec::new();
        for mine in &self.recent {
            match compare(own, mine) {
                Verdict::Different => return Verdict::Different,
                Verdict::Same => {
                    verdict = Verdict::Same;
                    if own.telling() || mine.telling() {
                        agreeing.push((own.at, mine.at));
                    }
                }
                Verdict::Unsure => {}
            }
        }
        for pair in agreeing {
            if !self.agreed.contains(&pair) {
                self.agreed.push(pair);
            }
        }
        verdict
    }

    /// Whether enough distinct readings on each side agreed to link it.
    pub fn agrees(&self) -> bool {
        let distinct = |of: fn(&(DateTime<Utc>, DateTime<Utc>)) -> DateTime<Utc>| {
            let mut times: Vec<DateTime<Utc>> = self.agreed.iter().map(of).collect();
            times.sort();
            times.dedup();
            times.len()
        };
        distinct(|p| p.0) >= AGREEING && distinct(|p| p.1) >= AGREEING
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: i64) -> DateTime<Utc> {
        DateTime::from_timestamp(s, 0).unwrap()
    }

    fn windows(five: (f64, i64), week: (f64, i64)) -> Vec<LimitWindow> {
        vec![
            LimitWindow {
                used_percent: five.0,
                resets_at: Some(at(five.1)),
                minutes: 300,
            },
            LimitWindow {
                used_percent: week.0,
                resets_at: Some(at(week.1)),
                minutes: 10_080,
            },
        ]
    }

    #[test]
    fn a_weekly_window_alone_is_never_a_print_to_link_by() {
        // As Claude Code reports a plan now: the weekly window alone,
        // resetting on the hour, as two plans' often do.
        let weekly = vec![LimitWindow {
            used_percent: 26.0,
            resets_at: Some(at(1_794_596_400)),
            minutes: 10_080,
        }];
        assert_eq!(Print::of(&weekly, at(0)), None);
        assert!(Print::of(&windows((10.0, 3600), (26.0, 86_400)), at(0)).is_some());
    }

    #[test]
    fn an_identity_is_its_organization_and_email_never_either() {
        let a = Identity::of("org-1", "Kim@Example.com").unwrap();
        assert_eq!(
            Some(a.clone()),
            Identity::of(" org-1 ", "kim@example.com").map(|i| Identity {
                email: Some("Kim@Example.com".into()),
                ..i
            })
        );
        assert_ne!(a.key, Identity::of("org-2", "kim@example.com").unwrap().key);
        assert!(!a.key.contains("org") && !a.key.contains("kim"));
        assert_eq!(a.plan_id(), format!("identity:{}", a.key));
        assert_eq!(Identity::of("", "kim@example.com"), None);
    }

    #[test]
    fn two_readings_of_one_plan_agree_and_another_plans_never_do() {
        let own = Print::of(&windows((12.0, 1000), (40.0, 9000)), at(100)).unwrap();
        let same = Print::of(&windows((12.0, 1000), (40.0, 9000)), at(160)).unwrap();
        assert_eq!(compare(&own, &same), Verdict::Same);
        // Another plan resets at another moment.
        let other = Print::of(&windows((12.0, 1000), (40.0, 9600)), at(160)).unwrap();
        assert_eq!(compare(&own, &other), Verdict::Different);
        // Too far apart, or used differently, even by a fraction: nothing
        // to go by.
        let later = Print::of(&windows((12.0, 1000), (40.0, 9000)), at(400)).unwrap();
        assert_eq!(compare(&own, &later), Verdict::Unsure);
        let moved = Print::of(&windows((12.2, 1000), (40.0, 9000)), at(160)).unwrap();
        assert_eq!(compare(&own, &moved), Verdict::Unsure);
        // Only a reading with both windows and their resets is one.
        assert_eq!(
            Print::of(&windows((1.0, 1000), (1.0, 9000))[..1], at(1)),
            None
        );
    }

    #[test]
    fn it_takes_two_distinct_readings_each_side_with_a_window_part_used() {
        let mut work = Prints::default();
        // Empty plans and full ones look alike: never counted.
        for (used, t) in [(0.0, 100), (100.0, 200)] {
            work.keep(Print::of(&windows((used, 1000), (used, 9000)), at(t)).unwrap());
            let own = Print::of(&windows((used, 1000), (used, 9000)), at(t + 10)).unwrap();
            assert_eq!(work.against(&own), Verdict::Same);
        }
        assert!(work.agreed.is_empty());
        // One reading of Work's against two of the CLI's: not enough.
        work.keep(Print::of(&windows((5.0, 1000), (2.0, 9000)), at(300)).unwrap());
        work.against(&Print::of(&windows((5.0, 1000), (2.0, 9000)), at(310)).unwrap());
        work.against(&Print::of(&windows((5.0, 1000), (2.0, 9000)), at(320)).unwrap());
        assert!(
            !work.agrees(),
            "one of Work's readings, however often matched"
        );
        work.keep(Print::of(&windows((6.0, 1000), (2.0, 9000)), at(600)).unwrap());
        work.against(&Print::of(&windows((6.0, 1000), (2.0, 9000)), at(620)).unwrap());
        assert!(work.agrees());
        // Kept as it was across a restart.
        let kept: Prints = serde_json::from_value(serde_json::to_value(&work).unwrap()).unwrap();
        assert_eq!(kept, work);
    }
}
