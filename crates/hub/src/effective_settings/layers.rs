//! One config table as each layer sets it and as the runtime merged it.
//!
//! Where a value comes from is read off by comparing what the merge
//! produced with what each layer put in, so no merge is written twice: the
//! effective value is always the runtime's own, and a layer is credited
//! only when its value is the one in effect.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::LazyLock;

use regex::Regex;
use serde::Serialize;
use serde_json::Value;
use workforce_common::redact::{contains_secret, is_credential_field};

use super::{Layer, Row, Source, Status};

type Leaves = BTreeMap<String, Value>;

/// What a credential field shows in place of its value.
pub(super) const SET: &str = "set";
const HIDDEN: &str = "hidden: looks like a secret";

static USERINFO: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"://[^/\s@]+@").expect("invalid userinfo regex"));

/// A URL's query and fragment, where a signature or a token rides.
static URL_QUERY: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r#"(?P<url>://[^\s?#"']*)[?#][^\s"']*"#).expect("invalid url-query regex")
});

/// Where the install runs and keeps its data, by the last word of a key
/// (`db_path`, `embedding_api_url`, `data_dir`): an admin's to see.
static LOCATION: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"(?i)(?:^|[_.])(?:url|uri|path|dir|host|endpoint|arn)$")
        .expect("invalid location regex")
});

/// `text` as anyone may read it: nothing when it carries a secret, and a
/// URL without the credentials in its authority, its query or its fragment.
pub(super) fn public_text(text: &str) -> String {
    let text = USERINFO.replace_all(text, "://");
    let text = URL_QUERY.replace_all(&text, "$url");
    match contains_secret(&text) {
        true => HIDDEN.to_string(),
        false => text.into_owned(),
    }
}

/// `value` with every credential field reduced to whether it is set, and
/// every string made [`public_text`]. An unset credential is dropped like
/// any other unset field.
pub(super) fn scrub(value: Value) -> Value {
    match value {
        Value::Object(fields) => Value::Object(
            fields
                .into_iter()
                .map(|(name, value)| match is_credential_field(&name) {
                    true => {
                        let set =
                            !value.is_null() && value.as_str().is_none_or(|v| !v.trim().is_empty());
                        let shown = match set {
                            true => Value::from(SET),
                            false => Value::Null,
                        };
                        (name, shown)
                    }
                    false => (name, scrub(value)),
                })
                .collect(),
        ),
        Value::Array(items) => Value::Array(items.into_iter().map(scrub).collect()),
        Value::String(text) => Value::String(public_text(&text)),
        other => other,
    }
}

fn flatten(prefix: &str, value: Value, leaves: &mut Leaves) {
    match value {
        Value::Null => {}
        Value::Object(fields) if !fields.is_empty() => {
            for (name, value) in fields {
                let key = match prefix.is_empty() {
                    true => name,
                    false => format!("{prefix}.{name}"),
                };
                flatten(&key, value, leaves);
            }
        }
        leaf => {
            leaves.insert(prefix.to_string(), leaf);
        }
    }
}

/// The scrubbed leaves of `value`, keyed by dotted path. A value that does
/// not serialize has none, so it can only go unreported, never unscrubbed.
fn leaves(value: &impl Serialize) -> Leaves {
    let mut leaves = Leaves::new();
    if let Ok(value) = serde_json::to_value(value) {
        flatten("", scrub(value), &mut leaves);
    }
    leaves
}

pub(super) struct Layered {
    section: &'static str,
    /// The sub-table inside the section (`programs` in `[tools.programs]`).
    table: &'static str,
    /// Config layers, lowest first. The install's holds only what differs
    /// from the built-in default.
    layers: Vec<(Source, Leaves)>,
    effective: Leaves,
    runtime: Leaves,
    /// Keys the config now gives another value (or none) than the one in
    /// effect, for a table the runtime builds once.
    stale: BTreeMap<String, Option<Value>>,
    install_wide: bool,
    /// Keys only an admin sees in a table anyone may read.
    admin_only: Vec<&'static str>,
}

impl Layered {
    pub(super) fn new(
        section: &'static str,
        table: &'static str,
        default: &impl Serialize,
        install: &impl Serialize,
    ) -> Self {
        let default = leaves(default);
        let mut install = leaves(install);
        install.retain(|key, value| default.get(key) != Some(value));
        Self {
            section,
            table,
            layers: vec![(Source::Default, default), (Source::Install, install)],
            effective: Leaves::new(),
            runtime: Leaves::new(),
            stale: BTreeMap::new(),
            install_wide: false,
            admin_only: Vec::new(),
        }
    }

    /// What `source` (the agent's `config.toml`, its `[[agents]]` entry)
    /// sets, above the layers already given.
    pub(super) fn layer(mut self, source: Source, value: Option<&impl Serialize>) -> Self {
        self.layers
            .push((source, value.map(leaves).unwrap_or_default()));
        self
    }

    pub(super) fn section(&self) -> &'static str {
        self.section
    }

    /// Drops what the layers given so far set under `path`, for a sub-table
    /// reported as a table of its own.
    pub(super) fn without(mut self, path: &str) -> Self {
        for (_, leaves) in &mut self.layers {
            leaves.retain(|key, _| !within(key, path));
        }
        self
    }

    /// What the runtime's own merge of the layers produced.
    pub(super) fn effective(mut self, value: &impl Serialize) -> Self {
        self.effective = leaves(value);
        self
    }

    /// What the layers merge to now, for a table the runtime built at
    /// startup: a key it gives another value than the one in effect waits
    /// for a restart.
    pub(super) fn rebuilt_on_restart(mut self, now: &impl Serialize) -> Self {
        let now = leaves(now);
        self.stale = self
            .effective
            .keys()
            .chain(now.keys())
            .filter(|key| self.effective.get(*key) != now.get(*key))
            .map(|key| (key.clone(), now.get(key).cloned()))
            .collect();
        self
    }

    /// `keys` the runtime still holds as `started`, the table the agent
    /// started with, has them: what is merged now waits for a restart.
    pub(super) fn held_from_start<'a>(
        mut self,
        started: &impl Serialize,
        keys: impl IntoIterator<Item = &'a str>,
    ) -> Self {
        let started = leaves(started);
        for key in keys {
            let held: BTreeSet<String> = self
                .effective
                .keys()
                .chain(started.keys())
                .filter(|leaf| within(leaf, key))
                .cloned()
                .collect();
            for leaf in held {
                let now = self.effective.remove(&leaf);
                let then = started.get(&leaf).cloned();
                if now != then {
                    self.stale.insert(leaf.clone(), now);
                }
                if let Some(then) = then {
                    self.effective.insert(leaf, then);
                }
            }
        }
        self
    }

    /// One merged value the runtime holds outside the table's own type, or
    /// keeps current without a restart.
    pub(super) fn effective_field(mut self, key: &str, value: &impl Serialize) -> Self {
        self.stale.retain(|k, _| !within(k, key));
        self.effective.retain(|k, _| !within(k, key));
        let mut field = Leaves::new();
        if let Ok(value) = serde_json::to_value(value) {
            flatten(key, scrub(value), &mut field);
        }
        self.effective.extend(field);
        self
    }

    /// Live values that stand in for the merged ones while they differ.
    pub(super) fn runtime(mut self, value: &impl Serialize) -> Self {
        self.runtime.extend(leaves(value));
        self
    }

    /// Only an admin sees this table.
    pub(super) fn install_wide(mut self) -> Self {
        self.install_wide = true;
        self
    }

    /// Only an admin sees `keys` of this table: other agents' names, the
    /// commands the hub runs. Locations are an admin's in every table.
    pub(super) fn admin_only(mut self, keys: &[&'static str]) -> Self {
        self.admin_only.extend(keys);
        self
    }

    /// A row for every key in effect and every key the agent's own layers
    /// set, whether or not it took effect.
    pub(super) fn rows(&self) -> Vec<Row> {
        let own = self
            .layers
            .iter()
            .filter(|(source, _)| matches!(source, Source::Agent | Source::InstallAgent))
            .flat_map(|(_, leaves)| leaves.keys());
        self.effective
            .keys()
            .chain(self.runtime.keys())
            .chain(self.stale.keys())
            .chain(own)
            .collect::<BTreeSet<_>>()
            .into_iter()
            .map(|key| self.row(key))
            .collect()
    }

    fn row(&self, key: &str) -> Row {
        let mut chain: Vec<Layer> = self
            .layers
            .iter()
            .filter_map(|(source, leaves)| {
                leaves.get(key).map(|value| Layer {
                    source: *source,
                    value: value.clone(),
                })
            })
            .collect();
        let merged = self.effective.get(key);
        let live = self.runtime.get(key).filter(|live| Some(*live) != merged);
        let top = chain.last().cloned();
        let winner = merged.and_then(|merged| chain.iter().rev().find(|l| l.value == *merged));
        let waiting = self.stale.get(key).filter(|_| live.is_none());
        let (source, status, reason) = match (waiting, live, merged, winner, &top) {
            (Some(now), ..) => (
                match merged {
                    Some(_) => winner.map(|winner| winner.source),
                    None => top.as_ref().map(|top| top.source),
                },
                Some(Status::Inert),
                Some(match (now, merged) {
                    (Some(now), Some(_)) => format!(
                        "the config now gives {now}; the running agent keeps this value until \
                         it restarts"
                    ),
                    (Some(_), None) => {
                        "set since the agent started; it applies after a restart".to_string()
                    }
                    (None, _) => "no longer in the config; the running agent keeps this value \
                                  until it restarts"
                        .to_string(),
                }),
            ),
            (None, Some(_), ..) => (Some(Source::Runtime), None, None),
            (None, None, Some(_), Some(winner), Some(top)) if winner.source != top.source => (
                Some(winner.source),
                Some(Status::Inert),
                Some(format!(
                    "{} sets {}, which is not in effect: the {} value holds",
                    top.source.label(),
                    top.value,
                    winner.source.label()
                )),
            ),
            (None, None, Some(_), Some(winner), _) => (Some(winner.source), None, None),
            // A number is never resolved to another: the merge bounded it.
            (None, None, Some(merged), None, Some(top))
                if merged.is_number()
                    && top.value.is_number()
                    && matches!(top.source, Source::Agent | Source::InstallAgent) =>
            {
                (
                    Some(top.source),
                    Some(Status::Inert),
                    Some(format!(
                        "{} sets {}, which is out of bounds: {merged} is in effect",
                        top.source.label(),
                        top.value
                    )),
                )
            }
            (None, None, Some(_), None, Some(top)) => (
                Some(top.source),
                None,
                Some(format!("resolved from {}", top.value)),
            ),
            (None, None, Some(_), None, None) => (None, None, None),
            (None, None, None, ..) => (top.as_ref().map(|top| top.source), None, None),
        };
        let value = live
            .or(merged)
            .or(waiting.and_then(Option::as_ref))
            .or(top.as_ref().map(|top| &top.value))
            .cloned()
            .unwrap_or(Value::Null);
        if let Some(live) = live {
            chain.push(Layer {
                source: Source::Runtime,
                value: live.clone(),
            });
        }
        Row {
            section: self.section,
            key: match self.table.is_empty() {
                true => key.to_string(),
                false => format!("{}.{key}", self.table),
            },
            value,
            source,
            status,
            reason,
            chain,
            install_wide: self.install_wide
                || LOCATION.is_match(key)
                || self.admin_only.iter().any(|path| within(key, path)),
        }
    }
}

/// Whether `key` is `path` or lies under it.
pub(super) fn within(key: &str, path: &str) -> bool {
    key.strip_prefix(path)
        .is_some_and(|rest| rest.is_empty() || rest.starts_with('.'))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row<'a>(rows: &'a [Row], key: &str) -> &'a Row {
        rows.iter()
            .find(|row| row.key == key)
            .unwrap_or_else(|| panic!("no row {key}"))
    }

    fn sources(row: &Row) -> Vec<Source> {
        row.chain.iter().map(|layer| layer.source).collect()
    }

    #[test]
    fn each_layer_is_credited_only_for_the_value_in_effect() {
        let rows = Layered::new(
            "know_how",
            "",
            &json!({"max_entries": 5, "max_tokens": 800, "min_score": 0.75, "extract": true}),
            &json!({"max_entries": 5, "max_tokens": 600, "min_score": 0.5, "extract": true}),
        )
        .layer(Source::InstallAgent, Some(&json!({"extract": false})))
        .layer(
            Source::Agent,
            Some(&json!({"min_score": 0.9, "enabled": true})),
        )
        .effective(
            &json!({"max_entries": 5, "max_tokens": 600, "min_score": 0.9, "extract": false}),
        )
        .runtime(&json!({"max_entries": 9, "max_tokens": 600}))
        .rows();

        assert_eq!(row(&rows, "max_tokens").source, Some(Source::Install));
        assert_eq!(row(&rows, "extract").source, Some(Source::InstallAgent));
        assert_eq!(row(&rows, "min_score").source, Some(Source::Agent));
        assert_eq!(row(&rows, "min_score").value, json!(0.9));
        assert_eq!(
            sources(row(&rows, "min_score")),
            vec![Source::Default, Source::Install, Source::Agent]
        );

        let overridden = row(&rows, "max_entries");
        assert_eq!(
            (overridden.source, &overridden.value),
            (Some(Source::Runtime), &json!(9))
        );
        assert_eq!(
            sources(overridden),
            vec![Source::Default, Source::Runtime],
            "an install value equal to the default is not a layer of its own"
        );
        assert_eq!(
            row(&rows, "max_tokens").chain.last().unwrap().source,
            Source::Install,
            "a live value equal to the merged one overrides nothing"
        );

        let own = row(&rows, "enabled");
        assert_eq!(
            (own.source, &own.value),
            (Some(Source::Agent), &json!(true))
        );
    }

    #[test]
    fn a_default_is_reported_as_one() {
        let rows = Layered::new("research", "", &json!({"a": 1}), &json!({"a": 1}))
            .effective(&json!({"a": 1}))
            .rows();
        assert_eq!(row(&rows, "a").source, Some(Source::Default));
        assert_eq!(sources(row(&rows, "a")), vec![Source::Default]);
    }

    #[test]
    fn an_override_that_did_not_take_effect_is_inert_and_says_what_holds() {
        let rows = Layered::new(
            "know_how",
            "",
            &json!({"min_users": 2}),
            &json!({"min_users": 3}),
        )
        .layer(Source::Agent, Some(&json!({"min_users": 1})))
        .effective(&json!({"min_users": 3}))
        .rows();
        let clamped = row(&rows, "min_users");
        assert_eq!(clamped.value, json!(3));
        assert_eq!(
            (clamped.source, clamped.status),
            (Some(Source::Install), Some(Status::Inert))
        );
        assert_eq!(
            clamped.reason.as_deref(),
            Some(
                "the agent's config.toml sets 1, which is not in effect: the install config \
                 value holds"
            )
        );
    }

    #[test]
    fn a_value_no_layer_holds_is_credited_to_the_layer_it_resolved_from() {
        let rows = Layered::new("llm", "", &json!({"default_model": "x"}), &json!({}))
            .layer(Source::Agent, Some(&json!({"default_model": "powerful"})))
            .effective(&json!({"default_model": "claude-opus"}))
            .rows();
        let model = row(&rows, "default_model");
        assert_eq!(
            (model.source, model.status),
            (Some(Source::Agent), None),
            "resolved, not overridden"
        );
        assert_eq!(model.reason.as_deref(), Some("resolved from \"powerful\""));
    }

    #[test]
    fn an_override_bounded_to_a_value_no_layer_holds_is_inert_and_says_the_bound() {
        let rows = Layered::new("handoff", "", &json!({"max_corrections": 1}), &json!({}))
            .layer(Source::Agent, Some(&json!({"max_corrections": 9})))
            .effective(&json!({"max_corrections": 3}))
            .rows();
        let bounded = row(&rows, "max_corrections");
        assert_eq!(
            (bounded.source, bounded.status, &bounded.value),
            (Some(Source::Agent), Some(Status::Inert), &json!(3))
        );
        assert_eq!(
            bounded.reason.as_deref(),
            Some("the agent's config.toml sets 9, which is out of bounds: 3 is in effect")
        );
    }

    #[test]
    fn a_table_built_at_startup_reports_a_later_edit_as_waiting_for_a_restart() {
        let rows = Layered::new(
            "llm",
            "",
            &json!({"max_tokens": 4096, "pacing": 1}),
            &json!({"max_tokens": 4096, "pacing": 1}),
        )
        .layer(
            Source::Agent,
            Some(&json!({"max_tokens": 9000, "vision": {"model": "b"}, "pacing": 2})),
        )
        .effective(&json!({"max_tokens": 4096, "pacing": 1, "health": 3}))
        .rebuilt_on_restart(&json!({"max_tokens": 9000, "vision": {"model": "b"}, "pacing": 2}))
        .effective_field("pacing", &2)
        .rows();

        let edited = row(&rows, "max_tokens");
        assert_eq!(
            (edited.source, edited.status, &edited.value),
            (Some(Source::Default), Some(Status::Inert), &json!(4096))
        );
        assert_eq!(
            edited.reason.as_deref(),
            Some("the config now gives 9000; the running agent keeps this value until it restarts")
        );
        let added = row(&rows, "vision.model");
        assert_eq!(
            (added.source, added.status, &added.value),
            (Some(Source::Agent), Some(Status::Inert), &json!("b"))
        );
        assert_eq!(
            added.reason.as_deref(),
            Some("set since the agent started; it applies after a restart")
        );
        let removed = row(&rows, "health");
        assert_eq!(
            (removed.source, removed.status),
            (None, Some(Status::Inert))
        );
        assert!(removed
            .reason
            .as_deref()
            .unwrap()
            .starts_with("no longer in the config"));
        let hot = row(&rows, "pacing");
        assert_eq!(
            (hot.source, hot.status, &hot.value),
            (Some(Source::Agent), None, &json!(2)),
            "a field the runtime keeps current waits for nothing"
        );
    }

    #[test]
    fn a_key_held_from_startup_reports_what_is_merged_now_as_waiting() {
        let rows = Layered::new(
            "memory",
            "",
            &json!({"backend": "sqlite", "passive": true}),
            &json!({"backend": "sqlite", "passive": true}),
        )
        .layer(
            Source::Agent,
            Some(&json!({"backend": "weaviate", "passive": false})),
        )
        .effective(&json!({"backend": "weaviate", "passive": false}))
        .held_from_start(&json!({"backend": "sqlite", "passive": true}), ["backend"])
        .rows();

        let held = row(&rows, "backend");
        assert_eq!(
            (held.source, held.status, &held.value),
            (Some(Source::Default), Some(Status::Inert), &json!("sqlite"))
        );
        assert_eq!(
            held.reason.as_deref(),
            Some(
                "the config now gives \"weaviate\"; the running agent keeps this value until it \
                 restarts"
            )
        );
        let hot = row(&rows, "passive");
        assert_eq!(
            (hot.source, hot.status, &hot.value),
            (Some(Source::Agent), None, &json!(false)),
            "a key the runtime reads each turn waits for nothing"
        );
    }

    #[test]
    fn a_location_and_a_named_key_are_an_admins_in_a_table_anyone_reads() {
        let table = json!({
            "db_path": "/var/lib/hub/memory.db",
            "embedding_api_url": "http://10.20.0.6:8080/v1",
            "vision": {"url": "http://vision.internal", "model": "m"},
            "data_dir": "/srv/piper",
            "agents": ["ada", "grace"],
            "min_name_match": 0.5,
        });
        let rows = Layered::new("x", "", &json!({}), &table)
            .admin_only(&["agents"])
            .effective(&table)
            .rows();
        let admins: Vec<&str> = rows
            .iter()
            .filter(|row| row.install_wide)
            .map(|row| row.key.as_str())
            .collect();
        assert_eq!(
            admins,
            [
                "agents",
                "data_dir",
                "db_path",
                "embedding_api_url",
                "vision.url"
            ]
        );
    }

    #[test]
    fn a_sub_table_prefixes_its_keys_and_an_install_wide_table_marks_its_rows() {
        let rows = Layered::new("tools", "programs", &json!({"a": 1}), &json!({"a": 2}))
            .effective(&json!({"a": 2}))
            .install_wide()
            .rows();
        assert_eq!(rows[0].key, "programs.a");
        assert!(rows[0].install_wide);
    }

    #[test]
    fn a_credential_field_shows_only_whether_it_is_set_at_any_depth() {
        let secret = concat!(
            "sk-ant",
            "-api03-ZXhhbXBsZS1rZXktZm9yLXRlc3RzLW9ubHktMDEyMzQ1Njc4OQ"
        );
        let rows = Layered::new(
            "decision",
            "",
            &json!({"api_key": null, "backends": []}),
            &json!({
                "api_key": secret,
                "auth_token": "  ",
                "backends": [{"name": "jev", "api_key": secret, "url": "https://u:pw@jev.example"}],
                "alias": secret,
                "url": "postgres://hub:hunter2@db.internal:5432/hub",
            }),
        )
        .effective(&json!({
            "api_key": secret,
            "auth_token": "  ",
            "backends": [{"name": "jev", "api_key": secret, "url": "https://u:pw@jev.example"}],
            "alias": secret,
            "url": "postgres://hub:hunter2@db.internal:5432/hub",
            "hook": "https://hooks.example/in?sig=s3cr3t&t=1#frag-tok",
            "dsn": "short-dsn",
            "connection_string": "Server=db;Password=pw1",
            "webhook_url": "https://hooks.example/T0/B0/xyz",
            "signing_key": "sk1",
            "servers": [{
                "name": "tickets",
                "headers": {"X-Auth": "hdr-tok"},
                "env": {"TICKETS_PAT": "env-tok"},
                "args": ["--endpoint", "https://t.example/api?token=arg-tok"],
            }],
        }))
        .rows();
        let text = serde_json::to_string(&rows).unwrap();
        for leaked in [
            "s3cr3t",
            "frag-tok",
            "short-dsn",
            "pw1",
            "xyz",
            "sk1",
            "hdr-tok",
            "env-tok",
            "arg-tok",
        ] {
            assert!(!text.contains(leaked), "{leaked} in {text}");
        }
        assert_eq!(row(&rows, "hook").value, json!("https://hooks.example/in"));
        assert_eq!(
            row(&rows, "servers").value,
            json!([{
                "name": "tickets",
                "headers": SET,
                "env": SET,
                "args": ["--endpoint", "https://t.example/api"],
            }])
        );
        let shown = serde_json::to_string(&rows).unwrap();
        assert!(
            !shown.contains(secret) && !shown.contains("sk-ant"),
            "{shown}"
        );
        assert!(
            !shown.contains("hunter2") && !shown.contains("pw@"),
            "{shown}"
        );
        assert_eq!(row(&rows, "api_key").value, json!(SET));
        assert_eq!(row(&rows, "api_key").source, Some(Source::Install));
        assert!(
            rows.iter().all(|row| row.key != "auth_token"),
            "blank is unset"
        );
        assert_eq!(
            row(&rows, "backends").value,
            json!([{"name": "jev", "api_key": SET, "url": "https://jev.example"}])
        );
        assert_eq!(row(&rows, "alias").value, json!(HIDDEN));
        assert_eq!(
            row(&rows, "url").value,
            json!("postgres://db.internal:5432/hub")
        );
    }

    #[test]
    fn a_field_set_in_any_layer_or_in_effect_always_has_a_row() {
        #[derive(Serialize, Default)]
        struct Agent {
            enabled: Option<bool>,
            brand_new_knob: Option<u32>,
        }
        let rows = Layered::new("x", "", &json!({"old": 1}), &json!({"old": 1}))
            .layer(
                Source::Agent,
                Some(&Agent {
                    enabled: None,
                    brand_new_knob: Some(4),
                }),
            )
            .effective(&json!({"old": 1, "brand_new_effective": [1, 2]}))
            .rows();
        let keys: Vec<&str> = rows.iter().map(|row| row.key.as_str()).collect();
        assert_eq!(keys, ["brand_new_effective", "brand_new_knob", "old"]);
        assert_eq!(row(&rows, "brand_new_effective").source, None);
    }
}
