//! Periodic memory consolidation and session scanning.
//!
//! Two complementary mechanisms:
//!
//! 1. **Session scanning** — scans recent session transcripts for memorable
//!    facts and stores them via the existing passive extraction pipeline.
//!    Tracks progress with a cursor file to avoid re-scanning.
//!
//! 2. **Memory consolidation** — a scheduled `AgentTurn` job that instructs
//!    the agent to merge related episodic memories into semantic memories
//!    and deduplicate.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::Result;
use tracing::{debug, info, warn};

use serde::{Deserialize, Serialize};

use workforce_agent::runner::ToolWhitelist;
use workforce_common::session::{estimate_tokens, MessageRole, TurnPrincipal};
use workforce_common::subscription::{Payer, AUTOMATED_SOURCES, CONSOLIDATION_CHANNEL_PREFIX};

use crate::scheduler::{isolated_run_channel, JobPayload, Schedule};
use crate::serve::{Route, Skip, TurnOutcome};

const DEFAULT_CONSOLIDATION_INTERVAL_SECS: u64 = 14400;

/// Hard cap on tool-call rounds for the memory-consolidation job. The agent's
/// general `max_rounds` (e.g. 75 for Penny) is far too generous for what is
/// supposed to be a focused background task, and freedom to loop has caused
/// it to busy-wait on background processes.
const CONSOLIDATION_MAX_ROUNDS: u32 = 20;

/// The only tools a consolidation turn is offered: exactly what
/// [`CONSOLIDATION_PROMPT`] allows it to call.
const CONSOLIDATION_TOOLS: [&str; 2] = ["memory", "notes"];

/// Below this many episodic memories a consolidation run has nothing to merge
/// — the rule [`CONSOLIDATION_PROMPT`] also gives the model.
pub(crate) const MIN_MEMORIES: usize = 5;

/// The most units one consolidation pass runs. The scheduler awaits the whole
/// pass, so an unbounded one would hold up the agent's reminders and jobs.
pub(crate) const MAX_UNITS_PER_PASS: usize = 4;

/// Where each unit's consolidation cursor is kept, under `memory/`.
const UNIT_CURSORS_FILENAME: &str = "consolidation-units.json";

const CONSOLIDATION_PROMPT: &str = "\
[BACKGROUND TASK — STRICT BUDGET]
This is a focused background memory-consolidation task. Hard rules:
- Use ONLY the `memory` tool and `notes(action=\"write_profile\")`. Do NOT call `exec.*`, \
  `knowledge.*`, `code.*`, `github.*`, `filesystem.*`, or any other tool.
- Do NOT poll, sleep, or wait for background processes (e.g. indexing). \
  Ignore them.
- Aim to finish in ≤20 tool calls. If you can't make progress, stop and \
  report what you found.

Steps:

1. Run `memory(action=\"count\")` to see how many memories you have per layer.
   If you have fewer than 5 episodic memories, skip consolidation and report \
   that there are not enough memories to consolidate yet.

2. Run `memory(action=\"recall\")` with query 'recent activity and decisions' \
   (layer: episodic, limit: 50) to scan your recent episodic memories.

3. Look for clusters of related memories that can be merged. For example, \
   multiple episodic memories about the same topic, person, or decision \
   should become a single consolidated semantic memory.

4. For each cluster:
   a. Use `memory(action=\"remember\")` to create a consolidated semantic memory \
      (layer: semantic, importance: 0.6-0.9 based on significance). \
      Tag it with 'source:consolidation'.
   b. Use `memory(action=\"forget\")` to soft-delete the individual episodic fragments \
      that were merged into the new semantic memory.

5. Check for duplicate or near-duplicate memories and forget the redundant ones.

6. Briefly report: how many memories were consolidated, how many forgotten, \
   and the current memory counts by layer.

7. Distil a rolling 'about this user' narrative — a short paragraph \
   (≤1500 characters) capturing stable preferences, recurring emotional \
   patterns, habits, and context observable across the user's memories. \
   Write it from your own perspective as the agent. Then call \
   `notes(action=\"write_profile\")` with that narrative as `content` — it replaces \
   (not appends to) the user's profile and appears as 'Learned Preferences' \
   in every future prompt. Skip this step entirely if fewer than 5 \
   memories exist for the current user.";

const CURSOR_FILENAME: &str = "consolidation-cursor";

/// Sentinel identity used by the system-wide consolidation job (DW-521).
/// Memories the global job writes carry this `identity_id`; per-user recall
/// (which filters on `identity_id = <real-user-uuid>`) does not match it,
/// so the prior "consolidated cross-user output leaks into every prompt"
/// failure mode is closed.
pub const CONSOLIDATION_SYSTEM_IDENTITY: &str = "system:consolidation";

/// Register the memory consolidation job with the scheduler.
///
/// One job per agent: each run consolidates people's memories, each as that
/// person, and the memories no person owns (see [`execute`]).
///
/// The payload is a typed [`JobPayload::Consolidation`] — the engine
/// (`[memory] consolidation_source`), model, and prompt resolve when the job
/// fires, and the by-name upsert migrates legacy `AgentTurn` rows on boot.
pub async fn register_consolidation_job(
    scheduler: &dyn crate::hosts::SchedulerHost,
    interval_secs: Option<u64>,
) -> Result<String> {
    let interval = interval_secs.unwrap_or(DEFAULT_CONSOLIDATION_INTERVAL_SECS);

    let schedule = Schedule::Every {
        every_secs: interval,
        anchor: None,
    };

    let job_id = scheduler
        .upsert_job(
            "memory-consolidation",
            Some("Periodic memory consolidation, deduplication, and promotion"),
            schedule,
            JobPayload::Consolidation { identity_id: None },
        )
        .await
        .map_err(|e| eyre::eyre!("failed to register consolidation job: {e}"))?;

    scheduler
        .update_job_identity(&job_id, Some(CONSOLIDATION_SYSTEM_IDENTITY.to_string()))
        .await;

    tracing::info!(
        job_id = %job_id,
        interval_secs = interval,
        identity = CONSOLIDATION_SYSTEM_IDENTITY,
        "Registered memory consolidation job"
    );

    Ok(job_id)
}

/// One unit of a consolidation pass: one person's memories, consolidated as
/// that person on their own login, or the memories no person owns, written
/// under one background identity and consolidated as it, paid for by whoever
/// the agent's own work falls to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unit {
    Person(uuid::Uuid),
    Unowned(&'static str),
}

impl Unit {
    /// The identities memory no person owns is written under: the automated
    /// sources a turn runs as when no person is behind it (memory tools and
    /// passive extraction refuse to write with no identity at all), and the
    /// consolidation sentinel's own output.
    fn unowned() -> impl Iterator<Item = Self> {
        AUTOMATED_SOURCES
            .into_iter()
            .filter(|id| matches!(Payer::classify(Some(id)), Payer::Background(_)))
            .chain(std::iter::once(CONSOLIDATION_SYSTEM_IDENTITY))
            .map(Self::Unowned)
    }

    fn of(identity: &str) -> Self {
        Payer::classify(Some(identity))
            .person()
            .map(Self::Person)
            .or_else(|| Self::unowned().find(|unit| unit.identity() == identity))
            .unwrap_or(Self::Unowned(CONSOLIDATION_SYSTEM_IDENTITY))
    }

    /// The identity the unit runs as, and whose memories it consolidates.
    fn identity(self) -> String {
        match self {
            Self::Person(person) => person.to_string(),
            Self::Unowned(identity) => identity.to_string(),
        }
    }

    fn principal(self, service: &crate::serve::AgentService) -> TurnPrincipal {
        TurnPrincipal {
            run_as: Some(self.identity()),
            bill_to: match self {
                Self::Person(_) => None,
                Self::Unowned(_) => service.agent_work_payer().map(|payer| payer.to_string()),
            },
        }
    }
}

/// Forget where consolidation stands for each of `units` (people who are
/// merged away or deleted). Returns how many cursors went.
pub(crate) fn forget_units(workspace: &Path, units: &[String]) -> usize {
    let mut cursors = UnitCursors::load(workspace);
    let before = cursors.0.len();
    cursors.0.retain(|unit, _| !units.contains(unit));
    let forgotten = before - cursors.0.len();
    if forgotten > 0 {
        cursors.save(workspace);
    }
    forgotten
}

/// How many of `units` consolidation keeps a cursor for.
pub(crate) fn unit_cursors(workspace: &Path, units: &[String]) -> usize {
    UnitCursors::load(workspace)
        .0
        .keys()
        .filter(|unit| units.contains(unit))
        .count()
}

/// Every unit consolidation keeps a cursor for.
pub(crate) fn cursor_units(workspace: &Path) -> Vec<String> {
    UnitCursors::load(workspace).0.into_keys().collect()
}

/// Where each unit's consolidation stands, kept per agent beside the
/// session-scan cursor.
#[derive(Debug, Default, Serialize, Deserialize)]
struct UnitCursors(BTreeMap<String, UnitCursor>);

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
struct UnitCursor {
    /// Every episodic memory created before this was in scope of the unit's
    /// last successful run.
    consolidated: Option<DateTime<Utc>>,
    /// When the unit last ran, whether or not it succeeded.
    attempted: DateTime<Utc>,
}

impl UnitCursors {
    fn path(workspace: &Path) -> std::path::PathBuf {
        workspace.join("memory").join(UNIT_CURSORS_FILENAME)
    }

    fn load(workspace: &Path) -> Self {
        std::fs::read_to_string(Self::path(workspace))
            .ok()
            .and_then(|json| {
                serde_json::from_str(&json)
                    .inspect_err(
                        |e| warn!(error = %e, "Unreadable consolidation cursors; starting afresh"),
                    )
                    .ok()
            })
            .unwrap_or_default()
    }

    fn save(&self, workspace: &Path) {
        let path = Self::path(workspace);
        let written = path
            .parent()
            .map_or(Ok(()), std::fs::create_dir_all)
            .and_then(|()| std::fs::write(&path, serde_json::to_vec(self).unwrap_or_default()));
        if let Err(e) = written {
            warn!(error = %e, path = %path.display(), "Failed to save consolidation cursors");
        }
    }

    fn get(&self, unit: Unit) -> Option<UnitCursor> {
        self.0.get(&unit.identity()).copied()
    }

    /// Note that `unit` ran from `started`, advancing its cursor to then
    /// only when the run succeeded.
    fn ran(&mut self, unit: Unit, started: DateTime<Utc>, succeeded: bool) {
        let consolidated = succeeded
            .then_some(started)
            .or_else(|| self.get(unit).and_then(|cursor| cursor.consolidated));
        self.0.insert(
            unit.identity(),
            UnitCursor {
                consolidated,
                attempted: started,
            },
        );
    }
}

/// Run one consolidation pass: `identity`'s memories alone, or (`None`)
/// every person's and then the memories no person owns, each as its own
/// [`Unit`] in its own session. A unit runs only when at least
/// [`MIN_MEMORIES`] episodic memories were created since its last successful
/// run, and at most [`MAX_UNITS_PER_PASS`] run in one pass, least recently
/// run first; the rest wait for the next pass. A unit whose payer has no
/// login, or whose budget refuses it, is skipped. The pass fails only when
/// every unit it ran failed, so one person's broken login never stops
/// everyone's consolidation. The engine is resolved from the live
/// `[memory]` config, so flipping `consolidation_source` takes effect on the
/// next fire. `job_id` keys each unit's cost records (`scheduler:{job_id}:…`)
/// for the per-job history JOIN; `cancel` is the scheduler's shutdown token.
pub async fn execute(
    service: &Arc<crate::serve::AgentService>,
    identity: Option<&str>,
    job_id: &str,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<String> {
    let mem = service.effective_memory();
    let mut cursors = UnitCursors::load(&service.workspace);
    let units = match identity {
        Some(identity) => vec![Unit::of(identity)],
        None => pass_units(service),
    };
    let mut due = Vec::new();
    for unit in units {
        let since = cursors.get(unit).and_then(|cursor| cursor.consolidated);
        if has_new_memories(service, unit, since).await {
            due.push(unit);
        }
    }
    due.sort_by_key(|unit| {
        (
            cursors.get(*unit).map(|cursor| cursor.attempted),
            unit.identity(),
        )
    });

    let mut ran = Vec::new();
    let mut skipped = Vec::new();
    let mut failed = Vec::new();
    let mut due = due.into_iter();
    for unit in due.by_ref() {
        let started = Utc::now();
        let outcome = service
            .run_background_unit(
                &unit.principal(service),
                route(&mem),
                Box::pin(consolidate(service, unit, job_id, &mem, cancel)),
            )
            .await;
        if cancel.is_some_and(tokio_util::sync::CancellationToken::is_cancelled) {
            break;
        }
        match outcome {
            Ok(TurnOutcome::Answered(summary)) => {
                cursors.ran(unit, started, true);
                ran.push(format!("{}: {summary}", unit.identity()));
            }
            Ok(TurnOutcome::Skipped(skip)) => {
                info!(identity = %unit.identity(), reason = %skip, "Memory consolidation skipped");
                skipped.push(format!("{}: {skip}", unit.identity()));
            }
            Ok(TurnOutcome::Stopped(stop)) => {
                cursors.ran(unit, started, false);
                failed.push(format!("{}: {}", unit.identity(), stop.reason()));
            }
            Err(e) => {
                cursors.ran(unit, started, false);
                failed.push(format!("{}: {e}", unit.identity()));
            }
        }
        cursors.save(&service.workspace);
        if ran.len() + failed.len() == MAX_UNITS_PER_PASS {
            break;
        }
    }
    let deferred = due.count();

    if ran.is_empty() && !failed.is_empty() {
        return Err(eyre::eyre!(
            "memory consolidation failed for {}",
            failed.join("; ")
        ));
    }
    if !failed.is_empty() {
        warn!(failed = ?failed, "Memory consolidation failed for some people");
    }
    let outcomes = [
        (deferred > 0).then(|| format!("{deferred} deferred to the next pass")),
        (!skipped.is_empty()).then(|| format!("{} skipped: {}", skipped.len(), skipped.join("; "))),
        (!failed.is_empty()).then(|| format!("failed: {}", failed.join("; "))),
        Some(if ran.is_empty() {
            "nothing to consolidate".to_string()
        } else {
            ran.join("\n")
        }),
    ];
    Ok(outcomes
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join("\n"))
}

/// What a unit runs on: an agent turn on the consolidation model, or the
/// Claude Code engine on the unit's own Claude login, falling over to the
/// model when `consolidation_cc_failover` lets it.
fn route(mem: &workforce_common::settings::MemoryConfig) -> Route<'_> {
    let claude = workforce_common::subscription::SubscriptionKind::Claude;
    let model = mem.consolidation_model.as_deref();
    match (mem.consolidation_source, mem.consolidation_cc_failover) {
        (workforce_common::settings::CurationSource::Model, _) => Route::Model(model),
        (workforce_common::settings::CurationSource::ClaudeCode, false) => Route::Login(claude),
        (workforce_common::settings::CurationSource::ClaudeCode, true) => {
            Route::LoginOrModel(claude, model)
        }
    }
}

/// Every person the install knows, then the memories no person owns.
fn pass_units(service: &crate::serve::AgentService) -> Vec<Unit> {
    let people = service
        .identity_store
        .read()
        .clone()
        .map(|store| {
            let all = workforce_identity::store::IdentityFilter::default();
            store
                .count_filtered(&all)
                .and_then(|total| {
                    store.list(&workforce_identity::store::IdentityFilter {
                        limit: Some(total),
                        ..all
                    })
                })
                .unwrap_or_else(|e| {
                    warn!(error = %e, "Failed to list identities for memory consolidation");
                    Vec::new()
                })
        })
        .unwrap_or_default();
    people
        .into_iter()
        .filter(crate::serve::is_member)
        .map(|identity| Unit::Person(identity.id))
        .chain(Unit::unowned())
        .collect()
}

/// Whether `unit` has at least [`MIN_MEMORIES`] episodic memories on this
/// agent created since `since` (ever, when `None`).
async fn has_new_memories(
    service: &crate::serve::AgentService,
    unit: Unit,
    since: Option<DateTime<Utc>>,
) -> bool {
    let Some(store) = service.memory_store.as_ref() else {
        return false;
    };
    store
        .count(workforce_memory::types::MemoryFilter {
            layer: Some(workforce_memory::types::MemoryLayer::Episodic),
            owner: Some(service.agent_id.clone()),
            identity_id: Some(unit.identity()),
            created_after: since,
            ..Default::default()
        })
        .await
        .inspect_err(|e| {
            warn!(
                error = %e,
                identity = %unit.identity(),
                "Failed to count memories for consolidation"
            )
        })
        .is_ok_and(|count| count >= MIN_MEMORIES)
}

/// Consolidate `unit` on the engine `[memory] consolidation_source` names.
async fn consolidate(
    service: &Arc<crate::serve::AgentService>,
    unit: Unit,
    job_id: &str,
    mem: &workforce_common::settings::MemoryConfig,
    cancel: Option<&tokio_util::sync::CancellationToken>,
) -> Result<TurnOutcome> {
    match mem.consolidation_source {
        workforce_common::settings::CurationSource::Model => {
            execute_model_turn(service, unit, job_id, mem).await
        }
        workforce_common::settings::CurationSource::ClaudeCode => {
            let profile = matches!(unit, Unit::Person(_));
            match crate::memory_cc::run(service, &unit.identity(), profile, cancel).await {
                crate::memory_cc::CcOutcome::Applied(summary) => Ok(TurnOutcome::Answered(summary)),
                crate::memory_cc::CcOutcome::Cancelled => Ok(TurnOutcome::Answered(
                    "consolidation cancelled (shutdown)".to_string(),
                )),
                crate::memory_cc::CcOutcome::Unavailable(reason) => {
                    if !mem.consolidation_cc_failover {
                        return Err(eyre::eyre!(
                            "Claude Code consolidation unavailable ({reason}); run skipped. \
                             Set `[memory] consolidation_cc_failover = true` to fall back to \
                             the Model engine."
                        ));
                    }
                    if !service
                        .has_route(Route::Model(mem.consolidation_model.as_deref()))
                        .await
                    {
                        return Ok(TurnOutcome::Skipped(Skip::NoLogin));
                    }
                    warn!(
                        reason = %reason,
                        "Claude Code consolidation unavailable — falling back to Model engine"
                    );
                    execute_model_turn(service, unit, job_id, mem).await
                }
            }
        }
    }
}

/// The model engine: one bounded agent turn against the consolidation
/// prompt, as `unit`, in a fresh session per run, offered only
/// [`CONSOLIDATION_TOOLS`]. The channel keeps the consolidation prefix its
/// cost records' origin is read from; the task id carries the scheduler's
/// job prefix, which the job history joins on. The turn works on the unit's
/// own memory and its reply goes nowhere, so it reads the unit's context as
/// the unit's own turn would; the job's history, which everyone may read,
/// records only that the unit was consolidated.
async fn execute_model_turn(
    service: &Arc<crate::serve::AgentService>,
    unit: Unit,
    job_id: &str,
    mem: &workforce_common::settings::MemoryConfig,
) -> Result<TurnOutcome> {
    let at = Utc::now();
    let run = format!("{job_id}:{}", unit.identity());
    let channel = isolated_run_channel(CONSOLIDATION_CHANNEL_PREFIX, &run, at);
    let task_id = isolated_run_channel("scheduler", &run, at);
    workforce_agent::runner::with_tool_whitelist(
        Some(ToolWhitelist::for_own_session(
            &CONSOLIDATION_TOOLS.map(str::to_string),
        )),
        Box::pin(service.process_turn(
            &channel,
            "scheduler",
            CONSOLIDATION_PROMPT,
            None,
            None,
            None,
            None,
            None,
            Some(CONSOLIDATION_MAX_ROUNDS),
            mem.consolidation_model.as_deref(),
            Some(&task_id),
            None,
            Some(&unit.identity()),
            crate::serve::RunAuthor::Principal,
        )),
    )
    .await
    .map(|outcome| match outcome {
        TurnOutcome::Answered(_) => TurnOutcome::Answered("consolidated".to_string()),
        skipped => skipped,
    })
}

#[derive(Debug)]
pub struct ScanReport {
    pub sessions_scanned: usize,
    pub turns_processed: usize,
    pub memories_stored: usize,
    pub failed_sessions: usize,
}

/// Result of scanning a single session.
#[derive(Default)]
struct SessionScanOutcome {
    turns_processed: usize,
    memories_stored: usize,
    /// `true` if any turn failed extraction or the transcript could not be
    /// read. Drives FU-1 cursor pinning so failures retry on the next run.
    failed: bool,
}

/// Pick the cursor value for the next run, given each scanned session's
/// `(updated_at, failed)` pair. Pins to the earliest failed session so it
/// retries on the next scan; otherwise advances to `now` (FU-1).
fn pin_cursor_after_scan<I>(sessions: I, now: DateTime<Utc>) -> DateTime<Utc>
where
    I: IntoIterator<Item = (DateTime<Utc>, bool)>,
{
    sessions
        .into_iter()
        .filter_map(|(updated_at, failed)| failed.then_some(updated_at))
        .min()
        .unwrap_or(now)
}

/// Scan recent session transcripts and extract memorable facts.
///
/// Uses a cursor file to track the last scan timestamp so sessions are
/// only scanned once. Sessions that receive new messages after scanning
/// will be re-scanned on the next run. Each session is extracted as its
/// owner, on their own login, and one whose payer has no login is passed
/// over rather than retried.
pub async fn scan_sessions_and_extract(
    service: &Arc<crate::serve::AgentService>,
    store: Arc<dyn workforce_memory::store::MemoryStore>,
    model: Option<String>,
) -> Result<ScanReport> {
    let mem = service.effective_memory();
    let workspace = service.workspace.as_path();
    let cutoff = read_cursor(workspace).unwrap_or_else(|| {
        Utc::now() - chrono::Duration::seconds(mem.consolidation_lookback_secs as i64)
    });

    // Push agent + cutoff filtering into the store; the channel-taxonomy
    // gate stays in Rust because it's domain logic outside the store
    // layer. The cap is a safety bound, not a budget — with `list_since`
    // doing primary filtering it's reached only on genuinely high-volume
    // agents.
    let recent: Vec<_> = service
        .sessions
        .list_since(service.agent_id.as_str(), cutoff, 2000)?
        .into_iter()
        .filter(|s| !crate::turn_extraction::leaves_no_memory(&s.channel))
        // Shared (multi-human) sessions belong to no single user; consolidating
        // them would widen one room's content into the agent's global memory,
        // recalled for people who were never in the room.
        .filter(|s| !s.is_shared_conversation())
        .collect();

    if recent.is_empty() {
        debug!("Session scan: no new sessions since last scan");
        write_cursor(workspace, Utc::now())?;
        return Ok(ScanReport {
            sessions_scanned: 0,
            turns_processed: 0,
            memories_stored: 0,
            failed_sessions: 0,
        });
    }

    info!(
        sessions = recent.len(),
        cutoff = %cutoff,
        "Session scan: scanning recent sessions"
    );

    let extraction = Extraction {
        store,
        model,
        prompt: mem.passive_prompt.clone(),
        min_turn_tokens: mem.passive_min_turn_tokens,
    };
    let mut total_turns = 0;
    let mut total_stored = 0;
    let mut session_outcomes: Vec<(DateTime<Utc>, bool)> = Vec::with_capacity(recent.len());

    for session in &recent {
        let outcome = service
            .in_background(
                &scan_principal(service, &session.owner),
                Route::Model(extraction.model.as_deref()),
                scan_one_session(service, &extraction, session),
            )
            .await
            .unwrap_or_else(|| {
                info!(session_id = %session.id, reason = %Skip::NoLogin, "Session scan passed a session over");
                SessionScanOutcome::default()
            });

        total_turns += outcome.turns_processed;
        total_stored += outcome.memories_stored;
        session_outcomes.push((session.updated_at, outcome.failed));
    }

    let failed_sessions = session_outcomes
        .iter()
        .filter(|(_, failed)| *failed)
        .count();
    let new_cursor = pin_cursor_after_scan(session_outcomes.iter().copied(), Utc::now());
    if failed_sessions > 0 {
        warn!(
            failed_sessions,
            cursor = %new_cursor,
            "Session scan: pinning cursor to earliest failure for retry"
        );
    }
    write_cursor(workspace, new_cursor)?;

    Ok(ScanReport {
        sessions_scanned: recent.len(),
        turns_processed: total_turns,
        memories_stored: total_stored,
        failed_sessions,
    })
}

/// What a session scan extracts with: where memories go, the model, the
/// prompt, and the smallest turn worth reading.
struct Extraction {
    store: Arc<dyn workforce_memory::store::MemoryStore>,
    model: Option<String>,
    prompt: Option<String>,
    min_turn_tokens: u32,
}

/// Who a session owned by `owner` is extracted as. A person's session runs
/// as that person, on their own login. A legacy owner (a Slack id, `system`)
/// names no person, so the session runs as nobody and its extraction is
/// skipped before any call; it is billed to whoever pays for the agent's own
/// work, in case a call is ever made.
fn scan_principal(service: &crate::serve::AgentService, owner: &str) -> TurnPrincipal {
    service.background_principal(
        Payer::classify(Some(owner))
            .person()
            .map(|person| person.to_string())
            .as_deref(),
    )
}

/// Scan one session's turns, extracting memories as whoever is in scope.
/// Surface `failed = true` when anything went wrong so the caller can pin
/// the cursor for a retry (audit FU-1, 2026-05-15).
async fn scan_one_session(
    service: &crate::serve::AgentService,
    extraction: &Extraction,
    session: &workforce_common::session::Session,
) -> SessionScanOutcome {
    let transcript = match service.sessions.read_transcript(&session.id) {
        Ok(t) => t,
        Err(e) => {
            warn!(session_id = %session.id, error = %e, "Failed to read transcript");
            return SessionScanOutcome {
                failed: true,
                ..SessionScanOutcome::default()
            };
        }
    };

    let mut outcome = SessionScanOutcome::default();
    for pair in extract_turn_pairs(&transcript) {
        let turn_tokens =
            estimate_tokens(&pair.user_content) + estimate_tokens(&pair.assistant_content);
        if turn_tokens < extraction.min_turn_tokens {
            continue;
        }
        outcome.turns_processed += 1;

        let result = crate::passive_memory::extract_and_store(
            Arc::clone(&service.llm),
            Arc::clone(&extraction.store),
            service.agent_id.clone(),
            &session.channel,
            Some(session.id.clone()),
            vec![pair.user_id.clone(), pair.assistant_id.clone()],
            Some(pair.event_time),
            &pair.user_content,
            &pair.assistant_content,
            extraction.model.clone(),
            extraction.prompt.clone(),
            Some(Arc::clone(&service.cost_tracker)),
        )
        .await;

        match result {
            Ok(count) => outcome.memories_stored += count,
            Err(e) => {
                warn!(
                    session_id = %session.id,
                    error = %e,
                    "Failed to extract memories from turn"
                );
                outcome.failed = true;
            }
        }
    }
    outcome
}

/// A user→assistant turn pair pulled from a session transcript, carrying the
/// message IDs so passively-extracted memories can link back to the originals.
struct TurnPair {
    user_id: String,
    user_content: String,
    assistant_id: String,
    assistant_content: String,
    /// Timestamp of the assistant message — used as `event_time` so that
    /// memories backfilled from old sessions are dated when the conversation
    /// happened, not when the scanner ran.
    event_time: DateTime<Utc>,
}

/// Extract consecutive user→assistant message pairs from a transcript.
fn extract_turn_pairs(transcript: &[workforce_common::session::TranscriptEntry]) -> Vec<TurnPair> {
    transcript
        .iter()
        .enumerate()
        .filter(|(_, e)| e.role == MessageRole::User)
        .filter_map(|(i, user_entry)| {
            let assistant = transcript[i + 1..]
                .iter()
                .find(|e| e.role == MessageRole::Assistant)?;
            (!user_entry.content.is_empty() && !assistant.content.is_empty()).then(|| TurnPair {
                user_id: user_entry.id.clone(),
                user_content: user_entry.content.clone(),
                assistant_id: assistant.id.clone(),
                assistant_content: assistant.content.clone(),
                event_time: assistant.timestamp,
            })
        })
        .collect()
}

fn cursor_path(workspace: &Path) -> std::path::PathBuf {
    workspace.join("memory").join(CURSOR_FILENAME)
}

fn read_cursor(workspace: &Path) -> Option<DateTime<Utc>> {
    let path = cursor_path(workspace);
    let content = std::fs::read_to_string(&path).ok()?;
    content.trim().parse::<DateTime<Utc>>().ok()
}

fn write_cursor(workspace: &Path, when: DateTime<Utc>) -> Result<()> {
    let path = cursor_path(workspace);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(&path, when.to_rfc3339())?;
    Ok(())
}

/// Pre-seed the consolidation cursor so the next scheduled scan reprocesses
/// every session updated on or after `since`. Used by the
/// `workforce memory backfill` recovery tool when a window of memory
/// extraction was lost (e.g. the FU-1 / spawn-context regression that
/// dropped writes for sessions outside the live-extraction path).
pub fn preseed_consolidation_cursor(workspace: &Path, since: DateTime<Utc>) -> Result<()> {
    write_cursor(workspace, since)
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_common::session::TranscriptEntry;

    /// The consolidation prompt drives a real agent turn, so it must document
    /// action-dispatch tools in the callable form — the space form is what made
    /// a model invent `notes_write_human` and drop the write.
    #[test]
    fn consolidation_prompt_uses_the_real_call_shape() {
        assert_eq!(
            workforce_common::identity::space_form_violation(CONSOLIDATION_PROMPT),
            None
        );
        assert!(CONSOLIDATION_PROMPT.contains(r#"notes(action="write_profile")"#));
    }

    #[tokio::test]
    async fn pass_units_skip_a_suspended_person() {
        let tmp = tempfile::tempdir().unwrap();
        let (service, active, suspended) =
            crate::serve::testing::service_with_members(tmp.path()).await;

        let units = pass_units(&service);

        assert!(units.contains(&Unit::Person(active.id)));
        assert!(!units.contains(&Unit::Person(suspended.id)));
        assert!(Unit::unowned().all(|unit| units.contains(&unit)));
    }

    #[test]
    fn default_interval_is_4_hours() {
        assert_eq!(DEFAULT_CONSOLIDATION_INTERVAL_SECS, 14400);
    }

    fn entry(role: MessageRole, content: &str) -> TranscriptEntry {
        TranscriptEntry {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: "test".into(),
            timestamp: Utc::now(),
            role,
            content: content.into(),
            tool_metadata: None,
            token_usage: None,
        }
    }

    #[test]
    fn extract_turn_pairs_basic() {
        let transcript = vec![
            entry(MessageRole::User, "Hello"),
            entry(MessageRole::Assistant, "Hi there"),
            entry(MessageRole::User, "How are you?"),
            entry(MessageRole::Assistant, "I'm good"),
        ];
        let pairs = extract_turn_pairs(&transcript);
        assert_eq!(pairs.len(), 2);
        assert_eq!(pairs[0].user_content, "Hello");
        assert_eq!(pairs[0].assistant_content, "Hi there");
        assert_eq!(pairs[1].user_content, "How are you?");
        assert_eq!(pairs[1].assistant_content, "I'm good");
        assert_eq!(pairs[0].user_id, transcript[0].id);
        assert_eq!(pairs[0].assistant_id, transcript[1].id);
    }

    #[test]
    fn extract_turn_pairs_skips_tool_messages() {
        let transcript = vec![
            entry(MessageRole::User, "Run a test"),
            entry(MessageRole::Tool, "test output"),
            entry(MessageRole::Assistant, "Tests passed"),
        ];
        let pairs = extract_turn_pairs(&transcript);
        assert_eq!(pairs.len(), 1);
        assert_eq!(pairs[0].user_content, "Run a test");
        assert_eq!(pairs[0].assistant_content, "Tests passed");
    }

    #[test]
    fn extract_turn_pairs_skips_empty() {
        let transcript = vec![
            entry(MessageRole::User, ""),
            entry(MessageRole::Assistant, "response"),
            entry(MessageRole::User, "valid"),
            entry(MessageRole::Assistant, ""),
        ];
        let pairs = extract_turn_pairs(&transcript);
        assert_eq!(pairs.len(), 0);
    }

    #[test]
    fn session_identity_uses_uuid_owners_only() {
        let valid_uuid = "f1706d96-86c8-492a-a88b-b259ecb08a25";
        assert!(uuid::Uuid::parse_str(valid_uuid).is_ok());

        // Non-UUID owners should produce empty (unscoped) identity
        for owner in &["system", "ws-user", "anon", "U0FAKEUSER2", ""] {
            assert!(
                uuid::Uuid::parse_str(owner).is_err(),
                "{owner} should not parse as UUID"
            );
        }
    }

    #[test]
    fn cursor_round_trip() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();

        assert!(read_cursor(workspace).is_none());

        let when = Utc::now();
        write_cursor(workspace, when).unwrap();
        let cursor = read_cursor(workspace).unwrap();
        assert!((when - cursor).num_milliseconds().abs() < 5);
    }

    #[test]
    fn cursor_round_trip_accepts_pinned_past_timestamp() {
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path();
        let pinned = Utc::now() - chrono::Duration::hours(3);
        write_cursor(workspace, pinned).unwrap();
        let cursor = read_cursor(workspace).unwrap();
        assert!((pinned - cursor).num_milliseconds().abs() < 5);
    }

    /// FU-1: cursor pinning ensures failed sessions retry on the next scan.
    #[test]
    fn pin_cursor_advances_to_now_when_no_failures() {
        let now = Utc::now();
        let t1 = now - chrono::Duration::hours(2);
        let t2 = now - chrono::Duration::hours(1);
        let result = pin_cursor_after_scan([(t1, false), (t2, false)], now);
        assert_eq!(result, now);
    }

    #[test]
    fn pin_cursor_pins_to_earliest_failure() {
        let now = Utc::now();
        let early = now - chrono::Duration::hours(3);
        let mid = now - chrono::Duration::hours(2);
        let late = now - chrono::Duration::hours(1);
        let result = pin_cursor_after_scan([(early, false), (mid, true), (late, true)], now);
        assert_eq!(result, mid);
    }

    #[test]
    fn pin_cursor_pins_to_earliest_when_first_fails() {
        let now = Utc::now();
        let early = now - chrono::Duration::hours(3);
        let late = now - chrono::Duration::hours(1);
        let result = pin_cursor_after_scan([(early, true), (late, false)], now);
        assert_eq!(result, early);
    }

    #[test]
    fn pin_cursor_handles_empty_input() {
        let now = Utc::now();
        let result = pin_cursor_after_scan(std::iter::empty(), now);
        assert_eq!(result, now);
    }

    #[test]
    fn pin_cursor_orders_independent_of_input_sequence() {
        let now = Utc::now();
        let a = now - chrono::Duration::hours(1);
        let b = now - chrono::Duration::hours(2);
        let c = now - chrono::Duration::hours(3);
        let abc = pin_cursor_after_scan([(a, true), (b, true), (c, true)], now);
        let cba = pin_cursor_after_scan([(c, true), (b, true), (a, true)], now);
        assert_eq!(abc, c);
        assert_eq!(cba, c);
    }
}
