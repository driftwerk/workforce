//! What Desk knows of its sessions' pull requests on GitHub, read as the
//! person ([`super::github`]) and kept: closing and what the boss sees
//! inside a session read the same pull requests, by number or by branch.
//!
//! - **Kept.** A merged or closed pull request is read once, an open one
//!   again after [`PULL_AGAIN`], a branch's list after [`BRANCH_AGAIN`];
//!   the least lately used go past [`KEPT`].
//! - **Paced.** At most [`PACE`] reads a minute, the rest answered from
//!   what is kept, said with when it was read.
//! - **Held off.** GitHub rate-limiting Desk, nothing is read until it says
//!   (else [`HOLD`]); not connected, nothing for [`NOT_CONNECTED_FOR`].

use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::hash::Hash;
use std::sync::Arc;
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use parking_lot::Mutex;
use serde_json::{json, Value};

use super::github::{self, GithubError};
use super::{AgentService, CrewHub};

/// How long an open pull request's state is believed before it is read
/// again: GitHub's rate limit is everyone's, but a session waiting on its
/// CI is told within minutes ([`super::plan_watch`]).
const PULL_AGAIN: Duration = Duration::from_secs(5 * 60);
/// How long a branch's pull requests are believed.
const BRANCH_AGAIN: Duration = Duration::from_secs(5 * 60);
/// The most pull requests, and the most branches, kept.
const KEPT: usize = 512;
/// The most reads in [`PACE_WITHIN`].
const PACE: usize = 30;
const PACE_WITHIN: Duration = Duration::from_secs(60);
/// How long nothing is read once GitHub rate-limits Desk without saying.
const HOLD: Duration = Duration::from_secs(15 * 60);
/// How long GitHub not connected is believed.
const NOT_CONNECTED_FOR: Duration = Duration::from_secs(10 * 60);
/// The most failed checks named.
const FAILED_NAMED: usize = 5;

/// A pull request as GitHub last said.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Pull {
    pub number: u64,
    pub open: bool,
    pub merged: bool,
    pub draft: bool,
    pub head: Option<String>,
    /// Its checks on its head overall: `success`, `failure`, `pending`,
    /// `none yet`, or none when they couldn't be read.
    pub checks: Option<String>,
    /// How many of its checks failed.
    pub failed: u64,
    /// The first [`FAILED_NAMED`] of them, as anyone with access named
    /// them.
    pub failed_names: Vec<String>,
    /// When it merged, as GitHub says.
    pub merged_at: Option<String>,
    pub url: Option<String>,
}

impl Pull {
    /// Pull request `n` from GitHub's answer.
    fn of(n: u64, got: &Value) -> Self {
        let checks = &got["checks"];
        Self {
            number: got["number"].as_u64().unwrap_or(n),
            open: got["state"] == "open",
            merged: got["merged"] == true,
            draft: got["draft"] == true,
            head: text(&got["head_sha"]),
            checks: github::overall(checks).map(str::to_string),
            failed: checks["failure"].as_u64().unwrap_or(0),
            failed_names: checks["failed"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_str)
                .take(FAILED_NAMED)
                .map(str::to_string)
                .collect(),
            merged_at: text(&got["merged_at"]),
            url: text(&got["html_url"]),
        }
    }
}

fn text(v: &Value) -> Option<String> {
    v.as_str().map(str::to_string)
}

/// One of a branch's pull requests, as GitHub lists it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Listed {
    number: u64,
    open: bool,
    merged: bool,
}

/// The branch's pull request that is its work: the newest open, else the
/// newest merged.
fn its_pull(listed: &[Listed]) -> Option<u64> {
    let newest = |f: fn(&Listed) -> bool| listed.iter().filter(|l| f(l)).map(|l| l.number).max();
    newest(|l| l.open).or_else(|| newest(|l| l.merged))
}

/// Why what is answered wasn't read from GitHub just now.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Held {
    /// More reads this minute than Desk makes.
    Paced,
    /// GitHub is rate-limiting Desk.
    Limited,
}

/// What GitHub said, and when; `held` when it was kept, not read again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Seen<T> {
    pub value: T,
    pub at: DateTime<Utc>,
    pub held: Option<Held>,
}

impl<T> Seen<T> {
    /// Why it is as of when it was read, in words, in `tz`; none when it
    /// was read now.
    #[cfg_attr(
        not(test),
        allow(dead_code, reason = "crew.inside says it (DW-724, PR 4)")
    )]
    pub(crate) fn note(&self, tz: &chrono_tz::Tz) -> Option<String> {
        let at = self.at.with_timezone(tz).format("%H:%M");
        self.held.map(|h| match h {
            Held::Paced => format!("as of {at}"),
            Held::Limited => format!("GitHub is rate-limiting Desk; CI as of {at}"),
        })
    }

    fn map<U>(self, f: impl FnOnce(T) -> U) -> Seen<U> {
        Seen {
            value: f(self.value),
            at: self.at,
            held: self.held,
        }
    }
}

/// Why GitHub has nothing to answer with.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Unseen {
    NotConnected,
    /// Nothing is kept, and nothing is read now.
    Held(Held),
    /// GitHub couldn't be read, in the words the boss is told.
    Failed(String),
}

/// One answer kept: when it was read and last used.
#[derive(Debug, Clone)]
struct Kept<T> {
    value: T,
    read: Instant,
    at: DateTime<Utc>,
    used: Instant,
}

impl<T: Clone> Kept<T> {
    fn seen(&self, held: Option<Held>) -> Seen<T> {
        Seen {
            value: self.value.clone(),
            at: self.at,
            held,
        }
    }
}

/// What is known of GitHub between reads.
#[derive(Debug, Default)]
pub(crate) struct GithubSeen {
    pulls: HashMap<(String, u64), Kept<Pull>>,
    branches: HashMap<(String, String), Kept<Vec<Listed>>>,
    gate: Gate,
}

/// Whether GitHub may be read now.
#[derive(Debug, Default)]
struct Gate {
    /// When each read of the last [`PACE_WITHIN`] was made.
    reads: VecDeque<Instant>,
    /// Nothing is read before then: GitHub rate-limits Desk.
    held_until: Option<Instant>,
    /// When GitHub was last found not connected.
    not_connected_at: Option<Instant>,
}

impl Gate {
    /// Whether GitHub may be read at `now`, counted when it may.
    fn open(&mut self, now: Instant) -> Result<(), Unseen> {
        if self
            .not_connected_at
            .is_some_and(|at| now.saturating_duration_since(at) < NOT_CONNECTED_FOR)
        {
            return Err(Unseen::NotConnected);
        }
        if self.held_until.is_some_and(|until| now < until) {
            return Err(Unseen::Held(Held::Limited));
        }
        while self
            .reads
            .front()
            .is_some_and(|at| now.saturating_duration_since(*at) >= PACE_WITHIN)
        {
            self.reads.pop_front();
        }
        if self.reads.len() >= PACE {
            return Err(Unseen::Held(Held::Paced));
        }
        self.reads.push_back(now);
        Ok(())
    }

    /// Takes in how a read made at `now` went: GitHub connected, not, or
    /// rate-limiting Desk.
    fn learn(&mut self, got: &Result<Value, GithubError>, now: Instant) {
        match got {
            Ok(_) => self.not_connected_at = None,
            Err(GithubError::NotConnected) => self.not_connected_at = Some(now),
            Err(GithubError::RateLimited { retry_after, .. }) => {
                self.held_until = Some(now + retry_after.unwrap_or(HOLD));
            }
            Err(GithubError::Other(_)) => {}
        }
    }
}

/// What `key` answers at `now`: what is kept when it is `fresh`, else
/// nothing when GitHub may be read now, else what is kept, said held.
fn kept_or_read<K: Eq + Hash, T: Clone>(
    kept: &mut HashMap<K, Kept<T>>,
    gate: &mut Gate,
    key: &K,
    fresh: impl Fn(&Kept<T>) -> bool,
    now: Instant,
) -> Result<Option<Seen<T>>, Unseen> {
    match kept.get_mut(key) {
        Some(k) if fresh(k) => {
            k.used = now;
            Ok(Some(k.seen(None)))
        }
        found => match (gate.open(now), found) {
            (Ok(()), _) => Ok(None),
            (Err(Unseen::Held(h)), Some(k)) => {
                k.used = now;
                Ok(Some(k.seen(Some(h))))
            }
            (Err(e), _) => Err(e),
        },
    }
}

/// Keeps `value` under `key`, read at `now`, the least lately used going
/// past [`KEPT`].
fn keep<K: Eq + Hash + Clone, T: Clone>(
    map: &mut HashMap<K, Kept<T>>,
    key: K,
    value: T,
    now: Instant,
) -> Seen<T> {
    let kept = Kept {
        value,
        read: now,
        at: Utc::now(),
        used: now,
    };
    let seen = kept.seen(None);
    map.insert(key, kept);
    if map.len() > KEPT {
        if let Some(oldest) = map
            .iter()
            .min_by_key(|(_, k)| k.used)
            .map(|(k, _)| k.clone())
        {
            map.remove(&oldest);
        }
    }
    seen
}

/// What a failed read answers: GitHub rate-limiting Desk, what is kept
/// when anything is, said held.
fn failed<T>(kept: Option<Seen<T>>, error: GithubError) -> Result<Seen<T>, Unseen> {
    match (error, kept) {
        (GithubError::NotConnected, _) => Err(Unseen::NotConnected),
        (GithubError::RateLimited { .. }, Some(k)) => Ok(Seen {
            held: Some(Held::Limited),
            ..k
        }),
        (e, _) => Err(Unseen::Failed(e.to_string())),
    }
}

/// Pull request `n` of `repo`, through `seen`, read with `read` when GitHub
/// is to be read at `now`.
pub(crate) async fn pull<R, F>(
    seen: &Mutex<GithubSeen>,
    read: &R,
    repo: &str,
    n: u64,
    now: Instant,
) -> Result<Seen<Pull>, Unseen>
where
    R: Fn(&'static str, Value) -> F,
    F: Future<Output = Result<Value, GithubError>>,
{
    let key = (repo.to_string(), n);
    {
        let s = &mut *seen.lock();
        let fresh =
            |k: &Kept<Pull>| !k.value.open || now.saturating_duration_since(k.read) < PULL_AGAIN;
        if let Some(answer) = kept_or_read(&mut s.pulls, &mut s.gate, &key, fresh, now)? {
            return Ok(answer);
        }
    }
    let got = read(github::PR, json!({ "repo": repo, "number": n })).await;
    let s = &mut *seen.lock();
    s.gate.learn(&got, now);
    match got {
        Ok(got) => Ok(keep(&mut s.pulls, key, Pull::of(n, &got), now)),
        Err(e) => failed(s.pulls.get(&key).map(|k| k.seen(None)), e),
    }
}

/// The pull request that is `branch`'s work on `repo` (its newest open,
/// else its newest merged), through `seen`; none when it has neither.
pub(crate) async fn branch_pull<R, F>(
    seen: &Mutex<GithubSeen>,
    read: &R,
    repo: &str,
    branch: &str,
    now: Instant,
) -> Result<Seen<Option<Pull>>, Unseen>
where
    R: Fn(&'static str, Value) -> F,
    F: Future<Output = Result<Value, GithubError>>,
{
    let key = (repo.to_string(), branch.to_string());
    let answered = {
        let s = &mut *seen.lock();
        let fresh = |k: &Kept<Vec<Listed>>| now.saturating_duration_since(k.read) < BRANCH_AGAIN;
        kept_or_read(&mut s.branches, &mut s.gate, &key, fresh, now)?
    };
    let listed = match answered {
        Some(listed) => listed,
        None => {
            let got = read(github::BRANCH, json!({ "repo": repo, "branch": branch })).await;
            let s = &mut *seen.lock();
            s.gate.learn(&got, now);
            match got {
                Ok(got) => keep(&mut s.branches, key, listed_of(&got), now),
                Err(e) => failed(s.branches.get(&key).map(|k| k.seen(None)), e)?,
            }
        }
    };
    let Some(n) = its_pull(&listed.value) else {
        return Ok(listed.map(|_| None));
    };
    let p = pull(seen, read, repo, n, now).await?;
    Ok(Seen {
        at: p.at.min(listed.at),
        held: listed.held.or(p.held),
        value: Some(p.value),
    })
}

/// A branch's pull requests from GitHub's list of them.
fn listed_of(got: &Value) -> Vec<Listed> {
    got["pulls"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            Some(Listed {
                number: p["number"].as_u64()?,
                open: p["state"] == "open",
                merged: !p["merged_at"].is_null(),
            })
        })
        .collect()
}

impl CrewHub {
    /// How `service`'s GitHub reads are made: as the person.
    fn github_reader(
        &self,
        service: &Arc<AgentService>,
    ) -> impl Fn(
        &'static str,
        Value,
    ) -> std::pin::Pin<Box<dyn Future<Output = Result<Value, GithubError>> + Send>> {
        let person = self.person(Some(service)).unwrap_or_default();
        let service = Arc::clone(service);
        move |name, input| {
            let (person, service) = (person.clone(), Arc::clone(&service));
            Box::pin(async move {
                workforce_agent::runner::CURRENT_USER_ID
                    .scope(
                        person,
                        github::fetch(
                            service.vault.clone(),
                            service.global_vault.clone(),
                            &service.config,
                            &service.tools,
                            name,
                            &input,
                        ),
                    )
                    .await
            })
        }
    }

    /// Pull request `n` of `repo` as GitHub says, read as the person: a
    /// merged or closed one once, an open one again after [`PULL_AGAIN`].
    pub(super) async fn pull(
        &self,
        service: &Arc<AgentService>,
        repo: &str,
        n: u64,
    ) -> Option<Pull> {
        let read = self.github_reader(service);
        match pull(&self.github_seen, &read, repo, n, Instant::now()).await {
            Ok(seen) => Some(seen.value),
            Err(e) => {
                tracing::info!(%repo, n, error = ?e, "a session's pull request couldn't be read");
                None
            }
        }
    }

    /// The pull request that is `branch`'s work on `repo`, as GitHub says,
    /// read as the person.
    #[allow(dead_code, reason = "crew.inside reads it (DW-724, PR 4)")]
    pub(crate) async fn branch_pull(
        &self,
        service: &Arc<AgentService>,
        repo: &str,
        branch: &str,
    ) -> Result<Seen<Option<Pull>>, Unseen> {
        let read = self.github_reader(service);
        branch_pull(&self.github_seen, &read, repo, branch, Instant::now()).await
    }

    /// Takes `p` as what GitHub says of pull request `n` of `repo` now.
    #[cfg(test)]
    pub(crate) fn pull_seen(&self, repo: &str, n: u64, p: Pull) {
        keep(
            &mut self.github_seen.lock().pulls,
            (repo.to_string(), n),
            p,
            Instant::now(),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::super::github::tests::{hub, no_writes};
    use super::*;
    use wiremock::matchers::{header, method, path, path_regex};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    const MINUTE: Duration = Duration::from_secs(60);

    async fn pr_at(server: &MockServer, n: u64, body: Value, times: u64) {
        Mock::given(method("GET"))
            .and(path(format!("/repos/acme/web/pulls/{n}")))
            .respond_with(ResponseTemplate::new(200).set_body_json(body))
            .expect(times)
            .mount(server)
            .await;
    }

    async fn the_rest(server: &MockServer) {
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn an_open_pull_request_is_read_again_after_five_minutes_a_merged_one_never() {
        let server = MockServer::start().await;
        no_writes(&server).await;
        pr_at(
            &server,
            7,
            json!({"number": 7, "state": "open", "draft": true, "head_sha": "abc"}),
            2,
        )
        .await;
        pr_at(
            &server,
            8,
            json!({"number": 8, "state": "closed", "merged": true, "merged_at": "2026-11-20T10:00:00Z"}),
            1,
        )
        .await;
        the_rest(&server).await;
        let h = hub(&server);
        let read = |name, input| h.fetch("ann", name, input);
        let seen = Mutex::new(GithubSeen::default());
        let t0 = Instant::now();
        let open = pull(&seen, &read, "acme/web", 7, t0).await.unwrap();
        assert!(open.value.open && open.value.draft && open.held.is_none());
        assert_eq!(open.value.head.as_deref(), Some("abc"));
        pull(&seen, &read, "acme/web", 7, t0 + 4 * MINUTE)
            .await
            .unwrap();
        pull(&seen, &read, "acme/web", 7, t0 + 6 * MINUTE)
            .await
            .unwrap();
        for at in [t0, t0 + 60 * MINUTE] {
            let merged = pull(&seen, &read, "acme/web", 8, at).await.unwrap();
            assert!(merged.value.merged && !merged.value.open);
        }
    }

    #[tokio::test]
    async fn past_thirty_reads_a_minute_it_answers_from_what_is_kept() {
        let server = MockServer::start().await;
        no_writes(&server).await;
        Mock::given(method("GET"))
            .and(path_regex(r"^/repos/acme/web/pulls/\d+$"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"state": "open", "head_sha": "abc"})),
            )
            .expect(32)
            .mount(&server)
            .await;
        the_rest(&server).await;
        let h = hub(&server);
        let read = |name, input| h.fetch("ann", name, input);
        let seen = Mutex::new(GithubSeen::default());
        let t0 = Instant::now();
        pull(&seen, &read, "acme/web", 1, t0).await.unwrap();
        let later = t0 + 6 * MINUTE;
        for n in 2..=31 {
            let p = pull(&seen, &read, "acme/web", n, later).await.unwrap();
            assert_eq!(p.held, None, "read {n}");
        }
        let kept = pull(&seen, &read, "acme/web", 1, later).await.unwrap();
        assert_eq!(kept.held, Some(Held::Paced));
        assert!(kept
            .note(&chrono_tz::UTC)
            .is_some_and(|n| n.starts_with("as of ")));
        assert_eq!(
            pull(&seen, &read, "acme/web", 32, later).await,
            Err(Unseen::Held(Held::Paced))
        );
        let p = pull(&seen, &read, "acme/web", 1, later + MINUTE)
            .await
            .unwrap();
        assert_eq!(p.held, None, "a minute on, it reads again");
    }

    #[tokio::test]
    async fn a_rate_limit_stops_every_read_until_github_says() {
        let server = MockServer::start().await;
        no_writes(&server).await;
        let pr = |status| {
            Mock::given(method("GET"))
                .and(path("/repos/acme/web/pulls/9"))
                .respond_with(
                    ResponseTemplate::new(status)
                        .insert_header("retry-after", "120")
                        .set_body_json(json!({"number": 9, "state": "open", "message": "limit"})),
                )
        };
        pr(200).up_to_n_times(1).expect(1).mount(&server).await;
        pr(429).up_to_n_times(1).expect(1).mount(&server).await;
        pr(200).expect(1).mount(&server).await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/web/pulls/10"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"state": "open"})))
            .expect(0)
            .mount(&server)
            .await;
        the_rest(&server).await;
        let h = hub(&server);
        let read = |name, input| h.fetch("ann", name, input);
        let seen = Mutex::new(GithubSeen::default());
        let t0 = Instant::now();
        pull(&seen, &read, "acme/web", 9, t0).await.unwrap();
        let limited = pull(&seen, &read, "acme/web", 9, t0 + 6 * MINUTE)
            .await
            .unwrap();
        assert_eq!(limited.held, Some(Held::Limited));
        assert!(limited.value.open, "what was kept");
        assert!(limited
            .note(&chrono_tz::UTC)
            .is_some_and(|n| n.starts_with("GitHub is rate-limiting Desk; CI as of ")));
        assert_eq!(
            pull(&seen, &read, "acme/web", 10, t0 + 7 * MINUTE).await,
            Err(Unseen::Held(Held::Limited))
        );
        let again = pull(&seen, &read, "acme/web", 9, t0 + 9 * MINUTE)
            .await
            .unwrap();
        assert_eq!(again.held, None, "past its retry, it reads again");
    }

    #[test]
    fn a_rate_limit_without_a_retry_holds_a_quarter_hour() {
        let mut gate = Gate::default();
        let t0 = Instant::now();
        gate.learn(
            &Err(GithubError::RateLimited {
                retry_after: None,
                said: "limited".into(),
            }),
            t0,
        );
        assert_eq!(
            gate.open(t0 + 14 * MINUTE),
            Err(Unseen::Held(Held::Limited))
        );
        assert_eq!(gate.open(t0 + 15 * MINUTE), Ok(()));
    }

    #[tokio::test]
    async fn not_connected_is_believed_for_ten_minutes() {
        let server = MockServer::start().await;
        no_writes(&server).await;
        Mock::given(method("GET"))
            .and(header("authorization", "Bearer ann-token"))
            .respond_with(
                ResponseTemplate::new(401).set_body_json(json!({"message": "Bad credentials"})),
            )
            .expect(2)
            .mount(&server)
            .await;
        let h = hub(&server);
        let read = |name, input| h.fetch("ann", name, input);
        let seen = Mutex::new(GithubSeen::default());
        let t0 = Instant::now();
        for at in [t0, t0 + 9 * MINUTE, t0 + 10 * MINUTE] {
            assert_eq!(
                pull(&seen, &read, "acme/web", 3, at).await,
                Err(Unseen::NotConnected),
                "{:?}",
                at - t0
            );
        }
    }

    #[tokio::test]
    async fn a_branchs_work_is_its_newest_open_pull_request_its_list_kept_five_minutes() {
        let server = MockServer::start().await;
        no_writes(&server).await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/web/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"number": 11, "state": "closed", "merged_at": "2026-11-20T10:00:00Z"},
                {"number": 12, "state": "open", "merged_at": null},
                {"number": 10, "state": "closed", "merged_at": null},
            ])))
            .expect(2)
            .mount(&server)
            .await;
        pr_at(
            &server,
            12,
            json!({"number": 12, "state": "open", "head_sha": "abc",
                   "html_url": "https://github.com/acme/web/pull/12"}),
            2,
        )
        .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/web/commits/abc/check-runs"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"check_runs": [
                    {"name": "Clippy", "status": "completed", "conclusion": "success"},
                    {"name": "Test (1/6)", "status": "completed", "conclusion": "failure"},
                ]})),
            )
            .mount(&server)
            .await;
        the_rest(&server).await;
        let h = hub(&server);
        let read = |name, input| h.fetch("ann", name, input);
        let seen = Mutex::new(GithubSeen::default());
        let t0 = Instant::now();
        let got = branch_pull(&seen, &read, "acme/web", "fix", t0)
            .await
            .unwrap();
        let p = got.value.unwrap();
        assert_eq!(p.number, 12);
        assert_eq!(p.checks.as_deref(), Some("failure"));
        assert_eq!(
            (p.failed, p.failed_names),
            (1, vec!["Test (1/6)".to_string()])
        );
        assert_eq!(
            p.url.as_deref(),
            Some("https://github.com/acme/web/pull/12")
        );
        branch_pull(&seen, &read, "acme/web", "fix", t0 + 4 * MINUTE)
            .await
            .unwrap();
        branch_pull(&seen, &read, "acme/web", "fix", t0 + 6 * MINUTE)
            .await
            .unwrap();
    }

    #[test]
    fn a_branchs_work_is_its_newest_open_else_its_newest_merged() {
        let l = |number, open, merged| Listed {
            number,
            open,
            merged,
        };
        assert_eq!(
            its_pull(&[l(3, false, true), l(5, false, true), l(4, false, false)]),
            Some(5)
        );
        assert_eq!(its_pull(&[l(9, false, true), l(2, true, false)]), Some(2));
        assert_eq!(its_pull(&[l(1, false, false)]), None);
        assert_eq!(its_pull(&[]), None);
    }

    #[test]
    fn the_least_lately_used_go_past_what_is_kept() {
        let mut map = HashMap::new();
        let t0 = Instant::now();
        for n in 0..=KEPT as u64 {
            keep(&mut map, n, (), t0 + Duration::from_secs(n));
        }
        assert_eq!(map.len(), KEPT);
        assert!(!map.contains_key(&0));
    }
}
