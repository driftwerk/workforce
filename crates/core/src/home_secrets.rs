//! Where secrets and start files are kept under a home folder, and how a
//! case-insensitive disk compares names: what a boss never reads or writes
//! for a session, and what no folder an agent works in may be or hold.

use std::path::{Component, Path, PathBuf};

/// Where secrets are kept under the home folder: never read or written for
/// a boss.
pub const SECRET_DIRS: &[&str] = &[
    ".ssh",
    ".aws",
    ".azure",
    ".gnupg",
    ".kube",
    ".docker",
    ".netrc",
    ".npmrc",
    ".pypirc",
    ".git-credentials",
    ".codex",
    ".cargo/credentials",
    ".cargo/credentials.toml",
    ".config/gh",
    ".config/gcloud",
    "Library/Keychains",
];

/// What starts when a shell, a session or a login does: never written for
/// a boss.
pub const START_FILES: &[&str] = &[
    ".zshrc",
    ".zshenv",
    ".zprofile",
    ".zlogin",
    ".bashrc",
    ".bash_profile",
    ".bash_login",
    ".profile",
    ".gitconfig",
    ".config/git",
    ".config/fish",
    // Devin's config runs its hooks' commands.
    ".config/devin",
    "Library/LaunchAgents",
];

/// Under home, beside the secret and start-file folders, the rest of the
/// person's own configuration and the engines' sign-ins.
pub const KEPT_IN_HOME: &[&str] = &[
    ".config",
    ".claude",
    ".claude.json",
    ".local/share/keyrings",
];

/// Whether `path` is, lies in or holds what the person keeps in `home`
/// ([`SECRET_DIRS`], [`START_FILES`], [`KEPT_IN_HOME`]); a pattern as its
/// last part (`~/*`) holds what its folder does. Names are compared as a
/// case-insensitive disk compares them.
pub fn kept_in_home(path: &Path, home: &Path) -> bool {
    let pattern = path
        .file_name()
        .is_some_and(|n| n.to_string_lossy().contains(['*', '?', '[']));
    let path = match pattern {
        true => path.parent().unwrap_or(path),
        false => path,
    };
    let Ok(rest) = fold(path).strip_prefix(fold(home)).map(Path::to_path_buf) else {
        return false;
    };
    SECRET_DIRS
        .iter()
        .chain(START_FILES)
        .chain(KEPT_IN_HOME)
        .map(|k| fold(Path::new(k)))
        .any(|k| rest.starts_with(&k) || k.starts_with(&rest))
}

/// `path` with every component as a case-insensitive disk (APFS) compares
/// it: compatibility-normalized (NFKC: `ſ` is `s`, `ﬀ` is `ff`) and
/// case-folded, so `.GIT` is `.git` and `~/.ſſh` is `~/.ssh`.
pub fn fold(path: &Path) -> PathBuf {
    use unicode_normalization::UnicodeNormalization;
    path.components()
        .map(|c| match c {
            Component::Normal(n) => {
                let once: String = n
                    .to_string_lossy()
                    .nfkc()
                    .collect::<String>()
                    .to_lowercase();
                PathBuf::from(once.nfkc().collect::<String>())
            }
            other => PathBuf::from(other.as_os_str()),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_person_keeps_in_home_is_told_apart() {
        let home = Path::new("/Users/sarah");
        for kept in [
            "/Users/sarah/.ssh/authorized_keys",
            "/Users/sarah/.ZSHRC",
            "/Users/sarah/Library/LaunchAgents/x.plist",
            "/Users/sarah/.config/anything",
            "/Users/sarah/Library",
            "/Users/sarah/*",
            "/Users/sarah",
        ] {
            assert!(kept_in_home(Path::new(kept), home), "{kept}");
        }
        for not in [
            "/Users/sarah/dev/app/.zshrc",
            "/Users/sarah/dev",
            "/Users/sarah/dev/*",
            "/etc/ssh",
            "/Users/sarahx/.ssh",
        ] {
            assert!(!kept_in_home(Path::new(not), home), "{not}");
        }
    }
}
