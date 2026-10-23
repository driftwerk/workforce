//! `generate_report` — turn a structured report spec into a beautifully typeset
//! PDF report or single-page PNG infographic and deliver it to the conversation.
//!
//! The agent emits content as an engine-neutral IR ([`ir::Report`]); code owns
//! all layout, theming and chart drawing, so weak models produce valid documents
//! and the agent can never break the design. Rendering goes through the `typst`
//! binary with bundled fonts; the bytes are staged and delivered out-of-band on
//! the same path as `generate_image` / `export_xlsx`, so they never re-enter the
//! model's context.

mod chart;
mod export;
pub(crate) mod fonts;
mod ir;
mod markdown;
mod native;
// `rasterize_svg` is shared with `tools::diagram`, which needs a PNG for chat.
pub(crate) mod render;
mod set_theme;
// Shared with `tools::diagram`, which paints a diagram in the report's own
// chrome rather than inventing a second palette.
pub(crate) mod theme;
mod typst_gen;

pub use export::ExportReportTool;
pub use set_theme::SetReportThemeTool;

use std::borrow::Cow;
use std::path::{Path, PathBuf};
use std::time::Instant;

use async_trait::async_trait;
use eyre::{eyre, Result};
use serde_json::{json, Value};
use workforce_common::settings::{AgentReportConfig, AgentWorkspaceConfig, BrandThemeConfig};

use super::image_gen::GENERATED_IMAGES_KEY;
use super::{Tool, ToolResult};
use crate::artifacts::resolve::{describe_shape, read_session_scoped};
use crate::artifacts::store::{global_store, ATTACHMENT_TTL};
use crate::artifacts::types::{ArtifactBody, ArtifactDescriptor, ArtifactId, ContentType};
use crate::artifacts::ArtifactStore;
use crate::runner::CURRENT_AGENT_ID;
use ir::{Block, Format, Report};
use theme::{Palette, Theme};
use typst_gen::{Images, ResolvedImage};

pub const GENERATE_REPORT_TOOL_NAME: &str = "generate_report";

const DESCRIPTION: &str = "Render a polished, designed document and deliver it to this conversation as a \
    file. `format: \"report\"` produces a multi-page PDF; `format: \"infographic\"` produces a \
    single-page PNG poster. \
    TO TYPESET A DOCUMENT THAT ALREADY EXISTS — a markdown file, or markdown you have written — pass \
    `markdown_path` (a file in the workspace), `markdown_artifact_id` (a staged markdown artifact), or \
    `markdown` (the text itself), and code converts headings, paragraphs, lists, tables and code fences \
    into the report for you. ALWAYS prefer this for \"make a report/PDF from this file\": re-typing the \
    document as a spec loses content and will not fit in one call. Add `patch` alongside it to set the \
    title, theme or meta. \
    Otherwise provide the spec inline as `report`, or by `report_artifact_id` — the `report_spec_id` \
    returned by a previous generate_report/export_report call. You write content as data — sections of \
    typed blocks (prose, heading, stats, chart, table, list, code, image) — never layout: code handles all \
    design, theming and chart drawing. Charts take \
    a small intent (kind bar|line|area|donut + the data rows + which fields are x/y); an unrenderable chart \
    degrades to a table. An image block embeds a staged image artifact (e.g. a generate_image output) by its \
    artifact_id. Themes: espresso, slate, noir (dark), editorial, mint, sunset — plus any brand theme \
    configured for this workspace; omit `theme` to use the workspace default. Emoji in your content \
    render by default; set `emoji: false` on the spec for a strictly formal, emoji-free document (e.g. \
    when the user asks for no emoji). Body text (prose, list items, headings, table cells, captions) may \
    use simple inline Markdown — **bold**, *italic*, `code` — which is styled; other markup is literal. \
    To revise a report you already generated, NEVER re-send the whole spec (you will drift and drop \
    content): pass `report_artifact_id` set to the prior result's `report_spec_id`, plus `patch` for \
    top-level fields (title, theme, meta), `section_edits` to change one section's content while \
    every other section is preserved exactly, and/or `add_sections` to append new sections. \
    Every section must contain at least one block — a heading with no content is rejected. \
    The file is delivered out-of-band: you will NOT see it and \
    its bytes do not re-enter your context, so don't try to read, re-upload or verify it — just tell the \
    user it's ready.";

pub struct GenerateReportTool {
    workspace: PathBuf,
}

impl GenerateReportTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for GenerateReportTool {
    fn name(&self) -> &str {
        GENERATE_REPORT_TOOL_NAME
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "report": {
                    "type": "object",
                    "description": "The report spec (this OR report_artifact_id). Fields: title (required), \
                        subtitle?, format: report|infographic (default report), theme: espresso|slate|noir|\
                        editorial|mint|sunset (default espresso), emoji? (bool, default true — set false to \
                        strip all emoji for a formal look), meta{kicker?,author?,date?}, \
                        sections:[{heading?, blocks:[…]}] — every section needs at least one block. Block \
                        types (each has \"type\"): prose{text}; \
                        heading{text}; code{text, language?} (verbatim, monospaced); \
                        stats{items:[{value,label,accent? accent|primary|secondary|ink}]}; \
                        chart{intent:{kind: bar|line|area|donut, data:[{…}], x?, y?, orientation? \
                        vertical|horizontal, value_suffix?}, title?, caption?}; table{columns:[str], \
                        rows:[[…]], caption?}; list{title?, items:[{lead?, text}]}; image{artifact_id, \
                        width? 1-100, align? left|center|right, caption?}; \
                        diagram{spec, title?, caption?} — a node-and-arrow picture for STRUCTURE (a \
                        process, a decision path, a hand-off), never for quantities (use chart) or \
                        records (use table). `spec` is {kind:\"flow\", direction?:\"down\"|\"right\", \
                        nodes:[{id, label, shape?:\"box\"|\"round\"|\"diamond\"|\"cylinder\", \
                        tone?:\"accent\"|\"good\"|\"warn\"|\"bad\", group?}], edges:[{from, to, label?, \
                        style?:\"dashed\", arrow?:\"both\"|\"none\"}]} or {kind:\"sequence\", \
                        participants:[{id,label}], steps:[{from, to, label?, style?, note?}]}. Layout \
                        and colour are automatic — each `group` gets its own hue, so group whenever a \
                        process crosses more than one team. About 12 nodes, never more than 20."
                },
                "report_artifact_id": {
                    "type": "string",
                    "description": "Alternative to `report`: the `report_spec_id` returned by a previous \
                        generate_report/export_report call, to re-render or (with `patch`/`section_edits`/\
                        `add_sections`) revise that exact spec."
                },
                "markdown_path": {
                    "type": "string",
                    "description": "Path to a markdown file in the agent workspace to typeset. Code converts \
                        headings into sections, and paragraphs, bullet/numbered lists, GFM tables and ``` code \
                        fences into blocks — nothing is summarised or dropped. The first heading becomes the \
                        title (override with `patch`). Use this for \"make a report from this file\"."
                },
                "markdown_artifact_id": {
                    "type": "string",
                    "description": "Alternative to `markdown_path`: an artifact id holding markdown text — \
                        e.g. a staged filesystem.read of a .md file, or an uploaded document."
                },
                "markdown": {
                    "type": "string",
                    "description": "Alternative to `markdown_path`: the markdown text itself. Writing a long \
                        document as markdown is far cheaper and less error-prone than writing the block spec \
                        by hand, so prefer this when you are composing the content yourself."
                },
                "add_sections": {
                    "type": "array",
                    "description": "Append new sections to the spec named by `report_artifact_id` (or a \
                        markdown source), in order, keeping everything already there. Each entry is a full \
                        section object {heading?, blocks:[…]} with at least one block. A heading that already \
                        exists is rejected — use `section_edits` to rewrite that one instead. This is how you \
                        add an appendix or a new chapter without re-sending the report.",
                    "items": { "type": "object" }
                },
                "patch": {
                    "type": "object",
                    "description": "Revise top-level fields of the spec named by `report_artifact_id` without \
                        re-sending it: an RFC 7386 JSON merge patch with ONLY the fields to change, e.g. \
                        {\"meta\": {\"author\": \"Ann\", \"date\": \"17 Jun 2026\"}}, {\"title\": \"New title\"}, \
                        or {\"theme\": \"slate\"}. A null removes a field. Do NOT put `sections` here (it would \
                        drop every section); change section content with `section_edits`. Requires `report_artifact_id`."
                },
                "section_edits": {
                    "type": "object",
                    "description": "Change a SECTION's content in the spec named by `report_artifact_id` while \
                        keeping every other section exactly as-is — the safe way to edit one part of a long \
                        report. Map an existing section's heading (exactly as it appears, case-insensitive) to \
                        its full new section object {heading, blocks:[…]} to replace it, or to null to remove \
                        it. E.g. {\"Overview\": {\"heading\": \"Overview\", \"blocks\": [{\"type\":\"list\", …}]}}. \
                        Only the headings you name change; an unknown heading is rejected with the list of real \
                        headings (it does NOT add a section). Requires `report_artifact_id`."
                },
                "filename": {
                    "type": "string",
                    "description": "Optional output filename; defaults to a slug of the title."
                }
            }
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = |t: Instant| t.elapsed().as_millis() as u64;

        let agent_id = CURRENT_AGENT_ID
            .try_with(|v| v.clone())
            .ok()
            .filter(|s| !s.is_empty());
        let Some(agent_id) = agent_id else {
            return Ok(ToolResult::failure(
                "no agent context in scope; cannot stage the report",
                elapsed(start),
            ));
        };
        let Some(store) = global_store() else {
            return Ok(ToolResult::failure(
                "artifact store is not configured; cannot stage the report",
                elapsed(start),
            ));
        };

        let Resolved { report, spec } =
            match resolve_report(&input, store.as_ref(), &agent_id, &self.workspace) {
                Ok(r) => r,
                Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
            };
        let images = resolve_images(&report, store.as_ref(), &agent_id);
        let rendered = match render_report(&report, &self.workspace, &images).await {
            Ok(r) => r,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        let size = rendered.bytes.len();
        let media_type = rendered.media_type;
        let filename = report_filename(&input, &report.title, report.format);

        let descriptor = ArtifactDescriptor::from_context(
            GENERATE_REPORT_TOOL_NAME,
            ContentType::OctetStream,
            agent_id.clone(),
        );
        let handle = match store.write(&agent_id, descriptor, ArtifactBody::Bytes(rendered.bytes)) {
            Ok(h) => h,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("failed to stage the report for delivery: {e}"),
                    elapsed(start),
                ))
            }
        };

        let mut output = json!({
            GENERATED_IMAGES_KEY: [ {
                "artifact_id": handle.id.as_str(),
                "media_type": media_type,
                "filename": filename,
                "bytes": size,
            } ],
            "filename": filename,
            "note": "Report generated and being delivered to this conversation as a downloadable file \
                     automatically — it happens outside your context, so you will NOT see it. This is \
                     expected, NOT a failure: do not try to read, re-upload, or verify it. To revise this \
                     report later, NEVER re-send the whole spec — call generate_report again with \
                     `report_artifact_id` set to the `report_spec_id` below, plus `patch` for top-level \
                     fields (e.g. {\"meta\": {\"author\": \"Kingsley Hendrickse\"}}) and/or `section_edits` \
                     to rewrite one section by heading (e.g. {\"Overview\": {\"heading\": \"Overview\", \
                     \"blocks\": [...]}}). Every section you don't name is preserved exactly. Just tell the \
                     user it's ready.",
        });
        attach_spec_id(
            &mut output,
            store.as_ref(),
            &agent_id,
            GENERATE_REPORT_TOOL_NAME,
            &spec,
        );
        Ok(ToolResult::success(output, elapsed(start)))
    }
}

/// A resolved report: the parsed IR plus its canonical (post-patch) spec JSON.
/// The caller persists the spec so a later call can revise it via
/// `report_artifact_id` + `patch`.
#[derive(Debug)]
struct Resolved {
    report: Report,
    spec: Value,
}

fn resolve_report(
    input: &Value,
    store: &dyn ArtifactStore,
    agent_id: &str,
    workspace: &Path,
) -> Result<Resolved> {
    let patch = input.get("patch").filter(|v| !v.is_null());
    let section_edits = input.get("section_edits").filter(|v| !v.is_null());
    let add_sections = input.get("add_sections").filter(|v| !v.is_null());
    let revisions = [patch, section_edits, add_sections]
        .iter()
        .any(Option::is_some);
    let mut spec = match spec_source(input)? {
        SpecSource::Inline(report) => {
            if revisions {
                return Err(eyre!(
                    "`patch`/`section_edits`/`add_sections` revise a spec you did not type out in \
                     this call — pass the previous `report_spec_id` as `report_artifact_id`, or a \
                     markdown source, not an inline `report`"
                ));
            }
            report.clone()
        }
        SpecSource::Artifact(id) => {
            let id = ArtifactId::parse(id)?;
            let (body, _) = read_session_scoped(store, agent_id, &id)?;
            spec_from_artifact_body(&body)
                .map_err(|e| eyre!("artifact {id} does not hold a report spec: {e}"))?
        }
        SpecSource::Markdown(text) => markdown::to_spec(text, "Report"),
        SpecSource::MarkdownPath(path) => {
            let (text, stem) = read_markdown_file(path, workspace)?;
            markdown::to_spec(&text, &stem)
        }
        SpecSource::MarkdownArtifact(id) => {
            let id = ArtifactId::parse(id)?;
            let (body, descriptor) = read_session_scoped(store, agent_id, &id)?;
            let title = descriptor
                .as_ref()
                .and_then(|d| d.filename.as_deref())
                .map_or_else(|| "Report".to_string(), |f| file_stem_title(Path::new(f)));
            markdown::to_spec(&markdown_from_artifact_body(&body)?, &title)
        }
    };
    if let Some(patch) = patch {
        if patch.get("sections").is_some() {
            return Err(eyre!(
                "don't put `sections` in `patch` — a merge patch replaces the whole array, \
                 dropping every section you didn't include. Use `section_edits` (keyed by \
                 heading) to change a section while keeping the rest."
            ));
        }
        merge_patch(&mut spec, patch);
    }
    if let Some(edits) = section_edits {
        let edits = edits.as_object().ok_or_else(|| {
            eyre!(
                "`section_edits` must be a JSON object mapping a section heading to its new \
                   section (or null to remove it)"
            )
        })?;
        apply_section_edits(&mut spec, edits)?;
    }
    // After `section_edits`, so one call can both rewrite and extend, and the
    // duplicate check sees the post-edit headings.
    if let Some(added) = add_sections {
        let added = added.as_array().ok_or_else(|| {
            eyre!("`add_sections` must be a JSON array of section objects {{heading?, blocks:[…]}}")
        })?;
        apply_add_sections(&mut spec, added)?;
    }
    let report = parse_spec(&spec)?;
    Ok(Resolved { report, spec })
}

/// Where a call's spec comes from. Exactly one, so the tool never has to guess
/// which of two conflicting inputs the caller meant.
enum SpecSource<'a> {
    Inline(&'a Value),
    Artifact(&'a str),
    Markdown(&'a str),
    MarkdownPath(&'a str),
    MarkdownArtifact(&'a str),
}

fn spec_source(input: &Value) -> Result<SpecSource<'_>> {
    let text = |key: &str| input[key].as_str().map(str::trim).filter(|s| !s.is_empty());
    let mut found: Vec<(&'static str, SpecSource<'_>)> = [
        input
            .get("report")
            .filter(|v| !v.is_null())
            .map(|r| ("report", SpecSource::Inline(r))),
        text("report_artifact_id").map(|v| ("report_artifact_id", SpecSource::Artifact(v))),
        text("markdown").map(|v| ("markdown", SpecSource::Markdown(v))),
        text("markdown_path").map(|v| ("markdown_path", SpecSource::MarkdownPath(v))),
        text("markdown_artifact_id")
            .map(|v| ("markdown_artifact_id", SpecSource::MarkdownArtifact(v))),
    ]
    .into_iter()
    .flatten()
    .collect();
    match found.len() {
        1 => Ok(found.remove(0).1),
        0 => Err(eyre!(
            "must specify one spec source: `report` (the spec object), `report_artifact_id` (a \
             previous call's `report_spec_id`), or a markdown source — `markdown`, \
             `markdown_path`, or `markdown_artifact_id`"
        )),
        _ => Err(eyre!(
            "specify exactly ONE spec source, not {:?}",
            found.iter().map(|(k, _)| *k).collect::<Vec<_>>()
        )),
    }
}

/// Read a markdown file from inside the agent workspace, returning its text and a
/// title derived from the filename. Containment is the read tools' own check, and
/// the workspace is canonicalized first: `resolve_within` canonicalizes the
/// candidate path, so an un-canonicalized root reached through a symlink would
/// compare unequal and refuse every legitimate path.
fn read_markdown_file(path: &str, workspace: &Path) -> Result<(String, String)> {
    let candidate = Path::new(path);
    let outside = || eyre!("`markdown_path` {path:?} is outside the agent workspace");
    // `resolve_within` canonicalizes to enforce containment, which silently
    // degrades to a lexical prefix test when the target does not exist — and
    // `<workspace>/../x` passes that test. Refuse `..` outright first.
    if candidate
        .components()
        .any(|c| c == std::path::Component::ParentDir)
    {
        return Err(outside());
    }
    let root = workspace
        .canonicalize()
        .unwrap_or_else(|_| workspace.to_path_buf());
    let resolved =
        crate::tools::filesystem::resolve_within(candidate, &[root]).ok_or_else(outside)?;
    let text = std::fs::read_to_string(&resolved)
        .map_err(|e| eyre!("could not read `markdown_path` {path:?}: {e}"))?;
    Ok((text, file_stem_title(&resolved)))
}

/// A human title from a filename: `tech-stack-overview.md` → `Tech Stack Overview`.
/// A stem with nothing wordlike in it (`.md`) yields no title at all.
fn file_stem_title(path: &Path) -> String {
    let stem = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or_default()
        .replace(['-', '_'], " ");
    let title = stem
        .split_whitespace()
        .filter(|word| word.chars().any(char::is_alphanumeric))
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                Some(first) => first.to_uppercase().chain(chars).collect::<String>(),
                None => String::new(),
            }
        })
        .collect::<Vec<_>>()
        .join(" ");
    if title.is_empty() {
        "Report".to_string()
    } else {
        title
    }
}

/// Pull a report spec out of an artifact body, tolerating one layer of
/// tool-output envelope.
///
/// The spec the caller means is often the *payload* of a staged tool result: a
/// spill artifact's body is `{"content": "<the spec text>", …}` from
/// `filesystem.read`, or the same shape around `exec.run` stdout. Rejecting those
/// outright is what sent a model round a five-call loop writing Python to chunk a
/// file, so unwrap them — the tool description still steers callers to
/// `report_spec_id`.
fn spec_from_artifact_body(body: &ArtifactBody) -> Result<Value> {
    let json = match body {
        ArtifactBody::Json(value) => Some(value.clone()),
        ArtifactBody::Bytes(bytes) => std::str::from_utf8(bytes)
            .ok()
            .and_then(|s| serde_json::from_str::<Value>(s).ok()),
        _ => None,
    };
    json.as_ref().and_then(unwrap_spec).ok_or_else(|| {
        eyre!(
            "it holds {} — pass the `report_spec_id` from a previous generate_report/export_report \
             result, not an arbitrary staged tool output. If that artifact holds a markdown \
             document rather than a spec, pass it as `markdown_artifact_id` instead",
            describe_shape(body)
        )
    })
}

/// Field names a staged tool result puts its payload under: `filesystem.read`
/// stages `{"content": …}`, `exec.run` `{"stdout": …}`, and so on.
const ENVELOPE_KEYS: [&str; 5] = ["content", "stdout", "text", "output", "result"];

/// The payload values of a tool-output envelope, in preference order. Empty for
/// anything that is not an object.
fn envelope_payloads(value: &Value) -> impl Iterator<Item = &Value> {
    let map = value.as_object();
    ENVELOPE_KEYS
        .into_iter()
        .filter_map(move |key| map?.get(key))
}

/// A spec, or the spec nested one level inside a tool-output envelope. A spec that
/// arrives as JSON *text* (the common case, since a staged payload is a string) is
/// parsed at either level.
fn unwrap_spec(value: &Value) -> Option<Value> {
    let as_spec = |v: &Value| match v {
        Value::String(s) => serde_json::from_str::<Value>(s)
            .ok()
            .filter(looks_like_spec),
        other => looks_like_spec(other).then(|| other.clone()),
    };
    as_spec(value).or_else(|| envelope_payloads(value).find_map(as_spec))
}

/// Markdown text out of an artifact body, tolerating the same tool-output
/// envelope as [`spec_from_artifact_body`] — a staged `filesystem.read` of a `.md`
/// file is the shape a model reaches for most often.
fn markdown_from_artifact_body(body: &ArtifactBody) -> Result<String> {
    let text = match body {
        ArtifactBody::Bytes(bytes) => String::from_utf8(bytes.clone()).ok(),
        ArtifactBody::Json(Value::String(s)) => Some(s.clone()),
        ArtifactBody::Json(value) => {
            envelope_payloads(value).find_map(|v| v.as_str().map(str::to_string))
        }
        _ => None,
    };
    text.filter(|t| !t.trim().is_empty()).ok_or_else(|| {
        eyre!(
            "`markdown_artifact_id` artifact holds no markdown text — it holds {}",
            describe_shape(body)
        )
    })
}

fn looks_like_spec(value: &Value) -> bool {
    value
        .as_object()
        .is_some_and(|m| m.contains_key("title") || m.contains_key("sections"))
}

/// Append sections to the spec's `sections`, in order. A heading that already
/// exists is refused rather than duplicated — the mirror of
/// [`apply_section_edits`]'s unknown-heading rejection, so an append can never
/// silently produce two sections with the same title.
fn apply_add_sections(spec: &mut Value, added: &[Value]) -> Result<()> {
    let sections = spec
        .get_mut("sections")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| eyre!("the stored report has no `sections` array to append to"))?;
    for section in added {
        if let Some(heading) = section.get("heading").and_then(Value::as_str) {
            if sections.iter().any(|s| section_heading_matches(s, heading)) {
                return Err(eyre!(
                    "a section titled {heading:?} already exists — use `section_edits` to rewrite \
                     it, or pick a different heading for the new section"
                ));
            }
        }
        sections.push(section.clone());
    }
    Ok(())
}

/// Apply heading-keyed section edits to the stored spec's `sections`, in place:
/// each entry replaces the section whose heading matches (case-insensitive) the
/// key, or `null` removes it — so every section the caller didn't name is
/// preserved byte-identical. A heading that matches nothing is an error listing
/// the real headings (so a model that drifts on a section title self-corrects
/// instead of silently appending a duplicate). The surgical alternative to a
/// merge `patch`, which would replace the whole `sections` array.
fn apply_section_edits(spec: &mut Value, edits: &serde_json::Map<String, Value>) -> Result<()> {
    let sections = spec
        .get_mut("sections")
        .and_then(Value::as_array_mut)
        .ok_or_else(|| eyre!("the stored report has no `sections` array to edit"))?;
    for (heading, value) in edits {
        let pos = sections
            .iter()
            .position(|s| section_heading_matches(s, heading));
        match (value, pos) {
            (Value::Null, Some(i)) => {
                sections.remove(i);
            }
            (Value::Null, None) => {}
            (section, Some(i)) => sections[i] = section.clone(),
            (_, None) => {
                let available: Vec<&str> = sections
                    .iter()
                    .filter_map(|s| s.get("heading").and_then(Value::as_str))
                    .collect();
                return Err(eyre!(
                    "no section titled {heading:?} to edit — the report's sections are {available:?}; \
                     match one of those headings exactly (case-insensitive)"
                ));
            }
        }
    }
    Ok(())
}

fn section_heading_matches(section: &Value, heading: &str) -> bool {
    section
        .get("heading")
        .and_then(Value::as_str)
        .is_some_and(|h| h.trim().eq_ignore_ascii_case(heading.trim()))
}

/// Apply an RFC 7386 JSON Merge Patch to `target` in place: object keys recurse,
/// a `null` value removes that key, and any other value replaces wholesale (so
/// arrays like `sections` are replaced, not merged). Lets a revise call change
/// just the fields it names (title, theme, `meta.author`…) and leave everything
/// else byte-identical.
fn merge_patch(target: &mut Value, patch: &Value) {
    let Value::Object(fields) = patch else {
        *target = patch.clone();
        return;
    };
    if !target.is_object() {
        *target = Value::Object(serde_json::Map::new());
    }
    let target_fields = target
        .as_object_mut()
        .expect("target set to an object above");
    for (key, value) in fields {
        if value.is_null() {
            target_fields.remove(key);
        } else {
            merge_patch(
                target_fields.entry(key.clone()).or_insert(Value::Null),
                value,
            );
        }
    }
}

/// Persist the canonical report spec as a 24h JSON artifact and, on success,
/// record its id on the tool output as `report_spec_id` so a later call can
/// revise it via `report_artifact_id` + `patch`. Best-effort: a staging failure
/// simply omits the id and the rendered document is unaffected.
fn attach_spec_id(
    output: &mut Value,
    store: &dyn ArtifactStore,
    agent_id: &str,
    tool: &str,
    spec: &Value,
) {
    let descriptor =
        ArtifactDescriptor::from_context(tool, ContentType::ApplicationJson, agent_id.to_string())
            .with_ttl(ATTACHMENT_TTL);
    if let Ok(handle) = store.write(agent_id, descriptor, ArtifactBody::Json(spec.clone())) {
        output["report_spec_id"] = json!(handle.id.as_str());
    }
}

/// A rendered report: bytes + a sensible filename + MIME. Shared return type for
/// the `generate_report` tool (which stages it) and the `research_report`
/// orchestrator (which uploads it to the channel).
pub struct RenderedReport {
    pub bytes: Vec<u8>,
    pub filename: String,
    pub media_type: &'static str,
}

/// Parse + validate a report spec (the IR JSON).
fn parse_spec(spec: &Value) -> Result<Report> {
    let report: Report =
        serde_json::from_value(spec.clone()).map_err(|e| eyre!("invalid report spec: {e}"))?;
    report.validate().map_err(|e| eyre!("{e}"))?;
    Ok(report)
}

/// Whether emoji render in this report: an explicit per-report `emoji` flag
/// wins, else the workspace `[report].emoji` default, else on.
fn emoji_enabled(report: &Report, cfg: Option<&AgentReportConfig>) -> bool {
    report
        .emoji
        .or_else(|| cfg.and_then(|c| c.emoji))
        .unwrap_or(true)
}

/// Resolve the workspace's theme + logo and render a report to bytes. The render
/// core shared by the tool and the orchestrator.
async fn render_report(
    report: &Report,
    workspace: &Path,
    images: &Images,
) -> Result<RenderedReport> {
    let (report, theme, logo_path) = prepare(report, workspace);
    let logo = logo_path.and_then(|p| load_logo(workspace, &p));
    let document = typst_gen::build(&report, &theme, images, logo.as_ref());
    let bytes = render::render(&document, report.format).await?;
    Ok(RenderedReport {
        filename: slug_filename(&report.title, report.format),
        media_type: render::media_type(report.format),
        bytes,
    })
}

/// Resolve the workspace report config, apply the emoji policy, and pick the
/// theme (+ optional logo path) — the shared front half of every render path.
fn prepare<'a>(report: &'a Report, workspace: &Path) -> (Cow<'a, Report>, Theme, Option<String>) {
    let report_cfg = AgentWorkspaceConfig::load(workspace)
        .ok()
        .flatten()
        .and_then(|w| w.report);
    // Normalise every render: convert Slack `:shortcode:` emoji to Unicode and
    // drop U+FFFD junk. Emoji render by default (bundled "Noto Emoji" fallback);
    // when off, strip them so the document is strictly formal.
    let mut r = report.clone();
    r.normalize_text();
    if !emoji_enabled(&r, report_cfg.as_ref()) {
        r.strip_emoji();
    }
    let report: Cow<'_, Report> = Cow::Owned(r);
    let (theme, logo_path) = resolve_theme(report.theme.as_deref(), report_cfg.as_ref());
    (report, theme, logo_path)
}

/// Lower a report to editable native-document bytes (`.docx`/`.pptx`), resolving
/// theme + emoji exactly as the PDF path does so the output stays on-brand.
pub(crate) async fn render_native(
    report: &Report,
    workspace: &Path,
    images: &Images,
    format: native::NativeFormat,
) -> Result<Vec<u8>> {
    let (report, theme, _logo) = prepare(report, workspace);
    native::render(&report, images, &theme, format).await
}

/// Parse a report spec and render it (no image-block artifacts). The entry point
/// the `research_report` orchestrator calls to turn a composed spec into a file.
///
/// Empty sections are pruned rather than rejected here. `validate`'s strict rule
/// exists so a *tool* caller is told to fill them in and retries; this path has no
/// caller left to retry — the composing job has already finished — so failing it
/// would discard every good section and degrade the whole report to a text dump.
/// A spec that is hollow all the way through still prunes to nothing and trips the
/// `no sections` rule, landing on that same fallback, which is correct.
pub async fn render_report_spec(spec: &Value, workspace: &Path) -> Result<RenderedReport> {
    let report = parse_spec(&prune_empty_sections(spec))?;
    render_report(&report, workspace, &Images::new()).await
}

/// Drop sections that carry no blocks, leaving everything else byte-identical.
fn prune_empty_sections(spec: &Value) -> Cow<'_, Value> {
    let is_empty = |s: &Value| {
        s.get("blocks")
            .and_then(Value::as_array)
            .is_none_or(Vec::is_empty)
    };
    let sections = spec.get("sections").and_then(Value::as_array);
    if !sections.is_some_and(|all| all.iter().any(is_empty)) {
        return Cow::Borrowed(spec);
    }
    let mut pruned = spec.clone();
    pruned["sections"] = sections
        .expect("checked above")
        .iter()
        .filter(|s| !is_empty(s))
        .cloned()
        .collect();
    Cow::Owned(pruned)
}

/// The theme a standalone render (a chart) draws in: `requested` by name —
/// a workspace brand theme or a built-in — else the workspace default.
pub(crate) fn workspace_theme(workspace: &Path, requested: Option<&str>) -> Theme {
    let cfg = AgentWorkspaceConfig::load(workspace)
        .ok()
        .flatten()
        .and_then(|w| w.report);
    resolve_theme(requested, cfg.as_ref()).0
}

/// Pick the theme (and its optional logo path) for a render: the requested name,
/// else the workspace `default_theme`. A configured brand theme wins over a
/// built-in of the same name; unknown names fall back to the default built-in.
fn resolve_theme(
    requested: Option<&str>,
    cfg: Option<&AgentReportConfig>,
) -> (Theme, Option<String>) {
    let name = requested.or_else(|| cfg.and_then(|c| c.default_theme.as_deref()));
    if let (Some(name), Some(cfg)) = (name, cfg) {
        if let Some(brand) = brand_lookup(cfg, name) {
            return (brand_theme(name, brand), brand.logo.clone());
        }
    }
    (theme::resolve(name).clone(), None)
}

fn brand_lookup<'a>(cfg: &'a AgentReportConfig, name: &str) -> Option<&'a BrandThemeConfig> {
    cfg.themes
        .iter()
        .find(|(k, _)| k.eq_ignore_ascii_case(name))
        .map(|(_, v)| v)
}

/// Build a brand theme by inheriting a built-in `base` and applying the
/// configured colour/font overrides; any unset field keeps the base value.
fn brand_theme(name: &str, b: &BrandThemeConfig) -> Theme {
    let base = b
        .base
        .as_deref()
        .and_then(theme::builtin)
        .cloned()
        .unwrap_or_else(|| theme::default_theme().clone());
    let bp = base.palette;
    Theme {
        name: name.to_string(),
        sans: b.sans.clone().unwrap_or(base.sans),
        serif: b.serif.clone().unwrap_or(base.serif),
        palette: Palette {
            cover_bg: b.cover_bg.clone().unwrap_or(bp.cover_bg),
            primary: b.primary.clone().unwrap_or(bp.primary),
            secondary: b.secondary.clone().unwrap_or(bp.secondary),
            accent: b.accent.clone().unwrap_or(bp.accent),
            ink: b.ink.clone().unwrap_or(bp.ink),
            paper: b.paper.clone().unwrap_or(bp.paper),
            panel: b.panel.clone().unwrap_or(bp.panel),
            muted: b.muted.clone().unwrap_or(bp.muted),
            hairline: b.hairline.clone().unwrap_or(bp.hairline),
            on_dark: b.on_dark.clone().unwrap_or(bp.on_dark),
            series: b.series.clone().unwrap_or(bp.series),
        },
    }
}

/// Read a brand logo from a workspace-relative path, refusing to escape the
/// workspace, and only accepting a recognised image format.
fn load_logo(workspace: &Path, rel: &str) -> Option<ResolvedImage> {
    let path = workspace.join(rel).canonicalize().ok()?;
    if !path.starts_with(workspace.canonicalize().ok()?) {
        return None;
    }
    let bytes = std::fs::read(&path).ok()?;
    let ext = image_ext(&bytes)?;
    Some(ResolvedImage { ext, bytes })
}

/// Resolve every `image` block's artifact to bytes + format, keyed by id, so the
/// pure `typst_gen::build` can embed them. Unresolvable artifacts (missing,
/// expired, cross-session, or not an image) are simply omitted — the block then
/// degrades to a placeholder.
fn resolve_images(report: &Report, store: &dyn ArtifactStore, agent_id: &str) -> Images {
    report
        .sections
        .iter()
        .flat_map(|s| &s.blocks)
        .filter_map(|b| match b {
            Block::Image { artifact_id, .. } => Some(artifact_id.as_str()),
            _ => None,
        })
        .filter_map(|id| Some((id.to_string(), resolve_image(store, agent_id, id)?)))
        .collect()
}

fn resolve_image(store: &dyn ArtifactStore, agent_id: &str, id: &str) -> Option<ResolvedImage> {
    let id = ArtifactId::parse(id).ok()?;
    let (body, _) = read_session_scoped(store, agent_id, &id).ok()?;
    let ArtifactBody::Bytes(bytes) = body else {
        return None;
    };
    let ext = image_ext(&bytes)?;
    Some(ResolvedImage { ext, bytes })
}

fn image_ext(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\x89PNG") {
        Some("png")
    } else if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some("jpg")
    } else if bytes.starts_with(b"GIF8") {
        Some("gif")
    } else if is_svg(bytes) {
        Some("svg")
    } else {
        None
    }
}

fn is_svg(bytes: &[u8]) -> bool {
    let head = String::from_utf8_lossy(&bytes[..bytes.len().min(512)]);
    let trimmed = head.trim_start();
    trimmed.starts_with("<?xml") || trimmed.starts_with("<svg") || trimmed.contains("<svg")
}

fn report_filename(input: &Value, title: &str, format: Format) -> String {
    match input["filename"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(name) => ensure_extension(name, render::extension(format)),
        None => slug_filename(title, format),
    }
}

fn slug_filename(title: &str, format: Format) -> String {
    slug_with_ext(title, render::extension(format))
}

fn slug_with_ext(title: &str, ext: &str) -> String {
    let slug = slugify(title);
    let stem = if slug.is_empty() { "report" } else { &slug };
    format!("{stem}.{ext}")
}

/// Filename for a native export: an explicit `filename` (extension forced to
/// match the format) else a slug of the title.
fn native_filename(input: &Value, title: &str, ext: &str) -> String {
    match input["filename"]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        Some(name) => ensure_extension(name, ext),
        None => slug_with_ext(title, ext),
    }
}

fn ensure_extension(name: &str, ext: &str) -> String {
    if name
        .rsplit_once('.')
        .map(|(_, e)| e.eq_ignore_ascii_case(ext))
        == Some(true)
    {
        name.to_string()
    } else {
        format!("{name}.{ext}")
    }
}

fn slugify(title: &str) -> String {
    title
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() {
                c.to_ascii_lowercase()
            } else {
                '-'
            }
        })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
        .chars()
        .take(60)
        .collect()
}

/// Render a report's diagram block to SVG in the report's own chrome. Lives
/// here so both the Typst and native lowerings reach it the same way.
pub(crate) fn diagram_svg(spec: &Value, theme: &Theme) -> Result<String, String> {
    let parsed = crate::tools::diagram::spec::validate(spec)?;
    let laid_out =
        crate::tools::diagram::layout::layout(&parsed, crate::tools::diagram::svg::SERVER_METRICS);
    Ok(crate::tools::diagram::svg::render(
        &parsed, &laid_out, theme,
    ))
}

#[cfg(test)]
mod vocabulary_tests {
    use super::*;

    /// The schema description IS the model's vocabulary: a block type the tool
    /// never names is a block type no agent will ever emit, however well it is
    /// wired underneath. This caught `diagram` shipping unreachable.
    #[test]
    fn the_schema_names_every_block_type() {
        let tool = GenerateReportTool::new(std::path::PathBuf::from("."));
        let schema = tool.input_schema();
        let description = schema["properties"]["report"]["description"]
            .as_str()
            .expect("report description");
        for block in [
            "prose", "heading", "code", "stats", "chart", "diagram", "table", "list", "image",
        ] {
            assert!(
                description.contains(&format!("{block}{{")),
                "the report schema never mentions the `{block}` block, so no model can emit one"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::store::FsArtifactStore;
    use serde_json::json;
    use tempfile::TempDir;

    fn store() -> (std::sync::Arc<FsArtifactStore>, TempDir) {
        let dir = TempDir::new().unwrap();
        (FsArtifactStore::new(dir.path().to_path_buf()).unwrap(), dir)
    }

    fn minimal_spec() -> Value {
        json!({ "title": "T", "sections": [{ "blocks": [{ "type": "prose", "text": "x" }] }] })
    }

    /// `resolve_report` against a workspace no test needs to reach — the
    /// `markdown_path` tests pass a real one explicitly.
    fn resolve(input: &Value, store: &dyn ArtifactStore, agent_id: &str) -> Result<Resolved> {
        resolve_report(input, store, agent_id, Path::new("."))
    }

    fn write_json(store: &FsArtifactStore, agent: &str, body: Value) -> ArtifactId {
        store
            .write(
                agent,
                ArtifactDescriptor::from_context("t", ContentType::ApplicationJson, agent),
                ArtifactBody::Json(body),
            )
            .unwrap()
            .id
    }

    const TINY_SVG: &[u8] = b"<svg xmlns='http://www.w3.org/2000/svg' viewBox='0 0 4 4'></svg>";

    #[test]
    fn image_ext_sniffs_supported_formats() {
        assert_eq!(image_ext(b"\x89PNG\r\n\x1a\n----"), Some("png"));
        assert_eq!(image_ext(&[0xFF, 0xD8, 0xFF, 0xE0]), Some("jpg"));
        assert_eq!(image_ext(b"GIF89a----"), Some("gif"));
        assert_eq!(image_ext(b"  <svg width='1'></svg>"), Some("svg"));
        assert_eq!(image_ext(b"not an image"), None);
    }

    #[test]
    fn resolve_images_loads_image_artifacts_and_skips_the_rest() {
        let (s, _g) = store();
        let svg_id = s
            .write(
                "agent-a",
                ArtifactDescriptor::from_context("t", ContentType::OctetStream, "agent-a"),
                ArtifactBody::Bytes(TINY_SVG.to_vec()),
            )
            .unwrap()
            .id;
        let json_id = s
            .write(
                "agent-a",
                ArtifactDescriptor::from_context("t", ContentType::ApplicationJson, "agent-a"),
                ArtifactBody::Json(json!({ "x": 1 })),
            )
            .unwrap()
            .id;
        let report: Report = serde_json::from_value(json!({
            "title": "T",
            "sections": [{ "blocks": [
                { "type": "image", "artifact_id": svg_id.as_str() },
                { "type": "image", "artifact_id": json_id.as_str() },
                { "type": "image", "artifact_id": "art_00000000000000000000000000000000" }
            ] }]
        }))
        .unwrap();
        let images = resolve_images(&report, s.as_ref(), "agent-a");
        assert_eq!(images.len(), 1, "only the real image resolves");
        assert_eq!(images.get(svg_id.as_str()).unwrap().ext, "svg");
    }

    #[test]
    fn resolve_report_one_of_validation() {
        let (s, _g) = store();
        let both = json!({ "report": minimal_spec(), "report_artifact_id": "art_00000000000000000000000000000000" });
        let err = resolve(&both, s.as_ref(), "a").unwrap_err().to_string();
        assert!(
            err.contains("exactly ONE") && err.contains("report_artifact_id"),
            "names the competing sources: {err}"
        );
        assert!(resolve(&json!({}), s.as_ref(), "a")
            .unwrap_err()
            .to_string()
            .contains("must specify"));
    }

    #[test]
    fn every_pair_of_spec_sources_is_rejected() {
        let (s, _g) = store();
        let sources = [
            ("report", minimal_spec()),
            (
                "report_artifact_id",
                json!("art_0000000000000000000000000000000a"),
            ),
            ("markdown", json!("# T\n\nbody\n")),
            ("markdown_path", json!("doc.md")),
            (
                "markdown_artifact_id",
                json!("art_0000000000000000000000000000000b"),
            ),
        ];
        for (i, (a, av)) in sources.iter().enumerate() {
            for (b, bv) in sources.iter().skip(i + 1) {
                let input = json!({ *a: av.clone(), *b: bv.clone() });
                let err = resolve(&input, s.as_ref(), "a").unwrap_err().to_string();
                assert!(err.contains("exactly ONE"), "{a} + {b} allowed: {err}");
            }
        }
    }

    #[test]
    fn resolve_report_parses_inline_and_reports_invalid_spec() {
        let (s, _g) = store();
        let ok = resolve(&json!({ "report": minimal_spec() }), s.as_ref(), "a").unwrap();
        assert_eq!(ok.report.title, "T");
        let bad = resolve(&json!({ "report": { "title": "T" } }), s.as_ref(), "a")
            .unwrap_err()
            .to_string();
        assert!(bad.contains("no sections"), "got: {bad}");
    }

    #[test]
    fn a_markdown_source_converts_and_accepts_a_patch() {
        let (s, _g) = store();
        let resolved = resolve(
            &json!({
                "markdown": "# Doc Title\n\n## One\n\nalpha\n",
                "patch": { "theme": "slate", "meta": { "author": "Ann" } }
            }),
            s.as_ref(),
            "a",
        )
        .unwrap();
        assert_eq!(resolved.report.title, "Doc Title");
        assert_eq!(resolved.report.theme.as_deref(), Some("slate"));
        assert_eq!(resolved.report.meta.author.as_deref(), Some("Ann"));
        assert_eq!(resolved.report.sections.len(), 1);
    }

    #[test]
    fn markdown_path_reads_inside_the_workspace_and_refuses_escape() {
        let (s, _g) = store();
        // TempDir on macOS is a symlink (/var → /private/var); the workspace has
        // to be canonicalized or every legitimate path is denied.
        let ws = TempDir::new().unwrap();
        std::fs::write(
            ws.path().join("tech-stack-overview.md"),
            "## One\n\nalpha\n\n## Two\n\nbeta\n",
        )
        .unwrap();
        let resolved = resolve_report(
            &json!({ "markdown_path": "tech-stack-overview.md" }),
            s.as_ref(),
            "a",
            ws.path(),
        )
        .unwrap();
        assert_eq!(
            resolved.report.title, "Tech Stack Overview",
            "no leading heading, so the filename supplies the title"
        );
        assert_eq!(resolved.report.sections.len(), 2);

        // An existing file outside the workspace, and a traversal to one that does
        // not exist — canonicalization catches the first, the `..` check the second.
        let outside = TempDir::new().unwrap();
        std::fs::write(outside.path().join("secret.md"), "# Secret\n\nx\n").unwrap();
        for escape in [
            outside.path().join("secret.md").display().to_string(),
            "../outside.md".to_string(),
            "sub/../../outside.md".to_string(),
        ] {
            let err = resolve_report(
                &json!({ "markdown_path": escape }),
                s.as_ref(),
                "a",
                ws.path(),
            )
            .unwrap_err()
            .to_string();
            assert!(
                err.contains("outside the agent workspace"),
                "{escape} allowed: {err}"
            );
        }

        let missing = resolve_report(
            &json!({ "markdown_path": "nope.md" }),
            s.as_ref(),
            "a",
            ws.path(),
        )
        .unwrap_err()
        .to_string();
        assert!(missing.contains("could not read"), "got: {missing}");
    }

    #[test]
    fn markdown_artifact_reads_bytes_json_and_envelope_bodies() {
        let (s, _g) = store();
        let md = "# From Artifact\n\n## One\n\nalpha\n";
        let bytes = s
            .write(
                "a",
                ArtifactDescriptor::from_context("t", ContentType::OctetStream, "a"),
                ArtifactBody::Bytes(md.as_bytes().to_vec()),
            )
            .unwrap()
            .id;
        // The shape a staged `filesystem.read` of a .md file actually has.
        let envelope = write_json(&s, "a", json!({ "content": md, "path": "doc.md" }));
        let string_body = write_json(&s, "a", json!(md));
        for id in [bytes, envelope, string_body] {
            let resolved = resolve(
                &json!({ "markdown_artifact_id": id.as_str() }),
                s.as_ref(),
                "a",
            )
            .unwrap();
            assert_eq!(resolved.report.title, "From Artifact", "for {id}");
        }
        let empty = write_json(&s, "a", json!({ "rows": [1, 2] }));
        let err = resolve(
            &json!({ "markdown_artifact_id": empty.as_str() }),
            s.as_ref(),
            "a",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("no markdown text"), "got: {err}");
    }

    #[test]
    fn a_spilled_tool_output_is_unwrapped_to_the_spec_it_carries() {
        let (s, _g) = store();
        let spec_text = serde_json::to_string(&minimal_spec()).unwrap();
        // Every shape the failing session actually produced, plus the direct one.
        let cases = [
            (
                "filesystem.read spill",
                json!({ "content": spec_text, "path": "report.json" }),
            ),
            (
                "exec.run spill",
                json!({ "stdout": spec_text, "exit_code": 0 }),
            ),
            ("nested object", json!({ "result": minimal_spec() })),
            ("whole-body string", json!(spec_text)),
            ("direct spec", minimal_spec()),
        ];
        for (label, body) in cases {
            let id = write_json(&s, "a", body);
            let resolved = resolve(
                &json!({ "report_artifact_id": id.as_str() }),
                s.as_ref(),
                "a",
            )
            .unwrap_or_else(|e| panic!("{label}: {e}"));
            assert_eq!(resolved.report.title, "T", "{label}");
        }
        let raw = s
            .write(
                "a",
                ArtifactDescriptor::from_context("t", ContentType::OctetStream, "a"),
                ArtifactBody::Bytes(spec_text.into_bytes()),
            )
            .unwrap()
            .id;
        assert_eq!(
            resolve(
                &json!({ "report_artifact_id": raw.as_str() }),
                s.as_ref(),
                "a"
            )
            .unwrap()
            .report
            .title,
            "T",
            "utf-8 bytes holding the spec"
        );
    }

    #[test]
    fn an_artifact_that_is_not_a_spec_says_what_it_actually_holds() {
        let (s, _g) = store();
        let id = write_json(&s, "a", json!({ "rows": [1, 2], "count": 2 }));
        let err = resolve(
            &json!({ "report_artifact_id": id.as_str() }),
            s.as_ref(),
            "a",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("rows"), "names the real keys: {err}");
        assert!(err.contains("report_spec_id"), "points at the fix: {err}");
    }

    #[test]
    fn add_sections_appends_and_refuses_a_duplicate_heading() {
        let (s, _g) = store();
        let id = write_json(
            &s,
            "a",
            json!({
                "title": "Audit",
                "sections": [{ "heading": "Overview", "blocks": [{ "type": "prose", "text": "keep" }] }]
            }),
        );
        let appendix =
            json!({ "heading": "Appendix", "blocks": [{ "type": "prose", "text": "new" }] });
        let resolved = resolve(
            &json!({ "report_artifact_id": id.as_str(), "add_sections": [appendix] }),
            s.as_ref(),
            "a",
        )
        .unwrap();
        let headings: Vec<&str> = resolved
            .report
            .sections
            .iter()
            .map(|sec| sec.heading.as_deref().unwrap())
            .collect();
        assert_eq!(headings, ["Overview", "Appendix"], "appended in order");
        assert_eq!(
            resolved.spec["sections"][0]["blocks"][0]["text"],
            json!("keep")
        );

        let dup = json!({ "heading": "overview", "blocks": [{ "type": "prose", "text": "x" }] });
        let err = resolve(
            &json!({ "report_artifact_id": id.as_str(), "add_sections": [dup] }),
            s.as_ref(),
            "a",
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("already exists") && err.contains("section_edits"),
            "got: {err}"
        );
    }

    #[test]
    fn section_edits_then_add_sections_apply_in_that_order() {
        let (s, _g) = store();
        let id = write_json(
            &s,
            "a",
            json!({
                "title": "T",
                "sections": [{ "heading": "One", "blocks": [{ "type": "prose", "text": "old" }] }]
            }),
        );
        let resolved = resolve(
            &json!({
                "report_artifact_id": id.as_str(),
                "section_edits": { "One": { "heading": "Renamed",
                    "blocks": [{ "type": "prose", "text": "fresh" }] } },
                "add_sections": [{ "heading": "One",
                    "blocks": [{ "type": "prose", "text": "reused name" }] }]
            }),
            s.as_ref(),
            "a",
        )
        .unwrap();
        // "One" was renamed first, so appending a new "One" is not a duplicate.
        let headings: Vec<&str> = resolved
            .report
            .sections
            .iter()
            .map(|sec| sec.heading.as_deref().unwrap())
            .collect();
        assert_eq!(headings, ["Renamed", "One"]);
    }

    #[test]
    fn revisions_are_rejected_alongside_an_inline_report_but_allowed_with_markdown() {
        let (s, _g) = store();
        for revision in [
            json!({ "patch": { "title": "X" } }),
            json!({ "section_edits": { "X": null } }),
            json!({ "add_sections": [] }),
        ] {
            let mut input = json!({ "report": minimal_spec() });
            for (k, v) in revision.as_object().unwrap() {
                input[k] = v.clone();
            }
            let err = resolve(&input, s.as_ref(), "a").unwrap_err().to_string();
            assert!(
                err.contains("report_artifact_id"),
                "inline + {revision}: {err}"
            );
        }
        let ok = resolve(
            &json!({
                "markdown": "# T\n\n## One\n\nalpha\n",
                "add_sections": [{ "heading": "Extra", "blocks": [{ "type": "prose", "text": "x" }] }]
            }),
            s.as_ref(),
            "a",
        )
        .unwrap();
        assert_eq!(ok.report.sections.len(), 2);
    }

    #[test]
    fn add_sections_must_be_an_array() {
        let (s, _g) = store();
        let id = write_json(&s, "a", minimal_spec());
        let err = resolve(
            &json!({ "report_artifact_id": id.as_str(), "add_sections": { "heading": "x" } }),
            s.as_ref(),
            "a",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("must be a JSON array"), "got: {err}");
    }

    #[test]
    fn render_report_spec_prunes_empty_sections_instead_of_failing() {
        // research_report's composer has already exited by the time this runs, so
        // one empty section must not cost the other five.
        let spec = json!({
            "title": "Research",
            "sections": [
                { "heading": "Findings", "blocks": [{ "type": "prose", "text": "real" }] },
                { "heading": "Empty", "blocks": [] },
                { "heading": "More", "blocks": [{ "type": "prose", "text": "also real" }] },
            ]
        });
        let pruned = prune_empty_sections(&spec);
        assert!(matches!(pruned, Cow::Owned(_)), "a copy, not the original");
        let report = parse_spec(&pruned).expect("prunes to a valid report");
        let headings: Vec<&str> = report
            .sections
            .iter()
            .map(|s| s.heading.as_deref().unwrap())
            .collect();
        assert_eq!(headings, ["Findings", "More"]);

        // Nothing to prune: the spec is passed through untouched.
        let clean = json!({
            "title": "T", "sections": [{ "blocks": [{ "type": "prose", "text": "x" }] }]
        });
        assert!(matches!(prune_empty_sections(&clean), Cow::Borrowed(_)));

        // Hollow throughout: prunes to nothing and still fails, as it must.
        let hollow = json!({ "title": "T", "sections": [{ "heading": "A", "blocks": [] }] });
        let err = parse_spec(&prune_empty_sections(&hollow))
            .unwrap_err()
            .to_string();
        assert!(err.contains("no sections"), "got: {err}");
    }

    #[test]
    fn file_stem_title_humanises_a_filename() {
        assert_eq!(
            file_stem_title(Path::new("/x/tech-stack_overview.md")),
            "Tech Stack Overview"
        );
        assert_eq!(file_stem_title(Path::new("/x/---.md")), "Report");
        assert_eq!(file_stem_title(Path::new("")), "Report");
    }

    #[test]
    fn resolve_report_reads_from_artifact() {
        let (s, _g) = store();
        let id = s
            .write(
                "agent-a",
                ArtifactDescriptor::from_context("test", ContentType::ApplicationJson, "agent-a"),
                ArtifactBody::Json(minimal_spec()),
            )
            .unwrap()
            .id;
        let resolved = resolve(
            &json!({ "report_artifact_id": id.as_str() }),
            s.as_ref(),
            "agent-a",
        )
        .unwrap();
        assert_eq!(resolved.report.title, "T");
    }

    #[test]
    fn resolve_report_applies_patch_to_a_stored_spec() {
        let (s, _g) = store();
        let id = s
            .write(
                "agent-a",
                ArtifactDescriptor::from_context("t", ContentType::ApplicationJson, "agent-a"),
                ArtifactBody::Json(json!({
                    "title": "Orig", "meta": { "kicker": "K" },
                    "sections": [{ "blocks": [{ "type": "prose", "text": "keep me" }] }]
                })),
            )
            .unwrap()
            .id;
        let resolved = resolve(
            &json!({
                "report_artifact_id": id.as_str(),
                "patch": { "title": "Revised", "meta": { "author": "Ann" } }
            }),
            s.as_ref(),
            "agent-a",
        )
        .unwrap();
        // Patched fields change; siblings and untouched content are preserved.
        assert_eq!(resolved.report.title, "Revised");
        assert_eq!(resolved.spec["meta"]["author"], json!("Ann"));
        assert_eq!(resolved.spec["meta"]["kicker"], json!("K"));
        assert_eq!(
            resolved.spec["sections"][0]["blocks"][0]["text"],
            json!("keep me")
        );
    }

    #[test]
    fn patch_requires_a_stored_base_artifact() {
        let (s, _g) = store();
        let err = resolve(
            &json!({ "report": minimal_spec(), "patch": { "title": "X" } }),
            s.as_ref(),
            "a",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("report_artifact_id"), "got: {err}");
    }

    #[test]
    fn merge_patch_sets_removes_and_replaces() {
        let mut t = json!({
            "title": "A", "meta": { "author": "X", "date": "1" }, "sections": [1, 2]
        });
        merge_patch(
            &mut t,
            &json!({ "meta": { "author": "Y" }, "subtitle": "S" }),
        );
        assert_eq!(t["title"], json!("A"), "unmentioned key untouched");
        assert_eq!(t["meta"]["author"], json!("Y"), "nested key set");
        assert_eq!(t["meta"]["date"], json!("1"), "nested sibling preserved");
        assert_eq!(t["subtitle"], json!("S"), "new key added");
        assert_eq!(t["sections"], json!([1, 2]), "array untouched");
        // A null deletes; an array replaces wholesale (no element merge).
        merge_patch(&mut t, &json!({ "subtitle": null, "sections": [9] }));
        assert!(t.get("subtitle").is_none(), "null removes the key");
        assert_eq!(t["sections"], json!([9]), "array replaced wholesale");
    }

    #[test]
    fn section_edits_replace_remove_keep_siblings_and_reject_unknown_heading() {
        let sec = |h: &str| json!({ "heading": h, "blocks": [{ "type": "prose", "text": h }] });
        let mut spec = json!({
            "title": "T",
            "sections": [sec("Overview"), sec("Critical Findings"), sec("High")]
        });
        // Case-insensitive replace + null remove; the unnamed section is kept in order.
        let edits = json!({
            "overview": { "heading": "Overview", "blocks": [{ "type": "list", "items": [] }] },
            "high": null,
        });
        apply_section_edits(&mut spec, edits.as_object().unwrap()).unwrap();
        let heads: Vec<_> = spec["sections"]
            .as_array()
            .unwrap()
            .iter()
            .map(|s| s["heading"].as_str().unwrap().to_string())
            .collect();
        assert_eq!(heads, ["Overview", "Critical Findings"]);
        assert_eq!(spec["sections"][0]["blocks"][0]["type"], json!("list"));

        // Word-drift on a heading ("Critical" vs "Critical Findings") errors with the real
        // headings instead of silently appending a duplicate — so the model self-corrects.
        let err = apply_section_edits(
            &mut spec,
            json!({ "Critical": sec("x") }).as_object().unwrap(),
        )
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("Critical Findings") && err.contains("Overview"),
            "lists the available headings: {err}"
        );
        assert_eq!(
            spec["sections"].as_array().unwrap().len(),
            2,
            "no duplicate appended on a miss"
        );
    }

    #[test]
    fn resolve_report_section_edits_change_only_the_named_section() {
        let (s, _g) = store();
        let id = s
            .write(
                "a",
                ArtifactDescriptor::from_context("t", ContentType::ApplicationJson, "a"),
                ArtifactBody::Json(json!({
                    "title": "Audit",
                    "sections": [
                        { "heading": "Overview", "blocks": [{ "type": "prose", "text": "old" }] },
                        { "heading": "Critical", "blocks": [{ "type": "prose", "text": "C-1 stays" }] }
                    ]
                })),
            )
            .unwrap()
            .id;
        let resolved = resolve(
            &json!({
                "report_artifact_id": id.as_str(),
                "section_edits": { "Overview": { "heading": "Overview",
                    "blocks": [{ "type": "list", "items": [{ "text": "bullet" }] }] } }
            }),
            s.as_ref(),
            "a",
        )
        .unwrap();
        // Overview rewritten; the Critical section is preserved untouched.
        assert_eq!(
            resolved.spec["sections"][0]["blocks"][0]["type"],
            json!("list")
        );
        assert_eq!(
            resolved.spec["sections"][1]["blocks"][0]["text"],
            json!("C-1 stays")
        );
        assert_eq!(resolved.spec["sections"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn patch_with_sections_is_rejected_and_section_edits_need_a_base() {
        let (s, _g) = store();
        let id = s
            .write(
                "a",
                ArtifactDescriptor::from_context("t", ContentType::ApplicationJson, "a"),
                ArtifactBody::Json(minimal_spec()),
            )
            .unwrap()
            .id;
        // `sections` in a merge patch would drop content — rejected, pointing to section_edits.
        let err = resolve(
            &json!({ "report_artifact_id": id.as_str(), "patch": { "sections": [] } }),
            s.as_ref(),
            "a",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("section_edits"), "got: {err}");
        // section_edits, like patch, revise a stored base — not an inline report.
        let err = resolve(
            &json!({ "report": minimal_spec(), "section_edits": { "X": null } }),
            s.as_ref(),
            "a",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("report_artifact_id"), "got: {err}");
    }

    #[test]
    fn resolve_theme_builds_brand_from_base_and_overrides() {
        let mut themes = std::collections::HashMap::new();
        themes.insert(
            "harbor".to_string(),
            BrandThemeConfig {
                base: Some("noir".into()),
                cover_bg: Some("#0B2A3A".into()),
                accent: Some("#42D17C".into()),
                secondary: Some("#EE8AA0".into()),
                logo: Some("assets/report/harbor.svg".into()),
                ..Default::default()
            },
        );
        let cfg = AgentReportConfig {
            default_theme: Some("harbor".into()),
            emoji: None,
            themes,
        };

        // Explicit (case-insensitive) brand name: overrides applied, base inherited.
        let (t, logo) = resolve_theme(Some("Harbor"), Some(&cfg));
        assert_eq!(t.palette.cover_bg, "#0B2A3A");
        assert_eq!(t.palette.accent, "#42D17C");
        assert_eq!(t.palette.primary, "#EDEFF2", "inherited from noir base");
        assert_eq!(logo.as_deref(), Some("assets/report/harbor.svg"));

        // No request → workspace default_theme.
        assert_eq!(resolve_theme(None, Some(&cfg)).0.name, "harbor");
        // Unknown name → default built-in; no config → built-in.
        assert_eq!(resolve_theme(Some("nope"), Some(&cfg)).0.name, "espresso");
        assert_eq!(resolve_theme(Some("noir"), None).0.name, "noir");
    }

    #[test]
    fn filename_uses_slug_or_override_with_correct_extension() {
        assert_eq!(
            report_filename(&json!({}), "The Coffee Economy!", Format::Report),
            "the-coffee-economy.pdf"
        );
        assert_eq!(
            report_filename(&json!({}), "  ", Format::Infographic),
            "report.png"
        );
        assert_eq!(
            report_filename(&json!({ "filename": "q3" }), "x", Format::Report),
            "q3.pdf"
        );
        assert_eq!(
            report_filename(&json!({ "filename": "q3.pdf" }), "x", Format::Report),
            "q3.pdf"
        );
    }

    #[test]
    fn emoji_enabled_resolves_per_report_then_workspace_then_on() {
        let with_flag = |emoji: Value| {
            parse_spec(&json!({
                "title": "T", "emoji": emoji,
                "sections": [{ "blocks": [{ "type": "prose", "text": "x" }] }]
            }))
            .unwrap()
        };
        let unset = parse_spec(
            &json!({ "title": "T", "sections": [{ "blocks": [{ "type": "prose", "text": "x" }] }] }),
        )
        .unwrap();
        let cfg_off = AgentReportConfig {
            emoji: Some(false),
            ..Default::default()
        };

        // A per-report flag always wins over the workspace default.
        assert!(emoji_enabled(&with_flag(json!(true)), Some(&cfg_off)));
        assert!(!emoji_enabled(&with_flag(json!(false)), None));
        // Unset defers to the workspace default, then to on.
        assert!(!emoji_enabled(&unset, Some(&cfg_off)));
        assert!(emoji_enabled(&unset, None));
    }
}
