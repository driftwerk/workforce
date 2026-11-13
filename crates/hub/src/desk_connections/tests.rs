use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};

use axum::extract::Form;
use axum::routing::post as post_route;
use workforce_skills::adapters::google::oauth::StoredTokens;

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
        "http://127.0.0.1:1/api/auth/google/callback".into(),
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
async fn google_signs_in_on_this_computer_only() {
    let h = hub().await;
    h.c.keep_google_client(SARAH, "client", "secret").unwrap();
    h.global
        .set(
            catalog::GOOGLE_REDIRECT_URI,
            "https://hub.acme.example/api/auth/google/callback",
        )
        .unwrap();
    match h.c.start(SARAH, "google", None, "all", None).await {
        Err(Refusal::Invalid(said)) => assert!(said.contains("this computer only"), "{said}"),
        other => panic!("{other:?}"),
    }
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
        h.c.start(SARAH, "google", Some("Work"), "agent", Some("allan"))
            .await,
        Err(Refusal::Invalid(_))
    ));
    h.c.keep_google_client(SARAH, "client.apps.googleusercontent.com", "client-secret")
        .unwrap();
    let started =
        h.c.start(SARAH, "google", Some("Work"), "agent", Some("allan"))
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
        h.allan
            .get(&mine("skills/google/workspaces/default/label"))
            .as_deref(),
        Some("Work")
    );
    // A second Google is another instance, never over the first.
    let started =
        h.c.start(SARAH, "google", Some("Personal"), "agent", Some("allan"))
            .await
            .unwrap();
    h.c.google_signed_in(&started.flow, SARAH, &tokens())
        .await
        .unwrap();
    assert!(h
        .allan
        .get(&mine("skills/google/workspaces/personal/refresh_token"))
        .is_some());
    let list = h.c.list(SARAH).await;
    assert_eq!(connector(&list, "google").connections.len(), 2);
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
    let started = h.c.start(SARAH, "google", None, "all", None).await.unwrap();
    h.c.google_cancelled(&started.flow);
    assert!(h
        .c
        .google_signed_in(&started.flow, SARAH, &tokens())
        .await
        .is_err());
    assert_eq!(keys(&h.global).len(), 2, "only the client");
    // Nor does a sign-in someone else started.
    let started = h.c.start(SARAH, "google", None, "all", None).await.unwrap();
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
