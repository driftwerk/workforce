//! Signing an engine's own CLI in on a terminal of the hub's own that no
//! one is shown, its screen read only for the page to open:
//!
//! - Codex's device sign-in (`codex login --device-auth`): the page and
//!   the one-time code to type there, which Desk shows on the person's own
//!   computer.
//! - Devin's login (`devin auth login`): 3000.12.57 first asks how to log
//!   in. Desk answers its exact picker, and only that, with Enter (its
//!   first option, the browser); Devin opens the page in the browser
//!   itself, shows no address, and finishes once the person signs in there
//!   ([`EngineSignInState::Browser`]). Anything else it asks ends the
//!   sign-in in plain words. With no browser (`--force-manual-token-flow`)
//!   it shows its page, which gives a code; the person pastes that into
//!   Desk and Desk types it into the CLI. That code is kept nowhere: not in
//!   a log, an event, this sign-in's view, or the screen, which is blinded
//!   (its output dropped unread) before Desk can be given a code, and wiped
//!   besides.
//!
//! The CLI's terminal is one no one is shown ([`Pty::spawn_unseen`]): the
//! hub answers every query its CLI waits on (colours, kitty keyboard
//! flags, cursor, device), or Devin never goes on past its picker.
//!
//! The CLI keeps the sign-in where its sessions read it. Nothing it signs
//! in with passes through Desk but a pasted code, on its way to the CLI.
//!
//! Every step a sign-in takes, and how it ended, is one log line: the
//! engine, the step, and (ended) why, with a page only ever as its scheme,
//! host and path; never a code, a token, a query or an email.
//!
//! A code is shown to the person, never logged or announced: Desk reads it
//! by asking for this sign-in.

use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use super::{Desk, DeskError};
use crate::engine::Engine;
use crate::pty::{Pty, Spawn};

/// How long a sign-in may take: Codex's code lasts 15 minutes.
const SIGN_IN_FOR: Duration = Duration::from_secs(16 * 60);
/// How long Devin's login may take to show something Desk knows (its
/// picker, or its page): well inside the 30 s Desk waits for a first step.
const DEVIN_SHOWS_IN: Duration = Duration::from_secs(20);
/// How often its screen is read.
const SIGN_IN_EVERY: Duration = Duration::from_millis(250);
const SIGN_IN_COLS: u16 = 400;
/// How long an ended one can still be read.
const KEPT_FOR: Duration = Duration::from_secs(60);

/// Where an engine's own sign-in is.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum EngineSignInState {
    Starting,
    /// Open `url` and type `code` there.
    Device {
        url: String,
        code: String,
    },
    /// Open `url`: the CLI finishes by itself once the person signs in.
    Page {
        url: String,
    },
    /// The CLI opened its sign-in page in this computer's browser itself,
    /// and finishes once the person signs in there: there is no address to
    /// show. `say` is what to tell the person ([`DEVIN_IN_BROWSER`]),
    /// `instead` the action beside it ([`DEVIN_PASTE_INSTEAD`]), which
    /// starts a new sign-in, `manual` (ending this one).
    Browser {
        say: String,
        instead: String,
    },
    /// Open `url` on the person's own computer, then paste the code it
    /// gives ([`Desk::engine_sign_in_token`]).
    Paste {
        url: String,
    },
    Done,
    Failed {
        why: String,
    },
}

impl EngineSignInState {
    /// Waiting in the browser Devin opened, said as Desk says it.
    pub fn browser() -> Self {
        Self::Browser {
            say: DEVIN_IN_BROWSER.into(),
            instead: DEVIN_PASTE_INSTEAD.into(),
        }
    }

    fn is_over(&self) -> bool {
        matches!(self, Self::Done | Self::Failed { .. })
    }

    /// Its step, as the wire names it.
    fn step(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Device { .. } => "device",
            Self::Page { .. } => "page",
            Self::Browser { .. } => "browser",
            Self::Paste { .. } => "paste",
            Self::Done => "done",
            Self::Failed { .. } => "failed",
        }
    }

    /// The page it shows, as a log may say it: its scheme, host and path,
    /// never its query (the login's state and challenge).
    fn page(&self) -> Option<String> {
        let (Self::Device { url, .. } | Self::Page { url } | Self::Paste { url }) = self else {
            return None;
        };
        let page = url::Url::parse(url).ok()?;
        Some(format!(
            "{}://{}{}",
            page.scheme(),
            page.host_str().unwrap_or_default(),
            page.path()
        ))
    }
}

/// How a sign-in ended, and why, for its one log line.
struct Ended {
    state: EngineSignInState,
    reason: &'static str,
}

impl Ended {
    fn done(reason: &'static str) -> Self {
        Self {
            state: EngineSignInState::Done,
            reason,
        }
    }

    fn failed(why: &str, reason: &'static str) -> Self {
        Self {
            state: EngineSignInState::Failed { why: why.into() },
            reason,
        }
    }
}

/// An engine's own sign-in, as Desk shows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineSignInView {
    pub id: Uuid,
    pub engine: Engine,
    #[serde(flatten)]
    pub state: EngineSignInState,
}

pub(super) struct EngineSignInRun {
    view: EngineSignInView,
    /// The CLI's terminal, until it ends.
    pty: Option<Arc<Pty>>,
    /// It was asked for the flow with no browser.
    manual: bool,
    /// A code was typed into it.
    typed: bool,
    /// Devin's picker was seen whole at the last look: Enter waits for a
    /// second, so the answers to the queries it drew it with are sent first.
    picker_seen: bool,
    /// Devin's picker was answered (Enter, once): never again.
    entered: bool,
}

/// What a person reads when the device sign-in ended without signing in.
pub const DEVICE_FAILED: &str =
    "ChatGPT's sign-in didn't finish. Start it again, and type the code on the page before it runs out.";
/// What they read when ChatGPT turned device sign-in away.
pub const DEVICE_REFUSED: &str =
    "ChatGPT didn't allow signing in with a code. Turn on device code sign-in in ChatGPT's security settings, then start it again.";

/// What a person reads when Devin's login ended without signing in.
pub const DEVIN_FAILED: &str =
    "Devin's sign-in didn't finish. Start it again, and sign in on the page before it times out.";
/// What they read when Devin turned the pasted code away.
pub const DEVIN_TOKEN_FAILED: &str =
    "Devin didn't take that code. Start the sign-in again, and paste the code the page gives you.";
/// What they read when Devin's login ended before showing its page.
pub const DEVIN_NO_PAGE: &str =
    "Devin's sign-in ended before it showed a page to sign in on. Start it again.";
/// What they read while Devin waits for them in the browser it opened
/// ([`EngineSignInState::Browser`]), [`DEVIN_PASTE_INSTEAD`] beside it.
pub const DEVIN_IN_BROWSER: &str = "Finish signing in in the browser window Devin opened.";
/// The way out of [`EngineSignInState::Browser`]: a new sign-in, `manual`.
pub const DEVIN_PASTE_INSTEAD: &str = "Paste a code instead";
/// What they read when Devin's login asked something Desk doesn't answer
/// (a changed picker, an enterprise prompt), or showed nothing Desk knows.
pub const DEVIN_ASKED_UNKNOWN: &str = "Devin's sign-in asked something Desk doesn't know yet. Paste a code instead, or sign in in a terminal with `devin auth login`.";
/// What they read when Devin, with no browser, showed a prompt but no page
/// Desk trusts.
pub const DEVIN_PAGE_UNKNOWN: &str = "Devin's sign-in showed no page Desk knows to sign in on. Sign in in a terminal with `devin auth login`.";
/// Devin couldn't listen for the browser's answer on this computer.
pub const DEVIN_NO_CALLBACK: &str = "Devin couldn't start its sign-in on this computer: another app may be using what it needs. Paste a code instead, or sign in in a terminal with `devin auth login`.";
/// The browser's answer never reached Devin.
pub const DEVIN_NO_ANSWER: &str =
    "Devin didn't hear back from the browser. Start the sign-in again, or paste a code instead.";
/// Devin's servers turned the sign-in away.
pub const DEVIN_EXCHANGE_FAILED: &str =
    "Devin's servers didn't finish the sign-in. Start it again in a minute.";
/// The whole sign-in ran past [`SIGN_IN_FOR`].
pub const DEVIN_TIMED_OUT: &str =
    "Devin's sign-in timed out. Start it again, and finish signing in within 15 minutes.";
/// Devin said it signed in, but `devin auth status` doesn't say so.
pub const DEVIN_NOT_CONFIRMED: &str =
    "Devin said it signed in, but it doesn't say so now. Start the sign-in again.";

/// What `codex login --device-auth` shows, read from its screen.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DeviceSaid {
    /// The page to open: only ever one of OpenAI's own.
    pub url: Option<String>,
    /// The one-time code to type there.
    pub code: Option<String>,
    /// It said device sign-in isn't allowed.
    pub refused: bool,
}

/// Codex's device page: exactly `https://auth.openai.com/codex/device`.
pub const DEVICE_PAGES: &[crate::engine_accounts::SignInPage] =
    &[crate::engine_accounts::SignInPage {
        host: "auth.openai.com",
        paths: &["/codex/device"],
        query: false,
    }];

/// Devin's login pages: its web app's, as 3000.12.57 shows it with no
/// browser
/// (`https://app.devin.ai/auth/cli/continue?state=…&prompt=select_account&code_challenge=…&code_challenge_method=S256&cli_pkce_marker=1`,
/// no `redirect_uri`), and Windsurf's
/// `https://windsurf.com/windsurf/signin?response_type=token&…`, which its
/// binary holds.
/// The query is the login's own (its state and PKCE challenge); nothing
/// else on those hosts, and no enterprise or beta host, is ever one.
pub const DEVIN_PAGES: &[crate::engine_accounts::SignInPage] = &[
    crate::engine_accounts::SignInPage {
        host: "app.devin.ai",
        paths: &["/auth/cli/continue"],
        query: true,
    },
    crate::engine_accounts::SignInPage {
        host: "windsurf.com",
        paths: &["/windsurf/signin"],
        query: true,
    },
];

/// What `devin auth login` shows, read from its screen.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DevinSaid {
    /// The page to open, the last it drew: only ever one of [`DEVIN_PAGES`].
    pub url: Option<String>,
    /// It fell back to waiting for a code pasted from that page, no
    /// browser having opened.
    pub manual: bool,
    /// It was signed in already.
    pub already: bool,
    /// Its picker, as 3000.12.57 draws it, with the browser chosen: Enter
    /// signs in in the browser.
    pub picker: bool,
    /// It asks something (a prompt's footer is drawn, or a question other
    /// than its picker's): with no [`Self::picker`], something Desk
    /// doesn't answer.
    pub asks: bool,
    /// It opened the browser and waits for the person there.
    pub in_browser: bool,
    /// It said it signed in.
    pub signed_in: bool,
    /// It said why it failed, as Desk says it, and that reason's name.
    pub failed: Option<(&'static str, &'static str)>,
}

/// Devin's picker (3000.12.57): its question, then its options; the
/// first, the browser, chosen (`❭`).
const DEVIN_ASKS_HOW: &str = "How would you like to log in?";
const DEVIN_BROWSER_CHOSEN: &str = "\u{276d} 1 Log in with browser";
/// What ends a prompt of Devin's: its keys.
const DEVIN_FOOTERS: &[&str] = &["esc cancel", "\u{21b5} confirm", "\u{21b5} submit"];
/// What Devin says when its login fails, as Desk says it.
const DEVIN_FAILURES: &[(&str, &str, &str)] = &[
    (
        "failed to start auth callback server",
        DEVIN_NO_CALLBACK,
        "callback server",
    ),
    (
        "failed to receive authorization code",
        DEVIN_NO_ANSWER,
        "no authorization code",
    ),
    (
        "token exchange failed",
        DEVIN_EXCHANGE_FAILED,
        "token exchange",
    ),
];

/// Reads a `devin auth login` screen. Escapes are read as a terminal
/// shows them: a hyperlink (OSC 8) is its text alone, read once.
pub fn devin_said(screen: &str) -> DevinSaid {
    let screen = crate::pty::plain_text(screen.as_bytes());
    let mut said = DevinSaid::default();
    let lines: Vec<&str> = screen.lines().map(str::trim).collect();
    for line in &lines {
        let lower = line.to_lowercase();
        for word in screen_words(line) {
            if let Some(page) = devin_page(word) {
                said.url = Some(page.as_str().to_string());
            }
        }
        // Only its falling back from the browser: a hint that a manual
        // flow exists is no reason to ask for a code.
        if [
            "falling back to manual",
            "switched to manual",
            "chose manual",
        ]
        .iter()
        .any(|w| lower.contains(w))
        {
            said.manual = true;
        }
        if lower.contains("already logged in") || lower.contains("already signed in") {
            said.already = true;
        }
        if lower.contains("waiting for login in browser") {
            said.in_browser = true;
        }
        if lower.contains("login successful") {
            said.signed_in = true;
        }
        if let Some((_, why, reason)) = DEVIN_FAILURES.iter().find(|(w, ..)| lower.contains(w)) {
            said.failed = Some((why, reason));
        }
        if DEVIN_FOOTERS.iter().any(|f| lower.contains(f))
            || (line.ends_with('?') && *line != DEVIN_ASKS_HOW)
        {
            said.asks = true;
        }
    }
    // Exactly its picker: the question, its first option the browser and
    // chosen, and no other option chosen.
    let asked = lines.iter().position(|l| *l == DEVIN_ASKS_HOW);
    said.picker = asked.is_some_and(|at| {
        let after = &lines[at + 1..];
        after.iter().find(|l| !l.is_empty()) == Some(&DEVIN_BROWSER_CHOSEN)
            && after
                .iter()
                .filter(|l| l.starts_with('\u{276d}'))
                .all(|l| *l == DEVIN_BROWSER_CHOSEN)
    });
    said
}

/// `word` as Devin's login page, read as a browser reads it: on
/// [`DEVIN_PAGES`], and carrying the login's own `state` and PKCE
/// `code_challenge` (a page cut short, or one without them, is none).
fn devin_page(word: &str) -> Option<url::Url> {
    let page = crate::engine_accounts::page_on(word, DEVIN_PAGES)?;
    let carries = |key: &str| page.query_pairs().any(|(k, v)| k == key && !v.is_empty());
    (carries("state") && carries("code_challenge")).then_some(page)
}

/// The words of a screen line a page or code may be.
fn screen_words(line: &str) -> impl Iterator<Item = &str> {
    line.split(|c: char| c.is_whitespace() || c == '│' || c == '"' || c == '\'')
        .map(|w| w.trim_matches(|c: char| matches!(c, '(' | ')' | '<' | '>' | ',' | '.')))
}

/// `url` as Codex's device page, read as a browser reads it.
fn device_page(url: &str) -> Option<url::Url> {
    crate::engine_accounts::page_on(url, DEVICE_PAGES)
}

/// A one-time code as Codex prints it: groups of capitals and digits
/// joined by `-` (`ABCD-12345`).
fn device_code(word: &str) -> bool {
    let groups: Vec<&str> = word.split('-').collect();
    (2..=3).contains(&groups.len())
        && word.len() <= 20
        && groups.iter().all(|g| {
            (3..=8).contains(&g.len())
                && g.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        })
}

/// Reads a `codex login --device-auth` screen.
pub fn device_said(screen: &str) -> DeviceSaid {
    let mut said = DeviceSaid::default();
    let mut after_code_line = false;
    for line in screen.lines() {
        let lower = line.to_lowercase();
        for word in screen_words(line) {
            if said.url.is_none() {
                said.url = device_page(word).map(|p| p.as_str().to_string());
            }
            if after_code_line && said.code.is_none() && device_code(word) {
                said.code = Some(word.to_string());
            }
        }
        if lower.contains("one-time code") || lower.contains("enter this code") {
            after_code_line = true;
        }
        if lower.contains("device code")
            && (lower.contains("not enabled")
                || lower.contains("disabled")
                || lower.contains("not allowed"))
        {
            said.refused = true;
        }
    }
    said
}

impl Desk {
    /// Starts `engine`'s own sign-in: Codex's device sign-in, or Devin's
    /// login (with `manual`, its flow for a computer with no browser; Codex
    /// has only the one). One at a time: a new one ends the last.
    pub fn start_engine_sign_in(
        &self,
        engine: Engine,
        manual: bool,
    ) -> Result<EngineSignInView, DeskError> {
        let args: &[&str] = match engine {
            Engine::Codex => &["login", "--device-auth"],
            Engine::Devin if manual => &["auth", "login", "--force-manual-token-flow"],
            Engine::Devin => &["auth", "login"],
            _ => {
                return Err(DeskError::Invalid(format!(
                    "Desk signs {} in another way",
                    engine.title()
                )))
            }
        };
        let program = self.program_of(engine)?.to_path_buf();
        let env = self.with_added(&self.0.state.lock().session_env.clone());
        // Devin signs in by its own login, never a key ahead of it.
        let env = match engine {
            Engine::Devin => crate::engine::devin_login_env(&env),
            _ => env,
        };
        let spec = Spawn {
            program,
            args: args.iter().map(|a| a.to_string()).collect(),
            cwd: env
                .get("HOME")
                .map(Into::into)
                .unwrap_or_else(std::env::temp_dir),
            env,
        };
        let (pty, exit) = Pty::spawn_unseen(&spec, SIGN_IN_COLS, crate::pty::DEFAULT_ROWS, 200)
            .map_err(|e| {
                tracing::warn!(
                    engine = engine.title(),
                    step = "failed",
                    reason = "the CLI didn't start",
                    "an engine's own sign-in ended"
                );
                DeskError::Internal(e.wrap_err(format!("starting {}'s sign-in", engine.title())))
            })?;
        let view = EngineSignInView {
            id: Uuid::new_v4(),
            engine,
            state: EngineSignInState::Starting,
        };
        let ended: Vec<Arc<Pty>> = {
            let mut st = self.0.state.lock();
            let older: Vec<Uuid> = st
                .engine_sign_ins
                .iter()
                .filter(|(_, r)| r.view.engine == engine)
                .map(|(id, _)| *id)
                .collect();
            let ended = older
                .iter()
                .filter_map(|id| st.engine_sign_ins.remove(id))
                .filter_map(|r| r.pty)
                .collect();
            st.engine_sign_ins.insert(
                view.id,
                EngineSignInRun {
                    view: view.clone(),
                    pty: Some(Arc::new(pty)),
                    manual: manual && engine == Engine::Devin,
                    typed: false,
                    picker_seen: false,
                    entered: false,
                },
            );
            ended
        };
        tracing::info!(
            engine = engine.title(),
            step = "starting",
            manual,
            replaced = ended.len(),
            "an engine's own sign-in moved"
        );
        ended.into_iter().for_each(|p| {
            tokio::spawn(end_cli(p));
        });
        tokio::spawn(self.clone().follow_engine_sign_in(view.id, exit));
        Ok(view)
    }

    /// Engine sign-in `id`, while it can be read.
    pub fn engine_sign_in(&self, id: Uuid) -> Result<EngineSignInView, DeskError> {
        self.0
            .state
            .lock()
            .engine_sign_ins
            .get(&id)
            .map(|r| r.view.clone())
            .ok_or(DeskError::NotFound)
    }

    /// Types the code the sign-in page gave (Devin's prompt says "Code")
    /// into sign-in `id`, which waits for one ([`EngineSignInState::Paste`]),
    /// then Enter; it says whether it took it by ending. The code is kept
    /// nowhere, and said nowhere.
    pub fn engine_sign_in_token(&self, id: Uuid, token: &str) -> Result<(), DeskError> {
        let token = token.trim();
        // Printable ASCII alone: a control character, a space or an escape
        // would be keys of their own in the CLI. 4 KiB is far past any
        // token; a terminal's line holds less still in canonical mode
        // (macOS's MAX_CANON is 1024), which the CLI's own read decides.
        if token.is_empty() || token.len() > 4096 || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(DeskError::Invalid(
                "paste the code the sign-in page gives you".into(),
            ));
        }
        let pty = {
            let mut st = self.0.state.lock();
            let run = st
                .engine_sign_ins
                .get_mut(&id)
                .filter(|r| matches!(r.view.state, EngineSignInState::Paste { .. }))
                .ok_or_else(|| {
                    DeskError::Conflict("that sign-in isn't waiting for a code".into())
                })?;
            let pty = run
                .pty
                .clone()
                .ok_or_else(|| DeskError::Conflict("that sign-in has ended".into()))?;
            run.typed = true;
            pty
        };
        // The bytes pass through the terminal's writer queue on their way
        // and are freed there, not zeroed; nothing keeps or logs them.
        pty.write(format!("{token}\r").as_bytes())
            .map_err(|_| DeskError::Conflict("that sign-in has ended".into()))
    }

    /// Ends an engine sign-in the person gave up on.
    pub fn cancel_engine_sign_in(&self, id: Uuid) -> Result<(), DeskError> {
        let run = self.0.state.lock().engine_sign_ins.remove(&id);
        let run = run.ok_or_else(|| DeskError::Conflict("that sign-in has ended".into()))?;
        tracing::info!(
            engine = run.view.engine.title(),
            step = "cancelled",
            from = run.view.state.step(),
            "an engine's own sign-in moved"
        );
        if let Some(pty) = run.pty {
            tokio::spawn(end_cli(pty));
        }
        Ok(())
    }

    /// The screen of sign-in `id` as Desk could still read it: none once
    /// its CLI ended.
    #[cfg(test)]
    pub(super) fn engine_sign_in_screen(&self, id: Uuid) -> Option<String> {
        let st = self.0.state.lock();
        st.engine_sign_ins
            .get(&id)
            .and_then(|r| r.pty.as_ref())
            .map(|p| p.text())
    }

    /// Sign-in `id` is at `state` now (never over: [`Self::follow_engine_sign_in`]
    /// ends it), said in one log line when it moved.
    fn engine_sign_in_moved(&self, id: Uuid, state: EngineSignInState) {
        let engine = {
            let mut st = self.0.state.lock();
            let Some(run) = st.engine_sign_ins.get_mut(&id) else {
                return;
            };
            if run.view.state == state {
                return;
            }
            run.view.state = state.clone();
            run.view.engine
        };
        tracing::info!(
            engine = engine.title(),
            step = state.step(),
            page = state.page(),
            "an engine's own sign-in moved"
        );
    }

    /// Where sign-in `id` is now, from what its CLI draws; how it ended,
    /// when what it drew ends it (the CLI is then still to be ended). A
    /// Devin waiting for a code is blind from the moment it is, so a code
    /// it echoes never reaches its screen; the screen is wiped at every
    /// look besides.
    fn engine_sign_in_looked(
        &self,
        id: Uuid,
        engine: Engine,
        pty: &Pty,
        since: Duration,
    ) -> Option<Ended> {
        let (state, manual, picker_seen, entered) = {
            let st = self.0.state.lock();
            let r = st.engine_sign_ins.get(&id)?;
            (r.view.state.clone(), r.manual, r.picker_seen, r.entered)
        };
        if matches!(state, EngineSignInState::Paste { .. }) {
            wipe(pty);
            return None;
        }
        if engine != Engine::Devin {
            let said = device_said(&pty.text());
            if let (Some(url), Some(code)) = (said.url, said.code) {
                self.engine_sign_in_moved(id, EngineSignInState::Device { url, code });
            }
            return None;
        }
        let said = devin_said(&pty.text());
        if let Some((why, reason)) = said.failed {
            return Some(Ended::failed(why, reason));
        }
        if let Some(url) = said.url.clone() {
            let next = match manual || said.manual {
                true => {
                    // Before Desk can be given a code: whatever the
                    // terminal shows from now on is never taken in.
                    pty.blind();
                    wipe(pty);
                    EngineSignInState::Paste { url }
                }
                false => EngineSignInState::Page { url },
            };
            self.engine_sign_in_moved(id, next);
            return None;
        }
        if entered {
            if said.in_browser {
                self.engine_sign_in_moved(id, EngineSignInState::browser());
            }
            return None;
        }
        if manual {
            // Its prompt for a code is drawn after its page: a prompt with
            // no page Desk knows above it has none to show.
            if said.asks {
                return Some(Ended::failed(DEVIN_PAGE_UNKNOWN, "no page Desk knows"));
            }
        } else if said.picker && said.asks {
            // Its picker, drawn to its footer, the browser chosen, at two
            // looks running (the answers its queries were owed are sent):
            // Enter, once.
            if let Some(run) = self.0.state.lock().engine_sign_ins.get_mut(&id) {
                run.entered = picker_seen;
                run.picker_seen = true;
            }
            if !picker_seen {
                return None;
            }
            if pty.write(b"\r").is_err() {
                return Some(Ended::failed(DEVIN_FAILED, "the CLI went before Enter"));
            }
            self.engine_sign_in_moved(id, EngineSignInState::browser());
            return None;
        } else if said.asks {
            return Some(Ended::failed(
                DEVIN_ASKED_UNKNOWN,
                "asked something unknown",
            ));
        }
        if since > DEVIN_SHOWS_IN {
            return Some(match manual {
                true => Ended::failed(DEVIN_NO_PAGE, "showed no page in time"),
                false => Ended::failed(DEVIN_ASKED_UNKNOWN, "showed nothing known in time"),
            });
        }
        None
    }

    /// How sign-in `id` ended, its CLI having exited with `code`.
    async fn engine_sign_in_outcome(
        &self,
        id: Uuid,
        engine: Engine,
        pty: &Pty,
        code: Option<u32>,
    ) -> Ended {
        let (state, typed) = {
            let st = self.0.state.lock();
            st.engine_sign_ins
                .get(&id)
                .map_or((EngineSignInState::Starting, false), |r| {
                    (r.view.state.clone(), r.typed)
                })
        };
        if matches!(state, EngineSignInState::Paste { .. }) {
            // What it left on screen may be the code: never read.
            wipe(pty);
            return match (code, typed) {
                (Some(0), _) => self.devin_confirmed("the CLI took the code").await,
                (_, true) => Ended::failed(DEVIN_TOKEN_FAILED, "the CLI turned the code away"),
                (_, false) => Ended::failed(DEVIN_FAILED, "the CLI ended before a code"),
            };
        }
        // Read the screen it left once more.
        tokio::time::sleep(SIGN_IN_EVERY).await;
        let screen = pty.text();
        match engine {
            Engine::Devin => {
                let said = devin_said(&screen);
                if let Some((why, reason)) = said.failed {
                    return Ended::failed(why, reason);
                }
                match code {
                    Some(0) => self.devin_confirmed("the CLI ended signed in").await,
                    _ if said.signed_in => self.devin_confirmed("the CLI said it signed in").await,
                    _ if said.already => self.devin_confirmed("already signed in").await,
                    _ if matches!(state, EngineSignInState::Browser { .. }) => {
                        Ended::failed(DEVIN_FAILED, "the CLI ended in the browser step")
                    }
                    _ if said.url.is_none() => {
                        Ended::failed(DEVIN_NO_PAGE, "the CLI ended before a page")
                    }
                    _ => Ended::failed(DEVIN_FAILED, "the CLI ended without signing in"),
                }
            }
            _ => {
                let said = device_said(&screen);
                match code {
                    Some(0) => Ended::done("the CLI ended signed in"),
                    _ if said.refused => Ended::failed(DEVICE_REFUSED, "device sign-in refused"),
                    _ => Ended::failed(DEVICE_FAILED, "the CLI ended without signing in"),
                }
            }
        }
    }

    /// Done when `devin auth status` now says Devin is signed in (read again
    /// now, so Desk says it at once); a plain failure when it doesn't.
    async fn devin_confirmed(&self, reason: &'static str) -> Ended {
        self.read_engine_now(Engine::Devin).await;
        match self.engine_read(Engine::Devin) {
            Some(crate::engine::SignIn::In { .. }) => Ended::done(reason),
            _ => Ended::failed(DEVIN_NOT_CONFIRMED, "auth status says signed out"),
        }
    }

    /// Follows the sign-in's screen until it ends, then reads the engine
    /// again, so what Desk says of its sign-in is now. How it ended is one
    /// log line.
    async fn follow_engine_sign_in(
        self,
        id: Uuid,
        mut exit: tokio::sync::oneshot::Receiver<crate::pty::Exit>,
    ) {
        let started = tokio::time::Instant::now();
        let Some(engine) = self.engine_sign_in(id).ok().map(|v| v.engine) else {
            return;
        };
        let ended = loop {
            let Some(pty) = ({
                let st = self.0.state.lock();
                st.engine_sign_ins.get(&id).and_then(|r| r.pty.clone())
            }) else {
                return; // cancelled, or a newer one began
            };
            if let Some(ended) = self.engine_sign_in_looked(id, engine, &pty, started.elapsed()) {
                end_cli(pty).await;
                break ended;
            }
            tokio::select! {
                code = &mut exit => {
                    break self.engine_sign_in_outcome(id, engine, &pty, code.ok().flatten()).await;
                }
                _ = tokio::time::sleep(SIGN_IN_EVERY) => {}
                _ = self.0.cancel.cancelled() => {
                    end_cli(pty).await;
                    return;
                }
            }
            if started.elapsed() > SIGN_IN_FOR {
                end_cli(Arc::clone(&pty)).await;
                break match engine {
                    Engine::Devin => Ended::failed(DEVIN_TIMED_OUT, "timed out"),
                    _ => Ended::failed(DEVICE_FAILED, "timed out"),
                };
            }
        };
        let Ended { state, reason } = ended;
        let done = state == EngineSignInState::Done;
        let from = {
            let mut st = self.0.state.lock();
            let Some(run) = st.engine_sign_ins.get_mut(&id) else {
                return; // cancelled as it ended
            };
            let from = run.view.state.step();
            run.view.state = state;
            // Its screen goes with it: nothing of it is kept.
            run.pty = None;
            from
        };
        match done {
            true => tracing::info!(
                engine = engine.title(),
                step = "done",
                from,
                reason,
                "an engine's own sign-in ended"
            ),
            false => tracing::warn!(
                engine = engine.title(),
                step = "failed",
                from,
                reason,
                "an engine's own sign-in ended"
            ),
        }
        // Devin's was read already, to say it was done.
        if done && engine != Engine::Devin {
            self.read_engine_now(engine).await;
        }
        let desk = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(KEPT_FOR).await;
            let mut st = desk.0.state.lock();
            if st
                .engine_sign_ins
                .get(&id)
                .is_some_and(|r| r.view.state.is_over())
            {
                st.engine_sign_ins.remove(&id);
            }
        });
    }
}

/// How long a CLI told to hang up has before it is killed.
const HANG_UP_FOR: Duration = Duration::from_secs(1);

/// Ends a sign-in's CLI and everything it started: its screen is wiped and
/// blinded, its process group (it leads its own session) is told to hang
/// up, and killed a second later (or as soon as the CLI itself has gone).
/// The group is signalled even just after the CLI was waited for: what it
/// started may be ignoring the hang-up, and a group id isn't given to
/// another while one of its members lives. Once the group is empty its id
/// can be given out again, so a CLI waited for longer than [`HANG_UP_FOR`]
/// before this began is not signalled at all, and the CLI's own pid is never
/// signalled once it was waited for.
async fn end_cli(pty: Arc<Pty>) {
    let began = std::time::Instant::now();
    pty.blind();
    wipe(&pty);
    let Some(pid) = pty.pid().and_then(|p| libc::pid_t::try_from(p).ok()) else {
        pty.kill();
        return;
    };
    if pty
        .reaped_at()
        .is_some_and(|at| began.saturating_duration_since(at) > HANG_UP_FOR)
    {
        return; // long gone: its group id may be another's by now
    }
    // SAFETY: killpg only sends a signal, to the CLI's own process group.
    let signal = |sig| unsafe { libc::killpg(pid, sig) };
    signal(libc::SIGHUP);
    let until = tokio::time::Instant::now() + HANG_UP_FOR;
    while !pty.reaped() && tokio::time::Instant::now() < until {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    signal(libc::SIGKILL);
    if !pty.reaped() {
        pty.kill();
    }
}

/// Wipes a sign-in's screen and the history behind it.
fn wipe(pty: &Pty) {
    pty.screen()
        .reset(SIGN_IN_COLS, crate::pty::DEFAULT_ROWS, 0, &[]);
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: &str = "Welcome to Codex [v0.166.0]\nOpenAI's command-line coding agent\n\nFollow these steps to sign in with ChatGPT using device code authorization:\n\n1. Open this link in your browser and sign in to your account\n   https://auth.openai.com/codex/device\n\n2. Enter this one-time code (expires in 15 minutes)\n   ABCD-12345\n\nContinue only if you started this login in Codex. If a website or another person gave you this code, cancel.\n";

    #[test]
    fn the_device_page_and_its_code_are_read_off_the_screen() {
        let said = device_said(SCREEN);
        assert_eq!(
            said.url.as_deref(),
            Some("https://auth.openai.com/codex/device")
        );
        assert_eq!(said.code.as_deref(), Some("ABCD-12345"));
        assert!(!said.refused);
        // Until the code is drawn, there is none; a page on another host is never one.
        assert_eq!(
            device_said(&SCREEN[..SCREEN.find("ABCD").unwrap()]).code,
            None
        );
        for elsewhere in [
            "openai.com.evil.example",
            "evil.example\\@auth.openai.com",
            "auth.openai.com:8443",
            "platform.openai.com",
        ] {
            let screen = SCREEN.replace("auth.openai.com", elsewhere);
            assert_eq!(device_said(&screen).url, None, "{elsewhere}");
        }
        assert!(device_said("Error: device code login is not enabled for this workspace").refused);
        // Only the device page itself: no other page there, no query.
        for other in [
            "https://auth.openai.com/codex/device?next=x",
            "https://chatgpt.com/share/abc",
            "https://auth.openai.com/other",
        ] {
            assert_eq!(
                device_said(&SCREEN.replace("https://auth.openai.com/codex/device", other)).url,
                None,
                "{other}"
            );
        }
    }

    /// A browser login's page as the 3000.12.57 binary builds it; the
    /// screen around it is pieced together from its strings (not yet seen
    /// signed in, design §9 Q1).
    const DEVIN_PAGE: &str = "https://app.devin.ai/auth/cli/continue?redirect_uri=http%3A%2F%2F127.0.0.1%3A53120%2Fcallback&state=Zx8q3V1kQ2&prompt=select_account&code_challenge=Q1w2E3r4T5y6U7i8O9p0A1s2D3f4G5h6J7k8L9z0X1c&code_challenge_method=S256";
    const DEVIN_MANUAL_PAGE: &str = "https://windsurf.com/windsurf/signin?response_type=token&client_id=3GUryQ7ldAeKEuD2obYnppsnmj58eP5u&redirect_uri=chisel-show-auth-token&state=Zx8q3V1kQ2&code_challenge=Q1w2E3r4T5y6U7i8O9p0A1s2D3f4G5h6J7k8L9z0X1c&code_challenge_method=S256&prompt=login&redirect_parameters_type=query";

    fn devin_screen(page: &str) -> String {
        format!("Opening {page} in your browser...\n\nWaiting for you to sign in (http://127.0.0.1:53120/callback)\n")
    }

    #[test]
    fn devins_login_page_is_read_off_its_screen_and_nothing_else_on_its_hosts() {
        let said = devin_said(&devin_screen(DEVIN_PAGE));
        assert_eq!(said.url.as_deref(), Some(DEVIN_PAGE));
        assert!(!said.manual && !said.already);
        let manual = format!(
            "Browser auth failed (no display), falling back to manual token flow\n{DEVIN_MANUAL_PAGE}\nPaste the token shown on the page: "
        );
        let said = devin_said(&format!("{}{manual}", devin_screen(DEVIN_PAGE)));
        assert_eq!(
            said.url.as_deref(),
            Some(DEVIN_MANUAL_PAGE),
            "the last page it drew"
        );
        assert!(said.manual);
        let hint = format!(
            "{}Press m to use the manual token flow instead, or paste a token\n",
            devin_screen(DEVIN_PAGE)
        );
        assert!(!devin_said(&hint).manual, "a hint is no fallback");
        assert!(
            devin_said(
                "You are already logged in. Use `devin auth logout` first to re-authenticate."
            )
            .already
        );
        for elsewhere in [
            "app.devin.ai.evil.example",
            "evil.example\\@app.devin.ai",
            "app.devin.ai:8443",
            "app.devin.ai.",
            "\u{430}pp.devin.ai",
            "xn--pp-8cd.devin.ai",
            "app.beta.devin.ai",
            "app.devinenterprise.com",
            "devin.ai",
            "windsurf.com.evil.example",
        ] {
            let screen = devin_screen(&DEVIN_PAGE.replace("app.devin.ai", elsewhere));
            assert_eq!(devin_said(&screen).url, None, "{elsewhere}");
        }
        for other in [
            "https://app.devin.ai/plans",
            "https://app.devin.ai/settings/usage",
            "https://app.devin.ai/settings/environment?tab=outposts",
            "http://127.0.0.1:53120/callback",
            "http://app.devin.ai/auth/cli/continue?state=x",
            "http://windsurf.com/windsurf/signin?state=x",
            "https://app.devin.ai/auth/cli/continue?state=x#y",
            "https://windsurf.com/editor/signin?state=x",
            "https://auth.openai.com/codex/device",
        ] {
            assert_eq!(devin_said(&devin_screen(other)).url, None, "{other}");
        }
        // A login of someone's at another host, by way of a user name.
        let at = DEVIN_PAGE.replace("app.devin.ai", "app.devin.ai@evil.example");
        assert_eq!(devin_said(&devin_screen(&at)).url, None);
        // The host as a browser reads it, and the page as it would open it.
        let upper = DEVIN_PAGE.replace("app.devin.ai", "APP.Devin.AI");
        assert_eq!(
            devin_said(&devin_screen(&upper)).url.as_deref(),
            Some(DEVIN_PAGE)
        );
        // Cut short, or without its own state and challenge, it is no login.
        let cut = &DEVIN_PAGE[..DEVIN_PAGE.find("&code_challenge=").unwrap()];
        assert_eq!(devin_said(&devin_screen(cut)).url, None, "{cut}");
        let no_state = DEVIN_PAGE.replace("state=", "stat=");
        assert_eq!(devin_said(&devin_screen(&no_state)).url, None);
        let empty = DEVIN_PAGE.replace("code_challenge=Q1w2", "code_challenge=&x=Q1w2");
        assert_eq!(devin_said(&devin_screen(&empty)).url, None);
        // Codex's device page is never taken for Devin's, nor Devin's for Codex's.
        assert_eq!(device_said(&devin_screen(DEVIN_PAGE)).url, None);
    }

    /// Devin 3000.12.57's own output, recorded (fixtures/devin).
    const PICKER: &[u8] = include_bytes!("../../fixtures/devin/1-picker.bin");
    const BROWSER: &[u8] = include_bytes!("../../fixtures/devin/2-browser.bin");
    const MANUAL: &[u8] = include_bytes!("../../fixtures/devin/3-manual.bin");
    /// The page [`MANUAL`] shows (its state and challenge replaced).
    const REAL_PAGE: &str = "https://app.devin.ai/auth/cli/continue?state=Zx8q3V1kQ2&prompt=select_account&code_challenge=Q1w2E3r4T5y6U7i8O9p0A1s2D3f4G5h6J7k8L9z0X1c&code_challenge_method=S256&cli_pkce_marker=1";

    /// `bytes` on a sign-in's screen, as Desk reads it.
    fn shown(bytes: &[&[u8]]) -> String {
        let mut term = crate::vt::Term::new(SIGN_IN_COLS, crate::pty::DEFAULT_ROWS, 200);
        for b in bytes {
            term.feed(b);
        }
        term.text()
    }

    #[test]
    fn devins_real_picker_is_known_and_a_changed_one_or_another_prompt_is_not() {
        let said = devin_said(&shown(&[PICKER]));
        assert!(said.picker && said.asks, "{said:?}");
        assert_eq!(
            (said.url, said.in_browser, said.failed),
            (None, false, None)
        );
        // Drawn up to its options only: not yet asked.
        let cut = &PICKER[..PICKER.windows(4).position(|w| w == b"   2").unwrap()];
        let said = devin_said(&shown(&[cut]));
        assert!(said.picker && !said.asks, "{said:?}");
        let picker = String::from_utf8(PICKER.to_vec()).unwrap();
        for changed in [
            picker.replace("1 Log in with browser", "1 Log in with Windsurf"),
            picker
                .replace(" \u{276d} 1 Log", "   1 Log")
                .replace("   2 Paste", " \u{276d} 2 Paste"),
            picker.replace(
                "How would you like to log in?",
                "How do you want to log in?",
            ),
            picker.replace("   2 Paste a token", " \u{276d} 2 Paste a token"),
        ] {
            let said = devin_said(&shown(&[changed.as_bytes()]));
            assert!(!said.picker && said.asks, "{changed}");
        }
        let enterprise = "\x1b]10;?\x07Enter your Windsurf Enterprise subdomain:\r\n \u{276d} \r\n \u{21b5} submit \u{b7} esc cancel";
        let said = devin_said(&shown(&[enterprise.as_bytes()]));
        assert!(!said.picker && said.asks);
        assert!(devin_said("Use your company's SSO instead?").asks);
        assert!(
            !devin_said(&shown(&[&PICKER[..PICKER.len() / 4]])).asks,
            "its logo alone"
        );
    }

    #[test]
    fn devin_waiting_in_the_browser_and_signing_in_or_failing_are_read() {
        let wait = &BROWSER[..BROWSER
            .windows(9)
            .position(|w| w == "Verifying".as_bytes())
            .unwrap()];
        let said = devin_said(&shown(&[PICKER, wait]));
        assert!(said.in_browser && !said.signed_in, "{said:?}");
        assert_eq!(said.url, None, "Devin opens the page itself: no address");
        let said = devin_said(&shown(&[PICKER, BROWSER]));
        assert!(said.in_browser && said.signed_in && said.failed.is_none());
        for (line, why) in [
            (
                "Error: Failed to start auth callback server: address in use",
                DEVIN_NO_CALLBACK,
            ),
            ("Failed to receive authorization code", DEVIN_NO_ANSWER),
            (
                "Token exchange failed for host https://server.codeium.com: 400",
                DEVIN_EXCHANGE_FAILED,
            ),
        ] {
            let said = devin_said(&shown(&[PICKER, wait, b"\r\n", line.as_bytes()]));
            assert_eq!(said.failed.map(|f| f.0), Some(why), "{line}");
        }
    }

    #[test]
    fn devins_page_is_read_once_through_its_hyperlink() {
        // As the screen shows it: the hyperlink's text alone.
        let screen = shown(&[MANUAL]);
        assert_eq!(screen.matches("https://").count(), 1, "{screen}");
        let said = devin_said(&screen);
        assert_eq!(said.url.as_deref(), Some(REAL_PAGE));
        assert!(said.asks && !said.picker, "its Code prompt");
        // And from its bytes as they came: OSC 8 is unwrapped, not read twice.
        let raw = String::from_utf8(MANUAL.to_vec()).unwrap();
        assert_eq!(devin_said(&raw).url.as_deref(), Some(REAL_PAGE));
        assert_eq!(
            crate::pty::plain_text(MANUAL).matches("https://").count(),
            1
        );
        // The real page carries no redirect_uri: its state and challenge are enough.
        assert!(!REAL_PAGE.contains("redirect_uri"));
        for cut in [
            "state=Zx8q3V1kQ2&",
            "code_challenge=Q1w2E3r4T5y6U7i8O9p0A1s2D3f4G5h6J7k8L9z0X1c&",
        ] {
            let page = REAL_PAGE.replace(cut, "");
            assert_eq!(
                devin_said(&raw.replace(REAL_PAGE, &page)).url,
                None,
                "{cut}"
            );
        }
        let elsewhere = raw.replace("app.devin.ai", "app.devin.ai.evil.example");
        assert_eq!(devin_said(&elsewhere).url, None);
        // Logged by its scheme, host and path alone.
        let page = EngineSignInState::Paste {
            url: REAL_PAGE.into(),
        }
        .page();
        assert_eq!(
            page.as_deref(),
            Some("https://app.devin.ai/auth/cli/continue")
        );
    }

    #[test]
    fn only_a_code_shaped_word_is_a_code() {
        assert!(device_code("ABCD-12345"));
        assert!(device_code("XY12-AB3C"));
        assert!(!device_code("abcd-12345"));
        assert!(!device_code("ABCD"));
        assert!(!device_code("ABCD-12345-6789-0000"));
        assert!(!device_code("15-minutes"));
    }

    #[test]
    fn a_sign_in_reaches_desk_in_its_wire_shape() {
        let view = EngineSignInView {
            id: Uuid::nil(),
            engine: Engine::Codex,
            state: EngineSignInState::Device {
                url: "https://auth.openai.com/codex/device".into(),
                code: "ABCD-12345".into(),
            },
        };
        let v = serde_json::to_value(&view).unwrap();
        assert_eq!(v["engine"], "codex");
        assert_eq!(v["state"], "device");
        assert_eq!(v["code"], "ABCD-12345");
        let failed = EngineSignInView {
            state: EngineSignInState::Failed { why: "x".into() },
            ..view.clone()
        };
        assert_eq!(serde_json::to_value(&failed).unwrap()["why"], "x");
        // Devin's, as agreed with Desk.
        let id = Uuid::nil();
        for (state, want) in [
            (
                EngineSignInState::Page {
                    url: "https://app.devin.ai/auth/cli/continue?state=s".into(),
                },
                serde_json::json!({"id": id, "engine": "devin", "state": "page", "url": "https://app.devin.ai/auth/cli/continue?state=s"}),
            ),
            (
                EngineSignInState::Paste {
                    url: "https://app.devin.ai/auth/cli/continue?state=s".into(),
                },
                serde_json::json!({"id": id, "engine": "devin", "state": "paste", "url": "https://app.devin.ai/auth/cli/continue?state=s"}),
            ),
            (
                EngineSignInState::Starting,
                serde_json::json!({"id": id, "engine": "devin", "state": "starting"}),
            ),
            (
                EngineSignInState::browser(),
                serde_json::json!({
                    "id": id,
                    "engine": "devin",
                    "state": "browser",
                    "say": "Finish signing in in the browser window Devin opened.",
                    "instead": "Paste a code instead",
                }),
            ),
            (
                EngineSignInState::Done,
                serde_json::json!({"id": id, "engine": "devin", "state": "done"}),
            ),
        ] {
            let view = EngineSignInView {
                id,
                engine: Engine::Devin,
                state,
            };
            assert_eq!(serde_json::to_value(&view).unwrap(), want);
        }
    }
}
