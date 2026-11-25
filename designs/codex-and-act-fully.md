# Codex as a first-class engine, and Act fully that ships (DW-762, DW-761)

Revision 2: this revision takes in the first design review (H1–H5, M6–M11, L12–L14) and the person's 25 Nov boss chat (A1–A6).

## Goal

**A Codex session in Desk works as a Claude Code session does, across its whole lifecycle:**
- it starts in a mode that does not ask about ordinary work;
- the boss and the UI see its real state, transcript and pending approvals;
- every approval reaches whoever decides, and the answer reaches Codex within seconds;
- it resumes on every path;
- failures are reported, never silent.

**At Act fully, the boss is in control and works like a capable engineer:**
- the person's standing directives bind, both ways;
- everything else the boss decides itself, at once, merges included;
- it asks the person only when a directive requires it, or when only they can do the step;
- the plan keeps its N slots working: it merges green, reviewed PRs, closes finished sessions and pulls the next ticket.

## Principles

- **One path per concern, shared by both engines.** Where Codex forks today (transcript reads, failures, approval routing), the fork is removed and Codex feeds the shared path. Engine-specific code only translates Codex's app-server protocol into Desk's existing types: `SessionState`, `PendingApproval`, `TurnFailure`, transcript records.
- **Facts are checked against `codex-cli 0.166.0`.**
  - *(verified)* means run against the real 0.166 app-server, in a scratch `CODEX_HOME` with a stand-in Responses model.
  - *(source)* means read in rust-v0.166.0, cited as `codex-rs/<path>:<line>`.
- **Remove special cases rather than adding them.** Each part lists what goes.
- **Code rules for every PR:**
  - no TODOs and no partial implementations;
  - idiomatic, functional-style Rust that follows the crate's patterns (`text_enum!`, `Desk::change`, `CodexEvent`, `Follower`, `Verdict`);
  - no comment that is not strictly needed;
  - no duplicated code;
  - every behaviour below has a test named in part 10.

Line numbers are from origin/main 6d0970bc6. D = `crates/desk/src`, H = `crates/hub/src/desk_crew`.

---

## 1. Start, with a true auto mode

### Today
- `Mode::codex_permissions` (D/mode.rs:112) maps:
  - Default, AcceptEdits and Auto to `on-request` + `workspace-write`;
  - Plan to `on-request` + `read-only`;
  - Bypass to `never` + `danger-full-access`.
- `codex::Permissions` (D/codex.rs:82-118) adds the reviewer: `auto_review` for Auto or Approve for me, otherwise `user`. A mode Desk doesn't keep gives `None`, so the session runs in the person's own config (D/service.rs:5926).
- **New threads** take the settings as TUI `-c` keys (D/codex.rs:111, 135). **Resumed threads** take them as `thread/resume` params (D/codex.rs:162-186), because the TUI refuses permission `-c` keys on a `--remote` resume.
- **Live evidence:** the Eden rollouts (`turn_context`, 24–25 Nov) all show `on-request`, `workspace-write`, `network_access: false` and reviewer `user`. MAN-544 ran `read-only`.

### What's wrong
1. **Auto has network off.** *(verified)* A `curl` inside `workspace-write` fails with exit 7. So even in Auto every fetch, `gh` call, push or install escalates.
2. **Escalations under a boss are slow.** Each one under a boss's charge is a model call (part 3). That is the stall the person sees.
3. **There's no rule for which wins** when the person's mode, Approve for me and a boss's charge disagree.

### Change
- **`Mode::codex_permissions(self, charged: bool, approve_for_me: bool) -> Permissions`** replaces the tuple and the separate reviewer rule. `Permissions` gains `network: Network` (`Person | On`). `charged` means a boss's live charge holds the session (`charge_of(id).is_some_and(Charge::live)`). The table:

  | Mode | uncharged: approval / sandbox / network / reviewer | under a boss charge (Act or Act fully) |
  |---|---|---|
  | Default, AcceptEdits | on-request / workspace-write / Person / user (auto_review with Approve for me) | on-request / workspace-write / Person / user |
  | Auto | on-request / workspace-write / **On** / auto_review | on-request / workspace-write / Person / user |
  | Plan | on-request / read-only / n/a / user | unchanged |
  | BypassPermissions | never / danger-full-access / n/a / user | unchanged |

  - **The charge wins over Auto and Approve for me.** Under a boss, every escalation goes to the boss, so the person's directives bind networked steps: `git push origin main`, force pushes, `gh pr merge`, a curl to production, deploys. *(review H1, L12)*
  - **Plan and Bypass are the person's explicit choices** and stay as they are. A boss may keep a session in a mode that doesn't ask only where `modes.rs` already lets it (D/service/modes.rs:134, :185).
  - **Auto uncharged is Codex's equivalent of Claude Code's auto mode.** Codex's own reviewer (`auto_review`, a risk-judging subagent) answers what its model escalates, with network on.
  - `never` was rejected: it gives every sandbox failure straight back to the model, so ordinary steps outside the sandbox could never succeed.
  - Under a charge, the speed comes from part 3's fast path, not from widening the sandbox.
- **`Network::On`:** Desk sets `sandbox_workspace_write.network_access = true`. The person's own config can't be read reliably through config layers and profiles. So the truth is the `sandbox.networkAccess` the app-server reports on `thread/start` and `thread/resume`. *(verified that it's reported; review M6)* Desk keeps that value on the session and shows it. If it differs from what Desk asked for, Desk notes it in Activity with what was asked and what Codex runs in.
- **One `Permissions` value, two serializers.** *(review H2)*
  - **New threads keep TUI `-c` keys**, which the TUI forwards to its own `thread/start`, `sandbox_workspace_write` included *(source: `tui/src/app_server_session.rs:2043-2070`)*. That keeps the TUI's own settings, which a Desk-started thread would lose *(source: :2286-2316)*. `Permissions::args()` writes them.
  - **Resumes keep `thread/resume` params:** `Permissions::resume_params()` writes `approvalPolicy`, `sandbox`, `approvalsReviewer` and `config`.
  - Both come from the one value, and a unit test serializes each and asserts each field appears exactly once.
  - The prompt stays a TUI argument (D/codex.rs:144-146).
  - A TUI can't join a thread that has no rollout yet *(source: `rollout/recorder.rs:1001`, `thread-store/src/read_thread.rs:38-54`)*, so Desk never starts threads itself.
- **Commits still escalate.** `workspace-write` keeps `.git` read-only, so `git commit` asks for approval in every mode. *(review M11)* That is deliberate: Desk does not widen the sandbox with `runtime_workspace_roots`, since a writable `.git` would let hooks and config changes skip the boss as well. Under a charge, a commit is an ordinary class that part 3's fast path answers at once. In Auto uncharged, Codex's reviewer answers it.
- **When a charge is taken or released, Desk restarts a running session in place between turns,** only when its newly computed `Permissions` differ from those it launched with (kept on `Live`). A stopped session simply resumes with the new ones. A grant or charge edit that leaves `Permissions` unchanged restarts nothing. as `set_mode` already does (D/service/modes.rs:410-460, through `start_again`).
  - A `--remote` TUI re-sends its own approval and reviewer settings with every turn *(source: `tui/src/user_turn_submission.rs:127-133`)*. So the app-server's live thread settings can't be changed under it: the TUI's settings are the ones that count. A restart makes the new TUI join a thread that Desk resumed with the new `Permissions`, so the TUI holds them from then on.
  - Mid-turn, or while a request waits, the restart waits for the turn's end. It is triggered by the queue that `modes_changed` (D/service/charge.rs, `put_charge`) already drives, so no new trigger is added.
  - The resume response's `sandbox` and `approvalsReviewer` are the truth, as above.
  - A refused restart is a `TurnFailure`: "Desk couldn't apply this session's permissions".
  - `thread/settings/update` and the `experimentalApi` capability are not used.

## 2. Seeing inside a session

### Today
- **State.** `codex::observe` → `Follower::heard` → `interpret` (D/codex.rs:1001-1020) → `codex_loop` (D/service.rs:8099-8470).
  - `waitingOnApproval` becomes Blocked.
  - `idle` and `systemError` both become `CodexEvent::Idle`, then Done or Asking (D/codex.rs:1018; D/service.rs:8426-8431).
- **`crew.history`.** `session_said` (H/tools.rs:573) returns `last_prompt`, `last_reply`, `pending` and `terminal_text(id, 3000)`.
  - For Codex, the prompt and reply come from `read_codex_tail` (D/transcript.rs:338), only at Idle (D/service.rs:8316-8334).
- **`crew.inside`'s transcript facet** uses the shared `transcript_page` (D/service/sight.rs:1355), which already decodes rollouts (`ledger::codex_read`, D/sight/transcript.rs:145-147).
- **Unforwarded requests.** When Codex is waiting on a request Desk didn't take, a placeholder reads "Answer in the terminal" with `answerable: false` (D/service.rs:8406-8423).
- **The ticket** shows nothing of the session: D/service/board.rs touches it only at Closed (:1434, :1460).

### What's wrong
1. **A failed turn shows as Done.**
2. **`crew.history` reads the terminal screen**, not the transcript, for both engines.
3. **The ticket can't say** whether the session is working, waiting or failed.

### Change
- **`Failed` is derived from whichever comes first:** `turn/completed` carrying `turn.error`, or `thread/status/changed` to `systemError`. *(review M8)*
  - `Follower` keeps the turn id it last reported as failed. That closes the gap where the `settled` dedup would otherwise swallow the second signal and lose the error text.
  - When `systemError` arrives first, the error text is filled in from the following `turn/completed`.
  - It sets the shared `turn_failures` entry (D/service.rs:813, 2535), as Claude's `StopFailure` does (D/service.rs:7385), and state Done.
  - The existing tool-item `CodexEvent::Failed` is renamed `ToolFailed`; it still feeds only the breaker.
- **`crew.history` reads `transcript_page`** for both engines: its last N records, plus `pending`. `terminal_text` remains only as the stated fallback when there is no transcript.
- **The placeholder goes,** except for TUI-owned requests that Codex really waits on (part 3).
- **One formatter writes the session line** (state, the pending request's summary, or the failure) for the tile and for the ticket's session link.

## 3. Approvals, end to end

### Who decides

| Level | Who decides | Where |
|---|---|---|
| Watch, Suggest | the person | the UI: `decide` (D/service.rs:~4680) → `decide_codex` (:4841) |
| Act | the boss, within Desk's judge | `decide_loop` (H/decide.rs:225) → `boss_decides` (D/service/charge.rs:1349) → `boss::judge` + `charge::answerable` |
| Act fully | the boss, with only the person's directives as rules | the fast path below, else `decide_request` (H/decide.rs:770). An Allow skips the judge (D/service/charge.rs:1415). |
| Boss served from a grant | the person, for Codex | `engine::boss_safe` (D/engine.rs:275). Codex has no pre-run hook, so this stays. |
| Uncharged Auto | Codex's `auto_review` | inside Codex |

### Today
- `Follower::heard` (D/codex.rs:1318-1340) takes command and file-change approvals, `requestUserInput` and MCP elicitations.
- Any other request is left to the TUI (D/codex.rs:1246). That includes `item/permissions/requestApproval` *(source: `app-server-protocol/src/protocol/common.rs:1873`)*.
- `decide_loop` skips requests that aren't answerable (H/decide.rs:577-591).
- A request not seen whole goes to the person (H/decide.rs:594-604).
- An observer that ends clears every request without a word (D/service.rs:8465).

### What's wrong
1. **A permissions request is never forwarded,** so it stalls at every level.
2. **Every charged escalation is a model call:** up to 30 s, and Low-effort judgement (A6).
3. **A dead observer is silent.**

### Change
- **`ask_of` is an exhaustive match over 0.166's server-request methods**, with three outcomes. *(review H3)*
  - **Desk's to answer:** `Command`, `FileChange`, `Permissions`, `Question` and `Elicitation`. Each becomes a `PendingApproval`.
  - **`TuiOwned`:** `item/tool/call`, the auth-token refresh, attestation, `currentTime/read`, and the legacy v1 approvals. Desk observes these and never answers them: they belong to the TUI.
  - **Unknown:** a `warn!` and an Activity note naming the method.
    - Desk never answers it. If Codex is still waiting on it after 10 s (`waitingOnApproval` or `waitingOnUserInput`), the session shows "waiting in its terminal" with the method name.
    - That is the only placeholder left, and `answerable` is false.
    - `Decision::Terminal` for forwarded kinds (D/service.rs:4866-4876) goes.
- **The `Permissions` answer shape** *(source: `v2/permissions.rs:505-511`, `:787-806`; review H4)*:
  - **Allow:** `{"permissions": <exactly the requested profile's network and fileSystem>, "scope": "turn"}`.
  - **Deny:** `{"permissions": {}, "scope": "turn"}`.
  - Desk never answers with the session scope, and never adds `strictAutoReview`.
  - A unit test pins both JSON bodies.
- **Command answers** use `accept` (this command only). Desk never uses `acceptForSession` or the policy amendments, so a later, riskier command of the same shape always reaches the decider again.
- **A fast path keeps the boss instant** without unbinding directives. *(review H1, H2)*

  **An escalated command runs outside the sandbox** once allowed: `require_escalated` means unsandboxed. So the classes the fast path answers are narrow, and everything it can't read for certain goes to the model.

  1. **Splitting.** Each command is split with the existing shell `Walk` (D/computer/boss.rs) into its parts:
     - pipelines, `&&`, `||` and `;`;
     - `$( )` and backticks;
     - `sh -c`, `bash -c` and `eval` (their argument is walked again);
     - env-var prefixes;
     - `git -c` and `git --git-dir` options;
     - git aliases, resolved from the session folder's `git config --get-regexp ^alias\.`.

     Each part is classified, and the riskiest part's class is the command's class. Anything `Walk` can't parse is `Other`.
  2. **Classes** (one classifier, `Class`, shared with Act's judge):
     - **Cacheable:** `OwnBranchPush`, `Commit`, `LocalGit`, `FetchPull`, `GhRead`, `GhPrCreate`, `TestBuild`, `PackageInstall`.
     - **Never cached:** `DefaultBranchPush`, `ForcePush`, `RemoteDelete`, `TagPush`, `GhPrMerge`, `Deploy`, `ProdRead`, `Other`.
     - **`OwnBranchPush`** is a push whose `PushKind` (part 4) is `OwnBranch`. A refspec or option makes it something else:
       - `HEAD:<other>` (including `HEAD:main`) is `DefaultBranchPush` or `OtherRemote`;
       - `+branch`, `--force`, `-f` and `--force-with-lease` are `ForcePush`;
       - `--mirror` and `--all` are `Other`;
       - `:branch` and `--delete` are `RemoteDelete`;
       - `--tags` and `--follow-tags` are `TagPush`.
     - **`LocalGit`** covers:
       - `add`, `status`, `diff`, `log` and `show`;
       - `branch <new>` (creating one);
       - `switch <branch>`;
       - `checkout <x>`, only when `git rev-parse --verify refs/heads/<x>` succeeds in the session folder;
       - `stash`, `stash push`, `stash list`, `stash show`, `stash pop` and `stash apply`;
       - `rebase` onto the session's base, and `merge` of the base.

       These are `Other`: `reset --hard`, `clean`, any `checkout` with `--` or of a non-branch (`.` and paths included), `stash drop`, `stash clear`, and `branch -d` / `-D`.
     - **Git aliases** are resolved before classifying. An alias starting with `!` is a shell command: it is walked and classified as shell, so `!git push -f` is `ForcePush`.
     - **`Commit`** is `git commit` with any message options, with or without `--amend`. `--amend` on a commit that is already pushed is `Other`: Desk reads whether `HEAD` is reachable from `@{push}`.
     - **`FetchPull`** is `git fetch` and `git pull` (`--rebase` or `--ff-only`) from `origin`, or the branch's own upstream.
     - **`GhPrCreate`** is `gh pr create` with its head as the session's own branch.
     - **`TestBuild`** is:
       - `cargo test|build|check|clippy|fmt|nextest`;
       - `npm ci`, `pnpm install --frozen-lockfile`, `yarn install --frozen-lockfile`;
       - `pytest`, `python -m pytest`;
       - `go test|build|vet`;
       - `tsc`, `eslint`, `prettier --check`;
       - `mvn test|verify`, `gradle test|build`.

       Each must be invoked directly, with no `-c` / `--config` that sets a runner or command. `npm test`, `npm run test`, `pnpm test` and `yarn test` run a package script, so they are `Other`.

       **Accepted risk** *(review L3)*: `TestBuild` runs workspace-defined code (`build.rs`, test code, conftest, lifecycle hooks of a lockfile install). At Act fully it is accepted as part of what Act fully allows. At Act it is never cached.
     - **`PackageInstall`** is a registry install of named packages: `npm install <name>[@version]`, `pnpm add <name>`, `yarn add <name>`, `cargo add <name>`, `pip install <name>[==version]`, `uv add <name>`, `go get <module>@<version>`. A URL, path, git (`git+…`, `github:`) or tarball source is `Other`, as is `--index-url` / `--registry` pointing somewhere other than the default.
     - **Always `Other`,** never cached:
       - `make`, `just`, `npm run`, `pnpm run`, `yarn <script>`;
       - running any script or binary from the workspace;
       - `curl … | sh` and similar pipes into a shell;
       - `npm publish`, `cargo publish`, `twine upload` (or `Deploy`, when Desk recognises them as one).
     - **`GhRead`** is `gh pr view|checks|diff|list`, `gh run view|list|watch`, `gh issue view|list`, and `gh api` with no `-X`/`--method`, or with it set to `GET`. Any other `gh api` method, and `gh api graphql`, are `Other`.
     - **`ProdRead`** is never cached. Every production read is the model's call.
     - **A `Permissions` request,** and a file change outside the session's workspace, are always `Other`.
  3. **Standing verdicts.**
     - For each `(boss, session, directives_version, class)`, the boss model gives one verdict per cacheable class, in one call per session at the decision effort (part 9): Allow, or Ask with the directive it rests on. The session also fixes the repo.
     - **`directives_version`** is a hash of exactly the directive text that the per-request decision prompt reads for that session. `session_brief` (H/decide.rs:979-1052) and the cache share one function, `directive_words(boss, session)`, which joins two texts:
       - `orders_words(boss, Some(session))` (H/items.rs:1650-1692), session-scoped notes included;
       - `preferences_context` (H/preferences.rs:373).

       So the two inputs can't drift. Any change to that text bumps the version.
     - **Person words beat the cache.** The fast path is skipped for a session whose picture has person words (prompts, messages or answers, `person_spoke_at`) newer than its cached verdict. The next request re-asks.
     - **A chat with the boss suspends the cache.** From the moment the person sends the boss a chat message until that turn ends, the fast path is suspended for every session the boss runs, since the turn may record an order. The turn's end re-reads `directive_words`.
     - A directive that is conditional ("unless", "until", "only if") or timed ("after 6pm", "this week") makes the verdict Ask for every class it could touch. When the classifier can't tell which classes, it is Ask for all of them.
     - The cache is in memory. The verdicts are recorded as `BossDecision`s and are re-asked after a restart.
     - **At Act,** `TestBuild` and `PackageInstall` are never cached (they run unsandboxed code from the workspace or a registry). At Act fully they are.
  4. **Where the fast path runs.** It runs inside `boss_decides` (D/service/charge.rs:1349), after the hard refusals: agent settings (:1380), budget and breaker, and the person driving. So it can never answer what those refuse.
     - A session under an active `Hold` (part 4) never takes the fast path: every request goes to the model, with the hold in its prompt.
     - A cached Allow is answered within one 2 s sweep, with no model call. It is recorded as the boss's decision, naming the standing verdict and the class.
  5. **Everything else** (never-cached classes, Ask verdicts, held sessions) goes to today's per-request `decide_request`, with the class and push facts in its prompt. So under a charge `git push origin main`, a force push, a tag push, a merge, a deploy and a production read always reach the boss's own judgement.
- **Look-at and observer fixes.**
  - `look_at`'s Codex exemption (H/decide.rs:577-591) is removed.
  - The "not seen whole goes to the person" rule (H/decide.rs:594-604) is kept and covered by a test. *(review M10)*
  - A boss's Ask goes to the person at once through `crew.ask_person` (DW-755).
  - If the observer ends while the TUI lives, Desk reconnects with `again()` (D/codex.rs:1221) and backoff. Pending requests are kept until a reconnect either confirms them (Codex re-sends open requests on subscribe) or drops them. If reconnecting fails within 30 s, the session gets a `TurnFailure`: "Desk lost sight of this Codex session".
  - The reviewer-mismatch warning (D/service.rs:8249) compares against `Permissions::of(...).reviewer` for the session's mode and charge, not against a constant. *(review M9)*

## 4. What Act fully allows

**Today.** This is taken from the DW-761 findings comment (25 Nov):
- There is no Desk gate at Act fully (D/service/charge.rs:1415). Every refusal was the model's, steered by stale narrow-grant texts:
  - `decide.rs:64` `SYSTEM` and `:76`;
  - `watcher.rs:328` `STANDING_RULE`, and `:1111` `level_words(Full)`;
  - `line.rs:884`;
  - `tools.rs:1241` `crew.escalate`.
- The boss has no merge tool (H/github.rs:6, :34, :356).
- On 25 Nov the boss also invented a gate: "merging can trigger a live release, and the separate go-live approval hasn't been verified" (A4). No directive said so.

### Change
- **One text, `ACT_FULLY` in `H/watcher.rs`.** Each of `level_words(Full)`, the `line.rs:884` text, `SYSTEM`'s policy sentences, the ask clause of `STANDING_RULE` and `crew.escalate`'s Act-fully clause references it. It says:
  - **You are in control.** The person's standing directives bind, both ways.
  - **Ordinary development is allowed:** edits, tests, builds and commits; pushing the session's own branch; opening a PR; merging a reviewed, green one; the deploys the work calls for; the production reads the ticket needs.
  - **A gate exists only if a directive states it.** Never invent one ("this might trigger a release", "the person might prefer"). If a directive says releases need the person's approval, ask them at once with `crew.ask_person`, naming the directive. Never wait silently.
  - **Deny** only for a directive, or for a step plainly wrong for the work, saying what to do instead.
  - **Ask** only when a directive needs the person's say, or when only they can do the step.
- **`PushKind`** is `OwnBranch | DefaultBranch | OtherRemote | Force | Delete | Unknown`.
  - It is resolved in `boss_judges` (D/service/charge.rs:1075-1170) from `rev-parse`, `symbolic-ref` and `ls-remote`. For a bare push, `@{push}` and `push.default` give the destination.
  - It replaces the inline checks of `push` (D/computer/boss.rs:2719-2810). Act's judge uses it as before, and Act fully's classifier and prompt use it too: one classifier, no duplicated rule.
- **The review record** *(review H5, M4, M5)*: `PrReview { owner, repo, number, head_sha, clean: bool, by_session: Uuid, at }`.
  - **Storage:** table `pr_reviews`, in a new `crates/desk/migrations/V094__pr_reviews.sql`. It uses `CREATE TABLE IF NOT EXISTS`, with `owner TEXT NOT NULL` and `UNIQUE (owner, repo, number, head_sha)`. On a re-record at the same head, the verdict is updated.
  - **No version collision:** V093 is the latest migration on origin/main (6d0970bc6), and no remote branch adds a V094 (checked across all remote branches on 25 Nov). The PR re-checks this before merge, and renumbers if another PR has landed one.
  - **Written by `crew.record_review(session_id, pr)`,** which takes nothing else from its arguments. Desk verifies four things and refuses with the reason if any fails:
    1. the review session is one the boss runs, and its ticket is the PR's ticket;
    2. that session's folder `HEAD` equals the PR's current head SHA, which is recorded as `head_sha`;
    3. the verdict is read from that session's transcript through `transcript_page`. It is the last line matching `^REVIEW: (clean|changes) ([0-9a-f]{40})$` in its final reply. Desk itself adds the instruction. *(review M1)*
       - The start call that starts any session (`crew.start` and the plan runner's start) takes an optional `review_of: {repo, number}`. That is one argument on the one start path, rather than a second tool.
       - With it, Desk appends `REVIEW_VERDICT` (one constant in D/plan.rs) to the session's prompt, filled with the PR's current head SHA. It asks the session to end its final reply with exactly `REVIEW: clean <sha>` or `REVIEW: changes <sha>`, using the SHA it reviewed.
       - Desk keeps the link `session → (owner, repo, number)` in `pr_review_sessions (session_id PRIMARY KEY, owner, repo, number)`, created in the same migration. `record_review` finds the PR from that link, never from its arguments: its only argument is the session.
    4. that `<sha>` equals both the folder `HEAD` and the PR's current head. `clean` is the verdict word.

    A verdict on any other SHA is refused: "the review was of <sha>, the PR is at <head>". A transcript with no `REVIEW:` line is refused with: "Its final reply has no REVIEW line: nudge it to end with `REVIEW: clean|changes <sha>`, then record again." 
  - A record is stale once the PR's head SHA differs from `head_sha`.
  - `crew.overview` shows the newest record per PR.
- **`crew.github_merge(repo, number)`** is a write kept apart from `CALLS`, available at Act fully only and recorded as a `BossDecision`.
  - **Every condition must hold:**
    - the PR's head branch is the current branch of a session the boss runs, in that session's origin repo, and not a fork;
    - the PR is open and not a draft;
    - `mergeable` is true;
    - the checks are all success, with none pending or failed;
    - there is a `PrReview` with `clean = true` at the current head SHA.
  - If `mergeable` is null (GitHub still computing), it retries 3 times, 5 s apart, then returns "GitHub hasn't decided mergeability yet; try again", which the loop treats as wait-and-retry.
  - **Method:** `squash` when the repo allows it, otherwise `merge`. It never rebases.
  - It calls `pr_merge` with `sha` = the reviewed head, so GitHub refuses the merge if the head has moved. Every refusal is returned with its reason, and none is retried silently.
- **Holds** (A5) are `Hold { session, directive_id, question, until: Until }`, where `Until` is `PersonAnswers | DirectiveWithdrawn | Condition(String)`.
  - A hold without a directive id is refused.
  - `crew.ask_person` carries the question at once.
  - A hold ends when the person answers, the directive is withdrawn, or the boss records that the named condition is met. The 25 Nov "explicit merge hold" was a hold of this kind, and its condition was "CI green at the head".
  - `crew.nudge` and `LOOP` lose open-ended "hold for their word".

## 5. Resume on every path

**Today.** Every path reaches `resume_checked` (D/service.rs:4154) → `launch` (:5806) → Codex priming (:5928-5951):
- **the tile:** api.rs:935;
- **the boss:** handoff.rs:1043 and `crew.resume`;
- **Desk restart:** `restore` (:4491);
- **after End session:** `restore_removed` (D/service/lifecycle.rs:947);
- **a mode reset:** modes.rs:363.

**What's wrong.** A resume applies permissions only when Desk keeps a mode. Otherwise the session runs in whatever its thread last ran in.

**Change.** Every resume path takes part 1's `Permissions` (mode, Approve for me, charge) through the one priming function, and the response's `networkAccess` is recorded. No path-specific code.

## 6. The plan loop: N working slots, merge, next ticket

### Today
- `plan::holding` (D/plan.rs:1262-1276) counts a ticket in `Review` with a session as holding a slot, whatever the session is doing.
- `room` (D/plan.rs:1286) subtracts those. With PRs never merged, Review tickets fill every slot. That is the 25 Nov board: 3 slots, 0 Doing, 0 ready, 8 open sessions (A1).
- `LOOP` (`orchestrator.rs:81`) merges only "as the person's directives say".
- Finished or superseded sessions stay open, because `close_held` (H/closing.rs:485-500) and the boss only close what is plainly idle.

### Change
- **`holding(tickets, busy: impl Fn(&str) -> bool)`** counts a ticket in Review only while `busy(session)` is true: its session is Starting, Working or Blocked. A Review ticket whose session is Idle or Done waits on the loop's merge step, not on a slot.
  - **Callers to update:**
    - D/plan.rs:1280 (`held`), and D/plan.rs:1410 (`p.holding`, the plan view);
    - H/plan.rs:2296 and H/plan.rs:2572 (`plan::held`).

    Each passes `|s| desk.view(s).is_ok_and(|v| v.state.busy())`. `SessionState::busy` is one new method, shared with `charge::busy`.
  - **N+1:** when a Review session is resumed (by the person, or the boss addressing review) while N others work, it counts again and the plan is at N+1. Nothing is stopped: the plan never preempts work. No new start happens until the count is back below N. Tested.
- **`LOOP` (one text):**
  - on a green PR with a clean review at its head, call `crew.github_merge`, move the ticket to Done and start the next;
  - a clean review at the current head ends review;
  - on red CI, tell the session to fix and push;
  - never hold without a directive.
- **The plan watch's tick does the bookkeeping:**
  - It closes sessions that are finished: ticket Done or Dropped, Idle or Done, with no open request or line message.
  - It also closes superseded sessions: a newer session holds the same ticket. A superseded session with uncommitted work is handed to the newer session's folder check and named in Activity, never dropped.
  - It then fills `room` from `next_starts`. The invariant, tested: at every tick, the sessions working on the plan = min(N, ready + in work).
- **`Landed::Merged` triggers the next start.** No new loop state.

## 7. The boss's own session: delivery, typing, close

### A2: the false "you're typing in that terminal"
- **Evidence.**
  - The boss's message to MAN-835 waited with `Wait::Typing` (H/actions.rs:1126 is the wording). The person confirms they weren't typing.
  - `Wait::Typing` comes from `line::deliverable` (D/line.rs:1146-1148). Its `typed` input is `typed_since_idle` (D/service/line.rs:568-573): true when `pty.typed_at()` is later than `lines.spoke`, the last time the person prompted the session. `last_input` becomes `typed_at().max(draft_at)`.
- **Two flaws.**
  1. **It never expires.** The wheel's own rule (`last_input` + `charge::HOLD` = 2 min, D/service/charge.rs:600-610) does, but delivery doesn't use it.
  2. **`typed_at` is set by things that aren't the person typing now:**
     - any viewer input that doesn't begin with ESC, which includes a lone ESC, so the first half of an escape sequence split across two chunks counts as a key (D/pty.rs:331-333, :580);
     - Desk's own draft paste (D/service/line.rs:2967).

  A prompt submitted in the TUI does call `person_spoke`: Codex at D/service.rs:8136, Claude at :6745. So the block comes from a stray byte or a draft after the session's last prompt. Either one holds every boss message until the next prompt, with no expiry.
- **What triggered it on 25 Nov.** Terminal input isn't logged, so the exact byte can't be recovered from the logs. The flaw that let it block the boss indefinitely is certain.
- **PR 0 (quick, contained, ships first).**
  - Delivery uses the wheel's rule. `typed` is true only while the last keystroke is within `charge::HOLD` (or the boss's `hold_minutes`), and later than the session's last turn start. For Codex, the turn start is `turn/started`, whoever prompted.
  - `typed_since_idle` is deleted.
  - **Split ESC.** The `Pty` keeps a pending-ESC instant (`esc_at`, an `AtomicI64` beside `typed`).
    - A chunk that is exactly `\x1b` sets `esc_at` and stamps nothing.
    - The next chunk, if it arrives within 50 ms and starts with `[`, `]` or `O`, is the tail of an escape sequence: it clears `esc_at`, and neither part stamps.
    - Any other next chunk, or none, makes the pending ESC a key: `typed_at()` reads a pending `esc_at` older than 50 ms as the keystroke (a lazy stamp, so no timer).
    - `typed(bytes)` (D/pty.rs:331) keeps its rule for whole chunks.
  - **Drafts are their own state, never "typing".** Bracketed paste (DECSET 2004, D/vt.rs:251) is the terminal's mode, not a record of drafts, so it doesn't protect one.
    - The `Pty` gains `draft_at` (`AtomicI64`). `line_draft` (D/service/line.rs:2953) sets it instead of calling `person_typed`.
    - It is cleared at the session's next turn start: Claude's `UserPromptSubmit` / Codex's `turn/started`, the same events that call `person_spoke` (D/service.rs:6745, :8136).
    - `deliverable` takes `drafted: bool` beside `typed`, and returns a new `Wait::Draft` while a draft waits in the prompt. Its wording (H/actions.rs, beside :1126) is "the person's draft is waiting in its prompt: it gets this once they send or clear it". It is never shown as typing. `urgent.rs:280` lists it with the other waits.
  - When a message waits on Typing, Desk logs at info with the keystroke's age, so any recurrence leaves evidence.
- **At Act fully, the boss's messages queue and deliver the moment the session is free.** That is already the line's behaviour once Typing is truthful. The boss is told the wait reason and its expected end ("the person typed 40 s ago; it goes in within 80 s"), never "it can't take this now".

### A3: closing a session that has a pending request
- **Today.** `close_held` refuses: "It's asking something" (H/closing.rs:489-492).
- **Change.** At Act fully, the boss's close cancels a pending request **only when it is one the boss itself decides**: a command, file-change or permissions approval that is answerable and is in the boss's decide queue. Then it ends the session. The cancel is:
  - **Codex:** `cancel`, or the empty grant for a permissions request;
  - **Claude:** the hook's deny.

  Each cancel is recorded as the boss's decision. The refusal stays, at every level, for:
  - a request sent to the person (a boss Ask, one not seen whole, or one served from a grant);
  - a session under a `Hold`;
  - a question or input the session itself asked (`asked`, Asking).

## 8. Failure reporting: never silent

| Failure | Today | Change |
|---|---|---|
| App-server didn't start | `warn!` + error (D/service.rs:5917) | unchanged |
| Resume refused | Conflict with why (:5938-5951) | unchanged |
| TUI refused, or still starting | kill + `warn!` (D/service.rs:6238-6262) | still Starting at the watch's end → `TurnFailure` "Codex didn't come up", with its screen tail |
| Turn failed | shown as Done | `TurnFailure` with Codex's message (part 2) |
| Permissions not applied, or differ | none | Activity note; a refused update → `TurnFailure` (part 1) |
| Observer ended | `warn!`; requests cleared (:6347, :8465) | reconnect, else `TurnFailure` (part 3) |
| Unknown server request | left to the TUI | `warn!` + Activity, then "waiting in its terminal" (part 3) |
| Unparsable message | silent `continue` (D/codex.rs:~1625) | `warn!` naming its method, at most once per method per session |
| Subscribe error | `debug!` (D/codex.rs:1281, 1290) | `warn!`, handled as an ended observer |
| Merge refused | n/a | returned to the boss with its reason, and noted in Activity |
| Message waiting on Typing | silent | info log + the reason told to the sender (part 7) |

## 9. The boss's effort for decisions and plan turns (A6)

- **Today.**
  - `decision_call` (H/decide.rs:1178-1190) sends no effort and caps output at 400 tokens. It runs outside the chat turn's effort scope (`with_effort_override`, agent-runtime `llm/provider.rs:201`), so the model's default applies.
  - **The override isn't Anthropic-only.** Its type is `anthropic::Effort`, but `effective_effort()` is read by the Anthropic path (`llm/anthropic.rs:357`), Bedrock (`llm/bedrock_invoke.rs:63`) and the Responses path used for ChatGPT and OpenAI models (`llm/responses.rs:666`, mapped onto `low`…`xhigh`). So it reaches the boss's GPT model. Providers without reasoning ignore it.
  - The boss's chat and plan turns take the person's setting. It was Low on 25 Nov and High since.
- **Decision quality needs more than Low,** and it matters most where the judgement is made: per-request decisions, standing verdicts and plan turns.
- **Change.** The person's choice is kept, and Desk sets a floor.
  - Decision calls and standing-verdict calls run under `with_effort_override(max(person's effort, Medium))`.
  - Their output cap becomes 2,000 tokens, so a reasoning model isn't cut off before its verdict line. The verdict parser is unchanged.
  - Plan turns (plan watch wakes, merges, starts) also run at no less than Medium.
  - The person's higher choice (High, as now) always wins.
  - Chat replies keep exactly the person's setting.
  - Medium is the floor because these calls are small and frequent, the fast path keeps the per-request ones rare, and the floor costs little.
- **Latency against `WITHIN` = 30 s** (H/decide.rs:46). Decision calls aren't timed today: hub.log on 25 Nov has no durations for them. So the PR adds `elapsed_ms` and the effort used to `decide`'s log of each call, and a `desk_decide_seconds` histogram.
  - `WITHIN` stays at 30 s. A Medium call is a short prompt and at most 2,000 output tokens; at the providers' streaming rates that finishes well inside 30 s. A timeout is retried once (two attempts), and then goes to the person as today.
  - The fast path takes ordinary work off this path altogether.
  - T7 asserts each decision records `elapsed_ms`, and that a decision exceeding `WITHIN` leaves the request with the person, said once.

## 10. Tests

### Real-Codex harness (new, shared): `crates/desk/tests/codex_real/`
- **The binary.** The pinned `codex` 0.166.0 comes from `WORKFORCE_CODEX_BIN`, installed by CI with `npm i -g @openai/codex@0.166.0`. Locally, a missing binary skips the test with its reason. In CI it is required.
- **The CI runner** needs a working Linux sandbox for `workspace-write`: bubblewrap present and unprivileged user namespaces enabled. The job checks both first and fails with a clear message if either is missing. *(review L13)*
- **Scratch home.** `config.toml` in a scratch `CODEX_HOME` sets the stand-in provider with `stream_max_retries = 0` and `request_max_retries = 0`, so an HTTP 429 reaches `systemError` at once. It also sets `check_for_update_on_startup = false`. The key is `sk-test-not-real`.
- **The stand-in model** is an in-process axum Responses-API server serving SSE, scripted per test:
  - an assistant message;
  - `exec_command`;
  - `exec_command` with `sandbox_permissions: require_escalated`;
  - a `request_permissions` call;
  - a 429 usage-limit reply.
- **Desk under test.** The real `Desk` runs with `with_program(Engine::Codex, real codex)`, a test Postgres, and the TUI in Desk's pty.

### Tests
1. **T1, start:**
   - Auto uncharged reports `workspaceWrite` + `networkAccess: true` + `auto_review`, and a fetch from the stand-in's port runs with no approval.
   - Default reports what the scratch config says.
   - A scratch profile with network off in Auto gives the Activity mismatch note.
   - Under a charge, Auto reports reviewer `user` and network as configured.
   - `git commit` escalates in each mode, and under an Act-fully charge the fast path allows it within 2 s.
2. **T2, approvals:**
   - an escalated command shows as Blocked with its command;
   - it is answered by the person, by a scripted boss at Act, and at Act fully by the fast path and by the model path; each runs within 5 s of the answer;
   - a `request_permissions` call becomes a `Permissions` approval, and the JSON Desk sends is exactly the requested profile with `scope: "turn"`, or the empty grant on deny;
   - a `TuiOwned` request is never answered by Desk, and the TUI answers it;
   - an unknown method gives the note, then "waiting in its terminal";
   - a request not seen whole goes to the person.
3. **T3, seeing in:**
   - `crew.history` and `crew.inside` return the prompt, the command item and the reply from the rollout;
   - a 429 turn ends with `view.failure` set, in both event orders (`turn/completed` first, `systemError` first);
   - the ticket line shows each state.
4. **T4, observer loss:** reconnect keeps the pending request; a refused reconnect sets the failure.
5. **T5, resume on every path** (tile, boss, `restore` after restart, `restore_removed` after End session): each reaches Idle and keeps the conversation (the stand-in sees the earlier messages). After a turn submitted **through the TUI** (keys typed into the pty), the rollout's `turn_context` shows the session's `Permissions`: approval, sandbox, network and reviewer.
6. **T6, charge changes:** a grant edit that leaves the session's `Permissions` unchanged restarts nothing. Taking a charge between turns restarts Auto in place. After the next turn submitted through the TUI, `turn_context` shows reviewer `user` and network as configured. Releasing the charge restarts it back, and the next TUI turn shows `auto_review` and network on. A charge change mid-turn restarts only after the turn ends.
7. **T7, Act fully lifecycle** (hub: `tests/act_fully.rs` + `plan_watch.rs` harnesses, scripted model, wiremock GitHub):
   - a session pushes its branch and opens a PR, checks go green, and `crew.record_review` is clean at the head;
   - `crew.github_merge` merges with `sha` and `merge_method: squash`;
   - `Landed::Merged` fires, the ticket goes to Done, the finished session is closed, and the next ticket starts, keeping N working throughout;
   - a directive "releases need my OK" makes the boss call `crew.ask_person` at once, and without it the boss merges.
   - **Negative cases:**
     - under a charge, `git push origin main` reaches the boss's model path and is never fast-pathed;
     - a force push and another remote never classify as `OwnBranch`;
     - a merge is refused when the review is stale, the head is not a session's own branch, the PR is from a fork, `mergeable` stays null, or a check is pending.
   - A hold without a directive is refused, and a hold with `Condition` ends when it is met.
   - **Closing at Act fully:**
     - with a pending request the boss decides, it cancels the request (recorded) and closes;
     - with a request sent to the person, a held session, or a question the session asked, it is refused;
     - below Act fully, it is always refused.
   - **Fast path:**
     - each push spelling is classified: `HEAD:main`, `+b`, `--force-with-lease`, `--mirror`, `--all`, `:b`, `--delete`, `--tags`, `--follow-tags` are never `OwnBranchPush`;
     - `true && git push origin main`, `sh -c 'git push -f'` and an alias for a force push take the riskiest class;
     - an unparsable command is `Other`;
     - `gh api -X POST` and `gh api graphql` aren't `GhRead`;
     - `reset --hard`, `clean` and `checkout -- .` aren't `LocalGit`;
     - a held session never takes the fast path;
     - a changed directive source bumps the version and re-asks;
     - a conditional directive gives Ask;
     - at Act, `TestBuild` and `PackageInstall` are never answered from the cache;
     - a hard refusal (agent settings, budget) wins over a cached Allow.
   - **Slots:** a resumed Review session at N brings the plan to N+1 with no stop and no new start, until the count is below N.
   - **`crew.record_review`** refuses when:
     - the session isn't the boss's;
     - its HEAD isn't the PR head;
     - no `REVIEW:` line is in its transcript;
     - the `REVIEW:` line names an older SHA (the PR moved on after the review).
   - **Cache inputs:**
     - an order given in chat and not yet recorded blocks the cache until that turn ends;
     - a new preference bumps the version;
     - a session-scoped note changes only its own session's version;
     - person words newer than a verdict skip the fast path.
   - **A review that ends without the line:** `record_review` refuses and says to nudge; the boss nudges, the session ends with the line, `record_review` records it, and `crew.github_merge` merges.
   - `npm test` and `npm run test` get the same class in each mode: both are `Other` (a script), since `npm test` runs the package's `test` script. They are listed together in `TestBuild`'s exclusions.
   - **Class negatives:**
     - `make test`, `npm run build`, `just ci` and `./scripts/x.sh` are `Other`;
     - `curl … | sh` is `Other`;
     - `npm publish` isn't `PackageInstall` or `TestBuild`;
     - `npm install git+https://…`, `pip install ./pkg` and `pip install --index-url https://other/` are `Other`;
     - `cargo test --config 'target.x.runner="sh"'` is `Other`;
     - an `!`-alias doing `git push -f` is `ForcePush`;
     - `checkout <tag-or-sha>` and `checkout -- .` are `Other`;
     - `stash drop`, `stash clear` and `branch -D` are `Other`;
     - `commit --amend` on a pushed `HEAD` is `Other`.
8. **T8, delivery (PR 0):**
   - a keystroke older than the hold doesn't block a boss message;
   - a real keystroke within the hold does, and the sender is told the wait's end;
   - a Codex `turn/started` clears a keystroke;
   - a draft blocks with `Wait::Draft` (never `Typing`) until the next turn start, then delivers;
   - **split ESC, tail half:** `\x1b` then `[I` within 50 ms stamps nothing;
   - **split ESC, lone half:** `\x1b` alone, with nothing within 50 ms, stamps a keystroke at its own time, and so does `\x1b` followed by `a`.
9. **Unit tests:**
   - the permissions table, every mode × Approve for me × charge;
   - `V094__pr_reviews.sql` applies twice without error (`IF NOT EXISTS`), and the unique key rejects a duplicate row;
   - both serializers;
   - `PushKind` and `Class` for each spelling;
   - the `ask_of` match, with a recorded fixture of each 0.166 request;
   - the `Permissions` answer JSON;
   - `holding` with Review tickets in each session state;
   - the effort floor (`max`), keeping the person's higher choice.

The existing stand-in-`codex` tests (D/service/codex_*_tests.rs) stay for what the real binary can't stage, such as a refused TUI or a hung screen.

## Delivery

Each PR ships with its tests and green CI. Ordered so the plan moves first: *(review L14)*

- **PR 0, delivery truth** (part 7 A2, T8). It fixes the boss's messages being held by a stray byte or a draft, and makes a draft its own, truthful wait.
- **PR 1, Act fully ships** (parts 4, 6, 7 A3 and 9; T7, except its permissions-cancel case):
  - the `ACT_FULLY` text;
  - the merge tool and review record;
  - holds;
  - slot accounting;
  - closing finished and superseded sessions, and closing with a pending request;
  - the effort floor.

  It fixes the boss refusing pushes it should allow, never merging, slots sitting empty, and invented gates. It does not make Codex escalations fast.
- **PR 2, Codex permissions and approvals** (parts 1, 3 and 5; T1, T2, T5, T6, the harness, and T7's permissions-cancel case). It builds on PR 1, which carries `PushKind` and `Hold`. It makes Auto run without prompts, makes charged escalations instant through the fast path, makes permissions requests answerable, and gives every resume the right settings.
- **PR 3, seeing in and failures** (parts 2 and 8; T3, T4). It gives the true state and the transcript to the boss, the tile and the ticket, and makes failures loud.

**Order** *(review M2)*:
- PR 0 and PR 1 go first, in parallel.
- PR 2 follows PR 1.
- PR 3 can go alongside PR 1, but its removal of the "Answer in the terminal" placeholder waits for PR 2's `TuiOwned` arm.

**Out of scope:** the levels below Act fully keep today's behaviour, except that Act's judge shares `PushKind` and `Class`.
