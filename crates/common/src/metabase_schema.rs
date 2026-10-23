//! What a Metabase instance's databases actually contain, learned once and read
//! before the model writes SQL.
//!
//! Agents guess column names — measured on a live install, unknown columns and
//! tables were 85% of Metabase tool failures, each costing a model round to
//! recover from. Carrying the real names in the native-query tool schema removes
//! the guess; this store is where those names live between turns and across
//! restarts.

use std::collections::BTreeMap;
use std::sync::Arc;

use eyre::Result;
use serde::{Deserialize, Serialize};

/// One database's names, as the tool schema needs them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DbSchema {
    /// Table names, schema-qualified where the instance reports a schema.
    pub tables: Vec<String>,
    /// Columns for the tables worth describing, keyed exactly as `tables`
    /// spells them.
    pub columns: BTreeMap<String, Vec<String>>,
    /// The database's engine (`postgres`, `mysql`, …), when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub engine: Option<String>,
}

/// How often one table is reached for, and how often that reach was wrong.
///
/// A table queried constantly and never fumbled needs no columns in the prompt;
/// one fumbled daily does.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TableUsage {
    pub hits: u64,
    pub misses: u64,
}

/// A miss costs a whole model round; a hit costs nothing. Ranking weights
/// accordingly.
const MISS_WEIGHT: u64 = 5;

impl TableUsage {
    pub fn weight(&self) -> u64 {
        self.hits + self.misses * MISS_WEIGHT
    }

    pub fn merge(self, other: Self) -> Self {
        Self {
            hits: self.hits + other.hits,
            misses: self.misses + other.misses,
        }
    }
}

/// Everything known about one Metabase instance.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct InstanceSchema {
    pub databases: BTreeMap<i64, DbSchema>,
    pub usage: BTreeMap<(i64, String), TableUsage>,
}

/// A table's name in one case, each part without its quoting, so the name a
/// query wrote and the name the instance reports compare equal.
fn folded(table: &str) -> String {
    table
        .split('.')
        .map(|part| part.trim_matches(['"', '`', '[', ']']))
        .collect::<Vec<_>>()
        .join(".")
        .to_ascii_lowercase()
}

/// A table's name as a query usually reaches for it: [`folded`], without its
/// schema qualifier.
///
/// The schema spells a table `schema.name` wherever the instance reports a
/// schema, while usage is counted under whatever a query wrote — almost always
/// the bare name, in the model's own casing. Matching the two exactly leaves
/// every such database with no usable counter, so its tables rank by name
/// alone and the fumbled ones never reach the tool schema.
fn bare_name(table: &str) -> String {
    let folded = folded(table);
    folded.rsplit('.').next().unwrap_or(&folded).to_string()
}

/// Whether two spellings name one table once the qualifier, the quoting and
/// the case are set aside: `warehouse.Orders` and `public.orders` do.
pub fn names_same_table(one: &str, other: &str) -> bool {
    bare_name(one) == bare_name(other)
}

/// Usage weight for the tables of one database.
///
/// A query that qualified its table says which schema it meant, so its usage
/// counts for that table alone. One that wrote the bare name — or a qualifier
/// the database does not have, a common way to fumble — counts for every
/// table of that name: which schema the engine's search path would have picked
/// is not known here, and ranking a namesake too high costs a few prompt bytes
/// where ranking the real one too low costs the next query.
#[derive(Default)]
struct Weights {
    qualified: BTreeMap<String, u64>,
    bare: BTreeMap<String, u64>,
}

impl Weights {
    fn of(&self, table: &str) -> u64 {
        let weight = |weights: &BTreeMap<String, u64>, name: &str| {
            weights.get(name).copied().unwrap_or_default()
        };
        let name = folded(table);
        match name.contains('.') {
            true => weight(&self.qualified, &name) + weight(&self.bare, &bare_name(table)),
            false => weight(&self.bare, &name),
        }
    }
}

impl InstanceSchema {
    fn weights(&self, database_id: i64, schema: &DbSchema) -> Weights {
        let known: std::collections::BTreeSet<String> = schema
            .tables
            .iter()
            .chain(schema.columns.keys())
            .map(|table| folded(table))
            .collect();
        self.usage
            .iter()
            .filter(|((db, _), _)| *db == database_id)
            .fold(Weights::default(), |mut weights, ((_, table), usage)| {
                let name = folded(table);
                let slot = match name.contains('.') && known.contains(&name) {
                    true => weights.qualified.entry(name),
                    false => weights.bare.entry(bare_name(table)),
                };
                *slot.or_default() += usage.weight();
                weights
            })
    }

    /// The `limit` heaviest of `tables`, all of database `database_id`. Ties
    /// break on name, so a database nobody has queried still ranks stably
    /// rather than by hash order.
    fn heaviest<'a>(
        &self,
        database_id: i64,
        schema: &DbSchema,
        tables: impl Iterator<Item = &'a String>,
        limit: usize,
    ) -> Vec<String> {
        let weights = self.weights(database_id, schema);
        let mut ranked: Vec<(u64, &String)> =
            tables.map(|table| (weights.of(table), table)).collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(b.1)));
        ranked
            .into_iter()
            .take(limit)
            .map(|(_, table)| table.clone())
            .collect()
    }

    /// Tables the sync should describe, most valuable first.
    ///
    /// Ranks every known table, not only those already described: columns are
    /// what [`Self::hot_tables`] needs, so selecting on them there would mean a
    /// database with none could never acquire any.
    pub fn tables_to_describe(&self, database_id: i64, limit: usize) -> Vec<String> {
        self.databases
            .get(&database_id)
            .map(|schema| self.heaviest(database_id, schema, schema.tables.iter(), limit))
            .unwrap_or_default()
    }

    /// Tables worth spending prompt bytes on, most valuable first.
    ///
    /// Only tables whose columns are known are offered: a bare name teaches the
    /// model nothing the table list did not already say.
    pub fn hot_tables(&self, database_id: i64, limit: usize) -> Vec<String> {
        self.databases
            .get(&database_id)
            .map(|schema| self.heaviest(database_id, schema, schema.columns.keys(), limit))
            .unwrap_or_default()
    }

    /// Every database holding a table of this name, with the table as that
    /// database spells it, in id order. `name` may carry a qualifier; only the
    /// table part is compared, since a wrong qualifier is one of the ways a
    /// query misses a table that exists.
    pub fn locate_table(&self, name: &str) -> Vec<(i64, &str)> {
        self.databases
            .iter()
            .flat_map(|(id, schema)| {
                schema
                    .tables
                    .iter()
                    .filter(|table| names_same_table(table, name))
                    .map(|table| (*id, table.as_str()))
            })
            .collect()
    }
}

impl InstanceSchema {
    /// Every database with a known engine, in id order.
    pub fn known_engines(&self) -> Vec<(i64, String)> {
        self.databases
            .iter()
            .filter_map(|(id, db)| db.engine.clone().map(|engine| (*id, engine)))
            .collect()
    }

    /// Take on what the store knows. Stored rows win where the store names a
    /// database's tables or holds a counter; what only this process learned
    /// survives: a database the store has nothing for keeps its in-request
    /// schema, a stored row naming no tables (one `upsert_engines` created)
    /// contributes only its engine, and an engine the store has not recorded
    /// is kept from the local row.
    pub fn absorb(&mut self, loaded: InstanceSchema) {
        for (id, stored) in loaded.databases {
            let merged = match self.databases.remove(&id) {
                None => stored,
                Some(local) => {
                    let engine = stored.engine.clone().or(local.engine.clone());
                    let names = if stored.tables.is_empty() {
                        local
                    } else {
                        stored
                    };
                    DbSchema { engine, ..names }
                }
            };
            self.databases.insert(id, merged);
        }
        self.usage.extend(loaded.usage);
    }

    /// Databases in the order a pass should spend its budget on them: most
    /// fumbled first.
    ///
    /// A pass walks databases sequentially under one request budget, so plain id
    /// order spends the budget on whichever databases happen to sort first —
    /// measured on a live install, 34 databases against a 200-request budget
    /// reached only the first eight, none of them the ones carrying the
    /// failures. Ties keep id order so an install with no history is stable.
    pub fn databases_by_need(&self, candidates: &[i64]) -> Vec<i64> {
        let mut ranked: Vec<(u64, i64)> = candidates
            .iter()
            .map(|id| {
                let weight = self
                    .usage
                    .iter()
                    .filter(|((db, _), _)| db == id)
                    .map(|(_, usage)| usage.weight())
                    .sum();
                (weight, *id)
            })
            .collect();
        ranked.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        ranked.into_iter().map(|(_, id)| id).collect()
    }
}

/// Persistent home for [`InstanceSchema`], shared by every agent on the install
/// and by the sync pass that refreshes it.
///
/// Usage arrives as *deltas* so two hub replicas flushing at once compose
/// instead of clobbering — the whole-snapshot write this replaces is the
/// lost-update shape that silently drops a concurrent writer's counters.
#[async_trait::async_trait]
pub trait MetabaseSchemaStore: Send + Sync {
    /// Everything stored for `instance`, or an empty schema when `site_url`
    /// disagrees with what was stored — never another instance's names.
    async fn load(&self, instance: &str, site_url: &str) -> Result<InstanceSchema>;

    /// Replace one database's names, recording the instance's `site_url`.
    async fn upsert_database(
        &self,
        instance: &str,
        site_url: &str,
        database_id: i64,
        schema: &DbSchema,
    ) -> Result<()>;

    /// Record engines as facts, creating a row for a database not yet
    /// described.
    async fn upsert_engines(
        &self,
        instance: &str,
        site_url: &str,
        engines: &[(i64, String)],
    ) -> Result<()>;

    /// Drop stored databases absent from `keep`, so an upstream deletion stops
    /// appearing in prompts.
    async fn prune_databases(&self, instance: &str, keep: &[i64]) -> Result<()>;

    /// Add usage deltas.
    async fn add_usage(&self, instance: &str, deltas: &[(i64, String, TableUsage)]) -> Result<()>;
}

/// The process-wide store, set once at startup.
///
/// One Postgres-backed store exists per process and every skill load needs the
/// same one; the alternative reaches eleven generic skill-loading call sites
/// with a Metabase-shaped parameter none of them use. Unset means no Postgres,
/// which is the disabled path rather than an error.
static STORE: std::sync::OnceLock<Arc<dyn MetabaseSchemaStore>> = std::sync::OnceLock::new();

/// Install the store. `false` if one was already installed, which only happens
/// if startup ran twice.
pub fn set_store(store: Arc<dyn MetabaseSchemaStore>) -> bool {
    STORE.set(store).is_ok()
}

pub fn store() -> Option<Arc<dyn MetabaseSchemaStore>> {
    STORE.get().cloned()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_with(columns: &[(&str, &[&str])], tables: &[&str]) -> DbSchema {
        DbSchema {
            tables: tables.iter().map(|t| t.to_string()).collect(),
            columns: columns
                .iter()
                .map(|(t, cols)| {
                    (
                        t.to_string(),
                        cols.iter().map(|c| c.to_string()).collect::<Vec<_>>(),
                    )
                })
                .collect(),
            ..Default::default()
        }
    }

    fn with_engine(schema: DbSchema, engine: &str) -> DbSchema {
        DbSchema {
            engine: Some(engine.to_string()),
            ..schema
        }
    }

    #[test]
    fn hot_tables_rank_by_weight_then_name() {
        let mut instance = InstanceSchema::default();
        instance.databases.insert(
            2,
            schema_with(
                &[("busy", &["id"]), ("fumbled", &["id"]), ("quiet", &["id"])],
                &["busy", "fumbled", "quiet", "bare"],
            ),
        );
        instance.usage.insert(
            (2, "busy".into()),
            TableUsage {
                hits: 10,
                misses: 0,
            },
        );
        instance
            .usage
            .insert((2, "fumbled".into()), TableUsage { hits: 0, misses: 3 });
        instance
            .usage
            .insert((2, "quiet".into()), TableUsage { hits: 1, misses: 0 });

        assert_eq!(
            instance.hot_tables(2, 10),
            vec![
                "fumbled".to_string(),
                "busy".to_string(),
                "quiet".to_string()
            ],
            "a fumbled table outranks a busy one; a table without columns is never offered"
        );
        assert_eq!(instance.hot_tables(2, 1), vec!["fumbled".to_string()]);
        assert!(instance.hot_tables(99, 5).is_empty());
    }

    fn usage(pairs: &[(&str, TableUsage)]) -> BTreeMap<(i64, String), TableUsage> {
        pairs
            .iter()
            .map(|(table, usage)| ((2, table.to_string()), *usage))
            .collect()
    }

    #[test]
    fn a_schema_qualified_table_is_ranked_by_the_usage_of_its_bare_name() {
        let mut instance = InstanceSchema::default();
        instance.databases.insert(
            2,
            schema_with(
                &[
                    ("public.accounts", &["id"]),
                    ("public.bookings", &["id"]),
                    ("public.orders", &["id"]),
                ],
                &["public.accounts", "public.bookings", "public.orders"],
            ),
        );
        instance.usage = usage(&[
            ("orders", TableUsage { hits: 1, misses: 3 }),
            ("bookings", TableUsage { hits: 4, misses: 0 }),
        ]);

        let ranked = ["public.orders", "public.bookings", "public.accounts"];
        assert_eq!(instance.hot_tables(2, 3), ranked);
        assert_eq!(instance.tables_to_describe(2, 3), ranked);
    }

    #[test]
    fn usage_counts_for_a_table_whatever_case_the_query_wrote_it_in() {
        let mut instance = InstanceSchema::default();
        instance.databases.insert(
            2,
            schema_with(
                &[("Accounts", &["id"]), ("orders", &["id"])],
                &["Accounts", "orders"],
            ),
        );
        instance.usage = usage(&[
            ("ORDERS", TableUsage { hits: 2, misses: 0 }),
            ("orders", TableUsage { hits: 0, misses: 1 }),
            ("accounts", TableUsage { hits: 1, misses: 0 }),
        ]);

        assert_eq!(instance.hot_tables(2, 1), ["orders"]);
    }

    fn two_schemas_sharing_a_table_name() -> InstanceSchema {
        let mut instance = InstanceSchema::default();
        let tables = ["archive.orders", "public.accounts", "catalogue.orders"];
        instance.databases.insert(
            2,
            schema_with(&tables.map(|table| (table, &["id"] as &[&str])), &tables),
        );
        instance
    }

    #[test]
    fn usage_that_names_the_schema_counts_for_that_schemas_table_alone() {
        let mut instance = two_schemas_sharing_a_table_name();
        instance.usage = usage(&[
            ("Catalogue.Orders", TableUsage { hits: 3, misses: 0 }),
            ("\"public\".\"accounts\"", TableUsage { hits: 1, misses: 0 }),
        ]);

        assert_eq!(
            instance.hot_tables(2, 3),
            ["catalogue.orders", "public.accounts", "archive.orders"]
        );
    }

    /// Which schema an unqualified name resolved to is the engine's business;
    /// both tables rise rather than one being guessed at.
    #[test]
    fn usage_of_a_bare_name_two_schemas_share_counts_for_both() {
        let mut instance = two_schemas_sharing_a_table_name();
        instance.usage = usage(&[
            ("orders", TableUsage { hits: 2, misses: 0 }),
            ("accounts", TableUsage { hits: 1, misses: 0 }),
        ]);

        assert_eq!(
            instance.hot_tables(2, 3),
            ["archive.orders", "catalogue.orders", "public.accounts"]
        );
    }

    #[test]
    fn usage_under_a_schema_the_database_lacks_counts_by_the_bare_name() {
        let mut instance = two_schemas_sharing_a_table_name();
        instance.usage = usage(&[("warehouse.accounts", TableUsage { hits: 0, misses: 2 })]);

        assert_eq!(instance.hot_tables(2, 1), ["public.accounts"]);
    }

    #[test]
    fn usage_in_one_database_does_not_rank_a_namesake_in_another() {
        let mut instance = InstanceSchema::default();
        for id in [2, 3] {
            instance.databases.insert(
                id,
                schema_with(
                    &[("accounts", &["id"]), ("orders", &["id"])],
                    &["accounts", "orders"],
                ),
            );
        }
        instance.usage = usage(&[("orders", TableUsage { hits: 0, misses: 9 })]);

        assert_eq!(instance.hot_tables(2, 1), ["orders"]);
        assert_eq!(instance.hot_tables(3, 1), ["accounts"]);
    }

    #[test]
    fn a_table_is_located_in_every_database_that_has_it_as_each_spells_it() {
        let mut instance = InstanceSchema::default();
        instance
            .databases
            .insert(2, schema_with(&[], &["accounts", "orders"]));
        instance
            .databases
            .insert(5, schema_with(&[], &["public.Orders", "public.catalogues"]));

        assert_eq!(
            instance.locate_table("orders"),
            [(2, "orders"), (5, "public.Orders")]
        );
        assert_eq!(
            instance.locate_table("warehouse.catalogues"),
            [(5, "public.catalogues")]
        );
        assert!(instance.locate_table("payslips").is_empty());
    }

    #[test]
    fn unused_tables_rank_stably_by_name() {
        let mut instance = InstanceSchema::default();
        instance.databases.insert(
            1,
            schema_with(
                &[("b", &["x"]), ("a", &["x"]), ("c", &["x"])],
                &["a", "b", "c"],
            ),
        );
        assert_eq!(
            instance.hot_tables(1, 3),
            vec!["a".to_string(), "b".to_string(), "c".to_string()]
        );
    }

    /// The bootstrap case: a database whose tables have no columns yet must
    /// still yield describe targets, or it can never acquire any.
    #[test]
    fn tables_to_describe_covers_undescribed_tables() {
        let mut instance = InstanceSchema::default();
        instance
            .databases
            .insert(3, schema_with(&[], &["alpha", "beta", "gamma"]));
        assert_eq!(
            instance.tables_to_describe(3, 2),
            vec!["alpha".to_string(), "beta".to_string()],
            "no columns anywhere, yet progress is deterministic"
        );
        assert!(
            instance.hot_tables(3, 2).is_empty(),
            "the prompt still offers nothing, which is why the two differ"
        );

        instance
            .usage
            .insert((3, "gamma".into()), TableUsage { hits: 0, misses: 4 });
        assert_eq!(
            instance.tables_to_describe(3, 1),
            vec!["gamma".to_string()],
            "a fumbled table is described first"
        );
    }

    #[test]
    fn databases_are_ordered_by_where_the_failures_are() {
        let mut instance = InstanceSchema::default();
        instance
            .usage
            .insert((70, "busy".into()), TableUsage { hits: 0, misses: 9 });
        instance
            .usage
            .insert((12, "quiet".into()), TableUsage { hits: 2, misses: 0 });
        assert_eq!(
            instance.databases_by_need(&[3, 12, 70]),
            vec![70, 12, 3],
            "the fumbled database is described before one that merely sorts first"
        );
        assert_eq!(
            InstanceSchema::default().databases_by_need(&[9, 2, 5]),
            vec![2, 5, 9],
            "no history yet: stable id order"
        );
    }

    #[test]
    fn absorb_prefers_stored_rows_and_keeps_local_only_ones() {
        let mut local = InstanceSchema::default();
        local.databases.insert(1, schema_with(&[], &["guessed"]));
        local
            .databases
            .insert(2, schema_with(&[("local", &["x"])], &["local"]));
        local
            .usage
            .insert((1, "guessed".into()), TableUsage { hits: 1, misses: 0 });
        local
            .usage
            .insert((2, "local".into()), TableUsage { hits: 3, misses: 0 });

        let mut stored = InstanceSchema::default();
        stored
            .databases
            .insert(1, schema_with(&[("people", &["id"])], &["people"]));
        stored
            .usage
            .insert((1, "guessed".into()), TableUsage { hits: 9, misses: 2 });

        local.absorb(stored);

        assert_eq!(
            local.databases[&1].tables,
            vec!["people".to_string()],
            "the store's row replaces the local one for a database it knows"
        );
        assert_eq!(
            local.databases[&2].tables,
            vec!["local".to_string()],
            "a database the store has nothing for is kept"
        );
        assert_eq!(
            local.usage[&(1, "guessed".to_string())],
            TableUsage { hits: 9, misses: 2 }
        );
        assert_eq!(
            local.usage[&(2, "local".to_string())],
            TableUsage { hits: 3, misses: 0 }
        );
    }

    #[test]
    fn known_engines_lists_databases_with_an_engine_in_id_order() {
        let mut instance = InstanceSchema::default();
        instance
            .databases
            .insert(9, with_engine(schema_with(&[], &[]), "postgres"));
        instance.databases.insert(4, schema_with(&[], &["bare"]));
        instance
            .databases
            .insert(2, with_engine(schema_with(&[], &["t"]), "mysql"));
        assert_eq!(
            instance.known_engines(),
            vec![(2, "mysql".to_string()), (9, "postgres".to_string())]
        );
        assert!(InstanceSchema::default().known_engines().is_empty());
    }

    #[test]
    fn absorb_keeps_a_local_engine_when_the_store_has_none() {
        let mut local = InstanceSchema::default();
        local
            .databases
            .insert(1, with_engine(schema_with(&[], &["guessed"]), "mysql"));
        local
            .databases
            .insert(2, with_engine(schema_with(&[], &["local"]), "sqlite"));

        let mut stored = InstanceSchema::default();
        stored.databases.insert(1, schema_with(&[], &["people"]));
        stored
            .databases
            .insert(2, with_engine(schema_with(&[], &["people"]), "postgres"));

        local.absorb(stored);

        assert_eq!(
            local.databases[&1].engine.as_deref(),
            Some("mysql"),
            "a stored row without an engine keeps the one learned here"
        );
        assert_eq!(
            local.databases[&1].tables,
            vec!["people".to_string()],
            "the stored names still win"
        );
        assert_eq!(
            local.databases[&2].engine.as_deref(),
            Some("postgres"),
            "a stored engine wins over the local one"
        );
    }

    #[test]
    fn absorb_keeps_local_names_when_the_store_row_is_engine_only() {
        let mut local = InstanceSchema::default();
        local
            .databases
            .insert(1, schema_with(&[("local", &["id"])], &["local"]));

        let mut stored = InstanceSchema::default();
        stored
            .databases
            .insert(1, with_engine(schema_with(&[], &[]), "postgres"));

        local.absorb(stored);

        assert_eq!(local.databases[&1].tables, vec!["local".to_string()]);
        assert_eq!(
            local.databases[&1].columns["local"],
            vec!["id".to_string()],
            "a placeholder row must not erase names learned in-request"
        );
        assert_eq!(local.databases[&1].engine.as_deref(), Some("postgres"));
    }

    #[test]
    fn usage_merges_additively() {
        let merged = TableUsage { hits: 2, misses: 1 }.merge(TableUsage { hits: 3, misses: 4 });
        assert_eq!(merged, TableUsage { hits: 5, misses: 5 });
        assert_eq!(merged.weight(), 5 + 5 * MISS_WEIGHT);
    }
}
