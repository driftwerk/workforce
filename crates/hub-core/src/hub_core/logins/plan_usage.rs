//! Where a login's plan readings go: the install's usage store, once it
//! has one.

use std::sync::Arc;

use workforce_common::usage::Window;

use super::resolve::Login;

/// What keeps the readings logins' calls report.
pub trait PlanUsage: Send + Sync {
    fn observe(&self, login: &Login, plan: Option<&str>, windows: Vec<Window>);
}

static OBSERVER: parking_lot::RwLock<Option<Arc<dyn PlanUsage>>> = parking_lot::RwLock::new(None);

/// Keep readings with `observer` from now on.
pub fn observe_with(observer: Arc<dyn PlanUsage>) {
    *OBSERVER.write() = Some(observer);
}

impl Login {
    /// The plan's windows as a call on this login reported them; nothing is
    /// kept of a report with none.
    pub fn observe_usage(&self, plan: Option<&str>, windows: Vec<Window>) {
        if windows.is_empty() {
            return;
        }
        if let Some(observer) = OBSERVER.read().clone() {
            observer.observe(self, plan, windows);
        }
    }
}
