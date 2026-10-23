//! What a run leaves on the host: every turn and every grade as one JSON
//! line, appended as it happens, so a run that stops can pick up where it
//! was. These files hold questions' answers and stay on the host.

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use eyre::{ensure, Result, WrapErr};
use parking_lot::Mutex;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use workforce_hub::eval_grader::GradeOutcome;

use super::arms::Arm;
use super::set::{EvalSet, UseCase};

const MANIFEST_FILE: &str = "run.json";
const TURNS_FILE: &str = "turns.jsonl";
const GRADES_FILE: &str = "grades.jsonl";

/// How many times a judge reads each answer: the grade, and a repeat that
/// measures how far the judge agrees with itself.
pub const GRADE_PASSES: u8 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Answered,
    Failed,
}

/// What the hub recorded of a turn.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct TurnMetrics {
    pub cost_usd: f64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub rounds: u32,
    pub model: String,
    pub tools: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TurnRecord {
    pub arm: String,
    pub arm_hash: String,
    pub question: String,
    pub question_hash: String,
    pub use_case: UseCase,
    pub attempt: u32,
    pub turn_id: String,
    pub status: TurnStatus,
    pub answer: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub wall_ms: u64,
    /// The turn stopped at its round cap: the answer is its wind-down.
    #[serde(default)]
    pub capped: bool,
    /// The content hash of the agent's `config.toml` the turn ran under.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metrics: Option<TurnMetrics>,
    pub at: DateTime<Utc>,
}

/// One reading of one answer by the judge. It names the turn, never the
/// arm: which arm a grade belongs to is only known by joining it to
/// [`TurnRecord`]s here, after the judge has answered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct GradeRecord {
    pub turn_id: String,
    pub pass: u8,
    #[serde(flatten)]
    pub outcome: GradeOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default)]
    pub cost_usd: f64,
    pub at: DateTime<Utc>,
}

/// The arm a run's results were produced under.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ArmEntry {
    pub name: String,
    pub agent: String,
    pub hash: String,
    pub overridden: bool,
}

impl From<&Arm> for ArmEntry {
    fn from(arm: &Arm) -> Self {
        Self {
            name: arm.name.clone(),
            agent: arm.agent.clone(),
            hash: arm.hash(),
            overridden: !arm.overrides.is_empty(),
        }
    }
}

impl ArmEntry {
    fn produced(&self, turn: &TurnRecord) -> bool {
        turn.arm == self.name && turn.arm_hash == self.hash
    }
}

/// Each of `set`'s questions' current version, by id.
fn versions(set: &EvalSet) -> HashMap<&str, String> {
    set.questions
        .iter()
        .map(|q| (q.id.as_str(), q.hash()))
        .collect()
}

fn is_current(versions: &HashMap<&str, String>, turn: &TurnRecord) -> bool {
    versions.get(turn.question.as_str()) == Some(&turn.question_hash)
}

/// Where a run's judge runs: one judge reads every answer of a run.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Judge {
    pub agent: String,
    /// A tier or routed spec; the agent's default model when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Manifest {
    /// Names the run, and seeds the order its answers are judged in.
    pub run_id: String,
    pub set: String,
    pub arms: Vec<ArmEntry>,
    pub judge: Judge,
}

/// A file only its owner can read, since what goes in it holds what people
/// asked and were told.
fn private(path: &Path, options: &mut OpenOptions) -> Result<File> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let file = options
        .open(path)
        .wrap_err_with(|| format!("open {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        file.set_permissions(std::fs::Permissions::from_mode(0o600))
            .wrap_err_with(|| format!("restrict {}", path.display()))?;
    }
    Ok(file)
}

/// Write `content` to `path`, readable by its owner alone.
pub fn write_private(path: &Path, content: &str) -> Result<()> {
    private(
        path,
        OpenOptions::new().write(true).create(true).truncate(true),
    )?
    .write_all(content.as_bytes())
    .wrap_err_with(|| format!("write {}", path.display()))
}

fn read_lines<T: DeserializeOwned>(path: &Path) -> Result<Vec<T>> {
    if !path.exists() {
        return Ok(Vec::new());
    }
    BufReader::new(File::open(path).wrap_err_with(|| format!("open {}", path.display()))?)
        .lines()
        .enumerate()
        .filter(|(_, line)| line.as_ref().map_or(true, |text| !text.trim().is_empty()))
        .map(|(n, line)| {
            serde_json::from_str(&line?)
                .wrap_err_with(|| format!("{} line {}", path.display(), n + 1))
        })
        .collect()
}

/// A run's directory.
pub struct Store {
    dir: PathBuf,
    turns: Mutex<File>,
    grades: Mutex<File>,
    pub manifest: Manifest,
}

impl Store {
    /// Open `dir` for a run of `set` against `arms`, starting it when it is
    /// new. A directory holds one set's results.
    pub fn open(dir: &Path, set: &EvalSet, arms: &[Arm], judge: Judge) -> Result<Self> {
        std::fs::create_dir_all(dir).wrap_err_with(|| format!("create {}", dir.display()))?;
        let previous = Self::manifest_in(dir)?;
        if let Some(previous) = &previous {
            ensure!(
                previous.set == set.name,
                "{} holds results of set {:?}, not {:?}",
                dir.display(),
                previous.set,
                set.name
            );
            ensure!(
                previous.judge == judge,
                "{} holds answers judged on {:?}: a run has one judge. Pass the same \
                 --grader-agent and --grader-model, or use a new run directory",
                dir.display(),
                previous.judge
            );
        }
        let manifest = Manifest {
            run_id: previous.map_or_else(
                || uuid::Uuid::new_v4().simple().to_string(),
                |previous| previous.run_id,
            ),
            set: set.name.clone(),
            arms: arms.iter().map(ArmEntry::from).collect(),
            judge,
        };
        write_private(
            &dir.join(MANIFEST_FILE),
            &serde_json::to_string_pretty(&manifest)?,
        )?;
        let appending = |file: &str| {
            private(
                &dir.join(file),
                OpenOptions::new().append(true).create(true),
            )
            .map(Mutex::new)
        };
        Ok(Self {
            turns: appending(TURNS_FILE)?,
            grades: appending(GRADES_FILE)?,
            dir: dir.to_path_buf(),
            manifest,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn manifest_in(dir: &Path) -> Result<Option<Manifest>> {
        let path = dir.join(MANIFEST_FILE);
        match path.exists() {
            false => Ok(None),
            true => serde_json::from_str(&std::fs::read_to_string(&path)?)
                .wrap_err_with(|| format!("parse {}", path.display()))
                .map(Some),
        }
    }

    fn append<T: Serialize>(file: &Mutex<File>, record: &T) -> Result<()> {
        let mut line = serde_json::to_string(record)?;
        line.push('\n');
        let mut file = file.lock();
        file.write_all(line.as_bytes())?;
        Ok(file.flush()?)
    }

    pub fn record_turn(&self, turn: &TurnRecord) -> Result<()> {
        Self::append(&self.turns, turn)
    }

    pub fn record_grade(&self, grade: &GradeRecord) -> Result<()> {
        Self::append(&self.grades, grade)
    }

    pub fn results(&self) -> Result<Results> {
        Results::read(&self.dir)
    }
}

/// Everything a run directory holds.
pub struct Results {
    pub manifest: Manifest,
    pub turns: Vec<TurnRecord>,
    pub grades: Vec<GradeRecord>,
}

impl Results {
    pub fn read(dir: &Path) -> Result<Self> {
        Ok(Self {
            manifest: Store::manifest_in(dir)?
                .ok_or_else(|| eyre::eyre!("{} holds no eval run", dir.display()))?,
            turns: read_lines(&dir.join(TURNS_FILE))?,
            grades: read_lines(&dir.join(GRADES_FILE))?,
        })
    }

    /// The latest turn of `arm` on each question, under the versions of the
    /// arm and of the questions `set` holds now, by question id. Turns of
    /// any other version are left out.
    pub fn current_turns<'a>(
        &'a self,
        arm: &ArmEntry,
        set: &EvalSet,
    ) -> HashMap<&'a str, &'a TurnRecord> {
        let versions = versions(set);
        self.turns
            .iter()
            .filter(|turn| arm.produced(turn) && is_current(&versions, turn))
            .map(|turn| (turn.question.as_str(), turn))
            .collect()
    }

    /// How many recorded turns belong to no current arm or question version.
    pub fn stale_turns(&self, set: &EvalSet) -> usize {
        let versions = versions(set);
        self.turns
            .iter()
            .filter(|turn| {
                !(is_current(&versions, turn)
                    && self.manifest.arms.iter().any(|arm| arm.produced(turn)))
            })
            .count()
    }

    /// The latest reading of `turn_id` on `pass`.
    pub fn grade(&self, turn_id: &str, pass: u8) -> Option<&GradeRecord> {
        self.grades
            .iter()
            .rev()
            .find(|grade| grade.turn_id == turn_id && grade.pass == pass)
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::evals::set::tests::example_dir;
    use workforce_hub::eval_grader::{Grade, Verdict};

    pub(crate) fn turn(arm: &Arm, set: &EvalSet, question: &str, attempt: u32) -> TurnRecord {
        let asked = set.questions.iter().find(|q| q.id == question).unwrap();
        TurnRecord {
            arm: arm.name.clone(),
            arm_hash: arm.hash(),
            question: question.to_string(),
            question_hash: asked.hash(),
            use_case: asked.use_case,
            attempt,
            turn_id: format!("{}-{question}-{attempt}", arm.name),
            status: TurnStatus::Answered,
            answer: "42".into(),
            error: None,
            session_id: Some("s".into()),
            wall_ms: 1_000,
            capped: false,
            config: None,
            metrics: Some(TurnMetrics {
                cost_usd: 0.1,
                rounds: 3,
                ..Default::default()
            }),
            at: Utc::now(),
        }
    }

    pub(crate) fn judge(agent: &str) -> Judge {
        Judge {
            agent: agent.to_string(),
            model: None,
        }
    }

    pub(crate) fn graded(turn_id: &str, pass: u8, verdict: Verdict) -> GradeRecord {
        GradeRecord {
            turn_id: turn_id.to_string(),
            pass,
            outcome: GradeOutcome::Graded(Grade {
                verdict,
                cited: true,
                met: vec![],
                fell_for_trap: false,
                reason: "because".into(),
            }),
            model: Some("judge".into()),
            cost_usd: 0.01,
            at: Utc::now(),
        }
    }

    #[test]
    fn a_reopened_run_keeps_its_id_and_everything_recorded() {
        let set = EvalSet::load(&example_dir()).unwrap();
        let arms = [Arm::of_agent("analyst")];
        let dir = tempfile::tempdir().unwrap();
        let run_id = {
            let store = Store::open(dir.path(), &set, &arms, judge("analyst")).unwrap();
            store
                .record_turn(&turn(&arms[0], &set, "db-001", 1))
                .unwrap();
            store
                .record_grade(&graded("analyst-db-001-1", 1, Verdict::Correct))
                .unwrap();
            store.manifest.run_id.clone()
        };
        let store = Store::open(dir.path(), &set, &arms, judge("analyst")).unwrap();
        assert_eq!(store.manifest.run_id, run_id);
        store
            .record_turn(&turn(&arms[0], &set, "code-001", 1))
            .unwrap();
        let results = store.results().unwrap();
        assert_eq!(results.turns.len(), 2);
        assert_eq!(
            results.grade("analyst-db-001-1", 1).unwrap().outcome,
            graded("analyst-db-001-1", 1, Verdict::Correct).outcome
        );
        assert!(results.grade("analyst-db-001-1", 2).is_none());
    }

    #[test]
    fn results_of_another_version_of_a_question_or_arm_are_left_out() {
        let set = EvalSet::load(&example_dir()).unwrap();
        let arm = Arm::of_agent("analyst");
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            &set,
            std::slice::from_ref(&arm),
            judge("analyst"),
        )
        .unwrap();
        let mut reworded = turn(&arm, &set, "db-001", 1);
        reworded.question_hash = "an-older-wording".into();
        let mut reconfigured = turn(&arm, &set, "code-001", 1);
        reconfigured.arm_hash = "other-overrides".into();
        for record in [&reworded, &reconfigured, &turn(&arm, &set, "inv-001", 1)] {
            store.record_turn(record).unwrap();
        }
        let results = store.results().unwrap();
        let current = results.current_turns(&store.manifest.arms[0], &set);
        assert_eq!(current.keys().copied().collect::<Vec<_>>(), ["inv-001"]);
        assert_eq!(results.stale_turns(&set), 2);
    }

    #[test]
    fn the_latest_attempt_and_the_latest_reading_win() {
        let set = EvalSet::load(&example_dir()).unwrap();
        let arm = Arm::of_agent("analyst");
        let dir = tempfile::tempdir().unwrap();
        let store = Store::open(
            dir.path(),
            &set,
            std::slice::from_ref(&arm),
            judge("analyst"),
        )
        .unwrap();
        store.record_turn(&turn(&arm, &set, "db-001", 1)).unwrap();
        store.record_turn(&turn(&arm, &set, "db-001", 2)).unwrap();
        store.record_grade(&graded("t", 1, Verdict::Wrong)).unwrap();
        store
            .record_grade(&graded("t", 1, Verdict::Partly))
            .unwrap();
        let results = store.results().unwrap();
        let current = results.current_turns(&store.manifest.arms[0], &set);
        assert_eq!(current["db-001"].attempt, 2);
        assert_eq!(
            results.grade("t", 1).unwrap().outcome,
            graded("t", 1, Verdict::Partly).outcome
        );
    }

    #[test]
    fn a_directory_holds_one_sets_results() {
        let set = EvalSet::load(&example_dir()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let arms = [Arm::of_agent("analyst")];
        Store::open(dir.path(), &set, &arms, judge("analyst")).unwrap();
        let other = EvalSet {
            name: "other".into(),
            ..set
        };
        assert!(Store::open(dir.path(), &other, &arms, judge("analyst")).is_err());
    }

    #[test]
    fn a_run_has_one_judge() {
        let set = EvalSet::load(&example_dir()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        let arms = [Arm::of_agent("analyst")];
        Store::open(dir.path(), &set, &arms, judge("analyst")).unwrap();
        Store::open(dir.path(), &set, &arms, judge("analyst")).unwrap();
        assert!(Store::open(dir.path(), &set, &arms, judge("other")).is_err());
        let stronger = Judge {
            model: Some("powerful".into()),
            ..judge("analyst")
        };
        assert!(Store::open(dir.path(), &set, &arms, stronger).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn result_files_are_private_to_their_owner() {
        use std::os::unix::fs::PermissionsExt;
        let set = EvalSet::load(&example_dir()).unwrap();
        let dir = tempfile::tempdir().unwrap();
        Store::open(
            dir.path(),
            &set,
            &[Arm::of_agent("analyst")],
            judge("analyst"),
        )
        .unwrap();
        for file in [MANIFEST_FILE, TURNS_FILE, GRADES_FILE] {
            let mode = std::fs::metadata(dir.path().join(file))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600, "{file}");
        }
    }
}
