//! `skill.*` — the consent operations of `cmd: skill` for the remote
//! surface: grant, list, revoke, share, unshare, visibility, transfer.
//!
//! Registered only into the remote surface, never into an agent's registry,
//! for the same reason `cmd: skill` runs on user-typed text only: the caller
//! here is the user's own client under the user's token, not an agent's
//! model, so a grant made here is the person's consent. The credential value
//! is never a tool argument — `skill.grant` reads it from the caller's own
//! per-user vault subtree (`users/{uid}/…`), where `vault.set` on this same
//! surface puts it.

use std::sync::Arc;

use async_trait::async_trait;
use serde::Deserialize;

use workforce_agent::runner::CURRENT_AGENT_ID;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_core::access::{GrantLevel, OutgoingAccess, Visibility};
use workforce_skills::catalog::SkillKind;
use workforce_wasm_runtime::CredentialScope;

use crate::hub_core::grants::GrantStatus;
use crate::skill_consent::{
    skill_transfer_notes, Consent, CredentialGrant, Refusal, Revocation, ShareChange, ShareOutcome,
    SkillGrants, Surface, Target, ToolsGrant, TransferOutcome, ValueAction, VisibilityChange,
};

use super::agent_tools::{caller, resolve_agent, unwrap_or_result, AgentToolsContext};

const CONTRACT_NOTE: &str = "Grants pin the skill's consent contract (capabilities, network \
    hosts, credentials, permissions): changing it invalidates this grant until it is \
    re-granted; pure code rebuilds keep it.";

pub fn skill_tools(ctx: Arc<AgentToolsContext>) -> Vec<Arc<dyn Tool>> {
    vec![
        Arc::new(GrantTool {
            ctx: Arc::clone(&ctx),
        }),
        Arc::new(GrantsTool {
            ctx: Arc::clone(&ctx),
        }),
        Arc::new(RevokeTool {
            ctx: Arc::clone(&ctx),
        }),
        Arc::new(ShareTool {
            ctx: Arc::clone(&ctx),
            adding: true,
        }),
        Arc::new(ShareTool {
            ctx: Arc::clone(&ctx),
            adding: false,
        }),
        Arc::new(VisibilityTool {
            ctx: Arc::clone(&ctx),
        }),
        Arc::new(TransferTool { ctx }),
    ]
}

/// The caller's standing through the endpoint's agent (the default agent on
/// the plain `/mcp` route), whose vault composition and identity store the
/// consent acts through.
fn consent(ctx: &AgentToolsContext) -> Result<Consent, ToolResult> {
    let uid = caller()?;
    let agent = CURRENT_AGENT_ID
        .try_with(|a| a.clone())
        .ok()
        .filter(|a| !a.is_empty());
    let service = resolve_agent(ctx, agent.as_deref())?;
    Ok(Consent::for_service(&service, &uid, Surface::RemoteMcp))
}

fn refused(refusal: Refusal) -> ToolResult {
    ToolResult::failure(refusal.to_string(), 0)
}

fn parse<T: for<'de> Deserialize<'de>>(input: serde_json::Value) -> Result<T, ToolResult> {
    serde_json::from_value(input)
        .map_err(|e| ToolResult::failure(format!("invalid arguments: {e}"), 0))
}

fn scope_word(scope: CredentialScope) -> &'static str {
    match scope {
        CredentialScope::User => "user",
        CredentialScope::Shared => "shared",
    }
}

fn status_word(status: GrantStatus) -> &'static str {
    match status {
        GrantStatus::Valid => "valid",
        GrantStatus::Stale => "stale",
        GrantStatus::Missing => "missing",
    }
}

fn permission_word(permission: GrantLevel) -> &'static str {
    match permission {
        GrantLevel::Run => "run",
        GrantLevel::Manage => "manage",
    }
}

// ── skill.grant ───────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GrantArgs {
    skill: String,
    credential: String,
    #[serde(default)]
    shared: bool,
    #[serde(default)]
    vault_path: Option<String>,
}

/// What a `skill.grant` call asks for, once the `tools` keyword and the
/// tools-only / credential-only options have been sorted out.
#[derive(Debug, PartialEq, Eq)]
enum GrantRequest {
    Tools { shared: bool },
    Credential { name: String, vault_path: String },
}

fn grant_request(args: &GrantArgs) -> Result<GrantRequest, String> {
    let credential = args.credential.trim();
    match (credential, args.shared, args.vault_path.as_deref()) {
        ("", _, _) => {
            Err("credential is required: a declared credential name, or \"tools\"".into())
        }
        ("tools", shared, None) => Ok(GrantRequest::Tools { shared }),
        ("tools", _, Some(_)) => Err("vault_path applies to a credential, not to tools".into()),
        (_, true, _) => Err(
            "shared applies to tools only; a shared-scope credential is \
                            granted by the skill's owner without this flag"
                .into(),
        ),
        (name, false, path) => Ok(GrantRequest::Credential {
            name: name.to_string(),
            vault_path: path.map_or(name, str::trim).to_string(),
        }),
    }
}

fn no_value(uid: &str, vault_path: &str, skill: &str, credential: &str) -> ToolResult {
    ToolResult::failure(
        format!(
            "No value at users/{uid}/{vault_path}. Store it first with vault.set (path \
             \"{vault_path}\", default per-user scope) — the value never passes through \
             this tool — then call skill.grant again for `{credential}` of `{skill}`."
        ),
        0,
    )
}

fn render_credential_grant(grant: &CredentialGrant) -> serde_json::Value {
    serde_json::json!({
        "status": "granted",
        "skill": grant.skill,
        "credential": grant.credential,
        "scope": scope_word(grant.scope),
        "value": match grant.value {
            ValueAction::Stored => "stored",
            ValueAction::Kept => "kept",
        },
        "may_send_to": grant.hosts,
        "note": CONTRACT_NOTE,
    })
}

fn render_tools_grant(grant: &ToolsGrant) -> serde_json::Value {
    let mut out = serde_json::json!({
        "status": "granted",
        "skill": grant.skill,
        "tools": true,
        "coverage": if grant.shared { "everyone" } else { "you" },
        "may_invoke": grant.permissions.iter().map(|p| serde_json::json!({
            "tool": p.tool,
            "description": p.description,
            "allowed_hosts": p.allowed_hosts,
        })).collect::<Vec<_>>(),
        "note": format!(
            "Every call still passes the invoking user's own capability limits and the \
             policy engine. {CONTRACT_NOTE}"
        ),
    });
    if grant.shared_stale {
        out["warning"] = serde_json::Value::from(format!(
            "The org-wide shared grant for `{}` is stale, so other users remain blocked; \
             an admin covers everyone with skill.grant credential=\"tools\" shared=true.",
            grant.skill
        ));
    }
    out
}

struct GrantTool {
    ctx: Arc<AgentToolsContext>,
}

impl GrantTool {
    fn run(&self, args: GrantArgs) -> Result<ToolResult, ToolResult> {
        let request = grant_request(&args).map_err(|e| ToolResult::failure(e, 0))?;
        let consent = consent(&self.ctx)?;
        let payload = match request {
            GrantRequest::Tools { shared } => consent
                .grant_tools(&args.skill, shared)
                .map(|grant| render_tools_grant(&grant))
                .map_err(refused)?,
            GrantRequest::Credential { name, vault_path } => {
                let value = consent.user_secret(&vault_path).map_err(refused)?;
                match consent.grant_credential(&args.skill, &name, value) {
                    Ok(grant) => render_credential_grant(&grant),
                    Err(Refusal::NoValue { skill, credential }) => {
                        return Err(no_value(consent.uid(), &vault_path, &skill, &credential))
                    }
                    Err(refusal) => return Err(refused(refusal)),
                }
            }
        };
        Ok(ToolResult::success(payload, 0))
    }
}

#[async_trait]
impl Tool for GrantTool {
    fn name(&self) -> &str {
        "skill.grant"
    }

    fn description(&self) -> &str {
        "Approve a WASM skill's declared credential or tool permissions for its current consent contract, as yourself. For a credential, first store the value with vault.set at a per-user path (default: the credential's name) — never pass a secret to this tool — then grant it; a re-grant keeps the stored value. credential=\"tools\" approves the skill's declared [[permissions]] for you, or with shared=true (admin only) for every user. Read help topic \"skill\" first."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": { "type": "string", "description": "Installed skill name" },
                "credential": { "type": "string", "description": "A credential the skill's manifest declares, or \"tools\" for its [[permissions]]" },
                "shared": { "type": "boolean", "description": "With credential=\"tools\": approve for every user (admin only)" },
                "vault_path": { "type": "string", "description": "Where you stored the value with vault.set, relative to your per-user scope; defaults to the credential name" }
            },
            "required": ["skill", "credential"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        unwrap_or_result(parse(input).and_then(|args| self.run(args)))
    }
}

// ── skill.grants ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct GrantsArgs {
    #[serde(default)]
    skill: Option<String>,
}

fn render_grants(grants: &SkillGrants, uid: &str) -> serde_json::Value {
    let skill = &grants.skill;
    serde_json::json!({
        "skill": skill.manifest.name,
        "owner": skill.owner,
        "mine": skill.owner.as_deref() == Some(uid),
        "visibility": match skill.owner.is_none() {
            true => "everyone",
            false => skill.visibility.word(),
        },
        "shared_with": skill.shares.len(),
        "tools": grants.tools.as_ref().map(|t| serde_json::json!({
            "declared": t.declared,
            "yours": status_word(t.own),
            "shared": status_word(t.shared),
        })),
        "credentials": grants.credentials.iter().map(|c| serde_json::json!({
            "name": c.name,
            "scope": scope_word(c.scope),
            "grant": status_word(c.status),
            "value_stored": c.value_stored,
        })).collect::<Vec<_>>(),
    })
}

struct GrantsTool {
    ctx: Arc<AgentToolsContext>,
}

impl GrantsTool {
    fn run(&self, args: GrantsArgs) -> Result<ToolResult, ToolResult> {
        let consent = consent(&self.ctx)?;
        let filter = args
            .skill
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty());
        let skills = consent.grants(filter).map_err(refused)?;
        Ok(ToolResult::success(
            serde_json::json!({
                "skills": skills.iter().map(|g| render_grants(g, consent.uid())).collect::<Vec<_>>(),
                "note": "stale = the skill's consent contract changed since the grant; re-run skill.grant to re-approve",
            }),
            0,
        ))
    }
}

#[async_trait]
impl Tool for GrantsTool {
    fn name(&self) -> &str {
        "skill.grants"
    }

    fn description(&self) -> &str {
        "Your grants on every WASM skill you can use: for each declared credential and the tool permissions, whether the grant is valid, stale (contract changed) or missing, and whether a value is stored."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": { "type": "string", "description": "Only this skill" }
            }
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        unwrap_or_result(parse(input).and_then(|args| self.run(args)))
    }
}

// ── skill.revoke ──────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct RevokeArgs {
    skill: String,
    credential: String,
    #[serde(default)]
    shared: bool,
}

fn revoke_target(args: &RevokeArgs) -> Result<Target, String> {
    match (args.credential.trim(), args.shared) {
        ("", _) => Err("credential is required: a declared credential name, or \"tools\"".into()),
        ("tools", shared) => Ok(Target::Tools { shared }),
        (_, true) => Err("shared applies to tools only".into()),
        (name, false) => Ok(Target::Credential(name.to_string())),
    }
}

fn render_revocation(revocation: &Revocation) -> serde_json::Value {
    let Revocation {
        skill,
        target,
        removed,
    } = revocation;
    let (credential, shared) = match target {
        Target::Tools { shared } => ("tools", *shared),
        Target::Credential(name) => (name.as_str(), false),
    };
    serde_json::json!({
        "status": if *removed { "revoked" } else { "nothing_to_revoke" },
        "skill": skill,
        "credential": credential,
        "shared": shared,
    })
}

struct RevokeTool {
    ctx: Arc<AgentToolsContext>,
}

impl RevokeTool {
    fn run(&self, args: RevokeArgs) -> Result<ToolResult, ToolResult> {
        let target = revoke_target(&args).map_err(|e| ToolResult::failure(e, 0))?;
        let consent = consent(&self.ctx)?;
        consent
            .revoke(&args.skill, target)
            .map(|revocation| ToolResult::success(render_revocation(&revocation), 0))
            .map_err(refused)
    }
}

#[async_trait]
impl Tool for RevokeTool {
    fn name(&self) -> &str {
        "skill.revoke"
    }

    fn description(&self) -> &str {
        "Remove your grant of a WASM skill's credential (and its stored value) or of its tool permissions (credential=\"tools\"; shared=true removes the org-wide grant, admin only)."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": { "type": "string" },
                "credential": { "type": "string", "description": "A declared credential name, or \"tools\"" },
                "shared": { "type": "boolean", "description": "With credential=\"tools\": the org-wide grant (admin only)" }
            },
            "required": ["skill", "credential"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        unwrap_or_result(parse(input).and_then(|args| self.run(args)))
    }
}

// ── skill.share / skill.unshare ───────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct ShareArgs {
    skill: String,
    user: String,
    #[serde(default)]
    permission: Option<String>,
}

fn share_permission(word: Option<&str>) -> Result<GrantLevel, String> {
    match word.map(str::trim) {
        None | Some("" | "run") => Ok(GrantLevel::Run),
        Some("manage") => Ok(GrantLevel::Manage),
        Some(other) => Err(format!(
            "permission must be \"run\" or \"manage\", not \"{other}\""
        )),
    }
}

fn render_share(outcome: &ShareOutcome) -> serde_json::Value {
    let ShareOutcome {
        skill,
        recipient,
        change,
        kind,
    } = outcome;
    let (status, permission) = match change {
        ShareChange::Added(permission) => ("shared", Some(permission_word(*permission))),
        ShareChange::Removed => ("unshared", None),
        ShareChange::NotShared => ("not_shared", None),
        ShareChange::SelfShare => ("already_owner", None),
    };
    serde_json::json!({
        "status": status,
        "skill": skill,
        "user": recipient.id(),
        "display_name": recipient.named(),
        "permission": permission,
        "note": match (change, kind) {
            (ShareChange::Added(_), SkillKind::Wasm) => {
                "They grant their own credentials with skill.grant."
            }
            _ => "",
        },
    })
}

struct ShareTool {
    ctx: Arc<AgentToolsContext>,
    adding: bool,
}

impl ShareTool {
    fn run(&self, args: ShareArgs) -> Result<ToolResult, ToolResult> {
        let permission = match self.adding {
            true => Some(
                share_permission(args.permission.as_deref())
                    .map_err(|e| ToolResult::failure(e, 0))?,
            ),
            false => None,
        };
        let consent = consent(&self.ctx)?;
        consent
            .share(&args.skill, &args.user, permission)
            .map(|outcome| ToolResult::success(render_share(&outcome), 0))
            .map_err(refused)
    }
}

#[async_trait]
impl Tool for ShareTool {
    fn name(&self) -> &str {
        match self.adding {
            true => "skill.share",
            false => "skill.unshare",
        }
    }

    fn description(&self) -> &str {
        match self.adding {
            true => "Let one person or team use a skill you own, WASM or external (SKILL.md): permission \"run\" (the default), or \"manage\" to use and edit it. Name them by email, Workforce user id, Slack mention, or team:<name>. Only the owner or an admin changes who reaches a skill.",
            false => "Take back a person's or team's access to a skill you own, WASM or external (SKILL.md). Name them by email, Workforce user id, Slack mention, or team:<name>.",
        }
    }

    fn input_schema(&self) -> serde_json::Value {
        let mut properties = serde_json::json!({
            "skill": { "type": "string" },
            "user": { "type": "string", "description": "Email address, Workforce user id, Slack mention, or team:<name>" }
        });
        if self.adding {
            properties["permission"] = serde_json::json!({
                "type": "string", "enum": ["run", "manage"], "description": "Default run"
            });
        }
        serde_json::json!({
            "type": "object",
            "properties": properties,
            "required": ["skill", "user"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        unwrap_or_result(parse(input).and_then(|args| self.run(args)))
    }
}

// ── skill.visibility ──────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct VisibilityArgs {
    skill: String,
    visibility: String,
}

fn render_visibility(change: &VisibilityChange) -> serde_json::Value {
    let note = match (change.visibility, change.kind) {
        (Visibility::Global, SkillKind::Wasm) => "Every user can call it now; each grants their own credentials with skill.grant.",
        (Visibility::Global, SkillKind::External) => "Every user's agents can load it now.",
        (Visibility::Team, SkillKind::Wasm) => "Anyone in a team with you can call it now; each grants their own credentials with skill.grant.",
        (Visibility::Team, SkillKind::External) => "Anyone in a team with you can load it now.",
        (Visibility::Private, SkillKind::Wasm) => "Only you and the people it is shared with can call it.",
        (Visibility::Private, SkillKind::External) => "Only you and the people it is shared with can load it.",
    };
    serde_json::json!({
        "status": "updated",
        "skill": change.skill,
        "visibility": change.visibility.word(),
        "shared_with": change.shares,
        // A reach nobody stands in is worth saying here too: this surface sets
        // it as readily as chat does.
        "note": match change.owner_has_no_team {
            true => format!("{note} {}", crate::resource_access::NO_TEAM_YET),
            false => note.to_string(),
        },
    })
}

struct VisibilityTool {
    ctx: Arc<AgentToolsContext>,
}

impl VisibilityTool {
    fn run(&self, args: VisibilityArgs) -> Result<ToolResult, ToolResult> {
        let visibility = Visibility::parse_word(args.visibility.trim()).ok_or_else(|| {
            ToolResult::failure(
                format!(
                    "visibility must be \"private\", \"team\" or \"everyone\", not \"{}\"",
                    args.visibility
                ),
                0,
            )
        })?;
        let consent = consent(&self.ctx)?;
        consent
            .set_visibility(&args.skill, visibility)
            .map(|change| ToolResult::success(render_visibility(&change), 0))
            .map_err(refused)
    }
}

#[async_trait]
impl Tool for VisibilityTool {
    fn name(&self) -> &str {
        "skill.visibility"
    }

    fn description(&self) -> &str {
        "Make a skill you own, WASM or external (SKILL.md), available to every user (\"everyone\"), to anyone in a team with you (\"team\"), or to you and your grantees alone (\"private\")."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": { "type": "string" },
                "visibility": { "type": "string", "enum": ["private", "team", "everyone"] }
            },
            "required": ["skill", "visibility"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        unwrap_or_result(parse(input).and_then(|args| self.run(args)))
    }
}

// ── skill.transfer ────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
struct TransferArgs {
    skill: String,
    user: String,
}

fn render_transfer(outcome: &TransferOutcome) -> serde_json::Value {
    serde_json::json!({
        "status": "transferred",
        "skill": outcome.skill,
        "from": outcome.previous_owner,
        "to": outcome.recipient.id,
        "display_name": outcome.recipient.display,
        "shared_credentials": outcome.shared_credentials,
        "notes": skill_transfer_notes(outcome),
    })
}

struct TransferTool {
    ctx: Arc<AgentToolsContext>,
}

impl TransferTool {
    fn run(&self, args: TransferArgs) -> Result<ToolResult, ToolResult> {
        let consent = consent(&self.ctx)?;
        consent
            // The tool takes no keep-access argument, so a transfer through it
            // means exactly what the word says.
            .transfer(&args.skill, &args.user, OutgoingAccess::Nothing)
            .map(|outcome| ToolResult::success(render_transfer(&outcome), 0))
            .map_err(refused)
    }
}

#[async_trait]
impl Tool for TransferTool {
    fn name(&self) -> &str {
        "skill.transfer"
    }

    fn description(&self) -> &str {
        "Hand a skill you own, WASM or external (SKILL.md), to another member — they own it afterwards and you keep no access, so this cannot be undone from your side. Name them by email, Workforce user id or Slack mention. Only the owner or an admin may transfer a skill."
    }

    fn input_schema(&self) -> serde_json::Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "skill": { "type": "string" },
                "user": { "type": "string", "description": "Email address, Workforce user id, or Slack mention of the member who will own it" }
            },
            "required": ["skill", "user"]
        })
    }

    async fn execute(&self, input: serde_json::Value) -> eyre::Result<ToolResult> {
        unwrap_or_result(parse(input).and_then(|args| self.run(args)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill_consent::tests::Harness;

    fn args(credential: &str, shared: bool, vault_path: Option<&str>) -> GrantArgs {
        GrantArgs {
            skill: "enabler".into(),
            credential: credential.into(),
            shared,
            vault_path: vault_path.map(str::to_string),
        }
    }

    #[test]
    fn a_grant_request_sorts_tools_from_credentials_and_their_options() {
        assert_eq!(
            grant_request(&args("tools", false, None)),
            Ok(GrantRequest::Tools { shared: false })
        );
        assert_eq!(
            grant_request(&args("tools", true, None)),
            Ok(GrantRequest::Tools { shared: true })
        );
        assert_eq!(
            grant_request(&args("api_token", false, None)),
            Ok(GrantRequest::Credential {
                name: "api_token".into(),
                vault_path: "api_token".into()
            }),
            "the vault path defaults to the credential name"
        );
        assert_eq!(
            grant_request(&args(" api_token ", false, Some(" enabler/token "))),
            Ok(GrantRequest::Credential {
                name: "api_token".into(),
                vault_path: "enabler/token".into()
            })
        );
        assert!(grant_request(&args("", false, None)).is_err());
        assert!(grant_request(&args("tools", false, Some("x"))).is_err());
        assert!(grant_request(&args("api_token", true, None)).is_err());
    }

    #[test]
    fn the_missing_value_message_names_vault_set_and_the_exact_path() {
        let result = no_value("bob", "enabler/token", "enabler", "api_token");
        assert!(!result.success);
        let text = result.error.unwrap();
        assert!(text.contains("users/bob/enabler/token"), "{text}");
        assert!(text.contains("vault.set"), "{text}");
        assert!(text.contains("never passes through this tool"), "{text}");
    }

    #[test]
    fn revoke_targets_follow_the_same_grammar() {
        let revoke = |credential: &str, shared: bool| {
            revoke_target(&RevokeArgs {
                skill: "enabler".into(),
                credential: credential.into(),
                shared,
            })
        };
        assert_eq!(revoke("tools", true), Ok(Target::Tools { shared: true }));
        assert_eq!(
            revoke("api_token", false),
            Ok(Target::Credential("api_token".into()))
        );
        assert!(revoke("api_token", true).is_err());
        assert!(revoke("", false).is_err());
    }

    #[test]
    fn share_permissions_default_to_run() {
        assert_eq!(share_permission(None), Ok(GrantLevel::Run));
        assert_eq!(share_permission(Some("")), Ok(GrantLevel::Run));
        assert_eq!(share_permission(Some("manage")), Ok(GrantLevel::Manage));
        assert!(share_permission(Some("owner")).is_err());
    }

    #[test]
    fn outcomes_render_as_flat_json_the_client_can_relay() {
        let h = Harness::new();
        h.publish();
        let bob = h.consent("bob", false);
        let grant = bob
            .grant_credential("enabler", "api_token", Some("v".into()))
            .unwrap();
        let json = render_credential_grant(&grant);
        assert_eq!(json["status"], "granted");
        assert_eq!(json["scope"], "user");
        assert_eq!(json["value"], "stored");
        assert_eq!(json["may_send_to"][0], "api.example.com");

        let mut tools = bob.grant_tools("enabler", false).unwrap();
        let json = render_tools_grant(&tools);
        assert_eq!(json["coverage"], "you");
        assert_eq!(json["may_invoke"][0]["tool"], "slack.post_message");
        assert!(json.get("warning").is_none());
        tools.shared_stale = true;
        assert!(render_tools_grant(&tools)["warning"]
            .as_str()
            .unwrap()
            .contains("stale"));

        let revoked = bob
            .revoke("enabler", Target::Tools { shared: false })
            .unwrap();
        assert_eq!(render_revocation(&revoked)["status"], "revoked");
        let again = bob
            .revoke("enabler", Target::Tools { shared: false })
            .unwrap();
        assert_eq!(render_revocation(&again)["status"], "nothing_to_revoke");

        let alice = h.consent("alice", false);
        let listed = alice.grants(None).unwrap();
        let json = render_grants(&listed[0], "alice");
        assert_eq!(json["mine"], true);
        assert_eq!(json["visibility"], "everyone");
        assert_eq!(json["tools"]["yours"], "missing");
        assert_eq!(json["credentials"][0]["grant"], "missing");

        let change = alice
            .set_visibility("enabler", Visibility::Private)
            .unwrap();
        assert_eq!(render_visibility(&change)["visibility"], "private");

        let share = ShareOutcome {
            skill: "enabler".into(),
            recipient: crate::recipients::Recipient::User {
                id: "bob".into(),
                display: "Bob".into(),
            },
            change: ShareChange::Added(GrantLevel::Manage),
            kind: SkillKind::Wasm,
        };
        let json = render_share(&share);
        assert_eq!(json["status"], "shared");
        assert_eq!(json["permission"], "manage");
    }

    #[test]
    fn wasm_replies_are_unchanged() {
        let outcome = |change| {
            render_share(&ShareOutcome {
                skill: "enabler".into(),
                recipient: crate::recipients::Recipient::User {
                    id: "bob".into(),
                    display: "Bob".into(),
                },
                change,
                kind: SkillKind::Wasm,
            })
        };
        let shared = |permission: &str, note: &str| {
            serde_json::json!({
                "status": "shared",
                "skill": "enabler",
                "user": "bob",
                "display_name": "Bob",
                "permission": permission,
                "note": note,
            })
        };
        let unchanged = |status: &str| {
            serde_json::json!({
                "status": status,
                "skill": "enabler",
                "user": "bob",
                "display_name": "Bob",
                "permission": null,
                "note": "",
            })
        };
        let credentials = "They grant their own credentials with skill.grant.";
        assert_eq!(
            outcome(ShareChange::Added(GrantLevel::Run)),
            shared("run", credentials)
        );
        assert_eq!(
            outcome(ShareChange::Added(GrantLevel::Manage)),
            shared("manage", credentials)
        );
        assert_eq!(outcome(ShareChange::Removed), unchanged("unshared"));
        assert_eq!(outcome(ShareChange::NotShared), unchanged("not_shared"));
        assert_eq!(outcome(ShareChange::SelfShare), unchanged("already_owner"));

        let visibility = |visibility, shares| {
            render_visibility(&VisibilityChange {
                skill: "enabler".into(),
                visibility,
                shares,
                kind: SkillKind::Wasm,
                owner_has_no_team: false,
            })
        };
        assert_eq!(
            visibility(Visibility::Global, 2),
            serde_json::json!({
                "status": "updated",
                "skill": "enabler",
                "visibility": "everyone",
                "shared_with": 2,
                "note": "Every user can call it now; each grants their own credentials with skill.grant.",
            })
        );
        assert_eq!(
            visibility(Visibility::Private, 0),
            serde_json::json!({
                "status": "updated",
                "skill": "enabler",
                "visibility": "private",
                "shared_with": 0,
                "note": "Only you and the people it is shared with can call it.",
            })
        );
        // Choosing a team reach while standing in no team says so here too.
        assert_eq!(
            render_visibility(&VisibilityChange {
                skill: "enabler".into(),
                visibility: Visibility::Team,
                shares: 0,
                kind: SkillKind::Wasm,
                owner_has_no_team: true,
            })["note"],
            format!(
                "Anyone in a team with you can call it now; each grants their own credentials \
                 with skill.grant. {}",
                crate::resource_access::NO_TEAM_YET
            )
        );
    }

    #[test]
    fn external_share_and_visibility_replies_leave_out_credentials() {
        let share = render_share(&ShareOutcome {
            skill: "hello-world".into(),
            recipient: crate::recipients::Recipient::User {
                id: "bob".into(),
                display: "Bob".into(),
            },
            change: ShareChange::Added(GrantLevel::Run),
            kind: SkillKind::External,
        });
        assert_eq!(share["status"], "shared");
        assert_eq!(share["note"], "");

        let visibility = |visibility| {
            render_visibility(&VisibilityChange {
                skill: "hello-world".into(),
                visibility,
                shares: 0,
                kind: SkillKind::External,
                owner_has_no_team: false,
            })["note"]
                .clone()
        };
        assert_eq!(
            visibility(Visibility::Global),
            "Every user's agents can load it now."
        );
        assert_eq!(
            visibility(Visibility::Private),
            "Only you and the people it is shared with can load it."
        );
    }

    #[test]
    fn every_skill_tool_is_named_under_the_skill_family() {
        let names: Vec<String> = skill_tools(super::super::agent_tools::tests::test_ctx())
            .iter()
            .map(|t| t.name().to_string())
            .collect();
        assert_eq!(
            names,
            [
                "skill.grant",
                "skill.grants",
                "skill.revoke",
                "skill.share",
                "skill.unshare",
                "skill.visibility",
                "skill.transfer"
            ]
        );
    }
}
