use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};

use serde::{Deserialize, Serialize};

macro_rules! define_id {
    ($name:ident, $counter:ident) => {
        static $counter: AtomicU64 = AtomicU64::new(1);

        #[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub u64);

        impl $name {
            pub fn next() -> Self {
                Self($counter.fetch_add(1, Ordering::Relaxed))
            }

            /// Ensures every subsequent `next()` returns a value greater than
            /// `value`. Call after loading persisted ids so freshly generated
            /// ids never collide with restored ones.
            pub fn bump_above(value: u64) {
                let mut cur = $counter.load(Ordering::Relaxed);
                while cur <= value {
                    match $counter.compare_exchange_weak(
                        cur,
                        value + 1,
                        Ordering::Relaxed,
                        Ordering::Relaxed,
                    ) {
                        Ok(_) => break,
                        Err(actual) => cur = actual,
                    }
                }
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

    #[test]
    fn bump_above_prevents_collision() {
        // Use a high sentinel unlikely to be reached by other tests sharing
        // this process-global counter.
        EdgeId::bump_above(1_000_000);
        assert!(EdgeId::next().0 > 1_000_000);
        // Bumping below the current value is a no-op.
        EdgeId::bump_above(5);
        assert!(EdgeId::next().0 > 1_000_000);
    }

    #[test]
    fn serde_round_trip() {
        let id = NodeId(123);
        let json = serde_json::to_string(&id).unwrap();
        assert_eq!(json, "123"); // transparent serialization
        let back: NodeId = serde_json::from_str(&json).unwrap();
        assert_eq!(back, id);
    }
}
