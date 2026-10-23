//! Helpers for `build.rs`. A crate that embeds a directory at compile time —
//! `refinery::embed_migrations!`, `rust_embed` — gives Cargo no way to know
//! that directory is an input, so an incremental build reuses the crate with
//! whatever the directory held last time. This is how a merged migration
//! shipped in an image that told refinery there was nothing to apply.

use std::path::Path;

/// Declare `dir` and everything under it as inputs of the calling crate, so
/// adding, removing or editing a file there recompiles the crate. The
/// directory itself is declared even when absent, so its creation counts too.
pub fn track_dir(dir: impl AsRef<Path>) {
    let dir = dir.as_ref();
    println!("cargo:rerun-if-changed={}", dir.display());
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for path in entries.flatten().map(|entry| entry.path()) {
        if path.is_dir() {
            track_dir(&path);
        } else {
            println!("cargo:rerun-if-changed={}", path.display());
        }
    }
}
