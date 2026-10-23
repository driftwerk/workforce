//! `cmd: skill` — the chat surface of the WASM skill consent core
//! ([`crate::skill_consent`]). `cmd:` commands run on user-typed text only
//! (`commands.rs`), which is what makes a chat grant a person's consent and
//! not the agent's.
//!
//! * **`grant <skill> <credential>`** — approve and provision. The value comes
//!   from a named secret the user stashed with `cmd: secret <credential>
//!   <value>`; on a re-grant after a contract change the stored value is kept
//!   and only the fingerprints are re-pinned.
//! * **`grant <skill> tools [shared]`** — approve the declared `[[permissions]]`
//!   for the caller, or (admin) for everyone.
//! * **`grants [<skill>]`** — list the caller's grants with validity.
//! * **`revoke <skill> <credential|tools> [shared]`** — delete the grant and the
//!   stored value.
//! * **`shares`** — who a skill the caller owns is currently shared with.
//! * **`share` / `unshare` / `visibility`** — who else may use (or edit) a
//!   skill the caller owns.

use std::sync::Arc;

use workforce_common::secret_marker::{global_cache, MarkerId};
use workforce_core::access::{GrantLevel, OutgoingAccess, Visibility};
use workforce_skills::catalog::SkillKind;
use workforce_wasm_runtime::InstalledWasmSkill;

use crate::commands::CommandResult;
use crate::endpoint_cmd::{identity_id, split_word};
use crate::resource_access::NO_TEAM_YET;
use crate::serve::AgentService;
use crate::skill_consent::{
    Consent, CredentialGrant, GrantStatus, Refusal, Revocation, ShareChange, ShareOutcome,
    SkillGrants, Surface, Target, ToolsGrant, ValueAction, VisibilityChange,
};

const USAGE: &str = "Usage:\n\
    `cmd: skill grant <skill> <credential>` — approve a declared credential for the \
    installed skill's contract (stash the value first with `cmd: secret <credential> \
    <value>`)\n\
    `cmd: skill grant <skill> tools` — approve the skill's declared [[permissions]] so \
    it may invoke those platform tools as you\n\
    `cmd: skill grant <skill> tools shared` — admin: approve them for every user at once\n\
    `cmd: skill grants [<skill>]` — list your grants and whether they are still valid\n\
    `cmd: skill revoke <skill> <credential|tools>` — remove a grant (and any stored value)\n\
    `cmd: skill shares <skill>` — who it is shared with\n\
    `cmd: skill share <skill> <@user|email|id|team:<name>> [manage]` — let someone use your \
    skill (or edit it)\n\
    `cmd: skill unshare <skill> <@user|email|id|team:<name>>` — take that back\n\
    `cmd: skill visibility <skill> private|team|everyone` — who else may use your skill\n\
    `cmd: skill transfer <skill> <@user|email|id>` — hand your skill to someone else; you \
    keep no access";

const NO_IDENTITY: &str = "I can't tell which identity you are, so I can't record a grant \
                           for you. This usually means your chat account isn't linked to a \
                           Workforce identity yet.";

/// One parsed `cmd: skill` line. Parsing comes before identity and store
/// lookups so a usage slip is answered with usage, whoever typed it.
#[derive(Debug, PartialEq, Eq)]
enum Request<'a> {
    Grant(&'a str, Target),
    Grants(Option<&'a str>),
    Revoke(&'a str, Target),
    Shares(&'a str),
    Share {
        skill: &'a str,
        who: &'a str,
        permission: Option<GrantLevel>,
    },
    Visibility(&'a str, Visibility),
    Transfer {
        skill: &'a str,
        who: &'a str,
    },
}

/// `Err` carries the reply for a line that asks nothing well-formed.
fn parse_request(args: &str) -> Result<Request<'_>, String> {
    let (sub, rest) = split_word(args.trim());
    let usage = || USAGE.to_string();
    match sub {
        "grant" => parse_target(rest)
            .map(|(skill, target)| Request::Grant(skill, target))
            .ok_or_else(usage),
        "revoke" => parse_target(rest)
            .map(|(skill, target)| Request::Revoke(skill, target))
            .ok_or_else(usage),
        "grants" | "list" => match split_word(rest) {
            (filter, "") => Ok(Request::Grants((!filter.is_empty()).then_some(filter))),
            _ => Err(usage()),
        },
        "shares" => match split_word(rest) {
            (skill, "") if !skill.is_empty() => Ok(Request::Shares(skill)),
            _ => Err(usage()),
        },
        "share" | "unshare" => {
            let (skill, rest) = split_word(rest);
            let (who, level) = split_word(rest);
            if skill.is_empty() || who.is_empty() {
                return Err(usage());
            }
            let permission = match (sub, level) {
                ("share", "" | "run") => Some(GrantLevel::Run),
                ("share", "manage") => Some(GrantLevel::Manage),
                ("unshare", "") => None,
                _ => return Err(usage()),
            };
            Ok(Request::Share {
                skill,
                who,
                permission,
            })
        }
        "transfer" => {
            let (skill, rest) = split_word(rest);
            match split_word(rest) {
                (who, "") if !skill.is_empty() && !who.is_empty() => {
                    Ok(Request::Transfer { skill, who })
                }
                _ => Err(usage()),
            }
        }
        "visibility" => {
            let (skill, rest) = split_word(rest);
            let (word, extra) = split_word(rest);
            if skill.is_empty() || word.is_empty() || !extra.is_empty() {
                return Err(usage());
            }
            Visibility::parse_word(word)
                .map(|visibility| Request::Visibility(skill, visibility))
                .ok_or_else(|| {
                    format!("`{word}` is not a visibility — use `private`, `team` or `everyone`.")
                })
        }
        _ => Err(usage()),
    }
}

pub async fn execute_skill(
    service: &Arc<AgentService>,
    channel: &str,
    user: &str,
    args: Option<&str>,
) -> CommandResult {
    let request = match parse_request(args.unwrap_or("")) {
        Ok(request) => request,
        Err(reply) => return CommandResult::info(reply),
    };
    let Some(uid) = identity_id(user) else {
        return CommandResult::info(NO_IDENTITY);
    };
    let consent = Consent::for_service(service, &uid, Surface::Chat);
    let outcome = match request {
        Request::Grant(skill, target) => grant(&consent, channel, skill, target),
        Request::Grants(filter) => grants(&consent, filter),
        Request::Revoke(skill, target) => revoke(&consent, skill, target),
        Request::Shares(skill) => shares(&consent, skill),
        Request::Share {
            skill,
            who,
            permission,
        } => share(&consent, skill, who, permission),
        Request::Visibility(skill, visibility) => set_visibility(&consent, skill, visibility),
        Request::Transfer { skill, who } => transfer(&consent, skill, who),
    };
    CommandResult::info(outcome.unwrap_or_else(explain))
}

/// A refusal in chat terms: `NoValue` names the stash step.
fn explain(refusal: Refusal) -> String {
    match refusal {
        Refusal::NoValue { skill, credential } => format!(
            "No value to grant: stash it first with `cmd: secret {credential} <value>` \
             (in this channel), then run `cmd: skill grant {skill} {credential}` again."
        ),
        other => other.to_string(),
    }
}

/// `tools [shared]` or a credential name — the shared grammar of `grant` and
/// `revoke`. `None` is a usage error.
fn parse_target(rest: &str) -> Option<(&str, Target)> {
    let (skill_name, rest) = split_word(rest);
    let (word, extra) = split_word(rest);
    if skill_name.is_empty() || word.is_empty() {
        return None;
    }
    let target = match (word, extra) {
        ("tools", "") => Target::Tools { shared: false },
        ("tools", "shared") => Target::Tools { shared: true },
        (credential, "") => Target::Credential(credential.to_string()),
        _ => return None,
    };
    Some((skill_name, target))
}

fn grant(
    consent: &Consent,
    channel: &str,
    skill_name: &str,
    target: Target,
) -> Result<String, Refusal> {
    match target {
        Target::Tools { shared } => match consent.grant_tools(skill_name, shared) {
            Ok(grant) => Ok(render_tools_grant(&grant)),
            Err(Refusal::AdminOnly(why)) => Err(Refusal::Denied(format!(
                "{why} Grant for yourself alone with `cmd: skill grant {skill_name} tools`."
            ))),
            Err(refusal) => Err(refusal),
        },
        Target::Credential(credential) => {
            let stashed = MarkerId::named(&credential)
                .ok()
                .and_then(|id| global_cache().and_then(|c| c.lookup(channel, &id)))
                .map(|(secret, _)| secret.expose_secret().clone());
            consent
                .grant_credential(skill_name, &credential, stashed)
                .map(|grant| render_credential_grant(&grant))
        }
    }
}

fn render_credential_grant(grant: &CredentialGrant) -> String {
    let CredentialGrant {
        skill,
        credential,
        scope,
        value,
        hosts,
    } = grant;
    let value_action = match value {
        ValueAction::Stored => "stored the value you stashed and",
        ValueAction::Kept => "kept the existing value and",
    };
    let hosts = match hosts.is_empty() {
        true => "no hosts (the skill declares no [network] allowed_hosts)".to_string(),
        false => hosts.join(", "),
    };
    format!(
        "Granted `{credential}` ({scope:?} scope) to skill `{skill}` — {value_action} \
         pinned the approval to the skill's consent contract. The skill may send it to: \
         {hosts}. Changing the contract (capabilities, network hosts, credentials, \
         permissions) invalidates this grant until you re-run `cmd: skill grant \
         {skill} {credential}`; pure code rebuilds keep it."
    )
}

fn render_tools_grant(grant: &ToolsGrant) -> String {
    let ToolsGrant {
        skill,
        shared,
        permissions,
        shared_stale,
    } = grant;
    let permissions: Vec<String> = permissions
        .iter()
        .map(|p| {
            let hosts = match p.allowed_hosts.is_empty() {
                true => String::new(),
                false => format!(" [URLs limited to: {}]", p.allowed_hosts.join(", ")),
            };
            format!("• `{}` — {}{hosts}", p.tool, p.description)
        })
        .collect();
    let (coverage, re_run) = match shared {
        true => (
            "every user, org-wide; each call still runs with the invoking user's own \
             capability limits",
            format!("`cmd: skill grant {skill} tools shared`"),
        ),
        false => (
            "you alone; every call passes your own capability limits",
            format!("`cmd: skill grant {skill} tools`"),
        ),
    };
    let shared_note = match shared_stale {
        true => format!(
            "\nNote: the org-wide shared grant for `{skill}` is still stale, so other users \
             remain blocked — an admin covers everyone at once with `cmd: skill grant \
             {skill} tools shared`."
        ),
        false => String::new(),
    };
    format!(
        "Granted tool access to skill `{skill}` (this consent contract only) for \
         {coverage} — plus the policy engine. It may invoke:\n{}\nChanging the contract \
         (capabilities, network hosts, credentials, permissions) invalidates this grant \
         until {re_run} is re-run; pure code rebuilds keep it.{shared_note}",
        permissions.join("\n")
    )
}

fn grants(consent: &Consent, filter: Option<&str>) -> Result<String, Refusal> {
    let skills = consent.grants(filter)?;
    if skills.is_empty() {
        return Ok(match filter {
            None => "No WASM skills are installed that you can use.".to_string(),
            Some(name) => format!("No WASM skill named `{name}` is installed that you can use."),
        });
    }
    let lines: Vec<String> = skills
        .iter()
        .flat_map(|grants| render_grants(grants, consent.uid()))
        .collect();
    Ok(format!("Skill grants:\n{}", lines.join("\n")))
}

fn status_label(status: GrantStatus) -> &'static str {
    match status {
        GrantStatus::Valid => "granted",
        GrantStatus::Stale => "stale — the contract changed; re-approve with `cmd: skill grant`",
        GrantStatus::Missing => "not granted",
    }
}

fn render_grants(grants: &SkillGrants, uid: &str) -> Vec<String> {
    let name = &grants.skill.manifest.name;
    let mut lines = vec![ownership_line(&grants.skill, uid)];
    if let Some(tools) = &grants.tools {
        lines.push(format!(
            "• `{name}` / tools ({} declared): yours {}, shared {}",
            tools.declared,
            status_label(tools.own),
            status_label(tools.shared),
        ));
    }
    lines.extend(grants.credentials.iter().map(|cred| {
        format!(
            "• `{name}` / `{}` ({:?}): {}, {}",
            cred.name,
            cred.scope,
            status_label(cred.status),
            match cred.value_stored {
                true => "value stored",
                false => "no value",
            }
        )
    }));
    lines
}

fn revoke(consent: &Consent, skill_name: &str, target: Target) -> Result<String, Refusal> {
    let Revocation {
        skill,
        target,
        removed,
    } = consent.revoke(skill_name, target)?;
    Ok(match (target, removed) {
        (Target::Tools { .. }, true) => format!("Revoked tool access for skill `{skill}`."),
        (Target::Tools { shared: true }, false) => {
            format!("Skill `{skill}` had no shared tools grant.")
        }
        (Target::Tools { shared: false }, false) => {
            format!("Skill `{skill}` had no tools grant from you.")
        }
        (Target::Credential(credential), true) => {
            format!("Revoked `{credential}` of skill `{skill}` — the skill can no longer read it.")
        }
        (Target::Credential(credential), false) => format!(
            "Nothing to revoke: no grant or value stored for `{credential}` of `{skill}` in \
             your scope."
        ),
    })
}

/// Hand a skill the caller owns to somebody else. The whole operation —
/// standing, write and audit — is [`Consent::transfer`]'s; this wording is
/// all the chat surface adds. The grammar offers no keep-access choice, so the
/// transfer is plain: `cmd: skill share` afterwards is the new owner's call.
fn transfer(consent: &Consent, skill_name: &str, who: &str) -> Result<String, Refusal> {
    let outcome = consent.transfer(skill_name, who, OutgoingAccess::Nothing)?;
    Ok(crate::skill_consent::skill_transfer_notes(&outcome).join(" "))
}

/// Who a skill is shared with, named — the set `share`/`unshare` edit.
fn shares(consent: &Consent, skill_name: &str) -> Result<String, Refusal> {
    let grantees = consent.shares(skill_name)?;
    if grantees.is_empty() {
        return Ok(format!(
            "`{skill_name}` isn't shared with anyone. Share it with \
             `cmd: skill share {skill_name} <user>`."
        ));
    }
    let listed: Vec<String> = grantees
        .iter()
        .map(|grantee| {
            format!(
                "{} ({})",
                grantee.display,
                match grantee.permission {
                    GrantLevel::Run => "run",
                    GrantLevel::Manage => "manage",
                }
            )
        })
        .collect();
    Ok(format!(
        "`{skill_name}` is shared with: {}.",
        listed.join(", ")
    ))
}

fn share(
    consent: &Consent,
    skill_name: &str,
    who: &str,
    permission: Option<GrantLevel>,
) -> Result<String, Refusal> {
    let ShareOutcome {
        skill,
        recipient,
        change,
        kind,
    } = consent.share(skill_name, who, permission)?;
    let (named, opening) = (recipient.named(), recipient.opening());
    Ok(match (change, kind) {
        (ShareChange::SelfShare, _) => {
            format!("You already own `{skill}` — sharing it with yourself changes nothing.")
        }
        (ShareChange::NotShared, _) => format!("`{skill}` was not shared with {named}."),
        (ShareChange::Removed, _) => format!("`{skill}` is no longer shared with {named}."),
        (ShareChange::Added(GrantLevel::Manage), SkillKind::Wasm) => format!(
            "{opening} can now use **and edit** `{skill}`. They still grant their own \
             credentials with `cmd: skill grant {skill} <credential>`."
        ),
        (ShareChange::Added(GrantLevel::Run), SkillKind::Wasm) => format!(
            "{opening} can now use `{skill}`. They grant their own credentials with \
             `cmd: skill grant {skill} <credential>`. Add `manage` to let them edit it."
        ),
        (ShareChange::Added(GrantLevel::Manage), SkillKind::External) => {
            format!("{opening} can now use **and edit** `{skill}`.")
        }
        (ShareChange::Added(GrantLevel::Run), SkillKind::External) => {
            format!("{opening} can now use `{skill}`. Add `manage` to let them edit it.")
        }
    })
}

fn set_visibility(
    consent: &Consent,
    skill_name: &str,
    visibility: Visibility,
) -> Result<String, Refusal> {
    let VisibilityChange {
        skill,
        visibility,
        shares,
        kind,
        owner_has_no_team,
    } = consent.set_visibility(skill_name, visibility)?;
    let reach = match (visibility, kind) {
        (Visibility::Global, SkillKind::Wasm) => format!(
            "`{skill}` is now available to everyone. Each user grants their own \
             credentials with `cmd: skill grant {skill} <credential>`."
        ),
        (Visibility::Global, SkillKind::External) => {
            format!("`{skill}` is now available to everyone.")
        }
        (Visibility::Team, SkillKind::Wasm) => format!(
            "`{skill}` is now available to anyone in a team with you. Each user grants their \
             own credentials with `cmd: skill grant {skill} <credential>`."
        ),
        (Visibility::Team, SkillKind::External) => {
            format!("`{skill}` is now available to anyone in a team with you.")
        }
        (Visibility::Private, _) => format!(
            "`{skill}` is private again — you{} can use it.",
            match shares {
                0 => String::new(),
                n => format!(" and the {n} user(s) you shared it with"),
            }
        ),
    };
    Ok(match owner_has_no_team {
        true => format!("{reach} {NO_TEAM_YET}"),
        false => reach,
    })
}

/// Who a skill belongs to and who can reach it, from the reader's point of view
/// — the header each skill's grant lines hang under.
fn ownership_line(skill: &InstalledWasmSkill, uid: &str) -> String {
    let mine = skill.owner.as_deref() == Some(uid);
    let belongs = match (&skill.owner, mine) {
        (None, _) => "installed for the org",
        (Some(_), true) => "yours",
        (Some(_), false) => "another user's",
    };
    let shared = match skill.shares.len() {
        0 => String::new(),
        n => format!(", shared with {n} user(s)"),
    };
    let reach = match skill.owner.is_none() {
        true => "everyone",
        false => skill.visibility.word(),
    };
    format!(
        "**`{}`** — {belongs}, visible to {reach}{shared}",
        skill.manifest.name
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::skill_consent::tests::{installed_fixture, Harness};
    use workforce_core::access::Grant;

    #[test]
    fn the_grant_grammar_separates_tools_from_credentials() {
        assert_eq!(
            parse_target("enabler api_token"),
            Some(("enabler", Target::Credential("api_token".into())))
        );
        assert_eq!(
            parse_target("enabler tools"),
            Some(("enabler", Target::Tools { shared: false }))
        );
        assert_eq!(
            parse_target("enabler tools shared"),
            Some(("enabler", Target::Tools { shared: true }))
        );
        assert_eq!(parse_target("enabler"), None);
        assert_eq!(parse_target("enabler api_token extra"), None);
        assert_eq!(parse_target("enabler tools everyone"), None);
    }

    #[test]
    fn a_line_is_parsed_before_anyone_is_looked_up() {
        assert_eq!(parse_request(""), Err(USAGE.to_string()));
        assert_eq!(parse_request("dance"), Err(USAGE.to_string()));
        assert_eq!(parse_request("grant enabler"), Err(USAGE.to_string()));
        assert_eq!(parse_request("grants a b"), Err(USAGE.to_string()));
        assert_eq!(parse_request("share enabler"), Err(USAGE.to_string()));
        assert_eq!(
            parse_request("share enabler bob owner"),
            Err(USAGE.to_string())
        );
        assert_eq!(
            parse_request("unshare enabler bob manage"),
            Err(USAGE.to_string())
        );
        assert_eq!(
            parse_request("visibility enabler public"),
            Err("`public` is not a visibility — use `private`, `team` or `everyone`.".into())
        );
        assert_eq!(
            parse_request("grant enabler tools shared"),
            Ok(Request::Grant("enabler", Target::Tools { shared: true }))
        );
        assert_eq!(parse_request("grants"), Ok(Request::Grants(None)));
        assert_eq!(
            parse_request("list enabler"),
            Ok(Request::Grants(Some("enabler")))
        );
        assert_eq!(
            parse_request("revoke enabler api_token"),
            Ok(Request::Revoke(
                "enabler",
                Target::Credential("api_token".into())
            ))
        );
        assert_eq!(
            parse_request("share enabler <@U1> manage"),
            Ok(Request::Share {
                skill: "enabler",
                who: "<@U1>",
                permission: Some(GrantLevel::Manage)
            })
        );
        assert_eq!(
            parse_request("unshare enabler bob@x.com"),
            Ok(Request::Share {
                skill: "enabler",
                who: "bob@x.com",
                permission: None
            })
        );
        assert_eq!(
            parse_request("visibility enabler everyone"),
            Ok(Request::Visibility("enabler", Visibility::Global))
        );
    }

    #[test]
    fn a_missing_value_names_the_stash_step() {
        let msg = explain(Refusal::NoValue {
            skill: "enabler".into(),
            credential: "api_token".into(),
        });
        assert!(msg.contains("cmd: secret api_token <value>"), "{msg}");
        assert!(msg.contains("cmd: skill grant enabler api_token"), "{msg}");
        assert_eq!(
            explain(Refusal::VaultUnavailable),
            Refusal::VaultUnavailable.to_string()
        );
    }

    #[test]
    fn a_shared_tools_grant_by_a_member_points_at_the_personal_command_once() {
        let h = Harness::new();
        h.publish();
        let msg = grant(
            &h.consent("bob", false),
            "C1",
            "enabler",
            Target::Tools { shared: true },
        )
        .unwrap_err()
        .to_string();
        assert_eq!(msg.matches("Grant for yourself alone").count(), 1, "{msg}");
        assert!(msg.contains("`cmd: skill grant enabler tools`"), "{msg}");
        let unknown = grant(
            &h.consent("bob", false),
            "C1",
            "ghost",
            Target::Tools { shared: true },
        )
        .unwrap_err()
        .to_string();
        assert!(!unknown.contains("yourself alone"), "{unknown}");
    }

    #[test]
    fn the_stale_shared_note_appears_exactly_when_flagged() {
        let h = Harness::new();
        h.publish();
        let mut grant = h
            .consent("bob", false)
            .grant_tools("enabler", false)
            .unwrap();
        assert!(!render_tools_grant(&grant).contains("still stale"));
        grant.shared_stale = true;
        let note = render_tools_grant(&grant);
        assert!(note.contains("still stale"), "{note}");
        assert!(
            note.contains("cmd: skill grant enabler tools shared"),
            "{note}"
        );
    }

    #[test]
    fn a_credential_grant_reads_back_the_hosts_and_the_re_run_command() {
        let h = Harness::new();
        h.publish();
        let grant = h
            .consent("bob", false)
            .grant_credential("enabler", "api_token", Some("v".into()))
            .unwrap();
        let text = render_credential_grant(&grant);
        assert!(text.contains("stored the value you stashed"), "{text}");
        assert!(text.contains("api.example.com"), "{text}");
        assert!(
            text.contains("cmd: skill grant enabler api_token"),
            "{text}"
        );
    }

    #[test]
    fn revocations_say_what_was_removed_or_that_nothing_was() {
        let h = Harness::new();
        h.publish();
        let consent = h.consent("bob", false);
        let tools = || Target::Tools { shared: false };
        assert!(revoke(&consent, "enabler", tools())
            .unwrap()
            .contains("had no tools grant from you"));
        consent.grant_tools("enabler", false).unwrap();
        assert!(revoke(&consent, "enabler", tools())
            .unwrap()
            .contains("Revoked tool access"));
        assert!(
            revoke(&consent, "enabler", Target::Credential("api_token".into()))
                .unwrap()
                .contains("Nothing to revoke")
        );
    }

    #[test]
    fn wasm_replies_are_unchanged() {
        use workforce_identity::sqlite::SqliteIdentityStore;
        use workforce_identity::store::IdentityStore;
        use workforce_identity::types::{Identity, IdentityRole};

        let identities = Arc::new(SqliteIdentityStore::in_memory().unwrap());
        let mut alice = Identity::new("Alice", IdentityRole::Member);
        alice.email = Some("alice@example.com".into());
        let mut bob = Identity::new("Bob", IdentityRole::Member);
        bob.email = Some("bob@example.com".into());
        identities.create(&alice).unwrap();
        identities.create(&bob).unwrap();
        let h = Harness::new();
        let mut record = h.store.get_installed("enabler").unwrap();
        record.owner = Some(alice.id.to_string());
        h.store.update_metadata("enabler", &record).unwrap();
        let owner = h.consent_with(
            &alice.id.to_string(),
            false,
            Some(identities as Arc<dyn IdentityStore>),
        );

        let reply = |who: &str, permission| share(&owner, "enabler", who, permission).unwrap();
        assert_eq!(
            reply("bob@example.com", Some(GrantLevel::Run)),
            "Bob can now use `enabler`. They grant their own credentials with \
             `cmd: skill grant enabler <credential>`. Add `manage` to let them edit it."
        );
        assert_eq!(
            reply("bob@example.com", Some(GrantLevel::Manage)),
            "Bob can now use **and edit** `enabler`. They still grant their own credentials \
             with `cmd: skill grant enabler <credential>`."
        );
        assert_eq!(
            set_visibility(&owner, "enabler", Visibility::Private).unwrap(),
            "`enabler` is private again — you and the 1 user(s) you shared it with can use it."
        );
        assert_eq!(
            reply("bob@example.com", None),
            "`enabler` is no longer shared with Bob."
        );
        assert_eq!(
            reply("bob@example.com", None),
            "`enabler` was not shared with Bob."
        );
        assert_eq!(
            reply("alice@example.com", Some(GrantLevel::Run)),
            "You already own `enabler` — sharing it with yourself changes nothing."
        );
        assert_eq!(
            set_visibility(&owner, "enabler", Visibility::Global).unwrap(),
            "`enabler` is now available to everyone. Each user grants their own credentials \
             with `cmd: skill grant enabler <credential>`."
        );
        assert_eq!(
            set_visibility(&owner, "enabler", Visibility::Private).unwrap(),
            "`enabler` is private again — you can use it."
        );
    }

    #[test]
    fn the_shares_listing_names_every_grantee() {
        use crate::skill_consent::tests::alice_and_bob;

        let (identities, alice, _bob) = alice_and_bob();
        let h = Harness::new();
        let mut record = h.store.get_installed("enabler").unwrap();
        record.owner = Some(alice.clone());
        h.store.update_metadata("enabler", &record).unwrap();
        let owner = h.consent_with(&alice, false, Some(identities));

        assert_eq!(
            shares(&owner, "enabler").unwrap(),
            "`enabler` isn't shared with anyone. Share it with \
             `cmd: skill share enabler <user>`."
        );
        share(
            &owner,
            "enabler",
            "bob@example.com",
            Some(GrantLevel::Manage),
        )
        .unwrap();
        assert_eq!(
            shares(&owner, "enabler").unwrap(),
            "`enabler` is shared with: Bob (manage)."
        );
        // Standing comes first: a listing is the owner's to read.
        assert!(shares(&h.consent("dave", false), "enabler")
            .unwrap_err()
            .to_string()
            .contains("belongs to another user"));
    }

    #[test]
    fn external_share_and_visibility_replies_leave_out_credentials() {
        use crate::skill_consent::tests::{alice_and_bob, external_owned_by};

        let (identities, alice, _bob) = alice_and_bob();
        let (_dir, skills) = external_owned_by(&alice);
        let h = Harness::new();
        let owner = h
            .consent_with(&alice, false, Some(identities))
            .with_external(skills, false);

        assert_eq!(
            share(
                &owner,
                "hello-world",
                "bob@example.com",
                Some(GrantLevel::Run)
            )
            .unwrap(),
            "Bob can now use `hello-world`. Add `manage` to let them edit it."
        );
        assert_eq!(
            share(
                &owner,
                "hello-world",
                "bob@example.com",
                Some(GrantLevel::Manage)
            )
            .unwrap(),
            "Bob can now use **and edit** `hello-world`."
        );
        assert_eq!(
            set_visibility(&owner, "hello-world", Visibility::Global).unwrap(),
            "`hello-world` is now available to everyone."
        );
        assert_eq!(
            set_visibility(&owner, "hello-world", Visibility::Private).unwrap(),
            "`hello-world` is private again — you and the 1 user(s) you shared it with can use it."
        );
        assert_eq!(
            share(&owner, "hello-world", "bob@example.com", None).unwrap(),
            "`hello-world` is no longer shared with Bob."
        );
    }

    #[test]
    fn the_ownership_line_reads_from_the_caller() {
        let mut skill = installed_fixture("mine", Some("alice"));
        assert!(ownership_line(&skill, "alice").contains("yours"));
        assert!(ownership_line(&skill, "bob").contains("another user's"));
        assert!(ownership_line(&skill, "alice").contains("visible to private"));

        skill.visibility = Visibility::Global;
        skill.shares = vec![Grant::user("bob", GrantLevel::Run)];
        let line = ownership_line(&skill, "alice");
        assert!(line.contains("visible to everyone"), "{line}");
        assert!(line.contains("shared with 1 user(s)"), "{line}");

        let org = installed_fixture("org", None);
        let line = ownership_line(&org, "alice");
        assert!(line.contains("installed for the org"), "{line}");
        assert!(line.contains("visible to everyone"), "{line}");
    }

    #[test]
    fn the_grants_listing_renders_every_line_under_its_skill() {
        let h = Harness::new();
        let consent = h.consent("alice", false);
        consent
            .grant_credential("enabler", "api_token", Some("v".into()))
            .unwrap();
        let text = grants(&consent, None).unwrap();
        assert!(text.contains("**`enabler`** — yours"), "{text}");
        assert!(
            text.contains("/ tools (1 declared): yours not granted, shared not granted"),
            "{text}"
        );
        assert!(
            text.contains("`api_token` (User): granted, value stored"),
            "{text}"
        );
        assert!(
            text.contains("`shared_key` (Shared): not granted, no value"),
            "{text}"
        );
        assert!(grants(&consent, Some("other"))
            .unwrap()
            .contains("No WASM skill named `other`"));
    }
}
