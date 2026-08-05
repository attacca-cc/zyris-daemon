//! `zyrisd status` — what is in what state, right now, on one screen.
//!
//! Printing the paths verbatim matters. When `enroll` (login shell) and `run` (the unit) end up
//! looking at different files, this output is the only thing that shows it.

use std::process::{Command, ExitCode};

use zyrisd_node::{config, state};

pub async fn status() -> ExitCode {
    println!("config     {}", config::config_path().display());
    println!("credential {}", config::credentials_path().display());

    let store = zyrisd_node::credentials::file_store();
    match store.load().await {
        Ok(Some(c)) => println!("enrolled   {} / {}", c.owner_email, c.node_name),
        Ok(None) => println!("enrolled   no — run `zyrisd enroll`"),
        Err(e) => println!("enrolled   unreadable: {e}"),
    }

    let unit = crate::service::unit_path();
    if unit.exists() {
        let active = Command::new("systemctl")
            .args(["--user", "is-active", "zyrisd.service"])
            .output()
            .ok()
            .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "unknown".into());
        println!("service    {active}");
    } else {
        #[cfg(windows)]
        println!("service    not installed — on Windows use the exe installer (zyrisd-setup)");
        #[cfg(not(windows))]
        println!("service    not installed — run `zyrisd install`");
    }

    match state::read() {
        Some(s) => {
            println!("connection {}", if s.connected { "connected" } else { "disconnected" });
            println!("capability {}", s.capabilities.join(", "));
        }
        None => println!("connection unknown (the daemon is not running)"),
    }
    ExitCode::SUCCESS
}
