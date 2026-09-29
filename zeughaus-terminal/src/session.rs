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

use std::ffi::OsString;
use std::fs::OpenOptions;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
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

    /// The one place a profile becomes a command line, for a local PTY and
    /// a shim's alike.
    pub(crate) fn command(&self) -> CommandBuilder {
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

    pub(crate) fn scrollback(&self) -> usize {
        self.scrollback_rows.min(MAX_SCROLLBACK_ROWS)
    }
}

/// Where a terminal's PTY and child live.
#[derive(Debug, Clone)]
pub enum TerminalHost {
    /// In this process: the child ends when the process does.
    Local,
    /// In a shim process per terminal, which outlives this one.
    #[cfg(unix)]
    Shim(ShimHost),
}

/// How to start a shim, and where shims keep their directories.
///
/// The shim is started as `program args... <root>/<terminal-id>`; for the
/// runner that is its own executable with the `shim` subcommand.
#[cfg(unix)]
#[derive(Debug, Clone)]
pub struct ShimHost {
    pub program: PathBuf,
    pub args: Vec<OsString>,
    pub root: PathBuf,
}

#[cfg(unix)]
impl ShimHost {
    /// The directory of terminal `id`'s shim.
    pub fn dir(&self, id: TerminalId) -> PathBuf {
        self.root.join(id.0.to_string())
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

/// What drives the child: the PTY in this process, or a shim's socket.
enum Io {
    Local {
        master: Mutex<Box<dyn MasterPty + Send>>,
        killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    },
    #[cfg(unix)]
    Shim { sender: crate::shim::ShimSender },
}

struct Inner {
    id: TerminalId,
    model: Mutex<Model>,
    /// Filled by the alert handler while the terminal is borrowed, drained by
    /// the reader under the model lock.
    alerts: Arc<Mutex<Vec<Alert>>>,
    /// The one writer the child's input goes through, shared by the terminal
    /// (key and mouse encodings, answerbacks) and by committed text.
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    io: Io,
    /// Set before the signal, read after the wait: it is what turns whatever
    /// status the kill produced into [`ExitState::Killed`].
    killed: AtomicBool,
    changes: watch::Sender<u64>,
    /// A copy of every byte the child writes, taken before the parser sees
    /// it. A job's log is this sink; a write error ends the tee and leaves
    /// the terminal running.
    tee: Mutex<Option<Box<dyn Write + Send>>>,
    /// Called by the reader after the child changed the terminal's title.
    /// The title reaches places no subscriber streams to (a workspace's tab
    /// bar), so it has its own notice besides `changes`.
    on_title: Mutex<Option<TitleListener>>,
}

/// What [`Session::on_title_change`] registers.
type TitleListener = Arc<dyn Fn() + Send + Sync>;

impl Inner {
    /// The shared state of a session whose child `io` drives. `writer` is
    /// where the child's input goes; the terminal writes through it only
    /// while `muted` is clear.
    #[allow(clippy::too_many_arguments)]
    fn new(
        id: TerminalId,
        size: Dimensions,
        scrollback: usize,
        label: &str,
        writer: Box<dyn Write + Send>,
        io: Io,
        tee: Option<Box<dyn Write + Send>>,
        muted: Arc<AtomicBool>,
    ) -> Arc<Inner> {
        let writer = Arc::new(Mutex::new(writer));
        let alerts: Arc<Mutex<Vec<Alert>>> = Arc::new(Mutex::new(Vec::new()));
        let mut model = Model::new(
            size,
            scrollback,
            label,
            Box::new(SharedWriter {
                writer: Arc::clone(&writer),
                muted,
            }),
        );
        // ConPTY reports a resize and moves the cursor differently enough
        // that the terminal has a mode for it.
        #[cfg(windows)]
        model.terminal_mut().enable_conpty_quirks();
        model
            .terminal_mut()
            .set_notification_handler(Box::new(Alerts(Arc::clone(&alerts))));
        let (changes, _) = watch::channel(model.seq());
        Arc::new(Inner {
            id,
            model: Mutex::new(model),
            alerts,
            writer,
            io,
            killed: AtomicBool::new(false),
            changes,
            tee: Mutex::new(tee),
            on_title: Mutex::new(None),
        })
    }

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
    /// Starts the profile's program in a PTY on `host`, and begins parsing
    /// its output.
    ///
    /// `log`, when given, is appended every byte the child writes before it
    /// is parsed: the raw PTY stream, escape sequences included, which is a
    /// recording of what the program produced rather than of what the screen
    /// shows. A write error ends the log and the terminal carries on.
    ///
    /// Returns as soon as the child exists; output arrives on a reader
    /// thread and is announced through [`Session::changes`].
    pub fn spawn(
        id: TerminalId,
        profile: &Profile,
        size: Dimensions,
        log: Option<&Path>,
        host: &TerminalHost,
    ) -> Result<Session, SpawnError> {
        if !size.is_valid() {
            return Err(SpawnError::Pty(format!(
                "refusing a {}x{} grid",
                size.cols, size.rows
            )));
        }
        match host {
            TerminalHost::Local => Session::spawn_local(id, profile, size, log),
            #[cfg(unix)]
            TerminalHost::Shim(shim) => Session::spawn_shim(id, profile, size, log, shim),
        }
    }

    fn spawn_local(
        id: TerminalId,
        profile: &Profile,
        size: Dimensions,
        log: Option<&Path>,
    ) -> Result<Session, SpawnError> {
        let tee: Option<Box<dyn Write + Send>> = match log {
            Some(path) => Some(Box::new(
                OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .map_err(|e| SpawnError::Spawn(format!("{}: {e}", path.display())))?,
            )),
            None => None,
        };
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
        let writer = master
            .take_writer()
            .map_err(|e| orphan(killer.as_mut(), &e))?;
        let reader = master
            .try_clone_reader()
            .map_err(|e| orphan(killer.as_mut(), &e))?;

        let inner = Inner::new(
            id,
            size,
            profile.scrollback(),
            &profile.label,
            writer,
            Io::Local {
                master: Mutex::new(master),
                killer: Mutex::new(killer),
            },
            tee,
            Arc::new(AtomicBool::new(false)),
        );
        let session = Session { inner };
        session.start_threads(reader, child)?;
        Ok(session)
    }

    /// Writes the shim's spec, starts it, and attaches to it once its socket
    /// answers.
    #[cfg(unix)]
    fn spawn_shim(
        id: TerminalId,
        profile: &Profile,
        size: Dimensions,
        log: Option<&Path>,
        host: &ShimHost,
    ) -> Result<Session, SpawnError> {
        use std::os::unix::fs::DirBuilderExt;
        use std::process::{Command, Stdio};
        use std::time::{Duration, Instant};

        let dir = host.dir(id);
        let failed = |e: &dyn std::fmt::Display| {
            let _ = std::fs::remove_dir_all(&dir);
            SpawnError::Spawn(format!("shim {}: {e}", dir.display()))
        };
        // Owner-only: whoever can connect to `sock` can type into the shell.
        std::fs::DirBuilder::new()
            .recursive(true)
            .mode(0o700)
            .create(&dir)
            .map_err(|e| failed(&e))?;
        let spec = crate::shim::ShimSpec {
            label: profile.label.clone(),
            program: profile.program.clone(),
            args: profile.args.clone(),
            cwd: profile.cwd.clone(),
            env: profile.env.clone(),
            scrollback_rows: profile.scrollback_rows,
            cols: size.cols,
            rows: size.rows,
            log: log.map(Path::to_path_buf),
        };
        let json = serde_json::to_vec_pretty(&spec).map_err(|e| failed(&e))?;
        std::fs::write(dir.join("spec.json"), json).map_err(|e| failed(&e))?;
        let shim_log = dir.join("shim.log");
        let stderr = std::fs::File::create(&shim_log).map_err(|e| failed(&e))?;
        // The shim forks away from this child at once; waiting for it is
        // what keeps it from lingering as a zombie.
        let status = Command::new(&host.program)
            .args(&host.args)
            .arg(&dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(stderr)
            .status()
            .map_err(|e| failed(&e))?;
        if !status.success() {
            return Err(failed(&format!("exited with {status}")));
        }

        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match crate::shim::ShimConn::connect(&dir) {
                Ok(conn) => {
                    return Session::attach_shim(
                        id,
                        &profile.label,
                        profile.scrollback(),
                        size,
                        conn,
                        false,
                    );
                }
                Err(e) => {
                    // A shim that could not start its program says why on
                    // its stderr and exits before it ever binds the socket.
                    let said = std::fs::read_to_string(&shim_log).unwrap_or_default();
                    if !said.trim().is_empty() && !dir.join("sock").exists() {
                        return Err(failed(&said.trim()));
                    }
                    if Instant::now() >= deadline {
                        return Err(failed(&e));
                    }
                }
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }

    /// Attaches to the shim of a terminal a previous runner started, and
    /// rebuilds its screen from the shim's replay.
    ///
    /// `size` is the grid the terminal last had: the replay is parsed at
    /// it, and the child is asked to redraw afterwards, because whatever
    /// scrolled out of the replay is gone.
    #[cfg(unix)]
    pub fn reattach(
        id: TerminalId,
        host: &ShimHost,
        label: &str,
        scrollback_rows: usize,
        size: Dimensions,
    ) -> Result<Session, SpawnError> {
        if !size.is_valid() {
            return Err(SpawnError::Pty(format!(
                "refusing a {}x{} grid",
                size.cols, size.rows
            )));
        }
        let conn = crate::shim::ShimConn::connect(&host.dir(id))?;
        Session::attach_shim(
            id,
            label,
            scrollback_rows.min(MAX_SCROLLBACK_ROWS),
            size,
            conn,
            true,
        )
    }

    #[cfg(unix)]
    fn attach_shim(
        id: TerminalId,
        label: &str,
        scrollback: usize,
        size: Dimensions,
        (conn, welcome, replay): (crate::shim::ShimConn, crate::shim::Welcome, Vec<u8>),
        redraw: bool,
    ) -> Result<Session, SpawnError> {
        use crate::shim::proto::ToShim;

        let sender = conn.sender();
        // Muted while the replay is parsed: the queries in it (device
        // attributes, cursor position) were answered when they were live,
        // and answering them again would type the answers into the shell.
        let muted = Arc::new(AtomicBool::new(true));
        let inner = Inner::new(
            id,
            size,
            scrollback,
            label,
            Box::new(ShimInput(sender.clone())),
            Io::Shim {
                sender: sender.clone(),
            },
            None,
            Arc::clone(&muted),
        );
        {
            let mut model = inner.model();
            model.advance(&replay);
            // Bells in the replay rang long ago; a title it set is still the
            // terminal's title.
            for alert in take_alerts(&inner.alerts) {
                if matches!(
                    alert,
                    Alert::WindowTitleChanged(_) | Alert::IconTitleChanged(_)
                ) {
                    model.note_title();
                }
            }
            if let Some(exit) = welcome.exit {
                model.set_exit(shim_exit(&inner, exit));
            }
        }
        muted.store(false, Ordering::SeqCst);
        inner.publish();
        if redraw {
            // Best effort: a shim that is already gone shows up as the
            // reader's end of stream.
            let _ = sender.send(&ToShim::Redraw);
        }
        let reading = Arc::clone(&inner);
        std::thread::Builder::new()
            .name(format!("zh-shim-read-{}", id.0))
            .spawn(move || shim_loop(reading, conn))
            .map_err(|e| SpawnError::Spawn(e.to_string()))?;
        Ok(Session { inner })
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

    /// The grid the terminal currently has.
    pub fn size(&self) -> Dimensions {
        self.inner.model().size()
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

    /// Everything that changed since `seq`: every retained row written since
    /// then, wherever the screen has scrolled it to.
    pub fn delta_since(&self, seq: u64) -> TerminalDelta {
        self.inner.model().delta_since(self.inner.id, seq)
    }

    /// The stable-row space this terminal's heads, deltas and pages are in;
    /// it changes when the child switches between the primary and the
    /// alternate screen.
    pub fn epoch(&self) -> u64 {
        self.inner.model().epoch()
    }

    /// The still-retained rows of `range`, the sequence number they were read
    /// at, and the oldest row that still exists; `None` when the terminal is
    /// no longer in `epoch`.
    pub fn rows(&self, epoch: u64, range: StableRange) -> Option<(u64, i64, Vec<RowData>)> {
        self.inner.model().rows(epoch, range)
    }

    pub fn exit(&self) -> Option<ExitState> {
        self.inner.model().exit()
    }

    pub fn title(&self) -> String {
        self.inner.model().title()
    }

    /// Registers `listener` to be called whenever [`Session::title`] changes,
    /// replacing any earlier one. It runs on the PTY reader thread with no
    /// lock of this session held, so it may call back into the session; it
    /// must not block, because output waits for it.
    pub fn on_title_change(&self, listener: impl Fn() + Send + Sync + 'static) {
        *self
            .inner
            .on_title
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = Some(Arc::new(listener));
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
                match &self.inner.io {
                    Io::Local { master, .. } => master
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .resize(pty_size(*size))
                        .map_err(|e| e.to_string())?,
                    #[cfg(unix)]
                    Io::Shim { sender } => sender
                        .send(&crate::shim::proto::ToShim::Resize {
                            cols: size.cols,
                            rows: size.rows,
                        })
                        .map_err(|e| e.to_string())?,
                }
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
    /// know is that this was deliberate. A shim is told to close, which also
    /// ends the shim and removes its directory.
    pub fn kill(&self) {
        self.inner.killed.store(true, Ordering::SeqCst);
        match &self.inner.io {
            Io::Local { killer, .. } => {
                let _ = killer.lock().unwrap_or_else(|e| e.into_inner()).kill();
            }
            #[cfg(unix)]
            Io::Shim { sender } => {
                let _ = sender.send(&crate::shim::proto::ToShim::Close);
            }
        }
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

pub(crate) fn pty_size(size: Dimensions) -> PtySize {
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
    // What the listener was last told, compared only in chunks that carry a
    // title alert: a shell that re-sends the same title per prompt is not a
    // change.
    let mut title = inner.model().title();
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
        consume(&inner, &buf[..read], &mut title);
    }
}

/// One chunk of the child's output: teed, parsed, published.
fn consume(inner: &Inner, chunk: &[u8], title: &mut String) {
    // The tee copies the raw stream first: what a job's log records is
    // what the program wrote, whatever the parser then makes of it.
    {
        let mut tee = inner.tee.lock().unwrap_or_else(|e| e.into_inner());
        let failed = match tee.as_mut() {
            Some(sink) => sink.write_all(chunk).and_then(|()| sink.flush()).is_err(),
            None => false,
        };
        if failed {
            *tee = None;
        }
    }
    let mut retitled = false;
    {
        let mut model = inner.model();
        model.advance(chunk);
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
                    retitled = true;
                }
                // Everything else is state the next head or delta
                // carries anyway (palette, working directory, progress).
                _ => {}
            }
        }
        if retitled {
            let now = model.title();
            retitled = now != *title;
            *title = now;
        }
    }
    inner.publish();
    if retitled {
        // Cloned out so the listener runs with no lock held, the slot's
        // own included.
        let listener = inner
            .on_title
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        if let Some(listener) = listener {
            listener();
        }
    }
}

/// The shim backend's reader and waiter in one: output is parsed like a
/// local PTY's, the child's exit arrives as a frame, and the end of the
/// connection is the end of the shim.
#[cfg(unix)]
fn shim_loop(inner: Arc<Inner>, mut conn: crate::shim::ShimConn) {
    use crate::shim::proto::FromShim;

    let mut title = inner.model().title();
    loop {
        match conn.recv() {
            Ok(Some(FromShim::Output(chunk))) => consume(&inner, chunk, &mut title),
            Ok(Some(FromShim::Exited(exit))) => {
                let exit = shim_exit(&inner, exit);
                inner.model().set_exit(exit);
                inner.publish();
            }
            Ok(Some(FromShim::Welcome(_))) => {}
            Ok(None) | Err(_) => {
                // A shim ends by being closed, or by dying. The first is the
                // kill this session asked for; the second leaves a child
                // nobody can reach, which is over as far as a pane can tell.
                let exit = if inner.killed.load(Ordering::SeqCst) {
                    ExitState::Killed
                } else {
                    ExitState::SpawnFailed {
                        reason: "the terminal's shim is gone".to_owned(),
                    }
                };
                inner.model().set_exit(exit);
                inner.publish();
                return;
            }
        }
    }
}

/// A shim's report of its child's end, as the session records it.
#[cfg(unix)]
fn shim_exit(inner: &Inner, exit: crate::shim::ShimExit) -> ExitState {
    if exit.killed || inner.killed.load(Ordering::SeqCst) {
        return ExitState::Killed;
    }
    match (exit.signal, exit.code) {
        (Some(signal), _) => ExitState::Signaled { signal },
        (None, Some(code)) => ExitState::Exited { code },
        (None, None) => ExitState::SpawnFailed {
            reason: "the shim could not reap its child".to_owned(),
        },
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

/// The child's single input, shared by the terminal and by committed text.
///
/// `take_writer` may be called once, and `Terminal` wants to own what it is
/// given, so the terminal gets this shim and everything else goes through the
/// same mutex behind it. One writer, two users, no interleaved half-writes.
/// While `muted`, what the terminal writes is dropped: its answers to a
/// replay's queries are not input.
struct SharedWriter {
    writer: Arc<Mutex<Box<dyn Write + Send>>>,
    muted: Arc<AtomicBool>,
}

impl Write for SharedWriter {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        if self.muted.load(Ordering::SeqCst) {
            return Ok(buf.len());
        }
        self.writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .write_all(buf)
            .map(|()| buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        self.writer
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .flush()
    }
}

/// Input to a shim's child, framed.
#[cfg(unix)]
struct ShimInput(crate::shim::ShimSender);

#[cfg(unix)]
impl Write for ShimInput {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        // Bounded per frame; `write_all` sends the rest in the next ones.
        let n = buf.len().min(crate::shim::proto::OUTPUT_CHUNK);
        self.0.send(&crate::shim::proto::ToShim::Input(&buf[..n]))?;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
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
            None,
            &TerminalHost::Local,
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
            None,
            &TerminalHost::Local,
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

    #[tokio::test]
    async fn a_logged_child_is_appended_byte_for_byte() {
        let log = std::env::temp_dir().join(format!("zh-session-log-{}", std::process::id()));
        std::fs::write(&log, "earlier\n").expect("seed the log");
        let session = Session::spawn(
            TerminalId(9),
            &shell("printf teed-hello; exit 0"),
            Dimensions { cols: 40, rows: 6 },
            Some(&log),
            &TerminalHost::Local,
        )
        .expect("spawn /bin/sh");

        let logged = || std::fs::read_to_string(&log).unwrap_or_default();
        let finished = settle(&session, Duration::from_secs(10), |session| {
            session.exit().is_some() && logged().contains("teed-hello")
        })
        .await;

        let text = logged();
        let _ = std::fs::remove_file(&log);
        assert!(finished, "the log never saw the output: {text:?}");
        assert!(
            text.starts_with("earlier\n"),
            "the log was truncated: {text:?}"
        );
    }
}
