//! The two exchanges the tools have with the CI: a JSON question on `/ci`
//! and a manual run's upload on `/manual`.
//!
//! The CI is reached the way an editor reaches it: the pinned URL names its
//! fingerprint, and this machine's client key is what the CI admits. One
//! weida runtime and one connected requester per path are made on first use
//! and kept, so a poll every two seconds reuses a connection; a requester
//! whose exchange failed is dropped and dialled again by the next call,
//! because weida does not redial an address it gave up on. Every wait is
//! bounded, so a CI that does not answer is a tool error and not a call that
//! never returns.

use std::collections::HashMap;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

use serde::Deserialize;
use tokio::sync::Mutex;
use weida::{
    ClientTls, EndpointAddr, OutgoingTransfer, ReplyStream, Requester, Runtime, RuntimeConfig,
    TransferMeta, Trust,
};
use zeughaus_link::enlist::{read_frame, write_frame};
use zeughaus_link::manual::{RunReply, RunUpload};
use zeughaus_link::{CI_PATH, CiReply, CiRequest, MANUAL_PATH, MAX_CI_REPLY_BYTES, credentials};

/// How long a first dial may take.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);

/// How long a question on `/ci` may take.
const CI_TIMEOUT: Duration = Duration::from_secs(30);

/// How long the CI may take to answer an upload once all of it was sent. It
/// waits up to 30 s for its scheduler to number the pipeline.
const MANUAL_REPLY_TIMEOUT: Duration = Duration::from_secs(90);

/// How long to look for the CI's refusal after an upload broke off: a CI
/// that refuses a request says why and stops reading.
const REFUSAL_TIMEOUT: Duration = Duration::from_secs(5);

/// Bytes read from a bundle and written to the exchange at a time.
const CHUNK: usize = 1 << 20;

/// The CI to talk to: its pinned URL and the identity to present.
pub struct Link {
    url: String,
    tls: ClientTls,
}

/// The part of the editor's `zeughaus.toml` this process reads. Not strict:
/// the file is the editor's, and holds what it needs besides.
#[derive(Deserialize)]
struct EditorFile {
    #[serde(default)]
    ci: Vec<String>,
}

impl Link {
    /// The CI is `ci`, else the first of `ci = [..]` in the state directory's
    /// `zeughaus.toml`; the identity is the state directory's `client.pem`.
    pub fn resolve(ci: Option<String>, state_dir: &Path) -> Result<Link, String> {
        let config = state_dir.join("zeughaus.toml");
        let url = match ci {
            Some(url) => url,
            None => first_ci(&config)?.ok_or_else(|| {
                format!("no CI configured: add ci = [..] to {}", config.display())
            })?,
        };
        let addr =
            EndpointAddr::parse(&url).map_err(|e| format!("the CI URL {url} is not valid: {e}"))?;
        if addr.peer.is_none() {
            return Err(format!(
                "the CI URL {url} names no fingerprint (weida://sha256:<fp>@host:port/): a CI \
                 that is not pinned could be anyone"
            ));
        }
        let identity = credentials::load_client_identity(state_dir).ok_or_else(|| {
            format!(
                "no client identity in {}",
                state_dir.join("client.pem").display()
            )
        })?;
        Ok(Link {
            url,
            tls: ClientTls::new(Trust::by_address()).with_identity(identity),
        })
    }

    pub fn url(&self) -> &str {
        &self.url
    }
}

fn first_ci(path: &Path) -> Result<Option<String>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
    };
    let file: EditorFile = toml::from_str(&text).map_err(|e| format!("{}: {e}", path.display()))?;
    Ok(file.ci.into_iter().next())
}

/// What is built on first use.
#[derive(Default)]
struct State {
    runtime: Option<Runtime>,
    requesters: HashMap<&'static str, Arc<Requester>>,
}

pub struct Client {
    link: Result<Link, String>,
    state: Mutex<State>,
}

impl Client {
    /// A client that dials nothing until a call is made. An `Err` link is why
    /// no call can be: every call returns it.
    pub fn new(link: Result<Link, String>) -> Client {
        Client {
            link,
            state: Mutex::new(State::default()),
        }
    }

    /// Whether there is a CI to talk to, for a caller that is about to do
    /// work a missing CI would waste.
    pub fn configured(&self) -> Result<(), String> {
        self.link.as_ref().map(|_| ()).map_err(Clone::clone)
    }

    /// Asks the CI one question. The CI's own refusal is the error.
    pub async fn ci(&self, request: CiRequest) -> Result<CiReply, String> {
        let (requester, _) = self.requester(CI_PATH).await?;
        let exchange = async {
            let reply = requester
                .request(&request.encode())
                .await
                .map_err(|e| format!("ci: {e}"))?;
            reply
                .collect(MAX_CI_REPLY_BYTES)
                .await
                .map_err(|e| format!("ci: {e}"))
        };
        let encoded = match tokio::time::timeout(CI_TIMEOUT, exchange).await {
            Ok(Ok(encoded)) => encoded,
            Ok(Err(e)) => {
                self.forget(CI_PATH).await;
                return Err(e);
            }
            Err(_) => {
                self.forget(CI_PATH).await;
                return Err(format!("ci: no answer within {} s", CI_TIMEOUT.as_secs()));
            }
        };
        match CiReply::decode(&encoded) {
            Some(CiReply::Error { message }) => Err(message),
            Some(reply) => Ok(reply),
            None => Err("ci: malformed reply".to_owned()),
        }
    }

    /// Uploads a snapshot: the frame, then `bundles` in order, then waits for
    /// the CI to say what became of it.
    pub async fn manual_upload(
        &self,
        upload: &RunUpload,
        bundles: &[PathBuf],
    ) -> Result<RunReply, String> {
        let (requester, url) = self.requester(MANUAL_PATH).await?;
        let reply = exchange(&requester, upload, bundles)
            .await
            .map_err(|e| format!("upload to {url}: {e}"));
        if reply.is_err() {
            self.forget(MANUAL_PATH).await;
        }
        reply
    }

    /// Closes the connections this client opened, so the CI sees it leave.
    pub async fn shutdown(&self) {
        let mut state = self.state.lock().await;
        state.requesters.clear();
        if let Some(runtime) = state.runtime.take() {
            runtime.shutdown().await;
        }
    }

    /// The connected requester for `path`, made now if there is none.
    async fn requester(&self, path: &'static str) -> Result<(Arc<Requester>, String), String> {
        let link = self.link.as_ref().map_err(Clone::clone)?;
        let url = crate::cli::url_of(&link.url, path)?;
        // Held across the dial: two calls that both find nothing would dial
        // twice.
        let mut state = self.state.lock().await;
        if let Some(requester) = state.requesters.get(path) {
            return Ok((Arc::clone(requester), url));
        }
        let runtime = match &state.runtime {
            Some(runtime) => runtime.clone(),
            None => {
                let runtime =
                    Runtime::new(RuntimeConfig::default()).map_err(|e| format!("weida: {e}"))?;
                state.runtime = Some(runtime.clone());
                runtime
            }
        };
        let requester = runtime.requester(link.tls.clone());
        match tokio::time::timeout(CONNECT_TIMEOUT, requester.connect(&url)).await {
            Ok(Ok(())) => {}
            Ok(Err(e)) => return Err(format!("connect {url}: {e}")),
            Err(_) => {
                return Err(format!(
                    "connect {url}: no answer within {} s",
                    CONNECT_TIMEOUT.as_secs()
                ));
            }
        }
        let requester = Arc::new(requester);
        state.requesters.insert(path, Arc::clone(&requester));
        Ok((requester, url))
    }

    async fn forget(&self, path: &'static str) {
        self.state.lock().await.requesters.remove(path);
    }
}

/// One `/manual` exchange.
async fn exchange(
    requester: &Requester,
    upload: &RunUpload,
    bundles: &[PathBuf],
) -> Result<RunReply, String> {
    // With every connection down weida waits for a redial, which may be long.
    let (mut out, reply) =
        tokio::time::timeout(CONNECT_TIMEOUT, requester.open(TransferMeta::default()))
            .await
            .map_err(|_| format!("open: no connection within {} s", CONNECT_TIMEOUT.as_secs()))?
            .map_err(|e| format!("open: {e}"))?;
    if let Err(e) = send(&mut out, upload, bundles).await {
        // A CI that refuses the upload answers and stops reading, which
        // breaks the stream under the writer. Its answer says why.
        let refusal = tokio::time::timeout(REFUSAL_TIMEOUT, read_reply(reply)).await;
        return match refusal {
            Ok(Ok(refused @ RunReply::Refused { .. })) => Ok(refused),
            _ => Err(e),
        };
    }
    out.finish().map_err(|e| format!("finish: {e}"))?;
    tokio::time::timeout(MANUAL_REPLY_TIMEOUT, read_reply(reply))
        .await
        .map_err(|_| {
            format!(
                "the CI gave no answer within {} s",
                MANUAL_REPLY_TIMEOUT.as_secs()
            )
        })?
}

/// The frame, then each bundle as raw bytes, exactly as many as `sizes` said.
async fn send(
    out: &mut OutgoingTransfer,
    upload: &RunUpload,
    bundles: &[PathBuf],
) -> Result<(), String> {
    write_frame(out, upload).await?;
    for (index, (path, size)) in bundles.iter().zip(&upload.sizes).enumerate() {
        send_file(out, path, *size)
            .await
            .map_err(|e| format!("bundle {index}: {e}"))?;
    }
    Ok(())
}

async fn send_file(out: &mut OutgoingTransfer, path: &Path, size: u64) -> Result<(), String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("cannot open {}: {e}", path.display()))?;
    let mut buf = vec![0u8; CHUNK];
    let mut sent = 0u64;
    while sent < size {
        let want = usize::try_from(size - sent).map_or(CHUNK, |left| left.min(CHUNK));
        let (back, filled, read) = read_chunk(file, buf, want).await?;
        (file, buf) = (back, filled);
        if read == 0 {
            break;
        }
        out.write_all(&buf[..read])
            .await
            .map_err(|e| format!("write: {e}"))?;
        sent += read as u64;
    }
    if sent != size {
        return Err(format!(
            "{} holds {sent} bytes, not the {size} announced",
            path.display()
        ));
    }
    Ok(())
}

/// One read of at most `want` bytes on the blocking pool: a disk's latency
/// must not sit on a worker thread that also drives QUIC.
async fn read_chunk(
    mut file: std::fs::File,
    mut buf: Vec<u8>,
    want: usize,
) -> Result<(std::fs::File, Vec<u8>, usize), String> {
    tokio::task::spawn_blocking(move || {
        let read = file
            .read(&mut buf[..want])
            .map_err(|e| format!("read: {e}"))?;
        Ok((file, buf, read))
    })
    .await
    .map_err(|e| format!("read: {e}"))?
}

async fn read_reply(reply: ReplyStream) -> Result<RunReply, String> {
    let mut body = reply.recv().await.map_err(|e| format!("reply: {e}"))?;
    read_frame::<_, RunReply>(&mut body)
        .await?
        .ok_or_else(|| "the CI ended the exchange without an answer".to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CI: &str = "weida://sha256:0000000000000000000000000000000000000000000000000000000000000001@10.8.0.1:7444/";

    fn state(tag: &str) -> PathBuf {
        let dir =
            std::env::temp_dir().join(format!("zeughaus-mcp-client-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("state directory");
        dir
    }

    #[test]
    fn no_ci_is_named_with_the_file_to_add_it_to() {
        let dir = state("none");
        let err = Link::resolve(None, &dir).err().expect("no CI");
        assert_eq!(
            err,
            format!(
                "no CI configured: add ci = [..] to {}",
                dir.join("zeughaus.toml").display()
            )
        );
        // An empty list is the same as no file.
        std::fs::write(dir.join("zeughaus.toml"), "remotes = []\nci = []\n").unwrap();
        assert!(
            Link::resolve(None, &dir)
                .err()
                .expect("empty")
                .starts_with("no CI configured")
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_client_key_is_named_with_its_path() {
        let dir = state("nokey");
        let err = Link::resolve(Some(CI.to_owned()), &dir)
            .err()
            .expect("no identity");
        assert_eq!(
            err,
            format!("no client identity in {}", dir.join("client.pem").display())
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn the_first_ci_of_the_editors_file_is_dialled_unless_one_is_given() {
        let dir = state("first");
        credentials::client_identity(&dir).expect("client key");
        std::fs::write(
            dir.join("zeughaus.toml"),
            format!("remotes = [\"weida://elsewhere/\"]\ntheme = \"x\"\nci = [\"{CI}\", \"weida://other/\"]\n"),
        )
        .unwrap();
        assert_eq!(Link::resolve(None, &dir).expect("from file").url(), CI);
        let other = CI.replace("10.8.0.1", "10.8.0.2");
        assert_eq!(
            Link::resolve(Some(other.clone()), &dir)
                .expect("flag")
                .url(),
            other
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_ci_that_is_not_pinned_or_not_a_url_is_refused() {
        let dir = state("pin");
        credentials::client_identity(&dir).expect("client key");
        let err = Link::resolve(Some("weida://10.8.0.1:7444/".to_owned()), &dir)
            .err()
            .expect("unpinned");
        assert!(err.contains("names no fingerprint"), "{err}");
        let err = Link::resolve(Some("not a url".to_owned()), &dir)
            .err()
            .expect("garbage");
        assert!(err.contains("is not valid"), "{err}");
        std::fs::write(dir.join("zeughaus.toml"), "ci = 3\n").unwrap();
        let err = Link::resolve(None, &dir).err().expect("bad toml");
        assert!(err.contains("zeughaus.toml"), "{err}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn an_unconfigured_client_says_why_on_every_call() {
        let client = Client::new(Err("no CI configured: add ci = [..] to x".to_owned()));
        let asked = client.ci(CiRequest::Overview).await.expect_err("no CI");
        assert!(asked.starts_with("no CI configured"), "{asked}");
        let upload = RunUpload {
            origin: "o".to_owned(),
            actor: "a@b".to_owned(),
            summary: "s".to_owned(),
            jobs: vec!["j".to_owned()],
            dirs: Vec::new(),
            sizes: Vec::new(),
        };
        let sent = client.manual_upload(&upload, &[]).await.expect_err("no CI");
        assert!(sent.starts_with("no CI configured"), "{sent}");
        client.shutdown().await;
    }
}
