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
