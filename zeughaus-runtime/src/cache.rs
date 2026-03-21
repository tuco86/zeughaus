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
