//! Sessions' Linear and Jira calls on their lines ([`crate::issues`]): the
//! session they are made for, bounded in time, handed to where the hub
//! said they go; and the links they may go through.

use std::sync::Arc;

use serde_json::Value;
use uuid::Uuid;

use super::{Desk, DeskError};
use crate::board::{Sync, System};
use crate::issues::{self, Asked, Asker, Issues};

impl Desk {
    /// Installs where sessions' Linear and Jira calls go.
    pub fn issues_with(&self, issues: Arc<dyn Issues>) {
        if self.0.issues.set(issues).is_err() {
            tracing::warn!("sessions' issue calls were given a home twice; the first stays");
        }
    }

    /// The links of `system` sessions' calls may go through: every synced
    /// board's, in the boards' order.
    pub async fn issue_links(&self, system: System) -> Result<Vec<Sync>, DeskError> {
        Ok(self
            .boards()
            .await?
            .into_iter()
            .filter_map(|b| b.sync)
            .filter(|s| s.system == system)
            .collect())
    }

    /// `asked`, for the session `caller`.
    pub(crate) async fn issue_call(&self, caller: Uuid, asked: Asked) -> Result<Value, String> {
        let system = asked.system.as_str();
        let action = asked.action().map_or("", issues::Action::as_str);
        let Some(home) = self.0.issues.get() else {
            return Err(format!(
                "Desk can't reach {} from here.",
                asked.system.title()
            ));
        };
        let asker = Asker {
            session: caller,
            name: self
                .view(caller)
                .map_or_else(|_| "a terminal".to_string(), |v| v.label().to_string()),
        };
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
