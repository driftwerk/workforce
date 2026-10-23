//! Playbook definition — parsed from TOML files.
//!
//! A playbook is an ordered list of step prompts injected, one at a time, into
//! a user's persistent chat session and gated by Continue/Exit. Unlike a
//! workflow (fresh agent per step + variable handoff), a playbook keeps one
//! live session; this struct only describes *what to say at each step*.

use std::collections::HashSet;
use std::fs;
use std::path::Path;

use eyre::{Result, WrapErr};
use serde::{Deserialize, Serialize};

/// A complete playbook definition parsed from TOML.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookDefinition {
    pub playbook: PlaybookMeta,
    #[serde(default, rename = "step")]
    pub steps: Vec<PlaybookStep>,
}

/// Top-level playbook metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookMeta {
    pub id: String,
    pub name: String,
    #[serde(default)]
    pub description: String,
}

/// A single step: a prompt injected as a turn, optionally producing a pinned
/// artifact file that later steps can read.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookStep {
    pub id: String,
    /// Short human label, shown on the `▶ Step N` divider and in the prompt.
    pub title: String,
    /// The instruction injected into the session for this step.
    pub prompt: String,
    /// Filename (within the session scratch dir) this step should write its
    /// output to. When set, a write-it instruction is auto-appended to the
    /// prompt and the file is pinned on Continue.
    #[serde(default)]
    pub artifact: Option<String>,
    /// Custom buttons offered on this step, rendered *above* the standard
    /// Continue/Exit row. Each jumps to another step (`goto`) or terminates
    /// (`exit`/`finish`) — this is how a playbook branches.
    #[serde(default, rename = "button")]
    pub buttons: Vec<PlaybookButton>,
    /// Whether the default linear `Continue` button is shown. Defaults to
    /// `true`. Set `false` on a forced-choice step (e.g. "Mac or Linux?") where
    /// only the custom buttons make sense; `Exit` is still always present.
    #[serde(default = "default_true")]
    pub keep_continue: bool,
    /// Optional per-step model override — a tier name (`"powerful"`) or a
    /// provider-routed tier (`"openrouter:anthropic:powerful"`). When set, the
    /// step runs on this model instead of the session's normal resolution;
    /// when absent the step uses whatever the session would normally use.
    /// Mirrors `WorkflowStep::model`.
    #[serde(default)]
    pub model: Option<String>,
}

fn default_true() -> bool {
    true
}

/// A custom button on a step. Exactly one of `goto` / `exit` / `finish` must be
/// set — see [`PlaybookButton::effect`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlaybookButton {
    /// Button label (emoji allowed, e.g. `🍎 Mac` / `✅ Approve`).
    pub label: String,
    /// Slack button style; defaults to the neutral grey.
    #[serde(default)]
    pub style: ButtonStyle,
    /// Jump to this step id.
    #[serde(default)]
    pub goto: Option<String>,
    /// Terminate the playbook as *abandoned* (same as the Exit control).
    #[serde(default)]
    pub exit: bool,
    /// Terminate the playbook as *complete* (posts the ✅ done message).
    #[serde(default)]
    pub finish: bool,
}

/// Slack button styling. Block Kit only offers these three — grey (default),
/// green (`primary`) and red (`danger`); there is no per-button colour control.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ButtonStyle {
    #[default]
    Default,
    Primary,
    Danger,
}

/// What a [`PlaybookButton`] does when clicked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ButtonEffect {
    /// Advance to the step with this id.
    Goto(String),
    /// Terminate the playbook (abandoned).
    Exit,
    /// Terminate the playbook (completed).
    Finish,
}

impl PlaybookButton {
    /// Resolve the button's effect, enforcing exactly-one-of goto/exit/finish.
    pub fn effect(&self) -> Result<ButtonEffect> {
        match (self.goto.as_deref(), self.exit, self.finish) {
            (Some(target), false, false) => Ok(ButtonEffect::Goto(target.to_string())),
            (None, true, false) => Ok(ButtonEffect::Exit),
            (None, false, true) => Ok(ButtonEffect::Finish),
            _ => Err(eyre::eyre!(
                "button '{}' must set exactly one of goto / exit / finish",
                self.label
            )),
        }
    }
}

impl PlaybookDefinition {
    /// Parse a playbook definition from a TOML string.
    pub fn from_toml(toml_str: &str) -> Result<Self> {
        let def: Self = toml::from_str(toml_str).wrap_err("Failed to parse playbook TOML")?;
        def.validate()?;
        Ok(def)
    }

    /// Load a playbook definition from a TOML file.
    pub fn from_file(path: &Path) -> Result<Self> {
        let content = fs::read_to_string(path)
            .wrap_err_with(|| format!("Failed to read {}", path.display()))?;
        Self::from_toml(&content)
    }

    /// Load every playbook definition in a directory, skipping invalid files.
    pub fn load_dir(dir: &Path) -> Result<Vec<Self>> {
        if !dir.exists() {
            return Ok(vec![]);
        }
        let mut defs = Vec::new();
        for entry in fs::read_dir(dir)? {
            let path = entry?.path();
            if path.extension().is_some_and(|e| e == "toml") {
                match Self::from_file(&path) {
                    Ok(def) => defs.push(def),
                    Err(e) => {
                        tracing::warn!(path = %path.display(), error = %e, "Skipping invalid playbook");
                    }
                }
            }
        }
        Ok(defs)
    }

    /// Validate the definition: at least one step, unique step ids, non-empty
    /// prompts, and well-formed buttons (exactly one effect each, `goto`
    /// targets that exist).
    pub fn validate(&self) -> Result<()> {
        if self.steps.is_empty() {
            return Err(eyre::eyre!("Playbook '{}' has no steps", self.playbook.id));
        }
        let ids: HashSet<&str> = self.steps.iter().map(|s| s.id.as_str()).collect();
        let mut seen = HashSet::new();
        for step in &self.steps {
            if step.prompt.trim().is_empty() {
                return Err(eyre::eyre!(
                    "Playbook '{}' step '{}' has an empty prompt",
                    self.playbook.id,
                    step.id
                ));
            }
            if !seen.insert(step.id.as_str()) {
                return Err(eyre::eyre!(
                    "Playbook '{}' has duplicate step id '{}'",
                    self.playbook.id,
                    step.id
                ));
            }
            // A `goto` re-injects `cmd: goto <nonce> <step-id>` (space-delimited)
            // and the `$exit`/`$finish` control tokens ride in the same button
            // value slot, so restrict step ids to a plain identifier charset:
            // this rules out whitespace (which would truncate a goto target) and
            // any `$` collision in one check.
            if step.id.is_empty()
                || !step
                    .id
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
            {
                return Err(eyre::eyre!(
                    "Playbook '{}' step id '{}' must be non-empty and use only \
                     letters, digits, '_' or '-'",
                    self.playbook.id,
                    step.id
                ));
            }
            // A forced-choice step with no buttons offers only Exit — a dead end.
            if !step.keep_continue && step.buttons.is_empty() {
                return Err(eyre::eyre!(
                    "Playbook '{}' step '{}' sets keep_continue = false but has no \
                     buttons, leaving no way forward",
                    self.playbook.id,
                    step.id
                ));
            }
            for button in &step.buttons {
                if button.label.trim().is_empty() {
                    return Err(eyre::eyre!(
                        "Playbook '{}' step '{}' has a button with an empty label",
                        self.playbook.id,
                        step.id
                    ));
                }
                // Enforce exactly-one-effect, and that any goto target exists.
                match button.effect().map_err(|e| {
                    eyre::eyre!("Playbook '{}' step '{}': {e}", self.playbook.id, step.id)
                })? {
                    ButtonEffect::Goto(target) if !ids.contains(target.as_str()) => {
                        return Err(eyre::eyre!(
                            "Playbook '{}' step '{}' button '{}' jumps to unknown step '{}'",
                            self.playbook.id,
                            step.id,
                            button.label,
                            target
                        ));
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"
[playbook]
id = "design-impl-review"
name = "Design → Implement → Review"
description = "Human-in-the-loop feature build."

[[step]]
id = "design"
title = "Design"
artifact = "design.md"
prompt = "Design the implementation."

[[step]]
id = "implement"
title = "Implement"
prompt = "Implement it."
"#;

    #[test]
    fn parses_valid_playbook() {
        let def = PlaybookDefinition::from_toml(SAMPLE).unwrap();
        assert_eq!(def.playbook.id, "design-impl-review");
        assert_eq!(def.steps.len(), 2);
        assert_eq!(def.steps[0].artifact.as_deref(), Some("design.md"));
        assert_eq!(def.steps[1].artifact, None);
        // `model` defaults to None when the step omits it.
        assert_eq!(def.steps[0].model, None);
    }

    #[test]
    fn parses_optional_step_model() {
        let toml = r#"
[playbook]
id = "x"
name = "X"
[[step]]
id = "think"
title = "Think"
prompt = "Reason hard."
model = "openrouter:anthropic:powerful"
[[step]]
id = "do"
title = "Do"
prompt = "Just do it."
"#;
        let def = PlaybookDefinition::from_toml(toml).unwrap();
        assert_eq!(
            def.steps[0].model.as_deref(),
            Some("openrouter:anthropic:powerful")
        );
        assert_eq!(def.steps[1].model, None);
    }

    #[test]
    fn rejects_no_steps() {
        let toml = "[playbook]\nid=\"x\"\nname=\"X\"\n";
        assert!(PlaybookDefinition::from_toml(toml).is_err());
    }

    #[test]
    fn rejects_duplicate_step_ids() {
        let toml = r#"
[playbook]
id = "x"
name = "X"
[[step]]
id = "a"
title = "A"
prompt = "p"
[[step]]
id = "a"
title = "A2"
prompt = "p2"
"#;
        assert!(PlaybookDefinition::from_toml(toml).is_err());
    }

    #[test]
    fn rejects_empty_prompt() {
        let toml = r#"
[playbook]
id = "x"
name = "X"
[[step]]
id = "a"
title = "A"
prompt = "   "
"#;
        assert!(PlaybookDefinition::from_toml(toml).is_err());
    }

    const BRANCHING: &str = r#"
[playbook]
id = "branchy"
name = "Branchy"

[[step]]
id = "review"
title = "Review"
prompt = "Review it."
keep_continue = false

  [[step.button]]
  label = "✅ Approve"
  style = "primary"
  goto = "merge"

  [[step.button]]
  label = "↩️ Needs work"
  style = "danger"
  goto = "review"

  [[step.button]]
  label = "Bail"
  exit = true

[[step]]
id = "merge"
title = "Merge"
prompt = "Merge it."

  [[step.button]]
  label = "Done"
  finish = true
"#;

    #[test]
    fn parses_buttons_and_keep_continue() {
        let def = PlaybookDefinition::from_toml(BRANCHING).unwrap();
        let review = &def.steps[0];
        assert!(!review.keep_continue);
        assert_eq!(review.buttons.len(), 3);
        assert_eq!(review.buttons[0].style, ButtonStyle::Primary);
        assert_eq!(
            review.buttons[0].effect().unwrap(),
            ButtonEffect::Goto("merge".into())
        );
        assert_eq!(review.buttons[2].effect().unwrap(), ButtonEffect::Exit);
        assert_eq!(
            def.steps[1].buttons[0].effect().unwrap(),
            ButtonEffect::Finish
        );
        // keep_continue defaults to true when omitted.
        assert!(def.steps[1].keep_continue);
    }

    #[test]
    fn rejects_goto_to_unknown_step() {
        let toml = r#"
[playbook]
id = "x"
name = "X"
[[step]]
id = "a"
title = "A"
prompt = "p"
  [[step.button]]
  label = "Go"
  goto = "nowhere"
"#;
        assert!(PlaybookDefinition::from_toml(toml).is_err());
    }

    #[test]
    fn rejects_button_with_two_effects() {
        let toml = r#"
[playbook]
id = "x"
name = "X"
[[step]]
id = "a"
title = "A"
prompt = "p"
  [[step.button]]
  label = "Go"
  goto = "a"
  exit = true
"#;
        assert!(PlaybookDefinition::from_toml(toml).is_err());
    }

    #[test]
    fn rejects_button_with_no_effect() {
        let toml = r#"
[playbook]
id = "x"
name = "X"
[[step]]
id = "a"
title = "A"
prompt = "p"
  [[step.button]]
  label = "Nope"
"#;
        assert!(PlaybookDefinition::from_toml(toml).is_err());
    }

    #[test]
    fn rejects_dead_end_forced_choice_step() {
        let toml = r#"
[playbook]
id = "x"
name = "X"
[[step]]
id = "a"
title = "A"
prompt = "p"
keep_continue = false
"#;
        assert!(PlaybookDefinition::from_toml(toml).is_err());
    }

    #[test]
    fn rejects_dollar_prefixed_step_id() {
        let toml = r#"
[playbook]
id = "x"
name = "X"
[[step]]
id = "$exit"
title = "A"
prompt = "p"
"#;
        assert!(PlaybookDefinition::from_toml(toml).is_err());
    }

    #[test]
    fn rejects_step_id_with_whitespace() {
        // A space in the id would truncate a `goto <id>` directive to its first
        // word, silently breaking branching to this step.
        let toml = r#"
[playbook]
id = "x"
name = "X"
[[step]]
id = "code review"
title = "A"
prompt = "p"
"#;
        assert!(PlaybookDefinition::from_toml(toml).is_err());
    }
}
