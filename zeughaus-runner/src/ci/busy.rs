//! Whether the workstation is in use: GPU utilisation over a window, unless
//! someone overrides it.
//!
//! A game (or a local model answering) keeps the GPU busy; a build that
//! starts then takes cores and IO from it. Busy is decided with hysteresis --
//! a short window to become busy, a long one to become free -- so a loading
//! screen does not let a build start in the middle of a session.
//!
//! The measurement only guesses. An editor's toggle sets a [`BusyMode`]:
//! `auto` follows the measurement, `busy` and `free` override it. The mode
//! is kept in `<state-dir>/ci/busy-mode`, so a restart does not undo it.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use weida::{Replier, TransferMeta};
use zeughaus_link::{BusyMode, BusyRequest, MAX_BUSY_BYTES, MachineState};

use super::config::BusyConfig;

/// How often `nvidia-smi` is asked.
const INTERVAL: Duration = Duration::from_secs(10);

/// The shared verdict, read every scheduler tick.
pub struct Busy {
    /// What the GPU says, kept up to date whatever the mode.
    measured: AtomicBool,
    mode: Mutex<BusyMode>,
    /// Where the mode is kept.
    file: PathBuf,
}

impl Busy {
    /// The machine as the scheduler acts on it.
    pub fn is_busy(&self) -> bool {
        self.state().busy
    }

    pub fn state(&self) -> MachineState {
        let mode = *self.mode.lock().unwrap_or_else(|e| e.into_inner());
        let busy = match mode {
            BusyMode::Auto => self.measured.load(Ordering::SeqCst),
            BusyMode::Busy => true,
            BusyMode::Free => false,
        };
        MachineState { mode, busy }
    }

    /// Sets the mode and keeps it on disk. A mode that cannot be written is
    /// refused rather than applied: the next restart would quietly undo it.
    pub fn set_mode(&self, mode: BusyMode) -> Result<MachineState, String> {
        let mut current = self.mode.lock().unwrap_or_else(|e| e.into_inner());
        super::write_atomic(&self.file, format!("{}\n", mode.as_str()).as_bytes())?;
        *current = mode;
        drop(current);
        let state = self.state();
        eprintln!(
            "[ci] busy mode {}: machine {}",
            mode.as_str(),
            if state.busy { "busy" } else { "free" }
        );
        Ok(state)
    }
}

/// The mode kept under `state_dir`; `auto` when there is none. A file that
/// says something else is reported and read as `auto`.
fn load_mode(file: &Path) -> BusyMode {
    match std::fs::read_to_string(file) {
        Ok(text) => BusyMode::parse(text.trim()).unwrap_or_else(|| {
            eprintln!(
                "[ci] {}: {:?} is not auto, busy or free; using auto",
                file.display(),
                text.trim()
            );
            BusyMode::Auto
        }),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => BusyMode::Auto,
        Err(e) => {
            eprintln!("[ci] cannot read {}: {e}; using auto", file.display());
            BusyMode::Auto
        }
    }
}

/// Starts the sampling thread. Without a working `nvidia-smi` the
/// measurement never says busy, which is said once.
pub fn start(config: BusyConfig, state_dir: &Path) -> Arc<Busy> {
    let file = super::ci_dir(state_dir).join("busy-mode");
    let mode = load_mode(&file);
    if mode != BusyMode::Auto {
        eprintln!("[ci] busy mode {}", mode.as_str());
    }
    let busy = Arc::new(Busy {
        measured: AtomicBool::new(false),
        mode: Mutex::new(mode),
        file,
    });
    let shared = Arc::clone(&busy);
    let spawned = std::thread::Builder::new()
        .name("zeughaus-ci-busy".into())
        .spawn(move || sample_forever(&config, &shared));
    if let Err(e) = spawned {
        eprintln!("[ci] no busy detection: {e}");
    }
    busy
}

/// Answers `/busy`: sets the mode and replies with the state after it. A
/// refused mode is answered with the state as it stays.
pub async fn serve(replier: Replier, busy: Arc<Busy>) {
    loop {
        let mut request = match replier.accept().await {
            Ok(request) => request,
            Err(e) => {
                eprintln!("[ci] stopped serving busy: {e}");
                return;
            }
        };
        let payload = match request.take_body().collect(MAX_BUSY_BYTES).await {
            Ok(payload) => payload,
            Err(e) => {
                eprintln!("[ci] unreadable busy request: {e}");
                continue;
            }
        };
        let Some(asked) = BusyRequest::decode(&payload) else {
            eprintln!(
                "[ci] refused a malformed busy request ({} bytes)",
                payload.len()
            );
            continue;
        };
        let state = busy.set_mode(asked.mode).unwrap_or_else(|e| {
            eprintln!("[ci] busy mode {}: {e}", asked.mode.as_str());
            busy.state()
        });
        let mut reply = match request.reply(TransferMeta::default()).await {
            Ok(reply) => reply,
            Err(e) => {
                eprintln!("[ci] cannot reply to a busy request: {e}");
                continue;
            }
        };
        if let Err(e) = reply.write_all(&state.encode()).await {
            eprintln!("[ci] cannot write a busy reply: {e}");
            continue;
        }
        if let Err(e) = reply.finish() {
            eprintln!("[ci] cannot finish a busy reply: {e}");
        }
    }
}

fn sample_forever(config: &BusyConfig, busy: &Busy) {
    let mut samples: VecDeque<(Instant, u32)> = VecDeque::new();
    let keep = Duration::from_secs(config.free_after_seconds.max(config.busy_after_seconds));
    let mut warned = false;
    loop {
        match gpu_utilisation() {
            Ok(percent) => {
                let now = Instant::now();
                samples.push_back((now, percent));
                while samples
                    .front()
                    .is_some_and(|(at, _)| now.duration_since(*at) > keep)
                {
                    samples.pop_front();
                }
                let values: Vec<(Duration, u32)> = samples
                    .iter()
                    .map(|(at, percent)| (now.duration_since(*at), *percent))
                    .collect();
                let was = busy.measured.load(Ordering::SeqCst);
                let is = decide(&values, config, was);
                if is != was {
                    busy.measured.store(is, Ordering::SeqCst);
                    eprintln!("[ci] GPU {}", if is { "busy" } else { "free" });
                }
            }
            Err(e) => {
                if !warned {
                    warned = true;
                    eprintln!("[ci] no GPU utilisation ({e}); the GPU never counts as busy");
                }
                if busy.measured.load(Ordering::SeqCst) {
                    busy.measured.store(false, Ordering::SeqCst);
                    eprintln!("[ci] GPU free");
                }
            }
        }
        std::thread::sleep(INTERVAL);
    }
}

/// The highest utilisation across the GPUs.
fn gpu_utilisation() -> Result<u32, String> {
    let output = Command::new("nvidia-smi")
        .args([
            "--query-gpu=utilization.gpu",
            "--format=csv,noheader,nounits",
        ])
        .output()
        .map_err(|e| format!("cannot run nvidia-smi: {e}"))?;
    if !output.status.success() {
        return Err(format!("nvidia-smi exited {}", output.status));
    }
    String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter_map(|line| line.trim().parse::<u32>().ok())
        .max()
        .ok_or_else(|| "nvidia-smi reported no GPU".to_owned())
}

/// The verdict for `samples` (age, percent), newest last. A window counts
/// only once it is covered by samples: a single hot sample right after a
/// start is not a minute of gaming.
fn decide(samples: &[(Duration, u32)], config: &BusyConfig, was_busy: bool) -> bool {
    let window = if was_busy {
        config.free_after_seconds
    } else {
        config.busy_after_seconds
    };
    let window = Duration::from_secs(window);
    let in_window: Vec<u32> = samples
        .iter()
        .filter(|(age, _)| *age <= window)
        .map(|(_, percent)| *percent)
        .collect();
    let needed = (window.as_secs() / INTERVAL.as_secs()).max(1) as usize;
    if in_window.len() < needed {
        return was_busy;
    }
    let mean = in_window.iter().map(|p| u64::from(*p)).sum::<u64>() / in_window.len() as u64;
    mean >= u64::from(config.gpu_percent)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config() -> BusyConfig {
        BusyConfig {
            gpu_percent: 50,
            busy_after_seconds: 60,
            free_after_seconds: 300,
        }
    }

    fn series(percents: &[u32]) -> Vec<(Duration, u32)> {
        let n = percents.len() as u64;
        percents
            .iter()
            .enumerate()
            .map(|(i, p)| (INTERVAL * (n - 1 - i as u64) as u32, *p))
            .collect()
    }

    #[test]
    fn becomes_busy_only_once_the_short_window_is_covered() {
        assert!(!decide(&series(&[100; 3]), &config(), false));
        assert!(decide(&series(&[100; 6]), &config(), false));
    }

    #[test]
    fn stays_busy_until_the_long_window_is_quiet() {
        let mut quiet_minute = vec![100; 24];
        quiet_minute.extend([0; 6]);
        assert!(decide(&series(&quiet_minute), &config(), true));
        assert!(!decide(&series(&[0; 30]), &config(), true));
    }
}
