//! Foreground enrollment.
//!
//! If the stored credential is still valid, `Enroller::obtain()` **returns it as-is and does
//! not enroll.** So asking "overwrite?" alone never re-enrolls. The store has to be emptied
//! first.

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;

use zyris::enroll::Enroller;
use zyrisd_node::config;

pub async fn enroll() -> ExitCode {
    // Enrolling as root leaves the credential in /root, where the session daemon never finds it.
    // Windows has no notion of root, so this check is Unix-only.
    #[cfg(unix)]
    let running_as_root = unsafe { libc::geteuid() } == 0;
    #[cfg(not(unix))]
    let running_as_root = false;
    if running_as_root {
        eprintln!("Do not run zyrisd enroll as root.");
        eprintln!("The credential lands in /root, where your session daemon cannot find it.");
        return ExitCode::from(2);
    }

    let cfg = match config::load() {
        Ok(cfg) => cfg,
        Err(e) => {
            eprintln!("Cannot read the config: {e}");
            return ExitCode::from(2);
        }
    };

    if let Err(e) = std::fs::create_dir_all(config::config_dir()) {
        eprintln!("Cannot create {}: {e}", config::config_dir().display());
        return ExitCode::from(2);
    }

    let store = zyrisd_node::credentials::file_store();
    match store.load().await {
        Ok(Some(existing)) => {
            println!("This computer is already enrolled.");
            println!("  account  {}", existing.owner_email);
            println!("  node     {}", existing.node_name);
            print!("Enroll again? The existing credential will be erased [y/N] ");
            let _ = std::io::stdout().flush();
            let mut answer = String::new();
            if std::io::stdin().read_line(&mut answer).is_err()
                || !matches!(answer.trim(), "y" | "Y")
            {
                println!("Cancelled.");
                return ExitCode::SUCCESS;
            }
            // Stop it first so a running daemon does not try to rotate with the old token.
            let _ = crate::service::stop_if_active();
            if let Err(e) = store.clear().await {
                eprintln!("Cannot erase the existing credential: {e}");
                return ExitCode::from(2);
            }
        }
        Ok(None) => {}
        Err(e) => {
            eprintln!("Cannot read the existing credential: {e}");
            eprintln!("Delete {} and try again.", config::credentials_path().display());
            return ExitCode::from(2);
        }
    }

    // No scopes. zyrisd is a pure tool provider with no reason to touch the owner account, and
    // "requests no account access" on the consent screen reads better anyway.
    let enroller = match Enroller::new(
        &cfg.node.server_url,
        cfg.node.name.clone(),
        platform().to_string(),
        Vec::new(),
        Arc::clone(&store),
    ) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("Cannot prepare enrollment: {e}");
            return ExitCode::from(2);
        }
    };

    match enroller.obtain().await {
        Ok(c) => {
            println!();
            println!("Enrolled. Account {}, node \"{}\".", c.owner_email, c.node_name);
            // Lay the way back on the exact path that pulled the human in. A unit stopped by a
            // cancel sits in failed from RestartPreventExitStatus=2 and nobody starts it again.
            crate::service::restart_if_installed();
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Enrollment failed: {e}");
            ExitCode::from(2)
        }
    }
}

fn platform() -> &'static str {
    match std::env::consts::OS {
        "linux" => "linux",
        "macos" => "macos",
        "windows" => "windows",
        _ => "other",
    }
}
