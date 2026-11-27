use axum::body::{to_bytes, Body};
use axum::extract::State;
use axum::http::{HeaderValue, Request, StatusCode};
use axum::middleware::{from_fn, Next};
use axum::response::Response;
use tower::ServiceExt;

use super::*;
use crate::routing::{delete, get};

async fn refuse(_: Request<Body>, _: Next) -> Response {
    StatusCode::FORBIDDEN.into_response()
}

async fn mark(request: Request<Body>, next: Next) -> Response {
    let mut response = next.run(request).await;
    response
        .headers_mut()
        .insert("x-marked", HeaderValue::from_static("yes"));
    response
}

fn app() -> Routes {
    Routes::new()
        .member("/things/{id}", get(|| async { "a thing" }))
        .admin("/things/export", get(|| async { "every thing" }))
        .member("/shelf", get(|| async { "the shelf" }))
        .admin("/shelf", delete(|| async { "emptied" }))
        .open("/", get(|| async { "home" }))
        .fallback(|| async { (StatusCode::NOT_FOUND, "nothing here") })
}

async fn call(router: &Router, method: Method, path: &str) -> (StatusCode, String, bool) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method(method)
                .uri(path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let marked = response.headers().contains_key("x-marked");
    let status = response.status();
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    (status, String::from_utf8(body.to_vec()).unwrap(), marked)
}

#[tokio::test]
async fn the_member_surface_answers_an_admin_route_as_an_unknown_one() {
    let public = app().render(Surface::Public, from_fn(refuse));
    let unknown = (StatusCode::NOT_FOUND, "nothing here".to_string(), false);
    assert_eq!(call(&public, Method::GET, "/things/export").await, unknown);
    assert_eq!(call(&public, Method::DELETE, "/shelf").await, unknown);
    assert_eq!(call(&public, Method::GET, "/nowhere").await, unknown);
    assert_eq!(
        call(&public, Method::GET, "/things/7").await,
        (StatusCode::OK, "a thing".to_string(), false)
    );
    assert_eq!(
        call(&public, Method::GET, "/shelf").await,
        (StatusCode::OK, "the shelf".to_string(), false)
    );
}

#[tokio::test]
async fn elsewhere_the_guard_stands_before_each_admin_handler_and_no_other() {
    for surface in [Surface::Combined, Surface::Admin] {
        let router = app().render(surface, from_fn(refuse));
        assert_eq!(
            call(&router, Method::GET, "/things/export").await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&router, Method::DELETE, "/shelf").await.0,
            StatusCode::FORBIDDEN
        );
        assert_eq!(call(&router, Method::GET, "/shelf").await.0, StatusCode::OK);
        assert_eq!(call(&router, Method::GET, "/").await.0, StatusCode::OK);
    }
}

#[tokio::test]
async fn a_layer_covers_the_routes_it_was_put_on_and_the_guard_inside_it() {
    let router = app()
        .layer(from_fn(mark))
        .merge(Routes::new().member("/later", get(|| async { "later" })))
        .render(Surface::Combined, from_fn(refuse));
    assert_eq!(
        call(&router, Method::GET, "/things/export").await,
        (StatusCode::FORBIDDEN, String::new(), true)
    );
    assert!(call(&router, Method::GET, "/shelf").await.2);
    assert!(call(&router, Method::GET, "/nowhere").await.2);
    assert!(!call(&router, Method::GET, "/later").await.2);
}

#[tokio::test]
async fn nested_routes_take_the_prefix_and_state_reaches_their_handlers() {
    let games = Routes::<u8>::new()
        .member("/", get(|State(n): State<u8>| async move { n.to_string() }))
        .member("/ws", get(|| async { "socket" }));
    let router = Routes::new()
        .nest("/games", games)
        .with_state(7)
        .render(Surface::Public, from_fn(refuse));
    assert_eq!(call(&router, Method::GET, "/games").await.1, "7");
    assert_eq!(call(&router, Method::GET, "/games/ws").await.1, "socket");
}

#[test]
fn the_manifest_names_each_method_of_each_route_in_path_order() {
    let lines: Vec<String> = Routes::<()>::new()
        .member("/b", get(|| async {}).delete(|| async {}))
        .machine("/hooks/{id}", crate::routing::post(|| async {}))
        .admin("/a", crate::routing::put(|| async {}))
        .manifest()
        .iter()
        .map(ToString::to_string)
        .collect();
    assert_eq!(
        lines,
        [
            "PUT /a admin",
            "DELETE /b member",
            "GET /b member",
            "POST /hooks/{id} machine",
        ]
    );
}

#[test]
#[should_panic(expected = "both have a fallback")]
fn two_fallbacks_do_not_merge() {
    let _ = Routes::<()>::new()
        .fallback(|| async {})
        .merge(Routes::new().fallback(|| async {}));
}

#[tokio::test]
async fn routes_declared_alone_keep_their_manifest_and_answer_nothing() {
    let shelf = || {
        Routes::<u8>::new()
            .member(
                "/shelf",
                get(|State(n): State<u8>| async move { n.to_string() }),
            )
            .admin("/shelf", delete(|| async { "emptied" }))
    };
    let declared = shelf().bind(Bound::Declared);
    assert_eq!(declared.manifest(), shelf().bind(Bound::To(3)).manifest());
    let router = declared.render(Surface::Combined, from_fn(refuse));
    assert_eq!(
        call(&router, Method::GET, "/shelf").await.0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    assert_eq!(
        call(&router, Method::DELETE, "/shelf").await.0,
        StatusCode::FORBIDDEN
    );
    let open = shelf()
        .bind(Bound::Declared)
        .render(Surface::Combined, tower::layer::util::Identity::new());
    assert_eq!(
        call(&open, Method::DELETE, "/shelf").await.0,
        StatusCode::METHOD_NOT_ALLOWED
    );
    let bound = shelf()
        .bind(Bound::To(3).map(|n| n * 2))
        .render(Surface::Combined, from_fn(refuse));
    assert_eq!(call(&bound, Method::GET, "/shelf").await.1, "6");
}
