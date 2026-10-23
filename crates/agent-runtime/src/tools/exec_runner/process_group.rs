//! A child that leads its own process group, so ending it ends everything it
//! spawned. Killing only the leader leaves its descendants alive and holding
//! its pipes — a background `pnpm install` under a finished `claude` is exactly
//! how a round that had already replied ran to its full wall clock.

use std::future::Future;
use std::io;
use std::ops::{Deref, DerefMut};
use std::time::Duration;

use tokio::process::{Child, Command};
use tokio::task::JoinError;
use tracing::warn;

pub struct GroupChild {
    child: Child,
    pgid: i32,
}

impl GroupChild {
    /// Spawn `cmd` as the leader of a new process group. Dropping the handle
    /// before the leader is reaped kills the group.
    pub fn spawn(cmd: &mut Command) -> io::Result<Self> {
        cmd.process_group(0);
        let child = cmd.spawn()?;
        let pgid = child
            .id()
            .map(|id| id as i32)
            .ok_or_else(|| io::Error::other("spawned child has no pid"))?;
        Ok(Self { child, pgid })
    }

    /// The group's id, which is the leader's pid.
    pub fn pgid(&self) -> i32 {
        self.pgid
    }

    /// `SIGKILL` the whole group, leader included. Sent only while the group
    /// still exists — an unreaped leader, or a member still holding a pipe —
    /// and skipped once the kernel reports it gone, so a recycled group id is
    /// never signalled.
    pub fn kill_group(&mut self) {
        // SAFETY: kill(2)/killpg(2) have no memory-safety preconditions; a
        // negative pid addresses the process group and signal 0 only probes.
        unsafe {
            if libc::killpg(self.pgid, 0) == 0 {
                libc::kill(-self.pgid, libc::SIGKILL);
            }
        }
        let _ = self.child.start_kill();
    }

    /// Collect both output readers once the leader has exited. A reader still
    /// open past `deadline` means a descendant inherited the pipe, so the
    /// group is killed and the readers given one more deadline; `None` means
    /// they must be abandoned.
    pub async fn drain_readers<R, T>(
        &mut self,
        deadline: Duration,
        stdout: &mut R,
        stderr: &mut R,
    ) -> Option<(T, T)>
    where
        R: Future<Output = Result<T, JoinError>> + Unpin,
        T: Default,
    {
        if let Ok(done) = tokio::time::timeout(deadline, join_readers(stdout, stderr)).await {
            return Some(done);
        }
        warn!("command exited but a descendant still holds its output pipe; killing its process group");
        self.kill_group();
        tokio::time::timeout(deadline, join_readers(stdout, stderr))
            .await
            .ok()
    }
}

async fn join_readers<R, T>(stdout: &mut R, stderr: &mut R) -> (T, T)
where
    R: Future<Output = Result<T, JoinError>> + Unpin,
    T: Default,
{
    let (stdout, stderr) = tokio::join!(stdout, stderr);
    (stdout.unwrap_or_default(), stderr.unwrap_or_default())
}

impl Drop for GroupChild {
    fn drop(&mut self) {
        if self.child.id().is_some() {
            self.kill_group();
        }
    }
}

impl Deref for GroupChild {
    type Target = Child;

    fn deref(&self) -> &Child {
        &self.child
    }
}

impl DerefMut for GroupChild {
    fn deref_mut(&mut self) -> &mut Child {
        &mut self.child
    }
}

#[cfg(test)]
mod tests {
    use std::process::Stdio;

    use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
    use tokio::process::ChildStdout;

    use super::*;

    /// A leader whose grandchild sleeps holding the leader's stdout. The
    /// handle reads to EOF only once every holder of the pipe is gone, which
    /// is a portable proof of death — a pid probe would still see a zombie.
    async fn leader_with_grandchild() -> (GroupChild, BufReader<ChildStdout>) {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 30 & echo started; wait")
            .stdout(Stdio::piped());
        let mut child = GroupChild::spawn(&mut cmd).unwrap();
        let mut stdout = BufReader::new(child.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).await.unwrap();
        assert_eq!(line.trim(), "started");
        (child, stdout)
    }

    async fn reaches_eof(mut stdout: BufReader<ChildStdout>) -> bool {
        let mut rest = Vec::new();
        tokio::time::timeout(Duration::from_secs(5), stdout.read_to_end(&mut rest))
            .await
            .is_ok()
    }

    #[tokio::test]
    async fn kill_group_takes_the_grandchild_with_the_leader() {
        let (mut child, stdout) = leader_with_grandchild().await;
        child.kill_group();
        let _ = child.wait().await;
        assert!(
            reaches_eof(stdout).await,
            "grandchild outlived the group kill"
        );
    }

    #[tokio::test]
    async fn dropping_an_unreaped_leader_kills_its_group() {
        let (child, stdout) = leader_with_grandchild().await;
        drop(child);
        assert!(reaches_eof(stdout).await, "grandchild outlived the drop");
    }

    #[tokio::test]
    async fn drain_readers_kills_the_group_when_a_descendant_holds_the_pipe() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("sleep 30 & echo hi")
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut child = GroupChild::spawn(&mut cmd).unwrap();
        let mut out = child.stdout.take().unwrap();
        let mut err = child.stderr.take().unwrap();
        let mut stdout = tokio::spawn(async move {
            let mut s = String::new();
            let _ = out.read_to_string(&mut s).await;
            s
        });
        let mut stderr = tokio::spawn(async move {
            let mut s = String::new();
            let _ = err.read_to_string(&mut s).await;
            s
        });
        assert!(child.wait().await.unwrap().success());

        let started = std::time::Instant::now();
        let drained = child
            .drain_readers(Duration::from_secs(2), &mut stdout, &mut stderr)
            .await;
        assert_eq!(drained, Some(("hi\n".to_string(), String::new())));
        assert!(started.elapsed() < Duration::from_secs(10));
    }

    #[tokio::test]
    async fn dropping_a_reaped_leader_is_harmless() {
        let mut cmd = Command::new("true");
        let mut child = GroupChild::spawn(&mut cmd).unwrap();
        assert!(child.wait().await.unwrap().success());
        assert!(child.id().is_none());
        drop(child);
    }
}
