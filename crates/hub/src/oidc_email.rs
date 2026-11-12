//! The email an identity's OIDC provider verified, as the identity keeps
//! it in `linked_ids`: what a board's sync may match a shared key's users
//! by. Plain data about an identity, so it builds with or without the
//! `oidc` feature; the sign-in that writes it is [`crate::oidc`]'s.

use workforce_identity::types::{Identity, IdentityStatus};

/// Key under which the OIDC `sub` is stored in `Identity::linked_ids`.
pub const OIDC_LINKED_ID_KEY: &str = "oidc";

/// Where an identity keeps the email its OIDC provider last verified for it
/// (`linked_ids`), and when: written only by an OIDC sign-in whose email the
/// provider says is verified. An admin's edit of an identity's links keeps
/// the stored ones whatever it sends ([`keep_stored_verified`]), a merge
/// moves them only with the `oidc` link itself, and suspending clears them.
/// What a board's sync may match a shared Jira or Linear key's users by
/// ([`verified_email`]). The hub trusts the identity provider's
/// `email_verified`.
pub const OIDC_VERIFIED_EMAIL_KEY: &str = "oidc_email";
/// When [`OIDC_VERIFIED_EMAIL_KEY`] was last verified, RFC 3339.
pub const OIDC_VERIFIED_AT_KEY: &str = "oidc_email_at";

/// How long a verified email stands without another sign-in.
pub const VERIFIED_EMAIL_FRESH_DAYS: i64 = 90;

/// Whether `key` is one of the verified-email links only a sign-in writes.
pub fn is_verified_key(key: &str) -> bool {
    key == OIDC_VERIFIED_EMAIL_KEY || key == OIDC_VERIFIED_AT_KEY
}

/// What an identity's verified email is now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerifiedEmail {
    /// Verified at a sign-in in the last [`VERIFIED_EMAIL_FRESH_DAYS`].
    Fresh(String),
    /// Verified once, but not lately: a sign-in renews it.
    Stale,
    /// None: never signed in through OIDC, or not active.
    None,
}

/// What `identity`'s verified email is at `now`: only an active identity's,
/// only the provider's, and only lately.
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
    let Some(email) = identity
        .linked_ids
        .get(OIDC_VERIFIED_EMAIL_KEY)
        .map(|e| e.trim().to_ascii_lowercase())
        .filter(|e| e.contains('@'))
    else {
        return VerifiedEmail::None;
    };
    let at = identity
        .linked_ids
        .get(OIDC_VERIFIED_AT_KEY)
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .map(|t| t.with_timezone(&chrono::Utc));
    match at {
        Some(at) if now - at <= chrono::Duration::days(VERIFIED_EMAIL_FRESH_DAYS) => {
            VerifiedEmail::Fresh(email)
        }
        _ => VerifiedEmail::Stale,
    }
}

/// The email `identity`'s OIDC provider verified lately, lowercased; none
/// otherwise, whatever its `email` says.
pub fn verified_email(identity: &Identity) -> Option<String> {
    match verified_email_state(identity, chrono::Utc::now()) {
        VerifiedEmail::Fresh(e) => Some(e),
        _ => None,
    }
}

/// `incoming` links with the verified-email ones of `stored` in place of
/// whatever it says of them: an edit never sets them.
pub fn keep_stored_verified(
    stored: &std::collections::HashMap<String, String>,
    incoming: &mut std::collections::HashMap<String, String>,
) {
    incoming.retain(|k, _| !is_verified_key(k));
    for (k, v) in stored.iter().filter(|(k, _)| is_verified_key(k)) {
        incoming.insert(k.clone(), v.clone());
    }
}

/// Forgets `identity`'s verified email (suspended, offboarded); whether it
/// had one.
pub fn forget_verified_email(identity: &mut Identity) -> bool {
    let had = identity.linked_ids.contains_key(OIDC_VERIFIED_EMAIL_KEY);
    identity.linked_ids.retain(|k, _| !is_verified_key(k));
    had
}

/// Keeps `email` (verified by the provider just now) as `identity`'s
/// verified email; whether anything changed. Its time is renewed once a day
/// at most, so a sign-in doesn't always write.
#[cfg(feature = "oidc")]
pub(crate) fn keep_verified_email(identity: &mut Identity, email: &str) -> bool {
    let now = chrono::Utc::now();
    let email = email.trim().to_ascii_lowercase();
    let same = identity.linked_ids.get(OIDC_VERIFIED_EMAIL_KEY) == Some(&email);
    let recent = identity
        .linked_ids
        .get(OIDC_VERIFIED_AT_KEY)
        .and_then(|t| chrono::DateTime::parse_from_rfc3339(t).ok())
        .is_some_and(|t| now - t.with_timezone(&chrono::Utc) < chrono::Duration::days(1));
    if same && recent {
        return false;
    }
    identity
        .linked_ids
        .insert(OIDC_VERIFIED_EMAIL_KEY.to_string(), email);
    identity
        .linked_ids
        .insert(OIDC_VERIFIED_AT_KEY.to_string(), now.to_rfc3339());
    true
}
