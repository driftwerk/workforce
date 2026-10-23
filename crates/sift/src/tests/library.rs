use super::{at, eval, fail};
use crate::code;
use serde_json::json;

const ROWS: &str = r#"let rows = [
  { team: "a", n: 3, name: "x" },
  { team: "b", n: 1, name: "y" },
  { team: "a", n: 2, name: "z" }
]
"#;

async fn with_rows(expr: &str) -> serde_json::Value {
    eval(&format!("{ROWS}return {expr}")).await
}

#[tokio::test]
async fn list_methods() {
    assert_eq!(with_rows("rows.map(r => r.n)").await, json!([3, 1, 2]));
    assert_eq!(
        with_rows("rows.filter(r => r.n > 1).map(r => r.name)").await,
        json!(["x", "z"])
    );
    assert_eq!(
        with_rows("rows.flat_map(r => [r.n, r.n])").await,
        json!([3, 3, 1, 1, 2, 2])
    );
    assert_eq!(
        with_rows("rows.find(r => r.team == \"b\").name").await,
        json!("y")
    );
    assert_eq!(with_rows("rows.find(r => r.n > 5)").await, json!(null));
    assert_eq!(
        with_rows("[rows.any(r => r.n > 2), rows.every(r => r.n > 2), [].every(r => false)]").await,
        json!([true, false, true])
    );
    assert_eq!(
        with_rows("[rows.take(2).length, rows.take(0), rows.length]").await,
        json!([2, [], 3])
    );
}

#[tokio::test]
async fn grouping_sorting_joining() {
    assert_eq!(
        with_rows("group_by(rows, r => r.team).map(g => { team: g.key, total: sum(g.items.map(r => r.n)), count: count(g.items) })").await,
        json!([{ "team": "a", "total": 5, "count": 2 }, { "team": "b", "total": 1, "count": 1 }])
    );
    assert_eq!(
        with_rows("sort_by(rows, r => r.n).map(r => r.name)").await,
        json!(["y", "z", "x"])
    );
    assert_eq!(
        with_rows("sort_by_desc(rows, r => r.name).map(r => r.name)").await,
        json!(["z", "y", "x"])
    );
    assert_eq!(
        with_rows(r#"join(rows, [{ id: "a", label: "Alpha" }], r => r.team, t => t.id).map(p => "${p.left.name}:${p.right.label}")"#).await,
        json!(["x:Alpha", "z:Alpha"])
    );
    assert_eq!(with_rows("table(rows).length").await, json!(3));
}

#[tokio::test]
async fn scalar_functions() {
    assert_eq!(
        eval(
            r#"let r = { a: 1, b: 2 }
return [one([5]), first([]), count([1, 2]), sum([1, 2.5]), min([3, 1]), max(["a", "c"]),
  len(keys(r)), values(r), len("héllo"), str(12), int("42"), int(3.7), float(2), round(2.456, 1),
  contains("hello", "ell"), contains([1, 2], 3), lower("A"), upper("b"), split("a-b", "-"),
  join_str(["a", "b"], ", ")]"#
        )
        .await,
        json!([
            5,
            null,
            2,
            3.5,
            1,
            "c",
            2,
            [1, 2],
            5,
            "12",
            42,
            3,
            2.0,
            2.5,
            true,
            false,
            "a",
            "B",
            ["a", "b"],
            "a, b"
        ])
    );
}

#[tokio::test]
async fn library_errors_name_the_function_input_and_need() {
    let e = fail("let found = [1, 2]\nreturn one(found)").await;
    assert_eq!((e.code, at(&e)), (code::ARGUMENT, (2, 8)));
    assert_eq!(
        e.message,
        "`one` needs exactly one element; got a list of 2 elements: [1, 2]"
    );
    let e = fail(r#"return sum([1, "2"])"#).await;
    assert_eq!(
        e.message,
        "`sum` needs a list of numbers; element 1 is string \"2\""
    );
    assert!(e.help.is_some_and(|h| h.contains("float(x)")));
    let e = fail(r#"return int("x")"#).await;
    assert_eq!(
        e.message,
        "`int` needs a string holding an integer; got string \"x\""
    );
    let e = fail("return group_by([{ a: 1 }], r => r)").await;
    assert!(e.message.starts_with("`group_by`'s key function must return a string, number, bool or null; for element 0 it returned record"), "{}", e.message);
    let e = fail("return sort_by([1, 2], 3)").await;
    assert_eq!(
        e.message,
        "`sort_by` needs a function like `x => ...`; got int 3"
    );
    let e = fail("return group_by(\"ab\", x => x)").await;
    assert_eq!(e.message, "`group_by` needs a list; got string \"ab\"");
}

#[tokio::test]
async fn arity_and_callback_shape() {
    let e = fail("return count([1], 2)").await;
    assert_eq!(
        (e.code, e.message.as_str()),
        (code::ARITY, "`count` takes 1 argument; got 2")
    );
    assert_eq!(e.help.as_deref(), Some("usage: count(xs)"));
    let e = fail("return [1].map((a, b) => a)").await;
    assert_eq!(
        e.message,
        "`.map` calls its function with 1 argument, but the function takes 2"
    );
    let e = fail("return [1, 2].filter(x => x)").await;
    assert_eq!(
        e.message,
        "`.filter`'s function must return a bool; for element 0 it returned int 1"
    );
    let e = fail("return [1].flat_map(x => x)").await;
    assert!(e
        .message
        .starts_with("`.flat_map`'s function must return a list"));
    let e = fail("return [1].take(1, 2)").await;
    assert_eq!(e.help.as_deref(), Some("usage: xs.take(n)"));
    assert_eq!(fail("return [1].take(-1)").await.code, code::ARGUMENT);
}

#[tokio::test]
async fn method_mistakes() {
    let e = fail("return [1].mpa(x => x)").await;
    assert_eq!((e.code, at(&e)), (code::UNKNOWN_NAME, (1, 12)));
    assert_eq!(e.message, "list has no method `.mpa`");
    assert!(e
        .help
        .is_some_and(|h| h.starts_with("did you mean one of: `map`")));
    let e = fail("return [2, 1].sort_by(x => x)").await;
    assert_eq!(e.fix.as_deref(), Some("sort_by(xs, x => key)"));
    let e = fail("return [1].length()").await;
    assert_eq!(e.fix.as_deref(), Some(".length"));
    let e = fail("let r = { a: 1 }\nreturn r.map(x => x)").await;
    assert_eq!(e.message, "`.map` needs a list; got record { \"a\": 1 }");
    let e = fail("let n = null\nreturn n.map(x => x)").await;
    assert_eq!((e.code, e.fix.as_deref()), (code::NULL, Some("?.map(...)")));
}
