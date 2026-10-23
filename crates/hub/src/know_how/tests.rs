use std::collections::BTreeMap;

use super::code::Symbol;
use super::*;
use async_trait::async_trait;
use workforce_agent::runner::ToolInvocation;
use workforce_common::metabase_schema::{
    DbSchema, InstanceSchema, MetabaseSchemaStore, TableUsage,
};

const SITE: &str = "mb.example.com";
const REPO: &str = "acme/app";
const A: &str = "11111111-1111-1111-1111-111111111111";
const B: &str = "22222222-2222-2222-2222-222222222222";
const C: &str = "33333333-3333-3333-3333-333333333333";

/// One site with one database (3), which tests may change.
struct Schemas(Mutex<BTreeMap<i64, DbSchema>>);

impl Schemas {
    fn orders() -> Arc<Self> {
        let mut columns = BTreeMap::new();
        columns.insert(
            "public.orders".to_string(),
            vec!["id".into(), "status".into(), "starts_at".into()],
        );
        Arc::new(Self(Mutex::new(BTreeMap::from([(
            3,
            DbSchema {
                tables: vec!["public.orders".into()],
                columns,
                engine: None,
            },
        )]))))
    }

    fn edit(&self, f: impl FnOnce(&mut BTreeMap<i64, DbSchema>)) {
        f(&mut self.0.lock());
    }
}

#[async_trait]
impl MetabaseSchemaStore for Schemas {
    async fn load(&self, instance: &str, _site: &str) -> eyre::Result<InstanceSchema> {
        let mut out = InstanceSchema::default();
        if instance == SITE {
            out.databases = self.0.lock().clone();
        }
        Ok(out)
    }
    async fn upsert_database(&self, _: &str, _: &str, _: i64, _: &DbSchema) -> eyre::Result<()> {
        Ok(())
    }
    async fn upsert_engines(&self, _: &str, _: &str, _: &[(i64, String)]) -> eyre::Result<()> {
        Ok(())
    }
    async fn prune_databases(&self, _: &str, _: &[i64]) -> eyre::Result<()> {
        Ok(())
    }
    async fn add_usage(&self, _: &str, _: &[(i64, String, TableUsage)]) -> eyre::Result<()> {
        Ok(())
    }
}

/// One indexed repo, `acme/app`, whose files tests may change: each file
/// with the symbols the code graph has in it.
struct Code(Mutex<HashMap<String, Vec<Symbol>>>);

impl Code {
    fn app() -> Arc<Self> {
        Arc::new(Self(Mutex::new(HashMap::from([(
            "src/orders.rs".to_string(),
            vec![
                Symbol {
                    name: "Orders".into(),
                    start: 1,
                    end: 80,
                },
                Symbol {
                    name: "fill_rate".into(),
                    start: 10,
                    end: 30,
                },
            ],
        )]))))
    }

    fn edit(&self, f: impl FnOnce(&mut HashMap<String, Vec<Symbol>>)) {
        f(&mut self.0.lock());
    }
}

#[async_trait]
impl CodeIndex for Code {
    fn resolve(&self, repo: Option<&str>) -> Option<String> {
        matches!(repo, None | Some("app") | Some(REPO)).then(|| REPO.to_string())
    }
    fn repos(&self) -> HashSet<String> {
        HashSet::from([REPO.to_string()])
    }
    fn has_file(&self, repo: &str, path: &str) -> Option<bool> {
        (repo == REPO).then(|| self.0.lock().contains_key(path))
    }
    async fn symbols(&self, _: &str, path: &str) -> Vec<Symbol> {
        self.0.lock().get(path).cloned().unwrap_or_default()
    }
    async fn defining(&self, _: &str, name: &str) -> Vec<String> {
        let files = self.0.lock();
        let mut out: Vec<String> = files
            .iter()
            .filter(|(_, symbols)| symbols.iter().any(|s| s.name == name))
            .map(|(path, _)| path.clone())
            .collect();
        out.sort();
        out
    }
}

/// Bag of words hashed into 64 dimensions: shared words, close vectors.
struct Words;

#[async_trait]
impl EmbeddingBackend for Words {
    async fn embed(&self, text: &str) -> eyre::Result<Vec<f32>> {
        let mut v = vec![0.0f32; 64];
        for w in text.to_lowercase().split(|c: char| !c.is_alphanumeric()) {
            if w.len() > 2 {
                let h = w
                    .bytes()
                    .fold(7u32, |h, b| h.wrapping_mul(31).wrapping_add(b as u32));
                v[(h % 64) as usize] += 1.0;
            }
        }
        Ok(v)
    }
    fn model_id(&self) -> &str {
        "words"
    }
}

fn know_how_with(schemas: Arc<Schemas>, code: Arc<Code>) -> (Arc<InMemoryKnowHowStore>, KnowHow) {
    let store = Arc::new(InMemoryKnowHowStore::default());
    let kh = KnowHow::new(
        Arc::clone(&store) as Arc<dyn KnowHowStore>,
        Some(Arc::new(Words)),
    )
    .with_schemas(schemas)
    .with_code_index(code)
    .with_person_key([7; 32]);
    (store, kh)
}

fn know_how() -> (Arc<InMemoryKnowHowStore>, KnowHow) {
    know_how_with(Schemas::orders(), Code::app())
}

fn cfg() -> KnowHowConfig {
    KnowHowConfig {
        min_score: 0.8,
        ..Default::default()
    }
}

fn sites() -> Sites {
    Sites {
        instances: vec![("default".into(), SITE.into())],
        default: Some("default".into()),
        pinned: None,
    }
}

fn access() -> Access {
    Access {
        sites: sites(),
        repos: HashSet::from([REPO.to_string()]),
        policy: None,
        admits: Some(Arc::new(|_, _| true)),
    }
}

fn call(
    tool: &str,
    input: serde_json::Value,
    output: serde_json::Value,
    success: bool,
) -> ToolInvocation {
    ToolInvocation {
        tool_name: tool.into(),
        input,
        error: (!success).then(|| output.to_string()),
        output,
        success,
        policy_decision: "allow".into(),
        duration_ms: 1,
        denial_reason: None,
    }
}

fn sql_call(query: &str, success: bool) -> ToolInvocation {
    call(
        SQL_TOOL,
        serde_json::json!({ "database_id": 3, "query": query }),
        serde_json::json!({}),
        success,
    )
}

/// A query the database refused with `error`.
fn sql_failed(query: &str, error: &str) -> ToolInvocation {
    ToolInvocation {
        error: Some(error.to_string()),
        ..sql_call(query, false)
    }
}

fn writes(intent: &'static str) -> IntentWriter {
    Box::new(move |_q| Box::pin(async move { Some(intent.to_string()) }))
}

async fn turn(
    kh: &KnowHow,
    who: &str,
    session: &str,
    calls: &[ToolInvocation],
    intent: &'static str,
) {
    kh.observe(
        &cfg(),
        Turn {
            agent: "atlas",
            identity: who,
            shared: false,
            question: "q",
            invocations: calls,
            sites: sites(),
        },
        kh.take_matched(session),
        writes(intent),
    )
    .await;
}

async fn ask(
    kh: &KnowHow,
    who: &str,
    session: &str,
    question: &str,
    sql: &str,
    intent: &'static str,
) {
    let calls = [sql_call(sql, true)];
    kh.observe(
        &cfg(),
        Turn {
            agent: "atlas",
            identity: who,
            shared: false,
            question,
            invocations: &calls,
            sites: sites(),
        },
        kh.take_matched(session),
        writes(intent),
    )
    .await;
}

const QUERY: &str =
    "SELECT status, count(*) FROM orders WHERE starts_at > '2026-09-01' GROUP BY status";

#[tokio::test]
async fn a_successful_query_teaches_a_location_and_a_masked_shape() {
    let (_, kh) = know_how();
    ask(
        &kh,
        A,
        "s1",
        "how many orders did Jane fill in September?",
        QUERY,
        "order counts by status",
    )
    .await;
    let entries = kh.store().entries("atlas").await.unwrap();
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].tables, ["public.orders"]);
    assert_eq!(entries[0].people, 1);
    let shape = entries[1].shape.as_deref().unwrap();
    assert!(!shape.contains("2026"), "{shape}");
    for e in &entries {
        let all = format!("{e:?}");
        assert!(!all.contains("Jane") && !all.contains("September"), "{all}");
    }
}

#[tokio::test]
async fn others_see_an_entry_only_once_two_people_produced_it() {
    let (_, kh) = know_how();
    ask(
        &kh,
        A,
        "s1",
        "order counts?",
        QUERY,
        "order counts by status",
    )
    .await;
    assert!(
        kh.visible("atlas", &cfg(), B, &access(), false)
            .await
            .unwrap()
            .is_empty(),
        "one person is not enough"
    );
    assert_eq!(
        kh.visible("atlas", &cfg(), A, &access(), false)
            .await
            .unwrap()
            .len(),
        2,
        "your own is yours"
    );
    ask(
        &kh,
        B,
        "s2",
        "status breakdown of orders",
        QUERY,
        "orders broken down by status",
    )
    .await;
    assert_eq!(
        kh.visible("atlas", &cfg(), C, &access(), false)
            .await
            .unwrap()
            .len(),
        2
    );
    let elsewhere = Access {
        sites: Sites {
            instances: vec![("default".into(), "other.example.com".into())],
            default: Some("default".into()),
            pinned: None,
        },
        ..access()
    };
    assert!(
        kh.visible("atlas", &cfg(), C, &elsewhere, false)
            .await
            .unwrap()
            .is_empty(),
        "never shown to someone whose Metabase is another site"
    );
}

#[tokio::test]
async fn a_similar_request_matches_and_an_unrelated_one_does_not() {
    let (_, kh) = know_how();
    ask(&kh, A, "s1", "q", QUERY, "order counts by status").await;
    ask(&kh, B, "s2", "q", QUERY, "order counts by status per week").await;
    let hit = kh
        .matches(
            "atlas",
            &cfg(),
            C,
            "order counts by status for last month",
            &access(),
            false,
        )
        .await
        .unwrap();
    assert!(!hit.is_empty());
    let miss = kh
        .matches(
            "atlas",
            &cfg(),
            C,
            "invoice totals per customer",
            &access(),
            false,
        )
        .await
        .unwrap();
    assert!(miss.is_empty(), "{miss:?}");
}

#[tokio::test]
async fn an_intent_carrying_names_or_numbers_is_dropped() {
    assert_eq!(
        screen_intent("order counts by status"),
        Some("order counts by status".into())
    );
    for bad in [
        "orders Jane filled",
        "orders in 2026",
        "orders at site 12",
        "mail bob@x.com",
        "\"quoted\" thing x",
    ] {
        assert_eq!(screen_intent(bad), None, "{bad}");
    }
    let (store, kh) = know_how();
    ask(&kh, A, "s1", "q", QUERY, "orders Jane filled").await;
    assert_eq!(
        kh.store().entries("atlas").await.unwrap().len(),
        2,
        "the names are still learned"
    );
    assert!(
        store.intents("atlas", "words").await.unwrap().is_empty(),
        "but no intent to match by"
    );
}

#[tokio::test]
async fn shadow_records_without_showing_and_on_shows_names_only() {
    let (store, kh) = know_how();
    ask(&kh, A, "s1", "q", QUERY, "order counts by status").await;
    ask(&kh, B, "s2", "q", QUERY, "order counts by status").await;
    let shadow = kh
        .for_request(
            "atlas",
            "s3",
            &cfg(),
            C,
            "order counts by status please",
            &access(),
            false,
        )
        .await;
    assert_eq!(shadow, None);
    let on = KnowHowConfig {
        inject: KnowHowInject::On,
        ..cfg()
    };
    let block = kh
        .for_request(
            "atlas",
            "s4",
            &on,
            C,
            "order counts by status please",
            &access(),
            false,
        )
        .await
        .expect("shown");
    assert!(block.contains("public.orders"), "{block}");
    assert!(
        !block.contains("please") && !block.contains("2026"),
        "{block}"
    );
    // The turn then read the table: a hit.
    ask(&kh, C, "s4", "q", QUERY, "order counts by status").await;
    let events = store.events();
    assert!(events.iter().any(|(_, k)| k == "shadow"));
    assert!(events.iter().any(|(_, k)| k == "hit"), "{events:?}");
}

#[tokio::test]
async fn failed_queries_and_unknown_tables_teach_nothing() {
    let (_, kh) = know_how();
    let calls = [
        sql_call(QUERY, false),
        sql_call("SELECT * FROM payroll_secret", true),
    ];
    kh.observe(
        &cfg(),
        Turn {
            agent: "atlas",
            identity: A,
            shared: false,
            question: "q",
            invocations: &calls,
            sites: sites(),
        },
        kh.take_matched("s1"),
        writes("payroll"),
    )
    .await;
    assert!(kh.store().entries("atlas").await.unwrap().is_empty());
}

#[tokio::test]
async fn forgetting_a_person_removes_what_only_they_produced() {
    let (_, kh) = know_how();
    ask(&kh, A, "s1", "q", QUERY, "order counts by status").await;
    kh.store()
        .forget_person("atlas", &kh.person("atlas", A).unwrap())
        .await
        .unwrap();
    assert!(kh.store().entries("atlas").await.unwrap().is_empty());
    ask(&kh, B, "s2", "q", QUERY, "order counts by status").await;
    assert_eq!(
        kh.forget_identity(B).await.unwrap(),
        2,
        "deleting an identity forgets them everywhere"
    );
    assert!(kh.store().entries("atlas").await.unwrap().is_empty());
}

#[tokio::test]
async fn in_a_shared_conversation_only_shared_entries_are_used() {
    let (_, kh) = know_how();
    ask(&kh, A, "s1", "q", QUERY, "order counts by status").await;
    assert_eq!(
        kh.visible("atlas", &cfg(), A, &access(), false)
            .await
            .unwrap()
            .len(),
        2
    );
    assert!(
        kh.visible("atlas", &cfg(), A, &access(), true)
            .await
            .unwrap()
            .is_empty(),
        "A's own entries never reach a room through A"
    );
}

#[tokio::test]
async fn similar_work_confirms_the_table_and_only_the_columns_both_read() {
    // Alice reads status and starts_at; Bob reads status alone.
    let (_, kh) = know_how();
    ask(
        &kh,
        A,
        "s1",
        "q",
        "SELECT status, starts_at FROM orders",
        "order statuses and starts",
    )
    .await;
    ask(
        &kh,
        B,
        "s2",
        "q",
        "SELECT status, count(*) FROM orders GROUP BY status",
        "order counts by status",
    )
    .await;
    let seen = kh
        .visible("atlas", &cfg(), C, &access(), false)
        .await
        .unwrap();
    let tables: Vec<&Entry> = seen
        .iter()
        .filter(|e| e.kind == Kind::DataLocation)
        .collect();
    assert_eq!(tables.len(), 1, "the shared table: {seen:?}");
    assert_eq!(
        tables[0].columns,
        ["public.orders.status"],
        "only the column both read"
    );
    assert!(
        seen.iter().all(|e| e.kind != Kind::QueryShape
            || !e.shape.as_deref().unwrap_or("").contains("starts_at")),
        "no shape naming a column only one person read: {seen:?}"
    );
    let alice = kh
        .visible("atlas", &cfg(), A, &access(), false)
        .await
        .unwrap();
    assert!(
        alice
            .iter()
            .any(|e| e.columns.contains(&"public.orders.starts_at".to_string())),
        "Alice still sees her own in full"
    );
}

#[tokio::test]
async fn without_a_person_key_nothing_is_recorded() {
    let kh = KnowHow::new(
        Arc::new(InMemoryKnowHowStore::default()),
        Some(Arc::new(Words)),
    )
    .with_schemas(Schemas::orders());
    ask(&kh, A, "s1", "q", QUERY, "order counts by status").await;
    assert!(kh.store().entries("atlas").await.unwrap().is_empty());
}

#[test]
fn the_person_hash_is_keyed() {
    let a = KnowHow::new(Arc::new(InMemoryKnowHowStore::default()), None).with_person_key([1; 32]);
    let b = KnowHow::new(Arc::new(InMemoryKnowHowStore::default()), None).with_person_key([2; 32]);
    assert_ne!(a.person("atlas", A), b.person("atlas", A));
    assert_eq!(a.person("atlas", A), a.person("atlas", A));
    assert_ne!(a.person("atlas", A), a.person("bernard", A));
}

#[test]
fn an_agent_may_raise_min_users_but_not_lower_it() {
    use workforce_common::settings::{AgentKnowHowConfig, ProgramsMode};
    let install = KnowHowConfig {
        mode: ProgramsMode::On,
        ..Default::default()
    };
    let lower = AgentKnowHowConfig {
        min_users: Some(1),
        ..Default::default()
    };
    let higher = AgentKnowHowConfig {
        min_users: Some(4),
        ..Default::default()
    };
    assert_eq!(install.for_agent(Some(&lower)).unwrap().min_users, 2);
    assert_eq!(install.for_agent(Some(&higher)).unwrap().min_users, 4);
}

// ── schema corrections ──────────────────────────────────────────────────

const WRONG: &str = "SELECT state, count(*) FROM orders GROUP BY state";
const RIGHT: &str = "SELECT status, count(*) FROM orders GROUP BY status";
const NO_STATE: &str = "ERROR: column \"state\" does not exist\n  Position: 8";

async fn corrects(kh: &KnowHow, who: &str, session: &str) {
    let calls = [sql_failed(WRONG, NO_STATE), sql_call(RIGHT, true)];
    turn(kh, who, session, &calls, "order counts by status").await;
}

fn corrections(entries: &[Entry]) -> Vec<&Entry> {
    entries
        .iter()
        .filter(|e| e.kind == Kind::SchemaCorrection)
        .collect()
}

#[tokio::test]
async fn a_retry_that_fixes_a_column_teaches_the_correction_once_shared() {
    let (_, kh) = know_how();
    corrects(&kh, A, "s1").await;
    let all = kh.store().entries("atlas").await.unwrap();
    let fix = corrections(&all);
    assert_eq!(fix.len(), 1, "{all:?}");
    assert_eq!(fix[0].wrong.as_deref(), Some("public.orders.state"));
    assert_eq!(fix[0].columns, ["public.orders.status"]);
    assert!(
        corrections(
            &kh.visible("atlas", &cfg(), C, &access(), false)
                .await
                .unwrap()
        )
        .is_empty(),
        "one person is not enough"
    );
    corrects(&kh, B, "s2").await;
    let matched = kh
        .matches(
            "atlas",
            &cfg(),
            C,
            "order counts by status",
            &access(),
            false,
        )
        .await
        .unwrap();
    let block = render(&matched, 800);
    assert!(
        block.contains("`public.orders.state` does not exist; use `public.orders.status`"),
        "{block}"
    );
}

#[tokio::test]
async fn a_correction_hides_once_the_real_table_is_no_longer_shared() {
    let (_, kh) = know_how();
    corrects(&kh, A, "s1").await;
    corrects(&kh, B, "s2").await;
    let seen = kh
        .visible("atlas", &cfg(), C, &access(), false)
        .await
        .unwrap();
    assert_eq!(corrections(&seen).len(), 1);
    let table = seen
        .iter()
        .find(|e| e.kind == Kind::DataLocation)
        .expect("the table is shared");
    kh.retire("atlas", table.id).await.unwrap();
    assert!(
        corrections(
            &kh.visible("atlas", &cfg(), C, &access(), false)
                .await
                .unwrap()
        )
        .is_empty(),
        "a correction points only at a name others may see"
    );
}

#[tokio::test]
async fn a_literal_the_database_read_as_a_name_is_never_learned() {
    let (_, kh) = know_how();
    let no_jane = "ERROR: column \"jane\" does not exist";
    for retry in [
        "SELECT id FROM orders WHERE status = 'Jane'",
        "SELECT starts_at FROM orders WHERE status = 'Jane'",
    ] {
        let calls = [
            sql_failed("SELECT id FROM orders WHERE status = Jane", no_jane),
            sql_call(retry, true),
        ];
        turn(&kh, A, "s1", &calls, "orders by status").await;
        turn(&kh, B, "s2", &calls, "orders by status").await;
    }
    let all = kh.store().entries("atlas").await.unwrap();
    assert!(corrections(&all).is_empty(), "{all:?}");
    assert!(
        !format!("{all:?}").to_lowercase().contains("jane"),
        "{all:?}"
    );
}

#[tokio::test]
async fn a_rejected_name_the_schema_has_or_a_retry_elsewhere_teaches_no_correction() {
    let (_, kh) = know_how();
    let calls = [
        sql_failed(
            "SELECT id, count(*) FROM orders GROUP BY id",
            "ERROR: column \"id\" does not exist",
        ),
        sql_call("SELECT status, count(*) FROM orders GROUP BY status", true),
    ];
    turn(&kh, A, "s1", &calls, "order counts").await;
    let other_database = [
        sql_failed(WRONG, NO_STATE),
        call(
            SQL_TOOL,
            serde_json::json!({ "database_id": 4, "query": RIGHT }),
            serde_json::json!({}),
            true,
        ),
    ];
    turn(&kh, A, "s2", &other_database, "order counts").await;
    assert!(corrections(&kh.store().entries("atlas").await.unwrap()).is_empty());
}

// ── code locations ──────────────────────────────────────────────────────

const PATTERN: &str = "fill_rate_[a-z]+_lookup";
const LINE: &str = "let token = \"hunter2\";";

fn grep() -> ToolInvocation {
    grep_in("orders.rs")
}

/// A grep under `src` hitting line 12 of `file`.
fn grep_in(file: &str) -> ToolInvocation {
    call(
        "repo.grep",
        serde_json::json!({ "repo": "app", "pattern": PATTERN, "path": "src" }),
        serde_json::json!({ "matches": [{ "path": file, "line_number": 12, "line": LINE }] }),
        true,
    )
}

fn read(path: &str) -> ToolInvocation {
    call(
        code::READ_TOOL,
        serde_json::json!({ "repo": REPO, "path": path }),
        serde_json::json!({ "content": LINE }),
        true,
    )
}

async fn reads_code(kh: &KnowHow, who: &str, session: &str) {
    let calls = [grep(), read("./src/orders.rs")];
    turn(
        kh,
        who,
        session,
        &calls,
        "how the delivery time is computed",
    )
    .await;
}

fn code_of(entries: &[Entry]) -> Vec<(Option<&str>, Option<&str>)> {
    entries
        .iter()
        .filter(|e| e.kind == Kind::CodeLocation)
        .map(|e| (e.path.as_deref(), e.symbol.as_deref()))
        .collect()
}

#[tokio::test]
async fn a_search_hit_then_read_teaches_the_file_and_symbol_never_the_pattern_or_line() {
    let (_, kh) = know_how();
    reads_code(&kh, A, "s1").await;
    let all = kh.store().entries("atlas").await.unwrap();
    assert_eq!(
        code_of(&all),
        [
            (Some("src/orders.rs"), None),
            (Some("src/orders.rs"), Some("fill_rate"))
        ]
    );
    assert!(all.iter().all(|e| e.repo.as_deref() == Some(REPO)));
    let dump = format!("{all:?}");
    assert!(
        !dump.contains("fill_rate_[a-z]") && !dump.contains("hunter2"),
        "{dump}"
    );
}

#[tokio::test]
async fn a_read_no_search_hit_led_to_teaches_nothing() {
    let (_, kh) = know_how();
    let calls = [read("src/orders.rs"), grep()];
    turn(&kh, A, "s1", &calls, "delivery time code").await;
    let missing = [grep(), read("src/gone.rs")];
    turn(&kh, A, "s2", &missing, "delivery time code").await;
    assert!(kh.store().entries("atlas").await.unwrap().is_empty());
}

#[tokio::test]
async fn code_is_shown_once_shared_and_only_to_people_who_can_read_the_repo() {
    let (_, kh) = know_how();
    reads_code(&kh, A, "s1").await;
    assert!(code_of(
        &kh.visible("atlas", &cfg(), C, &access(), false)
            .await
            .unwrap()
    )
    .is_empty());
    reads_code(&kh, B, "s2").await;
    assert_eq!(
        code_of(
            &kh.visible("atlas", &cfg(), C, &access(), false)
                .await
                .unwrap()
        )
        .len(),
        2
    );
    let no_code = Access {
        repos: HashSet::new(),
        ..access()
    };
    assert!(
        kh.visible("atlas", &cfg(), C, &no_code, false)
            .await
            .unwrap()
            .is_empty(),
        "never shown to someone whose code tools cannot read the repo"
    );
    let on = KnowHowConfig {
        inject: KnowHowInject::On,
        ..cfg()
    };
    let block = kh
        .for_request(
            "atlas",
            "s3",
            &on,
            C,
            "how the delivery time is computed",
            &access(),
            false,
        )
        .await
        .expect("shown");
    assert!(
        block.contains("- repo acme/app: `fill_rate` in src/orders.rs"),
        "{block}"
    );
    assert!(
        !block.contains("- repo acme/app: src/orders.rs\n"),
        "the file once: {block}"
    );
}

#[tokio::test]
async fn reading_a_matched_file_scores_a_hit() {
    let (store, kh) = know_how();
    reads_code(&kh, A, "s1").await;
    reads_code(&kh, B, "s2").await;
    kh.for_request(
        "atlas",
        "s3",
        &cfg(),
        C,
        "how the delivery time is computed",
        &access(),
        false,
    )
    .await;
    let calls = [read("src/orders.rs")];
    turn(&kh, C, "s3", &calls, "how the delivery time is computed").await;
    let events = store.events();
    assert!(events.iter().any(|(_, k)| k == "shadow_hit"), "{events:?}");
    assert!(
        !events.iter().any(|(_, k)| k == "shadow_miss"),
        "{events:?}"
    );
}

#[tokio::test]
async fn an_evals_question_gets_the_block_and_leaves_no_match_on_record() {
    let (store, kh) = know_how();
    reads_code(&kh, A, "s1").await;
    reads_code(&kh, B, "s2").await;
    let before = store.events();
    let asked = "how the delivery time is computed";
    let on = KnowHowConfig {
        inject: KnowHowInject::On,
        ..cfg()
    };
    let block = kh
        .for_eval("atlas", &on, C, asked, &access(), false)
        .await
        .expect("shown under inject = on");
    assert!(block.contains("src/orders.rs"), "{block}");
    let shadowed = kh
        .for_eval("atlas", &cfg(), C, asked, &access(), false)
        .await;
    assert_eq!(shadowed, None, "shadow shows nothing");
    assert_eq!(store.events(), before, "and neither is recorded");
    let as_a_person = kh
        .for_request("atlas", "s3", &on, C, asked, &access(), false)
        .await;
    assert_eq!(as_a_person, Some(block), "the block a person's turn gets");
    assert!(store.events().len() > before.len());
}

// ── kinds and scoring ───────────────────────────────────────────────────

const BOTH: &str = "order counts by status and how the delivery time is computed";

/// Shared data and code entries, both matching [`BOTH`].
async fn data_and_code(kh: &KnowHow) {
    for (who, data, code) in [(A, "s1", "s2"), (B, "s3", "s4")] {
        ask(kh, who, data, "q", QUERY, "order counts by status").await;
        reads_code(kh, who, code).await;
    }
}

fn of_kinds(kinds: Vec<Kind>) -> KnowHowConfig {
    KnowHowConfig {
        min_score: 0.55,
        max_entries: 10,
        kinds,
        ..cfg()
    }
}

fn kinds_of(matched: &[(Entry, f32)]) -> Vec<Kind> {
    matched.iter().map(|(e, _)| e.kind).collect()
}

/// The outcomes scored since `from` events were recorded.
fn outcomes(store: &InMemoryKnowHowStore, from: usize) -> Vec<String> {
    store.events()[from..]
        .iter()
        .map(|(_, k)| k.clone())
        .filter(|k| k != "shadow" && k != "shown")
        .collect()
}

/// Matching as it was before `kinds`: every visible entry, by its best
/// intent, at or above `min_score`, best first, cut to `max_entries`.
async fn matched_before_kinds(
    kh: &KnowHow,
    cfg: &KnowHowConfig,
    question: &str,
) -> Vec<(i64, f32)> {
    let visible = kh.visible("atlas", cfg, C, &access(), false).await.unwrap();
    let ids: HashSet<i64> = visible.iter().map(|e| e.id).collect();
    let intents = kh.snapshot("atlas").await.unwrap().intents;
    let q = Words.embed_query(question).await.unwrap();
    let mut best: HashMap<i64, f32> = HashMap::new();
    for (id, v) in intents.iter().filter(|(id, _)| ids.contains(id)) {
        let s = certainty(&q, v);
        let slot = best.entry(*id).or_insert(0.0);
        if s > *slot {
            *slot = s;
        }
    }
    let mut out: Vec<(i64, f32)> = visible
        .iter()
        .filter_map(|e| best.get(&e.id).map(|s| (e.id, *s)))
        .filter(|(_, s)| *s >= cfg.min_score)
        .collect();
    out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
    out.truncate(cfg.max_entries.max(1));
    out
}

#[tokio::test]
async fn with_no_kinds_set_the_same_entries_are_matched_recorded_and_shown_as_before() {
    let (store, kh) = know_how();
    data_and_code(&kh).await;
    corrects(&kh, A, "s5").await;
    corrects(&kh, B, "s6").await;
    let kinds: HashSet<Kind> = kh
        .visible("atlas", &cfg(), C, &access(), false)
        .await
        .unwrap()
        .iter()
        .map(|e| e.kind)
        .collect();
    assert_eq!(kinds, HashSet::from(Kind::ALL), "a store of every kind");
    let install: WorkforceConfig =
        toml::from_str("[know_how]\nmin_score = 0.55\nmax_entries = 3\n").unwrap();
    let own =
        workforce_common::settings::AgentWorkspaceConfig::parse("[know_how]\nenabled = true\n")
            .unwrap();
    let shadow = install.know_how.for_workspace(Some(&own)).unwrap();
    let on = KnowHowConfig {
        inject: KnowHowInject::On,
        ..shadow.clone()
    };
    let questions = [
        BOTH,
        "order counts by status",
        "how the delivery time is computed",
    ];
    let visible = kh.visible("atlas", &on, C, &access(), false).await.unwrap();
    for (i, question) in questions.into_iter().enumerate() {
        let before = matched_before_kinds(&kh, &shadow, question).await;
        assert!(!before.is_empty(), "{question}");
        let now: Vec<(i64, f32)> = kh
            .matched_for("atlas", &shadow, C, question, &access(), false)
            .await
            .iter()
            .map(|(e, s)| (e.id, *s))
            .collect();
        assert_eq!(now, before, "{question}");

        let from = store.events().len();
        let session = format!("same-{i}");
        let block = kh
            .for_request("atlas", &session, &shadow, C, question, &access(), false)
            .await;
        assert_eq!(block, None, "shadow shows nothing");
        let recorded: Vec<(Option<i64>, String)> = store.events()[from..].to_vec();
        let expected: Vec<(Option<i64>, String)> = before
            .iter()
            .map(|(id, _)| (Some(*id), "shadow".to_string()))
            .collect();
        assert_eq!(recorded, expected, "{question}");

        let shown: Vec<(Entry, f32)> = before
            .iter()
            .map(|(id, s)| (visible.iter().find(|e| e.id == *id).unwrap().clone(), *s))
            .collect();
        let block = kh
            .for_request("atlas", &session, &on, C, question, &access(), false)
            .await;
        assert_eq!(block, Some(render(&shown, on.max_tokens)), "{question}");
    }
}

#[tokio::test]
async fn only_the_kinds_an_agent_matches_are_matched_recorded_and_counted() {
    let (store, kh) = know_how();
    data_and_code(&kh).await;
    let every = kh
        .matched_for("atlas", &of_kinds(Kind::all()), C, BOTH, &access(), false)
        .await;
    assert_eq!(
        kinds_of(&every).into_iter().collect::<HashSet<_>>(),
        HashSet::from([Kind::DataLocation, Kind::QueryShape, Kind::CodeLocation])
    );
    let shapes = of_kinds(vec![Kind::QueryShape]);
    let matched = kh
        .matched_for("atlas", &shapes, C, BOTH, &access(), false)
        .await;
    assert_eq!(kinds_of(&matched), [Kind::QueryShape]);

    let top = |kinds| KnowHowConfig {
        max_entries: 1,
        ..of_kinds(kinds)
    };
    let first = kh
        .matched_for("atlas", &top(Kind::all()), C, BOTH, &access(), false)
        .await;
    assert_ne!(kinds_of(&first), [Kind::QueryShape]);
    let first_shape = kh
        .matched_for(
            "atlas",
            &top(vec![Kind::QueryShape]),
            C,
            BOTH,
            &access(),
            false,
        )
        .await;
    assert_eq!(
        kinds_of(&first_shape),
        [Kind::QueryShape],
        "a kind left out takes no place"
    );

    let before = store.events().len();
    kh.for_request("atlas", "s5", &shapes, C, BOTH, &access(), false)
        .await;
    let shape_ids: HashSet<i64> = store
        .entries("atlas")
        .await
        .unwrap()
        .iter()
        .filter(|e| e.kind == Kind::QueryShape)
        .map(|e| e.id)
        .collect();
    let recorded = &store.events()[before..];
    assert!(!recorded.is_empty());
    assert!(
        recorded
            .iter()
            .all(|(id, k)| k == "shadow" && id.is_some_and(|id| shape_ids.contains(&id))),
        "{recorded:?}"
    );

    let preview = kh
        .preview(
            "atlas",
            Some(&shapes),
            Some(&shapes),
            C,
            &access(),
            &[BOTH.to_string()],
        )
        .await
        .unwrap();
    assert_eq!(preview.matched, [1]);
    assert_eq!(
        preview.visible,
        kh.visible("atlas", &shapes, C, &access(), false)
            .await
            .unwrap()
            .len(),
        "every entry the person can see is still counted as visible"
    );
}

#[tokio::test]
async fn the_know_how_tool_searches_every_kind_whatever_an_agent_matches() {
    use workforce_agent::runner::CURRENT_USER_ID;
    use workforce_agent::tools::Tool;
    let (_, kh) = know_how();
    data_and_code(&kh).await;
    let tool = tool::KnowHowTool::new(
        Arc::new(kh),
        "atlas",
        Arc::new(|| Some(of_kinds(vec![Kind::QueryShape]))),
        Arc::new(access),
        Arc::new(|| false),
        Arc::new(|| false),
    );
    let found = CURRENT_USER_ID
        .scope(
            C.to_string(),
            tool.execute(serde_json::json!({ "action": "search", "query": BOTH })),
        )
        .await
        .unwrap();
    assert!(found.success, "{found:?}");
    let kinds: HashSet<String> = found.output["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|e| e["kind"].as_str().unwrap().to_string())
        .collect();
    assert_eq!(
        kinds,
        HashSet::from(["data_location", "query_shape", "code_location"].map(String::from))
    );
}

#[tokio::test]
async fn an_entry_a_turn_had_no_use_for_is_untried_and_one_it_passed_over_a_miss() {
    let (store, kh) = know_how();
    data_and_code(&kh).await;
    let (cfg, access) = (of_kinds(Kind::all()), access());
    let entries = store.entries("atlas").await.unwrap();
    let is_code = |id: Option<i64>| {
        entries
            .iter()
            .any(|e| Some(e.id) == id && e.kind == Kind::CodeLocation)
    };
    // Each outcome scored for a turn making `calls`, for code entries and for
    // data entries, through the path the hub takes for it.
    let scored = |session: &'static str, calls: Vec<ToolInvocation>| {
        let (store, kh, cfg, access, is_code) = (&store, &kh, &cfg, &access, &is_code);
        async move {
            let before = store.events().len();
            kh.for_request("atlas", session, cfg, C, BOTH, access, false)
                .await;
            let matched = kh.take_matched(session).expect("matched");
            match teaches(&calls) {
                true => {
                    kh.observe(
                        cfg,
                        Turn {
                            agent: "atlas",
                            identity: C,
                            shared: false,
                            question: BOTH,
                            invocations: &calls,
                            sites: sites(),
                        },
                        Some(matched),
                        writes("other work"),
                    )
                    .await
                }
                false => kh.settle("atlas", matched, &calls).await,
            }
            store.events()[before..]
                .iter()
                .filter(|(_, k)| k != "shadow")
                .map(|(id, k)| (is_code(*id), k.clone()))
                .collect::<HashSet<_>>()
        }
    };
    let outcome = |data: &str, code: &str| {
        HashSet::from([
            (false, format!("shadow_{data}")),
            (true, format!("shadow_{code}")),
        ])
    };
    let queried = scored("s5", vec![sql_call("SELECT * FROM payroll_secret", true)]).await;
    assert_eq!(queried, outcome("miss", "untried"));
    let failed = scored("s6", vec![sql_failed(WRONG, NO_STATE)]).await;
    assert_eq!(
        failed,
        outcome("miss", "untried"),
        "a failed query is data work"
    );
    let read_code = scored("s7", vec![read("src/other.rs")]).await;
    assert_eq!(read_code, outcome("untried", "miss"));
    let searched = scored("s8", vec![grep_in("other.rs")]).await;
    assert_eq!(
        searched,
        outcome("untried", "miss"),
        "a search is code work"
    );
    let card = call(
        "metabase.card_query",
        serde_json::json!({ "card_id": 7 }),
        serde_json::json!({ "rows": [] }),
        true,
    );
    let other = scored("s9", vec![card]).await;
    assert_eq!(
        other,
        outcome("untried", "untried"),
        "a saved question names no tables a hit could be read from"
    );
}

#[tokio::test]
async fn a_turn_that_never_finished_is_settled_untried_by_the_next_request() {
    let (store, kh) = know_how();
    data_and_code(&kh).await;
    let cfg = of_kinds(Kind::all());
    let before = store.events().len();
    kh.for_request("atlas", "s5", &cfg, C, BOTH, &access(), false)
        .await;
    kh.for_request("atlas", "s5", &cfg, C, BOTH, &access(), false)
        .await;
    let scored = outcomes(&store, before);
    assert!(
        !scored.is_empty() && scored.iter().all(|k| k == "shadow_untried"),
        "{scored:?}"
    );
    let matched = kh.take_matched("s5").expect("the second request's");
    assert!(kh.take_matched("s5").is_none(), "taken once");
    let before = store.events().len();
    kh.settle("atlas", matched, &[]).await;
    kh.for_request("atlas", "s5", &cfg, C, BOTH, &access(), false)
        .await;
    assert_eq!(
        outcomes(&store, before).len(),
        scored.len(),
        "settled once, not again by the next request"
    );
}

#[tokio::test]
async fn a_next_request_cannot_settle_a_turn_still_being_scored() {
    let (store, kh) = know_how();
    data_and_code(&kh).await;
    let cfg = of_kinds(vec![Kind::DataLocation]);
    kh.for_request("atlas", "s5", &cfg, C, BOTH, &access(), false)
        .await;
    let ended = kh.take_matched("s5");
    let before = store.events().len();
    kh.for_request("atlas", "s5", &cfg, C, BOTH, &access(), false)
        .await;
    assert!(
        outcomes(&store, before).is_empty(),
        "the next request finds nothing of the last to settle"
    );
    let calls = [sql_call(QUERY, true)];
    kh.observe(
        &cfg,
        Turn {
            agent: "atlas",
            identity: C,
            shared: false,
            question: BOTH,
            invocations: &calls,
            sites: sites(),
        },
        ended,
        writes("order counts by status"),
    )
    .await;
    assert_eq!(outcomes(&store, before), ["shadow_hit"]);
}

// ── re-checks ───────────────────────────────────────────────────────────

#[tokio::test]
async fn a_gone_column_is_dropped_and_a_gone_table_hides_its_entries_until_found_again() {
    let schemas = Schemas::orders();
    let (_, kh) = know_how_with(Arc::clone(&schemas), Code::app());
    ask(&kh, A, "s1", "q", QUERY, "order counts by status").await;
    ask(&kh, B, "s2", "q", QUERY, "order counts by status").await;
    schemas.edit(|dbs| {
        dbs.get_mut(&3)
            .unwrap()
            .columns
            .insert("public.orders".into(), vec!["id".into(), "status".into()]);
    });
    kh.recheck_schemas().await.unwrap();
    let seen = kh
        .visible("atlas", &cfg(), C, &access(), false)
        .await
        .unwrap();
    assert_eq!(
        seen.len(),
        1,
        "the shape naming the gone column is hidden: {seen:?}"
    );
    assert_eq!(seen[0].columns, ["public.orders.status"]);

    schemas.edit(|dbs| {
        let db = dbs.get_mut(&3).unwrap();
        db.tables.clear();
        db.columns.clear();
    });
    let done = kh.recheck_schemas().await.unwrap();
    assert!(done.stale >= 1, "{done:?}");
    assert!(kh
        .visible("atlas", &cfg(), C, &access(), false)
        .await
        .unwrap()
        .is_empty());
    assert!(
        kh.visible("atlas", &cfg(), A, &access(), false)
            .await
            .unwrap()
            .is_empty(),
        "stale even for whoever produced it"
    );

    schemas.edit(|dbs| *dbs = Schemas::orders().0.lock().clone());
    ask(
        &kh,
        A,
        "s3",
        "q",
        "SELECT status FROM orders",
        "order statuses",
    )
    .await;
    let back = kh
        .visible("atlas", &cfg(), C, &access(), false)
        .await
        .unwrap();
    assert!(
        back.iter().any(|e| e.kind == Kind::DataLocation),
        "a turn that reads it again brings it back: {back:?}"
    );
}

#[tokio::test]
async fn a_schema_store_with_nothing_for_the_site_changes_nothing_but_a_pruned_database_does() {
    let schemas = Schemas::orders();
    let (_, kh) = know_how_with(Arc::clone(&schemas), Code::app());
    ask(&kh, A, "s1", "q", QUERY, "order counts by status").await;
    let saved = schemas.0.lock().clone();
    schemas.edit(|dbs| dbs.clear());
    assert_eq!(
        kh.recheck_schemas().await.unwrap(),
        stale::Rechecked::default()
    );
    schemas.edit(|dbs| {
        *dbs = saved;
        let db = dbs.remove(&3).unwrap();
        dbs.insert(4, db);
    });
    kh.recheck_schemas().await.unwrap();
    assert!(kh
        .store()
        .entries("atlas")
        .await
        .unwrap()
        .iter()
        .all(|e| e.stale));
}

#[tokio::test]
async fn a_moved_file_is_followed_by_its_symbol_or_goes_stale() {
    let code = Code::app();
    let (_, kh) = know_how_with(Schemas::orders(), Arc::clone(&code));
    reads_code(&kh, A, "s1").await;
    reads_code(&kh, B, "s2").await;
    code.edit(|files| {
        let symbols = files.remove("src/orders.rs").unwrap();
        files.insert("src/fill.rs".into(), symbols);
    });
    kh.recheck_code().await.unwrap();
    let all = kh.store().entries("atlas").await.unwrap();
    assert!(
        all.iter()
            .any(|e| e.key == "code:acme/app:src/fill.rs#fill_rate" && !e.stale),
        "{all:?}"
    );
    assert!(
        code_of(
            &kh.visible("atlas", &cfg(), C, &access(), false)
                .await
                .unwrap()
        )
        .is_empty(),
        "nobody read the file it moved to yet"
    );
    for (who, session) in [(A, "s3"), (B, "s4")] {
        let calls = [grep_in("fill.rs"), read("src/fill.rs")];
        turn(
            &kh,
            who,
            session,
            &calls,
            "how the delivery time is computed",
        )
        .await;
    }
    let seen = kh
        .visible("atlas", &cfg(), C, &access(), false)
        .await
        .unwrap();
    assert_eq!(
        code_of(&seen),
        [
            (Some("src/fill.rs"), Some("fill_rate")),
            (Some("src/fill.rs"), None)
        ],
        "{seen:?}"
    );

    code.edit(|files| files.clear());
    kh.recheck_code().await.unwrap();
    assert!(kh
        .visible("atlas", &cfg(), C, &access(), false)
        .await
        .unwrap()
        .is_empty());
}

#[tokio::test]
async fn entries_unused_for_ninety_days_go_but_pinned_ones_and_matched_ones_stay() {
    let (store, kh) = know_how();
    ask(&kh, A, "s1", "q", QUERY, "order counts by status").await;
    ask(&kh, B, "s2", "q", QUERY, "order counts by status").await;
    let ids: Vec<i64> = kh
        .store()
        .entries("atlas")
        .await
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect();
    let old = chrono::Utc::now() - chrono::Duration::days(stale::UNUSED_DAYS + 1);
    for id in &ids {
        store.set_last_seen(*id, old);
    }
    kh.store().set_pinned("atlas", ids[0], true).await.unwrap();
    kh.forget_snapshot("atlas");
    // Matched in shadow: still in use.
    kh.for_request(
        "atlas",
        "s3",
        &cfg(),
        C,
        "order counts by status",
        &access(),
        false,
    )
    .await;
    let matched: HashSet<i64> = store
        .events()
        .iter()
        .filter(|(_, k)| k == "shadow")
        .filter_map(|(id, _)| *id)
        .collect();
    assert!(!matched.is_empty());
    kh.prune().await.unwrap();
    let left: HashSet<i64> = kh
        .store()
        .entries("atlas")
        .await
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect();
    assert!(left.contains(&ids[0]), "pinned");
    assert!(matched.iter().all(|id| left.contains(id)), "matched");
    assert!(
        ids.iter()
            .all(|id| left.contains(id) == (*id == ids[0] || matched.contains(id))),
        "the rest are gone: {left:?}"
    );
}

// ── admin ───────────────────────────────────────────────────────────────

#[tokio::test]
async fn admins_see_each_entry_with_its_status_and_never_intents_or_people() {
    let (_, kh) = know_how();
    ask(
        &kh,
        A,
        "s1",
        "q",
        "SELECT status FROM orders",
        "order statuses",
    )
    .await;
    let listed = kh.admin_entries("atlas", &cfg()).await.unwrap();
    assert!(listed.iter().all(|e| e.status == admin::Status::Private));
    ask(
        &kh,
        B,
        "s2",
        "q",
        "SELECT status FROM orders",
        "order statuses",
    )
    .await;
    let listed = kh.admin_entries("atlas", &cfg()).await.unwrap();
    assert!(
        listed.iter().all(|e| e.status == admin::Status::Shared),
        "{listed:?}"
    );
    assert!(listed.iter().all(|e| e.people == 2));

    let json = serde_json::to_string(&listed).unwrap();
    for hidden in ["intent", "vector", "person"] {
        assert!(!json.contains(hidden), "{json}");
    }
    for who in [A, B] {
        assert!(!json.contains(who));
        assert!(!json.contains(&kh.person("atlas", who).unwrap()));
    }
}

async fn status_of(kh: &KnowHow, id: i64) -> admin::Status {
    kh.admin_entries("atlas", &cfg())
        .await
        .unwrap()
        .into_iter()
        .find(|e| e.id == id)
        .unwrap()
        .status
}

#[tokio::test]
async fn only_what_others_see_or_an_addition_can_be_pinned() {
    let (_, kh) = know_how();
    ask(&kh, A, "s1", "q", QUERY, "order counts by status").await;
    let private = kh.store().entries("atlas").await.unwrap()[0].id;
    assert!(!kh.pin("atlas", &cfg(), private).await.unwrap());
    let added = kh
        .store()
        .record(
            "atlas",
            Observation {
                site: SITE.into(),
                database_id: 3,
                tables: vec!["public.orders".into()],
                manual: true,
                ..Observation::of(
                    Kind::DataLocation,
                    "manual:x".into(),
                    kh.person("atlas", B).unwrap(),
                )
            },
        )
        .await
        .unwrap();
    assert_eq!(status_of(&kh, added).await, admin::Status::Pending);
    assert!(kh.pin("atlas", &cfg(), added).await.unwrap());
    assert_eq!(status_of(&kh, added).await, admin::Status::Pinned);
    ask(&kh, B, "s2", "q", QUERY, "order counts by status").await;
    assert!(
        kh.pin("atlas", &cfg(), private).await.unwrap(),
        "shared now"
    );
}

#[tokio::test]
async fn a_retired_entry_stays_removed_and_a_deleted_one_can_be_learned_again() {
    let (_, kh) = know_how();
    ask(
        &kh,
        A,
        "s1",
        "q",
        "SELECT status FROM orders",
        "order statuses",
    )
    .await;
    let id = kh.store().entries("atlas").await.unwrap()[0].id;
    assert!(kh.retire("atlas", id).await.unwrap());
    ask(
        &kh,
        A,
        "s2",
        "q",
        "SELECT status FROM orders",
        "order statuses",
    )
    .await;
    assert!(kh
        .store()
        .entries("atlas")
        .await
        .unwrap()
        .iter()
        .all(|e| e.kind != Kind::DataLocation));

    let shape = kh.store().entries("atlas").await.unwrap()[0].id;
    assert!(kh.delete("atlas", shape).await.unwrap());
    assert!(!kh.delete("atlas", shape).await.unwrap());
    assert!(!kh.delete("other-agent", id).await.unwrap());
    ask(
        &kh,
        A,
        "s3",
        "q",
        "SELECT status FROM orders",
        "order statuses",
    )
    .await;
    assert!(kh
        .store()
        .entries("atlas")
        .await
        .unwrap()
        .iter()
        .any(|e| e.kind == Kind::QueryShape));
}

#[tokio::test]
async fn nothing_of_a_person_outlives_the_entries_that_named_them() {
    let (store, kh) = know_how();
    ask(&kh, A, "s1", "q", QUERY, "order counts by status").await;
    ask(&kh, B, "s2", "q", QUERY, "order counts by status").await;
    let ids: Vec<i64> = kh
        .store()
        .entries("atlas")
        .await
        .unwrap()
        .iter()
        .map(|e| e.id)
        .collect();
    kh.delete("atlas", ids[0]).await.unwrap();
    let old = chrono::Utc::now() - chrono::Duration::days(stale::UNUSED_DAYS + 1);
    for id in &ids[1..] {
        store.set_last_seen(*id, old);
    }
    kh.prune().await.unwrap();
    assert!(kh.store().entries("atlas").await.unwrap().is_empty());
    assert!(
        kh.store().column_people("atlas").await.unwrap().is_empty(),
        "who read a column goes with the last entry naming it"
    );

    ask(&kh, A, "s3", "q", QUERY, "order counts by status").await;
    for id in kh
        .store()
        .entries("atlas")
        .await
        .unwrap()
        .iter()
        .map(|e| e.id)
    {
        kh.retire("atlas", id).await.unwrap();
    }
    assert_eq!(kh.store().agents().await.unwrap(), ["atlas"]);
    kh.forget_identity(A).await.unwrap();
    assert!(kh.store().column_people("atlas").await.unwrap().is_empty());
}

// ── review: names fewer than `min_users` people used reach nobody ───────

/// `who` reads the status and start of orders: one more column than
/// [`QUERY`]'s readers who only count by status.
async fn reads_starts(kh: &KnowHow, who: &str, session: &str) {
    ask(
        kh,
        who,
        session,
        "q",
        "SELECT status, starts_at FROM orders",
        "order statuses and starts",
    )
    .await;
}

fn location(entries: &[Entry]) -> &Entry {
    entries
        .iter()
        .find(|e| e.kind == Kind::DataLocation)
        .expect("a data location")
}

#[tokio::test]
async fn a_pin_never_shows_a_column_only_one_person_read() {
    let (_, kh) = know_how();
    ask(
        &kh,
        A,
        "s1",
        "q",
        "SELECT status FROM orders",
        "order statuses",
    )
    .await;
    reads_starts(&kh, B, "s2").await;
    let id = location(&kh.store().entries("atlas").await.unwrap()).id;
    assert!(kh.pin("atlas", &cfg(), id).await.unwrap());
    let seen = kh
        .visible("atlas", &cfg(), C, &access(), false)
        .await
        .unwrap();
    assert_eq!(location(&seen).columns, ["public.orders.status"]);
    let listed = kh.admin_entries("atlas", &cfg()).await.unwrap();
    let admin = listed.iter().find(|e| e.id == id).unwrap();
    assert_eq!(admin.status, admin::Status::Pinned);
    assert_eq!(admin.columns, ["public.orders.status"], "nor to an admin");
}

#[tokio::test]
async fn admins_never_see_the_names_or_times_of_what_only_one_person_produced() {
    let schemas = Schemas::orders();
    let (_, kh) = know_how_with(Arc::clone(&schemas), Code::app());
    reads_starts(&kh, A, "s1").await;
    corrects(&kh, A, "s2").await;
    reads_code(&kh, A, "s3").await;
    let listed = kh.admin_entries("atlas", &cfg()).await.unwrap();
    assert!(listed.iter().all(|e| e.status == admin::Status::Private));
    assert!(listed
        .iter()
        .all(|e| e.names_hidden && e.last_seen.is_none()));
    let json = serde_json::to_string(&listed).unwrap();
    for name in ["orders", "starts_at", "state", "src/", "fill_rate", SITE] {
        assert!(!json.contains(name), "{name} in {json}");
    }

    schemas.edit(|dbs| dbs.clear());
    schemas.edit(|dbs| {
        dbs.insert(
            3,
            DbSchema {
                tables: vec!["public.other".into()],
                ..Default::default()
            },
        );
    });
    kh.recheck_schemas().await.unwrap();
    let listed = kh.admin_entries("atlas", &cfg()).await.unwrap();
    assert!(listed.iter().any(|e| e.status == admin::Status::Stale));
    assert!(
        listed.iter().all(|e| e.names_hidden),
        "stale is no way round it: {listed:?}"
    );

    schemas.edit(|dbs| *dbs = Schemas::orders().0.lock().clone());
    kh.recheck_schemas().await.unwrap();
    ask(
        &kh,
        B,
        "s4",
        "q",
        "SELECT status FROM orders",
        "order statuses",
    )
    .await;
    let listed = kh.admin_entries("atlas", &cfg()).await.unwrap();
    let shared = listed
        .iter()
        .find(|e| e.status == admin::Status::Shared)
        .expect("the table is shared now");
    assert_eq!(shared.tables, ["public.orders"]);
    assert_eq!(shared.columns, ["public.orders.status"], "{shared:?}");
    assert!(shared.last_seen.is_some());
}

#[tokio::test]
async fn erasing_a_person_takes_the_columns_only_they_read() {
    let (_, kh) = know_how();
    ask(
        &kh,
        A,
        "s1",
        "q",
        "SELECT status FROM orders",
        "order statuses",
    )
    .await;
    reads_starts(&kh, B, "s2").await;
    ask(
        &kh,
        C,
        "s3",
        "q",
        "SELECT status FROM orders",
        "order statuses",
    )
    .await;
    kh.forget_identity(B).await.unwrap();
    let all = kh.store().entries("atlas").await.unwrap();
    assert_eq!(location(&all).columns, ["public.orders.status"]);
    assert!(
        all.iter().all(|e| !format!("{e:?}").contains("starts_at")),
        "{all:?}"
    );
    assert!(!kh
        .store()
        .column_people("atlas")
        .await
        .unwrap()
        .keys()
        .any(|(_, _, c)| c.ends_with("starts_at")));
}

#[tokio::test]
async fn a_file_the_policy_denies_is_shown_to_nobody() {
    let (_, kh) = know_how();
    reads_code(&kh, A, "s1").await;
    reads_code(&kh, B, "s2").await;
    let policy = workforce_policy::engine::PolicyEngine::from_yaml(
        r#"
rules:
  - name: "no-orders-code"
    scope: global
    tool: "repo.read"
    action: allow
    conditions:
      - field: "path"
        operator: equals
        value: "src/orders.rs"
        action: deny
"#,
    )
    .unwrap();
    let denied = Access {
        policy: Some(Arc::new(policy)),
        ..access()
    };
    assert!(kh
        .visible("atlas", &cfg(), C, &denied, false)
        .await
        .unwrap()
        .is_empty());
    assert!(
        kh.visible("atlas", &cfg(), A, &denied, false)
            .await
            .unwrap()
            .is_empty(),
        "not even to whoever produced it"
    );
}

#[tokio::test]
async fn a_recheck_never_undoes_what_a_turn_recorded_meanwhile() {
    let (store, kh) = know_how();
    ask(
        &kh,
        A,
        "s1",
        "q",
        "SELECT status FROM orders",
        "order statuses",
    )
    .await;
    let e = location(&kh.store().entries("atlas").await.unwrap()).clone();
    let checked = chrono::Utc::now();
    reads_starts(&kh, B, "s2").await;
    let r = stale::recheck_data(&e, None).expect("a gone database");
    assert!(store.revise("atlas", &r, checked).await.unwrap());
    let now = location(&kh.store().entries("atlas").await.unwrap()).clone();
    assert!(!now.stale, "produced after the re-check read it");
    assert_eq!(
        now.columns,
        ["public.orders.starts_at", "public.orders.status"],
        "the column the turn added stays"
    );
    assert!(store.revise("atlas", &r, chrono::Utc::now()).await.unwrap());
    assert!(location(&kh.store().entries("atlas").await.unwrap()).stale);
}

#[tokio::test]
async fn a_symbol_moving_onto_a_retired_entry_goes_stale_and_the_pass_carries_on() {
    let code = Code::app();
    let (_, kh) = know_how_with(Schemas::orders(), Arc::clone(&code));
    reads_code(&kh, A, "s1").await;
    code.edit(|files| {
        let symbols = files["src/orders.rs"].clone();
        files.insert("src/fill.rs".into(), symbols);
    });
    let calls = [grep_in("fill.rs"), read("src/fill.rs")];
    turn(&kh, A, "s2", &calls, "how the delivery time is computed").await;
    let all = kh.store().entries("atlas").await.unwrap();
    let there = all
        .iter()
        .find(|e| e.key == "code:acme/app:src/fill.rs#fill_rate")
        .unwrap()
        .id;
    kh.retire("atlas", there).await.unwrap();
    code.edit(|files| {
        files.remove("src/orders.rs");
    });
    kh.recheck_code().await.unwrap();
    let all = kh.store().entries("atlas").await.unwrap();
    let symbol = all
        .iter()
        .find(|e| e.symbol.is_some())
        .expect("the symbol entry");
    assert_eq!(
        (symbol.path.as_deref(), symbol.stale),
        (Some("src/orders.rs"), true),
        "{all:?}"
    );
    assert!(
        all.iter()
            .any(|e| e.path.as_deref() == Some("src/fill.rs") && e.symbol.is_none() && !e.stale),
        "the rest of the pass ran"
    );
}

#[tokio::test]
async fn the_know_how_tool_adds_nothing_on_an_evals_turn() {
    use workforce_agent::runner::{CURRENT_SESSION_KEY, CURRENT_USER_ID};
    use workforce_agent::tools::Tool;
    let (store, kh) = know_how();
    let tool = tool::KnowHowTool::new(
        Arc::new(kh),
        "atlas",
        Arc::new(|| Some(cfg())),
        Arc::new(access),
        Arc::new(|| false),
        Arc::new(|| false),
    );
    let add = || {
        serde_json::json!({
            "action": "add", "database_id": 3, "tables": ["orders"], "columns": ["status"],
        })
    };
    let added_on = |channel: &'static str| {
        let tool = &tool;
        CURRENT_USER_ID.scope(
            A.to_string(),
            CURRENT_SESSION_KEY.scope(channel.to_string(), async move {
                tool.execute(add()).await.unwrap()
            }),
        )
    };

    let refused = added_on("eval:t1").await;
    assert!(!refused.success, "{refused:?}");
    assert!(store.entries("atlas").await.unwrap().is_empty());

    let added = added_on("rest").await;
    assert!(added.success, "{added:?}");
    assert_eq!(store.entries("atlas").await.unwrap().len(), 1);
}

// ── test copies ─────────────────────────────────────────────────────────

/// A test copy of `atlas`, reading `atlas`'s know-how.
const COPY: &str = "atlas-eval";

fn shown() -> KnowHowConfig {
    KnowHowConfig {
        inject: KnowHowInject::On,
        ..cfg()
    }
}

fn reading_atlas() -> KnowHowConfig {
    KnowHowConfig {
        source_agent: Some("atlas".into()),
        ..shown()
    }
}

const ASKED: &str = "order counts by status please";

async fn shared_counts(kh: &KnowHow) {
    ask(kh, A, "s1", "q", QUERY, "order counts by status").await;
    ask(kh, B, "s2", "q", QUERY, "order counts by status").await;
}

#[tokio::test]
async fn a_copy_with_source_agent_sees_the_sources_entries_and_one_without_sees_none() {
    let (_, kh) = know_how();
    shared_counts(&kh).await;
    let without = kh.visible(COPY, &shown(), C, &access(), false).await;
    assert!(
        without.unwrap().is_empty(),
        "a copy's own know-how is empty"
    );
    assert_eq!(
        kh.for_request(COPY, "s3", &shown(), C, ASKED, &access(), false)
            .await,
        None,
        "so know-how on shows it nothing"
    );
    let with = kh
        .visible(COPY, &reading_atlas(), C, &access(), false)
        .await;
    assert_eq!(with.unwrap().len(), 2);
    let block = kh
        .for_request(COPY, "s4", &reading_atlas(), C, ASKED, &access(), false)
        .await
        .expect("shown the source's entries");
    assert_eq!(
        Some(block),
        kh.for_eval("atlas", &shown(), C, ASKED, &access(), false)
            .await,
        "what an eval's turn on the source is shown"
    );
}

#[tokio::test]
async fn on_a_copy_the_asker_sees_their_own_unconfirmed_source_entries_and_nobody_else_does() {
    let (_, kh) = know_how();
    ask(&kh, A, "s1", "q", QUERY, "order counts by status").await;
    let (cfg, access) = (reading_atlas(), access());
    let seen = |who: &'static str| kh.visible(COPY, &cfg, who, &access, false);
    assert_eq!(seen(A).await.unwrap().len(), 2, "A's own, as on the source");
    assert!(
        seen(C).await.unwrap().is_empty(),
        "one person is not enough"
    );
}

#[tokio::test]
async fn a_copy_shows_the_sources_know_how_only_to_someone_the_source_admits() {
    let (_, kh) = know_how();
    shared_counts(&kh).await;
    let admitting = |who: &'static str| Access {
        admits: Some(Arc::new(move |agent: &str, identity: &str| {
            agent == "atlas" && identity == who
        })),
        ..access()
    };
    let kh = &kh;
    let seen = |access: Access| async move {
        kh.visible(COPY, &reading_atlas(), C, &access, false)
            .await
            .unwrap()
            .len()
    };
    assert_eq!(seen(admitting(C)).await, 2);
    assert_eq!(seen(admitting(A)).await, 0, "C may not use the source");
    let unknown = Access {
        admits: None,
        ..access()
    };
    assert_eq!(seen(unknown).await, 0, "unknown counts as not admitted");
    assert_eq!(
        kh.visible("atlas", &cfg(), C, &admitting(A), false)
            .await
            .unwrap()
            .len(),
        2,
        "an agent's own know-how is not gated by it"
    );
}

#[tokio::test]
async fn nothing_is_written_to_the_source_store_from_any_turn_on_a_copy() {
    let (store, kh) = know_how();
    shared_counts(&kh).await;
    reads_code(&kh, A, "s1").await;
    reads_code(&kh, B, "s2").await;
    let entries = store.entries("atlas").await.unwrap();
    let columns = store.column_people("atlas").await.unwrap();
    let events = store.events();
    let agents = store.agents().await.unwrap();
    for (who, session) in [(A, "c1"), (C, "c2")] {
        for cfg in [
            reading_atlas(),
            KnowHowConfig {
                inject: KnowHowInject::Shadow,
                ..reading_atlas()
            },
        ] {
            kh.for_request(COPY, session, &cfg, who, ASKED, &access(), false)
                .await;
            let calls = [sql_call(QUERY, true), grep(), read("src/orders.rs")];
            kh.observe(
                &cfg,
                Turn {
                    agent: COPY,
                    identity: who,
                    shared: false,
                    question: ASKED,
                    invocations: &calls,
                    sites: sites(),
                },
                kh.take_matched(session),
                writes("order counts by status"),
            )
            .await;
        }
    }
    assert_eq!(
        store.entries("atlas").await.unwrap(),
        entries,
        "no confirmation, use or staleness"
    );
    assert_eq!(store.column_people("atlas").await.unwrap(), columns);
    assert_eq!(store.events(), events, "no match recorded or scored");
    assert_eq!(
        store.agents().await.unwrap(),
        agents,
        "nothing under the copy either"
    );
    assert_eq!(
        kh.forget_identity(C).await.unwrap(),
        0,
        "nothing of the copy's askers to erase"
    );
}

#[tokio::test]
async fn a_copy_reads_its_sources_files_under_its_own_policy() {
    let (_, kh) = know_how();
    reads_code(&kh, A, "s1").await;
    reads_code(&kh, B, "s2").await;
    let policy = |action: &str| {
        let yaml = format!(
            r#"
rules:
  - name: "orders-code"
    scope: global
    tool: "repo.read"
    action: allow
    conditions:
      - field: "path"
        operator: equals
        value: "src/orders.rs"
        action: {action}
"#
        );
        Access {
            policy: Some(Arc::new(
                workforce_policy::engine::PolicyEngine::from_yaml(&yaml).unwrap(),
            )),
            ..access()
        }
    };
    let (allowed, denied) = (policy("allow"), policy("deny"));
    let source = kh.visible("atlas", &cfg(), C, &allowed, false).await;
    assert_eq!(code_of(&source.unwrap()).len(), 2);
    let copy = kh.visible(COPY, &reading_atlas(), C, &denied, false).await;
    assert!(
        copy.unwrap().is_empty(),
        "the source's answer is not the copy's"
    );
}

#[tokio::test]
async fn the_know_how_tool_on_a_copy_reads_the_source_and_changes_nothing() {
    use workforce_agent::runner::{CURRENT_SESSION_KEY, CURRENT_USER_ID};
    use workforce_agent::tools::Tool;
    let (store, kh) = know_how();
    shared_counts(&kh).await;
    let entries = store.entries("atlas").await.unwrap();
    let tool = tool::KnowHowTool::new(
        Arc::new(kh),
        COPY,
        Arc::new(|| Some(reading_atlas())),
        Arc::new(access),
        Arc::new(|| true),
        Arc::new(|| false),
    );
    let run = |input: serde_json::Value| {
        let tool = &tool;
        CURRENT_USER_ID.scope(
            C.to_string(),
            CURRENT_SESSION_KEY.scope("rest".to_string(), async move {
                tool.execute(input).await.unwrap()
            }),
        )
    };
    let listed = run(serde_json::json!({ "action": "list" })).await;
    assert!(listed.success, "{listed:?}");
    assert_eq!(listed.output["count"], 2);
    let id = entries[0].id;
    for input in [
        serde_json::json!({ "action": "add", "database_id": 3, "tables": ["orders"] }),
        serde_json::json!({ "action": "pending" }),
        serde_json::json!({ "action": "pin", "id": id }),
        serde_json::json!({ "action": "remove", "id": id }),
    ] {
        let refused = run(input.clone()).await;
        assert!(!refused.success, "{input}: {refused:?}");
    }
    assert_eq!(store.entries("atlas").await.unwrap(), entries);
    assert!(store.entries(COPY).await.unwrap().is_empty());
}

#[tokio::test]
async fn the_pre_check_counts_what_each_question_would_be_shown() {
    let (_, kh) = know_how();
    shared_counts(&kh).await;
    let questions = [ASKED.to_string(), "invoice totals per customer".to_string()];
    let preview = kh
        .preview(COPY, None, Some(&reading_atlas()), C, &access(), &questions)
        .await
        .unwrap();
    assert_eq!(
        preview,
        preview::Preview {
            enabled: false,
            shown: false,
            matching: true,
            source_agent: Some("atlas".into()),
            min_score: 0.8,
            visible: 2,
            matched: vec![2, 0],
        }
    );
    let own = kh
        .preview(
            COPY,
            Some(&shown()),
            Some(&shown()),
            C,
            &access(),
            &questions,
        )
        .await
        .unwrap();
    assert_eq!((own.shown, own.visible, own.matched), (true, 0, vec![0, 0]));
    let off = kh
        .preview(COPY, None, None, C, &access(), &questions)
        .await
        .unwrap();
    assert_eq!(
        (off.enabled, off.visible, off.matched),
        (false, 0, vec![0, 0])
    );
}

#[tokio::test]
async fn reassigning_a_person_in_memory_counts_them_once() {
    crate::know_how::store::reassign_person_contract(&InMemoryKnowHowStore::default()).await;
}
