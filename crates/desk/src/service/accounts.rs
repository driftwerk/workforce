//! The person's engine accounts: adding one (a pasted token, or signing in
//! through `claude setup-token` with its screen read here and never shown),
//! the default, removing one, and moving a session to another.
//!
//! A token is read from the store only to start a session's CLI, and goes
//! only into that CLI's environment ([`Desk::account_env`]).

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use uuid::Uuid;

use super::{Desk, DeskError, SIGN_IN_WAIT};
use crate::crew::{self, By};
use crate::engine::{self, Engine};
use crate::engine_accounts::{
    self as accounts, Account, AccountPatch, AccountRef, AccountToken, AccountView, AccountsVia,
    NewAccount, SignInState, SignInView, SwitchWhen,
};
use crate::floor::Outcome;
use crate::model::{DeskEvent, SessionState, SessionView, View};
use crate::pty::{Pty, Spawn};

/// How long signing in through the engine's command may take.
const SIGN_IN_FOR: Duration = Duration::from_secs(10 * 60);
/// How often its screen is read.
const SIGN_IN_EVERY: Duration = Duration::from_millis(250);
/// Wide enough that a token is never wrapped.
const SIGN_IN_COLS: u16 = 1000;

/// The id a Desk account's plan readings are known by.
pub(super) fn plan_id(account: Uuid) -> String {
    format!("desk-account:{account}")
}

/// What adding an account did.
#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "kind", rename_all = "camelCase")]
pub enum Added {
    /// A pasted token was checked and kept.
    Account { account: AccountView },
    /// Signing in through the engine's own command began.
    SignIn {
        #[serde(rename = "signIn")]
        sign_in: SignInView,
    },
}

/// An account's sign-in as handed to Desk to copy to another computer
/// ([`Desk::hand_over_account`]): its token is written only into the
/// answer's body, and its `Debug` says nothing of it.
#[derive(Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct HandOver {
    pub engine: Engine,
    pub name: String,
    pub is_default: bool,
    #[serde(serialize_with = "exposed")]
    pub token: AccountToken,
}

impl std::fmt::Debug for HandOver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("HandOver")
            .field("engine", &self.engine)
            .field("name", &self.name)
            .field("is_default", &self.is_default)
            .finish_non_exhaustive()
    }
}

fn exposed<S: serde::Serializer>(t: &AccountToken, s: S) -> Result<S::Ok, S::Error> {
    s.serialize_str(t.expose())
}

/// A sign-in running: the engine's command, read until it gives a token.
pub(super) struct SignInRun {
    view: SignInView,
    make_default: bool,
    pty: Arc<Pty>,
    /// For a sign-in made from another computer: where the page that comes
    /// back by itself is caught.
    relay: Option<Arc<Relay>>,
}

/// What `claude setup-token` would open in a browser on this computer,
/// caught for a sign-in made from another one: `BROWSER` names a script of
/// Desk's that writes the page down, in a folder only this user can open,
/// and says it couldn't open it, so the CLI also shows its page with a
/// code (what the person falls back on when the page can't come back).
/// The folder goes with the sign-in.
pub(super) struct Relay {
    dir: std::path::PathBuf,
}

impl Relay {
    pub(super) fn new(run_dir: &std::path::Path) -> eyre::Result<Self> {
        use eyre::WrapErr;
        use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt};
        crate::token_pipe::private_run_dir(run_dir)?;
        let dir = run_dir.join(format!("s-{}", Uuid::new_v4().simple()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&dir)
            .wrap_err_with(|| format!("making {}", dir.display()))?;
        let relay = Self { dir };
        let page = shell_quoted(&relay.page().display().to_string());
        let script = format!(
            "#!/bin/sh\n\
             # Desk: the sign-in page, for Desk to open on the person's own computer.\n\
             umask 077\n\
             printf '%s\\n' \"$1\" > {page}.new && mv -f {page}.new {page}\n\
             exit 1\n"
        );
        use std::io::Write;
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o700)
            .open(relay.browser())
            .wrap_err("making the sign-in's browser")?;
        f.write_all(script.as_bytes())?;
        Ok(relay)
    }

    /// What `BROWSER` names.
    pub(super) fn browser(&self) -> std::path::PathBuf {
        self.dir.join("open")
    }

    fn page(&self) -> std::path::PathBuf {
        self.dir.join("page")
    }

    /// The page, once the CLI has made it, and only one that comes back to
    /// a localhost callback on Claude's own hosts.
    fn direct(&self) -> Option<accounts::Direct> {
        let said = std::fs::read_to_string(self.page()).ok()?;
        accounts::direct_sign_in(&said)
    }
}

impl Drop for Relay {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.dir);
    }
}

/// `word` as one word to `sh`, in single quotes.
fn shell_quoted(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

impl Desk {
    fn keys(&self) -> Result<&Arc<dyn accounts::Keys>, DeskError> {
        self.0.opts.account_keys.as_ref().ok_or_else(|| {
            DeskError::Conflict(
                "this Desk keeps no accounts yet: update Desk to add accounts".into(),
            )
        })
    }

    /// Which accounts' sign-ins are in the store, looked for once the hub
    /// is up: only whether each item is there, never its secret, and never
    /// holding up the hub's start.
    pub(super) async fn look_for_sign_ins(self) {
        let Some(keys) = self.0.opts.account_keys.clone() else {
            return;
        };
        let ids: Vec<Uuid> = self.0.state.lock().accounts.iter().map(|a| a.id).collect();
        let found = tokio::task::spawn_blocking(move || {
            ids.into_iter()
                .filter(|id| match keys.has(*id) {
                    Ok(there) => there,
                    Err(e) => {
                        tracing::warn!(account = %id, error = %format!("{e:#}"), "couldn't look for an engine account's sign-in");
                        false
                    }
                })
                .collect::<HashSet<Uuid>>()
        })
        .await
        .unwrap_or_default();
        self.0.state.lock().signed_in_accounts.extend(found);
        self.announce_accounts();
    }

    /// Every account, as Settings shows them.
    pub fn accounts(&self) -> Vec<AccountView> {
        let st = self.0.state.lock();
        let keys = self.0.opts.account_keys.as_ref();
        st.accounts
            .iter()
            .map(|a| AccountView {
                id: a.id,
                engine: a.engine,
                name: a.name.clone(),
                is_default: a.is_default,
                plan_key: self.0.plans.as_ref().and_then(|p| {
                    let ids = Self::plan_ids(&st, &super::identities::Raw::Account(a.id))
                        .unwrap_or_default();
                    p.account_key(crate::usage::LocalPlan::Claude, &ids)
                }),
                identity: st.links.get(&a.id).map(|l| accounts::IdentityView {
                    key: l.identity.key.clone(),
                    email: l.identity.email.clone(),
                    linked_by: l.by,
                    this_computer: st
                        .own_identity
                        .as_ref()
                        .is_some_and(|o| o.key == l.identity.key),
                }),
                // As last seen: the store is never read under this lock.
                signed_in: keys.is_some() && st.signed_in_accounts.contains(&a.id),
                sessions: st
                    .records
                    .values()
                    .filter(|r| r.account == Some(a.id) && st.live.contains_key(&r.id))
                    .map(|r| r.id)
                    .collect(),
                created_at: a.created_at,
            })
            .collect()
    }

    /// The key this computer's CLI sign-in's plan card has on the Floor, when
    /// its identity is known: the card that is named by its email while it
    /// is none of the accounts.
    pub fn own_plan_key(&self) -> Option<String> {
        let ids = Self::own_plan_ids(&self.0.state.lock())?;
        self.0
            .plans
            .as_ref()?
            .account_key(crate::usage::LocalPlan::Claude, &ids)
    }

    /// The keys this computer's Claude plan readings are kept under now:
    /// each account's and the CLI sign-in's. A reading kept under another
    /// (an account removed, a sign-in gone, an identity learnt since) is
    /// nobody's card.
    pub fn current_plan_keys(&self) -> Vec<String> {
        let Some(plans) = self.0.plans.as_ref() else {
            return Vec::new();
        };
        let st = self.0.state.lock();
        st.accounts
            .iter()
            .filter(|a| a.engine == Engine::Claude)
            .filter_map(|a| Self::plan_ids(&st, &super::identities::Raw::Account(a.id)))
            .chain(Self::own_plan_ids(&st))
            .filter_map(|ids| plans.account_key(crate::usage::LocalPlan::Claude, &ids))
            .collect()
    }

    /// The store the accounts' sign-ins are kept in, as the person calls it;
    /// none when this Desk keeps no accounts.
    pub fn accounts_store(&self) -> Option<&'static str> {
        self.0.opts.account_keys.as_ref().map(|k| k.name())
    }

    /// What is wrong with accounts' sign-ins, for the Floor and Settings.
    pub fn sign_in_problems(&self) -> Vec<accounts::ProblemView> {
        let st = self.0.state.lock();
        let mut problems: Vec<accounts::ProblemView> = st
            .sign_in_problems
            .iter()
            .filter_map(|(id, problem)| {
                let name = st
                    .accounts
                    .iter()
                    .find(|a| a.id == *id)
                    .map(|a| a.name.clone())
                    .or_else(|| st.removed_names.get(id).cloned())?;
                Some(accounts::ProblemView {
                    account: *id,
                    name,
                    problem: *problem,
                })
            })
            .collect();
        problems.sort_by(|a, b| a.name.cmp(&b.name));
        problems
    }

    /// Forgets a sign-in problem the person has dealt with.
    pub fn dismiss_sign_in_problem(&self, account: Uuid) {
        let removed = {
            let mut st = self.0.state.lock();
            let removed = st.sign_in_problems.remove(&account).is_some();
            if !st.accounts.iter().any(|a| a.id == account)
                && !st.records.values().any(|r| r.account == Some(account))
            {
                st.removed_names.remove(&account);
            }
            removed
        };
        if removed {
            self.announce_accounts();
        }
    }

    /// The sign-ins under way, for a window that opens while one is.
    pub fn account_sign_ins(&self) -> Vec<SignInView> {
        let st = self.0.state.lock();
        st.sign_in_runs.values().map(|r| r.view.clone()).collect()
    }

    pub(super) fn announce_accounts(&self) {
        if self.0.opts.account_keys.is_none() {
            return;
        }
        self.announce(DeskEvent::Accounts {
            accounts: self.accounts(),
            needs: self.account_needs(),
            auto_switch: self.auto_switch(),
            problems: self.sign_in_problems(),
            own_plan_key: self.own_plan_key(),
        });
    }

    /// An account by id, of an engine that has them.
    pub(super) fn account(&self, id: Uuid) -> Result<Account, DeskError> {
        self.0
            .state
            .lock()
            .accounts
            .iter()
            .find(|a| a.id == id)
            .cloned()
            .ok_or_else(|| DeskError::Invalid("there is no such account".into()))
    }

    /// Account `id`'s sign-in, handed to Desk to copy to another computer
    /// of the person's (one of their machines, over Desk's SSH connection
    /// to it), as `claude setup-token` says its token may be: set wherever
    /// the person wants to sign in. It is never refreshed, so a copy never
    /// goes stale, nor makes this one stale. Only the admin route gives it,
    /// and only Desk's native side asks it (never its page); it is never
    /// logged, announced or kept anywhere else here.
    pub async fn hand_over_account(&self, id: Uuid) -> Result<HandOver, DeskError> {
        let a = self.account(id)?;
        let keys = Arc::clone(self.keys()?);
        let token = tokio::task::spawn_blocking(move || keys.get(id))
            .await
            .map_err(|e| DeskError::Internal(e.into()))?
            .map_err(DeskError::Internal)?
            .ok_or_else(|| {
                DeskError::Conflict(format!(
                    "{}'s sign-in is no longer in {}: add the account again",
                    a.name,
                    self.accounts_store().unwrap_or("the store")
                ))
            })?;
        tracing::warn!(account = %id, engine = a.engine.as_str(), "an engine account's sign-in was handed to Desk to copy to another computer");
        Ok(HandOver {
            engine: a.engine,
            name: a.name,
            is_default: a.is_default,
            token,
        })
    }

    /// The account `wanted` names for a new session of `engine`: absent,
    /// its default; `null`, the CLI's own sign-in (which, on a computer
    /// where the CLI has none, is an account at launch:
    /// [`Desk::instead_of_own`]).
    pub(super) fn account_for(
        &self,
        engine: Engine,
        wanted: Option<Option<Uuid>>,
    ) -> Result<Option<Uuid>, DeskError> {
        let has_accounts = engine::caps(engine, View::Terminal).accounts == AccountsVia::Token;
        match wanted {
            None if !has_accounts || self.0.opts.account_keys.is_none() => Ok(None),
            None => Ok(accounts::default_of(&self.0.state.lock().accounts, engine)),
            Some(None) => Ok(None),
            Some(Some(_)) if !has_accounts => Err(DeskError::Invalid(format!(
                "{} sessions run on its own sign-in here",
                engine.title()
            ))),
            Some(Some(id)) => {
                self.keys()?;
                let a = self.account(id)?;
                if a.engine != engine {
                    return Err(DeskError::Invalid(format!(
                        "{} is a {} account",
                        a.name,
                        a.engine.title()
                    )));
                }
                Ok(Some(id))
            }
        }
    }

    /// An account by the name the person or an agent gave it.
    pub fn account_named(&self, engine: Engine, name: &str) -> Result<Uuid, DeskError> {
        let st = self.0.state.lock();
        st.accounts
            .iter()
            .find(|a| a.engine == engine && a.name.eq_ignore_ascii_case(name.trim()))
            .map(|a| a.id)
            .ok_or_else(|| {
                let names: Vec<&str> = st
                    .accounts
                    .iter()
                    .filter(|a| a.engine == engine)
                    .map(|a| a.name.as_str())
                    .collect();
                DeskError::Invalid(match names.is_empty() {
                    true => format!("there are no {} accounts", engine.title()),
                    false => format!(
                        "there is no {} account called {name}; there are {}",
                        engine.title(),
                        names.join(", ")
                    ),
                })
            })
    }

    /// The token a session's account signs its CLI in with, read from the
    /// store now; none for a session on the CLI's own sign-in. It is handed
    /// over on a descriptor ([`crate::token_pipe`]), never put in an
    /// environment.
    pub(super) async fn account_token(
        &self,
        engine: Engine,
        account: Option<Uuid>,
    ) -> Result<Option<AccountToken>, DeskError> {
        let Some(id) = account else {
            return Ok(None);
        };
        let a = self.account(id).map_err(|_| {
            DeskError::Conflict(
                "the account this session ran on was removed: choose another for it".into(),
            )
        })?;
        match engine {
            Engine::Claude => {}
            Engine::Codex
            | Engine::Gemini
            | Engine::Copilot
            | Engine::OpenCode
            | Engine::Cursor
            | Engine::Devin
            | Engine::Custom => {
                return Err(DeskError::Invalid(format!(
                    "{} sessions run on its own sign-in here",
                    engine.title()
                )))
            }
        }
        self.read_token(&a).await.map(Some)
    }

    /// What a session of `engine` on no account (`None`: the CLI's own
    /// sign-in) runs on instead when this computer's CLI has no sign-in of
    /// its own (it says it isn't logged in): the first of its accounts, in
    /// the order a switch takes them (the default first, then oldest
    /// first), whose sign-in reads, with that sign-in. A session there
    /// never starts at the CLI's login prompt. `None` when the CLI may be
    /// signed in here, or there are no accounts to start on (as before
    /// accounts, the CLI's own login).
    pub(super) async fn instead_of_own(
        &self,
        engine: Engine,
    ) -> Result<Option<(Uuid, AccountToken)>, DeskError> {
        if engine::caps(engine, View::Terminal).accounts != AccountsVia::Token
            || self.0.opts.account_keys.is_none()
        {
            return Ok(None);
        }
        let mut ordered: Vec<Account> = {
            let st = self.0.state.lock();
            st.accounts
                .iter()
                .filter(|a| a.engine == engine)
                .cloned()
                .collect()
        };
        if ordered.is_empty() || self.own_sign_in_works(engine).await {
            return Ok(None);
        }
        ordered.sort_by_key(|a| !a.is_default);
        for a in &ordered {
            if let Ok(token) = self.read_token(a).await {
                tracing::info!(account = %a.id, engine = engine.as_str(), "a session runs on an account: the CLI has no sign-in of its own on this computer");
                return Ok(Some((a.id, token)));
            }
        }
        Err(DeskError::Conflict(format!(
            "{} isn't signed in on this computer, and none of its accounts here can sign in \
             just now: add one again in Settings → Engines",
            engine.title()
        )))
    }

    /// Whether `engine`'s CLI may be signed in on this computer by itself:
    /// false only once it says it isn't. A "logged out" read before is read
    /// again (a login since is honoured at once), as is one never read; one
    /// that can't be read is left to the CLI, as before accounts, and isn't
    /// asked again for each session.
    async fn own_sign_in_works(&self, engine: Engine) -> bool {
        match self.engine_read(engine) {
            Some(engine::SignIn::In { .. } | engine::SignIn::Unknown) => return true,
            Some(engine::SignIn::Out) | None => {}
        }
        self.read_engine_since(engine, self.read_asked(), super::Since::Ended)
            .await;
        self.spend_signed_in();
        self.engine_read(engine) != Some(engine::SignIn::Out)
    }

    /// `a`'s token from the store, off the runtime's threads and never under
    /// the state lock (a locked keychain or a prompt may block). A sign-in
    /// that can't be read is a Floor need until it can.
    pub(super) async fn read_token(&self, a: &Account) -> Result<AccountToken, DeskError> {
        let keys = Arc::clone(self.keys()?);
        let store = keys.name();
        let id = a.id;
        let read = tokio::task::spawn_blocking(move || keys.get(id))
            .await
            .map_err(|e| DeskError::Internal(e.into()));
        let problem = match read {
            Ok(Ok(Some(token))) => {
                let changed = {
                    let mut st = self.0.state.lock();
                    st.sign_in_problems.remove(&id).is_some() | st.signed_in_accounts.insert(id)
                };
                if changed {
                    self.announce_accounts();
                }
                return Ok(token);
            }
            Ok(Ok(None)) => accounts::SignInProblem::Gone,
            Ok(Err(e)) | Err(DeskError::Internal(e)) => {
                tracing::warn!(account = %id, error = %format!("{e:#}"), "couldn't read an engine account's sign-in");
                accounts::SignInProblem::Unreadable
            }
            Err(e) => return Err(e),
        };
        {
            let mut st = self.0.state.lock();
            st.sign_in_problems.insert(id, problem);
            if problem == accounts::SignInProblem::Gone {
                st.signed_in_accounts.remove(&id);
            }
        }
        self.announce_accounts();
        Err(DeskError::Conflict(match problem {
            accounts::SignInProblem::Gone => format!(
                "{}'s sign-in is no longer in {store}: add the account again in Settings → Engines",
                a.name
            ),
            accounts::SignInProblem::Unreadable | accounts::SignInProblem::NotRemoved => format!(
                "Desk couldn't read {}'s sign-in from {store} just now: nothing was switched",
                a.name
            ),
        }))
    }

    /// Whether Claude Code, run as a session on `token` is (the session's
    /// environment, the token on descriptor 3), signs in by it.
    async fn check_claude_token(&self, token: &AccountToken) -> Result<(), String> {
        use tokio::io::AsyncWriteExt;
        let program = self
            .program_of(Engine::Claude)
            .map_err(|e| e.to_string())?
            .to_path_buf();
        let mut env = self.with_added(&self.0.state.lock().session_env.clone());
        for k in accounts::OUTRANKS_CLAUDE_TOKEN {
            env.remove(k);
        }
        env.remove(accounts::CLAUDE_TOKEN_ENV);
        env.insert(
            crate::token_pipe::FD_ENV.into(),
            crate::token_pipe::FD.into(),
        );
        let (sh, args) = crate::token_pipe::one_off(&program, &["auth", "status", "--json"]);
        let run = async {
            let mut child = tokio::process::Command::new(sh)
                .args(args)
                .env_clear()
                .envs(env)
                .stdin(std::process::Stdio::piped())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::null())
                .kill_on_drop(true)
                .spawn()
                .map_err(|_| "Claude Code couldn't be run".to_string())?;
            if let Some(mut stdin) = child.stdin.take() {
                let mut line = token.expose().as_bytes().to_vec();
                line.push(b'\n');
                let _ = stdin.write_all(&line).await;
            }
            child
                .wait_with_output()
                .await
                .map_err(|_| "Claude Code couldn't be run".to_string())
        };
        let out = tokio::time::timeout(SIGN_IN_WAIT, run)
            .await
            .map_err(|_| "Claude Code didn't answer in time".to_string())??;
        accounts::claude_token_works(&String::from_utf8_lossy(&out.stdout))
    }

    /// Adds an account: a pasted token is checked and kept; without one,
    /// signing in through the engine's own command begins.
    pub async fn add_account(&self, new: NewAccount) -> Result<Added, DeskError> {
        let keys = Arc::clone(self.keys()?);
        if engine::caps(new.engine, View::Terminal).accounts != AccountsVia::Token {
            return Err(DeskError::Invalid(format!(
                "Desk can't keep accounts for {} yet",
                new.engine.title()
            )));
        }
        let name = {
            let st = self.0.state.lock();
            let of_engine: Vec<String> = st
                .accounts
                .iter()
                .filter(|a| a.engine == new.engine)
                .map(|a| a.name.clone())
                .chain(
                    st.sign_in_runs
                        .values()
                        .filter(|r| !r.view.state.is_over())
                        .map(|r| r.view.name.clone()),
                )
                .collect();
            if of_engine.len() >= accounts::MAX_PER_ENGINE {
                return Err(DeskError::Invalid(format!(
                    "{} has {} accounts, the most Desk keeps",
                    new.engine.title(),
                    accounts::MAX_PER_ENGINE
                )));
            }
            accounts::check_name(&new.name, of_engine).map_err(DeskError::Invalid)?
        };
        match new
            .token
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
        {
            Some(pasted) => {
                let token = AccountToken::new(pasted).map_err(DeskError::Invalid)?;
                self.check_claude_token(&token)
                    .await
                    .map_err(DeskError::Invalid)?;
                let account = self
                    .keep_account(keys, new.engine, name, new.make_default, &token)
                    .await?;
                let view = self
                    .accounts()
                    .into_iter()
                    .find(|a| a.id == account.id)
                    .ok_or_else(|| DeskError::Internal(eyre::eyre!("account not kept")))?;
                Ok(Added::Account { account: view })
            }
            None => self
                .start_sign_in(new.engine, name, new.make_default, new.remote)
                .map(|sign_in| Added::SignIn { sign_in }),
        }
    }

    /// Keeps a checked token and its account, the token first: an account
    /// is never listed without its sign-in.
    async fn keep_account(
        &self,
        keys: Arc<dyn accounts::Keys>,
        engine: Engine,
        name: String,
        make_default: bool,
        token: &AccountToken,
    ) -> Result<Account, DeskError> {
        let account = Account {
            id: Uuid::new_v4(),
            engine,
            name,
            is_default: make_default,
            created_at: Utc::now(),
        };
        let (id, kept) = (account.id, token.clone());
        let store = Arc::clone(&keys);
        tokio::task::spawn_blocking(move || store.set(id, &kept))
            .await
            .map_err(|e| DeskError::Internal(e.into()))?
            .map_err(DeskError::Internal)?;
        if let Err(e) = self.0.store.put_account(&account).await {
            let _ = tokio::task::spawn_blocking(move || keys.delete(id)).await;
            return Err(DeskError::Internal(e));
        }
        {
            let mut st = self.0.state.lock();
            if account.is_default {
                st.accounts
                    .iter_mut()
                    .filter(|a| a.engine == engine)
                    .for_each(|a| a.is_default = false);
            }
            st.accounts.push(account.clone());
            st.signed_in_accounts.insert(account.id);
        }
        self.announce_accounts();
        tracing::info!(account = %account.id, engine = engine.as_str(), "engine account added");
        Ok(account)
    }

    /// Starts `claude setup-token` on a terminal of the hub's own, which no
    /// one is shown: its screen is read here for the sign-in page, whether
    /// it asks for a code, and the token it prints.
    ///
    /// For a sign-in made from another computer (`remote`), the page the CLI
    /// would open in a browser here is caught instead ([`Relay`]), for Desk
    /// to open there.
    fn start_sign_in(
        &self,
        engine: Engine,
        name: String,
        make_default: bool,
        remote: bool,
    ) -> Result<SignInView, DeskError> {
        let program = self.program_of(engine)?.to_path_buf();
        let mut env = self.with_added(&self.0.state.lock().session_env.clone());
        for k in accounts::OUTRANKS_CLAUDE_TOKEN {
            env.remove(k);
        }
        env.remove(accounts::CLAUDE_TOKEN_ENV);
        // Without one, the sign-in goes on with its page and a code.
        let relay = match remote {
            true => match Relay::new(&self.0.opts.run_dir) {
                Ok(r) => Some(Arc::new(r)),
                Err(e) => {
                    tracing::warn!(error = %format!("{e:#}"), "couldn't catch a sign-in's page: it goes on with a code");
                    None
                }
            },
            false => None,
        };
        if let Some(r) = &relay {
            env.insert("BROWSER".into(), r.browser().display().to_string());
        }
        let spec = Spawn {
            program,
            args: vec!["setup-token".into()],
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
        let view = SignInView {
            id: Uuid::new_v4(),
            engine,
            name,
            state: SignInState::Starting,
            direct: None,
            direct_ready: false,
        };
        self.0.state.lock().sign_in_runs.insert(
            view.id,
            SignInRun {
                view: view.clone(),
                make_default,
                pty: Arc::new(pty),
                relay,
            },
        );
        self.announce(DeskEvent::AccountSignIn {
            sign_in: view.clone(),
        });
        tokio::spawn(self.clone().follow_sign_in(view.id, exit));
        Ok(view)
    }

    fn sign_in_moved(&self, id: Uuid, state: SignInState) {
        let view = {
            let mut st = self.0.state.lock();
            let Some(run) = st.sign_in_runs.get_mut(&id) else {
                return;
            };
            if run.view.state == state {
                return;
            }
            // Once it's over, there's no page to come back to.
            if state.is_over() {
                run.view.direct = None;
                run.view.direct_ready = false;
                run.relay = None;
            }
            run.view.state = state;
            run.view.clone()
        };
        self.announce(DeskEvent::AccountSignIn { sign_in: view });
    }

    /// The page that comes back by itself, once its CLI has made it.
    fn sign_in_direct_found(&self, id: Uuid, direct: accounts::Direct) {
        let view = {
            let mut st = self.0.state.lock();
            let Some(run) = st.sign_in_runs.get_mut(&id) else {
                return;
            };
            if run.view.direct.as_ref() == Some(&direct) || run.view.state.is_over() {
                return;
            }
            run.view.direct = Some(direct);
            run.view.direct_ready = true;
            run.view.clone()
        };
        self.announce(DeskEvent::AccountSignIn { sign_in: view });
    }

    /// Reads a sign-in's screen until it gives a token that works, says it
    /// failed, ends, or takes too long.
    async fn follow_sign_in(
        self,
        id: Uuid,
        mut exit: tokio::sync::oneshot::Receiver<crate::pty::Exit>,
    ) {
        let started = tokio::time::Instant::now();
        let mut tried: Vec<String> = Vec::new();
        // Why the last token read was refused, said if the run ends on it.
        let mut refused: Option<String> = None;
        let mut exited = false;
        let outcome = loop {
            let Some((pty, make_default, engine, name, relay)) = ({
                let st = self.0.state.lock();
                st.sign_in_runs.get(&id).map(|r| {
                    (
                        Arc::clone(&r.pty),
                        r.make_default,
                        r.view.engine,
                        r.view.name.clone(),
                        r.relay.clone(),
                    )
                })
            }) else {
                return; // cancelled
            };
            if let Some(direct) = relay.as_deref().and_then(Relay::direct) {
                self.sign_in_direct_found(id, direct);
            }
            let said = accounts::setup_said(&pty.text());
            let fresh: Vec<String> = said
                .candidates
                .into_iter()
                .filter(|c| !tried.contains(c))
                .collect();
            let mut ended = None;
            for candidate in fresh {
                tried.push(candidate.clone());
                let Ok(token) = AccountToken::new(&candidate) else {
                    continue;
                };
                self.sign_in_moved(id, SignInState::Checking);
                // A token drawn only in part fails the check: the run goes on,
                // and the rest of it is read next time.
                if let Err(why) = self.check_claude_token(&token).await {
                    refused = Some(why);
                    continue;
                }
                let kept = match self.keys() {
                    Ok(keys) => {
                        let keys = Arc::clone(keys);
                        self.keep_account(keys, engine, name.clone(), make_default, &token)
                            .await
                    }
                    Err(e) => Err(e),
                };
                pty.kill();
                ended = Some(match kept {
                    Ok(a) => SignInState::Done { account: a.id },
                    Err(e) => {
                        tracing::warn!(error = %format!("{e:#}"), "couldn't keep an engine account");
                        SignInState::Failed {
                            why: "Desk couldn't keep the sign-in: try again".into(),
                        }
                    }
                });
                break;
            }
            if let Some(ended) = ended {
                break ended;
            }
            if exited {
                break SignInState::Failed {
                    why: match (refused.take(), said.failed) {
                        (Some(why), _) => why,
                        (None, true) => accounts::SETUP_FAILED.into(),
                        (None, false) => accounts::SETUP_UNREAD.into(),
                    },
                };
            }
            if started.elapsed() > SIGN_IN_FOR {
                pty.kill();
                break SignInState::Failed {
                    why: "the sign-in took too long: start it again".into(),
                };
            }
            self.sign_in_moved(
                id,
                match said.asks_code {
                    true => SignInState::Code { url: said.url },
                    false if said.url.is_some() => SignInState::Open { url: said.url },
                    false => SignInState::Starting,
                },
            );
            tokio::select! {
                _ = &mut exit, if !exited => {
                    exited = true;
                    // Read the screen it left once more.
                    tokio::time::sleep(SIGN_IN_EVERY).await;
                }
                _ = tokio::time::sleep(SIGN_IN_EVERY) => {}
                _ = self.0.cancel.cancelled() => {
                    pty.kill();
                    return;
                }
            }
        };
        self.sign_in_moved(id, outcome);
        self.forget_sign_in_later(id);
    }

    /// Lets a finished sign-in be seen for a while, then drops it.
    fn forget_sign_in_later(&self, id: Uuid) {
        let desk = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(60)).await;
            desk.0.state.lock().sign_in_runs.remove(&id);
        });
    }

    /// The page that comes back by itself for sign-in `id`, while it runs
    /// and has one: for the Desk that forwards its port, never announced.
    pub fn sign_in_direct(&self, id: Uuid) -> Result<accounts::Direct, DeskError> {
        self.0
            .state
            .lock()
            .sign_in_runs
            .get(&id)
            .filter(|r| !r.view.state.is_over())
            .and_then(|r| r.view.direct.clone())
            .ok_or(DeskError::NotFound)
    }

    /// Types the code the browser showed into a sign-in waiting for it.
    pub fn sign_in_code(&self, id: Uuid, code: &str) -> Result<SignInView, DeskError> {
        let code = code.trim();
        if code.is_empty() || code.len() > 512 || code.chars().any(char::is_control) {
            return Err(DeskError::Invalid(
                "paste the code the sign-in page shows".into(),
            ));
        }
        let (pty, view) = {
            let st = self.0.state.lock();
            let run = st
                .sign_in_runs
                .get(&id)
                .filter(|r| !r.view.state.is_over())
                .ok_or_else(|| DeskError::Conflict("that sign-in has ended".into()))?;
            (Arc::clone(&run.pty), run.view.clone())
        };
        pty.write(format!("{code}\r").as_bytes())
            .map_err(DeskError::Internal)?;
        Ok(view)
    }

    /// Ends a sign-in the person gave up on.
    pub fn cancel_sign_in(&self, id: Uuid) -> Result<(), DeskError> {
        let run = self.0.state.lock().sign_in_runs.remove(&id);
        let run = run.ok_or_else(|| DeskError::Conflict("that sign-in has ended".into()))?;
        run.pty.kill();
        Ok(())
    }

    /// Renames an account, or makes it (or none) the default.
    pub async fn update_account(
        &self,
        id: Uuid,
        patch: AccountPatch,
    ) -> Result<AccountView, DeskError> {
        let mut a = self.account(id)?;
        if let Some(name) = patch.name {
            let taken: Vec<String> = self
                .0
                .state
                .lock()
                .accounts
                .iter()
                .filter(|o| o.engine == a.engine && o.id != id)
                .map(|o| o.name.clone())
                .collect();
            a.name = accounts::check_name(&name, taken).map_err(DeskError::Invalid)?;
        }
        if let Some(d) = patch.is_default {
            a.is_default = d;
        }
        self.0
            .store
            .put_account(&a)
            .await
            .map_err(DeskError::Internal)?;
        {
            let mut st = self.0.state.lock();
            for o in st.accounts.iter_mut().filter(|o| o.engine == a.engine) {
                if o.id == id {
                    *o = a.clone();
                } else if a.is_default {
                    o.is_default = false;
                }
            }
        }
        self.announce_accounts();
        self.announce_sessions_on(id);
        self.accounts()
            .into_iter()
            .find(|v| v.id == id)
            .ok_or(DeskError::NotFound)
    }

    /// Removes an account and its sign-in. The sessions on it move to the
    /// default account (or the CLI's own sign-in, or with none here the
    /// first account that signs in) at their next start;
    /// running ones carry on as they are until then.
    pub async fn remove_account(&self, id: Uuid) -> Result<(), DeskError> {
        let a = self.account(id)?;
        let to = {
            let st = self.0.state.lock();
            accounts::default_of(&st.accounts, a.engine).filter(|d| *d != id)
        };
        self.0
            .store
            .delete_account(id, to)
            .await
            .map_err(DeskError::Internal)?;
        let store = self.accounts_store().unwrap_or("the store");
        // Why its sign-in couldn't be deleted, if it couldn't.
        let not_deleted: Option<String> = match self.keys().map(Arc::clone) {
            Ok(keys) => match tokio::task::spawn_blocking(move || keys.delete(id)).await {
                Ok(Ok(())) => None,
                Ok(Err(e)) => {
                    let why = format!("{e:#}");
                    tracing::warn!(account = %id, error = %why, "couldn't remove an engine account's sign-in");
                    Some(why)
                }
                Err(e) => {
                    tracing::warn!(account = %id, error = %e, "couldn't remove an engine account's sign-in");
                    Some(e.to_string())
                }
            },
            Err(_) => None,
        };
        let deleted = not_deleted.is_none();
        let (running, stopped): (Vec<Uuid>, Vec<Uuid>) = {
            let mut st = self.0.state.lock();
            st.accounts.retain(|o| o.id != id);
            st.links.remove(&id);
            // Nothing of it is anyone's now.
            st.prints.remove(&super::identities::Raw::Account(id));
            st.standing.remove(&Some(id));
            let gone = super::identities::Raw::Account(id);
            st.moved_off.retain(|m| m.from != gone && m.to != gone);
            st.signed_in_accounts.remove(&id);
            st.switches.retain(|_, to| *to != Some(id));
            st.sign_in_problems.remove(&id);
            if !deleted {
                st.sign_in_problems
                    .insert(id, accounts::SignInProblem::NotRemoved);
            }
            let on: Vec<Uuid> = st
                .records
                .values()
                .filter(|r| r.account == Some(id))
                .map(|r| r.id)
                .collect();
            // Running ones keep its name until they have moved.
            if !deleted || on.iter().any(|s| st.live.contains_key(s)) {
                st.removed_names.insert(id, a.name.clone());
            }
            on.into_iter().partition(|s| st.live.contains_key(s))
        };
        for s in stopped {
            self.change(s, |r, _| r.account = to);
        }
        self.announce_accounts();
        // Running sessions move to the default as a switch does: idle ones
        // now, working ones once their turn ends.
        for s in running {
            self.0.state.lock().switches.insert(s, to);
            let desk = self.clone();
            tokio::spawn(async move {
                if let Err(e) = desk.drive_switch(s, None).await {
                    tracing::warn!(session = %s, error = %e, "couldn't move a session off a removed account");
                }
            });
        }
        match not_deleted {
            None => Ok(()),
            Some(why) => Err(DeskError::Conflict(format!(
                "The account was removed, but its sign-in couldn't be deleted from {store}: {why}"
            ))),
        }
    }

    fn announce_sessions_on(&self, account: Uuid) {
        let views: Vec<SessionView> = {
            let st = self.0.state.lock();
            st.records
                .values()
                .filter(|r| {
                    r.account == Some(account) || st.switches.get(&r.id) == Some(&Some(account))
                })
                .map(|r| Self::view_of(&st, r))
                .collect()
        };
        for v in views {
            self.announce(DeskEvent::Session {
                session: Box::new(v),
            });
        }
    }

    /// What an account is called, for the ledger.
    pub(super) fn account_name(&self, account: Option<Uuid>) -> Option<String> {
        let id = account?;
        let st = self.0.state.lock();
        st.accounts
            .iter()
            .find(|a| a.id == id)
            .map(|a| a.name.clone())
    }

    /// The names of a session's account and of the one it moves to, for
    /// its view.
    pub(super) fn account_refs(
        st: &super::State,
        r: &crate::model::SessionRecord,
    ) -> (Option<AccountRef>, Option<AccountRef>) {
        let named = |id: Option<Uuid>| {
            let id = id?;
            st.accounts
                .iter()
                .find(|a| a.id == id)
                .map(|a| a.name.clone())
                .or_else(|| st.removed_names.get(&id).map(|n| format!("{n} (removed)")))
                .map(|name| AccountRef { id, name })
        };
        let to = st.switches.get(&r.id).map(|to| {
            named(*to).unwrap_or_else(|| AccountRef {
                id: Uuid::nil(),
                name: "this computer's sign-in".into(),
            })
        });
        (named(r.account), to)
    }

    /// Moves a session to another account (`None`: the CLI's own sign-in).
    /// One idle or done is restarted on it now; one working, once its turn
    /// ends; one whose last turn stopped on its limit, now and told to
    /// carry on; one not running, at its next start. The account it moves to
    /// must be signed in now: if its sign-in can't be read, the session
    /// stays where it is and the Floor says why. Switches of one session are
    /// carried out one at a time, the latest asked for winning, with one
    /// restart.
    pub async fn switch_account(
        &self,
        id: Uuid,
        to: Option<Uuid>,
        by: Option<By>,
    ) -> Result<SessionView, DeskError> {
        let r = self.record(id)?;
        if engine::caps(r.engine, r.view).accounts != AccountsVia::Token {
            return Err(DeskError::Invalid(format!(
                "{} sessions run on its own sign-in here",
                r.engine.title()
            )));
        }
        if let Some(to) = to {
            self.account_for(r.engine, Some(Some(to)))?;
        }
        if r.terminal_id.is_some() || r.origin == crate::model::Origin::External {
            return Err(DeskError::Conflict(
                "this session runs in a terminal of its own: start it again there, or sign in \
                 there with /login"
                    .into(),
            ));
        }
        if let Some(to) = to {
            let a = self.account(to)?;
            self.read_token(&a).await?;
        }
        self.0.state.lock().switches.insert(id, to);
        self.drive_switch(id, by).await
    }

    /// Carries out the switch a session waits on, unless one is already
    /// being carried out (that one takes the latest when it is done).
    pub(super) async fn drive_switch(
        &self,
        id: Uuid,
        by: Option<By>,
    ) -> Result<SessionView, DeskError> {
        if !self.0.state.lock().switching.insert(id) {
            let view = self.view(id)?;
            self.announce(DeskEvent::Session {
                session: Box::new(view.clone()),
            });
            return Ok(view);
        }
        let result = self.drive_switch_now(id, by).await;
        self.0.state.lock().switching.remove(&id);
        result
    }

    async fn drive_switch_now(&self, id: Uuid, by: Option<By>) -> Result<SessionView, DeskError> {
        let by = by.unwrap_or_else(By::you);
        loop {
            let Some(to) = self.0.state.lock().switches.get(&id).copied() else {
                return self.view(id);
            };
            let r = self.record(id)?;
            let (live, hit_limit) = {
                let st = self.0.state.lock();
                let live = st.live.contains_key(&id);
                // What Claude Code said of how its turn ended comes first;
                // the reply's words only when it said nothing.
                let hit_limit = matches!(r.state, SessionState::Done | SessionState::Idle)
                    && match st.turn_failures.get(&id) {
                        Some(f) => f.limit().is_some(),
                        None => r
                            .last_reply
                            .as_deref()
                            .is_some_and(|reply| crew::outcome_of(Ok(reply)).0 == Outcome::Limit),
                    };
                (live, hit_limit)
            };
            if r.account == to && !hit_limit {
                self.0.state.lock().switches.remove(&id);
                return self.view(id);
            }
            let when = accounts::switch_when(r.state, live, hit_limit);
            if when == SwitchWhen::AfterTurn {
                let view = self.view(id)?;
                self.announce(DeskEvent::Session {
                    session: Box::new(view.clone()),
                });
                return Ok(view);
            }
            // Read again just before it is stopped: a sign-in gone since it
            // was asked for leaves it where it is.
            if let Some(to) = to {
                let read = match self.account(to) {
                    Ok(a) => self.read_token(&a).await.map(drop),
                    Err(e) => Err(e),
                };
                if let Err(e) = read {
                    self.0.state.lock().switches.remove(&id);
                    if let Ok(view) = self.view(id) {
                        self.announce(DeskEvent::Session {
                            session: Box::new(view),
                        });
                    }
                    return Err(e);
                }
            }
            let from = r.account;
            self.0.state.lock().switches.remove(&id);
            self.change(id, |r, _| r.account = to);
            {
                // A removed account's name is kept only while something is on it.
                let mut st = self.0.state.lock();
                if let Some(from) = from {
                    let still = st.records.values().any(|r| r.account == Some(from))
                        || st.sign_in_problems.contains_key(&from);
                    if !still {
                        st.removed_names.remove(&from);
                    }
                }
            }
            match when {
                SwitchWhen::NextStart | SwitchWhen::AfterTurn => return self.view(id),
                SwitchWhen::Now => {
                    self.restart(id, None, by.clone()).await?;
                }
                SwitchWhen::NowWithContinue => {
                    self.restart(id, Some(accounts::CONTINUE.into()), by.clone())
                        .await?;
                }
            }
            // A switch asked for while it restarted: carried out next.
        }
    }
}

/// Where the person's "switch automatically" choice is kept.
pub(super) const AUTO_SWITCH: &str = "accounts.autoSwitch";

/// Each sign-in problem as a Floor need.
fn sign_in_needs(st: &super::State) -> Vec<accounts::AccountNeed> {
    st.sign_in_problems
        .iter()
        .filter_map(|(id, problem)| {
            let name = st
                .accounts
                .iter()
                .find(|a| a.id == *id)
                .map(|a| a.name.clone())
                .or_else(|| st.removed_names.get(id).cloned())?;
            Some(accounts::AccountNeed::SignIn {
                id: format!("sign-in:{}", id.simple()),
                engine: Engine::Claude,
                account: accounts::Side {
                    id: Some(*id),
                    name,
                },
                problem: *problem,
            })
        })
        .collect()
}

/// What moving every session off an account did.
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct SwitchedAll {
    /// Restarted on the new account now.
    pub now: Vec<Uuid>,
    /// Restarted once their turn ends.
    pub after_turn: Vec<Uuid>,
    /// Not running: on the new account at their next start.
    pub next_start: Vec<Uuid>,
    /// Running in a terminal of their own: to be started again there.
    pub in_terminals: Vec<Uuid>,
}

impl Desk {
    /// The account needs the Floor shows, less those the person put off.
    pub fn account_needs(&self) -> Vec<accounts::AccountNeed> {
        let own_on_plan = self.sign_ins().of(Engine::Claude).on_plan();
        let st = self.0.state.lock();
        // The CLI's own sign-in, known to be one of the accounts, is that
        // account: never one more side.
        let own_signed_in = own_on_plan && Self::own_account(&st).is_none();
        let standing = Self::effective_standing(&st);
        let moved_off = Self::effective_moves(&st);
        let mut on: std::collections::HashMap<Option<Uuid>, Vec<Uuid>> = Default::default();
        for r in st
            .records
            .values()
            .filter(|r| r.engine == Engine::Claude && st.live.contains_key(&r.id))
        {
            on.entry(Self::side_of_session(&st, r.id))
                .or_default()
                .push(r.id);
        }
        accounts::needs(
            Engine::Claude,
            &st.accounts,
            &standing,
            &on,
            &moved_off,
            own_signed_in,
            &st.signed_in_accounts
                .iter()
                .filter(|id| !st.sign_in_problems.contains_key(id))
                .copied()
                .collect(),
            Utc::now(),
        )
        .into_iter()
        .chain(sign_in_needs(&st))
        .filter(|n| !st.put_off.contains(n.id()))
        .collect()
    }

    /// Whether Desk moves sessions off an account at its limit by itself.
    pub fn auto_switch(&self) -> bool {
        self.0.state.lock().auto_switch
    }

    pub async fn set_auto_switch(&self, on: bool) -> Result<(), DeskError> {
        self.0
            .store
            .put_state(AUTO_SWITCH, &serde_json::json!(on))
            .await
            .map_err(DeskError::Internal)?;
        self.0.state.lock().auto_switch = on;
        self.accounts_moved().await;
        Ok(())
    }

    /// Puts a need off until what it says changes.
    pub fn put_off(&self, need: &str) {
        self.0.state.lock().put_off.insert(need.to_string());
        self.announce_accounts();
    }

    /// What a session on `account` reported of its plan's windows.
    pub(super) fn plan_read(
        &self,
        raw: &super::identities::Raw,
        windows: Vec<crate::usage::LimitWindow>,
    ) {
        if windows.is_empty() {
            return;
        }
        {
            let mut st = self.0.state.lock();
            let s = Self::standing_mut(&mut st, raw);
            s.windows = windows;
            s.read_at = Some(Utc::now());
        }
        let desk = self.clone();
        tokio::spawn(async move { desk.accounts_moved().await });
    }

    /// The accounts' needs may have changed: what was put off and no longer
    /// stands is forgotten, an account at its limit is switched off when the
    /// person said so, and every Desk is told.
    pub(super) async fn accounts_moved(&self) {
        if self.0.opts.account_keys.is_none() {
            return;
        }
        // One at a time: two reports close together never move the same
        // sessions twice.
        let Ok(_one) = self.0.accounts_moving.try_lock() else {
            return;
        };
        let needs = self.account_needs_all();
        {
            let mut st = self.0.state.lock();
            st.put_off.retain(|id| needs.iter().any(|n| n.id() == id));
        }
        if self.auto_switch() {
            for n in &needs {
                if let accounts::AccountNeed::Limit {
                    engine, from, to, ..
                } = n
                {
                    match self.switch_all(*engine, from.id, to.id).await {
                        Ok(done) => tracing::info!(
                            moved = done.now.len() + done.after_turn.len(),
                            "sessions moved off an account at its limit"
                        ),
                        Err(e) => {
                            tracing::warn!(error = %e, "couldn't move sessions off an account at its limit")
                        }
                    }
                }
            }
        }
        self.announce_accounts();
    }

    /// Every need, those put off too.
    fn account_needs_all(&self) -> Vec<accounts::AccountNeed> {
        let put_off = std::mem::take(&mut self.0.state.lock().put_off);
        let needs = self.account_needs();
        self.0.state.lock().put_off = put_off;
        needs
    }

    /// Moves every session of `engine` on `from` to `to`, each as
    /// [`Desk::switch_account`] does. Sessions in terminals of their own are
    /// listed, to be started again there.
    pub async fn switch_all(
        &self,
        engine: Engine,
        from: Option<Uuid>,
        to: Option<Uuid>,
    ) -> Result<SwitchedAll, DeskError> {
        if from == to {
            return Err(DeskError::Invalid("they are the same account".into()));
        }
        if let Some(to) = to {
            self.account_for(engine, Some(Some(to)))?;
            // Its sign-in must read before any session is stopped.
            let a = self.account(to)?;
            self.read_token(&a).await?;
        }
        let (ids, full) = {
            let st = self.0.state.lock();
            let ids: Vec<(Uuid, bool, bool)> = st
                .records
                .values()
                .filter(|r| {
                    r.engine == engine
                        && Self::side_of_session(&st, r.id) == from
                        && r.state != SessionState::Closed
                })
                .map(|r| {
                    (
                        r.id,
                        r.terminal_id.is_some() || r.origin == crate::model::Origin::External,
                        st.live.contains_key(&r.id),
                    )
                })
                .collect();
            let full = Self::standing_of(&st, from).full(Utc::now()).is_some();
            (ids, full)
        };
        let mut done = SwitchedAll::default();
        for (id, own_terminal, live) in ids {
            if own_terminal {
                done.in_terminals.push(id);
                continue;
            }
            match self.switch_account(id, to, None).await {
                Ok(v) if v.switching_to.is_some() => done.after_turn.push(id),
                Ok(_) if live => done.now.push(id),
                Ok(_) => done.next_start.push(id),
                Err(e) => {
                    tracing::warn!(session = %id, error = %e, "couldn't move a session to another account")
                }
            }
        }
        {
            let mut st = self.0.state.lock();
            // Moving back settles it; moving off a full account is kept
            // until it resets and the person is asked back.
            let st = &mut *st;
            let settled: Vec<bool> = st
                .moved_off
                .iter()
                .map(|m| {
                    m.engine == engine
                        && (Self::side_of_raw(st, &m.from) == Some(to)
                            || Self::side_of_raw(st, &m.to) == Some(from))
                })
                .collect();
            let mut at = 0;
            st.moved_off.retain(|_| {
                at += 1;
                !settled[at - 1]
            });
            if full {
                let moved = super::identities::RawMove {
                    engine,
                    from: Self::raw_of(st, from),
                    to: Self::raw_of(st, to),
                };
                if !st.moved_off.contains(&moved) {
                    st.moved_off.push(moved);
                }
            }
        }
        self.announce_accounts();
        Ok(done)
    }
}
