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

use zyris::{Connection, ErrorCode};
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
        key_path: PathBuf,
    ) -> anyhow::Result<Transfer> {
        for dir in [&cfg.inbox, &cfg.undo] {
            std::fs::create_dir_all(dir)?;
        }

        // Without this the endpoint generates a fresh key every start, and `key.rs` spells out
        // what that costs: a peer that pinned us sees a stranger and refuses us. It is the quiet
        // kind of broken — receiving keeps working, because the accept loop admits any live node
        // of this account without consulting a pin, so nothing complains until the day someone
        // tries to *send* to a machine that pinned this one.
        let secret = zyris_p2p::key::load_or_create(&key_path).await?;

        let builder = iroh::Endpoint::builder(iroh::endpoint::presets::N0)
            .secret_key(secret)
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

    /// What a peer pins, and what the printed fingerprint is derived from.
    pub fn endpoint_id(&self) -> &str {
        &self.endpoint_id
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
                // A credential granted before this daemon asked for `peers:write` still works for
                // everything else, so the node comes up looking healthy and only this one call is
                // refused. Say what to do about it rather than printing the code and leaving the
                // operator to work out that a scope is granted with the credential and cannot be
                // added to one that already exists.
                Err(e) if e.code == ErrorCode::ForbiddenScope => tracing::error!(
                    "This node's credential predates the peers:write scope, so no peer can find \
                     it and file transfer cannot start. Run `zyrisd enroll` again to replace it."
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

#[cfg(test)]
mod tests {
    use super::*;

    fn config(dir: &std::path::Path) -> TransferConfig {
        TransferConfig {
            enabled: true,
            inbox: dir.join("inbox"),
            undo: dir.join("undo"),
            relay_url: String::new(),
        }
    }

    async fn bind_at(dir: &std::path::Path, key: &std::path::Path) -> Transfer {
        Transfer::bind(&config(dir), dir.to_path_buf(), dir.join("peers.json"), key.to_path_buf())
            .await
            .unwrap()
    }

    /// A restart must not change who this machine is.
    ///
    /// The fingerprint a human reads out and compares is a fingerprint of this key. If binding
    /// generated a new one each start — which is what iroh does when handed no key — then every
    /// pin made against this node would stop matching the next time the service bounced, and the
    /// comparison the pin is built on would have been for nothing.
    #[tokio::test]
    async fn the_endpoint_id_survives_a_restart() {
        let dir = tempfile::tempdir().unwrap();
        let key = dir.path().join("peer_key");

        let first = bind_at(dir.path(), &key).await;
        let before = first.endpoint_id().to_string();
        first.endpoint.close().await;

        let second = bind_at(dir.path(), &key).await;
        assert_eq!(
            before,
            second.endpoint_id(),
            "the node came back as a stranger, so every pin against it is now dead"
        );
        second.endpoint.close().await;
    }

    /// The other half of the claim: the id is not a constant, so the test above is comparing
    /// something that could have differed.
    #[tokio::test]
    async fn a_different_key_is_a_different_node() {
        let dir = tempfile::tempdir().unwrap();

        let a = bind_at(dir.path(), &dir.path().join("key-a")).await;
        let b = bind_at(dir.path(), &dir.path().join("key-b")).await;

        assert_ne!(a.endpoint_id(), b.endpoint_id());
        a.endpoint.close().await;
        b.endpoint.close().await;
    }
}
