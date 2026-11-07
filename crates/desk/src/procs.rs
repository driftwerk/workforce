//! This machine's processes, as `ps` lists them: what a session's own
//! process has running under it.

use std::collections::{HashMap, HashSet};

/// One process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub args: String,
}

impl Proc {
    /// The program it runs: its command's file name, without a login
    /// shell's leading `-`.
    pub(crate) fn program(&self) -> Option<String> {
        let command = self.args.split_whitespace().next()?;
        let name = command.rsplit('/').next().unwrap_or(command);
        let name = name.strip_prefix('-').unwrap_or(name);
        (!name.is_empty()).then(|| name.to_string())
    }
}

/// Every process, from `ps -A -ww -o pid=,ppid=,args=`.
pub fn parse(listing: &str) -> Vec<Proc> {
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            Some(Proc {
                pid,
                ppid,
                args: fields.collect::<Vec<_>>().join(" "),
            })
        })
        .collect()
}

/// The processes under `root`, however deep, each once.
pub fn under(all: &[Proc], root: u32) -> Vec<&Proc> {
    let mut children: HashMap<u32, Vec<&Proc>> = HashMap::new();
    all.iter()
        .filter(|p| p.pid != p.ppid)
        .for_each(|p| children.entry(p.ppid).or_default().push(p));
    let mut seen = HashSet::from([root]);
    let mut found = Vec::new();
    let mut next = vec![root];
    while let Some(pid) = next.pop() {
        for child in children.get(&pid).into_iter().flatten() {
            if seen.insert(child.pid) {
                found.push(*child);
                next.push(child.pid);
            }
        }
    }
    found
}

/// The processes `parent` started itself.
pub fn children(all: &[Proc], parent: u32) -> impl Iterator<Item = &Proc> {
    all.iter()
        .filter(move |p| p.ppid == parent && p.pid != parent)
}

/// What runs in a terminal whose shell is `root`, by program: the shell,
/// then each generation under it, outermost first and in pid order within
/// one. Empty when `root` isn't listed.
pub fn programs(all: &[Proc], root: u32) -> Vec<String> {
    let Some(shell) = all.iter().find(|p| p.pid == root) else {
        return Vec::new();
    };
    let mut depth = HashMap::from([(root, 0usize)]);
    let mut found: Vec<(usize, &Proc)> = under(all, root)
        .into_iter()
        .map(|p| {
            let d = depth[&p.ppid] + 1;
            depth.insert(p.pid, d);
            (d, p)
        })
        .collect();
    found.sort_by_key(|(d, p)| (*d, p.pid));
    std::iter::once(shell)
        .chain(found.into_iter().map(|(_, p)| p))
        .filter_map(Proc::program)
        .collect()
}

/// Whether a process is one of Claude Code's Bash tool shells, which source
/// its shell snapshot.
pub fn is_tool_shell(p: &Proc) -> bool {
    p.args.contains("/shell-snapshots/snapshot-")
}

/// Reads the listing now; `None` when `ps` can't be run.
pub async fn table() -> Option<Vec<Proc>> {
    let out = tokio::process::Command::new("ps")
        .args(["-A", "-ww", "-o", "pid=,ppid=,args="])
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| parse(&String::from_utf8_lossy(&out.stdout)))
}

/// How long `ps` is waited on for one process's start.
#[cfg(not(target_os = "linux"))]
const PS_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// When process `pid` started, to the second: what a process kept running
/// across a restart of the hub began at, not when it was taken back. On
/// Linux from `/proc`, elsewhere as `ps` says. `None` when neither can say.
pub async fn started_at(pid: u32) -> Option<chrono::DateTime<chrono::Utc>> {
    #[cfg(target_os = "linux")]
    {
        tokio::task::spawn_blocking(move || proc_started_at(pid))
            .await
            .ok()
            .flatten()
    }
    #[cfg(not(target_os = "linux"))]
    {
        let run = tokio::process::Command::new("ps")
            .args(["-o", "etime=", "-p", &pid.to_string()])
            .kill_on_drop(true)
            .output();
        let out = tokio::time::timeout(PS_WAIT, run).await.ok()?.ok()?;
        let ran = crate::codex_session::parse_etime(&String::from_utf8_lossy(&out.stdout))
            .filter(|_| out.status.success())?;
        Some(chrono::Utc::now() - chrono::Duration::seconds(i64::try_from(ran).ok()?))
    }
}

/// When `pid` started, from `/proc/<pid>/stat`'s start time (in clock ticks
/// since boot) and the boot time `/proc/stat` says.
#[cfg(target_os = "linux")]
fn proc_started_at(pid: u32) -> Option<chrono::DateTime<chrono::Utc>> {
    let stat = std::fs::read_to_string(format!("/proc/{pid}/stat")).ok()?;
    let boot = std::fs::read_to_string("/proc/stat").ok()?;
    // SAFETY: sysconf reads a constant of the system; it has no other effect.
    let ticks = unsafe { libc::sysconf(libc::_SC_CLK_TCK) };
    start_of(&stat, &boot, u64::try_from(ticks).ok()?)
}

/// A process's start from its `stat` line, the system's `stat` and its clock
/// ticks a second. Its name may hold spaces and parentheses: the fields are
/// counted after its last `)`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn start_of(stat: &str, boot: &str, ticks: u64) -> Option<chrono::DateTime<chrono::Utc>> {
    let fields: Vec<&str> = stat
        .get(stat.rfind(')')? + 1..)?
        .split_whitespace()
        .collect();
    // Field 22 of the line, the 20th after the name.
    let since_boot: u64 = fields.get(19)?.parse().ok()?;
    let btime: i64 = boot
        .lines()
        .find_map(|l| l.strip_prefix("btime "))?
        .trim()
        .parse()
        .ok()?;
    let secs = i64::try_from(since_boot / ticks.max(1)).ok()?;
    chrono::DateTime::from_timestamp(btime + secs, 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_processs_start_is_read_from_its_stat_whatever_its_name() {
        let stat = "4242 (claude (2.1)) S 1 4242 4242 0 -1 4194560 100 0 0 0 5 2 0 0 20 0 9 0 360000 1000 200";
        let boot = "cpu  1 2 3\nbtime 1794000000\nprocesses 9\n";
        assert_eq!(
            start_of(stat, boot, 100),
            chrono::DateTime::from_timestamp(1_794_000_000 + 3600, 0)
        );
        assert_eq!(start_of("4242 (x) S 1", boot, 100), None);
        assert_eq!(start_of(stat, "cpu 1\n", 100), None);
    }

    #[tokio::test]
    async fn this_process_started_before_now() {
        let started = started_at(std::process::id()).await.unwrap();
        assert!(started <= chrono::Utc::now());
        assert!(started_at(u32::MAX - 1).await.is_none());
    }

    #[test]
    fn what_runs_under_a_process_is_found_however_deep() {
        let all = parse(
            "  1     0 /sbin/launchd\n\
             100     1 claude --session-id s1\n\
             101   100 /bin/zsh -c source /h/.claude/shell-snapshots/snapshot-zsh-1.sh && eval 'npm run dev'\n\
             102   101 node server.js\n\
             200     1 claude --session-id s2\n\
             not a line\n",
        );
        let pids = |root| under(&all, root).iter().map(|p| p.pid).collect::<Vec<_>>();
        assert_eq!(pids(100).len(), 2);
        assert!(pids(100).contains(&102));
        assert!(pids(200).is_empty());
        assert_eq!(
            children(&all, 100).map(|p| p.pid).collect::<Vec<_>>(),
            [101],
            "a shell's own children are not more of its work"
        );
        assert_eq!(
            under(&all, 100)
                .into_iter()
                .filter(|p| is_tool_shell(p))
                .count(),
            1
        );
    }

    #[test]
    fn a_terminal_lists_its_shell_then_each_generation_in_pid_order() {
        let all = parse(
            "    1     0 /sbin/launchd\n\
                400     1 /usr/local/bin/desk-helper\n\
                501   400 -zsh\n\
                640   620 /bin/sh -c npm test\n\
                620   501 /home/sarah/.local/bin/claude --session-id s1\n\
                610   501 node server.js\n\
                641   640 npm test\n\
                521   400 /bin/bash -l\n\
                junk line\n",
        );
        assert_eq!(programs(&all, 501), ["zsh", "node", "claude", "sh", "npm"]);
        assert_eq!(programs(&all, 521), ["bash"]);
        assert!(programs(&all, 9999).is_empty());
    }

    #[test]
    fn a_listing_read_mid_reuse_still_ends() {
        let all = parse("100 1 claude\n101 100 sh\n100 101 sh\n");
        assert_eq!(under(&all, 100).len(), 1);
        assert_eq!(programs(&all, 100), ["claude", "sh"]);
    }

    #[tokio::test]
    async fn this_machines_listing_is_read_whole() {
        let table = table().await.expect("ps runs");
        let me = std::process::id();
        let mine = table
            .iter()
            .find(|p| p.pid == me)
            .expect("this process is listed");
        assert!(!mine.args.is_empty());
    }
}
