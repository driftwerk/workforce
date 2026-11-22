//! Sessions' Linear and Jira calls on their lines ([`crate::issues`]): the
//! session they are made for, bounded in time, handed to where the hub
//! said they go; and the links they may go through.

use std::collections::BTreeSet;
use std::sync::Arc;

use serde_json::Value;
use uuid::Uuid;

use super::{Desk, DeskError};
use crate::board::Sync;
use crate::issues::{self, Asked, Asker, Issues};
use crate::line::{LineTool, TOOLS};

impl Desk {
    /// Installs where sessions' Linear and Jira calls go.
    pub fn issues_with(&self, issues: Arc<dyn Issues>) {
        if self.0.issues.set(issues).is_err() {
            tracing::warn!("sessions' issue calls were given a home twice; the first stays");
        }
    }

    /// The links the session keyed `session` (its id, or `<machine>/<id>`
    /// for one on a machine) may go through: those of the synced boards it
    /// reaches, as the ticket tool reaches them, in the boards' order.
    ///
    /// A machine is one OS account, so what one session on it is given
    /// every other can reach: a machine whose sessions Desk places in one
    /// client's group (a group with its own boss) gives that group's alone
    /// anything, and one with more than one client's gives none.
    pub async fn issue_links(&self, session: &str) -> Result<Vec<Sync>, DeskError> {
        let layout = self.layout();
        let bossed = self.bossed();
        let group = layout
            .group_of(&format!("session:{session}"))
            .map(|g| g.id.clone());
        if let Some((machine, _)) = session.split_once('/') {
            let on_it = format!("session:{machine}/");
            let clients: BTreeSet<&str> = layout
                .groups
                .iter()
                .filter(|g| bossed.contains(&g.id) && g.items.iter().any(|i| i.starts_with(&on_it)))
                .map(|g| g.id.as_str())
                .collect();
            match clients.len() {
                0 => {}
                1 if group.as_deref().is_some_and(|g| clients.contains(g)) => {}
                1 => {
                    return Err(DeskError::Invalid(
                        "This machine runs another client's sessions, so Desk gives this one no Linear or Jira; keep each client's sessions on their own machine.".into(),
                    ))
                }
                _ => {
                    return Err(DeskError::Invalid(
                        "This machine runs sessions of more than one client, so Desk gives none of them Linear or Jira; keep each client's sessions on their own machine.".into(),
                    ))
                }
            }
        }
        Ok(self
            .boards()
            .await?
            .into_iter()
            .filter(|b| {
                crate::board::board_reachable(
                    self.line_reach(),
                    group.as_deref(),
                    b.group.as_deref(),
                    &bossed,
                )
            })
            .filter_map(|b| b.sync)
            .collect())
    }

    /// The line's tools offered to the session `caller`: an issue tool only
    /// when its system is reached from where the session runs, the links
    /// read once.
    pub async fn line_tools(&self, caller: Uuid) -> Vec<&'static LineTool> {
        let offered = match self.0.issues.get() {
            Some(home) => home.offered(&self.asker(caller)).await,
            None => vec![],
        };
        TOOLS
            .iter()
            .filter(|t| issues::system_of_tool(t.name).is_none_or(|s| offered.contains(&s)))
            .collect()
    }

    fn asker(&self, caller: Uuid) -> Asker {
        Asker {
            session: caller,
            name: self
                .view(caller)
                .map_or_else(|_| "a terminal".to_string(), |v| v.label().to_string()),
        }
    }

    /// `asked`, for the session `caller`.
    pub(crate) async fn issue_call(&self, caller: Uuid, asked: Asked) -> Result<Value, String> {
        let system = asked.system.as_str();
        let action = asked.action().map_or("", issues::Action::as_str);
        let Some(home) = self.0.issues.get() else {
            let title = asked.system.title();
            return Err(format!(
                "Desk can't reach {title} from here: this computer's hub doesn't carry {title} calls. Tell the person; restarting Desk sets it up."
            ));
        };
        let asker = self.asker(caller);
        let out = tokio::time::timeout(issues::WAIT, home.issue(&asker, &asked))
            .await
            .unwrap_or_else(|_| Err(issues::no_answer(&asked)));
        match &out {
            Ok(_) => {
                tracing::info!(session = %caller, system, action, "a session's issue call was answered")
            }
            Err(_) => {
                tracing::info!(session = %caller, system, action, "a session's issue call was refused or failed")
            }
        }
        out
    }
}

#[cfg(test)]
#[path = "issues_tests.rs"]
mod tests;
