//! Shows a desktop notice when a human has to be pulled in.
//!
//! The point is to **re-read the session environment every time**. A daemon brought up at boot by
//! linger has neither `DISPLAY` nor `DBUS_SESSION_BUS_ADDRESS`, and a later login running
//! `import-environment` does not apply retroactively to an already-running process.

// Both are only reached from the unix fallback below, so on Windows the import itself is unused.
#[cfg(unix)]
use std::path::{Path, PathBuf};
use std::process::Command;

/// Session variables to hand the child and notifications. Nothing else, least of all `SSH_AUTH_SOCK`.
const WANTED: [&str; 4] = ["DISPLAY", "WAYLAND_DISPLAY", "XAUTHORITY", "DBUS_SESSION_BUS_ADDRESS"];

fn parse_environment(raw: &str) -> Vec<(String, String)> {
    raw.lines()
        .filter_map(|line| line.split_once('='))
        .filter(|(k, _)| WANTED.contains(k))
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}

/// The graphical session environment as it stands right now.
pub fn session_env() -> Vec<(String, String)> {
    Command::new("systemctl")
        .args(["--user", "show-environment"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| parse_environment(&String::from_utf8_lossy(&o.stdout)))
        .unwrap_or_else(fallback_environment)
}

/// When `systemctl --user` fails (early boot, no systemd, containers), find the session variables
/// directly at the standard socket locations. This is the fallback from spec §6.
///
/// - `WAYLAND_DISPLAY` — name of the `/run/user/<uid>/wayland-*` socket
/// - `DISPLAY` — number of the `/tmp/.X11-unix/X*` socket (the lowest one)
/// - `DBUS_SESSION_BUS_ADDRESS` — `unix:path=…` when `/run/user/<uid>/bus` exists
///
/// `XAUTHORITY` is not guessed — the session-scoped file can live anywhere. Without it, xcb
/// finds it itself or just fails, and that failure is caught by the child probe (`capture_works`)
/// which then skips the announce, so it stays quiet.
#[cfg(unix)]
fn fallback_environment() -> Vec<(String, String)> {
    let uid = unsafe { libc::geteuid() };
    fallback_from_dirs(&PathBuf::from(format!("/run/user/{uid}")), Path::new("/tmp/.X11-unix"))
}

/// Windows has no session socket dirs for this fallback to scan. Desktop notices are future work.
#[cfg(not(unix))]
fn fallback_environment() -> Vec<(String, String)> {
    Vec::new()
}

/// The pure part, split out so tests can hand it socket directories directly.
///
/// Unix-only along with its one caller: the paths it walks are Wayland, X11 and dbus sockets,
/// which is why the Windows `fallback_environment` above returns nothing. Left ungated it was a
/// never-used function on Windows, and its test failed there for the same reason — the socket
/// layout it asserts does not exist on that platform.
#[cfg(unix)]
fn fallback_from_dirs(run_user: &Path, x11: &Path) -> Vec<(String, String)> {
    let mut out = Vec::new();

    if let Ok(entries) = std::fs::read_dir(run_user) {
        let mut wayland: Option<String> = None;
        let mut bus = false;
        for e in entries.flatten() {
            let name = e.file_name().to_string_lossy().to_string();
            if name == "bus" {
                bus = true;
            }
            // wayland-0.lock sits right next to wayland-0. The env var must be the socket name.
            if wayland.is_none() && name.starts_with("wayland-") && !name.ends_with(".lock") {
                wayland = Some(name);
            }
        }
        if let Some(w) = wayland {
            out.push(("WAYLAND_DISPLAY".into(), w));
        }
        if bus {
            out.push(("DBUS_SESSION_BUS_ADDRESS".into(), format!("unix:path={}/bus", run_user.display())));
        }
    }

    if let Ok(entries) = std::fs::read_dir(x11) {
        let mut numbers: Vec<String> = entries
            .flatten()
            .filter_map(|e| {
                let name = e.file_name().to_string_lossy().to_string();
                name.strip_prefix('X')
                    .filter(|n| !n.ends_with(".lock"))
                    .map(|n| n.to_string())
            })
            .collect();
        numbers.sort();
        if let Some(n) = numbers.into_iter().next() {
            out.push(("DISPLAY".into(), format!(":{n}")));
        }
    }
    out
}

/// Best effort. Missing binary or no session: pass over quietly — the log always says why.
///
/// The body is fixed text written by zyrisd; server-supplied strings only reach the log. `--` ends
/// option parsing so a summary starting with `-` is never taken as a flag.
pub fn needs_attention(summary: &str, body: &str) {
    let mut cmd = Command::new("notify-send");
    cmd.arg("--app-name=zyrisd").arg("--urgency=critical").arg("--").arg(summary).arg(body);
    for (k, v) in session_env() {
        cmd.env(k, v);
    }
    match cmd.status() {
        Ok(s) if s.success() => {}
        _ => tracing::debug!("failed to show the desktop notification"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Takes only the four we need from show-environment output. One `KEY=VALUE` per line, and `=`
    /// may appear inside the value.
    #[test]
    fn only_the_session_variables_are_taken() {
        let raw = "DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/1000/bus\n\
                   LANG=ko_KR.UTF-8\n\
                   WAYLAND_DISPLAY=wayland-0\n\
                   SSH_AUTH_SOCK=/run/user/1000/keyring/ssh\n\
                   DISPLAY=:0\n";
        let got = parse_environment(raw);
        assert_eq!(got.len(), 3);
        assert_eq!(
            got.iter().find(|(k, _)| k == "DBUS_SESSION_BUS_ADDRESS").unwrap().1,
            "unix:path=/run/user/1000/bus",
            "splits at the first = even when the value contains one"
        );
        assert!(got.iter().any(|(k, v)| k == "WAYLAND_DISPLAY" && v == "wayland-0"));
        assert!(
            !got.iter().any(|(k, _)| k == "SSH_AUTH_SOCK"),
            "nothing we do not need, above all no credentials that reach other machines"
        );
    }

    /// Finds the session variables in the standard socket dirs without systemctl (spec §6 fallback).
    /// Also checks that a `.lock` file is never used as the socket name.
    #[cfg(unix)]
    #[test]
    fn the_fallback_scans_the_standard_socket_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let run = dir.path().join("run");
        std::fs::create_dir(&run).unwrap();
        std::fs::write(run.join("wayland-0"), b"").unwrap();
        std::fs::write(run.join("wayland-0.lock"), b"").unwrap();
        std::fs::write(run.join("bus"), b"").unwrap();
        let x11 = dir.path().join("x11");
        std::fs::create_dir(&x11).unwrap();
        std::fs::write(x11.join("X1"), b"").unwrap();
        std::fs::write(x11.join("X0"), b"").unwrap();
        std::fs::write(x11.join("X0.lock"), b"").unwrap();

        let got = fallback_from_dirs(&run, &x11);
        assert_eq!(
            got.iter().find(|(k, _)| k == "WAYLAND_DISPLAY").unwrap().1,
            "wayland-0",
            ".lock must never be used as the socket name"
        );
        assert_eq!(got.iter().find(|(k, _)| k == "DISPLAY").unwrap().1, ":0");
        assert_eq!(
            got.iter().find(|(k, _)| k == "DBUS_SESSION_BUS_ADDRESS").unwrap().1,
            format!("unix:path={}", run.join("bus").display())
        );
    }

    /// No socket at all means an empty list. Injecting nothing beats injecting a wrong value.
    #[test]
    fn the_fallback_without_any_socket_is_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(fallback_from_dirs(&dir.path().join("run"), &dir.path().join("x11")).is_empty());
    }
}
