use std::time::Instant;

use crate::id::NodeId;

#[derive(Debug, Clone)]
pub struct EventMeta {
    pub event_id: u64,
    pub trace_id: u64,
    pub timestamp: Instant,
    pub source_node: NodeId,
}

#[derive(Debug, Clone)]
pub struct Event<T> {
    pub meta: EventMeta,
    pub payload: T,
}
