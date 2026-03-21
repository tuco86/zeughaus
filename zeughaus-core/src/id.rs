use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

macro_rules! define_id {
    ($name:ident, $counter:ident) => {
        static $counter: AtomicU64 = AtomicU64::new(1);

        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
        pub struct $name(pub u64);

        impl $name {
            pub fn next() -> Self {
                Self($counter.fetch_add(1, Ordering::Relaxed))
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}({})", stringify!($name), self.0)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", self.0)
            }
        }
    };
}

define_id!(NodeId, NODE_COUNTER);
define_id!(PinId, PIN_COUNTER);
define_id!(EdgeId, EDGE_COUNTER);

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn ids_are_unique() {
        let mut set = HashSet::new();
        for _ in 0..100 {
            assert!(set.insert(NodeId::next()));
        }
    }

    #[test]
    fn ids_are_copy() {
        let id = NodeId::next();
        let id2 = id;
        assert_eq!(id, id2);
    }

    #[test]
    fn debug_format() {
        let id = NodeId(42);
        assert_eq!(format!("{id:?}"), "NodeId(42)");
    }
}
