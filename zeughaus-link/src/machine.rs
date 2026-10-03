//! Whether a CI runner's machine counts as in use, and who decides it.
//!
//! A runner with CI measures it (GPU load over a window) unless someone in
//! an editor overrides the measurement. [`MachineState`] is what it reports
//! on `/events` and in the snapshot; [`BusyRequest`] on `/busy` changes the
//! override. A runner without CI reports no machine and serves no `/busy`.

use serde::{Deserialize, Serialize};

/// Who decides whether the machine is busy.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BusyMode {
    /// The measurement decides.
    #[default]
    Auto,
    /// Busy, whatever the measurement says.
    Busy,
    /// Free, whatever the measurement says.
    Free,
}

impl BusyMode {
    pub fn as_str(self) -> &'static str {
        match self {
            BusyMode::Auto => "auto",
            BusyMode::Busy => "busy",
            BusyMode::Free => "free",
        }
    }

    pub fn parse(text: &str) -> Option<BusyMode> {
        match text {
            "auto" => Some(BusyMode::Auto),
            "busy" => Some(BusyMode::Busy),
            "free" => Some(BusyMode::Free),
            _ => None,
        }
    }

    /// The mode a toggle moves to from this one: auto, busy, free, auto.
    pub fn next(self) -> BusyMode {
        match self {
            BusyMode::Auto => BusyMode::Busy,
            BusyMode::Busy => BusyMode::Free,
            BusyMode::Free => BusyMode::Auto,
        }
    }
}

/// The machine as the runner sees it: the mode, and what that makes it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct MachineState {
    pub mode: BusyMode,
    /// Busy as the scheduler acts on it: the mode, or under `Auto` the
    /// measurement.
    pub busy: bool,
}

impl MachineState {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| b"{}".to_vec())
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

/// Sets the override. The reply is the [`MachineState`] after it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct BusyRequest {
    pub mode: BusyMode,
}

impl BusyRequest {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| b"{}".to_vec())
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

/// Largest busy exchange either end reads. Two fields.
pub const MAX_BUSY_BYTES: usize = 1024;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_busy_exchange_round_trips() {
        let request = BusyRequest {
            mode: BusyMode::Free,
        };
        assert_eq!(BusyRequest::decode(&request.encode()), Some(request));
        let state = MachineState {
            mode: BusyMode::Auto,
            busy: true,
        };
        assert_eq!(MachineState::decode(&state.encode()), Some(state));
        assert_eq!(BusyRequest::decode(br#"{"mode":"maybe"}"#), None);
    }

    #[test]
    fn the_toggle_cycles_through_every_mode() {
        let mut mode = BusyMode::Auto;
        let mut seen = Vec::new();
        for _ in 0..3 {
            seen.push(mode);
            mode = mode.next();
        }
        assert_eq!(mode, BusyMode::Auto);
        assert_eq!(seen, [BusyMode::Auto, BusyMode::Busy, BusyMode::Free]);
        for mode in seen {
            assert_eq!(BusyMode::parse(mode.as_str()), Some(mode));
        }
    }
}
