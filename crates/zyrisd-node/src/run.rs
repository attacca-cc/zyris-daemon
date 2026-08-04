//! Assembles the node, keeps it alive, and dies properly when it dies.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use zyris::runtime::{CredentialsError, RunError, Runner};
use zyris::{Connection, NodeKind};
use zyris_caps::{FileIoServer, TerminalServer};

use crate::config::Config;
use crate::credentials::{file_store, StoredOnly};
use crate::file_io::GatedFileIo;
use crate::gate::PathGate;
use crate::terminal::GatedTerminal;

/// Long enough for the close frame to reach the server. Same value as upstream `CLOSE_GRACE`.
const CLOSE_GRACE: Duration = Duration::from_millis(200);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    Ok,
    Retry,
    NeedsOperator,
}

impl Exit {
    pub fn code(&self) -> u8 {
        match self {
            Exit::Ok => 0,
            Exit::Retry => 1,
            Exit::NeedsOperator => 2,
        }
    }
}

/// Splits `Refused` in two.
///
/// `RunError::Refused` folds in every `WireError` with `retriable == false`, so it is not only
/// "the server refused this node" — `ParseError`, `Internal`, `PayloadTooLarge` land here too, and
/// one 500 during a deploy must not park the daemon forever. But `Refused` is a `String`, so the
/// `ErrorCode` is gone. Don't scrape the string — read the flag `Credentials` set on revocation.
fn classify_refused(credential_gave_up: bool) -> Exit {
    if credential_gave_up {
        Exit::NeedsOperator
    } else {
        Exit::Retry
    }
}

/// Records the current announce set so `zyrisd status` can read it.
///
/// The set shifts at runtime as the desktop child comes and goes — config alone can't tell you.
fn publish_state(node_name: &str, connected: bool, caps: &zyris::Capabilities) {
    crate::state::write(&crate::state::State {
        node_name: node_name.to_string(),
        connected,
        capabilities: caps.descriptors().iter().map(|d| d.name.clone()).collect(),
        updated_unix: std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(0),
    });
}

/// Where the live connection is handed to the signal handler.
#[derive(Clone, Default)]
pub struct ConnSlot(Arc<Mutex<Option<Connection>>>);

impl ConnSlot {
    pub fn new() -> ConnSlot {
        ConnSlot::default()
    }

    pub fn put(&self, conn: Connection) {
        *self.0.lock().unwrap() = Some(conn);
    }

    /// The slot is not cleared on a drop, so it may hold a dead connection.
    pub fn close(&self) {
        if let Some(conn) = self.0.lock().unwrap().as_ref() {
            if !conn.is_closed() {
                conn.close("zyrisd shutting down");
            }
        }
    }
}

pub async fn run(cfg: Config) -> Exit {
    let (roots, deny) = cfg.resolve_roots();
    if roots.is_empty() {
        // May not be a config error — the mounts may just not be up yet. Retry.
        tracing::error!("None of the configured roots exist. Waiting for mounts");
        return Exit::Retry;
    }

    let store = file_store();
    let credentials = match StoredOnly::new(&cfg.node.server_url, cfg.node.name.clone(), store) {
        Ok(c) => Arc::new(c),
        Err(e) => {
            tracing::error!(error = %e, "Cannot prepare credentials");
            return Exit::NeedsOperator;
        }
    };

    // `scopes_pinned` is private, so struct update syntax is out. Assign field by field.
    let mut run_config = zyris::runtime::RunConfig::from_env();
    run_config.url = cfg.node.server_url.clone();
    run_config.node_name = cfg.node.name.clone();
    run_config.kind = NodeKind::Service;

    // The instance holding `PtyTerminal` is built exactly once, here. Build it again and every
    // open PTY session vanishes.
    let terminal =
        GatedTerminal::new(PathGate::new(roots.clone(), deny.clone()), cfg.terminal.clone());
    let files = GatedFileIo::new(PathGate::new(roots, deny));

    let runner = Runner::new(run_config, credentials.clone())
        .capability(TerminalServer(terminal))
        .capability(FileIoServer(files));

    // Grab this before the spawn. `capabilities()` takes `&self` and `Capabilities` is Clone (Arc
    // inside), so moving it into the desktop watch task later still points at the same set.
    let caps = runner.capabilities();

    let slot = ConnSlot::new();
    let hook_slot = slot.clone();
    let runner = runner.on_connect({
        // If the slot fill lives in the returned future, a signal can land before it is scheduled.
        // Fill it synchronously in the closure **body** and hand back an empty future.
        let caps = caps.clone();
        let name = cfg.node.name.clone();
        move |conn| {
            hook_slot.put(conn);
            publish_state(&name, true, &caps);
            std::future::ready(())
        }
    });

    tracing::info!(node = %cfg.node.name, url = %cfg.node.server_url, "Starting zyrisd");
    publish_state(&cfg.node.name, false, &caps);
    let running = tokio::spawn(runner.try_run());

    let outcome = tokio::select! {
        joined = running => match joined {
            Ok(Ok(())) => Exit::Ok,
            Ok(Err(e)) => {
                tracing::error!(error = %e, "zyrisd stopped");
                match e {
                    RunError::Credentials(CredentialsError::NeedsOperator(_)) => Exit::NeedsOperator,
                    RunError::Build(_) => Exit::NeedsOperator,
                    RunError::Refused(_) => classify_refused(credentials.gave_up()),
                    _ => Exit::Retry,
                }
            }
            Err(e) => {
                tracing::error!(error = %e, "Node task panicked");
                Exit::Retry
            }
        },
        _ = terminate() => {
            tracing::info!("Received shutdown signal");
            slot.close();
            tokio::time::sleep(CLOSE_GRACE).await;
            Exit::Ok
        }
    };

    if outcome == Exit::NeedsOperator && cfg.notify.enabled {
        crate::notify::needs_attention(
            "zyrisd stopped",
            "This machine lost its Attacca connection. Run `zyrisd enroll` in a terminal.",
        );
    }
    outcome
}

/// Catches both SIGTERM and SIGINT.
///
/// The upstream `Runner` waits only on `ctrl_c()` (= SIGINT), and only while a connection is up.
/// systemd stops us with SIGTERM, so left alone we die without a close frame, and a signal that
/// arrives while dialing or backing off goes unheard, even if it is SIGINT.
///
/// On SIGINT both this handler and the runner's `ctrl_c` fire (tokio broadcasts to every
/// listener), but both paths converge on exit 0 and calling `close()` twice is harmless, so
/// it is safe.
async fn terminate() {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
        tokio::select! {
            _ = term.recv() => {}
            _ = int.recv() => {}
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole exit-code table. The point is that `Refused` is 2 only on credential revocation —
    /// `RunError::Refused` folds in every WireError with retriable=false, so a 500 during a deploy
    /// lands there too. Map them all to 2 and every daemon parks forever at that moment.
    #[test]
    fn the_exit_code_table_holds() {
        assert_eq!(Exit::Ok.code(), 0);
        assert_eq!(Exit::Retry.code(), 1);
        assert_eq!(Exit::NeedsOperator.code(), 2);

        assert_eq!(classify_refused(true), Exit::NeedsOperator);
        assert_eq!(classify_refused(false), Exit::Retry);
    }

    /// Calling close on a closed connection must be harmless. on_connect only overwrites the slot
    /// and never clears it on a drop, so when the signal lands the slot may hold a dead connection.
    #[tokio::test]
    async fn closing_an_already_dead_connection_is_harmless() {
        let dialer = zyris::Node::builder().name("d").kind(NodeKind::Cli).build().unwrap();
        let acceptor = zyris::Node::builder().name("a").kind(NodeKind::Service).build().unwrap();
        let (client, server) = zyris::testing::duplex(&dialer, &acceptor).await.unwrap();
        server.close("bye");
        tokio::time::sleep(Duration::from_millis(50)).await;

        let slot = ConnSlot::new();
        slot.put(client);
        slot.close();
        slot.close();
    }

    /// Closing an empty slot must not panic — that is a signal arriving before we ever connect.
    #[test]
    fn closing_an_empty_slot_is_harmless() {
        ConnSlot::new().close();
    }
}
