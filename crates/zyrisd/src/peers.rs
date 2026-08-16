//! `zyrisd peers` / `pin` / `unpin` — the operator's half of the pin.
//!
//! The daemon deliberately refuses to decide this for itself. It runs with nobody in front of it,
//! so `zyris-p2p`'s `DenyUnknown` is what it uses to confirm an unknown peer, and that answer is
//! always no. `transfer.rs` says as much: sending from this machine needs an operator to have
//! pinned the far side first.
//!
//! **That operator had no tool.** The only way to create a pin was to write `peers.json` by hand —
//! which is what I did on a live machine to get a transfer moving, and it is the wrong shape for
//! this file. Hand-editing is how a fingerprint stops being compared and starts being pasted.
//!
//! # What the confirmation is actually for
//!
//! `pin` asks Attacca which endpoint id a slug belongs to, then shows the fingerprint and waits for
//! a yes. Asking the server first is not a contradiction of the pin's whole reason for existing —
//! the server *proposes* and the human *verifies*. `ZPeerEntry::slug` is a name a server issues and
//! can re-issue, so it anchors nothing; the fingerprint printed here has to be compared against the
//! one the other machine printed when it started, on a path that is not this one. Say yes without
//! doing that and the pin records whatever the server said, which is the state that was there
//! before any of this.

use std::process::ExitCode;
use std::time::Duration;

use zyris::{Node, NodeKind};
use zyris_attacca::{AttaccaApi, AttaccaApiClient, ZPeerEntry};
use zyris_p2p::fingerprint::fingerprint;
use zyris_p2p::tofu::{TofuError, TofuStore};
use zyrisd_node::{config, credentials, state};

/// The server announces `attacca_api` right after the handshake.
const CAPABILITY_WAIT: Duration = Duration::from_secs(5);

fn tofu() -> TofuStore {
    TofuStore::new(config::pins_path())
}

/// A short-lived connection, used and dropped.
///
/// The `Connection` is handed back with the client and has to outlive it: dropping it closes the
/// socket the client speaks over, and a returned-but-orphaned client is a call that hangs.
async fn connect() -> anyhow::Result<(AttaccaApiClient, zyris::Connection)> {
    let cfg = config::load()?;
    let store = credentials::file_store();
    if store.load().await?.is_none() {
        anyhow::bail!("this machine is not enrolled — run `zyrisd enroll` first");
    }
    // Through `StoredOnly` rather than reading the token out of the store, because a stored
    // credential may have expired while the daemon was down and this is the only path that
    // refreshes one without falling through to starting an enrollment.
    let creds = credentials::StoredOnly::new(&cfg.node.server_url, cfg.node.name.clone(), store)?;
    let bearer = zyris::runtime::Credentials::bearer(&creds).await?;

    let node = Node::builder().name("zyrisd-cli").kind(NodeKind::Cli).build()?;
    let conn = node.connect(&cfg.node.server_url, &bearer).await?;
    let api: AttaccaApiClient = conn.wait_capability(CAPABILITY_WAIT).await?;
    Ok((api, conn))
}

/// This machine's own endpoint id, from the running daemon's state file.
///
/// Read rather than derived: deriving it means opening the key file, and the function that knows
/// that format creates a key when there is none — not a side effect a listing should have. `None`
/// when the daemon is not running or transfer is off, which is the truthful answer: there is no
/// fingerprint for anyone to compare then.
fn own_endpoint_id() -> Option<String> {
    state::read().and_then(|s| s.endpoint_id)
}

fn show(endpoint_id: &str) -> String {
    fingerprint(endpoint_id).unwrap_or_else(|_| endpoint_id.to_string())
}

pub async fn peers() -> ExitCode {
    let (api, _conn) = match connect().await {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("Cannot reach Attacca: {e}");
            return ExitCode::from(2);
        }
    };
    let entries = match api.peer_list().await {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("Cannot list peers: {e}");
            return ExitCode::from(2);
        }
    };
    let pins = match tofu().pins().await {
        Ok(pins) => pins,
        Err(e) => {
            eprintln!("Cannot read {}: {e}", config::pins_path().display());
            return ExitCode::from(2);
        }
    };

    let mine = own_endpoint_id();
    if let Some(id) = &mine {
        println!("this machine  {}\n", show(id));
    }

    if entries.is_empty() {
        println!("No other nodes on this account.");
        return ExitCode::SUCCESS;
    }

    for entry in &entries {
        let live = if entry.online { "online" } else { "offline" };
        println!(
            "{:<24} {}  {:<8} {}",
            entry.slug,
            show(&entry.endpoint_id),
            live,
            mark(entry, &pins, mine.as_deref())
        );
    }

    // A pin whose slug is gone from the account is invisible in the loop above, and it is the one a
    // reader most needs to be told about: nothing will ever match it again, and it is still there.
    for (slug, id) in &pins {
        if !entries.iter().any(|e| &e.slug == slug) {
            println!("{slug:<24} {}  {:<8} pinned, but no such node on this account", show(id), "-");
        }
    }
    ExitCode::SUCCESS
}

/// What to say about one node's pin state.
///
/// Pulled out of the loop because it is the only judgment on this screen, and the third arm is the
/// one that matters: a slug pinned to a key the account no longer reports. That is either a peer
/// that was reinstalled or a peer that was substituted, and nothing here can tell those apart —
/// which is exactly why it is shouted rather than quietly showing the new key as if the pin still
/// meant something.
///
/// The `this machine` arm comes after the pinned ones on purpose. A machine cannot confirm itself,
/// so it will not normally be pinned — but if a ledger says otherwise, saying "this machine" and
/// hiding that is the wrong way round.
fn mark(entry: &ZPeerEntry, pins: &[(String, String)], mine: Option<&str>) -> &'static str {
    match pins.iter().find(|(slug, _)| slug == &entry.slug) {
        Some((_, id)) if id == &entry.endpoint_id => "pinned",
        Some(_) => "PINNED TO A DIFFERENT KEY",
        None if mine == Some(entry.endpoint_id.as_str()) => "this machine",
        None => "not pinned",
    }
}

pub async fn pin(slug: &str) -> ExitCode {
    let (api, _conn) = match connect().await {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("Cannot reach Attacca: {e}");
            return ExitCode::from(2);
        }
    };
    let entries: Vec<ZPeerEntry> = match api.peer_list().await {
        Ok(entries) => entries,
        Err(e) => {
            eprintln!("Cannot list peers: {e}");
            return ExitCode::from(2);
        }
    };

    // Not `peer_lookup`: that refuses an ambiguous slug outright, which is right for a transfer and
    // unhelpful here — the person is standing at a terminal and can be shown what the collision is.
    let matches: Vec<&ZPeerEntry> = entries.iter().filter(|e| e.slug == slug).collect();
    let entry = match matches.as_slice() {
        [] => {
            eprintln!("No node on this account is called {slug}. `zyrisd peers` lists them.");
            return ExitCode::from(2);
        }
        [one] => *one,
        many => {
            eprintln!("{} nodes on this account are called {slug}:", many.len());
            for e in many {
                eprintln!("  {}  {}", show(&e.endpoint_id), e.node_id);
            }
            eprintln!("Rename one of them; a pin has to name exactly one machine.");
            return ExitCode::from(2);
        }
    };

    if own_endpoint_id().as_deref() == Some(entry.endpoint_id.as_str()) {
        eprintln!("{slug} is this machine. There is nothing to confirm about itself.");
        return ExitCode::from(2);
    }

    match tofu().authorize(&AskHere, slug, &entry.endpoint_id).await {
        Ok(()) => {
            println!("Pinned {slug}.");
            ExitCode::SUCCESS
        }
        Err(TofuError::Refused { .. }) => {
            println!("Not pinned.");
            ExitCode::from(1)
        }
        // The one this exists to make loud. A slug that is already pinned to a different key is
        // either a reinstalled peer or a substituted one, and nothing here can tell the difference
        // — so it refuses and names the operation that would accept the change deliberately.
        Err(TofuError::Changed { pinned, offered }) => {
            eprintln!("{slug} is already pinned to a different key.");
            eprintln!("  pinned   {}", show(&pinned));
            eprintln!("  offered  {}", show(&offered));
            eprintln!();
            eprintln!("If that machine was reinstalled and you have checked the new fingerprint");
            eprintln!("some other way, `zyrisd unpin {slug}` and pin it again. If you have not,");
            eprintln!("this is what a substituted peer looks like.");
            ExitCode::from(2)
        }
        Err(e) => {
            eprintln!("Could not pin {slug}: {e}");
            ExitCode::from(2)
        }
    }
}

pub async fn unpin(slug: &str) -> ExitCode {
    // No network: forgetting is about this machine's own ledger, and it has to work when Attacca is
    // unreachable — a peer you no longer trust is not a good reason to need the server's agreement.
    match tofu().forget(slug).await {
        Ok(true) => {
            println!("Forgot {slug}. The next key offered under that name will be pinned as new.");
            ExitCode::SUCCESS
        }
        Ok(false) => {
            println!("{slug} was not pinned.");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("Could not update {}: {e}", config::pins_path().display());
            ExitCode::from(2)
        }
    }
}

/// Asks the person running the command, on the terminal they ran it from.
///
/// The same reasoning as `zyris-hello`'s confirmer, and the same refusal with no terminal: an
/// unknown peer must not be trusted merely because nobody was around to refuse it. That case is
/// less theoretical here than it looks — this is a command, and a command ends up in a script.
struct AskHere;

#[async_trait::async_trait]
impl zyris_p2p::fingerprint::PeerConfirmer for AskHere {
    async fn confirm(&self, label: &str, endpoint_id: &str) -> bool {
        use std::io::{BufRead, IsTerminal, Write};

        if !std::io::stdin().is_terminal() {
            eprintln!("Not a terminal, so there is nobody to confirm {label}'s fingerprint. Refusing.");
            return false;
        }
        println!("\n{label}");
        println!("    {}", show(endpoint_id));
        println!("\nThat machine printed the same fingerprint when it started, and `zyrisd status`");
        println!("shows it. Compare them somewhere that is not this connection.");
        print!("Is it the same? [y/N] ");
        let _ = std::io::stdout().flush();

        let mut line = String::new();
        if std::io::stdin().lock().read_line(&mut line).is_err() {
            return false;
        }
        matches!(line.trim(), "y" | "Y" | "yes")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(slug: &str, endpoint_id: &str) -> ZPeerEntry {
        ZPeerEntry {
            node_id: format!("node-{slug}"),
            slug: slug.to_string(),
            endpoint_id: endpoint_id.to_string(),
            online: true,
        }
    }

    fn pin(slug: &str, endpoint_id: &str) -> (String, String) {
        (slug.to_string(), endpoint_id.to_string())
    }

    #[test]
    fn a_matching_pin_reads_as_pinned() {
        let e = entry("laptop", "aaaa");
        assert_eq!(mark(&e, &[pin("laptop", "aaaa")], None), "pinned");
    }

    #[test]
    fn a_node_with_no_pin_reads_as_not_pinned() {
        let e = entry("laptop", "aaaa");
        assert_eq!(mark(&e, &[], None), "not pinned");
    }

    /// The line this screen exists for. A pin that no longer matches the key the account reports
    /// must not be shown as a pin, and must not be shown as absent either.
    #[test]
    fn a_pin_on_a_different_key_is_shouted() {
        let e = entry("laptop", "aaaa");
        assert_eq!(mark(&e, &[pin("laptop", "bbbb")], None), "PINNED TO A DIFFERENT KEY");
    }

    #[test]
    fn this_machine_is_named_rather_than_reported_unpinned() {
        let e = entry("desktop", "cccc");
        assert_eq!(mark(&e, &[], Some("cccc")), "this machine");
    }

    /// Matching is by key, not by name. Two nodes can share a slug — the gateway says so — and a
    /// pin that matched on the name alone would call one of them pinned on the other's evidence.
    #[test]
    fn another_nodes_pin_does_not_vouch_for_this_one() {
        let e = entry("laptop", "aaaa");
        assert_eq!(mark(&e, &[pin("phone", "aaaa")], None), "not pinned");
    }

    /// A ledger that somehow pins this machine is still reported as a mismatch when the key
    /// differs — "this machine" must not swallow a disagreement.
    #[test]
    fn being_this_machine_does_not_hide_a_mismatched_pin() {
        let e = entry("desktop", "cccc");
        assert_eq!(mark(&e, &[pin("desktop", "dddd")], Some("cccc")), "PINNED TO A DIFFERENT KEY");
    }
}
