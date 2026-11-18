use std::sync::Arc;

use uuid::Uuid;
use workforce_common::personal_route::{
    AgentRoute, PersonalRoute, PersonalRouteKind, PersonalRoutesConfig, RouteFallback,
};

use super::*;

const CLAUDE: PersonalRoute = PersonalRoute {
    kind: PersonalRouteKind::ClaudeCode,
    fallback: RouteFallback::Wait,
};
const CLAUDE_OR_HUB: PersonalRoute = PersonalRoute {
    kind: PersonalRouteKind::ClaudeCode,
    fallback: RouteFallback::Hub,
};

/// What every store keeps: the two settings apart, the narrower winning,
/// each person's own, and every change noted.
async fn keeps_settings(store: Arc<dyn PersonRouteStore>) {
    let alice = Uuid::new_v4();
    let bob = Uuid::new_v4();
    let actor = alice.to_string();
    assert_eq!(
        store.settings(alice, Some("lattice")).await.unwrap(),
        RouteSettings::default()
    );

    assert_eq!(
        store
            .set_every_agent(alice, Some(CLAUDE), &actor)
            .await
            .unwrap(),
        None
    );
    let quill = store.settings(alice, Some("quill")).await.unwrap();
    assert_eq!(quill.chosen(), Some(CLAUDE), "every agent, Quill included");

    assert_eq!(
        store
            .set_for_agent(alice, "quill", Some(AgentRoute::Hub), &actor)
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store.settings(alice, Some("quill")).await.unwrap().chosen(),
        None
    );
    assert_eq!(
        store
            .settings(alice, Some("lattice"))
            .await
            .unwrap()
            .chosen(),
        Some(CLAUDE)
    );
    assert_eq!(
        store
            .set_for_agent(
                alice,
                "lattice",
                Some(AgentRoute::Personal(CLAUDE_OR_HUB)),
                &actor
            )
            .await
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .settings(alice, Some("lattice"))
            .await
            .unwrap()
            .chosen(),
        Some(CLAUDE_OR_HUB),
        "the per-agent setting beats the every-agent one"
    );
    assert_eq!(
        store.settings(bob, Some("lattice")).await.unwrap(),
        RouteSettings::default(),
        "another person's settings are their own"
    );

    assert_eq!(
        store.set_every_agent(alice, None, &actor).await.unwrap(),
        Some(CLAUDE)
    );
    assert_eq!(
        store
            .set_for_agent(alice, "quill", None, &actor)
            .await
            .unwrap(),
        Some(AgentRoute::Hub)
    );
    assert_eq!(
        store.settings(alice, Some("quill")).await.unwrap().chosen(),
        None
    );
    assert_eq!(
        store.settings(alice, None).await.unwrap(),
        RouteSettings::default()
    );

    let changes = store.changes(alice, 10).await.unwrap();
    assert_eq!(changes.len(), 5);
    assert_eq!(changes[0].agent_id.as_deref(), Some("quill"));
    assert_eq!(changes[0].before, Some(AgentRoute::Hub));
    assert_eq!(changes[0].after, None);
    assert_eq!(changes[1].agent_id, None);
    assert_eq!(changes[1].before, Some(AgentRoute::Personal(CLAUDE)));
    assert_eq!(changes[4].after, Some(AgentRoute::Personal(CLAUDE)));
    assert!(changes.iter().all(|change| change.actor == actor));
    assert_eq!(store.changes(alice, 2).await.unwrap().len(), 2);
    assert!(store.changes(bob, 10).await.unwrap().is_empty());

    // What a person's removal finds and erases: their settings, not the
    // record of their changes.
    store
        .set_every_agent(alice, Some(CLAUDE), &actor)
        .await
        .unwrap();
    for agent in ["lattice", "quill"] {
        store
            .set_for_agent(alice, agent, Some(AgentRoute::Hub), &actor)
            .await
            .unwrap();
    }
    let alice_key = alice.to_string();
    let held = |people: Vec<(String, usize)>| {
        people
            .into_iter()
            .find(|(who, _)| *who == alice_key)
            .map(|(_, n)| n)
    };
    assert_eq!(held(store.people().await.unwrap()), Some(3));
    assert_eq!(store.forget_person(alice).await.unwrap(), 3);
    assert_eq!(held(store.people().await.unwrap()), None);
    assert_eq!(
        store.settings(alice, Some("quill")).await.unwrap(),
        RouteSettings::default()
    );
    assert_eq!(store.changes(alice, 100).await.unwrap().len(), 8);
}

#[tokio::test]
async fn memory_store_keeps_settings() {
    keeps_settings(Arc::new(MemoryRouteStore::default())).await;
}

#[cfg(feature = "postgres")]
#[tokio::test]
async fn postgres_store_keeps_settings() {
    workforce_postgres::test_support::with_test_pool(
        postgres::PostgresRouteStore::build_pool,
        |pool| async move {
            keeps_settings(Arc::new(
                postgres::PostgresRouteStore::with_pool(pool).await.unwrap(),
            ))
            .await;
        },
    )
    .await;
}

#[test]
fn the_switch_is_shared_by_every_holder_and_narrowed_per_agent() {
    let routes = PersonalRoutes::new(PersonalRoutesConfig::default());
    let held = routes.clone();
    assert!(!held.switch().enabled);
    routes.set_switch(PersonalRoutesConfig {
        enabled: true,
        ..Default::default()
    });
    assert!(held.switch().enabled, "a change reaches every holder");
    let opted_out = workforce_common::personal_route::AgentPersonalRoutes {
        enabled: Some(false),
        allow: None,
    };
    assert!(!held.for_agent(Some(&opted_out)).enabled);
    assert!(held.store().is_none());
    routes.attach_store(Arc::new(MemoryRouteStore::default()));
    assert!(held.store().is_some());
}
