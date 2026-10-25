//! The judge's rules (DW-676, PR 10), each written before it.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use chrono::Utc;
use workforce_agent::llm::types::{
    ContentBlock, LlmMessage, LlmResponse, LlmToolDef, StopReason, TokenUsage,
};

use super::*;

/// An LLM that answers as told, counting its calls and keeping what it
/// was asked.
pub(crate) struct Answering {
    pub answer: String,
    pub calls: AtomicUsize,
    pub asked: Mutex<Vec<String>>,
}

impl Answering {
    pub(crate) fn new(answer: &str) -> Arc<Self> {
        Arc::new(Self {
            answer: answer.to_string(),
            calls: AtomicUsize::new(0),
            asked: Mutex::new(Vec::new()),
        })
    }
}

#[async_trait]
impl LlmProvider for Answering {
    async fn create_message(
        &self,
        _system: Option<&str>,
        messages: &[LlmMessage],
        _tools: &[LlmToolDef],
        _model: Option<&str>,
        _max_tokens: Option<u32>,
        _cache: bool,
    ) -> Result<LlmResponse> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.asked
            .lock()
            .unwrap()
            .extend(messages.iter().flat_map(|m| {
                m.content.iter().filter_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                })
            }));
        Ok(LlmResponse {
            content: vec![ContentBlock::Text {
                text: self.answer.clone(),
            }],
            stop_reason: StopReason::EndTurn,
            usage: TokenUsage::default(),
            model: "judge".to_string(),
        })
    }
}

fn config(toml: &str) -> WorkforceConfig {
    WorkforceConfig::from_toml(toml).unwrap()
}

/// Off, the judge is not made: the LLM is never looked up, even with a
/// cap set and an LLM there to be had.
#[test]
fn off_the_judge_is_never_made_nor_its_llm_looked_up() {
    let looked = AtomicUsize::new(0);
    for toml in ["", "[vellev.shadow.judge]\ndaily_cap = 50\n"] {
        let judge = Judge::from_config(&config(toml), || {
            looked.fetch_add(1, Ordering::SeqCst);
            Some(Answering::new("1: 2") as Arc<dyn LlmProvider>)
        })
        .unwrap();
        assert!(judge.is_none(), "{toml}: a judge made while off");
    }
    assert_eq!(
        looked.load(Ordering::SeqCst),
        0,
        "the LLM looked up while off"
    );
}

/// On, the judge grades with the hub's LLM at its cap; with no LLM it is
/// refused, naming why.
#[tokio::test]
async fn on_the_judge_grades_with_the_hubs_llm() {
    let on = config("[vellev.shadow.judge]\nenabled = true\n");
    let refused = Judge::from_config(&on, || None)
        .err()
        .expect("no LLM, refused");
    assert!(format!("{refused:#}").contains("LLM"), "{refused:#}");

    let llm = Answering::new("1: 2\n2: 0");
    let judge = Judge::from_config(&on, || Some(llm.clone() as Arc<dyn LlmProvider>))
        .unwrap()
        .expect("on");
    assert_eq!(judge.cap, 200);
    let found = |id: &str, title: &str| ArtifactText {
        id: id.into(),
        title: title.into(),
        content: format!("all about {title}"),
        updated_at: Utc::now(),
        pending: false,
    };
    let grades = judge
        .grades
        .grade(
            "otter dens",
            &[found("a", "Otter dens"), found("b", "Tax forms")],
        )
        .await
        .unwrap();
    assert_eq!(grades, vec![2, 0]);
    assert_eq!(llm.calls.load(Ordering::SeqCst), 1);
    let asked = llm.asked.lock().unwrap().join("\n");
    for shown in [
        "otter dens",
        "Otter dens",
        "Tax forms",
        "all about Tax forms",
    ] {
        assert!(asked.contains(shown), "{shown} not shown: {asked}");
    }
}

/// Grades are read one `n: g` a line; every artifact must be graded 0, 1
/// or 2, once, or the answer is refused.
#[test]
fn grades_are_read_only_when_every_artifact_is_graded_once() {
    assert_eq!(parse_grades("1: 2\n2: 0\n3: 1", 3).unwrap(), vec![2, 0, 1]);
    assert_eq!(parse_grades(" 2:1 \n1 : 0\n", 2).unwrap(), vec![0, 1]);
    for bad in [
        "1: 2\n2: 0",
        "1: 2\n1: 1\n2: 0\n3: 0",
        "1: 3\n2: 0\n3: 0",
        "1: x\n2: 0\n3: 0",
        "1: 2\n2: 0\n4: 1",
    ] {
        assert!(parse_grades(bad, 3).is_err(), "{bad:?} read");
    }
}

/// nDCG@5 worked by hand: gains 2^g − 1 over log2(rank + 1), against the
/// best order of the pool's grades.
#[test]
fn ndcg_on_a_pool_worked_by_hand() {
    let grades: HashMap<String, u8> = [("a", 2), ("b", 0), ("c", 1), ("d", 2)]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
    let ids = |of: &[&str]| of.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    let ideal = 3.0 + 3.0 / 3f64.log2() + 1.0 / 2.0;
    let got = ndcg(&ids(&["a", "b", "c"]), &grades, 5);
    assert!((got - 3.5 / ideal).abs() < 1e-9, "{got}");
    let best = ndcg(&ids(&["d", "a", "c", "b"]), &grades, 5);
    assert!((best - 1.0).abs() < 1e-9, "{best}");
    // An id the pool did not grade gains nothing; past the depth, nothing counts.
    let cut = ndcg(&ids(&["x", "b", "b2", "b3", "b4", "a"]), &grades, 5);
    assert_eq!(cut, 0.0);
    let none: HashMap<String, u8> = [("a".to_string(), 0)].into_iter().collect();
    assert_eq!(ndcg(&ids(&["a"]), &none, 5), 0.0, "nothing relevant pooled");
}

/// An LLM that never answers.
struct Silent;

#[async_trait]
impl LlmProvider for Silent {
    async fn create_message(
        &self,
        _: Option<&str>,
        _: &[LlmMessage],
        _: &[LlmToolDef],
        _: Option<&str>,
        _: Option<u32>,
        _: bool,
    ) -> Result<LlmResponse> {
        std::future::pending().await
    }
}

/// A grading the LLM never answers fails within its timeout, rather than
/// holding the night's run.
#[tokio::test]
async fn a_grading_never_answered_fails_within_its_timeout() {
    let grades = LlmGrades {
        llm: Arc::new(Silent),
        timeout: std::time::Duration::from_millis(100),
    };
    let found = ArtifactText {
        id: "a".into(),
        title: "t".into(),
        content: "c".into(),
        updated_at: Utc::now(),
        pending: false,
    };
    let began = std::time::Instant::now();
    let failed = grades.grade("q", &[found]).await.unwrap_err();
    assert!(
        format!("{failed:#}").contains("did not answer"),
        "{failed:#}"
    );
    assert!(began.elapsed() < std::time::Duration::from_secs(5));
    assert_eq!(GRADE_TIMEOUT, std::time::Duration::from_secs(60));
}
