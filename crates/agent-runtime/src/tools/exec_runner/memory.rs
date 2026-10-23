//! The resident memory of a process group, for commands that run with a
//! memory limit. Read from `/proc`, so enforced only where it exists; a
//! limit elsewhere is never reached.

use std::time::Duration;

/// How often a limited group's memory is measured.
const POLL: Duration = Duration::from_secs(2);

/// Resolves with the group's resident bytes once they exceed `limit`; never
/// without a limit, or where the group cannot be measured.
pub(super) async fn exceeded(pgid: i32, limit: Option<u64>) -> u64 {
    let Some(limit) = limit else {
        return std::future::pending().await;
    };
    let mut tick = tokio::time::interval(POLL);
    loop {
        tick.tick().await;
        match group_rss(pgid) {
            Some(used) if used > limit => return used,
            Some(_) => {}
            None => return std::future::pending().await,
        }
    }
}

#[cfg(target_os = "linux")]
fn group_rss(pgid: i32) -> Option<u64> {
    // SAFETY: sysconf has no memory-safety preconditions.
    let page = u64::try_from(unsafe { libc::sysconf(libc::_SC_PAGESIZE) }).ok()?;
    let pages: u64 = std::fs::read_dir("/proc")
        .ok()?
        .flatten()
        .filter(|entry| {
            entry
                .file_name()
                .to_str()
                .is_some_and(|name| name.bytes().all(|b| b.is_ascii_digit()))
        })
        .filter_map(|entry| std::fs::read_to_string(entry.path().join("stat")).ok())
        .filter_map(|stat| group_and_rss_pages(&stat))
        .filter(|(group, _)| *group == pgid)
        .map(|(_, pages)| pages)
        .sum();
    Some(pages * page)
}

#[cfg(not(target_os = "linux"))]
fn group_rss(_pgid: i32) -> Option<u64> {
    None
}

/// The process group and resident pages a `/proc/<pid>/stat` line reports.
/// The command name is parenthesised and may hold spaces and parentheses, so
/// the fields are counted from the last `)`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn group_and_rss_pages(stat: &str) -> Option<(i32, u64)> {
    let fields: Vec<&str> = stat
        .get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .collect();
    Some((fields.get(2)?.parse().ok()?, fields.get(21)?.parse().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_stat_line_yields_its_group_and_resident_pages_whatever_the_command_name() {
        let stat =
            "4242 (rust (analyzer) x) S 1 4200 4200 0 -1 4194560 100 0 0 0 5 1 0 0 20 0 9 0 \
                    123 987654 3072 18446744073709551615 1 1 0 0 0 0 0 0 0 0 0 0 17 3 0 0 0 0 0";
        assert_eq!(group_and_rss_pages(stat), Some((4200, 3072)));
        assert_eq!(group_and_rss_pages("garbage"), None);
    }

    #[tokio::test]
    async fn without_a_limit_the_group_is_never_over_it() {
        let over = tokio::time::timeout(Duration::from_millis(50), exceeded(1, None));
        assert!(over.await.is_err());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn a_group_over_its_limit_is_reported() {
        // SAFETY: getpgrp has no preconditions.
        let own = unsafe { libc::getpgrp() };
        assert!(exceeded(own, Some(1)).await > 1);
    }
}
