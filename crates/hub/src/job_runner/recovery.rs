//! Startup recovery for the job runner.
//!
//! On startup, scans persisted job manifests and chains and:
//! - Marks orphaned running/submitted jobs as failed (hub_restart).
//! - Resumes monitor jobs that were actively polling.
//! - Reconstructs in-progress chains and cascades failures.
//! - Resumes retrying jobs that have remaining attempts.

use chrono::Utc;
use eyre::Result;
use tracing::{info, warn};

use super::persistence::JobPersistence;
use super::retry;
use super::types::{JobChain, JobManifest, JobStatus};

/// Result of startup recovery.
#[derive(Debug, Default)]
pub struct RecoveryResult {
    /// Number of running/submitted jobs marked as failed.
    pub orphaned_failed: usize,
    /// Monitor jobs that should be resumed.
    pub monitors_to_resume: Vec<JobManifest>,
    /// Jobs waiting for retry that should be rescheduled.
    pub retries_to_resume: Vec<JobManifest>,
    /// Chains that need downstream cancellation.
    pub chains_with_failures: Vec<ChainRecovery>,
    /// Total manifests scanned.
    pub total_scanned: usize,
    /// Total chains scanned.
    pub chains_scanned: usize,
}

/// Recovery information for a chain with failed jobs.
#[derive(Debug)]
pub struct ChainRecovery {
    /// The chain that needs recovery.
    pub chain: JobChain,
    /// Step IDs that failed and need downstream cancellation.
    pub failed_steps: Vec<String>,
}

/// Perform startup recovery: scan persisted jobs and chains, handle orphans.
///
/// Returns the recovery result with jobs to resume and counts of
/// orphaned jobs that were marked failed.
pub async fn recover_on_startup(persistence: &JobPersistence) -> Result<RecoveryResult> {
    let manifests = persistence.scan_all_manifests().await?;
    let chains = persistence.scan_all_chains().await?;

    let mut result = RecoveryResult {
        total_scanned: manifests.len(),
        chains_scanned: chains.len(),
        ..Default::default()
    };

    for manifest in manifests {
        match manifest.status {
            JobStatus::Running | JobStatus::Submitted => {
                // Running or submitted jobs from previous instance are orphans.
                let mut updated = manifest.clone();

                // Check if this job has retry capability.
                if retry::should_retry(&updated) {
                    // Instead of marking as permanently failed, prepare for retry.
                    updated.status = JobStatus::Failed;
                    updated.error =
                        Some("hub_restart: job was running when hub stopped".to_string());
                    updated.completed_at = Some(Utc::now());
                    retry::prepare_retry(&mut updated, Some("hub_restart"));

                    if let Err(e) = persistence.save_manifest(&updated).await {
                        warn!(
                            job_id = %updated.job_id,
                            error = %e,
                            "Failed to save retry-prepared manifest"
                        );
                    }
                    result.retries_to_resume.push(updated);

                    info!(
                        job_id = %manifest.job_id,
                        "Orphaned job scheduled for retry after hub restart"
                    );
                } else {
                    updated.status = JobStatus::Failed;
                    updated.error =
                        Some("hub_restart: job was running when hub stopped".to_string());
                    updated.completed_at = Some(Utc::now());

                    if let Err(e) = persistence.save_manifest(&updated).await {
                        warn!(
                            job_id = %updated.job_id,
                            error = %e,
                            "Failed to mark orphaned job as failed"
                        );
                    }
                    result.orphaned_failed += 1;

                    info!(
                        job_id = %manifest.job_id,
                        job_type = %manifest.job_type,
                        label = ?manifest.label,
                        "Orphaned job marked as failed (hub_restart)"
                    );
                }
            }
            JobStatus::Monitoring => {
                // Monitor jobs can be resumed — they just need to keep polling.
                info!(
                    job_id = %manifest.job_id,
                    label = ?manifest.label,
                    "Monitor job will be resumed"
                );
                result.monitors_to_resume.push(manifest);
            }
            JobStatus::Retrying => {
                // Job was waiting for retry — check if backoff has elapsed.
                if retry::is_ready_for_retry(&manifest) {
                    info!(
                        job_id = %manifest.job_id,
                        "Retrying job ready to execute (backoff elapsed during restart)"
                    );
                } else {
                    let remaining = retry::remaining_delay_secs(&manifest);
                    info!(
                        job_id = %manifest.job_id,
                        remaining_secs = remaining,
                        "Retrying job will resume after backoff delay"
                    );
                }
                result.retries_to_resume.push(manifest);
            }
            // Terminal states need no action.
            JobStatus::Completed
            | JobStatus::Failed
            | JobStatus::Cancelled
            | JobStatus::DependencyFailed => {}
        }
    }

    // Recover chains.
    for mut chain in chains {
        if chain.status.is_terminal() {
            continue;
        }

        // Find steps that were running/submitted and are now orphaned.
        let mut failed_steps = Vec::new();
        for step in &mut chain.step_jobs {
            match step.status {
                JobStatus::Running | JobStatus::Submitted => {
                    // Check if the individual job was handled above (marked failed or retrying).
                    // Load the current manifest state.
                    match persistence.load_manifest(&step.job_id).await {
                        Ok(Some(job_manifest)) => {
                            step.status = job_manifest.status.clone();
                            if job_manifest.status == JobStatus::Failed {
                                failed_steps.push(step.step_id.clone());
                            }
                        }
                        _ => {
                            // Manifest missing — mark as failed.
                            step.status = JobStatus::Failed;
                            failed_steps.push(step.step_id.clone());
                        }
                    }
                }
                JobStatus::Retrying => {
                    // Job is being retried — no cascading needed yet.
                }
                _ => {}
            }
        }

        if !failed_steps.is_empty() {
            // Update chain status and save.
            chain.status = chain.compute_status();
            if let Err(e) = persistence.save_chain(&chain).await {
                warn!(
                    chain_id = %chain.id,
                    error = %e,
                    "Failed to save chain during recovery"
                );
            }
            result.chains_with_failures.push(ChainRecovery {
                chain,
                failed_steps,
            });
        } else {
            // No failures — just update the chain status.
            chain.status = chain.compute_status();
            if let Err(e) = persistence.save_chain(&chain).await {
                warn!(
                    chain_id = %chain.id,
                    error = %e,
                    "Failed to save chain status during recovery"
                );
            }
        }
    }

    if result.orphaned_failed > 0
        || !result.monitors_to_resume.is_empty()
        || !result.retries_to_resume.is_empty()
        || !result.chains_with_failures.is_empty()
    {
        info!(
            total_scanned = result.total_scanned,
            chains_scanned = result.chains_scanned,
            orphaned_failed = result.orphaned_failed,
            monitors_resumed = result.monitors_to_resume.len(),
            retries_resumed = result.retries_to_resume.len(),
            chains_recovered = result.chains_with_failures.len(),
            "Startup recovery complete"
        );
    }

    Ok(result)
}
