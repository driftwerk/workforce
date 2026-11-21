//! Sessions' Linear and Jira calls on their lines ([`crate::issues`]): the
//! session they are made for, bounded in time, handed to where the hub
//! said they go; and the links they may go through.

use std::sync::Arc;

use serde_json::Value;
use uuid::Uuid;

use super::{Desk, DeskError};
use crate::board::{Sync, System};
use crate::issues::{self, Asked, Asker, Issues};
use crate::line::{LineTool, TOOLS};

impl Desk {
    /// Installs where sessions' Linear and Jira calls go.
    pub fn issues_with(&self, issues: Arc<dyn Issues>) {
        if self.0.issues.set(issues).is_err() {
            tracing::warn!("sessions' issue calls were given a home twice; the first stays");
        }
    }

    /// The links of `system` the session keyed `session` (its id, or
    /// `<machine>/<id>` for one on a machine) may go through: those of the
    /// synced boards it reaches, as the ticket tool reaches them, in the
    /// boards' order.
    pub async fn issue_links(&self, system: System, session: &str) -> Result<Vec<Sync>, DeskError> {
        let group = self
            .layout()
            .group_of(&format!("session:{session}"))
            .map(|g| g.id.clone());
        let bossed = self.bossed();
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
            .filter(|s| s.system == system)
            .collect())
    }

    /// The line's tools offered to the session `caller`: an issue tool only
    /// when its system is reached from where the session runs.
    pub async fn line_tools(&self, caller: Uuid) -> Vec<&'static LineTool> {
        let asker = self.asker(caller);
        let mut out = Vec::with_capacity(TOOLS.len());
        for t in &TOOLS {
            let offered = match (issues::system_of_tool(t.name), self.0.issues.get()) {
                (None, _) => true,
                (Some(_), None) => false,
                (Some(system), Some(home)) => home.offers(&asker, system).await,
            };
            if offered {
                out.push(t);
            }
        }
        out
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
