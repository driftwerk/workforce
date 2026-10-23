use super::{at, exec, exec_with, failure, value, Mock};
use crate::{code, run, Host, Limits, Outcome};
use serde_json::json;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;

fn echo() -> Mock {
    Mock::new().tool("fetch", |args| Ok(json!({ "got": args["id"] })))
}

fn limits(max_concurrency: usize) -> Limits {
    Limits {
        max_concurrency,
        ..Limits::default()
    }
}

fn elapsed(outcome: &Outcome) -> Duration {
    match outcome {
        Outcome::Returned { stats, .. } | Outcome::Failed { stats, .. } => stats.elapsed,
        Outcome::Rejected { .. } => Duration::ZERO,
    }
}

#[tokio::test(start_paused = true)]
async fn results_keep_order_when_calls_finish_out_of_order() {
    let mock = Arc::new(
        echo().delay_by(|args| Duration::from_millis(100 - 10 * args["id"].as_u64().unwrap_or(0))),
    );
    let source = "return all([1, 2, 3, 4, 5].map(i => fetch({ id: i }).got))";
    assert_eq!(
        value(exec(&mock, source).await, source),
        json!([1, 2, 3, 4, 5])
    );
    let source = "return all([fetch({ id: 1 }), fetch({ id: 2 }), 7])";
    assert_eq!(
        value(exec(&mock, source).await, source),
        json!([{ "got": 1 }, { "got": 2 }, 7])
    );
}

#[tokio::test(start_paused = true)]
async fn concurrency_is_bounded_and_faster_than_sequential() {
    let program = |call: &str| format!("let ids = [1, 2, 3, 4, 5, 6, 7, 8]\nreturn {call}");

    let mock = Arc::new(echo().delay(Duration::from_millis(100)));
    let source = program("all(ids.map(i => fetch({ id: i })))");
    let outcome = exec_with(&mock, &source, limits(3)).await;
    assert_eq!(elapsed(&outcome), Duration::from_millis(300));
    assert_eq!(value(outcome, &source).as_array().map(Vec::len), Some(8));
    assert_eq!(mock.peak.load(Ordering::SeqCst), 3);

    let mock = Arc::new(echo().delay(Duration::from_millis(100)));
    let source = program("ids.map(i => fetch({ id: i }))");
    let outcome = exec_with(&mock, &source, limits(3)).await;
    assert_eq!(elapsed(&outcome), Duration::from_millis(800));
    assert_eq!(mock.peak.load(Ordering::SeqCst), 1);
}

#[tokio::test(start_paused = true)]
async fn nested_all_shares_one_bound_without_deadlock() {
    let mock = Arc::new(echo().delay(Duration::from_millis(10)));
    let source = "return all([1, 2, 3].map(i => all([fetch({ id: i }), fetch({ id: i * 10 })]).map(r => r.got)))";
    let outcome = exec_with(&mock, source, limits(2)).await;
    assert_eq!(value(outcome, source), json!([[1, 10], [2, 20], [3, 30]]));
    assert_eq!(mock.peak.load(Ordering::SeqCst), 2);
}

#[tokio::test(start_paused = true)]
async fn the_first_failure_in_order_fails_the_run() {
    let mock = Arc::new(
        Mock::new()
            .tool("fetch", |args| match args["id"].as_i64() {
                Some(2) => Err("not found: 2".to_string()),
                _ => Ok(json!(1)),
            })
            .delay(Duration::from_millis(5)),
    );
    let source = "let ids = [1, 2, 3]\nreturn all(ids.map(i =>\n  fetch({ id: i })))";
    let e = failure(exec(&mock, source).await, source);
    assert_eq!((e.code, at(&e)), (code::TOOL, (3, 3)));
    assert_eq!(e.message, "`fetch` failed: not found: 2");
}

#[tokio::test]
async fn all_over_values_and_misuse() {
    let mock = Arc::new(Mock::new());
    let source = "let xs = [1, 2]\nreturn [all(xs), all([]), all(null?.map(x => x))]";
    assert_eq!(
        value(exec(&mock, source).await, source),
        json!([[1, 2], [], null])
    );
    let source = "return all(5)";
    let e = failure(exec(&mock, source).await, source);
    assert_eq!(e.message, "`all` needs a list; got int 5");
}

#[tokio::test(start_paused = true)]
async fn calls_run_on_the_calling_task() {
    let source = "return all([1, 2, 3].map(i => fetch({ id: i }).got))";
    let mock = Arc::new(echo().delay(Duration::from_millis(1)));
    assert_eq!(value(exec(&mock, source).await, source), json!([1, 2, 3]));

    let host: Arc<dyn Host> = mock.clone();
    let e = failure(run(source, host, Limits::default()).await, source);
    assert_eq!(e.message, "`fetch` failed: called outside the turn's task");
}

#[test]
fn the_run_future_is_send() {
    fn assert_send<T: Send>(_: &T) {}
    let host: Arc<dyn Host> = Arc::new(Mock::new());
    let future = run("return 1", host, Limits::default());
    assert_send(&future);
}

#[tokio::test(start_paused = true)]
async fn the_call_budget_holds_under_all() {
    let mock = Arc::new(echo());
    let source = "return all([1, 2, 3, 4, 5, 6].map(i => fetch({ id: i })))";
    let limits = Limits {
        max_calls: 4,
        ..Limits::default()
    };
    let e = failure(exec_with(&mock, source, limits).await, source);
    assert_eq!(e.code, code::BUDGET_CALLS);
    assert_eq!(mock.tool_calls().len(), 4);
}

#[tokio::test]
async fn a_null_safe_chain_short_circuits_inside_all() {
    let mock = Arc::new(echo());
    let source = "let org = null\nreturn all(org?.ids.map(i => fetch({ id: i })))";
    assert_eq!(value(exec(&mock, source).await, source), json!(null));
    assert!(mock.tool_calls().is_empty());
}
