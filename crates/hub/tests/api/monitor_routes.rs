//! The monitor write routes and the membership gate they now share with the
//! agent tool.
//!
//! Before this, `PUT /api/monitors/{id}` and the revision restore did only the
//! definition-write rule (whose *row* may I write?) and never asked whether the
//! caller could reach the channel the monitor points at — so the same member
//! refused in chat succeeded through REST. These pin the gate end to end over a
//! real gateway router, a real monitor engine, and an identity store carrying
//! the caller's linked platform id (which is what makes a REST caller
//! answerable at all).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use eyre::Result;
use tower::ServiceExt; // for oneshot

use workforce_common::channels::{
    ChannelCapabilities, ChannelMessage, ChannelSurface, Reaction, SurfaceKey, SurfaceRegistry,
};
use workforce_hub::gateway::{router, GatewayState};
use workforce_hub::monitor::{
    ActionOutcome, ActionSink, EngineAgent, MonitorEngine, MonitorEngineConfig, MonitorRuntime,
};
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_identity::store::IdentityStore;
use workforce_identity::types::{Identity, IdentityRole};
use workforce_orchestrator::monitor::{
    Action, MonitorDefinition, MonitorEvent, MonitorKey, MonitorStore,
};
use workforce_orchestrator::workflow_store::{
    EditContext, RunAsWrite, WorkflowStore, KIND_MONITOR,
};

use crate::support::{token_for, GatewayParts, GATEWAY_TOKEN};

const ALICE_SLACK: &str = "U0ALICE";
const WATCHED: &str = "C0OPS";
const ELSEWHERE: &str = "C0DEV";

/// A slack surface whose membership set can change under the caller's feet —
/// the "I have since left the channel" case the Pause path must survive — and
/// whose channel history is whatever a test has posted to it, read back with
/// the same exclusive `oldest` bound Slack applies.
struct MemberSurface {
    members: std::sync::Mutex<HashSet<String>>,
    messages: std::sync::Mutex<Vec<ChannelMessage>>,
}

impl MemberSurface {
    fn post(&self, channel: &str, ts: &str) {
        self.messages.lock().unwrap().push(ChannelMessage {
            channel: channel.into(),
            message_id: ts.into(),
            user: ALICE_SLACK.into(),
            user_name: None,
            channel_name: None,
            text: format!("posted at {ts}"),
            timestamp: ts.into(),
            thread_id: None,
            files: vec![],
            bot_id: None,
        });
    }
}

#[async_trait]
impl ChannelSurface for MemberSurface {
    fn platform(&self) -> &str {
        "slack"
    }
    fn capabilities(&self) -> ChannelCapabilities {
        ChannelCapabilities::default()
    }
    async fn add_reaction(&self, _: &Reaction) -> Result<()> {
        Ok(())
    }
    async fn remove_reaction(&self, _: &Reaction) -> Result<()> {
        Ok(())
    }
    async fn read_messages(
        &self,
        channel: &str,
        _: usize,
        oldest: Option<&str>,
    ) -> Result<Vec<ChannelMessage>> {
        let floor: f64 = oldest.and_then(|ts| ts.parse().ok()).unwrap_or(f64::MIN);
        Ok(self
            .messages
            .lock()
            .unwrap()
            .iter()
            .filter(|m| m.channel == channel && m.message_id.parse().unwrap_or(0.0) > floor)
            .cloned()
            .collect())
    }
    async fn is_channel_member(&self, channel: &str, user: &str) -> Result<bool> {
        Ok(user == ALICE_SLACK && self.members.lock().unwrap().contains(channel))
    }
}

/// Records what the engine dispatched, so a test can assert a save never fired
/// a monitor over the channel's backlog inside its own request.
#[derive(Default)]
struct RecordingSink {
    fired: std::sync::Mutex<Vec<String>>,
}

#[async_trait]
impl ActionSink for RecordingSink {
    async fn dispatch(
        &self,
        _: &Action,
        _: Option<&str>,
        _: &str,
        ev: &MonitorEvent,
    ) -> ActionOutcome {
        self.fired.lock().unwrap().push(ev.id().to_string());
        ActionOutcome::Done(None)
    }

    async fn tell(&self, _: &str, _: Option<&str>, _: &str, _: &str) -> ActionOutcome {
        ActionOutcome::Done(None)
    }
}

/// A platform timestamp `offset` seconds from now, on the same whole-second
/// grid the engine seeds cursors on.
fn at(offset: i64) -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64;
    format!("{}.000000", secs + offset)
}

fn monitor_toml(channel: &str, enabled: bool) -> String {
    format!(
        "[monitor]\nid = \"ops-releases\"\nname = \"Releases\"\nenabled = {enabled}\n\
         [monitor.binding]\nsurface = \"slack\"\nchannel = \"{channel}\"\n\
         [monitor.filter]\ntype = \"all\"\n\
         [monitor.action]\ntype = \"run_workflow\"\nworkflow = \"release-note\"\n"
    )
}

/// Everything one test needs: the wired router state, the identities behind its
/// tokens, the surface whose membership it can change, and the definition store
/// it can seed revisions through.
struct Fixture {
    state: Arc<GatewayState>,
    alice: Identity,
    admin: Identity,
    surface: Arc<MemberSurface>,
    sink: Arc<RecordingSink>,
    store: Arc<MonitorStore>,
    defs: Arc<WorkflowStore>,
    runtime: Arc<MonitorRuntime>,
    dir: std::path::PathBuf,
}

impl Fixture {
    async fn build(pool: workforce_common::postgres::PostgresPool) -> Self {
        let identities = SqliteIdentityStore::in_memory().expect("in-memory identity store");
        let alice =
            Identity::new("Alice", IdentityRole::Member).with_linked_id("slack", ALICE_SLACK);
        let admin = Identity::new("Admin", IdentityRole::SystemAdmin);
        identities.create(&alice).unwrap();
        identities.create(&admin).unwrap();
        let identity_store: Arc<dyn IdentityStore> = Arc::new(identities);

        let surface = Arc::new(MemberSurface {
            members: std::sync::Mutex::new([WATCHED.to_string()].into_iter().collect()),
            messages: std::sync::Mutex::new(Vec::new()),
        });
        let mut registry = SurfaceRegistry::new();
        registry.register(
            SurfaceKey::workspace("slack", "ws"),
            Arc::clone(&surface) as Arc<dyn ChannelSurface>,
        );

        let dir = std::env::temp_dir().join(format!("monroutes-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let defs = WorkflowStore::new(pool.clone()).await.expect("def store");
        let runtime = Arc::new(MonitorRuntime::new(dir.clone()));
        runtime.set_store(Arc::clone(&defs));
        runtime.reload().await.unwrap();

        let store = Arc::new(MonitorStore::new(pool).await.expect("monitor store"));
        let sink = Arc::new(RecordingSink::default());
        let engine = Arc::new(MonitorEngine::new(
            Arc::clone(&runtime),
            Arc::clone(&store),
            Arc::new(tokio::sync::RwLock::new(registry)),
            Arc::clone(&sink) as Arc<dyn ActionSink>,
            EngineAgent {
                id: "scout".into(),
                name: "Scout".into(),
                is_default: true,
            },
            MonitorEngineConfig::default(),
        ));

        let state = GatewayParts {
            def_store: Some(Arc::clone(&defs)),
            monitor_engine: Some(engine),
            ..GatewayParts::new(
                workforce_hub::agent_registry::AgentRegistry::from_parts(
                    HashMap::new(),
                    "scout".to_string(),
                ),
                identity_store,
                dir.join("skills"),
            )
        }
        .state();

        Self {
            state,
            alice,
            admin,
            surface,
            sink,
            store,
            defs,
            runtime,
            dir,
        }
    }

    fn token(&self, identity: &Identity) -> String {
        token_for(identity)
    }

    /// Fire a request at the gateway router, returning `(status, body text)`.
    async fn send(
        &self,
        method: &str,
        path: &str,
        bearer: &str,
        body: Option<String>,
    ) -> (StatusCode, String) {
        let builder = Request::builder()
            .method(method)
            .uri(path)
            .header("authorization", format!("Bearer {bearer}"));
        let req = builder
            .body(body.map(Body::from).unwrap_or_else(Body::empty))
            .unwrap();
        let res = router(Arc::clone(&self.state)).oneshot(req).await.unwrap();
        let status = res.status();
        let bytes = axum::body::to_bytes(res.into_body(), usize::MAX)
            .await
            .unwrap();
        (status, String::from_utf8_lossy(&bytes).to_string())
    }

    /// `PUT /api/monitors/ops-releases` on the caller's own row.
    async fn put(&self, identity: &Identity, toml: &str) -> (StatusCode, String) {
        self.send(
            "PUT",
            "/api/monitors/ops-releases",
            &self.token(identity),
            Some(toml.to_string()),
        )
        .await
    }

    /// Seed a revision straight through the definition store, bypassing the
    /// gate — the only way to get a revision bound somewhere alice cannot reach.
    async fn seed_revision(&self, channel: &str) {
        let toml = monitor_toml(channel, true);
        let def = MonitorDefinition::from_toml(&toml).unwrap();
        let owner = self.alice.id.to_string();
        self.defs
            .save(
                "ops-releases",
                KIND_MONITOR,
                &toml,
                Some(serde_json::to_value(&def).unwrap()),
                Some(&owner),
                workforce_core::access::Visibility::Private,
                RunAsWrite::Unchanged,
                EditContext::new("test", Some(&owner)),
            )
            .await
            .unwrap();
        self.runtime.reload().await.unwrap();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

async fn with_fixture<F, Fut>(test: F)
where
    F: FnOnce(Fixture) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    workforce_postgres::test_support::with_test_pool(MonitorStore::build_pool, |pool| async move {
        test(Fixture::build(pool).await).await;
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn put_refuses_a_binding_to_a_channel_the_caller_is_not_in() {
    with_fixture(|fx| async move {
        let (status, body) = fx.put(&fx.alice, &monitor_toml(ELSEWHERE, true)).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert_eq!(
            body,
            format!("you can only bind a monitor to a channel you're a member of ({ELSEWHERE})")
        );
        assert!(
            fx.runtime
                .stored_definition("ops-releases", Some(&fx.alice.id.to_string()))
                .await
                .is_none(),
            "a refused PUT stores nothing"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn put_allows_a_member_of_the_channel() {
    with_fixture(|fx| async move {
        let (status, body) = fx.put(&fx.alice, &monitor_toml(WATCHED, true)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert!(body.contains("\"saved\":\"ops-releases\""), "{body}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn put_allows_an_administrator_who_is_not_a_member() {
    with_fixture(|fx| async move {
        // The surface answers membership for alice alone, so the admin is a
        // member of nothing — and binds anywhere regardless.
        let (status, body) = fx.put(&fx.admin, &monitor_toml(ELSEWHERE, true)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn put_that_only_flips_enabled_is_not_membership_checked() {
    with_fixture(|fx| async move {
        let (status, body) = fx.put(&fx.alice, &monitor_toml(WATCHED, true)).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // Alice leaves the channel. The UI's Pause is a full PUT of the TOML,
        // so re-checking an unchanged binding would trap her own monitor on.
        fx.surface.members.lock().unwrap().clear();
        let (status, body) = fx.put(&fx.alice, &monitor_toml(WATCHED, false)).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // Pointing it somewhere new is still checked.
        let (status, _) = fx.put(&fx.alice, &monitor_toml(ELSEWHERE, true)).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_token_carrying_no_identity_cannot_write_a_monitor_at_all() {
    with_fixture(|fx| async move {
        // The gate waves an identity-less caller through as "the hub itself".
        // The only such caller that is not an administrator carries no
        // principal at all, and the write rule refuses it first — so that
        // branch is never a way past the membership check.
        let token = workforce_hub::jwt::generate_token_with_identity(
            &crate::support::jwt(),
            "nobody",
            IdentityRole::Member,
            None,
        )
        .unwrap();
        let (status, body) = fx
            .send(
                "PUT",
                "/api/monitors/ops-releases",
                &token,
                Some(monitor_toml(ELSEWHERE, true)),
            )
            .await;
        assert_eq!(status, StatusCode::UNAUTHORIZED, "{body}");
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn the_static_gateway_token_binds_anywhere_as_an_administrator() {
    with_fixture(|fx| async move {
        let (status, body) = fx
            .send(
                "PUT",
                "/api/monitors/ops-releases",
                GATEWAY_TOKEN,
                Some(monitor_toml(ELSEWHERE, true)),
            )
            .await;
        assert_eq!(status, StatusCode::OK, "{body}");
    })
    .await;
}

/// `monitor_put` probes the channel inside the request, and a probe sweeps, so
/// the catch-up policy has to have moved this monitor's place first — otherwise
/// a resume replays its own paused gap during its own save.
#[tokio::test(flavor = "multi_thread")]
async fn a_resume_points_the_catch_up_before_the_save_probes_the_channel() {
    with_fixture(|fx| async move {
        let owner = fx.alice.id.to_string();
        let key = MonitorKey::new("ops-releases", Some(&owner));
        let bindings = [("slack".to_string(), WATCHED.to_string())];

        let (status, body) = fx.put(&fx.alice, &monitor_toml(WATCHED, true)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let (status, body) = fx.put(&fx.alice, &monitor_toml(WATCHED, false)).await;
        assert_eq!(status, StatusCode::OK, "{body}");

        // A pause inside the catch-up window, with a backlog posted during it —
        // so nothing but the resume itself can be what keeps these unread.
        fx.store
            .set_cursors(&key, &bindings, &at(-7200), true)
            .await
            .unwrap();
        fx.surface.post(WATCHED, &at(-3600));
        fx.surface.post(WATCHED, &at(-1800));

        let (status, body) = fx.put(&fx.alice, &monitor_toml(WATCHED, true)).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        let fired = fx.sink.fired.lock().unwrap().clone();
        assert!(
            fired.is_empty(),
            "a resume never replays its own gap: {fired:?}"
        );
    })
    .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn restoring_a_revision_bound_elsewhere_is_refused() {
    with_fixture(|fx| async move {
        fx.seed_revision(ELSEWHERE).await;
        fx.seed_revision(WATCHED).await;
        let owner = fx.alice.id.to_string();
        let mut bound_elsewhere = None;
        for listed in fx
            .runtime
            .list_revisions("ops-releases", Some(&owner))
            .await
            .unwrap()
        {
            let content = fx
                .runtime
                .get_revision("ops-releases", Some(&owner), listed.rev)
                .await
                .unwrap()
                .and_then(|revision| revision.content)
                .expect("revision content");
            if content.contains(ELSEWHERE) {
                bound_elsewhere = Some(listed.rev);
            }
        }
        let rev = bound_elsewhere.expect("the revision bound elsewhere");

        let (status, body) = fx
            .send(
                "POST",
                &format!("/api/monitors/ops-releases/revisions/{rev}/restore?owner={owner}"),
                &fx.token(&fx.alice),
                None,
            )
            .await;
        assert_eq!(status, StatusCode::FORBIDDEN, "{body}");
        assert!(body.contains(ELSEWHERE), "{body}");
        assert!(
            fx.runtime
                .stored_definition("ops-releases", Some(&owner))
                .await
                .is_some_and(|def| def.monitor.binding().is_some_and(|b| b.channel == WATCHED)),
            "a refused restore leaves the stored monitor alone"
        );
    })
    .await;
}
