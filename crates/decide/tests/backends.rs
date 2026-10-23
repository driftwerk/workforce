//! A fake System One server per backend, so the client, the policy and the
//! branch helper are exercised over real HTTP.

use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::post;
use axum::{Json, Router};
use parking_lot::Mutex;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use url::Url;
use workforce_decide::branch::{self, BranchOption, StepResult, NONE_OF_THESE, QUESTION_ID};
use workforce_decide::question::batch;
use workforce_decide::{
    request, routing, scope, Backend, Call, DecideError, Decider, DeciderPolicy, Question,
    StateBudget, SystemOneClient, Use,
};

#[derive(Clone)]
struct Fake {
    /// The choice this backend always makes, or an HTTP status to fail with.
    /// A choice named `__silent__` answers with no answers at all.
    behaviour: Arc<Mutex<Result<(String, f64), u16>>>,
    seen: Arc<Mutex<Vec<(HeaderMap, Value)>>>,
    /// How long each request takes to answer.
    delay: Arc<Mutex<Duration>>,
    in_flight: Arc<AtomicUsize>,
    /// The most requests ever in flight at once.
    peak: Arc<AtomicUsize>,
}

/// Counts a request as in flight until dropped, which is also what happens
/// when the client abandons it.
struct InFlight(Arc<AtomicUsize>);

impl Drop for InFlight {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

async fn systemone(
    State(fake): State<Fake>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    fake.seen.lock().push((headers, body.clone()));
    let now = fake.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
    let _in_flight = InFlight(Arc::clone(&fake.in_flight));
    fake.peak.fetch_max(now, Ordering::SeqCst);
    let delay = *fake.delay.lock();
    tokio::time::sleep(delay).await;
    let behaviour = fake.behaviour.lock().clone();
    match behaviour {
        Err(status) => (
            StatusCode::from_u16(status).unwrap(),
            Json(json!({"detail": "nope"})),
        ),
        Ok((choice, p)) => {
            let ids: Vec<String> = body["questions"]
                .as_object()
                .map(|q| q.keys().cloned().collect())
                .unwrap_or_default();
            let mut answers = serde_json::Map::new();
            for id in ids.into_iter().filter(|_| choice != "__silent__") {
                answers.insert(
                    id,
                    json!({"type":"choice","choice":choice,"probabilities":{choice.clone():p, NONE_OF_THESE:1.0-p},"confidence":p}),
                );
            }
            (
                StatusCode::OK,
                Json(
                    json!({"model":"fake-1","answers":answers,"usage":{"input_tokens":10,"output_tokens":2}}),
                ),
            )
        }
    }
}

async fn spawn(fake: Fake) -> Url {
    let app = Router::new()
        .route("/v1/systemone", post(systemone))
        .with_state(fake);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    Url::parse(&format!("http://{addr}/")).unwrap()
}

fn fake(choice: &str, p: f64) -> Fake {
    Fake {
        behaviour: Arc::new(Mutex::new(Ok((choice.to_string(), p)))),
        seen: Arc::new(Mutex::new(Vec::new())),
        delay: Arc::default(),
        in_flight: Arc::default(),
        peak: Arc::default(),
    }
}

fn slow(choice: &str, delay: Duration) -> Fake {
    let fake = fake(choice, 0.9);
    *fake.delay.lock() = delay;
    fake
}

async fn client(name: &str, fake: &Fake, requires_key: bool) -> Arc<SystemOneClient> {
    capped(name, fake, requires_key, 8).await
}

async fn capped(
    name: &str,
    fake: &Fake,
    requires_key: bool,
    max_concurrent: usize,
) -> Arc<SystemOneClient> {
    let url = spawn(fake.clone()).await;
    Arc::new(
        SystemOneClient::new(Backend {
            name: name.into(),
            url,
            requires_key,
            model: format!("{name}-latest"),
            timeout: Duration::from_secs(5),
            max_concurrent,
        })
        .unwrap(),
    )
}

const CALL: Call = Call {
    usage: Use::BranchFallback,
    budget: Duration::from_secs(5),
};

fn within(budget_ms: u64) -> Call {
    Call::new(Use::ScopeGate, Duration::from_millis(budget_ms))
}

fn review(response: &str) -> Value {
    branch::state(
        &StepResult {
            step: "review".into(),
            response: response.into(),
            ..Default::default()
        },
        &options(),
        StateBudget::DEFAULT,
    )
    .expect("a state")
}

fn no_keys(_: &str) -> Option<String> {
    None
}

fn bearer(fake: &Fake, call: usize) -> Option<String> {
    fake.seen.lock()[call]
        .0
        .get(header::AUTHORIZATION)
        .map(|v| v.to_str().unwrap().to_string())
}

fn options() -> Vec<BranchOption> {
    vec![
        BranchOption::new("approved", None),
        BranchOption::new("changes-requested", None),
    ]
}

#[tokio::test]
async fn the_request_carries_model_key_and_questions_and_the_answer_routes() {
    let kev = fake("approved", 0.86);
    let c = client("kev", &kev, false).await;
    let state = review("LGTM, approving.");
    let questions = branch::questions("", &options());
    let response = c
        .ask(CALL, Some("secret-key"), &state, &questions)
        .await
        .unwrap();

    let (headers, body) = kev.seen.lock()[0].clone();
    assert_eq!(
        headers.get(header::AUTHORIZATION).unwrap(),
        "Bearer secret-key"
    );
    assert_eq!(body["model"], "kev-latest");
    assert_eq!(body["state"]["final_lines"], "LGTM, approving.");
    assert_eq!(body["questions"][QUESTION_ID]["type"], "choice");
    assert_eq!(response.model, "fake-1");
    assert_eq!(
        branch::choose(&response.answers[QUESTION_ID], &options(), 0.7),
        Some(("approved".into(), 0.86))
    );
}

#[tokio::test]
async fn an_error_body_that_echoes_the_key_never_carries_it() {
    let echo = Router::new().route(
        "/v1/systemone",
        post(|headers: HeaderMap| async move {
            let sent = headers[header::AUTHORIZATION].to_str().unwrap().to_string();
            (StatusCode::UNAUTHORIZED, format!("rejected {sent}"))
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = Url::parse(&format!("http://{}/", listener.local_addr().unwrap())).unwrap();
    tokio::spawn(async move { axum::serve(listener, echo).await.unwrap() });
    let c = SystemOneClient::new(Backend {
        name: "jev".into(),
        url,
        requires_key: true,
        model: "jev-latest".into(),
        timeout: Duration::from_secs(5),
        max_concurrent: 8,
    })
    .unwrap();
    let err = c
        .ask(
            CALL,
            Some("sk-very-secret"),
            &json!({}),
            &branch::questions("", &options()),
        )
        .await
        .unwrap_err();
    assert_eq!(
        err.to_string(),
        "jev: HTTP 401",
        "the message is logged, and a body can quote the state"
    );
    assert!(
        matches!(&err, DecideError::Http { body, .. } if body == "rejected Bearer <redacted>"),
        "{err:?}"
    );
}

#[tokio::test]
async fn a_failing_primary_falls_back_when_the_policy_allows_and_errors_when_it_does_not() {
    let kev = fake("approved", 0.9);
    *kev.behaviour.lock() = Err(503);
    let jev = fake("changes-requested", 0.8);
    let primary = client("kev", &kev, false).await;
    let secondary = client("jev", &jev, true).await;
    let keys = |b: &str| (b == "jev").then(|| "k".to_string());
    let state = review("please fix the tests");
    let questions = branch::questions("", &options());

    let strict = Decider::new(
        primary.clone(),
        Some(secondary.clone()),
        DeciderPolicy::default(),
    );
    assert!(strict.ask(CALL, &keys, &state, &questions).await.is_err());
    assert!(
        jev.seen.lock().is_empty(),
        "no fallback means the secondary is never asked"
    );

    let lenient = Decider::new(
        primary,
        Some(secondary),
        DeciderPolicy {
            fallback_on_error: true,
            compare: false,
        },
    );
    let d = lenient.ask(CALL, &keys, &state, &questions).await.unwrap();
    assert!(d.fell_back);
    assert_eq!(d.backend, "jev");
    assert_eq!(
        d.response.answers[QUESTION_ID].verdict(),
        "changes-requested"
    );
}

/// Wait, briefly, for something a detached task does.
async fn eventually(what: &str, done: impl Fn() -> bool) {
    for _ in 0..400 {
        if done() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    panic!("never happened: {what}");
}

#[tokio::test]
async fn compare_asks_both_and_acts_on_the_primary_without_waiting_for_the_other() {
    let kev = fake("approved", 0.9);
    let jev = slow("changes-requested", Duration::from_millis(1_200));
    let compare = |fallback_on_error| DeciderPolicy {
        fallback_on_error,
        compare: true,
    };
    let decider = Decider::new(
        client("kev", &kev, false).await,
        Some(client("jev", &jev, false).await),
        compare(false),
    );
    let questions = branch::questions("", &options());
    let started = Instant::now();
    let d = decider
        .ask(CALL, &no_keys, &json!({"x": 1}), &questions)
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_millis(600),
        "the answer waited for the comparison: {:?}",
        started.elapsed()
    );
    assert_eq!((d.backend.as_str(), d.fell_back), ("kev", false));
    assert_eq!(d.response.answers[QUESTION_ID].verdict(), "approved");
    eventually("the secondary is asked beside the primary", || {
        jev.seen.lock().len() == 1
    })
    .await;
    assert_eq!(kev.seen.lock().len(), 1);
    let (asked, compared) = (kev.seen.lock()[0].1.clone(), jev.seen.lock()[0].1.clone());
    assert_eq!(asked["state"], compared["state"]);
    assert_eq!(asked["questions"], compared["questions"]);

    *kev.behaviour.lock() = Err(500);
    let strict = decider
        .ask(CALL, &no_keys, &json!({"x": 1}), &questions)
        .await;
    assert!(strict.is_err(), "compare alone never acts on the secondary");
    let lenient = Decider::new(
        client("kev", &kev, false).await,
        Some(client("jev", &jev, false).await),
        compare(true),
    );
    let d = lenient
        .ask(CALL, &no_keys, &json!({"x": 1}), &questions)
        .await
        .unwrap();
    assert_eq!((d.backend.as_str(), d.fell_back), ("jev", true));
}

#[tokio::test]
async fn a_missing_answer_or_bad_status_is_an_error_not_a_guess() {
    let kev = fake("approved", 0.9);
    let c = client("kev", &kev, false).await;
    *kev.behaviour.lock() = Err(429);
    let err = c
        .ask(CALL, None, &json!({}), &branch::questions("", &options()))
        .await
        .unwrap_err();
    assert!(err.to_string().contains("429"), "{err}");

    // A server that answers, but not the question asked.
    *kev.behaviour.lock() = Ok(("__silent__".into(), 0.9));
    let mut questions: BTreeMap<String, Question> = BTreeMap::new();
    questions.insert("other".into(), Question::noul("Is it urgent?"));
    let err = c.ask(CALL, None, &json!({}), &questions).await.unwrap_err();
    assert!(err.to_string().contains("no answer under id"), "{err}");
}

fn lenient(compare: bool) -> DeciderPolicy {
    DeciderPolicy {
        fallback_on_error: true,
        compare,
    }
}

#[tokio::test]
async fn a_primary_the_caller_has_no_key_for_is_never_asked_and_the_fallback_answers() {
    let jev = fake("approved", 0.9);
    let kev = fake("changes-requested", 0.8);
    let decider = Decider::new(
        client("jev", &jev, true).await,
        Some(client("kev", &kev, false).await),
        lenient(false),
    );
    let d = decider
        .ask(
            CALL,
            &no_keys,
            &json!({}),
            &branch::questions("", &options()),
        )
        .await
        .unwrap();
    assert_eq!(d.backend, "kev");
    assert!(d.fell_back);
    assert!(jev.seen.lock().is_empty(), "no request without a key");
    assert_eq!(bearer(&kev, 0), None, "a keyless backend is asked bare");
}

#[tokio::test]
async fn compare_with_one_backend_unavailable_asks_only_the_other() {
    let kev = fake("approved", 0.9);
    let jev = fake("changes-requested", 0.8);
    let decider = Decider::new(
        client("kev", &kev, false).await,
        Some(client("jev", &jev, true).await),
        lenient(true),
    );
    let questions = branch::questions("", &options());
    let d = decider
        .ask(CALL, &no_keys, &json!({}), &questions)
        .await
        .unwrap();
    assert_eq!(d.backend, "kev");
    assert!(!d.fell_back);
    assert!(jev.seen.lock().is_empty());

    let jev_primary = Decider::new(
        client("jev", &jev, true).await,
        Some(client("kev", &kev, false).await),
        lenient(true),
    );
    let d = jev_primary
        .ask(CALL, &no_keys, &json!({}), &questions)
        .await
        .unwrap();
    assert_eq!(d.backend, "kev");
    assert!(d.fell_back);
    assert!(jev.seen.lock().is_empty());
    assert_eq!(kev.seen.lock().len(), 2);
}

#[tokio::test]
async fn with_no_backend_available_nothing_is_sent_and_the_error_says_so() {
    let jev = fake("approved", 0.9);
    let kev = fake("approved", 0.9);
    let decider = Decider::new(
        client("jev", &jev, true).await,
        Some(client("kev", &kev, true).await),
        lenient(true),
    );
    let blank = |_: &str| Some("  ".to_string());
    for keys in [&no_keys as &dyn workforce_decide::Keys, &blank] {
        let err = decider
            .ask(CALL, keys, &json!({}), &branch::questions("", &options()))
            .await
            .unwrap_err();
        assert!(err.is_unavailable(), "{err}");
        assert!(
            matches!(&err, DecideError::Unavailable { backends } if backends == &["jev", "kev"]),
            "{err}"
        );
    }
    assert!(jev.seen.lock().is_empty() && kev.seen.lock().is_empty());

    let strict = Decider::new(
        client("jev", &jev, true).await,
        Some(client("kev", &kev, false).await),
        DeciderPolicy::default(),
    );
    let err = strict
        .ask(
            CALL,
            &no_keys,
            &json!({}),
            &branch::questions("", &options()),
        )
        .await
        .unwrap_err();
    assert!(matches!(&err, DecideError::Unavailable { backends } if backends == &["jev"]));
    assert!(
        kev.seen.lock().is_empty(),
        "a backend that is not a fallback never steps in"
    );
}

#[tokio::test]
async fn every_call_sends_the_key_its_caller_resolves_now() {
    let jev = fake("approved", 0.9);
    let decider = Decider::new(client("jev", &jev, true).await, None, lenient(false));
    let questions = branch::questions("", &options());
    let current = Mutex::new("first-key".to_string());
    let keys = |_: &str| Some(current.lock().clone());
    decider
        .ask(CALL, &keys, &json!({}), &questions)
        .await
        .unwrap();
    *current.lock() = "rotated-key".to_string();
    decider
        .ask(CALL, &keys, &json!({}), &questions)
        .await
        .unwrap();
    let alice = |_: &str| Some("alice-key".to_string());
    decider
        .ask(CALL, &alice, &json!({}), &questions)
        .await
        .unwrap();
    assert_eq!(bearer(&jev, 0).as_deref(), Some("Bearer first-key"));
    assert_eq!(bearer(&jev, 1).as_deref(), Some("Bearer rotated-key"));
    assert_eq!(bearer(&jev, 2).as_deref(), Some("Bearer alice-key"));
}

/// `calls` asks at once; whether each was answered.
async fn burst(client: &Arc<SystemOneClient>, calls: usize, call: Call) -> Vec<bool> {
    let questions = branch::questions("", &options());
    let asks = (0..calls).map(|_| {
        let (client, questions) = (Arc::clone(client), questions.clone());
        tokio::spawn(async move { client.ask(call, None, &json!({}), &questions).await.is_ok() })
    });
    let mut answered = Vec::new();
    for ask in asks.collect::<Vec<_>>() {
        answered.push(ask.await.unwrap());
    }
    answered
}

#[tokio::test]
async fn a_backend_never_has_more_requests_in_flight_than_its_cap() {
    let kev = slow("approved", Duration::from_millis(150));
    let c = capped("kev", &kev, false, 3).await;
    let answered = burst(&c, 12, CALL).await;
    assert!(answered.iter().all(|ok| *ok), "everyone is served in turn");
    assert_eq!(kev.seen.lock().len(), 12);
    assert_eq!(kev.peak.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn a_call_that_cannot_get_a_slot_in_its_budget_is_busy_and_sends_nothing() {
    let kev = slow("approved", Duration::from_millis(1_500));
    let c = capped("kev", &kev, false, 1).await;
    let questions = branch::questions("", &options());
    let holder = {
        let (c, questions) = (Arc::clone(&c), questions.clone());
        tokio::spawn(async move { c.ask(CALL, None, &json!({}), &questions).await })
    };
    while kev.in_flight.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    let started = Instant::now();
    let err = c
        .ask(within(100), None, &json!({}), &questions)
        .await
        .unwrap_err();
    let waited = started.elapsed();
    assert!(
        matches!(&err, DecideError::Busy { backend, .. } if backend == "kev"),
        "{err}"
    );
    assert!(err.is_overloaded() && !err.is_unavailable());
    assert!(
        waited >= Duration::from_millis(100) && waited < Duration::from_secs(1),
        "{waited:?}"
    );
    assert_eq!(kev.seen.lock().len(), 1, "a queued call sends no request");
    assert!(
        holder.await.unwrap().is_ok(),
        "the call in flight is unharmed"
    );
}

#[tokio::test]
async fn a_request_that_outlasts_its_budget_times_out_and_holds_its_slot_until_the_backend_is_done()
{
    let kev = slow("approved", Duration::from_millis(700));
    let c = capped("kev", &kev, false, 1).await;
    let questions = branch::questions("", &options());

    let started = Instant::now();
    let err = c
        .ask(within(100), None, &json!({}), &questions)
        .await
        .unwrap_err();
    let waited = started.elapsed();
    assert!(
        matches!(&err, DecideError::Timeout { backend, budget_ms: 100 } if backend == "kev"),
        "{err}"
    );
    assert!(err.is_overloaded());
    assert!(waited < Duration::from_millis(600), "{waited:?}");

    let err = c
        .ask(within(100), None, &json!({}), &questions)
        .await
        .unwrap_err();
    assert!(
        matches!(err, DecideError::Busy { .. }),
        "the backend is still working on the abandoned request: {err}"
    );
    assert_eq!(kev.seen.lock().len(), 1, "nothing is piled onto it");

    c.ask(within(3_000), None, &json!({}), &questions)
        .await
        .expect("the slot comes back when the backend has answered");
    assert_eq!(kev.peak.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn a_cancelled_call_leaks_no_slot_whether_it_was_queued_or_in_flight() {
    let kev = slow("approved", Duration::from_millis(300));
    let c = capped("kev", &kev, false, 2).await;
    let questions = branch::questions("", &options());
    let asks: Vec<_> = (0..6)
        .map(|_| {
            let (c, questions) = (Arc::clone(&c), questions.clone());
            tokio::spawn(async move { c.ask(CALL, None, &json!({}), &questions).await })
        })
        .collect();
    while kev.in_flight.load(Ordering::SeqCst) < 2 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    asks.iter().for_each(|ask| ask.abort());
    for ask in asks {
        assert!(ask.await.unwrap_err().is_cancelled());
    }
    assert_eq!(
        kev.seen.lock().len(),
        2,
        "a cancelled queued call sends nothing"
    );

    let answered = burst(&c, 6, CALL).await;
    assert!(answered.iter().all(|ok| *ok), "every slot came back");
    assert_eq!(kev.seen.lock().len(), 8);
    assert_eq!(kev.peak.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn slots_are_handed_out_in_arrival_order() {
    let kev = slow("approved", Duration::from_millis(60));
    let c = capped("kev", &kev, false, 1).await;
    let mut asks = Vec::new();
    for i in 0..5 {
        let c = Arc::clone(&c);
        let questions = BTreeMap::from([(format!("q{i}"), Question::noul("Urgent?"))]);
        asks.push(tokio::spawn(async move {
            c.ask(CALL, None, &json!({}), &questions).await
        }));
        tokio::time::sleep(Duration::from_millis(15)).await;
    }
    for ask in asks {
        let _ = ask.await.unwrap();
    }
    let order: Vec<String> = kev
        .seen
        .lock()
        .iter()
        .flat_map(|(_, body)| {
            body["questions"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect::<Vec<_>>()
        })
        .collect();
    assert_eq!(order, ["q0", "q1", "q2", "q3", "q4"]);
}

#[tokio::test]
async fn a_backend_timeout_shorter_than_the_budget_is_a_timeout_not_a_fault() {
    let kev = slow("approved", Duration::from_millis(1_500));
    let c = SystemOneClient::new(Backend {
        name: "kev".into(),
        url: spawn(kev.clone()).await,
        requires_key: false,
        model: "kev-latest".into(),
        timeout: Duration::from_millis(200),
        max_concurrent: 1,
    })
    .unwrap();
    let err = c
        .ask(CALL, None, &json!({}), &branch::questions("", &options()))
        .await
        .unwrap_err();
    assert!(
        matches!(err, DecideError::Timeout { budget_ms: 200, .. }),
        "{err}"
    );
    assert!(err.is_overloaded());
}

#[tokio::test]
async fn a_caller_without_a_key_is_unavailable_at_once_even_when_the_backend_is_full() {
    let jev = slow("approved", Duration::from_millis(400));
    let c = capped("jev", &jev, true, 1).await;
    let questions = branch::questions("", &options());
    let holder = {
        let (c, questions) = (Arc::clone(&c), questions.clone());
        tokio::spawn(async move { c.ask(CALL, Some("k"), &json!({}), &questions).await })
    };
    while jev.in_flight.load(Ordering::SeqCst) == 0 {
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
    let started = Instant::now();
    let err = c.ask(CALL, None, &json!({}), &questions).await.unwrap_err();
    assert!(err.is_unavailable() && !err.is_overloaded());
    assert!(started.elapsed() < Duration::from_millis(300));
    holder.await.unwrap().unwrap();
}

#[tokio::test]
async fn an_overloaded_primary_hands_over_to_the_fallback_within_two_budgets() {
    let kev = slow("approved", Duration::from_secs(3));
    let jev = fake("changes-requested", 0.8);
    let decider = Decider::new(
        capped("kev", &kev, false, 1).await,
        Some(client("jev", &jev, false).await),
        lenient(false),
    );
    let questions = branch::questions("", &options());
    let started = Instant::now();
    let d = decider
        .ask(within(150), &no_keys, &json!({}), &questions)
        .await
        .unwrap();
    assert!(
        started.elapsed() < Duration::from_secs(1),
        "{:?}",
        started.elapsed()
    );
    assert_eq!((d.backend.as_str(), d.fell_back), ("jev", true));
    assert_eq!(
        d.response.answers[QUESTION_ID].verdict(),
        "changes-requested"
    );
}

#[tokio::test]
async fn with_every_backend_overloaded_the_call_fails_as_overloaded_within_its_bound() {
    let kev = slow("approved", Duration::from_secs(3));
    let jev = slow("approved", Duration::from_secs(3));
    let alone = Decider::new(client("kev", &kev, false).await, None, lenient(false));
    let both = Decider::new(
        client("kev", &kev, false).await,
        Some(client("jev", &jev, false).await),
        lenient(false),
    );
    let questions = branch::questions("", &options());
    for (decider, bound_ms) in [(&alone, 1_000), (&both, 1_500)] {
        let started = Instant::now();
        let err = decider
            .ask(within(150), &no_keys, &json!({}), &questions)
            .await
            .unwrap_err();
        assert!(err.is_overloaded(), "never an answer: {err}");
        assert!(
            started.elapsed() < Duration::from_millis(bound_ms),
            "{:?}",
            started.elapsed()
        );
    }

    *jev.delay.lock() = Duration::ZERO;
    *jev.behaviour.lock() = Err(500);
    let err = both
        .ask(within(150), &no_keys, &json!({}), &questions)
        .await
        .unwrap_err();
    assert!(
        !err.is_overloaded(),
        "a broken fallback is a fault, not load: {err}"
    );
}

#[tokio::test]
async fn several_question_sets_about_one_state_are_one_request() {
    let kev = fake("approved", 0.9);
    let c = client("kev", &kev, false).await;
    let state = request::state("Why did the nightly import fail?", StateBudget::DEFAULT).unwrap();
    let questions = batch([scope::questions("data imports"), routing::questions()]).unwrap();
    let response = c.ask(CALL, None, &state, &questions).await.unwrap();

    assert_eq!(kev.seen.lock().len(), 1);
    let (_, body) = kev.seen.lock()[0].clone();
    assert_eq!(body["state"]["message"], "Why did the nightly import fail?");
    assert_eq!(
        body["questions"]
            .as_object()
            .unwrap()
            .keys()
            .collect::<Vec<_>>(),
        [scope::QUESTION_ID, routing::QUESTION_ID]
    );
    assert!(response.answers.contains_key(scope::QUESTION_ID));
    assert!(response.answers.contains_key(routing::QUESTION_ID));
}

/// Counters as `name{label=value,…}` to their counts (a histogram counts
/// as 1 for being recorded), and every label any metric carried.
fn recorded(
    snapshotter: &metrics_util::debugging::Snapshotter,
) -> (BTreeMap<String, u64>, Vec<(String, String)>) {
    use metrics_util::debugging::DebugValue;
    let mut counters = BTreeMap::new();
    let mut labels = Vec::new();
    for (key, _, _, value) in snapshotter.snapshot().into_vec() {
        let key = key.key();
        let mut own: Vec<(String, String)> = key
            .labels()
            .map(|l| (l.key().to_string(), l.value().to_string()))
            .collect();
        own.sort();
        let described = own
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",");
        labels.extend(own);
        match value {
            DebugValue::Counter(count) => {
                counters.insert(format!("{}{{{described}}}", key.name()), count);
            }
            DebugValue::Histogram(samples) => {
                counters.insert(
                    format!("{}{{{described}}}", key.name()),
                    u64::from(!samples.is_empty()),
                );
            }
            DebugValue::Gauge(_) => {}
        }
    }
    (counters, labels)
}

#[test]
fn every_attempt_is_counted_by_use_backend_and_result_and_carries_nothing_else() {
    let recorder = metrics_util::debugging::DebuggingRecorder::new();
    let snapshotter = recorder.snapshotter();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    metrics::with_local_recorder(&recorder, || {
        runtime.block_on(async {
            let kev = slow("approved", Duration::from_millis(300));
            let jev = fake("approved", 0.9);
            let primary = capped("kev", &kev, false, 1).await;
            let state = json!({"message": "STATE-TEXT from alice@example.com"});
            let questions = branch::questions("TASK-TEXT approved", &options());
            let ask = |call| primary.ask(call, Some("sk-secret-key"), &state, &questions);

            assert!(matches!(
                ask(within(80)).await,
                Err(DecideError::Timeout { .. })
            ));
            assert!(matches!(
                ask(within(40)).await,
                Err(DecideError::Busy { .. })
            ));
            ask(within(3_000)).await.unwrap();
            *kev.behaviour.lock() = Err(500);
            *kev.delay.lock() = Duration::ZERO;
            let decider = Decider::new(
                Arc::clone(&primary),
                Some(client("jev", &jev, false).await),
                lenient(false),
            );
            let keys = |_: &str| Some("sk-secret-key".to_string());
            let d = decider
                .ask(
                    Call::new(Use::Tool, Duration::from_secs(3)),
                    &keys,
                    &state,
                    &questions,
                )
                .await
                .unwrap();
            assert!(d.fell_back);
        })
    });

    let (counters, labels) = recorded(&snapshotter);
    assert_eq!(
        counters,
        BTreeMap::from(
            [
                (
                    "workforce.decision.calls{backend=kev,result=timeout,use=scope_gate}",
                    1
                ),
                (
                    "workforce.decision.calls{backend=kev,result=busy,use=scope_gate}",
                    1
                ),
                (
                    "workforce.decision.calls{backend=kev,result=ok,use=scope_gate}",
                    1
                ),
                (
                    "workforce.decision.calls{backend=kev,result=error,use=tool}",
                    1
                ),
                (
                    "workforce.decision.calls{backend=jev,result=ok,use=tool}",
                    1
                ),
                (
                    "workforce.decision.fallbacks{backend=jev,reason=error,use=tool}",
                    1
                ),
                ("workforce.decision.queue_ms{backend=kev,use=scope_gate}", 1),
                ("workforce.decision.queue_ms{backend=kev,use=tool}", 1),
                ("workforce.decision.queue_ms{backend=jev,use=tool}", 1),
                (
                    "workforce.decision.latency_ms{backend=kev,use=scope_gate}",
                    1
                ),
                ("workforce.decision.latency_ms{backend=kev,use=tool}", 1),
                ("workforce.decision.latency_ms{backend=jev,use=tool}", 1),
            ]
            .map(|(metric, count)| (metric.to_string(), count))
        )
    );
    for (key, value) in labels {
        assert!(
            ["use", "backend", "result", "reason"].contains(&key.as_str()),
            "{key}"
        );
        assert!(
            ["STATE", "alice", "TASK", "sk-secret", "approved"]
                .iter()
                .all(|private| !value.contains(private)),
            "{key}={value}"
        );
    }
}
