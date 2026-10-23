//! Agent know-how: what an agent learns about
//! where its data and code live from the tool calls that answered its users,
//! shared with everyone who uses it.
//!
//! What is shared is only names checked against something real: database,
//! tables and columns against the schema the Metabase sync recorded, a
//! query's shape with every value masked ([`sql`]), a name a query got wrong
//! beside the real one its retry used, and a repo, path and symbol against
//! the indexed checkout and its code graph ([`code`]). What a request was
//! after is kept too, abstracted (no names, no values) and embedded, but only
//! as a key to match later requests by: it is never shown or put in a
//! prompt. A name reaches other people once `min_users` different people
//! used it, and only people who could reach it anyway (the same Metabase
//! site, a repo their code tools read) see it. Re-checks ([`stale`]) hide
//! entries whose names are gone and delete those nobody used for 90 days.

pub mod admin;
pub(crate) mod agent;
pub mod code;
pub mod preview;
pub mod sql;
pub mod stale;
pub mod store;
pub mod tool;

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::{Duration, Instant};

use hmac::{Hmac, Mac};
use parking_lot::Mutex;
use sha2::{Digest, Sha256};
use workforce_common::metabase_schema::{DbSchema, InstanceSchema, MetabaseSchemaStore};
use workforce_common::settings::{KnowHowConfig, KnowHowInject, WorkforceConfig};
use workforce_memory::embedding::{certainty, EmbedKind, EmbeddingBackend};
use workforce_skills::adapters::metabase::tools::{classify_schema_error, SchemaError};

pub use code::CodeIndex;
pub use store::{
    Entry, InMemoryKnowHowStore, IntentVector, Kind, KnowHowStore, Observation, Revision,
};

/// The SQL tool whose successful calls teach where data lives.
pub const SQL_TOOL: &str = "metabase.dataset_query_native";
/// How long an agent's entries and intents are reused before re-reading.
const SNAPSHOT_TTL: Duration = Duration::from_secs(60);
/// Most words an abstracted intent may have.
const MAX_INTENT_WORDS: usize = 16;

/// A request rewritten as a generic intent, for matching; `None` when it
/// could not be written. Supplied by the hub (it owns the LLM).
pub type IntentWriter =
    Box<dyn FnOnce(String) -> Pin<Box<dyn Future<Output = Option<String>> + Send>> + Send>;

/// The instruction an intent is written with. Written in ordinary sentence
/// case, so a name the model failed to remove still stands out capitalised.
pub const INTENT_PROMPT: &str = "Rewrite this request to an assistant as ONE short generic intent \
(at most 12 words) describing what is wanted: the kind of data and its breakdown, or the part of the code \
and what it does. Remove every name, organisation, person, place, number, date and id; keep only the kind \
of thing wanted. Output only the intent.";

/// HKDF `info` separating the know-how person key from every other use of
/// the vault master key.
const PERSON_KEY_INFO: &[u8] = b"workforce know-how person v1";

/// An intent as written, kept only if it could not carry anything personal:
/// short, no digits, no quotes, mail addresses or links, and no capital
/// letter past the first (a name left in stands out that way). Returned in
/// lower case.
pub fn screen_intent(text: &str) -> Option<String> {
    let line = text.lines().find(|l| !l.trim().is_empty())?.trim();
    let line = line.trim_matches(|c: char| c == '"' || c == '\'' || c == '.' || c == '`');
    let words: Vec<&str> = line.split_whitespace().collect();
    if words.is_empty() || words.len() > MAX_INTENT_WORDS {
        return None;
    }
    let bad = line
        .chars()
        .any(|c| c.is_ascii_digit() || "@\"'`<>:/\\#".contains(c))
        || line.chars().skip(1).any(char::is_uppercase)
        || !line.chars().all(|c| c.is_ascii() || c.is_alphabetic());
    (!bad).then(|| line.to_lowercase())
}

/// Whether a turn's tool calls could teach anything: SQL (a success, or a
/// failure a later success corrected) or a code read.
pub fn teaches(invocations: &[workforce_agent::runner::ToolInvocation]) -> bool {
    invocations
        .iter()
        .any(|i| i.success && (i.tool_name == SQL_TOOL || i.tool_name == code::READ_TOOL))
}

/// The kinds of work a turn attempted that an entry could have helped with:
/// a query (even one that failed), a code search or read. Other data tools
/// name no tables a hit could be read from, so they are not counted.
#[derive(Debug, Clone, Copy, Default)]
struct Work {
    data: bool,
    code: bool,
}

impl Work {
    fn of(invocations: &[workforce_agent::runner::ToolInvocation]) -> Self {
        let did = |tools: &[&str]| {
            invocations
                .iter()
                .any(|i| tools.contains(&i.tool_name.as_str()))
        };
        Self {
            data: did(&[SQL_TOOL]),
            code: did(code::SEARCH_TOOLS) || did(&[code::READ_TOOL]),
        }
    }

    fn tried(self, kind: Kind) -> bool {
        match kind {
            Kind::CodeLocation => self.code,
            Kind::DataLocation | Kind::QueryShape | Kind::SchemaCorrection => self.data,
        }
    }
}

/// The Metabase instances one person can reach: `(instance, normalised
/// site)`, the instance a call without `instance` uses, and the instance an
/// active environment pins Metabase to (which then wins, as it does for the
/// tool).
#[derive(Debug, Clone, Default)]
pub struct Sites {
    pub instances: Vec<(String, String)>,
    pub default: Option<String>,
    pub pinned: Option<String>,
}

impl Sites {
    fn site_of(&self, instance: Option<&str>) -> Option<&str> {
        let name = instance
            .map(str::to_string)
            .or_else(|| self.pinned.clone())
            .or_else(|| self.default.clone())?;
        self.instances
            .iter()
            .find(|(n, _)| *n == name)
            .map(|(_, s)| s.as_str())
    }

    fn reaches(&self, site: &str) -> bool {
        self.instances.iter().any(|(_, s)| s == site)
    }
}

/// Whether a person (second) may use an agent (first), as its access policy
/// admits them to a chat.
pub type AdmitsFn = Arc<dyn Fn(&str, &str) -> bool + Send + Sync>;

/// What one person could open anyway: the Metabase sites their own access
/// reaches, the repos (`owner/repo`) their code tools read on this agent,
/// the agent's policy, which decides each file a `repo.read` opens, and
/// which other agents they may use: a test copy reads its source's
/// know-how only for someone the source admits (`None` admits nobody).
#[derive(Clone, Default)]
pub struct Access {
    pub sites: Sites,
    pub repos: HashSet<String>,
    pub policy: Option<Arc<workforce_policy::engine::PolicyEngine>>,
    pub admits: Option<AdmitsFn>,
}

/// Whether an agent's policy lets `repo.read` open each `(agent, repo,
/// path)`, as last asked. The policy is the agent's, not the person's, so
/// one answer serves everyone on that agent until the snapshot is read
/// again; a test copy reading another agent's snapshot keeps its own.
type Opens = Mutex<HashMap<(String, String, String), bool>>;

impl Access {
    pub(crate) fn reaches(&self, agent: &str, e: &Entry) -> bool {
        self.reaches_with(agent, e, None)
    }

    fn reaches_with(&self, agent: &str, e: &Entry, opens: Option<&Opens>) -> bool {
        match e.kind {
            Kind::CodeLocation => {
                let (Some(repo), Some(path)) = (e.repo.as_deref(), e.path.as_deref()) else {
                    return false;
                };
                let open = |p: &workforce_policy::engine::PolicyEngine| match opens {
                    Some(cache) => *cache
                        .lock()
                        .entry((agent.to_string(), repo.to_string(), path.to_string()))
                        .or_insert_with(|| code::policy_opens(p, agent, repo, path)),
                    None => code::policy_opens(p, agent, repo, path),
                };
                self.repos.contains(repo) && self.policy.as_deref().is_none_or(open)
            }
            Kind::DataLocation | Kind::QueryShape | Kind::SchemaCorrection => {
                self.sites.reaches(&e.site)
            }
        }
    }
}

/// A finished turn, as far as know-how cares.
pub struct Turn<'a> {
    pub agent: &'a str,
    pub identity: &'a str,
    /// A conversation several people are in.
    pub shared: bool,
    /// The person's request as they wrote it.
    pub question: &'a str,
    pub invocations: &'a [workforce_agent::runner::ToolInvocation],
    pub sites: Sites,
}

/// What a turn taught: entries it produced, and the names it used (tables
/// read, `code:` files read), to score what was matched for it.
#[derive(Default)]
struct Learned {
    found: Vec<Observation>,
    used: HashSet<String>,
}

impl Learned {
    fn absorb(&mut self, other: Learned) {
        self.found.extend(other.found);
        self.used.extend(other.used);
    }
}

struct Snapshot {
    at: Instant,
    known: Known,
}

/// What an agent knows, as last read.
#[derive(Clone)]
struct Known {
    entries: Arc<Vec<Entry>>,
    intents: Arc<Vec<(i64, Vec<f32>)>>,
    /// Different people per column (`table.column`) of each site and database.
    columns: Arc<HashMap<store::ColumnKey, u32>>,
    opens: Arc<Opens>,
}

/// What was matched for a session's last request, to score after the turn:
/// each entry with its kind and the names a turn must use for it to count
/// as a hit.
pub struct Matched {
    ids: Vec<(i64, Kind, Vec<String>)>,
    shown: bool,
}

pub struct KnowHow {
    store: Arc<dyn KnowHowStore>,
    embedder: Option<Arc<dyn EmbeddingBackend>>,
    /// Where schemas are read; the process's Metabase schema store unless set.
    schemas: Option<Arc<dyn MetabaseSchemaStore>>,
    /// Where code is checked; the process's code index unless set.
    code: Option<Arc<dyn CodeIndex>>,
    snapshots: Mutex<HashMap<String, Snapshot>>,
    matched: Mutex<HashMap<String, Matched>>,
    /// Keys the per-agent person hashes; without it nothing is recorded.
    person_key: Option<[u8; 32]>,
}

impl KnowHow {
    pub fn new(store: Arc<dyn KnowHowStore>, embedder: Option<Arc<dyn EmbeddingBackend>>) -> Self {
        Self {
            store,
            embedder,
            schemas: None,
            code: None,
            snapshots: Mutex::new(HashMap::new()),
            matched: Mutex::new(HashMap::new()),
            person_key: None,
        }
    }

    /// Key person hashes with `key` (from the vault master key in the hub).
    pub fn with_person_key(mut self, key: [u8; 32]) -> Self {
        self.person_key = Some(key);
        self
    }

    /// Who produced an entry, as stored: an HMAC per agent under an install
    /// secret, so the same person is counted once and the database alone
    /// cannot say who. `None` without a key: nothing is then recorded.
    pub fn person(&self, agent: &str, identity: &str) -> Option<String> {
        let key = self.person_key.as_ref()?;
        let mut mac =
            <Hmac<Sha256> as Mac>::new_from_slice(key).expect("HMAC takes a key of any length");
        mac.update(agent.as_bytes());
        mac.update(b"\0");
        mac.update(identity.as_bytes());
        Some(hex::encode(mac.finalize().into_bytes()))
    }

    /// The process's know-how: Postgres when the install has it, otherwise
    /// in memory; the hub's memory embedder when `[embeddings]` names one.
    pub async fn shared(config: &WorkforceConfig) -> Arc<Self> {
        static SHARED: tokio::sync::OnceCell<Arc<KnowHow>> = tokio::sync::OnceCell::const_new();
        SHARED
            .get_or_init(|| async {
                let embedder = match crate::embeddings::from_config(config).await {
                    Ok(Some((e, _))) => Some(e),
                    Ok(None) => None,
                    Err(e) => {
                        tracing::warn!(target: "know_how", error = %e, "no embedder; know-how matches by name only");
                        None
                    }
                };
                let mut kh = Self::new(open_store(config).await, embedder);
                match person_key(config) {
                    Some(key) => kh = kh.with_person_key(key),
                    None => tracing::warn!(target: "know_how", "no vault master key; know-how records nothing"),
                }
                Arc::new(kh)
            })
            .await
            .clone()
    }

    /// Read schemas from `schemas` instead of the process's store.
    pub fn with_schemas(mut self, schemas: Arc<dyn MetabaseSchemaStore>) -> Self {
        self.schemas = Some(schemas);
        self
    }

    /// Check code against `code` instead of the process's code index.
    pub fn with_code_index(mut self, code: Arc<dyn CodeIndex>) -> Self {
        self.code = Some(code);
        self
    }

    fn code(&self) -> Option<Arc<dyn CodeIndex>> {
        self.code.clone().or_else(code::code_index)
    }

    async fn instance_schema(&self, site: &str) -> Option<InstanceSchema> {
        let store = self
            .schemas
            .clone()
            .or_else(workforce_common::metabase_schema::store)?;
        store.load(site, site).await.ok()
    }

    async fn schema_of(&self, site: &str, db: i64) -> Option<DbSchema> {
        self.instance_schema(site).await?.databases.remove(&db)
    }

    /// Remove everything `identity` contributed, for every agent (when the
    /// identity is deleted, or an admin erases it).
    pub async fn forget_identity(&self, identity: &str) -> eyre::Result<usize> {
        let mut touched = 0;
        for agent in self.store.agents().await? {
            if let Some(person) = self.person(&agent, identity) {
                touched += self.store.forget_person(&agent, &person).await?;
                self.forget_snapshot(&agent);
            }
        }
        Ok(touched)
    }

    /// How many entries `identity` contributed to, across every agent.
    pub async fn identity_entries(&self, identity: &str) -> eyre::Result<usize> {
        let mut entries = 0;
        for agent in self.store.agents().await? {
            if let Some(person) = self.person(&agent, identity) {
                entries += self.store.produced_by(&agent, &person).await?.len();
            }
        }
        Ok(entries)
    }

    /// Make everything `from` contributed `to`'s, for every agent (when one
    /// identity is merged into another).
    pub async fn reassign_identity(&self, from: &str, to: &str) -> eyre::Result<usize> {
        let mut touched = 0;
        for agent in self.store.agents().await? {
            if let (Some(from), Some(to)) = (self.person(&agent, from), self.person(&agent, to)) {
                touched += self.store.reassign_person(&agent, &from, &to).await?;
                self.forget_snapshot(&agent);
            }
        }
        Ok(touched)
    }

    pub fn store(&self) -> &Arc<dyn KnowHowStore> {
        &self.store
    }

    pub fn model(&self) -> Option<&str> {
        self.embedder.as_ref().map(|e| e.model_id())
    }

    async fn snapshot(&self, agent: &str) -> eyre::Result<Known> {
        if let Some(s) = self.snapshots.lock().get(agent) {
            if s.at.elapsed() < SNAPSHOT_TTL {
                return Ok(s.known.clone());
            }
        }
        let known = Known {
            entries: Arc::new(self.store.entries(agent).await?),
            intents: Arc::new(match self.model() {
                Some(model) => self.store.intents(agent, model).await?,
                None => Vec::new(),
            }),
            columns: Arc::new(self.store.column_people(agent).await?),
            opens: Arc::default(),
        };
        self.snapshots.lock().insert(
            agent.to_string(),
            Snapshot {
                at: Instant::now(),
                known: known.clone(),
            },
        );
        Ok(known)
    }

    pub(crate) fn forget_snapshot(&self, agent: &str) {
        self.snapshots.lock().remove(agent);
    }

    /// Entries `identity` may see on `agent`, each with only the names they
    /// may see: what is shared ([`shared_view`]), and their own entries in
    /// full (not in a `shared` conversation, where everyone in it would see
    /// the answer); never a stale entry, and only what their own access
    /// reaches. A test copy reads its source's entries, and finds the
    /// person's own among them as the source would, but only for someone
    /// the source admits: whoever can edit a copy's config cannot show its
    /// users what an agent they may not use knows.
    pub async fn visible(
        &self,
        agent: &str,
        cfg: &KnowHowConfig,
        identity: &str,
        access: &Access,
        shared: bool,
    ) -> eyre::Result<Vec<Entry>> {
        let source = cfg.source(agent);
        if cfg.read_only()
            && !access
                .admits
                .as_ref()
                .is_some_and(|admits| admits(source, identity))
        {
            return Ok(Vec::new());
        }
        let known = self.snapshot(source).await?;
        let own = match (shared, self.person(source, identity)) {
            (false, Some(person)) => self.store.produced_by(source, &person).await?,
            _ => HashSet::new(),
        };
        let everyone = shared_view(&known, cfg.min_users.max(1));
        Ok(known
            .entries
            .iter()
            .filter(|e| !e.stale && access.reaches_with(agent, e, Some(&known.opens)))
            .filter_map(|e| match own.contains(&e.id) {
                true => Some(e.clone()),
                false => everyone.get(&e.id).cloned(),
            })
            .collect())
    }

    /// The entries a request matches, best first, each with its score, of
    /// every kind: what a person asking the agent what it knows is shown.
    pub async fn matches(
        &self,
        agent: &str,
        cfg: &KnowHowConfig,
        identity: &str,
        question: &str,
        access: &Access,
        shared: bool,
    ) -> eyre::Result<Vec<(Entry, f32)>> {
        if self.embedder.is_none() {
            return Ok(Vec::new());
        }
        let visible = self.visible(agent, cfg, identity, access, shared).await?;
        self.best(agent, cfg, visible, question).await
    }

    /// Of `visible`, the entries `question` matches, best first.
    async fn best(
        &self,
        agent: &str,
        cfg: &KnowHowConfig,
        visible: Vec<Entry>,
        question: &str,
    ) -> eyre::Result<Vec<(Entry, f32)>> {
        let Some(embedder) = self.embedder.as_ref().filter(|_| !visible.is_empty()) else {
            return Ok(Vec::new());
        };
        let intents = self.snapshot(cfg.source(agent)).await?.intents;
        let q = embedder.embed_query(question).await?;
        let ids: HashSet<i64> = visible.iter().map(|e| e.id).collect();
        let mut best: HashMap<i64, f32> = HashMap::new();
        for (id, v) in intents.iter().filter(|(id, _)| ids.contains(id)) {
            let s = certainty(&q, v);
            let slot = best.entry(*id).or_insert(0.0);
            if s > *slot {
                *slot = s;
            }
        }
        let mut out: Vec<(Entry, f32)> = visible
            .into_iter()
            .filter_map(|e| best.get(&e.id).map(|s| (e, *s)))
            .filter(|(_, s)| *s >= cfg.min_score)
            .collect();
        out.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
        out.truncate(cfg.max_entries.max(1));
        Ok(out)
    }

    /// What a request matches when anything is injected or recorded for it,
    /// of the kinds `cfg` matches requests against: nothing with inject off,
    /// or when matching fails.
    async fn matched_for(
        &self,
        agent: &str,
        cfg: &KnowHowConfig,
        identity: &str,
        question: &str,
        access: &Access,
        shared: bool,
    ) -> Vec<(Entry, f32)> {
        if cfg.inject == KnowHowInject::Off || self.embedder.is_none() {
            return Vec::new();
        }
        let matched = async {
            let visible = self.visible(agent, cfg, identity, access, shared).await?;
            self.best(agent, cfg, of_kinds(cfg, visible), question)
                .await
        };
        matched.await.unwrap_or_else(|e| {
            tracing::warn!(target: "know_how", %agent, error = %e, "know-how match failed");
            Vec::new()
        })
    }

    /// The block put in front of a request under `inject = "on"`, or
    /// recorded only under `shadow`; `None` when nothing matched or inject is
    /// off. A test copy records nothing, as for an eval's question.
    #[allow(clippy::too_many_arguments)]
    pub async fn for_request(
        &self,
        agent: &str,
        session: &str,
        cfg: &KnowHowConfig,
        identity: &str,
        question: &str,
        access: &Access,
        shared: bool,
    ) -> Option<String> {
        if cfg.read_only() {
            return self
                .for_eval(agent, cfg, identity, question, access, shared)
                .await;
        }
        // Whatever the last request matched and its turn did not settle (it
        // never finished) is settled now: nothing that turn did was seen.
        if let Some(m) = self.take_matched(session) {
            self.settle(agent, m, &[]).await;
        }
        let matched = self
            .matched_for(agent, cfg, identity, question, access, shared)
            .await;
        if matched.is_empty() {
            return None;
        }
        let shown = cfg.inject == KnowHowInject::On;
        let kind = if shown { "shown" } else { "shadow" };
        for (e, s) in &matched {
            let _ = self.store.event(agent, Some(e.id), kind, Some(*s)).await;
        }
        self.matched.lock().insert(
            session.to_string(),
            Matched {
                ids: matched
                    .iter()
                    .map(|(e, _)| (e.id, e.kind, evidence(e)))
                    .collect(),
                shown,
            },
        );
        let ids: Vec<i64> = matched.iter().map(|(e, _)| e.id).collect();
        let _ = self.store.mark_used(agent, &ids, shown).await;
        shown.then(|| render(&matched, cfg.max_tokens))
    }

    /// [`Self::for_request`] for an eval's question: the same block under
    /// `inject = "on"`, with nothing recorded of what matched, so an eval
    /// leaves the hit and shadow counts as people's own requests made them.
    pub async fn for_eval(
        &self,
        agent: &str,
        cfg: &KnowHowConfig,
        identity: &str,
        question: &str,
        access: &Access,
        shared: bool,
    ) -> Option<String> {
        let matched = self
            .matched_for(agent, cfg, identity, question, access, shared)
            .await;
        (cfg.inject == KnowHowInject::On && !matched.is_empty())
            .then(|| render(&matched, cfg.max_tokens))
    }

    /// What was matched for `session`'s request, taken when its turn ends so
    /// that a next request arriving before the turn is scored cannot settle
    /// it first.
    pub fn take_matched(&self, session: &str) -> Option<Matched> {
        self.matched.lock().remove(session)
    }

    /// Score what was matched for a turn that taught nothing (see
    /// [`teaches`]) from the `invocations` it made, none when it never
    /// finished: it used no names, so each entry is a miss when the turn
    /// attempted its kind of work and `untried` when it did not.
    pub async fn settle(
        &self,
        agent: &str,
        matched: Matched,
        invocations: &[workforce_agent::runner::ToolInvocation],
    ) {
        self.score(agent, matched, &HashSet::new(), Work::of(invocations))
            .await;
    }

    /// Learn from a finished turn: successful SQL on tables the schema knows
    /// becomes (or confirms) data locations and a query shape, a failed query
    /// its retry fixed becomes a schema correction, and a file a code search
    /// hit that was then read becomes a code location; each keyed to what the
    /// request was after. Also scores `matched`, what was matched for this
    /// request: a hit when the turn used an entry's names. A test copy that
    /// reads another agent's know-how learns nothing and scores nothing.
    pub async fn observe(
        &self,
        cfg: &KnowHowConfig,
        turn: Turn<'_>,
        matched: Option<Matched>,
        write_intent: IntentWriter,
    ) {
        if cfg.read_only() {
            return;
        }
        let Some(person) = self.person(turn.agent, turn.identity) else {
            return;
        };
        let mut learned = self.learn_data(&turn, &person).await;
        learned.absorb(self.learn_code(&turn, &person).await);
        if let Some(m) = matched {
            self.score(turn.agent, m, &learned.used, Work::of(turn.invocations))
                .await;
        }
        let mut found = learned.found;
        if found.is_empty() || !cfg.extract {
            return;
        }
        found.sort_by(|a, b| a.key.cmp(&b.key));
        // The same table read twice in a turn: one entry, every column.
        found.dedup_by(|later, kept| {
            if later.key != kept.key {
                return false;
            }
            for c in later.columns.drain(..) {
                if !kept.columns.contains(&c) {
                    kept.columns.push(c);
                }
            }
            kept.columns.sort();
            true
        });
        let intent = self.intent(turn.question, write_intent).await;
        for mut obs in found {
            obs.intent = intent.clone();
            if let Err(e) = self.store.record(turn.agent, obs).await {
                tracing::warn!(target: "know_how", agent = %turn.agent, error = %e, "recording know-how failed");
            }
        }
        self.forget_snapshot(turn.agent);
    }

    /// Data locations, query shapes and schema corrections from a turn's SQL.
    async fn learn_data(&self, turn: &Turn<'_>, person: &str) -> Learned {
        let mut schemas: HashMap<(String, i64), Option<DbSchema>> = HashMap::new();
        let mut calls: Vec<SqlCall> = Vec::new();
        for inv in turn.invocations.iter().filter(|i| i.tool_name == SQL_TOOL) {
            let (Some(db), Some(query)) = (
                inv.input.get("database_id").and_then(|v| v.as_i64()),
                inv.input.get("query").and_then(|v| v.as_str()),
            ) else {
                continue;
            };
            let Some(site) = turn
                .sites
                .site_of(inv.input.get("instance").and_then(|v| v.as_str()))
            else {
                continue;
            };
            // Reads only: a write says nothing about where answers live.
            if !is_read(query) {
                continue;
            }
            if let std::collections::hash_map::Entry::Vacant(slot) =
                schemas.entry((site.to_string(), db))
            {
                slot.insert(self.schema_of(site, db).await);
            }
            calls.push(SqlCall {
                site: site.to_string(),
                db,
                query: query.to_string(),
                rejected: match inv.success {
                    true => None,
                    false => inv.error.as_deref().and_then(classify_schema_error),
                },
                success: inv.success,
            });
        }
        let mut out = Learned::default();
        for (i, call) in calls.iter().enumerate() {
            let Some(schema) = schemas
                .get(&(call.site.clone(), call.db))
                .and_then(Option::as_ref)
            else {
                continue;
            };
            if call.success {
                out.absorb(read_from(call, schema, person));
                continue;
            }
            let Some(unknown) = call.rejected.as_ref().map(sql::Unknown::from) else {
                continue;
            };
            // The nearest later success on the same database is the retry.
            let Some(retry) = calls[i + 1..]
                .iter()
                .find(|c| c.success && c.site == call.site && c.db == call.db)
            else {
                continue;
            };
            if let Some(fix) = sql::correction(&call.query, &retry.query, &unknown, schema) {
                let right = fix.column.clone().unwrap_or_else(|| fix.table.clone());
                out.found.push(Observation {
                    site: call.site.clone(),
                    database_id: call.db,
                    tables: vec![fix.table],
                    columns: fix.column.into_iter().collect(),
                    wrong: Some(fix.wrong.clone()),
                    ..Observation::of(
                        Kind::SchemaCorrection,
                        format!("fix:{}:{}:{}:{right}", call.site, call.db, fix.wrong),
                        person.to_string(),
                    )
                });
            }
        }
        out
    }

    async fn intent(&self, question: &str, write_intent: IntentWriter) -> Option<IntentVector> {
        let embedder = self.embedder.as_ref()?;
        let text = screen_intent(&write_intent(question.chars().take(2_000).collect()).await?)?;
        match embedder
            .embed_many(std::slice::from_ref(&text), EmbedKind::Query)
            .await
        {
            Ok(mut v) if !v.is_empty() => Some(IntentVector {
                model: embedder.model_id().to_string(),
                vector: v.remove(0),
            }),
            Ok(_) => None,
            Err(e) => {
                tracing::warn!(target: "know_how", error = %e, "embedding an intent failed");
                None
            }
        }
    }

    /// One event per matched entry: a hit when the turn used its names, a
    /// miss when the turn attempted that kind of work (a query, a code
    /// search or read) without them, and `untried` when it attempted none,
    /// so it had no use for the entry either way.
    async fn score(&self, agent: &str, m: Matched, used: &HashSet<String>, work: Work) {
        let prefix = if m.shown { "" } else { "shadow_" };
        for (id, kind, names) in m.ids {
            let outcome = if !names.is_empty() && names.iter().all(|t| used.contains(t)) {
                "hit"
            } else if work.tried(kind) {
                "miss"
            } else {
                "untried"
            };
            let _ = self
                .store
                .event(agent, Some(id), &format!("{prefix}{outcome}"), None)
                .await;
        }
    }
}

/// One SQL call of a turn, resolved to its site.
struct SqlCall {
    site: String,
    db: i64,
    query: String,
    success: bool,
    /// For a failure: the name the database said is not real.
    rejected: Option<SchemaError>,
}

fn is_read(query: &str) -> bool {
    let first = query
        .trim_start()
        .split(|c: char| !c.is_alphabetic())
        .next()
        .unwrap_or_default()
        .to_lowercase();
    first == "select" || first == "with"
}

/// What one successful read teaches: an entry per table, so everyone who
/// reads a table confirms it however different the rest of their query, and
/// the query's shape.
fn read_from(call: &SqlCall, schema: &DbSchema, person: &str) -> Learned {
    let a = sql::analyse(&call.query, schema);
    let (site, db) = (call.site.as_str(), call.db);
    let mut found: Vec<Observation> = a
        .tables
        .iter()
        .map(|t| {
            let prefix = format!("{t}.");
            Observation {
                site: site.to_string(),
                database_id: db,
                tables: vec![t.clone()],
                columns: a
                    .columns
                    .iter()
                    .filter(|c| c.starts_with(&prefix))
                    .cloned()
                    .collect(),
                ..Observation::of(
                    Kind::DataLocation,
                    format!("data:{site}:{db}:{t}"),
                    person.to_string(),
                )
            }
        })
        .collect();
    if let Some(shape) = &a.shape {
        let digest = hex::encode(Sha256::digest(shape.as_bytes()));
        found.push(Observation {
            site: site.to_string(),
            database_id: db,
            tables: a.tables.clone(),
            columns: a.columns.clone(),
            shape: Some(shape.clone()),
            ..Observation::of(
                Kind::QueryShape,
                format!("shape:{site}:{db}:{}", &digest[..16]),
                person.to_string(),
            )
        });
    }
    Learned {
        found,
        used: a.tables.into_iter().collect(),
    }
}

/// Each entry as everyone may see it once it is shared, with only its
/// confirmed names; entries not shared are absent.
///
/// Know-how is confirmed name by name. A table's entry is shared once
/// `min` different people read that table, showing only the columns that
/// many people read; a query shape once every table and column in it is
/// shared; a schema correction once `min` people made it and the real name
/// it points to is shared; a file once `min` people read it after a search
/// hit, and a symbol in it once `min` people hit that symbol and its file
/// (where it is now) is shared. A pin keeps a learned entry from ageing but
/// never shows a name fewer people used; an addition by hand is shown in
/// full once pinned, and not before. Stale entries never.
fn shared_view(known: &Known, min: u32) -> HashMap<i64, Entry> {
    let live = || known.entries.iter().filter(|e| !e.stale);
    let confirmed = |e: &Entry| !e.manual && e.people >= min;
    let tables: HashSet<(&str, i64, &str)> = live()
        .filter(|e| e.kind == Kind::DataLocation && confirmed(e))
        .flat_map(|e| {
            e.tables
                .iter()
                .map(move |t| (e.site.as_str(), e.database_id, t.as_str()))
        })
        .collect();
    let files: HashSet<(&str, &str)> = live()
        .filter(|e| e.kind == Kind::CodeLocation && e.symbol.is_none() && confirmed(e))
        .filter_map(|e| Some((e.repo.as_deref()?, e.path.as_deref()?)))
        .collect();
    let table_shared = |e: &Entry| {
        e.tables
            .iter()
            .all(|t| tables.contains(&(e.site.as_str(), e.database_id, t.as_str())))
    };
    let column_shared = |e: &Entry, c: &String| {
        known
            .columns
            .get(&(e.site.clone(), e.database_id, c.clone()))
            .is_some_and(|n| *n >= min)
    };
    let columns_shared = |e: &Entry| e.columns.iter().all(|c| column_shared(e, c));
    let file_shared = |e: &Entry| {
        e.repo
            .as_deref()
            .zip(e.path.as_deref())
            .is_some_and(|f| files.contains(&f))
    };
    live()
        .filter_map(|e| {
            if e.manual {
                return e.pinned.then(|| (e.id, e.clone()));
            }
            let enough = e.people >= min;
            let shown = match e.kind {
                Kind::DataLocation if enough => {
                    let mut shown = e.clone();
                    shown.columns.retain(|c| column_shared(e, c));
                    shown
                }
                Kind::QueryShape if table_shared(e) && columns_shared(e) => e.clone(),
                Kind::SchemaCorrection if enough && table_shared(e) && columns_shared(e) => {
                    e.clone()
                }
                Kind::CodeLocation if enough && (e.symbol.is_none() || file_shared(e)) => e.clone(),
                _ => return None,
            };
            Some((e.id, shown))
        })
        .collect()
}

/// Of `entries`, those of the kinds `cfg` matches requests against.
fn of_kinds(cfg: &KnowHowConfig, entries: Vec<Entry>) -> Vec<Entry> {
    entries
        .into_iter()
        .filter(|e| cfg.kinds.contains(&e.kind))
        .collect()
}

/// The names a turn must use for a matched entry to count as a hit: its
/// tables, or the file for a code location.
fn evidence(e: &Entry) -> Vec<String> {
    match e.kind {
        Kind::CodeLocation => e
            .repo
            .as_deref()
            .zip(e.path.as_deref())
            .map(|(r, p)| vec![code::file_key(r, p)])
            .unwrap_or_default(),
        Kind::DataLocation | Kind::QueryShape | Kind::SchemaCorrection => e.tables.clone(),
    }
}

/// The key of a hand-added location: its tables and columns.
pub(crate) fn location_key(site: &str, db: i64, tables: &[String], columns: &[String]) -> String {
    format!(
        "data:{site}:{db}:{}:{}",
        tables.join(","),
        columns.join(",")
    )
}

/// The person-hash key: an HKDF subkey of the hub's vault master key.
fn person_key(config: &WorkforceConfig) -> Option<[u8; 32]> {
    let master = crate::vault::Vault::read_master_key(
        config.vault.key_file.as_deref().map(std::path::Path::new),
        std::path::Path::new(&config.hub.data_dir),
    )
    .ok()
    .flatten()?;
    let mut key = [0u8; 32];
    hkdf::Hkdf::<Sha256>::new(None, master.as_bytes())
        .expand(PERSON_KEY_INFO, &mut key)
        .ok()?;
    Some(key)
}

async fn open_store(config: &WorkforceConfig) -> Arc<dyn KnowHowStore> {
    #[cfg(feature = "postgres")]
    if let Some(pg) = config.postgres.as_ref().filter(|p| p.enabled) {
        match store::PostgresKnowHowStore::new(pg).await {
            Ok(s) => return Arc::new(s),
            Err(e) => {
                tracing::warn!(target: "know_how", error = %e, "know-how Postgres store unavailable; keeping it in memory")
            }
        }
    }
    let _ = config;
    Arc::new(InMemoryKnowHowStore::default())
}

/// The block shown with a request: names only, best match first, within
/// about `max_tokens`. A file is left out when a symbol in it is shown.
pub fn render(matched: &[(Entry, f32)], max_tokens: usize) -> String {
    let budget = max_tokens.max(100) * 4;
    let mut out = String::from(
        "[Know-how] Where this agent has found data and code for requests like this one (learned from \
         earlier questions; names only, so check them before relying on them):\n",
    );
    let with_symbol: HashSet<(&str, &str)> = matched
        .iter()
        .filter(|(e, _)| e.kind == Kind::CodeLocation && e.symbol.is_some())
        .filter_map(|(e, _)| Some((e.repo.as_deref()?, e.path.as_deref()?)))
        .collect();
    for (e, _) in matched {
        let line = match e.kind {
            Kind::DataLocation => {
                let mut l = format!(
                    "- database {}: tables {}",
                    e.database_id,
                    e.tables.join(", ")
                );
                if !e.columns.is_empty() {
                    l.push_str(&format!("; columns {}", display_columns(e).join(", ")));
                }
                if let Some(note) = &e.note {
                    l.push_str(&format!(" ({note})"));
                }
                l
            }
            Kind::QueryShape => format!(
                "- database {}: a query that worked: {}",
                e.database_id,
                e.shape.as_deref().unwrap_or_default()
            ),
            Kind::SchemaCorrection => format!(
                "- database {}: `{}` does not exist; use `{}`",
                e.database_id,
                e.wrong.as_deref().unwrap_or_default(),
                e.columns
                    .first()
                    .or(e.tables.first())
                    .map(String::as_str)
                    .unwrap_or_default()
            ),
            Kind::CodeLocation => {
                let (repo, path) = (
                    e.repo.as_deref().unwrap_or_default(),
                    e.path.as_deref().unwrap_or_default(),
                );
                match &e.symbol {
                    Some(symbol) => format!("- repo {repo}: `{symbol}` in {path}"),
                    None if with_symbol.contains(&(repo, path)) => continue,
                    None => format!("- repo {repo}: {path}"),
                }
            }
        };
        if out.len() + line.len() + 1 > budget {
            break;
        }
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Columns as shown: bare names when the entry is one table.
pub(crate) fn display_columns(e: &Entry) -> Vec<String> {
    match e.tables.as_slice() {
        [only] => {
            let prefix = format!("{only}.");
            e.columns
                .iter()
                .map(|c| c.strip_prefix(&prefix).unwrap_or(c).to_string())
                .collect()
        }
        _ => e.columns.clone(),
    }
}

#[cfg(test)]
mod tests;
