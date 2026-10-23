//! State packages: the typed, validated data a step or sub-agent hands on.
//!
//! A package's schema is its producer's declared `outputs`. The producer emits
//! an `OUTPUTS:` block, [`read`] turns that into typed JSON or names what is
//! wrong with it, and [`render`] is what the receiver is shown in place of the
//! producer's prose.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// How many times a producer is sent back for a package that does not read,
/// before the hand-over fails.
pub const MAX_REPAIRS: u32 = 2;

/// The most correction rounds a verifier may send a producer back for.
pub use workforce_common::settings::MAX_HANDOFF_CORRECTIONS as MAX_CORRECTIONS;

/// The instance variable an exit step's hand-over is stored under.
pub const HANDOFF_KEY: &str = "_handoff";

/// The JSON type a package field holds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FieldType {
    /// Free text, kept exactly as written.
    #[default]
    String,
    Number,
    Integer,
    Boolean,
    /// A JSON array, or one `- item` per line.
    List,
    /// A JSON object.
    Object,
}

impl FieldType {
    fn is_string(&self) -> bool {
        *self == Self::String
    }

    /// How the type is named to the producer, and in a refusal.
    fn label(self) -> &'static str {
        match self {
            Self::String => "text",
            Self::Number => "a number",
            Self::Integer => "an integer",
            Self::Boolean => "true or false",
            Self::List => "a JSON array",
            Self::Object => "a JSON object",
        }
    }

    fn read(self, text: &str) -> Option<Value> {
        let trimmed = text.trim();
        match self {
            Self::String => Some(Value::String(text.to_string())),
            Self::Integer => trimmed.parse::<i64>().ok().map(Value::from),
            Self::Number => trimmed.parse::<i64>().map(Value::from).ok().or_else(|| {
                trimmed
                    .parse::<f64>()
                    .ok()
                    .and_then(|n| serde_json::Number::from_f64(n).map(Value::Number))
            }),
            Self::Boolean => match trimmed.to_ascii_lowercase().as_str() {
                "true" | "yes" => Some(Value::Bool(true)),
                "false" | "no" => Some(Value::Bool(false)),
                _ => None,
            },
            Self::List => serde_json::from_str::<Value>(unfenced(trimmed))
                .ok()
                .filter(Value::is_array)
                .or_else(|| bulleted(trimmed)),
            Self::Object => serde_json::from_str::<Value>(unfenced(trimmed))
                .ok()
                .filter(Value::is_object),
        }
    }
}

/// `text` without the Markdown code fence a model wraps JSON in.
fn unfenced(text: &str) -> &str {
    text.strip_prefix("```")
        .and_then(|rest| rest.strip_suffix("```"))
        .map_or(text, |inner| {
            inner
                .split_once('\n')
                .map_or(inner, |(_language, body)| body)
                .trim()
        })
}

/// A list written one `- item` per line.
fn bulleted(text: &str) -> Option<Value> {
    text.lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .map(|line| {
            line.strip_prefix("- ")
                .or_else(|| line.strip_prefix("* "))
                .map(|item| Value::String(item.trim().to_string()))
        })
        .collect::<Option<Vec<_>>>()
        .filter(|items| !items.is_empty())
        .map(Value::Array)
}

pub(super) fn default_true() -> bool {
    true
}

pub(super) fn is_true(flag: &bool) -> bool {
    *flag
}

/// One field of a package's schema: a declared output of a step or sub-agent.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputField {
    pub name: String,
    #[serde(default)]
    pub description: String,
    /// The JSON type the value is read as. Text unless said otherwise, so a
    /// field declared before types existed still reads as a string.
    #[serde(default, rename = "type", skip_serializing_if = "FieldType::is_string")]
    pub kind: FieldType,
    /// Whether the producer must supply it. `false` lets it be left out.
    #[serde(default = "default_true", skip_serializing_if = "is_true")]
    pub required: bool,
    /// Optional rule on the written text: `"number"` (digits only) or
    /// `"not_empty"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub validate: Option<String>,
}

/// The `validate` rules a field may carry.
const RULES: [&str; 2] = ["number", "not_empty"];

impl OutputField {
    /// Why the declaration cannot be honoured, if it cannot.
    pub fn defect(&self) -> Option<String> {
        let rule = self.validate.as_deref()?;
        (!RULES.contains(&rule)).then(|| {
            format!(
                "output '{}' has unknown validate rule '{rule}' (use {})",
                self.name,
                RULES.join(", ")
            )
        })
    }

    /// The value `text` holds for this field, or why it does not fit.
    fn check(&self, text: &str) -> Result<Value, String> {
        let rule = self.validate.as_deref().filter(|rule| match *rule {
            "number" => text.is_empty() || !text.chars().all(|c| c.is_ascii_digit()),
            "not_empty" => text.trim().is_empty(),
            _ => false,
        });
        let refusal = |expected: &str| {
            let shown = &text[..text.floor_char_boundary(80)];
            format!("'{}' expected {expected} but got: \"{shown}\"", self.name)
        };
        match rule {
            Some(rule) => Err(refusal(rule)),
            None => self
                .kind
                .read(text)
                .ok_or_else(|| refusal(self.kind.label())),
        }
    }

    fn placeholder(&self) -> String {
        let what = if self.description.is_empty() {
            "value"
        } else {
            self.description.as_str()
        };
        let shape = (!self.kind.is_string()).then(|| self.kind.label());
        let optional = (!self.required).then_some("optional");
        std::iter::once(what)
            .chain(shape)
            .chain(optional)
            .collect::<Vec<_>>()
            .join("; ")
    }
}

/// Whether a hand-over is checked before it moves on: `verify = true`, or
/// `verify = { criteria = ["…"], corrections = 2 }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "VerifyRepr")]
pub struct VerifySpec {
    /// `false` turns verification off where the agent's default has it on.
    pub enabled: bool,
    /// What the output must satisfy, beside answering its request and
    /// fitting its schema.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub criteria: Vec<String>,
    /// Correction rounds for this hand-over, in place of the agent's default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub corrections: Option<u32>,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum VerifyRepr {
    Flag(bool),
    Full {
        #[serde(default = "default_true")]
        enabled: bool,
        #[serde(default)]
        criteria: Vec<String>,
        #[serde(default)]
        corrections: Option<u32>,
    },
}

impl From<VerifyRepr> for VerifySpec {
    fn from(repr: VerifyRepr) -> Self {
        match repr {
            VerifyRepr::Flag(enabled) => Self {
                enabled,
                criteria: Vec::new(),
                corrections: None,
            },
            VerifyRepr::Full {
                enabled,
                criteria,
                corrections,
            } => Self {
                enabled,
                criteria,
                corrections,
            },
        }
    }
}

impl VerifySpec {
    /// Why the spec cannot be honoured, if it cannot.
    pub fn defect(&self) -> Option<String> {
        if self.criteria.iter().any(|c| c.trim().is_empty()) {
            return Some("lists a blank verify criterion".to_string());
        }
        self.corrections
            .filter(|rounds| *rounds > MAX_CORRECTIONS)
            .map(|rounds| {
                format!("asks for {rounds} verify corrections; the most is {MAX_CORRECTIONS}")
            })
    }
}

/// What an exit step with `handoff = "<type>"` leaves on its run.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct HandoffPackage {
    #[serde(rename = "type")]
    pub kind: String,
    /// The step that handed over.
    pub step: String,
    /// That step's validated package; `None` when it declares no outputs.
    pub package: Option<Value>,
    pub summary: String,
}

/// A producer's response read against its schema.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Reading {
    /// Every field that read cleanly, typed. `None` when none did.
    pub package: Option<Value>,
    /// Required fields the response did not supply.
    pub missing: Vec<String>,
    /// Supplied fields whose value does not fit, each said in full.
    pub invalid: Vec<String>,
}

impl Reading {
    /// What stops the package being handed on, said for the producer. A
    /// producer that is `excused` owes no missing field — a step leaving by
    /// a declared branch — but a value it did supply must still fit.
    pub fn problems(&self, excused: bool) -> Vec<String> {
        self.missing
            .iter()
            .filter(|_| !excused)
            .map(|name| format!("'{name}' is missing"))
            .chain(self.invalid.iter().cloned())
            .collect()
    }
}

/// Read `response`'s `OUTPUTS:` block against `fields`.
pub fn read(response: &str, fields: &[OutputField]) -> Reading {
    let names: Vec<&str> = fields.iter().map(|f| f.name.as_str()).collect();
    let written = parse_outputs(response, &names);
    let text_of = |field: &OutputField| {
        written
            .as_ref()
            .and_then(|block| block.get(&field.name))
            .and_then(Value::as_str)
    };
    let mut reading = Reading::default();
    let mut package = serde_json::Map::new();
    for field in fields {
        match text_of(field).map(|text| field.check(text)) {
            Some(Ok(value)) => {
                package.insert(field.name.clone(), value);
            }
            Some(Err(why)) => reading.invalid.push(why),
            None if field.required => reading.missing.push(field.name.clone()),
            None => {}
        }
    }
    reading.package = (!package.is_empty()).then_some(Value::Object(package));
    reading
}

/// The prompt section telling a producer to emit its package. Empty when it
/// declares no fields.
pub fn instructions(fields: &[OutputField]) -> String {
    if fields.is_empty() {
        return String::new();
    }
    let block: String = fields
        .iter()
        .map(|field| format!("{}: <{}>\n", field.name, field.placeholder()))
        .collect();
    format!(
        "\n---\nBefore you finish, you MUST emit a structured output block \
         with these fields:\nOUTPUTS:\n{block}END_OUTPUTS\n\n\
         Each field starts with its name followed by a colon. \
         Multi-line values are allowed — continue on the next lines \
         until the next field name or END_OUTPUTS.\n"
    )
}

/// What a producer is told when its package was sent back.
pub fn correction(problems: &[String]) -> String {
    format!(
        "The output block was not accepted: {}.",
        problems.join("; ")
    )
}

/// The most of a response shown where its producer handed over no package.
/// Its tail is kept: a response ends with its results.
const RESPONSE_TAIL_CHARS: usize = 12_000;

/// What a producer's run is shown as: its package, else the end of what it
/// said.
pub fn shown(package: Option<&Value>, response: &str) -> String {
    package.map_or_else(
        || super::engine::truncate_tail(response, RESPONSE_TAIL_CHARS),
        render,
    )
}

/// The prompt section a producer that was sent back runs again with: what
/// was wrong (`detail`) and what its attempt produced.
pub fn correction_section(detail: &str, package: Option<&Value>, response: &str) -> String {
    format!(
        "\n\n## Correction required\n\n\
         Your previous attempt was sent back. {detail}\n\n\
         What it produced:\n{}\n\n\
         Put right what is named above and emit the complete result again.\n",
        shown(package, response)
    )
}

/// A package as its receiver reads it: `key: value` per field, a multi-line
/// value under a bold header.
pub fn render(package: &Value) -> String {
    let Some(fields) = package.as_object() else {
        return package.to_string();
    };
    fields
        .iter()
        .map(|(name, value)| {
            let owned = value.to_string();
            let text = value.as_str().unwrap_or(&owned);
            if text.contains('\n') {
                format!("**{name}**:\n{text}")
            } else {
                format!("{name}: {text}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n\n")
}

/// The label of an `OUTCOME:` line, tolerating markdown decoration around it.
pub(super) fn outcome_label(line: &str) -> Option<String> {
    line.trim()
        .trim_start_matches(['*', '_', '>', '#', '-', '`', ' '])
        .strip_prefix("OUTCOME:")
        .map(|label| label.trim_end_matches(['*', '_', '`']).trim().to_string())
}

/// Parse structured outputs from the agent's response.
///
/// Looks for an `OUTPUTS:` block and extracts key-value pairs. The block ends
/// at `END_OUTPUTS`, or, when a response never emits that marker, at the
/// `OUTCOME:` line or the end of the response: a block cut short still carries
/// every field it managed to emit. Returns `None` if no block is found.
///
/// Every value is the text the producer wrote; [`read`] is what gives a field
/// its declared type.
///
/// When `declared_fields` is non-empty, field-aware multi-line parsing is used:
/// a line starting with `<declared_field>:` begins a new field, and subsequent
/// lines are appended as continuations until the next field or the block's end.
/// When `declared_fields` is empty, each line is parsed as an independent
/// `key: value` pair (backward-compatible single-line mode).
pub fn parse_outputs(response: &str, declared_fields: &[&str]) -> Option<Value> {
    let start_marker = "OUTPUTS:";
    let end_marker = "END_OUTPUTS";

    let start_idx = response.find(start_marker)?;
    let rest = &response[start_idx + start_marker.len()..];
    let block = rest.find(end_marker).map_or(rest, |end| &rest[..end]);
    let lines = block
        .lines()
        .take_while(|line| outcome_label(line).is_none());

    let mut map = serde_json::Map::new();

    if declared_fields.is_empty() {
        // Single-line mode (backward compatible).
        for line in lines {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some((key, value)) = trimmed.split_once(':') {
                let key = key.trim();
                let value = value.trim();
                if !key.is_empty() && !value.is_empty() {
                    map.insert(key.to_string(), Value::String(value.to_string()));
                }
            }
        }
    } else {
        // Field-aware multi-line mode.
        let mut current_key: Option<String> = None;
        let mut current_value = String::new();

        // Match field names case-insensitively so the parser is robust against
        // models that emit `Ticket_Summary:` instead of `ticket_summary:`.
        let lowered_fields: Vec<String> =
            declared_fields.iter().map(|f| f.to_lowercase()).collect();

        for line in lines {
            let trimmed = line.trim();
            let trimmed_lower = trimmed.to_lowercase();

            let new_field = lowered_fields.iter().enumerate().find(|(_, field)| {
                trimmed_lower.starts_with(field.as_str())
                    && trimmed_lower[field.len()..].trim_start().starts_with(':')
            });

            if let Some((idx, _)) = new_field {
                if let Some(key) = current_key.take() {
                    map.insert(key, Value::String(current_value.trim().to_string()));
                }
                let colon_pos = trimmed.find(':').unwrap();
                current_key = Some(declared_fields[idx].to_string());
                current_value = trimmed[colon_pos + 1..].trim_start().to_string();
            } else if current_key.is_some() {
                if !current_value.is_empty() {
                    current_value.push('\n');
                }
                current_value.push_str(trimmed);
            }
        }

        if let Some(key) = current_key.take() {
            map.insert(key, Value::String(current_value.trim().to_string()));
        }
    }

    if map.is_empty() {
        None
    } else {
        Some(Value::Object(map))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn field(name: &str, kind: FieldType) -> OutputField {
        OutputField {
            name: name.to_string(),
            description: String::new(),
            kind,
            required: true,
            validate: None,
        }
    }

    fn ruled(name: &str, rule: &str) -> OutputField {
        OutputField {
            validate: Some(rule.to_string()),
            ..field(name, FieldType::String)
        }
    }

    fn block(body: &str) -> String {
        format!("Done.\n\nOUTPUTS:\n{body}\nEND_OUTPUTS")
    }

    #[test]
    fn a_package_is_read_into_its_declared_types() {
        let fields = [
            field("title", FieldType::String),
            field("count", FieldType::Integer),
            field("ratio", FieldType::Number),
            field("ready", FieldType::Boolean),
            field("tables", FieldType::List),
            field("schema", FieldType::Object),
        ];
        let reading = read(
            &block(
                "title: Orders\ncount: 42\nratio: 0.5\nready: yes\n\
                 tables: [\"orders\", \"lines\"]\n\
                 schema:\n```json\n{\"orders\": {\"id\": \"uuid\"}}\n```",
            ),
            &fields,
        );

        assert_eq!(reading.problems(false), Vec::<String>::new());
        assert_eq!(
            reading.package.unwrap(),
            serde_json::json!({
                "title": "Orders",
                "count": 42,
                "ratio": 0.5,
                "ready": true,
                "tables": ["orders", "lines"],
                "schema": { "orders": { "id": "uuid" } },
            })
        );
    }

    #[test]
    fn a_list_may_be_written_one_item_per_line() {
        let reading = read(
            &block("tables:\n- orders\n- order lines"),
            &[field("tables", FieldType::List)],
        );
        assert_eq!(
            reading.package.unwrap()["tables"],
            serde_json::json!(["orders", "order lines"])
        );
    }

    #[test]
    fn a_malformed_package_names_every_problem() {
        let fields = [
            field("count", FieldType::Integer),
            field("ready", FieldType::Boolean),
            field("schema", FieldType::Object),
            field("owner", FieldType::String),
        ];
        let reading = read(
            &block("count: about forty\nready: maybe\nschema: [1, 2]"),
            &fields,
        );

        assert_eq!(reading.package, None);
        assert_eq!(reading.missing, ["owner"]);
        assert_eq!(
            reading.problems(false),
            [
                "'owner' is missing",
                "'count' expected an integer but got: \"about forty\"",
                "'ready' expected true or false but got: \"maybe\"",
                "'schema' expected a JSON object but got: \"[1, 2]\"",
            ]
        );
    }

    #[test]
    fn a_response_with_no_block_is_missing_every_required_field() {
        let optional = OutputField {
            required: false,
            ..field("notes", FieldType::String)
        };
        let reading = read(
            "I forgot the block.",
            &[field("count", FieldType::Integer), optional],
        );
        assert_eq!(reading.package, None);
        assert_eq!(reading.problems(false), ["'count' is missing"]);
    }

    #[test]
    fn an_optional_field_may_be_left_out_but_must_fit_when_given() {
        let optional = OutputField {
            required: false,
            ..field("count", FieldType::Integer)
        };
        let fields = [field("title", FieldType::String), optional];

        let without = read(&block("title: Orders"), &fields);
        assert!(without.problems(false).is_empty());
        assert_eq!(
            without.package.unwrap(),
            serde_json::json!({ "title": "Orders" })
        );

        let mistyped = read(&block("title: Orders\ncount: many"), &fields);
        assert_eq!(
            mistyped.problems(false),
            ["'count' expected an integer but got: \"many\""]
        );
    }

    #[test]
    fn an_excused_producer_owes_no_missing_field_but_a_given_value_must_fit() {
        let fields = [
            field("count", FieldType::Integer),
            field("title", FieldType::String),
        ];
        let reading = read(&block("count: none"), &fields);
        assert_eq!(
            reading.problems(true),
            ["'count' expected an integer but got: \"none\""]
        );
        assert!(read("no block", &fields).problems(true).is_empty());
    }

    #[test]
    fn an_untyped_field_stays_the_text_it_was_written_as() {
        let reading = read(&block("pr_number: 42"), &[ruled("pr_number", "number")]);
        assert_eq!(reading.package.unwrap()["pr_number"], "42");
    }

    #[test]
    fn the_number_rule_takes_digits_only() {
        let fields = [ruled("pr_number", "number")];
        let refused = read(&block("pr_number: Unable to create the PR"), &fields);
        assert_eq!(
            refused.problems(false),
            ["'pr_number' expected number but got: \"Unable to create the PR\""]
        );
        assert_eq!(read(&block("pr_number:"), &fields).problems(false).len(), 1);
    }

    #[test]
    fn the_not_empty_rule_refuses_blank_text() {
        let fields = [ruled("summary", "not_empty")];
        assert_eq!(read(&block("summary:   "), &fields).invalid.len(), 1);
        assert!(read(&block("summary: Added logging"), &fields)
            .problems(false)
            .is_empty());
    }

    #[test]
    fn an_unknown_rule_passes() {
        let reading = read(
            &block("pr_number: anything"),
            &[ruled("pr_number", "later")],
        );
        assert!(reading.problems(false).is_empty());
    }

    #[test]
    fn a_refusal_quotes_at_most_eighty_bytes_of_the_value() {
        let long = "é".repeat(200);
        let reading = read(
            &block(&format!("count: {long}")),
            &[field("count", FieldType::Integer)],
        );
        let quoted = reading.invalid[0].split('"').nth(1).unwrap();
        assert_eq!(quoted, "é".repeat(40));
    }

    #[test]
    fn instructions_say_a_fields_type_and_whether_it_may_be_left_out() {
        let typed = OutputField {
            description: "How many rows".to_string(),
            required: false,
            ..field("count", FieldType::Integer)
        };
        let text = instructions(&[field("title", FieldType::String), typed]);
        assert!(text.contains(
            "OUTPUTS:\ntitle: <value>\ncount: <How many rows; an integer; optional>\nEND_OUTPUTS"
        ));
        assert_eq!(instructions(&[]), "");
    }

    #[test]
    fn render_puts_a_multiline_value_under_its_own_header() {
        let rendered = render(&serde_json::json!({
            "plan": "1. Do this\n2. Do that",
            "count": 3,
            "tables": ["orders"],
        }));
        assert!(rendered.contains("count: 3"));
        assert!(rendered.contains("tables: [\"orders\"]"));
        assert!(rendered.contains("**plan**:\n1. Do this\n2. Do that"));
    }

    #[test]
    fn verify_reads_as_a_flag_or_a_table() {
        #[derive(Deserialize)]
        struct Step {
            verify: VerifySpec,
        }
        let flag: Step = toml::from_str("verify = false").unwrap();
        assert!(!flag.verify.enabled);

        let full: Step =
            toml::from_str("verify = { criteria = [\"every table has a key\"], corrections = 2 }")
                .unwrap();
        assert!(full.verify.enabled);
        assert_eq!(full.verify.criteria, ["every table has a key"]);
        assert_eq!(full.verify.corrections, Some(2));
        assert_eq!(full.verify.defect(), None);

        let round_trip: VerifySpec =
            serde_json::from_value(serde_json::to_value(&full.verify).unwrap()).unwrap();
        assert_eq!(round_trip, full.verify);
    }

    #[test]
    fn verify_refuses_a_blank_criterion_and_too_many_corrections() {
        let blank = VerifySpec {
            enabled: true,
            criteria: vec!["  ".to_string()],
            corrections: None,
        };
        assert!(blank.defect().unwrap().contains("blank"));
        let greedy = VerifySpec {
            enabled: true,
            criteria: Vec::new(),
            corrections: Some(MAX_CORRECTIONS + 1),
        };
        assert!(greedy.defect().unwrap().contains("the most is 3"));
    }
}
