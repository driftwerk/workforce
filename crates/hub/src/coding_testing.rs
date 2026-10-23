//! Fakes for driving coding rounds through their surfaces in tests: a
//! launcher seating every round on a worker that keeps its jobs (or does
//! not), and an engine that, like a worker, gives each job the same answer
//! every time.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use eyre::Result;
use workforce_agent::tools::exec_runner::{HomeSpec, JobId, RunnerEndpoint, ScopeId};
use workforce_agent::turn_journal::ToolCall;
use workforce_common::session::TurnPrincipal;

use crate::claude_code::{self, RemotePlacement};
use crate::coding_engine::{CliRoundResult, CodingEngine, CodingRound, ModelChoices, RoundContext};
use crate::coding_sandbox::{
    CodingSandbox, DirEntry, ExecOutcome, SandboxCommand, SandboxLauncher, WorkspaceSpec,
};
use crate::projects::setup::SetupReport;

/// A workspace of its own in the temp dir, placed on a worker that keeps its jobs
/// through a restart, or one that does not. It is never fresh, as the
/// workspace of a thread that has run before.
pub(crate) struct Seat {
    inner: Arc<dyn CodingSandbox>,
    kept: bool,
}

#[async_trait]
impl CodingSandbox for Seat {
    fn workdir(&self) -> &Path {
        self.inner.workdir()
    }
    fn setup(&self) -> &[SetupReport] {
        self.inner.setup()
    }
    fn hub_tool_patterns(&self) -> Vec<String> {
        self.inner.hub_tool_patterns()
    }
    fn fresh(&self) -> bool {
        false
    }
    async fn exec(&self, command: SandboxCommand) -> Result<ExecOutcome> {
        self.inner.exec(command).await
    }
    async fn read(&self, path: &Path, max_bytes: usize) -> Result<Option<String>> {
        self.inner.read(path, max_bytes).await
    }
    async fn write(&self, path: &Path, content: &str) -> Result<()> {
        self.inner.write(path, content).await
    }
    async fn list(&self, path: &Path) -> Result<Vec<DirEntry>> {
        self.inner.list(path).await
    }
    fn placement(&self) -> Option<RemotePlacement> {
        Some(RemotePlacement {
            endpoint: RunnerEndpoint::Unix(PathBuf::from("/run/worker.sock")),
            scope: ScopeId::new("seat"),
            jobs: self.kept,
        })
    }
}

/// Seats every round in a temp dir of its own, serving it the hub tools
/// `hub_tools` name, and keeps the workspaces it was asked for, which it was
/// asked to discard, which jobs to end, and which of the scopes `on_disk` a
/// sweep reaped. A `local` seat runs its rounds on this host; with `repo`
/// set, the workspace is a git checkout with one commit.
#[derive(Default)]
pub(crate) struct Seats {
    pub(crate) hub_tools: Vec<String>,
    pub(crate) local: bool,
    pub(crate) repo: bool,
    pub(crate) ensured: Mutex<Vec<(String, String, HomeSpec)>>,
    pub(crate) closed: Mutex<Vec<String>>,
    pub(crate) ended: Mutex<Vec<(String, String)>>,
    pub(crate) unkept: bool,
    pub(crate) on_disk: Vec<String>,
    pub(crate) reaped: Mutex<Vec<String>>,
}

#[async_trait]
impl SandboxLauncher for Seats {
    async fn ensure(&self, spec: &WorkspaceSpec<'_>) -> Result<Arc<dyn CodingSandbox>> {
        self.ensured.lock().unwrap().push((
            spec.project.name.clone(),
            spec.scope.to_string(),
            spec.home.clone(),
        ));
        let workdir = tempfile::tempdir()?.keep();
        if self.repo {
            committed_checkout(&workdir)?;
        }
        let inner = crate::coding_sandbox::adopt_workdir(workdir, self.hub_tools.clone(), None);
        Ok(match self.local {
            true => inner,
            false => Arc::new(Seat {
                inner,
                kept: !self.unkept,
            }),
        })
    }
    async fn exists(&self, _: &str, _: &str, _: &str) -> bool {
        true
    }
    async fn close(&self, _: &str, _: &str, scope: &str) -> Result<()> {
        self.closed.lock().unwrap().push(scope.to_string());
        Ok(())
    }
    async fn end_job(&self, _: &str, _: &str, scope: &str, job: &JobId) {
        self.ended
            .lock()
            .unwrap()
            .push((scope.to_string(), job.to_string()));
    }
    async fn sweep(
        &self,
        _: &str,
        _: &str,
        prefix: &str,
        live: &(dyn for<'a> Fn(&'a str) -> bool + Sync),
    ) {
        let orphans = self
            .on_disk
            .iter()
            .filter(|scope| scope.starts_with(prefix) && !live(scope))
            .cloned();
        self.reaped.lock().unwrap().extend(orphans);
    }
}

/// Make `dir` a git checkout whose one commit holds `src/lib.rs`.
fn committed_checkout(dir: &Path) -> Result<()> {
    std::fs::create_dir_all(dir.join("src"))?;
    std::fs::write(dir.join("src/lib.rs"), "")?;
    let out = std::process::Command::new("sh")
        .args([
            "-c",
            "git init -q && git add -A && git -c user.name=t -c user.email=t@example.com commit -qm base",
        ])
        .current_dir(dir)
        .output()?;
    eyre::ensure!(out.status.success(), "git: {out:?}");
    Ok(())
}

/// The answer every round of [`Rounds`] gives.
pub(crate) const ANSWER: &str = "done\nPR_URL: https://github.com/o/r/pull/1";

/// An engine that keeps each round it was given, and whom it ran as. A
/// keyed round records its job as the round's checkpoint before it runs, as
/// dispatching one does. With `lost` set, the driver's transcript is gone for
/// every leg but the cold restart; with `failing` set, every round fails. A
/// probe, when set, runs mid-round, and each round makes its `writes`. A
/// round answers with the next of `answers` while any are left.
#[derive(Default)]
pub(crate) struct Rounds {
    pub(crate) seen: Mutex<Vec<(CodingRound, TurnPrincipal)>>,
    pub(crate) lost: bool,
    pub(crate) failing: bool,
    pub(crate) probe: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// Files each round writes into its workspace, by path and content.
    pub(crate) writes: Vec<(&'static str, &'static str)>,
    pub(crate) answers: Mutex<std::collections::VecDeque<&'static str>>,
}

impl Rounds {
    /// An engine whose rounds give `answers` in turn.
    pub(crate) fn answering(answers: &[&'static str]) -> Self {
        Self {
            answers: Mutex::new(answers.iter().copied().collect()),
            ..Default::default()
        }
    }

    /// Each keyed job the engine was asked to run, whether it was pinned to
    /// that one job, and who it ran as.
    pub(crate) fn runs(&self) -> Vec<(String, bool, Option<String>)> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter_map(|(round, principal)| {
                let jobs = round.jobs.as_ref()?;
                Some((
                    jobs.job().to_string(),
                    jobs.pinned(),
                    principal.run_as.clone(),
                ))
            })
            .collect()
    }

    /// Whom each round was billed to, apart from whom it ran as.
    pub(crate) fn billed(&self) -> Vec<Option<String>> {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .map(|(_, principal)| principal.bill_to.clone())
            .collect()
    }
}

#[async_trait]
impl CodingEngine for Rounds {
    fn label(&self) -> &'static str {
        "claude-code"
    }
    fn preflight(&self, _: &RoundContext<'_>) -> Result<(), crate::coding_engine::Refusal> {
        Ok(())
    }
    fn model_choices(&self) -> ModelChoices {
        ModelChoices {
            models: Vec::new(),
            efforts: &[],
            raw_id_prefixes: &[],
        }
    }
    async fn run(
        &self,
        sandbox: Arc<dyn CodingSandbox>,
        round: &CodingRound,
        _: &RoundContext<'_>,
        _: Option<tokio_util::sync::CancellationToken>,
        progress: Option<tokio::sync::mpsc::Sender<workforce_agent::runner::ProgressEvent>>,
    ) -> Result<CliRoundResult> {
        if let (Some(jobs), Some(call)) = (round.jobs.as_ref(), ToolCall::current()) {
            call.checkpoint(jobs.job().as_str()).await?;
        }
        if let Some(probe) = self.probe.lock().unwrap().as_ref() {
            probe();
        }
        for (path, content) in &self.writes {
            sandbox.write(Path::new(path), content).await?;
        }
        if let Some(progress) = progress {
            let _ = progress
                .send(workforce_agent::runner::ProgressEvent::Response {
                    text: ANSWER.into(),
                })
                .await;
        }
        self.seen
            .lock()
            .unwrap()
            .push((round.clone(), crate::user_config::current_principal()));
        let cold = round
            .jobs
            .as_ref()
            .is_some_and(|jobs| jobs.round.ends_with("-cold"));
        let completion = match (self.failing, self.lost && !cold) {
            (true, _) => claude_code::Completion::Failed,
            (false, true) => claude_code::Completion::SessionNotFound,
            (false, false) => claude_code::Completion::Success,
        };
        let answer = self.answers.lock().unwrap().pop_front().unwrap_or(ANSWER);
        Ok(CliRoundResult {
            text: answer.into(),
            exit_code: i32::from(self.failing),
            completion,
            ..Default::default()
        })
    }
}

/// A chat surface that keeps what was posted to it and every edit made to
/// what it posted; with `forgetful` set, it holds no message to edit.
#[derive(Default)]
pub(crate) struct Board {
    pub(crate) posted: Mutex<Vec<String>>,
    pub(crate) edits: Mutex<Vec<(String, String)>>,
    pub(crate) forgetful: bool,
}

#[async_trait]
impl workforce_common::channels::ChannelSurface for Board {
    fn platform(&self) -> &str {
        "board"
    }
    fn capabilities(&self) -> workforce_common::channels::ChannelCapabilities {
        Default::default()
    }
    async fn add_reaction(&self, _: &workforce_common::channels::Reaction) -> Result<()> {
        Ok(())
    }
    async fn remove_reaction(&self, _: &workforce_common::channels::Reaction) -> Result<()> {
        Ok(())
    }
    async fn send_message(
        &self,
        message: &workforce_common::channels::OutboundMessage,
    ) -> Result<workforce_common::channels::SentMessage> {
        let mut posted = self.posted.lock().unwrap();
        posted.push(message.text.clone());
        Ok(workforce_common::channels::SentMessage {
            channel: message.channel.clone(),
            message_id: format!("card-{}", posted.len()),
            thread_id: message.thread_id.clone(),
            platform: "board".into(),
        })
    }
    async fn edit_message(&self, _: &str, message_id: &str, text: &str) -> Result<()> {
        eyre::ensure!(!self.forgetful, "message {message_id} not found");
        self.edits
            .lock()
            .unwrap()
            .push((message_id.to_string(), text.to_string()));
        Ok(())
    }
}
