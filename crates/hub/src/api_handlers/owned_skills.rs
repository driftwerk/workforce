//! What the Skills page's routes for owned skills share, whatever the kind: who
//! is calling, the caller's reach on a skill, and the sharing and visibility
//! changes an owner makes, each read through a [`SkillRegistry`].

use axum::http::HeaderMap;
use axum::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use workforce_core::access::{AccessLevel, AccessWrite, GrantEntry, OutgoingAccess, Visibility};
use workforce_wasm_runtime::OwnedSkill;

use crate::gateway::{CallerContext, GatewayState};
use crate::resource_access::validate_grants;
use crate::skill_access::{kind_word, SkillCaller, SkillRegistry};
use crate::skill_consent::TransferBy;

use super::errors::ApiError;

#[derive(Deserialize)]
pub struct TokenQuery {
    token: Option<String>,
}

#[derive(Deserialize)]
pub struct SharesBody {
    shares: Vec<GrantEntry>,
}

#[derive(Deserialize)]
pub struct VisibilityBody {
    visibility: String,
}

#[derive(Deserialize)]
pub struct OwnerBody {
    owner: String,
    /// Leave the outgoing owner a `manage` grant, written into the same access
    /// record as the new owner. Absent means no, which is what a transfer
    /// plainly means.
    #[serde(default)]
    keep_manage: bool,
}

/// The authenticated caller. Ownership is per identity, so a principal without
/// one — a paired device — has no standing to resolve. Only the admin token,
/// which carries its own authority, gets through without an identity.
pub(super) async fn authenticated(
    state: &GatewayState,
    headers: &HeaderMap,
    query: &TokenQuery,
) -> Result<CallerContext, ApiError> {
    let context =
        crate::gateway::resolve_caller_context(state, headers, query.token.as_deref()).await;
    match context.is_authenticated() && (context.identity_id.is_some() || context.is_admin) {
        true => Ok(context),
        false => Err(ApiError::unauthorized()),
    }
}

/// The caller's reach on `skill`. `shares` is only meaningful to someone who can
/// change them, and who else holds a skill is not a run-level grantee's
/// business — so it is empty unless the caller manages the skill. `fixed` says
/// the skill's registry would refuse to switch its visibility whoever asked.
pub(super) fn reach_json<S: OwnedSkill + ?Sized>(
    caller: &SkillCaller,
    skill: &S,
    access: AccessLevel,
    fixed: bool,
) -> Map<String, Value> {
    [
        ("owner", json!(skill.owner())),
        (
            "mine",
            json!(skill.owner().is_some() && skill.owner() == caller.user.as_deref()),
        ),
        // Who *reaches* a skill is the owner's call alone — a `manage` grantee
        // may edit it but not re-share or publish it, so the buttons that do
        // those must key on this, not on `access`.
        ("can_share", json!(caller.controls_access(skill) && !fixed)),
        ("visibility", json!(skill.visibility().word())),
        (
            "shares",
            json!(match access {
                AccessLevel::Manage => shares_json(skill),
                _ => Vec::new(),
            }),
        ),
        ("access", json!(access.as_str())),
    ]
    .into_iter()
    .map(|(key, value)| (key.to_string(), value))
    .collect()
}

/// Grantees in the shape the shared React share dialog speaks.
fn shares_json<S: OwnedSkill + ?Sized>(skill: &S) -> Vec<Value> {
    crate::resource_access::grant_entries(skill.shares(), skill.owner())
        .iter()
        .map(|entry| json!(entry))
        .collect()
}

/// Refuse a change of who reaches `skill`, to `visibility`, that the registry never makes.
fn unfixed<R: SkillRegistry + ?Sized>(
    registry: &R,
    skill: &R::Skill,
    visibility: Visibility,
) -> Result<(), ApiError> {
    registry
        .fixed_reach(skill, visibility)
        .map_or(Ok(()), |fixed| Err(ApiError::conflict(fixed)))
}

/// The grantees of a skill whose reach the caller controls.
pub(super) fn get_shares<R: SkillRegistry + ?Sized>(
    registry: &R,
    caller: &SkillCaller,
    name: &str,
) -> Result<Json<Value>, ApiError> {
    let skill = caller
        .require_owner_to_read(registry, name)
        .map_err(ApiError::forbidden)?;
    Ok(Json(json!({ "shares": shares_json(&skill) })))
}

/// Replace the grantees of a skill whose reach the caller controls.
pub(super) fn put_shares<R: SkillRegistry + ?Sized>(
    registry: &R,
    caller: &SkillCaller,
    name: &str,
    body: SharesBody,
) -> Result<Json<Value>, ApiError> {
    let skill = caller
        .require_owner(registry, name)
        .map_err(ApiError::forbidden)?;
    unfixed(registry, &skill, skill.visibility())?;
    let shares = validate_grants(body.shares, skill.owner()).map_err(ApiError::bad_request)?;
    let count = shares.len();
    registry
        .set_access(name, AccessWrite::kept(skill.visibility(), shares))
        .map_err(|e| ApiError::internal(format!("updating shares: {e}")))?;
    tracing::info!(
        target: "sentinel.audit",
        skill = %name,
        actor = caller.user.as_deref().unwrap_or("admin-token"),
        count,
        "{} skill shares replaced",
        kind_word(R::KIND).to_ascii_lowercase(),
    );
    Ok(Json(json!({ "shared": name, "count": count })))
}

/// Publish a skill whose reach the caller controls to everyone, or make it
/// private again.
pub(super) fn put_visibility<R: SkillRegistry + ?Sized>(
    registry: &R,
    caller: &SkillCaller,
    name: &str,
    body: VisibilityBody,
) -> Result<Json<Value>, ApiError> {
    let skill = caller
        .require_owner(registry, name)
        .map_err(ApiError::forbidden)?;
    let visibility = Visibility::parse_word(&body.visibility).ok_or_else(|| {
        ApiError::bad_request(format!(
            "`{}` is not a visibility — use `private`, `team` or `everyone`",
            body.visibility
        ))
    })?;
    unfixed(registry, &skill, visibility)?;
    registry
        .set_access(name, AccessWrite::kept(visibility, skill.shares().to_vec()))
        .map_err(|e| ApiError::internal(format!("updating visibility: {e}")))?;
    tracing::info!(
        target: "sentinel.audit",
        skill = %name,
        actor = caller.user.as_deref().unwrap_or("admin-token"),
        visibility = visibility.word(),
        "{} skill visibility changed",
        kind_word(R::KIND).to_ascii_lowercase(),
    );
    Ok(Json(
        json!({ "skill": name, "visibility": visibility.word() }),
    ))
}

/// Who a transfer on this surface is by. The admin token carries its own
/// authority with no identity to attribute to, exactly as the other audit
/// lines here record it.
pub(super) fn transfer_by<'a>(state: &'a GatewayState, caller: &'a SkillCaller) -> TransferBy<'a> {
    TransferBy {
        actor: caller.user.as_deref().unwrap_or("admin-token"),
        surface: crate::skill_consent::Surface::Web,
        identities: state.identity_store.as_deref(),
        teams: state.team_store.as_deref(),
    }
}

/// Hand a skill whose reach the caller controls to another member.
///
/// The standing, the write and the audit are
/// [`crate::skill_consent::transfer_skill`]'s, so this surface cannot forget
/// any of them. `shared` names the `shared`-scope credentials the transfer
/// leaves behind — the WASM side's alone, since an external skill declares
/// none — and the notes say the two things the requester cannot otherwise see.
/// `keep_manage` rides into the transfer rather than being shared beforehand: a
/// skill's owner cannot be their own grantee, so a share ahead of the handover
/// is refused, and one after it is no longer theirs to write.
pub(super) fn put_owner<R: SkillRegistry + ?Sized>(
    registry: &R,
    caller: &SkillCaller,
    by: TransferBy<'_>,
    name: &str,
    body: OwnerBody,
    shared: Vec<String>,
) -> Result<Json<Value>, ApiError> {
    let outcome = crate::skill_consent::TransferOutcome {
        shared_credentials: shared,
        ..crate::skill_consent::transfer_skill(
            registry,
            caller,
            by,
            name,
            &body.owner,
            OutgoingAccess::keeping_manage(body.keep_manage),
        )
        .map_err(refusal)?
    };
    Ok(Json(json!({
        "transferred": name,
        "from": outcome.previous_owner,
        "to": outcome.recipient.id,
        "to_display": outcome.recipient.display,
        "shared_credentials": outcome.shared_credentials,
        "notes": crate::skill_consent::skill_transfer_notes(&outcome),
    })))
}

/// A refused consent operation in HTTP terms: a request naming somebody who
/// cannot be given the skill is the requester's to correct, and every other
/// refusal is about their standing or the hub's.
fn refusal(refusal: crate::skill_consent::Refusal) -> ApiError {
    use crate::skill_consent::Refusal as R;
    match &refusal {
        R::NoStore | R::VaultUnavailable => ApiError::service_unavailable(),
        R::NoSuchTarget(_) => ApiError::bad_request(refusal.to_string()),
        _ => ApiError::forbidden(refusal.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use workforce_core::access::{Grant, GrantLevel};

    /// The grantee JSON the Skills page reads is the one it sends back, and a
    /// team grantee now travels over it unchanged.
    #[test]
    fn grantees_read_exactly_as_the_share_dialog_writes_them() {
        let shares = vec![
            Grant::user("bob", GrantLevel::Run),
            Grant::team("platform", GrantLevel::Manage),
        ];
        let skill = workforce_wasm_runtime::InstalledWasmSkill {
            shares: shares.clone(),
            ..crate::skill_consent::tests::installed_fixture("hello-world", Some("alice"))
        };
        assert_eq!(
            shares_json(&skill),
            vec![
                json!({"principal_type": "user", "principal_id": "bob", "permission": "run"}),
                json!({
                    "principal_type": "team",
                    "principal_id": "platform",
                    "permission": "manage"
                }),
            ]
        );
        let sent_back: Vec<GrantEntry> = shares_json(&skill)
            .into_iter()
            .map(|value| serde_json::from_value(value).expect("round trip"))
            .collect();
        assert_eq!(validate_grants(sent_back, Some("alice")), Ok(shares));
    }

    /// The reach a skill's row carries, including the word for its visibility.
    #[test]
    fn a_reach_names_the_visibility_in_the_word_the_page_shows() {
        let caller = SkillCaller::local(Some("alice".into()), false);
        for (visibility, word) in [
            (Visibility::Private, "private"),
            (Visibility::Team, "team"),
            (Visibility::Global, "everyone"),
        ] {
            let skill = workforce_wasm_runtime::InstalledWasmSkill {
                visibility,
                ..crate::skill_consent::tests::installed_fixture("hello-world", Some("alice"))
            };
            let reach = reach_json(&caller, &skill, AccessLevel::Manage, false);
            assert_eq!(reach["visibility"], json!(word));
            assert_eq!(reach["can_share"], json!(true));
            assert_eq!(reach["mine"], json!(true));
        }
    }
}
