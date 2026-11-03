//! The computer gate on a hub with `[desk]`: what each local agent may read,
//! change or run on the person's computer, decided per call by Desk's
//! verdict ([`workforce_desk::computer::policy::verdict`]) and recorded in
//! its activity. Installed by [`install`] when Desk starts; a hub without
//! `[desk]` installs nothing and every call is decided as before.
//!
//! Only the computer namespaces (`filesystem`, `code`, `exec`, `desk`,
//! `coding`, `claude_code`, `wasm_skill`, `attachments`) are decided.
//! Connectors pass until their read lists and per-agent bindings land
//! (AC9), and every other namespace never asks.
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
    match AgentWorkspaceConfig::load(&subject.workspace) {
        Ok(Some(config)) => config.computer().unwrap_or_else(|e| {
            warn!(agent = %subject.agent, error = %e, "[computer] unreadable; asking first");
            ComputerAccess::default()
        }),
        _ => ComputerAccess::default(),
    }
}

/// What no agent reaches on this computer: Desk's data (the hub's data, its
/// Postgres, vault, memory and logs, Desk's own secrets and the PTY holder's
/// socket, every other agent's folder) and the keychains. On Desk the hub's
/// data is `…/Desk/hub/data`, so Desk's folder is two up; elsewhere the
/// hub's data folder alone. An agent's own folder is carved out by the
/// verdict.
fn withheld(config: &WorkforceConfig, home: &Path) -> Vec<PathBuf> {
    let data = workforce_agent::tools::paths::resolve_real(Path::new(&config.hub.data_dir));
    let desk = data
        .parent()
        .filter(|hub| hub.file_name().is_some_and(|n| n == "hub"))
        .and_then(Path::parent)
        .map(Path::to_path_buf);
    let mut out = vec![desk.unwrap_or(data)];
    out.push(home.join("Library/Keychains"));
    out
}

/// An agent's own config files and folders, which no call changes.
fn own_config(workspace: &Path) -> Vec<PathBuf> {
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
        let home = home();
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
                withheld: withheld(config, &home),
                config: own_config(&subject.workspace),
                home,
                // Commands can't be contained yet (AC5).
                contained: false,
                connectors: None,
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
        // The record form keeps secret markers as markers; anything that
        // still reads as a secret is redacted before it is kept.
        let mut arguments = record.input.clone();
        workforce_common::redact::redact_json_strings(&mut arguments, 16);
        Activity {
            hub: self.policy.hub.clone(),
            agent: self.policy.agent.clone(),
            person: CURRENT_USER_ID
                .try_with(|u| u.clone())
                .ok()
                .filter(|u| !u.is_empty()),
            session_id: Some(session).filter(|s| !s.is_empty()),
            request_id: None,
            kind: kind.into(),
            doing: describe(&record.decision.effect).replace(&home.display().to_string(), "~"),
            folder: folder.map(|f| shown(&f, home)),
            connector: None,
            tool: record.tool.clone(),
            vault_source: None,
            decided_by: decided_by.into(),
            outcome: json!({
                "outcome": record.outcome.as_str(),
                "verdict": record.decision.verdict,
            }),
            reason: record.reason.clone(),
            exit_code: None,
            duration_ms: None,
            arguments,
        }
    }
}

impl ComputerGate for HubComputerGate {
    fn decides(&self, tool: &str) -> bool {
        namespace(tool) == Some(Namespace::Computer)
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
        reaches.then(|| workforce_agent::tools::paths::Reach {
            withheld: self.policy.withheld.clone(),
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
        let facts = self.facts("", &decision.effect);
        let person = facts
            .person
            .clone()
            .ok_or_else(|| eyre::eyre!("nobody to keep it for"))?;
        let (abilities, connector): (Vec<Ability>, Option<String>) = match &decision.effect {
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
                facts
                    .folder
                    .clone()
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
    async fn on_a_desk_hub_an_agent_with_no_computer_section_asks_before_commands() {
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
            // A command asks, offering only Allow once and Deny.
            assert_eq!(decide("exec.run", json!({"command": "npm test"})), "asks once");
            assert_eq!(decide("exec.run", json!({"command": "rm -rf /tmp/x"})), "asks once");
            // Other namespaces never ask.
            assert_eq!(decide("memory.recall", json!({})), "run");
            assert_eq!(decide("web.fetch", json!({})), "run");

            // Nobody to ask (a job, a workflow step): at Full access it runs,
            // as before.
            let ran = as_sarah(|| {
                unattended_refusal(Some(gate.as_ref()), &tools, "exec.run", &json!({"command": "npm test"}))
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
            // A change outside asks first.
            assert_eq!(
                run(
                    &full,
                    &tools,
                    "filesystem.write",
                    json!({"path": elsewhere.to_string_lossy(), "content": "x"})
                )
                .await
                .0,
                "asks once"
            );

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

    /// A card offers "for this session" and "always in this folder" (the
    /// repo holding the target), shows the hub's facts, and keeps an
    /// answer as a grant that then lets the call run: the session's only in
    /// its session. What wasn't offered is never kept.
    #[tokio::test(flavor = "multi_thread")]
    async fn session_and_folder_answers_are_kept_as_grants_scoped_as_offered() {
        with_test_pool(workforce_desk::store::build_pool, |pool| async move {
            let store = Arc::new(DeskStore::with_pool(pool).await.unwrap());
            let hub = hub();
            let repo = Path::new(&hub.config.hub.data_dir)
                .ancestors()
                .nth(3)
                .unwrap()
                .join("dev/acme-web");
            std::fs::create_dir_all(repo.join(".git")).unwrap();
            std::fs::create_dir_all(repo.join("src")).unwrap();
            let gate = HubComputerGate::for_agent(
                Computer::new(Arc::clone(&store)),
                &hub.config,
                &hub.subject,
            );
            let exec = |cwd: &Path, command: &str| Effect::Exec {
                command: command.into(),
                cwd: Some(cwd.to_path_buf()),
            };
            let decide = |effect: Effect| {
                let verdict = as_sarah(|| gate.verdict("exec.run", &effect));
                Decision {
                    effect,
                    verdict,
                    reach: None,
                }
            };
            let tests = decide(exec(&repo.join("src"), "npm test"));
            let card = as_sarah(|| gate.prompt(&tests));
            assert!(card.allow_once && card.deny_once && card.allow_session);
            assert!(!card.allow_always && !card.deny_always);
            assert_eq!(
                card.allow_in_folder.as_deref(),
                Some(shown(&repo, &gate.policy.home).as_str()),
                "the repo, not its src"
            );
            let facts = card.facts.unwrap();
            assert_eq!(facts.effect, "exec");
            assert_eq!(facts.command.as_deref(), Some("npm test"));
            assert_eq!(facts.why, "Runs on this computer");

            // "For this session": runs in this session only.
            as_sarah(|| gate.keep(&tests, PermissionChoice::AllowSession)).unwrap();
            let again = decide(exec(&repo, "npm run lint"));
            assert!(
                matches!(again.verdict, Verdict::Run { by: By::Session(_) }),
                "{:?}",
                again.verdict
            );
            let elsewhere = CURRENT_USER_ID.sync_scope("sarah".into(), || {
                CURRENT_SESSION_KEY
                    .sync_scope("s2".into(), || gate.verdict("exec.run", &again.effect))
            });
            assert!(matches!(elsewhere, Verdict::Ask { .. }), "{elsewhere:?}");
            // "Always in this folder": any session.
            as_sarah(|| gate.keep(&tests, PermissionChoice::AllowInFolder)).unwrap();
            let elsewhere = CURRENT_USER_ID.sync_scope("sarah".into(), || {
                CURRENT_SESSION_KEY
                    .sync_scope("s2".into(), || gate.verdict("exec.run", &again.effect))
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

            // The always-asks set offers neither, and keeps neither.
            let force = decide(exec(&repo, "git push --force"));
            let card = as_sarah(|| gate.prompt(&force));
            assert!(!card.allow_session && card.allow_in_folder.is_none());
            assert!(as_sarah(|| gate.keep(&force, PermissionChoice::AllowInFolder)).is_err());
            assert!(as_sarah(|| gate.keep(&force, PermissionChoice::AllowSession)).is_err());
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
            let input = json!({"command": "npm test"});
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
            let input = json!({"command": format!("echo {token}")});
            let effect = tools.get("exec.run").unwrap().effect(&input);
            let verdict = as_sarah(|| gate.verdict("exec.run", &effect));
            assert!(matches!(verdict, Verdict::Ask { .. }));
            let decision = Decision {
                effect,
                verdict,
                reach: None,
            };
            for outcome in [Outcome::Allowed, Outcome::Declined, Outcome::NotAnswered] {
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
            }
        })
        .await;
    }
}
