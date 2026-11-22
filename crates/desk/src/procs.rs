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

impl AsRef<Proc> for Proc {
    fn as_ref(&self) -> &Proc {
        self
    }
}

/// One process with what `ps` says of its run: its state, whether it is in
/// its terminal's foreground, how long it has run and used the CPU, and its
/// resident memory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    pub proc: Proc,
    pub state: char,
    pub foreground: bool,
    pub elapsed_secs: u64,
    pub cpu_secs: u64,
    pub rss_kb: u64,
}

impl AsRef<Proc> for Stat {
    fn as_ref(&self) -> &Proc {
        &self.proc
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

/// Every process with its run, from
/// `ps -A -ww -o pid=,ppid=,stat=,etime=,time=,rss=,args=`.
pub fn parse_stats(listing: &str) -> Vec<Stat> {
    listing
        .lines()
        .filter_map(|line| {
            let mut fields = line.split_whitespace();
            let pid = fields.next()?.parse().ok()?;
            let ppid = fields.next()?.parse().ok()?;
            let stat = fields.next()?;
            let elapsed_secs = parse_etime(fields.next()?)?;
            let cpu_secs = parse_etime(fields.next()?)?;
            let rss_kb = fields.next()?.parse().ok()?;
            Some(Stat {
                proc: Proc {
                    pid,
                    ppid,
                    args: fields.collect::<Vec<_>>().join(" "),
                },
                state: stat.chars().next()?,
                foreground: stat.contains('+'),
                elapsed_secs,
                cpu_secs,
                rss_kb,
            })
        })
        .collect()
}

/// Seconds from `ps`'s `etime` or `time` (`[[dd-]hh:]mm:ss`, macOS's
/// `time` with hundredths after its seconds, which are dropped).
pub fn parse_etime(etime: &str) -> Option<u64> {
    let etime = etime.trim();
    let whole = etime.split_once('.').map_or(etime, |(w, _)| w);
    let (days, rest) = match whole.split_once('-') {
        Some((d, r)) => (d.parse::<u64>().ok()?, r),
        None => (0, whole),
    };
    let parts: Vec<u64> = rest
        .split(':')
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    let secs = match parts[..] {
        [m, s] => m * 60 + s,
        [h, m, s] => h * 3600 + m * 60 + s,
        _ => return None,
    };
    Some(days * 86_400 + secs)
}

/// The processes under `root`, however deep, each once.
pub fn under<P: AsRef<Proc>>(all: &[P], root: u32) -> Vec<&P> {
    let mut children: HashMap<u32, Vec<&P>> = HashMap::new();
    all.iter()
        .filter(|p| p.as_ref().pid != p.as_ref().ppid)
        .for_each(|p| children.entry(p.as_ref().ppid).or_default().push(p));
    let mut seen = HashSet::from([root]);
    let mut found = Vec::new();
    let mut next = vec![root];
    while let Some(pid) = next.pop() {
        for child in children.get(&pid).into_iter().flatten() {
            let pid = child.as_ref().pid;
            if seen.insert(pid) {
                found.push(*child);
                next.push(pid);
            }
        }
    }
    found
}

/// The processes `parent` started itself.
pub fn children<P: AsRef<Proc>>(all: &[P], parent: u32) -> impl Iterator<Item = &P> {
    all.iter().filter(move |p| {
        let p = p.as_ref();
        p.ppid == parent && p.pid != parent
    })
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

/// Reads the listing with each process's run now; `None` when `ps` can't
/// be run.
pub async fn stats() -> Option<Vec<Stat>> {
    let out = tokio::process::Command::new("ps")
        .args([
            "-A",
            "-ww",
            "-o",
            "pid=,ppid=,stat=,etime=,time=,rss=,args=",
        ])
        .kill_on_drop(true)
        .output()
        .await
        .ok()?;
    out.status
        .success()
        .then(|| parse_stats(&String::from_utf8_lossy(&out.stdout)))
}

/// The processes in process group `group`, as `ps` lists them now.
pub async fn in_group(group: u32) -> Vec<u32> {
    let out = tokio::process::Command::new("ps")
        .args(["-A", "-o", "pid=,pgid="])
        .kill_on_drop(true)
        .output()
        .await;
    out.ok()
        .filter(|o| o.status.success())
        .map(|o| {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .filter_map(|l| {
                    let mut f = l.split_whitespace().map(|w| w.parse::<u32>().ok());
                    match (f.next().flatten(), f.next().flatten()) {
                        (Some(pid), Some(g)) if g == group => Some(pid),
                        _ => None,
                    }
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether process `pid`'s environment holds `entry` (`NAME=value`)
/// exactly, as the system keeps it apart from its arguments.
pub async fn carries(pid: u32, entry: &str) -> bool {
    let entry = entry.as_bytes().to_vec();
    tokio::task::spawn_blocking(move || environ(pid).is_some_and(|env| env.contains(&entry)))
        .await
        .unwrap_or(false)
}

/// Process `pid`'s environment, an entry each, from `/proc`.
#[cfg(target_os = "linux")]
fn environ(pid: u32) -> Option<Vec<Vec<u8>>> {
    let env = std::fs::read(format!("/proc/{pid}/environ")).ok()?;
    Some(
        env.split(|b| *b == 0)
            .filter(|e| !e.is_empty())
            .map(<[u8]>::to_vec)
            .collect(),
    )
}

/// Process `pid`'s environment, an entry each, from the kernel's
/// `KERN_PROCARGS2` (the person's own processes alone).
#[cfg(target_os = "macos")]
fn environ(pid: u32) -> Option<Vec<Vec<u8>>> {
    let pid = i32::try_from(pid).ok()?;
    let mut most: libc::c_int = 0;
    let mut size = std::mem::size_of::<libc::c_int>();
    let mut mib = [libc::CTL_KERN, libc::KERN_ARGMAX];
    // SAFETY: sysctl writes at most `size` bytes into `most`, an int.
    let read = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            2,
            std::ptr::from_mut(&mut most).cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return None;
    }
    let mut buf = vec![0u8; usize::try_from(most).ok()?];
    let mut size = buf.len();
    let mut mib = [libc::CTL_KERN, libc::KERN_PROCARGS2, pid];
    // SAFETY: sysctl writes at most `size` bytes into `buf`, that long.
    let read = unsafe {
        libc::sysctl(
            mib.as_mut_ptr(),
            3,
            buf.as_mut_ptr().cast(),
            &mut size,
            std::ptr::null_mut(),
            0,
        )
    };
    if read != 0 {
        return None;
    }
    buf.truncate(size);
    procargs_env(&buf)
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn environ(_pid: u32) -> Option<Vec<Vec<u8>>> {
    None
}

/// The environment in a `KERN_PROCARGS2` reading: past its argument count,
/// its program's path (and the NULs after it) and that many arguments,
/// each entry up to the first empty one.
#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn procargs_env(buf: &[u8]) -> Option<Vec<Vec<u8>>> {
    let argc = usize::try_from(i32::from_ne_bytes(buf.get(..4)?.try_into().ok()?)).ok()?;
    let rest = buf.get(4..)?;
    let path = rest.iter().position(|b| *b == 0)?;
    let rest = &rest[path..];
    let start = rest.iter().position(|b| *b != 0)?;
    let mut words = rest[start..].split(|b| *b == 0);
    for _ in 0..argc {
        words.next()?;
    }
    Some(
        words
            .take_while(|e| !e.is_empty())
            .map(<[u8]>::to_vec)
            .collect(),
    )
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
        let ran =
            parse_etime(&String::from_utf8_lossy(&out.stdout)).filter(|_| out.status.success())?;
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
    fn a_processs_environment_is_read_past_its_arguments() {
        let mut buf = 2i32.to_ne_bytes().to_vec();
        buf.extend_from_slice(
            b"/bin/sh\0\0\0sh\0DESK_DEPS_RUN=args\0HOME=/h\0DESK_DEPS_RUN=env\0\0junk\0",
        );
        assert_eq!(
            procargs_env(&buf),
            Some(vec![b"HOME=/h".to_vec(), b"DESK_DEPS_RUN=env".to_vec()])
        );
        assert_eq!(procargs_env(&buf[..3]), None);
        let mut short = 5i32.to_ne_bytes().to_vec();
        short.extend_from_slice(b"/bin/sh\0sh\0");
        assert_eq!(procargs_env(&short), None);
    }

    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn a_token_in_a_processs_arguments_alone_isnt_in_its_environment() {
        let entry = "DESK_DEPS_RUN=run-1";
        let mut argued = tokio::process::Command::new("sh")
            .args(["-c", "sleep 60; :", entry])
            .spawn()
            .unwrap();
        let mut carried = tokio::process::Command::new("sh")
            .args(["-c", "sleep 60; :"])
            .env("DESK_DEPS_RUN", "run-1")
            .spawn()
            .unwrap();
        assert!(!carries(argued.id().unwrap(), entry).await);
        assert!(carries(carried.id().unwrap(), entry).await);
        argued.kill().await.unwrap();
        carried.kill().await.unwrap();
    }

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

    #[test]
    fn elapsed_and_cpu_times_are_read_in_every_form_ps_prints() {
        assert_eq!(parse_etime("00:07"), Some(7));
        assert_eq!(parse_etime("12:07"), Some(727));
        assert_eq!(parse_etime("01:02:03"), Some(3723));
        assert_eq!(parse_etime("2-01:02:03"), Some(2 * 86_400 + 3723));
        assert_eq!(parse_etime("0:03.41"), Some(3));
        assert_eq!(parse_etime("125:30.12"), Some(125 * 60 + 30));
        assert_eq!(parse_etime("1-02:03:04.50"), Some(86_400 + 7384));
        assert_eq!(parse_etime("soon"), None);
        assert_eq!(parse_etime("1:2:3:4"), None);
        assert_eq!(parse_etime(""), None);
    }

    #[test]
    fn a_wide_listing_keeps_spaces_in_args_and_skips_a_bad_line() {
        let all = parse_stats(
            "  1     0 Ss   10-00:00:00 1:02.50  9000 /sbin/launchd\n\
             100     1 S+      02:50:00   0:03   212000 sh -c sleep 600; echo  done\n\
             101   100 R       01:00   125:30.12  64 cargo test\n\
             102   100 S       junk 0:01 10 broken\n",
        );
        assert_eq!(all.len(), 3);
        assert_eq!(all[1].proc.args, "sh -c sleep 600; echo done");
        assert_eq!(all[1].state, 'S');
        assert!(all[1].foreground);
        assert!(!all[2].foreground);
        assert_eq!(all[1].elapsed_secs, 10_200);
        assert_eq!(all[1].cpu_secs, 3);
        assert_eq!(all[2].cpu_secs, 7530);
        assert_eq!(all[1].rss_kb, 212_000);
        assert_eq!(all[0].elapsed_secs, 864_000);
        assert_eq!(
            under(&all, 1)
                .iter()
                .map(|s| s.proc.pid)
                .collect::<Vec<_>>()
                .len(),
            2
        );
    }

    #[tokio::test]
    async fn this_machines_listing_with_runs_is_read_whole() {
        let all = stats().await.expect("ps runs");
        let me = std::process::id();
        assert!(all.iter().any(|s| s.proc.pid == me));
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
