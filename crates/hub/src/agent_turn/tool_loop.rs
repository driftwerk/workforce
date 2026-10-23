//! Tool loops that answer a question with a handful of read tools —
//! claim grounding, conclusion review, the deep-expertise gate — run on the
//! agent's routes like its turns: on a runner route through the in-process
//! loop, on a CLI route as a contained CLI round.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use workforce_agent::llm::provider::LlmProvider;
use workforce_agent::llm::types::{ContentBlock, TokenUsage};
use workforce_agent::runner::CURRENT_USER_ID;
use workforce_agent::tools::registry::ToolRegistry;
use workforce_common::subscription::Served;

use super::cli::{CliAttempt, CliTurn};
use super::{
    answered, rig_or_miss, route_provider, Oversight, TurnChain, TurnRoute, TurnUnavailable,
    AGENT_TURN_TIMEOUT,
};
use crate::advisor_loop::{run_advisor_tool_loop, AdvisorLoopOutcome, AdvisorLoopRequest};
use crate::serve::AgentService;
use crate::turn_loop::tools::TurnTools;

/// Called with each call's usage, the model it ran on and the plan that paid.
pub type UsageFn<'a> = dyn Fn(&TokenUsage, &str, Option<&Served>) + Send + Sync + 'a;

/// A loop that answers a request with its tools.
#[async_trait]
pub trait ToolLoop: Send + Sync {
    async fn run(&self, request: AdvisorLoopRequest, on_usage: &UsageFn<'_>) -> AdvisorLoopOutcome;
}

/// The in-process loop on one provider.
pub struct ProviderLoop(pub Arc<dyn LlmProvider>);

#[async_trait]
impl ToolLoop for ProviderLoop {
    async fn run(&self, request: AdvisorLoopRequest, on_usage: &UsageFn<'_>) -> AdvisorLoopOutcome {
        run_advisor_tool_loop(self.0.as_ref(), request, |usage, model, served| {
            on_usage(usage, model, served)
        })
        .await
    }
}

/// The loop on an agent's routes, for a chain with a CLI route.
pub struct ChainLoop {
    chain: TurnChain,
    service: Arc<AgentService>,
}

#[async_trait]
impl ToolLoop for ChainLoop {
    async fn run(&self, request: AdvisorLoopRequest, on_usage: &UsageFn<'_>) -> AdvisorLoopOutcome {
        self.chain
            .run_loop(&self.service, request, on_usage, Oversight::Unattended)
            .await
    }
}

/// The loop a request on `spec` — else the agent's default model — runs
/// on: the agent's own provider when every route of the chain is a
/// runner's, else the chain's routes.
pub(crate) async fn tool_loop(
    service: &Arc<AgentService>,
    spec: Option<&str>,
) -> Arc<dyn ToolLoop> {
    match loop_routes(service, spec).await {
        None => Arc::new(ProviderLoop(Arc::clone(&service.llm))),
        Some(chain) => Arc::new(ChainLoop {
            chain,
            service: Arc::clone(service),
        }),
    }
}

/// The routes of `spec`, else of the agent's default model, when any of
/// them is a CLI's; `None` when the agent's provider serves them all.
pub(crate) async fn loop_routes(service: &AgentService, spec: Option<&str>) -> Option<TurnChain> {
    let spec = spec.map_or_else(
        || service.current_default_model.read().clone(),
        str::to_string,
    );
    Some(crate::turn_loop::turn_chain(service, &spec).await).filter(|chain| !chain.runner_only())
}

impl TurnChain {
    /// Answer `request` on the chain's routes in order, until one serves it,
    /// a call needing approval on a CLI route answered under `oversight`.
    pub(crate) async fn run_loop(
        &self,
        service: &AgentService,
        request: AdvisorLoopRequest,
        on_usage: &UsageFn<'_>,
        oversight: Oversight,
    ) -> AdvisorLoopOutcome {
        let mut misses = Vec::new();
        for route in &self.routes {
            if request
                .cancel
                .as_ref()
                .is_some_and(|cancel| cancel.is_cancelled())
            {
                return AdvisorLoopOutcome::Cancelled;
            }
            match route {
                TurnRoute::Runner(raws) => {
                    let (provider, slice) = route_provider(service, raws);
                    let request = AdvisorLoopRequest {
                        model: raws.first().cloned(),
                        ..request.clone()
                    };
                    match ProviderLoop(provider).run(request, on_usage).await {
                        AdvisorLoopOutcome::ProviderError(reason)
                            if !answered(slice.as_deref()) =>
                        {
                            misses.push(route.miss(reason))
                        }
                        outcome => return outcome,
                    }
                }
                TurnRoute::Cli { cli, model } => {
                    let rig = match rig_or_miss(service, route) {
                        Ok(rig) => rig,
                        Err(miss) => {
                            misses.push(miss);
                            continue;
                        }
                    };
                    let started = Instant::now();
                    let timeout = request.budget.map_or(AGENT_TURN_TIMEOUT, |budget| {
                        (budget.research_deadline + budget.write_up_reserve)
                            .saturating_duration_since(started)
                    });
                    let principal = CURRENT_USER_ID
                        .try_with(String::clone)
                        .unwrap_or_else(|_| format!("agent:{}", service.agent_id.as_str()));
                    let turn = CliTurn {
                        cli: *cli,
                        model: model.clone(),
                        system_prompt: request.system_prompt.clone(),
                        task: vec![ContentBlock::Text {
                            text: request.user_message.clone(),
                        }],
                        tools: TurnTools {
                            registry: Arc::new(ToolRegistry::from_tools(
                                request.tools.iter().map(Arc::clone),
                            )),
                            exclusive: true,
                        },
                        max_turns: request.max_rounds.max(1),
                        principal,
                        oversight,
                        self_serve: workforce_agent::tools::approval::SelfServe::Answerable,
                        journal: None,
                        progress: None,
                        cancel: request.cancel.clone().unwrap_or_default(),
                        timeout,
                    };
                    match rig.run(service, turn, None, None).await {
                        Ok(CliAttempt::Served(served)) => {
                            on_usage(&served.usage, &served.model, Some(&served.served));
                            return AdvisorLoopOutcome::Success {
                                text: served.text,
                                model: served.model,
                                duration_ms: started.elapsed().as_millis() as u64,
                                truncated: false,
                                capped: served.capped,
                                rounds_used: served.rounds,
                            };
                        }
                        Ok(CliAttempt::Unserved(miss)) => misses.push(miss),
                        Err(_)
                            if request
                                .cancel
                                .as_ref()
                                .is_some_and(|cancel| cancel.is_cancelled()) =>
                        {
                            return AdvisorLoopOutcome::Cancelled
                        }
                        Err(e) => return AdvisorLoopOutcome::ProviderError(format!("{e:#}")),
                    }
                }
            }
        }
        AdvisorLoopOutcome::ProviderError(TurnUnavailable { misses }.to_string())
    }
}
