//! The person's accounts per engine: several subscription logins (a "Work"
//! and a "Personal" Claude, say), one of them the default, and which one
//! each session runs on.
//!
//! An account's sign-in is a long-lived token (`claude setup-token`). It is
//! kept in the secret store Desk chose for its own secrets ([`Keys`]): the
//! keychain, or a private file inside WSL and for development runs. Never in
//! Postgres, events, logs, the hub's vault, the session's record or any
//! process's environment. It reaches one place only: the session's own CLI,
//! on a descriptor ([`crate::token_pipe`]). A token given to Claude Code
//! that way ranks above its keychain login, as `CLAUDE_CODE_OAUTH_TOKEN` does
//! (<https://code.claude.com/docs/en/authentication#authentication-precedence>).
//! Transcripts stay in the shared `~/.claude`, so `--resume` works under
//! any account.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::engine::Engine;

/// The variable Claude Code reads a `setup-token` token from.
pub const CLAUDE_TOKEN_ENV: &str = "CLAUDE_CODE_OAUTH_TOKEN";
/// What Claude Code ranks above [`CLAUDE_TOKEN_ENV`]: taken out of a
/// session's environment when it runs on an account, so the account is the
/// one it uses.
pub const OUTRANKS_CLAUDE_TOKEN: [&str; 2] = ["ANTHROPIC_AUTH_TOKEN", "ANTHROPIC_API_KEY"];
/// The longest name an account may have.
const MAX_NAME: usize = 40;
/// The most accounts an engine may have.
pub const MAX_PER_ENGINE: usize = 8;

/// An account's sign-in. No `Serialize`, and its `Debug` says nothing of it.
#[derive(Clone, PartialEq, Eq)]
pub struct AccountToken(String);

impl std::fmt::Debug for AccountToken {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("AccountToken(…)")
    }
}

impl AccountToken {
    /// A token as pasted or read: one line of printable characters, no
    /// spaces, of a token's length.
    pub fn new(token: &str) -> Result<Self, String> {
        let t = token.trim();
        if t.len() < 20 || t.len() > 4096 {
            return Err("that isn't a sign-in token: paste the whole token".into());
        }
        if !t.chars().all(|c| c.is_ascii_graphic()) {
            return Err("that isn't a sign-in token: it has spaces or other characters".into());
        }
        Ok(Self(t.to_string()))
    }

    /// The token itself, for the one environment it goes into.
    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// Where accounts' tokens are kept: one item per account.
pub trait Keys: Send + Sync {
    fn get(&self, account: Uuid) -> Result<Option<AccountToken>>;
    fn set(&self, account: Uuid, token: &AccountToken) -> Result<()>;
    fn delete(&self, account: Uuid) -> Result<()>;
    /// Whether `account`'s item is there, without reading its secret where
    /// the store can tell.
    fn has(&self, account: Uuid) -> Result<bool> {
        self.get(account).map(|t| t.is_some())
    }
    /// What the person calls the store ("the keychain").
    fn name(&self) -> &'static str;
}

impl std::fmt::Debug for dyn Keys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Keys({})", self.name())
    }
}

/// An account's item in the store. The hub writes and reads only these,
/// never Desk Helper's or the app's, so each item has one writer.
pub fn item_name(account: Uuid) -> String {
    format!("engine-account-{}", account.simple())
}

/// Tokens in a private file (mode 0600), one key per account.
pub struct FileKeys {
    path: PathBuf,
    lock: parking_lot::Mutex<()>,
}

impl FileKeys {
    pub fn new(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            lock: parking_lot::Mutex::new(()),
        }
    }

    fn read(&self) -> Result<std::collections::BTreeMap<String, String>> {
        match std::fs::read_to_string(&self.path) {
            Ok(s) => {
                serde_json::from_str(&s).with_context(|| format!("reading {}", self.path.display()))
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Default::default()),
            Err(e) => Err(e).with_context(|| format!("reading {}", self.path.display())),
        }
    }

    fn write(&self, items: &std::collections::BTreeMap<String, String>) -> Result<()> {
        write_private(&self.path, &serde_json::to_string(items)?)
    }
}

impl Keys for FileKeys {
    fn get(&self, account: Uuid) -> Result<Option<AccountToken>> {
        let _held = self.lock.lock();
        Ok(self
            .read()?
            .remove(&item_name(account))
            .and_then(|t| AccountToken::new(&t).ok()))
    }

    fn set(&self, account: Uuid, token: &AccountToken) -> Result<()> {
        let _held = self.lock.lock();
        let mut items = self.read()?;
        items.insert(item_name(account), token.expose().to_string());
        self.write(&items)
    }

    fn delete(&self, account: Uuid) -> Result<()> {
        let _held = self.lock.lock();
        let mut items = self.read()?;
        if items.remove(&item_name(account)).is_some() {
            self.write(&items)?;
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "Desk's private file"
    }
}

/// Writes `contents` to a file only the user can read, atomically.
fn write_private(path: &Path, contents: &str) -> Result<()> {
    use std::io::Write;
    #[cfg(unix)]
    use std::os::unix::fs::OpenOptionsExt;
    let dir = path
        .parent()
        .ok_or_else(|| eyre::eyre!("{} has no folder", path.display()))?;
    std::fs::create_dir_all(dir)?;
    // Made 0600 and new from its first byte, under a name no one can guess
    // or have made first.
    let tmp = dir.join(format!(".accounts-{}.tmp", Uuid::new_v4().simple()));
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    open.mode(0o600);
    let mut f = open
        .open(&tmp)
        .with_context(|| format!("making {}", tmp.display()))?;
    let written = f
        .write_all(contents.as_bytes())
        .and_then(|()| f.sync_all())
        .and_then(|()| std::fs::rename(&tmp, path));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written.with_context(|| format!("writing {}", path.display()))
}

/// Tokens in the keychain, each its own item under Desk's service. Only the
/// hub writes these items (one writer per item, as with Desk's own bundles),
/// so its signed binary is on each one's access list; a binary that isn't
/// (an unsigned development build) would be asked, which is why development
/// runs keep them in a private file instead.
#[cfg(target_os = "macos")]
pub struct SystemKeys {
    service: String,
}

#[cfg(target_os = "macos")]
impl SystemKeys {
    pub fn new(service: impl Into<String>) -> Self {
        Self {
            service: service.into(),
        }
    }

    fn entry(&self, account: Uuid) -> Result<keyring::Entry> {
        Ok(keyring::Entry::new(&self.service, &item_name(account))?)
    }
}

#[cfg(target_os = "macos")]
impl Keys for SystemKeys {
    fn get(&self, account: Uuid) -> Result<Option<AccountToken>> {
        match self.entry(account)?.get_password() {
            Ok(t) => Ok(AccountToken::new(&t).ok()),
            Err(keyring::Error::NoEntry) => Ok(None),
            Err(e) => Err(e).context("reading an account's sign-in from the keychain"),
        }
    }

    fn set(&self, account: Uuid, token: &AccountToken) -> Result<()> {
        self.entry(account)?
            .set_password(token.expose())
            .context("saving an account's sign-in to the keychain")
    }

    fn delete(&self, account: Uuid) -> Result<()> {
        match self.entry(account)?.delete_credential() {
            Ok(()) | Err(keyring::Error::NoEntry) => Ok(()),
            Err(e) => Err(e).context("removing an account's sign-in from the keychain"),
        }
    }

    /// `security find-generic-password` without `-w` reads only the item's
    /// attributes, which the keychain gives without asking.
    fn has(&self, account: Uuid) -> Result<bool> {
        let status = std::process::Command::new("/usr/bin/security")
            .args([
                "find-generic-password",
                "-s",
                &self.service,
                "-a",
                &item_name(account),
            ])
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .context("asking the keychain for an account's sign-in")?;
        Ok(status.success())
    }

    fn name(&self) -> &'static str {
        "the keychain"
    }
}

/// Tokens held in memory: for tests, never the keychain.
#[derive(Default)]
pub struct MemoryKeys(parking_lot::Mutex<std::collections::HashMap<Uuid, AccountToken>>);

impl Keys for MemoryKeys {
    fn get(&self, account: Uuid) -> Result<Option<AccountToken>> {
        Ok(self.0.lock().get(&account).cloned())
    }
    fn set(&self, account: Uuid, token: &AccountToken) -> Result<()> {
        self.0.lock().insert(account, token.clone());
        Ok(())
    }
    fn delete(&self, account: Uuid) -> Result<()> {
        self.0.lock().remove(&account);
        Ok(())
    }
    fn name(&self) -> &'static str {
        "memory"
    }
}

/// How an engine's accounts sign in, when it has several.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum AccountsVia {
    /// A long-lived token per account, from the engine's own command
    /// (`claude setup-token`), given to each session in its environment.
    Token,
    /// The engine runs on its one sign-in on this computer.
    None,
}

/// One of the person's accounts, as kept.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    pub id: Uuid,
    pub engine: Engine,
    pub name: String,
    pub is_default: bool,
    pub created_at: DateTime<Utc>,
}

/// An account as Desk shows it: never its token.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountView {
    pub id: Uuid,
    pub engine: Engine,
    pub name: String,
    pub is_default: bool,
    /// Whether its sign-in is still in the store.
    pub signed_in: bool,
    /// The key its plan readings have on the Floor's plan cards.
    pub plan_key: Option<String>,
    /// The sessions running on it now.
    pub sessions: Vec<Uuid>,
    pub created_at: DateTime<Utc>,
}

/// What is wrong with an account's sign-in in the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SignInProblem {
    /// It is no longer there: the account is to be added again.
    Gone,
    /// It couldn't be read just now (a locked keychain, a refused prompt).
    Unreadable,
    /// The account was removed, but its sign-in couldn't be deleted.
    NotRemoved,
}

/// An account's sign-in problem, as Desk shows it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ProblemView {
    pub account: Uuid,
    pub name: String,
    pub problem: SignInProblem,
}

/// A session's account, as its view names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountRef {
    pub id: Uuid,
    pub name: String,
}

/// What the person asks for to add an account. Its `Debug` leaves out the
/// token.
#[derive(Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct NewAccount {
    pub engine: Engine,
    pub name: String,
    /// A token the person pasted: none to sign in through the engine's own
    /// command instead.
    #[serde(default)]
    pub token: Option<String>,
    #[serde(default)]
    pub make_default: bool,
    /// The person signs in from another computer than the hub's (Desk on
    /// their Mac, for a machine's hub): the page that comes back to the
    /// hub's computer by itself is caught rather than opened here, and
    /// given to Desk in [`SignInView::direct`].
    #[serde(default)]
    pub remote: bool,
}

impl std::fmt::Debug for NewAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NewAccount")
            .field("engine", &self.engine)
            .field("name", &self.name)
            .field("token", &self.token.as_ref().map(|_| "…"))
            .field("make_default", &self.make_default)
            .field("remote", &self.remote)
            .finish()
    }
}

/// A change to an account.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountPatch {
    pub name: Option<String>,
    /// `true` makes it the default; `false` makes none the default (sessions
    /// then start on the CLI's own sign-in).
    pub is_default: Option<bool>,
}

/// An account's name, trimmed, or why it can't be one.
pub fn check_name(name: &str, taken: impl IntoIterator<Item = String>) -> Result<String, String> {
    let name = name.trim();
    if name.is_empty() {
        return Err("an account needs a name, like Work or Personal".into());
    }
    if name.chars().count() > MAX_NAME {
        return Err(format!(
            "an account's name is at most {MAX_NAME} characters"
        ));
    }
    if name.chars().any(char::is_control) {
        return Err("an account's name is one line of text".into());
    }
    if taken.into_iter().any(|t| t.eq_ignore_ascii_case(name)) {
        return Err(format!("there is already an account called {name}"));
    }
    Ok(name.to_string())
}

/// The account a new session of `engine` runs on when the person names
/// none: the default, else the CLI's own sign-in (`None`).
pub fn default_of(accounts: &[Account], engine: Engine) -> Option<Uuid> {
    accounts
        .iter()
        .find(|a| a.engine == engine && a.is_default)
        .map(|a| a.id)
}

/// What a session's switch to another account does, by what it is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub enum SwitchWhen {
    /// It isn't running: its next start uses the new account.
    NextStart,
    /// It is idle or done: restarted on the new account now.
    Now,
    /// It is working: restarted once its turn ends.
    AfterTurn,
    /// Its last turn stopped on the old account's limit: restarted now, and
    /// the turn is offered again as "Continue".
    NowWithContinue,
}

/// When a session switches account, from its state and how its last turn
/// ended.
pub fn switch_when(state: crate::model::SessionState, live: bool, hit_limit: bool) -> SwitchWhen {
    use crate::model::SessionState as S;
    if !live {
        return SwitchWhen::NextStart;
    }
    match state {
        S::Working | S::Blocked | S::Starting => SwitchWhen::AfterTurn,
        S::Done | S::Idle | S::Asking | S::Interrupted if hit_limit => SwitchWhen::NowWithContinue,
        S::Done | S::Idle | S::Asking | S::Interrupted => SwitchWhen::Now,
        S::Stopped | S::Closed => SwitchWhen::NextStart,
    }
}

/// The prompt a session stopped on its limit is offered to carry on with.
pub const CONTINUE: &str = "Continue";

/// What `claude auth status --json` says when a token works: signed in, by
/// the token. Anything else (another credential outranking it, the token
/// refused) is said in plain words.
pub fn claude_token_works(status: &str) -> Result<(), String> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase")]
    struct Status {
        #[serde(default)]
        logged_in: bool,
        auth_method: Option<String>,
    }
    let s: Status = serde_json::from_str(status.trim())
        .map_err(|_| "Claude Code didn't say whether the token works".to_string())?;
    match (s.logged_in, s.auth_method.as_deref()) {
        (true, Some("oauth_token")) => Ok(()),
        (true, Some(_)) => Err(
            "Claude Code signs in with something that comes before an account's token here \
             (an API key or apiKeyHelper in your settings): remove it to use accounts"
                .into(),
        ),
        _ => Err("Claude Code didn't accept that token".into()),
    }
}

/// What a `claude setup-token` run shows, read from its screen.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SetupSaid {
    /// The sign-in page it opens in the browser: only ever a claude.ai or
    /// anthropic.com page.
    pub url: Option<String>,
    /// It waits for the code the browser shows, when it didn't redirect.
    pub asks_code: bool,
    /// What may be the token it printed, from the lines after it says it is
    /// the token: each is checked before it is kept, never trusted on its
    /// shape.
    pub candidates: Vec<String>,
    /// It said it failed. What the person reads is a fixed sentence
    /// ([`SETUP_FAILED`]), never the screen's text.
    pub failed: bool,
}

/// What the person reads when `claude setup-token` says it failed.
pub const SETUP_FAILED: &str =
    "Claude Code's sign-in didn't finish: start it again, or paste a token instead";
/// What they read when no token could be read before it ended.
pub const SETUP_UNREAD: &str = "Desk couldn't read the token from Claude Code: paste it instead";

/// The prefix Claude Code's long-lived tokens have had. Only narrows where
/// to look; [`claude_token_works`] decides.
const TOKEN_PREFIX: &str = "sk-ant-o";

/// A sign-in page Desk opens: on exactly this host, at exactly one of these
/// paths, with a query only when `query` says so.
pub struct SignInPage {
    pub host: &'static str,
    pub paths: &'static [&'static str],
    pub query: bool,
}

/// The paths Claude Code's sign-in page is at (`claude setup-token` 2.1.318
/// shows `https://claude.com/cai/oauth/authorize?…`; earlier ones
/// `https://claude.ai/oauth/authorize?…`).
const CLAUDE_PATHS: &[&str] = &["/oauth/authorize", "/cai/oauth/authorize"];

/// Claude's sign-in pages: nothing else on its domains (a page someone
/// published there) is ever taken for one.
pub const CLAUDE_PAGES: &[SignInPage] = &[
    SignInPage {
        host: "claude.ai",
        paths: CLAUDE_PATHS,
        query: true,
    },
    SignInPage {
        host: "claude.com",
        paths: CLAUDE_PATHS,
        query: true,
    },
    SignInPage {
        host: "platform.claude.com",
        paths: CLAUDE_PATHS,
        query: true,
    },
];

/// `said` as one of `pages`, read as a browser reads it (so `\\`, `@` and
/// escapes can't move its host or path): https, no user, password, port or
/// fragment, on exactly its host and path. None for anything else.
pub fn page_on(said: &str, pages: &[SignInPage]) -> Option<url::Url> {
    if said.len() > 4096 || said.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return None;
    }
    let page = url::Url::parse(said).ok()?;
    if page.scheme() != "https"
        || !page.username().is_empty()
        || page.password().is_some()
        || page.port().is_some()
        || page.fragment().is_some()
    {
        return None;
    }
    let Some(url::Host::Domain(host)) = page.host() else {
        return None;
    };
    let host = host.to_ascii_lowercase();
    pages
        .iter()
        .any(|p| {
            host == p.host && p.paths.contains(&page.path()) && (p.query || page.query().is_none())
        })
        .then_some(page)
}

/// Whether `url` is Claude's own sign-in page.
fn sign_in_host(url: &str) -> bool {
    page_on(url, CLAUDE_PAGES).is_some()
}

/// Reads a `claude setup-token` screen (run wide enough that nothing
/// wraps).
pub fn setup_said(screen: &str) -> SetupSaid {
    let mut said = SetupSaid::default();
    let mut after_token_line = false;
    for line in screen.lines() {
        let lower = line.to_lowercase();
        for word in line.split(|c: char| c.is_whitespace() || c == '│' || c == '"' || c == '\'') {
            let word = word.trim_matches(|c: char| matches!(c, '(' | ')' | '<' | '>' | ',' | '.'));
            if said.url.is_none() && sign_in_host(word) {
                said.url = Some(word.to_string());
            }
            if !after_token_line {
                continue;
            }
            if let Some(at) = word.find(TOKEN_PREFIX) {
                let token: String = word[at..]
                    .chars()
                    .take_while(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
                    .collect();
                if token.len() >= 40 && !said.candidates.contains(&token) {
                    said.candidates.push(token);
                }
            }
        }
        if lower.contains("oauth token") {
            after_token_line = true;
        }
        if lower.contains("paste code") {
            said.asks_code = true;
        }
        if (lower.contains("error") || lower.contains("failed")) && !lower.contains("if prompted") {
            said.failed = true;
        }
    }
    said
}

/// One side of a switch: an account, or (`id: None`) the CLI's own sign-in
/// on this computer.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct Side {
    pub id: Option<Uuid>,
    pub name: String,
}

/// What the person calls the CLI's own sign-in.
pub const OWN_SIGN_IN: &str = "this computer's sign-in";

impl Side {
    pub fn of(accounts: &[Account], id: Option<Uuid>) -> Self {
        Self {
            id,
            name: id
                .and_then(|id| accounts.iter().find(|a| a.id == id))
                .map_or_else(|| OWN_SIGN_IN.to_string(), |a| a.name.clone()),
        }
    }
}

/// What Desk last read of one account's plan: its windows, and when one of
/// its sessions last stopped on its limit.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Standing {
    pub windows: Vec<crate::usage::LimitWindow>,
    pub read_at: Option<DateTime<Utc>>,
    pub hit_at: Option<DateTime<Utc>>,
}

/// How long a stop on the limit counts when no window says when it resets.
const HIT_FOR: chrono::Duration = chrono::Duration::hours(5);

impl Standing {
    /// Whether the account is at its limit `now`, and when that ends (`None`
    /// inside: not said).
    pub fn full(&self, now: DateTime<Utc>) -> Option<Option<DateTime<Utc>>> {
        let full_window = self
            .windows
            .iter()
            .filter(|w| w.used_percent >= 100.0 && w.resets_at.is_none_or(|r| r > now))
            .max_by_key(|w| w.resets_at);
        if let Some(w) = full_window {
            return Some(w.resets_at);
        }
        let hit = self.hit_at.filter(|at| now - *at < HIT_FOR)?;
        // A reading after the stop that shows room: it has reset.
        if self.read_at.is_some_and(|r| r > hit) && !self.windows.is_empty() {
            return None;
        }
        Some(
            self.windows
                .iter()
                .filter_map(|w| w.resets_at)
                .filter(|r| *r > now)
                .min(),
        )
    }
}

/// A Floor need about the person's accounts.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum AccountNeed {
    /// `from` hit its limit while sessions run on it, and `to` has room:
    /// "Work hit its weekly limit — switch 7 sessions to Personal?"
    #[serde(rename_all = "camelCase")]
    Limit {
        id: String,
        engine: Engine,
        from: Side,
        to: Side,
        sessions: Vec<Uuid>,
        resets_at: Option<DateTime<Utc>>,
    },
    /// Every account is at its limit: when each resets. Nothing switches.
    #[serde(rename_all = "camelCase")]
    AllFull {
        id: String,
        engine: Engine,
        resets: Vec<(Side, Option<DateTime<Utc>>)>,
    },
    /// An account's sign-in can't be used: gone, unreadable, or left
    /// behind when the account was removed. Nothing switches to it.
    #[serde(rename_all = "camelCase")]
    SignIn {
        id: String,
        engine: Engine,
        account: Side,
        problem: SignInProblem,
    },
    /// `account`, which sessions were moved off, has room again: "Work has
    /// reset — switch back?"
    #[serde(rename_all = "camelCase")]
    Reset {
        id: String,
        engine: Engine,
        account: Side,
        from: Side,
        sessions: Vec<Uuid>,
    },
}

impl AccountNeed {
    pub fn id(&self) -> &str {
        match self {
            AccountNeed::Limit { id, .. }
            | AccountNeed::AllFull { id, .. }
            | AccountNeed::SignIn { id, .. }
            | AccountNeed::Reset { id, .. } => id,
        }
    }
}

/// Sessions were moved off `from` onto `to` because `from` was full.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MovedOff {
    pub engine: Engine,
    pub from: Option<Uuid>,
    pub to: Option<Uuid>,
}

fn side_key(id: Option<Uuid>) -> String {
    id.map_or_else(|| "own".to_string(), |id| id.simple().to_string())
}

/// The account needs, from what Desk read of each account's plan.
/// `on` is the running sessions on each side; `own_signed_in` whether the
/// CLI's own sign-in is a plan this computer can switch to.
pub fn needs(
    engine: Engine,
    accounts: &[Account],
    standing: &std::collections::HashMap<Option<Uuid>, Standing>,
    on: &std::collections::HashMap<Option<Uuid>, Vec<Uuid>>,
    moved_off: &[MovedOff],
    own_signed_in: bool,
    usable: &std::collections::HashSet<Uuid>,
    now: DateTime<Utc>,
) -> Vec<AccountNeed> {
    // Only an account whose sign-in reads can be moved to.
    let takes = |id: &Option<Uuid>| id.is_none_or(|id| usable.contains(&id));
    let mut sides: Vec<Option<Uuid>> = accounts
        .iter()
        .filter(|a| a.engine == engine)
        .map(|a| Some(a.id))
        .collect();
    // The default first, as a switch picks it.
    sides.sort_by_key(|id| !accounts.iter().any(|a| Some(a.id) == *id && a.is_default));
    if own_signed_in {
        sides.push(None);
    }
    if sides.len() < 2 {
        return Vec::new();
    }
    let full = |id: &Option<Uuid>| standing.get(id).and_then(|s| s.full(now));
    let mut out = Vec::new();
    for from in &sides {
        let Some(resets_at) = full(from) else {
            continue;
        };
        let sessions = on.get(from).cloned().unwrap_or_default();
        if sessions.is_empty() {
            continue;
        }
        match sides
            .iter()
            .find(|to| *to != from && full(to).is_none() && takes(to))
        {
            Some(to) => out.push(AccountNeed::Limit {
                id: format!("limit:{}:{}", side_key(*from), side_key(*to)),
                engine,
                from: Side::of(accounts, *from),
                to: Side::of(accounts, *to),
                sessions,
                resets_at,
            }),
            None => {
                if !out.iter().any(|n| matches!(n, AccountNeed::AllFull { .. })) {
                    out.push(AccountNeed::AllFull {
                        id: "all-full".into(),
                        engine,
                        resets: sides
                            .iter()
                            .map(|s| (Side::of(accounts, *s), full(s).flatten()))
                            .collect(),
                    });
                }
            }
        }
    }
    for m in moved_off.iter().filter(|m| m.engine == engine) {
        let back = full(&m.from).is_none() && sides.contains(&m.from) && takes(&m.from);
        let sessions = on.get(&m.to).cloned().unwrap_or_default();
        if back && !sessions.is_empty() {
            out.push(AccountNeed::Reset {
                id: format!("reset:{}:{}", side_key(m.from), side_key(m.to)),
                engine,
                account: Side::of(accounts, m.from),
                from: Side::of(accounts, m.to),
                sessions,
            });
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_claude_page_coming_back_to_a_localhost_callback_is_direct() {
        let page = |redirect: &str| {
            format!("https://claude.com/cai/oauth/authorize?code=true&client_id=c&response_type=code&redirect_uri={redirect}&scope=user%3Ainference&state=s")
        };
        let direct = direct_sign_in(&page("http%3A%2F%2Flocalhost%3A34503%2Fcallback")).unwrap();
        assert_eq!(direct.port, 34503);
        assert!(direct
            .url
            .starts_with("https://claude.com/cai/oauth/authorize?"));
        assert_eq!(
            direct_sign_in(&format!(
                "{}\n",
                page("http%3A%2F%2Flocalhost%3A34503%2Fcallback")
            ))
            .map(|d| d.port),
            Some(34503)
        );
        // The page with a code, another host, port or path, or a second
        // redirect, isn't one.
        assert_eq!(
            direct_sign_in(&page(
                "https%3A%2F%2Fplatform.claude.com%2Foauth%2Fcode%2Fcallback"
            )),
            None
        );
        assert_eq!(
            direct_sign_in(&page("http%3A%2F%2Fevil.example%3A34503%2Fcallback")),
            None
        );
        assert_eq!(
            direct_sign_in(&page("http%3A%2F%2Flocalhost%3A80%2Fcallback")),
            None
        );
        assert_eq!(
            direct_sign_in(&page("http%3A%2F%2Flocalhost%3A99999%2Fcallback")),
            None
        );
        assert_eq!(
            direct_sign_in(&page("http%3A%2F%2Flocalhost%3A34503%2Fother")),
            None
        );
        assert_eq!(
            direct_sign_in(&page("http%3A%2F%2Flocalhost%3A%2B34503%2Fcallback")),
            None
        );
        assert_eq!(
            direct_sign_in(&format!(
                "{}&redirect_uri=http%3A%2F%2Flocalhost%3A1%2Fcallback",
                page("http%3A%2F%2Flocalhost%3A34503%2Fcallback")
            )),
            None
        );
        assert_eq!(direct_sign_in("https://evil.example/oauth/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A34503%2Fcallback"), None);
        assert_eq!(direct_sign_in("http://claude.ai/oauth/authorize?redirect_uri=http%3A%2F%2Flocalhost%3A34503%2Fcallback"), None);
        assert_eq!(
            direct_sign_in(
                "https://claude.ai/a b?redirect_uri=http%3A%2F%2Flocalhost%3A34503%2Fcallback"
            ),
            None
        );
        // Read as a browser reads it: a `\` or `@` doesn't keep Claude's host.
        for bad in [
            "https://evil.example\\@claude.ai/?redirect_uri=http%3A%2F%2Flocalhost%3A34503%2Fcallback",
            "https://evil.example\\.claude.ai/?redirect_uri=http%3A%2F%2Flocalhost%3A34503%2Fcallback",
            "https://claude.ai:8443/?redirect_uri=http%3A%2F%2Flocalhost%3A34503%2Fcallback",
            "https://claude.ai/?redirect_uri=http%3A%2F%2Flocalhost%3A34503%2Fcallback%3Fx%3D1",
        ] {
            assert_eq!(direct_sign_in(bad), None, "{bad}");
        }
        assert!(!sign_in_host("https://evil.example\\@claude.ai/"));
        assert!(sign_in_host(
            "https://platform.claude.com/oauth/authorize?code=true"
        ));
        // A page someone published on Claude's own domains is never one.
        assert_eq!(
            direct_sign_in("https://claude.ai/public/artifacts/x?redirect_uri=http%3A%2F%2Flocalhost%3A34503%2Fcallback"),
            None
        );
        assert!(!sign_in_host(
            "https://platform.claude.com/oauth/code/callback"
        ));
    }
    use crate::model::SessionState as S;

    fn token() -> String {
        format!("{TOKEN_PREFIX}at01-{}", "Ab3_-".repeat(20))
    }

    #[test]
    fn a_token_is_never_shown_by_debug() {
        let t = AccountToken::new(&token()).unwrap();
        assert_eq!(format!("{t:?}"), "AccountToken(…)");
        assert!(!format!("{:?}", Some(&t)).contains("Ab3"));
        assert!(AccountToken::new("short").is_err());
        assert!(AccountToken::new("has a space in it but is long enough").is_err());
        assert_eq!(
            AccountToken::new(&format!("  {}\n", token()))
                .unwrap()
                .expose(),
            token()
        );
    }

    #[test]
    fn the_file_keeps_each_accounts_token_privately() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("accounts").join("keys.json");
        let keys = FileKeys::new(&path);
        let (a, b) = (Uuid::new_v4(), Uuid::new_v4());
        assert_eq!(keys.get(a).unwrap(), None);
        let t = AccountToken::new(&token()).unwrap();
        keys.set(a, &t).unwrap();
        keys.set(b, &AccountToken::new(&format!("{}x", token())).unwrap())
            .unwrap();
        assert_eq!(keys.get(a).unwrap(), Some(t));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600);
        }
        keys.delete(a).unwrap();
        assert_eq!(keys.get(a).unwrap(), None);
        assert!(keys.get(b).unwrap().is_some());
        keys.delete(a).unwrap();
    }

    #[test]
    fn names_are_one_short_line_and_each_once() {
        assert_eq!(check_name("  Work ", []).unwrap(), "Work");
        assert!(check_name(" ", []).is_err());
        assert!(check_name(&"x".repeat(41), []).is_err());
        assert!(check_name("a\nb", []).is_err());
        assert!(check_name("work", ["Work".to_string()]).is_err());
    }

    #[test]
    fn a_new_session_runs_on_the_default_or_the_clis_own_sign_in() {
        let account = |engine, is_default| Account {
            id: Uuid::new_v4(),
            engine,
            name: "x".into(),
            is_default,
            created_at: Utc::now(),
        };
        let work = account(Engine::Claude, true);
        let other = account(Engine::Claude, false);
        let codex = account(Engine::Codex, true);
        let all = vec![other.clone(), work.clone(), codex.clone()];
        assert_eq!(default_of(&all, Engine::Claude), Some(work.id));
        assert_eq!(default_of(&all, Engine::Codex), Some(codex.id));
        assert_eq!(default_of(&[other], Engine::Claude), None);
    }

    #[test]
    fn a_switch_waits_for_a_working_turn_and_continues_one_stopped_on_its_limit() {
        assert_eq!(switch_when(S::Idle, true, false), SwitchWhen::Now);
        assert_eq!(switch_when(S::Done, true, false), SwitchWhen::Now);
        assert_eq!(switch_when(S::Working, true, false), SwitchWhen::AfterTurn);
        assert_eq!(switch_when(S::Blocked, true, true), SwitchWhen::AfterTurn);
        assert_eq!(
            switch_when(S::Done, true, true),
            SwitchWhen::NowWithContinue
        );
        assert_eq!(switch_when(S::Idle, false, true), SwitchWhen::NextStart);
        assert_eq!(switch_when(S::Stopped, true, false), SwitchWhen::NextStart);
    }

    #[test]
    fn a_token_works_only_when_claude_code_signs_in_by_it() {
        assert!(claude_token_works(r#"{"loggedIn":true,"authMethod":"oauth_token"}"#).is_ok());
        let key = claude_token_works(r#"{"loggedIn":true,"authMethod":"api_key"}"#).unwrap_err();
        assert!(
            key.contains("apiKeyHelper") && !key.contains("api_key"),
            "{key}"
        );
        assert!(claude_token_works(r#"{"loggedIn":false}"#).is_err());
        assert!(claude_token_works("Not json").is_err());
    }

    #[test]
    fn a_setup_screen_gives_its_page_its_code_prompt_and_candidates() {
        let t = token();
        let screen = format!(
            "Opening browser to sign in…\n\
             Browser didn't open? Use the url below to sign in:\n\
             https://claude.ai/oauth/authorize?code=true&client_id=abc&state=xyz\n\
             Paste code here if prompted >\n\
             ✓ Long-lived authentication token created successfully!\n\
             Your OAuth token (valid for 1 year):\n\
             {t}\n\
             Store this token securely. Use this token by setting: export CLAUDE_CODE_OAUTH_TOKEN=<token>\n"
        );
        let said = setup_said(&screen);
        assert_eq!(
            said.url.as_deref(),
            Some("https://claude.ai/oauth/authorize?code=true&client_id=abc&state=xyz")
        );
        assert!(said.asks_code);
        assert_eq!(said.candidates, vec![t.clone()]);
        assert!(!said.failed);
        let failed = setup_said("OAuth error: Request failed with status code 400\n");
        assert!(failed.failed);
        assert!(failed.candidates.is_empty());
        let after = "Your OAuth token (valid for 1 year):\n";
        assert!(setup_said(&format!("{after}sk-ant-oat01-short"))
            .candidates
            .is_empty());
        // Only after the line that says it is the token.
        assert!(setup_said(&format!("{t}\n{after}")).candidates.is_empty());
        // Only a claude.ai or anthropic.com page.
        for page in [
            "https://evil.example/oauth/authorize",
            "https://claude.ai.evil.example/oauth",
            "https://user@claude.ai/oauth",
            "http://claude.ai/oauth/authorize",
        ] {
            assert_eq!(setup_said(page).url, None, "{page}");
        }
        // Only the sign-in page itself, never another page on its domains.
        for other in [
            "https://console.anthropic.com/oauth/code/callback",
            "https://claude.ai/public/artifacts/abc",
            "https://claude.com/cai/oauth/authorize/../../public/x",
            "https://platform.claude.com/oauth/code/callback",
            "https://www.claude.ai/oauth/authorize",
        ] {
            assert_eq!(setup_said(other).url, None, "{other}");
        }
        for page in [
            "https://claude.com/cai/oauth/authorize?code=true",
            "https://platform.claude.com/oauth/authorize?code=true",
        ] {
            assert_eq!(setup_said(page).url.as_deref(), Some(page));
        }
    }

    fn standing(windows: &[(f64, Option<i64>)], read_at: i64, hit_at: Option<i64>) -> Standing {
        let at = |m: i64| chrono::DateTime::from_timestamp(m * 60, 0).unwrap();
        Standing {
            windows: windows
                .iter()
                .map(|(used, reset)| crate::usage::LimitWindow {
                    used_percent: *used,
                    resets_at: reset.map(at),
                    minutes: 10_080,
                })
                .collect(),
            read_at: Some(at(read_at)),
            hit_at: hit_at.map(at),
        }
    }

    #[test]
    fn an_account_is_full_until_its_window_resets_or_a_later_reading_has_room() {
        let at = |m: i64| chrono::DateTime::from_timestamp(m * 60, 0).unwrap();
        let full = standing(&[(100.0, Some(600))], 100, None);
        assert_eq!(full.full(at(200)), Some(Some(at(600))));
        assert_eq!(full.full(at(700)), None, "its window reset");
        let hit = standing(&[(80.0, Some(600))], 100, Some(150));
        assert_eq!(
            hit.full(at(200)),
            Some(Some(at(600))),
            "stopped on its limit"
        );
        let after = standing(&[(10.0, Some(900))], 300, Some(150));
        assert_eq!(after.full(at(310)), None, "read with room since");
        assert_eq!(standing(&[(40.0, None)], 100, None).full(at(200)), None);
    }

    #[test]
    fn a_full_account_asks_to_move_its_sessions_to_one_with_room_and_back_once_it_resets() {
        use std::collections::HashMap;
        let at = |m: i64| chrono::DateTime::from_timestamp(m * 60, 0).unwrap();
        let account = |name: &str, is_default| Account {
            id: Uuid::new_v4(),
            engine: Engine::Claude,
            name: name.into(),
            is_default,
            created_at: at(0),
        };
        let (work, personal) = (account("Work", true), account("Personal", false));
        let accounts = vec![personal.clone(), work.clone()];
        let all_usable: std::collections::HashSet<Uuid> = [work.id, personal.id].into();
        let sessions: Vec<Uuid> = (0..7).map(|_| Uuid::new_v4()).collect();
        let on = HashMap::from([(Some(work.id), sessions.clone())]);
        let full = HashMap::from([(Some(work.id), standing(&[(100.0, Some(900))], 100, None))]);
        let n = needs(
            Engine::Claude,
            &accounts,
            &full,
            &on,
            &[],
            false,
            &all_usable,
            at(200),
        );
        assert_eq!(n.len(), 1);
        let AccountNeed::Limit {
            from,
            to,
            sessions: s,
            resets_at,
            ..
        } = &n[0]
        else {
            panic!("{n:?}");
        };
        assert_eq!(
            (from.name.as_str(), to.name.as_str(), s.len()),
            ("Work", "Personal", 7)
        );
        assert_eq!(*resets_at, Some(at(900)));
        // No sessions on it: nothing to ask.
        assert!(needs(
            Engine::Claude,
            &accounts,
            &full,
            &HashMap::new(),
            &[],
            false,
            &all_usable,
            at(200)
        )
        .is_empty());
        // Both full: when each resets, and nothing to switch to.
        let both = HashMap::from([
            (Some(work.id), standing(&[(100.0, Some(900))], 100, None)),
            (
                Some(personal.id),
                standing(&[(100.0, Some(1200))], 100, None),
            ),
        ]);
        let n = needs(
            Engine::Claude,
            &accounts,
            &both,
            &on,
            &[],
            false,
            &all_usable,
            at(200),
        );
        assert!(
            matches!(&n[..], [AccountNeed::AllFull { resets, .. }] if resets.len() == 2),
            "{n:?}"
        );
        // Moved to Personal, and Work has reset: asks to switch back.
        let moved = [MovedOff {
            engine: Engine::Claude,
            from: Some(work.id),
            to: Some(personal.id),
        }];
        let on_personal = HashMap::from([(Some(personal.id), sessions.clone())]);
        let n = needs(
            Engine::Claude,
            &accounts,
            &full,
            &on_personal,
            &moved,
            false,
            &all_usable,
            at(1000),
        );
        assert!(
            matches!(&n[..], [AccountNeed::Reset { account, .. }] if account.name == "Work"),
            "{n:?}"
        );
        // The CLI's own sign-in counts as a side when it is a plan.
        let own = HashMap::from([(Some(work.id), sessions)]);
        let one = vec![work.clone()];
        assert!(needs(
            Engine::Claude,
            &one,
            &full,
            &own,
            &[],
            false,
            &all_usable,
            at(200)
        )
        .is_empty());
        let n = needs(
            Engine::Claude,
            &one,
            &full,
            &own,
            &[],
            true,
            &all_usable,
            at(200),
        );
        assert!(
            matches!(&n[..], [AccountNeed::Limit { to, .. }] if to.id.is_none()),
            "{n:?}"
        );
    }

    /// Run by hand before merging, on a computer with Claude Code and a
    /// `setup-token` token: Claude Code signs in by the token, over the
    /// keychain login. The token is never printed.
    #[tokio::test]
    #[ignore = "needs a real Claude Code and token: DESK_REAL_CLAUDE=<path> DESK_REAL_CLAUDE_TOKEN=<token>"]
    async fn real_claude_code_signs_in_by_an_accounts_token() {
        let (Ok(claude), Ok(token)) = (
            std::env::var("DESK_REAL_CLAUDE"),
            std::env::var("DESK_REAL_CLAUDE_TOKEN"),
        ) else {
            return;
        };
        let out = tokio::process::Command::new(claude)
            .args(["auth", "status", "--json"])
            .env_remove("ANTHROPIC_API_KEY")
            .env_remove("ANTHROPIC_AUTH_TOKEN")
            .env(CLAUDE_TOKEN_ENV, token)
            .output()
            .await
            .unwrap();
        claude_token_works(&String::from_utf8_lossy(&out.stdout)).unwrap();
    }

    /// Run by hand: `claude setup-token` shows its sign-in page where
    /// [`setup_said`] finds it. It opens the browser; nothing is signed in
    /// to, as it is ended once the page is read.
    #[tokio::test]
    #[ignore = "needs a real Claude Code and opens the browser: DESK_REAL_CLAUDE=<path>"]
    async fn real_setup_token_shows_its_sign_in_page() {
        let Ok(claude) = std::env::var("DESK_REAL_CLAUDE") else {
            return;
        };
        let env = std::env::vars()
            .filter(|(k, _)| !k.starts_with("CLAUDE"))
            .chain([("TERM".to_string(), "xterm-256color".to_string())])
            .collect();
        let spec = crate::pty::Spawn {
            program: claude.into(),
            args: vec!["setup-token".into()],
            cwd: std::env::temp_dir(),
            env,
        };
        let (pty, _exit) = crate::pty::Pty::spawn(&spec, 1000, 40, 200).unwrap();
        let mut said = SetupSaid::default();
        for _ in 0..600 {
            said = setup_said(&pty.text());
            if said.url.is_some() {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
        }
        pty.kill();
        assert!(said.url.is_some(), "no sign-in page on:\n{}", pty.text());
    }
}

impl crate::store::DeskStore {
    /// Every account, oldest first.
    pub async fn accounts(&self) -> Result<Vec<Account>> {
        let client = self.pool().get().await?;
        let rows = client
            .query(
                "SELECT id, engine, name, is_default, created_at FROM engine_accounts \
                 ORDER BY created_at, name",
                &[],
            )
            .await?;
        rows.iter()
            .map(|row| {
                Ok(Account {
                    id: row.try_get("id")?,
                    engine: Engine::parse(row.try_get("engine")?)?,
                    name: row.try_get("name")?,
                    is_default: row.try_get("is_default")?,
                    created_at: row.try_get("created_at")?,
                })
            })
            .collect()
    }

    /// Saves `a`; as the default, the engine's other accounts stop being it.
    pub async fn put_account(&self, a: &Account) -> Result<()> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        if a.is_default {
            tx.execute(
                "UPDATE engine_accounts SET is_default = FALSE WHERE engine = $1 AND id <> $2",
                &[&a.engine.as_str(), &a.id],
            )
            .await?;
        }
        tx.execute(
            "INSERT INTO engine_accounts (id, engine, name, is_default, created_at) \
             VALUES ($1, $2, $3, $4, $5) \
             ON CONFLICT (id) DO UPDATE SET name = $3, is_default = $4",
            &[
                &a.id,
                &a.engine.as_str(),
                &a.name,
                &a.is_default,
                &a.created_at,
            ],
        )
        .await?;
        tx.commit().await.context("saving an engine account")
    }

    /// Removes an account; the sessions on it move to `to` (the default, or
    /// none: the CLI's own sign-in) for their next start.
    pub async fn delete_account(&self, id: Uuid, to: Option<Uuid>) -> Result<()> {
        let mut client = self.pool().get().await?;
        let tx = client.transaction().await?;
        tx.execute(
            "UPDATE sessions SET account = $2 WHERE account = $1",
            &[&id, &to],
        )
        .await?;
        tx.execute("DELETE FROM engine_accounts WHERE id = $1", &[&id])
            .await?;
        tx.commit().await.context("removing an engine account")
    }
}

/// Where signing in to a new account through the engine's own command is.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum SignInState {
    Starting,
    /// The sign-in page is open in the browser (or `url` is to be).
    Open {
        url: Option<String>,
    },
    /// The browser showed a code instead of coming back: it is to be typed.
    Code {
        url: Option<String>,
    },
    /// The account's token was read and is being checked.
    Checking,
    Done {
        account: Uuid,
    },
    Failed {
        why: String,
    },
}

impl SignInState {
    pub fn is_over(&self) -> bool {
        matches!(self, SignInState::Done { .. } | SignInState::Failed { .. })
    }
}

/// A sign-in as Desk shows it: never the screen it is read from.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SignInView {
    pub id: Uuid,
    pub engine: Engine,
    pub name: String,
    #[serde(flatten)]
    pub state: SignInState,
    /// For a sign-in made from another computer: the page that comes back
    /// by itself, once `claude setup-token` has made it. Never announced or
    /// listed: only `GET /api/desk/accounts/sign-ins/{id}/direct` gives it,
    /// to the Desk that forwards its port.
    #[serde(skip)]
    pub direct: Option<Direct>,
    /// Whether [`SignInView::direct`] can be asked for now.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub direct_ready: bool,
}

/// The sign-in page `claude setup-token` would open in a browser on the
/// hub's computer: approved, it comes back to `http://localhost:{port}/callback`,
/// where the CLI listens, and the sign-in finishes with no code to type.
/// From another computer, its browser reaches that port only through a
/// forward to it (Desk's, for the length of the sign-in).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Direct {
    pub url: String,
    pub port: u16,
}

/// The lowest port a sign-in's page may come back to: never a system one.
const LOWEST_CALLBACK_PORT: u16 = 1024;

/// `said` as a page that comes back by itself: one line, a sign-in page on
/// Claude's own hosts, and one `redirect_uri` that is exactly
/// `http://localhost:{port}/callback` on an unprivileged port.
pub fn direct_sign_in(said: &str) -> Option<Direct> {
    let page = page_on(said.trim(), CLAUDE_PAGES)?;
    let mut redirects = page.query_pairs().filter(|(k, _)| k == "redirect_uri");
    let (_, redirect) = redirects.next()?;
    if redirects.next().is_some() {
        return None;
    }
    let back = url::Url::parse(&redirect).ok()?;
    let port = back.port()?;
    let exact = back.scheme() == "http"
        && back.host() == Some(url::Host::Domain("localhost"))
        && back.path() == "/callback"
        && back.query().is_none()
        && back.fragment().is_none()
        && back.username().is_empty()
        && back.password().is_none()
        && redirect == format!("http://localhost:{port}/callback");
    (exact && port >= LOWEST_CALLBACK_PORT).then(|| Direct {
        url: page.to_string(),
        port,
    })
}
