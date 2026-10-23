//! Backup: replicate the goldens and caches into the backup pool nightly,
//! incrementally where a common snapshot exists, and stream a full image to
//! the bucket weekly. Task clones are disposable and never backed up.

use chrono::{DateTime, Datelike, Utc, Weekday};
use eyre::Result;
use tracing::info;

use crate::ports::{Dataset, Snapshot};
use crate::scopes::Worker;

const WEEKLY_STORE_DAY: Weekday = Weekday::Sun;

#[derive(Debug, Default, PartialEq, Eq)]
pub struct BackupReport {
    pub replicated: Vec<String>,
    pub stored: Vec<String>,
}

pub async fn backup(worker: &Worker, now: DateTime<Utc>) -> Result<BackupReport> {
    let zfs = worker.zfs();
    let stamp = now.format("%Y%m%dT%H%M%SZ").to_string();
    let mut report = BackupReport::default();

    let mut sources = zfs.children(&worker.pool().child("golden")).await?;
    for cache in zfs.children(&worker.pool().child("caches")).await? {
        zfs.snapshot(&cache, &stamp).await?;
        sources.push(cache);
    }

    for source in sources {
        let Some(newest) = worker.newest_snapshot(&source).await? else {
            continue;
        };
        let target = backup_target(worker, &source);
        let common = common_snapshot(worker, &source, &target).await?;
        prune_target(worker, &target, common.as_ref(), now).await?;
        if common.as_ref() != Some(&newest) {
            if let Some(parent) = target
                .parent()
                .filter(|parent| parent.0 != worker.config().backup_pool)
            {
                if !zfs.exists(&parent).await? {
                    zfs.create(&parent, &[]).await?;
                }
            }
            zfs.send_receive(common.as_ref(), &newest, &target).await?;
            info!(%source, %target, "replicated");
            report.replicated.push(source.0.clone());
        }
        if now.weekday() == WEEKLY_STORE_DAY {
            let uri = format!(
                "{}/{}@{}.zst",
                worker.config().backup_bucket_prefix.trim_end_matches('/'),
                source.name(),
                newest.name
            );
            worker.cloud().store_snapshot(&newest, &uri).await?;
            report.stored.push(uri);
        }
    }
    Ok(report)
}

/// Destroy the target's snapshots past retention, before sending, so a pool
/// that filled up recovers on the next run instead of failing every send.
/// Kept: the target's newest whatever its age, and `common`, which the send
/// is incremental from. Without this the backup pool keeps every night's
/// snapshot forever.
async fn prune_target(
    worker: &Worker,
    target: &Dataset,
    common: Option<&Snapshot>,
    now: DateTime<Utc>,
) -> Result<()> {
    let zfs = worker.zfs();
    if !zfs.exists(target).await? {
        return Ok(());
    }
    let retention = chrono::Duration::days(worker.config().snapshot_retention_days as i64);
    let mut snapshots = zfs.snapshots(target).await?;
    snapshots.pop();
    for info in snapshots {
        let base = common.is_some_and(|common| common.name == info.snapshot.name);
        if !base && info.created + retention <= now {
            zfs.destroy_snapshot(&info.snapshot).await?;
            info!(snapshot = %info.snapshot, "pruned backup snapshot");
        }
    }
    Ok(())
}

/// The snapshot of `source` the next backup sends incrementally from.
pub(crate) async fn backup_base(worker: &Worker, source: &Dataset) -> Result<Option<Snapshot>> {
    common_snapshot(worker, source, &backup_target(worker, source)).await
}

fn backup_target(worker: &Worker, source: &Dataset) -> Dataset {
    let relative = source
        .0
        .strip_prefix(&worker.config().pool)
        .unwrap_or(&source.0)
        .trim_start_matches('/');
    Dataset(format!("{}/{relative}", worker.config().backup_pool))
}

/// The newest snapshot the target already holds that the source still has.
async fn common_snapshot(
    worker: &Worker,
    source: &Dataset,
    target: &Dataset,
) -> Result<Option<Snapshot>> {
    let zfs = worker.zfs();
    if !zfs.exists(target).await? {
        return Ok(None);
    }
    let held: Vec<String> = zfs
        .snapshots(target)
        .await?
        .into_iter()
        .map(|i| i.snapshot.name)
        .collect();
    let on_source = zfs.snapshots(source).await?;
    Ok(on_source
        .into_iter()
        .rev()
        .find(|info| held.contains(&info.snapshot.name))
        .map(|info| info.snapshot))
}

pub fn describe(report: &BackupReport) -> String {
    format!(
        "replicated {} dataset(s), stored {} to the bucket",
        report.replicated.len(),
        report.stored.len()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fakes::Harness;
    use chrono::TimeZone;

    fn monday() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 7, 3, 30, 0).unwrap()
    }

    fn sunday() -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 9, 13, 3, 30, 0).unwrap()
    }

    #[tokio::test]
    async fn the_first_backup_is_full_and_the_next_is_incremental_from_the_common_snapshot() {
        let h = Harness::new().await;
        h.zfs.add_snapshot("tank/golden/rails", "a");
        let report = backup(&h.worker, monday()).await.unwrap();
        assert_eq!(
            report.replicated,
            vec![
                "tank/golden/rails",
                "tank/caches/gradle",
                "tank/caches/pnpm"
            ]
        );
        assert_eq!(
            h.zfs.replications(),
            vec![
                (
                    None,
                    "tank/golden/rails@a".to_string(),
                    "backup/golden/rails".to_string()
                ),
                (
                    None,
                    "tank/caches/gradle@20260907T033000Z".to_string(),
                    "backup/caches/gradle".to_string()
                ),
                (
                    None,
                    "tank/caches/pnpm@20260907T033000Z".to_string(),
                    "backup/caches/pnpm".to_string()
                )
            ]
        );

        h.zfs.add_snapshot("tank/golden/rails", "b");
        let report = backup(&h.worker, monday() + chrono::Duration::days(1))
            .await
            .unwrap();
        assert!(report.replicated.contains(&"tank/golden/rails".to_string()));
        let last = h
            .zfs
            .replications()
            .into_iter()
            .find(|r| r.1 == "tank/golden/rails@b")
            .unwrap();
        assert_eq!(last.0.as_deref(), Some("tank/golden/rails@a"));
        assert!(report.stored.is_empty());
    }

    #[tokio::test]
    async fn the_first_backup_creates_the_mirror_parent_in_the_backup_pool() {
        let h = Harness::new().await;
        h.zfs.add_snapshot("tank/golden/rails", "a");
        assert!(!h.zfs.exists_sync("backup/golden"));
        backup(&h.worker, monday()).await.unwrap();
        assert!(h.zfs.exists_sync("backup/golden"));
        assert!(h.zfs.exists_sync("backup/caches"));
        assert!(h.zfs.exists_sync("backup/golden/rails"));
    }

    #[tokio::test]
    async fn the_backup_pool_drops_snapshots_past_retention_but_keeps_the_incremental_base() {
        let h = Harness::new().await;
        let day = |n| monday() - chrono::Duration::days(n);
        for (name, age) in [("old", 20), ("base", 10), ("recent", 2)] {
            h.zfs.add_snapshot_at("backup/golden/rails", name, day(age));
        }
        h.zfs.add_snapshot_at("tank/golden/rails", "base", day(10));
        h.zfs.add_snapshot_at("tank/golden/rails", "new", day(0));

        backup(&h.worker, monday()).await.unwrap();

        // "recent" is no longer on the source, so "base" is what the send
        // is incremental from: kept despite its age.
        let kept = h.zfs.snapshot_names("backup/golden/rails");
        assert!(!kept.contains(&"old".to_string()), "{kept:?}");
        assert!(kept.contains(&"base".to_string()), "{kept:?}");
        assert!(kept.contains(&"recent".to_string()), "{kept:?}");
        let sent = h
            .zfs
            .replications()
            .into_iter()
            .find(|r| r.1 == "tank/golden/rails@new")
            .unwrap();
        assert_eq!(sent.0.as_deref(), Some("tank/golden/rails@base"));
    }

    #[tokio::test]
    async fn an_already_replicated_golden_is_skipped() {
        let h = Harness::new().await;
        h.zfs.add_snapshot("tank/golden/rails", "a");
        backup(&h.worker, monday()).await.unwrap();
        let report = backup(&h.worker, monday()).await.unwrap();
        assert!(!report.replicated.contains(&"tank/golden/rails".to_string()));
    }

    #[tokio::test]
    async fn sundays_store_a_full_image_in_the_bucket_even_when_nothing_new_replicated() {
        let h = Harness::new().await;
        h.zfs.add_snapshot("tank/golden/rails", "a");
        let report = backup(&h.worker, sunday()).await.unwrap();
        assert!(report
            .stored
            .contains(&"s3://bucket/golden/rails@a.zst".to_string()));
        assert_eq!(h.cloud.stored().len(), 3);

        let report = backup(&h.worker, monday()).await.unwrap();
        assert!(report.stored.is_empty());
        let report = backup(&h.worker, sunday() + chrono::Duration::weeks(1))
            .await
            .unwrap();
        assert!(!report.replicated.contains(&"tank/golden/rails".to_string()));
        assert!(report
            .stored
            .contains(&"s3://bucket/golden/rails@a.zst".to_string()));
    }
}
