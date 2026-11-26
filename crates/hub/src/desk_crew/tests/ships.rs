//! Act fully ships (DW-761), end to end on a Desk whose sessions keep
//! Claude Code transcripts and a GitHub that serves `acme/web#7`:
//!
//! - a review session the boss starts on the pull request reads its head
//!   from GitHub; its verdict, read from its transcript at that head, is
//!   recorded, and the boss merges the green pull request at that head;
//! - a verdict is recorded only from the boss's own review session, once
//!   its turn has ended, from a reply that has one, at the folder's and
//!   the pull request's head; a review asking for changes, or a branch the
//!   person took back, merges nothing;
//! - a directive that releases need the person's OK has the boss ask them
//!   at once instead of merging; without it, the boss merges;
//! - a plan's ticket, end to end (DW-765): its pull request green, Desk
//!   starts its review with no model asked, records the verdict, and the
//!   boss merges it with `crew.github_merge` and the next ticket starts;
//!   changes asked for go to the ticket's session, and its next head is
//!   reviewed again.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use wiremock::matchers::{body_partial_json, method, path, path_regex};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};
use workforce_desk::charge::How;
use workforce_desk::crew::Party;
use workforce_desk::model::SessionView;
use workforce_desk::plan::{PrRef, PrReview};

use super::super::directives::NAME as DIRECTIVES;
use super::super::github_seen::Pull;
use super::super::review::{tests::github, MERGE, NO_VERDICT, RECORD};
use super::act_fully::{git, repo};
use super::goals::{boss_over, run};
use super::plan_watch::{asked_with, turn_ends};
use super::plans::started;
use super::unbound::UNBOUND;
use super::*;
use workforce_desk::board::TicketState;
use workforce_desk::model::SessionState;
use workforce_desk::plan::{Draft, DraftTicket, ReviewLink};
use workforce_desk::NewPlan;

const MERGED: &str = "3333333333333333333333333333333333333333";

/// The boss at Act fully, its GitHub `gh` as the person's own key, over a
/// Desk whose sessions' Claude Code transcripts are under `base`'s `home`.
async fn shipping(
    base: &Path,
    pool: workforce_postgres::PostgresPool,
    gh: &MockServer,
) -> (Arc<AgentService>, Arc<CrewHub>, Desk, MockServer) {
    shipping_with(base, pool, gh, UNBOUND).await
}

/// [`shipping`], its boss set as `oversight` says.
async fn shipping_with(
    base: &Path,
    pool: workforce_postgres::PostgresPool,
    gh: &MockServer,
    oversight: &str,
) -> (Arc<AgentService>, Arc<CrewHub>, Desk, MockServer) {
    let home = base.join("home");
    std::fs::create_dir_all(home.join(".claude/projects/-acme-web")).unwrap();
    let mut options = options(base, installed_cli(base), not_installed(base));
    options
        .env
        .insert("HOME".into(), home.to_string_lossy().into_owned());
    let floor = workforce_desk::Floor {
        redactor: Some(Arc::new(super::inside::Hides)),
        ..Default::default()
    };
    let d = desk_on(pool, options, floor).await;
    let (overseer, crew, d, llm) =
        boss_over(base, d, oversight, gh, CancellationToken::new()).await;
    crew.read_spent_today(&crew.overseers()).await;
    // As the hub sets it at start.
    d.boss_bounds_with(workforce_desk::charge::Bounds {
        home: base.to_path_buf(),
        ..Default::default()
    });
    eventually("Desk to know the boss is unbound", async || {
        d.boss_unbound("desk").then_some(())
    })
    .await;
    (overseer, crew, d, llm)
}

/// `acme/web` checked out at `base/web` on `dw-761`, a commit ahead of
/// `main` and pushed, and a detached worktree of it at that commit in
/// `base/web-review`: its head.
fn pull_request(base: &Path) -> String {
    let web = base.join("web");
    repo(&web, "git@github.com:acme/web.git");
    git(&web, &["checkout", "-q", "-b", "dw-761"]);
    std::fs::write(web.join("picker.ts"), "export const picker = 1;\n").unwrap();
    git(&web, &["add", "picker.ts"]);
    git(&web, &["commit", "-q", "-m", "The date picker"]);
    git(&web, &["update-ref", "refs/remotes/origin/dw-761", "HEAD"]);
    let review = base.join("web-review");
    git(
        &web,
        &[
            "worktree",
            "add",
            "-q",
            "--detach",
            &review.to_string_lossy(),
            "dw-761",
        ],
    );
    head_of(&web)
}

/// `dir`'s HEAD commit.
fn head_of(dir: &Path) -> String {
    let out = crate::git_hardening::hardened_git()
        .as_std_mut()
        .arg("-C")
        .arg(dir)
        .args(["rev-parse", "HEAD"])
        .env("GIT_CONFIG_GLOBAL", "/dev/null")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .output()
        .unwrap();
    String::from_utf8(out.stdout).unwrap().trim().to_string()
}

/// GitHub merges `acme/web#7` once, squashed, at `head`.
async fn merges(gh: &MockServer, head: &str) {
    Mock::given(method("PUT"))
        .and(path("/repos/acme/web/pulls/7/merge"))
        .and(body_partial_json(
            json!({ "sha": head, "merge_method": "squash" }),
        ))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({ "merged": true, "sha": MERGED, "message": "Merged" })),
        )
        .mount(gh)
        .await;
}

/// How many merges GitHub was asked for.
async fn merges_asked(gh: &MockServer) -> usize {
    gh.received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "PUT")
        .count()
}

/// The implementing session in `base/web`, handed to the boss.
async fn implementing(d: &Desk, overseer: &AgentService, base: &Path) -> SessionView {
    let s = new_session(d, &base.join("web"), "web").await;
    let boss = Party::manager("desk", &super::super::name_of(overseer));
    d.take_on(s.id, &boss, How::Handed, &Default::default(), &By::you())
        .await
        .unwrap();
    s
}

/// The review session the boss starts on `acme/web#7`, in `base/web-review`.
async fn reviewing(d: &Desk, overseer: &AgentService, base: &Path) -> SessionView {
    let r = run(
        overseer,
        "crew.start_session",
        json!({
            "folder": base.join("web-review").to_string_lossy(),
            "goal": "Review acme/web#7",
            "review_of": { "repo": "acme/web", "number": 7 },
            "reason": "It is ready for review.",
        }),
    )
    .await;
    assert!(r.success, "{r:?}");
    eventually("its review session", async || {
        d.list().into_iter().find(|v| v.goal == "Review acme/web#7")
    })
    .await
}

/// A session's Claude Code, as its hooks and transcript say it works.
struct Claude {
    url: String,
    transcript: PathBuf,
}

static LINE: AtomicU32 = AtomicU32::new(0);

impl Claude {
    /// Session `s` started, its transcript under `base`'s `home`.
    async fn started(base: &Path, s: uuid::Uuid) -> Self {
        let url = hook_url(base, s).await;
        let transcript = base
            .join("home/.claude/projects/-acme-web")
            .join(format!("cc-{s}.jsonl"));
        std::fs::write(&transcript, "").unwrap();
        hook(
            &url,
            json!({"hook_event_name": "SessionStart", "session_id": format!("cc-{s}"),
                "transcript_path": transcript}),
        )
        .await;
        Self { url, transcript }
    }

    fn record(&self, line: Value) {
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .append(true)
            .open(&self.transcript)
            .unwrap();
        writeln!(f, "{line}").unwrap();
    }

    fn at() -> String {
        let n = LINE.fetch_add(1, Ordering::Relaxed);
        (chrono::Utc::now() + chrono::Duration::milliseconds(i64::from(n))).to_rfc3339()
    }

    /// It is asked `ask`, and works on it.
    async fn asked(&self, ask: &str) {
        self.record(
            json!({"type": "user", "uuid": uuid::Uuid::new_v4().to_string(),
            "timestamp": Self::at(), "message": {"role": "user", "content": ask}}),
        );
        hook(
            &self.url,
            json!({"hook_event_name": "UserPromptSubmit", "prompt": ask}),
        )
        .await;
    }

    /// Its turn ends replying `reply`.
    async fn replies(&self, d: &Desk, s: uuid::Uuid, reply: &str) {
        self.record(
            json!({"type": "assistant", "uuid": uuid::Uuid::new_v4().to_string(),
            "timestamp": Self::at(), "message": {"role": "assistant", "stop_reason": "end_turn",
            "content": [{"type": "text", "text": reply}]}}),
        );
        hook(
            &self.url,
            json!({"hook_event_name": "Stop", "transcript_path": self.transcript,
                "last_assistant_message": reply}),
        )
        .await;
        eventually("its turn to end", async || {
            d.view(s).ok().filter(|v| !v.state.busy())
        })
        .await;
    }
}

/// What `tool` answers `input` with, as the boss: its output, or why not.
async fn said(overseer: &AgentService, tool: &str, input: Value) -> Result<Value, String> {
    let r = run(overseer, tool, input).await;
    match r.success {
        true => Ok(r.output),
        false => Err(r.error.unwrap_or_default()),
    }
}

fn merge_7() -> Value {
    json!({ "repo": "acme/web", "number": 7, "reason": "Green, reviewed clean at its head." })
}

/// Review, record, merge: the review session reads the pull request's head
/// from GitHub, its verdict at that head is recorded, and the green pull
/// request merges at that head, squashed, as the boss's own decision.
#[tokio::test(flavor = "multi_thread")]
async fn a_pull_request_reviewed_clean_at_its_head_is_recorded_and_merged_at_that_head() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let head = pull_request(&base);
        let gh = MockServer::start().await;
        github(&gh, &head, json!(true)).await;
        merges(&gh, &head).await;
        let (overseer, _crew, d, _llm) = shipping(&base, pool, &gh).await;
        let since = chrono::Utc::now();
        let web = implementing(&d, &overseer, &base).await;

        let review = reviewing(&d, &overseer, &base).await;
        assert_eq!(
            d.store().pr_review_of(review.id).await.unwrap(),
            Some(ReviewLink {
                session: review.id,
                boss: "desk".into(),
                pr: PrRef::new("acme", "web", 7),
                head_sha: Some(head.clone()),
                for_session: Some(web.id),
            }),
            "linked to the session whose branch it is"
        );
        let claude = Claude::started(&base, review.id).await;
        claude.asked("Review acme/web#7.").await;
        claude
            .replies(
                &d,
                review.id,
                &format!("It reads well and the tests cover it.\n\n`REVIEW: clean {head}`"),
            )
            .await;
        let recorded = said(
            &overseer,
            RECORD,
            json!({ "session_id": review.id.to_string() }),
        )
        .await
        .unwrap();
        assert_eq!(recorded["recorded"], "clean", "{recorded}");
        assert_eq!(recorded["head_sha"], head.as_str());

        let merged = said(&overseer, MERGE, merge_7()).await.unwrap();
        assert_eq!(merged["merged"], "acme/web#7", "{merged}");
        assert_eq!(merged["merge_method"], "squash");
        assert_eq!(merged["sha"], MERGED);
        assert_eq!(merges_asked(&gh).await, 1);
        let decided = d.store().decisions_since("desk", since, 50).await.unwrap();
        let d7 = decided
            .iter()
            .find(|x| x.what == "merged acme/web#7")
            .expect("the merge is the boss's decision");
        assert_eq!(d7.session, Some(review.id));
        assert_eq!(d7.action["input"]["sha"], head.as_str());
    })
    .await;
}

/// A verdict is recorded only from the boss's own review session, once its
/// turn has ended, from a reply that has one, at the folder's and the pull
/// request's head; changes asked for, or a branch the person took back,
/// merge nothing.
#[tokio::test(flavor = "multi_thread")]
async fn a_review_is_recorded_only_as_its_session_said_it_at_the_head() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let head = pull_request(&base);
        let gh = MockServer::start().await;
        super::super::github::tests::no_writes(&gh).await;
        github(&gh, &head, json!(true)).await;
        let (overseer, _crew, d, _llm) = shipping(&base, pool, &gh).await;
        let web = implementing(&d, &overseer, &base).await;
        let record =
            |s: uuid::Uuid| said(&overseer, RECORD, json!({ "session_id": s.to_string() }));

        let not_review = record(web.id).await.unwrap_err();
        assert!(
            not_review.starts_with("It wasn't started to review a pull request"),
            "{not_review}"
        );
        let review = reviewing(&d, &overseer, &base).await;
        let claude = Claude::started(&base, review.id).await;
        claude.asked("Review acme/web#7.").await;
        eventually("it to be at work", async || {
            d.view(review.id).ok().filter(|v| v.state.busy())
        })
        .await;
        assert_eq!(
            record(review.id).await.unwrap_err(),
            "It's still reviewing: record its verdict once its turn has ended."
        );
        claude
            .replies(&d, review.id, "Looked through it all; it reads well.")
            .await;
        assert_eq!(record(review.id).await.unwrap_err(), NO_VERDICT);

        claude.asked("End with your verdict line.").await;
        claude
            .replies(
                &d,
                review.id,
                &format!("The empty state is missing.\n**REVIEW: changes {head}**"),
            )
            .await;
        let changes = record(review.id).await.unwrap();
        assert_eq!(changes["recorded"], "changes", "{changes}");
        let refused = said(&overseer, MERGE, merge_7()).await.unwrap_err();
        assert!(refused.contains("asked for changes"), "{refused}");

        claude.asked("The empty state is in: review again.").await;
        claude
            .replies(
                &d,
                review.id,
                &format!("Good now.\n  REVIEW: clean {head}."),
            )
            .await;
        assert_eq!(record(review.id).await.unwrap()["recorded"], "clean");
        let newest = d
            .store()
            .pr_review_newest("desk", &PrRef::new("acme", "web", 7))
            .await
            .unwrap()
            .unwrap();
        assert!(newest.clean);
        assert_eq!(newest.by_session, review.id);

        // The person took the branch's session back: it's theirs to merge.
        d.release(web.id, &By::you()).await.unwrap();
        let theirs = said(&overseer, MERGE, merge_7()).await.unwrap_err();
        assert!(
            theirs.contains("isn't the branch of a session you run"),
            "{theirs}"
        );

        // Its folder moved on from the head it reviewed.
        let folder = base.join("web-review");
        std::fs::write(folder.join("later.ts"), "export {};\n").unwrap();
        git(&folder, &["add", "later.ts"]);
        git(&folder, &["commit", "-q", "-m", "Later"]);
        let moved = record(review.id).await.unwrap_err();
        assert!(
            moved.starts_with(&format!(
                "Its folder is at {}, the PR is at {head}",
                head_of(&folder)
            )),
            "{moved}"
        );
        assert_eq!(merges_asked(&gh).await, 0);
    })
    .await;
}

/// The boss's model, scripted: told a green, reviewed pull request is
/// ready, it merges it; once the person's directive says releases need
/// their OK, which every turn is told, it asks them at once instead, and
/// nothing more merges.
#[tokio::test(flavor = "multi_thread")]
async fn a_directive_that_releases_need_the_persons_ok_has_the_boss_ask_instead_of_merge() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let head = pull_request(&base);
        let gh = MockServer::start().await;
        github(&gh, &head, json!(true)).await;
        merges(&gh, &head).await;
        let (overseer, _crew, d, llm) = shipping(&base, pool, &gh).await;
        let web = implementing(&d, &overseer, &base).await;
        d.store()
            .pr_review_put(&PrReview {
                boss: "desk".into(),
                pr: PrRef::new("acme", "web", 7),
                head_sha: head.clone(),
                clean: true,
                by_session: uuid::Uuid::new_v4(),
                at: chrono::Utc::now(),
            })
            .await
            .unwrap();
        let tell = async |text: &str| {
            d.line_call(web.id, "tell", &json!({ "to": "desk", "text": text }), None)
                .await
                .unwrap();
        };

        // At Act fully, recording and merging are the person's standing
        // choice, so a turn that has read a transcript still runs them.
        for tool in [MERGE, RECORD] {
            assert!(
                overseer.tools.get(tool).unwrap().standing_choice(&merge_7()),
                "{tool}"
            );
        }
        calls_tool_once(&llm, "is ready to merge", "crew_github_merge", merge_7()).await;
        tell("acme/web#7 is green and reviewed clean: it is ready to merge.").await;
        let merged = async || {
            for _ in 0..200 {
                if merges_asked(&gh).await == 1 {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            let line = overseer
                .sessions
                .find(&super::super::line::line_channel(web.id), PERSON)
                .ok()
                .flatten()
                .map(|l| l.messages.iter().map(|m| m.content.clone()).collect::<Vec<_>>());
            let asked: Vec<(bool, bool)> = llm
                .received_requests()
                .await
                .unwrap_or_default()
                .iter()
                .map(|r| String::from_utf8_lossy(&r.body).to_string())
                .filter(|b| b.contains("is ready to merge"))
                .map(|b| (b.contains("\"stream\":true"), b.contains("crew_github_merge")))
                .collect();
            let github: Vec<String> = gh
                .received_requests()
                .await
                .unwrap_or_default()
                .iter()
                .map(|r| format!("{} {}", r.method, r.url.path()))
                .collect();
            panic!("the boss didn't merge: line {line:?}; model (stream, has the tool) {asked:?}; github {github:?}");
        };
        merged().await;

        let added = said(
            &overseer,
            DIRECTIVES,
            json!({ "action": "add", "text": "Releases need my OK" }),
        )
        .await
        .unwrap();
        assert!(added.to_string().contains("Releases need my OK"), "{added}");
        let ask = json!({
            "text": "acme/web#7's release is ready: it needs your OK to go out.",
            "session_id": web.id.to_string(),
            "cant_decide": "Your directive: releases need your OK.",
        });
        // At Act fully the first ask is checked against the directives
        // once, then asked.
        for _ in 0..2 {
            calls_tool_once(&llm, "Releases need my OK", "crew_ask_person", ask.clone()).await;
        }
        tell("acme/web#7 carries the release now.").await;
        let item = eventually("the person is asked", async || {
            d.store()
                .items_not_closed(Some("desk"))
                .await
                .unwrap()
                .into_iter()
                .find(|i| i.session == Some(web.id) && i.text.contains("needs your OK"))
        })
        .await;
        assert_eq!(
            item.cant_decide.as_deref(),
            Some("Your directive: releases need your OK.")
        );
        assert_eq!(merges_asked(&gh).await, 1, "nothing more merged");
    })
    .await;
}

/// GitHub as it stands for `acme/web#7` from `dw-761`: its head is what
/// `head` holds now, every check on it passes, and it merges once asked.
struct Github {
    server: MockServer,
    head: Arc<parking_lot::Mutex<String>>,
    merged: Arc<AtomicBool>,
}

impl Github {
    async fn at(head: &str) -> Self {
        let server = MockServer::start().await;
        let head = Arc::new(parking_lot::Mutex::new(head.to_string()));
        let merged = Arc::new(AtomicBool::new(false));
        let (h, m) = (head.clone(), merged.clone());
        Mock::given(method("GET"))
            .and(path("/repos/acme/web/pulls/7"))
            .respond_with(move |_: &Request| {
                let merged = m.load(Ordering::SeqCst);
                ResponseTemplate::new(200).set_body_json(json!({
                    "number": 7, "state": if merged { "closed" } else { "open" },
                    "merged": merged, "draft": false,
                    "mergeable": true, "mergeable_state": "clean",
                    "head": {"ref": "dw-761", "sha": *h.lock(), "repo": {"full_name": "acme/web"}},
                    "base": {"ref": "main", "repo": {"full_name": "acme/web"}},
                }))
            })
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/web/pulls"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([
                {"number": 7, "state": "open", "merged_at": null,
                 "head": {"ref": "dw-761"}, "base": {"ref": "main"}},
            ])))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path_regex(
                r"^/repos/acme/web/commits/[0-9a-f]{40}/check-runs$",
            ))
            .respond_with(
                ResponseTemplate::new(200).set_body_json(json!({"check_runs": [
                    {"name": "Test", "status": "completed", "conclusion": "success"},
                ]})),
            )
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/repos/acme/web"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "full_name": "acme/web", "allow_squash_merge": true, "allow_merge_commit": true,
            })))
            .mount(&server)
            .await;
        let m = merged.clone();
        Mock::given(method("PUT"))
            .and(path("/repos/acme/web/pulls/7/merge"))
            .respond_with(move |_: &Request| {
                m.store(true, Ordering::SeqCst);
                ResponseTemplate::new(200)
                    .set_body_json(json!({ "merged": true, "sha": MERGED, "message": "Merged" }))
            })
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!([])))
            .mount(&server)
            .await;
        Self {
            server,
            head,
            merged,
        }
    }
}

/// The boss at Act fully running a plan of two tickets in `acme/web`, one
/// at a time, its first one's session started on `dw-761` with #7 open:
/// the plan's first ticket's session.
async fn a_plan_of_two(
    base: &Path,
    pool: workforce_postgres::PostgresPool,
    gh: &Github,
) -> (
    Arc<AgentService>,
    Arc<CrewHub>,
    Desk,
    MockServer,
    SessionView,
) {
    let one_at_a_time = format!("{UNBOUND}[oversight.work]\nparallel = 1\n");
    let (overseer, crew, d, llm) = shipping_with(base, pool, &gh.server, &one_at_a_time).await;
    *crew.close_settle.lock() = chrono::Duration::zero();
    let p = d
        .propose_plan(
            NewPlan {
                draft: Draft {
                    goal: "Ship the date picker".into(),
                    tickets: ["The date picker", "Its empty state"]
                        .into_iter()
                        .map(|title| DraftTicket {
                            title: title.into(),
                            folder: base.join("web").to_string_lossy().into_owned(),
                            ..Default::default()
                        })
                        .collect(),
                },
                board: None,
                parallel: None,
                engine: Engine::Claude,
                group: None,
                budget: None,
                result: None,
                goal_ticket: None,
            },
            &Party::manager("desk", "Desk"),
            &By::manager("desk", "Desk", "Sam asked for the date picker."),
        )
        .await
        .unwrap();
    d.approve_plan(p.summary.plan.id, 1, &By::you())
        .await
        .unwrap();
    let first = eventually("its first ticket's session", async || {
        started(&d).into_iter().find(|v| v.name == "DES-1")
    })
    .await;
    (overseer, crew, d, llm, first)
}

/// The review session Desk started of `acme/web#7` at `head`.
async fn reviewed_by_desk(d: &Desk, head: &str) -> SessionView {
    let r = eventually("Desk to start its review", async || {
        d.list()
            .into_iter()
            .find(|v| v.goal.starts_with("Review acme/web#7") && v.goal.contains(&head[..7]))
    })
    .await;
    assert_eq!(r.name, "Review DES-1");
    assert_eq!(r.branch, None, "detached");
    assert!(
        r.cwd
            .ends_with(&format!(".worktrees/review-7-{}", &head[..7])),
        "{}",
        r.cwd
    );
    assert_eq!(head_of(Path::new(&r.cwd)), head, "at the head it reviews");
    r
}

/// Waits for `review` to be closed and its worktree removed.
async fn closed(d: &Desk, review: &SessionView) {
    eventually("the review closed and its worktree removed", async || {
        (!Path::new(&review.cwd).exists()
            && d.view(review.id)
                .map_or(true, |v| v.state == SessionState::Closed))
        .then_some(())
    })
    .await;
}

/// The ticket's lifecycle (DW-765): its session opens #7 and CI goes
/// green; Desk starts the review with no model asked; its verdict is
/// recorded and the review closed; the boss's plan turn is told, merges
/// with `crew.github_merge` at the reviewed head and moves the ticket to
/// Done; the next ticket starts.
#[tokio::test(flavor = "multi_thread")]
async fn a_green_pull_request_is_reviewed_by_desk_merged_by_the_boss_and_the_next_ticket_starts() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let head = pull_request(&base);
        let gh = Github::at(&head).await;
        let (_overseer, _crew, d, llm, first) = a_plan_of_two(&base, pool, &gh).await;
        calls_tool_once(
            &llm,
            "Desk recorded a clean review of acme/web#7",
            "crew_github_merge",
            merge_7(),
        )
        .await;
        calls_tool_once(
            &llm,
            "merge_method",
            "crew_move_ticket",
            json!({ "ticket": "DES-1", "state": "done", "reason": "#7 merged." }),
        )
        .await;

        turn_ends(&d, &base, &first, "Opened #7; CI is green.").await;
        let review = reviewed_by_desk(&d, &head).await;
        assert_eq!(
            d.store().pr_review_of(review.id).await.unwrap(),
            Some(ReviewLink {
                session: review.id,
                boss: "desk".into(),
                pr: PrRef::new("acme", "web", 7),
                head_sha: Some(head.clone()),
                for_session: Some(first.id),
            })
        );
        assert_eq!(
            asked_with(&llm, &["acme/web#7"]).await,
            0,
            "no model chose to start it"
        );

        let claude = Claude::started(&base, review.id).await;
        claude.asked(review.last_prompt.as_deref().unwrap()).await;
        claude
            .replies(
                &d,
                review.id,
                &format!("It reads well.\n\nREVIEW: clean {head}"),
            )
            .await;
        eventually("the boss to merge #7", async || {
            gh.merged.load(Ordering::SeqCst).then_some(())
        })
        .await;
        let recorded = d
            .store()
            .pr_review_newest("desk", &PrRef::new("acme", "web", 7))
            .await
            .unwrap()
            .unwrap();
        assert!(recorded.clean);
        assert_eq!(
            (recorded.head_sha.as_str(), recorded.by_session),
            (head.as_str(), review.id)
        );
        let put: Vec<Value> = gh
            .server
            .received_requests()
            .await
            .unwrap()
            .iter()
            .filter(|r| r.method.as_str() == "PUT")
            .map(|r| serde_json::from_slice(&r.body).unwrap())
            .collect();
        assert_eq!(put.len(), 1);
        assert_eq!(
            (put[0]["sha"].as_str(), put[0]["merge_method"].as_str()),
            (Some(head.as_str()), Some("squash"))
        );

        eventually("the next ticket to start", async || {
            started(&d).into_iter().find(|v| v.name == "DES-2")
        })
        .await;
        closed(&d, &review).await;
        let des1 = d.store().linked_tickets(&[first.id]).await.unwrap();
        assert!(
            des1.iter().all(|t| t.state == TicketState::Done),
            "{des1:?}"
        );
    })
    .await;
}

/// A review that asks for changes merges nothing: Desk gives the changes
/// to the ticket's session, and its next green head is reviewed again.
#[tokio::test(flavor = "multi_thread")]
async fn a_review_asking_for_changes_goes_to_its_session_and_the_next_head_is_reviewed_again() {
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        let head = pull_request(&base);
        let gh = Github::at(&head).await;
        let (_overseer, crew, d, _llm, first) = a_plan_of_two(&base, pool, &gh).await;

        turn_ends(&d, &base, &first, "Opened #7; CI is green.").await;
        let review = reviewed_by_desk(&d, &head).await;
        let claude = Claude::started(&base, review.id).await;
        claude.asked(review.last_prompt.as_deref().unwrap()).await;
        claude
            .replies(
                &d,
                review.id,
                &format!("The empty state is missing.\n\nREVIEW: changes {head}"),
            )
            .await;
        let told = eventually("its session given the changes", async || {
            d.line_thread(first.id, None, Some(30))
                .await
                .unwrap()
                .into_iter()
                .find(|m| m.text.contains("asked for changes"))
        })
        .await;
        assert!(
            told.text.contains("> The empty state is missing."),
            "{}",
            told.text
        );
        assert!(
            told.text.contains("Don't merge it yourself."),
            "{}",
            told.text
        );
        let recorded = d
            .store()
            .pr_review_newest("desk", &PrRef::new("acme", "web", 7))
            .await
            .unwrap()
            .unwrap();
        assert!(!recorded.clean);
        closed(&d, &review).await;

        let web = base.join("web");
        std::fs::write(web.join("empty.ts"), "export const empty = 1;\n").unwrap();
        git(&web, &["add", "empty.ts"]);
        git(&web, &["commit", "-q", "-m", "The empty state"]);
        git(&web, &["update-ref", "refs/remotes/origin/dw-761", "HEAD"]);
        let next = head_of(&web);
        *gh.head.lock() = next.clone();
        crew.pull_seen(
            "acme/web",
            7,
            Pull {
                number: 7,
                open: true,
                head: Some(next.clone()),
                checks: Some("success".into()),
                ..Default::default()
            },
        );
        let first = d.view(first.id).unwrap();
        turn_ends(&d, &base, &first, "Pushed the empty state; CI is green.").await;
        reviewed_by_desk(&d, &next).await;
        assert!(!gh.merged.load(Ordering::SeqCst), "nothing merged");
    })
    .await;
}

/// A session's own merge at Act fully reaches its boss's decision as the
/// merge class, and is refused at once for `crew.github_merge`, with no
/// model asked.
#[tokio::test(flavor = "multi_thread")]
async fn a_sessions_own_merge_is_refused_at_once_for_its_bosss_merge() {
    use super::decides_at_once::{asked, behavior, calls_about};
    with_test_pool(workforce_desk::store::build_pool, |pool| async move {
        let dir = tempfile::tempdir().unwrap();
        let base = std::fs::canonicalize(dir.path()).unwrap();
        pull_request(&base);
        let gh = MockServer::start().await;
        super::super::github::tests::no_writes(&gh).await;
        let (overseer, _crew, d, llm) = shipping(&base, pool, &gh).await;
        let web = implementing(&d, &overseer, &base).await;
        let url = hook_url(&base, web.id).await;
        let answer = tokio::time::timeout(
            Duration::from_secs(10),
            asked(&url, "gh pr merge 7 --squash"),
        )
        .await
        .expect("decided within seconds")
        .unwrap();
        assert_eq!(behavior(&answer), "deny", "{answer}");
        let message = answer["hookSpecificOutput"]["decision"]["message"].to_string();
        assert!(
            message.contains(super::super::decide::OWN_MERGE)
                && message.contains(super::super::decide::OWN_MERGE_INSTEAD),
            "{message}"
        );
        assert!(
            calls_about(&llm, "gh pr merge").await.is_empty(),
            "no model asked"
        );
    })
    .await;
}
