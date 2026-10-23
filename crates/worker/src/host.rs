//! The real ports: `zfs`, `docker`, and `aws` or `gcloud` on the host, run
//! through the execution core so they share its timeout, output cap and
//! process-group handling.

use std::io::Write;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{TimeZone, Utc};
use eyre::{bail, Context, Result};
use tokio::sync::mpsc;
use workforce_agent::tools::exec_runner::{
    run_or_dispatch, EnvMode, ExecOutcome, ExecRequest, ExecSummary, ServerFrame,
};

use crate::config::{fits_one_line, CloudKind, WorkerConfig};
use crate::ports::{
    Cloud, ContainerId, ContainerSpec, Containers, Dataset, DetachedSpec, ExecSpec, Repos,
    Snapshot, SnapshotInfo, Zfs, REPO_ENV_PREFIX,
};

const HOST_COMMAND_TIMEOUT_SECS: u64 = 600;
const REPLICATION_TIMEOUT_SECS: u64 = 6 * 3600;
const STOP_GRACE_SECS: u32 = 10;

/// The scope's run state: its ready markers and its jobs' leaders. A tmpfs,
/// so a container that starts again starts with none from its last run.
const RUN_DIR: &str = "/run/toolbox";
/// Where a job's session id is recorded inside the toolbox.
pub const JOB_DIR: &str = "/run/toolbox/jobs";
/// Shell functions naming a job's leader. A leader is recorded with its start
/// time, so a pid the kernel has since given to another process, after a
/// container restart for one, is not taken for the job.
const JOB_LEADER: &str = r#"started() { stat=$(cat "/proc/$1/stat" 2>/dev/null) || return 1; set -- ${stat##*) }; echo "${20}"; }
leader() { read -r pid start 2>/dev/null < "$1" && [ -n "$start" ] && [ "$(started "$pid")" = "$start" ] && echo "$pid"; }"#;
/// Starts a durable job's program as the leader of its own session, records
/// the leader where toolbox-run would, and waits to record its exit status,
/// which it writes whole or not at all. The waiting shell stays outside the
/// session, so ending the job leaves it to record how the job ended.
const DETACHED_WRAPPER: &str = r#"job=$1 dir=$2
shift 2
setsid "$@" < "$dir/stdin" > "$dir/out" 2> "$dir/err" &
child=$!
echo "$child $(started "$child")" > "/run/toolbox/jobs/$job"
wait "$child"
echo "$?" > "$dir/exit.tmp" && mv "$dir/exit.tmp" "$dir/exit""#;
/// The exit status of a liveness check whose job is gone.
const JOB_GONE: i32 = 3;
const TOOLBOX_RUN: &str = "/usr/local/bin/toolbox-run";
const SYSBOX_RUNTIME: &str = "sysbox-runc";
/// Runs host pipelines under `pipefail`. `bash`, not `sh`: on Debian and
/// Ubuntu `/bin/sh` is dash, which rejects `set -o pipefail` and exits
/// before the pipeline starts.
const PIPEFAIL_SHELL: &str = "bash";
/// Copy stdin (`-`) to an object, one per cloud.
const S3_COPY: &str = "aws s3 cp";
const GCS_COPY: &str = "gcloud storage cp";
const GITHUB_API: &str = "https://api.github.com";
const GITHUB_TIMEOUT: Duration = Duration::from_secs(30);

fn request(program: &str, args: &[String], timeout_secs: u64) -> ExecRequest {
    ExecRequest {
        program: program.to_string(),
        args: args.to_vec(),
        workdir: None,
        timeout_secs,
        allow_retry: false,
        env: Vec::new(),
        stdin: None,
        scope: None,
        mcp: None,
    }
}

async fn run(program: &str, args: &[String], timeout_secs: u64) -> Result<ExecOutcome> {
    execute(request(program, args, timeout_secs)).await
}

async fn execute(request: ExecRequest) -> Result<ExecOutcome> {
    run_or_dispatch(None, request, &[], EnvMode::Inherit { remove: &[] }).await
}

/// Run a host command that must succeed, returning its stdout.
async fn output(program: &str, args: &[String]) -> Result<String> {
    succeed(request(program, args, HOST_COMMAND_TIMEOUT_SECS)).await
}

async fn succeed(request: ExecRequest) -> Result<String> {
    let command = format!("{} {}", request.program, request.args.join(" "));
    let outcome = execute(request).await?;
    if !outcome.success() {
        bail!(
            "{command} failed (exit {}): {}",
            outcome.exit_code(),
            outcome.stderr.trim()
        );
    }
    Ok(outcome.stdout)
}

/// A private `KEY=value` file for `docker --env-file`, so no value appears in
/// a process list. Deleted when dropped, so hold it until docker has read it.
fn env_file(env: &[(String, String)]) -> Result<tempfile::NamedTempFile> {
    if let Some((key, _)) = env.iter().find(|(_, value)| !fits_one_line(value)) {
        bail!("environment variable {key} must be one line without NUL to go in an env file");
    }
    let mut file = tempfile::Builder::new()
        .prefix("wf-env-")
        .tempfile()
        .context("creating the env file")?;
    std::fs::set_permissions(file.path(), std::fs::Permissions::from_mode(0o600))?;
    let text: String = env.iter().map(|(k, v)| format!("{k}={v}\n")).collect();
    file.write_all(text.as_bytes())
        .context("writing the env file")?;
    Ok(file)
}

/// A container's environment split into each repo's settings, which span
/// lines an env file cannot carry, and everything else.
fn split_env(env: &[(String, String)]) -> (Vec<(String, String)>, Vec<(String, String)>) {
    env.iter()
        .cloned()
        .partition(|(key, _)| key.starts_with(REPO_ENV_PREFIX))
}

/// `docker run` for `spec` with its environment read from `env_file`, and
/// `repo_env` named in the arguments but valued in docker's own environment.
fn run_request(
    spec: &ContainerSpec,
    env_file: &Path,
    repo_env: Vec<(String, String)>,
) -> ExecRequest {
    let mut args = strings([
        "run",
        "-d",
        "--runtime",
        SYSBOX_RUNTIME,
        "--name",
        &spec.name,
        "--hostname",
        &spec.name,
        "--label",
        &format!("{}={}", spec.label.0, spec.label.1),
        "--add-host",
        "host.docker.internal:host-gateway",
        "--tmpfs",
        RUN_DIR,
        "--memory",
        &format!("{}g", spec.memory_gib),
        "--cpus",
        &spec.cpus.to_string(),
        "--env-file",
        &env_file.display().to_string(),
    ]);
    args.extend(
        repo_env
            .iter()
            .flat_map(|(key, _)| ["-e".to_string(), key.clone()]),
    );
    args.extend(spec.mounts.iter().flat_map(|mount| {
        let suffix = if mount.read_only { ":ro" } else { "" };
        [
            "-v".to_string(),
            format!("{}:{}{suffix}", mount.host.display(), mount.container),
        ]
    }));
    args.push(spec.image.clone());
    ExecRequest {
        env: repo_env,
        ..request("docker", &args, HOST_COMMAND_TIMEOUT_SECS)
    }
}

fn strings<const N: usize>(parts: [&str; N]) -> Vec<String> {
    parts.iter().map(|s| s.to_string()).collect()
}

/// The program and arguments that run `pipeline` so that a failure anywhere
/// in it, not only in its last command, fails the whole.
fn pipefail_command(pipeline: &str) -> (&'static str, Vec<String>) {
    (
        PIPEFAIL_SHELL,
        strings(["-c", &format!("set -o pipefail; {pipeline}")]),
    )
}

async fn run_pipeline(pipeline: &str, timeout_secs: u64) -> Result<ExecOutcome> {
    let (program, args) = pipefail_command(pipeline);
    run(program, &args, timeout_secs).await
}

/// A raw send of `to`, incremental from `from` when given, received into
/// `into`.
fn replication_pipeline(from: Option<&Snapshot>, to: &Snapshot, into: &Dataset) -> String {
    let send = match from {
        Some(from) => format!("zfs send -w -i '{from}' '{to}'"),
        None => format!("zfs send -w '{to}'"),
    };
    format!("{send} | zfs receive -F '{into}'")
}

/// A raw send of `snapshot`, compressed, streamed to the object `uri`
/// through the cloud's `copy` command.
fn store_pipeline(snapshot: &Snapshot, uri: &str, copy: &str) -> String {
    format!("zfs send -w '{snapshot}' | zstd -T0 -q | {copy} - '{uri}'")
}

async fn store(snapshot: &Snapshot, uri: &str, copy: &str) -> Result<()> {
    let pipeline = store_pipeline(snapshot, uri, copy);
    let outcome = run_pipeline(&pipeline, REPLICATION_TIMEOUT_SECS).await?;
    if !outcome.success() {
        bail!(
            "storing {snapshot} at {uri} failed: {}",
            outcome.stderr.trim()
        );
    }
    Ok(())
}

fn property_args(properties: &[(String, String)]) -> Vec<String> {
    properties
        .iter()
        .flat_map(|(key, value)| ["-o".to_string(), format!("{key}={value}")])
        .collect()
}

pub struct HostZfs;

#[async_trait]
impl Zfs for HostZfs {
    async fn exists(&self, dataset: &Dataset) -> Result<bool> {
        let outcome = run(
            "zfs",
            &strings(["list", "-H", "-o", "name", &dataset.0]),
            HOST_COMMAND_TIMEOUT_SECS,
        )
        .await?;
        Ok(outcome.success())
    }

    async fn create(&self, dataset: &Dataset, properties: &[(String, String)]) -> Result<()> {
        let mut args = vec!["create".to_string(), "-p".to_string()];
        args.extend(property_args(properties));
        args.push(dataset.0.clone());
        output("zfs", &args).await.map(drop)
    }

    async fn children(&self, dataset: &Dataset) -> Result<Vec<Dataset>> {
        let listing = output(
            "zfs",
            &strings(["list", "-H", "-o", "name", "-r", "-d", "1", &dataset.0]),
        )
        .await?;
        Ok(listing
            .lines()
            .map(str::trim)
            .filter(|line| !line.is_empty() && *line != dataset.0)
            .map(|line| Dataset(line.to_string()))
            .collect())
    }

    async fn mountpoint(&self, dataset: &Dataset) -> Result<PathBuf> {
        let value = output(
            "zfs",
            &strings(["get", "-H", "-o", "value", "mountpoint", &dataset.0]),
        )
        .await?;
        Ok(PathBuf::from(value.trim()))
    }

    async fn snapshot(&self, dataset: &Dataset, name: &str) -> Result<Snapshot> {
        let snapshot = dataset.snapshot(name);
        output("zfs", &strings(["snapshot", &snapshot.to_string()])).await?;
        Ok(snapshot)
    }

    async fn snapshots(&self, dataset: &Dataset) -> Result<Vec<SnapshotInfo>> {
        let listing = output(
            "zfs",
            &strings([
                "list",
                "-H",
                "-p",
                "-t",
                "snapshot",
                "-o",
                "name,creation,clones",
                "-s",
                "creation",
                "-d",
                "1",
                &dataset.0,
            ]),
        )
        .await?;
        listing
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(parse_snapshot_line)
            .collect()
    }

    async fn clone_snapshot(
        &self,
        from: &Snapshot,
        to: &Dataset,
        properties: &[(String, String)],
    ) -> Result<()> {
        let mut args = vec!["clone".to_string(), "-p".to_string()];
        args.extend(property_args(properties));
        args.push(from.to_string());
        args.push(to.0.clone());
        output("zfs", &args).await.map(drop)
    }

    async fn destroy(&self, dataset: &Dataset) -> Result<()> {
        output("zfs", &strings(["destroy", "-r", &dataset.0]))
            .await
            .map(drop)
    }

    async fn destroy_snapshot(&self, snapshot: &Snapshot) -> Result<()> {
        output("zfs", &strings(["destroy", &snapshot.to_string()]))
            .await
            .map(drop)
    }

    async fn free_bytes(&self, pool: &str) -> Result<u64> {
        let value = output(
            "zfs",
            &strings(["get", "-Hp", "-o", "value", "available", pool]),
        )
        .await?;
        value
            .trim()
            .parse()
            .with_context(|| format!("parsing free bytes {value:?}"))
    }

    async fn send_receive(
        &self,
        from: Option<&Snapshot>,
        to: &Snapshot,
        into: &Dataset,
    ) -> Result<()> {
        let pipeline = replication_pipeline(from, to, into);
        let outcome = run_pipeline(&pipeline, REPLICATION_TIMEOUT_SECS).await?;
        if !outcome.success() {
            bail!(
                "replicating {to} into {into} failed: {}",
                outcome.stderr.trim()
            );
        }
        Ok(())
    }
}

fn parse_snapshot_line(line: &str) -> Result<SnapshotInfo> {
    let mut fields = line.split('\t');
    let (Some(name), Some(created)) = (fields.next(), fields.next()) else {
        bail!("unexpected zfs snapshot line {line:?}");
    };
    let clones = fields
        .next()
        .filter(|c| *c != "-")
        .map_or(0, |c| c.split(',').count() as u32);
    let (dataset, snapshot) = name
        .split_once('@')
        .ok_or_else(|| eyre::eyre!("snapshot name {name:?} has no @"))?;
    let created = created
        .trim()
        .parse::<i64>()
        .ok()
        .and_then(|secs| Utc.timestamp_opt(secs, 0).single())
        .ok_or_else(|| eyre::eyre!("snapshot creation {created:?} is not epoch seconds"))?;
    Ok(SnapshotInfo {
        snapshot: Dataset(dataset.to_string()).snapshot(snapshot),
        created,
        clones,
    })
}

pub struct HostContainers;

#[async_trait]
impl Containers for HostContainers {
    async fn run(&self, spec: &ContainerSpec) -> Result<ContainerId> {
        let (repo_env, filed) = split_env(&spec.env);
        let env_file = env_file(&filed)?;
        let id = succeed(run_request(spec, env_file.path(), repo_env)).await?;
        Ok(ContainerId(id.trim().to_string()))
    }

    async fn ready(&self, container: &ContainerId) -> Result<bool> {
        let outcome = run(
            "docker",
            &strings(["exec", &container.0, "test", "-f", "/run/toolbox/ready"]),
            30,
        )
        .await?;
        Ok(outcome.success())
    }

    async fn exec(
        &self,
        container: &ContainerId,
        spec: ExecSpec,
        sink: mpsc::Sender<ServerFrame>,
    ) -> Result<ExecSummary> {
        let env_file = env_file(&spec.env)?;
        let mut args = strings([
            "exec",
            "-i",
            "-u",
            &spec.user,
            "-w",
            &spec.workdir,
            "--env-file",
            &env_file.path().display().to_string(),
            &container.0,
            TOOLBOX_RUN,
            &spec.job,
            "--",
            &spec.program,
        ]);
        args.extend(spec.args.iter().cloned());
        let request = ExecRequest {
            stdin: spec.stdin,
            ..request("docker", &args, spec.timeout_secs)
        };
        workforce_agent::tools::exec_runner::run_exec(
            &request,
            &[],
            EnvMode::Inherit { remove: &[] },
            sink,
        )
        .await
    }

    async fn start_detached(&self, container: &ContainerId, spec: DetachedSpec) -> Result<()> {
        let env_file = env_file(&spec.env)?;
        let mut args = strings([
            "exec",
            "-d",
            "-u",
            &spec.user,
            "-w",
            &spec.workdir,
            "--env-file",
            &env_file.path().display().to_string(),
            &container.0,
            "sh",
            "-c",
            &format!("{JOB_LEADER}\n{DETACHED_WRAPPER}"),
            "sh",
            &spec.job,
            &spec.dir,
            &spec.program,
        ]);
        args.extend(spec.args);
        let outcome = run("docker", &args, HOST_COMMAND_TIMEOUT_SECS).await?;
        if !outcome.success() {
            bail!(
                "starting job {} failed: {}",
                spec.job,
                outcome.stderr.trim()
            );
        }
        Ok(())
    }

    /// A container that is not running holds no live job; any other failure
    /// to ask is an error, never a verdict.
    async fn job_alive(&self, container: &ContainerId, job: &str) -> Result<bool> {
        let script = format!("{JOB_LEADER}\nleader {JOB_DIR}/{job} >/dev/null || exit {JOB_GONE}");
        let outcome = run(
            "docker",
            &strings(["exec", &container.0, "sh", "-c", &script]),
            30,
        )
        .await?;
        match outcome.summary.exit_code {
            Some(0) => Ok(true),
            Some(JOB_GONE) => Ok(false),
            _ if outcome.stderr.contains("is not running") => Ok(false),
            _ => bail!("checking on job {job} failed: {}", outcome.stderr.trim()),
        }
    }

    /// A job may be asked to end before its leader is recorded, so the record
    /// is waited for, briefly.
    async fn kill(&self, container: &ContainerId, job: &str) -> Result<()> {
        let script = format!(
            "{JOB_LEADER}\nfor _ in $(seq 50); do [ -s {JOB_DIR}/{job} ] && break; sleep 0.1; done; \
             pid=$(leader {JOB_DIR}/{job}) && kill -TERM -$pid && sleep 5 && kill -KILL -$pid 2>/dev/null; true"
        );
        run(
            "docker",
            &strings(["exec", &container.0, "sh", "-c", &script]),
            30,
        )
        .await
        .map(drop)
    }

    async fn stop(&self, container: &ContainerId) -> Result<()> {
        run(
            "docker",
            &strings(["stop", "-t", &STOP_GRACE_SECS.to_string(), &container.0]),
            60,
        )
        .await?;
        // `-v` takes the container's anonymous volumes with it; without it
        // each removed scope leaves the image's declared volumes behind.
        run("docker", &strings(["rm", "-f", "-v", &container.0]), 60)
            .await
            .map(drop)
    }

    async fn start(&self, container: &ContainerId) -> Result<()> {
        succeed(request("docker", &strings(["start", &container.0]), 60))
            .await
            .map(drop)
    }

    async fn list(&self, label: &str) -> Result<Vec<(ContainerId, String)>> {
        let format = format!("{{{{.ID}}}} {{{{.Label \"{label}\"}}}}");
        let listing = output(
            "docker",
            &strings([
                "ps",
                "-a",
                "--filter",
                &format!("label={label}"),
                "--format",
                &format,
            ]),
        )
        .await?;
        Ok(listing
            .lines()
            .filter_map(|line| line.split_once(' '))
            .map(|(id, value)| (ContainerId(id.to_string()), value.trim().to_string()))
            .collect())
    }

    async fn has_image(&self, image: &str) -> Result<bool> {
        let outcome = run(
            "docker",
            &strings(["image", "inspect", image]),
            HOST_COMMAND_TIMEOUT_SECS,
        )
        .await?;
        Ok(outcome.success())
    }

    async fn pull(&self, image: &str) -> Result<()> {
        output("docker", &strings(["pull", "-q", image]))
            .await
            .map(drop)
    }
}

/// Blob shas through the GitHub contents API.
#[derive(Default)]
pub struct HostRepos {
    client: reqwest::Client,
}

#[async_trait]
impl Repos for HostRepos {
    async fn blob_sha(
        &self,
        slug: &str,
        path: &str,
        branch: &str,
        token: Option<&str>,
    ) -> Result<String> {
        let what = format!("reading {path} on {slug}@{branch}");
        let request = self
            .client
            .get(contents_url(slug, path, branch)?)
            .header(reqwest::header::USER_AGENT, "workforce-worker")
            .header(reqwest::header::ACCEPT, "application/vnd.github+json")
            .timeout(GITHUB_TIMEOUT);
        let request = match token {
            Some(token) => request.bearer_auth(token),
            None => request,
        };
        let response = request.send().await.with_context(|| what.clone())?;
        let status = response.status();
        if !status.is_success() {
            bail!("{what}: GitHub answered {status}");
        }
        let reply: serde_json::Value = response.json().await.with_context(|| what.clone())?;
        file_sha(&reply).ok_or_else(|| eyre::eyre!("{what}: not a file"))
    }
}

fn contents_url(slug: &str, path: &str, branch: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(GITHUB_API)?;
    url.path_segments_mut()
        .map_err(|()| eyre::eyre!("{GITHUB_API} cannot carry a path"))?
        .pop_if_empty()
        .push("repos")
        .extend(slug.split('/'))
        .push("contents")
        .extend(path.split('/'));
    url.query_pairs_mut().append_pair("ref", branch);
    Ok(url)
}

/// The blob sha in a contents reply, when it describes a file.
fn file_sha(reply: &serde_json::Value) -> Option<String> {
    (reply["type"] == "file")
        .then(|| reply["sha"].as_str().map(str::to_string))
        .flatten()
}

/// The cloud `config` names, through its CLI on the host.
pub fn cloud(config: &WorkerConfig) -> Arc<dyn Cloud> {
    match config.cloud {
        CloudKind::Aws => Arc::new(HostCloud),
        CloudKind::Gcp => Arc::new(GcpCloud {
            project: config.gcp_project.clone(),
        }),
    }
}

/// SSM parameters and S3 through `aws`.
pub struct HostCloud;

#[async_trait]
impl Cloud for HostCloud {
    async fn parameter(&self, name: &str) -> Result<String> {
        let value = output("aws", &ssm_parameter_args(name)).await?;
        Ok(secret_value(&value))
    }

    async fn store_snapshot(&self, snapshot: &Snapshot, uri: &str) -> Result<()> {
        store(snapshot, uri, S3_COPY).await
    }
}

/// Secret Manager and GCS through `gcloud`, as whatever account it is
/// authenticated as (the VM's service account, on GCE).
pub struct GcpCloud {
    /// Where bare secret ids resolve; `None` leaves it to gcloud.
    pub project: Option<String>,
}

#[async_trait]
impl Cloud for GcpCloud {
    async fn parameter(&self, name: &str) -> Result<String> {
        let args = secret_access_args(name, self.project.as_deref())?;
        let value = output("gcloud", &args).await?;
        Ok(secret_value(&value))
    }

    async fn store_snapshot(&self, snapshot: &Snapshot, uri: &str) -> Result<()> {
        store(snapshot, uri, GCS_COPY).await
    }
}

fn ssm_parameter_args(name: &str) -> Vec<String> {
    strings([
        "ssm",
        "get-parameter",
        "--name",
        name,
        "--with-decryption",
        "--query",
        "Parameter.Value",
        "--output",
        "text",
    ])
}

/// `gcloud` arguments that print one secret version. `name` is a bare
/// secret id, read at `latest` in `project` when one is given, or a full
/// `projects/<p>/secrets/<id>[/versions/<v>]` resource, which names its own
/// project and version.
fn secret_access_args(name: &str, project: Option<&str>) -> Result<Vec<String>> {
    let malformed = || {
        eyre::eyre!(
            "secret {name:?} is neither a secret id nor projects/<project>/secrets/<id>[/versions/<version>]"
        )
    };
    let (project, secret, version) = match name.strip_prefix("projects/") {
        Some(resource) => match resource.split('/').collect::<Vec<_>>().as_slice() {
            [project, "secrets", secret] => (Some(*project), *secret, "latest"),
            [project, "secrets", secret, "versions", version] => {
                (Some(*project), *secret, *version)
            }
            _ => return Err(malformed()),
        },
        None => (project, name, "latest"),
    };
    if secret.is_empty()
        || secret.contains('/')
        || version.is_empty()
        || project.is_some_and(str::is_empty)
    {
        return Err(malformed());
    }
    let mut args = strings([
        "secrets",
        "versions",
        "access",
        version,
        &format!("--secret={secret}"),
    ]);
    if let Some(project) = project {
        args.push(format!("--project={project}"));
    }
    args.push("--quiet".to_string());
    Ok(args)
}

/// A secret as its CLI printed it, less the trailing newlines.
fn secret_value(stdout: &str) -> String {
    stdout.trim_end_matches('\n').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn container(env: &[(&str, &str)]) -> ContainerSpec {
        ContainerSpec {
            name: "wf-s1".into(),
            image: "registry/toolbox:git_abc".into(),
            label: ("workforce.scope".into(), "s1".into()),
            mounts: vec![crate::ports::Mount {
                host: "/tank/work/s1/rails".into(),
                container: "/work/rails".into(),
                read_only: false,
            }],
            env: env
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
            memory_gib: 8,
            cpus: 4,
        }
    }

    #[test]
    fn run_passes_the_environment_in_a_private_file_not_arguments() {
        let spec = container(&[
            ("RAILS_MASTER_KEY", "dev-master-key"),
            ("TOOLBOX_SERVICES", "mysql,redis"),
            (
                "TOOLBOX_REPO_ENV_RAILS",
                "DISABLE_SPRING=1\nTZ=Europe/London",
            ),
        ]);
        let (repo_env, filed) = split_env(&spec.env);
        let file = env_file(&filed).unwrap();
        let request = run_request(&spec, file.path(), repo_env);

        assert!(
            request
                .args
                .iter()
                .all(|arg| !arg.contains("dev-master-key") && !arg.contains("Europe/London")),
            "{:?}",
            request.args
        );
        let path = file.path().display().to_string();
        assert!(request
            .args
            .windows(2)
            .any(|pair| pair == ["--env-file", path.as_str()]));
        assert!(request
            .args
            .windows(2)
            .any(|pair| pair == ["-e", "TOOLBOX_REPO_ENV_RAILS"]));
        assert!(request
            .args
            .windows(2)
            .any(|pair| pair == ["--tmpfs", RUN_DIR]));
        assert_eq!(
            request.env,
            vec![(
                "TOOLBOX_REPO_ENV_RAILS".to_string(),
                "DISABLE_SPRING=1\nTZ=Europe/London".to_string()
            )]
        );
        assert_eq!(
            request.args.last().map(String::as_str),
            Some("registry/toolbox:git_abc")
        );
        assert_eq!(
            std::fs::read_to_string(file.path()).unwrap(),
            "RAILS_MASTER_KEY=dev-master-key\nTOOLBOX_SERVICES=mysql,redis\n"
        );
        assert_eq!(
            std::fs::metadata(file.path()).unwrap().permissions().mode() & 0o777,
            0o600
        );
    }

    #[test]
    fn an_env_file_refuses_a_value_it_cannot_carry_naming_only_the_variable() {
        for value in ["line one\nline two", "a\rb", "nul\0"] {
            let error = env_file(&[("SECRET".to_string(), value.to_string())])
                .unwrap_err()
                .to_string();
            assert!(
                error.contains("SECRET") && !error.contains(value),
                "{error}"
            );
        }
    }

    #[test]
    fn the_contents_url_encodes_each_segment_and_the_branch() {
        assert_eq!(
            contents_url("acme/core-api", "gradle/libs.versions.toml", "main")
                .unwrap()
                .as_str(),
            "https://api.github.com/repos/acme/core-api/contents/gradle/libs.versions.toml?ref=main"
        );
        assert_eq!(
            contents_url("acme/app", "odd name#.lock", "release/1.2")
                .unwrap()
                .as_str(),
            "https://api.github.com/repos/acme/app/contents/odd%20name%23.lock?ref=release%2F1.2"
        );
    }

    #[test]
    fn only_a_file_reply_yields_a_blob_sha() {
        let file = serde_json::json!({"type": "file", "sha": "abc123", "path": "Gemfile.lock"});
        assert_eq!(file_sha(&file).as_deref(), Some("abc123"));
        let directory = serde_json::json!([{"type": "file", "sha": "abc123"}]);
        assert_eq!(file_sha(&directory), None);
        let submodule = serde_json::json!({"type": "submodule", "sha": "abc123"});
        assert_eq!(file_sha(&submodule), None);
    }

    #[test]
    fn a_snapshot_line_parses_with_its_clone_count() {
        let info = parse_snapshot_line(
            "tank/golden/rails@2026-09-08T02\t1757296800\ttank/work/a/rails,tank/work/b/rails",
        )
        .unwrap();
        assert_eq!(info.snapshot.to_string(), "tank/golden/rails@2026-09-08T02");
        assert_eq!(info.clones, 2);
        assert_eq!(info.created.timestamp(), 1757296800);
        let unused = parse_snapshot_line("tank/golden/rails@x\t1757296800\t-").unwrap();
        assert_eq!(unused.clones, 0);
    }

    #[test]
    fn a_malformed_snapshot_line_is_an_error() {
        assert!(parse_snapshot_line("tank/golden/rails\tnot-a-time").is_err());
    }

    #[test]
    fn replication_runs_under_bash_because_dash_has_no_pipefail() {
        let a = Dataset("tank/golden/rails".into()).snapshot("a");
        let b = Dataset("tank/golden/rails".into()).snapshot("b");
        let into = Dataset("backup/golden/rails".into());
        assert_eq!(
            pipefail_command(&replication_pipeline(None, &a, &into)),
            (
                "bash",
                strings([
                    "-c",
                    "set -o pipefail; zfs send -w 'tank/golden/rails@a' | zfs receive -F 'backup/golden/rails'",
                ])
            )
        );
        assert_eq!(
            pipefail_command(&replication_pipeline(Some(&a), &b, &into)),
            (
                "bash",
                strings([
                    "-c",
                    "set -o pipefail; zfs send -w -i 'tank/golden/rails@a' 'tank/golden/rails@b' | zfs receive -F 'backup/golden/rails'",
                ])
            )
        );
    }

    #[test]
    fn a_bare_secret_id_reads_latest_in_the_configured_project_or_gclouds_default() {
        assert_eq!(
            secret_access_args("github-read-token", Some("contoso")).unwrap(),
            strings([
                "secrets",
                "versions",
                "access",
                "latest",
                "--secret=github-read-token",
                "--project=contoso",
                "--quiet",
            ])
        );
        assert_eq!(
            secret_access_args("github-read-token", None).unwrap(),
            strings([
                "secrets",
                "versions",
                "access",
                "latest",
                "--secret=github-read-token",
                "--quiet",
            ])
        );
    }

    #[test]
    fn storing_to_the_bucket_runs_under_bash_because_dash_has_no_pipefail() {
        let snapshot = Dataset("tank/golden/rails".into()).snapshot("a");
        assert_eq!(
            pipefail_command(&store_pipeline(
                &snapshot,
                "s3://bucket/golden/rails@a.zst",
                S3_COPY
            )),
            (
                "bash",
                strings([
                    "-c",
                    "set -o pipefail; zfs send -w 'tank/golden/rails@a' | zstd -T0 -q | aws s3 cp - 's3://bucket/golden/rails@a.zst'",
                ])
            )
        );
        assert_eq!(
            pipefail_command(&store_pipeline(
                &snapshot,
                "gs://bucket/golden/rails@a.zst",
                GCS_COPY
            )),
            (
                "bash",
                strings([
                    "-c",
                    "set -o pipefail; zfs send -w 'tank/golden/rails@a' | zstd -T0 -q | gcloud storage cp - 'gs://bucket/golden/rails@a.zst'",
                ])
            )
        );
    }

    #[test]
    fn a_full_secret_resource_names_its_own_project_and_version() {
        assert_eq!(
            secret_access_args("projects/other/secrets/rails-key", Some("contoso")).unwrap(),
            strings([
                "secrets",
                "versions",
                "access",
                "latest",
                "--secret=rails-key",
                "--project=other",
                "--quiet",
            ])
        );
        assert_eq!(
            secret_access_args("projects/other/secrets/rails-key/versions/3", None).unwrap(),
            strings([
                "secrets",
                "versions",
                "access",
                "3",
                "--secret=rails-key",
                "--project=other",
                "--quiet",
            ])
        );
    }

    #[test]
    fn a_malformed_secret_name_is_an_error_not_a_bare_id() {
        for name in [
            "projects/other/rails-key",
            "projects//secrets/rails-key",
            "projects/other/secrets/",
            "projects/other/secrets/rails-key/versions/",
            "projects/other/secrets/rails-key/versions/3/extra",
            "a/b",
            "",
        ] {
            assert!(secret_access_args(name, None).is_err(), "{name:?}");
        }
    }

    #[test]
    fn a_secret_value_loses_trailing_newlines_and_keeps_other_whitespace() {
        assert_eq!(
            secret_value("  token with spaces\t\n"),
            "  token with spaces\t"
        );
        assert_eq!(secret_value("line one\nline two"), "line one\nline two");
        assert_eq!(
            ssm_parameter_args("/p")[..3],
            strings(["ssm", "get-parameter", "--name"])
        );
    }
}
