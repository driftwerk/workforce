//! What a monitor dispatches on — a channel message or a source observation —
//! and the placeholders every action renders from it.

use std::sync::OnceLock;

use chrono::{DateTime, Utc};
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use workforce_common::channels::InboundEvent;

use super::condition::text_of;

/// The `surface` a source dispatch is keyed under in the claim gate.
pub const SOURCE_SURFACE: &str = "source";

/// The key a value-mode observation carries.
pub const VALUE_KEY: &str = "value";

/// One reading of a source that a monitor's condition admitted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Observation {
    /// The tool that read it.
    pub source: String,
    /// The item's id, or [`VALUE_KEY`] for a value.
    pub key: String,
    pub observed_at: DateTime<Utc>,
    pub text: String,
    /// The watched part.
    pub value: Value,
    /// The previous poll's watched part, when it was kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<Value>,
    /// The whole item (or the whole output, for a value).
    pub item: Value,
}

/// Untagged, with the message first: a row written before sources existed
/// parses exactly as it did, and an observation parses by its own fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MonitorEvent {
    Message(InboundEvent),
    Observed(Observation),
}

fn placeholder() -> &'static Regex {
    static RE: OnceLock<Regex> = OnceLock::new();
    RE.get_or_init(|| {
        Regex::new(r"\{\{(text|id|channel|user|ts|value|previous|json:[^}]*)\}\}")
            .expect("placeholder pattern")
    })
}

impl MonitorEvent {
    pub fn message(&self) -> Option<&InboundEvent> {
        match self {
            Self::Message(ev) => Some(ev),
            Self::Observed(_) => None,
        }
    }

    pub fn observation(&self) -> Option<&Observation> {
        match self {
            Self::Message(_) => None,
            Self::Observed(obs) => Some(obs),
        }
    }

    pub fn text(&self) -> &str {
        match self {
            Self::Message(ev) => &ev.text,
            Self::Observed(obs) => &obs.text,
        }
    }

    /// The message's ts, or the observation's key.
    pub fn id(&self) -> &str {
        match self {
            Self::Message(ev) => &ev.ts,
            Self::Observed(obs) => &obs.key,
        }
    }

    /// What one dispatch of this event is keyed by: a message's ts, which is
    /// unique in its channel; an observation's key at its instant, so the
    /// same key may fire again on a later transition.
    pub fn dispatch_id(&self) -> String {
        match self {
            Self::Message(ev) => ev.ts.clone(),
            Self::Observed(obs) => format!("{}@{}", obs.key, obs.observed_at.timestamp_millis()),
        }
    }

    /// The surface a message came from; an observation came from none.
    pub fn surface(&self) -> Option<&str> {
        self.message().map(|ev| ev.surface.as_str())
    }

    pub fn channel(&self) -> Option<&str> {
        self.message().map(|ev| ev.channel.as_str())
    }

    pub fn thread(&self) -> Option<&str> {
        self.message().and_then(|ev| ev.thread_ts.as_deref())
    }

    /// The whole event as JSON: a workflow's trigger payload, and what
    /// `{{json:…}}` points into.
    pub fn payload(&self) -> Value {
        match self {
            Self::Message(ev) => serde_json::to_value(ev).unwrap_or(Value::Null),
            Self::Observed(obs) => obs.item.clone(),
        }
    }

    fn placeholder_value(&self, name: &str, payload: &Value) -> String {
        match (self, name) {
            (_, "text") => self.text().to_string(),
            (_, "id") => self.id().to_string(),
            (Self::Message(ev), "channel") => ev.channel.clone(),
            (Self::Observed(obs), "channel") => obs.source.clone(),
            (Self::Message(ev), "user") => ev.user.clone().unwrap_or_default(),
            (Self::Message(ev), "ts") => ev.ts.clone(),
            (Self::Observed(obs), "ts") => obs.observed_at.to_rfc3339(),
            (Self::Message(ev), "value") => ev.text.clone(),
            (Self::Observed(obs), "value") => text_of(Some(&obs.value)).into_owned(),
            (Self::Observed(obs), "previous") => text_of(obs.previous.as_ref()).into_owned(),
            (_, name) => match name.strip_prefix("json:") {
                Some(pointer) => text_of(payload.pointer(pointer)).into_owned(),
                None => String::new(),
            },
        }
    }

    /// `template` with every placeholder filled: `{{text}}`, `{{id}}`,
    /// `{{channel}}`, `{{user}}`, `{{ts}}`, `{{value}}`, `{{previous}}` and
    /// `{{json:/a/b}}`. Plain substitution, no escaping.
    pub fn render(&self, template: &str) -> String {
        let payload = self.payload();
        placeholder()
            .replace_all(template, |captures: &regex::Captures<'_>| {
                self.placeholder_value(&captures[1], &payload)
            })
            .into_owned()
    }

    /// `input` with every string in it rendered, however deep.
    pub fn render_value(&self, input: &Value) -> Value {
        match input {
            Value::String(s) => Value::String(self.render(s)),
            Value::Array(items) => {
                Value::Array(items.iter().map(|v| self.render_value(v)).collect())
            }
            Value::Object(map) => Value::Object(
                map.iter()
                    .map(|(k, v)| (k.clone(), self.render_value(v)))
                    .collect(),
            ),
            other => other.clone(),
        }
    }
}

impl From<InboundEvent> for MonitorEvent {
    fn from(ev: InboundEvent) -> Self {
        Self::Message(ev)
    }
}

impl From<Observation> for MonitorEvent {
    fn from(obs: Observation) -> Self {
        Self::Observed(obs)
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn message() -> MonitorEvent {
        MonitorEvent::Message(InboundEvent {
            surface: "slack".into(),
            workspace: "T1".into(),
            channel: "C1".into(),
            user: Some("U1".into()),
            bot_id: None,
            text: "hello".into(),
            ts: "1.5".into(),
            thread_ts: Some("1.0".into()),
            has_files: false,
        })
    }

    fn observation() -> MonitorEvent {
        MonitorEvent::Observed(Observation {
            source: "google.gmail.search".into(),
            key: "m-7".into(),
            observed_at: DateTime::parse_from_rfc3339("2026-10-16T10:00:00Z")
                .unwrap()
                .with_timezone(&Utc),
            text: "Invoice 42".into(),
            value: json!({ "subject": "Invoice 42", "amount": 12.5 }),
            previous: Some(json!({ "subject": "Invoice 41", "amount": 10 })),
            item: json!({ "id": "m-7", "subject": "Invoice 42", "amount": 12.5, "from": { "name": "Acme" } }),
        })
    }

    #[test]
    fn a_message_renders_as_it_always_has() {
        let ev = message();
        assert_eq!(
            ev.render("from {{user}}: {{text}} ({{channel}} @ {{ts}}) id={{id}} v={{value}} p=[{{previous}}]"),
            "from U1: hello (C1 @ 1.5) id=1.5 v=hello p=[]"
        );
        assert_eq!(ev.render("{{json:/surface}}/{{json:/nope}}"), "slack/");
        assert_eq!(ev.dispatch_id(), "1.5");
        assert_eq!(ev.surface(), Some("slack"));
        assert_eq!(ev.channel(), Some("C1"));
        assert_eq!(ev.thread(), Some("1.0"));
        assert!(ev.observation().is_none());
    }

    #[test]
    fn an_observation_renders_its_part_its_previous_and_pointers_into_the_item() {
        let ev = observation();
        assert_eq!(
            ev.render("{{text}}|{{id}}|{{channel}}|{{user}}|{{ts}}"),
            "Invoice 42|m-7|google.gmail.search||2026-10-16T10:00:00+00:00"
        );
        assert_eq!(
            ev.render("{{value}}"),
            "{\"amount\":12.5,\"subject\":\"Invoice 42\"}"
        );
        assert_eq!(
            ev.render("{{previous}}"),
            "{\"amount\":10,\"subject\":\"Invoice 41\"}"
        );
        assert_eq!(
            ev.render("{{json:/from/name}} {{json:/amount}}"),
            "Acme 12.5"
        );
        assert_eq!(ev.render("{{unknown}} stays"), "{{unknown}} stays");
        assert_eq!(ev.dispatch_id(), "m-7@1792144800000");
        assert!(ev.surface().is_none() && ev.channel().is_none() && ev.thread().is_none());
        assert!(ev.message().is_none());
    }

    #[test]
    fn render_value_fills_every_string_however_deep() {
        let ev = observation();
        let input = json!({ "id": "{{id}}", "nested": { "list": ["{{text}}", 3, true] }, "n": 1 });
        assert_eq!(
            ev.render_value(&input),
            json!({ "id": "m-7", "nested": { "list": ["Invoice 42", 3, true] }, "n": 1 })
        );
    }

    #[test]
    fn events_round_trip_untagged_and_old_message_rows_still_parse() {
        let stored = serde_json::to_value(message()).unwrap();
        assert!(
            stored.get("surface").is_some(),
            "a message row keeps its shape: {stored}"
        );
        assert!(matches!(
            serde_json::from_value::<MonitorEvent>(stored).unwrap(),
            MonitorEvent::Message(_)
        ));
        let stored = serde_json::to_value(observation()).unwrap();
        assert_eq!(stored["text"], "Invoice 42");
        let back: MonitorEvent = serde_json::from_value(stored).unwrap();
        assert_eq!(back.observation(), observation().observation());
    }
}
