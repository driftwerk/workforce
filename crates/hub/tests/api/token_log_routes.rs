//! A refused credential leaves a log line that names its kind and a keyed
//! tag, and no part of the token itself, on the REST and the socket path.

use std::io;
use std::sync::{Arc, Mutex};

use reqwest::StatusCode;
use tracing_subscriber::fmt::MakeWriter;
use workforce_hub::agent_registry::AgentRegistry;
use workforce_identity::sqlite::SqliteIdentityStore;
use workforce_pairing::{DevicePairingService, PairingConfig, SqlitePairingStore};

use crate::support::{agent_service, serve, GatewayParts};

/// Everything logged, as one string.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Captured {
    fn text(&self) -> String {
        String::from_utf8_lossy(&self.0.lock().unwrap()).into_owned()
    }
}

impl io::Write for Captured {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Captured {
    type Writer = Captured;
    fn make_writer(&'a self) -> Captured {
        self.clone()
    }
}

/// Every run of `width` or more characters of `secret`'s body (past the
/// public `wfp_` marker) that `log` repeats; empty when none.
fn leaked(log: &str, secret: &str) -> Vec<String> {
    let body = &secret["wfp_".len()..];
    let width = 4;
    (0..=body.len() - width)
        .map(|at| &body[at..at + width])
        .filter(|piece| log.contains(piece))
        .map(str::to_string)
        .collect()
}

// One thread, so the spawned gateway logs to the subscriber set below.
#[tokio::test(flavor = "current_thread")]
async fn a_refused_token_is_logged_by_kind_and_tag_never_by_any_part_of_it() {
    let data = tempfile::tempdir().unwrap();
    let service = agent_service(data.path()).await;
    let state = GatewayParts {
        device_pairing: Some(Arc::new(DevicePairingService::new(
            Arc::new(SqlitePairingStore::in_memory().unwrap()),
            PairingConfig::default(),
        ))),
        ..GatewayParts::new(
            AgentRegistry::from_parts(
                [("default".to_string(), service)].into_iter().collect(),
                "default".into(),
            ),
            Arc::new(SqliteIdentityStore::in_memory().unwrap()),
            std::path::PathBuf::from("/tmp/token-log-test-skills"),
        )
    }
    .state();
    let addr = serve(state).await;

    // Letters the log lines' own words never contain.
    let secret = "wfp_Qz7JxKvWqXjZkQvXzJqWxKzVqJxQzKvW";
    let captured = Captured::default();
    let _guard = tracing::subscriber::set_default(
        tracing_subscriber::fmt()
            .with_writer(captured.clone())
            .with_max_level(tracing::Level::TRACE)
            .with_ansi(false)
            .finish(),
    );

    // REST.
    let rest = reqwest::Client::new()
        .get(format!("http://{addr}/api/me"))
        .bearer_auth(secret)
        .send()
        .await
        .unwrap();
    assert_eq!(rest.status(), StatusCode::UNAUTHORIZED);
    let after_rest = captured.text();
    assert!(
        after_rest.contains("device token #"),
        "the refusal says what it was: {after_rest}"
    );

    // Socket.
    let socket = tokio_tungstenite::connect_async(format!("ws://{addr}/ws?token={secret}")).await;
    assert!(socket.is_err(), "the handshake is refused");
    let after_ws = captured.text();
    assert!(
        after_ws.contains("WebSocket auth failed"),
        "the socket refusal is logged: {after_ws}"
    );

    let log = captured.text();
    assert_eq!(leaked(&log, secret), Vec::<String>::new(), "{log}");
    assert!(!log.contains(secret));

    // The same token is the same tag on both paths.
    let tags: Vec<&str> = log
        .match_indices("device token #")
        .map(|(at, m)| &log[at + m.len()..at + m.len() + 8])
        .collect();
    assert!(tags.len() >= 2, "{log}");
    assert!(tags.windows(2).all(|w| w[0] == w[1]), "{tags:?}");
}
