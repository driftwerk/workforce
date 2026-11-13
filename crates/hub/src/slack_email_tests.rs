//! Slack's word on an email counts only for a full member of the bot's own
//! workspace, and only for the account still linked.

use serde_json::{json, Value};
use workforce_identity::types::{Identity, IdentityRole};

use super::*;

fn answer(user: Value) -> Value {
    json!({ "ok": true, "user": user })
}

fn full_member() -> Value {
    json!({
        "id": "U-SAM", "team_id": "T_HOME", "deleted": false, "is_bot": false,
        "is_restricted": false, "is_ultra_restricted": false,
        "profile": { "email": " Sam@Example.com " },
    })
}

fn with(change: &[(&str, Value)]) -> Value {
    let mut user = full_member();
    for (k, v) in change {
        if v.is_null() {
            user.as_object_mut().unwrap().remove(*k);
        } else {
            user[*k] = v.clone();
        }
    }
    answer(user)
}

#[test]
fn only_a_full_member_of_the_bot_s_workspace_vouches() {
    let home = Some("T_HOME");
    assert_eq!(
        judge(&answer(full_member()), home),
        SlackSays::Member("sam@example.com".into())
    );
    // A guest (single- or multi-channel), a bot, an app, a deactivated
    // account: no email, and any kept one goes.
    for (change, why) in [
        (vec![("is_restricted", json!(true))], "a guest"),
        (vec![("is_ultra_restricted", json!(true))], "a guest"),
        (vec![("is_bot", json!(true))], "a bot"),
        (vec![("is_app_user", json!(true))], "a bot"),
        (vec![("deleted", json!(true))], "deactivated"),
        // A flag Slack didn't send doesn't count as "no".
        (vec![("is_restricted", Value::Null)], "a guest"),
        (vec![("deleted", Value::Null)], "deactivated"),
        (vec![("profile", json!({}))], "no email shown"),
        (vec![("is_invited_user", json!(true))], "not joined yet"),
        (
            vec![("is_email_confirmed", json!(false))],
            "email not confirmed",
        ),
    ] {
        assert_eq!(
            judge(&with(&change), home),
            SlackSays::NotMember(why),
            "{change:?}"
        );
    }
    // Another workspace's account, Slack failing, or the bot's workspace
    // not known: nothing to go by.
    assert_eq!(
        judge(&with(&[("team_id", json!("T_OTHER"))]), home),
        SlackSays::Unknown
    );
    assert_eq!(
        judge(&with(&[("is_stranger", json!(true))]), home),
        SlackSays::Unknown
    );
    assert_eq!(
        judge(&json!({ "ok": false, "error": "user_not_found" }), home),
        SlackSays::Unknown
    );
    assert_eq!(judge(&answer(full_member()), None), SlackSays::Unknown);
}

#[test]
fn slack_s_word_applies_only_to_the_account_still_linked() {
    let mut sam = Identity::new("Sam", IdentityRole::Member).with_linked_id("slack", "U-SAM");
    let member = SlackSays::Member("sam@example.com".into());
    assert!(apply(&mut sam, "U-SAM", &member));
    assert_eq!(
        crate::verified_email::verified(&sam),
        Some((
            "sam@example.com".into(),
            crate::verified_email::Source::Slack
        ))
    );
    // Someone else's account says nothing of Sam.
    assert!(!apply(
        &mut sam,
        "U-OTHER",
        &SlackSays::NotMember("a guest")
    ));
    assert!(!apply(&mut sam, "U-SAM", &SlackSays::Unknown));
    assert!(sam.linked_ids.contains_key("slack_email"));
    // Turned into a guest (or deactivated): it goes.
    assert!(apply(&mut sam, "U-SAM", &SlackSays::NotMember("a guest")));
    assert_eq!(crate::verified_email::verified(&sam), None);
    assert!(!sam.linked_ids.contains_key("slack_email_at"));
}

/// The daily sweep asks Slack about every linked account and clears one
/// that's been deactivated.
#[tokio::test]
async fn the_sweep_clears_a_deactivated_account_s_email() {
    use wiremock::matchers::{method, path, query_param};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    use workforce_identity::store::IdentityStore;
    let server = MockServer::start().await;
    let mut gone = full_member();
    gone["id"] = json!("U-GONE");
    gone["deleted"] = json!(true);
    for (id, user) in [("U-SAM", full_member()), ("U-GONE", gone)] {
        Mock::given(method("GET"))
            .and(path("/users.info"))
            .and(query_param("user", id))
            .respond_with(ResponseTemplate::new(200).set_body_json(answer(user)))
            .mount(&server)
            .await;
    }
    let surface = SlackSurface::with_base_url("xoxb-test", server.uri());
    surface.set_home_team_id("T_HOME".into());
    let store = workforce_identity::sqlite::SqliteIdentityStore::in_memory().unwrap();
    let sam = Identity::new("Sam", IdentityRole::Member).with_linked_id("slack", "U-SAM");
    let yesterday = (chrono::Utc::now() - chrono::Duration::days(2)).to_rfc3339();
    let ex = Identity::new("Ex", IdentityRole::Member)
        .with_linked_id("slack", "U-GONE")
        .with_linked_id("slack_email", "ex@example.com")
        .with_linked_id("slack_email_at", &yesterday);
    let unlinked = Identity::new("Nobody", IdentityRole::Member);
    for who in [&sam, &ex, &unlinked] {
        store.create(who).unwrap();
    }
    assert_eq!(sweep(&store, &surface).await, 2);
    let sam = store.get_by_id(sam.id).unwrap().unwrap();
    assert_eq!(
        sam.linked_ids.get("slack_email").map(String::as_str),
        Some("sam@example.com")
    );
    let ex = store.get_by_id(ex.id).unwrap().unwrap();
    assert!(!ex.linked_ids.contains_key("slack_email"));
}
