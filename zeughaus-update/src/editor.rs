//! The editor installed beside the runner. It is replaced with it so that the
//! two ends of the link always run the same build, and its running copies
//! are asked to restart into what was installed.

use std::path::{Path, PathBuf};

use crate::replace::Replacement;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Editor {
    /// `zeughaus` in the runner's directory.
    Binary(PathBuf),
    /// `~/Applications/Zeughaus.app`, where `deploy/macos-app.sh` puts it.
    App(PathBuf),
}

impl Editor {
    pub fn find(exe: &Path, home: Option<&Path>) -> Option<Editor> {
        if cfg!(target_os = "macos") {
            let app = home?.join("Applications/Zeughaus.app");
            app.is_dir().then_some(Editor::App(app))
        } else {
            let binary = exe.with_file_name(format!("zeughaus{}", std::env::consts::EXE_SUFFIX));
            binary.is_file().then_some(Editor::Binary(binary))
        }
    }

    /// The path the editor is installed at.
    pub fn target(&self) -> &Path {
        match self {
            Editor::Binary(path) | Editor::App(path) => path,
        }
    }

    pub fn is_bundle(&self) -> bool {
        matches!(self, Editor::App(_))
    }

    /// This editor's replacement from the unpacked release at `top`.
    pub fn replacement(&self, top: &Path) -> Replacement {
        let from = match self {
            Editor::Binary(path) => top.join(path.file_name().expect("the editor has a name")),
            Editor::App(_) => top.join("Zeughaus.app"),
        };
        Replacement::new(from, self.target().to_path_buf(), self.is_bundle())
    }

    /// Asks every running copy to restart into what is installed now. `old`
    /// is the build they run, for what is said where they cannot be asked.
    pub fn restart(&self, old: u64) {
        #[cfg(unix)]
        {
            let _ = old;
            match self {
                Editor::Binary(path) => restart_binary_editors(path),
                Editor::App(app) => restart_app_editors(app),
            }
        }
        #[cfg(not(unix))]
        eprintln!("[update] running editors keep build {old} until they are reopened");
    }
}

/// Whether `exe`, what `/proc/<pid>/exe` links to, is the file `editor`, or
/// was: the kernel names the executable of a process whose file was replaced
/// `<path> (deleted)`.
#[cfg(unix)]
fn runs_file(exe: &Path, editor: &Path) -> bool {
    let mut replaced = editor.as_os_str().to_owned();
    replaced.push(" (deleted)");
    exe == editor || exe.as_os_str() == replaced
}

/// Whether a `/proc/<pid>/status` says the process catches `SIGUSR1`. One
/// that does not would be ended by the signal, not restarted.
#[cfg(unix)]
fn catches_usr1(status: &str) -> bool {
    status
        .lines()
        .find_map(|line| line.strip_prefix("SigCgt:"))
        .and_then(|mask| u64::from_str_radix(mask.trim(), 16).ok())
        .is_some_and(|mask| mask & (1u64 << (libc::SIGUSR1 - 1)) != 0)
}

/// `SIGUSR1` to every running copy of the editor binary `editor`, after an
/// update or an install replaced it.
#[cfg(unix)]
pub fn restart_binary_editors(editor: &Path) {
    let Ok(processes) = std::fs::read_dir("/proc") else {
        return;
    };
    for process in processes.flatten() {
        let Some(pid) = process
            .file_name()
            .to_str()
            .and_then(|name| name.parse::<libc::pid_t>().ok())
        else {
            continue;
        };
        // Another user's process does not show its executable.
        let Ok(exe) = std::fs::read_link(process.path().join("exe")) else {
            continue;
        };
        if !runs_file(&exe, editor) {
            continue;
        }
        let status = std::fs::read_to_string(process.path().join("status")).unwrap_or_default();
        if catches_usr1(&status) {
            restart_editor(pid);
        } else {
            eprintln!("[update] editor {pid} cannot restart itself; close and reopen it");
        }
    }
}

/// `SIGUSR1` to every running copy of the editor inside `app`.
#[cfg(unix)]
pub fn restart_app_editors(app: &Path) {
    let pattern = format!("{}/Contents/MacOS/zeughaus", app.display());
    let output = match std::process::Command::new("pgrep")
        .arg("-f")
        .arg(&pattern)
        .output()
    {
        Ok(output) => output,
        Err(e) => {
            eprintln!("[update] cannot run pgrep: {e}");
            return;
        }
    };
    // `pgrep` exits 1 when nothing matches, and prints nothing.
    for pid in String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<libc::pid_t>().ok())
    {
        restart_editor(pid);
    }
}

#[cfg(unix)]
fn restart_editor(pid: libc::pid_t) {
    // SAFETY: `kill` takes two integers; a process that ended meanwhile
    // answers with `ESRCH`.
    if unsafe { libc::kill(pid, libc::SIGUSR1) } == 0 {
        eprintln!("[update] restarting editor {pid}");
    } else {
        eprintln!(
            "[update] cannot signal editor {pid}: {}",
            std::io::Error::last_os_error()
        );
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn a_replaced_editor_is_still_found() {
        let editor = Path::new("/home/u/.cargo/bin/zeughaus");
        assert!(runs_file(editor, editor));
        assert!(runs_file(
            Path::new("/home/u/.cargo/bin/zeughaus (deleted)"),
            editor
        ));
        assert!(!runs_file(
            Path::new("/home/u/.cargo/bin/zeughaus-runner"),
            editor
        ));
        assert!(!runs_file(Path::new("/usr/bin/zeughaus"), editor));
        assert!(!runs_file(
            Path::new("/home/u/.cargo/bin/zeughaus-runner (deleted)"),
            editor
        ));
    }

    #[test]
    fn only_a_process_that_catches_usr1_is_signalled() {
        let bit = 1u64 << (libc::SIGUSR1 - 1);
        let status = |caught: u64| {
            format!(
                "Name:\tzeughaus\nSigBlk:\t0000000000000000\nSigIgn:\t0000000000001000\n\
                 SigCgt:\t{caught:016x}\n"
            )
        };
        assert!(catches_usr1(&status(bit)));
        assert!(catches_usr1(&status(bit | 1)));
        assert!(!catches_usr1(&status(0)));
        assert!(!catches_usr1(&status(!bit)));
        assert!(!catches_usr1("Name:\tzeughaus\n"));
        assert!(!catches_usr1("SigCgt:\tnot hex\n"));
    }

    /// Tests that touch the filesystem or processes: the integration tier.
    #[cfg(target_os = "linux")]
    mod io {
        use std::process::Command;
        use std::time::Duration;

        use super::*;

        /// A copy of the shell at `<root>/editor/zeughaus` running `script`,
        /// then replaced on disk, the way an update replaces an editor: a
        /// process whose executable is a file that is gone. `script` is given
        /// the path of a file to create once it has set itself up.
        fn running_editor(root: &Path, script: &str) -> (PathBuf, std::process::Child) {
            let editor = root.join("editor/zeughaus");
            std::fs::create_dir_all(editor.parent().unwrap()).unwrap();
            std::fs::copy("/bin/sh", &editor).unwrap();
            let ready = root.join("ready");
            let mut attempts = 0;
            let child = loop {
                match Command::new(&editor)
                    .args(["-c", script, "sh"])
                    .arg(&ready)
                    .spawn()
                {
                    // The copy is still open for writing in another test's fork.
                    Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) && attempts < 100 => {
                        attempts += 1;
                        std::thread::sleep(Duration::from_millis(20));
                    }
                    spawned => break spawned.unwrap(),
                }
            };
            for _ in 0..500 {
                if ready.exists() {
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            assert!(ready.exists(), "the script did not start");
            std::fs::write(root.join("editor/zeughaus.new"), "new").unwrap();
            std::fs::rename(root.join("editor/zeughaus.new"), &editor).unwrap();
            (editor, child)
        }

        fn scratch(tag: &str) -> PathBuf {
            let root = std::env::temp_dir().join(format!(
                "zeughaus-update-editor-{tag}-{}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&root);
            std::fs::create_dir_all(&root).unwrap();
            // `/proc/<pid>/exe` names the resolved path.
            std::fs::canonicalize(&root).unwrap()
        }

        #[test]
        fn a_running_editor_that_catches_usr1_is_restarted() {
            let root = scratch("usr1");
            let (editor, mut child) = running_editor(
                &root,
                "trap 'exit 0' USR1; touch \"$1\"; while :; do sleep 0.05; done",
            );
            restart_binary_editors(&editor);
            let ended = (0..500).find_map(|_| {
                std::thread::sleep(Duration::from_millis(10));
                child.try_wait().unwrap()
            });
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&root);
            assert_eq!(
                ended.and_then(|status| status.code()),
                Some(0),
                "the editor was not signalled"
            );
        }

        #[test]
        fn a_running_editor_that_does_not_catch_usr1_is_left_alone() {
            let root = scratch("no-usr1");
            let (editor, mut child) =
                running_editor(&root, "touch \"$1\"; while :; do sleep 0.05; done");
            restart_binary_editors(&editor);
            // The signal's default action would end the shell within moments.
            std::thread::sleep(Duration::from_millis(300));
            let alive = child.try_wait().unwrap().is_none();
            let _ = child.kill();
            let _ = child.wait();
            let _ = std::fs::remove_dir_all(&root);
            assert!(
                alive,
                "a process that cannot restart itself must not be signalled"
            );
        }
    }
}
