use std::sync::atomic::Ordering;

use super::lsp::fake::{FakeLauncher, Script};
use super::lsp::uri_of;
use super::*;

struct Project {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Project {
    /// A Cargo package with a library and a binary that both name `total`.
    fn rust() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        std::fs::write(
            root.join("Cargo.toml"),
            "[package]\nname = \"p\"\nedition = \"2021\"\n",
        )
        .unwrap();
        std::fs::create_dir(root.join("src")).unwrap();
        std::fs::write(
            root.join("src/lib.rs"),
            "/// Adds.\npub fn total(a: u32) -> u32 {\n    a + 1\n}\n",
        )
        .unwrap();
        std::fs::write(
            root.join("src/main.rs"),
            "fn main() {\n    println!(\"{}\", p::total(1));\n}\n",
        )
        .unwrap();
        Self { _dir: dir, root }
    }

    fn files(&self) -> Vec<PathBuf> {
        vec![self.root.join("src/lib.rs"), self.root.join("src/main.rs")]
    }

    fn read(&self, path: &str) -> String {
        std::fs::read_to_string(self.root.join(path)).unwrap()
    }
}

fn config() -> RefactorConfig {
    RefactorConfig {
        startup_timeout_secs: 2,
        request_timeout_secs: 2,
        ..RefactorConfig::default()
    }
}

fn tool_with(
    project: &Project,
    config: Option<RefactorConfig>,
    launcher: Arc<FakeLauncher>,
    pool: Arc<Pool>,
) -> CodeRefactorTool {
    CodeRefactorTool::with_parts(
        vec![project.root.clone()],
        Arc::new(move || config.clone()),
        launcher,
        pool,
    )
}

fn tool(project: &Project, launcher: Arc<FakeLauncher>) -> CodeRefactorTool {
    tool_with(
        project,
        Some(config()),
        launcher,
        Pool::new(2, Duration::from_secs(60)),
    )
}

fn rename_total() -> Value {
    json!({"action": "rename", "path": "src/lib.rs", "symbol": "total", "new_name": "sum"})
}

#[tokio::test]
async fn a_definition_is_replaced_by_name_and_reported_as_an_edit() {
    let project = Project::rust();
    let launcher = FakeLauncher::new(Script::default());
    let result = tool(&project, Arc::clone(&launcher))
        .execute(json!({
            "action": "replace_definition",
            "path": "src/lib.rs",
            "symbol": "total",
            "new_content": "pub fn total(a: u32) -> u32 {\n    a + 2\n}",
        }))
        .await
        .unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(
        project.read("src/lib.rs"),
        "/// Adds.\npub fn total(a: u32) -> u32 {\n    a + 2\n}\n"
    );
    assert_eq!(result.output["kind"], "fn");
    assert_eq!(
        result.output["files"][0]["path"],
        project.root.join("src/lib.rs").display().to_string()
    );
    assert_eq!(launcher.launches(), 0, "no language server is needed");
}

#[tokio::test]
async fn nothing_runs_for_an_agent_without_structural_editing() {
    let project = Project::rust();
    let launcher = FakeLauncher::new(Script::default());
    let tool = tool_with(
        &project,
        None,
        Arc::clone(&launcher),
        Pool::new(1, Duration::from_secs(60)),
    );
    let result = tool.execute(rename_total()).await.unwrap();
    assert!(!result.success);
    assert!(result.error.unwrap().contains("not enabled"));
    assert_eq!(launcher.launches(), 0);
}

#[tokio::test]
async fn an_agent_without_language_servers_keeps_replace_definition() {
    let project = Project::rust();
    let launcher = FakeLauncher::new(Script::default());
    let tree_only = RefactorConfig {
        language_servers: false,
        ..config()
    };
    let tool = tool_with(
        &project,
        Some(tree_only),
        Arc::clone(&launcher),
        Pool::new(1, Duration::from_secs(60)),
    );
    let rename = tool.execute(rename_total()).await.unwrap();
    assert!(rename.error.unwrap().contains("turned off"));
    let replace = tool
        .execute(json!({
            "action": "replace_definition", "path": "src/lib.rs", "symbol": "total",
            "new_content": "pub fn total(a: u32) -> u32 { a }",
        }))
        .await
        .unwrap();
    assert!(replace.success, "{:?}", replace.error);
    assert_eq!(launcher.launches(), 0);
}

#[tokio::test]
async fn other_languages_and_paths_outside_the_workspace_are_refused() {
    let project = Project::rust();
    std::fs::write(project.root.join("a.py"), "def f(): pass\n").unwrap();
    let tool = tool(&project, FakeLauncher::new(Script::default()));
    let python = tool
        .execute(json!({"action": "rename", "path": "a.py", "symbol": "f", "new_name": "g"}))
        .await
        .unwrap();
    assert!(python.error.unwrap().contains("code.edit"));
    let outside = tool
        .execute(json!({"action": "diagnostics", "path": "../elsewhere.rs"}))
        .await
        .unwrap();
    assert!(outside.error.unwrap().contains("outside"));
}

#[tokio::test]
async fn a_rename_edits_every_file_it_touches() {
    let project = Project::rust();
    let launcher = FakeLauncher::new(Script {
        files: project.files(),
        ..Script::default()
    });
    let result = tool(&project, Arc::clone(&launcher))
        .execute(rename_total())
        .await
        .unwrap();
    assert!(result.success, "{:?}", result.error);
    assert!(project.read("src/lib.rs").contains("pub fn sum(a: u32)"));
    assert!(project.read("src/main.rs").contains("p::sum(1)"));
    assert_eq!(result.output["edits"], 2);
    assert_eq!(result.output["files"].as_array().unwrap().len(), 2);
}

#[tokio::test]
async fn a_rename_reaching_outside_the_workspace_changes_nothing() {
    let project = Project::rust();
    let range = json!({"start": {"line": 1, "character": 7}, "end": {"line": 1, "character": 12}});
    let launcher = FakeLauncher::new(Script {
        rename_answer: Some(json!({"changes": {
            uri_of(&project.root.join("src/lib.rs")): [{"range": range, "newText": "sum"}],
            "file:///etc/passwd": [{"range": range, "newText": "sum"}],
        }})),
        ..Script::default()
    });
    let result = tool(&project, launcher)
        .execute(rename_total())
        .await
        .unwrap();
    assert!(result.error.unwrap().contains("not source in"));
    assert!(project.read("src/lib.rs").contains("pub fn total"));
}

/// An agent workspace as the hub lays it out: the checkout the server runs
/// for, a second checkout, and a transcript the hub keeps for itself.
struct Workspace {
    _dir: tempfile::TempDir,
    root: PathBuf,
}

impl Workspace {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let write = |path: &str, text: &str| {
            let path = root.join(path);
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, text).unwrap();
        };
        write("app/.git/HEAD", "ref: refs/heads/main\n");
        write("app/Cargo.toml", "[package]\nname = \"p\"\n");
        write("app/src/lib.rs", "pub fn total() {}\n");
        write("app/notes.md", "total is the sum\n");
        write("app/closed/gen.rs", "pub fn total() {}\n");
        write("other/.git/HEAD", "ref: refs/heads/main\n");
        write("other/src/lib.rs", "pub fn total() {}\n");
        write("sessions/a.jsonl", "total secret of another user\n");
        Self { _dir: dir, root }
    }

    fn tool(&self, launcher: Arc<FakeLauncher>) -> CodeRefactorTool {
        CodeRefactorTool::with_parts(
            vec![self.root.clone()],
            Arc::new(|| Some(config())),
            launcher,
            Pool::new(2, Duration::from_secs(60)),
        )
    }

    fn read(&self, path: &str) -> String {
        std::fs::read_to_string(self.root.join(path)).unwrap()
    }
}

/// A language server runs the project's own code, so what it names is not
/// trusted: the hub must neither write nor read, on its word, a file outside
/// the checkout it serves or one that is not its language's source.
#[tokio::test]
async fn a_server_cannot_steer_a_rename_out_of_its_checkout_or_its_language() {
    let ws = Workspace::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let closed = std::fs::Permissions::from_mode(0o700);
        std::fs::set_permissions(ws.root.join("app/closed"), closed).unwrap();
    }
    let name = json!({"start": {"line": 0, "character": 7}, "end": {"line": 0, "character": 12}});
    let first_word =
        json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}});
    let misplaced =
        json!({"start": {"line": 0, "character": 6}, "end": {"line": 0, "character": 12}});
    let targets = [
        ("other/src/lib.rs", &name),
        ("app/notes.md", &first_word),
        ("sessions/a.jsonl", &first_word),
        ("sessions/a.jsonl", &misplaced),
        #[cfg(unix)]
        ("app/closed/gen.rs", &name),
    ];
    for (target, range) in targets {
        let before = ws.read(target);
        let launcher = FakeLauncher::new(Script {
            rename_answer: Some(json!({"changes": {
                uri_of(&ws.root.join("app/src/lib.rs")): [{"range": name, "newText": "sum"}],
                uri_of(&ws.root.join(target)): [{"range": range, "newText": "sum"}],
            }})),
            ..Script::default()
        });
        let shown = Arc::clone(&launcher.shown);
        let result = ws
            .tool(launcher)
            .execute(json!({
                "action": "rename", "path": "app/src/lib.rs", "symbol": "total", "new_name": "sum",
            }))
            .await
            .unwrap();
        let error = result.error.expect(target);
        assert!(error.contains("not source in"), "{target}: {error}");
        assert_eq!(ws.read(target), before, "{target}");
        assert!(
            ws.read("app/src/lib.rs").contains("pub fn total"),
            "{target}"
        );
        assert_eq!(
            *shown.lock(),
            [uri_of(&ws.root.join("app/src/lib.rs"))],
            "{target} was never read out to the server"
        );
    }
}

#[tokio::test]
async fn a_server_cannot_have_the_hub_read_out_files_outside_its_checkout() {
    let ws = Workspace::new();
    let start = json!({"start": {"line": 0, "character": 0}, "end": {"line": 0, "character": 5}});
    let launcher = FakeLauncher::new(Script {
        files: vec![ws.root.join("app/src/lib.rs")],
        extra_locations: ["sessions/a.jsonl", "other/src/lib.rs", "app/notes.md"]
            .iter()
            .map(|path| json!({"uri": uri_of(&ws.root.join(path)), "range": start}))
            .collect(),
        ..Script::default()
    });
    let result = ws
        .tool(launcher)
        .execute(json!({"action": "references", "path": "app/src/lib.rs", "symbol": "total"}))
        .await
        .unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["references"].as_array().unwrap().len(), 1);
    assert_eq!(result.output["outside_workspace"], 4);
    assert!(!result.output.to_string().contains("secret"));
}

#[tokio::test]
async fn a_file_too_large_for_a_language_server_starts_none() {
    let project = Project::rust();
    let large = format!("fn a() {{}}\n{}", "// padding\n".repeat(200_000));
    std::fs::write(project.root.join("src/lib.rs"), large).unwrap();
    let launcher = FakeLauncher::new(Script::default());
    let result = tool(&project, Arc::clone(&launcher))
        .execute(json!({"action": "diagnostics", "path": "src/lib.rs"}))
        .await
        .unwrap();
    assert!(result
        .error
        .unwrap()
        .contains("limit for a language server"));
    assert_eq!(launcher.launches(), 0);
}

#[tokio::test]
async fn a_rename_of_something_that_is_not_a_name_is_refused() {
    let project = Project::rust();
    std::fs::write(
        project.root.join("src/lib.rs"),
        "fn f<'a>(x: &'a str) -> &'a str {\n    x\n}\n",
    )
    .unwrap();
    let tool = tool(&project, FakeLauncher::new(Script::default()));
    let rename = |line: Option<u64>| json!({"action": "rename", "path": "src/lib.rs", "symbol": "'a", "line": line, "new_name": "b"});
    let result = tool.execute(rename(Some(1))).await.unwrap();
    assert!(result.error.unwrap().contains("does not name a symbol"));
    let result = tool.execute(rename(None)).await.unwrap();
    assert!(result.error.unwrap().contains("appears 3 times"));
}

#[tokio::test]
async fn a_rename_that_would_move_files_changes_nothing() {
    let project = Project::rust();
    let launcher = FakeLauncher::new(Script {
        rename_answer: Some(json!({"documentChanges": [
            {"kind": "rename", "oldUri": uri_of(&project.root.join("src/lib.rs")), "newUri": "file:///x.rs"},
        ]})),
        ..Script::default()
    });
    let result = tool(&project, launcher)
        .execute(rename_total())
        .await
        .unwrap();
    assert!(result.error.unwrap().contains("create, rename or delete"));
}

#[tokio::test]
async fn a_rename_from_a_stale_view_is_retried_once_then_refused_whole() {
    let project = Project::rust();
    let lib = uri_of(&project.root.join("src/lib.rs"));
    let main = uri_of(&project.root.join("src/main.rs"));
    let at = |line: u64, from: u64| json!({"start": {"line": line, "character": from}, "end": {"line": line, "character": from + 5}});
    let launcher = FakeLauncher::new(Script {
        rename_answer: Some(json!({"documentChanges": [
            {"textDocument": {"uri": lib, "version": null}, "edits": [{"range": at(1, 7), "newText": "sum"}]},
            {"textDocument": {"uri": main, "version": null}, "edits": [{"range": at(1, 4), "newText": "sum"}]},
        ]})),
        ..Script::default()
    });
    let renames = Arc::clone(&launcher.renames);
    let result = tool(&project, launcher)
        .execute(rename_total())
        .await
        .unwrap();
    assert!(result.error.unwrap().contains("older version"));
    assert_eq!(
        renames.load(Ordering::SeqCst),
        2,
        "asked again after the refresh"
    );
    assert!(
        project.read("src/lib.rs").contains("pub fn total"),
        "all or nothing"
    );
}

#[tokio::test]
async fn one_server_per_project_is_reused_across_calls() {
    let project = Project::rust();
    let launcher = FakeLauncher::new(Script {
        files: project.files(),
        ..Script::default()
    });
    let pool = Pool::new(2, Duration::from_secs(60));
    let tool = tool_with(
        &project,
        Some(config()),
        Arc::clone(&launcher),
        Arc::clone(&pool),
    );
    for _ in 0..3 {
        let result = tool
            .execute(json!({"action": "diagnostics", "path": "src/main.rs"}))
            .await
            .unwrap();
        assert!(result.success, "{:?}", result.error);
    }
    assert_eq!(launcher.launches(), 1);
    assert_eq!(pool.running(), 1);
}

#[tokio::test]
async fn the_server_limit_stops_the_idle_one_and_refuses_when_all_are_busy() {
    let (first, second) = (Project::rust(), Project::rust());
    let launcher = FakeLauncher::new(Script::default());
    let pool = Pool::new(1, Duration::from_secs(60));
    let spec = |project: &Project| ServerSpec {
        kind: ServerKind::RustAnalyzer,
        root: project.root.clone(),
        command: vec!["rust-analyzer".into()],
    };
    let timeouts = Timeouts {
        startup: Duration::from_secs(2),
        request: Duration::from_secs(2),
    };
    let held = pool
        .checkout(spec(&first), launcher.as_ref(), timeouts, 1024)
        .await
        .unwrap();
    let busy = pool
        .checkout(spec(&second), launcher.as_ref(), timeouts, 1024)
        .await;
    assert!(matches!(busy, Err(LspError::Busy(1))));
    drop(held);
    let _second = pool
        .checkout(spec(&second), launcher.as_ref(), timeouts, 1024)
        .await
        .unwrap();
    assert_eq!(
        (pool.running(), launcher.launches()),
        (1, 2),
        "the idle one made room"
    );
}

fn rust_spec(project: &Project) -> ServerSpec {
    ServerSpec {
        kind: ServerKind::RustAnalyzer,
        root: project.root.clone(),
        command: vec!["rust-analyzer".into()],
    }
}

/// Two calls for one project whose server does not start: the second, which
/// waited on the first, must not start one in the slot the first gave up.
#[tokio::test]
async fn calls_racing_on_a_server_that_fails_to_start_leave_nothing_running() {
    let project = Project::rust();
    let launcher = FakeLauncher::new(Script {
        missing: true,
        ..Script::default()
    });
    let pool = Pool::new(1, Duration::from_secs(60));
    let timeouts = Timeouts {
        startup: Duration::from_secs(2),
        request: Duration::from_secs(2),
    };
    let checkout = || pool.checkout(rust_spec(&project), launcher.as_ref(), timeouts, 1024);
    let (first, second) = tokio::join!(checkout(), checkout());
    assert!(matches!(first, Err(LspError::Unavailable(_))));
    assert!(matches!(second, Err(LspError::Unavailable(_))));
    assert_eq!((pool.running(), launcher.launches()), (0, 2));
}

#[tokio::test]
async fn a_call_gives_up_on_a_server_another_call_keeps_busy() {
    let project = Project::rust();
    let launcher = FakeLauncher::new(Script::default());
    let pool = Pool::new(1, Duration::from_secs(60));
    let timeouts = Timeouts {
        startup: Duration::from_millis(100),
        request: Duration::from_millis(100),
    };
    let _held = pool
        .checkout(rust_spec(&project), launcher.as_ref(), timeouts, 1024)
        .await
        .unwrap();
    let waited = pool
        .checkout(rust_spec(&project), launcher.as_ref(), timeouts, 1024)
        .await;
    let Err(LspError::NotReady(why)) = waited else {
        panic!("the second call was served")
    };
    assert!(why.contains("busy with another call"), "{why}");
    assert_eq!(launcher.launches(), 1);
}

#[tokio::test]
async fn an_idle_server_is_reaped() {
    let project = Project::rust();
    let launcher = FakeLauncher::new(Script::default());
    let pool = Pool::new(2, Duration::ZERO);
    let tool = tool_with(
        &project,
        Some(config()),
        Arc::clone(&launcher),
        Arc::clone(&pool),
    );
    let result = tool
        .execute(json!({"action": "diagnostics", "path": "src/lib.rs"}))
        .await
        .unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(pool.running(), 1);
    pool.reap();
    assert_eq!(pool.running(), 0);
}

#[tokio::test]
async fn a_server_that_stops_answering_is_replaced_on_the_next_call() {
    let project = Project::rust();
    let launcher = FakeLauncher::new(Script {
        hang_on_rename: true,
        ..Script::default()
    });
    let tool = tool(&project, Arc::clone(&launcher));
    let hung = tool.execute(rename_total()).await.unwrap();
    assert!(hung.error.unwrap().contains("did not answer in time"));
    let again = tool.execute(rename_total()).await.unwrap();
    assert!(!again.success);
    assert_eq!(
        launcher.launches(),
        2,
        "the wedged server was not asked again"
    );
}

#[tokio::test]
async fn a_server_still_loading_is_kept_for_the_next_call() {
    let project = Project::rust();
    let launcher = FakeLauncher::new(Script {
        never_quiescent: true,
        ..Script::default()
    });
    let tool = tool(&project, Arc::clone(&launcher));
    for _ in 0..2 {
        let result = tool.execute(rename_total()).await.unwrap();
        assert!(result.error.unwrap().contains("still loading"));
    }
    assert_eq!(launcher.launches(), 1);
}

#[tokio::test]
async fn a_missing_server_degrades_to_the_tree_sitter_action() {
    let project = Project::rust();
    let launcher = FakeLauncher::new(Script {
        missing: true,
        ..Script::default()
    });
    let pool = Pool::new(2, Duration::from_secs(60));
    let tool = tool_with(&project, Some(config()), launcher, Arc::clone(&pool));
    let result = tool.execute(rename_total()).await.unwrap();
    let error = result.error.unwrap();
    assert!(error.contains("not available on this hub"), "{error}");
    assert!(error.contains("replace_definition"), "{error}");
    assert_eq!(
        pool.running(),
        0,
        "a server that never started holds no slot"
    );
}

/// The hub's own daemon, as privilege separation runs it.
async fn daemon(dir: &Path) -> RunnerEndpoint {
    use std::os::unix::fs::MetadataExt;
    use workforce_agent::tools::exec_runner::{serve, JobHandler, Listener, LocalJobHandler};
    let socket = dir.join("exec.sock");
    let listener = Listener::Unix {
        path: socket.clone(),
        accept_uid: std::fs::metadata(dir).unwrap().uid(),
    };
    let handler: Arc<dyn JobHandler> = Arc::new(LocalJobHandler);
    tokio::spawn(serve(listener, handler));
    while !socket.exists() {
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    RunnerEndpoint::Unix(socket)
}

/// A server binary missing from the hub is named plainly when the start
/// goes through the executor's daemon, as it does in production.
#[tokio::test]
async fn a_server_missing_behind_the_daemon_is_reported_plainly() {
    let project = Project::rust();
    let sockets = tempfile::tempdir().unwrap();
    let config = RefactorConfig {
        rust_analyzer: vec!["workforce-no-such-language-server".into()],
        ..config()
    };
    let tool = CodeRefactorTool::with_parts(
        vec![project.root.clone()],
        Arc::new(move || Some(config.clone())),
        Arc::new(ProcessLauncher {
            runner: Some(daemon(sockets.path()).await),
            lifetime: Duration::from_secs(60),
            max_rss_bytes: 1 << 30,
        }),
        Pool::new(1, Duration::from_secs(60)),
    );
    let error = tool.execute(rename_total()).await.unwrap().error.unwrap();
    assert!(
        error.contains("rust-analyzer is not available on this hub"),
        "{error}"
    );
}

#[tokio::test]
async fn diagnostics_are_listed_with_their_lines() {
    let project = Project::rust();
    std::fs::write(project.root.join("src/lib.rs"), "fn a() {}\nERROR here\n").unwrap();
    let tool = tool(&project, FakeLauncher::new(Script::default()));
    let result = tool
        .execute(json!({"action": "diagnostics", "path": "src/lib.rs"}))
        .await
        .unwrap();
    assert!(result.success, "{:?}", result.error);
    assert_eq!(result.output["complete"], true);
    assert_eq!(
        result.output["diagnostics"],
        json!([{"line": 2, "column": 1, "severity": "error", "message": "an error here", "code": null, "source": "fake"}])
    );
    assert!(
        result.output.get("files").is_none(),
        "a read is never an edit"
    );
}

#[tokio::test]
async fn references_inside_the_workspace_are_listed_and_the_rest_counted() {
    let project = Project::rust();
    let launcher = FakeLauncher::new(Script {
        files: project.files(),
        ..Script::default()
    });
    let result = tool(&project, launcher)
        .execute(json!({"action": "references", "path": "src/main.rs", "symbol": "total"}))
        .await
        .unwrap();
    assert!(result.success, "{:?}", result.error);
    let references = result.output["references"].as_array().unwrap();
    assert_eq!(references.len(), 2);
    assert_eq!(references[1]["text"], "println!(\"{}\", p::total(1));");
    assert_eq!(
        (
            references[1]["line"].clone(),
            references[1]["column"].clone()
        ),
        (json!(2), json!(23))
    );
    assert_eq!(result.output["outside_workspace"], 1);
}

#[test]
fn a_symbol_is_found_on_its_line_at_its_definition_or_as_its_only_use() {
    let text = "fn a() {}\nfn b() { a(); a(); }\n";
    assert_eq!(locate(text, Grammar::Rust, "a", None).unwrap(), 3);
    assert_eq!(locate(text, Grammar::Rust, "a", Some(2)).unwrap(), 19);
    assert!(locate(text, Grammar::Rust, "a", Some(3)).is_err());
    let uses = "fn b() { x(); x(); }\n";
    assert!(locate(uses, Grammar::Rust, "x", None)
        .unwrap_err()
        .0
        .contains("pass the line"));
    assert_eq!(
        locate("let ab = 1; b;", Grammar::Rust, "b", None).unwrap(),
        12
    );
}

#[test]
fn only_replace_definition_runs_without_a_language_server() {
    assert!(!runs_language_server(
        &json!({"action": "replace_definition"})
    ));
    for input in [
        json!({"action": "rename"}),
        json!({"action": "diagnostics"}),
        json!({"action": "surprise"}),
        json!({}),
    ] {
        assert!(runs_language_server(&input), "{input}");
    }
}

/// A language server on this machine, found by asking it its version: a
/// rustup proxy without the component answers with an error.
fn installed(command: &[String]) -> bool {
    std::process::Command::new(&command[0])
        .arg("--version")
        .output()
        .is_ok_and(|out| out.status.success())
}

fn real_tool(root: &Path, config: RefactorConfig) -> CodeRefactorTool {
    CodeRefactorTool::with_parts(
        vec![root.to_path_buf()],
        {
            let config = config.clone();
            Arc::new(move || Some(config.clone()))
        },
        Arc::new(ProcessLauncher {
            runner: None,
            lifetime: Duration::from_secs(600),
            max_rss_bytes: config.max_memory_mb << 20,
        }),
        Pool::new(1, Duration::from_secs(60)),
    )
}

/// Against a real typescript-language-server (`WORKFORCE_TS_LSP` names one
/// not on the PATH); skipped where there is none.
#[tokio::test]
async fn typescript_language_server_renames_across_files_and_reports_errors() {
    let command = match std::env::var("WORKFORCE_TS_LSP") {
        Ok(path) => vec![path, "--stdio".to_string()],
        Err(_) => RefactorConfig::default().typescript,
    };
    if !installed(&command) {
        eprintln!("skipped: no typescript-language-server here");
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().canonicalize().unwrap();
    std::fs::write(
        root.join("tsconfig.json"),
        r#"{"compilerOptions": {"strict": true, "module": "commonjs", "target": "es2020"}}"#,
    )
    .unwrap();
    std::fs::write(
        root.join("a.ts"),
        "export function total(n: number): number {\n  return n + 1;\n}\n",
    )
    .unwrap();
    std::fs::write(
        root.join("b.ts"),
        "import { total } from \"./a\";\nconst x: number = total(1);\nconst bad: string = 1;\n",
    )
    .unwrap();
    let tool = real_tool(
        &root,
        RefactorConfig {
            typescript: command,
            startup_timeout_secs: 60,
            request_timeout_secs: 60,
            ..RefactorConfig::default()
        },
    );

    let renamed = tool
        .execute(json!({"action": "rename", "path": "a.ts", "symbol": "total", "new_name": "sum"}))
        .await
        .unwrap();
    assert!(renamed.success, "{:?}", renamed.error);
    let a = std::fs::read_to_string(root.join("a.ts")).unwrap();
    let b = std::fs::read_to_string(root.join("b.ts")).unwrap();
    assert!(a.contains("export function sum("), "{a}");
    assert!(
        b.contains("import { sum } from") && b.contains("= sum(1)"),
        "{b}"
    );

    let checked = tool
        .execute(json!({"action": "diagnostics", "path": "b.ts"}))
        .await
        .unwrap();
    assert!(checked.success, "{:?}", checked.error);
    let diagnostics = checked.output["diagnostics"].as_array().unwrap();
    assert!(
        diagnostics
            .iter()
            .any(|d| d["line"] == 3 && d["severity"] == "error"),
        "{diagnostics:?}"
    );
}

/// Against a real rust-analyzer (`WORKFORCE_RUST_ANALYZER` names one not on
/// the PATH); skipped where there is none. A standalone rust-analyzer newer
/// than this repo's pinned toolchain only links crates on a toolchain it
/// supports, which `WORKFORCE_RUST_ANALYZER_TOOLCHAIN` names.
#[tokio::test]
async fn rust_analyzer_renames_across_files() {
    let command = match std::env::var("WORKFORCE_RUST_ANALYZER") {
        Ok(path) => vec![path],
        Err(_) => RefactorConfig::default().rust_analyzer,
    };
    if !installed(&command) {
        eprintln!("skipped: no rust-analyzer here");
        return;
    }
    let project = Project::rust();
    if let Ok(toolchain) = std::env::var("WORKFORCE_RUST_ANALYZER_TOOLCHAIN") {
        std::env::set_var("RUSTUP_TOOLCHAIN", toolchain);
    }
    let tool = real_tool(
        &project.root,
        RefactorConfig {
            rust_analyzer: command,
            startup_timeout_secs: 300,
            request_timeout_secs: 120,
            ..RefactorConfig::default()
        },
    );
    let renamed = tool.execute(rename_total()).await.unwrap();
    assert!(renamed.success, "{:?}", renamed.error);
    assert!(project.read("src/lib.rs").contains("pub fn sum("));
    assert!(project.read("src/main.rs").contains("p::sum(1)"));
}
