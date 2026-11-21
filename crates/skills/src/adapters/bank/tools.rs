//! `bank.*`: a person's own bank connections, read only.
//!
//! Each tool takes an optional `workspace`, the connection (one bank) to
//! read, and says which it read. What a bank holds (descriptions,
//! references, merchant names) is anyone's to write, so it reaches the
//! model fenced as external content.

use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use base64::Engine;
use chrono::{NaiveDate, Utc};
use eyre::Result;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tracing::warn;

use workforce_agent::security::external_content::wrap_external_content;
use workforce_agent::tools::{Tool, ToolResult};

use super::model::{Approval, BankError, Transaction, Window};
use super::provider::BankProvider;
use crate::adapters::utils::{
    coerce_to_u64, instance_choices, unknown_instance, workspaces_answer, LiveInstances,
    LIVE_INSTANCE_DESCRIPTION,
};

const DEFAULT_LIMIT: u64 = 50;
const MOST_LIMIT: u64 = 200;
/// The most provider pages one search reads: BankSync allows 10 requests a
/// minute.
const SEARCH_PAGES: usize = 4;

/// The connections a tool reads, and the one a call naming none reads.
pub struct Ctx<P> {
    clients: Arc<LiveInstances<P>>,
    default: String,
}

impl<P> Clone for Ctx<P> {
    fn clone(&self) -> Self {
        Self {
            clients: Arc::clone(&self.clients),
            default: self.default.clone(),
        }
    }
}

impl<P: BankProvider> Ctx<P> {
    pub fn new(clients: Arc<LiveInstances<P>>, default: String) -> Self {
        Self { clients, default }
    }

    /// The connection `input` names, or the caller's default.
    fn connection(&self, input: &Value) -> Result<(String, Arc<P>), String> {
        let (clients, default) = self.clients.for_caller_or(&self.default);
        let name = workforce_agent::llm::environment::resolve_instance(
            "bank",
            input["workspace"].as_str(),
            &default,
            &clients,
        )
        .map_err(|d| d.message)?;
        clients
            .get(&name)
            .cloned()
            .map(|client| (name.clone(), client))
            .ok_or_else(|| unknown_instance("bank connection", &name, &clients).to_string())
    }

    fn workspace_prop(&self) -> Value {
        let description = match self.clients.is_live() {
            true => LIVE_INSTANCE_DESCRIPTION.to_string(),
            false => {
                let (clients, default) = self.clients.for_caller_or(&self.default);
                format!(
                    "Which bank connection to read. {}",
                    instance_choices(&clients, &default, "workspace")
                )
            }
        };
        json!({ "type": "string", "description": description })
    }

    fn schema(&self, mut properties: Value) -> Value {
        properties["workspace"] = self.workspace_prop();
        json!({ "type": "object", "properties": properties })
    }
}

/// What `connection` read, fenced: everything a bank says is untrusted.
fn answer(connection: &str, data: &impl Serialize) -> Value {
    let rendered = serde_json::to_string(data).unwrap_or_default();
    json!({
        "connection": connection,
        "source": "bank",
        "data": wrap_external_content(&rendered, "bank").wrapped_content,
    })
}

/// A listing's answer: its transactions fenced, and beside them, unfenced,
/// what Desk itself made (the cursor, the dates), so the cursor reaches the
/// model as it was given.
fn listing_answer(connection: &str, listing: Listing) -> Value {
    let mut out = answer(connection, &listing.transactions);
    out["account"] = json!(listing.account);
    out["from"] = json!(listing.from);
    out["to"] = json!(listing.to);
    out["next_cursor"] = json!(listing.next_cursor);
    out["partial"] = json!(listing.partial);
    out
}

fn failed(connection: &str, op: &'static str, e: &BankError, start: Instant) -> ToolResult {
    warn!(connection, op, "bank read failed");
    ToolResult::failure(e.to_string(), elapsed(start))
}

fn elapsed(start: Instant) -> u64 {
    start.elapsed().as_millis() as u64
}

/// The one account of `client`'s, when `account` names none.
async fn account_of<P: BankProvider>(client: &P, account: Option<&str>) -> Result<String, BankError> {
    if let Some(account) = account.map(str::trim).filter(|a| !a.is_empty()) {
        return Ok(account.to_string());
    }
    match client.accounts().await?.as_slice() {
        [one] => Ok(one.id.clone()),
        [] => Err(BankError::Invalid("This bank connection has no accounts.".into())),
        many => Err(BankError::Invalid(format!(
            "This bank has {} accounts: pass `account`, one of {}.",
            many.len(),
            many.iter()
                .map(|a| format!("{} ({} {})", a.id, a.name.as_deref().unwrap_or(&a.kind), a.number))
                .collect::<Vec<_>>()
                .join(", ")
        ))),
    }
}

fn date(input: &Value, key: &str) -> Result<Option<NaiveDate>, BankError> {
    input[key]
        .as_str()
        .map(|d| {
            d.trim().parse().map_err(|_| {
                BankError::Invalid(format!("`{key}` is a date like 2026-11-21."))
            })
        })
        .transpose()
}

// ── bank.workspaces ────────────────────────────────────────────────────

pub struct BankWorkspacesTool<P>(pub Ctx<P>);

#[async_trait]
impl<P: BankProvider + 'static> Tool for BankWorkspacesTool<P> {
    fn name(&self) -> &str {
        "bank.workspaces"
    }

    fn description(&self) -> &str {
        "The person's bank connections, one bank each, by name, as {workspaces, default}: default is the one a call without `workspace` reads, or null when you must pass one. Takes no arguments."
    }

    fn input_schema(&self) -> Value {
        json!({ "type": "object", "properties": {}, "required": [] })
    }

    async fn execute(&self, _input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let (clients, default) = self.0.clients.for_caller_or(&self.0.default);
        Ok(ToolResult::success(
            workspaces_answer(&clients, &default),
            elapsed(start),
        ))
    }
}

// ── bank.accounts ──────────────────────────────────────────────────────

pub struct BankAccountsTool<P>(pub Ctx<P>);

#[async_trait]
impl<P: BankProvider + 'static> Tool for BankAccountsTool<P> {
    fn name(&self) -> &str {
        "bank.accounts"
    }

    fn description(&self) -> &str {
        "A bank connection's bank (whether its approval stands), its accounts with their balances, and when the person's approval of the bank lapses (`approval.until`; `approval.remind` is true on the days to remind them to renew it in BankSync). Read only: nothing can move money."
    }

    fn input_schema(&self) -> Value {
        self.0.schema(json!({}))
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let (name, client) = match self.0.connection(&input) {
            Ok(c) => c,
            Err(e) => return Ok(ToolResult::failure(e, elapsed(start))),
        };
        let read = async {
            let bank = client.connection().await?;
            let accounts = client.accounts().await?;
            let approval = client
                .consented_at()
                .map(|at| Approval::of(at, Utc::now().date_naive()));
            Ok::<_, BankError>(json!({ "bank": bank, "accounts": accounts, "approval": approval }))
        };
        Ok(match read.await {
            Ok(data) => ToolResult::success(answer(&name, &data), elapsed(start)),
            Err(e) => failed(&name, "accounts", &e, start),
        })
    }
}

// ── bank.balances ──────────────────────────────────────────────────────

pub struct BankBalancesTool<P>(pub Ctx<P>);

#[async_trait]
impl<P: BankProvider + 'static> Tool for BankBalancesTool<P> {
    fn name(&self) -> &str {
        "bank.balances"
    }

    fn description(&self) -> &str {
        "The balance of one account (`account`, an id bank.accounts gives) or of every account of a bank connection, as the bank last gave it (`as_of`, `retrieved_at`): the provider may be up to 6 hours behind. `partial` is true when the rate limit stopped it before every account. Read only."
    }

    fn input_schema(&self) -> Value {
        self.0.schema(json!({
            "account": { "type": "string", "description": "An account id from bank.accounts. Every account when omitted." }
        }))
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let (name, client) = match self.0.connection(&input) {
            Ok(c) => c,
            Err(e) => return Ok(ToolResult::failure(e, elapsed(start))),
        };
        let read = async {
            let ids = match input["account"].as_str().map(str::trim).filter(|a| !a.is_empty()) {
                Some(id) => vec![id.to_string()],
                None => client.accounts().await?.into_iter().map(|a| a.id).collect(),
            };
            let mut balances = Vec::with_capacity(ids.len());
            let mut partial = false;
            for id in &ids {
                match client.balance(id).await {
                    Ok(b) => balances.push(b),
                    Err(BankError::RateLimited { .. }) if !balances.is_empty() => {
                        partial = true;
                        break;
                    }
                    Err(e) => return Err(e),
                }
            }
            Ok(json!({ "balances": balances, "partial": partial }))
        };
        Ok(match read.await {
            Ok(data) => ToolResult::success(answer(&name, &data), elapsed(start)),
            Err(e) => failed(&name, "balances", &e, start),
        })
    }
}

// ── bank.transactions ──────────────────────────────────────────────────

/// Where a listing goes on from: the provider's page (by the cursor that
/// fetched it), how far into it, and what it was asked for.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
struct Resume {
    account: String,
    from: NaiveDate,
    to: NaiveDate,
    page: Option<String>,
    offset: usize,
}

impl Resume {
    fn encode(&self) -> String {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .encode(serde_json::to_vec(self).unwrap_or_default())
    }

    fn decode(raw: &str) -> Result<Self, BankError> {
        base64::engine::general_purpose::URL_SAFE_NO_PAD
            .decode(raw.trim())
            .ok()
            .and_then(|bytes| serde_json::from_slice(&bytes).ok())
            .ok_or_else(|| BankError::Invalid("That cursor isn't one bank.transactions gave.".into()))
    }
}

/// One listing: what it found, and where it goes on from.
#[derive(Debug)]
struct Listing {
    account: String,
    from: NaiveDate,
    to: NaiveDate,
    transactions: Vec<Transaction>,
    next_cursor: Option<String>,
    partial: bool,
}

/// Lists `limit` of `resume.account`'s transactions from `resume`, those
/// mentioning `search` only (reading up to [`SEARCH_PAGES`] pages for them).
/// A page is never skipped past: the next cursor goes on inside it.
async fn list<P: BankProvider>(
    client: &P,
    mut resume: Resume,
    search: Option<&str>,
    limit: usize,
) -> Result<Listing, BankError> {
    let window = Window {
        from: resume.from,
        to: resume.to,
    };
    let account = resume.account.clone();
    let pages = if search.is_some() { SEARCH_PAGES } else { 1 };
    let mut found = Vec::new();
    let mut partial = false;
    let mut next = None;
    for read in 0..pages {
        let page = match client
            .transactions(&resume.account, window, resume.page.as_deref())
            .await
        {
            Ok(page) => page,
            Err(BankError::RateLimited { .. }) if read > 0 => {
                partial = true;
                next = Some(resume.clone());
                break;
            }
            Err(e) => return Err(e),
        };
        partial |= page.partial;
        let full = page
            .transactions
            .into_iter()
            .enumerate()
            .skip(resume.offset)
            .filter(|(_, t)| search.is_none_or(|needle| t.mentions(needle)))
            .find_map(|(at, t)| match found.len() < limit {
                true => {
                    found.push(t);
                    None
                }
                false => Some(at),
            });
        if let Some(offset) = full {
            next = Some(Resume { offset, ..resume });
            break;
        }
        match (page.has_more, page.cursor) {
            (true, Some(cursor)) => {
                resume = Resume {
                    page: Some(cursor),
                    offset: 0,
                    ..resume
                };
                next = Some(resume.clone());
            }
            _ => {
                next = None;
                break;
            }
        }
    }
    Ok(Listing {
        account,
        from: window.from,
        to: window.to,
        transactions: found,
        next_cursor: next.map(|r| r.encode()),
        partial,
    })
}

pub struct BankTransactionsTool<P>(pub Ctx<P>);

#[async_trait]
impl<P: BankProvider + 'static> Tool for BankTransactionsTool<P> {
    fn name(&self) -> &str {
        "bank.transactions"
    }

    fn description(&self) -> &str {
        "An account's transactions, newest first as the bank gives them, from `from` to `to` (dates like 2026-11-21; the last 30 days by default). `search` keeps those whose description, merchant, reference or category mention it. Amounts: money in positive, out negative. More with `next_cursor` passed back as `cursor` (with the same search; the cursor keeps the account and dates). `partial` is true when the provider or the rate limit gave only part. Read only."
    }

    fn input_schema(&self) -> Value {
        self.0.schema(json!({
            "account": { "type": "string", "description": "An account id from bank.accounts. May be omitted when the bank has one account." },
            "from": { "type": "string", "description": "First day, YYYY-MM-DD." },
            "to": { "type": "string", "description": "Last day, YYYY-MM-DD. Today when omitted." },
            "search": { "type": "string", "description": "Words to look for, any case." },
            "limit": { "type": "integer", "minimum": 1, "maximum": MOST_LIMIT, "description": "At most this many (default 50)." },
            "cursor": { "type": "string", "description": "next_cursor from the last call, to go on." }
        }))
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let (name, client) = match self.0.connection(&input) {
            Ok(c) => c,
            Err(e) => return Ok(ToolResult::failure(e, elapsed(start))),
        };
        let read = async {
            let resume = match input["cursor"].as_str().filter(|c| !c.trim().is_empty()) {
                Some(cursor) => Resume::decode(cursor)?,
                None => {
                    let window = Window::of(
                        date(&input, "from")?,
                        date(&input, "to")?,
                        Utc::now().date_naive(),
                    )?;
                    Resume {
                        account: account_of(client.as_ref(), input["account"].as_str()).await?,
                        from: window.from,
                        to: window.to,
                        page: None,
                        offset: 0,
                    }
                }
            };
            let search = input["search"]
                .as_str()
                .map(|s| s.trim().to_lowercase())
                .filter(|s| !s.is_empty());
            let limit = coerce_to_u64(&input["limit"])
                .unwrap_or(DEFAULT_LIMIT)
                .clamp(1, MOST_LIMIT) as usize;
            list(client.as_ref(), resume, search.as_deref(), limit).await
        };
        Ok(match read.await {
            Ok(listing) => ToolResult::success(listing_answer(&name, listing), elapsed(start)),
            Err(e) => failed(&name, "transactions", &e, start),
        })
    }
}

#[cfg(test)]
mod tests;
