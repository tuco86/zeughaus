//! This runner's own clock: once a second it lets idle machines stop, freezes
//! or thaws the runs that follow the busy measurement, ends machine jobs
//! that ran too long and tells the CI what changed.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use zeughaus_link::MachineState;
use zeughaus_link::enlist::{FromRunner, Place, WhenBusy};

use super::stage::Engine;
use super::{Executor, RunCtl, lock, stage};

const TICK: Duration = Duration::from_secs(1);

/// What the CI was last told on one connection.
#[derive(Default)]
struct Told {
    generation: u64,
    busy: Option<MachineState>,
    machines: BTreeMap<String, &'static str>,
}

/// Starts the clock thread.
pub fn spawn(exec: &Arc<Executor>) {
    let exec = Arc::clone(exec);
    let spawned = std::thread::Builder::new()
        .name("zeughaus-enlist-clock".into())
        .spawn(move || {
            let mut last = Instant::now();
            let mut told = Told::default();
            loop {
                std::thread::sleep(TICK);
                let elapsed = last.elapsed();
                last = Instant::now();
                for vm in exec.machines.values() {
                    vm.tick();
                }
                exec.apply_busy();
                exec.machine_timeouts(elapsed);
                exec.announce(&mut told);
            }
        });
    if let Err(e) = spawned {
        eprintln!("[enlist] no clock thread: {e}");
    }
}

/// Resumes every container and VM a previous process may have frozen.
pub fn thaw_everything(exec: &Executor) {
    if exec.containers
        && Engine::host() == Engine::Podman
        && let Ok(output) = std::process::Command::new("podman")
            .args([
                "ps",
                "--filter",
                "status=paused",
                "--filter",
                "name=^zci-",
                "--format",
                "{{.Names}}",
            ])
            .stderr(std::process::Stdio::null())
            .output()
    {
        for name in String::from_utf8_lossy(&output.stdout).lines() {
            if let Err(e) = stage::engine(Engine::Podman, &["unpause", name]) {
                eprintln!("[enlist] cannot unpause {name}: {e}");
            }
        }
    }
    for vm in exec.machines.values() {
        vm.thaw_if_running();
    }
}

impl Executor {
    fn running(&self) -> Vec<Arc<RunCtl>> {
        lock(&self.runs)
            .values()
            .filter(|ctl| ctl.running_in().is_some())
            .cloned()
            .collect()
    }

    /// Freezes the runs that asked to while the machine is busy, and thaws
    /// them when it is free again. A host job has nothing to pause: it only
    /// waits for the machine before it starts, which is the CI's decision.
    fn apply_busy(&self) {
        let Some(busy) = &self.busy else {
            return;
        };
        let is_busy = busy.is_busy();
        for ctl in self.running() {
            let a = &ctl.assignment;
            if a.when_busy != WhenBusy::Freeze
                || matches!(a.place, Place::Host)
                || ctl.frozen.load(Ordering::SeqCst) == is_busy
            {
                continue;
            }
            match self.freeze(&ctl, is_busy) {
                Ok(()) => {
                    ctl.frozen.store(is_busy, Ordering::SeqCst);
                    ctl.freeze_warned.store(false, Ordering::SeqCst);
                    eprintln!(
                        "[enlist] {}: {}",
                        a.run,
                        if is_busy { "frozen" } else { "thawed" }
                    );
                    self.send(FromRunner::Frozen {
                        run: a.run.clone(),
                        frozen: is_busy,
                    });
                }
                Err(e) => {
                    if !ctl.freeze_warned.swap(true, Ordering::SeqCst) {
                        eprintln!(
                            "[enlist] {}: cannot {}: {e}",
                            a.run,
                            if is_busy { "freeze" } else { "thaw" }
                        );
                    }
                }
            }
        }
    }

    fn freeze(&self, ctl: &RunCtl, freeze: bool) -> Result<(), String> {
        let a = &ctl.assignment;
        match &a.place {
            Place::Machine { name } => self
                .machines
                .get(name)
                .ok_or_else(|| format!("no machine {name}"))?
                .freeze(freeze),
            Place::Container { .. } => match Engine::host() {
                Engine::Podman => stage::engine(
                    Engine::Podman,
                    &[
                        if freeze { "pause" } else { "unpause" },
                        &format!("zci-{}", a.run),
                    ],
                ),
                Engine::Apple => Err("apple container cannot pause a container".to_owned()),
            },
            Place::Host => Err("a host job cannot be frozen".to_owned()),
        }
    }

    /// The timeout of a machine job, whose clock runs only while it is not
    /// frozen. A host or container job times itself out inside `inner.sh`.
    fn machine_timeouts(&self, elapsed: Duration) {
        for ctl in self.running() {
            let a = &ctl.assignment;
            if !matches!(a.place, Place::Machine { .. }) || ctl.frozen.load(Ordering::SeqCst) {
                continue;
            }
            let Some(run_dir) = ctl.running_in() else {
                continue;
            };
            let spent = ctl
                .active_millis
                .fetch_add(elapsed.as_millis() as u64, Ordering::SeqCst)
                + elapsed.as_millis() as u64;
            let limit = u64::from(a.timeout_minutes) * 60_000;
            if spent < limit || run_dir.join("timeout").exists() {
                continue;
            }
            let _ = std::fs::write(run_dir.join("timeout"), b"");
            // Machines exist on unix runners only.
            #[cfg(unix)]
            if let Some(pid) = std::fs::read_to_string(run_dir.join("ssh.pid"))
                .ok()
                .and_then(|text| text.trim().parse::<libc::pid_t>().ok())
                .filter(|pid| *pid > 0)
            {
                // SAFETY: a plain signal to the pid the launcher wrote.
                unsafe { libc::kill(pid, libc::SIGTERM) };
            }
            eprintln!("[enlist] {}: timed out", a.run);
        }
    }

    /// Tells the CI the busy state and the machines' status when they
    /// changed, and everything again on a connection it has not heard them
    /// on.
    fn announce(&self, told: &mut Told) {
        let Some(generation) = self.generation() else {
            return;
        };
        let fresh = told.generation != generation;
        if fresh {
            *told = Told {
                generation,
                ..Told::default()
            };
        }
        if let Some(busy) = &self.busy {
            let state = busy.state();
            if told.busy != Some(state) && self.send(FromRunner::Busy { state }) {
                told.busy = Some(state);
            }
        }
        for (name, vm) in &self.machines {
            let status = vm.status();
            if told.machines.get(name) != Some(&status)
                && self.send(FromRunner::Machine {
                    name: name.clone(),
                    status: status.to_owned(),
                })
            {
                told.machines.insert(name.clone(), status);
            }
        }
        if fresh {
            for ctl in self.running() {
                if ctl.frozen.load(Ordering::SeqCst) {
                    self.send(FromRunner::Frozen {
                        run: ctl.assignment.run.clone(),
                        frozen: true,
                    });
                }
            }
        }
    }
}
