//! Hub-global resolver for consolidation templates.
//!
//! The Xero export tool (in the `skills` crate) needs to fetch a user's stored
//! consolidation template by id at export time, but the owned-defs store that
//! holds it lives in the orchestrator crate, which `skills` does not depend on.
//! So — exactly like [`crate::artifacts::store::global_store`] — the hub installs
//! a resolver once at startup and any tool reads it through a free getter,
//! without threading a store handle through every skill constructor.

use std::sync::Arc;

use once_cell::sync::OnceCell;

/// Resolves a stored consolidation template (as its JSON source) by id, scoped to
/// a user with a shared/global fallback. Implemented by the hub over the
/// owned-defs store; absent in CLI/test runs, where resolution returns `None`.
pub trait ConsolidationTemplateResolver: Send + Sync {
    fn resolve(&self, id: &str, user: Option<&str>) -> Option<String>;
}

static RESOLVER: OnceCell<Arc<dyn ConsolidationTemplateResolver>> = OnceCell::new();

/// Install the hub-wide resolver at startup. Subsequent calls are no-ops — the
/// resolver is constructed once and never swapped under running tools.
pub fn set_consolidation_resolver(resolver: Arc<dyn ConsolidationTemplateResolver>) {
    let _ = RESOLVER.set(resolver);
}

/// Resolve a consolidation template's JSON source by id for `user` (with a
/// shared fallback). `None` when no resolver is installed (tests, CLI) or the id
/// is unknown — callers then fall back to their non-template behaviour.
pub fn resolve_consolidation_template(id: &str, user: Option<&str>) -> Option<String> {
    RESOLVER.get()?.resolve(id, user)
}
