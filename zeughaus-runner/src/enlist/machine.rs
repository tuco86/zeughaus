//! A machine behind this runner that a CI job can run on: a Windows VM booted
//! on demand and stopped after an idle timeout.
//!
//! A VM is driven through its `vm.sh` (boot, stop) and its HMP monitor socket
//! (freeze, thaw). One job runs in it at a time; the CI enforces that, and the
//! lease count here is what keeps the idle stop away while a job is booting
//! or running.

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime};

use super::config::VmConfig;

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
    pub fn new(name: &str, config: VmConfig, state_dir: &Path) -> Arc<Vm> {
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

    /// The power state as the CI's machine list shows it.
    pub fn status(&self) -> &'static str {
        match self.lock().power {
            Power::Off => "off",
            Power::Booting => "booting",
            Power::Ready => "ready",
            Power::Stopping => "stopping",
        }
    }

    pub fn ssh_host(&self) -> &str {
        &self.config.ssh_host
    }

    pub fn cpus(&self) -> u32 {
        self.config.cpus
    }

    /// Takes a lease, booting and preparing the VM first if it is off.
    /// Blocks for as long as that takes; called from a job's own thread.
    pub fn acquire(&self) -> Result<(), String> {
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

        eprintln!("[enlist] machine {}: booting", self.name);
        let booted = self.boot();
        let mut inner = self.lock();
        match &booted {
            Ok(()) => {
                inner.power = Power::Ready;
                eprintln!("[enlist] machine {}: ready", self.name);
            }
            Err(e) => {
                inner.leases -= 1;
                inner.last_release = Instant::now();
                eprintln!("[enlist] machine {}: {e}", self.name);
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
    pub fn adopt(&self) {
        let mut inner = self.lock();
        inner.leases += 1;
        inner.power = Power::Ready;
    }

    pub fn release(&self) {
        let mut inner = self.lock();
        inner.leases = inner.leases.saturating_sub(1);
        inner.last_release = Instant::now();
    }

    /// A failed run whose terminal may open a shell in the guest.
    pub fn add_debug_run(&self, run_dir: PathBuf) {
        self.lock().debug_runs.push(run_dir);
    }

    /// Stops the VM once nothing has used it for `idle_minutes` and no
    /// debug shell is open in it. The stop runs on a thread of its own:
    /// an ACPI power-off takes up to two minutes.
    pub fn tick(self: &Arc<Self>) {
        let mut inner = self.lock();
        if inner.power == Power::Ready && !qemu_alive(&self.config.dir) {
            eprintln!("[enlist] machine {}: QEMU is gone", self.name);
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
            .name(format!("zeughaus-enlist-{}-stop", self.name))
            .spawn(move || {
                eprintln!("[enlist] machine {}: idle, stopping", machine.name);
                if let Err(e) = machine.vm("stop") {
                    eprintln!("[enlist] machine {}: stop: {e}", machine.name);
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
            eprintln!("[enlist] machine {}: no stop thread: {e}", self.name);
            self.lock().power = Power::Ready;
        }
    }

    /// Pauses or resumes the vCPUs.
    pub fn freeze(&self, freeze: bool) -> Result<(), String> {
        self.monitor(if freeze { "stop" } else { "cont" })
    }

    /// Resumes a VM a previous process may have frozen.
    pub fn thaw_if_running(&self) {
        if self.config.dir.join("qemu.pid").exists() {
            let _ = self.monitor("cont");
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
