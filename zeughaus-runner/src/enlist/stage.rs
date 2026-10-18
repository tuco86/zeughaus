//! Files a run is staged from: secrets only its owner reads, input trees
//! shared by hard link, and the container engine.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

/// `$XDG_RUNTIME_DIR/zeughaus-ci/<run>`, or `<state>/ci/tmp/<run>` without
/// one (macOS and Windows have none): a tmpfs where it exists, mode 0700
/// on unix. On Windows the state directory's ACL is what keeps others out.
pub fn secret_dir(state_dir: &Path, run: &str) -> Result<PathBuf, String> {
    let root = std::env::var_os("XDG_RUNTIME_DIR")
        .filter(|v| !v.is_empty())
        .map(|dir| PathBuf::from(dir).join("zeughaus-ci"))
        .unwrap_or_else(|| super::tmp_dir(state_dir));
    let dir = root.join(run);
    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true);
    #[cfg(unix)]
    std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
    builder
        .create(&dir)
        .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700))
            .map_err(|e| format!("cannot restrict {}: {e}", dir.display()))?;
    }
    Ok(dir)
}

/// Writes a file only its owner can read (on Windows: whom the state
/// directory admits).
pub fn write_secret(path: &Path, text: &str) -> Result<(), String> {
    use std::io::Write;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create(true).truncate(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let mut file = options
        .open(path)
        .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
    file.write_all(text.as_bytes())
        .map_err(|e| format!("cannot write {}: {e}", path.display()))
}

/// Hard-links every file under `from` into `to` (copying where a link is
/// refused), creating directories as needed.
pub fn link_tree(from: &Path, to: &Path) -> std::io::Result<()> {
    std::fs::create_dir_all(to)?;
    for entry in std::fs::read_dir(from)? {
        let entry = entry?;
        let source = entry.path();
        let target = to.join(entry.file_name());
        let kind = entry.file_type()?;
        if kind.is_dir() {
            link_tree(&source, &target)?;
        } else if kind.is_symlink() {
            let _ = std::fs::remove_file(&target);
            #[cfg(unix)]
            std::os::unix::fs::symlink(std::fs::read_link(&source)?, &target)?;
            // A symlink needs a privilege on Windows; the file it names is
            // copied instead.
            #[cfg(not(unix))]
            std::fs::copy(&source, &target)?;
        } else {
            let _ = std::fs::remove_file(&target);
            if std::fs::hard_link(&source, &target).is_err() {
                std::fs::copy(&source, &target)?;
            }
        }
    }
    Ok(())
}

/// Whether `dir` exists and holds at least one entry.
pub fn has_entries(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut entries| entries.next().is_some())
}

/// The container engine of a host: Apple's `container` on macOS, rootless
/// podman elsewhere. There is no setting; a host has one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Engine {
    Podman,
    Apple,
}

impl Engine {
    pub fn host() -> Self {
        if cfg!(target_os = "macos") {
            Self::Apple
        } else {
            Self::Podman
        }
    }

    pub fn program(self) -> &'static str {
        match self {
            Self::Podman => "podman",
            Self::Apple => "container",
        }
    }
}

/// Runs `engine` with `args`; the error is its standard error.
pub fn engine(engine: Engine, args: &[&str]) -> Result<(), String> {
    let program = engine.program();
    let output = Command::new(program)
        .args(args)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run {program}: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_owned())
    }
}

// Hard links and their inode numbers are compared through unix metadata.
#[cfg(all(test, unix))]
mod tests {
    use super::*;

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`).
    mod io {
        use super::*;

        #[test]
        fn a_linked_tree_shares_its_files_and_keeps_its_shape() {
            use std::os::unix::fs::MetadataExt;
            let base = std::env::temp_dir().join(format!("zci-link-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&base);
            let from = base.join("from");
            std::fs::create_dir_all(from.join("sub")).unwrap();
            std::fs::write(from.join("sub/a"), "a").unwrap();
            let to = base.join("to");
            link_tree(&from, &to).unwrap();
            let same = std::fs::metadata(from.join("sub/a")).unwrap().ino()
                == std::fs::metadata(to.join("sub/a")).unwrap().ino();
            let _ = std::fs::remove_dir_all(&base);
            assert!(same);
        }
    }
}
