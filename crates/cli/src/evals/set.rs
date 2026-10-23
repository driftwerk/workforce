//! A golden set: a directory holding `set.toml` and one TOML file per
//! question. A question's version is the hash of what it asks and how it is
//! scored, so a changed question is a new question.

use std::path::Path;

use eyre::{bail, ensure, eyre, Result, WrapErr};
use serde::{Deserialize, Serialize};
use workforce_hub::eval_grader::{defect, GradeRequest, Rubric};
use workforce_orchestrator::workflow_store::content_hash;

const SET_FILE: &str = "set.toml";

/// The longest id a question, arm or set may have.
const MAX_NAME_CHARS: usize = 40;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UseCase {
    Database,
    Code,
    Investigation,
}

impl UseCase {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Database => "database",
            Self::Code => "code",
            Self::Investigation => "investigation",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceKind {
    Table,
    File,
    Ticket,
    Log,
}

/// A source a good answer rests on.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Evidence {
    pub kind: EvidenceKind,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    pub id: String,
    pub use_case: UseCase,
    pub question: String,
    pub expected: String,
    pub evidence: Vec<Evidence>,
    #[serde(default)]
    pub assumptions: Vec<String>,
    /// The mistake an answer commonly makes.
    pub trap: String,
    /// The SQL or code a good answer would have run or read.
    pub ideal_evidence: String,
    pub rubric: Rubric,
}

/// `text` as every platform and editor would have written it: Unix line
/// ends, no space at the end of a line, no blank lines around it.
fn canonical(text: &str) -> String {
    text.replace("\r\n", "\n")
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

fn canonicalised(value: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match value {
        Value::String(text) => Value::String(canonical(&text)),
        Value::Array(items) => items.into_iter().map(canonicalised).collect(),
        Value::Object(fields) => fields
            .into_iter()
            .map(|(key, value)| (key, canonicalised(value)))
            .collect(),
        other => other,
    }
}

impl Question {
    /// The question with every text in it [`canonical`], so what is asked,
    /// graded and hashed is the same wherever the file was checked out.
    fn canonical(self) -> Self {
        let fields = serde_json::to_value(self).expect("a question serialises");
        serde_json::from_value(canonicalised(fields)).expect("a question reads back")
    }

    /// The question's version: any change to what is asked or how it is
    /// scored changes it; how the file is laid out does not.
    pub fn hash(&self) -> String {
        content_hash(&serde_json::to_string(self).expect("a question serialises"))
    }

    /// What a judge is sent to grade `answer`, on `agent`'s routes.
    pub fn grading(&self, answer: &str, agent: &str, model: Option<&str>) -> GradeRequest {
        GradeRequest {
            agent_id: Some(agent.to_string()),
            model: model.map(str::to_string),
            question: self.question.clone(),
            expected: self.expected.clone(),
            evidence: self
                .evidence
                .iter()
                .map(|e| {
                    let kind = serde_json::to_value(e.kind).expect("a kind serialises");
                    format!("{}: {}", kind.as_str().unwrap_or_default(), e.name)
                })
                .collect(),
            assumptions: self.assumptions.clone(),
            trap: self.trap.clone(),
            ideal_evidence: self.ideal_evidence.clone(),
            rubric: self.rubric.clone(),
            answer: answer.to_string(),
        }
    }

    fn defect(&self) -> Option<&'static str> {
        let asked = self.question.trim_start();
        if asked.starts_with('/') || asked.starts_with("[VOICE_") {
            Some("the question would be read as a command, not asked")
        } else if self.evidence.is_empty() {
            Some("at least one piece of evidence is required")
        } else {
            defect(&self.grading("-", "-", None))
        }
    }
}

#[derive(Debug, Deserialize)]
#[serde(deny_unknown_fields)]
struct SetMeta {
    name: String,
    #[serde(default)]
    description: String,
}

#[derive(Debug, Clone)]
pub struct EvalSet {
    pub name: String,
    pub description: String,
    pub questions: Vec<Question>,
}

/// Whether `name` can stand in a file name, a channel and a turn id.
pub fn is_name(name: &str) -> bool {
    !name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
}

fn read_toml<T: serde::de::DeserializeOwned>(path: &Path) -> Result<T> {
    let text =
        std::fs::read_to_string(path).wrap_err_with(|| format!("read {}", path.display()))?;
    toml::from_str(&text).wrap_err_with(|| format!("parse {}", path.display()))
}

impl EvalSet {
    /// The set in `dir`: `set.toml` and every other `*.toml` as one
    /// question named after its file, in file-name order.
    pub fn load(dir: &Path) -> Result<Self> {
        let meta: SetMeta = read_toml(&dir.join(SET_FILE))?;
        ensure!(
            is_name(&meta.name),
            "set name {:?} must be letters, digits, '-' or '_'",
            meta.name
        );
        let mut files: Vec<_> = std::fs::read_dir(dir)
            .wrap_err_with(|| format!("read {}", dir.display()))?
            .collect::<std::io::Result<Vec<_>>>()?
            .into_iter()
            .map(|entry| entry.path())
            .filter(|path| {
                path.extension().is_some_and(|ext| ext == "toml")
                    && path.file_name().is_some_and(|name| name != SET_FILE)
            })
            .collect();
        files.sort();
        let questions = files
            .iter()
            .map(|path| {
                let question = read_toml::<Question>(path)?.canonical();
                let stem = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .unwrap_or_default();
                if !is_name(&question.id) || question.id != stem {
                    bail!(
                        "{}: id {:?} must be the file's name, in letters, digits, '-' or '_'",
                        path.display(),
                        question.id
                    );
                }
                match question.defect() {
                    Some(defect) => Err(eyre!("{}: {defect}", path.display())),
                    None => Ok(question),
                }
            })
            .collect::<Result<Vec<_>>>()?;
        ensure!(
            !questions.is_empty(),
            "{} holds no questions",
            dir.display()
        );
        Ok(Self {
            name: meta.name,
            description: meta.description,
            questions,
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// The synthetic set shipped for tests and docs.
    pub(crate) fn example_dir() -> std::path::PathBuf {
        Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/evals/example")
    }

    fn copy_of_example() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        for entry in std::fs::read_dir(example_dir()).unwrap() {
            let path = entry.unwrap().path();
            std::fs::copy(&path, dir.path().join(path.file_name().unwrap())).unwrap();
        }
        dir
    }

    fn rewrite(dir: &Path, file: &str, edit: impl Fn(&str) -> String) {
        let path = dir.join(file);
        std::fs::write(&path, edit(&std::fs::read_to_string(&path).unwrap())).unwrap();
    }

    #[test]
    fn the_example_set_loads_with_every_use_case() {
        let set = EvalSet::load(&example_dir()).unwrap();
        assert_eq!(set.name, "example");
        let ids: Vec<&str> = set.questions.iter().map(|q| q.id.as_str()).collect();
        assert_eq!(ids, ["code-001", "db-001", "inv-001"]);
        for use_case in [UseCase::Database, UseCase::Code, UseCase::Investigation] {
            assert!(set.questions.iter().any(|q| q.use_case == use_case));
        }
    }

    #[test]
    fn a_questions_hash_follows_its_content_not_its_layout() {
        let dir = copy_of_example();
        let before = EvalSet::load(dir.path()).unwrap().questions[1].hash();
        rewrite(dir.path(), "db-001.toml", |text| {
            format!("# reviewed\n\n{text}")
        });
        let commented = EvalSet::load(dir.path()).unwrap().questions[1].hash();
        assert_eq!(before, commented);
        rewrite(dir.path(), "db-001.toml", |text| {
            text.replace("expected = \"42", "expected = \"43")
        });
        let changed = EvalSet::load(dir.path()).unwrap().questions[1].hash();
        assert_ne!(before, changed);
    }

    #[test]
    fn a_questions_hash_is_the_same_whatever_wrote_its_line_ends() {
        let dir = copy_of_example();
        let before = EvalSet::load(dir.path()).unwrap().questions[1].clone();
        assert!(before.ideal_evidence.contains('\n'));
        rewrite(dir.path(), "db-001.toml", |text| {
            text.replace('\n', "  \r\n")
        });
        let after = EvalSet::load(dir.path()).unwrap().questions[1].clone();
        assert_eq!(before, after);
        assert_eq!(before.hash(), after.hash());
    }

    #[test]
    fn a_change_to_the_rubric_is_a_new_question() {
        let set = EvalSet::load(&example_dir()).unwrap();
        let mut edited = set.questions[0].clone();
        edited.rubric.points[0].text.push('!');
        assert_ne!(set.questions[0].hash(), edited.hash());
    }

    #[test]
    fn a_set_with_a_defective_question_does_not_load() {
        for (file, from, to) in [
            ("db-001.toml", "id = \"db-001\"", "id = \"db-002\""),
            ("db-001.toml", "question = \"", "question = \"/status "),
            ("db-001.toml", "trap = ", "trapp = "),
            (
                "db-001.toml",
                "use_case = \"database\"",
                "use_case = \"sql\"",
            ),
            ("db-001.toml", "id = \"count\"", "id = \"filter\""),
            ("set.toml", "name = \"example\"", "name = \"an example\""),
        ] {
            let dir = copy_of_example();
            rewrite(dir.path(), file, |text| {
                assert!(text.contains(from), "{file} has no {from:?}");
                text.replacen(from, to, 1)
            });
            assert!(EvalSet::load(dir.path()).is_err(), "{from} → {to} loaded");
        }
    }

    #[test]
    fn a_directory_without_questions_is_not_a_set() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(SET_FILE), "name = \"empty\"\n").unwrap();
        assert!(EvalSet::load(dir.path()).is_err());
    }

    #[test]
    fn grading_material_names_no_question_id() {
        let set = EvalSet::load(&example_dir()).unwrap();
        let request = set.questions[1].grading("42", "judge", None);
        let sent = serde_json::to_string(&request).unwrap();
        assert!(!sent.contains("db-001"));
        assert_eq!(request.evidence, ["table: orders"]);
    }
}
