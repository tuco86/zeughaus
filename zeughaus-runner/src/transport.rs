//! The runtime's weida transport: one bound listener, one announced URL.
//!
//! Everything this process says to an editor leaves through here. Frames go out
//! on `/samples`, and the endpoints the later steps add (events, snapshots,
//! triggers) are registered on the same listener, so an editor learns one
//! address and derives every path from it.
//!
//! Trust is fingerprint pinning: the identity is generated in memory per start
//! and its fingerprint is carried in the announced URL
//! (`weida://sha256:<fp>@host:port/`), so a viewer accepts exactly the process
//! that announced itself and nothing else -- no certificate to distribute, no
//! authority to run. [`Transport::start`] is the one place the server identity
//! is built, and therefore the swap point for a certificate issued by a secret
//! store later on.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::mpsc::{SyncSender, TrySendError};
use std::sync::{Arc, Mutex};

use weida::{
    Binding, EndpointAddr, Identity, Listener, Puller, Replier, Runtime, RuntimeConfig,
    TransferMeta,
};
use zeughaus_samples::{MAX_TRIGGER_BYTES, Snapshot, TriggerRequest};

/// A bound weida listener and the pinned URL that reaches it.
pub struct Transport {
    url: String,
    listener: Listener,
    /// The bound socket. Held for the life of the process: dropping it would
    /// stop accepting, and the transport has no shutdown short of exit.
    _binding: Binding,
    /// The weida runtime owns the connection pool and the QUIC endpoints.
    _runtime: Runtime,
}

impl Transport {
    /// Binds `bind` under a fresh in-memory identity and returns the pinned
    /// root URL for it.
    ///
    /// Nothing is announced from here: the caller does that once it has a store
    /// connection, and it must not happen before this returns -- an editor
    /// pointed at a runtime that is not yet serving would fail its first
    /// request and have no reason to try again.
    pub async fn start(bind: SocketAddr) -> Result<Transport, String> {
        let host = announced_host(bind);
        // Named for the host it will be announced under so that a peer trusting
        // the certificate as an anchor can still verify it; pinning by
        // fingerprint does not consult the name, but the two trust models then
        // cost the same identity.
        let identity = Identity::generate_for([host.clone()])
            .map_err(|e| format!("cannot generate an identity for {host}: {e}"))?;
        // Before the identity moves into the binding: the fingerprint is what
        // the URL pins, and it is read off the certificate.
        let fingerprint = identity
            .fingerprint()
            .map_err(|e| format!("cannot fingerprint the identity: {e}"))?;

        let runtime =
            Runtime::new(RuntimeConfig::default()).map_err(|e| format!("weida runtime: {e}"))?;
        let listener = runtime.listener();
        let binding = listener
            .bind_quic(bind, identity)
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
