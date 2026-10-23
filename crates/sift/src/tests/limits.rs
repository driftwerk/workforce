//! Programs built to exhaust the process rather than a budget: deep
//! nesting, values that repeat a shared part, and a join's product.

use super::{exec, exec_with, failure, value, Mock};
use crate::{code, Limits, Outcome};
use serde_json::json;
use std::sync::Arc;

/// Lambdas that each call the one before, `levels` deep, and `wrap` lists
/// deep around each call.
fn nested_lambdas(levels: usize, wrap: usize) -> String {
    let mut source = String::from("let f0 = x => x\n");
    for i in 1..=levels {
        let call = format!("f{}(x)", i - 1);
        source.push_str(&format!(
            "let f{i} = x => {}{call}{}\n",
            "[".repeat(wrap),
            "]".repeat(wrap)
        ));
    }
    source.push_str(&format!("return f{levels}(1)"));
    source
}

/// Run on a spawned task of a multi-thread runtime, as the hub runs turns.
async fn spawned(source: String) -> Outcome {
    let mock = Arc::new(Mock::new());
    tokio::spawn(async move { exec(&mock, &source).await })
        .await
        .expect("the run task panicked")
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn deep_lambda_nesting_fails_instead_of_overflowing() {
    for source in [nested_lambdas(3000, 0), nested_lambdas(10, 85)] {
        let outcome = spawned(source.clone()).await;
        let e = failure(outcome, &source);
        assert_eq!(e.code, code::NESTING, "{e:?}");
        assert!(e.message.starts_with("evaluation nests too deeply here"));
    }
    let source = nested_lambdas(20, 1);
    let expected = (0..20).fold(json!(1), |inner, _| json!([inner]));
    assert_eq!(value(spawned(source.clone()).await, &source), expected);
}

/// `a0 = [0]`, `a1 = [a0, a0]`, … : `a{k}` has 3 * 2^k - 1 parts.
fn doubling(levels: usize) -> String {
    let mut source = String::from("let a0 = [0]\n");
    for i in 1..=levels {
        source.push_str(&format!("let a{i} = [a{0}, a{0}]\n", i - 1));
    }
    source
}

#[tokio::test]
async fn shared_parts_cannot_build_a_huge_value() {
    let source = format!("{}return 0", doubling(40));
    let e = failure(exec(&Arc::new(Mock::new()), &source).await, &source);
    assert_eq!((e.code, e.line), (code::VALUE_SIZE, 20));
    assert_eq!(
        e.message,
        "this value has 1572863 parts, counting each repeat of a shared part; the limit is 1000000"
    );
}

#[tokio::test]
async fn walking_a_large_shared_value_costs_its_size() {
    let mock = Arc::new(Mock::new().tool("record", |_| Ok(json!(null))));
    for sink in [
        "\"${a18}\"",
        "a18 == a18",
        "record({ x: a18 })",
        "str(a18)",
        "contains(a18, 1)",
    ] {
        let source = format!("{}return {sink}", doubling(18));
        let e = failure(exec(&mock, &source).await, &source);
        assert_eq!((e.code, e.line), (code::BUDGET_STEPS, 20), "{sink}");
    }
    assert!(mock.tool_calls().is_empty());

    let source = format!("{}return a18", doubling(18));
    let e = failure(exec(&mock, &source).await, &source);
    assert_eq!(e.code, code::BUDGET_RETURN);
    assert_eq!(
        e.message,
        "the return value is at least 786431 bytes; the limit is 60000"
    );
}

#[tokio::test]
async fn values_cannot_nest_without_bound() {
    let mut source = String::from("let v0 = 1\n");
    for i in 1..=250 {
        source.push_str(&format!("let v{i} = [v{}]\n", i - 1));
    }
    source.push_str("return 0");
    let e = failure(exec(&Arc::new(Mock::new()), &source).await, &source);
    assert_eq!((e.code, e.line), (code::NESTING, 202));
    assert_eq!(
        e.message,
        "this value nests 201 levels deep; the limit is 200"
    );
}

#[tokio::test]
async fn a_join_is_charged_before_its_pairs_are_built() {
    let rows = json!((0..2000)
        .map(|i| json!({ "id": i, "k": 1 }))
        .collect::<Vec<_>>());
    let mock = Arc::new(Mock::new().tool("rows", move |_| Ok(rows.clone())));
    let source = "let a = rows({})\nlet b = rows({})\nreturn count(join(a, b, x => x.k, y => y.k))";
    let limits = Limits {
        max_steps: 1_000_000,
        ..Limits::default()
    };
    let outcome = exec_with(&mock, source, limits).await;
    let Outcome::Failed { error, stats } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!((error.code, error.line), (code::BUDGET_STEPS, 3));
    assert!(stats.steps > 4_000_000);
    let source = "let a = rows({})\nreturn count(join(a.take(3), a.take(2), x => x.k, y => y.k))";
    assert_eq!(value(exec(&mock, source).await, source), json!(6));
}

#[tokio::test]
async fn flat_map_is_charged_per_element_produced() {
    let rows = json!((0..2000).collect::<Vec<_>>());
    let mock = Arc::new(Mock::new().tool("rows", move |_| Ok(rows.clone())));
    let source = "let xs = rows({})\nlet all_pairs = xs.flat_map(x => xs)\nreturn 0";
    let e = failure(exec(&mock, source).await, source);
    assert_eq!((e.code, e.line), (code::BUDGET_STEPS, 2));
}
