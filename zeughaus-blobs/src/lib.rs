//! The files of a CI artifact set by content: a [`SetManifest`] names every
//! file by its BLAKE3 root, a [`Store`] holds each distinct file once (raw on a
//! runner, zstd-compressed on the CI), [`materialize`] lays a set out again
//! from a raw store, and [`sweep`] keeps a store within its retention.
//!
//! A store is one flat directory of blobs named by their root in hex
//! (`<hex>`, or `<hex>.zst` compressed) plus `tmp-*` files being received. The
//! sets that keep blobs alive are `<run>.set` files (encoded manifests) in a
//! directory of their own.

use std::collections::{BTreeMap, HashSet};
use std::fs::File;
use std::io::{BufWriter, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime};

use zeughaus_link::enlist::{SetEntry, SetManifest, parse_run_id};

/// A blob's id: the BLAKE3 hash of its bytes.
pub type Root = [u8; 32];

/// The deepest directory [`manifest_of`] descends into.
const MAX_DEPTH: usize = 64;

/// Names temp files uniquely within a process.
static COUNTER: AtomicU64 = AtomicU64::new(0);

/// The size and root of the file at `path`. The root equals `b3sum` of the
/// file, which is also weida's `BlobId`.
pub fn hash_file(path: &Path) -> std::io::Result<(u64, Root)> {
    let mut file = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buf = vec![0u8; 1 << 20];
    let mut size = 0u64;
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buf[..n]);
        size += n as u64;
    }
    Ok((size, *hasher.finalize().as_bytes()))
}

/// The manifest of the tree under `dir` and, for each distinct root, the
/// first file met that holds it. Symlinks are followed, as `tar` did.
pub fn manifest_of(dir: &Path) -> Result<(SetManifest, BTreeMap<Root, PathBuf>), String> {
    let mut entries = Vec::new();
    let mut sources = BTreeMap::new();
    walk(dir, "", 0, &mut entries, &mut sources)?;
    let manifest = SetManifest { entries };
    manifest.check()?;
    Ok((manifest, sources))
}

fn walk(
    dir: &Path,
    prefix: &str,
    depth: usize,
    entries: &mut Vec<SetEntry>,
    sources: &mut BTreeMap<Root, PathBuf>,
) -> Result<(), String> {
    let read = std::fs::read_dir(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    let mut children = Vec::new();
    for child in read {
        children.push(child.map_err(|e| format!("{}: {e}", dir.display()))?);
    }
    children.sort_by_key(|child| child.file_name());
    for child in children {
        let path = child.path();
        let Ok(name) = child.file_name().into_string() else {
            return Err(format!("{}: not a UTF-8 name", path.display()));
        };
        let rel = if prefix.is_empty() {
            name
        } else {
            format!("{prefix}/{name}")
        };
        let meta = std::fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        if meta.is_dir() {
            if depth + 1 > MAX_DEPTH {
                return Err(format!(
                    "{}: deeper than {MAX_DEPTH} directories (a symlink loop?)",
                    path.display()
                ));
            }
            entries.push(SetEntry::Dir { path: rel.clone() });
            walk(&path, &rel, depth + 1, entries, sources)?;
        } else if meta.is_file() {
            let (size, root) = hash_file(&path).map_err(|e| format!("{}: {e}", path.display()))?;
            sources.entry(root).or_insert(path);
            entries.push(SetEntry::File {
                path: rel,
                size,
                root,
                executable: executable(&meta),
            });
        } else {
            return Err(format!(
                "{}: neither a file nor a directory",
                path.display()
            ));
        }
    }
    Ok(())
}

#[cfg(unix)]
fn executable(meta: &std::fs::Metadata) -> bool {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o111 != 0
}

#[cfg(not(unix))]
fn executable(_meta: &std::fs::Metadata) -> bool {
    false
}

/// A directory of blobs named by their root: raw on a runner, where sets are
/// materialized from it, or zstd-compressed on the CI, which only relays.
#[derive(Debug)]
pub struct Store {
    dir: PathBuf,
    compressed: bool,
}

impl Store {
    pub fn open(dir: &Path, compressed: bool) -> std::io::Result<Store> {
        std::fs::create_dir_all(dir)?;
        Ok(Store {
            dir: dir.to_owned(),
            compressed,
        })
    }

    pub fn dir(&self) -> &Path {
        &self.dir
    }

    pub fn path(&self, root: &Root) -> PathBuf {
        let hex = hex::encode(root);
        if self.compressed {
            self.dir.join(format!("{hex}.zst"))
        } else {
            self.dir.join(hex)
        }
    }

    /// Whether the store holds the blob. A raw blob must also have its size;
    /// a compressed one is trusted once it was renamed into place.
    pub fn has(&self, root: &Root, size: u64) -> bool {
        match std::fs::metadata(self.path(root)) {
            Ok(meta) => self.compressed || meta.len() == size,
            Err(_) => false,
        }
    }

    /// Starts receiving the blob `root` of `size` bytes as zstd bytes.
    pub fn receive(&self, root: Root, size: u64) -> std::io::Result<Receiving> {
        let hex = hex::encode(root);
        let tmp = self.dir.join(format!(
            "tmp-{hex}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let file = BufWriter::new(File::create(&tmp)?);
        let (sink_file, kept) = if self.compressed {
            (None, Some(file))
        } else {
            (Some(file), None)
        };
        let sink = Sink {
            hasher: blake3::Hasher::new(),
            written: 0,
            limit: size,
            out: sink_file,
        };
        let decoder = match zstd::stream::write::Decoder::new(sink) {
            Ok(decoder) => decoder,
            Err(e) => {
                let _ = std::fs::remove_file(&tmp);
                return Err(e);
            }
        };
        Ok(Receiving {
            hex,
            root,
            size,
            tmp,
            target: self.path(&root),
            decoder: Some(decoder),
            compressed: kept,
        })
    }
}

/// Where decoded bytes go: the hash, the count and, in a raw store, the file.
struct Sink {
    hasher: blake3::Hasher,
    written: u64,
    limit: u64,
    out: Option<BufWriter<File>>,
}

impl Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.written + buf.len() as u64 > self.limit {
            return Err(std::io::Error::other(format!(
                "more than {} bytes",
                self.limit
            )));
        }
        self.hasher.update(buf);
        if let Some(out) = &mut self.out {
            out.write_all(buf)?;
        }
        self.written += buf.len() as u64;
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        match &mut self.out {
            Some(out) => out.flush(),
            None => Ok(()),
        }
    }
}

/// One blob arriving as zstd bytes. Dropped before [`Receiving::finish`], it
/// leaves nothing behind.
pub struct Receiving {
    hex: String,
    root: Root,
    size: u64,
    tmp: PathBuf,
    target: PathBuf,
    decoder: Option<zstd::stream::write::Decoder<'static, Sink>>,
    /// The temp file of a compressed store, which keeps the bytes as sent.
    compressed: Option<BufWriter<File>>,
}

impl Receiving {
    pub fn write(&mut self, compressed: &[u8]) -> Result<(), String> {
        let hex = &self.hex;
        if let Some(file) = &mut self.compressed {
            file.write_all(compressed)
                .map_err(|e| format!("blob {hex}: {e}"))?;
        }
        self.decoder
            .as_mut()
            .expect("a blob is written before it is finished")
            .write_all(compressed)
            .map_err(|e| format!("blob {hex}: {e}"))
    }

    /// Checks the blob against its size and root and puts it in place. A blob
    /// that is already there (another receiver was faster) is fine.
    pub fn finish(mut self) -> Result<(), String> {
        let checked = self.check();
        // Handles close before the temp file is renamed or removed: Windows
        // refuses both on an open file.
        self.decoder = None;
        self.compressed = None;
        if let Err(e) = checked {
            let _ = std::fs::remove_file(&self.tmp);
            return Err(e);
        }
        if self.target.exists() {
            let _ = std::fs::remove_file(&self.tmp);
            return Ok(());
        }
        match std::fs::rename(&self.tmp, &self.target) {
            Ok(()) => Ok(()),
            Err(_) if self.target.exists() => {
                let _ = std::fs::remove_file(&self.tmp);
                Ok(())
            }
            Err(e) => {
                let _ = std::fs::remove_file(&self.tmp);
                Err(format!("blob {}: {e}", self.hex))
            }
        }
    }

    fn check(&mut self) -> Result<(), String> {
        let hex = &self.hex;
        let mut decoder = self.decoder.take().expect("finished once");
        decoder.flush().map_err(|e| format!("blob {hex}: {e}"))?;
        let mut sink = decoder.into_inner();
        if let Some(out) = &mut sink.out {
            out.flush().map_err(|e| format!("blob {hex}: {e}"))?;
        }
        if let Some(file) = &mut self.compressed {
            file.flush().map_err(|e| format!("blob {hex}: {e}"))?;
        }
        if sink.written != self.size || *sink.hasher.finalize().as_bytes() != self.root {
            return Err(format!("blob {hex}: content does not match"));
        }
        Ok(())
    }
}

impl Drop for Receiving {
    fn drop(&mut self) {
        if self.decoder.is_some() {
            self.decoder = None;
            self.compressed = None;
            let _ = std::fs::remove_file(&self.tmp);
        }
    }
}

/// Compresses the file at `path` into `out` as one zstd frame.
pub fn compress_file(path: &Path, level: i32, out: &mut impl Write) -> std::io::Result<()> {
    let mut encoder = zstd::stream::write::Encoder::new(out, level)?;
    std::io::copy(&mut File::open(path)?, &mut encoder)?;
    encoder.finish()?;
    Ok(())
}

/// The distinct non-empty blobs of `manifest` that `store` lacks, in
/// manifest order. Empty files need no blob.
pub fn missing(manifest: &SetManifest, store: &Store) -> Vec<(Root, u64)> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for entry in &manifest.entries {
        if let SetEntry::File { size, root, .. } = entry
            && *size > 0
            && seen.insert(*root)
            && !store.has(root, *size)
        {
            out.push((*root, *size));
        }
    }
    out
}

/// Lays the set out under `dest` from a raw store, each file a copy of its
/// blob (a reflink or clone where the filesystem has them), so a job never
/// changes the store. Answers the sum of the file sizes.
pub fn materialize(manifest: &SetManifest, store: &Store, dest: &Path) -> Result<u64, String> {
    debug_assert!(!store.compressed, "materialize reads a raw store");
    std::fs::create_dir_all(dest).map_err(|e| format!("{}: {e}", dest.display()))?;
    let at = |path: &str| {
        path.split('/')
            .fold(dest.to_owned(), |p, part| p.join(part))
    };
    let mut total = 0;
    for entry in &manifest.entries {
        match entry {
            SetEntry::Dir { path } => {
                let target = at(path);
                std::fs::create_dir_all(&target)
                    .map_err(|e| format!("{}: {e}", target.display()))?;
            }
            SetEntry::File {
                path,
                size,
                root,
                executable,
            } => {
                let target = at(path);
                if let Some(parent) = target.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|e| format!("{}: {e}", parent.display()))?;
                }
                if *size == 0 {
                    File::create(&target).map_err(|e| format!("{}: {e}", target.display()))?;
                } else {
                    let source = store.path(root);
                    std::fs::copy(&source, &target).map_err(|e| {
                        format!("copy {} to {}: {e}", source.display(), target.display())
                    })?;
                }
                set_mode(&target, *executable)?;
                total += size;
            }
        }
    }
    Ok(total)
}

#[cfg(unix)]
fn set_mode(path: &Path, executable: bool) -> Result<(), String> {
    use std::os::unix::fs::PermissionsExt;
    let mode = if executable { 0o755 } else { 0o644 };
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
        .map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(not(unix))]
fn set_mode(_path: &Path, _executable: bool) -> Result<(), String> {
    Ok(())
}

fn set_path(sets: &Path, run: &str) -> Result<PathBuf, String> {
    if parse_run_id(run).is_none() {
        return Err(format!("{run:?} is no run id"));
    }
    Ok(sets.join(format!("{run}.set")))
}

/// Records that the set of `run` is `manifest`, which keeps its blobs alive
/// in the store this `sets` directory belongs to.
pub fn record_set(sets: &Path, run: &str, manifest: &SetManifest) -> Result<(), String> {
    let path = set_path(sets, run)?;
    std::fs::create_dir_all(sets).map_err(|e| format!("{}: {e}", sets.display()))?;
    let tmp = sets.join(format!(
        "tmp-{run}-{}-{}",
        std::process::id(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    ));
    let written = std::fs::write(&tmp, manifest.encode())
        .and_then(|()| std::fs::rename(&tmp, &path))
        .map_err(|e| format!("{}: {e}", path.display()));
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

/// The set recorded for `run`, if any.
pub fn read_set(sets: &Path, run: &str) -> Result<Option<SetManifest>, String> {
    let path = set_path(sets, run)?;
    match std::fs::read(&path) {
        Ok(bytes) => SetManifest::decode(&bytes)
            .map(Some)
            .map_err(|e| format!("{}: {e}", path.display())),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{}: {e}", path.display())),
    }
}

/// How long a store keeps sets and how large it may grow.
#[derive(Debug, Clone, Copy)]
pub struct Retention {
    /// A set older than this goes, whatever the budget.
    pub keep: Duration,
    /// Bytes of blobs the store may hold; above it the oldest sets go.
    pub budget: u64,
    /// Nothing younger than this goes: a set being recorded or a blob being
    /// received right now.
    pub grace: Duration,
}

/// What one [`sweep`] removed.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Swept {
    pub sets: usize,
    pub blobs: usize,
    /// Bytes of the removed blobs.
    pub bytes: u64,
}

/// Removes stale temp files, expired and unreadable sets, the blobs no set
/// refers to, and, while the blobs exceed the budget, the oldest sets. Errors
/// on single entries are logged and skipped.
pub fn sweep(store: &Store, sets: &Path, retention: &Retention) -> Swept {
    let now = SystemTime::now();
    let mut swept = Swept::default();
    for dir in [store.dir(), sets] {
        for (path, name, _, age) in listing(dir, now) {
            if name.starts_with("tmp-") && age >= retention.grace {
                remove(&path);
            }
        }
    }

    let mut kept: Vec<(PathBuf, Duration, HashSet<Root>)> = Vec::new();
    for (path, name, _, age) in listing(sets, now) {
        if !name.ends_with(".set") {
            continue;
        }
        if age > retention.keep {
            if remove(&path) {
                swept.sets += 1;
            }
            continue;
        }
        let decoded = std::fs::read(&path)
            .map_err(|e| e.to_string())
            .and_then(|bytes| SetManifest::decode(&bytes));
        match decoded {
            Ok(manifest) => kept.push((path, age, roots_of(&manifest))),
            Err(e) => {
                eprintln!("[blobs] {}: {e}; removed", path.display());
                if remove(&path) {
                    swept.sets += 1;
                }
            }
        }
    }

    let mut blobs: Vec<(PathBuf, Root, u64, Duration)> = listing(store.dir(), now)
        .into_iter()
        .filter_map(|(path, name, len, age)| {
            let hex = if store.compressed {
                name.strip_suffix(".zst")?
            } else {
                &name
            };
            let mut root = [0u8; 32];
            (hex.len() == 64 && hex::decode_to_slice(hex, &mut root).is_ok())
                .then_some((path, root, len, age))
        })
        .collect();
    loop {
        let referenced: HashSet<Root> = kept.iter().flat_map(|(_, _, r)| r).copied().collect();
        let mut total = 0u64;
        blobs.retain(|(path, root, len, age)| {
            if !referenced.contains(root) && *age >= retention.grace && remove(path) {
                swept.blobs += 1;
                swept.bytes += len;
                false
            } else {
                total += len;
                true
            }
        });
        if total <= retention.budget {
            break;
        }
        let oldest = kept
            .iter()
            .enumerate()
            .filter(|(_, (_, age, _))| *age >= retention.grace)
            .max_by_key(|(_, (_, age, _))| *age)
            .map(|(i, _)| i);
        let Some(oldest) = oldest else {
            eprintln!(
                "[blobs] {}: {} MB over budget, every set is younger than the grace",
                store.dir().display(),
                (total - retention.budget) >> 20
            );
            break;
        };
        let (path, _, _) = kept.swap_remove(oldest);
        if remove(&path) {
            swept.sets += 1;
        }
    }
    swept
}

fn roots_of(manifest: &SetManifest) -> HashSet<Root> {
    manifest
        .entries
        .iter()
        .filter_map(|entry| match entry {
            SetEntry::File { root, .. } => Some(*root),
            SetEntry::Dir { .. } => None,
        })
        .collect()
}

/// The regular files of `dir` with name, length and age.
fn listing(dir: &Path, now: SystemTime) -> Vec<(PathBuf, String, u64, Duration)> {
    let read = match std::fs::read_dir(dir) {
        Ok(read) => read,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Vec::new(),
        Err(e) => {
            eprintln!("[blobs] {}: {e}", dir.display());
            return Vec::new();
        }
    };
    let mut out = Vec::new();
    for entry in read {
        let entry = match entry {
            Ok(entry) => entry,
            Err(e) => {
                eprintln!("[blobs] {}: {e}", dir.display());
                continue;
            }
        };
        let path = entry.path();
        let meta = match entry.metadata() {
            Ok(meta) if meta.is_file() => meta,
            Ok(_) => continue,
            Err(e) => {
                eprintln!("[blobs] {}: {e}", path.display());
                continue;
            }
        };
        let Ok(name) = entry.file_name().into_string() else {
            continue;
        };
        let age = meta
            .modified()
            .ok()
            .and_then(|modified| now.duration_since(modified).ok())
            .unwrap_or(Duration::ZERO);
        out.push((path, name, meta.len(), age));
    }
    out
}

fn remove(path: &Path) -> bool {
    match std::fs::remove_file(path) {
        Ok(()) => true,
        Err(e) => {
            eprintln!("[blobs] {}: {e}", path.display());
            false
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    mod io {
        use super::*;

        fn scratch(name: &str) -> PathBuf {
            let dir =
                std::env::temp_dir().join(format!("zeughaus-blobs-{name}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            dir
        }

        fn root_of(bytes: &[u8]) -> Root {
            *blake3::hash(bytes).as_bytes()
        }

        fn compressed(bytes: &[u8]) -> Vec<u8> {
            zstd::encode_all(bytes, 3).unwrap()
        }

        fn store_blob(store: &Store, bytes: &[u8]) -> Root {
            let root = root_of(bytes);
            let mut receiving = store.receive(root, bytes.len() as u64).unwrap();
            receiving.write(&compressed(bytes)).unwrap();
            receiving.finish().unwrap();
            root
        }

        fn age(path: &Path, secs: u64) {
            let file = File::options().write(true).open(path).unwrap();
            file.set_modified(SystemTime::now() - Duration::from_secs(secs))
                .unwrap();
        }

        fn names(dir: &Path) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(dir)
                .unwrap()
                .map(|e| e.unwrap().file_name().into_string().unwrap())
                .collect();
            names.sort();
            names
        }

        fn file_entry(path: &str, bytes: &[u8], executable: bool) -> SetEntry {
            SetEntry::File {
                path: path.to_owned(),
                size: bytes.len() as u64,
                root: root_of(bytes),
                executable,
            }
        }

        #[test]
        fn a_tree_becomes_a_manifest_of_its_files_by_root() {
            let dir = scratch("manifest");
            std::fs::create_dir_all(dir.join("nested/inner")).unwrap();
            std::fs::create_dir(dir.join("empty_dir")).unwrap();
            std::fs::write(dir.join("nested/inner/file.txt"), b"hello").unwrap();
            std::fs::write(dir.join("empty"), b"").unwrap();
            std::fs::write(dir.join("script"), b"#!/bin/sh\n").unwrap();
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(
                    dir.join("script"),
                    std::fs::Permissions::from_mode(0o755),
                )
                .unwrap();
                std::os::unix::fs::symlink("nested/inner/file.txt", dir.join("link")).unwrap();
            }

            let (manifest, sources) = manifest_of(&dir).unwrap();

            let mut expected = vec![file_entry("empty", b"", false), dir_entry("empty_dir")];
            #[cfg(unix)]
            expected.push(file_entry("link", b"hello", false));
            expected.extend([
                dir_entry("nested"),
                dir_entry("nested/inner"),
                file_entry("nested/inner/file.txt", b"hello", false),
                file_entry("script", b"#!/bin/sh\n", cfg!(unix)),
            ]);
            assert_eq!(manifest.entries, expected);
            assert_eq!(sources.len(), 3, "one source per distinct root");
            let hello = &sources[&root_of(b"hello")];
            assert_eq!(std::fs::read(hello).unwrap(), b"hello");
            std::fs::remove_dir_all(&dir).unwrap();
        }

        fn dir_entry(path: &str) -> SetEntry {
            SetEntry::Dir {
                path: path.to_owned(),
            }
        }

        #[test]
        fn a_received_blob_rests_raw_or_compressed() {
            let dir = scratch("receive");
            let original = b"some bytes of a build, some bytes of a build".repeat(100);
            let source = dir.join("source");
            std::fs::write(&source, &original).unwrap();
            let mut zst = Vec::new();
            compress_file(&source, 9, &mut zst).unwrap();
            let root = root_of(&original);

            for (compressed, expected) in [(false, &original), (true, &zst)] {
                let store =
                    Store::open(&dir.join(format!("store-{compressed}")), compressed).unwrap();
                let mut receiving = store.receive(root, original.len() as u64).unwrap();
                for chunk in zst.chunks(7) {
                    receiving.write(chunk).unwrap();
                }
                receiving.finish().unwrap();
                assert_eq!(&std::fs::read(store.path(&root)).unwrap(), expected);
                assert!(store.has(&root, original.len() as u64));
                assert_eq!(names(store.dir()).len(), 1, "no temp file stays");
            }
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn a_blob_that_does_not_match_its_root_is_refused_without_a_trace() {
            let dir = scratch("mismatch");
            let original = b"the bytes the producer hashed".to_vec();
            let mut changed = original.clone();
            changed[0] ^= 1;
            for compressed_store in [false, true] {
                let store =
                    Store::open(&dir.join(format!("s-{compressed_store}")), compressed_store)
                        .unwrap();
                let root = root_of(&original);
                let mut receiving = store.receive(root, original.len() as u64).unwrap();
                receiving.write(&compressed(&changed)).unwrap();
                let err = receiving.finish().unwrap_err();
                assert!(err.contains("does not match"), "{err}");
                assert!(names(store.dir()).is_empty(), "{:?}", names(store.dir()));

                // The decoder hands its output on lazily, so the excess shows
                // at the next write or at the latest when the blob finishes.
                let mut longer = store.receive(root, original.len() as u64 - 1).unwrap();
                let err = match longer.write(&compressed(&original)) {
                    Ok(()) => longer.finish().unwrap_err(),
                    Err(e) => e,
                };
                assert!(
                    err.contains(&format!("blob {}", hex::encode(root))),
                    "{err}"
                );
                assert!(names(store.dir()).is_empty(), "nothing stays of it");
            }
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn missing_names_each_absent_blob_once_and_no_empty_file() {
            let dir = scratch("missing");
            let store = Store::open(&dir, false).unwrap();
            let present = store_blob(&store, b"present");
            let manifest = SetManifest {
                entries: vec![
                    file_entry("a", b"present", false),
                    file_entry("b", b"absent", false),
                    file_entry("c", b"absent", false),
                    file_entry("d", b"", false),
                ],
            };
            assert_eq!(missing(&manifest, &store), vec![(root_of(b"absent"), 6)]);
            assert!(store.has(&present, 7));
            assert!(!store.has(&present, 8), "a raw blob of another size");
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn a_materialized_set_is_the_tree_and_leaves_the_store_alone() {
            let dir = scratch("materialize");
            let store = Store::open(&dir.join("store"), false).unwrap();
            let manifest = SetManifest {
                entries: vec![
                    dir_entry("d"),
                    dir_entry("d/empty"),
                    file_entry("d/data", b"data", false),
                    file_entry("d/copy", b"data", false),
                    file_entry("zero", b"", false),
                    file_entry("tool", b"#!/bin/sh\n", true),
                ],
            };
            for bytes in [&b"data"[..], b"#!/bin/sh\n"] {
                store_blob(&store, bytes);
            }
            let dest = dir.join("out");
            assert_eq!(materialize(&manifest, &store, &dest).unwrap(), 18);
            assert!(dest.join("d/empty").is_dir());
            assert_eq!(std::fs::read(dest.join("d/data")).unwrap(), b"data");
            assert_eq!(std::fs::read(dest.join("d/copy")).unwrap(), b"data");
            assert_eq!(std::fs::read(dest.join("zero")).unwrap(), b"");
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = |p: &str| {
                    std::fs::metadata(dest.join(p))
                        .unwrap()
                        .permissions()
                        .mode()
                        & 0o777
                };
                assert_eq!(mode("tool"), 0o755);
                assert_eq!(mode("d/data"), 0o644);
            }

            std::fs::write(dest.join("d/data"), b"changed by a job").unwrap();
            assert_eq!(
                std::fs::read(store.path(&root_of(b"data"))).unwrap(),
                b"data"
            );
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn a_recorded_set_reads_back() {
            let dir = scratch("record");
            let manifest = SetManifest {
                entries: vec![file_entry("a", b"a", false)],
            };
            record_set(&dir, "r.1.j", &manifest).unwrap();
            assert_eq!(read_set(&dir, "r.1.j").unwrap(), Some(manifest.clone()));
            assert_eq!(read_set(&dir, "r.2.j").unwrap(), None);
            assert!(record_set(&dir, "../x", &manifest).is_err());
            assert_eq!(names(&dir), vec!["r.1.j.set"]);
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn a_sweep_drops_unreferenced_blobs_and_stale_temp_files() {
            let dir = scratch("sweep-unreferenced");
            let store = Store::open(&dir.join("store"), true).unwrap();
            let sets = dir.join("sets");
            let kept = store_blob(&store, b"kept");
            let dropped = store_blob(&store, b"dropped");
            let manifest = SetManifest {
                entries: vec![file_entry("k", b"kept", false)],
            };
            record_set(&sets, "r.1.j", &manifest).unwrap();
            std::fs::write(store.dir().join("tmp-stale"), b"x").unwrap();
            let retention = Retention {
                keep: Duration::from_secs(3600),
                budget: 1 << 30,
                grace: Duration::ZERO,
            };

            let swept = sweep(&store, &sets, &retention);

            assert_eq!((swept.sets, swept.blobs), (0, 1));
            assert!(store.has(&kept, 4));
            assert!(!store.has(&dropped, 7));
            assert_eq!(names(store.dir()).len(), 1, "{:?}", names(store.dir()));
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn a_sweep_over_budget_drops_the_oldest_set_and_its_blobs() {
            let dir = scratch("sweep-budget");
            let store = Store::open(&dir.join("store"), false).unwrap();
            let sets = dir.join("sets");
            let old = vec![b'o'; 1000];
            let new = vec![b'n'; 1000];
            store_blob(&store, &old);
            store_blob(&store, &new);
            for (run, bytes) in [("r.1.j", &old), ("r.2.j", &new)] {
                let manifest = SetManifest {
                    entries: vec![file_entry("f", bytes, false)],
                };
                record_set(&sets, run, &manifest).unwrap();
            }
            age(&sets.join("r.1.j.set"), 100);
            age(&sets.join("r.2.j.set"), 50);
            let retention = Retention {
                keep: Duration::from_secs(3600),
                budget: 1500,
                grace: Duration::ZERO,
            };

            let swept = sweep(&store, &sets, &retention);

            assert_eq!(
                swept,
                Swept {
                    sets: 1,
                    blobs: 1,
                    bytes: 1000
                }
            );
            assert_eq!(names(&sets), vec!["r.2.j.set"]);
            assert!(store.has(&root_of(&new), 1000));
            assert!(!store.has(&root_of(&old), 1000));
            std::fs::remove_dir_all(&dir).unwrap();
        }

        #[test]
        fn a_sweep_keeps_what_is_younger_than_the_grace() {
            let dir = scratch("sweep-grace");
            let store = Store::open(&dir.join("store"), false).unwrap();
            let sets = dir.join("sets");
            let blob = store_blob(&store, &[1u8; 100]);
            std::fs::write(store.dir().join("tmp-receiving"), b"x").unwrap();
            let retention = Retention {
                keep: Duration::from_secs(3600),
                budget: 0,
                grace: Duration::from_secs(3600),
            };

            assert_eq!(sweep(&store, &sets, &retention), Swept::default());
            assert!(store.has(&blob, 100));
            assert!(store.dir().join("tmp-receiving").exists());
            std::fs::remove_dir_all(&dir).unwrap();
        }
    }
}
