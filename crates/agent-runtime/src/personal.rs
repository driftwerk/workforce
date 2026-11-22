//! Whether the run in hand has read the person's private connector (their
//! bank: [`PERSONAL`](workforce_common::connector_binding::PERSONAL)): from
//! the moment one of its tools is called, or a tool hands back a
//! conversation that read it, or because its session already had. What
//! would keep something of it (memory, notes, a fine-tuning trace) asks
//! here, so it holds within the turn, not only once the turn is saved.
//!
//! Each run has its own flag ([`scoped`]), a child of the one around it:
//! marking a run marks the runs it is part of, and a run begun inside one
//! that has read it starts read. Runs side by side are kept apart.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use serde_json::Value;
use workforce_common::connector_binding::is_personal;
use workforce_common::session::PERSONAL_READ_KEY;

/// One run's flag.
#[derive(Debug, Default)]
pub struct RunPersonal {
    read: AtomicBool,
    parent: Option<Arc<RunPersonal>>,
}

impl RunPersonal {
    fn mark(&self) {
        self.read.store(true, Ordering::SeqCst);
        if let Some(parent) = &self.parent {
            parent.mark();
        }
    }
}

tokio::task_local! {
    static RUN: Arc<RunPersonal>;
}

/// The flag in scope, if any: what a spawned task carries across.
pub fn current() -> Option<Arc<RunPersonal>> {
    RUN.try_with(Arc::clone).ok()
}

/// `fut` as a run of its own, under the flag around it: it has read the
/// bank already when `read`, or when the run around it has.
pub async fn scoped<F: std::future::Future>(read: bool, fut: F) -> F::Output {
    let parent = current();
    let run = Arc::new(RunPersonal {
        read: AtomicBool::new(
            read || parent
                .as_ref()
                .is_some_and(|p| p.read.load(Ordering::SeqCst)),
        ),
        parent,
    });
    if read {
        run.mark();
    }
    RUN.scope(run, fut).await
}

/// `fut` under `flag` itself (a spawned task's share of its run's), or as
/// it is when there is none.
pub async fn within<F: std::future::Future>(flag: Option<Arc<RunPersonal>>, fut: F) -> F::Output {
    match flag {
        Some(flag) => RUN.scope(flag, fut).await,
        None => fut.await,
    }
}

/// [`within`], for a blocking body.
pub fn sync_within<T>(flag: Option<Arc<RunPersonal>>, f: impl FnOnce() -> T) -> T {
    match flag {
        Some(flag) => RUN.sync_scope(flag, f),
        None => f(),
    }
}

/// Whether the run in hand has read the bank. False outside any run.
pub fn read() -> bool {
    RUN.try_with(|r| r.read.load(Ordering::SeqCst))
        .unwrap_or(false)
}

/// Marks the run in hand, and those it is part of, as having read the bank.
pub fn mark() {
    let _ = RUN.try_with(|r| r.mark());
}

/// What tells one run's lines from another's: the address of its flag,
/// while it runs.
pub fn run_key() -> Option<usize> {
    RUN.try_with(|r| Arc::as_ptr(r) as usize).ok()
}

/// Notes a call of `tool` before it runs: a personal connector's marks the
/// run, whether or not it then succeeds.
pub fn calling(tool: &str) {
    if is_personal(tool) {
        mark();
    }
}

/// Notes what `tool` handed back: a conversation that read the bank, read
/// back, marks the run too.
pub fn answered(output: &Value) {
    if output[PERSONAL_READ_KEY] == Value::Bool(true) {
        mark();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn a_run_is_marked_by_its_bank_call_and_marks_the_runs_around_it_only() {
        assert!(!read());
        mark();
        assert!(!read(), "outside any run nothing is kept");
        let (outer, sibling) = scoped(false, async {
            let inner = scoped(false, async {
                calling("linear.issue_get");
                assert!(!read());
                calling("bank.transactions");
                read()
            })
            .await;
            assert!(inner);
            let sibling = scoped(false, async { read() }).await;
            (read(), sibling)
        })
        .await;
        assert!(outer, "the run it is part of is marked");
        assert!(sibling, "a run begun after inherits what it is part of");
        let (ann, bob) = tokio::join!(
            scoped(false, async {
                calling("bank.accounts");
                tokio::task::yield_now().await;
                read()
            }),
            scoped(false, async {
                tokio::task::yield_now().await;
                read()
            }),
        );
        assert!(ann && !bob, "another person's run alongside is untouched");
        let apart = scoped(false, async {
            scoped(false, async { calling("bank.accounts") }).await;
            scoped(false, async { read() }).await
        })
        .await;
        assert!(apart);
        assert!(
            !scoped(false, async { read() }).await,
            "a new run starts clean"
        );
    }

    #[tokio::test]
    async fn a_session_that_read_it_or_a_read_back_of_one_marks_the_run() {
        assert!(scoped(true, async { read() }).await);
        assert!(
            scoped(false, async {
                answered(&serde_json::json!({ "personal_read": false }));
                let before = read();
                answered(&serde_json::json!({ "personal_read": true }));
                !before && read()
            })
            .await
        );
    }

    #[tokio::test]
    async fn a_spawned_task_under_the_run_s_flag_marks_it() {
        let marked = scoped(false, async {
            let flag = current();
            tokio::spawn(within(flag, async { calling("bank.balances") }))
                .await
                .unwrap();
            read()
        })
        .await;
        assert!(marked);
    }
}
