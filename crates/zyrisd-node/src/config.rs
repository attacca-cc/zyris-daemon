//! `$HOME/.config/zyrisd/config.toml`.
//!
//! The point is pinning the path to a literal `$HOME`. Do the idiomatic thing and honor
//! `XDG_CONFIG_HOME`, and since the systemd user manager environment lacks that variable,
//! `zyrisd enroll` (login shell) and `zyrisd run` (the unit) can read different files.

use std::path::{Path, PathBuf};

use serde::Deserialize;

#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    #[error("config file syntax error: {0}")]
    Syntax(String),
    #[error("config is not valid: {0}")]
    Invalid(String),
    #[error("$HOME is not set")]
    NoHome,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Config {
    pub node: NodeConfig,
    pub files: FilesConfig,
    pub terminal: TerminalConfig,
    pub desktop: DesktopConfig,
    pub notify: NotifyConfig,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NodeConfig {
    /// Defaults to `<hostname>-<username>`. Hostname alone gives two identically named nodes in
    /// Attacca when two users share a machine, and nothing in the install flow catches it.
    pub name: String,
    pub server_url: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct FilesConfig {
    /// `roots[0]` is the base for relative paths and the PTY cwd, because upstream takes exactly
    /// one root. The rest serve only as an allow list for absolute paths.
    pub roots: Vec<PathBuf>,
    pub deny: Vec<PathBuf>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct TerminalConfig {
    /// Cap for stdout and stderr each. Not 1 MiB because Attacca measures a tool result with
    /// `serde_json::to_vec().len()` and compares it against `ZYRIS_MAX_RESULT_BYTES` (default
    /// 1,000,000) — 1 MiB blows that budget before JSON escaping even starts.
    pub max_output_bytes: usize,
    /// The effective timeout is `min(what the caller asked for, this)`.
    pub exec_timeout_secs: u64,
    /// Vars cleared before handing off to PTY and exec. The two defaults carry **credentials to
    /// other machines**, so the blast radius leaves this box. Undo it with `unset_env = []`.
    pub unset_env: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct DesktopConfig {
    pub enabled: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct NotifyConfig {
    pub enabled: bool,
}

impl Default for NodeConfig {
    fn default() -> NodeConfig {
        NodeConfig { name: default_node_name(), server_url: zyris::DEFAULT_SERVER_URL.to_string() }
    }
}

impl Default for FilesConfig {
    fn default() -> FilesConfig {
        FilesConfig { roots: vec![PathBuf::from("~")], deny: Vec::new() }
    }
}

impl Default for TerminalConfig {
    fn default() -> TerminalConfig {
        TerminalConfig {
            max_output_bytes: 256 * 1024,
            exec_timeout_secs: 120,
            unset_env: vec!["SSH_AUTH_SOCK".to_string(), "GPG_AGENT_INFO".to_string()],
        }
    }
}

impl Default for DesktopConfig {
    fn default() -> DesktopConfig {
        DesktopConfig { enabled: true }
    }
}

impl Default for NotifyConfig {
    fn default() -> NotifyConfig {
        NotifyConfig { enabled: true }
    }
}

fn default_node_name() -> String {
    let host = zyris::machine_name().unwrap_or_else(|| "zyrisd".to_string());
    match std::env::var("USER").ok().filter(|u| !u.is_empty()) {
        Some(user) => format!("{host}-{user}"),
        None => host,
    }
}

pub fn home() -> Result<PathBuf, ConfigError> {
    let home = std::env::var_os("HOME").filter(|h| !h.is_empty()).map(PathBuf::from);
    // Native Windows shells (cmd/PowerShell) have no HOME. USERPROFILE takes that place.
    home.or_else(windows_home).ok_or(ConfigError::NoHome)
}

#[cfg(windows)]
fn windows_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE").filter(|h| !h.is_empty()).map(PathBuf::from)
}

#[cfg(not(windows))]
fn windows_home() -> Option<PathBuf> {
    None
}

/// `$HOME/.config/zyrisd`. Does not honor `XDG_CONFIG_HOME` — see the module comment.
pub fn config_dir() -> PathBuf {
    home().unwrap_or_else(|_| PathBuf::from("/nonexistent")).join(".config").join("zyrisd")
}

pub fn config_path() -> PathBuf {
    config_dir().join("config.toml")
}

pub fn credentials_path() -> PathBuf {
    config_dir().join("credentials.json")
}

/// Expands only `~` and `~/`. `~user` is not supported.
fn expand_tilde(raw: &str, home: &Path) -> PathBuf {
    if raw == "~" {
        return home.to_path_buf();
    }
    match raw.strip_prefix("~/") {
        Some(rest) => home.join(rest),
        None => PathBuf::from(raw),
    }
}

fn expand_all(raw: &[PathBuf], home: &Path) -> Result<Vec<PathBuf>, ConfigError> {
    raw.iter()
        .map(|p| {
            let expanded = expand_tilde(&p.to_string_lossy(), home);
            if expanded.is_absolute() {
                Ok(expanded)
            } else {
                Err(ConfigError::Invalid(format!(
                    "paths must be absolute (or start with ~): {}",
                    p.display()
                )))
            }
        })
        .collect()
}

fn parse(text: &str, home: &Path) -> Result<Config, ConfigError> {
    let mut cfg: Config = toml::from_str(text).map_err(|e| ConfigError::Syntax(e.to_string()))?;
    cfg.files.roots = expand_all(&cfg.files.roots, home)?;
    cfg.files.deny = expand_all(&cfg.files.deny, home)?;
    if cfg.files.roots.is_empty() {
        return Err(ConfigError::Invalid("files.roots is empty".into()));
    }
    if cfg.terminal.max_output_bytes == 0 {
        return Err(ConfigError::Invalid("terminal.max_output_bytes cannot be 0".into()));
    }
    Ok(cfg)
}

/// No file means defaults. If there is one, read it and validate.
pub fn load() -> Result<Config, ConfigError> {
    let home = home()?;
    let path = config_path();
    match std::fs::read_to_string(&path) {
        Ok(text) => parse(&text, &home),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            let mut cfg = Config::default();
            cfg.files.roots = expand_all(&cfg.files.roots, &home)?;
            Ok(cfg)
        }
        Err(e) => Err(ConfigError::Invalid(format!("could not read {}: {e}", path.display()))),
    }
}

impl Config {
    /// Yields the roots and deny list actually used.
    ///
    /// A missing root is warned about and dropped. One late mount (NFS, automount, removable,
    /// LUKS) and the daemon started at boot dies, systemd gives up restarting by design, and even
    /// once the mount lands someone has to start it by hand. Dropping a root only narrows access.
    pub fn resolve_roots(&self) -> (Vec<PathBuf>, Vec<PathBuf>) {
        let roots: Vec<PathBuf> = self
            .files
            .roots
            .iter()
            .filter(|r| {
                let ok = r.exists();
                if !ok {
                    tracing::warn!(root = %r.display(), "root missing, dropped from allow list");
                }
                ok
            })
            .cloned()
            .collect();
        let mut deny = self.files.deny.clone();
        deny.push(config_dir());
        (roots, deny)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// It comes up with no config. The default root is home and deny is empty.
    #[test]
    fn defaults_stand_alone_without_a_file() {
        let cfg = Config::default();
        assert_eq!(cfg.files.roots, vec![PathBuf::from("~")]);
        assert!(cfg.files.deny.is_empty());
        assert_eq!(cfg.terminal.max_output_bytes, 262_144);
        assert_eq!(cfg.terminal.unset_env, vec!["SSH_AUTH_SOCK", "GPG_AGENT_INFO"]);
        assert!(cfg.desktop.enabled && cfg.notify.enabled);
    }

    /// Expands only `~` and `~/`. Neither TOML nor serde nor std does this, so skip it and
    /// the documented default itself becomes "a path that does not exist".
    #[test]
    fn tilde_expands_only_at_the_front() {
        let home = PathBuf::from("/home/x");
        assert_eq!(expand_tilde("~", &home), PathBuf::from("/home/x"));
        assert_eq!(expand_tilde("~/work", &home), PathBuf::from("/home/x/work"));
        assert_eq!(expand_tilde("/abs/~", &home), PathBuf::from("/abs/~"));
    }

    /// A relative path is a config error. Scope must not depend on the daemon's cwd.
    #[test]
    fn relative_roots_are_rejected() {
        let toml = "[files]\nroots = [\"work\"]";
        let err = parse(toml, &PathBuf::from("/home/x")).unwrap_err();
        assert!(matches!(err, ConfigError::Invalid(_)), "{err:?}");
    }

    #[test]
    fn a_syntax_error_is_its_own_kind() {
        let err = parse("[files\nroots = []", &PathBuf::from("/home/x")).unwrap_err();
        assert!(matches!(err, ConfigError::Syntax(_)), "{err:?}");
    }

    /// A missing root is no reason to die. One late mount and the daemon started at boot stays
    /// down for good. Warn and drop it — access only narrows, never widens.
    #[test]
    fn a_missing_root_is_dropped_rather_than_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = Config {
            files: FilesConfig {
                roots: vec![dir.path().to_path_buf(), PathBuf::from("/nope/never")],
                deny: vec![],
            },
            ..Config::default()
        };
        let (roots, _) = cfg.resolve_roots();
        assert_eq!(roots, vec![dir.path().to_path_buf()]);
    }

    /// The credential directory is a deny the user cannot remove. The refresh_token inside
    /// re-issues node identity without this box; "the terminal is open anyway" does not apply.
    #[test]
    fn the_credential_directory_is_always_denied() {
        let (_, deny) = Config::default().resolve_roots();
        assert!(deny.contains(&config_dir()), "{deny:?}");
    }
}
