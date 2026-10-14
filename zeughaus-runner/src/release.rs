//! `zeughaus-runner release`: making and checking signed release bundles.
//!
//! ```text
//! zeughaus-runner release keygen <file>
//! zeughaus-runner release bundle --channel <c> --build <n> --sha <sha> --version <v> --out <dir> <file>...
//! zeughaus-runner release verify <dir>
//! ```
//!
//! The CI's publish job runs `bundle` over the archives the platform jobs
//! built and publishes the directory it makes; `verify` is what anyone who
//! downloaded a copy runs before trusting it. Both end in
//! [`crate::update::manifest`], so what `bundle` signs is what an updater
//! reads. The signing key's seed is never an argument: it comes from the
//! environment, where a process listing does not show it.

use std::io::Write;
use std::path::{Path, PathBuf};

use ring::rand::{SecureRandom, SystemRandom};

use crate::files::write_atomic;
use crate::update::manifest::{self, Artifact, FORMAT, Manifest};

/// The environment variable that holds the signing key's seed: 64 hex
/// characters, as `keygen` writes them.
pub const SEED_ENV: &str = "ZEUGHAUS_UPDATE_KEY";

/// What the command line asks for.
#[derive(Debug, PartialEq, Eq)]
pub enum Command {
    Keygen { file: PathBuf },
    Bundle(Bundle),
    Verify { dir: PathBuf },
}

/// The arguments of `bundle`.
#[derive(Debug, PartialEq, Eq)]
pub struct Bundle {
    channel: String,
    build: u64,
    sha: String,
    version: String,
    out: PathBuf,
    files: Vec<PathBuf>,
}

/// Parses what follows `release`.
pub fn parse(args: &[String]) -> Result<Command, String> {
    let Some((subcommand, rest)) = args.split_first() else {
        return Err("release needs keygen, bundle or verify".to_owned());
    };
    match subcommand.as_str() {
        "keygen" => match rest {
            [file] => Ok(Command::Keygen { file: file.into() }),
            _ => Err("release keygen takes one file".to_owned()),
        },
        "bundle" => parse_bundle(rest).map(Command::Bundle),
        "verify" => match rest {
            [dir] => Ok(Command::Verify { dir: dir.into() }),
            _ => Err("release verify takes one directory".to_owned()),
        },
        other => Err(format!("unknown release subcommand {other:?}")),
    }
}

fn parse_bundle(args: &[String]) -> Result<Bundle, String> {
    let mut channel = None;
    let mut build = None;
    let mut sha = None;
    let mut version = None;
    let mut out = None;
    let mut files = Vec::new();
    let mut rest = args.iter();
    while let Some(arg) = rest.next() {
        match arg.as_str() {
            "--channel" => take(&mut channel, arg, &mut rest)?,
            "--build" => take(&mut build, arg, &mut rest)?,
            "--sha" => take(&mut sha, arg, &mut rest)?,
            "--version" => take(&mut version, arg, &mut rest)?,
            "--out" => take(&mut out, arg, &mut rest)?,
            flag if flag.starts_with("--") => {
                return Err(format!("unknown option {flag:?}"));
            }
            file => files.push(PathBuf::from(file)),
        }
    }
    let build = required(build, "--build")?;
    let build = build
        .parse()
        .map_err(|e| format!("--build {build:?} is not a number: {e}"))?;
    if files.is_empty() {
        return Err("release bundle needs at least one archive".to_owned());
    }
    Ok(Bundle {
        channel: required(channel, "--channel")?,
        build,
        sha: required(sha, "--sha")?,
        version: required(version, "--version")?,
        out: required(out, "--out")?.into(),
        files,
    })
}

/// Reads the value of `flag` into `slot`, once.
fn take<'a>(
    slot: &mut Option<String>,
    flag: &str,
    rest: &mut impl Iterator<Item = &'a String>,
) -> Result<(), String> {
    let value = rest.next().ok_or_else(|| format!("{flag} needs a value"))?;
    if slot.replace(value.clone()).is_some() {
        return Err(format!("{flag} given twice"));
    }
    Ok(())
}

fn required(value: Option<String>, flag: &str) -> Result<String, String> {
    value.ok_or_else(|| format!("release bundle needs {flag}"))
}

impl Command {
    /// Runs it; the line to print when it worked.
    pub fn run(&self) -> Result<String, String> {
        match self {
            Command::Keygen { file } => keygen(file),
            Command::Bundle(bundle) => bundle.run(&seed_from_env()?, manifest::TRUSTED_KEYS),
            Command::Verify { dir } => verify(manifest::TRUSTED_KEYS, dir),
        }
    }
}

/// The seed in [`SEED_ENV`]. An error never quotes the value.
fn seed_from_env() -> Result<[u8; 32], String> {
    let text = std::env::var(SEED_ENV).map_err(|_| format!("{SEED_ENV} is not set"))?;
    parse_seed(&text)
}

/// 64 hex characters, with the newline `keygen` ends its file with allowed.
fn parse_seed(text: &str) -> Result<[u8; 32], String> {
    let mut seed = [0u8; 32];
    hex::decode_to_slice(text.trim(), &mut seed)
        .map_err(|_| format!("{SEED_ENV} must be 64 hex characters"))?;
    Ok(seed)
}

/// Makes a signing key: its seed into `file`, readable by its owner only and
/// never over an existing file (that key is the one the installed binaries
/// trust, and a seed that is lost cannot be made again), and its public half
/// to print for [`manifest::TRUSTED_KEYS`].
fn keygen(file: &Path) -> Result<String, String> {
    let mut seed = [0u8; 32];
    SystemRandom::new()
        .fill(&mut seed)
        .map_err(|_| "the system random source failed".to_owned())?;
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut out = options.open(file).map_err(|e| {
        if e.kind() == std::io::ErrorKind::AlreadyExists {
            format!("{} exists; refusing to overwrite a key", file.display())
        } else {
            format!("cannot create {}: {e}", file.display())
        }
    })?;
    if let Err(e) = writeln!(out, "{}", hex::encode(seed)) {
        // A key file cut short must not be taken for one.
        let _ = std::fs::remove_file(file);
        return Err(format!("cannot write {}: {e}", file.display()));
    }
    let bytes: Vec<String> = manifest::public_key(&seed)
        .iter()
        .map(|byte| format!("0x{byte:02x}"))
        .collect();
    Ok(format!(
        "public key for TRUSTED_KEYS: [{}],",
        bytes.join(", ")
    ))
}

impl Bundle {
    /// Makes the release directory `--out` from the archives, signed with
    /// `seed`, and checks it against `keys` before saying it worked.
    fn run(&self, seed: &[u8; 32], keys: &[[u8; 32]]) -> Result<String, String> {
        // Names first: nothing is created for a list that cannot be a release.
        let mut sources: Vec<(&'static str, &str, &Path)> = Vec::with_capacity(self.files.len());
        for path in &self.files {
            let name = path
                .file_name()
                .and_then(|name| name.to_str())
                .ok_or_else(|| format!("{}: not a file name", path.display()))?;
            let platform = manifest::platform_of_archive(&self.version, name)?;
            if sources.iter().any(|(taken, ..)| *taken == platform) {
                return Err(format!("more than one archive for {platform}"));
            }
            sources.push((platform, name, path));
        }
        match std::fs::create_dir(&self.out) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                return Err(format!("{} already exists", self.out.display()));
            }
            Err(e) => return Err(format!("cannot create {}: {e}", self.out.display())),
        }
        let made = self.fill(&sources, seed, keys);
        if made.is_err() {
            // The retry wants the directory not to exist.
            let _ = std::fs::remove_dir_all(&self.out);
        }
        made
    }

    fn fill(
        &self,
        sources: &[(&'static str, &str, &Path)],
        seed: &[u8; 32],
        keys: &[[u8; 32]],
    ) -> Result<String, String> {
        let mut artifacts = Vec::with_capacity(sources.len());
        for (platform, name, path) in sources {
            let copy = self.out.join(name);
            std::fs::copy(path, &copy).map_err(|e| {
                format!("cannot copy {} to {}: {e}", path.display(), copy.display())
            })?;
            artifacts.push(Artifact::of_file(platform, &copy)?);
        }
        artifacts.sort_by(|a, b| a.platform.cmp(&b.platform));
        let manifest = Manifest {
            format: FORMAT,
            channel: self.channel.clone(),
            build: self.build,
            version: self.version.clone(),
            sha: self.sha.clone(),
            artifacts,
        };
        let json = manifest.to_json();
        // The same refusals an updater makes, before anything is signed.
        manifest::parse(&json)?;
        let signature = manifest::sign(seed, &json);
        write_atomic(&self.out.join(manifest::MANIFEST_FILE), &json)?;
        write_atomic(&self.out.join(manifest::SIGNATURE_FILE), &signature)?;
        let sums: String = manifest
            .artifacts
            .iter()
            .map(|artifact| format!("{}  {}\n", artifact.sha256, artifact.file))
            .collect();
        write_atomic(&self.out.join("SHA256SUMS"), sums.as_bytes())?;
        manifest::verify_with(keys, &json, &signature)
            .map_err(|_| "the signing key is not in TRUSTED_KEYS".to_owned())?;
        Ok(format!(
            "bundled {} build {}, {} artifacts into {}",
            manifest.channel,
            manifest.build,
            manifest.artifacts.len(),
            self.out.display()
        ))
    }
}

/// Checks the release in `dir`: the signature against `keys`, the manifest,
/// and the size and SHA-256 of every archive it lists.
fn verify(keys: &[[u8; 32]], dir: &Path) -> Result<String, String> {
    let json = read(&dir.join(manifest::MANIFEST_FILE))?;
    let signature = read(&dir.join(manifest::SIGNATURE_FILE))?;
    manifest::verify_with(keys, &json, &signature)?;
    let manifest = manifest::parse(&json)?;
    for artifact in &manifest.artifacts {
        artifact.check(&dir.join(&artifact.file))?;
    }
    Ok(format!(
        "ok {} build {}, {} artifacts",
        manifest.channel,
        manifest.build,
        manifest.artifacts.len()
    ))
}

fn read(path: &Path) -> Result<Vec<u8>, String> {
    std::fs::read(path).map_err(|e| format!("cannot read {}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SEED: [u8; 32] = [7; 32];
    const OTHER: [u8; 32] = [9; 32];

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    /// A directory that removes itself.
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let path =
                std::env::temp_dir().join(format!("zeughaus-release-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("scratch directory");
            Scratch(path)
        }

        /// A file of `bytes` named `name` in the directory.
        fn file(&self, name: &str, bytes: &[u8]) -> PathBuf {
            let path = self.0.join(name);
            std::fs::write(&path, bytes).expect("write");
            path
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn bundle(scratch: &Scratch, files: Vec<PathBuf>) -> Bundle {
        Bundle {
            channel: "dev".into(),
            build: 7,
            sha: "0123456789abcdef0123456789abcdef01234567".into(),
            version: "0.0.0".into(),
            out: scratch.0.join("bundle"),
            files,
        }
    }

    fn two_archives(scratch: &Scratch) -> Vec<PathBuf> {
        vec![
            scratch.file("zeughaus-0.0.0-windows-amd64.zip", b"windows archive"),
            scratch.file("zeughaus-0.0.0-linux-amd64.tar.gz", b"linux archive"),
        ]
    }

    #[test]
    fn the_subcommands_are_parsed() {
        assert_eq!(
            parse(&args(&["keygen", "key.hex"])),
            Ok(Command::Keygen {
                file: "key.hex".into()
            })
        );
        assert_eq!(
            parse(&args(&["verify", "dir"])),
            Ok(Command::Verify { dir: "dir".into() })
        );
        assert_eq!(
            parse(&args(&[
                "bundle",
                "--channel",
                "dev",
                "--build",
                "7",
                "--sha",
                "abc",
                "--version",
                "0.0.0",
                "--out",
                "out",
                "a.tar.gz",
                "b.zip",
            ])),
            Ok(Command::Bundle(Bundle {
                channel: "dev".into(),
                build: 7,
                sha: "abc".into(),
                version: "0.0.0".into(),
                out: "out".into(),
                files: vec!["a.tar.gz".into(), "b.zip".into()],
            }))
        );
        for bad in [
            &[][..],
            &["keygen"][..],
            &["keygen", "a", "b"][..],
            &["verify"][..],
            &["sign", "x"][..],
            &["bundle"][..],
        ] {
            assert!(parse(&args(bad)).is_err(), "{bad:?}");
        }
    }

    /// Every flag of `bundle`, with a value.
    const FLAGS: [&str; 10] = [
        "--channel",
        "dev",
        "--build",
        "7",
        "--sha",
        "abc",
        "--version",
        "0.0.0",
        "--out",
        "out",
    ];

    /// `bundle` with every flag, then `extra`.
    fn bundle_args(extra: &[&str]) -> Vec<String> {
        let mut list = vec!["bundle"];
        list.extend(FLAGS);
        list.extend(extra);
        args(&list)
    }

    #[test]
    fn a_bundle_needs_every_flag_once_and_a_file() {
        assert!(parse(&bundle_args(&[])).is_err(), "no archive");
        assert!(parse(&bundle_args(&["a.zip"])).is_ok());
        assert!(parse(&bundle_args(&["a.zip", "--out", "again"])).is_err());
        assert!(parse(&bundle_args(&["a.zip", "--mirror", "x"])).is_err());
        assert!(parse(&bundle_args(&["a.zip", "--sha"])).is_err());
        // Every flag in turn missing.
        for skipped in 0..FLAGS.len() / 2 {
            let mut list = vec!["bundle"];
            for (index, pair) in FLAGS.chunks(2).enumerate() {
                if index != skipped {
                    list.extend(pair);
                }
            }
            list.push("a.zip");
            assert!(parse(&args(&list)).is_err(), "flag {skipped} missing");
        }
        let mut bad_build = FLAGS;
        bad_build[3] = "seven";
        let mut list = vec!["bundle"];
        list.extend(bad_build);
        list.push("a.zip");
        assert!(parse(&args(&list)).unwrap_err().contains("--build"));
    }

    #[test]
    fn a_seed_is_64_hex_characters() {
        let text = "00".repeat(31) + "ff";
        let mut expected = [0u8; 32];
        expected[31] = 0xff;
        assert_eq!(parse_seed(&text), Ok(expected));
        assert_eq!(parse_seed(&format!("{text}\n")), Ok(expected));
        for bad in [
            "",
            "ab",
            &"00".repeat(33),
            &"zz".repeat(32),
            &"0".repeat(63),
        ] {
            // The message never quotes what was given.
            assert_eq!(
                parse_seed(bad),
                Err(format!("{SEED_ENV} must be 64 hex characters")),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn a_bundle_verifies_and_a_tampered_one_does_not() {
        let scratch = Scratch::new("bundle");
        let bundle = bundle(&scratch, two_archives(&scratch));
        let keys = [manifest::public_key(&SEED)];
        let line = bundle.run(&SEED, &keys).unwrap();
        assert!(
            line.starts_with("bundled dev build 7, 2 artifacts into "),
            "{line}"
        );
        let out = &bundle.out;

        assert_eq!(
            verify(&keys, out),
            Ok("ok dev build 7, 2 artifacts".to_owned())
        );
        let mut names: Vec<_> = std::fs::read_dir(out)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "SHA256SUMS",
                "manifest.json",
                "manifest.sig",
                "zeughaus-0.0.0-linux-amd64.tar.gz",
                "zeughaus-0.0.0-windows-amd64.zip",
            ]
        );
        assert_eq!(std::fs::read(out.join("manifest.sig")).unwrap().len(), 64);
        // `sha256sum -c` format, in platform order.
        let sums = std::fs::read_to_string(out.join("SHA256SUMS")).unwrap();
        let lines: Vec<&str> = sums.lines().collect();
        assert_eq!(lines.len(), 2);
        assert!(lines[0].ends_with("  zeughaus-0.0.0-linux-amd64.tar.gz"));
        assert!(lines[1].ends_with("  zeughaus-0.0.0-windows-amd64.zip"));
        let manifest = manifest::parse(&std::fs::read(out.join("manifest.json")).unwrap()).unwrap();
        assert_eq!(manifest.artifacts[0].platform, "linux-amd64");
        assert_eq!(manifest.artifacts[1].platform, "windows-amd64");
        assert_eq!(
            lines[0],
            format!(
                "{}  {}",
                manifest.artifacts[0].sha256, manifest.artifacts[0].file
            )
        );

        // A foreign key is not trusted.
        let err = verify(&[manifest::public_key(&OTHER)], out).unwrap_err();
        assert!(err.contains("no trusted key"), "{err}");

        // One flipped byte of the manifest.
        let manifest_path = out.join("manifest.json");
        let original = std::fs::read(&manifest_path).unwrap();
        let mut flipped = original.clone();
        flipped[30] ^= 1;
        std::fs::write(&manifest_path, &flipped).unwrap();
        assert!(verify(&keys, out).is_err());
        std::fs::write(&manifest_path, &original).unwrap();
        assert!(verify(&keys, out).is_ok());

        // One flipped byte of an archive: same size, other digest.
        let archive = out.join("zeughaus-0.0.0-linux-amd64.tar.gz");
        let mut bytes = std::fs::read(&archive).unwrap();
        bytes[0] ^= 1;
        std::fs::write(&archive, &bytes).unwrap();
        let err = verify(&keys, out).unwrap_err();
        assert!(err.contains("sha256"), "{err}");
        // A truncated one.
        std::fs::write(&archive, b"short").unwrap();
        let err = verify(&keys, out).unwrap_err();
        assert!(err.contains("bytes"), "{err}");
        // A missing one.
        std::fs::remove_file(&archive).unwrap();
        assert!(verify(&keys, out).is_err());
    }

    #[test]
    fn a_bundle_refuses_what_cannot_be_a_release() {
        let scratch = Scratch::new("refuse");
        let keys = [manifest::public_key(&SEED)];

        // A file that is not an archive of a platform of this version.
        let stray = scratch.file("zeughaus-0.0.0-plan9-amd64.tar.gz", b"x");
        let err = bundle(&scratch, vec![stray]).run(&SEED, &keys).unwrap_err();
        assert!(err.contains("plan9"), "{err}");
        assert!(!scratch.0.join("bundle").exists(), "nothing is made");

        // Two archives of one platform.
        let first = scratch.file("zeughaus-0.0.0-linux-amd64.tar.gz", b"a");
        let err = bundle(&scratch, vec![first.clone(), first])
            .run(&SEED, &keys)
            .unwrap_err();
        assert!(err.contains("more than one"), "{err}");

        // A version, channel or commit the manifest would refuse.
        let archive = scratch.file("zeughaus-0.0.0-linux-amd64.tar.gz", b"a");
        let mut wrong = bundle(&scratch, vec![archive.clone()]);
        wrong.channel = "Dev".into();
        assert!(wrong.run(&SEED, &keys).unwrap_err().contains("channel"));
        let mut wrong = bundle(&scratch, vec![archive.clone()]);
        wrong.sha = "HEAD".into();
        assert!(wrong.run(&SEED, &keys).unwrap_err().contains("sha"));
        assert!(
            !scratch.0.join("bundle").exists(),
            "a failure leaves nothing"
        );

        // An existing directory is never reused.
        std::fs::create_dir(scratch.0.join("bundle")).unwrap();
        let err = bundle(&scratch, vec![archive.clone()])
            .run(&SEED, &keys)
            .unwrap_err();
        assert!(err.contains("already exists"), "{err}");
        assert!(scratch.0.join("bundle").exists(), "the existing one stays");
        std::fs::remove_dir(scratch.0.join("bundle")).unwrap();

        // A key that is not in TRUSTED_KEYS.
        let err = bundle(&scratch, vec![archive])
            .run(&SEED, &[manifest::public_key(&OTHER)])
            .unwrap_err();
        assert_eq!(err, "the signing key is not in TRUSTED_KEYS");
        assert!(
            !scratch.0.join("bundle").exists(),
            "nothing unverifiable stays"
        );
    }

    #[test]
    fn keygen_writes_a_seed_once() {
        let scratch = Scratch::new("keygen");
        let file = scratch.0.join("update-key.hex");
        let line = keygen(&file).unwrap();

        let text = std::fs::read_to_string(&file).unwrap();
        assert!(text.ends_with('\n'));
        assert_eq!(text.trim().len(), 64);
        assert!(
            text.trim()
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
        );
        let seed = parse_seed(&text).unwrap();
        let public: Vec<String> = manifest::public_key(&seed)
            .iter()
            .map(|byte| format!("0x{byte:02x}"))
            .collect();
        assert_eq!(
            line,
            format!("public key for TRUSTED_KEYS: [{}],", public.join(", "))
        );
        assert!(!line.contains(text.trim()), "the seed is never printed");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&file).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let err = keygen(&file).unwrap_err();
        assert!(err.contains("refusing"), "{err}");
        assert_eq!(
            std::fs::read_to_string(&file).unwrap(),
            text,
            "the key stays"
        );
    }
}
