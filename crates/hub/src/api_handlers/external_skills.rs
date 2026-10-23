//! External (SKILL.md) skill management API — the Skills page's list of the
//! external skills in the skills directory a user can use, and their owners'
//! controls. A skill read from another root is for everyone and is not listed.
//!
//! Authority is per skill ([`crate::skill_access`]), with holders of
//! `skills_manage` in the bypass admins hold over WASM skills. Writing a
//! skill's files stays in chat, and with an admin on the Skills page editor.
//!
//! - `GET    /api/external-skills`                    — installed skills the caller can use.
//! - `GET    /api/external-skills/{name}/shares`      — grantees (owner or skills manager).
//! - `PUT    /api/external-skills/{name}/shares`      — replace them (owner or skills manager).
//! - `PUT    /api/external-skills/{name}/visibility`  — publish / unpublish (owner or skills manager).
//! - `PUT    /api/external-skills/{name}/owner`       — hand it over (owner or skills manager).
//! - `DELETE /api/external-skills/{name}`             — uninstall (manage).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::Json;
use serde_json::{json, Value};
use workforce_core::access::AccessLevel;

use crate::authz::Capability;
use crate::external_skills::{ExternalSkill, ExternalSkillError, ExternalSkills};
use crate::gateway::{CallerContext, GatewayState};
use crate::skill_access::{SkillCaller, SkillRegistry};
use crate::system_tool::REQUIREMENTS_MET;

use super::errors::ApiError;
use super::owned_skills::{
    authenticated, get_shares, put_owner, put_shares, put_visibility, reach_json, transfer_by,
    OwnerBody, SharesBody, TokenQuery, VisibilityBody,
};

/// The caller's standing over external skills: an active system administrator
/// and holders of `skills_manage` in the live capability matrix manage every
/// one. `skills_manage` is delegable through the matrix, so an administrator
/// holds the bypass by office as well, and cannot be edited out of it.
pub(crate) fn external_caller(state: &GatewayState, context: &CallerContext) -> SkillCaller {
    SkillCaller::new(
        context.identity_id.clone(),
        crate::gateway::caller_is_admin(state.identity_store.as_ref(), context)
            || context.identity_id.as_deref().is_some_and(|uid| {
                crate::authz::holds_capability(
                    state.identity_store.as_ref(),
                    uid,
                    Capability::SkillsManage,
                )
            }),
        crate::team_reach::TeamReach::new(state.team_store.clone(), context.identity_id.clone()),
    )
}

async fn caller(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
    query: &TokenQuery,
) -> Result<SkillCaller, ApiError> {
    authenticated(state, headers, query)
        .await
        .map(|context| external_caller(state, &context))
}

fn skill_json(
    skills: &ExternalSkills,
    caller: &SkillCaller,
    skill: &ExternalSkill,
    access: AccessLevel,
) -> Value {
    let (discovered, meta) = (&skill.discovered, &skill.discovered.skill.meta);
    let mut view = json!({
        "name": meta.name,
        "version": meta.version,
        "description": meta.description,
        "author": meta.author,
        "available": discovered.is_available(),
        "gating": discovered
            .unavailable_reason()
            .unwrap_or_else(|| REQUIREMENTS_MET.to_string()),
    });
    if let Some(fields) = view.as_object_mut() {
        let fixed = skills.reach_fixed(skill).is_some();
        fields.extend(reach_json(caller, skill, access, fixed));
    }
    view
}

pub async fn list_external_skills(
    Query(query): Query<TokenQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, ApiError> {
    let caller = caller(&state, &headers, &query).await?;
    let installed = state.external_skills.as_ref();
    let skills: Vec<Value> = caller
        .visible(installed)
        .iter()
        .filter(|(skill, _)| installed.installed_here(skill))
        .map(|(skill, access)| skill_json(installed, &caller, skill, *access))
        .collect();
    Ok(Json(json!({ "skills": skills })))
}

pub async fn get_external_skill_shares(
    Path(name): Path<String>,
    Query(query): Query<TokenQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, ApiError> {
    let caller = caller(&state, &headers, &query).await?;
    get_shares(state.external_skills.as_ref(), &caller, &name)
}

pub async fn put_external_skill_shares(
    Path(name): Path<String>,
    Query(query): Query<TokenQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<SharesBody>,
) -> Result<Json<Value>, ApiError> {
    let caller = caller(&state, &headers, &query).await?;
    put_shares(state.external_skills.as_ref(), &caller, &name, body)
}

pub async fn put_external_skill_visibility(
    Path(name): Path<String>,
    Query(query): Query<TokenQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<VisibilityBody>,
) -> Result<Json<Value>, ApiError> {
    let caller = caller(&state, &headers, &query).await?;
    put_visibility(state.external_skills.as_ref(), &caller, &name, body)
}

/// An external skill declares no credentials, so a transfer leaves no secret
/// of the outgoing owner's behind — the one consequence the WASM side has to
/// name and this one does not.
pub async fn put_external_skill_owner(
    Path(name): Path<String>,
    Query(query): Query<TokenQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<OwnerBody>,
) -> Result<Json<Value>, ApiError> {
    let caller = caller(&state, &headers, &query).await?;
    put_owner(
        state.external_skills.as_ref(),
        &caller,
        transfer_by(&state, &caller),
        &name,
        body,
        Vec::new(),
    )
}

pub async fn delete_external_skill(
    Path(name): Path<String>,
    Query(query): Query<TokenQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, ApiError> {
    let caller = caller(&state, &headers, &query).await?;
    caller
        .require(state.external_skills.as_ref(), &name, AccessLevel::Manage)
        .map_err(ApiError::forbidden)?;
    let actor = caller.user.clone();
    state
        .external_skills
        .uninstall(caller, name.clone())
        .await
        .map_err(|e| match e {
            ExternalSkillError::Refused(why) => ApiError::forbidden(why),
            ExternalSkillError::Failed(report) => {
                ApiError::internal(format!("uninstalling: {report}"))
            }
        })?;
    tracing::info!(
        target: "sentinel.audit",
        skill = %name,
        actor = actor.as_deref().unwrap_or("admin-token"),
        "external skill uninstalled via API",
    );
    Ok(Json(json!({ "skill": name, "uninstalled": true })))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::external_skills::tests::{owned, share};
    use workforce_core::access::{GrantLevel, Visibility};

    fn skill(owner: Option<&str>, visibility: Visibility, passed: bool) -> ExternalSkill {
        ExternalSkill {
            discovered: workforce_skills::scanner::DiscoveredSkill {
                skill: workforce_skills::skill_file::SkillFile::parse(
                    "---\nname: hello-world\nversion: 1.0.0\ndescription: Says hello\nauthor: acme\n---\nSay hello.\n",
                )
                .unwrap(),
                path: std::path::PathBuf::from("/srv/skills/hello-world"),
                gating: workforce_skills::gating::GatingResult {
                    passed,
                    checks: Vec::new(),
                },
                config_status: workforce_skills::config_loader::ConfigStatus::Unconfigured,
                security_findings: Vec::new(),
            },
            access: owned(
                owner,
                visibility,
                vec![share("bob", GrantLevel::Run)],
            ),
        }
    }

    #[test]
    fn external_skill_projection_matches_the_caller() {
        let skills = ExternalSkills::new(std::path::PathBuf::from("/srv/skills"), Vec::new());
        let skill_json = |caller: &SkillCaller, skill: &ExternalSkill, access| {
            super::skill_json(&skills, caller, skill, access)
        };
        let private = skill(Some("alice"), Visibility::Private, true);
        let alice = SkillCaller::local(Some("alice".into()), false);
        assert_eq!(
            skill_json(&alice, &private, AccessLevel::Manage),
            json!({
                "name": "hello-world",
                "version": "1.0.0",
                "description": "Says hello",
                "author": "acme",
                "available": true,
                "gating": REQUIREMENTS_MET,
                "owner": "alice",
                "mine": true,
                "can_share": true,
                "visibility": "private",
                "shares": [{ "principal_type": "user", "principal_id": "bob", "permission": "run" }],
                "access": "manage",
            })
        );

        let bob = SkillCaller::local(Some("bob".into()), false);
        let view = skill_json(&bob, &private, AccessLevel::Run);
        assert_eq!(view["mine"], false);
        assert_eq!(view["can_share"], false);
        assert_eq!(view["shares"], json!([]));
        assert_eq!(view["access"], "run");

        let manager = SkillCaller::local(Some("root".into()), true);
        let view = skill_json(&manager, &private, AccessLevel::Manage);
        assert_eq!(view["mine"], false);
        assert_eq!(view["can_share"], true);

        let org = skill(None, Visibility::Global, false);
        let view = skill_json(&bob, &org, AccessLevel::Run);
        assert_eq!(view["owner"], Value::Null);
        assert_eq!(view["visibility"], "everyone");
        assert_eq!(view["available"], false);
        assert_ne!(view["gating"], REQUIREMENTS_MET);
        assert_eq!(
            skill_json(&manager, &org, AccessLevel::Manage)["can_share"],
            false
        );

        let bundled = ExternalSkill {
            discovered: workforce_skills::scanner::DiscoveredSkill {
                path: std::path::PathBuf::from("/app/bundled/hello-world"),
                ..private.discovered.clone()
            },
            ..private
        };
        assert_eq!(
            skill_json(&alice, &bundled, AccessLevel::Manage)["can_share"],
            false
        );
    }
}
