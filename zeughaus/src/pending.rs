//! Edits held back from the shared store until the typing stops.
//!
//! A setting is typed one character at a time, and a character that reached
//! the store on its own would be a complete parameter push: seven keystrokes
//! would mean seven store writes, seven re-derivations of everything the value
//! feeds, seven relation edges replaced -- and, for a `db.database` path,
//! seven SQLite files created on the runner, one per prefix of what the user
//! was still typing.
//!
//! This window and its nodes keep seeing every character; only the store
//! waits. Two things make that safe rather than merely quieter:
//!
//! * the value the store still holds is remembered per key on the first
//!   un-pushed edit, so a rename is detected against what was pushed rather
//!   than against the previous keystroke -- seven keystrokes are one rename;
//! * anything about to observe store state flushes first
//!   ([`PendingEdits::drain`]), so no reader can see a graph the store does
//!   not have.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use zeughaus_core::NodeId;

/// How long a node's settings stay quiet before they reach the store. Long
/// enough that ordinary typing never pushes mid-word, short enough that the
/// pause after a word is not noticeable.
pub const DEBOUNCE: Duration = Duration::from_millis(400);

/// What one node owes the store: the value each touched setting had there
/// before the edits started.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Owed {
    /// Setting key -> the value the store still holds for it.
    pub was: HashMap<String, String>,
}

#[derive(Debug)]
struct Entry {
    owed: Owed,
    last_edit: Instant,
}

/// Per-node settings edits that have not reached the store yet.
#[derive(Debug, Default)]
pub struct PendingEdits {
    nodes: HashMap<NodeId, Entry>,
}

impl PendingEdits {
    pub fn new() -> Self {
        Self {
            nodes: HashMap::new(),
        }
    }

    /// Records an edit of `key` on `node`. `was` is the value the store holds;
    /// it is kept only the first time this key is touched after a flush, which
    /// is what makes a run of keystrokes one change.
    pub fn touch(&mut self, node: NodeId, key: &str, was: &str, now: Instant) {
        let entry = self.nodes.entry(node).or_insert_with(|| Entry {
            owed: Owed {
                was: HashMap::new(),
            },
            last_edit: now,
        });
        entry.last_edit = now;
        entry
            .owed
            .was
            .entry(key.to_string())
            .or_insert_with(|| was.to_string());
    }

    /// Whether anything is waiting.
    pub fn is_empty(&self) -> bool {
        self.nodes.is_empty()
    }

    /// Takes the nodes whose last edit is at least `delay` old.
    ///
    /// Sorted by node id: two windows applying the same run of edits have to
    /// produce the same sequence of store calls.
    pub fn settled(&mut self, now: Instant, delay: Duration) -> Vec<(NodeId, Owed)> {
        let due: Vec<NodeId> = self
            .nodes
            .iter()
            .filter(|(_, entry)| now.saturating_duration_since(entry.last_edit) >= delay)
            .map(|(id, _)| *id)
            .collect();
        self.collect(due)
    }

    /// Takes everything, however fresh: something is about to read the store.
    pub fn drain(&mut self) -> Vec<(NodeId, Owed)> {
        let all: Vec<NodeId> = self.nodes.keys().copied().collect();
        self.collect(all)
    }

    /// Takes one node's edits, e.g. because it is being deleted.
    pub fn take(&mut self, node: NodeId) -> Option<Owed> {
        self.nodes.remove(&node).map(|entry| entry.owed)
    }

    /// Whether this window still owes the store a value for `key` on `node`.
    ///
    /// The one question a remote row has to ask before it overwrites a field.
    /// A held-back edit is 400 ms of typing the store has not seen: adopting
    /// the shared value for that key snaps the field back mid-word, and the
    /// held edit then pushes whatever the overwrite left behind. The common
    /// trigger is this window's own echo -- the debounce fires, the store
    /// echoes the row a moment later, and by then the user has typed on.
    pub fn owes(&self, node: NodeId, key: &str) -> bool {
        self.nodes
            .get(&node)
            .is_some_and(|entry| entry.owed.was.contains_key(key))
    }

    fn collect(&mut self, mut ids: Vec<NodeId>) -> Vec<(NodeId, Owed)> {
        ids.sort_unstable();
        ids.into_iter()
            .filter_map(|id| self.nodes.remove(&id).map(|entry| (id, entry.owed)))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owed(pairs: &[(&str, &str)]) -> Owed {
        Owed {
            was: pairs
                .iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        }
    }

    /// The whole point: a run of keystrokes is one change, and the value it is
    /// compared against is the one the store holds -- not the previous
    /// keystroke. A rename detected per character would be seven renames.
    #[test]
    fn a_run_of_edits_keeps_the_value_the_store_still_holds() {
        let mut pending = PendingEdits::new();
        let start = Instant::now();
        let node = NodeId(1);
        // `was` is what the store holds, which does not change while the edits
        // are unflushed, so the caller passes the same thing every time.
        for step in 0..4 {
            pending.touch(node, "columns", "customer_id:int", start + ms(step * 50));
        }
        assert_eq!(
            pending.settled(start + ms(150 + 400), DEBOUNCE),
            vec![(node, owed(&[("columns", "customer_id:int")]))]
        );
        assert!(pending.is_empty());
    }

    /// Nothing is pushed while the edits keep coming, and the wait restarts
    /// with every one of them.
    #[test]
    fn edits_that_keep_arriving_never_settle() {
        let mut pending = PendingEdits::new();
        let start = Instant::now();
        let node = NodeId(1);
        pending.touch(node, "name", "old", start);
        assert!(pending.settled(start + ms(399), DEBOUNCE).is_empty());
        // One more keystroke at 399 ms: the deadline moves with it.
        pending.touch(node, "name", "old", start + ms(399));
        assert!(pending.settled(start + ms(700), DEBOUNCE).is_empty());
        assert_eq!(pending.settled(start + ms(800), DEBOUNCE).len(), 1);
    }

    /// One node settling must not carry another node's unfinished edits with
    /// it, and the order has to be the node order rather than a hash order.
    #[test]
    fn nodes_settle_one_by_one_in_id_order() {
        let mut pending = PendingEdits::new();
        let start = Instant::now();
        pending.touch(NodeId(9), "a", "", start);
        pending.touch(NodeId(2), "a", "", start);
        pending.touch(NodeId(5), "a", "", start + ms(500));

        let settled = pending.settled(start + ms(450), DEBOUNCE);
        assert_eq!(
            settled.iter().map(|(id, _)| *id).collect::<Vec<_>>(),
            vec![NodeId(2), NodeId(9)]
        );
        // The fresh one is still owed, and a drain takes it whatever its age.
        let drained: Vec<NodeId> = pending.drain().iter().map(|(id, _)| *id).collect();
        assert_eq!(drained, vec![NodeId(5)]);
    }

    /// Several keys on one node travel together, each remembering its own
    /// pre-edit value.
    #[test]
    fn every_touched_key_remembers_its_own_starting_value() {
        let mut pending = PendingEdits::new();
        let start = Instant::now();
        let node = NodeId(1);
        pending.touch(node, "name", "orders", start);
        pending.touch(node, "columns", "id:int", start);
        pending.touch(node, "name", "orders", start);
        assert_eq!(
            pending.take(node),
            Some(owed(&[("name", "orders"), ("columns", "id:int")]))
        );
        assert!(pending.take(node).is_none());
    }

    /// What a remote row asks before it overwrites a field. Owed is per key
    /// and per node, and it ends the moment the edit reaches the store --
    /// otherwise the shared value would never be adopted again.
    #[test]
    fn a_key_is_owed_until_it_reaches_the_store() {
        let mut pending = PendingEdits::new();
        let start = Instant::now();
        let node = NodeId(1);
        assert!(!pending.owes(node, "columns"));

        pending.touch(node, "columns", "id:int", start);
        assert!(pending.owes(node, "columns"));
        // Only that key, and only that node.
        assert!(!pending.owes(node, "name"));
        assert!(!pending.owes(NodeId(2), "columns"));

        // Still owed while the edits keep coming...
        pending.touch(node, "columns", "id:int", start + ms(100));
        assert!(pending.owes(node, "columns"));
        // ...and no longer once they have settled and been taken.
        assert_eq!(pending.settled(start + ms(600), DEBOUNCE).len(), 1);
        assert!(!pending.owes(node, "columns"));
    }

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }
}
