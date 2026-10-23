//! JSONL trace writer for LLM fine-tuning data.
//!
//! Writes trace events as newline-delimited JSON to files organised by
//! agent and date: `{base_dir}/{agent_id}/{YYYY-MM-DD}/{trace_id}.jsonl`.

use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::Mutex;

use eyre::Result;
use serde::Serialize;
use workforce_agent::trace::{TraceFooter, TraceHeader, TraceRound, TraceWriter};

/// How often a running hub re-prunes. A hub that never restarts still keeps
/// its window, and a pass costs one `read_dir` per agent.
const PRUNE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

/// What a prune pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct PruneOutcome {
    pub removed: usize,
    pub failed: usize,
}

/// Remove `{base_dir}/{agent}/{YYYY-MM-DD}` directories dated before `cutoff`.
///
/// Deliberately narrow: only a real directory (never a symlink — following one
/// would walk the sweep out of the traces tree and delete whatever it points
/// at) whose name is exactly `YYYY-MM-DD` and older than the cutoff. A file, a
/// symlink, a differently-named directory, or a date at any other depth is
/// left alone — the rules the incident cleanup verified against production
/// data before deleting anything.
pub fn prune_old_traces(base_dir: &std::path::Path, cutoff: chrono::NaiveDate) -> PruneOutcome {
    let agents = match fs::read_dir(base_dir) {
        Ok(agents) => agents,
        Err(e) => {
            if e.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %base_dir.display(), error = %e, "Cannot read trace directory to prune");
            }
            return PruneOutcome::default();
        }
    };

    let mut outcome = PruneOutcome::default();
    for agent in agents.flatten().filter(is_real_dir) {
        let days = match fs::read_dir(agent.path()) {
            Ok(days) => days,
            Err(e) => {
                tracing::warn!(path = %agent.path().display(), error = %e, "Cannot read agent trace directory");
                continue;
            }
        };
        for day in days.flatten().filter(|day| is_expired_day(day, cutoff)) {
            match fs::remove_dir_all(day.path()) {
                Ok(()) => outcome.removed += 1,
                Err(e) => {
                    outcome.failed += 1;
                    tracing::warn!(
                        path = %day.path().display(),
                        error = %e,
                        "Failed to prune trace directory"
                    );
                }
            }
        }
    }
    outcome
}

/// A directory that is not a symlink. `DirEntry::file_type` does not traverse
/// links, where `Path::is_dir` does — the difference is whether a symlink
/// planted in the traces tree can redirect the sweep at live data.
fn is_real_dir(entry: &fs::DirEntry) -> bool {
    entry.file_type().is_ok_and(|kind| kind.is_dir())
}

/// Whether a `{agent}/{day}` entry is a dated directory that has aged out.
fn is_expired_day(entry: &fs::DirEntry, cutoff: chrono::NaiveDate) -> bool {
    is_real_dir(entry)
        && entry
            .file_name()
            .to_str()
            .and_then(parse_day)
            .is_some_and(|day| day < cutoff)
}

/// Parse exactly `YYYY-MM-DD`. chrono alone accepts `2020-1-1`, a leading
/// space and a signed year, all of which widen what the sweep will delete.
fn parse_day(name: &str) -> Option<chrono::NaiveDate> {
    let shaped = name.len() == 10
        && name.bytes().enumerate().all(|(i, b)| {
            if i == 4 || i == 7 {
                b == b'-'
            } else {
                b.is_ascii_digit()
            }
        });
    shaped
        .then(|| chrono::NaiveDate::parse_from_str(name, "%Y-%m-%d").ok())
        .flatten()
}

/// What a compaction pass did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct CompactOutcome {
    pub compressed: usize,
    pub failed: usize,
    pub bytes_before: u64,
    pub bytes_after: u64,
}

/// Gzip the JSONL of every day directory before `live_day`.
///
/// Traces are highly repetitive JSON and compress roughly ten to one, so a
/// window costs a tenth of the disk. Only days no writer can still be
/// appending to are touched: each agent holds one file open for the life of
/// the process, and that file lives in the boot day's directory.
pub fn compact_old_traces(
    base_dir: &std::path::Path,
    live_day: chrono::NaiveDate,
) -> CompactOutcome {
    let Ok(agents) = fs::read_dir(base_dir) else {
        return CompactOutcome::default();
    };
    let mut outcome = CompactOutcome::default();
    for agent in agents.flatten().filter(is_real_dir) {
        let Ok(days) = fs::read_dir(agent.path()) else {
            continue;
        };
        for day in days
            .flatten()
            .filter(|day| is_compactable_day(day, live_day))
        {
            let Ok(files) = fs::read_dir(day.path()) else {
                continue;
            };
            for file in files.flatten().filter(is_uncompressed_trace) {
                match compress_file(&file.path()) {
                    Ok((before, after)) => {
                        outcome.compressed += 1;
                        outcome.bytes_before += before;
                        outcome.bytes_after += after;
                    }
                    Err(e) => {
                        outcome.failed += 1;
                        tracing::warn!(path = %file.path().display(), error = %e, "Failed to compress trace file");
                    }
                }
            }
        }
    }
    outcome
}

/// A dated directory old enough that no writer can still be appending to it.
fn is_compactable_day(entry: &fs::DirEntry, live_day: chrono::NaiveDate) -> bool {
    is_real_dir(entry)
        && entry
            .file_name()
            .to_str()
            .and_then(parse_day)
            .is_some_and(|day| day < live_day)
}

/// A plain `.jsonl` file — never a symlink, never one already compressed.
fn is_uncompressed_trace(entry: &fs::DirEntry) -> bool {
    entry.file_type().is_ok_and(|kind| kind.is_file())
        && entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.ends_with(".jsonl"))
}

/// Gzip `path` beside itself, then drop the original. Written to a temporary
/// first so an interrupted pass never leaves a truncated `.gz` standing in for
/// the trace it replaced.
fn compress_file(path: &std::path::Path) -> std::io::Result<(u64, u64)> {
    use std::io::Read;

    let before = fs::metadata(path)?.len();
    let staging = path.with_extension("jsonl.gz.tmp");
    let mut source = fs::File::open(path)?;
    let staged = workforce_common::private_fs::private_file_options()
        .write(true)
        .create(true)
        .truncate(true)
        .open(&staging)?;
    let mut encoder = flate2::write::GzEncoder::new(staged, flate2::Compression::default());
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        match source.read(&mut buf)? {
            0 => break,
            n => encoder.write_all(&buf[..n])?,
        }
    }
    encoder.finish()?.sync_all()?;

    let target = path.with_extension("jsonl.gz");
    fs::rename(&staging, &target)?;
    let after = fs::metadata(&target)?.len();
    fs::remove_file(path)?;
    Ok((before, after))
}

/// The oldest day a pass keeps: `retention_days` back, but never past the day
/// this process booted. Each agent's trace file is opened once at boot and
/// held open for the process lifetime, so pruning that day on a hub older than
/// the window would leave every later trace writing to an unlinked inode.
fn prune_cutoff(retention_days: u32, boot_day: chrono::NaiveDate) -> Option<chrono::NaiveDate> {
    chrono::Utc::now()
        .date_naive()
        .checked_sub_days(chrono::Days::new(u64::from(retention_days)))
        .map(|cutoff| cutoff.min(boot_day))
}

/// Prune at boot, then every [`PRUNE_INTERVAL`], until `drain` is cancelled.
///
/// Spawns nothing when retention is disabled. Only the leader prunes: the
/// traces tree is on shared storage, so in a clustered deployment two hubs
/// sweeping it would race each other into spurious failures.
pub fn spawn_pruner(
    base_dir: PathBuf,
    retention_days: u32,
    leader_flag: Option<Arc<std::sync::atomic::AtomicBool>>,
    drain: tokio_util::sync::CancellationToken,
) {
    if retention_days == 0 {
        tracing::info!(
            path = %base_dir.display(),
            "Trace retention disabled — trace files are kept forever"
        );
        return;
    }
    tracing::info!(
        path = %base_dir.display(),
        retention_days,
        "Trace retention active"
    );
    let boot_day = chrono::Utc::now().date_naive();
    tokio::spawn(async move {
        if leader_flag.is_some() {
            tokio::time::sleep(crate::leader_gate::BOOT_SINGLETON_DELAY).await;
            let lead = leader_flag
                .as_ref()
                .map(|f| f.load(std::sync::atomic::Ordering::Relaxed))
                .unwrap_or(true);
            if !lead {
                tracing::info!("Trace prune skipped — not the leader");
                return;
            }
        }
        let mut tick = tokio::time::interval(PRUNE_INTERVAL);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                _ = drain.cancelled() => break,
                _ = tick.tick() => {}
            }
            let Some(cutoff) = prune_cutoff(retention_days, boot_day) else {
                continue;
            };
            let dir = base_dir.clone();
            // Minutes of synchronous `remove_dir_all` over NFS must not park a
            // runtime worker, least of all during hub bring-up.
            let live_day = chrono::Utc::now().date_naive().min(boot_day);
            let dir_for_compact = base_dir.clone();
            let (pruned, compacted) = tokio::task::spawn_blocking(move || {
                // Prune first: no point compressing what is about to go.
                let pruned = prune_old_traces(&dir, cutoff);
                (pruned, compact_old_traces(&dir_for_compact, live_day))
            })
            .await
            .unwrap_or_default();
            if pruned.removed > 0 || pruned.failed > 0 {
                tracing::info!(
                    removed = pruned.removed,
                    failed = pruned.failed,
                    retention_days,
                    "Pruned expired trace directories"
                );
            }
            if compacted.compressed > 0 || compacted.failed > 0 {
                tracing::info!(
                    compressed = compacted.compressed,
                    failed = compacted.failed,
                    bytes_before = compacted.bytes_before,
                    bytes_after = compacted.bytes_after,
                    "Compressed closed trace files"
                );
            }
        }
    });
}

/// A trace event wrapper that adds a `type` discriminator for JSONL parsing.
#[derive(Serialize)]
struct TraceEnvelope<'a, T: Serialize> {
    r#type: &'a str,
    #[serde(flatten)]
    data: &'a T,
}

/// Writes trace data as JSONL files to the local filesystem.
pub struct JsonlTraceWriter {
    /// The opened file handle, protected by a mutex for thread safety.
    file: Mutex<std::io::BufWriter<fs::File>>,
}

impl JsonlTraceWriter {
    /// Create a new JSONL trace writer.
    ///
    /// Creates the directory structure `{base_dir}/{agent_id}/{YYYY-MM-DD}/`
    /// and opens a new file named `{trace_id}.jsonl`. Traces carry every
    /// prompt and raw tool output of every user of the agent, so the root is
    /// closed to the hub.
    pub fn new(base_dir: &std::path::Path, agent_id: &str) -> Result<Self> {
        let today = chrono::Utc::now().format("%Y-%m-%d").to_string();
        let trace_id = uuid::Uuid::new_v4().to_string();

        workforce_common::private_fs::ensure_private_dir(base_dir)?;
        let dir = base_dir.join(agent_id).join(&today);
        fs::create_dir_all(&dir)?;

        let file_path = dir.join(format!("{}.jsonl", trace_id));
        let file = workforce_common::private_fs::private_file_options()
            .write(true)
            .create_new(true)
            .open(&file_path)?;
        let writer = std::io::BufWriter::new(file);

        tracing::info!(
            path = %file_path.display(),
            agent = %agent_id,
            "Opened trace file"
        );

        Ok(Self {
            file: Mutex::new(writer),
        })
    }

    /// Write a single JSON line with a type discriminator.
    fn write_line<T: Serialize>(&self, type_name: &str, data: &T) -> Result<()> {
        let envelope = TraceEnvelope {
            r#type: type_name,
            data,
        };
        let json = serde_json::to_string(&envelope)?;
        let mut file = self
            .file
            .lock()
            .map_err(|e| eyre::eyre!("Trace file mutex poisoned: {}", e))?;
        writeln!(file, "{}", json)?;
        file.flush()?;
        Ok(())
    }
}

impl TraceWriter for JsonlTraceWriter {
    fn write_header(&self, header: &TraceHeader) -> Result<()> {
        self.write_line("header", header)
    }

    fn write_round(&self, round: &TraceRound) -> Result<()> {
        self.write_line("round", round)
    }

    fn write_footer(&self, footer: &TraceFooter) -> Result<()> {
        self.write_line("footer", footer)
    }
}

/// Resolve the traces directory from config.
///
/// Returns `Some(path)` if tracing is enabled, `None` otherwise.
pub fn resolve_traces_dir(config: &workforce_common::settings::WorkforceConfig) -> Option<PathBuf> {
    config.tracing.enabled.then(|| traces_dir(config))
}

/// Where traces live, whether or not capture is on. Cleanup must not depend on
/// `enabled`: turning capture off is the obvious response to a full disk, and
/// it would otherwise guarantee the existing files are never reclaimed.
pub fn traces_dir(config: &workforce_common::settings::WorkforceConfig) -> PathBuf {
    config
        .tracing
        .traces_dir
        .as_ref()
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(&config.hub.data_dir).join("traces"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cutoff_days_ago(n: i64) -> chrono::NaiveDate {
        chrono::Utc::now().date_naive() - chrono::Duration::days(n)
    }

    fn day_dir(base: &std::path::Path, agent: &str, day: &str) -> PathBuf {
        let dir = base.join(agent).join(day);
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("trace.jsonl"), "{}").unwrap();
        dir
    }

    #[cfg(unix)]
    #[test]
    fn the_writer_closes_the_traces_root_and_creates_its_file_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let other_bits =
            |p: &std::path::Path| fs::metadata(p).unwrap().permissions().mode() & 0o077;
        let base = tempfile::tempdir().unwrap();
        fs::set_permissions(base.path(), fs::Permissions::from_mode(0o2775)).unwrap();

        JsonlTraceWriter::new(base.path(), "agent").unwrap();

        assert_eq!(other_bits(base.path()), 0);
        let day = fs::read_dir(base.path().join("agent"))
            .unwrap()
            .next()
            .unwrap()
            .unwrap();
        let trace = fs::read_dir(day.path()).unwrap().next().unwrap().unwrap();
        assert_eq!(other_bits(&trace.path()), 0);
    }

    fn day_name(n: i64) -> String {
        cutoff_days_ago(n).format("%Y-%m-%d").to_string()
    }

    #[test]
    fn prunes_only_days_older_than_the_cutoff() {
        let tmp = tempfile::tempdir().unwrap();
        let old = day_dir(tmp.path(), "atlas", &day_name(30));
        let before = day_dir(tmp.path(), "atlas", &day_name(15));
        let on_cutoff = day_dir(tmp.path(), "atlas", &day_name(14));
        let inside = day_dir(tmp.path(), "atlas", &day_name(3));
        let today = day_dir(tmp.path(), "atlas", &day_name(0));

        let outcome = prune_old_traces(tmp.path(), cutoff_days_ago(14));

        assert_eq!(
            outcome,
            PruneOutcome {
                removed: 2,
                failed: 0
            }
        );
        assert!(!old.exists() && !before.exists());
        assert!(
            on_cutoff.exists(),
            "the cutoff day itself is kept — the comparison is strictly older"
        );
        assert!(inside.exists() && today.exists());
        assert!(
            tmp.path().join("atlas").is_dir(),
            "agent dir is never removed"
        );
    }

    /// The measured escape: `Path::is_dir` follows symlinks, so an agent-level
    /// link would walk the sweep out of the traces tree and delete its target.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_agent_directory_is_never_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live-data");
        fs::create_dir_all(live.join(day_name(30))).unwrap();
        fs::write(live.join(day_name(30)).join("vault.enc"), "secrets").unwrap();

        let traces = tmp.path().join("traces");
        fs::create_dir_all(&traces).unwrap();
        std::os::unix::fs::symlink(&live, traces.join("agent-link")).unwrap();

        let outcome = prune_old_traces(&traces, cutoff_days_ago(14));

        assert_eq!(outcome, PruneOutcome::default());
        assert!(
            live.join(day_name(30)).join("vault.enc").exists(),
            "live data deleted"
        );
        assert!(
            traces.join("agent-link").exists(),
            "the link itself is left alone"
        );
    }

    /// A day-level link must lose only the link, never the target's contents.
    #[test]
    #[cfg(unix)]
    fn a_symlinked_day_directory_is_never_followed() {
        let tmp = tempfile::tempdir().unwrap();
        let live = tmp.path().join("live-data");
        fs::create_dir_all(&live).unwrap();
        fs::write(live.join("vault.enc"), "secrets").unwrap();

        let agent = tmp.path().join("traces/atlas");
        fs::create_dir_all(&agent).unwrap();
        std::os::unix::fs::symlink(&live, agent.join(day_name(30))).unwrap();

        let outcome = prune_old_traces(&tmp.path().join("traces"), cutoff_days_ago(14));

        assert_eq!(outcome, PruneOutcome::default());
        assert!(live.join("vault.enc").exists());
    }

    #[test]
    fn leaves_everything_that_is_not_a_dated_directory() {
        let tmp = tempfile::tempdir().unwrap();
        let agent = tmp.path().join("atlas");
        fs::create_dir_all(&agent).unwrap();
        let named = agent.join("archive");
        fs::create_dir_all(&named).unwrap();
        let stray = agent.join("notes.txt");
        fs::write(&stray, "keep me").unwrap();
        let file_shaped = agent.join(day_name(30));
        fs::write(&file_shaped, "not a directory").unwrap();

        let outcome = prune_old_traces(tmp.path(), cutoff_days_ago(14));

        assert_eq!(
            outcome,
            PruneOutcome::default(),
            "a file is skipped, not failed"
        );
        assert!(named.is_dir() && stray.is_file() && file_shaped.is_file());
    }

    /// chrono alone accepts `2020-1-1`, a leading space and a signed year;
    /// every one of those widens what the sweep deletes.
    #[test]
    fn only_exactly_shaped_dates_are_eligible() {
        let tmp = tempfile::tempdir().unwrap();
        let kept: Vec<PathBuf> = [
            "2020-1-1",
            " 2020-01-01",
            "+2020-01-01",
            "2020-01-01extra",
            "20200101",
            "2020-01-01 ",
            "2020-02-30",
        ]
        .iter()
        .map(|name| day_dir(tmp.path(), "atlas", name))
        .collect();
        let eligible = day_dir(tmp.path(), "atlas", "2020-01-01");

        let outcome = prune_old_traces(tmp.path(), cutoff_days_ago(14));

        assert_eq!(outcome.removed, 1);
        assert!(!eligible.exists());
        for dir in kept {
            assert!(dir.exists(), "{} was deleted", dir.display());
        }
    }

    /// Only `{base}/{agent}/{day}` is swept: a dated directory anywhere else
    /// is somebody else's data.
    #[test]
    fn dates_at_other_depths_are_ignored() {
        let tmp = tempfile::tempdir().unwrap();
        let shallow = tmp.path().join("2020-01-01");
        fs::create_dir_all(&shallow).unwrap();
        let deep = tmp.path().join("atlas/sub/2020-01-01");
        fs::create_dir_all(&deep).unwrap();

        let outcome = prune_old_traces(tmp.path(), cutoff_days_ago(14));

        assert_eq!(outcome, PruneOutcome::default());
        assert!(shallow.exists() && deep.exists());
    }

    #[test]
    fn a_failed_removal_is_counted_not_propagated() {
        let tmp = tempfile::tempdir().unwrap();
        let doomed = day_dir(tmp.path(), "atlas", "2020-01-01");
        let ok = day_dir(tmp.path(), "bernard", "2020-01-02");

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            // A read-only agent directory blocks unlinking its children.
            let agent = tmp.path().join("atlas");
            fs::set_permissions(&agent, fs::Permissions::from_mode(0o500)).unwrap();

            let outcome = prune_old_traces(tmp.path(), cutoff_days_ago(14));

            fs::set_permissions(&agent, fs::Permissions::from_mode(0o700)).unwrap();
            assert_eq!(outcome.failed, 1, "the blocked directory is reported");
            assert_eq!(outcome.removed, 1, "and the sweep continues past it");
            assert!(doomed.exists() && !ok.exists());
        }
    }

    #[test]
    fn compacts_closed_days_and_leaves_the_live_one() {
        let tmp = tempfile::tempdir().unwrap();
        let old = day_dir(tmp.path(), "atlas", &day_name(3));
        let live = day_dir(tmp.path(), "atlas", &day_name(0));
        let body = "{\"round\":1}\n".repeat(500);
        fs::write(old.join("trace.jsonl"), &body).unwrap();

        let outcome = compact_old_traces(tmp.path(), cutoff_days_ago(0));

        assert_eq!(outcome.compressed, 1);
        assert_eq!(outcome.failed, 0);
        assert!(
            outcome.bytes_after < outcome.bytes_before / 5,
            "gzip should win big on JSONL"
        );
        assert!(!old.join("trace.jsonl").exists(), "the original is dropped");
        assert!(
            !old.join("trace.jsonl.gz.tmp").exists(),
            "no staging left behind"
        );
        assert!(
            live.join("trace.jsonl").exists(),
            "a writer may still be appending to the live day"
        );

        let compressed = fs::read(old.join("trace.jsonl.gz")).unwrap();
        let mut decoded = String::new();
        std::io::Read::read_to_string(
            &mut flate2::read::GzDecoder::new(&compressed[..]),
            &mut decoded,
        )
        .unwrap();
        assert_eq!(decoded, body, "content round-trips");
    }

    #[test]
    fn compaction_is_idempotent_and_skips_non_traces() {
        let tmp = tempfile::tempdir().unwrap();
        let old = day_dir(tmp.path(), "atlas", &day_name(3));
        fs::write(old.join("notes.txt"), "not a trace").unwrap();
        fs::write(old.join("already.jsonl.gz"), "pretend-gzip").unwrap();

        let first = compact_old_traces(tmp.path(), cutoff_days_ago(0));
        assert_eq!(first.compressed, 1, "only the .jsonl is compressed");

        let second = compact_old_traces(tmp.path(), cutoff_days_ago(0));
        assert_eq!(second, CompactOutcome::default(), "nothing left to do");
        assert!(old.join("notes.txt").exists());
        assert_eq!(
            fs::read_to_string(old.join("already.jsonl.gz")).unwrap(),
            "pretend-gzip"
        );
    }

    /// Compressed traces still age out — compaction shrinks the window, it
    /// does not replace it.
    #[test]
    fn compressed_days_are_still_pruned() {
        let tmp = tempfile::tempdir().unwrap();
        let old = day_dir(tmp.path(), "atlas", &day_name(30));
        compact_old_traces(tmp.path(), cutoff_days_ago(0));
        assert!(old.join("trace.jsonl.gz").exists());

        let outcome = prune_old_traces(tmp.path(), cutoff_days_ago(14));

        assert_eq!(outcome.removed, 1);
        assert!(!old.exists());
    }

    #[test]
    fn a_missing_base_directory_is_a_no_op() {
        let tmp = tempfile::tempdir().unwrap();
        let outcome = prune_old_traces(&tmp.path().join("never-written"), cutoff_days_ago(14));
        assert_eq!(outcome, PruneOutcome::default());
    }

    #[test]
    fn a_day_directory_is_removed_with_its_contents() {
        let tmp = tempfile::tempdir().unwrap();
        let old = day_dir(tmp.path(), "atlas", &day_name(30));
        fs::create_dir_all(old.join("nested")).unwrap();
        fs::write(old.join("nested/more.jsonl"), "{}").unwrap();

        let outcome = prune_old_traces(tmp.path(), cutoff_days_ago(14));

        assert_eq!(outcome.removed, 1, "counts directories, not files");
        assert!(!old.exists());
    }

    #[test]
    fn every_agent_is_swept() {
        let tmp = tempfile::tempdir().unwrap();
        let a = day_dir(tmp.path(), "atlas", &day_name(30));
        let b = day_dir(tmp.path(), "bernard", &day_name(20));
        let kept = day_dir(tmp.path(), "bernard", &day_name(1));

        let outcome = prune_old_traces(tmp.path(), cutoff_days_ago(14));

        assert_eq!(outcome.removed, 2);
        assert!(!a.exists() && !b.exists() && kept.exists());
    }

    /// Each agent's trace file is opened once at boot and held for the process
    /// lifetime, so a hub older than the window must not prune the day its
    /// writers are still writing into.
    #[test]
    fn the_cutoff_never_passes_the_boot_day() {
        let boot = cutoff_days_ago(40);
        let cutoff = prune_cutoff(14, boot).unwrap();
        assert_eq!(
            cutoff, boot,
            "a long-lived hub protects its open trace files"
        );

        let fresh_boot = cutoff_days_ago(0);
        assert_eq!(prune_cutoff(14, fresh_boot).unwrap(), cutoff_days_ago(14));
    }

    #[test]
    fn an_absurd_retention_window_prunes_nothing() {
        let tmp = tempfile::tempdir().unwrap();
        let old = day_dir(tmp.path(), "atlas", "2020-01-01");
        let cutoff = prune_cutoff(u32::MAX, chrono::Utc::now().date_naive());
        assert!(cutoff.is_none(), "an unrepresentable cutoff prunes nothing");
        assert!(old.exists());
    }
}
