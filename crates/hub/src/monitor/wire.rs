//! A service's monitors, wired once it exists: the definition runtime, and
//! the engine with its live tap, polls and sweep when durable watching is on.

use std::sync::Arc;
use std::time::Duration;

use tracing::warn;
use workforce_common::settings::{AgentConfig, WorkforceConfig};

use crate::serve::AgentService;

/// Process-wide channel-monitor store (the dispatch gate), built once from
/// `[postgres]` so every agent's `MonitorEngine` shares one pool. `None` when
/// Postgres is disabled or the store can't open — monitors are then disabled.
static SHARED_MONITOR_STORE: tokio::sync::OnceCell<
    Option<Arc<workforce_orchestrator::monitor::MonitorStore>>,
> = tokio::sync::OnceCell::const_new();

async fn shared_monitor_store(
    config: &WorkforceConfig,
) -> Option<Arc<workforce_orchestrator::monitor::MonitorStore>> {
    use workforce_orchestrator::monitor::MonitorStore;
    SHARED_MONITOR_STORE
        .get_or_init(|| async {
            let pg = config.postgres.as_ref().filter(|p| p.enabled)?;
            let pool = match MonitorStore::build_pool(pg) {
                Ok(p) => p,
                Err(e) => {
                    warn!(error = ?e, "monitor pool init failed; monitors disabled");
                    return None;
                }
            };
            match MonitorStore::new(pool).await {
                Ok(store) => Some(Arc::new(store)),
                Err(e) => {
                    warn!(error = ?e, "monitor store init failed; monitors disabled");
                    None
                }
            }
        })
        .await
        .clone()
}

/// Monitor runtime + engine. The agent tool is always available;
/// durable watching (the engine, live tap, sweep and source poll) needs
/// Postgres (member-owned defs + the dispatch gate) and a workflow
/// runtime.
pub(crate) async fn wire(
    service: &Arc<AgentService>,
    config: &WorkforceConfig,
    agent_config: Option<&AgentConfig>,
    def_write_auth: &crate::def_write_auth::TurnDefWriteAuth,
    adapter_loader: &crate::wasm_tool_invoker::AdapterLoader,
) {
    let monitors_dir = std::path::PathBuf::from(&config.hub.data_dir).join("monitors");
    let monitor_runtime = Arc::new(super::MonitorRuntime::new(monitors_dir));
    let own_id = service.agent_id.as_str().to_string();
    let default_agent_id = config
        .default_agent()
        .map_or_else(|| own_id.clone(), |a| a.id.clone());
    let agent_ids: Vec<String> = config.agents.iter().map(|a| a.id.clone()).collect();
    monitor_runtime.set_agents(super::KnownAgents {
        default_id: default_agent_id.clone(),
        ids: if agent_ids.is_empty() {
            vec![own_id.clone()]
        } else {
            agent_ids
        },
    });
    let engine_agent = super::EngineAgent {
        is_default: default_agent_id == own_id,
        name: agent_config
            .and_then(|a| a.name.clone())
            .unwrap_or_else(|| own_id.clone()),
        id: own_id,
    };

    let mon_def_store = crate::workflow_runtime::shared_workflow_store(config).await;
    let mut durable: Option<Arc<super::MonitorEngine>> = None;
    if let (Some(def_store), Some(wf_runtime)) =
        (mon_def_store, service.workflow_runtime().cloned())
    {
        monitor_runtime.set_store(Arc::clone(&def_store));
        if let Err(e) = monitor_runtime.reload().await {
            warn!(error = ?e, "Failed to load monitor definitions from DB");
        }
        let store = shared_monitor_store(config).await;
        if let Some(store) = store {
            // Sources are read, and skill actions run, through the
            // gauntlet an agent call faces, as the monitor's owner.
            let invoker: Arc<dyn super::ToolInvoke> =
                Arc::new(super::GovernedInvoker::for_service(
                    Arc::clone(service),
                    Some(Arc::clone(adapter_loader)),
                ));
            let sink: Arc<dyn super::ActionSink> = Arc::new(super::WorkflowNotifySink::new(
                wf_runtime,
                Arc::clone(&service.surface_registry),
                Arc::clone(service),
                Arc::clone(&invoker),
            ));
            let engine = Arc::new(
                super::MonitorEngine::new(
                    Arc::clone(&monitor_runtime),
                    store,
                    Arc::clone(&service.surface_registry),
                    sink,
                    engine_agent,
                    super::MonitorEngineConfig::from_settings(&config.monitor),
                )
                .with_invoker(invoker)
                .with_decide(Arc::new(super::DecisionGate::new(
                    service.vault.clone(),
                    service.global_vault.clone(),
                )))
                .with_peers({
                    let registry = Arc::clone(&service.agent_registry);
                    Arc::new(move |agent: &str| {
                        registry
                            .get()
                            .and_then(|weak| weak.upgrade())
                            .and_then(|registry| registry.get(agent))
                            .and_then(|service| service.monitor_engine().cloned())
                    })
                }),
            );
            let _ = service.monitor_engine.set(engine.clone());
            durable = Some(Arc::clone(&engine));

            let (tx, rx) = tokio::sync::mpsc::channel(1024);
            let _ = service.monitor_event_tx.set(tx);
            tokio::spawn(Arc::clone(&engine).run_live(rx));
            tokio::spawn(Arc::clone(&engine).run_polls(Arc::clone(&service.leader_gate)));
            engine.ensure_monitor_cursors().await;
            engine.reconcile_personas().await;

            if let Some(sched) = service.scheduler.as_ref() {
                // Each agent's scheduler persists its jobs across restarts,
                // so registering blindly accumulates duplicate sweeps per
                // boot. Clear any existing one(s) first so exactly one
                // survives per agent — each engine sweeps only the
                // monitors its agent watches.
                for job in sched.list_jobs().await {
                    if job.name == "monitor-sweep" {
                        sched.remove_job(&job.id).await;
                    }
                }
                if let Err(e) = sched
                    .add_job_with_desc(
                        "monitor-sweep",
                        Some("Monitor catch-up sweep, expiry and recovery"),
                        crate::hub_core::schedules::Schedule::Every {
                            every_secs: 120,
                            anchor: None,
                        },
                        crate::hub_core::schedules::JobPayload::MonitorSweep,
                    )
                    .await
                {
                    tracing::warn!(error = %e, "Failed to register monitor sweep job");
                }
            }

            // DB poll for out-of-band monitor edits (mirrors workflows).
            let mon_runtime = Arc::clone(&monitor_runtime);
            let poll_engine = Arc::clone(&engine);
            tokio::spawn(async move {
                let mut last = def_store.change_marker("monitor").await.ok();
                let mut tick = tokio::time::interval(Duration::from_secs(10));
                tick.tick().await;
                loop {
                    tick.tick().await;
                    match def_store.change_marker("monitor").await {
                        Ok(cur) if Some(cur) != last => {
                            last = Some(cur);
                            if let Err(e) = mon_runtime.reload().await {
                                tracing::warn!("DB monitor reload failed: {e}");
                            } else {
                                poll_engine.reconcile_personas().await;
                            }
                        }
                        Ok(_) => {}
                        Err(e) => tracing::warn!("monitor def poll failed: {e}"),
                    }
                }
            });
        }
    }
    // Registered last so it carries the engine (when durable watching is
    // on) for the catch-up policy a save or resume applies; the runtime
    // alone suffices otherwise. The same status-aware admin gate the
    // vault tools use lets admins bind anywhere / mutate globals; the
    // binding gate verifies a non-admin can reach the channel a save
    // would watch, and wherever its action would deliver.
    service.register_tool(Arc::new(
        super::MonitorTool::new(Arc::clone(&monitor_runtime), durable)
            .with_write_auth(def_write_auth.clone())
            .with_binding_access(super::BindingAccess::new(
                Some(Arc::clone(&service.surface_registry)),
                super::linked_platform_ids({
                    let identities = Arc::clone(&service.identity_store);
                    move || identities.read().clone()
                }),
            )),
    ));
}
