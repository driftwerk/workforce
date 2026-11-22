use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::Form;
use axum::routing::post as post_route;

use super::*;

const SARAH: &str = "4f1c2d3e-0000-4000-8000-000000000001";
const GOOD: &str = "lin_api_good_key_0123456789";
const BAD: &str = "lin_api_bad_key_9876543210";
const GOOD2: &str = "lin_api_other_key_0123456789";
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
                && c.instance == checked.instance
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
    async fn forget(
        &self,
        connector: &str,
        level: &Level,
        instance: &str,
        person: &str,
    ) -> eyre::Result<()> {
        self.0.lock().unwrap().retain(|c| {
            !(c.connector == connector
                && &c.level == level
                && c.instance == instance
                && c.person == person)
        });
        Ok(())
    }
    async fn moved(
        &self,
        connector: &str,
        level: &Level,
        person: &str,
        from: &str,
        to: &str,
    ) -> eyre::Result<()> {
        for c in self.0.lock().unwrap().iter_mut() {
            if c.connector == connector
                && &c.level == level
                && c.person == person
                && c.instance == from
            {
                c.instance = to.to_string();
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct Crew {
    agents: Mutex<Vec<Agent>>,
    reloads: Mutex<Vec<(String, String)>>,
    /// Whether each agent watches the person's bank approvals, as last set.
    watches: Mutex<BTreeMap<String, bool>>,
    /// Each agent's `config.toml`, as the hub's write keeps it.
    configs: Mutex<HashMap<String, String>>,
}

#[async_trait]
impl Agents for Crew {
    fn all(&self) -> Vec<Agent> {
        self.agents.lock().unwrap().clone()
    }
    async fn reload(&self, agent: &str, connector: &str, person: &str) {
        assert_eq!(person, SARAH);
        self.reloads
            .lock()
            .unwrap()
            .push((agent.to_string(), connector.to_string()));
    }
    async fn watch_approvals(&self, agent: &str, person: &str, on: bool) {
        assert_eq!(person, SARAH);
        self.watches.lock().unwrap().insert(agent.to_string(), on);
    }
    async fn choose(
        &self,
        agent: &str,
        person: &str,
        choice: &picks::Choice,
    ) -> Result<(), String> {
        assert_eq!(person, SARAH);
        let body = self
            .configs
            .lock()
            .unwrap()
            .get(agent)
            .cloned()
            .unwrap_or_default();
        let mut doc: toml_edit::DocumentMut = body.parse().map_err(|e| format!("{e}"))?;
        choice.apply(&mut doc)?;
        let body = doc.to_string();
        self.bind(agent, Binding::parse(&body));
        self.configs.lock().unwrap().insert(agent.into(), body);
        Ok(())
    }
}

impl Crew {
    fn bind(&self, agent: &str, binding: Binding) {
        for a in self.agents.lock().unwrap().iter_mut() {
            if a.id == agent {
                a.binding = binding.clone();
            }
        }
    }
    fn reloaded(&self) -> Vec<(String, String)> {
        std::mem::take(&mut *self.reloads.lock().unwrap())
    }
}

/// What the fake GitHub's token endpoint answers next.
#[derive(Default)]
struct Github {
    polls: AtomicUsize,
    /// `token`, `denied` or `pending`.
    outcome: Mutex<&'static str>,
}

const BS_GOOD: &str = "bsk_readonly0000000000000000000";
const BS_WRITES: &str = "bsk_canwrite0000000000000000000";
/// Whose a fake provider's token is: `at-<who>`, `rt-<who>`, a rotated
/// one `rt-<who>-r`.
fn who(token: &str) -> Option<&str> {
    let who = token
        .strip_prefix("at-")
        .or_else(|| token.strip_prefix("rt-"))?;
    Some(who.trim_end_matches("-r"))
}

/// Google's and FreeAgent's token endpoints, as a test has them: a code is
/// whose it is, `narrow` grants Calendar alone, a refresh rotates its token,
/// and `revoked` is refused as Google and FreeAgent refuse one.
async fn provider_token(h: HeaderMap, Form(form): Form<BTreeMap<String, String>>) -> Response {
    // FreeAgent sends its app in Basic auth, Google in the body.
    if h.get("authorization").is_none() && !form.contains_key("client_id") {
        return (
            StatusCode::UNAUTHORIZED,
            Json(json!({ "error": "invalid_client" })),
        )
            .into_response();
    }
    let refresh = form.get("grant_type").map(String::as_str) == Some("refresh_token");
    let named = match refresh {
        true => form.get("refresh_token").map(String::as_str).and_then(who),
        false => form.get("code").map(String::as_str),
    };
    let Some(who) = named.filter(|w| *w != "revoked") else {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_grant" })),
        )
            .into_response();
    };
    let scope = match who {
        "narrow" => "https://www.googleapis.com/auth/calendar".to_string(),
        _ => workforce_skills::adapters::google::oauth::APP_SCOPES.join(" "),
    };
    Json(json!({
        "access_token": format!("at-{who}"),
        "refresh_token": format!("rt-{who}{}", if refresh { "-r" } else { "" }),
        "expires_in": 3600,
        "scope": scope,
    }))
    .into_response()
}

fn bearer(h: &HeaderMap) -> Option<String> {
    let v = h.get("authorization")?.to_str().ok()?;
    who(v.strip_prefix("Bearer ")?).map(str::to_string)
}

async fn google_userinfo(h: HeaderMap) -> Response {
    match bearer(&h) {
        Some(who) => Json(json!({
            // `work2` is `work` signed in once more.
            "sub": format!("1{}", who.trim_end_matches('2').len() * 1000 + who.as_bytes()[0] as usize),
            "email": format!("{who}@acme.example"),
        }))
        .into_response(),
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

/// FreeAgent's company, as its API answers: the same `url` for every
/// business, told apart by `id`. `harbor2` is Harbor again, signed in once
/// more.
async fn freeagent_company(h: HeaderMap) -> Response {
    match bearer(&h) {
        Some(who) => {
            let at = who.trim_end_matches('2');
            let mut name = at.to_string();
            name[..1].make_ascii_uppercase();
            Json(json!({ "company": {
                "url": "https://api.freeagent.com/v2/company",
                "id": format!("{}", at.len() * 1000 + at.as_bytes()[0] as usize),
                "name": name,
                "subdomain": at,
            }}))
            .into_response()
        }
        None => StatusCode::UNAUTHORIZED.into_response(),
    }
}

async fn services(github: Arc<Github>) -> Services {
    let app = Router::new()
        .route(
            "/banksync/whoami",
            get(|h: HeaderMap| async move {
                let scopes = match h.get("x-api-key").and_then(|v| v.to_str().ok()) {
                    Some(BS_GOOD) => json!(["banks:read", "institutions:read"]),
                    Some(BS_WRITES) => json!(["banks:read", "banks:write"]),
                    _ => return StatusCode::UNAUTHORIZED.into_response(),
                };
                Json(json!({ "success": true, "data": { "scopes": scopes } })).into_response()
            }),
        )
        .route(
            "/banksync/banks",
            get(|| async {
                Json(json!({ "success": true, "data": [
                    { "id": "bnk_nw", "name": "NatWest", "connectionStatus": { "connected": true } },
                    { "id": "bnk_rev", "name": "Revolut", "connectionStatus": { "connected": false } }
                ] }))
            }),
        )
        .route(
            "/banksync/banks/{id}",
            get(|axum::extract::Path(id): axum::extract::Path<String>| async move {
                let (name, connected) = match id.as_str() {
                    "bnk_nw" => ("NatWest", true),
                    _ => ("Revolut", false),
                };
                Json(json!({ "success": true, "data": {
                    "id": id, "name": name, "connectionStatus": { "connected": connected }
                } }))
            }),
        )
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
                    Some(GOOD2) => Json(json!({ "data": { "viewer": { "email": "sarah@globex.example" } } }))
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
            post_route(|Form(form): Form<BTreeMap<String, String>>| async move {
                // A board synced with a GitHub project needs Projects.
                let scopes: Vec<&str> = form.get("scope").map(|s| s.split(' ').collect()).unwrap_or_default();
                assert!(scopes.contains(&"project"), "{scopes:?}");
                assert!(scopes.contains(&"repo"), "{scopes:?}");
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
        .route("/token", post_route(provider_token))
        .route("/token_endpoint", post_route(provider_token))
        .route("/userinfo", get(google_userinfo))
        .route("/company", get(freeagent_company))
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
        bank_api: format!("{base}/banksync"),
        bank_limiter: Default::default(),
        google_auth: format!("{base}/auth"),
        google_token: format!("{base}/token"),
        google_userinfo: format!("{base}/userinfo"),
        freeagent_api: base.clone(),
        poll_floor: Duration::from_millis(10),
        gh: super::gh::GhCli::none(),
    }
}

struct Hub {
    _dir: tempfile::TempDir,
    global: Arc<Vault>,
    allan: Arc<Vault>,
    ben: Arc<Vault>,
    kept: Arc<Kept>,
    github: Arc<Github>,
    crew: Arc<Crew>,
    c: Arc<Connections>,
}

async fn hub() -> Hub {
    let dir = tempfile::tempdir().unwrap();
    let open = |name: &str| Arc::new(Vault::open(&dir.path().join(name), "test-key").unwrap());
    let (global, allan, ben) = (open("global"), open("allan"), open("ben"));
    let kept = Arc::new(Kept::default());
    let github = Arc::new(Github::default());
    *github.outcome.lock().unwrap() = "token";
    let crew = Arc::new(Crew::default());
    *crew.agents.lock().unwrap() = vec![
        Agent {
            id: "allan".into(),
            name: "Allan".into(),
            vault: Some(Arc::clone(&allan)),
            binding: Binding::default(),
        },
        Agent {
            id: "ben".into(),
            name: "Ben".into(),
            vault: Some(Arc::clone(&ben)),
            binding: Binding::default(),
        },
    ];
    let c = Arc::new(Connections::new(
        Some(Arc::clone(&global)),
        crew.clone(),
        kept.clone(),
        services(Arc::clone(&github)).await,
        Some("Iv1.driftwerk".into()),
        "127.0.0.1:1".into(),
    ));
    Hub {
        _dir: dir,
        global,
        allan,
        ben,
        kept,
        github,
        crew,
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

/// Where Linear's instance `key` keeps `sub`.
fn ws(key: &str, sub: &str) -> String {
    format!("skills/linear/workspaces/{key}/{sub}")
}

fn connector<'a>(list: &'a [ConnectorView], id: &str) -> &'a ConnectorView {
    list.iter().find(|c| c.id == id).unwrap()
}

impl Hub {
    async fn connect(
        &self,
        name: Option<&str>,
        level: &str,
        agent: Option<&str>,
        key: &str,
    ) -> Result<ConnectionView, Refusal> {
        self.c
            .connect(SARAH, "linear", name, level, agent, linear(key))
            .await
    }
}

#[tokio::test]
async fn a_connection_for_all_my_agents_is_kept_at_global_per_user_alone() {
    let h = hub().await;
    let shown = h.connect(None, "all", None, GOOD).await.unwrap();
    assert_eq!(shown.source, VaultSource::GlobalPerUser);
    assert_eq!(shown.account.as_deref(), Some("sarah@acme.example"));
    assert_eq!(shown.name, "sarah@acme.example", "named after its account");
    assert_eq!(shown.status, Status::Connected);
    // A first connection is the adapters' `default` instance.
    assert_eq!(
        keys(&h.global),
        [
            mine(&ws("default", "api_key")),
            mine(&ws("default", "label"))
        ]
    );
    assert_eq!(
        h.global.get(&mine(&ws("default", "api_key"))).as_deref(),
        Some(GOOD)
    );
    assert!(keys(&h.allan).is_empty() && keys(&h.ben).is_empty());
}

#[tokio::test]
async fn a_connection_just_for_one_agent_is_kept_at_its_agent_per_user_alone() {
    let h = hub().await;
    let shown = h
        .connect(Some("Acme"), "agent", Some("allan"), GOOD)
        .await
        .unwrap();
    assert_eq!(shown.source, VaultSource::AgentPerUser);
    assert_eq!(shown.agent_name.as_deref(), Some("Allan"));
    assert_eq!(shown.name, "Acme");
    assert_eq!(
        keys(&h.allan),
        [
            mine(&ws("default", "api_key")),
            mine(&ws("default", "label"))
        ]
    );
    assert!(keys(&h.global).is_empty() && keys(&h.ben).is_empty());
}

#[tokio::test]
async fn a_service_holds_several_connections_each_its_own_instance() {
    let h = hub().await;
    let acme = h.connect(Some("Acme"), "all", None, GOOD).await.unwrap();
    let globex = h
        .connect(Some("Globex Corp"), "all", None, GOOD2)
        .await
        .unwrap();
    let lattice = h
        .connect(Some("Globex Corp"), "agent", Some("ben"), GOOD2)
        .await
        .unwrap();
    assert_eq!(acme.id, "all~~default");
    assert_eq!(globex.id, "all~~globex-corp");
    // Keys are the person's alone across every level: Ben's is another.
    assert_eq!(lattice.id, "agent~ben~globex-corp-2");
    assert_eq!(
        h.global
            .get(&mine(&ws("globex-corp", "api_key")))
            .as_deref(),
        Some(GOOD2)
    );
    assert_eq!(
        h.global.get(&mine(&ws("default", "api_key"))).as_deref(),
        Some(GOOD)
    );
    let list = h.c.list(SARAH).await;
    let linear = connector(&list, "linear");
    assert!(linear.several);
    assert_eq!(
        linear
            .connections
            .iter()
            .map(|c| (c.name.as_str(), c.account.as_deref()))
            .collect::<Vec<_>>(),
        [
            ("Acme", Some("sarah@acme.example")),
            ("Globex Corp", Some("sarah@globex.example")),
            ("Globex Corp", Some("sarah@globex.example")),
        ]
    );
    // Allan reaches the two for all; Ben those and his own.
    let users = |i: usize| -> Vec<&str> {
        linear.connections[i]
            .used_by
            .iter()
            .map(|u| u.agent.as_str())
            .collect()
    };
    assert_eq!(users(0), ["allan", "ben"]);
    assert_eq!(users(2), ["ben"]);
    // Slack holds one.
    assert!(!connector(&list, "slack").several);
}

#[tokio::test]
async fn a_rename_names_it_again_and_keeps_its_instance() {
    let h = hub().await;
    let acme = h.connect(Some("Acme"), "all", None, GOOD).await.unwrap();
    let renamed =
        h.c.rename(SARAH, "linear", &acme.id, "  Acme Corp  ")
            .await
            .unwrap();
    assert_eq!(
        (renamed.id.as_str(), renamed.name.as_str()),
        (acme.id.as_str(), "Acme Corp")
    );
    assert_eq!(
        h.global.get(&mine(&ws("default", "label"))).as_deref(),
        Some("Acme Corp")
    );
    assert!(h.global.get(&mine(&ws("default", "api_key"))).is_some());
    for bad in ["", "   ", &"x".repeat(81)] {
        assert!(matches!(
            h.c.rename(SARAH, "linear", &acme.id, bad).await,
            Err(Refusal::Invalid(_))
        ));
    }
    assert!(matches!(
        h.c.rename(SARAH, "linear", "all~~nothing", "Zed").await,
        Err(Refusal::NotFound(_))
    ));
}

#[tokio::test]
async fn a_reconnect_keeps_new_keys_only_when_they_work() {
    let h = hub().await;
    let acme = h.connect(Some("Acme"), "all", None, GOOD).await.unwrap();
    h.c.reconnect(SARAH, "linear", &acme.id, linear(BAD))
        .await
        .unwrap_err();
    assert_eq!(
        h.global.get(&mine(&ws("default", "api_key"))).as_deref(),
        Some(GOOD)
    );
    let again =
        h.c.reconnect(SARAH, "linear", &acme.id, linear(GOOD2))
            .await
            .unwrap();
    assert_eq!(again.name, "Acme", "its name stays");
    assert_eq!(again.account.as_deref(), Some("sarah@globex.example"));
    assert_eq!(
        h.global.get(&mine(&ws("default", "api_key"))).as_deref(),
        Some(GOOD2)
    );
}

#[tokio::test]
async fn slacks_bot_token_is_kept_in_the_shared_tiers() {
    let h = hub().await;
    let token = BTreeMap::from([("bot_token".to_string(), "xoxb-good-0123456789".to_string())]);
    let all =
        h.c.connect(SARAH, "slack", None, "all", None, token.clone())
            .await
            .unwrap();
    assert_eq!(
        (all.source, all.account.as_deref()),
        (VaultSource::Global, Some("Acme"))
    );
    assert_eq!(keys(&h.global), ["slack.bot_token"]);
    let allan =
        h.c.connect(SARAH, "slack", None, "agent", Some("allan"), token.clone())
            .await
            .unwrap();
    assert_eq!(allan.source, VaultSource::AgentSpecific);
    assert_eq!(keys(&h.allan), ["slack.bot_token"]);
    // Connecting it again replaces the one it holds.
    h.c.connect(SARAH, "slack", None, "all", None, token)
        .await
        .unwrap();
    let list = h.c.list(SARAH).await;
    assert_eq!(connector(&list, "slack").connections.len(), 2);
    assert!(matches!(
        h.c.rename(SARAH, "slack", &all.id, "Work").await,
        Err(Refusal::Invalid(_))
    ));
}

#[tokio::test]
async fn a_failed_test_keeps_nothing_not_even_over_a_working_key() {
    let h = hub().await;
    let refused = h.connect(None, "all", None, BAD).await.unwrap_err();
    match refused {
        Refusal::TestFailed { error, status } => {
            assert_eq!(status, Status::NeedsReconnect);
            assert_eq!(error, "Linear says this key is invalid");
        }
        other => panic!("{other:?}"),
    }
    assert!(keys(&h.global).is_empty());
    assert!(h.kept.0.lock().unwrap().is_empty());

    h.connect(None, "all", None, GOOD).await.unwrap();
    h.connect(None, "all", None, BAD).await.unwrap_err();
    assert_eq!(keys(&h.global).len(), 2, "one connection, its key and name");
}

#[tokio::test]
async fn a_services_words_never_carry_the_key_back() {
    let h = hub().await;
    // Linear's fake echoes the key it was given.
    let shown = format!("{:?}", h.connect(None, "all", None, BAD).await.unwrap_err());
    assert!(!shown.contains(BAD), "{shown}");
    let acme = h.connect(None, "all", None, GOOD).await.unwrap();
    let shown = format!(
        "{:?}",
        h.c.reconnect(SARAH, "linear", &acme.id, linear(BAD))
            .await
            .unwrap_err()
    );
    assert!(!shown.contains(BAD), "{shown}");
}

#[tokio::test]
async fn no_route_returns_a_value() {
    let h = hub().await;
    let all = h.connect(None, "all", None, GOOD).await.unwrap();
    h.connect(None, "agent", Some("allan"), GOOD).await.unwrap();
    let listed = serde_json::to_string(&h.c.list(SARAH).await).unwrap();
    let tested = serde_json::to_string(&h.c.test(SARAH, "linear", &all.id).await.unwrap()).unwrap();
    let renamed =
        serde_json::to_string(&h.c.rename(SARAH, "linear", &all.id, "Acme").await.unwrap())
            .unwrap();
    for shown in [listed, tested, renamed] {
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

    h.connect(None, "all", None, GOOD).await.unwrap();
    h.connect(None, "agent", Some("ben"), GOOD).await.unwrap();
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
    // A Desk from before several connections names the level alone.
    let left =
        h.c.disconnect_level(SARAH, "linear", "all", None)
            .await
            .unwrap();
    assert_eq!(left.still_kept, None);
    assert!(keys(&h.global).is_empty());
}

#[tokio::test]
async fn a_disconnect_says_when_a_hub_wide_key_still_connects_it() {
    let h = hub().await;
    h.global.set(&ws("default", "api_key"), "hub-wide").unwrap();
    h.global
        .set(&mine(&ws("default", "api_key")), GOOD)
        .unwrap();
    let left =
        h.c.disconnect(SARAH, "linear", "all~~default")
            .await
            .unwrap();
    assert_eq!(keys(&h.global), [ws("default", "api_key")]);
    assert_eq!(left.still_kept, Some(VaultSource::Global));
    assert!(left
        .note
        .unwrap()
        .contains("saved for everyone on this hub"));
}

#[tokio::test]
async fn a_connect_or_disconnect_loads_the_connector_again_where_it_reaches() {
    let h = hub().await;
    h.connect(None, "all", None, GOOD).await.unwrap();
    assert_eq!(
        h.crew.reloaded(),
        [
            ("allan".into(), "linear".into()),
            ("ben".into(), "linear".into())
        ]
    );
    let ben = h.connect(None, "agent", Some("ben"), GOOD).await.unwrap();
    assert_eq!(h.crew.reloaded(), [("ben".into(), "linear".into())]);
    h.c.disconnect(SARAH, "linear", &ben.id).await.unwrap();
    assert_eq!(h.crew.reloaded(), [("ben".into(), "linear".into())]);
    // A failed Test changes nothing, so nothing loads again.
    h.connect(None, "all", None, BAD).await.unwrap_err();
    assert!(h.crew.reloaded().is_empty());
}

#[tokio::test]
async fn used_by_follows_each_agents_skills() {
    let h = hub().await;
    h.connect(None, "all", None, GOOD).await.unwrap();
    h.crew.bind(
        "allan",
        Binding {
            enabled: Some(vec![]),
            own: vec![],
            ..Default::default()
        },
    );
    h.crew.bind(
        "ben",
        Binding {
            enabled: Some(vec!["linear".into()]),
            own: vec!["linear".into()],
            ..Default::default()
        },
    );
    let list = h.c.list(SARAH).await;
    // Allan has Linear off; Ben uses only his own, which isn't connected.
    assert!(connector(&list, "linear").used_by.is_empty());
    h.crew.bind(
        "ben",
        Binding {
            enabled: Some(vec!["linear".into()]),
            own: vec![],
            ..Default::default()
        },
    );
    let list = h.c.list(SARAH).await;
    assert_eq!(
        connector(&list, "linear").used_by,
        [UsedBy {
            agent: "ben".into(),
            name: "Ben".into(),
            connection: "mine"
        }]
    );
}

#[test]
fn a_binding_is_read_from_the_agents_config() {
    let dir = tempfile::tempdir().unwrap();
    assert_eq!(Binding::read(dir.path()), Binding::default());
    std::fs::write(
        dir.path()
            .join(workforce_common::config_files::WORKSPACE_CONFIG),
        "[skills]\nenabled = [\"linear\"]\nown_credential = [\"linear\"]\n",
    )
    .unwrap();
    assert_eq!(
        Binding::read(dir.path()),
        Binding {
            enabled: Some(vec!["linear".into()]),
            own: vec!["linear".into()],
            ..Default::default()
        }
    );
}

/// Email copies its keys when it loads: once disconnected, reloading it
/// leaves it unloaded, so it stops at once.
#[test]
fn a_disconnected_email_stops_at_once() {
    use workforce_agent::runner::CURRENT_USER_ID;
    let dir = tempfile::tempdir().unwrap();
    let agent = Arc::new(Vault::open(&dir.path().join("agent"), "test-key").unwrap());
    let global = Arc::new(Vault::open(&dir.path().join("global"), "test-key").unwrap());
    for (field, value) in [
        ("username", "sarah@acme.example"),
        ("password", "app-password"),
        ("imap_host", "imap.acme.example"),
        ("smtp_host", "smtp.acme.example"),
    ] {
        global
            .set(
                &mine(&format!("skills/email/accounts/default/{field}")),
                value,
            )
            .unwrap();
    }
    let config = workforce_common::settings::WorkforceConfig::default();
    let tools = workforce_agent::tools::registry::ToolRegistry::new();
    let state = crate::skill_state::SkillStateTracker::new();
    CURRENT_USER_ID
        .sync_scope(SARAH.to_string(), || {
            crate::skill_loader::load_and_track(
                "email",
                Arc::clone(&agent),
                Some(Arc::clone(&global)),
                &config,
                &tools,
                &state,
                "vault",
            )
        })
        .unwrap();
    let accounts = |tools: &workforce_agent::tools::registry::ToolRegistry| {
        let tool = tools.get("email.accounts");
        async move {
            match tool {
                None => String::new(),
                Some(t) => {
                    let r = CURRENT_USER_ID
                        .scope(SARAH.to_string(), t.execute(serde_json::json!({})))
                        .await
                        .unwrap();
                    format!("{:?} {:?}", r.output, r.error)
                }
            }
        }
    };
    let runtime = tokio::runtime::Runtime::new().unwrap();
    assert!(runtime
        .block_on(accounts(&tools))
        .contains("sarah@acme.example"));
    for key in global.list(None) {
        global.delete(&key).unwrap();
    }
    reload_skill("email", agent, Some(global), &config, &tools, &state, SARAH);
    // The copy it loaded with is gone: it reaches no mailbox.
    assert!(!runtime
        .block_on(accounts(&tools))
        .contains("sarah@acme.example"));
}

#[tokio::test]
async fn a_metabase_behind_cloudflare_access_tests_with_its_service_token() {
    use wiremock::matchers::{header, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};
    let h = hub().await;
    let metabase = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api/user/current"))
        .and(header("x-api-key", "mb-key"))
        .and(header("cf-access-client-id", "test-client-id.access"))
        .and(header("cf-access-client-secret", "not-a-real-secret"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "email": "sarah@acme.example" })),
        )
        .mount(&metabase)
        .await;
    for (sub, value) in [
        ("api_key", "mb-key"),
        ("site_url", metabase.uri().as_str()),
        ("access_client_id", "test-client-id.access"),
        ("access_client_secret", "not-a-real-secret"),
    ] {
        h.global
            .set(
                &mine(&format!("skills/metabase/instances/default/{sub}")),
                value,
            )
            .unwrap();
    }
    let list = h.c.list(SARAH).await;
    let id = connector(&list, "metabase").connections[0].id.clone();
    let tested = h.c.test(SARAH, "metabase", &id).await.unwrap();
    assert_eq!(tested.status, Status::Connected, "{:?}", tested.detail);
    assert_eq!(tested.account.as_deref(), Some("sarah@acme.example"));
}

#[tokio::test]
async fn a_flat_connection_moves_to_a_named_one_at_its_own_tier_and_nothing_is_lost() {
    let h = hub().await;
    // Kept before a service held several: Sarah's for all her agents,
    // Allan's own, and one saved for the whole hub.
    h.global.set(&mine("skills/linear/api_key"), GOOD).unwrap();
    h.global
        .set(&mine("skills/linear/default_team_id"), "team-1")
        .unwrap();
    h.allan.set(&mine("skills/linear/api_key"), GOOD2).unwrap();
    h.global.set("skills/datadog/api_key", "dd-api").unwrap();
    h.global.set("skills/datadog/app_key", "dd-app").unwrap();
    h.global.set("skills/datadog/site", "eu1").unwrap();
    h.global
        .set(&mine("skills/jira/site"), "acme.atlassian.net")
        .unwrap();
    h.global
        .set(&mine("skills/jira/email"), "sarah@acme.example")
        .unwrap();
    h.global
        .set(&mine("skills/jira/api_token"), "jira-token")
        .unwrap();
    // Email always kept its account at `default`.
    h.global
        .set(
            &mine("skills/email/accounts/default/username"),
            "sarah@acme.example",
        )
        .unwrap();
    h.global
        .set(&mine("skills/email/accounts/default/password"), "app-pass")
        .unwrap();
    // And Allan's own mailbox, at the same instance of his own tier.
    h.allan
        .set(
            &mine("skills/email/accounts/default/username"),
            "bot@acme.example",
        )
        .unwrap();
    h.allan
        .set(&mine("skills/email/accounts/default/password"), "bot-pass")
        .unwrap();
    // What Sarah's Linear's last check found goes with it.
    h.kept
        .put(
            &Checked {
                connector: "linear".into(),
                level: Level::All,
                instance: DEFAULT_INSTANCE.into(),
                person: SARAH.into(),
                account: Some("sarah@acme.example".into()),
                status: Status::Connected,
                detail: None,
                connected_at: Utc::now(),
                checked_at: Some(Utc::now()),
            },
            true,
        )
        .await
        .unwrap();

    let list = h.c.list(SARAH).await;
    let names = |id: &str| -> Vec<(String, String, VaultSource)> {
        connector(&list, id)
            .connections
            .iter()
            .map(|c| (c.id.clone(), c.name.clone(), c.source))
            .collect()
    };
    assert_eq!(
        names("linear"),
        [
            (
                "all~~default".into(),
                "sarah@acme.example".into(),
                VaultSource::GlobalPerUser
            ),
            // Allan's own takes a key of its own: `default` is Sarah's.
            (
                "agent~allan~linear".into(),
                "Linear".into(),
                VaultSource::AgentPerUser
            ),
        ]
    );
    assert_eq!(
        names("datadog"),
        [("all~~default".into(), "Eu1".into(), VaultSource::Global)]
    );
    assert_eq!(
        names("jira"),
        [(
            "all~~default".into(),
            "Acme".into(),
            VaultSource::GlobalPerUser
        )]
    );
    assert_eq!(
        names("email"),
        [
            (
                "all~~default".into(),
                "sarah@acme.example".into(),
                VaultSource::GlobalPerUser
            ),
            (
                "agent~allan~bot-acme-example".into(),
                "bot@acme.example".into(),
                VaultSource::AgentPerUser
            ),
        ]
    );
    assert_eq!(
        h.allan
            .get(&mine("skills/email/accounts/bot-acme-example/password"))
            .as_deref(),
        Some("bot-pass")
    );
    assert!(h
        .allan
        .get(&mine("skills/email/accounts/default/password"))
        .is_none());
    // Each moved at its tier, every value with it, the flat ones gone.
    assert_eq!(
        h.global.get(&mine(&ws("default", "api_key"))).as_deref(),
        Some(GOOD)
    );
    assert_eq!(
        h.allan.get(&mine(&ws("linear", "api_key"))).as_deref(),
        Some(GOOD2)
    );
    assert_eq!(
        h.global
            .get("skills/datadog/accounts/default/site")
            .as_deref(),
        Some("eu1")
    );
    assert_eq!(
        h.global
            .get(&mine("skills/jira/workspaces/default/site"))
            .as_deref(),
        Some("acme.atlassian.net")
    );
    for flat in [
        mine("skills/linear/api_key"),
        mine("skills/jira/api_token"),
        "skills/datadog/api_key".into(),
    ] {
        assert!(h.global.get(&flat).is_none(), "{flat} moved");
    }
    assert!(h.allan.get(&mine("skills/linear/api_key")).is_none());
    // A setting the adapter reads beside the key moves with it.
    assert_eq!(
        h.global
            .get(&mine(&ws("default", "default_team_id")))
            .as_deref(),
        Some("team-1")
    );
    assert!(h
        .global
        .get(&mine("skills/linear/default_team_id"))
        .is_none());
    let linear = &connector(&list, "linear").connections[0];
    assert_eq!(linear.account.as_deref(), Some("sarah@acme.example"));
    // Every agent it reaches loads it again.
    assert!(h
        .crew
        .reloaded()
        .contains(&("allan".into(), "linear".into())));

    // Again: nothing moves, nothing loads.
    let before = (keys(&h.global), keys(&h.allan));
    assert_eq!(h.c.list(SARAH).await.len(), catalog::CATALOG.len());
    assert_eq!(before, (keys(&h.global), keys(&h.allan)));
    assert!(h.crew.reloaded().is_empty());
}

#[tokio::test]
async fn a_flat_connection_never_moves_over_a_named_one() {
    let h = hub().await;
    h.global.set(&mine("skills/linear/api_key"), GOOD).unwrap();
    h.global
        .set(&mine(&ws("default", "api_key")), GOOD2)
        .unwrap();
    h.c.migrate(SARAH).await;
    assert_eq!(
        h.global.get(&mine(&ws("default", "api_key"))).as_deref(),
        Some(GOOD2)
    );
    assert_eq!(
        h.global.get(&mine("skills/linear/api_key")).as_deref(),
        Some(GOOD),
        "left as it was"
    );
}

/// What an agent used before the move is what it uses after: its own
/// connection, kept for it alone, never the person's in its place.
#[tokio::test]
async fn an_agents_own_connection_stays_the_one_it_uses() {
    let h = hub().await;
    h.global.set(&mine("skills/linear/api_key"), GOOD).unwrap();
    h.allan.set(&mine("skills/linear/api_key"), GOOD2).unwrap();
    h.ben.set(&mine("skills/linear/api_key"), GOOD2).unwrap();
    // Allan named his own; Ben chose none, so his own shadowed Sarah's.
    h.crew.bind(
        "allan",
        Binding {
            enabled: Some(vec!["linear".into()]),
            own: vec!["linear".into()],
            ..Default::default()
        },
    );
    h.c.migrate(SARAH).await;
    for agent in ["allan", "ben"] {
        let services = h.c.agent_picks(SARAH, agent).await.unwrap();
        let linear = services.iter().find(|s| s.id == "linear").unwrap();
        let used: Vec<(&str, bool)> = linear
            .connections
            .iter()
            .filter(|p| p.picked)
            .map(|p| (p.level, p.default))
            .collect();
        assert_eq!(used, [("agent", true)], "{agent} uses its own, as before");
    }
    // Each names its own: `default` is Sarah's for all her agents.
    let picked = |agent: &str| h.crew.configs.lock().unwrap()[agent].clone();
    assert!(
        picked("allan").contains("linear = [\"linear\"]"),
        "{}",
        picked("allan")
    );
    assert!(
        picked("ben").contains("linear = [\"linear-2\"]"),
        "{}",
        picked("ben")
    );
}

#[tokio::test]
async fn every_setting_an_adapter_reads_moves_with_its_connection() {
    let h = hub().await;
    for (path, value) in [
        ("skills/datadog/api_key", "dd-api"),
        ("skills/datadog/app_key", "dd-app"),
        ("skills/datadog/site", "eu1"),
        ("skills/datadog/base_url", "https://api.datadoghq.eu"),
        ("skills/metabase/api_key", "mb-key"),
        ("skills/metabase/site_url", "https://mb.acme.example"),
        ("skills/metabase/max_query_rows", "500"),
        ("skills/metabase/access_client_id", "test-client-id.access"),
        ("skills/metabase/access_client_secret", "not-a-real-secret"),
        ("skills/jira/api_token", "jira-token"),
        ("skills/jira/email", "sarah@acme.example"),
        ("skills/jira/site", "acme.atlassian.net"),
        ("skills/jira/default_project", "ACM"),
        ("skills/email/accounts/default/username", "bot@acme.example"),
        ("skills/email/accounts/default/password", "app-pass"),
        (
            "skills/email/accounts/default/imap_host",
            "imap.acme.example",
        ),
        ("skills/email/accounts/default/imap_port", "1993"),
    ] {
        h.allan.set(&mine(path), value).unwrap();
    }
    // The person's own Email for all agents takes `default`, so Allan's
    // moves to a key of its own, settings and all.
    h.global
        .set(&mine("skills/email/accounts/default/password"), "p")
        .unwrap();
    h.c.migrate(SARAH).await;
    let at = |path: &str| h.allan.get(&mine(path));
    assert_eq!(
        at("skills/datadog/accounts/default/base_url").as_deref(),
        Some("https://api.datadoghq.eu")
    );
    assert_eq!(
        at("skills/datadog/accounts/default/site").as_deref(),
        Some("eu1")
    );
    assert_eq!(
        at("skills/metabase/instances/default/max_query_rows").as_deref(),
        Some("500")
    );
    assert_eq!(
        at("skills/metabase/instances/default/access_client_id").as_deref(),
        Some("test-client-id.access")
    );
    assert_eq!(
        at("skills/metabase/instances/default/access_client_secret").as_deref(),
        Some("not-a-real-secret")
    );
    assert_eq!(
        at("skills/jira/workspaces/default/default_project").as_deref(),
        Some("ACM")
    );
    assert_eq!(
        at("skills/email/accounts/bot-acme-example/imap_port").as_deref(),
        Some("1993")
    );
    let left: Vec<String> = keys(&h.allan)
        .into_iter()
        .filter(|k| {
            !k.contains("/workspaces/") && !k.contains("/accounts/") && !k.contains("/instances/")
        })
        .collect();
    assert!(left.is_empty(), "nothing flat is left: {left:?}");
    assert!(at("skills/email/accounts/default/imap_port").is_none());
}

#[tokio::test]
async fn no_field_of_a_named_connection_is_ever_written_over() {
    let h = hub().await;
    // A named connection, and a flat one beside it that disagrees on its
    // site and has a setting the named one doesn't.
    for (f, v) in [
        ("api_token", "named-token"),
        ("email", "sarah@acme.example"),
        ("site", "acme.atlassian.net"),
    ] {
        h.global
            .set(&mine(&format!("skills/jira/workspaces/default/{f}")), v)
            .unwrap();
    }
    h.global
        .set(&mine("skills/jira/api_token"), "named-token")
        .unwrap();
    h.global
        .set(&mine("skills/jira/site"), "elsewhere.atlassian.net")
        .unwrap();
    h.global
        .set(&mine("skills/jira/default_project"), "ACM")
        .unwrap();
    h.c.migrate(SARAH).await;
    let at = |path: &str| h.global.get(&mine(path));
    // The named one's site stays; the flat one that differs stays beside it.
    assert_eq!(
        at("skills/jira/workspaces/default/site").as_deref(),
        Some("acme.atlassian.net")
    );
    assert_eq!(
        at("skills/jira/site").as_deref(),
        Some("elsewhere.atlassian.net")
    );
    // The same key is the same: the flat copy goes.
    assert!(at("skills/jira/api_token").is_none());
    // A setting written flat after the move joins the connection.
    assert_eq!(
        at("skills/jira/workspaces/default/default_project").as_deref(),
        Some("ACM")
    );
    assert!(at("skills/jira/default_project").is_none());
    // Again: nothing changes.
    let before = keys(&h.global);
    h.c.migrate(SARAH).await;
    assert_eq!(before, keys(&h.global));
}

#[tokio::test]
async fn a_flat_setting_without_the_connections_key_never_joins_it() {
    let h = hub().await;
    for (f, v) in [
        ("api_token", "named-token"),
        ("email", "sarah@acme.example"),
        ("site", "acme.atlassian.net"),
    ] {
        h.global
            .set(&mine(&format!("skills/jira/workspaces/default/{f}")), v)
            .unwrap();
    }
    // Another account's key and project: neither joins.
    h.global
        .set(&mine("skills/jira/api_token"), "other-token")
        .unwrap();
    h.global
        .set(&mine("skills/jira/default_project"), "OTH")
        .unwrap();
    h.c.migrate(SARAH).await;
    let at = |path: &str| h.global.get(&mine(path));
    assert!(at("skills/jira/workspaces/default/default_project").is_none());
    assert_eq!(at("skills/jira/api_token").as_deref(), Some("other-token"));
    // A project alone, with no key: it doesn't join either.
    h.global.delete(&mine("skills/jira/api_token")).unwrap();
    h.c.migrate(SARAH).await;
    assert!(at("skills/jira/workspaces/default/default_project").is_none());
    assert_eq!(at("skills/jira/default_project").as_deref(), Some("OTH"));
}

#[tokio::test]
async fn a_persons_and_a_shared_flat_connection_both_show() {
    let h = hub().await;
    h.global.set(&mine("skills/linear/api_key"), GOOD).unwrap();
    h.global.set("skills/linear/api_key", GOOD2).unwrap();
    // The level's one check was of the person's own, which stays `default`.
    h.kept
        .put(
            &Checked {
                connector: "linear".into(),
                level: Level::All,
                instance: DEFAULT_INSTANCE.into(),
                person: SARAH.into(),
                account: Some("sarah".into()),
                status: Status::Connected,
                detail: None,
                connected_at: Utc::now(),
                checked_at: None,
            },
            true,
        )
        .await
        .unwrap();
    let list = h.c.list(SARAH).await;
    let linear = connector(&list, "linear");
    assert_eq!(linear.connections[0].account.as_deref(), Some("sarah"));
    assert_eq!(
        linear.connections[1].account, None,
        "the shared one beside it never takes the person's check"
    );
    let ids: Vec<(String, VaultSource)> = connector(&list, "linear")
        .connections
        .iter()
        .map(|c| (c.id.clone(), c.source))
        .collect();
    assert_eq!(
        ids,
        [
            ("all~~default".into(), VaultSource::GlobalPerUser),
            ("all~~shared".into(), VaultSource::Global),
        ]
    );
    // A key written by hand that isn't one Connections makes isn't listed
    // or named.
    h.global
        .set(&mine("skills/linear/workspaces/Odd_Key/api_key"), GOOD)
        .unwrap();
    assert_eq!(
        connector(&h.c.list(SARAH).await, "linear")
            .connections
            .len(),
        2
    );
    for id in ["all~~Odd_Key", "all~~a b", "all~~../x"] {
        assert!(h.c.test(SARAH, "linear", id).await.is_err(), "{id}");
    }
}

#[tokio::test]
async fn a_new_connection_moves_a_flat_one_first_so_neither_is_hidden() {
    let h = hub().await;
    h.global.set(&mine("skills/linear/api_key"), GOOD).unwrap();
    let globex = h.connect(Some("Globex"), "all", None, GOOD2).await.unwrap();
    assert_eq!(globex.id, "all~~globex");
    assert_eq!(
        h.global.get(&mine(&ws("default", "api_key"))).as_deref(),
        Some(GOOD)
    );
    assert!(h.global.get(&mine("skills/linear/api_key")).is_none());
}

#[tokio::test]
async fn a_flat_github_moves_to_default_and_still_reaches_every_owner() {
    use workforce_agent::runner::CURRENT_USER_ID;
    let h = hub().await;
    h.global.set(&mine("skills/github/token"), GH_GOOD).unwrap();
    h.c.migrate(SARAH).await;
    assert_eq!(
        h.global
            .get(&mine("skills/github/workspaces/default/token"))
            .as_deref(),
        Some(GH_GOOD)
    );
    // git, Claude Code and an open bench still find it, for any owner.
    let (a, g) = (Arc::clone(&h.allan), Arc::clone(&h.global));
    CURRENT_USER_ID
        .scope(SARAH.to_string(), async move {
            for owner in ["acme", "globex"] {
                assert_eq!(
                    workforce_hub_core::user_config::resolve_github_pat(Some(&a), Some(&g), owner)
                        .as_deref(),
                    Some(GH_GOOD),
                    "{owner}"
                );
            }
            assert_eq!(
                workforce_hub_core::user_config::resolve_bench_github_pat(Some(&a), Some(&g))
                    .as_deref(),
                Some(GH_GOOD)
            );
        })
        .await;
    // Beside the person's, a bare shared flat token can't be `default`, so
    // it stays flat, where git still reads it.
    let h2 = hub().await;
    h2.global
        .set(&mine("skills/github/token"), GH_GOOD)
        .unwrap();
    h2.global
        .set("skills/github/token", "ghp_sharedtoken0123456789abcd")
        .unwrap();
    h2.c.migrate(SARAH).await;
    assert_eq!(
        h2.global.get("skills/github/token").as_deref(),
        Some("ghp_sharedtoken0123456789abcd")
    );
    assert!(h2
        .global
        .get(&mine("skills/github/workspaces/default/token"))
        .is_some());
    assert!(keys(&h2.global)
        .iter()
        .all(|k| !k.contains("workspaces/shared")));
    // An agent's own flat token stays its `default` too.
    let h = hub().await;
    h.allan.set(&mine("skills/github/token"), GH_GOOD).unwrap();
    h.kept
        .put(
            &Checked {
                connector: "github".into(),
                level: Level::Agent("allan".into()),
                instance: DEFAULT_INSTANCE.into(),
                person: SARAH.into(),
                account: Some("sarah".into()),
                status: Status::Connected,
                detail: None,
                connected_at: Utc::now(),
                checked_at: None,
            },
            true,
        )
        .await
        .unwrap();
    let list = h.c.list(SARAH).await;
    let github = &connector(&list, "github").connections[0];
    assert_eq!(github.id, "agent~allan~default");
    assert_eq!(
        github.account.as_deref(),
        Some("sarah"),
        "its check moved too"
    );
}

/// The adapter still builds its client from a connection that moved.
#[test]
fn a_moved_connection_still_loads() {
    use workforce_agent::runner::CURRENT_USER_ID;
    let dir = tempfile::tempdir().unwrap();
    let agent = Arc::new(Vault::open(&dir.path().join("agent"), "test-key").unwrap());
    let global = Arc::new(Vault::open(&dir.path().join("global"), "test-key").unwrap());
    let runtime = tokio::runtime::Runtime::new().unwrap();
    global.set(&mine("skills/linear/api_key"), GOOD).unwrap();
    let crew = Arc::new(Crew::default());
    *crew.agents.lock().unwrap() = vec![Agent {
        id: "allan".into(),
        name: "Allan".into(),
        vault: Some(Arc::clone(&agent)),
        binding: Binding::default(),
    }];
    let c = Connections::new(
        Some(Arc::clone(&global)),
        crew,
        Arc::new(Kept::default()),
        Services::live(),
        None,
        String::new(),
    );
    runtime.block_on(c.migrate(SARAH));
    assert!(global.get(&mine(&ws("default", "api_key"))).is_some());
    let config = workforce_common::settings::WorkforceConfig::default();
    let tools = workforce_agent::tools::registry::ToolRegistry::new();
    let state = crate::skill_state::SkillStateTracker::new();
    CURRENT_USER_ID
        .sync_scope(SARAH.to_string(), || {
            crate::skill_loader::load_and_track(
                "linear",
                Arc::clone(&agent),
                Some(Arc::clone(&global)),
                &config,
                &tools,
                &state,
                "vault",
            )
        })
        .unwrap();
    assert!(tools.has("linear.issue_get"));
}

#[tokio::test]
async fn a_disconnect_removes_only_its_tier() {
    let h = hub().await;
    h.connect(None, "all", None, GOOD).await.unwrap();
    let allan = h.connect(None, "agent", Some("allan"), GOOD).await.unwrap();
    assert_eq!(allan.id, "agent~allan~sarah-acme-example");
    h.allan
        .set(&ws("sarah-acme-example", "api_key"), "agent-shared")
        .unwrap();
    h.c.disconnect(SARAH, "linear", &allan.id).await.unwrap();
    assert_eq!(
        keys(&h.allan),
        [ws("sarah-acme-example", "api_key")],
        "the bare agent key stays"
    );
    assert_eq!(keys(&h.global).len(), 2);
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
    h.c.keep(c, &Level::All, "work", SARAH, &fields, Some("Work"))
        .unwrap();
    fields.remove("smtp_host");
    assert_eq!(
        h.c.keep(c, &Level::All, "work", SARAH, &fields, None)
            .unwrap(),
        tier
    );
    assert!(h
        .global
        .get(&mine("skills/email/accounts/work/smtp_host"))
        .is_none());
    assert!(h
        .global
        .get(&mine("skills/email/accounts/work/imap_host"))
        .is_some());
    assert_eq!(
        h.global
            .get(&mine("skills/email/accounts/work/label"))
            .as_deref(),
        Some("Work")
    );
}

#[tokio::test]
async fn what_is_asked_for_is_checked_first() {
    let h = hub().await;
    for (connector, name, level, agent, fields, want) in [
        ("nope", None, "all", None, linear(GOOD), "no connector"),
        (
            "linear",
            None,
            "agent",
            Some("zed"),
            linear(GOOD),
            "no agent",
        ),
        ("linear", None, "agent", None, linear(GOOD), "which agent"),
        ("linear", None, "all", None, BTreeMap::new(), "is needed"),
        (
            "linear",
            None,
            "all",
            None,
            BTreeMap::from([("x".into(), "y".into())]),
            "no field x",
        ),
        ("google", None, "all", None, BTreeMap::new(), "signing in"),
        (
            "linear",
            Some("a\nb"),
            "all",
            None,
            linear(GOOD),
            "one line",
        ),
    ] {
        let e =
            h.c.connect(SARAH, connector, name, level, agent, fields)
                .await
                .unwrap_err();
        assert!(format!("{e:?}").contains(want), "{connector}: {e:?}");
    }
    for id in ["", "all~~", "all~~a/b", "nope~x~y", "agent~zed~acme"] {
        assert!(
            h.c.test(SARAH, "linear", id).await.is_err(),
            "{id} names no connection"
        );
    }
    assert!(keys(&h.global).is_empty());
}

#[test]
fn a_name_makes_a_key_of_letters_digits_and_dashes() {
    assert_eq!(slug("Acme Corp"), "acme-corp");
    assert_eq!(slug("  Globex — EU / prod "), "globex-eu-prod");
    assert_eq!(slug("sarah@acme.example"), "sarah-acme-example");
    assert_eq!(slug("!!!"), "");
    assert!(slug(&"a".repeat(100)).len() <= 40);
    assert_eq!(
        site_name("https://acme.atlassian.net/").as_deref(),
        Some("Acme")
    );
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
        h.c.start(SARAH, "github", None, "agent", Some("allan"))
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
    assert_eq!(
        keys(&h.allan),
        [
            mine("skills/github/workspaces/default/label"),
            mine("skills/github/workspaces/default/token")
        ],
        "a first GitHub connection is the default, which git uses for any owner"
    );
    assert!(keys(&h.global).is_empty());
    assert!(h.c.flow("someone-else", &started.flow).is_none());
}

#[tokio::test]
async fn a_github_sign_in_turned_down_or_cancelled_keeps_nothing() {
    let h = hub().await;
    *h.github.outcome.lock().unwrap() = "denied";
    let started = h.c.start(SARAH, "github", None, "all", None).await.unwrap();
    assert_eq!(settled(&h, &started.flow).await, FlowState::Cancelled);

    *h.github.outcome.lock().unwrap() = "pending";
    let started = h.c.start(SARAH, "github", None, "all", None).await.unwrap();
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
        h.c.start(SARAH, "github", None, "all", None).await,
        Err(Refusal::Unavailable(_))
    ));
    let token = BTreeMap::from([("token".to_string(), GH_GOOD.to_string())]);
    assert_eq!(
        h.c.connect(SARAH, "github", None, "all", None, token)
            .await
            .unwrap()
            .account
            .as_deref(),
        Some("sarah")
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

#[tokio::test]
async fn an_agent_uses_the_connections_it_is_given_one_by_one() {
    let h = hub().await;
    let acme = h.connect(Some("Acme"), "all", None, GOOD).await.unwrap();
    let globex = h.connect(Some("Globex"), "all", None, GOOD2).await.unwrap();
    let bens = h
        .connect(Some("Initech"), "agent", Some("ben"), GOOD)
        .await
        .unwrap();
    let services = h.c.agent_picks(SARAH, "allan").await.unwrap();
    let linear = services.iter().find(|s| s.id == "linear").unwrap();
    // Before anything is chosen: what reaches it today. Ben's own isn't
    // Allan's to use.
    assert!(!linear.chosen && linear.read_only_each);
    assert_eq!(
        linear
            .connections
            .iter()
            .map(|p| (p.name.as_str(), p.picked, p.write, p.default))
            .collect::<Vec<_>>(),
        [("Acme", true, true, true), ("Globex", true, true, false)]
    );

    let services =
        h.c.set_picks(
            SARAH,
            "allan",
            picks::PicksBody {
                service: "linear".into(),
                connections: vec![
                    picks::Use {
                        id: acme.id.clone(),
                        write: false,
                    },
                    picks::Use {
                        id: globex.id.clone(),
                        write: true,
                    },
                ],
                default: Some(globex.id.clone()),
            },
        )
        .await
        .unwrap();
    let linear = services.iter().find(|s| s.id == "linear").unwrap();
    assert!(linear.chosen);
    assert_eq!(
        linear
            .connections
            .iter()
            .map(|p| (p.name.as_str(), p.picked, p.write, p.default))
            .collect::<Vec<_>>(),
        [("Acme", true, false, false), ("Globex", true, true, true)]
    );
    let config = h.crew.configs.lock().unwrap()["allan"].clone();
    assert!(
        config.contains("linear = [\"globex-2\", \"default\"]")
            || config.contains("linear = [\"globex\", \"default\"]"),
        "{config}"
    );
    assert!(config.contains("linear:default"), "{config}");
    // Every other connector stays on.
    assert!(config.contains("\"github\""), "{config}");

    // Used by follows it.
    let list = h.c.list(SARAH).await;
    let users: Vec<Vec<&str>> = connector(&list, "linear")
        .connections
        .iter()
        .map(|c| c.used_by.iter().map(|u| u.agent.as_str()).collect())
        .collect();
    assert_eq!(
        users,
        [vec!["allan", "ben"], vec!["allan", "ben"], vec!["ben"]]
    );

    // Another agent's own, a default it doesn't use, and one gone are refused.
    for (connections, default, want) in [
        (vec![bens.id.clone()], None, "another agent's own"),
        (
            vec![acme.id.clone()],
            Some(globex.id.clone()),
            "one of the connections",
        ),
        (vec!["all~~gone".to_string()], None, "isn't there any more"),
    ] {
        let e =
            h.c.set_picks(
                SARAH,
                "allan",
                picks::PicksBody {
                    service: "linear".into(),
                    connections: connections
                        .into_iter()
                        .map(|id| picks::Use { id, write: true })
                        .collect(),
                    default,
                },
            )
            .await
            .unwrap_err();
        assert!(format!("{e:?}").contains(want), "{e:?}");
    }

    // A connection disconnected is no longer named.
    h.c.disconnect(SARAH, "linear", &globex.id).await.unwrap();
    let config = h.crew.configs.lock().unwrap()["allan"].clone();
    assert!(config.contains("linear = [\"default\"]"), "{config}");
}

/// The tools of an agent reach exactly the connections it uses: its
/// adapter builds only those, a write to one that only reads is refused
/// wherever the instance is resolved, and an environment can't reach past
/// them.
#[test]
fn an_agents_tools_reach_exactly_its_connections() {
    use workforce_agent::runner::{ACTIVE_ENVIRONMENT, ACTIVE_SKILL_INSTANCES, CURRENT_USER_ID};
    let dir = tempfile::tempdir().unwrap();
    let agent = Arc::new(Vault::open(&dir.path().join("agent"), "test-key").unwrap());
    let global = Arc::new(Vault::open(&dir.path().join("global"), "test-key").unwrap());
    for (key, value) in [("acme", GOOD), ("globex", GOOD2), ("initech", GOOD)] {
        global.set(&mine(&ws(key, "api_key")), value).unwrap();
        global
            .set(&mine(&ws(key, "label")), &key.to_uppercase())
            .unwrap();
    }
    let workspace = dir.path().join("allan");
    std::fs::create_dir_all(&workspace).unwrap();
    std::fs::write(
        workspace.join(workforce_common::config_files::WORKSPACE_CONFIG),
        "[skills]\nenabled = [\"linear\"]\nread_only = [\"linear:globex\"]\n\n[skills.connections]\nlinear = [\"acme\", \"globex\"]\n",
    )
    .unwrap();
    let config = workforce_common::settings::WorkforceConfig::default();
    let tools = workforce_agent::tools::registry::ToolRegistry::new();
    tools
        .credential_pins()
        .fill(Arc::new(crate::connector_pins::AgentPins::new(workspace)));
    let loaded = CURRENT_USER_ID
        .sync_scope(SARAH.to_string(), || {
            crate::skill_loader::load_skill_from_vault(
                "linear",
                Arc::clone(&agent),
                Some(Arc::clone(&global)),
                &config,
                &tools,
            )
        })
        .unwrap();
    let hint = loaded.context_hint.unwrap();
    assert!(hint.contains("\"acme\" is ACME (default)"), "{hint}");
    assert!(hint.contains("\"globex\" is GLOBEX (read only)"), "{hint}");
    assert!(!hint.contains("INITECH"), "{hint}");
    assert!(hint.contains("ask"), "{hint}");

    let runtime = tokio::runtime::Runtime::new().unwrap();
    let call = |tool: &str, input: serde_json::Value| {
        let t = tools.get(tool).unwrap();
        runtime.block_on(CURRENT_USER_ID.scope(SARAH.to_string(), async move {
            let r = t.execute(input).await;
            match r {
                Ok(r) => format!("{:?} {:?}", r.output, r.error),
                Err(e) => format!("{e:#}"),
            }
        }))
    };
    // One it doesn't use isn't there to reach.
    let said = call("linear.issue_get", json!({ "workspace": "initech" }));
    assert!(
        said.contains("initech") && said.contains("Unknown"),
        "{said}"
    );
    // A write to one that only reads is refused, by name or by default.
    let said = call("linear.issue_create", json!({ "workspace": "globex" }));
    assert!(said.contains("only reads"), "{said}");
    // Acme takes it, and goes on to ask for its title, with no call made.
    let said = call("linear.issue_create", json!({}));
    assert!(
        said.contains("title") && !said.contains("only reads"),
        "{said}"
    );
    // A read of the one that only reads goes ahead.
    let said = call("linear.issue_get", json!({ "workspace": "globex" }));
    assert!(!said.contains("only reads"), "{said}");
    // An environment can only narrow it: one pointing past it is refused.
    let pins: workforce_agent::llm::environment::SkillPins =
        [("linear".to_string(), vec!["initech".to_string()])].into();
    let t = tools.get("linear.issue_get").unwrap();
    let said = runtime.block_on(CURRENT_USER_ID.scope(
        SARAH.to_string(),
        ACTIVE_ENVIRONMENT.scope(
            "work".to_string(),
            ACTIVE_SKILL_INSTANCES.scope(pins, async move {
                match t.execute(json!({})).await {
                    Ok(r) => format!("{:?} {:?}", r.output, r.error),
                    Err(e) => format!("{e:#}"),
                }
            }),
        ),
    ));
    assert!(said.contains("not configured"), "{said}");
}

mod setups {
    use super::*;
    use setup::{Caller, Callers, Ended, NowKind, Setups, StepState, TabAt, Tabs, Way};
    use workforce_common::secret::Secret;

    struct People;

    impl Callers for People {
        fn holds_the_hub(&self, person: &str) -> bool {
            person == SARAH
        }
        fn oversees(&self, agent: &str) -> bool {
            agent == "ben"
        }
        fn agent_named(&self, name: &str) -> Option<String> {
            ["allan", "ben"]
                .into_iter()
                .find(|a| a.eq_ignore_ascii_case(name))
                .map(str::to_string)
        }
    }

    fn caller(agent: &str) -> Caller {
        Caller {
            person: SARAH.into(),
            agent: agent.into(),
            session: Some(format!("s-{agent}")),
            session_key: Some(format!("desk:{agent}")),
            overseer: agent == "ben",
        }
    }

    async fn setups() -> (Hub, Arc<Setups>) {
        let h = hub().await;
        let s = Arc::new(Setups::new(Arc::clone(&h.c), Arc::new(People)));
        (h, s)
    }

    const LINEAR_PAGE: (&str, &str) = ("https://linear.app", "/settings/account/security");

    /// What `browser.capture_key` reaches through Desk's browser hooks: the
    /// turn's own caller, the setup's page as the browser checks it, and the
    /// key kept from where the extension says it read it.
    #[tokio::test]
    async fn the_browser_reaches_a_setup_only_from_its_own_turn() {
        use workforce_agent::runner::{
            CURRENT_AGENT_ID, CURRENT_SESSION_ID, CURRENT_SESSION_KEY, CURRENT_USER_ID,
        };
        use workforce_skills::adapters::browser::keys;
        setup::register_key_pages();
        let (_h, s) = setups().await;
        let allan = caller("allan");
        s.start(&allan, "linear", Some("Acme"), None, false)
            .await
            .unwrap();
        let in_turn = |person: &'static str, key: &'static str, f: Box<dyn FnOnce() + Send>| {
            CURRENT_USER_ID.sync_scope(person.to_string(), || {
                CURRENT_AGENT_ID.sync_scope("allan".to_string(), || {
                    CURRENT_SESSION_ID.sync_scope("s-allan".to_string(), || {
                        CURRENT_SESSION_KEY.sync_scope(key.to_string(), f)
                    })
                })
            })
        };
        // Outside a turn, or when the browser's word disagrees with the
        // turn's, there is no caller and so no page.
        assert!(s.turn_caller(SARAH, "allan", "desk:allan").is_none());
        let s2 = Arc::clone(&s);
        in_turn(
            SARAH,
            "desk:allan",
            Box::new(move || {
                assert!(s2.turn_caller("mallory", "allan", "desk:allan").is_none());
                assert!(s2.turn_caller(SARAH, "allan", "desk:other").is_none());
                let who = s2.turn_caller(SARAH, "allan", "desk:allan").unwrap();
                assert_eq!(who.session.as_deref(), Some("s-allan"));
                // Allan's turn is never Ben's, and Ben runs no setup here:
                // nothing to read.
                assert!(s2.turn_caller(SARAH, "ben", "desk:allan").is_none());
                assert!(s2.browser_page(&caller("ben"), "linear").is_none());
                // Linear's page, with its workspace's segment, and registered so
                // the browser hides it and takes no picture of it.
                let page = s2.browser_page(&who, "linear").unwrap();
                let at = "https://linear.app/acme/settings/account/security";
                assert!(keys::matches(&page, at));
                assert!(keys::is_key_page(at));
                assert!(keys::shaped(&page, GOOD));
                assert!(!keys::shaped(&page, "lin_api_x"));
                assert_eq!(
                    keys::paths_at(&page, at)[0],
                    "/acme/settings/account/security"
                );
                // Read where the extension says, the key is held; read
                // elsewhere, it is dropped.
                assert!(s2
                    .keep_key(
                        &who,
                        "linear",
                        None,
                        "https://linear.app",
                        "/acme/settings/api-x",
                        Secret::new(GOOD.into())
                    )
                    .is_err());
                let kept = s2
                    .keep_key(
                        &who,
                        "linear",
                        None,
                        "https://linear.app",
                        "/acme/settings/account/security",
                        Secret::new(GOOD.into()),
                    )
                    .unwrap();
                assert!(kept.ready && kept.test.starts_with("kept"), "{}", kept.test);
                assert!(!format!("{kept:?}").contains(GOOD));
            }),
        );
    }

    #[test]
    fn every_known_key_page_is_hidden_before_a_setup_starts() {
        use workforce_skills::adapters::browser::keys;
        setup::register_key_pages();
        for at in [
            "https://id.atlassian.com/manage-profile/security/api-tokens",
            "https://linear.app/acme/settings/api",
            "https://github.com/settings/personal-access-tokens/new",
            "https://app.datadoghq.eu/organization-settings/api-keys",
            "https://app.ddog-gov.com/personal-settings/application-keys",
            "https://app.banksync.io/wsAbc123/settings/developers",
            "https://app.banksync.io/wsAbc123/settings/developers/api-keys",
            // A Metabase's address is each person's own: its key page is
            // hidden wherever it is, before any setup has kept one.
            "https://bi.globex.example/admin/settings/authentication/api-keys",
            "https://10.1.2.3:3000/admin/settings/authentication/api-keys",
        ] {
            assert!(keys::is_key_page(at), "{at}");
        }
        assert!(!keys::is_key_page("https://linear.app/acme/issue/ACM-1"));
        assert!(!keys::is_key_page("https://app.banksync.io/wsAbc123/banks"));
        assert!(!keys::is_key_page(
            "https://bi.globex.example/admin/settings/authentication"
        ));
        // The extension refuses pictures by exact origin, so Metabase's
        // page anywhere is never sent it: the hub throws the picture away.
        assert!(!keys::refusals()
            .to_string()
            .contains("/admin/settings/authentication/api-keys"));
    }

    #[tokio::test]
    async fn an_agent_sets_itself_up_and_the_key_never_comes_back() {
        let cache = Arc::new(crate::secret_cache::InMemorySecretCache::with_defaults());
        let _cache = crate::secret_cache::testing::GlobalCache::install(cache.clone());
        let (h, s) = setups().await;
        let allan = caller("allan");
        let started = s
            .start(&allan, "linear", Some("Acme"), None, false)
            .await
            .unwrap();
        assert_eq!(
            started.steps.iter().map(|x| x.id).collect::<Vec<_>>(),
            ["page", "sign_in", "key", "check"]
        );
        assert!(s.next(&started).contains("browser.capture_key"));
        // The browser asks where the key may be read: this setup's page.
        let page = s.key_page(&allan, "linear").unwrap();
        assert_eq!(page.origin, "https://linear.app");
        assert_eq!(page.fields, ["api_key"]);
        assert!(
            s.key_page(&caller("ben"), "linear").is_none(),
            "Ben runs none"
        );
        // A secret is never kept by `keep`.
        assert!(s
            .keep(&allan, &started.id, "api_key", GOOD)
            .unwrap_err()
            .contains("browser.capture_key"));

        let kept = s
            .keep_key(
                &allan,
                "linear",
                None,
                LINEAR_PAGE.0,
                LINEAR_PAGE.1,
                Secret::new(GOOD.into()),
            )
            .unwrap();
        // Held, not sent: the check is connections.test's, which asks.
        assert!(kept.ready && kept.test.starts_with("kept"), "{}", kept.test);
        assert_eq!(kept.length, GOOD.len());
        assert!(keys(&h.allan).is_empty(), "nothing kept before the check");
        s.test(&allan, &started.id).await.unwrap();
        let said = serde_json::to_string(&kept).unwrap();
        assert!(
            !said.contains(&GOOD[..8]) && !said.contains(&GOOD[GOOD.len() - 4..]),
            "{said}"
        );
        // Should it come back into view, it reaches the model as a marker.
        use workforce_common::secret_marker::SecretCache;
        let looked = cache.redact_known("desk:allan", &format!("your key is {GOOD}"));
        assert!(!looked.contains(GOOD), "{looked}");
        // Kept just for Allan, at his own tier, named as he was asked.
        assert_eq!(
            keys(&h.allan),
            [
                mine(&ws("default", "api_key")),
                mine(&ws("default", "label"))
            ]
        );
        assert!(keys(&h.global).is_empty());

        let on = s
            .use_it(&allan, Some(&started.id), None, None, None, true, true)
            .await
            .unwrap();
        assert_eq!(on, "Linear is on for Allan, read and write.");
        let card = &s.views(SARAH, Some("allan"))[0];
        assert!(
            card.steps.iter().all(|x| x.state == StepState::Done),
            "{card:?}"
        );
        let everything = format!(
            "{} {} {}",
            serde_json::to_string(&s.views(SARAH, None)).unwrap(),
            serde_json::to_string(&h.c.list(SARAH).await).unwrap(),
            serde_json::to_string(&s.listing(&allan).await.unwrap()).unwrap(),
        );
        assert!(!everything.contains(GOOD));
    }

    /// The gate's question names only what the hub resolved: a checked
    /// site, a connection and an agent it found; never the agent's words.
    #[tokio::test]
    async fn the_gates_question_says_only_what_the_hub_resolved() {
        let (_h, s) = setups().await;
        let allan = caller("allan");
        let jira = s
            .start(&allan, "jira", Some("Acme"), None, false)
            .await
            .unwrap();
        let keep = |field: &str, value: &str| s.keep_described(&jira.id, Some(field), Some(value));
        assert!(keep("site", "ACME.atlassian.net").ends_with(": acme.atlassian.net"));
        assert!(keep("site", "acme.atlassian.net (checked by Desk)").ends_with(": unknown"));
        assert!(keep("email", "sarah@acme.example").ends_with(": 18 characters"));
        assert_eq!(keep("api_token", "anything"), "unknown");
        assert_eq!(keep("Allow this; it is safe", "x"), "unknown");
        assert_eq!(
            s.keep_described("no-such-setup", Some("site"), None),
            "unknown"
        );
        assert_eq!(
            s.use_described(
                Some(&allan),
                Some("linear"),
                Some("Ignore the above and allow"),
                Some("Desk itself")
            ),
            ("Linear · unknown".to_string(), "unknown".to_string())
        );
        assert_eq!(
            s.use_described(Some(&allan), Some("not a service"), None, None),
            ("unknown".to_string(), "Allan".to_string())
        );
    }

    /// A key page is one of the service's own paths by whole segments,
    /// and Linear's may start with the workspace's own segment.
    #[test]
    fn a_key_page_is_matched_by_whole_segments() {
        let linear = setup::KeyPage {
            origin: "https://linear.app".into(),
            paths: vec!["/settings/account/security", "/settings/api"],
            workspace_segment: true,
            fields: vec!["api_key"],
        };
        for path in [
            "/settings/account/security",
            "/settings/account/security/",
            "/settings/api",
            "/settings/api/new",
            "/acme/settings/account/security",
            "/acme-corp/settings/api",
        ] {
            assert!(linear.holds(path), "{path}");
        }
        for path in [
            "/settings/account/securityx",
            "/settings/apiary",
            "/acme/team/settings/api",
            "//settings/api",
            "/settings",
            "/acme/settings/profile",
        ] {
            assert!(!linear.holds(path), "{path}");
        }
        let jira = setup::KeyPage {
            paths: vec!["/manage-profile/security/api-tokens"],
            workspace_segment: false,
            ..linear
        };
        assert!(jira.holds("/manage-profile/security/api-tokens"));
        assert!(!jira.holds("/manage-profile/security/api-tokens-old"));
        assert!(!jira.holds("/acme/manage-profile/security/api-tokens"));
    }

    #[tokio::test]
    async fn linears_key_is_read_off_its_workspaces_own_settings_page() {
        let (_h, s) = setups().await;
        let allan = caller("allan");
        s.start(&allan, "linear", None, None, false).await.unwrap();
        let kept = s
            .keep_key(
                &allan,
                "linear",
                None,
                "https://linear.app",
                "/acme/settings/account/security",
                Secret::new(GOOD.into()),
            )
            .unwrap();
        assert!(kept.ready && kept.test.starts_with("kept"), "{}", kept.test);
    }

    #[tokio::test]
    async fn a_key_off_another_page_not_shaped_as_a_key_or_refused_keeps_nothing() {
        let (h, s) = setups().await;
        let allan = caller("allan");
        let started = s.start(&allan, "linear", None, None, false).await.unwrap();
        for (origin, path, value, want) in [
            ("https://evil.example", LINEAR_PAGE.1, GOOD, "key page"),
            (LINEAR_PAGE.0, "/settings/profile", GOOD, "key page"),
            (
                LINEAR_PAGE.0,
                "/settings/account/securityx",
                GOOD,
                "key page",
            ),
            (
                LINEAR_PAGE.0,
                LINEAR_PAGE.1,
                "not a key",
                "doesn't look like",
            ),
        ] {
            let e = s
                .keep_key(
                    &allan,
                    "linear",
                    None,
                    origin,
                    path,
                    Secret::new(value.into()),
                )
                .unwrap_err();
            assert!(e.contains(want), "{e}");
        }
        // The service refuses it: nothing kept, the agent isn't told its
        // words, the card is.
        let kept = s
            .keep_key(
                &allan,
                "linear",
                None,
                LINEAR_PAGE.0,
                LINEAR_PAGE.1,
                Secret::new(BAD.into()),
            )
            .unwrap();
        assert!(kept.ready && kept.test.starts_with("kept"), "{}", kept.test);
        s.test(&allan, &started.id).await.unwrap_err();
        let e = s.test(&allan, &started.id).await.unwrap_err();
        assert!(e.contains("Read the key"), "the key was dropped: {e}");
        assert!(keys(&h.allan).is_empty());
        let card = &s.views(SARAH, None)[0];
        let check = card.steps.iter().find(|x| x.id == "check").unwrap();
        assert_eq!(check.state, StepState::Failed);
        assert!(!format!("{card:?}").contains(BAD));
        // No setup: nothing read, nothing kept.
        assert!(s
            .keep_key(
                &caller("ben"),
                "linear",
                None,
                LINEAR_PAGE.0,
                LINEAR_PAGE.1,
                Secret::new(GOOD.into())
            )
            .unwrap_err()
            .contains("No linear setup"));
    }

    #[tokio::test]
    async fn a_setup_is_its_own_agents_and_conversations_and_the_rest_is_the_overseers() {
        let (h, s) = setups().await;
        let allan = caller("allan");
        for whom in ["all", "ben"] {
            let e = s
                .start(&allan, "linear", None, Some(whom), false)
                .await
                .unwrap_err();
            assert!(e.contains("overseer"), "{whom}: {e}");
        }
        let started = s.start(&allan, "linear", None, None, false).await.unwrap();
        let other = Caller {
            session: Some("elsewhere".into()),
            ..caller("allan")
        };
        for who in [caller("ben"), other] {
            assert!(s
                .step(&who, &started.id, "page", StepState::Done, None)
                .unwrap_err()
                .contains("no such setup"));
        }
        assert!(s
            .step(&allan, &started.id, "check", StepState::Done, None)
            .is_err());
        // No one but the person, and never outside a conversation.
        let stranger = Caller {
            person: "someone-else".into(),
            ..caller("allan")
        };
        let sessionless = Caller {
            session: None,
            ..caller("allan")
        };
        for who in [stranger, sessionless] {
            assert!(s.start(&who, "linear", None, None, false).await.is_err());
            assert!(s.key_page(&who, "linear").is_none());
        }
        for by_hand in ["email", "slack"] {
            let e = s
                .start(&allan, by_hand, None, None, false)
                .await
                .unwrap_err();
            assert!(e.contains("Settings → Connections"), "{by_hand}: {e}");
        }
        // The overseer may set one up for all, and turn one on for Allan.
        let ben = caller("ben");
        let for_all = s
            .start(&ben, "linear", Some("Acme"), Some("all"), false)
            .await
            .unwrap();
        s.keep_key(
            &ben,
            "linear",
            None,
            LINEAR_PAGE.0,
            LINEAR_PAGE.1,
            Secret::new(GOOD.into()),
        )
        .unwrap();
        s.test(&ben, &for_all.id).await.unwrap();
        assert!(keys(&h.global).contains(&mine(&ws("default", "api_key"))));
        assert!(s
            .use_it(
                &allan,
                None,
                Some("linear"),
                Some("Acme"),
                Some("ben"),
                false,
                false
            )
            .await
            .unwrap_err()
            .contains("for itself only"));
        let on = s
            .use_it(
                &ben,
                None,
                Some("linear"),
                Some("acme"),
                Some("allan"),
                false,
                false,
            )
            .await
            .unwrap();
        assert_eq!(on, "Linear is on for Allan, read only.");
    }

    /// A key held goes only where it was read for: a Jira Cloud site, kept
    /// before the key, unchanged after it; Metabase tested only at the
    /// origin its key was read on.
    #[tokio::test]
    async fn a_key_goes_only_to_the_site_it_was_read_for() {
        let (_h, s) = setups().await;
        let allan = caller("allan");
        let jira = s
            .start(&allan, "jira", Some("Acme"), None, false)
            .await
            .unwrap();
        for bad in [
            "http://acme.atlassian.net",
            "acme.evil.example",
            "https://acme.atlassian.net/jira/projects",
            "https://acme.atlassian.net?x=1",
            "https://user:pw@acme.atlassian.net",
            "https://127.0.0.1",
            "https://localhost",
            "https://acme.atlassian.net:8443",
        ] {
            assert!(s.keep(&allan, &jira.id, "site", bad).is_err(), "{bad}");
        }
        let kept = s
            .keep(&allan, &jira.id, "site", "acme.atlassian.net")
            .unwrap();
        assert_eq!(
            kept.steps[0].label,
            "Found your site: https://acme.atlassian.net"
        );
        s.keep(&allan, &jira.id, "email", "sarah@acme.example")
            .unwrap();
        let token = "ATATT3xFfGF0example0123456789abcdef";
        s.keep_key(
            &allan,
            "jira",
            None,
            "https://id.atlassian.com",
            "/manage-profile/security/api-tokens",
            Secret::new(token.into()),
        )
        .unwrap();

        let mb = s
            .start(&allan, "metabase", None, None, false)
            .await
            .unwrap();
        assert!(s
            .keep(&allan, &mb.id, "site_url", "http://metabase.acme.example")
            .is_err());
        s.keep(&allan, &mb.id, "site_url", "https://metabase.acme.example")
            .unwrap();
        let page = s.key_page(&allan, "metabase").unwrap();
        assert_eq!(page.origin, "https://metabase.acme.example");
        assert!(s
            .keep_key(
                &allan,
                "metabase",
                None,
                "https://metabase.evil.example",
                "/admin/settings/authentication/api-keys",
                Secret::new("mb_0123456789abcdefghij".into())
            )
            .unwrap_err()
            .contains("key page"));
        // Datadog's site is a region.
        let dd = s.start(&allan, "datadog", None, None, false).await.unwrap();
        assert!(s.keep(&allan, &dd.id, "site", "evil.example").is_err());
        s.keep(&allan, &dd.id, "site", "eu1").unwrap();
        assert_eq!(
            s.key_page(&allan, "datadog").unwrap().origin,
            "https://app.datadoghq.eu"
        );
        let held = s
            .keep_key(
                &allan,
                "datadog",
                None,
                "https://app.datadoghq.eu",
                "/organization-settings/api-keys",
                Secret::new("0123456789abcdef0123456789abcdef".into()),
            )
            .unwrap();
        assert_eq!(held.test, "still needs app_key");
        // Once a key is held, nothing about where it goes changes.
        assert!(s
            .keep(&allan, &dd.id, "site", "us1")
            .unwrap_err()
            .contains("held"));
    }

    #[tokio::test]
    async fn too_many_setups_start_nothing() {
        let (_h, s) = setups().await;
        let allan = caller("allan");
        for n in 0..16 {
            // Each in its own conversation: one conversation's again is
            // the same setup.
            let here = Caller {
                session: Some(format!("s-{n}")),
                ..allan.clone()
            };
            s.start(&here, "linear", None, None, false).await.unwrap();
        }
        assert!(s
            .start(&allan, "github", None, None, false)
            .await
            .unwrap_err()
            .contains("Too many"));
    }

    #[tokio::test]
    async fn the_tools_say_what_they_change_to_desks_gate() {
        use workforce_agent::tools::Tool;
        use workforce_core::computer::Effect;
        let write = |e: Effect| match e {
            Effect::Connector {
                connector,
                write: true,
            } => Some(connector),
            _ => None,
        };
        for tool in setup::SetupTool::all("allan") {
            let effect = write(tool.effect(&json!({ "service": "linear" })));
            match tool.name() {
                "connections.list" | "connections.step" | "connections.wait" => {
                    assert_eq!(effect, None, "{}", tool.name())
                }
                // A connection write: asks below unbound. Starting a setup
                // is the person's one word for keeping and turning it on.
                "connections.keep" | "connections.test" | "connections.setup" => {
                    assert_eq!(effect.as_deref(), Some("connections"), "{}", tool.name())
                }
                _ => assert!(effect.is_some(), "{}", tool.name()),
            }
        }
        let setup = setup::SetupTool::all("allan")
            .into_iter()
            .find(|t| t.name() == "connections.setup")
            .unwrap();
        assert_eq!(
            write(setup.effect(&json!({ "service": "github" }))).as_deref(),
            Some("connections"),
            "GitHub's sign-in is gated"
        );
        assert_eq!(
            write(setup.effect(&json!({ "service": "linear", "for": "ben" }))).as_deref(),
            Some("connections:other")
        );
        let use_it = setup::SetupTool::all("allan")
            .into_iter()
            .find(|t| t.name() == "connections.use")
            .unwrap();
        assert_eq!(
            write(use_it.effect(&json!({ "agent": "ben" }))).as_deref(),
            Some("connections:other")
        );
    }

    /// Desk's browser, as a test has it: the agent's tab's address, and
    /// how often it was put in front.
    #[derive(Default)]
    struct Tab {
        at: Mutex<Option<String>>,
        shown: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl Tabs for Tab {
        async fn location(&self, _agent: &str, _person: &str) -> Option<TabAt> {
            self.at.lock().unwrap().clone().map(|url| TabAt {
                password: url.contains("#password"),
                url,
            })
        }
        async fn show(&self, _agent: &str) -> bool {
            self.shown.fetch_add(1, Ordering::SeqCst);
            true
        }
    }

    impl Tab {
        fn on(&self, url: &str) {
            *self.at.lock().unwrap() = Some(url.to_string());
        }
    }

    /// Setups over `h`, with GitHub's CLI at `gh` and the tab `tab`.
    fn over(h: &mut Hub, gh: Option<super::gh::GhCli>, tab: Arc<Tab>) -> Arc<Setups> {
        if let Some(gh) = gh {
            Arc::get_mut(&mut h.c).unwrap().services.gh = gh;
        }
        Arc::new(Setups::new(Arc::clone(&h.c), Arc::new(People)).with_tabs(tab))
    }

    /// A stand-in `gh`, signed in as Sarah: it never reaches the real one.
    fn fake_gh(dir: &std::path::Path) -> super::gh::GhCli {
        let bin = dir.join("gh");
        workforce_common::executable::write(
            &bin,
            format!(
                "#!/bin/sh\ncase \"$1\" in\n  api) printf 'HTTP/2.0 200 OK\\nX-Oauth-Scopes: repo, read:org\\n\\n{{\"login\":\"sarah\"}}' ;;\n  auth) printf '%s\\n' '{GH_GOOD}' ;;\n  *) exit 1 ;;\nesac\n"
            ),
        )
        .unwrap();
        super::gh::GhCli::at(bin)
    }

    /// Waits for setup `id`'s card to show `kind`.
    async fn card_shows(s: &Setups, id: &str, kind: NowKind) -> setup::SetupView {
        for _ in 0..400 {
            if let Some(v) = s.views(SARAH, None).into_iter().find(|v| v.id == id) {
                if v.now.kind == kind {
                    return v;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{id} never showed {kind:?}: {:?}", s.views(SARAH, None));
    }

    /// One setup is one card: asked again in the conversation it is the
    /// same; another connection of the service there replaces it.
    #[tokio::test]
    async fn the_same_setup_again_is_the_same_card_and_a_new_one_replaces_it() {
        let (_h, s) = setups().await;
        let allan = caller("allan");
        let first = s.start(&allan, "linear", None, None, false).await.unwrap();
        let again = s.start(&allan, "linear", None, None, false).await.unwrap();
        assert_eq!(first.id, again.id);
        assert_eq!(s.views(SARAH, None).len(), 1);
        // Another conversation's is its own.
        let elsewhere = Caller {
            session: Some("elsewhere".into()),
            ..caller("allan")
        };
        let other = s
            .start(&elsewhere, "linear", None, None, false)
            .await
            .unwrap();
        assert_ne!(other.id, first.id);
        // Another connection of Linear here takes over its card.
        let acme = s
            .start(&allan, "linear", Some("Acme"), None, false)
            .await
            .unwrap();
        assert_ne!(acme.id, first.id);
        let views = s.views(SARAH, None);
        let old = views.iter().find(|v| v.id == first.id).unwrap();
        assert_eq!(old.ended, Some(Ended::Replaced));
        assert_eq!(old.now.kind, NowKind::Ended);
        assert!(
            s.key_page(&allan, "linear").is_some(),
            "the new one reads its key"
        );
        let said = s.wait(&allan, &first.id, Duration::ZERO).await.unwrap();
        assert_eq!(said["ended"], json!(true));
    }

    /// GitHub's own sign-in: the card shows the code, the agent never sees
    /// it, and once it is approved the hub keeps it and turns it on itself.
    #[tokio::test]
    async fn githubs_code_is_the_persons_and_the_hub_finishes_by_itself() {
        let (h, s) = setups().await;
        let allan = caller("allan");
        let started = s.start(&allan, "github", None, None, true).await.unwrap();
        assert_eq!(started.way, Way::Device);
        assert_eq!(started.now.kind, NowKind::Code);
        assert_eq!(started.now.code.as_deref(), Some("ABCD-1234"));
        assert_eq!(started.now.action, Some("open_code"));
        let next = s.next(&started);
        assert!(!next.contains("ABCD-1234"), "{next}");
        assert!(next.contains("connections.wait"), "{next}");
        // The person is told where the card is, before the agent waits.
        let line = format!(
            "I've put a GitHub card here: [open it](desk://setup/{})",
            started.id
        );
        assert!(next.contains(&line), "{next}");
        assert!(next.find(&line) < next.find("connections.wait"), "{next}");
        assert!(!format!("{:?}", started.steps).contains("ABCD-1234"));
        let said = s
            .wait(&allan, &started.id, Duration::from_secs(5))
            .await
            .unwrap();
        assert_eq!(said["connected"], json!("sarah"), "{said}");
        assert_eq!(said["onFor"], json!("Allan"));
        let card = card_shows(&s, &started.id, NowKind::Connected).await;
        assert_eq!(card.now.title, "GitHub connected as sarah");
        assert!(card.now.action.is_none() && card.now.other.is_none());
        assert!(card.now.detail.unwrap().contains("read and write"));
        assert!(keys(&h.allan).iter().any(|k| k.ends_with("/token")));
        let everything = serde_json::to_string(&s.views(SARAH, None)).unwrap();
        assert!(!everything.contains(GH_GOOD) && !everything.contains("dev-code-secret"));
    }

    /// A code turned down is the card's to start again: a new code, on the
    /// same card.
    #[tokio::test]
    async fn a_code_turned_down_gets_a_new_one_on_the_same_card() {
        let (h, s) = setups().await;
        *h.github.outcome.lock().unwrap() = "denied";
        let allan = caller("allan");
        let started = s.start(&allan, "github", None, None, false).await.unwrap();
        let failed = card_shows(&s, &started.id, NowKind::Failed).await;
        assert_eq!(failed.now.action, Some("new_code"));
        assert!(failed.now.detail.unwrap().contains("Nothing was kept"));
        assert!(keys(&h.allan).is_empty());
        // Only the person, never another.
        assert!(s.new_code("someone-else", &started.id).await.is_err());
        *h.github.outcome.lock().unwrap() = "token";
        let again = s.new_code(SARAH, &started.id).await.unwrap();
        assert_eq!(again.id, started.id);
        assert_eq!(again.now.kind, NowKind::Code);
        card_shows(&s, &started.id, NowKind::Connected).await;
    }

    /// The GitHub CLI signed in on this computer: one word on the card
    /// connects it, and nothing is read of it before that word.
    #[tokio::test]
    async fn the_github_cli_sign_in_is_used_at_the_persons_word() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = hub().await;
        let s = over(&mut h, Some(fake_gh(dir.path())), Arc::new(Tab::default()));
        let allan = caller("allan");
        let started = s.start(&allan, "github", None, None, false).await.unwrap();
        assert_eq!(started.way, Way::Cli);
        assert_eq!(started.now.kind, NowKind::Offer);
        assert_eq!(
            started.now.title,
            "GitHub is signed in on this computer as sarah"
        );
        let detail = started.now.detail.clone().unwrap();
        assert!(
            detail.contains("repo, read:org") && detail.contains("read only"),
            "{detail}"
        );
        assert_eq!(started.now.action, Some("use_cli"));
        assert_eq!(started.now.other, Some("other_way"));
        assert!(
            keys(&h.allan).is_empty(),
            "nothing kept before the person's word"
        );
        assert!(s.use_cli("someone-else", &started.id).await.is_err());
        let done = s.use_cli(SARAH, &started.id).await.unwrap();
        assert_eq!(done.now.kind, NowKind::Connected, "{:?}", done.now);
        assert_eq!(done.now.title, "GitHub connected as sarah");
        assert!(keys(&h.allan).iter().any(|k| k.ends_with("/token")));
        assert!(!serde_json::to_string(&done).unwrap().contains(GH_GOOD));
    }

    /// Turning the CLI's sign-in down: GitHub's own sign-in on the same card.
    #[tokio::test]
    async fn another_way_than_the_cli_is_githubs_own_sign_in() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = hub().await;
        let s = over(&mut h, Some(fake_gh(dir.path())), Arc::new(Tab::default()));
        let allan = caller("allan");
        let started = s.start(&allan, "github", None, None, false).await.unwrap();
        let other = s.other_way(SARAH, &started.id).await.unwrap();
        assert_eq!(other.id, started.id);
        assert_eq!(other.way, Way::Device);
        assert_eq!(other.now.kind, NowKind::Code);
    }

    /// Without the CLI or Desk's GitHub app, a token made in the browser, on
    /// GitHub's page filled in with the least it needs.
    #[tokio::test]
    async fn githubs_token_page_comes_filled_in() {
        let mut h = hub().await;
        Arc::get_mut(&mut h.c).unwrap().github_client_id = None;
        let s = over(&mut h, None, Arc::new(Tab::default()));
        let started = s
            .start(&caller("allan"), "github", None, None, false)
            .await
            .unwrap();
        assert_eq!(started.way, Way::Browser);
        let next = s.next(&started);
        for want in [
            "settings/personal-access-tokens/new?",
            "expires_in=90",
            "contents=read",
            "metadata=read",
            "no selector",
        ] {
            assert!(next.contains(want), "{want}: {next}");
        }
        assert!(next.contains("never call browser.sign_in"), "{next}");
        assert!(
            next.starts_with("First tell the person")
                && next.contains(&format!("desk://setup/{}", started.id)),
            "{next}"
        );
    }

    /// Signing in is seen from the tab: on the service's sign-in, the card
    /// asks the person and the tab comes to the front; on its key page, the
    /// agent goes on. Once the key is read, the hub checks it and turns it on.
    #[tokio::test]
    async fn signing_in_is_seen_and_the_hub_turns_it_on() {
        let mut h = hub().await;
        let tab = Arc::new(Tab::default());
        let s = over(&mut h, None, Arc::clone(&tab));
        let allan = caller("allan");
        let started = s
            .start(&allan, "linear", Some("Acme"), None, false)
            .await
            .unwrap();
        // No page open yet: the agent is told to open it.
        let said = s.wait(&allan, &started.id, Duration::ZERO).await.unwrap();
        assert!(
            said["next"].as_str().unwrap().contains("browser.navigate"),
            "{said}"
        );
        tab.on("https://linear.app/login");
        let said = s
            .wait(&allan, &started.id, Duration::from_millis(30))
            .await
            .unwrap();
        assert_eq!(said["waiting"], json!(true), "{said}");
        let card = card_shows(&s, &started.id, NowKind::SignIn).await;
        assert_eq!(card.now.title, "Sign in to Linear in Desk's window");
        assert_eq!(card.now.action, Some("show_page"));
        assert_eq!(tab.shown.load(Ordering::SeqCst), 1, "put in front once");
        // While its card asks, the agent opens no sign-in of its own.
        assert!(s
            .sign_in_owned(&allan, "https://linear.app/login")
            .is_some());
        assert!(s
            .sign_in_owned(&allan, "https://example.com/login")
            .is_none());
        assert!(s
            .sign_in_owned(&caller("ben"), "https://linear.app/login")
            .is_none());
        // The key page asking for the password again is still the
        // person's sign-in.
        tab.on("https://linear.app/acme/settings/account/security#password");
        let said = s
            .wait(&allan, &started.id, Duration::from_millis(30))
            .await
            .unwrap();
        assert_eq!(said["waiting"], json!(true), "{said}");
        card_shows(&s, &started.id, NowKind::SignIn).await;
        tab.on("https://linear.app/acme/settings/account/security");
        let said = s
            .wait(&allan, &started.id, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(said["signedIn"], json!(true), "{said}");
        let kept = s
            .keep_key(
                &allan,
                "linear",
                None,
                LINEAR_PAGE.0,
                LINEAR_PAGE.1,
                Secret::new(GOOD.into()),
            )
            .unwrap();
        assert!(kept.ready);
        s.finish(&allan, &kept.setup).await;
        let said = s
            .wait(&allan, &started.id, Duration::from_secs(1))
            .await
            .unwrap();
        assert_eq!(said["onFor"], json!("Allan"), "{said}");
        let card = card_shows(&s, &started.id, NowKind::Connected).await;
        assert!(card.now.title.starts_with("Linear connected as"));
        assert!(s
            .sign_in_owned(&allan, "https://linear.app/login")
            .is_none());
    }

    /// A key the service refuses is the card's failure, and its words never
    /// reach the agent.
    #[tokio::test]
    async fn a_refused_key_fails_on_the_card_alone() {
        let (h, s) = setups().await;
        let allan = caller("allan");
        let started = s.start(&allan, "linear", None, None, false).await.unwrap();
        let kept = s
            .keep_key(
                &allan,
                "linear",
                None,
                LINEAR_PAGE.0,
                LINEAR_PAGE.1,
                Secret::new(BAD.into()),
            )
            .unwrap();
        s.finish(&allan, &kept.setup).await;
        let card = card_shows(&s, &started.id, NowKind::Failed).await;
        assert_eq!(card.now.action, Some("retry"));
        assert!(!format!("{card:?}").contains(BAD));
        let said = s.wait(&allan, &started.id, Duration::ZERO).await.unwrap();
        assert_eq!(said["failed"], json!(true));
        assert!(!said.to_string().contains(BAD));
        assert!(keys(&h.allan).is_empty());
    }

    /// A token of the person's own, pasted on the card: offered from the
    /// start beside whatever else the card asks, checked, kept, turned on;
    /// a device sign-in under way stops.
    #[tokio::test]
    async fn a_pasted_token_connects_from_the_card_alone() {
        let (h, s) = setups().await;
        *h.github.outcome.lock().unwrap() = "pending";
        let allan = caller("allan");
        let started = s.start(&allan, "github", None, None, false).await.unwrap();
        assert_eq!(started.now.kind, NowKind::Code);
        let page = started.now.paste.clone().unwrap();
        assert!(
            page.starts_with("https://github.com/settings/personal-access-tokens/new?"),
            "{page}"
        );
        assert!(
            page.contains("contents=read") && page.contains("expires_in=90"),
            "{page}"
        );
        // Only the person, and only a token shaped like GitHub's.
        assert!(s
            .paste_token("someone-else", &started.id, Secret::new(GH_GOOD.into()))
            .await
            .is_err());
        assert!(s
            .paste_token(SARAH, &started.id, Secret::new("hunter2".into()))
            .await
            .unwrap_err()
            .contains("doesn't look like"));
        let done = s
            .paste_token(SARAH, &started.id, Secret::new(format!(" {GH_GOOD}\n")))
            .await
            .unwrap();
        assert_eq!(done.way, Way::Token);
        assert_eq!(done.now.kind, NowKind::Connected, "{:?}", done.now);
        assert_eq!(done.now.title, "GitHub connected as sarah");
        assert!(done.now.paste.is_none());
        assert!(keys(&h.allan).iter().any(|k| k.ends_with("/token")));
        assert!(!serde_json::to_string(&s.views(SARAH, None))
            .unwrap()
            .contains(GH_GOOD));
        // The device sign-in it replaced keeps nothing more.
        *h.github.outcome.lock().unwrap() = "token";
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(
            keys(&h.allan)
                .iter()
                .filter(|k| k.ends_with("/token"))
                .count(),
            1
        );
    }

    /// A token the agent made for GitHub is a fine-grained one: a classic
    /// one, which could carry any scope, is kept nowhere.
    #[tokio::test]
    async fn a_classic_token_the_agent_made_is_refused() {
        let mut h = hub().await;
        Arc::get_mut(&mut h.c).unwrap().github_client_id = None;
        let s = over(&mut h, None, Arc::new(Tab::default()));
        let allan = caller("allan");
        let started = s.start(&allan, "github", None, None, false).await.unwrap();
        assert_eq!(started.way, Way::Browser);
        let kept = s
            .keep_key(
                &allan,
                "github",
                None,
                "https://github.com",
                "/settings/tokens",
                Secret::new(GH_GOOD.into()),
            )
            .unwrap();
        s.finish(&allan, &kept.setup).await;
        let card = card_shows(&s, &started.id, NowKind::Failed).await;
        assert!(card.now.detail.unwrap().contains("fine-grained"));
        assert!(keys(&h.allan).is_empty());
    }

    /// GitHub's sign-in that can't start, after the CLI's was turned down:
    /// the card says so and offers a new code, never a spinner for good.
    #[tokio::test]
    async fn a_github_sign_in_that_cant_start_fails_on_the_card() {
        let dir = tempfile::tempdir().unwrap();
        let mut h = hub().await;
        Arc::get_mut(&mut h.c).unwrap().services.github_web = "http://127.0.0.1:9".into();
        let s = over(&mut h, Some(fake_gh(dir.path())), Arc::new(Tab::default()));
        let allan = caller("allan");
        let started = s.start(&allan, "github", None, None, false).await.unwrap();
        assert_eq!(started.way, Way::Cli);
        assert!(s.other_way(SARAH, &started.id).await.is_err());
        let card = card_shows(&s, &started.id, NowKind::Failed).await;
        assert_eq!(card.now.action, Some("new_code"));
        let said = s.wait(&allan, &started.id, Duration::ZERO).await.unwrap();
        assert_eq!(said["failed"], json!(true), "{said}");
    }

    /// A pasted token GitHub refuses: nothing kept, and the card asks for
    /// another.
    #[tokio::test]
    async fn a_pasted_token_github_refuses_keeps_nothing() {
        let mut h = hub().await;
        Arc::get_mut(&mut h.c).unwrap().github_client_id = None;
        let s = over(&mut h, None, Arc::new(Tab::default()));
        let started = s
            .start(&caller("allan"), "github", None, None, false)
            .await
            .unwrap();
        assert!(
            started.now.paste.is_some(),
            "offered with no GitHub app too"
        );
        let failed = s
            .paste_token(
                SARAH,
                &started.id,
                Secret::new("ghp_badtoken0123456789abcdef".into()),
            )
            .await
            .unwrap();
        assert_eq!(failed.now.kind, NowKind::Failed);
        assert_eq!(failed.now.action, Some("paste_token"));
        assert!(failed.now.paste.is_some());
        assert!(!format!("{failed:?}").contains("ghp_badtoken"));
        assert!(keys(&h.allan).is_empty());
    }

    const GOOGLE_PAGE: (&str, &str) = ("https://console.cloud.google.com", "/auth/clients");

    /// Google with no app of the person's: every step the agent drives,
    /// the keys read off the console kept as the person's app (never as the
    /// connection), then approving on Google's page, and the hub finishing.
    #[tokio::test]
    async fn google_setup_without_an_app_walks_every_step_then_approves() {
        let (h, s) = setups().await;
        let allan = caller("allan");
        let started = s
            .start(&allan, "google", Some("Work"), None, false)
            .await
            .unwrap();
        assert_eq!(started.way, Way::App);
        let ids: Vec<&str> = started.steps.iter().map(|x| x.id).collect();
        assert_eq!(
            ids,
            [
                "page", "sign_in", "project", "apis", "branding", "audience", "access", "client",
                "key", "approve", "check"
            ]
        );
        let next = s.next(&started);
        for says in [
            "projectcreate",
            "Desktop app",
            "Publish app",
            "https://mail.google.com/",
            "stuck",
        ] {
            assert!(next.contains(says), "{says}: {next}");
        }
        let kept = s
            .keep(&allan, &started.id, "email", "sarah@acme.example")
            .unwrap();
        let branding = kept.steps[4].instruction.clone().unwrap();
        assert!(
            branding.contains("support email sarah@acme.example"),
            "{branding}"
        );
        for step in [
            "page", "sign_in", "project", "apis", "branding", "audience", "access", "client",
        ] {
            s.step(&allan, &started.id, step, StepState::Done, None)
                .unwrap();
        }
        assert!(s
            .step(&allan, &started.id, "approve", StepState::Done, None)
            .is_err());
        let page = s.key_page(&allan, "google").unwrap();
        assert_eq!(page.fields, ["client_id", "client_secret"]);
        let kept = s
            .keep_key(
                &allan,
                "google",
                None,
                GOOGLE_PAGE.0,
                GOOGLE_PAGE.1,
                Secret::new(GOOGLE_ID.to_string()),
            )
            .unwrap();
        assert!(
            !kept.ready && kept.test.contains("client_secret"),
            "{}",
            kept.test
        );
        let kept = s
            .keep_key(
                &allan,
                "google",
                None,
                GOOGLE_PAGE.0,
                GOOGLE_PAGE.1,
                Secret::new(GOOGLE_SECRET.to_string()),
            )
            .unwrap();
        assert!(kept.ready);
        assert!(!format!("{kept:?}").contains(GOOGLE_SECRET.as_str()));
        s.finish(&allan, &started.id).await;
        let approving = card_shows(&s, &started.id, NowKind::Approve).await;
        assert_eq!(approving.now.action, Some("open_approve"));
        assert!(approving
            .now
            .detail
            .as_deref()
            .unwrap()
            .contains("Advanced"));
        // The app is the person's, hub-wide; nothing of the connection yet.
        assert_eq!(
            h.global
                .get("skills/google/oauth_client/client_id")
                .as_deref(),
            Some(GOOGLE_ID.as_str())
        );
        assert!(keys(&h.allan).is_empty());
        let url = approving.now.url.unwrap();
        let pending = h.c.take_pending(&state_of(&url)).unwrap();
        h.c.complete(pending, "work").await.unwrap();
        let done = card_shows(&s, &started.id, NowKind::Connected).await;
        assert_eq!(done.connected.unwrap().account, "work@acme.example");
        assert_eq!(
            h.allan
                .get(&mine("skills/google/workspaces/default/label"))
                .as_deref(),
            Some("Work")
        );
    }

    /// FreeAgent's identifier and secret look alike: each is read by name,
    /// and the same value read for both is neither.
    #[tokio::test]
    async fn freeagent_keys_are_read_by_name_and_one_value_is_never_both() {
        let (_h, s) = setups().await;
        let allan = caller("allan");
        let started = s
            .start(&allan, "freeagent", Some("Harbor"), None, false)
            .await
            .unwrap();
        for step in started
            .steps
            .iter()
            .map(|x| x.id)
            .take_while(|x| *x != "key")
        {
            s.step(&allan, &started.id, step, StepState::Done, None)
                .unwrap();
        }
        let read = |field: &str, value: &str| {
            s.keep_key(
                &allan,
                "freeagent",
                Some(field),
                "https://dev.freeagent.com",
                "/apps/1",
                Secret::new(value.to_string()),
            )
        };
        assert!(s.next(&started).contains("field: \"client_secret\""));
        assert!(!read("client_id", FA_ID).unwrap().ready);
        let twice = read("client_secret", FA_ID).unwrap_err();
        assert!(
            twice.contains("already kept as the app's client_id"),
            "{twice}"
        );
        assert!(read("client_secret", FA_SECRET).unwrap().ready);
    }

    /// Walks a FreeAgent setup with no app yet to its keys, and reads both.
    async fn freeagent_app_kept(s: &Arc<Setups>, allan: &Caller) -> String {
        let started = s
            .start(allan, "freeagent", Some("Harbor"), None, false)
            .await
            .unwrap();
        for step in started
            .steps
            .iter()
            .map(|x| x.id)
            .take_while(|x| *x != "key")
        {
            s.step(allan, &started.id, step, StepState::Done, None)
                .unwrap();
        }
        for (field, value) in [("client_id", FA_ID), ("client_secret", FA_SECRET)] {
            s.keep_key(
                allan,
                "freeagent",
                Some(field),
                "https://dev.freeagent.com",
                "/apps/1",
                Secret::new(value.to_string()),
            )
            .unwrap();
        }
        s.finish(allan, &started.id).await;
        started.id
    }

    /// The keys read off the app's page are the person's app, hub-wide:
    /// no connection holds them, and none is made before they approve.
    #[tokio::test]
    async fn an_app_key_is_kept_as_the_app_not_the_connection() {
        let (h, s) = setups().await;
        let allan = caller("allan");
        let id = freeagent_app_kept(&s, &allan).await;
        card_shows(&s, &id, NowKind::Approve).await;
        assert_eq!(
            h.global
                .get("skills/freeagent/oauth_client/client_id")
                .as_deref(),
            Some(FA_ID)
        );
        assert!(keys(&h.allan).is_empty(), "{:?}", keys(&h.allan));
        assert!(
            !keys(&h.global).iter().any(|k| k.contains("/workspaces/")),
            "{:?}",
            keys(&h.global)
        );
    }

    /// A FreeAgent connection set up for an agent is turned on for it read
    /// only: its books change only once the person allows it.
    #[tokio::test]
    async fn a_freeagent_connection_is_turned_on_read_only() {
        let (h, s) = setups().await;
        let allan = caller("allan");
        let id = freeagent_app_kept(&s, &allan).await;
        let approving = card_shows(&s, &id, NowKind::Approve).await;
        let pending =
            h.c.take_pending(&state_of(&approving.now.url.unwrap()))
                .unwrap();
        h.c.complete(pending, "harbor").await.unwrap();
        card_shows(&s, &id, NowKind::Connected).await;
        let on = s
            .use_it(&allan, Some(&id), None, None, None, false, true)
            .await
            .unwrap();
        assert!(on.contains("read only"), "{on}");
        let config = h
            .crew
            .configs
            .lock()
            .unwrap()
            .get("allan")
            .cloned()
            .unwrap_or_default();
        assert!(!config.contains("writable"), "{config}");
    }

    #[tokio::test]
    async fn freeagent_second_business_starts_at_approve() {
        let (h, s) = setups().await;
        h.c.keep_app(SARAH, "freeagent", FA_ID, FA_SECRET)
            .await
            .unwrap();
        let allan = caller("allan");
        let lumen = s
            .start(&allan, "freeagent", Some("Lumen"), None, false)
            .await
            .unwrap();
        let ids: Vec<&str> = lumen.steps.iter().map(|x| x.id).collect();
        assert_eq!(ids, ["approve", "check"]);
        assert_eq!(lumen.now.kind, NowKind::Approve);
        assert!(lumen
            .now
            .detail
            .as_deref()
            .unwrap()
            .contains("sign in as Lumen"));
        assert!(
            lumen.steps[0].label.contains("as Lumen"),
            "{:?}",
            lumen.steps[0]
        );
        assert!(s.next(&lumen).contains("approve"));
    }

    /// A step the agent can't do is the person's, exactly as the agent was
    /// told it; once they've done it, the agent is told to go on.
    #[tokio::test]
    async fn a_stuck_step_waits_on_the_person_with_its_instruction() {
        let (_h, s) = setups().await;
        let allan = caller("allan");
        let started = s.start(&allan, "google", None, None, false).await.unwrap();
        let stuck = s
            .keep(&allan, &started.id, "stuck", "Chrome isn't connected")
            .unwrap();
        assert_eq!(stuck.now.kind, NowKind::Step);
        assert_eq!(stuck.now.title, "Step 1 of 11: Open Google Cloud");
        assert!(stuck
            .now
            .detail
            .as_deref()
            .unwrap()
            .contains("projectcreate"));
        assert_eq!(stuck.now.action, Some("open_step"));
        assert_eq!(stuck.now.other, Some("done_step"));
        assert!(!stuck.now.app_paste);
        let done = s.done_step(SARAH, &started.id).await.unwrap();
        assert_eq!(done.steps[0].state, StepState::Done);
        let said = s.wait(&allan, &started.id, Duration::ZERO).await.unwrap();
        assert_eq!(said["did"], json!("page"));
        assert!(s.keep(&allan, &started.id, "colour", "blue").is_err());
    }

    #[tokio::test]
    async fn the_persons_app_paste_keeps_it_and_moves_on() {
        let (h, s) = setups().await;
        let allan = caller("allan");
        let started = s
            .start(&allan, "freeagent", Some("Harbor"), None, false)
            .await
            .unwrap();
        assert!(started.steps.iter().any(|x| x
            .instruction
            .as_deref()
            .is_some_and(|i| i.contains("http://localhost:1/api/auth/freeagent/callback"))));
        assert!(s
            .paste_app(
                SARAH,
                &started.id,
                Secret::new("short".into()),
                Secret::new(FA_SECRET.into())
            )
            .await
            .is_err());
        assert!(s
            .paste_app(
                "someone-else",
                &started.id,
                Secret::new(FA_ID.into()),
                Secret::new(FA_SECRET.into())
            )
            .await
            .is_err());
        let pasted = s
            .paste_app(
                SARAH,
                &started.id,
                Secret::new(FA_ID.into()),
                Secret::new(FA_SECRET.into()),
            )
            .await
            .unwrap();
        assert_eq!(pasted.now.kind, NowKind::Approve);
        assert!(!format!("{pasted:?}").contains(FA_SECRET));
        assert_eq!(
            h.global
                .get("skills/freeagent/oauth_client/client_id")
                .as_deref(),
            Some(FA_ID)
        );
        assert_eq!(
            h.global
                .get("skills/freeagent/oauth_client/redirect_uri")
                .as_deref(),
            Some("http://localhost:1/api/auth/freeagent/callback")
        );
    }

    #[test]
    fn connections_setup_describes_google_and_freeagent() {
        use workforce_agent::tools::Tool;
        let tools = setup::SetupTool::all("allan");
        let setup = tools
            .iter()
            .find(|t| t.name() == "connections.setup")
            .unwrap();
        assert!(setup.description().contains("Google"));
        assert!(setup.description().contains("FreeAgent"));
        let schema = setup.input_schema().to_string();
        assert!(
            schema.contains("\"google\"") && schema.contains("\"freeagent\""),
            "{schema}"
        );
        let keep = tools
            .iter()
            .find(|t| t.name() == "connections.keep")
            .unwrap();
        assert!(keep.description().contains("stuck"));
    }
}

#[test]
fn a_copied_connections_hub_is_named_on_one_short_line() {
    assert_eq!(hub_name(Some("  Patchwork ")).as_deref(), Some("Patchwork"));
    assert_eq!(
        hub_name(Some("Patch\nwork\u{7}")).as_deref(),
        Some("Patchwork")
    );
    assert_eq!(
        hub_name(Some(&"x".repeat(200))).map(|n| n.len()),
        Some(MAX_NAME)
    );
    assert_eq!(hub_name(Some("   ")), None);
    assert_eq!(hub_name(None), None);
}

/// Keys Desk copied from a paired hub are kept as any connection is, and
/// this hub's audit says where from, never a value; a connection made by
/// hand writes no such row.
#[tokio::test]
async fn a_copied_connection_is_audited_here_with_where_it_came_from() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;
    use workforce_identity::store::IdentityStore as _;
    use workforce_identity::types::{Identity, IdentityRole};

    let h = hub().await;
    let fx = crate::remote_mcp::testing::fixture(false);
    let mut sarah = Identity::new("Sarah", IdentityRole::SystemAdmin);
    sarah.id = uuid::Uuid::parse_str(SARAH).unwrap();
    fx.identities.create(&sarah).unwrap();
    let jwt =
        crate::jwt::JwtConfig::new("a-test-secret-of-at-least-32-bytes!!".into(), 3600).unwrap();
    let token = crate::jwt::generate_token_with_identity(
        &jwt,
        "Sarah",
        IdentityRole::SystemAdmin,
        Some(SARAH.to_string()),
    )
    .unwrap();
    let audit = Arc::new(workforce_common::persistence::SqliteStore::in_memory().unwrap());
    let state = Arc::new(GatewayState {
        jwt_config: Some(jwt),
        telemetry_store: Some(Arc::clone(&audit) as _),
        ..Arc::try_unwrap(fx.state)
            .ok()
            .expect("the fixture's only handle")
    });
    let app = router(state, Arc::clone(&h.c));
    let post = |body: Value| {
        Request::builder()
            .method("POST")
            .uri("/api/desk/connections/linear")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap()
    };
    let res = app
        .clone()
        .oneshot(post(
            json!({"level": "all", "fields": {"api_key": GOOD}, "copied_from": "Patchwork\n"}),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    let res = app
        .clone()
        .oneshot(post(
            json!({"name": "Mine", "level": "all", "fields": {"api_key": GOOD2}}),
        ))
        .await
        .unwrap();
    assert_eq!(res.status(), StatusCode::OK);
    // A copy whose check fails is kept nowhere, and its row says so.
    let res = app
        .clone()
        .oneshot(post(
            json!({"level": "all", "fields": {"api_key": BAD}, "copied_from": "Patchwork"}),
        ))
        .await
        .unwrap();
    assert!(!res.status().is_success());
    let rows: Vec<_> = audit
        .recent_audit(100)
        .unwrap()
        .into_iter()
        .filter(|e| e.action == "desk_connections.copy")
        .collect();
    assert_eq!(rows.len(), 2, "only the copies: {rows:?}");
    let detail = |at: usize| -> Value {
        serde_json::from_str(rows[at].detail.as_deref().unwrap_or("{}")).unwrap()
    };
    let (kept, failed): (Vec<_>, Vec<_>) = (0..rows.len())
        .map(detail)
        .partition(|d| d["check"] == "passed");
    assert_eq!((kept.len(), failed.len()), (1, 1), "{rows:?}");
    let said = kept[0]["summary"].as_str().unwrap();
    assert!(
        said.starts_with("Desk copied your Linear key")
            && said.ends_with("from Patchwork: its check passed"),
        "{said}"
    );
    assert!(kept[0]["connection"].is_string());
    assert_eq!(
        failed[0]["summary"],
        "Desk didn't keep your Linear key from Patchwork: its check failed"
    );
    assert_eq!(failed[0]["check"], "failed");
    for row in &rows {
        let text = row.detail.clone().unwrap_or_default();
        for secret in [GOOD, BAD] {
            assert!(
                !text.contains(secret) && !text.contains(&secret[..8]),
                "{text}"
            );
        }
        assert_eq!(row.actor, format!("user:{SARAH}"));
    }
}

fn bank(key: &str, bank: &str) -> BTreeMap<String, String> {
    BTreeMap::from([
        ("api_key".to_string(), key.to_string()),
        ("bank".to_string(), bank.to_string()),
    ])
}

/// BankSync allows a person ten requests a minute and a check makes three, so
/// each test below holds to ten of them for Sarah.
#[tokio::test]
async fn a_bank_connects_only_on_a_key_that_reads_and_says_when_its_approval_lapses() {
    let h = hub().await;
    let refused =
        h.c.connect(
            SARAH,
            "bank",
            Some("NatWest"),
            "agent",
            Some("allan"),
            bank(BS_WRITES, "NatWest"),
        )
        .await;
    assert!(refused.is_err(), "a key that can write is refused");

    let natwest =
        h.c.connect(
            SARAH,
            "bank",
            Some("NatWest"),
            "agent",
            Some("allan"),
            bank(BS_GOOD, "natwest"),
        )
        .await
        .unwrap();
    assert_eq!(natwest.account.as_deref(), Some("NatWest"));
    let approval = natwest.approval.expect("an approval date");
    assert!(approval.estimated && !approval.due);
    assert_eq!(approval.days_left, 90);
    assert_eq!(
        approval.until,
        (Utc::now() + chrono::Duration::days(90)).date_naive()
    );

    let path = key_at(
        VaultSource::AgentPerUser,
        SARAH,
        &format!(
            "skills/bank/workspaces/{}/consented_at",
            natwest.id.rsplit('~').next().unwrap()
        ),
    );
    let stamp = || h.allan.get(&path).expect("a stamp");
    let long_ago = (Utc::now() - chrono::Duration::days(100)).to_rfc3339();
    h.allan
        .apply(
            &[VaultOp::Set {
                path: &path,
                value: &long_ago,
            }],
            VaultActor::User(SARAH),
        )
        .unwrap();

    let tested = h.c.test(SARAH, "bank", &natwest.id).await.unwrap();
    assert_eq!(stamp(), long_ago, "a Test moves nothing");
    let lapsed = tested.approval.unwrap();
    assert!(lapsed.due && lapsed.days_left < 0);
    h.c.reconnect(SARAH, "bank", &natwest.id, bank(BS_GOOD, "natwest"))
        .await
        .unwrap();
    assert_eq!(stamp(), long_ago, "nor does a new key");
    let key = natwest.id.rsplit('~').next().unwrap().to_string();
    h.c.keep(
        catalog::find("bank").unwrap(),
        &Level::Agent("allan".into()),
        &key,
        SARAH,
        &bank(BS_GOOD, "natwest"),
        Some("NatWest"),
    )
    .unwrap();
    assert_eq!(
        stamp(),
        long_ago,
        "nor a setup run again, which keeps it the same way"
    );
}

#[tokio::test]
async fn a_lapsed_bank_is_refused_and_only_the_persons_renewal_moves_its_approval() {
    let h = hub().await;
    let lapsed =
        h.c.connect(
            SARAH,
            "bank",
            Some("Revolut"),
            "agent",
            Some("allan"),
            bank(BS_GOOD, "Revolut"),
        )
        .await;
    assert!(lapsed.is_err(), "a bank whose approval lapsed is refused");

    let natwest =
        h.c.connect(
            SARAH,
            "bank",
            Some("NatWest"),
            "agent",
            Some("allan"),
            bank(BS_GOOD, "natwest"),
        )
        .await
        .unwrap();
    let path = key_at(
        VaultSource::AgentPerUser,
        SARAH,
        &format!(
            "skills/bank/workspaces/{}/consented_at",
            natwest.id.rsplit('~').next().unwrap()
        ),
    );
    let long_ago = (Utc::now() - chrono::Duration::days(100)).to_rfc3339();
    h.allan
        .apply(
            &[VaultOp::Set {
                path: &path,
                value: &long_ago,
            }],
            VaultActor::User(SARAH),
        )
        .unwrap();
    let renewed = h.c.renew(SARAH, "bank", &natwest.id).await.unwrap();
    assert_ne!(
        h.allan.get(&path).unwrap(),
        long_ago,
        "the person's renewal does"
    );
    assert_eq!(renewed.approval.unwrap().days_left, 90);

    h.c.disconnect(SARAH, "bank", &natwest.id).await.unwrap();
    assert!(h.allan.get(&path).is_none(), "its stamp goes with it");
}

/// One person's checks never spend another's limit: Sarah's requests are
/// gone, and a second person's check still goes through.
#[tokio::test]
async fn one_persons_bank_checks_do_not_spend_anothers_limit() {
    let h = hub().await;
    let keys = || bank(BS_GOOD, "natwest");
    for _ in 0..3 {
        assert!(matches!(
            h.c.services.check("bank", SARAH, &keys()).await,
            Found::Works { .. }
        ));
    }
    // Sarah has spent nine requests; a fourth check's second one is her
    // eleventh, and waits too long to be sent.
    assert!(matches!(
        h.c.services.check("bank", SARAH, &keys()).await,
        Found::Failed(ref why) if why.contains("10 requests a minute")
    ));
    assert!(matches!(
        h.c.services.check("bank", "ben", &keys()).await,
        Found::Works { .. }
    ));
}

#[tokio::test]
async fn one_agent_watches_the_person_s_bank_approvals_while_a_bank_is_kept() {
    let h = hub().await;
    let watching = || {
        h.crew
            .watches
            .lock()
            .unwrap()
            .iter()
            .filter(|(_, on)| **on)
            .map(|(a, _)| a.clone())
            .collect::<Vec<_>>()
    };
    let own =
        h.c.connect(
            SARAH,
            "bank",
            Some("NatWest"),
            "agent",
            Some("ben"),
            bank(BS_GOOD, "natwest"),
        )
        .await
        .unwrap();
    assert_eq!(watching(), ["ben"], "the agent whose own it is");
    let all =
        h.c.connect(
            SARAH,
            "bank",
            Some("Revolut"),
            "all",
            None,
            bank(BS_GOOD, "natwest"),
        )
        .await
        .unwrap();
    assert_eq!(watching(), ["ben"], "ben reaches the shared one too");
    h.c.disconnect(SARAH, "bank", &own.id).await.unwrap();
    assert_eq!(watching(), ["allan"], "the first by name it reaches");

    // Allan is removed, and its jobs with it: the watch is ben's now.
    h.crew.agents.lock().unwrap().retain(|a| a.id != "allan");
    h.crew.watches.lock().unwrap().remove("allan");
    h.c.rewatch(SARAH).await;
    assert_eq!(watching(), ["ben"], "the next it reaches takes it on");

    h.c.disconnect(SARAH, "bank", &all.id).await.unwrap();
    assert!(watching().is_empty(), "gone with the last bank");
    assert_eq!(h.crew.watches.lock().unwrap().len(), 1);
}

/// A Google app's id and secret in their shapes, made up: built here so no
/// source line looks like a real one.
static GOOGLE_ID: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
    format!(
        "{}-{}.{}",
        "1".repeat(12),
        "ab".repeat(16),
        "apps.googleusercontent.com"
    )
});
static GOOGLE_SECRET: std::sync::LazyLock<String> =
    std::sync::LazyLock::new(|| ["GOCSPX", &"x".repeat(28)].join("-"));
const FA_ID: &str = "p0sIUgWBrTC8BOy5B-6bmA";
const FA_SECRET: &str = "Zx9_kq2LmN4pR7sT1vW3yA";

/// The `state` a sign-in page's address carries.
fn state_of(url: &str) -> String {
    url::Url::parse(url)
        .unwrap()
        .query_pairs()
        .find(|(k, _)| k == "state")
        .unwrap()
        .1
        .into_owned()
}

mod apps {
    use super::*;

    /// Signs `started` in as `code`'s account, as the service's redirect
    /// would.
    async fn signs_in(h: &Hub, started: &Started, code: &str) -> Result<String, String> {
        let pending =
            h.c.take_pending(&state_of(started.url.as_deref().unwrap()))
                .unwrap();
        h.c.complete(pending, code).await
    }

    async fn freeagent(h: &Hub, name: Option<&str>, code: &str) -> Result<String, String> {
        let started =
            h.c.start(SARAH, "freeagent", name, "all", None)
                .await
                .unwrap();
        signs_in(h, &started, code).await
    }

    #[tokio::test]
    async fn google_sign_in_keeps_tokens_email_and_client_at_the_level() {
        let h = hub().await;
        assert!(matches!(
            h.c.start(SARAH, "google", Some("Work"), "agent", Some("allan"))
                .await,
            Err(Refusal::Invalid(_))
        ));
        assert_eq!(
            connector(&h.c.list(SARAH).await, "google").app,
            Some(app::AppState::Missing)
        );
        assert!(h
            .c
            .keep_app(SARAH, "google", "123", &GOOGLE_SECRET)
            .await
            .is_err());
        h.c.keep_app(SARAH, "google", &GOOGLE_ID, &GOOGLE_SECRET)
            .await
            .unwrap();
        let started =
            h.c.start(SARAH, "google", Some("Work"), "agent", Some("allan"))
                .await
                .unwrap();
        let url = started.url.clone().unwrap();
        assert!(
            url.contains(
                "redirect_uri=http%3A%2F%2F127.0.0.1%3A1%2Fapi%2Fauth%2Fgoogle%2Fcallback"
            ),
            "{url}"
        );
        assert!(
            url.contains("code_challenge=") && url.contains("access_type=offline"),
            "{url}"
        );
        assert_eq!(
            signs_in(&h, &started, "work").await.unwrap(),
            "Google is connected as work@acme.example."
        );
        let at = |sub: &str| {
            h.allan
                .get(&mine(&format!("skills/google/workspaces/default/{sub}")))
        };
        assert_eq!(at("email").as_deref(), Some("work@acme.example"));
        assert_eq!(at("client_id").as_deref(), Some(GOOGLE_ID.as_str()));
        assert_eq!(at("label").as_deref(), Some("Work"));
        assert_eq!(at("refresh_token").as_deref(), Some("rt-work"));
        assert_eq!(
            keys(&h.global),
            [
                "skills/google/oauth_client/client_id",
                "skills/google/oauth_client/client_secret"
            ],
            "only the person's own app is hub-wide"
        );
        assert_eq!(
            h.c.flow(SARAH, &started.flow),
            Some(FlowState::Connected {
                account: "work@acme.example".into()
            })
        );
        // A second account is another instance, never over the first.
        let started =
            h.c.start(SARAH, "google", None, "agent", Some("allan"))
                .await
                .unwrap();
        signs_in(&h, &started, "personal").await.unwrap();
        let list = h.c.list(SARAH).await;
        let google = connector(&list, "google");
        assert_eq!(google.app, Some(app::AppState::Ready));
        let names: Vec<&str> = google.connections.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names.len(), 2);
        assert!(
            names.contains(&"Work") && names.contains(&"personal@acme.example"),
            "{names:?}"
        );
    }

    #[tokio::test]
    async fn freeagent_sign_in_keeps_company_and_url() {
        let h = hub().await;
        h.c.keep_app(SARAH, "freeagent", FA_ID, FA_SECRET)
            .await
            .unwrap();
        let started =
            h.c.start(SARAH, "freeagent", None, "all", None)
                .await
                .unwrap();
        let url = started.url.clone().unwrap();
        assert!(
            !url.contains("code_challenge"),
            "FreeAgent takes no PKCE: {url}"
        );
        assert!(url.contains("localhost%3A1"), "{url}");
        signs_in(&h, &started, "harbor").await.unwrap();
        let at = |sub: &str| {
            h.global
                .get(&mine(&format!("skills/freeagent/workspaces/default/{sub}")))
        };
        assert_eq!(at("company").as_deref(), Some("Harbor"));
        assert_eq!(at("company_id").as_deref(), Some("6104"));
        assert_eq!(at("label").as_deref(), Some("Harbor"));
        freeagent(&h, None, "lumen").await.unwrap();
        let list = h.c.list(SARAH).await;
        let accounts: Vec<Option<&str>> = connector(&list, "freeagent")
            .connections
            .iter()
            .map(|c| c.account.as_deref())
            .collect();
        assert_eq!(accounts.len(), 2);
        assert!(accounts.contains(&Some("Harbor")) && accounts.contains(&Some("Lumen")));
    }

    /// The same business again is its own connection signed in again, by
    /// its name: never a second one over the same books.
    #[tokio::test]
    async fn the_same_business_again_signs_its_connection_in_again() {
        let h = hub().await;
        h.c.keep_app(SARAH, "freeagent", FA_ID, FA_SECRET)
            .await
            .unwrap();
        freeagent(&h, None, "harbor").await.unwrap();
        let started =
            h.c.start(SARAH, "freeagent", Some("Other"), "all", None)
                .await
                .unwrap();
        let said = signs_in(&h, &started, "harbor2").await.unwrap();
        assert!(said.contains("connected already as 'Harbor'"), "{said}");
        let list = h.c.list(SARAH).await;
        let fa = connector(&list, "freeagent");
        assert_eq!(fa.connections.len(), 1);
        assert_eq!(fa.connections[0].name, "Harbor");
        assert_eq!(
            h.global
                .get(&mine("skills/freeagent/workspaces/default/refresh_token"))
                .as_deref(),
            Some("rt-harbor2")
        );
    }

    /// Google's id and secret are each found by their own shape;
    /// FreeAgent's look alike, so neither is searched for alone.
    #[test]
    fn an_apps_keys_are_searched_for_by_their_own_shape() {
        let google = app::of("google").unwrap();
        let (id, secret) = (
            google.find_shape("client_id").unwrap(),
            google.find_shape("client_secret").unwrap(),
        );
        assert_ne!(id, secret);
        assert!(!id.starts_with('^') && !secret.ends_with('$'));
        assert!(regex::Regex::new(id).unwrap().is_match(&GOOGLE_ID));
        assert!(!regex::Regex::new(id).unwrap().is_match(&GOOGLE_SECRET));
        let freeagent = app::of("freeagent").unwrap();
        assert_eq!(freeagent.find_shape("client_id"), None);
        assert_eq!(google.find_shape("other"), None);
    }

    /// Two sign-ins in flight: the one finishing second never writes over
    /// the first's connection, whatever key it started on.
    #[tokio::test]
    async fn two_sign_ins_in_flight_keep_two_connections() {
        let h = hub().await;
        h.c.keep_app(SARAH, "google", &GOOGLE_ID, &GOOGLE_SECRET)
            .await
            .unwrap();
        let first = h.c.start(SARAH, "google", None, "all", None).await.unwrap();
        let second = h.c.start(SARAH, "google", None, "all", None).await.unwrap();
        signs_in(&h, &second, "home").await.unwrap();
        signs_in(&h, &first, "work").await.unwrap();
        let list = h.c.list(SARAH).await;
        assert_eq!(connector(&list, "google").connections.len(), 2);
        let tokens: Vec<String> = keys(&h.global)
            .into_iter()
            .filter(|k| k.ends_with("/refresh_token"))
            .filter_map(|k| h.global.get(&k))
            .collect();
        assert!(tokens.contains(&"rt-home".to_string()), "{tokens:?}");
        assert!(tokens.contains(&"rt-work".to_string()), "{tokens:?}");
    }

    /// A connection that kept no account takes the one approving it again
    /// only when no other connection is that account.
    #[tokio::test]
    async fn approving_again_a_connection_with_no_account_kept() {
        let h = hub().await;
        h.c.keep_app(SARAH, "google", &GOOGLE_ID, &GOOGLE_SECRET)
            .await
            .unwrap();
        let work =
            h.c.start(SARAH, "google", Some("Work"), "all", None)
                .await
                .unwrap();
        signs_in(&h, &work, "work").await.unwrap();
        let home =
            h.c.start(SARAH, "google", Some("Home"), "all", None)
                .await
                .unwrap();
        signs_in(&h, &home, "home").await.unwrap();
        let list = h.c.list(SARAH).await;
        let home_id = connector(&list, "google")
            .connections
            .iter()
            .find(|c| c.name == "Home")
            .unwrap()
            .id
            .clone();
        let blank: Vec<String> = keys(&h.global)
            .into_iter()
            .filter(|k| {
                k.contains("/home/") && (k.ends_with("/account_id") || k.ends_with("/email"))
            })
            .collect();
        assert_eq!(blank.len(), 2, "{blank:?}");
        for k in &blank {
            h.global.delete(k).unwrap();
        }
        // `work` is Work already: never Home as well.
        let again = h.c.approve_again(SARAH, "google", &home_id).await.unwrap();
        let said = signs_in(&h, &again, "work").await.unwrap_err();
        assert!(said.contains("connected already as 'Work'"), "{said}");
        // An account no connection is: Home takes it.
        let again = h.c.approve_again(SARAH, "google", &home_id).await.unwrap();
        signs_in(&h, &again, "other").await.unwrap();
        assert!(blank.iter().any(|k| h.global.get(k).is_some()));
    }

    /// A Google connection kept by email alone, before its account id was,
    /// is matched by email once and keeps the id from then on.
    #[tokio::test]
    async fn a_connection_kept_by_email_keeps_its_account_id_on_the_next_sign_in() {
        let h = hub().await;
        h.c.keep_app(SARAH, "google", &GOOGLE_ID, &GOOGLE_SECRET)
            .await
            .unwrap();
        let started =
            h.c.start(SARAH, "google", Some("Work"), "all", None)
                .await
                .unwrap();
        signs_in(&h, &started, "work").await.unwrap();
        let id_at = mine("skills/google/workspaces/default/account_id");
        assert!(h.global.get(&id_at).is_some());
        h.global.delete(&id_at).unwrap();
        let started = h.c.start(SARAH, "google", None, "all", None).await.unwrap();
        let said = signs_in(&h, &started, "work").await.unwrap();
        assert!(said.contains("connected already as 'Work'"), "{said}");
        assert!(h.global.get(&id_at).is_some());
        assert_eq!(
            connector(&h.c.list(SARAH).await, "google")
                .connections
                .len(),
            1
        );
    }

    /// Kat's setup for every agent never signs in again, or takes over,
    /// another agent's own connection of the same account.
    #[tokio::test]
    async fn a_sign_in_for_every_agent_never_reaches_another_agents_own() {
        let h = hub().await;
        h.c.keep_app(SARAH, "google", &GOOGLE_ID, &GOOGLE_SECRET)
            .await
            .unwrap();
        let bens =
            h.c.start(SARAH, "google", Some("Ben's"), "agent", Some("ben"))
                .await
                .unwrap();
        signs_in(&h, &bens, "work").await.unwrap();
        let ben_token = || {
            keys(&h.ben)
                .into_iter()
                .find(|k| k.ends_with("/refresh_token"))
                .and_then(|k| h.ben.get(&k))
        };
        assert_eq!(ben_token().as_deref(), Some("rt-work"));
        let all = h.c.start(SARAH, "google", None, "all", None).await.unwrap();
        let said = signs_in(&h, &all, "work2").await.unwrap();
        assert!(!said.contains("connected already"), "{said}");
        assert_eq!(ben_token().as_deref(), Some("rt-work"));
        assert!(keys(&h.global)
            .into_iter()
            .filter(|k| k.ends_with("/refresh_token"))
            .any(|k| h.global.get(&k).as_deref() == Some("rt-work2")));
    }

    /// Approving Harbor again signed in as Lumen keeps nothing: Lumen's
    /// books never go under Harbor's name.
    #[tokio::test]
    async fn approving_again_as_another_business_keeps_nothing() {
        let h = hub().await;
        h.c.keep_app(SARAH, "freeagent", FA_ID, FA_SECRET)
            .await
            .unwrap();
        freeagent(&h, Some("Books"), "harbor").await.unwrap();
        let id = connector(&h.c.list(SARAH).await, "freeagent").connections[0]
            .id
            .clone();
        let again = h.c.approve_again(SARAH, "freeagent", &id).await.unwrap();
        let said = signs_in(&h, &again, "lumen").await.unwrap_err();
        assert!(said.contains("this connection is 'Books'"), "{said}");
        let path = |sub: &str| mine(&format!("skills/freeagent/workspaces/default/{sub}"));
        assert_eq!(
            h.global.get(&path("refresh_token")).as_deref(),
            Some("rt-harbor")
        );
        assert_eq!(h.global.get(&path("company")).as_deref(), Some("Harbor"));
    }

    /// A lapsed Google connection set up again is signed in again where it
    /// is, so asking the agent to set it up never dead-ends.
    #[tokio::test]
    async fn a_lapsed_connection_set_up_again_is_signed_in_again() {
        let h = hub().await;
        h.c.keep_app(SARAH, "google", &GOOGLE_ID, &GOOGLE_SECRET)
            .await
            .unwrap();
        let started =
            h.c.start(SARAH, "google", Some("Work"), "all", None)
                .await
                .unwrap();
        signs_in(&h, &started, "work").await.unwrap();
        let path = |sub: &str| mine(&format!("skills/google/workspaces/default/{sub}"));
        h.global.set(&path("refresh_token"), "rt-revoked").unwrap();
        h.global.set(&path("expires_at"), "1").unwrap();
        let id = connector(&h.c.list(SARAH).await, "google").connections[0]
            .id
            .clone();
        assert_eq!(
            h.c.test(SARAH, "google", &id).await.unwrap().status,
            Status::NeedsReconnect
        );
        let started = h.c.start(SARAH, "google", None, "all", None).await.unwrap();
        signs_in(&h, &started, "work").await.unwrap();
        let checked = h.c.test(SARAH, "google", &id).await.unwrap();
        assert_eq!(checked.status, Status::Connected);
        assert_eq!(checked.name, "Work");
        assert_eq!(
            connector(&h.c.list(SARAH).await, "google")
                .connections
                .len(),
            1
        );
    }

    #[tokio::test]
    async fn missing_scopes_keep_and_say_what_is_missing() {
        let h = hub().await;
        h.c.keep_app(SARAH, "google", &GOOGLE_ID, &GOOGLE_SECRET)
            .await
            .unwrap();
        let started = h.c.start(SARAH, "google", None, "all", None).await.unwrap();
        let said = signs_in(&h, &started, "narrow").await.unwrap();
        assert!(said.contains("without Drive or Gmail"), "{said}");
        let list = h.c.list(SARAH).await;
        let conn = &connector(&list, "google").connections[0];
        assert_eq!(conn.status, Status::Connected);
        assert!(conn
            .detail
            .as_deref()
            .unwrap()
            .contains("tick Drive or Gmail"));
    }

    #[tokio::test]
    async fn a_cancelled_flow_keeps_nothing() {
        let h = hub().await;
        h.c.keep_app(SARAH, "google", &GOOGLE_ID, &GOOGLE_SECRET)
            .await
            .unwrap();
        let started = h.c.start(SARAH, "google", None, "all", None).await.unwrap();
        assert!(h.c.cancel(SARAH, &started.flow));
        assert!(signs_in(&h, &started, "work").await.is_err());
        assert_eq!(keys(&h.global).len(), 2, "only the app");
    }

    #[tokio::test]
    async fn the_callback_takes_desk_state_and_leaves_agent_flows() {
        let h = hub().await;
        h.c.keep_app(SARAH, "google", &GOOGLE_ID, &GOOGLE_SECRET)
            .await
            .unwrap();
        let started = h.c.start(SARAH, "google", None, "all", None).await.unwrap();
        assert!(h.c.take_pending("an-agents-own-state").is_none());
        let state = state_of(started.url.as_deref().unwrap());
        assert!(h.c.take_pending(&state).is_some());
        assert!(h.c.take_pending(&state).is_none(), "once");
    }

    #[tokio::test]
    async fn a_moved_freeagent_address_is_refused_with_words() {
        let h = hub().await;
        h.c.keep_app(SARAH, "freeagent", FA_ID, FA_SECRET)
            .await
            .unwrap();
        h.global
            .set(
                "skills/freeagent/oauth_client/redirect_uri",
                "http://localhost:2/api/auth/freeagent/callback",
            )
            .unwrap();
        let list = h.c.list(SARAH).await;
        let fa = connector(&list, "freeagent");
        assert_eq!(fa.app, Some(app::AppState::Moved));
        assert!(!fa.sign_in_ready);
        // The same app saved again isn't the address added: still moved.
        h.c.keep_app(SARAH, "freeagent", FA_ID, FA_SECRET)
            .await
            .unwrap();
        let c = catalog::find("freeagent").unwrap();
        assert_eq!(h.c.app_state(c), Some(app::AppState::Moved));
        match h.c.start(SARAH, "freeagent", None, "all", None).await {
            Err(Refusal::Invalid(said)) => assert!(said.contains("address changed"), "{said}"),
            other => panic!("{other:?}"),
        }
        h.c.keep_app_address(SARAH, c).unwrap();
        assert_eq!(h.c.app_state(c), Some(app::AppState::Ready));
    }

    #[tokio::test]
    async fn approve_again_keeps_the_name_and_instance() {
        let h = hub().await;
        h.c.keep_app(SARAH, "freeagent", FA_ID, FA_SECRET)
            .await
            .unwrap();
        freeagent(&h, Some("Books"), "harbor").await.unwrap();
        let id = connector(&h.c.list(SARAH).await, "freeagent").connections[0]
            .id
            .clone();
        let again = h.c.approve_again(SARAH, "freeagent", &id).await.unwrap();
        signs_in(&h, &again, "harbor").await.unwrap();
        let list = h.c.list(SARAH).await;
        let fa = connector(&list, "freeagent");
        assert_eq!(fa.connections.len(), 1);
        assert_eq!(fa.connections[0].id, id);
        assert_eq!(fa.connections[0].name, "Books");
        assert!(h
            .c
            .approve_again(SARAH, "freeagent", "all~~nothing")
            .await
            .is_err());
    }

    #[tokio::test]
    async fn a_check_that_refreshes_freeagent_keeps_the_rotated_token() {
        let h = hub().await;
        h.c.keep_app(SARAH, "freeagent", FA_ID, FA_SECRET)
            .await
            .unwrap();
        freeagent(&h, None, "harbor").await.unwrap();
        let path = |sub: &str| mine(&format!("skills/freeagent/workspaces/default/{sub}"));
        h.global.set(&path("expires_at"), "1").unwrap();
        let id = connector(&h.c.list(SARAH).await, "freeagent").connections[0]
            .id
            .clone();
        let checked = h.c.test(SARAH, "freeagent", &id).await.unwrap();
        assert_eq!(checked.status, Status::Connected);
        assert_eq!(checked.account.as_deref(), Some("Harbor"));
        assert_eq!(
            h.global.get(&path("refresh_token")).as_deref(),
            Some("rt-harbor-r")
        );
    }

    #[tokio::test]
    async fn invalid_grant_reads_lapsed() {
        let h = hub().await;
        h.c.keep_app(SARAH, "google", &GOOGLE_ID, &GOOGLE_SECRET)
            .await
            .unwrap();
        let started = h.c.start(SARAH, "google", None, "all", None).await.unwrap();
        signs_in(&h, &started, "work").await.unwrap();
        let path = |sub: &str| mine(&format!("skills/google/workspaces/default/{sub}"));
        h.global.set(&path("refresh_token"), "rt-revoked").unwrap();
        h.global.set(&path("expires_at"), "1").unwrap();
        let id = connector(&h.c.list(SARAH).await, "google").connections[0]
            .id
            .clone();
        let checked = h.c.test(SARAH, "google", &id).await.unwrap();
        assert_eq!(checked.status, Status::NeedsReconnect);
        assert!(checked
            .detail
            .as_deref()
            .unwrap()
            .contains("Approve it again"));
        assert!(checked.approve_again);
    }

    #[tokio::test]
    async fn another_app_reads_other_app() {
        let h = hub().await;
        h.c.keep_app(SARAH, "google", &GOOGLE_ID, &GOOGLE_SECRET)
            .await
            .unwrap();
        let started = h.c.start(SARAH, "google", None, "all", None).await.unwrap();
        signs_in(&h, &started, "work").await.unwrap();
        let other = GOOGLE_ID.replace(&"1".repeat(12), &"9".repeat(12));
        h.c.keep_app(SARAH, "google", &other, &GOOGLE_SECRET)
            .await
            .unwrap();
        let id = connector(&h.c.list(SARAH).await, "google").connections[0]
            .id
            .clone();
        let checked = h.c.test(SARAH, "google", &id).await.unwrap();
        assert_eq!(checked.status, Status::NeedsReconnect);
        assert!(checked
            .detail
            .as_deref()
            .unwrap()
            .contains("earlier Google app"));
    }
}

/// Every local agent has Desk's browser it sets connections up in, and a
/// documents folder its context names.
#[tokio::test]
async fn every_local_agent_has_the_browser() {
    let dir = tempfile::tempdir().unwrap();
    let (service, _llm) = crate::serve::testing::service_on_mock_llm_with(dir.path(), |c| {
        c.desk.enabled = true;
        c.desk.machine = Some(false);
    })
    .await;
    setup::register(&service);
    assert!(service.tools.has("browser.navigate"));
    assert!(service.tools.has("browser.capture_key"));
    assert!(service.tools.has("connections.setup"));
}

/// A machine's hub has no person's Chrome beside it: its agents get no
/// browser.
#[tokio::test]
async fn a_machine_hub_equips_none() {
    let dir = tempfile::tempdir().unwrap();
    let (service, _llm) = crate::serve::testing::service_on_mock_llm_with(dir.path(), |c| {
        c.desk.enabled = true;
        c.desk.machine = Some(true);
    })
    .await;
    setup::register(&service);
    crate::desk_browser::equip(&service);
    assert!(!service.tools.has("browser.navigate"));
}

/// The documents folder is made once, when the agent is, and the context
/// names it; a `documents` that is a link is never used.
#[tokio::test]
async fn documents_folder_is_made_and_named_in_the_hint() {
    let dir = tempfile::tempdir().unwrap();
    let (service, _llm) = crate::serve::testing::service_on_mock_llm_with(dir.path(), |c| {
        c.desk.enabled = true;
        c.desk.machine = Some(false);
    })
    .await;
    let documents = service.workspace.join("documents");
    let _ = std::fs::remove_dir_all(&documents);
    assert_eq!(crate::desk_driver::documents_line(&service.workspace), None);
    assert!(!documents.exists(), "naming it never makes it");
    setup::register(&service);
    assert!(documents.is_dir());
    let line = crate::desk_driver::documents_line(&service.workspace).unwrap();
    assert!(line.contains(&documents.display().to_string()), "{line}");

    #[cfg(unix)]
    {
        let elsewhere = tempfile::tempdir().unwrap();
        std::fs::remove_dir_all(&documents).unwrap();
        std::os::unix::fs::symlink(elsewhere.path(), &documents).unwrap();
        assert_eq!(crate::desk_driver::documents(&service.workspace), None);
        assert_eq!(crate::desk_driver::documents_line(&service.workspace), None);
    }
}

/// An agent's documents folder is said only to the hub's own person, for
/// an agent they may reach; asking never makes it.
#[tokio::test]
async fn the_documents_folder_is_said_only_to_the_hubs_person_and_never_made() {
    use axum::body::Body;
    use axum::http::Request;
    use tower::ServiceExt as _;
    use workforce_identity::types::{Identity, IdentityRole};
    let h = hub().await;
    let dir = tempfile::tempdir().unwrap();
    let (service, _llm) = crate::desk_attachments::tests::agent(
        dir.path(),
        "scout",
        "done",
        std::time::Duration::ZERO,
    )
    .await;
    // Alice and Bob are members, neither the hub's own person.
    let (state, [(_, alice), _]) = crate::desk_attachments::tests::gateway(&service);
    let owner = Identity::new("Sarah", IdentityRole::SystemAdmin);
    state
        .identity_store
        .as_ref()
        .unwrap()
        .create(&owner)
        .unwrap();
    let jwt =
        crate::jwt::JwtConfig::new("a-test-secret-of-at-least-32-bytes!!".into(), 3600).unwrap();
    let sarah = crate::jwt::generate_token_with_identity(
        &jwt,
        "Sarah",
        IdentityRole::SystemAdmin,
        Some(owner.id.to_string()),
    )
    .unwrap();
    let app = router(state, Arc::clone(&h.c));
    let ask = |agent: &str, token: &str| {
        Request::builder()
            .uri(format!("/api/desk/agents/{agent}/documents"))
            .header("authorization", format!("Bearer {token}"))
            .body(Body::empty())
            .unwrap()
    };
    let refused = app.clone().oneshot(ask("scout", &alice)).await.unwrap();
    assert_eq!(refused.status(), StatusCode::FORBIDDEN);
    let unknown = app.clone().oneshot(ask("nobody", &sarah)).await.unwrap();
    assert_eq!(unknown.status(), StatusCode::NOT_FOUND);
    let said = app.clone().oneshot(ask("scout", &sarah)).await.unwrap();
    assert_eq!(said.status(), StatusCode::OK);
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(said.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["exists"], false);
    assert_eq!(body["state"], "missing");
    assert!(!service.workspace.join("documents").exists());
    // A file where the folder should be is said as such, never as missing.
    std::fs::write(service.workspace.join("documents"), "x").unwrap();
    let said = app.clone().oneshot(ask("scout", &sarah)).await.unwrap();
    let body: Value = serde_json::from_slice(
        &axum::body::to_bytes(said.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["state"], "not_a_folder");
    assert_eq!(body["exists"], false);
}
