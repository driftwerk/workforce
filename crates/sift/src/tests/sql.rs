use super::{at, exec, failure, rejection, value, Mock, SqlCall};
use crate::code;
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn holes_become_numbered_placeholders_with_values() {
    let mock = Arc::new(Mock::new().sql(|_, _, _| Ok(json!([{ "n": 1 }]))));
    let source = r#"let org = { id: 7, db: 3 }
let since = "2026-01-01"
let rows = sql(org.db) {
  SELECT '{literal}' AS "a}b", count(*) AS n -- it's { fine }
  FROM t WHERE org_id = ${org.id} AND at >= ${since} AND tag IN (${["x", "y"]}) AND id = ${org.id}
}
return rows"#;
    assert_eq!(
        value(exec(&mock, source).await, source),
        json!([{ "n": 1 }])
    );
    assert_eq!(
        mock.sql_calls(),
        [SqlCall {
            db: json!(3),
            query: "\n  SELECT '{literal}' AS \"a}b\", count(*) AS n -- it's { fine }\n  FROM t WHERE org_id = {{p1}} AND at >= {{p2}} AND tag IN ({{p3}}) AND id = {{p4}}\n".to_string(),
            params: vec![
                ("p1".to_string(), json!(7)),
                ("p2".to_string(), json!("2026-01-01")),
                ("p3".to_string(), json!(["x", "y"])),
                ("p4".to_string(), json!(7)),
            ],
        }]
    );
}

#[tokio::test]
async fn database_may_be_any_value_and_blocks_nest_braces() {
    let mock = Arc::new(Mock::new());
    let source = "let db = { id: 2, engine: \"postgres\" }\nreturn sql(db) { SELECT json_build_object('k', 1) -> '{a}' WHERE x = ${ { a: 1 }.a } }";
    assert_eq!(value(exec(&mock, source).await, source), json!([]));
    let calls = mock.sql_calls();
    assert_eq!(calls[0].db, json!({ "id": 2, "engine": "postgres" }));
    assert_eq!(
        calls[0].query,
        " SELECT json_build_object('k', 1) -> '{a}' WHERE x = {{p1}} "
    );
    assert_eq!(calls[0].params, vec![("p1".to_string(), json!(1))]);
}

#[tokio::test]
async fn host_errors_fail_the_run_at_the_block() {
    let mock =
        Arc::new(Mock::new().sql(|_, _, _| Err("column \"nme\" does not exist".to_string())));
    let source = "let a = 1\nlet rows = sql(1) { SELECT nme FROM t }\nreturn rows";
    let outcome = exec(&mock, source).await;
    let rendered = outcome.render(source);
    let e = failure(outcome, source);
    assert_eq!((e.code, at(&e)), (code::SQL, (2, 12)));
    assert_eq!(
        e.message,
        "the SQL query failed: column \"nme\" does not exist"
    );
    assert!(
        rendered.ends_with("ran 1 call: sql; failed: sql"),
        "{rendered}"
    );
}

#[tokio::test]
async fn quoted_holes_are_rejected_before_running() {
    let mock = Arc::new(Mock::new());
    let source = "let name = \"x\"\nreturn sql(1) { SELECT 1 WHERE name = '${name}' }";
    let diagnostics = rejection(exec(&mock, source).await, source);
    assert_eq!(diagnostics[0].code, code::SQL_QUOTED_HOLE);
    assert_eq!(at(&diagnostics[0]), (2, 40));
    assert!(mock.sql_calls().is_empty());
}

#[tokio::test]
async fn functions_cannot_be_parameters() {
    let mock = Arc::new(Mock::new());
    let source = "let f = x => x\nreturn sql(1) { SELECT ${f} }";
    let e = failure(exec(&mock, source).await, source);
    assert_eq!((e.code, at(&e)), (code::ARGUMENT, (2, 26)));
    assert_eq!(e.message, "a parameter of `sql` is a function, not data");
    assert!(mock.sql_calls().is_empty());
}
