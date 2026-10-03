//! Whether the workstation is in use: GPU utilisation over a window.
//!
//! A game (or a local model answering) keeps the GPU busy; a build that
//! starts then takes cores and IO from it. Busy is decided with hysteresis --
//! a short window to become busy, a long one to become free -- so a loading
//! screen does not let a build start in the middle of a session.

use std::collections::VecDeque;
use std::process::Command;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use super::config::BusyConfig;

/// How often `nvidia-smi` is asked.
const INTERVAL: Duration = Duration::from_secs(10);

/// The shared verdict, read every scheduler tick.
pub struct Busy {
    busy: AtomicBool,
}

impl Busy {
    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }
}

/// Starts the sampling thread. Without a working `nvidia-smi` the machine
/// is never busy, which is said once.
pub fn start(config: BusyConfig) -> Arc<Busy> {
    let busy = Arc::new(Busy {
        busy: AtomicBool::new(false),
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
                let was = busy.is_busy();
                let is = decide(&values, config, was);
                if is != was {
                    busy.busy.store(is, Ordering::SeqCst);
                    eprintln!("[ci] machine {}", if is { "busy" } else { "free" });
                }
            }
            Err(e) => {
                if !warned {
                    warned = true;
                    eprintln!("[ci] no GPU utilisation ({e}); the machine never counts as busy");
                }
                if busy.is_busy() {
                    busy.busy.store(false, Ordering::SeqCst);
                    eprintln!("[ci] machine free");
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
