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

    /// A node refused a setting: the value never took effect and the node kept
    /// the one it had.
    ///
    /// Distinct from [`Self::ExecutionFailed`] because it is answered at a
    /// different place. A failed run is reported on the node; a refused value
    /// belongs under the field it was typed into, where the reason alone is
    /// the whole message -- "node execution failed: limit 'lots': ..." says
    /// nothing the field does not already show.
    #[error("{0}")]
    InvalidParameter(String),

    #[error("unknown node type: {0}")]
    UnknownNodeType(String),
}

pub type Result<T> = std::result::Result<T, ZeughausError>;
