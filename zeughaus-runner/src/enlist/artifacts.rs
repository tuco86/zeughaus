//! Artifact sets crossing to and from the CI.
//!
//! An artifact set lives on the runner that produced it, under
//! `ci/artifacts/<run>`, until the CI says `DropArtifacts`. A job that needs
//! another job's set and runs on another runner gets it relayed by the CI's
//! `/artifacts` service, which stores nothing: the consumer's `Download` makes
//! the CI ask the producer to `Upload` as one `transfer`, and the CI relays
//! those chunks to the consumer. One exchange per transfer: an
//! [`ArtifactRequest`] frame, then the chunks of the set in whichever
//! direction the request names (`zeughaus_link::enlist` has the framing).
//!
//! A set is never a file in transit. The producer's upload thread writes the
//! tar into a [`ChunkWriter`] and a task sends what it fills; the consumer's
//! task receives, and its thread unpacks from a [`ChunkReader`] as the chunks
//! arrive. A short queue sits between thread and task at both ends, so reading
//! and writing files overlaps with moving bytes, and neither runner needs the
//! disk space of a second copy.
//!
//! The bundles of a manual run come the same way from the CI itself, which
//! stored them (`ManualBundle`), straight into the run's own directory: they
//! go with it.

use std::io::{self, BufRead, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use weida::{OutgoingTransfer, ReplyStream, TransferMeta};
use zeughaus_link::ARTIFACTS_PATH;
use zeughaus_link::enlist::{
    ARTIFACT_CHUNK, ArtifactReply, ArtifactRequest, FromRunner, MAX_ARTIFACT_BYTES, TransferEvent,
    read_chunk, read_frame, write_chunk, write_end, write_frame,
};

use super::cleanup::remove_tree;
use super::run::safe_run;
use super::{Executor, artifacts_dir};

/// Chunks that wait between the thread that reads or writes files and the
/// task that moves the bytes.
const QUEUE: usize = 4;

/// What the thread that packs a set hands to the task that sends it.
enum Piece {
    Bytes(Vec<u8>),
    /// The tar is complete.
    End,
}

/// What the task that receives a set hands to the thread that unpacks it.
enum Received {
    Bytes(Vec<u8>),
    /// The end marker arrived.
    End,
}

impl Executor {
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

    /// Packs the set on this thread while a task sends what is packed.
    fn upload_blocking(self: &Arc<Self>, run: &str, transfer: u64) -> Result<(), String> {
        let dir = match self.set_dir(run) {
            Ok(dir) => dir,
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
        let (tx, rx) = mpsc::channel(QUEUE);
        let exec = Arc::clone(self);
        let name = run.to_owned();
        let sender = self
            .handle
            .spawn(async move { exec.send_stream(&name, transfer, rx).await });
        let packed = pack(&dir, tx);
        let sent = self
            .handle
            .block_on(sender)
            .unwrap_or_else(|e| Err(format!("the sending task failed: {e}")));
        match packed {
            Ok(()) => sent,
            // The writer fails like this only when the sender stopped first,
            // and the sender's error says why.
            Err(e) if e.kind() == io::ErrorKind::BrokenPipe => {
                sent.and(Err(format!("cannot pack {}: {e}", dir.display())))
            }
            Err(e) => Err(format!("cannot pack {}: {e}", dir.display())),
        }
    }

    /// The directory of `run`'s artifact set.
    fn set_dir(&self, run: &str) -> Result<PathBuf, String> {
        if !safe_run(run) {
            return Err("not a run id".to_owned());
        }
        let dir = artifacts_dir(&self.state_dir).join(run);
        if !dir.is_dir() {
            return Err(format!("{} does not exist", dir.display()));
        }
        Ok(dir)
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
        verdict(reply, "report").await
    }

    /// Opens the exchange for `transfer` and sends what `pieces` yields as
    /// the chunks of the upload, up to the end of the set.
    async fn send_stream(
        &self,
        run: &str,
        transfer: u64,
        mut pieces: mpsc::Receiver<Piece>,
    ) -> Result<(), String> {
        let (mut out, reply) = self.open_exchange().await?;
        let request = ArtifactRequest::Upload {
            run: run.to_owned(),
            transfer,
        };
        write_frame(&mut out, &request).await?;
        let mut ended = false;
        while let Some(piece) = pieces.recv().await {
            match piece {
                Piece::Bytes(bytes) => write_chunk(&mut out, &bytes).await?,
                Piece::End => {
                    write_end(&mut out).await?;
                    ended = true;
                    break;
                }
            }
        }
        // A set that was never completed is finished without its end marker:
        // the CI answers it and the download that waits for it sees the break.
        out.finish().map_err(|e| format!("finish: {e}"))?;
        let confirmed = verdict(reply, "upload").await;
        if !ended {
            return Err("packing the set failed".to_owned());
        }
        confirmed
    }

    /// Fetches the set of `run` into `dest` as the input `need` of the job
    /// `consumer`, and tells the CI how it went: the transfer is what an
    /// editor shows of a job that waits for its inputs. Returns the bytes
    /// received.
    pub(super) fn fetch_input(
        self: &Arc<Self>,
        consumer: &str,
        need: &str,
        run: &str,
        dest: &Path,
    ) -> Result<u64, String> {
        let report = |event| {
            self.send(FromRunner::Transfer {
                run: consumer.to_owned(),
                need: need.to_owned(),
                event,
            });
        };
        report(TransferEvent::Started);
        let fetched = self.download(run, dest);
        report(match &fetched {
            Ok(bytes) => TransferEvent::Done { bytes: *bytes },
            Err(message) => TransferEvent::Failed {
                message: message.clone(),
            },
        });
        fetched
    }

    /// Fetches the artifacts of `run` from the CI, which relays them from the
    /// runner that produced them, and unpacks them into `dest` as they
    /// arrive. Returns the bytes received. Called on the thread that prepares
    /// a job.
    pub fn download(self: &Arc<Self>, run: &str, dest: &Path) -> Result<u64, String> {
        let request = ArtifactRequest::Download {
            run: run.to_owned(),
        };
        let (mut reader, task) = self.receive(request);
        let unpacked = unpack(&mut reader, dest);
        let bytes = reader.bytes;
        drop(reader);
        self.finish_receiving(task);
        unpacked.map(|()| bytes)
    }

    /// Fetches bundle `index` of the manual run `run` from the CI, which stored
    /// it when the run was uploaded, into the file `dest`. The directory of
    /// `dest` is the run's own and goes with it, so a failed transfer leaves
    /// nothing to clean up. Called on the thread that prepares a run.
    pub fn fetch_manual_bundle(
        self: &Arc<Self>,
        run: &str,
        index: u32,
        dest: &Path,
    ) -> Result<(), String> {
        if let Some(parent) = dest.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("cannot create {}: {e}", parent.display()))?;
        }
        let request = ArtifactRequest::ManualBundle {
            run: run.to_owned(),
            index,
        };
        let (mut reader, task) = self.receive(request);
        let saved = save(&mut reader, dest);
        drop(reader);
        self.finish_receiving(task);
        saved
    }

    /// Starts the exchange for `request` on a task of the runtime. Returns the
    /// reader of what the CI answers with, and the task that feeds it.
    fn receive(self: &Arc<Self>, request: ArtifactRequest) -> (ChunkReader, JoinHandle<()>) {
        let (tx, rx) = mpsc::channel(QUEUE);
        let exec = Arc::clone(self);
        let task = self
            .handle
            .spawn(async move { exec.receive_stream(request, tx).await });
        (ChunkReader::new(rx), task)
    }

    /// Ends the task of [`Executor::receive`] and waits until it is gone and
    /// its streams are released. Its reader is done: it has the whole set, or
    /// it failed, and then the task may be waiting for the CI, which can take
    /// minutes, for nothing. A task that failed told the reader through its
    /// queue, so the result of the join has nothing to add.
    fn finish_receiving(&self, task: JoinHandle<()>) {
        task.abort();
        let _ = self.handle.block_on(task);
    }

    /// Asks the CI for what `request` names and passes its chunks on to the
    /// reader on `tx`. Whatever goes wrong is passed on too, as the last
    /// message.
    async fn receive_stream(
        &self,
        request: ArtifactRequest,
        tx: mpsc::Sender<Result<Received, String>>,
    ) {
        if let Err(message) = self.pass_chunks(request, &tx).await {
            let _ = tx.send(Err(message)).await;
        }
    }

    async fn pass_chunks(
        &self,
        request: ArtifactRequest,
        tx: &mpsc::Sender<Result<Received, String>>,
    ) -> Result<(), String> {
        let (mut out, reply) = self.open_exchange().await?;
        write_frame(&mut out, &request).await?;
        out.finish().map_err(|e| format!("finish: {e}"))?;
        let mut body = reply.recv().await.map_err(|e| format!("reply: {e}"))?;
        match read_frame::<_, ArtifactReply>(&mut body).await? {
            Some(ArtifactReply::Sending) => {}
            Some(ArtifactReply::Error { message }) => return Err(message),
            Some(ArtifactReply::Done) | None => {
                return Err("the CI did not answer the request".to_owned());
            }
        }
        let mut chunk = Vec::new();
        let mut total = 0u64;
        while read_chunk(&mut body, &mut chunk).await? {
            total += chunk.len() as u64;
            if total > MAX_ARTIFACT_BYTES {
                return Err(format!("more than {MAX_ARTIFACT_BYTES} bytes"));
            }
            if tx
                .send(Ok(Received::Bytes(std::mem::take(&mut chunk))))
                .await
                .is_err()
            {
                // The reader is gone: nobody wants the rest.
                return Ok(());
            }
        }
        let _ = tx.send(Ok(Received::End)).await;
        Ok(())
    }
}

/// Reads the CI's verdict on an upload or on an `Unavailable`.
async fn verdict(reply: ReplyStream, what: &str) -> Result<(), String> {
    let mut body = reply.recv().await.map_err(|e| format!("reply: {e}"))?;
    match read_frame::<_, ArtifactReply>(&mut body).await? {
        Some(ArtifactReply::Done) => Ok(()),
        Some(ArtifactReply::Error { message }) => Err(message),
        Some(ArtifactReply::Sending) | None => Err(format!("the CI did not confirm the {what}")),
    }
}

/// Packs the contents of `dir` as a tar into the chunks of `tx`, ending with
/// the end of the set. Blocks while the sender is behind, so it runs on a
/// thread, never in a task.
fn pack(dir: &Path, tx: mpsc::Sender<Piece>) -> io::Result<()> {
    let mut builder = tar::Builder::new(ChunkWriter::new(tx));
    builder.append_dir_all(".", dir)?;
    // Finishes the tar: its two closing blocks.
    builder.into_inner()?.finish()
}

/// Unpacks the set that `reader` yields into `dest`, which is created, and
/// reads on to its end marker. Paths that would leave `dest` are refused by
/// the tar reader.
fn unpack(reader: &mut ChunkReader, dest: &Path) -> Result<(), String> {
    let failed = |e: io::Error| format!("cannot unpack into {}: {}", dest.display(), causes(&e));
    std::fs::create_dir_all(dest).map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    tar::Archive::new(&mut *reader)
        .unpack(dest)
        .map_err(failed)?;
    // The tar reader stops at the first of the two closing blocks; what is
    // left of the set, and the end marker, are still to come.
    reader.drain().map_err(failed)
}

/// Writes the set that `reader` yields, byte for byte, into the file `dest`.
fn save(reader: &mut ChunkReader, dest: &Path) -> Result<(), String> {
    let mut file = std::fs::File::create(dest)
        .map_err(|e| format!("cannot create {}: {e}", dest.display()))?;
    loop {
        let chunk = reader.fill_buf().map_err(|e| e.to_string())?;
        if chunk.is_empty() {
            return Ok(());
        }
        file.write_all(chunk)
            .map_err(|e| format!("cannot write {}: {e}", dest.display()))?;
        let n = chunk.len();
        reader.consume(n);
    }
}

/// `e` and the causes below it: the tar reader names what it was doing and
/// keeps the reason out of its own words.
fn causes(e: &io::Error) -> String {
    let mut text = e.to_string();
    let mut cause = std::error::Error::source(e);
    while let Some(inner) = cause {
        text = format!("{text}: {inner}");
        cause = inner.source();
    }
    text
}

/// Receives the tar a [`tar::Builder`] writes, cuts it into chunks of
/// [`ARTIFACT_CHUNK`] bytes and hands them to the task that sends them. Where
/// a chunk ends is not the tar's business: a write fills the chunk that is
/// open and starts the next.
struct ChunkWriter {
    tx: mpsc::Sender<Piece>,
    buf: Vec<u8>,
    /// Bytes written so far.
    total: u64,
    /// The most bytes it accepts.
    limit: u64,
}

impl ChunkWriter {
    fn new(tx: mpsc::Sender<Piece>) -> ChunkWriter {
        ChunkWriter {
            tx,
            buf: Vec::with_capacity(ARTIFACT_CHUNK),
            total: 0,
            limit: MAX_ARTIFACT_BYTES,
        }
    }

    /// Hands on the chunk that is open and ends the set.
    fn finish(mut self) -> io::Result<()> {
        if !self.buf.is_empty() {
            let last = std::mem::take(&mut self.buf);
            self.send(Piece::Bytes(last))?;
        }
        self.send(Piece::End)
    }

    fn send(&self, piece: Piece) -> io::Result<()> {
        self.tx
            .blocking_send(piece)
            .map_err(|_| io::Error::new(io::ErrorKind::BrokenPipe, "the transfer was abandoned"))
    }
}

impl Write for ChunkWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.total += bytes.len() as u64;
        if self.total > self.limit {
            return Err(io::Error::other(format!("more than {} bytes", self.limit)));
        }
        let mut rest = bytes;
        while !rest.is_empty() {
            let take = rest.len().min(ARTIFACT_CHUNK - self.buf.len());
            self.buf.extend_from_slice(&rest[..take]);
            rest = &rest[take..];
            if self.buf.len() == ARTIFACT_CHUNK {
                let full = std::mem::replace(&mut self.buf, Vec::with_capacity(ARTIFACT_CHUNK));
                self.send(Piece::Bytes(full))?;
            }
        }
        Ok(bytes.len())
    }

    /// The open chunk waits for more bytes until [`ChunkWriter::finish`].
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

/// Reads the set from the chunks a receiving task passes on, on the calling
/// thread, which blocks until the next one arrives. It ends only at the end
/// marker: a task that failed, or went away before it, is an error, so a
/// truncated set is never taken for a complete one.
struct ChunkReader {
    rx: mpsc::Receiver<Result<Received, String>>,
    /// The chunk being read, and how much of it was.
    cur: Vec<u8>,
    pos: usize,
    /// The end marker arrived.
    ended: bool,
    /// Bytes received so far.
    bytes: u64,
}

impl ChunkReader {
    fn new(rx: mpsc::Receiver<Result<Received, String>>) -> ChunkReader {
        ChunkReader {
            rx,
            cur: Vec::new(),
            pos: 0,
            ended: false,
            bytes: 0,
        }
    }

    /// Reads and drops what is left of the set, up to its end marker.
    fn drain(&mut self) -> io::Result<()> {
        loop {
            let n = self.fill_buf()?.len();
            if n == 0 {
                return Ok(());
            }
            self.consume(n);
        }
    }
}

impl BufRead for ChunkReader {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        while self.pos == self.cur.len() {
            if self.ended {
                return Ok(&[]);
            }
            match self.rx.blocking_recv() {
                Some(Ok(Received::Bytes(bytes))) => {
                    self.bytes += bytes.len() as u64;
                    self.cur = bytes;
                    self.pos = 0;
                }
                Some(Ok(Received::End)) => self.ended = true,
                Some(Err(message)) => return Err(io::Error::other(message)),
                None => return Err(io::Error::other("the transfer ended early")),
            }
        }
        Ok(&self.cur[self.pos..])
    }

    fn consume(&mut self, n: usize) {
        self.pos = (self.pos + n).min(self.cur.len());
    }
}

impl Read for ChunkReader {
    fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
        let available = self.fill_buf()?;
        let n = available.len().min(out.len());
        out[..n].copy_from_slice(&available[..n]);
        self.consume(n);
        Ok(n)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Bytes that differ along their length, so a chunk boundary that loses
    /// or repeats one shows.
    fn pattern(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 251) as u8).collect()
    }

    #[test]
    fn the_writer_cuts_the_tar_into_full_chunks() {
        let (tx, mut rx) = mpsc::channel(8);
        let data = pattern(2 * ARTIFACT_CHUNK + 5);
        let mut writer = ChunkWriter::new(tx);
        // Writes that straddle the chunk boundaries.
        for part in data.chunks(300_000) {
            writer.write_all(part).unwrap();
        }
        writer.finish().unwrap();

        let (mut sizes, mut joined, mut ended) = (Vec::new(), Vec::new(), false);
        while let Some(piece) = rx.blocking_recv() {
            match piece {
                Piece::Bytes(bytes) => {
                    sizes.push(bytes.len());
                    joined.extend_from_slice(&bytes);
                }
                Piece::End => ended = true,
            }
        }
        assert_eq!(sizes, [ARTIFACT_CHUNK, ARTIFACT_CHUNK, 5]);
        assert_eq!(joined, data);
        assert!(ended);
    }

    #[test]
    fn the_writer_refuses_a_set_over_its_limit_and_one_nobody_takes() {
        let (tx, _rx) = mpsc::channel(QUEUE);
        let mut writer = ChunkWriter::new(tx);
        writer.limit = 10;
        writer.write_all(b"0123456789").unwrap();
        let err = writer.write_all(b"!").unwrap_err();
        assert!(err.to_string().contains("more than 10 bytes"), "{err}");

        // The receiving task failed and dropped its queue: the next chunk
        // that fills up finds nobody.
        let (tx, rx) = mpsc::channel(QUEUE);
        drop(rx);
        let mut writer = ChunkWriter::new(tx);
        let err = writer.write_all(&vec![0u8; ARTIFACT_CHUNK]).unwrap_err();
        assert_eq!(err.kind(), std::io::ErrorKind::BrokenPipe);
    }

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`).
    mod io {
        use super::*;

        fn scratch(name: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("zci-art-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        /// The tar of a small set, closing blocks included.
        fn tar_of(dir: &Path) -> Vec<u8> {
            let mut bytes = Vec::new();
            let mut builder = tar::Builder::new(&mut bytes);
            builder.append_dir_all(".", dir).unwrap();
            builder.finish().unwrap();
            drop(builder);
            bytes
        }

        /// A set whose big file crosses two chunk boundaries.
        fn make_set(dir: &Path) {
            std::fs::create_dir_all(dir.join("sub/deeper")).unwrap();
            std::fs::write(dir.join("a"), "one").unwrap();
            std::fs::write(dir.join("sub/b"), "two").unwrap();
            let big = pattern(2 * ARTIFACT_CHUNK + 12_345);
            std::fs::write(dir.join("sub/deeper/big"), big).unwrap();
        }

        /// Carries the pieces of a [`ChunkWriter`] through the chunk framing, as
        /// the CI's relay and a receiving task do, to the queue of a
        /// [`ChunkReader`]. The stream between them is small, so the sender has
        /// to wait for the reader.
        fn through_the_wire(
            rt: &tokio::runtime::Runtime,
            mut pieces: mpsc::Receiver<Piece>,
        ) -> mpsc::Receiver<Result<Received, String>> {
            let (tx, rx) = mpsc::channel(QUEUE);
            rt.spawn(async move {
                let (mut near, mut far) = tokio::io::duplex(64 << 10);
                let sending = async move {
                    while let Some(piece) = pieces.recv().await {
                        match piece {
                            Piece::Bytes(bytes) => write_chunk(&mut near, &bytes).await.unwrap(),
                            Piece::End => {
                                write_end(&mut near).await.unwrap();
                                break;
                            }
                        }
                    }
                };
                let receiving = async move {
                    let mut chunk = Vec::new();
                    loop {
                        let next = match read_chunk(&mut far, &mut chunk).await {
                            Ok(true) => Ok(Received::Bytes(std::mem::take(&mut chunk))),
                            Ok(false) => Ok(Received::End),
                            Err(e) => Err(e),
                        };
                        let last = !matches!(next, Ok(Received::Bytes(_)));
                        tx.send(next).await.unwrap();
                        if last {
                            break;
                        }
                    }
                };
                tokio::join!(sending, receiving);
            });
            rx
        }

        #[test]
        fn an_artifact_set_survives_a_round_trip_through_the_chunks() {
            let base = scratch("round-trip");
            let from = base.join("from");
            make_set(&from);
            let rt = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(2)
                .enable_all()
                .build()
                .unwrap();

            let (tx, pieces) = mpsc::channel(QUEUE);
            let packer = std::thread::spawn({
                let from = from.clone();
                move || pack(&from, tx)
            });
            let mut reader = ChunkReader::new(through_the_wire(&rt, pieces));
            let to = base.join("to");
            unpack(&mut reader, &to).unwrap();
            packer.join().unwrap().unwrap();

            assert_eq!(std::fs::read(to.join("a")).unwrap(), b"one");
            assert_eq!(std::fs::read(to.join("sub/b")).unwrap(), b"two");
            assert_eq!(
                std::fs::read(to.join("sub/deeper/big")).unwrap(),
                pattern(2 * ARTIFACT_CHUNK + 12_345)
            );
            assert!(reader.ended, "the end marker was read");
            assert!(reader.bytes > 2 * ARTIFACT_CHUNK as u64);
            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn a_set_that_stops_before_its_end_marker_is_not_a_set() {
            let base = scratch("short");
            let from = base.join("from");
            make_set(&from);
            // Every file is there; the tar's closing blocks and the end marker
            // are not. A reader that took the end of the stream for the end of
            // the set would accept this.
            let mut tar = tar_of(&from);
            tar.truncate(tar.len() - 1024);

            let (tx, rx) = mpsc::channel(QUEUE);
            tx.blocking_send(Ok(Received::Bytes(tar))).unwrap();
            drop(tx);
            let to = base.join("to");
            let err = unpack(&mut ChunkReader::new(rx), &to).unwrap_err();
            assert!(err.contains("the transfer ended early"), "{err}");
            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn what_the_ci_says_is_the_reason_the_set_did_not_arrive() {
            let base = scratch("refused");
            let (tx, rx) = mpsc::channel(QUEUE);
            tx.blocking_send(Err("runner x: view.7.a left no artifacts".to_owned()))
                .unwrap();
            let err = unpack(&mut ChunkReader::new(rx), &base.join("to")).unwrap_err();
            assert!(err.contains("view.7.a left no artifacts"), "{err}");

            // Also when the failure comes in the middle of a file, where the tar
            // reader wraps it.
            let from = base.join("from");
            make_set(&from);
            let tar = tar_of(&from);
            let (tx, rx) = mpsc::channel(QUEUE);
            tx.blocking_send(Ok(Received::Bytes(tar[..ARTIFACT_CHUNK].to_vec())))
                .unwrap();
            tx.blocking_send(Err("the CI went away".to_owned()))
                .unwrap();
            let err = unpack(&mut ChunkReader::new(rx), &base.join("to2")).unwrap_err();
            assert!(err.contains("the CI went away"), "{err}");
            let _ = std::fs::remove_dir_all(&base);
        }

        #[test]
        fn a_bundle_is_written_chunk_by_chunk() {
            let base = scratch("bundle");
            let bundle = pattern(ARTIFACT_CHUNK + 7);
            let (tx, rx) = mpsc::channel(QUEUE);
            tx.blocking_send(Ok(Received::Bytes(bundle[..ARTIFACT_CHUNK].to_vec())))
                .unwrap();
            tx.blocking_send(Ok(Received::Bytes(bundle[ARTIFACT_CHUNK..].to_vec())))
                .unwrap();
            tx.blocking_send(Ok(Received::End)).unwrap();
            let dest = base.join("bundle");
            save(&mut ChunkReader::new(rx), &dest).unwrap();
            assert_eq!(std::fs::read(&dest).unwrap(), bundle);

            // Without its end marker the file is no bundle.
            let (tx, rx) = mpsc::channel(QUEUE);
            tx.blocking_send(Ok(Received::Bytes(bundle))).unwrap();
            drop(tx);
            assert!(save(&mut ChunkReader::new(rx), &dest).is_err());
            let _ = std::fs::remove_dir_all(&base);
        }
    }
}
