//! A systemd **user** unit.
//!
//! A system unit does not work — the PTY shell inherits the daemon environment, and the system
//! manager's has no `$SHELL`; `/run/user/<uid>` is 0700 so another uid never reaches the session;
//! and credentials are 0600 files, so enroll and the daemon must run as the same uid.
//!
//! Why the unit is written here and not by the packaging: `.deb` (`/usr/bin`) and
//! `install.sh` (`~/.local/bin`) need different `ExecStart` lines, and whoever writes it
//! always knows where it lives.

use std::path::{Path, PathBuf};
use std::process::Command;

use zyrisd_node::config;

pub fn unit_dir() -> PathBuf {
    config::home().unwrap_or_else(|_| PathBuf::from("/nonexistent")).join(".config/systemd/user")
}

pub fn unit_path() -> PathBuf {
    unit_dir().join("zyrisd.service")
}

pub fn unit_text(exec: &Path, display_bin: Option<&Path>) -> String {
    let exec = exec.display();
    let display_line = match display_bin {
        Some(p) => format!("Environment=ZYRISD_DISPLAY_BIN={}\n", p.display()),
        None => String::new(),
    };
    format!(
        "[Unit]\n\
         Description=Zyris daemon\n\
         # Remove the package first and ExecStart is gone. That failure is 203/EXEC, not exit\n\
         # code 2, so without this guard the unit spins in a restart loop.\n\
         ConditionPathExists={exec}\n\
         \n\
         [Service]\n\
         Type=simple\n\
         ExecStart={exec} run\n\
         {display_line}\
         RuntimeDirectory=zyrisd\n\
         Restart=on-failure\n\
         RestartSec=5\n\
         # Exit code 2 is a condition a human has to clear. Restarting cannot fix it.\n\
         RestartPreventExitStatus=2\n\
         # Shortened from the 1m30s default. This budget is not for the daemon (200ms) but\n\
         # for the agent's open PTY children, since expiry SIGKILLs the whole cgroup.\n\
         TimeoutStopSec=10\n\
         \n\
         [Install]\n\
         WantedBy=default.target\n"
    )
}

fn systemctl(args: &[&str]) -> std::io::Result<std::process::ExitStatus> {
    Command::new("systemctl").arg("--user").args(args).status()
}

fn unit_is_installed() -> bool {
    unit_path().exists()
}

pub fn stop_if_active() -> std::io::Result<()> {
    if unit_is_installed() {
        systemctl(&["stop", "zyrisd.service"])?;
    }
    Ok(())
}

/// Call this once enrollment is done.
///
/// A revoke stops the unit with exit 2, and `RestartPreventExitStatus=2` wedges it in failed;
/// `restart` won't take without `reset-failed`. Without this there is no way back onto the
/// exact path the product just told a human to walk.
pub fn restart_if_installed() {
    if !unit_is_installed() {
        println!();
        println!("Run `zyrisd install` and it will connect automatically on every boot.");
        return;
    }
    let _ = systemctl(&["reset-failed", "zyrisd.service"]);
    match systemctl(&["restart", "zyrisd.service"]) {
        Ok(s) if s.success() => println!("Restarted the service."),
        _ => println!("Could not restart the service: systemctl --user restart zyrisd"),
    }
}

/// Finds the desktop helper path to bake into the unit. `PATH` is not a candidate —
/// same reason as `zyrisd-node::display::helper_path`.
///
/// **An already-set `$ZYRISD_DISPLAY_BIN` wins.** Otherwise, in layouts where the child is not
/// in its conventional place — a dev tree — the desktop works in the foreground and silently
/// drops out under the service. That has already happened once.
fn display_helper_beside(exec: &Path) -> Option<PathBuf> {
    if let Some(p) = std::env::var_os("ZYRISD_DISPLAY_BIN").map(PathBuf::from) {
        if p.exists() {
            return p.canonicalize().ok().or(Some(p));
        }
    }
    let parent = exec.parent()?;
    [
        parent.join("../libexec/zyrisd-display"),
        parent.join("zyrisd-display"),
        PathBuf::from("/usr/libexec/zyrisd-display"),
    ]
    .into_iter()
    .find(|p| p.exists())
    .and_then(|p| p.canonicalize().ok())
}

pub fn install() -> anyhow::Result<()> {
    if unsafe { libc::geteuid() } == 0 {
        anyhow::bail!("Do not run zyrisd install as root. It is a user session service.");
    }
    // Don't judge by the exit code of `is-system-running`. One unrelated degraded unit makes
    // it call a healthy machine broken.
    if std::env::var_os("XDG_RUNTIME_DIR").is_none() {
        anyhow::bail!("XDG_RUNTIME_DIR is not set. Run this from a graphical/login session.");
    }

    let exec = std::env::current_exe()?;
    let display_bin = display_helper_beside(&exec);
    let text = unit_text(&exec, display_bin.as_deref());

    std::fs::create_dir_all(unit_dir())?;
    let changed = std::fs::read_to_string(unit_path()).ok().as_deref() != Some(text.as_str());
    if changed {
        std::fs::write(unit_path(), &text)?;
        systemctl(&["daemon-reload"])?;
    }

    // Without linger the user manager only starts at login, so "starts at boot" is a lie.
    // An ordinary user can turn this on for themselves without escalating.
    let user = std::env::var("USER").unwrap_or_default();
    if !user.is_empty() {
        let _ = Command::new("loginctl").args(["enable-linger", &user]).status();
    }

    systemctl(&["enable", "zyrisd.service"])?;
    let _ = systemctl(&["reset-failed", "zyrisd.service"]);
    // `start` is a no-op on an active unit; if it changed, only restart runs the new binary.
    systemctl(&[if changed { "restart" } else { "start" }, "zyrisd.service"])?;

    if !report_liveness() {
        // The unit exists but the daemon didn't come up. Scripts need to tell those apart —
        // the usual cause is that `zyrisd enroll` hasn't been run yet.
        anyhow::bail!("Installed the service, but the daemon did not come up");
    }
    Ok(())
}

/// With `Type=simple`, `start` returns success right after fork. Check it is actually alive
/// so that "installed" isn't a lie.
fn report_liveness() -> bool {
    for _ in 0..12 {
        std::thread::sleep(std::time::Duration::from_millis(300));
        let active = Command::new("systemctl")
            .args(["--user", "is-active", "zyrisd.service"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string());
        match active.as_deref() {
            Some("active") => {
                println!("zyrisd is running. It will connect automatically on every boot.");
                return true;
            }
            Some("failed") => break,
            _ => continue,
        }
    }
    eprintln!("zyrisd did not come up. Check that enrollment finished:");
    eprintln!("  zyrisd enroll        if you have not enrolled yet");
    eprintln!("  zyrisd status");
    eprintln!("  journalctl --user -u zyrisd -n 30");
    false
}

pub fn uninstall(purge: bool) -> anyhow::Result<()> {
    let _ = systemctl(&["stop", "zyrisd.service"]);
    let _ = systemctl(&["disable", "zyrisd.service"]);
    if unit_path().exists() {
        std::fs::remove_file(unit_path())?;
    }
    let _ = systemctl(&["daemon-reload"]);
    // Deleting the unit file leaves the failed state in systemd's memory. Then even after
    // removal, `systemctl --user status` shows a unit that no longer exists as failed.
    let _ = systemctl(&["reset-failed", "zyrisd.service"]);

    let user = std::env::var("USER").unwrap_or_default();
    if !user.is_empty() {
        let _ = Command::new("loginctl").args(["disable-linger", &user]).status();
    }

    if let Some(dir) = std::env::var_os("XDG_RUNTIME_DIR") {
        let _ = std::fs::remove_dir_all(PathBuf::from(dir).join("zyrisd"));
    }

    if purge {
        let _ = std::fs::remove_dir_all(config::config_dir());
        println!("Removed credentials and configuration.");
    } else {
        println!("Left the credentials in place. Use --purge to remove them.");
    }
    println!("Uninstalled.");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What the unit must carry, and what it must not.
    #[test]
    fn the_unit_carries_the_directives_that_matter() {
        let text = unit_text(Path::new("/opt/zyrisd"), Some(Path::new("/opt/zyrisd-display")));

        assert!(text.contains("ExecStart=/opt/zyrisd run"));
        assert!(text.contains("Environment=ZYRISD_DISPLAY_BIN=/opt/zyrisd-display"));
        // Exit 2 is a condition a human has to clear. Restarting won't fix it.
        assert!(text.contains("RestartPreventExitStatus=2"));
        assert!(text.contains("Restart=on-failure"));
        // Remove the package first and ExecStart is gone. That failure is 203/EXEC, not
        // exit code 2, and without the guard it spins in a restart loop.
        assert!(text.contains("ConditionPathExists=/opt/zyrisd"));
        // Where the state file that status reads lives.
        assert!(text.contains("RuntimeDirectory=zyrisd"));
        // It comes up at boot via linger, so this is not a graphical session.
        assert!(text.contains("WantedBy=default.target"));

        // It's the default, so it does nothing. Writing it makes readers think it does.
        assert!(!text.contains("KillSignal="));
        // Silently ignored in user units, yet systemd-analyze --user verify lets it through.
        assert!(!text.contains("network-online"));
    }

    /// No desktop child, no line. An empty value makes the parent exec an empty path.
    #[test]
    fn without_a_display_helper_the_environment_line_is_absent() {
        let text = unit_text(Path::new("/opt/zyrisd"), None);
        assert!(!text.contains("ZYRISD_DISPLAY_BIN"));
    }
}
