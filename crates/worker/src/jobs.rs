//! Durable jobs: commands that belong to the worker rather than to the
//! connection that started them. Each runs detached inside its scope and
//! writes its streams and exit status to the scope's jobs dataset, so it
//! outlives its client and this daemon alike; any client may attach to it
//! again and read on from the bytes it already holds.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use eyre::{bail, Result};
use tokio::sync::watch;
use tokio::task::JoinHandle;
use tracing::{info, warn};
use workforce_agent::tools::exec_runner::{
    validate_overrides, ExecRequest, ExecSummary, JobId, JobLink, Offsets, RefusalReason,
    RunRequest, ScopeId, ServerFrame, Stream,
};

use crate::bridge::Bridge;
use crate::ports::DetachedSpec;
use crate::scopes::{checked_workdir, refused, valid_home_key, Worker, AGENT_USER};
use crate::state::{Finished, JobRecord, Scope, Stop};

/// Where a scope mounts its jobs dataset.
pub const JOBS_ROOT: &str = "/var/lib/workforce/jobs";

/// How long a finished job nobody forgets is kept.
const FORGET_AFTER: chrono::Duration = chrono::Duration::days(1);
/// How often an attached client is sent what a job wrote since.
const STREAM_POLL: Duration = Duration::from_millis(250);
/// How often a running job is looked in on.
const WATCH_TICK: Duration = Duration::from_secs(1);
/// Every how many ticks a job with no exit status is checked for being alive.
const LIVENESS_EVERY: u32 = 30;
/// Time given a job that looks gone to record its exit before it is counted
/// as lost.
const EXIT_GRACE: Duration = Duration::from_secs(2);
const READ_CHUNK: usize = 256 * 1024;

/// A running job's presence in this daemon: who is attached to it, its MCP
/// bridge, and the task watching it once it has started. Dropping it
/// detaches everything from the job, never the job itself.
pub(crate) struct LiveJob {
    attached: Arc<watch::Sender<Option<Arc<JobLink>>>>,
    _bridge: Option<Bridge>,
    watcher: Option<JoinHandle<()>>,
}

impl Drop for LiveJob {
    fn drop(&mut self) {
        if let Some(watcher) = &self.watcher {
            watcher.abort();
        }
    }
}

enum Begun {
    Job(JobRecord),
    Refused(RefusalReason),
}

impl Worker {
    /// Start the job `request` names if it is new and has a command, then
    /// stream it to `link` until it ends. The job keeps running if `link`
    /// goes away first.
    pub async fn run(
        self: &Arc<Self>,
        request: RunRequest,
        link: Arc<JobLink>,
    ) -> Result<ServerFrame> {
        checked_job_id(&request.job)?;
        let known = self.jobs.lock().get(&request.job).cloned();
        let record = match (known, request.exec) {
            (Some(record), _) if record.scope != request.scope => {
                bail!("job {} runs in another scope", record.id)
            }
            (Some(record), _) => record,
            (None, None) => return Ok(refused(RefusalReason::UnknownJob)),
            (None, Some(exec)) => {
                let (worker, job, scope) =
                    (Arc::clone(self), request.job.clone(), request.scope.clone());
                let begun = tokio::spawn(async move { worker.begin(&job, &scope, exec).await });
                match begun.await?? {
                    Begun::Job(record) => record,
                    Begun::Refused(reason) => return Ok(refused(reason)),
                }
            }
        };
        self.attach(&record, request.from, link).await
    }

    /// Start a job, run to completion even when its client goes away
    /// meanwhile, since a start abandoned halfway would leave a job nothing
    /// watches.
    async fn begin(
        self: &Arc<Self>,
        job: &JobId,
        scope_id: &ScopeId,
        exec: ExecRequest,
    ) -> Result<Begun> {
        let _one_at_a_time = self.starts.lock().await;
        if let Some(record) = self.jobs.lock().get(job).cloned() {
            return Ok(Begun::Job(record));
        }
        let Some(scope) = self.touch(scope_id) else {
            return Ok(Begun::Refused(RefusalReason::UnknownScope));
        };
        if !scope.jobs {
            return Ok(Begun::Refused(RefusalReason::JobsUnsupported));
        }
        if self.jobs_running_in(&scope.id) {
            return Ok(Begun::Refused(RefusalReason::ScopeBusy));
        }
        let workdir = checked_workdir(&exec)?;
        validate_overrides(&exec.env).map_err(|reason| eyre::eyre!(reason))?;
        let dir = self.job_dir(&scope, job);
        prepare_job_dir(&dir, exec.stdin.as_deref().unwrap_or_default())?;

        let (attached, attachment) = watch::channel(None);
        let bridge = match exec.mcp.as_ref() {
            Some(binding) => {
                let bridge = Bridge::durable(attachment, None, &self.bridge_ports()).await?;
                self.write_mcp_config(&scope, binding, &bridge)?;
                Some(bridge)
            }
            None => None,
        };
        let now = Utc::now();
        let record = JobRecord {
            id: job.clone(),
            scope: scope.id.clone(),
            started_at: now,
            deadline: now + chrono::Duration::seconds(exec.timeout_secs as i64),
            bridge: bridge.as_ref().map(|bridge| bridge.address().clone()),
            stopped: None,
            finished: None,
        };
        self.live.lock().insert(
            job.clone(),
            LiveJob {
                attached: Arc::new(attached),
                _bridge: bridge,
                watcher: None,
            },
        );
        self.jobs.lock().insert(job.clone(), record.clone());
        if let Err(e) = self.save() {
            self.abandon_start(&scope, job);
            return Err(e);
        }

        let spec = DetachedSpec {
            job: job.to_string(),
            user: AGENT_USER.to_string(),
            workdir,
            program: exec.program,
            args: exec.args,
            env: exec.env,
            dir: format!("{JOBS_ROOT}/{job}"),
        };
        if let Err(e) = self.containers.start_detached(&scope.container, spec).await {
            let _ = self.containers.kill(&scope.container, job.as_str()).await;
            self.abandon_start(&scope, job);
            return Err(e);
        }
        self.keep_watch(&record, &scope);
        info!(job = %job, scope = %scope.id, "job started");
        Ok(Begun::Job(record))
    }

    /// Undo a start that failed: the job is forgotten along with its output.
    fn abandon_start(&self, scope: &Scope, job: &JobId) {
        self.jobs.lock().remove(job);
        self.live.lock().remove(job);
        if let Err(e) = self.save() {
            warn!(job = %job, error = %e, "forgetting a job that did not start");
        }
        let _ = std::fs::remove_dir_all(self.job_dir(scope, job));
    }

    fn keep_watch(self: &Arc<Self>, record: &JobRecord, scope: &Scope) {
        let watcher = tokio::spawn(watch_job(
            Arc::clone(self),
            record.clone(),
            scope.clone(),
            self.job_dir(scope, &record.id),
        ));
        match self.live.lock().get_mut(&record.id) {
            Some(live) => live.watcher = Some(watcher),
            None => watcher.abort(),
        }
    }

    async fn attach(
        self: &Arc<Self>,
        record: &JobRecord,
        from: Offsets,
        link: Arc<JobLink>,
    ) -> Result<ServerFrame> {
        let Some(scope) = self.scope(&record.scope) else {
            return Ok(refused(RefusalReason::UnknownJob));
        };
        let dir = self.job_dir(&scope, &record.id);
        let mut attached = None;
        let mut held = from;
        let mut stopping = false;
        loop {
            if attached.is_none() {
                attached =
                    self.live.lock().get(&record.id).map(|live| {
                        AttachedClient::new(Arc::clone(&live.attached), Arc::clone(&link))
                    });
            }
            let finished = self
                .jobs
                .lock()
                .get(&record.id)
                .map(|record| record.finished.clone());
            let Some(finished) = finished else {
                return Ok(refused(RefusalReason::UnknownJob));
            };
            held = stream_output(&dir, held, finished.is_some(), &link).await?;
            if let Some(finished) = finished {
                return Ok(ServerFrame::Done(finished.summary));
            }
            tokio::select! {
                _ = link.cancel_requested(), if !stopping => {
                    stopping = true;
                    self.stop_job(&record.id, Stop::Cancelled);
                }
                _ = tokio::time::sleep(STREAM_POLL) => {}
            }
        }
    }

    /// End a running job, once: the first reason given is the one its
    /// summary reports.
    fn stop_job(self: &Arc<Self>, job: &JobId, reason: Stop) {
        let scope = {
            let mut jobs = self.jobs.lock();
            let Some(record) = jobs
                .get_mut(job)
                .filter(|record| record.finished.is_none() && record.stopped.is_none())
            else {
                return;
            };
            record.stopped = Some(reason);
            record.scope.clone()
        };
        if let Err(e) = self.save() {
            warn!(job = %job, error = %e, "remembering why a job was stopped");
        }
        let Some(container) = self.scope(&scope).map(|scope| scope.container) else {
            return;
        };
        let (worker, job) = (Arc::clone(self), job.to_string());
        tokio::spawn(async move {
            if let Err(e) = worker.containers.kill(&container, &job).await {
                warn!(job, error = %e, "ending a job");
            }
        });
    }

    /// Record how a job ended; `None` when it ended without an exit status.
    fn finish(&self, job: &JobId, exit_code: Option<i32>) {
        let finished = {
            let mut jobs = self.jobs.lock();
            let Some(record) = jobs.get_mut(job).filter(|record| record.finished.is_none()) else {
                return;
            };
            let summary = ExecSummary {
                exit_code,
                attempts: 1,
                truncated: false,
                timed_out: record.stopped == Some(Stop::Deadline),
            };
            record.finished = Some(Finished {
                at: Utc::now(),
                summary,
            });
            record.scope.clone()
        };
        if let Err(e) = self.save() {
            warn!(job = %job, error = %e, "remembering how a job ended");
        }
        if let Some(scope) = self.scope(&finished) {
            let _ = std::fs::remove_file(self.mcp_config_host_path(&scope));
        }
        self.touch(&finished);
        self.live.lock().remove(job);
        info!(job = %job, ?exit_code, "job ended");
    }

    /// Drop a finished job and its output. A running job is kept: only its
    /// own end or a cancel ends it.
    pub fn forget(&self, job: &JobId) -> Result<ServerFrame> {
        checked_job_id(job)?;
        let forgotten = {
            let mut jobs = self.jobs.lock();
            match jobs.get(job) {
                Some(record) if record.finished.is_some() => jobs.remove(job),
                _ => None,
            }
        };
        if let Some(record) = forgotten {
            self.save()?;
            if let Some(scope) = self.scope(&record.scope) {
                let _ = std::fs::remove_dir_all(self.job_dir(&scope, &record.id));
            }
        }
        Ok(ServerFrame::Done(ExecSummary::exited(0)))
    }

    /// Whether any job in `scope` is still running.
    pub(crate) fn jobs_running_in(&self, scope: &ScopeId) -> bool {
        self.jobs
            .lock()
            .values()
            .any(|record| &record.scope == scope && record.finished.is_none())
    }

    pub fn running_jobs(&self) -> u32 {
        self.jobs
            .lock()
            .values()
            .filter(|record| record.finished.is_none())
            .count() as u32
    }

    /// Forget every job of a scope going away; its container and dataset go
    /// with it.
    pub(crate) fn drop_jobs_of(&self, scope: &ScopeId) {
        let gone: Vec<JobId> = {
            let mut jobs = self.jobs.lock();
            let gone = jobs
                .values()
                .filter(|record| &record.scope == scope)
                .map(|record| record.id.clone())
                .collect::<Vec<_>>();
            gone.iter().for_each(|job| {
                jobs.remove(job);
            });
            gone
        };
        let mut live = self.live.lock();
        gone.iter().for_each(|job| {
            live.remove(job);
        });
    }

    /// Forget finished jobs no client came back for.
    pub(crate) fn sweep_jobs(&self, now: chrono::DateTime<Utc>) -> Result<()> {
        let stale: Vec<JobId> = self
            .jobs
            .lock()
            .values()
            .filter(|record| {
                record
                    .finished
                    .as_ref()
                    .is_some_and(|finished| finished.at + FORGET_AFTER <= now)
            })
            .map(|record| record.id.clone())
            .collect();
        stale.iter().try_for_each(|job| self.forget(job).map(drop))
    }

    /// Take the jobs this daemon remembers back into its care after a
    /// restart: a job that ended meanwhile is recorded as ended, a live one is
    /// watched again and served at its old bridge address, and one that is
    /// gone without an exit status is recorded as lost.
    pub async fn resume_jobs(self: &Arc<Self>) -> Result<()> {
        let running: Vec<JobRecord> = self
            .jobs
            .lock()
            .values()
            .filter(|record| record.finished.is_none())
            .cloned()
            .collect();
        for record in running {
            let Some(scope) = self.scope(&record.scope) else {
                self.drop_jobs_of(&record.scope);
                continue;
            };
            let dir = self.job_dir(&scope, &record.id);
            if let Some(code) = read_exit(&dir) {
                self.finish(&record.id, Some(code));
                continue;
            }
            match self
                .containers
                .job_alive(&scope.container, record.id.as_str())
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    warn!(job = %record.id, "job is gone without an exit status");
                    self.finish(&record.id, None);
                    continue;
                }
                Err(e) => warn!(job = %record.id, error = %e, "checking on a job; watching it"),
            }
            let (attached, attachment) = watch::channel(None);
            let bridge = match record.bridge.clone() {
                Some(address) => Bridge::durable(attachment, Some(address), &[])
                    .await
                    .map_err(
                        |e| warn!(job = %record.id, error = %e, "the job's tools are unreachable"),
                    )
                    .ok(),
                None => None,
            };
            self.live.lock().insert(
                record.id.clone(),
                LiveJob {
                    attached: Arc::new(attached),
                    _bridge: bridge,
                    watcher: None,
                },
            );
            self.keep_watch(&record, &scope);
            info!(job = %record.id, "job resumed");
        }
        self.save()
    }

    fn bridge_ports(&self) -> Vec<u16> {
        self.jobs
            .lock()
            .values()
            .filter_map(|record| record.bridge.as_ref().map(|bridge| bridge.port))
            .collect()
    }

    fn job_dir(&self, scope: &Scope, job: &JobId) -> PathBuf {
        self.host_path(&scope.dataset.child("jobs"))
            .join(job.as_str())
    }
}

/// A job id names a directory and a file inside the scope, so it must be one
/// plain path component.
fn checked_job_id(job: &JobId) -> Result<()> {
    if !valid_home_key(job.as_str()) {
        bail!("job id {:?} is not a plain name", job.as_str());
    }
    Ok(())
}

/// While alive, the client a durable job's tool calls go to.
struct AttachedClient {
    attached: Arc<watch::Sender<Option<Arc<JobLink>>>>,
    link: Arc<JobLink>,
}

impl AttachedClient {
    fn new(attached: Arc<watch::Sender<Option<Arc<JobLink>>>>, link: Arc<JobLink>) -> Self {
        attached.send_replace(Some(Arc::clone(&link)));
        Self { attached, link }
    }
}

impl Drop for AttachedClient {
    fn drop(&mut self) {
        self.attached.send_if_modified(|current| {
            let ours = current
                .as_ref()
                .is_some_and(|current| Arc::ptr_eq(current, &self.link));
            if ours {
                *current = None;
            }
            ours
        });
    }
}

async fn watch_job(worker: Arc<Worker>, record: JobRecord, scope: Scope, dir: PathBuf) {
    let mut ticks = 0u32;
    loop {
        tokio::time::sleep(WATCH_TICK).await;
        ticks = ticks.wrapping_add(1);
        if let Some(code) = read_exit(&dir) {
            worker.finish(&record.id, Some(code));
            return;
        }
        if Utc::now() >= record.deadline {
            worker.stop_job(&record.id, Stop::Deadline);
        }
        if ticks.is_multiple_of(LIVENESS_EVERY) {
            match worker
                .containers
                .job_alive(&scope.container, record.id.as_str())
                .await
            {
                Ok(true) => {}
                Ok(false) => {
                    tokio::time::sleep(EXIT_GRACE).await;
                    let exit = read_exit(&dir);
                    if exit.is_none() {
                        warn!(job = %record.id, "job is gone without an exit status");
                    }
                    worker.finish(&record.id, exit);
                    return;
                }
                Err(e) => warn!(job = %record.id, error = %e, "checking on a job"),
            }
        }
    }
}

fn prepare_job_dir(dir: &Path, stdin: &str) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::create_dir_all(dir)?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o777))?;
    let input = dir.join("stdin");
    std::fs::write(&input, stdin)?;
    std::fs::set_permissions(&input, std::fs::Permissions::from_mode(0o644))?;
    Ok(())
}

fn read_exit(dir: &Path) -> Option<i32> {
    std::fs::read_to_string(dir.join("exit"))
        .ok()
        .and_then(|code| code.trim().parse().ok())
}

/// Send `link` what each stream holds past `held`, and return what it holds
/// after. Once the job has ended the streams are complete, so a trailing
/// incomplete character is sent as a replacement rather than waited for.
async fn stream_output(dir: &Path, held: Offsets, ended: bool, link: &JobLink) -> Result<Offsets> {
    let stdout = stream_file(&dir.join("out"), Stream::Stdout, held.stdout, ended, link).await?;
    let stderr = stream_file(&dir.join("err"), Stream::Stderr, held.stderr, ended, link).await?;
    Ok(Offsets { stdout, stderr })
}

async fn stream_file(
    path: &Path,
    stream: Stream,
    mut offset: u64,
    ended: bool,
    link: &JobLink,
) -> Result<u64> {
    loop {
        let bytes = read_from(path, offset)?;
        let (chunk, consumed) = decode(&bytes, ended);
        if consumed == 0 {
            return Ok(offset);
        }
        let to = offset + consumed as u64;
        link.emit(ServerFrame::Output {
            stream,
            from: offset,
            to,
            chunk,
        })
        .await?;
        offset = to;
        if bytes.len() < READ_CHUNK {
            return Ok(offset);
        }
    }
}

/// Up to [`READ_CHUNK`] bytes of `path` from `offset`; nothing when the file
/// is not there yet.
fn read_from(path: &Path, offset: u64) -> Result<Vec<u8>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e.into()),
    };
    file.seek(SeekFrom::Start(offset))?;
    let mut bytes = Vec::with_capacity(READ_CHUNK);
    file.take(READ_CHUNK as u64).read_to_end(&mut bytes)?;
    Ok(bytes)
}

/// The text `bytes` begin with and how many bytes it covers. An invalid byte
/// becomes a replacement character; an incomplete character at the end is
/// left for the next read unless the stream is complete.
fn decode(bytes: &[u8], complete: bool) -> (String, usize) {
    let mut text = String::new();
    let mut at = 0;
    while at < bytes.len() {
        match std::str::from_utf8(&bytes[at..]) {
            Ok(rest) => {
                text.push_str(rest);
                at = bytes.len();
            }
            Err(e) => {
                let valid = at + e.valid_up_to();
                text.push_str(std::str::from_utf8(&bytes[at..valid]).expect("validated prefix"));
                match e.error_len() {
                    Some(invalid) => {
                        text.push(char::REPLACEMENT_CHARACTER);
                        at = valid + invalid;
                    }
                    None if complete => {
                        text.push(char::REPLACEMENT_CHARACTER);
                        at = bytes.len();
                    }
                    None => return (text, valid),
                }
            }
        }
    }
    (text, at)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::Harness;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;
    use workforce_agent::tools::exec_runner::{HomeSpec, PrepareRequest};

    async fn prepared(h: &Harness) -> Scope {
        prepare_scope(h, "s1").await
    }

    async fn prepare_scope(h: &Harness, scope: &str) -> Scope {
        h.worker
            .prepare(PrepareRequest {
                scope: ScopeId::new(scope),
                repos: Vec::new(),
                ttl_secs: 60,
                env: Vec::new(),
                home: HomeSpec::Ephemeral,
            })
            .await
            .unwrap();
        h.worker.scope(&ScopeId::new(scope)).unwrap()
    }

    fn exec(timeout_secs: u64) -> ExecRequest {
        ExecRequest {
            program: "claude".into(),
            args: vec!["-p".into()],
            workdir: None,
            timeout_secs,
            allow_retry: false,
            env: vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "secret".into())],
            stdin: Some("the task".into()),
            scope: Some(ScopeId::new("s1")),
            mcp: None,
        }
    }

    fn run_request(job: &str, exec: Option<ExecRequest>, from: Offsets) -> RunRequest {
        RunRequest {
            job: JobId::new(job),
            scope: ScopeId::new("s1"),
            exec,
            from,
            timeout_secs: 60,
        }
    }

    struct Attached {
        run: JoinHandle<Result<ServerFrame>>,
        frames: mpsc::Receiver<ServerFrame>,
        cancel: CancellationToken,
        gone: CancellationToken,
    }

    fn attach(h: &Harness, request: RunRequest) -> Attached {
        let (tx, frames) = mpsc::channel(64);
        let (cancel, gone) = (CancellationToken::new(), CancellationToken::new());
        let link = Arc::new(JobLink::for_tests_with_tokens(
            tx,
            cancel.clone(),
            gone.clone(),
        ));
        let worker = Arc::clone(&h.worker);
        Attached {
            run: tokio::spawn(async move { worker.run(request, link).await }),
            frames,
            cancel,
            gone,
        }
    }

    async fn started(h: &Harness) -> PathBuf {
        for _ in 0..200 {
            if let Some(spec) = h.containers.detached().pop() {
                let scope = h.worker.scope(&ScopeId::new("s1")).unwrap();
                return h.worker.job_dir(&scope, &JobId::new(spec.job));
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("the job was never started")
    }

    fn write(dir: &Path, name: &str, content: &str) {
        std::fs::write(dir.join(name), content).unwrap();
    }

    async fn finish(attached: Attached) -> (ServerFrame, String, String) {
        let Attached {
            run, mut frames, ..
        } = attached;
        let done = run.await.unwrap().unwrap();
        let (mut out, mut err) = (String::new(), String::new());
        while let Ok(frame) = frames.try_recv() {
            if let ServerFrame::Output { stream, chunk, .. } = frame {
                match stream {
                    Stream::Stdout => out.push_str(&chunk),
                    Stream::Stderr => err.push_str(&chunk),
                }
            }
        }
        (done, out, err)
    }

    fn exited(code: i32) -> ServerFrame {
        ServerFrame::Done(ExecSummary {
            exit_code: Some(code),
            attempts: 1,
            truncated: false,
            timed_out: false,
        })
    }

    #[tokio::test]
    async fn a_job_starts_detached_and_its_output_and_exit_reach_the_client() {
        let h = Harness::new().await;
        prepared(&h).await;
        let attached = attach(&h, run_request("r1", Some(exec(60)), Offsets::default()));
        let dir = started(&h).await;
        let spec = h.containers.detached().pop().unwrap();
        assert_eq!(spec.dir, format!("{JOBS_ROOT}/r1"));
        assert_eq!(spec.user, "agent");
        assert_eq!(
            std::fs::read_to_string(dir.join("stdin")).unwrap(),
            "the task"
        );
        write(&dir, "out", "hello ");
        write(&dir, "err", "warn");
        tokio::time::sleep(STREAM_POLL * 2).await;
        write(&dir, "out", "hello world");
        write(&dir, "exit", "3\n");

        let (done, out, err) = finish(attached).await;
        assert_eq!(done, exited(3));
        assert_eq!((out.as_str(), err.as_str()), ("hello world", "warn"));
        assert!(h.worker.jobs.lock()[&JobId::new("r1")].finished.is_some());
        assert!(!h.worker.jobs_running_in(&ScopeId::new("s1")));
    }

    #[tokio::test]
    async fn starting_a_job_again_attaches_to_it_and_reads_on_from_the_offsets_held() {
        let h = Harness::new().await;
        prepared(&h).await;
        let first = attach(&h, run_request("r1", Some(exec(60)), Offsets::default()));
        let dir = started(&h).await;
        write(&dir, "out", "hello world");
        tokio::time::sleep(STREAM_POLL * 2).await;
        first.run.abort();

        let again = attach(
            &h,
            run_request(
                "r1",
                Some(exec(60)),
                Offsets {
                    stdout: 6,
                    stderr: 0,
                },
            ),
        );
        write(&dir, "exit", "0");
        let (done, out, _) = finish(again).await;
        assert_eq!(done, exited(0));
        assert_eq!(out, "world");
        assert_eq!(h.containers.detached().len(), 1, "the command ran once");
        assert!(
            h.containers.kills().is_empty(),
            "losing a client ends nothing"
        );
    }

    #[tokio::test]
    async fn a_client_that_goes_away_leaves_its_job_running_for_the_next_one() {
        let h = Harness::new().await;
        prepared(&h).await;
        let first = attach(&h, run_request("r1", Some(exec(60)), Offsets::default()));
        let dir = started(&h).await;
        first.gone.cancel();
        tokio::time::sleep(STREAM_POLL * 4).await;
        assert!(
            h.containers.kills().is_empty(),
            "a lost client is not a cancel"
        );
        first.run.abort();

        let again = attach(&h, run_request("r1", None, Offsets::default()));
        write(&dir, "out", "done");
        write(&dir, "exit", "0");
        let (done, out, _) = finish(again).await;
        assert_eq!((done, out.as_str()), (exited(0), "done"));
        assert_eq!(h.worker.jobs.lock()[&JobId::new("r1")].stopped, None);
    }

    #[tokio::test]
    async fn a_second_job_is_refused_while_one_runs_in_its_scope() {
        let h = Harness::new().await;
        prepared(&h).await;
        let first = attach(&h, run_request("r1", Some(exec(60)), Offsets::default()));
        let dir = started(&h).await;
        let (busy, _, _) = finish(attach(
            &h,
            run_request("r2", Some(exec(60)), Offsets::default()),
        ))
        .await;
        assert_eq!(
            busy,
            ServerFrame::Refused {
                reason: RefusalReason::ScopeBusy
            }
        );
        write(&dir, "exit", "0");
        finish(first).await;
        let second = attach(&h, run_request("r2", Some(exec(60)), Offsets::default()));
        for _ in 0..200 {
            if h.containers.detached().len() == 2 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        second.run.abort();
        assert_eq!(h.containers.detached().len(), 2);
    }

    #[tokio::test]
    async fn a_job_id_that_is_not_a_plain_name_is_refused() {
        let h = Harness::new().await;
        prepared(&h).await;
        for job in ["../../escaped", "a/b", ".hidden", ""] {
            let attached = attach(&h, run_request(job, Some(exec(60)), Offsets::default()));
            assert!(attached.run.await.unwrap().is_err(), "{job:?}");
            assert!(h.worker.forget(&JobId::new(job)).is_err(), "{job:?}");
        }
        assert!(h.containers.detached().is_empty());
    }

    #[tokio::test]
    async fn attaching_without_a_command_never_starts_a_job() {
        let h = Harness::new().await;
        prepared(&h).await;
        let (done, _, _) = finish(attach(&h, run_request("r1", None, Offsets::default()))).await;
        assert_eq!(
            done,
            ServerFrame::Refused {
                reason: RefusalReason::UnknownJob
            }
        );
        assert!(h.containers.detached().is_empty());
    }

    #[tokio::test]
    async fn a_scope_made_before_durable_jobs_refuses_them() {
        let h = Harness::new().await;
        prepared(&h).await;
        h.worker
            .scopes
            .lock()
            .get_mut(&ScopeId::new("s1"))
            .unwrap()
            .jobs = false;
        let (done, _, _) = finish(attach(
            &h,
            run_request("r1", Some(exec(60)), Offsets::default()),
        ))
        .await;
        assert_eq!(
            done,
            ServerFrame::Refused {
                reason: RefusalReason::JobsUnsupported
            }
        );
    }

    #[tokio::test]
    async fn a_cancel_ends_the_job_and_its_summary_is_how_it_ended() {
        let h = Harness::new().await;
        prepared(&h).await;
        let attached = attach(&h, run_request("r1", Some(exec(60)), Offsets::default()));
        let dir = started(&h).await;
        attached.cancel.cancel();
        for _ in 0..200 {
            if !h.containers.kills().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(h.containers.kills().len(), 1);
        write(&dir, "exit", "143");
        let (done, _, _) = finish(attached).await;
        assert_eq!(done, exited(143));
        assert_eq!(
            h.worker.jobs.lock()[&JobId::new("r1")].stopped,
            Some(Stop::Cancelled)
        );
    }

    #[tokio::test]
    async fn a_job_past_its_deadline_is_ended_and_reported_as_timed_out() {
        let h = Harness::new().await;
        prepared(&h).await;
        let attached = attach(&h, run_request("r1", Some(exec(1)), Offsets::default()));
        let dir = started(&h).await;
        for _ in 0..400 {
            if !h.containers.kills().is_empty() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        assert_eq!(h.containers.kills().len(), 1, "ended once, not every tick");
        write(&dir, "exit", "143");
        let (done, _, _) = finish(attached).await;
        assert!(
            matches!(done, ServerFrame::Done(ref s) if s.timed_out && s.exit_code == Some(143))
        );
        tokio::time::sleep(WATCH_TICK * 2).await;
        assert_eq!(h.containers.kills().len(), 1);
    }

    #[tokio::test]
    async fn a_job_whose_container_restarted_ends_with_no_status_and_its_scope_kept() {
        let h = Harness::new().await;
        prepared(&h).await;
        let attached = attach(&h, run_request("r1", Some(exec(600)), Offsets::default()));
        started(&h).await;
        attached.run.abort();
        h.worker.live.lock().clear();
        h.containers
            .mark_unready(&crate::scopes::container_name(&ScopeId::new("s1")));
        h.containers.end_detached("r1");

        let restarted = h.restarted().await;
        restarted.reconcile().await.unwrap();
        restarted.resume_jobs().await.unwrap();

        assert_eq!(restarted.scopes().len(), 1);
        let finished = restarted.jobs.lock()[&JobId::new("r1")].finished.clone();
        assert_eq!(finished.map(|f| f.summary.exit_code), Some(None));
    }

    #[tokio::test]
    async fn a_restarted_worker_takes_its_jobs_back() {
        let h = Harness::with(|config| config.max_scopes = 3).await;
        let mut ended = None;
        for job in ["ended", "running", "lost"] {
            let scope = prepare_scope(&h, job).await;
            let request = RunRequest {
                scope: scope.id.clone(),
                ..run_request(
                    job,
                    Some(ExecRequest {
                        scope: Some(scope.id.clone()),
                        ..exec(600)
                    }),
                    Offsets::default(),
                )
            };
            let attached = attach(&h, request);
            while !h.worker.jobs.lock().contains_key(&JobId::new(job)) {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
            attached.run.abort();
            ended.get_or_insert(scope);
        }
        h.worker.live.lock().clear();

        let restarted = h.restarted().await;
        write(
            &restarted.job_dir(&ended.unwrap(), &JobId::new("ended")),
            "exit",
            "0",
        );
        h.containers.end_detached("lost");
        restarted.reconcile().await.unwrap();
        restarted.resume_jobs().await.unwrap();

        let jobs = restarted.jobs.lock().clone();
        let exit_of = |job: &str| {
            jobs[&JobId::new(job)]
                .finished
                .as_ref()
                .map(|f| f.summary.exit_code)
        };
        assert_eq!(exit_of("ended"), Some(Some(0)));
        assert_eq!(exit_of("lost"), Some(None));
        assert_eq!(exit_of("running"), None);
        assert!(restarted.live.lock().contains_key(&JobId::new("running")));
        assert!(h.containers.kills().is_empty());
    }

    #[tokio::test]
    async fn a_scope_is_kept_while_a_job_runs_and_a_forgotten_job_leaves_nothing() {
        let h = Harness::new().await;
        prepared(&h).await;
        let attached = attach(&h, run_request("r1", Some(exec(600)), Offsets::default()));
        let dir = started(&h).await;
        attached.run.abort();
        let later = Utc::now() + chrono::Duration::hours(1);
        assert!(h.worker.sweep(later).await.unwrap().is_empty());

        write(&dir, "exit", "0");
        let (done, _, _) = finish(attach(&h, run_request("r1", None, Offsets::default()))).await;
        assert_eq!(done, exited(0));
        h.worker.forget(&JobId::new("r1")).unwrap();
        assert!(!dir.exists());
        assert!(h.worker.jobs.lock().is_empty());
        assert!(h.restarted().await.jobs.lock().is_empty());
    }

    #[tokio::test]
    async fn a_finished_job_nobody_forgets_is_dropped_after_a_day() {
        let h = Harness::new().await;
        prepared(&h).await;
        let attached = attach(&h, run_request("r1", Some(exec(600)), Offsets::default()));
        let dir = started(&h).await;
        write(&dir, "exit", "0");
        finish(attached).await;
        h.worker.sweep_jobs(Utc::now()).unwrap();
        assert_eq!(h.worker.jobs.lock().len(), 1);
        h.worker
            .sweep_jobs(Utc::now() + FORGET_AFTER + chrono::Duration::seconds(1))
            .unwrap();
        assert!(h.worker.jobs.lock().is_empty());
        assert!(!dir.exists());
    }

    fn serve_on_socket(worker: Arc<Worker>, socket: &Path, runtime: &tokio::runtime::Handle) {
        let listener = workforce_agent::tools::exec_runner::Listener::Unix {
            path: socket.to_path_buf(),
            accept_uid: current_uid(),
        };
        let handler = Arc::new(crate::handler::WorkerHandler::new(worker));
        runtime.spawn(async move {
            let _ = workforce_agent::tools::exec_runner::serve(listener, handler).await;
        });
    }

    fn current_uid() -> u32 {
        let (a, _b) = std::os::unix::net::UnixStream::pair().unwrap();
        a.set_nonblocking(true).unwrap();
        tokio::net::UnixStream::from_std(a)
            .unwrap()
            .peer_cred()
            .unwrap()
            .uid()
    }

    async fn wait_for(path: &Path) {
        for _ in 0..200 {
            if path.exists() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        panic!("{} never appeared", path.display())
    }

    /// The daemon dies mid-round, every task of it with it, and a fresh one
    /// takes the job back; the client reattaches and gets the whole of the
    /// output once, and the command ran once.
    #[tokio::test]
    async fn a_round_survives_its_daemon_dying_and_a_new_one_taking_over() {
        use workforce_agent::tools::exec_runner::{dispatch_job, DurableJob, RunnerEndpoint};
        let h = Harness::new().await;
        prepared(&h).await;
        let sockets = tempfile::tempdir().unwrap();
        let socket = sockets.path().join("worker.sock");

        let first = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .unwrap();
        serve_on_socket(Arc::clone(&h.worker), &socket, first.handle());
        wait_for(&socket).await;

        let (tx, mut rx) = mpsc::channel(64);
        let endpoint = RunnerEndpoint::Unix(socket.clone());
        let round = tokio::spawn(async move {
            dispatch_job(
                &endpoint,
                DurableJob {
                    job: JobId::new("r1"),
                    scope: ScopeId::new("s1"),
                    exec: Some(exec(600)),
                },
                tx,
                CancellationToken::new(),
                None,
                Duration::from_secs(60),
            )
            .await
        });
        let dir = started(&h).await;
        write(&dir, "out", "hello ");
        tokio::time::sleep(STREAM_POLL * 4).await;
        first.shutdown_background();

        let restarted = h.restarted().await;
        restarted.reconcile().await.unwrap();
        restarted.resume_jobs().await.unwrap();
        serve_on_socket(
            Arc::clone(&restarted),
            &socket,
            &tokio::runtime::Handle::current(),
        );
        write(&dir, "out", "hello world");
        write(&dir, "exit", "0");

        let summary = round.await.unwrap().unwrap();
        assert_eq!(summary.exit_code, Some(0));
        let mut out = String::new();
        while let Ok(frame) = rx.try_recv() {
            if let ServerFrame::Stdout { chunk } = frame {
                out.push_str(&chunk);
            }
        }
        assert_eq!(out, "hello world");
        assert_eq!(h.containers.detached().len(), 1);
        assert!(h.containers.kills().is_empty());
    }

    #[test]
    fn decoding_keeps_a_split_character_for_the_next_read_until_the_stream_is_complete() {
        let text = "héllo".as_bytes();
        assert_eq!(decode(&text[..2], false), ("h".to_string(), 1));
        assert_eq!(decode(text, false), ("héllo".to_string(), text.len()));
        assert_eq!(decode(&text[..2], true), ("h\u{FFFD}".to_string(), 2));
        assert_eq!(decode(b"a\xffb", false), ("a\u{FFFD}b".to_string(), 3));
        assert_eq!(decode(b"", true), (String::new(), 0));
    }
}
