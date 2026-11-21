//! BankSync (`api.banksync.io/v1`), read only.
//!
//! A key is bound to one BankSync workspace and may reach every bank in it,
//! so a connection is a key and the bank it reads (by name or id). Only
//! `GET` is ever sent, never redirected, and a key is used only once
//! `whoami` shows it can do nothing but read.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde_json::{Number, Value};
use sha2::{Digest, Sha256};
use tokio::time::Instant;
use tracing::warn;
use workforce_common::secret::Secret;
use workforce_common::settings::SecretStore;

use super::model::{masked, Account, Balance, BankError, Connection, Page, Transaction, Window};
use super::provider::BankProvider;
use crate::adapters::auth::{field_paths, resolve_credential};

pub const API: &str = "https://api.banksync.io/v1";

/// The scopes a key may hold: reading banks, and the institutions list.
pub const READ_SCOPES: &[&str] = &["banks:read", "institutions:read"];

/// BankSync's limit: 10 requests a minute for a workspace, all its keys.
const PER_MINUTE: usize = 10;
const MINUTE: Duration = Duration::from_secs(60);
/// The longest one read waits for the limit, by itself or on a 429.
const MOST_WAIT: Duration = Duration::from_secs(10);
const TIMEOUT: Duration = Duration::from_secs(20);

/// Whether a key holding `scopes` can only read banks.
pub fn read_only(scopes: &[String]) -> bool {
    scopes.iter().any(|s| s == "banks:read")
        && scopes.iter().all(|s| READ_SCOPES.contains(&s.as_str()))
}

/// The requests made of BankSync in the last minute, by person and BankSync
/// workspace: every connection and agent of theirs on one workspace shares
/// its limit.
#[derive(Default)]
pub struct Limiter(Mutex<HashMap<String, VecDeque<Instant>>>);

static SHARED: std::sync::LazyLock<Arc<Limiter>> = std::sync::LazyLock::new(Arc::default);

impl Limiter {
    /// The process's one limiter, which every agent's connections share.
    pub fn shared() -> Arc<Limiter> {
        Arc::clone(&SHARED)
    }

    /// Takes a slot for `who`, waiting up to [`MOST_WAIT`] for one.
    async fn take(&self, who: &str) -> Result<(), BankError> {
        loop {
            let wait = {
                let mut all = self.0.lock().unwrap_or_else(PoisonError::into_inner);
                let sent = all.entry(who.to_string()).or_default();
                let now = Instant::now();
                while sent.front().is_some_and(|&t| now - t >= MINUTE) {
                    sent.pop_front();
                }
                match sent.len() < PER_MINUTE {
                    true => {
                        sent.push_back(now);
                        return Ok(());
                    }
                    false => MINUTE - (now - sent[0]),
                }
            };
            if wait > MOST_WAIT {
                return Err(BankError::RateLimited {
                    retry_after: wait.as_secs().max(1),
                });
            }
            tokio::time::sleep(wait).await;
        }
    }
}

/// Where a connection's key and bank come from.
enum Keys {
    Fixed {
        api_key: Secret<String>,
        bank: String,
    },
    Store {
        store: Arc<dyn SecretStore>,
        workspace: String,
    },
}

pub struct BankSync {
    http: reqwest::Client,
    base: String,
    keys: Keys,
    limiter: Arc<Limiter>,
    /// The digest of the key `whoami` last showed reads only, and the
    /// BankSync workspace it is of.
    verified: Mutex<Option<([u8; 32], String)>>,
    /// The bank named, and its id, under the key last verified.
    bank_id: Mutex<Option<(String, String)>>,
}

impl BankSync {
    /// A connection on `api_key` reading `bank`, at `base` (BankSync's
    /// [`API`] outside tests).
    pub fn fixed(api_key: &str, bank: &str, base: &str, limiter: Arc<Limiter>) -> Self {
        Self::new(
            Keys::Fixed {
                api_key: Secret::new(api_key.to_string()),
                bank: bank.to_string(),
            },
            base,
            limiter,
        )
    }

    /// Connection `workspace`, its key and bank read from `store` as the
    /// caller, on every call.
    pub fn with_store(
        store: Arc<dyn SecretStore>,
        workspace: &str,
        base: &str,
        limiter: Arc<Limiter>,
    ) -> Self {
        Self::new(
            Keys::Store {
                store,
                workspace: workspace.to_string(),
            },
            base,
            limiter,
        )
    }

    fn new(keys: Keys, base: &str, limiter: Arc<Limiter>) -> Self {
        let http = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .timeout(TIMEOUT)
            .user_agent("workforce-desk")
            .build()
            .unwrap_or_default();
        Self {
            http,
            base: base.trim_end_matches('/').to_string(),
            keys,
            limiter,
            verified: Mutex::new(None),
            bank_id: Mutex::new(None),
        }
    }

    fn field(&self, field: &str) -> Option<String> {
        match &self.keys {
            Keys::Fixed { api_key, bank } => match field {
                "api_key" => Some(api_key.expose_secret().clone()),
                "bank" => Some(bank.clone()),
                _ => None,
            },
            Keys::Store { store, workspace } => {
                let (at, flat) = field_paths("bank", "workspaces", workspace, field);
                resolve_credential(store.as_ref(), &at, &flat).map(|r| r.value)
            }
        }
    }

    fn api_key(&self) -> Result<Secret<String>, BankError> {
        self.field("api_key")
            .filter(|k| !k.trim().is_empty())
            .map(|k| Secret::new(k.trim().to_string()))
            .ok_or(BankError::NotConfigured("key"))
    }

    /// `path` read with `key`, of BankSync `workspace` (empty before
    /// `whoami` has said which).
    async fn get(
        &self,
        key: &Secret<String>,
        workspace: &str,
        path: &str,
        query: &[(&str, String)],
    ) -> Result<Value, BankError> {
        let who = workforce_agent::runner::CURRENT_USER_ID
            .try_with(String::clone)
            .unwrap_or_default();
        let bucket = format!("{who}\u{0}{workspace}");
        let mut waited = false;
        loop {
            self.limiter.take(&bucket).await?;
            let sent = self
                .http
                .get(format!("{}{path}", self.base))
                .header("X-API-Key", key.expose_secret())
                .header("Accept", "application/json")
                .query(query)
                .send()
                .await
                .map_err(|_| BankError::Unavailable(None))?;
            let status = sent.status().as_u16();
            if status == 429 {
                let retry_after = crate::adapters::utils::retry_after_secs(sent.headers())
                    .unwrap_or(MINUTE.as_secs() / PER_MINUTE as u64);
                if !waited && Duration::from_secs(retry_after) <= MOST_WAIT {
                    waited = true;
                    tokio::time::sleep(Duration::from_secs(retry_after)).await;
                    continue;
                }
                return Err(BankError::RateLimited { retry_after });
            }
            if !sent.status().is_success() {
                warn!(op = path_op(path), status, "BankSync read refused");
                return Err(refusal(status));
            }
            return sent
                .json::<Value>()
                .await
                .map_err(|_| BankError::Unavailable(Some(status)));
        }
    }

    /// The key, once `whoami` has shown it only reads, and its workspace.
    /// A key not seen before forgets the bank found with the last one.
    async fn reader(&self) -> Result<(Secret<String>, String), BankError> {
        let key = self.api_key()?;
        let digest: [u8; 32] = Sha256::digest(key.expose_secret().as_bytes()).into();
        let known = self
            .verified
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .filter(|(d, _)| *d == digest);
        if let Some((_, workspace)) = known {
            return Ok((key, workspace));
        }
        let me = self.get(&key, "", "/whoami", &[]).await?;
        let scopes: Vec<String> = me["data"]["scopes"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|s| s.as_str().map(str::to_string))
            .collect();
        if !scopes.iter().any(|s| s == "banks:read") {
            return Err(BankError::ScopeMissing);
        }
        if !read_only(&scopes) {
            return Err(BankError::NotReadOnly);
        }
        let workspace = text(&me["data"]["workspaceId"]).unwrap_or_default();
        *self.bank_id.lock().unwrap_or_else(PoisonError::into_inner) = None;
        *self.verified.lock().unwrap_or_else(PoisonError::into_inner) =
            Some((digest, workspace.clone()));
        Ok((key, workspace))
    }

    /// The key, its workspace and the id of the bank the connection reads.
    async fn bank(&self) -> Result<(Secret<String>, String, String), BankError> {
        let (key, workspace) = self.reader().await?;
        let named = self
            .field("bank")
            .map(|b| b.trim().to_string())
            .filter(|b| !b.is_empty())
            .ok_or(BankError::NotConfigured("bank"))?;
        let cached = self
            .bank_id
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
            .filter(|(n, _)| *n == named);
        if let Some((_, id)) = cached {
            return Ok((key, workspace, id));
        }
        let banks = self.get(&key, &workspace, "/banks", &[]).await?;
        let id = bank_named(&banks, &named)?;
        *self.bank_id.lock().unwrap_or_else(PoisonError::into_inner) = Some((named, id.clone()));
        Ok((key, workspace, id))
    }
}

/// The id of the bank `named` in a `/banks` answer: the one with that id,
/// else the one with that name (any case), else the one whose name holds it.
fn bank_named(banks: &Value, named: &str) -> Result<String, BankError> {
    let all = banks["data"].as_array().cloned().unwrap_or_default();
    let wanted = named.to_lowercase();
    let name = |b: &Value| b["name"].as_str().map(str::to_lowercase);
    let by = |pick: &dyn Fn(&Value) -> bool| -> Vec<String> {
        all.iter()
            .filter(|b| pick(b))
            .filter_map(|b| b["id"].as_str().map(str::to_string))
            .collect()
    };
    let found = [
        by(&|b| b["id"].as_str() == Some(named)),
        by(&|b| name(b).as_deref() == Some(wanted.as_str())),
        by(&|b| name(b).is_some_and(|n| n.contains(&wanted))),
    ]
    .into_iter()
    .find(|ids| !ids.is_empty())
    .unwrap_or_default();
    match found.as_slice() {
        [id] => Ok(id.clone()),
        [] => Err(BankError::Invalid(format!(
            "BankSync has no bank called {named}. The person connects it in BankSync first."
        ))),
        _ => Err(BankError::Invalid(format!(
            "More than one bank in BankSync is called {named}: name it more fully."
        ))),
    }
}

/// What a refused read means.
fn refusal(status: u16) -> BankError {
    match status {
        401 => BankError::Revoked,
        403 => BankError::ScopeMissing,
        404 => BankError::NotFound("bank or account"),
        503 => BankError::NeedsReconnect,
        400 => BankError::Invalid("BankSync refused the request as asked.".into()),
        s => BankError::Unavailable(Some(s)),
    }
}

/// The kind of read `path` is, for a log line: no ids.
fn path_op(path: &str) -> &'static str {
    match path.rsplit('/').next() {
        Some("whoami") => "whoami",
        Some("transactions") => "transactions",
        Some("balances") => "balances",
        Some("accounts") => "accounts",
        _ => "banks",
    }
}

/// Whether `id` can stand in a path: BankSync's ids are plain words.
fn plain_id(id: &str) -> Result<&str, BankError> {
    match !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
    {
        true => Ok(id),
        false => Err(BankError::Invalid(format!(
            "`{}` isn't an account id: use one bank.accounts gives.",
            id.chars().take(64).collect::<String>()
        ))),
    }
}

fn text(v: &Value) -> Option<String> {
    v.as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn number(v: &Value) -> Option<Number> {
    v.as_number().cloned()
}

pub fn connection_of(bank: &Value) -> Connection {
    let status = &bank["connectionStatus"];
    Connection {
        bank: text(&bank["name"]).unwrap_or_else(|| "Bank".into()),
        connected: status["connected"].as_bool().unwrap_or(false),
        status: text(&status["status"]),
        history: text(&bank["historyReadiness"]["state"]),
    }
}

pub fn account_of(a: &Value) -> Account {
    Account {
        id: text(&a["id"]).unwrap_or_default(),
        name: text(&a["accountName"]),
        kind: text(&a["accountType"]).unwrap_or_default(),
        number: masked(a["accountNumber"].as_str().unwrap_or_default()),
        currency: text(&a["currency"]).unwrap_or_default(),
        balance: number(&a["balance"]),
        available: number(&a["availableBalance"]),
    }
}

pub fn balance_of(b: &Value) -> Result<Balance, BankError> {
    Ok(Balance {
        account: text(&b["accountId"]).unwrap_or_default(),
        name: text(&b["accountName"]),
        currency: text(&b["currency"]).unwrap_or_default(),
        amount: number(&b["amount"]).ok_or(BankError::Unavailable(None))?,
        available: number(&b["availableBalance"]),
        pending: number(&b["pendingBalance"]),
        credit_limit: number(&b["creditLimit"]),
        as_of: text(&b["balanceAsOf"]),
        retrieved_at: text(&b["retrievedAt"]),
    })
}

pub fn transaction_of(t: &Value) -> Option<Transaction> {
    Some(Transaction {
        id: text(&t["id"])?,
        date: text(&t["date"])?,
        description: text(&t["description"]).unwrap_or_default(),
        merchant: text(&t["merchantName"]),
        amount: number(&t["amount"])?,
        currency: text(&t["currency"]).unwrap_or_default(),
        category: text(&t["category"]),
        kind: text(&t["type"]),
        reference: text(&t["reference"]),
        pending: t["pending"].as_bool().unwrap_or(false),
        card: text(&t["cardLast4"]).map(|c| masked(&c)),
    })
}

pub fn page_of(body: &Value) -> Page {
    let meta = &body["meta"];
    Page {
        transactions: body["data"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(transaction_of)
            .collect(),
        cursor: text(&meta["cursor"]),
        has_more: meta["hasMore"].as_bool().unwrap_or(false),
        partial: meta["partial"].as_bool().unwrap_or(false),
    }
}

#[async_trait]
impl BankProvider for BankSync {
    async fn connection(&self) -> Result<Connection, BankError> {
        let (key, ws, id) = self.bank().await?;
        let bank = self.get(&key, &ws, &format!("/banks/{id}"), &[]).await?;
        Ok(connection_of(&bank["data"]))
    }

    async fn accounts(&self) -> Result<Vec<Account>, BankError> {
        let (key, ws, id) = self.bank().await?;
        let body = self
            .get(&key, &ws, &format!("/banks/{id}/accounts"), &[])
            .await?;
        Ok(body["data"]
            .as_array()
            .into_iter()
            .flatten()
            .map(account_of)
            .collect())
    }

    async fn balance(&self, account: &str) -> Result<Balance, BankError> {
        let account = plain_id(account)?;
        let (key, ws, id) = self.bank().await?;
        let body = self
            .get(
                &key,
                &ws,
                &format!("/banks/{id}/accounts/{account}/balances"),
                &[],
            )
            .await?;
        balance_of(&body["data"])
    }

    async fn transactions(
        &self,
        account: &str,
        window: Window,
        cursor: Option<&str>,
    ) -> Result<Page, BankError> {
        let account = plain_id(account)?;
        let (key, ws, id) = self.bank().await?;
        let query: Vec<(&str, String)> = [
            ("from", Some(window.from.to_string())),
            ("to", Some(window.to.to_string())),
            ("cursor", cursor.map(str::to_string)),
        ]
        .into_iter()
        .filter_map(|(k, v)| v.map(|v| (k, v)))
        .collect();
        let body = self
            .get(
                &key,
                &ws,
                &format!("/banks/{id}/accounts/{account}/transactions"),
                &query,
            )
            .await?;
        Ok(page_of(&body))
    }

    fn consented_at(&self) -> Option<DateTime<Utc>> {
        self.field(super::CONSENTED_AT)
            .and_then(|at| DateTime::parse_from_rfc3339(at.trim()).ok())
            .map(|at| at.with_timezone(&Utc))
    }
}

/// What a key and bank test as, for Connections: the bank's name when the
/// key only reads and the bank's approval stands.
pub async fn check(api_key: &str, bank: &str, base: &str) -> Result<Connection, BankError> {
    BankSync::fixed(api_key, bank, base, Limiter::shared())
        .connection()
        .await
}

#[cfg(test)]
mod tests;
