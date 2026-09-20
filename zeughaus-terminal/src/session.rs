//! One terminal: a PTY, a child, two threads and the canonical screen.
//!
//! A [`Session`] is a handle, cheap to clone and safe to hold from anywhere:
//! the runner's workspace actor keeps one, every subscriber task keeps one,
//! and the two threads that drive it keep one. What they share is a [`Model`]
//! behind a mutex. The PTY reader locks it per chunk to parse, a command locks
//! it to apply a keystroke, a subscriber locks it to build a delta -- all
//! short, none of them across an await, and never in the other order than
//! model first, PTY writer second.
//!
//! Why threads and not tasks: `portable-pty`'s reader is a blocking `Read`
//! and its `Child::wait` is a blocking wait. Both would need a
//! `spawn_blocking` slot for their whole life, which is a tokio thread with
//! extra steps. What crosses back into async is one
//! [`watch`](tokio::sync::watch) channel carrying the latest sequence number,
//! which is exactly the coalescing the plan asks for: a subscriber that wakes
//! late sees the newest state, never a queue of intermediate ones.
//!
//! The child outlives every client. Nothing here closes a session because a
//! GUI detached; only [`Session::kill`] and the runner's process exit end a
//! child (`TERMINAL_MUX_ARCHITECTURE_PLAN.md`, "Authority and lifetime").

use std::io::{self, Read, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};

use portable_pty::{Child, ChildKiller, CommandBuilder, MasterPty, PtySize, native_pty_system};
use tokio::sync::watch;
use wezterm_term::{Alert, AlertHandler, TerminalSize};
use zeughaus_mux::input::MAX_TEXT_BYTES;
use zeughaus_mux::{
    Controller, Dimensions, ExitState, RowData, StableRange, TerminalCommand, TerminalDelta,
    TerminalEvent, TerminalHead, TerminalId,
};

use crate::config::MAX_SCROLLBACK_ROWS;
use crate::convert;
use crate::model::Model;

/// Bytes read from the PTY in one go. Large enough that a full-screen redraw
/// or a `cat` of a big file is a handful of parser calls rather than
/// thousands, small enough that one chunk is one short turn under the model
/// lock.
const READ_CHUNK: usize = 64 * 1024;

/// Alerts buffered between two parser calls. The handler runs while the
/// terminal is borrowed and cannot touch the model, so it parks them here;
/// the reader drains them immediately afterwards. A child that rings the bell
/// a million times in one chunk gets the first few hundred.
const MAX_PENDING_ALERTS: usize = 256;

/// What to start, and how much of it to remember.
///
/// A profile is runner policy: a client asks for a profile by id and never
/// supplies argv, environment or a working directory (see the plan's
/// "Security"). `program: None` means the login shell of the user the runner
/// runs as -- `$SHELL` or the passwd entry, `/bin/sh` if neither is usable,
/// `%ComSpec%` or `cmd.exe` on Windows -- which is `portable-pty`'s default
/// program, and the only case where `args` is ignored: there is no argv to
/// append to a shell the builder resolves at spawn time.
#[derive(Debug, Clone)]
pub struct Profile {
    /// What a tab calls this before the child sets a title.
    pub label: String,
    pub program: Option<PathBuf>,
    pub args: Vec<String>,
    /// `None` runs the child in the runner's own working directory.
    pub cwd: Option<PathBuf>,
    /// Added to the runner's environment, after `TERM`/`COLORTERM`, so a
    /// profile may override those too.
    pub env: Vec<(String, String)>,
    pub scrollback_rows: usize,
}

impl Profile {
    /// The default profile: the runner user's login shell, the runner's
    /// working directory, and 10 000 rows of history.
    pub fn default_shell() -> Profile {
        Profile {
            label: "Shell".to_string(),
            program: None,
            args: Vec::new(),
            cwd: None,
            env: Vec::new(),
            scrollback_rows: 10_000,
        }
    }

    fn command(&self) -> CommandBuilder {
        let mut cmd = match &self.program {
            Some(program) => {
                let mut cmd = CommandBuilder::new(program);
                cmd.args(&self.args);
                cmd
            }
            // `new_default_prog` panics on `arg`, by design: the program it
            // will run is not known until it runs.
            None => CommandBuilder::new_default_prog(),
        };
        // What every terminal has to announce about itself. Set before the
        // profile's own environment so a profile can still override them.
        cmd.env("TERM", "xterm-256color");
        cmd.env("COLORTERM", "truecolor");
        for (key, value) in &self.env {
            cmd.env(key, value);
        }
        if let Some(cwd) = &self.cwd {
            cmd.cwd(cwd);
        }
        cmd
    }

    fn scrollback(&self) -> usize {
        self.scrollback_rows.min(MAX_SCROLLBACK_ROWS)
    }
}

/// Why a terminal could not be started. Both carry the platform's message
/// verbatim, because that is what tells the user whether the profile's
/// program does not exist or the system is out of PTYs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SpawnError {
    /// The PTY itself: allocating it, or getting a reader or writer from it.
    Pty(String),
    /// The child: the program is missing, not executable, or the fork failed.
    Spawn(String),
}

impl std::fmt::Display for SpawnError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            SpawnError::Pty(e) => write!(f, "pty: {e}"),
            SpawnError::Spawn(e) => write!(f, "spawn: {e}"),
        }
    }
}

impl std::error::Error for SpawnError {}

struct Inner {
    id: TerminalId,
    model: Mutex<Model>,
    /// Filled by the alert handler while the terminal is borrowed, drained by
    /// the reader under the model lock.
    alerts: Arc<Mutex<Vec<Alert>>>,
    /// The one writer the PTY gives out, shared by the terminal (key and
    /// mouse encodings, answerbacks) and by committed text.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    /// Set before the signal, read after the wait: it is what turns whatever
    /// status the kill produced into [`ExitState::Killed`].
    killed: AtomicBool,
    changes: watch::Sender<u64>,
}

impl Inner {
    fn model(&self) -> MutexGuard<'_, Model> {
        // A panic under the lock must not take the whole mux down with it:
        // the terminal's state is a screen, not an invariant a client can
        // corrupt, and a poisoned session that still shows its last screen is
        // better than one that panics every caller.
        self.model.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn publish(&self) {
        let seq = self.model().seq();
        self.changes.send_replace(seq);
    }
}

/// A live terminal. Clones are the same terminal.
#[derive(Clone)]
pub struct Session {
    inner: Arc<Inner>,
}

impl Session {
    /// Open a PTY, start the profile's program in it, and begin parsing its
    /// output.
    ///
    /// Returns as soon as the child exists; output arrives on the reader
    /// thread and is announced through [`Session::changes`].
    pub fn spawn(
        id: TerminalId,
        profile: &Profile,
        size: Dimensions,
    ) -> Result<Session, SpawnError> {
        if !size.is_valid() {
            return Err(SpawnError::Pty(format!(
                "refusing a {}x{} grid",
                size.cols, size.rows
            )));
        }
        let pty = native_pty_system()
            .openpty(pty_size(size))
            .map_err(|e| SpawnError::Pty(e.to_string()))?;
        let child = pty
            .slave
            .spawn_command(profile.command())
            .map_err(|e| SpawnError::Spawn(e.to_string()))?;
        // Our copy of the slave must go before the child can ever see EOF:
        // as long as any process holds it open, the master never reads one.
        drop(pty.slave);

        let master = pty.master;
        // From here on the child is alive but nothing is attached to it: a
        // failure now must take it with us rather than leave a shell running
        // in a PTY no one will ever read.
        let mut killer = child.clone_killer();
        let writer = Arc::new(Mutex::new(
            master
                .take_writer()
                .map_err(|e| orphan(killer.as_mut(), &e))?,
        ));
        let reader = master
            .try_clone_reader()
            .map_err(|e| orphan(killer.as_mut(), &e))?;

        let alerts: Arc<Mutex<Vec<Alert>>> = Arc::new(Mutex::new(Vec::new()));
        let mut model = Model::new(
            size,
            profile.scrollback(),
            &profile.label,
            Box::new(SharedWriter(Arc::clone(&writer))),
        );
        // ConPTY reports a resize and moves the cursor differently enough
        // that the terminal has a mode for it.
        #[cfg(windows)]
        model.terminal_mut().enable_conpty_quirks();
        model
            .terminal_mut()
            .set_notification_handler(Box::new(Alerts(Arc::clone(&alerts))));
        let (changes, _) = watch::channel(model.seq());

        let inner = Arc::new(Inner {
            id,
            model: Mutex::new(model),
            alerts,
            writer,
            master: Mutex::new(master),
            killer: Mutex::new(killer),
            killed: AtomicBool::new(false),
            changes,
        });

        let session = Session { inner };
        session.start_threads(reader, child)?;
        Ok(session)
    }

    fn start_threads(
        &self,
        reader: Box<dyn Read + Send>,
        child: Box<dyn Child + Send + Sync>,
    ) -> Result<(), SpawnError> {
        let id = self.inner.id.0;
        let reading = Arc::clone(&self.inner);
        std::thread::Builder::new()
            .name(format!("zh-pty-read-{id}"))
            .spawn(move || read_loop(reading, reader))
            .map_err(|e| {
                // Without a reader the child would block on its first full
                // buffer; a terminal nobody reads is worse than none.
                self.kill();
                SpawnError::Spawn(e.to_string())
            })?;
        let waiting = Arc::clone(&self.inner);
        std::thread::Builder::new()
            .name(format!("zh-pty-wait-{id}"))
            .spawn(move || wait_loop(waiting, child))
            .map_err(|e| {
                self.kill();
                SpawnError::Spawn(e.to_string())
            })?;
        Ok(())
    }

    pub fn id(&self) -> TerminalId {
        self.inner.id
    }

    /// The latest model sequence number, coalesced: a subscriber that wakes
    /// after a thousand chunks sees the newest number once, and asks for one
    /// delta.
    pub fn changes(&self) -> watch::Receiver<u64> {
        self.inner.changes.subscribe()
    }

    /// The whole current state, plus up to `rows_above` rows of scrollback
    /// above the screen (bounded by [`crate::MAX_ROWS_ABOVE`]).
    pub fn head(&self, rows_above: usize) -> TerminalHead {
        self.inner.model().head(self.inner.id, rows_above)
    }

    /// Everything that changed since `seq`, for the visible screen and the
    /// rows this client watches.
    pub fn delta_since(&self, seq: u64, watched: StableRange) -> TerminalDelta {
        self.inner.model().delta_since(self.inner.id, seq, watched)
    }

    /// The still-retained rows of `range`, the sequence number they were read
    /// at, and the oldest row that still exists.
    pub fn rows(&self, range: StableRange) -> (u64, i64, Vec<RowData>) {
        self.inner.model().rows(range)
    }

    pub fn exit(&self) -> Option<ExitState> {
        self.inner.model().exit()
    }

    pub fn title(&self) -> String {
        self.inner.model().title()
    }

    /// Record who holds the control lease. The engine does not enforce it --
    /// the runner decides whose commands reach [`Session::apply`] -- but the
    /// change is an ordered event, so every viewer learns about a takeover in
    /// the same place it learns about an exit.
    pub fn set_controller(&self, controller: Option<Controller>) {
        self.inner.model().set_controller(controller);
        self.inner.publish();
    }

    /// Apply one client command.
    ///
    /// `Viewport` is a no-op: which rows a client watches is the runner
    /// subscriber's state, not the terminal's. Everything else either reaches
    /// the child as bytes or changes the model; an error is a refusal (a
    /// paste beyond the wire limit, an impossible size), never a panic.
    pub fn apply(&self, command: &TerminalCommand) -> Result<(), String> {
        match command {
            TerminalCommand::Key { serial, input } => {
                let (code, mods) = convert::key(*input);
                let mut model = self.inner.model();
                model
                    .terminal_mut()
                    .key_down(code, mods)
                    .map_err(|e| e.to_string())?;
                model.note_serial(*serial);
            }
            TerminalCommand::Text { serial, text } => {
                if text.len() > MAX_TEXT_BYTES {
                    return Err(format!("text of {} bytes refused", text.len()));
                }
                self.write(text.as_bytes())?;
                self.inner.model().note_serial(*serial);
            }
            TerminalCommand::Paste { serial, text } => {
                if text.len() > MAX_TEXT_BYTES {
                    return Err(format!("paste of {} bytes refused", text.len()));
                }
                let mut model = self.inner.model();
                // The terminal brackets it if the child asked for brackets,
                // and canonicalizes the line endings; a client never does.
                model
                    .terminal_mut()
                    .send_paste(text)
                    .map_err(|e| e.to_string())?;
                model.note_serial(*serial);
            }
            TerminalCommand::Mouse { serial, input } => {
                let mut model = self.inner.model();
                model
                    .terminal_mut()
                    .mouse_event(convert::mouse(*input))
                    .map_err(|e| e.to_string())?;
                model.note_serial(*serial);
            }
            TerminalCommand::Resize(size) => {
                if !size.is_valid() {
                    return Err(format!("refusing a {}x{} grid", size.cols, size.rows));
                }
                // The model first: it rewraps and reports the damage. The
                // kernel second: that is what wakes the child with SIGWINCH,
                // and by then the screen it will draw into is the right one.
                self.inner.model().terminal_mut().resize(TerminalSize {
                    rows: size.rows as usize,
                    cols: size.cols as usize,
                    pixel_width: 0,
                    pixel_height: 0,
                    dpi: 0,
                });
                let master = self.inner.master.lock().unwrap_or_else(|e| e.into_inner());
                master.resize(pty_size(*size)).map_err(|e| e.to_string())?;
            }
            TerminalCommand::Focus(focused) => {
                self.inner.model().terminal_mut().focus_changed(*focused);
            }
            TerminalCommand::Viewport { .. } => return Ok(()),
        }
        self.inner.publish();
        Ok(())
    }

    /// Kill the child. The exit is recorded as [`ExitState::Killed`] whatever
    /// signal or status it produced, because what a closing pane wants to
    /// know is that this was deliberate.
    pub fn kill(&self) {
        self.inner.killed.store(true, Ordering::SeqCst);
        let mut killer = self.inner.killer.lock().unwrap_or_else(|e| e.into_inner());
        let _ = killer.kill();
    }

    fn write(&self, bytes: &[u8]) -> Result<(), String> {
        let mut writer = self.inner.writer.lock().unwrap_or_else(|e| e.into_inner());
        writer.write_all(bytes).map_err(|e| e.to_string())?;
        writer.flush().map_err(|e| e.to_string())
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.inner.id)
            .finish()
    }
}

fn pty_size(size: Dimensions) -> PtySize {
    PtySize {
        rows: size.rows,
        cols: size.cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}

/// Turn a PTY failure into a [`SpawnError`], killing the child that failure
/// left behind.
fn orphan(killer: &mut dyn ChildKiller, error: &dyn std::fmt::Display) -> SpawnError {
    let _ = killer.kill();
    SpawnError::Pty(error.to_string())
}

/// Parse everything the child writes until the PTY reports EOF.
///
/// EOF is the end of output, not the end of the session: the final screen and
/// the exit status stay readable until the pane is closed.
fn read_loop(inner: Arc<Inner>, mut reader: Box<dyn Read + Send>) {
    let mut buf = vec![0u8; READ_CHUNK];
    loop {
        let read = match reader.read(&mut buf) {
            Ok(0) => break,
            Ok(read) => read,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            // Anything else is a PTY that will not produce more output; on
            // unix the usual end (EIO after the slave closed) already comes
            // back as EOF.
            Err(_) => break,
        };
        {
            let mut model = inner.model();
            model.advance(&buf[..read]);
            for alert in take_alerts(&inner.alerts) {
                match alert {
                    // A bell is ordered: it is the one alert a client must
                    // see even if it never looks at the rows that caused it.
                    Alert::Bell => {
                        model.record(TerminalEvent::Bell);
                    }
                    // The title itself is read from the terminal; what the
                    // alert adds is that the child has named itself at all,
                    // and the profile's label stops standing in for it.
                    Alert::WindowTitleChanged(_) | Alert::IconTitleChanged(_) => {
                        model.note_title();
                    }
                    // Everything else is state the next head or delta
                    // carries anyway (palette, working directory, progress).
                    _ => {}
                }
            }
        }
        inner.publish();
    }
}

fn take_alerts(alerts: &Mutex<Vec<Alert>>) -> Vec<Alert> {
    let mut alerts = alerts.lock().unwrap_or_else(|e| e.into_inner());
    std::mem::take(&mut *alerts)
}

/// Reap the child and record why it is gone.
///
/// This does not wait for the reader: a child whose grandchild still holds
/// the PTY open would never let it finish, and a pane that shows "running"
/// for a process that is gone is the worse lie. Output still in flight keeps
/// arriving afterwards and moves the watch again.
fn wait_loop(inner: Arc<Inner>, mut child: Box<dyn Child + Send + Sync>) {
    let status = child.wait();
    let exit = if inner.killed.load(Ordering::SeqCst) {
        ExitState::Killed
    } else {
        match status {
            Ok(status) => match status.signal() {
                Some(signal) => ExitState::Signaled {
                    signal: signal.to_string(),
                },
                None => ExitState::Exited {
                    code: status.exit_code(),
                },
            },
            // The child exists but cannot be reaped; the session is over
            // either way and the reason is worth showing.
            Err(e) => ExitState::SpawnFailed {
                reason: e.to_string(),
            },
        }
    };
    inner.model().set_exit(exit);
    inner.publish();
}

/// The PTY's single writer, shared by the terminal and by committed text.
///
/// `take_writer` may be called once, and `Terminal` wants to own what it is
/// given, so the terminal gets this shim and everything else goes through the
/// same mutex behind it. One writer, two users, no interleaved half-writes.
struct SharedWriter(Arc<Mutex<Box<dyn Write + Send>>>);

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(buf)
            .map(|()| buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.0.lock().unwrap_or_else(|e| e.into_inner()).flush()
    }
}

/// Collects the terminal's alerts while it is borrowed.
///
/// The handler is called from inside `advance_bytes`, with the terminal (and
/// therefore the model) already mutably borrowed, so it must not reach for
/// the model lock. It parks what it saw here instead.
struct Alerts(Arc<Mutex<Vec<Alert>>>);

impl AlertHandler for Alerts {
    fn alert(&mut self, alert: Alert) {
        let mut alerts = self.0.lock().unwrap_or_else(|e| e.into_inner());
        if alerts.len() < MAX_PENDING_ALERTS {
            alerts.push(alert);
        }
    }
}

/// A real child in a real PTY. Unix only: the tests drive `/bin/sh`, and a
/// Windows equivalent would prove something about `cmd.exe`, not about this
/// crate.
#[cfg(all(test, unix))]
mod tests {
    use std::time::Duration;

    use super::*;

    fn shell(script: &str) -> Profile {
        Profile {
            label: "test".to_string(),
            program: Some(PathBuf::from("/bin/sh")),
            args: vec!["-c".to_string(), script.to_string()],
            cwd: None,
            env: Vec::new(),
            scrollback_rows: 64,
        }
    }

    fn text_of(head: &TerminalHead) -> String {
        head.rows
            .iter()
            .flat_map(|row| row.spans.iter().map(|span| span.text.as_str()))
            .collect()
    }

    /// Wait for `done` to hold, driven by the session's own notifications.
    async fn settle(session: &Session, within: Duration, done: impl Fn(&Session) -> bool) -> bool {
        let mut changes = session.changes();
        let deadline = tokio::time::Instant::now() + within;
        loop {
            if done(session) {
                return true;
            }
            if tokio::time::timeout_at(deadline, changes.changed())
                .await
                .is_err()
            {
                return done(session);
            }
        }
    }

    #[tokio::test]
    async fn a_child_runs_and_its_exit_status_outlives_it() {
        let session = Session::spawn(
            TerminalId(7),
            &shell("printf hello; exit 3"),
            Dimensions { cols: 40, rows: 6 },
        )
        .expect("spawn /bin/sh");

        let finished = settle(&session, Duration::from_secs(10), |session| {
            session.exit().is_some() && text_of(&session.head(0)).contains("hello")
        })
        .await;

        assert!(finished, "child never finished: exit={:?}", session.exit());
        assert_eq!(session.exit(), Some(ExitState::Exited { code: 3 }));
        // The final screen survives the child: a closed pane still shows what
        // the program left behind.
        assert!(text_of(&session.head(0)).contains("hello"));
    }

    #[tokio::test]
    async fn killing_a_child_is_recorded_as_a_kill() {
        let session = Session::spawn(
            TerminalId(8),
            &shell("sleep 30"),
            Dimensions { cols: 40, rows: 6 },
        )
        .expect("spawn /bin/sh");

        session.kill();
        let finished = settle(&session, Duration::from_secs(5), |session| {
            session.exit().is_some()
        })
        .await;

        assert!(finished, "the child outlived its kill");
        assert_eq!(session.exit(), Some(ExitState::Killed));
    }
}
