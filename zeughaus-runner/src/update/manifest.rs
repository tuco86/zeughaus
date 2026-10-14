//! The manifest of a signed release, and the key that vouches for it.
//!
//! A release is a directory: one archive per platform, `manifest.json`, which
//! lists every archive with its size and SHA-256, and `manifest.sig`, the raw
//! Ed25519 signature of the manifest's exact bytes. The CI signs it; the
//! public half of its key is compiled into every binary ([`TRUSTED_KEYS`]),
//! so what a runner installs is decided by the binary it already trusts and
//! not by the server that answered.
//!
//! Built on every platform: `release bundle` and `release verify` are
//! subcommands wherever this binary runs, only the updater is unix-only.

use std::collections::BTreeSet;
use std::io::Read;
use std::path::Path;

use ring::signature::{ED25519, Ed25519KeyPair, KeyPair, UnparsedPublicKey};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::names::valid_name;

/// The manifest layout this binary reads and writes. A field added or
/// changed is a new format: the parser refuses what it does not know.
pub const FORMAT: u32 = 1;

/// The platforms a release has an archive for, as `<os>-<arch>` with the
/// names the release uses (not those of `std::env::consts`).
pub const PLATFORMS: [&str; 5] = [
    "darwin-amd64",
    "darwin-arm64",
    "linux-amd64",
    "linux-arm64",
    "windows-amd64",
];

/// The manifest in a release's directory.
pub const MANIFEST_FILE: &str = "manifest.json";

/// Its signature next to it: the 64 raw bytes of the Ed25519 signature of
/// the manifest file as it is.
pub const SIGNATURE_FILE: &str = "manifest.sig";

/// The public Ed25519 keys whose signature an update accepts.
pub const TRUSTED_KEYS: &[[u8; 32]] = &[
    // ZEUGHAUS_UPDATE_KEY of the zeughaus CI, generated 2026-10-14
    [
        0x83, 0xdb, 0xed, 0x9e, 0xfb, 0x4c, 0x11, 0xd1, 0x6c, 0x1d, 0x77, 0x81, 0xf9, 0x45, 0x02,
        0x62, 0x75, 0x0a, 0x1a, 0xc7, 0x30, 0xe5, 0x50, 0xed, 0xe7, 0x20, 0x90, 0x81, 0x02, 0x8c,
        0x85, 0xf6,
    ],
];

/// What a file name and a version may be made of; they become part of a URL
/// and of a path.
const NAME_RULE: &str = "1 to 128 characters of [A-Za-z0-9._+-], and not . or ..";

/// What a release says about itself. The bytes that are signed and published
/// are exactly [`Manifest::to_json`]: a verifier checks the bytes it
/// downloaded and parses those, it never signs or compares a re-serialization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    pub format: u32,
    /// The channel it was published to: `dev`, `nightly`, `release`.
    pub channel: String,
    /// The CI pipeline number that built it, which is what "newer" means to
    /// an updater.
    pub build: u64,
    /// The workspace version the archives were built from.
    pub version: String,
    /// The commit.
    pub sha: String,
    /// One per platform, sorted by platform.
    pub artifacts: Vec<Artifact>,
}

/// One archive of a release.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub platform: String,
    /// The file name, next to the manifest.
    pub file: String,
    pub size: u64,
    /// Lowercase hex.
    pub sha256: String,
}

impl Manifest {
    /// The bytes that are signed and published: pretty JSON and a newline.
    pub fn to_json(&self) -> Vec<u8> {
        let mut json = serde_json::to_vec_pretty(self)
            .expect("a manifest is strings and integers, which always serialize");
        json.push(b'\n');
        json
    }
}

impl Artifact {
    /// The entry for the file at `path`, which is published under its own
    /// file name.
    pub fn of_file(platform: &str, path: &Path) -> Result<Artifact, String> {
        let file = path
            .file_name()
            .and_then(|name| name.to_str())
            .ok_or_else(|| format!("{}: not a file name", path.display()))?;
        let (size, sha256) = digest(path)?;
        Ok(Artifact {
            platform: platform.to_owned(),
            file: file.to_owned(),
            size,
            sha256,
        })
    }

    /// Whether the file at `path` is the one this entry describes.
    pub fn check(&self, path: &Path) -> Result<(), String> {
        let (size, sha256) = digest(path)?;
        if size != self.size {
            return Err(format!(
                "{}: {size} bytes, the manifest says {}",
                self.file, self.size
            ));
        }
        if sha256 != self.sha256 {
            return Err(format!(
                "{}: sha256 is {sha256}, the manifest says {}",
                self.file, self.sha256
            ));
        }
        Ok(())
    }
}

/// The size and the lowercase hex SHA-256 of a file, read in chunks: an
/// archive is the size of a whole build.
fn digest(path: &Path) -> Result<(u64, String), String> {
    let mut file =
        std::fs::File::open(path).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut chunk = vec![0u8; 1 << 16];
    let mut size = 0u64;
    loop {
        match file.read(&mut chunk) {
            Ok(0) => break,
            Ok(n) => {
                hasher.update(&chunk[..n]);
                size += n as u64;
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(format!("cannot read {}: {e}", path.display())),
        }
    }
    Ok((size, hex::encode(hasher.finalize())))
}

/// Reads a manifest and refuses every field a later step would put into a
/// path, a URL or a comparison without looking at it again.
pub fn parse(bytes: &[u8]) -> Result<Manifest, String> {
    let manifest: Manifest = serde_json::from_slice(bytes).map_err(|e| format!("manifest: {e}"))?;
    if manifest.format != FORMAT {
        return Err(format!(
            "manifest: format {} is not supported, this binary reads format {FORMAT}",
            manifest.format
        ));
    }
    if !valid_name(&manifest.channel) {
        return Err(format!(
            "manifest: channel `{}` must match [a-z0-9][a-z0-9-]*",
            manifest.channel
        ));
    }
    if !is_file_name(&manifest.version) {
        return Err(format!(
            "manifest: version `{}` must be {NAME_RULE}",
            manifest.version
        ));
    }
    if !is_lower_hex(&manifest.sha, &[40, 64]) {
        return Err(format!(
            "manifest: sha `{}` must be 40 or 64 lowercase hex characters",
            manifest.sha
        ));
    }
    let mut platforms = BTreeSet::new();
    let mut files = BTreeSet::new();
    for artifact in &manifest.artifacts {
        if !PLATFORMS.contains(&artifact.platform.as_str()) {
            return Err(format!(
                "manifest: platform `{}` is not one of {}",
                artifact.platform,
                PLATFORMS.join(", ")
            ));
        }
        if !platforms.insert(artifact.platform.as_str()) {
            return Err(format!(
                "manifest: platform `{}` has more than one artifact",
                artifact.platform
            ));
        }
        if !is_file_name(&artifact.file) {
            return Err(format!(
                "manifest: file `{}` of {} must be {NAME_RULE}",
                artifact.file, artifact.platform
            ));
        }
        if !files.insert(artifact.file.as_str()) {
            return Err(format!(
                "manifest: file `{}` is listed twice",
                artifact.file
            ));
        }
        if !is_lower_hex(&artifact.sha256, &[64]) {
            return Err(format!(
                "manifest: sha256 `{}` of {} must be 64 lowercase hex characters",
                artifact.sha256, artifact.platform
            ));
        }
    }
    Ok(manifest)
}

fn is_file_name(s: &str) -> bool {
    (1..=128).contains(&s.len())
        && s != "."
        && s != ".."
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'+' | b'-'))
}

fn is_lower_hex(s: &str, lengths: &[usize]) -> bool {
    lengths.contains(&s.len()) && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The directory an archive unpacks into: `zeughaus-<version>-<platform>`.
pub fn stem(version: &str, platform: &str) -> String {
    format!("zeughaus-{version}-{platform}")
}

/// The file name of `platform`'s archive: a zip on Windows, a gzipped tar
/// everywhere else.
pub fn archive_name(version: &str, platform: &str) -> String {
    let extension = if platform == "windows-amd64" {
        "zip"
    } else {
        "tar.gz"
    };
    format!("{}.{extension}", stem(version, platform))
}

/// The platform that a file named `file` is the archive of, or why it is
/// none.
pub fn platform_of_archive(version: &str, file: &str) -> Result<&'static str, String> {
    PLATFORMS
        .into_iter()
        .find(|platform| archive_name(version, platform) == file)
        .ok_or_else(|| {
            format!(
                "{file}: not an archive of version {version}; expected \
                 zeughaus-{version}-<platform>.tar.gz (.zip for windows-amd64) \
                 with <platform> one of {}",
                PLATFORMS.join(", ")
            )
        })
}

/// The platform this binary runs on, as the release names it; `None` for a
/// host no archive is built for.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn host_platform() -> Option<&'static str> {
    platform_of(std::env::consts::OS, std::env::consts::ARCH)
}

fn platform_of(os: &str, arch: &str) -> Option<&'static str> {
    Some(match (os, arch) {
        ("linux", "x86_64") => "linux-amd64",
        ("linux", "aarch64") => "linux-arm64",
        ("macos", "aarch64") => "darwin-arm64",
        ("macos", "x86_64") => "darwin-amd64",
        ("windows", "x86_64") => "windows-amd64",
        _ => return None,
    })
}

/// The CI pipeline number this binary was built by (`ZEUGHAUS_BUILD` at
/// compile time); 0 for a build made anywhere else, which is older than every
/// published one.
#[cfg_attr(not(unix), allow(dead_code))]
pub fn own_build() -> u64 {
    build_from(option_env!("ZEUGHAUS_BUILD"))
}

fn build_from(env: Option<&str>) -> u64 {
    env.and_then(|build| build.parse().ok()).unwrap_or(0)
}

fn key_pair(seed: &[u8; 32]) -> Ed25519KeyPair {
    Ed25519KeyPair::from_seed_unchecked(seed).expect("a 32-byte seed is an Ed25519 key")
}

/// The public key of `seed`: the entry to add to [`TRUSTED_KEYS`].
pub fn public_key(seed: &[u8; 32]) -> [u8; 32] {
    let mut key = [0u8; 32];
    key.copy_from_slice(key_pair(seed).public_key().as_ref());
    key
}

/// The signature of `bytes` by the key `seed` makes.
pub fn sign(seed: &[u8; 32], bytes: &[u8]) -> [u8; 64] {
    let mut signature = [0u8; 64];
    signature.copy_from_slice(key_pair(seed).sign(bytes).as_ref());
    signature
}

/// Whether `sig` signs `bytes` by one of `keys`. Everything that checks a
/// release passes [`TRUSTED_KEYS`]; a test passes a key of its own, because
/// the private half of the real one is the CI's.
pub fn verify_with(keys: &[[u8; 32]], bytes: &[u8], sig: &[u8]) -> Result<(), String> {
    if keys.iter().any(|key| {
        UnparsedPublicKey::new(&ED25519, key)
            .verify(bytes, sig)
            .is_ok()
    }) {
        Ok(())
    } else {
        Err("manifest signature matches no trusted key".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: [u8; 32] = [7; 32];
    const OTHER: [u8; 32] = [9; 32];

    fn artifact(platform: &str, file: &str) -> Artifact {
        Artifact {
            platform: platform.into(),
            file: file.into(),
            size: 3,
            sha256: "ab".repeat(32),
        }
    }

    fn sample() -> Manifest {
        Manifest {
            format: FORMAT,
            channel: "dev".into(),
            build: 42,
            version: "0.1.0-dev".into(),
            sha: "0123456789abcdef0123456789abcdef01234567".into(),
            artifacts: vec![
                artifact("linux-amd64", "zeughaus-0.1.0-dev-linux-amd64.tar.gz"),
                artifact("windows-amd64", "zeughaus-0.1.0-dev-windows-amd64.zip"),
            ],
        }
    }

    /// Why `parse` refuses the manifest.
    fn refused(manifest: &Manifest) -> String {
        parse(&manifest.to_json()).expect_err("the manifest must be refused")
    }

    /// A directory that removes itself.
    struct Scratch(std::path::PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let path = std::env::temp_dir()
                .join(format!("zeughaus-manifest-{tag}-{}", std::process::id()));
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

    #[test]
    fn a_signed_manifest_verifies_against_its_key() {
        let json = sample().to_json();
        let signature = sign(&SEED, &json);
        assert_eq!(verify_with(&[public_key(&SEED)], &json, &signature), Ok(()));
        assert!(json.ends_with(b"}\n"));
        assert_eq!(parse(&json), Ok(sample()));
    }

    #[test]
    fn a_flipped_manifest_byte_fails() {
        let mut json = sample().to_json();
        let signature = sign(&SEED, &json);
        json[20] ^= 1;
        assert!(verify_with(&[public_key(&SEED)], &json, &signature).is_err());
    }

    #[test]
    fn a_flipped_signature_byte_fails() {
        let json = sample().to_json();
        let mut signature = sign(&SEED, &json);
        signature[9] ^= 1;
        assert!(verify_with(&[public_key(&SEED)], &json, &signature).is_err());
    }

    #[test]
    fn a_foreign_key_fails() {
        let json = sample().to_json();
        let signature = sign(&SEED, &json);
        assert_eq!(
            verify_with(&[public_key(&OTHER)], &json, &signature),
            Err("manifest signature matches no trusted key".to_owned())
        );
        assert!(verify_with(&[], &json, &signature).is_err());
        // One entry that verifies is enough: a retired key may stay listed.
        assert!(verify_with(&[public_key(&OTHER), public_key(&SEED)], &json, &signature).is_ok());
    }

    #[test]
    fn a_truncated_signature_fails() {
        let json = sample().to_json();
        let signature = sign(&SEED, &json);
        let keys = [public_key(&SEED)];
        assert!(verify_with(&keys, &json, &signature[..63]).is_err());
        assert!(verify_with(&keys, &json, &[]).is_err());
    }

    #[test]
    fn the_trusted_list_holds_a_real_key() {
        assert!(!TRUSTED_KEYS.is_empty());
        assert!(
            TRUSTED_KEYS
                .iter()
                .all(|key| key.iter().any(|byte| *byte != 0))
        );
    }

    #[test]
    fn parse_refuses_a_platform_twice() {
        let mut manifest = sample();
        manifest.artifacts.push(artifact(
            "linux-amd64",
            "zeughaus-0.1.0-dev-linux-amd64-again.tar.gz",
        ));
        let err = refused(&manifest);
        assert!(
            err.contains("`linux-amd64`") && err.contains("more than one"),
            "{err}"
        );
    }

    #[test]
    fn parse_refuses_a_file_twice() {
        let mut manifest = sample();
        manifest.artifacts[1].file = manifest.artifacts[0].file.clone();
        let err = refused(&manifest);
        assert!(err.contains("listed twice"), "{err}");
    }

    #[test]
    fn parse_refuses_a_file_name_that_is_not_one() {
        let too_long = "x".repeat(129);
        for bad in [
            "",
            ".",
            "..",
            "a/b.tar.gz",
            "..\\x",
            "a b",
            "\u{e4}.tar.gz",
            too_long.as_str(),
        ] {
            let mut manifest = sample();
            manifest.artifacts[0].file = bad.to_owned();
            let err = refused(&manifest);
            assert!(err.contains("file `"), "{bad:?}: {err}");
        }
        let mut longest = sample();
        longest.artifacts[0].file = "x".repeat(128);
        assert!(parse(&longest.to_json()).is_ok());
    }

    #[test]
    fn parse_refuses_format_two() {
        let mut manifest = sample();
        manifest.format = 2;
        let err = refused(&manifest);
        assert!(err.contains("format 2"), "{err}");
    }

    #[test]
    fn parse_refuses_a_channel_that_is_not_a_name() {
        for bad in ["", "Dev", "-dev", "de v", "a/b"] {
            let mut manifest = sample();
            manifest.channel = bad.to_owned();
            let err = refused(&manifest);
            assert!(err.contains("channel"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn parse_refuses_a_version_that_cannot_be_a_path() {
        for bad in ["", "..", "1/2", "1 2"] {
            let mut manifest = sample();
            manifest.version = bad.to_owned();
            let err = refused(&manifest);
            assert!(err.contains("version"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn parse_refuses_a_commit_that_is_not_hex() {
        for bad in [
            "",
            "0123456789ABCDEF0123456789ABCDEF01234567",
            "0123456789abcdef0123456789abcdef0123456",
        ] {
            let mut manifest = sample();
            manifest.sha = bad.to_owned();
            let err = refused(&manifest);
            assert!(err.contains("sha `"), "{bad:?}: {err}");
        }
        let mut long = sample();
        long.sha = "a".repeat(64);
        assert!(parse(&long.to_json()).is_ok());
    }

    #[test]
    fn parse_refuses_a_platform_that_is_not_built() {
        let mut manifest = sample();
        manifest.artifacts[0].platform = "plan9-amd64".to_owned();
        let err = refused(&manifest);
        assert!(err.contains("plan9-amd64"), "{err}");
    }

    #[test]
    fn parse_refuses_a_digest_that_is_not_sha256_hex() {
        for bad in ["", &"AB".repeat(32), &"ab".repeat(31), &"zz".repeat(32)] {
            let mut manifest = sample();
            manifest.artifacts[0].sha256 = bad.to_owned();
            let err = refused(&manifest);
            assert!(err.contains("sha256"), "{bad:?}: {err}");
        }
    }

    #[test]
    fn parse_refuses_what_it_does_not_know() {
        let mut value: serde_json::Value = serde_json::from_slice(&sample().to_json()).unwrap();
        value["channel_note"] = "extra".into();
        let err = parse(&serde_json::to_vec(&value).unwrap()).unwrap_err();
        assert!(err.contains("channel_note"), "{err}");

        let mut value: serde_json::Value = serde_json::from_slice(&sample().to_json()).unwrap();
        value["artifacts"][0]["mirror"] = "elsewhere".into();
        let err = parse(&serde_json::to_vec(&value).unwrap()).unwrap_err();
        assert!(err.contains("mirror"), "{err}");

        assert!(parse(b"not json").is_err());
        assert!(parse(b"{}").is_err());
    }

    #[test]
    fn an_archive_name_maps_to_its_platform() {
        for platform in PLATFORMS {
            assert_eq!(
                platform_of_archive("0.1.0", &archive_name("0.1.0", platform)),
                Ok(platform)
            );
        }
        assert_eq!(
            archive_name("0.1.0", "windows-amd64"),
            "zeughaus-0.1.0-windows-amd64.zip"
        );
        assert_eq!(
            archive_name("0.1.0", "linux-arm64"),
            "zeughaus-0.1.0-linux-arm64.tar.gz"
        );
        assert_eq!(stem("0.1.0", "linux-arm64"), "zeughaus-0.1.0-linux-arm64");
    }

    #[test]
    fn a_file_that_is_no_archive_of_a_platform_is_refused() {
        for bad in [
            "zeughaus-0.1.0-plan9-amd64.tar.gz",
            "zeughaus-0.1.0-windows-amd64.tar.gz",
            "zeughaus-0.1.0-linux-amd64.zip",
            "zeughaus-0.2.0-linux-amd64.tar.gz",
            "zeughaus-0.1.0-linux-amd64.tar.gz.sig",
            "linux-amd64.tar.gz",
            "dir/zeughaus-0.1.0-linux-amd64.tar.gz",
            "",
        ] {
            assert!(platform_of_archive("0.1.0", bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn the_host_maps_to_a_release_platform() {
        for (os, arch, platform) in [
            ("linux", "x86_64", "linux-amd64"),
            ("linux", "aarch64", "linux-arm64"),
            ("macos", "aarch64", "darwin-arm64"),
            ("macos", "x86_64", "darwin-amd64"),
            ("windows", "x86_64", "windows-amd64"),
        ] {
            assert_eq!(platform_of(os, arch), Some(platform));
            assert!(PLATFORMS.contains(&platform));
        }
        assert_eq!(platform_of("linux", "riscv64"), None);
        assert_eq!(platform_of("freebsd", "x86_64"), None);
    }

    #[test]
    fn the_own_build_is_zero_unless_the_ci_set_it() {
        assert_eq!(build_from(Some("1234")), 1234);
        for bad in [None, Some(""), Some("dev"), Some("-1"), Some("12 ")] {
            assert_eq!(build_from(bad), 0, "{bad:?}");
        }
    }

    #[test]
    fn an_artifact_checks_the_file_it_describes() {
        let scratch = Scratch::new("artifact");
        let path = scratch.0.join("zeughaus-0.1.0-linux-amd64.tar.gz");
        std::fs::write(&path, b"abc").unwrap();

        let artifact = Artifact::of_file("linux-amd64", &path).unwrap();
        assert_eq!(artifact.platform, "linux-amd64");
        assert_eq!(artifact.file, "zeughaus-0.1.0-linux-amd64.tar.gz");
        assert_eq!(artifact.size, 3);
        // FIPS 180-2's SHA-256 of "abc".
        assert_eq!(
            artifact.sha256,
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        assert_eq!(artifact.check(&path), Ok(()));

        std::fs::write(&path, b"abd").unwrap();
        let err = artifact.check(&path).unwrap_err();
        assert!(err.contains("sha256"), "{err}");
        std::fs::write(&path, b"ab").unwrap();
        let err = artifact.check(&path).unwrap_err();
        assert!(err.contains("2 bytes"), "{err}");
        assert!(artifact.check(&scratch.0.join("missing")).is_err());
    }
}
