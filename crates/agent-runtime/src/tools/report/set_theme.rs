//! `set_report_theme` — create or update a brand theme in the workspace config
//! so `generate_report` can render in a company's colours and logo. Writes the
//! `[report]` section of `{workspace}/config.toml`; an optional logo is taken
//! from a staged image artifact and persisted under the workspace.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use async_trait::async_trait;
use eyre::Result;
use serde_json::{json, Value};
use workforce_common::settings::{AdmitWorkspace, AgentWorkspaceConfig, BrandThemeConfig};

use super::{resolve_image, slugify};
use crate::artifacts::store::global_store;
use crate::runner::CURRENT_AGENT_ID;
use crate::tools::config_guard;
use crate::tools::paths::CanonicalRoots;
use crate::tools::{Tool, ToolResult};

pub const SET_REPORT_THEME_TOOL_NAME: &str = "set_report_theme";

const DESCRIPTION: &str = "Create or update a reusable brand theme for `generate_report` (company colours \
    + logo), stored in this workspace. Give it a `name`, an optional `base` built-in to inherit \
    (espresso|slate|noir|editorial|mint|sunset — pick `noir` for a dark brand), and override any of the \
    colours (`#rrggbb`): cover_bg (cover/header background), accent (the brand pop), secondary, primary \
    (foreground/title colour — make it light on a dark base), ink, paper, panel, muted, hairline, on_dark, \
    and a `series` list for chart colours. Optionally set a `logo` from a staged image artifact_id (it is \
    saved into the workspace), `sans`/`serif` font names, and `default: true` to make it this workspace's \
    default theme. Afterwards, call generate_report with theme set to this name (or omit theme to use the \
    default).";

const COLOR_KEYS: &[&str] = &[
    "cover_bg",
    "primary",
    "secondary",
    "accent",
    "ink",
    "paper",
    "panel",
    "muted",
    "hairline",
    "on_dark",
];

pub struct SetReportThemeTool {
    workspace: PathBuf,
    admission: Arc<dyn AdmitWorkspace>,
}

impl SetReportThemeTool {
    pub fn new(workspace: PathBuf, admission: Arc<dyn AdmitWorkspace>) -> Self {
        Self {
            workspace,
            admission,
        }
    }
}

#[async_trait]
impl Tool for SetReportThemeTool {
    fn name(&self) -> &str {
        SET_REPORT_THEME_TOOL_NAME
    }

    fn description(&self) -> &str {
        DESCRIPTION
    }

    fn input_schema(&self) -> Value {
        json!({
            "type": "object",
            "required": ["name"],
            "properties": {
                "name": { "type": "string", "description": "Theme name to create or update (e.g. \"harbor\")." },
                "base": { "type": "string", "description": "Built-in theme to inherit (default espresso); use noir for a dark brand." },
                "cover_bg": { "type": "string", "description": "Cover/header background, #rrggbb." },
                "accent": { "type": "string", "description": "Brand accent (the pop colour), #rrggbb." },
                "secondary": { "type": "string", "description": "Secondary accent, #rrggbb." },
                "primary": { "type": "string", "description": "Foreground/title colour, #rrggbb (light on a dark base)." },
                "ink": { "type": "string", "description": "Body text colour, #rrggbb." },
                "paper": { "type": "string", "description": "Page background, #rrggbb." },
                "panel": { "type": "string", "description": "Card background, #rrggbb." },
                "muted": { "type": "string", "description": "Muted/label text, #rrggbb." },
                "hairline": { "type": "string", "description": "Rule/border colour, #rrggbb." },
                "on_dark": { "type": "string", "description": "Text colour on the cover, #rrggbb." },
                "series": { "type": "array", "items": { "type": "string" }, "description": "Chart series colours, #rrggbb each." },
                "sans": { "type": "string", "description": "Sans font family (must be a bundled font; default Lato)." },
                "serif": { "type": "string", "description": "Serif font family (default IBM Plex Serif)." },
                "logo": { "type": "string", "description": "Artifact id of a staged image to use as the brand logo." },
                "default": { "type": "boolean", "description": "Make this the workspace's default theme." }
            }
        })
    }

    async fn execute(&self, input: Value) -> Result<ToolResult> {
        let start = Instant::now();
        let elapsed = |t: Instant| t.elapsed().as_millis() as u64;

        let Some(name) = input["name"]
            .as_str()
            .map(str::trim)
            .filter(|s| !s.is_empty())
        else {
            return Ok(ToolResult::failure(
                "a theme `name` is required",
                elapsed(start),
            ));
        };

        let mut brand = BrandThemeConfig {
            base: opt_str(&input, "base"),
            sans: opt_str(&input, "sans"),
            serif: opt_str(&input, "serif"),
            ..Default::default()
        };
        if let Err(e) = apply_colors(&mut brand, &input) {
            return Ok(ToolResult::failure(e, elapsed(start)));
        }

        let logo_note = match input["logo"].as_str().filter(|s| !s.is_empty()) {
            None => String::new(),
            Some(artifact_id) => match self.save_logo(name, artifact_id) {
                Ok(rel) => {
                    brand.logo = Some(rel.clone());
                    format!(" Logo saved to {rel}.")
                }
                Err(e) => return Ok(ToolResult::failure(e, elapsed(start))),
            },
        };

        let make_default = input["default"].as_bool().unwrap_or(false);
        let stored_name = name.to_string();
        if let Err(e) =
            AgentWorkspaceConfig::update(&self.workspace, self.admission.as_ref(), |cfg| {
                let report = cfg.report.get_or_insert_with(Default::default);
                report.themes.insert(stored_name.clone(), brand);
                if make_default {
                    report.default_theme = Some(stored_name.clone());
                }
            })
        {
            return Ok(ToolResult::failure(
                format!("failed to save the theme: {e}"),
                elapsed(start),
            ));
        }

        let default_note = if make_default {
            " It is now the workspace default."
        } else {
            ""
        };
        Ok(ToolResult::success(
            json!({
                "theme": name,
                "note": format!(
                    "Brand theme `{name}` saved.{logo_note}{default_note} Use it by calling \
                     generate_report with theme set to `{name}`."
                ),
            }),
            elapsed(start),
        ))
    }
}

impl SetReportThemeTool {
    /// Persist the logo artifact's bytes under the workspace and return the
    /// stored path (relative to the workspace) for the brand config.
    fn save_logo(&self, name: &str, artifact_id: &str) -> Result<String, String> {
        let agent_id = CURRENT_AGENT_ID
            .try_with(|v| v.clone())
            .ok()
            .filter(|s| !s.is_empty())
            .ok_or("no agent context in scope; cannot read the logo artifact")?;
        let store = global_store().ok_or("artifact store is not configured")?;
        let logo = resolve_image(store.as_ref(), &agent_id, artifact_id)
            .ok_or("the `logo` artifact is missing, expired, or not an image (png/jpeg/gif/svg)")?;
        let rel = format!("assets/report/{}.{}", slugify(name), logo.ext);
        let roots = CanonicalRoots::new(std::slice::from_ref(&self.workspace));
        let real = roots
            .resolve(&self.workspace.join(&rel))
            .ok_or("the logo's directory leads outside the workspace")?;
        if let Some(dir) = real.parent() {
            config_guard::create_dirs(&roots, dir)
                .map_err(|e| format!("creating logo dir: {e}"))?;
        }
        config_guard::write(&roots, &real, &logo.bytes)
            .map_err(|e| format!("writing logo: {e}"))?;
        Ok(rel)
    }
}

fn opt_str(input: &Value, key: &str) -> Option<String> {
    input[key]
        .as_str()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

fn is_hex(c: &str) -> bool {
    c.len() == 7 && c.starts_with('#') && c[1..].chars().all(|d| d.is_ascii_hexdigit())
}

fn apply_colors(brand: &mut BrandThemeConfig, input: &Value) -> Result<(), String> {
    let set = |key: &str, slot: &mut Option<String>| -> Result<(), String> {
        match input[key].as_str().map(str::trim).filter(|s| !s.is_empty()) {
            None => Ok(()),
            Some(c) if is_hex(c) => {
                *slot = Some(c.to_string());
                Ok(())
            }
            Some(c) => Err(format!("`{key}` must be a #rrggbb colour, got `{c}`")),
        }
    };
    for key in COLOR_KEYS {
        let slot = match *key {
            "cover_bg" => &mut brand.cover_bg,
            "primary" => &mut brand.primary,
            "secondary" => &mut brand.secondary,
            "accent" => &mut brand.accent,
            "ink" => &mut brand.ink,
            "paper" => &mut brand.paper,
            "panel" => &mut brand.panel,
            "muted" => &mut brand.muted,
            "hairline" => &mut brand.hairline,
            "on_dark" => &mut brand.on_dark,
            _ => unreachable!(),
        };
        set(key, slot)?;
    }
    if let Some(series) = input["series"].as_array() {
        let colors: Result<Vec<String>, String> = series
            .iter()
            .map(|v| match v.as_str() {
                Some(c) if is_hex(c) => Ok(c.to_string()),
                other => Err(format!("series colours must be #rrggbb, got `{other:?}`")),
            })
            .collect();
        brand.series = Some(colors?);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_colors_validates_hex_and_sets_fields() {
        let mut brand = BrandThemeConfig::default();
        apply_colors(
            &mut brand,
            &json!({ "accent": "#42D17C", "cover_bg": "#0B2A3A", "series": ["#42D17C", "#EE8AA0"] }),
        )
        .unwrap();
        assert_eq!(brand.accent.as_deref(), Some("#42D17C"));
        assert_eq!(brand.cover_bg.as_deref(), Some("#0B2A3A"));
        assert_eq!(brand.series.as_ref().unwrap().len(), 2);

        let mut bad = BrandThemeConfig::default();
        let err = apply_colors(&mut bad, &json!({ "accent": "green" })).unwrap_err();
        assert!(err.contains("accent") && err.contains("#rrggbb"));
    }
}
