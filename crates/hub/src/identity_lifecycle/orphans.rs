//! What identities that no longer exist left behind: found by diffing every
//! category's [`Holding::owners`] against the identity store, and handed to
//! a live identity or erased through the same engine as a merge or delete.
//!
//! A name is taken for a missing person only with proof: a lifecycle
//! operation recorded it as the name of an identity that is now gone, or it
//! is an identity id, written as identity ids are, that no live identity
//! has and that some field only identity ids are written to holds. A name a
//! live identity answers to in any case or id form, or that went with an
//! identity merged into one still live, is that live identity's. Rooms,
//! teams, system and agent owners, edge customers, devices, unlinked
//! platform ids and identity-shaped ids without that proof are counted and
//! never acted on.

use std::collections::{BTreeMap, BTreeSet};

use serde::Serialize;
use uuid::Uuid;

use super::runs::{Operation, Steps};
use super::{
    fold, holding, Actor, Category, CategoryReport, Context, DeletePlan, Lifecycle, LifecycleError,
    OrphanAction, OrphanRule, Owners, Person, Report, Request, Subject, Tally,
};

/// Lifecycle operations read for the names missing identities went by.
const OPS_READ: usize = 100_000;

/// A missing identity and what it left.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Orphan {
    pub id: Uuid,
    pub proof: Proof,
    /// The other names it went by that no live identity has.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub names: Vec<String>,
    pub held: BTreeMap<Category, usize>,
}

/// How a name is known to have been a person's.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Proof {
    /// A lifecycle operation recorded it.
    Recorded,
    /// A field only identity ids are written to holds it.
    IdentityField,
}

/// A name things are held under that is never acted on, and what it holds.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Named {
    pub name: String,
    /// The missing identities it could have belonged to.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub could_be: Vec<Uuid>,
    pub held: BTreeMap<Category, usize>,
}

/// What an owner that is not a missing person is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OwnerKind {
    /// A name of an identity in the store, or of one merged into it.
    Live,
    /// A team's id.
    Team,
    /// A paired device, revoked ones included.
    Device,
    /// A room many people share (`shared:`).
    Shared,
    /// The hub itself: `system`, the scheduler, the owner token.
    System,
    /// An agent (`agent:`).
    Agent,
    /// An edge customer, who has no hub identity (`ext:`).
    External,
    /// A platform id or legacy name no identity is known to have gone by.
    Other,
}

/// The namespaces owners that are not people are written under, as a bare
/// word or a `word:` prefix.
const NAMESPACES: [(&str, OwnerKind); 6] = [
    ("shared", OwnerKind::Shared),
    ("system", OwnerKind::System),
    ("scheduler", OwnerKind::System),
    ("rest", OwnerKind::System),
    ("agent", OwnerKind::Agent),
    ("ext", OwnerKind::External),
];

impl OwnerKind {
    fn of(name: &str) -> Self {
        NAMESPACES
            .iter()
            .find(|(word, _)| {
                name.strip_prefix(word)
                    .is_some_and(|rest| rest.is_empty() || rest.starts_with(':'))
            })
            .map_or(Self::Other, |(_, kind)| *kind)
    }
}

/// Everything identities that no longer exist left, as counts per category.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct OrphanScan {
    /// Every category answered and every operation was read; otherwise some
    /// of what is left may be missing from the lists.
    pub complete: bool,
    pub orphans: Vec<Orphan>,
    /// Identity-shaped ids no live identity has and nothing proves were
    /// identities.
    pub unproven: Vec<Named>,
    /// Names more than one missing identity went by.
    pub ambiguous: Vec<Named>,
    /// How many distinct owners of each other kind hold anything.
    pub not_orphaned: BTreeMap<OwnerKind, usize>,
    /// The categories that could not answer, with why.
    #[serde(skip_serializing_if = "BTreeMap::is_empty")]
    pub failed: BTreeMap<Category, String>,
}

/// What a name is, for the clean-up.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Class {
    Orphan(Uuid),
    Ambiguous(Vec<Uuid>),
    Unproven,
    Not(OwnerKind),
}

/// Who is who: the live identities' names, the devices and teams, and the
/// names missing identities went by.
#[derive(Debug, Default)]
pub(super) struct Directory {
    /// Case folded, as [`super::fold`] compares them.
    live: BTreeSet<String>,
    devices: BTreeSet<String>,
    teams: BTreeSet<Uuid>,
    /// Each name recorded for a missing identity, with every missing
    /// identity it was recorded for.
    dead: BTreeMap<String, BTreeSet<Uuid>>,
    proven: BTreeMap<Uuid, Proof>,
    /// Every lifecycle operation was read, so a name recorded for one
    /// missing identity was recorded for no other.
    pub(super) history_read: bool,
}

/// Who is who, as read from the stores.
#[derive(Debug, Default)]
pub(super) struct Known {
    /// Case folded.
    pub(super) live: BTreeSet<String>,
    pub(super) devices: BTreeSet<String>,
    pub(super) teams: BTreeSet<Uuid>,
    pub(super) ops: Vec<workforce_identity::lifecycle::LifecycleOp>,
    pub(super) history_read: bool,
}

/// `name` as an identity id, when it is one in the form identity ids are
/// written in.
fn identity_id(name: &str) -> Option<Uuid> {
    Uuid::parse_str(name)
        .ok()
        .filter(|id| id.to_string() == name)
}

impl Directory {
    /// A merge moved its subject's links to its target, so whatever was
    /// later held under one of those names while the target is live is the
    /// target's; only the merged-away id itself is gone.
    pub(super) fn new(known: Known, identity_fields: &BTreeSet<String>) -> Self {
        let Known {
            mut live,
            devices,
            teams,
            ops,
            history_read,
        } = known;
        let is_live = |id: &Uuid| live.contains(&id.to_string());
        let gone: Vec<_> = ops.iter().filter(|op| !is_live(&op.subject)).collect();
        let merged: Vec<String> = gone
            .iter()
            .filter(|op| op.kind == "merge" && op.target.as_ref().is_some_and(is_live))
            .flat_map(|op| {
                let id = op.subject.to_string();
                op.names
                    .iter()
                    .filter(move |name| **name != id)
                    .map(|name| fold(name))
            })
            .collect();
        live.extend(merged);
        let mut dead: BTreeMap<String, BTreeSet<Uuid>> = BTreeMap::new();
        let mut proven = BTreeMap::new();
        for op in gone {
            proven.insert(op.subject, Proof::Recorded);
            for name in std::iter::once(op.subject.to_string()).chain(op.names.iter().cloned()) {
                if !live.contains(&fold(&name)) {
                    dead.entry(name).or_default().insert(op.subject);
                }
            }
        }
        for id in identity_fields
            .iter()
            .filter(|name| !live.contains(&fold(name)) && !devices.contains(&fold(name)))
            .filter_map(|name| identity_id(name))
            .filter(|id| !teams.contains(id))
        {
            proven.entry(id).or_insert(Proof::IdentityField);
        }
        Self {
            live,
            devices,
            teams,
            dead,
            proven,
            history_read,
        }
    }

    pub(super) fn classify(&self, name: &str) -> Class {
        let parsed = Uuid::parse_str(name).ok();
        if self.live.contains(&fold(name))
            || parsed.is_some_and(|id| self.live.contains(&id.to_string()))
        {
            return Class::Not(OwnerKind::Live);
        }
        if parsed.is_some_and(|id| self.teams.contains(&id)) {
            return Class::Not(OwnerKind::Team);
        }
        if let Some(subjects) = self.dead.get(name) {
            return match subjects.iter().collect::<Vec<_>>().as_slice() {
                [only] => Class::Orphan(**only),
                many => Class::Ambiguous(many.iter().map(|id| **id).collect()),
            };
        }
        if self.devices.contains(&fold(name)) {
            return Class::Not(OwnerKind::Device);
        }
        match (parsed, identity_id(name)) {
            (_, Some(id)) if self.proven.contains_key(&id) => Class::Orphan(id),
            (Some(_), _) => Class::Unproven,
            (None, _) => Class::Not(OwnerKind::of(name)),
        }
    }

    /// The missing identity `id` with every name only it went by.
    fn person(&self, id: Uuid) -> Person {
        let names: Vec<String> = self
            .dead
            .iter()
            .filter(|(_, subjects)| subjects.len() == 1 && subjects.contains(&id))
            .map(|(name, _)| name.clone())
            .collect();
        Person::orphan(id, &names)
    }
}

/// The scan as it is assembled, category by category.
#[derive(Default)]
struct Found {
    orphans: BTreeMap<Uuid, BTreeMap<Category, usize>>,
    unproven: BTreeMap<String, BTreeMap<Category, usize>>,
    ambiguous: BTreeMap<String, (Vec<Uuid>, BTreeMap<Category, usize>)>,
    not_orphaned: BTreeMap<OwnerKind, BTreeSet<String>>,
}

impl Found {
    fn add(&mut self, directory: &Directory, category: Category, name: String, count: usize) {
        if count == 0 {
            return;
        }
        let tally = |held: &mut BTreeMap<Category, usize>| {
            *held.entry(category).or_default() += count;
        };
        match directory.classify(&name) {
            Class::Orphan(id) => tally(self.orphans.entry(id).or_default()),
            Class::Unproven => tally(self.unproven.entry(name).or_default()),
            Class::Ambiguous(could_be) => tally(
                &mut self
                    .ambiguous
                    .entry(name)
                    .or_insert((could_be, BTreeMap::new()))
                    .1,
            ),
            Class::Not(kind) => {
                self.not_orphaned.entry(kind).or_default().insert(name);
            }
        }
    }

    fn scan(self, directory: &Directory, failed: BTreeMap<Category, String>) -> OrphanScan {
        let named = |(name, held): (String, BTreeMap<Category, usize>)| Named {
            name,
            could_be: Vec::new(),
            held,
        };
        OrphanScan {
            complete: failed.is_empty() && directory.history_read,
            orphans: self
                .orphans
                .into_iter()
                .map(|(id, held)| Orphan {
                    id,
                    proof: directory.proven[&id],
                    names: directory
                        .person(id)
                        .names
                        .into_iter()
                        .filter(|name| *name != id.to_string())
                        .collect(),
                    held,
                })
                .collect(),
            unproven: self.unproven.into_iter().map(named).collect(),
            ambiguous: self
                .ambiguous
                .into_iter()
                .map(|(name, (could_be, held))| Named {
                    name,
                    could_be,
                    held,
                })
                .collect(),
            not_orphaned: self
                .not_orphaned
                .into_iter()
                .map(|(kind, names)| (kind, names.len()))
                .collect(),
            failed,
        }
    }
}

impl Lifecycle {
    /// Everything identities that no longer exist left behind, per missing
    /// identity and category. Reads only, and fails outright rather than
    /// classify anything against an identity store it could not read whole.
    pub async fn orphans(&self, actor: &Actor) -> Result<OrphanScan, LifecycleError> {
        Ok(self.survey(actor).await?.0)
    }

    /// The scan, and the directory it classified names by.
    async fn survey(&self, actor: &Actor) -> Result<(OrphanScan, Directory), LifecycleError> {
        let cx = self.context(actor);
        let mut answers: Vec<(Category, eyre::Result<Owners>)> = Vec::new();
        for category in Category::ALL {
            answers.push((category, holding(category).owners(&cx).await));
        }
        let identity_fields: BTreeSet<String> = answers
            .iter()
            .filter_map(|(_, answer)| answer.as_ref().ok())
            .flat_map(|owners| owners.identities.iter().cloned())
            .collect();
        let directory = Directory::new(self.known()?, &identity_fields);
        let candidates: Vec<Uuid> = directory.proven.keys().copied().collect();
        let mut found = Found::default();
        let mut failed = BTreeMap::new();
        for (category, answer) in answers {
            match answer {
                Ok(owners) if owners.probed => {
                    for id in &candidates {
                        match holding(category).held(&cx, &directory.person(*id)).await {
                            Ok(tally) => {
                                found.add(&directory, category, id.to_string(), tally.count)
                            }
                            Err(e) => {
                                failed.insert(category, format!("{e:#}"));
                                break;
                            }
                        }
                    }
                }
                Ok(owners) => {
                    for (name, count) in owners.held {
                        found.add(&directory, category, name, count);
                    }
                }
                Err(e) => {
                    failed.insert(category, format!("{e:#}"));
                }
            }
        }
        let scan = found.scan(&directory, failed);
        Ok((scan, directory))
    }

    /// Who is who, read now.
    fn known(&self) -> eyre::Result<Known> {
        let devices = match &self.devices {
            Some(devices) => devices
                .list_devices()?
                .into_iter()
                .flat_map(|device| std::iter::once(device.id).chain(device.client_id))
                .filter(|id| !id.is_empty())
                .map(|id| fold(&id))
                .collect(),
            None => BTreeSet::new(),
        };
        let ops = self.identities.list_ops(None, OPS_READ)?;
        Ok(Known {
            live: self.live_names()?,
            devices,
            teams: self.identities.list_teams()?.iter().map(|t| t.id).collect(),
            history_read: ops.len() < OPS_READ,
            ops,
        })
    }

    /// The missing identity `id`, as the scan finds it now.
    async fn orphan(&self, id: Uuid, actor: &Actor) -> Result<Person, LifecycleError> {
        if self.identities.get_by_id(id)?.is_some() {
            return Err(LifecycleError::Refused(format!(
                "{id} is an identity; merge or delete it instead"
            )));
        }
        let (scan, directory) = self.survey(actor).await?;
        if !directory.history_read {
            return Err(LifecycleError::Refused(format!(
                "More than {OPS_READ} lifecycle operations are recorded, so whether a name was \
                 only {id}'s cannot be told; nothing is acted on"
            )));
        }
        match scan.orphans.into_iter().find(|orphan| orphan.id == id) {
            Some(orphan) => Ok(Person::orphan(id, &orphan.names)),
            None if scan.unproven.iter().any(|u| u.name == id.to_string()) => {
                Err(LifecycleError::Refused(format!(
                    "Nothing proves {id} was an identity, so what is held under it is left alone"
                )))
            }
            None => Err(LifecycleError::NotFound(format!(
                "Nothing is left under a missing identity {id}"
            ))),
        }
    }

    /// Hand what the missing identity `id` left to `to`. Nobody can confirm
    /// `to` was that person, so what it said and what was learned about it
    /// stays unless `personal` asks for it to move as a merge moves it;
    /// whatever acts, grants or signs in goes as a delete naming `to`
    /// ([`OrphanAction::of`]): shared work is handed over as the
    /// administrator's edit, the rest erased, and nothing it went by is
    /// linked to `to`.
    pub async fn transfer_orphan(
        &self,
        id: Uuid,
        to: Uuid,
        personal: bool,
        actor: &Actor,
        dry_run: bool,
    ) -> Result<Report, LifecycleError> {
        self.perform(
            &Request::TransferOrphan { id, to, personal },
            actor,
            dry_run,
        )
        .await
    }

    /// Erase what the missing identity `id` left by `plan`, as a delete
    /// would: billing and audit records are kept.
    pub async fn delete_orphan(
        &self,
        id: Uuid,
        plan: &DeletePlan,
        actor: &Actor,
        dry_run: bool,
    ) -> Result<Report, LifecycleError> {
        let request = Request::DeleteOrphan {
            id,
            plan: plan.clone(),
        };
        self.perform(&request, actor, dry_run).await
    }

    pub(super) async fn prepare_transfer_orphan(
        &self,
        id: Uuid,
        to: Uuid,
        personal: bool,
        actor: &Actor,
    ) -> Result<Operation, LifecycleError> {
        let recipient = self.recipient(to, id)?;
        Ok(Operation {
            kind: "orphan_transfer",
            subject: Subject::Orphan,
            who: self.orphan(id, actor).await?,
            target: Some(to),
            steps: Box::new(HandingOver {
                recipient,
                personal,
            }),
            actor: actor.clone(),
        })
    }

    pub(super) async fn prepare_delete_orphan(
        &self,
        id: Uuid,
        plan: &DeletePlan,
        actor: &Actor,
    ) -> Result<Operation, LifecycleError> {
        let who = self.orphan(id, actor).await?;
        self.prepare_erase("orphan_delete", Subject::Orphan, who, plan, actor)
            .await
    }
}

/// What a hand-over does with each category ([`OrphanAction::of`]).
struct HandingOver {
    recipient: Person,
    personal: bool,
}

#[async_trait::async_trait]
impl Steps for HandingOver {
    async fn step(
        &self,
        cx: &Context<'_>,
        category: Category,
        who: &Person,
        apply: bool,
    ) -> eyre::Result<Tally> {
        let held = holding(category);
        match (
            category.rules().orphan,
            OrphanAction::of(category, self.personal),
        ) {
            (OrphanRule::Personal, OrphanAction::Move) => {
                held.merge(cx, who, &self.recipient, apply).await
            }
            (OrphanRule::Personal, _) => held.held(cx, who).await,
            (_, OrphanAction::HandOver) => held.delete(cx, who, Some(&self.recipient), apply).await,
            _ => held.delete(cx, who, None, apply).await,
        }
    }

    fn annotate(&self, report: &mut CategoryReport) {
        report.orphan = Some(OrphanAction::of(report.category, self.personal));
    }
}
