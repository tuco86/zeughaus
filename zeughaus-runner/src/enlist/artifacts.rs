//! Artifact sets crossing to and from the CI.
//!
//! A job that needs another job's artifacts and runs on another runner gets
//! them as a tar file from the CI's `/artifacts` service; the runner that ran
//! the producing job sends that tar when the CI asks for it. One exchange per
//! transfer: a [`ArtifactRequest`] frame, then the raw bytes of the tar in
//! whichever direction the request names.

use std::io::{Read, Write};
use std::path::Path;
use std::sync::Arc;

use tokio::io::AsyncReadExt;
use weida::TransferMeta;
use zeughaus_link::ARTIFACTS_PATH;
use zeughaus_link::enlist::{
    ArtifactReply, ArtifactRequest, MAX_ARTIFACT_BYTES, read_frame, write_frame,
};

use super::{Executor, artifacts_dir, tmp_dir};

/// Bytes moved per read or write.
const CHUNK: usize = 1 << 20;

impl Executor {
    /// Sends the artifacts of `run` to the CI, on a thread of its own.
    pub fn upload(self: &Arc<Self>, run: String) {
        let exec = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("zeughaus-enlist-upload".into())
            .spawn(move || {
                if let Err(e) = exec.upload_blocking(&run) {
                    eprintln!("[enlist] {run}: artifacts not sent: {e}");
                }
            });
        if let Err(e) = spawned {
            eprintln!("[enlist] cannot start the upload thread: {e}");
        }
    }

    fn upload_blocking(&self, run: &str) -> Result<(), String> {
        if run.contains('/') {
            return Err("not a run id".to_owned());
        }
        let dir = artifacts_dir(&self.state_dir).join(run);
        if !dir.is_dir() {
            return Err(format!("{} does not exist", dir.display()));
        }
        let tar = tmp_dir(&self.state_dir).join(format!("{run}.tar"));
        pack(&dir, &tar)?;
        let sent = self.handle.block_on(self.send_tar(run, &tar));
        let _ = std::fs::remove_file(&tar);
        sent
    }

    async fn send_tar(&self, run: &str, tar: &Path) -> Result<(), String> {
        let size = std::fs::metadata(tar)
            .map_err(|e| format!("cannot stat {}: {e}", tar.display()))?
            .len();
        if size > MAX_ARTIFACT_BYTES {
            return Err(format!(
                "{size} bytes is more than the {MAX_ARTIFACT_BYTES} allowed"
            ));
        }
        let mut file =
            std::fs::File::open(tar).map_err(|e| format!("cannot read {}: {e}", tar.display()))?;
        let requester = self.runtime.requester(self.tls.clone());
        let url = self.config.ci_url(ARTIFACTS_PATH)?;
        requester
            .connect(&url)
            .await
            .map_err(|e| format!("connect {url}: {e}"))?;
        let (mut out, reply) = requester
            .open(TransferMeta::default())
            .await
            .map_err(|e| format!("open: {e}"))?;
        let request = ArtifactRequest::Upload {
            run: run.to_owned(),
            size,
        };
        write_frame(&mut out, &request).await?;
        let mut buf = vec![0u8; CHUNK];
        loop {
            let n = file.read(&mut buf).map_err(|e| format!("read: {e}"))?;
            if n == 0 {
                break;
            }
            out.write_all(&buf[..n])
                .await
                .map_err(|e| format!("write: {e}"))?;
        }
        out.finish().map_err(|e| format!("finish: {e}"))?;
        let mut body = reply.recv().await.map_err(|e| format!("reply: {e}"))?;
        match read_frame::<_, ArtifactReply>(&mut body).await? {
            Some(ArtifactReply::Stored) => Ok(()),
            Some(ArtifactReply::Error { message }) => Err(message),
            Some(ArtifactReply::Sending { .. }) | None => {
                Err("the CI did not confirm the upload".to_owned())
            }
        }
    }

    /// Fetches the artifacts of `run` from the CI and unpacks them into
    /// `dest`. Called on the thread that prepares a job.
    pub fn download(&self, run: &str, dest: &Path) -> Result<(), String> {
        let tar = tmp_dir(&self.state_dir).join(format!("{run}.tar"));
        if let Some(parent) = tar.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let fetched = self
            .handle
            .block_on(self.receive_tar(run, &tar))
            .and_then(|()| unpack(&tar, dest));
        let _ = std::fs::remove_file(&tar);
        fetched
    }

    async fn receive_tar(&self, run: &str, tar: &Path) -> Result<(), String> {
        let requester = self.runtime.requester(self.tls.clone());
        let url = self.config.ci_url(ARTIFACTS_PATH)?;
        requester
            .connect(&url)
            .await
            .map_err(|e| format!("connect {url}: {e}"))?;
        let (mut out, reply) = requester
            .open(TransferMeta::default())
            .await
            .map_err(|e| format!("open: {e}"))?;
        let request = ArtifactRequest::Download {
            run: run.to_owned(),
        };
        write_frame(&mut out, &request).await?;
        out.finish().map_err(|e| format!("finish: {e}"))?;
        let mut body = reply.recv().await.map_err(|e| format!("reply: {e}"))?;
        let size = match read_frame::<_, ArtifactReply>(&mut body).await? {
            Some(ArtifactReply::Sending { size }) => size,
            Some(ArtifactReply::Error { message }) => return Err(message),
            Some(ArtifactReply::Stored) | None => {
                return Err("the CI did not answer the download".to_owned());
            }
        };
        if size > MAX_ARTIFACT_BYTES {
            return Err(format!(
                "{size} bytes is more than the {MAX_ARTIFACT_BYTES} allowed"
            ));
        }
        let mut file = std::fs::File::create(tar)
            .map_err(|e| format!("cannot create {}: {e}", tar.display()))?;
        let mut buf = vec![0u8; CHUNK];
        let mut remaining = size;
        while remaining > 0 {
            let want = usize::try_from(remaining).map_or(CHUNK, |r| r.min(CHUNK));
            body.read_exact(&mut buf[..want])
                .await
                .map_err(|e| format!("the CI stopped {remaining} bytes short: {e}"))?;
            file.write_all(&buf[..want])
                .map_err(|e| format!("write: {e}"))?;
            remaining -= want as u64;
        }
        Ok(())
    }
}

/// Packs the contents of `dir` into the tar file `tar`.
fn pack(dir: &Path, tar: &Path) -> Result<(), String> {
    if let Some(parent) = tar.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
    }
    let file =
        std::fs::File::create(tar).map_err(|e| format!("cannot create {}: {e}", tar.display()))?;
    let mut builder = tar::Builder::new(file);
    builder
        .append_dir_all(".", dir)
        .map_err(|e| format!("cannot pack {}: {e}", dir.display()))?;
    builder
        .finish()
        .map_err(|e| format!("cannot finish {}: {e}", tar.display()))
}

/// Unpacks the tar file `tar` into `dest`, which is created. Paths that
/// would leave `dest` are refused by the reader.
fn unpack(tar: &Path, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    let file =
        std::fs::File::open(tar).map_err(|e| format!("cannot read {}: {e}", tar.display()))?;
    tar::Archive::new(file)
        .unpack(dest)
        .map_err(|e| format!("cannot unpack {}: {e}", tar.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_artifact_set_survives_a_round_trip_through_a_tar() {
        let base = std::env::temp_dir().join(format!("zci-tar-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let from = base.join("from");
        std::fs::create_dir_all(from.join("sub")).unwrap();
        std::fs::write(from.join("a"), "one").unwrap();
        std::fs::write(from.join("sub/b"), "two").unwrap();
        let tar = base.join("set.tar");
        pack(&from, &tar).unwrap();
        let to = base.join("to");
        unpack(&tar, &to).unwrap();
        let a = std::fs::read_to_string(to.join("a")).ok();
        let b = std::fs::read_to_string(to.join("sub/b")).ok();
        let _ = std::fs::remove_dir_all(&base);
        assert_eq!((a.as_deref(), b.as_deref()), (Some("one"), Some("two")));
    }
}
