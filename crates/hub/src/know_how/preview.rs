//! What one person's eval turns on an agent would be shown of its know-how,
//! counted before a run spends anything: whether it is on and shown, how
//! many entries they can see, and how many each question matches. Numbers
//! only: never a name, an entry or a score.

use serde::{Deserialize, Serialize};
use workforce_common::settings::{KnowHowConfig, KnowHowInject};

use super::{Access, KnowHow};

/// Most questions one preview counts.
pub const MAX_QUESTIONS: usize = 500;
/// Most characters of one question, as for a chat message.
pub const MAX_QUESTION_CHARS: usize = 20_000;

/// `POST /api/evals/know-how`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct PreviewRequest {
    /// The agent; the install's default when absent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    /// A `config.toml` to count under in place of the agent's own: an arm's
    /// overrides laid over it. A system admin's to send.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub config: Option<String>,
    /// Questions to match, asked as an eval asks them.
    #[serde(default)]
    pub questions: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Preview {
    /// Know-how is on for the agent under the config.
    pub enabled: bool,
    /// And put in front of requests (`inject = "on"`).
    pub shown: bool,
    /// The hub can match a request to entries at all: without an embedder
    /// nothing is ever shown.
    pub matching: bool,
    /// The agent whose entries a test copy reads, when it reads another's.
    pub source_agent: Option<String>,
    /// The score a request needs to be shown an entry.
    pub min_score: f32,
    /// Entries the person can see.
    pub visible: usize,
    /// How many entries each question would be shown, in order.
    pub matched: Vec<usize>,
}

impl KnowHow {
    /// The preview for `identity` on `agent`, which runs under `running`
    /// and is counted under `counted`: its config as if know-how were on,
    /// so a copy whose own config leaves it off shows what an arm turning
    /// it on would find. Counted as an eval's turn sees it, the person's own
    /// conversation; nothing is recorded.
    pub async fn preview(
        &self,
        agent: &str,
        running: Option<&KnowHowConfig>,
        counted: Option<&KnowHowConfig>,
        identity: &str,
        access: &Access,
        questions: &[String],
    ) -> eyre::Result<Preview> {
        let (visible, matched) = match counted {
            Some(cfg) => {
                let visible = self.visible(agent, cfg, identity, access, false).await?;
                let matchable = super::of_kinds(cfg, visible.clone());
                let mut matched = Vec::with_capacity(questions.len());
                for question in questions {
                    matched.push(
                        self.best(agent, cfg, matchable.clone(), question)
                            .await?
                            .len(),
                    );
                }
                (visible.len(), matched)
            }
            None => (0, vec![0; questions.len()]),
        };
        Ok(Preview {
            enabled: running.is_some(),
            shown: running.is_some_and(|cfg| cfg.inject == KnowHowInject::On),
            matching: self.embedder.is_some(),
            source_agent: counted.and_then(|cfg| cfg.source_agent.clone()),
            min_score: counted.or(running).map_or(0.0, |cfg| cfg.min_score),
            visible,
            matched,
        })
    }
}
