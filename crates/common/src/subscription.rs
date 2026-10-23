//! Running an agent's work on a person's Claude or ChatGPT subscription: who
//! pays for a call, which login tier serves it, and how the cost ledger
//! labels it. Pure types; the vault-backed resolver lives in the hub.

use std::collections::BTreeSet;
use std::fmt;

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::settings::VaultSource;

pub use workforce_core::subscription::{split_per_user_key, ConnectPrompt, SubscriptionKind};

/// The provider a subscription call went through: the Claude Code CLI, the
/// Codex CLI, or the ChatGPT backend directly.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SubscriptionRoute {
    ClaudeCode,
    Codex,
    #[serde(rename = "chatgpt")]
    ChatGpt,
}

impl SubscriptionRoute {
    pub fn kind(self) -> SubscriptionKind {
        match self {
            Self::ClaudeCode => SubscriptionKind::Claude,
            Self::Codex | Self::ChatGpt => SubscriptionKind::ChatGpt,
        }
    }

    /// The cost-ledger prefix of calls served on this route.
    pub const fn label(self) -> &'static str {
        match self {
            Self::ClaudeCode => "claude-code",
            Self::Codex => "codex",
            Self::ChatGpt => "chatgpt",
        }
    }

    /// How the route reads to a person.
    pub const fn title(self) -> &'static str {
        match self {
            Self::ClaudeCode => "Claude Code",
            Self::Codex => "Codex",
            Self::ChatGpt => "ChatGPT",
        }
    }

    /// `model` without a ledger prefix of this route.
    fn bare_model(self, model: &str) -> &str {
        self.ledger_prefixes()
            .iter()
            .find_map(|prefix| model.strip_prefix(prefix)?.strip_prefix(':'))
            .unwrap_or(model)
    }

    /// Every ledger prefix a call on this route is recorded under: its label,
    /// and for Claude Code also the Claude Pilot driver's.
    const fn ledger_prefixes(self) -> &'static [&'static str] {
        match self {
            Self::ClaudeCode => &["claude-code", "claude-pilot"],
            Self::Codex => &["codex"],
            Self::ChatGpt => &["chatgpt"],
        }
    }
}

/// Where a login is stored, from one user on one agent to the whole install.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialTier {
    /// Agent vault `users/{uid}/<key>`.
    AgentUser,
    /// Global vault `users/{uid}/<key>`.
    GlobalUser,
    /// Agent vault `<key>`, gated by [`SharedServes`].
    AgentShared,
    /// Global vault `<key>`: the hub's own login, the last tier an agent
    /// looks in, gated by [`SharedServes`] like `AgentShared`.
    SystemShared,
}

impl CredentialTier {
    const ALL: [Self; 4] = [
        Self::AgentUser,
        Self::GlobalUser,
        Self::AgentShared,
        Self::SystemShared,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            Self::AgentUser => "agent_user",
            Self::GlobalUser => "global_user",
            Self::AgentShared => "agent_shared",
            Self::SystemShared => "system_shared",
        }
    }

    /// The tier a cost row's column names, if it names one.
    pub fn from_column(value: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|tier| tier.as_str() == value)
    }

    /// Whether this tier's login is one people share, gated by
    /// [`SharedServes`].
    pub const fn is_shared(self) -> bool {
        matches!(self, Self::AgentShared | Self::SystemShared)
    }
}

impl TryFrom<VaultSource> for CredentialTier {
    type Error = VaultSource;

    fn try_from(source: VaultSource) -> Result<Self, Self::Error> {
        match source {
            VaultSource::AgentPerUser => Ok(Self::AgentUser),
            VaultSource::GlobalPerUser => Ok(Self::GlobalUser),
            VaultSource::AgentSpecific => Ok(Self::AgentShared),
            VaultSource::Global => Ok(Self::SystemShared),
            VaultSource::Unknown => Err(source),
        }
    }
}

impl From<CredentialTier> for VaultSource {
    fn from(tier: CredentialTier) -> Self {
        match tier {
            CredentialTier::AgentUser => VaultSource::AgentPerUser,
            CredentialTier::GlobalUser => VaultSource::GlobalPerUser,
            CredentialTier::AgentShared => VaultSource::AgentSpecific,
            CredentialTier::SystemShared => VaultSource::Global,
        }
    }
}

/// Who an agent's shared login may serve besides the agent's own work.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SharedServes {
    /// Background work and the agent's owner (or the owning team's members).
    Owner,
    /// Also every identified person who talks to the agent.
    AgentUsers,
    /// Also unidentified callers and calls with no identity in scope: where
    /// a login is stored alone decides whom it serves.
    #[default]
    Anyone,
}

impl SharedServes {
    /// Whether a shared login may pay for `payer`'s call on an agent owned
    /// by `owner`. An outside caller is never served by a shared plan.
    pub fn admits(self, payer: &Payer, owner: &AgentOwner) -> bool {
        match payer {
            Payer::External(_) => false,
            Payer::Background(_) => true,
            Payer::Person(person) => self != Self::Owner || owner.includes(person),
            Payer::Unidentified(_) | Payer::Unset => self == Self::Anyone,
        }
    }
}

/// Whose subscription a call should draw on, classified from the identity
/// the call runs as (`CURRENT_USER_ID`), never the billing identity.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum Payer {
    /// A real identity.
    Person(Uuid),
    /// Automated work under an explicit sentinel identity.
    Background(String),
    /// A non-UUID identity that is not a sentinel: an unresolved platform id,
    /// a legacy owner string.
    Unidentified(String),
    /// An untrusted outside caller: an edge customer (`ext:*`) or a public
    /// game's player (`games`).
    External(String),
    /// No identity in scope at all: an identity gap, not background work.
    Unset,
}

/// The identity heartbeat turns run as.
pub const HEARTBEAT: &str = "heartbeat";
/// The identity scheduled work runs as when it has no person.
pub const SCHEDULER: &str = "scheduler";
/// The identity workflow steps run as when the run has no person.
pub const WORKFLOW_ENGINE: &str = "workflow-engine";
const MONITOR: &str = "monitor";
const GAMES: &str = "games";

/// The sources automated turns enter the service as, in place of a person.
pub const AUTOMATED_SOURCES: [&str; 5] = [HEARTBEAT, SCHEDULER, WORKFLOW_ENGINE, MONITOR, GAMES];

/// The automated sources whose output goes to a channel rather than to the
/// person they run as: a monitor reacting to channel events, a heartbeat
/// reporting to its delivery channel.
const CHANNEL_DELIVERED_SOURCES: [&str; 2] = [HEARTBEAT, MONITOR];

/// Whether a turn was triggered by one of the [`AUTOMATED_SOURCES`].
pub fn is_automated_source(user: &str) -> bool {
    AUTOMATED_SOURCES.contains(&user)
}

/// Whether a turn entering as `user` personalizes its prompt with the
/// private context (profile, pins, preferences, daily notes) of the person
/// it runs as. A turn whose output goes to a channel never does; its tools
/// still act as that person.
pub fn personalizes_prompt(user: &str) -> bool {
    !CHANNEL_DELIVERED_SOURCES.contains(&user)
}

const SYSTEM: &str = "system";
const INSTALL_PREFIX: &str = "system:";
const BACKGROUND_PREFIXES: [&str; 2] = [INSTALL_PREFIX, "agent:"];
const EXTERNAL_PREFIX: &str = "ext:";

/// Games run untrusted players' turns, so they are outside callers rather
/// than the agent's own automated work.
fn is_external(id: &str) -> bool {
    id.starts_with(EXTERNAL_PREFIX) || id == GAMES
}

/// Automated work the agent's shared login may pay for.
fn is_background(id: &str) -> bool {
    is_automated_source(id) || id == SYSTEM || BACKGROUND_PREFIXES.iter().any(|p| id.starts_with(p))
}

impl Payer {
    pub fn classify(current_user_id: Option<&str>) -> Self {
        match current_user_id.map(str::trim).filter(|id| !id.is_empty()) {
            None => Self::Unset,
            Some(id) => match Uuid::parse_str(id) {
                Ok(uuid) => Self::Person(uuid),
                Err(_) if is_external(id) => Self::External(id.to_string()),
                Err(_) if is_background(id) => Self::Background(id.to_string()),
                Err(_) => Self::Unidentified(id.to_string()),
            },
        }
    }

    /// Whose subscription a call draws on: the identity it runs as, unless
    /// that is background work or no one, which draws on the person it is
    /// billed to. A person's own work is never put on another person's
    /// plan, and an untrusted or external caller's never on anyone's.
    pub fn paying(run_as: Option<&str>, billing: Option<&str>) -> Self {
        match Self::classify(run_as) {
            unowned @ (Self::Background(_) | Self::Unset) => match Self::classify(billing) {
                person @ Self::Person(_) => person,
                _ => unowned,
            },
            payer => payer,
        }
    }

    /// The identity a call's spend is charged to, the budget it counts
    /// against: its explicit bill-to, unless that only names the person
    /// whose plan pays for background work ([`Self::paying`]), whose budget
    /// it never touches; else the identity it runs as.
    pub fn charged<'a>(run_as: Option<&'a str>, bill_to: Option<&'a str>) -> Option<&'a str> {
        let present = |id: Option<&'a str>| id.filter(|id| !id.trim().is_empty());
        let (run_as, bill_to) = (present(run_as), present(bill_to));
        let pays_for_background =
            matches!(Self::classify(run_as), Self::Background(_) | Self::Unset)
                && matches!(Self::classify(bill_to), Self::Person(_));
        bill_to.filter(|_| !pays_for_background).or(run_as)
    }

    /// The person whose own logins may serve the call.
    pub fn person(&self) -> Option<Uuid> {
        match self {
            Self::Person(uuid) => Some(*uuid),
            _ => None,
        }
    }
}

impl fmt::Display for Payer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Person(uuid) => write!(f, "{uuid}"),
            Self::Background(id) | Self::Unidentified(id) | Self::External(id) => f.write_str(id),
            Self::Unset => f.write_str("no identity"),
        }
    }
}

/// Whose plan pays for an agent's background work that runs as `runs_as`:
/// nobody else's for a person's own work or an untrusted or external
/// caller's; the install's operator for the install's own work (`system`,
/// `system:*`); otherwise the agent's owner, else the operator. The work
/// still runs as `runs_as`: only the plan changes.
pub fn background_payer(
    runs_as: &Payer,
    owner: &AgentOwner,
    operator: Option<Uuid>,
) -> Option<Uuid> {
    match runs_as {
        Payer::Person(_) | Payer::Unidentified(_) | Payer::External(_) => None,
        Payer::Background(id) if id == SYSTEM || id.starts_with(INSTALL_PREFIX) => operator,
        Payer::Background(_) | Payer::Unset => agent_work_payer(owner, operator),
    }
}

/// Whose plan pays for an agent's own background work: its owner when that
/// is one person, else the install's operator.
pub fn agent_work_payer(owner: &AgentOwner, operator: Option<Uuid>) -> Option<Uuid> {
    match owner {
        AgentOwner::User(owner) => Some(*owner),
        AgentOwner::Team { .. } | AgentOwner::Unowned => operator,
    }
}

/// Who owns an agent, as far as its shared login is concerned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentOwner {
    User(Uuid),
    Team { id: Uuid, members: BTreeSet<Uuid> },
    Unowned,
}

impl AgentOwner {
    pub fn includes(&self, person: &Uuid) -> bool {
        match self {
            Self::User(owner) => owner == person,
            Self::Team { members, .. } => members.contains(person),
            Self::Unowned => false,
        }
    }
}

/// Whose plan a login is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginOwner {
    Person(Uuid),
    Agent(AgentOwner),
    Install,
}

impl LoginOwner {
    /// The plan as a cost row names it: a uuid, `team:<id>`,
    /// `agent:<agent_id>` or `install`.
    pub fn payer(&self, agent_id: &str) -> String {
        match self {
            Self::Person(person) | Self::Agent(AgentOwner::User(person)) => person.to_string(),
            Self::Agent(AgentOwner::Team { id, .. }) => format!("{TEAM_PAYER}:{id}"),
            Self::Agent(AgentOwner::Unowned) => format!("{AGENT_PAYER}:{agent_id}"),
            Self::Install => INSTALL_PAYER.to_string(),
        }
    }
}

const TEAM_PAYER: &str = "team";
const AGENT_PAYER: &str = "agent";
const INSTALL_PAYER: &str = "install";

/// A cost row's payer read back from what [`LoginOwner::payer`] wrote.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PayerRef<'a> {
    Person(&'a str),
    Team(&'a str),
    Agent(&'a str),
    Install,
}

impl<'a> PayerRef<'a> {
    pub fn parse(payer: &'a str) -> Self {
        match (payer, payer.split_once(':')) {
            (INSTALL_PAYER, _) => Self::Install,
            (_, Some((TEAM_PAYER, id))) => Self::Team(id),
            (_, Some((AGENT_PAYER, id))) => Self::Agent(id),
            _ => Self::Person(payer),
        }
    }
}

/// A call a subscription paid for: the route, the tier the login came from,
/// and whose plan it was (a uuid, `team:<id>`, `agent:<id>` or `install`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Served {
    pub route: SubscriptionRoute,
    pub tier: CredentialTier,
    pub payer: String,
}

impl Served {
    /// The cost-ledger key of `model` served on this route: prefixed with the
    /// route unless an engine of the route already labelled it.
    pub fn ledger_model(&self, model: &str) -> String {
        let labelled = self.route.ledger_prefixes().iter().any(|prefix| {
            model
                .strip_prefix(prefix)
                .is_some_and(|rest| rest.is_empty() || rest.starts_with(':'))
        });
        if labelled {
            model.to_string()
        } else {
            format!("{}:{model}", self.route.label())
        }
    }
}

/// The line under an answer saying what ran it, if one is shown: a turn no
/// model ran gets none, nor a metered turn on a metered agent.
pub fn route_line(
    model: &str,
    served: Option<&Served>,
    subscription_agent: bool,
    person_name: impl Fn(&str) -> Option<String>,
) -> Option<String> {
    if model.is_empty() {
        return None;
    }
    match served {
        Some(served) => {
            let whose = match served.tier {
                CredentialTier::AgentShared => "the agent's shared login".to_string(),
                CredentialTier::SystemShared => "the install login".to_string(),
                personal => plan_label(
                    Some(&served.payer),
                    Some(&person_name(&served.payer).unwrap_or_else(|| "someone".to_string())),
                    Some(personal),
                ),
            };
            Some(format!(
                "{} · {} · {whose}",
                served.route.title(),
                served.route.bare_model(model)
            ))
        }
        None => subscription_agent.then(|| format!("Metered · {model}")),
    }
}

/// Whose plan paid for a call, by the tier its login came from: `payer`
/// as `payer_name` reads it (an unowned agent's shared login already reads
/// as such), else the bare payer.
pub fn plan_label(
    payer: Option<&str>,
    payer_name: Option<&str>,
    tier: Option<CredentialTier>,
) -> String {
    let name = payer_name.or(payer).unwrap_or("unknown");
    match (tier, payer.map(PayerRef::parse)) {
        (Some(CredentialTier::SystemShared), _) => "Install login".to_string(),
        (_, Some(PayerRef::Agent(_))) => name.to_string(),
        (Some(CredentialTier::AgentUser), _) => format!("{name}'s plan (this agent)"),
        (Some(CredentialTier::AgentShared), _) => format!("{name}'s plan (agent login)"),
        (Some(CredentialTier::GlobalUser) | None, _) => format!("{name}'s plan"),
    }
}

/// What a cost row paid with.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Billing {
    #[default]
    Metered,
    Subscription,
}

impl Billing {
    pub fn of(served: Option<&Served>) -> Self {
        match served {
            Some(_) => Self::Subscription,
            None => Self::Metered,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Metered => "metered",
            Self::Subscription => "subscription",
        }
    }

    /// The billing a cost row's column holds: metered unless it says
    /// subscription, as the column defaults.
    pub fn from_column(value: &str) -> Self {
        if value == Self::Subscription.as_str() {
            Self::Subscription
        } else {
            Self::Metered
        }
    }
}

/// What caused an LLM call: a turn of one kind, or a helper inside or after
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Origin {
    Interactive,
    Voice,
    Heartbeat,
    Scheduled,
    Monitor,
    Workflow,
    Consolidation,
    SelfLearning,
    Subagent,
    Games,
    ApiTask,
    Coding,
    Advisor,
    Compaction,
    PassiveMemory,
    Lessons,
    Vision,
    Transcription,
    Knowledge,
    McpTool,
    VoiceSummary,
}

/// The channel prefix of memory-consolidation sessions, which marks their
/// turns' origin.
pub const CONSOLIDATION_CHANNEL_PREFIX: &str = "memory-consolidation";

impl Origin {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Interactive => "interactive",
            Self::Voice => "voice",
            Self::Heartbeat => "heartbeat",
            Self::Scheduled => "scheduled",
            Self::Monitor => "monitor",
            Self::Workflow => "workflow",
            Self::Consolidation => "consolidation",
            Self::SelfLearning => "self_learning",
            Self::Subagent => "subagent",
            Self::Games => "games",
            Self::ApiTask => "api_task",
            Self::Coding => "coding",
            Self::Advisor => "advisor",
            Self::Compaction => "compaction",
            Self::PassiveMemory => "passive_memory",
            Self::Lessons => "lessons",
            Self::Vision => "vision",
            Self::Transcription => "transcription",
            Self::Knowledge => "knowledge",
            Self::McpTool => "mcp_tool",
            Self::VoiceSummary => "voice_summary",
        }
    }

    /// The origin of a turn entering through the service, from the
    /// triggering source (`user`), the session channel, and whether the turn
    /// was spoken.
    pub fn of_turn(user: &str, channel: &str, voice: bool) -> Self {
        match user {
            HEARTBEAT => Self::Heartbeat,
            SCHEDULER if channel.starts_with(CONSOLIDATION_CHANNEL_PREFIX) => Self::Consolidation,
            SCHEDULER => Self::Scheduled,
            MONITOR => Self::Monitor,
            WORKFLOW_ENGINE => Self::Workflow,
            GAMES => Self::Games,
            _ if voice => Self::Voice,
            _ => Self::Interactive,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALICE: &str = "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11";
    const BOB: &str = "0b7f3d52-8f0a-4c2f-9d44-3c1e6a1f4e22";

    fn uuid(s: &str) -> Uuid {
        Uuid::parse_str(s).unwrap()
    }

    #[test]
    fn payer_classifies_uuid_sentinels_ext_and_platform_ids() {
        assert_eq!(Payer::classify(Some(ALICE)), Payer::Person(uuid(ALICE)));
        for sentinel in [
            "heartbeat",
            "scheduler",
            "workflow-engine",
            "monitor",
            "system",
            "system:consolidation",
            "agent:allan",
        ] {
            assert_eq!(
                Payer::classify(Some(sentinel)),
                Payer::Background(sentinel.to_string()),
                "{sentinel}"
            );
        }
        assert_eq!(
            Payer::classify(Some("ext:cust-9")),
            Payer::External("ext:cust-9".to_string())
        );
        for other in ["U0123ABCD", "shared:team", "cli", "systemic"] {
            assert_eq!(
                Payer::classify(Some(other)),
                Payer::Unidentified(other.to_string()),
                "{other}"
            );
        }
    }

    #[test]
    fn a_person_is_paid_for_by_themselves_and_background_by_its_billed_person() {
        assert_eq!(
            Payer::paying(Some(ALICE), Some(BOB)),
            Payer::Person(uuid(ALICE)),
            "a person's turn never draws on the plan of the person it bills"
        );
        assert_eq!(
            Payer::paying(Some("heartbeat"), Some(BOB)),
            Payer::Person(uuid(BOB))
        );
        assert_eq!(Payer::paying(None, Some(BOB)), Payer::Person(uuid(BOB)));
        assert_eq!(
            Payer::paying(Some("heartbeat"), Some("system:indexing")),
            Payer::Background("heartbeat".into())
        );
        assert_eq!(
            Payer::paying(Some("games"), Some(BOB)),
            Payer::External("games".into()),
            "an untrusted game turn never spends a person's plan"
        );
        assert_eq!(
            Payer::paying(Some("ext:c"), Some(BOB)),
            Payer::External("ext:c".into()),
            "nor does an edge customer's"
        );
    }

    #[test]
    fn the_person_paying_for_background_work_is_never_charged_for_it() {
        let cases = [
            (
                Some(ALICE),
                Some("system:indexing"),
                Some("system:indexing"),
            ),
            (Some(ALICE), Some(BOB), Some(BOB)),
            (Some("heartbeat"), Some(BOB), Some("heartbeat")),
            (None, Some(BOB), None),
            (None, Some("system:indexing"), Some("system:indexing")),
            (Some("heartbeat"), Some(""), Some("heartbeat")),
            (Some(ALICE), None, Some(ALICE)),
        ];
        for (run_as, bill_to, charged) in cases {
            assert_eq!(
                Payer::charged(run_as, bill_to),
                charged,
                "{run_as:?} billed to {bill_to:?}"
            );
        }
    }

    #[test]
    fn background_work_is_billed_to_the_owner_else_the_operator() {
        let operator = Some(uuid(BOB));
        let alice = AgentOwner::User(uuid(ALICE));
        let team = AgentOwner::Team {
            id: Uuid::new_v4(),
            members: BTreeSet::from([uuid(ALICE)]),
        };
        let background = |id: &str| Payer::Background(id.into());

        assert_eq!(
            background_payer(&background("heartbeat"), &alice, operator),
            Some(uuid(ALICE))
        );
        assert_eq!(
            background_payer(&Payer::Unset, &alice, operator),
            Some(uuid(ALICE)),
            "an ownerless job is the agent's own work"
        );
        assert_eq!(
            background_payer(&background("agent:allan"), &team, operator),
            operator,
            "a team has no single plan: the operator pays"
        );
        assert_eq!(
            background_payer(&background("heartbeat"), &AgentOwner::Unowned, None),
            None
        );
        for install in ["system", "system:consolidation"] {
            assert_eq!(
                background_payer(&background(install), &alice, operator),
                operator,
                "{install} is the install's work"
            );
        }
        for not_billed in [
            Payer::Person(uuid(ALICE)),
            Payer::Unidentified("U0123ABCD".into()),
            Payer::External("games".into()),
            Payer::External("ext:c".into()),
        ] {
            assert_eq!(background_payer(&not_billed, &alice, operator), None);
        }
    }

    #[test]
    fn games_never_reach_a_shared_login_under_default_policy() {
        let games = Payer::classify(Some("games"));
        let owner = AgentOwner::User(uuid(ALICE));
        assert!(!SharedServes::default().admits(&games, &owner));
        for policy in [
            SharedServes::Owner,
            SharedServes::AgentUsers,
            SharedServes::Anyone,
        ] {
            assert!(!policy.admits(&games, &owner), "{policy:?}");
        }
        assert!(SharedServes::default().admits(&Payer::classify(Some("U0123ABCD")), &owner));
        assert!(is_automated_source("games"));
        assert_eq!(Origin::of_turn("games", "g", false), Origin::Games);
    }

    #[test]
    fn unset_payer_is_not_background() {
        assert_eq!(Payer::classify(None), Payer::Unset);
        assert_eq!(Payer::classify(Some("")), Payer::Unset);
        assert_eq!(Payer::classify(Some("  ")), Payer::Unset);
    }

    #[test]
    fn unset_payer_is_served_only_under_anyone() {
        let owner = AgentOwner::User(uuid(ALICE));
        assert!(!SharedServes::Owner.admits(&Payer::Unset, &owner));
        assert!(!SharedServes::AgentUsers.admits(&Payer::Unset, &owner));
        assert!(SharedServes::Anyone.admits(&Payer::Unset, &owner));
    }

    #[test]
    fn shared_serves_truth_table() {
        let owner = AgentOwner::User(uuid(ALICE));
        let alice = Payer::Person(uuid(ALICE));
        let bob = Payer::Person(uuid(BOB));
        let heartbeat = Payer::Background("heartbeat".into());
        let stranger = Payer::Unidentified("U999".into());
        let customer = Payer::External("ext:c".into());

        let admitted = |serves: SharedServes| {
            [&alice, &bob, &heartbeat, &stranger, &customer]
                .map(|payer| serves.admits(payer, &owner))
        };
        assert_eq!(
            admitted(SharedServes::Owner),
            [true, false, true, false, false]
        );
        assert_eq!(
            admitted(SharedServes::AgentUsers),
            [true, true, true, false, false]
        );
        assert_eq!(
            admitted(SharedServes::Anyone),
            [true, true, true, true, false]
        );
    }

    #[test]
    fn team_owner_includes_its_members_only() {
        let team = AgentOwner::Team {
            id: Uuid::new_v4(),
            members: BTreeSet::from([uuid(ALICE)]),
        };
        assert!(SharedServes::Owner.admits(&Payer::Person(uuid(ALICE)), &team));
        assert!(!SharedServes::Owner.admits(&Payer::Person(uuid(BOB)), &team));
        assert!(!SharedServes::Owner.admits(&Payer::Person(uuid(ALICE)), &AgentOwner::Unowned));
    }

    #[test]
    fn served_ledger_model_prefixes_the_route_once() {
        let served = Served {
            route: SubscriptionRoute::ChatGpt,
            tier: CredentialTier::AgentShared,
            payer: "agent:allan".into(),
        };
        assert_eq!(served.ledger_model("gpt-6.1-sol"), "chatgpt:gpt-6.1-sol");
        assert_eq!(
            served.ledger_model("chatgpt:gpt-6.1-sol"),
            "chatgpt:gpt-6.1-sol"
        );
        assert_eq!(served.ledger_model("chatgptx"), "chatgpt:chatgptx");
        let claude = Served {
            route: SubscriptionRoute::ClaudeCode,
            ..served
        };
        assert_eq!(claude.ledger_model("sonnet"), "claude-code:sonnet");
        assert_eq!(claude.ledger_model("claude-code"), "claude-code");
        assert_eq!(
            claude.ledger_model("claude-pilot:sonnet"),
            "claude-pilot:sonnet"
        );
        assert_eq!(claude.ledger_model("codex:x"), "claude-code:codex:x");
    }

    #[test]
    fn credential_tiers_round_trip_through_vault_sources() {
        for tier in [
            CredentialTier::AgentUser,
            CredentialTier::GlobalUser,
            CredentialTier::AgentShared,
            CredentialTier::SystemShared,
        ] {
            assert_eq!(CredentialTier::try_from(VaultSource::from(tier)), Ok(tier));
        }
        assert!(CredentialTier::try_from(VaultSource::Unknown).is_err());
    }

    #[test]
    fn turn_origin_follows_the_triggering_source() {
        assert_eq!(Origin::of_turn("heartbeat", "hb", false), Origin::Heartbeat);
        assert_eq!(
            Origin::of_turn("scheduler", "memory-consolidation-u1", false),
            Origin::Consolidation
        );
        assert_eq!(
            Origin::of_turn("scheduler", "slack:C1", false),
            Origin::Scheduled
        );
        assert_eq!(Origin::of_turn("monitor", "m", false), Origin::Monitor);
        assert_eq!(
            Origin::of_turn("workflow-engine", "w", false),
            Origin::Workflow
        );
        assert_eq!(Origin::of_turn("games", "g", false), Origin::Games);
        assert_eq!(Origin::of_turn(ALICE, "ios", true), Origin::Voice);
        assert_eq!(Origin::of_turn(ALICE, "slack", false), Origin::Interactive);
    }

    #[test]
    fn origin_and_tier_strings_match_their_serde_names() {
        for origin in [Origin::SelfLearning, Origin::ApiTask, Origin::VoiceSummary] {
            assert_eq!(
                serde_json::to_value(origin).unwrap(),
                serde_json::Value::String(origin.as_str().into())
            );
        }
        for tier in [CredentialTier::AgentUser, CredentialTier::SystemShared] {
            assert_eq!(
                serde_json::to_value(tier).unwrap(),
                serde_json::Value::String(tier.as_str().into())
            );
        }
        assert_eq!(
            serde_json::to_value(SubscriptionKind::ChatGpt).unwrap(),
            serde_json::Value::String("chatgpt".into())
        );
    }

    fn served(route: SubscriptionRoute, tier: CredentialTier) -> Served {
        Served {
            route,
            tier,
            payer: "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11".into(),
        }
    }

    fn alice(payer: &str) -> Option<String> {
        (payer == "6f1c1c3e-2a51-4a8e-9a57-0d6b1c9f2b11").then(|| "Alice".to_string())
    }

    #[test]
    fn route_line_names_route_model_and_plan_by_tier() {
        let line = |route, model: &str, tier| {
            route_line(model, Some(&served(route, tier)), true, alice).unwrap()
        };
        assert_eq!(
            line(
                SubscriptionRoute::ClaudeCode,
                "claude-code:sonnet",
                CredentialTier::AgentUser
            ),
            "Claude Code · sonnet · Alice's plan (this agent)"
        );
        assert_eq!(
            line(
                SubscriptionRoute::ChatGpt,
                "gpt-6.1-sol",
                CredentialTier::GlobalUser
            ),
            "ChatGPT · gpt-6.1-sol · Alice's plan"
        );
        assert_eq!(
            line(
                SubscriptionRoute::Codex,
                "codex:gpt-5.4",
                CredentialTier::AgentShared
            ),
            "Codex · gpt-5.4 · the agent's shared login"
        );
        assert_eq!(
            line(
                SubscriptionRoute::ClaudeCode,
                "claude-pilot:opus",
                CredentialTier::SystemShared
            ),
            "Claude Code · opus · the install login"
        );
        assert_eq!(
            route_line(
                "gpt-6.1-sol",
                Some(&served(
                    SubscriptionRoute::ChatGpt,
                    CredentialTier::GlobalUser
                )),
                false,
                |_| None,
            )
            .unwrap(),
            "ChatGPT · gpt-6.1-sol · someone's plan"
        );
    }

    #[test]
    fn plan_label_names_the_plan_by_the_tier_its_login_came_from() {
        let alice = |tier| plan_label(Some(ALICE), Some("Alice"), Some(tier));
        assert_eq!(
            alice(CredentialTier::AgentUser),
            "Alice's plan (this agent)"
        );
        assert_eq!(alice(CredentialTier::GlobalUser), "Alice's plan");
        assert_eq!(
            alice(CredentialTier::AgentShared),
            "Alice's plan (agent login)"
        );
        assert_eq!(
            plan_label(
                Some("install"),
                Some("Install login"),
                Some(CredentialTier::SystemShared)
            ),
            "Install login"
        );
        assert_eq!(
            plan_label(
                Some("agent:allan"),
                Some("allan's shared login"),
                Some(CredentialTier::AgentShared)
            ),
            "allan's shared login"
        );
    }

    #[test]
    fn plan_label_reads_an_unowned_agents_shared_login_as_its_name_and_an_unknown_payer_as_unknown()
    {
        assert_eq!(
            plan_label(Some("agent:allan"), None, Some(CredentialTier::AgentShared)),
            "agent:allan"
        );
        assert_eq!(
            plan_label(None, None, Some(CredentialTier::GlobalUser)),
            "unknown's plan"
        );
    }

    #[test]
    fn credential_tier_reads_back_from_its_column() {
        for tier in CredentialTier::ALL {
            assert_eq!(CredentialTier::from_column(tier.as_str()), Some(tier));
            assert_eq!(
                serde_json::to_value(tier).unwrap(),
                serde_json::json!(tier.as_str())
            );
        }
        assert_eq!(CredentialTier::from_column("metered"), None);
    }

    #[test]
    fn route_line_hidden_for_metered_turn_on_metered_agent() {
        assert_eq!(route_line("gemini-3.1-pro", None, false, alice), None);
    }

    #[test]
    fn route_line_shown_for_metered_fallback_on_subscription_agent() {
        assert_eq!(
            route_line("gemini-3.1-pro", None, true, alice).as_deref(),
            Some("Metered · gemini-3.1-pro")
        );
        assert_eq!(
            route_line("", None, true, alice),
            None,
            "a turn no model ran says nothing"
        );
    }

    #[test]
    fn payer_reads_back_as_written() {
        let person = Uuid::new_v4();
        let team = Uuid::new_v4();
        let person_payer = LoginOwner::Person(person).payer("allan");
        assert_eq!(
            PayerRef::parse(&person_payer),
            PayerRef::Person(&person_payer)
        );
        let team_payer = LoginOwner::Agent(AgentOwner::Team {
            id: team,
            members: BTreeSet::new(),
        })
        .payer("allan");
        assert_eq!(
            PayerRef::parse(&team_payer),
            PayerRef::Team(&team.to_string())
        );
        assert_eq!(
            PayerRef::parse(&LoginOwner::Agent(AgentOwner::Unowned).payer("allan")),
            PayerRef::Agent("allan")
        );
        assert_eq!(
            PayerRef::parse(&LoginOwner::Install.payer("allan")),
            PayerRef::Install
        );
    }

    #[test]
    fn login_keys_are_recognised_bare_and_per_user() {
        for (key, kind) in [
            ("codex/auth_json", Some(SubscriptionKind::ChatGpt)),
            ("users/u1/codex/auth_json", Some(SubscriptionKind::ChatGpt)),
            ("claude_code/oauth_token", Some(SubscriptionKind::Claude)),
            (
                "users/u1/claude_code/oauth_token",
                Some(SubscriptionKind::Claude),
            ),
            ("users/codex/auth_json", None),
            ("codex/auth_json/backup", None),
            ("skills/codex/auth_json", None),
            ("users/u1/x/codex/auth_json", None),
        ] {
            assert_eq!(SubscriptionKind::of_vault_key(key), kind, "{key}");
        }
    }
}
