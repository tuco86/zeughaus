//! The runners enlisted right now, and which of them each assigned run is
//! on. Shared by the scheduler (sends work), the `/enlist` service (adds and
//! removes connections, checks output against assignments) and `/ci`
//! (shows the runners to editors).

use std::collections::{BTreeMap, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc::UnboundedSender;
use zeughaus_link::MachineState;
use zeughaus_link::enlist::{Offer, ToRunner};

/// One enlisted runner's connection.
pub struct Connected {
    /// The `/enlist` exchange's writer; dropping it ends that exchange.
    pub tx: UnboundedSender<ToRunner>,
    pub offer: Offer,
    /// The runner's busy state, once it reported one.
    pub busy: Option<MachineState>,
    /// Machine name -> status, as the runner last reported them.
    pub machines: BTreeMap<String, String>,
    /// When it connected, seconds since the epoch.
    pub since: u64,
}

/// What `/ci` and the scheduler read about a connected runner.
#[derive(Debug, Clone)]
pub struct RunnerView {
    pub offer: Offer,
    pub busy: Option<MachineState>,
    pub machines: BTreeMap<String, String>,
    pub since: u64,
}

struct Entry {
    id: u64,
    connected: Connected,
}

#[derive(Default)]
pub struct Runners {
    connected: Mutex<BTreeMap<String, Entry>>,
    /// Run id -> runner name, for every run the scheduler assigned and has
    /// not forgotten.
    assigned: Mutex<HashMap<String, String>>,
    next_id: AtomicU64,
}

impl Runners {
    pub fn new() -> Runners {
        Runners::default()
    }

    /// Adds a connection, replacing an older one of the same name (whose
    /// exchange ends once its sender is dropped here). Returns the id that
    /// [`Runners::remove`] needs.
    pub fn insert(&self, name: &str, connected: Connected) -> u64 {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        lock(&self.connected).insert(name.to_owned(), Entry { id, connected });
        id
    }

    /// Removes the entry of `name` only if it is still connection `id`: a
    /// replaced connection that ends late must not remove its successor.
    pub fn remove(&self, name: &str, id: u64) -> bool {
        let mut connected = lock(&self.connected);
        if connected.get(name).is_some_and(|e| e.id == id) {
            connected.remove(name);
            true
        } else {
            false
        }
    }

    pub fn is_connected(&self, name: &str) -> bool {
        lock(&self.connected).contains_key(name)
    }

    /// Queues `message` for runner `name`; false when it is not connected
    /// and the message was dropped.
    pub fn send(&self, name: &str, message: ToRunner) -> bool {
        lock(&self.connected)
            .get(name)
            .is_some_and(|e| e.connected.tx.send(message).is_ok())
    }

    pub fn view(&self, name: &str) -> Option<RunnerView> {
        lock(&self.connected).get(name).map(|e| view(&e.connected))
    }

    pub fn views(&self) -> BTreeMap<String, RunnerView> {
        lock(&self.connected)
            .iter()
            .map(|(name, e)| (name.clone(), view(&e.connected)))
            .collect()
    }

    pub fn set_busy(&self, name: &str, state: MachineState) {
        if let Some(e) = lock(&self.connected).get_mut(name) {
            e.connected.busy = Some(state);
        }
    }

    pub fn set_machine(&self, name: &str, machine: &str, status: &str) {
        if let Some(e) = lock(&self.connected).get_mut(name) {
            e.connected
                .machines
                .insert(machine.to_owned(), status.to_owned());
        }
    }

    pub fn assign(&self, run: &str, runner: &str) {
        lock(&self.assigned).insert(run.to_owned(), runner.to_owned());
    }

    pub fn unassign(&self, run: &str) {
        lock(&self.assigned).remove(run);
    }

    pub fn assigned_to(&self, run: &str) -> Option<String> {
        lock(&self.assigned).get(run).cloned()
    }
}

fn view(connected: &Connected) -> RunnerView {
    RunnerView {
        offer: connected.offer.clone(),
        busy: connected.busy,
        machines: connected.machines.clone(),
        since: connected.since,
    }
}

/// A poisoned lock still holds consistent maps: every critical section is
/// a single insert, remove or read.
fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}
