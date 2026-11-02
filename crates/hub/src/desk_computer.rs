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

use std::path::PathBuf;
use std::sync::Arc;

use serde_json::json;
use tracing::warn;
use workforce_agent::runner::{CURRENT_SESSION_KEY, CURRENT_USER_ID};
use workforce_agent::tools::computer::{describe, ComputerGate, Outcome, Record};
use workforce_common::settings::{AgentWorkspaceConfig, WorkforceConfig};
use workforce_core::computer::{By, ComputerAccess, Effect, Refusal, Refused, Verdict};
use workforce_desk::computer::policy::{namespace, shown, Namespace};
use workforce_desk::computer::{Activity, AgentPolicy, CallFacts, Computer};

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
                // The rest of the withheld set (Desk's data, every agent's
                // config files) lands with the roots per level (AC3c); the
                // file tools keep off config files already.
                withheld: vec![home.join("Library/Keychains")],
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
            // "Always" answers come with grants (AC3d).
            folder: None,
        }
    }

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
        let dir = tempfile::tempdir().unwrap();
        let workspace = dir.path().join("agents/allan");
        std::fs::create_dir_all(&workspace).unwrap();
        let mut config = WorkforceConfig::default();
        config.hub.data_dir = dir.path().to_string_lossy().into_owned();
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

    /// Driftwerk and Patchwork run no `[desk]`: nothing installs the gate,
    /// and every call is decided as on main.
    #[test]
    fn a_hub_without_desk_has_no_gate_and_decides_as_before() {
        let hub = hub();
        assert!(workforce_hub_core::serve::computer_gate_for(&hub.subject).is_none());
        let tools = tools(&hub.subject.workspace);
        for (tool, input) in [
            ("exec.run", json!({"command": "git push --force"})),
            (
                "filesystem.write",
                json!({"path": "/etc/hosts", "content": "x"}),
            ),
            ("memory.recall", json!({})),
        ] {
            let outcome =
                as_sarah(|| gate_tool_call(&gates(None), &policy(), &tools, tool, &input));
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

            // Nobody to ask (a job, a workflow step): refused at once.
            let refusal = as_sarah(|| {
                unattended_refusal(Some(gate.as_ref()), &tools, "exec.run", &json!({"command": "npm test"}))
            })
            .unwrap();
            assert!(refusal.contains("nobody can be asked here"), "{refusal}");

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
            let decision = Decision { effect, verdict };
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
