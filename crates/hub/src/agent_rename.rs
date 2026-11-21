//! Renaming an agent, in one place for every way it's asked for: Desk's
//! settings (`PATCH /api/agents/{id}`), another agent's `agent.update`, and
//! the agent itself with `agent.rename` once its person approves.
//!
//! A rename is refused when another agent on the hub has the name (its case,
//! spaces and Unicode spelling aside). Otherwise the new name goes where the
//! agent reads who it is, not only into its config:
//!
//! - IDENTITY.md's `**Name:**` line;
//! - the sentences Desk writes when it sets an agent up (`You are X.` in
//!   SOUL.md, `As X,` in AGENTS.md, `Greet them as X,` in WELCOME.md) and
//!   the instructions it gives the hub (`You are X, one of the agents …`);
//! - the config overlay's `name`, which every later build reads, and the
//!   running agent's name at once.
//!
//! Identity files are re-read on every turn (by mtime), so the default agent,
//! which the overlay watcher never rebuilds, answers to its new name on its
//! next turn as well. Sentences a person wrote in their own words are theirs
//! and are left alone; [`mentions`] lists where they, or the hub's other
//! agents and jobs, still say the old name.

use std::path::{Path, PathBuf};

use unicode_normalization::UnicodeNormalization;
use workforce_common::locking::LockManager;
use workforce_orchestrator::workflow_store::{EditContext, WorkflowStore, KIND_IDENTITY_FILE};

use crate::hosts::RegistryView;
use crate::serve::AgentService;

/// Said when another agent on the hub has the name.
pub const NAME_TAKEN: &str = "That name is taken.";

/// The longest display name an agent may be given.
pub const AGENT_NAME_MAX_CHARS: usize = 100;

/// Why a rename didn't happen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RenameError {
    /// The name is blank or too long.
    Invalid(String),
    /// Another agent on the hub has it.
    Taken,
    /// No agent of that id serves.
    Missing(String),
    /// A file or the config couldn't be written; nothing was kept.
    Failed(String),
}

impl std::fmt::Display for RenameError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(why) | Self::Failed(why) => f.write_str(why),
            Self::Taken => f.write_str(NAME_TAKEN),
            Self::Missing(id) => write!(f, "no agent '{id}' is serving"),
        }
    }
}

impl std::error::Error for RenameError {}

/// A rename's name, trimmed: 1 to [`AGENT_NAME_MAX_CHARS`] characters on
/// one line. A name goes into the agent's own instructions, so a line
/// break or other control character in it is refused.
pub fn valid_name(name: &str) -> Result<String, RenameError> {
    let name = name.trim();
    if name.chars().any(char::is_control) {
        return Err(RenameError::Invalid(
            "an agent's name is one line of plain text".to_string(),
        ));
    }
    match name.chars().count() {
        1..=AGENT_NAME_MAX_CHARS => Ok(name.to_string()),
        _ => Err(RenameError::Invalid(format!(
            "an agent's name is 1 to {AGENT_NAME_MAX_CHARS} characters"
        ))),
    }
}

/// Characters that show nothing: a name with one reads as without it.
const UNSEEN: &[char] = &[
    '\u{00AD}', '\u{200B}', '\u{200C}', '\u{200D}', '\u{2060}', '\u{FEFF}',
];

/// A name as it reads: its case, spaces and Unicode spelling aside. Desk
/// compares names the same way.
pub fn as_read(name: &str) -> String {
    name.nfkc()
        .filter(|c| !UNSEEN.contains(c))
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

/// Whether an agent other than `own` among `others` (id, name) has `name`.
pub fn taken<'a>(
    name: &str,
    own: &str,
    others: impl IntoIterator<Item = (&'a str, &'a str)>,
) -> bool {
    let said = as_read(name);
    !said.is_empty()
        && others
            .into_iter()
            .any(|(id, other)| id != own && as_read(other) == said)
}

/// How a file reads with a new name: `(text, old, new)`.
type Rewrite = fn(&str, &str, &str) -> String;

/// The identity files a rename rewrites, with how.
const FILES: &[(&str, Rewrite)] = &[
    ("IDENTITY.md", identity_with_name),
    ("SOUL.md", soul_with_name),
    ("AGENTS.md", agents_with_name),
    ("WELCOME.md", welcome_with_name),
];

/// The name IDENTITY.md gives on its `**Name:**` line, if any.
pub fn identity_name(text: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.split_once("**Name:**"))
        .map(|(_, name)| name.trim().to_string())
        .filter(|name| !name.is_empty())
}

/// IDENTITY.md with `new` on its `**Name:**` line (one added when it has
/// none).
pub fn identity_with_name(text: &str, _old: &str, new: &str) -> String {
    let mut found = false;
    let lines: Vec<String> = text
        .split('\n')
        .map(|line| match line.find("**Name:**") {
            Some(at) if !found => {
                found = true;
                format!("{} {new}", &line[..at + "**Name:**".len()])
            }
            _ => line.to_string(),
        })
        .collect();
    if found {
        lines.join("\n")
    } else {
        format!("- **Name:** {new}\n{text}")
    }
}

/// `text` with each line that opens `{lead}{old}{tail}` opening
/// `{lead}{new}{tail}` instead: only the sentences Desk wrote.
fn swap_lead(text: &str, lead: &str, tail: &str, old: &str, new: &str) -> String {
    let from = format!("{lead}{old}{tail}");
    let to = format!("{lead}{new}{tail}");
    text.split('\n')
        .map(|line| match line.strip_prefix(&from) {
            Some(rest) => format!("{to}{rest}"),
            None => line.to_string(),
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// SOUL.md's "You are X. Your human set you up for this…".
pub fn soul_with_name(text: &str, old: &str, new: &str) -> String {
    swap_lead(text, "You are ", ". Your human set you up", old, new)
}

/// AGENTS.md's "As X, your work is this…".
pub fn agents_with_name(text: &str, old: &str, new: &str) -> String {
    swap_lead(text, "As ", ", your work is this", old, new)
}

/// WELCOME.md's "Greet them as X, and say…".
pub fn welcome_with_name(text: &str, old: &str, new: &str) -> String {
    swap_lead(text, "Greet them as ", ", and say", old, new)
}

/// The instructions Desk gives the hub: "You are X, one of the agents the
/// person works with in Desk."
pub fn instructions_with_name(text: &str, old: &str, new: &str) -> String {
    swap_lead(text, "You are ", ", one of the agents", old, new)
}

/// What a rename needs from the hub it runs in.
pub struct Renamer<'a> {
    pub registry: &'a dyn RegistryView,
    /// The base config the overlay sits beside.
    pub config_path: &'a Path,
    pub data_dir: &'a Path,
    pub lock_manager: &'a LockManager,
    pub def_store: Option<&'a WorkflowStore>,
    /// Who asked: the revision author.
    pub actor: Option<&'a str>,
    /// How it was asked (`web`, `agent_tool`).
    pub via: &'a str,
}

/// A file as it was and as the rename leaves it.
struct Written {
    path: PathBuf,
    was: String,
}

/// Held across a rename's check and writes, so two renames to one name
/// can't both pass the check.
static RENAMING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

impl Renamer<'_> {
    /// The display names of every agent on the hub but `own`: serving, not
    /// serving, and configured but not yet picked up (one just made).
    fn others(&self, own: &str) -> Vec<(String, String)> {
        let configured: Vec<(String, String)> =
            workforce_common::settings::WorkforceConfig::load(self.config_path)
                .map(|c| {
                    c.agents
                        .into_iter()
                        .map(|a| {
                            let name = a.name.unwrap_or_else(|| a.id.clone());
                            (a.id, name)
                        })
                        .collect()
                })
                .unwrap_or_default();
        let serving = self.registry.all_services().into_iter().map(|svc| {
            let id = svc.agent_id.as_str().to_string();
            let name = svc.display_name().unwrap_or_else(|| id.clone());
            (id, name)
        });
        let unloaded = self.registry.unloaded().into_iter().map(|u| {
            let name = u.config.name.clone().unwrap_or_else(|| u.config.id.clone());
            (u.config.id, name)
        });
        // A serving agent's name is its live one (a rename shows at once).
        let known: Vec<(String, String)> = serving.chain(unloaded).collect();
        let picked_up_later: Vec<(String, String)> = configured
            .into_iter()
            .filter(|(id, _)| !known.iter().any(|(k, _)| k == id))
            .collect();
        known
            .into_iter()
            .chain(picked_up_later)
            .filter(|(id, _)| id != own)
            .collect()
    }

    /// Renames agent `id` to `name`, in its files, its instructions and its
    /// config, and shows it at once. `instructions`, when given, replace the
    /// agent's (empty removes them); otherwise Desk's sentence in them is
    /// rewritten. Answers with the name as kept (trimmed).
    pub async fn rename(
        &self,
        id: &str,
        name: &str,
        instructions: Option<String>,
    ) -> Result<String, RenameError> {
        let name = valid_name(name)?;
        let _one_at_a_time = RENAMING.lock().await;
        let service = self
            .registry
            .get(id)
            .ok_or_else(|| RenameError::Missing(id.to_string()))?;
        let others = self.others(id);
        if taken(
            &name,
            id,
            others.iter().map(|(i, n)| (i.as_str(), n.as_str())),
        ) {
            return Err(RenameError::Taken);
        }
        let old = service.display_name().unwrap_or_else(|| id.to_string());

        let written = self.write_files(id, &service.workspace, &old, &name).await;
        let written = match written {
            Ok(written) => written,
            Err((written, why)) => {
                undo(&written).await;
                return Err(RenameError::Failed(why));
            }
        };

        let kept = name.clone();
        let new_name = name.clone();
        let saved = crate::config_writer::overlay_update_agent_fields(
            self.config_path,
            self.data_dir,
            id,
            move |entry| {
                entry.insert("name".to_string(), toml::Value::String(kept));
                let prompt = match instructions {
                    Some(given) => Some(given),
                    None => entry
                        .get("system_prompt")
                        .and_then(toml::Value::as_str)
                        .map(|was| instructions_with_name(was, &old, &new_name)),
                };
                match prompt {
                    Some(p) if p.trim().is_empty() => {
                        entry.remove("system_prompt");
                    }
                    Some(p) => {
                        entry.insert("system_prompt".to_string(), toml::Value::String(p));
                    }
                    None => {}
                }
            },
            self.actor,
            self.via,
            self.def_store,
            self.lock_manager,
        )
        .await;
        if let Err(e) = saved {
            undo(&written).await;
            tracing::warn!(agent_id = %id, error = %e, "An agent's new name couldn't be saved to the config overlay");
            return Err(RenameError::Failed("saving the agent failed".to_string()));
        }
        *service.name_override.write() = Some(name.clone());
        tracing::info!(agent_id = %id, via = %self.via, "Agent renamed");
        Ok(name)
    }

    /// Writes the new name into each identity file that says the old one,
    /// recording a revision first. On a failure, answers with the files
    /// written so far and why.
    async fn write_files(
        &self,
        id: &str,
        workspace: &Path,
        old: &str,
        new: &str,
    ) -> Result<Vec<Written>, (Vec<Written>, String)> {
        let mut written = Vec::new();
        for (file, rewrite) in FILES {
            let path = workspace.join(file);
            let Ok(Some(was)) = workforce_common::config_files::read(&path) else {
                // A missing file isn't made: the hub's own defaults apply.
                continue;
            };
            let now = rewrite(&was, old, new);
            if now == was {
                continue;
            }
            let edit = EditContext::new(self.via, self.actor);
            if let Err(e) = crate::agent_config_write::record_revision(
                self.def_store,
                KIND_IDENTITY_FILE,
                &format!("{id}/{file}"),
                &path,
                &now,
                edit,
            )
            .await
            {
                return Err((written, format!("{file}'s history couldn't be kept: {e}")));
            }
            let replaced = {
                let path = path.clone();
                let now = now.clone();
                tokio::task::spawn_blocking(move || {
                    workforce_common::config_files::replace(&path, now.as_bytes())
                })
                .await
                .unwrap_or_else(|e| Err(std::io::Error::other(e)))
            };
            if let Err(e) = replaced {
                tracing::warn!(agent_id = %id, file = %file, error = %e, "A renamed agent's file couldn't be written");
                return Err((written, format!("{file} couldn't be written")));
            }
            written.push(Written { path, was });
        }
        Ok(written)
    }
}

/// A line, in the person's words, that still calls an agent by an old name.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
pub struct Mention {
    /// The agent whose text it is, by id.
    pub agent: String,
    /// A file ("SOUL.md"), "its instructions", or `job “<name>”`.
    pub place: String,
    pub line: String,
}

/// Whose texts [`mentions`] reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Reach {
    /// The renamed agent's own: what the agent itself is told.
    Own,
    /// Every serving agent's, the renamed one's first: an admin's view.
    Hub,
}

pub const MENTIONS_MAX: usize = 50;

const MENTION_LINE_CHARS: usize = 160;

/// A shorter name would be found in nearly every line.
const MENTION_NAME_MIN_CHARS: usize = 2;

/// Whether `line` says `name` as a word, its case aside: "Desk's helper"
/// does, "desktop" doesn't.
fn says_name(line: &str, name: &str) -> bool {
    let (line, name) = (line.to_lowercase(), name.trim().to_lowercase());
    !name.is_empty()
        && line.match_indices(&name).any(|(at, _)| {
            !line[..at]
                .chars()
                .next_back()
                .is_some_and(char::is_alphanumeric)
                && !line[at + name.len()..]
                    .chars()
                    .next()
                    .is_some_and(char::is_alphanumeric)
        })
}

/// The lines of `text` that say `name`, secrets redacted.
fn naming(agent: &str, place: &str, text: &str, name: &str) -> Vec<Mention> {
    if as_read(name).chars().count() < MENTION_NAME_MIN_CHARS {
        return Vec::new();
    }
    text.lines()
        .filter(|l| says_name(l, name))
        .map(|l| Mention {
            agent: agent.to_string(),
            place: place.to_string(),
            line: crate::redact::redact_secrets(l.trim())
                .chars()
                .take(MENTION_LINE_CHARS)
                .collect(),
        })
        .collect()
}

/// Job `name`'s place, as Desk's settings read it.
fn job_place(name: &str) -> String {
    format!("job “{name}”")
}

/// An agent's identity files, instructions and jobs, as (place, text).
async fn texts_of(service: &AgentService, config_path: &Path) -> Vec<(String, String)> {
    let id = service.agent_id.as_str().to_string();
    let workspace = service.workspace.clone();
    let config_path = config_path.to_path_buf();
    let read = tokio::task::spawn_blocking(move || {
        let files = workforce_common::config_files::IDENTITY_FILES
            .iter()
            .filter_map(|f| {
                workforce_common::config_files::read(&workspace.join(f))
                    .ok()
                    .flatten()
                    .map(|text| ((*f).to_string(), text))
            });
        let instructions = workforce_common::settings::WorkforceConfig::load(&config_path)
            .ok()
            .and_then(|c| c.agent_by_id(&id).and_then(|a| a.system_prompt.clone()))
            .map(|p| ("its instructions".to_string(), p));
        files.chain(instructions).collect::<Vec<_>>()
    })
    .await
    .unwrap_or_default();
    let jobs = match &service.scheduler {
        Some(scheduler) => scheduler
            .list_jobs()
            .await
            .iter()
            .filter_map(|job| {
                job.payload
                    .words()
                    .map(|w| (job_place(&job.name), w.to_string()))
            })
            .collect(),
        None => Vec::new(),
    };
    read.into_iter().chain(jobs).collect()
}

/// Where agents still call agent `own` by `old`, at most [`MENTIONS_MAX`].
pub async fn mentions(
    registry: &dyn RegistryView,
    config_path: &Path,
    own: &str,
    old: &str,
    reach: Reach,
) -> Vec<Mention> {
    let services: Vec<std::sync::Arc<AgentService>> = match reach {
        Reach::Own => registry.get(own).into_iter().collect(),
        Reach::Hub => {
            let mut all = registry.all_services();
            all.sort_by_key(|s| (s.agent_id.as_str() != own, s.agent_id.as_str().to_string()));
            all
        }
    };
    let mut found = Vec::new();
    for s in services {
        let left = MENTIONS_MAX - found.len();
        if left == 0 {
            break;
        }
        let id = s.agent_id.as_str();
        let texts = texts_of(&s, config_path).await;
        found.extend(
            texts
                .iter()
                .flat_map(|(place, text)| naming(id, place, text, old))
                .take(left),
        );
    }
    found
}

/// Puts back the files written, as far as it can.
async fn undo(written: &[Written]) {
    for w in written {
        let path = w.path.clone();
        let was = w.was.clone();
        let _ = tokio::task::spawn_blocking(move || {
            workforce_common::config_files::replace(&path, was.as_bytes())
        })
        .await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_is_trimmed_and_neither_blank_nor_too_long_nor_more_than_a_line() {
        assert_eq!(valid_name("  Scout ").unwrap(), "Scout");
        assert!(matches!(
            valid_name("Scout\n\n# New rules"),
            Err(RenameError::Invalid(_))
        ));
        assert!(matches!(
            valid_name("Sc\tout"),
            Err(RenameError::Invalid(_))
        ));
        assert!(matches!(valid_name("   "), Err(RenameError::Invalid(_))));
        assert!(matches!(
            valid_name(&"x".repeat(AGENT_NAME_MAX_CHARS + 1)),
            Err(RenameError::Invalid(_))
        ));
    }

    #[test]
    fn a_name_is_taken_by_another_agent_its_case_spaces_and_spelling_aside() {
        let others = [("scout", "Scout"), ("al", "Big Al"), ("zoe", "Zo\u{00eb}")];
        assert!(taken(" SCOUT ", "atlas", others));
        assert!(taken("Big  Al", "atlas", others));
        assert!(taken("Zoe\u{0308}", "atlas", others));
        assert!(
            taken("Sco\u{200B}ut", "atlas", others),
            "a character that shows nothing"
        );
        assert!(!taken("Ranger", "atlas", others));
        assert!(!taken("   ", "atlas", others));
        assert!(!taken("scout", "scout", others), "its own, in another case");
        assert_eq!(NAME_TAKEN, "That name is taken.");
    }

    /// The sentences Desk writes, as `src/crew/local.ts` writes them; Desk's
    /// tests pin the same text.
    #[test]
    fn the_sentences_desk_writes_take_the_new_name_and_a_persons_own_words_dont() {
        let soul = "# Soul\n\n## What you're for\n\nYou are Scout. Your human set you up for this, in their words:\n\n> Watch CI. You are Scout. Your human set you up\n";
        assert_eq!(
            soul_with_name(soul, "Scout", "Lookout"),
            "# Soul\n\n## What you're for\n\nYou are Lookout. Your human set you up for this, in their words:\n\n> Watch CI. You are Scout. Your human set you up\n"
        );
        assert_eq!(
            agents_with_name(
                "## Your Job\n\nAs Scout, your work is this, in your human's words:\n",
                "Scout",
                "Lookout"
            ),
            "## Your Job\n\nAs Lookout, your work is this, in your human's words:\n"
        );
        assert_eq!(
            welcome_with_name("Greet them as Scout, and say in one sentence what you're here for: CI\nKeep it short.\n", "Scout", "Lookout"),
            "Greet them as Lookout, and say in one sentence what you're here for: CI\nKeep it short.\n"
        );
        assert_eq!(
            instructions_with_name("You are Scout, one of the agents the person works with in Desk.\n\nWhat you are for, in their words:\nCI", "Scout", "Lookout"),
            "You are Lookout, one of the agents the person works with in Desk.\n\nWhat you are for, in their words:\nCI"
        );
        // Another agent's name in a sentence of the same shape stays.
        assert_eq!(
            soul_with_name(
                "You are Scoutmaster. Your human set you up",
                "Scout",
                "Lookout"
            ),
            "You are Scoutmaster. Your human set you up"
        );
    }

    #[test]
    fn a_name_is_said_as_a_word_its_case_aside() {
        assert!(says_name("Ask Desk's helper first.", "Desk"));
        assert!(says_name("ask desk.", "Desk"));
        assert!(says_name("You report to Big Al.", "big al"));
        assert!(!says_name("Open the desktop app.", "Desk"));
        assert!(!says_name("Anything at all.", "  "));
        let found = naming(
            "scout",
            "SOUL.md",
            "# Soul\nDesk runs you.\n\nNothing here.",
            "Desk",
        );
        assert_eq!(
            found,
            [Mention {
                agent: "scout".into(),
                place: "SOUL.md".into(),
                line: "Desk runs you.".into()
            }]
        );
        assert!(naming("a", "p", "A plan, a note.", "A").is_empty());
        assert_eq!(job_place("Nightly"), "job “Nightly”");
        let keyed = concat!(
            "Desk pushes with ghp_EX",
            "AMPLE00000000000000000000000000000 daily."
        );
        let line = &naming("a", "TOOLS.md", keyed, "Desk")[0].line;
        assert!(
            line.contains("[REDACTED GITHUB_PAT]") && !line.contains("AMPLE"),
            "{line}"
        );
        let long = format!("Desk {}", "x".repeat(400));
        assert_eq!(
            naming("a", "p", &long, "Desk")[0].line.chars().count(),
            MENTION_LINE_CHARS
        );
    }

    #[test]
    fn identity_says_its_name_once_one_is_set() {
        assert_eq!(
            identity_name("# Identity\n- **Name:** Kat\n").as_deref(),
            Some("Kat")
        );
        assert_eq!(identity_name("# Identity\n- **Name:**\n"), None);
        assert_eq!(identity_name("# Identity\n"), None);
    }

    #[test]
    fn identity_gets_its_name_line_or_one_is_added() {
        assert_eq!(
            identity_with_name(
                "# Identity\n- **Name:** Scout\n- **Vibe:** calm\n",
                "Scout",
                "Lookout"
            ),
            "# Identity\n- **Name:** Lookout\n- **Vibe:** calm\n"
        );
        assert_eq!(
            identity_with_name("# Identity\n", "Scout", "Lookout"),
            "- **Name:** Lookout\n# Identity\n"
        );
    }
}
