use std::collections::HashMap;

use zeughaus_core::{EdgeId, Value};

pub struct EdgeCache {
    caches: HashMap<EdgeId, Value>,
}

impl EdgeCache {
    pub fn new() -> Self {
        Self {
            caches: HashMap::new(),
        }
    }

    pub fn get(&self, edge_id: EdgeId) -> Option<&Value> {
        self.caches.get(&edge_id)
    }

    pub fn set(&mut self, edge_id: EdgeId, value: Value) {
        self.caches.insert(edge_id, value);
    }

    pub fn remove(&mut self, edge_id: EdgeId) {
        self.caches.remove(&edge_id);
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
}
