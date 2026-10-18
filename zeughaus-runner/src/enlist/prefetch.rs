//! Early fetch: the artifact set of a job that ended, fetched by the runner of
//! a job that is still waiting for other needs.
//!
//! The CI sends `Prefetch` when a job that left a set ends and a job that
//! needs it is pending on this runner. The set is downloaded into
//! `ci/prefetch/<consumer>/<need>` on a thread of its own and reported like any
//! other transfer. When the job is assigned, its staging moves the set into
//! `inputs/` and waits only for what is still on its way. A failed fetch costs
//! nothing: the job fetches again, as it would have.
//!
//! What no job takes goes with `DropPrefetch`, with the run when the CI forgets
//! it, when its staging fails, or at the next start of this process, which
//! empties the directory: it forgets every fetch it had going.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex};

use super::run::safe_run;
use super::{Executor, ci_dir, cleanup, lock, stage};
use crate::names::valid_name;

/// Where fetched sets wait for their job: a directory per consumer run and
/// one per need in it.
pub(super) fn prefetch_dir(state_dir: &Path) -> PathBuf {
    ci_dir(state_dir).join("prefetch")
}

/// One early fetch: how it ended, once it did.
pub(super) struct Prefetch {
    result: Mutex<Option<Result<(), String>>>,
    done: Condvar,
}

impl Prefetch {
    fn new() -> Prefetch {
        Prefetch {
            result: Mutex::new(None),
            done: Condvar::new(),
        }
    }

    /// Blocks until the fetch ended.
    fn wait(&self) -> Result<(), String> {
        let mut result = lock(&self.result);
        loop {
            if let Some(outcome) = result.as_ref() {
                return outcome.clone();
            }
            result = self.done.wait(result).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn finish(&self, outcome: Result<(), String>) {
        *lock(&self.result) = Some(outcome);
        self.done.notify_all();
    }
}

/// The early fetches nobody has claimed yet, by consumer run and need.
#[derive(Default)]
pub(super) struct Prefetches(Mutex<HashMap<(String, String), Arc<Prefetch>>>);

impl Prefetches {
    /// Registers a fetch of `need` for `consumer`; `None` when one is known
    /// already.
    fn begin(&self, consumer: &str, need: &str) -> Option<Arc<Prefetch>> {
        let mut table = lock(&self.0);
        let key = (consumer.to_owned(), need.to_owned());
        if table.contains_key(&key) {
            return None;
        }
        let early = Arc::new(Prefetch::new());
        table.insert(key, Arc::clone(&early));
        Some(early)
    }

    /// Takes the fetch out of the table: each is claimed once.
    fn claim(&self, consumer: &str, need: &str) -> Option<Arc<Prefetch>> {
        lock(&self.0).remove(&(consumer.to_owned(), need.to_owned()))
    }

    /// Takes every fetch for `consumer` out of the table.
    fn take_all(&self, consumer: &str) -> Vec<Arc<Prefetch>> {
        let mut table = lock(&self.0);
        let keys: Vec<(String, String)> = table
            .keys()
            .filter(|(run, _)| run == consumer)
            .cloned()
            .collect();
        keys.iter().filter_map(|key| table.remove(key)).collect()
    }
}

impl Executor {
    /// Starts fetching the set of `run` as the input `need` of the job
    /// `consumer`, which runs here and is not assigned yet. Returns at once;
    /// a fetch of the same input that is known already is ignored.
    pub fn prefetch(self: &Arc<Self>, run: String, consumer: String, need: String) {
        if !safe_run(&run) || !safe_run(&consumer) || !valid_name(&need) {
            eprintln!("[enlist] {run:?} for {consumer:?}/{need:?}: not run ids and a job name");
            return;
        }
        let Some(early) = self.prefetches.begin(&consumer, &need) else {
            eprintln!("[enlist] {consumer}: {need} is fetched early already; ignored");
            return;
        };
        let exec = Arc::clone(self);
        let fetching = Arc::clone(&early);
        let (job, input) = (consumer.clone(), need.clone());
        let spawned = std::thread::Builder::new()
            .name("zeughaus-enlist-prefetch".into())
            .spawn(move || exec.fetch_early(&run, &job, &input, &fetching));
        if let Err(e) = spawned {
            eprintln!("[enlist] cannot start the prefetch thread: {e}");
            // A staging that claimed the fetch meanwhile must not wait for it.
            early.finish(Err(format!("cannot start a thread: {e}")));
            self.prefetches.claim(&consumer, &need);
        }
    }

    /// The body of the thread of [`Executor::prefetch`].
    fn fetch_early(self: &Arc<Self>, run: &str, consumer: &str, need: &str, early: &Prefetch) {
        let dest = prefetch_dir(&self.state_dir).join(consumer).join(need);
        // The set is exactly what the tar holds: nothing a fetch that died
        // halfway left may be in it.
        let fetched = cleanup::remove_tree(&dest)
            .map_err(|e| format!("cannot remove {}: {e}", dest.display()))
            .and_then(|()| self.fetch_input(consumer, need, run, &dest));
        match &fetched {
            Ok(bytes) => eprintln!("[enlist] {consumer}: {need} fetched early, {bytes} bytes"),
            Err(e) => eprintln!("[enlist] {consumer}: {need} not fetched early: {e}"),
        }
        early.finish(fetched.map(|_| ()));
    }

    /// Moves the set that an early fetch of `need` for `consumer` left into
    /// `dest`, after waiting for the fetch if it is still going. False when
    /// there was none, or it failed: the caller fetches the set itself.
    pub(super) fn claim_prefetch(
        &self,
        consumer: &str,
        need: &str,
        dest: &Path,
    ) -> Result<bool, String> {
        let Some(early) = self.prefetches.claim(consumer, need) else {
            return Ok(false);
        };
        // The thread that fetched it said why it failed.
        if early.wait().is_err() {
            eprintln!("[enlist] {consumer}: fetching {need} again");
            return Ok(false);
        }
        let from = prefetch_dir(&self.state_dir).join(consumer).join(need);
        if std::fs::rename(&from, dest).is_ok() {
            return Ok(true);
        }
        // Another file system than the run's: link the files, then drop them
        // from here.
        stage::link_tree(&from, dest).map_err(|e| format!("cannot stage inputs of {need}: {e}"))?;
        if let Err(e) = cleanup::remove_tree(&from) {
            eprintln!("[enlist] cannot remove {}: {e}", from.display());
        }
        Ok(true)
    }

    /// Forgets the early fetches for `consumer` and deletes what they fetched:
    /// the job will not stage them. Returns at once; a fetch that is still
    /// going is waited for on a thread of its own, since its directory is what
    /// is to go.
    pub fn drop_prefetch(&self, consumer: &str) {
        if !safe_run(consumer) {
            eprintln!("[enlist] {consumer:?}: not a run id; early fetches not dropped");
            return;
        }
        let going = self.prefetches.take_all(consumer);
        let dir = prefetch_dir(&self.state_dir).join(consumer);
        if going.is_empty() && !dir.exists() {
            return;
        }
        let consumer = consumer.to_owned();
        let spawned = std::thread::Builder::new()
            .name("zeughaus-enlist-prefetch-drop".into())
            .spawn(move || {
                for early in &going {
                    let _ = early.wait();
                }
                if let Err(e) = cleanup::remove_tree(&dir) {
                    eprintln!("[enlist] {consumer}: cannot drop the early fetches: {e}");
                }
            });
        if let Err(e) = spawned {
            // The next start of this process empties the directory.
            eprintln!("[enlist] cannot start the prefetch drop thread: {e}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_waiter_gets_the_outcome_once_the_fetch_ended() {
        let early = Arc::new(Prefetch::new());
        let waiters: Vec<_> = (0..2)
            .map(|_| {
                let early = Arc::clone(&early);
                std::thread::spawn(move || early.wait())
            })
            .collect();
        // Nothing has ended: both are blocked.
        std::thread::sleep(std::time::Duration::from_millis(50));
        assert!(waiters.iter().all(|waiter| !waiter.is_finished()));

        early.finish(Err("the CI is gone".to_owned()));
        for waiter in waiters {
            assert_eq!(waiter.join().unwrap(), Err("the CI is gone".to_owned()));
        }
        // A late waiter does not block either.
        assert_eq!(early.wait(), Err("the CI is gone".to_owned()));
    }

    #[test]
    fn a_fetch_is_known_once_and_claimed_once() {
        let table = Prefetches::default();
        assert!(table.begin("r.2.c", "a").is_some());
        assert!(table.begin("r.2.c", "a").is_none(), "already known");
        assert!(
            table.begin("r.2.c", "b").is_some(),
            "another need of the job"
        );
        assert!(table.begin("r.3.c", "a").is_some(), "another job");

        assert!(table.claim("r.2.c", "a").is_some());
        assert!(table.claim("r.2.c", "a").is_none(), "claimed once");
        // The input can be fetched again once it was claimed.
        assert!(table.begin("r.2.c", "a").is_some());
    }

    #[test]
    fn dropping_a_job_takes_its_fetches_and_no_other() {
        let table = Prefetches::default();
        for (consumer, need) in [("r.2.c", "a"), ("r.2.c", "b"), ("r.3.c", "a")] {
            table.begin(consumer, need).unwrap();
        }
        // One of them was claimed by the staging already.
        table.claim("r.2.c", "b").unwrap();

        assert_eq!(table.take_all("r.2.c").len(), 1);
        assert!(table.take_all("r.2.c").is_empty());
        assert!(
            table.claim("r.3.c", "a").is_some(),
            "another job keeps its fetch"
        );
    }
}
