//! Node-to-node file transfer, for a daemon with nobody sitting at it.
//!
//! Three things have to happen for a transfer to be possible at all, and the middle one is the
//! easiest to leave out: announce `file_transfer` so an agent has a tool, **publish this node's
//! peer endpoint** so another machine can look it up, and accept the connections that result.
//! Without the second, `peer_lookup` has nothing to answer with and no transfer can start — not
//! even between two machines on the same desk.
//!
//! # The part that differs from `zyris-hello`
//!
//! The reference node asks its terminal to confirm an unknown peer's fingerprint. This one has no
//! terminal: it comes up at boot, under a service manager, with nobody to ask. `zyris-p2p` ships
//! [`DenyUnknown`] for exactly that and says why — the choice left is which way to fail, and an
//! unknown peer must not be trusted merely because nobody was around to refuse it.
//!
//! That lands differently on the two directions, which is worth being plain about:
//!
//! | Direction | Works out of the box? |
//! |---|---|
//! | **Receiving** | Yes. The accept loop checks the ledger and never writes to it, so an unpinned peer is admitted on the strength of being a live node of this account |
//! | **Sending** | Only to a peer already pinned. `DenyUnknown` refuses the rest, and there is nobody here to change that answer |
//!
//! So a machine running this daemon is a place files can be *sent to* immediately, and sending
//! *from* it needs an operator to have pinned the far side first. That is the honest arrangement
//! for an unattended box, and it is the one the pin is for: the anchor is a person who compared a
//! fingerprint, and on this machine that person is not present at 3am.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use zyris::Connection;
use zyris_attacca::{AttaccaApi, AttaccaApiClient};
use zyris_caps::FileTransferServer;
use zyris_capkit::transfer::listen::serve_peers;
use zyris_capkit::transfer::{
    FileTransferConfig, IrohPeerLink, LocalFileTransfer, TransferConfig as PeerTransferConfig,
};
use zyris_p2p::fingerprint::{fingerprint, DenyUnknown};
use zyris_p2p::iroh;
use zyris_p2p::tofu::TofuStore;

use crate::config::TransferConfig;

/// How long to wait for the server to announce `attacca_api` on a fresh connection.
const CONSUME_WAIT: std::time::Duration = std::time::Duration::from_secs(5);

/// The transfer half of the node, held across reconnects.
pub struct Transfer {
    transfer: LocalFileTransfer,
    endpoint: iroh::Endpoint,
    peer_config: PeerTransferConfig,
    tofu: TofuStore,
    endpoint_id: String,
    /// `on_connect` runs again on every reconnect. Publishing again is right — addresses move
    /// when a laptop changes network. Starting a second accept loop on one endpoint is not.
    listening: AtomicBool,
}

impl Transfer {
    /// Binds the peer endpoint and builds the capability, with no rendezvous client yet.
    ///
    /// `send_root` is what `send_to` may read out of, and it is `roots[0]` for the same reason the
    /// PTY's cwd is: upstream takes exactly one, and the first root is the one this daemon treats
    /// as its base. A file the agent could already read through `file_io` is a file it can send.
    pub async fn bind(
        cfg: &TransferConfig,
        send_root: PathBuf,
        pins: PathBuf,
    ) -> anyhow::Result<Transfer> {
        for dir in [&cfg.inbox, &cfg.undo] {
            std::fs::create_dir_all(dir)?;
        }

        let builder = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
            .alpns(vec![zyris_p2p::transport::ALPN.to_vec()]);
        let builder = match cfg.relay_url.trim() {
            "" => {
                tracing::info!("No transfer.relay_url set. Falling back on the public relays");
                builder
            }
            url => {
                let parsed: iroh::RelayUrl = url.parse()?;
                tracing::info!(relay = %parsed, "Using this deployment's own relay");
                builder.relay_mode(iroh::RelayMode::Custom(parsed.into()))
            }
        };
        let endpoint = builder.bind().await?;
        let endpoint_id = endpoint.id().to_string();
        tracing::info!(
            endpoint_id = %endpoint_id,
            fingerprint = %fingerprint(&endpoint_id).unwrap_or_default(),
            "Peer endpoint bound"
        );

        let tofu = TofuStore::new(pins);
        let peer_config = PeerTransferConfig {
            inbox: cfg.inbox.clone(),
            undo: cfg.undo.clone(),
            ..PeerTransferConfig::default()
        };
        let transfer = LocalFileTransfer::pending(
            FileTransferConfig {
                root: send_root,
                inbox: cfg.inbox.clone(),
                node_id: endpoint_id.clone(),
                ..FileTransferConfig::default()
            },
            tofu.clone(),
            // Nobody to ask. See the module docs for what this costs and what it does not.
            Arc::new(DenyUnknown),
            Arc::new(IrohPeerLink::new(endpoint.clone())),
        );

        Ok(Transfer {
            transfer,
            endpoint,
            peer_config,
            tofu,
            endpoint_id,
            listening: AtomicBool::new(false),
        })
    }

    pub fn capability(&self) -> FileTransferServer<LocalFileTransfer> {
        FileTransferServer(self.transfer.clone())
    }

    /// Runs on every connect: hands the transfer its rendezvous client, republishes where this
    /// node can be reached, and starts the accept loop the first time.
    pub async fn on_connect(&self, conn: &Connection) {
        let Ok(api) = conn.wait_capability::<AttaccaApiClient>(CONSUME_WAIT).await else {
            tracing::warn!("No attacca_api on this connection. File transfer stays offline");
            return;
        };
        self.transfer.set_api(api);

        // An empty address list is still worth publishing: the endpoint id alone is dialable
        // through a relay, which is the case this daemon is most likely to be in.
        let addrs: Vec<String> = self.endpoint.addr().ip_addrs().map(|a| a.to_string()).collect();
        match conn.wait_capability::<AttaccaApiClient>(CONSUME_WAIT).await {
            Ok(api) => match api.peer_publish(self.endpoint_id.clone(), addrs.clone()).await {
                Ok(()) => tracing::info!(
                    endpoint_id = %self.endpoint_id,
                    addrs = addrs.len(),
                    "Published this node's peer address"
                ),
                Err(e) => tracing::warn!(error = %e, "Could not publish. Peers cannot find us"),
            },
            Err(e) => tracing::warn!(error = %e, "attacca_api went away before publishing"),
        }

        if self.listening.swap(true, Ordering::SeqCst) {
            return;
        }
        let Ok(directory) = conn.wait_capability::<AttaccaApiClient>(CONSUME_WAIT).await else {
            self.listening.store(false, Ordering::SeqCst);
            return;
        };
        let endpoint = self.endpoint.clone();
        let peer_config = self.peer_config.clone();
        let tofu = self.tofu.clone();
        let endpoint_id = self.endpoint_id.clone();
        tokio::spawn(async move {
            tracing::info!("Accepting peer connections");
            serve_peers(endpoint, Arc::new(directory), peer_config, tofu, endpoint_id).await;
            tracing::warn!("The peer accept loop ended");
        });
    }
}
