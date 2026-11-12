//! An identity's verified email: one an outside source vouched for, kept in
//! its `linked_ids` beside the link it came with, and when. What a Desk
//! board's sync matches a shared Jira or Linear key's users by
//! ([`crate::by_my_email`]), and what `identity.whoami` reports.
//!
//! Two sources, tried in this order:
//! - [`Source::Oidc`]: an OIDC sign-in whose provider says the email is
//!   verified (`oidc_email`, `oidc_email_at`; written in [`crate::oidc`]).
//! - [`Source::Slack`]: the email Slack gives for the identity's linked
//!   Slack account, in the workspace the hub's bot is in, while Slack says
//!   that account is a full member: not a bot or an app, not deactivated,
//!   not a guest, not from another workspace (`slack_email`,
//!   `slack_email_at`; written in [`crate::slack_email`]).
//!
//! Only those write them. An edit of an identity's links keeps the stored
//! ones, and drops a source's when its link is removed or changed
//! ([`keep_stored_verified`]); a merge moves a source's only with its link;
//! suspending clears them all ([`forget_verified_email`]); one not renewed
//! in [`VERIFIED_EMAIL_FRESH_DAYS`] is stale.

use std::collections::HashMap;

use workforce_identity::types::{Identity, IdentityStatus};

/// How long a verified email stands without being renewed.
pub const VERIFIED_EMAIL_FRESH_DAYS: i64 = 90;

/// Key under which the OIDC `sub` is stored in `Identity::linked_ids`.
pub const OIDC_LINKED_ID_KEY: &str = "oidc";

/// Where an identity keeps the email its OIDC provider last verified for it
/// (`linked_ids`), and when: written only by an OIDC sign-in whose email the
/// provider says is verified. The hub trusts the identity provider's
/// `email_verified`. How it's kept, and what else may vouch for an email:
/// this module.
pub const OIDC_VERIFIED_EMAIL_KEY: &str = "oidc_email";
/// When [`OIDC_VERIFIED_EMAIL_KEY`] was last verified, RFC 3339.
pub const OIDC_VERIFIED_AT_KEY: &str = "oidc_email_at";

/// Who vouched for an email.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Source {
    Oidc,
    Slack,
}

impl Source {
    /// Every source, the preferred first.
    pub const ALL: [Source; 2] = [Source::Oidc, Source::Slack];

    /// The link the email comes with, and goes with.
    pub fn link(self) -> &'static str {
        match self {
            Self::Oidc => OIDC_LINKED_ID_KEY,
            Self::Slack => "slack",
        }
    }

    /// Where the email is kept.
    pub fn email_key(self) -> &'static str {
        match self {
            Self::Oidc => OIDC_VERIFIED_EMAIL_KEY,
            Self::Slack => "slack_email",
        }
    }

    /// When it was last vouched for, RFC 3339.
    pub fn at_key(self) -> &'static str {
        match self {
            Self::Oidc => OIDC_VERIFIED_AT_KEY,
            Self::Slack => "slack_email_at",
        }
    }

    /// As reported (`identity.whoami`, the by-email tools).
    pub fn name(self) -> &'static str {
        match self {
            Self::Oidc => "sign_in",
            Self::Slack => "slack",
        }
    }

    /// The source whose verified email `key` holds, if it's one.
    pub fn of_key(key: &str) -> Option<Source> {
        Self::ALL
            .into_iter()
            .find(|s| key == s.email_key() || key == s.at_key())
    }
}

/// Whether `key` is one of the verified-email links only their source writes.
pub fn is_verified_key(key: &str) -> bool {
    Source::of_key(key).is_some()
}

/// What an identity's verified email is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifiedEmail {
    /// Vouched for in the last [`VERIFIED_EMAIL_FRESH_DAYS`], by `Source`.
    Fresh(String, Source),
    /// Vouched for once, but not lately: a sign-in, or Slack, renews it.
    Stale,
    /// None, or the identity isn't active.
    None,
}

/// What one source says of `identity` at `now`.
fn state_of(
    identity: &Identity,
    source: Source,
    now: chrono::DateTime<chrono::Utc>,
) -> VerifiedEmail {
    let links = &identity.linked_ids;
    // Never without the link it came with.
    if links.get(source.link()).is_none_or(|l| l.trim().is_empty()) {
        return VerifiedEmail::None;
    }
    let Some(email) = links
        .get(source.email_key())
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| e.contains('@'))
    else {
        return VerifiedEmail::None;
    };
    let at = links
        .get(source.at_key())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&chrono::Utc));
    match at {
        Some(at) if now - at <= chrono::Duration::days(VERIFIED_EMAIL_FRESH_DAYS) => {
            VerifiedEmail::Fresh(email, source)
        }
        _ => VerifiedEmail::Stale,
    }
}

/// What `identity`'s verified email is at `now`: only an active identity's,
/// only a source's, only lately; a sign-in's before Slack's.
pub fn verified_email_state(
    identity: &Identity,
    now: chrono::DateTime<chrono::Utc>,
) -> VerifiedEmail {
    if !matches!(
        identity.status,
        IdentityStatus::Active | IdentityStatus::AutoProvisioned
    ) {
        return VerifiedEmail::None;
    }
    let mut stale = false;
    for source in Source::ALL {
        match state_of(identity, source, now) {
            fresh @ VerifiedEmail::Fresh(..) => return fresh,
            VerifiedEmail::Stale => stale = true,
            VerifiedEmail::None => {}
        }
    }
    if stale {
        VerifiedEmail::Stale
    } else {
        VerifiedEmail::None
    }
}

/// `identity`'s fresh verified email, lowercased, and who vouched for it.
pub fn verified(identity: &Identity) -> Option<(String, Source)> {
    match verified_email_state(identity, chrono::Utc::now()) {
        VerifiedEmail::Fresh(e, s) => Some((e, s)),
        _ => None,
    }
}

/// `identity`'s fresh verified email; none otherwise, whatever its `email`
/// says.
pub fn verified_email(identity: &Identity) -> Option<String> {
    verified(identity).map(|(e, _)| e)
}

/// `incoming` links with the verified-email ones of `stored` in place of
/// whatever it says of them: an edit never sets them. A source's go when
/// the edit removes or changes the link they came with.
pub fn keep_stored_verified(
    stored: &HashMap<String, String>,
    incoming: &mut HashMap<String, String>,
) {
    // Whom a person chose to be on a shared key is theirs alone to say.
    incoming.retain(|k, _| !is_verified_key(k) && !crate::chosen_me::is_chosen_key(k));
    for (k, v) in stored
        .iter()
        .filter(|(k, _)| crate::chosen_me::is_chosen_key(k))
    {
        incoming.insert(k.clone(), v.clone());
    }
    for source in Source::ALL {
        let link = source.link();
        if stored.get(link).is_none() || stored.get(link) != incoming.get(link) {
            continue;
        }
        for key in [source.email_key(), source.at_key()] {
            if let Some(v) = stored.get(key) {
                incoming.insert(key.to_string(), v.clone());
            }
        }
    }
}

/// Forgets every verified email of `identity` (suspended); whether it had
/// one.
pub fn forget_verified_email(identity: &mut Identity) -> bool {
    let had = identity.linked_ids.keys().any(|k| is_verified_key(k));
    identity.linked_ids.retain(|k, _| !is_verified_key(k));
    had
}

/// Forgets `source`'s verified email of `identity`; whether it had one.
pub fn forget(identity: &mut Identity, source: Source) -> bool {
    let had = identity.linked_ids.remove(source.email_key()).is_some();
    identity.linked_ids.remove(source.at_key());
    had
}

/// Keeps `email`, vouched for by `source` just now, as `identity`'s;
/// whether anything changed. Its time is renewed once a day at most, so a
/// sign-in or a Slack message doesn't always write.
pub fn renew(identity: &mut Identity, source: Source, email: &str) -> bool {
    let now = chrono::Utc::now();
    let email = email.trim().to_ascii_lowercase();
    if !email.contains('@') {
        return forget(identity, source);
    }
    let same = identity.linked_ids.get(source.email_key()) == Some(&email);
    if same && !due(identity, source, now) {
        return false;
    }
    identity
        .linked_ids
        .insert(source.email_key().to_string(), email);
    identity
        .linked_ids
        .insert(source.at_key().to_string(), now.to_rfc3339());
    true
}

/// Whether `source`'s verified email of `identity` was last vouched for a
/// day or more ago (or never): what's worth asking the source again.
pub fn due(identity: &Identity, source: Source, now: chrono::DateTime<chrono::Utc>) -> bool {
    identity
        .linked_ids
        .get(source.at_key())
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .is_none_or(|t| now - t.with_timezone(&chrono::Utc) >= chrono::Duration::days(1))
}

#[cfg(test)]
#[path = "verified_email_tests.rs"]
mod tests;
