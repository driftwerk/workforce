//! Privilege-separated command execution (audit finding F7).
//!
//! One execution core ([`run_exec`]) is driven by two callers: the in-process
//! fallback (dev/local) and the [`serve`]d runner daemon (prod), reached by the
//! dispatch [`client`]. Routing every execution-class command through a daemon
//! running as an unprivileged user keeps an agent's `sh`/`cargo`/`git` from
//! reading the hub user's vault, env, or `/proc`. The same client and daemon
//! reach a remote worker host over mutual TLS.

mod client;
mod env_policy;
mod handler;
pub mod lines;
mod link;
mod memory;
mod process_group;
pub mod protocol;
mod run_exec;
mod run_or_dispatch;
mod server;
mod setup;
mod tls;
mod transport;

pub use client::{
    cancel_job, dispatch, dispatch_exec, dispatch_interactive, dispatch_job, dispatch_with,
    forget_job, observe_protocol, DurableJob, LinkLost, Liveness, McpResponder, Outcome,
    ProtocolEvent, ProtocolObserver,
};
pub use env_policy::{validate_overrides, EnvPolicy};
pub use handler::{JobHandler, LocalJobHandler};
pub use lines::LineReassembler;
pub use link::{JobLink, ATTACH_WAIT, MCP_IN_FLIGHT};
pub use process_group::GroupChild;
pub use protocol::{
    CodexMcpConfig, CodexMcpServer, ExecRequest, ExecSummary, HomeSpec, InteractiveRequest, JobId,
    McpBinding, Offsets, PrepareRequest, Prepared, RefusalReason, RepoSpec, Request, RunRequest,
    ScopeId, ServerFrame, Stream, WorkerHealth, MAX_FRAME_LEN, PROTOCOL_DURABLE_JOBS, SCOPE_BENCH,
    SCOPE_HOME, SCOPE_MCP_CONFIG_PATH, SCOPE_WORK_ROOT,
};
pub use run_exec::{
    is_transient_error, run_exec, run_interactive, validate_workdir, EnvMode, MAX_OUTPUT_SIZE,
    MAX_RETRIES,
};
pub use run_or_dispatch::{run_or_dispatch, spawn_interactive, ExecOutcome, Interactive};
pub use server::{serve, serve_on};
pub use setup::{run_setup, tail_utf8, SetupOutcome, SetupReport};
pub use tls::certificate_expiry;
pub use transport::{Acceptor, Listener, RunnerEndpoint, TlsEndpoint, TlsIdentity};

#[cfg(test)]
mod integration_tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use async_trait::async_trait;
    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use super::client::{dispatch, dispatch_exec, dispatch_with, Liveness, McpResponder, Outcome};
    use super::handler::{JobHandler, LocalJobHandler};
    use super::link::JobLink;
    use super::protocol::{
        read_framed, write_framed, ClientFrame, ExecRequest, ExecSummary, RefusalReason, Request,
        ScopeId, ServerFrame, PROTOCOL_VERSION,
    };
    use super::server::{serve, serve_on};
    use super::transport::{Acceptor, Listener, RunnerEndpoint, TlsEndpoint, TlsIdentity};

    fn req(program: &str, args: &[&str]) -> ExecRequest {
        ExecRequest {
            program: program.to_string(),
            args: args.iter().map(|s| s.to_string()).collect(),
            workdir: None,
            timeout_secs: 30,
            allow_retry: false,
            env: Vec::new(),
            stdin: None,
            scope: None,
            mcp: None,
        }
    }

    fn local() -> Arc<dyn JobHandler> {
        Arc::new(LocalJobHandler)
    }

    /// Spawn the daemon on a temp socket accepting `accept_uid`, returning the
    /// endpoint (the daemon task runs detached for the test's lifetime).
    async fn spawn_daemon(dir: &std::path::Path, accept_uid: u32) -> RunnerEndpoint {
        spawn_daemon_with(dir, accept_uid, local()).await
    }

    async fn spawn_daemon_with(
        dir: &std::path::Path,
        accept_uid: u32,
        handler: Arc<dyn JobHandler>,
    ) -> RunnerEndpoint {
        let socket = dir.join("exec.sock");
        let listener = Listener::Unix {
            path: socket.clone(),
            accept_uid,
        };
        tokio::spawn(async move {
            let _ = serve(listener, handler).await;
        });
        for _ in 0..200 {
            if socket.exists() {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        RunnerEndpoint::Unix(socket)
    }

    /// This process's uid, read without an extra dependency by inspecting the
    /// peer credentials of a self-connected socket pair (both ends are us).
    fn current_uid() -> u32 {
        let (a, _b) = tokio::net::UnixStream::pair().unwrap();
        a.peer_cred().unwrap().uid()
    }

    async fn collect(
        endpoint: &RunnerEndpoint,
        request: ExecRequest,
    ) -> eyre::Result<(String, String, ExecSummary)> {
        collect_with(endpoint, request, CancellationToken::new(), None).await
    }

    async fn collect_with(
        endpoint: &RunnerEndpoint,
        request: ExecRequest,
        cancel: CancellationToken,
        mcp: Option<McpResponder>,
    ) -> eyre::Result<(String, String, ExecSummary)> {
        let (tx, mut rx) = mpsc::channel::<ServerFrame>(64);
        let drain = tokio::spawn(async move {
            let mut out = String::new();
            let mut err = String::new();
            while let Some(frame) = rx.recv().await {
                match frame {
                    ServerFrame::Stdout { chunk } => out.push_str(&chunk),
                    ServerFrame::Stderr { chunk } => err.push_str(&chunk),
                    _ => {}
                }
            }
            (out, err)
        });
        let summary = dispatch_exec(endpoint, request, tx, cancel, mcp).await?;
        let (out, err) = drain.await.unwrap();
        Ok((out, err, summary))
    }

    /// Wait for the command to print `started`, which each script here does
    /// only once what the test goes on to watch is on disk.
    async fn started(frames: &mut mpsc::Receiver<ServerFrame>) {
        loop {
            match frames.recv().await {
                Some(ServerFrame::Stdout { chunk }) if chunk.contains("started") => break,
                Some(_) => continue,
                None => panic!("stream closed before the command started"),
            }
        }
    }

    /// Whether what `probe` reads stops changing, as the file a killed
    /// process was writing does. The kill is given time to land rather than
    /// a fixed moment to have landed by.
    async fn goes_quiet<T: PartialEq>(probe: impl Fn() -> T) -> bool {
        for _ in 0..10 {
            let before = probe();
            tokio::time::sleep(Duration::from_secs(1)).await;
            if probe() == before {
                return true;
            }
        }
        false
    }

    /// A script that keeps touching `marker`, printing `started` once it
    /// exists, with a probe for when it was last touched.
    fn touching(marker: &std::path::Path) -> (String, impl Fn() -> std::time::SystemTime + '_) {
        let script = format!(
            "touch {0}; echo started; for i in $(seq 1 300); do touch {0}; sleep 0.1; done",
            marker.display()
        );
        (script, move || {
            std::fs::metadata(marker).unwrap().modified().unwrap()
        })
    }

    /// Closing the connection ends the round's whole process group, not just
    /// the leader: a background child left by a finished command must not
    /// outlive the dispatch that started it. Death is proven by the
    /// grandchild's heartbeat file going quiet — a zombie cannot write.
    #[tokio::test]
    async fn dropping_the_dispatch_kills_the_commands_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = spawn_daemon(dir.path(), current_uid()).await;
        let heartbeat = dir.path().join("heartbeat");
        let script = format!(
            "(while :; do echo x >> {0}; sleep 0.05; done) & \
             until [ -s {0} ]; do sleep 0.01; done; echo started; wait",
            heartbeat.display()
        );
        let r = req("sh", &["-c", &script]);

        let (tx, mut rx) = mpsc::channel::<ServerFrame>(64);
        let run = tokio::spawn({
            let endpoint = endpoint.clone();
            async move { dispatch_exec(&endpoint, r, tx, CancellationToken::new(), None).await }
        });
        started(&mut rx).await;

        run.abort();
        assert!(
            goes_quiet(|| std::fs::metadata(&heartbeat).unwrap().len()).await,
            "grandchild kept writing after the closed connection"
        );
    }

    fn interactive(program: &str, args: &[&str]) -> super::protocol::InteractiveRequest {
        super::protocol::InteractiveRequest {
            exec: ExecRequest {
                timeout_secs: 60,
                ..req(program, args)
            },
            max_rss_bytes: None,
        }
    }

    /// Stdout as it streams, until `want` has arrived.
    async fn read_until(output: &mut mpsc::Receiver<ServerFrame>, want: usize) -> String {
        let mut seen = String::new();
        while seen.len() < want {
            match tokio::time::timeout(Duration::from_secs(10), output.recv()).await {
                Ok(Some(ServerFrame::Stdout { chunk })) => seen.push_str(&chunk),
                Ok(Some(_)) => {}
                Ok(None) | Err(_) => break,
            }
        }
        seen
    }

    /// Input written while the command runs reaches it in order, whole even
    /// where it spans many frames and cuts through multi-byte characters,
    /// and the command sees end of input when the client's stream ends.
    #[tokio::test]
    async fn an_interactive_command_is_fed_its_input_as_it_arrives_through_the_daemon() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = spawn_daemon(dir.path(), current_uid()).await;
        let mut running = super::spawn_interactive(
            Some(&endpoint),
            interactive("cat", &[]),
            super::EnvMode::Scrubbed,
        );
        running.stdin.send("ping\n".into()).await.unwrap();
        assert_eq!(read_until(&mut running.output, 5).await, "ping\n");
        let large = "é".repeat(100_000);
        running.stdin.send(large.clone()).await.unwrap();
        assert_eq!(read_until(&mut running.output, large.len()).await, large);
        drop(running.stdin);
        let summary = tokio::time::timeout(Duration::from_secs(10), running.done)
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(
            summary.exit_code,
            Some(0),
            "cat must see the end of its input"
        );
    }

    #[tokio::test]
    async fn an_interactive_command_runs_in_process_without_a_runner() {
        let mut running = super::spawn_interactive(
            None,
            interactive("cat", &[]),
            super::EnvMode::Inherit { remove: &[] },
        );
        running.stdin.send("local\n".into()).await.unwrap();
        assert_eq!(read_until(&mut running.output, 6).await, "local\n");
        drop(running.stdin);
        let summary = running.done.await.unwrap().unwrap();
        assert_eq!(summary.exit_code, Some(0));
    }

    /// A command that stops reading its input must not hold the connection:
    /// the daemon ends it once its backlog fills, rather than blocking the
    /// reader that would see a cancel or a hang-up.
    #[tokio::test]
    async fn an_interactive_command_that_stops_reading_is_ended() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = spawn_daemon(dir.path(), current_uid()).await;
        let running = super::spawn_interactive(
            Some(&endpoint),
            interactive("sleep", &["600"]),
            super::EnvMode::Scrubbed,
        );
        let flood = "x".repeat(1 << 20);
        for _ in 0..40 {
            if running.stdin.send(flood.clone()).await.is_err() {
                break;
            }
        }
        let summary = tokio::time::timeout(Duration::from_secs(20), running.done)
            .await
            .expect("the stuck command outlived its backlog")
            .unwrap()
            .unwrap();
        assert_eq!(summary, ExecSummary::killed());
    }

    /// Dropping an interactive command ends its whole process group.
    #[tokio::test]
    async fn dropping_an_interactive_command_kills_its_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = spawn_daemon(dir.path(), current_uid()).await;
        let heartbeat = dir.path().join("heartbeat");
        let script = format!(
            "(while :; do echo x >> {}; sleep 0.05; done) & echo started; cat",
            heartbeat.display()
        );
        let mut running = super::spawn_interactive(
            Some(&endpoint),
            interactive("sh", &["-c", &script]),
            super::EnvMode::Scrubbed,
        );
        assert!(read_until(&mut running.output, 8).await.contains("started"));
        let size = || std::fs::metadata(&heartbeat).map(|m| m.len()).unwrap_or(0);
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert!(size() > 0);

        drop(running);
        tokio::time::sleep(Duration::from_secs(1)).await;
        let after_kill = size();
        tokio::time::sleep(Duration::from_secs(1)).await;
        assert_eq!(size(), after_kill, "the group kept running after the drop");
    }

    /// An explicit cancel reaches the daemon as a frame, so the job dies even
    /// on a link where a dropped connection would not be noticed promptly,
    /// and the client reports the cancellation rather than an exit.
    #[tokio::test]
    async fn an_explicit_cancel_kills_the_job_and_is_reported_as_cancelled() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = spawn_daemon(dir.path(), current_uid()).await;
        let marker = dir.path().join("still-running");
        let (script, touched) = touching(&marker);
        let cancel = CancellationToken::new();
        let (tx, mut rx) = mpsc::channel::<ServerFrame>(64);
        let run = tokio::spawn({
            let endpoint = endpoint.clone();
            let cancel = cancel.clone();
            let r = req("sh", &["-c", &script]);
            async move { dispatch_exec(&endpoint, r, tx, cancel, None).await }
        });

        started(&mut rx).await;
        cancel.cancel();
        let err = run.await.unwrap().unwrap_err();
        assert!(err.to_string().contains("cancelled"), "{err}");

        assert!(
            goes_quiet(touched).await,
            "child kept running after the cancel"
        );
    }

    /// The end-to-end shape Claude Code depends on: a hub-composed credential
    /// and a prompt cross the socket to the child, while the metered-API key
    /// present in the daemon's own environment does not (I1).
    #[tokio::test]
    async fn a_dispatched_command_receives_the_environment_and_stdin() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = spawn_daemon(dir.path(), current_uid()).await;

        let mut r = req(
            "sh",
            &[
                "-c",
                "printf %s \"$CLAUDE_CODE_OAUTH_TOKEN:${ANTHROPIC_API_KEY:-absent}:\"; cat",
            ],
        );
        r.env = vec![("CLAUDE_CODE_OAUTH_TOKEN".into(), "sub-token".into())];
        r.stdin = Some("the-prompt".into());

        let (out, _err, summary) = collect(&endpoint, r).await.unwrap();
        assert_eq!(summary.exit_code, Some(0));
        assert_eq!(out, "sub-token:absent:the-prompt");
    }

    /// The daemon announces its protocol before any output, so a client can
    /// tell it understands the fields it was sent rather than discovering the
    /// command ran without them.
    #[tokio::test]
    async fn the_daemon_announces_its_protocol_version() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = spawn_daemon(dir.path(), current_uid()).await;

        let mut r = req("true", &[]);
        r.env = vec![("A_VAR".into(), "v".into())];
        assert!(
            collect(&endpoint, r).await.is_ok(),
            "a current daemon must satisfy a request using v1 fields"
        );
    }

    /// Dropping the connection still cancels: that covers a cancelled turn, a
    /// hub crash and a dropped socket with one mechanism — and a Claude Code
    /// turn can run for minutes, so an orphan here would outlive its session.
    #[tokio::test]
    async fn dropping_the_client_kills_the_child() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = spawn_daemon(dir.path(), current_uid()).await;
        let marker = dir.path().join("still-running");
        let (script, touched) = touching(&marker);
        let mut r = req("sh", &["-c", &script]);
        r.stdin = Some(String::new());

        let (tx, mut rx) = mpsc::channel::<ServerFrame>(1);
        let handle = tokio::spawn({
            let endpoint = endpoint.clone();
            async move { dispatch_exec(&endpoint, r, tx, CancellationToken::new(), None).await }
        });

        started(&mut rx).await;
        handle.abort();
        let _ = handle.await;

        assert!(
            goes_quiet(touched).await,
            "child kept running after the client went away"
        );
    }

    #[tokio::test]
    async fn dispatch_to_daemon_happy_path() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = spawn_daemon(tmp.path(), current_uid()).await;

        let (out, _err, summary) = collect(&endpoint, req("echo", &["hello daemon"]))
            .await
            .unwrap();
        assert_eq!(out.trim(), "hello daemon");
        assert_eq!(summary.exit_code, Some(0));
        assert!(!summary.timed_out);
    }

    #[tokio::test]
    async fn dispatch_streams_stderr_and_nonzero_exit() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = spawn_daemon(tmp.path(), current_uid()).await;

        let (_out, err, summary) = collect(&endpoint, req("sh", &["-c", "echo oops >&2; exit 3"]))
            .await
            .unwrap();
        assert_eq!(err.trim(), "oops");
        assert_eq!(summary.exit_code, Some(3));
    }

    /// SO_PEERCRED reject: a daemon configured to accept a *different* uid drops
    /// our connection, so dispatch sees an EOF-before-Done and fails closed.
    #[tokio::test]
    async fn peer_cred_reject_fails_closed() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = spawn_daemon(tmp.path(), current_uid().wrapping_add(1)).await;

        let err = collect(&endpoint, req("echo", &["should-not-run"]))
            .await
            .unwrap_err();
        assert!(
            err.to_string()
                .contains("secure command runner unavailable"),
            "unexpected error: {err}"
        );
    }

    /// A command that is silent for a while before finishing keeps the stream
    /// live and still delivers `Done` (bounds-checking the streamed protocol,
    /// not the in-process buffer-everything path).
    #[tokio::test]
    async fn long_silent_stream_stays_live_and_completes() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = spawn_daemon(tmp.path(), current_uid()).await;

        let mut request = req("sh", &["-c", "sleep 2; echo done-after-silence"]);
        request.timeout_secs = 30;
        let (out, _err, summary) = collect(&endpoint, request).await.unwrap();
        assert_eq!(out.trim(), "done-after-silence");
        assert_eq!(summary.exit_code, Some(0));
        assert!(!summary.timed_out);
    }

    /// The daemon enforces the timeout authoritatively and reports `timed_out`
    /// without the client's grace window expiring first.
    #[tokio::test]
    async fn daemon_enforces_timeout() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = spawn_daemon(tmp.path(), current_uid()).await;

        let mut request = req("sh", &["-c", "sleep 30"]);
        request.timeout_secs = 1;
        let (_out, _err, summary) = collect(&endpoint, request).await.unwrap();
        assert!(summary.timed_out);
        assert_eq!(summary.exit_code, None);
    }

    /// The hub-local daemon shares the hub's filesystem and has no scopes, so
    /// a scoped request is refused outright rather than run on the wrong
    /// tree; the refusal names why.
    #[tokio::test]
    async fn the_local_daemon_refuses_scoped_requests() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = spawn_daemon(tmp.path(), current_uid()).await;

        let mut scoped = req("echo", &["never"]);
        scoped.scope = Some(ScopeId::new("abc"));
        let err = collect(&endpoint, scoped).await.unwrap_err();
        assert!(err.to_string().contains("does not serve"), "{err}");

        let (tx, _rx) = mpsc::channel(8);
        let outcome = dispatch(
            &endpoint,
            Request::Release {
                scope: ScopeId::new("abc"),
            },
            tx,
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        assert_eq!(outcome, Outcome::Refused(RefusalReason::ProtocolMismatch));
    }

    #[tokio::test]
    async fn the_local_daemon_answers_health_without_scopes() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = spawn_daemon(tmp.path(), current_uid()).await;
        let (tx, _rx) = mpsc::channel(8);
        let outcome = dispatch(
            &endpoint,
            Request::Health,
            tx,
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap();
        let Outcome::Healthy(health) = outcome else {
            panic!("expected a health report, got {outcome:?}")
        };
        assert_eq!(health.capacity, 0);
        assert_eq!(health.version, env!("CARGO_PKG_VERSION"));
    }

    /// A job that calls back into the client gets its answer over the same
    /// connection, and without a responder it gets a JSON-RPC error rather
    /// than a hang.
    struct EchoMcp;

    #[async_trait]
    impl JobHandler for EchoMcp {
        async fn run(&self, _request: Request, link: JobLink) -> eyre::Result<ServerFrame> {
            let reply = link.mcp_call("ping".into()).await?;
            link.emit(ServerFrame::Stdout { chunk: reply }).await?;
            Ok(ServerFrame::Done(ExecSummary {
                exit_code: Some(0),
                attempts: 1,
                truncated: false,
                timed_out: false,
            }))
        }
    }

    #[tokio::test]
    async fn a_jobs_tool_call_is_answered_by_the_clients_responder() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = spawn_daemon_with(tmp.path(), current_uid(), Arc::new(EchoMcp)).await;
        let responder: McpResponder =
            Arc::new(|body| Box::pin(async move { format!("pong:{body}") }));
        let (out, _err, summary) = collect_with(
            &endpoint,
            req("x", &[]),
            CancellationToken::new(),
            Some(responder),
        )
        .await
        .unwrap();
        assert_eq!(out, "pong:ping");
        assert_eq!(summary.exit_code, Some(0));
    }

    #[tokio::test]
    async fn a_jobs_tool_call_without_a_responder_gets_a_json_rpc_error() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = spawn_daemon_with(tmp.path(), current_uid(), Arc::new(EchoMcp)).await;
        let (out, _err, _summary) = collect(&endpoint, req("x", &[])).await.unwrap();
        assert!(out.contains("-32601"), "{out}");
    }

    /// The daemon lets at most sixteen tool calls wait at once and holds the
    /// seventeenth back until one completes, so a chatty job cannot flood the
    /// hub; every call is still answered.
    struct FloodMcp;

    #[async_trait]
    impl JobHandler for FloodMcp {
        async fn run(&self, _request: Request, link: JobLink) -> eyre::Result<ServerFrame> {
            let calls = (0..17).map(|i| link.mcp_call(i.to_string()));
            let replies = futures::future::try_join_all(calls).await?;
            link.emit(ServerFrame::Stdout {
                chunk: replies.len().to_string(),
            })
            .await?;
            Ok(ServerFrame::Done(ExecSummary {
                exit_code: Some(0),
                attempts: 1,
                truncated: false,
                timed_out: false,
            }))
        }
    }

    #[tokio::test]
    async fn in_flight_tool_calls_are_bounded_and_all_answered() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = spawn_daemon_with(tmp.path(), current_uid(), Arc::new(FloodMcp)).await;
        let in_flight = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let responder: McpResponder = Arc::new({
            let in_flight = Arc::clone(&in_flight);
            let peak = Arc::clone(&peak);
            move |body| {
                let in_flight = Arc::clone(&in_flight);
                let peak = Arc::clone(&peak);
                Box::pin(async move {
                    let now = in_flight.fetch_add(1, Ordering::SeqCst) + 1;
                    peak.fetch_max(now, Ordering::SeqCst);
                    tokio::time::sleep(Duration::from_millis(50)).await;
                    in_flight.fetch_sub(1, Ordering::SeqCst);
                    body
                })
            }
        });
        let (out, _err, _summary) = collect_with(
            &endpoint,
            req("x", &[]),
            CancellationToken::new(),
            Some(responder),
        )
        .await
        .unwrap();
        assert_eq!(out, "17");
        assert!(peak.load(Ordering::SeqCst) <= super::link::MCP_IN_FLIGHT);
    }

    /// A connection that opens with anything but a request is closed, so a
    /// stray frame can never be mistaken for work.
    #[tokio::test]
    async fn the_first_frame_must_be_a_request() {
        let tmp = tempfile::tempdir().unwrap();
        let RunnerEndpoint::Unix(socket) = spawn_daemon(tmp.path(), current_uid()).await else {
            unreachable!()
        };
        let mut stream = tokio::net::UnixStream::connect(&socket).await.unwrap();
        write_framed(&mut stream, &ClientFrame::Cancel)
            .await
            .unwrap();
        let answer: Option<ServerFrame> = read_framed(&mut stream).await.unwrap();
        assert!(
            answer.is_none(),
            "expected the daemon to close, got {answer:?}"
        );
    }

    /// A fake daemon that announces `version`, sends nothing for `quiet_for`
    /// and then either finishes or hangs, for the protocol-skew and liveness
    /// cases a real daemon cannot produce.
    async fn fake_unix_daemon(
        dir: &std::path::Path,
        version: u32,
        quiet_for: Duration,
        then_done: bool,
    ) -> RunnerEndpoint {
        let socket = dir.join("fake.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let _: Option<ClientFrame> = read_framed(&mut stream).await.unwrap();
            write_framed(
                &mut stream,
                &ServerFrame::Ready {
                    protocol_version: version,
                },
            )
            .await
            .unwrap();
            tokio::time::sleep(quiet_for).await;
            if then_done {
                write_framed(
                    &mut stream,
                    &ServerFrame::Done(ExecSummary {
                        exit_code: Some(0),
                        attempts: 1,
                        truncated: false,
                        timed_out: false,
                    }),
                )
                .await
                .unwrap();
            } else {
                std::future::pending::<()>().await;
            }
        });
        RunnerEndpoint::Unix(socket)
    }

    struct FailingHandler;

    #[async_trait]
    impl JobHandler for FailingHandler {
        async fn run(&self, _request: Request, _link: JobLink) -> eyre::Result<ServerFrame> {
            eyre::bail!("the workdir is outside the scope")
        }
    }

    #[tokio::test]
    async fn a_handler_error_reaches_the_client_as_a_failure_not_a_lost_link() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = spawn_daemon_with(tmp.path(), current_uid(), Arc::new(FailingHandler)).await;
        let err = dispatch_exec(
            &endpoint,
            req("true", &[]),
            mpsc::channel(8).0,
            CancellationToken::new(),
            None,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().contains("the workdir is outside the scope"),
            "{err}"
        );
        assert!(err.downcast_ref::<super::LinkLost>().is_none(), "{err}");
    }

    /// How a fake durable daemon answers one connection: output ranges to send,
    /// then whether to end the job or drop the link.
    struct Attempt {
        output: Vec<(u64, u64, &'static str)>,
        then: Then,
    }

    enum Then {
        Drop,
        Done(i32),
        Refuse(RefusalReason),
        Fail(&'static str),
    }

    /// A daemon that plays `attempts` in turn, one per connection, and keeps
    /// every request it was sent.
    async fn fake_durable_daemon(
        dir: &std::path::Path,
        attempts: Vec<Attempt>,
    ) -> (
        RunnerEndpoint,
        std::sync::Arc<std::sync::Mutex<Vec<super::RunRequest>>>,
    ) {
        let socket = dir.join("durable.sock");
        let listener = tokio::net::UnixListener::bind(&socket).unwrap();
        let seen = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let kept = std::sync::Arc::clone(&seen);
        tokio::spawn(async move {
            for attempt in attempts {
                let (mut stream, _) = listener.accept().await.unwrap();
                if let Some(ClientFrame::Request(request)) = read_framed(&mut stream).await.unwrap()
                {
                    if let Request::Run(run) = *request {
                        kept.lock().unwrap().push(run);
                    }
                }
                write_framed(
                    &mut stream,
                    &ServerFrame::Ready {
                        protocol_version: PROTOCOL_VERSION,
                    },
                )
                .await
                .unwrap();
                for (from, to, chunk) in attempt.output {
                    write_framed(
                        &mut stream,
                        &ServerFrame::Output {
                            stream: super::Stream::Stdout,
                            from,
                            to,
                            chunk: chunk.into(),
                        },
                    )
                    .await
                    .unwrap();
                }
                match attempt.then {
                    Then::Drop => drop(stream),
                    Then::Done(code) => {
                        write_framed(&mut stream, &ServerFrame::Done(ExecSummary::exited(code)))
                            .await
                            .unwrap()
                    }
                    Then::Refuse(reason) => {
                        write_framed(&mut stream, &ServerFrame::Refused { reason })
                            .await
                            .unwrap()
                    }
                    Then::Fail(message) => write_framed(
                        &mut stream,
                        &ServerFrame::Failed {
                            message: message.into(),
                        },
                    )
                    .await
                    .unwrap(),
                }
            }
        });
        (RunnerEndpoint::Unix(socket), seen)
    }

    async fn run_durable(
        endpoint: &RunnerEndpoint,
        exec: Option<ExecRequest>,
        timeout: Duration,
    ) -> (eyre::Result<ExecSummary>, String) {
        let (tx, mut rx) = mpsc::channel::<ServerFrame>(64);
        let drain = tokio::spawn(async move {
            let mut out = String::new();
            while let Some(frame) = rx.recv().await {
                if let ServerFrame::Stdout { chunk } = frame {
                    out.push_str(&chunk);
                }
            }
            out
        });
        let job = super::DurableJob {
            job: super::JobId::new("round-1"),
            scope: ScopeId::new("s1"),
            exec,
        };
        let summary =
            super::dispatch_job(endpoint, job, tx, CancellationToken::new(), None, timeout).await;
        (summary, drain.await.unwrap())
    }

    /// A lost link is met by attaching again from what is already held, and
    /// the command is offered each time so a start that never arrived still
    /// happens, once.
    #[tokio::test]
    async fn a_durable_job_is_attached_to_again_after_a_lost_link_and_read_on_from_its_offsets() {
        let tmp = tempfile::tempdir().unwrap();
        let (endpoint, seen) = fake_durable_daemon(
            tmp.path(),
            vec![
                Attempt {
                    output: vec![(0, 6, "hello ")],
                    then: Then::Drop,
                },
                Attempt {
                    output: vec![(0, 6, "hello "), (6, 11, "world")],
                    then: Then::Done(0),
                },
            ],
        )
        .await;
        let (summary, out) =
            run_durable(&endpoint, Some(req("claude", &[])), Duration::from_secs(30)).await;
        assert_eq!(summary.unwrap().exit_code, Some(0));
        assert_eq!(
            out, "hello world",
            "a repeated range is not delivered twice"
        );
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 2);
        assert_eq!(seen[1].from.stdout, 6);
        assert!(seen.iter().all(|run| run.exec.is_some()));
    }

    #[tokio::test]
    async fn a_job_the_worker_no_longer_has_is_an_error_not_a_retry() {
        let tmp = tempfile::tempdir().unwrap();
        let (endpoint, seen) = fake_durable_daemon(
            tmp.path(),
            vec![Attempt {
                output: Vec::new(),
                then: Then::Refuse(RefusalReason::UnknownJob),
            }],
        )
        .await;
        let (summary, _) = run_durable(&endpoint, None, Duration::from_secs(30)).await;
        let err = summary.unwrap_err().to_string();
        assert!(err.contains("no such job"), "{err}");
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn a_job_waits_for_the_one_before_it_in_its_scope_to_end() {
        let tmp = tempfile::tempdir().unwrap();
        let (endpoint, seen) = fake_durable_daemon(
            tmp.path(),
            vec![
                Attempt {
                    output: Vec::new(),
                    then: Then::Refuse(RefusalReason::ScopeBusy),
                },
                Attempt {
                    output: Vec::new(),
                    then: Then::Done(0),
                },
            ],
        )
        .await;
        let (summary, _) =
            run_durable(&endpoint, Some(req("claude", &[])), Duration::from_secs(30)).await;
        assert_eq!(summary.unwrap().exit_code, Some(0));
        assert_eq!(seen.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn a_request_the_worker_failed_is_not_asked_again() {
        let tmp = tempfile::tempdir().unwrap();
        let (endpoint, seen) = fake_durable_daemon(
            tmp.path(),
            vec![Attempt {
                output: Vec::new(),
                then: Then::Fail("the workdir is outside the scope"),
            }],
        )
        .await;
        let (summary, _) =
            run_durable(&endpoint, Some(req("claude", &[])), Duration::from_secs(30)).await;
        let err = summary.unwrap_err().to_string();
        assert!(err.contains("the workdir is outside the scope"), "{err}");
        assert_eq!(seen.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn reattaching_stops_when_the_jobs_time_runs_out() {
        let tmp = tempfile::tempdir().unwrap();
        let socket = tmp.path().join("nobody.sock");
        let (summary, _) = run_durable(
            &RunnerEndpoint::Unix(socket),
            Some(req("claude", &[])),
            Duration::from_secs(2),
        )
        .await;
        let err = format!("{:#}", summary.unwrap_err());
        assert!(err.contains("time ran out"), "{err}");
    }

    /// Over the hub-local socket an older daemon still serves a request that
    /// needs none of the newer fields, so a hub can deploy ahead of its image.
    #[tokio::test]
    async fn a_v1_daemon_over_unix_serves_a_request_needing_nothing_newer() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = fake_unix_daemon(tmp.path(), 1, Duration::ZERO, true).await;
        let (_out, _err, summary) = collect(&endpoint, req("true", &[])).await.unwrap();
        assert_eq!(summary.exit_code, Some(0));
    }

    /// A v1 daemon sends no heartbeats, so its silence during a long job must
    /// not be mistaken for death; only the request deadline bounds it.
    #[tokio::test]
    async fn a_v1_daemon_is_not_held_to_the_heartbeat_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = fake_unix_daemon(tmp.path(), 1, Duration::from_millis(600), true).await;
        let (tx, _rx) = mpsc::channel(8);
        let outcome = dispatch_with(
            &endpoint,
            Request::Exec(req("true", &[])),
            tx,
            CancellationToken::new(),
            None,
            Liveness {
                silence: Duration::from_millis(200),
                cancel_grace: Duration::from_millis(100),
            },
        )
        .await
        .unwrap();
        assert!(matches!(outcome, Outcome::Done(ref s) if s.exit_code == Some(0)));
    }

    /// A request that uses newer fields is refused as soon as an older daemon
    /// announces itself, before it can run the command without them.
    #[tokio::test]
    async fn a_request_needing_newer_fields_is_refused_by_an_older_daemon() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = fake_unix_daemon(tmp.path(), 1, Duration::ZERO, true).await;
        let mut scoped = req("true", &[]);
        scoped.scope = Some(ScopeId::new("s"));
        let err = collect(&endpoint, scoped).await.unwrap_err();
        assert!(err.to_string().contains("too old"), "{err}");
    }

    static PROTOCOLS_SEEN: std::sync::Mutex<Vec<(RunnerEndpoint, super::ProtocolEvent)>> =
        std::sync::Mutex::new(Vec::new());

    /// Every connection tells the process's observer the protocol its daemon
    /// announced, and a refusal what the request needed.
    #[tokio::test]
    async fn each_connection_reports_the_protocol_its_daemon_announced() {
        use super::{observe_protocol, ProtocolEvent};
        observe_protocol(std::sync::Arc::new(|endpoint, event| {
            PROTOCOLS_SEEN
                .lock()
                .unwrap()
                .push((endpoint.clone(), event))
        }));
        let current_dir = tempfile::tempdir().unwrap();
        let current =
            fake_unix_daemon(current_dir.path(), PROTOCOL_VERSION, Duration::ZERO, true).await;
        collect(&current, req("true", &[])).await.unwrap();
        let old_dir = tempfile::tempdir().unwrap();
        let old = fake_unix_daemon(old_dir.path(), 1, Duration::ZERO, true).await;
        let mut scoped = req("true", &[]);
        scoped.scope = Some(ScopeId::new("s"));
        let refused = collect(&old, scoped).await.unwrap_err();
        assert!(refused.to_string().contains("too old"), "{refused}");

        let seen = |endpoint: &RunnerEndpoint| -> Vec<ProtocolEvent> {
            PROTOCOLS_SEEN
                .lock()
                .unwrap()
                .iter()
                .filter(|(at, _)| at == endpoint)
                .map(|(_, event)| *event)
                .collect()
        };
        assert_eq!(seen(&current), [ProtocolEvent::Announced(PROTOCOL_VERSION)]);
        assert_eq!(
            seen(&old),
            [ProtocolEvent::TooOld {
                announced: Some(1),
                required: super::protocol::PROTOCOL_SCOPES,
            }]
        );
    }

    /// A daemon that goes quiet without heartbeats is declared dead at the
    /// liveness bound, so a job on a half-open link cannot hang the caller.
    #[tokio::test]
    async fn a_silent_daemon_is_declared_dead_at_the_liveness_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = fake_unix_daemon(tmp.path(), PROTOCOL_VERSION, Duration::ZERO, false).await;
        let (tx, _rx) = mpsc::channel(8);
        let err = dispatch_with(
            &endpoint,
            Request::Exec(req("true", &[])),
            tx,
            CancellationToken::new(),
            None,
            Liveness {
                silence: Duration::from_millis(300),
                cancel_grace: Duration::from_millis(100),
            },
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("no heartbeat"), "{err}");
    }

    /// A daemon that keeps a silent job alive with heartbeats is not declared
    /// dead, even with a liveness bound shorter than the job.
    #[tokio::test]
    async fn heartbeats_keep_a_silent_job_alive_past_the_liveness_bound() {
        let tmp = tempfile::tempdir().unwrap();
        let endpoint = spawn_daemon(tmp.path(), current_uid()).await;
        let (tx, _rx) = mpsc::channel(8);
        let outcome = dispatch_with(
            &endpoint,
            Request::Exec(req("sh", &["-c", "sleep 1"])),
            tx,
            CancellationToken::new(),
            None,
            Liveness {
                silence: Duration::from_secs(11),
                cancel_grace: Duration::from_secs(1),
            },
        )
        .await
        .unwrap();
        assert!(matches!(outcome, Outcome::Done(ref s) if s.exit_code == Some(0)));
    }

    mod tls {
        use super::*;

        struct Authority {
            cert: rcgen::Certificate,
            key: rcgen::KeyPair,
        }

        fn authority(name: &str) -> Authority {
            let mut params = rcgen::CertificateParams::new(Vec::<String>::new()).unwrap();
            params.is_ca = rcgen::IsCa::Ca(rcgen::BasicConstraints::Unconstrained);
            params
                .distinguished_name
                .push(rcgen::DnType::CommonName, name);
            let key = rcgen::KeyPair::generate().unwrap();
            let cert = params.self_signed(&key).unwrap();
            Authority { cert, key }
        }

        fn identity(ca: &Authority, common_name: &str, sans: &[&str]) -> TlsIdentity {
            let mut params = rcgen::CertificateParams::new(
                sans.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
            )
            .unwrap();
            params
                .distinguished_name
                .push(rcgen::DnType::CommonName, common_name);
            let key = rcgen::KeyPair::generate().unwrap();
            let cert = params.signed_by(&key, &ca.cert, &ca.key).unwrap();
            TlsIdentity {
                ca_pem: ca.cert.pem(),
                cert_pem: cert.pem(),
                key_pem: key.serialize_pem(),
            }
        }

        const HUB: &str = "workforce-hub";

        async fn spawn_tls_daemon(ca: &Authority, handler: Arc<dyn JobHandler>) -> String {
            let listener = Listener::Tls {
                addr: "127.0.0.1:0".parse().unwrap(),
                identity: identity(ca, "worker", &["localhost"]),
                accept_common_name: HUB.into(),
            };
            let acceptor = Acceptor::bind(listener).await.unwrap();
            let addr = acceptor.local_addr().unwrap();
            tokio::spawn(async move {
                let _ = serve_on(acceptor, handler).await;
            });
            addr.to_string()
        }

        fn endpoint(addr: &str, server_name: &str, identity: TlsIdentity) -> RunnerEndpoint {
            RunnerEndpoint::Tls(TlsEndpoint {
                addr: addr.into(),
                server_name: server_name.into(),
                identity,
            })
        }

        /// A remote worker is held to scopes, where remote daemons began, and
        /// above that only to what a request uses: a hub that rolls first
        /// keeps a worker a revision behind serving everything it supports.
        #[test]
        fn a_remote_worker_is_held_to_what_each_request_uses_above_the_scopes_floor() {
            let ca = authority("test ca");
            let remote = endpoint("127.0.0.1:1", "localhost", identity(&ca, HUB, &[]));
            let floor = super::super::protocol::PROTOCOL_SCOPES;
            assert_eq!(remote.minimum_protocol(), floor);
            assert!(floor < PROTOCOL_VERSION);
            assert_eq!(req("true", &[]).required_protocol().max(floor), floor);
        }

        #[tokio::test]
        async fn a_hub_with_the_right_certificate_is_served() {
            let ca = authority("test ca");
            let addr = spawn_tls_daemon(&ca, local()).await;
            let endpoint = endpoint(&addr, "localhost", identity(&ca, HUB, &[]));
            let (out, _err, summary) = collect(&endpoint, req("echo", &["over tls"]))
                .await
                .unwrap();
            assert_eq!(out.trim(), "over tls");
            assert_eq!(summary.exit_code, Some(0));
        }

        #[tokio::test]
        async fn a_client_from_another_authority_is_refused_before_any_frame() {
            let ca = authority("test ca");
            let other = authority("other ca");
            let addr = spawn_tls_daemon(&ca, local()).await;
            let mut stranger = identity(&other, HUB, &[]);
            stranger.ca_pem = ca.cert.pem();
            let endpoint = endpoint(&addr, "localhost", stranger);
            let err = collect(&endpoint, req("echo", &["never"]))
                .await
                .unwrap_err();
            assert!(
                err.to_string()
                    .contains("secure command runner unavailable"),
                "{err}"
            );
        }

        /// The handshake succeeds — the certificate is genuine — and the daemon
        /// then closes on the name, so the client sees the link go away before
        /// any frame and fails closed.
        #[tokio::test]
        async fn a_client_with_the_wrong_common_name_is_refused() {
            let ca = authority("test ca");
            let addr = spawn_tls_daemon(&ca, local()).await;
            let endpoint = endpoint(&addr, "localhost", identity(&ca, "intruder", &[]));
            let err = collect(&endpoint, req("echo", &["never"]))
                .await
                .unwrap_err();
            assert!(
                err.to_string()
                    .contains("secure command runner unavailable"),
                "{err}"
            );
        }

        #[tokio::test]
        async fn a_daemon_with_the_wrong_name_is_refused_by_the_client() {
            let ca = authority("test ca");
            let addr = spawn_tls_daemon(&ca, local()).await;
            let endpoint = endpoint(&addr, "elsewhere", identity(&ca, HUB, &[]));
            let err = collect(&endpoint, req("echo", &["never"]))
                .await
                .unwrap_err();
            assert!(err.to_string().contains("cannot connect"), "{err}");
        }

        /// Over TLS the daemon is by definition remote, so a v1 daemon is
        /// refused even for a request needing nothing newer.
        #[tokio::test]
        async fn a_v1_daemon_over_tls_is_refused() {
            let ca = authority("test ca");
            let server =
                super::super::tls::server_config(&identity(&ca, "worker", &["localhost"])).unwrap();
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap().to_string();
            tokio::spawn(async move {
                let (tcp, _) = listener.accept().await.unwrap();
                let mut stream = tokio_rustls::TlsAcceptor::from(server)
                    .accept(tcp)
                    .await
                    .unwrap();
                let _: Option<ClientFrame> = read_framed(&mut stream).await.unwrap();
                write_framed(
                    &mut stream,
                    &ServerFrame::Ready {
                        protocol_version: 1,
                    },
                )
                .await
                .unwrap();
                tokio::time::sleep(Duration::from_secs(5)).await;
            });
            let endpoint = endpoint(&addr, "localhost", identity(&ca, HUB, &[]));
            let err = collect(&endpoint, req("true", &[])).await.unwrap_err();
            assert!(err.to_string().contains("too old"), "{err}");
        }
    }

    /// A real toolchain build of a crate with a fetched dependency, with a
    /// *fresh* `CARGO_HOME` so cargo must write its registry/cache there — the
    /// agent-writable cache story that `echo`/`git init` never exercise. The
    /// controlled env carries `CARGO_HOME` from the parent (set just below), and
    /// we assert the registry got populated, proving the write path works under
    /// the `env_clear`'d allowlist. Gated via `RUN_EXEC_TOOLCHAIN_E2E=1` (needs
    /// network + cargo) so the default suite stays hermetic.
    #[tokio::test]
    async fn toolchain_cargo_build_with_dependency_e2e() {
        if std::env::var("RUN_EXEC_TOOLCHAIN_E2E").as_deref() != Ok("1") {
            eprintln!("skipping toolchain e2e (set RUN_EXEC_TOOLCHAIN_E2E=1 to run)");
            return;
        }
        let tmp = tempfile::tempdir().unwrap();
        let crate_dir = tmp.path().join("probe");
        std::fs::create_dir_all(crate_dir.join("src")).unwrap();
        std::fs::write(
            crate_dir.join("Cargo.toml"),
            "[package]\nname = \"probe\"\nversion = \"0.0.0\"\nedition = \"2021\"\n\n[dependencies]\nitoa = \"1\"\n",
        )
        .unwrap();
        std::fs::write(
            crate_dir.join("src/lib.rs"),
            "pub fn f() -> String { let mut b = itoa::Buffer::new(); b.format(7u32).to_string() }\n",
        )
        .unwrap();

        let cargo_home = tmp.path().join("cargo-home");
        std::fs::create_dir_all(&cargo_home).unwrap();
        // SAFETY: single-threaded test setup; the controlled env snapshots this.
        unsafe {
            std::env::set_var("CARGO_HOME", &cargo_home);
        }

        let endpoint = spawn_daemon(tmp.path(), current_uid()).await;
        let mut request = req("sh", &["-c", "cargo build 2>&1"]);
        request.workdir = Some(crate_dir.display().to_string());
        request.timeout_secs = 600;
        let (out, err, summary) = collect(&endpoint, request).await.unwrap();
        unsafe {
            std::env::remove_var("CARGO_HOME");
        }

        assert_eq!(
            summary.exit_code,
            Some(0),
            "cargo build failed: out={out}\nerr={err}"
        );
        assert!(
            cargo_home.join("registry").exists(),
            "cargo did not populate the agent-writable CARGO_HOME"
        );
    }
}
