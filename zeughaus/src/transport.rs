//! The editor's weida client: the one QUIC runtime this process dials with,
//! the trust every dial uses, and what this side still has to do about a
//! connection now that weida keeps a dialled address alive.
//!
//! Since weida 0031 a dialled address outlives its connection: the runtime
//! redials it under [`RuntimeConfig::reconnect`], a subscriber's filters are
//! re-sent on the new connection, an `open` with every peer down waits for the
//! redial, and every transition is reported as a [`PeerEvent`]. What weida
//! does *not* redial is an address whose first dial failed -- an address that
//! is wrong or refused is not something a redial fixes -- so the first dial is
//! this module's, and everything after it is weida's. What a redial restores is
//! the transport, not what was said over it: a subscriber sees a gap, and the
//! feed layer asks for a fresh snapshot on every `Connected`.
//!
//! Native only: the wasm editor has no sync layer, so it never learns where a
//! runtime serves and has nothing to dial.

use std::sync::{LazyLock, Once};
use std::time::Duration;

use weida::{
    ClientTls, EndpointAddr, GiveUp, PeerEvent, PeerEvents, ReconnectPolicy, Runtime,
    RuntimeConfig, Trust,
};
use zeughaus_samples::credentials;

/// The one QUIC client this process needs.
///
/// A process-wide `Runtime` rather than one per feed, because connections are
/// pooled per (address, trust anchors) inside it: several Display nodes watching
/// the same runner then share one QUIC connection instead of each opening its
/// own UDP socket and handshake.
///
/// Built on first use, which happens inside a feed task and therefore inside
/// iced's tokio runtime -- `Runtime::new` requires an ambient reactor, and the
/// editor's `App::new` runs before there is one.
pub static QUIC: LazyLock<Option<Runtime>> = LazyLock::new(|| {
    let config = RuntimeConfig {
        reconnect: policy(),
        ..RuntimeConfig::default()
    };
    match Runtime::new(config) {
        Ok(runtime) => Some(runtime),
        // Not fatal: the editor still edits graphs, it just cannot show video.
        Err(e) => {
            eprintln!("[transport] no QUIC runtime, video disabled: {e}");
            None
        }
    }
});

/// How a lost runtime is redialled: doubling from 250 ms, capped at 4 s, and
/// never given up. Long enough that a runtime restart is not a storm, short
/// enough that a viewer notices the runtime coming back. Weida's default caps
/// at 30 s, which is a fleet's number, not an editor's: a runtime that comes
/// back is what the person at the screen is waiting for.
///
/// The same schedule paces [`first_dial`], so a runtime that is not yet
/// listening and one that went away are waited for alike.
pub fn policy() -> ReconnectPolicy {
    ReconnectPolicy {
        initial: Duration::from_millis(250),
        max: Duration::from_secs(4),
        ..ReconnectPolicy::default()
    }
}

/// Where a runtime is reachable: the pinned root URL it announced.
///
/// Compared for equality to notice a runner that restarted on a different port
/// or with a fresh identity; every feed dialled the old one and has to be
/// redialled. A restarted runner is a *different* peer to weida as well: the
/// redial pins the key the first connection proved, and a replacement server
/// with a fresh key is refused in the handshake and reported as
/// [`GiveUp::PeerChanged`] rather than silently connected to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Endpoint(pub String);

impl Endpoint {
    /// The URL of one endpoint path on this runtime.
    ///
    /// The announced URL is the root (`/`); every path this editor dials is the
    /// same authority and the same pinned fingerprint with the path replaced,
    /// so a runtime announces one address and not a list.
    pub fn path(&self, path: &str) -> Result<String, String> {
        let mut addr =
            EndpointAddr::parse(&self.0).map_err(|e| format!("endpoint {}: {e}", self.0))?;
        addr.path = path.to_owned();
        Ok(addr.to_string())
    }
}

/// The TLS every dial uses: the fingerprint pinned in the URL the runtime
/// announced, and this machine's client identity when one was bootstrapped.
///
/// The identity is not created here. A runner pins the key it wrote itself, so
/// a key this editor minted would authenticate nothing -- the absence of one
/// means no runner ever ran under this state directory, and the honest
/// outcome is a refused handshake with a log line naming the path, not a
/// second key nobody trusts.
pub fn client_tls() -> ClientTls {
    let dir = credentials::state_dir();
    match credentials::load_client_identity(&dir) {
        Some(identity) => ClientTls::new(Trust::by_address()).with_identity(identity),
        None => {
            // Once per process: every feed, event subscription and snapshot
            // request builds its own `ClientTls`, and each of them would
            // otherwise repeat the same line for the same missing file.
            static ANNOUNCED: Once = Once::new();
            ANNOUNCED.call_once(|| {
                eprintln!(
                    "[editor] no client identity in {} -- a runner that requires \
                     a client will refuse every connection (start a runner on \
                     this machine, or copy client.pem from the one you dial)",
                    dir.display()
                );
            });
            ClientTls::new(Trust::by_address())
        }
    }
}

/// Dials `url` until the first dial succeeds, paced by [`policy`].
///
/// Weida keeps an address alive from its first successful dial on, and
/// reports a first dial that failed to the caller instead: a runtime that is
/// announced but not yet answering, or one whose row outlived it, has to be
/// asked again by this side. Only an address that cannot be dialled at all --
/// malformed, or a fingerprint that is not one -- is given up on; everything
/// else is a runtime that may still come, and the caller's task is dropped
/// when the store says otherwise.
///
/// `connect` is a plain closure returning the endpoint's `connect` future,
/// not an `async` closure: the future an `AsyncFn` returns borrows the closure
/// for a lifetime the caller cannot name, and a task handed to iced has to be
/// provably `Send` for it.
pub async fn first_dial<F>(url: &str, mut connect: impl FnMut() -> F) -> Result<(), String>
where
    F: Future<Output = Result<(), weida::Error>>,
{
    let policy = policy();
    let mut attempt = 0u32;
    loop {
        match connect().await {
            Ok(()) => return Ok(()),
            Err(
                e @ (weida::Error::InvalidAddress(_)
                | weida::Error::InvalidEndpointPath
                | weida::Error::InvalidFingerprint(_)
                | weida::Error::Runtime(_)),
            ) => return Err(format!("connect {url}: {e}")),
            Err(e) => {
                attempt = attempt.saturating_add(1);
                // The first failure is worth a line; a runtime that stays down
                // is not worth one per attempt.
                if attempt == 1 {
                    eprintln!("[transport] connect {url}: {e}; retrying");
                }
                tokio::time::sleep(policy.delay(attempt)).await;
            }
        }
    }
}

/// Resolves once weida stops redialling an address, with the reason.
///
/// For a task that only needs to know when its peer is gone for good -- a
/// frame feed, whose every `open` would otherwise fail with the loss cause on
/// each turn of its loop -- and does not care about the transitions in
/// between. `Connected`, `Lost` and `Retrying` are skipped, a lagging reader
/// is not an error, and an ended stream is an ended endpoint.
pub async fn gave_up(link: &mut PeerEvents) -> GiveUp {
    loop {
        match link.recv().await {
            Some(PeerEvent::GaveUp { why, .. }) => return why,
            // The endpoint is gone, which for an endpoint this task owns means
            // the task is ending anyway.
            None => return GiveUp::Policy { attempts: 0 },
            Some(_) => {}
        }
    }
}

/// Why weida stopped redialling, in a log line's words.
///
/// `PeerChanged` is the one worth recognizing: a runner regenerates its
/// identity per start, so a restarted runner on the same port is a stranger to
/// the address that was dialled. The store announces the new address, and the
/// task that logged this is replaced by one that dials it.
pub fn explain(why: &GiveUp) -> String {
    match why {
        GiveUp::PeerChanged {
            presented: Some(fp),
        } => {
            format!("a different runtime answers at this address ({fp})")
        }
        GiveUp::PeerChanged { presented: None } => {
            "something that proves no identity answers at this address".to_owned()
        }
        GiveUp::Failed(reason) => format!("the redial cannot be used: {reason}"),
        GiveUp::Policy { attempts } => format!("given up after {attempts} redial(s)"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every endpoint this editor dials is derived from the one announced URL,
    /// so deriving must keep the pinned fingerprint: dropping it would turn a
    /// pinned dial into one that trusts nothing and fails.
    #[test]
    fn a_derived_path_keeps_the_pinned_fingerprint() {
        let fingerprint = "sha256:".to_owned() + &"ab".repeat(32);
        let root = Endpoint(format!("weida://{fingerprint}@127.0.0.1:7443/"));
        assert_eq!(
            root.path("/samples").expect("derive"),
            format!("weida://{fingerprint}@127.0.0.1:7443/samples")
        );
    }

    /// A malformed announcement is a runtime problem, not a panic here.
    #[test]
    fn a_bad_endpoint_reports_instead_of_panicking() {
        assert!(Endpoint("not a url".to_owned()).path("/samples").is_err());
    }

    /// The redial schedule is bounded in both directions: quick enough to
    /// notice a runtime coming back, slow enough that one that is not serving
    /// is not hammered. Jitter draws from the upper half, so the bound is on
    /// the base.
    #[test]
    fn redial_backoff_is_bounded() {
        let policy = ReconnectPolicy {
            jitter: false,
            ..policy()
        };
        assert_eq!(policy.delay(1), Duration::from_millis(250));
        assert_eq!(policy.delay(5), Duration::from_millis(4000));
        assert_eq!(policy.delay(99), Duration::from_millis(4000));
        assert!(policy.redials(), "a runtime that went away is waited for");
    }

    /// A first dial that cannot succeed must say so rather than retry forever
    /// on an address nothing will ever answer.
    #[tokio::test]
    async fn an_unparseable_address_is_refused_at_once() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let result = first_dial("not a url", async || {
            calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            Err(weida::Error::InvalidAddress("not a url".to_owned()))
        })
        .await;
        assert!(result.is_err());
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 1);
    }

    /// A refused dial is a runtime that is not there yet: asked again, and
    /// the success is the caller's.
    #[tokio::test(start_paused = true)]
    async fn a_refused_dial_is_asked_again() {
        let calls = std::sync::atomic::AtomicU32::new(0);
        let result = first_dial("weida://127.0.0.1:1/", async || {
            let n = calls.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
            if n < 2 {
                Err(weida::Error::NotConnected)
            } else {
                Ok(())
            }
        })
        .await;
        assert!(result.is_ok());
        assert_eq!(calls.load(std::sync::atomic::Ordering::Relaxed), 3);
    }
}
