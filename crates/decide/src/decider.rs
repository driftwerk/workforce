//! The policy over backends: which one answers, which one steps in when it
//! fails, and whether a second one is consulted for comparison.

use crate::call::Call;
use crate::client::{DecideError, Keys, SystemOneClient};
use crate::question::{Question, Response};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;

/// How the backends are combined. The primary's answer is always the one
/// acted on; the fallback answers only when the primary errors; compare asks
/// both and logs whether they agree, so a candidate model (Jev beside Kev, or
/// a bigger Kev beside a smaller one) can be judged on live traffic without
/// steering anything — or slowing it: the primary's answer never waits for
/// the comparison.
#[derive(Debug, Clone, Default)]
pub struct DeciderPolicy {
    pub fallback_on_error: bool,
    pub compare: bool,
}

pub struct Decider {
    primary: Arc<SystemOneClient>,
    secondary: Option<Arc<SystemOneClient>>,
    policy: DeciderPolicy,
}

#[derive(Debug, Clone)]
pub struct Decision {
    pub response: Response,
    /// Which backend the response came from.
    pub backend: String,
    /// The primary failed and the secondary answered instead.
    pub fell_back: bool,
}

/// A backend this caller can ask: it needs no key, or the caller has one.
struct Ready {
    client: Arc<SystemOneClient>,
    key: Option<String>,
}

impl Ready {
    fn of(client: &Arc<SystemOneClient>, keys: &dyn Keys) -> Option<Self> {
        let key = keys
            .key(client.name())
            .map(|k| k.trim().to_string())
            .filter(|k| !k.is_empty());
        (key.is_some() || !client.requires_key()).then(|| Self {
            client: Arc::clone(client),
            key,
        })
    }

    fn name(&self) -> &str {
        self.client.name()
    }

    async fn ask(
        &self,
        call: Call,
        state: &Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Response, DecideError> {
        self.client
            .ask(call, self.key.as_deref(), state, questions)
            .await
    }

    fn answers(&self, response: Response, fell_back: bool) -> Decision {
        Decision {
            response,
            backend: self.name().to_string(),
            fell_back,
        }
    }
}

/// The fallback is stepping in for `primary`, which failed with `error`.
/// Load is expected and counted; anything else is worth a warning.
fn log_fallback(call: Call, primary: &str, fallback: &str, error: &DecideError) {
    call.fell_back(fallback, error.fallback_reason());
    if error.is_overloaded() {
        tracing::debug!(target: "decision", r#use = call.usage.as_str(), primary, %error, fallback, "primary overloaded; using the fallback");
    } else {
        tracing::warn!(target: "decision", r#use = call.usage.as_str(), primary, %error, fallback, "primary failed; using the fallback");
    }
}

impl Decider {
    pub fn new(
        primary: Arc<SystemOneClient>,
        secondary: Option<Arc<SystemOneClient>>,
        policy: DeciderPolicy,
    ) -> Self {
        Self {
            primary,
            secondary,
            policy,
        }
    }

    pub fn primary_name(&self) -> &str {
        self.primary.name()
    }

    /// Ask with the caller's `keys`. A backend that needs a key the caller
    /// lacks is left out of this call: a missing primary hands over to the
    /// fallback (when the policy has one), a missing secondary just means no
    /// fallback and no comparison, and with nothing left to ask the answer
    /// is [`DecideError::Unavailable`] without a request being sent.
    ///
    /// `call`'s budget applies to each backend asked, so a call returns
    /// within one budget, or two when a fallback steps in. A backend that
    /// is busy or out of time fails like any other: the fallback answers,
    /// or the error says the call was [overloaded](DecideError::is_overloaded).
    pub async fn ask(
        &self,
        call: Call,
        keys: &dyn Keys,
        state: &Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecideError> {
        let primary = Ready::of(&self.primary, keys);
        let secondary = self.secondary.as_ref().and_then(|s| Ready::of(s, keys));
        match (primary, secondary) {
            (Some(primary), secondary) => {
                if secondary.is_none() {
                    if let Some(s) = &self.secondary {
                        tracing::debug!(
                            primary = primary.name(),
                            secondary = s.name(),
                            "secondary has no key for this caller; asking the primary alone"
                        );
                    }
                }
                self.ask_ready(call, primary, secondary, state, questions)
                    .await
            }
            (None, Some(fallback)) if self.policy.fallback_on_error => {
                tracing::debug!(
                    primary = self.primary.name(),
                    fallback = fallback.name(),
                    "primary has no key for this caller; asking the fallback"
                );
                call.fell_back(fallback.name(), "no_key");
                let response = fallback.ask(call, state, questions).await?;
                Ok(fallback.answers(response, true))
            }
            (None, secondary) => Err(DecideError::Unavailable {
                backends: std::iter::once(self.primary.name())
                    .chain(
                        self.secondary
                            .as_deref()
                            .filter(|_| secondary.is_none())
                            .map(SystemOneClient::name),
                    )
                    .map(str::to_string)
                    .collect(),
            }),
        }
    }

    async fn ask_ready(
        &self,
        call: Call,
        primary: Ready,
        secondary: Option<Ready>,
        state: &Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecideError> {
        match secondary {
            Some(secondary) if self.policy.compare => {
                self.compare(call, primary, secondary, state, questions)
                    .await
            }
            secondary => match primary.ask(call, state, questions).await {
                Ok(response) => Ok(primary.answers(response, false)),
                Err(e) => match secondary.filter(|_| self.policy.fallback_on_error) {
                    Some(fallback) => {
                        log_fallback(call, primary.name(), fallback.name(), &e);
                        match fallback.ask(call, state, questions).await {
                            Ok(response) => Ok(fallback.answers(response, true)),
                            Err(e2) => Err(DecideError::all_failed(e, e2)),
                        }
                    }
                    None => Err(e),
                },
            },
        }
    }

    /// Ask both at once and act on the primary as soon as it answers; the
    /// comparison is logged whenever the secondary gets there. A primary
    /// that fails is the one case that waits for the secondary, as its
    /// fallback.
    async fn compare(
        &self,
        call: Call,
        primary: Ready,
        secondary: Ready,
        state: &Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Decision, DecideError> {
        let second = Arc::new(secondary);
        let beside = tokio::spawn({
            let (second, state, questions) =
                (Arc::clone(&second), state.clone(), questions.clone());
            async move { second.ask(call, &state, &questions).await }
        });
        match primary.ask(call, state, questions).await {
            Ok(response) => {
                let (first, acted_on) = (primary.name().to_string(), response.clone());
                tokio::spawn(async move {
                    if let Ok(compared) = beside.await {
                        log_comparison(&first, second.name(), &acted_on, compared);
                    }
                });
                Ok(primary.answers(response, false))
            }
            Err(e) if self.policy.fallback_on_error => {
                log_fallback(call, primary.name(), second.name(), &e);
                match beside.await {
                    Ok(Ok(response)) => Ok(second.answers(response, true)),
                    Ok(Err(e2)) => Err(DecideError::all_failed(e, e2)),
                    Err(_) => Err(e),
                }
            }
            Err(e) => Err(e),
        }
    }
}

/// Whether two backends reached the same verdict on every question.
fn agree(primary: &Response, secondary: &Response) -> bool {
    primary.answers.iter().all(|(id, a)| {
        secondary
            .answers
            .get(id)
            .is_some_and(|b| a.verdict() == b.verdict())
    })
}

/// A secondary too busy to be compared is load, not a fault, and never a
/// disagreement.
fn log_comparison(
    primary: &str,
    secondary: &str,
    acted_on: &Response,
    compared: Result<Response, DecideError>,
) {
    match compared {
        Ok(response) => {
            tracing::info!(target: "decision", primary, secondary, agrees = agree(acted_on, &response), "decision comparison")
        }
        Err(error) if error.is_overloaded() => {
            tracing::debug!(target: "decision", primary, secondary, %error, "comparison backend overloaded; nothing compared")
        }
        Err(error) => {
            tracing::warn!(target: "decision", primary, secondary, %error, "comparison backend failed")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::question::Answer;

    fn said(verdict: &str) -> Response {
        Response {
            model: "m".into(),
            answers: BTreeMap::from([(
                "q".to_string(),
                Answer::Choice {
                    choice: verdict.into(),
                    probabilities: BTreeMap::from([(verdict.to_string(), 0.9)]),
                    confidence: 0.9,
                },
            )]),
            usage: Default::default(),
        }
    }

    #[test]
    fn backends_agree_when_every_question_has_the_same_verdict() {
        assert!(agree(&said("approved"), &said("approved")));
        assert!(!agree(&said("approved"), &said("rejected")));
        let silent = Response {
            answers: BTreeMap::new(),
            ..said("approved")
        };
        assert!(!agree(&said("approved"), &silent));
    }
}
