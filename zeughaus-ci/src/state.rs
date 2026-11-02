//! Job state: the stores a job names with `state = [..]`, kept by the CI from
//! one pipeline to the next, independent of workspaces, `clean` and runner
//! budgets.
//!
//! A version of a store is a [`SetManifest`] recorded under `ci/state/sets`
//! as `<repo>.<number>.<name>.set`, `number` being the pipeline that saved
//! it and the file's mtime the time it was saved; its files are zstd blobs
//! in the compressed store `ci/state/blobs`. The directory is separate from
//! the artifact store, so neither its expiry nor `artifact_store_gb` ever
//! removes a version: only [`record`] (beyond [`STATE_VERSIONS`]) and
//! [`delete`] do, and the sweep then takes the blobs no version names.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use weida::{IncomingRequest, Replier, TransferMeta};
use zeughaus_blobs::{Store, read_set, record_set};
use zeughaus_link::ci::StateDownload;
use zeughaus_link::enlist::{
    ARTIFACT_CHUNK, ArtifactReply, BlobHead, SetEntry, SetManifest, parse_run_id, read_frame,
    run_id, write_chunk, write_end, write_frame,
};
use zeughaus_link::{StateVersionView, StateView};

use crate::config::{Shared, valid_name};
use crate::transport::editor_principal;

/// The versions the CI keeps of each store.
pub const STATE_VERSIONS: usize = 3;

/// The compressed blob store of every version.
pub fn blobs_dir(state_dir: &Path) -> PathBuf {
    crate::ci_dir(state_dir).join("state/blobs")
}

/// The recorded versions.
pub fn sets_dir(state_dir: &Path) -> PathBuf {
    crate::ci_dir(state_dir).join("state/sets")
}

fn check_names(repo: &str, name: &str) -> Result<(), String> {
    if !valid_name(repo) {
        return Err(format!("{repo:?} is no repository name"));
    }
    if !valid_name(name) {
        return Err(format!("{name:?} is no state name"));
    }
    Ok(())
}

/// Every recorded version in `sets_dir` as (repo, number, name, path).
fn recorded(state_dir: &Path) -> Result<Vec<(String, u64, String, PathBuf)>, String> {
    let dir = sets_dir(state_dir);
    let read = match std::fs::read_dir(&dir) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(format!("{}: {e}", dir.display())),
    };
    let mut out = Vec::new();
    for entry in read.flatten() {
        let file = entry.file_name();
        let Some(id) = file.to_str().and_then(|f| f.strip_suffix(".set")) else {
            continue;
        };
        if let Some((repo, number, name)) = parse_run_id(id) {
            out.push((repo, number, name, entry.path()));
        }
    }
    Ok(out)
}

/// The version numbers of one store, newest first.
fn versions(state_dir: &Path, repo: &str, name: &str) -> Result<Vec<u64>, String> {
    check_names(repo, name)?;
    let mut numbers: Vec<u64> = recorded(state_dir)?
        .into_iter()
        .filter(|(r, _, n, _)| r == repo && n == name)
        .map(|(_, number, _, _)| number)
        .collect();
    numbers.sort_unstable_by(|a, b| b.cmp(a));
    Ok(numbers)
}

/// The newest version of a store, `None` while it has none.
pub fn latest(state_dir: &Path, repo: &str, name: &str) -> Result<Option<u64>, String> {
    Ok(versions(state_dir, repo, name)?.first().copied())
}

/// The manifest of one version, `None` when there is no such version.
pub fn read(
    state_dir: &Path,
    repo: &str,
    name: &str,
    number: u64,
) -> Result<Option<SetManifest>, String> {
    check_names(repo, name)?;
    read_set(&sets_dir(state_dir), &run_id(repo, number, name))
}

/// Records `manifest` as version `number` of the store, then removes the
/// versions beyond the [`STATE_VERSIONS`] highest numbers. A lower number
/// recorded after a higher one is kept as a version but never becomes the
/// newest: the newest pipeline wins.
pub fn record(
    state_dir: &Path,
    repo: &str,
    name: &str,
    number: u64,
    manifest: &SetManifest,
) -> Result<(), String> {
    check_names(repo, name)?;
    let sets = sets_dir(state_dir);
    record_set(&sets, &run_id(repo, number, name), manifest)?;
    for old in versions(state_dir, repo, name)?
        .into_iter()
        .skip(STATE_VERSIONS)
    {
        let path = sets.join(format!("{}.set", run_id(repo, old, name)));
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(format!("{}: {e}", path.display()));
        }
    }
    Ok(())
}

/// Every store of `repo` by name, versions newest first. A version that
/// cannot be read is left out and logged.
pub fn list(state_dir: &Path, repo: &str) -> Result<Vec<StateView>, String> {
    if !valid_name(repo) {
        return Err(format!("{repo:?} is no repository name"));
    }
    let mut found: Vec<(String, u64, PathBuf)> = recorded(state_dir)?
        .into_iter()
        .filter(|(r, _, _, _)| r == repo)
        .map(|(_, number, name, path)| (name, number, path))
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0).then(b.1.cmp(&a.1)));
    let mut states: Vec<StateView> = Vec::new();
    for (name, number, path) in found {
        let version = match version_view(&path, number) {
            Ok(version) => version,
            Err(e) => {
                eprintln!("[ci] state {}: {e}; skipped", path.display());
                continue;
            }
        };
        match states.last_mut() {
            Some(state) if state.name == name => state.versions.push(version),
            _ => states.push(StateView {
                name,
                versions: vec![version],
            }),
        }
    }
    Ok(states)
}

fn version_view(path: &Path, number: u64) -> Result<StateVersionView, String> {
    let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
    let saved = std::fs::metadata(path)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map_or(0, |d| d.as_secs());
    let manifest = SetManifest::decode(&bytes)?;
    let files = manifest
        .entries
        .iter()
        .filter(|e| matches!(e, SetEntry::File { .. }))
        .count() as u64;
    Ok(StateVersionView {
        number,
        saved,
        bytes: manifest.files(),
        files,
    })
}

/// Removes one version of a store, or every version when `version` is
/// `None`. Their blobs go with the next sweep.
pub fn delete(
    state_dir: &Path,
    repo: &str,
    name: &str,
    version: Option<u64>,
) -> Result<(), String> {
    let numbers = versions(state_dir, repo, name)?;
    let doomed: Vec<u64> = match version {
        None if numbers.is_empty() => return Err(format!("no state {name} in {repo}")),
        None => numbers,
        Some(v) if !numbers.contains(&v) => {
            return Err(format!("no version #{v} of state {name} in {repo}"));
        }
        Some(v) => vec![v],
    };
    let sets = sets_dir(state_dir);
    for number in doomed {
        let path = sets.join(format!("{}.set", run_id(repo, number, name)));
        if let Err(e) = std::fs::remove_file(&path)
            && e.kind() != std::io::ErrorKind::NotFound
        {
            return Err(format!("{}: {e}", path.display()));
        }
    }
    Ok(())
}

/// Answers `/state` downloads until the replier goes away, which for this
/// process means never; each exchange has a task of its own.
pub async fn serve(replier: Replier, state_dir: PathBuf, config: Arc<Shared>) {
    let blobs = blobs_dir(&state_dir);
    let store = match Store::open(&blobs, true) {
        Ok(store) => Arc::new(store),
        Err(e) => {
            eprintln!(
                "[ci] no state service: cannot open {}: {e}",
                blobs.display()
            );
            return;
        }
    };
    let service = Arc::new(StateService {
        state_dir,
        config,
        store,
    });
    loop {
        match replier.accept().await {
            Ok(request) => {
                let service = Arc::clone(&service);
                tokio::spawn(async move { service.exchange(request).await });
            }
            Err(e) => {
                eprintln!("[ci] stopped accepting state downloads: {e}");
                return;
            }
        }
    }
}

struct StateService {
    state_dir: PathBuf,
    config: Arc<Shared>,
    store: Arc<Store>,
}

impl StateService {
    async fn exchange(&self, mut request: IncomingRequest) {
        if let Err(why) = editor_principal(&self.config.get(), request.meta()) {
            eprintln!("[ci] refused a state download from a peer that {why}");
            request.refuse(weida::ErrorCode::Rejected).await;
            return;
        }
        let mut body = request.take_body();
        let asked = match read_frame::<_, StateDownload>(&mut body).await {
            Ok(Some(asked)) => asked,
            Ok(None) => return,
            Err(e) => {
                eprintln!("[ci] unreadable state download: {e}");
                request.refuse(weida::ErrorCode::Rejected).await;
                return;
            }
        };
        let mut reply = match request.reply(TransferMeta::default()).await {
            Ok(reply) => reply,
            Err(e) => {
                eprintln!("[ci] cannot open the reply half of a state download: {e}");
                return;
            }
        };
        let sent = match self.version(&asked) {
            Ok(manifest) => self.send(&manifest, &mut reply).await,
            Err(message) => write_frame(&mut reply, &ArtifactReply::Error { message }).await,
        };
        if let Err(e) = sent {
            eprintln!(
                "[ci] state {} #{} of {}: download failed: {e}",
                asked.name, asked.version, asked.repo
            );
        }
        let _ = reply.finish();
    }

    /// The manifest the download names, of a repository of `ci.toml`.
    fn version(&self, asked: &StateDownload) -> Result<SetManifest, String> {
        let StateDownload {
            repo,
            name,
            version,
        } = asked;
        if !self.config.get().repos.contains_key(repo) {
            return Err(format!("no repository {repo}"));
        }
        read(&self.state_dir, repo, name, *version)?
            .ok_or_else(|| format!("no version #{version} of state {name} in {repo}"))
    }

    /// `Sending`, the manifest as chunks and an end marker, then every
    /// distinct file's blob in manifest order.
    async fn send(
        &self,
        manifest: &SetManifest,
        reply: &mut weida::OutgoingTransfer,
    ) -> Result<(), String> {
        write_frame(reply, &ArtifactReply::Sending).await?;
        for chunk in manifest.encode().chunks(ARTIFACT_CHUNK) {
            write_chunk(reply, chunk).await?;
        }
        write_end(reply).await?;
        crate::artifacts::stream_blobs(&self.store, blob_heads(manifest), reply).await
    }
}

/// The blob of every distinct file of `manifest` that has bytes, in
/// manifest order: an empty file needs none.
pub fn blob_heads(manifest: &SetManifest) -> Vec<BlobHead> {
    let mut seen = HashSet::new();
    manifest
        .entries
        .iter()
        .filter_map(|entry| match entry {
            SetEntry::File { root, size, .. } if *size > 0 && seen.insert(*root) => {
                Some(BlobHead {
                    root: *root,
                    size: *size,
                })
            }
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    mod io {
        use super::super::*;

        fn scratch(name: &str) -> PathBuf {
            let dir = std::env::temp_dir().join(format!("zci-state-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).expect("create the scratch directory");
            dir
        }

        fn manifest(sizes: &[u64]) -> SetManifest {
            let mut entries = vec![SetEntry::Dir {
                path: "d".to_owned(),
            }];
            for (i, size) in sizes.iter().enumerate() {
                entries.push(SetEntry::File {
                    path: format!("d/f{i}"),
                    size: *size,
                    root: [i as u8 + 1; 32],
                    executable: false,
                });
            }
            SetManifest { entries }
        }

        #[test]
        fn the_three_newest_versions_stay_and_the_newest_pipeline_wins() {
            let dir = scratch("versions");
            for number in 1..=4 {
                record(&dir, "demo", "corpus", number, &manifest(&[number])).unwrap();
            }
            assert_eq!(versions(&dir, "demo", "corpus").unwrap(), [4, 3, 2]);
            assert_eq!(latest(&dir, "demo", "corpus").unwrap(), Some(4));
            assert_eq!(
                read(&dir, "demo", "corpus", 4).unwrap(),
                Some(manifest(&[4]))
            );
            assert_eq!(read(&dir, "demo", "corpus", 1).unwrap(), None);

            record(&dir, "demo", "corpus", 1, &manifest(&[1])).unwrap();
            assert_eq!(latest(&dir, "demo", "corpus").unwrap(), Some(4));
            assert_eq!(latest(&dir, "demo", "other").unwrap(), None);
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn list_shows_one_repository_with_bytes_and_files() {
            let dir = scratch("list");
            record(&dir, "demo", "corpus", 1, &manifest(&[10])).unwrap();
            record(&dir, "demo", "corpus", 2, &manifest(&[10, 20])).unwrap();
            record(&dir, "demo", "notes", 5, &manifest(&[])).unwrap();
            record(&dir, "else", "corpus", 9, &manifest(&[1])).unwrap();

            let states = list(&dir, "demo").unwrap();
            let names: Vec<&str> = states.iter().map(|s| s.name.as_str()).collect();
            assert_eq!(names, ["corpus", "notes"]);
            let corpus: Vec<(u64, u64, u64)> = states[0]
                .versions
                .iter()
                .map(|v| (v.number, v.bytes, v.files))
                .collect();
            assert_eq!(corpus, [(2, 30, 2), (1, 10, 1)]);
            assert!(states[0].versions[0].saved > 0);
            assert_eq!(states[1].versions[0].files, 0);
            assert!(list(&dir, "nothing").unwrap().is_empty());
            let _ = std::fs::remove_dir_all(&dir);
        }

        #[test]
        fn delete_takes_one_version_or_the_store() {
            let dir = scratch("delete");
            for number in 1..=3 {
                record(&dir, "demo", "corpus", number, &manifest(&[number])).unwrap();
            }
            delete(&dir, "demo", "corpus", Some(3)).unwrap();
            assert_eq!(versions(&dir, "demo", "corpus").unwrap(), [2, 1]);
            assert_eq!(
                delete(&dir, "demo", "corpus", Some(3)),
                Err("no version #3 of state corpus in demo".to_owned())
            );
            delete(&dir, "demo", "corpus", None).unwrap();
            assert!(versions(&dir, "demo", "corpus").unwrap().is_empty());
            assert_eq!(
                delete(&dir, "demo", "corpus", None),
                Err("no state corpus in demo".to_owned())
            );
            let _ = std::fs::remove_dir_all(&dir);
        }
    }
}
