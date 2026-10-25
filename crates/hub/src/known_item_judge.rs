//! Shadow mode's opt-in LLM judge (DW-676, PR 10; design r2 section 6.5,
//! ladder step 2): for the artifacts' known-item queries, an LLM grades
//! the artifacts both sides found, and each side's ranking is scored by
//! nDCG@5 against those grades. The two sides embed with different
//! models, so their agreement measures difference, not quality; the
//! judge measures quality.
//!
//! **Off unless `[vellev.shadow.judge] enabled`.** It reads artifacts
//! whatever their access rules, and its calls cost (design r2 section 13).
//! Off, it is never made: no LLM is looked up, no artifact is read for it,
//! and nothing is asked ([`Judge::from_config`]). On, it judges at most
//! `daily_cap` queries a UTC day, and only the known-item queries, which
//! are made from artifacts, never a user's: no live query text is kept.

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use eyre::{eyre, Result};
use workforce_agent::llm::provider::LlmProvider;
use workforce_common::settings::WorkforceConfig;
use workforce_common::shadow_postgres::ArtifactText;

/// How deep each side's ranking is judged.
pub const DEPTH: usize = 5;
/// How much of each artifact the judge is shown.
pub const EXCERPT_CHARS: usize = 1500;
/// How long one grading may take: a provider that never answers costs
/// that item its grades, not the night's run.
pub const GRADE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// What grades what was found for a query: for each of `found`, 0 (not
/// relevant), 1 (partly) or 2 (relevant), in its order.
#[async_trait]
pub trait Grades: Send + Sync {
    async fn grade(&self, query: &str, found: &[ArtifactText]) -> Result<Vec<u8>>;
}

/// The judge, when on.
pub struct Judge {
    /// Queries judged a UTC day at most.
    pub cap: usize,
    pub grades: Arc<dyn Grades>,
}

impl Judge {
    /// The judge `config` asks for: `None` when it is off, `llm` then never
    /// called. On, it grades with what `llm` gives, the hub's tagger LLM;
    /// `Err` when there is none.
    pub fn from_config(
        config: &WorkforceConfig,
        llm: impl FnOnce() -> Option<Arc<dyn LlmProvider>>,
    ) -> Result<Option<Self>> {
        let Some(cap) = config.vellev.shadow.judge_cap()? else {
            return Ok(None);
        };
        let llm = llm().ok_or_else(|| {
            eyre!(
                "[vellev.shadow.judge] is on, but this hub has no LLM to judge with (the artifact tagger's): set one up, or turn the judge off"
            )
        })?;
        Ok(Some(Self {
            cap,
            grades: Arc::new(LlmGrades {
                llm,
                timeout: GRADE_TIMEOUT,
            }),
        }))
    }
}

/// [`Grades`] by an LLM.
pub struct LlmGrades {
    pub llm: Arc<dyn LlmProvider>,
    /// How long a grading may take ([`GRADE_TIMEOUT`]).
    pub timeout: std::time::Duration,
}

#[async_trait]
impl Grades for LlmGrades {
    async fn grade(&self, query: &str, found: &[ArtifactText]) -> Result<Vec<u8>> {
        let system = "You judge search results. For the search query and each numbered \
                      document, say how relevant the document is to the query: 0 not \
                      relevant, 1 partly relevant, 2 relevant. Reply with ONLY one line a \
                      document, `n: grade`, for every document, and nothing else.";
        let mut user = format!("Query: {query}\n");
        for (n, artifact) in found.iter().enumerate() {
            let excerpt: String = artifact.content.chars().take(EXCERPT_CHARS).collect();
            user.push_str(&format!(
                "\n[{}] Title: {}\n{}\n",
                n + 1,
                artifact.title,
                excerpt
            ));
        }
        let asked = crate::artifact_index::ask_costed(
            self.llm.as_ref(),
            system,
            user,
            16 * found.len().max(1) as u32,
            "shadow_judge",
        );
        let answer = tokio::time::timeout(self.timeout, asked)
            .await
            .map_err(|_| eyre!("the judge did not answer within {:?}", self.timeout))??;
        parse_grades(&answer, found.len())
    }
}

/// The grades in `answer`, one `n: g` a line for each of `n` artifacts
/// (1-based); `Err` unless every one is graded 0, 1 or 2, once.
pub fn parse_grades(answer: &str, n: usize) -> Result<Vec<u8>> {
    let mut grades: Vec<Option<u8>> = vec![None; n];
    for line in answer.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let (place, grade) = line
            .split_once(':')
            .ok_or_else(|| eyre!("the judge answered {line:?}, not `n: grade`"))?;
        let place: usize = place
            .trim()
            .parse()
            .map_err(|_| eyre!("the judge numbered {place:?}"))?;
        let grade: u8 = grade
            .trim()
            .parse()
            .map_err(|_| eyre!("the judge graded {grade:?}"))?;
        if grade > 2 {
            return Err(eyre!("the judge graded {grade}, not 0, 1 or 2"));
        }
        let slot = place
            .checked_sub(1)
            .and_then(|i| grades.get_mut(i))
            .ok_or_else(|| eyre!("the judge graded document {place} of {n}"))?;
        if slot.replace(grade).is_some() {
            return Err(eyre!("the judge graded document {place} twice"));
        }
    }
    grades
        .into_iter()
        .enumerate()
        .map(|(i, g)| g.ok_or_else(|| eyre!("the judge did not grade document {}", i + 1)))
        .collect()
}

/// `ranked`'s nDCG at `depth`, by `grades` (gain 2^g − 1, discounted by
/// log2 of the rank plus one), against the best order of every grade in
/// the pool. 0 when nothing pooled is relevant: neither side found any.
pub fn ndcg(ranked: &[String], grades: &HashMap<String, u8>, depth: usize) -> f64 {
    let gain = |g: u8| f64::from((1u32 << g) - 1);
    let discounted = |gains: &mut dyn Iterator<Item = f64>| -> f64 {
        gains
            .take(depth)
            .enumerate()
            .map(|(i, g)| g / ((i + 2) as f64).log2())
            .sum()
    };
    let mut best: Vec<u8> = grades.values().copied().collect();
    best.sort_unstable_by(|a, b| b.cmp(a));
    let ideal = discounted(&mut best.into_iter().map(gain));
    if ideal == 0.0 {
        return 0.0;
    }
    let got = discounted(
        &mut ranked
            .iter()
            .map(|id| grades.get(id).copied().map_or(0.0, gain)),
    );
    got / ideal
}

#[cfg(test)]
#[path = "known_item_judge_tests.rs"]
pub(crate) mod tests;
