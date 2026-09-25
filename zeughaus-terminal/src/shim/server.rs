//! The shim process: one PTY and its child, held across runner restarts.

use std::collections::VecDeque;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::{Arc, Mutex, MutexGuard};

use portable_pty::{ChildKiller, MasterPty, native_pty_system};

use super::proto::{
    FromShim, OUTPUT_CHUNK, PROTOCOL, REPLAY_BYTES, ShimExit, ShimSpec, ToShim, Welcome,
    read_frame, write_frame,
};
use crate::Profile;
use crate::session::pty_size;

/// What the reader, the waiter and the connections share. One lock, so that
/// "append to the replay, then send to the session" and "send the replay,
/// then register the session" can never interleave.
struct Shared {
    ring: VecDeque<u8>,
    log: Option<File>,
    client: Option<UnixStream>,
    exit: Option<ShimExit>,
    killed: bool,
}

struct Shim {
    dir: PathBuf,
    shared: Mutex<Shared>,
    master: Mutex<Box<dyn MasterPty + Send>>,
    writer: Mutex<Box<dyn Write + Send>>,
    killer: Mutex<Box<dyn ChildKiller + Send + Sync>>,
    child_pid: u32,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

impl Shared {
    /// Sends to the attached session, dropping it on a write error: a
    /// session that went away is the normal case (its runner restarted),
    /// and the next one gets the replay.
    fn send(&mut self, frame: &[u8]) {
        if let Some(client) = self.client.as_mut()
            && write_frame(client, frame).is_err()
        {
            self.client = None;
        }
    }
}

/// Runs the shim for the terminal directory `dir`: detaches from the
/// caller, starts `dir/spec.json`'s program in a PTY, and serves sessions on
/// `dir/sock` until one sends `Close`.
///
/// The caller waits for this process to exit; the shim itself lives on in
/// a grandchild, in a session of its own, so it is never the runner's child
/// and a runner that `exec`s itself cannot leave it a zombie.
pub fn run(dir: &Path) -> ExitCode {
    // SAFETY: called as the first thing a single-threaded process does;
    // nothing else runs between fork and the next statement of either side.
    unsafe {
        match libc::fork() {
            -1 => {
                eprintln!("[shim] fork: {}", io::Error::last_os_error());
                return ExitCode::FAILURE;
            }
            0 => {}
            _ => libc::_exit(0),
        }
        libc::setsid();
        // A session leader acquires the first terminal it opens as its
        // controlling one; the PTY belongs to the child's session instead.
        match libc::fork() {
            -1 => {
                eprintln!("[shim] fork: {}", io::Error::last_os_error());
                libc::_exit(1);
            }
            0 => {}
            _ => libc::_exit(0),
        }
    }
    match serve(dir) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("[shim] {}: {e}", dir.display());
            ExitCode::FAILURE
        }
    }
}

fn serve(dir: &Path) -> io::Result<()> {
    let spec: ShimSpec = serde_json::from_slice(&std::fs::read(dir.join("spec.json"))?)
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
    let profile = Profile {
        label: spec.label,
        program: spec.program,
        args: spec.args,
        cwd: spec.cwd,
        env: spec.env,
        scrollback_rows: spec.scrollback_rows,
    };
    let size = zeughaus_mux::Dimensions {
        cols: spec.cols,
        rows: spec.rows,
    };
    let pty = native_pty_system()
        .openpty(pty_size(size))
        .map_err(io::Error::other)?;
    let mut child = pty
        .slave
        .spawn_command(profile.command())
        .map_err(io::Error::other)?;
    // As in a local session: the child's EOF depends on nobody else holding
    // the slave.
    drop(pty.slave);
    let mut killer = child.clone_killer();
    let fail = |killer: &mut Box<dyn ChildKiller + Send + Sync>, e: io::Error| {
        let _ = killer.kill();
        e
    };
    let writer = pty
        .master
        .take_writer()
        .map_err(|e| fail(&mut killer, io::Error::other(e)))?;
    let reader = pty
        .master
        .try_clone_reader()
        .map_err(|e| fail(&mut killer, io::Error::other(e)))?;
    let log = match &spec.log {
        Some(path) => Some(
            OpenOptions::new()
                .create(true)
                .append(true)
                .open(path)
                .map_err(|e| fail(&mut killer, e))?,
        ),
        None => None,
    };
    let sock = dir.join("sock");
    let _ = std::fs::remove_file(&sock);
    let listener = UnixListener::bind(&sock).map_err(|e| fail(&mut killer, e))?;

    let shim = Arc::new(Shim {
        dir: dir.to_path_buf(),
        shared: Mutex::new(Shared {
            ring: VecDeque::new(),
            log,
            client: None,
            exit: None,
            killed: false,
        }),
        master: Mutex::new(pty.master),
        writer: Mutex::new(writer),
        killer: Mutex::new(killer),
        child_pid: child.process_id().unwrap_or(0),
    });

    let reading = Arc::clone(&shim);
    std::thread::spawn(move || read_loop(&reading, reader));
    let waiting = Arc::clone(&shim);
    std::thread::spawn(move || {
        let status = child.wait();
        let mut shared = lock(&waiting.shared);
        let killed = shared.killed;
        let exit = match status {
            Ok(status) => ShimExit {
                code: status.signal().is_none().then(|| status.exit_code()),
                signal: status.signal().map(str::to_owned),
                killed,
            },
            Err(_) => ShimExit {
                code: None,
                signal: None,
                killed,
            },
        };
        shared.send(&FromShim::Exited(exit.clone()).encode());
        shared.exit = Some(exit);
    });

    for stream in listener.incoming() {
        let stream = match stream {
            Ok(stream) => stream,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        let connection = Arc::clone(&shim);
        std::thread::spawn(move || {
            if let Err(e) = connection.serve_client(stream) {
                eprintln!("[shim] session: {e}");
            }
        });
    }
    Ok(())
}

/// Copies every PTY chunk into the replay, the log and the attached session.
fn read_loop(shim: &Shim, mut reader: Box<dyn Read + Send>) {
    let mut buf = vec![0u8; OUTPUT_CHUNK];
    loop {
        let read = match reader.read(&mut buf) {
            Ok(0) => return,
            Ok(read) => read,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(_) => return,
        };
        let chunk = &buf[..read];
        let mut shared = lock(&shim.shared);
        let held = shared.ring.len();
        let overflow = (held + read).saturating_sub(REPLAY_BYTES).min(held);
        shared.ring.drain(..overflow);
        // A chunk is never larger than the ring, so dropping the oldest
        // bytes always makes room for all of it.
        shared.ring.extend(chunk);
        if let Some(log) = shared.log.as_mut()
            && log.write_all(chunk).is_err()
        {
            shared.log = None;
        }
        shared.send(&FromShim::Output(chunk).encode());
    }
}

impl Shim {
    fn serve_client(&self, stream: UnixStream) -> io::Result<()> {
        let mut reader = stream.try_clone()?;
        let mut buf = Vec::new();
        let Some((kind, body)) = read_frame(&mut reader, &mut buf)? else {
            return Ok(());
        };
        let ToShim::Hello { version } = ToShim::decode(kind, body)? else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "first frame is not Hello",
            ));
        };
        {
            let mut shared = lock(&self.shared);
            let mut out = stream.try_clone()?;
            let welcome = Welcome {
                version: PROTOCOL,
                child_pid: self.child_pid,
                replay_bytes: if version == PROTOCOL {
                    shared.ring.len() as u64
                } else {
                    0
                },
                exit: shared.exit.clone(),
            };
            write_frame(&mut out, &FromShim::Welcome(welcome).encode())?;
            if version != PROTOCOL {
                return Ok(());
            }
            let (front, back) = shared.ring.as_slices();
            for chunk in front.chunks(OUTPUT_CHUNK).chain(back.chunks(OUTPUT_CHUNK)) {
                write_frame(&mut out, &FromShim::Output(chunk).encode())?;
            }
            // One session at a time: the newest is the runner that is
            // alive, the old one's reader ends with its socket.
            if let Some(old) = shared.client.replace(out) {
                let _ = old.shutdown(std::net::Shutdown::Both);
            }
        }
        while let Some((kind, body)) = read_frame(&mut reader, &mut buf)? {
            match ToShim::decode(kind, body)? {
                ToShim::Hello { .. } => {}
                ToShim::Input(bytes) => {
                    let mut writer = lock(&self.writer);
                    // A child that exited no longer reads; its input is
                    // dropped like a local terminal's would be.
                    let _ = writer.write_all(bytes).and_then(|()| writer.flush());
                }
                ToShim::Resize { cols, rows } => {
                    let size = zeughaus_mux::Dimensions { cols, rows };
                    if size.is_valid() {
                        let _ = lock(&self.master).resize(pty_size(size));
                    }
                }
                ToShim::Redraw => {
                    if let Some(group) = lock(&self.master).process_group_leader() {
                        // SAFETY: plain signal delivery to a process group.
                        unsafe {
                            libc::kill(-group, libc::SIGWINCH);
                        }
                    }
                }
                ToShim::Close => self.close(),
            }
        }
        Ok(())
    }

    /// Ends the child and the shim, and removes the directory that made it
    /// findable. Does not return.
    fn close(&self) -> ! {
        {
            let mut shared = lock(&self.shared);
            shared.killed = true;
            if shared.exit.is_none() {
                let _ = lock(&self.killer).kill();
            }
        }
        let _ = std::fs::remove_dir_all(&self.dir);
        std::process::exit(0);
    }
}
