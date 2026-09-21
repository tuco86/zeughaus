//! The runtime's weida transport: one bound listener, one announced URL.
//!
//! Everything this process says to an editor leaves through here. Frames go out
//! on `/samples`, and the endpoints the later steps add (events, snapshots,
//! triggers) are registered on the same listener, so an editor learns one
//! address and derives every path from it.
//!
//! Trust is fingerprint pinning in both directions. The server identity is
//! loaded from the runner's state directory, not minted per start: its
//! fingerprint is what the announced URL carries
//! (`weida://sha256:<fp>@host:port/`), so an editor accepts exactly this
//! runner -- and a fingerprint that survived the restart is also what lets
//! weida redial transparently instead of handing the editor a stale pin.
//! Every dialling peer must present a key the runner trusts
//! ([`zeughaus_link::credentials::client_trust`]); an anonymous editor is
//! refused in the handshake, because a terminal endpoint on this listener
//! hands out a shell and an opaque id is not a credential.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use weida::{
    Binding, EndpointAddr, Fingerprint, Listener, Puller, Replier, Runtime, RuntimeConfig,
    ServerTls, TransferMeta, Trust,
};
use zeughaus_link::{MAX_TRIGGER_BYTES, Snapshot, TriggerRequest, credentials};

/// A bound weida listener, the pinned URL that reaches it, and whom it lets in.
pub struct Transport {
    url: String,
    listener: Listener,
    trusted_clients: Vec<Fingerprint>,
    /// The bound socket. Held for the life of the process: dropping it would
    /// stop accepting, and the transport has no shutdown short of exit.
    _binding: Binding,
    /// The weida runtime owns the connection pool and the QUIC endpoints.
    _runtime: Runtime,
}

impl Transport {
    /// Binds `bind` under the identity stored in `state_dir` and returns the
    /// pinned root URL for it.
    ///
    /// The identity comes off disk so the announced fingerprint outlives a
    /// restart, and `client.pem` is bootstrapped here because the runner is
    /// the process that exists first: an editor on this machine must find a
    /// key that is already trusted rather than one it minted itself.
    ///
    /// Nothing is announced from here: the caller does that once it has a store
    /// connection, and it must not happen before this returns -- an editor
    /// pointed at a runtime that is not yet serving would fail its first
    /// request and have no reason to try again.
    pub async fn start(bind: SocketAddr, state_dir: &Path) -> Result<Transport, String> {
        let host = announced_host(bind);
        let identity = credentials::runner_identity(state_dir)?;
        // Before the identity moves into the binding: the fingerprint is what
        // the URL pins, and it is read off the certificate.
        let fingerprint = identity
            .fingerprint()
            .map_err(|e| format!("cannot fingerprint the runner identity: {e}"))?;
        credentials::client_identity(state_dir)?;
        let trust = credentials::client_trust(state_dir)?;
        refuse_anonymous_exposure(bind, &trust)?;
        let trusted_clients = trust.pins.clone();

        let runtime =
            Runtime::new(RuntimeConfig::default()).map_err(|e| format!("weida runtime: {e}"))?;
        let listener = runtime.listener();
        let binding = listener
            .bind_quic(bind, ServerTls::new(identity).require_client(trust))
            .await
            .map_err(|e| format!("cannot bind {bind}: {e}"))?;

        // Built through `EndpointAddr` rather than `format!` so an IPv6 bind
        // address is bracketed the way the parser on the other side expects.
        let url = EndpointAddr {
            host,
            port: Some(binding.local_addr().port()),
            path: "/".to_owned(),
            peer: Some(fingerprint),
        }
        .to_string();

        Ok(Transport {
            url,
            listener,
            trusted_clients,
            _binding: binding,
            _runtime: runtime,
        })
    }

    /// The pinned `weida://sha256:<fp>@host:port/` address to announce. Every
    /// endpoint path is derived from it.
    pub fn url(&self) -> &str {
        &self.url
    }

    /// The namespace every endpoint of this runtime is registered on.
    pub fn listener(&self) -> &Listener {
        &self.listener
    }

    /// The client keys this binding accepts. Logged at startup: a refused
    /// editor is otherwise a handshake failure with no side saying which keys
    /// were on the list.
    pub fn trusted_clients(&self) -> &[Fingerprint] {
        &self.trusted_clients
    }
}

/// Refuses a reachable bind that would accept anybody.
///
/// The local bootstrap always leaves one pinned client, so an empty trust here
/// means the state directory could not be written or was emptied by hand. On
/// loopback that is still only this machine's users; on any other interface it
/// is the whole network, and this listener carries terminal endpoints -- so it
/// fails closed and says which address made it refuse.
fn refuse_anonymous_exposure(bind: SocketAddr, trust: &Trust) -> Result<(), String> {
    if trust.is_empty() && !bind.ip().is_loopback() {
        return Err(format!(
            "refusing to serve {bind}: no client is trusted, and a non-loopback \
             address must not be served anonymously -- provision a client \
             certificate under <state-dir>/clients/ or bind loopback"
        ));
    }
    Ok(())
}

/// The host an editor will dial, and the name the identity is issued for.
///
/// Taken from the bind address, so `--feed-addr` on a routable interface is
/// announced as that interface. An unspecified address (`0.0.0.0`) names no
/// reachable host, so loopback is announced instead: right for the local case,
/// and a remote viewer needs an explicit `--feed-addr` regardless.
fn announced_host(bind: SocketAddr) -> String {
    match bind.ip() {
        ip if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST).to_string(),
        ip => ip.to_string(),
    }
}

/// What became of a press offered to the event loop.
#[derive(Debug, PartialEq, Eq)]
enum Intake {
    Taken,
    /// The loop has not caught up and the queue is full, so the press is gone.
    Dropped,
    /// The event loop is gone, which for this process means it is shutting
    /// down.
    Gone,
}

/// Hands one press to the event loop without ever waiting for it.
///
/// Never blocking is the whole point: this runs on a weida task that also has
/// to keep reading the connection, and a press is a moment -- so a queue that
/// is full is a reason to drop one, not to hold the transport still. A peer
/// pressing in a loop therefore costs a bounded queue and a log line instead
/// of the memory of every press it ever sent.
fn offer(tx: &SyncSender<u64>, node_id: u64) -> Intake {
    match tx.try_send(node_id) {
        Ok(()) => Intake::Taken,
        Err(TrySendError::Full(_)) => Intake::Dropped,
        Err(TrySendError::Disconnected(_)) => Intake::Gone,
    }
}

/// Takes trigger presses until the puller goes away, which for this process
/// means never: it owns the puller for the life of the program.
///
/// Push/Pull rather than Req/Rep because a press has no answer: the editor
/// learns that it worked by seeing the value change, and waiting for a reply
/// would only add a round trip to a button.
pub async fn accept_triggers(puller: Puller, tx: SyncSender<u64>) {
    loop {
        let transfer = match puller.recv().await {
            Ok(transfer) => transfer,
            Err(e) => {
                eprintln!("[runner] stopped taking triggers: {e}");
                return;
            }
        };
        let payload = match transfer.collect(MAX_TRIGGER_BYTES).await {
            Ok(payload) => payload,
            Err(e) => {
                eprintln!("[runner] unreadable trigger: {e}");
                continue;
            }
        };
        let Some(request) = TriggerRequest::decode(&payload) else {
            eprintln!(
                "[runner] refused a malformed trigger ({} bytes)",
                payload.len()
            );
            continue;
        };
        match offer(&tx, request.node_id) {
            Intake::Taken => {}
            Intake::Dropped => {
                eprintln!(
                    "[runner] trigger backlog full, dropped a press for {}",
                    request.node_id
                );
            }
            Intake::Gone => return,
        }
    }
}

/// Answers snapshot requests with the current output set.
///
/// The request body carries nothing -- there is one snapshot and it is the
/// whole of it -- so it is dropped unread, which refuses whatever a peer sent
/// instead of buffering it.
pub async fn serve_snapshots(replier: Replier, snapshot: Arc<Mutex<Snapshot>>) {
    loop {
        let mut request = match replier.accept().await {
            Ok(request) => request,
            Err(e) => {
                eprintln!("[runner] stopped serving snapshots: {e}");
                return;
            }
        };
        drop(request.take_body());
        let encoded = snapshot.lock().unwrap_or_else(|e| e.into_inner()).encode();
        let mut reply = match request.reply(TransferMeta::default()).await {
            Ok(reply) => reply,
            Err(e) => {
                eprintln!("[runner] cannot reply to a snapshot request: {e}");
                continue;
            }
        };
        if let Err(e) = reply.write_all(&encoded).await {
            eprintln!("[runner] cannot write a snapshot: {e}");
            continue;
        }
        if let Err(e) = reply.finish() {
            eprintln!("[runner] cannot finish a snapshot: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};

    use weida::{ClientTls, Trust};
    use zeughaus_link::SNAPSHOT_PATH;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// A state directory that removes itself, so the tests below start from
    /// no credentials at all rather than from the developer's own.
    struct TempState(PathBuf);

    impl TempState {
        fn new(tag: &str) -> TempState {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "zeughaus-runner-{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            TempState(path)
        }
    }

    impl Drop for TempState {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn loopback() -> SocketAddr {
        SocketAddr::from(([127, 0, 0, 1], 0))
    }

    fn pinned(url: &str) -> Fingerprint {
        EndpointAddr::parse(url)
            .expect("announced url")
            .peer
            .expect("a pinned fingerprint")
    }

    /// Why the identity is a file: an editor pins what the URL named, and a
    /// runner that re-keyed on restart would be a different peer to every one
    /// of them -- which is also what would defeat weida's transparent redial.
    #[tokio::test]
    async fn the_announced_fingerprint_survives_a_restart() {
        let state = TempState::new("restart");
        let first = Transport::start(loopback(), &state.0).await.expect("start");
        let before = pinned(first.url());
        drop(first);

        let second = Transport::start(loopback(), &state.0)
            .await
            .expect("restart");
        assert_eq!(before, pinned(second.url()));
    }

    /// Authorization is the handshake: this listener carries terminal
    /// endpoints, so a client whose key was never pinned must not reach an
    /// endpoint at all -- and the key the runner bootstrapped must.
    #[tokio::test]
    async fn only_a_trusted_client_gets_a_connection() {
        let state = TempState::new("mtls");
        let transport = Transport::start(loopback(), &state.0).await.expect("start");
        let _replier = transport
            .listener()
            .replier(SNAPSHOT_PATH)
            .expect("snapshot endpoint");
        let mut addr = EndpointAddr::parse(transport.url()).expect("announced url");
        addr.path = SNAPSHOT_PATH.to_owned();
        let url = addr.to_string();

        let client = Runtime::new(RuntimeConfig::default()).expect("client runtime");
        let anonymous = client.requester(ClientTls::new(Trust::by_address()));
        let refused =
            tokio::time::timeout(std::time::Duration::from_secs(10), anonymous.connect(&url))
                .await
                .expect("the handshake must fail rather than hang");
        assert!(refused.is_err(), "an anonymous editor must be refused");

        let identity = credentials::load_client_identity(&state.0).expect("bootstrapped client");
        let trusted = client.requester(ClientTls::new(Trust::by_address()).with_identity(identity));
        trusted.connect(&url).await.expect("the pinned client");
    }

    #[test]
    fn an_unspecified_bind_address_is_announced_as_loopback() {
        assert_eq!(
            announced_host("0.0.0.0:7443".parse().expect("addr")),
            "127.0.0.1"
        );
        assert_eq!(
            announced_host("10.0.0.8:7443".parse().expect("addr")),
            "10.0.0.8"
        );
    }

    /// Shell endpoints live on this listener, so a reachable address with an
    /// empty client list must not come up at all -- a runner that served the
    /// network anonymously would be indistinguishable from one that is
    /// configured, right up to the first stranger.
    #[test]
    fn a_reachable_bind_without_client_trust_is_refused() {
        let exposed: SocketAddr = "10.0.0.8:7443".parse().expect("addr");
        let local: SocketAddr = "127.0.0.1:7443".parse().expect("addr");
        let any: SocketAddr = "0.0.0.0:7443".parse().expect("addr");
        let nobody = Trust::by_address();
        let somebody = Trust::pin(Fingerprint::from_bytes([7; 32]));

        let refused = refuse_anonymous_exposure(exposed, &nobody).expect_err("must refuse");
        assert!(refused.contains("10.0.0.8:7443"), "{refused}");
        assert!(refuse_anonymous_exposure(any, &nobody).is_err());
        // Loopback is this machine's own users, which is the bootstrap case
        // before any client was ever provisioned.
        assert!(refuse_anonymous_exposure(local, &nobody).is_ok());
        assert!(refuse_anonymous_exposure(exposed, &somebody).is_ok());
    }

    /// A full queue must cost a dropped press, not a stalled transport: this
    /// runs on the task that also reads the connection, and the event loop it
    /// feeds serves the store and the clocks.
    #[test]
    fn a_full_trigger_queue_drops_the_press() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<u64>(1);
        assert_eq!(offer(&tx, 1), Intake::Taken);
        assert_eq!(offer(&tx, 2), Intake::Dropped);
        // Room again once the loop took one.
        assert_eq!(rx.recv().expect("press"), 1);
        assert_eq!(offer(&tx, 3), Intake::Taken);
    }

    /// The event loop being gone is the process shutting down, which is a
    /// different answer from a backlog and ends the intake task.
    #[test]
    fn a_gone_event_loop_is_not_a_full_queue() {
        let (tx, rx) = std::sync::mpsc::sync_channel::<u64>(1);
        drop(rx);
        assert_eq!(offer(&tx, 1), Intake::Gone);
    }
}
