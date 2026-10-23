//! One System One backend over HTTP.

use crate::call::Call;
use crate::question::{Question, Response};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::Semaphore;
use url::Url;

/// Where one model is served. Kev and Jev differ only here.
#[derive(Debug, Clone)]
pub struct Backend {
    /// How it is named in config and logs: `kev`, `jev`, …
    pub name: String,
    /// Base URL; `/v1/systemone` is appended.
    pub url: Url,
    /// Whether a request needs a bearer key. A caller with no key for such
    /// a backend is not sent to it at all. Jev always needs one; a Kev
    /// server only when started with `KEV_API_KEY`.
    pub requires_key: bool,
    /// `kev-latest`, `jev-latest`, or a pinned version.
    pub model: String,
    /// The longest any one request may take, whatever its call's budget,
    /// and so the longest a request its caller gave up on holds its slot.
    pub timeout: Duration,
    /// How many requests the backend may be working on at once; the rest
    /// wait their turn within their call's budget. A backend slows for
    /// everyone past the point its hardware batches well.
    pub max_concurrent: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum DecideError {
    #[error("{backend}: request failed: {source}")]
    Transport {
        backend: String,
        #[source]
        source: reqwest::Error,
    },
    /// `body` is kept out of the message: a backend's error can quote the
    /// state it was sent, and messages are logged.
    #[error("{backend}: HTTP {status}")]
    Http {
        backend: String,
        status: u16,
        body: String,
    },
    #[error("{backend}: response did not parse: {source}")]
    Decode {
        backend: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("{backend}: no answer under id {id:?}")]
    MissingAnswer { backend: String, id: String },
    #[error("{backend}: busy: no free slot within {waited_ms} ms")]
    Busy { backend: String, waited_ms: u64 },
    #[error("{backend}: no answer within {budget_ms} ms")]
    Timeout { backend: String, budget_ms: u64 },
    #[error("question id {id:?} is asked twice")]
    DuplicateQuestion { id: String },
    #[error("every backend failed: {reason}")]
    AllFailed {
        reason: String,
        /// Every backend was busy or timed out, rather than broken.
        overloaded: bool,
    },
    #[error("no API key for {} at any vault level for this user", backends.join(" or "))]
    Unavailable { backends: Vec<String> },
}

impl DecideError {
    /// No backend could be asked because none had a key for the caller:
    /// an expected state for a person who never added one, not a fault.
    pub fn is_unavailable(&self) -> bool {
        matches!(self, Self::Unavailable { .. })
    }

    /// The backends had no room or no time for this call within its budget:
    /// load, not a fault, and never an answer.
    pub fn is_overloaded(&self) -> bool {
        matches!(
            self,
            Self::Busy { .. }
                | Self::Timeout { .. }
                | Self::AllFailed {
                    overloaded: true,
                    ..
                }
        )
    }

    /// Why a fallback stepped in for the backend that failed this way.
    pub(crate) fn fallback_reason(&self) -> &'static str {
        if self.is_overloaded() {
            "overloaded"
        } else {
            "error"
        }
    }

    pub(crate) fn all_failed(first: Self, second: Self) -> Self {
        Self::AllFailed {
            overloaded: first.is_overloaded() && second.is_overloaded(),
            reason: format!("{first}; {second}"),
        }
    }
}

/// Where a request finds each backend's key. Asked on every call, so a key
/// rotated at its source is used on the next one.
pub trait Keys: Sync {
    fn key(&self, backend: &str) -> Option<String>;
}

impl<F: Fn(&str) -> Option<String> + Sync> Keys for F {
    fn key(&self, backend: &str) -> Option<String> {
        self(backend)
    }
}

pub struct SystemOneClient {
    backend: Backend,
    http: reqwest::Client,
    slots: Arc<Semaphore>,
}

impl SystemOneClient {
    pub fn new(backend: Backend) -> Result<Self, reqwest::Error> {
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(5))
            .timeout(backend.timeout)
            .build()?;
        let slots = Arc::new(Semaphore::new(backend.max_concurrent.max(1)));
        Ok(Self {
            backend,
            http,
            slots,
        })
    }

    pub fn name(&self) -> &str {
        &self.backend.name
    }

    pub fn model(&self) -> &str {
        &self.backend.model
    }

    pub fn requires_key(&self) -> bool {
        self.backend.requires_key
    }

    /// Ask every question of `state` in one request, authorised by `key`,
    /// within `call`'s budget: the wait for one of the backend's slots and
    /// the request together. Out of time is [`DecideError::Busy`] while
    /// still queued and [`DecideError::Timeout`] once sent.
    ///
    /// A request that was sent keeps its slot until the backend answers it
    /// or the backend's `timeout` passes, whether or not its caller is
    /// still waiting: a backend goes on working on a request its caller
    /// gave up on, so the slot is not free before that work is done.
    pub async fn ask(
        &self,
        call: Call,
        key: Option<&str>,
        state: &Value,
        questions: &BTreeMap<String, Question>,
    ) -> Result<Response, DecideError> {
        let backend = &self.backend.name;
        let key = key.map(str::trim).filter(|k| !k.is_empty());
        if key.is_none() && self.backend.requires_key {
            return Err(DecideError::Unavailable {
                backends: vec![backend.clone()],
            });
        }
        let started = Instant::now();
        let deadline = tokio::time::Instant::now() + call.budget;
        let slots = Arc::clone(&self.slots);
        let Ok(slot) = tokio::time::timeout_at(deadline, slots.acquire_owned()).await else {
            let waited = started.elapsed();
            let busy = Err(DecideError::Busy {
                backend: backend.clone(),
                waited_ms: waited.as_millis() as u64,
            });
            call.observe(backend, waited, None, &busy);
            return busy;
        };
        let queued = started.elapsed();
        let request = self.request(key, state, questions);
        let sent = tokio::spawn(async move {
            let _slot = slot;
            let response = request.send().await?;
            Ok::<_, reqwest::Error>((response.status(), response.bytes().await?))
        });
        let out_of_time = |budget: Duration| DecideError::Timeout {
            backend: backend.clone(),
            budget_ms: budget.as_millis() as u64,
        };
        let result = match tokio::time::timeout_at(deadline, sent).await {
            Ok(Ok(Ok((status, body)))) => self.read(key, status, &body, questions),
            Ok(Ok(Err(source))) if source.is_timeout() => Err(out_of_time(self.backend.timeout)),
            Ok(Ok(Err(source))) => Err(DecideError::Transport {
                backend: backend.clone(),
                source,
            }),
            Ok(Err(stopped)) => match stopped.try_into_panic() {
                Ok(panic) => std::panic::resume_unwind(panic),
                Err(_) => Err(out_of_time(call.budget)),
            },
            Err(_) => Err(out_of_time(call.budget)),
        };
        call.observe(backend, queued, Some(started.elapsed() - queued), &result);
        result
    }

    fn request(
        &self,
        key: Option<&str>,
        state: &Value,
        questions: &BTreeMap<String, Question>,
    ) -> reqwest::RequestBuilder {
        let url = self
            .backend
            .url
            .join("v1/systemone")
            .expect("a base url joins a relative path");
        let body = serde_json::json!({
            "model": self.backend.model,
            "state": state,
            "questions": questions,
        });
        let request = self.http.post(url).json(&body);
        match key {
            Some(key) => request.bearer_auth(key),
            None => request,
        }
    }

    fn read(
        &self,
        key: Option<&str>,
        status: reqwest::StatusCode,
        bytes: &[u8],
        questions: &BTreeMap<String, Question>,
    ) -> Result<Response, DecideError> {
        let backend = self.backend.name.clone();
        if !status.is_success() {
            let body = String::from_utf8_lossy(bytes);
            let body = key.map_or(body.to_string(), |key| body.replace(key, "<redacted>"));
            return Err(DecideError::Http {
                backend,
                status: status.as_u16(),
                body: body.chars().take(300).collect(),
            });
        }
        let parsed: Response =
            serde_json::from_slice(bytes).map_err(|source| DecideError::Decode {
                backend: backend.clone(),
                source,
            })?;
        match questions
            .keys()
            .find(|id| !parsed.answers.contains_key(*id))
        {
            Some(id) => Err(DecideError::MissingAnswer {
                backend,
                id: id.clone(),
            }),
            None => Ok(parsed),
        }
    }
}
