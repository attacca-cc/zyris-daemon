//! zyrisd entry point.
//!
//! Logs go to **stderr**. `zyrisd enroll` prints its code block on stdout, and mixing the two
//! wrecks the one screen a human has to read. Under the unit, stderr goes to the journal.

mod cli;
mod enroll;
mod service;
mod status;

use std::process::ExitCode;

/// Settle which rustls crypto provider this process uses, before anything opens a TLS connection.
///
/// Two of them are linked in. `iroh` brings `aws-lc-rs` by way of quinn, and the WebSocket client
/// brings `ring`. iroh never notices, because it builds its own config from its own
/// `default_provider()`; `tokio_tungstenite::connect_async` does not, and reads rustls' *process*
/// default instead. With both features on, rustls refuses to guess between them — and it does not
/// return an error, it panics, on the first connect:
///
/// ```text
/// Could not automatically determine the process-level CryptoProvider from Rustls crate features.
/// ```
///
/// That is the whole daemon, at the first reconnect after enrollment. This is not something the
/// node code can do anything about, and it is not something a config can turn off; the process
/// that links both is the one that has to choose, which is here.
///
/// `ring` because iroh prefers it when both are present, so one implementation does both TLS
/// stacks rather than two doing one each.
///
/// The result is deliberately dropped. `install_default` fails only if a provider is already
/// installed, and the reason to call this is to have *one*, not to have installed it.
fn install_crypto_provider() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

fn main() -> ExitCode {
    install_crypto_provider();

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

#[cfg(test)]
mod tests {
    /// Without the install this is `None`, and `None` is exactly what makes `connect_async` panic
    /// rather than return an error — so asserting a provider is present is asserting the daemon
    /// survives its first reconnect.
    #[test]
    fn a_crypto_provider_is_installed_for_the_websocket_client_to_find() {
        super::install_crypto_provider();
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_some(),
            "no process-wide provider, so the first TLS connect will panic instead of connecting"
        );
    }
}
