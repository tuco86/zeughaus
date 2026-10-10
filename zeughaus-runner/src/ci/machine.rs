//! The machines a CI job can run on: a VM the CI boots on demand and stops
//! after an idle timeout, and a unix host (a Mac, say) that is always on and
//! reached over ssh.
//!
//! A VM is driven through its `vm.sh` (boot, stop) and its HMP monitor
//! socket (freeze, thaw). One job runs in it at a time; the scheduler
//! enforces that, and the lease count here is what keeps the idle stop away
//! while a job is booting or running. A unix host is only probed: it has no
//! power state to manage, and a job waits for it to answer.
//!
//! The busy measurement is this workstation's GPU, so it holds and freezes
//! only the jobs that run on the workstation (host, container, VM), never a
//! unix host's.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use super::config::{MachineConfig, UnixHostConfig, VmConfig};

/// How often an offline unix host is probed again while a job waits for it.
const HOST_RETRY: Duration = Duration::from_secs(30);

/// How long a failed run's debug shell keeps the VM up after it was opened.
const DEBUG_HOLD: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Power {
    Off,
    Booting,
    Ready,
    Stopping,
}

struct Inner {
    power: Power,
    leases: u32,
    last_release: Instant,
    /// Failed runs whose terminal may hold a shell in the guest.
    debug_runs: Vec<PathBuf>,
}

pub struct Vm {
    pub name: String,
    config: VmConfig,
    log: PathBuf,
    inner: Mutex<Inner>,
    changed: Condvar,
}

impl Vm {
    /// A machine whose QEMU is alive counts as up: a restarted runner finds
    /// the VM its predecessor booted.
    fn new(name: &str, config: VmConfig, state_dir: &Path) -> Arc<Vm> {
        let power = if qemu_alive(&config.dir) {
            Power::Ready
        } else {
            Power::Off
        };
        Arc::new(Vm {
            name: name.to_owned(),
            log: machine_log(state_dir, name),
            config,
            inner: Mutex::new(Inner {
                power,
                leases: 0,
                last_release: Instant::now(),
                debug_runs: Vec::new(),
            }),
            changed: Condvar::new(),
        })
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Takes a lease, booting and preparing the VM first if it is off.
    /// Blocks for as long as that takes; called from a job's own thread.
    fn acquire(&self) -> Result<(), String> {
        let mut inner = self.lock();
        loop {
            match inner.power {
                Power::Ready => {
                    inner.leases += 1;
                    return Ok(());
                }
                Power::Off => break,
                Power::Booting | Power::Stopping => {
                    inner = self.changed.wait(inner).unwrap_or_else(|e| e.into_inner());
                }
            }
        }
        inner.power = Power::Booting;
        inner.leases += 1;
        drop(inner);

        eprintln!("[ci] machine {}: booting", self.name);
        let booted = self.boot();
        let mut inner = self.lock();
        match &booted {
            Ok(()) => {
                inner.power = Power::Ready;
                eprintln!("[ci] machine {}: ready", self.name);
            }
            Err(e) => {
                inner.leases -= 1;
                inner.last_release = Instant::now();
                eprintln!("[ci] machine {}: {e}", self.name);
                // A half-booted QEMU would make the next `vm.sh run` refuse.
                drop(inner);
                let _ = self.vm("stop");
                inner = self.lock();
                inner.power = Power::Off;
            }
        }
        drop(inner);
        self.changed.notify_all();
        booted.map_err(|_| {
            format!(
                "machine {} did not come up, see ci/machines/{}.log",
                self.name, self.name
            )
        })
    }

    /// A lease for a job a previous runner started in a VM that is still up.
    fn adopt(&self) {
        let mut inner = self.lock();
        inner.leases += 1;
        inner.power = Power::Ready;
    }

    fn release(&self) {
        let mut inner = self.lock();
        inner.leases = inner.leases.saturating_sub(1);
        inner.last_release = Instant::now();
    }

    /// A failed run whose terminal may open a shell in the guest.
    fn add_debug_run(&self, run_dir: PathBuf) {
        self.lock().debug_runs.push(run_dir);
    }

    /// Stops the VM once nothing has used it for `idle_minutes` and no
    /// debug shell is open in it. The stop runs on a thread of its own:
    /// an ACPI power-off takes up to two minutes.
    fn tick(self: &Arc<Self>) {
        let mut inner = self.lock();
        if inner.power == Power::Ready && !qemu_alive(&self.config.dir) {
            eprintln!("[ci] machine {}: QEMU is gone", self.name);
            inner.power = Power::Off;
            drop(inner);
            self.changed.notify_all();
            return;
        }
        inner.debug_runs.retain(|run| debug_shell_open(run));
        let idle = Duration::from_secs(self.config.idle_minutes * 60);
        if inner.power != Power::Ready
            || inner.leases > 0
            || !inner.debug_runs.is_empty()
            || inner.last_release.elapsed() < idle
        {
            return;
        }
        inner.power = Power::Stopping;
        drop(inner);
        let machine = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name(format!("zeughaus-ci-{}-stop", self.name))
            .spawn(move || {
                eprintln!("[ci] machine {}: idle, stopping", machine.name);
                if let Err(e) = machine.vm("stop") {
                    eprintln!("[ci] machine {}: stop: {e}", machine.name);
                }
                let mut inner = machine.lock();
                inner.power = if qemu_alive(&machine.config.dir) {
                    Power::Ready
                } else {
                    Power::Off
                };
                inner.last_release = Instant::now();
                drop(inner);
                machine.changed.notify_all();
            });
        if let Err(e) = spawned {
            eprintln!("[ci] machine {}: no stop thread: {e}", self.name);
            self.lock().power = Power::Ready;
        }
    }

    /// Pauses (`stop`) or resumes (`cont`) every vCPU through the HMP monitor.
    fn monitor(&self, command: &str) -> Result<(), String> {
        let path = self.config.dir.join("monitor.sock");
        let mut stream = UnixStream::connect(&path)
            .map_err(|e| format!("cannot connect {}: {e}", path.display()))?;
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .map_err(|e| e.to_string())?;
        stream
            .write_all(format!("{command}\n").as_bytes())
            .map_err(|e| format!("monitor: {e}"))?;
        // HMP answers with its prompt once the command ran; reading until
        // the second prompt (the first greets) keeps the connection open
        // that long. A timeout leaves it to QEMU, which still runs it.
        let mut seen = String::new();
        let mut buf = [0u8; 512];
        while seen.matches("(qemu)").count() < 2 {
            match stream.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => seen.push_str(&String::from_utf8_lossy(&buf[..n])),
            }
        }
        Ok(())
    }

    /// `vm.sh <command>` with this machine's settings, output appended to
    /// the machine's log.
    fn vm(&self, command: &str) -> Result<(), String> {
        let log = open_log(&self.log)?;
        let status = Command::new(&self.config.script)
            .arg(command)
            .env("VM_DIR", &self.config.dir)
            .env("SSH_HOST", &self.config.ssh_host)
            .env("CPUS", self.config.cpus.to_string())
            .env("MEM", &self.config.memory)
            .env("CACHE_DISK_SIZE", &self.config.cache_disk)
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log)
            .status()
            .map_err(|e| format!("cannot run {}: {e}", self.config.script.display()))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("vm.sh {command} exited {status}"))
        }
    }

    /// Boots the VM and runs `prepare.ps1` in it: the cache disk as W:,
    /// the per-boot cleanup, rustup. Shipped beside `vm.sh`.
    fn boot(&self) -> Result<(), String> {
        self.vm("run")?;
        let prepare = self
            .config
            .script
            .parent()
            .map(|dir| dir.join("prepare.ps1"))
            .ok_or("vm.sh has no directory")?;
        self.run_logged(
            Command::new("scp").arg("-q").arg(&prepare).arg(format!(
                "{}:/C:/Windows/Temp/zeughaus-prepare.ps1",
                self.config.ssh_host
            )),
            "scp prepare.ps1",
        )?;
        self.run_logged(
            Command::new("ssh").arg(&self.config.ssh_host).arg(
                r"powershell -NoProfile -ExecutionPolicy Bypass -File C:\Windows\Temp\zeughaus-prepare.ps1",
            ),
            "prepare.ps1",
        )
    }

    fn run_logged(&self, command: &mut Command, what: &str) -> Result<(), String> {
        let log = open_log(&self.log)?;
        let status = command
            .stdin(Stdio::null())
            .stdout(log.try_clone().map_err(|e| e.to_string())?)
            .stderr(log)
            .status()
            .map_err(|e| format!("{what}: {e}"))?;
        if status.success() {
            Ok(())
        } else {
            Err(format!("{what} exited {status}"))
        }
    }
}

/// A unix host reached over ssh, always on.
pub struct Host {
    name: String,
    config: UnixHostConfig,
    log: PathBuf,
}

impl Host {
    fn new(name: &str, config: UnixHostConfig, state_dir: &Path) -> Arc<Host> {
        Arc::new(Host {
            name: name.to_owned(),
            log: machine_log(state_dir, name),
            config,
        })
    }

    /// Whether the host answers ssh and its run directory is usable; the
    /// probe also prunes runs a killed launcher left behind.
    fn probe(&self) -> bool {
        let Ok(log) = open_log(&self.log) else {
            return false;
        };
        let Ok(err) = log.try_clone() else {
            return false;
        };
        Command::new("ssh")
            .args(["-o", "BatchMode=yes"])
            .arg(&self.config.ssh_host)
            .arg(super::launch::host_probe(&self.config.dir))
            .stdin(Stdio::null())
            .stdout(log)
            .stderr(err)
            .status()
            .is_ok_and(|status| status.success())
    }

    /// Returns once the host answers, or after `wait_minutes` without an
    /// answer. `waiting` is told once, at the first failed probe, so the
    /// pipeline can show why the job has not started. Blocks; called from a
    /// job's own thread.
    fn acquire(&self, waiting: &mut dyn FnMut(String)) -> Result<(), String> {
        if self.probe() {
            return Ok(());
        }
        eprintln!("[ci] machine {}: offline", self.name);
        waiting(format!("machine {} offline", self.name));
        let deadline = Instant::now() + Duration::from_secs(self.config.wait_minutes * 60);
        while Instant::now() < deadline {
            std::thread::sleep(HOST_RETRY.min(deadline.saturating_duration_since(Instant::now())));
            if self.probe() {
                eprintln!("[ci] machine {}: back", self.name);
                return Ok(());
            }
        }
        Err(format!(
            "machine {} offline for {} min",
            self.name, self.config.wait_minutes
        ))
    }
}

/// Why a job's machine did not take it.
pub enum StartError {
    /// The machine failed to come up: the job fails.
    Failed(String),
    /// The machine did not answer within its wait: the job is skipped.
    Offline(String),
}

/// A machine of either kind, as the scheduler holds it.
#[derive(Clone)]
pub enum Machine {
    Vm(Arc<Vm>),
    Host(Arc<Host>),
}

impl Machine {
    /// The machine `config` describes; see [`Vm::new`] for a VM found up.
    pub fn new(name: &str, config: &MachineConfig, state_dir: &Path) -> Machine {
        match config {
            MachineConfig::WindowsVm(vm) => Machine::Vm(Vm::new(name, vm.clone(), state_dir)),
            MachineConfig::UnixHost(host) => {
                Machine::Host(Host::new(name, host.clone(), state_dir))
            }
        }
    }

    /// Makes the machine ready for one job. A VM is booted if it is off; a
    /// unix host is waited for, and `waiting` gets the reason once.
    pub fn acquire(&self, waiting: &mut dyn FnMut(String)) -> Result<(), StartError> {
        match self {
            Machine::Vm(vm) => vm.acquire().map_err(StartError::Failed),
            Machine::Host(host) => host.acquire(waiting).map_err(StartError::Offline),
        }
    }

    /// A lease for a job a previous runner started on a VM that is still up.
    pub fn adopt(&self) {
        if let Machine::Vm(vm) = self {
            vm.adopt();
        }
    }

    pub fn release(&self) {
        if let Machine::Vm(vm) = self {
            vm.release();
        }
    }

    /// A failed run whose terminal may open a shell in the guest.
    pub fn add_debug_run(&self, run_dir: PathBuf) {
        if let Machine::Vm(vm) = self {
            vm.add_debug_run(run_dir);
        }
    }

    pub fn tick(&self) {
        if let Machine::Vm(vm) = self {
            vm.tick();
        }
    }

    /// Pauses or resumes a VM's vCPUs. A unix host is never frozen.
    pub fn freeze(&self, freeze: bool) -> Result<(), String> {
        match self {
            Machine::Vm(vm) => vm.monitor(if freeze { "stop" } else { "cont" }),
            Machine::Host(host) => Err(format!(
                "machine {} is a unix host and is never frozen",
                host.name
            )),
        }
    }

    /// Resumes a VM a previous process may have frozen.
    pub fn thaw_if_running(&self) {
        if let Machine::Vm(vm) = self
            && vm.config.dir.join("qemu.pid").exists()
        {
            let _ = vm.monitor("cont");
        }
    }

    /// Whether the workstation's busy measurement holds and freezes jobs on
    /// this machine: only if they share the workstation's GPU.
    pub fn follows_busy(&self) -> bool {
        matches!(self, Machine::Vm(_))
    }

    pub fn ssh_host(&self) -> &str {
        match self {
            Machine::Vm(vm) => &vm.config.ssh_host,
            Machine::Host(host) => &host.config.ssh_host,
        }
    }

    pub fn cpus(&self) -> u32 {
        match self {
            Machine::Vm(vm) => vm.config.cpus,
            Machine::Host(host) => host.config.cpus,
        }
    }

    /// The absolute directory on a unix host that holds its `runs/` and
    /// `work/`; a VM has none.
    pub fn unix_dir(&self) -> Option<&str> {
        match self {
            Machine::Vm(_) => None,
            Machine::Host(host) => Some(&host.config.dir),
        }
    }
}

/// `ci/machines/<name>.log`, where a machine's own commands write.
fn machine_log(state_dir: &Path, name: &str) -> PathBuf {
    super::ci_dir(state_dir)
        .join("machines")
        .join(format!("{name}.log"))
}

fn open_log(log: &Path) -> Result<std::fs::File, String> {
    if let Some(parent) = log.parent() {
        std::fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log)
        .map_err(|e| format!("cannot open {}: {e}", log.display()))
}

/// Whether `<dir>/qemu.pid` names a live process.
fn qemu_alive(dir: &Path) -> bool {
    let Some(pid) = std::fs::read_to_string(dir.join("qemu.pid"))
        .ok()
        .and_then(|text| text.trim().parse::<libc::pid_t>().ok())
    else {
        return false;
    };
    // SAFETY: signal 0 checks for existence and delivers nothing.
    pid > 0 && unsafe { libc::kill(pid, 0) } == 0
}

/// Whether a failed run's guest shell was opened within [`DEBUG_HOLD`] and
/// has not ended: the launcher removes the marker when it exits.
fn debug_shell_open(run_dir: &Path) -> bool {
    std::fs::metadata(run_dir.join("debug-shell"))
        .and_then(|m| m.modified())
        .is_ok_and(|at| {
            SystemTime::now()
                .duration_since(at)
                .is_ok_and(|age| age < DEBUG_HOLD)
        })
}
