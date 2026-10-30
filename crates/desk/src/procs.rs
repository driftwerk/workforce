//! This machine's processes, as `ps` lists them: what a session's own
//! process has running under it.

use std::collections::HashMap;

/// One process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proc {
    pub pid: u32,
    pub ppid: u32,
    pub args: String,
}

/// Every process, from `ps -axo pid=,ppid=,args=`.
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

/// The processes under `root`, however deep.
pub fn under(all: &[Proc], root: u32) -> Vec<&Proc> {
    let mut children: HashMap<u32, Vec<&Proc>> = HashMap::new();
    all.iter()
        .filter(|p| p.pid != p.ppid)
        .for_each(|p| children.entry(p.ppid).or_default().push(p));
    let mut found = Vec::new();
    let mut next = vec![root];
    while let Some(pid) = next.pop() {
        for child in children.get(&pid).into_iter().flatten() {
            found.push(*child);
            next.push(child.pid);
        }
    }
    found
}

/// Whether a process is one of Claude Code's Bash tool shells, which source
/// its shell snapshot.
pub fn is_tool_shell(p: &Proc) -> bool {
    p.args.contains("/shell-snapshots/snapshot-")
}

/// Reads the listing now; `None` when `ps` can't be run.
pub async fn table() -> Option<Vec<Proc>> {
    let out = tokio::process::Command::new("ps")
        .args(["-axo", "pid=,ppid=,args="])
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| parse(&String::from_utf8_lossy(&out.stdout)))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            under(&all, 100)
                .into_iter()
                .filter(|p| is_tool_shell(p))
                .count(),
            1
        );
    }
}
