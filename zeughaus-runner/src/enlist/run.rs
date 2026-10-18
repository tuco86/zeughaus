//! One assignment, from staging to its end, and the delivery of its log.
//!
//! An `Assign` is prepared on a thread of its own: the mirror is fetched, the
//! commit pinned, the inputs and secrets staged, the scripts rendered and the
//! run started as a job of this runner's [`JobHost`]. From the start on, the
//! run is described by `assigned/<run>.json`; a restarted runner finds it
//! there, reattaches the terminal (the mux does that) and goes on
//! delivering.
//!
//! The CI's copy of the log is the run's `wal` file byte for byte: a
//! delivery thread per run sends it in chunks from the offset the CI reported
//! at the last welcome, and sends the end of the run only after the file has
//! stopped growing. Nothing here remembers what the CI acknowledged beyond
//! the connection that is up: after a loss, the next welcome says it again.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use zeughaus_job::{JobSpec, ProcessHost, RunExit, RunHandle, record_run_end};
use zeughaus_link::enlist::{
    Assignment, FromRunner, Input, InputSource, MAX_OUTPUT_CHUNK, Place, RunEnd, Upload,
    parse_run_id, run_id,
};
use zeughaus_link::manual::{MAX_MANUAL_DIRS, valid_dir_name};

use super::cleanup;
use super::launch::{self, Launch, Target};
use super::stage;
use super::{
    Executor, RunCtl, artifacts_dir, assigned_dir, git, lock, mirror_dir, now_secs, work_dir,
};
use crate::files::{read_json, write_atomic, write_json};
use crate::names::valid_name;

/// How often a run's log is looked at for new bytes.
const LOG_POLL: Duration = Duration::from_millis(200);

/// Passes without growth after a run ended before its end is sent: the
/// last output can still be on its way into the log when the child is gone.
const SETTLE_PASSES: u32 = 3;

/// How often a recovered run's exit record is looked for.
const RECOVERY_POLL: Duration = Duration::from_secs(2);

/// How often an assignment waits for the runner to stop being held.
const HELD_POLL: Duration = Duration::from_secs(1);

/// What `<state>/ci/assigned/<run>.json` holds. Secrets and the fetch token
/// are not in it: they are not needed once the run started, and a file that
/// stays for days is not where they belong.
#[derive(Debug, Serialize, Deserialize)]
struct Assigned {
    assignment: Assignment,
    run_dir: PathBuf,
    started: u64,
    end: Option<RunEnd>,
}

/// Why a run did not start.
enum Prep {
    Failed(String),
    /// The CI asked for it to end before it started.
    Aborted,
}

impl From<String> for Prep {
    fn from(message: String) -> Prep {
        Prep::Failed(message)
    }
}

/// A run that started.
struct Started {
    handle: Box<dyn RunHandle>,
    run_dir: PathBuf,
    started: u64,
}

fn check_abort(ctl: &RunCtl) -> Result<(), Prep> {
    if ctl.abort.load(Ordering::SeqCst) {
        Err(Prep::Aborted)
    } else {
        Ok(())
    }
}

/// The assignment is the CI's, and its names become paths here.
fn validate(a: &Assignment) -> Result<(), String> {
    let names_ok = valid_name(&a.repo) && valid_name(&a.job);
    if !names_ok || a.run != run_id(&a.repo, a.number, &a.job) {
        return Err(format!("malformed assignment for run {:?}", a.run));
    }
    // The workspace is named after the place, so the image or machine name
    // becomes a path component as well.
    let workspace = a.place.workspace();
    if !valid_name(&workspace) {
        return Err(format!("{workspace:?} is not a workspace name"));
    }
    if a.sha.is_empty() || !a.sha.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err(format!("{:?} is not a commit", a.sha));
    }
    // The file is a name inside `.zeughaus-ci/`, quoted into a script.
    if a.file.is_empty() || a.file.contains('/') || a.file.starts_with('.') {
        return Err(format!("{:?} is not a job file name", a.file));
    }
    // A Windows runner has neither containers nor machines, and no `sh`.
    if cfg!(windows) && (a.place != Place::Host || a.interpreter != "powershell") {
        return Err(format!(
            "a Windows runner runs .ps1 jobs on its own host, not {}",
            a.file
        ));
    }
    if let Some(upload) = &a.upload {
        validate_upload(a, upload)?;
    }
    Ok(())
}

/// The names of a manual run's sibling checkouts become directories below its
/// workspace and the commits of all its checkouts arguments of git: both are
/// the CI's words.
fn validate_upload(a: &Assignment, upload: &Upload) -> Result<(), String> {
    // A machine's workspace is one bundle, copied into the guest.
    if !matches!(a.place, Place::Host | Place::Container { .. }) {
        return Err("a manual run runs on a host or in a container".to_owned());
    }
    if upload.dirs.is_empty() || upload.dirs.len() > MAX_MANUAL_DIRS {
        return Err(format!(
            "{} checkouts are not in 1..={MAX_MANUAL_DIRS}",
            upload.dirs.len()
        ));
    }
    for dir in &upload.dirs {
        if !valid_dir_name(&dir.dir) {
            return Err(format!("{:?} is not a checkout directory name", dir.dir));
        }
        if dir.sha.is_empty() || !dir.sha.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(format!("{:?} is not a commit", dir.sha));
        }
    }
    Ok(())
}

/// What `CI_SIBLINGS` names in a manual run: the directory of the workspace
/// that holds its sibling checkouts, at the path the job sees it. Only a host
/// and a container get here: `validate` refuses an upload for a machine.
fn siblings_var(place: &Place, workspace: &Path) -> String {
    match place {
        Place::Container { .. } => format!("/work/{}", launch::SIBLINGS_DIR),
        Place::Host | Place::Machine { .. } => workspace
            .join(launch::SIBLINGS_DIR)
            .to_string_lossy()
            .into_owned(),
    }
}

/// The run id as one path component of `<state>/ci/`.
pub(super) fn safe_run(run: &str) -> bool {
    !run.contains('/') && parse_run_id(run).is_some()
}

/// Removes a run's `inputs/`, which a container job may have left files in.
fn remove_inputs(inputs: &Path) {
    if let Err(e) = cleanup::remove_tree(inputs) {
        eprintln!("[enlist] cannot remove {}: {e}", inputs.display());
    }
}

impl Executor {
    /// Takes an assignment. Returns at once: the work runs on a thread, and
    /// a run id that is already known is ignored, which is what a CI that
    /// assigned again after a reconnect gets.
    pub fn assign(self: &Arc<Self>, mut assignment: Assignment) {
        let secrets = std::mem::take(&mut assignment.secrets);
        let token = assignment.fetch_token.take();
        let run = assignment.run.clone();
        let ctl = Arc::new(RunCtl::new(assignment));
        {
            let mut runs = lock(&self.runs);
            if runs.contains_key(&run) {
                eprintln!("[enlist] {run}: assigned again while known; ignored");
                return;
            }
            runs.insert(run.clone(), Arc::clone(&ctl));
        }
        let exec = Arc::clone(self);
        let thread_run = run.clone();
        let spawned = std::thread::Builder::new()
            .name("zeughaus-enlist-job".into())
            .spawn(move || {
                let run = thread_run;
                match exec.prepare(&ctl, &secrets, token.as_deref()) {
                    Ok(started) => exec.supervise(&ctl, started),
                    Err(prep) => {
                        let message = match prep {
                            Prep::Failed(message) => message,
                            Prep::Aborted => "killed before it started".to_owned(),
                        };
                        eprintln!("[enlist] {run}: {message}");
                        lock(&exec.runs).remove(&run);
                        exec.drop_prefetch(&run);
                        exec.send(FromRunner::StartFailed { run, message });
                    }
                }
            });
        if let Err(e) = spawned {
            // Called from the connection's task: nothing here may block.
            eprintln!("[enlist] cannot start a thread for the assignment: {e}");
            lock(&self.runs).remove(&run);
            self.drop_prefetch(&run);
            let tx = lock(&self.link).tx.clone();
            if let Some(tx) = tx {
                let _ = tx.try_send(FromRunner::StartFailed {
                    run,
                    message: format!("cannot start a thread: {e}"),
                });
            }
        }
    }

    /// Ends a run, or keeps one that has not started from starting.
    pub fn kill(&self, run: &str) {
        let Some(ctl) = self.ctl(run) else {
            return;
        };
        ctl.abort.store(true, Ordering::SeqCst);
        if let Some(run_dir) = ctl.running_in()
            && let Err(e) = self.host.kill_run(&run_dir)
        {
            eprintln!("[enlist] {run}: cannot kill: {e}");
        }
    }

    /// The CI is done with `run`: its record and its delivery thread go. The
    /// artifact set stays until the CI's `DropArtifacts`, and the run's
    /// inputs go with the run, neither of them here.
    pub fn forget(&self, run: &str) {
        if let Some(ctl) = lock(&self.runs).remove(run) {
            ctl.forgotten.store(true, Ordering::SeqCst);
        }
        if safe_run(run) {
            let _ = std::fs::remove_file(assigned_dir(&self.state_dir).join(format!("{run}.json")));
        }
        // What was fetched early for the run and never staged.
        self.drop_prefetch(run);
    }

    /// Everything up to and including the start of the process.
    fn prepare(
        self: &Arc<Self>,
        ctl: &Arc<RunCtl>,
        secrets: &[(String, String)],
        token: Option<&str>,
    ) -> Result<Started, Prep> {
        let a = &ctl.assignment;
        validate(a)?;
        // A held runner starts nothing new; the assignment waits for it to be
        // released (or killed).
        while self.host.held() {
            check_abort(ctl)?;
            std::thread::sleep(HELD_POLL);
        }
        check_abort(ctl)?;
        let mirror = mirror_dir(&self.state_dir, &a.repo);
        // A manual run has its commits in bundles the CI stored, not on the
        // forge.
        if a.upload.is_none() {
            // Jobs of one pipeline start together, and two fetches into one
            // mirror refuse each other's ref updates. A commit is immutable:
            // one the mirror already holds needs no fetch at all.
            let mirror_lock = self.mirror_lock(&a.repo);
            let _guard = lock(&mirror_lock);
            if !git::has_commit(&mirror, &a.sha) {
                git::fetch_mirror(&mirror, &a.url, &a.fetch_user, token)?;
                if !git::has_commit(&mirror, &a.sha) {
                    return Err(Prep::Failed(format!(
                        "commit {} is not on the forge",
                        a.sha
                    )));
                }
            }
            git::pin_commit(&mirror, a.number, &a.sha)?;
        }
        check_abort(ctl)?;

        let run_dir = self.host.new_run_dir()?;
        if let Some(upload) = &a.upload
            && let Err(prep) = self.stage_bundles(ctl, upload, &run_dir)
        {
            let _ = std::fs::remove_dir_all(&run_dir);
            return Err(prep);
        }

        let secret_dir = match stage::secret_dir(&self.state_dir, &a.run) {
            Ok(dir) => dir,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&run_dir);
                return Err(Prep::Failed(e));
            }
        };
        match self.stage_and_spawn(ctl, &mirror, &run_dir, &secret_dir, secrets) {
            Ok(started) => Ok(started),
            Err(prep) => {
                // Nothing ran: the launcher that would remove the secrets
                // never did.
                let _ = std::fs::remove_dir_all(&secret_dir);
                let _ = std::fs::remove_dir_all(&run_dir);
                Err(prep)
            }
        }
    }

    /// Downloads the bundles of a manual run into `<run>/bundles/`.
    fn stage_bundles(
        self: &Arc<Self>,
        ctl: &RunCtl,
        upload: &Upload,
        run_dir: &Path,
    ) -> Result<(), Prep> {
        for (index, _) in (0u32..).zip(&upload.dirs) {
            let dest = launch::bundle_file(run_dir, index);
            self.fetch_manual_bundle(&ctl.assignment.run, index, &dest)
                .map_err(|e| {
                    Prep::Failed(format!(
                        "cannot download bundle {index} of the manual run: {e}"
                    ))
                })?;
            check_abort(ctl)?;
        }
        Ok(())
    }

    fn stage_and_spawn(
        self: &Arc<Self>,
        ctl: &Arc<RunCtl>,
        mirror: &Path,
        run_dir: &Path,
        secret_dir: &Path,
        secrets: &[(String, String)],
    ) -> Result<Started, Prep> {
        let a = &ctl.assignment;
        let started = now_secs();
        let inputs = run_dir.join("inputs");
        std::fs::create_dir_all(&inputs).map_err(|e| format!("cannot create inputs: {e}"))?;
        self.stage_inputs(&a.run, &inputs, &a.inputs)?;
        check_abort(ctl)?;

        let place_dir = a.place.workspace();
        let workspace = work_dir(&self.state_dir, &a.repo, &place_dir);
        let (cpus, workspace_var, output_var, inputs_var) = match &a.place {
            Place::Host => (
                self.cpus,
                workspace.to_string_lossy().into_owned(),
                run_dir.join("artifacts").to_string_lossy().into_owned(),
                inputs.to_string_lossy().into_owned(),
            ),
            Place::Container { .. } => {
                let containers = self
                    .config
                    .containers
                    .as_ref()
                    .filter(|_| self.containers)
                    .ok_or_else(|| format!("runner {} offers no containers", self.config.name))?;
                (
                    containers.cpus,
                    "/work".to_owned(),
                    "/ci/run/artifacts".to_owned(),
                    "/ci/inputs".to_owned(),
                )
            }
            Place::Machine { name } => {
                let vm = self.machine(name)?;
                let guest = launch::guest_run_dir(&a.run);
                (
                    vm.cpus(),
                    launch::guest_workspace(&a.repo, &place_dir),
                    format!(r"{guest}\out"),
                    format!(r"{guest}\in"),
                )
            }
        };
        let mut env = a.env.clone();
        env.extend([
            ("CI_WORKSPACE".to_owned(), workspace_var),
            ("CI_OUTPUT".to_owned(), output_var),
            ("CI_INPUTS".to_owned(), inputs_var),
            ("CI_CPUS".to_owned(), cpus.to_string()),
            ("CARGO_BUILD_JOBS".to_owned(), cpus.to_string()),
        ]);
        // The other checkouts of a manual run, for a job that links to them.
        if a.upload.is_some() {
            env.push(("CI_SIBLINGS".to_owned(), siblings_var(&a.place, &workspace)));
        }

        let write = |path: &Path, text: &str| write_atomic(path, text.as_bytes());
        // Owned here because the launcher's target borrows them.
        let containers = self.config.containers.clone().unwrap_or_default();
        let vm = match &a.place {
            Place::Machine { name } => Some(self.machine(name)?),
            _ => None,
        };
        let ssh_host = vm.as_ref().map(|vm| vm.ssh_host().to_owned());
        let mut host_env = Vec::new();
        let target = match &a.place {
            Place::Container { image, tag } => {
                let mut file_env = env;
                file_env.push(("ZEUGHAUS_RUN_DIR".to_owned(), "/ci/run".to_owned()));
                file_env.extend(secrets.iter().cloned());
                stage::write_secret(&secret_dir.join("env"), &launch::env_file(&file_env))?;
                Target::Container {
                    engine: stage::Engine::host(),
                    workspace: &workspace,
                    image,
                    tag,
                    cpus: containers.cpus,
                    memory: &containers.memory,
                }
            }
            Place::Machine { .. } => {
                let mut remote_env = env;
                remote_env.extend(secrets.iter().cloned());
                git::bundle(mirror, a.number, &run_dir.join("src.bundle"))?;
                stage::write_secret(&secret_dir.join("env.ps1"), &launch::env_ps1(&remote_env))?;
                write(&run_dir.join("vm").join("inner.ps1"), &launch::inner_ps1(a))?;
                Target::WindowsVm {
                    ssh_host: ssh_host.as_deref().unwrap_or_default(),
                }
            }
            Place::Host => {
                if cfg!(windows) {
                    stage::write_secret(&secret_dir.join("env.ps1"), &launch::env_ps1(secrets))?;
                } else {
                    stage::write_secret(&secret_dir.join("env.sh"), &launch::env_sh(secrets))?;
                }
                host_env = env;
                Target::Host {
                    workspace: &workspace,
                }
            }
        };
        let (program, args) = if cfg!(windows) {
            // `validate` let only host jobs through.
            let script = run_dir.join("launch.ps1");
            let rendered = launch::launch_ps1(a, secret_dir, mirror, &workspace, &self.exe);
            write(&script, &rendered)?;
            let args = [
                "-NoLogo",
                "-NoProfile",
                "-ExecutionPolicy",
                "Bypass",
                "-File",
            ]
            .into_iter()
            .map(str::to_owned)
            .chain([script.to_string_lossy().into_owned()])
            .collect();
            (launch::windows_powershell(), args)
        } else {
            let script = run_dir.join("launch.sh");
            let rendered = launch::launch_sh(&Launch {
                assignment: a,
                run_dir,
                secret_dir,
                mirror,
                target,
            });
            write(&script, &rendered)?;
            if vm.is_none() {
                let exe = matches!(a.place, Place::Host).then_some(self.exe.as_path());
                write(&run_dir.join("inner.sh"), &launch::inner_sh(a, exe))?;
                // Apple's containers set their clock from the host's before
                // the job starts.
                if matches!(a.place, Place::Container { .. })
                    && stage::Engine::host() == stage::Engine::Apple
                {
                    write(&run_dir.join("clock.sh"), launch::clock_sh())?;
                }
            }
            (
                "/bin/sh".to_owned(),
                vec![script.to_string_lossy().into_owned()],
            )
        };
        check_abort(ctl)?;

        // Booting takes minutes; the lease it takes is given back on any
        // failure from here on.
        if let Some(vm) = &vm {
            vm.acquire()?;
        }
        let spec = JobSpec {
            label: format!("{}/{} #{}", a.repo, a.job, a.number),
            program,
            args,
            env: host_env,
            cwd: Some(run_dir.to_path_buf()),
            run_dir: run_dir.to_path_buf(),
            started,
            artifacts: Vec::new(),
            external: true,
        };
        let handle = match self.host.spawn(spec) {
            Ok(handle) => handle,
            Err(e) => {
                if let Some(vm) = &vm {
                    vm.release();
                }
                return Err(Prep::Failed(e));
            }
        };

        *lock(&ctl.live) = Some(run_dir.to_path_buf());
        let assigned = Assigned {
            assignment: a.clone(),
            run_dir: run_dir.to_path_buf(),
            started,
            end: None,
        };
        if let Err(e) = write_json(&self.assigned_path(&a.run), &assigned) {
            // The run is going; a restart of this process would lose track
            // of it, which is worth a line and not worth stopping it.
            eprintln!("[enlist] {}: {e}", a.run);
        }
        self.send(FromRunner::Started { run: a.run.clone() });
        self.deliver_log(ctl, run_dir);
        if ctl.abort.load(Ordering::SeqCst)
            && let Err(e) = self.host.kill_run(run_dir)
        {
            eprintln!("[enlist] {}: cannot kill: {e}", a.run);
        }
        Ok(Started {
            handle,
            run_dir: run_dir.to_path_buf(),
            started,
        })
    }

    /// The lock that serializes changes to `repo`'s mirror.
    fn mirror_lock(&self, repo: &str) -> Arc<Mutex<()>> {
        Arc::clone(lock(&self.mirrors).entry(repo.to_owned()).or_default())
    }

    fn assigned_path(&self, run: &str) -> PathBuf {
        assigned_dir(&self.state_dir).join(format!("{run}.json"))
    }

    fn machine(&self, name: &str) -> Result<Arc<super::machine::Vm>, String> {
        self.machines
            .get(name)
            .cloned()
            .ok_or_else(|| format!("runner {} offers no machine {name}", self.config.name))
    }

    /// Stages every input at once. Each is a transfer of its own, bound by a
    /// link or a disk and not by this thread, so they overlap. The first
    /// error, in the order of the inputs, is the run's.
    fn stage_inputs(
        self: &Arc<Self>,
        consumer: &str,
        inputs: &Path,
        needs: &[Input],
    ) -> Result<(), Prep> {
        std::thread::scope(|scope| {
            let staging: Vec<_> = needs
                .iter()
                .map(|input| {
                    std::thread::Builder::new()
                        .name("zeughaus-enlist-stage".into())
                        .spawn_scoped(scope, move || self.stage_input(consumer, inputs, input))
                })
                .collect();
            let mut first = Ok(());
            for thread in staging {
                let staged = match thread {
                    Ok(handle) => handle.join().unwrap_or_else(|_| {
                        Err(Prep::Failed("an input thread panicked".to_owned()))
                    }),
                    Err(e) => Err(Prep::Failed(format!(
                        "cannot start a thread to stage inputs: {e}"
                    ))),
                };
                if first.is_ok() {
                    first = staged;
                }
            }
            first
        })
    }

    /// Puts one need's artifacts under `inputs/<need>`, for the run
    /// `consumer`.
    fn stage_input(
        self: &Arc<Self>,
        consumer: &str,
        inputs: &Path,
        input: &Input,
    ) -> Result<(), Prep> {
        if !valid_name(&input.need) {
            return Err(Prep::Failed(format!("{:?} is not a job name", input.need)));
        }
        let dest = inputs.join(&input.need);
        match &input.source {
            InputSource::Local { run } => {
                if !safe_run(run) {
                    return Err(Prep::Failed(format!("{run:?} is not a run")));
                }
                let from = artifacts_dir(&self.state_dir).join(run);
                if from.is_dir() {
                    stage::link_tree(&from, &dest)
                        .map_err(|e| format!("cannot stage inputs of {}: {e}", input.need))?;
                }
            }
            InputSource::Ci { run } => {
                if !safe_run(run) {
                    return Err(Prep::Failed(format!("{run:?} is not a run")));
                }
                // A set the CI had this runner fetch before the run started.
                if self.claim_prefetch(consumer, &input.need, &dest)? {
                    return Ok(());
                }
                self.fetch_input(consumer, &input.need, run, &dest)
                    .map_err(|e| format!("cannot fetch inputs of {}: {e}", input.need))?;
            }
        }
        Ok(())
    }

    /// Waits for the run to end, then keeps what it left and reports it.
    fn supervise(self: &Arc<Self>, ctl: &Arc<RunCtl>, started: Started) {
        let Started {
            handle,
            run_dir,
            started,
        } = started;
        let exit = handle.wait();
        // The exit record is what pruning keeps or deletes a run by, and
        // what a restarted runner reads; the job node writes it for its own
        // runs, so this does for the CI's.
        if let Err(e) = record_run_end(&run_dir, exit, started, None, &[]) {
            eprintln!("[enlist] {}: {e}", run_dir.display());
        }
        self.finish(ctl, &run_dir, started, exit);
    }

    /// The run is over: keep its artifacts, give back its machine and make
    /// its end known to the delivery thread.
    fn finish(self: &Arc<Self>, ctl: &Arc<RunCtl>, run_dir: &Path, started: u64, exit: RunExit) {
        let a = &ctl.assignment;
        let timed_out = exit.code == Some(124) || run_dir.join("timeout").exists();
        let source = run_dir.join("artifacts");
        let artifacts = exit.code == Some(0)
            && stage::has_entries(&source)
            && self.keep_artifacts(&a.run, &source);
        let end = RunEnd {
            code: exit.code,
            killed: exit.killed,
            timed_out,
            artifacts,
        };
        if let Place::Machine { name } = &a.place
            && let Some(vm) = self.machines.get(name)
        {
            vm.release();
        }
        if !ctl.forgotten.load(Ordering::SeqCst) {
            let assigned = Assigned {
                assignment: a.clone(),
                run_dir: run_dir.to_path_buf(),
                started,
                end: Some(end.clone()),
            };
            if let Err(e) = write_json(&self.assigned_path(&a.run), &assigned) {
                eprintln!("[enlist] {}: {e}", a.run);
            }
        }
        *lock(&ctl.end) = Some(end);
        eprintln!(
            "[enlist] {}: ended ({})",
            a.run,
            match exit.code {
                Some(code) => format!("exit {code}"),
                None if exit.killed => "killed".to_owned(),
                None => "signal".to_owned(),
            }
        );
        self.housekeeping(a);
        remove_inputs(&run_dir.join("inputs"));
    }

    /// Moves the artifacts of `run` out of its run directory to
    /// `ci/artifacts/<run>`, so the set exists once on disk. Where a rename
    /// is not possible the files are linked instead. False when the set was
    /// not kept.
    fn keep_artifacts(&self, run: &str, source: &Path) -> bool {
        let dest = artifacts_dir(&self.state_dir).join(run);
        if let Err(e) = cleanup::remove_tree(&dest) {
            eprintln!("[enlist] {run}: cannot remove a stale artifact set: {e}");
        }
        if let Some(parent) = dest.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            eprintln!("[enlist] {run}: artifacts not kept: {e}");
            return false;
        }
        let Err(e) = std::fs::rename(source, &dest) else {
            return true;
        };
        eprintln!("[enlist] {run}: cannot move the artifacts ({e}); linking them");
        match stage::link_tree(source, &dest) {
            Ok(()) => true,
            Err(e) => {
                eprintln!("[enlist] {run}: artifacts not kept: {e}");
                false
            }
        }
    }

    /// Removes the inputs of every run that has ended and still has them: a
    /// process that stopped between a run's exit record and the removal left
    /// them there. A run without an exit record is still going, and its end
    /// removes them.
    pub(super) fn sweep_inputs(&self) {
        for run_dir in self.host.run_dirs() {
            if run_dir.join("exit").exists() {
                remove_inputs(&run_dir.join("inputs"));
            }
        }
    }

    /// Disk housekeeping after a run: workspaces over budget and container
    /// images nothing uses any more.
    fn housekeeping(&self, finished: &Assignment) {
        let live: Vec<Arc<RunCtl>> = lock(&self.runs).values().cloned().collect();
        let active: Vec<&Arc<RunCtl>> = live
            .iter()
            .filter(|ctl| ctl.running_in().is_some())
            .collect();
        let in_use: HashSet<PathBuf> = active
            .iter()
            .map(|ctl| {
                work_dir(
                    &self.state_dir,
                    &ctl.assignment.repo,
                    &ctl.assignment.place.workspace(),
                )
            })
            .collect();
        cleanup::enforce_budget(&self.state_dir, self.config.budget_gb, &in_use);
        if self.containers && matches!(finished.place, Place::Container { .. }) {
            let used: HashSet<String> = live
                .iter()
                .filter(|ctl| ctl.assignment.repo == finished.repo)
                .filter_map(|ctl| match &ctl.assignment.place {
                    Place::Container { tag, .. } => Some(tag.clone()),
                    _ => None,
                })
                .chain(match &finished.place {
                    Place::Container { tag, .. } => Some(tag.clone()),
                    _ => None,
                })
                .collect();
            cleanup::prune_images(&finished.repo, &used);
        }
    }

    /// Takes over the runs a previous process left: each one keeps being
    /// delivered, and one without an end is waited for through its exit
    /// record, which the mux's adoption writes when the reattached terminal
    /// ends. A run no terminal was reattached for (a Windows runner has no
    /// shims, and a shim may have died with its host) ended with the
    /// previous process and is recorded as killed.
    pub fn recover(self: &Arc<Self>) {
        let Ok(entries) = std::fs::read_dir(assigned_dir(&self.state_dir)) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_none_or(|ext| ext != "json") {
                continue;
            }
            let assigned: Assigned = match read_json(&path) {
                Ok(assigned) => assigned,
                Err(e) => {
                    eprintln!("[enlist] {e}");
                    continue;
                }
            };
            let Assigned {
                assignment,
                run_dir,
                started,
                end,
            } = assigned;
            let run = assignment.run.clone();
            let ended = end.is_some();
            let ctl = Arc::new(RunCtl::new(assignment));
            *lock(&ctl.live) = Some(run_dir.clone());
            *lock(&ctl.end) = end;
            lock(&self.runs).insert(run.clone(), Arc::clone(&ctl));
            eprintln!(
                "[enlist] {run}: recovered ({})",
                if ended { "ended" } else { "running" }
            );
            self.deliver_log(&ctl, &run_dir);
            if ended {
                continue;
            }
            if let Place::Machine { name } = &ctl.assignment.place
                && let Some(vm) = self.machines.get(name)
            {
                vm.adopt();
            }
            if read_exit_record(&run_dir).is_none() && !self.host.has_run(&run_dir) {
                let exit = RunExit {
                    code: None,
                    killed: true,
                };
                if let Err(e) = record_run_end(&run_dir, exit, started, None, &[]) {
                    eprintln!("[enlist] {run}: {e}");
                }
            }
            let exec = Arc::clone(self);
            let spawned = std::thread::Builder::new()
                .name("zeughaus-enlist-watch".into())
                .spawn(move || {
                    let exit = loop {
                        if let Some(exit) = read_exit_record(&run_dir) {
                            break exit;
                        }
                        std::thread::sleep(RECOVERY_POLL);
                    };
                    exec.finish(&ctl, &run_dir, started, exit);
                });
            if let Err(e) = spawned {
                eprintln!("[enlist] {run}: cannot watch a recovered run: {e}");
            }
        }
    }

    /// Starts the thread that delivers `run`'s log and, once it is over,
    /// its end.
    ///
    /// It follows the connection: each new one starts from the offset the
    /// CI's welcome reports, so a loss costs a resend of what was in flight
    /// and nothing else. It ends when the CI forgets the run.
    fn deliver_log(self: &Arc<Self>, ctl: &Arc<RunCtl>, run_dir: &Path) {
        let exec = Arc::clone(self);
        let run_name = ctl.assignment.run.clone();
        let ctl = Arc::clone(ctl);
        let wal = run_dir.join("wal");
        let spawned = std::thread::Builder::new()
            .name("zeughaus-enlist-log".into())
            .spawn(move || {
                let run = ctl.assignment.run.clone();
                let mut generation = 0u64;
                let mut sent = 0u64;
                let mut idle = 0u32;
                let mut finished = false;
                let mut was_ended = false;
                while !ctl.forgotten.load(Ordering::SeqCst) {
                    std::thread::sleep(LOG_POLL);
                    let Some((current, tx, offset)) = exec.link_of(&run) else {
                        continue;
                    };
                    if current != generation {
                        generation = current;
                        sent = offset;
                        idle = 0;
                        finished = false;
                    }
                    let end = lock(&ctl.end).clone();
                    if end.is_some() && !was_ended {
                        was_ended = true;
                        idle = 0;
                    }
                    match pump(&wal, &run, &tx, &mut sent) {
                        Ok(0) => idle += 1,
                        Ok(_) => idle = 0,
                        // The connection went away under the send; the next
                        // turn finds the new one, or none.
                        Err(Pump::Closed) => {
                            generation = 0;
                            continue;
                        }
                        Err(Pump::Read(e)) => {
                            eprintln!("[enlist] {run}: cannot read the log: {e}");
                            continue;
                        }
                    }
                    if let Some(end) = end
                        && idle >= SETTLE_PASSES
                        && !finished
                        && tx
                            .blocking_send(FromRunner::Finished {
                                run: run.clone(),
                                end,
                            })
                            .is_ok()
                    {
                        finished = true;
                    }
                }
            });
        if let Err(e) = spawned {
            eprintln!("[enlist] {run_name}: cannot start the log thread: {e}");
        }
    }
}

/// Why a delivery pass stopped.
enum Pump {
    /// The connection's queue is gone.
    Closed,
    Read(std::io::Error),
}

/// Sends the log from `*sent` to its end in chunks. Returns the bytes sent.
/// A log that does not exist yet has nothing to send.
fn pump(
    wal: &Path,
    run: &str,
    tx: &tokio::sync::mpsc::Sender<FromRunner>,
    sent: &mut u64,
) -> Result<u64, Pump> {
    let mut file = match std::fs::File::open(wal) {
        Ok(file) => file,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(e) => return Err(Pump::Read(e)),
    };
    file.seek(SeekFrom::Start(*sent)).map_err(Pump::Read)?;
    let mut total = 0;
    let mut buf = vec![0u8; MAX_OUTPUT_CHUNK];
    loop {
        let n = file.read(&mut buf).map_err(Pump::Read)?;
        if n == 0 {
            return Ok(total);
        }
        let message = FromRunner::Output {
            run: run.to_owned(),
            offset: *sent,
            bytes: buf[..n].to_vec(),
        };
        if tx.blocking_send(message).is_err() {
            return Err(Pump::Closed);
        }
        *sent += n as u64;
        total += n as u64;
    }
}

/// Reads `code=` and `killed=` from a run's exit record.
fn read_exit_record(run_dir: &Path) -> Option<RunExit> {
    let text = std::fs::read_to_string(run_dir.join("exit")).ok()?;
    let mut exit = RunExit {
        code: None,
        killed: false,
    };
    let mut seen = false;
    for line in text.lines() {
        if let Some(code) = line.strip_prefix("code=") {
            exit.code = code.parse().ok();
            seen = true;
        } else if let Some(killed) = line.strip_prefix("killed=") {
            exit.killed = killed == "true";
        }
    }
    seen.then_some(exit)
}

#[cfg(test)]
mod tests {
    use zeughaus_link::enlist::{Checkout, UploadDir, WhenBusy};

    use super::*;

    fn scratch(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("zci-run-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_exit_record_is_read_back() {
        let dir = scratch("exit");
        assert_eq!(read_exit_record(&dir), None);
        std::fs::write(dir.join("exit"), "code=3\nkilled=false\nstarted=1\n").unwrap();
        assert_eq!(
            read_exit_record(&dir),
            Some(RunExit {
                code: Some(3),
                killed: false
            })
        );
        std::fs::write(dir.join("exit"), "code=none\nkilled=true\n").unwrap();
        assert_eq!(
            read_exit_record(&dir),
            Some(RunExit {
                code: None,
                killed: true
            })
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// The log goes out from the offset it is asked to start at, in pieces
    /// that join back to the file, and a second pass sends only what grew.
    #[test]
    fn the_log_is_sent_from_an_offset_and_only_what_grew() {
        let dir = scratch("pump");
        let wal = dir.join("wal");
        std::fs::write(&wal, b"hello world").unwrap();
        let (tx, mut rx) = tokio::sync::mpsc::channel(8);
        let mut sent = 6;
        assert_eq!(pump(&wal, "r", &tx, &mut sent).ok(), Some(5));
        assert_eq!(pump(&wal, "r", &tx, &mut sent).ok(), Some(0));
        {
            use std::io::Write as _;
            std::fs::OpenOptions::new()
                .append(true)
                .open(&wal)
                .unwrap()
                .write_all(b"!!")
                .unwrap();
        }
        assert_eq!(pump(&wal, "r", &tx, &mut sent).ok(), Some(2));
        let mut got = Vec::new();
        while let Ok(FromRunner::Output { offset, bytes, .. }) = rx.try_recv() {
            assert_eq!(offset, 6 + got.len() as u64);
            got.extend(bytes);
        }
        let _ = std::fs::remove_dir_all(&dir);
        assert_eq!(got, b"world!!");
    }

    #[test]
    fn a_log_that_does_not_exist_yet_has_nothing_to_send() {
        let (tx, _rx) = tokio::sync::mpsc::channel(1);
        let mut sent = 0;
        assert_eq!(
            pump(Path::new("/nonexistent/wal"), "r", &tx, &mut sent).ok(),
            Some(0)
        );
    }

    /// A job as the CI gives it to a runner of this host's kind: a Windows
    /// runner runs `.ps1` jobs only.
    fn assignment() -> Assignment {
        let (file, interpreter) = if cfg!(windows) {
            ("build.ps1", "powershell")
        } else {
            ("build.sh", "/bin/sh")
        };
        Assignment {
            run: "demo.7.build".to_owned(),
            repo: "demo".to_owned(),
            number: 7,
            job: "build".to_owned(),
            url: String::new(),
            fetch_user: String::new(),
            fetch_token: None,
            sha: "ab12".to_owned(),
            file: file.to_owned(),
            interpreter: interpreter.to_owned(),
            place: Place::Host,
            env: Vec::new(),
            secrets: Vec::new(),
            timeout_minutes: 1,
            when_busy: WhenBusy::Wait,
            inputs: Vec::new(),
            checkout: Checkout::Clean,
            upload: None,
        }
    }

    fn container(image: &str) -> Place {
        Place::Container {
            image: image.to_owned(),
            tag: format!("localhost/zci-demo-{image}:0123456789abcdef"),
        }
    }

    #[test]
    fn an_assignment_is_checked_before_its_names_become_paths() {
        let ok = assignment();
        assert!(validate(&ok).is_ok());
        for bad in [
            Assignment {
                repo: "../x".to_owned(),
                ..ok.clone()
            },
            Assignment {
                run: "demo.8.build".to_owned(),
                ..ok.clone()
            },
            Assignment {
                file: "../build.sh".to_owned(),
                ..ok.clone()
            },
            Assignment {
                sha: "main".to_owned(),
                ..ok.clone()
            },
            // The workspace is named after the place, so its image or
            // machine name is a path component.
            Assignment {
                place: container("../x"),
                ..ok.clone()
            },
            Assignment {
                place: Place::Machine {
                    name: "x/y".to_owned(),
                },
                ..ok.clone()
            },
        ] {
            assert!(validate(&bad).is_err(), "{bad:?}");
        }
        // A Windows runner has no containers.
        let in_container = Assignment {
            place: container("trixie"),
            ..ok
        };
        assert_eq!(validate(&in_container).is_ok(), cfg!(unix));
    }

    /// The names of a manual run's checkouts become directories below its
    /// workspace and their commits arguments of git: nothing of them is used
    /// before it is checked.
    #[test]
    fn a_manual_run_is_checked_before_its_trees_become_paths() {
        let dir = |name: &str, sha: &str| UploadDir {
            dir: name.to_owned(),
            sha: sha.to_owned(),
        };
        let ok = Assignment {
            run: "demo.9.check".to_owned(),
            number: 9,
            job: "check".to_owned(),
            sha: "a".repeat(40),
            upload: Some(Upload {
                dirs: vec![dir("demo", &"a".repeat(40)), dir("weida", &"b".repeat(40))],
            }),
            ..assignment()
        };
        assert!(validate(&ok).is_ok());
        let in_container = Assignment {
            place: container("trixie"),
            ..ok.clone()
        };
        assert_eq!(validate(&in_container).is_ok(), cfg!(unix));

        let with = |dirs: Vec<UploadDir>| Assignment {
            upload: Some(Upload { dirs }),
            ..ok.clone()
        };
        for bad in [
            with(Vec::new()),
            with(
                (0..=MAX_MANUAL_DIRS)
                    .map(|i| dir(&format!("d{i}"), "ab12"))
                    .collect(),
            ),
            with(vec![dir("../weida", "ab12")]),
            with(vec![dir("a/b", "ab12")]),
            with(vec![dir("demo", "main")]),
            with(vec![dir("demo", "")]),
            // A machine's workspace is one bundle, copied into the guest.
            Assignment {
                place: Place::Machine {
                    name: "win11".to_owned(),
                },
                ..ok.clone()
            },
        ] {
            assert!(validate(&bad).is_err(), "{bad:?}");
        }
    }

    /// `CI_SIBLINGS` is a directory of the workspace, as the job sees the
    /// workspace: the host's path, or `/work` in a container.
    #[test]
    fn the_siblings_are_a_directory_of_the_workspace() {
        let workspace = Path::new("work").join("demo").join("host");
        assert_eq!(
            Path::new(&siblings_var(&Place::Host, &workspace)),
            workspace.join(".zci-siblings")
        );
        assert_eq!(
            siblings_var(&container("trixie"), &workspace),
            "/work/.zci-siblings"
        );
    }
}
