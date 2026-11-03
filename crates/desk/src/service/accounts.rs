//! The person's engine accounts: adding one (a pasted token, or signing in
//! through `claude setup-token` with its screen read here and never shown),
//! the default, removing one, and moving a session to another.
//!
//! A token is read from the store only to start a session's CLI, and goes
//! only into that CLI's environment ([`Desk::account_env`]).

use std::collections::BTreeMap;
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

/// A sign-in running: the engine's command, read until it gives a token.
pub(super) struct SignInRun {
    view: SignInView,
    make_default: bool,
    pty: Arc<Pty>,
}

impl Desk {
    fn keys(&self) -> Result<&Arc<dyn accounts::Keys>, DeskError> {
        self.0.opts.account_keys.as_ref().ok_or_else(|| {
            DeskError::Conflict(
                "this Desk keeps no accounts yet: update Desk to add accounts".into(),
            )
        })
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
                signed_in: keys.is_some_and(|k| k.get(a.id).ok().flatten().is_some()),
                plan_key: self
                    .0
                    .plans
                    .as_ref()
                    .and_then(|p| p.account_key(crate::usage::LocalPlan::Claude, &[plan_id(a.id)])),
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

    /// The store the accounts' sign-ins are kept in, as the person calls it;
    /// none when this Desk keeps no accounts.
    pub fn accounts_store(&self) -> Option<&'static str> {
        self.0.opts.account_keys.as_ref().map(|k| k.name())
    }

    /// The sign-ins under way, for a window that opens while one is.
    pub fn account_sign_ins(&self) -> Vec<SignInView> {
        let st = self.0.state.lock();
        st.sign_in_runs.values().map(|r| r.view.clone()).collect()
    }

    pub(super) fn announce_accounts(&self) {
        self.announce(DeskEvent::Accounts {
            accounts: self.accounts(),
            needs: self.account_needs(),
            auto_switch: self.auto_switch(),
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

    /// The account `wanted` names for a new session of `engine`: absent,
    /// its default; `null`, the CLI's own sign-in.
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

    /// What a session's account adds to its CLI's environment: its token,
    /// with what Claude Code would rank above it taken out. Nothing for a
    /// session on the CLI's own sign-in.
    pub(super) fn account_env(
        &self,
        engine: Engine,
        account: Option<Uuid>,
        env: &mut BTreeMap<String, String>,
    ) -> Result<(), DeskError> {
        let Some(id) = account else {
            return Ok(());
        };
        let a = self.account(id).map_err(|_| {
            DeskError::Conflict(
                "the account this session ran on was removed: choose another for it".into(),
            )
        })?;
        let keys = self.keys()?;
        let token = keys.get(id).map_err(DeskError::Internal)?.ok_or_else(|| {
            DeskError::Conflict(format!(
                "{}'s sign-in is no longer in {}: add the account again in Settings → Engines",
                a.name,
                keys.name()
            ))
        })?;
        match engine {
            Engine::Claude => {
                for k in accounts::OUTRANKS_CLAUDE_TOKEN {
                    env.remove(k);
                }
                env.insert(
                    accounts::CLAUDE_TOKEN_ENV.into(),
                    token.expose().to_string(),
                );
                Ok(())
            }
            Engine::Codex
            | Engine::Gemini
            | Engine::Copilot
            | Engine::OpenCode
            | Engine::Cursor
            | Engine::Devin
            | Engine::Custom => Err(DeskError::Invalid(format!(
                "{} sessions run on its own sign-in here",
                engine.title()
            ))),
        }
    }

    /// The environment Claude Code runs a session on `token` in: a
    /// session's, as Desk adds to it.
    fn claude_env_with(&self, token: &AccountToken) -> BTreeMap<String, String> {
        let mut env = self.with_added(&self.0.state.lock().session_env.clone());
        for k in accounts::OUTRANKS_CLAUDE_TOKEN {
            env.remove(k);
        }
        env.insert(
            accounts::CLAUDE_TOKEN_ENV.into(),
            token.expose().to_string(),
        );
        env
    }

    /// Whether Claude Code, run as a session on `token` is, signs in by it.
    async fn check_claude_token(&self, token: &AccountToken) -> Result<(), String> {
        let program = self
            .program_of(Engine::Claude)
            .map_err(|e| e.to_string())?
            .to_path_buf();
        let run = tokio::process::Command::new(program)
            .args(["auth", "status", "--json"])
            .env_clear()
            .envs(self.claude_env_with(token))
            .stdin(std::process::Stdio::null())
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(SIGN_IN_WAIT, run)
            .await
            .map_err(|_| "Claude Code didn't answer in time".to_string())?
            .map_err(|_| "Claude Code couldn't be run".to_string())?;
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
                    .keep_account(&*keys, new.engine, name, new.make_default, &token)
                    .await?;
                let view = self
                    .accounts()
                    .into_iter()
                    .find(|a| a.id == account.id)
                    .ok_or_else(|| DeskError::Internal(eyre::eyre!("account not kept")))?;
                Ok(Added::Account { account: view })
            }
            None => self
                .start_sign_in(new.engine, name, new.make_default)
                .map(|sign_in| Added::SignIn { sign_in }),
        }
    }

    /// Keeps a checked token and its account, the token first: an account
    /// is never listed without its sign-in.
    async fn keep_account(
        &self,
        keys: &dyn accounts::Keys,
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
        keys.set(account.id, token).map_err(DeskError::Internal)?;
        if let Err(e) = self.0.store.put_account(&account).await {
            let _ = keys.delete(account.id);
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
        }
        self.announce_accounts();
        tracing::info!(account = %account.id, engine = engine.as_str(), "engine account added");
        Ok(account)
    }

    /// Starts `claude setup-token` on a terminal of the hub's own, which no
    /// one is shown: its screen is read here for the sign-in page, whether
    /// it asks for a code, and the token it prints.
    fn start_sign_in(
        &self,
        engine: Engine,
        name: String,
        make_default: bool,
    ) -> Result<SignInView, DeskError> {
        let program = self.program_of(engine)?.to_path_buf();
        let mut env = self.with_added(&self.0.state.lock().session_env.clone());
        for k in accounts::OUTRANKS_CLAUDE_TOKEN {
            env.remove(k);
        }
        env.remove(accounts::CLAUDE_TOKEN_ENV);
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
        };
        self.0.state.lock().sign_in_runs.insert(
            view.id,
            SignInRun {
                view: view.clone(),
                make_default,
                pty: Arc::new(pty),
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
            run.view.state = state;
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
        let mut exited = false;
        let outcome = loop {
            let Some((pty, make_default, engine, name)) = ({
                let st = self.0.state.lock();
                st.sign_in_runs.get(&id).map(|r| {
                    (
                        Arc::clone(&r.pty),
                        r.make_default,
                        r.view.engine,
                        r.view.name.clone(),
                    )
                })
            }) else {
                return; // cancelled
            };
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
                if let Err(why) = self.check_claude_token(&token).await {
                    pty.kill();
                    ended = Some(SignInState::Failed { why });
                    break;
                }
                let kept = match self.keys() {
                    Ok(keys) => {
                        let keys = Arc::clone(keys);
                        self.keep_account(&*keys, engine, name.clone(), make_default, &token)
                            .await
                    }
                    Err(e) => Err(e),
                };
                pty.kill();
                ended = Some(match kept {
                    Ok(a) => SignInState::Done { account: a.id },
                    Err(e) => SignInState::Failed { why: e.to_string() },
                });
                break;
            }
            if let Some(ended) = ended {
                break ended;
            }
            if exited {
                break SignInState::Failed {
                    why: said.failed.unwrap_or_else(|| {
                        "Desk couldn't read the token from Claude Code: paste it instead".into()
                    }),
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
    /// default account (or the CLI's own sign-in) at their next start;
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
        if let Ok(keys) = self.keys() {
            if let Err(e) = keys.delete(id) {
                tracing::warn!(account = %id, error = %e, "couldn't remove an engine account's sign-in");
            }
        }
        let moved: Vec<Uuid> = {
            let mut st = self.0.state.lock();
            st.accounts.retain(|o| o.id != id);
            st.switches.retain(|_, to| *to != Some(id));
            st.records
                .values()
                .filter(|r| r.account == Some(id))
                .map(|r| r.id)
                .collect()
        };
        for s in moved {
            self.change(s, |r, _| r.account = to);
        }
        self.announce_accounts();
        Ok(())
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
            id.and_then(|id| st.accounts.iter().find(|a| a.id == id))
                .map(|a| AccountRef {
                    id: a.id,
                    name: a.name.clone(),
                })
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
    /// carry on; one not running, at its next start.
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
        let (live, hit_limit) = {
            let st = self.0.state.lock();
            let live = st.live.contains_key(&id);
            let hit_limit = matches!(r.state, SessionState::Done | SessionState::Idle)
                && r.last_reply
                    .as_deref()
                    .is_some_and(|reply| crew::outcome_of(Ok(reply)).0 == Outcome::Limit);
            (live, hit_limit)
        };
        if r.account == to && !hit_limit {
            self.0.state.lock().switches.remove(&id);
            return self.view(id);
        }
        let by = by.unwrap_or_else(By::you);
        match accounts::switch_when(r.state, live, hit_limit) {
            SwitchWhen::NextStart => {
                self.0.state.lock().switches.remove(&id);
                self.change(id, |r, _| r.account = to);
                self.view(id)
            }
            SwitchWhen::AfterTurn => {
                self.0.state.lock().switches.insert(id, to);
                let view = self.view(id)?;
                self.announce(DeskEvent::Session {
                    session: Box::new(view.clone()),
                });
                Ok(view)
            }
            SwitchWhen::Now => {
                self.0.state.lock().switches.remove(&id);
                self.change(id, |r, _| r.account = to);
                self.restart(id, None, by).await
            }
            SwitchWhen::NowWithContinue => {
                self.0.state.lock().switches.remove(&id);
                self.change(id, |r, _| r.account = to);
                self.restart(id, Some(accounts::CONTINUE.into()), by).await
            }
        }
    }
}

/// Where the person's "switch automatically" choice is kept.
pub(super) const AUTO_SWITCH: &str = "accounts.autoSwitch";

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
        let own_signed_in = self.sign_ins().of(Engine::Claude).on_plan();
        let st = self.0.state.lock();
        let mut on: std::collections::HashMap<Option<Uuid>, Vec<Uuid>> = Default::default();
        for r in st
            .records
            .values()
            .filter(|r| r.engine == Engine::Claude && st.live.contains_key(&r.id))
        {
            on.entry(r.account).or_default().push(r.id);
        }
        accounts::needs(
            Engine::Claude,
            &st.accounts,
            &st.standing,
            &on,
            &st.moved_off,
            own_signed_in,
            Utc::now(),
        )
        .into_iter()
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
    pub(super) fn plan_read(&self, account: Option<Uuid>, windows: Vec<crate::usage::LimitWindow>) {
        if windows.is_empty() {
            return;
        }
        {
            let mut st = self.0.state.lock();
            let s = st.standing.entry(account).or_default();
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
        }
        let (ids, full) = {
            let st = self.0.state.lock();
            let ids: Vec<(Uuid, bool, bool)> = st
                .records
                .values()
                .filter(|r| {
                    r.engine == engine && r.account == from && r.state != SessionState::Closed
                })
                .map(|r| {
                    (
                        r.id,
                        r.terminal_id.is_some() || r.origin == crate::model::Origin::External,
                        st.live.contains_key(&r.id),
                    )
                })
                .collect();
            let full = st
                .standing
                .get(&from)
                .is_some_and(|s| s.full(Utc::now()).is_some());
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
            st.moved_off
                .retain(|m| !(m.engine == engine && (m.from == to || m.to == from)));
            if full {
                st.moved_off.push(accounts::MovedOff { engine, from, to });
            }
        }
        self.announce_accounts();
        Ok(done)
    }
}
