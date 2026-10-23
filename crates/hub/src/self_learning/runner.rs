//! `SelfLearningRunner` — the per-`AgentService` background producer.
//!
//! Mirrors [`crate::heartbeat::HeartbeatRunner`]: a `tokio::spawn`'d loop with
//! a `CancellationToken` and an mpsc wake channel. Always spawned; it no-ops
//! while `self_learning_enabled()` is false, so toggling the per-agent override
//! starts/stops curation with no restart.
//!
//! Each tick (cadence re-read live from `effective.tick_secs`):
//! 1. gate on `self_learning_enabled()`;
//! 2. `ensure_self_learning_store()` (lazy open on hot-enable);
//! 3. freshness sweep — items past `expires_at` → `Stale` + memory forgotten,
//!    and a per-plan re-verify worklist collected;
//! 4. for each enabled, due plan with a `run_as_identity`, run ONE read-only
//!    curation turn via `run_isolated(AgentProfile::Curator)` scoped to the
//!    plan's identity and the `SELF_LEARNING_CURATION` task-local.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use chrono_tz::Tz;
use serde::{Deserialize, Serialize};
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

use workforce_agent::llm::provider::ProviderKind;
use workforce_common::learning_plan::{self, CurationMode, LearningPlan};
use workforce_common::session::TurnPrincipal;
use workforce_common::settings::{CurationSource, SelfLearningConfig};
use workforce_memory::store::MemoryStore;
use workforce_memory::types::{MemoryFilter, MemoryId};
use workforce_self_learning::{
    DeepExpertiseStore, ItemStatus, KnowledgeItem, NodeQuery, SelfLearningStore,
};

use super::record::{CurationCtx, SELF_LEARNING_CURATION};
use super::{deep, experiential, DEEP_EXPERTISE_TAG, SELF_LEARNED_TAG, TASK_LABEL_PREFIX};
use crate::job_runner::AgentProfile;
use crate::serve::{AgentService, IsolatedRoute, IsolatedRun};
use crate::user_config::as_principal;

/// Persisted last-run timestamps, one per plan id. Sidecar at
/// `{workspace}/learning/.state.json`, the same shape as heartbeat state.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
struct RunnerState {
    #[serde(default)]
    last_runs: HashMap<String, i64>,
}

impl RunnerState {
    fn path(workspace: &Path) -> PathBuf {
        learning_plan::learning_plans_dir(workspace).join(".state.json")
    }

    fn load(workspace: &Path) -> Self {
        std::fs::read_to_string(Self::path(workspace))
            .ok()
            .and_then(|s| serde_json::from_str(&s).ok())
            .unwrap_or_default()
    }

    fn save(&self, workspace: &Path) -> eyre::Result<()> {
        let dir = learning_plan::learning_plans_dir(workspace);
        std::fs::create_dir_all(&dir)?;
        std::fs::write(Self::path(workspace), serde_json::to_string_pretty(self)?)?;
        Ok(())
    }

    fn last_run(&self, plan_id: &str) -> Option<DateTime<Utc>> {
        self.last_runs
            .get(plan_id)
            .and_then(|ts| DateTime::from_timestamp(*ts, 0))
    }

    fn mark_run(&mut self, plan_id: &str, at: DateTime<Utc>) {
        self.last_runs.insert(plan_id.to_string(), at.timestamp());
    }
}

/// Last-run timestamp per plan id, for the REST plan listing.
pub fn plan_last_runs(workspace: &Path) -> HashMap<String, DateTime<Utc>> {
    RunnerState::load(workspace)
        .last_runs
        .into_iter()
        .filter_map(|(id, ts)| DateTime::from_timestamp(ts, 0).map(|t| (id, t)))
        .collect()
}

/// Command sent through the wake channel.
#[derive(Debug, Clone)]
enum WakeCommand {
    /// Run all due plans now.
    All,
    /// Force one plan to run now regardless of schedule.
    Plan(String),
}

/// Handle to trigger an immediate curation run (used by the REST `run now`
/// trigger in increment 3).
#[derive(Clone)]
pub struct SelfLearningWakeHandle {
    wake_tx: tokio::sync::mpsc::Sender<WakeCommand>,
}

impl SelfLearningWakeHandle {
    /// Run all due plans now.
    pub async fn wake(&self) -> bool {
        self.wake_tx.send(WakeCommand::All).await.is_ok()
    }
}

#[async_trait::async_trait]
impl crate::hosts::SelfLearningWake for SelfLearningWakeHandle {
    async fn wake_plan(&self, plan_id: String) -> bool {
        self.wake_tx.send(WakeCommand::Plan(plan_id)).await.is_ok()
    }
}

/// The self-learning runner.
pub struct SelfLearningRunner {
    service: Arc<AgentService>,
    workspace: PathBuf,
    user_tz: Tz,
    wake_rx: tokio::sync::mpsc::Receiver<WakeCommand>,
    /// When set, curation runs only while this instance is the cluster leader,
    /// so a multi-instance HA deployment doesn't run every plan N times. `None`
    /// (standalone / no election) ⇒ always runs. Mirrors `HeartbeatRunner`.
    leader: Option<Arc<AtomicBool>>,
    /// Latches once the one-shot orphan sweep has run this process.
    swept: AtomicBool,
}

impl SelfLearningRunner {
    /// Construct a runner with a wake handle for manual triggers.
    pub fn new(
        service: Arc<AgentService>,
        workspace: &Path,
        user_tz: Tz,
    ) -> (Self, SelfLearningWakeHandle) {
        let (wake_tx, wake_rx) = tokio::sync::mpsc::channel(8);
        let handle = SelfLearningWakeHandle { wake_tx };
        let runner = Self {
            service,
            workspace: workspace.to_path_buf(),
            user_tz,
            wake_rx,
            leader: None,
            swept: AtomicBool::new(false),
        };
        (runner, handle)
    }

    /// Gate curation on cluster leadership. When set, [`Self::tick`] no-ops on
    /// non-leaders so only one instance runs each plan.
    pub fn set_leader_flag(&mut self, flag: Arc<AtomicBool>) {
        self.leader = Some(flag);
    }

    /// Run the curation loop until cancelled. Re-reads `tick_secs` each tick so
    /// a cadence edit takes effect without a restart.
    pub async fn run(mut self, cancel: tokio_util::sync::CancellationToken) {
        info!("Self-learning runner started");
        loop {
            let tick_secs = self.service.effective_self_learning().tick_secs.max(1);
            tokio::select! {
                _ = tokio::time::sleep(std::time::Duration::from_secs(tick_secs)) => {
                    self.tick(None, &cancel).await;
                }
                wake = self.wake_rx.recv() => {
                    match wake {
                        None => {
                            info!("Self-learning wake channel closed, shutting down");
                            break;
                        }
                        Some(WakeCommand::All) => self.tick(None, &cancel).await,
                        Some(WakeCommand::Plan(id)) => self.tick(Some(&id), &cancel).await,
                    }
                }
                _ = cancel.cancelled() => {
                    info!("Self-learning runner stopped");
                    break;
                }
            }
        }
    }

    /// One tick. `forced_plan` runs that plan regardless of schedule (manual
    /// `run now`); otherwise every enabled, due plan runs. `cancel` aborts a CC
    /// backend's wait for a concurrency slot on shutdown.
    async fn tick(&self, forced_plan: Option<&str>, cancel: &CancellationToken) {
        if !self.service.self_learning_enabled() {
            debug!("Self-learning disabled, skipping tick");
            return;
        }
        // In multi-instance mode only the leader curates, so a plan runs once
        // cluster-wide rather than once per replica.
        if self
            .leader
            .as_ref()
            .is_some_and(|f| !f.load(Ordering::Relaxed))
        {
            debug!("Skipping self-learning tick (not leader)");
            return;
        }
        crate::self_learning::ensure_store(&self.service).await;
        let Some(store) = self.service.self_learning_store() else {
            debug!("Self-learning store unavailable, skipping tick");
            return;
        };

        // One-shot startup sweep: reclaim self-learned memories a crash may have
        // orphaned (forgotten-but-not, or unlinked) between an item's write
        // steps. Harmless to the surface (the SQL join already drops them) but
        // frees vector-store space.
        if !self.swept.swap(true, Ordering::Relaxed) {
            if let Some(memory) = self.service.memory_store.clone() {
                match sweep_orphan_memories(store.as_ref(), memory.as_ref()).await {
                    Ok(n) if n > 0 => info!(forgotten = n, "Self-learning orphan sweep complete"),
                    Ok(_) => {}
                    Err(e) => warn!(error = %e, "Self-learning orphan sweep failed"),
                }
                if let Some(deep_store) = self.service.deep_expertise_store() {
                    match sweep_orphan_deep_memories(deep_store.as_ref(), memory.as_ref()).await {
                        Ok(n) if n > 0 => {
                            info!(forgotten = n, "Deep-expertise orphan sweep complete")
                        }
                        Ok(_) => {}
                        Err(e) => warn!(error = %e, "Deep-expertise orphan sweep failed"),
                    }
                }
            }
        }

        let cfg = self.service.effective_self_learning();
        let agent_id = self.service.agent_id.as_str().to_string();
        let now = Utc::now();

        // Freshness sweep, off the hot path: expire stale items and collect a
        // per-plan re-verify worklist.
        let reverify = self.freshness_sweep(store.as_ref(), &agent_id, now).await;

        let plans = match learning_plan::list_learning_plans(&self.workspace) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "Failed to load learning plans");
                return;
            }
        };

        let mut state = RunnerState::load(&self.workspace);
        for plan in &plans {
            if !plan.meta.enabled {
                continue;
            }
            let Some(run_as) = plan
                .meta
                .run_as_identity
                .as_deref()
                .filter(|s| !s.trim().is_empty())
            else {
                debug!(plan = %plan.id, "Plan has no run_as_identity, inert");
                continue;
            };
            let due = match forced_plan {
                Some(forced) => forced == plan.id,
                None => self.is_plan_due(plan, &state, now),
            };
            if !due {
                continue;
            }
            // Held, not run and not marked run: it runs when next due once
            // the person is back.
            if self.service.suspended(run_as).is_some() {
                debug!(plan = %plan.id, "Plan held: the person it runs as is suspended");
                continue;
            }
            let worklist = reverify.get(&plan.id).cloned().unwrap_or_default();
            self.run_plan(plan, run_as, &cfg, &worklist, now, cancel)
                .await;
            state.mark_run(&plan.id, Utc::now());
            if let Err(e) = state.save(&self.workspace) {
                warn!(error = %e, "Failed to persist self-learning runner state");
            }
        }
    }

    /// Expire items past `expires_at`: `Stale` + forget memory + clear the link
    /// (clear first so a crash leaves a reclaimable orphan, never a void link).
    /// Returns the re-verify worklist grouped by plan.
    async fn freshness_sweep(
        &self,
        store: &dyn SelfLearningStore,
        agent_id: &str,
        now: DateTime<Utc>,
    ) -> HashMap<String, Vec<KnowledgeItem>> {
        let due = match store.due_for_reverify(agent_id, now) {
            Ok(items) => items,
            Err(e) => {
                warn!(error = %e, "Self-learning freshness sweep query failed");
                return HashMap::new();
            }
        };
        let mut by_plan: HashMap<String, Vec<KnowledgeItem>> = HashMap::new();
        for item in due {
            if let Some(mid) = item.memory_id.clone() {
                let _ = store.set_memory_id(item.id, None);
                if let Some(memory) = self.service.memory_store.as_ref() {
                    let _ = memory.forget(&MemoryId(mid)).await;
                }
            }
            let _ = store.set_status(item.id, ItemStatus::Stale, None);
            by_plan.entry(item.plan_id.clone()).or_default().push(item);
        }
        by_plan
    }

    /// A plan's work as `run_as`, billed to the person its background work
    /// falls to when `run_as` is not a person.
    fn principal(&self, run_as: &str) -> TurnPrincipal {
        self.service.background_principal(Some(run_as))
    }

    /// Run one curation turn for a plan under its `run_as` identity and the
    /// curation task-local, on the plan's effective source. Every mode runs
    /// the same read-only `Curator` turn and records through the same scope;
    /// only the prompt and the post-turn grounding differ.
    async fn run_plan(
        &self,
        plan: &LearningPlan,
        run_as: &str,
        cfg: &SelfLearningConfig,
        worklist: &[KnowledgeItem],
        now: DateTime<Utc>,
        cancel: &CancellationToken,
    ) {
        let route = self.curation_route(plan, cfg);
        // The deep-expertise mode deepens ONE tree node per turn instead of
        // recording many atomic facts; it has its own target selection, prompt
        // and post-turn quality gate. The atomic-item path below is unchanged.
        if plan.meta.resolved_mode() == CurationMode::DeepExpertise {
            self.run_deep_expertise(plan, run_as, cfg, now, &route, cancel)
                .await;
            return;
        }
        // Experiential expertise deepens the SAME tree from the agent's own
        // session transcripts: it reuses the deep loop's target selection,
        // prompt and record tool, swapping the source (transcripts not code),
        // the system prompt, and the grounding (transcript-presence, never the
        // code quality gate).
        if plan.meta.resolved_mode() == CurationMode::ExperientialExpertise {
            self.run_experiential(plan, run_as, cfg, now, &route, cancel)
                .await;
            return;
        }

        let max_items = plan.meta.max_items_per_run.unwrap_or(cfg.max_items_per_run);
        let freshness_days = plan
            .meta
            .freshness_days
            .unwrap_or(cfg.default_freshness_days);
        let agent_id = self.service.agent_id.as_str().to_string();

        let approved = self.recall_for_status(&agent_id, plan, ItemStatus::Approved);
        let rejected = self.recall_for_status(&agent_id, plan, ItemStatus::Rejected);
        let prompt = build_curation_prompt(plan, &approved, &rejected, worklist, now);

        let ctx = CurationCtx::new(agent_id, plan.id.clone(), max_items, freshness_days);
        let recorded = Arc::clone(&ctx.recorded);
        if let Err(e) = self
            .curate(
                plan,
                run_as,
                &prompt,
                &curation_system_prompt(max_items),
                (ctx, CurationMode::Items),
                &route,
                cancel,
            )
            .await
        {
            warn!(plan = %plan.id, identity = %run_as, error = %e, "Self-learning curation turn failed");
        }

        info!(
            plan = %plan.id,
            identity = %run_as,
            route = ?route.route,
            items_recorded = recorded.load(Ordering::Relaxed),
            "Self-learning curation turn completed",
        );
    }

    /// The read-only `Curator` turn for `plan` as `run_as`, recording
    /// through `curation`'s tool, on `route`. Ends early once `cancel` fires.
    #[allow(clippy::too_many_arguments)]
    async fn curate(
        &self,
        plan: &LearningPlan,
        run_as: &str,
        prompt: &str,
        system_prompt: &str,
        curation: (CurationCtx, CurationMode),
        route: &CurationRoute,
        cancel: &CancellationToken,
    ) -> eyre::Result<()> {
        let task_label = format!("{TASK_LABEL_PREFIX}{}", plan.id);
        let run = IsolatedRun {
            principal: self.principal(run_as),
            route: route.route.clone(),
        };
        let turn = as_principal(
            &run.principal,
            SELF_LEARNING_CURATION.scope(
                curation.0.clone(),
                self.service.run_isolated(
                    "self-learning",
                    prompt,
                    route.model.as_deref(),
                    Some(system_prompt),
                    &run,
                    AgentProfile::Curator,
                    Some(curation),
                    Some(&task_label),
                ),
            ),
        );
        tokio::select! {
            outcome = turn => outcome.map(drop),
            () = cancel.cancelled() => Err(eyre::eyre!("cancelled (shutdown)")),
        }
    }

    /// The deep-expertise quality gate on `node`, run and paid for as the
    /// plan's curation turn is.
    async fn quality_gate(
        &self,
        store: &dyn DeepExpertiseStore,
        node: workforce_self_learning::DeepNode,
        run_as: &str,
    ) {
        as_principal(
            &self.principal(run_as),
            crate::self_learning::quality_gate::deep_expertise_quality_gate(
                &self.service,
                store,
                node,
                run_as,
            ),
        )
        .await
    }

    /// Where a plan's curation turns run: on the agent's routes from the
    /// curation model; on the Claude Code source, on Claude Code alone, or
    /// first when the plan may fall back to the agent's models.
    fn curation_route(&self, plan: &LearningPlan, cfg: &SelfLearningConfig) -> CurationRoute {
        let model = self.effective_curation_model(plan, cfg);
        match plan.meta.source.unwrap_or(cfg.source) {
            CurationSource::Model => CurationRoute {
                model,
                route: IsolatedRoute::Agent,
            },
            CurationSource::ClaudeCode => {
                let spec = claude_code_spec(model.as_deref());
                CurationRoute {
                    route: match cfg.cc_allow_failover_to_models {
                        true => IsolatedRoute::Agent,
                        false => IsolatedRoute::Pinned(spec.clone()),
                    },
                    model: Some(spec),
                }
            }
        }
    }

    /// One deep-expertise deepening turn for a plan: load the tree, pick the
    /// thinnest node ([`deep::select_target`]), run a single read-only `Curator`
    /// turn under the deepening protocol (recording via `deep_expertise.record_node`),
    /// then run the post-turn quality gate. Mirrors [`Self::run_curation_model`]'s
    /// scope/identity wiring; the producer and post-processing differ.
    async fn run_deep_expertise(
        &self,
        plan: &LearningPlan,
        run_as: &str,
        cfg: &SelfLearningConfig,
        now: DateTime<Utc>,
        route: &CurationRoute,
        cancel: &CancellationToken,
    ) {
        let Some(store) = self.service.deep_expertise_store() else {
            debug!(plan = %plan.id, "Deep-expertise store unavailable, skipping turn");
            return;
        };
        // Deep-expertise runs mint child seeds; the separate (smaller) child
        // budget damps the tree-growth ratchet that made runs ever more
        // expensive. A per-plan `max_items_per_run` may only tighten it.
        let max_items = plan
            .meta
            .max_items_per_run
            .unwrap_or(cfg.deep_max_children_per_run)
            .min(cfg.deep_max_children_per_run);
        let freshness_days = plan
            .meta
            .freshness_days
            .unwrap_or(cfg.default_freshness_days);
        let agent_id = self.service.agent_id.as_str().to_string();

        let query = NodeQuery {
            plan_id: Some(plan.id.clone()),
            ..Default::default()
        };
        let nodes = match store.list_nodes(&agent_id, &query) {
            Ok(nodes) => nodes,
            Err(e) => {
                warn!(plan = %plan.id, error = %e, "Deep-expertise: failed to load tree");
                return;
            }
        };
        let Some(target) = deep::select_target(&nodes, &plan.meta.name) else {
            debug!(plan = %plan.id, "Deep-expertise: no target to deepen this turn");
            return;
        };

        let prompt = deep::build_deep_prompt(&plan.meta.name, &plan.body, &nodes, &target, now);
        let system_prompt = deep::deep_system_prompt(&target.title, max_items);

        let ctx = CurationCtx::new(agent_id.clone(), plan.id.clone(), max_items, freshness_days);
        let recorded = Arc::clone(&ctx.recorded);
        let target_path = workforce_self_learning::node_path_hash(
            &agent_id,
            &plan.id,
            target.parent_id,
            &target.title,
        );

        let outcome = self
            .curate(
                plan,
                run_as,
                &prompt,
                &system_prompt,
                (ctx, CurationMode::DeepExpertise),
                route,
                cancel,
            )
            .await;
        if let Err(e) = outcome {
            warn!(plan = %plan.id, identity = %run_as, error = %e, "Deep-expertise curation turn failed");
            return;
        }

        // Re-load the target by its tree position; the turn may have updated it
        // in place or (a seed) created it. If the model recorded nothing for the
        // target, there is nothing to gate.
        let gated = match store.find_by_path(&agent_id, &target_path) {
            Ok(Some(node)) => {
                self.quality_gate(store.as_ref(), node, run_as).await;
                true
            }
            Ok(None) => false,
            Err(e) => {
                warn!(plan = %plan.id, error = %e, "Deep-expertise: failed to re-load target node");
                false
            }
        };

        info!(
            plan = %plan.id,
            identity = %run_as,
            target = %target.title,
            nodes_recorded = recorded.load(Ordering::Relaxed),
            gated,
            "Deep-expertise deepening turn completed",
        );
    }

    /// One experiential-expertise deepening turn: reuse the deep loop's target
    /// selection ([`deep::select_target`]), tree store and `deep_expertise.record_node`
    /// tool, but feed the Curator the agent's own session transcripts
    /// ([`experiential::scan_experiential_sessions`], formatted as an
    /// `## Operational Experience` block appended to the deep prompt) under the
    /// experiential system prompt, and ground the result on transcript presence
    /// ([`experiential::ground_experiential`]) — NEVER the code quality gate,
    /// which would falsely mark an operational lesson `Verified`.
    async fn run_experiential(
        &self,
        plan: &LearningPlan,
        run_as: &str,
        cfg: &SelfLearningConfig,
        now: DateTime<Utc>,
        route: &CurationRoute,
        cancel: &CancellationToken,
    ) {
        let Some(store) = self.service.deep_expertise_store() else {
            debug!(plan = %plan.id, "Experiential: deep-expertise store unavailable, skipping turn");
            return;
        };
        // Deep-expertise runs mint child seeds; the separate (smaller) child
        // budget damps the tree-growth ratchet that made runs ever more
        // expensive. A per-plan `max_items_per_run` may only tighten it.
        let max_items = plan
            .meta
            .max_items_per_run
            .unwrap_or(cfg.deep_max_children_per_run)
            .min(cfg.deep_max_children_per_run);
        let freshness_days = plan
            .meta
            .freshness_days
            .unwrap_or(cfg.default_freshness_days);
        let lookback_secs = plan
            .meta
            .transcript_lookback_secs
            .unwrap_or(cfg.experiential_lookback_secs);
        let agent_id = self.service.agent_id.as_str().to_string();

        let query = NodeQuery {
            plan_id: Some(plan.id.clone()),
            ..Default::default()
        };
        let nodes = match store.list_nodes(&agent_id, &query) {
            Ok(nodes) => nodes,
            Err(e) => {
                warn!(plan = %plan.id, error = %e, "Experiential: failed to load tree");
                return;
            }
        };
        let Some(target) = deep::select_target(&nodes, &plan.meta.name) else {
            debug!(plan = %plan.id, "Experiential: no target to deepen this turn");
            return;
        };

        let turns = match experiential::scan_experiential_sessions(
            self.service.sessions.as_ref(),
            &self.workspace,
            &agent_id,
            lookback_secs,
            now,
        ) {
            Ok(turns) => turns,
            Err(e) => {
                warn!(plan = %plan.id, error = %e, "Experiential: failed to scan sessions");
                return;
            }
        };
        if turns.is_empty() {
            debug!(plan = %plan.id, "Experiential: no recent session experience to distil this turn");
            return;
        }

        let prompt = format!(
            "{}\n{}",
            deep::build_deep_prompt(&plan.meta.name, &plan.body, &nodes, &target, now),
            experiential::format_experiential_section(&turns),
        );
        let system_prompt = experiential_system_prompt(max_items);

        let ctx = CurationCtx::new(agent_id.clone(), plan.id.clone(), max_items, freshness_days);
        let recorded = Arc::clone(&ctx.recorded);
        let target_path = workforce_self_learning::node_path_hash(
            &agent_id,
            &plan.id,
            target.parent_id,
            &target.title,
        );

        let outcome = self
            .curate(
                plan,
                run_as,
                &prompt,
                &system_prompt,
                (ctx, CurationMode::ExperientialExpertise),
                route,
                cancel,
            )
            .await;
        if let Err(e) = outcome {
            warn!(plan = %plan.id, identity = %run_as, error = %e, "Experiential curation turn failed");
            return;
        }

        // Re-load the target by tree position and ground it on transcript
        // presence. An experiential node NEVER goes through the code quality gate
        // — its sources cite transcript turns, not researchable primary sources,
        // so the gate's verifier would extract no claims and vacuously mark it
        // `Verified`. It stays `Modelled` for a human to approve the interpretation;
        // a fabricated/unparseable citation is recorded as a review note.
        let grounded = match store.find_by_path(&agent_id, &target_path) {
            Ok(Some(node)) => {
                let verdict =
                    experiential::ground_experiential(&node, self.service.sessions.as_ref());
                if !verdict.is_grounded() {
                    let note = format!(
                        "ungrounded experiential citations: {}",
                        verdict.bad_citations.join("; ")
                    );
                    self.service
                        .note_experiential_ungrounded(store.as_ref(), &node, &note, run_as);
                }
                Some(verdict.is_grounded())
            }
            Ok(None) => None,
            Err(e) => {
                warn!(plan = %plan.id, error = %e, "Experiential: failed to re-load target node");
                None
            }
        };

        info!(
            plan = %plan.id,
            identity = %run_as,
            target = %target.title,
            nodes_recorded = recorded.load(Ordering::Relaxed),
            turns_scanned = turns.len(),
            grounded = ?grounded,
            "Experiential deepening turn completed",
        );
    }

    /// The curation model: the plan's own `model` (tier-resolved here) overrides
    /// the section `[self_learning] model` (resolved in `ResolvedConfig`), which
    /// falls back to the agent's normal resolution (`None`).
    fn effective_curation_model(
        &self,
        plan: &LearningPlan,
        cfg: &SelfLearningConfig,
    ) -> Option<String> {
        let mut model = plan.meta.model.clone();
        if model.is_some() {
            let tier_key = self.service.tier_key.read().clone();
            crate::resolved_config::resolve_routed_field(
                &mut model,
                &self.service.tier_registry(),
                &tier_key,
            );
        }
        model.or_else(|| cfg.model.clone())
    }

    /// Recall this plan's items at a given status, owned by the agent. Used to
    /// hand-feed approved facts / rejected "do not re-learn" into the prompt.
    /// Provisional items are NEVER recalled here (invariant #2).
    fn recall_for_status(
        &self,
        agent_id: &str,
        plan: &LearningPlan,
        status: ItemStatus,
    ) -> Vec<KnowledgeItem> {
        let Some(store) = self.service.self_learning_store() else {
            return Vec::new();
        };
        let filter = workforce_self_learning::ItemQuery {
            plan_id: Some(plan.id.clone()),
            status: Some(status),
            limit: Some(50),
            ..Default::default()
        };
        store.list_items(agent_id, &filter).unwrap_or_default()
    }

    /// Cron (via croner) OR interval vs the persisted `last_run`. Never run
    /// before ⇒ due. Mirrors heartbeat's `is_check_due`.
    fn is_plan_due(&self, plan: &LearningPlan, state: &RunnerState, now: DateTime<Utc>) -> bool {
        plan_due(plan, state.last_run(&plan.id), now, self.user_tz)
    }
}

/// The model and routes a plan's curation turns run on.
#[derive(Debug, Clone, PartialEq, Eq)]
struct CurationRoute {
    model: Option<String>,
    route: IsolatedRoute,
}

/// The `claude-code` spec a curation model names: itself when it already
/// routes there, the bare model on Claude Code, else the standard tier.
fn claude_code_spec(model: Option<&str>) -> String {
    let claude_code = ProviderKind::ClaudeCode.canonical_name();
    match model.map(|m| (m, crate::subscription::routed_kind(m))) {
        Some((spec, Some(ProviderKind::ClaudeCode))) => spec.to_string(),
        Some((bare, None)) if !bare.contains(':') => format!("{claude_code}:{bare}"),
        _ => format!("{claude_code}:standard"),
    }
}

/// Schedule decision for one plan, factored out of [`SelfLearningRunner`] so the
/// logic is unit-testable without an `AgentService`: cron (via croner) OR
/// interval vs `last_run`; never run before ⇒ due.
fn plan_due(
    plan: &LearningPlan,
    last_run: Option<DateTime<Utc>>,
    now: DateTime<Utc>,
    tz: Tz,
) -> bool {
    if let Some(cron_expr) = plan.meta.schedule.as_deref() {
        let anchor = last_run.unwrap_or_else(|| DateTime::from_timestamp(0, 0).unwrap());
        if let Some(next_fire) = cron_next_run(cron_expr, &anchor, tz) {
            return next_fire <= now;
        }
    }
    match (plan.meta.interval_secs, last_run) {
        (Some(interval), Some(last)) => (now - last).num_seconds() as u64 >= interval,
        (Some(_), None) => true,
        (None, _) => last_run.is_none(),
    }
}

/// Forget self-learned Memory rows no longer referenced by any item row.
/// Mirrors `crate::training::sweep_orphans` (forget-only direction): list the
/// referenced memory ids, scan every `self_learned`-tagged memory across agents,
/// and forget those with no backing row. Returns the count forgotten.
async fn sweep_orphan_memories(
    store: &dyn SelfLearningStore,
    memory: &dyn MemoryStore,
) -> eyre::Result<usize> {
    let referenced: HashSet<String> = store.list_all_memory_ids()?.into_iter().collect();
    forget_orphan_memories(&referenced, SELF_LEARNED_TAG, memory).await
}

/// The deep-expertise counterpart to [`sweep_orphan_memories`]: re-recording a
/// node in place writes a fresh Memory and unlinks the prior one, so a
/// `deep_expertise`-tagged memory not referenced by any node row leaks. Build
/// the still-referenced set from the tree store and forget the rest.
async fn sweep_orphan_deep_memories(
    store: &dyn DeepExpertiseStore,
    memory: &dyn MemoryStore,
) -> eyre::Result<usize> {
    let referenced: HashSet<String> = store.list_all_memory_ids()?.into_iter().collect();
    forget_orphan_memories(&referenced, DEEP_EXPERTISE_TAG, memory).await
}

/// Upper bound on rows the orphan sweep scans per run. Every backend caps an
/// unset `MemoryFilter::limit` at a small page (Weaviate 10, SQLite/InMemory
/// 100), which silently reduced the sweep to a no-op: each self-learning run
/// minted a fresh orphan per re-recorded node while the sweep saw only the
/// first page — 337 stale `deep_expertise` bodies on driftwerk-prod by
/// 2026-07-05. 10k matches Weaviate's default QUERY_MAXIMUM_RESULTS, far
/// above any real tag population (hundreds).
const ORPHAN_SWEEP_SCAN_LIMIT: usize = 10_000;

/// The filter the orphan sweep scans with: every `tag`-tagged memory across
/// agents, up to [`ORPHAN_SWEEP_SCAN_LIMIT`].
fn orphan_sweep_filter(tag: &str) -> MemoryFilter {
    MemoryFilter {
        tags: Some(vec![tag.to_string()]),
        system_unscoped: true,
        limit: Some(ORPHAN_SWEEP_SCAN_LIMIT),
        ..Default::default()
    }
}

/// Scan every `tag`-tagged memory across agents and forget those whose id is not
/// in `referenced`. Shared by the atomic-item and deep-expertise sweeps; the
/// only difference between them is the tag and how the referenced set is built.
async fn forget_orphan_memories(
    referenced: &HashSet<String>,
    tag: &str,
    memory: &dyn MemoryStore,
) -> eyre::Result<usize> {
    let filter = orphan_sweep_filter(tag);
    let mut forgotten = 0;
    for mem in memory.list(filter).await? {
        if referenced.contains(&mem.id.0) {
            continue;
        }
        if memory.forget(&mem.id).await.is_ok() {
            forgotten += 1;
        }
    }
    Ok(forgotten)
}

/// Next cron occurrence after `anchor`, evaluated in `tz`, back in UTC.
fn cron_next_run(expr: &str, anchor: &DateTime<Utc>, tz: Tz) -> Option<DateTime<Utc>> {
    let cron = match croner::Cron::new(expr).parse() {
        Ok(c) => c,
        Err(e) => {
            warn!(expr = expr, error = %e, "Invalid learning-plan cron expression");
            return None;
        }
    };
    let anchor_in_tz = anchor.with_timezone(&tz);
    cron.find_next_occurrence(&anchor_in_tz, false)
        .ok()
        .map(|next| next.with_timezone(&Utc))
}

/// The fixed experiential deepening protocol handed to the curation turn,
/// verbatim from the design doc with the per-run node budget substituted.
fn experiential_system_prompt(max_items: u32) -> String {
    format!(
        "You are deepening your knowledge tree from your OWN OPERATIONAL EXPERIENCE, not from code. \
Below are excerpts from your recent sessions. Do NOT restate them — DISTILL durable lessons.\n\n\
Mine the transcripts for, about the domain being deepened:\n\
- failures and their cause (what went wrong and why),\n\
- gotchas and edge cases hit in practice (\"X looks right but Y\"),\n\
- corrections the user gave you (what you had wrong),\n\
- non-obvious operational nuance (\"in practice Z, because…\").\n\n\
For each durable lesson call deep_expertise.record_node to add it to the tree — woven into the \
relevant domain node (update its body with the \"in practice…\" nuance) or as a child under it. \
Each lesson MUST cite the exact turn that evidences it as a source with kind \"experience\" and \
reference \"session_id:turn_index\". Record nothing you cannot cite to a specific turn. A one-off \
or trivial exchange is not a durable lesson — skip it.\n\n\
Record at most {max_items} lessons this run. Stop when the durable experience is distilled."
    )
}

/// The fixed instruction block handed to the curation turn (design §6).
fn curation_system_prompt(max_items: u32) -> String {
    format!(
        "You are running a self-learning curation turn. Research the objectives below ONLY via \
         your read-only tools (knowledge, context, web, the named read sources, and database \
         reads). You CANNOT write to, post to, or modify any source — your only write is \
         `self_learning.record`.\n\n\
         For each atomic fact you establish:\n\
         - Call `self_learning.record` once, with the fact as `claim` and a primary \
         `source.reference` (page id, url, query, or entity). A claim with no resolvable source \
         is rejected — never record an inference about your own reasoning, and never cite another \
         self-learned item.\n\
         - Mark pricing, security, contractual or legal claims as `high_stakes`.\n\
         - To correct an out-of-date APPROVED fact (listed under \"Established facts\"), pass \
         `supersedes` with that item's id rather than recording a duplicate.\n\
         - Do not re-learn anything under \"Do not re-learn\".\n\n\
         Record at most {max_items} items this run. When you have nothing further to record, stop."
    )
}

/// Assemble the curation prompt: plan body + ready sources + approved facts +
/// rejected "do not re-learn" + this plan's re-verify worklist.
fn build_curation_prompt(
    plan: &LearningPlan,
    approved: &[KnowledgeItem],
    rejected: &[KnowledgeItem],
    worklist: &[KnowledgeItem],
    now: DateTime<Utc>,
) -> String {
    let mut out = String::new();
    out.push_str(&format!("# Learning objectives: {}\n\n", plan.meta.name));
    out.push_str(plan.body.trim());
    out.push_str("\n\n");

    if !plan.meta.sources.is_empty() {
        out.push_str("## Read these sources\n");
        for src in &plan.meta.sources {
            let instance = src
                .instance
                .as_deref()
                .map(|i| format!(" (instance: {i})"))
                .unwrap_or_default();
            let scope = src
                .scope
                .as_deref()
                .map(|s| format!(" — {s}"))
                .unwrap_or_default();
            out.push_str(&format!("- {}{instance}{scope}\n", src.skill));
        }
        out.push('\n');
    }

    if !approved.is_empty() {
        out.push_str("## Established facts (human-approved — treat as ground truth)\n");
        for item in approved {
            out.push_str(&format!(
                "- [{}] {} (src: {})\n",
                item.id, item.claim, item.source.reference
            ));
        }
        out.push('\n');
    }

    if !rejected.is_empty() {
        out.push_str("## Do not re-learn (previously rejected)\n");
        for item in rejected {
            let note = item
                .review_note
                .as_deref()
                .map(|n| format!(" — {n}"))
                .unwrap_or_default();
            out.push_str(&format!("- {}{note}\n", item.claim));
        }
        out.push('\n');
    }

    if !worklist.is_empty() {
        out.push_str("## Re-verify these against the source (they have gone stale)\n");
        for item in worklist {
            out.push_str(&format!(
                "- {} (src: {})\n",
                item.claim, item.source.reference
            ));
        }
        out.push('\n');
    }

    out.push_str(&format!(
        "Current time (UTC): {}\n",
        now.format("%Y-%m-%d %H:%M:%S")
    ));
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_self_learning::{ItemSource, SourceKind};

    fn plan(id: &str, schedule: Option<&str>, interval: Option<u64>) -> LearningPlan {
        LearningPlan {
            id: id.to_string(),
            meta: workforce_common::learning_plan::LearningPlanMeta {
                name: format!("Plan {id}"),
                enabled: true,
                schedule: schedule.map(str::to_string),
                interval_secs: interval,
                run_as_identity: Some("id-uuid".into()),
                priority: 0,
                max_items_per_run: None,
                freshness_days: None,
                model: None,
                source: None,
                mode: None,
                transcript_lookback_secs: None,
                sources: Vec::new(),
            },
            body: "Learn the thing.".into(),
        }
    }

    fn item(claim: &str, status: ItemStatus) -> KnowledgeItem {
        let now = Utc::now();
        KnowledgeItem {
            id: uuid::Uuid::new_v4(),
            agent_id: "a".into(),
            plan_id: "p".into(),
            topic: "t".into(),
            claim: claim.into(),
            content_hash: "h".into(),
            memory_id: None,
            status,
            confidence: 0.8,
            high_stakes: false,
            source: ItemSource {
                kind: SourceKind::Skill,
                skill: Some("notion".into()),
                reference: "page-1".into(),
                excerpt: None,
            },
            created_at: now,
            created_by: "id".into(),
            verified_at: now,
            expires_at: Some(now),
            reviewed_at: None,
            reviewed_by: None,
            review_note: None,
            superseded_by: None,
        }
    }

    fn runner_state(plan_id: &str, last: DateTime<Utc>) -> RunnerState {
        let mut s = RunnerState::default();
        s.mark_run(plan_id, last);
        s
    }

    #[test]
    fn interval_due_when_never_run() {
        let (svc_tz, p) = (Tz::UTC, plan("p", None, Some(600)));
        assert!(is_due(&p, &RunnerState::default(), Utc::now(), svc_tz));
    }

    #[test]
    fn interval_not_due_within_window() {
        let now = Utc::now();
        let p = plan("p", None, Some(600));
        let state = runner_state("p", now - chrono::Duration::seconds(300));
        assert!(!is_due(&p, &state, now, Tz::UTC));
    }

    #[test]
    fn interval_due_after_window() {
        let now = Utc::now();
        let p = plan("p", None, Some(600));
        let state = runner_state("p", now - chrono::Duration::seconds(700));
        assert!(is_due(&p, &state, now, Tz::UTC));
    }

    #[test]
    fn no_schedule_runs_once_then_never() {
        let now = Utc::now();
        let p = plan("p", None, None);
        assert!(is_due(&p, &RunnerState::default(), now, Tz::UTC));
        let state = runner_state("p", now - chrono::Duration::days(1));
        assert!(!is_due(&p, &state, now, Tz::UTC));
    }

    #[test]
    fn cron_due_after_fire_time() {
        use chrono::TimeZone;
        let p = plan("p", Some("0 9 * * *"), None);
        let last = Utc.with_ymd_and_hms(2026, 1, 15, 8, 0, 0).unwrap();
        let state = runner_state("p", last);
        let after = Utc.with_ymd_and_hms(2026, 1, 15, 9, 30, 0).unwrap();
        assert!(is_due(&p, &state, after, Tz::UTC));
        let before = Utc.with_ymd_and_hms(2026, 1, 15, 8, 30, 0).unwrap();
        assert!(!is_due(&p, &state, before, Tz::UTC));
    }

    #[test]
    fn prompt_includes_sections() {
        let p = plan("p", None, None);
        let approved = vec![item("billing is monthly", ItemStatus::Approved)];
        let rejected = vec![item("billing is weekly", ItemStatus::Rejected)];
        let worklist = vec![item("mrr net of refunds", ItemStatus::Stale)];
        let out = build_curation_prompt(&p, &approved, &rejected, &worklist, Utc::now());
        assert!(out.contains("Established facts"));
        assert!(out.contains("billing is monthly"));
        assert!(out.contains("Do not re-learn"));
        assert!(out.contains("Re-verify"));
    }

    #[test]
    fn prompt_omits_empty_sections() {
        let p = plan("p", None, None);
        let out = build_curation_prompt(&p, &[], &[], &[], Utc::now());
        assert!(!out.contains("Established facts"));
        assert!(!out.contains("Do not re-learn"));
        assert!(!out.contains("Re-verify"));
    }

    #[test]
    fn state_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let now = Utc::now();
        let mut s = RunnerState::default();
        s.mark_run("p", now);
        s.save(dir.path()).unwrap();
        let loaded = RunnerState::load(dir.path());
        assert!(loaded.last_run("p").is_some());
    }

    #[tokio::test]
    async fn sweep_forgets_only_unreferenced_self_learned_memories() {
        use workforce_core::types::AgentId;
        use workforce_memory::in_memory::InMemoryStore;
        use workforce_memory::types::{Memory, MemoryLayer, MemorySource, Visibility};
        use workforce_self_learning::sqlite::SqliteSelfLearningStore;

        let memory = InMemoryStore::new();
        let store = SqliteSelfLearningStore::in_memory().unwrap();
        let tagged = |body: &str| {
            let mut m = Memory::new(
                body.to_string(),
                MemoryLayer::Organisation,
                AgentId("a".into()),
            );
            m.visibility = Visibility::Global;
            m.source = MemorySource::Curated;
            m.tags = vec![SELF_LEARNED_TAG.to_string()];
            m
        };
        let referenced = memory.remember(tagged("kept")).await.unwrap();
        let orphan = memory.remember(tagged("orphan")).await.unwrap();

        let mut it = item("kept", ItemStatus::Provisional);
        it.memory_id = Some(referenced.0.clone());
        store.upsert_item(&it).unwrap();

        assert_eq!(sweep_orphan_memories(&store, &memory).await.unwrap(), 1);
        assert!(memory
            .get(&referenced)
            .await
            .unwrap()
            .is_some_and(|m| !m.deleted));
        assert!(memory.get(&orphan).await.unwrap().is_none_or(|m| m.deleted));
    }

    #[tokio::test]
    async fn deep_sweep_forgets_only_unreferenced_deep_memories() {
        use workforce_core::types::AgentId;
        use workforce_memory::in_memory::InMemoryStore;
        use workforce_memory::types::{Memory, MemoryLayer, MemorySource, Visibility};
        use workforce_self_learning::sqlite::SqliteSelfLearningStore;
        use workforce_self_learning::{
            node_path_hash, DeepNode, ItemSource, NodeStatus, SourceKind,
        };

        let memory = InMemoryStore::new();
        let store = SqliteSelfLearningStore::in_memory().unwrap();
        let tagged = |body: &str| {
            let mut m = Memory::new(
                body.to_string(),
                MemoryLayer::Organisation,
                AgentId("a".into()),
            );
            m.visibility = Visibility::Global;
            m.source = MemorySource::Curated;
            m.tags = vec![DEEP_EXPERTISE_TAG.to_string()];
            m
        };
        let referenced = memory.remember(tagged("kept")).await.unwrap();
        let orphan = memory.remember(tagged("orphan")).await.unwrap();

        let now = Utc::now();
        let node = DeepNode {
            id: uuid::Uuid::new_v4(),
            agent_id: "a".into(),
            plan_id: "p".into(),
            parent_id: None,
            depth: 0,
            title: "Root".into(),
            path_hash: node_path_hash("a", "p", None, "Root"),
            body: "body".into(),
            sources: vec![ItemSource {
                kind: SourceKind::Codebase,
                skill: None,
                reference: "src/lib.rs".into(),
                excerpt: None,
            }],
            coverage: Vec::new(),
            memory_id: Some(referenced.0.clone()),
            status: NodeStatus::Modelled,
            confidence: 0.7,
            created_at: now,
            created_by: "id".into(),
            verified_at: now,
            expires_at: Some(now),
            reviewed_at: None,
            reviewed_by: None,
            review_note: None,
            superseded_by: None,
        };
        store.upsert_node(&node).unwrap();

        assert_eq!(
            sweep_orphan_deep_memories(&store, &memory).await.unwrap(),
            1
        );
        assert!(memory
            .get(&referenced)
            .await
            .unwrap()
            .is_some_and(|m| !m.deleted));
        assert!(memory.get(&orphan).await.unwrap().is_none_or(|m| m.deleted));
    }

    /// The sweep must scan the WHOLE tag population, not one store page:
    /// with an unset limit every backend caps `list` at a small default
    /// (Weaviate 10, InMemory/SQLite 100), which let orphans accumulate
    /// faster than the sweep collected them (337 on driftwerk-prod).
    #[tokio::test]
    async fn sweep_scans_past_the_default_store_page() {
        use workforce_core::types::AgentId;
        use workforce_memory::in_memory::InMemoryStore;
        use workforce_memory::types::{Memory, MemoryLayer, MemorySource, Visibility};
        use workforce_self_learning::sqlite::SqliteSelfLearningStore;

        let memory = InMemoryStore::new();
        let store = SqliteSelfLearningStore::in_memory().unwrap();

        let orphan_count = 150; // > InMemoryStore's default list page of 100
        for i in 0..orphan_count {
            let mut m = Memory::new(
                format!("stale body {i}"),
                MemoryLayer::Organisation,
                AgentId("a".into()),
            );
            m.visibility = Visibility::Global;
            m.source = MemorySource::Curated;
            m.tags = vec![DEEP_EXPERTISE_TAG.to_string()];
            memory.remember(m).await.unwrap();
        }

        assert_eq!(
            sweep_orphan_deep_memories(&store, &memory).await.unwrap(),
            orphan_count
        );
    }

    #[test]
    fn orphan_sweep_filter_sets_an_explicit_scan_limit() {
        let filter = orphan_sweep_filter(DEEP_EXPERTISE_TAG);
        assert_eq!(filter.limit, Some(ORPHAN_SWEEP_SCAN_LIMIT));
        assert!(filter.system_unscoped);
        assert_eq!(filter.tags, Some(vec![DEEP_EXPERTISE_TAG.to_string()]));
    }

    // Exercise the real schedule decision (not a copy) via the extracted
    // `plan_due`, threading `last_run` from state the way `is_plan_due` does.
    fn is_due(plan: &LearningPlan, state: &RunnerState, now: DateTime<Utc>, tz: Tz) -> bool {
        plan_due(plan, state.last_run(&plan.id), now, tz)
    }

    mod claude_code_source {
        use super::*;
        use crate::agent_turn::tests::{claude_first, hub, Hub, ALICE};
        use crate::claude_code::tests::scripted::{answer, print};
        use workforce_common::settings::WorkforceConfig;

        fn curating(config: &mut WorkforceConfig) {
            config.self_learning.enabled = true;
        }

        /// A plan of `mode` curated on the Claude Code source, as Alice.
        fn claude_plan(mode: CurationMode) -> LearningPlan {
            let mut plan = plan("tree", None, Some(3600));
            plan.meta.mode = Some(mode);
            plan.meta.source = Some(CurationSource::ClaudeCode);
            plan.meta.run_as_identity = Some(ALICE.into());
            plan
        }

        async fn curate(hub: &Hub, plan: &LearningPlan) {
            crate::self_learning::ensure_store(&hub.service).await;
            let (runner, _) =
                SelfLearningRunner::new(Arc::clone(&hub.service), &hub.service.workspace, Tz::UTC);
            runner
                .run_plan(
                    plan,
                    ALICE,
                    &hub.service.effective_self_learning(),
                    &[],
                    Utc::now(),
                    &CancellationToken::new(),
                )
                .await;
        }

        /// The one curation turn ran on Claude Code, one-shot, and never on
        /// the agent's own provider.
        async fn ran_on_claude_code(hub: &Hub) {
            let calls = hub.claude.calls();
            assert_eq!(calls.len(), 1, "the curation turn ran on the CLI");
            assert_eq!(calls[0].flag("--model"), Some("sonnet"));
            assert!(calls[0].has("--no-session-persistence"));
            assert_eq!(
                hub.chatgpt_requests().await,
                0,
                "not on the agent's provider"
            );
        }

        #[tokio::test]
        async fn self_learning_deep_honours_claude_code_source() {
            let hub = hub(curating).await;
            hub.claude_login(ALICE);
            hub.chatgpt_login(ALICE).await;
            hub.claude.reply(None, &answer("Deepened."));

            curate(&hub, &claude_plan(CurationMode::DeepExpertise)).await;

            ran_on_claude_code(&hub).await;
        }

        #[tokio::test]
        async fn deep_gate_on_claude_code_runs_as_run_as() {
            let hub = hub(|config| {
                claude_first(config);
                curating(config);
            })
            .await;
            let token = hub.claude_login(ALICE);
            hub.service
                .tools
                .register(crate::mcp_turn::testing::Probe::named("code.read"));
            hub.claude.reply(None, &answer("[]"));
            crate::self_learning::ensure_store(&hub.service).await;
            let store = hub
                .service
                .deep_expertise_store()
                .expect("the store is open");
            let now = Utc::now();
            let node = workforce_self_learning::DeepNode {
                id: uuid::Uuid::new_v4(),
                agent_id: hub.service.agent_id.to_string(),
                plan_id: "tree".into(),
                parent_id: None,
                depth: 0,
                title: "Invoicing".into(),
                path_hash: "invoicing".into(),
                body: "Invoices are generated by `billing::run` on the first of the month.".into(),
                sources: Vec::new(),
                coverage: Vec::new(),
                memory_id: None,
                status: workforce_self_learning::NodeStatus::Modelled,
                confidence: 0.5,
                created_at: now,
                created_by: ALICE.into(),
                verified_at: now,
                expires_at: None,
                reviewed_at: None,
                reviewed_by: None,
                review_note: None,
                superseded_by: None,
            };
            store.upsert_node(&node).unwrap();
            let (runner, _) =
                SelfLearningRunner::new(Arc::clone(&hub.service), &hub.service.workspace, Tz::UTC);

            runner.quality_gate(store.as_ref(), node, ALICE).await;

            let calls = hub.claude.calls();
            assert!(!calls.is_empty(), "the gate ran on Claude Code");
            assert!(
                calls.iter().all(|call| call.token == token),
                "on the plan's run-as login, never a shared one"
            );
        }

        #[tokio::test]
        async fn self_learning_experiential_honours_claude_code_source() {
            let hub = hub(curating).await;
            hub.claude_login(ALICE);
            hub.chatgpt_login(ALICE).await;
            hub.ask(ALICE, "how do I rotate the staging key?")
                .await
                .unwrap();
            assert_eq!(hub.chatgpt_requests().await, 1);
            hub.claude.reply(None, &answer("Distilled."));

            curate(&hub, &claude_plan(CurationMode::ExperientialExpertise)).await;

            let calls = hub.claude.calls();
            assert_eq!(calls.len(), 1, "the curation turn ran on the CLI");
            assert!(calls[0].text().contains("how do I rotate the staging key?"));
            assert_eq!(
                hub.chatgpt_requests().await,
                1,
                "not on the agent's provider"
            );
        }

        #[tokio::test]
        async fn self_learning_items_records_via_mcp_record_tool() {
            let hub = hub(curating).await;
            hub.claude_login(ALICE);
            let call = serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {
                    "name": "self_learning_record",
                    "arguments": {
                        "topic": "billing",
                        "claim": "Invoices go out on the first of the month.",
                        "confidence": 0.9,
                        "source": { "kind": "skill", "skill": "notion", "reference": "page-1" },
                    },
                },
            });
            hub.claude.script(
                None,
                &format!(
                    r#"cfg=$(awk 'prev=="--mcp-config"{{print}} {{prev=$0}}' "$dir/$n.args")
url=$(sed -n 's/.*"url": *"\([^"]*\)".*/\1/p' "$cfg")
auth=$(sed -n 's/.*"Authorization": *"\([^"]*\)".*/\1/p' "$cfg")
curl -s -X POST "$url" -H "Authorization: $auth" -H 'Content-Type: application/json' -H 'Accept: application/json, text/event-stream' --data '{call}' > "$dir/$n.mcp"
{answer}"#,
                    answer = print(&answer("Recorded one fact."))
                ),
            );
            let plan = claude_plan(CurationMode::Items);

            curate(&hub, &plan).await;

            let items = hub
                .service
                .self_learning_store()
                .expect("the store is open")
                .list_items(
                    hub.service.agent_id.as_str(),
                    &workforce_self_learning::ItemQuery {
                        plan_id: Some(plan.id.clone()),
                        ..Default::default()
                    },
                )
                .unwrap();
            assert_eq!(
                items.iter().map(|i| i.claim.as_str()).collect::<Vec<_>>(),
                ["Invoices go out on the first of the month."],
                "recorded through the gated MCP tool, outside the curation scope: {}",
                hub.claude.file("1.mcp")
            );
        }
    }
}
