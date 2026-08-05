//! Wraps `Terminal`. Delegates the PTY calls and rewrites only `exec`.
//!
//! **Why exec is not delegated:** upstream `PtyTerminal::exec` spawns the child inside itself,
//! reads to EOF with `cmd.output()` and hands back a finished string — no pid, no `Child`, no
//! stream ever reaches the caller, and the timeout branch returns without killing anything. A
//! decorator cannot kill the process group at all, and an output cap could only trim a string
//! that is already fully in memory.

use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncReadExt;
use zyris::{Blob, Streaming};
use zyris_capkit::PtyTerminal;
use zyris_caps::{ExecOutput, PtyChunk, PtyId, PtyOpened, PtyRead, PtyScreen, Settle, Terminal};

use crate::config::TerminalConfig;
use crate::gate::PathGate;

/// Grace between SIGTERM and SIGKILL.
const KILL_GRACE: Duration = Duration::from_millis(200);

pub struct GatedTerminal {
    gate: PathGate,
    cfg: TerminalConfig,
    /// **Built once, never rebuilt.** The session cap is per instance and the reaper sweeper
    /// holds a `Weak`, so dropping this loses every open PTY session at once.
    inner: PtyTerminal,
}

impl GatedTerminal {
    pub fn new(gate: PathGate, cfg: TerminalConfig) -> GatedTerminal {
        let inner = PtyTerminal::rooted(gate.root().to_path_buf());
        GatedTerminal { gate, cfg, inner }
    }

    /// Effective timeout = `min(what the caller asked for, config)`. Config acts as the cap.
    fn effective_timeout(&self, caller_ms: Option<u64>) -> Duration {
        let cap_ms = self.cfg.exec_timeout_secs.saturating_mul(1000);
        Duration::from_millis(caller_ms.map(|c| c.min(cap_ms)).unwrap_or(cap_ms))
    }
}

/// **Keeps** only up to the cap, but keeps reading to EOF.
///
/// Never stop reading just because the cap was hit. The child fills the pipe buffer and blocks
/// in write, `child.wait()` never returns, and the call hangs until the effective timeout. Only
/// by draining and discarding the excess does the command exit cleanly with a real exit code.
async fn read_capped<R: tokio::io::AsyncRead + Unpin>(r: &mut R, cap: usize) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut buf = [0u8; 8192];
    let mut capped = false;
    loop {
        match r.read(&mut buf).await {
            Ok(0) | Err(_) => return (out, capped),
            Ok(n) => {
                let room = cap.saturating_sub(out.len());
                if room > 0 {
                    out.extend_from_slice(&buf[..n.min(room)]);
                }
                if n > room {
                    capped = true;
                }
            }
        }
    }
}

/// A negative pid means the whole process group. Spawned with `process_group(0)`, so the child's
/// pid is the group id and every grandchild under it dies with it.
#[cfg(unix)]
fn kill_group(pid: i32, sig: i32) {
    if pid > 0 {
        unsafe { libc::kill(-pid, sig) };
    }
}

fn finish(bytes: Vec<u8>, capped: bool) -> String {
    let mut s = String::from_utf8_lossy(&bytes).to_string();
    if capped {
        s.push_str("\n… output truncated at the cap");
    }
    s
}

#[zyris::async_trait]
impl Terminal for GatedTerminal {
    async fn open(&self, shell: Option<String>, cols: u16, rows: u16) -> zyris::Result<PtyOpened> {
        self.inner.open(shell, cols, rows).await
    }

    async fn open_stream(
        &self,
        shell: Option<String>,
        cols: u16,
        rows: u16,
    ) -> zyris::Result<Streaming<PtyOpened, PtyChunk>> {
        self.inner.open_stream(shell, cols, rows).await
    }

    async fn read(
        &self,
        pty: PtyId,
        input: Option<String>,
        settle: Option<Settle>,
    ) -> zyris::Result<PtyRead> {
        self.inner.read(pty, input, settle).await
    }

    async fn screen(
        &self,
        pty: PtyId,
        input: Option<String>,
        settle: Option<Settle>,
    ) -> zyris::Result<PtyScreen> {
        self.inner.screen(pty, input, settle).await
    }

    async fn write(&self, pty: PtyId, data: Blob) -> zyris::Result<()> {
        self.inner.write(pty, data).await
    }

    async fn resize(&self, pty: PtyId, cols: u16, rows: u16) -> zyris::Result<()> {
        self.inner.resize(pty, cols, rows).await
    }

    async fn close(&self, pty: PtyId) -> zyris::Result<()> {
        self.inner.close(pty).await
    }

    async fn exec(
        &self,
        command: String,
        cwd: Option<String>,
        timeout_ms: Option<u64>,
    ) -> zyris::Result<ExecOutput> {
        let dir = match cwd {
            Some(c) => self.gate.check(&c)?,
            None => self.gate.root().to_path_buf(),
        };

        let mut cmd = tokio::process::Command::new("/bin/sh");
        cmd.arg("-c")
            .arg(&command)
            .current_dir(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        // Credentials that reach other machines never go to the shell. A PTY shell inherits the
        // daemon's whole environment, and the systemd user manager's environment block carries
        // the SSH_AUTH_SOCK the session pushed in at graphical login.
        for key in &self.cfg.unset_env {
            cmd.env_remove(key);
        }
        #[cfg(unix)]
        cmd.process_group(0);

        let mut child = cmd.spawn().map_err(|e| {
            zyris::WireError::new(zyris::ErrorCode::Internal, format!("could not run: {e}"))
        })?;
        let pid = child.id().unwrap_or(0) as i32;
        let cap = self.cfg.max_output_bytes;
        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");

        let collect = async {
            // Read both at once. Read them in sequence and a child that fills the stderr pipe
            // buffer blocks before stdout hits EOF, and it deadlocks.
            let (out, err) =
                tokio::join!(read_capped(&mut stdout, cap), read_capped(&mut stderr, cap));
            let status = child.wait().await;
            (out, err, status)
        };

        match tokio::time::timeout(self.effective_timeout(timeout_ms), collect).await {
            Ok(((o, oc), (e, ec), status)) => Ok(ExecOutput {
                exit_code: status.ok().and_then(|s| s.code()).unwrap_or(-1),
                stdout: finish(o, oc),
                stderr: finish(e, ec),
                timed_out: false,
            }),
            Err(_) => {
                #[cfg(unix)]
                {
                    kill_group(pid, libc::SIGTERM);
                    tokio::time::sleep(KILL_GRACE).await;
                    kill_group(pid, libc::SIGKILL);
                }
                let _ = pid;
                Ok(ExecOutput {
                    exit_code: -1,
                    stdout: String::new(),
                    stderr: "command timed out; the whole process group was killed".into(),
                    timed_out: true,
                })
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn term(root: &Path) -> GatedTerminal {
        GatedTerminal::new(
            PathGate::new(vec![root.to_path_buf()], vec![]),
            TerminalConfig { max_output_bytes: 64, ..Default::default() },
        )
    }

    #[tokio::test]
    async fn exec_runs_and_captures() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let out = term(&root).exec("echo hi".into(), None, None).await.unwrap();
        assert_eq!(out.exit_code, 0);
        assert_eq!(out.stdout.trim(), "hi");
        assert!(!out.timed_out);
    }

    /// Over the cap it marks the truncation and returns **success**. Not an error.
    ///
    /// The point is checking `timed_out` and `exit_code` together. Stop reading at the cap and
    /// the child blocks on a full pipe, `wait()` never returns, and the command hangs until the
    /// effective timeout and ends timed_out. Only draining the excess makes this pass.
    #[tokio::test]
    async fn output_over_the_cap_is_truncated_but_the_command_still_completes() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let out = term(&root)
            .exec("head -c 100000 /dev/zero | tr '\\0' 'a'".into(), None, Some(5_000))
            .await
            .unwrap();
        assert!(!out.timed_out, "going over the cap deadlocked");
        assert_eq!(out.exit_code, 0, "the command must exit normally");
        assert!(out.stdout.len() < 200, "way over the cap: {}", out.stdout.len());
        assert!(out.stdout.contains("truncated"), "{}", out.stdout);
    }

    /// This is why exec was rewritten. Upstream does not kill on timeout; orphans keep running.
    #[tokio::test]
    async fn a_timeout_kills_the_whole_process_group() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let marker = root.join("still-alive");
        // A surviving grandchild creates the file after 2s. Kill the group and it never appears.
        let cmd = format!("(sleep 2; touch {}) & sleep 5", marker.display());
        let out = term(&root).exec(cmd, None, Some(300)).await.unwrap();
        assert!(out.timed_out);
        assert_eq!(out.exit_code, -1);
        tokio::time::sleep(Duration::from_secs(3)).await;
        assert!(!marker.exists(), "a grandchild process survived");
    }

    /// The effective timeout is min(caller, config). Config acts as the cap.
    #[tokio::test]
    async fn the_config_timeout_caps_the_caller() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let t = GatedTerminal::new(
            PathGate::new(vec![root.to_path_buf()], vec![]),
            TerminalConfig { exec_timeout_secs: 1, ..Default::default() },
        );
        let started = std::time::Instant::now();
        let out = t.exec("sleep 30".into(), None, Some(60_000)).await.unwrap();
        assert!(out.timed_out);
        assert!(started.elapsed() < Duration::from_secs(5), "{:?}", started.elapsed());
    }

    /// cwd goes through the gate too.
    #[tokio::test]
    async fn a_cwd_outside_the_root_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        assert!(term(&root).exec("pwd".into(), Some("/etc".into()), None).await.is_err());
    }

    /// Credentials that reach other machines never go to the shell.
    #[tokio::test]
    async fn ssh_auth_sock_is_removed_from_the_child_environment() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        // SAFETY: the test mutates its own process environment. No other test reads this var.
        unsafe { std::env::set_var("SSH_AUTH_SOCK", "/run/user/1000/keyring/ssh") };
        let out = term(&root)
            .exec("printf '[%s]' \"$SSH_AUTH_SOCK\"".into(), None, None)
            .await
            .unwrap();
        assert_eq!(out.stdout.trim(), "[]");
    }

    /// Without reading stdout and stderr at once, a child that fills the stderr pipe buffer
    /// blocks and it deadlocks.
    #[tokio::test]
    async fn a_command_writing_heavily_to_stderr_does_not_deadlock() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().canonicalize().unwrap();
        let t = GatedTerminal::new(
            PathGate::new(vec![root.to_path_buf()], vec![]),
            TerminalConfig { max_output_bytes: 1 << 20, exec_timeout_secs: 10, unset_env: vec![] },
        );
        let out = t
            .exec("head -c 400000 /dev/zero | tr '\\0' 'e' >&2; echo done".into(), None, None)
            .await
            .unwrap();
        assert!(!out.timed_out, "deadlocked into a timeout");
        assert_eq!(out.stdout.trim(), "done");
        assert_eq!(out.stderr.len(), 400_000);
    }
}
