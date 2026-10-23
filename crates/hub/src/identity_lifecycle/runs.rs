//! How an operation runs: checked and claimed in the caller, then driven
//! category by category, inline for a dry run or as a task the hub owns for
//! a real one, so a caller that gives up never stops it part-way. A running
//! operation renews its lease and records its progress before every category
//! and every [`HEARTBEAT`] while one runs, and stops between categories once
//! the hub begins to shut down; a rerun resumes it.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use eyre::Result;
use tokio_util::sync::CancellationToken;
use tokio_util::task::TaskTracker;
use uuid::Uuid;
use workforce_identity::lifecycle::{LifecycleOp, OpStatus};

use super::{
    category_report, Actor, Category, CategoryReport, Context, DeletePlan, Lifecycle,
    LifecycleError, Person, Progress, Report, Subject, Tally,
};

/// How often a category under way renews its operation's lease.
pub(super) const HEARTBEAT: Duration = Duration::from_secs(60);

/// The operations the hub runs in the background, and what stops them.
pub struct Runs {
    stop: CancellationToken,
    tasks: TaskTracker,
    heartbeat: Duration,
}

impl Default for Runs {
    fn default() -> Self {
        Self::until(CancellationToken::new())
    }
}

impl Runs {
    /// Runs that stop between categories once `stop` is cancelled.
    pub fn until(stop: CancellationToken) -> Self {
        Self {
            stop,
            tasks: TaskTracker::new(),
            heartbeat: HEARTBEAT,
        }
    }

    #[cfg(test)]
    pub(super) fn beating_every(mut self, heartbeat: Duration) -> Self {
        self.heartbeat = heartbeat;
        self
    }

    pub fn running(&self) -> usize {
        self.tasks.len()
    }

    fn stopping(&self) -> bool {
        self.stop.is_cancelled()
    }
}

/// An operation an administrator asks for.
#[derive(Debug, Clone)]
pub enum Request {
    Merge { from: Uuid, into: Uuid },
    Delete { subject: Uuid, plan: DeletePlan },
    TransferOrphan { id: Uuid, to: Uuid, personal: bool },
    DeleteOrphan { id: Uuid, plan: DeletePlan },
}

/// What [`Lifecycle::launch`] gives back: a dry run's report, or the record
/// of a real run it started, which carries its progress and then its report.
#[derive(Debug)]
pub enum Launched {
    Report(Report),
    Started(LifecycleOp),
}

/// What an operation does with each category.
#[async_trait]
pub(super) trait Steps: Send + Sync {
    async fn step(
        &self,
        cx: &Context<'_>,
        category: Category,
        who: &Person,
        apply: bool,
    ) -> Result<Tally>;

    /// Whether this operation acts on `category` at all; one it skips is
    /// left out of the report.
    fn visits(&self, _category: Category) -> bool {
        true
    }

    /// Name in `report` what this operation chose for its category.
    fn annotate(&self, _report: &mut CategoryReport) {}

    /// Whether a run that finishes every category removes the subject's
    /// identity: only a merge or a delete of a live identity does.
    fn retires(&self) -> bool {
        false
    }
}

/// An operation checked and ready to run.
pub(super) struct Operation {
    pub(super) kind: &'static str,
    pub(super) subject: Subject,
    pub(super) who: Person,
    pub(super) target: Option<Uuid>,
    pub(super) steps: Box<dyn Steps>,
    pub(super) actor: Actor,
}

/// How a visit of the categories ended.
enum Ended {
    Visited,
    Stopped,
}

impl Lifecycle {
    /// Run `request` to the end: a dry run, or a real run claimed here.
    pub(super) async fn perform(
        &self,
        request: &Request,
        actor: &Actor,
        dry_run: bool,
    ) -> Result<Report, LifecycleError> {
        let operation = self.prepare(request, actor).await?;
        let op = (!dry_run).then(|| self.claim(&operation)).transpose()?;
        self.run(&operation, op.as_ref()).await
    }

    /// A dry run answers with its report. A real run is checked and claimed
    /// before this returns, so a refusal or a busy subject is answered at
    /// once, and then runs as a task of its own: progress and the final
    /// report are read from its record ([`Lifecycle::operation`]).
    pub async fn launch(
        self: &Arc<Self>,
        request: Request,
        actor: Actor,
        dry_run: bool,
    ) -> Result<Launched, LifecycleError> {
        if dry_run {
            return self
                .perform(&request, &actor, true)
                .await
                .map(Launched::Report);
        }
        let operation = self.prepare(&request, &actor).await?;
        self.start(operation).map(Launched::Started)
    }

    /// Claim `operation` and run it as a task the hub owns, answering with
    /// the claim at once.
    pub(super) fn start(
        self: &Arc<Self>,
        operation: Operation,
    ) -> Result<LifecycleOp, LifecycleError> {
        if self.runs.stopping() {
            return Err(LifecycleError::Busy(
                "The hub is shutting down; run it again in a moment".into(),
            ));
        }
        let op = self.claim(&operation)?;
        let (lifecycle, claimed) = (Arc::clone(self), op.clone());
        self.runs.tasks.spawn(async move {
            if let Err(e) = lifecycle.run(&operation, Some(&claimed)).await {
                tracing::warn!(
                    target: "audit.identity_lifecycle",
                    operation = claimed.kind,
                    id = %claimed.id,
                    error = %format!("{e:#}"),
                    "lifecycle operation stopped"
                );
            }
        });
        Ok(op)
    }

    async fn prepare(&self, request: &Request, actor: &Actor) -> Result<Operation, LifecycleError> {
        match request {
            Request::Merge { from, into } => self.prepare_merge(*from, *into, actor),
            Request::Delete { subject, plan } => {
                super::refuse_self(actor, *subject, "delete")?;
                let who = self.person(*subject, "delete")?;
                self.prepare_erase("delete", Subject::Identity, who, plan, actor)
                    .await
            }
            Request::TransferOrphan { id, to, personal } => {
                self.prepare_transfer_orphan(*id, *to, *personal, actor)
                    .await
            }
            Request::DeleteOrphan { id, plan } => {
                self.prepare_delete_orphan(*id, plan, actor).await
            }
        }
    }

    pub(super) fn claim(&self, operation: &Operation) -> Result<LifecycleOp, LifecycleError> {
        self.record_op(
            operation.kind,
            &operation.who,
            operation.target,
            &operation.actor,
        )
    }

    /// Visit every category, as a dry run or, with `op`, for real. A real
    /// run finishes by removing the subject's identity once every category
    /// is done, and records how it ended unless another run took it over.
    pub(super) async fn run(
        &self,
        operation: &Operation,
        op: Option<&LifecycleOp>,
    ) -> Result<Report, LifecycleError> {
        let mut report = Report::new(
            operation.kind,
            operation.who.id,
            operation.target,
            op.is_none(),
        );
        let visited = self.visit(operation, op, &mut report).await;
        let Some(op) = op else {
            return visited.map(|_| report);
        };
        match visited.and_then(|ended| self.conclude(operation, op, ended, &mut report)) {
            Ok(status) => {
                self.finish_op(op, status, &report)?;
                Ok(report)
            }
            Err(e @ LifecycleError::Busy(_)) => Err(e),
            Err(e) => {
                report.error = Some(format!("{e:#}"));
                self.finish_op(op, OpStatus::Incomplete, &report)?;
                Err(e)
            }
        }
    }

    async fn visit(
        &self,
        operation: &Operation,
        op: Option<&LifecycleOp>,
        report: &mut Report,
    ) -> Result<Ended, LifecycleError> {
        let cx = self.context(&operation.actor);
        for (done, category) in Category::ALL.into_iter().enumerate() {
            if !operation.steps.visits(category) {
                continue;
            }
            if op.is_some() && self.runs.stopping() {
                report.progress = None;
                return Ok(Ended::Stopped);
            }
            let who = match operation.subject {
                Subject::Identity => operation.who.clone(),
                Subject::Orphan => self.still_orphaned(&operation.who)?,
            };
            report.progress = Some(Progress {
                category,
                done,
                total: Category::ALL.len(),
            });
            let step = operation.steps.step(&cx, category, &who, op.is_some());
            let outcome = match op {
                Some(op) => {
                    self.hold(op, report)?;
                    self.attend(op, report, step).await?
                }
                None => step.await,
            };
            if let Err(e) = &outcome {
                tracing::warn!(target: "audit.identity_lifecycle", category = category.name(), error = %format!("{e:#}"), "lifecycle category failed");
            }
            let mut entry = category_report(category, outcome);
            operation.steps.annotate(&mut entry);
            report.categories.push(entry);
        }
        report.progress = None;
        Ok(Ended::Visited)
    }

    /// Wait for `step`, renewing `op`'s lease every heartbeat so a category
    /// that runs longer than the lease is never taken over; once another run
    /// has taken it over, `step` is dropped unfinished. A renewal that fails
    /// is tried again at the next beat, the lease having most of its time
    /// left.
    async fn attend(
        &self,
        op: &LifecycleOp,
        report: &Report,
        step: impl Future<Output = Result<Tally>>,
    ) -> Result<Result<Tally>, LifecycleError> {
        let every = self.runs.heartbeat;
        let mut beat = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
        let mut step = std::pin::pin!(step);
        loop {
            tokio::select! {
                outcome = &mut step => return Ok(outcome),
                _ = beat.tick() => match self.hold(op, report) {
                    Err(e @ LifecycleError::Busy(_)) => return Err(e),
                    Err(e) => tracing::warn!(
                        target: "audit.identity_lifecycle",
                        id = %op.id,
                        error = %format!("{e:#}"),
                        "lifecycle lease renewal failed; trying again at the next beat"
                    ),
                    Ok(()) => {}
                },
            }
        }
    }

    /// How the run ended. The subject's identity is removed only by an
    /// operation that retires it, and only while the lease is still this
    /// run's.
    fn conclude(
        &self,
        operation: &Operation,
        op: &LifecycleOp,
        ended: Ended,
        report: &mut Report,
    ) -> Result<OpStatus, LifecycleError> {
        match ended {
            Ended::Stopped => {
                report.error =
                    Some("The hub stopped before this finished; run it again to resume".into());
                Ok(OpStatus::Incomplete)
            }
            Ended::Visited if report.finished() => {
                if operation.subject == Subject::Identity && operation.steps.retires() {
                    self.hold(op, report)?;
                    self.identities.delete(operation.who.id)?;
                }
                report.complete = true;
                Ok(OpStatus::Complete)
            }
            Ended::Visited => Ok(OpStatus::Incomplete),
        }
    }
}
