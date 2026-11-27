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
/// A token GitHub's device flow gave Ann's sign-in on this hub's OAuth app.
const ANN_GITHUB_SIGNED_IN: &str = "gho_ann_signed_in_secret_0006";
const ALLAN_ANN_LINEAR: &str = "t7allan-ann-linear-0007";
const ALLAN_ANN_METABASE: &str = "t8allan-ann-metabase-0008";
const ALLAN_ANN_JIRA: &str = "t9allan-ann-jira-0009";
const ALLAN_BOB_EMAIL: &str = "t10allan-bob-mail-0010";
const ALLAN_SHARED: &str = "t11allan-shared-mb-0011";
const ALLAN_ANN_SIGNED_IN: &str = "gho_t12allan-signed-0012";
const CLOSED_ANN_LINEAR: &str = "t13closed-ann-linear-0013";
const ALLAN_ANN_GOOGLE: &str = "t14allan-google-0014";
const SECRETS: &[&str] = &[
    ALLAN_ANN_LINEAR,
    ALLAN_ANN_METABASE,
    ALLAN_ANN_JIRA,
    ALLAN_BOB_EMAIL,
    ALLAN_SHARED,
    ALLAN_ANN_SIGNED_IN,
    CLOSED_ANN_LINEAR,
    ALLAN_ANN_GOOGLE,
    ANN_JIRA,
    ANN_GITHUB,
    BOB_LINEAR,
    ORG_DATADOG,
    ANN_SLACK,
    ANN_GITHUB_SIGNED_IN,
];

struct Hub {
    app: axum::Router,
    state: Arc<GatewayState>,
    audit: Arc<workforce_common::persistence::SqliteStore>,
    jwt: crate::jwt::JwtConfig,
    ann: Identity,
    bob: Identity,
    ann_desk: String,
    bob_desk: String,
    ann_mcp: String,
    _dir: tempfile::TempDir,
}

/// A hub holding Ann's flat Jira and named GitHub keys, a GitHub sign-in
/// of hers, her Slack bot token, Bob's Linear key and a shared Datadog key;
/// each with a desktop device paired.
async fn hub(allow: bool) -> Hub {
    hub_on(allow, None).await
}

/// [`hub`], its vault `global` when given.
async fn hub_on(allow: bool, global: Option<Arc<Vault>>) -> Hub {
    let dir = tempfile::tempdir().unwrap();
    let (fx, devices) = fixture_with_devices(false);
    let mut ann = Identity::new("Ann", IdentityRole::Member);
    let mut bob = Identity::new("Bob", IdentityRole::Member);
    fx.identities.create(&ann).unwrap();
    fx.identities.create(&bob).unwrap();
    let (_, ann_desk) = pair_device(&fx, &devices, &mut ann, "Ann's Mac");
    let (_, bob_desk) = pair_device(&fx, &devices, &mut bob, "Bob's Mac");
    let ann_mcp = issue(&fx, &ann, TokenShape::default());
    let vault = global
        .unwrap_or_else(|| Arc::new(Vault::open(&dir.path().join("vault"), "test-key").unwrap()));
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
    me(
        "skills/github/workspaces/work/token",
        ANN_GITHUB_SIGNED_IN,
        &a,
    );
    me("skills/github/workspaces/work/label", "Work", &a);
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
        app: crate::serve::combined(router(Arc::clone(&state))),
        state,
        audit,
        jwt,
        ann,
        bob,
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

    /// [`Hub::take`], each pick through the agent given (or none).
    async fn take_from(
        &self,
        bearer: &str,
        picks: &[(&str, &str, Option<&str>)],
    ) -> (StatusCode, Value, String) {
        let connections: Vec<Value> = picks
            .iter()
            .map(|(c, k, a)| match a {
                Some(a) => json!({ "connector": c, "key": k, "agent": a }),
                None => json!({ "connector": c, "key": k }),
            })
            .collect();
        let (s, _, v, t) = self
            .ask(
                "POST",
                TAKE_PATH,
                Some(bearer),
                Some(json!({ "connections": connections })),
            )
            .await;
        (s, v, t)
    }

    /// The `desk.key_copy` audit rows' targets.
    fn copy_targets(&self) -> Vec<String> {
        self.audit
            .recent_audit(100)
            .unwrap()
            .into_iter()
            .filter(|e| e.action == "desk.key_copy")
            .map(|e| e.resource)
            .collect()
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
        "her own Jira and GitHub token only: not her GitHub sign-in, Slack's bot token, the org's Datadog or Bob's Linear"
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
        said.contains(&"Your Jira key was handed over to Desk on Ann's Mac"),
        "{said:?}"
    );
    assert!(
        said.contains(&"Your GitHub key (Acme) was handed over to Desk on Ann's Mac"),
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

/// A GitHub token from a sign-in on this hub's OAuth app is never listed
/// nor handed over, even asked for by name beside a personal access token:
/// signing it out here would leave a copy on Desk working.
#[tokio::test]
async fn a_github_sign_ins_token_is_never_copied_but_a_personal_access_token_is() {
    let hub = hub(true).await;
    let (_, v, _) = hub.list(&hub.ann_desk).await;
    assert!(
        !listed_keys(&v).contains(&("github".to_string(), "work".to_string())),
        "{v}"
    );
    for picks in [
        &[("github", "work")][..],
        &[("github", "acme"), ("github", "work")][..],
    ] {
        let (status, _, v, text) = hub.take(&hub.ann_desk, picks).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{picks:?}: {text}");
        assert!(v.get("copies").is_none(), "nothing handed over: {text}");
        assert!(text.contains("sign-in"), "{text}");
        no_secret_in(&text);
    }
    assert!(hub.copies_audited().is_empty());

    // Kept flat too, a space before it or not.
    let dir = tempfile::tempdir().unwrap();
    let vault = Arc::new(Vault::open(&dir.path().join("vault"), "test-key").unwrap());
    let mine = [Source::personal(Arc::clone(&vault))];
    let ann = Uuid::new_v4();
    let github = copyable("github").unwrap();
    for (token, copied) in [
        (" gho_flat_sign_in_token", false),
        ("ghu_app_user_token", false),
        ("ghp_personal_access_token", true),
        ("github_pat_fine_grained_token", true),
    ] {
        vault
            .set(&format!("users/{ann}/skills/github/token"), token)
            .unwrap();
        assert_eq!(
            connections_of(&mine, &ann).len(),
            usize::from(copied),
            "{token}"
        );
        assert_eq!(
            take_one(&mine[0], github, &ann, DEFAULT_INSTANCE).is_some(),
            copied,
            "{token}"
        );
    }
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
    let app = crate::serve::combined(router(state));
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
    let vault = Arc::new(Vault::open(&dir.path().join("vault"), "test-key").unwrap());
    let mine = [Source::personal(Arc::clone(&vault))];
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
    let listed = connections_of(&mine, &ann);
    assert_eq!(listed.len(), 1, "{listed:?}");
    let c = copyable("linear").unwrap();
    let taken = take_one(&mine[0], c, &ann, DEFAULT_INSTANCE).unwrap();
    assert_eq!(taken.fields["api_key"], "named");
}

#[test]
fn the_summary_says_handed_over_naming_the_device_and_the_connection() {
    let l = Listed {
        connector: "jira",
        service: "Jira",
        key: "acme".into(),
        name: "Acme".into(),
        site: None,
        account: None,
        updated_at: None,
        tier: "global_per_user",
        agent: None,
        agent_name: None,
    };
    assert_eq!(
        summary("Sam's Mac", &l),
        "Your Jira key (Acme) was handed over to Desk on Sam's Mac"
    );
    assert_eq!(
        summary(" ", &l),
        "Your Jira key (Acme) was handed over to Desk on a computer"
    );
}

// ── Through an agent: the agent-per-user tier ─────────────────────────────

/// An agent named `name` with a vault of its own, on `dir`.
async fn agent_with_vault(dir: &std::path::Path, name: &str) -> Arc<crate::serve::AgentService> {
    let mut config = workforce_common::settings::WorkforceConfig::default();
    config.hub.data_dir = dir.to_string_lossy().into_owned();
    config.memory.db_path = dir.join("memory.db").to_string_lossy().into_owned();
    config.llm.api_key = Some("test-key".into());
    config.vault.enabled = true;
    use crate::serve::compose::Compose as _;
    let service = crate::serve::AgentService::from_config(config, None)
        .await
        .unwrap();
    *service.name_override.write() = Some(name.into());
    assert!(service.vault.is_some(), "an agent with a vault of its own");
    service
}

/// Closes agent `id` to everyone but a stranger.
fn close(state: &GatewayState, id: &str) {
    use workforce_common::settings::{AccessMode, AgentAccessConfig};
    state.access_gate.upsert(
        id.into(),
        AgentAccessConfig {
            mode: AccessMode::Allowlist,
            allow: vec![Uuid::new_v4().to_string()],
            ..Default::default()
        },
    );
}

/// [`hub`], with agent Allan holding Ann's two Linear workspaces, her
/// Metabase, a second copy of her personal Jira, a GitHub sign-in and a
/// Google sign-in of hers, Bob's Email and a key shared by all who use
/// Allan; and agent `closed`, which Ann can't use, holding her Linear key.
async fn hub_with_agents() -> (Hub, tempfile::TempDir) {
    let hub = hub(true).await;
    let dir = tempfile::tempdir().unwrap();
    let allan = agent_with_vault(&dir.path().join("allan"), "Allan").await;
    let closed = agent_with_vault(&dir.path().join("closed"), "Closed").await;
    let (a, b) = (hub.ann.id, hub.bob.id);
    let put = |v: &Vault, p: String, val: &str| v.set(&p, val).unwrap();
    let av = allan.vault.as_ref().unwrap();
    for (key, secret) in [
        ("driftwerk", ALLAN_ANN_LINEAR),
        ("mantiscrm", ALLAN_ANN_LINEAR),
    ] {
        put(
            av,
            format!("users/{a}/skills/linear/workspaces/{key}/api_key"),
            secret,
        );
        put(
            av,
            format!("users/{a}/skills/linear/workspaces/{key}/label"),
            key,
        );
    }
    put(
        av,
        format!("users/{a}/skills/metabase/api_key"),
        ALLAN_ANN_METABASE,
    );
    put(
        av,
        format!("users/{a}/skills/metabase/site_url"),
        "https://mb.driftwerk.example",
    );
    // The same Jira as her personal one: the personal one is offered.
    put(
        av,
        format!("users/{a}/skills/jira/site"),
        "https://ACME.atlassian.net/",
    );
    put(
        av,
        format!("users/{a}/skills/jira/email"),
        "ann@acme.example",
    );
    put(
        av,
        format!("users/{a}/skills/jira/api_token"),
        ALLAN_ANN_JIRA,
    );
    put(
        av,
        format!("users/{a}/skills/github/workspaces/signed/token"),
        ALLAN_ANN_SIGNED_IN,
    );
    put(
        av,
        format!("users/{a}/skills/google/workspaces/default/refresh_token"),
        ALLAN_ANN_GOOGLE,
    );
    put(
        av,
        format!("users/{b}/skills/email/accounts/default/password"),
        ALLAN_BOB_EMAIL,
    );
    put(
        av,
        format!("users/{b}/skills/email/accounts/default/username"),
        "bob@acme.example",
    );
    // Allan's agent-specific tier: everyone's who uses Allan.
    put(
        av,
        "skills/metabase/instances/team/api_key".into(),
        ALLAN_SHARED,
    );
    put(
        av,
        "skills/metabase/instances/team/site_url".into(),
        "https://team.example",
    );
    put(
        closed.vault.as_ref().unwrap(),
        format!("users/{a}/skills/linear/api_key"),
        CLOSED_ANN_LINEAR,
    );
    hub.state.registry.insert("allan", allan);
    hub.state.registry.insert("closed", closed);
    close(&hub.state, "closed");
    (hub, dir)
}

/// Each connection listed: (connector, key, agent).
fn listed_from(v: &Value) -> Vec<(String, String, Option<String>)> {
    v["connections"]
        .as_array()
        .unwrap()
        .iter()
        .map(|c| {
            (
                c["connector"].as_str().unwrap().to_string(),
                c["key"].as_str().unwrap().to_string(),
                c["agent"].as_str().map(str::to_string),
            )
        })
        .collect()
}

fn row(c: &str, k: &str, a: Option<&str>) -> (String, String, Option<String>) {
    (c.into(), k.into(), a.map(str::to_string))
}

const WITH_AGENTS: &str = "/api/me/desk-key-copies?with=agents";

#[tokio::test]
async fn ann_lists_her_keys_through_the_agents_she_uses_and_never_a_shared_or_anothers() {
    let (hub, _dir) = hub_with_agents().await;
    let (status, _, v, text) = hub.ask("GET", WITH_AGENTS, Some(&hub.ann_desk), None).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    no_secret_in(&text);
    assert_eq!(
        listed_from(&v),
        [
            row("github", "acme", None),
            row("jira", "default", None),
            row("linear", "driftwerk", Some("allan")),
            row("linear", "mantiscrm", Some("allan")),
            row("metabase", "default", Some("allan")),
        ],
        "not Allan's copy of her Jira, nor her sign-ins, Bob's Email, Allan's shared \
         Metabase, or her Linear through an agent she can't use"
    );
    let linear = &v["connections"][2];
    assert_eq!(linear["tier"], "agent_per_user");
    assert_eq!(linear["agentName"], "Allan");
    assert_eq!(linear["name"], "driftwerk");
    assert_eq!(v["connections"][1]["tier"], "global_per_user");
    assert_eq!(v["connections"][1]["agent"], Value::Null);
    assert_eq!(
        v["signInOnly"],
        json!([{ "connector": "google", "service": "Google" }])
    );
    assert!(!text.contains("team.example"), "{text}");

    // A Desk from before asks for none through agents: only her personal tier.
    let (_, v, _) = hub.list(&hub.ann_desk).await;
    assert!(listed_from(&v).iter().all(|(_, _, a)| a.is_none()), "{v}");
}

#[tokio::test]
async fn a_key_through_an_agent_is_taken_from_that_tier_only_and_audited_naming_it() {
    let (hub, _dir) = hub_with_agents().await;
    let (status, v, text) = hub
        .take_from(
            &hub.ann_desk,
            &[
                ("linear", "driftwerk", Some("allan")),
                ("jira", "default", Some("allan")),
                ("jira", "default", None),
                // Not in her personal tier: never served from Allan's.
                ("linear", "mantiscrm", None),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    let copies = v["copies"].as_array().unwrap();
    assert_eq!(copies.len(), 3, "{v}");
    assert_eq!(copies[0]["fields"]["api_key"], ALLAN_ANN_LINEAR);
    assert_eq!(copies[0]["agent"], "allan");
    assert_eq!(copies[1]["fields"]["api_token"], ALLAN_ANN_JIRA);
    assert_eq!(copies[2]["fields"]["api_token"], ANN_JIRA);
    assert_eq!(
        v["missing"],
        json!([{ "connector": "linear", "key": "mantiscrm" }])
    );

    let rows = hub.copies_audited();
    assert_eq!(rows.len(), 3, "{rows:?}");
    let linear = rows.iter().find(|r| r["connector"] == "linear").unwrap();
    assert_eq!(linear["tier"], "agent_per_user");
    assert_eq!(linear["agent"], "allan");
    assert_eq!(
        linear["summary"],
        "Your Linear key (driftwerk), through Allan, was handed over to Desk on Ann's Mac"
    );
    assert!(rows
        .iter()
        .any(|r| r["tier"] == "global_per_user" && r["agent"].is_null()));
    let targets = hub.copy_targets();
    assert!(
        targets.contains(&"desk_key_copy:linear/driftwerk@agent:allan".to_string()),
        "{targets:?}"
    );
    assert!(
        targets.contains(&"desk_key_copy:jira/default".to_string()),
        "{targets:?}"
    );
    for r in &rows {
        no_secret_in(&r.to_string());
    }
}

#[tokio::test]
async fn an_agent_she_cant_use_or_that_isnt_here_hands_over_nothing() {
    let (hub, _dir) = hub_with_agents().await;
    let mut said = Vec::new();
    for agent in ["closed", "nobody"] {
        let (status, v, text) = hub
            .take_from(
                &hub.ann_desk,
                &[
                    ("jira", "default", None),
                    ("linear", "default", Some(agent)),
                ],
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{agent}: {text}");
        assert!(v.get("copies").is_none(), "{text}");
        no_secret_in(&text);
        said.push(v["error"].clone());
    }
    assert_eq!(
        said[0], said[1],
        "the same words, so no agent is told apart"
    );
    assert!(hub.copies_audited().is_empty());
}

#[tokio::test]
async fn bob_gets_his_own_through_allan_and_never_anns() {
    let (hub, _dir) = hub_with_agents().await;
    let (_, _, v, text) = hub.ask("GET", WITH_AGENTS, Some(&hub.bob_desk), None).await;
    assert_eq!(
        listed_from(&v),
        [
            row("email", "default", Some("allan")),
            row("linear", "default", None)
        ],
        "{text}"
    );
    assert_eq!(v["signInOnly"], json!([]));
    no_secret_in(&text);
    let (status, v, text) = hub
        .take_from(
            &hub.bob_desk,
            &[
                ("linear", "driftwerk", Some("allan")),
                ("metabase", "default", Some("allan")),
                ("metabase", "team", Some("allan")),
            ],
        )
        .await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(v["copies"], json!([]), "{text}");
    assert_eq!(v["missing"].as_array().unwrap().len(), 3);
    no_secret_in(&text);
}

#[tokio::test]
async fn a_github_sign_in_through_an_agent_is_never_copied() {
    let (hub, _dir) = hub_with_agents().await;
    let (status, v, text) = hub
        .take_from(&hub.ann_desk, &[("github", "signed", Some("allan"))])
        .await;
    assert_eq!(status, StatusCode::BAD_REQUEST, "{text}");
    assert!(v.get("copies").is_none());
    no_secret_in(&text);
    assert!(hub.copies_audited().is_empty());
}

#[tokio::test]
async fn an_agent_on_this_hubs_own_vault_isnt_listed_as_another_tier() {
    let dir = tempfile::tempdir().unwrap();
    let mirror = agent_with_vault(&dir.path().join("mirror"), "Mirror").await;
    let hub = hub_on(true, mirror.vault.clone()).await;
    hub.state.registry.insert("mirror", mirror);
    let (status, _, v, text) = hub.ask("GET", WITH_AGENTS, Some(&hub.ann_desk), None).await;
    assert_eq!(status, StatusCode::OK, "{text}");
    assert_eq!(
        listed_from(&v),
        [row("github", "acme", None), row("jira", "default", None)]
    );
}

#[test]
fn one_connection_in_two_tiers_is_listed_once_from_the_first() {
    let dir = tempfile::tempdir().unwrap();
    let open = |n: &str| Arc::new(Vault::open(&dir.path().join(n), "test-key").unwrap());
    let (mine, allan, bea) = (open("mine"), open("allan"), open("bea"));
    let ann = Uuid::new_v4();
    let through = |v: &Arc<Vault>, id: &str| Source {
        vault: Arc::clone(v),
        agent: Some(AgentOf {
            id: id.into(),
            name: id.into(),
        }),
    };
    for v in [&mine, &allan, &bea] {
        v.set(
            &format!("users/{ann}/skills/linear/workspaces/acme/api_key"),
            "k",
        )
        .unwrap();
        v.set(
            &format!("users/{ann}/skills/linear/workspaces/acme/label"),
            "Acme",
        )
        .unwrap();
    }
    // Two of one tier stay two; another name is another connection.
    for (key, label) in [("acme2", "acme"), ("other", "Other")] {
        allan
            .set(
                &format!("users/{ann}/skills/linear/workspaces/{key}/api_key"),
                &format!("k-{key}"),
            )
            .unwrap();
        allan
            .set(
                &format!("users/{ann}/skills/linear/workspaces/{key}/label"),
                label,
            )
            .unwrap();
    }
    let sources = [
        Source::personal(Arc::clone(&mine)),
        through(&allan, "allan"),
        through(&bea, "bea"),
    ];
    let got: Vec<(String, Option<String>)> = connections_of(&sources, &ann)
        .into_iter()
        .map(|l| (l.key, l.agent))
        .collect();
    assert_eq!(
        got,
        [
            ("acme".to_string(), None),
            ("other".to_string(), Some("allan".to_string())),
        ],
        "Acme from the personal tier only; Allan's own second Acme is shadowed by it too"
    );
    // An unnamed key at no site is the same connection only when it is the
    // same key: a different one through Allan is offered too.
    mine.set(&format!("users/{ann}/skills/github/token"), "ghp_mine")
        .unwrap();
    allan
        .set(&format!("users/{ann}/skills/github/token"), "ghp_allans")
        .unwrap();
    bea.set(&format!("users/{ann}/skills/github/token"), "ghp_mine")
        .unwrap();
    let github: Vec<_> = connections_of(&sources, &ann)
        .into_iter()
        .filter(|l| l.connector == "github")
        .map(|l| l.agent)
        .collect();
    assert_eq!(
        github,
        [None, Some("allan".to_string())],
        "Bea's is the personal one, again"
    );
    // Without the personal one, the first agent's.
    let got: Vec<_> = connections_of(&sources[1..], &ann)
        .into_iter()
        .map(|l| (l.key, l.agent))
        .collect();
    assert_eq!(
        got,
        [
            ("default".to_string(), Some("allan".to_string())),
            ("default".to_string(), Some("bea".to_string())),
            ("acme".to_string(), Some("allan".to_string())),
            ("acme2".to_string(), Some("allan".to_string())),
            ("other".to_string(), Some("allan".to_string())),
        ]
    );
}
