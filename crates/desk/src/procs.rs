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

/// How long a process has run, from `ps`'s `etime` (`[[dd-]hh:]mm:ss`), in
/// seconds.
pub fn elapsed(etime: &str) -> Option<i64> {
    let etime = etime.trim();
    let (days, clock) = match etime.split_once('-') {
        Some((d, rest)) => (d.parse::<i64>().ok()?, rest),
        None => (0, etime),
    };
    let parts: Vec<i64> = clock
        .split(':')
        .map(|p| p.parse::<i64>().ok())
        .collect::<Option<_>>()?;
    let (h, m, s) = match parts[..] {
        [m, s] => (0, m, s),
        [h, m, s] => (h, m, s),
        _ => return None,
    };
    Some(((days * 24 + h) * 60 + m) * 60 + s)
}

/// When process `pid` started, as `ps` says, to the second: what a process
/// kept running across a restart of the hub began at, not when it was
/// taken back.
pub async fn started_at(pid: u32) -> Option<chrono::DateTime<chrono::Utc>> {
    let out = tokio::process::Command::new("ps")
        .args(["-o", "etime=", "-p", &pid.to_string()])
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    let ran = elapsed(&String::from_utf8_lossy(&out.stdout)).filter(|_| out.status.success())?;
    Some(chrono::Utc::now() - chrono::Duration::seconds(ran))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn how_long_a_process_has_run_is_read_from_ps() {
        assert_eq!(elapsed("   00:07"), Some(7));
        assert_eq!(elapsed("12:34"), Some(12 * 60 + 34));
        assert_eq!(elapsed("01:02:03"), Some(3723));
        assert_eq!(elapsed("2-07:24:00"), Some(2 * 86_400 + 7 * 3600 + 24 * 60));
        assert_eq!(elapsed(""), None);
        assert_eq!(elapsed("ELAPSED"), None);
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
