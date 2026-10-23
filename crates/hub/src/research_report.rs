//! `research_report` — research a topic, then deliver a designed report/infographic.
//!
//! Reuses the `investigate` fan-out: N worker agents (one per angle, each recording
//! findings) plus a final **compose** job that reads the findings and emits the
//! `generate_report` spec (the report IR) as JSON. Background jobs can't deliver a
//! file to the channel (they aren't scoped to one), so the compose job only *emits*
//! the spec; the orchestrator — which holds the channel surface — renders it and
//! uploads the file, posting live progress as the chain advances.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use workforce_agent::tools::report::render_report_spec;
use workforce_agent::tools::{Tool, ToolResult};
use workforce_common::channels::{ChannelSurface, OutboundMessage};
use workforce_common::session::TurnPrincipal;

use crate::investigate::{step_emoji, truncate};
use crate::job_runner::{
    AgentJobConfig, AgentProfile, ChainStep, ChainSummary, JobConfig, JobStatus,
};
use crate::serve::AgentService;

const POLL_INTERVAL: Duration = Duration::from_secs(3);
const DELIVERY_DEADLINE: Duration = Duration::from_secs(1800);
const WORKER_TIMEOUT_SECS: u64 = 600;
const COMPOSE_TIMEOUT_SECS: u64 = 600;
/// Max chars of the research fallback posted when the spec can't be built.
const FALLBACK_CHARS: usize = 3000;

const COMPOSE_PROMPT: &str = "You are a report author with read-only access. Use `data.query` to read \
    the investigators' findings, dedup and rank what matters, then design the report. Output ONLY the \
    report specification as a single fenced ```json code block — the JSON object for `generate_report`'s \
    `report` field — and nothing else; do not call any tool to render it. Spec shape: { title, subtitle?, \
    format: \"report\"|\"infographic\", theme?, meta:{kicker?,author?,date?}, sections:[{heading?, \
    blocks:[…]}] }. Block types (each has \"type\"): prose{text}; heading{text}; stats{items:[{value,label,\
    accent? accent|primary|secondary|ink}]}; chart{intent:{kind: bar|line|area|donut, data:[{…}], x?, y?, \
    orientation? vertical|horizontal, value_suffix?}, title?, caption?}; table{columns:[str], rows:[[…]], \
    caption?}; list{title?, items:[{lead?, text}]}. Follow the length budget given in your task EXACTLY \
    — select the most important findings and never pad to fill space; an infographic must fit a single \
    screen. Put real numeric values in chart data. Keep every figure accurate to the findings; invent \
    nothing.";

pub struct ResearchReportTool {
    service: Arc<AgentService>,
    workspace: PathBuf,
}

impl ResearchReportTool {
    pub fn new(service: Arc<AgentService>, workspace: PathBuf) -> Self {
        Self { service, workspace }
    }
}

#[derive(Deserialize)]
struct Input {
    question: String,
    #[serde(default)]
    angles: Vec<String>,
    #[serde(default)]
    depth: Option<String>,
    #[serde(default)]
    theme: Option<String>,
    #[serde(default)]
    format: Option<String>,
    #[serde(default)]
    title: Option<String>,
}

/// `depth` is the single level knob: it sets the number of investigators, how
/// much each one digs (`worker_goal`), and how much reaches the page
/// (`output_budget`) — so one word scales the whole pipeline coherently.
fn max_workers(depth: Option<&str>) -> usize {
    match depth {
        Some("quick") => 3,
        Some("deep") => 8,
        _ => 5,
    }
}

/// Per-worker effort: a target number of findings, so a worker stops at "enough"
/// rather than searching until it exhausts its rounds.
fn worker_goal(depth: Option<&str>) -> &'static str {
    match depth {
        Some("quick") => "Record the 4–6 most important well-sourced facts and figures, then stop — \
             don't keep searching for marginal extras.",
        Some("deep") => "Record about 10–15 well-sourced facts and figures (favour hard numbers with \
             sources), then stop.",
        _ => "Record about 6–10 well-sourced facts and figures, then stop — enough to ground a report, \
             not exhaustive.",
    }
}

/// How much reaches the page. An infographic is capped to a single screen at
/// every level (the level changes rigour/density, not height); a report's
/// section count grows with the level.
fn output_budget(depth: Option<&str>, format: &str) -> &'static str {
    match (format, depth) {
        ("infographic", Some("quick")) => {
            "ONE screen only: a header, 3 headline stats, 1–2 charts, a \
             one-line intro, and ~4 key takeaways. Nothing more."
        }
        ("infographic", Some("deep")) => {
            "ONE screen only (even though the research is deep): a header, \
             4 headline stats, up to 3 charts, a short intro, and up to ~8 takeaways. Favour the \
             best-sourced figures over more blocks — never pad to fill height."
        }
        ("infographic", _) => {
            "ONE screen only: a header, 4 headline stats, 2–3 charts, a short intro, \
             and ~5–7 takeaways."
        }
        (_, Some("quick")) => {
            "A cover and 2 focused sections (each a few sentences plus at most one \
             chart), a headline stats band, and a short takeaways list."
        }
        (_, Some("deep")) => {
            "A cover and 5–6 sections (prose plus charts/tables where they add real \
             value), a headline stats band, and an outlook/takeaways list."
        }
        (_, _) => {
            "A cover and 3–4 sections (each prose plus at most one chart or table), a headline \
             stats band, and a takeaways/outlook list."
        }
    }
}

fn worker_task(question: &str, angle: &str, goal: &str) -> String {
    format!(
        "Research question: {question}\n\nYour assignment — investigate this specific angle: {angle}\n\n\
         {goal} Record findings via `report_findings`, each with its citations and a 0–100 \
         confidence. Finish with a short summary."
    )
}

fn compose_task(
    question: &str,
    angle_count: usize,
    theme: &str,
    format: &str,
    title: &str,
    budget: &str,
) -> String {
    format!(
        "Research question: {question}\n\n{angle_count} investigators recorded findings; read them with \
         `data.query`. Design a {format} titled \"{title}\" in the \"{theme}\" theme.\n\nLength budget — \
         {budget}\n\nOutput its spec as a ```json block exactly as instructed."
    )
}

#[async_trait]
impl Tool for ResearchReportTool {
    fn name(&self) -> &str {
        "research_report"
    }

    fn description(&self) -> &str {
        "Research a topic and deliver a polished, designed report or infographic about it — in one step. \
         It does the research ITSELF (fanning out parallel investigators), then composes their findings \
         into a themed report (cover, headline stats, charts built from the numbers found, sections, \
         takeaways) and posts the rendered PDF/PNG to this conversation. Use this — and ONLY this — for \
         'research X and make me a report/infographic' requests: do NOT also call `investigate` or \
         `generate_report` for the same request; this already covers both. Set `theme` to a brand or \
         built-in theme and `format` to report (PDF) or infographic (PNG)."
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "question": { "type": "string", "description": "The topic / research question for the report." },
                "angles": {
                    "type": "array",
                    "items": { "type": "string" },
                    "description": "Distinct sub-questions to investigate in parallel (one worker each). Defaults to the whole question."
                },
                "depth": { "type": "string", "enum": ["quick", "standard", "deep"], "description": "Level — scales research breadth + per-investigator digging AND how much reaches the page. quick = fast snapshot (3 investigators, tight output); standard = balanced (5); deep = thorough (8, richer multi-section report). Infographics stay one screen at every level. Default standard." },
                "format": { "type": "string", "enum": ["report", "infographic"], "description": "report = multi-page PDF, infographic = single-page PNG. Default report." },
                "theme": { "type": "string", "description": "Theme name — a built-in (espresso|slate|noir|editorial|mint|sunset) or a workspace brand theme. Omit for the workspace default." },
                "title": { "type": "string", "description": "Report title. Defaults to the question." }
            },
            "required": ["question"]
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = || start.elapsed().as_millis() as u64;
        let input: Input = match serde_json::from_value(input) {
            Ok(i) => i,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Invalid research_report input: {e}"),
                    elapsed(),
                ))
            }
        };
        let question = input.question.trim().to_string();
        if question.is_empty() {
            return Ok(ToolResult::failure("Missing 'question'", elapsed()));
        }
        let Some(runner) = self.service.job_runner.clone() else {
            return Ok(ToolResult::failure(
                "Background execution is not available",
                elapsed(),
            ));
        };

        let mut angles = input.angles;
        angles.retain(|a| !a.trim().is_empty());
        if angles.is_empty() {
            angles = vec![question.clone()];
        }
        angles.truncate(max_workers(input.depth.as_deref()));

        let theme = input.theme.unwrap_or_default();
        let format = match input.format.as_deref() {
            Some("infographic") => "infographic",
            _ => "report",
        };
        let title = input
            .title
            .map(|t| t.trim().to_string())
            .filter(|t| !t.is_empty())
            .unwrap_or_else(|| truncate(&question, 80));

        let principal = crate::user_config::current_principal();
        let compose_model = self.compose_model();
        let steps = self.build_chain(
            &question,
            &angles,
            input.depth.as_deref(),
            &theme,
            format,
            &title,
            &principal,
            compose_model,
        );

        let (chain_id, step_jobs) = match runner
            .submit_chain(
                steps,
                self.service.agent_id.as_str().to_string(),
                None,
                None,
                None,
            )
            .await
        {
            Ok(r) => r,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("Failed to start research_report: {e}"),
                    elapsed(),
                ))
            }
        };
        let Some(compose_job_id) = step_jobs
            .iter()
            .find(|(s, _)| s == "compose")
            .map(|(_, j)| j.clone())
        else {
            return Ok(ToolResult::failure(
                "research_report chain is missing its compose step",
                elapsed(),
            ));
        };

        let (channel, thread) = self.service.current_messaging_context().await;

        // No channel (e.g. a gateway session): render inline and hand the file back
        // via the live turn's delivery path.
        let Some(channel) = channel else {
            return Ok(self
                .render_inline(
                    runner.as_ref(),
                    &compose_job_id,
                    &theme,
                    format,
                    &title,
                    elapsed(),
                )
                .await);
        };

        let reporter = Reporter {
            service: Arc::clone(&self.service),
            workspace: self.workspace.clone(),
            runner,
            chain_id: chain_id.clone(),
            compose_job_id,
            channel,
            thread,
            question: question.clone(),
            angles: angles.clone(),
            theme,
            format: format.to_string(),
            title,
        };
        tokio::spawn(reporter.run());

        Ok(ToolResult::success(
            json!({
                "status": "running",
                "chain_id": chain_id,
                "message": format!(
                    "Researching '{}' across {} angles — I'll post progress and deliver the {} here.",
                    question, angles.len(), format
                ),
            }),
            elapsed(),
        ))
    }
}

impl ResearchReportTool {
    fn compose_model(&self) -> Option<String> {
        let tier_key = self.service.tier_key.read().clone();
        let resolved = self
            .service
            .tier_registry()
            .resolve_model("standard", &tier_key);
        (resolved != "standard").then_some(resolved)
    }

    #[allow(clippy::too_many_arguments)]
    fn build_chain(
        &self,
        question: &str,
        angles: &[String],
        depth: Option<&str>,
        theme: &str,
        format: &str,
        title: &str,
        principal: &TurnPrincipal,
        compose_model: Option<String>,
    ) -> Vec<ChainStep> {
        let goal = worker_goal(depth);
        let budget = output_budget(depth, format);
        let mut steps: Vec<ChainStep> = angles
            .iter()
            .enumerate()
            .map(|(i, angle)| ChainStep {
                id: format!("w{i}"),
                config: JobConfig::Agent(AgentJobConfig::new(
                    worker_task(question, angle, goal),
                    AgentProfile::Worker,
                    principal.clone(),
                )),
                depends_on: Vec::new(),
                label: Some(format!("research: {}", truncate(angle, 60))),
                retry: None,
                timeout_secs: Some(WORKER_TIMEOUT_SECS),
                stream: false,
            })
            .collect();

        steps.push(ChainStep {
            id: "compose".to_string(),
            config: JobConfig::Agent(AgentJobConfig {
                model: compose_model,
                system_prompt: Some(COMPOSE_PROMPT.to_string()),
                ..AgentJobConfig::new(
                    compose_task(question, angles.len(), theme, format, title, budget),
                    AgentProfile::Worker,
                    principal.clone(),
                )
            }),
            depends_on: (0..angles.len()).map(|i| format!("w{i}")).collect(),
            label: Some("compose report".to_string()),
            retry: None,
            timeout_secs: Some(COMPOSE_TIMEOUT_SECS),
            stream: false,
        });
        steps
    }

    async fn render_inline(
        &self,
        runner: &dyn crate::hosts::JobHost,
        compose_job_id: &str,
        theme: &str,
        format: &str,
        title: &str,
        elapsed: u64,
    ) -> ToolResult {
        let text = match await_text(runner, compose_job_id).await {
            Ok(t) => t,
            Err(e) => return ToolResult::failure(format!("research_report failed: {e}"), elapsed),
        };
        let Some(spec) = extract_spec(&text, theme, format, title) else {
            return ToolResult::failure(
                "could not extract a report spec from the research",
                elapsed,
            );
        };
        match render_report_spec(&spec, &self.workspace).await {
            Ok(r) => ToolResult::success(
                json!({ "status": "complete", "format": format, "filename": r.filename }),
                elapsed,
            ),
            Err(e) => ToolResult::failure(format!("failed to render the report: {e}"), elapsed),
        }
    }
}

/// Posts live progress, then renders the composed spec and uploads the file.
/// Runs detached after the tool call returns.
struct Reporter {
    service: Arc<AgentService>,
    workspace: PathBuf,
    runner: Arc<dyn crate::hosts::JobHost>,
    chain_id: String,
    compose_job_id: String,
    channel: String,
    thread: Option<String>,
    question: String,
    angles: Vec<String>,
    theme: String,
    format: String,
    title: String,
}

impl Reporter {
    async fn run(self) {
        let posted = self.post(self.progress(None)).await;
        let deadline = Instant::now() + DELIVERY_DEADLINE;
        loop {
            let summary = self.runner.chain_summary(&self.chain_id).await;
            let terminal = summary
                .as_ref()
                .map(|s| s.status.is_terminal())
                .unwrap_or(true);
            self.edit(&posted, self.progress(summary.as_ref())).await;
            if terminal || Instant::now() >= deadline {
                break;
            }
            tokio::time::sleep(POLL_INTERVAL).await;
        }
        self.deliver(&posted).await;
    }

    async fn deliver(&self, posted: &[(Arc<dyn ChannelSurface>, String)]) {
        let text = match await_text(self.runner.as_ref(), &self.compose_job_id).await {
            Ok(t) => t,
            Err(e) => {
                self.post(format!(":x: *Report failed* — {e}")).await;
                return;
            }
        };
        let spec = match extract_spec(&text, &self.theme, &self.format, &self.title) {
            Some(s) => s,
            None => {
                self.post(format!(
                    ":warning: *Couldn't build the formatted report* — here's the research:\n{}",
                    truncate(&text, FALLBACK_CHARS)
                ))
                .await;
                return;
            }
        };
        match render_report_spec(&spec, &self.workspace).await {
            Ok(report) => {
                let caption = format!(":bar_chart: *{}* — ready", self.title);
                for (surface, _) in posted {
                    if let Err(e) = surface
                        .upload_file(
                            &self.channel,
                            &report.filename,
                            &report.bytes,
                            Some(&caption),
                            self.thread.as_deref(),
                        )
                        .await
                    {
                        tracing::warn!(platform = %surface.platform(), error = %e, "Failed to upload research report");
                    }
                }
            }
            Err(e) => {
                self.post(format!(
                    ":warning: *Couldn't render the report* ({e}) — here's the research:\n{}",
                    truncate(&text, FALLBACK_CHARS)
                ))
                .await;
            }
        }
    }

    fn progress(&self, summary: Option<&ChainSummary>) -> String {
        let status_of = |step_id: &str| -> Option<&JobStatus> {
            summary.and_then(|s| {
                s.steps
                    .iter()
                    .find(|st| st.step_id == step_id)
                    .map(|st| &st.status)
            })
        };
        let mut out = format!(
            "📊 *Building {} report:* {}\n",
            self.theme_label(),
            truncate(&self.question, 200)
        );
        for (i, angle) in self.angles.iter().enumerate() {
            out.push_str(&format!(
                "\n- {} {}",
                step_emoji(status_of(&format!("w{i}"))),
                truncate(angle, 80)
            ));
        }
        out.push_str(&format!(
            "\n- {} _composing the report_",
            step_emoji(status_of("compose"))
        ));
        out
    }

    fn theme_label(&self) -> &str {
        if self.theme.is_empty() {
            &self.format
        } else {
            &self.theme
        }
    }

    async fn post(&self, text: String) -> Vec<(Arc<dyn ChannelSurface>, String)> {
        let message = self.message(text);
        let mut posted = Vec::new();
        for surface in self.service.surface_registry.read().await.all() {
            match surface.send_message(&message).await {
                Ok(sent) => posted.push((surface, sent.message_id)),
                Err(e) => {
                    tracing::warn!(platform = %surface.platform(), error = %e, "Failed to post research_report progress")
                }
            }
        }
        posted
    }

    async fn edit(&self, posted: &[(Arc<dyn ChannelSurface>, String)], text: String) {
        for (surface, message_id) in posted {
            if let Err(e) = surface.edit_message(&self.channel, message_id, &text).await {
                tracing::warn!(platform = %surface.platform(), error = %e, "Failed to update research_report progress");
            }
        }
    }

    fn message(&self, text: String) -> OutboundMessage {
        OutboundMessage {
            channel: self.channel.clone(),
            text,
            thread_id: self.thread.clone(),
            username: None,
            icon_url: None,
            blocks: None,
        }
    }
}

/// Poll a job until terminal, returning its text output or the failure reason.
async fn await_text(runner: &dyn crate::hosts::JobHost, job_id: &str) -> Result<String> {
    let deadline = Instant::now() + DELIVERY_DEADLINE;
    loop {
        let res = runner.get_result(job_id, None).await?;
        match res
            .get("status")
            .and_then(Value::as_str)
            .unwrap_or_default()
        {
            "completed" => {
                return Ok(res
                    .pointer("/output/text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string())
            }
            "failed" | "cancelled" | "dependency_failed" => {
                return Err(eyre!(res
                    .get("error")
                    .and_then(Value::as_str)
                    .unwrap_or("research failed")
                    .to_string()))
            }
            _ if Instant::now() >= deadline => return Err(eyre!("research timed out")),
            _ => tokio::time::sleep(POLL_INTERVAL).await,
        }
    }
}

/// Pull the report-spec JSON out of the compose job's text (a fenced ```json
/// block, else the first balanced object), then force the requested
/// theme/format/title so the brand is always applied.
fn extract_spec(text: &str, theme: &str, format: &str, title: &str) -> Option<Value> {
    let raw = fenced_json(text).or_else(|| first_object(text))?;
    let mut spec: Value = serde_json::from_str(&raw).ok()?;
    let obj = spec.as_object_mut()?;
    obj.insert("format".into(), json!(format));
    if !theme.is_empty() {
        obj.insert("theme".into(), json!(theme));
    }
    if obj
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim()
        .is_empty()
    {
        obj.insert("title".into(), json!(title));
    }
    Some(spec)
}

fn fenced_json(text: &str) -> Option<String> {
    let fence = text.find("```json").or_else(|| text.find("```"))?;
    let body_start = fence + text[fence..].find('\n')? + 1;
    let end = text[body_start..].find("```")?;
    Some(text[body_start..body_start + end].trim().to_string())
}

fn first_object(text: &str) -> Option<String> {
    let start = text.find('{')?;
    let mut depth = 0i32;
    for (i, c) in text[start..].char_indices() {
        match c {
            '{' => depth += 1,
            '}' => {
                depth -= 1;
                if depth == 0 {
                    return Some(text[start..start + i + 1].to_string());
                }
            }
            _ => {}
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_spec_from_fenced_block_forces_theme_format_title() {
        let text = "Here is the report:\n```json\n{\"title\": \"\", \"sections\": []}\n```\nDone.";
        let spec = extract_spec(text, "harbor", "infographic", "Q2 Review").unwrap();
        assert_eq!(spec["format"], json!("infographic"));
        assert_eq!(spec["theme"], json!("harbor"));
        assert_eq!(spec["title"], json!("Q2 Review"), "blank title is filled");
    }

    #[test]
    fn extract_spec_keeps_a_provided_title_and_falls_back_to_bare_object() {
        let text = "{\"title\": \"Coffee\", \"sections\": [{\"blocks\": []}]}";
        let spec = extract_spec(text, "", "report", "Default").unwrap();
        assert_eq!(spec["title"], json!("Coffee"), "model title kept");
        assert_eq!(spec["format"], json!("report"));
        assert!(spec.get("theme").is_none(), "empty theme not forced");
    }

    #[test]
    fn extract_spec_returns_none_without_json() {
        assert!(extract_spec("no json here", "", "report", "T").is_none());
    }

    #[test]
    fn first_object_balances_nested_braces() {
        assert_eq!(
            first_object("x {\"a\": {\"b\": 1}} y").as_deref(),
            Some("{\"a\": {\"b\": 1}}")
        );
        assert!(first_object("no braces").is_none());
    }

    #[test]
    fn max_workers_caps_by_depth() {
        assert_eq!(max_workers(Some("quick")), 3);
        assert_eq!(max_workers(Some("deep")), 8);
        assert_eq!(max_workers(None), 5);
    }

    #[test]
    fn level_scales_worker_effort_and_output() {
        assert!(worker_goal(Some("quick")).contains("4–6"));
        assert!(worker_goal(Some("deep")).contains("10–15"));
        assert!(worker_goal(None).contains("6–10"));
        // An infographic stays one screen at EVERY level.
        for d in [Some("quick"), Some("standard"), Some("deep"), None] {
            assert!(
                output_budget(d, "infographic").contains("ONE screen"),
                "{d:?}"
            );
        }
        // The report's section count grows with the level.
        assert!(output_budget(Some("quick"), "report").contains("2 focused sections"));
        assert!(output_budget(Some("deep"), "report").contains("5–6 sections"));
    }
}
