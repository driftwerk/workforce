//! Server-side `wf-chart`: the widget's chart contract, rendered in Rust so hub
//! agents and WASM skills can hand a chart to Slack, an email, or a report.
//!
//! Validation is held to the widget by a shared golden corpus rather than by
//! description — see `goldens.rs`.

use std::path::Path;

use eyre::{Result, WrapErr};
use serde_json::Value;

use crate::tools::report::{render::rasterize_svg, theme, workspace_theme};

pub mod render;
pub mod spec;
pub mod tool;

pub use spec::ChartSpec;
pub use tool::{ChartTool, CHART_TOOL_NAME};

#[cfg(test)]
mod goldens;

/// The media type of a chart's validated spec staged as a file: what a reader
/// that draws charts itself is shown in place of the picture.
pub const CHART_SPEC_MEDIA_TYPE: &str = "application/vnd.workforce.chart+json";

/// What a chart is delivered as: PNG is the one chat and email show inline,
/// SVG the vector original.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Png,
    Svg,
}

impl Format {
    pub fn parse(s: &str) -> Option<Self> {
        match s.to_ascii_lowercase().as_str() {
            "png" => Some(Self::Png),
            "svg" => Some(Self::Svg),
            _ => None,
        }
    }

    pub fn media_type(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Svg => "image/svg+xml",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Svg => "svg",
        }
    }
}

/// A chart to draw: the validated spec, how to deliver it, and what to draw
/// it in — a named theme (a workspace brand theme or a built-in) and, over
/// that, an explicit series palette.
#[derive(Debug, Clone)]
pub struct ChartRequest {
    pub spec: ChartSpec,
    pub format: Format,
    pub title: String,
    pub theme: Option<String>,
    pub colors: Vec<String>,
}

/// Draw the request as the bytes of its format, in the named theme resolved
/// against the workspace (the default theme when there is no workspace), with
/// `colors` replacing the theme's series palette when given.
pub async fn draw(request: &ChartRequest, workspace: Option<&Path>) -> Result<Vec<u8>> {
    let named = request.theme.as_deref();
    let mut theme = workspace
        .map(|w| workspace_theme(w, named))
        .unwrap_or_else(|| theme::resolve(named).clone());
    if !request.colors.is_empty() {
        theme.palette.series = request.colors.clone();
    }
    let svg = render::render(&request.spec, &theme);
    match request.format {
        Format::Svg => Ok(svg.into_bytes()),
        Format::Png => rasterize_svg(&svg)
            .await
            .wrap_err("could not rasterize the chart"),
    }
}

/// At most this many series colours, one per series the spec allows.
const MAX_COLORS: usize = spec::MAX_SERIES;

fn hex_colour(v: &Value) -> Option<String> {
    let s = v.as_str()?.trim();
    let digits = s.strip_prefix('#')?;
    (matches!(digits.len(), 3 | 6) && digits.chars().all(|c| c.is_ascii_hexdigit()))
        .then(|| format!("#{digits}"))
}

/// The request shape the tool and the WASM host share: `{spec, format?,
/// title?, theme?, colors?}`. A non-blank `title` overrides the spec's own and
/// names the file; `format` is png unless asked otherwise; `colors` are
/// `#rgb`/`#rrggbb` strings, one per series.
pub fn request(input: &Value) -> Result<ChartRequest, String> {
    let mut spec = spec::validate(input.get("spec").unwrap_or(&Value::Null))?;
    if let Some(title) = spec::optional_string(input.get("title")) {
        spec.title = Some(title);
    }
    let format = match input.get("format").and_then(Value::as_str) {
        None => Format::Png,
        Some(f) => {
            Format::parse(f).ok_or_else(|| format!("unknown format {f:?} — use png or svg"))?
        }
    };
    let colors = match input.get("colors") {
        None | Some(Value::Null) => Vec::new(),
        Some(v) => {
            let raw = v
                .as_array()
                .filter(|a| !a.is_empty() && a.len() <= MAX_COLORS)
                .ok_or_else(|| format!("colors must be 1 to {MAX_COLORS} hex strings"))?;
            raw.iter()
                .map(|c| {
                    hex_colour(c).ok_or_else(|| format!("colors: {c} is not a #rrggbb colour"))
                })
                .collect::<Result<_, _>>()?
        }
    };
    Ok(ChartRequest {
        title: spec.title.clone().unwrap_or_else(|| "chart".to_string()),
        spec,
        format,
        theme: spec::optional_string(input.get("theme")),
        colors,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn request_validates_the_spec_and_defaults_to_png() {
        let req = request(&json!({
            "spec": { "kind": "pie", "title": "Split", "labels": ["a", "b"], "series": [{ "data": [1, 2] }] }
        }))
        .unwrap();
        assert_eq!(req.spec.labels.as_deref().map(<[String]>::len), Some(2));
        assert_eq!((req.format, req.title.as_str()), (Format::Png, "Split"));
        assert!(req.theme.is_none() && req.colors.is_empty());
    }

    #[test]
    fn request_takes_a_title_override_unless_blank() {
        let with = |title: &str, format: Option<&str>| {
            let mut input = json!({ "spec": { "kind": "bar", "title": "Own", "series": [{ "data": [1] }] }, "title": title });
            if let Some(format) = format {
                input["format"] = json!(format);
            }
            request(&input).unwrap()
        };
        let named = with("Named", Some("SVG"));
        assert_eq!((named.format, named.title.as_str()), (Format::Svg, "Named"));
        assert_eq!(with("  ", None).title, "Own");
        let untitled =
            request(&json!({ "spec": { "kind": "bar", "series": [{ "data": [1] }] } })).unwrap();
        assert_eq!(untitled.title, "chart");
    }

    #[test]
    fn request_takes_a_theme_name_and_a_hex_palette() {
        let req = request(&json!({
            "spec": { "kind": "pie", "labels": ["a", "b"], "series": [{ "data": [1, 2] }] },
            "theme": "slate", "colors": ["#2a78d6", " #ABC ", "#1baf7a"]
        }))
        .unwrap();
        assert_eq!(req.theme.as_deref(), Some("slate"));
        assert_eq!(req.colors, ["#2a78d6", "#ABC", "#1baf7a"]);
        let bad = |colors: Value| {
            request(&json!({ "spec": { "kind": "bar", "series": [{ "data": [1] }] }, "colors": colors })).unwrap_err()
        };
        assert!(bad(json!(["blue"])).contains("not a #rrggbb colour"));
        assert!(bad(json!([])).contains("1 to 8"));
        assert!(bad(json!(vec!["#123456"; 9])).contains("1 to 8"));
        assert!(bad(json!("#123456")).contains("hex strings"));
    }

    #[tokio::test]
    async fn draw_honours_the_named_theme_and_the_palette() {
        let base = request(&json!({
            "spec": { "kind": "pie", "labels": ["a", "b"], "series": [{ "data": [1, 2] }] }, "format": "svg"
        }))
        .unwrap();
        let slate = ChartRequest {
            theme: Some("slate".into()),
            ..base.clone()
        };
        let svg = String::from_utf8(draw(&slate, None).await.unwrap()).unwrap();
        assert!(svg.contains(&theme::resolve(Some("slate")).palette.series[0]));
        let blue = ChartRequest {
            colors: vec!["#0A2A6B".into(), "#2a78d6".into()],
            ..base
        };
        let svg = String::from_utf8(draw(&blue, None).await.unwrap()).unwrap();
        assert!(
            svg.contains("fill='#0A2A6B'") && svg.contains("fill='#2a78d6'"),
            "{svg}"
        );
        assert!(
            !svg.contains("fill='#C8964F'"),
            "the espresso second slice colour is gone"
        );
    }

    #[test]
    fn request_rejects_an_invalid_spec_and_an_unknown_format() {
        let err = request(&json!({ "spec": { "kind": "donut" } })).unwrap_err();
        assert!(err.contains("kind must be one of"), "{err}");
        let err = request(
            &json!({ "spec": { "kind": "bar", "series": [{ "data": [1] }] }, "format": "gif" }),
        )
        .unwrap_err();
        assert!(err.contains("unknown format"), "{err}");
    }

    #[tokio::test]
    async fn draw_uses_the_workspace_theme_when_there_is_one() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join("config.toml"),
            "[report]\ndefault_theme = \"mint\"\n",
        )
        .unwrap();
        let req = request(&json!({
            "spec": { "kind": "bar", "labels": ["a"], "series": [{ "data": [1] }] }, "format": "svg"
        }))
        .unwrap();
        let themed = String::from_utf8(draw(&req, Some(dir.path())).await.unwrap()).unwrap();
        let plain = String::from_utf8(draw(&req, None).await.unwrap()).unwrap();
        assert!(themed.starts_with("<svg") && plain.starts_with("<svg"));
        assert_ne!(themed, plain, "the mint palette differs from espresso");
        assert!(themed.contains(&theme::resolve(Some("mint")).palette.secondary));
    }
}
