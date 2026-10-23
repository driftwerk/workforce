//! Who may do what with an installed skill of either kind, WASM or external.
//!
//! A skill is an owned resource: the identity that installed it controls it,
//! may share it with named users at `run` or `manage`, and may publish it for
//! everyone. Above that sits one bypass, so the skills nobody owns (legacy,
//! catalog and operator installs) stay manageable at all, and so an install can
//! still be administered when a skill's owner is away, suspended or gone. Every
//! operation it allows over a skill somebody owns is recorded against whoever
//! took it ([`crate::resource_audit`]); an owner's own work records nothing, and
//! so does a grantee's, so the log separates an override from ordinary work. The
//! record is written where the standing is taken, before the write it authorises
//! runs, so one that then fails still reports an override that was allowed. One
//! install path records less than the operation it performs: an install claiming
//! a free *name* whose directory already holds another member's record passes
//! [`SkillCaller::require_install`] with nothing to record, and clears the
//! directory's own guard unrecorded.
//!
//! A [`SkillCaller`] is built for one kind of skill, and its `is_admin` is that
//! kind's bypass: administrators for WASM skills, administrators and holders of
//! `skills_manage` for external skills. The rules read any [`SkillRegistry`],
//! so both kinds refuse, list and share with the same words.

use std::sync::Arc;

use workforce_core::access::{
    access_with, granted_to, AccessLevel, AccessWrite, TeamMembership, Visibility,
};
use workforce_skills::catalog::SkillKind;
use workforce_wasm_runtime::{InstalledWasmSkill, OwnedSkill, SkillOwnership, WasmSkillStore};

use crate::authz::IsAdminFn;
use crate::def_write_auth::TeamStoreHandle;
use crate::team_reach::TeamReach;

/// The installed skills of one kind, as the access rules read them.
pub trait SkillRegistry {
    type Skill: OwnedSkill + Clone;
    const KIND: SkillKind;

    fn installed(&self, name: &str) -> Option<Self::Skill>;
    fn all_installed(&self) -> Vec<Self::Skill>;
    fn skill_name(skill: &Self::Skill) -> &str;
    /// Replace who owns `name` and who may use it, returning the skill as it now stands.
    fn set_access(&self, name: &str, write: AccessWrite) -> eyre::Result<Self::Skill>;
    /// Why no change of who reaches `skill` is possible, whoever asks, or `None` when standing
    /// alone decides.
    fn reach_fixed(&self, _skill: &Self::Skill) -> Option<String> {
        None
    }
    /// The same question for one target visibility: a change that moves nothing asks nothing.
    fn fixed_reach(&self, skill: &Self::Skill, visibility: Visibility) -> Option<String> {
        ownerless_team(Self::skill_name(skill), skill, visibility)
    }
}

/// Why a skill nobody owns cannot be made team-visible: there is no owner whose
/// teams could be resolved, so the stored word would name a reach nothing
/// honours — the rule a definition row states in
/// [`crate::def_write_auth::resolve_visibility`]. Leaving the reach where it is
/// asks nothing of it, so a record that already holds the value is not refused.
pub fn ownerless_team(
    name: &str,
    skill: &(impl OwnedSkill + ?Sized),
    visibility: Visibility,
) -> Option<String> {
    let widened = skill.owner().is_none()
        && visibility == Visibility::Team
        && skill.visibility() != visibility;
    widened.then(|| {
        format!(
            "Skill `{name}` has no owner, so it has no team; install it again with \
             `visibility = \"private\"` to make it a skill of your own."
        )
    })
}

impl<R: SkillRegistry + ?Sized> SkillRegistry for Arc<R> {
    type Skill = R::Skill;
    const KIND: SkillKind = R::KIND;

    fn installed(&self, name: &str) -> Option<Self::Skill> {
        R::installed(self, name)
    }

    fn all_installed(&self) -> Vec<Self::Skill> {
        R::all_installed(self)
    }

    fn skill_name(skill: &Self::Skill) -> &str {
        R::skill_name(skill)
    }

    fn set_access(&self, name: &str, write: AccessWrite) -> eyre::Result<Self::Skill> {
        R::set_access(self, name, write)
    }

    fn reach_fixed(&self, skill: &Self::Skill) -> Option<String> {
        R::reach_fixed(self, skill)
    }

    fn fixed_reach(&self, skill: &Self::Skill, visibility: Visibility) -> Option<String> {
        R::fixed_reach(self, skill, visibility)
    }
}

/// Who, besides owners and grantees, acts on a skill of `kind` nobody owns.
fn bypass_holder(kind: SkillKind) -> &'static str {
    match kind {
        SkillKind::Wasm => "an admin",
        SkillKind::External => "someone with the skills_manage capability",
    }
}

/// The kind as it reads inside a sentence.
pub fn kind_word(kind: SkillKind) -> &'static str {
    match kind {
        SkillKind::Wasm => "WASM",
        SkillKind::External => "external",
    }
}

/// The kind as an audit record names it — a stable machine word, beside the
/// definition kinds, so a log query never depends on how a refusal is worded.
pub const fn audit_kind(kind: SkillKind) -> &'static str {
    match kind {
        SkillKind::Wasm => "wasm",
        SkillKind::External => "external",
    }
}

/// The kind as it reads at the start of a sentence.
pub fn kind_title(kind: SkillKind) -> &'static str {
    match kind {
        SkillKind::Wasm => "WASM",
        SkillKind::External => "External",
    }
}

/// The acting identity plus its standing over one kind of skill, resolved once
/// per operation.
#[derive(Debug, Clone)]
pub struct SkillCaller {
    pub user: Option<String>,
    pub is_admin: bool,
    /// The teams this caller stands in — how a team-visible skill and a grant
    /// to a team reach them. Asked only when one of those is in play.
    teams: TeamReach,
}

impl SkillCaller {
    pub fn new(user: Option<String>, is_admin: bool, teams: TeamReach) -> Self {
        Self {
            user: user.filter(|uid| !uid.is_empty()),
            is_admin,
            teams,
        }
    }

    /// A caller who stands in no team: the single-tenant CLI, an operator
    /// install, and every surface with no team store to ask.
    pub fn local(user: Option<String>, is_admin: bool) -> Self {
        Self::new(user, is_admin, TeamReach::none())
    }

    /// The caller of the current turn — the identity task-local, the
    /// status-aware policy that decides the bypass, and the turn's teams.
    pub fn current(admin: &IsAdminFn, teams: &TeamStoreHandle) -> Self {
        let user = crate::user_config::current_identity_id();
        Self::new(
            user.clone(),
            admin(),
            TeamReach::new(teams.read().clone(), user),
        )
    }

    /// The caller behind a surface that holds an agent service rather than the
    /// tools' injected admin policy — `cmd:` commands and the workflow runtime.
    pub fn for_service(service: &Arc<crate::serve::AgentService>, uid: &str) -> Self {
        Self::new(
            Some(uid.to_string()),
            crate::authz::is_admin(service, uid),
            TeamReach::for_service(service, Some(uid.to_string())),
        )
    }

    /// The teams the caller stands in, for the surfaces that say so.
    pub fn teams(&self) -> &TeamReach {
        &self.teams
    }

    /// Whether this is a member: an identified caller without the kind's
    /// bypass, whose installs are confined to skills and sources of their own.
    /// The bypass holder and the identity-less operator are not.
    pub fn is_member(&self) -> bool {
        self.user.is_some() && !self.is_admin
    }

    /// This caller's standing on `skill`, with the admin bypass applied. An
    /// admin manages every skill; ownership still decides for everyone else.
    ///
    /// A caller with **no identity at all** manages an **ownerless** skill: that
    /// pairing is the single-tenant operator at their own keyboard, where no
    /// per-user boundary exists to enforce (the capability gate skips the same
    /// case). It grants nothing over a skill somebody owns, and the REST surface
    /// refuses identity-less callers outright, so it cannot be reached from a
    /// device token.
    pub fn access(&self, skill: &(impl OwnedSkill + ?Sized)) -> AccessLevel {
        match self.is_admin || (self.user.is_none() && skill.owner().is_none()) {
            true => AccessLevel::Manage,
            false => self.earned(skill),
        }
    }

    /// The standing this caller *earned* — ownership, a grant, or the reach the
    /// record was published with. No bypass, so it is what an override is
    /// measured against.
    fn earned(&self, skill: &(impl OwnedSkill + ?Sized)) -> AccessLevel {
        access_with(
            skill.owner(),
            skill.visibility(),
            self.user.as_deref(),
            granted_to(skill.shares(), self.user.as_deref(), &self.teams),
            || {
                skill
                    .owner()
                    .is_some_and(|owner| self.teams.shares_team_with(owner))
            },
        )
    }

    /// Whether this caller owns `skill` outright.
    fn owns(&self, skill: &(impl OwnedSkill + ?Sized)) -> bool {
        self.user
            .as_deref()
            .is_some_and(|uid| skill.owner() == Some(uid))
    }

    /// Record a standing this caller took but had not earned: the bypass
    /// reaching past a skill's owner ([`crate::resource_audit`]). A skill
    /// nobody owns has no owner to reach past, so administering one — the
    /// reason the bypass exists — records nothing.
    fn note_override(
        &self,
        skill: &(impl OwnedSkill + ?Sized),
        name: &str,
        earned: bool,
        took: AccessLevel,
        kind: SkillKind,
    ) {
        if !earned && skill.owner().is_some() {
            crate::hub_core::resource_audit::override_granted(
                audit_kind(kind),
                name,
                skill.owner(),
                self.user.as_deref(),
                took.as_str(),
            );
        }
    }

    /// Whether this caller may take `needed` over `skill`, recording the bypass
    /// when it reaches past the owner. The one gate every write goes through —
    /// [`Self::require`] wraps it for the surfaces that take its refusal, and
    /// the rest call it directly because they word their own.
    pub fn may_take(
        &self,
        skill: &(impl OwnedSkill + ?Sized),
        name: &str,
        needed: AccessLevel,
        kind: SkillKind,
    ) -> bool {
        let allowed = self.access(skill).allows(needed);
        if allowed {
            self.note_override(skill, name, self.earned(skill).allows(needed), needed, kind);
        }
        allowed
    }

    /// Fetch the installed skill and require `needed`, or explain the refusal.
    pub fn require<R: SkillRegistry + ?Sized>(
        &self,
        registry: &R,
        name: &str,
        needed: AccessLevel,
    ) -> Result<R::Skill, String> {
        let skill = registry
            .installed(name)
            .ok_or_else(|| not_installed(self, registry, name))?;
        match self.may_take(&skill, name, needed, R::KIND) {
            true => Ok(skill),
            false => Err(refusal(name, needed, skill.owner().is_none(), R::KIND)),
        }
    }

    /// Whether this caller decides who else reaches `skill`. Only the owner (or
    /// an admin) does: a `manage` grantee may edit the skill but not re-share or
    /// publish it.
    pub fn controls_access(&self, skill: &(impl OwnedSkill + ?Sized)) -> bool {
        self.is_admin || self.owns(skill) || (self.user.is_none() && skill.owner().is_none())
    }

    /// [`Self::controls_access`] over an installed skill, or the refusal.
    ///
    /// Only ownership earns this, never a `manage` grant — so an administrator
    /// who also holds one is still overriding the owner, and is recorded.
    pub fn require_owner<R: SkillRegistry + ?Sized>(
        &self,
        registry: &R,
        name: &str,
    ) -> Result<R::Skill, String> {
        let (skill, owns) = self.owner_standing(registry, name)?;
        self.note_override(&skill, name, owns, AccessLevel::Manage, R::KIND);
        Ok(skill)
    }

    /// The same standing, for *reading* who a skill reaches: that is the
    /// owner's business, so it takes the authority to change it — and records
    /// nothing, because a read is not an override.
    pub fn require_owner_to_read<R: SkillRegistry + ?Sized>(
        &self,
        registry: &R,
        name: &str,
    ) -> Result<R::Skill, String> {
        self.owner_standing(registry, name).map(|(skill, _)| skill)
    }

    /// The installed skill, and whether this caller owns it outright, once
    /// [`Self::controls_access`] has let them through.
    fn owner_standing<R: SkillRegistry + ?Sized>(
        &self,
        registry: &R,
        name: &str,
    ) -> Result<(R::Skill, bool), String> {
        let skill = registry
            .installed(name)
            .ok_or_else(|| not_installed(self, registry, name))?;
        match self.controls_access(&skill) {
            true => {
                let owns = self.owns(&skill);
                Ok((skill, owns))
            }
            false => Err(match skill.owner().is_none() {
                true => format!(
                    "Skill `{name}` has no owner — it was installed for the whole org, so only \
                     {} can change who reaches it.",
                    bypass_holder(R::KIND)
                ),
                false => format!(
                    "Skill `{name}` belongs to another user, so only they can change who \
                     reaches it."
                ),
            }),
        }
    }

    /// Authorise claiming `name` for a new install: either nothing holds it,
    /// or the caller may manage what does (their own skill, one shared with
    /// them at `manage`, or — for an admin — anything, which is recorded because
    /// an install rewrites the bytes of whatever it replaces).
    pub fn require_install<R: SkillRegistry + ?Sized>(
        &self,
        registry: &R,
        name: &str,
    ) -> Result<Option<R::Skill>, String> {
        let claimed = self.claim_install(registry, name)?;
        if let Some(held) = &claimed {
            self.note_override(
                held,
                name,
                self.earned(held).allows(AccessLevel::Manage),
                AccessLevel::Manage,
                R::KIND,
            );
        }
        Ok(claimed)
    }

    /// [`Self::require_install`] as a pre-flight, asked before anything is
    /// fetched or built: the same refusal, without the record, which the claim
    /// that authorises the write writes once when it runs.
    pub fn may_install<R: SkillRegistry + ?Sized>(
        &self,
        registry: &R,
        name: &str,
    ) -> Result<(), String> {
        self.claim_install(registry, name).map(|_| ())
    }

    /// Who holds `name`, once the caller is allowed to take it over.
    fn claim_install<R: SkillRegistry + ?Sized>(
        &self,
        registry: &R,
        name: &str,
    ) -> Result<Option<R::Skill>, String> {
        match registry.installed(name) {
            None => Ok(None),
            Some(existing) if self.access(&existing).allows(AccessLevel::Manage) => {
                Ok(Some(existing))
            }
            Some(existing) if existing.owner().is_none() => Err(format!(
                "The name `{name}` is an org-wide skill installed for everyone, so only {} can \
                 replace it. Pick a different name.",
                bypass_holder(R::KIND)
            )),
            Some(_) => Err(format!(
                "The name `{name}` belongs to another user's skill. Pick a different name, or ask \
                 its owner to share it with you (`cmd: skill share {name} <you> manage`)."
            )),
        }
    }

    /// Ownership to stamp on an install. An identity-less caller (single-tenant
    /// CLI, system jobs) installs an operator skill. Claiming a skill that had
    /// no owner keeps it visible to everyone unless the caller says otherwise —
    /// reinstalling an org-wide skill must not quietly take it away from the
    /// people using it.
    pub fn ownership(
        &self,
        requested: Option<Visibility>,
        previous: Option<&(impl OwnedSkill + ?Sized)>,
    ) -> SkillOwnership {
        let Some(uid) = &self.user else {
            return SkillOwnership::operator();
        };
        let visibility = requested.unwrap_or(match previous {
            Some(ownerless) if ownerless.owner().is_none() => Visibility::Global,
            _ => Visibility::Private,
        });
        SkillOwnership::owned(uid, visibility)
    }

    /// Every installed skill this caller may run, with the standing they hold.
    pub fn visible<R: SkillRegistry + ?Sized>(&self, registry: &R) -> Vec<(R::Skill, AccessLevel)> {
        let mut visible: Vec<_> = registry
            .all_installed()
            .into_iter()
            .map(|skill| {
                let access = self.access(&skill);
                (skill, access)
            })
            .filter(|(_, access)| access.allows(AccessLevel::Run))
            .collect();
        visible.sort_by(|(a, _), (b, _)| R::skill_name(a).cmp(R::skill_name(b)));
        visible
    }
}

/// "Not installed", plus the skills this caller could have meant — never the
/// ones they cannot see, which would turn a typo into a listing of everyone
/// else's private skills.
fn not_installed<R: SkillRegistry + ?Sized>(
    caller: &SkillCaller,
    registry: &R,
    name: &str,
) -> String {
    let kind = kind_word(R::KIND);
    let visible: Vec<String> = caller
        .visible(registry)
        .iter()
        .map(|(skill, _)| R::skill_name(skill).to_string())
        .collect();
    match visible.is_empty() {
        true => format!("No {kind} skill named `{name}` is installed."),
        false => format!(
            "No {kind} skill named `{name}` is installed. Available to you: {}",
            visible.join(", ")
        ),
    }
}

pub fn refusal(name: &str, needed: AccessLevel, ownerless: bool, kind: SkillKind) -> String {
    match (needed, ownerless) {
        (_, true) => format!(
            "Skill `{name}` has no owner — it was installed for the whole org, so only {} can \
             change it.",
            bypass_holder(kind)
        ),
        (AccessLevel::Manage, false) => format!(
            "Skill `{name}` belongs to another user. Ask them to share it with you \
             (`cmd: skill share {name} <you> manage`)."
        ),
        _ => format!(
            "Skill `{name}` is private to another user. Ask them to share it with you \
             (`cmd: skill share {name} <you>`)."
        ),
    }
}

/// What to tell the author about who can reach their skill — the one place the
/// install and import results name the commands that change it. Reads the
/// record, so a reinstall that kept existing grantees says so.
pub fn sharing_hint(name: &str, skill: &(impl OwnedSkill + ?Sized)) -> String {
    match (skill.visibility(), skill.shares().len()) {
        (Visibility::Global, _) => format!(
            "Everyone can use `{name}`. Make it yours alone again with \
             `cmd: skill visibility {name} private`."
        ),
        (Visibility::Team, _) => format!(
            "Anyone in a team with you can use `{name}`. Make it yours alone again with \
             `cmd: skill visibility {name} private`."
        ),
        (Visibility::Private, 0) => format!(
            "Only you can use `{name}`. Share it with `cmd: skill share {name} <user>` \
             (add `manage` to let them edit it), or publish it to everyone with \
             `cmd: skill visibility {name} everyone`."
        ),
        (Visibility::Private, shared) => format!(
            "You and the {shared} user(s) you shared it with can use `{name}`. Publish it to \
             everyone with `cmd: skill visibility {name} everyone`, or change who has it with \
             `cmd: skill share {name} <user>` / `cmd: skill unshare {name} <user>`."
        ),
    }
}

impl SkillRegistry for WasmSkillStore {
    type Skill = InstalledWasmSkill;
    const KIND: SkillKind = SkillKind::Wasm;

    fn installed(&self, name: &str) -> Option<InstalledWasmSkill> {
        self.get_installed(name)
    }

    fn all_installed(&self) -> Vec<InstalledWasmSkill> {
        self.list_installed()
    }

    fn skill_name(skill: &InstalledWasmSkill) -> &str {
        &skill.manifest.name
    }

    fn set_access(&self, name: &str, write: AccessWrite) -> eyre::Result<InstalledWasmSkill> {
        WasmSkillStore::set_access(self, name, write)
    }
}

#[cfg(any(test, feature = "test-support"))]
pub mod testing {
    use std::collections::HashMap;
    use std::sync::Arc;

    use serde_json::json;
    use workforce_core::access::{AccessWrite, Grant};
    use workforce_wasm_runtime::{SkillOwnership, WasmSkillStore};

    pub fn manifest(name: &str) -> workforce_wasm_runtime::SkillManifest {
        workforce_wasm_runtime::SkillManifest {
            name: name.to_string(),
            version: "1.0.0".into(),
            description: "test".into(),
            author: String::new(),
            license: String::new(),
            min_workforce_version: None,
            capabilities: Default::default(),
            tools: vec![workforce_wasm_runtime::manifest::ManifestTool {
                name: "ping".into(),
                description: "ping".into(),
                requires_approval: false,
                approval_summary: None,
                input_schema: json!({"type": "object"}),
                handler: None,
            }],
            credentials: vec![],
            config_schema: vec![],
            allowed_hosts: vec![],
            permissions: vec![],
        }
    }

    pub fn store_with(
        skills: &[(&str, SkillOwnership, Vec<Grant>)],
    ) -> (tempfile::TempDir, Arc<WasmSkillStore>) {
        let tmp = tempfile::tempdir().unwrap();
        let store = WasmSkillStore::open(tmp.path()).unwrap();
        for (name, ownership, shares) in skills {
            store
                .install_prebuilt(
                    manifest(name),
                    b"bytes",
                    None,
                    "test".into(),
                    HashMap::new(),
                    ownership.clone(),
                )
                .unwrap();
            if !shares.is_empty() {
                store
                    .set_access(
                        name,
                        AccessWrite::kept(ownership.visibility, shares.clone()),
                    )
                    .unwrap();
            }
        }
        (tmp, Arc::new(store))
    }
}

#[cfg(test)]
pub mod tests {
    use super::testing::*;
    use super::*;

    use workforce_core::access::{Grant, GrantLevel};
    use workforce_wasm_runtime::WasmSkillStore;

    fn alice() -> SkillCaller {
        SkillCaller::local(Some("alice".into()), false)
    }
    fn bob() -> SkillCaller {
        SkillCaller::local(Some("bob".into()), false)
    }
    fn admin() -> SkillCaller {
        SkillCaller::local(Some("root".into()), true)
    }

    #[test]
    fn management_authority_follows_ownership() {
        let (_tmp, store) = store_with(&[
            (
                "mine",
                SkillOwnership::owned("alice", Visibility::Private),
                vec![
                    Grant::user("bob", GrantLevel::Run),
                    Grant::user("carol", GrantLevel::Manage),
                ],
            ),
            ("org", SkillOwnership::operator(), vec![]),
        ]);
        let carol = SkillCaller::local(Some("carol".into()), false);

        assert!(alice().require(&store, "mine", AccessLevel::Manage).is_ok());
        assert!(carol.require(&store, "mine", AccessLevel::Manage).is_ok());
        assert!(bob().require(&store, "mine", AccessLevel::Run).is_ok());
        assert!(bob().require(&store, "mine", AccessLevel::Manage).is_err());
        assert!(admin().require(&store, "mine", AccessLevel::Manage).is_ok());

        let stranger = SkillCaller::local(Some("dave".into()), false);
        let denied = stranger
            .require(&store, "mine", AccessLevel::Run)
            .unwrap_err();
        assert!(denied.contains("private to another user"), "got: {denied}");

        assert!(bob().require(&store, "org", AccessLevel::Run).is_ok());
        let ownerless = bob()
            .require(&store, "org", AccessLevel::Manage)
            .unwrap_err();
        assert!(ownerless.contains("no owner"), "got: {ownerless}");
        assert!(admin().require(&store, "org", AccessLevel::Manage).is_ok());

        let missing = alice()
            .require(&store, "nope", AccessLevel::Run)
            .unwrap_err();
        assert!(
            missing.contains("No WASM skill named `nope`"),
            "got: {missing}"
        );
    }

    /// The single-tenant CLI has no identity and no owned skills — it must
    /// still manage the operator skills it installed, while an identity-less
    /// caller gets nothing over a skill somebody owns.
    #[test]
    fn an_identity_less_caller_manages_only_ownerless_skills() {
        let (_tmp, store) = store_with(&[
            ("org", SkillOwnership::operator(), vec![]),
            (
                "hers",
                SkillOwnership::owned("alice", Visibility::Global),
                vec![],
            ),
        ]);
        let cli = SkillCaller::local(None, false);
        assert!(cli.require(&store, "org", AccessLevel::Manage).is_ok());
        assert!(cli.require_owner(&store, "org").is_ok());
        assert!(cli.require_install(&store, "org").unwrap().is_some());

        assert!(cli.require(&store, "hers", AccessLevel::Run).is_ok());
        assert!(cli.require(&store, "hers", AccessLevel::Manage).is_err());
        assert!(cli.require_owner(&store, "hers").is_err());
        assert!(cli.require_install(&store, "hers").is_err());
    }

    #[test]
    fn install_claims_a_free_name_or_a_manageable_one() {
        let (_tmp, store) = store_with(&[(
            "mine",
            SkillOwnership::owned("alice", Visibility::Private),
            vec![Grant::user("carol", GrantLevel::Manage)],
        )]);
        assert!(alice().require_install(&store, "fresh").unwrap().is_none());
        assert!(alice().require_install(&store, "mine").unwrap().is_some());
        assert!(SkillCaller::local(Some("carol".into()), false)
            .require_install(&store, "mine")
            .unwrap()
            .is_some());
        assert!(admin().require_install(&store, "mine").unwrap().is_some());
        let taken = bob().require_install(&store, "mine").unwrap_err();
        assert!(
            taken.contains("belongs to another user's skill"),
            "got: {taken}"
        );

        // An org-wide skill is not "someone else's" — it belongs to no one, and
        // the refusal has to say so or the author hunts for an owner to ask.
        let (_org_tmp, org_store) = store_with(&[("org", SkillOwnership::operator(), vec![])]);
        let org = bob().require_install(&org_store, "org").unwrap_err();
        assert!(org.contains("org-wide skill"), "got: {org}");
        assert!(admin()
            .require_install(&org_store, "org")
            .unwrap()
            .is_some());
    }

    #[test]
    fn wasm_refusals_read_exactly_as_before() {
        let (_tmp, store) = store_with(&[
            (
                "mine",
                SkillOwnership::owned("alice", Visibility::Private),
                vec![],
            ),
            ("org", SkillOwnership::operator(), vec![]),
        ]);
        let stranger = SkillCaller::local(Some("dave".into()), false);
        let (_empty_tmp, empty) = store_with(&[]);

        assert_eq!(
            stranger
                .require(&empty, "nope", AccessLevel::Run)
                .unwrap_err(),
            "No WASM skill named `nope` is installed."
        );
        assert_eq!(
            stranger
                .require(&store, "nope", AccessLevel::Run)
                .unwrap_err(),
            "No WASM skill named `nope` is installed. Available to you: org"
        );
        assert_eq!(
            refusal("org", AccessLevel::Run, true, SkillKind::Wasm),
            "Skill `org` has no owner — it was installed for the whole org, so only an admin \
             can change it."
        );
        assert_eq!(
            refusal("org", AccessLevel::Manage, true, SkillKind::Wasm),
            "Skill `org` has no owner — it was installed for the whole org, so only an admin \
             can change it."
        );
        assert_eq!(
            stranger
                .require(&store, "mine", AccessLevel::Manage)
                .unwrap_err(),
            "Skill `mine` belongs to another user. Ask them to share it with you \
             (`cmd: skill share mine <you> manage`)."
        );
        assert_eq!(
            stranger
                .require(&store, "mine", AccessLevel::Run)
                .unwrap_err(),
            "Skill `mine` is private to another user. Ask them to share it with you \
             (`cmd: skill share mine <you>`)."
        );
        assert_eq!(
            stranger.require_owner(&store, "org").unwrap_err(),
            "Skill `org` has no owner — it was installed for the whole org, so only an admin \
             can change who reaches it."
        );
        assert_eq!(
            stranger.require_owner(&store, "mine").unwrap_err(),
            "Skill `mine` belongs to another user, so only they can change who reaches it."
        );
        assert_eq!(
            stranger.require_owner(&store, "nope").unwrap_err(),
            "No WASM skill named `nope` is installed. Available to you: org"
        );
        assert!(stranger.require_install(&store, "fresh").unwrap().is_none());
        assert_eq!(
            stranger.require_install(&store, "org").unwrap_err(),
            "The name `org` is an org-wide skill installed for everyone, so only an admin can \
             replace it. Pick a different name."
        );
        assert_eq!(
            stranger.require_install(&store, "mine").unwrap_err(),
            "The name `mine` belongs to another user's skill. Pick a different name, or ask its \
             owner to share it with you (`cmd: skill share mine <you> manage`)."
        );
    }

    #[test]
    fn listing_is_scoped_to_the_caller() {
        let (_tmp, store) = store_with(&[
            (
                "private",
                SkillOwnership::owned("alice", Visibility::Private),
                vec![],
            ),
            (
                "published",
                SkillOwnership::owned("alice", Visibility::Global),
                vec![],
            ),
            ("org", SkillOwnership::operator(), vec![]),
        ]);
        let names = |caller: SkillCaller| {
            caller
                .visible(&store)
                .into_iter()
                .map(|(s, a)| (s.manifest.name, a))
                .collect::<Vec<_>>()
        };
        assert_eq!(
            names(alice()),
            vec![
                ("org".to_string(), AccessLevel::Run),
                ("private".to_string(), AccessLevel::Manage),
                ("published".to_string(), AccessLevel::Manage),
            ]
        );
        assert_eq!(
            names(bob()),
            vec![
                ("org".to_string(), AccessLevel::Run),
                ("published".to_string(), AccessLevel::Run),
            ]
        );
        assert_eq!(names(admin()).len(), 3);
        assert_eq!(names(SkillCaller::local(None, false)).len(), 2);
    }

    /// A team reaches a skill two ways — the owner's team visibility, and a
    /// grant naming a team — and neither reaches a caller with no team store
    /// behind them.
    #[test]
    fn a_team_reaches_a_skill_two_ways() {
        use workforce_identity::store::IdentityAndTeamStore;
        use workforce_identity::team::{Team, TeamStore};

        let identities =
            Arc::new(workforce_identity::sqlite::SqliteIdentityStore::in_memory().expect("sqlite"));
        let (alice, bob) = (uuid::Uuid::new_v4(), uuid::Uuid::new_v4());
        let mut platform = Team::new("platform", alice);
        platform.add_member(bob);
        identities.create_team(&platform).expect("team");

        let owned = |visibility| SkillOwnership::owned(alice.to_string(), visibility);
        let (_tmp, store) = store_with(&[
            ("team-wide", owned(Visibility::Team), vec![]),
            (
                "team-granted",
                owned(Visibility::Private),
                vec![Grant::team(platform.id.to_string(), GrantLevel::Manage)],
            ),
            ("closed", owned(Visibility::Private), vec![]),
        ]);

        let teammate = SkillCaller::new(
            Some(bob.to_string()),
            false,
            TeamReach::new(Some(identities.as_team_store()), Some(bob.to_string())),
        );
        assert!(teammate
            .require(&store, "team-wide", AccessLevel::Run)
            .is_ok());
        // Team visibility lets a teammate run it, never manage it.
        assert!(teammate
            .require(&store, "team-wide", AccessLevel::Manage)
            .is_err());
        assert!(teammate
            .require(&store, "team-granted", AccessLevel::Manage)
            .is_ok());
        assert!(teammate
            .require(&store, "closed", AccessLevel::Run)
            .is_err());
        assert_eq!(
            teammate
                .visible(&store)
                .into_iter()
                .map(|(s, a)| (s.manifest.name, a))
                .collect::<Vec<_>>(),
            vec![
                ("team-granted".to_string(), AccessLevel::Manage),
                ("team-wide".to_string(), AccessLevel::Run),
            ]
        );

        // The same person with no team store to ask stands in no team.
        let alone = SkillCaller::local(Some(bob.to_string()), false);
        for name in ["team-wide", "team-granted", "closed"] {
            assert!(
                alone.require(&store, name, AccessLevel::Run).is_err(),
                "{name}"
            );
        }
    }

    /// A skill nobody owns has no owner whose teams could be resolved, so the
    /// word would name a reach nothing honours — and both kinds refuse it in
    /// the same sentence. Everything else about an ownerless record is as it
    /// was: its grantees still change, and so does the reach it already holds.
    #[test]
    fn a_skill_nobody_owns_has_no_team_to_be_visible_to() {
        let (_tmp, store) = store_with(&[
            ("org", SkillOwnership::operator(), vec![]),
            (
                "mine",
                SkillOwnership::owned("alice", Visibility::Private),
                vec![],
            ),
        ]);
        let no_team = "Skill `org` has no owner, so it has no team; install it again with \
                       `visibility = \"private\"` to make it a skill of your own.";
        let org = SkillRegistry::installed(&store, "org").unwrap();
        assert_eq!(
            SkillRegistry::fixed_reach(&store, &org, Visibility::Team),
            Some(no_team.to_string())
        );
        for visibility in [Visibility::Private, Visibility::Global] {
            assert_eq!(
                SkillRegistry::fixed_reach(&store, &org, visibility),
                None,
                "{visibility:?}"
            );
        }
        // An owned skill takes every reach, team included.
        let mine = SkillRegistry::installed(&store, "mine").unwrap();
        for visibility in [Visibility::Private, Visibility::Team, Visibility::Global] {
            assert_eq!(SkillRegistry::fixed_reach(&store, &mine, visibility), None);
        }
        // The same sentence, from the other kind of registry.
        assert_eq!(
            ownerless_team("org", &org, Visibility::Team),
            Some(no_team.to_string())
        );
    }

    /// An administrator reaching past a skill's owner is recorded, once, with
    /// the administrator as the actor; the owner's own work, the work of a
    /// grantee they chose, a listing and a refusal are not overrides. A skill
    /// nobody owns has no owner to reach past, which is the case the bypass
    /// exists for.
    #[test]
    fn an_override_over_a_skill_is_recorded_and_ordinary_work_is_not() {
        use workforce_memory::access_audit::{self, AuditDecision, AuditOperation};

        let buffer = access_audit::install_ring_buffer(256);
        // The hub lib test binary registers no other global sink, so routing
        // the global sink at the buffer makes the emit observable.
        let _ = access_audit::set_global_sink(buffer.clone());
        let unique = |stem: &str| format!("{stem}-{}", uuid::Uuid::new_v4());
        let (mine, org, granted) = (unique("mine"), unique("org"), unique("granted"));
        let events = |name: &str| {
            let resource = format!("wasm:{name}");
            buffer
                .snapshot()
                .into_iter()
                .filter(|event| event.resource_id.as_deref() == Some(resource.as_str()))
                .collect::<Vec<_>>()
        };

        let (_tmp, store) = store_with(&[
            (
                &mine,
                SkillOwnership::owned("alice", Visibility::Private),
                vec![Grant::user("carol", GrantLevel::Manage)],
            ),
            (&org, SkillOwnership::operator(), vec![]),
            (
                &granted,
                SkillOwnership::owned("alice", Visibility::Private),
                vec![Grant::user("root", GrantLevel::Manage)],
            ),
        ]);
        let carol = SkillCaller::local(Some("carol".into()), false);

        // The owner, and the grantee they chose, record nothing.
        assert!(alice().require(&store, &mine, AccessLevel::Manage).is_ok());
        assert!(alice().require_owner(&store, &mine).is_ok());
        assert!(carol.require(&store, &mine, AccessLevel::Manage).is_ok());
        // Nor does a listing, a snapshot of it, or a refusal.
        assert_eq!(admin().visible(&store).len(), 3);
        assert_eq!(
            admin().access(&SkillRegistry::installed(&store, &mine).unwrap()),
            AccessLevel::Manage
        );
        assert!(bob().require(&store, &mine, AccessLevel::Manage).is_err());
        assert!(events(&mine).is_empty(), "only an override is recorded");

        // Administering a skill nobody owns is what the bypass is for.
        assert!(admin().require(&store, &org, AccessLevel::Manage).is_ok());
        assert!(admin().require_owner(&store, &org).is_ok());
        assert!(events(&org).is_empty());

        // Reaching past an owner is recorded, against the administrator.
        assert!(admin().require(&store, &mine, AccessLevel::Run).is_ok());
        let recorded = events(&mine);
        assert_eq!(recorded.len(), 1, "exactly one event per override");
        assert_eq!(
            recorded[0].operation,
            AuditOperation::ResourceAccessOverride
        );
        assert_eq!(recorded[0].decision, AuditDecision::Allow);
        assert_eq!(recorded[0].requester_identity_id.as_deref(), Some("root"));
        assert_eq!(recorded[0].owner_identity_id.as_deref(), Some("alice"));

        // A `manage` grant earns the work, so an administrator holding one
        // does that work unrecorded — but it never earns deciding who else
        // reaches the skill, so taking that is still an override, and the
        // grantee alone cannot take it at all.
        assert!(admin()
            .require(&store, &granted, AccessLevel::Manage)
            .is_ok());
        assert!(events(&granted).is_empty());
        assert!(SkillCaller::local(Some("root".into()), false)
            .require_owner(&store, &granted)
            .is_err());
        assert!(admin().require_owner(&store, &granted).is_ok());
        assert_eq!(events(&granted).len(), 1);

        // Reading who a skill reaches takes the same standing and records
        // nothing, so opening the share dialog is not an override.
        assert!(admin().require_owner_to_read(&store, &mine).is_ok());
        assert!(carol.require_owner_to_read(&store, &mine).is_err());
        assert_eq!(events(&mine).len(), 1, "the read added nothing");

        // An install claims a name, and claiming one somebody else holds
        // rewrites their skill — so the bypass that allows it is recorded, on
        // the same gate every other write passes.
        let fresh = unique("fresh");
        assert!(admin().require_install(&store, &fresh).unwrap().is_none());
        assert!(admin().require_install(&store, &org).unwrap().is_some());
        assert!(carol.require_install(&store, &mine).unwrap().is_some());
        assert!(bob().require_install(&store, &mine).is_err());
        assert!(events(&fresh).is_empty());
        assert!(events(&org).is_empty());
        assert_eq!(events(&mine).len(), 1, "an earned claim is ordinary work");
        // The pre-flight an installer asks before a long fetch gives the same
        // answer and records nothing, so one install is one event.
        assert!(admin().may_install(&store, &mine).is_ok());
        assert_eq!(events(&mine).len(), 1, "a pre-flight is not a claim");
        assert!(admin().require_install(&store, &mine).unwrap().is_some());
        assert_eq!(events(&mine).len(), 2, "claiming an owner's name is");
    }

    #[test]
    fn an_arc_registry_answers_as_its_store() {
        let (_tmp, store) = store_with(&[
            (
                "mine",
                SkillOwnership::owned("alice", Visibility::Private),
                vec![],
            ),
            ("org", SkillOwnership::operator(), vec![]),
        ]);
        let names = |skills: Vec<workforce_wasm_runtime::InstalledWasmSkill>| {
            let mut names: Vec<String> = skills.into_iter().map(|s| s.manifest.name).collect();
            names.sort();
            names
        };
        assert_eq!(
            <Arc<WasmSkillStore> as SkillRegistry>::KIND,
            SkillKind::Wasm
        );
        assert_eq!(
            names(SkillRegistry::all_installed(&store)),
            names(store.list_installed())
        );
        let through_arc = SkillRegistry::installed(&store, "mine").unwrap();
        assert_eq!(
            <Arc<WasmSkillStore> as SkillRegistry>::skill_name(&through_arc),
            "mine"
        );
        assert!(SkillRegistry::installed(&store, "nope").is_none());

        let shares = vec![Grant::user("bob", GrantLevel::Run)];
        let shared = SkillRegistry::set_access(
            &store,
            "mine",
            AccessWrite::kept(Visibility::Global, shares.clone()),
        )
        .unwrap();
        assert_eq!(shared.shares, shares);
        let stored = store.get_installed("mine").unwrap();
        assert_eq!(stored.visibility, Visibility::Global);
        assert_eq!(stored.shares, shares);
        assert_eq!(bob().access(&stored), AccessLevel::Run);
    }
}
