//! Slack pictures, fetched by the hub: each agent's (its Slack identity's
//! icon, else its Slack bot's profile image) and each person's with a linked
//! Slack account (their Slack profile image). One is fetched soon after its
//! owner appears, again when what it was fetched for changes, and daily. A
//! fetch that fails is logged and tried again later; until one succeeds the
//! owner shows whatever it showed before, or no picture.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::Result;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};
use workforce_identity::store::{IdentityFilter, IdentityStore};

use super::{Image, Kind, Owner, PictureStore, SlackPicture, Source, MAX_BYTES};

/// How often the refresher looks for pictures that are due.
pub const TICK: Duration = Duration::from_secs(120);
/// How old a Slack picture gets before it is fetched again.
pub const STALE_AFTER: Duration = Duration::from_secs(24 * 60 * 60);
/// How long a failed fetch waits before it is tried again.
pub const RETRY_AFTER: Duration = Duration::from_secs(30 * 60);

/// Where an owner's Slack picture comes from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackFace {
    /// An icon the agent's Slack identity names.
    Icon {
        url: String,
        workspace: Option<String>,
    },
    /// A Slack user's profile image (an agent's bot, or a person); with no
    /// workspace, whichever of the hub's workspaces knows them.
    Member {
        user_id: String,
        workspace: Option<String>,
    },
}

impl SlackFace {
    /// What the picture is fetched for: when it changes, it is fetched again.
    pub fn origin(&self) -> String {
        match self {
            Self::Icon { url, .. } => format!("icon:{url}"),
            Self::Member { user_id, workspace } => match workspace {
                Some(ws) => format!("member:{ws}:{user_id}"),
                None => format!("member:{user_id}"),
            },
        }
    }
}

/// The hub's Slack, as the refresher uses it.
#[async_trait]
pub trait SlackDirectory: Send + Sync {
    /// Whether the hub's Slack has started: until it has, no agent seems to
    /// have a Slack identity, which must not be taken as theirs gone.
    async fn ready(&self) -> bool;
    /// Where agent `agent_id`'s Slack picture comes from, if it has one.
    /// Asks Slack nothing.
    async fn agent(&self, agent_id: &str) -> Option<SlackFace>;
    /// The image's address, and the workspace it is from.
    async fn locate(&self, face: &SlackFace) -> Result<(String, String)>;
    async fn download(&self, url: &str) -> Result<Image>;
}

/// The agents the hub serves now.
pub type Agents = Arc<dyn Fn() -> Vec<String> + Send + Sync>;

pub struct Refresher {
    store: Arc<dyn PictureStore>,
    slack: Arc<dyn SlackDirectory>,
    agents: Agents,
    identities: Option<Arc<dyn IdentityStore>>,
    /// When each owner's last fetch failed.
    failed: Mutex<HashMap<Owner, DateTime<Utc>>>,
}

/// Whether a Slack picture fetched for `kept` (its origin and when) is due
/// again for `origin` at `now`.
pub fn due(kept: Option<(&str, DateTime<Utc>)>, origin: &str, now: DateTime<Utc>) -> bool {
    match kept {
        None => true,
        Some((was, at)) => {
            was != origin
                || now
                    .signed_duration_since(at)
                    .to_std()
                    .is_ok_and(|age| age >= STALE_AFTER)
        }
    }
}

impl Refresher {
    pub fn new(
        store: Arc<dyn PictureStore>,
        slack: Arc<dyn SlackDirectory>,
        agents: Agents,
        identities: Option<Arc<dyn IdentityStore>>,
    ) -> Self {
        Self {
            store,
            slack,
            agents,
            identities,
            failed: Mutex::new(HashMap::new()),
        }
    }

    /// Fetches every Slack picture due at `now`, and removes those whose
    /// owner has no Slack picture any more.
    pub async fn refresh(&self, now: DateTime<Utc>) -> Result<()> {
        if !self.slack.ready().await {
            return Ok(());
        }
        let mut agents = Vec::new();
        for id in (self.agents)() {
            let face = self.slack.agent(&id).await;
            agents.push((id, face));
        }
        self.refresh_kind(Kind::Agent, agents, |_| true, now).await?;
        let Some(identities) = &self.identities else {
            return Ok(());
        };
        // People are read a page at a time while others come and go, so one
        // not read is gone only once the store says so.
        let gone = |id: &str| {
            uuid::Uuid::parse_str(id)
                .ok()
                .and_then(|id| identities.get_by_id(id).ok())
                .is_some_and(|found| found.is_none())
        };
        self.refresh_kind(Kind::Identity, linked_people(identities.as_ref())?, gone, now)
            .await
    }

    /// Fetches what is due of `wanted`, and removes the Slack pictures of
    /// those with none any more and of the owners not wanted that are
    /// `gone`.
    async fn refresh_kind(
        &self,
        kind: Kind,
        wanted: Vec<(String, Option<SlackFace>)>,
        gone: impl Fn(&str) -> bool,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let held = self.store.kept(kind).await?;
        let slack_of = |id: &str| {
            held.get(id)
                .and_then(|kept| kept.iter().find(|k| k.source == Source::Slack))
                .map(|k| (k.origin.clone().unwrap_or_default(), k.at))
        };
        let mut seen = HashSet::new();
        for (id, face) in wanted {
            seen.insert(id.clone());
            let owner = Owner { kind, id };
            let kept = slack_of(&owner.id);
            match face {
                None if kept.is_some() => {
                    self.store.remove(&owner, Source::Slack).await?;
                }
                None => {}
                Some(face)
                    if due(
                        kept.as_ref().map(|(o, at)| (o.as_str(), *at)),
                        &face.origin(),
                        now,
                    ) && !self.waiting(&owner, now) =>
                {
                    self.fetch(&owner, &face, now).await;
                }
                Some(_) => {}
            }
        }
        for id in held.keys().filter(|id| !seen.contains(*id)) {
            let owner = Owner {
                kind,
                id: id.clone(),
            };
            if slack_of(id).is_some() && gone(id) {
                self.store.remove(&owner, Source::Slack).await?;
            }
        }
        Ok(())
    }

    fn waiting(&self, owner: &Owner, now: DateTime<Utc>) -> bool {
        self.failed
            .lock()
            .expect("failed")
            .get(owner)
            .is_some_and(|at| {
                now.signed_duration_since(*at)
                    .to_std()
                    .map_or(true, |since| since < RETRY_AFTER)
            })
    }

    async fn fetch(&self, owner: &Owner, face: &SlackFace, now: DateTime<Utc>) {
        let fetched = async {
            let (url, workspace) = self.slack.locate(face).await?;
            let image = self.slack.download(&url).await?;
            let picture = SlackPicture {
                image,
                origin: face.origin(),
                workspace,
            };
            self.store.put_slack(owner, &picture).await
        }
        .await;
        let mut failed = self.failed.lock().expect("failed");
        match fetched {
            Ok(()) => {
                failed.remove(owner);
                info!(target: "pictures", %owner, "fetched Slack picture");
            }
            Err(e) => {
                failed.insert(owner.clone(), now);
                warn!(target: "pictures", %owner, error = %format!("{e:#}"), "couldn't fetch Slack picture; trying again later");
            }
        }
    }

    /// Refreshes every [`TICK`] until `cancel`; with peers, only while this
    /// replica leads.
    pub fn spawn(
        self,
        leader: Option<Arc<AtomicBool>>,
        cancel: CancellationToken,
    ) -> tokio::task::JoinHandle<()> {
        tokio::spawn(async move {
            let mut tick = tokio::time::interval(TICK);
            tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                tokio::select! {
                    _ = cancel.cancelled() => return,
                    _ = tick.tick() => {}
                }
                if leader.as_ref().is_some_and(|l| !l.load(Ordering::Relaxed)) {
                    continue;
                }
                if let Err(e) = self.refresh(Utc::now()).await {
                    warn!(target: "pictures", error = %format!("{e:#}"), "refreshing Slack pictures failed");
                }
            }
        })
    }
}

/// Identities read per page.
const PAGE: usize = 500;

/// Every identity, with where its Slack picture comes from when it's
/// linked to a Slack account.
fn linked_people(identities: &dyn IdentityStore) -> Result<Vec<(String, Option<SlackFace>)>> {
    let mut people = Vec::new();
    loop {
        let page = identities.list(&IdentityFilter {
            limit: Some(PAGE),
            offset: Some(people.len()),
            ..IdentityFilter::default()
        })?;
        let read = page.len();
        people.extend(page.into_iter().map(|identity| {
            let face = identity
                .linked_ids
                .get("slack")
                .filter(|id| !id.is_empty())
                .map(|user_id| SlackFace::Member {
                    user_id: user_id.clone(),
                    workspace: None,
                });
            (identity.id.to_string(), face)
        }));
        if read < PAGE {
            return Ok(people);
        }
    }
}

/// Whether the hub fetches a picture from `url`: only over https.
pub fn fetchable(url: &str) -> bool {
    reqwest::Url::parse(url).is_ok_and(|u| u.scheme() == "https" && u.host().is_some())
}

/// The hub's Slack: its connected workspaces and its agents' Slack
/// identities.
pub struct HubSlack {
    slack: crate::slack_onboarding::SlackSlot,
    registry: Arc<crate::agent_registry::AgentRegistry>,
    http: reqwest::Client,
}

impl HubSlack {
    pub fn new(
        slack: crate::slack_onboarding::SlackSlot,
        registry: Arc<crate::agent_registry::AgentRegistry>,
    ) -> Self {
        Self {
            slack,
            registry,
            http: reqwest::Client::builder()
                .timeout(Duration::from_secs(15))
                .redirect(reqwest::redirect::Policy::custom(|attempt| {
                    if fetchable(attempt.url().as_str()) && attempt.previous().len() < 5 {
                        attempt.follow()
                    } else {
                        attempt.stop()
                    }
                }))
                .build()
                .unwrap_or_default(),
        }
    }

    async fn surfaces(&self) -> crate::slack_manager::WorkspaceSurfaces {
        match self.slack.get() {
            Some(control) => control.manager().lock().await.workspace_surfaces(),
            None => Default::default(),
        }
    }
}

#[async_trait]
impl SlackDirectory for HubSlack {
    async fn ready(&self) -> bool {
        self.slack.get().is_some()
    }

    async fn agent(&self, agent_id: &str) -> Option<SlackFace> {
        let bot = match self.registry.get(agent_id) {
            Some(service) => {
                let identities = service.slack_bot_identities.read().await;
                identities
                    .iter()
                    .min_by(|a, b| a.0.cmp(b.0))
                    .map(|(ws, bot)| (ws.clone(), bot.user_id.clone()))
            }
            None => None,
        };
        let icon = match self.slack.get() {
            Some(control) => control.manager().lock().await.agent_icon_url(agent_id),
            None => None,
        };
        match (icon, bot) {
            (Some(url), bot) => Some(SlackFace::Icon {
                url,
                workspace: bot.map(|(ws, _)| ws),
            }),
            (None, Some((ws, user_id))) => Some(SlackFace::Member {
                user_id,
                workspace: Some(ws),
            }),
            (None, None) => None,
        }
    }

    async fn locate(&self, face: &SlackFace) -> Result<(String, String)> {
        let (user_id, workspace) = match face {
            SlackFace::Icon { url, workspace } => {
                return Ok((url.clone(), workspace.clone().unwrap_or_default()))
            }
            SlackFace::Member { user_id, workspace } => (user_id, workspace),
        };
        let mut surfaces: Vec<_> = self
            .surfaces()
            .await
            .into_iter()
            .filter(|(ws, _)| workspace.as_ref().is_none_or(|w| w == ws))
            .collect();
        surfaces.sort_by(|a, b| a.0.cmp(&b.0));
        let mut last = None;
        for (ws, surface) in surfaces {
            match surface.member_info(user_id).await {
                Ok(member) => match member.avatar_url.filter(|u| !u.is_empty()) {
                    Some(url) => {
                        return Ok((
                            workforce_skills::adapters::user_photo::larger_avatar(&url),
                            ws,
                        ))
                    }
                    None => last = Some(eyre::eyre!("{user_id} has no Slack picture in {ws}")),
                },
                Err(e) => last = Some(e.wrap_err(format!("looking up {user_id} in {ws}"))),
            }
        }
        Err(last.unwrap_or_else(|| eyre::eyre!("no connected Slack workspace knows {user_id}")))
    }

    async fn download(&self, url: &str) -> Result<Image> {
        if !fetchable(url) {
            eyre::bail!("not an https address: {url}");
        }
        let mut response = self.http.get(url).send().await?.error_for_status()?;
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await? {
            bytes.extend_from_slice(&chunk);
            if bytes.len() > MAX_BYTES {
                eyre::bail!("{url} is larger than a picture can be");
            }
        }
        Ok(Image::accept(bytes)?)
    }
}
