//! A person's own keys, copied by their paired Desk: only theirs, only at
//! their personal tier, only by their desktop device, and audited.

use axum::body::Body;
use axum::http::Request;
use serde_json::Value;
use tower::ServiceExt as _;
use workforce_common::settings::VaultActor;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole};

use super::*;
use crate::remote_mcp::testing::{
    fixture_with_devices, issue, pair_device, pair_device_as, TokenShape,
};

const ANN_JIRA: &str = "ann-jira-secret-token-0001";
const ANN_GITHUB: &str = "ghp_ann_github_secret_0002";
const BOB_LINEAR: &str = "lin_api_bob_secret_0003";
const ORG_DATADOG: &str = "org-datadog-shared-secret-0004";
const ANN_SLACK: &str = "xoxb-ann-slack-secret-0005";
const SECRETS: &[&str] = &[ANN_JIRA, ANN_GITHUB, BOB_LINEAR, ORG_DATADOG, ANN_SLACK];

struct Hub {
    app: Router,
    state: Arc<GatewayState>,
    audit: Arc<workforce_common::persistence::SqliteStore>,
    jwt: crate::jwt::JwtConfig,
    ann: Identity,
    ann_desk: String,
    bob_desk: String,
    ann_mcp: String,
    _dir: tempfile::TempDir,
}

/// A hub holding Ann's flat Jira and named GitHub keys, her Slack bot
/// token, Bob's Linear key and a shared Datadog key; each with a desktop
/// device paired.
async fn hub(allow: bool) -> Hub {
    let dir = tempfile::tempdir().unwrap();
    let (fx, devices) = fixture_with_devices(false);
    let mut ann = Identity::new("Ann", IdentityRole::Member);
    let mut bob = Identity::new("Bob", IdentityRole::Member);
    fx.identities.create(&ann).unwrap();
    fx.identities.create(&bob).unwrap();
    let (_, ann_desk) = pair_device(&fx, &devices, &mut ann, "Ann's Mac");
    let (_, bob_desk) = pair_device(&fx, &devices, &mut bob, "Bob's Mac");
    let ann_mcp = issue(&fx, &ann, TokenShape::default());
    let vault = Arc::new(Vault::open(&dir.path().join("vault"), "test-key").unwrap());
    let a = ann.id;
    let b = bob.id;
    let me = |p: &str, v: &str, who: &Uuid| {
        vault
            .set_with_actor(
                &format!("users/{who}/{p}"),
                v,
                VaultActor::User(&who.to_string()),
            )
            .unwrap()
    };
    me("skills/jira/site", "acme.atlassian.net", &a);
    me("skills/jira/email", "ann@acme.example", &a);
    me("skills/jira/api_token", ANN_JIRA, &a);
    me("skills/github/workspaces/acme/token", ANN_GITHUB, &a);
    me("skills/github/workspaces/acme/label", "Acme", &a);
    me("slack.bot_token", ANN_SLACK, &a);
    me("skills/linear/api_key", BOB_LINEAR, &b);
    // The shared tier: the org's, never a person's to copy.
    vault.set("skills/datadog/api_key", ORG_DATADOG).unwrap();
    vault.set("skills/datadog/app_key", ORG_DATADOG).unwrap();
    let jwt =
        crate::jwt::JwtConfig::new("a-test-secret-of-at-least-32-bytes!!".into(), 3600).unwrap();
    let audit = Arc::new(workforce_common::persistence::SqliteStore::in_memory().unwrap());
    let state = Arc::new(GatewayState {
        jwt_config: Some(jwt.clone()),
        vault: Some(Arc::clone(&vault)),
        telemetry_store: Some(Arc::clone(&audit) as _),
        ..Arc::try_unwrap(fx.state)
            .ok()
            .expect("the fixture's only handle")
    });
    state.desk_key_export.set(allow);
    Hub {
        app: router(Arc::clone(&state)),
        state,
        audit,
        jwt,
        ann,
        ann_desk,
        bob_desk,
        ann_mcp,
        _dir: dir,
    }
}

impl Hub {
    async fn ask(
        &self,
        method: &str,
        uri: &str,
        bearer: Option<&str>,
        body: Option<Value>,
    ) -> (StatusCode, HeaderMap, Value, String) {
        let mut req = Request::builder().method(method).uri(uri);
        if let Some(b) = bearer {
            req = req.header("authorization", format!("Bearer {b}"));
        }
        let body = match body {
            Some(v) => {
                req = req.header("content-type", "application/json");
                Body::from(v.to_string())
            }
            None => Body::empty(),
        };
        let res = self
            .app
            .clone()
            .oneshot(req.body(body).unwrap())
            .await
            .unwrap();
        let status = res.status();
        let headers = res.headers().clone();
        let bytes = axum::body::to_bytes(res.into_body(), 1 << 20)
            .await
            .unwrap();
        let text = String::from_utf8_lossy(&bytes).to_string();
        (
            status,
            headers,
            serde_json::from_str(&text).unwrap_or(Value::Null),
            text,
        )
    }

    async fn list(&self, bearer: &str) -> (StatusCode, Value, String) {
        let (s, _, v, t) = self.ask("GET", LIST_PATH, Some(bearer), None).await;
        (s, v, t)
    }

    async fn take(
        &self,
        bearer: &str,
        picks: &[(&str, &str)],
    ) -> (StatusCode, HeaderMap, Value, String) {
        let connections: Vec<Value> = picks
            .iter()
            .map(|(c, k)| json!({ "connector": c, "key": k }))
            .collect();
        self.ask(
            "POST",
            TAKE_PATH,
            Some(bearer),
            Some(json!({ "connections": connections })),
        )
        .await
    }

    fn sign_in(&self, who: &Identity) -> String {
        crate::jwt::generate_token_with_identity(
            &self.jwt,
            &who.display_name,
            who.role,
            Some(who.id.to_string()),
        )
        .unwrap()
    }

    /// The `desk.key_copy` audit rows' details.
    fn copies_audited(&self) -> Vec<Value> {
        self.audit
            .recent_audit(100)
            .unwrap()
            .into_iter()
            .filter(|e| e.action == "desk.key_copy")
            .map(|e| serde_json::from_str(e.detail.as_deref().unwrap_or("{}")).unwrap())
            .collect()
    }
}

fn no_secret_in(text: &str) {
    for s in SECRETS {
        assert!(!text.contains(s), "a secret came back: {text}");
        assert!(
            !text.contains(&s[..8]),
            "a secret's prefix came back: {text}"
        );
    }
}

fn listed_keys(v: &Value) -> Vec<(String, String)> {
    v["connections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["connector"].as_str().unwrap().to_string(),
                c["key"].as_str().unwrap().to_string(),
            )
        })
        .collect()
}

#[tokio::test]
async fn ann_lists_her_own_personal_connections_and_never_a_value() {
    let hub = hub(true).await;
    let (status, v, text) = hub.list(&hub.ann_desk).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    no_secret_in(&text);
    assert_eq!(
        listed_keys(&v),
        [
            ("github".to_string(), "acme".to_string()),
            ("jira".to_string(), "default".to_string()),
        ],
        "her own Jira and GitHub only: not Slack's bot token, not the org's Datadog, not Bob's Linear"
    );
    let jira = &v["connections"][1];
    assert_eq!(jira["name"], "Jira");
    assert_eq!(jira["site"], "acme.atlassian.net");
    assert_eq!(jira["account"], "ann@acme.example");
    assert!(jira["updatedAt"].is_string());
    assert_eq!(v["connections"][0]["name"], "Acme");
    assert_eq!(v["device"], "Ann's Mac");
}

#[tokio::test]
async fn ann_takes_her_keys_unstored_and_each_copy_is_audited_without_its_value() {
    let hub = hub(true).await;
    let (status, headers, v, text) = hub
        .take(
            &hub.ann_desk,
            &[("jira", "default"), ("github", "acme"), ("jira", "default")],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(headers.get(header::CACHE_CONTROL).unwrap(), "no-store");
    let copies = v["copies"].as_array().unwrap();
    assert_eq!(copies.len(), 2, "one each, the repeat dropped: {v}");
    assert_eq!(copies[0]["fields"]["api_token"], ANN_JIRA);
    assert_eq!(copies[0]["fields"]["site"], "acme.atlassian.net");
    assert_eq!(copies[0]["fields"]["email"], "ann@acme.example");
    assert_eq!(copies[1]["fields"]["token"], ANN_GITHUB);
    assert_eq!(v["missing"], json!([]));

    let rows = hub.copies_audited();
    assert_eq!(rows.len(), 2, "{rows:?}");
    let said: Vec<&str> = rows
        .iter()
        .map(|r| r["summary"].as_str().unwrap())
        .collect();
    assert!(
        said.contains(&"Desk on Ann's Mac copied your Jira key"),
        "{said:?}"
    );
    assert!(
        said.contains(&"Desk on Ann's Mac copied your GitHub key (Acme)"),
        "{said:?}"
    );
    for r in &rows {
        assert_eq!(r["identity_id"], hub.ann.id.to_string());
        no_secret_in(&r.to_string());
    }
}

#[tokio::test]
async fn bob_can_never_list_or_take_anns_keys() {
    let hub = hub(true).await;
    let (status, v, text) = hub.list(&hub.bob_desk).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        listed_keys(&v),
        [("linear".to_string(), "default".to_string())]
    );
    assert!(!text.contains("acme"), "{text}");

    let (status, _, v, text) = hub
        .take(&hub.bob_desk, &[("jira", "default"), ("github", "acme")])
        .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["copies"], json!([]), "{text}");
    assert_eq!(v["missing"].as_array().unwrap().len(), 2);
    no_secret_in(&text);
    // Whatever names a person: the key is a connection's, never a path.
    for key in [
        format!("../{}", hub.ann.id),
        format!("users/{}/skills/jira", hub.ann.id),
        hub.ann.id.to_string().to_uppercase(),
    ] {
        let (status, _, _, text) = hub.take(&hub.bob_desk, &[("jira", &key)]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{key}: {text}");
        no_secret_in(&text);
    }
    assert!(hub.copies_audited().is_empty());
}

#[tokio::test]
async fn shared_and_sign_in_keys_are_never_copied() {
    let hub = hub(true).await;
    // The org's Datadog is in the shared tier alone: nothing of Ann's.
    let (status, _, v, text) = hub.take(&hub.ann_desk, &[("datadog", "default")]).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(v["copies"], json!([]));
    no_secret_in(&text);
    // Slack's bot token is shared by nature; Google's is this hub's client's.
    for connector in ["slack", "google", "notion", ""] {
        let (status, _, _, text) = hub.take(&hub.ann_desk, &[(connector, "default")]).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{connector}: {text}");
        no_secret_in(&text);
    }
    let too_many: Vec<(&str, &str)> = (0..=MAX_TAKE).map(|_| ("jira", "default")).collect();
    let (status, _, _, _) = hub.take(&hub.ann_desk, &too_many).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    let (status, _, _, _) = hub.take(&hub.ann_desk, &[]).await;
    assert_eq!(status, StatusCode::BAD_REQUEST);
    assert!(hub.copies_audited().is_empty());
}

#[tokio::test]
async fn only_the_persons_desktop_device_is_taken_never_a_sign_in_token_or_agent() {
    let hub = hub(true).await;
    let admin = Identity::new("Root", IdentityRole::SystemAdmin);
    hub.state
        .identity_store
        .as_ref()
        .unwrap()
        .create(&admin)
        .unwrap();
    let admin = hub.sign_in(&admin);
    let ann_sign_in = hub.sign_in(&hub.ann);
    let refused = [
        ("an admin's sign-in", Some(admin.as_str())),
        ("Ann's own sign-in", Some(ann_sign_in.as_str())),
        ("the hub's own token", Some(hub.state.auth_token.as_str())),
        ("Ann's remote MCP token", Some(hub.ann_mcp.as_str())),
        ("an unknown token", Some("wfp_not-a-device")),
        ("no one", None),
    ];
    for (who, bearer) in refused {
        let (status, _, _, text) = hub.ask("GET", LIST_PATH, bearer, None).await;
        assert!(
            matches!(status, StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED),
            "{who} lists: {status} {text}"
        );
        let (status, _, _, text) = hub
            .ask(
                "POST",
                TAKE_PATH,
                bearer,
                Some(json!({"connections": [{"connector": "jira", "key": "default"}]})),
            )
            .await;
        assert!(
            matches!(status, StatusCode::FORBIDDEN | StatusCode::UNAUTHORIZED),
            "{who} takes: {status} {text}"
        );
        no_secret_in(&text);
    }
    assert!(hub.copies_audited().is_empty());
}

#[tokio::test]
async fn a_phone_paired_to_ann_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let (fx, devices) = fixture_with_devices(false);
    let mut ann = Identity::new("Ann", IdentityRole::Member);
    fx.identities.create(&ann).unwrap();
    let (_, phone) = pair_device_as(&fx, &devices, &mut ann, "Ann's phone", "ios");
    let vault = Arc::new(Vault::open(&dir.path().join("vault"), "test-key").unwrap());
    vault
        .set(&format!("users/{}/skills/jira/api_token", ann.id), ANN_JIRA)
        .unwrap();
    let state = Arc::new(GatewayState {
        vault: Some(vault),
        ..Arc::try_unwrap(fx.state).ok().unwrap()
    });
    let app = router(state);
    let req = Request::builder()
        .method("GET")
        .uri(LIST_PATH)
        .header("authorization", format!("Bearer {phone}"))
        .body(Body::empty())
        .unwrap();
    let res = app.oneshot(req).await.unwrap();
    assert_eq!(res.status(), StatusCode::FORBIDDEN);
}

#[tokio::test]
async fn with_the_admins_setting_off_both_routes_refuse() {
    let hub = hub(false).await;
    let (status, _, text) = hub.list(&hub.ann_desk).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    assert!(text.contains("turned off"), "{text}");
    let (status, _, _, text) = hub.take(&hub.ann_desk, &[("jira", "default")]).await;
    assert_eq!(status, StatusCode::FORBIDDEN, "{text}");
    no_secret_in(&text);
    assert!(hub.copies_audited().is_empty());
    // Turned on again, as the overlay watcher would: no restart.
    hub.state.desk_key_export.set(true);
    let (status, _, text) = hub.list(&hub.ann_desk).await;
    assert_eq!(status, StatusCode::OK, "{text}");
}

#[tokio::test]
async fn takes_are_rate_limited_per_person() {
    let hub = hub(true).await;
    for _ in 0..10 {
        let (status, _, _, _) = hub.take(&hub.ann_desk, &[("github", "acme")]).await;
        assert_eq!(status, StatusCode::OK);
    }
    let (status, headers, _, text) = hub.take(&hub.ann_desk, &[("github", "acme")]).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{text}");
    assert!(headers.get(header::RETRY_AFTER).is_some());
    no_secret_in(&text);
    // Bob's are his own.
    let (status, _, _, _) = hub.take(&hub.bob_desk, &[("linear", "default")]).await;
    assert_eq!(status, StatusCode::OK);
}

#[test]
fn an_instance_named_default_is_preferred_over_the_flat_keys() {
    let dir = tempfile::tempdir().unwrap();
    let vault = Vault::open(&dir.path().join("vault"), "test-key").unwrap();
    let ann = Uuid::new_v4();
    vault
        .set(&format!("users/{ann}/skills/linear/api_key"), "flat")
        .unwrap();
    vault
        .set(
            &format!("users/{ann}/skills/linear/workspaces/default/api_key"),
            "named",
        )
        .unwrap();
    let listed = connections_of(&vault, &ann);
    assert_eq!(listed.len(), 1, "{listed:?}");
    let c = copyable("linear").unwrap();
    let taken = take_one(&vault, c, &ann, DEFAULT_INSTANCE).unwrap();
    assert_eq!(taken.fields["api_key"], "named");
}

#[test]
fn the_summary_names_the_device_and_the_connection() {
    let l = Listed {
        connector: "jira",
        service: "Jira",
        key: "acme".into(),
        name: "Acme".into(),
        site: None,
        account: None,
        updated_at: None,
    };
    assert_eq!(
        summary("Sam's Mac", &l),
        "Desk on Sam's Mac copied your Jira key (Acme)"
    );
    assert_eq!(
        summary(" ", &l),
        "Desk on a computer copied your Jira key (Acme)"
    );
}
