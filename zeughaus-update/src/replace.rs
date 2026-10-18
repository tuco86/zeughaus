//! Putting a new binary or app bundle over the one in place, keeping the one
//! it replaces as `<target>.previous`, and putting that back.
//!
//! A unix file is replaced by a rename over it: the path always names a
//! complete binary, and a running process keeps its image. Windows cannot
//! rename over a running executable but can rename it away, and a directory
//! cannot be renamed over on any platform, so those move the target aside
//! first ([`replace_aside`]).

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Command;

/// A file or an app bundle that is replaced in two steps: written next to
/// its target, then put over it by a rename. The first step can fail for a
/// full disk or a directory that is not writable, the second hardly ever, so
/// [`replace_all`] does the first for everything before the second for
/// anything.
pub struct Replacement {
    from: PathBuf,
    target: PathBuf,
    /// `<target>.new`, on the target's file system, where a rename works.
    staged: PathBuf,
    /// A directory tree (an app bundle) and not a file.
    bundle: bool,
}

impl Replacement {
    pub fn new(from: PathBuf, target: PathBuf, bundle: bool) -> Replacement {
        let staged = sibling(&target, ".new");
        Replacement {
            from,
            target,
            staged,
            bundle,
        }
    }

    fn stage(&self) -> Result<(), String> {
        if !self.from.exists() {
            let name = self.from.file_name().unwrap_or_default();
            return Err(format!("the archive has no {}", name.to_string_lossy()));
        }
        if let Some(dir) = self.target.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        }
        if self.bundle {
            // `ditto` keeps what an app bundle is made of: links, modes and
            // the code signature.
            let _ = std::fs::remove_dir_all(&self.staged);
            let output = Command::new("ditto")
                .arg(&self.from)
                .arg(&self.staged)
                .output()
                .map_err(|e| format!("cannot run ditto: {e}"))?;
            if !output.status.success() {
                return Err(format!("ditto: {}", crate::stderr_of(&output)));
            }
        } else {
            std::fs::copy(&self.from, &self.staged)
                .map_err(|e| format!("cannot write {}: {e}", self.staged.display()))?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(&self.staged, std::fs::Permissions::from_mode(0o755))
                    .map_err(|e| format!("cannot chmod {}: {e}", self.staged.display()))?;
            }
        }
        Ok(())
    }

    fn commit(&self) -> Result<(), String> {
        let previous = previous_of(&self.target);
        if self.bundle || cfg!(windows) {
            replace_aside(&self.staged, &self.target, &previous, self.bundle)
        } else {
            replace_keeping_link(&self.staged, &self.target, &previous)
        }
    }

    /// Removes what was written and not renamed into place.
    fn discard(&self) {
        if self.bundle {
            let _ = std::fs::remove_dir_all(&self.staged);
        } else {
            let _ = std::fs::remove_file(&self.staged);
        }
    }
}

/// Writes every replacement, then puts them in place.
pub fn replace_all(replacements: &[Replacement]) -> Result<(), String> {
    let replaced = replacements
        .iter()
        .try_for_each(Replacement::stage)
        .and_then(|()| replacements.iter().try_for_each(Replacement::commit));
    replacements.iter().for_each(Replacement::discard);
    replaced
}

/// `<target>.previous`.
pub fn previous_of(target: &Path) -> PathBuf {
    sibling(target, ".previous")
}

/// The unix way for a file: `previous` becomes a second name of the file in
/// place, then `staged` is renamed over `target`. At no moment is `target`
/// missing.
fn replace_keeping_link(staged: &Path, target: &Path, previous: &Path) -> Result<(), String> {
    if target.exists() {
        remove(previous, false)
            .map_err(|e| format!("cannot remove {}: {e}", previous.display()))?;
        if std::fs::hard_link(target, previous).is_err() {
            std::fs::copy(target, previous)
                .map_err(|e| format!("cannot keep {}: {e}", target.display()))?;
        }
    }
    std::fs::rename(staged, target).map_err(|e| format!("cannot replace {}: {e}", target.display()))
}

/// The way for a running Windows executable and for a directory: `target` is
/// renamed to `previous`, then `staged` to `target`. A failed second rename
/// puts `target` back: an old binary is better than none.
pub fn replace_aside(
    staged: &Path,
    target: &Path,
    previous: &Path,
    dir: bool,
) -> Result<(), String> {
    if !target.exists() {
        return std::fs::rename(staged, target)
            .map_err(|e| format!("cannot replace {}: {e}", target.display()));
    }
    // The previous build stays until the new one is in place: a replace
    // that fails leaves both builds as they were.
    let older = sibling(previous, ".old");
    remove(&older, dir).map_err(|e| format!("cannot remove {}: {e}", older.display()))?;
    let had_previous = previous.exists();
    if had_previous {
        std::fs::rename(previous, &older)
            .map_err(|e| format!("cannot move {} aside: {e}", previous.display()))?;
    }
    let put_older_back = || {
        if had_previous {
            let _ = std::fs::rename(&older, previous);
        }
    };
    if let Err(e) = std::fs::rename(target, previous) {
        put_older_back();
        return Err(format!("cannot move {} aside: {e}", target.display()));
    }
    if let Err(e) = std::fs::rename(staged, target) {
        let _ = std::fs::rename(previous, target);
        put_older_back();
        return Err(format!("cannot replace {}: {e}", target.display()));
    }
    // A running Windows executable cannot be removed; the next replace
    // tries again.
    let _ = remove(&older, dir);
    Ok(())
}

/// Puts `<target>.previous` back in place of `target`. `Ok(false)` when there
/// is no previous one.
pub fn restore(target: &Path, dir: bool) -> Result<bool, String> {
    let previous = previous_of(target);
    if !previous.exists() {
        return Ok(false);
    }
    if dir || cfg!(windows) {
        restore_aside(target, &previous, dir)?;
    } else {
        std::fs::rename(&previous, target)
            .map_err(|e| format!("cannot put {} back: {e}", previous.display()))?;
    }
    Ok(true)
}

/// The way back for a running Windows executable and for a directory: the
/// failed `target` goes to `<target>.failed` and is removed if it can be (a
/// running executable cannot; the next restore removes it).
pub fn restore_aside(target: &Path, previous: &Path, dir: bool) -> Result<(), String> {
    let failed = sibling(target, ".failed");
    let _ = remove(&failed, dir);
    if target.exists() {
        std::fs::rename(target, &failed)
            .map_err(|e| format!("cannot move {} aside: {e}", target.display()))?;
    }
    if let Err(e) = std::fs::rename(previous, target) {
        let _ = std::fs::rename(&failed, target);
        return Err(format!("cannot put {} back: {e}", previous.display()));
    }
    let _ = remove(&failed, dir);
    Ok(())
}

/// Removes a file or a directory tree; one that is not there is fine.
fn remove(path: &Path, dir: bool) -> std::io::Result<()> {
    let removed = if dir {
        std::fs::remove_dir_all(path)
    } else {
        std::fs::remove_file(path)
    };
    match removed {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        other => other,
    }
}

/// `path` with `suffix` appended to its file name.
pub fn sibling(path: &Path, suffix: &str) -> PathBuf {
    let mut name: OsString = path.as_os_str().to_owned();
    name.push(suffix);
    PathBuf::from(name)
}
