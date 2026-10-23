//! What runs as the person: scheduled jobs and reminders, definitions and
//! their runs, sub-agent jobs, heartbeat checks and learning plans.

use std::sync::Arc;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use eyre::Result;
use workforce_core::access::OutgoingAccess;
use workforce_orchestrator::workflow::instance::InstanceStatus;
use workforce_orchestrator::workflow_store::{
    TransferFailed, WorkflowStore, KIND_CONSOLIDATION_TEMPLATE, KIND_MONITOR, KIND_PLAYBOOK,
    KIND_WORKFLOW,
};

use super::{Context, Holding, Item, LifecycleError, Movable, Owners, Pausable, Person, Tally};
use crate::hub_core::schedules::ScheduledJob;
use crate::serve::AgentService;

/// The kinds of definition a member can own.
const OWNED_KINDS: [&str; 4] = [
    KIND_WORKFLOW,
    KIND_PLAYBOOK,
    KIND_MONITOR,
    KIND_CONSOLIDATION_TEMPLATE,
];

/// Who holds a run's execution lease while an operation rewrites or
/// removes it, so no replica executes it meanwhile.
const LEASE_OWNER: &str = "identity-lifecycle";
const LEASE_SECS: i64 = 60;

/// The engine-owned variables naming who a run acts as and bills.
const RUN_AS: &str = "_run_as";
const BILL_TO: &str = "_bill_to";

pub struct ScheduledJobs;

impl ScheduledJobs {
    /// Per agent: the jobs the person owns, and the jobs others own that run
    /// as, bill or were last saved by them.
    async fn split(
        cx: &Context<'_>,
        who: &Person,
    ) -> Vec<(
        Arc<AgentService>,
        Arc<dyn crate::hosts::SchedulerHost>,
        Vec<ScheduledJob>,
        Vec<ScheduledJob>,
    )> {
        let mut found = Vec::new();
        for service in cx.services() {
            let Some(scheduler) = service.scheduler.clone() else {
                continue;
            };
            let (owned, referenced) = scheduler
                .jobs_naming(&who.names)
                .await
                .into_iter()
                .partition(|job| job.owned_by(&who.names));
            found.push((service, scheduler, owned, referenced));
        }
        found
    }
}

/// Who a delete acts on in `job`: its owner, who loses it, and anyone else
/// it runs as or bills, whom it stops naming. Who last saved it stays as a
/// record.
fn job_names(job: &ScheduledJob) -> std::collections::BTreeSet<String> {
    job.owner()
        .into_iter()
        .chain(
            [&job.run_as_identity_id, &job.bill_to_identity_id]
                .into_iter()
                .flatten()
                .map(String::as_str),
        )
        .filter(|name| !name.is_empty())
        .map(str::to_string)
        .collect()
}

#[async_trait]
impl Holding for ScheduledJobs {
    /// Creator, run-as and bill-to are written from the signed-in identity
    /// ([`crate::user_config::current_identity_id`]) or by an administrator
    /// naming one, so they only ever hold identity ids.
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        let mut owners = Owners::default();
        for scheduler in cx.services().iter().filter_map(|s| s.scheduler.clone()) {
            for job in scheduler.current_jobs().await {
                owners.absorb(Owners::of(job_names(&job)));
                owners.absorb(Owners::proving(
                    [
                        &job.creator_identity_id,
                        &job.run_as_identity_id,
                        &job.bill_to_identity_id,
                    ]
                    .into_iter()
                    .flatten()
                    .cloned(),
                ));
            }
        }
        Ok(owners)
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        let mut tally = Tally::default();
        for (service, _, owned, referenced) in Self::split(cx, who).await {
            for job in owned {
                let label = format!("{}: {}", service.agent_id, job.name);
                tally.item(label.clone());
                tally.transferable += 1;
                tally.movable.push(Movable {
                    label,
                    item: Item::Job {
                        agent: service.agent_id.to_string(),
                        id: job.id,
                    },
                });
            }
            for job in referenced {
                tally.item(format!(
                    "{}: {} (another's, names them)",
                    service.agent_id, job.name
                ));
            }
        }
        Ok(tally)
    }

    /// The same person now goes by the identity merged into, so their jobs
    /// keep running as them, and a job that was theirs alone stays so.
    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        let tally = self.held(cx, from).await?;
        if apply {
            for scheduler in cx.services().iter().filter_map(|s| s.scheduler.clone()) {
                scheduler
                    .reassign_jobs(None, &from.names, &into.key(), None)
                    .await;
            }
            release_held_jobs(cx, true).await;
        }
        Ok(tally)
    }

    /// Their own jobs stop and are deleted with their run history, or go to
    /// the recipient as the administrator's edit, so a job runs without the
    /// recipient's private context until they save it themselves. Jobs other
    /// people own stay theirs and stop running as or billing the person.
    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        to: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        let tally = self.held(cx, who).await?;
        if !apply {
            return Ok(tally);
        }
        for (_, scheduler, owned, _) in Self::split(cx, who).await {
            let ids: Vec<String> = owned.into_iter().map(|job| job.id).collect();
            match to {
                Some(to) => {
                    scheduler
                        .reassign_jobs(Some(&ids), &who.names, &to.key(), Some(&cx.actor.name()))
                        .await;
                }
                None => {
                    scheduler.forget_jobs(&ids).await;
                }
            }
            scheduler.release_jobs(&who.names).await;
        }
        release_held_jobs(cx, true).await;
        Ok(tally)
    }
}

/// Every job a suspension held whose person is no longer suspended, on
/// every agent, released: the jobs a reactivation releases, and the jobs a
/// merge or a delete moved on to someone who is not suspended.
async fn release_held_jobs(cx: &Context<'_>, apply: bool) -> Tally {
    let mut tally = Tally::default();
    for service in cx.services() {
        let Some(scheduler) = service.scheduler.clone() else {
            continue;
        };
        for job in scheduler
            .release_suspension_holds(&|person| service.suspended(person).is_some(), apply)
            .await
        {
            tally.item(format!("{}: {}", service.agent_id, job.name));
        }
    }
    tally
}

/// A suspended person's jobs are switched off with a mark saying the
/// suspension did it, so a hub build that does not know the mark leaves
/// them off too; reactivation switches on exactly the marked ones.
#[async_trait]
impl Pausable for ScheduledJobs {
    async fn suspend(&self, cx: &Context<'_>, who: &Person, apply: bool) -> Result<Tally> {
        let mut tally = Tally::default();
        for service in cx.services() {
            let Some(scheduler) = service.scheduler.clone() else {
                continue;
            };
            for job in scheduler.hold_for_suspension(&who.names, apply).await {
                tally.item(format!("{}: {}", service.agent_id, job.name));
            }
        }
        Ok(tally)
    }

    async fn resume(&self, cx: &Context<'_>, _: &Person, apply: bool) -> Result<Tally> {
        Ok(release_held_jobs(cx, apply).await)
    }
}

pub struct Definitions;

/// A definition the person owns: `(kind, id, owner as stored, run_as)`.
struct Owned {
    kind: &'static str,
    id: String,
    owner: String,
    run_as: Option<String>,
}

impl Definitions {
    async fn owned(store: &WorkflowStore, who: &Person) -> Result<Vec<Owned>> {
        let mut owned = Vec::new();
        for kind in OWNED_KINDS {
            for def in store.list(kind).await? {
                if let Some(owner) = def.owner.filter(|o| who.names.contains(o)) {
                    owned.push(Owned {
                        kind,
                        id: def.id,
                        owner,
                        run_as: def.run_as,
                    });
                }
            }
        }
        Ok(owned)
    }

    async fn references(store: &WorkflowStore, who: &Person) -> Result<Vec<String>> {
        let mut found = Vec::new();
        for name in &who.names {
            for (kind, id, owner) in store.references_to(name).await? {
                found.push(format!("{kind}:{id} (owned by {owner})"));
            }
        }
        Ok(found)
    }

    async fn tally(store: &WorkflowStore, who: &Person) -> Result<Tally> {
        let owned = Self::owned(store, who).await?;
        let mut tally = Tally::of(owned.iter().map(|d| format!("{}:{}", d.kind, d.id)));
        tally.transferable = tally.count;
        tally.movable = owned
            .iter()
            .filter(|d| d.owner == who.key())
            .map(|d| Movable {
                label: format!("{}:{}", d.kind, d.id),
                item: def_item(d.kind, &d.id),
            })
            .collect();
        tally.absorb(Tally::of(Self::references(store, who).await?));
        Ok(tally)
    }
}

#[async_trait]
impl Holding for Definitions {
    /// A member definition's owner is the identity that saved it; the run
    /// identities and grants on others' definitions are counted beside.
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        let Some(store) = &cx.defs else {
            return Ok(Owners::default());
        };
        let mut owners = Owners::default();
        for kind in OWNED_KINDS {
            owners.absorb(
                Owners::of(
                    store
                        .list(kind)
                        .await?
                        .into_iter()
                        .filter_map(|def| def.owner),
                )
                .of_identities(),
            );
        }
        owners.absorb(Owners::counted(store.reference_holders().await?));
        Ok(owners)
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        match &cx.defs {
            Some(store) => Self::tally(store, who).await,
            None => Ok(Tally::default()),
        }
    }

    /// Moved by [`WorkflowStore::transfer`], then given back the run identity
    /// it had, so it keeps running as the same person. Both owning one under
    /// the same id blocks: an id is inside its definition and its public
    /// trigger URLs, so an administrator decides which goes.
    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        let Some(store) = &cx.defs else {
            return Ok(Tally::default());
        };
        let mut tally = Self::tally(store, from).await?;
        let to = into.key();
        for def in Self::owned(store, from).await? {
            let clash = store.get(&def.id, def.kind, Some(&to)).await?.is_some();
            if clash {
                tally.blocking.push(format!(
                    "{}:{} is owned by both; delete or transfer one, then merge again",
                    def.kind, def.id
                ));
                continue;
            }
            if apply {
                transfer(store, &def, &to, &cx.actor.name()).await?;
                let run_as = def.run_as.filter(|r| !from.names.contains(r));
                store
                    .adopt_merged(def.kind, &def.id, &to, run_as.as_deref())
                    .await?;
            }
        }
        if apply {
            for name in &from.names {
                store.reassign_references(name, &to).await?;
            }
            reload_definitions(cx).await;
        }
        Ok(tally)
    }

    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        to: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        let Some(store) = &cx.defs else {
            return Ok(Tally::default());
        };
        let mut tally = Self::tally(store, who).await?;
        if !apply {
            return Ok(tally);
        }
        for def in Self::owned(store, who).await? {
            match to {
                Some(to) => match transfer(store, &def, &to.key(), &cx.actor.name()).await {
                    Err(LifecycleError::Refused(why)) => tally.blocking.push(why),
                    other => other?,
                },
                None => {
                    store.erase(def.kind, &def.id, &def.owner).await?;
                }
            }
        }
        for name in &who.names {
            store.drop_references(name).await?;
        }
        reload_definitions(cx).await;
        Ok(tally)
    }
}

/// A suspended person's definitions are refused at every trigger as they
/// fire; their schedule triggers are unregistered by a sync and registered
/// again by the sync after reactivation, on this replica now and on the
/// others when their maintenance next runs.
#[async_trait]
impl Pausable for Definitions {
    async fn suspend(&self, cx: &Context<'_>, who: &Person, apply: bool) -> Result<Tally> {
        self.resync(cx, who, apply).await
    }

    async fn resume(&self, cx: &Context<'_>, who: &Person, apply: bool) -> Result<Tally> {
        self.resync(cx, who, apply).await
    }
}

impl Definitions {
    async fn resync(&self, cx: &Context<'_>, who: &Person, apply: bool) -> Result<Tally> {
        let Some(store) = &cx.defs else {
            return Ok(Tally::default());
        };
        let tally = Tally::of(
            Self::owned(store, who)
                .await?
                .into_iter()
                .map(|d| format!("{}:{}", d.kind, d.id)),
        );
        if apply {
            reload_definitions(cx).await;
        }
        Ok(tally)
    }
}

fn def_item(kind: &str, id: &str) -> Item {
    let id = id.to_string();
    match kind {
        KIND_PLAYBOOK => Item::Playbook { id },
        KIND_MONITOR => Item::Monitor { id },
        KIND_CONSOLIDATION_TEMPLATE => Item::ConsolidationTemplate { id },
        _ => Item::Workflow { id },
    }
}

async fn transfer(
    store: &WorkflowStore,
    def: &Owned,
    to: &str,
    granted_by: &str,
) -> Result<(), LifecycleError> {
    store
        .transfer(
            def.kind,
            &def.id,
            &def.owner,
            to,
            OutgoingAccess::Nothing,
            granted_by,
        )
        .await
        .map(|_| ())
        .map_err(|failed| match failed {
            TransferFailed::NotFound => {
                LifecycleError::NotFound(format!("No {} {} to transfer", def.kind, def.id))
            }
            TransferFailed::Occupied => LifecycleError::Refused(format!(
                "The recipient already owns a {} named {}",
                def.kind, def.id
            )),
            TransferFailed::Storage(e) => LifecycleError::Failed(e),
        })
}

/// Make this replica's runtimes see the definitions as they now stand; the
/// others notice on their next poll of the store.
async fn reload_definitions(cx: &Context<'_>) {
    let Ok(hub) = cx.hub() else {
        return;
    };
    let reloads = [
        match hub.workflow_runtime() {
            Some(runtime) => runtime.reload_definitions().await.err(),
            None => None,
        },
        match hub.playbook_runtime() {
            Some(runtime) => runtime.reload().await.err(),
            None => None,
        },
        match hub.monitor_engine() {
            Some(engine) => engine.definitions().reload().await.err(),
            None => None,
        },
    ];
    for error in reloads.into_iter().flatten() {
        tracing::warn!(error = %format!("{error:#}"), "reloading definitions after a lifecycle change failed");
    }
}

/// Give one item to `to` as `cx.actor`'s edit.
pub(super) async fn transfer_item(
    cx: &Context<'_>,
    from: &Person,
    to: &Person,
    item: &Item,
) -> Result<Tally, LifecycleError> {
    let (kind, id) = match item {
        Item::Job { agent, id } => {
            let scheduler = cx
                .registry
                .get(agent)
                .and_then(|service| service.scheduler.clone())
                .ok_or_else(|| LifecycleError::NotFound(format!("No agent {agent}")))?;
            let job = scheduler
                .get_job(id)
                .await
                .filter(|job| job.owned_by(&from.names))
                .ok_or_else(|| {
                    LifecycleError::NotFound(format!("{} has no job {id}", from.display))
                })?;
            scheduler
                .reassign_jobs(
                    Some(std::slice::from_ref(id)),
                    &from.names,
                    &to.key(),
                    Some(&cx.actor.name()),
                )
                .await;
            return Ok(Tally::of([format!("{agent}: {}", job.name)]));
        }
        Item::Workflow { id } => (KIND_WORKFLOW, id),
        Item::Playbook { id } => (KIND_PLAYBOOK, id),
        Item::Monitor { id } => (KIND_MONITOR, id),
        Item::ConsolidationTemplate { id } => (KIND_CONSOLIDATION_TEMPLATE, id),
    };
    let store = cx
        .defs
        .as_ref()
        .ok_or_else(|| LifecycleError::NotFound("This hub keeps no member definitions".into()))?;
    let def = Owned {
        kind,
        id: id.clone(),
        owner: from.key(),
        run_as: None,
    };
    transfer(store, &def, &to.key(), &cx.actor.name()).await?;
    reload_definitions(cx).await;
    Ok(Tally::of([format!("{kind}:{id}")]))
}

pub struct WorkflowRuns;

/// One run executed as, billed to, or of a definition owned by the person.
struct Run {
    row: workforce_common::persistence::WorkflowInstanceRow,
    terminal: bool,
}

impl WorkflowRuns {
    /// Visit every run a page at a time, so memory stays bounded however
    /// many there are.
    fn each(cx: &Context<'_>, mut visit: impl FnMut(Run)) -> Result<()> {
        let telemetry = Arc::clone(&cx.hub()?.telemetry_store);
        let (mut after, mut after_id) = (DateTime::<Utc>::UNIX_EPOCH, String::new());
        loop {
            let page = telemetry.workflow_instances_after(after, &after_id, 500)?;
            let Some(last) = page.last() else {
                return Ok(());
            };
            after = instance_time(&last.updated_at)?;
            after_id = last.id.clone();
            for row in page {
                visit(Run {
                    terminal: row
                        .status
                        .parse::<InstanceStatus>()
                        .is_ok_and(|s| s.is_terminal()),
                    row,
                });
            }
        }
    }

    async fn runs(cx: &Context<'_>, who: &Person) -> Result<Vec<Run>> {
        let mut found = Vec::new();
        Self::each(cx, |run| {
            if run_names(&run.row, &who.names) {
                found.push(run);
            }
        })?;
        Ok(found)
    }

    fn tally(runs: &[Run]) -> Tally {
        Tally::of(
            runs.iter()
                .map(|run| format!("{} ({})", run.row.workflow_id, run.row.status)),
        )
    }
}

/// When a run was last written. A run cancelled on SQLite before this
/// build carries SQLite's own `YYYY-MM-DD HH:MM:SS` form.
pub(super) fn instance_time(raw: &str) -> Result<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(raw)
        .map(|t| t.with_timezone(&Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(raw, "%Y-%m-%d %H:%M:%S").map(|t| t.and_utc())
        })
        .map_err(|e| eyre::eyre!("unreadable instance timestamp {raw}: {e}"))
}

/// Who a delete acts on in a run: whom it runs as, and the owner of its
/// definition when it runs as nobody else or is still open (a delete of
/// that owner cancels it). Whom it bills is a billing record.
fn run_owners(
    row: &workforce_common::persistence::WorkflowInstanceRow,
    terminal: bool,
) -> std::collections::BTreeSet<String> {
    let run_as = principal(row, RUN_AS);
    let owner = row
        .workflow_id
        .split_once('/')
        .map(|(owner, _)| owner.to_string())
        .filter(|_| run_as.is_none() || !terminal);
    run_as.into_iter().chain(owner).collect()
}

fn run_names(row: &workforce_common::persistence::WorkflowInstanceRow, names: &[String]) -> bool {
    run_principal(row, RUN_AS, names)
        || run_principal(row, BILL_TO, names)
        || of_their_definition(row, names)
}

/// Whether the run is of a definition the person owns.
fn of_their_definition(
    row: &workforce_common::persistence::WorkflowInstanceRow,
    names: &[String],
) -> bool {
    row.workflow_id
        .split_once('/')
        .is_some_and(|(owner, _)| names.iter().any(|name| name == owner))
}

/// Whether the run acts as the person: it runs as them, or it is of their
/// definition and acts as nobody else. A run someone else started of a
/// definition they share acts as that someone, and is theirs.
fn run_is_theirs(
    row: &workforce_common::persistence::WorkflowInstanceRow,
    names: &[String],
) -> bool {
    run_principal(row, RUN_AS, names)
        || (of_their_definition(row, names) && principal(row, RUN_AS).is_none())
}

fn principal(
    row: &workforce_common::persistence::WorkflowInstanceRow,
    key: &str,
) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(&row.variables)
        .ok()?
        .get(key)?
        .as_str()
        .filter(|id| !id.is_empty())
        .map(str::to_string)
}

fn run_principal(
    row: &workforce_common::persistence::WorkflowInstanceRow,
    key: &str,
    names: &[String],
) -> bool {
    principal(row, key).is_some_and(|id| names.contains(&id))
}

/// Move the open run read as `row` from `names` to `to` while holding its
/// lease, and only if it is still as it was read. Why it is left for the
/// next pass, when it is.
pub(super) fn merge_run(
    telemetry: &dyn workforce_common::telemetry::TelemetryStore,
    mut row: workforce_common::persistence::WorkflowInstanceRow,
    names: &[String],
    to: &str,
) -> Result<Option<String>> {
    if !telemetry.try_claim_workflow_instance(&row.id, LEASE_OWNER, LEASE_SECS)? {
        return Ok(Some(format!("run {} is executing", row.id)));
    }
    let version = row.version;
    reassign_run(&mut row, names, to);
    let saved = telemetry.save_workflow_instance_fenced(&row, version);
    telemetry.release_workflow_instance(&row.id, LEASE_OWNER)?;
    Ok((!saved?).then(|| format!("run {} changed; try again", row.id)))
}

/// Rewrite every principal and the owner prefix naming `names` to `to`.
fn reassign_run(
    row: &mut workforce_common::persistence::WorkflowInstanceRow,
    names: &[String],
    to: &str,
) {
    if let Ok(mut variables) = serde_json::from_str::<serde_json::Value>(&row.variables) {
        for key in [RUN_AS, BILL_TO] {
            let named = variables
                .get(key)
                .and_then(|v| v.as_str())
                .is_some_and(|id| names.iter().any(|n| n == id));
            if named {
                variables[key] = serde_json::json!(to);
            }
        }
        row.variables = variables.to_string();
    }
    if let Some((owner, id)) = row.workflow_id.split_once('/') {
        if names.iter().any(|n| n == owner) {
            row.workflow_id = format!("{to}/{id}");
        }
    }
}

#[async_trait]
impl Holding for WorkflowRuns {
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        let mut owners = Owners::default();
        Self::each(cx, |run| {
            owners.absorb(Owners::of(run_owners(&run.row, run.terminal)));
        })?;
        Ok(owners)
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        Ok(Self::tally(&Self::runs(cx, who).await?))
    }

    /// An open run goes on as the identity merged into: its principal and
    /// its definition's address are rewritten while this operation holds its
    /// execution lease, and only if the run is still as it was read. A run
    /// some replica is executing right now, or one that changed meanwhile, is
    /// left for the next pass. Finished runs stay as the record they are.
    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        let runs = Self::runs(cx, from).await?;
        let mut tally = Self::tally(&runs);
        if !apply {
            return Ok(tally);
        }
        let telemetry = Arc::clone(&cx.hub()?.telemetry_store);
        for run in runs.into_iter().filter(|run| !run.terminal) {
            if let Some(left) = merge_run(telemetry.as_ref(), run.row, &from.names, &into.key())? {
                tally.blocking.push(left);
            }
        }
        Ok(tally)
    }

    /// Every run that acts as them goes: its variables and step outputs are
    /// theirs. A run of someone else's that only bills them stays, as the
    /// billing record it is, and so does a run someone else started of a
    /// definition of theirs, which acts as that someone: it is cancelled if
    /// still open, because its definition is going. One a replica is
    /// executing is cancelled and removed on the next pass, once it has
    /// stopped. These writes bypass the steering gate deliberately: they are
    /// an administrator's lifecycle operation, not a steer.
    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        let runs = Self::runs(cx, who).await?;
        let mut tally = Self::tally(&runs);
        if !apply {
            return Ok(tally);
        }
        let telemetry = Arc::clone(&cx.hub()?.telemetry_store);
        for run in runs {
            let theirs = run_is_theirs(&run.row, &who.names);
            let orphaned = !theirs && !run.terminal && of_their_definition(&run.row, &who.names);
            if !(theirs || orphaned) {
                continue;
            }
            let free = run.terminal
                || telemetry.try_claim_workflow_instance(&run.row.id, LEASE_OWNER, LEASE_SECS)?;
            match (theirs, free) {
                (true, true) => {
                    telemetry.delete_workflow_instance(&run.row.id)?;
                }
                (true, false) => {
                    cancel(
                        telemetry.as_ref(),
                        &run.row.id,
                        "the identity it ran as was deleted",
                    )?;
                    tally
                        .blocking
                        .push(format!("run {} was executing and is cancelled", run.row.id));
                }
                (false, _) => {
                    cancel(
                        telemetry.as_ref(),
                        &run.row.id,
                        "the owner of its workflow was deleted",
                    )?;
                    if free {
                        telemetry.release_workflow_instance(&run.row.id, LEASE_OWNER)?;
                    }
                    tally.conflicts.push(format!(
                        "run {} acts as someone else, so it is kept and cancelled",
                        run.row.id
                    ));
                }
            }
        }
        Ok(tally)
    }
}

/// A suspended person's runs are held before their next step, by the
/// runtime that executes them, which needs nothing from here. Reactivation
/// resumes the runs the suspension held and no others, now on this replica,
/// and on every maintenance tick anywhere.
#[async_trait]
impl Pausable for WorkflowRuns {
    async fn suspend(&self, cx: &Context<'_>, who: &Person, _: bool) -> Result<Tally> {
        let runs = Self::runs(cx, who).await?;
        Ok(Self::tally(
            &runs
                .into_iter()
                .filter(|run| !run.terminal && run_is_theirs(&run.row, &who.names))
                .collect::<Vec<_>>(),
        ))
    }

    async fn resume(&self, cx: &Context<'_>, who: &Person, apply: bool) -> Result<Tally> {
        let runs = Self::runs(cx, who).await?;
        let tally = Self::tally(
            &runs
                .into_iter()
                .filter(|run| {
                    run_is_theirs(&run.row, &who.names)
                        && serde_json::from_str(&run.row.variables)
                            .is_ok_and(|vars| crate::workflow_runtime::held_for_suspension(&vars))
                })
                .collect::<Vec<_>>(),
        );
        if apply {
            if let Some(runtime) = cx.hub()?.workflow_runtime() {
                Arc::clone(runtime).resume_suspension_held().await;
            }
        }
        Ok(tally)
    }
}

fn cancel(
    telemetry: &dyn workforce_common::telemetry::TelemetryStore,
    id: &str,
    reason: &str,
) -> Result<()> {
    telemetry.cancel_workflow_instance(id, "cancelled", "_cancellation_reason", reason)?;
    Ok(())
}

pub struct SubagentJobs;

impl SubagentJobs {
    fn runners(cx: &Context<'_>) -> Vec<(Arc<AgentService>, Arc<dyn crate::hosts::JobHost>)> {
        cx.services()
            .into_iter()
            .filter_map(|service| service.job_runner.clone().map(|runner| (service, runner)))
            .collect()
    }
}

#[async_trait]
impl Holding for SubagentJobs {
    /// The jobs run as each person; one that only bills them stays.
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        let mut owners = Owners::default();
        for (_, runner) in Self::runners(cx) {
            owners.absorb(Owners::counted(runner.people().await?));
        }
        Ok(owners)
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        let mut tally = Tally::default();
        for (service, runner) in Self::runners(cx) {
            for job in runner.jobs_for(&who.names).await? {
                tally.item(format!("{}: {}", service.agent_id, job.job_id));
            }
        }
        Ok(tally)
    }

    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        let tally = self.held(cx, from).await?;
        if apply {
            for (_, runner) in Self::runners(cx) {
                runner.reassign_person(&from.names, &into.key()).await?;
            }
        }
        Ok(tally)
    }

    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        let mut tally = self.held(cx, who).await?;
        if apply {
            for (_, runner) in Self::runners(cx) {
                let (_, running) = runner.forget_person(&who.names).await?;
                tally.blocking.extend(
                    running
                        .into_iter()
                        .map(|id| format!("job {id} was running and is cancelled")),
                );
            }
        }
        Ok(tally)
    }
}

/// A background job is one run, not a schedule: a suspended person's
/// running or queued ones of every kind (a sub-agent's, or any other they
/// submitted) are cancelled, and nothing restarts them.
#[async_trait]
impl Pausable for SubagentJobs {
    async fn suspend(&self, cx: &Context<'_>, who: &Person, apply: bool) -> Result<Tally> {
        let mut tally = Tally::default();
        for (service, runner) in Self::runners(cx) {
            for job in runner.active_run_as(&who.names).await? {
                tally.item(format!("{}: {}", service.agent_id, job.job_id));
                if apply {
                    runner.cancel(&job.job_id).await?;
                }
            }
        }
        Ok(tally)
    }

    async fn resume(&self, _: &Context<'_>, _: &Person, _: bool) -> Result<Tally> {
        Ok(Tally::default())
    }
}

pub struct HeartbeatChecks;

impl HeartbeatChecks {
    /// Re-stamp every check `who` created on every agent as `editor`'s, or,
    /// without one, as nobody's and paused: a check with no creator runs as
    /// the agent itself, so the person's instruction must not start doing
    /// that until someone re-enables it and so takes it on as their own.
    fn restamp(cx: &Context<'_>, who: &Person, editor: Option<&str>, apply: bool) -> Result<Tally> {
        let mut tally = Tally::default();
        for service in cx.services() {
            let checks =
                workforce_common::heartbeat_check::list_heartbeat_checks(&service.workspace)?;
            for mut check in checks.into_iter().filter(|check| {
                check
                    .meta
                    .creator
                    .as_ref()
                    .is_some_and(|c| who.names.contains(c))
            }) {
                tally.item(format!("{}: {}", service.agent_id, check.id));
                if apply {
                    check.meta.enabled &= editor.is_some();
                    crate::heartbeat_signature::stamp_creator(
                        service.check_signer.as_deref(),
                        &mut check,
                        editor,
                    );
                    workforce_common::heartbeat_check::save_heartbeat_check(
                        &service.workspace,
                        &check,
                    )?;
                }
            }
        }
        Ok(tally)
    }
}

#[async_trait]
impl Holding for HeartbeatChecks {
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        let mut owners = Owners::default();
        for service in cx.services() {
            owners.absorb(Owners::of(
                workforce_common::heartbeat_check::list_heartbeat_checks(&service.workspace)?
                    .into_iter()
                    .filter_map(|check| check.meta.creator),
            ));
        }
        Ok(owners)
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        Self::restamp(cx, who, None, false)
    }

    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        Self::restamp(cx, from, Some(&into.key()), apply)
    }

    /// The check is the agent's and other people may rely on it, so it
    /// stays, paused and no longer the person's: never run with their
    /// credentials, nor as the agent until someone re-enables it.
    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        Self::restamp(cx, who, None, apply)
    }
}

pub struct LearningPlans;

impl LearningPlans {
    fn rewrite(cx: &Context<'_>, who: &Person, to: Option<&str>, apply: bool) -> Result<Tally> {
        let mut tally = Tally::default();
        for service in cx.services() {
            let plans = workforce_common::learning_plan::list_learning_plans(&service.workspace)?;
            for mut plan in plans.into_iter().filter(|plan| {
                plan.meta
                    .run_as_identity
                    .as_ref()
                    .is_some_and(|r| who.names.contains(r))
            }) {
                tally.item(format!("{}: {}", service.agent_id, plan.id));
                if apply {
                    plan.meta.run_as_identity = to.map(str::to_string);
                    workforce_common::learning_plan::write_learning_plan(
                        &service.workspace,
                        &plan,
                    )?;
                }
            }
        }
        Ok(tally)
    }
}

#[async_trait]
impl Holding for LearningPlans {
    async fn owners(&self, cx: &Context<'_>) -> Result<Owners> {
        let mut owners = Owners::default();
        for service in cx.services() {
            owners.absorb(Owners::of(
                workforce_common::learning_plan::list_learning_plans(&service.workspace)?
                    .into_iter()
                    .filter_map(|plan| plan.meta.run_as_identity),
            ));
        }
        Ok(owners)
    }

    async fn held(&self, cx: &Context<'_>, who: &Person) -> Result<Tally> {
        Self::rewrite(cx, who, None, false)
    }

    async fn merge(
        &self,
        cx: &Context<'_>,
        from: &Person,
        into: &Person,
        apply: bool,
    ) -> Result<Tally> {
        Self::rewrite(cx, from, Some(&into.key()), apply)
    }

    /// The plan stays the agent's and no longer runs with the person's
    /// credentials.
    async fn delete(
        &self,
        cx: &Context<'_>,
        who: &Person,
        _: Option<&Person>,
        apply: bool,
    ) -> Result<Tally> {
        Self::rewrite(cx, who, None, apply)
    }
}
