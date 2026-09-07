use std::collections::HashMap;

use zeughaus_core::{EdgeId, Value};

pub struct EdgeCache {
    caches: HashMap<EdgeId, Value>,
    /// How many values have crossed each edge.
    ///
    /// The value alone cannot say whether it was just delivered: writing the
    /// same number twice is two messages. The counter is what lets a node be
    /// told which of its inputs arrived since it last ran.
    generation: HashMap<EdgeId, u64>,
}

impl EdgeCache {
    pub fn new() -> Self {
        Self {
            caches: HashMap::new(),
            generation: HashMap::new(),
        }
    }

    pub fn get(&self, edge_id: EdgeId) -> Option<&Value> {
        self.caches.get(&edge_id)
    }

    pub fn set(&mut self, edge_id: EdgeId, value: Value) {
        self.caches.insert(edge_id, value);
        *self.generation.entry(edge_id).or_default() += 1;
    }

    pub fn remove(&mut self, edge_id: EdgeId) {
        self.caches.remove(&edge_id);
        self.generation.remove(&edge_id);
    }

    /// How many values have crossed this edge; `0` for an edge that never
    /// carried one, which is also what a reconnected edge starts from.
    pub fn generation(&self, edge_id: EdgeId) -> u64 {
        self.generation.get(&edge_id).copied().unwrap_or(0)
    }
}

impl Default for EdgeCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn get_missing_returns_none() {
        let cache = EdgeCache::new();
        assert!(cache.get(EdgeId::next()).is_none());
    }

    #[test]
    fn set_then_get() {
        let mut cache = EdgeCache::new();
        let id = EdgeId::next();
        cache.set(id, Value::new(42.0f64));
        assert_eq!(cache.get(id).unwrap().downcast_ref::<f64>(), Some(&42.0));
    }

    #[test]
    fn set_overwrites() {
        let mut cache = EdgeCache::new();
        let id = EdgeId::next();
        cache.set(id, Value::new(1.0f64));
        cache.set(id, Value::new(2.0f64));
        assert_eq!(cache.get(id).unwrap().downcast_ref::<f64>(), Some(&2.0));
    }

    #[test]
    fn remove_clears() {
        let mut cache = EdgeCache::new();
        let id = EdgeId::next();
        cache.set(id, Value::new(1.0f64));
        cache.remove(id);
        assert!(cache.get(id).is_none());
    }

    #[test]
    fn remove_nonexistent_is_noop() {
        let mut cache = EdgeCache::new();
        cache.remove(EdgeId::next()); // should not panic
    }

    /// The counter is what distinguishes a delivery from a value: the same
    /// number written twice has to count twice.
    #[test]
    fn every_write_advances_the_generation() {
        let mut cache = EdgeCache::new();
        let id = EdgeId::next();
        assert_eq!(cache.generation(id), 0);
        cache.set(id, Value::new(1.0f64));
        assert_eq!(cache.generation(id), 1);
        cache.set(id, Value::new(1.0f64));
        assert_eq!(cache.generation(id), 2);
        // A removed edge is a new edge if it comes back.
        cache.remove(id);
        assert_eq!(cache.generation(id), 0);
    }
}
