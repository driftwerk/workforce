//! A person's own bank accounts, read only: `bank.*`.
//!
//! Each connection is one bank the person linked in BankSync, kept like
//! every connector's instances: `skills/bank/workspaces/<name>/{api_key,
//! bank, label, consented_at}`, picked with `workspace`, the default at
//! `skills/bank/default_workspace`. Nothing here can move money: the
//! provider is only ever sent `GET`, with a key shown to hold read scopes
//! alone. These tools are deliberately left out of
//! [`crate::adapters::read_only_adapter_tools`]: no worker reads a
//! person's bank.

pub mod banksync;
pub mod model;
pub mod provider;
pub mod tools;

use std::sync::Arc;

use workforce_agent::tools::Tool;
use workforce_common::settings::SecretStore;

use crate::adapters::utils::{InstancePaths, LiveInstances};
use banksync::{BankSync, Limiter};

pub const READ_TOOLS: &[&str] = &[
    "bank.workspaces",
    "bank.accounts",
    "bank.balances",
    "bank.transactions",
];

pub const WRITE_TOOLS: &[&str] = &[];

/// Where Desk keeps when the person last approved a bank, beside its key.
pub const CONSENTED_AT: &str = "consented_at";

pub struct BankSkill {
    clients: Arc<LiveInstances<BankSync>>,
    default: String,
}

impl BankSkill {
    /// The caller's connections in `store`, read on every call, against
    /// BankSync at `base`. All of them share one limiter: a person's banks
    /// are in one BankSync workspace, and so under one limit.
    pub fn with_store(store: Arc<dyn SecretStore>, selector: Option<String>, base: &str) -> Self {
        let limiter = Arc::new(Limiter::default());
        let base = base.to_string();
        let reads = Arc::clone(&store);
        let clients = LiveInstances::live(store, Self::instance_paths(), selector, move |name| {
            Ok(BankSync::with_store(
                Arc::clone(&reads),
                name,
                &base,
                Arc::clone(&limiter),
            ))
        });
        Self {
            clients: Arc::new(clients),
            default: "default".into(),
        }
    }

    pub fn instance_paths() -> InstancePaths {
        InstancePaths::new("skills/bank/workspaces/", "api_key", "skills/bank/api_key")
    }

    pub fn instance_source(&self) -> Arc<dyn workforce_agent::llm::environment::InstanceSource> {
        Arc::clone(&self.clients) as Arc<dyn workforce_agent::llm::environment::InstanceSource>
    }

    pub fn tools(&self) -> Vec<Arc<dyn Tool>> {
        let ctx = || tools::Ctx::new(Arc::clone(&self.clients), self.default.clone());
        vec![
            Arc::new(tools::BankWorkspacesTool(ctx())),
            Arc::new(tools::BankAccountsTool(ctx())),
            Arc::new(tools::BankBalancesTool(ctx())),
            Arc::new(tools::BankTransactionsTool(ctx())),
        ]
    }

    pub fn tool_names() -> Vec<&'static str> {
        READ_TOOLS.to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::adapters::test_support::assert_read_write_partition;

    #[test]
    fn every_tool_only_reads_and_is_named_once() {
        let skill = BankSkill::with_store(
            crate::adapters::test_support::InMem::from(&[]),
            None,
            banksync::API,
        );
        let names: Vec<String> = skill.tools().iter().map(|t| t.name().to_string()).collect();
        assert_eq!(names, BankSkill::tool_names());
        assert_read_write_partition(BankSkill::tool_names(), READ_TOOLS, WRITE_TOOLS);
    }

    #[test]
    fn no_worker_is_given_a_bank_tool() {
        let reads = crate::adapters::read_only_adapter_tools();
        assert!(reads.iter().all(|t| !t.starts_with("bank.")));
    }
}
