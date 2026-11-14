//! Signing an engine's own CLI in on a terminal of the hub's own that no
//! one is shown, its screen read only for the page to open:
//!
//! - Codex's device sign-in (`codex login --device-auth`): the page and
//!   the one-time code to type there, which Desk shows on the person's own
//!   computer.
//! - Devin's login (`devin auth login`): the page, which Devin may open
//!   itself too; the CLI finishes by itself once the person signs in there.
//!   On a computer with no browser (`--force-manual-token-flow`) the page
//!   gives a token, which the person pastes into Desk and Desk types into
//!   the CLI. That token is kept nowhere: not in a log, an event, this
//!   sign-in's view, or the screen, which is blinded (its output dropped
//!   unread) before Desk can be given a token, and wiped besides.
//!
//! The CLI keeps the sign-in where its sessions read it. Nothing it signs
//! in with passes through Desk but a pasted token, on its way to the CLI.
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
    /// Open `url` on the person's own computer, then paste the token it
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
    fn is_over(&self) -> bool {
        matches!(self, Self::Done | Self::Failed { .. })
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
    /// A token was typed into it.
    typed: bool,
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
/// What they read when Devin turned the pasted token away.
pub const DEVIN_TOKEN_FAILED: &str =
    "Devin didn't take that token. Start the sign-in again, and paste the token the page gives you.";
/// What they read when Devin's login ended before showing its page.
pub const DEVIN_NO_PAGE: &str =
    "Devin's sign-in ended before it showed a page to sign in on. Start it again.";

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

/// Devin's login pages, as the 3000.12.57 binary builds them (not yet seen
/// signed in; design §9 Q1): its web app's
/// `https://app.devin.ai/auth/cli/continue?redirect_uri=…&state=…&code_challenge=…`,
/// and Windsurf's `https://windsurf.com/windsurf/signin?response_type=token&…`.
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
    /// It fell back to waiting for a token pasted from that page, no
    /// browser having opened.
    pub manual: bool,
    /// It was signed in already.
    pub already: bool,
}

/// Reads a `devin auth login` screen.
pub fn devin_said(screen: &str) -> DevinSaid {
    let mut said = DevinSaid::default();
    for line in screen.lines() {
        let lower = line.to_lowercase();
        for word in screen_words(line) {
            if let Some(page) = devin_page(word) {
                said.url = Some(page.as_str().to_string());
            }
        }
        // Only its falling back from the browser: a hint that a manual
        // flow exists is no reason to ask for a token.
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
    }
    said
}

/// `word` as Devin's login page, read as a browser reads it: on
/// [`DEVIN_PAGES`], carrying the login's own `state` and PKCE
/// `code_challenge` (a page cut short, or one without them, is none), and
/// sending the browser on to nowhere but this computer
/// ([`returns_here_only`]).
fn devin_page(word: &str) -> Option<url::Url> {
    let page = crate::engine_accounts::page_on(word, DEVIN_PAGES)?;
    let carries = |key: &str| page.query_pairs().any(|(k, v)| k == key && !v.is_empty());
    if !(carries("state") && carries("code_challenge")) {
        return None;
    }
    if !returns_here_only(&page) {
        // Without its query: the login's own state and challenge.
        let mut shown = page.clone();
        shown.set_query(None);
        tracing::warn!(page = %shown, "Devin's sign-in page would send the browser on to somewhere other than this computer: not opened");
        return None;
    }
    Some(page)
}

/// The schemes of the apps on this computer a sign-in page may hand its
/// result to.
const APP_SCHEMES: &[&str] = &["windsurf", "devin", "codeium"];

/// Whether every value in `page`'s query (split on `&` and `;`, as some
/// servers split it) sends the browser on to nowhere but this computer. A
/// value with a scheme, or starting with two of `/` and `\` (a host by the
/// page's own scheme), is allowed only as `http(s)://` on `127.0.0.1`,
/// `localhost` or `[::1]`, or as one of the [`APP_SCHEMES`]; every other
/// scheme (`javascript:`, `data:`, `file:`, `blob:`, …) is refused. A value
/// without one (a path, a word, Windsurf's `chisel-show-auth-token`) is
/// allowed: the real shapes are still to be seen. Desk's machines check
/// the same (`desk-machine`'s `returns_here_only`).
fn returns_here_only(page: &url::Url) -> bool {
    page.query()
        .unwrap_or("")
        .split(['&', ';'])
        .flat_map(|part| url::form_urlencoded::parse(part.as_bytes()))
        .all(|(_, v)| returns_here(&v))
}

/// One query value, read as a browser reads it.
fn returns_here(value: &str) -> bool {
    // Browsers drop leading and trailing spaces and controls, and tabs and
    // newlines anywhere, and read `\` as `/`.
    let value: String = value
        .trim_matches(|c: char| c <= ' ')
        .chars()
        .filter(|c| !matches!(c, '\t' | '\n' | '\r'))
        .collect();
    if value.starts_with(['/', '\\']) && value[1..].starts_with(['/', '\\']) {
        return false;
    }
    let scheme = value.split_once(':').map(|(s, _)| s).filter(|s| {
        s.starts_with(|c: char| c.is_ascii_alphabetic())
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || "+-.".contains(c))
    });
    if scheme.is_none() {
        return true;
    }
    let Ok(back) = url::Url::parse(&value) else {
        return false;
    };
    if APP_SCHEMES.contains(&back.scheme()) {
        return true;
    }
    matches!(back.scheme(), "http" | "https")
        && match back.host() {
            Some(url::Host::Domain(host)) => host == "localhost",
            Some(url::Host::Ipv4(ip)) => ip == std::net::Ipv4Addr::LOCALHOST,
            Some(url::Host::Ipv6(ip)) => ip == std::net::Ipv6Addr::LOCALHOST,
            None => false,
        }
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
        let (pty, exit) =
            Pty::spawn(&spec, SIGN_IN_COLS, crate::pty::DEFAULT_ROWS, 200).map_err(|e| {
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
                },
            );
            ended
        };
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

    /// Types the token the sign-in page gave into sign-in `id`, which waits
    /// for one ([`EngineSignInState::Paste`]); it says whether it took it by
    /// ending. The token is kept nowhere, and said nowhere.
    pub fn engine_sign_in_token(&self, id: Uuid, token: &str) -> Result<(), DeskError> {
        let token = token.trim();
        // Printable ASCII alone: a control character, a space or an escape
        // would be keys of their own in the CLI. 4 KiB is far past any
        // token; a terminal's line holds less still in canonical mode
        // (macOS's MAX_CANON is 1024), which the CLI's own read decides.
        if token.is_empty() || token.len() > 4096 || !token.bytes().all(|b| b.is_ascii_graphic()) {
            return Err(DeskError::Invalid(
                "paste the token the sign-in page gives you".into(),
            ));
        }
        let pty = {
            let mut st = self.0.state.lock();
            let run = st
                .engine_sign_ins
                .get_mut(&id)
                .filter(|r| matches!(r.view.state, EngineSignInState::Paste { .. }))
                .ok_or_else(|| {
                    DeskError::Conflict("that sign-in isn't waiting for a token".into())
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

    fn engine_sign_in_moved(&self, id: Uuid, state: EngineSignInState) {
        if let Some(run) = self.0.state.lock().engine_sign_ins.get_mut(&id) {
            run.view.state = state;
        }
    }

    /// Where sign-in `id` is now, from what its CLI draws. A Devin waiting
    /// for a token is blind from the moment it is, so a token it echoes
    /// never reaches its screen; the screen is wiped at every look besides.
    fn engine_sign_in_looked(&self, id: Uuid, engine: Engine, pty: &Pty) {
        let (state, manual) = {
            let st = self.0.state.lock();
            match st.engine_sign_ins.get(&id) {
                Some(r) => (r.view.state.clone(), r.manual),
                None => return,
            }
        };
        if matches!(state, EngineSignInState::Paste { .. }) {
            wipe(pty);
            return;
        }
        match engine {
            Engine::Devin => {
                let said = devin_said(&pty.text());
                if let Some(url) = said.url {
                    let next = match manual || said.manual {
                        true => {
                            // Before Desk can be given a token: whatever the
                            // terminal shows from now on is never taken in.
                            pty.blind();
                            wipe(pty);
                            EngineSignInState::Paste { url }
                        }
                        false => EngineSignInState::Page { url },
                    };
                    if next != state {
                        self.engine_sign_in_moved(id, next);
                    }
                }
            }
            _ => {
                let said = device_said(&pty.text());
                if let (Some(url), Some(code)) = (said.url, said.code) {
                    self.engine_sign_in_moved(id, EngineSignInState::Device { url, code });
                }
            }
        }
    }

    /// How sign-in `id` ended, its CLI having exited with `code`.
    async fn engine_sign_in_outcome(
        &self,
        id: Uuid,
        engine: Engine,
        pty: &Pty,
        code: Option<u32>,
    ) -> EngineSignInState {
        let (state, typed) = {
            let st = self.0.state.lock();
            st.engine_sign_ins
                .get(&id)
                .map_or((EngineSignInState::Starting, false), |r| {
                    (r.view.state.clone(), r.typed)
                })
        };
        if matches!(state, EngineSignInState::Paste { .. }) {
            // What it left on screen may be the token: never read.
            wipe(pty);
            return match (code, typed) {
                (Some(0), _) => EngineSignInState::Done,
                (_, true) => EngineSignInState::Failed {
                    why: DEVIN_TOKEN_FAILED.into(),
                },
                (_, false) => EngineSignInState::Failed {
                    why: DEVIN_FAILED.into(),
                },
            };
        }
        // Read the screen it left once more.
        tokio::time::sleep(SIGN_IN_EVERY).await;
        let screen = pty.text();
        match engine {
            Engine::Devin => {
                let said = devin_said(&screen);
                match code {
                    Some(0) => EngineSignInState::Done,
                    _ if said.already => EngineSignInState::Done,
                    _ if said.url.is_none() => EngineSignInState::Failed {
                        why: DEVIN_NO_PAGE.into(),
                    },
                    _ => EngineSignInState::Failed {
                        why: DEVIN_FAILED.into(),
                    },
                }
            }
            _ => {
                let said = device_said(&screen);
                match code {
                    Some(0) => EngineSignInState::Done,
                    _ if said.refused => EngineSignInState::Failed {
                        why: DEVICE_REFUSED.into(),
                    },
                    _ => EngineSignInState::Failed {
                        why: DEVICE_FAILED.into(),
                    },
                }
            }
        }
    }

    /// Follows the sign-in's screen until it ends, then reads the engine
    /// again, so what Desk says of its sign-in is now.
    async fn follow_engine_sign_in(
        self,
        id: Uuid,
        mut exit: tokio::sync::oneshot::Receiver<crate::pty::Exit>,
    ) {
        let started = tokio::time::Instant::now();
        let Some(engine) = self.engine_sign_in(id).ok().map(|v| v.engine) else {
            return;
        };
        let outcome = loop {
            let Some(pty) = ({
                let st = self.0.state.lock();
                st.engine_sign_ins.get(&id).and_then(|r| r.pty.clone())
            }) else {
                return; // cancelled, or a newer one began
            };
            self.engine_sign_in_looked(id, engine, &pty);
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
                break EngineSignInState::Failed {
                    why: match engine {
                        Engine::Devin => DEVIN_FAILED,
                        _ => DEVICE_FAILED,
                    }
                    .into(),
                };
            }
        };
        let done = outcome == EngineSignInState::Done;
        tracing::info!(
            engine = engine.title(),
            done,
            "an engine's own sign-in ended"
        );
        {
            let mut st = self.0.state.lock();
            let Some(run) = st.engine_sign_ins.get_mut(&id) else {
                return; // cancelled as it ended
            };
            run.view.state = outcome;
            // Its screen goes with it: nothing of it is kept.
            run.pty = None;
        }
        if done {
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

    #[test]
    fn a_devin_page_sending_the_browser_anywhere_but_here_is_never_opened() {
        let devin = |query: &str| {
            format!("https://app.devin.ai/auth/cli/continue?state=s&code_challenge=c{query}")
        };
        for here in [
            "",
            "&redirect_uri=http%3A%2F%2Flocalhost%3A4321%2Fcallback",
            "&redirect_uri=http%3A%2F%2F127.0.0.1%3A4321%2Fcallback",
            "&redirect_uri=http%3A%2F%2F%5B%3A%3A1%5D%3A4321%2Fcallback",
            "&redirect_uri=https%3A%2F%2FLOCALHOST%2Fcb",
            "&redirect_uri=chisel-show-auth-token",
            "&next=%2Fdashboard",
            "&return_to=settings",
            "&callback=",
            "&redirect_uri=windsurf%3A%2F%2Fcodeium.windsurf%2Fauth",
            "&redirect_uri=DEVIN%3A%2F%2Fauth%2Fcallback",
            "&redirect_uri=codeium%3A%2F%2Fauth",
            "&prompt=select_account&code_challenge_method=S256",
        ] {
            let page = devin(here);
            assert_eq!(
                devin_said(&devin_screen(&page)).url.as_deref(),
                Some(page.as_str()),
                "{here}"
            );
        }
        for away in [
            "&redirect_uri=https%3A%2F%2Fevil.example%2Fcb",
            "&redirect_uri=%2F%2Fevil",
            "&redirect=http%3A%2F%2Fevil.example%2F",
            "&redirect_url=https%3A%2F%2F127.0.0.1.evil.example%2F",
            "&return_to=https%3A%2F%2Flocalhost.evil.example%2F",
            "&callback=https%3A%2F%2Flocalhost%40evil.example%2F",
            "&next=javascript%3Aalert(1)",
            "&next=data%3Atext%2Fhtml%2C%3Cscript%3E",
            "&next=%5C%5Cevil",
            "&next=%2F%5Cevil",
            "&next=%20%2F%2Fevil",
            "&next=%2F%09%2Fevil",
            "&next=https%3Aevil.example",
            "&next=localhost%3A4321%2Fx",
            "&next=file%3A%2F%2F%2Fetc%2Fpasswd",
            "&next=blob%3Ahttps%3A%2F%2Flocalhost%2Fx",
            "&Redirect_URI=https%3A%2F%2Fevil.example%2Fcb",
            // Any parameter, not only those named for it.
            "&continue=https%3A%2F%2Fevil.example%2F",
            "&goto=%2F%2Fevil",
            // Split on `;` as some servers split it.
            "&x=1;next=https%3A%2F%2Fevil.example",
            "&redirect_uri=http%3A%2F%2Flocalhost%3A4321%2Fcallback&next=https%3A%2F%2Fevil.example",
        ] {
            assert_eq!(devin_said(&devin_screen(&devin(away))).url, None, "{away}");
        }
        let windsurf = "https://windsurf.com/windsurf/signin?state=s&code_challenge=c&redirect_uri=https%3A%2F%2Fevil.example%2Fcb";
        assert_eq!(devin_said(&devin_screen(windsurf)).url, None);
        // Both pages as the binary builds them still pass.
        assert!(returns_here_only(&url::Url::parse(DEVIN_PAGE).unwrap()));
        assert!(returns_here_only(
            &url::Url::parse(DEVIN_MANUAL_PAGE).unwrap()
        ));
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
