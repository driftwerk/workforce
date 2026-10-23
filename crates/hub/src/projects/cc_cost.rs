//! Per-Slack-thread running cost ledger for `/cc`.
//!
//! Each turn's reported `cost_usd` accumulates here so the Slack card
//! can render `thread $X.XX` alongside the per-turn cost — the
//! retrospective's pain point was a $10+ thread cost being invisible
//! until completion. The ledger also raises a one-shot notice when the
//! running total crosses each `THRESHOLD_INCREMENT` band, so the user
//! sees a soft warning at $25 / $50 / … without being spammed every
//! turn afterwards.
//!
//! Storage layout: `{data_dir}/cc-costs/{slugified_session_key}.json`.
//! Atomic via tempfile-rename, same pattern as the brief store.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use eyre::{eyre, Result};
use serde::{Deserialize, Serialize};

/// Spend per running cost-band before the user gets a soft warning. A
/// $25 increment fires roughly every couple of medium /cc turns at
/// current Anthropic pricing — frequent enough to surprise the user
/// out of a runaway thread, sparse enough not to be noise.
pub const THRESHOLD_INCREMENT_USD: f64 = 25.0;

/// On-disk shape of the per-session cost record.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct CostSnapshot {
    pub total_cost_usd: f64,
    pub turn_count: u32,
    pub last_turn_at: DateTime<Utc>,
    /// Highest cost-band the user has already been warned about.
    /// `0.0` = never warned. Bumped by [`CostLedger::add_turn`] when a
    /// new threshold is crossed; persisted so a subsequent turn in
    /// the same band doesn't re-warn.
    #[serde(default)]
    pub last_warned_at_usd: f64,
    /// Schema marker so we can evolve the on-disk layout without
    /// silently misreading older files.
    #[serde(default = "schema_v1")]
    pub schema: String,
}

fn schema_v1() -> String {
    "cc-cost/v1".to_string()
}

impl Default for CostSnapshot {
    fn default() -> Self {
        Self {
            total_cost_usd: 0.0,
            turn_count: 0,
            last_turn_at: DateTime::<Utc>::default(),
            last_warned_at_usd: 0.0,
            schema: schema_v1(),
        }
    }
}

/// Result of adding a turn's cost to the ledger.
#[derive(Debug, Clone, PartialEq)]
pub struct CostUpdate {
    pub snapshot: CostSnapshot,
    /// `Some(band)` when this turn pushed the running total past a new
    /// `THRESHOLD_INCREMENT_USD` band; the dispatcher posts the soft
    /// warning card with this value. `None` otherwise.
    pub threshold_crossed: Option<f64>,
}

/// Filesystem-backed cost ledger rooted at `{data_dir}`.
#[derive(Debug, Clone)]
pub struct CostLedger {
    root: PathBuf,
}

impl CostLedger {
    pub fn new(data_dir: impl Into<PathBuf>) -> Self {
        Self {
            root: data_dir.into(),
        }
    }

    pub fn path_for(&self, session_key: &str) -> PathBuf {
        self.root
            .join("cc-costs")
            .join(format!("{}.json", slugify(session_key)))
    }

    /// Add `cost_usd` to the running total for `session_key`. Returns
    /// the post-update snapshot plus any threshold band the new total
    /// just crossed. Negative or zero `cost_usd` is a no-op (some Max
    /// OAuth turns report `total_cost_usd: 0` and we don't want those
    /// to bump `turn_count` either).
    pub async fn add_turn(&self, session_key: &str, cost_usd: f64) -> Result<CostUpdate> {
        if !cost_usd.is_finite() || cost_usd <= 0.0 {
            let snapshot = self.load(session_key).await.unwrap_or_default();
            return Ok(CostUpdate {
                snapshot,
                threshold_crossed: None,
            });
        }
        let mut snapshot = self.load(session_key).await.unwrap_or_default();
        snapshot.total_cost_usd += cost_usd;
        snapshot.turn_count = snapshot.turn_count.saturating_add(1);
        snapshot.last_turn_at = Utc::now();

        let crossed_band =
            next_threshold_band(snapshot.total_cost_usd, snapshot.last_warned_at_usd);
        if let Some(band) = crossed_band {
            snapshot.last_warned_at_usd = band;
        }

        self.write(session_key, &snapshot).await?;
        Ok(CostUpdate {
            snapshot,
            threshold_crossed: crossed_band,
        })
    }

    /// Load the snapshot for `session_key`, or `None` if no turns
    /// have completed yet (or the file is corrupt — corrupt files
    /// surface as `None` like in the brief store).
    pub async fn load(&self, session_key: &str) -> Option<CostSnapshot> {
        let path = self.path_for(session_key);
        let raw = tokio::fs::read_to_string(&path).await.ok()?;
        match serde_json::from_str::<CostSnapshot>(&raw) {
            Ok(s) => Some(s),
            Err(e) => {
                tracing::debug!(
                    target: "cc",
                    path = %path.display(),
                    error = %e,
                    "cc cost ledger parse failed; treating as missing"
                );
                None
            }
        }
    }

    async fn write(&self, session_key: &str, snapshot: &CostSnapshot) -> Result<()> {
        let path = self.path_for(session_key);
        let parent = path
            .parent()
            .ok_or_else(|| eyre!("cost ledger path has no parent: {}", path.display()))?;
        tokio::fs::create_dir_all(parent).await?;
        let body = serde_json::to_string_pretty(snapshot)?;
        let tmp = parent.join(format!(
            ".{}.{}.tmp",
            path.file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("ledger.json"),
            uuid::Uuid::new_v4(),
        ));
        tokio::fs::write(&tmp, body.as_bytes()).await?;
        tokio::fs::rename(&tmp, &path).await?;
        Ok(())
    }
}

/// Return the highest `THRESHOLD_INCREMENT_USD` band the running
/// `total` has crossed strictly above `last_warned`, or `None` if no
/// new band has been crossed. Bands are right-open: total $25.00 lands
/// in the [25, 50) band, total $24.99 doesn't yet.
fn next_threshold_band(total: f64, last_warned: f64) -> Option<f64> {
    let band = (total / THRESHOLD_INCREMENT_USD).floor() * THRESHOLD_INCREMENT_USD;
    if band > last_warned && band > 0.0 {
        Some(band)
    } else {
        None
    }
}

fn slugify(s: &str) -> String {
    let mut out = String::with_capacity(s.len().min(120));
    for c in s.chars().take(120) {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
            out.push(c);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        out.push('_');
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn add_turn_accumulates_cost_and_persists_snapshot() {
        let tmp = TempDir::new().unwrap();
        let ledger = CostLedger::new(tmp.path());
        let key = "slack:T1:C1:msg1";

        let u1 = ledger.add_turn(key, 0.50).await.unwrap();
        assert!((u1.snapshot.total_cost_usd - 0.50).abs() < 1e-9);
        assert_eq!(u1.snapshot.turn_count, 1);
        assert_eq!(u1.threshold_crossed, None);

        let u2 = ledger.add_turn(key, 1.25).await.unwrap();
        assert!((u2.snapshot.total_cost_usd - 1.75).abs() < 1e-9);
        assert_eq!(u2.snapshot.turn_count, 2);

        // Reload from disk to confirm persistence.
        let loaded = ledger.load(key).await.unwrap();
        assert!((loaded.total_cost_usd - 1.75).abs() < 1e-9);
        assert_eq!(loaded.turn_count, 2);
    }

    #[tokio::test]
    async fn add_turn_zero_or_negative_cost_is_a_noop() {
        let tmp = TempDir::new().unwrap();
        let ledger = CostLedger::new(tmp.path());
        ledger.add_turn("k", 1.0).await.unwrap();
        let mid = ledger.load("k").await.unwrap();

        let zero = ledger.add_turn("k", 0.0).await.unwrap();
        assert_eq!(zero.snapshot, mid, "zero-cost turn must not bump anything");

        let neg = ledger.add_turn("k", -5.0).await.unwrap();
        assert_eq!(
            neg.snapshot, mid,
            "negative-cost turn must not bump anything"
        );

        let nan = ledger.add_turn("k", f64::NAN).await.unwrap();
        assert_eq!(nan.snapshot, mid, "NaN cost must not corrupt the ledger");
    }

    #[tokio::test]
    async fn threshold_warning_fires_once_per_band_then_silences_until_next() {
        let tmp = TempDir::new().unwrap();
        let ledger = CostLedger::new(tmp.path());
        let key = "k";

        // Below the first $25 band.
        let u = ledger.add_turn(key, 10.0).await.unwrap();
        assert_eq!(u.threshold_crossed, None);

        // First crossing into [25, 50): warn at $25.
        let u = ledger.add_turn(key, 18.0).await.unwrap();
        assert_eq!(u.threshold_crossed, Some(25.0));

        // Still inside the same band: no second warning.
        let u = ledger.add_turn(key, 5.0).await.unwrap();
        assert_eq!(u.threshold_crossed, None);

        // Crossing $50 fires the next band warning.
        let u = ledger.add_turn(key, 25.0).await.unwrap();
        assert_eq!(u.threshold_crossed, Some(50.0));
    }

    #[tokio::test]
    async fn load_returns_none_for_unknown_session() {
        let tmp = TempDir::new().unwrap();
        let ledger = CostLedger::new(tmp.path());
        assert!(ledger.load("missing").await.is_none());
    }

    #[tokio::test]
    async fn load_returns_none_for_corrupt_file() {
        let tmp = TempDir::new().unwrap();
        let ledger = CostLedger::new(tmp.path());
        let path = ledger.path_for("k");
        tokio::fs::create_dir_all(path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&path, b"{not valid json").await.unwrap();
        assert!(ledger.load("k").await.is_none());
    }

    #[tokio::test]
    async fn write_path_is_anchored_under_data_dir() {
        let tmp = TempDir::new().unwrap();
        let ledger = CostLedger::new(tmp.path());
        let evil = "../../../etc/passwd";
        ledger.add_turn(evil, 0.01).await.unwrap();
        let path = ledger.path_for(evil);
        let canon_root = tmp.path().canonicalize().unwrap();
        let canon_parent = path.parent().unwrap().canonicalize().unwrap();
        assert!(canon_parent.starts_with(&canon_root));
    }

    #[tokio::test]
    async fn save_is_atomic_under_concurrent_writers() {
        // Two concurrent saves: file must always parse, total must
        // never be lost (we can't guarantee both deltas land — a
        // last-writer-wins race may eat one — but at least one must
        // win and the file stays parseable).
        let tmp = TempDir::new().unwrap();
        let ledger = CostLedger::new(tmp.path());
        let l1 = ledger.clone();
        let l2 = ledger.clone();
        let h1 = tokio::spawn(async move {
            l1.add_turn("k", 1.0).await.unwrap();
        });
        let h2 = tokio::spawn(async move {
            l2.add_turn("k", 2.0).await.unwrap();
        });
        h1.await.unwrap();
        h2.await.unwrap();
        let snap = ledger.load("k").await.unwrap();
        // At least one update must have landed.
        assert!(snap.total_cost_usd >= 1.0);
        assert!(snap.turn_count >= 1);
    }
}
