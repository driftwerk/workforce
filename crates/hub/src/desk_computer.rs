//! The computer gate on a hub with `[desk]`: what each local agent may read,
//! change or run on the person's computer, decided per call by Desk's
//! verdict ([`workforce_desk::computer::policy::verdict`]) and recorded in
//! its activity. Installed by [`install`] when Desk starts; a hub without
//! `[desk]` installs nothing and every call is decided as before.
//!
//! The computer namespaces (`filesystem`, `code`, `exec`, `desk`, `coding`,
//! `claude_code`, `wasm_skill`, `attachments`) are decided, and so are the
//! connectors Desk lists (`CONNECTORS`): a call there is a read or a write
//! by its tool name, refused when the agent's `[skills]` turns the
//! connector off or makes it read only, and asked about when it sends to
//! people. Every other namespace, the other built-in adapters included,
//! never asks.
//!
//! An agent with no `[computer]` is Full access, asking first; its own
//! folder is the roots its tools already have (`tool_roots`).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde_json::json;
use tracing::warn;
use workforce_agent::runner::{CURRENT_SESSION_KEY, CURRENT_USER_ID};
use workforce_agent::tools::computer::Decision;
use workforce_agent::tools::computer::{describe, ComputerGate, Outcome, Record};
use workforce_common::settings::{AgentWorkspaceConfig, WorkforceConfig};
use workforce_core::computer::{By, ComputerAccess, Effect, Refusal, Refused, Verdict};
use workforce_core::policy::PermissionChoice;
use workforce_desk::computer::policy::{namespace, shown, Namespace};
use workforce_desk::computer::{Activity, AgentPolicy, CallFacts, Computer, NewGrant};

/// How long "Allow for this session" lasts at most.
pub const SESSION_HOURS: i64 = 12;

use crate::serve::ComputerSubject;

/// One agent's gate, as its turn found its settings.
pub struct HubComputerGate {
    computer: Computer,
    policy: AgentPolicy,
}

/// Where the person's home is.
fn home() -> PathBuf {
    std::env::var_os("HOME")
        .map(PathBuf::from)
        .map(|h| workforce_agent::tools::paths::resolve_real(&h))
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// This agent's `[computer]`. One that can't be read keeps the default,
/// Full access asking first: admission refuses a bad table before it is
/// written, so only a hand edit gets here.
fn access_of(subject: &ComputerSubject) -> ComputerAccess {
    // Read once per change of the file: a gate is built per turn and per
    // call outside one.
    static READ: std::sync::LazyLock<
        parking_lot::Mutex<
            std::collections::HashMap<PathBuf, (Option<std::time::SystemTime>, ComputerAccess)>,
        >,
    > = std::sync::LazyLock::new(Default::default);
    let file = subject
        .workspace
        .join(workforce_common::config_files::WORKSPACE_CONFIG);
    let at = std::fs::metadata(&file).and_then(|m| m.modified()).ok();
    if let Some((seen, access)) = READ.lock().get(&subject.workspace) {
        if *seen == at && at.is_some() {
            return *access;
        }
    }
    let access = read_access(subject);
    READ.lock().insert(subject.workspace.clone(), (at, access));
    access
}

fn read_access(subject: &ComputerSubject) -> ComputerAccess {
    match AgentWorkspaceConfig::load(&subject.workspace) {
        Ok(Some(config)) => config.computer().unwrap_or_else(|e| {
            warn!(agent = %subject.agent, error = %e, "[computer] unreadable; asking first");
            ComputerAccess::default()
        }),
        _ => ComputerAccess::default(),
    }
}

/// The folders this agent works in (`[computer] works_in`), real, each one
/// an agent may work in on this machine: one admission would refuse (a hand
/// edit, or a home that moved) is left out, never widened to.
fn works_in_of(subject: &ComputerSubject, config: &WorkforceConfig, home: &Path) -> Vec<PathBuf> {
    let folders = match AgentWorkspaceConfig::load(&subject.workspace) {
        Ok(Some(c)) => c.works_in().unwrap_or_else(|e| {
            warn!(agent = %subject.agent, error = %e, "[computer] works_in unreadable; none used");
            Vec::new()
        }),
        _ => Vec::new(),
    };
    let withheld = withheld(config, home);
    folders
        .into_iter()
        .filter(|folder| {
            match workforce_common::computer_access::works_in_refusal(folder, home, &withheld) {
                Some(why) => {
                    warn!(agent = %subject.agent, folder = %folder.display(), why, "a folder it works in is left out");
                    false
                }
                None => true,
            }
        })
        .map(|folder| workforce_agent::tools::paths::resolve_real(&folder))
        .collect()
}

/// This agent's `[skills]` binding, as the verdict reads it: `None` (every
/// connector) where it names no `enabled` nor `read_only`. One that can't be
/// read turns every connector off: admission refuses it before it is written.
fn connectors_of(subject: &ComputerSubject) -> Option<workforce_desk::computer::Connectors> {
    let binding = match AgentWorkspaceConfig::load(&subject.workspace) {
        Ok(Some(config)) => config.connectors().unwrap_or_else(|e| {
            warn!(agent = %subject.agent, error = %e, "[skills] unreadable; its connectors are off");
            workforce_common::connector_binding::ConnectorBinding {
                enabled: Some(Vec::new()),
                ..Default::default()
            }
        }),
        _ => return None,
    };
    if binding.enabled.is_none() && binding.read_only.is_empty() {
        return None;
    }
    Some(workforce_desk::computer::Connectors {
        enabled: binding.enabled.unwrap_or_else(|| {
            workforce_common::connector_binding::CONNECTORS
                .iter()
                .map(|c| c.to_string())
                .collect()
        }),
        read_only: binding.read_only,
    })
}

/// What no agent reaches on this computer: Desk's data (the hub's data, its
/// Postgres, vault, memory and logs, Desk's own secrets and the PTY holder's
/// socket, every other agent's folder) and the keychains. On Desk the hub's
/// data is `…/Desk/hub/data`, so Desk's folder is two up; elsewhere the
/// hub's data folder alone. An agent's own folder is carved out by the
/// verdict.
fn withheld(config: &WorkforceConfig, home: &Path) -> Vec<PathBuf> {
    let data = workforce_agent::tools::paths::resolve_real(Path::new(&config.hub.data_dir));
    let mut out = vec![crate::desk_folder::of(config).unwrap_or(data)];
    out.push(home.join("Library/Keychains"));
    out
}

/// Where a boss's answers for the sessions it runs are judged: the same
/// home, scratch space and withheld set (Desk's data, every agent's
/// settings in it, the keychains) as the agents' own calls.
pub fn boss_bounds(config: &WorkforceConfig) -> workforce_desk::charge::Bounds {
    let home = home();
    workforce_desk::charge::Bounds {
        withheld: withheld(config, &home),
        scratch: scratch(),
        home,
    }
}

/// Scratch space a delete may clear as freely as its own folder: `/tmp` and
/// `$TMPDIR`, real.
fn scratch() -> Vec<PathBuf> {
    let mut named = vec![PathBuf::from("/tmp")];
    if let Some(t) = std::env::var_os("TMPDIR").filter(|t| !t.is_empty()) {
        named.push(PathBuf::from(t));
    }
    scratch_of(&named)
}

/// `named`, as written (a command names `/tmp`) and real (a tool resolves
/// it to `/private/tmp` on macOS).
fn scratch_of(named: &[PathBuf]) -> Vec<PathBuf> {
    let mut out = Vec::new();
    for n in named {
        for p in [n.clone(), workforce_agent::tools::paths::resolve_real(n)] {
            if !out.contains(&p) {
                out.push(p);
            }
        }
    }
    out
}

/// An agent's own config files and folders, which no call changes.
pub(crate) fn own_config(workspace: &Path) -> Vec<PathBuf> {
    let root = workforce_agent::tools::paths::resolve_real(workspace);
    workforce_common::config_files::agent_config_files()
        .chain(workforce_common::config_files::AGENT_CONFIG_DIRS)
        .map(|name| root.join(name))
        .collect()
}

impl HubComputerGate {
    pub fn for_agent(
        computer: Computer,
        config: &WorkforceConfig,
        subject: &ComputerSubject,
    ) -> Self {
        Self::for_agent_at(computer, config, subject, home())
    }

    /// [`Self::for_agent`] on a machine whose home is `home`.
    fn for_agent_at(
        computer: Computer,
        config: &WorkforceConfig,
        subject: &ComputerSubject,
        home: PathBuf,
    ) -> Self {
        let own = crate::serve::tool_roots(config, &subject.workspace)
            .iter()
            .map(|root| workforce_agent::tools::paths::resolve_real(root))
            .collect();
        Self {
            computer,
            policy: AgentPolicy {
                hub: "local".into(),
                agent: subject.agent.clone(),
                name: subject
                    .name
                    .clone()
                    .unwrap_or_else(|| subject.agent.clone()),
                access: access_of(subject),
                own,
                works_in: works_in_of(subject, config, &home),
                withheld: withheld(config, &home),
                config: own_config(&subject.workspace),
                scratch: scratch(),
                home,
                // Commands can't be contained yet (AC5).
                contained: false,
                connectors: connectors_of(subject),
            },
        }
    }

    fn facts(&self, tool: &str, effect: &Effect) -> CallFacts {
        let local = |key: &'static tokio::task::LocalKey<String>| {
            key.try_with(|v| v.clone()).ok().filter(|v| !v.is_empty())
        };
        CallFacts {
            tool: tool.to_string(),
            effect: effect.clone(),
            person: local(&CURRENT_USER_ID),
            session_id: local(&CURRENT_SESSION_KEY),
            run: None,
            folder: folder_for(effect, &self.policy.home),
        }
    }

    /// Reaches Postgres from the synchronous gate, or says why it can't.
    fn block_on<T>(
        &self,
        fut: impl std::future::Future<Output = eyre::Result<T>>,
    ) -> eyre::Result<T> {
        let handle = tokio::runtime::Handle::try_current()
            .map_err(|_| eyre::eyre!("no runtime to reach Desk's store"))?;
        if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
            eyre::bail!("Desk's store is reached from the hub's multi-thread runtime only");
        }
        tokio::task::block_in_place(|| handle.block_on(fut))
    }

    /// The answers and facts the card for `effect` shows, given its verdict.
    fn card(
        &self,
        effect: &Effect,
        verdict: &Verdict,
    ) -> workforce_core::policy::PermissionPromptOptions {
        use workforce_core::computer::{Always, Choice};
        let mut options = workforce_agent::tools::computer::once_or_deny();
        let home = &self.policy.home;
        let (choices, why) = match verdict {
            Verdict::Ask { choices, why } => (choices.as_slice(), why.sentence()),
            _ => (&[][..], "Asks first"),
        };
        let always = choices.iter().find_map(|c| match c {
            Choice::Always(Always::Folder(f)) => Some(shown(f, home)),
            Choice::Always(Always::Connector(c)) => Some(c.clone()),
            _ => None,
        });
        // A session allowance is scoped to the same folder an "always" would
        // be: where no folder can be offered, neither can it.
        options.allow_session =
            always.is_some() && choices.iter().any(|c| matches!(c, Choice::AllowSession));
        options.allow_in_folder = always;
        options.facts = Some(Box::new(facts_of(effect, home, why)));
        options
    }
}

/// What a card shows of `effect`, every path as the person reads it.
fn facts_of(effect: &Effect, home: &Path, why: &str) -> workforce_core::computer::ComputerFacts {
    let list = |paths: &[PathBuf]| paths.iter().map(|p| shown(p, home)).collect::<Vec<_>>();
    let mut facts = workforce_core::computer::ComputerFacts {
        why: why.to_string(),
        ..Default::default()
    };
    match effect {
        Effect::Read { paths } => (facts.effect, facts.paths) = ("read".into(), list(paths)),
        Effect::Write { paths } => (facts.effect, facts.paths) = ("write".into(), list(paths)),
        Effect::Delete { paths } => (facts.effect, facts.paths) = ("delete".into(), list(paths)),
        Effect::Move { from, to } => {
            facts.effect = "write".into();
            facts.paths = list(from);
            facts.paths.push(shown(to, home));
        }
        Effect::Exec { command, cwd } => {
            facts.effect = "exec".into();
            facts.command = Some(command.clone());
            facts.cwd = cwd.as_ref().map(|c| shown(c, home));
        }
        Effect::Session { cwd } => {
            facts.effect = "session".into();
            facts.cwd = cwd.as_ref().map(|c| shown(c, home));
        }
        Effect::Connector { connector, .. } => {
            facts.effect = "connector".into();
            facts.connector = Some(connector.clone());
        }
        Effect::None | Effect::Unknown => facts.effect = "unknown".into(),
    }
    facts
}

/// The folder an "always" answer to `effect` would cover: the git work tree
/// holding its target, else the target's folder (a command's or session's
/// cwd itself). Never above home's own level: the verdict refuses to offer
/// home, the root or a volume.
fn folder_for(effect: &Effect, home: &Path) -> Option<PathBuf> {
    let start = match effect {
        Effect::Write { paths } | Effect::Delete { paths } | Effect::Read { paths } => {
            paths.first()?.parent()?.to_path_buf()
        }
        Effect::Move { to, .. } => to.parent()?.to_path_buf(),
        Effect::Exec { cwd, .. } | Effect::Session { cwd } => cwd.clone()?,
        _ => return None,
    };
    let mut at = start.as_path();
    loop {
        if at.join(".git").exists() {
            return Some(at.to_path_buf());
        }
        match at.parent() {
            Some(up) if at != home && up != Path::new("/") => at = up,
            _ => return Some(start),
        }
    }
}

impl HubComputerGate {
    fn unreachable(&self) -> Verdict {
        Verdict::Refuse {
            refusal: Refusal {
                rule: Refused::Stopped,
                sentence: format!(
                    "Desk couldn't check what {} may do on this computer, so nothing ran.",
                    self.policy.name
                ),
            },
        }
    }

    fn activity(&self, record: &Record) -> Activity {
        let home = &self.policy.home;
        let connector = match &record.decision.effect {
            Effect::Connector { connector, .. } => workforce_common::connector_binding::CONNECTORS
                .iter()
                .copied()
                .find(|c| c == connector),
            _ => None,
        };
        let (kind, folder) = match &record.decision.effect {
            Effect::Read { paths } => ("read", paths.first().cloned()),
            Effect::Write { paths } => ("write", paths.first().cloned()),
            Effect::Delete { paths } => ("delete", paths.first().cloned()),
            Effect::Move { to, .. } => ("write", Some(to.clone())),
            Effect::Exec { cwd, .. } => ("exec", cwd.clone()),
            Effect::Session { cwd } => ("session", cwd.clone()),
            Effect::Connector { .. } => ("connector", None),
            Effect::None | Effect::Unknown => ("write", None),
        };
        let decided_by = match (&record.decision.verdict, record.outcome) {
            (Verdict::Run { by: By::Grant(_) }, _) => "grant",
            (Verdict::Run { by: By::Session(_) }, _) => "session",
            (_, Outcome::Allowed | Outcome::Declined | Outcome::NotAnswered) => "person",
            _ => "rule",
        };
        let session = CURRENT_SESSION_KEY
            .try_with(|k| k.clone())
            .unwrap_or_default();
        // Never the call's input (a file's contents, a long body): its tool,
        // the paths it names and a short line, secrets redacted.
        let doing = capped(&workforce_common::redact::redact_secrets(
            &describe(&record.decision.effect).replace(&home.display().to_string(), "~"),
        ));
        let targets: Vec<String> = facts_of(&record.decision.effect, home, "").paths;
        let arguments = json!({
            "tool": record.tool,
            "targets": targets.iter().take(MAX_TARGETS).collect::<Vec<_>>(),
            "summary": doing,
        });
        let device = record.request_id.as_deref().and_then(answered_by);
        Activity {
            hub: self.policy.hub.clone(),
            agent: self.policy.agent.clone(),
            person: CURRENT_USER_ID
                .try_with(|u| u.clone())
                .ok()
                .filter(|u| !u.is_empty()),
            session_id: Some(session).filter(|s| !s.is_empty()),
            request_id: record.request_id.clone(),
            kind: kind.into(),
            doing,
            folder: folder.map(|f| shown(&f, home)),
            connector: connector.map(str::to_string),
            tool: record.tool.clone(),
            vault_source: connector
                .and_then(|c| crate::connector_pins::source_of(&self.policy.agent, c))
                .map(|s| s.label().to_string()),
            decided_by: decided_by.into(),
            outcome: json!({
                "outcome": record.outcome.as_str(),
                "verdict": record.decision.verdict,
            }),
            reason: record.reason.clone(),
            exit_code: None,
            duration_ms: None,
            arguments,
            device,
        }
    }
}

/// The longest an activity row's line is kept.
const MAX_DOING: usize = 300;
/// The most targets an activity row names.
const MAX_TARGETS: usize = 20;

fn capped(line: &str) -> String {
    match line.char_indices().nth(MAX_DOING) {
        Some((at, _)) => format!("{}…", &line[..at]),
        None => line.to_string(),
    }
}

/// Which paired device answered each open prompt on this hub, as the answer
/// came in: the activity row says it.
static ANSWERED: std::sync::LazyLock<
    parking_lot::Mutex<std::collections::HashMap<String, String>>,
> = std::sync::LazyLock::new(Default::default);

/// Notes that `device` answered `request_id`.
pub fn answered_from(request_id: &str, device: Option<&str>) {
    let Some(device) = device.filter(|d| !d.is_empty()) else {
        return;
    };
    let mut answered = ANSWERED.lock();
    // Answers are kept only until their row is written; a bound keeps a
    // flood of unmatched ones from growing it.
    if answered.len() > 1024 {
        answered.clear();
    }
    answered.insert(request_id.to_string(), device.to_string());
}

fn answered_by(request_id: &str) -> Option<String> {
    ANSWERED.lock().remove(request_id)
}

/// Prunes access activity older than its 90 days, at start and daily after.
pub fn prune_daily(computer: Computer, cancel: tokio_util::sync::CancellationToken) {
    tokio::spawn(async move {
        loop {
            let before = chrono::Utc::now()
                - chrono::Duration::days(workforce_desk::computer::ACTIVITY_KEPT_DAYS);
            match computer.store().prune_access(before).await {
                Ok(n) if n > 0 => tracing::info!(pruned = n, "access activity pruned"),
                Ok(_) => {}
                Err(e) => warn!(error = %format!("{e:#}"), "pruning access activity failed"),
            }
            tokio::select! {
                _ = cancel.cancelled() => return,
                _ = tokio::time::sleep(std::time::Duration::from_secs(24 * 60 * 60)) => {}
            }
        }
    });
}

impl ComputerGate for HubComputerGate {
    fn decides(&self, tool: &str) -> bool {
        match namespace(tool) {
            Some(Namespace::Computer) => true,
            Some(Namespace::Connector) => {
                workforce_common::connector_binding::connector_of(tool).is_some()
            }
            None => false,
        }
    }

    fn effect_of(&self, tool: &str, effect: Effect) -> Effect {
        match (
            effect,
            workforce_common::connector_binding::connector_of(tool),
        ) {
            (Effect::Unknown, Some(connector)) => Effect::Connector {
                connector: connector.to_string(),
                write: !workforce_common::connector_binding::is_read(tool),
            },
            (effect, _) => effect,
        }
    }

    fn reach(&self, effect: &Effect) -> Option<workforce_agent::tools::paths::Reach> {
        use workforce_core::computer::Access;
        let level = self.policy.access.access;
        let reaches = match effect {
            Effect::Read { .. } => level != Access::OwnFolder,
            Effect::Write { .. } | Effect::Delete { .. } | Effect::Move { .. } => {
                level == Access::Full
            }
            _ => false,
        };
        if reaches {
            return Some(workforce_agent::tools::paths::Reach {
                withheld: self.policy.withheld.clone(),
                only: Vec::new(),
            });
        }
        // The folders it works in, at every level, for what touches them:
        // the verdict has already held the call to its level.
        let touches = matches!(
            effect,
            Effect::Read { .. }
                | Effect::Write { .. }
                | Effect::Delete { .. }
                | Effect::Move { .. }
                | Effect::Exec { .. }
                | Effect::Session { .. }
        );
        (touches && !self.policy.works_in.is_empty()).then(|| {
            workforce_agent::tools::paths::Reach {
                withheld: self.policy.withheld.clone(),
                only: self.policy.works_in.clone(),
            }
        })
    }

    fn prompt(&self, decision: &Decision) -> workforce_core::policy::PermissionPromptOptions {
        self.card(&decision.effect, &decision.verdict)
    }

    fn keep(&self, decision: &Decision, choice: PermissionChoice) -> eyre::Result<()> {
        use workforce_core::computer::Ability;
        // Only what the card offered is kept: a client can send any answer.
        let card = self.card(&decision.effect, &decision.verdict);
        let offered = match choice {
            PermissionChoice::AllowSession => card.allow_session,
            PermissionChoice::AllowInFolder => card.allow_in_folder.is_some(),
            _ => false,
        };
        if !offered {
            eyre::bail!("{choice:?} wasn't offered for this call");
        }
        // The folder kept is the one the card showed, never found again.
        let shown_folder = match &decision.verdict {
            Verdict::Ask { choices, .. } => choices.iter().find_map(|c| match c {
                workforce_core::computer::Choice::Always(
                    workforce_core::computer::Always::Folder(f),
                ) => Some(f.clone()),
                _ => None,
            }),
            _ => None,
        };
        let deleting = matches!(
            &decision.verdict,
            Verdict::Ask {
                why: workforce_core::computer::Why::CantBeUndone,
                ..
            }
        );
        let facts = self.facts("", &decision.effect);
        let person = facts
            .person
            .clone()
            .ok_or_else(|| eyre::eyre!("nobody to keep it for"))?;
        let (abilities, connector): (Vec<Ability>, Option<String>) = match &decision.effect {
            // What can't be undone is offered "always" only as a delete in a
            // folder: an `rm` command's grant is a delete grant.
            _ if deleting => (vec![Ability::Delete], None),
            Effect::Write { .. } => (vec![Ability::Write], None),
            Effect::Delete { .. } => (vec![Ability::Delete], None),
            Effect::Move { .. } => (vec![Ability::Write, Ability::Delete], None),
            Effect::Exec { .. } => (vec![Ability::Run], None),
            Effect::Session { .. } => (vec![Ability::Session], None),
            Effect::Connector {
                connector,
                write: true,
            } => (vec![Ability::ConnectorWrite], Some(connector.clone())),
            _ => eyre::bail!("nothing to keep for this call"),
        };
        let session = matches!(choice, PermissionChoice::AllowSession);
        let session_id = match session {
            true => Some(
                facts
                    .session_id
                    .clone()
                    .ok_or_else(|| eyre::eyre!("no session"))?,
            ),
            false => None,
        };
        let folder = match connector {
            Some(_) => None,
            None => Some(
                shown_folder
                    .or_else(|| facts.folder.clone().filter(|_| session))
                    .ok_or_else(|| eyre::eyre!("no folder"))?,
            ),
        };
        for ability in abilities {
            let grant = NewGrant {
                hub: self.policy.hub.clone(),
                agent: self.policy.agent.clone(),
                person: person.clone(),
                ability,
                folder: folder.clone(),
                connector: connector.clone(),
                session_id: session_id.clone(),
                expires_at: session
                    .then(|| chrono::Utc::now() + chrono::Duration::hours(SESSION_HOURS)),
                run: None,
                run_name: None,
            };
            self.block_on(self.computer.store().add_grant(&grant))?;
        }
        Ok(())
    }

    fn asks_nothing(&self) -> bool {
        self.policy.access.access == workforce_core::computer::Access::Full
            && self.policy.access.asks == workforce_core::computer::Asking::Never
    }

    fn unattended_runs(&self) -> bool {
        self.policy.access.access == workforce_core::computer::Access::Full
            && self.policy.access.unattended == workforce_core::computer::Unattended::Run
    }

    fn verdict(&self, tool: &str, effect: &Effect) -> Verdict {
        let facts = self.facts(tool, effect);
        // The dispatch gate is synchronous; the hub runs on a multi-thread
        // runtime, as the surface allow list's lookup relies on too. Anywhere
        // else the verdict can't be reached, and nothing runs.
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return self.unreachable();
        };
        if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
            return self.unreachable();
        }
        let reached = tokio::task::block_in_place(|| {
            handle.block_on(self.computer.verdict(&self.policy, &facts))
        });
        reached.unwrap_or_else(|e| {
            warn!(agent = %self.policy.agent, tool, error = %format!("{e:#}"), "computer verdict unreachable");
            self.unreachable()
        })
    }

    fn record(&self, record: Record) {
        let activity = self.activity(&record);
        let computer = self.computer.clone();
        let Ok(handle) = tokio::runtime::Handle::try_current() else {
            return;
        };
        handle.spawn(async move {
            if let Err(e) = computer.store().record_activity(&activity).await {
                warn!(error = %format!("{e:#}"), tool = %activity.tool, "recording computer activity failed");
            }
        });
    }
}

/// Makes every agent's turns on this hub go through the gate. Called once,
/// when `[desk]` starts.
pub fn install(computer: Computer, config: Arc<WorkforceConfig>) {
    let installed = workforce_hub_core::serve::install_computer_gates(Box::new(move |subject| {
        Some(Arc::new(HubComputerGate::for_agent(
            computer.clone(),
            &config,
            subject,
        )) as Arc<dyn ComputerGate>)
    }));
    if !installed {
        warn!("the computer gate was installed twice; the first stands");
    }
}

#[cfg(test)]
mod tests {
    use std::path::Path;
    use std::time::Duration;

    use async_trait::async_trait;
    use serde_json::{json, Value};
    use workforce_agent::runner::{CURRENT_SESSION_KEY, CURRENT_USER_ID};
    use workforce_agent::tools::computer::{settle, unattended_refusal, Decision};
    use workforce_agent::tools::dispatch::{gate_tool_call, GateOutcome, ToolGates};
    use workforce_agent::tools::exec::ExecCommand;
    use workforce_agent::tools::filesystem::WriteFile;
    use workforce_agent::tools::registry::ToolRegistry;
    use workforce_agent::tools::{Tool, ToolResult};
    use workforce_core::types::AgentId;
    use workforce_desk::computer::ActivityFilter;
    use workforce_desk::DeskStore;
    use workforce_policy::engine::PolicyEngine;
    use workforce_postgres::test_support::with_test_pool;

    use super::*;

    /// A tool in a namespace that touches nothing here.
    struct Elsewhere(&'static str);

    #[async_trait]
    impl Tool for Elsewhere {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "elsewhere"
        }
        fn input_schema(&self) -> Value {
            json!({"type": "object"})
        }
        async fn execute(&self, _: Value) -> eyre::Result<ToolResult> {
            Ok(ToolResult::success(json!({}), 0))
        }
    }

    fn policy() -> PolicyEngine {
        PolicyEngine::from_yaml(
            "rules:\n  - name: allow-all\n    scope: global\n    tool: \"*\"\n    action: allow\n",
        )
        .unwrap()
    }

    fn tools(workspace: &Path) -> ToolRegistry {
        let tools = ToolRegistry::new();
        tools.register(Arc::new(
            ExecCommand::default().with_workspace(workspace.to_path_buf()),
        ));
        tools.register(Arc::new(WriteFile::new(vec![workspace.to_path_buf()])));
        tools.register(Arc::new(workforce_agent::tools::filesystem::ReadFile::new(
            vec![workspace.to_path_buf()],
        )));
        tools.register(Arc::new(Elsewhere("memory.recall")));
        tools.register(Arc::new(Elsewhere("web.fetch")));
        tools
    }

    fn gates(gate: Option<Arc<dyn ComputerGate>>) -> ToolGates {
        ToolGates {
            agent_id: AgentId::new("allan"),
            team_id: None,
            role: None,
            capability_gate: None,
            permission_gate: None,
            surface_allows: None,
            computer_gate: gate,
            arguments: Default::default(),
        }
    }

    struct Hub {
        _dir: tempfile::TempDir,
        config: WorkforceConfig,
        subject: ComputerSubject,
    }

    fn hub() -> Hub {
        // Desk's layout: the hub's data in `…/Desk/hub/data`, agents in it.
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path().canonicalize().unwrap().join("Desk/hub/data");
        let workspace = data.join("agents/allan");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut config = WorkforceConfig::default();
        config.hub.data_dir = data.to_string_lossy().into_owned();
        Hub {
            config,
            subject: ComputerSubject {
                agent: "allan".into(),
                name: Some("Allan".into()),
                workspace,
            },
            _dir: dir,
        }
    }

    fn as_sarah<T>(f: impl FnOnce() -> T) -> T {
        CURRENT_USER_ID.sync_scope("sarah".into(), || {
            CURRENT_SESSION_KEY.sync_scope("s1".into(), f)
        })
    }

    fn kind(outcome: &GateOutcome) -> &'static str {
        match outcome {
            GateOutcome::Execute { .. } => "run",
            GateOutcome::Blocked { .. } => "refused",
            GateOutcome::RequireApproval {
                allow_save_allow: false,
                holds: None,
                ..
            } => "asks once",
            GateOutcome::RequireApproval { .. } => "asks, offering always",
        }
    }

    /// Desk's browser keeps its profiles in Desk's own folder, which no
    /// agent's file tools reach, its own folder or Full access included.
    #[test]
    fn desks_browser_profiles_are_kept_from_every_agent() {
        let mut hub = hub();
        assert_eq!(
            crate::desk_folder::browser_profiles(&hub.config),
            None,
            "only on Desk"
        );
        hub.config.desk.enabled = true;
        let root =
            crate::desk_folder::browser_profiles(&hub.config).expect("Desk's browser folder");
        assert!(root.ends_with("Desk/browser"), "{}", root.display());
        let home = home();
        let policy = AgentPolicy {
            hub: "local".into(),
            agent: "allan".into(),
            name: "Allan".into(),
            access: ComputerAccess::default(),
            own: vec![hub.subject.workspace.clone()],
            works_in: Vec::new(),
            withheld: withheld(&hub.config, &home),
            config: own_config(&hub.subject.workspace),
            scratch: scratch(),
            home,
            contained: false,
            connectors: None,
        };
        let profile = root.join("0123abcd/Default/Cookies");
        for effect in [
            Effect::Read {
                paths: vec![profile.clone()],
            },
            Effect::Write {
                paths: vec![profile.clone()],
            },
        ] {
            let call = CallFacts {
                tool: "filesystem.read".into(),
                effect: effect.clone(),
                person: Some("sarah".into()),
                session_id: Some("s1".into()),
                run: None,
                folder: None,
            };
            let verdict = workforce_desk::computer::policy::verdict(&policy, false, &[], &call);
            assert!(
                matches!(
                    &verdict,
                    Verdict::Refuse {
                        refusal: Refusal {
                            rule: Refused::Withheld,
                            ..
                        }
                    }
                ),
                "{effect:?}: {verdict:?}"
            );
        }
    }

    /// A server hub runs no `[desk]`: nothing installs the gate,
    /// a composed agent has none, and its calls are decided as on main.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_hub_without_desk_has_no_gate_and_decides_as_before() {
        let dir = tempfile::tempdir().unwrap();
        let service = crate::serve::testing::plain_service(dir.path()).await;
        assert!(service.computer_gate().is_none());
        let gates = service.tool_gates(None);
        assert!(gates.computer_gate.is_none());
        let hub = hub();
        let tools = tools(&hub.subject.workspace);
        for (tool, input) in [
            ("exec.run", json!({"command": "git push --force"})),
            ("filesystem.write", json!({"path": "a.md", "content": "x"})),
            ("memory.recall", json!({})),
        ] {
            let outcome = as_sarah(|| gate_tool_call(&gates, &policy(), &tools, tool, &input));
            assert_eq!(kind(&outcome), "run", "{tool}");
        }
    }

    /// Every tool a composed agent registers in a namespace the gate decides
    /// says what it does.
    #[tokio::test]
    async fn every_computer_tool_an_agent_registers_declares_its_effect() {
        let dir = tempfile::tempdir().unwrap();
        let service = crate::serve::testing::plain_service(dir.path()).await;
        let decided: Vec<String> = service
            .tools
            .names()
            .into_iter()
            .filter(|name| namespace(name) == Some(Namespace::Computer))
            .collect();
        for wanted in [
            "exec.run",
            "filesystem.write",
            "filesystem.read",
            "code.git",
        ] {
            assert!(
                decided.iter().any(|n| n == wanted),
                "{wanted} in {decided:?}"
            );
        }
        for name in &decided {
            let tool = service.tools.get(name).unwrap();
            assert_ne!(
                tool.effect(&json!({})),
                Effect::Unknown,
                "{name} declares no effect"
            );
        }
    }

    async fn wait_for(store: &DeskStore, n: usize) -> Vec<workforce_desk::computer::ActivityRow> {
        for _ in 0..100 {
            let rows = store.activity(&ActivityFilter::default()).await.unwrap();
            if rows.len() >= n {
                return rows;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("activity never reached {n} rows");
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn on_a_desk_hub_an_agent_with_no_computer_section_asks_only_before_what_cant_be_undone()
    {
        with_test_pool(workforce_desk::store::build_pool, |pool| async move {
            let store = Arc::new(DeskStore::with_pool(pool).await.unwrap());
            let hub = hub();
            let gate: Arc<dyn ComputerGate> = Arc::new(HubComputerGate::for_agent(
                Computer::new(Arc::clone(&store)),
                &hub.config,
                &hub.subject,
            ));
            let tools = tools(&hub.subject.workspace);
            let g = gates(Some(Arc::clone(&gate)));
            let decide = |tool: &str, input: Value| {
                as_sarah(|| kind(&gate_tool_call(&g, &policy(), &tools, tool, &input)))
            };
            // An own-folder write runs, a relative path included.
            assert_eq!(decide("filesystem.write", json!({"path": "notes/a.md", "content": "x"})), "run");
            // An ordinary command runs; one that can't be undone asks,
            // offering only Allow once and Deny.
            assert_eq!(decide("exec.run", json!({"command": "npm test"})), "run");
            assert_eq!(decide("exec.run", json!({"command": "rm -rf /opt/x"})), "asks once");
            assert_eq!(decide("exec.run", json!({"command": "git push --force"})), "asks once");
            // Other namespaces never ask.
            assert_eq!(decide("memory.recall", json!({})), "run");
            assert_eq!(decide("web.fetch", json!({})), "run");

            // Nobody to ask (a job, a workflow step): even what would ask runs
            // at Full access, as before.
            let ran = as_sarah(|| {
                unattended_refusal(Some(gate.as_ref()), &tools, "exec.run", &json!({"command": "rm -rf /opt/x"}))
            });
            assert_eq!(ran, None);

            // The kill switch refuses even an own-folder write.
            store.set_access_stopped(true).await.unwrap();
            let stopped = as_sarah(|| {
                gate_tool_call(&g, &policy(), &tools, "filesystem.write", &json!({"path": "a.md", "content": "x"}))
            });
            match stopped {
                GateOutcome::Blocked { reason, .. } => assert_eq!(
                    reason,
                    "Access to this computer is stopped, so nothing was read, changed or run. Allow it again in Settings → This computer."
                ),
                other => panic!("{other:?}"),
            }
            store.set_access_stopped(false).await.unwrap();
        })
        .await;
    }

    /// Connector calls by their tool names, as the agent's `[skills]` and
    /// its asking say: off and read-only refuse, a send to people asks by
    /// default and never once the agent asks before nothing, any other
    /// write runs; a job still runs what would ask. Adapters Desk doesn't
    /// list are never decided.
    #[tokio::test(flavor = "multi_thread")]
    async fn connector_calls_follow_the_binding_and_only_sends_to_people_ask() {
        with_test_pool(workforce_desk::store::build_pool, |pool| async move {
            let store = Arc::new(DeskStore::with_pool(pool).await.unwrap());
            let hub = hub();
            let config = hub.subject.workspace.join("config.toml");
            let tools = tools(&hub.subject.workspace);
            for name in [
                "linear.issue_get",
                "linear.issue_create",
                "slack.send",
                "github.pr_merge",
                "github.pr_get",
                "jira.search",
                "browser.navigate",
            ] {
                tools.register(Arc::new(Elsewhere(name)));
            }
            let decide_with = |body: &str, tool: &str| {
                std::fs::write(&config, body).unwrap();
                let gate: Arc<dyn ComputerGate> = Arc::new(HubComputerGate::for_agent(
                    Computer::new(Arc::clone(&store)),
                    &hub.config,
                    &hub.subject,
                ));
                let g = gates(Some(Arc::clone(&gate)));
                let asked = as_sarah(|| kind(&gate_tool_call(&g, &policy(), &tools, tool, &json!({}))));
                let unattended = as_sarah(|| unattended_refusal(Some(gate.as_ref()), &tools, tool, &json!({})));
                (asked, unattended.is_none())
            };
            let bound = "[skills]\nenabled = [\"linear\", \"slack\", \"github\"]\nread_only = [\"github\"]\n";
            assert_eq!(decide_with(bound, "linear.issue_get").0, "run");
            assert_eq!(decide_with(bound, "linear.issue_create").0, "run");
            assert_eq!(decide_with(bound, "slack.send"), ("asks once", true));
            assert_eq!(decide_with(bound, "github.pr_get").0, "run");
            assert_eq!(decide_with(bound, "github.pr_merge").0, "refused");
            assert_eq!(decide_with(bound, "jira.search").0, "refused");
            assert_eq!(decide_with(bound, "browser.navigate").0, "run");
            // Asking before nothing: a send runs too.
            let never = format!("[computer]\nasks = \"never\"\n\n{bound}");
            assert_eq!(decide_with(&never, "slack.send").0, "run");
            // No binding: every connector, as before; a send still asks.
            assert_eq!(decide_with("", "jira.search").0, "run");
            assert_eq!(decide_with("", "slack.send").0, "asks once");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn below_full_access_commands_are_refused_in_a_plain_sentence() {
        with_test_pool(workforce_desk::store::build_pool, |pool| async move {
            let store = Arc::new(DeskStore::with_pool(pool).await.unwrap());
            let hub = hub();
            std::fs::write(
                hub.subject.workspace.join("config.toml"),
                "[computer]\naccess = \"own_folder\"\n",
            )
            .unwrap();
            let gate: Arc<dyn ComputerGate> = Arc::new(HubComputerGate::for_agent(
                Computer::new(Arc::clone(&store)),
                &hub.config,
                &hub.subject,
            ));
            let tools = tools(&hub.subject.workspace);
            let outcome = as_sarah(|| {
                gate_tool_call(&gates(Some(gate)), &policy(), &tools, "exec.run", &json!({"command": "ls"}))
            });
            match outcome {
                GateOutcome::Blocked { reason, .. } => assert_eq!(
                    reason,
                    "Allan can't run commands at Own folder on this computer. Choose Full access in Settings → This computer to let it."
                ),
                other => panic!("{other:?}"),
            }
        })
        .await;
    }

    /// Run `tool` as the gates decide it, with the reach they give it.
    async fn run(
        gates: &ToolGates,
        tools: &ToolRegistry,
        tool: &str,
        input: Value,
    ) -> (&'static str, Option<String>) {
        let (outcome, decided) = as_sarah(|| {
            workforce_agent::tools::dispatch::gate_tool_call_decided(
                gates,
                &policy(),
                tools,
                tool,
                &input,
            )
        });
        let GateOutcome::Execute { exec_input, .. } = outcome else {
            return (kind(&outcome), None);
        };
        let access = workforce_agent::tools::access::ToolAccessSnapshot::default();
        let exec = CURRENT_USER_ID
            .scope(
                "sarah".into(),
                workforce_agent::tools::dispatch::execute_decided(
                    tools,
                    &access,
                    tool,
                    &input,
                    exec_input,
                    decided.as_ref(),
                ),
            )
            .await;
        ("run", Some(exec.output_text))
    }

    /// Roots follow the level: Full access reads (and, asked, changes)
    /// anywhere but Desk's data; Own folder reads only its own; a hub
    /// without `[desk]` keeps the tool's roots.
    #[tokio::test(flavor = "multi_thread")]
    async fn file_tools_reach_as_far_as_the_level_and_never_into_desks_data() {
        with_test_pool(workforce_desk::store::build_pool, |pool| async move {
            let store = Arc::new(DeskStore::with_pool(pool).await.unwrap());
            let hub = hub();
            let data = Path::new(&hub.config.hub.data_dir).to_path_buf();
            let desk = data.parent().unwrap().parent().unwrap().to_path_buf();
            let elsewhere = desk.parent().unwrap().join("dev/acme/README.md");
            std::fs::create_dir_all(elsewhere.parent().unwrap()).unwrap();
            std::fs::write(&elsewhere, "the readme text").unwrap();
            let ben = data.join("agents/ben/notes.md");
            std::fs::create_dir_all(ben.parent().unwrap()).unwrap();
            std::fs::write(&ben, "ben's").unwrap();
            let tools = tools(&hub.subject.workspace);
            let read = |path: &Path| json!({"path": path.to_string_lossy()});
            let gate_at = |config: &str| -> ToolGates {
                std::fs::write(hub.subject.workspace.join("config.toml"), config).unwrap();
                gates(Some(Arc::new(HubComputerGate::for_agent(
                    Computer::new(Arc::clone(&store)),
                    &hub.config,
                    &hub.subject,
                ))))
            };

            let full = gate_at("");
            let (ran, out) = run(&full, &tools, "filesystem.read", read(&elsewhere)).await;
            assert_eq!(ran, "run");
            assert!(out.unwrap().contains("the readme text"));
            assert_eq!(
                run(&full, &tools, "filesystem.read", read(&ben)).await.0,
                "refused"
            );
            assert_eq!(
                run(
                    &full,
                    &tools,
                    "filesystem.read",
                    read(&data.join("vault.db"))
                )
                .await
                .0,
                "refused"
            );
            // Its own config reads as before; changing it is refused.
            assert_eq!(
                run(
                    &full,
                    &tools,
                    "filesystem.read",
                    json!({"path": "config.toml"})
                )
                .await
                .0,
                "run"
            );
            assert_eq!(
                run(
                    &full,
                    &tools,
                    "filesystem.write",
                    json!({"path": "config.toml", "content": "x"})
                )
                .await
                .0,
                "refused"
            );
            // A change outside runs: Full access asks only before what
            // can't be undone.
            assert_eq!(
                run(
                    &full,
                    &tools,
                    "filesystem.write",
                    json!({"path": elsewhere.to_string_lossy(), "content": "changed"})
                )
                .await
                .0,
                "run"
            );
            assert_eq!(std::fs::read_to_string(&elsewhere).unwrap(), "changed");

            let own = gate_at("[computer]\naccess = \"own_folder\"\n");
            assert_eq!(
                run(&own, &tools, "filesystem.read", read(&elsewhere))
                    .await
                    .0,
                "refused"
            );

            // No gate: the tool's own roots, as on main.
            let (ran, out) = run(&gates(None), &tools, "filesystem.read", read(&elsewhere)).await;
            assert_eq!(ran, "run");
            assert!(
                !out.unwrap().contains("the readme text"),
                "refused by the tool's roots"
            );
        })
        .await;
    }

    /// A folder the person gave it to work in is read and changed at Own
    /// folder as its own folder is; a folder beside it is not, and one in
    /// Desk's data is left out however it got into the file.
    #[tokio::test(flavor = "multi_thread")]
    async fn file_tools_reach_the_folders_it_works_in_at_every_level() {
        with_test_pool(workforce_desk::store::build_pool, |pool| async move {
            let store = Arc::new(DeskStore::with_pool(pool).await.unwrap());
            let hub = hub();
            let data = Path::new(&hub.config.hub.data_dir).to_path_buf();
            let desk = data.parent().unwrap().parent().unwrap().to_path_buf();
            let repo = desk.parent().unwrap().join("dev/acme");
            let file = repo.join("README.md");
            let beside = desk.parent().unwrap().join("dev/other/notes.md");
            for f in [&file, &beside] {
                std::fs::create_dir_all(f.parent().unwrap()).unwrap();
                std::fs::write(f, "words").unwrap();
            }
            let ben = data.join("agents/ben/notes.md");
            std::fs::create_dir_all(ben.parent().unwrap()).unwrap();
            std::fs::write(&ben, "ben's").unwrap();
            let tools = tools(&hub.subject.workspace);
            let read = |path: &Path| json!({"path": path.to_string_lossy()});
            let config = format!(
                "[computer]\naccess = \"own_folder\"\nworks_in = [\"{}\", \"{}\"]\n",
                repo.display(),
                ben.parent().unwrap().display()
            );
            std::fs::write(hub.subject.workspace.join("config.toml"), config).unwrap();
            // The test's folders are in its own home.
            let home = workforce_agent::tools::paths::resolve_real(desk.parent().unwrap());
            let gate = HubComputerGate::for_agent_at(
                Computer::new(Arc::clone(&store)),
                &hub.config,
                &hub.subject,
                home,
            );
            assert_eq!(
                gate.policy.works_in,
                [workforce_agent::tools::paths::resolve_real(&repo)],
                "the folder in Desk's data is left out"
            );
            let gates = gates(Some(Arc::new(gate)));
            let (ran, out) = run(&gates, &tools, "filesystem.read", read(&file)).await;
            assert_eq!(ran, "run");
            assert!(out.unwrap().contains("words"));
            assert_eq!(
                run(
                    &gates,
                    &tools,
                    "filesystem.write",
                    json!({"path": file.to_string_lossy(), "content": "changed"})
                )
                .await
                .0,
                "run"
            );
            assert_eq!(std::fs::read_to_string(&file).unwrap(), "changed");
            // `code.*` takes the folder's full path as its repo, and runs
            // there; one beside it is no repo.
            assert!(std::process::Command::new("git")
                .args(["init", "-q"])
                .current_dir(&repo)
                .status()
                .unwrap()
                .success());
            tools.register(Arc::new(crate::code::CodeGitTool::new(vec![hub
                .subject
                .workspace
                .clone()])));
            let (ran, out) = run(
                &gates,
                &tools,
                "code.git",
                json!({"action": "status", "repo": repo.to_string_lossy()}),
            )
            .await;
            assert_eq!(ran, "run");
            let out = out.unwrap();
            assert!(out.contains("README.md"), "{out}");
            let (_, out) = run(
                &gates,
                &tools,
                "code.git",
                json!({"action": "status", "repo": beside.parent().unwrap().to_string_lossy()}),
            )
            .await;
            assert!(!out.unwrap_or_default().contains("notes.md"));
            assert_eq!(
                run(&gates, &tools, "filesystem.read", read(&beside))
                    .await
                    .0,
                "refused"
            );
            assert_eq!(
                run(&gates, &tools, "filesystem.read", read(&ben)).await.0,
                "refused"
            );
        })
        .await;
    }

    /// A card offers "for this session" and "always in this folder" (the
    /// repo holding the target), shows the hub's facts, and keeps an
    /// answer as a grant that then lets the call run: the session's only in
    /// its session. What wasn't offered is never kept.
    #[tokio::test(flavor = "multi_thread")]
    async fn session_and_folder_answers_are_kept_as_grants_scoped_as_offered() {
        with_test_pool(workforce_desk::store::build_pool, |pool| async move {
            let store = Arc::new(DeskStore::with_pool(pool).await.unwrap());
            let hub = hub();
            // Asking in its own folder, where the repo it works in is: the
            // level where cards with choices are reached now.
            let repo = hub.subject.workspace.join("acme-web");
            std::fs::create_dir_all(repo.join(".git")).unwrap();
            std::fs::create_dir_all(repo.join("src")).unwrap();
            std::fs::write(
                hub.subject.workspace.join("config.toml"),
                "[computer]\naccess = \"read_anywhere\"\nask_in_own_folder = true\n",
            )
            .unwrap();
            let gate = HubComputerGate::for_agent(
                Computer::new(Arc::clone(&store)),
                &hub.config,
                &hub.subject,
            );
            let write = |path: &Path| Effect::Write {
                paths: vec![path.to_path_buf()],
            };
            let decide = |gate: &HubComputerGate, effect: Effect| {
                let verdict = as_sarah(|| gate.verdict("filesystem.write", &effect));
                Decision {
                    effect,
                    verdict,
                    reach: None,
                }
            };
            let first = decide(&gate, write(&repo.join("src/a.rs")));
            let card = as_sarah(|| gate.prompt(&first));
            assert!(card.allow_once && card.deny_once && card.allow_session);
            assert!(!card.allow_always && !card.deny_always);
            assert_eq!(
                card.allow_in_folder.as_deref(),
                Some(shown(&repo, &gate.policy.home).as_str()),
                "the repo, not its src"
            );
            let facts = card.facts.unwrap();
            assert_eq!(facts.effect, "write");
            assert_eq!(
                facts.paths,
                [shown(&repo.join("src/a.rs"), &gate.policy.home)]
            );
            assert_eq!(facts.why, "You asked to be asked in its folder too");

            // "For this session": runs in this session only.
            as_sarah(|| gate.keep(&first, PermissionChoice::AllowSession)).unwrap();
            let again = decide(&gate, write(&repo.join("b.rs")));
            assert!(
                matches!(again.verdict, Verdict::Run { by: By::Session(_) }),
                "{:?}",
                again.verdict
            );
            let elsewhere = CURRENT_USER_ID.sync_scope("sarah".into(), || {
                CURRENT_SESSION_KEY.sync_scope("s2".into(), || {
                    gate.verdict("filesystem.write", &again.effect)
                })
            });
            assert!(matches!(elsewhere, Verdict::Ask { .. }), "{elsewhere:?}");
            // "Always in this folder": any session.
            as_sarah(|| gate.keep(&first, PermissionChoice::AllowInFolder)).unwrap();
            let elsewhere = CURRENT_USER_ID.sync_scope("sarah".into(), || {
                CURRENT_SESSION_KEY.sync_scope("s2".into(), || {
                    gate.verdict("filesystem.write", &again.effect)
                })
            });
            assert!(
                matches!(elsewhere, Verdict::Run { by: By::Grant(_) }),
                "{elsewhere:?}"
            );
            let grants = store.grants(Some("allan")).await.unwrap();
            assert_eq!(grants.len(), 2);
            assert!(grants
                .iter()
                .all(|g| g.grant.folder.as_deref() == Some(repo.as_path())));
            assert!(grants
                .iter()
                .any(|g| g.grant.session_id.as_deref() == Some("s1") && g.expires_at.is_some()));

            // At Full access, what can't be undone offers neither, and keeps
            // neither.
            std::fs::write(hub.subject.workspace.join("config.toml"), "").unwrap();
            let full = HubComputerGate::for_agent(
                Computer::new(Arc::clone(&store)),
                &hub.config,
                &hub.subject,
            );
            let force = Effect::Exec {
                command: "git push --force".into(),
                cwd: Some(repo.clone()),
            };
            let force = Decision {
                verdict: as_sarah(|| full.verdict("exec.run", &force)),
                effect: force,
                reach: None,
            };
            let card = as_sarah(|| full.prompt(&force));
            assert!(!card.allow_session && card.allow_in_folder.is_none());
            assert_eq!(card.facts.unwrap().why, "Can't be undone");
            assert!(as_sarah(|| full.keep(&force, PermissionChoice::AllowInFolder)).is_err());
            assert!(as_sarah(|| full.keep(&force, PermissionChoice::AllowSession)).is_err());
            assert_eq!(store.grants(Some("allan")).await.unwrap().len(), 2);
        })
        .await;
    }

    /// An approved call that would now do something else than the card
    /// showed is refused, not run.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_approval_is_bound_to_what_the_card_showed() {
        with_test_pool(workforce_desk::store::build_pool, |pool| async move {
            let store = Arc::new(DeskStore::with_pool(pool).await.unwrap());
            let hub = hub();
            let gate: Arc<dyn ComputerGate> = Arc::new(HubComputerGate::for_agent(
                Computer::new(Arc::clone(&store)),
                &hub.config,
                &hub.subject,
            ));
            let tools = tools(&hub.subject.workspace);
            let g = gates(Some(Arc::clone(&gate)));
            let input = json!({"command": "echo hi"});
            let shown = Decision {
                effect: Effect::Exec {
                    command: "echo hi".into(),
                    cwd: Some(PathBuf::from("/somewhere/else")),
                },
                verdict: Verdict::Ask {
                    choices: vec![],
                    why: workforce_core::computer::Why::Runs,
                },
                reach: None,
            };
            let access = workforce_agent::tools::access::ToolAccessSnapshot::default();
            let exec = CURRENT_USER_ID
                .scope(
                    "sarah".into(),
                    workforce_agent::tools::dispatch::execute_approved(
                        &g,
                        &policy(),
                        &tools,
                        &access,
                        "exec.run",
                        &input,
                        Some(&shown),
                    ),
                )
                .await;
            assert_eq!(
                exec.deny_reason.as_deref(),
                Some(workforce_agent::tools::computer::CHANGED_SINCE_ALLOWED)
            );
        })
        .await;
    }

    /// With `unattended = "refuse"`, what would ask is refused at once where
    /// nobody can be asked; what the level refuses stays refused either way.
    #[tokio::test(flavor = "multi_thread")]
    async fn where_nobody_can_be_asked_an_ask_runs_unless_the_agent_says_refuse() {
        with_test_pool(workforce_desk::store::build_pool, |pool| async move {
            let store = Arc::new(DeskStore::with_pool(pool).await.unwrap());
            let hub = hub();
            let tools = tools(&hub.subject.workspace);
            // What can't be undone: the only thing Full access asks about.
            let input = json!({"command": "rm -rf /opt/x"});
            let gate_with = |config: &str| -> Arc<dyn ComputerGate> {
                std::fs::write(hub.subject.workspace.join("config.toml"), config).unwrap();
                Arc::new(HubComputerGate::for_agent(
                    Computer::new(Arc::clone(&store)),
                    &hub.config,
                    &hub.subject,
                ))
            };
            let runs = gate_with("");
            assert!(as_sarah(|| unattended_refusal(
                Some(runs.as_ref()),
                &tools,
                "exec.run",
                &input
            ))
            .is_none());
            let (outcome, _) = as_sarah(|| {
                workforce_agent::tools::dispatch::gate_tool_call_unattended(
                    &gates(Some(Arc::clone(&runs))),
                    &policy(),
                    &tools,
                    "exec.run",
                    &input,
                )
            });
            assert_eq!(kind(&outcome), "run");
            let refuses = gate_with("[computer]\nunattended = \"refuse\"\n");
            let refusal =
                as_sarah(|| unattended_refusal(Some(refuses.as_ref()), &tools, "exec.run", &input))
                    .unwrap();
            assert!(refusal.contains("nobody can be asked here"), "{refusal}");
            let (outcome, _) = as_sarah(|| {
                workforce_agent::tools::dispatch::gate_tool_call_unattended(
                    &gates(Some(refuses)),
                    &policy(),
                    &tools,
                    "exec.run",
                    &input,
                )
            });
            assert_eq!(
                kind(&outcome),
                "asks once",
                "the runner refuses it: no card"
            );
            let lower = gate_with("[computer]\naccess = \"own_folder\"\n");
            assert!(as_sarah(|| unattended_refusal(
                Some(lower.as_ref()),
                &tools,
                "exec.run",
                &input
            ))
            .is_some());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn each_outcome_is_recorded_once_with_its_arguments_redacted() {
        with_test_pool(workforce_desk::store::build_pool, |pool| async move {
            let store = Arc::new(DeskStore::with_pool(pool).await.unwrap());
            let hub = hub();
            let gate: Arc<dyn ComputerGate> = Arc::new(HubComputerGate::for_agent(
                Computer::new(Arc::clone(&store)),
                &hub.config,
                &hub.subject,
            ));
            let tools = tools(&hub.subject.workspace);
            let token = format!("ghp_{}", "a1B2c3D4e5F6g7H8i9J0k1L2m3N4o5P6q7R8");
            let input = json!({"command": format!("rm -rf /opt/{token}"), "content": "a whole file's text"});
            let effect = tools.get("exec.run").unwrap().effect(&input);
            let verdict = as_sarah(|| gate.verdict("exec.run", &effect));
            assert!(matches!(verdict, Verdict::Ask { .. }));
            let decision = Decision {
                effect,
                verdict,
                reach: None,
            };
            // Answered from Desk's device: the row says which.
            answered_from("q1", Some("desk-on-ash"));
            as_sarah(|| {
                workforce_agent::tools::computer::settle_answered(
                    Some(gate.as_ref()),
                    "exec.run",
                    Some(&decision),
                    &input,
                    Outcome::Allowed,
                    "q1",
                )
            });
            for outcome in [Outcome::Declined, Outcome::NotAnswered] {
                as_sarah(|| {
                    settle(
                        Some(gate.as_ref()),
                        "exec.run",
                        Some(&decision),
                        &input,
                        outcome,
                        None,
                    )
                });
            }
            // Deciding alone records nothing.
            as_sarah(|| {
                gate_tool_call(
                    &gates(Some(Arc::clone(&gate))),
                    &policy(),
                    &tools,
                    "exec.run",
                    &input,
                )
            });
            let rows = wait_for(&store, 3).await;
            tokio::time::sleep(Duration::from_millis(100)).await;
            let rows = if rows.len() == 3 {
                store.activity(&ActivityFilter::default()).await.unwrap()
            } else {
                rows
            };
            assert_eq!(rows.len(), 3);
            let mut outcomes: Vec<String> = rows
                .iter()
                .map(|r| r.activity.outcome["outcome"].as_str().unwrap().to_string())
                .collect();
            outcomes.sort();
            assert_eq!(outcomes, ["allowed", "declined", "not_answered"]);
            for row in &rows {
                let a = &row.activity;
                assert_eq!((a.kind.as_str(), a.decided_by.as_str()), ("exec", "person"));
                assert_eq!(a.person.as_deref(), Some("sarah"));
                assert_eq!(a.session_id.as_deref(), Some("s1"));
                assert!(!a.arguments.to_string().contains(&token), "{}", a.arguments);
                assert!(!a.doing.contains(&token), "{}", a.doing);
                // Its tool and a short line, never what it was given.
                assert_eq!(a.arguments["tool"], "exec.run");
                assert!(a.arguments.get("command").is_none() && a.arguments.get("content").is_none());
                assert!(!a.arguments.to_string().contains("a whole file's text"));
                let answered = a.outcome["outcome"] == "allowed";
                assert_eq!(a.device.as_deref(), answered.then_some("desk-on-ash"));
                assert_eq!(a.request_id.as_deref(), answered.then_some("q1"));
            }
            assert!(capped(&"x".repeat(MAX_DOING * 2)).chars().count() <= MAX_DOING + 1);
        })
        .await;
    }

    /// At Full access, deleting outside the folder asks once per folder: the
    /// "always" kept is a delete grant on the folder the card showed, and
    /// then `rm -rf node_modules` there runs.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_always_for_a_delete_is_kept_for_the_folder_the_card_showed() {
        with_test_pool(workforce_desk::store::build_pool, |pool| async move {
            let store = Arc::new(DeskStore::with_pool(pool).await.unwrap());
            let hub = hub();
            let repo = Path::new(&hub.config.hub.data_dir)
                .ancestors()
                .nth(3)
                .unwrap()
                .join("dev/acme-web");
            std::fs::create_dir_all(repo.join(".git")).unwrap();
            std::fs::create_dir_all(repo.join("web")).unwrap();
            let mut gate = HubComputerGate::for_agent(
                Computer::new(Arc::clone(&store)),
                &hub.config,
                &hub.subject,
            );
            // The test's folders are under $TMPDIR, scratch space itself:
            // only /tmp is scratch here.
            gate.policy.scratch = scratch_of(&[PathBuf::from("/tmp")]);
            let rm = |cwd: &Path, command: &str| Effect::Exec {
                command: command.into(),
                cwd: Some(cwd.to_path_buf()),
            };
            let first = rm(&repo.join("web"), "rm -rf node_modules");
            let first = Decision {
                verdict: as_sarah(|| gate.verdict("exec.run", &first)),
                effect: first,
                reach: None,
            };
            let card = as_sarah(|| gate.prompt(&first));
            assert_eq!(
                card.allow_in_folder.as_deref(),
                Some(shown(&repo, &gate.policy.home).as_str())
            );
            assert!(!card.allow_session, "a delete is never a session's");
            as_sarah(|| gate.keep(&first, PermissionChoice::AllowInFolder)).unwrap();
            let kept = store.grants(Some("allan")).await.unwrap();
            assert_eq!(kept.len(), 1);
            assert_eq!(
                kept[0].grant.ability,
                workforce_core::computer::Ability::Delete
            );
            assert_eq!(kept[0].grant.folder.as_deref(), Some(repo.as_path()));
            // Anywhere in the repo now runs; elsewhere still asks.
            let again = rm(&repo, "rm -rf dist web/node_modules");
            assert!(matches!(
                as_sarah(|| gate.verdict("exec.run", &again)),
                Verdict::Run { by: By::Grant(_) }
            ));
            let elsewhere = rm(&repo, "rm -rf ../other");
            assert!(matches!(
                as_sarah(|| gate.verdict("exec.run", &elsewhere)),
                Verdict::Ask { .. }
            ));
            // Scratch space never asks.
            let tmp = rm(&repo, "rm -rf /tmp/scratch-build");
            assert!(matches!(
                as_sarah(|| gate.verdict("exec.run", &tmp)),
                Verdict::Run { .. }
            ));
        })
        .await;
    }
}
