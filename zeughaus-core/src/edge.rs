use serde::{Deserialize, Serialize};

/// How an edge delivers data. Only `LastValue` is currently implemented.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum EdgeSemantic {
    #[default]
    LastValue,
    /// Reserved: ring buffer, drops oldest on overflow (see DESIGN.md)
    BoundedQueue(usize),
    /// Reserved: unbounded backpressure queue (see DESIGN.md)
    Queue,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_last_value() {
        assert_eq!(EdgeSemantic::default(), EdgeSemantic::LastValue);
    }
}
