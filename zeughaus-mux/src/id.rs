//! Stable identifiers.
//!
//! Every id here is minted by exactly one authority and never reused within
//! that authority's lifetime: the runner mints [`RunnerIncarnation`] once per
//! process and every workspace, tab, split, pane and terminal id from a
//! counter; the editor mints [`ClientInstanceId`] once per process and
//! [`RequestId`] from a counter. None of them is a credential -- the runner
//! authorizes by the peer identity weida proved, and an id only names.
//!
//! iced's `pane_grid::Pane` is deliberately not among them: it is local widget
//! state, rebuilt from a [`crate::PaneNode`] tree, and cannot be sent.

use std::fmt;

use serde::{Deserialize, Serialize};

macro_rules! counter_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        pub struct $name(pub u64);

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}#{}", stringify!($name), self.0)
            }
        }
    };
}

macro_rules! random_id {
    ($(#[$doc:meta])* $name:ident) => {
        $(#[$doc])*
        #[derive(Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
        pub struct $name(pub [u8; 16]);

        impl $name {
            /// An id from any 16 random bytes. The caller brings the
            /// randomness: this crate has no dependency to draw it from, and
            /// the runner and the editor already have one each.
            pub fn from_bytes(bytes: [u8; 16]) -> Self {
                Self(bytes)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}(", stringify!($name))?;
                for byte in &self.0[..4] {
                    write!(f, "{byte:02x}")?;
                }
                f.write_str("..)")
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                for byte in &self.0 {
                    write!(f, "{byte:02x}")?;
                }
                Ok(())
            }
        }
    };
}

random_id! {
    /// One runner process. Everything a client cached about a workspace or a
    /// terminal belongs to one incarnation; a different one on attach means
    /// every cache is discarded, because the terminals it named are gone.
    RunnerIncarnation
}

random_id! {
    /// One editor process, retained across redials so a control lease can be
    /// handed back to the same client after a network blink.
    ClientInstanceId
}

counter_id! {
    /// A workspace. One per runner today; the id exists so that never has
    /// to change the wire.
    WorkspaceId
}

counter_id! {
    /// A tab in the shared workspace.
    TabId
}

counter_id! {
    /// A split in a tab's pane tree, so a ratio change can name it.
    SplitId
}

counter_id! {
    /// A leaf in a tab's pane tree.
    PaneId
}

counter_id! {
    /// A terminal session on the runner: a child process, its PTY and its
    /// canonical screen. Outlives any pane that shows it only until the pane
    /// is explicitly closed.
    TerminalId
}

counter_id! {
    /// Correlates a command with its reply on the control exchange. Minted
    /// by the client; the runner deduplicates a repeated id from the same
    /// client instance, so a command resent after a redial applies once.
    RequestId
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn random_ids_print_whole_and_debug_short() {
        let id = RunnerIncarnation::from_bytes([0xab; 16]);
        assert_eq!(id.to_string(), "ab".repeat(16));
        assert_eq!(format!("{id:?}"), "RunnerIncarnation(abababab..)");
    }

    #[test]
    fn counter_ids_order_by_value() {
        assert!(PaneId(1) < PaneId(2));
        assert_eq!(TerminalId(7).to_string(), "TerminalId#7");
    }
}
