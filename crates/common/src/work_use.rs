//! Background work and what it uses of a person's plan: which job, monitor
//! or boss a turn belongs to, and the rows kept of its turns, its tokens and
//! its share of the plan's windows.
//!
//! A turn is background work exactly when it runs inside [`within`]: a
//! scheduled job's run, a monitor's firing, a boss's own turn (a check-in, a
//! notice, a wake-up). Anything else is the person's own work, which the
//! plan guard never holds.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// One unit of background work, as the person would name it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Work {
    /// Stable across runs: `job:<id>`, `monitor:<id>`, `boss:<agent>`,
    /// `heartbeat:<agent>`.
    pub key: String,
    /// What the person calls it: the job's name, "Atlas's own turns".
    pub label: String,
    /// The agent it runs on.
    pub agent_id: String,
    /// What the person said of it.
    #[serde(default)]
    pub allowed: Allowed,
}

/// What the person said of one unit of work, which the plan guard judges a
/// runaway by.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct Allowed {
    /// The share of its plan's longest window it may use an hour, in place
    /// of being held to its usual.
    pub hourly_limit: Option<f64>,
    /// When they last resumed it: what it did before is not held against it.
    pub resumed_at: Option<DateTime<Utc>>,
}

/// What kind of work a [`Work`] is, by its key.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkKind {
    Job,
    Monitor,
    Boss,
    Heartbeat,
    Other,
}

impl Work {
    pub fn job(agent_id: &str, id: &str, name: &str) -> Self {
        Self::new(format!("job:{id}"), name, agent_id)
    }

    pub fn monitor(agent_id: &str, id: &str, name: &str) -> Self {
        Self::new(format!("monitor:{id}"), name, agent_id)
    }

    /// A boss's own turns: its check-ins, notices and wake-ups.
    pub fn boss(agent_id: &str, name: &str) -> Self {
        Self::new(
            format!("boss:{agent_id}"),
            &format!("{name}'s own turns"),
            agent_id,
        )
    }

    pub fn heartbeat(agent_id: &str, name: &str) -> Self {
        Self::new(
            format!("heartbeat:{agent_id}"),
            &format!("{name}'s heartbeat"),
            agent_id,
        )
    }

    fn new(key: String, label: &str, agent_id: &str) -> Self {
        Self {
            key,
            label: label.to_string(),
            agent_id: agent_id.to_string(),
            allowed: Allowed::default(),
        }
    }

    /// This work, with what the person said of it.
    pub fn allowing(mut self, allowed: Allowed) -> Self {
        self.allowed = allowed;
        self
    }

    pub fn kind(&self) -> WorkKind {
        match self.key.split_once(':').map(|(k, _)| k) {
            Some("job") => WorkKind::Job,
            Some("monitor") => WorkKind::Monitor,
            Some("boss") => WorkKind::Boss,
            Some("heartbeat") => WorkKind::Heartbeat,
            _ => WorkKind::Other,
        }
    }

    /// The job's id, for a job's work.
    pub fn job_id(&self) -> Option<&str> {
        self.key.strip_prefix("job:")
    }
}

tokio::task_local! {
    static BACKGROUND: Work;
}

/// Run `fut` as `work`: its turns are background work, held by the plan
/// guard and counted toward `work`.
pub async fn within<F: std::future::Future>(work: Work, fut: F) -> F::Output {
    BACKGROUND.scope(work, fut).await
}

/// [`within`] when there is work, else `fut` as the person's own.
pub async fn within_opt<F: std::future::Future>(work: Option<Work>, fut: F) -> F::Output {
    match work {
        Some(w) => within(w, fut).await,
        None => fut.await,
    }
}

/// [`within`] when there is work to carry, as across a spawn: task-locals
/// don't cross a bare `tokio::spawn`.
pub fn within_if_present<F>(
    work: Option<Work>,
    fut: F,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = F::Output> + Send>>
where
    F: std::future::Future + Send + 'static,
    F::Output: Send + 'static,
{
    match work {
        Some(w) => Box::pin(BACKGROUND.scope(w, fut)),
        None => Box::pin(fut),
    }
}

/// The background work under way here; `None` is the person's own.
pub fn current() -> Option<Work> {
    BACKGROUND.try_with(Clone::clone).ok()
}

/// One thing kept of a unit of work: a turn it began, the tokens a call of it
/// used, or the share of a plan's window a call of it moved.
#[derive(Debug, Clone, PartialEq)]
pub struct WorkUseRow {
    pub at: DateTime<Utc>,
    pub work: String,
    pub label: String,
    pub agent_id: String,
    /// Whose plan or budget it drew on: a person's id, `install`, ...
    pub payer: Option<String>,
    /// `claude` or `chatgpt`, for a plan's share.
    pub plan: Option<String>,
    /// The length of the window `share` is of, in minutes.
    pub window_minutes: Option<u32>,
    /// Points of that window, 0 to 100.
    pub share: f64,
    pub turns: u32,
    pub tokens: u64,
}

impl WorkUseRow {
    fn of(work: &Work, at: DateTime<Utc>) -> Self {
        Self {
            at,
            work: work.key.clone(),
            label: work.label.clone(),
            agent_id: work.agent_id.clone(),
            payer: None,
            plan: None,
            window_minutes: None,
            share: 0.0,
            turns: 0,
            tokens: 0,
        }
    }

    /// A turn of `work` began.
    pub fn turn(work: &Work, payer: Option<&str>, at: DateTime<Utc>) -> Self {
        Self {
            payer: payer.map(str::to_string),
            turns: 1,
            ..Self::of(work, at)
        }
    }

    /// A call of `work` used `tokens`.
    pub fn tokens(work: &Work, payer: Option<&str>, tokens: u64, at: DateTime<Utc>) -> Self {
        Self {
            payer: payer.map(str::to_string),
            tokens,
            ..Self::of(work, at)
        }
    }

    /// A call of `work` moved `plan`'s `window_minutes` window by `share`.
    pub fn share(
        work: &Work,
        payer: Option<&str>,
        plan: &str,
        window_minutes: u32,
        share: f64,
        at: DateTime<Utc>,
    ) -> Self {
        Self {
            payer: payer.map(str::to_string),
            plan: Some(plan.to_string()),
            window_minutes: Some(window_minutes),
            share,
            ..Self::of(work, at)
        }
    }
}

/// One unit of work's use over a stretch of time, for one plan window (or,
/// with none, its turns and tokens alone).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkUse {
    pub work: String,
    pub label: String,
    pub agent_id: String,
    pub turns: u64,
    pub tokens: u64,
    /// Each plan window it moved: (plan, window minutes, points).
    pub shares: Vec<WorkShare>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WorkShare {
    pub plan: String,
    pub window_minutes: u32,
    pub share: f64,
}

impl WorkUse {
    /// Its share of the longest window it moved: the plan's weekly one,
    /// where the plan reports one.
    pub fn longest_share(&self) -> Option<&WorkShare> {
        self.shares.iter().max_by_key(|s| s.window_minutes)
    }
}

/// Folds kept rows into each unit of work's use, the latest label winning.
pub fn fold(rows: impl IntoIterator<Item = WorkUseRow>) -> Vec<WorkUse> {
    let mut out: Vec<WorkUse> = Vec::new();
    let mut latest: std::collections::HashMap<String, DateTime<Utc>> = Default::default();
    for r in rows {
        let i = match out.iter().position(|u| u.work == r.work) {
            Some(i) => i,
            None => {
                out.push(WorkUse {
                    work: r.work.clone(),
                    label: r.label.clone(),
                    agent_id: r.agent_id.clone(),
                    turns: 0,
                    tokens: 0,
                    shares: Vec::new(),
                });
                out.len() - 1
            }
        };
        let u = &mut out[i];
        if latest.get(&r.work).is_none_or(|at| r.at >= *at) {
            latest.insert(r.work.clone(), r.at);
            u.label = r.label.clone();
        }
        u.turns += u64::from(r.turns);
        u.tokens += r.tokens;
        if let (Some(plan), Some(minutes)) = (r.plan, r.window_minutes) {
            match u
                .shares
                .iter_mut()
                .find(|s| s.plan == plan && s.window_minutes == minutes)
            {
                Some(s) => s.share += r.share,
                None => u.shares.push(WorkShare {
                    plan,
                    window_minutes: minutes,
                    share: r.share,
                }),
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn work_is_background_only_inside_its_scope_and_crosses_a_spawn_when_carried() {
        assert!(current().is_none(), "the person's own");
        let job = Work::job("atlas", "j1", "email check");
        assert_eq!(job.kind(), WorkKind::Job);
        assert_eq!(job.job_id(), Some("j1"));
        within(job.clone(), async {
            assert_eq!(current().as_ref(), Some(&job));
            let carried = current();
            let seen = tokio::spawn(within_if_present(carried, async { current() }))
                .await
                .unwrap();
            assert_eq!(seen.as_ref(), Some(&job));
            let bare = tokio::spawn(async { current() }).await.unwrap();
            assert!(bare.is_none(), "a bare spawn drops it");
        })
        .await;
        assert_eq!(Work::boss("atlas", "Atlas").label, "Atlas's own turns");
        assert_eq!(Work::boss("atlas", "Atlas").kind(), WorkKind::Boss);
    }

    #[test]
    fn rows_fold_into_each_works_turns_tokens_and_shares() {
        let job = Work::job("atlas", "j1", "email check");
        let t = |s: &str| DateTime::parse_from_rfc3339(s).unwrap().to_utc();
        let mut renamed = job.clone();
        renamed.label = "inbox check".into();
        let rows = vec![
            WorkUseRow::turn(&job, Some("p"), t("2026-11-19T09:00:00Z")),
            WorkUseRow::tokens(&job, Some("p"), 1200, t("2026-11-19T09:00:01Z")),
            WorkUseRow::share(
                &job,
                Some("p"),
                "claude",
                10_080,
                0.5,
                t("2026-11-19T09:00:02Z"),
            ),
            WorkUseRow::share(
                &renamed,
                Some("p"),
                "claude",
                10_080,
                1.0,
                t("2026-11-19T10:00:00Z"),
            ),
            WorkUseRow::share(
                &job,
                Some("p"),
                "claude",
                300,
                2.0,
                t("2026-11-19T09:30:00Z"),
            ),
        ];
        let used = fold(rows);
        assert_eq!(used.len(), 1);
        let u = &used[0];
        assert_eq!((u.turns, u.tokens), (1, 1200));
        assert_eq!(u.label, "inbox check", "the latest name");
        let weekly = u.longest_share().unwrap();
        assert_eq!((weekly.window_minutes, weekly.share), (10_080, 1.5));
    }
}
