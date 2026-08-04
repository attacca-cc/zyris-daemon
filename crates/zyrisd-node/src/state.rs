//! Runtime state the daemon writes and `zyrisd status` reads.
//!
//! The announced set lives only in daemon memory and shifts as the desktop child comes and goes.
//! `status` is a separate process; it cannot learn that from the config alone.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct State {
    pub node_name: String,
    pub connected: bool,
    pub capabilities: Vec<String>,
    pub updated_unix: i64,
}

/// The spot the unit's `RuntimeDirectory=zyrisd` creates for us.
pub fn path() -> Option<PathBuf> {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")?;
    Some(PathBuf::from(dir).join("zyrisd").join("state.json"))
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

    /// The write has to be atomic or status reads half-written JSON.
    #[test]
    fn a_state_round_trips_through_the_runtime_file() {
        let dir = tempfile::tempdir().unwrap();
        // SAFETY: this test is the only one that touches XDG_RUNTIME_DIR.
        unsafe { std::env::set_var("XDG_RUNTIME_DIR", dir.path()) };
        let s = State {
            node_name: "box".into(),
            connected: true,
            capabilities: vec!["terminal".into(), "file_io".into()],
            updated_unix: 1_700_000_000,
        };
        write(&s);
        assert_eq!(read().unwrap(), s);
        assert!(!dir.path().join("zyrisd/state.json.tmp").exists(), "temp file left behind");
    }
}
