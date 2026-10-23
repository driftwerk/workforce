//! Running a set: each question is asked of each arm's agent as the person,
//! through the hub's ordinary chat turn, in a session of its own; then every
//! answer is judged, twice, without the judge being told whose it is.
//!
//! Every turn and grade is recorded as it lands, so a run that stops (the
//! budget, a restart, Ctrl-C) carries on from there when it is started
//! again. A run that is told to stop starts nothing new and lets what is in
//! flight land, since the hub finishes and bills a turn whether or not
//! anyone waits for it. Arms run one after another, since an arm may lay
//! settings over a test copy for as long as it runs.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use chrono::Utc;
use eyre::{bail, ensure, eyre, Result, WrapErr};
use futures::future::ready;
use futures::stream::{self, StreamExt};
use serde::{Deserialize, Serialize};
use workforce_hub::eval_grader::{GradeOutcome, GradeResponse};
use workforce_hub::turn_extraction::EVAL_CHANNEL_PREFIX;
use workforce_orchestrator::workflow_store::content_hash;

use super::arms::{is_test_copy, Arm};
use super::hub::{AskError, Asked, Hub};
use super::results::{
    write_private, ArmEntry, GradeRecord, Judge, Store, TurnRecord, TurnStatus, GRADE_PASSES,
};
use super::set::{EvalSet, Question};

const BACKUP_DIR: &str = "config-backups";

const CARRY_ON: &str = "Everything so far is recorded. Run the same command again to carry on.";

/// Why an answer given before an arm's overrides reached the replica that
/// gave it is not the arm's answer.
const STALE: &str = "answered under a config other than the arm's: a hub replica had not taken \
                     up the overrides";

pub struct Options {
    /// How many turns, and then grades, are in flight at once.
    pub concurrency: usize,
    /// Ask again what failed, and grade again what went ungraded.
    pub retry: bool,
    /// Ask only the set's first questions.
    pub limit: Option<usize>,
    pub turn_timeout: Duration,
    pub allow_missing_credentials: bool,
    /// Leave each turn's session on the hub instead of deleting it once its
    /// answer is recorded.
    pub keep_sessions: bool,
    /// How long an arm's overrides are given to reach every replica of the
    /// hub before the arm's first question.
    pub settle: Duration,
}

/// A name for one go at one question, never used twice: it keys the turn's
/// telemetry and gives the turn a session of its own, even when an earlier
/// go at the same attempt was cut off and may have reached the hub.
fn turn_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()
}

/// The channel, and so the session, of the turn `turn_id`.
fn channel_of(turn_id: &str) -> String {
    format!("{EVAL_CHANNEL_PREFIX}{turn_id}")
}

/// Run `jobs`, at most `bound` at once. Once one fails, or Ctrl-C is
/// pressed, no further job starts; those in flight still finish, so what the
/// hub has already been asked for is recorded rather than asked for twice.
/// A second Ctrl-C abandons them. Fails with the first failure.
async fn drain<J, F>(jobs: Vec<J>, bound: usize, start: impl Fn(J) -> F) -> Result<()>
where
    F: Future<Output = Result<()>>,
{
    let stopping = AtomicBool::new(false);
    let mut running = stream::iter(jobs)
        .take_while(|_| ready(!stopping.load(Ordering::SeqCst)))
        .map(start)
        .buffer_unordered(bound.max(1));
    let mut stopped = None;
    let mut interrupted = false;
    // One listener for the whole of the loop, so a Ctrl-C that lands while a
    // result is being handled is still seen.
    let mut interrupt = Box::pin(tokio::signal::ctrl_c());
    loop {
        tokio::select! {
            landed = running.next() => match landed {
                None => break,
                Some(Ok(())) => {}
                Some(Err(e)) => {
                    stopping.store(true, Ordering::SeqCst);
                    stopped.get_or_insert(e);
                }
            },
            _ = &mut interrupt => {
                interrupt = Box::pin(tokio::signal::ctrl_c());
                ensure!(
                    !interrupted,
                    "interrupted twice: what was in flight is abandoned, and the hub may still \
                     finish and bill it. {CARRY_ON}"
                );
                interrupted = true;
                stopping.store(true, Ordering::SeqCst);
                stopped.get_or_insert(eyre!("interrupted. {CARRY_ON}"));
                eprintln!("stopping: waiting for what is in flight (Ctrl-C again abandons it)");
            }
        }
    }
    stopped.map_or(Ok(()), Err)
}

/// A test copy's config as an arm found it and as the arm left it, kept
/// beside the results from before the overrides are written until the config
/// is back, so a run that dies leaves the next one able to restore it.
#[derive(Serialize, Deserialize)]
struct Backup {
    agent: String,
    original: String,
    overridden: String,
}

/// An arm's overrides, laid over its test copy's config for as long as the
/// arm runs.
struct Overridden {
    backup: Backup,
    kept_at: PathBuf,
    etag: String,
}

/// Refuse to change the settings of an agent whose own config does not say
/// it is a test copy.
fn ensure_test_copy(arm: &Arm, config: &str) -> Result<()> {
    ensure!(
        is_test_copy(config),
        "arm {:?}: {agent} does not say it is a test copy, so its settings are left alone. If \
         nobody but you talks to {agent}, add\n\n[evals]\ncopy = true\n\nto its config.toml",
        arm.name,
        agent = arm.agent
    );
    Ok(())
}

impl Overridden {
    async fn apply(hub: &Hub, dir: &Path, arm: &Arm, settle: Duration) -> Result<Option<Self>> {
        if arm.overrides.is_empty() {
            return Ok(None);
        }
        let read = hub.config(&arm.agent).await?;
        ensure_test_copy(arm, &read.content)?;
        let backup = Backup {
            agent: arm.agent.clone(),
            overridden: arm.laid_over(&read.content)?,
            original: read.content,
        };
        let backups = dir.join(BACKUP_DIR);
        std::fs::create_dir_all(&backups)?;
        let kept_at = backups.join(format!("{}.json", arm.agent));
        write_private(&kept_at, &serde_json::to_string(&backup)?)?;
        // A write that fails may still have landed: the backup stays, and
        // the next run puts the config back if it did.
        let saved = hub
            .put_config(&arm.agent, &backup.overridden, &read.etag)
            .await?;
        let overridden = Self {
            backup,
            kept_at,
            etag: saved.etag,
        };
        if !saved.restart_required.is_empty() {
            overridden.restore(hub).await?;
            bail!(
                "arm {:?} overrides {}, which {} would only take up after a restart: it cannot \
                 be an arm's override",
                arm.name,
                saved.restart_required.join(", "),
                arm.agent
            );
        }
        tokio::time::sleep(settle).await;
        Ok(Some(overridden))
    }

    /// What every turn of the arm must say it ran under.
    fn config(&self) -> String {
        content_hash(&self.backup.overridden)
    }

    async fn restore(self, hub: &Hub) -> Result<()> {
        let agent = &self.backup.agent;
        hub.put_config(agent, &self.backup.original, &self.etag)
            .await
            .wrap_err_with(|| {
                format!(
                    "{agent}'s config still carries the arm's overrides; its own is kept in {}",
                    self.kept_at.display()
                )
            })?;
        Ok(std::fs::remove_file(&self.kept_at)?)
    }

    /// Put back every config an earlier run in `dir` died before restoring.
    /// One that has been changed since is left as it is: it is no longer
    /// this run's to overwrite.
    pub async fn restore_leftovers(hub: &Hub, dir: &Path) -> Result<()> {
        let Ok(backups) = std::fs::read_dir(dir.join(BACKUP_DIR)) else {
            return Ok(());
        };
        for backup in backups {
            let kept_at = backup?.path();
            let backup: Backup = serde_json::from_str(&std::fs::read_to_string(&kept_at)?)
                .wrap_err_with(|| format!("read {}", kept_at.display()))?;
            let agent = &backup.agent;
            let now = hub.config(agent).await?;
            if now.content == backup.overridden {
                hub.put_config(agent, &backup.original, &now.etag)
                    .await
                    .wrap_err_with(|| format!("restoring {agent}'s config from an earlier run"))?;
                eprintln!("restored {agent}'s config, left overridden by an earlier run");
            } else {
                ensure!(
                    now.content == backup.original,
                    "{agent}'s config was left overridden by an earlier run and has been changed \
                     since, so it is not put back for you. Its config from before that run is \
                     under \"original\" in {}: put back what you want of it by hand, then delete \
                     that file",
                    kept_at.display()
                );
            }
            std::fs::remove_file(&kept_at)?;
        }
        Ok(())
    }
}

/// Refuse a run that cannot give a fair answer, before it spends anything.
async fn preflight(
    hub: &Hub,
    set: &EvalSet,
    arms: &[Arm],
    judge: &Judge,
    options: &Options,
) -> Result<()> {
    let me = hub.me().await?;
    let mut pointless = set.questions[0].grading("-", &judge.agent, None);
    pointless.rubric.points.clear();
    ensure!(
        hub.runs_evals(&pointless).await?,
        "this hub predates `workforce eval`: it cannot name a turn, tell a refusal from an \
         answer, or grade. Update the hub first"
    );
    let agents = hub.agents().await?;
    let seen = |id: &str| agents.iter().find(|agent| agent.id == id);
    ensure!(
        seen(&judge.agent).is_some(),
        "{} cannot see the grader's agent {:?}",
        me.display_name,
        judge.agent
    );
    for arm in arms {
        let agent = seen(&arm.agent).ok_or_else(|| {
            eyre!(
                "arm {:?}: {} cannot see an agent {:?}",
                arm.name,
                me.display_name,
                arm.agent
            )
        })?;
        let overridden = match arm.overrides.is_empty() {
            true => None,
            false => {
                ensure!(
                    !agent.default,
                    "arm {:?} would change the settings of {:?}, the install's default agent",
                    arm.name,
                    arm.agent
                );
                let config = hub.config(&arm.agent).await?.content;
                ensure_test_copy(arm, &config)?;
                Some(arm.laid_over(&config)?)
            }
        };
        know_how_seen(hub, arm, overridden.as_deref()).await?;
        let Some(original) = &arm.copy_of else {
            continue;
        };
        let own = |key: &&String| {
            key.strip_prefix("users/")
                .is_none_or(|rest| rest.starts_with(&format!("{}/", me.id)))
        };
        match (
            hub.vault_keys(original).await?,
            hub.vault_keys(&arm.agent).await?,
        ) {
            (Some(theirs), Some(ours)) => {
                let ours: BTreeSet<&String> = ours.iter().collect();
                let missing: Vec<&str> = theirs
                    .iter()
                    .filter(own)
                    .filter(|key| !ours.contains(key))
                    .map(String::as_str)
                    .collect();
                ensure!(
                    missing.is_empty() || options.allow_missing_credentials,
                    "arm {:?}: {} lacks credentials {} has, so it could not reach the same \
                     data: {}. Copy them, or pass --allow-missing-credentials",
                    arm.name,
                    arm.agent,
                    original,
                    missing.join(", ")
                );
            }
            _ => eprintln!(
                "arm {:?}: could not compare {}'s credentials with {}'s; ask one question \
                 first (--limit 1) and check it reached the data",
                arm.name, arm.agent, original
            ),
        }
    }
    Ok(())
}

/// Refuse a copy that reads the know-how of an agent other than the one it
/// is a copy of, or that an arm shows know-how and that would be shown none
/// of it: such an arm would send the same prompts as one without and report
/// no difference as if it were a result. `config` is the copy's with the
/// arm's overrides laid over it, when it has any.
async fn know_how_seen(hub: &Hub, arm: &Arm, config: Option<&str>) -> Result<()> {
    let seen = hub.know_how(&arm.agent, config, &[]).await?;
    if let (Some(original), Some(source)) = (&arm.copy_of, &seen.source_agent) {
        ensure!(
            source == original,
            "arm {:?}: {} reads the know-how of {source:?} (`[know_how] source_agent`), not that \
             of {original:?}, the agent it is a copy of",
            arm.name,
            arm.agent
        );
    }
    if !seen.shown {
        return Ok(());
    }
    eprintln!(
        "[{}] know-how shown; entries visible to you: {}",
        arm.name, seen.visible
    );
    if arm.copy_of.is_none() {
        return Ok(());
    }
    ensure!(
        seen.matching,
        "arm {:?} shows know-how, but the hub has no embedder to match a request by, so it is \
         shown none",
        arm.name
    );
    ensure!(
        seen.visible > 0,
        "arm {:?}: {}",
        arm.name,
        match seen.source_agent {
            None => "this copy sees no know-how; set `[know_how] source_agent`",
            Some(_) => "none of the source's entries are visible to you",
        }
    );
    Ok(())
}

/// One arm as its questions are asked: the agent, and the config every
/// answer must have run under when the arm lays overrides.
struct Asking<'a> {
    arm: &'a Arm,
    entry: ArmEntry,
    config: Option<String>,
}

/// Ask `question`: the turn to record, or the hub's reason for not running
/// it at all.
async fn ask(
    hub: &Hub,
    asking: &Asking<'_>,
    question: &Question,
    attempt: u32,
    options: &Options,
) -> Result<Result<TurnRecord, String>> {
    let turn_id = turn_id();
    let started = Instant::now();
    let asked = hub
        .ask(
            &asking.arm.agent,
            &channel_of(&turn_id),
            &turn_id,
            &question.question,
            options.turn_timeout,
        )
        .await;
    let record = |status, answer, error| TurnRecord {
        arm: asking.entry.name.clone(),
        arm_hash: asking.entry.hash.clone(),
        question: question.id.clone(),
        question_hash: question.hash(),
        use_case: question.use_case,
        attempt,
        turn_id: turn_id.clone(),
        status,
        answer,
        error,
        session_id: None,
        wall_ms: started.elapsed().as_millis() as u64,
        capped: false,
        config: None,
        metrics: None,
        at: Utc::now(),
    };
    match asked {
        Ok(Asked::Answered {
            reply,
            session_id,
            task_id,
            capped,
            config,
            took,
        }) => {
            let metrics = hub.metrics(&task_id).await.unwrap_or_else(|e| {
                eprintln!(
                    "[{}] {}: no cost or rounds ({e:#})",
                    asking.entry.name, question.id
                );
                None
            });
            let stale = asking
                .config
                .as_ref()
                .is_some_and(|arms| config.as_ref() != Some(arms));
            let ran = match stale {
                true => record(TurnStatus::Failed, String::new(), Some(STALE.to_string())),
                false => record(TurnStatus::Answered, reply, None),
            };
            Ok(Ok(TurnRecord {
                session_id: Some(session_id),
                wall_ms: took.as_millis() as u64,
                capped,
                config,
                metrics,
                ..ran
            }))
        }
        Ok(Asked::Refused(reason)) => Ok(Err(reason)),
        Err(AskError::Turn(reason)) => {
            Ok(Ok(record(TurnStatus::Failed, String::new(), Some(reason))))
        }
        Err(AskError::Run(e)) => Err(e),
    }
}

/// Delete what `turn` left on the hub, so no later turn can find its answer.
/// A turn that failed names no session: it is looked for by its channel,
/// which the hub only lists for a system admin.
async fn forget(hub: &Hub, agent: &str, turn: &TurnRecord) {
    let deleted = async {
        let sessions = match &turn.session_id {
            Some(session) => vec![session.clone()],
            None => hub.sessions_on(agent, &channel_of(&turn.turn_id)).await?,
        };
        for session in sessions {
            hub.delete_session(agent, &session).await?;
        }
        eyre::Ok(())
    };
    if let Err(e) = deleted.await {
        eprintln!("[{}] {}: {e:#}", turn.arm, turn.question);
    }
}

async fn ask_all(
    hub: &Hub,
    store: &Store,
    asking: &Asking<'_>,
    pending: Vec<(&Question, u32)>,
    options: &Options,
) -> Result<()> {
    let (total, done) = (pending.len(), &AtomicUsize::new(0));
    let arm = &asking.entry.name;
    drain(
        pending,
        options.concurrency,
        |(question, attempt)| async move {
            let turn = ask(hub, asking, question, attempt, options)
                .await?
                .map_err(|refusal| {
                    eyre!("the hub refused a turn for arm {arm:?}: {refusal}\n{CARRY_ON}")
                })?;
            store.record_turn(&turn)?;
            if !options.keep_sessions {
                forget(hub, &asking.arm.agent, &turn).await;
            }
            eprintln!(
                "[{arm}] {}/{total} {} {} in {}s",
                done.fetch_add(1, Ordering::SeqCst) + 1,
                turn.question,
                match (turn.status, turn.capped) {
                    (TurnStatus::Answered, false) => "answered",
                    (TurnStatus::Answered, true) => "stopped at the round cap",
                    (TurnStatus::Failed, _) => "failed",
                },
                turn.wall_ms / 1000
            );
            Ok(())
        },
    )
    .await
}

async fn run_arm(
    hub: &Hub,
    set: &EvalSet,
    store: &Store,
    arm: &Arm,
    options: &Options,
) -> Result<()> {
    let entry = ArmEntry::from(arm);
    let results = store.results()?;
    let recorded = results.current_turns(&entry, set);
    let pending: Vec<(&Question, u32)> = set
        .questions
        .iter()
        .take(options.limit.unwrap_or(usize::MAX))
        .filter_map(|question| match recorded.get(question.id.as_str()) {
            None => Some((question, 1)),
            Some(turn) if turn.status == TurnStatus::Failed && options.retry => {
                Some((question, turn.attempt + 1))
            }
            Some(_) => None,
        })
        .collect();
    if pending.is_empty() {
        return Ok(());
    }
    let overridden = Overridden::apply(hub, store.dir(), arm, options.settle).await?;
    let asking = Asking {
        arm,
        entry,
        config: overridden.as_ref().map(Overridden::config),
    };
    let asked = ask_all(hub, store, &asking, pending, options).await;
    let restored = match overridden {
        Some(overridden) => overridden.restore(hub).await,
        None => Ok(()),
    };
    match (asked, restored) {
        (Err(asked), Err(restored)) => Err(asked.wrap_err(format!("{restored:#}"))),
        (asked, restored) => asked.and(restored),
    }
}

/// Judge every answered turn that still lacks a reading, in an order that
/// follows neither arms nor questions: by a hash seeded with the run's id.
async fn grade_all(hub: &Hub, set: &EvalSet, store: &Store, options: &Options) -> Result<()> {
    let results = store.results()?;
    let Judge { agent, model } = &store.manifest.judge;
    let run_id = store.manifest.run_id.as_str();
    let mut due: Vec<(String, &Question, &TurnRecord, u8)> = store
        .manifest
        .arms
        .iter()
        .flat_map(|arm| results.current_turns(arm, set).into_values())
        .filter(|turn| turn.status == TurnStatus::Answered)
        .filter_map(|turn| {
            let question = set.questions.iter().find(|q| q.id == turn.question)?;
            Some((question, turn))
        })
        .flat_map(|(question, turn)| (1..=GRADE_PASSES).map(move |pass| (question, turn, pass)))
        .filter(
            |(_, turn, pass)| match results.grade(&turn.turn_id, *pass) {
                None => true,
                Some(read) => {
                    options.retry && matches!(read.outcome, GradeOutcome::Ungraded { .. })
                }
            },
        )
        .map(|(question, turn, pass)| {
            let order = content_hash(&format!("{run_id}\n{}\n{pass}", turn.turn_id));
            (order, question, turn, pass)
        })
        .collect();
    due.sort_by(|a, b| a.0.cmp(&b.0));
    let (total, done) = (due.len(), &AtomicUsize::new(0));
    drain(
        due,
        options.concurrency,
        |(_, question, turn, pass)| async move {
            let request = question.grading(&turn.answer, agent, model.as_deref());
            let GradeResponse {
                outcome,
                model,
                cost_usd,
            } = hub.grade(&request).await?;
            if let GradeOutcome::Refused { reason } = &outcome {
                bail!("the hub refused to grade: {reason}\n{CARRY_ON}");
            }
            store.record_grade(&GradeRecord {
                turn_id: turn.turn_id.clone(),
                pass,
                outcome,
                model,
                cost_usd,
                at: Utc::now(),
            })?;
            eprintln!(
                "[grading] {}/{total}",
                done.fetch_add(1, Ordering::SeqCst) + 1
            );
            Ok(())
        },
    )
    .await
}

/// Look again for the sessions of turns an earlier go at this run recorded
/// as failed: the hub may have finished them after the run stopped waiting.
async fn forget_failed(hub: &Hub, store: &Store) -> Result<()> {
    let results = store.results()?;
    for turn in &results.turns {
        let arm = store.manifest.arms.iter().find(|arm| arm.name == turn.arm);
        if let Some(arm) = arm.filter(|_| turn.session_id.is_none()) {
            forget(hub, &arm.agent, turn).await;
        }
    }
    Ok(())
}

/// Ask `set` of every arm, then grade the answers. What is already recorded
/// in `store` for the same versions is not done again.
pub async fn run(
    hub: &Hub,
    set: &EvalSet,
    arms: &[Arm],
    store: &Store,
    options: &Options,
) -> Result<()> {
    Overridden::restore_leftovers(hub, store.dir()).await?;
    preflight(hub, set, arms, &store.manifest.judge, options).await?;
    if !options.keep_sessions {
        forget_failed(hub, store).await?;
    }
    for arm in arms {
        run_arm(hub, set, store, arm, options).await?;
    }
    grade_all(hub, set, store, options).await
}

/// Put back the configs an earlier run in `dir` left overridden.
pub async fn restore(hub: &Hub, dir: &Path) -> Result<()> {
    Overridden::restore_leftovers(hub, dir).await
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use serde_json::{json, Value};
    use wiremock::matchers::{method, path, path_regex};
    use wiremock::{Mock, MockServer, Request, ResponseTemplate};

    use super::*;
    use crate::evals::arms;
    use crate::evals::set::tests::example_dir;

    const TOKEN: &str = "a-persons-token";

    fn options() -> Options {
        Options {
            concurrency: 1,
            retry: false,
            limit: None,
            turn_timeout: Duration::from_secs(30),
            allow_missing_credentials: false,
            keep_sessions: false,
            settle: Duration::ZERO,
        }
    }

    fn body(request: &Request) -> Value {
        serde_json::from_slice(&request.body).unwrap()
    }

    fn answered(request: &Request) -> ResponseTemplate {
        let asked = body(request);
        ResponseTemplate::new(200).set_body_json(json!({
            "response": format!("{} says 42", asked["agent_id"].as_str().unwrap()),
            "session_id": format!("session-{}", asked["turn_id"].as_str().unwrap()),
            "task_id": format!("chat:me:{}", asked["turn_id"].as_str().unwrap()),
            "outcome": "answered",
        }))
    }

    /// The hub's refusal of a turn its asker's budget does not admit.
    fn refused(request: &Request) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "response": "Daily LLM budget reached for your account: $5 of $5 spent today.",
            "session_id": "session",
            "task_id": format!("chat:me:{}", body(request)["turn_id"].as_str().unwrap()),
            "outcome": "refused",
        }))
    }

    fn graded(_: &Request) -> ResponseTemplate {
        ResponseTemplate::new(200).set_body_json(json!({
            "status": "graded", "verdict": "correct", "cited": true, "met": [],
            "fell_for_trap": false, "reason": "right", "model": "judge-model", "cost_usd": 0.01,
        }))
    }

    /// A hub with a signed-in person, three agents, telemetry for every
    /// turn, and `chat` and `grade` answering as given.
    async fn hub_with(
        chat: impl Fn(&Request) -> ResponseTemplate + Send + Sync + 'static,
        grade: impl Fn(&Request) -> ResponseTemplate + Send + Sync + 'static,
    ) -> (MockServer, Hub) {
        let server = MockServer::start().await;
        let reply = |body: Value| ResponseTemplate::new(200).set_body_json(body);
        Mock::given(method("GET"))
            .and(path("/api/me"))
            .respond_with(reply(json!({"id": "me", "display_name": "Alice"})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/agents"))
            .respond_with(reply(json!({"agents": [
                {"id": "analyst", "default": true},
                {"id": "analyst-eval"},
                {"id": "judge"},
            ]})))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex("^/api/stats/tasks/chat:me:[0-9a-f]{32}$"))
            .respond_with(reply(json!({
                "task": {
                    "model": "standard-model", "cost_usd": 0.25, "input_tokens": 1000,
                    "output_tokens": 200, "cache_creation_input_tokens": 10,
                    "cache_read_input_tokens": 500, "rounds": 4,
                },
                "tools": [{"tool_name": "database.query"}, {"tool_name": "repo.read"}],
            })))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/sessions"))
            .respond_with(reply(json!({"sessions": []})))
            .mount(&server)
            .await;
        Mock::given(method("DELETE"))
            .and(path_regex("^/api/sessions/session-[0-9a-f]{32}$"))
            .respond_with(reply(json!({"deleted": true})))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/evals/grade"))
            .and(|request: &Request| body(request)["rubric"]["points"] == json!([]))
            .respond_with(
                ResponseTemplate::new(400)
                    .set_body_json(json!({"error": "the rubric needs at least one point"})),
            )
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/evals/know-how"))
            .respond_with(reply(json!({
                "enabled": false, "shown": false, "matching": true, "source_agent": null,
                "min_score": 0.75, "visible": 0, "matched": [],
            })))
            .with_priority(10)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/chat"))
            .respond_with(chat)
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(path("/api/evals/grade"))
            .respond_with(grade)
            .mount(&server)
            .await;
        let hub = Hub::new(&server.uri(), TOKEN).unwrap();
        (server, hub)
    }

    async fn sent(server: &MockServer, verb: &str, to: &str) -> Vec<Request> {
        server
            .received_requests()
            .await
            .unwrap()
            .into_iter()
            .filter(|r| r.method.as_str() == verb && r.url.path() == to)
            .collect()
    }

    /// Every answer sent to the judge (the preflight's pointless rubric is
    /// not one).
    async fn judged(server: &MockServer) -> Vec<Request> {
        sent(server, "POST", "/api/evals/grade")
            .await
            .into_iter()
            .filter(|request| body(request)["rubric"]["points"] != json!([]))
            .collect()
    }

    fn example() -> EvalSet {
        EvalSet::load(&example_dir()).unwrap()
    }

    fn open(dir: &Path, set: &EvalSet, arms: &[Arm]) -> Store {
        let judge = Judge {
            agent: "judge".into(),
            model: None,
        };
        Store::open(dir, set, arms, judge).unwrap()
    }

    const TWO_ARMS: &str = r#"
        [[arm]]
        name = "current"
        agent = "analyst"

        [[arm]]
        name = "candidate"
        agent = "analyst-eval"
        copy_of = "analyst"
    "#;

    #[tokio::test]
    async fn every_question_is_asked_as_the_person_in_a_session_of_its_own_and_graded_twice() {
        let (server, hub) = hub_with(answered, graded).await;
        let (set, arms, dir) = (
            example(),
            [Arm::of_agent("analyst")],
            tempfile::tempdir().unwrap(),
        );
        let store = open(dir.path(), &set, &arms);
        run(&hub, &set, &arms, &store, &options()).await.unwrap();

        let chats = sent(&server, "POST", "/api/chat").await;
        assert_eq!(chats.len(), 3);
        let channels: BTreeSet<String> = chats
            .iter()
            .map(|chat| body(chat)["channel"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(channels.len(), 3, "each question has its own session");
        assert!(channels.iter().all(|c| c.starts_with(EVAL_CHANNEL_PREFIX)));
        for chat in &chats {
            let auth = chat.headers.get("authorization").unwrap().to_str().unwrap();
            assert_eq!(auth, format!("Bearer {TOKEN}"));
        }
        let results = store.results().unwrap();
        assert_eq!(results.turns.len(), 3);
        let turn = &results.turns[0];
        assert_eq!(turn.status, TurnStatus::Answered);
        assert_eq!(turn.answer, "analyst says 42");
        let metrics = turn.metrics.as_ref().unwrap();
        assert_eq!((metrics.cost_usd, metrics.rounds), (0.25, 4));
        assert_eq!((metrics.input_tokens, metrics.output_tokens), (1000, 200));
        assert_eq!(metrics.tools, ["database.query", "repo.read"]);
        assert_eq!(results.grades.len(), 3 * usize::from(GRADE_PASSES));
        for turn in &results.turns {
            for pass in 1..=GRADE_PASSES {
                assert!(results.grade(&turn.turn_id, pass).is_some());
            }
        }
    }

    #[tokio::test]
    async fn a_run_the_budget_stops_carries_on_from_where_it_was() {
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&asked);
        let budget_after_one = move |request: &Request| match counter.fetch_add(1, Ordering::SeqCst)
        {
            1 => refused(request),
            _ => answered(request),
        };
        let (server, hub) = hub_with(budget_after_one, graded).await;
        let (set, arms, dir) = (
            example(),
            [Arm::of_agent("analyst")],
            tempfile::tempdir().unwrap(),
        );
        let store = open(dir.path(), &set, &arms);

        let stopped = run(&hub, &set, &arms, &store, &options())
            .await
            .unwrap_err();
        assert!(format!("{stopped:#}").contains("Daily LLM budget reached"));
        let results = store.results().unwrap();
        assert_eq!(results.turns.len(), 1, "the refusal is not an answer");
        assert!(
            results.grades.is_empty(),
            "nothing is graded until every arm is asked"
        );

        let store = open(dir.path(), &set, &arms);
        run(&hub, &set, &arms, &store, &options()).await.unwrap();
        let results = store.results().unwrap();
        assert_eq!(results.turns.len(), 3);
        assert_eq!(
            asked.load(Ordering::SeqCst),
            4,
            "the answered question is not asked again"
        );
        assert_eq!(judged(&server).await.len(), 6);

        run(&hub, &set, &arms, &store, &options()).await.unwrap();
        assert_eq!(
            asked.load(Ordering::SeqCst),
            4,
            "a finished run asks nothing"
        );
        assert_eq!(judged(&server).await.len(), 6);
    }

    #[tokio::test]
    async fn a_question_cut_off_mid_turn_is_asked_again_in_a_session_of_its_own() {
        let slow = |request: &Request| answered(request).set_delay(Duration::from_millis(300));
        let (server, hub) = hub_with(slow, graded).await;
        let (set, arms, dir) = (
            example(),
            [Arm::of_agent("analyst")],
            tempfile::tempdir().unwrap(),
        );
        let store = open(dir.path(), &set, &arms);
        let cut_off = tokio::time::timeout(
            Duration::from_millis(150),
            run(&hub, &set, &arms, &store, &options()),
        )
        .await;
        assert!(cut_off.is_err(), "the run was dropped mid-turn");
        assert!(store.results().unwrap().turns.is_empty());

        run(&hub, &set, &arms, &store, &options()).await.unwrap();
        let chats = sent(&server, "POST", "/api/chat").await;
        let asked = |n: usize| body(&chats[n]);
        assert_eq!(chats.len(), 4);
        assert_eq!(asked(0)["message"], asked(1)["message"]);
        assert_ne!(asked(0)["channel"], asked(1)["channel"]);
        assert_ne!(asked(0)["turn_id"], asked(1)["turn_id"]);
    }

    #[tokio::test]
    async fn a_recorded_answers_session_is_deleted_unless_kept() {
        let (server, hub) = hub_with(answered, graded).await;
        let (set, arms) = (example(), [Arm::of_agent("analyst")]);
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path(), &set, &arms);
        run(&hub, &set, &arms, &store, &options()).await.unwrap();
        let deleted: BTreeSet<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "DELETE")
            .map(|r| {
                assert_eq!(r.url.query(), Some("agent_id=analyst"));
                r.url
                    .path()
                    .trim_start_matches("/api/sessions/")
                    .to_string()
            })
            .collect();
        let recorded: BTreeSet<String> = store
            .results()
            .unwrap()
            .turns
            .into_iter()
            .filter_map(|turn| turn.session_id)
            .collect();
        assert_eq!(recorded.len(), 3);
        assert_eq!(deleted, recorded);

        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path(), &set, &arms);
        let keeping = Options {
            keep_sessions: true,
            ..options()
        };
        run(&hub, &set, &arms, &store, &keeping).await.unwrap();
        let deletes = server.received_requests().await.unwrap();
        let deletes = deletes.iter().filter(|r| r.method.as_str() == "DELETE");
        assert_eq!(deletes.count(), 3, "none on the second run");
    }

    #[tokio::test]
    async fn a_hub_that_predates_evals_is_refused_before_a_turn_is_spent() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/me"))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"id": "me", "display_name": "A"})),
            )
            .mount(&server)
            .await;
        let hub = Hub::new(&server.uri(), TOKEN).unwrap();
        let (set, arms, dir) = (
            example(),
            [Arm::of_agent("analyst")],
            tempfile::tempdir().unwrap(),
        );
        let store = open(dir.path(), &set, &arms);
        let refused = run(&hub, &set, &arms, &store, &options())
            .await
            .unwrap_err();
        assert!(format!("{refused:#}").contains("predates"), "{refused:#}");
        assert!(sent(&server, "POST", "/api/chat").await.is_empty());
    }

    #[tokio::test]
    async fn a_failed_turn_is_recorded_not_graded_and_asked_again_only_on_retry() {
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&asked);
        let first_fails = move |request: &Request| match counter.fetch_add(1, Ordering::SeqCst) {
            0 => ResponseTemplate::new(500),
            _ => answered(request),
        };
        let (server, hub) = hub_with(first_fails, graded).await;
        let (set, arms, dir) = (
            example(),
            [Arm::of_agent("analyst")],
            tempfile::tempdir().unwrap(),
        );
        let store = open(dir.path(), &set, &arms);

        run(&hub, &set, &arms, &store, &options()).await.unwrap();
        let results = store.results().unwrap();
        let failed: Vec<&TurnRecord> = results
            .turns
            .iter()
            .filter(|turn| turn.status == TurnStatus::Failed)
            .collect();
        assert_eq!(failed.len(), 1);
        assert!(failed[0].error.as_deref().unwrap().contains("500"));
        assert_eq!(results.grades.len(), 4, "only the two answers are graded");

        run(&hub, &set, &arms, &store, &options()).await.unwrap();
        assert_eq!(
            asked.load(Ordering::SeqCst),
            3,
            "a failure stands until --retry"
        );

        let retry = Options {
            retry: true,
            ..options()
        };
        run(&hub, &set, &arms, &store, &retry).await.unwrap();
        assert_eq!(asked.load(Ordering::SeqCst), 4);
        let results = store.results().unwrap();
        let entry = ArmEntry::from(&arms[0]);
        let current = results.current_turns(&entry, &set);
        let again = current[failed[0].question.as_str()];
        assert_eq!((again.status, again.attempt), (TurnStatus::Answered, 2));
        assert_ne!(
            again.turn_id, failed[0].turn_id,
            "a new attempt is a new session"
        );
        assert_eq!(judged(&server).await.len(), 6);
    }

    #[tokio::test]
    async fn turns_in_flight_never_exceed_the_bound() {
        const TURN: Duration = Duration::from_millis(400);
        let arrivals = Arc::new(parking_lot::Mutex::new(Vec::new()));
        for (concurrency, overlapping) in [(1, false), (2, true)] {
            arrivals.lock().clear();
            let seen = Arc::clone(&arrivals);
            let slow = move |request: &Request| {
                seen.lock().push(Instant::now());
                answered(request).set_delay(TURN)
            };
            let (_server, hub) = hub_with(slow, graded).await;
            let (set, arms, dir) = (
                example(),
                [Arm::of_agent("analyst")],
                tempfile::tempdir().unwrap(),
            );
            let store = open(dir.path(), &set, &arms);
            let bounded = Options {
                concurrency,
                ..options()
            };
            run(&hub, &set, &arms, &store, &bounded).await.unwrap();
            let at = arrivals.lock().clone();
            assert_eq!(at.len(), 3);
            assert!(
                at[concurrency] - at[0] >= TURN,
                "turn {concurrency} waited for one of the first {concurrency} to finish"
            );
            assert_eq!(at[1] - at[0] < TURN, overlapping);
        }
    }

    #[tokio::test]
    async fn the_judge_cannot_tell_which_arm_an_answer_came_from() {
        let (server, hub) = hub_with(answered, graded).await;
        let (set, arms, dir) = (
            example(),
            arms::parse(TWO_ARMS).unwrap(),
            tempfile::tempdir().unwrap(),
        );
        Mock::given(method("GET"))
            .and(path_regex("^/api/agents/[a-z-]+/vault$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"keys": ["db/key"]})))
            .mount(&server)
            .await;
        let store = open(dir.path(), &set, &arms);
        run(&hub, &set, &arms, &store, &options()).await.unwrap();

        let grades = judged(&server).await;
        assert_eq!(grades.len(), 2 * 3 * usize::from(GRADE_PASSES));
        let answers: Vec<String> = grades
            .iter()
            .map(|grade| {
                let mut sent = body(grade);
                let fields: BTreeSet<&str> = sent
                    .as_object()
                    .unwrap()
                    .keys()
                    .map(String::as_str)
                    .collect();
                assert_eq!(
                    fields,
                    BTreeSet::from([
                        "agent_id",
                        "question",
                        "expected",
                        "evidence",
                        "assumptions",
                        "trap",
                        "ideal_evidence",
                        "rubric",
                        "answer",
                    ])
                );
                assert_eq!(sent["agent_id"], "judge", "one judge for every arm");
                let answer = sent["answer"].take().as_str().unwrap().to_string();
                let rest = sent.to_string();
                for arm in ["current", "candidate", "analyst-eval"] {
                    assert!(!rest.contains(arm), "the judge was told {arm:?}");
                }
                answer
            })
            .collect();
        let by_arm = |agent: &str| answers.iter().filter(|a| a.starts_with(agent)).count();
        assert_eq!(
            (by_arm("analyst says"), by_arm("analyst-eval says")),
            (6, 6)
        );
        let run_lengths = answers.windows(2).filter(|pair| pair[0] != pair[1]).count();
        assert!(
            run_lengths > 1,
            "answers are not judged arm by arm: {answers:?}"
        );
    }

    #[tokio::test]
    async fn an_ungraded_answer_stays_ungraded_until_retry() {
        let ungraded = |_: &Request| {
            ResponseTemplate::new(200).set_body_json(json!({
                "status": "ungraded", "reason": "the judge wrote nothing", "cost_usd": 0.0,
            }))
        };
        let (server, hub) = hub_with(answered, ungraded).await;
        let (set, arms, dir) = (
            example(),
            [Arm::of_agent("analyst")],
            tempfile::tempdir().unwrap(),
        );
        let store = open(dir.path(), &set, &arms);
        run(&hub, &set, &arms, &store, &options()).await.unwrap();
        let results = store.results().unwrap();
        assert!(results
            .grades
            .iter()
            .all(|grade| matches!(grade.outcome, GradeOutcome::Ungraded { .. })));
        run(&hub, &set, &arms, &store, &options()).await.unwrap();
        assert_eq!(judged(&server).await.len(), 6);
        let retry = Options {
            retry: true,
            ..options()
        };
        run(&hub, &set, &arms, &store, &retry).await.unwrap();
        assert_eq!(judged(&server).await.len(), 12);
    }

    #[tokio::test]
    async fn a_refused_grade_stops_the_run_and_records_nothing() {
        let no_budget = |_: &Request| {
            ResponseTemplate::new(200).set_body_json(json!({
                "status": "refused", "reason": "Daily LLM budget reached", "cost_usd": 0.0,
            }))
        };
        let (_server, hub) = hub_with(answered, no_budget).await;
        let (set, arms, dir) = (
            example(),
            [Arm::of_agent("analyst")],
            tempfile::tempdir().unwrap(),
        );
        let store = open(dir.path(), &set, &arms);
        let stopped = run(&hub, &set, &arms, &store, &options())
            .await
            .unwrap_err();
        assert!(format!("{stopped:#}").contains("refused to grade"));
        assert!(store.results().unwrap().grades.is_empty());
    }

    const OVERRIDDEN: &str = r#"
        [[arm]]
        name = "current"
        agent = "analyst"

        [[arm]]
        name = "know-how-on"
        agent = "analyst-eval"
        copy_of = "analyst"
        [arm.overrides.know_how]
        enabled = true
        inject = "on"
    "#;

    const COPYS_CONFIG: &str = "[evals]\ncopy = true\n\n[llm]\nmax_rounds = 30\n";

    /// The copy's config with [`OVERRIDDEN`]'s overrides laid over it.
    fn overridden_config() -> String {
        arms::parse(OVERRIDDEN).unwrap()[1]
            .laid_over(COPYS_CONFIG)
            .unwrap()
    }

    /// [`answered`], saying the turn ran under the config the arm laid over
    /// its copy.
    fn answered_as_configured(request: &Request) -> ResponseTemplate {
        let asked = body(request);
        let config =
            (asked["agent_id"] == "analyst-eval").then(|| content_hash(&overridden_config()));
        ResponseTemplate::new(200).set_body_json(json!({
            "response": "42",
            "session_id": format!("session-{}", asked["turn_id"].as_str().unwrap()),
            "task_id": format!("chat:me:{}", asked["turn_id"].as_str().unwrap()),
            "outcome": "answered",
            "config": config,
        }))
    }

    async fn serve_config(server: &MockServer) {
        Mock::given(method("GET"))
            .and(path_regex("^/api/agents/[a-z-]+/vault$"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({"keys": []})))
            .mount(server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/agents/analyst-eval/config"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"as-it-was\"")
                    .set_body_string(COPYS_CONFIG),
            )
            .mount(server)
            .await;
        Mock::given(method("PUT"))
            .and(path("/api/agents/analyst-eval/config"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "saved": true, "restart_required": [], "findings": [], "etag": "\"overridden\"",
            })))
            .mount(server)
            .await;
    }

    #[tokio::test]
    async fn overrides_are_on_the_copy_only_while_its_arm_runs_and_then_put_back() {
        let (server, hub) = hub_with(answered_as_configured, graded).await;
        serve_config(&server).await;
        let (set, arms, dir) = (
            example(),
            arms::parse(OVERRIDDEN).unwrap(),
            tempfile::tempdir().unwrap(),
        );
        let store = open(dir.path(), &set, &arms);
        run(&hub, &set, &arms, &store, &options()).await.unwrap();

        let order: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "PUT" || r.url.path() == "/api/chat")
            .map(|r| match r.method.as_str() {
                "PUT" => "put".to_string(),
                _ => body(r)["agent_id"].as_str().unwrap().to_string(),
            })
            .collect();
        let expected = ["analyst"; 3]
            .into_iter()
            .chain(["put"])
            .chain(["analyst-eval"; 3])
            .chain(["put"]);
        assert_eq!(order, expected.collect::<Vec<_>>());

        let puts = sent(&server, "PUT", "/api/agents/analyst-eval/config").await;
        let laid: toml::Table = String::from_utf8(puts[0].body.clone())
            .unwrap()
            .parse()
            .unwrap();
        assert_eq!(laid["know_how"]["inject"].as_str(), Some("on"));
        assert_eq!(laid["llm"]["max_rounds"].as_integer(), Some(30));
        assert_eq!(laid["evals"]["copy"].as_bool(), Some(true));
        assert_eq!(puts[0].headers["if-match"], "\"as-it-was\"");
        assert_eq!(puts[1].body, COPYS_CONFIG.as_bytes());
        assert_eq!(puts[1].headers["if-match"], "\"overridden\"");
        assert!(!dir
            .path()
            .join(BACKUP_DIR)
            .join("analyst-eval.json")
            .exists());
        assert!(sent(&server, "PUT", "/api/agents/analyst/config")
            .await
            .is_empty());
        let results = store.results().unwrap();
        assert!(results
            .turns
            .iter()
            .all(|turn| turn.status == TurnStatus::Answered));
        assert_eq!(results.grades.len(), 2 * 3 * usize::from(GRADE_PASSES));
    }

    #[tokio::test]
    async fn an_answer_from_a_replica_without_the_overrides_is_not_the_arms() {
        let (server, hub) = hub_with(answered, graded).await;
        serve_config(&server).await;
        let (set, arms, dir) = (
            example(),
            arms::parse(OVERRIDDEN).unwrap(),
            tempfile::tempdir().unwrap(),
        );
        let store = open(dir.path(), &set, &arms);
        run(&hub, &set, &arms, &store, &options()).await.unwrap();
        let results = store.results().unwrap();
        let (baseline, candidate): (Vec<_>, Vec<_>) =
            results.turns.iter().partition(|turn| turn.arm == "current");
        assert!(baseline.iter().all(|t| t.status == TurnStatus::Answered));
        assert_eq!(candidate.len(), 3);
        for turn in candidate {
            assert_eq!(turn.status, TurnStatus::Failed);
            assert!(turn.answer.is_empty());
            assert!(turn.error.as_deref().unwrap().contains("config"));
            assert!(turn.metrics.is_some(), "what it cost is still on record");
        }
        assert_eq!(judged(&server).await.len(), 3 * usize::from(GRADE_PASSES));
    }

    #[tokio::test]
    async fn an_agent_that_does_not_call_itself_a_test_copy_is_never_overridden() {
        let (server, hub) = hub_with(answered, graded).await;
        Mock::given(method("GET"))
            .and(path("/api/agents/analyst-eval/config"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"as-it-was\"")
                    .set_body_string("[llm]\nmax_rounds = 30\n"),
            )
            .mount(&server)
            .await;
        serve_config(&server).await;
        let (set, arms, dir) = (
            example(),
            arms::parse(OVERRIDDEN).unwrap(),
            tempfile::tempdir().unwrap(),
        );
        let store = open(dir.path(), &set, &arms);
        let refused = run(&hub, &set, &arms, &store, &options())
            .await
            .unwrap_err();
        assert!(format!("{refused:#}").contains("test copy"), "{refused:#}");
        let requests = server.received_requests().await.unwrap();
        assert!(requests.iter().all(|r| r.method.as_str() != "PUT"));
        assert!(sent(&server, "POST", "/api/chat").await.is_empty());
    }

    #[tokio::test]
    async fn a_stopped_arm_still_gets_its_config_back() {
        let no_budget = |request: &Request| match body(request)["agent_id"] == "analyst-eval" {
            true => refused(request),
            false => answered(request),
        };
        let (server, hub) = hub_with(no_budget, graded).await;
        serve_config(&server).await;
        let (set, arms, dir) = (
            example(),
            arms::parse(OVERRIDDEN).unwrap(),
            tempfile::tempdir().unwrap(),
        );
        let store = open(dir.path(), &set, &arms);
        assert!(run(&hub, &set, &arms, &store, &options()).await.is_err());
        let puts = sent(&server, "PUT", "/api/agents/analyst-eval/config").await;
        assert_eq!(puts.len(), 2);
        assert_eq!(puts[1].body, COPYS_CONFIG.as_bytes());
    }

    /// A hub whose copy's config reads `now`, and a run directory holding
    /// what a run that died while the copy was overridden left behind.
    async fn after_a_dead_run(now: &str) -> (MockServer, Hub, tempfile::TempDir, PathBuf) {
        let (server, hub) = hub_with(answered, graded).await;
        Mock::given(method("GET"))
            .and(path("/api/agents/analyst-eval/config"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("etag", "\"as-it-is\"")
                    .set_body_string(now),
            )
            .mount(&server)
            .await;
        serve_config(&server).await;
        let dir = tempfile::tempdir().unwrap();
        let backups = dir.path().join(BACKUP_DIR);
        std::fs::create_dir_all(&backups).unwrap();
        let kept_at = backups.join("analyst-eval.json");
        let backup = Backup {
            agent: "analyst-eval".into(),
            original: COPYS_CONFIG.into(),
            overridden: overridden_config(),
        };
        std::fs::write(&kept_at, serde_json::to_string(&backup).unwrap()).unwrap();
        (server, hub, dir, kept_at)
    }

    #[tokio::test]
    async fn a_config_left_overridden_by_a_dead_run_is_put_back_first() {
        let (server, hub, dir, kept_at) = after_a_dead_run(&overridden_config()).await;
        let (set, arms) = (example(), [Arm::of_agent("analyst")]);
        let store = open(dir.path(), &set, &arms);
        run(&hub, &set, &arms, &store, &options()).await.unwrap();
        let requests = server.received_requests().await.unwrap();
        assert_eq!(requests[1].method.as_str(), "PUT");
        assert_eq!(requests[1].body, COPYS_CONFIG.as_bytes());
        assert_eq!(requests[1].headers["if-match"], "\"as-it-is\"");
        assert!(!kept_at.exists());
        assert_eq!(
            sent(&server, "PUT", "/api/agents/analyst-eval/config")
                .await
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn a_config_changed_since_a_dead_run_is_left_for_its_owner() {
        let edited = format!("{}\n[research]\nadversarial = true\n", overridden_config());
        let (server, hub, dir, kept_at) = after_a_dead_run(&edited).await;
        let refused = restore(&hub, dir.path()).await.unwrap_err();
        assert!(
            format!("{refused:#}").contains("changed since"),
            "{refused:#}"
        );
        assert!(kept_at.exists(), "the original is still on hand");
        assert!(sent(&server, "PUT", "/api/agents/analyst-eval/config")
            .await
            .is_empty());

        let (server, hub, dir, kept_at) = after_a_dead_run(COPYS_CONFIG).await;
        restore(&hub, dir.path()).await.unwrap();
        assert!(
            !kept_at.exists(),
            "it was never overridden, or is already back"
        );
        assert!(sent(&server, "PUT", "/api/agents/analyst-eval/config")
            .await
            .is_empty());
    }

    #[tokio::test]
    async fn an_override_that_needs_a_restart_is_undone_and_refused() {
        let (server, hub) = hub_with(answered, graded).await;
        Mock::given(method("PUT"))
            .and(path("/api/agents/analyst-eval/config"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "saved": true, "restart_required": ["memory.backend"], "etag": "\"overridden\"",
            })))
            .mount(&server)
            .await;
        serve_config(&server).await;
        let (set, arms, dir) = (
            example(),
            arms::parse(OVERRIDDEN).unwrap(),
            tempfile::tempdir().unwrap(),
        );
        let store = open(dir.path(), &set, &arms);
        let refused = run(&hub, &set, &arms, &store, &options())
            .await
            .unwrap_err();
        assert!(format!("{refused:#}").contains("memory.backend"));
        let puts = sent(&server, "PUT", "/api/agents/analyst-eval/config").await;
        assert_eq!(puts.last().unwrap().body, COPYS_CONFIG.as_bytes());
        let chats = sent(&server, "POST", "/api/chat").await;
        assert!(chats.iter().all(|chat| body(chat)["agent_id"] == "analyst"));
    }

    /// A hub on which [`OVERRIDDEN`]'s copy, under the arm's config, sees
    /// know-how as `seen` says.
    async fn copy_seeing(seen: Value) -> (MockServer, Hub) {
        let (server, hub) = hub_with(answered_as_configured, graded).await;
        serve_config(&server).await;
        let preview = json!({
            "enabled": true, "shown": true, "matching": true, "source_agent": null,
            "min_score": 0.75, "visible": 0, "matched": [],
        });
        let preview =
            seen.as_object()
                .unwrap()
                .iter()
                .fold(preview, |mut preview, (key, value)| {
                    preview[key] = value.clone();
                    preview
                });
        Mock::given(method("POST"))
            .and(path("/api/evals/know-how"))
            .and(|request: &Request| body(request)["agent_id"] == "analyst-eval")
            .respond_with(ResponseTemplate::new(200).set_body_json(preview))
            .mount(&server)
            .await;
        (server, hub)
    }

    async fn refusal_of(seen: Value) -> Option<String> {
        let (server, hub) = copy_seeing(seen).await;
        let (set, arms, dir) = (
            example(),
            arms::parse(OVERRIDDEN).unwrap(),
            tempfile::tempdir().unwrap(),
        );
        let store = open(dir.path(), &set, &arms);
        let limited = Options {
            limit: Some(1),
            ..options()
        };
        let ran = run(&hub, &set, &arms, &store, &limited).await;
        let counted = sent(&server, "POST", "/api/evals/know-how").await;
        let copys = counted
            .iter()
            .find(|request| body(request)["agent_id"] == "analyst-eval")
            .expect("the copy's know-how was counted");
        assert_eq!(
            body(copys)["config"].as_str(),
            Some(overridden_config().as_str()),
            "counted under the arm's config"
        );
        let refused = ran.err().map(|e| format!("{e:#}"));
        if refused.is_some() {
            assert!(sent(&server, "POST", "/api/chat").await.is_empty());
            assert!(sent(&server, "PUT", "/api/agents/analyst-eval/config")
                .await
                .is_empty());
        }
        refused
    }

    #[tokio::test]
    async fn a_copy_reading_another_agents_know_how_than_its_originals_is_refused() {
        let refused = refusal_of(json!({"source_agent": "other", "visible": 4})).await;
        let said = refused.expect("refused");
        assert!(
            said.contains("\"other\"") && said.contains("copy of"),
            "{said}"
        );
        assert_eq!(
            refusal_of(json!({"source_agent": "analyst", "visible": 4})).await,
            None
        );
    }

    #[tokio::test]
    async fn an_arm_showing_know_how_its_copy_sees_none_of_is_refused() {
        let unset = refusal_of(json!({})).await.expect("refused");
        assert!(
            unset.contains("this copy sees no know-how; set `[know_how] source_agent`"),
            "{unset}"
        );
        let hidden = refusal_of(json!({"source_agent": "analyst"}))
            .await
            .expect("refused");
        assert!(
            hidden.contains("none of the source's entries are visible to you"),
            "{hidden}"
        );
        let unmatched =
            refusal_of(json!({"source_agent": "analyst", "visible": 4, "matching": false}))
                .await
                .expect("refused");
        assert!(unmatched.contains("no embedder"), "{unmatched}");
        assert_eq!(
            refusal_of(json!({"shown": false})).await,
            None,
            "an arm that shows no know-how is not refused for it"
        );
    }

    #[tokio::test]
    async fn nothing_is_spent_on_a_run_that_cannot_be_fair() {
        // The copy lacks a credential its original has.
        let (server, hub) = hub_with(answered, graded).await;
        let keys = |keys: Value| ResponseTemplate::new(200).set_body_json(json!({ "keys": keys }));
        Mock::given(method("GET"))
            .and(path("/api/agents/analyst/vault"))
            .respond_with(keys(json!([
                "db/key",
                "users/me/db/key",
                "users/bob/db/key"
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/api/agents/analyst-eval/vault"))
            .respond_with(keys(json!(["users/me/db/key"])))
            .mount(&server)
            .await;
        let (set, arms, dir) = (
            example(),
            arms::parse(TWO_ARMS).unwrap(),
            tempfile::tempdir().unwrap(),
        );
        let store = open(dir.path(), &set, &arms);
        let refused = run(&hub, &set, &arms, &store, &options())
            .await
            .unwrap_err();
        let said = format!("{refused:#}");
        assert!(said.contains("db/key") && !said.contains("bob"), "{said}");
        assert!(sent(&server, "POST", "/api/chat").await.is_empty());
        let allowed = Options {
            allow_missing_credentials: true,
            limit: Some(1),
            ..options()
        };
        run(&hub, &set, &arms, &store, &allowed).await.unwrap();
        assert_eq!(sent(&server, "POST", "/api/chat").await.len(), 2);

        // Overrides aimed at the install's default agent.
        let on_default = arms::parse(
            &OVERRIDDEN
                .replace(
                    "agent = \"analyst-eval\"\n        copy_of = \"analyst\"",
                    "agent = \"analyst\"\n        copy_of = \"judge\"",
                )
                .replace(
                    "name = \"current\"\n        agent = \"analyst\"",
                    "name = \"current\"\n        agent = \"judge\"",
                ),
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path(), &set, &on_default);
        let refused = run(&hub, &set, &on_default, &store, &options())
            .await
            .unwrap_err();
        assert!(format!("{refused:#}").contains("default agent"));

        // An agent the person cannot see.
        let unseen = [Arm::of_agent("ghost")];
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path(), &set, &unseen);
        assert!(run(&hub, &set, &unseen, &store, &options()).await.is_err());
    }

    #[tokio::test]
    async fn the_owner_token_is_told_a_person_is_needed() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/api/me"))
            .respond_with(ResponseTemplate::new(401))
            .mount(&server)
            .await;
        let hub = Hub::new(&server.uri(), "owner-token").unwrap();
        let (set, arms, dir) = (
            example(),
            [Arm::of_agent("analyst")],
            tempfile::tempdir().unwrap(),
        );
        let store = open(dir.path(), &set, &arms);
        let refused = run(&hub, &set, &arms, &store, &options())
            .await
            .unwrap_err();
        assert!(format!("{refused:#}").contains("not a person's sign-in"));
        assert_eq!(server.received_requests().await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_refusal_stops_the_run_once_what_is_in_flight_has_landed() {
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&asked);
        let first_refused = move |request: &Request| match counter.fetch_add(1, Ordering::SeqCst) {
            0 => refused(request),
            _ => answered(request).set_delay(Duration::from_millis(300)),
        };
        let (_server, hub) = hub_with(first_refused, graded).await;
        let (set, arms, dir) = (
            example(),
            [Arm::of_agent("analyst")],
            tempfile::tempdir().unwrap(),
        );
        let store = open(dir.path(), &set, &arms);
        let two_at_once = Options {
            concurrency: 2,
            ..options()
        };
        let stopped = run(&hub, &set, &arms, &store, &two_at_once)
            .await
            .unwrap_err();
        assert!(format!("{stopped:#}").contains("refused a turn"));
        assert_eq!(
            asked.load(Ordering::SeqCst),
            2,
            "nothing new starts after a refusal"
        );
        let results = store.results().unwrap();
        assert_eq!(
            results.turns.len(),
            1,
            "the turn in flight was not thrown away"
        );
        assert_eq!(results.turns[0].status, TurnStatus::Answered);
    }

    #[tokio::test]
    async fn an_answer_from_a_replica_that_predates_evals_is_a_failed_turn() {
        let unnamed = |request: &Request| {
            ResponseTemplate::new(200).set_body_json(json!({
                "response": "Daily LLM budget reached",
                "session_id": format!("session-{}", body(request)["turn_id"].as_str().unwrap()),
            }))
        };
        let (server, hub) = hub_with(unnamed, graded).await;
        let (set, arms, dir) = (
            example(),
            [Arm::of_agent("analyst")],
            tempfile::tempdir().unwrap(),
        );
        let store = open(dir.path(), &set, &arms);
        run(&hub, &set, &arms, &store, &options()).await.unwrap();
        let results = store.results().unwrap();
        assert_eq!(results.turns.len(), 3);
        for turn in &results.turns {
            assert_eq!(turn.status, TurnStatus::Failed);
            assert!(turn.error.as_deref().unwrap().contains("predates"));
        }
        assert!(judged(&server).await.is_empty());
    }

    #[tokio::test]
    async fn a_question_the_scope_filter_turns_away_fails_alone_and_access_denied_stops_the_run() {
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&asked);
        let first_off_topic = move |request: &Request| match counter.fetch_add(1, Ordering::SeqCst)
        {
            0 => ResponseTemplate::new(422)
                .set_body_json(json!({"error": "I only answer questions about orders."})),
            _ => answered(request),
        };
        let (_server, hub) = hub_with(first_off_topic, graded).await;
        let (set, arms, dir) = (
            example(),
            [Arm::of_agent("analyst")],
            tempfile::tempdir().unwrap(),
        );
        let store = open(dir.path(), &set, &arms);
        run(&hub, &set, &arms, &store, &options()).await.unwrap();
        let results = store.results().unwrap();
        assert_eq!(results.turns.len(), 3);
        let failed: Vec<_> = results
            .turns
            .iter()
            .filter(|turn| turn.status == TurnStatus::Failed)
            .collect();
        assert_eq!(failed.len(), 1);
        assert!(failed[0].error.as_deref().unwrap().contains("scope filter"));

        let denied = |_: &Request| {
            ResponseTemplate::new(403)
                .set_body_json(json!({"error": "Access denied by this agent's policy."}))
        };
        let (server, hub) = hub_with(denied, graded).await;
        let dir = tempfile::tempdir().unwrap();
        let store = open(dir.path(), &set, &arms);
        let stopped = run(&hub, &set, &arms, &store, &options())
            .await
            .unwrap_err();
        assert!(
            format!("{stopped:#}").contains("Access denied"),
            "{stopped:#}"
        );
        assert!(store.results().unwrap().turns.is_empty());
        assert_eq!(sent(&server, "POST", "/api/chat").await.len(), 1);
    }

    #[tokio::test]
    async fn a_turn_stopped_at_its_round_cap_is_recorded_as_capped() {
        let capped = |request: &Request| {
            let asked = body(request);
            ResponseTemplate::new(200).set_body_json(json!({
                "response": "I ran out of rounds; so far: 40.",
                "session_id": format!("session-{}", asked["turn_id"].as_str().unwrap()),
                "task_id": format!("chat:me:{}", asked["turn_id"].as_str().unwrap()),
                "outcome": "answered",
                "capped": true,
            }))
        };
        let (server, hub) = hub_with(capped, graded).await;
        let (set, arms, dir) = (
            example(),
            [Arm::of_agent("analyst")],
            tempfile::tempdir().unwrap(),
        );
        let store = open(dir.path(), &set, &arms);
        run(&hub, &set, &arms, &store, &options()).await.unwrap();
        let results = store.results().unwrap();
        assert!(results
            .turns
            .iter()
            .all(|turn| turn.capped && turn.status == TurnStatus::Answered));
        assert_eq!(judged(&server).await.len(), 3 * usize::from(GRADE_PASSES));
    }

    #[tokio::test]
    async fn the_session_a_failed_turn_left_behind_is_looked_for_and_deleted() {
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&asked);
        let first_fails = move |request: &Request| match counter.fetch_add(1, Ordering::SeqCst) {
            0 => ResponseTemplate::new(500),
            _ => answered(request),
        };
        let (server, hub) = hub_with(first_fails, graded).await;
        let leftover = |request: &Request| {
            let channel = request
                .url
                .query_pairs()
                .find(|(key, _)| key == "channel")
                .map(|(_, channel)| channel.to_string())
                .unwrap();
            let turn = channel.trim_start_matches(EVAL_CHANNEL_PREFIX);
            ResponseTemplate::new(200).set_body_json(json!({"sessions": [
                {"id": format!("session-{turn}"), "channel": channel},
                {"id": "someone-elses", "channel": "web"},
            ]}))
        };
        Mock::given(method("GET"))
            .and(path("/api/sessions"))
            .respond_with(leftover)
            .with_priority(1)
            .mount(&server)
            .await;
        let (set, arms, dir) = (
            example(),
            [Arm::of_agent("analyst")],
            tempfile::tempdir().unwrap(),
        );
        let store = open(dir.path(), &set, &arms);
        run(&hub, &set, &arms, &store, &options()).await.unwrap();

        let results = store.results().unwrap();
        let failed = results
            .turns
            .iter()
            .find(|turn| turn.status == TurnStatus::Failed)
            .expect("the first turn failed");
        let deleted: Vec<String> = server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "DELETE")
            .map(|r| r.url.path().to_string())
            .collect();
        assert_eq!(deleted.len(), 3, "one session per turn: {deleted:?}");
        assert!(deleted.contains(&format!("/api/sessions/session-{}", failed.turn_id)));
    }

    #[tokio::test]
    async fn a_rate_limited_call_is_sent_again() {
        let asked = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&asked);
        let limited_once = move |request: &Request| match counter.fetch_add(1, Ordering::SeqCst) {
            0 => ResponseTemplate::new(429).insert_header("retry-after", "0"),
            _ => answered(request),
        };
        let (_server, hub) = hub_with(limited_once, graded).await;
        let (set, arms, dir) = (
            example(),
            [Arm::of_agent("analyst")],
            tempfile::tempdir().unwrap(),
        );
        let store = open(dir.path(), &set, &arms);
        run(&hub, &set, &arms, &store, &options()).await.unwrap();
        let results = store.results().unwrap();
        assert_eq!(asked.load(Ordering::SeqCst), 4);
        assert!(results
            .turns
            .iter()
            .all(|t| t.status == TurnStatus::Answered));
    }
}
