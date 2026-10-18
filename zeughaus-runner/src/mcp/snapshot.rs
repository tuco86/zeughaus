//! A working tree as the git bundles a manual run uploads.
//!
//! The CI takes one bundle per checkout. Each holds one commit without a
//! parent whose tree is what the checkout contains right now: the files of
//! `HEAD`, the changes made to them, and the untracked files the ignore
//! rules do not exclude. The commit is built in a temporary index with git
//! plumbing, so the checkout's own index, branch and refs are never touched;
//! the one ref `git bundle` needs exists only while it runs.
//!
//! The checkout of the call is the main one. A Cargo path dependency outside
//! it is another checkout the build needs; it is snapshotted the same way
//! when it sits beside the main one, because the runner puts such checkouts
//! in one directory (`CI_SIBLINGS`) by their directory name, which the
//! `../name` paths of the build resolve against.

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::Value;
use zeughaus_link::enlist::UploadDir;
use zeughaus_link::manual::{MAX_MANUAL_BUNDLE_BYTES, MAX_MANUAL_DIRS, RunUpload, valid_dir_name};

/// Where the bundled commit is named while `git bundle` reads it.
const BUNDLE_REF: &str = "refs/zci-manual";

/// The author and committer of a snapshot commit.
const IDENTITY: [(&str, &str); 4] = [
    ("GIT_AUTHOR_NAME", "zeughaus-manual"),
    ("GIT_AUTHOR_EMAIL", "manual@zeughaus.invalid"),
    ("GIT_COMMITTER_NAME", "zeughaus-manual"),
    ("GIT_COMMITTER_EMAIL", "manual@zeughaus.invalid"),
];

/// The longest summary a pipeline carries as its subject.
const SUMMARY_CHARS: usize = 200;

/// Makes the names of temporary directories and bundle refs unique within
/// this process; the process id separates processes.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// The snapshot of one call, and the temporary directory its bundles are in.
/// The directory goes away with the value.
pub struct Snapshot {
    origin: String,
    actor: String,
    summary: String,
    /// The main checkout first, then the siblings by directory name.
    pub dirs: Vec<UploadDir>,
    /// `dirs[i]`'s bundle.
    pub bundles: Vec<PathBuf>,
    sizes: Vec<u64>,
    _temp: TempDir,
}

impl Snapshot {
    /// The first frame of the `/manual` exchange for this snapshot: a run of
    /// `jobs` of the repository's pipeline.
    pub fn upload(&self, jobs: &[String]) -> RunUpload {
        RunUpload {
            origin: self.origin.clone(),
            actor: self.actor.clone(),
            summary: self.summary.clone(),
            jobs: jobs.to_vec(),
            dirs: self.dirs.clone(),
            sizes: self.sizes.clone(),
        }
    }
}

/// Snapshots the checkout `path` is in, and the checkouts its Cargo path
/// dependencies need.
///
/// Blocking: runs git and cargo. Call it on the blocking pool.
pub fn take(path: &Path) -> Result<Snapshot, String> {
    if !path.is_dir() {
        return Err(format!("{} is not a directory", path.display()));
    }
    let root = toplevel(path)?;
    let origin = git(&root, &[], &["remote", "get-url", "origin"], None)
        .map(|url| without_credentials(url.trim()))
        .ok()
        .filter(|url| !url.is_empty())
        .ok_or("a manual run needs an origin remote to name the repository")?;

    let mut checkouts = vec![root.clone()];
    checkouts.extend(siblings(&root)?);
    if checkouts.len() > MAX_MANUAL_DIRS {
        return Err(format!(
            "a manual run carries at most {MAX_MANUAL_DIRS} checkouts and this one needs {}",
            checkouts.len()
        ));
    }

    let temp = TempDir::new()?;
    let mut dirs = Vec::with_capacity(checkouts.len());
    let mut bundles = Vec::with_capacity(checkouts.len());
    let mut sizes = Vec::with_capacity(checkouts.len());
    for (index, checkout) in checkouts.iter().enumerate() {
        let dir = checkout
            .file_name()
            .and_then(OsStr::to_str)
            .filter(|name| valid_dir_name(name))
            .ok_or_else(|| {
                format!(
                    "the directory name of {} is not one a manual run can carry: letters, \
                     digits, `.`, `_` and `-`, at most 100 characters",
                    checkout.display()
                )
            })?;
        let sha =
            commit(checkout, temp.path(), index).map_err(|e| format!("snapshot of {dir}: {e}"))?;
        let bundle = temp.path().join(format!("{index}.bundle"));
        let size =
            write_bundle(checkout, &sha, &bundle).map_err(|e| format!("bundle of {dir}: {e}"))?;
        if size > MAX_MANUAL_BUNDLE_BYTES {
            return Err(format!(
                "the bundle of {dir} is {size} bytes and a manual run takes at most \
                 {MAX_MANUAL_BUNDLE_BYTES}"
            ));
        }
        dirs.push(UploadDir {
            dir: dir.to_owned(),
            sha,
        });
        bundles.push(bundle);
        sizes.push(size);
    }
    Ok(Snapshot {
        origin,
        actor: actor(),
        summary: summary(&root),
        dirs,
        bundles,
        sizes,
        _temp: temp,
    })
}

/// One commit without a parent whose tree is `dir`'s working tree: `HEAD`'s
/// files with every change, plus the untracked files not ignored. Returns
/// its id.
fn commit(dir: &Path, temp: &Path, index: usize) -> Result<String, String> {
    let index_file = temp.join(format!("index-{index}"));
    let in_temp_index = [("GIT_INDEX_FILE", index_file.as_os_str())];
    let unborn = git(
        dir,
        &[],
        &["rev-parse", "--verify", "--quiet", "HEAD"],
        None,
    )
    .is_err();
    git(
        dir,
        &in_temp_index,
        &["read-tree", if unborn { "--empty" } else { "HEAD" }],
        None,
    )?;
    git(dir, &in_temp_index, &["add", "-A"], None)?;
    let tree = git(dir, &in_temp_index, &["write-tree"], None)?;
    let identity: Vec<(&str, &OsStr)> = IDENTITY
        .iter()
        .map(|(key, value)| (*key, OsStr::new(*value)))
        .collect();
    let sha = git(
        dir,
        &identity,
        &["commit-tree", tree.trim(), "-m", "zeughaus manual run"],
        None,
    )?;
    Ok(sha.trim().to_owned())
}

/// Writes the commit `sha` as the bundle `to` and returns its size. The
/// commit is named by a ref for as long as that takes, and the ref is gone
/// whatever happens.
fn write_bundle(dir: &Path, sha: &str, to: &Path) -> Result<u64, String> {
    let name = format!(
        "{BUNDLE_REF}/{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    );
    git(dir, &[], &["update-ref", &name, sha], None)?;
    let made = git(
        dir,
        &[],
        &["bundle", "create", "-q", &to.to_string_lossy(), &name],
        None,
    );
    let _ = git(dir, &[], &["update-ref", "-d", &name], None);
    made?;
    std::fs::metadata(to)
        .map(|meta| meta.len())
        .map_err(|e| format!("cannot stat {}: {e}", to.display()))
}

/// The git checkouts beside `root` that its Cargo path dependencies live in,
/// sorted by directory name.
///
/// A path dependency is a package of `cargo metadata` without a source whose
/// manifest is outside `root`. Each must be in a git checkout that has the
/// same parent directory as `root`: the runner lays the checkouts out side
/// by side, and a dependency anywhere else would not be where its
/// `path = "../.."` says.
fn siblings(root: &Path) -> Result<Vec<PathBuf>, String> {
    let manifest = root.join("Cargo.toml");
    if !manifest.is_file() {
        return Ok(Vec::new());
    }
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(&cargo)
        .args(["metadata", "--format-version", "1", "--manifest-path"])
        .arg(&manifest)
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("cannot run cargo: {e}"))?;
    if !output.status.success() {
        return Err(format!(
            "cargo metadata: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    let metadata: Value =
        serde_json::from_slice(&output.stdout).map_err(|e| format!("cargo metadata: {e}"))?;

    let real_root = canonical(root)?;
    let mut found: BTreeMap<PathBuf, PathBuf> = BTreeMap::new();
    for package in metadata["packages"].as_array().into_iter().flatten() {
        if !package["source"].is_null() {
            continue;
        }
        let Some(dir) = package["manifest_path"]
            .as_str()
            .and_then(|manifest| Path::new(manifest).parent())
        else {
            continue;
        };
        if canonical(dir)?.starts_with(&real_root) {
            continue;
        }
        let not_a_sibling = || {
            format!(
                "path dependency {} is not a sibling checkout of {}",
                dir.display(),
                root.display()
            )
        };
        let top = toplevel(dir).map_err(|_| not_a_sibling())?;
        let real_top = canonical(&top)?;
        if real_top.parent() != real_root.parent() {
            return Err(not_a_sibling());
        }
        found.insert(real_top, top);
    }
    let mut tops: Vec<PathBuf> = found.into_values().collect();
    tops.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
    Ok(tops)
}

fn canonical(path: &Path) -> Result<PathBuf, String> {
    std::fs::canonicalize(path).map_err(|e| format!("cannot resolve {}: {e}", path.display()))
}

pub(super) fn toplevel(dir: &Path) -> Result<PathBuf, String> {
    git(dir, &[], &["rev-parse", "--show-toplevel"], None)
        .map(|top| PathBuf::from(top.trim_end_matches(['\n', '\r'])))
        .map_err(|e| format!("{} is not inside a git checkout ({e})", dir.display()))
}

/// `git -C <dir> <args>`; stdout on success. `env` is added to the
/// environment of the process and `stdin` is what it reads.
pub(super) fn git(
    dir: &Path,
    env: &[(&str, &OsStr)],
    args: &[&str],
    stdin: Option<&[u8]>,
) -> Result<String, String> {
    let what = args.first().copied().unwrap_or_default();
    let mut command = Command::new("git");
    command
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(if stdin.is_some() {
            Stdio::piped()
        } else {
            Stdio::null()
        })
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for (key, value) in env {
        command.env(key, value);
    }
    let mut child = command
        .spawn()
        .map_err(|e| format!("cannot run git: {e}"))?;
    if let (Some(bytes), Some(mut pipe)) = (stdin, child.stdin.take()) {
        // Dropped at the end of the block, which is what ends git's input.
        pipe.write_all(bytes)
            .map_err(|e| format!("git {what}: {e}"))?;
    }
    let output = child
        .wait_with_output()
        .map_err(|e| format!("git {what}: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "git {what}: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        ))
    }
}

/// `url` without the user and password of its authority. A remote may carry
/// a token, and the URL is stored by the CI and shown in its logs.
pub(super) fn without_credentials(url: &str) -> String {
    let Some((scheme, rest)) = url.split_once("://") else {
        return url.to_owned();
    };
    let (authority, path) = rest.split_at(rest.find('/').unwrap_or(rest.len()));
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    format!("{scheme}://{host}{path}")
}

/// The subject a pipeline shows for the run: the first line of `root`'s
/// `HEAD` commit message, or `working tree` when it has no commit. Cut to
/// [`SUMMARY_CHARS`].
fn summary(root: &Path) -> String {
    let subject = git(root, &[], &["log", "-1", "--format=%s"], None)
        .ok()
        .map(|text| text.trim().to_owned())
        .filter(|text| !text.is_empty());
    match subject {
        Some(text) if text.chars().count() > SUMMARY_CHARS => {
            let kept: String = text.chars().take(SUMMARY_CHARS).collect();
            format!("{kept}...")
        }
        Some(text) => text,
        None => "working tree".to_owned(),
    }
}

/// `<user>@<host>`, with only the characters a name can hold.
fn actor() -> String {
    let user = ["USER", "USERNAME", "LOGNAME"]
        .iter()
        .find_map(|key| std::env::var(key).ok().filter(|value| !value.is_empty()))
        .unwrap_or_else(|| "unknown".to_owned());
    let name = format!("{user}@{}", hostname());
    name.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '@') {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn hostname() -> String {
    let named = |text: String| Some(text.trim().to_owned()).filter(|name| !name.is_empty());
    Command::new("hostname")
        .stdin(Stdio::null())
        .output()
        .ok()
        .filter(|output| output.status.success())
        .and_then(|output| named(String::from_utf8_lossy(&output.stdout).into_owned()))
        .or_else(|| {
            std::fs::read_to_string("/proc/sys/kernel/hostname")
                .ok()
                .and_then(named)
        })
        .or_else(|| {
            ["HOSTNAME", "COMPUTERNAME"]
                .iter()
                .find_map(|key| std::env::var(key).ok().and_then(named))
        })
        .unwrap_or_else(|| "unknown".to_owned())
}

/// A directory of this process's own under the system's temporary one,
/// private to the user because it holds their source, removed on drop.
struct TempDir(PathBuf);

impl TempDir {
    fn new() -> Result<TempDir, String> {
        let path = std::env::temp_dir().join(format!(
            "zeughaus-manual-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        // What an earlier process with this id left behind.
        let _ = std::fs::remove_dir_all(&path);
        // Only the user reads it: it holds their source.
        #[cfg(unix)]
        let builder = {
            let mut builder = std::fs::DirBuilder::new();
            std::os::unix::fs::DirBuilderExt::mode(&mut builder, 0o700);
            builder
        };
        #[cfg(not(unix))]
        let builder = std::fs::DirBuilder::new();
        builder
            .create(&path)
            .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
        Ok(TempDir(path))
    }

    fn path(&self) -> &Path {
        &self.0
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_remotes_credentials_do_not_travel() {
        for (url, expected) in [
            (
                "https://user:token@github.com/o/r.git",
                "https://github.com/o/r.git",
            ),
            ("https://token@host:8443/o/r", "https://host:8443/o/r"),
            ("ssh://git@host/o/r.git", "ssh://host/o/r.git"),
            ("https://host/o/r.git", "https://host/o/r.git"),
            ("git@github.com:o/r.git", "git@github.com:o/r.git"),
            ("/srv/git/r.git", "/srv/git/r.git"),
            ("https://u@host", "https://host"),
        ] {
            assert_eq!(without_credentials(url), expected, "{url}");
        }
    }

    /// Tests that touch the filesystem, network, processes or real time: the
    /// integration tier (`.config/nextest.toml`).
    mod io {
        use super::*;

        /// A directory tree under the system's temporary one, removed on drop.
        struct Scratch(PathBuf);

        impl Scratch {
            fn new(tag: &str) -> Scratch {
                let path = std::env::temp_dir().join(format!(
                    "zeughaus-mcp-{tag}-{}-{}",
                    std::process::id(),
                    COUNTER.fetch_add(1, Ordering::Relaxed)
                ));
                let _ = std::fs::remove_dir_all(&path);
                std::fs::create_dir_all(&path).expect("scratch directory");
                Scratch(path)
            }
        }

        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }

        /// Git as a test fixture runs it: its own identity, whatever the
        /// developer has configured.
        fn fx(dir: &Path, args: &[&str]) -> String {
            let output = Command::new("git")
                .current_dir(dir)
                .args(["-c", "user.name=fx", "-c", "user.email=fx@example.invalid"])
                .args(args)
                .output()
                .expect("run git");
            assert!(
                output.status.success(),
                "git {args:?}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            String::from_utf8_lossy(&output.stdout).trim().to_owned()
        }

        fn write(path: impl AsRef<Path>, text: &str) {
            let path = path.as_ref();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("parent directory");
            }
            std::fs::write(path, text).expect("write fixture file");
        }

        /// A repository with one commit holding `files`.
        fn repo(dir: &Path, origin: Option<&str>, files: &[(&str, &str)]) {
            std::fs::create_dir_all(dir).expect("repository directory");
            fx(dir, &["init", "-q"]);
            for (name, text) in files {
                write(dir.join(name), text);
            }
            fx(dir, &["add", "."]);
            fx(dir, &["commit", "-q", "-m", "first"]);
            if let Some(origin) = origin {
                fx(dir, &["remote", "add", "origin", origin]);
            }
        }

        fn crate_files<'a>(name: &'a str, dependency: Option<&'a str>) -> [(&'a str, String); 2] {
            // A dependency is named like the package in the directory it points to.
            let dependency = dependency
                .map(|path| {
                    let name = Path::new(path).file_name().and_then(OsStr::to_str).unwrap();
                    format!("{name} = {{ path = \"{path}\" }}\n")
                })
                .unwrap_or_default();
            [
                (
                    "Cargo.toml",
                    format!(
                        "[package]\nname = \"{name}\"\nversion = \"0.1.0\"\nedition = \"2021\"\n\n\
                     [dependencies]\n{dependency}"
                    ),
                ),
                ("src/lib.rs", String::new()),
            ]
        }

        fn crate_repo(dir: &Path, origin: Option<&str>, dependency: Option<&str>) {
            let files = crate_files(dir.file_name().and_then(OsStr::to_str).unwrap(), dependency);
            let files: Vec<(&str, &str)> = files.iter().map(|(n, t)| (*n, t.as_str())).collect();
            repo(dir, origin, &files);
        }

        #[test]
        fn a_snapshot_holds_the_changes_and_the_untracked_files_but_not_the_ignored_ones() {
            let base = Scratch::new("tree");
            let dir = base.0.join("griasdi");
            repo(
                &dir,
                Some("git@github.com:Griasdi/griasdi.git"),
                &[("tracked.txt", "one\n"), (".gitignore", "ignored/\n")],
            );
            write(dir.join("tracked.txt"), "two\n");
            write(dir.join("untracked.txt"), "new\n");
            write(dir.join("ignored/junk"), "junk\n");
            // A commit made here would be signed with this program, which fails:
            // a snapshot must not depend on the developer's signing setup.
            fx(&dir, &["config", "commit.gpgsign", "true"]);
            fx(&dir, &["config", "gpg.program", "false"]);
            let status_before = fx(&dir, &["status", "--porcelain"]);

            let snapshot = take(&dir).expect("snapshot");

            assert_eq!(snapshot.origin, "git@github.com:Griasdi/griasdi.git");
            assert_eq!(snapshot.dirs.len(), 1);
            assert_eq!(snapshot.dirs[0].dir, "griasdi");
            let sha = &snapshot.dirs[0].sha;
            assert_eq!(sha.len(), 40, "{sha}");
            let files: Vec<String> = fx(&dir, &["ls-tree", "-r", "--name-only", sha])
                .lines()
                .map(str::to_owned)
                .collect();
            assert_eq!(files, [".gitignore", "tracked.txt", "untracked.txt",]);
            assert_eq!(fx(&dir, &["show", &format!("{sha}:tracked.txt")]), "two");
            // No parent and no signature.
            let object = fx(&dir, &["cat-file", "-p", sha]);
            assert!(!object.contains("parent "), "{object}");
            assert!(!object.contains("gpgsig"), "{object}");

            // The checkout itself is as it was, and no ref is left behind.
            assert_eq!(fx(&dir, &["status", "--porcelain"]), status_before);
            assert_eq!(fx(&dir, &["for-each-ref", BUNDLE_REF]), "");

            // What the runner does with the bundle: fetch it into a repository
            // of its own and check the commit out.
            let runner = base.0.join("runner");
            std::fs::create_dir_all(&runner).expect("runner directory");
            fx(&runner, &["init", "-q"]);
            let bundle = snapshot.bundles[0].to_string_lossy().into_owned();
            fx(
                &runner,
                &[
                    "fetch",
                    "-q",
                    "--no-tags",
                    &bundle,
                    "+refs/zci-manual/*:refs/zci-manual/*",
                ],
            );
            fx(&runner, &["checkout", "-q", "--force", "--detach", sha]);
            assert_eq!(
                std::fs::read_to_string(runner.join("tracked.txt")).unwrap(),
                "two\n"
            );
            assert!(runner.join("untracked.txt").is_file());
            assert!(!runner.join("ignored").exists());

            let upload = snapshot.upload(&["build-x".to_owned(), "check-x".to_owned()]);
            assert_eq!(upload.jobs, ["build-x", "check-x"]);
            assert_eq!(upload.summary, "first");
            assert_eq!(
                upload.sizes,
                [std::fs::metadata(&snapshot.bundles[0]).unwrap().len()]
            );
            assert!(upload.actor.contains('@'), "{}", upload.actor);

            let temp = snapshot._temp.0.clone();
            drop(snapshot);
            assert!(!temp.exists(), "the bundles go with the snapshot");
        }

        #[test]
        fn a_repository_without_commits_and_without_an_origin_is_refused_by_name() {
            let base = Scratch::new("origin");
            let dir = base.0.join("fresh");
            std::fs::create_dir_all(&dir).unwrap();
            fx(&dir, &["init", "-q"]);
            write(dir.join("a.txt"), "a\n");
            let refused = take(&dir).err().expect("no origin");
            assert_eq!(
                refused,
                "a manual run needs an origin remote to name the repository"
            );

            // With one, a repository that has no commit yet still snapshots.
            fx(
                &dir,
                &["remote", "add", "origin", "https://u:secret@host/o/r.git"],
            );
            let snapshot = take(&dir).expect("unborn HEAD");
            assert_eq!(snapshot.upload(&[]).summary, "working tree");
            assert_eq!(snapshot.origin, "https://host/o/r.git");
            let files = fx(
                &dir,
                &["ls-tree", "-r", "--name-only", &snapshot.dirs[0].sha],
            );
            assert!(files.contains("a.txt"), "{files}");
        }

        #[test]
        fn a_path_outside_a_checkout_is_refused() {
            let base = Scratch::new("outside");
            let err = take(&base.0).err().expect("not a repository");
            assert!(err.contains("is not inside a git checkout"), "{err}");
            let err = take(&base.0.join("missing")).err().expect("missing");
            assert!(err.contains("is not a directory"), "{err}");
        }

        #[test]
        fn a_sub_directory_call_snapshots_the_whole_checkout() {
            let base = Scratch::new("sub");
            let dir = base.0.join("proj");
            repo(
                &dir,
                Some("https://example.invalid/o/proj.git"),
                &[("crates/foo/a.txt", "a\n"), ("top.txt", "t\n")],
            );
            let snapshot = take(&dir.join("crates/foo")).expect("snapshot");
            assert_eq!(snapshot.dirs[0].dir, "proj");
            let sha = &snapshot.dirs[0].sha;
            assert_eq!(fx(&dir, &["show", &format!("{sha}:top.txt")]), "t");
        }

        #[test]
        fn the_subject_is_the_head_commits_or_working_tree() {
            let base = Scratch::new("summary");
            let dir = base.0.join("proj");
            repo(&dir, None, &[("a.txt", "a\n")]);
            assert_eq!(summary(&dir), "first");
            fx(
                &dir,
                &["commit", "-q", "--allow-empty", "-m", &"x".repeat(500)],
            );
            let long = summary(&dir);
            assert_eq!(long.chars().count(), SUMMARY_CHARS + 3);
            assert!(long.ends_with("..."));

            let fresh = base.0.join("fresh");
            std::fs::create_dir_all(&fresh).unwrap();
            fx(&fresh, &["init", "-q"]);
            assert_eq!(summary(&fresh), "working tree");
        }

        #[test]
        fn a_path_dependency_two_levels_up_is_refused() {
            let base = Scratch::new("far");
            crate_repo(&base.0.join("util"), None, None);
            let app = base.0.join("a/b/app");
            crate_repo(
                &app,
                Some("https://example.invalid/o/app.git"),
                Some("../../../util"),
            );
            let err = take(&app).err().expect("not a sibling");
            assert!(
                err.starts_with("path dependency ") && err.contains("is not a sibling checkout of"),
                "{err}"
            );
            assert!(err.contains("util") && err.contains("app"), "{err}");
        }

        #[test]
        fn sibling_path_dependencies_ride_along_with_their_uncommitted_changes() {
            let base = Scratch::new("siblings");
            let util = base.0.join("util");
            crate_repo(&util, None, None);
            write(util.join("src/lib.rs"), "pub fn answer() -> u32 { 42 }\n");
            let app = base.0.join("app");
            crate_repo(
                &app,
                Some("https://example.invalid/o/app.git"),
                Some("../util"),
            );

            let snapshot = take(&app).expect("snapshot");

            let names: Vec<&str> = snapshot.dirs.iter().map(|d| d.dir.as_str()).collect();
            assert_eq!(names, ["app", "util"]);
            assert_eq!(snapshot.bundles.len(), 2);
            assert_eq!(snapshot.upload(&["x".to_owned()]).sizes.len(), 2);
            let sha = &snapshot.dirs[1].sha;
            assert_eq!(
                fx(&util, &["show", &format!("{sha}:src/lib.rs")]),
                "pub fn answer() -> u32 { 42 }"
            );
        }

        #[test]
        fn a_directory_name_a_manual_run_cannot_carry_is_refused() {
            let base = Scratch::new("names");
            let dir = base.0.join("with space");
            repo(&dir, Some("https://example.invalid/o/r.git"), &[("a", "a")]);
            let err = take(&dir).err().expect("invalid name");
            assert!(err.contains("not one a manual run can carry"), "{err}");
        }
    }
}
