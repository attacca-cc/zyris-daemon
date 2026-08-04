//! Shows a desktop notice when a human has to be pulled in.
//!
//! The point is to **re-read the session environment every time**. A daemon brought up at boot by
//! linger has neither `DISPLAY` nor `DBUS_SESSION_BUS_ADDRESS`, and a later login running
//! `import-environment` does not apply retroactively to an already-running process.

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
        .unwrap_or_default()
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
}
