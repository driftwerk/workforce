//! Real-time output streaming for jobs via NATS.
//!
//! Provides throttled progress updates published to ephemeral NATS subjects.
//! Streaming is independent of JetStream — if the hub restarts, the job is
//! dead and there's no stream to resume.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use chrono::Utc;
use tokio::sync::Mutex;
use tracing::debug;

use workforce_nats::client::JetstreamPublisher;

use super::types::{JobId, StreamConfig, StreamProgress, DEFAULT_STREAM_THROTTLE_SECS};

/// Handle for writing streaming output from a running job.
///
/// Collects output lines and periodically publishes them as progress
/// updates via NATS. Thread-safe and cheaply cloneable.
#[derive(Clone)]
pub struct OutputStreamer {
    inner: Arc<StreamerInner>,
}

struct StreamerInner {
    job_id: JobId,
    agent_id: String,
    /// NATS publisher (if available).
    publisher: Option<Arc<dyn JetstreamPublisher>>,
    /// Whether streaming is currently enabled.
    enabled: AtomicBool,
    /// Throttle interval in seconds.
    throttle_secs: u64,
    /// Buffered lines since last flush.
    buffer: Mutex<Vec<String>>,
    /// Last time a progress update was sent.
    last_flush: Mutex<Option<Instant>>,
    /// Total bytes of output produced.
    bytes_total: AtomicU64,
    /// When the job started.
    start_time: Instant,
    /// Session injector for real-time updates.
    session_injector: Option<Arc<dyn super::runner::SessionInjector>>,
    /// Session channel for injection targeting.
    session_channel: Option<String>,
    /// Full output accumulator (for disk persistence alongside streaming).
    full_output: Mutex<String>,
}

impl OutputStreamer {
    /// Create a new output streamer.
    pub fn new(
        job_id: JobId,
        agent_id: String,
        config: &StreamConfig,
        publisher: Option<Arc<dyn JetstreamPublisher>>,
        session_injector: Option<Arc<dyn super::runner::SessionInjector>>,
        session_channel: Option<String>,
    ) -> Self {
        Self {
            inner: Arc::new(StreamerInner {
                job_id,
                agent_id,
                publisher,
                enabled: AtomicBool::new(config.enabled),
                throttle_secs: config.throttle_secs,
                buffer: Mutex::new(Vec::new()),
                last_flush: Mutex::new(None),
                bytes_total: AtomicU64::new(0),
                start_time: Instant::now(),
                session_injector,
                session_channel,
                full_output: Mutex::new(String::new()),
            }),
        }
    }

    /// Create a no-op streamer (streaming disabled, no publisher).
    pub fn noop(job_id: JobId) -> Self {
        Self {
            inner: Arc::new(StreamerInner {
                job_id,
                agent_id: String::new(),
                publisher: None,
                enabled: AtomicBool::new(false),
                throttle_secs: DEFAULT_STREAM_THROTTLE_SECS,
                buffer: Mutex::new(Vec::new()),
                last_flush: Mutex::new(None),
                bytes_total: AtomicU64::new(0),
                start_time: Instant::now(),
                session_injector: None,
                session_channel: None,
                full_output: Mutex::new(String::new()),
            }),
        }
    }

    /// Write output lines. Lines are buffered and flushed per throttle interval.
    pub async fn write_lines(&self, lines: &[String]) {
        if lines.is_empty() {
            return;
        }

        // Always accumulate into full output.
        {
            let mut full = self.inner.full_output.lock().await;
            for line in lines {
                if !full.is_empty() {
                    full.push('\n');
                }
                full.push_str(line);
                self.inner
                    .bytes_total
                    .fetch_add(line.len() as u64 + 1, Ordering::Relaxed);
            }
        }

        // If streaming is disabled, don't buffer for NATS.
        if !self.inner.enabled.load(Ordering::Relaxed) {
            return;
        }

        // Buffer lines.
        {
            let mut buffer = self.inner.buffer.lock().await;
            buffer.extend(lines.iter().cloned());
        }

        // Check if it's time to flush.
        self.maybe_flush().await;
    }

    /// Write a single line of output.
    pub async fn write_line(&self, line: &str) {
        self.write_lines(&[line.to_string()]).await;
    }

    /// Check if enough time has passed since the last flush and flush if so.
    async fn maybe_flush(&self) {
        let should_flush = {
            let last = self.inner.last_flush.lock().await;
            match *last {
                Some(last_time) => last_time.elapsed().as_secs() >= self.inner.throttle_secs,
                None => true, // First flush — always send.
            }
        };

        if should_flush {
            self.flush().await;
        }
    }

    /// Force flush buffered lines as a progress update.
    pub async fn flush(&self) {
        let lines = {
            let mut buffer = self.inner.buffer.lock().await;
            if buffer.is_empty() {
                return;
            }
            std::mem::take(&mut *buffer)
        };

        // Update last flush time.
        {
            let mut last = self.inner.last_flush.lock().await;
            *last = Some(Instant::now());
        }

        let progress = StreamProgress {
            job_id: self.inner.job_id.clone(),
            lines: lines.clone(),
            timestamp: Utc::now(),
            bytes_total: self.inner.bytes_total.load(Ordering::Relaxed),
            elapsed_secs: self.inner.start_time.elapsed().as_secs_f64(),
        };

        // Publish to NATS.
        if let Some(ref publisher) = self.inner.publisher {
            let subject = format!(
                "workforce.jobs.{}.progress.{}",
                self.inner.agent_id, self.inner.job_id
            );
            if let Err(e) =
                workforce_nats::client::publish_json(publisher.as_ref(), subject, &progress).await
            {
                debug!(
                    job_id = %self.inner.job_id,
                    error = %e,
                    "Failed to publish stream progress (non-fatal)"
                );
            }
        }

        // Inject into session.
        if let Some(ref injector) = self.inner.session_injector {
            if let Some(ref channel) = self.inner.session_channel {
                let elapsed = self.inner.start_time.elapsed().as_secs();
                let preview_lines: Vec<&str> =
                    lines.iter().rev().take(5).map(|s| s.as_str()).collect();
                let preview: Vec<&str> = preview_lines.into_iter().rev().collect();

                let msg = format!(
                    "⏳ Job `{}` — {}s elapsed\n```\n{}\n```",
                    self.inner.job_id,
                    elapsed,
                    preview.join("\n")
                );

                if let Err(e) = injector.inject_job_completion(channel, &msg).await {
                    debug!(
                        job_id = %self.inner.job_id,
                        error = %e,
                        "Failed to inject stream progress into session"
                    );
                }
            }
        }
    }

    /// Enable or disable streaming at runtime.
    pub fn set_enabled(&self, enabled: bool) {
        self.inner.enabled.store(enabled, Ordering::Relaxed);
        debug!(
            job_id = %self.inner.job_id,
            enabled,
            "Streaming toggled"
        );
    }

    /// Check if streaming is currently enabled.
    pub fn is_enabled(&self) -> bool {
        self.inner.enabled.load(Ordering::Relaxed)
    }

    /// Get the full accumulated output.
    pub async fn full_output(&self) -> String {
        self.inner.full_output.lock().await.clone()
    }

    /// Get total bytes produced.
    pub fn bytes_total(&self) -> u64 {
        self.inner.bytes_total.load(Ordering::Relaxed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::job_runner::types::StreamConfig;

    #[tokio::test]
    async fn test_noop_streamer() {
        let streamer = OutputStreamer::noop("j-test".to_string());
        streamer.write_line("hello").await;
        assert_eq!(streamer.full_output().await, "hello");
    }

    #[tokio::test]
    async fn test_full_output_accumulation() {
        let config = StreamConfig {
            enabled: true,
            throttle_secs: 5,
        };
        let streamer = OutputStreamer::new(
            "j-test".to_string(),
            "test-agent".to_string(),
            &config,
            None,
            None,
            None,
        );

        streamer.write_line("line 1").await;
        streamer.write_line("line 2").await;
        streamer.write_line("line 3").await;

        assert_eq!(streamer.full_output().await, "line 1\nline 2\nline 3");
        assert!(streamer.bytes_total() > 0);
    }

    #[tokio::test]
    async fn test_toggle_streaming() {
        let config = StreamConfig {
            enabled: true,
            throttle_secs: 5,
        };
        let streamer = OutputStreamer::new(
            "j-test".to_string(),
            "test-agent".to_string(),
            &config,
            None,
            None,
            None,
        );

        assert!(streamer.is_enabled());
        streamer.set_enabled(false);
        assert!(!streamer.is_enabled());
        streamer.set_enabled(true);
        assert!(streamer.is_enabled());
    }
}
