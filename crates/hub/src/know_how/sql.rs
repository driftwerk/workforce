//! What a successful SQL query says that is safe to share: the tables and
//! columns it read (checked against the schema the Metabase sync recorded)
//! and its shape with every value masked. A shape keeps only SQL words,
//! known table and column names, and aliases renamed positionally; anything
//! else in the query (a value outside quotes, a name the schema does not
//! know) rejects the shape, so nothing a person typed can ride along.

use std::collections::{BTreeSet, HashMap};

use workforce_common::metabase_schema::DbSchema;
use workforce_skills::adapters::metabase::tools::SchemaError;

/// Longest shape kept, in characters.
const MAX_SHAPE: usize = 2_500;

#[derive(Debug, Clone, PartialEq)]
pub struct Analysis {
    /// Canonical names (as the schema store has them), sorted.
    pub tables: Vec<String>,
    /// Columns the query names, as `table.column` with the table it belongs
    /// to (through an alias or the only table read that has it), sorted.
    pub columns: Vec<String>,
    /// The query with every value masked, or `None` when it holds anything
    /// that is not SQL or a known name.
    pub shape: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
enum Tok {
    Word(String),
    /// A quoted identifier: `"x"`, `` `x` `` or `[x]`.
    Quoted(String),
    /// A string or number literal, or a Metabase `{{variable}}`.
    Value,
    Punct(String),
    /// Anything else: a character no SQL operator uses. Rejects the shape.
    Other,
}

/// Characters SQL operators and punctuation are made of.
const OPERATOR_CHARS: &str = "(),.;*+-/%<>=!|&^~?:[]";

fn tokenize(sql: &str) -> Vec<Tok> {
    let c: Vec<char> = sql.chars().collect();
    let mut out = Vec::new();
    let mut i = 0;
    while i < c.len() {
        let ch = c[i];
        if ch.is_whitespace() {
            i += 1;
        } else if ch == '-' && c.get(i + 1) == Some(&'-') {
            while i < c.len() && c[i] != '\n' {
                i += 1;
            }
        } else if ch == '/' && c.get(i + 1) == Some(&'*') {
            i += 2;
            while i + 1 < c.len() && !(c[i] == '*' && c[i + 1] == '/') {
                i += 1;
            }
            i += 2;
        } else if ch == '{' && c.get(i + 1) == Some(&'{') {
            while i + 1 < c.len() && !(c[i] == '}' && c[i + 1] == '}') {
                i += 1;
            }
            i += 2;
            out.push(Tok::Value);
        } else if ch == '[' && c.get(i + 1) == Some(&'[') {
            // Metabase optional clause markers: the clause inside stays.
            i += 2;
        } else if ch == ']' && c.get(i + 1) == Some(&']') {
            i += 2;
        } else if ch == '$'
            && c[i + 1..].iter().position(|x| *x == '$').is_some_and(|n| {
                c[i + 1..i + 1 + n]
                    .iter()
                    .all(|x| x.is_alphanumeric() || *x == '_')
            })
        {
            // A dollar-quoted string: `$$...$$` or `$tag$...$tag$`.
            let n = c[i + 1..].iter().position(|x| *x == '$').unwrap_or(0);
            let tag = &c[i..i + n + 2];
            let mut j = i + n + 2;
            while j < c.len() && !c[j..].starts_with(tag) {
                j += 1;
            }
            i = (j + tag.len()).min(c.len());
            out.push(Tok::Value);
        } else if ch == '\'' {
            i += 1;
            while i < c.len() {
                // A backslash escapes in some dialects; treating it so can only
                // mask more.
                if c[i] == '\\' {
                    i += 2;
                    continue;
                }
                if c[i] == '\'' {
                    if c.get(i + 1) == Some(&'\'') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i += 1;
            out.push(Tok::Value);
        } else if ch == '"' || ch == '`' || ch == '[' {
            let close = match ch {
                '[' => ']',
                other => other,
            };
            let start = i + 1;
            i += 1;
            while i < c.len() && c[i] != close {
                i += 1;
            }
            out.push(Tok::Quoted(c[start..i.min(c.len())].iter().collect()));
            i += 1;
        } else if ch.is_ascii_digit()
            || (ch == '.' && c.get(i + 1).is_some_and(|d| d.is_ascii_digit()))
        {
            while i < c.len() && (c[i].is_ascii_alphanumeric() || c[i] == '.') {
                i += 1;
            }
            out.push(Tok::Value);
        } else if ch.is_alphabetic() || ch == '_' {
            let start = i;
            while i < c.len() && (c[i].is_alphanumeric() || c[i] == '_' || c[i] == '$') {
                i += 1;
            }
            out.push(Tok::Word(c[start..i].iter().collect()));
        } else if ch == ':' && c.get(i + 1) == Some(&':') {
            out.push(Tok::Punct("::".into()));
            i += 2;
        } else if "<>!=|".contains(ch) && c.get(i + 1).is_some_and(|n| "=>|".contains(*n)) {
            out.push(Tok::Punct(c[i..i + 2].iter().collect()));
            i += 2;
        } else if OPERATOR_CHARS.contains(ch) {
            out.push(Tok::Punct(ch.to_string()));
            i += 1;
        } else {
            out.push(Tok::Other);
            i += 1;
        }
    }
    out
}

const KEYWORDS: &[&str] = &[
    "select",
    "from",
    "where",
    "and",
    "or",
    "not",
    "in",
    "is",
    "null",
    "as",
    "on",
    "join",
    "left",
    "right",
    "inner",
    "outer",
    "full",
    "cross",
    "group",
    "by",
    "order",
    "having",
    "limit",
    "offset",
    "distinct",
    "case",
    "when",
    "then",
    "else",
    "end",
    "with",
    "union",
    "all",
    "except",
    "intersect",
    "asc",
    "desc",
    "between",
    "like",
    "ilike",
    "exists",
    "any",
    "some",
    "true",
    "false",
    "interval",
    "date",
    "timestamp",
    "time",
    "over",
    "partition",
    "rows",
    "range",
    "preceding",
    "following",
    "unbounded",
    "current",
    "row",
    "filter",
    "lateral",
    "using",
    "natural",
    "top",
    "fetch",
    "first",
    "next",
    "only",
    "nulls",
    "last",
    "cast",
    "at",
    "zone",
    "escape",
    "similar",
    "to",
    "recursive",
    "values",
    "within",
    "year",
    "month",
    "day",
    "week",
    "quarter",
    "hour",
    "minute",
    "second",
    "epoch",
    "dow",
    "isodow",
    "doy",
    "integer",
    "int",
    "bigint",
    "smallint",
    "numeric",
    "decimal",
    "text",
    "varchar",
    "char",
    "boolean",
    "bool",
    "float",
    "double",
    "precision",
    "real",
    "timestamptz",
    "date_trunc",
    "count",
    "sum",
    "avg",
    "min",
    "max",
    "coalesce",
    "nullif",
    "extract",
    "to_char",
    "to_date",
    "to_timestamp",
    "now",
    "round",
    "floor",
    "ceil",
    "ceiling",
    "abs",
    "lower",
    "upper",
    "trim",
    "length",
    "substring",
    "substr",
    "concat",
    "string_agg",
    "array_agg",
    "row_number",
    "rank",
    "dense_rank",
    "lag",
    "lead",
    "date_part",
    "dateadd",
    "datediff",
    "date_add",
    "date_sub",
    "greatest",
    "least",
    "ifnull",
    "isnull",
    "convert",
    "format",
    "percentile_cont",
    "percentile_disc",
    "median",
    "stddev",
    "variance",
    "split_part",
    "replace",
    "position",
    "strpos",
    "left",
    "right",
    "generate_series",
    "unnest",
    "array",
    "json_extract",
    "json_extract_path_text",
    "jsonb",
    "json",
    "bool_or",
    "bool_and",
    "every",
    "mod",
    "power",
    "sqrt",
    "ln",
    "log",
    "exp",
    "sign",
    "trunc",
    "current_date",
    "current_timestamp",
    "localtimestamp",
    "age",
    "justify_interval",
    "make_date",
    "ntile",
    "first_value",
    "last_value",
    "nth_value",
    "cume_dist",
    "percent_rank",
    "regexp_replace",
    "regexp_matches",
    "initcap",
    "lpad",
    "rpad",
    "reverse",
    "md5",
    "uuid",
    "jsonb_array_elements",
    "jsonb_array_elements_text",
    "jsonb_typeof",
    "jsonb_object_keys",
    "jsonb_array_length",
    "jsonb_pretty",
    "jsonb_build_object",
    "jsonb_agg",
    "jsonb_each",
    "jsonb_each_text",
    "jsonb_extract_path_text",
    "json_build_object",
    "json_agg",
    "json_array_elements",
    "json_object_keys",
    "json_typeof",
    "to_jsonb",
    "to_json",
    "ordinality",
    "date_format",
    "curdate",
    "curtime",
    "getdate",
    "sysdate",
    "utc_timestamp",
    "str_to_date",
    "timestampdiff",
    "timestampadd",
    "from_unixtime",
    "unix_timestamp",
    "weekday",
    "dayofweek",
    "dayofmonth",
    "dayofyear",
    "yearweek",
    "last_day",
    "datepart",
    "datename",
    "eomonth",
    "json_contains",
    "json_unquote",
    "json_length",
    "json_arrayagg",
    "json_objectagg",
    "group_concat",
    "separator",
    "regexp",
    "rlike",
    "listagg",
    "nvl",
    "nvl2",
    "iif",
    "array_length",
    "array_to_string",
    "string_to_array",
    "cardinality",
    "any_value",
    "unsigned",
    "signed",
    "isoyear",
    "millisecond",
    "microsecond",
    "localtime",
    "distinct_on",
    "rollup",
    "cube",
    "grouping",
    "sets",
    "qualify",
    "ignore",
    "respect",
    "approx_count_distinct",
];

/// Names the schema store knows, lowercased, mapped to their canonical form.
struct Known {
    /// `orders` and `public.orders` both map to the store's name.
    tables: HashMap<String, String>,
    /// Column → canonical column, for the tables the query reads.
    columns: HashMap<String, String>,
}

fn known_tables(schema: &DbSchema) -> HashMap<String, String> {
    let mut out = HashMap::new();
    for t in &schema.tables {
        let lower = t.to_lowercase();
        out.insert(lower.clone(), t.clone());
        if let Some((_, short)) = lower.rsplit_once('.') {
            out.entry(short.to_string()).or_insert_with(|| t.clone());
        }
    }
    out
}

/// A dotted name starting at `i` (`a`, `a.b`, `"a"."b"`) and how many
/// tokens it spans.
fn dotted(toks: &[Tok], i: usize) -> Option<(String, usize)> {
    let part = |t: &Tok| match t {
        Tok::Word(w) | Tok::Quoted(w) => Some(w.to_lowercase()),
        _ => None,
    };
    let mut name = part(toks.get(i)?)?;
    let mut n = 1;
    while toks.get(i + n) == Some(&Tok::Punct(".".into())) {
        let Some(next) = toks.get(i + n + 1).and_then(part) else {
            break;
        };
        name.push('.');
        name.push_str(&next);
        n += 2;
    }
    Some((name, n))
}

/// Tables named after FROM / JOIN, resolved against the schema; unknown
/// references (CTEs, subquery aliases) are skipped.
/// Tables named after FROM / JOIN, each with the names it goes by in the
/// query (its own, its short name, its alias).
fn referenced(
    toks: &[Tok],
    tables: &HashMap<String, String>,
) -> (BTreeSet<String>, HashMap<String, String>) {
    let mut out = BTreeSet::new();
    let mut names: HashMap<String, String> = HashMap::new();
    for i in 0..toks.len() {
        let Tok::Word(w) = &toks[i] else { continue };
        let w = w.to_lowercase();
        if w != "from" && w != "join" {
            continue;
        }
        let mut j = i + 1;
        loop {
            let Some((name, n)) = dotted(toks, j) else {
                break;
            };
            let canon = resolve_table(&name, tables);
            if let Some(canon) = &canon {
                out.insert(canon.clone());
                names.insert(name.clone(), canon.clone());
                if let Some((_, short)) = name.rsplit_once('.') {
                    names.insert(short.to_string(), canon.clone());
                }
            }
            j += n;
            // An alias, then a comma for another table in a FROM list.
            let alias = match toks.get(j) {
                Some(Tok::Word(a)) if a.eq_ignore_ascii_case("as") => {
                    j += 2;
                    match toks.get(j - 1) {
                        Some(Tok::Word(x)) | Some(Tok::Quoted(x)) => Some(x.to_lowercase()),
                        _ => None,
                    }
                }
                Some(Tok::Word(a)) if !KEYWORDS.contains(&a.to_lowercase().as_str()) => {
                    j += 1;
                    Some(a.to_lowercase())
                }
                _ => None,
            };
            if let (Some(alias), Some(canon)) = (alias, canon) {
                names.insert(alias, canon);
            }
            if w == "from" && toks.get(j) == Some(&Tok::Punct(",".into())) {
                j += 1;
                continue;
            }
            break;
        }
    }
    (out, names)
}

fn resolve_table(name: &str, tables: &HashMap<String, String>) -> Option<String> {
    if let Some(t) = tables.get(name) {
        return Some(t.clone());
    }
    // `db.schema.table` where the store keeps `schema.table`.
    let mut rest = name;
    while let Some((_, tail)) = rest.split_once('.') {
        if let Some(t) = tables.get(tail) {
            return Some(t.clone());
        }
        rest = tail;
    }
    None
}

pub fn analyse(sql: &str, schema: &DbSchema) -> Analysis {
    let toks = tokenize(sql);
    let tables_known = known_tables(schema);
    let (tables, table_names) = referenced(&toks, &tables_known);
    let mut columns_known: HashMap<String, String> = HashMap::new();
    // column -> the tables read that have it
    let mut owners: HashMap<String, Vec<String>> = HashMap::new();
    for t in &tables {
        for c in schema.columns.get(t).into_iter().flatten() {
            columns_known.insert(c.to_lowercase(), c.clone());
            owners.entry(c.to_lowercase()).or_default().push(t.clone());
        }
    }
    let known = Known {
        tables: tables_known,
        columns: columns_known,
    };
    let mut used: BTreeSet<String> = BTreeSet::new();
    let mut i = 0;
    while i < toks.len() {
        let Some((name, n)) = dotted(&toks, i) else {
            i += 1;
            continue;
        };
        let (owner, column) = match name.rsplit_once('.') {
            Some((prefix, col)) => (table_names.get(prefix).cloned(), col.to_string()),
            None => (None, name.clone()),
        };
        if let Some(c) = known.columns.get(&column) {
            let table = owner.or_else(|| match owners.get(&column).map(Vec::as_slice) {
                Some([only]) => Some(only.clone()),
                _ => None,
            });
            if let Some(t) = table.filter(|t| owners.get(&column).is_some_and(|o| o.contains(t))) {
                used.insert(format!("{t}.{c}"));
            }
        }
        i += n;
    }
    let shape = if tables.is_empty() {
        None
    } else {
        shape(&toks, &known)
    };
    Analysis {
        tables: tables.into_iter().collect(),
        columns: used.into_iter().collect(),
        shape,
    }
}

/// The masked shape (see the module docs), or `None` when anything in the
/// query is neither SQL nor a known name.
fn shape(toks: &[Tok], known: &Known) -> Option<String> {
    // Aliases: a name after AS, after a table reference, or a CTE name.
    let mut aliases: HashMap<String, String> = HashMap::new();
    let mut ctes = 0;
    let mut alias_n = 0;
    for i in 0..toks.len() {
        let word = |k: usize| match toks.get(k) {
            Some(Tok::Word(w)) | Some(Tok::Quoted(w)) => Some(w.to_lowercase()),
            _ => None,
        };
        let Some(w) = word(i) else { continue };
        let is_cte_name = (i > 0 && word(i - 1).as_deref() == Some("with"))
            || (i > 0
                && toks.get(i - 1) == Some(&Tok::Punct(",".into()))
                && word(i + 1).as_deref() == Some("as")
                && toks.get(i + 2) == Some(&Tok::Punct("(".into())));
        if is_cte_name && word(i + 1).as_deref() == Some("as") && !aliases.contains_key(&w) {
            ctes += 1;
            aliases.insert(w, format!("cte{ctes}"));
            continue;
        }
        let after_as = i > 0 && word(i - 1).as_deref() == Some("as");
        let after_table = i > 0 && {
            let mut k = i;
            // walk back over a dotted table name
            let mut seen_name = false;
            while k > 0 {
                k -= 1;
                match &toks[k] {
                    Tok::Word(p) | Tok::Quoted(p)
                        if !KEYWORDS.contains(&p.to_lowercase().as_str()) =>
                    {
                        seen_name = true;
                        if k > 0 && toks[k - 1] == Tok::Punct(".".into()) {
                            k -= 1;
                            continue;
                        }
                        break;
                    }
                    _ => break,
                }
            }
            seen_name
                && k > 0
                && matches!(word(k - 1).as_deref(), Some("from") | Some("join"))
                && dotted(toks, k).is_some_and(|(n, len)| {
                    k + len == i && resolve_table(&n, &known.tables).is_some()
                })
        };
        if (after_as || after_table)
            && !KEYWORDS.contains(&w.as_str())
            && !aliases.contains_key(&w)
            && !known.columns.contains_key(&w)
        {
            alias_n += 1;
            aliases.insert(w, format!("a{alias_n}"));
        }
    }

    let mut out: Vec<String> = Vec::new();
    let mut i = 0;
    while i < toks.len() {
        match &toks[i] {
            Tok::Value => out.push("?".into()),
            Tok::Punct(p) => out.push(p.clone()),
            Tok::Other => return None,
            Tok::Word(_) | Tok::Quoted(_) => {
                let (name, n) = dotted(toks, i)?;
                let rendered = if let Some(t) = resolve_table(&name, &known.tables)
                    .filter(|_| name.contains('.') || known.tables.contains_key(&name))
                {
                    t
                } else {
                    let parts: Vec<&str> = name.split('.').collect();
                    let mut rendered = Vec::new();
                    for part in &parts {
                        if let Some(a) = aliases.get(*part) {
                            rendered.push(a.clone());
                        } else if let Some(c) = known.columns.get(*part) {
                            rendered.push(c.clone());
                        } else if parts.len() == 1 && KEYWORDS.contains(part) {
                            rendered.push(part.to_uppercase());
                        } else if *part == "*" {
                            rendered.push("*".into());
                        } else {
                            return None;
                        }
                    }
                    rendered.join(".")
                };
                out.push(rendered);
                i += n;
                continue;
            }
        }
        i += 1;
    }
    let mut text = String::new();
    for (k, t) in out.iter().enumerate() {
        // A function's parenthesis sits against its name; a clause's does not.
        const CLAUSE: &[&str] = &[
            "IN",
            "AS",
            "ON",
            "AND",
            "OR",
            "NOT",
            "EXISTS",
            "USING",
            "OVER",
            "VALUES",
            "FROM",
            "JOIN",
            "WHERE",
            "SELECT",
            "ANY",
            "ALL",
            "SOME",
            "FILTER",
            "WITHIN",
            "THEN",
            "ELSE",
            "WHEN",
            "BY",
            "HAVING",
            "UNION",
            "EXCEPT",
            "INTERSECT",
            "LATERAL",
            "CASE",
        ];
        let call = t == "("
            && k > 0
            && out[k - 1]
                .chars()
                .all(|c| c.is_ascii_uppercase() || c == '_')
            && !CLAUSE.contains(&out[k - 1].as_str());
        let glue = k == 0
            || call
            || matches!(t.as_str(), "," | ")" | "." | "::")
            || matches!(out[k - 1].as_str(), "(" | "." | "::");
        if !glue {
            text.push(' ');
        }
        text.push_str(t);
    }
    // `IN (?, ?, ?)` says no more than `IN (?)`.
    let collapse = regex::Regex::new(r"\(\?(?:, ?\?)+\)").expect("valid regex");
    let text = collapse.replace_all(&text, "(?)").into_owned();
    (text.len() <= MAX_SHAPE).then_some(text)
}

/// Whether the schema has a table by this name (schema-qualified or not).
pub fn knows_table(name: &str, schema: &DbSchema) -> bool {
    resolve_table(&name.to_lowercase(), &known_tables(schema)).is_some()
}

/// A name a database said is not real, bare, as its error gave it.
#[derive(Debug, Clone, PartialEq)]
pub enum Unknown {
    Column(String),
    Table(String),
}

impl From<&SchemaError> for Unknown {
    fn from(e: &SchemaError) -> Self {
        match e {
            SchemaError::UnknownColumn { column, .. } => Self::Column(column.clone()),
            SchemaError::UnknownTable { name } => Self::Table(name.clone()),
        }
    }
}

/// A name a failed query got wrong, beside the real one its retry used.
#[derive(Debug, Clone, PartialEq)]
pub struct Correction {
    /// The real table: the one the retry named, or the one its real column
    /// belongs to.
    pub table: String,
    /// The real column (`table.column`), for a column correction.
    pub column: Option<String>,
    /// The name as the failed query wrote it, in lower case: under the real
    /// table (`table.column`) for a column, bare for a table.
    pub wrong: String,
}

/// What a successful `retry` corrects in a `failed` query: only when it is
/// the same query token for token (values masked) except that the name the
/// database rejected is replaced, wherever it changed, by one real name, and
/// the rejected name is not in the schema. A retry that also quoted a
/// literal, renamed something else or rewrote a clause teaches nothing; that
/// is what keeps a literal the database read as a name (`WHERE name = Jane`)
/// out.
pub fn correction(
    failed: &str,
    retry: &str,
    unknown: &Unknown,
    schema: &DbSchema,
) -> Option<Correction> {
    let (rejected, column) = match unknown {
        Unknown::Column(c) => (c.as_str(), true),
        Unknown::Table(t) => (t.as_str(), false),
    };
    let (f, r) = (tokenize(failed), tokenize(retry));
    if f.len() != r.len() {
        return None;
    }
    let mut right: Option<String> = None;
    for (a, b) in f.iter().zip(&r) {
        let same = match (a, b) {
            (Tok::Word(x), Tok::Word(y)) | (Tok::Quoted(x), Tok::Quoted(y)) => {
                x.eq_ignore_ascii_case(y)
            }
            (Tok::Value, Tok::Value) => true,
            (Tok::Punct(x), Tok::Punct(y)) => x == y,
            _ => false,
        };
        if same {
            continue;
        }
        let (Tok::Word(w) | Tok::Quoted(w), Tok::Word(x) | Tok::Quoted(x)) = (a, b) else {
            return None;
        };
        if !w.eq_ignore_ascii_case(rejected) {
            return None;
        }
        let x = x.to_lowercase();
        match &right {
            Some(prev) if *prev != x => return None,
            _ => right = Some(x),
        }
    }
    let right = right?;
    let wrong = rejected.to_lowercase();
    let read = analyse(retry, schema);
    if column {
        let mut owners = read.columns.iter().filter(|c| {
            c.rsplit_once('.')
                .is_some_and(|(_, name)| name.eq_ignore_ascii_case(&right))
        });
        let real = owners.next()?.clone();
        if owners.next().is_some() {
            return None;
        }
        let (table, _) = real.rsplit_once('.')?;
        let known = schema.columns.get(table).filter(|c| !c.is_empty())?;
        if known.iter().any(|c| c.eq_ignore_ascii_case(&wrong)) {
            return None;
        }
        Some(Correction {
            wrong: format!("{table}.{wrong}"),
            table: table.to_string(),
            column: Some(real),
        })
    } else {
        let tables = known_tables(schema);
        if resolve_table(&wrong, &tables).is_some() {
            return None;
        }
        let table = resolve_table(&right, &tables)?;
        read.tables.contains(&table).then_some(Correction {
            table,
            column: None,
            wrong,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn schema() -> DbSchema {
        let mut columns = BTreeMap::new();
        columns.insert(
            "public.orders".to_string(),
            vec![
                "id".into(),
                "worker_id".into(),
                "starts_at".into(),
                "status".into(),
                "site_id".into(),
            ],
        );
        columns.insert(
            "public.workers".to_string(),
            vec!["id".into(), "name".into(), "grade".into()],
        );
        DbSchema {
            tables: vec!["public.orders".into(), "public.workers".into()],
            columns,
            engine: Some("postgres".into()),
        }
    }

    #[test]
    fn tables_and_columns_come_from_the_schema_and_values_are_masked() {
        let a = analyse(
            "SELECT w.grade, count(*) AS filled FROM orders s JOIN public.workers w ON w.id = s.worker_id \
             WHERE s.status = 'filled' AND s.starts_at >= '2026-09-01' AND s.site_id IN (12, 13, 14) \
             GROUP BY w.grade ORDER BY filled DESC LIMIT 20",
            &schema(),
        );
        assert_eq!(a.tables, ["public.orders", "public.workers"]);
        assert_eq!(
            a.columns,
            [
                "public.orders.site_id",
                "public.orders.starts_at",
                "public.orders.status",
                "public.orders.worker_id",
                "public.workers.grade",
                "public.workers.id"
            ]
        );
        let shape = a.shape.expect("only SQL and known names");
        assert_eq!(
            shape,
            "SELECT a3.grade, COUNT(*) AS a1 FROM public.orders a2 JOIN public.workers a3 ON a3.id = a2.worker_id \
             WHERE a2.status = ? AND a2.starts_at >= ? AND a2.site_id IN (?) GROUP BY a3.grade ORDER BY a1 DESC LIMIT ?"
        );
        assert!(!shape.contains("filled'") && !shape.contains("2026") && !shape.contains("12"));
    }

    #[test]
    fn an_unknown_word_rejects_the_shape_but_keeps_the_tables() {
        // An unquoted name the schema does not know could be anything a person typed.
        let a = analyse(
            "SELECT * FROM orders WHERE status = filled_by_jane_smith",
            &schema(),
        );
        assert_eq!(a.tables, ["public.orders"]);
        assert_eq!(a.shape, None);
    }

    #[test]
    fn quoted_labels_ctes_and_metabase_variables_are_renamed_or_masked() {
        let a = analyse(
            "WITH recent AS (SELECT worker_id FROM orders WHERE starts_at > {{since}}) \
             SELECT count(*) AS \"Jane's orders\" FROM recent [[WHERE worker_id = {{who}}]]",
            &schema(),
        );
        let shape = a.shape.expect("shape");
        assert!(
            shape.starts_with(
                "WITH cte1 AS (SELECT worker_id FROM public.orders WHERE starts_at > ?)"
            ),
            "{shape}"
        );
        assert!(
            !shape.contains("Jane") && !shape.contains("since") && !shape.contains("who"),
            "{shape}"
        );
    }

    #[test]
    fn no_known_table_means_nothing_to_share() {
        let a = analyse("SELECT 1 FROM secret_table", &schema());
        assert!(a.tables.is_empty());
        assert_eq!(a.shape, None);
    }

    #[test]
    fn comments_are_dropped() {
        let a = analyse(
            "-- for Jane\nSELECT id FROM orders /* urgent: Bob */",
            &schema(),
        );
        assert_eq!(a.shape.as_deref(), Some("SELECT id FROM public.orders"));
    }

    #[test]
    fn values_outside_ordinary_quotes_never_survive() {
        for sql in [
            "SELECT id FROM orders WHERE status = $$jane smith$$",
            "SELECT id FROM orders WHERE status = $t$jane smith$t$",
            "SELECT id FROM orders WHERE status = 'it\\'s jane'",
            "SELECT id FROM orders WHERE status = \u{0660}\u{0667}\u{0667}",
            "SELECT id FROM orders WHERE status = \u{00a3}",
        ] {
            let a = analyse(sql, &schema());
            assert_eq!(a.tables, ["public.orders"], "{sql}");
            if let Some(shape) = &a.shape {
                assert!(
                    !shape.contains("jane")
                        && !shape.contains('\u{0667}')
                        && !shape.contains('\u{00a3}'),
                    "{sql} -> {shape}"
                );
            }
        }
    }

    #[test]
    fn a_retry_renaming_only_the_rejected_name_is_a_correction() {
        let s = schema();
        assert_eq!(
            correction(
                "SELECT s.state FROM orders s WHERE s.id = 1",
                "SELECT s.status FROM orders s WHERE s.id = 2",
                &Unknown::Column("state".into()),
                &s
            ),
            Some(Correction {
                table: "public.orders".into(),
                column: Some("public.orders.status".into()),
                wrong: "public.orders.state".into(),
            })
        );
        assert_eq!(
            correction(
                "SELECT id FROM public.order",
                "SELECT id FROM public.orders",
                &Unknown::Table("order".into()),
                &s
            ),
            Some(Correction {
                table: "public.orders".into(),
                column: None,
                wrong: "order".into(),
            })
        );
    }

    #[test]
    fn a_retry_that_changed_anything_else_is_no_correction() {
        let s = schema();
        let state = Unknown::Column("state".into());
        for (failed, retry) in [
            ("SELECT state FROM orders", "SELECT status, id FROM orders"),
            (
                "SELECT state, idd FROM orders",
                "SELECT status, id FROM orders",
            ),
            (
                "SELECT state FROM orders WHERE id = x",
                "SELECT status FROM orders WHERE id = 'x'",
            ),
            ("SELECT state FROM orders", "SELECT nonsense FROM orders"),
            ("SELECT state FROM orders", "SELECT state FROM orders"),
        ] {
            assert_eq!(correction(failed, retry, &state, &s), None, "{retry}");
        }
        assert_eq!(
            correction(
                "SELECT id FROM workers",
                "SELECT id FROM orders",
                &Unknown::Table("workers".into()),
                &s
            ),
            None,
            "a name the schema has is not wrong"
        );
    }
}
