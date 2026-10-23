//! WASM skill management API — the surface for the Skills page.
//!
//! Authority is per skill, not per role ([`crate::skill_access`]): any
//! authenticated user may call these, and each route answers for the skill it
//! names. Authoring stays in chat, where the agent that writes the skill is.
//!
//! - `GET    /api/wasm-skills`                    — skills the caller can use.
//! - `GET    /api/wasm-skills/{name}/shares`      — grantees (owner/admin).
//! - `PUT    /api/wasm-skills/{name}/shares`      — replace them (owner/admin).
//! - `PUT    /api/wasm-skills/{name}/visibility`  — publish / unpublish (owner/admin).
//! - `PUT    /api/wasm-skills/{name}/owner`       — hand it over (owner/admin).
//! - `DELETE /api/wasm-skills/{name}`             — uninstall (manage).

use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::Json;
use serde_json::{json, Value};
use workforce_core::access::AccessLevel;
use workforce_wasm_runtime::{InstalledWasmSkill, WasmSkillStore};

use crate::gateway::GatewayState;
use crate::skill_access::SkillCaller;

use super::errors::ApiError;
use super::owned_skills::{
    authenticated, get_shares, put_owner, put_shares, put_visibility, reach_json, transfer_by,
    OwnerBody, SharesBody, TokenQuery, VisibilityBody,
};

/// The caller, plus the process-wide skill store. Every route's preamble.
async fn caller(
    state: &GatewayState,
    headers: &axum::http::HeaderMap,
    query: &TokenQuery,
) -> Result<(SkillCaller, Arc<WasmSkillStore>), ApiError> {
    let context = authenticated(state, headers, query).await?;
    let store =
        crate::wasm_skill_tools::global_skill_store().ok_or_else(ApiError::service_unavailable)?;
    Ok((
        SkillCaller::new(
            context.identity_id.clone(),
            crate::gateway::caller_is_admin(state.identity_store.as_ref(), &context),
            crate::team_reach::TeamReach::new(
                state.team_store.clone(),
                context.identity_id.clone(),
            ),
        ),
        store,
    ))
}

fn skill_json(caller: &SkillCaller, skill: &InstalledWasmSkill, access: AccessLevel) -> Value {
    let mut view = json!({
        "name": skill.manifest.name,
        "version": skill.manifest.version,
        "description": skill.manifest.description,
        "state": skill.state,
        "source": skill.source,
        "installed_at": skill.installed_at.to_rfc3339(),
        "tools": skill.manifest.qualified_tool_names(),
        "capabilities": skill.manifest.capabilities,
        "credentials": skill.manifest.credentials.iter().map(|c| c.name.clone()).collect::<Vec<_>>(),
    });
    if let Some(fields) = view.as_object_mut() {
        fields.extend(reach_json(caller, skill, access, false));
    }
    view
}

pub async fn list_wasm_skills(
    Query(query): Query<TokenQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, ApiError> {
    let (caller, store) = caller(&state, &headers, &query).await?;
    let skills: Vec<Value> = caller
        .visible(&store)
        .iter()
        .map(|(skill, access)| skill_json(&caller, skill, *access))
        .collect();
    Ok(Json(json!({ "skills": skills })))
}

pub async fn get_wasm_skill_shares(
    Path(name): Path<String>,
    Query(query): Query<TokenQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, ApiError> {
    let (caller, store) = caller(&state, &headers, &query).await?;
    get_shares(&store, &caller, &name)
}

pub async fn put_wasm_skill_shares(
    Path(name): Path<String>,
    Query(query): Query<TokenQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<SharesBody>,
) -> Result<Json<Value>, ApiError> {
    let (caller, store) = caller(&state, &headers, &query).await?;
    put_shares(&store, &caller, &name, body)
}

pub async fn put_wasm_skill_visibility(
    Path(name): Path<String>,
    Query(query): Query<TokenQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<VisibilityBody>,
) -> Result<Json<Value>, ApiError> {
    let (caller, store) = caller(&state, &headers, &query).await?;
    put_visibility(&store, &caller, &name, body)
}

pub async fn put_wasm_skill_owner(
    Path(name): Path<String>,
    Query(query): Query<TokenQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
    Json(body): Json<OwnerBody>,
) -> Result<Json<Value>, ApiError> {
    let (caller, store) = caller(&state, &headers, &query).await?;
    let shared = crate::skill_consent::shared_credentials(&store, &name);
    put_owner(
        &store,
        &caller,
        transfer_by(&state, &caller),
        &name,
        body,
        shared,
    )
}

pub async fn delete_wasm_skill(
    Path(name): Path<String>,
    Query(query): Query<TokenQuery>,
    headers: axum::http::HeaderMap,
    State(state): State<Arc<GatewayState>>,
) -> Result<Json<Value>, ApiError> {
    let (caller, store) = caller(&state, &headers, &query).await?;
    let skill = caller
        .require(&store, &name, AccessLevel::Manage)
        .map_err(ApiError::forbidden)?;
    let engine =
        crate::wasm_skill_tools::global_skill_engine().ok_or_else(ApiError::service_unavailable)?;
    let hosts = crate::wasm_skill_tools::SkillHosts::process();
    let removed = crate::wasm_skill_tools::uninstall_skill(&hosts, &engine, &store, &skill)
        .await
        .map_err(|e| ApiError::internal(format!("uninstalling: {e}")))?;
    tracing::info!(
        target: "sentinel.audit",
        skill = %name,
        actor = caller.user.as_deref().unwrap_or("admin-token"),
        "wasm skill uninstalled via API",
    );
    Ok(Json(
        json!({ "skill": name, "uninstalled": true, "tools_removed": removed }),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_core::access::{Grant, GrantLevel, Visibility};
    use workforce_wasm_runtime::{SkillManifest, WasmSkillState};

    fn skill(owner: Option<&str>, shares: Vec<Grant>) -> InstalledWasmSkill {
        InstalledWasmSkill {
            manifest: SkillManifest::from_toml(
                "[skill]\nname = \"s\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\
                 [[tools]]\nname = \"ping\"\ndescription = \"p\"\n",
            )
            .expect("manifest"),
            state: WasmSkillState::Active,
            installed_at: chrono::Utc::now(),
            source: "test".into(),
            config: Default::default(),
            binary_version: 1,
            sha256: String::new(),
            owner: owner.map(str::to_string),
            visibility: Visibility::Private,
            shares,
        }
    }

    #[test]
    fn wasm_skill_listing_reads_exactly_as_before() {
        use workforce_wasm_runtime::SkillOwnership;

        let tmp = tempfile::tempdir().expect("tempdir");
        let store = WasmSkillStore::open(tmp.path()).expect("store");
        let install = |name: &str, ownership: SkillOwnership| {
            store
                .install_prebuilt(
                    SkillManifest::from_toml(&format!(
                        "[skill]\nname = \"{name}\"\nversion = \"1.0.0\"\ndescription = \"d\"\n\
                         [capabilities]\nhttp = true\n\
                         [network]\nallowed_hosts = [\"api.example.com\"]\n\
                         [[credentials]]\nname = \"api_token\"\ndescription = \"t\"\n\
                         [[tools]]\nname = \"ping\"\ndescription = \"p\"\n"
                    ))
                    .expect("manifest"),
                    b"wasm",
                    None,
                    "test".into(),
                    Default::default(),
                    ownership,
                )
                .expect("install")
        };
        install(
            "private",
            SkillOwnership::owned("alice", Visibility::Private),
        );
        install("global", SkillOwnership::owned("alice", Visibility::Global));
        install("org", SkillOwnership::operator());
        store
            .set_access(
                "private",
                workforce_core::access::AccessWrite::kept(
                    Visibility::Private,
                    vec![
                        Grant::user("bob", GrantLevel::Run),
                        Grant::user("carol", GrantLevel::Manage),
                    ],
                ),
            )
            .expect("share");

        let listing = |caller: SkillCaller| {
            let skills: Vec<Value> = caller
                .visible(&store)
                .iter()
                .map(|(skill, access)| skill_json(&caller, skill, *access))
                .collect();
            json!({ "skills": skills })
        };
        let row = |name: &str,
                   owner: Value,
                   (mine, can_share): (bool, bool),
                   visibility: &str,
                   shares: Value,
                   access: &str| {
            json!({
                "name": name,
                "version": "1.0.0",
                "description": "d",
                "state": "active",
                "source": "test",
                "installed_at": store.get_installed(name).expect("installed").installed_at.to_rfc3339(),
                "tools": [format!("{name}.ping")],
                "capabilities": ["http"],
                "credentials": ["api_token"],
                "owner": owner,
                "mine": mine,
                "can_share": can_share,
                "visibility": visibility,
                "shares": shares,
                "access": access,
            })
        };
        let grantees = json!([
            { "principal_type": "user", "principal_id": "bob", "permission": "run" },
            { "principal_type": "user", "principal_id": "carol", "permission": "manage" },
        ]);
        let org = |access: &str, can_share: bool| {
            row(
                "org",
                Value::Null,
                (false, can_share),
                "everyone",
                json!([]),
                access,
            )
        };

        assert_eq!(
            listing(SkillCaller::local(Some("alice".into()), false)),
            json!({ "skills": [
                row("global", json!("alice"), (true, true), "everyone", json!([]), "manage"),
                org("run", false),
                row("private", json!("alice"), (true, true), "private", grantees.clone(), "manage"),
            ]})
        );
        assert_eq!(
            listing(SkillCaller::local(Some("bob".into()), false)),
            json!({ "skills": [
                row("global", json!("alice"), (false, false), "everyone", json!([]), "run"),
                org("run", false),
                row("private", json!("alice"), (false, false), "private", json!([]), "run"),
            ]})
        );
        assert_eq!(
            listing(SkillCaller::local(Some("carol".into()), false)),
            json!({ "skills": [
                row("global", json!("alice"), (false, false), "everyone", json!([]), "run"),
                org("run", false),
                row("private", json!("alice"), (false, false), "private", grantees.clone(), "manage"),
            ]})
        );
        assert_eq!(
            listing(SkillCaller::local(Some("dave".into()), false)),
            json!({ "skills": [
                row("global", json!("alice"), (false, false), "everyone", json!([]), "run"),
                org("run", false),
            ]})
        );
        assert_eq!(
            listing(SkillCaller::local(None, true)),
            json!({ "skills": [
                row("global", json!("alice"), (false, true), "everyone", json!([]), "manage"),
                org("manage", true),
                row("private", json!("alice"), (false, true), "private", grantees, "manage"),
            ]})
        );
    }

    /// A run-level grantee sees the skill but not who else holds it, and only
    /// the owner gets the controls that change who reaches it.
    #[test]
    fn the_projection_matches_the_caller() {
        let shares = vec![Grant::user("bob", GrantLevel::Run)];
        let owned = skill(Some("alice"), shares);

        let alice = SkillCaller::local(Some("alice".into()), false);
        let view = skill_json(&alice, &owned, AccessLevel::Manage);
        assert_eq!(view["mine"], true);
        assert_eq!(view["can_share"], true);
        assert_eq!(view["shares"].as_array().expect("shares").len(), 1);
        assert_eq!(view["visibility"], "private");

        let bob = SkillCaller::local(Some("bob".into()), false);
        let view = skill_json(&bob, &owned, AccessLevel::Run);
        assert_eq!(view["mine"], false);
        assert_eq!(view["can_share"], false);
        assert!(view["shares"].as_array().expect("shares").is_empty());

        // A `manage` grantee edits the skill but does not control its reach.
        let carol = SkillCaller::local(Some("carol".into()), false);
        let view = skill_json(&carol, &owned, AccessLevel::Manage);
        assert_eq!(view["can_share"], false);

        let org = skill(None, Vec::new());
        let view = skill_json(&bob, &org, AccessLevel::Run);
        assert_eq!(view["owner"], serde_json::Value::Null);
        assert_eq!(view["mine"], false);
        assert_eq!(view["can_share"], false);
    }
}
