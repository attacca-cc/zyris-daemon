use std::process::ExitCode;

use zyrisd_node::config;

const USAGE: &str = "\
zyrisd — keeps this machine connected to Attacca

Usage:
  zyrisd enroll               Register this machine with your Attacca account
  zyrisd run                  Run the daemon (what systemd calls)
  zyrisd install              Install as a service so it starts at boot
  zyrisd status               Show enrollment, service, and capability status
  zyrisd uninstall [--purge]  Remove the service (--purge drops credentials too)
";

pub async fn dispatch() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => run().await,
        Some("enroll") => crate::enroll::enroll().await,
        Some("install") => match crate::service::install() {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("Install failed: {e}");
                ExitCode::from(2)
            }
        },
        Some("uninstall") => {
            let purge = args.iter().any(|a| a == "--purge");
            match crate::service::uninstall(purge) {
                Ok(()) => ExitCode::SUCCESS,
                Err(e) => {
                    eprintln!("Uninstall failed: {e}");
                    ExitCode::from(2)
                }
            }
        }
        Some("status") => crate::status::status().await,
        Some("--version" | "-V") => {
            println!("zyrisd {}", env!("CARGO_PKG_VERSION"));
            ExitCode::SUCCESS
        }
        Some("--help" | "-h") | None => {
            print!("{USAGE}");
            ExitCode::SUCCESS
        }
        Some(other) => {
            eprintln!("Unknown command: {other}\n");
            print!("{USAGE}");
            ExitCode::from(2)
        }
    }
}

async fn run() -> ExitCode {
    let cfg = match config::load() {
        Ok(cfg) => cfg,
        Err(e) => {
            // A restart can't fix a malformed config. Exit 2 and systemd gives up.
            tracing::error!(error = %e, "Cannot read the config");
            return ExitCode::from(2);
        }
    };
    ExitCode::from(zyrisd_node::run::run(cfg).await.code())
}
