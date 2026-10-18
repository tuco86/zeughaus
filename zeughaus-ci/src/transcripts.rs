//! Job transcripts as read-only terminals, served on `/mux`.
//!
//! A transcript is a [`Session::fed`] screen that a follower thread feeds
//! from the stored copy of the run's WAL (`<n>.runs/<job>/wal`, appended by
//! `/enlist`). The CI has no workspace and no shells: its mux answers the
//! control exchange with an empty topology, streams the screens it holds,
//! and ignores input except a resize, which rewraps the screen to the pane
//! that asked last (one screen is shared by every editor that shows it). At
//! most [`MAX_TRANSCRIPTS`] screens stay open across all editors; opening one
//! more closes the one used longest ago, which is cheap to open again.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use weida::{IncomingRequest, Replier, TransferMeta};
use zeughaus_mux::message::ControlAttached;
use zeughaus_mux::{
    Capability, CommandOutcome, CommandReply, ControlAttach, Dimensions, ErrorCode, ExitState,
    MAJOR, MINOR, Message, RowFetch, RunnerIncarnation, ServerHello, TerminalAttach,
    TerminalCommand, TerminalId, WireError, WorkspaceSnapshot,
};
use zeughaus_terminal::Session;
use zeughaus_terminal::frames::{ReadError, read_frame, write_frame};
use zeughaus_terminal::serve::{fetch_rows, stream_terminal};
use zeughaus_terminal::wal::Follower;

use crate::config::CiConfig;
use crate::scheduler::transcript_dir;
use crate::transport::editor_principal;

/// Transcript screens kept open across all editors.
pub const MAX_TRANSCRIPTS: usize = 16;

/// Scrollback of a transcript: a build log is long and the screen is the
/// only copy an editor scrolls.
const SCROLLBACK_ROWS: usize = 10_000;

/// How often a follower looks for new output.
const POLL: Duration = Duration::from_millis(200);

/// A job: its repository, its pipeline's number and its name.
type JobKey = (String, u64, String);

struct Open {
    key: JobKey,
    id: TerminalId,
    session: Session,
    /// Tells the follower thread to stop; set when the screen is closed.
    stop: Arc<AtomicBool>,
}

impl Drop for Open {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
    }
}

#[derive(Default)]
struct State {
    /// The screens open now, the one used longest ago first.
    open: Vec<Open>,
    next: u64,
}

/// The transcript screens this process holds.
pub struct Transcripts {
    state_dir: PathBuf,
    incarnation: RunnerIncarnation,
    state: Mutex<State>,
}

impl Transcripts {
    pub fn new(state_dir: PathBuf) -> Transcripts {
        Transcripts {
            state_dir,
            incarnation: incarnation(),
            state: Mutex::new(State::default()),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// The screen of a job's transcript, opened now unless it is open
    /// already. The names must be valid: they are joined into a path.
    pub fn open(&self, repo: &str, number: u64, job: &str) -> TerminalId {
        let key = (repo.to_owned(), number, job.to_owned());
        let mut state = self.lock();
        if let Some(at) = state.open.iter().position(|o| o.key == key) {
            let entry = state.open.remove(at);
            let id = entry.id;
            state.open.push(entry);
            return id;
        }
        if state.open.len() >= MAX_TRANSCRIPTS {
            state.open.remove(0);
        }
        state.next += 1;
        let id = TerminalId(state.next);
        let session = Session::fed(
            id,
            Dimensions { cols: 80, rows: 24 },
            SCROLLBACK_ROWS,
            &format!("{repo} #{number} {job}"),
        );
        let stop = Arc::new(AtomicBool::new(false));
        follow(
            transcript_dir(&self.state_dir, repo, number, job),
            session.clone(),
            Arc::clone(&stop),
        );
        state.open.push(Open {
            key,
            id,
            session,
            stop,
        });
        id
    }

    /// Closes a screen. One that is closed already is as closed as asked.
    pub fn close(&self, id: TerminalId) {
        let mut state = self.lock();
        if let Some(at) = state.open.iter().position(|o| o.id == id) {
            let closed = state.open.remove(at);
            closed.session.kill();
        }
    }

    pub fn session(&self, id: TerminalId) -> Option<Session> {
        self.lock()
            .open
            .iter()
            .find(|o| o.id == id)
            .map(|o| o.session.clone())
    }

    pub fn exists(&self, id: TerminalId) -> bool {
        self.lock().open.iter().any(|o| o.id == id)
    }
}

/// A fresh incarnation for this process: sixteen random bytes with no crate
/// to draw them from. `RandomState` is seeded from the OS per instance, so
/// hashing a constant through two of them is two independent draws. Not a
/// key; it names a process run, and only has to differ from every other run
/// an editor may have cached.
fn incarnation() -> RunnerIncarnation {
    use std::hash::{BuildHasher, RandomState};
    let a = RandomState::new().hash_one(0u8).to_le_bytes();
    let b = RandomState::new().hash_one(1u8).to_le_bytes();
    let mut bytes = [0u8; 16];
    bytes[..8].copy_from_slice(&a);
    bytes[8..].copy_from_slice(&b);
    RunnerIncarnation::from_bytes(bytes)
}

/// The exit a transcript's `exit` file records: `code=<n>` or `code=none`.
fn read_exit(dir: &Path) -> Option<ExitState> {
    let text = std::fs::read_to_string(dir.join("exit")).ok()?;
    let code = text.trim().strip_prefix("code=")?;
    Some(match code.parse::<u32>() {
        Ok(code) => ExitState::Exited { code },
        // No code: the run was killed or never reported one.
        Err(_) => ExitState::Killed,
    })
}

/// Feeds `session` from the run's WAL until the run's exit is recorded and
/// the WAL is drained, or `stop` is set.
///
/// The exit is read before the drain: output is stored before the exit is
/// written, so a drain that finds nothing after an exit was seen is the end.
fn follow(dir: PathBuf, session: Session, stop: Arc<AtomicBool>) {
    let spawned = std::thread::Builder::new()
        .name("zeughaus-ci-transcript".to_owned())
        .spawn(move || {
            let wal = dir.join("wal");
            let mut follower: Option<Follower> = None;
            loop {
                if stop.load(Ordering::SeqCst) {
                    return;
                }
                let exit = read_exit(&dir);
                if follower.is_none() {
                    follower = Follower::open(&wal).ok();
                }
                let mut fed = false;
                if let Some(follower) = &mut follower {
                    match follower.next_records() {
                        Ok(records) => {
                            for record in records {
                                session.feed(&record.bytes);
                                fed = true;
                            }
                        }
                        Err(e) => {
                            eprintln!("[ci] cannot follow {}: {e}", wal.display());
                            return;
                        }
                    }
                }
                if !fed && let Some(exit) = exit {
                    session.finish(exit);
                    return;
                }
                std::thread::sleep(POLL);
            }
        });
    if let Err(e) = spawned {
        eprintln!("[ci] cannot start a transcript follower: {e}");
    }
}

/// Serves `/mux` until the replier goes away, which for this process means
/// never.
pub async fn serve(replier: Replier, config: Arc<CiConfig>, transcripts: Arc<Transcripts>) {
    loop {
        match replier.accept().await {
            Ok(request) => {
                let config = Arc::clone(&config);
                let transcripts = Arc::clone(&transcripts);
                tokio::spawn(async move { exchange(request, &config, &transcripts).await });
            }
            Err(e) => {
                eprintln!("[ci] stopped accepting mux exchanges: {e}");
                return;
            }
        }
    }
}

fn error(code: ErrorCode, message: String) -> WireError {
    WireError { code, message }
}

/// One exchange: the first frame says which kind.
async fn exchange(mut request: IncomingRequest, config: &CiConfig, transcripts: &Transcripts) {
    let principal = match editor_principal(config, request.meta()) {
        Ok(principal) => principal,
        Err(why) => {
            eprintln!("[ci] refused a mux exchange from a peer that {why}");
            request.refuse(weida::ErrorCode::Rejected).await;
            return;
        }
    };
    let mut body = request.take_body();
    let canceled = request.canceled();
    let first = match read_frame(&mut body).await {
        Ok((_, message)) => message,
        Err(e) => {
            eprintln!("[ci] {principal}: unreadable first frame: {e}");
            request.refuse(weida::ErrorCode::Rejected).await;
            return;
        }
    };
    let mut reply = match request.reply(TransferMeta::default()).await {
        Ok(reply) => reply,
        Err(e) => {
            eprintln!("[ci] {principal}: cannot open the reply half: {e}");
            return;
        }
    };
    let outcome = match first {
        Message::ControlAttach(attach) => {
            control(
                &principal,
                attach,
                transcripts.incarnation,
                &mut body,
                &mut reply,
                canceled,
            )
            .await
        }
        Message::TerminalAttach(attach) => {
            terminal(transcripts, attach, &mut body, &mut reply, canceled).await
        }
        Message::RowFetch(fetch) => fetch_page(transcripts, &fetch, &mut reply).await,
        other => Err(error(
            ErrorCode::Protocol,
            format!("{:?} cannot open an exchange", other.kind()),
        )),
    };
    if let Err(e) = outcome {
        // Best effort: the peer may already be gone.
        let _ = write_frame(&mut reply, &Message::Error(e), 0).await;
    }
    let _ = reply.finish();
}

/// The control exchange of a peer that only reads: an empty workspace, and a
/// refusal for every command, until the peer ends it.
async fn control(
    principal: &str,
    attach: ControlAttach,
    incarnation: RunnerIncarnation,
    body: &mut weida::IncomingTransfer,
    reply: &mut weida::OutgoingTransfer,
    canceled: impl Future<Output = ()>,
) -> Result<(), WireError> {
    if attach.hello.major != MAJOR {
        return Err(error(
            ErrorCode::Version,
            format!("protocol {} is not {MAJOR}", attach.hello.major),
        ));
    }
    let attached = ControlAttached {
        hello: ServerHello {
            major: MAJOR,
            minor: MINOR,
            incarnation,
            capabilities: Vec::<Capability>::new(),
            principal: principal.to_owned(),
            profiles: Vec::new(),
        },
        workspace: WorkspaceSnapshot {
            incarnation,
            revision: 0,
            items: Vec::new(),
            detached: Vec::new(),
        },
        heads: Vec::new(),
    };
    if let Err(e) = write_frame(reply, &Message::ControlAttached(attached), 0).await {
        eprintln!("[ci] {principal}: {e}");
        return Ok(());
    }
    let mut canceled = std::pin::pin!(canceled);
    loop {
        let frame = tokio::select! {
            () = &mut canceled => return Ok(()),
            frame = read_frame(body) => frame,
        };
        let message = match frame {
            Ok((_, message)) => message,
            Err(ReadError::Ended) | Err(ReadError::Io(_)) => return Ok(()),
            Err(ReadError::Codec(e)) => {
                return Err(error(ErrorCode::Protocol, e.to_string()));
            }
        };
        let Message::Command(command) = message else {
            return Err(error(
                ErrorCode::Protocol,
                format!("{:?} on a control exchange", message.kind()),
            ));
        };
        let refused = Message::CommandReply(CommandReply {
            request: command.request,
            outcome: CommandOutcome::Refused {
                reason: "the CI has no workspace to change".to_owned(),
            },
        });
        if let Err(e) = write_frame(reply, &refused, command.request.0).await {
            eprintln!("[ci] {principal}: {e}");
            return Ok(());
        }
    }
}

/// Streams a transcript. A resize is applied, so the screen rewraps to the
/// pane that asked last; other input is read and dropped: a transcript has
/// no process to type into.
async fn terminal(
    transcripts: &Transcripts,
    attach: TerminalAttach,
    body: &mut weida::IncomingTransfer,
    reply: &mut weida::OutgoingTransfer,
    canceled: impl Future<Output = ()>,
) -> Result<(), WireError> {
    let id = attach.terminal;
    let session = transcripts
        .session(id)
        .ok_or_else(|| error(ErrorCode::UnknownTerminal, format!("no {id}")))?;
    let acked = AtomicU64::new(0);
    let resize = |size| {
        if let Err(e) = session.apply(&TerminalCommand::Resize(size)) {
            eprintln!("[ci] {id}: resize refused: {e}");
        }
    };
    let streaming = stream_terminal(
        &session,
        &attach,
        &acked,
        reply,
        || resize(attach.size),
        || !transcripts.exists(id),
    );
    let reading = async {
        loop {
            match read_frame(body).await {
                Ok((_, Message::TerminalCommand(TerminalCommand::Resize(size)))) => resize(size),
                Ok(_) => {}
                Err(ReadError::Ended) | Err(ReadError::Io(_)) => return Ok(()),
                Err(ReadError::Codec(e)) => {
                    return Err(error(ErrorCode::Protocol, e.to_string()));
                }
            }
        }
    };
    tokio::select! {
        () = canceled => Ok(()),
        result = streaming => {
            if let Err(e) = result {
                eprintln!("[ci] {id}: {e}");
            }
            Ok(())
        }
        result = reading => result,
    }
}

async fn fetch_page(
    transcripts: &Transcripts,
    fetch: &RowFetch,
    reply: &mut weida::OutgoingTransfer,
) -> Result<(), WireError> {
    let session = transcripts
        .session(fetch.terminal)
        .ok_or_else(|| error(ErrorCode::UnknownTerminal, format!("no {}", fetch.terminal)))?;
    match fetch_rows(&session, fetch) {
        Message::Error(e) => Err(e),
        page => {
            if let Err(e) = write_frame(reply, &page, fetch.generation).await {
                eprintln!("[ci] {e}");
            }
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`).
    mod io {
        use super::*;
        use std::time::Instant;
        use zeughaus_terminal::wal::WalWriter;

        fn scratch(name: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!(
                "zeughaus-transcripts-{name}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create the scratch directory");
            dir
        }

        fn text_of(session: &Session) -> String {
            session
                .head(0)
                .rows
                .iter()
                .flat_map(|row| row.spans.iter().map(|span| span.text.as_str()))
                .collect()
        }

        fn until(what: &str, mut done: impl FnMut() -> bool) {
            let deadline = Instant::now() + Duration::from_secs(10);
            while !done() {
                assert!(Instant::now() < deadline, "timed out waiting for {what}");
                std::thread::sleep(Duration::from_millis(20));
            }
        }

        #[test]
        fn a_transcript_follows_the_wal_and_ends_with_the_recorded_exit() {
            let state = scratch("follow");
            let transcripts = Transcripts::new(state.clone());
            let dir = transcript_dir(&state, "view", 1, "build");
            std::fs::create_dir_all(&dir).expect("create the transcript directory");

            // Opened before the run wrote anything: the follower waits for it.
            let id = transcripts.open("view", 1, "build");
            let session = transcripts.session(id).expect("the screen is open");
            let mut wal = WalWriter::open(&dir.join("wal"), None).expect("open the WAL");
            wal.append(1, b"first\r\n").expect("append");
            until("the first line", || text_of(&session).contains("first"));
            assert_eq!(session.exit(), None);

            wal.append(2, b"second").expect("append");
            std::fs::write(dir.join("exit"), "code=3\n").expect("write the exit");
            until("the exit", || session.exit().is_some());
            assert_eq!(session.exit(), Some(ExitState::Exited { code: 3 }));
            let text = text_of(&session);
            assert!(
                text.contains("first") && text.contains("second"),
                "output written before the exit is shown: {text:?}"
            );
            let _ = std::fs::remove_dir_all(&state);
        }

        #[test]
        fn transcripts_are_reused_and_the_one_used_longest_ago_makes_room() {
            let state = scratch("lru");
            let transcripts = Transcripts::new(state.clone());

            let first = transcripts.open("view", 1, "j0");
            assert_eq!(
                transcripts.open("view", 1, "j0"),
                first,
                "same job, same screen"
            );
            let mut ids = vec![first];
            for i in 1..MAX_TRANSCRIPTS {
                ids.push(transcripts.open("view", 1, &format!("j{i}")));
            }
            assert!(ids.iter().all(|id| transcripts.exists(*id)));

            // One more closes j0, the oldest.
            let last = transcripts.open("view", 1, &format!("j{MAX_TRANSCRIPTS}"));
            assert!(!transcripts.exists(first));
            assert!(transcripts.exists(ids[1]) && transcripts.exists(last));

            // Using j1 makes j2 the oldest; opening j0 again takes j2's place.
            assert_eq!(transcripts.open("view", 1, "j1"), ids[1]);
            let again = transcripts.open("view", 1, "j0");
            assert_ne!(again, first, "a closed screen is a new one");
            assert!(!transcripts.exists(ids[2]) && transcripts.exists(ids[1]));

            // Closing is idempotent.
            transcripts.close(ids[1]);
            assert!(!transcripts.exists(ids[1]));
            transcripts.close(ids[1]);
            transcripts.close(TerminalId(123_456));
            let _ = std::fs::remove_dir_all(&state);
        }

        #[test]
        fn the_exit_file_names_a_code_or_none() {
            let dir = scratch("exit");
            assert_eq!(read_exit(&dir), None);
            std::fs::write(dir.join("exit"), "code=0\n").expect("write");
            assert_eq!(read_exit(&dir), Some(ExitState::Exited { code: 0 }));
            std::fs::write(dir.join("exit"), "code=none\n").expect("write");
            assert_eq!(read_exit(&dir), Some(ExitState::Killed));
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
