//! Runtime state the daemon writes and `zyrisd status` reads.
//!
//! The announced set lives only in daemon memory and shifts as the desktop child comes and goes.
//! `status` is a separate process; it cannot learn that from the config alone.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub node_name: String,
    pub connected: bool,
    pub capabilities: Vec<String>,
    pub updated_unix: i64,
    /// This machine's peer endpoint id, once transfer has bound one — the thing the fingerprint a
    /// person reads out is a fingerprint of.
    ///
    /// Here rather than derived from the key file by whoever wants it: reading that file means
    /// knowing its format, and the one function that knows it creates a key when there is none,
    /// which is not a side effect `zyrisd status` should have. `None` while transfer is off or has
    /// not bound yet, which is the honest answer — there is no fingerprint to compare then.
    ///
    /// `serde(default)` so a state file written by an older build still parses.
    #[serde(default)]
    pub endpoint_id: Option<String>,
}

impl State {
    /// Whether this file is recent enough to describe a daemon that still exists.
    ///
    /// A future timestamp counts as recent. It means the clock moved backwards between the write
    /// and this read, and "the daemon disappeared" is the wrong thing to conclude from that.
    pub fn is_recent(&self, now_unix: i64) -> bool {
        now_unix - self.updated_unix <= FRESH_FOR_SECS
    }
}

/// Seconds since the epoch, or 0 if the clock is before it.
pub fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// How long a state file is believed after it was last written.
///
/// The daemon rewrites it every 30s, so anything older than this is a file whose writer is gone.
/// Wide enough that a paused VM or a slow disk does not make a live daemon look dead, narrow
/// enough that a killed one stops being quoted within a couple of minutes.
pub const FRESH_FOR_SECS: i64 = 150;

/// The spot the unit's `RuntimeDirectory=zyrisd` creates for us.
///
/// **Windows has no `XDG_RUNTIME_DIR`**, and returning `None` there is not "no state" — it made
/// `write` a silent no-op, so `zyrisd status` called a running daemon "not running", `zyrisd peers`
/// could not recognise this machine among the account's nodes, and the guard that stops `zyrisd
/// pin` pinning the machine it is typed on never fired. One missing Linux variable, four things.
///
/// `LOCALAPPDATA` is the per-user spot Windows has; what it does not have is
/// `XDG_RUNTIME_DIR`'s "emptied when the session ends". That property is what let `status` trust
/// the file's contents without asking how old they were, so the age check in [`State::is_recent`]
/// replaces it rather than the directory choice pretending to.
pub fn path() -> Option<PathBuf> {
    Some(state_file_in(&runtime_dir()?))
}

/// The variable that names the per-user directory to write under.
#[cfg(not(windows))]
const RUNTIME_DIR_VAR: &str = "XDG_RUNTIME_DIR";
#[cfg(windows)]
const RUNTIME_DIR_VAR: &str = "LOCALAPPDATA";

fn runtime_dir() -> Option<PathBuf> {
    std::env::var_os(RUNTIME_DIR_VAR).filter(|d| !d.is_empty()).map(PathBuf::from)
}

fn state_file_in(dir: &Path) -> PathBuf {
    dir.join("zyrisd").join("state.json")
}

/// Ignores failure. Not being able to write the state file is no reason to stop the daemon.
///
/// Temp file + rename, so `status` never reads half-written JSON.
pub fn write(state: &State) {
    let Some(path) = path() else { return };
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let tmp = path.with_extension("json.tmp");
    let Ok(text) = serde_json::to_string_pretty(state) else { return };
    if std::fs::write(&tmp, text).is_ok() && std::fs::rename(&tmp, &path).is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
}

pub fn read() -> Option<State> {
    let text = std::fs::read_to_string(path()?).ok()?;
    serde_json::from_str(&text).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The daemon has to have somewhere to write on every platform it ships to. Returning `None`
    /// on Windows made `write` a silent no-op, and from there `status` called a running daemon
    /// "not running", `peers` could not spot this machine in the account's list, and the guard
    /// against pinning the machine you are typing on never fired.
    ///
    /// Asserted without touching the environment: another test in this binary sets
    /// `XDG_RUNTIME_DIR`, and two tests writing the same variable from different threads is how a
    /// suite starts failing depending on the order it happens to run in.
    #[test]
    fn every_platform_this_ships_to_names_a_directory_to_write_under() {
        assert!(!RUNTIME_DIR_VAR.is_empty());
        #[cfg(windows)]
        assert_eq!(RUNTIME_DIR_VAR, "LOCALAPPDATA", "Windows never sets XDG_RUNTIME_DIR");
        assert_eq!(
            state_file_in(Path::new("/tmp/x")),
            PathBuf::from("/tmp/x").join("zyrisd").join("state.json")
        );
    }

    /// `status` believes a state file only while its writer plausibly still exists. On Linux the
    /// runtime directory is emptied at logout so this rarely mattered; nothing empties
    /// `LOCALAPPDATA`, so a daemon killed mid-run would otherwise claim to be connected forever.
    #[test]
    fn a_state_file_outlives_its_writer_and_stops_being_believed() {
        let s = State { updated_unix: 1_700_000_000, connected: true, ..State::default() };
        assert!(s.is_recent(1_700_000_000 + FRESH_FOR_SECS), "still inside the window");
        assert!(!s.is_recent(1_700_000_000 + FRESH_FOR_SECS + 1), "the writer is gone");
    }

    /// A clock that stepped backwards between the write and the read is not evidence that the
    /// daemon died, and treating it as such would report a healthy node as absent.
    #[test]
    fn a_timestamp_from_the_future_is_still_believed() {
        let s = State { updated_unix: 1_700_000_500, ..State::default() };
        assert!(s.is_recent(1_700_000_000));
    }

    /// The write has to be atomic or status reads half-written JSON.
    #[test]
    fn a_state_round_trips_through_the_runtime_file() {
        let dir = tempfile::tempdir().unwrap();
        // `RUNTIME_DIR_VAR`, not `XDG_RUNTIME_DIR`: on Windows the latter is read by nothing, so
        // this would have written into the real per-user directory and then compared against
        // whatever the daemon on this machine last wrote there.
        // SAFETY: this test is the only one that touches that variable.
        unsafe { std::env::set_var(RUNTIME_DIR_VAR, dir.path()) };
        let s = State {
            node_name: "box".into(),
            connected: true,
            capabilities: vec!["terminal".into(), "file_io".into()],
            endpoint_id: Some("d3adb33f".into()),
            updated_unix: 1_700_000_000,
        };
        write(&s);
        assert_eq!(read().unwrap(), s);
        assert!(!dir.path().join("zyrisd/state.json.tmp").exists(), "temp file left behind");
    }

    /// A state file written by a build that predates `endpoint_id` has to keep parsing.
    ///
    /// The file survives an upgrade — it is whatever the daemon last wrote, and `zyrisd status`
    /// reads it the moment the new binary lands, before any daemon has rewritten it. Without the
    /// `serde(default)` this is not a missing fingerprint, it is `status` reporting "the daemon is
    /// not running" about a daemon that is.
    #[test]
    fn a_state_file_from_before_the_endpoint_id_still_parses() {
        let older = r#"{
            "node_name": "box",
            "connected": true,
            "capabilities": ["terminal"],
            "updated_unix": 1700000000
        }"#;
        let parsed: State = serde_json::from_str(older).expect("an older state file must parse");
        assert_eq!(parsed.node_name, "box");
        assert!(parsed.connected);
        assert_eq!(parsed.endpoint_id, None, "there was no fingerprint to know about");
    }
}
