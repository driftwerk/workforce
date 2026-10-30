use std::path::Path;

/// The hub embeds `../../ui/dist` into the binary via `rust_embed` at compile
/// time (see the `RustEmbed` derive in `gateway.rs`). Cargo has no way to know
/// that folder is an input to this crate, so a rebuilt SPA does NOT invalidate
/// the hub crate: an incremental build re-links the previously embedded bytes
/// and ships a stale UI even though `ui/dist` on disk is fresh. (This is how a
/// merged, green-deployed frontend change can fail to appear in production.)
///
/// Emit `rerun-if-changed` for every file under `ui/dist` so any change to the
/// built bundle forces this crate — and therefore the embed — to recompile.
///
/// The folder is created when absent so a Rust-only build (no `make ui`) still
/// compiles; the hub then answers UI routes with "UI not built".
fn main() {
    workforce_build_support::track_dir("migrations");
    workforce_build_support::track_dir("migrations_scheduler");
    workforce_build_support::track_dir("migrations_store");
    workforce_build_support::track_dir("migrations_know_how");
    workforce_build_support::track_dir("migrations_decision");
    workforce_build_support::track_dir("migrations_floor");
    workforce_build_support::track_dir("migrations_surface");
    let dist = Path::new("../../ui/dist");
    if !dist.exists() {
        let _ = std::fs::create_dir_all(dist);
    }
    track(Path::new("../../end-user-docs"));
    // Watch the directory itself so added/removed chunks (vite content-hashed
    // filenames) are caught even when the dir is currently empty or absent.
    println!("cargo:rerun-if-changed={}", dist.display());
    track(dist);
}

fn track(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        println!("cargo:rerun-if-changed={}", path.display());
        if path.is_dir() {
            track(&path);
        }
    }
}
