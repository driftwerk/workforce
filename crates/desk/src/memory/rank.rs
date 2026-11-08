//! How recalled hits are ordered. Pure:
//! `score = relevance × kind × recency × nearness × validity`, with every
//! weight in [`WEIGHTS`]. The tests pin the order they give, not the
//! numbers.

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::reach::Reader;

/// What a hit is, as it is weighed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HitKind {
    /// An item the person confirmed or wrote.
    Confirmed,
    Decision,
    OpenTodo,
    Fact,
    Exchange,
    /// An exchange with no reply, only tool steps.
    StepsOnly,
    /// An engine's own summary of what came before.
    Compaction,
}

/// Every weight `rank` uses, in one place.
pub struct Weights {
    pub confirmed: f32,
    pub decision: f32,
    pub open_todo: f32,
    pub fact: f32,
    pub exchange: f32,
    pub steps_only: f32,
    pub compaction: f32,
    /// Recency halves every this many days...
    pub half_life_days: f32,
    /// ...down to this.
    pub recency_floor: f32,
    pub same_work: f32,
    pub same_project: f32,
    pub same_group: f32,
    pub superseded: f32,
    pub flagged: f32,
}

pub const WEIGHTS: Weights = Weights {
    confirmed: 1.4,
    decision: 1.3,
    open_todo: 1.2,
    fact: 1.15,
    exchange: 1.0,
    steps_only: 0.7,
    compaction: 0.8,
    half_life_days: 14.0,
    recency_floor: 0.5,
    same_work: 1.25,
    same_project: 1.1,
    same_group: 1.0,
    superseded: 0.3,
    flagged: 0.8,
};

impl HitKind {
    fn weight(self) -> f32 {
        let w = &WEIGHTS;
        match self {
            HitKind::Confirmed => w.confirmed,
            HitKind::Decision => w.decision,
            HitKind::OpenTodo => w.open_todo,
            HitKind::Fact => w.fact,
            HitKind::Exchange => w.exchange,
            HitKind::StepsOnly => w.steps_only,
            HitKind::Compaction => w.compaction,
        }
    }
}

/// A hit read back from Postgres, before it is ranked.
#[derive(Debug, Clone, PartialEq)]
pub struct Candidate {
    pub of: Uuid,
    /// From 0 to 1.
    pub relevance: f32,
    pub kind: HitKind,
    pub at: DateTime<Utc>,
    pub session: Uuid,
    pub project: String,
    pub flagged: bool,
    pub superseded: bool,
}

/// `max(floor, 0.5^(age_days / half_life))`.
pub fn recency(at: DateTime<Utc>, now: DateTime<Utc>) -> f32 {
    let days = (now - at).num_seconds().max(0) as f32 / 86_400.0;
    0.5f32
        .powf(days / WEIGHTS.half_life_days)
        .max(WEIGHTS.recency_floor)
}

/// How near the hit is to a session reading it: its own work, its
/// project, or only its group. Every hit is as near the person and the
/// overseer.
fn nearness(c: &Candidate, reader: &Reader) -> f32 {
    match reader {
        Reader::Session {
            id, project, work, ..
        } => {
            if c.session == *id || work.contains(&c.session) {
                WEIGHTS.same_work
            } else if c.project == *project {
                WEIGHTS.same_project
            } else {
                WEIGHTS.same_group
            }
        }
        Reader::Person | Reader::Overseer(_) => WEIGHTS.same_group,
    }
}

/// A hit's score.
pub fn score(c: &Candidate, now: DateTime<Utc>, reader: &Reader) -> f32 {
    let validity = if c.superseded {
        WEIGHTS.superseded
    } else {
        1.0
    } * if c.flagged { WEIGHTS.flagged } else { 1.0 };
    c.relevance.clamp(0.0, 1.0)
        * c.kind.weight()
        * recency(c.at, now)
        * nearness(c, reader)
        * validity
}

/// `hits` best first; ties go to the newer, then by id, so the order is
/// the same each time.
pub fn rank(hits: Vec<Candidate>, now: DateTime<Utc>, reader: &Reader) -> Vec<(Candidate, f32)> {
    let mut scored: Vec<(Candidate, f32)> = hits
        .into_iter()
        .map(|c| {
            let s = score(&c, now, reader);
            (c, s)
        })
        .collect();
    scored.sort_by(|(a, sa), (b, sb)| sb.total_cmp(sa).then(b.at.cmp(&a.at)).then(a.of.cmp(&b.of)));
    scored
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::Duration;

    fn hit(n: u128, kind: HitKind, days_old: i64, now: DateTime<Utc>) -> Candidate {
        Candidate {
            of: Uuid::from_u128(n),
            relevance: 0.8,
            kind,
            at: now - Duration::days(days_old),
            session: Uuid::from_u128(100),
            project: "/dev/acme-web".into(),
            flagged: false,
            superseded: false,
        }
    }

    fn order(hits: Vec<Candidate>, now: DateTime<Utc>, reader: &Reader) -> Vec<u128> {
        rank(hits, now, reader)
            .into_iter()
            .map(|(c, _)| c.of.as_u128())
            .collect()
    }

    #[test]
    fn a_confirmed_decision_outranks_a_fresher_exchange_and_superseded_sinks() {
        let now = Utc::now();
        let p = Reader::Person;
        assert_eq!(
            order(
                vec![
                    hit(1, HitKind::Exchange, 0, now),
                    hit(2, HitKind::Confirmed, 5, now)
                ],
                now,
                &p
            ),
            [2, 1]
        );
        assert_eq!(
            order(
                vec![
                    hit(1, HitKind::Exchange, 0, now),
                    hit(2, HitKind::Decision, 3, now)
                ],
                now,
                &p
            ),
            [2, 1]
        );
        let mut old = hit(2, HitKind::Decision, 0, now);
        old.superseded = true;
        assert_eq!(
            order(vec![old, hit(1, HitKind::Exchange, 30, now)], now, &p),
            [1, 2]
        );
        let mut flagged = hit(2, HitKind::Exchange, 0, now);
        flagged.flagged = true;
        assert_eq!(
            order(vec![flagged, hit(1, HitKind::Exchange, 0, now)], now, &p),
            [1, 2]
        );
        assert_eq!(
            order(
                vec![
                    hit(2, HitKind::StepsOnly, 0, now),
                    hit(3, HitKind::Compaction, 0, now),
                    hit(1, HitKind::Exchange, 0, now)
                ],
                now,
                &p
            ),
            [1, 3, 2]
        );
    }

    #[test]
    fn newer_is_nearer_until_the_floor_and_relevance_still_leads() {
        let now = Utc::now();
        let p = Reader::Person;
        assert_eq!(
            order(
                vec![
                    hit(2, HitKind::Exchange, 20, now),
                    hit(1, HitKind::Exchange, 1, now)
                ],
                now,
                &p
            ),
            [1, 2]
        );
        // Past the floor, age no longer tells two apart: the newer still
        // goes first on the tie.
        assert_eq!(
            recency(now - Duration::days(60), now),
            WEIGHTS.recency_floor
        );
        assert_eq!(
            order(
                vec![
                    hit(2, HitKind::Exchange, 90, now),
                    hit(1, HitKind::Exchange, 60, now)
                ],
                now,
                &p
            ),
            [1, 2]
        );
        let mut relevant = hit(2, HitKind::Exchange, 30, now);
        relevant.relevance = 1.0;
        let mut weak = hit(1, HitKind::Exchange, 0, now);
        weak.relevance = 0.3;
        assert_eq!(order(vec![weak, relevant], now, &p), [2, 1]);
    }

    #[test]
    fn a_session_hears_its_own_work_first_then_its_project() {
        let now = Utc::now();
        let reader = Reader::Session {
            id: Uuid::from_u128(100),
            group: Some("g-acme".into()),
            project: "/dev/acme-web".into(),
            work: vec![Uuid::from_u128(101)],
        };
        let mut sibling = hit(2, HitKind::Exchange, 0, now);
        sibling.session = Uuid::from_u128(200);
        let mut elsewhere = hit(3, HitKind::Exchange, 0, now);
        elsewhere.session = Uuid::from_u128(300);
        elsewhere.project = "/dev/acme-api".into();
        let mut lineage = hit(4, HitKind::Exchange, 0, now);
        lineage.session = Uuid::from_u128(101);
        let got = order(
            vec![
                elsewhere.clone(),
                sibling.clone(),
                hit(1, HitKind::Exchange, 0, now),
                lineage.clone(),
            ],
            now,
            &reader,
        );
        assert_eq!(got, [1, 4, 2, 3]);
        // The person weighs them all alike: newest, then id.
        assert_eq!(
            order(vec![elsewhere, sibling, lineage], now, &Reader::Person),
            [2, 3, 4]
        );
    }
}
