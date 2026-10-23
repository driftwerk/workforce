//! Code vectors are searched by meaning only (track 2, W0): no source in
//! the workspace that names the code collections asks vellev a keyword
//! (BM25) or hybrid question, so their schema need keep no keyword index.
//! The store's own requests are checked in `vector::vellev`'s tests; this
//! keeps any other crate from growing such a question of them.

use std::path::{Path, PathBuf};

/// What asks vellev a keyword or hybrid question.
const LEXICAL: [&str; 4] = [".keyword(", ".hybrid(", "Keyword {", "Hybrid {"];

/// What names the code collections.
const CODE: [&str; 4] = [
    "CODE_ENTITY_COLLECTION",
    "CODE_SUMMARY_COLLECTION",
    "\"CodeEntity\"",
    "\"CodeSummary\"",
];

fn sources(dir: &Path, found: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name();
        if path.is_dir() {
            if name != "target" && name != "node_modules" {
                sources(&path, found);
            }
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            found.push(path);
        }
    }
}

/// The files that name a code collection and ask a lexical question.
fn offending(crates: &Path) -> Vec<String> {
    let mut found = Vec::new();
    sources(crates, &mut found);
    found
        .into_iter()
        .filter(|path| !path.ends_with("tests/no_lexical_code_search.rs"))
        .filter_map(|path| {
            let text = std::fs::read_to_string(&path).ok()?;
            let code = CODE.iter().any(|name| text.contains(name));
            let lexical = LEXICAL.iter().any(|asked| text.contains(asked));
            (code && lexical).then(|| path.display().to_string())
        })
        .collect()
}

#[test]
fn no_source_asks_the_code_collections_a_keyword_or_hybrid_question() {
    let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
    let mut scanned = Vec::new();
    sources(&crates, &mut scanned);
    assert!(
        scanned.len() > 100,
        "the workspace's crates were read: {} files",
        scanned.len()
    );
    let offending = offending(&crates);
    assert!(
        offending.is_empty(),
        "sources naming the code collections ask a keyword or hybrid question: {offending:#?}"
    );
}

#[test]
fn a_lexical_question_of_a_code_collection_is_found() {
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("asks.rs");
    std::fs::write(
        &file,
        "let k = Keyword { text }; records(CODE_ENTITY_COLLECTION).keyword(&k);",
    )
    .unwrap();
    std::fs::write(dir.path().join("memories.rs"), "records.hybrid(&h);").unwrap();
    assert_eq!(offending(dir.path()), [file.display().to_string()]);
}
