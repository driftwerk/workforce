//! Which making of an agent this is: a random id kept under the hub's data
//! folder (never the agent's own workspace, which it can write and which an
//! agent made again with the same id gets back), made when first asked for
//! and forgotten whenever an agent of that id is made or deleted. What the
//! person said yes to for an agent (Desk's machine grants) holds for that
//! making alone.

use std::path::{Path, PathBuf};

const FOLDER: &str = "agent-generations";

fn path_of(data_dir: &Path, agent: &str) -> Option<PathBuf> {
    // A relative data folder (`./data`) is the hub's own, from where it runs.
    let data_dir = match data_dir.is_absolute() {
        true => data_dir.to_path_buf(),
        false => std::fs::canonicalize(data_dir).ok()?,
    };
    let ok = !agent.is_empty()
        && agent.len() <= 100
        && agent
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.'))
        && !agent.starts_with('.');
    ok.then(|| data_dir.join(FOLDER).join(agent))
}

/// This making of `agent`: the id kept for it, made now if there is none.
/// None where it can't be read or kept.
pub fn of(data_dir: &Path, agent: &str) -> Option<String> {
    let path = path_of(data_dir, agent)?;
    if let Ok(text) = std::fs::read_to_string(&path) {
        let id = text.trim();
        if uuid::Uuid::parse_str(id).is_ok() {
            return Some(id.to_string());
        }
    }
    let id = uuid::Uuid::new_v4().to_string();
    std::fs::create_dir_all(path.parent()?).ok()?;
    std::fs::write(&path, &id).ok()?;
    Some(id)
}

/// Forgets `agent`'s making: the next one asked for is new.
pub fn forget(data_dir: &Path, agent: &str) {
    let Some(path) = path_of(data_dir, agent) else {
        return;
    };
    match std::fs::remove_file(&path) {
        Ok(()) => {}
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => tracing::warn!(%agent, error = %e, "an agent's making wasn't forgotten"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_making_holds_until_the_agent_is_made_or_deleted_again() {
        let dir = tempfile::tempdir().unwrap();
        let first = of(dir.path(), "sarah").unwrap();
        assert_eq!(of(dir.path(), "sarah").as_deref(), Some(first.as_str()));
        assert_ne!(of(dir.path(), "tom").unwrap(), first);
        forget(dir.path(), "sarah");
        let again = of(dir.path(), "sarah").unwrap();
        assert_ne!(again, first, "made again: another making");
        assert_eq!(of(dir.path(), "../x"), None);
        forget(dir.path(), "nobody");
    }
}
