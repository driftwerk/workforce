//! DB-backed storage for workflow & playbook *definitions* (DW-550).
//!
//! Definitions previously lived as `<id>.toml` files on disk, hot-reloaded by
//! an `fs_watcher`. This store moves them into the `workflow_defs` table
//! (telemetry schema) so they survive across hosts and can be edited
//! out-of-band. Workflow *instances* are unchanged (already in
//! `workflow_instances`).
//!
//! Definitions are global today — the runtime keys them by `id` alone — so we
//! write every row under a single sentinel [`GLOBAL_AGENT`] `agent_id` and
//! load all rows keyed by `id`. The `agent_id` column is retained for future
//! per-agent scoping but is not interpreted yet.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use chrono::{DateTime, Utc};
use eyre::{Context, Result};
use tracing::warn;

use workforce_common::postgres::{PostgresConfig, PostgresError, PostgresPool};
use workforce_core::access::{
    access_with, AccessLevel, Grant, GrantLevel, OutgoingAccess, PrincipalKind, TeamMembership,
    Visibility,
};

/// Sentinel owner for global (non-agent-scoped) definitions.
pub const GLOBAL_AGENT: &str = "_global";

/// Owner sentinel for operator/disk-seeded definitions that have no member
/// owner. Stored in `owner_identity_id` (NOT NULL) and surfaced as `None`.
pub const SYSTEM_OWNER: &str = "__system__";

/// `edited_by` for edits the hub makes on its own behalf (e.g. a sweep
/// auto-disable). Never a share grantee, so it does not taint auto-runs.
pub const SYSTEM_EDITOR: &str = "system";

/// `def_revisions.kind` for agent identity files (`ref_id` =
/// `{agent_id}/{filename}`); the operative file stays on disk.
pub const KIND_IDENTITY_FILE: &str = "identity_file";
/// `def_revisions.kind` for the global config overlay (`ref_id` = `global`);
/// the operative file stays on disk (`workforce.overlay.toml`).
pub const KIND_CONFIG_OVERLAY: &str = "config_overlay";
/// `def_revisions.kind` for per-agent workspace `config.toml` files
/// (`ref_id` = agent id); the operative file stays on disk.
pub const KIND_AGENT_CONFIG: &str = "agent_config";
/// `workflow_defs.kind` / `def_revisions.kind` for channel monitors — owned,
/// runtime-authored definitions that durably watch a channel and dispatch an
/// action. Stored and revisioned exactly like workflows and playbooks.
pub const KIND_MONITOR: &str = "monitor";
/// The remaining owned-definition kinds, centralised alongside [`KIND_MONITOR`]
/// so the sharing layer can switch on a single source of truth.
pub const KIND_WORKFLOW: &str = "workflow";
pub const KIND_PLAYBOOK: &str = "playbook";
pub const KIND_CONSOLIDATION_TEMPLATE: &str = "consolidation_template";

/// Newest revisions kept per `(kind, ref_id, owner)` when no explicit
/// retention is configured. Tombstones are always kept.
pub const DEFAULT_MAX_REVISIONS: u32 = 100;

/// Who/where for a definition write, recorded in `def_revisions`.
#[derive(Debug, Clone, Copy)]
pub struct EditContext<'a> {
    /// Authenticated identity that made the change (`None` = unattributed).
    pub edited_by: Option<&'a str>,
    /// Channel: `web` | `slack` | `cli` | `agent_tool` | `restore` | `oob` | `seed`.
    pub edited_via: &'a str,
    /// Source revision number when `edited_via == "restore"`.
    pub restored_from: Option<i32>,
}

impl<'a> EditContext<'a> {
    pub fn new(edited_via: &'a str, edited_by: Option<&'a str>) -> Self {
        Self {
            edited_by,
            edited_via,
            restored_from: None,
        }
    }

    /// A restore edit: inserts a NEW revision (history is never rewritten)
    /// whose content equals revision `from`.
    pub fn restore(edited_by: Option<&'a str>, from: i32) -> Self {
        Self {
            edited_by,
            edited_via: "restore",
            restored_from: Some(from),
        }
    }
}

/// One row of revision history. `content` is populated by
/// [`WorkflowStore::get_revision`] and omitted by the list query.
#[derive(Debug, Clone, serde::Serialize)]
pub struct DefRevision {
    pub rev: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    pub content_hash: String,
    pub deleted: bool,
    pub edited_by: Option<String>,
    pub edited_via: String,
    pub restored_from: Option<i32>,
    pub created_at: DateTime<Utc>,
}

/// BLAKE3 hex of revision content (matches identity-file integrity hashing).
pub fn content_hash(content: &str) -> String {
    blake3::hash(content.as_bytes()).to_hex().to_string()
}

fn row_to_revision(r: &tokio_postgres::Row, with_content: bool) -> DefRevision {
    DefRevision {
        rev: r.get("rev"),
        content: with_content.then(|| r.get("content")),
        content_hash: r.get("content_hash"),
        deleted: r.get("deleted"),
        edited_by: r.get("edited_by"),
        edited_via: r.get("edited_via"),
        restored_from: r.get("restored_from"),
        created_at: r.get("created_at"),
    }
}

/// Insert a revision row (deduping byte-identical saves) and prune old
/// non-tombstone rows past `max_revisions` (0 = unlimited). Generic over
/// client, but must run inside a transaction: concurrent saves of one ref
/// are serialised on a transaction-scoped advisory lock, so two writers
/// never both read the same newest `rev` and collide on the next one.
/// Returns the new revision number, or `None` when deduped.
async fn record_revision_in<C>(
    client: &C,
    kind: &str,
    ref_id: &str,
    owner: Option<&str>,
    content: &str,
    deleted: bool,
    edit: EditContext<'_>,
    detail: Option<&str>,
    max_revisions: u32,
) -> Result<Option<i32>>
where
    C: deadpool_postgres::GenericClient,
{
    let owner_col = owner_column(owner);
    client
        .execute(
            "SELECT pg_advisory_xact_lock(hashtextextended($1 || chr(31) || $2 || chr(31) || $3, 0))",
            &[&kind, &ref_id, &owner_col],
        )
        .await?;
    let latest = client
        .query_opt(
            "SELECT rev, content_hash, deleted FROM def_revisions \
             WHERE kind = $1 AND ref_id = $2 AND owner_identity_id = $3 \
             ORDER BY rev DESC LIMIT 1",
            &[&kind, &ref_id, &owner_col],
        )
        .await?;

    let hash = content_hash(content);
    if let Some(r) = &latest {
        let prev_hash: String = r.get("content_hash");
        let prev_deleted: bool = r.get("deleted");
        // Byte-identical to the newest revision → no new row. Explicit
        // restores are always recorded (the action itself is the audit).
        if prev_hash == hash && prev_deleted == deleted && edit.restored_from.is_none() {
            return Ok(None);
        }
    }

    let rev = latest.map(|r| r.get::<_, i32>("rev")).unwrap_or(0) + 1;
    client
        .execute(
            "INSERT INTO def_revisions \
             (kind, ref_id, owner_identity_id, rev, content, content_hash, deleted, \
              edited_by, edited_via, restored_from, detail, created_at) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
            &[
                &kind,
                &ref_id,
                &owner_col,
                &rev,
                &content,
                &hash,
                &deleted,
                &edit.edited_by,
                &edit.edited_via,
                &edit.restored_from,
                &detail,
                &Utc::now(),
            ],
        )
        .await?;

    if max_revisions > 0 {
        // Keep the newest `max_revisions` content rows; tombstones survive
        // retention so "who deleted it" never ages out.
        client
            .execute(
                "DELETE FROM def_revisions \
                 WHERE kind = $1 AND ref_id = $2 AND owner_identity_id = $3 \
                   AND deleted = FALSE \
                   AND rev NOT IN ( \
                     SELECT rev FROM def_revisions \
                     WHERE kind = $1 AND ref_id = $2 AND owner_identity_id = $3 \
                       AND deleted = FALSE \
                     ORDER BY rev DESC LIMIT $4)",
                &[&kind, &ref_id, &owner_col, &(max_revisions as i64)],
            )
            .await?;
    }

    Ok(Some(rev))
}

fn owner_column(owner: Option<&str>) -> &str {
    owner.unwrap_or(SYSTEM_OWNER)
}

fn owner_from_column(stored: &str) -> Option<String> {
    (stored != SYSTEM_OWNER).then(|| stored.to_string())
}

fn row_to_stored(r: &tokio_postgres::Row) -> StoredDef {
    StoredDef {
        id: r.get("id"),
        toml_source: r.get("toml_source"),
        owner: owner_from_column(r.get("owner_identity_id")),
        visibility: Visibility::parse(r.get::<_, &str>("visibility")).unwrap_or_default(),
        last_editor: r.get("last_modified_by"),
        run_as: r.get("run_as_identity_id"),
    }
}

/// A stored definition row.
#[derive(Debug, Clone)]
pub struct StoredDef {
    pub id: String,
    pub toml_source: String,
    /// `None` for operator/disk-seeded (system-owned) definitions.
    pub owner: Option<String>,
    /// The identity that last wrote this definition (`workflow_defs`.`
    /// `last_modified_by`); `None` for never-attributed/seeded rows. Used to
    /// taint-gate automated runs: a member-owned definition last edited by
    /// someone other than its run identity must not auto-fire.
    pub last_editor: Option<String>,
    /// Who reaches it beyond its owner and grantees. A value the column holds
    /// that this hub cannot read is the closed one.
    pub visibility: Visibility,
    /// Identity an automated run executes as when it is not the owner: a
    /// manage-share grantee who saved with `run_as = me`. `None` = the owner.
    pub run_as: Option<String>,
}

/// A definition the caller may reach through a share: its `(id, owner)` address
/// plus the level the caller holds. Used to widen visibility/list queries.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SharedGrant {
    pub def_id: String,
    pub owner: String,
    pub permission: GrantLevel,
}

/// The `{owner}/{id}` addressing key of a member-owned definition — the form the
/// runtimes key their in-memory maps by and the form a shared-key set holds.
pub fn member_def_key(owner: &str, id: &str) -> String {
    format!("{owner}/{id}")
}

/// One caller's standing on a definition row, from the shared rule. A row
/// nobody owns belongs to the operator rather than to everybody — unlike a
/// skill record, its visibility is what says who reaches it — so the rule is
/// asked about the system row under its own owner name.
pub fn def_access(
    owner: Option<&str>,
    visibility: Visibility,
    caller: Option<&str>,
    granted: Option<GrantLevel>,
    teams: &dyn TeamMembership,
) -> AccessLevel {
    access_with(
        owner.or(Some(SYSTEM_OWNER)),
        visibility,
        caller.filter(|c| !c.is_empty() && *c != SYSTEM_OWNER),
        granted,
        || owner.is_some_and(|owner| teams.shares_team_with(owner)),
    )
}

/// Whether a definition is visible to a caller in a listing: an admin sees all;
/// everyone else sees what [`def_access`] reaches plus anything shared to them.
/// `shared` holds the [`member_def_key`] keys granted to the caller via a share.
pub fn visible_to(
    owner: Option<&str>,
    visibility: Visibility,
    id: &str,
    caller: Option<&str>,
    is_admin: bool,
    shared: Option<&std::collections::HashSet<String>>,
    teams: &dyn TeamMembership,
) -> bool {
    is_admin
        || def_access(owner, visibility, caller, None, teams).allows(AccessLevel::Run)
        || owner.is_some_and(|o| shared.is_some_and(|set| set.contains(&member_def_key(o, id))))
}

/// What a save does to a definition's run identity.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RunAsWrite {
    /// Keep the stored value (`NULL` on a fresh row).
    Unchanged,
    /// Automated runs execute as the owner.
    Owner,
    /// Automated runs execute as this identity, which must be the saver.
    Identity(String),
}

impl RunAsWrite {
    /// `(set, value)` for the `CASE WHEN $set THEN $value ELSE stored END`
    /// column expression.
    fn column_write(&self) -> (bool, Option<&str>) {
        match self {
            Self::Unchanged => (false, None),
            Self::Owner => (true, None),
            Self::Identity(id) => (true, Some(id.as_str())),
        }
    }
}

/// Whether an automated run of a definition is *tainted*: its content was last
/// written by someone other than the identity it will execute as, so it must not
/// auto-fire until that run identity re-saves (re-approves). System/disk
/// definitions (no owner) are operator-trusted and never tainted. This is the
/// guard that keeps a share grantee's edit from auto-running as the owner on a
/// monitor fire, schedule, or webhook.
pub fn auto_run_tainted(
    owner: Option<&str>,
    last_editor: Option<&str>,
    run_identity: Option<&str>,
) -> bool {
    // `last_editor.is_some()` is load-bearing: a real foreign edit always stamps
    // the editor server-side, so an unattributed (`None`) member definition was
    // never foreign-edited and is safe to auto-run — fail-open on the unknown
    // rather than suspending legacy rows. Hub automation (`SYSTEM_EDITOR`) is
    // not a share grantee: its edits never carry foreign content.
    owner.is_some()
        && last_editor.is_some_and(|editor| editor != SYSTEM_EDITOR)
        && last_editor != run_identity
}

/// A trigger's identity in the store: what an ownership transfer moves, and
/// what a public path is minted against.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TriggerBinding {
    pub kind: String,
    pub def_id: String,
    /// `None` = system/disk definition, stored as [`SYSTEM_OWNER`].
    pub owner: Option<String>,
    pub trigger_id: String,
}

/// Marks a minted trigger path. An HTTP trigger id cannot contain `.`
/// (`is_valid_http_trigger_id`) and a minted path never contains `/`, so a
/// minted path and an owner-derived one can never collide.
pub const TRIGGER_PATH_PREFIX: &str = "t.";

/// A trigger's permanent public path. Minted once per binding and never
/// recomputed — ownership is not an input to it, which is what keeps an
/// external caller's URL working when the workflow changes hands.
pub fn mint_trigger_path() -> String {
    format!("{TRIGGER_PATH_PREFIX}{}", uuid::Uuid::new_v4().simple())
}

/// Why a definition did not change hands. A refusal is the requester's to act
/// on; a storage failure is not, so the two are told apart at the type.
#[derive(Debug)]
pub enum TransferFailed {
    /// No definition of `(kind, id)` under the owner the requester named.
    NotFound,
    /// The target already owns a definition of `(kind, id)`.
    Occupied,
    Storage(eyre::Report),
}

impl std::fmt::Display for TransferFailed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NotFound => f.write_str("no such definition under that owner"),
            Self::Occupied => f.write_str("the new owner already holds a definition of that name"),
            Self::Storage(e) => write!(f, "{e}"),
        }
    }
}

impl From<tokio_postgres::Error> for TransferFailed {
    fn from(e: tokio_postgres::Error) -> Self {
        Self::Storage(e.into())
    }
}

impl From<PostgresError> for TransferFailed {
    fn from(e: PostgresError) -> Self {
        Self::Storage(e.into())
    }
}

/// What a transfer moved, for the surfaces that have to say what changed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Transferred {
    /// Public paths of the definition's HTTP triggers, which the move leaves
    /// exactly where they were — the reason they are minted rather than
    /// derived from the owner. A surface names them beside the owner-derived
    /// address that has stopped answering.
    pub trigger_paths: Vec<String>,
    /// Grants carried across. One naming the new owner is left behind, since
    /// ownership already subsumes it.
    pub shares: u64,
    /// Revisions carried across, renumbered where the target held a series of
    /// their own.
    pub revisions: u64,
    /// Whether the outgoing owner was left a `manage` grant. What the
    /// transaction did rather than what was asked for, so a surface's note
    /// cannot contradict it.
    pub kept_manage: bool,
}

/// A monitor's runtime state, keyed `(monitor_id, owner)` in four tables that
/// therefore move with the definition.
const MONITOR_SATELLITES: [&str; 4] = [
    "monitor_dispatches",
    "monitor_cursors",
    "monitor_channel_blocks",
    "monitor_source_state",
];

const DEF_ROW_FOR_UPDATE: &str = "SELECT 1 FROM workflow_defs \
                                  WHERE id = $1 AND kind = $2 AND owner_identity_id = $3 \
                                  FOR UPDATE";

const LIST_DEFS: &str = "SELECT id, toml_source, owner_identity_id, visibility, \
                                last_modified_by, run_as_identity_id \
                         FROM workflow_defs WHERE kind = $1 ORDER BY id";

/// Aliased so one column list serves both the plain read and the join that
/// reads back a mint.
const ENDPOINT_COLUMNS: &str = "e.public_path, e.kind, e.def_id, e.owner_identity_id, e.trigger_id";

fn row_to_endpoint(r: &tokio_postgres::Row) -> (TriggerBinding, String) {
    (
        TriggerBinding {
            kind: r.get("kind"),
            def_id: r.get("def_id"),
            owner: owner_from_column(r.get("owner_identity_id")),
            trigger_id: r.get("trigger_id"),
        },
        r.get("public_path"),
    )
}

/// The four binding columns of `bindings`, as the parallel arrays both the
/// mint and its read-back `UNNEST` over.
fn binding_columns<'a>(
    bindings: &[&'a TriggerBinding],
) -> (Vec<&'a str>, Vec<&'a str>, Vec<&'a str>, Vec<&'a str>) {
    (
        bindings.iter().map(|b| b.kind.as_str()).collect(),
        bindings.iter().map(|b| b.def_id.as_str()).collect(),
        bindings
            .iter()
            .map(|b| owner_column(b.owner.as_deref()))
            .collect(),
        bindings.iter().map(|b| b.trigger_id.as_str()).collect(),
    )
}

/// Move a definition's trigger endpoints to a new owner. `public_path` is not
/// touched: an external caller's address survives the definition changing
/// hands, which is the whole reason it is minted rather than derived from the
/// owner. Returns the paths that moved, so a surface can name the addresses
/// that go on working.
async fn move_endpoints(
    tx: &tokio_postgres::Transaction<'_>,
    kind: &str,
    def_id: &str,
    from: &str,
    to: &str,
) -> Result<Vec<String>, TransferFailed> {
    let rows = tx
        .query(
            "UPDATE trigger_endpoints SET owner_identity_id = $4 \
             WHERE kind = $1 AND def_id = $2 AND owner_identity_id = $3 \
             RETURNING public_path",
            &[&kind, &def_id, &from, &to],
        )
        .await?;
    Ok(rows.iter().map(|r| r.get("public_path")).collect())
}

/// Drop the endpoint rows `owner` holds for `(kind, def_id)`. Endpoint rows
/// deliberately outlive their definition, so a definition the target once
/// owned and deleted leaves one behind that addresses nothing and would
/// otherwise collide with the move. [`WorkflowStore::transfer`] has already
/// proved the target holds no such definition, which is what makes every row
/// this finds an orphan rather than a live address.
async fn purge_orphan_endpoints(
    tx: &tokio_postgres::Transaction<'_>,
    kind: &str,
    def_id: &str,
    owner: &str,
) -> Result<u64, TransferFailed> {
    Ok(tx
        .execute(
            "DELETE FROM trigger_endpoints \
             WHERE kind = $1 AND def_id = $2 AND owner_identity_id = $3",
            &[&kind, &def_id, &owner],
        )
        .await?)
}

/// Leave `outgoing` a `manage` grant on the row now owned by `new_owner`, in
/// the caller's transaction — so a transfer that rolls back leaves no grant
/// behind, and a grant never lands ahead of a handover that then fails.
///
/// It runs **after** the `def_shares` move, and that order is load-bearing.
/// The move carries across every grant except a `user` one naming the new
/// owner, so a self-grant the outgoing owner already held — inert while they
/// owned the row, storable before the rule that now refuses one — arrives in
/// the new owner's namespace on this very key. Upserting after it meets that
/// row and raises it to `manage`; inserting before it would leave the move
/// pushing the old row onto a primary key this had just taken. A *team* grant
/// that happens to contain the outgoing owner is a different principal type, so
/// it neither collides nor is disturbed: it goes on reaching the whole team.
async fn keep_manage_grant(
    tx: &tokio_postgres::Transaction<'_>,
    kind: &str,
    def_id: &str,
    new_owner: &str,
    outgoing: &str,
    granted_by: &str,
) -> Result<(), TransferFailed> {
    tx.execute(
        "INSERT INTO def_shares \
         (kind, def_id, owner_identity_id, principal_type, principal_id, \
          permission, granted_by, granted_at) \
         VALUES ($1, $2, $3, 'user', $4, 'manage', $5, $6) \
         ON CONFLICT (kind, def_id, owner_identity_id, principal_type, principal_id) \
         DO UPDATE SET permission = 'manage', granted_by = EXCLUDED.granted_by, \
                       granted_at = EXCLUDED.granted_at",
        &[
            &kind,
            &def_id,
            &new_owner,
            &outgoing,
            &granted_by,
            &Utc::now(),
        ],
    )
    .await?;
    Ok(())
}

/// Postgres-backed workflow/playbook definition store.
pub struct WorkflowStore {
    pool: PostgresPool,
    /// Newest revisions kept per `(kind, ref_id, owner)`; 0 = unlimited.
    /// Tombstones always survive retention.
    max_revisions: u32,
}

impl WorkflowStore {
    /// Telemetry-pinned pool (the `workflow_defs` table lives in that schema).
    pub fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
        workforce_common::telemetry_postgres::PostgresTelemetryStore::build_pool(config)
    }

    /// Ensure the tables exist, then return a ready store.
    pub async fn new(pool: PostgresPool) -> Result<Arc<Self>> {
        Self::with_retention(pool, DEFAULT_MAX_REVISIONS).await
    }

    /// [`Self::new`] with an explicit revision-retention cap
    /// (`[revisions] max_per_def`; 0 = unlimited).
    pub async fn with_retention(pool: PostgresPool, max_revisions: u32) -> Result<Arc<Self>> {
        workforce_common::telemetry_postgres::run_telemetry_migrations(&pool)
            .await
            .context("ensuring workflow_defs table exists")?;
        Ok(Arc::new(Self {
            pool,
            max_revisions,
        }))
    }

    /// The store as an offline reader opens it, over a read-only pool
    /// (`PostgresTelemetryStore::read_only_pool`): no migration is run.
    pub fn read_only(pool: PostgresPool) -> Arc<Self> {
        Arc::new(Self {
            pool,
            max_revisions: 0,
        })
    }

    /// The content of the revision of `kind` whose hash is `content_hash`,
    /// while it is still kept.
    pub async fn revision_content(&self, kind: &str, content_hash: &str) -> Result<Option<String>> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                "SELECT content FROM def_revisions \
                 WHERE kind = $1 AND content_hash = $2 AND NOT deleted LIMIT 1",
                &[&kind, &content_hash],
            )
            .await?;
        Ok(row.map(|r| r.get("content")))
    }

    /// List all definitions of `kind` (`"workflow"` | `"playbook"`) across all
    /// owners. The runtime keys them by `(owner, id)`.
    pub async fn list(&self, kind: &str) -> Result<Vec<StoredDef>> {
        let conn = self.pool.get().await?;
        let rows = conn.query(LIST_DEFS, &[&kind]).await?;
        Ok(rows.iter().map(row_to_stored).collect())
    }

    /// [`Self::list`] together with the trigger endpoints already minted for
    /// that kind, read in one repeatable-read snapshot.
    ///
    /// The snapshot is the whole point of reading them together. At `READ
    /// COMMITTED` each statement takes its own snapshot, so an ownership
    /// transfer committing between the two reads would leave the definitions
    /// naming the old owner while the endpoint row had already moved to the
    /// new one; [`Self::ensure_endpoints`] would then see a live binding as
    /// unminted and mint a *second* path for a URL that external callers are
    /// already using. Both statements here are reads, which can never raise a
    /// serialisation failure — the minting that can is deliberately outside.
    ///
    /// An endpoint read that fails (a replica that has not run the migration
    /// yet) degrades to an empty overlay instead of failing the definition
    /// read: the owner-derived legacy paths are built from the definitions
    /// alone and must keep resolving whatever the endpoint table is doing.
    pub async fn list_with_endpoints(
        &self,
        kind: &str,
    ) -> Result<(Vec<StoredDef>, HashMap<TriggerBinding, String>)> {
        let mut conn = self.pool.get().await?;
        let tx = conn
            .build_transaction()
            .isolation_level(tokio_postgres::IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await?;
        let defs = tx.query(LIST_DEFS, &[&kind]).await?;
        let endpoints = tx
            .query(
                &format!("SELECT {ENDPOINT_COLUMNS} FROM trigger_endpoints e WHERE e.kind = $1"),
                &[&kind],
            )
            .await;
        let defs = defs.iter().map(row_to_stored).collect();
        match endpoints {
            Ok(rows) => {
                tx.commit().await?;
                Ok((defs, rows.iter().map(row_to_endpoint).collect()))
            }
            Err(e) => {
                warn!(error = %e, kind, "Reading trigger endpoints failed — legacy trigger paths only");
                Ok((defs, HashMap::new()))
            }
        }
    }

    /// The public path of every binding in `bindings`, minting one for each
    /// binding `known` (from [`Self::list_with_endpoints`]) does not already
    /// carry.
    ///
    /// Insert-only: it never deletes and never updates, so a definition that
    /// is transiently unparseable — or a partial load during a bad deploy —
    /// cannot take a live URL off the air, and recreating a deleted definition
    /// under the same owner revives its path.
    ///
    /// Issued at the pool's default `READ COMMITTED`. Do **not** wrap this in
    /// `REPEATABLE READ` or stricter: two replicas reloading the same new
    /// binding concurrently is the normal case, and under `REPEATABLE READ`
    /// the second one's insert fails with `could not serialize access due to
    /// concurrent update` rather than yielding to the first one's path.
    pub async fn ensure_endpoints(
        &self,
        known: &HashMap<TriggerBinding, String>,
        bindings: &HashSet<TriggerBinding>,
    ) -> Result<HashMap<TriggerBinding, String>> {
        let found: HashMap<TriggerBinding, String> = bindings
            .iter()
            .filter_map(|b| known.get(b).map(|path| (b.clone(), path.clone())))
            .collect();
        let missing: Vec<&TriggerBinding> = bindings
            .iter()
            .filter(|b| !found.contains_key(*b))
            .collect();
        if missing.is_empty() {
            return Ok(found);
        }

        let (kinds, def_ids, owners, trigger_ids) = binding_columns(&missing);
        let paths: Vec<String> = missing.iter().map(|_| mint_trigger_path()).collect();
        let conn = self.pool.get().await?;
        conn.execute(
            "INSERT INTO trigger_endpoints \
             (public_path, kind, def_id, owner_identity_id, trigger_id) \
             SELECT * FROM UNNEST($1::text[], $2::text[], $3::text[], $4::text[], $5::text[]) \
             ON CONFLICT ON CONSTRAINT trigger_endpoints_binding_key DO NOTHING",
            &[&paths, &kinds, &def_ids, &owners, &trigger_ids],
        )
        .await?;
        // Read back rather than trusting the mint: a replica that lost the
        // race inserted nothing and must adopt the winner's path.
        let rows = conn
            .query(
                &format!(
                    "SELECT {ENDPOINT_COLUMNS} FROM trigger_endpoints e \
                     JOIN UNNEST($1::text[], $2::text[], $3::text[], $4::text[]) \
                          AS m(kind, def_id, owner_identity_id, trigger_id) \
                       ON e.kind = m.kind AND e.def_id = m.def_id \
                      AND e.owner_identity_id = m.owner_identity_id \
                      AND e.trigger_id = m.trigger_id"
                ),
                &[&kinds, &def_ids, &owners, &trigger_ids],
            )
            .await?;

        let resolved: HashMap<TriggerBinding, String> = found
            .into_iter()
            .chain(rows.iter().map(row_to_endpoint))
            .collect();
        match missing.into_iter().find(|b| !resolved.contains_key(*b)) {
            Some(b) => Err(eyre::eyre!(
                "trigger endpoint for {} '{}' trigger '{}' is absent after minting it",
                b.kind,
                b.def_id,
                b.trigger_id
            )),
            None => Ok(resolved),
        }
    }

    /// Get the raw TOML for one definition owned by `owner` (`None` = system).
    pub async fn get(&self, id: &str, kind: &str, owner: Option<&str>) -> Result<Option<String>> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                "SELECT toml_source FROM workflow_defs \
                 WHERE id = $1 AND kind = $2 AND owner_identity_id = $3",
                &[&id, &kind, &owner_column(owner)],
            )
            .await?;
        Ok(row.map(|r| r.get("toml_source")))
    }

    /// Insert or update a definition owned by `owner` (`None` = system),
    /// recording a revision (and `last_modified_by`) in the same transaction.
    pub async fn save(
        &self,
        id: &str,
        kind: &str,
        toml_source: &str,
        parsed_json: Option<serde_json::Value>,
        owner: Option<&str>,
        visibility: Visibility,
        run_as: RunAsWrite,
        edit: EditContext<'_>,
    ) -> Result<()> {
        let now = Utc::now();
        let (set_run_as, run_as_value) = run_as.column_write();
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        tx.execute(
            "INSERT INTO workflow_defs \
             (id, agent_id, kind, toml_source, parsed_json, owner_identity_id, visibility, \
              last_modified_by, created_at, updated_at, run_as_identity_id) \
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $9, $11) \
             ON CONFLICT (id, kind, owner_identity_id) DO UPDATE \
             SET kind = EXCLUDED.kind, toml_source = EXCLUDED.toml_source, \
                 parsed_json = EXCLUDED.parsed_json, visibility = EXCLUDED.visibility, \
                 last_modified_by = EXCLUDED.last_modified_by, \
                 updated_at = EXCLUDED.updated_at, \
                 run_as_identity_id = CASE WHEN $10 THEN $11 \
                                      ELSE workflow_defs.run_as_identity_id END",
            &[
                &id,
                &GLOBAL_AGENT,
                &kind,
                &toml_source,
                &parsed_json,
                &owner_column(owner),
                &visibility.as_str(),
                &edit.edited_by,
                &now,
                &set_run_as,
                &run_as_value,
            ],
        )
        .await?;
        record_revision_in(
            &tx,
            kind,
            id,
            owner,
            toml_source,
            false,
            edit,
            None,
            self.max_revisions,
        )
        .await?;
        tx.commit().await?;
        Ok(())
    }

    /// Seed a system-owned, globally-visible definition only if absent
    /// (idempotent disk→DB migration). Returns `true` if a row was inserted.
    /// A seeded definition gets a baseline revision (`edited_via = "seed"`).
    ///
    /// A definition whose newest revision is a delete tombstone is NOT
    /// re-seeded — an operator's deletion survives restarts even while the
    /// source TOML remains on disk. Restoring a revision revives it.
    pub async fn seed(&self, id: &str, kind: &str, toml_source: &str) -> Result<bool> {
        let now = Utc::now();
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        let tombstoned = tx
            .query_opt(
                "SELECT deleted FROM def_revisions \
                 WHERE kind = $1 AND ref_id = $2 AND owner_identity_id = $3 \
                 ORDER BY rev DESC LIMIT 1",
                &[&kind, &id, &SYSTEM_OWNER],
            )
            .await?
            .map(|r| r.get::<_, bool>("deleted"))
            .unwrap_or(false);
        if tombstoned {
            tx.commit().await?;
            return Ok(false);
        }
        let n = tx
            .execute(
                "INSERT INTO workflow_defs \
                 (id, agent_id, kind, toml_source, owner_identity_id, visibility, \
                  created_at, updated_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $7) \
                 ON CONFLICT (id, kind, owner_identity_id) DO NOTHING",
                &[
                    &id,
                    &GLOBAL_AGENT,
                    &kind,
                    &toml_source,
                    &SYSTEM_OWNER,
                    &Visibility::Global.as_str(),
                    &now,
                ],
            )
            .await?;
        if n > 0 {
            record_revision_in(
                &tx,
                kind,
                id,
                None,
                toml_source,
                false,
                EditContext::new("seed", None),
                None,
                self.max_revisions,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(n > 0)
    }

    /// Delete a definition owned by `owner` (`None` = system). When a row was
    /// actually removed, a tombstone revision records who deleted it.
    pub async fn delete(
        &self,
        id: &str,
        kind: &str,
        owner: Option<&str>,
        edit: EditContext<'_>,
    ) -> Result<()> {
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        let n = tx
            .execute(
                "DELETE FROM workflow_defs WHERE id = $1 AND kind = $2 AND owner_identity_id = $3",
                &[&id, &kind, &owner_column(owner)],
            )
            .await?;
        if n > 0 {
            record_revision_in(
                &tx,
                kind,
                id,
                owner,
                "",
                true,
                edit,
                None,
                self.max_revisions,
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    // ── Revision history ──────────────────────────────────────────

    /// Record a revision for a non-def kind (identity files, config overlay)
    /// whose operative copy lives outside this store. Returns the revision
    /// number, or `None` when byte-identical to the newest (deduped).
    #[allow(clippy::too_many_arguments)]
    pub async fn record_revision(
        &self,
        kind: &str,
        ref_id: &str,
        owner: Option<&str>,
        content: &str,
        deleted: bool,
        edit: EditContext<'_>,
        detail: Option<&str>,
    ) -> Result<Option<i32>> {
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        let rev = record_revision_in(
            &tx,
            kind,
            ref_id,
            owner,
            content,
            deleted,
            edit,
            detail,
            self.max_revisions,
        )
        .await?;
        tx.commit().await?;
        Ok(rev)
    }

    /// Revision metadata, newest first (content omitted — fetch one via
    /// [`Self::get_revision`] for the diff view).
    pub async fn list_revisions(
        &self,
        kind: &str,
        ref_id: &str,
        owner: Option<&str>,
    ) -> Result<Vec<DefRevision>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT rev, content_hash, deleted, edited_by, edited_via, restored_from, \
                        created_at \
                 FROM def_revisions \
                 WHERE kind = $1 AND ref_id = $2 AND owner_identity_id = $3 \
                 ORDER BY rev DESC",
                &[&kind, &ref_id, &owner_column(owner)],
            )
            .await?;
        Ok(rows.iter().map(|r| row_to_revision(r, false)).collect())
    }

    /// One revision with its full content.
    pub async fn get_revision(
        &self,
        kind: &str,
        ref_id: &str,
        owner: Option<&str>,
        rev: i32,
    ) -> Result<Option<DefRevision>> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                "SELECT rev, content, content_hash, deleted, edited_by, edited_via, \
                        restored_from, created_at \
                 FROM def_revisions \
                 WHERE kind = $1 AND ref_id = $2 AND owner_identity_id = $3 AND rev = $4",
                &[&kind, &ref_id, &owner_column(owner), &rev],
            )
            .await?;
        Ok(row.map(|r| row_to_revision(&r, true)))
    }

    /// `content_hash` of the newest revision (used by the identity-file
    /// out-of-band change detector).
    pub async fn latest_revision_hash(
        &self,
        kind: &str,
        ref_id: &str,
        owner: Option<&str>,
    ) -> Result<Option<String>> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_opt(
                "SELECT content_hash FROM def_revisions \
                 WHERE kind = $1 AND ref_id = $2 AND owner_identity_id = $3 \
                 ORDER BY rev DESC LIMIT 1",
                &[&kind, &ref_id, &owner_column(owner)],
            )
            .await?;
        Ok(row.map(|r| r.get("content_hash")))
    }

    /// What every replica's definition poll compares: the newest edit and how
    /// many rows there are, so a deletion that leaves the newest row in
    /// place is noticed too.
    pub async fn change_marker(&self, kind: &str) -> Result<(Option<DateTime<Utc>>, i64)> {
        let conn = self.pool.get().await?;
        let row = conn
            .query_one(
                "SELECT max(updated_at), count(*) FROM workflow_defs WHERE kind = $1",
                &[&kind],
            )
            .await?;
        Ok((row.get(0), row.get(1)))
    }

    // ── Identity lifecycle ────────────────────────────────────────

    /// Delete a member's definition with its whole history: every revision,
    /// grant, trigger endpoint and monitor satellite. Unlike [`Self::delete`]
    /// no tombstone is kept, because the content of earlier revisions is the
    /// person's own. `false` when there was nothing at that address.
    pub async fn erase(&self, kind: &str, id: &str, owner: &str) -> Result<bool> {
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        for table in MONITOR_SATELLITES.iter().filter(|_| kind == KIND_MONITOR) {
            tx.execute(
                &format!("DELETE FROM {table} WHERE monitor_id = $1 AND owner = $2"),
                &[&id, &owner],
            )
            .await?;
        }
        tx.execute(
            "DELETE FROM trigger_endpoints \
             WHERE kind = $1 AND def_id = $2 AND owner_identity_id = $3",
            &[&kind, &id, &owner],
        )
        .await?;
        tx.execute(
            "DELETE FROM def_revisions \
             WHERE kind = $1 AND ref_id = $2 AND owner_identity_id = $3",
            &[&kind, &id, &owner],
        )
        .await?;
        let removed = tx
            .execute(
                "DELETE FROM workflow_defs WHERE id = $1 AND kind = $2 AND owner_identity_id = $3",
                &[&id, &kind, &owner],
            )
            .await?;
        tx.commit().await?;
        Ok(removed > 0)
    }

    /// After a merge moved a definition with [`Self::transfer`] (which clears
    /// its run identity): the same person owns it under another identity, so
    /// it keeps the run identity it had and counts as saved by its owner.
    pub async fn adopt_merged(
        &self,
        kind: &str,
        id: &str,
        owner: &str,
        run_as: Option<&str>,
    ) -> Result<bool> {
        let conn = self.pool.get().await?;
        Ok(conn
            .execute(
                "UPDATE workflow_defs \
                 SET run_as_identity_id = $4, last_modified_by = $3, updated_at = $5 \
                 WHERE id = $1 AND kind = $2 AND owner_identity_id = $3",
                &[&id, &kind, &owner, &run_as, &Utc::now()],
            )
            .await?
            > 0)
    }

    /// Definitions owned by others that name `identity`: as their run
    /// identity, their last editor, or a user grantee. `(kind, id, owner)`.
    pub async fn references_to(&self, identity: &str) -> Result<Vec<(String, String, String)>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT kind, id, owner_identity_id FROM workflow_defs \
                 WHERE owner_identity_id <> $1 \
                   AND (run_as_identity_id = $1 OR last_modified_by = $1) \
                 UNION \
                 SELECT kind, def_id, owner_identity_id FROM def_shares \
                 WHERE principal_type = 'user' AND principal_id = $1 \
                 ORDER BY 1, 2, 3",
                &[&identity],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| (r.get(0), r.get(1), r.get(2)))
            .collect())
    }

    /// For each person, how many definitions others own run as them and how
    /// many grants name them: what [`Self::drop_references`] removes.
    pub async fn reference_holders(&self) -> Result<std::collections::BTreeMap<String, usize>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT run_as_identity_id, COUNT(*) FROM workflow_defs \
                 WHERE run_as_identity_id IS NOT NULL AND owner_identity_id <> run_as_identity_id \
                 GROUP BY run_as_identity_id \
                 UNION ALL \
                 SELECT principal_id, COUNT(*) FROM def_shares \
                 WHERE principal_type = 'user' GROUP BY principal_id",
                &[],
            )
            .await?;
        Ok(rows
            .iter()
            .fold(std::collections::BTreeMap::new(), |mut held, row| {
                *held.entry(row.get::<_, String>(0)).or_default() +=
                    usize::try_from(row.get::<_, i64>(1)).unwrap_or_default();
                held
            }))
    }

    /// One person's references on definitions others own become `to`'s
    /// (a merge): their run identity, last edit and grants. Where `to`
    /// already holds a grant on the same definition the stronger one is
    /// kept, and a grant on a definition `to` owns goes, since ownership
    /// subsumes it. Returns the rows changed.
    pub async fn reassign_references(&self, from: &str, to: &str) -> Result<u64> {
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        let defs = tx
            .execute(
                "UPDATE workflow_defs SET \
                   run_as_identity_id = CASE WHEN run_as_identity_id = $1 THEN $2 \
                                             ELSE run_as_identity_id END, \
                   last_modified_by = CASE WHEN last_modified_by = $1 THEN $2 \
                                           ELSE last_modified_by END, \
                   updated_at = $3 \
                 WHERE owner_identity_id <> $1 \
                   AND (run_as_identity_id = $1 OR last_modified_by = $1)",
                &[&from, &to, &Utc::now()],
            )
            .await?;
        let mut grants = 0;
        for sql in [
            "UPDATE def_shares b SET permission = 'manage' FROM def_shares a \
             WHERE a.principal_type = 'user' AND a.principal_id = $1 AND a.permission = 'manage' \
               AND b.principal_type = 'user' AND b.principal_id = $2 \
               AND b.kind = a.kind AND b.def_id = a.def_id \
               AND b.owner_identity_id = a.owner_identity_id",
            "DELETE FROM def_shares a \
             WHERE a.principal_type = 'user' AND a.principal_id = $1 \
               AND (a.owner_identity_id = $2 OR EXISTS ( \
                   SELECT 1 FROM def_shares b \
                   WHERE b.principal_type = 'user' AND b.principal_id = $2 \
                     AND b.kind = a.kind AND b.def_id = a.def_id \
                     AND b.owner_identity_id = a.owner_identity_id))",
            "UPDATE def_shares SET principal_id = $2 \
             WHERE principal_type = 'user' AND principal_id = $1",
        ] {
            grants += tx.execute(sql, &[&from, &to]).await?;
        }
        tx.commit().await?;
        Ok(defs + grants)
    }

    /// One person's references on definitions others own go (a delete):
    /// their grants, and their run identity, so such a definition runs as
    /// its owner. It is left blocked as tainted until the owner saves it,
    /// because the person stays its last editor. Returns the rows changed.
    pub async fn drop_references(&self, identity: &str) -> Result<u64> {
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        let defs = tx
            .execute(
                "UPDATE workflow_defs SET run_as_identity_id = NULL, updated_at = $2 \
                 WHERE owner_identity_id <> $1 AND run_as_identity_id = $1",
                &[&identity, &Utc::now()],
            )
            .await?;
        let grants = tx
            .execute(
                "DELETE FROM def_shares WHERE principal_type = 'user' AND principal_id = $1",
                &[&identity],
            )
            .await?;
        tx.commit().await?;
        Ok(defs + grants)
    }

    // ── Definition sharing ────────────────────────────────────────

    /// Change who else may reach one definition, leaving its content, its
    /// editor and its run identity alone. Returns whether a row changed.
    ///
    /// Visibility is not content, so this is deliberately not a [`Self::save`]:
    /// a save re-validates a definition that may predate a tightened authoring
    /// rule, and stamps `last_modified_by` — which pauses automated runs
    /// through [`auto_run_tainted`]. `updated_at` still moves, so the other
    /// replicas' definition polls pick the change up.
    pub async fn set_visibility(
        &self,
        kind: &str,
        id: &str,
        owner: &str,
        visibility: Visibility,
    ) -> Result<bool> {
        let conn = self.pool.get().await?;
        let n = conn
            .execute(
                "UPDATE workflow_defs SET visibility = $4, updated_at = $5 \
                 WHERE kind = $1 AND id = $2 AND owner_identity_id = $3",
                &[&kind, &id, &owner, &visibility.as_str(), &Utc::now()],
            )
            .await?;
        Ok(n > 0)
    }

    /// Every grantee of one definition, ordered for a stable UI.
    pub async fn list_shares(&self, kind: &str, def_id: &str, owner: &str) -> Result<Vec<Grant>> {
        let conn = self.pool.get().await?;
        let rows = conn
            .query(
                "SELECT principal_type, principal_id, permission FROM def_shares \
                 WHERE kind = $1 AND def_id = $2 AND owner_identity_id = $3 \
                 ORDER BY principal_type, principal_id",
                &[&kind, &def_id, &owner],
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                Some(Grant {
                    principal_type: PrincipalKind::parse(r.get::<_, &str>("principal_type"))?,
                    principal_id: r.get("principal_id"),
                    permission: GrantLevel::parse(r.get::<_, &str>("permission"))?,
                })
            })
            .collect())
    }

    /// Replace the full grantee set of one definition (the UI multiselect is the
    /// authoritative set), deduplicated by principal keeping the highest
    /// permission, in one transaction.
    pub async fn set_shares(
        &self,
        kind: &str,
        def_id: &str,
        owner: &str,
        shares: &[Grant],
        granted_by: &str,
    ) -> Result<()> {
        let mut deduped: HashMap<(PrincipalKind, String), GrantLevel> = HashMap::new();
        for s in shares {
            let entry = deduped
                .entry((s.principal_type, s.principal_id.clone()))
                .or_insert(s.permission);
            *entry = (*entry).max(s.permission);
        }

        let now = Utc::now();
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        tx.execute(
            "DELETE FROM def_shares WHERE kind = $1 AND def_id = $2 AND owner_identity_id = $3",
            &[&kind, &def_id, &owner],
        )
        .await?;
        for ((principal_type, principal_id), permission) in &deduped {
            let principal_type = principal_type.as_str();
            let perm = permission.as_str();
            tx.execute(
                "INSERT INTO def_shares \
                 (kind, def_id, owner_identity_id, principal_type, principal_id, \
                  permission, granted_by, granted_at) \
                 VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
                &[
                    &kind,
                    &def_id,
                    &owner,
                    &principal_type,
                    principal_id,
                    &perm,
                    &granted_by,
                    &now,
                ],
            )
            .await?;
        }
        tx.commit().await?;
        Ok(())
    }

    /// The highest permission the caller (their identity and each of their
    /// teams) holds on one definition through a share, if any.
    pub async fn caller_share_permission(
        &self,
        kind: &str,
        def_id: &str,
        owner: &str,
        identity: Option<&str>,
        team_ids: &[String],
    ) -> Result<Option<GrantLevel>> {
        let conn = self.pool.get().await?;
        let teams = team_ids.to_vec();
        let rows = conn
            .query(
                "SELECT permission FROM def_shares \
                 WHERE kind = $1 AND def_id = $2 AND owner_identity_id = $3 \
                   AND ( (principal_type = 'user' AND principal_id = $4) \
                      OR (principal_type = 'team' AND principal_id = ANY($5)) )",
                &[&kind, &def_id, &owner, &identity.unwrap_or(""), &teams],
            )
            .await?;
        Ok(rows
            .iter()
            .filter_map(|r| GrantLevel::parse(r.get::<_, &str>("permission")))
            .max())
    }

    /// Every definition of `kind` shared to the caller (identity + teams),
    /// deduplicated by `(id, owner)` keeping the highest permission.
    pub async fn shares_for_caller(
        &self,
        kind: &str,
        identity: Option<&str>,
        team_ids: &[String],
    ) -> Result<Vec<SharedGrant>> {
        let conn = self.pool.get().await?;
        let teams = team_ids.to_vec();
        let rows = conn
            .query(
                "SELECT def_id, owner_identity_id, permission FROM def_shares \
                 WHERE kind = $1 \
                   AND ( (principal_type = 'user' AND principal_id = $2) \
                      OR (principal_type = 'team' AND principal_id = ANY($3)) )",
                &[&kind, &identity.unwrap_or(""), &teams],
            )
            .await?;
        let mut by_def: HashMap<(String, String), GrantLevel> = HashMap::new();
        for r in &rows {
            let Some(perm) = GrantLevel::parse(r.get::<_, &str>("permission")) else {
                continue;
            };
            let key = (
                r.get::<_, String>("def_id"),
                r.get::<_, String>("owner_identity_id"),
            );
            let entry = by_def.entry(key).or_insert(perm);
            *entry = (*entry).max(perm);
        }
        Ok(by_def
            .into_iter()
            .map(|((def_id, owner), permission)| SharedGrant {
                def_id,
                owner,
                permission,
            })
            .collect())
    }

    // ── Ownership transfer ────────────────────────────────────────

    /// Hand one definition, and everything keyed to `(id, kind, owner)`, from
    /// `from` to `to` in one transaction. Both are members: the system row is
    /// not transferable, and becomes a member's by a save rather than by this.
    ///
    /// It is a row move rather than an `UPDATE` of the owner column because
    /// `def_shares` carries a composite foreign key to `workflow_defs` with
    /// `ON DELETE CASCADE` and no `ON UPDATE`, so Postgres refuses the parent
    /// update *and* the child-first one. Insert the new parent, move the
    /// children onto it, delete the old parent — whose cascade then reaches
    /// nothing.
    ///
    /// The new row is built from the old one rather than from a column list,
    /// so a column added later travels with a transfer instead of silently
    /// reverting to its default. Three are overridden: the owner; `updated_at`,
    /// because every replica rebuilds its definition map only when
    /// [`Self::change_marker`] moves, and a transfer that left it alone would
    /// leave the other replicas running the old owner's key as the old owner;
    /// and `run_as_identity_id`, which is cleared so a transferred definition
    /// blocks as *tainted* — naming the save that resumes it — rather than as
    /// *inactive*, which names no action at all when the identity it ran as is
    /// the departed owner.
    ///
    /// Seven tables move: `workflow_defs`, `def_shares`, `def_revisions`,
    /// `trigger_endpoints`, and for a monitor the three satellites of
    /// [`MONITOR_SATELLITES`]. Four are deliberately left where they are.
    /// `monitor_channel_cursors` is keyed `(surface, channel)`, with neither
    /// monitor nor owner in the key. `workflow_instances` records runs that
    /// happened under the ownership that then applied, so rewriting them would
    /// falsify the record — the new owner's run history begins at the
    /// transfer. `workflow_leases.owner_instance` is a hub instance holding an
    /// execution lease, not a member. And an artifact's `owner_id` is whoever
    /// produced it, not a reference to a definition.
    ///
    /// Where the target once owned `(kind, id)` and deleted it, two kinds of
    /// leftover collide with the move, and they are answered oppositely on
    /// purpose: **revisions are history to preserve**, so the moved series is
    /// renumbered above the target's own (tombstones survive deletion and are
    /// exempt from retention, so this is reachable); **satellite and endpoint
    /// rows are runtime state whose definition is gone**, so they are purged.
    /// Purging is not `ON CONFLICT DO NOTHING`, which on `monitor_dispatches`
    /// would keep the orphans and discard the transferred monitor's own
    /// idempotency records, re-delivering messages it had already handled.
    ///
    /// `keep` is what the outgoing owner is left with, written by
    /// [`keep_manage_grant`] inside this same transaction and recorded against
    /// `granted_by` — so the choice cannot half-land, and
    /// [`OutgoingAccess::Nothing`] leaves the move exactly as it was.
    pub async fn transfer(
        &self,
        kind: &str,
        id: &str,
        from: &str,
        to: &str,
        keep: OutgoingAccess,
        granted_by: &str,
    ) -> Result<Transferred, TransferFailed> {
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;

        if tx
            .query_opt(DEF_ROW_FOR_UPDATE, &[&id, &kind, &from])
            .await?
            .is_none()
        {
            return Err(TransferFailed::NotFound);
        }
        if tx
            .query_opt(DEF_ROW_FOR_UPDATE, &[&id, &kind, &to])
            .await?
            .is_some()
        {
            return Err(TransferFailed::Occupied);
        }

        let monitor = kind == KIND_MONITOR;
        for table in MONITOR_SATELLITES.iter().filter(|_| monitor) {
            tx.execute(
                &format!("DELETE FROM {table} WHERE monitor_id = $1 AND owner = $2"),
                &[&id, &to],
            )
            .await?;
        }
        purge_orphan_endpoints(&tx, kind, id, to).await?;

        tx.execute(
            "INSERT INTO workflow_defs \
             SELECT (jsonb_populate_record(NULL::workflow_defs, to_jsonb(d) || $4::jsonb)).* \
             FROM workflow_defs d \
             WHERE d.id = $1 AND d.kind = $2 AND d.owner_identity_id = $3",
            &[
                &id,
                &kind,
                &from,
                &serde_json::json!({
                    "owner_identity_id": to,
                    "updated_at": Utc::now(),
                    "run_as_identity_id": None::<String>,
                }),
            ],
        )
        .await
        // The step-2 pre-flight reads a row that is not there, so it takes no
        // lock and cannot hold the gap open: a concurrent save or transfer may
        // still create the target's row before this insert lands. The primary
        // key catches it, and the requester is owed the same answer they would
        // have had a moment earlier rather than a storage failure they can do
        // nothing about.
        .map_err(|e| match e.code() {
            Some(&tokio_postgres::error::SqlState::UNIQUE_VIOLATION) => TransferFailed::Occupied,
            _ => e.into(),
        })?;

        // A grant naming the new owner stays behind: ownership subsumes every
        // grant, so moving it would only show them sharing with themselves,
        // and the old parent's cascade clears it. A *team* grant that happens
        // to contain them moves, because dropping it would revoke everyone
        // else on that team.
        let shares = tx
            .execute(
                "UPDATE def_shares SET owner_identity_id = $4 \
                 WHERE kind = $1 AND def_id = $2 AND owner_identity_id = $3 \
                   AND NOT (principal_type = 'user' AND principal_id = $4)",
                &[&kind, &id, &from, &to],
            )
            .await?;

        if keep.keeps_manage() {
            keep_manage_grant(&tx, kind, id, to, from, granted_by).await?;
        }

        // `restored_from` is a revision number in the same series, so it takes
        // the same offset or every restored revision's provenance points into
        // the target's old history. `NULL + n` is `NULL`, which is already
        // right for a revision that restored nothing. Retention is left to the
        // next write: pruning inside a move would destroy history as a side
        // effect of it.
        let revisions = tx
            .execute(
                "UPDATE def_revisions r \
                    SET owner_identity_id = $4, \
                        rev           = r.rev + o.base, \
                        restored_from = r.restored_from + o.base \
                   FROM (SELECT COALESCE(max(rev), 0) AS base FROM def_revisions \
                          WHERE kind = $1 AND ref_id = $2 AND owner_identity_id = $4) o \
                  WHERE r.kind = $1 AND r.ref_id = $2 AND r.owner_identity_id = $3",
                &[&kind, &id, &from, &to],
            )
            .await?;

        for table in MONITOR_SATELLITES.iter().filter(|_| monitor) {
            tx.execute(
                &format!("UPDATE {table} SET owner = $3 WHERE monitor_id = $1 AND owner = $2"),
                &[&id, &from, &to],
            )
            .await?;
        }
        let trigger_paths = move_endpoints(&tx, kind, id, from, to).await?;

        tx.execute(
            "DELETE FROM workflow_defs WHERE id = $1 AND kind = $2 AND owner_identity_id = $3",
            &[&id, &kind, &from],
        )
        .await?;
        tx.commit().await?;
        Ok(Transferred {
            trigger_paths,
            shares,
            revisions,
            kept_manage: keep.keeps_manage(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn with_store<F, Fut>(test: F)
    where
        F: FnOnce(Arc<WorkflowStore>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        with_store_retention(DEFAULT_MAX_REVISIONS, test).await;
    }

    async fn with_store_retention<F, Fut>(max_revisions: u32, test: F)
    where
        F: FnOnce(Arc<WorkflowStore>) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        workforce_postgres::test_support::with_test_pool(
            WorkflowStore::build_pool,
            |pool| async move {
                let store = WorkflowStore::with_retention(pool, max_revisions)
                    .await
                    .expect("store");
                test(store).await;
            },
        )
        .await;
    }

    /// A store and the pool behind it, for the cases that assert on rows the
    /// store has no reader for.
    async fn with_store_pool<F, Fut>(test: F)
    where
        F: FnOnce(Arc<WorkflowStore>, PostgresPool) -> Fut,
        Fut: std::future::Future<Output = ()>,
    {
        workforce_postgres::test_support::with_test_pool(
            WorkflowStore::build_pool,
            |pool| async move {
                let store = WorkflowStore::with_retention(pool.clone(), DEFAULT_MAX_REVISIONS)
                    .await
                    .expect("store");
                test(store, pool).await;
            },
        )
        .await;
    }

    fn binding(def_id: &str, owner: Option<&str>, trigger_id: &str) -> TriggerBinding {
        TriggerBinding {
            kind: KIND_WORKFLOW.to_string(),
            def_id: def_id.to_string(),
            owner: owner.map(str::to_string),
            trigger_id: trigger_id.to_string(),
        }
    }

    /// A reload's two steps, exactly as the runtime runs them: read the
    /// definitions-and-endpoints snapshot, then mint whatever it lacks.
    async fn ensure(
        store: &WorkflowStore,
        bindings: &HashSet<TriggerBinding>,
    ) -> HashMap<TriggerBinding, String> {
        let (_, known) = store
            .list_with_endpoints(KIND_WORKFLOW)
            .await
            .expect("read definitions and endpoints");
        store
            .ensure_endpoints(&known, bindings)
            .await
            .expect("ensure endpoints")
    }

    async fn endpoint_count(pool: &PostgresPool) -> i64 {
        pool.get()
            .await
            .expect("conn")
            .query_one("SELECT count(*) FROM trigger_endpoints", &[])
            .await
            .expect("count")
            .get(0)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ensure_endpoints_mints_once_and_is_idempotent() {
        with_store_pool(|s, pool| async move {
            let bindings = HashSet::from([binding("wf1", Some("alice"), "deploy")]);

            let first = ensure(&s, &bindings).await;
            let second = ensure(&s, &bindings).await;

            assert_eq!(first, second, "a binding's path is minted once");
            assert!(first
                .values()
                .all(|path| path.starts_with(TRIGGER_PATH_PREFIX)));
            assert_eq!(endpoint_count(&pool).await, 1);
        })
        .await;
    }

    /// Two replicas reloading the same new binding concurrently is the normal
    /// case. They must converge on one path with no coordination beyond the
    /// unique constraint — a claim about PostgreSQL's behaviour under
    /// `ON CONFLICT … DO NOTHING`, not about our reasoning.
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_ensure_endpoints_agree_on_one_path() {
        with_store_pool(|s, pool| async move {
            let bindings = HashSet::from([binding("wf1", None, "deploy")]);

            let (a, b) = tokio::join!(ensure(&s, &bindings), ensure(&s, &bindings));

            assert_eq!(a, b, "both replicas must serve the same path");
            assert_eq!(endpoint_count(&pool).await, 1);
        })
        .await;
    }

    /// The row outlives the definition, so a workflow rebuilt after a mistake
    /// keeps the integrations pointed at it.
    #[tokio::test(flavor = "multi_thread")]
    async fn an_endpoint_survives_its_definition_being_deleted() {
        with_store(|s| async move {
            let bindings = HashSet::from([binding("wf1", Some("alice"), "deploy")]);
            let save = |s: Arc<WorkflowStore>| async move {
                s.save(
                    "wf1",
                    KIND_WORKFLOW,
                    TOML,
                    None,
                    Some("alice"),
                    Visibility::Private,
                    RunAsWrite::Unchanged,
                    edit("web", Some("alice")),
                )
                .await
                .expect("save")
            };

            save(Arc::clone(&s)).await;
            let before = ensure(&s, &bindings).await;
            s.delete(
                "wf1",
                KIND_WORKFLOW,
                Some("alice"),
                edit("web", Some("alice")),
            )
            .await
            .expect("delete");
            save(Arc::clone(&s)).await;

            assert_eq!(ensure(&s, &bindings).await, before);
        })
        .await;
    }

    /// The property that makes never deleting a row safe: a different member
    /// taking a deleted definition's id cannot inherit an address external
    /// callers still hold.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_different_owner_reusing_the_id_gets_a_different_path() {
        with_store(|s| async move {
            let alice = binding("wf1", Some("alice"), "deploy");
            let bob = binding("wf1", Some("bob"), "deploy");

            let paths = ensure(&s, &HashSet::from([alice.clone(), bob.clone()])).await;

            assert_ne!(paths[&alice], paths[&bob]);
        })
        .await;
    }

    /// A partial load during a bad deploy — or a definition that is
    /// transiently unparseable — must never take a live URL off the air.
    #[tokio::test(flavor = "multi_thread")]
    async fn ensure_endpoints_is_insert_only() {
        with_store_pool(|s, pool| async move {
            let kept = binding("wf1", None, "deploy");
            let omitted = binding("wf2", None, "notify");
            let both = HashSet::from([kept.clone(), omitted.clone()]);
            let all = ensure(&s, &both).await;

            let narrowed = ensure(&s, &HashSet::from([kept.clone()])).await;

            assert_eq!(narrowed.len(), 1, "only what was asked for is returned");
            assert_eq!(narrowed[&kept], all[&kept]);
            assert_eq!(endpoint_count(&pool).await, 2, "the omitted row survives");
            assert_eq!(ensure(&s, &both).await, all);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn ensure_endpoints_touches_nothing_when_given_no_bindings() {
        with_store_pool(|s, pool| async move {
            let paths = s
                .ensure_endpoints(&HashMap::new(), &HashSet::new())
                .await
                .expect("ensure endpoints");

            assert!(paths.is_empty());
            assert_eq!(endpoint_count(&pool).await, 0);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn system_and_member_bindings_coexist() {
        with_store_pool(|s, pool| async move {
            let system = binding("wf1", None, "deploy");
            let member = binding("wf1", Some("alice"), "deploy");

            let paths = ensure(&s, &HashSet::from([system.clone(), member.clone()])).await;

            assert_ne!(paths[&system], paths[&member]);
            assert_eq!(endpoint_count(&pool).await, 2);
        })
        .await;
    }

    /// What the ownership transfer that follows this change will do to the
    /// rows: every one of a definition's endpoints moves, and not one public
    /// path changes. Asserted against the schema, which is what the transfer
    /// will rely on.
    #[tokio::test(flavor = "multi_thread")]
    async fn move_endpoints_keeps_the_path_and_moves_the_owner() {
        with_store_pool(|s, pool| async move {
            let bindings = HashSet::from([
                binding("wf1", Some("alice"), "deploy"),
                binding("wf1", Some("alice"), "notify"),
            ]);
            let before = ensure(&s, &bindings).await;

            let conn = pool.get().await.expect("conn");
            let moved = conn
                .execute(
                    "UPDATE trigger_endpoints SET owner_identity_id = $4 \
                     WHERE kind = $1 AND def_id = $2 AND owner_identity_id = $3",
                    &[&KIND_WORKFLOW, &"wf1", &"alice", &"bob"],
                )
                .await
                .expect("move");

            assert_eq!(moved, 2, "both of a definition's endpoints move");
            let after = ensure(
                &s,
                &HashSet::from([
                    binding("wf1", Some("bob"), "deploy"),
                    binding("wf1", Some("bob"), "notify"),
                ]),
            )
            .await;
            assert_eq!(
                after.values().collect::<HashSet<_>>(),
                before.values().collect::<HashSet<_>>(),
                "a transfer must not move a public path"
            );
            assert_eq!(endpoint_count(&pool).await, 2, "nothing was re-minted");
        })
        .await;
    }

    /// The backstop under the transfer's own refusal: moving onto a binding
    /// the target already holds fails loudly rather than corrupting.
    #[tokio::test(flavor = "multi_thread")]
    async fn moving_onto_an_occupied_binding_is_refused_by_the_constraint() {
        with_store_pool(|s, pool| async move {
            ensure(
                &s,
                &HashSet::from([
                    binding("wf1", Some("alice"), "deploy"),
                    binding("wf1", Some("bob"), "deploy"),
                ]),
            )
            .await;

            let conn = pool.get().await.expect("conn");
            let clash = conn
                .execute(
                    "UPDATE trigger_endpoints SET owner_identity_id = $4 \
                     WHERE kind = $1 AND def_id = $2 AND owner_identity_id = $3",
                    &[&KIND_WORKFLOW, &"wf1", &"alice", &"bob"],
                )
                .await;

            let err = clash.expect_err("an occupied binding must refuse the move");
            let db = err.as_db_error().expect("a database error");
            assert_eq!(db.constraint(), Some("trigger_endpoints_binding_key"));
        })
        .await;
    }

    const TOML: &str = "[workflow]\nid = \"wf1\"\nname = \"WF\"\n";

    fn edit(via: &'static str, by: Option<&'static str>) -> EditContext<'static> {
        EditContext::new(via, by)
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_revision_is_found_by_its_content_hash_while_it_is_kept() {
        with_store(|s| async move {
            for text in ["v1", "v2"] {
                s.save(
                    "wf1",
                    "workflow",
                    text,
                    None,
                    None,
                    Visibility::Global,
                    RunAsWrite::Unchanged,
                    edit("web", Some("kingsley")),
                )
                .await
                .unwrap();
                let found = s.revision_content("workflow", &content_hash(text)).await;
                assert_eq!(found.unwrap().as_deref(), Some(text));
            }
            let earlier = s.revision_content("workflow", &content_hash("v1")).await;
            assert_eq!(
                earlier.unwrap().as_deref(),
                Some("v1"),
                "not only the newest"
            );
            let unknown = s.revision_content("workflow", &content_hash("v3")).await;
            assert_eq!(unknown.unwrap(), None);
            let other_kind = s.revision_content("playbook", &content_hash("v1")).await;
            assert_eq!(other_kind.unwrap(), None);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn seed_is_idempotent_then_save_updates() {
        with_store(|s| async move {
            assert!(s.seed("wf1", "workflow", TOML).await.unwrap());
            // Second seed is a no-op.
            assert!(!s.seed("wf1", "workflow", "changed").await.unwrap());
            // Original preserved; seed is system-owned + global.
            assert_eq!(
                s.get("wf1", "workflow", None).await.unwrap().as_deref(),
                Some(TOML)
            );

            // save under the system owner overwrites.
            s.save(
                "wf1",
                "workflow",
                "v2",
                None,
                None,
                Visibility::Global,
                RunAsWrite::Unchanged,
                edit("web", Some("kingsley")),
            )
            .await
            .unwrap();
            assert_eq!(
                s.get("wf1", "workflow", None).await.unwrap().as_deref(),
                Some("v2")
            );

            // kind isolation.
            assert!(s.get("wf1", "playbook", None).await.unwrap().is_none());

            let all = s.list("workflow").await.unwrap();
            assert_eq!(all.len(), 1);
            assert!(all[0].owner.is_none());
            assert_eq!(all[0].visibility, Visibility::Global);

            assert!(s.change_marker("workflow").await.unwrap().0.is_some());

            s.delete("wf1", "workflow", None, edit("web", Some("kingsley")))
                .await
                .unwrap();
            assert!(s.list("workflow").await.unwrap().is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn revisions_capture_dedupe_tombstone_restore() {
        with_store(|s| async move {
            // Seed gives the baseline revision.
            assert!(s.seed("wf1", "workflow", TOML).await.unwrap());
            let revs = s.list_revisions("workflow", "wf1", None).await.unwrap();
            assert_eq!(revs.len(), 1);
            assert_eq!(revs[0].edited_via, "seed");
            assert!(revs[0].edited_by.is_none());

            // A content change appends a revision with the author.
            s.save(
                "wf1",
                "workflow",
                "v2",
                None,
                None,
                Visibility::Global,
                RunAsWrite::Unchanged,
                edit("web", Some("kingsley")),
            )
            .await
            .unwrap();
            // A byte-identical save is deduped — no new revision.
            s.save(
                "wf1",
                "workflow",
                "v2",
                None,
                None,
                Visibility::Global,
                RunAsWrite::Unchanged,
                edit("web", Some("kingsley")),
            )
            .await
            .unwrap();
            let revs = s.list_revisions("workflow", "wf1", None).await.unwrap();
            assert_eq!(revs.len(), 2, "identical save must not add a revision");
            assert_eq!(revs[0].rev, 2);
            assert_eq!(revs[0].edited_by.as_deref(), Some("kingsley"));
            assert_eq!(revs[0].edited_via, "web");

            // List omits content; get returns it.
            assert!(revs[0].content.is_none());
            let full = s
                .get_revision("workflow", "wf1", None, 2)
                .await
                .unwrap()
                .expect("rev 2");
            assert_eq!(full.content.as_deref(), Some("v2"));
            assert_eq!(full.content_hash, content_hash("v2"));

            // Delete writes a tombstone that survives the def row.
            s.delete("wf1", "workflow", None, edit("web", Some("kingsley")))
                .await
                .unwrap();
            assert!(s.get("wf1", "workflow", None).await.unwrap().is_none());
            let revs = s.list_revisions("workflow", "wf1", None).await.unwrap();
            assert_eq!(revs.len(), 3);
            assert!(revs[0].deleted);
            assert_eq!(revs[0].edited_by.as_deref(), Some("kingsley"));
            // Deleting again (no row) adds nothing.
            s.delete("wf1", "workflow", None, edit("web", Some("kingsley")))
                .await
                .unwrap();
            assert_eq!(
                s.list_revisions("workflow", "wf1", None)
                    .await
                    .unwrap()
                    .len(),
                3
            );

            // A deleted definition is NOT resurrected by a restart's re-seed.
            assert!(
                !s.seed("wf1", "workflow", TOML).await.unwrap(),
                "seed must respect the delete tombstone"
            );
            assert!(s.get("wf1", "workflow", None).await.unwrap().is_none());

            // Restore = a NEW revision whose content equals the source rev.
            let source = s
                .get_revision("workflow", "wf1", None, 1)
                .await
                .unwrap()
                .expect("rev 1");
            s.save(
                "wf1",
                "workflow",
                source.content.as_deref().unwrap(),
                None,
                None,
                Visibility::Global,
                RunAsWrite::Unchanged,
                EditContext::restore(Some("kingsley"), 1),
            )
            .await
            .unwrap();
            let revs = s.list_revisions("workflow", "wf1", None).await.unwrap();
            assert_eq!(revs[0].rev, 4);
            assert_eq!(revs[0].edited_via, "restore");
            assert_eq!(revs[0].restored_from, Some(1));
            assert_eq!(
                s.get("wf1", "workflow", None).await.unwrap().as_deref(),
                Some(TOML)
            );

            // last_modified_by landed on the def row.
            let conn = s.pool.get().await.unwrap();
            let row = conn
                .query_one(
                    "SELECT last_modified_by FROM workflow_defs WHERE id = 'wf1'",
                    &[],
                )
                .await
                .unwrap();
            assert_eq!(
                row.get::<_, Option<String>>("last_modified_by").as_deref(),
                Some("kingsley")
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn revision_retention_prunes_content_keeps_tombstones() {
        with_store_retention(2, |s| async move {
            for v in ["v1", "v2", "v3", "v4"] {
                s.save(
                    "wf1",
                    "workflow",
                    v,
                    None,
                    None,
                    Visibility::Global,
                    RunAsWrite::Unchanged,
                    edit("web", Some("kingsley")),
                )
                .await
                .unwrap();
            }
            let revs = s.list_revisions("workflow", "wf1", None).await.unwrap();
            assert_eq!(revs.len(), 2, "retention=2 keeps the newest two");
            assert_eq!(revs[0].rev, 4);
            assert_eq!(revs[1].rev, 3);

            // The tombstone is exempt from retention.
            s.delete("wf1", "workflow", None, edit("web", Some("kingsley")))
                .await
                .unwrap();
            s.save(
                "wf1",
                "workflow",
                "v5",
                None,
                None,
                Visibility::Global,
                RunAsWrite::Unchanged,
                edit("web", Some("kingsley")),
            )
            .await
            .unwrap();
            s.save(
                "wf1",
                "workflow",
                "v6",
                None,
                None,
                Visibility::Global,
                RunAsWrite::Unchanged,
                edit("web", Some("kingsley")),
            )
            .await
            .unwrap();
            let revs = s.list_revisions("workflow", "wf1", None).await.unwrap();
            let tombstones: Vec<_> = revs.iter().filter(|r| r.deleted).collect();
            assert_eq!(tombstones.len(), 1, "tombstone survives pruning");
            assert_eq!(revs.iter().filter(|r| !r.deleted).count(), 2);
        })
        .await;
    }

    /// Two saves of one file at the same moment — Desk applying a setting
    /// twice — each get their own revision instead of one losing to a
    /// duplicate `rev`.
    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_revisions_of_one_ref_all_land() {
        with_store(|s| async move {
            let saves: Vec<_> = (0..8)
                .map(|n| {
                    let s = s.clone();
                    tokio::spawn(async move {
                        s.record_revision(
                            KIND_IDENTITY_FILE,
                            "desk/config.toml",
                            None,
                            &format!("# v{n}"),
                            false,
                            edit("web", Some("kingsley")),
                            None,
                        )
                        .await
                    })
                })
                .collect();
            let mut revs = Vec::new();
            for save in saves {
                revs.push(
                    save.await
                        .expect("task")
                        .expect("revision")
                        .expect("not deduped"),
                );
            }
            revs.sort_unstable();
            assert_eq!(revs, (1..=8).collect::<Vec<_>>());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn record_revision_for_non_def_kinds() {
        with_store(|s| async move {
            // Identity-file capture: write-through then an OOB change.
            let rev = s
                .record_revision(
                    KIND_IDENTITY_FILE,
                    "allan/SOUL.md",
                    None,
                    "# Soul v1",
                    false,
                    edit("web", Some("kingsley")),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(rev, Some(1));
            // Dedupe applies to non-def kinds too.
            let rev = s
                .record_revision(
                    KIND_IDENTITY_FILE,
                    "allan/SOUL.md",
                    None,
                    "# Soul v1",
                    false,
                    edit("web", Some("kingsley")),
                    None,
                )
                .await
                .unwrap();
            assert_eq!(rev, None);
            assert_eq!(
                s.latest_revision_hash(KIND_IDENTITY_FILE, "allan/SOUL.md", None)
                    .await
                    .unwrap()
                    .as_deref(),
                Some(content_hash("# Soul v1").as_str())
            );

            // Config-overlay capture with a detail payload.
            let rev = s
                .record_revision(
                    KIND_CONFIG_OVERLAY,
                    "global",
                    None,
                    "[overlay]\n",
                    false,
                    edit("web", Some("kingsley")),
                    Some(r#"{"action":"set","key_path":"agents.maple"}"#),
                )
                .await
                .unwrap();
            assert_eq!(rev, Some(1));
        })
        .await;
    }

    /// Reproduces the prod V005→V006 upgrade on a *populated* table.
    ///
    /// The existing tests run V006 against an empty `workflow_defs`, where
    /// `ADD PRIMARY KEY` is trivially satisfiable. Production already holds
    /// rows (all `agent_id = '_global'`), so this builds the V005 shape by
    /// hand, inserts two legacy rows, then runs V006's exact body and asserts
    /// the PK swap succeeds and the new columns are backfilled. Guards against
    /// a wrong `workflow_defs_pkey` name and any duplicate-(id) PK violation.
    #[tokio::test(flavor = "multi_thread")]
    async fn v006_alters_populated_v005_table() {
        workforce_postgres::test_support::with_test_pool(
            WorkflowStore::build_pool,
            |pool| async move {
                let conn = pool.get().await.expect("conn");

                // ── V005 shape (pre-ownership), in the telemetry schema the
                // pinned search_path resolves to.
                conn.batch_execute(
                    "CREATE SCHEMA IF NOT EXISTS telemetry;
                     CREATE TABLE telemetry.workflow_defs (
                         id          TEXT NOT NULL,
                         agent_id    TEXT NOT NULL,
                         kind        TEXT NOT NULL DEFAULT 'workflow',
                         toml_source TEXT NOT NULL,
                         parsed_json JSONB,
                         created_at  TIMESTAMPTZ NOT NULL,
                         updated_at  TIMESTAMPTZ NOT NULL,
                         PRIMARY KEY (id, agent_id)
                     );",
                )
                .await
                .expect("create V005 workflow_defs");

                // Two legacy rows, both under the single '_global' agent_id —
                // exactly how the store has only ever written them.
                let now = workforce_postgres::test_support::now_micros();
                for id in ["wf-a", "wf-b"] {
                    conn.execute(
                        "INSERT INTO workflow_defs \
                         (id, agent_id, kind, toml_source, created_at, updated_at) \
                         VALUES ($1, '_global', 'workflow', 'x', $2, $2)",
                        &[&id, &now],
                    )
                    .await
                    .expect("seed legacy row");
                }

                // ── V006 body, verbatim from the migration file.
                conn.batch_execute(
                    "ALTER TABLE workflow_defs
                         ADD COLUMN IF NOT EXISTS owner_identity_id TEXT NOT NULL DEFAULT '__system__',
                         ADD COLUMN IF NOT EXISTS visibility TEXT NOT NULL DEFAULT 'private';
                     UPDATE workflow_defs SET visibility = 'global' WHERE owner_identity_id = '__system__';
                     ALTER TABLE workflow_defs DROP CONSTRAINT IF EXISTS workflow_defs_pkey;
                     ALTER TABLE workflow_defs ADD PRIMARY KEY (id, owner_identity_id);
                     CREATE INDEX IF NOT EXISTS idx_workflow_defs_owner ON workflow_defs(owner_identity_id);
                     CREATE INDEX IF NOT EXISTS idx_workflow_defs_visibility ON workflow_defs(visibility);",
                )
                .await
                .expect("V006 must apply cleanly on a populated V005 table");

                // Rows survived, backfilled to system-owner + global.
                let rows = conn
                    .query(
                        "SELECT id, owner_identity_id, visibility FROM workflow_defs ORDER BY id",
                        &[],
                    )
                    .await
                    .expect("read back");
                assert_eq!(rows.len(), 2);
                for r in &rows {
                    assert_eq!(r.get::<_, String>("owner_identity_id"), SYSTEM_OWNER);
                    assert_eq!(r.get::<_, String>("visibility"), Visibility::Global.as_str());
                }

                // New PK is (id, owner_identity_id): same id under a different
                // owner now coexists; a duplicate (id, owner) is rejected.
                conn.execute(
                    "INSERT INTO workflow_defs \
                     (id, agent_id, kind, toml_source, owner_identity_id, visibility, \
                      created_at, updated_at) \
                     VALUES ('wf-a', '_global', 'workflow', 'y', 'alice', 'private', $1, $1)",
                    &[&now],
                )
                .await
                .expect("same id, distinct owner must insert");
                let dup = conn
                    .execute(
                        "INSERT INTO workflow_defs \
                         (id, agent_id, kind, toml_source, owner_identity_id, visibility, \
                          created_at, updated_at) \
                         VALUES ('wf-a', '_global', 'workflow', 'z', '__system__', 'global', $1, $1)",
                        &[&now],
                    )
                    .await;
                assert!(dup.is_err(), "duplicate (id, owner) must violate the new PK");
            },
        )
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn same_id_distinct_owners_do_not_collide() {
        with_store(|s| async move {
            s.save(
                "shared",
                "workflow",
                "a",
                None,
                Some("alice"),
                Visibility::Private,
                RunAsWrite::Unchanged,
                edit("web", Some("alice")),
            )
            .await
            .unwrap();
            s.save(
                "shared",
                "workflow",
                "b",
                None,
                Some("bob"),
                Visibility::Global,
                RunAsWrite::Unchanged,
                edit("web", Some("bob")),
            )
            .await
            .unwrap();

            assert_eq!(
                s.get("shared", "workflow", Some("alice"))
                    .await
                    .unwrap()
                    .as_deref(),
                Some("a")
            );
            assert_eq!(
                s.get("shared", "workflow", Some("bob"))
                    .await
                    .unwrap()
                    .as_deref(),
                Some("b")
            );

            // Deleting alice's leaves bob's intact.
            s.delete(
                "shared",
                "workflow",
                Some("alice"),
                edit("web", Some("alice")),
            )
            .await
            .unwrap();
            assert!(s
                .get("shared", "workflow", Some("alice"))
                .await
                .unwrap()
                .is_none());
            assert_eq!(
                s.get("shared", "workflow", Some("bob"))
                    .await
                    .unwrap()
                    .as_deref(),
                Some("b")
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn run_as_is_kept_set_and_cleared_by_saves() {
        async fn run_as_of(s: &WorkflowStore) -> Option<String> {
            s.list("workflow")
                .await
                .unwrap()
                .into_iter()
                .find(|d| d.id == "triage")
                .and_then(|d| d.run_as)
        }
        async fn save_triage(
            s: &WorkflowStore,
            source: &str,
            run_as: RunAsWrite,
            by: &'static str,
        ) {
            s.save(
                "triage",
                "workflow",
                source,
                None,
                Some("alice"),
                Visibility::Private,
                run_as,
                edit("web", Some(by)),
            )
            .await
            .unwrap();
        }
        with_store(|s| async move {
            save_triage(&s, "v1", RunAsWrite::Unchanged, "alice").await;
            assert_eq!(run_as_of(&s).await, None, "a fresh row runs as its owner");

            save_triage(&s, "v2", RunAsWrite::Identity("bob".into()), "bob").await;
            assert_eq!(run_as_of(&s).await.as_deref(), Some("bob"));

            save_triage(&s, "v3", RunAsWrite::Unchanged, "alice").await;
            assert_eq!(
                run_as_of(&s).await.as_deref(),
                Some("bob"),
                "a save that says nothing keeps the run identity"
            );

            save_triage(&s, "v4", RunAsWrite::Owner, "alice").await;
            assert_eq!(run_as_of(&s).await, None);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn same_id_distinct_kinds_do_not_collide() {
        with_store(|s| async move {
            s.save(
                "group",
                "playbook",
                "PB",
                None,
                Some("alice"),
                Visibility::Private,
                RunAsWrite::Unchanged,
                edit("web", Some("alice")),
            )
            .await
            .unwrap();
            s.save(
                "group",
                "consolidation_template",
                "CT",
                None,
                Some("alice"),
                Visibility::Private,
                RunAsWrite::Unchanged,
                edit("web", Some("alice")),
            )
            .await
            .unwrap();

            // Same id + owner, different kind → both coexist (no overwrite).
            assert_eq!(
                s.get("group", "playbook", Some("alice"))
                    .await
                    .unwrap()
                    .as_deref(),
                Some("PB")
            );
            assert_eq!(
                s.get("group", "consolidation_template", Some("alice"))
                    .await
                    .unwrap()
                    .as_deref(),
                Some("CT")
            );

            // Delete is kind-scoped: removing the playbook leaves the template.
            s.delete(
                "group",
                "playbook",
                Some("alice"),
                edit("web", Some("alice")),
            )
            .await
            .unwrap();
            assert!(s
                .get("group", "playbook", Some("alice"))
                .await
                .unwrap()
                .is_none());
            assert_eq!(
                s.get("group", "consolidation_template", Some("alice"))
                    .await
                    .unwrap()
                    .as_deref(),
                Some("CT")
            );
        })
        .await;
    }

    #[test]
    fn auto_run_taint_gate() {
        // System/disk definitions (no owner) are operator-trusted, never tainted.
        assert!(!auto_run_tainted(None, None, Some("ops")));
        // Owner authored it and it runs as the owner → safe.
        assert!(!auto_run_tainted(
            Some("alice"),
            Some("alice"),
            Some("alice")
        ));
        // A share grantee last edited it but it runs as the owner → tainted.
        assert!(auto_run_tainted(Some("alice"), Some("bob"), Some("alice")));
        // Owner-authored, but the run identity is someone else (e.g. another user
        // scheduled it) → foreign content would run as that user → tainted.
        assert!(auto_run_tainted(Some("alice"), Some("alice"), Some("bob")));
        // Unattributed member definition (legacy / never foreign-edited) → safe.
        assert!(!auto_run_tainted(Some("alice"), None, Some("alice")));
        // Hub automation (a sweep auto-disable) last saved it → not foreign.
        assert!(!auto_run_tainted(
            Some("alice"),
            Some(SYSTEM_EDITOR),
            Some("alice")
        ));
    }

    /// What automation does the instant after a transfer, which is the same
    /// question read with the new owner as the run identity — `transfer`
    /// clears `run_as`, so the run identity *is* the new owner. The second row
    /// is why the rule is worded "paused until the new owner has saved it"
    /// rather than an unconditional "paused": handing a definition to the
    /// grantee who has been maintaining it hands it to someone who already
    /// wrote what it will run.
    #[test]
    fn a_transfer_pauses_automation_until_the_new_owner_has_saved_it() {
        let after =
            |last_editor: Option<&str>| auto_run_tainted(Some("bob"), last_editor, Some("bob"));
        assert!(
            after(Some("alice")),
            "content the new owner has not approved"
        );
        assert!(!after(Some("bob")), "the grantee who wrote it now owns it");
        assert!(!after(None), "a legacy row was never foreign-edited");
        assert!(
            !after(Some(SYSTEM_EDITOR)),
            "hub automation is not a grantee"
        );
        // The row that makes clearing `run_as` load-bearing: were it kept, the
        // definition would go on firing as a third party the new owner never
        // approved — and, when that identity has departed, block as `Inactive`,
        // whose message names no action anyone can take.
        assert!(!auto_run_tainted(Some("bob"), Some("carol"), Some("carol")));
    }

    /// Alice's teammate, and nobody else's.
    struct AliceTeam;

    impl TeamMembership for AliceTeam {
        fn in_team(&self, team_id: &str) -> bool {
            team_id == "platform"
        }

        fn shares_team_with(&self, owner: &str) -> bool {
            owner == "alice"
        }
    }

    #[test]
    fn a_row_nobody_owns_is_reached_through_the_visibility_it_was_given() {
        use workforce_core::access::NoTeams;
        // The system row is the operator's, not everybody's: `global` is open,
        // and the reach an operator narrowed stays narrowed.
        assert_eq!(
            def_access(None, Visibility::Global, Some("bob"), None, &NoTeams),
            AccessLevel::Run
        );
        for visibility in [Visibility::Private, Visibility::Team] {
            assert_eq!(
                def_access(None, visibility, Some("bob"), None, &AliceTeam),
                AccessLevel::None
            );
        }
        // Nobody owns it, so nobody manages it by owning it — not even a
        // caller whose id happens to be the owner column's sentinel.
        assert_eq!(
            def_access(None, Visibility::Global, Some(SYSTEM_OWNER), None, &NoTeams),
            AccessLevel::Run
        );
    }

    #[test]
    fn a_member_row_is_reached_by_its_owner_their_team_and_their_grantees() {
        use workforce_core::access::NoTeams;
        let team = &AliceTeam;
        assert_eq!(
            def_access(
                Some("alice"),
                Visibility::Private,
                Some("alice"),
                None,
                team
            ),
            AccessLevel::Manage
        );
        assert_eq!(
            def_access(Some("alice"), Visibility::Team, Some("bob"), None, team),
            AccessLevel::Run
        );
        assert_eq!(
            def_access(Some("alice"), Visibility::Team, Some("bob"), None, &NoTeams),
            AccessLevel::None
        );
        assert_eq!(
            def_access(
                Some("alice"),
                Visibility::Private,
                Some("bob"),
                Some(GrantLevel::Manage),
                &NoTeams
            ),
            AccessLevel::Manage
        );
        // An empty identity is no identity.
        assert_eq!(
            def_access(Some("alice"), Visibility::Team, Some(""), None, team),
            AccessLevel::None
        );
    }

    #[test]
    fn a_listing_shows_admins_everything_and_everyone_else_what_reaches_them() {
        use workforce_core::access::NoTeams;
        let shared: std::collections::HashSet<String> =
            [member_def_key("alice", "triage")].into_iter().collect();
        let sees = |owner, visibility, caller, is_admin, shared, teams: &dyn TeamMembership| {
            visible_to(owner, visibility, "triage", caller, is_admin, shared, teams)
        };
        assert!(sees(
            Some("alice"),
            Visibility::Private,
            Some("bob"),
            true,
            None,
            &NoTeams
        ));
        assert!(!sees(
            Some("alice"),
            Visibility::Private,
            Some("bob"),
            false,
            None,
            &NoTeams
        ));
        assert!(sees(
            Some("alice"),
            Visibility::Private,
            Some("bob"),
            false,
            Some(&shared),
            &NoTeams
        ));
        assert!(sees(
            Some("alice"),
            Visibility::Team,
            Some("bob"),
            false,
            None,
            &AliceTeam
        ));
        assert!(!sees(
            None,
            Visibility::Private,
            Some("bob"),
            false,
            Some(&shared),
            &AliceTeam
        ));
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn visibility_moves_without_touching_the_content_or_its_editor() {
        with_store(|s| async move {
            s.save(
                "triage",
                "workflow",
                "x",
                None,
                Some("alice"),
                Visibility::Private,
                RunAsWrite::Identity("bob".into()),
                edit("web", Some("alice")),
            )
            .await
            .unwrap();
            let stored = |s: Arc<WorkflowStore>| async move {
                s.list("workflow")
                    .await
                    .unwrap()
                    .into_iter()
                    .find(|d| d.id == "triage")
                    .unwrap()
            };
            let before = stored(Arc::clone(&s)).await;

            assert!(s
                .set_visibility("workflow", "triage", "alice", Visibility::Global)
                .await
                .unwrap());
            let after = stored(Arc::clone(&s)).await;
            assert_eq!(after.visibility, Visibility::Global);
            assert_eq!(after.toml_source, before.toml_source);
            assert_eq!(after.last_editor, before.last_editor);
            assert_eq!(after.run_as, before.run_as);
            assert_eq!(
                s.list_revisions("workflow", "triage", Some("alice"))
                    .await
                    .unwrap()
                    .len(),
                1,
                "who reaches a definition is not a revision of it"
            );

            // Another owner's row of the same id, and an unknown one, are left
            // alone — the write is addressed by the whole key.
            assert!(!s
                .set_visibility("workflow", "triage", "bob", Visibility::Global)
                .await
                .unwrap());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn shares_grant_resolve_dedupe_and_cascade() {
        with_store(|s| async move {
            // A member-owned definition the share FK can reference.
            s.save(
                "wf1",
                "workflow",
                "x",
                None,
                Some("alice"),
                Visibility::Private,
                RunAsWrite::Unchanged,
                edit("web", Some("alice")),
            )
            .await
            .unwrap();

            // Share with bob (a duplicate principal collapses to the highest
            // permission) and with a team.
            s.set_shares(
                "workflow",
                "wf1",
                "alice",
                &[
                    Grant::user("bob", GrantLevel::Run),
                    Grant::user("bob", GrantLevel::Manage),
                    Grant::team("team-x", GrantLevel::Manage),
                ],
                "alice",
            )
            .await
            .unwrap();

            let shares = s.list_shares("workflow", "wf1", "alice").await.unwrap();
            assert_eq!(shares.len(), 2, "duplicate principal deduped");
            let bob = shares.iter().find(|x| x.principal_id == "bob").unwrap();
            assert_eq!(bob.permission, GrantLevel::Manage, "highest kept");

            // bob resolves manage directly; carol resolves manage via team-x.
            assert_eq!(
                s.caller_share_permission("workflow", "wf1", "alice", Some("bob"), &[])
                    .await
                    .unwrap(),
                Some(GrantLevel::Manage)
            );
            assert_eq!(
                s.caller_share_permission(
                    "workflow",
                    "wf1",
                    "alice",
                    Some("carol"),
                    &["team-x".into()]
                )
                .await
                .unwrap(),
                Some(GrantLevel::Manage)
            );
            // A stranger (no grant, no matching team) gets nothing.
            assert_eq!(
                s.caller_share_permission("workflow", "wf1", "alice", Some("dave"), &[])
                    .await
                    .unwrap(),
                None
            );

            // Reverse lookup returns the def once at the max permission.
            let bobs = s
                .shares_for_caller("workflow", Some("bob"), &[])
                .await
                .unwrap();
            assert_eq!(bobs.len(), 1);
            assert_eq!(bobs[0].def_id, "wf1");
            assert_eq!(bobs[0].owner, "alice");
            assert_eq!(bobs[0].permission, GrantLevel::Manage);

            // set_shares is a full replace: re-sharing only with the team at a
            // lower level drops bob and downgrades the team grant.
            s.set_shares(
                "workflow",
                "wf1",
                "alice",
                &[Grant::team("team-x", GrantLevel::Run)],
                "alice",
            )
            .await
            .unwrap();
            assert_eq!(
                s.list_shares("workflow", "wf1", "alice")
                    .await
                    .unwrap()
                    .len(),
                1
            );
            assert_eq!(
                s.caller_share_permission("workflow", "wf1", "alice", Some("bob"), &[])
                    .await
                    .unwrap(),
                None
            );
            assert_eq!(
                s.caller_share_permission(
                    "workflow",
                    "wf1",
                    "alice",
                    Some("carol"),
                    &["team-x".into()]
                )
                .await
                .unwrap(),
                Some(GrantLevel::Run)
            );

            // Deleting the definition cascades its shares away.
            s.delete("wf1", "workflow", Some("alice"), edit("web", Some("alice")))
                .await
                .unwrap();
            assert!(s
                .list_shares("workflow", "wf1", "alice")
                .await
                .unwrap()
                .is_empty());
            assert!(s
                .shares_for_caller("workflow", Some("carol"), &["team-x".into()])
                .await
                .unwrap()
                .is_empty());
        })
        .await;
    }
    // ── Ownership transfer ────────────────────────────────────────

    async fn save_owned(s: &WorkflowStore, kind: &str, id: &str, owner: &str, body: &str) {
        s.save(
            id,
            kind,
            body,
            None,
            Some(owner),
            Visibility::Private,
            RunAsWrite::Unchanged,
            EditContext::new("web", Some(owner)),
        )
        .await
        .expect("save");
    }

    async fn def_row(
        pool: &PostgresPool,
        kind: &str,
        id: &str,
        owner: &str,
    ) -> Option<tokio_postgres::Row> {
        pool.get()
            .await
            .expect("conn")
            .query_opt(
                "SELECT * FROM workflow_defs WHERE id = $1 AND kind = $2 AND owner_identity_id = $3",
                &[&id, &kind, &owner],
            )
            .await
            .expect("row")
    }

    /// A transfer that leaves the outgoing owner nothing — every case but the
    /// ones that name the choice.
    async fn plain_transfer(
        store: &WorkflowStore,
        kind: &str,
        id: &str,
        from: &str,
        to: &str,
    ) -> Result<Transferred, TransferFailed> {
        store
            .transfer(kind, id, from, to, OutgoingAccess::Nothing, from)
            .await
    }

    /// Every grantee of one definition, as the store's own reader returns them,
    /// ordered so a comparison is stable.
    async fn grantees(store: &WorkflowStore, kind: &str, id: &str, owner: &str) -> Vec<Grant> {
        store.list_shares(kind, id, owner).await.expect("shares")
    }

    async fn count(
        pool: &PostgresPool,
        sql: &str,
        params: &[&(dyn tokio_postgres::types::ToSql + Sync)],
    ) -> i64 {
        pool.get()
            .await
            .expect("conn")
            .query_one(sql, params)
            .await
            .expect("count")
            .get(0)
    }

    fn user_grant(id: &str, permission: GrantLevel) -> Grant {
        Grant {
            principal_type: PrincipalKind::User,
            principal_id: id.to_string(),
            permission,
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn erasing_a_definition_takes_its_whole_history_and_the_marker_moves() {
        with_store_pool(|s, pool| async move {
            save_owned(&s, KIND_WORKFLOW, "wf1", "alice", TOML).await;
            save_owned(&s, KIND_WORKFLOW, "wf2", "alice", TOML).await;
            s.set_shares(
                KIND_WORKFLOW,
                "wf1",
                "alice",
                &[user_grant("bob", GrantLevel::Run)],
                "alice",
            )
            .await
            .unwrap();
            let before = s.change_marker(KIND_WORKFLOW).await.unwrap();

            assert!(s.erase(KIND_WORKFLOW, "wf1", "alice").await.unwrap());
            assert!(!s.erase(KIND_WORKFLOW, "wf1", "alice").await.unwrap());
            assert!(def_row(&pool, KIND_WORKFLOW, "wf1", "alice")
                .await
                .is_none());
            assert_eq!(
                count(
                    &pool,
                    "SELECT count(*) FROM def_revisions WHERE ref_id = 'wf1'",
                    &[]
                )
                .await,
                0,
                "no tombstone: earlier revisions were the person's content"
            );
            assert_eq!(
                count(&pool, "SELECT count(*) FROM def_shares", &[]).await,
                0
            );
            assert_ne!(
                s.change_marker(KIND_WORKFLOW).await.unwrap(),
                before,
                "a deletion that leaves the newest row in place still moves the marker"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_merge_carries_references_and_a_delete_drops_them() {
        with_store_pool(|s, pool| async move {
            save_owned(&s, KIND_WORKFLOW, "carols", "carol", TOML).await;
            save_owned(&s, KIND_WORKFLOW, "bobs", "bob", TOML).await;
            pool.get()
                .await
                .unwrap()
                .execute(
                    "UPDATE workflow_defs SET run_as_identity_id = 'alice', last_modified_by = 'alice' \
                     WHERE id = 'carols'",
                    &[],
                )
                .await
                .unwrap();
            s.set_shares(
                KIND_WORKFLOW,
                "carols",
                "carol",
                &[
                    user_grant("alice", GrantLevel::Manage),
                    user_grant("bob", GrantLevel::Run),
                ],
                "carol",
            )
            .await
            .unwrap();
            s.set_shares(KIND_WORKFLOW, "bobs", "bob", &[user_grant("alice", GrantLevel::Run)], "bob")
                .await
                .unwrap();

            assert_eq!(
                s.reference_holders().await.unwrap(),
                std::collections::BTreeMap::from([("alice".to_string(), 3), ("bob".to_string(), 1)]),
                "one definition runs as alice and two grants name her"
            );
            assert_eq!(
                s.references_to("alice").await.unwrap(),
                [
                    ("workflow".to_string(), "bobs".to_string(), "bob".to_string()),
                    ("workflow".to_string(), "carols".to_string(), "carol".to_string()),
                ]
            );
            s.reassign_references("alice", "bob").await.unwrap();
            assert!(s.references_to("alice").await.unwrap().is_empty());
            let carols = def_row(&pool, KIND_WORKFLOW, "carols", "carol").await.unwrap();
            assert_eq!(carols.get::<_, Option<String>>("run_as_identity_id").as_deref(), Some("bob"));
            assert_eq!(carols.get::<_, Option<String>>("last_modified_by").as_deref(), Some("bob"));
            assert_eq!(
                grantees(&s, KIND_WORKFLOW, "carols", "carol").await,
                [user_grant("bob", GrantLevel::Manage)],
                "the stronger of the two grants is kept"
            );
            assert!(
                grantees(&s, KIND_WORKFLOW, "bobs", "bob").await.is_empty(),
                "ownership subsumes a grant"
            );

            s.drop_references("bob").await.unwrap();
            let carols = def_row(&pool, KIND_WORKFLOW, "carols", "carol").await.unwrap();
            assert_eq!(carols.get::<_, Option<String>>("run_as_identity_id"), None);
            assert_eq!(
                carols.get::<_, Option<String>>("last_modified_by").as_deref(),
                Some("bob"),
                "still tainted until its owner saves it"
            );
            assert!(grantees(&s, KIND_WORKFLOW, "carols", "carol").await.is_empty());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn an_adopted_merge_keeps_its_run_identity_and_counts_as_its_owners() {
        with_store_pool(|s, pool| async move {
            save_owned(&s, KIND_WORKFLOW, "wf1", "alice", TOML).await;
            plain_transfer(&s, KIND_WORKFLOW, "wf1", "alice", "bob")
                .await
                .unwrap();
            assert!(s
                .adopt_merged(KIND_WORKFLOW, "wf1", "bob", Some("carol"))
                .await
                .unwrap());
            let row = def_row(&pool, KIND_WORKFLOW, "wf1", "bob").await.unwrap();
            assert_eq!(
                row.get::<_, Option<String>>("run_as_identity_id")
                    .as_deref(),
                Some("carol")
            );
            assert_eq!(
                row.get::<_, Option<String>>("last_modified_by").as_deref(),
                Some("bob")
            );
        })
        .await;
    }

    /// The whole of the row travels — including whatever columns a later
    /// migration adds — and only the three a transfer decides change.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_transfer_carries_the_row_over_and_changes_only_what_it_must() {
        with_store_pool(|s, pool| async move {
            save_owned(&s, KIND_WORKFLOW, "wf1", "alice", TOML).await;
            // The cached parse is the one column a `to_jsonb`/`jsonb_populate_record`
            // round-trip could quietly flatten, and the monitor binding is read
            // straight out of it, so it carries every shape jsonb has.
            let parsed = serde_json::json!({
                "monitor": { "binding": { "surface": "slack", "channel": "c1" } },
                "steps": [1, -2.5, "three", null, true, {"nested": []}],
                "absent": null,
            });
            s.save(
                "wf1",
                KIND_WORKFLOW,
                TOML,
                Some(parsed.clone()),
                Some("alice"),
                Visibility::Team,
                RunAsWrite::Identity("carol".into()),
                EditContext::new("web", Some("alice")),
            )
            .await
            .expect("run_as");
            let before = def_row(&pool, KIND_WORKFLOW, "wf1", "alice")
                .await
                .expect("source row");

            let moved = plain_transfer(&s, KIND_WORKFLOW, "wf1", "alice", "bob")
                .await
                .expect("transfer");

            assert_eq!(
                moved,
                Transferred {
                    trigger_paths: Vec::new(),
                    shares: 0,
                    revisions: 1,
                    kept_manage: false,
                }
            );
            assert!(def_row(&pool, KIND_WORKFLOW, "wf1", "alice")
                .await
                .is_none());
            let after = def_row(&pool, KIND_WORKFLOW, "wf1", "bob")
                .await
                .expect("moved row");
            for column in ["toml_source", "visibility", "last_modified_by", "agent_id"] {
                assert_eq!(
                    after.get::<_, Option<String>>(column),
                    before.get::<_, Option<String>>(column),
                    "{column} must survive the move"
                );
            }
            assert_eq!(
                after.get::<_, Option<serde_json::Value>>("parsed_json"),
                Some(parsed),
                "jsonb must survive the round-trip through the record, value for value"
            );
            assert_eq!(
                after.get::<_, DateTime<Utc>>("created_at"),
                before.get::<_, DateTime<Utc>>("created_at")
            );
            assert!(
                after.get::<_, DateTime<Utc>>("updated_at")
                    > before.get::<_, DateTime<Utc>>("updated_at"),
                "the replicas rebuild only when the change marker moves"
            );
            assert_eq!(
                after.get::<_, Option<String>>("run_as_identity_id"),
                None,
                "a transferred definition blocks as tainted, never as inactive"
            );
            assert_eq!(
                s.list(KIND_WORKFLOW).await.expect("list")[0]
                    .owner
                    .as_deref(),
                Some("bob")
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_definition_the_named_owner_does_not_hold_is_not_found() {
        with_store(|s| async move {
            save_owned(&s, KIND_WORKFLOW, "wf1", "alice", TOML).await;

            assert!(matches!(
                plain_transfer(&s, KIND_WORKFLOW, "wf1", "bob", "carol").await,
                Err(TransferFailed::NotFound)
            ));
            assert!(matches!(
                plain_transfer(&s, KIND_WORKFLOW, "missing", "alice", "bob").await,
                Err(TransferFailed::NotFound)
            ));
        })
        .await;
    }

    /// Revision history survives deletion and is exempt from retention, so a
    /// target who once owned this id still holds a series while holding no
    /// definition. Both histories must survive, in order, with provenance
    /// intact.
    #[tokio::test(flavor = "multi_thread")]
    async fn revisions_are_renumbered_above_a_series_the_target_already_holds() {
        with_store(|s| async move {
            // Bob's own history for the same id, ending in a tombstone.
            save_owned(&s, KIND_WORKFLOW, "wf1", "bob", "bob v1").await;
            save_owned(&s, KIND_WORKFLOW, "wf1", "bob", "bob v2").await;
            s.delete("wf1", KIND_WORKFLOW, Some("bob"), edit("web", Some("bob")))
                .await
                .expect("delete");

            save_owned(&s, KIND_WORKFLOW, "wf1", "alice", "alice v1").await;
            save_owned(&s, KIND_WORKFLOW, "wf1", "alice", "alice v2").await;
            s.save(
                "wf1",
                KIND_WORKFLOW,
                "alice v1",
                None,
                Some("alice"),
                Visibility::Private,
                RunAsWrite::Unchanged,
                EditContext::restore(Some("alice"), 1),
            )
            .await
            .expect("restore");

            let moved = plain_transfer(&s, KIND_WORKFLOW, "wf1", "alice", "bob")
                .await
                .expect("transfer");

            assert_eq!(moved.revisions, 3);
            assert!(s
                .list_revisions(KIND_WORKFLOW, "wf1", Some("alice"))
                .await
                .expect("alice")
                .is_empty());
            let revs = s
                .list_revisions(KIND_WORKFLOW, "wf1", Some("bob"))
                .await
                .expect("bob");
            let numbered: Vec<(i32, Option<i32>, bool)> = revs
                .iter()
                .map(|r| (r.rev, r.restored_from, r.deleted))
                .collect();
            assert_eq!(
                numbered,
                vec![
                    (6, Some(4), false),
                    (5, None, false),
                    (4, None, false),
                    (3, None, true),
                    (2, None, false),
                    (1, None, false)
                ],
                "bob's tombstone keeps its place and alice's series sits above it"
            );
            let restored = s
                .get_revision(KIND_WORKFLOW, "wf1", Some("bob"), 6)
                .await
                .expect("read")
                .expect("restored revision");
            let source = s
                .get_revision(
                    KIND_WORKFLOW,
                    "wf1",
                    Some("bob"),
                    restored.restored_from.expect("provenance"),
                )
                .await
                .expect("read")
                .expect("its source");
            assert_eq!(
                source.content, restored.content,
                "provenance must not point into bob's old history"
            );
        })
        .await;
    }

    /// The assertion that fails loudly if anyone "simplifies" the move into a
    /// plain `UPDATE` of the owner column, which the composite foreign key
    /// refuses.
    #[tokio::test(flavor = "multi_thread")]
    async fn shares_follow_the_definition_and_the_foreign_key_holds() {
        with_store(|s| async move {
            save_owned(&s, KIND_WORKFLOW, "wf1", "alice", TOML).await;
            s.set_shares(
                KIND_WORKFLOW,
                "wf1",
                "alice",
                &[
                    Grant::user("carol", GrantLevel::Manage),
                    Grant::team("platform", GrantLevel::Run),
                ],
                "alice",
            )
            .await
            .expect("shares");

            let moved = plain_transfer(&s, KIND_WORKFLOW, "wf1", "alice", "bob")
                .await
                .expect("transfer");

            assert_eq!(moved.shares, 2);
            assert!(s
                .list_shares(KIND_WORKFLOW, "wf1", "alice")
                .await
                .expect("alice")
                .is_empty());
            assert_eq!(
                s.list_shares(KIND_WORKFLOW, "wf1", "bob")
                    .await
                    .expect("bob"),
                vec![
                    Grant::team("platform", GrantLevel::Run),
                    Grant::user("carol", GrantLevel::Manage),
                ]
            );
            // The coupling `manages_for_automation` rests on: a grantee's
            // standing is read against the *new* owner's namespace.
            assert_eq!(
                s.caller_share_permission(KIND_WORKFLOW, "wf1", "bob", Some("carol"), &[])
                    .await
                    .expect("permission"),
                Some(GrantLevel::Manage)
            );
        })
        .await;
    }

    /// Ownership subsumes every grant, so a grant naming the new owner is dead
    /// weight the old parent's cascade clears — but a team grant that happens
    /// to contain them reaches everyone else on that team and must not be
    /// touched.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_grant_naming_the_new_owner_is_left_behind_and_a_team_grant_is_not() {
        with_store(|s| async move {
            save_owned(&s, KIND_WORKFLOW, "wf1", "alice", TOML).await;
            s.set_shares(
                KIND_WORKFLOW,
                "wf1",
                "alice",
                &[
                    Grant::user("bob", GrantLevel::Manage),
                    Grant::user("alice", GrantLevel::Run),
                    Grant::team("platform", GrantLevel::Run),
                ],
                "alice",
            )
            .await
            .expect("shares");

            plain_transfer(&s, KIND_WORKFLOW, "wf1", "alice", "bob")
                .await
                .expect("transfer");

            assert_eq!(
                s.list_shares(KIND_WORKFLOW, "wf1", "bob").await.expect("bob"),
                vec![
                    Grant::team("platform", GrantLevel::Run),
                    Grant::user("alice", GrantLevel::Run),
                ],
                "the outgoing owner's grant is somebody's deliberate choice; the new owner's is not"
            );
        })
        .await;
    }

    /// The keep-access choice rides into the move's own transaction. Ticked,
    /// the outgoing owner is left exactly one `manage` grant — the inert
    /// self-grant that predates the owner-is-never-a-grantee rule is *raised*
    /// rather than doubled, which is why the write happens after the move and
    /// not before it — and every other grantee is exactly as it was. Unticked,
    /// the same move touches nothing it was not already moving.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_kept_manage_grant_lands_inside_the_move_and_disturbs_no_other() {
        with_store_pool(|s, pool| async move {
            let held = [
                Grant::user("carol", GrantLevel::Run),
                Grant::user("alice", GrantLevel::Run),
                Grant::team("platform", GrantLevel::Manage),
            ];
            for id in ["wf1", "wf2"] {
                save_owned(&s, KIND_WORKFLOW, id, "alice", TOML).await;
                s.set_shares(KIND_WORKFLOW, id, "alice", &held, "alice")
                    .await
                    .expect("shares");
            }

            let kept = s
                .transfer(
                    KIND_WORKFLOW,
                    "wf1",
                    "alice",
                    "bob",
                    OutgoingAccess::Manage,
                    "alice",
                )
                .await
                .expect("transfer");

            assert!(kept.kept_manage);
            assert_eq!(
                grantees(&s, KIND_WORKFLOW, "wf1", "bob").await,
                vec![
                    Grant::team("platform", GrantLevel::Manage),
                    Grant::user("alice", GrantLevel::Manage),
                    Grant::user("carol", GrantLevel::Run),
                ],
                "one manage grant for the outgoing owner, every other grantee intact"
            );
            assert_eq!(
                count(
                    &pool,
                    "SELECT count(*) FROM def_shares \
                     WHERE kind = $1 AND def_id = $2 AND principal_id = $3",
                    &[&KIND_WORKFLOW, &"wf1", &"alice"],
                )
                .await,
                1,
                "the outgoing owner is named once across every namespace"
            );

            let plain = plain_transfer(&s, KIND_WORKFLOW, "wf2", "alice", "bob")
                .await
                .expect("transfer");

            assert!(!plain.kept_manage);
            assert_eq!(
                grantees(&s, KIND_WORKFLOW, "wf2", "bob").await,
                vec![
                    Grant::team("platform", GrantLevel::Manage),
                    Grant::user("alice", GrantLevel::Run),
                    Grant::user("carol", GrantLevel::Run),
                ],
                "an unticked transfer grants nothing and raises nothing"
            );
        })
        .await;
    }

    /// A kept grant is part of the move rather than a step beside it, so a
    /// transfer that fails after writing it leaves none behind. The failure is
    /// forced where one is genuinely reachable: the renumbering adds the
    /// target's highest `rev` to every moved revision, and a tombstone topped
    /// out at `i32::MAX` — history the target's own deletion left behind —
    /// overflows the column, which is the step straight after the grant.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_rolled_back_transfer_leaves_no_kept_grant() {
        with_store_pool(|s, pool| async move {
            save_owned(&s, KIND_WORKFLOW, "wf1", "alice", TOML).await;
            pool.get()
                .await
                .expect("conn")
                .execute(
                    "INSERT INTO def_revisions \
                     (kind, ref_id, owner_identity_id, rev, content, content_hash, \
                      deleted, edited_via) \
                     VALUES ($1, $2, $3, $4, '', '', TRUE, 'test')",
                    &[&KIND_WORKFLOW, &"wf1", &"bob", &i32::MAX],
                )
                .await
                .expect("tombstone");

            assert!(
                matches!(
                    s.transfer(
                        KIND_WORKFLOW,
                        "wf1",
                        "alice",
                        "bob",
                        OutgoingAccess::Manage,
                        "alice",
                    )
                    .await,
                    Err(TransferFailed::Storage(_))
                ),
                "the renumbering must overflow rather than silently truncate"
            );

            for owner in ["alice", "bob"] {
                assert!(
                    grantees(&s, KIND_WORKFLOW, "wf1", owner).await.is_empty(),
                    "a transfer that rolled back leaves {owner} no grant"
                );
            }
            assert!(
                def_row(&pool, KIND_WORKFLOW, "wf1", "alice")
                    .await
                    .is_some(),
                "and leaves the definition where it was"
            );
        })
        .await;
    }

    async fn seed_monitor_state(pool: &PostgresPool, id: &str, owner: &str, message: &str) {
        let conn = pool.get().await.expect("conn");
        conn.execute(
            "INSERT INTO monitor_dispatches \
             (monitor_id, owner, surface, channel, message_id, payload) \
             VALUES ($1, $2, 'slack', 'c1', $3, '{}'::jsonb)",
            &[&id, &owner, &message],
        )
        .await
        .expect("dispatch");
        conn.execute(
            "INSERT INTO monitor_cursors (monitor_id, owner, surface, channel, last_swept_ts) \
             VALUES ($1, $2, 'slack', 'c1', '1')",
            &[&id, &owner],
        )
        .await
        .expect("cursor");
        conn.execute(
            "INSERT INTO monitor_channel_blocks \
             (monitor_id, owner, agent_id, surface, channel, error) \
             VALUES ($1, $2, 'a1', 'slack', 'c1', 'not in channel')",
            &[&id, &owner],
        )
        .await
        .expect("block");
        conn.execute(
            "INSERT INTO monitor_source_state (monitor_id, owner, error, failures) \
             VALUES ($1, $2, 'boom', 2)",
            &[&id, &owner],
        )
        .await
        .expect("source state");
    }

    /// All four satellites move, the global pre-V018 cursor does not, and the
    /// dispatch rows `delete_monitor` never clears are purged rather than
    /// colliding — never merged, which would discard the transferred monitor's
    /// own idempotency records.
    #[tokio::test(flavor = "multi_thread")]
    async fn monitor_satellites_move_and_the_targets_orphans_are_purged() {
        with_store_pool(|s, pool| async move {
            save_owned(&s, KIND_MONITOR, "m1", "alice", TOML).await;
            seed_monitor_state(&pool, "m1", "alice", "msg-alice").await;
            seed_monitor_state(&pool, "m1", "bob", "msg-bob-orphan").await;
            pool.get()
                .await
                .expect("conn")
                .execute(
                    "INSERT INTO monitor_channel_cursors (surface, channel, last_swept_ts) \
                     VALUES ('slack', 'c1', '7')",
                    &[],
                )
                .await
                .expect("global cursor");

            plain_transfer(&s, KIND_MONITOR, "m1", "alice", "bob")
                .await
                .expect("transfer");

            for table in MONITOR_SATELLITES {
                assert_eq!(
                    count(
                        &pool,
                        &format!("SELECT count(*) FROM {table} WHERE monitor_id = 'm1' AND owner = 'alice'"),
                        &[],
                    )
                    .await,
                    0,
                    "{table} must not be left behind"
                );
            }
            assert_eq!(
                count(
                    &pool,
                    "SELECT count(*) FROM monitor_dispatches WHERE monitor_id = 'm1' AND owner = 'bob'",
                    &[],
                )
                .await,
                1,
                "the orphan is purged and the moved dispatch is kept"
            );
            assert_eq!(
                pool.get()
                    .await
                    .expect("conn")
                    .query_one(
                        "SELECT message_id FROM monitor_dispatches WHERE monitor_id = 'm1'",
                        &[],
                    )
                    .await
                    .expect("dispatch")
                    .get::<_, String>("message_id"),
                "msg-alice"
            );
            assert_eq!(
                count(&pool, "SELECT count(*) FROM monitor_channel_cursors", &[]).await,
                1,
                "the pre-V018 global cursor is keyed by neither monitor nor owner"
            );
        })
        .await;
    }

    /// A definition of the same name in the target's hands refuses the whole
    /// transfer, and the source keeps every row it had.
    #[tokio::test(flavor = "multi_thread")]
    async fn a_collision_is_refused_and_not_one_row_moves() {
        with_store_pool(|s, pool| async move {
            save_owned(&s, KIND_MONITOR, "m1", "alice", TOML).await;
            save_owned(&s, KIND_MONITOR, "m1", "bob", "bob's own").await;
            s.set_shares(
                KIND_MONITOR,
                "m1",
                "alice",
                &[Grant::user("carol", GrantLevel::Run)],
                "alice",
            )
            .await
            .expect("shares");
            seed_monitor_state(&pool, "m1", "alice", "msg-alice").await;
            seed_monitor_state(&pool, "m1", "bob", "msg-bob").await;
            let endpoints = ensure(
                &s,
                &HashSet::from([TriggerBinding {
                    kind: KIND_MONITOR.to_string(),
                    def_id: "m1".to_string(),
                    owner: Some("alice".to_string()),
                    trigger_id: "deploy".to_string(),
                }]),
            )
            .await;

            assert!(matches!(
                plain_transfer(&s, KIND_MONITOR, "m1", "alice", "bob").await,
                Err(TransferFailed::Occupied)
            ));

            assert!(def_row(&pool, KIND_MONITOR, "m1", "alice").await.is_some());
            assert_eq!(
                s.list_shares(KIND_MONITOR, "m1", "alice").await.expect("shares").len(),
                1
            );
            assert_eq!(
                s.list_revisions(KIND_MONITOR, "m1", Some("alice")).await.expect("revs").len(),
                1
            );
            for table in MONITOR_SATELLITES {
                assert_eq!(
                    count(
                        &pool,
                        &format!("SELECT count(*) FROM {table} WHERE monitor_id = 'm1' AND owner = 'bob'"),
                        &[],
                    )
                    .await,
                    1,
                    "{table}: bob's live monitor keeps its state"
                );
            }
            assert_eq!(endpoint_count(&pool).await, 1);
            assert_eq!(
                pool.get()
                    .await
                    .expect("conn")
                    .query_one("SELECT owner_identity_id FROM trigger_endpoints", &[])
                    .await
                    .expect("endpoint")
                    .get::<_, String>("owner_identity_id"),
                "alice",
                "the endpoint stayed with the definition that still owns it"
            );
            assert_eq!(endpoints.len(), 1);
        })
        .await;
    }

    /// The address external callers hold is minted against the binding, not
    /// derived from the owner, so a transfer moves the row and leaves the path
    /// exactly where it was. An orphan the target left behind is cleared first
    /// rather than blocking a transfer over a definition nobody can see.
    #[tokio::test(flavor = "multi_thread")]
    async fn trigger_endpoints_follow_the_definition_with_their_paths_unchanged() {
        with_store_pool(|s, pool| async move {
            save_owned(&s, KIND_WORKFLOW, "wf1", "alice", TOML).await;
            let bindings = |owner: &str| {
                HashSet::from([
                    binding("wf1", Some(owner), "deploy"),
                    binding("wf1", Some(owner), "notify"),
                ])
            };
            let before = ensure(&s, &bindings("alice")).await;
            // An address bob's deleted workflow of the same name left behind.
            ensure(&s, &HashSet::from([binding("wf1", Some("bob"), "deploy")])).await;

            let moved = plain_transfer(&s, KIND_WORKFLOW, "wf1", "alice", "bob")
                .await
                .expect("transfer");

            assert_eq!(
                moved.trigger_paths.iter().collect::<HashSet<_>>(),
                before.values().collect::<HashSet<_>>(),
                "a transfer must not move a public path"
            );
            assert_eq!(ensure(&s, &bindings("bob")).await, {
                let mut expected = HashMap::new();
                for (b, path) in &before {
                    expected.insert(binding("wf1", Some("bob"), &b.trigger_id), path.clone());
                }
                expected
            });
            assert_eq!(
                endpoint_count(&pool).await,
                2,
                "the orphan was purged, nothing re-minted"
            );
        })
        .await;
    }

    /// Two members handing the same id to one person at once. The step-2
    /// pre-flight reads a row that is not there, so it takes no lock and both
    /// pass it; the primary key decides, and the loser is owed the refusal it
    /// would have had a moment earlier rather than a storage failure.
    #[tokio::test(flavor = "multi_thread")]
    async fn one_of_two_simultaneous_transfers_onto_one_name_is_refused_as_occupied() {
        with_store(|s| async move {
            save_owned(&s, KIND_WORKFLOW, "wf1", "alice", "alice's").await;
            save_owned(&s, KIND_WORKFLOW, "wf1", "carol", "carol's").await;
            let hand_over = |from: &'static str| {
                let s = Arc::clone(&s);
                async move { plain_transfer(&s, KIND_WORKFLOW, "wf1", from, "bob").await }
            };

            let (a, b) = tokio::join!(hand_over("alice"), hand_over("carol"));

            let refused = match (a, b) {
                (Ok(_), Err(refused)) | (Err(refused), Ok(_)) => refused,
                other => panic!("exactly one may land: {other:?}"),
            };
            assert!(
                matches!(refused, TransferFailed::Occupied),
                "the loser names the clash, never a storage failure: {refused}"
            );
            assert_eq!(
                s.list(KIND_WORKFLOW).await.expect("list").len(),
                2,
                "the loser keeps its row"
            );
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_transfer_moves_one_kind_and_leaves_the_same_id_in_another_alone() {
        with_store(|s| async move {
            save_owned(&s, KIND_PLAYBOOK, "group", "alice", "PB").await;
            save_owned(&s, KIND_CONSOLIDATION_TEMPLATE, "group", "alice", "CT").await;

            plain_transfer(&s, KIND_PLAYBOOK, "group", "alice", "bob")
                .await
                .expect("transfer");

            assert_eq!(
                s.get("group", KIND_PLAYBOOK, Some("bob"))
                    .await
                    .expect("get")
                    .as_deref(),
                Some("PB")
            );
            assert!(s
                .get("group", KIND_PLAYBOOK, Some("alice"))
                .await
                .expect("get")
                .is_none());
            assert_eq!(
                s.get("group", KIND_CONSOLIDATION_TEMPLATE, Some("alice"))
                    .await
                    .expect("get")
                    .as_deref(),
                Some("CT"),
                "a consolidation template is covered by the kind-agnostic move, not by it"
            );
        })
        .await;
    }
}
