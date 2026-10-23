//! Post-completion quality gate for `/cc` turns.
//!
//! After a turn ends cleanly, run the checkout's own "did this break the
//! build?" checks (`cargo fmt --check`, the JS typecheck) inside the round's
//! sandbox and surface one Slack card with the verdict. The gate reports; it
//! never retries, because an auto-retry loop is a costly footgun on a flaky
//! check. A toolchain the checkout needs but the sandbox lacks is a failure
//! naming the tool, never a silent pass.

use std::collections::HashSet;
use std::path::Path;
use std::time::Duration;

use super::sandbox_git::{repo_dirs, RepoDir};
use crate::coding_sandbox::{CodingSandbox, EntryKind, SandboxCommand};

#[derive(Debug, Clone)]
pub struct RepoGateReport {
    pub repo: String,
    pub outcome: GateOutcome,
}

#[derive(Debug, Clone)]
pub enum GateOutcome {
    /// The checkout has no gates.
    Skipped,
    Passed {
        ran: Vec<String>,
    },
    Failed(GateFailure),
}

#[derive(Debug, Clone)]
pub struct GateFailure {
    pub gate: String,
    pub exit_code: i32,
    pub output_tail: String,
}

#[derive(Debug, Clone)]
pub struct GateCommand {
    pub label: String,
    pub program: String,
    pub args: Vec<String>,
}

impl GateCommand {
    fn new(label: &str, program: &str, args: &[&str]) -> Self {
        Self {
            label: label.to_string(),
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
        }
    }
}

const GATE_TIMEOUT: Duration = Duration::from_secs(300);
const OUTPUT_TAIL_BYTES: usize = 2_400;
/// The exit code a shell gives a command it cannot find.
const NOT_FOUND: i32 = 127;
const PACKAGE_JSON_MAX_BYTES: usize = 256 * 1024;

fn env_flag(name: &str, default: bool) -> bool {
    flag(std::env::var(name).ok().as_deref(), default)
}

/// A switch as the environment sets it: on unless empty, `0` or `false`.
fn flag(value: Option<&str>, default: bool) -> bool {
    match value {
        Some(v) => !v.is_empty() && v != "0" && !v.eq_ignore_ascii_case("false"),
        None => default,
    }
}

fn enabled_globally() -> bool {
    env_flag("WORKFORCE_CC_QUALITY_GATE", true)
}

fn heavy_gates() -> bool {
    env_flag("WORKFORCE_CC_QUALITY_GATE_HEAVY", false)
}

/// What the gate can see of a checkout without running anything in it.
#[derive(Debug, Default)]
pub struct RepoFiles {
    names: HashSet<String>,
    package_json: Option<String>,
    local_tsc: bool,
}

impl RepoFiles {
    pub async fn probe(sandbox: &dyn CodingSandbox, dir: &Path) -> Self {
        let names: HashSet<String> = sandbox
            .list(dir)
            .await
            .unwrap_or_default()
            .into_iter()
            .map(|e| e.name)
            .collect();
        let package_json = match names.contains("package.json") {
            true => sandbox
                .read(&dir.join("package.json"), PACKAGE_JSON_MAX_BYTES)
                .await
                .ok()
                .flatten(),
            false => None,
        };
        let local_tsc = sandbox
            .list(&dir.join("node_modules/.bin"))
            .await
            .map(|entries| {
                entries
                    .iter()
                    .any(|e| e.name == "tsc" && e.kind != EntryKind::Dir)
            })
            .unwrap_or(false);
        Self {
            names,
            package_json,
            local_tsc,
        }
    }

    fn has(&self, name: &str) -> bool {
        self.names.contains(name)
    }
}

/// The checks a checkout with these files gets.
pub fn gates_for(files: &RepoFiles) -> Vec<GateCommand> {
    let mut out = Vec::new();
    if files.has("Cargo.toml") {
        out.push(GateCommand::new(
            "cargo fmt --check",
            "cargo",
            &["fmt", "--check"],
        ));
        if heavy_gates() {
            out.push(GateCommand::new(
                "cargo clippy",
                "cargo",
                &["clippy", "--all-targets", "--", "-D", "warnings"],
            ));
            out.push(GateCommand::new(
                "cargo test",
                "cargo",
                &["test", "--workspace"],
            ));
        }
    }
    if let Some(gate) = files
        .package_json
        .as_deref()
        .and_then(|package_json| js_typecheck_gate(files, package_json))
    {
        out.push(gate);
    }
    out
}

fn js_typecheck_gate(files: &RepoFiles, package_json: &str) -> Option<GateCommand> {
    if has_typecheck_script(package_json) {
        if let Some(pm) = js_package_manager(files) {
            return Some(GateCommand::new(
                &format!("{pm} run typecheck"),
                pm,
                &["run", "typecheck"],
            ));
        }
    }
    files
        .local_tsc
        .then(|| GateCommand::new("tsc --noEmit", "node_modules/.bin/tsc", &["--noEmit"]))
}

fn has_typecheck_script(package_json: &str) -> bool {
    serde_json::from_str::<serde_json::Value>(package_json)
        .ok()
        .and_then(|parsed| {
            parsed
                .get("scripts")?
                .get("typecheck")?
                .as_str()
                .map(|s| !s.trim().is_empty())
        })
        .unwrap_or(false)
}

fn js_package_manager(files: &RepoFiles) -> Option<&'static str> {
    [
        ("pnpm-lock.yaml", "pnpm"),
        ("yarn.lock", "yarn"),
        ("package-lock.json", "npm"),
    ]
    .into_iter()
    .find_map(|(lock, pm)| files.has(lock).then_some(pm))
}

/// Run every checkout's gates in the sandbox; a checkout with none is left
/// out of the report.
pub async fn run_in(sandbox: &dyn CodingSandbox) -> Vec<RepoGateReport> {
    if !enabled_globally() {
        return Vec::new();
    }
    let mut reports = Vec::new();
    for repo in repo_dirs(sandbox).await {
        if let Some(report) = run_for_one_repo(sandbox, &repo).await {
            reports.push(report);
        }
    }
    reports
}

async fn run_for_one_repo(sandbox: &dyn CodingSandbox, repo: &RepoDir) -> Option<RepoGateReport> {
    let gates = gates_for(&RepoFiles::probe(sandbox, &repo.path).await);
    if gates.is_empty() {
        return None;
    }
    let mut ran = Vec::with_capacity(gates.len());
    for gate in gates {
        if let Err(failure) = run_one_gate(sandbox, &repo.path, &gate).await {
            return Some(RepoGateReport {
                repo: repo.alias.clone(),
                outcome: GateOutcome::Failed(failure),
            });
        }
        ran.push(gate.label);
    }
    Some(RepoGateReport {
        repo: repo.alias.clone(),
        outcome: GateOutcome::Passed { ran },
    })
}

async fn run_one_gate(
    sandbox: &dyn CodingSandbox,
    repo: &Path,
    gate: &GateCommand,
) -> Result<(), GateFailure> {
    let failure = |exit_code, output_tail: String| GateFailure {
        gate: gate.label.clone(),
        exit_code,
        output_tail,
    };
    let command = SandboxCommand::new(&gate.program, gate.args.iter().cloned())
        .cwd(repo)
        .timeout(GATE_TIMEOUT);
    let out = sandbox.exec(command).await.map_err(|e| {
        failure(
            NOT_FOUND,
            format!(
                "`{}` could not run where this round runs: {e}",
                gate.program
            ),
        )
    })?;
    if out.summary.timed_out {
        return Err(failure(
            -1,
            format!("gate timed out after {}s", GATE_TIMEOUT.as_secs()),
        ));
    }
    match out.exit_code() {
        0 => Ok(()),
        NOT_FOUND => Err(failure(
            NOT_FOUND,
            format!("`{}` is not installed where this round runs", gate.program),
        )),
        code => Err(failure(
            code,
            crate::claude_code::tail_utf8(
                &format!("{}\n{}", out.stdout, out.stderr),
                OUTPUT_TAIL_BYTES,
            ),
        )),
    }
}

pub fn summarise(reports: &[RepoGateReport]) -> GateSummary {
    let mut passed: Vec<String> = Vec::new();
    let mut failed: Vec<&RepoGateReport> = Vec::new();
    for r in reports {
        match &r.outcome {
            GateOutcome::Passed { .. } => passed.push(r.repo.clone()),
            GateOutcome::Failed(_) => failed.push(r),
            GateOutcome::Skipped => {}
        }
    }
    if !failed.is_empty() {
        GateSummary::Failed {
            failures: failed.iter().map(|&r| r.clone()).collect(),
        }
    } else if !passed.is_empty() {
        GateSummary::Passed { repos: passed }
    } else {
        GateSummary::NothingToDo
    }
}

#[derive(Debug, Clone)]
pub enum GateSummary {
    NothingToDo,
    Passed { repos: Vec<String> },
    Failed { failures: Vec<RepoGateReport> },
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;
    use tokio::process::Command;

    use crate::coding_sandbox::adopt_workdir;

    async fn gate(dir: &Path) -> Vec<RepoGateReport> {
        run_in(adopt_workdir(dir.to_path_buf(), Vec::new(), None).as_ref()).await
    }

    async fn probe(dir: &Path) -> RepoFiles {
        RepoFiles::probe(
            adopt_workdir(dir.to_path_buf(), Vec::new(), None).as_ref(),
            Path::new("."),
        )
        .await
    }

    #[test]
    fn a_switch_is_off_when_set_to_nothing_zero_or_false() {
        for off in ["", "0", "false", "FALSE"] {
            assert!(!flag(Some(off), true), "{off:?}");
        }
        assert!(flag(Some("1"), false));
        assert!(flag(None, true));
        assert!(!flag(None, false));
    }

    #[tokio::test]
    async fn skipped_when_no_toolchain_markers() {
        // A bare git repo with no Cargo.toml / package.json should
        // produce no reports at all — the runner has nothing to gate.
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().join("repo");
        tokio::fs::create_dir_all(&workdir).await.unwrap();
        run_git_init(&workdir).await;
        let reports = gate(&workdir).await;
        assert!(reports.is_empty());
    }

    #[tokio::test]
    async fn cargo_fmt_passes_on_well_formatted_rust() {
        // Skip when cargo isn't installed on the test machine —
        // CI/dev workstations always have it; this guard stops a
        // bare-bones environment from spuriously failing the test.
        if Command::new("cargo")
            .arg("--version")
            .output()
            .await
            .ok()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().join("crate");
        tokio::fs::create_dir_all(&workdir).await.unwrap();
        run_git_init(&workdir).await;
        tokio::fs::write(
            workdir.join("Cargo.toml"),
            "[package]\nname = \"f\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(workdir.join("src"))
            .await
            .unwrap();
        tokio::fs::write(
            workdir.join("src/lib.rs"),
            "pub fn x() -> u32 {\n    1\n}\n",
        )
        .await
        .unwrap();

        let reports = gate(&workdir).await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].repo, "crate");
        match &reports[0].outcome {
            GateOutcome::Passed { ran } => {
                assert!(ran.iter().any(|s| s.contains("fmt")));
            }
            other => panic!("expected Passed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn cargo_fmt_fails_on_misformatted_rust_and_captures_output() {
        if Command::new("cargo")
            .arg("--version")
            .output()
            .await
            .ok()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            return;
        }
        let tmp = TempDir::new().unwrap();
        let workdir = tmp.path().join("crate");
        tokio::fs::create_dir_all(&workdir).await.unwrap();
        run_git_init(&workdir).await;
        tokio::fs::write(
            workdir.join("Cargo.toml"),
            "[package]\nname = \"f\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(workdir.join("src"))
            .await
            .unwrap();
        // Deliberately ugly indentation so rustfmt's --check fails.
        tokio::fs::write(workdir.join("src/lib.rs"), "pub fn x() -> u32 {     1\n}\n")
            .await
            .unwrap();

        let reports = gate(&workdir).await;
        assert_eq!(reports.len(), 1);
        match &reports[0].outcome {
            GateOutcome::Failed(failure) => {
                assert!(failure.gate.contains("fmt"));
                assert_ne!(failure.exit_code, 0);
            }
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn project_bound_iterates_per_repo_subdirs() {
        // session_root with two subdirs, only one is a Rust crate:
        // we should get a single report for the Rust one and nothing
        // for the bare subdir.
        let tmp = TempDir::new().unwrap();
        let session_root = tmp.path().join("cc-session");
        tokio::fs::create_dir_all(&session_root).await.unwrap();

        let rust = session_root.join("rails");
        tokio::fs::create_dir_all(&rust).await.unwrap();
        run_git_init(&rust).await;
        tokio::fs::write(
            rust.join("Cargo.toml"),
            "[package]\nname = \"r\"\nversion = \"0.0.0\"\nedition = \"2021\"\n",
        )
        .await
        .unwrap();
        tokio::fs::create_dir_all(rust.join("src")).await.unwrap();
        tokio::fs::write(rust.join("src/lib.rs"), "pub fn x() -> u32 {\n    1\n}\n")
            .await
            .unwrap();

        let bare = session_root.join("docs");
        tokio::fs::create_dir_all(&bare).await.unwrap();
        run_git_init(&bare).await;
        tokio::fs::write(bare.join("README"), "hello")
            .await
            .unwrap();

        // Force-skip when cargo isn't available in the test env.
        if Command::new("cargo")
            .arg("--version")
            .output()
            .await
            .ok()
            .map(|o| !o.status.success())
            .unwrap_or(true)
        {
            return;
        }

        let reports = gate(&session_root).await;
        assert_eq!(reports.len(), 1);
        assert_eq!(reports[0].repo, "rails");
    }

    #[tokio::test]
    async fn summarise_returns_failed_when_any_repo_failed() {
        let reports = vec![
            RepoGateReport {
                repo: "a".into(),
                outcome: GateOutcome::Passed {
                    ran: vec!["fmt".into()],
                },
            },
            RepoGateReport {
                repo: "b".into(),
                outcome: GateOutcome::Failed(GateFailure {
                    gate: "fmt".into(),
                    exit_code: 1,
                    output_tail: "diff".into(),
                }),
            },
        ];
        match summarise(&reports) {
            GateSummary::Failed { failures } => assert_eq!(failures.len(), 1),
            other => panic!("expected Failed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn summarise_returns_passed_when_all_pass() {
        let reports = vec![RepoGateReport {
            repo: "a".into(),
            outcome: GateOutcome::Passed {
                ran: vec!["fmt".into()],
            },
        }];
        match summarise(&reports) {
            GateSummary::Passed { repos } => assert_eq!(repos, vec!["a".to_string()]),
            other => panic!("expected Passed, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn summarise_returns_nothing_to_do_when_only_skipped() {
        let reports = vec![RepoGateReport {
            repo: "a".into(),
            outcome: GateOutcome::Skipped,
        }];
        assert!(matches!(summarise(&reports), GateSummary::NothingToDo));
    }

    #[test]
    fn tail_keeps_last_bytes_after_newline_boundary() {
        let s = "alpha\nbeta\ngamma\n";
        let t = crate::claude_code::tail_utf8(s, 7);
        assert!(t.starts_with("…\n"));
        assert!(t.ends_with("gamma\n"));
    }

    #[tokio::test]
    async fn js_gate_skipped_when_no_node_modules() {
        // package.json present but no node_modules — the prior version
        // crashed with `npx canceled … tsc@2.0.4`. Should now produce
        // no gate at all.
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("frontend");
        std::fs::create_dir_all(&repo).unwrap();
        std::fs::write(repo.join("package.json"), r#"{"name":"f"}"#).unwrap();
        let gates = gates_for(&probe(&repo).await);
        assert!(
            gates.is_empty(),
            "expected no JS gate without node_modules, got {gates:?}"
        );
    }

    #[tokio::test]
    async fn js_gate_uses_local_tsc_when_node_modules_present() {
        // node_modules/.bin/tsc exists, no typecheck script → invoke
        // the binary directly by absolute path.
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("frontend");
        std::fs::create_dir_all(repo.join("node_modules").join(".bin")).unwrap();
        std::fs::write(repo.join("package.json"), r#"{"name":"f"}"#).unwrap();
        std::fs::write(repo.join("node_modules/.bin/tsc"), "#!/bin/sh\nexit 0\n").unwrap();
        let gates = gates_for(&probe(&repo).await);
        assert_eq!(gates.len(), 1);
        assert_eq!(gates[0].label, "tsc --noEmit");
        assert!(
            gates[0].program.ends_with("node_modules/.bin/tsc"),
            "expected absolute local tsc path, got {}",
            gates[0].program
        );
        assert_eq!(gates[0].args, vec!["--noEmit".to_string()]);
    }

    #[tokio::test]
    async fn js_gate_prefers_typecheck_script_when_present() {
        // pnpm-lock + package.json with a `typecheck` script → run
        // `pnpm run typecheck`, even when node_modules/.bin/tsc also
        // exists. Turbo-shaped repos need this — a root tsc would
        // typecheck effectively nothing.
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("frontend");
        std::fs::create_dir_all(repo.join("node_modules").join(".bin")).unwrap();
        std::fs::write(
            repo.join("package.json"),
            r#"{"name":"f","scripts":{"typecheck":"turbo typecheck"}}"#,
        )
        .unwrap();
        std::fs::write(repo.join("pnpm-lock.yaml"), "lockfileVersion: 9.0\n").unwrap();
        std::fs::write(repo.join("node_modules/.bin/tsc"), "#!/bin/sh\nexit 0\n").unwrap();
        let gates = gates_for(&probe(&repo).await);
        assert_eq!(gates.len(), 1);
        assert_eq!(gates[0].label, "pnpm run typecheck");
        assert_eq!(gates[0].program, "pnpm");
        assert_eq!(
            gates[0].args,
            vec!["run".to_string(), "typecheck".to_string()]
        );
    }

    #[tokio::test]
    async fn js_gate_typecheck_script_without_lockfile_falls_back_to_tsc() {
        // No lockfile → can't pick a package manager, so the typecheck
        // script preference is unreachable. Fall through to the direct
        // tsc path if the binary is there.
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("frontend");
        std::fs::create_dir_all(repo.join("node_modules").join(".bin")).unwrap();
        std::fs::write(
            repo.join("package.json"),
            r#"{"scripts":{"typecheck":"tsc"}}"#,
        )
        .unwrap();
        std::fs::write(repo.join("node_modules/.bin/tsc"), "#!/bin/sh\nexit 0\n").unwrap();
        let gates = gates_for(&probe(&repo).await);
        assert_eq!(gates.len(), 1);
        assert_eq!(gates[0].label, "tsc --noEmit");
    }

    #[tokio::test]
    async fn a_missing_toolchain_fails_the_gate_by_name() {
        let tmp = TempDir::new().unwrap();
        let sandbox = adopt_workdir(tmp.path().to_path_buf(), Vec::new(), None);
        let gate = GateCommand::new(
            "typecheck",
            "workforce-test-tool-that-does-not-exist",
            &["run", "typecheck"],
        );
        let failure = run_one_gate(sandbox.as_ref(), Path::new("."), &gate)
            .await
            .unwrap_err();
        assert_eq!(failure.gate, "typecheck");
        assert_eq!(failure.exit_code, NOT_FOUND);
        assert!(
            failure
                .output_tail
                .contains("workforce-test-tool-that-does-not-exist"),
            "{}",
            failure.output_tail
        );
    }

    #[tokio::test]
    async fn a_local_tsc_gate_runs_the_checkouts_own_binary_from_its_directory() {
        use std::os::unix::fs::PermissionsExt;
        let tmp = TempDir::new().unwrap();
        let repo = tmp.path().join("frontend");
        std::fs::create_dir_all(repo.join("node_modules/.bin")).unwrap();
        std::fs::write(repo.join("package.json"), r#"{"name":"f"}"#).unwrap();
        let tsc = repo.join("node_modules/.bin/tsc");
        std::fs::write(&tsc, "#!/bin/sh\n[ \"$1\" = --noEmit ] && exit 0\nexit 7\n").unwrap();
        std::fs::set_permissions(&tsc, std::fs::Permissions::from_mode(0o755)).unwrap();
        let sandbox = adopt_workdir(tmp.path().to_path_buf(), Vec::new(), None);
        let gates = gates_for(&RepoFiles::probe(sandbox.as_ref(), Path::new("frontend")).await);
        assert_eq!(gates.len(), 1);
        assert!(
            run_one_gate(sandbox.as_ref(), Path::new("frontend"), &gates[0])
                .await
                .is_ok()
        );
    }

    async fn run_git_init(dir: &Path) {
        let out = Command::new("git")
            .args(["init", "--initial-branch=main"])
            .current_dir(dir)
            .output()
            .await
            .unwrap();
        assert!(out.status.success());
    }
}
