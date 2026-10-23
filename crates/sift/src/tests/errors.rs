use super::{at, exec, failure, rejection, value, Mock};
use crate::code;
use serde_json::json;
use std::sync::Arc;

fn tools() -> Mock {
    Mock::new()
        .tool("jira_search", |_| Ok(json!({ "issues": [] })))
        .tool("jira_get_issue", |_| Err("403: not allowed".to_string()))
        .tool("metabase_dataset_query_native", |_| Ok(json!([])))
        .hidden("jira_delete_issue")
}

#[tokio::test]
async fn unknown_tool_lists_the_closest_names() {
    let mock = Arc::new(tools());
    let source = "let a = 1\nreturn jira_serch({ jql: \"x\" })";
    let e = failure(exec(&mock, source).await, source);
    assert_eq!((e.code, at(&e)), (code::UNKNOWN_NAME, (2, 8)));
    assert_eq!(e.message, "unknown function `jira_serch`");
    let help = e.help.unwrap_or_default();
    assert!(
        help.starts_with("did you mean one of: `jira_search`"),
        "{help}"
    );
    assert_eq!(help.matches('`').count(), 10, "{help}");
    assert!(!help.contains("jira_delete_issue"));
}

#[tokio::test]
async fn a_tool_the_host_does_not_allow_is_unknown() {
    let mock = Arc::new(tools());
    let source = "return jira_delete_issue({ key: \"A-1\" })";
    let e = failure(exec(&mock, source).await, source);
    assert_eq!(e.code, code::UNKNOWN_NAME);
    assert!(mock.tool_calls().is_empty());
}

#[tokio::test]
async fn tool_failure_names_the_tool_and_host_message() {
    let mock = Arc::new(tools());
    let source = "let found = jira_search({ jql: \"project = A\" })\nlet issue = jira_get_issue({ key: \"A-1\" })\nreturn issue";
    let outcome = exec(&mock, source).await;
    assert_eq!(
        outcome.render(source),
        "error[E-TOOL] line 2:13: `jira_get_issue` failed: 403: not allowed\n\
         2 | let issue = jira_get_issue({ key: \"A-1\" })\n  |             ^\n\
         ran 2 calls: jira_search, jira_get_issue; failed: jira_get_issue"
    );
    assert_eq!(
        mock.tool_calls(),
        [
            ("jira_search".to_string(), json!({ "jql": "project = A" })),
            ("jira_get_issue".to_string(), json!({ "key": "A-1" })),
        ]
    );
}

#[tokio::test]
async fn tools_take_one_record() {
    let mock = Arc::new(tools());
    for (source, got) in [
        ("return jira_search(\"x\")", "string \"x\""),
        ("return jira_search()", "0 arguments"),
        ("return jira_search({}, {})", "2 arguments"),
    ] {
        let e = failure(exec(&mock, source).await, source);
        assert_eq!(
            e.message,
            format!("`jira_search` takes one record of arguments; got {got}")
        );
        assert_eq!(e.fix.as_deref(), Some("jira_search({ ... })"));
    }
    let source = "return jira_search({ f: x => x })";
    let e = failure(exec(&mock, source).await, source);
    assert_eq!(e.message, "`jira_search`'s arguments contain a function");
    assert!(mock.tool_calls().is_empty());
}

#[tokio::test]
async fn parse_errors_reject_before_anything_runs() {
    let mock = Arc::new(tools());
    let source = "let a = jira_search({ jql: \"x\" })\nlet b = [1, 2\nlet c = 'q'\nif a { let d = 1 }\nreturn { a b }";
    let diagnostics = rejection(exec(&mock, source).await, source);
    let found: Vec<_> = diagnostics.iter().map(|d| (d.code, at(d))).collect();
    assert_eq!(
        found,
        [
            (code::SYNTAX, (3, 1)),
            (code::LEX, (3, 9)),
            (code::SYNTAX, (5, 12))
        ]
    );
    assert!(mock.tool_calls().is_empty());
}

#[tokio::test]
async fn static_rules_reject_before_anything_runs() {
    let mock = Arc::new(tools());
    for (source, expected) in [
        ("let a = jira_search({})\nlet b = a", code::RETURN_MISSING),
        (
            "let a = jira_search({})\nreturn a\nlet b = 1",
            code::RETURN_NOT_LAST,
        ),
        (
            "let a = jira_search({})\nlet a = 2\nreturn a",
            code::LET_REBIND,
        ),
    ] {
        let diagnostics = rejection(exec(&mock, source).await, source);
        assert_eq!(diagnostics[0].code, expected, "{source}");
    }
    assert!(mock.tool_calls().is_empty());
}

#[tokio::test]
async fn rejected_render() {
    let mock = Arc::new(Mock::new());
    let source = "let x 1\nlet y = [1,\n  2 3]\nreturn x";
    let outcome = exec(&mock, source).await;
    assert_eq!(
        outcome.render(source),
        "error[E-SYNTAX] line 1:7: expected `=` after `let x`, found number `1`\n\
         1 | let x 1\n  |       ^\n\
         fix: let x = <value>\n\
         error[E-SYNTAX] line 3:5: expected `,` or `]` in the list opened at line 2:9, found number `3`\n\
         3 |   2 3]\n  |     ^\n"
    );
}

#[tokio::test]
async fn returned_render_is_pretty_json() {
    let mock = Arc::new(Mock::new());
    let source = "return { n: 1, xs: [true] }";
    let outcome = exec(&mock, source).await;
    assert_eq!(
        outcome.render(source),
        "{\n  \"n\": 1,\n  \"xs\": [\n    true\n  ]\n}"
    );
    assert_eq!(value(outcome, source), json!({ "n": 1, "xs": [true] }));
}
