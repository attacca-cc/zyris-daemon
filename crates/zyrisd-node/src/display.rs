//! Watches the desktop child live and die, attaching and detaching its capabilities.
//!
//! Framing is synchronous `std::io`, so a dedicated OS thread owns the child's stdin/stdout and
//! the async side speaks only over a channel. One request at a time — an agent looks at the
//! screen, then acts, which is serial by nature; multiplexing only complicates the child.

use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, oneshot};
use zyris::{Datum, ErrorCode, WireError};
use zyris_caps::{
    Display, ImageFormat, Input, InputServer, MouseButton, Region, ScreenCapture,
    ScreenCaptureServer,
};
use zyrisd_display_proto::{read_frame, write_frame, ImageMeta, Request, Response};

use crate::config::DesktopConfig;

/// Longest a single request may take. Plenty for a 4K capture plus encoding.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(20);
const BACKOFF_MIN: Duration = Duration::from_secs(1);
const BACKOFF_MAX: Duration = Duration::from_secs(60);

/// Where to look for the child, in order.
///
/// **`PATH` is not a candidate.** The `libexec` paths are on no `PATH` so it buys nothing,
/// and whoever can put that name early on `PATH` gets code re-run for the daemon's lifetime.
pub fn helper_path() -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("ZYRISD_DISPLAY_BIN").map(PathBuf::from) {
        if p.exists() {
            return p.canonicalize().ok().or(Some(p));
        }
        tracing::warn!(path = %p.display(), "ZYRISD_DISPLAY_BIN points at a missing file");
    }
    let exe = std::env::current_exe().ok()?;
    let dir = exe.parent()?;
    [
        dir.join("../libexec/zyrisd-display"),
        dir.join("zyrisd-display"), // convenience for cargo run
        PathBuf::from("/usr/libexec/zyrisd-display"),
    ]
    .into_iter()
    .find(|p| p.exists())
    .and_then(|p| p.canonicalize().ok())
}

struct Job {
    req: Request,
    reply: oneshot::Sender<Result<(Response, Vec<u8>), String>>,
}

/// One live child and the pipe used to talk to it.
pub struct Child {
    tx: mpsc::Sender<Job>,
    pid: i32,
}

impl Child {
    async fn call(&self, req: Request) -> zyris::Result<(Response, Vec<u8>)> {
        let (reply, rx) = oneshot::channel();
        self.tx
            .send(Job { req, reply })
            .await
            .map_err(|_| internal("Desktop helper has exited"))?;

        match tokio::time::timeout(REQUEST_TIMEOUT, rx).await {
            Ok(Ok(Ok(pair))) => Ok(pair),
            Ok(Ok(Err(e))) => Err(internal(e)),
            // The thread vanished without answering — EOF or a partial frame.
            Ok(Err(_)) => Err(internal("Desktop helper exited without answering")),
            Err(_) => {
                // A blocking read cannot be woken, so kill the child to free the thread.
                // The watcher then sees the death, drops the capabilities, and respawns.
                tracing::warn!("Desktop helper is not responding; killing it");
                kill(self.pid);
                Err(internal("Desktop helper is not responding"))
            }
        }
    }

    async fn expect_ok(&self, req: Request) -> zyris::Result<()> {
        match self.call(req).await?.0 {
            Response::Ok => Ok(()),
            Response::Error { message } => Err(internal(message)),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }
}

fn internal(msg: impl std::fmt::Display) -> WireError {
    WireError::new(ErrorCode::Internal, msg.to_string())
}

fn kill(pid: i32) {
    #[cfg(unix)]
    if pid > 0 {
        unsafe { libc::kill(pid, libc::SIGKILL) };
    }
}

/// Spawns the child and hands back the pipe and a "it died" signal.
fn spawn_child(path: &PathBuf) -> std::io::Result<(Child, oneshot::Receiver<()>)> {
    let mut cmd = Command::new(path);
    cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::inherit());
    // Re-read the session environment **every time** and inject it. A daemon started at boot
    // by linger has no DISPLAY and no WAYLAND_DISPLAY, and a later login never backfills them.
    for (k, v) in crate::notify::session_env() {
        cmd.env(k, v);
    }
    let mut child = cmd.spawn()?;

    let pid = child.id() as i32;
    let mut stdin = child.stdin.take().expect("piped");
    let mut stdout = child.stdout.take().expect("piped");
    let (tx, mut rx) = mpsc::channel::<Job>(1);
    let (dead_tx, dead_rx) = oneshot::channel();

    // A separate thread reaps the child.
    //
    // The framing thread below sits in `blocking_recv` until a job arrives, so on its own it
    // would not notice a dead child until the next call. Meanwhile the capabilities of a dead
    // helper stay announced — exactly the state this design exists to avoid.
    // It really did end up stuck that way in live.
    std::thread::spawn(move || {
        let _ = child.wait();
        let _ = dead_tx.send(());
    });

    std::thread::spawn(move || {
        let seq = AtomicU64::new(1);
        while let Some(job) = rx.blocking_recv() {
            let id = seq.fetch_add(1, Ordering::Relaxed);
            let outcome = (|| -> Result<(Response, Vec<u8>), String> {
                write_frame(&mut stdin, id, &job.req, &[]).map_err(|e| e.to_string())?;
                let frame = read_frame(&mut stdout).map_err(|e| e.to_string())?;
                let resp: Response =
                    serde_json::from_value(frame.body).map_err(|e| e.to_string())?;
                Ok((resp, frame.blob))
            })();
            let broken = outcome.is_err();
            let _ = job.reply.send(outcome);
            if broken {
                // Once framing breaks there is no more talking to this child.
                break;
            }
        }
        // Fail whatever is left in the queue at once, or callers hang until the timeout.
        rx.close();
        while let Ok(job) = rx.try_recv() {
            let _ = job.reply.send(Err("Desktop helper has exited".into()));
        }
        // If framing broke, end the child too. Reaping is the other thread's job.
        kill(pid);
    });

    Ok((Child { tx, pid }, dead_rx))
}

/// Parent-side half of `screen_capture`. Forwards calls to the child.
struct ScreenProxy(Arc<Child>);

#[zyris::async_trait]
impl ScreenCapture for ScreenProxy {
    async fn list_displays(&self) -> zyris::Result<Vec<Display>> {
        match self.0.call(Request::ListDisplays).await?.0 {
            Response::Displays { displays } => Ok(displays),
            Response::Error { message } => Err(internal(message)),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }

    async fn screenshot(
        &self,
        display: Option<String>,
        region: Option<Region>,
        format: Option<ImageFormat>,
        max_width: Option<u32>,
    ) -> zyris::Result<Datum> {
        let (resp, blob) =
            self.0.call(Request::Screenshot { display, region, format, max_width }).await?;
        match resp {
            // Meta and bytes are rejoined here. If the child sent a finished Datum as JSON,
            // Blob would serialize to base64 and the blob frame would lose its whole point.
            Response::Image { meta } => {
                let ImageMeta { name, description, media_type } = meta;
                Ok(Datum::Image { name, description, media_type, blob: zyris::Blob::from_bytes(blob) })
            }
            Response::Error { message } => Err(internal(message)),
            other => Err(internal(format!("unexpected response: {other:?}"))),
        }
    }
}

/// Parent-side half of `input`.
struct InputProxy(Arc<Child>);

#[zyris::async_trait]
impl Input for InputProxy {
    async fn type_text(&self, text: String) -> zyris::Result<()> {
        self.0.expect_ok(Request::TypeText { text }).await
    }

    async fn key(&self, chord: String) -> zyris::Result<()> {
        self.0.expect_ok(Request::Key { chord }).await
    }

    async fn move_to(&self, display: String, x: i32, y: i32) -> zyris::Result<()> {
        self.0.expect_ok(Request::MoveTo { display, x, y }).await
    }

    async fn click(&self, button: MouseButton) -> zyris::Result<()> {
        self.0.expect_ok(Request::Click { button }).await
    }

    async fn scroll(&self, dx: i32, dy: i32) -> zyris::Result<()> {
        self.0.expect_ok(Request::Scroll { dx, dy }).await
    }
}

/// Drops the capabilities on child death, retries with backoff. A display appearing later attaches.
///
/// **Only `add`/`remove`; never hand a new instance to `replace()`.** Those two clone the
/// existing `Arc`s and reassemble, so `PtyTerminal` is not dropped. `replace` with a fresh
/// instance kills every open PTY session.
pub async fn watch<F>(caps: zyris::Capabilities, cfg: DesktopConfig, on_change: F)
where
    F: Fn() + Send + 'static,
{
    if !cfg.enabled {
        tracing::info!("Desktop helper is disabled in config");
        return;
    }
    let Some(path) = helper_path() else {
        tracing::info!("No desktop helper found; screen and input will not be offered");
        return;
    };
    tracing::info!(path = %path.display(), "desktop helper");

    let mut backoff = BACKOFF_MIN;
    loop {
        match attach(&caps, &path).await {
            Ok(dead) => {
                backoff = BACKOFF_MIN;
                on_change();
                let _ = dead.await; // until the child dies
                tracing::info!("Desktop helper exited; taking the capabilities down");
                caps.remove("screen_capture").await;
                caps.remove("input").await;
                on_change();
            }
            Err(e) => tracing::debug!(error = %e, "could not attach the desktop helper"),
        }
        tokio::time::sleep(backoff).await;
        backoff = (backoff * 2).min(BACKOFF_MAX);
    }
}

async fn attach(
    caps: &zyris::Capabilities,
    path: &PathBuf,
) -> Result<oneshot::Receiver<()>, String> {
    let (child, dead) = spawn_child(path).map_err(|e| e.to_string())?;
    let child = Arc::new(child);

    let (displays, screen_ok, input_ok) = match child.call(Request::Probe).await {
        Ok((Response::Probe { displays, screen_ok, input_ok }, _)) => {
            (displays, screen_ok, input_ok)
        }
        Ok((other, _)) => return Err(format!("unexpected response to probe: {other:?}")),
        Err(e) => return Err(e.to_string()),
    };

    if displays.is_empty() {
        return Err("no displays".into());
    }

    // Announce only what actually works. Advertising what fails leaves the agent no way to
    // tell "absent" from "broken" — on this machine (GNOME) capture is exactly that case.
    if screen_ok {
        if let Err(e) = caps.add(ScreenCaptureServer(ScreenProxy(child.clone()))).await {
            tracing::warn!(error = %e, "could not attach screen_capture");
        } else {
            tracing::info!(displays = displays.len(), "announcing screen_capture");
        }
    } else {
        tracing::info!("Screen capture does not work on this display server");
    }

    if input_ok {
        if let Err(e) = caps.add(InputServer(InputProxy(child.clone()))).await {
            tracing::warn!(error = %e, "could not attach input");
        } else {
            tracing::info!("announcing input");
        }
    }

    if !screen_ok && !input_ok {
        return Err("neither screen nor input works".into());
    }
    Ok(dead)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The environment variable wins.
    ///
    /// This test does not touch `PATH` — `set_var` is process-global, and other tests running
    /// in parallel would lose `head` and `tr` in their `/bin/sh`. That broke once for real.
    /// That `helper_path` never reads `PATH` at all is held down by the source check below.
    #[test]
    fn the_environment_variable_wins() {
        let dir = tempfile::tempdir().unwrap();
        let bin = dir.path().join("zyrisd-display");
        std::fs::write(&bin, b"#!/bin/sh\n").unwrap();
        let real = bin.canonicalize().unwrap();

        // SAFETY: ZYRISD_DISPLAY_BIN is used here and nowhere else in this crate's tests.
        unsafe { std::env::set_var("ZYRISD_DISPLAY_BIN", &bin) };
        assert_eq!(helper_path().unwrap(), real);
        unsafe { std::env::remove_var("ZYRISD_DISPLAY_BIN") };
    }

    /// A dangling path only warns and falls through to the next candidate — no empty exec.
    #[test]
    fn a_dangling_environment_variable_does_not_become_the_answer() {
        // SAFETY: same as above.
        unsafe { std::env::set_var("ZYRISD_DISPLAY_BIN", "/nope/never/zyrisd-display") };
        assert_ne!(helper_path(), Some(PathBuf::from("/nope/never/zyrisd-display")));
        unsafe { std::env::remove_var("ZYRISD_DISPLAY_BIN") };
    }

    /// `PATH` is not a candidate. The `libexec` paths are on no `PATH` so it buys nothing,
    /// and whoever can put that name early on `PATH` gets code re-run for the daemon's life.
    ///
    /// Read the source rather than touch the environment — this rule is just the candidate list.
    #[test]
    fn path_is_never_consulted_when_locating_the_helper() {
        let source = include_str!("display.rs");
        let body = source
            .split("pub fn helper_path()")
            .nth(1)
            .and_then(|s| s.split("\nstruct Job").next())
            .expect("could not find the helper_path body");
        assert!(
            !body.contains("\"PATH\""),
            "helper_path reads PATH. It has to come off the candidate list:\n{body}"
        );
    }
}
