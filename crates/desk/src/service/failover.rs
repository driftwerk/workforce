//! Moving sessions off a login that can't serve them, to another of the
//! person's logins of the same provider (never another provider's): one at
//! its plan's limit, or one the provider turns away (its organisation turned
//! Claude Code off, it is on hold). The login moved to is probed with a
//! one-word call first, so nothing moves onto a login that would refuse it
//! too. Each session is restarted once on the new login, told why and that
//! what it ran in the background stopped; the person is told in one line
//! (an Activity event the boss's DM carries), and, when no login has room,
//! once until that changes, with when each resets.

use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use uuid::Uuid;

use super::{identities, Desk, DeskError};
use crate::crew::{By, Event, Kind, Party};
use crate::engine::Engine;
use crate::engine_accounts::{self as accounts, AccountNeed, AccountToken, Probe, Side};
use crate::model::SessionState;

/// How long a login the provider turned away is left out of moves before
/// it is tried again (with a probe): the hub's own calls re-check as often.
pub(super) const REFUSED_RECHECK: chrono::TimeDelta = chrono::TimeDelta::hours(1);
/// How long a probe that found a login serving stands for further moves.
const VERIFIED_FOR: Duration = Duration::from_secs(5 * 60);
/// How long a login whose probe didn't answer is left out of moves, so
/// each look at the needs doesn't wait on it again.
const UNCHECKED_FOR: Duration = Duration::from_secs(10 * 60);
/// The longest a probe may take.
const PROBE_WAIT: Duration = Duration::from_secs(90);
/// The most moves one look at the needs makes: each login is tried once.
const MOVES_AT_MOST: usize = 8;
/// Where what the person was last told of no login having room is kept,
/// so it is said once until it changes, a restart too.
pub(super) const NO_ROOM_TOLD: &str = "accounts.noRoomTold";
/// The call a probe makes: one word, on the cheapest model, with no tools,
/// hooks, MCP servers or saved session.
const PROBE_ARGS: &[&str] = &[
    "-p",
    "--model",
    "haiku",
    "--output-format",
    "json",
    "--no-session-persistence",
    "--strict-mcp-config",
    "--tools",
    "",
    "--settings",
    r#"{"disableAllHooks":true}"#,
    "Reply with the single word OK.",
];

/// Why sessions move off a login.
#[derive(Debug, Clone)]
enum Why {
    /// It is at its plan's limit (since `at`), which resets at `until`.
    Limit {
        at: DateTime<Utc>,
        window: &'static str,
        until: Option<DateTime<Utc>>,
    },
    /// The provider turned it away (since `at`), for `reason`.
    Refused { at: DateTime<Utc>, reason: String },
}

/// One move a need asks for.
struct Step {
    need: String,
    engine: Engine,
    from: Side,
    to: Side,
    why: Why,
}

impl Desk {
    /// Session `id`'s call on the login `raw` was turned away by the
    /// provider for the account itself (`reason`, in plain words): the
    /// login is left out of moves and starts until it is re-checked, and
    /// what runs on it moves.
    pub(super) fn session_refused(&self, id: Uuid, raw: &identities::Raw, reason: &str) {
        let now = Utc::now();
        let (account, news) = {
            let st = self.0.state.lock();
            let Some(Some(account)) = Self::side_of_raw(&st, raw) else {
                return;
            };
            let news = !Self::refused_now(&st, account, now)
                || st.refused.get(&account).is_some_and(|r| r.reason != reason);
            (account, news)
        };
        tracing::warn!(session = %id, %account, reason, "a session's call found its login turned away by the provider");
        self.hub_refused(account, reason, now + REFUSED_RECHECK, news);
        if !news {
            // Already known: this session moves off it all the same.
            let desk = self.clone();
            tokio::spawn(async move { desk.accounts_moved().await });
        }
    }

    /// Checks that `side` (`None`: the CLI's own sign-in) of `engine`
    /// serves a call now, with a one-word probe, unless one found so in the
    /// last few minutes. A login at its limit is marked full, and one turned
    /// away is marked so: either way nothing moves to it. Only Claude Code is
    /// probed: another engine's login has its sign-in read, as before.
    pub(super) async fn verify_login(
        &self,
        engine: Engine,
        side: Option<Uuid>,
    ) -> Result<(), DeskError> {
        if engine != Engine::Claude {
            if let Some(id) = side {
                let a = self.account(id)?;
                if a.engine != engine {
                    return Err(DeskError::Invalid(format!(
                        "{} is a {} login, not {}'s",
                        a.name,
                        a.engine.title(),
                        engine.title()
                    )));
                }
                self.read_token(&a).await?;
            }
            return Ok(());
        }
        {
            // A probe's word stands a few minutes, unless the login was
            // found full or turned away since.
            let st = self.0.state.lock();
            let still = st
                .verified
                .get(&side)
                .is_some_and(|at| at.elapsed() < VERIFIED_FOR)
                && Self::full_now(&st, side).is_none()
                && side.is_none_or(|id| !st.refused.contains_key(&id));
            if still {
                return Ok(());
            }
        }
        let (name, token) = match side {
            Some(id) => {
                let a = self.account(id)?;
                if a.engine != engine {
                    return Err(DeskError::Invalid(format!(
                        "{} is a {} login, not {}'s",
                        a.name,
                        a.engine.title(),
                        engine.title()
                    )));
                }
                let token = self.read_token(&a).await?;
                (a.name, Some(token))
            }
            None => ("this computer's sign-in".to_string(), None),
        };
        let said = self.probe_claude(token.as_ref()).await;
        let now = Utc::now();
        match said.map(|out| accounts::claude_probe_said(&out)) {
            Ok(Probe::Serves) => {
                {
                    let mut st = self.0.state.lock();
                    st.verified.insert(side, Instant::now());
                    st.unchecked.remove(&side);
                }
                // One turned away before serves again: no longer so.
                if let Some(id) = side {
                    self.ok_seen(&identities::Raw::Account(id), now);
                }
                Ok(())
            }
            Ok(Probe::Full) => {
                self.0.state.lock().verified.remove(&side);
                {
                    let mut st = self.0.state.lock();
                    let raw = Self::raw_of(&st, side);
                    let s = Self::standing_mut(&mut st, &raw);
                    if s.full(now).is_none() {
                        s.hit_at = Some(now);
                        s.hit_until = None;
                        s.hit_window = None;
                        Self::wrote_hit(&mut st, &raw, identities::Writer::Pool);
                    }
                }
                self.keep_standing();
                tracing::info!(login = %name, "a login probed before a move is at its limit");
                Err(DeskError::Conflict(format!(
                    "{name} is at its plan's limit too: nothing was moved to it"
                )))
            }
            Ok(Probe::Refused(why)) => {
                self.0.state.lock().verified.remove(&side);
                if let Some(id) = side {
                    let news = !Self::refused_now(&self.0.state.lock(), id, now);
                    self.hub_refused(id, why, now + REFUSED_RECHECK, news);
                }
                Err(DeskError::Conflict(format!(
                    "{name} was turned away: {why}. Nothing was moved to it"
                )))
            }
            Ok(Probe::Failed(w)) | Err(w) => {
                tracing::warn!(login = %name, said = %w, "a login probed before a move didn't answer");
                self.0
                    .state
                    .lock()
                    .unchecked
                    .insert(side, (Instant::now(), name.clone()));
                Err(DeskError::Conflict(format!(
                    "Desk couldn't check that {name} can serve just now ({w}): nothing was moved"
                )))
            }
        }
    }

    /// One `claude -p` call on `token` (none: the CLI's own sign-in), as a
    /// session's would run, and what it printed.
    async fn probe_claude(&self, token: Option<&AccountToken>) -> Result<String, String> {
        use tokio::io::AsyncWriteExt;
        let program = self
            .program_of(Engine::Claude)
            .map_err(|e| e.to_string())?
            .to_path_buf();
        let base = self.0.state.lock().session_env.clone();
        let mut env = self.with_added(&base);
        for k in accounts::OUTRANKS_CLAUDE_TOKEN {
            env.remove(k);
        }
        env.remove(accounts::CLAUDE_TOKEN_ENV);
        // Its own folder: no project's settings or instructions are read.
        let cwd = self.0.opts.run_dir.join("probe");
        std::fs::create_dir_all(&cwd).map_err(|_| "Desk couldn't make a folder to probe in")?;
        let mut cmd = match token {
            Some(_) => {
                env.insert(
                    crate::token_pipe::FD_ENV.into(),
                    crate::token_pipe::FD.into(),
                );
                let (sh, args) = crate::token_pipe::one_off(&program, PROBE_ARGS);
                let mut c = tokio::process::Command::new(sh);
                c.args(args);
                c
            }
            None => {
                let mut c = tokio::process::Command::new(&program);
                c.args(PROBE_ARGS);
                c
            }
        };
        let run = async {
            let mut child = cmd
                .current_dir(&cwd)
                .env_clear()
                .envs(env)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .kill_on_drop(true)
                .spawn()
                .map_err(|_| "Claude Code couldn't be run".to_string())?;
            if let Some(mut stdin) = child.stdin.take() {
                if let Some(token) = token {
                    let mut line = token.expose().as_bytes().to_vec();
                    line.push(b'\n');
                    let _ = stdin.write_all(&line).await;
                }
            }
            child
                .wait_with_output()
                .await
                .map_err(|_| "Claude Code couldn't be run".to_string())
        };
        let out = tokio::time::timeout(PROBE_WAIT, run)
            .await
            .map_err(|_| "Claude Code didn't answer in time".to_string())??;
        let mut said = String::from_utf8_lossy(&out.stdout).into_owned();
        if said.trim().is_empty() {
            said = String::from_utf8_lossy(&out.stderr).into_owned();
        }
        Ok(said)
    }

    /// [`Self::fail_over`] on its own task, one run at a time: one asked
    /// for while another runs makes that one run once more.
    pub(super) fn fail_over_soon(&self) {
        use std::sync::atomic::Ordering;
        let desk = self.clone();
        tokio::spawn(async move {
            loop {
                let one = match desk.0.failing_over.try_lock() {
                    Ok(one) => one,
                    Err(_) => {
                        desk.0.fail_over_again.store(true, Ordering::SeqCst);
                        // The run may have ended just before the flag was
                        // set: then this one runs it.
                        match desk.0.failing_over.try_lock() {
                            Ok(one) => one,
                            Err(_) => return,
                        }
                    }
                };
                // Turned off meanwhile: nothing more moves.
                if !desk.auto_switch() {
                    return;
                }
                desk.0.fail_over_again.store(false, Ordering::SeqCst);
                desk.fail_over().await;
                drop(one);
                if !desk.0.fail_over_again.swap(false, Ordering::SeqCst) {
                    return;
                }
            }
        });
    }

    /// Moves what runs on a login that can't serve (at its limit, or turned
    /// away) to the login with most headroom that a probe finds serving,
    /// one need at a time; a login a probe finds full or turned away is
    /// marked so and the next is tried. When none has room, the person is
    /// told once.
    pub(super) async fn fail_over(&self) {
        let mut tried: std::collections::HashSet<String> = Default::default();
        for _ in 0..MOVES_AT_MOST {
            let needs = self.account_needs_all();
            let Some(step) = self.next_step(&needs, &tried) else {
                break;
            };
            tried.insert(step.need.clone());
            if let Err(e) = self.verify_login(step.engine, step.to.id).await {
                tracing::warn!(from = %step.from.name, to = %step.to.name, error = %e, "a login wasn't moved to: it can't serve");
                continue;
            }
            self.move_off(step).await;
        }
        self.tell_no_room().await;
    }

    /// The next move the needs ask for, not tried already.
    fn next_step(
        &self,
        needs: &[AccountNeed],
        tried: &std::collections::HashSet<String>,
    ) -> Option<Step> {
        let now = Utc::now();
        let st = self.0.state.lock();
        needs.iter().find_map(|n| match n {
            AccountNeed::Limit {
                id,
                engine,
                from,
                to,
                resets_at,
                ..
            } if !tried.contains(id)
                && !Self::unchecked_now(&st, to.id)
                && !Self::moving_off(&st, *engine, from.id).is_empty() =>
            {
                let standing = Self::standing_of(&st, from.id);
                let window = standing
                    .full_by(now)
                    .map_or("usage limit", |f| f.window_name());
                Some(Step {
                    need: id.clone(),
                    engine: *engine,
                    from: from.clone(),
                    to: to.clone(),
                    why: Why::Limit {
                        at: standing.live_hit().unwrap_or(now),
                        window,
                        until: *resets_at,
                    },
                })
            }
            AccountNeed::Refused {
                id,
                engine,
                account,
                reason,
                using: Some(to),
                ..
            } if !tried.contains(id)
                && !Self::unchecked_now(&st, to.id)
                && account.id.is_some_and(|a| Self::refused_now(&st, a, now))
                && !Self::moving_off(&st, *engine, account.id).is_empty() =>
            {
                Some(Step {
                    need: id.clone(),
                    engine: *engine,
                    from: account.clone(),
                    to: to.clone(),
                    why: Why::Refused {
                        at: account
                            .id
                            .and_then(|a| st.refused.get(&a))
                            .map_or(now, |r| r.at),
                        reason: reason.clone(),
                    },
                })
            }
            _ => None,
        })
    }

    /// The sessions of `engine` on `side` a move would take: not closed,
    /// not in a terminal of their own, and not already moving (one waiting
    /// for its turn to end is moved, and told of, once).
    fn moving_off(st: &super::State, engine: Engine, side: Option<Uuid>) -> Vec<Uuid> {
        st.records
            .values()
            .filter(|r| {
                r.engine == engine
                    && r.state != SessionState::Closed
                    && r.terminal_id.is_none()
                    && r.origin != crate::model::Origin::External
                    && Self::side_of_session(st, r.id) == Some(side)
                    && !st.switches.contains_key(&r.id)
            })
            .map(|r| r.id)
            .collect()
    }

    /// Carries out `step`: each session told why it moved, the move Desk's,
    /// and the person told in one line.
    async fn move_off(&self, step: Step) {
        let tz = self.timezone();
        let at_words = |t: DateTime<Utc>| when(t, tz);
        let (because, said_why) = match &step.why {
            Why::Limit { at, window, until } => (
                format!(
                    "{} hit its {window}{}",
                    step.from.name,
                    until
                        .map(|u| format!(" (it resets {})", at_words(u)))
                        .unwrap_or_default()
                ),
                format!("{} hit its {window} at {}", step.from.name, at_words(*at)),
            ),
            Why::Refused { at, reason } => (
                format!("{} was turned away: {reason}", step.from.name),
                format!(
                    "{} was turned away at {} ({reason})",
                    step.from.name,
                    at_words(*at)
                ),
            ),
        };
        let moving = Self::moving_off(&self.0.state.lock(), step.engine, step.from.id);
        if moving.is_empty() {
            return;
        }
        let now = at_words(Utc::now());
        let note = format!(
            "Desk moved this session from {} to {} at {now}, because {because}. The conversation \
             is the same; it runs on {} from here.",
            step.from.name, step.to.name, step.to.name
        );
        {
            let mut st = self.0.state.lock();
            for id in &moving {
                st.switch_notes.insert(*id, (step.to.id, note.clone()));
            }
        }
        let by = By::desk(&format!(
            "{because}, so Desk moved its sessions to {}",
            step.to.name
        ));
        let done = match self
            .switch_all(step.engine, step.from.id, step.to.id, by)
            .await
        {
            Ok(done) => done,
            Err(e) => {
                tracing::warn!(error = %e, "couldn't move sessions off a login that can't serve");
                let mut st = self.0.state.lock();
                for id in &moving {
                    st.switch_notes.remove(id);
                }
                return;
            }
        };
        let moved: Vec<Uuid> = done
            .now
            .iter()
            .chain(&done.after_turn)
            .chain(&done.next_start)
            .copied()
            .collect();
        {
            // Told only as it carries on now or after its turn: one that
            // moves at its next start was told nothing it would hear.
            let mut st = self.0.state.lock();
            for id in &done.next_start {
                st.switch_notes.remove(id);
            }
        }
        // Only those this move took: one already moving was told of then.
        let moved: Vec<Uuid> = moved.into_iter().filter(|id| moving.contains(id)).collect();
        {
            // A session it didn't take is told nothing of it later.
            let mut st = self.0.state.lock();
            for id in moving.iter().filter(|id| !moved.contains(id)) {
                st.switch_notes.remove(id);
            }
        }
        tracing::info!(
            from = %step.from.name,
            to = %step.to.name,
            moved = moved.len(),
            "sessions moved off a login that can't serve"
        );
        if moved.is_empty() {
            return;
        }
        let labels: Vec<(Uuid, String)> = moved
            .iter()
            .filter_map(|id| self.view(*id).ok())
            .map(|v| (v.id, v.label().to_string()))
            .collect();
        let names = told_apart(&labels);
        let text = format!(
            "{said_why}; I moved {} to {}.",
            and_list(&names),
            step.to.name
        );
        self.note(
            Event::new(Kind::LoginMoved, Party::desk(), None, text).detail(serde_json::json!({
                "from": { "id": step.from.id, "name": step.from.name },
                "to": { "id": step.to.id, "name": step.to.name },
                "sessions": moved,
                "why": match &step.why { Why::Limit { .. } => "limit", Why::Refused { .. } => "refused" },
            })),
        );
    }

    /// When sessions sit on a login that can't serve and no login of the
    /// same provider has room (each full, turned away, or not answering a
    /// probe), the person is told so once, with when each resets; again only
    /// once that changes. Whether it changed is read from the logins' states,
    /// never the words (a reset said "Fri 16:00" reads "16:00" on Friday).
    async fn tell_no_room(&self) {
        let needs = self.account_needs_all();
        let now = Utc::now();
        let tz = self.timezone();
        let said = {
            let mut st = self.0.state.lock();
            let stuck = |n: &AccountNeed| match n {
                AccountNeed::AllFull { engine, .. } => Some(*engine),
                // Turned away and not yet seen serving, as the parts say.
                AccountNeed::Refused {
                    account,
                    using: None,
                    engine,
                    ..
                } if account.id.is_some_and(|a| st.refused.contains_key(&a))
                    && !Self::moving_off(&st, *engine, account.id).is_empty() =>
                {
                    Some(*engine)
                }
                // A move it couldn't check: sessions still on a full login.
                AccountNeed::Limit {
                    engine, from, to, ..
                } if Self::unchecked_now(&st, to.id)
                    && !Self::moving_off(&st, *engine, from.id).is_empty() =>
                {
                    Some(*engine)
                }
                _ => None,
            };
            let engine = needs.iter().find_map(stuck);
            let (key, text) = match engine {
                None => (None, None),
                Some(engine) => {
                    let mut key = Vec::new();
                    let mut parts = Vec::new();
                    for a in st.accounts.iter().filter(|a| a.engine == engine) {
                        let full = Self::standing_of(&st, Some(a.id)).full_by(now);
                        // Turned away and not yet seen serving: so, until a
                        // probe or a turn on it says otherwise.
                        let (k, part) = match (st.refused.get(&a.id), full) {
                            (Some(r), _) => (
                                format!("refused:{}", r.reason),
                                format!("{} is turned away ({})", a.name, r.reason),
                            ),
                            (None, Some(f)) => (
                                format!(
                                    "full:{}:{}",
                                    f.window_name(),
                                    f.until.map(|u| u.to_rfc3339()).unwrap_or_default()
                                ),
                                match f.until {
                                    Some(u) => format!(
                                        "{}'s {} resets {}",
                                        a.name,
                                        f.window_name(),
                                        when(u, tz)
                                    ),
                                    None => format!(
                                        "{} is at its {}, no reset time yet",
                                        a.name,
                                        f.window_name()
                                    ),
                                },
                            ),
                            (None, None) if Self::unchecked_now(&st, Some(a.id)) => (
                                "unchecked".into(),
                                format!("{} didn't answer Desk's check just now", a.name),
                            ),
                            (None, None) => (
                                "unusable".into(),
                                format!("{} has no sign-in Desk can use", a.name),
                            ),
                        };
                        key.push(format!("{}={k}", a.id));
                        parts.push(part);
                    }
                    if engine == Engine::Claude && Self::unchecked_now(&st, None) {
                        key.push("own=unchecked".into());
                        parts.push(
                            "this computer's own sign-in didn't answer Desk's check just now"
                                .into(),
                        );
                    }
                    (
                        Some(format!("{}|{}", engine.as_str(), key.join("|"))),
                        Some(format!(
                            "No {} login has room to move your sessions to: {}.",
                            engine.title(),
                            parts.join("; ")
                        )),
                    )
                }
            };
            // Read and set together: two looks close together say it once.
            if key == st.no_room_told {
                return;
            }
            st.no_room_told = key.clone();
            drop(st);
            self.keep_state(NO_ROOM_TOLD, serde_json::json!(key));
            text
        };
        if let Some(text) = said {
            self.note(Event::new(Kind::LoginsFull, Party::desk(), None, text));
        }
    }

    /// Whether `side`'s last probe didn't answer, a short while ago.
    pub(super) fn unchecked_now(st: &super::State, side: Option<Uuid>) -> bool {
        st.unchecked
            .get(&side)
            .is_some_and(|(at, _)| at.elapsed() < UNCHECKED_FOR)
    }
}

/// Sessions' names for one line: one that two share is told apart by the
/// start of its id, in brackets.
fn told_apart(labels: &[(Uuid, String)]) -> Vec<String> {
    labels
        .iter()
        .map(
            |(id, label)| match labels.iter().filter(|(_, l)| l == label).count() {
                1 => label.clone(),
                _ => format!("{label} [{}]", &id.to_string()[..8]),
            },
        )
        .collect()
}

/// "18:53" today, "Fri 16:00" within the week, "21 Nov 16:00" further.
fn when(t: DateTime<Utc>, tz: chrono_tz::Tz) -> String {
    let local = t.with_timezone(&tz);
    let today = Utc::now().with_timezone(&tz).date_naive();
    let days = (local.date_naive() - today).num_days();
    match days {
        0 => local.format("%H:%M").to_string(),
        -6..=6 => local.format("%a %H:%M").to_string(),
        _ => local.format("%-d %b %H:%M").to_string(),
    }
}

/// "A", "A and B", "A, B and C".
fn and_list(names: &[String]) -> String {
    match names {
        [] => "its sessions".into(),
        [one] => one.clone(),
        [rest @ .., last] => format!("{} and {last}", rest.join(", ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn names_are_listed_as_a_sentence_says_them() {
        let n = |v: &[&str]| and_list(&v.iter().map(|s| s.to_string()).collect::<Vec<_>>());
        assert_eq!(n(&["A"]), "A");
        assert_eq!(n(&["A", "B"]), "A and B");
        assert_eq!(n(&["A", "B", "C"]), "A, B and C");
    }

    #[test]
    fn two_sessions_of_one_name_are_told_apart_by_their_ids() {
        let (a, b, c) = (Uuid::new_v4(), Uuid::new_v4(), Uuid::new_v4());
        let named = told_apart(&[
            (a, "auth-fix".into()),
            (b, "auth-fix".into()),
            (c, "invites".into()),
        ]);
        assert_eq!(named[0], format!("auth-fix [{}]", &a.to_string()[..8]));
        assert_eq!(named[1], format!("auth-fix [{}]", &b.to_string()[..8]));
        assert_eq!(named[2], "invites");
    }
}
