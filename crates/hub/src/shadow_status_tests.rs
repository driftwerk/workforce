//! The hidden counts made seen (DW-676, PR 9c): rules 9 and 10.

use super::*;
use workforce_common::shadow::ShadowCounts;

fn state() -> ProcessState {
    ProcessState {
        kernels: vec![KernelState {
            store: "memories",
            enabled: true,
            breaker: "open",
            counts: ShadowCounts {
                offered: 40,
                busy: 3,
                breaker_open: 2,
                started: 35,
                ok: 30,
                timed_out: 5,
                ..ShadowCounts::default()
            },
        }],
        deferred_dropped: Some(7),
        sink: Some(SinkCounts {
            recorded: 35,
            dropped: 1,
            written: 33,
            failed: 1,
        }),
        noter: Some(NoterState {
            written: 9,
            dropped: 0,
            lost: 0,
            setup_failed: 1,
        }),
    }
}

/// Rule 9: a backlog not read in time, or not readable, is told unknown,
/// and no pending count is rendered: never 0.
#[tokio::test]
async fn a_backlog_not_read_is_unknown_never_zero() {
    let hung = await_backlog(std::future::pending(), Duration::from_millis(20)).await;
    assert!(matches!(hung, BacklogSeen::Unknown(_)), "{hung:?}");
    let failed = await_backlog(
        async { Err(eyre::eyre!("connection refused")) },
        Duration::from_secs(1),
    )
    .await;
    assert!(matches!(failed, BacklogSeen::Unknown(_)), "{failed:?}");
    for seen in [&hung, &failed] {
        let mut out = String::new();
        render_metrics(&mut out, &ProcessState::default(), Some(seen));
        assert!(
            out.contains("workforce_shadow_memory_backlog_unknown 1\n"),
            "{out}"
        );
        assert!(!out.contains("backlog_pending"), "{out}");
        assert_eq!(backlog_json(Some(seen))["pending"], "unknown");
    }
    let read = await_backlog(
        async {
            Ok(Backlog {
                pending: 4,
                oldest: None,
            })
        },
        Duration::from_secs(1),
    )
    .await;
    let mut out = String::new();
    render_metrics(&mut out, &ProcessState::default(), Some(&read));
    assert!(
        out.contains("workforce_shadow_memory_backlog_pending 4\n"),
        "{out}"
    );
    assert!(
        out.contains("workforce_shadow_memory_backlog_unknown 0\n"),
        "{out}"
    );
    assert_eq!(backlog_json(Some(&read))["pending"], 4);
}

/// Rule 10: the `Deferred` drops, the noter's setup failure and every
/// kernel count are in `/api/metrics` and `/api/health`.
#[test]
fn the_hidden_counts_are_in_metrics_and_health() {
    let state = state();
    let mut out = String::new();
    render_metrics(&mut out, &state, None);
    for want in [
        "workforce_shadow_memory_deferred_dropped 7\n",
        "workforce_shadow_kernel{store=\"memories\",count=\"busy\"} 3\n",
        "workforce_shadow_kernel{store=\"memories\",count=\"breaker_open\"} 2\n",
        "workforce_shadow_kernel{store=\"memories\",count=\"timed_out\"} 5\n",
        "workforce_shadow_kernel{store=\"memories\",count=\"offered\"} 40\n",
        "workforce_shadow_breaker_open{store=\"memories\"} 1\n",
        "workforce_shadow_enabled{store=\"memories\"} 1\n",
        "workforce_shadow_sink{count=\"dropped\"} 1\n",
    ] {
        assert!(out.contains(want), "{want:?} not in:\n{out}");
    }
    // Every name keeps the prefix the Datadog check filters on.
    for l in out.lines().filter(|l| !l.starts_with('#')) {
        assert!(l.starts_with("workforce_shadow_"), "{l}");
    }

    let health = health_json(&state, false);
    assert_eq!(health["memory_deferred_dropped"], 7);
    assert_eq!(health["memory_noter"]["setup_failed"], 1);
    assert_eq!(health["kernels"]["memories"]["busy"], 3);
    assert_eq!(health["kernels"]["memories"]["breaker"], "open");
    assert_eq!(health["scope"], "this hub process");
    assert_eq!(
        health_json(&ProcessState::default(), false),
        serde_json::Value::Null
    );
}

/// A scrape reads the backlog only on tables this process already opened:
/// with none open it is unknown, and nothing is opened by it.
#[tokio::test(flavor = "multi_thread")]
async fn a_scrape_opens_no_tables() {
    let Ok(url) = std::env::var("WORKFORCE_TEST_POSTGRES_URL") else {
        return;
    };
    if crate::shadow::opened_store().is_some() {
        return;
    }
    let mut config = WorkforceConfig::default();
    config.memory.backend = "shadow".into();
    config.postgres = Some(workforce_common::postgres::PostgresConfig {
        enabled: true,
        url: Some(url),
        ..Default::default()
    });
    let seen = backlog(&config).await;
    assert!(matches!(seen, Some(BacklogSeen::Unknown(_))), "{seen:?}");
    assert!(
        crate::shadow::opened_store().is_none(),
        "the scrape opened them"
    );
}
