//! The compile gate: a turn that edited source files in a repo does not end
//! on code that does not build.
//!
//! When the agent would answer, the repo's build check runs over everything
//! it edited since the last passing check. A failure goes back to the agent
//! as a note for a bounded number of fix rounds; one that outlasts them is
//! stated in the answer. Which edits count and how a repo is checked are the
//! [`CompileGate`]'s business; when to check and what to do with the result
//! are the loop's, here.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;
use tokio_util::sync::CancellationToken;
use tracing::info;

use crate::runner::ProgressEvent;

/// Whether a call to the tool named with this input would run right now for
/// the turn's user, without asking anyone. A check runs the repo's own build
/// scripts, so it runs only where the call it stands for would.
pub type Authorise<'a> = dyn Fn(&str, &Value) -> bool + Send + Sync + 'a;

/// What checking the projects behind a set of edited files found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CheckReport {
    /// Nothing among the files is checked: docs, configs, generated files,
    /// or a project whose build the gate cannot run safely.
    NotApplicable,
    /// Every check these files call for stands for a call the turn's user
    /// may not make, so none ran.
    NotPermitted,
    Passed,
    /// The build fails; `digest` names each check and its errors, and
    /// `advice`, when there is any, tells the agent how to fix them.
    Failed {
        digest: String,
        advice: Option<String>,
    },
    /// A check could not finish (timed out, or could not be started), so the
    /// build is neither passing nor known to fail.
    Inconclusive {
        reason: String,
    },
}

/// Decides which edits count and checks the projects they belong to.
#[async_trait]
pub trait CompileGate: Send + Sync {
    /// The files a successful call to `tool` changed, as that tool resolved
    /// them; empty for a call that edits nothing.
    fn edited_files(&self, tool: &str, input: &Value, output: &Value) -> Vec<PathBuf>;

    /// Check that the projects `files` belong to build, running each check
    /// only when `authorise` admits the call it stands for.
    async fn check(&self, files: &BTreeSet<PathBuf>, authorise: &Authorise<'_>) -> CheckReport;
}

/// The compile gate a turn runs with.
#[derive(Clone)]
pub struct CompileGateSettings {
    pub gate: Arc<dyn CompileGate>,
    /// Rounds given to fixing a failing build before the turn ends with it.
    pub max_fix_rounds: u32,
}

/// What a turn about to answer should do.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    /// Answer, with this notice after the answer when there is one.
    Finish(Option<String>),
    /// Take another round: this note tells the agent what to fix.
    Fix(String),
    /// The turn was cancelled while the check ran.
    Cancelled,
}

/// One turn's gate: what it edited since the last passing check, and how
/// the checks so far went.
pub(crate) struct GateTurn {
    settings: CompileGateSettings,
    agent: String,
    files: BTreeSet<PathBuf>,
    unchecked: bool,
    fixes: u32,
    failing: Option<String>,
}

impl GateTurn {
    pub(crate) fn new(settings: Option<&CompileGateSettings>, agent: &str) -> Option<Self> {
        settings.map(|settings| Self {
            settings: settings.clone(),
            agent: agent.to_string(),
            files: BTreeSet::new(),
            unchecked: false,
            fixes: 0,
            failing: None,
        })
    }

    /// Take note of one finished call.
    pub(crate) fn observe(&mut self, tool: &str, input: &Value, output: &Value, success: bool) {
        if !success {
            return;
        }
        let edited = self.settings.gate.edited_files(tool, input, output);
        self.unchecked |= !edited.is_empty();
        self.files.extend(edited);
    }

    /// Answer with the last check's failure, if it failed, without checking
    /// again: what a turn asked to stop does.
    pub(crate) fn standing(&self) -> Verdict {
        Verdict::Finish(self.failing.as_deref().map(failing_notice))
    }

    /// What the turn should do now that the agent would answer. `can_fix`
    /// says whether a round that can still call tools is left.
    pub(crate) async fn verdict(
        &mut self,
        authorise: &Authorise<'_>,
        can_fix: bool,
        progress: Option<&tokio::sync::mpsc::Sender<ProgressEvent>>,
        cancel: Option<&CancellationToken>,
    ) -> Verdict {
        if !self.unchecked {
            return self.standing();
        }
        self.unchecked = false;
        if let Some(tx) = progress {
            let _ = tx
                .send(ProgressEvent::SubActivity {
                    source: "build check".to_string(),
                    detail: "checking the edited code builds".to_string(),
                })
                .await;
        }
        let check = self.settings.gate.check(&self.files, authorise);
        let report = match cancel {
            Some(token) => tokio::select! {
                report = check => report,
                () = token.cancelled() => return Verdict::Cancelled,
            },
            None => check.await,
        };
        info!(target: "compile_gate", agent = %self.agent, files = self.files.len(), fixes = self.fixes, report = report_label(&report), "build check finished");
        match report {
            CheckReport::NotPermitted => self.standing(),
            CheckReport::Passed | CheckReport::NotApplicable => {
                self.files.clear();
                self.failing = None;
                Verdict::Finish(None)
            }
            CheckReport::Failed { digest, advice } => {
                let verdict = match can_fix && self.fixes < self.settings.max_fix_rounds {
                    true => {
                        self.fixes += 1;
                        Verdict::Fix(fix_note(&digest, advice.as_deref()))
                    }
                    false => Verdict::Finish(Some(failing_notice(&digest))),
                };
                self.failing = Some(digest);
                verdict
            }
            CheckReport::Inconclusive { reason } => {
                self.failing = None;
                Verdict::Finish(Some(unverified_notice(&reason)))
            }
        }
    }
}

fn report_label(report: &CheckReport) -> &'static str {
    match report {
        CheckReport::NotApplicable => "not_applicable",
        CheckReport::NotPermitted => "not_permitted",
        CheckReport::Passed => "passed",
        CheckReport::Failed { .. } => "failed",
        CheckReport::Inconclusive { .. } => "inconclusive",
    }
}

fn fix_note(digest: &str, advice: Option<&str>) -> String {
    let advice = advice
        .map(|advice| format!(" {advice}"))
        .unwrap_or_default();
    format!(
        "[SYSTEM: Your edits do not build.\n\n{digest}\n\nFix these errors, then give your answer \
         again.{advice} The build is checked again before your answer goes out.]"
    )
}

fn failing_notice(digest: &str) -> String {
    format!(
        "**The build is failing.** These errors remain after the edits in this turn:\n\n{digest}"
    )
}

fn unverified_notice(reason: &str) -> String {
    format!("**The build was not verified:** {reason}.")
}

/// The answer with the gate's notice, if any, after it, and the text that
/// adds to the answer as it stood: what a streamed turn still has to stream.
pub(crate) fn with_notice(answer: String, notice: Option<&str>) -> (String, Option<String>) {
    match notice {
        Some(notice) if answer.trim().is_empty() => (notice.to_string(), Some(notice.to_string())),
        Some(notice) => {
            let added = format!("\n\n{notice}");
            (format!("{answer}{added}"), Some(added))
        }
        None => (answer, None),
    }
}

/// `response` without the gate's `notice`, as [`with_notice`] added it.
pub fn without_notice<'a>(response: &'a str, notice: &str) -> std::borrow::Cow<'a, str> {
    let added = format!("\n\n{notice}");
    match (response.find(&added), response.strip_prefix(notice)) {
        (Some(at), _) => std::borrow::Cow::Owned(format!(
            "{}{}",
            &response[..at],
            &response[at + added.len()..]
        )),
        (None, Some(rest)) => std::borrow::Cow::Borrowed(rest),
        (None, None) => std::borrow::Cow::Borrowed(response),
    }
}

#[cfg(test)]
pub(crate) mod testing {
    use std::collections::VecDeque;
    use std::path::Path;

    use parking_lot::Mutex;

    use super::*;

    /// The tool a scripted check stands for.
    pub(crate) const BUILD: &str = "code.build";

    /// A gate that counts every call whose tool is `edit` with a `path` as an
    /// edit of that path, and answers checks the turn may run as [`BUILD`]
    /// from a script.
    pub(crate) struct ScriptedGate {
        reports: Mutex<VecDeque<CheckReport>>,
        pub(crate) checked: Mutex<Vec<BTreeSet<PathBuf>>>,
    }

    impl ScriptedGate {
        pub(crate) fn new(reports: Vec<CheckReport>) -> Arc<Self> {
            Arc::new(Self {
                reports: Mutex::new(reports.into()),
                checked: Mutex::new(Vec::new()),
            })
        }

        pub(crate) fn checks(&self) -> usize {
            self.checked.lock().len()
        }
    }

    #[async_trait]
    impl CompileGate for ScriptedGate {
        fn edited_files(&self, tool: &str, input: &Value, _output: &Value) -> Vec<PathBuf> {
            match (tool, input.get("path").and_then(Value::as_str)) {
                ("edit", Some(path)) => vec![Path::new(path).to_path_buf()],
                _ => Vec::new(),
            }
        }

        async fn check(&self, files: &BTreeSet<PathBuf>, authorise: &Authorise<'_>) -> CheckReport {
            if !authorise(BUILD, &Value::Null) {
                return CheckReport::NotPermitted;
            }
            self.checked.lock().push(files.clone());
            self.reports
                .lock()
                .pop_front()
                .expect("more checks than scripted")
        }
    }

    pub(crate) fn settings(gate: &Arc<ScriptedGate>, max_fix_rounds: u32) -> CompileGateSettings {
        CompileGateSettings {
            gate: Arc::clone(gate) as Arc<dyn CompileGate>,
            max_fix_rounds,
        }
    }

    pub(crate) fn failed(digest: &str) -> CheckReport {
        CheckReport::Failed {
            digest: digest.to_string(),
            advice: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::testing::{failed, settings, ScriptedGate};
    use super::*;

    fn turn(gate: &Arc<ScriptedGate>, max_fix_rounds: u32) -> GateTurn {
        GateTurn::new(Some(&settings(gate, max_fix_rounds)), "agent").unwrap()
    }

    fn edit(turn: &mut GateTurn, path: &str) {
        turn.observe("edit", &json!({ "path": path }), &json!({}), true);
    }

    async fn verdict(turn: &mut GateTurn, can_fix: bool) -> Verdict {
        turn.verdict(&|_, _| true, can_fix, None, None).await
    }

    #[tokio::test]
    async fn a_turn_that_edited_nothing_is_never_checked() {
        let gate = ScriptedGate::new(vec![]);
        let mut turn = turn(&gate, 2);
        turn.observe("read", &json!({ "path": "src/lib.rs" }), &json!({}), true);
        turn.observe("edit", &json!({ "path": "src/lib.rs" }), &json!({}), false);
        assert_eq!(verdict(&mut turn, true).await, Verdict::Finish(None));
        assert_eq!(gate.checks(), 0);
    }

    #[tokio::test]
    async fn a_failure_is_fixed_within_the_bound_then_reported() {
        let gate = ScriptedGate::new(vec![failed("e1"), failed("e2"), failed("e3")]);
        let mut turn = turn(&gate, 2);
        for round in 0..2 {
            edit(&mut turn, "src/lib.rs");
            match verdict(&mut turn, true).await {
                Verdict::Fix(note) => assert!(note.contains(&format!("e{}", round + 1))),
                other => panic!("round {round}: expected a fix round, got {other:?}"),
            }
        }
        edit(&mut turn, "src/lib.rs");
        match verdict(&mut turn, true).await {
            Verdict::Finish(Some(notice)) => {
                assert!(notice.contains("The build is failing"));
                assert!(notice.contains("e3"));
            }
            other => panic!("expected the failure in the answer, got {other:?}"),
        }
        assert_eq!(gate.checks(), 3);
    }

    #[tokio::test]
    async fn advice_goes_to_the_agent_and_never_into_the_answer() {
        let advised = || CheckReport::Failed {
            digest: "E1".into(),
            advice: Some("Send these to the session that made them.".into()),
        };
        let gate = ScriptedGate::new(vec![advised(), advised()]);
        let mut turn = turn(&gate, 1);
        edit(&mut turn, "src/lib.rs");
        match verdict(&mut turn, true).await {
            Verdict::Fix(note) => assert!(note.contains("Send these to the session")),
            other => panic!("expected a fix round, got {other:?}"),
        }
        edit(&mut turn, "src/lib.rs");
        match verdict(&mut turn, true).await {
            Verdict::Finish(Some(notice)) => {
                assert!(notice.contains("E1"));
                assert!(!notice.contains("Send these"), "{notice}");
            }
            other => panic!("expected the failure in the answer, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn a_fix_round_that_edits_nothing_keeps_the_failure_without_checking_again() {
        let gate = ScriptedGate::new(vec![failed("broken")]);
        let mut turn = turn(&gate, 2);
        edit(&mut turn, "src/lib.rs");
        assert!(matches!(verdict(&mut turn, true).await, Verdict::Fix(_)));
        match verdict(&mut turn, true).await {
            Verdict::Finish(Some(notice)) => assert!(notice.contains("broken")),
            other => panic!("expected the standing failure, got {other:?}"),
        }
        assert_eq!(gate.checks(), 1);
    }

    #[tokio::test]
    async fn a_recheck_covers_every_file_since_the_last_pass() {
        let gate = ScriptedGate::new(vec![failed("broken"), CheckReport::Passed]);
        let mut turn = turn(&gate, 2);
        edit(&mut turn, "a/src/lib.rs");
        assert!(matches!(verdict(&mut turn, true).await, Verdict::Fix(_)));
        edit(&mut turn, "b/src/lib.rs");
        assert_eq!(verdict(&mut turn, true).await, Verdict::Finish(None));
        let checked = gate.checked.lock();
        assert_eq!(
            checked[1],
            BTreeSet::from(["a/src/lib.rs".into(), "b/src/lib.rs".into()]),
            "a fix elsewhere must not hide the project that failed"
        );
    }

    #[tokio::test]
    async fn no_fix_round_is_taken_when_none_can_call_tools() {
        let gate = ScriptedGate::new(vec![failed("broken")]);
        let mut turn = turn(&gate, 2);
        edit(&mut turn, "src/lib.rs");
        assert!(matches!(
            verdict(&mut turn, false).await,
            Verdict::Finish(Some(_))
        ));
    }

    #[tokio::test]
    async fn a_check_that_times_out_is_reported_unverified_and_not_retried() {
        let gate = ScriptedGate::new(vec![CheckReport::Inconclusive {
            reason: "`cargo check` timed out after 600s".into(),
        }]);
        let mut turn = turn(&gate, 2);
        edit(&mut turn, "src/lib.rs");
        match verdict(&mut turn, true).await {
            Verdict::Finish(Some(notice)) => {
                assert!(notice.contains("not verified"));
                assert!(notice.contains("timed out"));
            }
            other => panic!("expected an unverified notice, got {other:?}"),
        }
        assert_eq!(gate.checks(), 1);
    }

    #[tokio::test]
    async fn a_turn_not_permitted_to_build_is_never_checked_and_keeps_a_standing_failure() {
        let gate = ScriptedGate::new(vec![failed("broken")]);
        let mut turn = turn(&gate, 2);
        edit(&mut turn, "src/lib.rs");
        assert!(matches!(verdict(&mut turn, true).await, Verdict::Fix(_)));
        edit(&mut turn, "src/lib.rs");
        match turn.verdict(&|_, _| false, true, None, None).await {
            Verdict::Finish(Some(notice)) => assert!(notice.contains("broken")),
            other => panic!("expected the standing failure, got {other:?}"),
        }
        assert_eq!(gate.checks(), 1);
    }

    #[tokio::test]
    async fn a_turn_asked_to_stop_answers_with_the_standing_failure() {
        let gate = ScriptedGate::new(vec![failed("broken")]);
        let mut turn = turn(&gate, 2);
        edit(&mut turn, "src/lib.rs");
        assert!(matches!(verdict(&mut turn, true).await, Verdict::Fix(_)));
        edit(&mut turn, "src/lib.rs");
        match turn.standing() {
            Verdict::Finish(Some(notice)) => assert!(notice.contains("broken")),
            other => panic!("expected the standing failure, got {other:?}"),
        }
        assert_eq!(gate.checks(), 1);
    }

    #[tokio::test]
    async fn a_cancelled_turn_stops_waiting_for_its_check() {
        struct Hangs;
        #[async_trait]
        impl CompileGate for Hangs {
            fn edited_files(&self, _: &str, _: &Value, _: &Value) -> Vec<PathBuf> {
                vec!["src/lib.rs".into()]
            }
            async fn check(&self, _: &BTreeSet<PathBuf>, _: &Authorise<'_>) -> CheckReport {
                std::future::pending().await
            }
        }
        let settings = CompileGateSettings {
            gate: Arc::new(Hangs),
            max_fix_rounds: 2,
        };
        let mut turn = GateTurn::new(Some(&settings), "agent").unwrap();
        turn.observe("edit", &json!({}), &json!({}), true);
        let token = CancellationToken::new();
        token.cancel();
        assert_eq!(
            turn.verdict(&|_, _| true, true, None, Some(&token)).await,
            Verdict::Cancelled
        );
    }

    #[test]
    fn a_notice_follows_the_answer_and_comes_off_it_again() {
        assert_eq!(with_notice("done".into(), None), ("done".into(), None));
        assert_eq!(
            with_notice("done".into(), Some("N")),
            ("done\n\nN".into(), Some("\n\nN".into()))
        );
        assert_eq!(
            with_notice(" ".into(), Some("N")),
            ("N".into(), Some("N".into()))
        );
        assert_eq!(without_notice("done\n\nN", "N"), "done");
        assert_eq!(
            without_notice("done\n\nN\n\nSources: a.rs", "N"),
            "done\n\nSources: a.rs",
            "text appended after the notice stays"
        );
        assert_eq!(without_notice("N", "N"), "");
        assert_eq!(without_notice("done", "N"), "done");
    }
}
