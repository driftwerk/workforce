//! What the native-query tool schema says about an instance's databases.
//!
//! Pure rendering over an [`InstanceSchema`], kept apart from the client that
//! learns it: the text rides in a tool definition, so what it contains — the
//! ordering, the caps, the overflow wording — is a prompt-budget decision that
//! must be testable without a client and must not move between publishes.

use workforce_common::metabase_schema::{DbSchema, InstanceSchema};

/// The static `database_id` schema sentence, used until any engine has been
/// harvested (a `metabase.database_list` call fills the cache).
pub(crate) const DATABASE_ID_STATIC_DESCRIPTION: &str =
    "Database id. Match your SQL to the engine's dialect (check `engine` via metabase.database_list).";

/// Cap on the engine pairs inlined into the schema hint.
const SCHEMA_HINT_MAX_ENGINES: usize = 8;

/// The `database_id` property description: known engines inlined when the
/// default instance's cache has any, else the static sentence.
pub(crate) fn database_id_description(known_engines: &[(i64, String)]) -> String {
    if known_engines.is_empty() {
        return DATABASE_ID_STATIC_DESCRIPTION.to_string();
    }
    let pairs = known_engines
        .iter()
        .take(SCHEMA_HINT_MAX_ENGINES)
        .map(|(id, engine)| format!("{id}={engine}"))
        .collect::<Vec<_>>()
        .join(", ");
    format!("Database id. Known engines: {pairs}. Match your SQL to the engine's dialect.")
}

/// How many tables get their columns spelled out in the tool schema.
///
/// Measured on one install, the tables carrying column mistakes are heavily
/// concentrated — a couple of dozen covered the large majority — while the
/// mistaken *names* are a long tail. That asymmetry is what makes this
/// affordable. The number is a default, not a fact about anyone's database:
/// which tables fill the slots is learned per install from usage.
const PROMPT_HOT_TABLES: usize = 25;

/// Columns listed per hot table before the rest are summarised away.
const PROMPT_COLUMNS_PER_TABLE: usize = 40;

/// Hard ceiling on the whole schema block, whatever the caps above allow. This
/// rides in the tool definition on every turn, so it is a prompt-budget
/// decision, not a display one.
const PROMPT_SCHEMA_MAX_BYTES: usize = 8_000;

/// Worst-case width of the trailing `(+N more)`, reserved up front because it
/// is appended after the budget loop has already spent everything.
const OVERFLOW_RESERVE: usize = 24;

/// Whether a name can be pasted into SQL as a bare identifier.
///
/// NOT a formality. Metabase's `fields[].name` is usually the physical column,
/// but for a model or question-backed table it comes back humanised — measured
/// on a live instance, 9 of 81 described tables returned names like
/// `Order Id` and `Organisation Name`. The reactive enrichment can afford to
/// echo those (the reader is recovering from a failure and can see the shape);
/// this block cannot, because it is presented up front as the names to write.
/// Teaching the model `SELECT Order Id FROM …` is worse than letting it guess.
fn usable_as_identifier(name: &str) -> bool {
    !name.is_empty()
        && !name.starts_with(|c: char| c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// The schema block carried in `database_id`'s description: the hot tables with
/// their columns, then whatever table names still fit.
///
/// Returns empty when nothing is known: a cold cache must contribute nothing
/// rather than an empty-looking claim about the database.
fn schema_block(database_id: i64, schema: &DbSchema, hot: &[String]) -> String {
    if schema.tables.is_empty() {
        return String::new();
    }
    let prefix = format!("db {database_id}: ");
    let mut budget = PROMPT_SCHEMA_MAX_BYTES.saturating_sub(prefix.len());
    let mut described: Vec<String> = Vec::new();
    let mut spelled: std::collections::HashSet<&String> = std::collections::HashSet::new();

    for table in hot.iter().take(PROMPT_HOT_TABLES) {
        let Some(all) = schema.columns.get(table) else {
            continue;
        };
        // A table whose names are not writable as SQL is left to the reactive
        // path rather than half-quoted into the prompt.
        let columns: Vec<&String> = all.iter().filter(|c| usable_as_identifier(c)).collect();
        if columns.len() * 2 < all.len() {
            continue;
        }
        let shown = columns
            .iter()
            .take(PROMPT_COLUMNS_PER_TABLE)
            .map(|c| c.as_str())
            .collect::<Vec<_>>()
            .join(", ");
        let rendered = match columns.len().saturating_sub(PROMPT_COLUMNS_PER_TABLE) {
            0 => format!("{table}({shown})"),
            n => format!("{table}({shown}, +{n} more)"),
        };
        let cost = rendered.len() + if described.is_empty() { 0usize } else { 2 };
        if cost > budget {
            break;
        }
        budget -= cost;
        described.push(rendered);
        spelled.insert(table);
    }

    // Tables we did not spell out still deserve their bare names: preventing an
    // unknown-table guess is cheaper than preventing an unknown-column one.
    let rest: Vec<&String> = schema
        .tables
        .iter()
        .filter(|t| !spelled.contains(*t))
        .collect();
    let mut listed: Vec<&str> = Vec::new();
    // The "(+N more)" tail is written after the loop, so its worst case has to
    // be reserved before it — budgeting only what we append as we go is how the
    // block overshot its cap by the width of that suffix.
    let head_cost =
        if described.is_empty() { 0usize } else { 2 } + "other tables: ".len() + OVERFLOW_RESERVE;
    if !rest.is_empty() && budget > head_cost {
        budget -= head_cost;
        for name in &rest {
            let cost = name.len() + if listed.is_empty() { 0usize } else { 2 };
            if cost > budget {
                break;
            }
            budget -= cost;
            listed.push(name.as_str());
        }
    }

    let mut sections: Vec<String> = Vec::new();
    if !described.is_empty() {
        sections.push(described.join("; "));
    }
    if !listed.is_empty() {
        let remaining = rest.len() - listed.len();
        let names = listed.join(", ");
        sections.push(match remaining {
            0 => format!("other tables: {names}"),
            n => format!("other tables: {names} (+{n} more)"),
        });
    }
    match sections.is_empty() {
        true => String::new(),
        false => format!("{prefix}{}", sections.join("; ")),
    }
}

/// [`database_id_description`] plus the cached schema, when there is any.
///
/// Split from the engine-only form so the existing contract (and its test)
/// stays exactly as it was when nothing has been learned yet.
fn database_id_description_with_schema(
    known_engines: &[(i64, String)],
    schema_blocks: &[String],
) -> String {
    let base = database_id_description(known_engines);
    match schema_blocks.is_empty() {
        true => base,
        false => format!(
            "{base} Known schema — use these names, do not invent others: {}",
            schema_blocks.join(" | ")
        ),
    }
}

/// The `database_id` description for one instance as it stands now: every
/// database the snapshot knows, engines and hot tables first, in id order.
pub(crate) fn database_id_description_for(schema: &InstanceSchema) -> String {
    let blocks: Vec<String> = schema
        .databases
        .iter()
        .map(|(db, db_schema)| {
            schema_block(*db, db_schema, &schema.hot_tables(*db, PROMPT_HOT_TABLES))
        })
        .filter(|block| !block.is_empty())
        .collect();
    database_id_description_with_schema(&schema.known_engines(), &blocks)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn schema_of(tables: &[&str], columns: &[(&str, &[&str])]) -> DbSchema {
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

    /// The whole point: the names reach the model before it writes SQL.
    #[test]
    fn the_schema_block_spells_out_hot_tables_and_names_the_rest() {
        let schema = schema_of(
            &["people", "orders", "audit", "legacy"],
            &[
                ("people", &["id", "email_address"]),
                ("orders", &["id", "starts_at"]),
            ],
        );
        let block = schema_block(2, &schema, &["people".into(), "orders".into()]);
        assert!(block.starts_with("db 2: "), "{block}");
        assert!(block.contains("people(id, email_address)"), "{block}");
        assert!(block.contains("orders(id, starts_at)"), "{block}");
        assert!(
            block.contains("other tables: audit, legacy"),
            "tables without columns still get named: {block}"
        );
    }

    /// Taken from a live Metabase: a model-backed table reports humanised
    /// field names, which are not writable as SQL. Echoing them in an error the
    /// reader is already debugging is fair; presenting them up front as the
    /// names to use is how you teach `SELECT Order Id FROM …`.
    #[test]
    fn a_table_reporting_display_names_is_left_out_of_the_block() {
        let schema = schema_of(
            &["reporting__orders", "people"],
            &[
                (
                    "reporting__orders",
                    &["Order Id", "Block Id", "Organisation Name"],
                ),
                ("people", &["id", "email_address"]),
            ],
        );
        let block = schema_block(2, &schema, &["reporting__orders".into(), "people".into()]);
        assert!(
            !block.contains("Order Id"),
            "display names must never be offered as SQL: {block}"
        );
        assert!(block.contains("people(id, email_address)"), "{block}");
        assert!(
            block.contains("other tables: reporting__orders"),
            "the table is still named, just not spelled out: {block}"
        );
    }

    #[test]
    fn usable_as_identifier_accepts_sql_names_and_rejects_prose() {
        assert!(usable_as_identifier("email_address"));
        assert!(usable_as_identifier("id"));
        assert!(usable_as_identifier("col$1"));
        assert!(!usable_as_identifier("Order Id"));
        assert!(!usable_as_identifier("2fa"));
        assert!(!usable_as_identifier(""));
        assert!(!usable_as_identifier("total-cost"));
    }

    /// A cold cache must say nothing rather than imply the database is empty.
    #[test]
    fn an_unknown_schema_renders_nothing() {
        assert_eq!(schema_block(2, &DbSchema::default(), &[]), "");
        assert_eq!(
            database_id_description_with_schema(&[(2, "mysql".to_string())], &[]),
            database_id_description(&[(2, "mysql".to_string())]),
            "no learned schema leaves the existing description untouched"
        );
    }

    /// The block rides in every turn's tool definition, so the cap is a
    /// prompt-budget guarantee, not a display nicety.
    #[test]
    fn the_schema_block_stays_inside_its_byte_cap() {
        let many: Vec<String> = (0..400).map(|i| format!("table_{i:04}")).collect();
        let cols: Vec<String> = (0..200).map(|i| format!("column_{i:04}")).collect();
        let refs: Vec<&str> = many.iter().map(String::as_str).collect();
        let col_refs: Vec<&str> = cols.iter().map(String::as_str).collect();
        let pairs: Vec<(&str, &[&str])> =
            refs.iter().take(50).map(|t| (*t, &col_refs[..])).collect();
        let schema = schema_of(&refs, &pairs);
        let block = schema_block(7, &schema, &many[..50]);
        assert!(
            block.len() <= PROMPT_SCHEMA_MAX_BYTES,
            "block was {} bytes",
            block.len()
        );
        assert!(block.contains("more)"), "overflow is admitted: {block}");
    }

    /// Only the tables we can actually describe earn their columns; a table
    /// listed without columns teaches the model nothing it did not have.
    #[test]
    fn a_hot_table_without_columns_is_not_spelled_out() {
        let schema = schema_of(&["a", "b"], &[("b", &["x"])]);
        let block = schema_block(1, &schema, &["a".into(), "b".into()]);
        assert!(!block.contains("a("), "{block}");
        assert!(block.contains("b(x)"), "{block}");
        assert!(block.contains("other tables: a"), "{block}");
    }

    #[test]
    fn database_id_description_inlines_known_engines_capped() {
        assert_eq!(database_id_description(&[]), DATABASE_ID_STATIC_DESCRIPTION);
        assert_eq!(
            database_id_description(&[(2, "mysql".to_string()), (5, "postgres".to_string())]),
            "Database id. Known engines: 2=mysql, 5=postgres. Match your SQL to the engine's dialect."
        );
        let many: Vec<(i64, String)> = (1..=12).map(|i| (i, "mysql".to_string())).collect();
        let desc = database_id_description(&many);
        assert!(desc.contains("8=mysql"));
        assert!(!desc.contains("9=mysql"), "capped at 8 entries");
    }

    #[test]
    fn render_covers_every_known_database_in_id_order_without_engines() {
        let mut schema = InstanceSchema::default();
        schema
            .databases
            .insert(7, schema_of(&["audit"], &[("audit", &["id", "actor"])]));
        schema
            .databases
            .insert(2, schema_of(&["people"], &[("people", &["id", "email"])]));

        let description = database_id_description_for(&schema);
        assert!(
            description.starts_with(DATABASE_ID_STATIC_DESCRIPTION),
            "{description}"
        );
        let db2 = description.find("db 2:").expect("db 2 rendered");
        let db7 = description
            .find("db 7:")
            .expect("db 7 rendered without an engine");
        assert!(db2 < db7, "id order: {description}");

        schema.databases.get_mut(&2).unwrap().engine = Some("mysql".into());
        let with_engine = database_id_description_for(&schema);
        assert!(
            with_engine.starts_with("Database id. Known engines: 2=mysql."),
            "{with_engine}"
        );
        assert!(
            with_engine.contains("db 2: people(id, email)"),
            "{with_engine}"
        );
        assert!(
            with_engine.contains("db 7: audit(id, actor)"),
            "{with_engine}"
        );
    }
}
