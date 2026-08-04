//! zyrisd entry point.
//!
//! Logs go to **stderr**. `zyrisd enroll` prints its code block on stdout, and mixing the two
//! wrecks the one screen a human has to read. Under the unit, stderr goes to the journal.

mod cli;
mod enroll;
mod service;
mod status;

use std::process::ExitCode;

fn main() -> ExitCode {
    tracing_subscriber::fmt()
        .with_writer(std::io::stderr)
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "zyrisd=info,zyrisd_node=info,zyris=info".into()),
        )
        .init();

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(rt) => rt,
        Err(e) => {
            eprintln!("Cannot create the tokio runtime: {e}");
            return ExitCode::from(1);
        }
    };
    runtime.block_on(cli::dispatch())
}
