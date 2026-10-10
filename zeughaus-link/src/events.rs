//! Runtime events: what the executing runtime tells editors about a pass.
//!
//! Three endpoints, one vocabulary. [`RuntimeEvent`] is published on `/events`
//! as it happens, [`Snapshot`] answers `/snapshot` for an editor that joined
//! late, and [`TriggerRequest`] travels the other way on `/triggers`. None of
//! it goes through the graph link: that carries the document (which nodes
//! exist and how they are wired), and everything a pass produces travels
//! straight from the process that computed it.
//!
//! `seq` is one monotonic counter per runner process, stamped on every event
//! and on the snapshot. Pub/Sub messages are separate QUIC streams and may
//! reorder, and the snapshot is fetched concurrently with the subscription, so
//! an editor drops anything not newer than what it already applied for that
//! pin. Without it a late snapshot would overwrite a fresh value.

use serde::{Deserialize, Serialize};

use crate::machine::{BusyMode, MachineState};

/// Something that happened during one pass.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum RuntimeEvent {
    /// An output pin's current scalar (`ty`/`value` as `encode_scalar` writes
    /// them).
    Output {
        seq: u64,
        node_id: u64,
        pin: String,
        ty: String,
        value: String,
    },
    /// A pin stopped producing a value. Absence is a state a viewer has to be
    /// able to reach: a stale number must not outlive the run that produced it.
    OutputCleared { seq: u64, node_id: u64, pin: String },
    /// A value was delivered across an edge during execution. The record of
    /// traffic rather than of state, which is what makes one message one
    /// particle even when the value did not change.
    Edge { seq: u64, edge_id: u64 },
    /// A node's last run failed, with the message the user has to act on.
    ///
    /// The only path a failure has to an editor: the process that ran the node
    /// is not the process drawing it, so without this the report existed
    /// nowhere but the runtime's own log.
    NodeError {
        seq: u64,
        node_id: u64,
        message: String,
    },
    /// A node stopped failing. Recovery is a state an editor has to be able to
    /// reach, or a fixed node would keep its red border for the session.
    NodeErrorCleared { seq: u64, node_id: u64 },
    /// A node refused one of its settings, with the reason and the setting it
    /// is about.
    ///
    /// Its own event rather than a [`RuntimeEvent::NodeError`] because it is
    /// not a failed run: the node kept the value it had and goes on working.
    /// An editor draws it under the field it belongs to, and a refused setting
    /// therefore does not raise the alarm a failure raises. The runtime is the
    /// only process that can report it for a window that did not type it.
    SettingRejected {
        seq: u64,
        node_id: u64,
        key: String,
        message: String,
    },
    /// A setting that was refused is no longer refused: the node took a value
    /// for that key. The counterpart of [`RuntimeEvent::SettingRejected`], for
    /// the same reason [`RuntimeEvent::OutputCleared`] exists.
    SettingAccepted { seq: u64, node_id: u64, key: String },
    /// The CI machine's busy state changed: its mode, or under
    /// [`BusyMode::Auto`] the measurement. Only a runner that runs CI sends it.
    Machine {
        seq: u64,
        mode: BusyMode,
        busy: bool,
    },
    /// A repository's default branch turned red, or has stayed red for an
    /// hour and failed again. An editor shows it on the desktop. Only a
    /// runner that runs CI sends it; it is not state, so no snapshot
    /// carries it.
    CiAlert {
        seq: u64,
        title: String,
        body: String,
    },
}

impl RuntimeEvent {
    /// Encodes one event as JSON. One weida message is one payload, so no
    /// framing and no trailing newline are needed.
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| b"{}".to_vec())
    }

    /// Decodes an event. `None` for anything malformed: the bytes come from
    /// another process and a peer must not be able to panic a viewer.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

/// One output pin's current value in a [`Snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutputRow {
    pub node_id: u64,
    pub pin: String,
    pub ty: String,
    pub value: String,
}

/// One failing node in a [`Snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ErrorRow {
    pub node_id: u64,
    pub message: String,
}

/// One refused setting in a [`Snapshot`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RejectionRow {
    pub node_id: u64,
    pub key: String,
    pub message: String,
}

/// Everything the runtime currently holds, for an editor that joined after the
/// values were produced.
///
/// The whole set rather than a delta, because absence is meaningful: a pin with
/// no row produced no value, which is what an editor draws dimmed.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Snapshot {
    pub seq: u64,
    pub outputs: Vec<OutputRow>,
    /// The nodes currently failing. Absence means "not failing", exactly as an
    /// absent output means "produced nothing".
    #[serde(default)]
    pub errors: Vec<ErrorRow>,
    /// The settings currently refused, one row per (node, setting). Absence
    /// means "accepted", the same way an absent error means "not failing".
    #[serde(default)]
    pub rejections: Vec<RejectionRow>,
    /// The CI machine's busy state; `None` from a runner without CI.
    #[serde(default)]
    pub machine: Option<MachineState>,
}

impl Snapshot {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| b"{}".to_vec())
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

/// A node asked to fire once: a manual press in an editor, or an external
/// trigger relayed here over weida.
///
/// The payload is handed to the node as its `fire` parameter, which is what
/// lets one trigger carry what it is about (a webhook body, a revision) while
/// the protocol stays free of anything node-specific. A bare press sends none,
/// and a sender older than the payload leaves the field out, so it defaults.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TriggerRequest {
    pub node_id: u64,
    #[serde(default)]
    pub payload: Option<String>,
    /// Whether the press came from outside an editor (the `zeughaus-runner
    /// trigger` CLI, a hook). A job run started that way shows up in the
    /// runner's locked "Triggered" group instead of staying detached.
    #[serde(default)]
    pub external: bool,
}

impl TriggerRequest {
    pub fn encode(&self) -> Vec<u8> {
        serde_json::to_vec(self).unwrap_or_else(|_| b"{}".to_vec())
    }

    pub fn decode(bytes: &[u8]) -> Option<Self> {
        serde_json::from_slice(bytes).ok()
    }
}

/// Topic of [`RuntimeEvent::Output`] and [`RuntimeEvent::OutputCleared`].
pub const TOPIC_OUTPUT: &str = "output";

/// Topic of [`RuntimeEvent::Edge`].
pub const TOPIC_EDGE: &str = "edge";

/// Topic of everything an editor draws in red: the node failures
/// ([`RuntimeEvent::NodeError`], [`RuntimeEvent::NodeErrorCleared`]) and the
/// refused settings ([`RuntimeEvent::SettingRejected`],
/// [`RuntimeEvent::SettingAccepted`]). One subscription, because a viewer
/// wants both or neither.
pub const TOPIC_ERROR: &str = "error";

/// Topic of [`RuntimeEvent::Machine`].
pub const TOPIC_MACHINE: &str = "machine";

/// Topic of [`RuntimeEvent::CiAlert`].
pub const TOPIC_CI: &str = "ci";

/// Largest event payload a subscriber reads. An event is a few identifiers and
/// a scalar rendered as text; the cap is what stops a peer from making a viewer
/// allocate on its behalf.
pub const MAX_EVENT_BYTES: usize = 64 * 1024;

/// Largest snapshot a viewer reads. Generous because it scales with the graph
/// (one row per producing pin), bounded because it is still one allocation.
pub const MAX_SNAPSHOT_BYTES: usize = 16 * 1024 * 1024;

/// Largest trigger request the runtime reads. It carries one node id and a
/// free-text payload -- a webhook body is the size this has to allow for,
/// while still being one bounded allocation per press.
pub const MAX_TRIGGER_BYTES: usize = 64 * 1024;

#[cfg(test)]
mod tests {
    use super::*;

    /// The two ends are separate processes, so every variant has to survive the
    /// round trip -- a renamed field would silently stop delivering values.
    #[test]
    fn every_event_round_trips() {
        let events = [
            RuntimeEvent::Output {
                seq: 7,
                node_id: 3,
                pin: "result".to_owned(),
                ty: "float".to_owned(),
                value: "1.5".to_owned(),
            },
            RuntimeEvent::OutputCleared {
                seq: 8,
                node_id: 3,
                pin: "result".to_owned(),
            },
            RuntimeEvent::Edge { seq: 9, edge_id: 4 },
            RuntimeEvent::NodeError {
                seq: 10,
                node_id: 5,
                message: "no table wired".to_owned(),
            },
            RuntimeEvent::NodeErrorCleared {
                seq: 11,
                node_id: 5,
            },
            RuntimeEvent::SettingRejected {
                seq: 12,
                node_id: 5,
                key: "limit".to_owned(),
                message: "expected a row count between 1 and 10000".to_owned(),
            },
            RuntimeEvent::SettingAccepted {
                seq: 13,
                node_id: 5,
                key: "limit".to_owned(),
            },
            RuntimeEvent::Machine {
                seq: 14,
                mode: BusyMode::Busy,
                busy: true,
            },
            RuntimeEvent::CiAlert {
                seq: 15,
                title: "griasdi main is red (#138)".to_owned(),
                body: "windows red since #138".to_owned(),
            },
        ];
        for event in events {
            assert_eq!(RuntimeEvent::decode(&event.encode()), Some(event));
        }
    }

    /// A malformed payload is a peer's problem, never a panic here.
    #[test]
    fn garbage_decodes_to_nothing() {
        assert!(RuntimeEvent::decode(b"not json").is_none());
        assert!(Snapshot::decode(b"").is_none());
        assert!(TriggerRequest::decode(b"{}").is_none());
    }

    #[test]
    fn a_snapshot_round_trips() {
        let snapshot = Snapshot {
            seq: 12,
            outputs: vec![OutputRow {
                node_id: 1,
                pin: "out".to_owned(),
                ty: "int".to_owned(),
                value: "42".to_owned(),
            }],
            errors: vec![ErrorRow {
                node_id: 2,
                message: "cannot open /srv/x.sqlite".to_owned(),
            }],
            rejections: vec![RejectionRow {
                node_id: 3,
                key: "name".to_owned(),
                message: "a table needs a name".to_owned(),
            }],
            machine: Some(MachineState {
                mode: BusyMode::Auto,
                busy: false,
            }),
        };
        assert_eq!(Snapshot::decode(&snapshot.encode()), Some(snapshot));
    }

    /// A runtime older than the error channel sends a snapshot without the
    /// field. Reading it as "nothing is failing" is what keeps a viewer from
    /// refusing the whole snapshot -- and the values in it -- over an addition.
    #[test]
    fn a_snapshot_without_errors_still_decodes() {
        let decoded = Snapshot::decode(br#"{"seq":3,"outputs":[]}"#).expect("snapshot");
        assert_eq!(decoded.seq, 3);
        assert!(decoded.errors.is_empty());
        assert!(decoded.rejections.is_empty());
        assert_eq!(decoded.machine, None);
    }

    #[test]
    fn a_trigger_request_round_trips_with_and_without_a_payload() {
        for payload in [None, Some("{\"ref\":\"main\"}".to_owned())] {
            let request = TriggerRequest {
                node_id: 99,
                payload,
                external: true,
            };
            assert_eq!(TriggerRequest::decode(&request.encode()), Some(request));
        }
    }

    /// A press from an editor that predates the payload carries only the node
    /// id; refusing it would break the button rather than the addition.
    #[test]
    fn a_trigger_request_without_a_payload_still_decodes() {
        let decoded = TriggerRequest::decode(br#"{"node_id":1}"#).expect("trigger");
        assert_eq!(decoded.node_id, 1);
        assert_eq!(decoded.payload, None);
    }
}
