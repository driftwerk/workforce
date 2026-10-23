//! Postgres backend: vault entries as hub-encrypted rows in the shared
//! database. Each process caches the
//! scopes it has open and polls `vault_scopes.generation` for other
//! processes' writes; its own writes are visible as soon as they commit.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::future::Future;
use std::sync::{Arc, Weak};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use deadpool_postgres::GenericClient;
use eyre::{bail, eyre, Result, WrapErr};
use parking_lot::{Mutex, RwLock};
use sha2::{Digest, Sha256};
use tokio::sync::Semaphore;
use tokio::time::MissedTickBehavior;
use tokio_postgres::{IsolationLevel, Row};
use uuid::Uuid;
use workforce_common::secret_lock::{LocalRefreshLock, RefreshGuard, RefreshLock};
use workforce_common::settings::{VaultActor, VaultConfig, VaultEntry};
use workforce_postgres::{
    MigrationRun, MigrationRunner, MigrationSet, PostgresConfig, PostgresError, PostgresPool,
};
use zeroize::Zeroizing;

use super::crypto::{Dek, KdfParams, Kek};
use super::{plan, Plan, Vault, VaultError, VaultOp};
use crate::hub_core::vault_audit::AuditEntry;

mod embedded {
    refinery::embed_migrations!("migrations_vault");
}

const SCHEMA: &str = "vault";
const REFRESH_LOCK_WAIT: Duration = Duration::from_secs(30);
const REFRESH_LOCK_CONNECTIONS: usize = 8;
const PRUNE_EVERY: Duration = Duration::from_secs(3600);

pub(super) fn build_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
    PostgresPool::from_config_with_session_init(config, &["SET search_path TO vault, public"])
}

/// A pool on which every transaction is read-only: for a reader beside a
/// live hub.
pub(super) fn build_read_only_pool(config: &PostgresConfig) -> Result<PostgresPool, PostgresError> {
    PostgresPool::from_config_with_session_init(
        config,
        &[
            "SET search_path TO vault, public",
            workforce_postgres::READ_ONLY_SESSION,
        ],
    )
}

async fn run_migrations(pool: &PostgresPool) -> Result<()> {
    let set = MigrationSet::new(
        SCHEMA,
        Box::new(EmbeddedVaultMigrations) as Box<dyn MigrationRun>,
    );
    MigrationRunner::run(pool, &[set])
        .await
        .wrap_err("running vault migrations")
}

struct EmbeddedVaultMigrations;

#[async_trait]
impl MigrationRun for EmbeddedVaultMigrations {
    async fn run(&self, client: &mut tokio_postgres::Client) -> Result<(), PostgresError> {
        embedded::migrations::runner()
            .run_async(client)
            .await
            .map_err(PostgresError::from)?;
        Ok(())
    }
}

/// Tunables from `[vault]`.
#[derive(Debug, Clone, Copy)]
pub(super) struct PgSettings {
    pub sync_interval: Duration,
    pub audit_retention_days: u32,
}

impl PgSettings {
    pub fn from_config(vault: &VaultConfig) -> Self {
        Self {
            sync_interval: Duration::from_millis(vault.sync_interval_ms.max(1)),
            audit_retention_days: vault.audit_retention_days,
        }
    }
}

/// KEKs by the salt and cost they were derived with.
type Keks = HashMap<(Vec<u8>, KdfParams), Arc<Kek>>;

/// A passphrase and the KEKs derived from it, one per salt.
struct KeySource {
    passphrase: Zeroizing<String>,
    keks: Mutex<Keks>,
}

impl KeySource {
    fn new(passphrase: String) -> Self {
        Self {
            passphrase: Zeroizing::new(passphrase),
            keks: Mutex::default(),
        }
    }

    async fn kek(&self, salt: Vec<u8>, params: KdfParams) -> Result<Arc<Kek>> {
        let cache_key = (salt, params);
        if let Some(kek) = self.keks.lock().get(&cache_key) {
            return Ok(Arc::clone(kek));
        }
        let passphrase = self.passphrase.clone();
        let salt = cache_key.0.clone();
        let kek = Arc::new(
            tokio::task::spawn_blocking(move || Kek::derive(&passphrase, salt, params)).await??,
        );
        self.keks.lock().insert(cache_key, Arc::clone(&kek));
        Ok(kek)
    }

    async fn fresh_kek(&self) -> Result<Arc<Kek>> {
        let passphrase = self.passphrase.clone();
        let kek = Arc::new(
            tokio::task::spawn_blocking(move || Kek::generate(&passphrase, KdfParams::default()))
                .await??,
        );
        self.keks
            .lock()
            .insert((kek.salt().to_vec(), kek.params()), Arc::clone(&kek));
        Ok(kek)
    }

    /// A KEK for every distinct salt and cost among `wraps`, skipping any cost
    /// that is not [`KdfParams::usable`].
    async fn keks_for<'w>(&self, wraps: impl IntoIterator<Item = &'w WrapRow>) -> Result<Keks> {
        let kdfs: Vec<_> = wraps
            .into_iter()
            .filter(|wrap| {
                let usable = wrap.params.usable();
                if !usable {
                    tracing::warn!(
                        wrap_id = %wrap.wrap_id,
                        "Skipping a vault key wrap whose key-derivation cost is invalid or too high"
                    );
                }
                usable
            })
            .map(WrapRow::kdf)
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        let derived = futures::future::try_join_all(
            kdfs.iter()
                .cloned()
                .map(|(salt, params)| self.kek(salt, params)),
        )
        .await?;
        Ok(kdfs.into_iter().zip(derived).collect())
    }
}

/// Every DEK the passphrase opens, and the one sealing new rows.
struct Keyring {
    active: Arc<Dek>,
    keys: HashMap<Uuid, Arc<Dek>>,
    /// The KEK that opened `active`; wraps keys this process creates.
    kek: Arc<Kek>,
}

impl Keyring {
    fn open_row(&self, scope: &str, row: &Row) -> Result<(String, VaultEntry)> {
        let key_id: Uuid = row.try_get("key_id")?;
        self.keys
            .get(&key_id)
            .ok_or_else(|| eyre!("sealed with unknown key {key_id}"))?
            .open_entry(
                scope,
                row.try_get("path_mac")?,
                row.try_get("nonce")?,
                row.try_get("ciphertext")?,
            )
    }
}

struct WrapRow {
    wrap_id: Uuid,
    key_id: Uuid,
    salt: Vec<u8>,
    params: KdfParams,
    nonce: Vec<u8>,
    wrapped: Vec<u8>,
}

impl WrapRow {
    fn from_row(row: &Row) -> Result<Self> {
        let cost =
            |column: &str| -> Result<u32> { Ok(u32::try_from(row.try_get::<_, i32>(column)?)?) };
        Ok(Self {
            wrap_id: row.try_get("wrap_id")?,
            key_id: row.try_get("key_id")?,
            salt: row.try_get("kdf_salt")?,
            params: KdfParams {
                m_kib: cost("kdf_m_kib")?,
                t: cost("kdf_t")?,
                p: cost("kdf_p")?,
            },
            nonce: row.try_get("nonce")?,
            wrapped: row.try_get("wrapped")?,
        })
    }

    fn kdf(&self) -> (Vec<u8>, KdfParams) {
        (self.salt.clone(), self.params)
    }

    /// The key this wrap holds and the KEK that opened it, when one of `keks`
    /// opens it.
    fn open(&self, keks: &Keks) -> Option<(Dek, Arc<Kek>)> {
        let kek = keks.get(&self.kdf())?;
        kek.unwrap(self.key_id, &self.nonce, &self.wrapped)
            .map(|dek| (dek, Arc::clone(kek)))
    }
}

async fn insert_wrap(client: &impl GenericClient, kek: &Kek, dek: &Dek) -> Result<()> {
    let wrapped = kek.wrap(dek)?;
    let params = kek.params();
    client
        .execute(
            "INSERT INTO vault_key_wraps
                 (wrap_id, key_id, kdf_salt, kdf_m_kib, kdf_t, kdf_p, nonce, wrapped)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8)",
            &[
                &Uuid::new_v4(),
                &dek.id(),
                &kek.salt(),
                &i32::try_from(params.m_kib)?,
                &i32::try_from(params.t)?,
                &i32::try_from(params.p)?,
                &wrapped.nonce.as_slice(),
                &wrapped.ciphertext,
            ],
        )
        .await?;
    Ok(())
}

async fn load_wraps(client: &impl GenericClient) -> Result<Vec<WrapRow>> {
    client
        .query(
            "SELECT wrap_id, key_id, kdf_salt, kdf_m_kib, kdf_t, kdf_p, nonce, wrapped
             FROM vault_key_wraps ORDER BY created_at",
            &[],
        )
        .await?
        .iter()
        .map(WrapRow::from_row)
        .collect()
}

/// Create the keyring on an empty store. Racing creators both insert a key,
/// but only one keyring row lands; the loser rolls its key back.
async fn bootstrap_keyring(pool: &PostgresPool, keys: &KeySource) -> Result<()> {
    let mut conn = pool.get().await?;
    if conn
        .query_opt("SELECT 1 FROM vault_keyring", &[])
        .await?
        .is_some()
    {
        return Ok(());
    }
    let dek = Dek::generate()?;
    let kek = keys.fresh_kek().await?;
    let tx = conn.transaction().await?;
    tx.execute("INSERT INTO vault_keys (key_id) VALUES ($1)", &[&dek.id()])
        .await?;
    insert_wrap(&tx, &kek, &dek).await?;
    let created = tx
        .execute(
            "INSERT INTO vault_keyring (active_key_id) VALUES ($1) ON CONFLICT (singleton) DO NOTHING",
            &[&dek.id()],
        )
        .await?
        == 1;
    if created {
        tx.commit().await?;
    } else {
        tx.rollback().await?;
    }
    Ok(())
}

async fn load_keyring(pool: &PostgresPool, keys: &KeySource) -> Result<Keyring> {
    let conn = pool.get().await?;
    let active_id: Uuid = conn
        .query_opt("SELECT active_key_id FROM vault_keyring", &[])
        .await?
        .ok_or_else(|| eyre!("the vault keyring has not been created"))?
        .try_get(0)?;
    let wraps = load_wraps(&conn).await?;
    drop(conn);
    let keks = keys.keks_for(&wraps).await?;

    let opened: Vec<(Uuid, Arc<Dek>, Arc<Kek>)> = wraps
        .iter()
        .filter_map(|wrap| {
            wrap.open(&keks)
                .map(|(dek, kek)| (wrap.key_id, Arc::new(dek), kek))
        })
        .collect();
    let (active, kek) = opened
        .iter()
        .find(|(id, ..)| *id == active_id)
        .map(|(_, dek, kek)| (Arc::clone(dek), Arc::clone(kek)))
        .ok_or_else(|| {
            eyre!("the configured vault passphrase does not open the active vault key")
        })?;
    Ok(Keyring {
        active,
        keys: opened.into_iter().map(|(id, dek, _)| (id, dek)).collect(),
        kek,
    })
}

struct Snapshot {
    generation: i64,
    entries: HashMap<String, VaultEntry>,
}

/// One open scope's decrypted entries, as of `generation`.
struct ScopeCache {
    scope: String,
    snapshot: RwLock<Snapshot>,
}

impl ScopeCache {
    fn generation(&self) -> i64 {
        self.snapshot.read().generation
    }

    /// Install `next` unless the cache already holds one at least as new.
    fn install(&self, next: Snapshot) {
        let mut current = self.snapshot.write();
        if next.generation > current.generation {
            *current = next;
        }
    }

    /// Apply this process's own commit in place. `false` when a peer's commit
    /// landed in between, so only a reload gives a consistent view.
    fn apply_committed(&self, generation: i64, changes: &[(String, Option<VaultEntry>)]) -> bool {
        let mut current = self.snapshot.write();
        if generation <= current.generation {
            return true;
        }
        if generation != current.generation + 1 {
            return false;
        }
        changes.iter().for_each(|(path, entry)| match entry {
            Some(entry) => {
                current.entries.insert(path.clone(), entry.clone());
            }
            None => {
                current.entries.remove(path);
            }
        });
        current.generation = generation;
        true
    }
}

struct Committed {
    existed: Vec<Vec<bool>>,
    changes: HashMap<String, Vec<(String, Option<VaultEntry>)>>,
    generations: Vec<(String, i64)>,
}

/// One process's connection to the Postgres vault: pool, keyring, and every
/// scope it has open.
pub(super) struct PgStore {
    pool: PostgresPool,
    keys: KeySource,
    keyring: RwLock<Arc<Keyring>>,
    caches: Mutex<Vec<Weak<ScopeCache>>>,
    vaults: Mutex<HashMap<String, Weak<Vault>>>,
    /// This process's refresh-lock waiters queue here, so only one per key
    /// waits in Postgres.
    refresh_queue: Arc<LocalRefreshLock>,
    /// Bounds the connections this process's refresh locks hold at once.
    refresh_connections: Arc<Semaphore>,
    runtime: tokio::runtime::Handle,
    settings: PgSettings,
}

impl PgStore {
    /// Migrate, open (or create) the keyring, and start the sync loop.
    pub async fn connect(
        pool: PostgresPool,
        passphrase: String,
        settings: PgSettings,
    ) -> Result<Arc<Self>> {
        run_migrations(&pool).await?;
        let keys = KeySource::new(passphrase);
        bootstrap_keyring(&pool, &keys).await?;
        let keyring = load_keyring(&pool, &keys).await?;
        let store = Arc::new(Self {
            pool,
            keys,
            keyring: RwLock::new(Arc::new(keyring)),
            caches: Mutex::default(),
            vaults: Mutex::default(),
            refresh_queue: Arc::default(),
            refresh_connections: Arc::new(Semaphore::new(REFRESH_LOCK_CONNECTIONS)),
            runtime: tokio::runtime::Handle::current(),
            settings,
        });
        spawn_sync_loop(&store);
        Ok(store)
    }

    /// The store as a reader beside a live hub opens it, over
    /// [`build_read_only_pool`]: the keyring as it stands. Nothing is
    /// migrated, no keyring is created, and no sync loop runs, so nothing is
    /// pruned; a vault opened from it is the scope as it was when opened.
    pub async fn connect_read_only(
        pool: PostgresPool,
        passphrase: String,
        settings: PgSettings,
    ) -> Result<Arc<Self>> {
        let keys = KeySource::new(passphrase);
        let keyring = load_keyring(&pool, &keys).await?;
        Ok(Arc::new(Self {
            pool,
            keys,
            keyring: RwLock::new(Arc::new(keyring)),
            caches: Mutex::default(),
            vaults: Mutex::default(),
            refresh_queue: Arc::default(),
            refresh_connections: Arc::new(Semaphore::new(REFRESH_LOCK_CONNECTIONS)),
            runtime: tokio::runtime::Handle::current(),
            settings,
        }))
    }

    /// Whether `scope` has been imported or created in Postgres.
    pub async fn scope_initialised(&self, scope: &str) -> Result<bool> {
        Ok(self
            .pool
            .get()
            .await?
            .query_opt(
                "SELECT 1 FROM vault_scopes WHERE scope = $1 AND imported_at IS NOT NULL",
                &[&scope],
            )
            .await?
            .is_some())
    }

    /// Create `scope` empty — only for scopes with no legacy file to import.
    pub async fn init_scope(&self, scope: &str) -> Result<()> {
        self.pool
            .get()
            .await?
            .execute(
                "INSERT INTO vault_scopes (scope, imported_at) VALUES ($1, now())
                 ON CONFLICT (scope) DO UPDATE
                 SET imported_at = COALESCE(vault_scopes.imported_at, EXCLUDED.imported_at)",
                &[&scope],
            )
            .await?;
        Ok(())
    }

    /// The live vault for `scope`, opened on first use.
    pub async fn vault(self: &Arc<Self>, scope: &str) -> Result<Arc<Vault>> {
        if let Some(vault) = self.live_vault(scope) {
            return Ok(vault);
        }
        let cache = Arc::new(ScopeCache {
            scope: scope.to_owned(),
            snapshot: RwLock::new(self.snapshot(scope).await?),
        });
        let opened = Arc::new(Vault::from_postgres(PgScope {
            store: Arc::clone(self),
            cache: Arc::clone(&cache),
        }));
        let mut vaults = self.vaults.lock();
        if let Some(raced) = vaults.get(scope).and_then(Weak::upgrade) {
            return Ok(raced);
        }
        vaults.insert(scope.to_owned(), Arc::downgrade(&opened));
        self.caches.lock().push(Arc::downgrade(&cache));
        Ok(opened)
    }

    fn live_vault(&self, scope: &str) -> Option<Arc<Vault>> {
        let mut vaults = self.vaults.lock();
        vaults.retain(|_, vault| vault.strong_count() > 0);
        vaults.get(scope).and_then(Weak::upgrade)
    }

    fn live_caches(&self) -> Vec<Arc<ScopeCache>> {
        let mut caches = self.caches.lock();
        caches.retain(|cache| cache.strong_count() > 0);
        caches.iter().filter_map(Weak::upgrade).collect()
    }

    fn keyring(&self) -> Arc<Keyring> {
        Arc::clone(&self.keyring.read())
    }

    async fn refresh_keyring(&self) -> Result<()> {
        let keyring = load_keyring(&self.pool, &self.keys).await?;
        *self.keyring.write() = Arc::new(keyring);
        Ok(())
    }

    /// The keyring, refreshed once if it lacks a key some row is sealed with.
    async fn keyring_for(&self, key_ids: BTreeSet<Uuid>) -> Result<Arc<Keyring>> {
        let covers = |keyring: &Keyring| {
            key_ids
                .iter()
                .find(|id| !keyring.keys.contains_key(id))
                .copied()
        };
        let keyring = self.keyring();
        if covers(&keyring).is_none() {
            return Ok(keyring);
        }
        self.refresh_keyring().await?;
        let keyring = self.keyring();
        match covers(&keyring) {
            Some(missing) => bail!(
                "vault rows are sealed with key {missing}, which the configured passphrase cannot open"
            ),
            None => Ok(keyring),
        }
    }

    /// A consistent view of `scope` at one generation.
    async fn snapshot(&self, scope: &str) -> Result<Snapshot> {
        let mut conn = self.pool.get().await?;
        let tx = conn
            .build_transaction()
            .isolation_level(IsolationLevel::RepeatableRead)
            .read_only(true)
            .start()
            .await?;
        let generation: i64 = tx
            .query_opt(
                "SELECT generation FROM vault_scopes WHERE scope = $1",
                &[&scope],
            )
            .await?
            .ok_or_else(|| eyre!("vault scope `{scope}` is not initialised"))?
            .try_get(0)?;
        let rows = tx
            .query(
                "SELECT path_mac, key_id, nonce, ciphertext FROM vault_entries WHERE scope = $1",
                &[&scope],
            )
            .await?;
        tx.commit().await?;
        let key_ids = rows
            .iter()
            .map(|row| row.try_get("key_id"))
            .collect::<Result<BTreeSet<Uuid>, _>>()?;
        let keyring = self.keyring_for(key_ids).await?;
        Ok(Snapshot {
            generation,
            entries: rows
                .iter()
                .filter_map(|row| open_logged(&keyring, scope, row))
                .collect(),
        })
    }

    async fn reload(&self, cache: &ScopeCache) -> Result<()> {
        cache.install(self.snapshot(&cache.scope).await?);
        Ok(())
    }

    async fn sync_scope(&self, cache: &ScopeCache) -> Result<()> {
        let generation: i64 = self
            .pool
            .get()
            .await?
            .query_one(
                "SELECT generation FROM vault_scopes WHERE scope = $1",
                &[&cache.scope],
            )
            .await?
            .try_get(0)?;
        if generation > cache.generation() {
            self.reload(cache).await?;
        }
        Ok(())
    }

    async fn sync_live(&self) -> Result<()> {
        let caches = self.live_caches();
        if caches.is_empty() {
            return Ok(());
        }
        let scopes: Vec<String> = caches
            .iter()
            .map(|cache| cache.scope.clone())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        let generations: HashMap<String, i64> = self
            .pool
            .get()
            .await?
            .query(
                "SELECT scope, generation FROM vault_scopes WHERE scope = ANY($1)",
                &[&scopes],
            )
            .await?
            .iter()
            .map(|row| Ok((row.try_get(0)?, row.try_get(1)?)))
            .collect::<Result<_, tokio_postgres::Error>>()?;
        for cache in caches.iter().filter(|cache| {
            generations
                .get(&cache.scope)
                .is_some_and(|g| *g > cache.generation())
        }) {
            if let Err(e) = self.reload(cache).await {
                tracing::error!(
                    scope = %cache.scope,
                    error = %e,
                    "Vault reload failed; serving the last good snapshot"
                );
            }
        }
        Ok(())
    }

    /// Drop audit rows past retention, then keys nothing references any more.
    async fn prune(&self) -> Result<()> {
        let conn = self.pool.get().await?;
        let days = i32::try_from(self.settings.audit_retention_days).unwrap_or(i32::MAX);
        conn.execute(
            "DELETE FROM vault_audit WHERE at < now() - make_interval(days => $1)",
            &[&days],
        )
        .await?;
        conn.execute(
            "DELETE FROM vault_keys k
             WHERE k.key_id <> (SELECT active_key_id FROM vault_keyring)
               AND NOT EXISTS (SELECT 1 FROM vault_entries e WHERE e.key_id = k.key_id)
               AND NOT EXISTS (SELECT 1 FROM vault_audit a WHERE a.key_id = k.key_id)",
            &[],
        )
        .await?;
        Ok(())
    }

    async fn commit(
        &self,
        batch: &[(&PgScope, &[VaultOp<'_>])],
        actor: &str,
    ) -> Result<Vec<Vec<bool>>> {
        let committed = match self.try_commit(batch, actor).await? {
            Some(committed) => committed,
            None => {
                self.refresh_keyring().await?;
                self.try_commit(batch, actor)
                    .await?
                    .ok_or_else(|| eyre!("the vault key changed twice during one write"))?
            }
        };
        let mut caches: Vec<&Arc<ScopeCache>> = Vec::new();
        batch
            .iter()
            .map(|(scope, _)| &scope.cache)
            .for_each(|cache| {
                if !caches.iter().any(|seen| Arc::ptr_eq(seen, cache)) {
                    caches.push(cache);
                }
            });
        for (scope, generation) in &committed.generations {
            let changes = committed
                .changes
                .get(scope)
                .map(Vec::as_slice)
                .unwrap_or_default();
            for cache in caches.iter().filter(|cache| &cache.scope == scope) {
                if !cache.apply_committed(*generation, changes) {
                    if let Err(e) = self.reload(cache).await {
                        tracing::warn!(
                            scope = %cache.scope,
                            error = %e,
                            "Vault reload after a write failed; the next sync catches up"
                        );
                    }
                }
            }
        }
        Ok(committed.existed)
    }

    /// One transaction for the whole batch. `None` when the active key
    /// changed since this process loaded its keyring.
    async fn try_commit(
        &self,
        batch: &[(&PgScope, &[VaultOp<'_>])],
        actor: &str,
    ) -> Result<Option<Committed>> {
        let keyring = self.keyring();
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        let active: Uuid = tx
            .query_one("SELECT active_key_id FROM vault_keyring FOR SHARE", &[])
            .await?
            .try_get(0)?;
        if active != keyring.active.id() {
            tx.rollback().await?;
            return Ok(None);
        }
        let scopes: Vec<String> = batch
            .iter()
            .map(|(scope, _)| scope.scope().to_owned())
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        lock_writable(&tx, &scopes).await?;

        let now = Utc::now();
        let mut existed = Vec::with_capacity(batch.len());
        let mut changes: HashMap<String, Vec<(String, Option<VaultEntry>)>> = HashMap::new();
        for (scope, ops) in batch {
            let scope = scope.scope();
            let current = current_entries(&tx, &keyring, scope, ops).await?;
            let plan = plan(current, ops, actor, now);
            let changed = write_plan(&tx, &keyring.active, scope, &plan).await?;
            changes.entry(scope.to_owned()).or_default().extend(changed);
            existed.push(plan.existed);
        }
        let touched: Vec<&String> = changes
            .iter()
            .filter(|(_, changed)| !changed.is_empty())
            .map(|(scope, _)| scope)
            .collect();
        let generations = if touched.is_empty() {
            Vec::new()
        } else {
            tx.query(
                "UPDATE vault_scopes SET generation = generation + 1
                 WHERE scope = ANY($1) RETURNING scope, generation",
                &[&touched],
            )
            .await?
            .iter()
            .map(|row| Ok((row.try_get(0)?, row.try_get(1)?)))
            .collect::<Result<Vec<_>, tokio_postgres::Error>>()?
        };
        tx.commit().await?;
        Ok(Some(Committed {
            existed,
            changes,
            generations,
        }))
    }

    /// Run `fut` to completion from synchronous vault code — on a runtime
    /// worker (`block_in_place`), a blocking-pool thread, or a thread outside
    /// any runtime (the runtime this store connected on).
    fn block_on<T>(&self, fut: impl Future<Output = Result<T>>) -> Result<T> {
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => match handle.runtime_flavor() {
                tokio::runtime::RuntimeFlavor::MultiThread => {
                    tokio::task::block_in_place(|| handle.block_on(fut))
                }
                _ => handle.block_on(fut),
            },
            Err(_) => self.runtime.block_on(fut),
        }
    }
}

/// One scope's lifecycle, as `workforce vault verify` reports it.
#[derive(Debug, Clone)]
pub struct ScopeState {
    pub generation: i64,
    pub imported_at: Option<DateTime<Utc>>,
    /// The imported file's SHA-256; `None` for a scope a hub created empty.
    pub import_sha256: Option<Vec<u8>>,
    pub frozen_at: Option<DateTime<Utc>>,
    pub entries: usize,
}

impl ScopeState {
    /// Whether hubs on Postgres write this scope.
    pub fn writable(&self) -> bool {
        self.imported_at.is_some() && self.frozen_at.is_none()
    }
}

/// How an import transaction ended.
pub(super) enum Import {
    Imported,
    FileChanged,
    OnPostgres,
    /// A hub created the scope empty and has stored secrets in it since.
    Diverged {
        entries: usize,
    },
}

/// What a key rotation re-sealed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RotationSummary {
    pub scopes: usize,
    pub entries: usize,
}

impl PgStore {
    pub async fn scope_states(&self) -> Result<HashMap<String, ScopeState>> {
        let rows = self
            .pool
            .get()
            .await?
            .query(
                "SELECT s.scope, s.generation, s.imported_at, s.import_sha256, s.frozen_at,
                        (SELECT count(*) FROM vault_entries e WHERE e.scope = s.scope)
                 FROM vault_scopes s",
                &[],
            )
            .await?;
        rows.iter()
            .map(|row| -> Result<(String, ScopeState)> {
                Ok((
                    row.try_get(0)?,
                    ScopeState {
                        generation: row.try_get(1)?,
                        imported_at: row.try_get(2)?,
                        import_sha256: row.try_get(3)?,
                        frozen_at: row.try_get(4)?,
                        entries: usize::try_from(row.try_get::<_, i64>(5)?)?,
                    },
                ))
            })
            .collect()
    }

    /// Replace `scope` with an imported file's entries and append its audit
    /// history, in one transaction that commits only if `unchanged` confirms
    /// the file still holds the bytes that were read. A scope hubs already
    /// write is left alone ([`import_refusal`]).
    pub async fn import_scope(
        &self,
        scope: &str,
        entries: &HashMap<String, VaultEntry>,
        audit: &[AuditEntry<'_>],
        file_sha256: &[u8],
        unchanged: impl FnOnce() -> Result<bool>,
    ) -> Result<Import> {
        let keyring = self.keyring();
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        let active: Uuid = tx
            .query_one("SELECT active_key_id FROM vault_keyring FOR SHARE", &[])
            .await?
            .try_get(0)?;
        if active != keyring.active.id() {
            bail!("the vault key changed during the import; run the command again");
        }
        tx.execute(
            "INSERT INTO vault_scopes (scope) VALUES ($1) ON CONFLICT (scope) DO NOTHING",
            &[&scope],
        )
        .await?;
        if let Some(refusal) = import_refusal(&tx, scope).await? {
            tx.rollback().await?;
            return Ok(refusal);
        }
        tx.execute("DELETE FROM vault_entries WHERE scope = $1", &[&scope])
            .await?;
        for (path, entry) in entries {
            write_entry(&tx, &keyring.active, scope, path, Some(entry)).await?;
        }
        for record in audit {
            insert_audit(&tx, &keyring.active, scope, record).await?;
        }
        tx.execute(
            "UPDATE vault_scopes SET generation = generation + 1, imported_at = now(),
                 import_sha256 = $2, frozen_at = NULL
             WHERE scope = $1",
            &[&scope, &file_sha256],
        )
        .await?;
        if !unchanged()? {
            tx.rollback().await?;
            return Ok(Import::FileChanged);
        }
        tx.commit().await?;
        Ok(Import::Imported)
    }

    pub async fn freeze_scopes(&self, scopes: &[String]) -> Result<()> {
        self.pool
            .get()
            .await?
            .execute(
                "UPDATE vault_scopes SET frozen_at = COALESCE(frozen_at, now()) WHERE scope = ANY($1)",
                &[&scopes],
            )
            .await?;
        Ok(())
    }

    pub async fn scope_entries(&self, scope: &str) -> Result<HashMap<String, VaultEntry>> {
        Ok(self.snapshot(scope).await?.entries)
    }

    /// `scope`'s entries, and its audit records since it was last imported
    /// (oldest first) — what `workforce vault export` writes back to files.
    pub async fn export_scope(
        &self,
        scope: &str,
    ) -> Result<(HashMap<String, VaultEntry>, Vec<AuditEntry<'static>>)> {
        let entries = self.scope_entries(scope).await?;
        let rows = self
            .pool
            .get()
            .await?
            .query(
                "SELECT a.scope, a.key_id, a.nonce, a.details
                 FROM vault_audit a JOIN vault_scopes s USING (scope)
                 WHERE a.scope = $1 AND a.at > s.imported_at
                 ORDER BY a.at, a.audit_id",
                &[&scope],
            )
            .await?;
        let audit = self
            .open_audit_rows(&rows)
            .await?
            .into_iter()
            .map(|(_, record)| record)
            .collect();
        Ok((entries, audit))
    }

    /// Audit records, newest first.
    pub async fn audit_records(
        &self,
        scope: Option<&str>,
        since: Option<DateTime<Utc>>,
        limit: i64,
    ) -> Result<Vec<(String, AuditEntry<'static>)>> {
        let rows = self
            .pool
            .get()
            .await?
            .query(
                "SELECT scope, key_id, nonce, details FROM vault_audit
                 WHERE ($1::text IS NULL OR scope = $1)
                   AND ($2::timestamptz IS NULL OR at >= $2)
                 ORDER BY at DESC, audit_id DESC
                 LIMIT $3",
                &[&scope, &since, &limit],
            )
            .await?;
        self.open_audit_rows(&rows).await
    }

    async fn open_audit_rows(&self, rows: &[Row]) -> Result<Vec<(String, AuditEntry<'static>)>> {
        let key_ids = rows
            .iter()
            .map(|row| row.try_get("key_id"))
            .collect::<Result<BTreeSet<Uuid>, _>>()?;
        let keyring = match self.keyring_for(key_ids).await {
            Ok(keyring) => keyring,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "Skipping audit records sealed with keys this passphrase cannot open"
                );
                self.keyring()
            }
        };
        Ok(rows
            .iter()
            .filter_map(|row| {
                let scope: String = row.try_get("scope").ok()?;
                let key_id: Uuid = row.try_get("key_id").ok()?;
                let record = keyring
                    .keys
                    .get(&key_id)?
                    .open_audit(&scope, row.try_get("nonce").ok()?, row.try_get("details").ok()?)
                    .inspect_err(|e| {
                        tracing::error!(scope = %scope, error = %e, "Skipping an audit record that does not decrypt");
                    })
                    .ok()?;
                Some((scope, record))
            })
            .collect())
    }

    /// Seal every secret under a new key (design D18). With a new passphrase,
    /// also wrap the new key — and every retained key, so older audit
    /// history stays readable — under it.
    pub async fn rotate(&self, new_passphrase: Option<String>) -> Result<RotationSummary> {
        self.refresh_keyring().await?;
        let keyring = self.keyring();
        let incoming = new_passphrase.map(KeySource::new);
        self.ensure_no_passphrase_is_stranded(keyring.active.id(), incoming.as_ref())
            .await?;
        let new_kek = match &incoming {
            Some(keys) => Some(keys.fresh_kek().await?),
            None => None,
        };
        let next = Dek::generate()?;
        let mut conn = self.pool.get().await?;
        let tx = conn.transaction().await?;
        let active: Uuid = tx
            .query_one("SELECT active_key_id FROM vault_keyring FOR UPDATE", &[])
            .await?
            .try_get(0)?;
        if active != keyring.active.id() {
            bail!("the vault key changed during the rotation; run the command again");
        }
        let scopes = tx
            .query(
                "SELECT scope FROM vault_scopes ORDER BY scope FOR UPDATE",
                &[],
            )
            .await?
            .len();
        let entries = tx
            .query(
                "SELECT scope, path_mac, key_id, nonce, ciphertext FROM vault_entries",
                &[],
            )
            .await?
            .iter()
            .map(|row| {
                let scope: String = row.try_get("scope")?;
                let (path, entry) = keyring.open_row(&scope, row)?;
                Ok((scope, path, entry))
            })
            .collect::<Result<Vec<_>>>()
            .wrap_err("every secret must decrypt before the key can rotate")?;
        tx.execute("INSERT INTO vault_keys (key_id) VALUES ($1)", &[&next.id()])
            .await?;
        insert_wrap(&tx, &keyring.kek, &next).await?;
        if let Some(kek) = &new_kek {
            insert_wrap(&tx, kek, &next).await?;
            // Pruning may have dropped a retained key since the keyring loaded;
            // the ones still stored are held until commit.
            let ids: Vec<Uuid> = keyring.keys.keys().copied().collect();
            let stored = tx
                .query(
                    "SELECT key_id FROM vault_keys WHERE key_id = ANY($1) FOR KEY SHARE",
                    &[&ids],
                )
                .await?
                .iter()
                .map(|row| row.try_get(0))
                .collect::<Result<BTreeSet<Uuid>, _>>()?;
            for retained in stored.iter().filter_map(|id| keyring.keys.get(id)) {
                insert_wrap(&tx, kek, retained).await?;
            }
        }
        tx.execute("DELETE FROM vault_entries", &[]).await?;
        for (scope, path, entry) in &entries {
            write_entry(&tx, &next, scope, path, Some(entry)).await?;
        }
        tx.execute("UPDATE vault_keyring SET active_key_id = $1", &[&next.id()])
            .await?;
        tx.execute("UPDATE vault_scopes SET generation = generation + 1", &[])
            .await?;
        tx.commit().await?;
        self.refresh_keyring().await?;
        Ok(RotationSummary {
            scopes,
            entries: entries.len(),
        })
    }

    /// Refuse a rotation that would lock out a passphrase that opens the
    /// active key today and is neither the configured one nor `incoming`.
    async fn ensure_no_passphrase_is_stranded(
        &self,
        active: Uuid,
        incoming: Option<&KeySource>,
    ) -> Result<()> {
        let wraps = load_wraps(&self.pool.get().await?).await?;
        let active_wraps: Vec<&WrapRow> =
            wraps.iter().filter(|wrap| wrap.key_id == active).collect();
        let ours = self.keys.keks_for(active_wraps.iter().copied()).await?;
        let theirs = match incoming {
            Some(keys) => keys.keks_for(active_wraps.iter().copied()).await?,
            None => Keks::new(),
        };
        if active_wraps
            .iter()
            .any(|wrap| wrap.open(&ours).is_none() && wrap.open(&theirs).is_none())
        {
            bail!(
                "refusing to rotate: another passphrase also opens the vault key and would be \
                 locked out. Finish that passphrase change first (`workforce vault retire-key` \
                 once every hub runs the new passphrase), or rotate onto it with --new-key-file"
            );
        }
        Ok(())
    }

    /// Delete every key wrap the configured passphrase cannot open. Run once
    /// every hub holds the new passphrase.
    pub async fn retire_foreign_wraps(&self) -> Result<usize> {
        self.refresh_keyring().await?;
        let conn = self.pool.get().await?;
        let wraps = load_wraps(&conn).await?;
        let keks = self.keys.keks_for(&wraps).await?;
        let (ours, foreign): (Vec<&WrapRow>, Vec<&WrapRow>) =
            wraps.iter().partition(|wrap| wrap.open(&keks).is_some());
        let active = self.keyring().active.id();
        if !ours.iter().any(|wrap| wrap.key_id == active) {
            bail!(
                "refusing to retire: the configured passphrase does not open the active vault key"
            );
        }
        let retired: Vec<Uuid> = foreign.iter().map(|wrap| wrap.wrap_id).collect();
        conn.execute(
            "DELETE FROM vault_key_wraps WHERE wrap_id = ANY($1)",
            &[&retired],
        )
        .await?;
        Ok(retired.len())
    }
}

/// Lock `scopes` (sorted, so concurrent writers never deadlock) and refuse
/// the write if any is missing or frozen.
async fn lock_writable(client: &impl GenericClient, scopes: &[String]) -> Result<()> {
    let states: HashMap<String, Option<DateTime<Utc>>> = client
        .query(
            "SELECT scope, frozen_at FROM vault_scopes WHERE scope = ANY($1)
             ORDER BY scope FOR UPDATE",
            &[&scopes],
        )
        .await?
        .iter()
        .map(|row| Ok((row.try_get(0)?, row.try_get(1)?)))
        .collect::<Result<_, tokio_postgres::Error>>()?;
    if let Some(missing) = scopes.iter().find(|scope| !states.contains_key(*scope)) {
        bail!("vault scope `{missing}` is not initialised");
    }
    if let Some(frozen) = scopes
        .iter()
        .find(|scope| states.get(*scope).is_some_and(Option::is_some))
    {
        return Err(VaultError::Frozen {
            scope: frozen.clone(),
        }
        .into());
    }
    Ok(())
}

/// Why an import must leave `scope` alone, if it must: hubs write it and it
/// was imported from a file, or a hub created it empty and has stored secrets
/// in it since. Locks the scope row for the rest of the transaction.
async fn import_refusal(client: &impl GenericClient, scope: &str) -> Result<Option<Import>> {
    let state = client
        .query_one(
            "SELECT imported_at IS NOT NULL AND frozen_at IS NULL, import_sha256 IS NOT NULL
             FROM vault_scopes WHERE scope = $1 FOR UPDATE",
            &[&scope],
        )
        .await?;
    let (writable, from_file): (bool, bool) = (state.try_get(0)?, state.try_get(1)?);
    if !writable {
        return Ok(None);
    }
    if from_file {
        return Ok(Some(Import::OnPostgres));
    }
    let entries = usize::try_from(
        client
            .query_one(
                "SELECT count(*) FROM vault_entries WHERE scope = $1",
                &[&scope],
            )
            .await?
            .try_get::<_, i64>(0)?,
    )?;
    Ok((entries > 0).then_some(Import::Diverged { entries }))
}

/// The stored entries `ops` touch. A row that doesn't decrypt counts as
/// absent, so a normal write can replace it.
async fn current_entries(
    client: &impl GenericClient,
    keyring: &Keyring,
    scope: &str,
    ops: &[VaultOp<'_>],
) -> Result<HashMap<String, VaultEntry>> {
    let path_macs: Vec<Vec<u8>> = ops
        .iter()
        .map(VaultOp::path)
        .collect::<BTreeSet<_>>()
        .into_iter()
        .map(|path| keyring.active.path_mac(scope, path))
        .collect();
    Ok(client
        .query(
            "SELECT path_mac, key_id, nonce, ciphertext FROM vault_entries
             WHERE scope = $1 AND path_mac = ANY($2)",
            &[&scope, &path_macs],
        )
        .await?
        .iter()
        .filter_map(|row| open_logged(keyring, scope, row))
        .collect())
}

/// Persist a plan's changed rows and audit records; returns each changed
/// path with its final state.
async fn write_plan(
    client: &impl GenericClient,
    dek: &Dek,
    scope: &str,
    plan: &Plan,
) -> Result<Vec<(String, Option<VaultEntry>)>> {
    let changed: Vec<(String, Option<VaultEntry>)> = plan
        .audit
        .iter()
        .map(|record| record.path.as_ref())
        .fold(Vec::<&str>::new(), |mut paths, path| {
            if !paths.contains(&path) {
                paths.push(path);
            }
            paths
        })
        .into_iter()
        .map(|path| (path.to_owned(), plan.entries.get(path).cloned()))
        .collect();
    for (path, entry) in &changed {
        write_entry(client, dek, scope, path, entry.as_ref()).await?;
    }
    for record in &plan.audit {
        insert_audit(client, dek, scope, record).await?;
    }
    Ok(changed)
}

async fn write_entry(
    client: &impl GenericClient,
    dek: &Dek,
    scope: &str,
    path: &str,
    entry: Option<&VaultEntry>,
) -> Result<()> {
    match entry {
        Some(entry) => {
            let sealed = dek.seal_entry(scope, path, entry)?;
            client
                .execute(
                    "INSERT INTO vault_entries (scope, path_mac, key_id, nonce, ciphertext)
                     VALUES ($1, $2, $3, $4, $5)
                     ON CONFLICT (scope, path_mac) DO UPDATE SET
                         key_id = EXCLUDED.key_id,
                         nonce = EXCLUDED.nonce,
                         ciphertext = EXCLUDED.ciphertext",
                    &[
                        &scope,
                        &sealed.path_mac,
                        &sealed.key_id,
                        &sealed.nonce.as_slice(),
                        &sealed.ciphertext,
                    ],
                )
                .await?;
        }
        None => {
            client
                .execute(
                    "DELETE FROM vault_entries WHERE scope = $1 AND path_mac = $2",
                    &[&scope, &dek.path_mac(scope, path)],
                )
                .await?;
        }
    }
    Ok(())
}

async fn insert_audit(
    client: &impl GenericClient,
    dek: &Dek,
    scope: &str,
    record: &AuditEntry<'_>,
) -> Result<()> {
    let sealed = dek.seal_audit(scope, record)?;
    client
        .execute(
            "INSERT INTO vault_audit (scope, at, key_id, nonce, details) VALUES ($1, $2, $3, $4, $5)",
            &[
                &scope,
                &record.ts,
                &dek.id(),
                &sealed.nonce.as_slice(),
                &sealed.ciphertext,
            ],
        )
        .await?;
    Ok(())
}

fn open_logged(keyring: &Keyring, scope: &str, row: &Row) -> Option<(String, VaultEntry)> {
    keyring
        .open_row(scope, row)
        .inspect_err(|e| {
            tracing::error!(
                scope,
                path_mac = %row
                    .try_get::<_, &[u8]>("path_mac")
                    .map(hex::encode)
                    .unwrap_or_default(),
                error = %e,
                "Skipping a vault row that does not decrypt"
            );
        })
        .ok()
}

/// Poll the generation of every open scope and reload the ones peers have
/// changed; prune the audit table hourly. Stops once the store is dropped.
fn spawn_sync_loop(store: &Arc<PgStore>) {
    let weak = Arc::downgrade(store);
    let period = store.settings.sync_interval;
    tokio::spawn(async move {
        let mut ticker = tokio::time::interval(period);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);
        let mut pruned_at: Option<Instant> = None;
        loop {
            ticker.tick().await;
            let Some(store) = weak.upgrade() else {
                break;
            };
            if let Err(e) = store.sync_live().await {
                tracing::warn!(error = %e, "Vault sync failed; serving cached secrets until it recovers");
            }
            if pruned_at.is_none_or(|at| at.elapsed() >= PRUNE_EVERY) {
                if let Err(e) = store.prune().await {
                    tracing::warn!(error = %e, "Vault audit pruning failed");
                }
                pruned_at = Some(Instant::now());
            }
        }
    });
}

/// A [`Vault`]'s handle on one scope of a [`PgStore`].
pub(super) struct PgScope {
    store: Arc<PgStore>,
    cache: Arc<ScopeCache>,
}

impl PgScope {
    pub fn scope(&self) -> &str {
        &self.cache.scope
    }

    pub fn read<T>(&self, f: impl FnOnce(&HashMap<String, VaultEntry>) -> T) -> T {
        f(&self.cache.snapshot.read().entries)
    }

    pub fn sync(&self) -> Result<()> {
        self.store.block_on(self.store.sync_scope(&self.cache))
    }

    pub fn refresh_lock(&self) -> Arc<dyn RefreshLock> {
        Arc::new(PgRefreshLock {
            pool: self.store.pool.clone(),
            queue: Arc::clone(&self.store.refresh_queue),
            connections: Arc::clone(&self.store.refresh_connections),
        })
    }
}

/// Apply every scope's ops in one transaction. All scopes must share a store.
pub(super) fn commit(
    batch: &[(&PgScope, &[VaultOp<'_>])],
    actor: VaultActor<'_>,
) -> Result<Vec<Vec<bool>>> {
    let Some(((first, _), rest)) = batch.split_first() else {
        return Ok(Vec::new());
    };
    if rest
        .iter()
        .any(|(scope, _)| !Arc::ptr_eq(&scope.store, &first.store))
    {
        bail!("one vault commit cannot span Postgres stores");
    }
    let store = &first.store;
    store.block_on(store.commit(batch, &actor.audit_string()))
}

/// Fleet-wide refresh lock: a session advisory lock held on a connection
/// detached from the pool. Dropping the guard closes that connection, which
/// releases the lock — as does the holder's process dying. A process's
/// waiters for one key queue locally first, and it holds at most
/// [`REFRESH_LOCK_CONNECTIONS`] of these connections at once.
struct PgRefreshLock {
    pool: PostgresPool,
    queue: Arc<LocalRefreshLock>,
    connections: Arc<Semaphore>,
}

impl PgRefreshLock {
    async fn acquire(&self, key: &str) -> Result<RefreshGuard> {
        let queued = self.queue.lock(key).await?;
        let slot = Arc::clone(&self.connections).acquire_owned().await?;
        let client = deadpool_postgres::Object::take(self.pool.get().await?);
        let id = advisory_key(key);
        async {
            client
                .batch_execute(&format!(
                    "SET lock_timeout = '{}s'",
                    REFRESH_LOCK_WAIT.as_secs()
                ))
                .await?;
            client.execute("SELECT pg_advisory_lock($1)", &[&id]).await
        }
        .await
        .wrap_err("taking the vault refresh lock")?;
        Ok(RefreshGuard::new((client, slot, queued)))
    }
}

#[async_trait]
impl RefreshLock for PgRefreshLock {
    async fn lock(&self, key: &str) -> Result<RefreshGuard> {
        tokio::time::timeout(
            REFRESH_LOCK_WAIT + Duration::from_secs(5),
            self.acquire(key),
        )
        .await
        .map_err(|_| eyre!("timed out waiting for the vault refresh lock"))?
    }
}

fn advisory_key(key: &str) -> i64 {
    let digest = Sha256::new()
        .chain_update(b"workforce-vault-refresh\0")
        .chain_update(key.as_bytes())
        .finalize();
    i64::from_be_bytes(std::array::from_fn(|i| digest[i]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vault::{commit as commit_writes, VaultWrite};
    use tokio_postgres::error::SqlState;
    use workforce_common::settings::SecretStore;
    use workforce_postgres::test_support::with_test_pool;

    const PASSPHRASE: &str = "correct horse battery staple";

    fn settings() -> PgSettings {
        PgSettings {
            sync_interval: Duration::from_millis(50),
            audit_retention_days: 30,
        }
    }

    async fn connect(pool: &PostgresPool) -> Arc<PgStore> {
        PgStore::connect(pool.clone(), PASSPHRASE.into(), settings())
            .await
            .unwrap()
    }

    async fn open(store: &Arc<PgStore>, scope: &str) -> Arc<Vault> {
        store.init_scope(scope).await.unwrap();
        store.vault(scope).await.unwrap()
    }

    async fn sql(pool: &PostgresPool, statement: &str) {
        pool.get()
            .await
            .unwrap()
            .batch_execute(statement)
            .await
            .unwrap();
    }

    async fn count(pool: &PostgresPool, query: &str) -> i64 {
        pool.get()
            .await
            .unwrap()
            .query_one(query, &[])
            .await
            .unwrap()
            .get(0)
    }

    async fn eventually(check: impl Fn() -> bool) {
        for _ in 0..100 {
            if check() {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        panic!("condition not reached within 2s");
    }

    fn entry(value: &str) -> VaultEntry {
        let now = Utc::now();
        VaultEntry {
            value: value.into(),
            created_at: now,
            updated_at: now,
            created_by: "cli".into(),
            updated_by: "cli".into(),
        }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn peer_writes_reach_other_processes() {
        with_test_pool(build_pool, |pool| async move {
            let (a, b) = (connect(&pool).await, connect(&pool).await);
            let (va, vb) = (open(&a, "global").await, open(&b, "global").await);

            va.set("slack/bot_token", "xoxb-1").unwrap();
            assert_eq!(va.get("slack/bot_token").as_deref(), Some("xoxb-1"));
            vb.sync().unwrap();
            assert_eq!(vb.get("slack/bot_token").as_deref(), Some("xoxb-1"));

            va.set("slack/bot_token", "xoxb-2").unwrap();
            eventually(|| vb.get("slack/bot_token").as_deref() == Some("xoxb-2")).await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_peer_write_never_resurrects_a_delete() {
        with_test_pool(build_pool, |pool| async move {
            let (a, b) = (connect(&pool).await, connect(&pool).await);
            let (va, vb) = (open(&a, "global").await, open(&b, "global").await);
            va.set("github/pat", "compromised").unwrap();
            vb.sync().unwrap();

            assert!(va.delete("github/pat").unwrap());
            vb.set("unrelated", "x").unwrap();

            va.sync().unwrap();
            assert!(va.get("github/pat").is_none());
            assert!(vb.get("github/pat").is_none());
            assert_eq!(va.get("unrelated").as_deref(), Some("x"));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_peer_write_never_rolls_back_a_rotated_token() {
        with_test_pool(build_pool, |pool| async move {
            let (a, b) = (connect(&pool).await, connect(&pool).await);
            let (va, vb) = (open(&a, "global").await, open(&b, "global").await);
            va.set("oauth/refresh_token", "rt-1").unwrap();
            vb.sync().unwrap();

            va.set("oauth/refresh_token", "rt-2").unwrap();
            vb.set("oauth/access_token", "at").unwrap();

            assert_eq!(vb.get("oauth/refresh_token").as_deref(), Some("rt-2"));
            let fresh = open(&connect(&pool).await, "global").await;
            assert_eq!(fresh.get("oauth/refresh_token").as_deref(), Some("rt-2"));
            assert_eq!(fresh.get("oauth/access_token").as_deref(), Some("at"));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_refused_write_is_never_served() {
        with_test_pool(build_pool, |pool| async move {
            let vault = open(&connect(&pool).await, "global").await;
            vault.set("k", "before").unwrap();
            sql(
                &pool,
                "UPDATE vault_scopes SET frozen_at = now() WHERE scope = 'global'",
            )
            .await;

            let err = vault.set("k", "after").unwrap_err();
            assert!(matches!(
                err.downcast_ref::<VaultError>(),
                Some(VaultError::Frozen { .. })
            ));
            assert_eq!(vault.get("k").as_deref(), Some("before"));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn concurrent_writers_across_processes_lose_nothing() {
        with_test_pool(build_pool, |pool| async move {
            let stores = [connect(&pool).await, connect(&pool).await];
            let vaults = [
                open(&stores[0], "global").await,
                open(&stores[1], "global").await,
            ];

            let handles: Vec<_> = vaults
                .iter()
                .enumerate()
                .flat_map(|(n, vault)| {
                    (0..4).map(move |t| {
                        let vault = Arc::clone(vault);
                        tokio::task::spawn_blocking(move || {
                            (0..10).try_for_each(|i| {
                                vault.set(&format!("p{n}t{t}k{i}"), "v")?;
                                vault.set("shared", &format!("p{n}t{t}k{i}"))
                            })
                        })
                    })
                })
                .collect();
            for handle in handles {
                handle.await.unwrap().unwrap();
            }

            let fresh = open(&connect(&pool).await, "global").await;
            assert_eq!(fresh.list(None).len(), 81);
            assert_eq!(
                count(
                    &pool,
                    "SELECT generation FROM vault_scopes WHERE scope = 'global'"
                )
                .await,
                160
            );
            assert_eq!(count(&pool, "SELECT count(*) FROM vault_audit").await, 160);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_cross_scope_move_is_all_or_nothing() {
        with_test_pool(build_pool, |pool| async move {
            let store = connect(&pool).await;
            let (global, agent) = (open(&store, "global").await, open(&store, "agent:a").await);
            agent.set("skills/x/token", "tok").unwrap();
            let moving = [
                VaultWrite {
                    vault: &global,
                    op: VaultOp::Set {
                        path: "skills/x/token",
                        value: "tok",
                    },
                },
                VaultWrite {
                    vault: &agent,
                    op: VaultOp::Delete {
                        path: "skills/x/token",
                    },
                },
            ];

            sql(
                &pool,
                "UPDATE vault_scopes SET frozen_at = now() WHERE scope = 'agent:a'",
            )
            .await;
            assert!(commit_writes(&moving, VaultActor::Cli).is_err());
            assert!(global.get("skills/x/token").is_none());
            assert_eq!(agent.get("skills/x/token").as_deref(), Some("tok"));

            sql(
                &pool,
                "UPDATE vault_scopes SET frozen_at = NULL WHERE scope = 'agent:a'",
            )
            .await;
            assert_eq!(
                commit_writes(&moving, VaultActor::Cli).unwrap(),
                vec![false, true]
            );
            assert_eq!(global.get("skills/x/token").as_deref(), Some("tok"));
            assert!(agent.get("skills/x/token").is_none());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn racing_first_connections_create_one_keyring() {
        with_test_pool(build_pool, |pool| async move {
            let (a, b) = tokio::join!(
                PgStore::connect(pool.clone(), PASSPHRASE.into(), settings()),
                PgStore::connect(pool.clone(), PASSPHRASE.into(), settings())
            );
            let (a, b) = (a.unwrap(), b.unwrap());
            assert_eq!(count(&pool, "SELECT count(*) FROM vault_keys").await, 1);

            let (va, vb) = (open(&a, "global").await, open(&b, "global").await);
            va.set("k", "v").unwrap();
            vb.sync().unwrap();
            assert_eq!(vb.get("k").as_deref(), Some("v"));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_different_passphrase_cannot_connect() {
        with_test_pool(build_pool, |pool| async move {
            connect(&pool).await;
            let err = PgStore::connect(pool.clone(), "not the passphrase".into(), settings())
                .await
                .err()
                .unwrap();
            assert!(err.to_string().contains("passphrase"), "{err}");
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn writes_work_from_blocking_and_plain_threads() {
        with_test_pool(build_pool, |pool| async move {
            let vault = open(&connect(&pool).await, "global").await;

            let blocking = Arc::clone(&vault);
            tokio::task::spawn_blocking(move || blocking.set("from-blocking", "1"))
                .await
                .unwrap()
                .unwrap();
            let plain = Arc::clone(&vault);
            std::thread::spawn(move || plain.set("from-thread", "1"))
                .join()
                .unwrap()
                .unwrap();

            assert_eq!(vault.list(None), vec!["from-blocking", "from-thread"]);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_refresh_lock_is_exclusive_across_processes() {
        with_test_pool(build_pool, |pool| async move {
            let (a, b) = (connect(&pool).await, connect(&pool).await);
            let (va, vb) = (open(&a, "global").await, open(&b, "global").await);
            let (la, lb) = (va.refresh_lock(), vb.refresh_lock());

            let held = la.lock("token").await.unwrap();
            assert!(
                tokio::time::timeout(Duration::from_millis(300), lb.lock("token"))
                    .await
                    .is_err(),
                "a second process must wait for the holder"
            );
            tokio::time::timeout(Duration::from_secs(5), lb.lock("other"))
                .await
                .expect("an unrelated key never waits")
                .unwrap();

            drop(held);
            tokio::time::timeout(Duration::from_secs(5), lb.lock("token"))
                .await
                .expect("dropping the guard releases the lock")
                .unwrap();
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn two_hubs_refresh_one_chatgpt_login_once() {
        use crate::hub_core::logins::chatgpt_auth::{AuthJson, TokenRefresher};
        use crate::hub_core::logins::resolve::VaultSlot;
        use crate::hub_core::logins::test_support::login;

        with_test_pool(build_pool, |pool| async move {
            let (a, b) = (connect(&pool).await, connect(&pool).await);
            let (va, vb) = (open(&a, "global").await, open(&b, "global").await);
            let key = workforce_common::subscription::SubscriptionKind::ChatGpt.vault_key();
            let stored = login("acct", "access-0", "rt-0");
            va.set(key, &stored).unwrap();
            vb.sync().unwrap();

            let server = wiremock::MockServer::start().await;
            wiremock::Mock::given(wiremock::matchers::method("POST"))
                .respond_with(
                    wiremock::ResponseTemplate::new(200)
                        .set_body_json(serde_json::json!({"refresh_token": "rt-1"}))
                        .set_delay(Duration::from_millis(100)),
                )
                .expect(1)
                .mount(&server)
                .await;
            let refresher = TokenRefresher::at(
                reqwest::Client::new(),
                format!("{}/oauth/token", server.uri()),
            );
            let seen = AuthJson::parse(&stored).unwrap();
            let hubs = [va, vb].map(|vault| {
                let (refresher, seen) = (refresher.clone(), seen.clone());
                tokio::spawn(async move {
                    let slot = VaultSlot {
                        vault,
                        key: key.to_string(),
                    };
                    refresher
                        .fresh(&slot, &seen, chrono::TimeDelta::MAX)
                        .await
                        .unwrap()
                })
            });
            for hub in hubs {
                assert_eq!(hub.await.unwrap().refresh_token(), Some("rt-1"));
            }
            server.verify().await;
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn lock_timeout_aborts_a_waiting_advisory_lock() {
        with_test_pool(build_pool, |pool| async move {
            let holder = deadpool_postgres::Object::take(pool.get().await.unwrap());
            holder
                .execute("SELECT pg_advisory_lock($1)", &[&42i64])
                .await
                .unwrap();
            let waiter = deadpool_postgres::Object::take(pool.get().await.unwrap());
            waiter
                .batch_execute("SET lock_timeout = '200ms'")
                .await
                .unwrap();

            let started = Instant::now();
            let err = waiter
                .execute("SELECT pg_advisory_lock($1)", &[&42i64])
                .await
                .unwrap_err();
            assert_eq!(err.code(), Some(&SqlState::LOCK_NOT_AVAILABLE));
            assert!(started.elapsed() < Duration::from_secs(5));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn every_change_is_audited_in_its_transaction() {
        with_test_pool(build_pool, |pool| async move {
            let store = connect(&pool).await;
            let vault = open(&store, "global").await;
            vault
                .set_with_actor("users/u1/token", "value-1", VaultActor::User("u1"))
                .unwrap();
            vault
                .delete_with_actor("users/u1/token", VaultActor::Cli)
                .unwrap();
            vault.delete("never-existed").unwrap();

            let keyring = store.keyring();
            let records: Vec<AuditEntry<'static>> = pool
                .get()
                .await
                .unwrap()
                .query(
                    "SELECT scope, key_id, nonce, details FROM vault_audit ORDER BY audit_id",
                    &[],
                )
                .await
                .unwrap()
                .iter()
                .map(|row| {
                    keyring.keys[&row.get::<_, Uuid>(1)]
                        .open_audit(row.get(0), row.get(2), row.get(3))
                        .unwrap()
                })
                .collect();
            assert_eq!(records.len(), 2);
            assert_eq!(
                (records[0].action, records[0].actor.as_ref()),
                (crate::hub_core::vault_audit::AuditAction::Set, "user:u1")
            );
            assert_eq!(
                (records[1].action, records[1].actor.as_ref()),
                (crate::hub_core::vault_audit::AuditAction::Delete, "cli")
            );
            assert!(records.iter().all(|r| r.path == "users/u1/token"));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn the_database_never_holds_paths_or_values_in_the_clear() {
        with_test_pool(build_pool, |pool| async move {
            let vault = open(&connect(&pool).await, "global").await;
            vault
                .set(
                    "users/u-needle/skills/zzz/api_token",
                    "plaintext-secret-xyz",
                )
                .unwrap();

            let blobs: Vec<Vec<u8>> = pool
                .get()
                .await
                .unwrap()
                .query(
                    "SELECT path_mac || nonce || ciphertext FROM vault_entries
                     UNION ALL SELECT nonce || details FROM vault_audit",
                    &[],
                )
                .await
                .unwrap()
                .iter()
                .map(|row| row.get(0))
                .collect();
            assert!(!blobs.is_empty());
            for needle in ["plaintext-secret-xyz", "u-needle", "api_token"] {
                assert!(
                    !blobs
                        .iter()
                        .any(|blob| blob.windows(needle.len()).any(|w| w == needle.as_bytes())),
                    "{needle} leaked into the database"
                );
            }
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_corrupt_row_is_skipped_and_can_be_overwritten() {
        with_test_pool(build_pool, |pool| async move {
            let store = connect(&pool).await;
            let vault = open(&store, "global").await;
            vault.set("a", "1").unwrap();
            vault.set("b", "2").unwrap();
            pool.get()
                .await
                .unwrap()
                .execute(
                    "UPDATE vault_entries SET ciphertext = '\\xdeadbeef'::bytea WHERE path_mac = $1",
                    &[&store.keyring().active.path_mac("global", "b")],
                )
                .await
                .unwrap();

            let fresh = open(&connect(&pool).await, "global").await;
            assert_eq!(fresh.get("a").as_deref(), Some("1"));
            assert!(fresh.get("b").is_none());
            fresh.set("b", "3").unwrap();
            assert_eq!(fresh.get("b").as_deref(), Some("3"));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn rows_under_a_key_we_cannot_open_keep_the_last_snapshot() {
        with_test_pool(build_pool, |pool| async move {
            let vault = open(&connect(&pool).await, "global").await;
            vault.set("a", "1").unwrap();

            let foreign = Dek::generate().unwrap();
            let kek = Kek::generate(
                "someone else's passphrase",
                KdfParams {
                    m_kib: 8,
                    t: 1,
                    p: 1,
                },
            )
            .unwrap();
            let conn = pool.get().await.unwrap();
            conn.execute(
                "INSERT INTO vault_keys (key_id) VALUES ($1)",
                &[&foreign.id()],
            )
            .await
            .unwrap();
            insert_wrap(&conn, &kek, &foreign).await.unwrap();
            write_entry(&conn, &foreign, "global", "x", Some(&entry("foreign")))
                .await
                .unwrap();
            conn.execute(
                "UPDATE vault_scopes SET generation = generation + 1 WHERE scope = 'global'",
                &[],
            )
            .await
            .unwrap();

            assert!(vault.sync().is_err());
            assert_eq!(vault.get("a").as_deref(), Some("1"));
            assert!(vault.get("x").is_none());
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn pruning_drops_only_expired_audit_rows() {
        with_test_pool(build_pool, |pool| async move {
            let store = connect(&pool).await;
            let vault = open(&store, "global").await;
            vault.set("k", "1").unwrap();
            sql(
                &pool,
                "UPDATE vault_audit SET at = now() - interval '400 days'",
            )
            .await;
            vault.set("k", "2").unwrap();

            store.prune().await.unwrap();
            assert_eq!(count(&pool, "SELECT count(*) FROM vault_audit").await, 1);
            assert_eq!(count(&pool, "SELECT count(*) FROM vault_keys").await, 1);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_rotation_skips_a_key_pruned_while_it_waited() {
        with_test_pool(build_pool, |pool| async move {
            let store = connect(&pool).await;
            open(&store, "global").await.set("k", "1").unwrap();
            store.rotate(None).await.unwrap();
            let mut holder = pool.get().await.unwrap();
            let held = holder.transaction().await.unwrap();
            held.execute("SELECT 1 FROM vault_keyring FOR SHARE", &[])
                .await
                .unwrap();

            let prune_while_it_waits = async {
                tokio::time::timeout(Duration::from_secs(30), async {
                    while count(
                        &pool,
                        "SELECT count(*) FROM pg_stat_activity
                         WHERE datname = current_database() AND wait_event_type = 'Lock'",
                    )
                    .await
                        == 0
                    {
                        tokio::time::sleep(Duration::from_millis(20)).await;
                    }
                })
                .await
                .expect("the rotation waits on the keyring lock");
                sql(&pool, "DELETE FROM vault_audit").await;
                store.prune().await.unwrap();
                assert_eq!(count(&pool, "SELECT count(*) FROM vault_keys").await, 1);
                held.rollback().await.unwrap();
            };
            let (rotated, ()) = tokio::join!(
                store.rotate(Some("a new passphrase".into())),
                prune_while_it_waits
            );
            rotated.unwrap();

            let moved = PgStore::connect(pool.clone(), "a new passphrase".into(), settings())
                .await
                .unwrap();
            assert_eq!(open(&moved, "global").await.get("k").as_deref(), Some("1"));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_reader_beside_a_live_hub_reads_what_is_there_and_changes_nothing() {
        with_test_pool(build_pool, |pool| async move {
            let tables = "SELECT count(*) FROM information_schema.tables \
                          WHERE table_schema = 'vault'";
            let unmade = PgStore::connect_read_only(pool.clone(), PASSPHRASE.into(), settings());
            assert!(unmade.await.is_err(), "no vault here: nothing to read");
            assert_eq!(count(&pool, tables).await, 0, "and none was created");

            let hub = connect(&pool).await;
            open(&hub, "global").await.set("llm/kev", "k-1").unwrap();
            let rows = "SELECT (SELECT count(*) FROM vault_keys) * 1000000 \
                             + (SELECT count(*) FROM vault_audit) * 1000 \
                             + (SELECT count(*) FROM vault_scopes)";
            let before = count(&pool, rows).await;

            let reader = PgStore::connect_read_only(pool.clone(), PASSPHRASE.into(), settings())
                .await
                .unwrap();
            let read = reader.vault("global").await.unwrap();
            assert_eq!(read.get("llm/kev").as_deref(), Some("k-1"));
            assert!(!reader.scope_initialised("agent:a").await.unwrap());
            assert!(reader.vault("agent:a").await.is_err());
            tokio::time::sleep(Duration::from_millis(200)).await;
            assert_eq!(
                count(&pool, rows).await,
                before,
                "no key, audit row or scope was added or pruned"
            );
            assert!(
                PgStore::connect_read_only(pool.clone(), "another".into(), settings())
                    .await
                    .is_err(),
                "the wrong passphrase opens nothing and creates no key"
            );
            assert_eq!(count(&pool, rows).await, before);
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_readers_pool_refuses_every_write() {
        with_test_pool(build_read_only_pool, |pool| async move {
            let conn = pool.get().await.unwrap();
            for write in ["CREATE SCHEMA vault", "CREATE TABLE written (id INT)"] {
                let refused = conn.execute(write, &[]).await.unwrap_err();
                let why = refused.as_db_error().map(|e| e.message().to_string());
                assert!(
                    why.as_deref().is_some_and(|why| why.contains("read-only")),
                    "{write}: {why:?}"
                );
            }
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_scope_is_one_vault_and_uninitialised_scopes_do_not_open() {
        with_test_pool(build_pool, |pool| async move {
            let store = connect(&pool).await;
            assert!(!store.scope_initialised("agent:a").await.unwrap());
            assert!(store.vault("agent:a").await.is_err());

            let first = open(&store, "agent:a").await;
            assert!(store.scope_initialised("agent:a").await.unwrap());
            assert!(Arc::ptr_eq(&first, &store.vault("agent:a").await.unwrap()));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn one_refresh_serves_every_process() {
        use wiremock::matchers::method;
        use wiremock::{Mock, MockServer, ResponseTemplate};
        use workforce_oauth_client::{
            DiscoveredProvider, OAuthClient, OAuthClientCreds, PathLayout, TokenStore,
        };

        #[derive(Debug)]
        struct Layout;
        impl PathLayout for Layout {
            fn path(&self, target: &str, suffix: &str) -> String {
                format!("oauth/{target}/{suffix}")
            }
        }

        with_test_pool(build_pool, |pool| async move {
            let server = MockServer::start().await;
            Mock::given(method("POST"))
                .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                    "access_token": "at-new",
                    "refresh_token": "rt-new",
                    "expires_in": 3600
                })))
                .expect(1)
                .mount(&server)
                .await;
            let client = Arc::new(OAuthClient::new(
                Arc::new(DiscoveredProvider::new(
                    "test",
                    format!("{}/authorize", server.uri()),
                    server.uri(),
                )),
                OAuthClientCreds {
                    client_id: "id".into(),
                    client_secret: Some("secret".into()),
                    redirect_uri: None,
                },
                reqwest::Client::new(),
            ));
            let (a, b) = (connect(&pool).await, connect(&pool).await);
            let (va, vb) = (open(&a, "global").await, open(&b, "global").await);
            va.set_many(
                &[
                    ("oauth/x/refresh_token", "rt-old"),
                    ("oauth/x/access_token", "at-old"),
                    ("oauth/x/expires_at", "1"),
                ],
                VaultActor::Cli,
            )
            .unwrap();
            vb.sync().unwrap();

            let token_stores: Vec<Arc<TokenStore>> = [va, vb]
                .into_iter()
                .map(|vault| {
                    Arc::new(TokenStore::new(
                        vault as Arc<dyn SecretStore>,
                        "x",
                        Arc::new(Layout),
                        Arc::clone(&client),
                    ))
                })
                .collect();
            let handles: Vec<_> = (0..8)
                .map(|i| {
                    let token_store = Arc::clone(&token_stores[i % 2]);
                    tokio::spawn(async move { token_store.access_token().await })
                })
                .collect();
            for handle in handles {
                assert_eq!(handle.await.unwrap().unwrap(), "at-new");
            }
            let fresh = open(&connect(&pool).await, "global").await;
            assert_eq!(
                fresh.get("oauth/x/refresh_token").as_deref(),
                Some("rt-new")
            );
        })
        .await;
    }

    const THIS_DATABASE: &str =
        "database = (SELECT oid FROM pg_database WHERE datname = current_database())";

    fn contents(vault: &Vault) -> Vec<(String, String)> {
        vault
            .list(None)
            .into_iter()
            .map(|path| {
                let entry = serde_json::to_string(&vault.get_entry(&path).unwrap()).unwrap();
                (path, entry)
            })
            .collect()
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn interleaved_writers_cache_exactly_what_the_database_holds() {
        with_test_pool(build_pool, |pool| async move {
            let (a, b) = (connect(&pool).await, connect(&pool).await);
            let (va, vb) = (open(&a, "global").await, open(&b, "global").await);

            va.set("a", "1").unwrap();
            vb.set("b", "2").unwrap();
            va.set("c", "3").unwrap();
            vb.delete("a").unwrap();
            va.set("b", "22").unwrap();
            va.set("d", "4").unwrap();
            va.sync().unwrap();
            vb.sync().unwrap();

            let fresh = open(&connect(&pool).await, "global").await;
            assert_eq!(fresh.list(None), vec!["b", "c", "d"]);
            assert_eq!(fresh.get("b").as_deref(), Some("22"));
            assert_eq!(contents(&va), contents(&fresh));
            assert_eq!(contents(&vb), contents(&fresh));
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_process_waits_in_postgres_once_per_refresh_key() {
        with_test_pool(build_pool, |pool| async move {
            let vault = open(&connect(&pool).await, "global").await;
            let held = vault.refresh_lock().lock("token").await.unwrap();
            let waiters: Vec<_> = (0..4)
                .map(|_| {
                    let lock = vault.refresh_lock();
                    tokio::spawn(async move { lock.lock("token").await.map(drop) })
                })
                .collect();
            tokio::time::sleep(Duration::from_millis(300)).await;
            assert_eq!(
                count(
                    &pool,
                    &format!(
                        "SELECT count(*) FROM pg_locks WHERE locktype = 'advisory' AND {THIS_DATABASE}"
                    )
                )
                .await,
                1
            );

            drop(held);
            for waiter in waiters {
                waiter.await.unwrap().unwrap();
            }
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn refresh_locks_share_a_bounded_set_of_connections() {
        with_test_pool(build_pool, |pool| async move {
            let lock = open(&connect(&pool).await, "global").await.refresh_lock();
            let keys: Vec<String> = (0..REFRESH_LOCK_CONNECTIONS)
                .map(|n| format!("token-{n}"))
                .collect();
            let mut held = futures::future::try_join_all(keys.iter().map(|key| lock.lock(key)))
                .await
                .unwrap();

            assert!(
                tokio::time::timeout(Duration::from_millis(300), lock.lock("one-more"))
                    .await
                    .is_err(),
                "every connection is taken"
            );
            held.pop();
            tokio::time::timeout(Duration::from_secs(5), lock.lock("one-more"))
                .await
                .expect("a released lock frees its connection")
                .unwrap();
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_dead_holders_refresh_lock_is_released() {
        with_test_pool(build_pool, |pool| async move {
            let (a, b) = (connect(&pool).await, connect(&pool).await);
            let (va, vb) = (open(&a, "global").await, open(&b, "global").await);
            let _held = va.refresh_lock().lock("token").await.unwrap();

            sql(
                &pool,
                &format!(
                    "SELECT pg_terminate_backend(pid) FROM pg_locks
                     WHERE locktype = 'advisory' AND granted AND {THIS_DATABASE}"
                ),
            )
            .await;

            tokio::time::timeout(Duration::from_secs(5), vb.refresh_lock().lock("token"))
                .await
                .expect("a terminated holder's lock is released")
                .unwrap();
        })
        .await;
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_wrap_with_an_absurd_key_derivation_cost_is_skipped() {
        with_test_pool(build_pool, |pool| async move {
            open(&connect(&pool).await, "global")
                .await
                .set("k", "v")
                .unwrap();
            pool.get()
                .await
                .unwrap()
                .execute(
                    "INSERT INTO vault_key_wraps
                         (wrap_id, key_id, kdf_salt, kdf_m_kib, kdf_t, kdf_p, nonce, wrapped)
                     SELECT $1, active_key_id, '\\x00'::bytea, 2147483647, 1, 1,
                            '\\x00'::bytea, '\\x00'::bytea
                     FROM vault_keyring",
                    &[&Uuid::new_v4()],
                )
                .await
                .unwrap();

            let fresh = open(&connect(&pool).await, "global").await;
            assert_eq!(fresh.get("k").as_deref(), Some("v"));
        })
        .await;
    }

    #[test]
    fn advisory_keys_are_stable_per_key() {
        assert_eq!(advisory_key("a"), advisory_key("a"));
        assert_ne!(advisory_key("a"), advisory_key("b"));
    }
}
