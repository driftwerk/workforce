use std::collections::HashMap;
use std::sync::Mutex;

use serde_json::Number;

use super::*;
use crate::adapters::bank::model::{Account, Balance, Connection, Page};

/// A bank of `pages` of transactions, rate limited after `budget` reads.
struct Fake {
    accounts: Vec<&'static str>,
    pages: Vec<Vec<&'static str>>,
    budget: Mutex<usize>,
    asked: Mutex<Vec<Option<String>>>,
}

impl Fake {
    fn new(accounts: &[&'static str], pages: &[&[&'static str]]) -> Self {
        Self {
            accounts: accounts.to_vec(),
            pages: pages.iter().map(|p| p.to_vec()).collect(),
            budget: Mutex::new(usize::MAX),
            asked: Mutex::new(Vec::new()),
        }
    }

    fn spend(&self) -> Result<(), BankError> {
        let mut left = self.budget.lock().unwrap();
        match *left {
            0 => Err(BankError::RateLimited { retry_after: 30 }),
            _ => {
                *left -= 1;
                Ok(())
            }
        }
    }
}

fn tx(id: &str) -> Transaction {
    Transaction {
        id: id.into(),
        date: "2026-11-01".into(),
        description: format!("PAYMENT {id}"),
        merchant: None,
        amount: Number::from(-1),
        currency: "GBP".into(),
        category: None,
        kind: None,
        reference: None,
        pending: false,
        card: None,
    }
}

#[async_trait]
impl BankProvider for Fake {
    async fn connection(&self) -> Result<Connection, BankError> {
        Ok(Connection {
            bank: "NatWest".into(),
            connected: true,
            status: None,
            history: None,
        })
    }
    async fn accounts(&self) -> Result<Vec<Account>, BankError> {
        Ok(self
            .accounts
            .iter()
            .map(|id| Account {
                id: id.to_string(),
                name: Some("Current".into()),
                kind: "checking".into(),
                number: "•••• 1234".into(),
                currency: "GBP".into(),
                balance: None,
                available: None,
            })
            .collect())
    }
    async fn balance(&self, account: &str) -> Result<Balance, BankError> {
        self.spend()?;
        Ok(Balance {
            account: account.into(),
            name: None,
            currency: "GBP".into(),
            amount: Number::from(10),
            available: None,
            pending: None,
            credit_limit: None,
            as_of: None,
            retrieved_at: None,
        })
    }
    async fn transactions(
        &self,
        _account: &str,
        _window: Window,
        cursor: Option<&str>,
    ) -> Result<Page, BankError> {
        self.spend()?;
        self.asked.lock().unwrap().push(cursor.map(str::to_string));
        let at: usize = cursor.map_or(0, |c| c.parse().unwrap());
        let more = at + 1 < self.pages.len();
        Ok(Page {
            transactions: self.pages[at].iter().map(|id| tx(id)).collect(),
            cursor: more.then(|| (at + 1).to_string()),
            has_more: more,
            partial: false,
        })
    }
    fn consented_at(&self) -> Option<chrono::DateTime<Utc>> {
        Some(Utc::now() - chrono::Duration::days(83))
    }
}

fn ctx(fakes: Vec<(&str, Fake)>, default: &str) -> Ctx<Fake> {
    let clients: HashMap<String, Arc<Fake>> = fakes
        .into_iter()
        .map(|(n, f)| (n.to_string(), Arc::new(f)))
        .collect();
    Ctx::new(
        Arc::new(LiveInstances::fixed_map(clients, default)),
        default.into(),
    )
}

fn resume() -> Resume {
    Resume {
        account: "acc".into(),
        from: "2026-10-01".parse().unwrap(),
        to: "2026-11-01".parse().unwrap(),
        page: None,
        offset: 0,
    }
}

fn ids(l: &Listing) -> Vec<&str> {
    l.transactions.iter().map(|t| t.id.as_str()).collect()
}

/// The fenced answer's data, unfenced for the test.
fn data(out: &Value) -> Value {
    let fenced = out["data"].as_str().unwrap();
    serde_json::from_str(fenced.lines().nth(2).unwrap()).unwrap()
}

#[tokio::test]
async fn a_listing_goes_on_inside_a_page_it_stopped_in() {
    let fake = Fake::new(&["acc"], &[&["a", "b", "c"], &["d"]]);
    let first = list(&fake, resume(), None, 2).await.unwrap();
    assert_eq!(ids(&first), ["a", "b"]);
    let next = Resume::decode(first.next_cursor.as_deref().unwrap()).unwrap();
    assert_eq!((next.page.as_deref(), next.offset), (None, 2));
    let second = list(&fake, next, None, 2).await.unwrap();
    assert_eq!(ids(&second), ["c"]);
    let third = list(
        &fake,
        Resume::decode(&second.next_cursor.unwrap()).unwrap(),
        None,
        2,
    )
    .await
    .unwrap();
    assert_eq!(ids(&third), ["d"]);
    assert_eq!(third.next_cursor, None);
}

#[tokio::test]
async fn a_search_reads_at_most_four_pages() {
    let fake = Fake::new(
        &["acc"],
        &[&["x1"], &["x2"], &["x3"], &["hit"], &["x5"], &["hit2"]],
    );
    let got = list(&fake, resume(), Some("hit"), 50).await.unwrap();
    assert_eq!(ids(&got), ["hit"]);
    assert_eq!(fake.asked.lock().unwrap().len(), SEARCH_PAGES);
    let next = Resume::decode(&got.next_cursor.unwrap()).unwrap();
    assert_eq!(next.page.as_deref(), Some("4"));
}

#[tokio::test]
async fn a_search_the_rate_limit_stops_says_what_it_found_and_goes_on_from_there() {
    let fake = Fake::new(&["acc"], &[&["hit1"], &["hit2"], &["hit3"]]);
    *fake.budget.lock().unwrap() = 2;
    let got = list(&fake, resume(), Some("hit"), 50).await.unwrap();
    assert_eq!(ids(&got), ["hit1", "hit2"]);
    assert!(got.partial);
    let next = Resume::decode(&got.next_cursor.unwrap()).unwrap();
    assert_eq!(next.page.as_deref(), Some("2"));
}

#[tokio::test]
async fn a_first_read_the_rate_limit_stops_is_an_error() {
    let fake = Fake::new(&["acc"], &[&["a"]]);
    *fake.budget.lock().unwrap() = 0;
    assert!(matches!(
        list(&fake, resume(), None, 5).await,
        Err(BankError::RateLimited { .. })
    ));
}

#[tokio::test]
async fn the_account_is_implied_only_when_there_is_one() {
    let tool = BankTransactionsTool(ctx(
        vec![("natwest", Fake::new(&["acc"], &[&["a"]]))],
        "natwest",
    ));
    let out = tool.execute(json!({})).await.unwrap();
    assert!(out.success, "{:?}", out.error);
    assert_eq!(out.output["connection"], "natwest");
    assert_eq!(out.output["account"], "acc");
    assert_eq!(data(&out.output)[0]["id"], "a");

    let tool = BankTransactionsTool(ctx(
        vec![("natwest", Fake::new(&["acc1", "acc2"], &[&["a"]]))],
        "natwest",
    ));
    let out = tool.execute(json!({})).await.unwrap();
    assert!(!out.success);
    let said = out.error.unwrap();
    assert!(said.contains("2 accounts"), "{said}");
    assert!(
        !["acc1", "Current", "1234"].iter().any(|x| said.contains(x)),
        "nothing of the bank's outside the fence: {said}"
    );
}

#[tokio::test]
async fn a_cursor_or_date_that_is_not_one_is_refused() {
    let tool = BankTransactionsTool(ctx(
        vec![("natwest", Fake::new(&["acc"], &[&["a"]]))],
        "natwest",
    ));
    for input in [
        json!({"cursor": "not-a-cursor"}),
        json!({"from": "1st November"}),
        json!({"from": "2026-11-02", "to": "2026-11-01"}),
    ] {
        assert!(!tool.execute(input).await.unwrap().success);
    }
}

#[tokio::test]
async fn several_connections_are_picked_by_name_and_an_unknown_one_lists_them() {
    let ctx = ctx(
        vec![
            ("natwest", Fake::new(&["nw"], &[&["a"]])),
            ("revolut", Fake::new(&["rv"], &[&["b"]])),
        ],
        "natwest",
    );
    let tool = BankAccountsTool(ctx.clone());
    let out = tool.execute(json!({"workspace": "revolut"})).await.unwrap();
    assert_eq!(out.output["connection"], "revolut");
    assert_eq!(data(&out.output)["accounts"][0]["id"], "rv");
    let out = tool.execute(json!({"workspace": "monzo"})).await.unwrap();
    assert!(!out.success);
    assert!(out.error.unwrap().contains("revolut"));

    let names = BankWorkspacesTool(ctx).execute(json!({})).await.unwrap();
    assert_eq!(
        names.output,
        json!({"workspaces": ["natwest", "revolut"], "default": "natwest"})
    );
}

#[tokio::test]
async fn accounts_say_when_the_approval_lapses_and_to_remind_a_week_before() {
    let tool = BankAccountsTool(ctx(
        vec![("natwest", Fake::new(&["acc"], &[&["a"]]))],
        "natwest",
    ));
    let out = tool.execute(json!({})).await.unwrap();
    let approval = &data(&out.output)["approval"];
    assert_eq!(approval["days_left"], 7);
    assert_eq!(approval["remind"], true);
    assert_eq!(approval["due"], true);
    assert_eq!(approval["estimated"], true);
}

#[tokio::test]
async fn balances_of_every_account_stop_at_the_rate_limit_with_what_they_have() {
    let fake = Fake::new(&["a1", "a2", "a3"], &[&[]]);
    *fake.budget.lock().unwrap() = 2;
    let tool = BankBalancesTool(ctx(vec![("natwest", fake)], "natwest"));
    let out = tool.execute(json!({})).await.unwrap();
    let got = data(&out.output);
    assert_eq!(got["balances"].as_array().unwrap().len(), 2);
    assert_eq!(got["partial"], true);
}

/// Every read of what a bank says comes fenced, so a turn that reads it is
/// armed against it as against any outside content.
#[tokio::test]
async fn what_a_bank_says_reaches_the_model_fenced() {
    let ctx = || ctx(vec![("natwest", Fake::new(&["acc"], &[&["a"]]))], "natwest");
    let reads: [Box<dyn Tool>; 3] = [
        Box::new(BankAccountsTool(ctx())),
        Box::new(BankBalancesTool(ctx())),
        Box::new(BankTransactionsTool(ctx())),
    ];
    for tool in reads {
        let out = tool.execute(json!({})).await.unwrap();
        assert!(out.success, "{}: {:?}", tool.name(), out.error);
        assert!(
            out.output["data"]
                .as_str()
                .unwrap()
                .contains("EXTERNAL_UNTRUSTED_CONTENT"),
            "{}",
            tool.name()
        );
    }
}
