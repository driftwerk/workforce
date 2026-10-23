//! The hub's HTTP API as `workforce eval` uses it: every call is made with
//! one person's sign-in, so turns run, are billed and are budgeted as theirs.

use std::time::{Duration, Instant};

use eyre::{bail, eyre, Result, WrapErr};
use reqwest::header::{ETAG, IF_MATCH, RETRY_AFTER};
use reqwest::{Method, RequestBuilder, Response, StatusCode, Url};
use serde::Deserialize;
use serde_json::Value;
use workforce_gateway_client::{
    AgentInfo, AgentsResponse, ChatOutcome, ChatRequest, ChatResponse, SessionsResponse,
};
use workforce_hub::eval_grader::{GradeRequest, GradeResponse};
use workforce_hub::know_how::preview::{Preview, PreviewRequest};

use super::results::TurnMetrics;

/// How many times a rate-limited call is sent again before it fails.
const RATE_LIMIT_RETRIES: u32 = 5;

/// The longest a rate-limited call waits before it is sent again.
const RATE_LIMIT_MAX_WAIT: Duration = Duration::from_secs(60);

/// How long any call but a turn may take.
const CALL_TIMEOUT: Duration = Duration::from_secs(180);

/// How long the hub is given to file a finished turn's telemetry.
const TELEMETRY_WAITS: [Duration; 3] = [
    Duration::ZERO,
    Duration::from_millis(500),
    Duration::from_secs(2),
];

pub struct Hub {
    base: Url,
    token: String,
    http: reqwest::Client,
}

/// Who the run is made as.
#[derive(Debug, Deserialize)]
pub struct Me {
    pub id: String,
    #[serde(default)]
    pub display_name: String,
}

/// A turn the hub took.
#[derive(Debug)]
pub enum Asked {
    Answered {
        reply: String,
        session_id: String,
        task_id: String,
        /// The turn stopped at its round cap.
        capped: bool,
        /// The content hash of the agent's config the whole turn ran under.
        config: Option<String>,
        /// How long the hub took over it, waits for the rate limit aside.
        took: Duration,
    },
    /// The turn did not run, for this reason (the budget, say).
    Refused(String),
}

/// A turn the hub did not take.
#[derive(Debug)]
pub enum AskError {
    /// This turn failed; the next may not.
    Turn(String),
    /// No turn will work until something is put right.
    Run(eyre::Report),
}

#[derive(Deserialize)]
struct TaskTrace {
    task: TracedTask,
    #[serde(default)]
    tools: Vec<TracedTool>,
}

#[derive(Deserialize)]
struct TracedTask {
    model: String,
    cost_usd: f64,
    input_tokens: u64,
    output_tokens: u64,
    cache_creation_input_tokens: u64,
    cache_read_input_tokens: u64,
    rounds: u32,
}

#[derive(Deserialize)]
struct TracedTool {
    tool_name: String,
}

/// A config write the hub took.
#[derive(Debug, Deserialize)]
pub struct SavedConfig {
    pub etag: String,
    /// The settings in it the agent only takes up after a restart.
    #[serde(default)]
    pub restart_required: Vec<String>,
}

/// An agent's `config.toml` as it was read, and the tag a write must match.
pub struct ConfigFile {
    pub content: String,
    pub etag: String,
}

async fn detail(response: Response) -> String {
    let status = response.status();
    let body = response.text().await.unwrap_or_default();
    let said = serde_json::from_str::<Value>(&body)
        .ok()
        .and_then(|json| json.get("error").and_then(Value::as_str).map(String::from))
        .unwrap_or(body);
    match said.trim().is_empty() {
        true => status.to_string(),
        false => format!("{status}: {}", said.trim()),
    }
}

impl Hub {
    pub fn new(url: &str, token: &str) -> Result<Self> {
        let http_url = match url.split_once("://") {
            Some(("ws", rest)) => format!("http://{rest}"),
            Some(("wss", rest)) => format!("https://{rest}"),
            _ => url.to_string(),
        };
        let base = Url::parse(&http_url).wrap_err_with(|| format!("hub url {url:?}"))?;
        if base.cannot_be_a_base() {
            bail!("hub url {url:?} is not an http address");
        }
        Ok(Self {
            base,
            token: token.to_string(),
            http: reqwest::Client::new(),
        })
    }

    fn url(&self, segments: &[&str]) -> Url {
        let mut url = self.base.clone();
        url.path_segments_mut()
            .expect("checked to be a base in `new`")
            .pop_if_empty()
            .extend(segments);
        url
    }

    fn request(&self, method: Method, segments: &[&str]) -> RequestBuilder {
        self.http
            .request(method, self.url(segments))
            .bearer_auth(&self.token)
            .timeout(CALL_TIMEOUT)
    }

    /// Send `request`, waiting out the hub's rate limit.
    async fn send(request: RequestBuilder) -> reqwest::Result<Response> {
        Self::send_timed(request)
            .await
            .map(|(response, _)| response)
    }

    /// [`Self::send`], with how long the attempt that was answered took.
    async fn send_timed(request: RequestBuilder) -> reqwest::Result<(Response, Duration)> {
        let mut retries = 0;
        loop {
            let attempt = request
                .try_clone()
                .expect("eval requests carry no streamed body");
            let started = Instant::now();
            let response = attempt.send().await?;
            if response.status() != StatusCode::TOO_MANY_REQUESTS || retries == RATE_LIMIT_RETRIES {
                return Ok((response, started.elapsed()));
            }
            let wait = response
                .headers()
                .get(RETRY_AFTER)
                .and_then(|v| v.to_str().ok())
                .and_then(|v| v.parse().ok())
                .map_or(Duration::from_secs(1), Duration::from_secs);
            tokio::time::sleep(wait.min(RATE_LIMIT_MAX_WAIT)).await;
            retries += 1;
        }
    }

    async fn ok(request: RequestBuilder, what: &str) -> Result<Response> {
        let response = Self::send(request)
            .await
            .wrap_err_with(|| format!("{what}: the hub did not answer"))?;
        match response.status().is_success() {
            true => Ok(response),
            false => Err(eyre!("{what}: {}", detail(response).await)),
        }
    }

    /// The person the token signs in. The owner token signs in nobody, and
    /// a turn needs a person.
    pub async fn me(&self) -> Result<Me> {
        let response = Self::send(self.request(Method::GET, &["api", "me"]))
            .await
            .wrap_err("the hub did not answer")?;
        match response.status() {
            StatusCode::OK => Ok(response.json().await?),
            StatusCode::UNAUTHORIZED => Err(eyre!(
                "the token is not a person's sign-in. Evals run as a person: pass that \
                 person's token, not the install's owner token"
            )),
            _ => Err(eyre!("who am I: {}", detail(response).await)),
        }
    }

    /// The agents the person can see.
    pub async fn agents(&self) -> Result<Vec<AgentInfo>> {
        let response = Self::ok(self.request(Method::GET, &["api", "agents"]), "agents").await?;
        Ok(response.json::<AgentsResponse>().await?.agents)
    }

    /// Ask `agent` one question as the person, on `channel`, named
    /// `turn_id`.
    pub async fn ask(
        &self,
        agent: &str,
        channel: &str,
        turn_id: &str,
        question: &str,
        timeout: Duration,
    ) -> Result<Asked, AskError> {
        let request = self
            .request(Method::POST, &["api", "chat"])
            .timeout(timeout)
            .json(&ChatRequest {
                message: question.to_string(),
                channel: Some(channel.to_string()),
                session_id: None,
                agent_id: Some(agent.to_string()),
                turn_id: Some(turn_id.to_string()),
            });
        let (response, took) = Self::send_timed(request).await.map_err(|e| {
            AskError::Turn(match e.is_timeout() {
                true => format!("no answer within {}s", timeout.as_secs()),
                false => format!("the hub did not answer: {e}"),
            })
        })?;
        match response.status() {
            StatusCode::OK => {
                let chat: ChatResponse = response
                    .json()
                    .await
                    .map_err(|e| AskError::Turn(format!("unreadable reply: {e}")))?;
                // A replica that predates evals names no turn and tells no
                // refusal from an answer, so what it sent cannot be recorded
                // as either.
                let task_id = chat.task_id.ok_or_else(|| {
                    AskError::Turn(
                        "answered by a hub replica that predates `workforce eval`".to_string(),
                    )
                })?;
                Ok(match chat.outcome {
                    ChatOutcome::Answered => Asked::Answered {
                        reply: chat.response,
                        session_id: chat.session_id,
                        task_id,
                        capped: chat.capped,
                        config: chat.config,
                        took,
                    },
                    ChatOutcome::Refused => Asked::Refused(chat.response),
                })
            }
            StatusCode::UNPROCESSABLE_ENTITY => Err(AskError::Turn(format!(
                "the agent's scope filter turned the question away: {}",
                detail(response).await
            ))),
            status if status.is_server_error() => Err(AskError::Turn(detail(response).await)),
            _ => Err(AskError::Run(eyre!(
                "asking {agent}: {}",
                detail(response).await
            ))),
        }
    }

    /// What the hub filed for the turn `task_id`: `None` when it files no
    /// telemetry, or filed none for this turn.
    pub async fn metrics(&self, task_id: &str) -> Result<Option<TurnMetrics>> {
        for wait in TELEMETRY_WAITS {
            tokio::time::sleep(wait).await;
            let request = self.request(Method::GET, &["api", "stats", "tasks", task_id]);
            let response = Self::send(request)
                .await
                .wrap_err("the hub did not answer")?;
            match response.status() {
                StatusCode::OK => {
                    let trace: TaskTrace = response.json().await?;
                    return Ok(Some(TurnMetrics {
                        cost_usd: trace.task.cost_usd,
                        input_tokens: trace.task.input_tokens,
                        output_tokens: trace.task.output_tokens,
                        cache_read_tokens: trace.task.cache_read_input_tokens,
                        cache_write_tokens: trace.task.cache_creation_input_tokens,
                        rounds: trace.task.rounds,
                        model: trace.task.model,
                        tools: trace.tools.into_iter().map(|t| t.tool_name).collect(),
                    }));
                }
                StatusCode::NOT_FOUND => continue,
                StatusCode::SERVICE_UNAVAILABLE => return Ok(None),
                _ => bail!("turn telemetry: {}", detail(response).await),
            }
        }
        Ok(None)
    }

    /// Whether the hub is new enough to run evals: one that is refuses a
    /// rubric with no points before any model is called.
    pub async fn runs_evals(&self, pointless: &GradeRequest) -> Result<bool> {
        let call = self
            .request(Method::POST, &["api", "evals", "grade"])
            .json(pointless);
        let response = Self::send(call).await.wrap_err("the hub did not answer")?;
        Ok(response.status() == StatusCode::BAD_REQUEST
            && response
                .json::<Value>()
                .await
                .is_ok_and(|said| said["error"].is_string()))
    }

    /// The ids of the person's sessions with `agent` on `channel`.
    pub async fn sessions_on(&self, agent: &str, channel: &str) -> Result<Vec<String>> {
        let request = self
            .request(Method::GET, &["api", "sessions"])
            .query(&[("agent_id", agent), ("channel", channel)]);
        let listed: SessionsResponse = Self::ok(request, "listing the turn's sessions")
            .await?
            .json()
            .await?;
        Ok(listed
            .sessions
            .into_iter()
            .filter(|session| session.channel.as_deref() == Some(channel))
            .map(|session| session.id)
            .collect())
    }

    /// Delete the person's session `id` with `agent`.
    pub async fn delete_session(&self, agent: &str, id: &str) -> Result<()> {
        let request = self
            .request(Method::DELETE, &["api", "sessions", id])
            .query(&[("agent_id", agent)]);
        Self::ok(request, "deleting the turn's session")
            .await
            .map(drop)
    }

    pub async fn grade(&self, request: &GradeRequest) -> Result<GradeResponse> {
        let call = self
            .request(Method::POST, &["api", "evals", "grade"])
            .json(request);
        Ok(Self::ok(call, "grading").await?.json().await?)
    }

    /// What the person's eval turns on `agent` would be shown of its
    /// know-how, counted under `config` in place of its own when given, with
    /// how many entries each of `questions` matches.
    pub async fn know_how(
        &self,
        agent: &str,
        config: Option<&str>,
        questions: &[String],
    ) -> Result<Preview> {
        let what = format!("counting {agent}'s know-how");
        let call = self
            .request(Method::POST, &["api", "evals", "know-how"])
            .json(&PreviewRequest {
                agent_id: Some(agent.to_string()),
                config: config.map(str::to_string),
                questions: questions.to_vec(),
            });
        let response = Self::send(call)
            .await
            .wrap_err_with(|| format!("{what}: the hub did not answer"))?;
        match response.status() {
            StatusCode::OK => Ok(response.json().await?),
            StatusCode::NOT_FOUND | StatusCode::METHOD_NOT_ALLOWED
                if response.content_length() == Some(0) =>
            {
                Err(eyre!(
                    "{what}: this hub predates the know-how check of `workforce eval`. Update \
                     the hub first"
                ))
            }
            StatusCode::FORBIDDEN if config.is_some() => Err(eyre!(
                "{what}: only a system admin's sign-in may lay an arm's overrides"
            )),
            _ => Err(eyre!("{what}: {}", detail(response).await)),
        }
    }

    pub async fn config(&self, agent: &str) -> Result<ConfigFile> {
        let what = format!("reading {agent}'s config");
        let request = self.request(Method::GET, &["api", "agents", agent, "config"]);
        let response = Self::ok(request, &what).await?;
        let etag = response
            .headers()
            .get(ETAG)
            .and_then(|v| v.to_str().ok())
            .map(str::to_string)
            .ok_or_else(|| eyre!("{what}: the hub sent no ETag"))?;
        Ok(ConfigFile {
            content: response.text().await?,
            etag,
        })
    }

    /// Replace `agent`'s config with `content` if it is still the one
    /// tagged `if_match`.
    pub async fn put_config(
        &self,
        agent: &str,
        content: &str,
        if_match: &str,
    ) -> Result<SavedConfig> {
        let what = format!("writing {agent}'s config");
        let request = self
            .request(Method::PUT, &["api", "agents", agent, "config"])
            .header(IF_MATCH, if_match)
            .body(content.to_string());
        let response = Self::send(request)
            .await
            .wrap_err_with(|| format!("{what}: the hub did not answer"))?;
        match response.status() {
            StatusCode::OK => Ok(response.json().await?),
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => Err(eyre!(
                "{what}: only a system admin's sign-in may change an agent's config. Run \
                 the comparison as one, or configure the test copy by hand and drop the \
                 arm's overrides"
            )),
            _ => Err(eyre!("{what}: {}", detail(response).await)),
        }
    }

    /// The keys of `agent`'s own vault: `None` when the person may not list
    /// them.
    pub async fn vault_keys(&self, agent: &str) -> Result<Option<Vec<String>>> {
        let request = self.request(Method::GET, &["api", "agents", agent, "vault"]);
        let response = Self::send(request)
            .await
            .wrap_err("the hub did not answer")?;
        match response.status() {
            StatusCode::OK => {
                let listed: Value = response.json().await?;
                Ok(Some(
                    listed["keys"]
                        .as_array()
                        .map(|keys| {
                            keys.iter()
                                .filter_map(Value::as_str)
                                .map(String::from)
                                .collect()
                        })
                        .unwrap_or_default(),
                ))
            }
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN | StatusCode::NOT_FOUND => Ok(None),
            _ => bail!("listing {agent}'s vault: {}", detail(response).await),
        }
    }
}
