//! Signing an engine's own CLI in where it has no browser: Codex's device
//! sign-in (`codex login --device-auth`), run on a terminal of the hub's
//! own that no one is shown. Its screen is read here for the page to open
//! and the one-time code to type there, which Desk shows on the person's
//! own computer; the CLI waits for the page and keeps the sign-in where its
//! sessions read it. Nothing the CLI signs in with passes through Desk.
//!
//! The code is shown to the person, never logged or announced: Desk reads
//! it by asking for this sign-in.

use std::sync::Arc;
use std::time::Duration;

use uuid::Uuid;

use super::{Desk, DeskError};
use crate::engine::Engine;
use crate::pty::{Pty, Spawn};

/// How long the device sign-in may take: Codex's code lasts 15 minutes.
const SIGN_IN_FOR: Duration = Duration::from_secs(16 * 60);
/// How often its screen is read.
const SIGN_IN_EVERY: Duration = Duration::from_millis(250);
const SIGN_IN_COLS: u16 = 400;
/// How long an ended one can still be read.
const KEPT_FOR: Duration = Duration::from_secs(60);

/// Where an engine's own sign-in is.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(tag = "state", rename_all = "camelCase")]
pub enum EngineSignInState {
    Starting,
    /// Open `url` and type `code` there.
    Device {
        url: String,
        code: String,
    },
    Done,
    Failed {
        why: String,
    },
}

impl EngineSignInState {
    fn is_over(&self) -> bool {
        matches!(self, Self::Done | Self::Failed { .. })
    }
}

/// An engine's own sign-in, as Desk shows it.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct EngineSignInView {
    pub id: Uuid,
    pub engine: Engine,
    #[serde(flatten)]
    pub state: EngineSignInState,
}

pub(super) struct EngineSignInRun {
    view: EngineSignInView,
    pty: Arc<Pty>,
}

/// What a person reads when the device sign-in ended without signing in.
pub const DEVICE_FAILED: &str =
    "ChatGPT's sign-in didn't finish. Start it again, and type the code on the page before it runs out.";
/// What they read when ChatGPT turned device sign-in away.
pub const DEVICE_REFUSED: &str =
    "ChatGPT didn't allow signing in with a code. Turn on device code sign-in in ChatGPT's security settings, then start it again.";

/// What `codex login --device-auth` shows, read from its screen.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct DeviceSaid {
    /// The page to open: only ever one of OpenAI's own.
    pub url: Option<String>,
    /// The one-time code to type there.
    pub code: Option<String>,
    /// It said device sign-in isn't allowed.
    pub refused: bool,
}

/// The hosts the device page may be on.
fn device_host(url: &str) -> bool {
    let Some(rest) = url.strip_prefix("https://") else {
        return false;
    };
    let host = rest
        .split(['/', '?', '#'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase();
    if host.contains(['@', ':']) {
        return false;
    }
    ["openai.com", "chatgpt.com"]
        .iter()
        .any(|d| host == *d || host.ends_with(&format!(".{d}")))
}

/// A one-time code as Codex prints it: groups of capitals and digits
/// joined by `-` (`ABCD-12345`).
fn device_code(word: &str) -> bool {
    let groups: Vec<&str> = word.split('-').collect();
    (2..=3).contains(&groups.len())
        && word.len() <= 20
        && groups.iter().all(|g| {
            (3..=8).contains(&g.len())
                && g.chars()
                    .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
        })
}

/// Reads a `codex login --device-auth` screen.
pub fn device_said(screen: &str) -> DeviceSaid {
    let mut said = DeviceSaid::default();
    let mut after_code_line = false;
    for line in screen.lines() {
        let lower = line.to_lowercase();
        for word in line.split(|c: char| c.is_whitespace() || c == '│' || c == '"' || c == '\'') {
            let word = word.trim_matches(|c: char| matches!(c, '(' | ')' | '<' | '>' | ',' | '.'));
            if said.url.is_none() && device_host(word) {
                said.url = Some(word.to_string());
            }
            if after_code_line && said.code.is_none() && device_code(word) {
                said.code = Some(word.to_string());
            }
        }
        if lower.contains("one-time code") || lower.contains("enter this code") {
            after_code_line = true;
        }
        if lower.contains("device code")
            && (lower.contains("not enabled")
                || lower.contains("disabled")
                || lower.contains("not allowed"))
        {
            said.refused = true;
        }
    }
    said
}

impl Desk {
    /// Starts `engine`'s own sign-in for a computer with no browser: Codex's
    /// device sign-in. One at a time: a new one ends the last.
    pub fn start_engine_sign_in(&self, engine: Engine) -> Result<EngineSignInView, DeskError> {
        if engine != Engine::Codex {
            return Err(DeskError::Invalid(format!(
                "Desk signs {} in another way",
                engine.title()
            )));
        }
        let program = self.program_of(engine)?.to_path_buf();
        let env = self.with_added(&self.0.state.lock().session_env.clone());
        let spec = Spawn {
            program,
            args: vec!["login".into(), "--device-auth".into()],
            cwd: env
                .get("HOME")
                .map(Into::into)
                .unwrap_or_else(std::env::temp_dir),
            env,
        };
        let (pty, exit) =
            Pty::spawn(&spec, SIGN_IN_COLS, crate::pty::DEFAULT_ROWS, 200).map_err(|e| {
                DeskError::Internal(e.wrap_err(format!("starting {}'s sign-in", engine.title())))
            })?;
        let view = EngineSignInView {
            id: Uuid::new_v4(),
            engine,
            state: EngineSignInState::Starting,
        };
        let ended: Vec<Arc<Pty>> = {
            let mut st = self.0.state.lock();
            let older: Vec<Uuid> = st
                .engine_sign_ins
                .iter()
                .filter(|(_, r)| r.view.engine == engine)
                .map(|(id, _)| *id)
                .collect();
            let ended = older
                .iter()
                .filter_map(|id| st.engine_sign_ins.remove(id))
                .map(|r| r.pty)
                .collect();
            st.engine_sign_ins.insert(
                view.id,
                EngineSignInRun {
                    view: view.clone(),
                    pty: Arc::new(pty),
                },
            );
            ended
        };
        ended.iter().for_each(|p| p.kill());
        tokio::spawn(self.clone().follow_engine_sign_in(view.id, exit));
        Ok(view)
    }

    /// Engine sign-in `id`, while it can be read.
    pub fn engine_sign_in(&self, id: Uuid) -> Result<EngineSignInView, DeskError> {
        self.0
            .state
            .lock()
            .engine_sign_ins
            .get(&id)
            .map(|r| r.view.clone())
            .ok_or(DeskError::NotFound)
    }

    /// Ends an engine sign-in the person gave up on.
    pub fn cancel_engine_sign_in(&self, id: Uuid) -> Result<(), DeskError> {
        let run = self.0.state.lock().engine_sign_ins.remove(&id);
        let run = run.ok_or_else(|| DeskError::Conflict("that sign-in has ended".into()))?;
        run.pty.kill();
        Ok(())
    }

    fn engine_sign_in_moved(&self, id: Uuid, state: EngineSignInState) {
        if let Some(run) = self.0.state.lock().engine_sign_ins.get_mut(&id) {
            run.view.state = state;
        }
    }

    /// Reads the sign-in's screen until it ends, then reads the engine
    /// again, so what Desk says of its sign-in is now.
    async fn follow_engine_sign_in(
        self,
        id: Uuid,
        mut exit: tokio::sync::oneshot::Receiver<crate::pty::Exit>,
    ) {
        let started = tokio::time::Instant::now();
        let outcome = loop {
            let Some(pty) = ({
                let st = self.0.state.lock();
                st.engine_sign_ins.get(&id).map(|r| Arc::clone(&r.pty))
            }) else {
                return; // cancelled, or a newer one began
            };
            let said = device_said(&pty.text());
            if let (Some(url), Some(code)) = (said.url.clone(), said.code.clone()) {
                self.engine_sign_in_moved(id, EngineSignInState::Device { url, code });
            }
            tokio::select! {
                code = &mut exit => {
                    // Read the screen it left once more.
                    tokio::time::sleep(SIGN_IN_EVERY).await;
                    let said = device_said(&pty.text());
                    break match code.ok().flatten() {
                        Some(0) => EngineSignInState::Done,
                        _ if said.refused => EngineSignInState::Failed { why: DEVICE_REFUSED.into() },
                        _ => EngineSignInState::Failed { why: DEVICE_FAILED.into() },
                    };
                }
                _ = tokio::time::sleep(SIGN_IN_EVERY) => {}
                _ = self.0.cancel.cancelled() => {
                    pty.kill();
                    return;
                }
            }
            if started.elapsed() > SIGN_IN_FOR {
                pty.kill();
                break EngineSignInState::Failed {
                    why: DEVICE_FAILED.into(),
                };
            }
        };
        let done = outcome == EngineSignInState::Done;
        tracing::info!(engine = "codex", done, "an engine's own sign-in ended");
        self.engine_sign_in_moved(id, outcome);
        if done {
            self.read_engine_now(Engine::Codex).await;
        }
        let desk = self.clone();
        tokio::spawn(async move {
            tokio::time::sleep(KEPT_FOR).await;
            let mut st = desk.0.state.lock();
            if st
                .engine_sign_ins
                .get(&id)
                .is_some_and(|r| r.view.state.is_over())
            {
                st.engine_sign_ins.remove(&id);
            }
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SCREEN: &str = "Welcome to Codex [v0.166.0]\nOpenAI's command-line coding agent\n\nFollow these steps to sign in with ChatGPT using device code authorization:\n\n1. Open this link in your browser and sign in to your account\n   https://auth.openai.com/codex/device\n\n2. Enter this one-time code (expires in 15 minutes)\n   ABCD-12345\n\nContinue only if you started this login in Codex. If a website or another person gave you this code, cancel.\n";

    #[test]
    fn the_device_page_and_its_code_are_read_off_the_screen() {
        let said = device_said(SCREEN);
        assert_eq!(
            said.url.as_deref(),
            Some("https://auth.openai.com/codex/device")
        );
        assert_eq!(said.code.as_deref(), Some("ABCD-12345"));
        assert!(!said.refused);
        // Until the code is drawn, there is none; a page on another host is never one.
        assert_eq!(
            device_said(&SCREEN[..SCREEN.find("ABCD").unwrap()]).code,
            None
        );
        let elsewhere = SCREEN.replace("auth.openai.com", "openai.com.evil.example");
        assert_eq!(device_said(&elsewhere).url, None);
        assert!(device_said("Error: device code login is not enabled for this workspace").refused);
    }

    #[test]
    fn only_a_code_shaped_word_is_a_code() {
        assert!(device_code("ABCD-12345"));
        assert!(device_code("XY12-AB3C"));
        assert!(!device_code("abcd-12345"));
        assert!(!device_code("ABCD"));
        assert!(!device_code("ABCD-12345-6789-0000"));
        assert!(!device_code("15-minutes"));
    }

    #[test]
    fn a_sign_in_reaches_desk_in_its_wire_shape() {
        let view = EngineSignInView {
            id: Uuid::nil(),
            engine: Engine::Codex,
            state: EngineSignInState::Device {
                url: "https://auth.openai.com/codex/device".into(),
                code: "ABCD-12345".into(),
            },
        };
        let v = serde_json::to_value(&view).unwrap();
        assert_eq!(v["engine"], "codex");
        assert_eq!(v["state"], "device");
        assert_eq!(v["code"], "ABCD-12345");
        let failed = EngineSignInView {
            state: EngineSignInState::Failed { why: "x".into() },
            ..view
        };
        assert_eq!(serde_json::to_value(&failed).unwrap()["why"], "x");
    }
}
