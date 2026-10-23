//! `cmd: workflow|playbook|monitor` — who else may reach one definition, typed
//! by the person whose definition it is. One grammar serves all three kinds:
//! the row, the rule ([`crate::def_write_auth`]) and the refusals are already
//! kind-generic, so only the noun changes.
//!
//! Like every `cmd:`, these run on user-typed chat text only (`commands.rs`),
//! which is what makes a chat grant a person's consent rather than an agent's
//! — the reason no agent tool carries them ([`crate::skill_consent`]).

use std::sync::Arc;

use workforce_core::access::{Grant, GrantLevel, OutgoingAccess, PrincipalKind, Visibility};
use workforce_identity::store::IdentityStore;
use workforce_identity::team::TeamStore;
use workforce_orchestrator::workflow_store::{
    WorkflowStore, KIND_MONITOR, KIND_PLAYBOOK, KIND_WORKFLOW,
};

use crate::authz::is_admin;
use crate::commands::CommandResult;
use crate::def_write_auth::{
    authorize_share_manage, authorize_share_read, transfer_definition, transfer_notes, DefOp,
    DefTransfer, DefWriter, StoredShares, TransferStores,
};
use crate::endpoint_cmd::identity_id;
use crate::hosts::{DefinitionSurface, MonitorHost};
use crate::recipients::{self, Recipient};
use crate::resource_access::NO_TEAM_YET;
use crate::serve::AgentService;
use crate::team_reach::TeamReach;

const OWNER_PREFIX: &str = "owner:";

/// The definition kinds this grammar serves: the word a person types, and the
/// `kind` the store keeps their definitions under.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DefNoun {
    Workflow,
    Playbook,
    Monitor,
}

impl DefNoun {
    const fn word(self) -> &'static str {
        match self {
            DefNoun::Workflow => "workflow",
            DefNoun::Playbook => "playbook",
            DefNoun::Monitor => "monitor",
        }
    }

    const fn kind(self) -> &'static str {
        match self {
            DefNoun::Workflow => KIND_WORKFLOW,
            DefNoun::Playbook => KIND_PLAYBOOK,
            DefNoun::Monitor => KIND_MONITOR,
        }
    }

    /// The plural that opens a sentence about the hub rather than about one
    /// definition.
    const fn plural(self) -> &'static str {
        match self {
            DefNoun::Workflow => "Workflows",
            DefNoun::Playbook => "Playbooks",
            DefNoun::Monitor => "Monitors",
        }
    }

    /// What a `run` share lets its holder do with this kind: a workflow runs, a
    /// playbook is worked through step by step, and a monitor watches by
    /// itself — nobody starts one, they only ever look at it.
    const fn verb(self) -> &'static str {
        match self {
            DefNoun::Workflow => "run",
            DefNoun::Playbook => "start",
            DefNoun::Monitor => "see",
        }
    }
}

/// What `cmd: <noun>` answers a line it cannot read with. Starting one is each
/// kind's own business — a workflow runs, a playbook is worked through, a
/// monitor watches by itself — so only the lines below that are shared.
pub fn usage(noun: DefNoun) -> String {
    let word = noun.word();
    let verb = noun.verb();
    let run = match noun {
        DefNoun::Workflow => {
            "`cmd: workflow run <workflow-id> [key=value …]` — start a workflow now; its \
             report posts in this channel\n"
        }
        DefNoun::Playbook => {
            "`cmd: playbook list` — the playbooks you can start\n\
             `cmd: playbook start <playbook-id>` — work through one step by step \
             (then `cmd: playbook status`, `cmd: continue`, `cmd: exit`)\n"
        }
        DefNoun::Monitor => "",
    };
    format!(
        "Usage:\n{run}\
         `cmd: {word} shares <{word}-id>` — who it is shared with\n\
         `cmd: {word} share <{word}-id> <@user|email|id|team:<name>> [manage]` — let someone \
         {verb} it (add `manage` to let them edit it)\n\
         `cmd: {word} unshare <{word}-id> <@user|email|id|team:<name>>` — take that back\n\
         `cmd: {word} visibility <{word}-id> private|team|everyone` — who else may {verb} it\n\
         `cmd: {word} transfer <{word}-id> <@user|email|id>` — hand it to someone else; \
         you keep no access\n\
         Add `owner:<@user|email|id>` to address a {word} of theirs you may manage."
    )
}

/// One parsed line. Parsing happens before every lookup, so a usage slip is
/// answered with usage whoever typed it and whatever is stored.
#[derive(Debug, PartialEq, Eq)]
struct Line<'a> {
    id: &'a str,
    /// The `owner:<…>` qualifier: whose row to address, when not one's own.
    owner: Option<&'a str>,
    request: Request<'a>,
}

#[derive(Debug, PartialEq, Eq)]
enum Request<'a> {
    Shares,
    Share {
        who: &'a str,
        permission: GrantLevel,
    },
    Unshare {
        who: &'a str,
    },
    Visibility(Visibility),
    Transfer {
        who: &'a str,
    },
}

/// Who takes the standing for a line before it runs.
enum Standing {
    /// Reading who a definition reaches takes as much as changing it, and
    /// records nothing.
    Read,
    Write(DefOp),
    /// A transfer takes its own, inside the one function that performs one
    /// ([`crate::def_write_auth::transfer_definition`]) — so the audit can
    /// never be split from the move, and so no surface records a second
    /// override for a single action.
    Deferred,
}

impl Request<'_> {
    const fn standing(&self) -> Standing {
        match self {
            Request::Shares => Standing::Read,
            Request::Share { .. } | Request::Unshare { .. } => Standing::Write(DefOp::Share),
            Request::Visibility(_) => Standing::Write(DefOp::Visibility),
            Request::Transfer { .. } => Standing::Deferred,
        }
    }
}

fn parse_line(noun: DefNoun, args: &str) -> Result<Line<'_>, String> {
    let (qualifiers, words): (Vec<&str>, Vec<&str>) = args
        .split_whitespace()
        .partition(|word| word.starts_with(OWNER_PREFIX));
    let owner = match qualifiers.as_slice() {
        [] => None,
        // The partition guarantees the prefix; an empty remainder names nobody.
        [only] => Some(
            only.strip_prefix(OWNER_PREFIX)
                .filter(|named| !named.is_empty())
                .ok_or_else(|| usage(noun))?,
        ),
        _ => return Err(usage(noun)),
    };
    let (id, request) = match words.as_slice() {
        ["shares", id] => (*id, Request::Shares),
        ["share", id, who] | ["share", id, who, "run"] => (
            *id,
            Request::Share {
                who,
                permission: GrantLevel::Run,
            },
        ),
        ["share", id, who, "manage"] => (
            *id,
            Request::Share {
                who,
                permission: GrantLevel::Manage,
            },
        ),
        ["unshare", id, who] => (*id, Request::Unshare { who }),
        ["visibility", id, word] => (*id, Request::Visibility(parse_visibility(word)?)),
        ["transfer", id, who] => (*id, Request::Transfer { who }),
        _ => return Err(usage(noun)),
    };
    Ok(Line { id, owner, request })
}

/// The visibility a person typed.
fn parse_visibility(word: &str) -> Result<Visibility, String> {
    Visibility::parse_word(word).ok_or_else(|| {
        format!("`{word}` is not a visibility — use `private`, `team` or `everyone`.")
    })
}

/// The runtime holding one kind of definition, as this grammar reads it: where
/// a row lives, who owns it, and how to make a change take effect.
enum DefSurface<'a> {
    Workflow(&'a dyn DefinitionSurface),
    Playbook(&'a dyn DefinitionSurface),
    Monitor(&'a dyn MonitorHost),
}

impl<'a> DefSurface<'a> {
    fn of(service: &'a Arc<AgentService>, noun: DefNoun) -> Option<Self> {
        match noun {
            DefNoun::Workflow => service
                .workflow_runtime()
                .map(|runtime| DefSurface::Workflow(runtime.as_ref())),
            DefNoun::Playbook => service
                .playbook_runtime()
                .map(|runtime| DefSurface::Playbook(runtime.as_ref())),
            // Monitors reach their store through the engine, exactly as the
            // gateway's share routes do, so both surfaces address the same set.
            DefNoun::Monitor => service
                .monitor_engine()
                .map(|engine| DefSurface::Monitor(engine.as_ref())),
        }
    }

    fn definitions(&self) -> &dyn DefinitionSurface {
        match self {
            DefSurface::Workflow(definitions) | DefSurface::Playbook(definitions) => *definitions,
            DefSurface::Monitor(engine) => engine.definitions(),
        }
    }

    fn def_store(&self) -> Option<&Arc<WorkflowStore>> {
        self.definitions().def_store()
    }

    /// The owner of the row `(id, owner)` addresses, or `None` when no such
    /// row is loaded.
    async fn owner_of(&self, id: &str, owner: Option<&str>) -> Option<Option<String>> {
        self.definitions()
            .definition_ownership(id, owner)
            .await
            .map(|(owner, _visibility)| owner)
    }

    /// Every member who owns a definition with this bare id.
    async fn owners_of(&self, id: &str) -> Vec<String> {
        member_owners(self.definitions().owners().await.into_iter(), id)
    }

    /// Re-read the definitions so a change of who reaches one takes effect
    /// here without waiting for the store poll.
    async fn reload(&self) -> eyre::Result<()> {
        let reloaded = self.definitions().reload().await.map(drop);
        // A transferred definition is tainted, so a persona monitor's
        // grantee-controlled prompt must stop driving its channel now
        // rather than at the next sweep.
        if let DefSurface::Monitor(engine) = self {
            engine.reconcile_personas().await;
        }
        reloaded
    }
}

fn member_owners(rows: impl Iterator<Item = (String, Option<String>)>, id: &str) -> Vec<String> {
    rows.filter(|(row_id, _)| row_id == id)
        .filter_map(|(_, owner)| owner)
        .collect()
}

pub async fn execute_def(
    service: &Arc<AgentService>,
    noun: DefNoun,
    user: &str,
    args: Option<&str>,
) -> CommandResult {
    let line = match parse_line(noun, args.unwrap_or("")) {
        Ok(line) => line,
        Err(reply) => return CommandResult::info(reply),
    };
    let Some(uid) = identity_id(user) else {
        return CommandResult::info(format!(
            "Only a signed-in user can change who reaches a {}.",
            noun.word()
        ));
    };
    let Some(surface) = DefSurface::of(service, noun) else {
        return CommandResult::info(format!("{} are not available on this hub.", noun.plural()));
    };
    let Some(store) = surface.def_store().cloned() else {
        return CommandResult::info(format!(
            "Sharing needs a database store; this hub keeps its {}s on the filesystem.",
            noun.word()
        ));
    };
    let teams = service.team_store.read().clone();
    let chat = DefChat {
        noun,
        surface,
        store,
        identities: service.identity_store.read().clone(),
        reach: TeamReach::new(teams.clone(), Some(uid.clone())),
        teams,
        is_admin: is_admin(service, &uid),
        uid,
    };
    CommandResult::info(chat.apply(&line).await.unwrap_or_else(|refusal| refusal))
}

/// One person acting on one kind of definition from chat: the runtime that
/// holds it, the store behind it, and the stores that turn an id into a name.
struct DefChat<'a> {
    noun: DefNoun,
    surface: DefSurface<'a>,
    store: Arc<WorkflowStore>,
    identities: Option<Arc<dyn IdentityStore>>,
    teams: Option<Arc<dyn TeamStore>>,
    /// The caller's own team standing, for the rows their teams reach.
    reach: TeamReach,
    uid: String,
    is_admin: bool,
}

impl DefChat<'_> {
    /// Place the line on a row, check the rule once, then do the one thing
    /// asked. Every subcommand takes the standing to change who reaches the
    /// definition, so reading the grantee set needs as much as writing it —
    /// only the write is recorded when an administrator overrides its owner.
    async fn apply(&self, line: &Line<'_>) -> Result<String, String> {
        let owner = self.resolve_owner(line).await?;
        let (writer, shares) = (self.writer(), self.shares());
        let (kind, id) = (self.noun.kind(), line.id);
        match line.request.standing() {
            Standing::Write(op) => {
                authorize_share_manage(&writer, &shares, kind, id, &owner, op).await
            }
            Standing::Read => authorize_share_read(&writer, &shares, kind, id, &owner).await,
            Standing::Deferred => Ok(()),
        }
        .map_err(|denied| denied.to_string())?;
        match &line.request {
            Request::Shares => self.list_shares(line.id, &owner).await,
            Request::Share { who, permission } => {
                self.share(line.id, &owner, who, Some(*permission)).await
            }
            Request::Unshare { who } => self.share(line.id, &owner, who, None).await,
            Request::Visibility(visibility) => {
                self.set_visibility(line.id, &owner, *visibility).await
            }
            Request::Transfer { who } => self.transfer(line.id, &owner, who).await,
        }
    }

    fn writer(&self) -> DefWriter {
        DefWriter {
            identity_id: Some(self.uid.clone()),
            is_admin: self.is_admin,
        }
    }

    fn shares(&self) -> StoredShares {
        StoredShares::new(Some(Arc::clone(&self.store)), self.teams.clone())
    }

    /// The row a line addresses: the one `owner:` names, else the caller's own,
    /// else the single definition with that id the caller may manage.
    async fn resolve_owner(&self, line: &Line<'_>) -> Result<String, String> {
        if let Some(token) = line.owner {
            let Recipient::User { id: named, .. } = self.person(token)? else {
                return Err(recipients::person_not_team(self.noun.word()));
            };
            return match self.surface.owner_of(line.id, Some(&named)).await {
                Some(Some(owner)) => Ok(owner),
                _ => Err(self.not_yours(line.id)),
            };
        }
        if let Some(Some(mine)) = self.surface.owner_of(line.id, Some(&self.uid)).await {
            return Ok(mine);
        }
        let managed = self.managed_owners(line.id).await;
        let mut candidates = self.surface.owners_of(line.id).await;
        candidates.retain(|owner| managed.contains(owner));
        match candidates.len() {
            0 => Err(self.unaddressable(line.id).await),
            1 => Ok(candidates.swap_remove(0)),
            _ => {
                let mut names: Vec<String> = candidates
                    .iter()
                    .map(|owner| recipients::display_name(self.identities.as_deref(), owner))
                    .collect();
                names.sort();
                Err(format!(
                    "More than one {} is called `{}`: {}. Name one with `owner:<@user|email|id>`.",
                    self.noun.word(),
                    line.id,
                    possessives(&names)
                ))
            }
        }
    }

    /// The owners of definitions called `id` that the caller holds a `manage`
    /// share on — the other rows a bare id can land on.
    async fn managed_owners(&self, id: &str) -> Vec<String> {
        self.store
            .shares_for_caller(self.noun.kind(), Some(&self.uid), self.reach.team_ids())
            .await
            .map(|grants| {
                grants
                    .into_iter()
                    .filter(|grant| {
                        grant.def_id == id && grant.permission.allows(GrantLevel::Manage)
                    })
                    .map(|grant| grant.owner)
                    .collect()
            })
            .unwrap_or_default()
    }

    /// The refusal for a bare id no row of the caller's answers to. An
    /// install-wide definition of that name is named for what it is: it belongs
    /// to nobody, so there is no owner to share it on anyone's behalf — the same
    /// answer the gateway's share routes give a system row.
    async fn unaddressable(&self, id: &str) -> String {
        match self.surface.owner_of(id, None).await {
            Some(_) => format!(
                "`{id}` belongs to the whole install rather than to a member, so there is \
                 nobody to share it with."
            ),
            None => self.not_yours(id),
        }
    }

    fn not_yours(&self, id: &str) -> String {
        format!("No {} called `{id}` that you can manage.", self.noun.word())
    }

    fn person(&self, token: &str) -> Result<Recipient, String> {
        let store = self
            .identities
            .as_deref()
            .ok_or_else(|| recipients::NO_IDENTITY_STORE.to_string())?;
        recipients::resolve(store, self.teams.as_deref(), token)
    }

    async fn list_shares(&self, id: &str, owner: &str) -> Result<String, String> {
        let shares = self.stored_shares(id, owner).await?;
        if shares.is_empty() {
            return Ok(format!(
                "`{id}` isn't shared with anyone. Share it with `cmd: {} share {id} <user>`.",
                self.noun.word()
            ));
        }
        let listed: Vec<String> = shares
            .iter()
            .map(|grant| format!("{} ({})", self.grantee(grant), grant.permission.as_str()))
            .collect();
        Ok(format!("`{id}` is shared with: {}.", listed.join(", ")))
    }

    async fn stored_shares(&self, id: &str, owner: &str) -> Result<Vec<Grant>, String> {
        self.store
            .list_shares(self.noun.kind(), id, owner)
            .await
            .map_err(|e| format!("Could not read who `{id}` is shared with: {e}"))
    }

    /// How one grantee reads: the person's name, or the team's — each falling
    /// back to the bare id, because a grant outlives the account or team it
    /// names.
    fn grantee(&self, grant: &Grant) -> String {
        match grant.principal_type {
            PrincipalKind::User => {
                recipients::display_name(self.identities.as_deref(), &grant.principal_id)
            }
            PrincipalKind::Team => format!(
                "{} team",
                recipients::team_name(self.teams.as_deref(), &grant.principal_id)
                    .unwrap_or_else(|| grant.principal_id.clone())
            ),
        }
    }

    async fn share(
        &self,
        id: &str,
        owner: &str,
        who: &str,
        permission: Option<GrantLevel>,
    ) -> Result<String, String> {
        let recipient = self.person(who)?;
        if matches!(&recipient, Recipient::User { id, .. } if id == owner) {
            return Ok(format!(
                "`{id}` is already yours — there's nothing to share."
            ));
        }
        let named = recipient.named();
        let existing = self.stored_shares(id, owner).await?;
        let shares = apply_share(&existing, &recipient, permission);
        if permission.is_none() && shares.len() == existing.len() {
            return Ok(format!("`{id}` wasn't shared with {named}."));
        }
        self.store
            .set_shares(self.noun.kind(), id, owner, &shares, &self.uid)
            .await
            .map_err(|e| format!("Could not update sharing: {e}"))?;
        tracing::info!(
            target: "sentinel.audit",
            kind = self.noun.kind(), definition = %id, owner = %owner, actor = %self.uid,
            recipient = %recipient.id(), principal = recipient.principal().as_str(),
            added = permission.is_some(), permission = ?permission,
            "definition sharing changed"
        );
        let (verb, who) = (self.noun.verb(), recipient.who());
        Ok(match permission {
            Some(GrantLevel::Run) => {
                format!("Shared `{id}` with {named} — {who} can {verb} it.")
            }
            Some(GrantLevel::Manage) => format!(
                "Shared `{id}` with {named} — {who} can {verb} and edit it (not re-share it)."
            ),
            None => format!("`{id}` is no longer shared with {named}."),
        })
    }

    /// Hand one definition to somebody else. The standing, the move and the
    /// audit are all [`transfer_definition`]'s; what this adds is the reload
    /// the other definition commands do and this surface's own log line.
    async fn transfer(&self, id: &str, owner: &str, who: &str) -> Result<String, String> {
        let done = transfer_definition(
            &self.writer(),
            &self.shares(),
            &TransferStores {
                defs: Arc::clone(&self.store),
                identities: self.identities.clone(),
                teams: self.teams.clone(),
            },
            DefTransfer {
                kind: self.noun.kind(),
                id,
                from: owner,
                to: who,
                // The grammar offers no choice, so chat transfers plainly:
                // `cmd: … share` afterwards is the owner's to make, not this
                // command's to assume.
                keep: OutgoingAccess::Nothing,
            },
        )
        .await
        .map_err(|denied| denied.to_string())?;
        self.surface
            .reload()
            .await
            .map_err(|e| format!("Transferred `{id}`, but reloading it failed: {e}"))?;
        tracing::info!(
            target: "sentinel.audit",
            kind = self.noun.kind(), definition = %id, owner = %done.to,
            from_owner = %done.from, actor = %self.uid,
            "definition owner changed"
        );
        Ok(format!(
            "`{id}` is {}'s now. {}",
            done.to_display,
            transfer_notes(id, &done, owner == self.uid).join(" ")
        ))
    }

    async fn set_visibility(
        &self,
        id: &str,
        owner: &str,
        visibility: Visibility,
    ) -> Result<String, String> {
        let changed = self
            .store
            .set_visibility(self.noun.kind(), id, owner, visibility)
            .await
            .map_err(|e| format!("Could not update visibility: {e}"))?;
        if !changed {
            // The row went while this line was being read; the loaded map that
            // placed it is a copy, the store is the fact.
            return Err(self.not_yours(id));
        }
        self.surface
            .reload()
            .await
            .map_err(|e| format!("Changed who reaches `{id}`, but reloading it failed: {e}"))?;
        tracing::info!(
            target: "sentinel.audit",
            kind = self.noun.kind(), definition = %id, owner = %owner, actor = %self.uid,
            visibility = %visibility.as_str(),
            "definition visibility changed"
        );
        let verb = self.noun.verb();
        Ok(match visibility {
            Visibility::Global => format!("`{id}` is visible to everyone on this install."),
            Visibility::Team => {
                let reach = format!(
                    "`{id}` is visible to your teams — anyone in a team with you can {verb} it."
                );
                match owner == self.uid && self.reach.team_ids().is_empty() {
                    true => format!("{reach} {NO_TEAM_YET}"),
                    false => reach,
                }
            }
            Visibility::Private => format!(
                "`{id}` is private — only you and the people you shared it with can {verb} it."
            ),
        })
    }
}

/// `Alice's`, `Alice's and Bob's`, `Alice's, Bob's and Carol's`.
fn possessives(names: &[String]) -> String {
    let owned: Vec<String> = names.iter().map(|name| format!("{name}'s")).collect();
    match owned.split_last() {
        Some((last, [])) => last.clone(),
        Some((last, rest)) => format!("{} and {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// The grantee set after adding (`Some`) or removing (`None`) one person. The
/// stored set is authoritative, so this is the whole edit; it keys on the pair
/// the store keys on, so a team grant of the same id is left alone.
fn apply_share(
    existing: &[Grant],
    recipient: &Recipient,
    permission: Option<GrantLevel>,
) -> Vec<Grant> {
    existing
        .iter()
        .filter(|held| {
            (held.principal_type, held.principal_id.as_str())
                != (recipient.principal(), recipient.id())
        })
        .cloned()
        .chain(permission.map(|permission| recipient.grant(permission)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::playbook_runtime::PlaybookRuntime;

    fn line(noun: DefNoun, args: &str) -> Result<Line<'_>, String> {
        parse_line(noun, args)
    }

    #[test]
    fn one_grammar_reads_the_same_for_every_kind() {
        for noun in [DefNoun::Workflow, DefNoun::Playbook, DefNoun::Monitor] {
            assert_eq!(
                line(noun, "shares triage"),
                Ok(Line {
                    id: "triage",
                    owner: None,
                    request: Request::Shares
                })
            );
            assert_eq!(
                line(noun, "share triage bob@example.com"),
                Ok(Line {
                    id: "triage",
                    owner: None,
                    request: Request::Share {
                        who: "bob@example.com",
                        permission: GrantLevel::Run
                    }
                })
            );
            assert_eq!(
                line(noun, "share triage <@U1> run"),
                Ok(Line {
                    id: "triage",
                    owner: None,
                    request: Request::Share {
                        who: "<@U1>",
                        permission: GrantLevel::Run
                    }
                })
            );
            assert_eq!(
                line(noun, "share triage <@U1> manage"),
                Ok(Line {
                    id: "triage",
                    owner: None,
                    request: Request::Share {
                        who: "<@U1>",
                        permission: GrantLevel::Manage
                    }
                })
            );
            assert_eq!(
                line(noun, "unshare triage bob@example.com"),
                Ok(Line {
                    id: "triage",
                    owner: None,
                    request: Request::Unshare {
                        who: "bob@example.com"
                    }
                })
            );
            assert_eq!(
                line(noun, "visibility triage everyone"),
                Ok(Line {
                    id: "triage",
                    owner: None,
                    request: Request::Visibility(Visibility::Global)
                })
            );
            assert_eq!(
                line(noun, "visibility triage PRIVATE"),
                Ok(Line {
                    id: "triage",
                    owner: None,
                    request: Request::Visibility(Visibility::Private)
                })
            );
            assert_eq!(
                line(noun, "transfer triage bob@example.com"),
                Ok(Line {
                    id: "triage",
                    owner: None,
                    request: Request::Transfer {
                        who: "bob@example.com"
                    }
                })
            );
            // The qualifier names the row being transferred *from*, exactly
            // as it names the row being shared.
            assert_eq!(
                line(noun, "transfer triage <@U1> owner:@alice"),
                Ok(Line {
                    id: "triage",
                    owner: Some("@alice"),
                    request: Request::Transfer { who: "<@U1>" }
                })
            );
        }
    }

    #[test]
    fn the_owner_qualifier_is_read_wherever_it_is_typed() {
        assert_eq!(
            line(DefNoun::Workflow, "shares triage owner:@alice"),
            Ok(Line {
                id: "triage",
                owner: Some("@alice"),
                request: Request::Shares
            })
        );
        assert_eq!(
            line(
                DefNoun::Monitor,
                "share owner:alice@example.com triage <@U1> manage"
            ),
            Ok(Line {
                id: "triage",
                owner: Some("alice@example.com"),
                request: Request::Share {
                    who: "<@U1>",
                    permission: GrantLevel::Manage
                }
            })
        );
        // An empty or repeated qualifier says nothing, so it is a usage slip.
        assert_eq!(
            line(DefNoun::Workflow, "shares triage owner:"),
            Err(usage(DefNoun::Workflow))
        );
        assert_eq!(
            line(DefNoun::Workflow, "shares triage owner:a owner:b"),
            Err(usage(DefNoun::Workflow))
        );
    }

    #[test]
    fn a_line_is_parsed_before_anything_is_looked_up() {
        let usage = usage(DefNoun::Playbook);
        for args in [
            "",
            "dance",
            "shares",
            "shares triage extra",
            "share triage",
            "share triage bob owner",
            "unshare triage bob manage",
            "visibility triage",
            "visibility triage private extra",
            "transfer triage",
            "transfer triage bob extra",
        ] {
            assert_eq!(line(DefNoun::Playbook, args), Err(usage.clone()), "{args}");
        }
        assert_eq!(
            line(DefNoun::Playbook, "visibility triage public"),
            Err("`public` is not a visibility — use `private`, `team` or `everyone`.".to_string())
        );
        assert_eq!(
            line(DefNoun::Playbook, "visibility triage TEAM"),
            Ok(Line {
                id: "triage",
                owner: None,
                request: Request::Visibility(Visibility::Team)
            })
        );
    }

    #[test]
    fn usage_names_its_own_noun_and_only_workflows_run() {
        let workflow = usage(DefNoun::Workflow);
        assert!(
            workflow.contains("`cmd: workflow run <workflow-id> [key=value …]`"),
            "{workflow}"
        );
        assert!(workflow.contains("`cmd: workflow visibility <workflow-id> private|team|everyone`"));
        assert!(
            workflow.contains("<@user|email|id|team:<name>> [manage]"),
            "{workflow}"
        );
        assert!(
            workflow.contains("`cmd: workflow transfer <workflow-id> <@user|email|id>`"),
            "{workflow}"
        );
        assert!(usage(DefNoun::Playbook).contains("`cmd: playbook start <playbook-id>`"));
        // Each kind is reached its own way: a workflow runs, a playbook is
        // started, and a monitor is only ever looked at.
        for (noun, verb) in [
            (DefNoun::Workflow, "run"),
            (DefNoun::Playbook, "start"),
            (DefNoun::Monitor, "see"),
        ] {
            let text = usage(noun);
            assert!(text.contains(&format!("let someone {verb} it")), "{text}");
            assert!(text.contains(&format!("who else may {verb} it")), "{text}");
        }
        for noun in [DefNoun::Playbook, DefNoun::Monitor] {
            let text = usage(noun);
            assert!(!text.contains("run <"), "{text}");
            assert!(text.contains(&format!(
                "`cmd: {} shares <{}-id>`",
                noun.word(),
                noun.word()
            )));
            assert!(text.ends_with(&format!(
                "Add `owner:<@user|email|id>` to address a {} of theirs you may manage.",
                noun.word()
            )));
        }
    }

    #[test]
    fn a_grantee_set_changes_one_principal_at_a_time() {
        let bob = Recipient::User {
            id: "bob".into(),
            display: "Bob".into(),
        };
        let existing = vec![
            Grant::user("bob", GrantLevel::Run),
            Grant::team("bob", GrantLevel::Run),
        ];
        let promoted = apply_share(&existing, &bob, Some(GrantLevel::Manage));
        assert_eq!(promoted.len(), 2, "the team of the same id is untouched");
        assert_eq!(
            promoted
                .iter()
                .find(|g| g.principal_type.is_user())
                .unwrap()
                .permission,
            GrantLevel::Manage
        );
        let removed = apply_share(&existing, &bob, None);
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].principal_type, PrincipalKind::Team);
        let carol = Recipient::User {
            id: "carol".into(),
            display: "Carol".into(),
        };
        assert_eq!(apply_share(&existing, &carol, None), existing);
    }

    #[test]
    fn an_ambiguous_id_names_every_owner_it_could_mean() {
        assert_eq!(possessives(&["Alice".to_string()]), "Alice's");
        assert_eq!(
            possessives(&["Alice".to_string(), "Bob".to_string()]),
            "Alice's and Bob's"
        );
        assert_eq!(
            possessives(&["Alice".to_string(), "Bob".to_string(), "Carol".to_string()]),
            "Alice's, Bob's and Carol's"
        );
        assert_eq!(possessives(&[]), "");
    }

    // ── Execution, against a real definition store ───────────────────────

    const TRIAGE: &str = "[playbook]\nid=\"triage\"\nname=\"Triage\"\ndescription=\"d\"\n\
                          [[step]]\nid=\"a\"\ntitle=\"A\"\nprompt=\"do\"\n";

    struct People {
        store: Arc<workforce_identity::sqlite::SqliteIdentityStore>,
        alice: String,
        bob: String,
        carol: String,
    }

    fn people() -> People {
        use workforce_identity::types::{Identity, IdentityRole};
        let store = Arc::new(workforce_identity::sqlite::SqliteIdentityStore::in_memory().unwrap());
        let made = |name: &str, email: &str| {
            let mut who = Identity::new(name, IdentityRole::Member);
            who.email = Some(email.to_string());
            store.create(&who).unwrap();
            who.id.to_string()
        };
        let alice = made("Alice", "alice@example.com");
        let bob = made("Bob", "bob@example.com");
        let carol = made("Carol", "carol@example.com");
        People {
            store: Arc::clone(&store),
            alice,
            bob,
            carol,
        }
    }

    /// A playbook runtime backed by `store`, with `triage` owned by each of
    /// `owners`, loaded.
    async fn runtime_with(
        store: &Arc<WorkflowStore>,
        owners: &[&str],
    ) -> (Arc<PlaybookRuntime>, std::path::PathBuf) {
        use workforce_orchestrator::workflow_store::{EditContext, RunAsWrite};
        let dir = std::env::temp_dir().join(format!("defcmd-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let runtime = Arc::new(PlaybookRuntime::new(dir.clone()));
        runtime.set_store(Arc::clone(store));
        for owner in owners {
            store
                .save(
                    "triage",
                    KIND_PLAYBOOK,
                    TRIAGE,
                    None,
                    Some(owner),
                    Visibility::Private,
                    RunAsWrite::Unchanged,
                    EditContext::new("test", Some(owner)),
                )
                .await
                .unwrap();
        }
        runtime.reload().await.unwrap();
        (runtime, dir)
    }

    fn chat<'a>(
        runtime: &'a Arc<PlaybookRuntime>,
        store: &Arc<WorkflowStore>,
        people: &People,
        uid: &str,
    ) -> DefChat<'a> {
        DefChat {
            noun: DefNoun::Playbook,
            surface: DefSurface::Playbook(runtime.as_ref()),
            store: Arc::clone(store),
            identities: Some(Arc::clone(&people.store) as Arc<dyn IdentityStore>),
            teams: None,
            reach: TeamReach::new(None, Some(uid.to_string())),
            uid: uid.to_string(),
            is_admin: false,
        }
    }

    async fn say(chat: &DefChat<'_>, args: &str) -> String {
        let line = parse_line(chat.noun, args).expect("parses");
        chat.apply(&line).await.unwrap_or_else(|refusal| refusal)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_owner_shares_lists_and_re_scopes_their_own_definition() {
        workforce_postgres::test_support::with_test_pool(
            WorkflowStore::build_pool,
            |pool| async move {
                let store = WorkflowStore::new(pool).await.expect("store");
                let people = people();
                let (runtime, dir) = runtime_with(&store, &[&people.alice]).await;
                let alice = chat(&runtime, &store, &people, &people.alice);

                assert_eq!(
                    say(&alice, "shares triage").await,
                    "`triage` isn't shared with anyone. Share it with \
                     `cmd: playbook share triage <user>`."
                );
                assert_eq!(
                    say(&alice, "share triage bob@example.com").await,
                    "Shared `triage` with Bob — they can start it."
                );
                assert_eq!(
                    say(&alice, "shares triage").await,
                    "`triage` is shared with: Bob (run)."
                );
                assert_eq!(
                    say(&alice, "share triage bob@example.com manage").await,
                    "Shared `triage` with Bob — they can start and edit it (not re-share it)."
                );
                assert_eq!(
                    say(&alice, "shares triage").await,
                    "`triage` is shared with: Bob (manage)."
                );
                assert_eq!(
                    say(&alice, "unshare triage bob@example.com").await,
                    "`triage` is no longer shared with Bob."
                );
                assert_eq!(
                    say(&alice, "unshare triage bob@example.com").await,
                    "`triage` wasn't shared with Bob."
                );
                assert_eq!(
                    say(&alice, "share triage alice@example.com").await,
                    "`triage` is already yours — there's nothing to share."
                );

                assert_eq!(
                    say(&alice, "visibility triage everyone").await,
                    "`triage` is visible to everyone on this install."
                );
                assert_eq!(
                    runtime
                        .list_definitions_visible(&crate::resource_access::DefListing::local(
                            Some(&people.carol),
                            false,
                        ))
                        .await
                        .len(),
                    1,
                    "the reload makes the new reach take effect here"
                );
                assert_eq!(
                    say(&alice, "visibility triage private").await,
                    "`triage` is private — only you and the people you shared it with can start it."
                );
                assert!(runtime
                    .list_definitions_visible(&crate::resource_access::DefListing::local(
                        Some(&people.carol),
                        false,
                    ))
                    .await
                    .is_empty());

                let _ = std::fs::remove_dir_all(&dir);
            },
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_manage_grantee_edits_but_never_re_shares() {
        workforce_postgres::test_support::with_test_pool(
            WorkflowStore::build_pool,
            |pool| async move {
                let store = WorkflowStore::new(pool).await.expect("store");
                let people = people();
                let (runtime, dir) = runtime_with(&store, &[&people.alice]).await;
                store
                    .set_shares(
                        KIND_PLAYBOOK,
                        "triage",
                        &people.alice,
                        &[Grant::user(people.bob.clone(), GrantLevel::Manage)],
                        &people.alice,
                    )
                    .await
                    .unwrap();

                // Bob reaches Alice's row without naming her — and is still
                // refused, in the words the rule already uses.
                let bob = chat(&runtime, &store, &people, &people.bob);
                let refused = format!(
                    "'triage' is owned by {}; only its owner, or someone they granted manage \
                     access, may change it",
                    people.alice
                );
                assert_eq!(say(&bob, "shares triage").await, refused);
                assert_eq!(say(&bob, "share triage carol@example.com").await, refused);
                assert_eq!(say(&bob, "visibility triage everyone").await, refused);

                // Carol holds nothing, so there is no row for her to land on.
                let carol = chat(&runtime, &store, &people, &people.carol);
                assert_eq!(
                    say(&carol, "shares triage").await,
                    "No playbook called `triage` that you can manage."
                );
                let named = format!("shares triage owner:{}", people.alice);
                assert_eq!(say(&carol, &named).await, refused);
                assert_eq!(
                    say(&carol, "shares triage owner:nobody@example.com").await,
                    "I can't tell who `nobody@example.com` is. Name them with an @-mention, \
                     their email address, or their Workforce user id, or a team with \
                     `team:<name>`."
                );

                let _ = std::fs::remove_dir_all(&dir);
            },
        )
        .await;
    }

    /// The install-wide row belongs to nobody, so it has no owner to share it
    /// on anyone's behalf — the same answer the gateway's share routes give it.
    /// It must not read as "nothing of that name", which is a different fact.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_install_wide_definition_says_it_belongs_to_nobody() {
        workforce_postgres::test_support::with_test_pool(
            WorkflowStore::build_pool,
            |pool| async move {
                use workforce_orchestrator::workflow_store::{EditContext, RunAsWrite};
                let store = WorkflowStore::new(pool).await.expect("store");
                let people = people();
                let (runtime, dir) = runtime_with(&store, &[]).await;
                store
                    .save(
                        "triage",
                        KIND_PLAYBOOK,
                        TRIAGE,
                        None,
                        None,
                        Visibility::Global,
                        RunAsWrite::Unchanged,
                        EditContext::new("test", None),
                    )
                    .await
                    .unwrap();
                runtime.reload().await.unwrap();

                let carol = chat(&runtime, &store, &people, &people.carol);
                assert_eq!(
                    say(&carol, "shares triage").await,
                    "`triage` belongs to the whole install rather than to a member, so there \
                     is nobody to share it with."
                );
                assert_eq!(
                    say(&carol, "visibility triage everyone").await,
                    "`triage` belongs to the whole install rather than to a member, so there \
                     is nobody to share it with."
                );
                // An id nothing answers to still reads the other way round.
                assert_eq!(
                    say(&carol, "shares missing").await,
                    "No playbook called `missing` that you can manage."
                );

                let _ = std::fs::remove_dir_all(&dir);
            },
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_bare_id_two_people_own_asks_which_one() {
        workforce_postgres::test_support::with_test_pool(
            WorkflowStore::build_pool,
            |pool| async move {
                let store = WorkflowStore::new(pool).await.expect("store");
                let people = people();
                let (runtime, dir) = runtime_with(&store, &[&people.alice, &people.bob]).await;
                for owner in [&people.alice, &people.bob] {
                    store
                        .set_shares(
                            KIND_PLAYBOOK,
                            "triage",
                            owner,
                            &[Grant::user(people.carol.clone(), GrantLevel::Manage)],
                            owner,
                        )
                        .await
                        .unwrap();
                }
                let carol = chat(&runtime, &store, &people, &people.carol);
                assert_eq!(
                    say(&carol, "shares triage").await,
                    "More than one playbook is called `triage`: Alice's and Bob's. Name one \
                     with `owner:<@user|email|id>`."
                );
                // Naming one lands on it — and the rule then has its say.
                assert!(say(&carol, "shares triage owner:alice@example.com")
                    .await
                    .contains("only its owner"));

                let _ = std::fs::remove_dir_all(&dir);
            },
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_team_grant_is_listed_by_its_name_or_its_bare_id() {
        workforce_postgres::test_support::with_test_pool(
            WorkflowStore::build_pool,
            |pool| async move {
                use workforce_identity::team::Team;
                let store = WorkflowStore::new(pool).await.expect("store");
                let people = people();
                let (runtime, dir) = runtime_with(&store, &[&people.alice, &people.bob]).await;
                let team = Team::new("platform", uuid::Uuid::parse_str(&people.alice).unwrap());
                people.store.create_team(&team).unwrap();
                let stray = uuid::Uuid::new_v4().to_string();
                store
                    .set_shares(
                        KIND_PLAYBOOK,
                        "triage",
                        &people.alice,
                        &[
                            Grant::team(team.id.to_string(), GrantLevel::Run),
                            Grant::team(stray.clone(), GrantLevel::Manage),
                        ],
                        &people.alice,
                    )
                    .await
                    .unwrap();
                let teams = Some(Arc::clone(&people.store) as Arc<dyn TeamStore>);
                let mut alice = chat(&runtime, &store, &people, &people.alice);
                alice.teams = teams.clone();
                alice.reach = TeamReach::new(teams.clone(), Some(people.alice.clone()));

                let listed = say(&alice, "shares triage").await;
                assert!(listed.contains("platform team (run)"), "{listed}");
                assert!(
                    listed.contains(&format!("{stray} team (manage)")),
                    "a team since deleted still lists: {listed}"
                );

                // A team is named, granted and revoked from chat like a person.
                assert_eq!(
                    say(&alice, "share triage team:PLATFORM manage").await,
                    "Shared `triage` with the platform team — its members can start and edit \
                     it (not re-share it)."
                );
                assert_eq!(
                    store
                        .caller_share_permission(
                            KIND_PLAYBOOK,
                            "triage",
                            &people.alice,
                            None,
                            &[team.id.to_string()],
                        )
                        .await
                        .unwrap(),
                    Some(GrantLevel::Manage)
                );
                assert_eq!(
                    say(&alice, "unshare triage team:platform").await,
                    "`triage` is no longer shared with the platform team."
                );
                assert_eq!(
                    say(&alice, "unshare triage team:platform").await,
                    "`triage` wasn't shared with the platform team."
                );
                assert_eq!(
                    say(&alice, "share triage team:ops").await,
                    "I can't tell who `team:ops` is. Name them with an @-mention, their email \
                     address, or their Workforce user id, or a team with `team:<name>`."
                );

                // Alice created the team, so `team` reaches her teammates and the
                // reply says only that; Bob stands in none, so his own says so.
                assert_eq!(
                    say(&alice, "visibility triage team").await,
                    "`triage` is visible to your teams — anyone in a team with you can start it."
                );
                let mut bob = chat(&runtime, &store, &people, &people.bob);
                bob.teams = teams.clone();
                bob.reach = TeamReach::new(teams, Some(people.bob.clone()));
                assert_eq!(
                    say(&bob, "visibility triage team").await,
                    "`triage` is visible to your teams — anyone in a team with you can start \
                     it. You're not in a team yet, so nobody else can run it until you join one."
                );

                let _ = std::fs::remove_dir_all(&dir);
            },
        )
        .await;
    }

    /// An administrator reaches a member's definition by naming its owner, so
    /// an install can still be administered when its owner is away — and every
    /// change they make there is recorded against them, while reading who it
    /// reaches is not. A bare id still lands only on rows they hold, so an
    /// override is always something they asked for by name.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_administrator_reaches_a_members_definition_by_naming_its_owner() {
        workforce_postgres::test_support::with_test_pool(
            WorkflowStore::build_pool,
            |pool| async move {
                use workforce_memory::access_audit::{self, AuditOperation};

                let buffer = access_audit::install_ring_buffer(256);
                let _ = access_audit::set_global_sink(buffer.clone());
                let store = WorkflowStore::new(pool).await.expect("store");
                let people = people();
                let (runtime, dir) = runtime_with(&store, &[&people.alice]).await;
                let mut root = chat(&runtime, &store, &people, &people.carol);
                root.is_admin = true;
                let owner = format!("owner:{}", people.alice);
                let overrides = || {
                    buffer
                        .snapshot()
                        .into_iter()
                        .filter(|event| {
                            event.requester_identity_id.as_deref() == Some(people.carol.as_str())
                        })
                        .collect::<Vec<_>>()
                };

                assert_eq!(
                    say(&root, &format!("shares triage {owner}")).await,
                    "`triage` isn't shared with anyone. Share it with \
                     `cmd: playbook share triage <user>`."
                );
                assert!(overrides().is_empty(), "a read is not an override");

                assert_eq!(
                    say(&root, &format!("share triage bob@example.com {owner}")).await,
                    "Shared `triage` with Bob — they can start it."
                );
                assert_eq!(
                    say(&root, &format!("visibility triage everyone {owner}")).await,
                    "`triage` is visible to everyone on this install."
                );
                let recorded = overrides();
                assert_eq!(recorded.len(), 2, "one per change, and only per change");
                for event in &recorded {
                    assert_eq!(event.operation, AuditOperation::ResourceAccessOverride);
                    assert_eq!(
                        event.owner_identity_id.as_deref(),
                        Some(people.alice.as_str())
                    );
                    assert_eq!(event.resource_id.as_deref(), Some("playbook:triage"));
                }

                // A bare id still lands only on a row of their own.
                assert_eq!(
                    say(&root, "shares triage").await,
                    "No playbook called `triage` that you can manage."
                );

                let _ = std::fs::remove_dir_all(&dir);
            },
        )
        .await;
    }

    #[test]
    fn only_member_rows_with_that_id_are_candidates() {
        let rows = || {
            vec![
                ("triage".to_string(), Some("alice".to_string())),
                ("triage".to_string(), None),
                ("other".to_string(), Some("bob".to_string())),
            ]
            .into_iter()
        };
        assert_eq!(member_owners(rows(), "triage"), vec!["alice".to_string()]);
        assert!(member_owners(rows(), "missing").is_empty());
    }
}
