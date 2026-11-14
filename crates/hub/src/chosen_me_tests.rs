//! Choosing who you are on a shared key: names only (never emails), only
//! your own choice, kept per Jira site or Linear workspace, and used before
//! a verified email.

use std::sync::Arc;

use parking_lot::RwLock;
use serde_json::{json, Value};
use workforce_agent::tools::registry::ToolRegistry;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole};

use super::*;
use crate::by_my_email::ByMyEmail;

/// An adapter tool answering `answer`, keeping what it was asked.
struct Fake {
    name: &'static str,
    answer: parking_lot::Mutex<Value>,
    asked: parking_lot::Mutex<Vec<Value>>,
}

impl Fake {
    fn new(name: &'static str, answer: Value) -> Arc<Self> {
        Arc::new(Self {
            name,
            answer: parking_lot::Mutex::new(answer),
            asked: Default::default(),
        })
    }
}

#[async_trait::async_trait]
impl Tool for Fake {
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
        Ok(ToolResult::success(self.answer.lock().clone(), 0))
    }
}

const KAY: &str = "8a7b6c5d-0000-4000-8000-0000000000aa";
const ANA: &str = "8a7b6c5d-0000-4000-8000-0000000000bb";

/// A Linear workspace `acme` behind a shared key: Kay (whose Linear email
/// isn't their Slack one), Ana, someone gone, and the key's own viewer.
struct Hub {
    tools: Arc<ToolRegistry>,
    viewer: Arc<Fake>,
    store: Arc<SqliteIdentityStore>,
    handle: Arc<RwLock<Option<Arc<dyn IdentityStore>>>>,
}

fn hub() -> Hub {
    let tools = Arc::new(ToolRegistry::new());
    let viewer = Fake::new(
        "linear.viewer",
        json!({ "id": "bot", "name": "Bot", "personal": false, "workspace": "acme" }),
    );
    let members = Fake::new(
        "linear.members_list",
        json!({ "members": [
            { "id": KAY, "name": "kay", "displayName": "K. Hendrickse", "email": "kay@linear.example",
              "active": true, "avatarUrl": "https://avatars.example/kay.png",
              "url": "https://linear.app/acme/profiles/kay" },
            { "id": ANA, "name": "ana", "displayName": "Ana", "email": "ana@example.com", "active": true },
            { "id": "8a7b6c5d-0000-4000-8000-0000000000cc", "name": "kayleigh", "displayName": "Kayleigh",
              "email": "kayleigh@example.com", "active": false },
        ] }),
    );
    tools.register(Arc::clone(&viewer) as Arc<dyn Tool>);
    tools.register(members as Arc<dyn Tool>);
    let store = Arc::new(SqliteIdentityStore::in_memory().unwrap());
    let handle: Arc<RwLock<Option<Arc<dyn IdentityStore>>>> = Arc::new(RwLock::new(Some(
        Arc::clone(&store) as Arc<dyn IdentityStore>,
    )));
    Hub {
        tools,
        viewer,
        store,
        handle,
    }
}

async fn as_who(who: &Identity, tool: &dyn Tool, input: Value) -> ToolResult {
    workforce_agent::runner::CURRENT_USER_ID
        .scope(who.id.to_string(), tool.execute(input))
        .await
        .unwrap()
}

#[test]
fn a_name_search_shows_names_and_avatars_only() {
    let answer = json!({ "users": [
        { "account_id": "557058:kay", "display_name": "K. Hendrickse", "email": "kay@example.com",
          "active": true, "account_type": "atlassian", "avatar": "https://a.example/k.png" },
        { "account_id": "bot", "display_name": "Hendrickse bot", "email": null, "active": true, "account_type": "app" },
        { "account_id": "557058:old", "display_name": "K. Hendrickse (old)", "active": false, "account_type": "atlassian" },
        // Found by Jira by their email, not their name: not shown.
        // (Nor a bot or someone gone, whatever their name.)
        { "account_id": "557058:ana", "display_name": "Ana", "email": "hendrickse.friend@example.com",
          "active": true, "account_type": "atlassian" },
    ] });
    let found = members_named(On::Jira, &answer, "hendr");
    assert_eq!(
        found,
        vec![
            json!({ "id": "557058:kay", "name": "K. Hendrickse", "avatar": "https://a.example/k.png" })
        ]
    );
    assert!(!serde_json::to_string(&found).unwrap().contains('@'));
    // A Gravatar picture's address is a hash of the email: left out.
    let answer = json!({ "users": [
        { "account_id": "557058:kay", "display_name": "K. Hendrickse", "active": true,
          "account_type": "atlassian",
          "avatar": "https://secure.gravatar.com/avatar/0123456789abcdef?d=identicon" },
    ] });
    assert_eq!(
        members_named(On::Jira, &answer, "hendr"),
        vec![json!({ "id": "557058:kay", "name": "K. Hendrickse", "avatar": null })]
    );
}

#[tokio::test]
async fn a_member_list_never_carries_an_email_and_needs_a_name() {
    let h = hub();
    let kay = Identity::new("Kay", IdentityRole::Member);
    h.store.create(&kay).unwrap();
    let tool = MembersNamed::new(On::Linear, &h.tools, Arc::clone(&h.handle));
    let out = as_who(&kay, &tool, json!({ "name": "hendrick" })).await;
    assert!(out.success, "{:?}", out.error);
    assert_eq!(
        out.output,
        // Its profile's address and full name too: what a description's
        // mention of them is read by.
        json!({ "members": [{ "id": KAY, "name": "K. Hendrickse", "avatar": "https://avatars.example/kay.png",
                              "url": "https://linear.app/acme/profiles/kay", "full_name": "kay" }] })
    );
    assert!(!out.output.to_string().contains('@'));
    // One letter (or none): no listing everyone.
    for name in ["", "k", " k "] {
        assert!(!as_who(&kay, &tool, json!({ "name": name })).await.success);
    }
}

/// Choose, change, clear; the choice beats a verified email, and clearing
/// falls back to it; it's kept per workspace.
#[tokio::test]
async fn a_choice_comes_before_the_email_and_clearing_it_falls_back() {
    let h = hub();
    let now = chrono::Utc::now().to_rfc3339();
    // Slack vouches for kay@slack.example: no one on Linear has it.
    let kay = Identity::new("Kay", IdentityRole::Member)
        .with_linked_id("slack", "U-KAY")
        .with_linked_id("slack_email", "kay@slack.example")
        .with_linked_id("slack_email_at", &now);
    h.store.create(&kay).unwrap();
    let who = ByMyEmail::new(On::Linear, &h.tools, Arc::clone(&h.handle));
    let choose = ChooseMe::new(On::Linear, &h.tools, Arc::clone(&h.handle));
    // The members fake answers everyone whatever it's asked: the exact
    // match by email finds no one with kay@slack.example.
    assert_eq!(
        as_who(&kay, &who, json!({})).await.output["user"],
        Value::Null
    );

    let out = as_who(&kay, &choose, json!({ "id": KAY, "name": "K. Hendrickse" })).await;
    assert!(out.success, "{:?}", out.error);
    assert_eq!(out.output["place"], "acme");
    let me = as_who(&kay, &who, json!({})).await.output;
    assert_eq!(
        me,
        json!({ "user": { "id": KAY, "name": "K. Hendrickse" }, "why": null, "source": "chosen" })
    );

    // Changed.
    assert!(
        as_who(&kay, &choose, json!({ "id": ANA, "name": "Ana" }))
            .await
            .success
    );
    assert_eq!(
        as_who(&kay, &who, json!({})).await.output["user"]["id"],
        ANA
    );

    // Another workspace: the choice in acme isn't it.
    *h.viewer.answer.lock() = json!({ "id": "bot", "personal": false, "workspace": "other" });
    assert_ne!(
        as_who(&kay, &who, json!({})).await.output["source"],
        "chosen"
    );
    *h.viewer.answer.lock() = json!({ "id": "bot", "personal": false, "workspace": "acme" });

    // Cleared: back to the email.
    assert!(
        as_who(&kay, &choose, json!({ "clear": true }))
            .await
            .success
    );
    let back = as_who(&kay, &who, json!({})).await.output;
    assert_eq!(back["user"], Value::Null);
    assert_eq!(back["source"], "slack");

    // What isn't an id there is never kept.
    let bad = as_who(&kay, &choose, json!({ "id": "x\" OR 1" })).await;
    assert!(!bad.success);
}

/// Only the caller's own choice is ever written: Ana choosing writes Ana's,
/// and an identity edit can't write or change anyone's.
#[tokio::test]
async fn another_person_s_choice_is_untouchable() {
    let h = hub();
    let kay = Identity::new("Kay", IdentityRole::Member);
    let ana = Identity::new("Ana", IdentityRole::Member);
    h.store.create(&kay).unwrap();
    h.store.create(&ana).unwrap();
    let choose = ChooseMe::new(On::Linear, &h.tools, Arc::clone(&h.handle));
    assert!(
        as_who(&kay, &choose, json!({ "id": KAY, "name": "K. Hendrickse" }))
            .await
            .success
    );
    // Ana names Kay's member as herself: it's Ana's choice, not Kay's.
    assert!(
        as_who(&ana, &choose, json!({ "id": KAY, "name": "K. Hendrickse" }))
            .await
            .success
    );
    assert!(
        as_who(&ana, &choose, json!({ "clear": true }))
            .await
            .success
    );
    let kay_now = h.store.get_by_id(kay.id).unwrap().unwrap();
    assert_eq!(
        chosen_of(&kay_now, On::Linear, "acme"),
        Some(json!({ "id": KAY, "name": "K. Hendrickse" }))
    );
    // An edit's links keep the stored choice, and can't add one.
    let key = chosen_key(On::Linear, "acme");
    let mut sent =
        std::collections::HashMap::from([(key.clone(), json!({ "id": ANA }).to_string())]);
    crate::verified_email::keep_stored_verified(&kay_now.linked_ids, &mut sent);
    assert_eq!(sent[&key], kay_now.linked_ids[&key]);
    let mut adding =
        std::collections::HashMap::from([(key.clone(), json!({ "id": ANA }).to_string())]);
    crate::verified_email::keep_stored_verified(&std::collections::HashMap::new(), &mut adding);
    assert!(adding.is_empty());
}
