use workforce_surface::{current, set_default, Arrival, Surface};

#[tokio::test]
async fn the_default_once_set_holds_outside_every_scope_and_cannot_be_reset() {
    assert_eq!(current().surface, Surface::Combined);
    set_default(Surface::Public).unwrap();
    assert_eq!(set_default(Surface::Combined), Err(Surface::Combined));
    assert_eq!(current(), Arrival::new(Surface::Public, None));
    let spawned = Arrival::new(Surface::Admin, None)
        .scope(async { tokio::spawn(async { current() }).await.unwrap() })
        .await;
    assert_eq!(spawned, Arrival::new(Surface::Public, None));
}
