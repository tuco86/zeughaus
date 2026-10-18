//! `zeughaus-ci serve`: the one process that holds pipelines. Runners
//! enlist into it, editors read from it, and forges post to its webhook
//! intake.
//!
//! One weida listener carries every service: `/enlist` and `/artifacts` for
//! runners; `/ci`, `/events`, `/snapshot`, `/mux` and `/manual` for editors.
//! Trust is pinned keys in both directions. The listener lets in the editors
//! under `<state-dir>/clients/` and the runners `ci.toml` names, and each
//! service then admits only the kind of peer it is for.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::Path;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use weida::{
    Binding, EndpointAddr, Fingerprint, Listener, Publisher, Runtime, RuntimeConfig, ServerTls,
};
use zeughaus_link::{
    ARTIFACTS_PATH, CI_PATH, ENLIST_PATH, EVENTS_PATH, MANUAL_PATH, MUX_PATH, RuntimeEvent,
    SNAPSHOT_PATH, TOPIC_CI, credentials,
};

use crate::artifacts::ArtifactService;
use crate::config::CiConfig;
use crate::enlist::EnlistService;
use crate::runners::Runners;
use crate::scheduler::{self, Alert, CiChange, Started};
use crate::secrets::{self, Secrets};
use crate::serve::{self, CiServer};
use crate::transcripts::{self, Transcripts};
use crate::{hook, manual, transport};

const USAGE: &str = "usage: zeughaus-ci serve --listen <host:port> [--hook <host:port>]";

/// How often the loop looks for alerts and changes the scheduler produced.
const DRAIN_INTERVAL: Duration = Duration::from_millis(100);

#[derive(Debug, PartialEq, Eq)]
struct Args {
    listen: SocketAddr,
    hook: Option<SocketAddr>,
}

fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut listen = None;
    let mut hook = None;
    let mut args = args.iter();
    while let Some(flag) = args.next() {
        let slot = match flag.as_str() {
            "--listen" => &mut listen,
            "--hook" => &mut hook,
            _ => return Err(USAGE.to_owned()),
        };
        let text = args.next().ok_or_else(|| USAGE.to_owned())?;
        let addr = text
            .parse::<SocketAddr>()
            .map_err(|e| format!("{flag} {text:?}: {e}"))?;
        *slot = Some(addr);
    }
    Ok(Args {
        listen: listen.ok_or_else(|| USAGE.to_owned())?,
        hook,
    })
}

pub fn run(args: &[String], state_dir: &Path) -> ExitCode {
    match serve(args, state_dir) {
        Ok(never) => match never {},
        Err(e) => {
            eprintln!("[ci] {e}");
            ExitCode::FAILURE
        }
    }
}

/// Replaces this process with `exe`, same arguments, after an update
/// replaced the file. The listeners are close-on-exec, so the new image binds
/// the same ports. Returns only when that failed; the CI then carries on.
#[cfg(unix)]
fn restart(exe: &Path) {
    use std::os::unix::process::CommandExt;

    eprintln!("[ci] restarting");
    let e = std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .exec();
    eprintln!("[ci] restart failed: {e}");
}

/// No process image to replace: the service is started again by hand.
#[cfg(not(unix))]
fn restart(_exe: &Path) {
    eprintln!("[ci] a new build is installed; start the service again to run it");
}

/// Everything a bound listener keeps alive: dropping any of them stops
/// accepting.
struct Bound {
    url: String,
    listener: Listener,
    _binding: Binding,
    _runtime: Runtime,
}

/// Binds `listen` under the identity stored in `state_dir` and returns the
/// pinned URL that reaches it.
///
/// `client.pem` is not bootstrapped here: the CI is not the editor's local
/// runner, and an editor's key is provisioned under `clients/` by hand.
async fn bind(listen: SocketAddr, state_dir: &Path, config: &CiConfig) -> Result<Bound, String> {
    let identity = credentials::runner_identity(state_dir)?;
    let fingerprint = identity
        .fingerprint()
        .map_err(|e| format!("cannot fingerprint the CI identity: {e}"))?;
    let mut trust = credentials::client_trust(state_dir)?;
    for (name, runner) in &config.runners {
        let pin = runner
            .fingerprint
            .parse::<Fingerprint>()
            .map_err(|e| format!("runner {name}: {e}"))?;
        trust = trust.and_pin(pin);
    }
    let runtime =
        Runtime::new(RuntimeConfig::default()).map_err(|e| format!("weida runtime: {e}"))?;
    let listener = runtime.listener();
    let binding = listener
        .bind_quic(listen, ServerTls::new(identity).require_client(trust))
        .await
        .map_err(|e| format!("cannot bind {listen}: {e}"))?;
    let url = EndpointAddr {
        host: announced_host(listen),
        port: Some(binding.local_addr().port()),
        path: "/".to_owned(),
        peer: Some(fingerprint),
    }
    .to_string();
    Ok(Bound {
        url,
        listener,
        _binding: binding,
        _runtime: runtime,
    })
}

/// The host an editor dials. An unspecified address names no reachable host,
/// so loopback is announced instead.
fn announced_host(bind: SocketAddr) -> String {
    match bind.ip() {
        ip if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST).to_string(),
        ip => ip.to_string(),
    }
}

fn serve(args: &[String], state_dir: &Path) -> Result<std::convert::Infallible, String> {
    let args = parse_args(args)?;
    let config = match CiConfig::load(state_dir)? {
        Some(config) => Arc::new(config),
        None => return Err("no ci.toml".to_owned()),
    };
    std::fs::create_dir_all(state_dir)
        .map_err(|e| format!("cannot create {}: {e}", state_dir.display()))?;
    eprintln!("[ci] state dir {}", state_dir.display());
    // Resolved now: after an update replaced the file, only this path names
    // the new binary.
    let exe = std::env::current_exe();
    // A build on trial counts this start, and the one that failed to come up
    // too often is replaced by the previous build before anything is bound.
    if let Ok(exe) = &exe {
        zeughaus_update::boot(state_dir, exe, zeughaus_update::Product::Ci);
    }

    let secrets = Secrets::new(config.secrets.clone(), state_dir);
    match secrets::check(&secrets, &config) {
        Ok(lines) => {
            for line in lines {
                eprintln!("[ci] secrets: {line}");
            }
        }
        Err(e) => eprintln!("[ci] secrets: {e}"),
    }
    if let Some(addr) = args.hook {
        hook::start(
            addr,
            Arc::clone(&config),
            Arc::clone(&secrets),
            state_dir.to_path_buf(),
        )?;
    }

    // The io driver is what quinn drives its UDP socket on, and the time
    // driver is what the transcript and reply timeouts sleep on.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .thread_name("zeughaus-ci-async")
        .enable_all()
        .build()
        .map_err(|e| format!("cannot start the async runtime: {e}"))?;
    // Bound before anything is announced: an editor must never be handed an
    // endpoint that is not serving yet.
    let bound = rt.block_on(bind(args.listen, state_dir, &config))?;
    eprintln!("[ci] endpoint {}", bound.url);
    credentials::write_endpoint(state_dir, &bound.url)
        .map_err(|e| format!("cannot write the endpoint file: {e}"))?;
    // Every pipeline is on disk, so an update is a plain restart.
    match exe {
        Ok(exe) => zeughaus_update::start(
            state_dir,
            exe.clone(),
            zeughaus_update::Product::Ci,
            Box::new(|| true),
            Box::new(move || restart(&exe)),
        ),
        Err(e) => eprintln!("[ci] not updating: this executable cannot be located: {e}"),
    }

    // `Publisher::publish` is synchronous, but every weida handle was created
    // on this runtime and quinn's driver has to be reachable from this thread.
    let _enter = rt.enter();
    let runners = Arc::new(Runners::new());
    let Started {
        alerts,
        changes,
        tx,
    } = scheduler::start(
        state_dir,
        Arc::clone(&config),
        Arc::clone(&secrets),
        Arc::clone(&runners),
    );
    let transcripts = Arc::new(Transcripts::new(state_dir.to_path_buf()));
    let seq = Arc::new(AtomicU64::new(0));
    let listener = &bound.listener;

    match listener.replier(ENLIST_PATH) {
        Ok(replier) => {
            let service = Arc::new(EnlistService::new(
                state_dir.to_path_buf(),
                Arc::clone(&config),
                Arc::clone(&runners),
                tx,
            ));
            rt.spawn(service.accept(replier));
        }
        Err(e) => return Err(format!("no enlist service: {e}")),
    }
    match listener.replier(ARTIFACTS_PATH) {
        Ok(replier) => {
            let service = Arc::new(ArtifactService::new(
                state_dir.to_path_buf(),
                Arc::clone(&config),
                Arc::clone(&runners),
            ));
            rt.spawn(service.accept(replier));
        }
        Err(e) => return Err(format!("no artifact service: {e}")),
    }
    match listener.replier(CI_PATH) {
        Ok(replier) => {
            let server = CiServer::new(
                state_dir.to_path_buf(),
                Arc::clone(&config),
                Arc::clone(&runners),
                Arc::clone(&transcripts),
            );
            rt.spawn(serve::serve(replier, Arc::new(server)));
        }
        Err(e) => return Err(format!("no ci service: {e}")),
    }
    match listener.replier(SNAPSHOT_PATH) {
        Ok(replier) => {
            rt.spawn(transport::serve_snapshots(
                replier,
                Arc::clone(&config),
                Arc::clone(&seq),
            ));
        }
        Err(e) => eprintln!("[ci] no snapshot service: {e}"),
    }
    match listener.replier(MUX_PATH) {
        Ok(replier) => {
            rt.spawn(transcripts::serve(
                replier,
                Arc::clone(&config),
                Arc::clone(&transcripts),
            ));
        }
        Err(e) => eprintln!("[ci] no mux service: {e}"),
    }
    match listener.replier(MANUAL_PATH) {
        Ok(replier) => {
            rt.spawn(manual::serve(
                replier,
                state_dir.to_path_buf(),
                Arc::clone(&config),
            ));
        }
        Err(e) => eprintln!("[ci] no manual service: {e}"),
    }
    let publisher = match listener.publisher(EVENTS_PATH) {
        Ok(publisher) => Some(publisher),
        Err(e) => {
            eprintln!("[ci] no event stream: {e}");
            None
        }
    };

    let mut events = Events {
        publisher,
        seq,
        logged: None,
    };
    loop {
        while let Ok(alert) = alerts.try_recv() {
            events.alert(alert);
        }
        while let Ok(change) = changes.try_recv() {
            events.change(change);
        }
        std::thread::sleep(DRAIN_INTERVAL);
    }
}

/// Publishes what the scheduler reports on `/events`, numbered by one
/// process-wide sequence that `/snapshot` reports too.
struct Events {
    publisher: Option<Publisher>,
    seq: Arc<AtomicU64>,
    /// The last publish failure, logged once: a broken publisher fails on
    /// every event.
    logged: Option<String>,
}

impl Events {
    fn alert(&mut self, alert: Alert) {
        let seq = self.next_seq();
        self.emit(RuntimeEvent::Alert {
            seq,
            title: alert.title,
            body: alert.body,
        });
    }

    fn change(&mut self, change: CiChange) {
        let seq = self.next_seq();
        self.emit(RuntimeEvent::CiPipeline {
            seq,
            repo: change.repo,
            channel: change.channel,
            number: change.number,
        });
    }

    fn next_seq(&self) -> u64 {
        self.seq.fetch_add(1, Ordering::SeqCst) + 1
    }

    fn emit(&mut self, event: RuntimeEvent) {
        let Some(publisher) = &self.publisher else {
            return;
        };
        if let Err(e) = publisher.publish(TOPIC_CI, event.encode()) {
            let text = e.to_string();
            if self.logged.as_deref() != Some(text.as_str()) {
                eprintln!("[ci] cannot publish on {TOPIC_CI}: {text}");
                self.logged = Some(text);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| (*s).to_owned()).collect()
    }

    #[test]
    fn serve_takes_a_listen_address_and_optionally_a_hook() {
        assert_eq!(
            parse_args(&args(&["--listen", "127.0.0.1:7444"])),
            Ok(Args {
                listen: "127.0.0.1:7444".parse().expect("addr"),
                hook: None
            })
        );
        assert_eq!(
            parse_args(&args(&[
                "--hook",
                "127.0.0.1:8686",
                "--listen",
                "10.8.0.1:7444"
            ])),
            Ok(Args {
                listen: "10.8.0.1:7444".parse().expect("addr"),
                hook: Some("127.0.0.1:8686".parse().expect("addr"))
            })
        );
        assert!(parse_args(&args(&[])).is_err());
        assert!(parse_args(&args(&["--hook", "127.0.0.1:1"])).is_err());
        assert!(parse_args(&args(&["--listen"])).is_err());
        assert!(parse_args(&args(&["--listen", "nowhere"])).is_err());
        assert!(parse_args(&args(&["--listen", "127.0.0.1:1", "extra"])).is_err());
    }

    #[test]
    fn an_unspecified_bind_is_announced_as_loopback() {
        let any: SocketAddr = "0.0.0.0:7444".parse().expect("addr");
        let wg: SocketAddr = "10.8.0.1:7444".parse().expect("addr");
        assert_eq!(announced_host(any), "127.0.0.1");
        assert_eq!(announced_host(wg), "10.8.0.1");
    }
}
