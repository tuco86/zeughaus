use crate::id::{NodeId, PinId};

#[derive(Debug, thiserror::Error)]
pub enum ZeughausError {
    #[error("node not found: {0:?}")]
    NodeNotFound(NodeId),

    #[error("pin not found: {0:?}")]
    PinNotFound(PinId),

    #[error("type mismatch: expected {expected}, got {actual}")]
    TypeMismatch {
        expected: String,
        actual: String,
    },

    #[error("cycle detected in graph")]
    CycleDetected,

    #[error("node execution failed: {0}")]
    ExecutionFailed(String),

    #[error("unknown node type: {0}")]
    UnknownNodeType(String),
}

pub type Result<T> = std::result::Result<T, ZeughausError>;
