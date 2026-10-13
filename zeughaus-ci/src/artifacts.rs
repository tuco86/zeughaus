//! The `/artifacts` service: runners hand a finished job's output directory
//! to the CI as a tar when a job on another runner needs it, and fetch it
//! again for that job.
//!
//! One exchange moves one tar. The request is one frame
//! ([`ArtifactRequest`]); an upload's bytes follow it, a download's follow
//! the reply frame ([`ArtifactReply`]).

use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use weida::{IncomingRequest, Replier, TransferMeta};
use zeughaus_link::enlist::{
    ArtifactReply, ArtifactRequest, MAX_ARTIFACT_BYTES, parse_run_id, read_frame, write_frame,
};

use crate::config::{CiConfig, valid_name};
use crate::scheduler::{artifacts_dir, load_pipeline};
use crate::transport::principal_of;

/// Bytes moved per read or write.
const CHUNK: usize = 256 << 10;

pub struct ArtifactService {
    state_dir: PathBuf,
    config: Arc<CiConfig>,
}

impl ArtifactService {
    pub fn new(state_dir: PathBuf, config: Arc<CiConfig>) -> ArtifactService {
        ArtifactService { state_dir, config }
    }

    /// Accepts exchanges until the replier goes away, which for this process
    /// means never.
    pub async fn accept(self: Arc<Self>, replier: Replier) {
        loop {
            match replier.accept().await {
                Ok(request) => {
                    let service = Arc::clone(&self);
                    tokio::spawn(async move { service.exchange(request).await });
                }
                Err(e) => {
                    eprintln!("[ci] stopped accepting artifacts: {e}");
                    return;
                }
            }
        }
    }

    /// The tar of a run's output, for a run the pipeline records know. The
    /// name came from a peer and is joined into a path, so it is checked
    /// before and against the records after.
    fn tar_of(&self, run: &str) -> Result<(PathBuf, Option<String>), String> {
        let (repo, number, job) = parse_run_id(run).ok_or_else(|| format!("{run} is not a run"))?;
        if !valid_name(&repo) || !valid_name(&job) {
            return Err(format!("{run} is not a run"));
        }
        let pipeline = load_pipeline(&self.state_dir, &repo, number)
            .map_err(|_| format!("{run} is not a known run"))?;
        let record = pipeline
            .jobs
            .iter()
            .find(|j| j.def.name == job)
            .ok_or_else(|| format!("{run} is not a known run"))?;
        Ok((
            tar_path(&self.state_dir, &repo, number, &job),
            record.runner.clone(),
        ))
    }

    async fn exchange(&self, mut request: IncomingRequest) {
        let runner = principal_of(request.meta()).and_then(|principal| {
            self.config
                .runner_of_fingerprint(&principal)
                .map(str::to_owned)
        });
        let Some(runner) = runner else {
            eprintln!("[ci] refused an artifact exchange from a peer that is no runner");
            request.refuse(weida::ErrorCode::Rejected).await;
            return;
        };
        let mut body = request.take_body();
        let first = match read_frame::<_, ArtifactRequest>(&mut body).await {
            Ok(Some(first)) => first,
            Ok(None) => return,
            Err(e) => {
                eprintln!("[ci] {runner}: unreadable artifact request: {e}");
                request.refuse(weida::ErrorCode::Rejected).await;
                return;
            }
        };
        let mut reply = match request.reply(TransferMeta::default()).await {
            Ok(reply) => reply,
            Err(e) => {
                eprintln!("[ci] {runner}: cannot open the reply half: {e}");
                return;
            }
        };
        let outcome = match first {
            ArtifactRequest::Upload { run, size } => {
                let stored = self.upload(&runner, &run, size, &mut body).await;
                match stored {
                    Ok(()) => write_frame(&mut reply, &ArtifactReply::Stored).await,
                    Err(message) => {
                        eprintln!("[ci] {runner}: upload of {run} refused: {message}");
                        write_frame(&mut reply, &ArtifactReply::Error { message }).await
                    }
                }
            }
            ArtifactRequest::Download { run } => self.download(&run, &mut reply).await,
        };
        if let Err(e) = outcome {
            eprintln!("[ci] {runner}: artifact exchange failed: {e}");
        }
        let _ = reply.finish();
    }

    async fn upload(
        &self,
        runner: &str,
        run: &str,
        size: u64,
        body: &mut weida::IncomingTransfer,
    ) -> Result<(), String> {
        if size > MAX_ARTIFACT_BYTES {
            return Err(format!("{size} bytes is more than {MAX_ARTIFACT_BYTES}"));
        }
        let (tar, owner) = self.tar_of(run)?;
        if owner.as_deref() != Some(runner) {
            return Err(format!("{run} did not run on {runner}"));
        }
        if let Some(parent) = tar.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let part = part_path(&tar);
        let result = receive(body, size, &part).await;
        if result.is_err() {
            let _ = tokio::fs::remove_file(&part).await;
            return result;
        }
        tokio::fs::rename(&part, &tar)
            .await
            .map_err(|e| format!("cannot store {}: {e}", tar.display()))
    }

    async fn download(&self, run: &str, reply: &mut weida::OutgoingTransfer) -> Result<(), String> {
        let opened = match self.tar_of(run) {
            Ok((tar, _)) => tokio::fs::File::open(&tar)
                .await
                .map_err(|_| format!("{run} has no artifacts here")),
            Err(message) => Err(message),
        };
        let mut file = match opened {
            Ok(file) => file,
            Err(message) => return write_frame(reply, &ArtifactReply::Error { message }).await,
        };
        let size = file
            .metadata()
            .await
            .map_err(|e| format!("cannot stat {run}: {e}"))?
            .len();
        write_frame(reply, &ArtifactReply::Sending { size }).await?;
        let mut buf = vec![0u8; CHUNK];
        let mut sent = 0u64;
        while sent < size {
            let n = file
                .read(&mut buf)
                .await
                .map_err(|e| format!("cannot read {run}: {e}"))?;
            if n == 0 {
                return Err(format!("{run} shrank while it was sent"));
            }
            reply
                .write_all(&buf[..n])
                .await
                .map_err(|e| format!("cannot send {run}: {e}"))?;
            sent += n as u64;
        }
        Ok(())
    }
}

/// `<n>.artifacts/<job>.tar` of pipeline `number`.
pub fn tar_path(state_dir: &Path, repo: &str, number: u64, job: &str) -> PathBuf {
    artifacts_dir(state_dir, repo, number).join(format!("{job}.tar"))
}

fn part_path(tar: &Path) -> PathBuf {
    let mut part = tar.as_os_str().to_owned();
    part.push(".part");
    PathBuf::from(part)
}

/// Writes exactly `size` bytes of `body` to `part`; a body that ends short
/// is an error.
async fn receive(
    body: &mut (impl AsyncReadExt + Unpin),
    size: u64,
    part: &Path,
) -> Result<(), String> {
    let mut file = tokio::fs::File::create(part)
        .await
        .map_err(|e| format!("cannot create {}: {e}", part.display()))?;
    let mut buf = vec![0u8; CHUNK];
    let mut left = size;
    while left > 0 {
        let want = usize::try_from(left).map_or(CHUNK, |left| left.min(CHUNK));
        let n = body
            .read(&mut buf[..want])
            .await
            .map_err(|e| format!("upload failed: {e}"))?;
        if n == 0 {
            return Err(format!("upload ended {left} bytes short"));
        }
        file.write_all(&buf[..n])
            .await
            .map_err(|e| format!("cannot write {}: {e}", part.display()))?;
        left -= n as u64;
    }
    file.flush()
        .await
        .map_err(|e| format!("cannot write {}: {e}", part.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tar_sits_in_the_pipelines_artifact_directory() {
        let tar = tar_path(Path::new("/s"), "repo", 4, "build");
        assert!(tar.ends_with("4.artifacts/build.tar"), "{}", tar.display());
        assert!(part_path(&tar).ends_with("build.tar.part"));
    }

    #[tokio::test]
    async fn a_short_upload_is_an_error_and_a_full_one_is_exact() {
        let dir = std::env::temp_dir().join(format!("zeughaus-artifacts-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create");
        let part = dir.join("a.tar.part");

        let mut body: &[u8] = b"0123456789";
        assert!(receive(&mut body, 12, &part).await.is_err());

        let mut body: &[u8] = b"0123456789";
        receive(&mut body, 6, &part).await.expect("receive");
        assert_eq!(std::fs::read(&part).expect("read"), b"012345");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
