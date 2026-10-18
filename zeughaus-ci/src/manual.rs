//! The `/manual` service: an agent uploads a snapshot of its working trees
//! (see [`zeughaus_link::manual`]) and the CI runs named jobs of the
//! repository's pipeline from it, as a pipeline of the channel a push to the
//! repository's default branch lands in.
//!
//! The upload lives under `ci/manual/<id>/`: `<i>.bundle` for every checkout
//! and, once they are all stored, `upload.json`. An event of kind `manual`
//! that names the id goes to the inbox; the scheduler makes the pipeline of it
//! (`Scheduler::set_up_manual`), says which number it got in
//! `ci/manual/<id>.number` (beside the directory, so the directory can go with
//! a pipeline that never started), and removes `ci/manual/<id>/` when the
//! pipeline ended. A runner fetches the bundles from the CI on `/artifacts`
//! (`ManualBundle`).
//!
//! The exchange ends when the pipeline has its number. An uploader whose
//! upload is refused before its bundles were read finds its writes closed:
//! the answer is still there to read.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use weida::{IncomingRequest, Replier, TransferMeta};
use zeughaus_link::enlist::{read_frame, write_frame};
use zeughaus_link::manual::{
    MAX_MANUAL_BUNDLE_BYTES, MAX_MANUAL_DIRS, MAX_MANUAL_JOBS, RunReply, RunUpload, valid_dir_name,
};

use crate::config::{CiConfig, RepoConfig, valid_name};
use crate::event::{self, CiEvent, EventKind};
use crate::files::{read_json, write_json};
use crate::scheduler::Pipeline;
use crate::transport::editor_principal;

/// Description of a complete upload; written after its bundles.
const UPLOAD_FILE: &str = "upload.json";

/// Bytes moved per read or write.
const CHUNK: usize = 256 << 10;

/// How long an upload waits for the scheduler to make its pipeline.
const NUMBER_WAIT: Duration = Duration::from_secs(30);

/// How often the scheduler's answer is looked for meanwhile.
const NUMBER_POLL: Duration = Duration::from_millis(250);

/// How old an upload directory or number file has to be for [`remove_stale`]
/// to take it: a pipeline cannot run longer than a day.
pub const STALE_AFTER: Duration = Duration::from_secs(7 * 24 * 3600);

/// Characters of a manual run's summary its pipeline shows as its subject.
const SUBJECT_CHARS: usize = 200;

/// `ci/manual/`: what the `/manual` service and the scheduler hand each other,
/// a directory per upload and a number file beside it.
fn root(state_dir: &Path) -> PathBuf {
    crate::ci_dir(state_dir).join("manual")
}

/// `ci/manual/<id>/`: the bundles and the description of one upload.
pub fn upload_dir(state_dir: &Path, id: &str) -> PathBuf {
    root(state_dir).join(id)
}

/// Bundle `index` of upload `id`.
pub fn bundle_path(state_dir: &Path, id: &str, index: u32) -> PathBuf {
    upload_dir(state_dir, id).join(format!("{index}.bundle"))
}

/// Where the scheduler says which pipeline upload `id` became; the service
/// reads and removes it.
pub fn number_path(state_dir: &Path, id: &str) -> PathBuf {
    root(state_dir).join(format!("{id}.number"))
}

/// The bare repository the `.zeughaus-ci/` of a manual run is read from: the
/// first bundle of a manual run of `repo`, which no other mirror may hold.
pub fn mirror_dir(state_dir: &Path, repo: &str) -> PathBuf {
    crate::ci_dir(state_dir)
        .join("mirrors")
        .join(format!("{repo}.manual.git"))
}

/// Whether `id` is the id [`new_upload_id`] makes. The id of an event is
/// joined into a path, so one that is not goes no further.
pub fn valid_upload_id(id: &str) -> bool {
    id.len() == 16 && id.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'))
}

/// The upload a manual run's pipeline carries; no other pipeline has one.
pub fn upload_of(pipeline: &Pipeline) -> Option<&str> {
    pipeline
        .event
        .upload
        .as_deref()
        .filter(|id| pipeline.event.kind == EventKind::Manual && valid_upload_id(id))
}

/// What the `/manual` service stored of upload `id`.
pub fn read_upload(state_dir: &Path, id: &str) -> Result<RunUpload, String> {
    if !valid_upload_id(id) {
        return Err(format!("{id:?} is not an upload"));
    }
    read_json(&upload_dir(state_dir, id).join(UPLOAD_FILE))
}

/// Describes upload `id` for the scheduler: written after its bundles, so a
/// directory with a description holds all of them.
pub fn write_upload(state_dir: &Path, id: &str, upload: &RunUpload) -> Result<(), String> {
    write_json(&upload_dir(state_dir, id).join(UPLOAD_FILE), upload)
}

/// Tells the service which pipeline upload `id` became.
pub fn write_number(state_dir: &Path, id: &str, number: u64) -> Result<(), String> {
    crate::files::write_atomic(&number_path(state_dir, id), number.to_string().as_bytes())
}

/// Removes the bundles and the description of upload `id`.
pub fn remove_upload(state_dir: &Path, id: &str) {
    if !valid_upload_id(id) {
        return;
    }
    let dir = upload_dir(state_dir, id);
    if let Err(e) = std::fs::remove_dir_all(&dir)
        && e.kind() != std::io::ErrorKind::NotFound
    {
        eprintln!("[ci] cannot remove {}: {e}", dir.display());
    }
}

/// Removes what no pipeline will read any more: upload directories and
/// number files untouched for `max_age`, left by a pipeline that was lost or
/// a service that gave up waiting.
pub fn remove_stale(state_dir: &Path, max_age: Duration) {
    let Ok(entries) = std::fs::read_dir(root(state_dir)) else {
        return;
    };
    let now = SystemTime::now();
    for entry in entries.flatten() {
        let age = entry
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|modified| now.duration_since(modified).ok());
        if !age.is_some_and(|age| age > max_age) {
            continue;
        }
        let path = entry.path();
        let removed = if path.is_dir() {
            std::fs::remove_dir_all(&path)
        } else {
            std::fs::remove_file(&path)
        };
        if let Err(e) = removed {
            eprintln!("[ci] cannot remove {}: {e}", path.display());
        }
    }
}

/// The first line of a manual run's summary, as its pipeline's subject.
pub fn subject(summary: &str) -> String {
    summary
        .lines()
        .next()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(SUBJECT_CHARS)
        .collect()
}

/// A repository URL reduced to what names it: lowercase, without scheme,
/// user, `.git` and trailing slashes, an scp-style `host:path` as
/// `host/path`. `git@github.com:Griasdi/Griasdi.git` and
/// `https://github.com/griasdi/griasdi` come out the same.
pub fn normalize_url(url: &str) -> String {
    let lower = url.trim().to_lowercase();
    let address = match lower.split_once("://") {
        Some((_scheme, rest)) => rest.to_owned(),
        // scp-style: `host:path`, where the host holds no `/` (a local path
        // that has a `:` further in is no host).
        None => match lower.split_once(':') {
            Some((host, path)) if !host.contains('/') => format!("{host}/{path}"),
            _ => lower,
        },
    };
    // The user (and password) belong to the authority, which ends at the
    // first `/`.
    let path_at = address.find('/').unwrap_or(address.len());
    let (authority, path) = address.split_at(path_at);
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let joined = format!("{host}{path}");
    let trimmed = joined.trim_end_matches('/');
    trimmed
        .strip_suffix(".git")
        .unwrap_or(trimmed)
        .trim_end_matches('/')
        .to_owned()
}

/// The repository of `ci.toml` whose `url` is the same repository as
/// `origin`, with its name.
fn repo_of<'a>(config: &'a CiConfig, origin: &str) -> Result<(&'a str, &'a RepoConfig), String> {
    let wanted = normalize_url(origin);
    config
        .repos
        .iter()
        .find(|(_, repo)| normalize_url(&repo.url) == wanted)
        .map(|(name, repo)| (name.as_str(), repo))
        .ok_or_else(|| format!("{origin} is no repository of ci.toml"))
}

/// What an upload must be before a byte of it is stored.
fn validate(upload: &RunUpload) -> Result<(), String> {
    if upload.jobs.is_empty() || upload.jobs.len() > MAX_MANUAL_JOBS {
        return Err(format!(
            "{} jobs are not in 1..={MAX_MANUAL_JOBS}",
            upload.jobs.len()
        ));
    }
    let mut named = HashSet::new();
    for job in &upload.jobs {
        if !valid_name(job) {
            return Err(format!("{job:?} is not a job name"));
        }
        if !named.insert(job.as_str()) {
            return Err(format!("job {job} is named twice"));
        }
    }
    if upload.dirs.is_empty() || upload.dirs.len() > MAX_MANUAL_DIRS {
        return Err(format!(
            "{} checkouts are not in 1..={MAX_MANUAL_DIRS}",
            upload.dirs.len()
        ));
    }
    if upload.sizes.len() != upload.dirs.len() {
        return Err(format!(
            "{} sizes for {} checkouts",
            upload.sizes.len(),
            upload.dirs.len()
        ));
    }
    let mut seen = HashSet::new();
    for (dir, &size) in upload.dirs.iter().zip(&upload.sizes) {
        if !valid_dir_name(&dir.dir) {
            return Err(format!("{:?} is not a checkout directory name", dir.dir));
        }
        if !seen.insert(dir.dir.as_str()) {
            return Err(format!("{:?} is listed twice", dir.dir));
        }
        if dir.sha.len() != 40 || !dir.sha.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(format!("{:?} is not a commit", dir.sha));
        }
        if size > MAX_MANUAL_BUNDLE_BYTES {
            return Err(format!(
                "the bundle of {} is {size} bytes; at most {MAX_MANUAL_BUNDLE_BYTES}",
                dir.dir
            ));
        }
    }
    Ok(())
}

/// The first 16 hex characters of a hash over the time, a counter and the
/// uploader: no two uploads share one.
fn new_upload_id(principal: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let mut hasher = Sha256::new();
    hasher.update(nanos.to_le_bytes());
    hasher.update(COUNTER.fetch_add(1, Ordering::Relaxed).to_le_bytes());
    hasher.update(principal.as_bytes());
    hex::encode(hasher.finalize())[..16].to_owned()
}

/// Copies exactly `size` bytes of `body` to `out`; a body that ends short is
/// an error.
async fn copy_exact(
    body: &mut (impl AsyncRead + Unpin),
    size: u64,
    out: &mut (impl AsyncWrite + Unpin),
) -> Result<(), String> {
    let mut buf = vec![0u8; CHUNK];
    let mut left = size;
    while left > 0 {
        let want = usize::try_from(left).map_or(CHUNK, |left| left.min(CHUNK));
        let n = body
            .read(&mut buf[..want])
            .await
            .map_err(|e| format!("the upload failed: {e}"))?;
        if n == 0 {
            return Err(format!("the upload ended {left} bytes short"));
        }
        out.write_all(&buf[..n])
            .await
            .map_err(|e| format!("cannot store it: {e}"))?;
        left -= n as u64;
    }
    out.flush()
        .await
        .map_err(|e| format!("cannot store it: {e}"))
}

/// Answers `/manual` exchanges until the replier goes away, which for this
/// process means never. Each exchange has a task of its own: an upload takes
/// as long as its bundles do.
pub async fn serve(replier: Replier, state_dir: PathBuf, config: Arc<CiConfig>) {
    let service = Arc::new(ManualService {
        state_dir,
        config,
        wait: NUMBER_WAIT,
    });
    loop {
        match replier.accept().await {
            Ok(request) => {
                let service = Arc::clone(&service);
                tokio::spawn(async move { service.exchange(request).await });
            }
            Err(e) => {
                eprintln!("[ci] stopped accepting manual uploads: {e}");
                return;
            }
        }
    }
}

struct ManualService {
    state_dir: PathBuf,
    config: Arc<CiConfig>,
    /// How long an accepted upload waits for its pipeline's number.
    wait: Duration,
}

impl ManualService {
    async fn exchange(&self, mut request: IncomingRequest) {
        let principal = match editor_principal(&self.config, request.meta()) {
            Ok(principal) => principal,
            Err(why) => {
                eprintln!("[ci] refused a manual upload from a peer that {why}");
                request.refuse(weida::ErrorCode::Rejected).await;
                return;
            }
        };
        let mut body = request.take_body();
        let upload = match read_frame::<_, RunUpload>(&mut body).await {
            Ok(Some(upload)) => upload,
            Ok(None) => return,
            Err(e) => {
                eprintln!("[ci] unreadable manual upload: {e}");
                request.refuse(weida::ErrorCode::Rejected).await;
                return;
            }
        };
        let mut reply = match request.reply(TransferMeta::default()).await {
            Ok(reply) => reply,
            Err(e) => {
                eprintln!("[ci] cannot open the reply half of a manual upload: {e}");
                return;
            }
        };
        let answer = match self.accept(&principal, upload, &mut body).await {
            Ok((repo, number)) => RunReply::Accepted { repo, number },
            Err(message) => {
                eprintln!("[ci] manual upload refused: {message}");
                RunReply::Refused { message }
            }
        };
        if let Err(e) = write_frame(&mut reply, &answer).await {
            eprintln!("[ci] cannot answer a manual upload: {e}");
        }
        let _ = reply.finish();
    }

    /// Stores `upload` and the bundles that follow it on `body`, hands it to
    /// the scheduler and waits for the number of its pipeline. Returns that
    /// pipeline's repository and number. An origin that is no repository of
    /// `ci.toml` is refused before a byte is stored.
    async fn accept<R>(
        &self,
        principal: &str,
        upload: RunUpload,
        body: &mut R,
    ) -> Result<(String, u64), String>
    where
        R: AsyncRead + Unpin,
    {
        validate(&upload)?;
        let (repo, default_branch) = {
            let (name, config) = repo_of(&self.config, &upload.origin)?;
            (name.to_owned(), config.default_branch.clone())
        };
        let id = new_upload_id(principal);
        eprintln!(
            "[ci] manual {id}: {} of {repo} from {}",
            upload.jobs.join(", "),
            upload.actor
        );
        if let Err(e) = self.store(&id, &upload, body).await {
            remove_upload(&self.state_dir, &id);
            return Err(e);
        }
        let event = CiEvent {
            repo: repo.clone(),
            kind: EventKind::Manual,
            git_ref: default_branch,
            sha: upload.dirs.first().map(|dir| dir.sha.clone()),
            cron: None,
            actor: upload.actor,
            delivery: None,
            upload: Some(id.clone()),
            received: crate::now_secs(),
        };
        if let Err(e) = event::write_inbox(&self.state_dir, &event) {
            remove_upload(&self.state_dir, &id);
            return Err(e);
        }
        // From here on the scheduler owns the directory, whether or not it
        // answers in time.
        let number = self.number_of(&id).await?;
        Ok((repo, number))
    }

    /// Writes the bundles and then the description of upload `id`: a
    /// directory with a description holds all of them.
    async fn store<R>(&self, id: &str, upload: &RunUpload, body: &mut R) -> Result<(), String>
    where
        R: AsyncRead + Unpin,
    {
        let dir = upload_dir(&self.state_dir, id);
        tokio::fs::create_dir_all(&dir)
            .await
            .map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
        for (index, (&size, checkout)) in upload.sizes.iter().zip(&upload.dirs).enumerate() {
            let path = dir.join(format!("{index}.bundle"));
            let mut file = tokio::fs::File::create(&path)
                .await
                .map_err(|e| format!("cannot create {}: {e}", path.display()))?;
            copy_exact(body, size, &mut file)
                .await
                .map_err(|e| format!("bundle of {}: {e}", checkout.dir))?;
        }
        write_upload(&self.state_dir, id, upload)
    }

    /// The number the scheduler gave the pipeline of upload `id`, once it
    /// says so; the file it says it in goes with the read.
    async fn number_of(&self, id: &str) -> Result<u64, String> {
        let path = number_path(&self.state_dir, id);
        let deadline = Instant::now() + self.wait;
        loop {
            if let Ok(text) = tokio::fs::read_to_string(&path).await
                && let Ok(number) = text.trim().parse::<u64>()
            {
                let _ = tokio::fs::remove_file(&path).await;
                return Ok(number);
            }
            if Instant::now() >= deadline {
                return Err(format!(
                    "the CI did not start the manual run within {} s",
                    self.wait.as_secs()
                ));
            }
            tokio::time::sleep(NUMBER_POLL).await;
        }
    }
}

#[cfg(test)]
mod tests {
    use zeughaus_link::enlist::UploadDir;

    use super::*;

    const FP: &str = "sha256:0000000000000000000000000000000000000000000000000000000000000001";

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zci-manual-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create the scratch directory");
        dir
    }

    fn config() -> Arc<CiConfig> {
        Arc::new(
            CiConfig::parse(&format!(
                "default_runner = \"x\"\n\
                 [runners.x]\nfingerprint = \"{FP}\"\n\
                 [repos.griasdi]\nurl = \"https://github.com/Griasdi/Griasdi.git\"\n\
                 default_branch = \"trunk\"\n"
            ))
            .expect("a valid ci.toml"),
        )
    }

    fn dir(name: &str, fill: char) -> UploadDir {
        UploadDir {
            dir: name.to_owned(),
            sha: fill.to_string().repeat(40),
        }
    }

    fn upload(origin: &str, sizes: &[u64]) -> RunUpload {
        RunUpload {
            origin: origin.to_owned(),
            actor: "hannes@tuco-pc".to_owned(),
            summary: "fix(core): thing\nbody".to_owned(),
            jobs: ["build-windows-amd64", "check-windows-amd64"]
                .map(str::to_owned)
                .to_vec(),
            dirs: ["griasdi", "weida", "nodegraph"]
                .iter()
                .zip("abc".chars())
                .take(sizes.len())
                .map(|(name, fill)| dir(name, fill))
                .collect(),
            sizes: sizes.to_vec(),
        }
    }

    #[test]
    fn one_repository_is_named_the_same_by_every_url_form() {
        let same = [
            "git@github.com:griasdi/griasdi.git",
            "https://github.com/Griasdi/Griasdi.git",
            "https://github.com/griasdi/griasdi",
            "ssh://git@github.com/Griasdi/griasdi.git/",
            "https://user:token@github.com/griasdi/griasdi.git",
            " HTTP://GitHub.com/griasdi/griasdi.git\n",
        ];
        for url in same {
            assert_eq!(normalize_url(url), "github.com/griasdi/griasdi", "{url}");
        }
        // Another repository, another host or another port is another name.
        assert_ne!(
            normalize_url("git@github.com:griasdi/weida.git"),
            normalize_url("git@github.com:griasdi/griasdi.git")
        );
        assert_eq!(
            normalize_url("ssh://git@git.example.org:2222/o/r.git"),
            "git.example.org:2222/o/r"
        );
        // A local checkout is its path, with or without the scheme.
        assert_eq!(normalize_url("/tmp/zh-fx/demo.git"), "/tmp/zh-fx/demo");
        assert_eq!(normalize_url("file:///tmp/zh-fx/demo"), "/tmp/zh-fx/demo");
    }

    #[test]
    fn a_repository_is_one_of_ci_toml() {
        let config = config();
        let name = |origin: &str| repo_of(&config, origin).map(|(name, _)| name.to_owned());
        assert_eq!(
            name("git@github.com:griasdi/griasdi.git").as_deref(),
            Ok("griasdi")
        );
        assert_eq!(
            name("https://github.com/Griasdi/Griasdi.git").as_deref(),
            Ok("griasdi")
        );
        assert_eq!(
            repo_of(&config, "git@github.com:griasdi/griasdi.git")
                .unwrap()
                .1
                .default_branch,
            "trunk"
        );
        // Nothing else is one: not the last path segment of an unknown URL,
        // not another owner's repository of a configured name, and not
        // nothing.
        for origin in [
            "git@github.com:Tuco86/Scan-RS.git",
            "git@example.org:someone/griasdi.git",
            "git@example.org:someone/iced_nodegraph.git",
            "",
        ] {
            assert_eq!(
                name(origin),
                Err(format!("{origin} is no repository of ci.toml")),
                "{origin:?}"
            );
        }
    }

    #[test]
    fn an_upload_is_checked_before_a_byte_of_it_is_stored() {
        let ok = upload("git@github.com:griasdi/griasdi.git", &[10, 20]);
        assert_eq!(validate(&ok), Ok(()));

        let change = |what: &str, edit: &dyn Fn(&mut RunUpload)| {
            let mut bad = ok.clone();
            edit(&mut bad);
            let err = validate(&bad).expect_err(what);
            assert!(!err.is_empty(), "{what}");
        };
        change("no jobs", &|u| u.jobs.clear());
        change("too many jobs", &|u| {
            u.jobs = (0..=MAX_MANUAL_JOBS).map(|i| format!("job-{i}")).collect();
        });
        change("a job twice", &|u| u.jobs.push(u.jobs[0].clone()));
        change("a job name", &|u| u.jobs[0] = "Build Windows".to_owned());
        change("no dirs", &|u| {
            u.dirs.clear();
            u.sizes.clear();
        });
        change("too many dirs", &|u| {
            u.dirs = (0..=MAX_MANUAL_DIRS)
                .map(|i| dir(&format!("d{i}"), 'a'))
                .collect();
            u.sizes = vec![1; MAX_MANUAL_DIRS + 1];
        });
        change("a size missing", &|u| {
            u.sizes.pop();
        });
        change("a dir name", &|u| u.dirs[1].dir = "../weida".to_owned());
        change("a dir twice", &|u| u.dirs[1].dir = "griasdi".to_owned());
        change("a short sha", &|u| u.dirs[0].sha = "abc".to_owned());
        change("a sha that is no hex", &|u| u.dirs[0].sha = "g".repeat(40));
        change("a big bundle", &|u| {
            u.sizes[1] = MAX_MANUAL_BUNDLE_BYTES + 1
        });
    }

    #[test]
    fn the_jobs_of_an_upload_are_one_to_the_most_and_each_named_once() {
        let ok = upload("git@github.com:griasdi/griasdi.git", &[10]);
        let jobs = |jobs: Vec<String>| validate(&RunUpload { jobs, ..ok.clone() });
        let numbered = |count: usize| (0..count).map(|i| format!("job-{i}")).collect();

        assert_eq!(jobs(numbered(1)), Ok(()));
        assert_eq!(jobs(numbered(MAX_MANUAL_JOBS)), Ok(()));
        assert_eq!(
            jobs(Vec::new()),
            Err(format!("0 jobs are not in 1..={MAX_MANUAL_JOBS}"))
        );
        assert_eq!(
            jobs(numbered(MAX_MANUAL_JOBS + 1)),
            Err(format!(
                "{} jobs are not in 1..={MAX_MANUAL_JOBS}",
                MAX_MANUAL_JOBS + 1
            ))
        );
        assert_eq!(
            jobs(["a", "b", "a"].map(str::to_owned).to_vec()),
            Err("job a is named twice".to_owned())
        );
    }

    #[test]
    fn an_upload_directory_is_named_by_its_id_and_nothing_else_is_one() {
        let id = new_upload_id("sha256:abc");
        assert!(valid_upload_id(&id), "{id}");
        assert_ne!(id, new_upload_id("sha256:abc"));
        for bad in [
            "",
            "../etc/passwd0000",
            "0123456789ABCDEF",
            "0123456789abcde",
        ] {
            assert!(!valid_upload_id(bad), "{bad:?}");
        }
        assert!(read_upload(Path::new("/nonexistent"), "../x").is_err());
    }

    #[test]
    fn the_subject_is_the_first_line_of_the_summary() {
        assert_eq!(subject("fix(core): thing\nsecond line"), "fix(core): thing");
        assert_eq!(subject("  fix(core): thing  "), "fix(core): thing");
        assert_eq!(subject(""), "");
        assert_eq!(subject(&"é".repeat(500)).chars().count(), SUBJECT_CHARS);
    }

    #[test]
    fn stale_uploads_and_numbers_go_and_fresh_ones_stay() {
        let state = scratch("stale");
        let (old, fresh) = ("0000000000000001", "0000000000000002");
        for id in [old, fresh] {
            std::fs::create_dir_all(upload_dir(&state, id)).unwrap();
            write_number(&state, id, 3).unwrap();
        }
        let long_ago = SystemTime::now() - 2 * STALE_AFTER;
        for path in [upload_dir(&state, old), number_path(&state, old)] {
            std::fs::File::open(&path)
                .unwrap()
                .set_modified(long_ago)
                .unwrap();
        }
        remove_stale(&state, STALE_AFTER);
        assert!(!upload_dir(&state, old).exists());
        assert!(!number_path(&state, old).exists());
        assert!(upload_dir(&state, fresh).exists());
        assert!(number_path(&state, fresh).exists());
        let _ = std::fs::remove_dir_all(&state);
    }

    fn service(state: &Path, wait: Duration) -> ManualService {
        ManualService {
            state_dir: state.to_path_buf(),
            config: config(),
            wait,
        }
    }

    /// The scheduler's half of the exchange: takes the event the service
    /// queued and says which pipeline it became.
    async fn scheduler_answers(state: &Path, number: u64) -> CiEvent {
        loop {
            event::drain_inbox(state);
            if let Some((_, event)) = event::queued(state).pop() {
                let id = event.upload.clone().expect("an upload");
                write_number(state, &id, number).unwrap();
                return event;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    }

    #[tokio::test]
    async fn an_accepted_upload_is_stored_queued_and_answered_with_its_number() {
        let state = scratch("accept");
        let service = service(&state, Duration::from_secs(20));
        let sent = upload("git@github.com:griasdi/griasdi.git", &[5, 6]);
        let mut body: &[u8] = b"firstsecond-rest";

        let (answer, queued) = tokio::join!(
            service.accept("sha256:me", sent.clone(), &mut body),
            scheduler_answers(&state, 12)
        );
        assert_eq!(answer, Ok(("griasdi".to_owned(), 12)));
        assert_eq!(body, b"-rest", "exactly the declared bytes were read");

        let id = queued.upload.clone().expect("an upload");
        assert_eq!(
            (queued.repo.as_str(), queued.kind, queued.git_ref.as_str()),
            ("griasdi", EventKind::Manual, "trunk")
        );
        assert_eq!(queued.sha, Some("a".repeat(40)));
        assert_eq!(queued.actor, "hannes@tuco-pc");
        assert_eq!(event::queue_key(&queued), format!("griasdi.manual.{id}"));
        assert_eq!(read_upload(&state, &id), Ok(sent));
        assert_eq!(
            std::fs::read(bundle_path(&state, &id, 0)).unwrap(),
            b"first"
        );
        assert_eq!(
            std::fs::read(bundle_path(&state, &id, 1)).unwrap(),
            b"second"
        );
        // The number was consumed.
        assert!(!number_path(&state, &id).exists());
        let _ = std::fs::remove_dir_all(&state);
    }

    /// What a refused upload leaves: nothing, and no event.
    #[tokio::test]
    async fn an_upload_that_cannot_be_stored_leaves_nothing() {
        let state = scratch("refuse");
        let service = service(&state, Duration::from_secs(1));
        let left = || {
            std::fs::read_dir(crate::ci_dir(&state).join("manual"))
                .map(|entries| entries.count())
                .unwrap_or(0)
        };

        // The body ends inside the second bundle.
        let mut short: &[u8] = b"firstsec";
        let sent = upload("git@github.com:griasdi/griasdi.git", &[5, 6]);
        let err = service
            .accept("sha256:me", sent, &mut short)
            .await
            .expect_err("a short upload");
        assert!(
            err.contains("bundle of weida") && err.contains("3 bytes short"),
            "{err}"
        );
        assert_eq!(left(), 0);

        // A repository that ci.toml does not list, and a run of no job.
        let mut body: &[u8] = b"first";
        let unlisted = upload("git@example.org:someone/iced_nodegraph.git", &[5]);
        let err = service
            .accept("sha256:me", unlisted, &mut body)
            .await
            .expect_err("an unlisted repository");
        assert_eq!(
            err,
            "git@example.org:someone/iced_nodegraph.git is no repository of ci.toml"
        );
        let no_jobs = RunUpload {
            jobs: Vec::new(),
            ..upload("git@github.com:griasdi/griasdi.git", &[5])
        };
        assert!(
            service
                .accept("sha256:me", no_jobs, &mut body)
                .await
                .is_err()
        );
        assert_eq!(body, b"first", "nothing of a refused upload is read");
        assert_eq!(left(), 0);
        event::drain_inbox(&state);
        assert!(event::queued(&state).is_empty());
        let _ = std::fs::remove_dir_all(&state);
    }

    /// A scheduler that does not answer leaves the upload to itself: the
    /// event is queued, and it may still run.
    #[tokio::test]
    async fn an_upload_the_scheduler_does_not_answer_is_refused_but_kept() {
        let state = scratch("silent");
        let service = service(&state, Duration::from_millis(400));
        let sent = upload("git@github.com:griasdi/griasdi.git", &[5]);
        let mut body: &[u8] = b"first";
        let err = service
            .accept("sha256:me", sent, &mut body)
            .await
            .expect_err("no answer");
        assert!(
            err.starts_with("the CI did not start the manual run within "),
            "{err}"
        );
        event::drain_inbox(&state);
        let queued = event::queued(&state);
        assert_eq!(queued.len(), 1);
        let id = queued[0].1.upload.clone().expect("an upload");
        assert!(read_upload(&state, &id).is_ok());
        let _ = std::fs::remove_dir_all(&state);
    }
}
