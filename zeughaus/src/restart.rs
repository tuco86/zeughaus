//! Replacing this editor with a fresh build of itself: `SIGUSR1` saves what
//! the window shows, closes the runner connections cleanly and `exec`s the
//! binary again with the same arguments. The new image reads the restore
//! file named by [`crate::app::restore::ENV`].

use std::path::{Path, PathBuf};
use std::sync::LazyLock;

use iced::futures::Stream;

static EXE: LazyLock<Option<PathBuf>> = LazyLock::new(|| std::env::current_exe().ok());

/// Resolves this executable's path while it still names the running image.
/// After a rebuild replaced the file, `/proc/self/exe` reads "(deleted)";
/// the path taken here is the one that names the new build.
pub fn remember_exe() {
    LazyLock::force(&EXE);
}

/// Replaces this process with the executable [`remember_exe`] saw, same
/// arguments, with `restore` in its environment. Returns only on failure.
pub fn exec(restore: &Path) -> std::io::Error {
    use std::os::unix::process::CommandExt;

    let Some(exe) = EXE.as_ref() else {
        return std::io::Error::other("this executable was not located at start");
    };
    std::process::Command::new(exe)
        .args(std::env::args_os().skip(1))
        .env(crate::app::restore::ENV, restore)
        .exec()
}

/// One item per `SIGUSR1`. Installing the handler is also what keeps the
/// signal's default action -- ending the process -- from applying.
pub fn sigusr1() -> impl Stream<Item = ()> {
    use tokio::signal::unix::{Signal, SignalKind, signal};

    iced::futures::stream::unfold(None::<Signal>, |held| async move {
        let mut usr1 = match held {
            Some(usr1) => usr1,
            None => match signal(SignalKind::user_defined1()) {
                Ok(usr1) => usr1,
                Err(e) => {
                    eprintln!("[editor] no restart signal: {e}");
                    return None;
                }
            },
        };
        usr1.recv().await?;
        Some(((), Some(usr1)))
    })
}
