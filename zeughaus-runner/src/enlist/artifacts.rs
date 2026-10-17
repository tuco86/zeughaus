//! Artifact sets crossing to and from the CI.
//!
//! An artifact set lives on the runner that produced it, under
//! `ci/artifacts/<run>`, until the CI says `DropArtifacts`. A job that needs
//! another job's set and runs on another runner gets it as a tar file relayed
//! by the CI's `/artifacts` service, which stores nothing: the consumer's
//! `Download` makes the CI ask the producer to `Upload` as one `transfer`, and
//! the CI streams those bytes to the consumer. One exchange per transfer: an
//! [`ArtifactRequest`] frame, then the raw bytes of the tar in whichever
//! direction the request names.
//!
//! A tar in transit is `<tmp>/<run>.<n>.tar` with `n` from the executor's
//! counter: two downloads of one producer's set on one runner never share a
//! file. Nothing outlives its transfer except after a crash, and
//! [`Executor::clear_stale_tars`] removes those at start.
//!
//! The bundles of a try run come the same way from the CI itself, which
//! stored them (`TryBundle`), straight into the run's own directory: they go
//! with it.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use tokio::io::AsyncReadExt;
use weida::{OutgoingTransfer, ReplyStream, TransferMeta};
use zeughaus_link::ARTIFACTS_PATH;
use zeughaus_link::enlist::{
    ArtifactReply, ArtifactRequest, MAX_ARTIFACT_BYTES, read_frame, write_frame,
};

use super::cleanup::remove_tree;
use super::run::safe_run;
use super::{Executor, artifacts_dir, tmp_dir};

/// Bytes moved per read or write.
const CHUNK: usize = 1 << 20;

impl Executor {
    /// A path for one tar in transit, unique among this process's transfers.
    fn transit_tar(&self, run: &str) -> PathBuf {
        let n = self.transfers.fetch_add(1, Ordering::Relaxed);
        tmp_dir(&self.state_dir).join(format!("{run}.{n}.tar"))
    }

    /// Removes tar files a previous process left in the scratch directory.
    /// Only files: directories there hold secrets. One runner per state
    /// directory holds `runner.lock`, so nothing else writes these.
    pub(super) fn clear_stale_tars(&self) {
        let Ok(entries) = std::fs::read_dir(tmp_dir(&self.state_dir)) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let is_tar = path.extension().is_some_and(|ext| ext == "tar");
            if is_tar
                && entry.file_type().is_ok_and(|kind| kind.is_file())
                && let Err(e) = std::fs::remove_file(&path)
            {
                eprintln!("[enlist] cannot remove {}: {e}", path.display());
            }
        }
    }

    /// The run ids of the artifact sets this runner holds, for `Hello`.
    pub(super) fn artifact_sets(&self) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(artifacts_dir(&self.state_dir)) else {
            return Vec::new();
        };
        entries
            .flatten()
            .filter(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .filter_map(|entry| entry.file_name().into_string().ok())
            .filter(|run| safe_run(run))
            .collect()
    }

    /// Deletes the artifact set of `run`: no job needs it any more. Runs on
    /// a thread of its own, as removing a big tree can take a while.
    pub fn drop_artifacts(self: &Arc<Self>, run: &str) {
        if !safe_run(run) {
            eprintln!("[enlist] {run:?}: not a run id; artifacts not dropped");
            return;
        }
        let dir = artifacts_dir(&self.state_dir).join(run);
        let run = run.to_owned();
        let spawned = std::thread::Builder::new()
            .name("zeughaus-enlist-drop".into())
            .spawn(move || match remove_tree(&dir) {
                Ok(()) => eprintln!("[enlist] {run}: artifacts dropped"),
                Err(e) => eprintln!("[enlist] {run}: cannot drop artifacts: {e}"),
            });
        if let Err(e) = spawned {
            eprintln!("[enlist] cannot start the drop thread: {e}");
        }
    }

    /// Sends the artifacts of `run` to the CI as `transfer`, on a thread of
    /// its own.
    pub fn upload(self: &Arc<Self>, run: String, transfer: u64) {
        let exec = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("zeughaus-enlist-upload".into())
            .spawn(move || {
                if let Err(e) = exec.upload_blocking(&run, transfer) {
                    eprintln!("[enlist] {run}: artifacts not sent: {e}");
                }
            });
        if let Err(e) = spawned {
            eprintln!("[enlist] cannot start the upload thread: {e}");
        }
    }

    fn upload_blocking(&self, run: &str, transfer: u64) -> Result<(), String> {
        let (tar, size) = match self.pack_set(run) {
            Ok(packed) => packed,
            Err(e) => {
                // The CI holds a download waiting for this transfer: tell it
                // now rather than let it time out.
                if let Err(report) = self
                    .handle
                    .block_on(self.send_unavailable(run, transfer, &e))
                {
                    eprintln!("[enlist] {run}: cannot report the set unavailable: {report}");
                }
                return Err(e);
            }
        };
        let sent = self
            .handle
            .block_on(self.send_tar(run, transfer, &tar, size));
        let _ = std::fs::remove_file(&tar);
        sent
    }

    /// Packs the set of `run` into a tar in transit. Returns it and its size.
    fn pack_set(&self, run: &str) -> Result<(PathBuf, u64), String> {
        if !safe_run(run) {
            return Err("not a run id".to_owned());
        }
        let dir = artifacts_dir(&self.state_dir).join(run);
        if !dir.is_dir() {
            return Err(format!("{} does not exist", dir.display()));
        }
        let tar = self.transit_tar(run);
        let packed = pack(&dir, &tar).and_then(|()| {
            let size = std::fs::metadata(&tar)
                .map_err(|e| format!("cannot stat {}: {e}", tar.display()))?
                .len();
            if size > MAX_ARTIFACT_BYTES {
                return Err(format!(
                    "{size} bytes is more than the {MAX_ARTIFACT_BYTES} allowed"
                ));
            }
            Ok(size)
        });
        match packed {
            Ok(size) => Ok((tar, size)),
            Err(e) => {
                let _ = std::fs::remove_file(&tar);
                Err(e)
            }
        }
    }

    /// Opens one exchange on the CI's artifact service.
    async fn open_exchange(&self) -> Result<(OutgoingTransfer, ReplyStream), String> {
        let requester = self.runtime.requester(self.tls.clone());
        let url = self.config.ci_url(ARTIFACTS_PATH)?;
        requester
            .connect(&url)
            .await
            .map_err(|e| format!("connect {url}: {e}"))?;
        requester
            .open(TransferMeta::default())
            .await
            .map_err(|e| format!("open: {e}"))
    }

    /// Tells the CI that `transfer` cannot be served.
    async fn send_unavailable(
        &self,
        run: &str,
        transfer: u64,
        message: &str,
    ) -> Result<(), String> {
        let (mut out, reply) = self.open_exchange().await?;
        let request = ArtifactRequest::Unavailable {
            run: run.to_owned(),
            transfer,
            message: message.to_owned(),
        };
        write_frame(&mut out, &request).await?;
        out.finish().map_err(|e| format!("finish: {e}"))?;
        let mut body = reply.recv().await.map_err(|e| format!("reply: {e}"))?;
        match read_frame::<_, ArtifactReply>(&mut body).await? {
            Some(ArtifactReply::Done) => Ok(()),
            Some(ArtifactReply::Error { message }) => Err(message),
            Some(ArtifactReply::Sending { .. }) | None => {
                Err("the CI did not confirm the report".to_owned())
            }
        }
    }

    async fn send_tar(
        &self,
        run: &str,
        transfer: u64,
        tar: &Path,
        size: u64,
    ) -> Result<(), String> {
        let mut file =
            std::fs::File::open(tar).map_err(|e| format!("cannot read {}: {e}", tar.display()))?;
        let (mut out, reply) = self.open_exchange().await?;
        let request = ArtifactRequest::Upload {
            run: run.to_owned(),
            transfer,
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
            Some(ArtifactReply::Done) => Ok(()),
            Some(ArtifactReply::Error { message }) => Err(message),
            Some(ArtifactReply::Sending { .. }) | None => {
                Err("the CI did not confirm the upload".to_owned())
            }
        }
    }

    /// Fetches the artifacts of `run` from the CI, which relays them from the
    /// runner that produced them, and unpacks them into `dest`. Called on the
    /// thread that prepares a job.
    pub fn download(&self, run: &str, dest: &Path) -> Result<(), String> {
        let tar = self.transit_tar(run);
        if let Some(parent) = tar.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let request = ArtifactRequest::Download {
            run: run.to_owned(),
        };
        let fetched = self
            .handle
            .block_on(self.receive_file(request, &tar))
            .and_then(|()| unpack(&tar, dest));
        let _ = std::fs::remove_file(&tar);
        fetched
    }

    /// Fetches bundle `index` of the try run `run` from the CI, which stored
    /// it when the run was uploaded, into the file `dest`. The directory of
    /// `dest` is the run's own and goes with it, so a failed transfer leaves
    /// nothing to clean up. Called on the thread that prepares a run.
    pub fn fetch_try_bundle(&self, run: &str, index: u32, dest: &Path) -> Result<(), String> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let request = ArtifactRequest::TryBundle {
            run: run.to_owned(),
            index,
        };
        self.handle.block_on(self.receive_file(request, dest))
    }

    /// Asks the CI's artifact service for what `request` names and streams
    /// the bytes it answers with into the file `dest`.
    async fn receive_file(&self, request: ArtifactRequest, dest: &Path) -> Result<(), String> {
        let (mut out, reply) = self.open_exchange().await?;
        write_frame(&mut out, &request).await?;
        out.finish().map_err(|e| format!("finish: {e}"))?;
        let mut body = reply.recv().await.map_err(|e| format!("reply: {e}"))?;
        let size = match read_frame::<_, ArtifactReply>(&mut body).await? {
            Some(ArtifactReply::Sending { size }) => size,
            Some(ArtifactReply::Error { message }) => return Err(message),
            Some(ArtifactReply::Done) | None => {
                return Err("the CI did not answer the request".to_owned());
            }
        };
        if size > MAX_ARTIFACT_BYTES {
            return Err(format!(
                "{size} bytes is more than the {MAX_ARTIFACT_BYTES} allowed"
            ));
        }
        let mut file = std::fs::File::create(dest)
            .map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
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
