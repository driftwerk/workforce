//! Desk's own folder on a hub that runs inside Desk, and what lives in it.
//! Every agent's tools are kept from it (`desk_computer`'s withheld set).

use std::path::{Path, PathBuf};

use workforce_common::settings::WorkforceConfig;

/// Desk's own folder, where the hub's data is Desk's (`…/Desk/hub/data`):
/// two up from it, real. `None` on any other hub.
pub fn of(config: &WorkforceConfig) -> Option<PathBuf> {
    let data = workforce_agent::tools::paths::resolve_real(Path::new(&config.hub.data_dir));
    data.parent()
        .filter(|hub| hub.file_name().is_some_and(|n| n == "hub"))
        .and_then(Path::parent)
        .map(Path::to_path_buf)
}

/// Where Desk's browser keeps its profiles: `…/Desk/browser`, inside the
/// folder every agent is kept from. `None` on a hub that isn't Desk's.
pub fn browser_profiles(config: &WorkforceConfig) -> Option<PathBuf> {
    config
        .desk
        .enabled
        .then(|| of(config))
        .flatten()
        .map(|desk| desk.join("browser"))
}
