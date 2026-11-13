//! A verified email from Slack: the email Slack gives for an identity's
//! linked Slack account, asked of the workspace the hub's bot is in, kept
//! only while Slack says that account is a full member of it. Kept as
//! `slack_email` and `slack_email_at` ([`crate::verified_email`]), renewed
//! when the link is used (a message from that account) and by a daily
//! sweep, and cleared when Slack says the account is deactivated, a guest
//! or a bot.
//!
//! Slack is trusted here as it already is when it links a Slack account to
//! an identity by email: a full member's email is one their workspace
//! admitted them by.

use std::sync::Arc;
use std::time::Duration;

use serde_json::Value;
use tracing::{info, warn};
use workforce_identity::store::{IdentityFilter, IdentityStore};
use workforce_identity::types::Identity;
use workforce_skills::adapters::slack::surface::SlackSurface;

use crate::verified_email::{self, Source};

/// What Slack says of an account.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlackSays {
    /// A full member of the bot's workspace, with this email (lowercased).
    Member(String),
    /// Not one whose email may vouch: deactivated, a bot or app, a guest,
    /// or with no email Slack will show. Its Slack email goes.
    NotMember(&'static str),
    /// Nothing to go by: Slack failed, the account is another workspace's
    /// (a shared channel's), or the bot's own workspace isn't known yet.
    /// Nothing changes.
    Unknown,
}

/// What `answer` (Slack's `users.info`) says of its user, in the workspace
/// `home_team`. Every flag must be there and say so: one missing doesn't
/// count as a member.
pub fn judge(answer: &Value, home_team: Option<&str>) -> SlackSays {
    let Some(home) = home_team.filter(|t| !t.is_empty()) else {
        return SlackSays::Unknown;
    };
    if answer["ok"].as_bool() != Some(true) {
        return SlackSays::Unknown;
    }
    let user = &answer["user"];
    let no = |flag: &str| user[flag].as_bool() == Some(false);
    // Another workspace's account (Slack Connect) is that workspace's to
    // judge, not this one's.
    if user["is_stranger"].as_bool() == Some(true)
        || user["team_id"].as_str().is_none_or(|t| t != home)
    {
        return SlackSays::Unknown;
    }
    if !no("deleted") {
        return SlackSays::NotMember("deactivated");
    }
    if !no("is_bot") || user["is_app_user"].as_bool() == Some(true) || user["id"] == "USLACKBOT" {
        return SlackSays::NotMember("a bot");
    }
    if !no("is_restricted") || !no("is_ultra_restricted") {
        return SlackSays::NotMember("a guest");
    }
    // An invite not yet taken up, or an address Slack hasn't confirmed,
    // isn't a member's email yet. Older answers leave these out.
    if user["is_invited_user"].as_bool() == Some(true) {
        return SlackSays::NotMember("not joined yet");
    }
    if user["is_email_confirmed"].as_bool() == Some(false) {
        return SlackSays::NotMember("email not confirmed");
    }
    match user["profile"]["email"]
        .as_str()
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| e.contains('@'))
    {
        Some(email) => SlackSays::Member(email),
        None => SlackSays::NotMember("no email shown"),
    }
}

/// Asks Slack about `slack_user`, through `surface`.
pub async fn ask(surface: &SlackSurface, slack_user: &str) -> SlackSays {
    match surface.user_info_answer(slack_user).await {
        Ok(answer) => judge(&answer, surface.home_team_id()),
        Err(e) => {
            warn!(error = %format!("{e:#}"), slack_user, "Slack users.info failed; the Slack email is left as it is");
            SlackSays::Unknown
        }
    }
}

/// Applies what Slack says of `slack_user` to `identity`, only while it is
/// still the identity's Slack link; whether anything changed.
pub fn apply(identity: &mut Identity, slack_user: &str, says: &SlackSays) -> bool {
    if identity
        .linked_ids
        .get(Source::Slack.link())
        .map(String::as_str)
        != Some(slack_user)
    {
        return false;
    }
    match says {
        SlackSays::Member(email) => verified_email::renew(identity, Source::Slack, email),
        SlackSays::NotMember(_) => verified_email::forget(identity, Source::Slack),
        SlackSays::Unknown => false,
    }
}

/// Renews `identity_id`'s Slack email when it's due (a day since), asking
/// Slack through `surface`; whether it changed.
pub async fn refresh(
    store: &dyn IdentityStore,
    surface: &SlackSurface,
    identity_id: uuid::Uuid,
) -> bool {
    let Ok(Some(identity)) = store.get_by_id(identity_id) else {
        return false;
    };
    refresh_one(store, surface, identity).await
}

async fn refresh_one(
    store: &dyn IdentityStore,
    surface: &SlackSurface,
    identity: Identity,
) -> bool {
    let Some(slack_user) = identity.linked_ids.get(Source::Slack.link()).cloned() else {
        return false;
    };
    if !verified_email::due(&identity, Source::Slack, chrono::Utc::now()) {
        return false;
    }
    let says = ask(surface, &slack_user).await;
    // Read again: what Slack said applies to the identity as it is now.
    let Ok(Some(mut identity)) = store.get_by_id(identity.id) else {
        return false;
    };
    if !apply(&mut identity, &slack_user, &says) {
        return false;
    }
    identity.updated_at = chrono::Utc::now();
    match store.update(&identity) {
        Ok(()) => {
            if let SlackSays::NotMember(why) = says {
                info!(identity = %identity.id, slack_user, why, "Slack email cleared");
            }
            true
        }
        Err(e) => {
            warn!(error = %format!("{e:#}"), identity = %identity.id, "Keeping the Slack email failed");
            false
        }
    }
}

/// Renews (or clears) every Slack-linked identity's Slack email that's due;
/// how many changed.
pub async fn sweep(store: &dyn IdentityStore, surface: &SlackSurface) -> usize {
    const PAGE: usize = 200;
    let mut changed = 0;
    let mut offset = 0;
    loop {
        let filter = IdentityFilter {
            limit: Some(PAGE),
            offset: Some(offset),
            ..Default::default()
        };
        let Ok(page) = store.list(&filter) else {
            return changed;
        };
        let last = page.len() < PAGE;
        offset += page.len();
        for identity in page {
            if identity.linked_ids.contains_key(Source::Slack.link())
                && refresh_one(store, surface, identity).await
            {
                changed += 1;
            }
        }
        if last {
            return changed;
        }
    }
}

/// How often every Slack-linked identity is asked about again.
pub const SWEEP_EVERY: Duration = Duration::from_secs(24 * 60 * 60);

/// Sweeps each of `surfaces` now (after a short wait for the bot's
/// workspace to be known) and then daily, until `stop`.
pub fn spawn_daily_sweep(
    store: Arc<dyn IdentityStore>,
    surfaces: Vec<Arc<SlackSurface>>,
    stop: tokio_util::sync::CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut tick = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(5 * 60),
            SWEEP_EVERY,
        );
        loop {
            tokio::select! {
                _ = stop.cancelled() => return,
                _ = tick.tick() => {}
            }
            for surface in &surfaces {
                let changed = sweep(store.as_ref(), surface).await;
                if changed > 0 {
                    info!(changed, "Slack emails renewed or cleared");
                }
            }
        }
    })
}

#[cfg(test)]
#[path = "slack_email_tests.rs"]
mod tests;
