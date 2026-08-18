//! Assembles the node, keeps it alive, and dies properly when it dies.

use std::path::PathBuf;
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
use crate::transfer::Transfer;

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
fn publish_state(
    node_name: &str,
    connected: bool,
    caps: &zyris::Capabilities,
    endpoint_id: Option<String>,
) {
    crate::state::write(&crate::state::State {
        node_name: node_name.to_string(),
        connected,
        capabilities: caps.descriptors().iter().map(|d| d.name.clone()).collect(),
        endpoint_id,
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

    /// The real connection state that fills `connected` in the state file. The slot is not cleared
    /// on a drop, so it may hold a dead connection — and a dead one is not a connection.
    pub fn is_alive(&self) -> bool {
        self.0.lock().unwrap().as_ref().map(|c| !c.is_closed()).unwrap_or(false)
    }
}

pub async fn run(cfg: Config) -> Exit {
    let (roots, deny) = cfg.resolve_roots();
    if roots.is_empty() {
        // May not be a config error — the mounts may just not be up yet. Retry.
        tracing::error!("None of the configured roots exist. Waiting for mounts");
        return Exit::Retry;
    }

    // Taken before `roots` is moved into the gates below. `send_to` reads out of one directory and
    // upstream takes exactly one, so it gets the same first root the PTY uses as its cwd.
    let send_root = roots.first().cloned().unwrap_or_else(|| PathBuf::from("."));

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

    let mut runner = Runner::new(run_config, credentials.clone())
        .capability(TerminalServer(terminal))
        .capability(FileIoServer(files));

    // Windows serves the desktop from this process; Linux spawns the child and the watcher below
    // attaches it. Announced before the runner starts, because a node says what it can do before
    // it has anywhere to say it — and unlike the child, this set never changes afterwards.
    #[cfg(windows)]
    {
        runner = crate::display::with_desktop(runner, &cfg.desktop);
    }

    // Bound before the runner starts, because a node announces what it can do before it has
    // anywhere to say it. The rendezvous client and the accept loop both arrive on the connection
    // that comes later — see `transfer::Transfer::on_connect`.
    //
    // A failure to bind is not a reason to stop being a node. The socket may be unavailable for
    // reasons that have nothing to do with the terminal and files this daemon is mostly here for,
    // so it is logged and the rest comes up regardless.
    let transfer = if cfg.transfer.enabled {
        let pins = crate::config::pins_path();
        let key_path = crate::config::peer_key_path();
        match Transfer::bind(&cfg.transfer, send_root, pins, key_path).await {
            Ok(t) => {
                let t = Arc::new(t);
                runner = runner.capability(t.capability());
                Some(t)
            }
            Err(e) => {
                tracing::error!(error = %e, "Could not start file transfer. Continuing without it");
                None
            }
        }
    } else {
        None
    };

    // `None` when transfer is off or failed to bind, which is the honest answer: there is no
    // fingerprint for anyone to compare against until an endpoint exists. Fixed for the life of the
    // process — the key is loaded from disk once, so this does not change under a reconnect.
    let endpoint_id = transfer.as_ref().map(|t| t.endpoint_id().to_string());

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
        let transfer = transfer.clone();
        let endpoint_id = endpoint_id.clone();
        move |conn: Connection| {
            // If the slot fill lived in the returned future, a signal could land before it is
            // scheduled. It stays here, in the body; the future only holds work that must await.
            hook_slot.put(conn.clone());
            publish_state(&name, true, &caps, endpoint_id.clone());
            let transfer = transfer.clone();
            async move {
                if let Some(transfer) = transfer {
                    transfer.on_connect(&conn).await;
                }
            }
        }
    });

    tracing::info!(node = %cfg.node.name, url = %cfg.node.server_url, "Starting zyrisd");
    publish_state(&cfg.node.name, false, &caps, endpoint_id.clone());

    // Third branch: watches the desktop child. Rewrites the state file as the announce set changes.
    let watcher_slot = slot.clone();
    let watcher = tokio::spawn({
        let caps = caps.clone();
        let desktop = cfg.desktop.clone();
        let name = cfg.node.name.clone();
        let endpoint_id = endpoint_id.clone();
        let for_change = caps.clone();
        async move {
            crate::display::watch(caps, desktop, move || {
                // Child up or down changes the announce set. Write the connection state too —
                // hardcoding `true` lies "connected" even while backoff has us disconnected.
                publish_state(&name, watcher_slot.is_alive(), &for_change, endpoint_id.clone());
            })
            .await
        }
    });

    // Nothing refreshes the connection state on a drop (upstream Runner has no disconnect hook).
    // on_connect only overwrites the slot and never clears a dead connection, so after a silent
    // drop, `zyrisd status` shows "connection" as "connected" forever. Rewrite the real state
    // periodically.
    //
    // Every tick, not only when the value changed. The timestamp is the other half of what this
    // file says: `State::is_recent` is how a reader tells a daemon that is connected from one that
    // was connected when it died, and skipping the write on an unchanged value leaves a healthy
    // daemon's file ageing until it looks like a corpse.
    let refresher = {
        let caps = caps.clone();
        let name = cfg.node.name.clone();
        let slot = slot.clone();
        async move {
            let mut tick = tokio::time::interval(Duration::from_secs(30));
            tick.tick().await; // Already wrote once above at startup. Skip the immediate tick.
            loop {
                tick.tick().await;
                publish_state(&name, slot.is_alive(), &caps, endpoint_id.clone());
            }
        }
    };
    let refresher = tokio::spawn(refresher);

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

    // Stop the watch and refresh tasks. The child exits itself once the parent's stdin closes, so
    // nothing is orphaned; and if something is, the unit's cgroup reaps it within TimeoutStopSec.
    watcher.abort();
    refresher.abort();

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

    /// A dead connection is not a connection. The value behind `connected` must not lie.
    #[tokio::test]
    async fn a_closed_connection_is_not_alive() {
        let dialer = zyris::Node::builder().name("d").kind(NodeKind::Cli).build().unwrap();
        let acceptor = zyris::Node::builder().name("a").kind(NodeKind::Service).build().unwrap();
        let (client, server) = zyris::testing::duplex(&dialer, &acceptor).await.unwrap();

        let slot = ConnSlot::new();
        assert!(!slot.is_alive(), "counts as dead before any connection");
        slot.put(client);
        assert!(slot.is_alive());
        server.close("bye");
        // Give the close frame time to land and shut it down.
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(!slot.is_alive(), "a closed connection must not report alive");
    }
}
