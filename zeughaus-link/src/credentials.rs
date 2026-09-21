//! The weida credentials both processes keep on disk: who the runner is, and
//! who is allowed to talk to it.
//!
//! The runner's identity lives in a file so its fingerprint survives a
//! restart: the announced URL pins that fingerprint, and a pin that changed on
//! every start would break weida's transparent redial. And because the same
//! listener hands out shells, access is authorized rather than merely
//! encrypted: the runner only accepts clients whose public keys it already
//! knows.
//!
//! Two files under one owner-only directory:
//!
//! * `runner.pem` -- the identity the runner binds with. Its fingerprint is
//!   what the announced `weida://sha256:<fp>@host:port/` URL names.
//! * `client.pem` -- the identity a native editor on this machine presents.
//!   The runner pins it, which is the whole of the local bootstrap: one user,
//!   one machine, one key pair each.
//!
//! A remote machine gets no bootstrap. Its public certificate is dropped into
//! `clients/<name>.pem` by whoever provisions it, and the runner pins that as
//! well -- there is no enrollment protocol here, and inventing one silently
//! would be the wrong kind of convenience for a shell.
//!
//! Everything is written atomically and owner-only, and a PEM that exists but
//! does not parse is an error: overwriting it would throw away the one secret
//! that cannot be re-derived, and would silently re-key a runner that peers
//! still pin.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use weida::{Fingerprint, Identity, Trust};

/// The runner's server identity: the fingerprint peers pin.
const RUNNER_PEM: &str = "runner.pem";

/// The local editor's client identity, bootstrapped next to the runner's.
const CLIENT_PEM: &str = "client.pem";

/// Certificates of clients provisioned by hand (remote machines).
const CLIENTS_DIR: &str = "clients";

/// Where the credentials live: `$ZEUGHAUS_STATE_DIR`, else
/// `$XDG_STATE_HOME/zeughaus`, else `~/.local/state/zeughaus`
/// (`%LOCALAPPDATA%\zeughaus\state` on Windows).
///
/// State, not config and not cache: a private key is neither something a user
/// edits nor something that may be thrown away between runs.
pub fn state_dir() -> PathBuf {
    // A named function rather than `std::env::var_os` itself: the generic
    // parameter binds one lifetime, and [`state_dir_from`] takes a lookup that
    // works for any.
    fn var(key: &str) -> Option<OsString> {
        std::env::var_os(key)
    }
    state_dir_from(var)
}

/// [`state_dir`] against an explicit environment.
///
/// Separate so the resolution order is testable: `std::env::set_var` is
/// `unsafe` and process-global, which makes an environment-mutating test a
/// race against every other thread in the binary.
pub fn state_dir_from(env: impl Fn(&str) -> Option<OsString>) -> PathBuf {
    if let Some(explicit) = non_empty(env("ZEUGHAUS_STATE_DIR")) {
        return PathBuf::from(explicit);
    }
    #[cfg(windows)]
    if let Some(local) = non_empty(env("LOCALAPPDATA")) {
        return Path::new(&local).join("zeughaus").join("state");
    }
    #[cfg(not(windows))]
    {
        if let Some(xdg) = non_empty(env("XDG_STATE_HOME")) {
            return Path::new(&xdg).join("zeughaus");
        }
        if let Some(home) = non_empty(env("HOME")) {
            return Path::new(&home)
                .join(".local")
                .join("state")
                .join("zeughaus");
        }
    }
    // No home at all -- a service started with an empty environment. A
    // relative directory keeps the keys next to whatever the process was
    // started in instead of writing into a shared world-readable place.
    PathBuf::from(".zeughaus-state")
}

/// The runner's server identity, created on first use.
///
/// Named for the hosts it will be announced under so a peer that trusts the
/// certificate as an anchor can verify the host it dialled; pinning by
/// fingerprint never consults a name, so both trust models cost one identity.
pub fn runner_identity(dir: &Path) -> Result<Identity, String> {
    load_or_create(&dir.join(RUNNER_PEM))
}

/// The local editor's client identity, created on first use.
///
/// The runner bootstraps this too, because the first editor to run must find a
/// key the runner already trusts -- a client that has to be enrolled before it
/// can connect, on the machine that owns the runner, would be ceremony with no
/// security in it.
pub fn client_identity(dir: &Path) -> Result<Identity, String> {
    load_or_create(&dir.join(CLIENT_PEM))
}

/// The local client identity if one was bootstrapped, without creating it.
///
/// The editor must not mint credentials: a key the runner has never pinned
/// authenticates nothing, and writing one would only hide the real cause
/// (no runner ever ran here) behind a refused handshake.
///
/// A file that does not parse reads as absent for the same reason the editor
/// does not create one: it cannot repair it, and the caller's answer is the
/// same either way.
pub fn load_client_identity(dir: &Path) -> Option<Identity> {
    let path = dir.join(CLIENT_PEM);
    let identity = Identity::from_pem_file(&path);
    identity.fingerprint().ok().map(|_| identity)
}

/// Whom the runner lets in: the bootstrapped local client plus every
/// certificate provisioned under `clients/`.
///
/// Empty means nobody was ever provisioned, which is the caller's signal to
/// fail closed on a non-loopback bind rather than to serve anonymously.
pub fn client_trust(dir: &Path) -> Result<Trust, String> {
    let mut trust = Trust::by_address();

    let local = dir.join(CLIENT_PEM);
    if local.exists() {
        trust = trust.and_pin(fingerprint_of(&local)?);
    }

    // Sorted, because a `Trust` is part of weida's connection-pool key and a
    // directory listing has no defined order: the same set of files must build
    // the same value on every start.
    let mut provisioned = Vec::new();
    match fs::read_dir(dir.join(CLIENTS_DIR)) {
        Ok(entries) => {
            for entry in entries {
                let entry = entry
                    .map_err(|e| format!("reading {}: {e}", dir.join(CLIENTS_DIR).display()))?;
                let path = entry.path();
                if path.extension().is_some_and(|ext| ext == "pem") {
                    provisioned.push(path);
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
        Err(e) => return Err(format!("reading {}: {e}", dir.join(CLIENTS_DIR).display())),
    }
    provisioned.sort();
    for path in provisioned {
        trust = trust.and_pin(fingerprint_of(&path)?);
    }

    Ok(trust)
}

/// The fingerprint a PEM file names, or why it could not be read.
///
/// A provisioned file holds a certificate and no key, the bootstrapped ones
/// hold both; the fingerprint is taken off the certificate either way.
fn fingerprint_of(path: &Path) -> Result<Fingerprint, String> {
    Identity::from_pem_file(path)
        .fingerprint()
        .map_err(|e| format!("{} is not a usable PEM certificate: {e}", path.display()))
}

/// Loads `path`, or creates a fresh identity there.
///
/// Reloads what was just written instead of returning the in-memory value, so
/// the returned identity is the one on disk: two processes racing on an empty
/// state directory then agree on the file rather than each serving its own
/// key.
fn load_or_create(path: &Path) -> Result<Identity, String> {
    let dir = path.parent().unwrap_or(Path::new("."));
    ensure_owner_only_dir(dir)?;

    if !path.exists() {
        let fresh = Identity::generate_for(local_names())
            .map_err(|e| format!("cannot generate an identity for {}: {e}", path.display()))?;
        let pem = fresh
            .to_pem()
            .map_err(|e| format!("cannot serialize the identity for {}: {e}", path.display()))?;
        write_private(path, &pem)?;
    }

    let identity = Identity::from_pem_file(path);
    identity.fingerprint().map_err(|e| {
        format!(
            "{} exists but is not a usable PEM identity: {e} -- move it aside by hand; \
             a key file is never overwritten",
            path.display()
        )
    })?;
    Ok(identity)
}

/// Writes `contents` to `path` atomically, readable by the owner alone.
///
/// Through a temporary file and a rename, because a half-written key file that
/// a restart would then refuse to overwrite is unrecoverable by anything short
/// of a human. The temporary name carries the process id and is created
/// exclusively: a leftover from a killed process must not be written into, and
/// two processes bootstrapping at once must not share one.
fn write_private(path: &Path, contents: &str) -> Result<(), String> {
    let tmp = path.with_extension(format!("pem.{}.tmp", std::process::id()));

    let mut options = fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        // Owner-only from the first byte: a chmod after the write would leave
        // the key readable for the length of the write.
        options.mode(0o600);
    }
    let mut file = options
        .open(&tmp)
        .map_err(|e| format!("cannot create {}: {e}", tmp.display()))?;

    let written = file
        .write_all(contents.as_bytes())
        .and_then(|()| file.sync_all());
    drop(file);
    if let Err(e) = written {
        let _ = fs::remove_file(&tmp);
        return Err(format!("cannot write {}: {e}", tmp.display()));
    }

    fs::rename(&tmp, path).map_err(|e| {
        let _ = fs::remove_file(&tmp);
        format!("cannot move {} into place: {e}", tmp.display())
    })
}

/// Creates the state directory and keeps it owner-only.
///
/// Tightened on every start rather than only at creation: the directory holds
/// private keys, and a mode that was widened by hand (or by a permissive
/// umask on an older version) is a finding, not a preference to respect.
/// Windows has no mode bits here; the directory inherits its parent's ACL,
/// which for `%LOCALAPPDATA%` is the user.
fn ensure_owner_only_dir(dir: &Path) -> Result<(), String> {
    fs::create_dir_all(dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = fs::metadata(dir)
            .map_err(|e| format!("cannot stat {}: {e}", dir.display()))?
            .permissions();
        if perms.mode() & 0o077 != 0 {
            perms.set_mode(0o700);
            fs::set_permissions(dir, perms)
                .map_err(|e| format!("cannot restrict {}: {e}", dir.display()))?;
        }
    }
    Ok(())
}

/// The names a generated certificate carries.
///
/// Loopback always, plus this host's name when it has one: an identity is
/// pinned by fingerprint, but a deployment that verifies it through an
/// authority instead needs the dialled host in the certificate, and adding the
/// names now costs nothing while re-keying later costs every peer its pin.
fn local_names() -> Vec<String> {
    let mut names = vec!["localhost".to_owned(), "127.0.0.1".to_owned()];
    if let Some(host) = host_name()
        && !names.contains(&host)
    {
        names.push(host);
    }
    names
}

/// This machine's name, when it is one a certificate may carry.
///
/// Read rather than asked of libc: the value is a hint for anchor-based
/// verification, and pulling in a C call (or a crate for it) to sharpen a hint
/// is not worth it. Anything that is not a plain DNS label is dropped -- the
/// certificate generator rejects it, and a host with an odd name must not make
/// the runner unable to start.
fn host_name() -> Option<String> {
    #[cfg(not(windows))]
    let raw = fs::read_to_string("/proc/sys/kernel/hostname")
        .or_else(|_| fs::read_to_string("/etc/hostname"))
        .ok()
        .or_else(|| std::env::var("HOSTNAME").ok())?;
    #[cfg(windows)]
    let raw = std::env::var("COMPUTERNAME").ok()?;

    let name = raw.trim().to_owned();
    let usable = !name.is_empty()
        && name.len() <= 253
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.');
    usable.then_some(name)
}

/// An environment variable that is set and not the empty string.
///
/// An empty `XDG_STATE_HOME` is unset by the specification, and treating it as
/// a path would put the keys in the filesystem root.
fn non_empty(value: Option<OsString>) -> Option<OsString> {
    value.filter(|v| !v.is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// A directory that removes itself. No `tempfile` dependency for four
    /// tests, and the name carries the process id so a parallel test binary
    /// cannot collide.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "zeughaus-credentials-{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = fs::remove_dir_all(&path);
            TempDir(path)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The point of the file: the fingerprint an editor pinned must still be
    /// the runner's after a restart.
    #[test]
    fn the_runner_identity_is_created_once_and_then_reused() {
        let dir = TempDir::new("runner");
        let first = runner_identity(dir.path()).expect("create");
        let second = runner_identity(dir.path()).expect("reuse");

        assert!(dir.path().join(RUNNER_PEM).is_file());
        assert_eq!(
            first.fingerprint().expect("fingerprint"),
            second.fingerprint().expect("fingerprint")
        );
    }

    /// A private key readable by the rest of the machine is the same as no
    /// authentication at all.
    #[cfg(unix)]
    #[test]
    fn the_key_file_and_its_directory_are_owner_only() {
        use std::os::unix::fs::PermissionsExt;

        let dir = TempDir::new("modes");
        runner_identity(dir.path()).expect("create");

        let dir_mode = fs::metadata(dir.path())
            .expect("stat dir")
            .permissions()
            .mode();
        let file_mode = fs::metadata(dir.path().join(RUNNER_PEM))
            .expect("stat file")
            .permissions()
            .mode();
        assert_eq!(dir_mode & 0o777, 0o700, "state dir mode {dir_mode:o}");
        assert_eq!(file_mode & 0o777, 0o600, "key file mode {file_mode:o}");
    }

    /// Regenerating over an unreadable key would silently re-key a runner that
    /// peers still pin, and destroy the only copy of the old one.
    #[test]
    fn a_corrupt_identity_is_reported_and_left_alone() {
        let dir = TempDir::new("corrupt");
        fs::create_dir_all(dir.path()).expect("mkdir");
        let path = dir.path().join(CLIENT_PEM);
        fs::write(&path, b"not a pem at all\n").expect("write");

        let error = client_identity(dir.path()).expect_err("must not regenerate");
        assert!(error.contains("client.pem"), "{error}");
        assert_eq!(fs::read(&path).expect("read"), b"not a pem at all\n");

        // The runner's trust is built from the same file, so it must refuse
        // to start rather than come up trusting one client fewer.
        assert!(client_trust(dir.path()).is_err());
    }

    /// The runner pins the local bootstrap and every hand-provisioned remote
    /// certificate, and nothing else.
    #[test]
    fn trust_pins_the_local_client_and_provisioned_ones() {
        let dir = TempDir::new("trust");
        let remote = TempDir::new("remote");

        assert!(
            client_trust(dir.path()).expect("empty trust").is_empty(),
            "an unprovisioned directory must trust nobody"
        );

        let local = client_identity(dir.path()).expect("bootstrap");
        let provisioned = client_identity(remote.path()).expect("remote bootstrap");
        fs::create_dir_all(dir.path().join(CLIENTS_DIR)).expect("mkdir clients");
        fs::write(
            dir.path().join(CLIENTS_DIR).join("laptop.pem"),
            provisioned.certificate_pem().expect("certificate"),
        )
        .expect("provision");
        // Not a PEM file: must be ignored rather than fail the runner.
        fs::write(dir.path().join(CLIENTS_DIR).join("README.txt"), b"notes").expect("write");

        let trust = client_trust(dir.path()).expect("trust");
        assert_eq!(
            trust.pins,
            vec![
                local.fingerprint().expect("fingerprint"),
                provisioned.fingerprint().expect("fingerprint"),
            ]
        );
        assert!(trust.anchors.is_empty());
    }

    /// The knob a deployment sets, and the order it is resolved in.
    #[cfg(not(windows))]
    #[test]
    fn the_state_dir_follows_the_environment() {
        let env = |vars: Vec<(&'static str, &'static str)>| {
            move |key: &str| {
                vars.iter()
                    .find(|(k, _)| *k == key)
                    .map(|(_, v)| OsString::from(*v))
            }
        };

        assert_eq!(
            state_dir_from(env(vec![
                ("ZEUGHAUS_STATE_DIR", "/srv/zeughaus"),
                ("XDG_STATE_HOME", "/home/u/.local/state"),
                ("HOME", "/home/u"),
            ])),
            PathBuf::from("/srv/zeughaus")
        );
        // An empty value is unset, not a path.
        assert_eq!(
            state_dir_from(env(vec![("ZEUGHAUS_STATE_DIR", ""), ("HOME", "/home/u")])),
            PathBuf::from("/home/u/.local/state/zeughaus")
        );
        assert_eq!(
            state_dir_from(env(vec![
                ("XDG_STATE_HOME", "/home/u/state"),
                ("HOME", "/home/u"),
            ])),
            PathBuf::from("/home/u/state/zeughaus")
        );
    }
}
