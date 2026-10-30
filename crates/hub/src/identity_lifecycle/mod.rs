//! What happens to a person's things when their identity is merged into
//! another, transferred item by item, or deleted.
//!
//! Every store that keys data by identity is a [`Category`] with a
//! [`Holding`]: what the person holds there and how a merge or a delete
//! moves or erases it. [`holding`] maps every category to its holding with an
//! exhaustive `match`, and the schema guard test fails when a table gains an
//! identity column no category claims, so a new store cannot be forgotten.
//! Every holding also answers who it holds things for, which the orphan
//! clean-up ([`orphans`]) diffs against the identity store.

mod access;
mod automation;
mod conversations;
#[cfg(test)]
mod orphan_tests;
mod orphans;
mod records;
mod runs;
#[cfg(test)]
mod runs_tests;
#[cfg(test)]
mod schema_guard;
#[cfg(test)]
mod suspension_tests;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};

use async_trait::async_trait;
use chrono::{Duration, Utc};
use eyre::Result;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use workforce_identity::lifecycle::{LifecycleOp, OpStatus};
use workforce_identity::store::{IdentityAndTeamStore, IdentityFilter};
use workforce_identity::types::{Identity, IdentityStatus};

pub use orphans::{Named, Orphan, OrphanScan, OwnerKind, Proof};
pub use runs::{Launched, Request, Runs};

use crate::hosts::RegistryView;
pub use crate::hub_core::roles::Actor;
use crate::serve::AgentService;

/// How long an operation holds its subject before another replica may take
/// over; renewed before every category and every [`runs::HEARTBEAT`] while
/// one runs.
const LEASE: Duration = Duration::minutes(30);

/// How many of a subject's latest operations are read for an unfinished one
/// to resume.
const RESUME_LOOKBACK: usize = 20;

/// Labels a category reports at most; its count is always the true count.
const MAX_ITEMS: usize = 50;

/// Identities read per page when the whole store is listed.
const IDENTITY_PAGE: usize = 500;

/// One kind of thing a person can hold, in the order an operation visits
/// them: what cuts the person off from new traffic comes first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Category {
    IdentityRecord,
    RemoteMcpTokens,
    Devices,
    ChannelAllowlist,
    Teams,
    RoleGrants,
    PermissionRules,
    ApproverSlots,
    BudgetOverrides,
    Approvals,
    ScheduledJobs,
    Definitions,
    WorkflowRuns,
    SubagentJobs,
    HeartbeatChecks,
    LearningPlans,
    Sessions,
    Memories,
    PersonalFiles,
    Vault,
    KnowHow,
    DecisionRecords,
    Documents,
    TrainingCorpora,
    Artifacts,
    Skills,
    GameTallies,
    Pictures,
    Signals,
    CostLedger,
    ConfigReferences,
    AuditRecords,
    RetentionBound,
}

impl Category {
    pub const ALL: [Category; 33] = [
        Self::IdentityRecord,
        Self::RemoteMcpTokens,
        Self::Devices,
        Self::ChannelAllowlist,
        Self::Teams,
        Self::RoleGrants,
        Self::PermissionRules,
        Self::ApproverSlots,
        Self::BudgetOverrides,
        Self::Approvals,
        Self::ScheduledJobs,
        Self::Definitions,
        Self::WorkflowRuns,
        Self::SubagentJobs,
        Self::HeartbeatChecks,
        Self::LearningPlans,
        Self::Sessions,
        Self::Memories,
        Self::PersonalFiles,
        Self::Vault,
        Self::KnowHow,
        Self::DecisionRecords,
        Self::Documents,
        Self::TrainingCorpora,
        Self::Artifacts,
        Self::Skills,
        Self::GameTallies,
        Self::Pictures,
        Self::Signals,
        Self::CostLedger,
        Self::ConfigReferences,
        Self::AuditRecords,
        Self::RetentionBound,
    ];

    /// What a merge and a delete do with this category, as the dry run and
    /// the design doc state them.
    pub const fn rules(self) -> Rules {
        use DeleteRule as D;
        use MergeRule as M;
        let (merge, delete) = match self {
            Self::IdentityRecord => (M::Move, D::Erase),
            Self::RemoteMcpTokens => (M::Revoke, D::Erase),
            Self::Devices => (M::Move, D::Erase),
            Self::ChannelAllowlist => (M::Keep, D::Erase),
            Self::Teams => (M::Move, D::Erase),
            Self::RoleGrants
            | Self::PermissionRules
            | Self::ApproverSlots
            | Self::BudgetOverrides => (M::Drop, D::Erase),
            Self::Approvals => (M::Move, D::Erase),
            Self::ScheduledJobs | Self::Definitions => (
                M::Move,
                D::Choice {
                    default: Choice::Erase,
                },
            ),
            Self::WorkflowRuns | Self::SubagentJobs => (M::Move, D::Erase),
            Self::HeartbeatChecks | Self::LearningPlans => (M::Move, D::Release),
            Self::Sessions => (M::Move, D::Erase),
            Self::Memories | Self::PersonalFiles | Self::GameTallies => (M::Union, D::Erase),
            Self::Vault | Self::Documents => (M::Move, D::Erase),
            Self::KnowHow | Self::DecisionRecords | Self::TrainingCorpora => (M::Move, D::Erase),
            Self::Artifacts => (
                M::Move,
                D::Choice {
                    default: Choice::Transfer,
                },
            ),
            Self::Skills => (M::Move, D::Transfer),
            Self::Signals => (M::Move, D::Erase),
            Self::Pictures => (M::Move, D::Erase),
            Self::CostLedger => (M::Move, D::Keep),
            Self::AuditRecords => (M::Keep, D::Keep),
            Self::ConfigReferences => (M::Report, D::Report),
            Self::RetentionBound => (M::Keep, D::Expire),
        };
        let orphan = match self {
            Self::Sessions
            | Self::Memories
            | Self::PersonalFiles
            | Self::KnowHow
            | Self::DecisionRecords
            | Self::Documents
            | Self::TrainingCorpora
            | Self::GameTallies
            | Self::Pictures
            | Self::Signals
            | Self::CostLedger => OrphanRule::Personal,
            Self::IdentityRecord
            | Self::RemoteMcpTokens
            | Self::Devices
            | Self::ChannelAllowlist
            | Self::Teams
            | Self::RoleGrants
            | Self::PermissionRules
            | Self::ApproverSlots
            | Self::BudgetOverrides
            | Self::Approvals
            | Self::ScheduledJobs
            | Self::Definitions
            | Self::WorkflowRuns
            | Self::SubagentJobs
            | Self::HeartbeatChecks
            | Self::LearningPlans
            | Self::Vault
            | Self::Artifacts
            | Self::Skills
            | Self::ConfigReferences
            | Self::AuditRecords
            | Self::RetentionBound => OrphanRule::HandOver,
        };
        let suspend = match self {
            Self::ScheduledJobs | Self::Definitions | Self::WorkflowRuns => SuspendRule::Hold,
            Self::Sessions | Self::SubagentJobs => SuspendRule::Stop,
            Self::RemoteMcpTokens
            | Self::Devices
            | Self::Approvals
            | Self::HeartbeatChecks
            | Self::LearningPlans => SuspendRule::Refuse,
            Self::IdentityRecord
            | Self::ChannelAllowlist
            | Self::Teams
            | Self::RoleGrants
            | Self::PermissionRules
            | Self::ApproverSlots
            | Self::BudgetOverrides
            | Self::Memories
            | Self::PersonalFiles
            | Self::Vault
            | Self::KnowHow
            | Self::DecisionRecords
            | Self::Documents
            | Self::TrainingCorpora
            | Self::Artifacts
            | Self::Skills
            | Self::GameTallies
            | Self::Pictures
            | Self::Signals
            | Self::CostLedger
            | Self::ConfigReferences
            | Self::AuditRecords
            | Self::RetentionBound => SuspendRule::Unaffected,
        };
        Rules {
            merge,
            delete,
            orphan,
            suspend,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::IdentityRecord => "identity_record",
            Self::RemoteMcpTokens => "remote_mcp_tokens",
            Self::Devices => "devices",
            Self::ChannelAllowlist => "channel_allowlist",
            Self::Teams => "teams",
            Self::RoleGrants => "role_grants",
            Self::PermissionRules => "permission_rules",
            Self::ApproverSlots => "approver_slots",
            Self::BudgetOverrides => "budget_overrides",
            Self::Approvals => "approvals",
            Self::ScheduledJobs => "scheduled_jobs",
            Self::Definitions => "definitions",
            Self::WorkflowRuns => "workflow_runs",
            Self::SubagentJobs => "subagent_jobs",
            Self::HeartbeatChecks => "heartbeat_checks",
            Self::LearningPlans => "learning_plans",
            Self::Sessions => "sessions",
            Self::Memories => "memories",
            Self::PersonalFiles => "personal_files",
            Self::Vault => "vault",
            Self::KnowHow => "know_how",
            Self::DecisionRecords => "decision_records",
            Self::Documents => "documents",
            Self::TrainingCorpora => "training_corpora",
            Self::Artifacts => "artifacts",
            Self::Skills => "skills",
            Self::GameTallies => "game_tallies",
            Self::Pictures => "pictures",
            Self::Signals => "signals",
            Self::CostLedger => "cost_ledger",
            Self::ConfigReferences => "config_references",
            Self::AuditRecords => "audit_records",
            Self::RetentionBound => "retention_bound",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Rules {
    pub merge: MergeRule,
    pub delete: DeleteRule,
    pub orphan: OrphanRule,
    pub suspend: SuspendRule,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MergeRule {
    /// Moves to the identity merged into; conflicts follow the category's rule.
    Move,
    /// Both sides are kept as one.
    Union,
    /// A credential: revoked, never handed to anyone else.
    Revoke,
    /// Authority, not a belonging: dropped and reported, never carried over.
    Drop,
    /// Stays where it is.
    Keep,
    /// Named for an administrator to change; nothing is rewritten.
    Report,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case", tag = "rule")]
pub enum DeleteRule {
    Erase,
    /// The thing stays and no longer acts as the person.
    Release,
    /// The administrator chooses, per category.
    Choice {
        default: Choice,
    },
    /// Handed to the recipient; a delete without one is refused while there
    /// is anything to hand over.
    Transfer,
    /// Kept: billing and audit records name a deleted identity.
    Keep,
    /// Gone within its stated retention window.
    Expire,
    Report,
}

/// What handing a missing identity's holdings to a live one does with a
/// category. Nobody can confirm the recipient was that person, so what the
/// person said and what was learned about them stays unless the
/// administrator also moves it; whatever acts, grants or signs in goes as a
/// delete naming that recipient: shared work is handed over, the rest erased.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrphanRule {
    /// Kept where it is, or moved as a merge moves it when asked.
    Personal,
    HandOver,
}

/// What suspending a person does with a category, and reactivating them
/// undoes. Whatever runs as a suspended person is refused at the moment it
/// would run, on every replica, whichever path suspended them; these rules
/// say what a suspension does beyond that.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SuspendRule {
    /// Held, with a record on each item of what the suspension held, and
    /// released exactly on reactivation: what the person or anyone else
    /// paused or resumed meanwhile is left as they left it.
    Hold,
    /// What is running as them is stopped. A stopped turn or sub-agent job
    /// is not started again.
    Stop,
    /// Refused while they are suspended; nothing is changed, so nothing is
    /// released.
    Refuse,
    /// Nothing here runs as them.
    Unaffected,
}

/// What one hand-over of a missing identity's holdings does with a category.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OrphanAction {
    Move,
    Keep,
    HandOver,
    Erase,
    Release,
    Report,
}

impl OrphanAction {
    pub const fn of(category: Category, personal: bool) -> Self {
        let rules = category.rules();
        match (rules.orphan, rules.delete) {
            (OrphanRule::Personal, _) if personal => Self::Move,
            (OrphanRule::Personal, _) => Self::Keep,
            (_, DeleteRule::Erase) => Self::Erase,
            (_, DeleteRule::Release) => Self::Release,
            (_, DeleteRule::Choice { .. } | DeleteRule::Transfer) => Self::HandOver,
            (_, DeleteRule::Keep | DeleteRule::Expire) => Self::Keep,
            (_, DeleteRule::Report) => Self::Report,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Choice {
    Erase,
    Transfer,
}

/// Every name a person is known by: their identity id, and each platform id
/// linked to it both as `platform:id` and bare (a request made before they
/// had an identity was recorded under the bare id).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Person {
    pub id: Uuid,
    pub display: String,
    pub names: Vec<String>,
    /// Their linked platform ids, `(platform, id)`.
    linked: Vec<(String, String)>,
}

impl Person {
    pub fn of(identity: &Identity) -> Self {
        let mut linked: Vec<(String, String)> = identity
            .linked_ids
            .iter()
            .map(|(platform, id)| (platform.clone(), id.clone()))
            .collect();
        linked.sort();
        let names = std::iter::once(identity.id.to_string())
            .chain(
                linked
                    .iter()
                    .flat_map(|(platform, id)| [format!("{platform}:{id}"), id.clone()]),
            )
            .collect();
        Self {
            id: identity.id,
            display: identity.display_name.clone(),
            names,
            linked,
        }
    }

    /// This person with the names an earlier, unfinished run of the same
    /// operation recorded: once their linked ids have moved they can no
    /// longer be read from the identity. A recorded `platform:id` is one of
    /// their links only when its bare `id` was recorded beside it, so an id
    /// that itself holds a colon is never read as a platform.
    fn with_names(mut self, recorded: &[String]) -> Self {
        for name in recorded {
            if !self.names.contains(name) {
                self.names.push(name.clone());
            }
        }
        for name in recorded {
            if let Some((platform, id)) = name.split_once(':') {
                let pair = (platform.to_string(), id.to_string());
                if recorded.iter().any(|n| n == id) && !self.linked.contains(&pair) {
                    self.linked.push(pair);
                }
            }
        }
        self
    }

    /// A missing identity, known by `names` besides its id.
    fn orphan(id: Uuid, names: &[String]) -> Self {
        let known: Vec<String> = std::iter::once(id.to_string())
            .chain(names.iter().cloned())
            .collect();
        Self {
            id,
            display: id.to_string(),
            names: Vec::new(),
            linked: Vec::new(),
        }
        .with_names(&known)
    }

    pub fn key(&self) -> String {
        self.id.to_string()
    }

    /// Their linked platform ids, `(platform, id)`.
    pub fn linked(&self) -> Vec<(&str, &str)> {
        self.linked
            .iter()
            .map(|(platform, id)| (platform.as_str(), id.as_str()))
            .collect()
    }

    /// A short tag naming where something moved from.
    pub fn tag(&self) -> String {
        self.key().chars().take(8).collect()
    }
}

/// What one category holds for a person, or did with it.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize)]
pub struct Tally {
    pub count: usize,
    /// What it is, by name (never a value or content), up to [`MAX_ITEMS`].
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
    /// Clashes the category's rule resolved.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub conflicts: Vec<String>,
    /// What keeps the operation from finishing until it is resolved and the
    /// operation is run again.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub blocking: Vec<String>,
    /// How many items a transfer would hand to a recipient.
    #[serde(skip)]
    pub transferable: usize,
    /// The items an administrator can transfer one at a time.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub movable: Vec<Movable>,
}

/// Who a category holds things for, by the names it keeps them under.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Owners {
    /// Each name with something here a delete would act on, or an
    /// administrator must change (for what is only reported), with how many
    /// items. Records kept by rule (billing, audit, who created what) are
    /// left out: they stay whatever happens to the person.
    pub held: BTreeMap<String, usize>,
    /// Names read from a field only identity ids are written to: one seen
    /// here was an identity.
    pub identities: BTreeSet<String>,
    /// Keyed so that a name cannot be read back (a per-agent hash): the
    /// clean-up asks [`Holding::held`] for each missing identity instead.
    pub probed: bool,
}

impl Owners {
    /// One item per name.
    pub fn of(names: impl IntoIterator<Item = String>) -> Self {
        Self::counted(names.into_iter().map(|name| (name, 1)))
    }

    pub fn counted(counts: impl IntoIterator<Item = (String, usize)>) -> Self {
        let mut owners = Self::default();
        for (name, count) in counts {
            owners.add(name, count);
        }
        owners
    }

    /// Names that prove an identity without holding anything to act on.
    pub fn proving(names: impl IntoIterator<Item = String>) -> Self {
        Self {
            identities: names.into_iter().filter(|n| !n.is_empty()).collect(),
            ..Self::default()
        }
    }

    pub fn probed() -> Self {
        Self {
            probed: true,
            ..Self::default()
        }
    }

    /// These names are read from a field only identity ids are written to.
    pub fn of_identities(mut self) -> Self {
        self.identities.extend(self.held.keys().cloned());
        self
    }

    pub fn add(&mut self, name: String, count: usize) {
        if !name.is_empty() && count > 0 {
            *self.held.entry(name).or_default() += count;
        }
    }

    pub fn absorb(&mut self, other: Owners) {
        for (name, count) in other.held {
            self.add(name, count);
        }
        self.identities.extend(other.identities);
        self.probed |= other.probed;
    }
}

/// An item [`Lifecycle::transfer`] can move, as a report names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Movable {
    pub label: String,
    pub item: Item,
}

impl Tally {
    pub fn of(items: impl IntoIterator<Item = String>) -> Self {
        let mut tally = Self::default();
        for item in items {
            tally.item(item);
        }
        tally
    }

    pub fn counted(count: usize) -> Self {
        Self {
            count,
            ..Self::default()
        }
    }

    pub fn item(&mut self, item: impl Into<String>) {
        self.count += 1;
        if self.items.len() < MAX_ITEMS {
            self.items.push(item.into());
        }
    }

    pub fn absorb(&mut self, other: Tally) {
        self.count += other.count;
        self.transferable += other.transferable;
        let room = MAX_ITEMS.saturating_sub(self.items.len());
        self.items.extend(other.items.into_iter().take(room));
        self.conflicts.extend(other.conflicts);
        self.blocking.extend(other.blocking);
        self.movable.extend(other.movable);
    }

    pub fn transferable(mut self) -> Self {
        self.transferable = self.count;
        self
    }
}

/// The installed hub's handles an operation reaches through. Per-agent
/// stores are reached through each serving agent in the registry; the rest
/// are install-wide.
pub struct Lifecycle {
    pub registry: Arc<dyn RegistryView>,
    pub identities: Arc<dyn IdentityAndTeamStore>,
    pub devices: Option<Arc<workforce_pairing::DevicePairingService>>,
    pub channels: Option<Arc<workforce_pairing::ChannelPairingService>>,
    pub defs: Option<Arc<workforce_orchestrator::workflow_store::WorkflowStore>>,
    pub artifacts: Option<Arc<workforce_orchestrator::artifact_store::KnowledgeStore>>,
    /// People's pictures.
    pub pictures: Option<Arc<dyn crate::pictures::PictureStore>>,
    pub external_skills: Arc<crate::external_skills::ExternalSkills>,
    pub data_dir: PathBuf,
    pub runs: Runs,
}

static INSTALLED: OnceLock<Arc<Lifecycle>> = OnceLock::new();

/// Make `lifecycle` the hub's, for the API and the admin tool. The first
/// call wins.
pub fn install(lifecycle: Lifecycle) {
    let _ = INSTALLED.set(Arc::new(lifecycle));
}

pub fn installed() -> Option<Arc<Lifecycle>> {
    INSTALLED.get().cloned()
}

/// How many operations the installed hub is running in the background.
pub fn running() -> usize {
    INSTALLED
        .get()
        .map_or(0, |lifecycle| lifecycle.runs.running())
}

/// One owned item a transfer moves.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Item {
    Job { agent: String, id: String },
    Workflow { id: String },
    Playbook { id: String },
    Monitor { id: String },
    ConsolidationTemplate { id: String },
}

#[derive(Debug, thiserror::Error)]
pub enum LifecycleError {
    #[error("{0}")]
    NotFound(String),
    #[error("{0}")]
    Refused(String),
    #[error("{0}")]
    Busy(String),
    #[error(transparent)]
    Failed(#[from] eyre::Report),
}

/// What a delete does with the categories that offer a choice.
#[derive(Debug, Clone, Default, Deserialize)]
pub struct DeletePlan {
    #[serde(default)]
    pub recipient: Option<Uuid>,
    #[serde(default)]
    pub choices: BTreeMap<Category, Choice>,
}

#[derive(Debug, Clone, Serialize)]
pub struct CategoryReport {
    pub category: Category,
    pub rules: Rules,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub choice: Option<Choice>,
    /// What a hand-over of a missing identity's holdings does with it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub orphan: Option<OrphanAction>,
    #[serde(flatten)]
    pub tally: Tally,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Report {
    pub operation: &'static str,
    pub subject: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<Uuid>,
    pub dry_run: bool,
    /// The operation finished: the subject was retired or deleted, or the
    /// item moved. `false` on a dry run, and when a category failed or is
    /// blocked; rerunning the operation resumes it.
    pub complete: bool,
    pub categories: Vec<CategoryReport>,
    /// The category under way, while the operation runs.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub progress: Option<Progress>,
    /// Why the operation stopped before every category was visited.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Progress {
    pub category: Category,
    pub done: usize,
    pub total: usize,
}

impl Report {
    fn new(operation: &'static str, subject: Uuid, target: Option<Uuid>, dry_run: bool) -> Self {
        Self {
            operation,
            subject,
            target,
            dry_run,
            complete: false,
            categories: Vec::new(),
            progress: None,
            error: None,
        }
    }

    fn finished(&self) -> bool {
        self.categories
            .iter()
            .all(|c| c.error.is_none() && c.tally.blocking.is_empty())
    }
}

/// The hub's handles and who is running the operation.
pub struct Context<'a> {
    pub lifecycle: &'a Lifecycle,
    pub actor: &'a Actor,
}

impl std::ops::Deref for Context<'_> {
    type Target = Lifecycle;

    fn deref(&self) -> &Lifecycle {
        self.lifecycle
    }
}

/// One store keyed by identity.
#[async_trait]
pub trait Holding: Send + Sync {
    /// Everyone it holds things for, by name: what the orphan clean-up
    /// diffs against the identity store.
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners>;
    /// What the person holds there.
    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally>;
    /// Move what `from` holds to `into`, by the category's merge rule. With
    /// `apply` off, report what it would do and change nothing.
    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally>;
    /// Remove what the person holds, by the category's delete rule; `to` is
    /// the recipient when the administrator chose to transfer.
    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        to: Option<&Person>,
        apply: bool,
    ) -> Result<Tally>;
}

/// The part of a store that holds or stops what runs as a person while they
/// are suspended.
#[async_trait]
pub trait Pausable: Send + Sync {
    /// Hold or stop what runs as the person, who is suspended.
    async fn suspend(&self, cx: &Context<'_>, who: &Person, apply: bool) -> Result<Tally>;
    /// Release what a suspension held for anyone no longer suspended.
    async fn resume(&self, cx: &Context<'_>, who: &Person, apply: bool) -> Result<Tally>;
}

/// The categories a suspension changes: exactly those whose rule is
/// [`SuspendRule::Hold`] or [`SuspendRule::Stop`]. Exhaustive, as
/// [`holding`] is.
pub fn pausable(category: Category) -> Option<&'static dyn Pausable> {
    match category {
        Category::ScheduledJobs => Some(&automation::ScheduledJobs),
        Category::Definitions => Some(&automation::Definitions),
        Category::WorkflowRuns => Some(&automation::WorkflowRuns),
        Category::SubagentJobs => Some(&automation::SubagentJobs),
        Category::Sessions => Some(&conversations::Sessions),
        Category::IdentityRecord
        | Category::RemoteMcpTokens
        | Category::Devices
        | Category::ChannelAllowlist
        | Category::Teams
        | Category::RoleGrants
        | Category::PermissionRules
        | Category::ApproverSlots
        | Category::BudgetOverrides
        | Category::Approvals
        | Category::HeartbeatChecks
        | Category::LearningPlans
        | Category::Memories
        | Category::PersonalFiles
        | Category::Vault
        | Category::KnowHow
        | Category::DecisionRecords
        | Category::Documents
        | Category::TrainingCorpora
        | Category::Artifacts
        | Category::Skills
        | Category::GameTallies
        | Category::Pictures
        | Category::Signals
        | Category::CostLedger
        | Category::ConfigReferences
        | Category::AuditRecords
        | Category::RetentionBound => None,
    }
}

/// Every category's holding. Exhaustive: a category added to [`Category`]
/// does not compile without one.
pub fn holding(category: Category) -> &'static dyn Holding {
    match category {
        Category::IdentityRecord => &access::IdentityRecord,
        Category::RemoteMcpTokens => &access::RemoteMcpTokens,
        Category::Devices => &access::Devices,
        Category::ChannelAllowlist => &access::ChannelAllowlist,
        Category::Teams => &access::Teams,
        Category::RoleGrants => &access::RoleGrants,
        Category::PermissionRules => &access::PermissionRules,
        Category::ApproverSlots => &access::ApproverSlots,
        Category::BudgetOverrides => &access::BudgetOverrides,
        Category::Approvals => &access::Approvals,
        Category::ScheduledJobs => &automation::ScheduledJobs,
        Category::Definitions => &automation::Definitions,
        Category::WorkflowRuns => &automation::WorkflowRuns,
        Category::SubagentJobs => &automation::SubagentJobs,
        Category::HeartbeatChecks => &automation::HeartbeatChecks,
        Category::LearningPlans => &automation::LearningPlans,
        Category::Sessions => &conversations::Sessions,
        Category::Memories => &conversations::Memories,
        Category::PersonalFiles => &conversations::PersonalFiles,
        Category::GameTallies => &conversations::GameTallies,
        Category::Pictures => &records::Pictures,
        Category::Signals => &conversations::Signals,
        Category::CostLedger => &conversations::CostLedger,
        Category::Vault => &records::Vault,
        Category::KnowHow => &records::KnowHow,
        Category::DecisionRecords => &records::DecisionRecords,
        Category::Documents => &records::Documents,
        Category::TrainingCorpora => &records::TrainingCorpora,
        Category::Artifacts => &records::Artifacts,
        Category::Skills => &records::Skills,
        Category::ConfigReferences => &records::ConfigReferences,
        Category::AuditRecords => &records::AuditRecords,
        Category::RetentionBound => &records::RetentionBound,
    }
}

impl Lifecycle {
    pub(crate) fn context<'a>(&'a self, actor: &'a Actor) -> Context<'a> {
        Context {
            lifecycle: self,
            actor,
        }
    }

    /// Every serving agent.
    pub(crate) fn services(&self) -> Vec<Arc<AgentService>> {
        self.registry.all_services()
    }

    /// The default agent, whose handles are the install-wide ones.
    pub(crate) fn hub(&self) -> Result<Arc<AgentService>> {
        self.registry
            .default_agent()
            .ok_or_else(|| eyre::eyre!("no agent is serving"))
    }

    fn identity(&self, id: Uuid) -> Result<Identity, LifecycleError> {
        self.identities
            .get_by_id(id)?
            .ok_or_else(|| LifecycleError::NotFound(format!("Identity {id} not found")))
    }

    /// The person `id` is, with the names an unfinished `kind` operation on
    /// them recorded.
    fn person(&self, id: Uuid, kind: &str) -> Result<Person, LifecycleError> {
        let identity = self.identity(id)?;
        let recorded = self
            .identities
            .list_ops(Some(id), RESUME_LOOKBACK)?
            .into_iter()
            .find(|op| op.kind == kind && op.status != OpStatus::Complete)
            .map(|op| op.names)
            .unwrap_or_default();
        Ok(Person::of(&identity).with_names(&recorded))
    }

    /// Everything `subject` holds, category by category.
    pub async fn holdings(&self, subject: Uuid, actor: &Actor) -> Result<Report, LifecycleError> {
        let who = Person::of(&self.identity(subject)?);
        let cx = self.context(actor);
        let mut report = Report::new("holdings", subject, None, true);
        for category in Category::ALL {
            let outcome = holding(category).held(&cx, &who).await;
            report.categories.push(category_report(category, outcome));
        }
        Ok(report)
    }

    /// Merge `from` into `into`: everything `from` holds moves to `into`, and
    /// `from` is retired once nothing is left.
    pub async fn merge(
        &self,
        from: Uuid,
        into: Uuid,
        actor: &Actor,
        dry_run: bool,
    ) -> Result<Report, LifecycleError> {
        self.perform(&Request::Merge { from, into }, actor, dry_run)
            .await
    }

    /// Delete `subject` and remove or archive everything it holds by `plan`.
    pub async fn delete(
        &self,
        subject: Uuid,
        plan: &DeletePlan,
        actor: &Actor,
        dry_run: bool,
    ) -> Result<Report, LifecycleError> {
        let request = Request::Delete {
            subject,
            plan: plan.clone(),
        };
        self.perform(&request, actor, dry_run).await
    }

    fn prepare_merge(
        &self,
        from: Uuid,
        into: Uuid,
        actor: &Actor,
    ) -> Result<runs::Operation, LifecycleError> {
        if from == into {
            return Err(LifecycleError::Refused(
                "An identity cannot be merged into itself".into(),
            ));
        }
        refuse_self(actor, from, "merge away")?;
        let source = self.person(from, "merge")?;
        let target = self.recipient(into, from)?;
        Ok(runs::Operation {
            kind: "merge",
            subject: Subject::Identity,
            who: source,
            target: Some(into),
            steps: Box::new(Merging { into: target }),
            actor: actor.clone(),
        })
    }

    /// Remove everything `who` holds by `plan`; the subject's identity row,
    /// when it has one, goes last.
    async fn prepare_erase(
        &self,
        kind: &'static str,
        subject: Subject,
        who: Person,
        plan: &DeletePlan,
        actor: &Actor,
    ) -> Result<runs::Operation, LifecycleError> {
        let recipient = plan
            .recipient
            .map(|id| self.recipient(id, who.id))
            .transpose()?;
        let choices: BTreeMap<Category, Choice> = Category::ALL
            .into_iter()
            .filter_map(|category| match category.rules().delete {
                DeleteRule::Choice { default } => Some((
                    category,
                    plan.choices.get(&category).copied().unwrap_or(default),
                )),
                DeleteRule::Transfer => Some((category, Choice::Transfer)),
                _ => None,
            })
            .collect();
        if recipient.is_none() {
            let cx = self.context(actor);
            let mut stranded = Vec::new();
            for (category, _) in choices.iter().filter(|(_, c)| **c == Choice::Transfer) {
                let held = holding(*category)
                    .held(&cx, &who)
                    .await
                    .map_err(LifecycleError::Failed)?;
                if held.transferable > 0 {
                    stranded.push(format!("{} ({})", category.name(), held.transferable));
                }
            }
            if !stranded.is_empty() {
                return Err(LifecycleError::Refused(format!(
                    "Name a recipient, or choose erase, for what others may rely on: {}",
                    stranded.join(", ")
                )));
            }
        }
        Ok(runs::Operation {
            kind,
            subject,
            who,
            target: plan.recipient,
            steps: Box::new(Erasing { recipient, choices }),
            actor: actor.clone(),
        })
    }

    /// Give one item `subject` owns to `to`, as an administrator's edit.
    pub async fn transfer(
        &self,
        subject: Uuid,
        item: &Item,
        to: Uuid,
        actor: &Actor,
    ) -> Result<Report, LifecycleError> {
        let from = Person::of(&self.identity(subject)?);
        let recipient = self.recipient(to, subject)?;
        let op = self.record_op("transfer", &from, Some(to), actor)?;
        let category = match item {
            Item::Job { .. } => Category::ScheduledJobs,
            _ => Category::Definitions,
        };
        let outcome =
            automation::transfer_item(&self.context(actor), &from, &recipient, item).await;
        let (tally, failure) = match outcome {
            Ok(tally) => (Ok(tally), None),
            Err(e) => (Err(eyre::eyre!("{e:#}")), Some(e)),
        };
        let mut report = Report::new("transfer", subject, Some(to), false);
        report.complete = failure.is_none();
        report.categories.push(category_report(category, tally));
        let status = match report.complete {
            true => OpStatus::Complete,
            false => OpStatus::Incomplete,
        };
        self.finish_op(&op, status, &report)?;
        match failure {
            Some(e) => Err(e),
            None => Ok(report),
        }
    }

    /// Suspend `subject`, then hold and stop what runs as them.
    pub async fn suspend(&self, subject: Uuid, actor: &Actor) -> Result<Report, LifecycleError> {
        refuse_self(actor, subject, "suspend")?;
        self.set_status(subject, IdentityStatus::Suspended)?;
        self.follow_standing(subject, actor).await
    }

    /// Make a suspended `subject` active again, then release what their
    /// suspension held.
    pub async fn resume(&self, subject: Uuid, actor: &Actor) -> Result<Report, LifecycleError> {
        let identity = self.identity(subject)?;
        if identity.status != IdentityStatus::Suspended {
            return Err(LifecycleError::Refused(format!(
                "{} is not suspended",
                identity.display_name
            )));
        }
        self.set_status(subject, IdentityStatus::Active)?;
        self.follow_standing(subject, actor).await
    }

    fn set_status(&self, subject: Uuid, status: IdentityStatus) -> Result<(), LifecycleError> {
        let mut identity = self.identity(subject)?;
        if identity.status != status {
            identity.status = status;
            identity.updated_at = Utc::now();
            self.identities.update(&identity)?;
            crate::standing::forget(subject);
        }
        Ok(())
    }

    /// Bring what runs as `subject` in line with their status now, however
    /// it was set: a suspended person's work is held and stopped, anyone
    /// else's is released from a suspension's hold. Recorded and leased as
    /// a `suspend` or `resume` operation; running it again changes nothing
    /// more.
    pub async fn follow_standing(
        &self,
        subject: Uuid,
        actor: &Actor,
    ) -> Result<Report, LifecycleError> {
        let identity = self.identity(subject)?;
        let suspended = identity.status == IdentityStatus::Suspended;
        let kind = match suspended {
            true => "suspend",
            false => "resume",
        };
        let operation = runs::Operation {
            kind,
            subject: Subject::Identity,
            who: Person::of(&identity),
            target: None,
            steps: Box::new(Standing { suspended }),
            actor: actor.clone(),
        };
        let op = self.claim(&operation)?;
        self.run(&operation, Some(&op)).await
    }

    /// The lifecycle operations recorded, newest first.
    pub fn operations(&self, subject: Option<Uuid>, limit: usize) -> Result<Vec<LifecycleOp>> {
        self.identities.list_ops(subject, limit)
    }

    /// One recorded operation, with its progress while it runs.
    pub fn operation(&self, id: Uuid) -> Result<LifecycleOp, LifecycleError> {
        self.identities
            .get_op(id)?
            .ok_or_else(|| LifecycleError::NotFound(format!("No lifecycle operation {id}")))
    }

    fn recipient(&self, id: Uuid, subject: Uuid) -> Result<Person, LifecycleError> {
        if id == subject {
            return Err(LifecycleError::Refused(
                "The recipient must be someone else".into(),
            ));
        }
        let identity = self.identity(id)?;
        if identity.status == IdentityStatus::Suspended {
            return Err(LifecycleError::Refused(format!(
                "{} is suspended and cannot receive anything",
                identity.display_name
            )));
        }
        Ok(Person::of(&identity))
    }

    /// `who`, a missing identity, as it stands now: still missing, and
    /// without any name a live identity has taken on since, whether the
    /// whole store or a direct lookup finds it. Checked before every
    /// category, under the operation's lease, so an identity created or
    /// linked meanwhile is never treated as gone.
    fn still_orphaned(&self, who: &Person) -> Result<Person, LifecycleError> {
        if self.identities.get_by_id(who.id)?.is_some() {
            return Err(LifecycleError::Refused(format!(
                "{} is an identity again; merge or delete it instead",
                who.id
            )));
        }
        let live = self.live_names()?;
        let mut names = Vec::new();
        for name in &who.names {
            if !live.contains(&fold(name)) && !self.claimed(who, name)? {
                names.push(name.clone());
            }
        }
        Ok(Person::orphan(who.id, &names))
    }

    /// A live identity answers to `name`, one of `who`'s, when looked up by
    /// it: as an identity id in any form, or as one of `who`'s links.
    fn claimed(&self, who: &Person, name: &str) -> Result<bool> {
        if let Ok(id) = Uuid::parse_str(name) {
            if self.identities.get_by_id(id)?.is_some() {
                return Ok(true);
            }
        }
        for (platform, id) in who.linked() {
            if (name == id || name == format!("{platform}:{id}"))
                && self.identities.get_by_linked_id(platform, id)?.is_some()
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Every name a live identity goes by, case folded, read whole: an
    /// identity missed here would be taken for gone, so a store that cannot
    /// be read whole, or changes while it is read, fails the read.
    fn live_names(&self) -> Result<BTreeSet<String>> {
        let expected = self.identities.count_filtered(&IdentityFilter::default())?;
        let mut names = BTreeSet::new();
        let mut ids = BTreeSet::new();
        let mut offset = 0;
        loop {
            let page = self.identities.list(&IdentityFilter {
                limit: Some(IDENTITY_PAGE),
                offset: Some(offset),
                ..IdentityFilter::default()
            })?;
            offset += page.len();
            ids.extend(page.iter().map(|identity| identity.id));
            names.extend(
                page.iter()
                    .flat_map(|identity| Person::of(identity).names)
                    .map(|name| fold(&name)),
            );
            if page.len() < IDENTITY_PAGE {
                break;
            }
        }
        let now = self.identities.count_filtered(&IdentityFilter::default())?;
        eyre::ensure!(
            ids.len() == expected && now == expected,
            "the identity store could not be read whole ({} read, {expected} then {now} counted); try again",
            ids.len()
        );
        Ok(names)
    }

    /// Renew `op`'s lease and record `report` as its progress, or stop:
    /// once a lapsed lease was taken over by another run, this one must
    /// write nothing more.
    fn hold(&self, op: &LifecycleOp, report: &Report) -> Result<(), LifecycleError> {
        let progress = serde_json::to_value(report).map_err(eyre::Report::from)?;
        match self
            .identities
            .renew_op(op.id, Utc::now() + LEASE, &progress)?
        {
            true => Ok(()),
            false => Err(LifecycleError::Busy(format!(
                "Another run took over this {} after its lease lapsed",
                op.kind
            ))),
        }
    }

    fn record_op(
        &self,
        kind: &str,
        who: &Person,
        target: Option<Uuid>,
        actor: &Actor,
    ) -> Result<LifecycleOp, LifecycleError> {
        let now = Utc::now();
        let op = LifecycleOp {
            id: Uuid::new_v4(),
            subject: who.id,
            kind: kind.to_string(),
            target,
            actor: actor.name(),
            names: who.names.clone(),
            status: OpStatus::Running,
            report: serde_json::json!({}),
            started_at: now,
            lease_until: now + LEASE,
            finished_at: None,
        };
        if !self.identities.claim_op(&op, now)? {
            return Err(LifecycleError::Busy(format!(
                "Another lifecycle operation on {} is still running",
                who.display
            )));
        }
        Ok(op)
    }

    fn finish_op(
        &self,
        op: &LifecycleOp,
        status: OpStatus,
        report: &Report,
    ) -> Result<(), LifecycleError> {
        let recorded = serde_json::to_value(report).map_err(eyre::Report::from)?;
        if !self
            .identities
            .finish_op(op.id, status, &recorded, Utc::now())?
        {
            return Err(LifecycleError::Busy(format!(
                "Another run took over this {} after its lease lapsed",
                op.kind
            )));
        }
        let counts: BTreeMap<&str, usize> = report
            .categories
            .iter()
            .filter(|c| c.tally.count > 0)
            .map(|c| (c.category.name(), c.tally.count))
            .collect();
        tracing::info!(
            target: "audit.identity_lifecycle",
            operation = op.kind,
            subject = %op.subject,
            target = ?op.target,
            actor = %op.actor,
            status = status.as_str(),
            counts = ?counts,
            "identity lifecycle operation"
        );
        workforce_memory::access_audit::emit(
            workforce_memory::access_audit::AccessAuditEvent::new(
                workforce_memory::access_audit::AuditOperation::IdentityLifecycle,
                workforce_memory::access_audit::AuditDecision::Allow,
                Some(&op.actor),
                Some(&op.subject.to_string()),
                op.target.map(|t| t.to_string()).as_deref(),
            ),
        );
        Ok(())
    }
}

/// `name` as compared with the names live identities go by: a platform
/// that ignores case, or an id written in capitals, still names them.
pub(super) fn fold(name: &str) -> String {
    name.to_lowercase()
}

/// Whom an operation acts on: an identity in the store, or one that no
/// longer is, whose holdings the orphan clean-up reaches.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Subject {
    Identity,
    Orphan,
}

/// What a merge does with each category.
struct Merging {
    into: Person,
}

#[async_trait]
impl runs::Steps for Merging {
    async fn step(
        &self,
        cx: &Context<'_>,
        category: Category,
        who: &Person,
        apply: bool,
    ) -> Result<Tally> {
        holding(category).merge(cx, who, &self.into, apply).await
    }

    fn retires(&self) -> bool {
        true
    }
}

/// What a delete does with each category: what the administrator chose to
/// transfer goes to the recipient, the rest by the category's rule.
struct Erasing {
    recipient: Option<Person>,
    choices: BTreeMap<Category, Choice>,
}

#[async_trait]
impl runs::Steps for Erasing {
    async fn step(
        &self,
        cx: &Context<'_>,
        category: Category,
        who: &Person,
        apply: bool,
    ) -> Result<Tally> {
        let to = self
            .choices
            .get(&category)
            .filter(|c| **c == Choice::Transfer)
            .and(self.recipient.as_ref());
        holding(category).delete(cx, who, to, apply).await
    }

    fn annotate(&self, report: &mut CategoryReport) {
        report.choice = self.choices.get(&report.category).copied();
    }

    fn retires(&self) -> bool {
        true
    }
}

/// What following a person's standing does with each category: a
/// suspended person's work is held and stopped, anyone else's released
/// from a suspension's hold.
struct Standing {
    suspended: bool,
}

#[async_trait]
impl runs::Steps for Standing {
    fn visits(&self, category: Category) -> bool {
        category.rules().suspend != SuspendRule::Unaffected
            && (self.suspended || pausable(category).is_some())
    }

    async fn step(
        &self,
        cx: &Context<'_>,
        category: Category,
        who: &Person,
        apply: bool,
    ) -> Result<Tally> {
        match (pausable(category), self.suspended) {
            (Some(pausable), true) => pausable.suspend(cx, who, apply).await,
            (Some(pausable), false) => pausable.resume(cx, who, apply).await,
            (None, _) => holding(category).held(cx, who).await,
        }
    }
}

/// An administrator may not merge away or delete the identity they act as:
/// the operation would end by removing who is running it.
fn refuse_self(actor: &Actor, subject: Uuid, what: &str) -> Result<(), LifecycleError> {
    match actor.0 == Some(subject) {
        true => Err(LifecycleError::Refused(format!(
            "You cannot {what} your own identity"
        ))),
        false => Ok(()),
    }
}

fn category_report(category: Category, outcome: Result<Tally>) -> CategoryReport {
    let (tally, error) = match outcome {
        Ok(tally) => (tally, None),
        Err(e) => (Tally::default(), Some(format!("{e:#}"))),
    };
    CategoryReport {
        category,
        rules: category.rules(),
        choice: None,
        orphan: None,
        tally,
        error,
    }
}
