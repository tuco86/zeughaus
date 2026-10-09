//! The graph document protocol on [`GRAPH_PATH`](crate::GRAPH_PATH): an
//! editor edits the document a runner holds, and every editor attached to
//! that runner sees the result.
//!
//! One exchange per attached editor, both halves long-lived. The editor's
//! first frame is [`GraphMessage::Attach`]; the runner answers with the whole
//! document at a revision, then streams every applied change with the
//! revision it produced. An editor that sees a gap in the revisions, or whose
//! edit was refused, re-attaches and starts again from a fresh document: the
//! runner's document is the only truth, and an editor's copy is a cache of it.
//!
//! A frame is a 4-byte little-endian body length followed by the JSON body.

use serde::{Deserialize, Serialize};
use zeughaus_core::document::{EdgeData, GraphDocument, NodeData};

/// The protocol major an editor names in [`GraphMessage::Attach`]. A runner
/// refuses any other: the variants below are not versioned individually.
pub const GRAPH_MAJOR: u32 = 1;

/// Upper bound of one frame body. A whole document travels in one
/// [`GraphMessage::Attached`]; this bounds what a peer can make the other
/// side allocate.
pub const MAX_GRAPH_FRAME_BYTES: usize = 16 * 1024 * 1024;

/// What an editor asks the runner to do to its document.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GraphEdit {
    CreateNode {
        node: NodeData,
    },
    MoveNode {
        id: u64,
        x: f32,
        y: f32,
    },
    SetParams {
        id: u64,
        params: Vec<(String, String)>,
    },
    RenameNode {
        id: u64,
        display_name: String,
    },
    DeleteNode {
        id: u64,
    },
    ConnectEdge {
        edge: EdgeData,
    },
    DisconnectEdge {
        id: u64,
    },
}

/// One change the runner applied to its document. A node change carries the
/// whole node, so applying it never depends on the previous state of it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GraphChange {
    NodeUpsert { node: NodeData },
    NodeRemove { id: u64 },
    EdgeInsert { edge: EdgeData },
    EdgeRemove { id: u64 },
}

/// One frame of a graph exchange, in either direction.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GraphMessage {
    /// Editor to runner, the first frame of an exchange.
    Attach { major: u32 },
    /// Editor to runner. `request` is echoed in a [`GraphMessage::Refused`].
    Edit { request: u64, edit: GraphEdit },
    /// Runner to editor: the whole document as of `revision`.
    Attached {
        revision: u64,
        document: GraphDocument,
    },
    /// Runner to editor: the change that produced `revision`.
    Changed { revision: u64, change: GraphChange },
    /// Runner to editor: an edit that was not applied, and why.
    Refused { request: u64, message: String },
}

impl GraphMessage {
    /// The frame: a 4-byte little-endian body length, then the JSON body.
    pub fn encode(&self) -> Result<Vec<u8>, String> {
        let body = serde_json::to_vec(self).map_err(|e| format!("graph message: {e}"))?;
        if body.len() > MAX_GRAPH_FRAME_BYTES {
            return Err(format!(
                "graph message of {} bytes exceeds the {MAX_GRAPH_FRAME_BYTES} byte limit",
                body.len()
            ));
        }
        let len = u32::try_from(body.len()).map_err(|_| "graph message too large".to_owned())?;
        let mut frame = Vec::with_capacity(4 + body.len());
        frame.extend_from_slice(&len.to_le_bytes());
        frame.extend_from_slice(&body);
        Ok(frame)
    }

    /// The body length a frame header announces, refused above
    /// [`MAX_GRAPH_FRAME_BYTES`] before anything is allocated for it.
    pub fn body_len(header: [u8; 4]) -> Result<usize, String> {
        let len = u32::from_le_bytes(header) as usize;
        if len > MAX_GRAPH_FRAME_BYTES {
            return Err(format!(
                "graph frame of {len} bytes exceeds the {MAX_GRAPH_FRAME_BYTES} byte limit"
            ));
        }
        Ok(len)
    }

    /// Parses one frame body.
    pub fn decode(body: &[u8]) -> Result<GraphMessage, String> {
        serde_json::from_slice(body).map_err(|e| format!("graph message: {e}"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn node() -> NodeData {
        NodeData {
            id: 7,
            type_id: "graph.sub".into(),
            display_name: "Main".into(),
            x: 1.5,
            y: -2.0,
            parent: 0,
            params: vec![("k".into(), "v".into())],
        }
    }

    fn edge() -> EdgeData {
        EdgeData {
            id: 9,
            from_node: 7,
            from_pin: "out".into(),
            to_node: 8,
            to_pin: "in".into(),
        }
    }

    #[test]
    fn every_message_round_trips() {
        let messages = vec![
            GraphMessage::Attach { major: GRAPH_MAJOR },
            GraphMessage::Edit {
                request: 3,
                edit: GraphEdit::CreateNode { node: node() },
            },
            GraphMessage::Edit {
                request: 4,
                edit: GraphEdit::MoveNode {
                    id: 7,
                    x: 3.0,
                    y: 4.0,
                },
            },
            GraphMessage::Edit {
                request: 5,
                edit: GraphEdit::SetParams {
                    id: 7,
                    params: vec![("a".into(), "1".into())],
                },
            },
            GraphMessage::Edit {
                request: 6,
                edit: GraphEdit::RenameNode {
                    id: 7,
                    display_name: "Other".into(),
                },
            },
            GraphMessage::Edit {
                request: 7,
                edit: GraphEdit::DeleteNode { id: 7 },
            },
            GraphMessage::Edit {
                request: 8,
                edit: GraphEdit::ConnectEdge { edge: edge() },
            },
            GraphMessage::Edit {
                request: 9,
                edit: GraphEdit::DisconnectEdge { id: 9 },
            },
            GraphMessage::Attached {
                revision: 12,
                document: GraphDocument {
                    nodes: vec![node()],
                    edges: vec![edge()],
                },
            },
            GraphMessage::Changed {
                revision: 13,
                change: GraphChange::NodeUpsert { node: node() },
            },
            GraphMessage::Changed {
                revision: 14,
                change: GraphChange::NodeRemove { id: 7 },
            },
            GraphMessage::Changed {
                revision: 15,
                change: GraphChange::EdgeInsert { edge: edge() },
            },
            GraphMessage::Changed {
                revision: 16,
                change: GraphChange::EdgeRemove { id: 9 },
            },
            GraphMessage::Refused {
                request: 8,
                message: "node 8 does not exist".into(),
            },
        ];
        for message in messages {
            let frame = message.encode().unwrap();
            let header: [u8; 4] = frame[..4].try_into().unwrap();
            let len = GraphMessage::body_len(header).unwrap();
            assert_eq!(len, frame.len() - 4);
            assert_eq!(GraphMessage::decode(&frame[4..]).unwrap(), message);
        }
    }

    #[test]
    fn oversized_frame_is_refused_before_reading() {
        let header = ((MAX_GRAPH_FRAME_BYTES + 1) as u32).to_le_bytes();
        assert!(GraphMessage::body_len(header).is_err());
        let header = (MAX_GRAPH_FRAME_BYTES as u32).to_le_bytes();
        assert_eq!(
            GraphMessage::body_len(header).unwrap(),
            MAX_GRAPH_FRAME_BYTES
        );
    }
}
