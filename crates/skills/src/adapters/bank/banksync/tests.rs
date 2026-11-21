//! Against a BankSync served from the shapes in its published openapi.json.

use serde_json::json;
use wiremock::matchers::{header, method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use super::*;
use crate::adapters::test_support::InMem;

const KEY: &str = "bsk_test0000000000000000000000000000";

fn whoami(scopes: &[&str]) -> Value {
    json!({"success": true, "data": {
        "workspaceId": "ws_1", "workspaceName": "Home", "authMethod": "apiKey",
        "scopes": scopes, "planTier": "starter", "appId": null, "installId": null
    }})
}

fn bank(id: &str, name: &str, connected: bool) -> Value {
    json!({
        "id": id, "name": name, "source": "saltedge", "type": "bank", "order": 0,
        "connectionStatus": {"connected": connected, "status": if connected {"active"} else {"consent_expired"}},
        "historyReadiness": {"state": "ready"},
        "institutionId": "inst", "createdAt": "2026-09-01T00:00:00Z",
        "updatedAt": "2026-09-01T00:00:00Z", "links": {}
    })
}

fn banks() -> Value {
    json!({"success": true, "data": [bank("bnk_rev", "Revolut", true), bank("bnk_nw", "NatWest", true)]})
}

fn accounts() -> Value {
    json!({"success": true, "data": [{
        "id": "acc_1", "bankId": "bnk_nw", "accountNumber": "60-00-01 12345678",
        "accountName": "Current", "accountType": "checking", "balance": 1234.56,
        "availableBalance": 1200.5, "currency": "GBP", "createdAt": "x", "updatedAt": "x", "links": {}
    }]})
}

fn transactions(ids: &[&str], cursor: Option<&str>) -> Value {
    json!({"success": true, "data": ids.iter().map(|id| json!({
        "id": id, "date": "2026-11-02", "description": format!("CARD {id}"),
        "merchantName": "Tesco", "amount": -12.3, "creditAmount": 0, "debitAmount": 12.3,
        "currency": "GBP", "category": "groceries", "type": "card", "pending": false,
        "cardLast4": "9876", "cardholder": "A PERSON", "accountId": "acc_1", "bankId": "bnk_nw"
    })).collect::<Vec<_>>(),
    "meta": {"count": ids.len(), "cursor": cursor, "hasMore": cursor.is_some(),
             "from": "2026-10-22", "to": "2026-11-21"},
    "modified": [], "removed": [], "links": {}})
}

async fn serve(scopes: &[&str]) -> MockServer {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/whoami"))
        .and(header("X-API-Key", KEY))
        .respond_with(ResponseTemplate::new(200).set_body_json(whoami(scopes)))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/banks"))
        .respond_with(ResponseTemplate::new(200).set_body_json(banks()))
        .mount(&server)
        .await;
    server
}

fn natwest(server: &MockServer) -> BankSync {
    BankSync::fixed(KEY, "natwest", &server.uri(), Arc::default())
}

fn window() -> Window {
    Window::of(
        None,
        Some("2026-11-21".parse().unwrap()),
        Utc::now().date_naive(),
    )
    .unwrap()
}

#[test]
fn only_a_key_that_reads_banks_and_nothing_else_reads_only() {
    let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert!(read_only(&s(&["banks:read"])));
    assert!(read_only(&s(&["banks:read", "institutions:read"])));
    assert!(!read_only(&s(&["banks:read", "banks:write"])));
    assert!(!read_only(&s(&["banks:read", "feeds:write"])));
    assert!(!read_only(&s(&["institutions:read"])));
    assert!(!read_only(&s(&[])));
}

#[tokio::test]
async fn every_read_is_a_get_and_the_key_is_checked_once() {
    let server = serve(&["banks:read", "institutions:read"]).await;
    Mock::given(method("GET"))
        .and(path("/banks/bnk_nw/accounts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(accounts()))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/banks/bnk_nw"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"success": true, "data": bank("bnk_nw", "NatWest", true)})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/banks/bnk_nw/accounts/acc_1/balances"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({"success": true, "data": {
                "date": "2026-11-21", "retrievedAt": "2026-11-21T08:00:00Z", "balanceAsOf": null,
                "bank": "NatWest", "account": "Current", "accountName": "Current",
                "accountNumber": "12345678", "accountType": "checking", "accountId": "acc_1",
                "bankId": "bnk_nw", "amount": 1234.56, "availableBalance": 1200.5, "currency": "GBP"
            }})),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/banks/bnk_nw/accounts/acc_1/transactions"))
        .and(query_param("from", "2026-10-22"))
        .and(query_param("to", "2026-11-21"))
        .respond_with(ResponseTemplate::new(200).set_body_json(transactions(&["t1"], None)))
        .mount(&server)
        .await;

    let bs = natwest(&server);
    let connection = bs.connection().await.unwrap();
    assert_eq!(connection.bank, "NatWest");
    assert!(connection.connected);
    let accounts = bs.accounts().await.unwrap();
    assert_eq!(accounts[0].number, "•••• 5678");
    assert_eq!(
        accounts[0].balance,
        Some(Number::from_f64(1234.56).unwrap())
    );
    let balance = bs.balance("acc_1").await.unwrap();
    assert_eq!(balance.amount.to_string(), "1234.56");
    let page = bs.transactions("acc_1", window(), None).await.unwrap();
    assert_eq!(page.transactions[0].card.as_deref(), Some("•••• 9876"));
    assert!(!page.has_more);

    let sent = server.received_requests().await.unwrap();
    assert!(sent.iter().all(|r| r.method == wiremock::http::Method::GET));
    assert_eq!(sent.iter().filter(|r| r.url.path() == "/whoami").count(), 1);
    assert_eq!(sent.iter().filter(|r| r.url.path() == "/banks").count(), 1);
}

#[tokio::test]
async fn a_key_that_can_write_is_never_used() {
    let server = serve(&["banks:read", "banks:write"]).await;
    let bs = natwest(&server);
    assert_eq!(bs.accounts().await, Err(BankError::NotReadOnly));
    let sent = server.received_requests().await.unwrap();
    assert!(sent.iter().all(|r| r.url.path() == "/whoami"));
}

#[tokio::test]
async fn a_key_without_banks_read_is_told_so() {
    let server = serve(&["institutions:read"]).await;
    assert_eq!(
        natwest(&server).accounts().await,
        Err(BankError::ScopeMissing)
    );
}

#[tokio::test]
async fn a_bank_is_named_by_its_id_or_its_name_alone() {
    let all = banks();
    assert_eq!(bank_named(&all, "bnk_rev").unwrap(), "bnk_rev");
    assert_eq!(bank_named(&all, "REVOLUT").unwrap(), "bnk_rev");
    assert!(matches!(
        bank_named(&all, "Monzo"),
        Err(BankError::Invalid(_))
    ));
    let twins = json!({"data": [bank("a", "NatWest", true), bank("b", "NatWest Business", true)]});
    assert_eq!(
        bank_named(&twins, "natwest").unwrap(),
        "a",
        "its own name first"
    );
    assert_eq!(bank_named(&twins, "natwest business").unwrap(), "b");
    assert!(matches!(
        bank_named(&twins, "west"),
        Err(BankError::Invalid(_))
    ));
}

#[tokio::test]
async fn what_banksync_refuses_is_said_as_what_to_do() {
    for (status, expected) in [
        (503, BankError::NeedsReconnect),
        (401, BankError::Revoked),
        (403, BankError::ScopeMissing),
        (404, BankError::NotFound("bank or account")),
        (502, BankError::Unavailable(Some(502))),
    ] {
        let server = serve(&["banks:read"]).await;
        Mock::given(method("GET"))
            .and(path("/banks/bnk_nw/accounts"))
            .respond_with(ResponseTemplate::new(status).set_body_string("body with £12.30 in it"))
            .mount(&server)
            .await;
        let got = natwest(&server).accounts().await.unwrap_err();
        assert_eq!(got, expected);
        assert!(!got.to_string().contains("12.30"));
    }
}

#[tokio::test]
async fn a_short_rate_limit_is_waited_out_once_and_a_long_one_is_said() {
    let server = serve(&["banks:read"]).await;
    Mock::given(method("GET"))
        .and(path("/banks/bnk_nw/accounts"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .up_to_n_times(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/banks/bnk_nw/accounts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(accounts()))
        .mount(&server)
        .await;
    assert_eq!(natwest(&server).accounts().await.unwrap().len(), 1);

    let server = serve(&["banks:read"]).await;
    Mock::given(method("GET"))
        .and(path("/banks/bnk_nw/accounts"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "60"))
        .mount(&server)
        .await;
    assert_eq!(
        natwest(&server).accounts().await,
        Err(BankError::RateLimited { retry_after: 60 })
    );
}

#[tokio::test(start_paused = true)]
async fn the_limiter_keeps_each_person_to_ten_a_minute() {
    let limiter = Limiter::default();
    for _ in 0..PER_MINUTE {
        limiter.take("ann").await.unwrap();
    }
    assert_eq!(
        limiter.take("ann").await,
        Err(BankError::RateLimited { retry_after: 60 })
    );
    limiter.take("bob").await.unwrap();
    tokio::time::advance(Duration::from_secs(55)).await;
    let before = Instant::now();
    limiter.take("ann").await.unwrap();
    assert_eq!(Instant::now() - before, Duration::from_secs(5));
}

#[tokio::test]
async fn a_stored_connection_reads_its_key_bank_and_approval_as_the_caller() {
    let server = serve(&["banks:read"]).await;
    Mock::given(method("GET"))
        .and(path("/banks/bnk_rev/accounts"))
        .respond_with(ResponseTemplate::new(200).set_body_json(accounts()))
        .mount(&server)
        .await;
    let store = InMem::from(&[
        ("skills/bank/workspaces/revolut/api_key", KEY),
        ("skills/bank/workspaces/revolut/bank", "Revolut"),
        (
            "skills/bank/workspaces/revolut/consented_at",
            "2026-09-01T10:00:00Z",
        ),
    ]);
    let bs = BankSync::with_store(store, "revolut", &server.uri(), Arc::default());
    assert_eq!(bs.accounts().await.unwrap().len(), 1);
    assert_eq!(
        bs.consented_at(),
        Some("2026-09-01T10:00:00Z".parse().unwrap())
    );

    let empty = BankSync::with_store(InMem::from(&[]), "revolut", &server.uri(), Arc::default());
    assert_eq!(empty.accounts().await, Err(BankError::NotConfigured("key")));
    assert_eq!(empty.consented_at(), None);
}

/// A new key may be of another BankSync workspace: it is checked again, and
/// the bank found under the old one is looked up again.
#[tokio::test]
async fn a_new_key_is_checked_again_and_its_bank_looked_up_again() {
    const OTHER: &str = "bsk_other000000000000000000000000000";
    let server = serve(&["banks:read"]).await;
    Mock::given(method("GET"))
        .and(path("/whoami"))
        .and(header("X-API-Key", OTHER))
        .respond_with(ResponseTemplate::new(200).set_body_json(whoami(&["banks:read"])))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/banks"))
        .and(header("X-API-Key", OTHER))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(
                json!({"success": true, "data": [bank("bnk_rev2", "Revolut", true)]}),
            ),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    for id in ["bnk_rev", "bnk_rev2"] {
        Mock::given(method("GET"))
            .and(path(format!("/banks/{id}/accounts")))
            .respond_with(ResponseTemplate::new(200).set_body_json(accounts()))
            .mount(&server)
            .await;
    }
    let store = InMem::from(&[
        ("skills/bank/workspaces/revolut/api_key", KEY),
        ("skills/bank/workspaces/revolut/bank", "Revolut"),
    ]);
    let bs = BankSync::with_store(store.clone(), "revolut", &server.uri(), Arc::default());
    bs.accounts().await.unwrap();
    store
        .set("skills/bank/workspaces/revolut/api_key", OTHER)
        .unwrap();
    bs.accounts().await.unwrap();
    let asked: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| r.url.path().to_string())
        .collect();
    assert_eq!(
        asked,
        [
            "/whoami",
            "/banks",
            "/banks/bnk_rev/accounts",
            "/whoami",
            "/banks",
            "/banks/bnk_rev2/accounts"
        ]
    );
}

/// Every connection and agent of a person's on one BankSync workspace
/// counts against its one limit.
#[tokio::test]
async fn a_person_s_connections_on_one_workspace_share_its_limit() {
    let server = serve(&["banks:read"]).await;
    for id in ["bnk_nw", "bnk_rev"] {
        Mock::given(method("GET"))
            .and(path(format!("/banks/{id}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"success": true, "data": bank(id, "Bank", true)})),
            )
            .mount(&server)
            .await;
    }
    let limiter = Arc::new(Limiter::default());
    let natwest = BankSync::fixed(KEY, "natwest", &server.uri(), Arc::clone(&limiter));
    let revolut = BankSync::fixed(KEY, "revolut", &server.uri(), Arc::clone(&limiter));
    workforce_agent::runner::CURRENT_USER_ID
        .scope("ann".into(), async {
            natwest.connection().await.unwrap();
            revolut.connection().await.unwrap();
        })
        .await;
    let counts = limiter.0.lock().unwrap();
    assert_eq!(counts["ann\u{0}ws_1"].len(), 4, "two banks, two reads each");
    assert!(Arc::ptr_eq(&Limiter::shared(), &Limiter::shared()));
}

#[tokio::test]
async fn an_account_id_never_leaves_its_path_segment() {
    let server = serve(&["banks:read"]).await;
    let got = natwest(&server).balance("../../whoami").await;
    assert!(matches!(got, Err(BankError::Invalid(_))));
}

#[test]
fn a_page_reads_its_cursor_and_drops_rows_without_an_amount() {
    let mut body = transactions(&["t1", "t2"], Some("c2"));
    body["data"][1]["amount"] = Value::Null;
    let page = page_of(&body);
    assert_eq!(page.transactions.len(), 1);
    assert_eq!(page.cursor.as_deref(), Some("c2"));
    assert!(page.has_more);
}
