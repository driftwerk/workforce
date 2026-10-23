//! `budget` tool — live spend visibility for everyone, runtime ceiling
//! overrides for SysAdmins.
//!
//! `status` answers "how much have I spent / have left, and what are the
//! agents up to" from the governor's live figures. `set_override` /
//! `clear_override` are the agent-reachable half of the runtime override
//! layer (see [`crate::budget_overrides`]): SysAdmin-gated via the standard
//! `IsAdminFn`, audited, optionally expiring — so "give this user $150 for
//! today" works from a Slack thread and lands within seconds.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use eyre::Result;
use serde::Deserialize;
use workforce_agent::tools::{Tool, ToolResult};

use crate::authz::IsAdminFn;
use crate::budget_overrides::{BudgetOverride, OverrideScope};
use crate::cost_governor::{BudgetStatus, ScopeStatus};
use crate::serve::AgentService;

pub struct BudgetTool {
    service: Arc<AgentService>,
    is_admin: IsAdminFn,
}

impl BudgetTool {
    pub fn new(service: Arc<AgentService>) -> Self {
        Self {
            service,
            is_admin: crate::vault_tool::always_admin(),
        }
    }

    pub fn with_admin_policy(mut self, is_admin: IsAdminFn) -> Self {
        self.is_admin = is_admin;
        self
    }

    fn agent_ids(&self) -> Vec<String> {
        self.service
            .agent_registry
            .get()
            .and_then(|w| w.upgrade())
            .map(|r| r.agent_ids().iter().map(|a| a.to_string()).collect())
            .unwrap_or_else(|| vec![self.service.agent_id.to_string()])
    }
}

#[derive(Deserialize)]
struct BudgetInput {
    action: String,
    /// Override target: "hub", "agent", or "identity".
    #[serde(default)]
    scope: Option<String>,
    /// Agent id or identity uuid the override applies to (omit for hub).
    #[serde(default)]
    key: Option<String>,
    /// New daily ceiling in USD. 0 means unlimited.
    #[serde(default)]
    daily_usd: Option<f64>,
    /// "today" (until midnight in the workspace timezone), a number of
    /// hours, or omitted for no expiry.
    #[serde(default)]
    expires: Option<serde_json::Value>,
    #[serde(default)]
    note: Option<String>,
}

#[async_trait]
impl Tool for BudgetTool {
    fn name(&self) -> &str {
        "budget"
    }

    fn description(&self) -> &str {
        "Show live LLM budget status (spend, limits, remaining — per agent and for your account). \
         SysAdmins can also set or clear runtime ceiling overrides that take effect immediately."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["status", "set_override", "clear_override"],
                    "description": "status: spend vs limits. set_override/clear_override: SysAdmin only."
                },
                "scope": {
                    "type": "string",
                    "enum": ["hub", "agent", "identity"],
                    "description": "Override target scope (set/clear only)."
                },
                "key": {
                    "type": "string",
                    "description": "Agent id or identity uuid the override targets. Omit for hub scope."
                },
                "daily_usd": {
                    "type": "number",
                    "description": "New daily ceiling in USD (set_override). 0 = unlimited."
                },
                "expires": {
                    "description": "\"today\" = until midnight in the workspace timezone; a number = hours from now; omit = until cleared."
                },
                "note": {"type": "string", "description": "Why the override was granted."}
            },
            "required": ["action"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> Result<ToolResult> {
        let start = Instant::now();
        let params: BudgetInput = serde_json::from_value(input)?;
        let elapsed = || start.elapsed().as_millis() as u64;
        let admin = (self.is_admin)();

        match params.action.as_str() {
            "status" => {
                let identity = crate::user_config::current_identity_id();
                let status = self.service.cost_governor.status(
                    &self.agent_ids(),
                    identity.as_deref(),
                    admin,
                )?;
                Ok(ToolResult::success(
                    serde_json::Value::String(render_status(&status, admin)),
                    elapsed(),
                ))
            }
            "set_override" => {
                if !admin {
                    return Ok(ToolResult::failure(
                        "budget set_override requires the system admin role".to_string(),
                        elapsed(),
                    ));
                }
                let Some(store) = crate::budget_overrides::global_store() else {
                    return Ok(ToolResult::failure(
                        "no budget override store is configured".to_string(),
                        elapsed(),
                    ));
                };
                let (scope, key) = match parse_target(&params) {
                    Ok(t) => t,
                    Err(msg) => return Ok(ToolResult::failure(msg, elapsed())),
                };
                if scope == OverrideScope::Identity
                    && self
                        .service
                        .cost_governor
                        .config()
                        .exempt_identities
                        .contains(&key)
                {
                    return Ok(ToolResult::failure(
                        format!("identity `{key}` is exempt from budgets — no override needed"),
                        elapsed(),
                    ));
                }
                let Some(daily_usd) = params.daily_usd.filter(|v| v.is_finite() && *v >= 0.0)
                else {
                    return Ok(ToolResult::failure(
                        "set_override needs `daily_usd` >= 0 (0 = unlimited)".to_string(),
                        elapsed(),
                    ));
                };
                let expires_at = match parse_expiry(params.expires.as_ref(), &self.service.config) {
                    Ok(e) => e,
                    Err(msg) => return Ok(ToolResult::failure(msg, elapsed())),
                };
                let actor = crate::user_config::current_identity_id()
                    .unwrap_or_else(|| "unknown-admin".to_string());
                let row = BudgetOverride {
                    scope,
                    key: key.clone(),
                    daily_usd,
                    expires_at,
                    note: params.note.clone(),
                    created_by: actor,
                    updated_at: Utc::now(),
                };
                store.upsert(&row)?;
                let target = if key.is_empty() {
                    "the whole workspace".to_string()
                } else {
                    format!("{} `{key}`", scope.as_str())
                };
                let until = expires_at
                    .map(|e| format!(" until {}", e.format("%Y-%m-%d %H:%M UTC")))
                    .unwrap_or_else(|| " until cleared".to_string());
                Ok(ToolResult::success(
                    serde_json::Value::String(format!(
                        "Daily budget for {target} is now ${daily_usd:.2}{until}. \
                         Takes effect within a few seconds."
                    )),
                    elapsed(),
                ))
            }
            "clear_override" => {
                if !admin {
                    return Ok(ToolResult::failure(
                        "budget clear_override requires the system admin role".to_string(),
                        elapsed(),
                    ));
                }
                let Some(store) = crate::budget_overrides::global_store() else {
                    return Ok(ToolResult::failure(
                        "no budget override store is configured".to_string(),
                        elapsed(),
                    ));
                };
                let (scope, key) = match parse_target(&params) {
                    Ok(t) => t,
                    Err(msg) => return Ok(ToolResult::failure(msg, elapsed())),
                };
                let actor = crate::user_config::current_identity_id()
                    .unwrap_or_else(|| "unknown-admin".to_string());
                if store.remove(scope, &key, &actor)? {
                    Ok(ToolResult::success(
                        serde_json::Value::String(format!(
                            "Override cleared — {} `{}` is back on its configured ceiling.",
                            scope.as_str(),
                            if key.is_empty() { "hub" } else { &key }
                        )),
                        elapsed(),
                    ))
                } else {
                    Ok(ToolResult::failure(
                        "no override exists for that scope/key".to_string(),
                        elapsed(),
                    ))
                }
            }
            other => Ok(ToolResult::failure(
                format!("unknown action `{other}` — use status, set_override or clear_override"),
                elapsed(),
            )),
        }
    }
}

fn parse_target(params: &BudgetInput) -> Result<(OverrideScope, String), String> {
    let scope = params
        .scope
        .as_deref()
        .and_then(OverrideScope::parse)
        .ok_or("`scope` must be one of hub, agent, identity")?;
    let key = params.key.clone().unwrap_or_default();
    if scope != OverrideScope::Hub && key.is_empty() {
        return Err(format!(
            "`key` (the {} id) is required for scope `{}`",
            if scope == OverrideScope::Agent {
                "agent"
            } else {
                "identity uuid"
            },
            scope.as_str()
        ));
    }
    Ok((
        scope,
        if scope == OverrideScope::Hub {
            String::new()
        } else {
            key
        },
    ))
}

pub fn parse_expiry(
    expires: Option<&serde_json::Value>,
    config: &workforce_common::settings::WorkforceConfig,
) -> Result<Option<chrono::DateTime<Utc>>, String> {
    let Some(v) = expires else { return Ok(None) };
    if let Some(hours) = v.as_f64() {
        if !hours.is_finite() || hours <= 0.0 || hours > 24.0 * 366.0 {
            return Err("`expires` hours must be between 0 and 8784 (one year)".to_string());
        }
        return Ok(Some(
            Utc::now() + chrono::Duration::seconds((hours * 3600.0) as i64),
        ));
    }
    match v.as_str() {
        Some("today") => {
            let tz = config.user_timezone();
            let tomorrow = Utc::now()
                .with_timezone(&tz)
                .date_naive()
                .succ_opt()
                .and_then(|d| d.and_hms_opt(0, 0, 0))
                .ok_or("could not compute midnight")?;
            let expires = tz
                .from_local_datetime(&tomorrow)
                .earliest()
                .ok_or("could not compute midnight in the workspace timezone")?
                .with_timezone(&Utc);
            Ok(Some(expires))
        }
        Some(other) => Err(format!(
            "`expires` must be \"today\" or a number of hours, got `{other}`"
        )),
        None => Err("`expires` must be \"today\" or a number of hours".to_string()),
    }
}

fn money(v: f64) -> String {
    format!("${v:.2}")
}

fn render_scope_line(s: &ScopeStatus, label: &str) -> String {
    let limit = if s.unlimited {
        "unlimited".to_string()
    } else {
        money(s.limit_usd)
    };
    let remaining = if s.unlimited {
        String::new()
    } else {
        format!(", {} left", money((s.limit_usd - s.spent_usd).max(0.0)))
    };
    let flag = if s.overridden {
        match s.override_expires_at {
            Some(e) => format!(" [override until {}]", e.format("%H:%M UTC")),
            None => " [override]".to_string(),
        }
    } else {
        String::new()
    };
    format!(
        "- {label}: {} spent of {limit}{remaining}{flag}",
        money(s.spent_usd)
    )
}

fn render_status(status: &BudgetStatus, admin: bool) -> String {
    if !status.enabled {
        return "Budget governance is disabled — no ceilings are enforced.".to_string();
    }
    let mut out = format!(
        "LLM budget for {} (spend updates within ~30s):\n",
        status.day
    );
    out.push_str(&render_scope_line(&status.hub, "Workspace"));
    out.push('\n');
    out.push_str("\nAgents:\n");
    for a in &status.agents {
        out.push_str(&render_scope_line(a, &a.key));
        out.push('\n');
    }
    if !status.identities.is_empty() {
        out.push_str(if admin {
            "\nIdentities with spend today:\n"
        } else {
            "\nYour account:\n"
        });
        for i in &status.identities {
            let label = if admin { i.key.as_str() } else { "you" };
            out.push_str(&render_scope_line(i, label));
            out.push('\n');
        }
    }
    if admin && !status.overrides.is_empty() {
        out.push_str("\nActive overrides:\n");
        for o in &status.overrides {
            let until = o
                .expires_at
                .map(|e| format!(" until {}", e.format("%Y-%m-%d %H:%M UTC")))
                .unwrap_or_else(|| " until cleared".to_string());
            let note = o
                .note
                .as_deref()
                .map(|n| format!(" — {n}"))
                .unwrap_or_default();
            out.push_str(&format!(
                "- {} `{}`: {}{until} (by {}){note}\n",
                o.scope.as_str(),
                if o.key.is_empty() { "hub" } else { &o.key },
                money(o.daily_usd),
                o.created_by,
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_expiry_hours_and_today() {
        let cfg = workforce_common::settings::WorkforceConfig::default();
        let h = parse_expiry(Some(&serde_json::json!(2)), &cfg)
            .unwrap()
            .unwrap();
        let dt = h - Utc::now();
        assert!(dt > chrono::Duration::minutes(119) && dt <= chrono::Duration::minutes(120));

        let t = parse_expiry(Some(&serde_json::json!("today")), &cfg)
            .unwrap()
            .unwrap();
        assert!(t > Utc::now());
        assert!(t <= Utc::now() + chrono::Duration::hours(24));

        assert!(parse_expiry(None, &cfg).unwrap().is_none());
        assert!(parse_expiry(Some(&serde_json::json!("never")), &cfg).is_err());
        assert!(parse_expiry(Some(&serde_json::json!(-1)), &cfg).is_err());
        assert!(parse_expiry(Some(&serde_json::json!(1e15)), &cfg).is_err());
    }

    #[test]
    fn parse_target_validation() {
        let mk = |scope: Option<&str>, key: Option<&str>| BudgetInput {
            action: "set_override".to_string(),
            scope: scope.map(str::to_string),
            key: key.map(str::to_string),
            daily_usd: Some(1.0),
            expires: None,
            note: None,
        };
        assert!(parse_target(&mk(None, None)).is_err());
        assert!(parse_target(&mk(Some("agent"), None)).is_err());
        assert!(parse_target(&mk(Some("identity"), None)).is_err());
        let (s, k) = parse_target(&mk(Some("hub"), Some("ignored"))).unwrap();
        assert_eq!(s, OverrideScope::Hub);
        assert!(k.is_empty(), "hub scope normalises the key away");
        let (s, k) = parse_target(&mk(Some("agent"), Some("atlas"))).unwrap();
        assert_eq!(s, OverrideScope::Agent);
        assert_eq!(k, "atlas");
    }

    #[test]
    fn render_status_shapes() {
        let mk = |key: &str, spent: f64, limit: f64, overridden: bool| ScopeStatus {
            key: key.to_string(),
            spent_usd: spent,
            limit_usd: limit,
            unlimited: limit <= 0.0,
            overridden,
            override_expires_at: None,
        };
        let status = BudgetStatus {
            enabled: true,
            day: chrono::NaiveDate::from_ymd_opt(2026, 8, 19).unwrap(),
            hub: mk("", 12.5, 2000.0, false),
            agents: vec![mk("atlas", 10.0, 1200.0, false)],
            identities: vec![mk("u-1", 47.9, 150.0, true)],
            overrides: vec![],
        };
        let user_view = render_status(&status, false);
        assert!(user_view.contains("Your account"));
        assert!(user_view.contains("$47.90 spent of $150.00"));
        assert!(user_view.contains("[override]"));
        let admin_view = render_status(&status, true);
        assert!(admin_view.contains("Identities with spend today"));
        assert!(admin_view.contains("u-1"));
    }
}
