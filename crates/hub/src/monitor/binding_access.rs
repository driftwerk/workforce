//! May this caller point a monitor at this channel?
//!
//! One rule for every surface that writes a monitor — the agent tool, the web
//! API, and anything added later — so a member can never reach through a
//! monitor to a channel or a person they cannot reach themselves. The rule
//! asks the surface for the binding's own platform, and answers "I cannot
//! tell" as a refusal with its reason rather than as a bare denial.
//!
//! Only what a write *introduces* is checked: a binding or a delivery target
//! the stored monitor already had asks no new question. A source has no
//! channel to be a member of — the owner's own credentials are what reads it
//! — so only where its action delivers is asked. Stopping a monitor —
//! `set_enabled`, `delete`, the engine's auto-disable — is never checked at
//! all, or a monitor would be trapped on a channel its owner has left. Which
//! *row* a writer may write stays [`crate::def_write_auth`]'s decision.

use std::sync::Arc;

use tokio::sync::RwLock;

use workforce_common::channels::SurfaceRegistry;
use workforce_orchestrator::monitor::definition::SURFACE_ANY;
use workforce_orchestrator::monitor::MonitorDefinition;

/// The one platform whose id a turn already carries: `CURRENT_PLATFORM_USER_ID`
/// is set on the Slack path alone.
const TURN_PLATFORM: &str = "slack";

/// The identity behind the current turn, when it is bound to one.
pub(crate) fn current_turn_user() -> Option<String> {
    workforce_agent::runner::CURRENT_USER_ID
        .try_with(|u| u.clone())
        .ok()
        .filter(|u| !u.is_empty())
}

/// A member's platform-native id on one platform (`slack`, `teams`), from
/// their linked ids. `None` when the account carries no link there.
pub type PlatformIdFn = Arc<dyn Fn(&str, &str) -> Option<String> + Send + Sync>;

/// The deny default: no account is linked anywhere (CLI, tests).
pub fn no_platform_ids() -> PlatformIdFn {
    Arc::new(|_, _| None)
}

/// Linked ids as the identity store holds them. The store is read at call
/// time, so a hub that binds it late resolves as soon as it is there. Unlike
/// `serve.rs`'s Slack-only resolver, a non-UUID identity does not pass through
/// as an already-platform id: the only turn that carries one is the Slack path,
/// which supplies it through [`BindingCaller::turn_platform_id`] instead.
pub fn linked_platform_ids(
    store: impl Fn() -> Option<Arc<dyn workforce_identity::store::IdentityStore>>
        + Send
        + Sync
        + 'static,
) -> PlatformIdFn {
    Arc::new(move |identity, platform| {
        let id = uuid::Uuid::parse_str(identity).ok()?;
        store()?
            .get_by_id(id)
            .ok()??
            .linked_ids
            .get(platform)
            .filter(|linked| !linked.is_empty())
            .cloned()
    })
}

/// Who is asking.
#[derive(Debug, Clone, Default)]
pub struct BindingCaller {
    pub is_admin: bool,
    /// The member behind the write; `None` is the hub itself (a system turn,
    /// the static gateway token) — there is no human to protect.
    pub identity: Option<String>,
    /// The platform id the turn already carries. Only the Slack path sets it.
    pub turn_platform_id: Option<String>,
}

impl BindingCaller {
    /// The turn's user, as an agent tool sees them.
    pub fn for_turn(is_admin: bool) -> Self {
        Self {
            is_admin,
            identity: current_turn_user(),
            turn_platform_id: workforce_agent::runner::CURRENT_PLATFORM_USER_ID
                .try_with(|id| id.clone())
                .ok()
                .filter(|id| !id.is_empty()),
        }
    }
}

/// The membership gate every monitor write passes through.
#[derive(Clone)]
pub struct BindingAccess {
    surfaces: Option<Arc<RwLock<SurfaceRegistry>>>,
    platform_ids: PlatformIdFn,
}

impl Default for BindingAccess {
    fn default() -> Self {
        Self::new(None, no_platform_ids())
    }
}

/// Why a monitor write was refused. The message is what the author sees.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum BindingDenied {
    #[error("you can only bind a monitor to a channel you're a member of ({channel})")]
    NotInWatched { channel: String },
    #[error("you can only deliver a monitor to a channel you're a member of or to your own DM")]
    NotInTarget,
    #[error("this hub cannot check your membership of {channel}: your account has no linked {platform} id — link it, or ask an administrator to save this monitor")]
    Unlinked { platform: String, channel: String },
    #[error("this hub cannot check membership of {channel} on {platform} — only an administrator may point a monitor there")]
    Unanswerable { platform: String, channel: String },
}

/// A membership answer, worst to best, so a fold over the binding's platforms
/// is `.max()`. The two "cannot tell" answers carry the platform they are about
/// so the refusal can name it; between two of a kind the concrete platform wins
/// over the binding's own `any`.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum Membership {
    Unanswerable(String),
    Unlinked(String),
    Out,
    In,
}

/// The channel a monitor watches, when it watches one.
fn watched(def: &MonitorDefinition) -> Option<(&str, &str)> {
    def.monitor
        .binding()
        .map(|binding| (binding.surface.as_str(), binding.channel.as_str()))
}

/// Where a monitor's action delivers, on the platform the definition names
/// for it: the action's own surface, else the watched channel's, else none
/// yet (the hub's only surface, resolved at the gate). `run_workflow`,
/// `run_skill` and `persona` deliver nowhere of their own.
fn delivered(def: &MonitorDefinition) -> Option<(Option<&str>, &str)> {
    let (target, named) = def.monitor.action.delivery()?;
    Some((named.or(watched(def).map(|(surface, _)| surface)), target))
}

/// The refusal a non-`In` answer about `channel` becomes; `out` supplies the
/// variant for an honest "no", which differs between a bind and a delivery.
fn refusal(
    answer: Membership,
    channel: &str,
    out: fn(&str) -> BindingDenied,
) -> Option<BindingDenied> {
    match answer {
        Membership::In => None,
        Membership::Out => Some(out(channel)),
        Membership::Unlinked(platform) => Some(BindingDenied::Unlinked {
            platform,
            channel: channel.to_string(),
        }),
        Membership::Unanswerable(platform) => Some(BindingDenied::Unanswerable {
            platform,
            channel: channel.to_string(),
        }),
    }
}

impl BindingAccess {
    pub fn new(surfaces: Option<Arc<RwLock<SurfaceRegistry>>>, platform_ids: PlatformIdFn) -> Self {
        Self {
            surfaces,
            platform_ids,
        }
    }

    /// Authorise what this save *introduces*: the channel it binds to and where
    /// its action delivers. `stored` is the definition as saved today; a part
    /// the write leaves untouched asks no new question and is not re-checked.
    pub async fn authorize(
        &self,
        caller: &BindingCaller,
        def: &MonitorDefinition,
        stored: Option<&MonitorDefinition>,
    ) -> Result<(), BindingDenied> {
        // An administrator binds and delivers anywhere.
        if caller.is_admin {
            return Ok(());
        }
        // A system/unbound turn writes the system row, which the definition-write
        // rule already restricts to administrators; there is no human to protect.
        let Some(identity) = caller.identity.as_deref() else {
            return Ok(());
        };

        if let Some((surface, channel)) = watched(def) {
            if !channel.is_empty() && stored.and_then(watched) != Some((surface, channel)) {
                let answer = self.membership(caller, identity, surface, channel).await;
                if let Some(denied) =
                    refusal(answer, channel, |channel| BindingDenied::NotInWatched {
                        channel: channel.to_string(),
                    })
                {
                    return Err(denied);
                }
            }
        }

        let Some((platform, target)) = delivered(def) else {
            return Ok(());
        };
        // An empty target, the already-gated watched channel, and a target the
        // stored monitor already had all ask nothing new.
        if target.is_empty()
            || watched(def).is_some_and(|(_, channel)| channel == target)
            || stored.and_then(delivered) == Some((platform, target))
        {
            return Ok(());
        }
        let surface = match platform {
            Some(platform) => platform.to_string(),
            None => match self.sole_platform().await {
                Some(platform) => platform,
                None => {
                    return Err(BindingDenied::Unanswerable {
                        platform: "any one surface".to_string(),
                        channel: target.to_string(),
                    })
                }
            },
        };
        let surface = surface.as_str();
        // Classified exactly as the engine resolves it: `@…` is a user DM,
        // anything else a channel.
        let answer = match target.strip_prefix('@') {
            Some(user) => {
                if self.is_self_dm(caller, identity, surface, user).await {
                    return Ok(());
                }
                Membership::Out
            }
            None => self.membership(caller, identity, surface, target).await,
        };
        match refusal(answer, target, |_| BindingDenied::NotInTarget) {
            Some(denied) => Err(denied),
            None => Ok(()),
        }
    }

    /// Is `identity` a member of `channel` on the platforms `surface` covers?
    /// `any` passes when *some* connected platform confirms it — a channel id
    /// belongs to one platform, and `any` only means "watch it wherever it is".
    async fn membership(
        &self,
        caller: &BindingCaller,
        identity: &str,
        surface: &str,
        channel: &str,
    ) -> Membership {
        // With no platform to ask — an `any` binding on a hub with no connected
        // surface — the honest answer is that the hub cannot tell.
        let mut best = Membership::Unanswerable(surface.to_string());
        for platform in self.platforms(surface).await {
            best = best.max(
                self.membership_on(caller, identity, &platform, channel)
                    .await,
            );
        }
        best
    }

    /// One platform's answer: the first surface to confirm wins, an honest "no"
    /// beats a surface that cannot answer, and an error — including the
    /// fail-closed default `is_channel_member` — cannot answer.
    async fn membership_on(
        &self,
        caller: &BindingCaller,
        identity: &str,
        platform: &str,
        channel: &str,
    ) -> Membership {
        let Some(user) = self.platform_id(caller, identity, platform) else {
            return Membership::Unlinked(platform.to_string());
        };
        let Some(surfaces) = &self.surfaces else {
            return Membership::Unanswerable(platform.to_string());
        };
        let mut best = Membership::Unanswerable(platform.to_string());
        for surface in surfaces.read().await.all_for(platform) {
            match surface.is_channel_member(channel, &user).await {
                Ok(true) => return Membership::In,
                Ok(false) => best = best.max(Membership::Out),
                Err(_) => {}
            }
        }
        best
    }

    /// Whether `user` names the caller themselves. The engine's `resolve_target`
    /// maps an internal identity id to its platform id, so both forms reach the
    /// same DM and the gate must accept both.
    async fn is_self_dm(
        &self,
        caller: &BindingCaller,
        identity: &str,
        surface: &str,
        user: &str,
    ) -> bool {
        if user == identity {
            return true;
        }
        for platform in self.platforms(surface).await {
            if self.platform_id(caller, identity, &platform).as_deref() == Some(user) {
                return true;
            }
        }
        false
    }

    /// The caller's id on `platform`: their linked account, or — on the one
    /// platform whose id a turn already carries — the turn's own.
    fn platform_id(
        &self,
        caller: &BindingCaller,
        identity: &str,
        platform: &str,
    ) -> Option<String> {
        (self.platform_ids)(identity, platform).or_else(|| {
            (platform == TURN_PLATFORM)
                .then(|| caller.turn_platform_id.clone())
                .flatten()
        })
    }

    /// The platforms a binding's surface covers: every registered one for
    /// `any`, otherwise the one it names.
    async fn platforms(&self, surface: &str) -> Vec<String> {
        if surface != SURFACE_ANY {
            return vec![surface.to_string()];
        }
        match &self.surfaces {
            Some(surfaces) => surfaces.read().await.platforms(),
            None => Vec::new(),
        }
    }

    async fn sole_platform(&self) -> Option<String> {
        match &self.surfaces {
            Some(surfaces) => crate::monitor::engine::sole_platform(surfaces).await,
            None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashSet;

    use async_trait::async_trait;
    use eyre::Result;
    use workforce_common::channels::{
        ChannelCapabilities, ChannelSurface, Reaction, SurfaceKey, SurfaceRegistry,
    };

    use super::*;

    const ALICE: &str = "alice";
    const ALICE_SLACK: &str = "U0ALICE";
    const BOB_SLACK: &str = "U0BOB";

    /// A surface that answers membership from a set — or, with `answers =
    /// false`, leaves `is_channel_member` to the trait's fail-closed default,
    /// which is what every non-Slack surface does today.
    struct MemberSurface {
        platform: String,
        members: HashSet<String>,
        answers: bool,
    }

    #[async_trait]
    impl ChannelSurface for MemberSurface {
        fn platform(&self) -> &str {
            &self.platform
        }
        fn capabilities(&self) -> ChannelCapabilities {
            ChannelCapabilities::default()
        }
        async fn add_reaction(&self, _: &Reaction) -> Result<()> {
            Ok(())
        }
        async fn remove_reaction(&self, _: &Reaction) -> Result<()> {
            Ok(())
        }
        async fn is_channel_member(&self, channel: &str, user: &str) -> Result<bool> {
            if !self.answers {
                return Err(eyre::eyre!(
                    "is_channel_member not supported on {}",
                    self.platform
                ));
            }
            Ok(self.members.contains(&format!("{user}@{channel}")))
        }
    }

    /// A registry of `(platform, answers, memberships)` surfaces, each
    /// membership written `user@channel`.
    fn registry(surfaces: &[(&str, bool, &[&str])]) -> Arc<RwLock<SurfaceRegistry>> {
        let mut reg = SurfaceRegistry::new();
        for (platform, answers, members) in surfaces {
            reg.register(
                SurfaceKey::workspace(*platform, "ws"),
                Arc::new(MemberSurface {
                    platform: platform.to_string(),
                    members: members.iter().map(|m| m.to_string()).collect(),
                    answers: *answers,
                }) as Arc<dyn ChannelSurface>,
            );
        }
        Arc::new(RwLock::new(reg))
    }

    /// A gate whose only linked account is alice's, on `platform`.
    fn gate(surfaces: Arc<RwLock<SurfaceRegistry>>, platform: &'static str) -> BindingAccess {
        BindingAccess::new(
            Some(surfaces),
            Arc::new(move |identity, asked| {
                (identity == ALICE && asked == platform).then(|| ALICE_SLACK.to_string())
            }),
        )
    }

    /// A web caller: an identity, no platform id of their own.
    fn web(identity: &str) -> BindingCaller {
        BindingCaller {
            is_admin: false,
            identity: Some(identity.to_string()),
            turn_platform_id: None,
        }
    }

    fn def(surface: &str, channel: &str, action: &str) -> MonitorDefinition {
        MonitorDefinition::from_toml(&format!(
            "[monitor]\nid = \"ops-releases\"\nname = \"Releases\"\n\
             [monitor.binding]\nsurface = \"{surface}\"\nchannel = \"{channel}\"\n\
             [monitor.filter]\ntype = \"all\"\n{action}"
        ))
        .expect("monitor")
    }

    fn notify(surface: &str, channel: &str, target: &str) -> MonitorDefinition {
        def(
            surface,
            channel,
            &format!(
                "[monitor.action]\ntype = \"notify\"\ntarget = \"{target}\"\ntemplate = \"x\"\n"
            ),
        )
    }

    fn workflow(surface: &str, channel: &str) -> MonitorDefinition {
        def(
            surface,
            channel,
            "[monitor.action]\ntype = \"run_workflow\"\nworkflow = \"release-note\"\n",
        )
    }

    fn agent_task(surface: &str, channel: &str, deliver_to: &str) -> MonitorDefinition {
        def(
            surface,
            channel,
            &format!(
                "[monitor.action]\ntype = \"agent_task\"\nprompt = \"p\"\ndeliver_to = \"{deliver_to}\"\n"
            ),
        )
    }

    fn slack_in(channels: &[&str]) -> Arc<RwLock<SurfaceRegistry>> {
        let members: Vec<String> = channels
            .iter()
            .map(|c| format!("{ALICE_SLACK}@{c}"))
            .collect();
        let refs: Vec<&str> = members.iter().map(String::as_str).collect();
        registry(&[("slack", true, &refs)])
    }

    #[tokio::test]
    async fn admin_binds_and_delivers_anywhere() {
        let caller = BindingCaller {
            is_admin: true,
            ..web(ALICE)
        };
        let access = gate(slack_in(&[]), "slack");
        assert!(access
            .authorize(&caller, &notify("slack", "C0OPS", "@U0BOB"), None)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn a_turn_with_no_identity_is_not_membership_checked() {
        let access = gate(slack_in(&[]), "slack");
        assert!(access
            .authorize(
                &BindingCaller::default(),
                &notify("slack", "C0OPS", "C0OUT"),
                None
            )
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn a_slack_turn_is_checked_through_the_id_it_carries() {
        // No linked account at all — the turn's own id is what answers.
        let access = BindingAccess::new(Some(slack_in(&["C0OPS"])), no_platform_ids());
        let caller = BindingCaller {
            turn_platform_id: Some(ALICE_SLACK.to_string()),
            ..web(ALICE)
        };
        assert!(access
            .authorize(&caller, &workflow("slack", "C0OPS"), None)
            .await
            .is_ok());
        assert_eq!(
            access
                .authorize(&caller, &workflow("slack", "C0DEV"), None)
                .await,
            Err(BindingDenied::NotInWatched {
                channel: "C0DEV".into()
            })
        );
    }

    #[tokio::test]
    async fn a_web_caller_is_checked_through_their_linked_id() {
        let access = gate(slack_in(&["C0OPS"]), "slack");
        assert!(access
            .authorize(&web(ALICE), &workflow("slack", "C0OPS"), None)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn a_non_member_may_not_bind() {
        let access = gate(slack_in(&["C0OPS"]), "slack");
        let denied = access
            .authorize(&web(ALICE), &workflow("slack", "C0DEV"), None)
            .await
            .expect_err("non-member");
        assert_eq!(
            denied.to_string(),
            "you can only bind a monitor to a channel you're a member of (C0DEV)"
        );
    }

    #[tokio::test]
    async fn a_caller_with_no_linked_id_is_told_to_link_it() {
        let access = gate(slack_in(&["C0OPS"]), "slack");
        let denied = access
            .authorize(&web("bob"), &workflow("slack", "C0OPS"), None)
            .await
            .expect_err("no linked id");
        assert_eq!(
            denied,
            BindingDenied::Unlinked {
                platform: "slack".into(),
                channel: "C0OPS".into()
            }
        );
        assert!(
            denied.to_string().contains("no linked slack id"),
            "{denied}"
        );
    }

    #[tokio::test]
    async fn a_surface_that_cannot_answer_membership_refuses_and_says_which_platform() {
        // The teams surface today: registered, but membership is left to the
        // trait's fail-closed default.
        let access = gate(registry(&[("teams", false, &[])]), "teams");
        let denied = access
            .authorize(&web(ALICE), &workflow("teams", "C0OPS"), None)
            .await
            .expect_err("unanswerable");
        assert_eq!(
            denied,
            BindingDenied::Unanswerable {
                platform: "teams".into(),
                channel: "C0OPS".into()
            }
        );
        assert!(
            denied
                .to_string()
                .contains("cannot check membership of C0OPS on teams"),
            "{denied}"
        );
    }

    #[tokio::test]
    async fn a_teams_binding_asks_the_teams_surface_not_slack() {
        // Alice is in C0OPS on teams and in nothing on slack. The old gate asked
        // slack for every binding and so could never pass this.
        let access = BindingAccess::new(
            Some(registry(&[
                ("slack", true, &[]),
                ("teams", true, &[&format!("{ALICE_SLACK}@C0OPS")]),
            ])),
            Arc::new(|identity, _| (identity == ALICE).then(|| ALICE_SLACK.to_string())),
        );
        assert!(access
            .authorize(&web(ALICE), &workflow("teams", "C0OPS"), None)
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn any_passes_when_one_connected_platform_confirms() {
        let access = BindingAccess::new(
            Some(registry(&[
                ("slack", true, &[]),
                ("teams", true, &[&format!("{ALICE_SLACK}@C0OPS")]),
            ])),
            Arc::new(|identity, _| (identity == ALICE).then(|| ALICE_SLACK.to_string())),
        );
        assert!(access
            .authorize(&web(ALICE), &workflow("any", "C0OPS"), None)
            .await
            .is_ok());
        // And an `any` binding nobody can confirm is still an honest refusal.
        assert_eq!(
            access
                .authorize(&web(ALICE), &workflow("any", "C0DEV"), None)
                .await,
            Err(BindingDenied::NotInWatched {
                channel: "C0DEV".into()
            })
        );
    }

    #[tokio::test]
    async fn delivery_to_an_unreachable_channel_is_refused() {
        // A member of the watched channel, not of the delivery target: the
        // delivery gate is what fires.
        let access = gate(slack_in(&["C0OPS"]), "slack");
        assert_eq!(
            access
                .authorize(&web(ALICE), &notify("slack", "C0OPS", "C0OUT"), None)
                .await,
            Err(BindingDenied::NotInTarget)
        );
        assert_eq!(
            access
                .authorize(&web(ALICE), &agent_task("slack", "C0OPS", "C0OUT"), None)
                .await,
            Err(BindingDenied::NotInTarget)
        );
    }

    #[tokio::test]
    async fn a_self_dm_is_allowed_by_platform_id_or_by_identity_id() {
        let access = gate(slack_in(&["C0OPS"]), "slack");
        for target in [&format!("@{ALICE_SLACK}"), &format!("@{ALICE}")] {
            assert!(
                access
                    .authorize(&web(ALICE), &notify("slack", "C0OPS", target), None)
                    .await
                    .is_ok(),
                "{target} is alice's own DM"
            );
        }
    }

    #[tokio::test]
    async fn another_users_dm_is_refused() {
        let access = gate(slack_in(&["C0OPS"]), "slack");
        let denied = access
            .authorize(
                &web(ALICE),
                &notify("slack", "C0OPS", &format!("@{BOB_SLACK}")),
                None,
            )
            .await
            .expect_err("another user's DM");
        assert_eq!(
            denied.to_string(),
            "you can only deliver a monitor to a channel you're a member of or to your own DM"
        );
    }

    #[tokio::test]
    async fn delivery_to_the_watched_channel_and_run_workflow_and_persona_need_no_check() {
        // A caller the surface can confirm nothing about: only the watched
        // channel is checked, and these three ask nothing beyond it.
        let access = gate(slack_in(&["C0OPS"]), "slack");
        assert!(access
            .authorize(&web(ALICE), &notify("slack", "C0OPS", "C0OPS"), None)
            .await
            .is_ok());
        assert!(access
            .authorize(&web(ALICE), &workflow("slack", "C0OPS"), None)
            .await
            .is_ok());
        let persona = def(
            "slack",
            "C0OPS",
            "[monitor.action]\ntype = \"persona\"\nagent = \"scout\"\nprompt = \"be helpful\"\n",
        );
        assert!(access.authorize(&web(ALICE), &persona, None).await.is_ok());
    }

    #[tokio::test]
    async fn an_edit_that_changes_neither_binding_nor_target_is_not_re_checked() {
        // The UI's Pause/Resume is a full PUT: a member who has left the channel
        // must still be able to stop their own monitor.
        let access = gate(slack_in(&[]), "slack");
        let stored = notify("slack", "C0OPS", "C0OUT");
        let mut edited = stored.clone();
        edited.monitor.enabled = false;
        assert!(access
            .authorize(&web(ALICE), &edited, Some(&stored))
            .await
            .is_ok());
    }

    #[tokio::test]
    async fn a_rebind_is_checked_even_when_the_action_is_unchanged() {
        let access = gate(slack_in(&["C0OPS"]), "slack");
        let stored = notify("slack", "C0OPS", "C0OUT");
        let rebound = notify("slack", "C0DEV", "C0OUT");
        assert_eq!(
            access.authorize(&web(ALICE), &rebound, Some(&stored)).await,
            Err(BindingDenied::NotInWatched {
                channel: "C0DEV".into()
            })
        );
    }

    fn source(action: &str) -> MonitorDefinition {
        MonitorDefinition::from_toml(&format!(
            "[monitor]\nid = \"invoices\"\nname = \"Invoices\"\n\
             [monitor.source]\ntool = \"google.gmail.search\"\nitems = \"/messages\"\n{action}"
        ))
        .expect("monitor")
    }

    #[tokio::test]
    async fn a_source_has_no_channel_to_be_in_and_gates_only_where_it_delivers() {
        // Alice is in C0OPS alone; her credentials, not her memberships, are
        // what read a source.
        let access = gate(slack_in(&["C0OPS"]), "slack");
        let run_skill = source("[monitor.action]\ntype = \"run_skill\"\ntool = \"books.file\"\n");
        assert!(access
            .authorize(&web(ALICE), &run_skill, None)
            .await
            .is_ok());
        let to_ops = source(
            "[monitor.action]\ntype = \"notify\"\ntarget = \"C0OPS\"\ntemplate = \"x\"\nsurface = \"slack\"\n",
        );
        assert!(access.authorize(&web(ALICE), &to_ops, None).await.is_ok());
        let to_dev = source(
            "[monitor.action]\ntype = \"notify\"\ntarget = \"C0DEV\"\ntemplate = \"x\"\nsurface = \"slack\"\n",
        );
        assert_eq!(
            access.authorize(&web(ALICE), &to_dev, None).await,
            Err(BindingDenied::NotInTarget)
        );
        let own_dm = source(&format!(
            "[monitor.action]\ntype = \"agent_task\"\nprompt = \"p\"\ndeliver_to = \"@{ALICE_SLACK}\"\nsurface = \"slack\"\n"
        ));
        assert!(access.authorize(&web(ALICE), &own_dm, None).await.is_ok());
    }

    #[tokio::test]
    async fn a_source_delivery_naming_no_surface_rides_the_hubs_only_one_or_is_unanswerable() {
        let unnamed =
            source("[monitor.action]\ntype = \"notify\"\ntarget = \"C0OPS\"\ntemplate = \"x\"\n");
        let access = gate(slack_in(&["C0OPS"]), "slack");
        assert!(access.authorize(&web(ALICE), &unnamed, None).await.is_ok());
        let two = BindingAccess::new(
            Some(registry(&[
                ("slack", true, &[&format!("{ALICE_SLACK}@C0OPS")]),
                ("teams", true, &[]),
            ])),
            Arc::new(|identity, _| (identity == ALICE).then(|| ALICE_SLACK.to_string())),
        );
        assert_eq!(
            two.authorize(&web(ALICE), &unnamed, None).await,
            Err(BindingDenied::Unanswerable {
                platform: "any one surface".into(),
                channel: "C0OPS".into()
            })
        );
    }

    #[tokio::test]
    async fn a_retargeted_delivery_is_checked_even_when_the_binding_is_unchanged() {
        let access = gate(slack_in(&["C0OPS"]), "slack");
        let stored = notify("slack", "C0OPS", "C0OUT");
        let retargeted = notify("slack", "C0OPS", "C0ELSE");
        assert_eq!(
            access
                .authorize(&web(ALICE), &retargeted, Some(&stored))
                .await,
            Err(BindingDenied::NotInTarget)
        );
    }
}
