//! The `chart` tool: draw a chart and hand it over as a file.
//!
//! The same `wf-chart` spec the customer-facing widget renders, so a chart an
//! agent shows in a chat widget and one it drops into Slack or an email are the
//! same picture. PNG is the default because it is the only format Slack
//! previews inline and the only one email clients show; SVG is the vector
//! original.

use std::path::PathBuf;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde_json::{json, Value};

use super::{draw, request, ChartRequest, CHART_SPEC_MEDIA_TYPE};
use crate::artifacts::download_marker;
use crate::artifacts::store::ArtifactStore;
use crate::artifacts::store::{global_store, ATTACHMENT_TTL};
use crate::artifacts::stream::download_filename;
use crate::artifacts::types::{ArtifactBody, ArtifactDescriptor, ContentType};
use crate::runner::CURRENT_AGENT_ID;
use crate::tools::image_gen::{GENERATED_IMAGES_KEY, SPEC_KEY};
use crate::tools::{Tool, ToolResult};

pub const CHART_TOOL_NAME: &str = "chart";

const DESCRIPTION: &str =
    "Draw a chart from numbers and deliver it as an image. Use it whenever a \
    trend, a share of a whole, or a comparison between quantities is clearer as a picture than as \
    prose — sales by month, tickets by status, signups per plan. Never for a process or a hand-off \
    (that is a diagram), and never for a list of records (that is a table; prefer a table when \
    there are more than about 8 categories).\n\
    Give it the data only: `kind` (pie, bar, line, area or scatter), `labels` for the categories, \
    and one or more `series` of numbers aligned with the labels. Layout and axes are computed; \
    colours follow the workspace theme unless you name a `theme` or pass `colors`. One series per \
    pie; at most 8 series.\n\
    The image is delivered to this conversation automatically. To send it somewhere else instead \
    — another Slack channel or thread, or an email — pass `deliver: false` and hand the returned \
    `artifact_id` to `messaging.send_file` or an email tool's `attach_artifact_ids`.\n\
    `format`: `png` (default, the one chat and email show inline) or `svg` (vector). Axis \
    titles (`x_label`, `y_label`) apply to vertical charts; horizontal bars label each bar instead.";

pub struct ChartTool {
    workspace: PathBuf,
}

impl ChartTool {
    pub fn new(workspace: PathBuf) -> Self {
        Self { workspace }
    }
}

fn input_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "spec": {
                "type": "object",
                "description": "The chart. {kind:\"pie\"|\"bar\"|\"line\"|\"area\"|\"scatter\", \
                    title?, labels?:[category names], \
                    series:[{name?, data:[numbers aligned with labels]}], \
                    stacked?:true (bar/line/area), horizontal?:true (bar), x_label?, y_label?, \
                    unit?:\"£\" or \"%\"}. For scatter, each series' data is [[x,y], …] pairs and \
                    labels are not used. A pie takes exactly one series and needs labels."
            },
            "format": {
                "type": "string",
                "enum": ["png", "svg"],
                "description": "Delivery format. Defaults to png."
            },
            "theme": {
                "type": "string",
                "description": "Draw in a named theme instead of the workspace default: a brand theme \
                    from set_report_theme, or a built-in (espresso|slate|noir|editorial|mint|sunset; \
                    slate is blue)."
            },
            "colors": {
                "type": "array",
                "items": { "type": "string" },
                "description": "Series colours as #rrggbb, in series order (pie slices in label \
                    order), at most 8. Overrides the theme's palette for this chart only."
            },
            "title": {
                "type": "string",
                "description": "Overrides the spec's own title, and names the downloaded file."
            },
            "caption": {
                "type": "string",
                "description": "One-sentence takeaway posted with the image."
            },
            "deliver": {
                "type": "boolean",
                "description": "Deliver to this conversation (default true). Set false when you will \
                    route the file yourself by artifact_id."
            }
        },
        "required": ["spec"]
    })
}

#[async_trait]
impl Tool for ChartTool {
    fn name(&self) -> &str {
        CHART_TOOL_NAME
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
                "no agent context in scope; cannot stage the chart",
                elapsed(start),
            ));
        };
        let Some(store) = global_store() else {
            return Ok(ToolResult::failure(
                "artifact store is not configured; cannot stage the chart",
                elapsed(start),
            ));
        };

        let chart = match request(&input) {
            Ok(parsed) => parsed,
            Err(e) => return Ok(ToolResult::failure(e, elapsed(start))),
        };
        let caption = input
            .get("caption")
            .and_then(Value::as_str)
            .filter(|c| !c.trim().is_empty());
        let deliver = input
            .get("deliver")
            .and_then(Value::as_bool)
            .unwrap_or(true);

        let bytes = match draw(&chart, Some(&self.workspace)).await {
            Ok(bytes) => bytes,
            Err(e) => return Ok(ToolResult::failure(format!("{e:#}"), elapsed(start))),
        };
        let (mime, ext) = (chart.format.media_type(), chart.format.extension());

        let filename = download_filename(&chart.title, ext);
        let size = bytes.len();
        let descriptor = ArtifactDescriptor::from_context(
            CHART_TOOL_NAME,
            ContentType::OctetStream,
            agent_id.clone(),
        )
        .with_ttl(ATTACHMENT_TTL)
        .with_file_meta(filename.clone(), mime);
        let handle = match store.write(&agent_id, descriptor, ArtifactBody::Bytes(bytes)) {
            Ok(h) => h,
            Err(e) => {
                return Ok(ToolResult::failure(
                    format!("failed to stage the chart: {e}"),
                    elapsed(start),
                ))
            }
        };

        let mut output = json!({
            "artifact_id": handle.id.as_str(),
            "filename": filename,
            "media_type": mime,
            "format": ext,
        });
        if deliver {
            let mut marker = download_marker(&handle.id, mime, &filename, size as u64);
            if let Some(caption) = caption {
                marker["caption"] = json!(caption);
            }
            if let Some(spec) = stage_spec(store.as_ref(), &agent_id, &chart) {
                marker[SPEC_KEY] = spec;
            }
            output[GENERATED_IMAGES_KEY] = json!([marker]);
            output["note"] = json!(
                "Chart generated and being delivered to this conversation as an image \
                 automatically — it happens outside your context, so you will NOT see it. This is \
                 expected, NOT a failure: do not try to read, re-upload, or verify it. Just tell \
                 the user the chart is ready."
            );
        } else {
            output["note"] = json!(
                "Chart staged but NOT delivered, as requested. Pass `artifact_id` to \
                 `messaging.send_file` (Slack channel/thread or DM) or to an email tool's \
                 `attach_artifact_ids` (with `inline: true` to embed it in the HTML body)."
            );
        }
        Ok(ToolResult::success(output, elapsed(start)))
    }
}

/// The chart's validated spec, staged beside its picture for a reader that
/// draws it itself, as the marker naming it. A spec that cannot be staged
/// leaves the picture to stand alone.
fn stage_spec(store: &dyn ArtifactStore, agent_id: &str, chart: &ChartRequest) -> Option<Value> {
    let bytes = serde_json::to_vec(&chart.spec).ok()?;
    let filename = download_filename(&chart.title, "json");
    let size = bytes.len() as u64;
    let descriptor = ArtifactDescriptor::from_context(
        CHART_TOOL_NAME,
        ContentType::OctetStream,
        agent_id.to_string(),
    )
    .with_ttl(ATTACHMENT_TTL)
    .with_file_meta(filename.clone(), CHART_SPEC_MEDIA_TYPE);
    match store.write(agent_id, descriptor, ArtifactBody::Bytes(bytes)) {
        Ok(handle) => Some(download_marker(
            &handle.id,
            CHART_SPEC_MEDIA_TYPE,
            &filename,
            size,
        )),
        Err(e) => {
            tracing::warn!(error = %e, "staging a chart's spec failed; its picture stands alone");
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::artifacts::store::install_test_global_store;

    fn tool() -> ChartTool {
        ChartTool::new(tempfile::tempdir().expect("tempdir").keep())
    }

    async fn run(input: Value) -> ToolResult {
        install_test_global_store();
        CURRENT_AGENT_ID
            .scope("chart-tool-test".to_string(), tool().execute(input))
            .await
            .expect("tool runs")
    }

    #[test]
    fn the_schema_requires_a_spec_and_offers_two_formats() {
        let schema = input_schema();
        assert_eq!(schema["required"], json!(["spec"]));
        assert_eq!(
            schema["properties"]["format"]["enum"],
            json!(["png", "svg"])
        );
        assert!(
            schema["properties"]["theme"].is_object() && schema["properties"]["colors"].is_object()
        );
    }

    #[test]
    fn the_description_says_when_not_to_use_it_and_how_to_route_elsewhere() {
        assert!(DESCRIPTION.contains("that is a diagram"));
        assert!(DESCRIPTION.contains("that is a table"));
        assert!(DESCRIPTION.contains("deliver: false"));
        assert!(DESCRIPTION.contains("attach_artifact_ids"));
    }

    #[tokio::test]
    async fn an_invalid_spec_fails_without_staging_anything() {
        let result = run(json!({ "spec": { "kind": "donut" } })).await;
        assert!(!result.success);
        assert!(result
            .error
            .as_deref()
            .unwrap_or_default()
            .contains("kind must be one of"));
    }

    #[tokio::test]
    async fn svg_is_staged_with_the_marker_and_caption() {
        let result = run(json!({
            "spec": { "kind": "bar", "title": "Signups by plan", "labels": ["A"], "series": [{ "data": [1] }] },
            "format": "svg",
            "caption": "Pro leads."
        }))
        .await;
        assert!(result.success, "{:?}", result.error);
        let marker = &result.output[GENERATED_IMAGES_KEY][0];
        assert_eq!(marker["media_type"], "image/svg+xml");
        assert_eq!(marker["filename"], "Signups-by-plan.svg");
        assert_eq!(marker["caption"], "Pro leads.");
        assert_eq!(result.output["artifact_id"], marker["artifact_id"]);
    }

    /// Beside the picture the marker names the validated spec, staged as a
    /// file of its own, which reads back as exactly what the validator
    /// normalised — what a reader that draws charts itself is shown.
    #[tokio::test]
    async fn the_marker_names_the_validated_spec_staged_beside_the_picture() {
        let input = json!({
            "spec": { "kind": "bar", "labels": ["A", "B"], "series": [{ "name": "Q3", "data": ["4", 5] }] },
            "format": "svg",
            "title": "Signups by plan"
        });
        let result = run(input.clone()).await;
        assert!(result.success, "{:?}", result.error);
        let spec = &result.output[GENERATED_IMAGES_KEY][0][SPEC_KEY];
        assert_eq!(spec["media_type"], CHART_SPEC_MEDIA_TYPE);
        assert_eq!(spec["filename"], "Signups-by-plan.json");
        let id = crate::artifacts::types::ArtifactId::parse(spec["artifact_id"].as_str().unwrap())
            .unwrap();
        assert_ne!(spec["artifact_id"], result.output["artifact_id"]);
        let body = global_store()
            .unwrap()
            .read("chart-tool-test", &id)
            .unwrap()
            .into_bytes()
            .unwrap();
        let staged: Value = serde_json::from_slice(&body).unwrap();
        let normalised = serde_json::to_value(request(&input).unwrap().spec).unwrap();
        assert_eq!(staged, normalised);
        assert_eq!(staged["title"], "Signups by plan");
        assert_eq!(staged["series"][0]["data"], json!([4.0, 5.0]));
        let refs = crate::tools::image_gen::parse_generated_images(&result.output);
        assert_eq!(refs[0].spec.as_ref().unwrap().artifact_id, id);
    }

    #[tokio::test]
    async fn deliver_false_omits_the_marker_but_returns_the_id() {
        let result = run(json!({
            "spec": { "kind": "pie", "labels": ["A", "B"], "series": [{ "data": [1, 2] }] },
            "format": "svg",
            "title": "Split",
            "deliver": false
        }))
        .await;
        assert!(result.success, "{:?}", result.error);
        assert!(result.output.get(GENERATED_IMAGES_KEY).is_none());
        assert!(result.output["artifact_id"]
            .as_str()
            .unwrap()
            .starts_with("art_"));
        assert_eq!(result.output["filename"], "Split.svg");
        assert!(result.output["note"]
            .as_str()
            .unwrap()
            .contains("attach_artifact_ids"));
    }
}

#[cfg(test)]
mod live_render {
    use serde_json::json;

    /// Renders through the real typst binary — proves the Slack path end to end
    /// rather than trusting that valid SVG rasterizes.
    #[tokio::test]
    async fn renders_a_real_png_through_typst() {
        if !crate::tools::report::render::typst_available() {
            eprintln!("skipping: `typst` binary not on PATH");
            return;
        }
        let spec = crate::tools::chart::spec::validate(&json!({
            "kind": "bar", "title": "Signups by plan", "unit": "£",
            "labels": ["Free", "Pro", "Team"],
            "series": [{ "name": "March", "data": [420, 180, 60] }, { "name": "April", "data": [510, 220, 90] }]
        }))
        .expect("valid");
        let svg =
            crate::tools::chart::render::render(&spec, crate::tools::report::theme::resolve(None));
        let png = crate::tools::report::render::rasterize_svg(&svg)
            .await
            .expect("rasterizes");
        assert_eq!(&png[..4], b"\x89PNG", "a real PNG, not an error page");
        assert!(png.len() > 5000, "a drawn image, got {} bytes", png.len());
    }
}
