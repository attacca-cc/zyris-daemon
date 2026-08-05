//! A credential source that never starts an enrollment.
//!
//! Upstream `DeviceGrant` in the daemon path **prints an enroll code to stdout (the
//! journal)** every boot, blocks up to 30 minutes, then dies — via `println!`, not `tracing`,
//! so `RUST_LOG` can't stop it. The daemon uses stored credentials only, else asks for a human.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use zyris::enroll::{
    CredentialStore, CredentialStoreError, EnrollError, Enroller, StoredCredential,
};
use zyris::runtime::{Credentials, CredentialsError};

/// Same as upstream `DeviceGrant`. A slightly off clock won't present a near-expired token.
const SKEW_SECS: i64 = 30;

pub struct StoredOnly {
    store: Arc<dyn CredentialStore>,
    /// We never enroll, but the **only public API that reaches the refresh endpoint without
    /// starting an enrollment** is `Enroller::force_refresh`, so we need an instance. (`obtain()`
    /// also reaches refresh, but falls through to enrollment when the store is empty.)
    enroller: Enroller,
    held: tokio::sync::Mutex<Option<StoredCredential>>,
    /// Tells `main` "this credential is definitely dead".
    ///
    /// `RunError::Refused` collapses every `WireError` with `retriable == false`, so a 500 during
    /// a deploy lands there too. Instead of grepping strings, the place that actually saw the
    /// revoke leaves the mark. That is what separates exit 2 (give up) from 1 (restart).
    gave_up: AtomicBool,
}

/// The store shared by the daemon and `zyrisd enroll`.
pub fn file_store() -> Arc<dyn CredentialStore> {
    Arc::new(zyris::enroll::FileCredentialStore::at(crate::config::credentials_path()))
}

impl StoredOnly {
    pub fn new(
        server_url: &str,
        node_name: String,
        store: Arc<dyn CredentialStore>,
    ) -> Result<StoredOnly, EnrollError> {
        // Scopes stay empty. zyrisd is a pure tool provider with no reason to touch the owner's
        // account, and `Runner::request_scopes` never hits the Enroller on the own-Credentials path.
        let enroller =
            Enroller::new(server_url, node_name, platform().to_string(), Vec::new(), store.clone())?;
        Ok(StoredOnly {
            store,
            enroller,
            held: tokio::sync::Mutex::new(None),
            gave_up: AtomicBool::new(false),
        })
    }

    pub fn gave_up(&self) -> bool {
        self.gave_up.load(Ordering::SeqCst)
    }

    /// Rotates an expired credential. The three outcomes mean different things.
    async fn rotate(
        &self,
        current: &StoredCredential,
    ) -> Result<StoredCredential, CredentialsError> {
        match self.enroller.force_refresh(current).await {
            Ok(Some(rotated)) => Ok(rotated),
            // The server refused. `force_refresh` already cleared the store.
            Ok(None) => {
                self.gave_up.store(true, Ordering::SeqCst);
                Err(CredentialsError::NeedsOperator(
                    "Credentials were revoked. Run zyrisd enroll again".into(),
                ))
            }
            // Couldn't reach the server. The credential may be fine, so never delete it.
            // Collapsing this into NeedsOperator would let one 503 during a deploy park every
            // daemon reconnecting at that moment, permanently.
            Err(EnrollError::Transport(e)) => Err(CredentialsError::Unavailable(e)),
            Err(e) => Err(CredentialsError::NeedsOperator(e.to_string())),
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

fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

/// Splits store errors three ways.
///
/// The upstream file backend marks `UnknownVersion`, `Corrupt`, and **every `io::Error` as
/// discardable**, so transient EACCES/EIO/ESTALE read failures share a bucket with corrupt files.
/// Folding it all into `NeedsOperator` makes a transient condition permanent. Wrongly retrying
/// only costs a backoff loop and some log lines — so only the certain cases are singled out.
fn store_error(e: CredentialStoreError) -> CredentialsError {
    let text = e.to_string();
    if !e.is_discardable() {
        // Permissive (permissions are wide open) or NoConfigDir. A human has to fix it.
        return CredentialsError::NeedsOperator(text);
    }
    // These two are the only ones retrying will never fix.
    if text.contains("corrupt") || text.contains("newer version") {
        return CredentialsError::NeedsOperator(format!("{text} — run zyrisd enroll again"));
    }
    CredentialsError::Unavailable(text)
}

#[zyris::async_trait]
impl Credentials for StoredOnly {
    async fn bearer(&self) -> Result<String, CredentialsError> {
        let mut held = self.held.lock().await;
        if let Some(token) = held.as_ref().and_then(|c| c.bearer(now_unix(), SKEW_SECS)) {
            return Ok(token.to_string());
        }

        let stored = self.store.load().await.map_err(store_error)?;
        let Some(stored) = stored else {
            return Err(CredentialsError::NeedsOperator(
                "Not enrolled. Run zyrisd enroll".into(),
            ));
        };

        let fresh = match stored.bearer(now_unix(), SKEW_SECS) {
            Some(_) => stored,
            None => self.rotate(&stored).await?,
        };
        let token = fresh
            .bearer(now_unix(), SKEW_SECS)
            .ok_or_else(|| {
                CredentialsError::NeedsOperator(
                    "Just-issued credential is already expired. Check this machine's clock".into(),
                )
            })?
            .to_string();
        *held = Some(fresh);
        Ok(token)
    }

    /// The runner calls this once after it gets a 401.
    ///
    /// **Must call `force_refresh`.** Dropping only the in-memory copy leaves the store intact, so
    /// the next `bearer()` hands back the same dead token, and since the runner already rotated
    /// once it folds the second 401 straight into `Refused` — store never cleared, no re-enroll
    /// prompt. Clock drift (resume from sleep) and redeploys that invalidate only access tokens
    /// — everything the refresh token could heal on its own — is decided right here.
    async fn refresh(&self) -> Result<bool, CredentialsError> {
        let mut held = self.held.lock().await;
        let Some(current) = held.take() else { return Ok(false) };
        match self.rotate(&current).await {
            Ok(rotated) => {
                *held = Some(rotated);
                Ok(true)
            }
            // Definitely dead. false tells the runner "give up", and that is right.
            Err(CredentialsError::NeedsOperator(_)) if self.gave_up() => Ok(false),
            Err(e) => Err(e),
        }
    }

    fn describe(&self) -> String {
        format!("stored credentials ({})", self.enroller.store_description())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use zyris::enroll::MemoryCredentialStore;

    /// 127.0.0.1:1 refuses instantly — no waiting on the network.
    fn creds(store: Arc<dyn CredentialStore>) -> StoredOnly {
        StoredOnly::new("ws://127.0.0.1:1/zyris/v1/ws", "test".into(), store).unwrap()
    }

    fn stored(expires_at: i64) -> StoredCredential {
        StoredCredential::new(
            "zna_access".into(),
            "znr_refresh".into(),
            "node-id".into(),
            "test".into(),
            "x@example.com".into(),
            expires_at,
        )
    }

    /// An empty store needs a human. The daemon never starts an enrollment —
    /// that would print an enroll code to the journal every boot, block 30 minutes, then die.
    #[tokio::test]
    async fn an_empty_store_asks_for_a_human_rather_than_enrolling() {
        let c = creds(Arc::new(MemoryCredentialStore::new()));
        let err = c.bearer().await.unwrap_err();
        assert!(matches!(err, CredentialsError::NeedsOperator(_)), "{err:?}");
        assert!(err.to_string().contains("zyrisd enroll"), "{err}");
        assert!(!c.gave_up(), "not enrolled is not the same as 'the credential is dead'");
    }

    /// A still-valid access token is used as is. No rotation attempt.
    #[tokio::test]
    async fn a_live_access_token_is_used_as_is() {
        let store = Arc::new(MemoryCredentialStore::new());
        store.save(&stored(now_unix() + 3600)).await.unwrap();
        let c = creds(store);
        assert_eq!(c.bearer().await.unwrap(), "zna_access");
    }

    /// Nothing held means nothing to rotate. The runner reads false as "give up", correctly.
    #[tokio::test]
    async fn refresh_without_anything_held_has_nothing_to_rotate() {
        let c = creds(Arc::new(MemoryCredentialStore::new()));
        assert!(!c.refresh().await.unwrap());
    }

    /// An unreachable server is not a dead credential. It has to be Unavailable so the runner
    /// retries with backoff — as NeedsOperator, one 503 in a deploy parks every daemon forever.
    #[tokio::test]
    async fn an_unreachable_server_is_retriable_not_fatal() {
        let store = Arc::new(MemoryCredentialStore::new());
        store.save(&stored(now_unix() - 10)).await.unwrap(); // expired → rotation attempt
        let c = creds(store.clone());
        let err = c.bearer().await.unwrap_err();
        assert!(matches!(err, CredentialsError::Unavailable(_)), "{err:?}");
        assert!(!c.gave_up());
        // We only failed to reach it, so the credential must still be there.
        assert!(store.load().await.unwrap().is_some(), "unreachable is no reason to delete");
    }

    /// A world-readable file needs a human, not a retry.
    #[test]
    fn a_world_readable_file_needs_a_human_not_a_retry() {
        let e = CredentialStoreError::Refused("credential file is readable by other users".into());
        assert!(matches!(store_error(e), CredentialsError::NeedsOperator(_)));
    }

    /// A corrupt file doesn't heal by retrying either.
    #[test]
    fn a_corrupt_file_needs_a_human() {
        let e = CredentialStoreError::Unusable("credential file is corrupt: expected value".into());
        assert!(matches!(store_error(e), CredentialsError::NeedsOperator(_)));
    }

    /// But a plain read failure must be retried. Upstream hands back every io::Error as
    /// discardable, so EACCES/EIO/ESTALE share a bucket with corrupt files.
    #[test]
    fn a_transient_read_failure_is_retriable() {
        let e = CredentialStoreError::Unusable("Input/output error (os error 5)".into());
        assert!(matches!(store_error(e), CredentialsError::Unavailable(_)));
    }
}
