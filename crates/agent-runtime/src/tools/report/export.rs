//! `export_report` — render a report spec to an editable native document
//! (`.docx` or `.pptx`) and deliver it to the conversation as a file.
//!
//! Same spec, theming and charts as `generate_report`; the difference is the
//! container. The lowering ([`super::native`]) resolves charts to embedded
//! images plus an adjacent data table so the chart's numbers stay editable.
//! Bytes are staged and delivered out-of-band, exactly like `generate_report`
//! and `export_xlsx`, so they never re-enter the model context. For a native
//! Google Doc/Slides, upload the returned artifact with
//! `google.drive.upload(convert_to=…)`, which converts on upload.

use std::path::PathBuf;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde_json::{json, Value};

use super::native::NativeFormat;
use super::{
    attach_spec_id, native_filename, render_native, resolve_images, resolve_report, Resolved,
};
use crate::artifacts::download_marker;
use crate::artifacts::store::global_store;
use crate::artifacts::types::{ArtifactBody, ArtifactDescriptor, ContentType};
use crate::runner::CURRENT_AGENT_ID;
use crate::tools::image_gen::GENERATED_IMAGES_KEY;
use crate::tools::{Tool, ToolResult};

pub const EXPORT_REPORT_TOOL_NAME: &str = "export_report";

const DESCRIPTION: &str = "Render a report spec to an EDITABLE native document and deliver it to this \
    conversation as a file. `format: \"docx\"` produces a Microsoft Word document; `format: \"pptx\"` \
    produces a PowerPoint deck (a title slide plus a slide per section). The spec is identical to \
    generate_report's (title, sections of typed blocks: prose, heading, stats, chart, table, list, \
    code, image) and uses the same themes — use this when the user wants something they can edit rather \
    than a finished PDF. Charts render as an image plus an adjacent data table, so their numbers stay \
    editable. \
    TO TYPESET A DOCUMENT THAT ALREADY EXISTS — a markdown file, or markdown you have written — pass \
    `markdown_path`, `markdown_artifact_id` or `markdown` and code converts it for you; always prefer \
    that to re-typing the document as a spec, which loses content. \
    Otherwise provide the spec inline as `report`, or by `report_artifact_id`. To produce a native \
    Google Doc or Google Slides, call this with format docx (for Docs) or pptx (for Slides), then \
    upload the returned artifact with google.drive.upload using convert_to \
    'application/vnd.google-apps.document' or '…presentation' — Drive converts it to a fully editable \
    native Google file. To revise a document you already generated, NEVER re-send the whole spec: pass \
    `report_artifact_id` set to the prior result's `report_spec_id`, plus `patch` for top-level fields, \
    `section_edits` to change one section's content while every other section is preserved exactly, \
    and/or `add_sections` to append new sections. Every section must contain at least one block. \
    The file is delivered out-of-band: you will NOT see it and its bytes do not re-enter your context, so \
    don't try to read, re-upload, or verify it — just tell the user it's ready.";

pub struct ExportReportTool {
    workspace: PathBuf,
}

impl ExportReportTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

#[async_trait]
impl Tool for ExportReportTool {
    fn name(&self) -> &str {
        EXPORT_REPORT_TOOL_NAME
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
                    "description": "The report spec (this OR report_artifact_id). Same shape as generate_report: \
                        title (required), subtitle?, theme?, emoji?, meta{kicker?,author?,date?}, \
                        sections:[{heading?, blocks:[…]}] with block types prose|heading|stats|chart|table|list|image."
                },
                "report_artifact_id": {
                    "type": "string",
                    "description": "Alternative to `report`: the `report_spec_id` from a previous \
                        generate_report/export_report call, to re-render or (with `patch`/`section_edits`/\
                        `add_sections`) revise that exact spec."
                },
                "markdown_path": {
                    "type": "string",
                    "description": "Path to a markdown file in the agent workspace to typeset. Code converts \
                        headings into sections, and paragraphs, bullet/numbered lists, GFM tables and ``` code \
                        fences into blocks — nothing is summarised or dropped. The first heading becomes the \
                        title (override with `patch`). Use this for \"make a Word doc from this file\"."
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
                        exists is rejected — use `section_edits` to rewrite that one instead.",
                    "items": { "type": "object" }
                },
                "patch": {
                    "type": "object",
                    "description": "Revise top-level fields of the spec named by `report_artifact_id` without \
                        re-sending it: an RFC 7386 JSON merge patch with ONLY the fields to change (e.g. \
                        {\"title\": \"New title\"}). A null removes a field. Do NOT put `sections` here; use \
                        `section_edits`. Requires `report_artifact_id`."
                },
                "section_edits": {
                    "type": "object",
                    "description": "Change a SECTION's content in the spec named by `report_artifact_id` while \
                        keeping every other section as-is: map an existing section heading (case-insensitive) to \
                        its full new section object {heading, blocks:[…]} to replace it, or null to remove it. \
                        Only the headings you name change; an unknown heading is rejected with the real headings. \
                        Requires `report_artifact_id`."
                },
                "format": {
                    "type": "string",
                    "enum": ["docx", "pptx"],
                    "description": "Output container: docx (Word) or pptx (PowerPoint). Default docx."
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
                "no agent context in scope; cannot stage the document",
                elapsed(start),
            ));
        };
        let Some(store) = global_store() else {
            return Ok(ToolResult::failure(
                "artifact store is not configured; cannot stage the document",
                elapsed(start),
            ));
        };

        let format = match input["format"].as_str() {
            None => NativeFormat::Docx,
            Some(name) => match NativeFormat::parse(name.trim()) {
                Some(f) => f,
                None => {
                    return Ok(ToolResult::failure(
                        format!("unknown format `{name}`; use \"docx\" or \"pptx\""),
                        elapsed(start),
                    ))
                }
            },
        };

        let Resolved { report, spec } =
            match resolve_report(&input, store.as_ref(), &agent_id, &self.workspace) {
                Ok(r) => r,
                Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
            };
        let images = resolve_images(&report, store.as_ref(), &agent_id);
        let bytes = match render_native(&report, &self.workspace, &images, format).await {
            Ok(b) => b,
            Err(e) => return Ok(ToolResult::failure(e.to_string(), elapsed(start))),
        };
        let size = bytes.len();
        let filename = native_filename(&input, &report.title, format.ext());

        let descriptor = ArtifactDescriptor::from_context(
            EXPORT_REPORT_TOOL_NAME,
            ContentType::OctetStream,
            agent_id.clone(),
        )
        .with_file_meta(filename.clone(), format.mime());
        let handle = match store.write(&agent_id, descriptor, ArtifactBody::Bytes(bytes)) {
            Ok(h) => h,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("failed to stage the document for delivery: {e}"),
                    elapsed(start),
                ))
            }
        };

        let mut output = json!({
            GENERATED_IMAGES_KEY: [download_marker(&handle.id, format.mime(), &filename, size as u64)],
            "filename": filename,
            "note": "Document generated and being delivered to this conversation as a downloadable file \
                     automatically — it happens outside your context, so you will NOT see it. This is \
                     expected, NOT a failure: do not try to read, re-upload, or verify it. To turn it into \
                     a native Google Doc/Slides, upload this artifact with google.drive.upload(convert_to=…). \
                     To revise it later, NEVER re-send the whole spec — call export_report again with \
                     `report_artifact_id` set to the `report_spec_id` below, plus `patch` for top-level \
                     fields and/or `section_edits` to change one section by heading. Just tell the user \
                     it's ready.",
        });
        attach_spec_id(
            &mut output,
            store.as_ref(),
            &agent_id,
            EXPORT_REPORT_TOOL_NAME,
            &spec,
        );
        Ok(ToolResult::success(output, elapsed(start)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::store::install_test_global_store;
    use crate::artifacts::types::ArtifactId;
    use tempfile::TempDir;

    fn tool() -> (ExportReportTool, TempDir) {
        let ws = TempDir::new().unwrap();
        (ExportReportTool::new(ws.path().to_path_buf()), ws)
    }

    fn spec() -> Value {
        json!({
            "report": {
                "title": "Quarter Review",
                "sections": [{ "heading": "Summary", "blocks": [
                    { "type": "prose", "text": "All on track." },
                    { "type": "table", "columns": ["A", "B"], "rows": [[1, 2]] }
                ] }]
            }
        })
    }

    async fn run(agent: &str, tool: &ExportReportTool, input: Value) -> ToolResult {
        crate::runner::CURRENT_AGENT_ID
            .scope(agent.to_string(), async {
                tool.execute(input).await.unwrap()
            })
            .await
    }

    #[tokio::test]
    async fn exports_a_docx_and_stages_it_for_delivery() {
        let store = install_test_global_store();
        let (tool, _ws) = tool();
        let mut input = spec();
        input["format"] = json!("docx");
        let result = run("export-docx-test", &tool, input).await;
        assert!(result.success, "{:?}", result.output);
        let marker = &result.output[GENERATED_IMAGES_KEY][0];
        assert_eq!(marker["filename"], "quarter-review.docx");
        assert!(marker["media_type"]
            .as_str()
            .unwrap()
            .contains("wordprocessingml"));
        let id = ArtifactId::parse(marker["artifact_id"].as_str().unwrap()).unwrap();
        let ArtifactBody::Bytes(bytes) = store.read("export-docx-test", &id).unwrap() else {
            panic!("expected staged bytes");
        };
        assert_eq!(&bytes[0..2], b"PK", "a docx is a zip");
    }

    #[tokio::test]
    async fn a_markdown_file_in_the_workspace_exports_end_to_end() {
        install_test_global_store();
        let (tool, ws) = tool();
        std::fs::write(
            ws.path().join("quarter-review.md"),
            "## Summary\n\nAll on track.\n\n| A | B |\n|---|---|\n| 1 | 2 |\n\n## Next\n\n```sh\nmake ship\n```\n",
        )
        .unwrap();
        let result = run(
            "export-md-test",
            &tool,
            json!({ "markdown_path": "quarter-review.md", "format": "docx" }),
        )
        .await;
        assert!(result.success, "{:?}", result.error);
        assert_eq!(
            result.output[GENERATED_IMAGES_KEY][0]["filename"], "quarter-review.docx",
            "title comes from the filename when the document has no title heading"
        );
        assert!(
            result.output["report_spec_id"].is_string(),
            "the converted spec is staged for revision like any other"
        );
    }

    #[tokio::test]
    async fn pptx_format_and_filename_override() {
        install_test_global_store();
        let (tool, _ws) = tool();
        let mut input = spec();
        input["format"] = json!("pptx");
        input["filename"] = json!("deck");
        let result = run("export-pptx-test", &tool, input).await;
        assert!(result.success, "{:?}", result.output);
        assert_eq!(
            result.output[GENERATED_IMAGES_KEY][0]["filename"],
            "deck.pptx"
        );
    }

    #[tokio::test]
    async fn rejects_unknown_format() {
        install_test_global_store();
        let (tool, _ws) = tool();
        let mut input = spec();
        input["format"] = json!("odt");
        let result = run("export-fmt-test", &tool, input).await;
        assert!(!result.success);
        assert!(result.error.unwrap().contains("unknown format"));
    }
}
