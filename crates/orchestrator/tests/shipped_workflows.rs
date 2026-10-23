//! The definitions shipped in `.driftwerk/workflows/` load through `load_dir`,
//! which only warns — so nothing stops a broken one reaching production. This
//! is the gate: every shipped definition must parse and resolve every reference
//! on every path that can reach it.

use std::path::PathBuf;

use workforce_orchestrator::workflow::definition::WorkflowDefinition;

/// The repo's definitions by default. `WORKFLOW_AUDIT_DIR` points the same gate
/// at a deployed directory, which is how a live host gets checked — a box's
/// `data/workflows` is separate mutable state from the copies in git.
fn shipped_dir() -> PathBuf {
    std::env::var_os("WORKFLOW_AUDIT_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("../..")
                .join(".driftwerk/workflows")
        })
}

#[test]
fn every_shipped_workflow_parses_and_resolves() {
    let dir = shipped_dir();
    let mut files: Vec<PathBuf> = std::fs::read_dir(&dir)
        .unwrap_or_else(|e| panic!("cannot read {}: {e}", dir.display()))
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "toml"))
        .collect();
    files.sort();

    assert!(
        !files.is_empty(),
        "no shipped workflows found in {} — the gate would pass vacuously",
        dir.display()
    );

    let failures: Vec<String> = files
        .iter()
        .filter_map(|path| {
            let name = path.file_name()?.to_string_lossy().into_owned();
            let source = std::fs::read_to_string(path).ok()?;
            match WorkflowDefinition::from_toml(&source) {
                Err(e) => Some(format!("{name}: does not parse — {e:#}")),
                Ok(def) => {
                    let issues = def.authoring_issues();
                    (!issues.is_empty())
                        .then(|| format!("{name}:\n    - {}", issues.join("\n    - ")))
                }
            }
        })
        .collect();

    assert!(
        failures.is_empty(),
        "{} of {} shipped workflows are broken:\n\n{}",
        failures.len(),
        files.len(),
        failures.join("\n\n")
    );
}

/// The worked `decide_first` example in the end-user workflow docs is a
/// definition that saves: an example a reader copies must run.
#[test]
fn the_documented_decide_first_example_saves() {
    let docs = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../../end-user-docs/concepts/workflows.mdx");
    let docs = std::fs::read_to_string(&docs).unwrap();
    let example = docs
        .split("### Deciding First")
        .nth(1)
        .and_then(|section| section.split("```toml\n").nth(1))
        .and_then(|block| block.split("```").next())
        .expect("a worked example under Deciding First");
    let def = WorkflowDefinition::from_toml(example).unwrap();
    assert_eq!(def.authoring_issues(), Vec::<String>::new());
    assert!(def.get_step("classify").unwrap().decide_first.is_some());
}
