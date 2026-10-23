//! `aws.*` — read-only AWS tools. `aws.s3.*` (list / head / get / collect) is
//! gated by a per-stack bucket allowlist; the `infra` submodule adds ECS/ECR/
//! ELBv2/RDS/STS diagnosis tools plus CloudWatch Logs and metrics. Both run under dedicated `sts:AssumeRole`'d
//! roles sharing one [`client::SdkConfigFactory`].

pub mod client;
pub mod collect;
pub mod content;
pub mod expand;
pub mod get;
pub mod head;
pub mod infra;
pub mod list;

pub use client::{
    Allowlist, AwsS3ClientFactory, GetResponse, HeadResponse, ListResponse, ListedKey, S3Backend,
    S3Error, SdkConfigFactory, SdkS3Backend,
};
pub use collect::AwsS3CollectTool;
pub use get::AwsS3GetTool;
pub use head::AwsS3HeadTool;
pub use infra::{
    AwsCloudwatchGetMetricDataTool, AwsDeployStatusTool, AwsEcrDescribeImagesTool,
    AwsEcrListRepositoriesTool, AwsEcsDescribeContainerInstancesTool, AwsEcsDescribeServicesTool,
    AwsEcsDescribeTaskDefinitionTool, AwsEcsDescribeTasksTool, AwsEcsListClustersTool,
    AwsEcsListServicesTool, AwsEcsListTasksTool, AwsElbDescribeTargetHealthTool,
    AwsLogsDescribeLogGroupsTool, AwsLogsFilterEventsTool, AwsRdsDescribeDbClustersTool,
    AwsRdsDescribeEventsTool, AwsStsGetCallerIdentityTool, InfraBackend, SdkInfraBackend,
};
pub use list::AwsS3ListTool;

/// Shared test fixture: build an in-memory xlsx workbook so spreadsheet tests
/// need no binary files in the repo.
#[cfg(test)]
pub(crate) fn test_workbook(build: impl FnOnce(&mut rust_xlsxwriter::Workbook)) -> bytes::Bytes {
    let mut wb = rust_xlsxwriter::Workbook::new();
    build(&mut wb);
    bytes::Bytes::from(wb.save_to_buffer().expect("save workbook"))
}

use std::collections::HashSet;

use workforce_common::settings::AwsS3Config;
use workforce_policy::rules::{
    ConditionOperator, PolicyRule, PolicyScope, RuleAction, RuleCondition,
};

/// Metadata-only tools: plain `Allow` (no body bytes ever leave S3).
const METADATA_TOOLS: &[&str] = &["aws.s3.list", "aws.s3.head"];
/// Body-reading tools: subject to the `require_approval_buckets → Ask` gate,
/// since they surface object contents (potential PII).
const BODY_TOOLS: &[&str] = &["aws.s3.get", "aws.s3.collect"];

/// Every read-only `aws.*` tool name (S3 list/head/get + the infra
/// diagnosis tools). Every `aws.*` tool is read-only by construction, so this
/// is the full registered AWS surface and the single source of truth for the
/// worker-acquirable AWS read set (see `crate::worker_read`).
pub fn read_tool_names() -> Vec<&'static str> {
    METADATA_TOOLS
        .iter()
        .copied()
        .chain(BODY_TOOLS.iter().copied())
        .chain(infra::TOOL_NAMES.iter().copied())
        .collect()
}

/// Constitutional policy rules for the `aws.s3.*` surface.
///
/// Every tool defaults to `Allow`. Access is admin-determined upstream: a
/// bucket only reaches a tool if it's on `bucket_allowlist` (and the assumed
/// role's IAM `Resource` scope), so being readable at all is already an
/// operator decision — a per-call human prompt on every object adds friction
/// without adding a control the admin doesn't already hold.
///
/// Each body-reading tool (`get` / `collect`) is a SINGLE rule with outer
/// `Allow` and one `Equals → Ask` condition per bucket in
/// `require_approval_buckets` — the opt-in inverse. The policy engine selects
/// the rule by tool+scope+priority; `evaluate_conditions` walks the conditions
/// in order and returns the matching condition's action, else falls back to
/// the rule's outer action (`engine.rs:178-192`). So reading a body from a
/// bucket the operator flagged for approval returns `Ask`; any other
/// allowlisted bucket returns `Allow`. Metadata-only tools carry no conditions.
///
/// Require-approval entries are silently filtered to those also in
/// `bucket_allowlist`: a typo or stale entry naming a bucket the tool
/// would refuse outright must not appear in the policy audit log as an
/// `Ask` decision (it would diverge from the tool's actual refusal). The
/// caller is expected to `warn!` about dropped entries — see
/// [`require_approval_entries_outside_allowlist`].
pub fn default_policies(config: &AwsS3Config) -> Vec<PolicyRule> {
    let allowlist: HashSet<&str> = config.bucket_allowlist.iter().map(String::as_str).collect();
    let conditions: Vec<RuleCondition> = config
        .require_approval_buckets
        .iter()
        .filter(|b| allowlist.contains(b.as_str()))
        .map(|bucket| RuleCondition {
            field: "bucket".to_string(),
            operator: ConditionOperator::Equals,
            value: serde_json::Value::String(bucket.clone()),
            action: RuleAction::Ask,
        })
        .collect();

    let allow_rule = |tool: &str, conditions: Vec<RuleCondition>| PolicyRule {
        name: format!("{tool}-allow-default"),
        scope: PolicyScope::Global,
        tool: tool.to_string(),
        action: RuleAction::Allow,
        conditions,
        priority: 10,
        constitutional: true,
    };

    METADATA_TOOLS
        .iter()
        .map(|tool| allow_rule(tool, Vec::new()))
        .chain(
            BODY_TOOLS
                .iter()
                .map(|tool| allow_rule(tool, conditions.clone())),
        )
        .collect()
}

/// Return the require-approval entries that aren't in the bucket allowlist —
/// callers `warn!` about each so an operator typo (`prod-y` vs `prod-x`)
/// surfaces at startup rather than as silent policy / tool divergence.
pub fn require_approval_entries_outside_allowlist(config: &AwsS3Config) -> Vec<String> {
    let allowlist: HashSet<&str> = config.bucket_allowlist.iter().map(String::as_str).collect();
    config
        .require_approval_buckets
        .iter()
        .filter(|b| !allowlist.contains(b.as_str()))
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_core::policy::{PolicyContext, PolicyDecision, PolicyRequest};
    use workforce_core::types::AgentId;
    use workforce_policy::engine::PolicyEngine;
    use workforce_policy::rules::PolicyConfig;

    #[test]
    fn default_policies_ship_allow_for_list_and_head() {
        let rules = default_policies(&AwsS3Config::default());
        let list = rules.iter().find(|r| r.tool == "aws.s3.list").unwrap();
        let head = rules.iter().find(|r| r.tool == "aws.s3.head").unwrap();
        assert_eq!(list.action, RuleAction::Allow);
        assert_eq!(head.action, RuleAction::Allow);
        assert!(list.constitutional);
    }

    #[test]
    fn default_policies_default_get_to_allow_single_rule() {
        let rules = default_policies(&AwsS3Config::default());
        let get_rules: Vec<&PolicyRule> = rules.iter().filter(|r| r.tool == "aws.s3.get").collect();
        assert_eq!(
            get_rules.len(),
            1,
            "get is always one rule; require-approval buckets go on its conditions",
        );
        assert_eq!(get_rules[0].action, RuleAction::Allow);
        assert_eq!(get_rules[0].priority, 10);
        assert!(get_rules[0].conditions.is_empty());
    }

    #[test]
    fn default_policies_get_carries_one_condition_per_require_approval_bucket() {
        let cfg = AwsS3Config {
            bucket_allowlist: vec!["staging".to_string(), "prod".to_string()],
            require_approval_buckets: vec!["prod".to_string()],
            ..AwsS3Config::default()
        };
        let rules = default_policies(&cfg);
        let get = rules.iter().find(|r| r.tool == "aws.s3.get").unwrap();
        assert_eq!(
            get.action,
            RuleAction::Allow,
            "outer fallback must be Allow"
        );
        assert_eq!(get.conditions.len(), 1);
        assert_eq!(get.conditions[0].field, "bucket");
        assert_eq!(
            get.conditions[0].value,
            serde_json::Value::String("prod".to_string())
        );
        assert_eq!(get.conditions[0].action, RuleAction::Ask);
    }

    #[test]
    fn collect_is_a_body_tool_sharing_the_get_approval_gate() {
        let cfg = AwsS3Config {
            bucket_allowlist: vec!["prod".to_string()],
            require_approval_buckets: vec!["prod".to_string()],
            ..AwsS3Config::default()
        };
        let rules = default_policies(&cfg);
        let collect = rules.iter().find(|r| r.tool == "aws.s3.collect").unwrap();
        let get = rules.iter().find(|r| r.tool == "aws.s3.get").unwrap();
        assert_eq!(collect.action, RuleAction::Allow);
        assert_eq!(
            collect.conditions.len(),
            get.conditions.len(),
            "collect reads bodies, so it carries the same require-approval gate as get",
        );
        assert_eq!(collect.conditions[0].field, "bucket");
        assert_eq!(
            collect.conditions[0].value,
            serde_json::Value::String("prod".to_string())
        );
        assert_eq!(collect.conditions[0].action, RuleAction::Ask);
        let list = rules.iter().find(|r| r.tool == "aws.s3.list").unwrap();
        assert!(
            list.conditions.is_empty(),
            "metadata-only tools never carry the body approval gate",
        );
    }

    fn engine_with(rules: Vec<PolicyRule>) -> PolicyEngine {
        let base = PolicyEngine::new(PolicyConfig { rules: Vec::new() });
        base.with_constitutional_rules(rules)
    }

    fn get_request(bucket: &str) -> PolicyRequest {
        PolicyRequest {
            tool_name: "aws.s3.get".to_string(),
            agent_id: AgentId::new("atlas"),
            parameters: serde_json::json!({ "bucket": bucket, "key": "trust/main.csv" }),
            context: PolicyContext::empty(),
        }
    }

    #[test]
    fn engine_returns_ask_for_require_approval_bucket_and_allow_for_others() {
        let cfg = AwsS3Config {
            bucket_allowlist: vec!["staging".to_string(), "prod".to_string()],
            require_approval_buckets: vec!["prod".to_string()],
            ..AwsS3Config::default()
        };
        let engine = engine_with(default_policies(&cfg));

        let prod = engine.evaluate(&get_request("prod")).decision;
        assert!(
            matches!(prod, PolicyDecision::RequireApproval { .. }),
            "require-approval bucket must prompt: {prod:?}",
        );

        let staging = engine.evaluate(&get_request("staging")).decision;
        assert!(
            matches!(staging, PolicyDecision::Allow),
            "every other allowlisted bucket flows without a prompt: {staging:?}",
        );
    }

    #[test]
    fn engine_asks_for_collect_on_require_approval_bucket_like_get() {
        let cfg = AwsS3Config {
            bucket_allowlist: vec!["staging".to_string(), "prod".to_string()],
            require_approval_buckets: vec!["prod".to_string()],
            ..AwsS3Config::default()
        };
        let engine = engine_with(default_policies(&cfg));
        let request = |bucket: &str| PolicyRequest {
            tool_name: "aws.s3.collect".to_string(),
            agent_id: AgentId::new("atlas"),
            parameters: serde_json::json!({ "bucket": bucket, "keys": ["a/main.csv"] }),
            context: PolicyContext::empty(),
        };
        assert!(
            matches!(
                engine.evaluate(&request("prod")).decision,
                PolicyDecision::RequireApproval { .. }
            ),
            "collect on a require-approval bucket must prompt, exactly like get",
        );
        assert!(matches!(
            engine.evaluate(&request("staging")).decision,
            PolicyDecision::Allow
        ));
    }

    #[test]
    fn engine_returns_allow_for_every_bucket_when_no_require_approval_configured() {
        let cfg = AwsS3Config {
            bucket_allowlist: vec!["staging".to_string(), "prod".to_string()],
            ..AwsS3Config::default()
        };
        let engine = engine_with(default_policies(&cfg));
        for bucket in ["staging", "prod"] {
            let decision = engine.evaluate(&get_request(bucket)).decision;
            assert!(
                matches!(decision, PolicyDecision::Allow),
                "{bucket}: {decision:?}",
            );
        }
    }

    #[test]
    fn require_approval_entries_filtered_to_bucket_allowlist() {
        let cfg = AwsS3Config {
            bucket_allowlist: vec!["prod".to_string()],
            require_approval_buckets: vec!["prod".to_string(), "typo-bucket".to_string()],
            ..AwsS3Config::default()
        };
        let rules = default_policies(&cfg);
        let get = rules.iter().find(|r| r.tool == "aws.s3.get").unwrap();
        assert_eq!(
            get.conditions.len(),
            1,
            "typo entry must not produce a policy rule",
        );
        let dropped = require_approval_entries_outside_allowlist(&cfg);
        assert_eq!(dropped, vec!["typo-bucket".to_string()]);
    }
}
