//! Prices the desk's ledger with the hub's own price table: the one
//! `default_pricing.toml`, workspace overrides and `pricing.update` build
//! in an agent's `CostTracker`, so agents and sessions are priced alike.

use std::sync::Arc;

use workforce_common::cost::CostTracker;
use workforce_desk::engine::Engine;
use workforce_desk::ledger::{Billing, Pricer, Tokens};

use crate::agent_registry::AgentRegistry;

/// The hub's default agent's price table, read at each call so a price
/// changed with `pricing.update` applies from then on.
pub struct HubPricer {
    tracker: Box<dyn Fn() -> Option<Arc<CostTracker>> + Send + Sync>,
}

impl HubPricer {
    pub fn new(registry: Arc<AgentRegistry>) -> Self {
        Self {
            tracker: Box::new(move || {
                registry
                    .default_agent()
                    .map(|service| Arc::clone(&service.cost_tracker))
            }),
        }
    }

    #[cfg(test)]
    fn with(tracker: Arc<CostTracker>) -> Self {
        Self {
            tracker: Box::new(move || Some(Arc::clone(&tracker))),
        }
    }
}

/// The rows of the table a call of `engine` on `model` may be priced
/// under, the closest first. Codex's own rows are keyed by how it was paid
/// (`codex:` on a ChatGPT plan, `codex_api:` on a key); Claude Code names
/// its model as the API does, sometimes with a context tag (`[1m]`).
fn keys(engine: Engine, billing: Billing, model: &str) -> Vec<String> {
    let bare = model.split('[').next().unwrap_or(model).trim();
    match engine {
        Engine::Codex => {
            let paid = match billing {
                Billing::Key => ["codex_api", "codex"],
                Billing::Plan | Billing::Vendor | Billing::Unknown => ["codex", "codex_api"],
            };
            paid.iter()
                .map(|p| format!("{p}:{bare}"))
                .chain([bare.to_string(), format!("openai/{bare}")])
                .collect()
        }
        Engine::Claude
        | Engine::Gemini
        | Engine::Copilot
        | Engine::OpenCode
        | Engine::Cursor
        | Engine::Devin
        | Engine::Custom => vec![bare.to_string()],
    }
}

impl Pricer for HubPricer {
    /// `None` for a model the table has no row for: the tracker's own
    /// fallback guesses Sonnet-tier rates, which the ledger never shows as
    /// a price.
    fn usd(&self, engine: Engine, billing: Billing, model: &str, t: &Tokens) -> Option<f64> {
        let tracker = (self.tracker)()?;
        let key = keys(engine, billing, model)
            .into_iter()
            .find(|k| tracker.is_priced(k))?;
        Some(tracker.calculate_cost(&key, t.input, t.output, t.cache_write, t.cache_read))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn table() -> Arc<CostTracker> {
        let tracker = CostTracker::new();
        tracker.load_pricing_toml(workforce_agent::llm::DEFAULT_PRICING_TOML, None);
        Arc::new(tracker)
    }

    #[test]
    fn a_claude_call_is_priced_at_its_models_list_rates() {
        let pricer = HubPricer::with(table());
        let t = Tokens {
            input: 1_000_000,
            output: 1_000_000,
            cache_read: 1_000_000,
            cache_write: 1_000_000,
            reasoning: 500_000,
        };
        let usd = pricer
            .usd(Engine::Claude, Billing::Plan, "claude-opus-5-5", &t)
            .unwrap();
        assert!((usd - (4.0 + 20.0 + 0.20 + 5.00)).abs() < 1e-9, "{usd}");
        assert_eq!(
            pricer.usd(Engine::Claude, Billing::Plan, "claude-opus-5-5[1m]", &t),
            Some(usd)
        );
    }

    #[test]
    fn a_codex_call_is_priced_under_how_it_was_paid() {
        let pricer = HubPricer::with(table());
        let t = Tokens {
            input: 1_000_000,
            cache_read: 1_000_000,
            ..Default::default()
        };
        let usd = pricer
            .usd(Engine::Codex, Billing::Plan, "gpt-5.6-sol", &t)
            .unwrap();
        assert!((usd - (4.0 + 0.40)).abs() < 1e-9, "{usd}");
        assert!(pricer
            .usd(Engine::Codex, Billing::Key, "gpt-5.6-sol", &t)
            .is_some());
    }

    #[test]
    fn a_model_the_table_doesnt_have_is_never_guessed() {
        let pricer = HubPricer::with(table());
        let t = Tokens {
            output: 1000,
            ..Default::default()
        };
        assert_eq!(
            pricer.usd(Engine::Claude, Billing::Plan, "claude-mystery-1", &t),
            None
        );
        let none = HubPricer {
            tracker: Box::new(|| None),
        };
        assert_eq!(
            none.usd(Engine::Claude, Billing::Plan, "claude-opus-5-5", &t),
            None
        );
    }
}
