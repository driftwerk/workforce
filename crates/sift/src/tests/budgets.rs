use super::{at, exec, exec_with, failure, Mock};
use crate::{code, Limits, Outcome};
use serde_json::json;
use std::sync::Arc;
use std::time::Duration;

fn numbers() -> Mock {
    Mock::new().tool("numbers", |_| Ok(json!((0..1000).collect::<Vec<i64>>())))
}

#[tokio::test]
async fn steps() {
    let mock = Arc::new(Mock::new());
    let source =
        "let xs = [1, 2, 3, 4, 5, 6, 7, 8, 9, 10]\nfor x in xs {\n  let y = x * 2 + 1\n}\nreturn 1";
    let limits = Limits {
        max_steps: 40,
        ..Limits::default()
    };
    let outcome = exec_with(&mock, source, limits.clone()).await;
    let Outcome::Failed { error, stats } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!(error.code, code::BUDGET_STEPS);
    assert_eq!(error.message, "the run used all of its 40 steps");
    assert_eq!(error.line, 3);
    assert!(stats.steps > 40);
    let ok = exec_with(&mock, "return 1 + 2", limits).await;
    assert!(matches!(ok, Outcome::Returned { stats, .. } if stats.steps == 4));
}

#[tokio::test]
async fn bulk_library_work_costs_steps() {
    let mock = Arc::new(numbers());
    let source = "let xs = numbers({})\nreturn sum(xs)";
    let limits = Limits {
        max_steps: 500,
        ..Limits::default()
    };
    let e = failure(exec_with(&mock, source, limits).await, source);
    assert_eq!((e.code, at(&e)), (code::BUDGET_STEPS, (2, 8)));
}

#[tokio::test]
async fn calls() {
    let mock = Arc::new(Mock::new().tool("get", |_| Ok(json!(1))));
    let source = "for i in [1, 2, 3, 4] {\n  let r = get({ i })\n}\nreturn 0";
    let limits = Limits {
        max_calls: 3,
        ..Limits::default()
    };
    let outcome = exec_with(&mock, source, limits).await;
    let rendered = outcome.render(source);
    let Outcome::Failed { error, stats } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!((error.code, at(&error)), (code::BUDGET_CALLS, (2, 11)));
    assert_eq!(
        error.message,
        "the run may make 3 tool and SQL calls; `get` would be call 4"
    );
    assert_eq!(stats.calls, 3);
    assert_eq!(mock.tool_calls().len(), 3);
    assert!(rendered.ends_with("ran 3 calls: get ×3"), "{rendered}");
}

#[tokio::test(start_paused = true)]
async fn deadline_names_the_running_call() {
    let mock = Arc::new(
        Mock::new()
            .tool("slow", |_| Ok(json!(1)))
            .delay(Duration::from_secs(10)),
    );
    let source = "let a = 1\nreturn slow({})";
    let limits = Limits {
        deadline: Duration::from_secs(2),
        ..Limits::default()
    };
    let outcome = exec_with(&mock, source, limits).await;
    let Outcome::Failed { error, stats } = outcome else {
        panic!("{outcome:?}")
    };
    assert_eq!((error.code, at(&error)), (code::BUDGET_TIME, (2, 8)));
    assert_eq!(
        error.message,
        "the run's deadline of 2s passed while `slow` was running"
    );
    assert_eq!(stats.elapsed, Duration::from_secs(2));
    assert_eq!(stats.journal.len(), 1);
    assert!(!stats.journal[0].ok);
}

#[tokio::test]
async fn deadline_in_pure_computation() {
    let mock = Arc::new(Mock::new());
    let source = "let xs = split(\"a,b,c,d,e,f,g,h,i,j\", \",\")\nfor x in xs { for y in xs { for z in xs { let w = x + y + z } } }\nreturn 0";
    let limits = Limits {
        deadline: Duration::ZERO,
        ..Limits::default()
    };
    let e = failure(exec_with(&mock, source, limits).await, source);
    assert_eq!(e.code, code::BUDGET_TIME);
    assert_eq!(
        e.message,
        "the run's deadline of 0ns passed while evaluating"
    );
    assert_eq!(e.line, 2);
}

#[tokio::test]
async fn return_size() {
    let mock = Arc::new(numbers());
    let source = "let xs = numbers({})\nreturn xs";
    let limits = Limits {
        max_return_bytes: 2000,
        ..Limits::default()
    };
    let e = failure(exec_with(&mock, source, limits).await, source);
    assert_eq!((e.code, at(&e)), (code::BUDGET_RETURN, (2, 1)));
    assert_eq!(
        e.message,
        "the return value is 3891 bytes; the limit is 2000"
    );
    assert!(e.help.is_some_and(|h| h.contains(".take(n)")));
    let source = "let xs = numbers({})\nreturn xs.take(3)";
    assert!(matches!(
        exec(&mock, source).await,
        Outcome::Returned { .. }
    ));
}
