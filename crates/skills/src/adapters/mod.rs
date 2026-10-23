#[cfg(feature = "github")]
pub mod github;

#[cfg(feature = "google")]
pub mod google;

#[cfg(feature = "linear")]
pub mod linear;

#[cfg(feature = "jira")]
pub mod jira;

#[cfg(feature = "datadog")]
pub mod datadog;

#[cfg(feature = "metabase")]
pub mod metabase;

#[cfg(feature = "hubspot")]
pub mod hubspot;

#[cfg(feature = "notion")]
pub mod notion;

#[cfg(feature = "slack")]
pub mod slack;

#[cfg(feature = "teams")]
pub mod teams;

#[cfg(feature = "browser")]
pub mod browser;

#[cfg(feature = "obscura")]
pub mod obscura;

#[cfg(feature = "email")]
pub mod email;

#[cfg(any(feature = "email", feature = "google"))]
pub mod outbound_mail;

#[cfg(feature = "postmark")]
pub mod postmark;

#[cfg(feature = "sendgrid")]
pub mod sendgrid;

#[cfg(feature = "xero")]
pub mod xero;

#[cfg(feature = "freeagent")]
pub mod freeagent;

#[cfg(feature = "spotify")]
pub mod spotify;

// `messaging` and `messaging_routing` depend on `tokio::sync::RwLock`, which
// is only linked when at least one adapter feature brings tokio in. Gate the
// modules to match so the crate still builds with `default = []`.
#[cfg(any(
    feature = "slack",
    feature = "teams",
    feature = "github",
    feature = "google",
    feature = "linear",
    feature = "jira",
    feature = "datadog",
    feature = "metabase",
    feature = "hubspot",
    feature = "notion",
    feature = "browser",
    feature = "obscura",
    feature = "email",
    feature = "postmark",
    feature = "sendgrid",
    feature = "xero",
    feature = "freeagent",
))]
pub mod messaging;

#[cfg(any(
    feature = "slack",
    feature = "teams",
    feature = "github",
    feature = "google",
    feature = "linear",
    feature = "jira",
    feature = "datadog",
    feature = "metabase",
    feature = "hubspot",
    feature = "notion",
    feature = "browser",
    feature = "obscura",
    feature = "email",
    feature = "postmark",
    feature = "sendgrid",
    feature = "xero",
    feature = "freeagent",
))]
pub mod messaging_routing;

pub mod utils;

pub mod auth;

/// The `[[vault:<key>]]` resolver, which lives in `workforce_agent` so the
/// core `http.*` tools can share it. Re-exported here under its original path
/// for the browser and obscura adapters.
#[cfg(any(feature = "browser", feature = "obscura"))]
pub mod credential_resolver {
    pub use workforce_agent::security::vault_refs::*;
}

#[cfg(feature = "slack")]
pub mod image_view;

#[cfg(feature = "slack")]
pub mod user_photo;

#[cfg(test)]
pub(crate) mod test_support;

/// Read-only tool names across every compiled-in service adapter.
///
/// This is adapter knowledge — "which of my tools never mutate" — and the
/// single source for callers that grant a restricted, non-mutating subset
/// (e.g. research-worker sub-agents). It does not decide which of these an
/// autonomous caller should actually receive; that judgement lives with the
/// caller (see the hub's worker allowlist).
pub fn read_only_adapter_tools() -> Vec<&'static str> {
    let mut tools: Vec<&'static str> = Vec::new();
    #[cfg(feature = "datadog")]
    tools.extend_from_slice(datadog::READ_TOOLS);
    #[cfg(feature = "jira")]
    tools.extend_from_slice(jira::READ_TOOLS);
    #[cfg(feature = "linear")]
    tools.extend_from_slice(linear::READ_TOOLS);
    #[cfg(feature = "metabase")]
    tools.extend_from_slice(metabase::READ_TOOLS);
    #[cfg(feature = "postmark")]
    tools.extend_from_slice(postmark::READ_TOOLS);
    #[cfg(feature = "sendgrid")]
    tools.extend_from_slice(sendgrid::READ_TOOLS);
    tools
}

#[cfg(all(
    test,
    feature = "datadog",
    feature = "jira",
    feature = "linear",
    feature = "metabase"
))]
mod read_only_adapter_tools_tests {
    use super::read_only_adapter_tools;

    #[test]
    fn includes_reads_and_excludes_writes() {
        let reads = read_only_adapter_tools();
        for name in [
            "datadog.logs_search",
            "datadog.logs_export",
            "jira.search",
            "linear.issue_search",
            "metabase.card_query",
        ] {
            assert!(reads.contains(&name), "{name} should be read-only");
        }
        for name in [
            "datadog.monitors_create",
            "datadog.api",
            "jira.create_issue",
            "jira.api",
            "linear.issue_create",
            "metabase.card_create",
        ] {
            assert!(!reads.contains(&name), "{name} must not be read-only");
        }
    }
}
