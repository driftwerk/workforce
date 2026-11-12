//! The by-email tools match on the hub: only the caller's own fresh verified
//! email, only exact, active users (on Jira, atlassian accounts), and never
//! the directory.

use std::sync::Arc;

use parking_lot::RwLock;
use serde_json::{json, Value};
use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole, IdentityStatus};

use super::*;

/// An adapter lookup that answers `answer` whatever it's asked, and keeps
/// what it was asked.
struct Lookup {
    name: &'static str,
    answer: Value,
    asked: parking_lot::Mutex<Vec<Value>>,
}

#[async_trait::async_trait]
impl Tool for Lookup {
    fn name(&self) -> &str {
        self.name
    }
    fn description(&self) -> &str {
        "fixture"
    }
    fn input_schema(&self) -> Value {
        json!({ "type": "object" })
    }
    async fn execute(&self, input: Value) -> eyre::Result<ToolResult> {
        self.asked.lock().push(input);
        Ok(ToolResult::success(self.answer.clone(), 0))
    }
}

fn member(id: &str, name: &str, email: Option<&str>, active: Option<bool>) -> Value {
    let mut v = json!({ "id": id, "name": name, "email": email });
    if let Some(a) = active {
        v["active"] = json!(a);
    }
    v
}

fn jira_user(id: &str, email: Option<&str>, active: bool, kind: &str) -> Value {
    json!({ "account_id": id, "display_name": format!("User {id}"), "email": email, "active": active, "account_type": kind })
}

#[test]
fn only_one_exact_active_member_is_anyone() {
    let email = "sam@example.com";
    // Others the search brings back (a longer address, someone inactive,
    // someone whose `active` isn't said) are never taken for them.
    let answer = json!({ "members": [
        member("m-1", "Sam", Some("SAM@example.com"), Some(true)),
        member("m-2", "Sam B", Some("sam@example.com.au"), Some(true)),
        member("m-3", "Old Sam", Some("sam@example.com"), Some(false)),
        member("m-4", "Unsaid", Some("sam@example.com"), None),
    ]});
    assert_eq!(
        pick(On::Linear, &answer, email),
        json!({ "user": { "id": "m-1", "name": "Sam" }, "why": null })
    );
    let two = json!({ "members": [
        member("m-1", "Sam", Some("sam@example.com"), Some(true)),
        member("m-5", "Sam Too", Some("Sam@Example.com"), Some(true)),
    ]});
    assert_eq!(pick(On::Linear, &two, email)["why"], "several");
    assert_eq!(
        pick(On::Linear, &json!({ "members": [] }), email)["why"],
        "none"
    );
}

#[test]
fn a_jira_user_is_an_active_atlassian_account_with_exactly_that_email() {
    let email = "sam@example.com";
    let answer = json!({ "count": 4, "users": [
        jira_user("acc-1", Some("sam@example.com"), true, "atlassian"),
        jira_user("app-1", Some("sam@example.com"), true, "app"),
        jira_user("acc-2", Some("sam@example.com"), false, "atlassian"),
        jira_user("acc-3", Some("samuel@example.com"), true, "atlassian"),
    ]});
    assert_eq!(
        pick(On::Jira, &answer, email),
        json!({ "user": { "id": "acc-1", "name": "User acc-1" }, "why": null })
    );
    // Jira hides the email from the key: said as such.
    let hidden = json!({ "users": [jira_user("acc-1", None, true, "atlassian")] });
    assert_eq!(pick(On::Jira, &hidden, email)["why"], "email_hidden");
    // Only inactive or non-people: no one, not "hidden".
    let none_left = json!({ "users": [jira_user("acc-2", None, false, "atlassian"), jira_user("app", None, true, "app")] });
    assert_eq!(pick(On::Jira, &none_left, email)["why"], "none");
}

fn store_with(identity: &Identity) -> Arc<RwLock<Option<Arc<dyn IdentityStore>>>> {
    let store = SqliteIdentityStore::in_memory().unwrap();
    store.create(identity).unwrap();
    Arc::new(RwLock::new(Some(Arc::new(store) as Arc<dyn IdentityStore>)))
}

async fn ask(tool: &ByMyEmail, who: &Identity) -> Value {
    workforce_agent::runner::CURRENT_USER_ID
        .scope(who.id.to_string(), tool.execute(json!({})))
        .await
        .unwrap()
        .output
}

/// The tool looks up only the caller's own fresh verified email; stale,
/// none, or a suspended caller: no lookup at all.
#[tokio::test]
async fn it_asks_only_by_the_caller_s_own_fresh_verified_email() {
    let tools = Arc::new(ToolRegistry::new());
    let lookup = Arc::new(Lookup {
        name: "linear.members_list",
        answer: json!({ "members": [member("m-1", "Sam", Some("sam@example.com"), Some(true))] }),
        asked: Default::default(),
    });
    tools.register(Arc::clone(&lookup) as Arc<dyn Tool>);
    let mut sam = Identity::new("Sam", IdentityRole::Member);
    sam.email = Some("typed@example.com".into());
    let fresh = chrono::Utc::now().to_rfc3339();
    sam.linked_ids.insert(
        crate::oidc::OIDC_VERIFIED_EMAIL_KEY.into(),
        "sam@example.com".into(),
    );
    sam.linked_ids
        .insert(crate::oidc::OIDC_VERIFIED_AT_KEY.into(), fresh);
    let tool = ByMyEmail::new(On::Linear, &tools, store_with(&sam));
    assert_eq!(ask(&tool, &sam).await["user"]["id"], "m-1");
    assert_eq!(
        *lookup.asked.lock(),
        vec![json!({ "email": "sam@example.com", "limit": 10 })]
    );
    // Verified long ago: renewed only by signing in.
    let mut old = sam.clone();
    old.linked_ids.insert(
        crate::oidc::OIDC_VERIFIED_AT_KEY.into(),
        (chrono::Utc::now() - chrono::Duration::days(91)).to_rfc3339(),
    );
    let tool = ByMyEmail::new(On::Linear, &tools, store_with(&old));
    assert_eq!(
        ask(&tool, &old).await,
        json!({ "user": null, "why": "stale" })
    );
    // Only an email someone typed: none verified.
    let mut typed = Identity::new("Typed", IdentityRole::Member);
    typed.email = Some("sam@example.com".into());
    let tool = ByMyEmail::new(On::Linear, &tools, store_with(&typed));
    assert_eq!(ask(&tool, &typed).await["why"], "no_verified_email");
    // Suspended: their verified email vouches for no one.
    let mut gone = sam.clone();
    gone.status = IdentityStatus::Suspended;
    let tool = ByMyEmail::new(On::Linear, &tools, store_with(&gone));
    assert_eq!(ask(&tool, &gone).await["why"], "no_verified_email");
    assert_eq!(lookup.asked.lock().len(), 1, "looked up once only");
}
