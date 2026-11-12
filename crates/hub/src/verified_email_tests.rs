//! A verified email stands only with its link, lately, for an active
//! identity; a sign-in's comes before Slack's; an edit never writes one.

use std::collections::HashMap;

use workforce_identity::types::{Identity, IdentityRole, IdentityStatus};

use super::*;

fn vouched(source: Source, email: &str, days_ago: i64) -> Identity {
    let at = (chrono::Utc::now() - chrono::Duration::days(days_ago)).to_rfc3339();
    Identity::new("Sam", IdentityRole::Member)
        .with_linked_id(source.link(), "linked")
        .with_linked_id(source.email_key(), email)
        .with_linked_id(source.at_key(), &at)
}

#[test]
fn slack_s_email_is_stale_after_ninety_days_and_never_without_its_link() {
    let now = chrono::Utc::now();
    let fresh = vouched(Source::Slack, "Sam@Example.com", 89);
    assert_eq!(
        verified_email_state(&fresh, now),
        VerifiedEmail::Fresh("sam@example.com".into(), Source::Slack)
    );
    assert_eq!(
        verified_email_state(&vouched(Source::Slack, "sam@example.com", 91), now),
        VerifiedEmail::Stale
    );
    let mut unlinked = fresh.clone();
    unlinked.linked_ids.remove("slack");
    assert_eq!(verified_email_state(&unlinked, now), VerifiedEmail::None);
    let mut suspended = fresh.clone();
    suspended.status = IdentityStatus::Suspended;
    assert_eq!(verified_email_state(&suspended, now), VerifiedEmail::None);
}

#[test]
fn a_sign_in_s_email_comes_before_slack_s_and_a_stale_one_doesn_t_hide_slack_s() {
    let now = chrono::Utc::now();
    let mut both = vouched(Source::Slack, "sam@slack.example", 1);
    for (k, v) in vouched(Source::Oidc, "sam@corp.example", 1).linked_ids {
        both.linked_ids.insert(k, v);
    }
    assert_eq!(
        verified_email_state(&both, now),
        VerifiedEmail::Fresh("sam@corp.example".into(), Source::Oidc)
    );
    both.linked_ids.insert(
        Source::Oidc.at_key().into(),
        (now - chrono::Duration::days(100)).to_rfc3339(),
    );
    assert_eq!(
        verified_email_state(&both, now),
        VerifiedEmail::Fresh("sam@slack.example".into(), Source::Slack)
    );
}

#[test]
fn an_edit_never_sets_a_slack_email_and_unlinking_drops_it() {
    let stored = vouched(Source::Slack, "sam@example.com", 1).linked_ids;
    // Sent with the same link: the stored one stands, whatever was sent.
    let mut same: HashMap<String, String> = HashMap::from([
        ("slack".into(), "linked".into()),
        ("slack_email".into(), "someone-else@example.com".into()),
    ]);
    keep_stored_verified(&stored, &mut same);
    assert_eq!(same["slack_email"], "sam@example.com");
    assert!(same.contains_key("slack_email_at"));
    // Link removed or changed: it goes.
    for links in [
        HashMap::new(),
        HashMap::from([("slack".to_string(), "U-OTHER".to_string())]),
    ] {
        let mut sent = links;
        sent.insert("slack_email".into(), "someone-else@example.com".into());
        keep_stored_verified(&stored, &mut sent);
        assert!(!sent.keys().any(|k| is_verified_key(k)), "{sent:?}");
    }
    // None stored: an edit can't add one, even with a link.
    let mut adding: HashMap<String, String> = HashMap::from([
        ("slack".into(), "U-NEW".into()),
        ("slack_email".into(), "me@example.com".into()),
        ("slack_email_at".into(), chrono::Utc::now().to_rfc3339()),
    ]);
    keep_stored_verified(&HashMap::new(), &mut adding);
    assert_eq!(adding.len(), 1);
}
