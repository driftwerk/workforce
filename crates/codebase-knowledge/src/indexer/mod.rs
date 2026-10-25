pub mod crawler;
pub mod fill;
pub mod incremental;
pub mod parser;
pub mod relationship_builder;
pub mod summary_generator;
pub mod symbol_extractor;

use eyre::Result;
use std::fmt::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

use crate::graph::entities::{CodeEntity, EntityId, EntityKind};
use crate::graph::relationships::Relationship;
use crate::graph::CodeGraph;
use crate::lease::{IndexPhase, LeaseContext, LeaseProgress};
use crate::vector::{CodeVectorStore, VectorCodeEntity, VectorCodeSummary};
use crawler::{CrawledFile, CrawlerConfig, FileCrawler};
use incremental::IncrementalDetector;
use parser::AstParser;
use relationship_builder::RelationshipBuilder;
use summary_generator::{SummaryGenerator, SummaryGranularity};
use symbol_extractor::{CallReference, ImplTraitInfo, SymbolExtractor};

/// Options bundle for [`IndexingPipeline`] runs. Defaults to "no
/// progress channel, no lease" — fine for tests and one-off scripts.
/// Production callers (the `KnowledgeService` trigger paths) wire up a
/// lease so the run is observable, resumable, and steal-safe.
#[derive(Default)]
pub struct RunOptions {
    pub progress_tx: Option<mpsc::Sender<IndexProgress>>,
    pub lease: Option<Arc<LeaseContext>>,
}

impl RunOptions {
    pub fn with_progress(mut self, progress_tx: mpsc::Sender<IndexProgress>) -> Self {
        self.progress_tx = Some(progress_tx);
        self
    }

    pub fn with_lease(mut self, lease: Arc<LeaseContext>) -> Self {
        self.lease = Some(lease);
        self
    }
}

/// Helper: drive one phase transition on the lease (if present).
async fn report_phase(lease: &Option<Arc<LeaseContext>>, phase: IndexPhase) {
    if let Some(l) = lease {
        l.set_phase(phase).await;
    }
}

/// Helper: push progress counters to the lease (if present).
async fn report_progress(lease: &Option<Arc<LeaseContext>>, update: LeaseProgress) {
    if let Some(l) = lease {
        l.set_progress(update).await;
    }
}

/// Progress events emitted during indexing.
#[derive(Debug, Clone)]
pub enum IndexProgress {
    Started {
        total_files: usize,
    },
    FileIndexed {
        path: String,
        entities: usize,
    },
    RelationshipsBuilt {
        count: usize,
    },
    SummariesGenerated {
        count: usize,
    },
    Completed {
        duration_ms: u64,
        entities: usize,
        relationships: usize,
    },
    Error {
        message: String,
    },
}

/// Configuration for the indexing pipeline.
#[derive(Debug, Clone)]
pub struct IndexingConfig {
    /// Root directory to index.
    pub root: PathBuf,
    /// Repository ID (for multi-repo support).
    pub repo_id: String,
    /// Repository name.
    pub repo_name: String,
    /// Crawler configuration.
    pub crawler: CrawlerConfig,
    /// Number of concurrent file parsing workers.
    pub parse_concurrency: usize,
    /// Number of concurrent LLM summary calls. Network-bound, so can be
    /// much higher than `parse_concurrency`.
    pub summary_concurrency: usize,
    /// Whether to generate summaries during indexing.
    pub generate_summaries: bool,
}

impl Default for IndexingConfig {
    fn default() -> Self {
        Self {
            root: PathBuf::from("."),
            repo_id: String::new(),
            repo_name: String::new(),
            crawler: CrawlerConfig::default(),
            parse_concurrency: num_cpus::get() * 2,
            summary_concurrency: 16,
            generate_summaries: false,
        }
    }
}

/// Owned-data record for one summary job, queued onto the
/// `buffer_unordered` stream. Materialising owned `String`s upfront keeps
/// the per-entity future free of borrows from the entity slice — the
/// `Send` bound `buffer_unordered` requires would otherwise fail to
/// resolve across the higher-ranked lifetime.
struct SummaryJob {
    entity_id: EntityId,
    qualified_name: String,
    granularity: SummaryGranularity,
    source: String,
    context: String,
    /// blake3 of the exact LLM input (`source` + `context`). Stored alongside
    /// the summary so a later resume can skip regenerating an unchanged input.
    source_hash: String,
}

/// Maximum number of child signatures — and maximum total characters — fed
/// into a file/module summary. Bounds the input cost on very large files
/// while still giving the model real material to summarise.
const MAX_CHILD_SIGNATURES: usize = 60;
const MAX_CHILD_SIGNATURE_CHARS: usize = 6000;

/// Build the [`SummaryJob`]s for an entity set.
///
/// Applies [`CodeEntity::is_summarisable`] (markdown/data files and trivial
/// private helpers are dropped) and assembles real input per granularity:
/// functions use their signature + doc-comment; files and modules use the
/// signatures of the symbols they contain rather than their bare path —
/// without that, a file/module summary hallucinates from a filename alone and
/// pollutes semantic retrieval.
fn build_summary_jobs(entities: &[CodeEntity]) -> Vec<SummaryJob> {
    use std::collections::HashMap;

    // Children indexed two ways: by file path (for File summaries) and by
    // parent id (for Module summaries).
    let mut by_file: HashMap<&str, Vec<&CodeEntity>> = HashMap::new();
    let mut by_parent: HashMap<&str, Vec<&CodeEntity>> = HashMap::new();
    for e in entities {
        by_file.entry(e.file_path.as_str()).or_default().push(e);
        if let Some(parent) = &e.parent_id {
            by_parent.entry(parent.as_str()).or_default().push(e);
        }
    }

    entities
        .iter()
        .filter(|e| e.is_summarisable())
        .filter_map(|e| {
            let (granularity, source) = match e.kind {
                EntityKind::Function | EntityKind::Method => (
                    SummaryGranularity::Function,
                    e.signature
                        .clone()
                        .unwrap_or_else(|| e.qualified_name.clone()),
                ),
                EntityKind::File => (
                    SummaryGranularity::File,
                    assemble_child_signatures(e, by_file.get(e.file_path.as_str())),
                ),
                EntityKind::Module => (
                    SummaryGranularity::Module,
                    assemble_child_signatures(e, by_parent.get(e.id.as_str())),
                ),
                // `is_summarisable` already excludes every other kind.
                _ => return None,
            };
            let context = e.doc_comment.clone().unwrap_or_default();
            let source_hash = summary_source_hash(&source, &context);
            Some(SummaryJob {
                entity_id: e.id.clone(),
                qualified_name: e.qualified_name.clone(),
                granularity,
                source,
                context,
                source_hash,
            })
        })
        .collect()
}

/// Concatenate the signatures of `parent`'s child symbols into a bounded
/// block for a file/module summary. Falls back to the parent's qualified
/// name when there are no child signatures (e.g. a comment-only file).
///
/// Children are ordered by source position so the assembled text — and thus
/// the summary's `source_hash` — is independent of the order entities arrive
/// in (full/incremental parse order vs. the `ORDER BY` of a resume read).
fn assemble_child_signatures(parent: &CodeEntity, children: Option<&Vec<&CodeEntity>>) -> String {
    let mut kids: Vec<&CodeEntity> = children
        .into_iter()
        .flatten()
        .copied()
        .filter(|c| {
            c.id != parent.id && c.signature.as_deref().is_some_and(|s| !s.trim().is_empty())
        })
        .collect();
    if kids.is_empty() {
        return parent.qualified_name.clone();
    }
    kids.sort_by(|a, b| {
        (&a.file_path, a.start_line, &a.name).cmp(&(&b.file_path, b.start_line, &b.name))
    });

    let lines: Vec<String> = kids
        .iter()
        .map(|c| {
            format!(
                "- {} {}",
                c.kind.as_str(),
                c.signature.as_deref().unwrap_or("").trim()
            )
        })
        .collect();

    let mut out = String::new();
    for (i, line) in lines.iter().enumerate() {
        if i >= MAX_CHILD_SIGNATURES || out.len() + line.len() + 1 > MAX_CHILD_SIGNATURE_CHARS {
            let _ = write!(out, "\n- … and {} more", lines.len() - i);
            break;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
    }
    out
}

/// blake3 of the exact LLM input, used as the summary's `source_hash` so an
/// unchanged input can be skipped on resume. The NUL separator keeps the
/// `(source, context)` pair unambiguous.
fn summary_source_hash(source: &str, context: &str) -> String {
    let mut input = String::with_capacity(source.len() + context.len() + 1);
    input.push_str(source);
    input.push('\u{0}');
    input.push_str(context);
    blake3::hash(input.as_bytes()).to_hex().to_string()
}

/// The main indexing pipeline that orchestrates crawling, parsing,
/// relationship building, and storage.
pub struct IndexingPipeline {
    config: IndexingConfig,
    graph: Arc<CodeGraph>,
    vector_store: Option<Arc<dyn CodeVectorStore>>,
    summary_generator: Option<SummaryGenerator>,
    /// Set when the vector store did not take something this run changed:
    /// the files it noted as pending then stay so.
    vectors_behind: AtomicBool,
}

impl IndexingPipeline {
    pub fn new(config: IndexingConfig, graph: Arc<CodeGraph>) -> Self {
        Self {
            config,
            graph,
            vector_store: None,
            summary_generator: None,
            vectors_behind: AtomicBool::new(false),
        }
    }

    pub fn with_vector_store(
        config: IndexingConfig,
        graph: Arc<CodeGraph>,
        vector_store: Arc<dyn CodeVectorStore>,
    ) -> Self {
        Self {
            config,
            graph,
            vector_store: Some(vector_store),
            summary_generator: None,
            vectors_behind: AtomicBool::new(false),
        }
    }

    pub fn with_summary_generator(mut self, generator: SummaryGenerator) -> Self {
        self.summary_generator = Some(generator);
        self
    }

    /// Run a full index of the repository.
    pub async fn run_full_index(&self, opts: RunOptions) -> Result<IndexResult> {
        let RunOptions { progress_tx, lease } = opts;
        let start = std::time::Instant::now();

        report_phase(&lease, IndexPhase::Crawling).await;

        // 1. Purge all existing data for this repo_id so a full re-index
        //    starts clean. This is essential when a different repo is cloned
        //    to the same path — old entities from the previous repo would
        //    otherwise persist because `INSERT OR REPLACE` only overwrites
        //    rows with the same primary key.
        //    What the repo's vectors were recorded complete in goes with
        //    it, so it is read first.
        let whole_in = match self.tracked_store() {
            Some(vs) => {
                let recorded = self.graph.vectors_complete_in(&self.config.repo_id).await?;
                let held = vs.identity(false).await.ok();
                recorded.filter(|recorded| Some(recorded) == held.as_ref())
            }
            None => None,
        };
        // The standby is told of every file the repo had, before they go:
        // its notes outlast the purge (see `CodeGraph::delete_repo_data`),
        // and those of the files the run makes are added once they are
        // stored (6).
        // With no standby, nothing is read: the run is as it was.
        if self.graph.vector_keys().standby().is_some() {
            let had = self.graph.get_file_paths(&self.config.repo_id).await?;
            self.note_for_standby(&had).await?;
        }
        self.graph.delete_repo_data(&self.config.repo_id).await?;

        // 1b. Also purge vector store data for this repo, in a store that
        //     does not track completeness: the new vectors are stored below
        //     (6b), as ever. In one that does, nothing is purged: the
        //     vectors are stored after this run by a fill from the graph
        //     (see [`fill`]), which is paced, goes on from where it stopped
        //     and holds no lease, where embedding a large repo here would
        //     outlast the run's timeout. The fill writes over what the
        //     store holds and removes at its end what it did not write, so
        //     a repo whose vectors were whole goes on being searched by
        //     them meanwhile (7).
        if let Some(vs) = self
            .vector_store
            .as_ref()
            .filter(|vs| !vs.tracks_completeness())
        {
            if let Err(e) = vs.delete_repo_entities(&self.config.repo_id).await {
                warn!("Vector store entity purge failed (non-fatal): {}", e);
            }
            if let Err(e) = vs.delete_repo_summaries(&self.config.repo_id).await {
                warn!("Vector store summary purge failed (non-fatal): {}", e);
            }
        }

        // 2. Register the repository (re-creates the row after purge)
        self.graph
            .register_repo(
                &self.config.repo_id,
                &self.config.repo_name,
                &self.config.root.to_string_lossy(),
            )
            .await?;

        // 3. Crawl files
        let crawler = FileCrawler::new(self.config.crawler.clone());
        let files = crawler.crawl().await?;
        let total_files = files.len();
        info!("Crawled {} files", total_files);

        if let Some(tx) = &progress_tx {
            let _ = tx.send(IndexProgress::Started { total_files }).await;
        }

        report_phase(&lease, IndexPhase::Parsing).await;

        // 4. Parse files and extract entities
        let parser = AstParser::new();
        let extractor = SymbolExtractor::new(&self.config.repo_id);
        let mut all_entities: Vec<CodeEntity> = Vec::new();
        let mut all_relationships: Vec<Relationship> = Vec::new();
        let mut all_impl_traits: Vec<ImplTraitInfo> = Vec::new();
        let mut all_calls: Vec<CallReference> = Vec::new();

        for file in &files {
            match self.index_file(&parser, &extractor, file).await {
                Ok((entities, rels, impl_traits, calls)) => {
                    let entity_count = entities.len();

                    // Update file index
                    self.graph
                        .update_file_index(
                            &self.config.repo_id,
                            &file.relative_path,
                            &file.content_hash,
                            file.language.as_str(),
                            entity_count,
                        )
                        .await?;

                    all_entities.extend(entities);
                    all_relationships.extend(rels);
                    all_impl_traits.extend(impl_traits);
                    all_calls.extend(calls);

                    if let Some(tx) = &progress_tx {
                        let _ = tx
                            .send(IndexProgress::FileIndexed {
                                path: file.relative_path.clone(),
                                entities: entity_count,
                            })
                            .await;
                    }
                }
                Err(e) => {
                    warn!("Failed to index {}: {}", file.relative_path, e);
                    if let Some(tx) = &progress_tx {
                        let _ = tx
                            .send(IndexProgress::Error {
                                message: format!("Failed to index {}: {}", file.relative_path, e),
                            })
                            .await;
                    }
                }
            }
        }

        report_phase(&lease, IndexPhase::Committing).await;

        // 5. Build cross-file relationships
        let rel_builder = RelationshipBuilder::new(&self.config.repo_id);
        let cross_file_rels =
            rel_builder.build_cross_file_relationships(&all_entities, &all_impl_traits, &all_calls);
        all_relationships.extend(cross_file_rels);

        // 6. Store entities and relationships
        let entity_count = self.graph.upsert_entities(&all_entities).await?;
        let rel_count = self.graph.upsert_relationships(&all_relationships).await?;
        let mut made: Vec<String> = all_entities
            .iter()
            .map(|entity| entity.file_path.clone())
            .collect();
        made.sort();
        made.dedup();
        self.note_for_standby(&made).await?;
        report_progress(
            &lease,
            LeaseProgress {
                entities_total: Some(entity_count as u64),
                entities_processed: Some(entity_count as u64),
                ..Default::default()
            },
        )
        .await;

        report_phase(&lease, IndexPhase::VectorIndexing).await;

        // 6b. Index entities to vector store (non-fatal)
        if let Some(vs) = self
            .vector_store
            .as_ref()
            .filter(|vs| !vs.tracks_completeness())
        {
            let vec_entities: Vec<VectorCodeEntity> = all_entities
                .iter()
                .map(|e| VectorCodeEntity::from_code_entity(e, &self.config.repo_id, None))
                .collect();
            let submitted = vec_entities.len();
            match vs.index_entities(&vec_entities).await {
                Ok(indexed) => info!(
                    "{}",
                    format_vector_index_outcome("entities", submitted, indexed)
                ),
                Err(e) => warn!("Vector indexing failed (non-fatal): {}", e),
            }
        }

        if let Some(tx) = &progress_tx {
            let _ = tx
                .send(IndexProgress::RelationshipsBuilt { count: rel_count })
                .await;
        }

        // 7. Commit the index state. The lease (held for the rest of
        //    the run) is the signal for "indexing not yet finished" —
        //    `last_commit` here records "entities are at this commit",
        //    which is true regardless of whether summaries follow.
        let commit = self.get_current_commit().await.unwrap_or_default();
        self.graph
            .update_index_state_full(&self.config.repo_id, &commit, entity_count, rel_count)
            .await?;
        // A full index forgets what collection the repo's vectors were
        // complete in, and any fill under way: a store that tracks
        // completeness is filled from the graph before it is relied on,
        // one configured later likewise. Unless the vectors were whole in
        // the very collection the store still has: then it is relied on
        // still, holding the repo as it was, while a fill begun here makes
        // it the repo as it is.
        if let (Some(whole_in), Some(vs)) = (whole_in, self.tracked_store()) {
            let filler = fill::VectorFiller::new(self.graph.clone(), vs.clone());
            match vs.identity(true).await {
                Ok(held) if held == whole_in => {
                    self.graph
                        .set_vectors_complete_in(&self.config.repo_id, Some(&whole_in))
                        .await?;
                    filler.begin(&self.config.repo_id, true).await?;
                }
                _ => warn!(
                    repo_id = %self.config.repo_id,
                    "the collection the repo's vectors were complete in could not be found again; they are filled from the graph before they are searched"
                ),
            }
        }

        if let Err(e) = self.graph.checkpoint().await {
            warn!("WAL checkpoint after full index failed: {}", e);
        }

        // 8. Generate summaries (if enabled). Runs to completion under
        //    the lease's heartbeat; an interrupted run leaves a stale
        //    lease that the next acquirer steals, and `next_action()`
        //    sees missing summaries with HEAD already indexed and
        //    returns `ResumeSummaries`.
        let summary_count = if let Some(ref generator) = self.summary_generator {
            report_phase(&lease, IndexPhase::GeneratingSummaries).await;
            let count = self
                .generate_summaries(generator, &all_entities, lease.as_ref(), false)
                .await;
            if let Some(tx) = &progress_tx {
                let _ = tx.send(IndexProgress::SummariesGenerated { count }).await;
            }
            count
        } else {
            0
        };

        let duration = start.elapsed();
        let result = IndexResult {
            files_indexed: total_files,
            entities_created: entity_count,
            relationships_created: rel_count,
            summaries_generated: summary_count,
            duration_ms: duration.as_millis() as u64,
        };

        if let Some(tx) = &progress_tx {
            let _ = tx
                .send(IndexProgress::Completed {
                    duration_ms: result.duration_ms,
                    entities: result.entities_created,
                    relationships: result.relationships_created,
                })
                .await;
        }

        info!(
            "Full index complete: {} files, {} entities, {} relationships, {} summaries in {:.1}s",
            result.files_indexed,
            result.entities_created,
            result.relationships_created,
            result.summaries_generated,
            duration.as_secs_f64()
        );

        Ok(result)
    }

    /// Run an incremental index, processing only files that changed since the last commit.
    /// Falls back to a full index if no previous commit is recorded or git is unavailable.
    pub async fn run_incremental_index(&self, opts: RunOptions) -> Result<IndexResult> {
        let RunOptions { progress_tx, lease } = opts;
        report_phase(&lease, IndexPhase::Crawling).await;

        // 1. Check for a previous index state
        let state = self.graph.get_index_state(&self.config.repo_id).await?;
        let last_commit = match state.and_then(|s| s.last_commit) {
            Some(c) if !c.is_empty() => c,
            _ => {
                info!("No previous commit found, falling back to full index");
                return self.run_full_index(RunOptions { progress_tx, lease }).await;
            }
        };

        // 2. Compute delta since last commit
        let delta = match IncrementalDetector::compute_delta(&self.config.root, &last_commit).await
        {
            Ok(d) => d,
            Err(e) => {
                warn!(
                    "Failed to compute delta ({}), falling back to full index",
                    e
                );
                return self.run_full_index(RunOptions { progress_tx, lease }).await;
            }
        };

        // 3. Early return if nothing changed
        if delta.is_empty() {
            info!(
                "No changes detected since {}",
                &last_commit[..8.min(last_commit.len())]
            );
            return Ok(IndexResult {
                files_indexed: 0,
                entities_created: 0,
                relationships_created: 0,
                summaries_generated: 0,
                duration_ms: 0,
            });
        }

        let start = std::time::Instant::now();
        let total_changes = delta.total_changes();
        info!(
            "Incremental index: {} changes (+{} ~{} -{} R{})",
            total_changes,
            delta.added_files.len(),
            delta.modified_files.len(),
            delta.deleted_files.len(),
            delta.renamed_files.len(),
        );

        if let Some(tx) = &progress_tx {
            let _ = tx
                .send(IndexProgress::Started {
                    total_files: total_changes,
                })
                .await;
        }

        report_phase(&lease, IndexPhase::Parsing).await;

        // 3b. The files this run is about to change are noted first, for a
        //     store that keeps track of the repo's vectors: so that if the
        //     run is killed, or the store does not take them, or this run
        //     writes to another store than the one whose vectors are kept
        //     track of, they are put right in that store later, file by
        //     file, rather than the whole repo embedded again.
        let noted = self.note_pending(&delta).await?;

        // 4. Process deletions
        for file_path in &delta.deleted_files {
            self.graph
                .delete_file_entities(&self.config.repo_id, file_path)
                .await?;
            self.graph
                .delete_file_from_index(&self.config.repo_id, file_path)
                .await?;
            self.purge_file_vectors(file_path).await;
        }

        // 5. Process renames. The vector store has no rename primitive,
        //    so we drop old-path vectors and index the file's entities
        //    again at their new path (8b), as the graph now has them.
        let mut renamed_entities: Vec<CodeEntity> = Vec::new();
        for (old_path, new_path) in &delta.renamed_files {
            self.graph
                .rename_file(&self.config.repo_id, old_path, new_path)
                .await?;
            self.purge_file_vectors(old_path).await;
            renamed_entities.extend(
                self.graph
                    .find_entities_in_file(&self.config.repo_id, new_path)
                    .await?,
            );
        }

        // 6. Process modified + added files
        let crawler = FileCrawler::new(self.config.crawler.clone());
        let parser = AstParser::new();
        let extractor = SymbolExtractor::new(&self.config.repo_id);
        let mut new_entities: Vec<CodeEntity> = Vec::new();
        let mut new_relationships: Vec<Relationship> = Vec::new();
        let mut new_impl_traits: Vec<ImplTraitInfo> = Vec::new();
        let mut new_calls: Vec<CallReference> = Vec::new();

        for file_path in &delta.modified_files {
            self.graph
                .delete_file_entities(&self.config.repo_id, file_path)
                .await?;
            self.purge_file_vectors(file_path).await;
        }

        let files_to_index: Vec<String> = delta
            .added_files
            .iter()
            .chain(delta.modified_files.iter())
            .cloned()
            .collect();

        let mut files_indexed = 0;
        for relative_path in &files_to_index {
            let file = match crawler.crawl_file(relative_path).await {
                Ok(Some(f)) => f,
                Ok(None) => continue, // excluded or missing
                Err(e) => {
                    warn!("Failed to crawl {}: {}", relative_path, e);
                    continue;
                }
            };

            match self.index_file(&parser, &extractor, &file).await {
                Ok((entities, rels, impl_traits, calls)) => {
                    let entity_count = entities.len();

                    self.graph
                        .update_file_index(
                            &self.config.repo_id,
                            &file.relative_path,
                            &file.content_hash,
                            file.language.as_str(),
                            entity_count,
                        )
                        .await?;

                    new_entities.extend(entities);
                    new_relationships.extend(rels);
                    new_impl_traits.extend(impl_traits);
                    new_calls.extend(calls);
                    files_indexed += 1;

                    if let Some(tx) = &progress_tx {
                        let _ = tx
                            .send(IndexProgress::FileIndexed {
                                path: file.relative_path.clone(),
                                entities: entity_count,
                            })
                            .await;
                    }
                }
                Err(e) => {
                    warn!("Failed to index {}: {}", relative_path, e);
                    if let Some(tx) = &progress_tx {
                        let _ = tx
                            .send(IndexProgress::Error {
                                message: format!("Failed to index {}: {}", relative_path, e),
                            })
                            .await;
                    }
                }
            }
        }

        // 7. Build cross-file relationships for changed files
        // Load all entities (including previously indexed ones) for resolution
        let all_entities = self.graph.all_entities(&self.config.repo_id).await?;
        let rel_builder = RelationshipBuilder::new(&self.config.repo_id);
        let cross_file_rels =
            rel_builder.build_cross_file_relationships(&all_entities, &new_impl_traits, &new_calls);
        new_relationships.extend(cross_file_rels);

        report_phase(&lease, IndexPhase::Committing).await;

        // 8. Store new entities and relationships
        let entity_count = self.graph.upsert_entities(&new_entities).await?;
        let rel_count = self.graph.upsert_relationships(&new_relationships).await?;

        report_phase(&lease, IndexPhase::VectorIndexing).await;

        // 8b. Index new and renamed entities to vector store (non-fatal)
        if let Some(vs) = &self.vector_store {
            let generation =
                fill::VectorFiller::generation(&self.graph, &self.config.repo_id).await?;
            let vec_entities: Vec<VectorCodeEntity> = new_entities
                .iter()
                .chain(&renamed_entities)
                .map(|e| {
                    VectorCodeEntity::from_code_entity(e, &self.config.repo_id, None)
                        .of_generation(&generation)
                })
                .collect();
            let submitted = vec_entities.len();
            match vs.index_entities(&vec_entities).await {
                Ok(indexed) => info!(
                    "{} (incremental)",
                    format_vector_index_outcome("entities", submitted, indexed)
                ),
                Err(e) => {
                    warn!("Vector indexing failed (non-fatal): {}", e);
                    self.vectors_behind.store(true, Ordering::SeqCst);
                }
            }
        }
        // The store that keeps track has every file this run changed: none
        // is pending, unless noted again since. A run that wrote elsewhere
        // leaves its notes for that store.
        if self.writes_where_tracked().await? && !self.vectors_behind.load(Ordering::SeqCst) {
            for (file_path, seq) in &noted {
                self.graph
                    .clear_vectors_pending(&self.config.repo_id, file_path, *seq)
                    .await?;
            }
        }

        if let Some(tx) = &progress_tx {
            let _ = tx
                .send(IndexProgress::RelationshipsBuilt { count: rel_count })
                .await;
        }

        // 9. Update index state BEFORE summaries — summaries are slow (Ollama)
        //    and the index should be usable even if they time out or fail.
        let commit = self.get_current_commit().await.unwrap_or_default();
        let stats = self.graph.get_stats(&self.config.repo_id).await?;
        self.graph
            .update_index_state_incremental(
                &self.config.repo_id,
                &commit,
                stats.entity_count,
                stats.relationship_count,
            )
            .await?;

        // 9b. Checkpoint WAL so the index state is persisted immediately
        if let Err(e) = self.graph.checkpoint().await {
            warn!("WAL checkpoint after incremental index failed: {}", e);
        }

        // 10. Generate summaries for new entities (if enabled). Run
        //     under the lease's heartbeat — interruption is recoverable
        //     via `run_summaries_only` on the next trigger.
        let summary_count = if let Some(ref generator) = self.summary_generator {
            report_phase(&lease, IndexPhase::GeneratingSummaries).await;
            let count = self
                .generate_summaries(generator, &new_entities, lease.as_ref(), false)
                .await;
            if let Some(tx) = &progress_tx {
                let _ = tx.send(IndexProgress::SummariesGenerated { count }).await;
            }
            count
        } else {
            0
        };

        let duration = start.elapsed();
        let result = IndexResult {
            files_indexed,
            entities_created: entity_count,
            relationships_created: rel_count,
            summaries_generated: summary_count,
            duration_ms: duration.as_millis() as u64,
        };

        if let Some(tx) = &progress_tx {
            let _ = tx
                .send(IndexProgress::Completed {
                    duration_ms: result.duration_ms,
                    entities: result.entities_created,
                    relationships: result.relationships_created,
                })
                .await;
        }

        info!(
            "Incremental index complete: {} files, {} entities, {} relationships, {} summaries in {:.1}s",
            result.files_indexed,
            result.entities_created,
            result.relationships_created,
            result.summaries_generated,
            duration.as_secs_f64()
        );

        Ok(result)
    }

    /// Resume the summary phase against the entities already in the
    /// graph. Used by [`crate::completeness::NextAction::ResumeSummaries`]
    /// when a previous run committed entities at HEAD but was killed
    /// (or hung) during summary generation. Passes `skip_fresh = true`, so
    /// `generate_summaries` skips entries whose stored `source_hash` matches
    /// the current input and is not stale — only the missing/changed ones
    /// cost an LLM call, making the backfill resumable and idempotent.
    pub async fn run_summaries_only(&self, opts: RunOptions) -> Result<IndexResult> {
        let RunOptions { progress_tx, lease } = opts;
        let start = std::time::Instant::now();

        let generator = match self.summary_generator.as_ref() {
            Some(g) => g,
            None => {
                return Ok(IndexResult {
                    files_indexed: 0,
                    entities_created: 0,
                    relationships_created: 0,
                    summaries_generated: 0,
                    duration_ms: 0,
                });
            }
        };

        report_phase(&lease, IndexPhase::GeneratingSummaries).await;

        let all_entities = self.graph.all_entities(&self.config.repo_id).await?;
        let count = self
            .generate_summaries(generator, &all_entities, lease.as_ref(), true)
            .await;

        if let Some(tx) = &progress_tx {
            let _ = tx.send(IndexProgress::SummariesGenerated { count }).await;
        }

        let duration = start.elapsed();
        let result = IndexResult {
            files_indexed: 0,
            entities_created: 0,
            relationships_created: 0,
            summaries_generated: count,
            duration_ms: duration.as_millis() as u64,
        };

        info!(
            "Summary resume complete: {} summaries in {:.1}s",
            count,
            duration.as_secs_f64()
        );

        Ok(result)
    }

    /// Index a single file: parse it and extract entities + intra-file relationships.
    async fn index_file(
        &self,
        parser: &AstParser,
        extractor: &SymbolExtractor,
        file: &CrawledFile,
    ) -> Result<(
        Vec<CodeEntity>,
        Vec<Relationship>,
        Vec<ImplTraitInfo>,
        Vec<CallReference>,
    )> {
        let content = tokio::fs::read_to_string(&file.path).await?;

        if !file.language.has_parser() {
            // For non-parseable files, create a File entity only
            let file_entity = extractor.create_file_entity(file, &content);
            return Ok((vec![file_entity], vec![], vec![], vec![]));
        }

        // Parse with tree-sitter
        let tree = parser.parse(&content, &file.language)?;

        // Extract entities from the AST
        let result = extractor.extract_from_tree(&tree, &content, file);

        // Build intra-file relationships (contains, parent-child)
        let rel_builder = RelationshipBuilder::new(&self.config.repo_id);
        let relationships = rel_builder.build_intra_file_relationships(&result.entities);

        Ok((
            result.entities,
            relationships,
            result.impl_traits,
            result.calls,
        ))
    }

    /// The vector store, if it is one whose vectors of a repo may be
    /// missing or behind, and are kept track of.
    fn tracked_store(&self) -> Option<&Arc<dyn CodeVectorStore>> {
        self.vector_store
            .as_ref()
            .filter(|vs| vs.tracks_completeness())
    }

    /// Whether this run's store is the one the repo's vectors are kept
    /// track of in: a store that tracks completeness, and, if the repo's
    /// vectors are recorded complete or being filled somewhere, that very
    /// collection.
    async fn writes_where_tracked(&self) -> Result<bool> {
        let Some(vs) = self.tracked_store() else {
            return Ok(false);
        };
        let repo_id = &self.config.repo_id;
        let recorded = match self.graph.vectors_complete_in(repo_id).await? {
            Some(recorded) => Some(recorded),
            None => self
                .graph
                .vector_fill(repo_id)
                .await?
                .map(|fill| fill.collection),
        };
        Ok(match recorded {
            Some(recorded) => vs.identity(false).await.ok() == Some(recorded),
            None => true,
        })
    }

    /// The vector databases a file this run changes is to be noted pending
    /// in: the one in use if its store keeps track of its vectors (it is
    /// put right from the note if the run does not store the file, and the
    /// note is taken off if it does), and the standby always, which nothing
    /// here writes to and which is brought up to date from its notes when
    /// it is switched to (see [`crate::graph::VectorKeys`]).
    async fn noted_in(&self) -> Result<Vec<&str>> {
        let keys = self.graph.vector_keys();
        let mut backends = Vec::new();
        if self.tracked_store().is_some() {
            backends.push(keys.active());
        } else if keys.standby().is_none() {
            // A repo whose vectors were kept track of in another store,
            // with that store not configured as the standby, falls behind
            // there unseen until its vectors are next compared.
            let repo_id = &self.config.repo_id;
            if self.graph.vectors_complete_in(repo_id).await?.is_some() {
                warn!(
                    repo_id = %repo_id,
                    "the repo's code vectors are kept track of in another vector database, which is not [knowledge] vector_standby: what this run changes is not noted for it"
                );
            }
        }
        backends.extend(keys.standby());
        Ok(backends)
    }

    /// Note every file `delta` touches as pending (see [`Self::noted_in`]),
    /// and return the notes of the vector database in use, to be taken off
    /// once its store, keeping track, has the file. A standby's are never
    /// taken off here.
    async fn note_pending(
        &self,
        delta: &incremental::IndexDelta,
    ) -> Result<Vec<crate::graph::vectors::PendingFile>> {
        let repo_id = &self.config.repo_id;
        let backends = self.noted_in().await?;
        if backends.is_empty() {
            return Ok(Vec::new());
        }
        let renamed = delta.renamed_files.iter().flat_map(|(old, new)| [old, new]);
        let files: Vec<String> = delta
            .deleted_files
            .iter()
            .chain(&delta.added_files)
            .chain(&delta.modified_files)
            .chain(renamed)
            .cloned()
            .collect();
        self.graph
            .note_vectors_pending_in(&backends, repo_id, &files)
            .await?;
        if !backends.contains(&self.graph.vector_keys().active()) {
            return Ok(Vec::new());
        }
        let pending = self.graph.vectors_pending(repo_id, usize::MAX >> 1).await?;
        Ok(pending
            .into_iter()
            .filter(|(file_path, _)| files.contains(file_path))
            .collect())
    }

    /// Note `files` pending for the standby, if there is one.
    async fn note_for_standby(&self, files: &[String]) -> Result<()> {
        let Some(standby) = self.graph.vector_keys().standby() else {
            return Ok(());
        };
        self.graph
            .note_vectors_pending_in(&[standby], &self.config.repo_id, files)
            .await
    }

    /// Drop vector store entries for one file. Non-fatal: the graph is
    /// authoritative, so a vector store outage must not block indexing;
    /// the file then stays noted as pending.
    async fn purge_file_vectors(&self, file_path: &str) {
        let Some(vs) = &self.vector_store else {
            return;
        };
        if let Err(e) = vs
            .delete_entities_by_file(&self.config.repo_id, file_path)
            .await
        {
            warn!(
                file_path = file_path,
                "Vector entity purge failed (non-fatal): {}", e
            );
            self.vectors_behind.store(true, Ordering::SeqCst);
        }
        if let Err(e) = vs
            .delete_summaries_by_file(&self.config.repo_id, file_path)
            .await
        {
            warn!(
                file_path = file_path,
                "Vector summary purge failed (non-fatal): {}", e
            );
            self.vectors_behind.store(true, Ordering::SeqCst);
        }
    }

    /// Get the current git HEAD commit SHA.
    async fn get_current_commit(&self) -> Option<String> {
        let output = tokio::process::Command::new("git")
            .args(["-c", "safe.directory=*", "rev-parse", "HEAD"])
            .current_dir(&self.config.root)
            .output()
            .await
            .ok()?;

        if output.status.success() {
            let sha = String::from_utf8_lossy(&output.stdout).trim().to_string();
            if sha.is_empty() {
                None
            } else {
                Some(sha)
            }
        } else {
            None
        }
    }

    /// Generate LLM summaries for entities (Function, Method, File, Module
    /// only). Summary generation is network-bound, so we drive it with
    /// `buffer_unordered(summary_concurrency)` — sequential calls would
    /// take O(entities × LLM latency) and stall large repos for hours.
    /// SQLite writes inside each future hit a single `Mutex<Connection>`
    /// which serialises them naturally.
    ///
    /// When a `lease` is provided, progress counters (total / processed)
    /// are pushed to the lease at boundaries so observers can chart the
    /// long phase. The heartbeat task driven by `LeaseContext` keeps the
    /// row alive in parallel.
    ///
    /// `skip_fresh` is set only by the resume / summaries-only path: it loads
    /// the stored `source_hash`es once and drops jobs whose summary is already
    /// fresh for the identical input, so an interrupted backfill resumes
    /// instead of re-paying, and a residual completeness gap doesn't re-bill
    /// the whole repo every trigger. Full and incremental runs pass `false` —
    /// their entities are known new/changed, so the extra query is pure waste.
    async fn generate_summaries(
        &self,
        generator: &SummaryGenerator,
        entities: &[CodeEntity],
        lease: Option<&Arc<LeaseContext>>,
        skip_fresh: bool,
    ) -> usize {
        use futures::stream::{self, StreamExt};
        use std::sync::atomic::AtomicUsize;

        let mut jobs = build_summary_jobs(entities);

        // Resume path only: ids whose summary we skip because it is already
        // fresh. Re-vectored below — a prior run may have committed summaries
        // but died before vectoring, and `is_current()` doesn't track vector
        // coverage, so this is the only thing that heals that gap.
        let mut already_fresh: Vec<EntityId> = Vec::new();

        if skip_fresh && !jobs.is_empty() {
            match self.graph.summary_hashes(&self.config.repo_id).await {
                Ok(existing) => {
                    jobs.retain(|job| {
                        let key = (job.entity_id.clone(), job.granularity.as_str().to_string());
                        let is_fresh = matches!(
                            existing.get(&key),
                            Some((hash, false)) if *hash == job.source_hash
                        );
                        if is_fresh {
                            already_fresh.push(job.entity_id.clone());
                        }
                        !is_fresh
                    });
                    if !already_fresh.is_empty() {
                        info!(
                            "Summary resume: {} already-fresh summaries skipped",
                            already_fresh.len()
                        );
                    }
                }
                Err(e) => warn!("Could not load existing summary hashes; regenerating all: {e}"),
            }
        }

        if jobs.is_empty() && already_fresh.is_empty() {
            return 0;
        }

        let total_jobs = jobs.len();
        let lease_owned = lease.cloned();
        if let Some(l) = &lease_owned {
            l.set_progress(LeaseProgress {
                summaries_total: Some(total_jobs as u64),
                summaries_processed: Some(0),
                ..Default::default()
            })
            .await;
        }

        let processed = Arc::new(AtomicUsize::new(0));
        // Collect the ids that were actually (re)generated this pass so we only
        // re-index those to the vector store — not the whole entity slice.
        let fresh_ids: Vec<EntityId> = stream::iter(jobs)
            .map(|job| {
                let processed = Arc::clone(&processed);
                let lease = lease_owned.clone();
                async move {
                    let entity_id = job.entity_id.clone();
                    let ok = self.run_summary_job(generator, job).await;
                    let done = processed.fetch_add(1, Ordering::Relaxed) + 1;
                    if done.is_multiple_of(64) {
                        if let Some(l) = lease {
                            l.set_progress(LeaseProgress {
                                summaries_processed: Some(done as u64),
                                ..Default::default()
                            })
                            .await;
                        }
                    }
                    ok.then_some(entity_id)
                }
            })
            .buffer_unordered(self.config.summary_concurrency.max(1))
            .filter_map(|r| async move { r })
            .collect()
            .await;

        if let Some(l) = &lease_owned {
            l.set_progress(LeaseProgress {
                summaries_processed: Some(processed.load(Ordering::Relaxed) as u64),
                ..Default::default()
            })
            .await;
        }

        let count = fresh_ids.len();
        if let Some(vs) = &self.vector_store {
            // What we (re)generated, plus the fresh summaries skipped on resume
            // (empty on the full/incremental path).
            let mut to_index = fresh_ids;
            to_index.extend(already_fresh);
            if !to_index.is_empty() {
                self.index_summaries_to_vector(vs.as_ref(), entities, &to_index)
                    .await;
            }
        }

        info!(
            "Generated {}/{} summaries (concurrency={})",
            count, total_jobs, self.config.summary_concurrency
        );
        count
    }

    /// Drive a single summary job to completion: LLM call, then summary
    /// upsert (storing the input `source_hash` so it can be skipped on a later
    /// resume). Returns `true` on full success, `false` on any failure
    /// (logged). Owned-data inputs make the future trivially `Send`, which is
    /// required by `buffer_unordered`.
    async fn run_summary_job(&self, generator: &SummaryGenerator, job: SummaryJob) -> bool {
        let text = match generator
            .generate(&job.source, &job.context, job.granularity.clone())
            .await
        {
            Ok(t) => t,
            Err(e) => {
                warn!(
                    "Summary generation failed for {}: {}",
                    job.qualified_name, e
                );
                return false;
            }
        };
        match self
            .graph
            .upsert_summary(
                &job.entity_id,
                job.granularity.as_str(),
                &text,
                &job.source_hash,
                generator.model_name(),
            )
            .await
        {
            Ok(()) => true,
            Err(e) => {
                warn!("Failed to store summary for {}: {}", job.qualified_name, e);
                false
            }
        }
    }

    /// Index freshly-(re)generated summaries to the Weaviate vector store.
    /// Only the entities in `fresh_ids` are uploaded, so an incremental pass
    /// doesn't re-push the whole repo's summaries on every run.
    async fn index_summaries_to_vector(
        &self,
        vs: &dyn CodeVectorStore,
        entities: &[CodeEntity],
        fresh_ids: &[EntityId],
    ) {
        let repo_id = &self.config.repo_id;
        let tracked = vs.tracks_completeness();
        // The standby is written to by nothing here: the files of these
        // summaries are noted for it, whatever the store in use does.
        let mut standby_files: Vec<String> = entities
            .iter()
            .filter(|entity| fresh_ids.contains(&entity.id))
            .map(|entity| entity.file_path.clone())
            .collect();
        standby_files.sort();
        standby_files.dedup();
        if let Err(e) = self.note_for_standby(&standby_files).await {
            warn!(
                "noting files whose summaries changed for the standby failed: {}",
                e
            );
        }
        // Whether the repo's vectors are kept track of in the store in
        // use: recorded complete, or being filled. If that cannot be told
        // they are taken to be.
        let kept_track = tracked
            && match (
                self.graph.vectors_complete_in(repo_id).await,
                self.graph.vector_fill(repo_id).await,
            ) {
                (Ok(complete_in), Ok(fill)) => complete_in.is_some() || fill.is_some(),
                _ => true,
            };
        // In a store that keeps track, a repo no fill of which has begun
        // gets its summaries with the rest, from the fill: these are in the
        // graph already, and it reads the graph after it begins. One being
        // filled gets them here, since the fill may have passed them.
        if tracked && !kept_track {
            return;
        }
        let generation = fill::VectorFiller::generation(&self.graph, repo_id)
            .await
            .unwrap_or_default();
        let fresh: std::collections::HashSet<&str> =
            fresh_ids.iter().map(|id| id.as_str()).collect();
        let mut vec_summaries = Vec::new();
        for entity in entities {
            if !fresh.contains(entity.id.as_str()) {
                continue;
            }
            let granularity = match entity.kind {
                EntityKind::Function | EntityKind::Method => "function",
                EntityKind::File => "file",
                EntityKind::Module => "module",
                _ => continue,
            };
            if let Ok(Some((text, is_stale))) =
                self.graph.get_summary(&entity.id, granularity).await
            {
                vec_summaries.push(VectorCodeSummary {
                    entity_id: entity.id.0.clone(),
                    repo_id: self.config.repo_id.clone(),
                    granularity: granularity.to_string(),
                    file_path: entity.file_path.clone(),
                    summary_text: text,
                    is_stale,
                    generation: generation.clone(),
                });
            }
        }
        if vec_summaries.is_empty() {
            return;
        }
        // Their files are put right later, one by one, in the store in use,
        // which keeps track.
        let pending = || async {
            let mut files: Vec<String> = vec_summaries
                .iter()
                .map(|summary| summary.file_path.clone())
                .collect();
            files.sort();
            files.dedup();
            if let Err(e) = self.graph.note_vectors_pending(repo_id, &files).await {
                warn!("noting files whose summaries were not stored failed: {}", e);
            }
        };
        // Kept track of in another collection than the one this run writes
        // to.
        let elsewhere = kept_track && !self.writes_where_tracked().await.unwrap_or(false);
        if elsewhere {
            pending().await;
        }
        let submitted = vec_summaries.len();
        match vs.index_summaries(&vec_summaries).await {
            Ok(indexed) => info!(
                "{}",
                format_vector_index_outcome("summaries", submitted, indexed)
            ),
            Err(e) => {
                warn!("Vector summary indexing failed (non-fatal): {}", e);
                if kept_track && !elsewhere {
                    pending().await;
                }
            }
        }
    }
}

/// Format the indexer-level summary line for a vector-store batch write so
/// operators can see filtered counts on the same line as the indexed count.
/// The vector store itself already logs the filtering at warn-level; this
/// surfaces the same fact in the operator-facing progress log.
fn format_vector_index_outcome(label: &str, submitted: usize, indexed: usize) -> String {
    let filtered = submitted.saturating_sub(indexed);
    if filtered > 0 {
        format!(
            "Indexed {indexed} {label} to vector store ({filtered} skipped as non-vectorizable)"
        )
    } else {
        format!("Indexed {indexed} {label} to vector store")
    }
}

/// Result of an indexing run.
#[derive(Debug, Clone)]
pub struct IndexResult {
    pub files_indexed: usize,
    pub entities_created: usize,
    pub relationships_created: usize,
    pub summaries_generated: usize,
    pub duration_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::graph::entities::{mk_entity, CodeVisibility, Language, TRIVIAL_FN_MAX_SPAN};

    /// Convenience for the common "public Rust code symbol" case.
    fn code(
        name: &str,
        kind: EntityKind,
        signature: Option<&str>,
        doc: Option<&str>,
    ) -> CodeEntity {
        mk_entity(
            name,
            kind,
            "f.rs",
            signature,
            CodeVisibility::Public,
            doc,
            Language::Rust,
            5,
        )
    }

    #[test]
    fn is_summarisable_excludes_non_code_languages() {
        // The real pollution case: a markdown file-entity has no child symbols,
        // so it must never be summarised (would hallucinate from a filename).
        for lang in [
            Language::Markdown,
            Language::Toml,
            Language::Yaml,
            Language::Json,
            Language::Unknown("txt".to_string()),
        ] {
            let f = mk_entity(
                "f",
                EntityKind::File,
                "f",
                None,
                CodeVisibility::Public,
                None,
                lang.clone(),
                0,
            );
            assert!(!f.is_summarisable(), "{lang:?} file should not summarise");
        }
    }

    #[test]
    fn is_summarisable_includes_code_files_and_modules() {
        assert!(code("file", EntityKind::File, None, None).is_summarisable());
        assert!(code("mod", EntityKind::Module, None, None).is_summarisable());
    }

    #[test]
    fn is_summarisable_scope_filter_only_drops_trivial_private_helpers() {
        let priv_tiny_nodoc = mk_entity(
            "t",
            EntityKind::Function,
            "f.rs",
            Some("fn t()"),
            CodeVisibility::Private,
            None,
            Language::Rust,
            TRIVIAL_FN_MAX_SPAN,
        );
        assert!(
            !priv_tiny_nodoc.is_summarisable(),
            "private + tiny + undocumented should be skipped"
        );

        // Any one of the three conditions failing keeps it summarisable.
        let documented = mk_entity(
            "d",
            EntityKind::Function,
            "f.rs",
            Some("fn d()"),
            CodeVisibility::Private,
            Some("does d"),
            Language::Rust,
            TRIVIAL_FN_MAX_SPAN,
        );
        let larger = mk_entity(
            "b",
            EntityKind::Function,
            "f.rs",
            Some("fn b()"),
            CodeVisibility::Private,
            None,
            Language::Rust,
            TRIVIAL_FN_MAX_SPAN + 1,
        );
        let public = mk_entity(
            "p",
            EntityKind::Function,
            "f.rs",
            Some("fn p()"),
            CodeVisibility::Public,
            None,
            Language::Rust,
            0,
        );
        assert!(documented.is_summarisable(), "documented helper kept");
        assert!(larger.is_summarisable(), "larger helper kept");
        assert!(public.is_summarisable(), "public helper kept");
    }

    #[test]
    fn is_summarisable_excludes_non_summary_kinds() {
        for kind in [
            EntityKind::Struct,
            EntityKind::Class,
            EntityKind::Trait,
            EntityKind::Enum,
            EntityKind::Constant,
            EntityKind::Import,
        ] {
            assert!(
                !code("x", kind.clone(), Some("sig"), None).is_summarisable(),
                "kind {kind:?} should not be summarisable"
            );
        }
    }

    #[test]
    fn build_summary_jobs_emits_one_job_per_summarisable_entity() {
        let entities = vec![
            code("func", EntityKind::Function, Some("fn func()"), None),
            code("meth", EntityKind::Method, Some("fn meth()"), None),
            code("file", EntityKind::File, None, None),
            code("modu", EntityKind::Module, None, None),
            // excluded: markdown file + a struct kind
            mk_entity(
                "readme",
                EntityKind::File,
                "README.md",
                None,
                CodeVisibility::Public,
                None,
                Language::Markdown,
                0,
            ),
            code("strukt", EntityKind::Struct, Some("struct S"), None),
        ];
        let jobs = build_summary_jobs(&entities);

        // Exactly one job per summarisable entity — the invariant the SQL
        // count in `count_summarisable_entities` must match.
        let summarisable = entities.iter().filter(|e| e.is_summarisable()).count();
        assert_eq!(jobs.len(), summarisable);
        assert_eq!(jobs.len(), 4);

        let g = |name: &str| {
            jobs.iter()
                .find(|j| j.qualified_name == format!("mod::{name}"))
                .map(|j| j.granularity.as_str())
        };
        assert_eq!(g("func"), Some("function"));
        assert_eq!(g("meth"), Some("function"));
        assert_eq!(g("file"), Some("file"));
        assert_eq!(g("modu"), Some("module"));
        assert_eq!(g("readme"), None);
        assert_eq!(g("strukt"), None);
    }

    #[test]
    fn function_job_uses_signature_then_falls_back_to_name() {
        let with_sig =
            build_summary_jobs(&[code("a", EntityKind::Function, Some("fn a()"), Some("d"))]);
        assert_eq!(with_sig[0].source, "fn a()");
        assert_eq!(with_sig[0].context, "d");

        let no_sig = build_summary_jobs(&[code("a", EntityKind::Function, None, None)]);
        assert_eq!(no_sig[0].source, "mod::a");
        assert_eq!(no_sig[0].context, "");
    }

    #[test]
    fn file_summary_input_lists_child_signatures_not_just_path() {
        let entities = vec![
            code("file", EntityKind::File, None, None),
            code(
                "alpha",
                EntityKind::Function,
                Some("fn alpha(x: u8) -> u8"),
                None,
            ),
            code("beta", EntityKind::Struct, Some("struct Beta"), None),
        ];
        let jobs = build_summary_jobs(&entities);
        let file = jobs
            .iter()
            .find(|j| j.granularity.as_str() == "file")
            .expect("file job");
        assert!(
            file.source.contains("fn alpha(x: u8) -> u8"),
            "expected child fn signature in file input, got: {}",
            file.source
        );
        assert!(
            file.source.contains("struct Beta"),
            "expected child struct in file input, got: {}",
            file.source
        );
        assert_ne!(
            file.source, "mod::file",
            "file input must not be a bare path"
        );
    }

    #[test]
    fn file_summary_falls_back_to_name_when_no_children() {
        let jobs = build_summary_jobs(&[mk_entity(
            "lonely",
            EntityKind::File,
            "lonely.rs",
            None,
            CodeVisibility::Public,
            None,
            Language::Rust,
            0,
        )]);
        assert_eq!(jobs[0].source, "mod::lonely");
    }

    #[test]
    fn source_hash_is_stable_and_input_sensitive() {
        assert_eq!(summary_source_hash("a", "b"), summary_source_hash("a", "b"));
        assert_ne!(summary_source_hash("a", "b"), summary_source_hash("a", "c"));
        // NUL separator: (ab,"") must not collide with (a,b).
        assert_ne!(summary_source_hash("ab", ""), summary_source_hash("a", "b"));
    }

    #[test]
    fn format_vector_index_outcome_without_filtering() {
        assert_eq!(
            format_vector_index_outcome("entities", 47, 47),
            "Indexed 47 entities to vector store"
        );
    }

    #[test]
    fn format_vector_index_outcome_with_filtering_appends_skipped_count() {
        assert_eq!(
            format_vector_index_outcome("entities", 50, 47),
            "Indexed 47 entities to vector store (3 skipped as non-vectorizable)"
        );
    }

    #[test]
    fn format_vector_index_outcome_zero_indexed_when_all_filtered() {
        assert_eq!(
            format_vector_index_outcome("summaries", 5, 0),
            "Indexed 0 summaries to vector store (5 skipped as non-vectorizable)"
        );
    }

    #[test]
    fn format_vector_index_outcome_saturates_on_unexpected_inversion() {
        assert_eq!(
            format_vector_index_outcome("entities", 3, 5),
            "Indexed 5 entities to vector store"
        );
    }
}
