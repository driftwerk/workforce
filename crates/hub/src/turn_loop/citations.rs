//! The citations a turn carries from the training corpus.

pub(crate) fn build_training_doc_map(
    store: &dyn workforce_training::TrainingStore,
    agent_id: &str,
) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    let Ok(corpora) = store.list_corpora(agent_id) else {
        return map;
    };
    for corpus in corpora {
        let Ok(docs) = store.list_documents(corpus.id) else {
            continue;
        };
        for doc in docs {
            map.insert(crate::training::doc_short_id(doc.id), doc.original_filename);
        }
    }
    map
}

/// Build the per-turn citation log from a recall result. Pairs each
/// (memory, score) with a filename (resolved via the doc-tag short id
/// against `doc_map`) and a corpus tag, falling back to placeholders
/// when either is missing.
pub(crate) fn build_turn_citations(
    kept: &[(workforce_memory::types::Memory, f32)],
    doc_map: &std::collections::HashMap<String, String>,
) -> crate::training::TurnCitations {
    kept.iter()
        .map(|(m, score)| {
            let short_id = m
                .tags
                .iter()
                .find_map(|t| t.strip_prefix(crate::training::DOC_TAG_PREFIX))
                .unwrap_or("")
                .to_string();
            let corpus = m
                .tags
                .iter()
                .find_map(|t| t.strip_prefix(crate::training::CORPUS_TAG_PREFIX))
                .unwrap_or("")
                .to_string();
            let locator = m
                .tags
                .iter()
                .find_map(|t| t.strip_prefix("page:").map(|p| format!("p.{p}")))
                .unwrap_or_default();
            let filename = doc_map
                .get(&short_id)
                .cloned()
                .unwrap_or_else(|| "(unknown)".to_string());
            crate::training::Citation {
                short_id,
                filename,
                corpus,
                locator,
                score: *score,
                snippet: crate::training::make_snippet(&m.content),
            }
        })
        .collect()
}
