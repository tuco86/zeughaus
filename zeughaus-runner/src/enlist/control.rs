//! This runner's own clock: once a second it lets idle machines stop, ends
//! machine jobs that ran too long and tells the CI what changed.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use zeughaus_link::enlist::{FromRunner, Place};

use super::{Executor, RunCtl, lock};

const TICK: Duration = Duration::from_secs(1);

/// What the CI was last told on one connection.
#[derive(Default)]
struct Told {
    generation: u64,
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
                exec.machine_timeouts(elapsed);
                exec.announce(&mut told);
            }
        });
    if let Err(e) = spawned {
        eprintln!("[enlist] no clock thread: {e}");
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

    /// The timeout of a machine job. A host or container job times itself
    /// out inside `inner.sh`.
    fn machine_timeouts(&self, elapsed: Duration) {
        for ctl in self.running() {
            let a = &ctl.assignment;
            if !matches!(a.place, Place::Machine { .. }) {
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

    /// Tells the CI the machines' status when it changed, and again on a
    /// connection it has not heard it on.
    fn announce(&self, told: &mut Told) {
        let Some(generation) = self.generation() else {
            return;
        };
        if told.generation != generation {
            *told = Told {
                generation,
                ..Told::default()
            };
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
    }
}
