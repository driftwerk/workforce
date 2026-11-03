use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::Form;
use axum::routing::post as post_route;
use workforce_skills::adapters::google::oauth::StoredTokens;

use super::*;

const SARAH: &str = "4f1c2d3e-0000-4000-8000-000000000001";
const GOOD: &str = "lin_api_good_key_0123456789";
const BAD: &str = "lin_api_bad_key_9876543210";
const GH_GOOD: &str = "ghp_goodtoken0123456789abcdef";

#[derive(Default)]
struct Kept(Mutex<Vec<Checked>>);

#[async_trait]
impl Statuses for Kept {
    async fn all(&self, person: &str) -> eyre::Result<Vec<Checked>> {
        Ok(self
            .0
            .lock()
            .unwrap()
            .iter()
            .filter(|c| c.person == person)
            .cloned()
            .collect())
    }
    async fn put(&self, checked: &Checked, connected: bool) -> eyre::Result<()> {
        let mut rows = self.0.lock().unwrap();
        let old = rows.iter().position(|c| {
            c.connector == checked.connector
                && c.level == checked.level
                && c.person == checked.person
        });
        let mut row = checked.clone();
        if let Some(i) = old {
            let old = rows.remove(i);
            if !connected {
                row.connected_at = old.connected_at;
            }
            row.account = row.account.or(old.account);
        }
        rows.push(row);
        Ok(())
    }
    async fn forget(&self, connector: &str, level: &Level, person: &str) -> eyre::Result<()> {
        self.0
            .lock()
            .unwrap()
            .retain(|c| !(c.connector == connector && &c.level == level && c.person == person));
        Ok(())
    }
}

struct Crew(Vec<Agent>);

impl Agents for Crew {
    fn all(&self) -> Vec<Agent> {
        self.0.clone()
    }
}

/// What the fake GitHub's token endpoint answers next.
#[derive(Default)]
struct Github {
    polls: AtomicUsize,
    /// `token`, `denied` or `pending`.
    outcome: Mutex<&'static str>,
}

async fn services(github: Arc<Github>) -> Services {
    let app = Router::new()
        .route(
            "/user",
            get(|h: HeaderMap| async move {
                match h.get("authorization").and_then(|v| v.to_str().ok()) {
                    Some(a) if a == format!("Bearer {GH_GOOD}") => {
                        Json(json!({ "login": "sarah" })).into_response()
                    }
                    _ => (StatusCode::UNAUTHORIZED, Json(json!({"message": "Bad credentials"}))).into_response(),
                }
            }),
        )
        .route(
            "/graphql",
            post_route(|h: HeaderMap| async move {
                match h.get("authorization").and_then(|v| v.to_str().ok()) {
                    Some(GOOD) => Json(json!({ "data": { "viewer": { "email": "sarah@acme.example" } } }))
                        .into_response(),
                    _ => (
                        StatusCode::BAD_REQUEST,
                        Json(json!({ "errors": [{ "message": format!("Authentication required, not {BAD}") }] })),
                    )
                        .into_response(),
                }
            }),
        )
        .route(
            "/auth.test",
            post_route(|h: HeaderMap| async move {
                match h.get("authorization").and_then(|v| v.to_str().ok()) {
                    Some("Bearer xoxb-good-0123456789") => Json(json!({ "ok": true, "team": "Acme" })),
                    _ => Json(json!({ "ok": false, "error": "invalid_auth" })),
                }
            }),
        )
        .route(
            "/login/device/code",
            post_route(|| async {
                Json(json!({
                    "device_code": "dev-code-secret",
                    "user_code": "ABCD-1234",
                    "verification_uri": "https://github.com/login/device",
                    "expires_in": 900,
                    "interval": 0
                }))
            }),
        )
        .route(
            "/login/oauth/access_token",
            post_route(
                move |State(gh): State<Arc<Github>>, Form(form): Form<BTreeMap<String, String>>| async move {
                    assert_eq!(form.get("device_code").map(String::as_str), Some("dev-code-secret"));
                    let n = gh.polls.fetch_add(1, Ordering::SeqCst);
                    let outcome = *gh.outcome.lock().unwrap();
                    Json(match (n, outcome) {
                        (0, _) | (_, "pending") => json!({ "error": "authorization_pending" }),
                        (_, "denied") => json!({ "error": "access_denied" }),
                        _ => json!({ "access_token": GH_GOOD, "token_type": "bearer" }),
                    })
                },
            ),
        )
        .with_state(github);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Services {
        http: reqwest::Client::new(),
        github_api: base.clone(),
        github_web: base.clone(),
        linear_api: base.clone(),
        slack_api: base.clone(),
        datadog_api: Some(base.clone()),
        google_token: format!("{base}/token"),
        google_userinfo: format!("{base}/userinfo"),
        poll_floor: Duration::from_millis(10),
    }
}

struct Hub {
    _dir: tempfile::TempDir,
    global: Arc<Vault>,
    allan: Arc<Vault>,
    ben: Arc<Vault>,
    kept: Arc<Kept>,
    github: Arc<Github>,
    c: Arc<Connections>,
}

async fn hub() -> Hub {
    let dir = tempfile::tempdir().unwrap();
    let open = |name: &str| Arc::new(Vault::open(&dir.path().join(name), "test-key").unwrap());
    let (global, allan, ben) = (open("global"), open("allan"), open("ben"));
    let kept = Arc::new(Kept::default());
    let github = Arc::new(Github::default());
    *github.outcome.lock().unwrap() = "token";
    let agents = Crew(vec![
        Agent {
            id: "allan".into(),
            name: "Allan".into(),
            vault: Some(Arc::clone(&allan)),
        },
        Agent {
            id: "ben".into(),
            name: "Ben".into(),
            vault: Some(Arc::clone(&ben)),
        },
    ]);
    let c = Arc::new(Connections::new(
        Some(Arc::clone(&global)),
        Arc::new(agents),
        kept.clone(),
        services(Arc::clone(&github)).await,
        Some("Iv1.driftwerk".into()),
        "http://127.0.0.1:1/api/auth/google/callback".into(),
    ));
    Hub {
        _dir: dir,
        global,
        allan,
        ben,
        kept,
        github,
        c,
    }
}

fn linear(key: &str) -> BTreeMap<String, String> {
    BTreeMap::from([("api_key".to_string(), key.to_string())])
}

/// Every key in `vault`.
fn keys(vault: &Vault) -> Vec<String> {
    let mut keys = vault.list(None);
    keys.sort();
    keys
}

fn mine(path: &str) -> String {
    format!("users/{SARAH}/{path}")
}

fn connector<'a>(list: &'a [ConnectorView], id: &str) -> &'a ConnectorView {
    list.iter().find(|c| c.id == id).unwrap()
}

#[tokio::test]
async fn a_connection_for_all_my_agents_is_kept_at_global_per_user_alone() {
    let h = hub().await;
    let shown =
        h.c.connect(SARAH, "linear", "all", None, linear(GOOD))
            .await
            .unwrap();
    assert_eq!(shown.source, VaultSource::GlobalPerUser);
    assert_eq!(shown.account.as_deref(), Some("sarah@acme.example"));
    assert_eq!(shown.status, Status::Connected);
    assert_eq!(keys(&h.global), [mine("skills/linear/api_key")]);
    assert_eq!(
        h.global.get(&mine("skills/linear/api_key")).as_deref(),
        Some(GOOD)
    );
    assert!(keys(&h.allan).is_empty() && keys(&h.ben).is_empty());
}

#[tokio::test]
async fn a_connection_just_for_one_agent_is_kept_at_its_agent_per_user_alone() {
    let h = hub().await;
    let shown =
        h.c.connect(SARAH, "linear", "agent", Some("allan"), linear(GOOD))
            .await
            .unwrap();
    assert_eq!(shown.source, VaultSource::AgentPerUser);
    assert_eq!(shown.agent_name.as_deref(), Some("Allan"));
    assert_eq!(keys(&h.allan), [mine("skills/linear/api_key")]);
    assert!(keys(&h.global).is_empty() && keys(&h.ben).is_empty());
}

#[tokio::test]
async fn slacks_bot_token_is_kept_in_the_shared_tiers() {
    let h = hub().await;
    let token = BTreeMap::from([("bot_token".to_string(), "xoxb-good-0123456789".to_string())]);
    let all =
        h.c.connect(SARAH, "slack", "all", None, token.clone())
            .await
            .unwrap();
    assert_eq!(
        (all.source, all.account.as_deref()),
        (VaultSource::Global, Some("Acme"))
    );
    assert_eq!(keys(&h.global), ["slack.bot_token"]);
    let allan =
        h.c.connect(SARAH, "slack", "agent", Some("allan"), token)
            .await
            .unwrap();
    assert_eq!(allan.source, VaultSource::AgentSpecific);
    assert_eq!(keys(&h.allan), ["slack.bot_token"]);
}

#[tokio::test]
async fn a_failed_test_keeps_nothing_not_even_over_a_working_key() {
    let h = hub().await;
    let refused =
        h.c.connect(SARAH, "linear", "all", None, linear(BAD))
            .await
            .unwrap_err();
    match refused {
        Refusal::TestFailed { error, status } => {
            assert_eq!(status, Status::NeedsReconnect);
            assert_eq!(error, "Linear says this key is invalid");
        }
        other => panic!("{other:?}"),
    }
    assert!(keys(&h.global).is_empty());
    assert!(h.kept.0.lock().unwrap().is_empty());

    // A reconnect that fails leaves the key that works.
    h.c.connect(SARAH, "linear", "all", None, linear(GOOD))
        .await
        .unwrap();
    h.c.connect(SARAH, "linear", "all", None, linear(BAD))
        .await
        .unwrap_err();
    assert_eq!(
        h.global.get(&mine("skills/linear/api_key")).as_deref(),
        Some(GOOD)
    );
}

#[tokio::test]
async fn a_services_words_never_carry_the_key_back() {
    let h = hub().await;
    // GitHub's fake echoes nothing, Linear's echoes the key it was given.
    let mut fields = linear(BAD);
    fields.insert("api_key".into(), BAD.into());
    let shown = format!(
        "{:?}",
        h.c.connect(SARAH, "linear", "all", None, fields)
            .await
            .unwrap_err()
    );
    assert!(!shown.contains(BAD), "{shown}");
}

#[tokio::test]
async fn no_route_returns_a_value() {
    let h = hub().await;
    h.c.connect(SARAH, "linear", "all", None, linear(GOOD))
        .await
        .unwrap();
    h.c.connect(SARAH, "linear", "agent", Some("allan"), linear(GOOD))
        .await
        .unwrap();
    let listed = serde_json::to_string(&h.c.list(SARAH).await).unwrap();
    let tested =
        serde_json::to_string(&h.c.test(SARAH, "linear", "all", None).await.unwrap()).unwrap();
    for shown in [listed, tested] {
        assert!(!shown.contains(GOOD), "{shown}");
    }
}

#[tokio::test]
async fn the_list_says_where_each_is_kept_and_who_uses_it() {
    let h = hub().await;
    let empty = h.c.list(SARAH).await;
    assert_eq!(empty.len(), catalog::CATALOG.len());
    assert!(empty
        .iter()
        .all(|c| c.connections.is_empty() && c.used_by.is_empty()));

    h.c.connect(SARAH, "linear", "all", None, linear(GOOD))
        .await
        .unwrap();
    h.c.connect(SARAH, "linear", "agent", Some("ben"), linear(GOOD))
        .await
        .unwrap();
    let list = h.c.list(SARAH).await;
    let linear = connector(&list, "linear");
    assert_eq!(
        linear
            .connections
            .iter()
            .map(|c| (c.level, c.agent.as_deref()))
            .collect::<Vec<_>>(),
        [("all", None), ("agent", Some("ben"))]
    );
    assert_eq!(
        linear.used_by,
        [
            UsedBy {
                agent: "allan".into(),
                name: "Allan".into(),
                connection: "mine"
            },
            UsedBy {
                agent: "ben".into(),
                name: "Ben".into(),
                connection: "own"
            },
        ]
    );
    // Another person sees none of Sarah's.
    let other = h.c.list("someone-else").await;
    assert!(connector(&other, "linear").connections.is_empty());
}

#[tokio::test]
async fn a_key_saved_before_connections_still_counts_and_a_disconnect_removes_it() {
    let h = hub().await;
    h.global.set("skills/linear/api_key", GOOD).unwrap();
    let list = h.c.list(SARAH).await;
    let linear = connector(&list, "linear");
    assert_eq!(linear.connections[0].source, VaultSource::Global);
    assert_eq!(linear.connections[0].checked_at, None);
    assert_eq!(linear.used_by.len(), 2);
    h.c.disconnect(SARAH, "linear", "all", None).await.unwrap();
    assert!(keys(&h.global).is_empty());
}

#[tokio::test]
async fn a_disconnect_removes_only_its_tier() {
    let h = hub().await;
    h.c.connect(SARAH, "linear", "all", None, linear(GOOD))
        .await
        .unwrap();
    h.c.connect(SARAH, "linear", "agent", Some("allan"), linear(GOOD))
        .await
        .unwrap();
    h.allan
        .set("skills/linear/api_key", "agent-shared")
        .unwrap();
    h.c.disconnect(SARAH, "linear", "agent", Some("allan"))
        .await
        .unwrap();
    assert_eq!(
        keys(&h.allan),
        ["skills/linear/api_key"],
        "the bare agent key stays"
    );
    assert_eq!(keys(&h.global), [mine("skills/linear/api_key")]);
    let list = h.c.list(SARAH).await;
    let rows = &connector(&list, "linear").connections;
    // Allan's bare key still connects it; Sarah's own for all stays.
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].source, VaultSource::AgentSpecific);
    assert_eq!(h.kept.0.lock().unwrap().len(), 1);
}

#[tokio::test]
async fn an_optional_field_left_out_is_not_kept_from_before() {
    let h = hub().await;
    let mut fields = BTreeMap::from([
        ("username".to_string(), "sarah@acme.example".to_string()),
        ("password".to_string(), "app-password".to_string()),
        ("imap_host".to_string(), "imap.acme.example".to_string()),
        ("smtp_host".to_string(), "smtp.acme.example".to_string()),
    ]);
    let tier = VaultSource::GlobalPerUser;
    let c = catalog::find("email").unwrap();
    h.c.keep(c, &Level::All, SARAH, &fields).unwrap();
    fields.remove("smtp_host");
    assert_eq!(h.c.keep(c, &Level::All, SARAH, &fields).unwrap(), tier);
    assert!(h
        .global
        .get(&mine("skills/email/accounts/default/smtp_host"))
        .is_none());
    assert!(h
        .global
        .get(&mine("skills/email/accounts/default/imap_host"))
        .is_some());
}

#[tokio::test]
async fn what_is_asked_for_is_checked_first() {
    let h = hub().await;
    for (connector, level, agent, fields, want) in [
        ("nope", "all", None, linear(GOOD), "no connector"),
        ("linear", "agent", Some("zed"), linear(GOOD), "no agent"),
        ("linear", "agent", None, linear(GOOD), "which agent"),
        ("linear", "all", None, BTreeMap::new(), "is needed"),
        (
            "linear",
            "all",
            None,
            BTreeMap::from([("x".into(), "y".into())]),
            "no field x",
        ),
        ("google", "all", None, BTreeMap::new(), "signing in"),
    ] {
        let e =
            h.c.connect(SARAH, connector, level, agent, fields)
                .await
                .unwrap_err();
        assert!(format!("{e:?}").contains(want), "{connector}: {e:?}");
    }
    assert!(keys(&h.global).is_empty());
}

async fn settled(h: &Hub, flow: &str) -> FlowState {
    for _ in 0..200 {
        match h.c.flow(SARAH, flow) {
            Some(FlowState::Waiting) => tokio::time::sleep(Duration::from_millis(10)).await,
            Some(state) => return state,
            None => panic!("the flow is gone"),
        }
    }
    panic!("the flow never settled")
}

#[tokio::test]
async fn githubs_device_sign_in_keeps_the_token_at_the_tier_it_was_for() {
    let h = hub().await;
    let started =
        h.c.start(SARAH, "github", "agent", Some("allan"))
            .await
            .unwrap();
    assert_eq!(started.user_code.as_deref(), Some("ABCD-1234"));
    // The device code stays on the hub.
    assert!(!serde_json::to_string(&started)
        .unwrap()
        .contains("dev-code-secret"));
    assert_eq!(
        settled(&h, &started.flow).await,
        FlowState::Connected {
            account: "sarah".into()
        }
    );
    assert_eq!(keys(&h.allan), [mine("skills/github/token")]);
    assert!(keys(&h.global).is_empty());
    assert!(h.c.flow("someone-else", &started.flow).is_none());
}

#[tokio::test]
async fn a_github_sign_in_turned_down_or_cancelled_keeps_nothing() {
    let h = hub().await;
    *h.github.outcome.lock().unwrap() = "denied";
    let started = h.c.start(SARAH, "github", "all", None).await.unwrap();
    assert_eq!(settled(&h, &started.flow).await, FlowState::Cancelled);

    *h.github.outcome.lock().unwrap() = "pending";
    let started = h.c.start(SARAH, "github", "all", None).await.unwrap();
    assert!(h.c.cancel(SARAH, &started.flow));
    *h.github.outcome.lock().unwrap() = "token";
    tokio::time::sleep(Duration::from_millis(100)).await;
    assert_eq!(h.c.flow(SARAH, &started.flow), Some(FlowState::Cancelled));
    assert!(keys(&h.global).is_empty());
}

#[tokio::test]
async fn without_driftwerks_github_app_github_takes_a_token() {
    let mut h = hub().await;
    let c = Arc::get_mut(&mut h.c).unwrap();
    c.github_client_id = None;
    let list = h.c.list(SARAH).await;
    assert!(!connector(&list, "github").sign_in_ready);
    assert!(matches!(
        h.c.start(SARAH, "github", "all", None).await,
        Err(Refusal::Unavailable(_))
    ));
    let token = BTreeMap::from([("token".to_string(), GH_GOOD.to_string())]);
    assert_eq!(
        h.c.connect(SARAH, "github", "all", None, token)
            .await
            .unwrap()
            .account
            .as_deref(),
        Some("sarah")
    );
}

fn tokens() -> StoredTokens {
    StoredTokens {
        email: Some("sarah@acme.example".into()),
        refresh_token: "1//refresh".into(),
        access_token: "ya29.access".into(),
        expires_at: 1,
        scopes: "email".into(),
    }
}

#[tokio::test]
async fn a_google_sign_in_is_kept_where_desk_started_it() {
    let h = hub().await;
    assert!(matches!(
        h.c.start(SARAH, "google", "agent", Some("allan")).await,
        Err(Refusal::Invalid(_))
    ));
    h.c.keep_google_client(SARAH, "client.apps.googleusercontent.com", "client-secret")
        .unwrap();
    let started =
        h.c.start(SARAH, "google", "agent", Some("allan"))
            .await
            .unwrap();
    let url = started.url.unwrap();
    assert!(
        url.contains("redirect_uri=http%3A%2F%2F127.0.0.1%3A1"),
        "{url}"
    );
    let account =
        h.c.google_signed_in(&started.flow, SARAH, &tokens())
            .await
            .unwrap();
    assert_eq!(account, "sarah@acme.example");
    assert!(keys(&h.allan)
        .iter()
        .all(|k| k.starts_with(&mine("skills/google/workspaces/default/"))));
    assert!(h.allan.get(&mine(catalog::GOOGLE_EMAIL)).is_some());
    assert_eq!(
        keys(&h.global),
        [catalog::GOOGLE_CLIENT_ID, catalog::GOOGLE_CLIENT_SECRET],
        "only the person's own client is hub-wide"
    );
    assert_eq!(
        h.c.flow(SARAH, &started.flow),
        Some(FlowState::Connected {
            account: "sarah@acme.example".into()
        })
    );
}

#[tokio::test]
async fn a_google_sign_in_cancelled_meanwhile_keeps_nothing() {
    let h = hub().await;
    h.c.keep_google_client(SARAH, "client", "secret").unwrap();
    let started = h.c.start(SARAH, "google", "all", None).await.unwrap();
    h.c.google_cancelled(&started.flow);
    assert!(h
        .c
        .google_signed_in(&started.flow, SARAH, &tokens())
        .await
        .is_err());
    assert_eq!(keys(&h.global).len(), 2, "only the client");
    // Nor does a sign-in someone else started.
    let started = h.c.start(SARAH, "google", "all", None).await.unwrap();
    assert!(h
        .c
        .google_signed_in(&started.flow, "someone-else", &tokens())
        .await
        .is_err());
}

#[test]
fn the_google_callback_is_the_hubs_own_on_this_computer() {
    assert_eq!(
        google_redirect("0.0.0.0:50111"),
        "http://127.0.0.1:50111/api/auth/google/callback"
    );
}

#[test]
fn the_other_adapters_are_every_built_in_one_the_catalog_leaves_out() {
    let others = other_adapters();
    assert!(others.contains(&"xero") && others.contains(&"browser"));
    assert!(!others.contains(&"linear"));
    assert_eq!(
        others.len() + catalog::CATALOG.len(),
        crate::skill_loader::builtin_skill_catalog().len()
    );
}
