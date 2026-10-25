//! A released migration is never edited: refinery compares each applied
//! migration's checksum with the file it embeds and refuses to start when they
//! differ. `migrations.lock` at the repository root records every migration's
//! SHA-256; a new migration adds a line, an existing line never changes.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn repo_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn collect(dir: &Path, root: &Path, in_migrations: bool, out: &mut BTreeMap<String, String>) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().into_owned();
        if path.is_dir() {
            if name != "target" && name != "node_modules" {
                collect(
                    &path,
                    root,
                    in_migrations || name.starts_with("migrations"),
                    out,
                );
            }
        } else if in_migrations && name.ends_with(".sql") {
            let relative = path
                .strip_prefix(root)
                .unwrap()
                .to_string_lossy()
                .into_owned();
            let digest = format!("{:x}", Sha256::digest(fs::read(&path).unwrap()));
            out.insert(relative, digest);
        }
    }
}

#[test]
fn released_migrations_are_never_edited() {
    let root = repo_root();
    let lock = fs::read_to_string(root.join("migrations.lock")).expect("migrations.lock");
    let locked: BTreeMap<String, String> = lock
        .lines()
        .filter_map(|line| line.split_once("  "))
        .map(|(digest, path)| (path.to_string(), digest.to_string()))
        .collect();
    let mut found = BTreeMap::new();
    collect(&root.join("crates"), &root, false, &mut found);

    let edited: Vec<&String> = locked
        .iter()
        .filter(|(path, digest)| found.get(*path) != Some(*digest))
        .map(|(path, _)| path)
        .collect();
    assert!(
        edited.is_empty(),
        "released migrations changed or disappeared, so refinery would refuse to start \
         against databases that already ran them; restore them exactly: {edited:?}"
    );

    let unlisted: Vec<String> = found
        .iter()
        .filter(|(path, _)| !locked.contains_key(*path))
        .map(|(path, digest)| format!("{digest}  {path}"))
        .collect();
    assert!(
        unlisted.is_empty(),
        "new migrations are missing from migrations.lock; add these lines:\n{}",
        unlisted.join("\n")
    );
}
