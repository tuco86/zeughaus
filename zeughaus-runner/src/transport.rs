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

use weida::{Binding, EndpointAddr, Identity, Listener, Runtime, RuntimeConfig};

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
            port: binding.local_addr().port(),
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
}
