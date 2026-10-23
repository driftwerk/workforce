use super::{at, exec, exec_with, failure, value, Mock};
use crate::{code, Limits};
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn a_program_reads_a_stored_table_and_works_on_its_rows() {
    let mock = Arc::new(Mock::new().artifact(
        "art_1",
        json!([{ "org": "a", "n": 2 }, { "org": "b", "n": 5 }, { "org": "a", "n": 1 }]),
    ));
    let source = r#"let rows = artifact_read("art_1")
let by_org = group_by(rows, r => r.org).map(g => ({ org: g.key, n: sum(g.items.map(r => r.n)) }))
return sort_by_desc(by_org, g => g.n)"#;
    assert_eq!(
        value(exec(&mock, source).await, source),
        json!([{ "org": "b", "n": 5 }, { "org": "a", "n": 3 }])
    );
}

#[tokio::test]
async fn a_program_writes_rows_to_a_file_and_returns_only_its_summary() {
    let mock = Arc::new(Mock::new());
    let source = r#"let rows = [{ "a": 1 }, { "a": 2 }]
let file = artifact_write("export.xlsx", rows)
return { file: file.filename, rows: file.rows }"#;
    assert_eq!(
        value(exec(&mock, source).await, source),
        json!({ "file": "export.xlsx", "rows": 2 })
    );
    let written = mock.written.lock().unwrap().clone();
    assert_eq!(
        written,
        [("export.xlsx".to_string(), json!([{ "a": 1 }, { "a": 2 }]))]
    );
}

#[tokio::test]
async fn artifact_calls_say_what_is_wrong() {
    let mock = Arc::new(Mock::new());
    let missing = "return artifact_read(\"art_nope\")";
    let e = failure(exec(&mock, missing).await, missing);
    assert_eq!((e.code, at(&e)), (code::TOOL, (1, 8)));
    assert!(e.message.contains("not found"), "{}", e.message);

    let not_text = "return artifact_read(7)";
    let e = failure(exec(&mock, not_text).await, not_text);
    assert_eq!(e.code, code::ARGUMENT);

    let not_rows = "return artifact_write(\"x.csv\", { a: 1 })";
    let e = failure(exec(&mock, not_rows).await, not_rows);
    assert!(e.message.contains("needs a list of rows"), "{}", e.message);
}

#[tokio::test]
async fn artifact_calls_count_against_the_call_budget() {
    let mock = Arc::new(Mock::new().artifact("art_1", json!([])));
    let source = "let a = artifact_read(\"art_1\")\nlet b = artifact_read(\"art_1\")\nreturn 1";
    let limits = Limits {
        max_calls: 1,
        ..Limits::default()
    };
    let e = failure(exec_with(&mock, source, limits).await, source);
    assert_eq!(e.code, code::BUDGET_CALLS);
}
