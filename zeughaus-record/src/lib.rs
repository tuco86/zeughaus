//! Recorder and player: a graph's frames and values as a dataset on disk.
//!
//! A recording is a directory: one `<seq:06>.png` per frame and one
//! `index.jsonl` whose lines say when each frame was taken and what the graph's
//! other values were at that moment. Plain files and one JSON object per line
//! on purpose -- a dataset is something to train on, plot and inspect with
//! other tools, so nothing here invents a container format.
//!
//! [`RecorderNode`] writes, [`PlayerNode`] reads back, and the two are exact
//! counterparts: what the recorder wrote as `values` is what the player emits
//! as `values`, and the pixels round-trip byte for byte.
//!
//! PNG encoding happens synchronously inside the pass. A 4K frame costs tens of
//! milliseconds, which is fine at the rates a timer sets for a recording and is
//! the honest cost of writing a lossless frame; a deferred encoder would need a
//! queue, and a queue of 33 MB frames is how a process runs out of memory.
//!
//! Native only: it writes files.

mod nodes;

use zeughaus_core::*;

pub use nodes::{INDEX_FILE, PlayerNode, RecorderNode, frame_file, timestamp_name};

pub struct RecordPlugin;

impl DomainPlugin for RecordPlugin {
    fn name(&self) -> &str {
        "record"
    }

    fn node_catalog(&self) -> Vec<NodeDefinition> {
        vec![
            catalog_entry("record.writer", "Recorder", "Record", &RecorderNode::new()),
            catalog_entry("record.player", "Player", "Record", &PlayerNode::new()),
        ]
    }

    fn create_node(&self, type_id: &str) -> Option<Box<dyn ExecutableNode>> {
        match type_id {
            "record.writer" => Some(Box::new(RecorderNode::new())),
            "record.player" => Some(Box::new(PlayerNode::new())),
            _ => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_all_catalog_nodes() {
        let plugin = RecordPlugin;
        for def in plugin.node_catalog() {
            assert!(
                plugin.create_node(&def.type_id).is_some(),
                "failed: {}",
                def.type_id
            );
        }
    }
}
