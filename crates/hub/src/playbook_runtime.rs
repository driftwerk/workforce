//! Playbook runtime — definition store + dispatch.
//!
//! Playbooks are interactive, human-in-the-loop flows (the sibling to
//! workflows). This module holds the loaded [`PlaybookDefinition`]s and drives
//! the controller: starting a playbook, advancing on Continue, exiting, pinning
//! each step's artifact, and posting the per-step control card (which carries
//! the running timer and the Continue/Exit buttons). The actual agent turn still
//! runs on the normal chat loop — the controller only decides *what prompt to
//! inject next* (returned as [`PlaybookOutcome::RunTurn`]).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use eyre::WrapErr;
use tokio::sync::RwLock;
use tracing::{info, warn};
use workforce_common::channels::{ChannelSurface, OutboundMessage};
use workforce_common::playbook_state::{ArtifactRef, PlaybookProgress};
use workforce_core::access::{AccessLevel, TeamMembership, Visibility};
use workforce_orchestrator::playbook::{engine, PlaybookDefinition, StepOutcome};
use workforce_orchestrator::workflow_store::{def_access, RunAsWrite, WorkflowStore};

pub use crate::hub_core::playbooks::PlaybookCard;
pub use crate::hub_core::playbooks::{PlaybookOutcome, PlaybookResolution, PlaybookSummary};
use crate::resource_access::DefListing;
use crate::serve::AgentService;
use crate::team_reach::TeamReach;

/// Playbook definitions live in `workflow_defs` under `kind = "playbook"`.
const KIND: &str = "playbook";

/// Composite addressing key — bare id for system/disk definitions (so existing
/// references stay valid), `{owner}/{id}` for member-owned ones so two members
/// may share an id. Mirrors the workflow runtime.
fn def_key(owner: Option<&str>, id: &str) -> String {
    match owner {
        Some(o) => format!("{o}/{id}"),
        None => id.to_string(),
    }
}

/// A loaded playbook tagged with its owner (`None` = system/disk) and visibility.
#[derive(Clone)]
pub(crate) struct OwnedPlaybook {
    def: PlaybookDefinition,
    owner: Option<String>,
    visibility: Visibility,
}

impl OwnedPlaybook {
    fn key(&self) -> String {
        def_key(self.owner.as_deref(), &self.def.playbook.id)
    }
}

/// Wrap a disk/operator definition as system-owned + globally runnable.
fn system_playbook(def: PlaybookDefinition) -> OwnedPlaybook {
    OwnedPlaybook {
        def,
        owner: None,
        visibility: Visibility::Global,
    }
}

/// Resolve a step's model spec to the concrete routed model string the runner
/// expects (`"powerful"` → `"claude-opus-4-7"`, `"openrouter:anthropic:powerful"`
/// → `"openrouter:anthropic/claude-opus-4.7"`), or `None` when the step declares
/// no model. Resolved once per start/advance against the live tier registry —
/// the same resolution the workflow runtime applies to its per-step model — and
/// stashed in `PlaybookProgress::step_model` so every turn of the step routes
/// identically.
fn resolve_step_model(svc: &AgentService, spec: Option<&str>) -> Option<String> {
    // `save_definition_source` rejects a blank model, but a disk/system def
    // bypasses that check — treat a blank spec as "no override" rather than
    // routing an empty model string to the provider.
    let spec = spec.map(str::trim).filter(|s| !s.is_empty())?;
    let tier_key = svc.tier_key.read().clone();
    let (routing_key, model) = svc.tier_registry().resolve_routed_model(spec, &tier_key);
    Some(workforce_agent::llm::tiers::TierRegistry::format_routed(
        routing_key.as_deref(),
        model,
    ))
}

/// Reject a step's model spec at save time. The tier registry's runtime
/// resolution never errors — it passes an unknown spec straight through — so
/// without this a (global) playbook could embed an arbitrary provider string.
/// Rejects an empty spec and a provider-routed tier whose provider is unknown
/// (`evil:powerful`, bare `openrouter:powerful`); allows bare tiers and concrete
/// model ids, the same leniency the workflow runtime has (a concrete id can't be
/// allowlisted without a full model registry).
fn validate_step_model(spec: &str) -> eyre::Result<()> {
    use workforce_agent::llm::tiers::{parse_provider_tier_key, ModelTier};
    let spec = spec.trim();
    if spec.is_empty() {
        return Err(eyre::eyre!("model override is empty"));
    }
    if let Some((provider_part, last)) = spec.rsplit_once(':') {
        if ModelTier::from_name(last).is_some() {
            parse_provider_tier_key(provider_part)
                .map_err(|e| eyre::eyre!("invalid model override '{spec}': {e}"))?;
        }
    }
    Ok(())
}

/// Append the active step model to a card status line, e.g. `"⏱ working…"` →
/// `"⏱ working… · claude-opus-4-7"`. Renders unchanged when the step has no
/// model. Shared by every card state so the suffix is uniform.
pub(crate) fn status_with_model(status: &str, model: Option<&str>) -> String {
    match model {
        Some(m) => format!("{status} · {m}"),
        None => status.to_string(),
    }
}

/// Holds the loaded playbook definitions, hot-reloadable from disk or DB.
pub struct PlaybookRuntime {
    dir: PathBuf,
    /// Loaded definitions keyed by composite `def_key(owner, id)`.
    defs: RwLock<HashMap<String, OwnedPlaybook>>,
    /// DB-backed definition store (DW-550). When set, definitions read from /
    /// write to Postgres; `dir` is the non-Postgres fallback.
    store: OnceLock<Arc<WorkflowStore>>,
}

impl PlaybookRuntime {
    /// Create a runtime and load any definitions already on disk.
    pub fn new(dir: PathBuf) -> Self {
        let mut map = HashMap::new();
        match PlaybookDefinition::load_dir(&dir) {
            Ok(defs) => {
                for def in defs {
                    let owned = system_playbook(def);
                    map.insert(owned.key(), owned);
                }
            }
            Err(e) => tracing::warn!(error = %e, "Failed to load playbook definitions"),
        }
        Self {
            dir,
            defs: RwLock::new(map),
            store: OnceLock::new(),
        }
    }

    /// Attach the DB definition store. Call before [`Self::reload`].
    pub fn set_store(&self, store: Arc<WorkflowStore>) {
        let _ = self.store.set(store);
    }

    pub(crate) fn def_store(&self) -> Option<&Arc<WorkflowStore>> {
        self.store.get()
    }

    fn block_on<F, T>(fut: F) -> T
    where
        F: std::future::Future<Output = T>,
    {
        tokio::task::block_in_place(|| tokio::runtime::Handle::current().block_on(fut))
    }

    /// Reload definitions (DB when a store is attached, else disk).
    pub async fn reload(&self) -> eyre::Result<usize> {
        let mut map = HashMap::new();
        if let Some(store) = self.def_store() {
            for row in store.list(KIND).await? {
                match PlaybookDefinition::from_toml(&row.toml_source) {
                    Ok(def) => {
                        let owned = OwnedPlaybook {
                            def,
                            owner: row.owner,
                            visibility: row.visibility,
                        };
                        map.insert(owned.key(), owned);
                    }
                    Err(e) => {
                        tracing::warn!(id = %row.id, error = %e, "Skipping invalid playbook def in DB")
                    }
                }
            }
        } else {
            for def in PlaybookDefinition::load_dir(&self.dir)? {
                let owned = system_playbook(def);
                map.insert(owned.key(), owned);
            }
        }
        let count = map.len();
        *self.defs.write().await = map;
        Ok(count)
    }

    /// Resolve a bare playbook id named by an interactive user (who carries no
    /// explicit owner). The user's own playbook shadows one of the same id they
    /// reach some other way; otherwise the single playbook with that id they
    /// may start — global, or a teammate's team-visible one — or
    /// `Ambiguous`/`NotFound`.
    pub async fn resolve_for_user(
        &self,
        bare_id: &str,
        user: &str,
        teams: &dyn TeamMembership,
    ) -> PlaybookResolution {
        let defs = self.defs.read().await;
        if let Some(o) = defs.get(&def_key(Some(user), bare_id)) {
            return PlaybookResolution::Found(o.def.clone());
        }
        let mut open = defs
            .values()
            .filter(|o| {
                o.def.playbook.id == bare_id
                    && def_access(o.owner.as_deref(), o.visibility, Some(user), None, teams)
                        .allows(AccessLevel::Run)
            })
            .map(|o| &o.def);
        match (open.next(), open.next()) {
            (Some(def), None) => PlaybookResolution::Found(def.clone()),
            (None, _) => PlaybookResolution::NotFound,
            (Some(_), Some(_)) => PlaybookResolution::Ambiguous,
        }
    }

    /// Every loaded playbook as a summary (bare id + owner + visibility),
    /// sorted by id. Use [`Self::list_definitions_visible`] for user-facing
    /// surfaces — this is the unfiltered list.
    pub async fn list_definitions(&self) -> Vec<PlaybookSummary> {
        let mut out: Vec<_> = self
            .defs
            .read()
            .await
            .values()
            .map(|o| PlaybookSummary {
                id: o.def.playbook.id.clone(),
                name: o.def.playbook.name.clone(),
                description: o.def.playbook.description.clone(),
                step_count: o.def.steps.len(),
                owner: o.owner.clone(),
                visibility: o.visibility,
            })
            .collect();
        out.sort_by(|a, b| (&a.id, &a.owner).cmp(&(&b.id, &b.owner)));
        out
    }

    /// Playbooks visible to `caller`: an admin sees all; everyone else sees
    /// global definitions plus their own. The owner-aware sibling of
    /// [`Self::resolve_for_user`] — keeps member-private playbooks off shared
    /// listings (Slack `list`, the agent prompt, the gateway).
    pub async fn list_definitions_visible(&self, listing: &DefListing) -> Vec<PlaybookSummary> {
        self.list_definitions()
            .await
            .into_iter()
            .filter(|s| listing.sees(s.owner.as_deref(), s.visibility, &s.id))
            .collect()
    }

    /// Resolve a `(bare id, owner)` pair to the loaded definition's `(owner,
    /// visibility)`, if present — backs the gateway's access checks. `owner ==
    /// None` addresses a system/disk definition (bare key).
    pub async fn definition_ownership(
        &self,
        bare_id: &str,
        owner: Option<&str>,
    ) -> Option<(Option<String>, Visibility)> {
        self.defs
            .read()
            .await
            .get(&def_key(owner, bare_id))
            .map(|o| (o.owner.clone(), o.visibility))
    }

    /// The raw TOML source of a definition (DB when a store is attached, else
    /// disk). `owner` is `None` for system/disk defs, `Some` for a member's own.
    pub fn get_definition_source(&self, id: &str, owner: Option<&str>) -> eyre::Result<String> {
        if let Some(store) = self.def_store() {
            return Self::block_on(store.get(id, KIND, owner))?
                .ok_or_else(|| eyre::eyre!("Playbook '{}' not found", id));
        }
        let path = self.dir.join(format!("{id}.toml"));
        std::fs::read_to_string(&path)
            .wrap_err_with(|| format!("Playbook '{}' not found at {}", id, path.display()))
    }

    /// Validate and persist a definition (DB or disk), then hot-reload. The
    /// TOML's own `id` must match `id`. `owner`/`visibility` place it in the
    /// caller's namespace (`None`/global for system/operator defs); `edit`
    /// records who/where in the revision history.
    pub async fn save_definition_source(
        &self,
        id: &str,
        toml_content: &str,
        owner: Option<&str>,
        visibility: Visibility,
        edit: workforce_orchestrator::workflow_store::EditContext<'_>,
    ) -> eyre::Result<()> {
        let def = PlaybookDefinition::from_toml(toml_content).wrap_err("Invalid playbook TOML")?;
        if def.playbook.id != id {
            return Err(eyre::eyre!(
                "Playbook id mismatch: TOML declares '{}' but saving as '{}'",
                def.playbook.id,
                id
            ));
        }
        // The structural validation in `from_toml` can't reach the tier registry
        // (it lives in `orchestrator`, which doesn't depend on `agent-runtime`),
        // so the model spec is checked here — the one choke point every save path
        // funnels through.
        for step in &def.steps {
            if let Some(spec) = &step.model {
                validate_step_model(spec).map_err(|e| {
                    eyre::eyre!("Playbook '{}' step '{}': {e}", def.playbook.id, step.id)
                })?;
            }
        }
        if let Some(store) = self.def_store() {
            let parsed = serde_json::to_value(&def).ok();
            store
                .save(
                    id,
                    KIND,
                    toml_content,
                    parsed,
                    owner,
                    visibility,
                    RunAsWrite::Unchanged,
                    edit,
                )
                .await?;
        } else if owner.is_some() {
            // The disk fallback is a single flat namespace ({id}.toml) and reloads
            // every file as system/global — so a member's private playbook would
            // silently become global. Refuse it (mirrors the workflow runtime).
            return Err(eyre::eyre!(
                "member-owned playbooks require a database store; this deployment uses the filesystem"
            ));
        } else {
            if !workforce_common::config_files::is_plain_name(id) {
                return Err(eyre::eyre!("'{id}' is not a playbook id"));
            }
            std::fs::create_dir_all(&self.dir)?;
            std::fs::write(self.dir.join(format!("{id}.toml")), toml_content)?;
        }
        self.reload().await?;
        Ok(())
    }

    /// Every definition's raw TOML source, keyed by id (for `export` / pull-all).
    pub fn export_all_sources(&self) -> eyre::Result<HashMap<String, String>> {
        if let Some(store) = self.def_store() {
            let rows = Self::block_on(store.list(KIND))?;
            return Ok(rows.into_iter().map(|r| (r.id, r.toml_source)).collect());
        }
        let mut map = HashMap::new();
        if !self.dir.exists() {
            return Ok(map);
        }
        for entry in std::fs::read_dir(&self.dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "toml") {
                let content = std::fs::read_to_string(&path)?;
                if let Ok(def) = PlaybookDefinition::from_toml(&content) {
                    map.insert(def.playbook.id, content);
                }
            }
        }
        Ok(map)
    }

    /// Delete a definition (DB or disk), then hot-reload. `owner` is `None` for
    /// system/disk defs, `Some` for a member's own. `edit` records who deleted
    /// it in the tombstone revision.
    pub async fn delete_definition(
        &self,
        id: &str,
        owner: Option<&str>,
        edit: workforce_orchestrator::workflow_store::EditContext<'_>,
    ) -> eyre::Result<()> {
        if let Some(store) = self.def_store() {
            store.delete(id, KIND, owner, edit).await?;
        } else if owner.is_some() {
            return Err(eyre::eyre!(
                "member-owned playbooks require a database store; this deployment uses the filesystem"
            ));
        } else {
            let path = self.dir.join(format!("{id}.toml"));
            if !path.exists() {
                return Err(eyre::eyre!(
                    "Playbook '{}' not found at {}",
                    id,
                    path.display()
                ));
            }
            std::fs::remove_file(&path)
                .wrap_err_with(|| format!("Failed to delete {}", path.display()))?;
        }
        self.reload().await?;
        Ok(())
    }

    /// Revision history (newest first, content omitted) for one definition.
    /// Requires the database store.
    pub async fn list_revisions(
        &self,
        id: &str,
        owner: Option<&str>,
    ) -> eyre::Result<Vec<workforce_orchestrator::workflow_store::DefRevision>> {
        let store = self
            .def_store()
            .ok_or_else(|| eyre::eyre!("revision history requires the database store"))?;
        store.list_revisions(KIND, id, owner).await
    }

    /// One revision with its full content. Requires the database store.
    pub async fn get_revision(
        &self,
        id: &str,
        owner: Option<&str>,
        rev: i32,
    ) -> eyre::Result<Option<workforce_orchestrator::workflow_store::DefRevision>> {
        let store = self
            .def_store()
            .ok_or_else(|| eyre::eyre!("revision history requires the database store"))?;
        store.get_revision(KIND, id, owner, rev).await
    }
}

/// Absolute scratch directory for a playbook, re-joining the workspace-relative
/// `progress.scratch` with the live workspace root. The agent writes here
/// (sandboxed to its workspace) and the controller reads it back; keeping the
/// stored path relative means it survives a redeploy with a different mount.
fn scratch_abs(svc: &AgentService, progress: &PlaybookProgress) -> PathBuf {
    svc.workspace.join(&progress.scratch)
}

/// Wall-clock millis since the Unix epoch — the clock for step timing. Kept here
/// (not in the pure engine, which stays clock-free) and shared with the Slack
/// dispatcher's card timer.
pub(crate) fn now_ms() -> i64 {
    use std::time::{SystemTime, UNIX_EPOCH};
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as i64)
        .unwrap_or(0)
}

/// Freeze the card's agent-work duration at the *first* hand-back and never
/// after, returning the frozen value. A step can span several turns (the user
/// iterates, e.g. "change X"); without this, a follow-up turn would refreeze to
/// `now - step_start` and fold the user's deliberation time into the displayed
/// "done in X" — the opposite of the freeze-on-hand-back behaviour chosen for the
/// timer.
pub(crate) fn freeze_work_ms(card: &mut PlaybookCard, now: i64) -> i64 {
    let started = card.started_at_ms;
    *card.work_ms.get_or_insert_with(|| (now - started).max(0))
}

/// Claim the live step card for a hand-back, freezing its work duration and
/// handing back a clone — but only if the tracked card still matches
/// `expected_ts` (the id snapshotted while the runner still guarded the session).
/// A Continue that advanced in the window after the runner was released swaps the
/// tracked card for the next step's; claiming it would freeze and reposition a
/// live working card, so this returns `None` and the hand-back becomes a no-op.
pub(crate) fn claim_card_for_handback(
    cards: &mut HashMap<String, PlaybookCard>,
    session_key: &str,
    expected_ts: Option<&str>,
    now: i64,
) -> Option<(PlaybookCard, i64)> {
    let card = cards.get_mut(session_key)?;
    if Some(card.ts.as_str()) != expected_ts {
        return None;
    }
    let work_ms = freeze_work_ms(card, now);
    Some((card.clone(), work_ms))
}

/// Adopt the re-posted card id `new_ts` as the tracked card, but only if the
/// tracked id still points at the one we moved (`from_ts`). If a concurrent
/// advance has since replaced it, our re-post is an orphan — returned so the
/// caller can delete it rather than leave stale-nonce controls at the thread foot.
pub(crate) fn adopt_reposted_card(
    cards: &mut HashMap<String, PlaybookCard>,
    session_key: &str,
    from_ts: &str,
    new_ts: String,
) -> Option<String> {
    match cards.get_mut(session_key) {
        Some(c) if c.ts == from_ts => {
            c.ts = new_ts;
            None
        }
        _ => Some(new_ts),
    }
}

/// A parsed playbook control directive.
enum Directive {
    /// Continue to the next step. Carries the button nonce when re-injected from
    /// a Continue button click (`cmd: continue <nonce>`), so a stale button is
    /// rejected; the typed `cmd: continue` carries none.
    Continue(Option<String>),
    /// Exit the playbook. Carries the button nonce when re-injected from an Exit
    /// button click, so a stale Exit button can't exit a later step.
    Exit(Option<String>),
    /// Jump to a specific step (a custom branching button), re-injected as
    /// `cmd: goto <nonce> <target>`. The target is a step id, or the reserved
    /// `$finish` token for a "finish" button. Always carries a nonce (buttons
    /// only), guarded like Continue.
    Goto {
        nonce: String,
        target: String,
    },
    Start(String),
    List,
    Status,
}

/// Parse a playbook directive from message text. Only the explicit prefix forms
/// (`/continue`, `cmd: continue`, …) count — a bare word in prose never
/// triggers an advance, so ordinary chat ("continue with that approach") is
/// left untouched.
fn parse_directive(text: &str) -> Option<Directive> {
    let body = crate::commands::command_body(text)?;
    let mut parts = body.split_whitespace();
    match parts.next()?.to_lowercase().as_str() {
        "continue" => Some(Directive::Continue(parts.next().map(str::to_string))),
        "exit" => Some(Directive::Exit(parts.next().map(str::to_string))),
        "goto" => {
            // `goto <nonce> <target>` — both required (button-only directive).
            let nonce = parts.next()?.to_string();
            let target = parts.next()?.to_string();
            Some(Directive::Goto { nonce, target })
        }
        "playbook" => playbook_directive(&mut parts),
        _ => None,
    }
}

/// The `playbook …` directives, read from the words after the noun.
fn playbook_directive<'a>(words: &mut impl Iterator<Item = &'a str>) -> Option<Directive> {
    match words.next().map(str::to_lowercase).as_deref() {
        Some("list") => Some(Directive::List),
        Some("status") => Some(Directive::Status),
        Some("start") => words.next().map(|id| Directive::Start(id.to_string())),
        _ => None,
    }
}

/// Whether the arguments to `cmd: playbook` drive an interactive playbook
/// rather than ask who reaches one ([`crate::def_cmd`]). The two grammars share
/// the noun, and these directives were there first: they are intercepted ahead
/// of any command parsing on Slack and reach the agent elsewhere, so
/// [`crate::commands::parse_command`] must not claim them and answer with usage.
pub fn is_directive_args(args: &str) -> bool {
    playbook_directive(&mut args.split_whitespace()).is_some()
}

fn new_nonce() -> String {
    uuid::Uuid::new_v4().to_string()
}

/// Handle a possible playbook directive on an inbound message. Continue button
/// clicks reach here too, re-injected as `cmd: continue <nonce>` text, so the
/// nonce is parsed from the directive rather than passed separately.
#[allow(clippy::too_many_arguments)]
pub async fn handle_message(
    svc: &Arc<AgentService>,
    runtime: &PlaybookRuntime,
    session_key: &str,
    owner: &str,
    surfaces: &[Arc<dyn ChannelSurface>],
    channel: &str,
    thread_id: &Option<String>,
    text: &str,
) -> PlaybookOutcome {
    let Some(directive) = parse_directive(text) else {
        return PlaybookOutcome::NotHandled;
    };

    let mut session = match svc
        .sessions
        .get_or_create(session_key, owner, svc.agent_id.as_str())
    {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(error = %e, "playbook: failed to load session");
            return PlaybookOutcome::NotHandled;
        }
    };

    // What this person's teams reach, asked once for whatever the line needs.
    let reach = TeamReach::for_service(svc, Some(owner.to_string()));

    match directive {
        Directive::List => {
            // Members see global playbooks, the ones a team of theirs reaches,
            // and their own — never another member's private one.
            let summaries = runtime
                .list_definitions_visible(&DefListing::unshared(Some(owner), reach.clone()))
                .await;
            let body = if summaries.is_empty() {
                "No playbooks are defined.".to_string()
            } else {
                let mut s = String::from(
                    "*Available playbooks* (start with `cmd: playbook start <id>`):\n",
                );
                for p in summaries {
                    s.push_str(&format!(
                        "• `{}` — {} ({} steps): {}\n",
                        p.id, p.name, p.step_count, p.description
                    ));
                }
                s
            };
            post(surfaces, channel, thread_id, &body).await;
            PlaybookOutcome::Consumed
        }

        Directive::Status => {
            let Some(progress) = session.playbook() else {
                post(surfaces, channel, thread_id, "No active playbook.").await;
                return PlaybookOutcome::Consumed;
            };
            let body = match runtime
                .resolve_for_user(&progress.id, owner, &reach)
                .await
                .found()
            {
                Some(def) => build_status_message(&def, &progress),
                None => format!("Playbook `{}` is no longer defined.", progress.id),
            };
            post(surfaces, channel, thread_id, &body).await;
            PlaybookOutcome::Consumed
        }

        Directive::Start(id) => {
            if session.playbook().is_some() {
                post(
                    surfaces,
                    channel,
                    thread_id,
                    "A playbook is already active. `cmd: exit` it first.",
                )
                .await;
                return PlaybookOutcome::Consumed;
            }
            let def = match runtime.resolve_for_user(&id, owner, &reach).await {
                PlaybookResolution::Found(def) => def,
                PlaybookResolution::NotFound => {
                    post(
                        surfaces,
                        channel,
                        thread_id,
                        &format!("Unknown playbook `{id}`. Try `cmd: playbook list`."),
                    )
                    .await;
                    return PlaybookOutcome::Consumed;
                }
                PlaybookResolution::Ambiguous => {
                    post(
                        surfaces,
                        channel,
                        thread_id,
                        &format!(
                            "Multiple global playbooks are named `{id}`. Ask an admin to \
                             rename one so it's unambiguous."
                        ),
                    )
                    .await;
                    return PlaybookOutcome::Consumed;
                }
            };
            // Don't start over an in-flight turn — its end-of-turn whole-object
            // save would otherwise clobber the playbook state we set here.
            if reject_if_busy(svc, session_key, surfaces, channel, thread_id, "start").await {
                return PlaybookOutcome::Consumed;
            }

            // Scratch is workspace-relative and fixed now, so it stays stable
            // across rollover (when session.id changes) and lands inside the
            // agent's file-tool sandbox.
            let scratch = format!("playbooks/{}", session.id);
            let _ = std::fs::create_dir_all(svc.workspace.join(&scratch));
            let mut progress =
                PlaybookProgress::new(&id, def.steps[0].id.clone(), new_nonce(), scratch);
            let started = now_ms();
            progress.step_started_at = Some(started);
            progress.step_model = resolve_step_model(svc, def.steps[0].model.as_deref());
            let prompt = engine::render_step_prompt(&def, 0, &progress).unwrap_or_default();
            session.set_playbook(&progress);
            if let Err(e) = svc.sessions.save(&session) {
                tracing::warn!(error = %e, "playbook: failed to save start state");
                return PlaybookOutcome::NotHandled;
            }
            let display = format!(
                "▶ Playbook '{}' — Step 1/{}: {}",
                def.playbook.name,
                def.steps.len(),
                def.steps[0].title
            );
            // Post the full step list up front (informational; the lean `display`
            // is kept for session history), then the step-1 control card, which
            // the dispatcher anchors the working reactions and live timer on.
            let overview = engine::render_overview(&def);
            post(surfaces, channel, thread_id, &overview).await;
            let header = format!("Step 1/{}: {}", def.steps.len(), def.steps[0].title);
            let anchor_ts = post_step_card(
                svc,
                surfaces,
                channel,
                thread_id,
                session_key,
                header,
                started,
                progress.step_model.clone(),
            )
            .await;
            PlaybookOutcome::RunTurn {
                message: prompt,
                display,
                anchor_ts,
            }
        }

        Directive::Exit(nonce) => {
            // No active playbook: leave the message alone so a bare `cmd: exit`
            // still reaches the agent on non-playbook sessions.
            let Some(progress) = session.playbook() else {
                return PlaybookOutcome::NotHandled;
            };
            // Stale Exit button from an earlier step shouldn't exit a later one.
            if let Some(n) = nonce {
                if n != progress.nonce {
                    post(
                        surfaces,
                        channel,
                        thread_id,
                        "That was an older Exit button — use the latest one.",
                    )
                    .await;
                    return PlaybookOutcome::Consumed;
                }
            }
            // Exit means "stop now", so cancel any in-flight turn rather than
            // bouncing the user with "still working" (which forced a second
            // click). Re-read the session after the cancel settles so we clear
            // the freshest copy, not the one loaded before the turn ended.
            cancel_active_turn(svc, session_key).await;
            // The turn (and its card timer) is now stopped, so finalizing the
            // card can't race a late timer edit — strip its buttons and freeze.
            finalize_step_card(svc, surfaces, session_key).await;
            if let Ok(fresh) = svc
                .sessions
                .get_or_create(session_key, owner, svc.agent_id.as_str())
            {
                session = fresh;
            }
            session.clear_playbook();
            let _ = svc.sessions.save(&session);
            post(
                surfaces,
                channel,
                thread_id,
                "Exited the playbook — your conversation context is preserved, continue normally.",
            )
            .await;
            PlaybookOutcome::Consumed
        }

        Directive::Continue(nonce) => {
            // No active playbook: pass through so a bare `cmd: continue` reaches
            // the agent on non-playbook sessions.
            if session.playbook().is_none() {
                return PlaybookOutcome::NotHandled;
            }
            advance(
                svc,
                runtime,
                &mut session,
                session_key,
                surfaces,
                channel,
                thread_id,
                nonce.as_deref(),
                None,
            )
            .await
        }

        Directive::Goto { nonce, target } => {
            // Goto only ever comes from a button, so there's always an active
            // playbook; if there isn't, the button is stale — drop it quietly.
            if session.playbook().is_none() {
                return PlaybookOutcome::Consumed;
            }
            advance(
                svc,
                runtime,
                &mut session,
                session_key,
                surfaces,
                channel,
                thread_id,
                Some(&nonce),
                Some(&target),
            )
            .await
        }
    }
}

/// Reply "still working" and return `true` when a turn is in flight for this
/// session, so playbook mutations don't race the turn's end-of-turn save.
async fn reject_if_busy(
    svc: &Arc<AgentService>,
    session_key: &str,
    surfaces: &[Arc<dyn ChannelSurface>],
    channel: &str,
    thread_id: &Option<String>,
    verb: &str,
) -> bool {
    if svc.active_runners.read().await.contains_key(session_key) {
        post(
            surfaces,
            channel,
            thread_id,
            &format!("Still working on this step — {verb} once I've finished."),
        )
        .await;
        return true;
    }
    false
}

/// Hard-stop any in-flight turn for this session and wait for it to deregister,
/// so Exit takes effect on the first click. A cancelled turn skips its
/// end-of-turn session save (see `process_message_with_progress`), so clearing
/// the playbook afterwards can't be resurrected by a stale whole-object write.
async fn cancel_active_turn(svc: &Arc<AgentService>, session_key: &str) {
    let stopped = {
        let runners = svc.active_runners.read().await;
        let Some(handle) = runners.get(session_key) else {
            return;
        };
        // Suppress the cancelled turn's stale output, then cancel cooperatively
        // with a hard abort as the fallback (mirrors the dispatch-loop hard-stop).
        *handle.hard_stop_reason.lock().await =
            Some(crate::slack_manager::PLAYBOOK_EXIT_REASON.to_string());
        handle.cancel_token.cancel();
        handle.abort();
        (
            handle.run_id,
            handle.session_id().map(String::from),
            handle.turn_token().map(String::from),
            Arc::clone(&handle.snapshot),
        )
    };
    let (run_id, session_id, turn_token, snapshot) = stopped;
    // Guarded by run_id, so the deadline force-release can never evict a
    // successor claim that took the slot while we polled.
    svc.wait_runner_release(session_key, run_id).await;
    // Release is also the point at which the turn's step tally is complete, so
    // the next turn learns what the exit stopped rather than only that it did.
    if let Some(session_id) = session_id {
        let steps = snapshot.read().await.steps.clone();
        crate::slack_manager::annotate_stopped_session(
            svc,
            &session_id,
            turn_token.as_deref(),
            crate::slack_manager::EXITED_DIRECTIVE,
            &steps,
        )
        .await;
    }
}

/// Advance a running playbook by one step. Reached from both the typed
/// `cmd: continue` and the Continue button (the latter re-injected as text), so
/// the single-consumer dispatch loop serialises double-clicks: the first
/// advance regenerates the nonce, the second is rejected by the stale-nonce
/// guard below.
#[allow(clippy::too_many_arguments)]
async fn advance(
    svc: &Arc<AgentService>,
    runtime: &PlaybookRuntime,
    session: &mut workforce_common::session::Session,
    session_key: &str,
    surfaces: &[Arc<dyn ChannelSurface>],
    channel: &str,
    thread_id: &Option<String>,
    nonce: Option<&str>,
    // `Some(step_id)` for a custom branching button; `Some("$finish")` for a
    // finish button; `None` for the default linear Continue.
    goto_target: Option<&str>,
) -> PlaybookOutcome {
    let Some(mut progress) = session.playbook() else {
        return PlaybookOutcome::NotHandled;
    };
    let Some(def) = runtime
        .resolve_for_user(
            &progress.id,
            &session.owner,
            &TeamReach::for_service(svc, Some(session.owner.clone())),
        )
        .await
        .found()
    else {
        session.clear_playbook();
        let _ = svc.sessions.save(session);
        post(
            surfaces,
            channel,
            thread_id,
            &format!("Playbook `{}` is no longer defined — exited.", progress.id),
        )
        .await;
        return PlaybookOutcome::Consumed;
    };

    // Stale-button guard: a click whose nonce no longer matches is from an
    // earlier step still sitting in the scrollback.
    if let Some(n) = nonce {
        if n != progress.nonce {
            post(
                surfaces,
                channel,
                thread_id,
                "That was an older Continue button — use the latest one.",
            )
            .await;
            return PlaybookOutcome::Consumed;
        }
    }

    // Don't advance while a turn is still running, or we'd pin a half-written
    // artifact (and race its end-of-turn save). `active_runners` is the live
    // source of truth.
    if reject_if_busy(svc, session_key, surfaces, channel, thread_id, "Continue").await {
        return PlaybookOutcome::Consumed;
    }

    // Pin the *current* step's artifact (resolved by id) before leaving it.
    let completed = engine::current_index(&progress, &def)
        .and_then(|i| pin_artifact(&def, i, &scratch_abs(svc, &progress)));
    let outcome = match goto_target {
        Some("$finish") => engine::finish(&mut progress, completed, new_nonce()),
        Some(target) => engine::advance_to(&mut progress, &def, target, completed, new_nonce()),
        None => engine::advance(&mut progress, &def, completed, new_nonce()),
    };

    match outcome {
        StepOutcome::Complete => {
            // Finalize the step we're leaving (freeze its timer, strip buttons).
            finalize_step_card(svc, surfaces, session_key).await;
            session.clear_playbook();
            let _ = svc.sessions.save(session);
            post(
                surfaces,
                channel,
                thread_id,
                &format!("✅ Playbook '{}' complete.", def.playbook.name),
            )
            .await;
            PlaybookOutcome::Consumed
        }
        StepOutcome::Run(idx) => {
            // Finalize the step we're leaving before posting the next card, so
            // only the current step ever carries live buttons.
            finalize_step_card(svc, surfaces, session_key).await;
            let started = now_ms();
            progress.step_started_at = Some(started);
            // Re-resolve unconditionally: a step with no model clears the prior
            // step's, so an earlier `powerful` never bleeds into a later step.
            progress.step_model = resolve_step_model(svc, def.steps[idx].model.as_deref());
            let prompt = engine::render_step_prompt(&def, idx, &progress).unwrap_or_default();
            session.set_playbook(&progress);
            let _ = svc.sessions.save(session);
            let display = format!(
                "▶ Step {}/{}: {}",
                idx + 1,
                def.steps.len(),
                def.steps[idx].title
            );
            let header = format!(
                "Step {}/{}: {}",
                idx + 1,
                def.steps.len(),
                def.steps[idx].title
            );
            let anchor_ts = post_step_card(
                svc,
                surfaces,
                channel,
                thread_id,
                session_key,
                header,
                started,
                progress.step_model.clone(),
            )
            .await;
            PlaybookOutcome::RunTurn {
                message: prompt,
                display,
                anchor_ts,
            }
        }
    }
}

/// Read a step's artifact file and build a pinned reference with a cheap
/// synopsis. Records the path even if the file is missing, so the next step
/// still sees where it was expected.
fn pin_artifact(def: &PlaybookDefinition, step_idx: usize, scratch: &Path) -> Option<ArtifactRef> {
    let step = def.steps.get(step_idx)?;
    let artifact = step.artifact.as_ref()?;
    let path = scratch.join(artifact);
    let path_str = path.to_string_lossy().to_string();
    let synopsis = match std::fs::read_to_string(&path) {
        Ok(content) => engine::synopsis_of(&content),
        Err(_) => "(not written)".to_string(),
    };
    Some(ArtifactRef {
        step_id: step.id.clone(),
        path: path_str,
        synopsis,
    })
}

/// Render a `cmd: playbook status` card: where the user is in the playbook,
/// completed steps with their pinned artifacts, and what's next. The playbook
/// analogue of the workflow status card.
fn build_status_message(def: &PlaybookDefinition, progress: &PlaybookProgress) -> String {
    let total = def.steps.len();
    let step_idx = engine::current_index(progress, def).unwrap_or(0);
    let current = (step_idx + 1).min(total);
    let current_title = def
        .steps
        .get(step_idx)
        .map(|s| s.title.as_str())
        .unwrap_or("—");
    let mut out = format!(
        "▶ *Playbook: {}*\nStep {current}/{total}: {current_title}\n\n*Steps:*\n",
        def.playbook.name
    );
    for (i, step) in def.steps.iter().enumerate() {
        let marker = if i < step_idx {
            "✓"
        } else if i == step_idx {
            "▶"
        } else {
            "•"
        };
        let artifact = progress
            .artifacts
            .iter()
            .find(|a| a.step_id == step.id)
            .map(|a| format!(" — `{}`", a.path))
            .unwrap_or_default();
        out.push_str(&format!("{marker} {}. {}{artifact}\n", i + 1, step.title));
    }
    out.push_str("\n_`cmd: continue` (or the button) to advance, `cmd: exit` to stop._");
    out
}

/// Post a short controller message (divider, ack, list) to the thread,
/// returning the message id of the first surface that accepted it so the
/// dispatcher can anchor the "working" reactions on the `▶ Step N` divider.
/// Multi-surface deployments only get the primary id — the same single-anchor
/// limitation the normal turn path already has.
async fn post_returning(
    surfaces: &[Arc<dyn ChannelSurface>],
    channel: &str,
    thread_id: &Option<String>,
    text: &str,
) -> Option<String> {
    let mut first_id = None;
    for surface in surfaces {
        if let Ok(sent) = surface
            .send_message(&OutboundMessage {
                channel: channel.to_string(),
                text: text.to_string(),
                thread_id: thread_id.clone(),
                username: None,
                icon_url: None,
                blocks: None,
            })
            .await
        {
            first_id = first_id.or(Some(sent.message_id));
        }
    }
    first_id
}

/// The outbound payload for a step card in a given state: a `{emoji} header`
/// line over a status line, with any control rows below. `thread_id` is `Some`
/// only for the initial post — edits target the message by id, so they pass
/// `None`. The header is playbook-author text (the step title), so card messages
/// post directly rather than through the outbound prose pipeline.
fn card_outbound(
    channel: &str,
    thread_id: Option<String>,
    emoji: &str,
    header: &str,
    status: &str,
    button_rows: Vec<serde_json::Value>,
) -> OutboundMessage {
    OutboundMessage {
        channel: channel.to_string(),
        text: format!("{header} — {status}"),
        thread_id,
        username: None,
        icon_url: None,
        blocks: Some(crate::slack_blocks::playbook_card_blocks(
            emoji,
            header,
            status,
            button_rows,
        )),
    }
}

/// Edit an existing step card to a new state across every surface. Shared by the
/// live timer (working tick), the post-turn hand-back (awaiting + buttons), and
/// the finalize paths (done, button-less).
pub(crate) async fn edit_step_card(
    surfaces: &[Arc<dyn ChannelSurface>],
    channel: &str,
    ts: &str,
    emoji: &str,
    header: &str,
    status: &str,
    button_rows: Vec<serde_json::Value>,
) {
    let outbound = card_outbound(channel, None, emoji, header, status, button_rows);
    for surface in surfaces {
        let _ = surface.update_message(channel, ts, &outbound).await;
    }
}

/// Re-render the live step card in its awaiting state with `button_rows` at the
/// *foot* of its thread, so the Continue/Exit controls always sit beneath the
/// latest message and the user never scrolls up to reach them. Slack can't move a
/// message, so "move" means post a fresh card then delete the old one — the post
/// comes first so a failed delete degrades to a harmless same-nonce duplicate
/// rather than losing the controls entirely. Returns the new message id, or `None`
/// when every surface refused the fresh post (then the card is edited in place as
/// a fallback so the controls still appear, and the caller leaves the tracked id
/// unchanged). Like the rest of the card lifecycle (`post_step_card` tracks only
/// the first surface's id, `edit_step_card` re-uses it across all), this assumes a
/// single primary surface — the real Slack deployment.
pub(crate) async fn repost_awaiting_card(
    surfaces: &[Arc<dyn ChannelSurface>],
    card: &PlaybookCard,
    status: &str,
    button_rows: Vec<serde_json::Value>,
) -> Option<String> {
    let outbound = card_outbound(
        &card.channel,
        card.thread_id.clone(),
        "▶",
        &card.header,
        status,
        button_rows,
    );
    let mut new_ts = None;
    for surface in surfaces {
        if let Ok(sent) = surface.send_message(&outbound).await {
            new_ts = new_ts.or(Some(sent.message_id));
        }
    }
    match new_ts {
        Some(new_ts) => {
            for surface in surfaces {
                let _ = surface.delete_message(&card.channel, &card.ts).await;
            }
            Some(new_ts)
        }
        None => {
            for surface in surfaces {
                let _ = surface
                    .update_message(&card.channel, &card.ts, &outbound)
                    .await;
            }
            None
        }
    }
}

/// Post a fresh step "control card" in the *working* state and record it on the
/// service. Returns the card's message id — the anchor the dispatcher hangs the
/// working reactions and the live timer on.
async fn post_step_card(
    svc: &AgentService,
    surfaces: &[Arc<dyn ChannelSurface>],
    channel: &str,
    thread_id: &Option<String>,
    session_key: &str,
    header: String,
    started_at_ms: i64,
    model: Option<String>,
) -> Option<String> {
    let outbound = card_outbound(
        channel,
        thread_id.clone(),
        "▶",
        &header,
        &status_with_model("⏱ working…", model.as_deref()),
        vec![],
    );
    let mut ts = None;
    for surface in surfaces {
        if let Ok(sent) = surface.send_message(&outbound).await {
            ts = ts.or(Some(sent.message_id));
        }
    }
    if let Some(ref ts) = ts {
        svc.playbook_cards.write().await.insert(
            session_key.to_string(),
            PlaybookCard {
                channel: channel.to_string(),
                ts: ts.clone(),
                thread_id: thread_id.clone(),
                header,
                started_at_ms,
                work_ms: None,
                model,
            },
        );
    }
    ts
}

/// Finalize and forget the current step card for a session: strip its buttons
/// and freeze the duration to `✅ Step …: … — done in X`. Called on advance (for
/// the step being left), exit, and completion, so no stale buttons ever linger.
/// No-op when the session has no live card.
pub async fn finalize_step_card(
    svc: &AgentService,
    surfaces: &[Arc<dyn ChannelSurface>],
    session_key: &str,
) {
    let Some(card) = svc.playbook_cards.write().await.remove(session_key) else {
        return;
    };
    let dur = card
        .work_ms
        .unwrap_or_else(|| (now_ms() - card.started_at_ms).max(0));
    let status = status_with_model(
        &format!("done in {}", crate::slack_blocks::fmt_duration_ms(dur)),
        card.model.as_deref(),
    );
    edit_step_card(
        surfaces,
        &card.channel,
        &card.ts,
        "✅",
        &card.header,
        &status,
        vec![],
    )
    .await;
}

/// Fire-and-forget [`post_returning`] for controller messages whose id we don't
/// need (acks, lists, the completion notice).
async fn post(
    surfaces: &[Arc<dyn ChannelSurface>],
    channel: &str,
    thread_id: &Option<String>,
    text: &str,
) {
    post_returning(surfaces, channel, thread_id, text).await;
}

#[cfg(test)]
pub(crate) mod testing {
    use super::*;

    /// A definition tagged with owner + visibility; the name encodes the owner
    /// (`{id}-{owner|sys}`) so a test can assert *which* of two same-id defs won.
    pub(crate) fn owned(id: &str, owner: Option<&str>, visibility: Visibility) -> OwnedPlaybook {
        let tag = owner.unwrap_or("sys");
        OwnedPlaybook {
            def: PlaybookDefinition::from_toml(&format!(
                "[playbook]\nid=\"{id}\"\nname=\"{id}-{tag}\"\n\
                 [[step]]\nid=\"s\"\ntitle=\"S\"\nprompt=\"go\"\n"
            ))
            .unwrap(),
            owner: owner.map(str::to_string),
            visibility,
        }
    }

    pub(crate) async fn runtime_with(items: Vec<OwnedPlaybook>) -> PlaybookRuntime {
        let dir = std::env::temp_dir().join(format!("pb-empty-{}", uuid::Uuid::new_v4()));
        let rt = PlaybookRuntime::new(dir);
        let map = items.into_iter().map(|o| (o.key(), o)).collect();
        *rt.defs.write().await = map;
        rt
    }
}

/// The playbook runtime over the install's playbook directory.
pub(crate) fn runtime(
    config: &workforce_common::settings::WorkforceConfig,
) -> Arc<PlaybookRuntime> {
    let data_dir = std::path::PathBuf::from(&config.hub.data_dir);
    Arc::new(PlaybookRuntime::new(data_dir.join("playbooks")))
}

/// The playbook runtime's definition store, and the watcher that
/// hot-reloads its definitions.
pub(crate) async fn wire(
    service: &Arc<crate::serve::AgentService>,
    config: &workforce_common::settings::WorkforceConfig,
    playbook_runtime: Arc<PlaybookRuntime>,
) {
    let playbooks_dir = playbook_runtime.dir.clone();
    // Playbook definitions share the workflow_defs table (DW-550).
    let pb_def_store = crate::workflow_runtime::shared_workflow_store(config).await;
    if let Some(ref store) = pb_def_store {
        playbook_runtime.set_store(Arc::clone(store));
        if let Err(e) = playbook_runtime.reload().await {
            warn!(error = ?e, "Failed to load playbook definitions from DB");
        }
    }

    if let Some(store) = pb_def_store.clone() {
        let pb_runtime = Arc::clone(&playbook_runtime);
        tokio::spawn(async move {
            let mut last = store.change_marker("playbook").await.ok();
            let mut tick = tokio::time::interval(Duration::from_secs(10));
            tick.tick().await;
            loop {
                tick.tick().await;
                match store.change_marker("playbook").await {
                    Ok(cur) if Some(cur) != last => {
                        last = Some(cur);
                        match pb_runtime.reload().await {
                            Ok(count) => {
                                info!(count, "Reloaded playbook definitions from DB")
                            }
                            Err(e) => tracing::warn!("DB playbook reload failed: {e}"),
                        }
                    }
                    Ok(_) => {}
                    Err(e) => tracing::warn!("playbook def poll failed: {e}"),
                }
            }
        });
    } else {
        let pb_runtime = Arc::downgrade(&playbook_runtime);
        service
            .watches
            .lock()
            .extend(crate::fs_watcher::spawn_on_change(
                "playbook definitions",
                crate::fs_watcher::FsWatcherConfig {
                    paths: vec![playbooks_dir.to_string_lossy().to_string()],
                    patterns: vec!["*.toml".to_string()],
                    debounce_ms: 1000,
                    recursive: true,
                },
                move || {
                    pb_runtime.upgrade().map(|runtime| async move {
                        match runtime.reload().await {
                            Ok(count) => info!(count, "Hot-reloaded playbook definitions"),
                            Err(e) => {
                                tracing::warn!("Failed to hot-reload playbook definitions: {}", e)
                            }
                        }
                    })
                },
            ));
    }
}

#[async_trait::async_trait]
impl crate::hosts::DefinitionSurface for PlaybookRuntime {
    fn def_store(&self) -> Option<&Arc<WorkflowStore>> {
        PlaybookRuntime::def_store(self)
    }

    async fn definition_ownership(
        &self,
        bare_id: &str,
        owner: Option<&str>,
    ) -> Option<(Option<String>, Visibility)> {
        PlaybookRuntime::definition_ownership(self, bare_id, owner).await
    }

    async fn owners(&self) -> Vec<(String, Option<String>)> {
        self.list_definitions()
            .await
            .into_iter()
            .map(|d| (d.id, d.owner))
            .collect()
    }

    async fn reload(&self) -> eyre::Result<usize> {
        PlaybookRuntime::reload(self).await
    }
}

#[async_trait::async_trait]
impl crate::hosts::PlaybookHost for PlaybookRuntime {
    async fn resolve_for_user(
        &self,
        bare_id: &str,
        user: &str,
        teams: &dyn TeamMembership,
    ) -> PlaybookResolution {
        PlaybookRuntime::resolve_for_user(self, bare_id, user, teams).await
    }

    async fn list_definitions_visible(&self, listing: &DefListing) -> Vec<PlaybookSummary> {
        PlaybookRuntime::list_definitions_visible(self, listing).await
    }

    async fn handle_message(
        &self,
        svc: &Arc<AgentService>,
        session_key: &str,
        owner: &str,
        surfaces: &[Arc<dyn ChannelSurface>],
        channel: &str,
        thread_id: &Option<String>,
        text: &str,
    ) -> PlaybookOutcome {
        handle_message(
            svc,
            self,
            session_key,
            owner,
            surfaces,
            channel,
            thread_id,
            text,
        )
        .await
    }

    fn get_definition_source(&self, id: &str, owner: Option<&str>) -> eyre::Result<String> {
        PlaybookRuntime::get_definition_source(self, id, owner)
    }

    async fn save_definition_source(
        &self,
        id: &str,
        toml_content: &str,
        owner: Option<&str>,
        visibility: Visibility,
        edit: workforce_orchestrator::workflow_store::EditContext<'_>,
    ) -> eyre::Result<()> {
        PlaybookRuntime::save_definition_source(self, id, toml_content, owner, visibility, edit)
            .await
    }

    fn export_all_sources(&self) -> eyre::Result<HashMap<String, String>> {
        PlaybookRuntime::export_all_sources(self)
    }

    async fn delete_definition(
        &self,
        id: &str,
        owner: Option<&str>,
        edit: workforce_orchestrator::workflow_store::EditContext<'_>,
    ) -> eyre::Result<()> {
        PlaybookRuntime::delete_definition(self, id, owner, edit).await
    }

    async fn list_revisions(
        &self,
        id: &str,
        owner: Option<&str>,
    ) -> eyre::Result<Vec<workforce_orchestrator::workflow_store::DefRevision>> {
        PlaybookRuntime::list_revisions(self, id, owner).await
    }

    async fn get_revision(
        &self,
        id: &str,
        owner: Option<&str>,
        rev: i32,
    ) -> eyre::Result<Option<workforce_orchestrator::workflow_store::DefRevision>> {
        PlaybookRuntime::get_revision(self, id, owner, rev).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_orchestrator::playbook::PlaybookDefinition;

    fn sample_def() -> PlaybookDefinition {
        PlaybookDefinition::from_toml(
            "[playbook]\nid=\"p\"\nname=\"P\"\n\
             [[step]]\nid=\"design\"\ntitle=\"Design\"\nartifact=\"design.md\"\nprompt=\"Design it.\"\n",
        )
        .unwrap()
    }

    /// The headline regression: an artifact written by the REAL workspace-
    /// sandboxed write tool (using the workspace-relative path the step prompt
    /// hands the agent) must be readable by the controller's pin — i.e. NOT
    /// "(not written)". This is the integration the pure-logic tests missed.
    #[tokio::test]
    async fn artifact_round_trips_through_the_sandboxed_write_tool() {
        use workforce_agent::tools::filesystem::WriteFile;
        use workforce_agent::tools::Tool;

        // Stand-in for the agent workspace; canonicalised so the sandbox's
        // parent-dir check matches on macOS (/var → /private/var).
        let workspace = std::env::temp_dir().join(format!("pb-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let workspace = workspace.canonicalize().unwrap();

        // Controller (Start) pre-creates the workspace-relative scratch dir.
        let scratch_rel = "playbooks/sess-1";
        std::fs::create_dir_all(workspace.join(scratch_rel)).unwrap();

        // Agent writes the artifact via the real sandboxed tool — proves the
        // path is inside allowed_paths = [workspace].
        let writer = WriteFile::new(vec![workspace.clone()]);
        let result = writer
            .execute(serde_json::json!({
                "path": format!("{scratch_rel}/design.md"),
                "content": "# The Design\n\nDetails.",
            }))
            .await
            .unwrap();
        assert!(
            result.success,
            "agent write into scratch must be allowed: {result:?}"
        );

        // Controller pins it back with the real heading as synopsis.
        let def = sample_def();
        let progress = PlaybookProgress::new("p", "design", "n0", scratch_rel);
        let pinned = pin_artifact(&def, 0, &workspace.join(&progress.scratch)).unwrap();
        assert_eq!(pinned.synopsis, "The Design");

        let _ = std::fs::remove_dir_all(&workspace);
    }

    #[test]
    fn pin_missing_artifact_marks_not_written() {
        let workspace = std::env::temp_dir().join(format!("pb-{}", uuid::Uuid::new_v4()));
        let def = sample_def();
        let pinned = pin_artifact(&def, 0, &workspace).unwrap();
        assert_eq!(pinned.synopsis, "(not written)");
    }

    /// The step timer freezes at the first hand-back: a within-step follow-up
    /// turn must not fold the user's deliberation time into "done in X".
    #[test]
    fn freeze_work_ms_freezes_on_first_hand_back() {
        let mut card = PlaybookCard {
            channel: "c".into(),
            ts: "t".into(),
            thread_id: None,
            header: "Step 1/2: Design".into(),
            started_at_ms: 1_000,
            work_ms: None,
            model: None,
        };
        // First hand-back, +2s of work: freezes at 2s.
        assert_eq!(freeze_work_ms(&mut card, 3_000), 2_000);
        assert_eq!(card.work_ms, Some(2_000));
        // A follow-up turn 60s later must keep the original 2s, not 59s.
        assert_eq!(freeze_work_ms(&mut card, 62_000), 2_000);
        assert_eq!(card.work_ms, Some(2_000));
    }

    // ── repost_awaiting_card (controls follow the conversation) ──────────────
    use std::sync::atomic::{AtomicUsize, Ordering};
    use tokio::sync::Mutex;
    use workforce_common::channels::{ChannelCapabilities, Reaction, SentMessage};

    /// A test surface that records the messages it posts, the ids it deletes, and
    /// the ids it edits in place, handing back monotonic ids so a re-post is
    /// distinguishable from the card it replaces. `fail_send` forces every post to
    /// error, to exercise the edit-in-place fallback.
    #[derive(Default)]
    struct RecordingSurface {
        sent: Mutex<Vec<OutboundMessage>>,
        deleted: Mutex<Vec<String>>,
        updated: Mutex<Vec<String>>,
        next_id: AtomicUsize,
        fail_send: bool,
    }

    #[async_trait::async_trait]
    impl ChannelSurface for RecordingSurface {
        fn platform(&self) -> &str {
            "mock"
        }
        fn capabilities(&self) -> ChannelCapabilities {
            ChannelCapabilities::default()
        }
        async fn add_reaction(&self, _: &Reaction) -> eyre::Result<()> {
            Ok(())
        }
        async fn remove_reaction(&self, _: &Reaction) -> eyre::Result<()> {
            Ok(())
        }
        async fn send_message(&self, m: &OutboundMessage) -> eyre::Result<SentMessage> {
            if self.fail_send {
                return Err(eyre::eyre!("send failed"));
            }
            self.sent.lock().await.push(m.clone());
            let n = self.next_id.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(SentMessage {
                channel: m.channel.clone(),
                message_id: format!("m{n}"),
                thread_id: m.thread_id.clone(),
                platform: "mock".into(),
            })
        }
        async fn update_message(
            &self,
            _channel: &str,
            message_id: &str,
            _outbound: &OutboundMessage,
        ) -> eyre::Result<()> {
            self.updated.lock().await.push(message_id.to_string());
            Ok(())
        }
        async fn delete_message(&self, _channel: &str, message_id: &str) -> eyre::Result<()> {
            self.deleted.lock().await.push(message_id.to_string());
            Ok(())
        }
    }

    fn awaiting_card(thread: Option<&str>) -> PlaybookCard {
        PlaybookCard {
            channel: "C1".into(),
            ts: "old".into(),
            thread_id: thread.map(str::to_string),
            header: "Step 1/2: Design".into(),
            started_at_ms: 0,
            work_ms: Some(6_000),
            model: None,
        }
    }

    /// On hand-back the controls move to the foot of the thread: a fresh card
    /// carrying the buttons is posted into the same thread and the old card is
    /// deleted, so the user clicks Continue under the latest message.
    #[tokio::test]
    async fn repost_awaiting_card_moves_controls_to_thread_foot() {
        let surface = Arc::new(RecordingSurface::default());
        let surfaces: Vec<Arc<dyn ChannelSurface>> = vec![surface.clone()];
        let rows = crate::slack_blocks::playbook_control_buttons("k|n|true");

        let new_ts = repost_awaiting_card(
            &surfaces,
            &awaiting_card(Some("T1")),
            "✅ done — your move",
            rows,
        )
        .await;

        assert_eq!(new_ts.as_deref(), Some("m1"));
        let sent = surface.sent.lock().await;
        assert_eq!(sent.len(), 1, "exactly one fresh card is posted");
        assert_eq!(
            sent[0].thread_id.as_deref(),
            Some("T1"),
            "re-posted into the same thread"
        );
        let blocks = sent[0]
            .blocks
            .as_ref()
            .expect("card carries Block Kit blocks");
        assert!(
            blocks
                .iter()
                .any(|b| b.get("type").and_then(|t| t.as_str()) == Some("actions")),
            "the re-posted card carries the controls"
        );
        assert_eq!(*surface.deleted.lock().await, vec!["old".to_string()]);
        assert!(surface.updated.lock().await.is_empty());
    }

    /// When every surface refuses the fresh post, the controls fall back to an
    /// in-place edit of the existing card — never lost — and the tracked id is
    /// left for the caller to keep unchanged (`None`).
    #[tokio::test]
    async fn repost_awaiting_card_falls_back_to_edit_when_post_fails() {
        let surface = Arc::new(RecordingSurface {
            fail_send: true,
            ..Default::default()
        });
        let surfaces: Vec<Arc<dyn ChannelSurface>> = vec![surface.clone()];
        let rows = crate::slack_blocks::playbook_control_buttons("k|n|true");

        let new_ts =
            repost_awaiting_card(&surfaces, &awaiting_card(None), "✅ done — your move", rows)
                .await;

        assert!(new_ts.is_none());
        assert_eq!(*surface.updated.lock().await, vec!["old".to_string()]);
        assert!(surface.deleted.lock().await.is_empty());
        assert!(surface.sent.lock().await.is_empty());
    }

    fn cards_with(card: PlaybookCard) -> HashMap<String, PlaybookCard> {
        HashMap::from([("s".to_string(), card)])
    }

    /// The hand-back claims the card only while it is still the one snapshotted
    /// before the runner was released; a mismatch (a concurrent advance swapped it)
    /// claims nothing and — crucially — does not freeze the live card's timer.
    #[test]
    fn claim_card_for_handback_guards_on_expected_ts() {
        // Match: claims a clone and freezes the duration.
        let mut cards = cards_with(awaiting_card(Some("T1")));
        let claimed = claim_card_for_handback(&mut cards, "s", Some("old"), 9_000);
        assert!(matches!(claimed, Some((c, ms)) if c.ts == "old" && ms == 6_000));

        // Mismatch: a fresh (unfrozen) card is left untouched — no freeze, no claim.
        let mut fresh = awaiting_card(None);
        fresh.work_ms = None;
        let mut cards = cards_with(fresh);
        assert!(claim_card_for_handback(&mut cards, "s", Some("T2"), 9_000).is_none());
        assert_eq!(cards["s"].work_ms, None);

        // No expected id, or no such session: nothing to claim.
        let mut cards = cards_with(awaiting_card(Some("T1")));
        assert!(claim_card_for_handback(&mut cards, "s", None, 9_000).is_none());
        assert!(claim_card_for_handback(&mut cards, "missing", Some("old"), 9_000).is_none());
    }

    /// Adopting the re-post repoints the tracked id only when it still points at
    /// the card we moved; a concurrent advance (different id, or gone) makes the
    /// re-post an orphan to delete and leaves the tracked card alone.
    #[test]
    fn adopt_reposted_card_orphans_on_concurrent_advance() {
        // Still ours: adopt — repoint to the new id, no orphan.
        let mut cards = cards_with(awaiting_card(Some("T1")));
        assert_eq!(
            adopt_reposted_card(&mut cards, "s", "old", "new".into()),
            None
        );
        assert_eq!(cards["s"].ts, "new");

        // Advanced to a different card: don't touch it; the re-post is an orphan.
        let mut advanced = awaiting_card(Some("T1"));
        advanced.ts = "next-step".into();
        let mut cards = cards_with(advanced);
        assert_eq!(
            adopt_reposted_card(&mut cards, "s", "old", "new".into()),
            Some("new".to_string())
        );
        assert_eq!(cards["s"].ts, "next-step");

        // Card gone (finalized): the re-post is an orphan.
        let mut cards: HashMap<String, PlaybookCard> = HashMap::new();
        assert_eq!(
            adopt_reposted_card(&mut cards, "s", "old", "new".into()),
            Some("new".to_string())
        );
    }

    /// Ordinary chat must never be intercepted — only explicit prefix/button
    /// directives. This guards the path every message takes.
    #[test]
    fn parse_directive_leaves_normal_chat_alone() {
        assert!(parse_directive("continue with that approach, but reconsider X").is_none());
        assert!(parse_directive("let's exit the building").is_none());
        assert!(parse_directive("hello there").is_none());
        assert!(matches!(
            parse_directive("cmd: continue"),
            Some(Directive::Continue(None))
        ));
        assert!(matches!(
            parse_directive("cmd: continue abc123"),
            Some(Directive::Continue(Some(_)))
        ));
        assert!(matches!(
            parse_directive("/exit"),
            Some(Directive::Exit(None))
        ));
        assert!(matches!(
            parse_directive("cmd: playbook start design-impl-review"),
            Some(Directive::Start(_))
        ));
        // Missing id is not a crash and not a directive.
        assert!(parse_directive("cmd: playbook start").is_none());
        // Goto carries nonce + target; a missing target is not a directive.
        assert!(matches!(
            parse_directive("cmd: goto abc123 merge"),
            Some(Directive::Goto { .. })
        ));
        assert!(parse_directive("cmd: goto abc123").is_none());
    }

    // ── resolve_for_user (interactive owner resolution) ──────────────────────
    use workforce_core::access::{NoTeams, Visibility};

    use super::testing::{owned, runtime_with};

    #[tokio::test]
    async fn resolve_prefers_own_playbook_over_global() {
        let rt = runtime_with(vec![
            owned("deploy", Some("alice"), Visibility::Private),
            owned("deploy", None, Visibility::Global),
        ])
        .await;
        let def = rt
            .resolve_for_user("deploy", "alice", &NoTeams)
            .await
            .found()
            .unwrap();
        assert_eq!(def.playbook.name, "deploy-alice");
    }

    #[tokio::test]
    async fn resolve_falls_back_to_global_when_user_has_none() {
        let rt = runtime_with(vec![
            owned("deploy", Some("alice"), Visibility::Private),
            owned("deploy", None, Visibility::Global),
        ])
        .await;
        let def = rt
            .resolve_for_user("deploy", "bob", &NoTeams)
            .await
            .found()
            .unwrap();
        assert_eq!(def.playbook.name, "deploy-sys");
    }

    #[tokio::test]
    async fn resolve_finds_a_single_member_published_global() {
        let rt = runtime_with(vec![owned("deploy", Some("carol"), Visibility::Global)]).await;
        let def = rt
            .resolve_for_user("deploy", "alice", &NoTeams)
            .await
            .found()
            .unwrap();
        assert_eq!(def.playbook.name, "deploy-carol");
    }

    #[tokio::test]
    async fn resolve_is_ambiguous_across_two_member_globals() {
        let rt = runtime_with(vec![
            owned("deploy", Some("carol"), Visibility::Global),
            owned("deploy", Some("dave"), Visibility::Global),
        ])
        .await;
        assert!(matches!(
            rt.resolve_for_user("deploy", "alice", &NoTeams).await,
            PlaybookResolution::Ambiguous
        ));
    }

    /// A teammate's team-visible playbook is startable by its bare id, exactly
    /// as a published one is; a stranger's is not.
    #[tokio::test]
    async fn resolve_reaches_a_teammates_team_visible_playbook() {
        struct AliceTeam;
        impl workforce_core::access::TeamMembership for AliceTeam {
            fn in_team(&self, _team_id: &str) -> bool {
                false
            }
            fn shares_team_with(&self, owner: &str) -> bool {
                owner == "alice"
            }
        }
        let rt = runtime_with(vec![
            owned("deploy", Some("alice"), Visibility::Team),
            owned("secret", Some("carol"), Visibility::Team),
        ])
        .await;
        assert_eq!(
            rt.resolve_for_user("deploy", "bob", &AliceTeam)
                .await
                .found()
                .unwrap()
                .playbook
                .name,
            "deploy-alice"
        );
        assert!(matches!(
            rt.resolve_for_user("deploy", "bob", &NoTeams).await,
            PlaybookResolution::NotFound
        ));
        assert!(matches!(
            rt.resolve_for_user("secret", "bob", &AliceTeam).await,
            PlaybookResolution::NotFound
        ));
    }

    #[tokio::test]
    async fn resolve_not_found_for_unknown_id() {
        let rt = runtime_with(vec![]).await;
        assert!(matches!(
            rt.resolve_for_user("nope", "alice", &NoTeams).await,
            PlaybookResolution::NotFound
        ));
    }

    #[tokio::test]
    async fn resolve_does_not_leak_another_members_private_playbook() {
        let rt = runtime_with(vec![owned("secret", Some("alice"), Visibility::Private)]).await;
        // A private playbook is invisible to everyone but its owner …
        assert!(matches!(
            rt.resolve_for_user("secret", "bob", &NoTeams).await,
            PlaybookResolution::NotFound
        ));
        // … and still resolvable by the owner.
        assert!(rt
            .resolve_for_user("secret", "alice", &NoTeams)
            .await
            .found()
            .is_some());
    }

    #[tokio::test]
    async fn list_visible_hides_other_members_private_playbooks() {
        let rt = runtime_with(vec![
            owned("g", None, Visibility::Global),
            owned("mine", Some("alice"), Visibility::Private),
            owned("theirs", Some("bob"), Visibility::Private),
        ])
        .await;
        // Alice (member, not admin): global + her own, never bob's private.
        let ids: Vec<_> = rt
            .list_definitions_visible(&DefListing::local(Some("alice"), false))
            .await
            .into_iter()
            .map(|p| p.id)
            .collect();
        assert!(ids.contains(&"g".to_string()));
        assert!(ids.contains(&"mine".to_string()));
        assert!(!ids.contains(&"theirs".to_string()));
        // Admin sees everything.
        assert_eq!(
            rt.list_definitions_visible(&DefListing::local(None, true))
                .await
                .len(),
            3
        );
    }

    #[tokio::test]
    async fn definition_ownership_addresses_by_owner() {
        let rt = runtime_with(vec![
            owned("p", Some("alice"), Visibility::Private),
            owned("p", None, Visibility::Global),
        ])
        .await;
        assert_eq!(
            rt.definition_ownership("p", Some("alice")).await,
            Some((Some("alice".to_string()), Visibility::Private))
        );
        assert_eq!(
            rt.definition_ownership("p", None).await,
            Some((None, Visibility::Global))
        );
        assert_eq!(rt.definition_ownership("p", Some("zzz")).await, None);
    }

    #[tokio::test]
    async fn member_save_without_db_store_is_refused() {
        // A filesystem-only runtime must reject a member-owned save rather than
        // silently writing it as a global system playbook (the flat {id}.toml
        // disk namespace can't carry owner/visibility).
        let dir = std::env::temp_dir().join(format!("pb-nostore-{}", uuid::Uuid::new_v4()));
        let rt = PlaybookRuntime::new(dir);
        let toml = "[playbook]\nid=\"x\"\nname=\"X\"\n\
                    [[step]]\nid=\"s\"\ntitle=\"S\"\nprompt=\"go\"\n";
        let edit = workforce_orchestrator::workflow_store::EditContext::new("web", Some("alice"));
        let err = rt
            .save_definition_source("x", toml, Some("alice"), Visibility::Private, edit)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("database store"));
        // A system save (owner=None) still works on disk.
        assert!(rt
            .save_definition_source("x", toml, None, Visibility::Global, edit)
            .await
            .is_ok());
        let _ = std::fs::remove_dir_all(&rt.dir);
    }

    #[test]
    fn validate_step_model_accepts_tiers_and_concrete_rejects_unknown_providers() {
        // Bare tiers, provider-routed tiers, and concrete model ids are allowed.
        for ok in [
            "powerful",
            "cheap",
            "anthropic:powerful",
            "openrouter:anthropic:powerful",
            "claude-sonnet-4-6",
            "grok:grok-4",
        ] {
            assert!(validate_step_model(ok).is_ok(), "{ok} should be valid");
        }
        // Empty/blank, an unknown provider, and a bare-openrouter tier are
        // rejected — the cases a global playbook author could smuggle in.
        for bad in [
            "",
            "   ",
            "evil:powerful",
            "openrouter:powerful",
            ":powerful",
            // Trailing whitespace must not slip a rejected spec past the check:
            // it is trimmed before tokenizing, so it resolves identically.
            "evil:powerful ",
            "openrouter:powerful ",
        ] {
            assert!(
                validate_step_model(bad).is_err(),
                "{bad:?} should be rejected"
            );
        }
    }

    #[test]
    fn status_with_model_appends_only_when_present() {
        assert_eq!(status_with_model("⏱ working…", None), "⏱ working…");
        assert_eq!(
            status_with_model("⏱ working…", Some("claude-opus-4-7")),
            "⏱ working… · claude-opus-4-7"
        );
    }

    #[tokio::test]
    async fn save_definition_source_rejects_invalid_step_model() {
        let dir = std::env::temp_dir().join(format!("pb-model-{}", uuid::Uuid::new_v4()));
        let rt = PlaybookRuntime::new(dir);
        let edit = workforce_orchestrator::workflow_store::EditContext::new("web", None);
        let bad = "[playbook]\nid=\"x\"\nname=\"X\"\n\
                   [[step]]\nid=\"s\"\ntitle=\"S\"\nprompt=\"go\"\nmodel=\"evil:powerful\"\n";
        let err = rt
            .save_definition_source("x", bad, None, Visibility::Global, edit)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("step 's'"), "{err}");
        // A valid tier saves fine.
        let good = "[playbook]\nid=\"x\"\nname=\"X\"\n\
                    [[step]]\nid=\"s\"\ntitle=\"S\"\nprompt=\"go\"\nmodel=\"powerful\"\n";
        assert!(rt
            .save_definition_source("x", good, None, Visibility::Global, edit)
            .await
            .is_ok());
        let _ = std::fs::remove_dir_all(&rt.dir);
    }
}
