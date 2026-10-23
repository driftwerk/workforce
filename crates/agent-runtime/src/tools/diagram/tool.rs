//! The `diagram` tool: draw a diagram and hand it over as a file.
//!
//! The same `wf-diagram` spec the customer-facing widget renders, so a process
//! an agent describes in a chat and one it drops into Slack are the same
//! picture. PNG is the default because it is the only format Slack renders
//! inline; SVG is the vector original; `.excalidraw` is the editable scene, and
//! also the way to get the hand-drawn look (the sketch skin is not rendered
//! server-side).

use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde_json::{json, Value};

use super::excalidraw::to_excalidraw;
use super::layout::layout;
use super::spec::validate;
use super::svg::{render, SERVER_METRICS};
use crate::artifacts::download_marker;
use crate::artifacts::store::{global_store, ATTACHMENT_TTL};
use crate::artifacts::stream::download_filename;
use crate::artifacts::types::{ArtifactBody, ArtifactDescriptor, ContentType};
use crate::runner::CURRENT_AGENT_ID;
use crate::tools::image_gen::GENERATED_IMAGES_KEY;
use crate::tools::report::theme::resolve as resolve_theme;
use crate::tools::{Tool, ToolResult};

pub const DIAGRAM_TOOL_NAME: &str = "diagram";

const DESCRIPTION: &str = "Draw a node-and-arrow diagram and deliver it as a file. Use it when the \
    answer is a STRUCTURE — a process, a decision path, a hand-off between people or systems, or an \
    ordering of events. Never for quantities (that is a chart), never for a list of records (that is \
    a table), and never for something with no structure to show.\n\
    Describe the diagram in meaning only: name the boxes and what connects to what. There is no \
    colour field and no positions — each `group` is given its own colour automatically and the \
    layout is computed, so grouping is worth doing whenever a process crosses more than one team.\n\
    Keep it small: about 12 nodes, never more than 20. Labels of at most 6 words.\n\
    `format`: `png` (default, the only one that previews inline in chat), `svg` (vector), or \
    `excalidraw` (an editable scene — also how to get a hand-drawn/whiteboard look, which the \
    server-rendered image does not do).";

pub struct DiagramTool;

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "spec": {
                "type": "object",
                "description": "The diagram. `kind` is `flow` or `sequence`.\n\
                    flow: {kind:\"flow\", title?, direction?:\"down\"|\"right\", \
                    nodes:[{id, label, shape?:\"box\"|\"round\"|\"diamond\"|\"cylinder\", \
                    tone?:\"accent\"|\"good\"|\"warn\"|\"bad\", group?}], \
                    edges:[{from, to, label?, style?:\"dashed\", arrow?:\"both\"|\"none\"}]} — every \
                    edge's from/to must name a declared node; `round` for a start or end, `diamond` \
                    for a decision (label its outgoing edges with the answers), `cylinder` for a \
                    store; `tone` only to mark an exception or failure path.\n\
                    sequence: {kind:\"sequence\", title?, participants:[{id, label}] (2-12), \
                    steps:[{from, to, label?, style?:\"dashed\" for a reply, note?}]} in time order; \
                    from equal to to is a self-step."
            },
            "format": {
                "type": "string",
                "enum": ["png", "svg", "excalidraw"],
                "description": "Delivery format. Defaults to png."
            },
            "title": {
                "type": "string",
                "description": "Overrides the spec's own title, and names the downloaded file."
            }
        },
        "required": ["spec"]
    })
}

#[async_trait]
impl Tool for DiagramTool {
    fn name(&self) -> &str {
        DIAGRAM_TOOL_NAME
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        input_schema()
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
                "no agent context in scope; cannot stage the diagram",
                elapsed(start),
            ));
        };
        let Some(store) = global_store() else {
            return Ok(ToolResult::failure(
                "artifact store is not configured; cannot stage the diagram",
                elapsed(start),
            ));
        };

        let spec = match validate(input.get("spec").unwrap_or(&Value::Null)) {
            Ok(s) => s,
            Err(e) => return Ok(ToolResult::failure(e, elapsed(start))),
        };
        let laid_out = layout(&spec, SERVER_METRICS);
        let format = input
            .get("format")
            .and_then(Value::as_str)
            .unwrap_or("png")
            .to_lowercase();
        let title = input
            .get("title")
            .and_then(Value::as_str)
            .or_else(|| spec.title())
            .unwrap_or("diagram")
            .to_string();

        let (bytes, mime, ext) = match format.as_str() {
            "excalidraw" => (
                serde_json::to_vec_pretty(&to_excalidraw(&laid_out, Some(&title)))
                    .unwrap_or_default(),
                "application/json",
                "excalidraw",
            ),
            "svg" => (
                render(&spec, &laid_out, resolve_theme(None)).into_bytes(),
                "image/svg+xml",
                "svg",
            ),
            "png" => {
                let svg = render(&spec, &laid_out, resolve_theme(None));
                match crate::tools::report::render::rasterize_svg(&svg).await {
                    Ok(png) => (png, "image/png", "png"),
                    Err(e) => {
                        return Ok(ToolResult::failure(
                            format!("could not rasterize the diagram: {e}"),
                            elapsed(start),
                        ))
                    }
                }
            }
            other => {
                return Ok(ToolResult::failure(
                    format!("unknown format {other:?} — use png, svg or excalidraw"),
                    elapsed(start),
                ))
            }
        };

        let filename = download_filename(&title, ext);
        let size = bytes.len();
        let descriptor = ArtifactDescriptor::from_context(
            DIAGRAM_TOOL_NAME,
            ContentType::OctetStream,
            agent_id.clone(),
        )
        .with_ttl(ATTACHMENT_TTL)
        .with_file_meta(filename.clone(), mime);
        let handle = match store.write(&agent_id, descriptor, ArtifactBody::Bytes(bytes)) {
            Ok(h) => h,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("failed to stage the diagram for delivery: {e}"),
                    elapsed(start),
                ))
            }
        };

        let mut output = json!({
            GENERATED_IMAGES_KEY: [download_marker(&handle.id, mime, &filename, size as u64)],
            "filename": filename,
            "format": format,
            "note": "Diagram generated and being delivered to this conversation as a downloadable \
                     file automatically — it happens outside your context, so you will NOT see it. \
                     This is expected, NOT a failure: do not try to read, re-upload, or verify it. \
                     Just tell the user the diagram is ready.",
        });
        // Fail loud about anything the layout could not place, so a wrong id is
        // visible rather than a silently thinner diagram.
        if !laid_out.dropped.is_empty() {
            output["dropped_connections"] = json!(laid_out.dropped);
            output["warning"] = json!(
                "These connections named a node that does not exist and were NOT drawn. Tell the \
                 user, or re-issue the diagram with the ids corrected."
            );
        }
        Ok(ToolResult::success(output, elapsed(start)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_schema_requires_a_spec_and_offers_three_formats() {
        let schema = input_schema();
        assert_eq!(schema["required"], json!(["spec"]));
        assert_eq!(
            schema["properties"]["format"]["enum"],
            json!(["png", "svg", "excalidraw"])
        );
    }

    /// The description is what makes a model reach for this instead of prose,
    /// so the discriminating guidance has to be in it.
    #[test]
    fn the_description_says_when_not_to_use_it() {
        assert!(DESCRIPTION.contains("that is a chart"));
        assert!(DESCRIPTION.contains("that is a table"));
        assert!(DESCRIPTION.contains("no colour field"));
    }

    #[tokio::test]
    async fn an_invalid_spec_fails_without_staging_anything() {
        let result = DiagramTool
            .execute(json!({ "spec": { "kind": "mindmap" } }))
            .await
            .expect("tool runs");
        assert!(!result.success);
    }
}

#[cfg(test)]
mod live_render {
    use serde_json::json;

    /// Renders through the real typst binary — proves the Slack path end to end
    /// rather than trusting that valid SVG rasterizes.
    #[tokio::test]
    async fn renders_a_real_png_through_typst() {
        // Rasterizing shells out to `typst`, which is on a dev laptop and NOT on
        // the CI runner. Without this the test fails there — and the hub build
        // that gates every hub deploy fails with it.
        if !crate::tools::report::render::typst_available() {
            eprintln!("skipping: `typst` binary not on PATH");
            return;
        }
        let spec = crate::tools::diagram::spec::validate(&json!({
            "kind": "flow", "title": "Order pipeline",
            "nodes": [
                { "id": "in", "label": "Order received", "shape": "round" },
                { "id": "chk", "label": "Stock available?", "shape": "diamond" },
                { "id": "pick", "label": "Pick and pack", "group": "Warehouse" },
                { "id": "bo", "label": "Backorder", "group": "Warehouse", "tone": "warn" },
                { "id": "inv", "label": "Raise invoice", "group": "Finance" },
                { "id": "done", "label": "Notify customer", "shape": "round" }],
            "edges": [
                { "from": "in", "to": "chk" },
                { "from": "chk", "to": "pick", "label": "yes" },
                { "from": "chk", "to": "bo", "label": "no" },
                { "from": "pick", "to": "inv" }, { "from": "inv", "to": "done" },
                { "from": "bo", "to": "done" }]
        }))
        .expect("valid");
        let laid_out = crate::tools::diagram::layout::layout(
            &spec,
            crate::tools::diagram::svg::SERVER_METRICS,
        );
        let svg = crate::tools::diagram::svg::render(
            &spec,
            &laid_out,
            crate::tools::report::theme::resolve(None),
        );
        let png = crate::tools::report::render::rasterize_svg(&svg)
            .await
            .expect("rasterizes");
        assert_eq!(&png[..4], b"\x89PNG", "a real PNG, not an error page");
        assert!(png.len() > 5000, "a drawn image, got {} bytes", png.len());
    }
}
