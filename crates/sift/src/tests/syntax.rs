use super::{at, eval, exec, fail, rejection, Mock};
use crate::code;
use serde_json::json;
use std::sync::Arc;

#[tokio::test]
async fn literals_lists_and_records() {
    assert_eq!(
        eval(r#"return [null, true, false, 42, -7, 2.5, "s", [], {}]"#).await,
        json!([null, true, false, 42, -7, 2.5, "s", [], {}])
    );
    assert_eq!(
        eval(r#"let d = 4; return { a: 1, "b c": 2, d, in: 3 }"#).await,
        json!({ "a": 1, "b c": 2, "d": 4, "in": 3 })
    );
}

#[tokio::test]
async fn string_escapes_and_interpolation() {
    assert_eq!(
        eval(r#"return "q\"b\\s\n\t\${x}""#).await,
        json!("q\"b\\s\n\t${x}")
    );
    assert_eq!(
        eval(
            r#"let n = 3; let r = { name: "Ann", tags: ["a"] }
return "${r.name} has ${n + 1} (${1.5}) ${r.tags} ${null} ${true} ${ r["name"] }""#
        )
        .await,
        json!("Ann has 4 (1.5) [\"a\"] null true Ann")
    );
}

#[tokio::test]
async fn member_index_and_null_safety() {
    let program = |expr: &str| {
        format!(
            r#"let r = {{ a: {{ b: [10, 20] }}, n: null, "k": 1 }}; let xs = [1, 2, 3]; return {expr}"#
        )
    };
    assert_eq!(eval(&program("r.a.b[1]")).await, json!(20));
    assert_eq!(eval(&program(r#"r["k"]"#)).await, json!(1));
    assert_eq!(eval(&program("r.n?.x")).await, json!(null));
    assert_eq!(eval(&program("r?.missing")).await, json!(null));
    assert_eq!(eval(&program("r.n?.x.y ?? 5")).await, json!(5));
    assert_eq!(eval(&program("r.n?.map(x => x)")).await, json!(null));
    assert_eq!(eval(&program("r.a ?? 1")).await, json!({ "b": [10, 20] }));
    assert_eq!(eval(&program("xs.length + \"abc\".length")).await, json!(6));

    let e = fail(&program("r.n.x")).await;
    assert_eq!(
        (e.code, e.message.as_str()),
        (code::NULL, "cannot read `.x` of null")
    );
    assert_eq!(e.fix.as_deref(), Some("?.x"));
    let e = fail(&program("r.nn")).await;
    assert_eq!(e.code, code::FIELD);
    assert_eq!(
        e.message,
        "the record has no field `nn`; its fields are: a, n, k"
    );
    assert_eq!(e.fix.as_deref(), Some(".n"));
    let e = fail(&program("xs[3]")).await;
    assert_eq!(
        e.message,
        "index 3 is out of range for a list of 3 elements"
    );
    assert_eq!(fail(&program("xs[-1]")).await.code, code::INDEX);
    assert_eq!(fail(&program(r#"xs["a"]"#)).await.code, code::TYPE);
    assert_eq!(fail(&program("r[0]")).await.code, code::TYPE);
    assert_eq!(
        fail(&program("xs.name")).await.fix.as_deref(),
        Some("xs.map(x => x.name)")
    );
}

#[tokio::test]
async fn arithmetic_and_promotion() {
    assert_eq!(
        eval("return [1 + 2 * 3, (1 + 2) * 3, 7 / 2, -7 / 2, 7 % 3, 1 + 0.5, 2 * 1.5, 1.0 / 4]")
            .await,
        json!([7, 9, 3.5, -3.5, 1, 1.5, 3.0, 0.25])
    );
    assert_eq!(
        eval(r#"return ["a" + "b", [1] + [2, 3]]"#).await,
        json!(["ab", [1, 2, 3]])
    );
    let e = fail("return 1 / 0").await;
    assert_eq!((e.code, at(&e)), (code::ARITHMETIC, (1, 10)));
    assert_eq!(fail("return 1.5 % 0").await.code, code::ARITHMETIC);
    assert_eq!(
        fail("return 9223372036854775807 + 1").await.code,
        code::ARITHMETIC
    );
    assert_eq!(
        fail("let m = -9223372036854775807 - 1\nreturn -m")
            .await
            .code,
        code::ARITHMETIC
    );
    assert_eq!(
        fail("let m = -9223372036854775807 - 1\nreturn m % -1")
            .await
            .code,
        code::ARITHMETIC
    );
    assert_eq!(fail("return 1e308 * 10").await.code, code::ARITHMETIC);
}

#[tokio::test]
async fn no_implicit_conversions() {
    let e = fail("let a = 1\nreturn a + \"a\"").await;
    assert_eq!((e.code, at(&e)), (code::TYPE, (2, 10)));
    assert_eq!(
        e.message,
        "`+` needs two numbers, two strings or two lists; got int 1 and string \"a\""
    );
    assert!(e.help.is_some_and(|h| h.contains("str(x)")));
    assert_eq!(fail("return [1] + 1").await.code, code::TYPE);
    assert_eq!(fail("return \"a\" * 2").await.code, code::TYPE);
    assert_eq!(fail("return -\"a\"").await.code, code::TYPE);
    assert_eq!(fail("return !1").await.code, code::TYPE);
    assert_eq!(fail("if 1 { return 1 }\nreturn 0").await.code, code::TYPE);
    assert_eq!(fail("return 1 && true").await.code, code::TYPE);
    assert_eq!(fail("return false || 1").await.code, code::TYPE);
}

#[tokio::test]
async fn comparisons_and_structural_equality() {
    assert_eq!(
        eval(r#"return [1 < 2, 2 <= 2, 3 > 4, 4 >= 4, "a" < "b", 1 < 1.5, 1 == 1.0, 1 != 2, "1" == 1, null == null, null == false]"#).await,
        json!([true, true, false, true, true, true, true, true, false, true, false])
    );
    assert_eq!(
        eval(r#"return [{ a: [1, { b: 2 }], c: null } == { c: null, a: [1, { b: 2.0 }] }, [1, 2] == [2, 1], {} == { a: null }]"#).await,
        json!([true, false, false])
    );
    let e = fail(r#"return 1 < "2""#).await;
    assert_eq!(
        e.message,
        "`<` needs two numbers or two strings; got int 1 and string \"2\""
    );
    assert_eq!(fail("return true < false").await.code, code::TYPE);
    assert_eq!(fail("return [1] < [2]").await.code, code::TYPE);
}

#[tokio::test]
async fn logic_short_circuits() {
    assert_eq!(
        eval("return [false && nope(), true || nope(), true && false, false || true, 1 ?? nope()]")
            .await,
        json!([false, true, false, true, 1])
    );
    assert_eq!(fail("return null ?? nope()").await.code, code::UNKNOWN_NAME);
}

#[tokio::test]
async fn if_else_chains() {
    let program = |n: i64| {
        format!(
            "let n = {n}\nlet out = \"none\"\nif n < 0 {{ return \"neg\" }} else if n == 0 {{ return \"zero\" }} else if n < 10 {{ let small = true\n return \"small\" }} else {{ let big = n }}\nreturn out"
        )
    };
    assert_eq!(eval(&program(-1)).await, json!("neg"));
    assert_eq!(eval(&program(0)).await, json!("zero"));
    assert_eq!(eval(&program(5)).await, json!("small"));
    assert_eq!(eval(&program(50)).await, json!("none"));
}

#[tokio::test]
async fn for_loops_and_early_return() {
    assert_eq!(
        eval("for x in [1, 2, 3] { if x == 2 { return x * 10 } }\nreturn 0").await,
        json!(20)
    );
    assert_eq!(eval("for x in [] { return 1 }\nreturn 0").await, json!(0));
    let e = fail("let r = { a: 1 }\nfor x in r { }\nreturn 0").await;
    assert_eq!((e.code, at(&e)), (code::TYPE, (2, 10)));
    assert_eq!(e.message, "`for` needs a list; got record { \"a\": 1 }");
}

#[tokio::test]
async fn shadowing_in_inner_blocks() {
    assert_eq!(
        eval("let a = 1\nfor x in [1] { let a = 2 }\nif true { let a = 3 }\nlet f = a => a + 1\nreturn [a, f(10)]")
            .await,
        json!([1, 11])
    );
}

#[tokio::test]
async fn match_expressions() {
    let program = |v: &str| {
        format!(
            r#"return match {v} {{ "a" | "b" => "letter", 1 | -1 => "one", true => "yes", _ => "other" }}"#
        )
    };
    assert_eq!(eval(&program(r#""b""#)).await, json!("letter"));
    assert_eq!(eval(&program("-1")).await, json!("one"));
    assert_eq!(eval(&program("true")).await, json!("yes"));
    assert_eq!(eval(&program(r#""z""#)).await, json!("other"));
    assert_eq!(eval(&program("false")).await, json!("other"));
    let e = fail(&program("1.5")).await;
    assert_eq!((e.code, at(&e)), (code::TYPE, (1, 14)));
    assert_eq!(fail(&program("null")).await.code, code::TYPE);
}

#[tokio::test]
async fn lambdas_and_local_calls() {
    assert_eq!(
        eval("let k = 10\nlet add = (a, b) => a + b + k\nlet one = () => 1\nreturn [add(1, 2), one(), [1, 2].map(x => add(x, x))]").await,
        json!([13, 1, [12, 14]])
    );
    let e = fail("let f = x => x\nreturn f(1, 2)").await;
    assert_eq!(e.code, code::ARITY);
    assert_eq!(
        e.message,
        "`f` calls its function with 2 arguments, but the function takes 1"
    );
    let e = fail("let n = 1\nreturn n(2)").await;
    assert_eq!(e.message, "`n` is int 1, not a function");
    let e = fail("return [x => x]").await;
    assert_eq!(e.code, code::TYPE);
    assert_eq!(e.message, "the return value contains a function");
}

#[tokio::test]
async fn names() {
    let e = fail("let total = 1\nreturn totl").await;
    assert_eq!((e.code, at(&e)), (code::UNKNOWN_NAME, (2, 8)));
    assert_eq!(e.help.as_deref(), Some("did you mean `total`?"));
    let e = fail("return count").await;
    assert_eq!(e.message, "`count` is a function and must be called");
    assert_eq!(e.fix.as_deref(), Some("count(...)"));
    assert_eq!(eval("let count = 3\nreturn count").await, json!(3));
}

#[tokio::test]
async fn accepted_deviations_run() {
    assert_eq!(
        eval("const a = 1;\nvar b = a === 1;\nlet c = a !== 1;\nreturn [a, b, c];").await,
        json!([1, true, false])
    );
}

#[tokio::test]
async fn cond_picks_a_branch_and_nests_to_the_right() {
    let band = |n: i64| format!(r#"let n = {n}; return n > 999 ? "high" : n > 99 ? "mid" : "low""#);
    assert_eq!(eval(&band(1000)).await, json!("high"));
    assert_eq!(eval(&band(100)).await, json!("mid"));
    assert_eq!(eval(&band(1)).await, json!("low"));
    assert_eq!(
        eval("return [1, 2].map(x => x == 1 ? x ?? 0 : -x)").await,
        json!([1, -2])
    );
    let e = fail(r#"return 1 ? "a" : "b""#).await;
    assert_eq!((e.code, at(&e)), (code::TYPE, (1, 8)));
}

#[tokio::test]
async fn a_spread_copies_fields_that_later_entries_override() {
    assert_eq!(
        eval(r#"let r = { a: 1, b: 2 }; return { z: 0, ...r, b: 3, ...null }"#).await,
        json!({ "z": 0, "a": 1, "b": 3 })
    );
    assert_eq!(
        eval(r#"let r = { a: 1, b: 2 }; return { b: 9, ...r }"#).await,
        json!({ "b": 2, "a": 1 })
    );
    let e = fail("let xs = [1]; return { ...xs }").await;
    assert_eq!((e.code, at(&e)), (code::TYPE, (1, 27)));
}

#[tokio::test]
async fn a_block_bodied_function_returns_its_value() {
    assert_eq!(
        eval(
            r#"let orgs = [{ a: 2, b: 1 }, { a: 20, b: 5 }]
let scored = orgs.map(o => {
  let score = o.a * 9 + o.b
  if score > 99 { return { ...o, score, band: "high" } }
  return { ...o, score, band: score > 20 ? "mid" : "low" }
})
return scored"#
        )
        .await,
        json!([
            { "a": 2, "b": 1, "score": 19, "band": "low" },
            { "a": 20, "b": 5, "score": 185, "band": "high" }
        ])
    );
    let source = "let f = x => { if x { return 1 } }\nreturn f(false)";
    let rejected = rejection(exec(&Arc::new(Mock::new()), source).await, source);
    assert_eq!(
        rejected.iter().map(|d| (d.code, at(d))).collect::<Vec<_>>(),
        [(code::RETURN_MISSING, (1, 34))]
    );
}
