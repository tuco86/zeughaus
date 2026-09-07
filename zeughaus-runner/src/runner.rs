//! The executing half of the runner: turns store events into graph mutations,
//! runs the graph and publishes what it computed.
//!
//! Kept apart from `main.rs` so the decisions that are pure -- which press is a
//! new one, what type a stored parameter string becomes -- can be tested
//! without a server.
//!
//! The graph itself is the only node bookkeeping here. The editor keeps a second
//! representation because it has to draw one; this process has nothing to draw,
//! so type ids, pins and positions are read back out of [`GraphExecutor::graph`]
//! rather than mirrored beside it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use zeughaus_core::{
    DomainPlugin, EdgeData, EdgeId, EdgeSemantic, Image, NodeConfig, NodeData, NodeId,
    PinDirection, Ty, TypeConverters, Value, encode_scalar,
};
use weida::Publisher;
use zeughaus_runtime::{DeferredWork, Graph, GraphEdge, GraphExecutor, GraphNode};
use zeughaus_samples::{OutputRow, RuntimeEvent, Snapshot, TOPIC_EDGE, TOPIC_OUTPUT};
use zeughaus_sync::SyncEvent;
use zeughaus_sync::module_bindings::DbConnection;

use crate::feed::FrameRegistry;

/// Outcome of one node's deferred work, as reported back by the host loop.
pub type AsyncResult = Result<HashMap<String, Value>, String>;

pub struct Runner {
    conn: DbConnection,
    plugins: Vec<Box<dyn DomainPlugin>>,
    executor: GraphExecutor,
    /// Whether this process currently owns execution. Read back from the store
    /// on every batch rather than remembered, because ownership changes by
    /// another runner disappearing, not by a handshake.
    is_owner: bool,
    /// Role and runner count as last reported. A headless standby that logged
    /// nothing would be indistinguishable from one that is stuck, so the role is
    /// stated as soon as it is known and again whenever it or the number of
    /// connected runners changes.
    logged_role: Option<(bool, usize)>,
    /// What this process last published, per node and output pin, as the
    /// `(type tag, text)` pair that went over the wire. Diffed against the next
    /// pass so an unchanged value costs no reducer call -- a capture graph runs
    /// at frame rate and would otherwise flood the store.
    published: HashMap<NodeId, HashMap<String, (String, String)>>,
    /// Where runtime events go. `None` when the transport failed to start:
    /// publishing then does nothing, the same degradation as having no feed.
    publisher: Option<Publisher>,
    /// Stamped on every event and on the snapshot, so an editor can tell a
    /// message it already applied from a newer one. Pub/Sub messages travel on
    /// separate QUIC streams and may reorder.
    seq: u64,
    /// The current output set, served to editors that join late. Shared with
    /// the task answering `/snapshot`.
    snapshot: Arc<Mutex<Snapshot>>,
    /// The last publish failure written to the log, so a broken publisher
    /// reports once instead of once per event.
    logged_publish: Option<String>,
    /// Edges that named a node this process did not have yet. See
    /// [`Runner::apply_edge_insert`] for why they cannot be dropped.
    pending_edges: Vec<EdgeData>,
    /// When each clocked node is next due. Held by the owner only.
    due: HashMap<NodeId, Instant>,
    /// Node errors already written to the log, so a failing node reports once
    /// instead of on every pass it stays broken.
    logged_errors: HashMap<NodeId, String>,
    /// Nodes whose type no plugin here knows, already reported. A remote editor
    /// can create a type this build lacks, and an upsert repeats on every
    /// parameter edit.
    unknown_types: HashSet<u64>,
    /// Graph size at the last log line, so the counts are reported when they
    /// change instead of once per batch.
    logged_size: (usize, usize),
    /// The newest frame per frame-producing output pin, read by the feed
    /// server's tasks. Rebuilt after every pass: see
    /// [`Runner::refresh_frames`].
    frames: Arc<FrameRegistry>,
    /// The pinned root URL this process serves on
    /// (`weida://sha256:<fp>@host:port/`). `None` until the listener is bound
    /// -- an editor must never be pointed at a runtime that is not serving yet.
    endpoint: Option<String>,
}

impl Runner {
    pub fn new(
        conn: DbConnection,
        frames: Arc<FrameRegistry>,
        publisher: Option<Publisher>,
        snapshot: Arc<Mutex<Snapshot>>,
    ) -> Self {
        // The same plugin set the native editor registers. Both sides must agree
        // on what exists and what may connect: the editor validates a drag
        // against these converters, this process coerces the value that then
        // crosses the edge.
        let plugins: Vec<Box<dyn DomainPlugin>> = vec![
            Box::new(zeughaus_transform::TransformPlugin),
            Box::new(zeughaus_ml::MlPlugin),
            Box::new(zeughaus_flow::FlowPlugin),
            Box::new(zeughaus_graph::GraphPlugin),
            Box::new(zeughaus_capture::CapturePlugin),
            Box::new(zeughaus_llm::LlmPlugin),
        ];
        let converters = Arc::new({
            let mut c = TypeConverters::with_builtins();
            for p in &plugins {
                p.register_converters(&mut c);
            }
            c
        });
        let catalog: usize = plugins.iter().map(|p| p.node_catalog().len()).sum();
        eprintln!(
            "[runner] {} plugins, {catalog} node types available",
            plugins.len()
        );

        let mut executor = GraphExecutor::new(Graph::new());
        executor.set_converters(converters);

        Self {
            conn,
            plugins,
            executor,
            is_owner: false,
            logged_role: None,
            published: HashMap::new(),
            publisher,
            seq: 0,
            snapshot,
            logged_publish: None,
            pending_edges: Vec::new(),
            due: HashMap::new(),
            logged_errors: HashMap::new(),
            unknown_types: HashSet::new(),
            logged_size: (0, 0),
            frames,
            endpoint: None,
        }
    }

    /// Re-reads who owns execution, and reports the role a human debugging this
    /// process needs before anything else.
    pub fn refresh_ownership(&mut self) {
        let runners = zeughaus_sync::runtime_count(&self.conn);
        if runners == 0 {
            // The runtime table has not reached this client yet, so the store
            // has said nothing about who executes. Staying a non-owner is the
            // safe reading: a runner that assumed ownership here would
            // double-execute a graph another one is already running.
            return;
        }
        let owner = zeughaus_sync::is_owner(&self.conn);
        if self.logged_role != Some((owner, runners)) {
            self.logged_role = Some((owner, runners));
            if owner {
                eprintln!("[runner] executing this session ({runners} runner(s) connected)");
            } else {
                eprintln!(
                    "[runner] standby ({runners} runners connected): another one executes"
                );
            }
        }
        if owner == self.is_owner {
            return;
        }
        self.is_owner = owner;
        // Ownership decides where editors are pointed: `owner_endpoint`
        // resolves the *owning* runtime's row, so a standby inheriting
        // execution becomes the row every editor reads and has to be sure its
        // address is in it.
        self.announce_endpoint();
        if !owner {
            // The frames this process holds are the last ones it produced.
            // Another runtime is producing the real ones now, so its viewers
            // have to be sent away rather than shown a still picture.
            self.frames.clear();
            // Nothing this process published is current any more, and the
            // snapshot it serves must not answer with values it no longer
            // produces.
            self.published.clear();
            *self.snapshot.lock().unwrap_or_else(|e| e.into_inner()) = Snapshot::default();
            return;
        }
        // Ownership was just inherited, so this process has never run this
        // graph: every node is stale.
        for id in self.executor.graph.node_ids().collect::<Vec<_>>() {
            self.executor.mark_dirty(id);
        }
        // Nothing was published by this process, so its first pass republishes
        // everything. Values do not live in the store any more, so there is no
        // predecessor's row left behind to diff against and nothing to clear.
        self.published.clear();
    }

    pub fn apply(&mut self, event: SyncEvent) {
        match event {
            SyncEvent::NodeUpsert(nd) => self.apply_node_upsert(nd),
            SyncEvent::NodeRemove(id) => self.apply_node_remove(NodeId(id)),
            SyncEvent::EdgeInsert(ed) => self.apply_edge_insert(ed),
            SyncEvent::EdgeRemove(id) => self.apply_edge_remove(EdgeId(id)),
            // Ownership is read from the client cache, not from the event.
            SyncEvent::RuntimesChanged => {}
            // Values travel over weida now, so nothing in the store reports
            // them and nothing here has to be adopted.
            SyncEvent::SubscriptionApplied => {
                // The subscription is the first look this process gets at its
                // own `runtime` row, and a reconnect recreates that row without
                // the endpoint. Re-announcing here is what keeps an editor from
                // resolving an owning runtime with an empty address.
                self.announce_endpoint();
            }
        }
    }

    /// Runs one pass: execute the dirty nodes and publish what changed.
    /// Returns the work that has to run off-thread.
    ///
    /// A standby does nothing at all. Executing "just the cheap nodes" would
    /// double-run every side effect in the graph, which is the split brain the
    /// single-owner rule exists to remove.
    pub fn pass(&mut self) -> DeferredWork {
        self.resolve_pending_edges();
        // Logged for a standby too: it tracks the graph so a takeover is
        // immediate, and a silent process is impossible to tell from a stuck one.
        self.log_size();
        if !self.is_owner {
            return DeferredWork::new();
        }
        let deferred = match self.executor.execute_dirty() {
            Ok(deferred) => deferred,
            // Only a graph that cannot be ordered (a cycle) fails the pass
            // itself; a single node's failure is reported per node.
            Err(e) => {
                eprintln!("[runner] pass failed: {e}");
                return DeferredWork::new();
            }
        };
        self.after_pass();
        deferred
    }

    /// Marks every clocked node whose interval has elapsed, returning whether
    /// any did. This is what turns a screen capture into a video source: nothing
    /// upstream wakes it, so the clock does.
    ///
    /// A standby holds no schedule at all. It must not run the graph, and a
    /// deadline it kept while idle would fire a burst the moment it took over.
    pub fn mark_due_ticks(&mut self) -> bool {
        if !self.is_owner {
            self.due.clear();
            return false;
        }
        let now = Instant::now();
        let clocked: Vec<(NodeId, Duration)> = self.executor.clocked_nodes().collect();
        self.due.retain(|id, _| clocked.iter().any(|(c, _)| c == id));
        let mut fired = false;
        for (id, interval) in clocked {
            // A node seen for the first time is due immediately: a source should
            // produce its first value without waiting out a period.
            let deadline = *self.due.entry(id).or_insert(now);
            if deadline > now {
                continue;
            }
            // Downstream too: the tick is only useful because it reaches what it
            // drives.
            self.executor.mark_dirty_downstream(id);
            // Count from the deadline that was served, so a steady rate does not
            // drift -- but never from the past, or a slow pass would queue up
            // ticks it can never catch up with.
            self.due.insert(id, (deadline + interval).max(now));
            fired = true;
        }
        fired
    }

    /// How long the event loop may block: the soonest clock deadline, capped by
    /// `cap`. Without the cap an idle graph would still wake on nothing; without
    /// the deadline a 30 Hz timer would be served at the polling interval.
    pub fn next_wait(&self, cap: Duration) -> Duration {
        let now = Instant::now();
        self.due
            .values()
            .map(|deadline| deadline.saturating_duration_since(now))
            .min()
            .map_or(cap, |wait| wait.min(cap))
    }

    /// Applies the result of one node's deferred work, resuming the downstream
    /// nodes it was holding back.
    pub fn deliver(&mut self, node_id: NodeId, result: AsyncResult) -> DeferredWork {
        let deferred = match result {
            Ok(outputs) => match self.executor.deliver_async_result(node_id, outputs) {
                Ok(deferred) => deferred,
                Err(e) => {
                    eprintln!("[runner] pass failed after {node_id}: {e}");
                    DeferredWork::new()
                }
            },
            Err(message) => {
                // Marking the node clears its pending state, so a re-trigger
                // can retry it instead of the node hanging forever.
                self.executor.mark_error(node_id, message);
                DeferredWork::new()
            }
        };
        self.after_pass();
        deferred
    }

    /// Reporting and publishing shared by a normal pass and an async delivery.
    fn after_pass(&mut self) {
        self.log_errors();
        self.publish();
        // Traffic, after the values it carried: an editor that draws a particle
        // has to have the value the particle stands for.
        let delivered = self.executor.take_delivered();
        if self.is_owner {
            let mut seen = HashSet::new();
            for edge in delivered {
                // Two writes to one edge in one pass are two messages, but one
                // particle is all a viewer can see of them.
                if !seen.insert(edge) {
                    continue;
                }
                let seq = self.next_seq();
                self.emit(
                    TOPIC_EDGE,
                    RuntimeEvent::Edge {
                        seq,
                        edge_id: edge.0,
                    },
                );
            }
        }
        self.refresh_frames();
    }

    /// Fires one node once, on an editor's request.
    ///
    /// Returns whether the graph changed, so the host loop knows to run a pass.
    /// A standby ignores the request: the owner received the same push and is
    /// firing it, and a press this process replayed after a takeover would fire
    /// twice.
    pub fn trigger(&mut self, node_id: u64) -> bool {
        let id = NodeId(node_id);
        if !self.is_owner || self.executor.graph.node(id).is_none() {
            eprintln!("[runner] ignoring trigger for {id}");
            return false;
        }
        eprintln!("[runner] firing {id}");
        // The press itself is the signal; the value only has to arrive.
        let _ = self.executor.set_parameter(id, "fire", Value::new(true));
        true
    }

    /// The sequence number of the next event this process sends.
    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Publishes one event on `topic`. A failure is reported once per distinct
    /// message: a broken publisher fails on every event, and a log line per
    /// event would bury everything else.
    fn emit(&mut self, topic: &str, event: RuntimeEvent) {
        let Some(publisher) = &self.publisher else {
            return;
        };
        if let Err(e) = publisher.publish(topic, event.encode()) {
            let text = e.to_string();
            if self.logged_publish.as_deref() != Some(text.as_str()) {
                eprintln!("[runner] cannot publish on {topic}: {text}");
                self.logged_publish = Some(text);
            }
        }
    }

    fn apply_node_upsert(&mut self, nd: NodeData) {
        let id = NodeId(nd.id);
        if self.executor.graph.node(id).is_some() {
            // Position has no bearing on execution, but the graph node carries
            // it and a stale copy would be written back by anything that
            // serializes the graph.
            if let Some(gn) = self.executor.graph.node_mut(id) {
                gn.position = (nd.x, nd.y);
            }
            self.apply_params(id, &nd.type_id, &nd.params);
            return;
        }

        let Some(exec) = self.plugins.iter().find_map(|p| p.create_node(&nd.type_id)) else {
            if self.unknown_types.insert(nd.id) {
                eprintln!(
                    "[runner] unknown node type '{}' for {id}: it will not execute here",
                    nd.type_id
                );
            }
            return;
        };
        let pin_defs = exec.pin_definitions().to_vec();
        self.executor.graph.add_node(GraphNode {
            id,
            type_id: nd.type_id.clone(),
            config: NodeConfig::default(),
            pin_defs,
            position: (nd.x, nd.y),
        });
        // Marks the node dirty, so the pass after this batch runs it.
        self.executor.register_node(id, exec);

        // The node instance already holds its own setting defaults; the stored
        // params are the user's deviations from them.
        self.apply_params(id, &nd.type_id, &nd.params);
    }

    fn apply_params(&mut self, id: NodeId, type_id: &str, params: &[(String, String)]) {
        for (name, text) in params {
            if let Some(value) = param_value(type_id, text) {
                let _ = self.executor.set_parameter(id, name, value);
            }
        }
    }

    fn apply_node_remove(&mut self, id: NodeId) {
        self.executor.remove_node(id);
        self.published.remove(&id);
        self.logged_errors.remove(&id);
        self.unknown_types.remove(&id.0);
    }

    /// Retries the edges whose endpoints were missing when they arrived. An
    /// edge that is still unresolvable stays queued, because the node row it
    /// names may simply be in a later batch.
    fn resolve_pending_edges(&mut self) {
        if self.pending_edges.is_empty() {
            return;
        }
        for ed in std::mem::take(&mut self.pending_edges) {
            self.apply_edge_insert(ed);
        }
    }

    fn apply_edge_insert(&mut self, ed: EdgeData) {
        let edge_id = EdgeId(ed.id);
        if self.executor.graph.edge(edge_id).is_some() {
            return;
        }
        let from_node = NodeId(ed.from_node);
        let to_node = NodeId(ed.to_node);
        // A subscription applies as one burst of row callbacks with no ordering
        // between tables, so an edge routinely arrives before the nodes it
        // names. Dropping it would lose that wire for the life of the process --
        // the row never changes again, so no second event would ever bring it
        // back -- hence it waits for its endpoints instead.
        if self.executor.graph.node(from_node).is_none()
            || self.executor.graph.node(to_node).is_none()
        {
            self.pending_edges.push(ed);
            return;
        }
        let from_pin: Arc<str> = Arc::from(ed.from_pin.as_str());
        let to_pin: Arc<str> = Arc::from(ed.to_pin.as_str());

        // An input pin holds at most one edge; a re-route arrives as a new edge
        // without a removal for the old one.
        let stale: Vec<EdgeId> = self
            .executor
            .graph
            .incoming_edges(to_node)
            .iter()
            .copied()
            .filter(|eid| {
                self.executor
                    .graph
                    .edge(*eid)
                    .is_some_and(|e| e.to_pin == to_pin)
            })
            .collect();
        for eid in stale {
            self.executor.disconnect_edge(eid);
        }

        self.executor.graph.add_edge(GraphEdge {
            id: edge_id,
            from_node,
            from_pin,
            to_node,
            to_pin,
            semantic: EdgeSemantic::default(),
        });
        // Seeds the edge from the source's cached output and dirties only the
        // target's subtree, so the source is not re-run (no duplicate LLM call
        // or capture just because a wire appeared).
        self.executor.on_edge_added(edge_id);
        // A variadic target (e.g. merge) grows an input once the last one fills.
        self.executor.sync_node_pins(to_node);
    }

    fn apply_edge_remove(&mut self, id: EdgeId) {
        self.pending_edges.retain(|ed| ed.id != id.0);
        let to_node = self.executor.graph.edge(id).map(|e| e.to_node);
        self.executor.disconnect_edge(id);
        if let Some(to_node) = to_node {
            self.executor.sync_node_pins(to_node);
        }
    }

    /// Publishes this pass's scalar outputs so editors can display them.
    ///
    /// Only what changed is sent: a capture graph runs at frame rate and an
    /// unchanged value costs nothing. A pin that lost its value is reported as
    /// cleared, because absence is a state a viewer has to be able to reach --
    /// otherwise a stale number would outlive the run that produced it.
    ///
    /// The snapshot is rebuilt whenever anything was emitted, so an editor that
    /// joins between two passes sees the same set the live events describe.
    fn publish(&mut self) {
        if !self.is_owner {
            return;
        }
        let mut emitted = false;
        for id in self.executor.graph.node_ids().collect::<Vec<_>>() {
            let Some(node) = self.executor.graph.node(id) else {
                continue;
            };
            let current: HashMap<String, (String, String)> = node
                .pin_defs
                .iter()
                .filter(|p| p.direction == PinDirection::Output)
                .filter_map(|p| {
                    let value = self.executor.output_value(id, &p.name)?;
                    let (ty, text) = encode_scalar(value)?;
                    Some((p.name.to_string(), (ty, text)))
                })
                .collect();
            let previous = self.published.get(&id);
            if previous == Some(&current) {
                continue;
            }
            // Collected while `previous` is borrowed, emitted after: the diff
            // reads `published` and emitting takes the publisher mutably.
            let mut events: Vec<(String, Option<(String, String)>)> = Vec::new();
            if let Some(prev) = previous {
                for pin in prev.keys() {
                    if !current.contains_key(pin) {
                        events.push((pin.clone(), None));
                    }
                }
            }
            for (pin, (ty, text)) in &current {
                let unchanged = previous
                    .and_then(|prev| prev.get(pin))
                    .is_some_and(|(was_ty, was_text)| was_ty == ty && was_text == text);
                if unchanged {
                    continue;
                }
                events.push((pin.clone(), Some((ty.clone(), text.clone()))));
            }
            self.published.insert(id, current);
            for (pin, value) in events {
                let seq = self.next_seq();
                let event = match value {
                    Some((ty, value)) => RuntimeEvent::Output {
                        seq,
                        node_id: id.0,
                        pin,
                        ty,
                        value,
                    },
                    None => RuntimeEvent::OutputCleared {
                        seq,
                        node_id: id.0,
                        pin,
                    },
                };
                self.emit(TOPIC_OUTPUT, event);
                emitted = true;
            }
        }
        if emitted {
            self.rebuild_snapshot();
        }
    }

    /// Restates the whole output set for editors that join later.
    ///
    /// Rebuilt rather than patched: the snapshot has to be exactly what the
    /// diff baseline says, or a late editor would inherit a value the live
    /// events already cleared.
    fn rebuild_snapshot(&mut self) {
        let outputs: Vec<OutputRow> = self
            .published
            .iter()
            .flat_map(|(id, pins)| {
                pins.iter().map(move |(pin, (ty, value))| OutputRow {
                    node_id: id.0,
                    pin: pin.clone(),
                    ty: ty.clone(),
                    value: value.clone(),
                })
            })
            .collect();
        *self.snapshot.lock().unwrap_or_else(|e| e.into_inner()) = Snapshot {
            seq: self.seq,
            outputs,
        };
    }

    /// Records the pinned URL this process serves on and announces it.
    ///
    /// Called once the listener is bound and never before: an editor that
    /// reached a runtime which is not serving yet would fail its first request
    /// and have no reason to try again.
    pub fn set_endpoint(&mut self, url: String) {
        self.endpoint = Some(url);
        self.announce_endpoint();
    }

    /// Writes the endpoint into this runtime's row. Idempotent, which is what
    /// lets it be repeated whenever ownership or the subscription changes.
    fn announce_endpoint(&self) {
        let Some(url) = &self.endpoint else {
            return;
        };
        zeughaus_sync::send_announce_endpoint(&self.conn, url);
    }

    /// Hands this pass's frames to the feed server.
    ///
    /// Frames only: a scalar already reaches every editor as a `node_output`
    /// row, and 33 MB of pixels must not go the same way. What counts as a
    /// frame pin is the graph's own statement -- an output pin declaring
    /// [`Image`] -- rather than a guess from the value, so a pin that has not
    /// produced anything yet is still served (a viewer attaching before the
    /// first capture waits instead of being told the feed is over).
    ///
    /// The whole set is restated every pass because the registry is
    /// authoritative: a pin missing from it is a pin whose node is gone, which
    /// is both how its frame is released and how its viewers learn to stop.
    fn refresh_frames(&self) {
        if !self.is_owner {
            return;
        }
        let frame_ty = Ty::of::<Image>();
        let mut pins: Vec<(NodeId, &str, Option<&Image>)> = Vec::new();
        for id in self.executor.graph.node_ids() {
            let Some(node) = self.executor.graph.node(id) else {
                continue;
            };
            for pin in &node.pin_defs {
                if pin.direction != PinDirection::Output || pin.ty != frame_ty {
                    continue;
                }
                let image = self
                    .executor
                    .output_value(id, &pin.name)
                    .and_then(Value::downcast_ref::<Image>);
                pins.push((id, &pin.name, image));
            }
        }
        self.frames.publish(pins);
    }

    /// Reports node failures and recoveries once each. A headless process has
    /// no status bar, so this log is the only place a broken node surfaces.
    fn log_errors(&mut self) {
        let current: HashMap<NodeId, String> = self
            .executor
            .errors()
            .map(|(id, msg)| (id, msg.to_string()))
            .collect();
        for (id, message) in &current {
            if self.logged_errors.get(id) != Some(message) {
                eprintln!("[runner] {id} failed: {message}");
            }
        }
        for id in self.logged_errors.keys() {
            if !current.contains_key(id) {
                eprintln!("[runner] {id} recovered");
            }
        }
        self.logged_errors = current;
    }

    fn log_size(&mut self) {
        let size = (
            self.executor.graph.node_count(),
            self.executor.graph.edge_count(),
        );
        if size == self.logged_size {
            return;
        }
        self.logged_size = size;
        eprintln!("[runner] graph: {} nodes, {} edges", size.0, size.1);
    }
}

/// The value a stored parameter string becomes for a node of this type.
///
/// Const nodes carry their payload as text in `params`, so it has to be parsed
/// back into the type their output pin declares: a `const_f64` handed a string
/// would emit a string, and every coercion downstream of it would be working
/// from a lie. Everything else takes settings as text, which is what a
/// `SettingDef` promises its node.
///
/// `None` means the text is not a value of that type at all (`"abc"` as an
/// f64). The parameter is then skipped, leaving the node at its previous value
/// rather than silently substituting zero -- the editor shows the text the user
/// is still typing, and a half-typed number must not become one.
fn param_value(type_id: &str, text: &str) -> Option<Value> {
    match type_id {
        "transform.const_f64" => text.parse::<f64>().ok().map(Value::new),
        // Any other text is false, matching the editor's checkbox round-trip.
        "transform.const_bool" => Some(Value::new(text == "true" || text == "1")),
        _ => Some(Value::new(text.to_string())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn const_f64_param_parses_to_float() {
        let value = param_value("transform.const_f64", "1.5").expect("parsable float");
        assert_eq!(value.downcast_ref::<f64>(), Some(&1.5));
    }

    #[test]
    fn unparsable_const_f64_param_is_skipped() {
        assert!(param_value("transform.const_f64", "1.").is_some());
        assert!(param_value("transform.const_f64", "").is_none());
        assert!(param_value("transform.const_f64", "abc").is_none());
    }

    #[test]
    fn const_bool_param_parses_to_bool() {
        for text in ["true", "1"] {
            let value = param_value("transform.const_bool", text).expect("bool");
            assert_eq!(value.downcast_ref::<bool>(), Some(&true));
        }
        for text in ["false", "0", ""] {
            let value = param_value("transform.const_bool", text).expect("bool");
            assert_eq!(value.downcast_ref::<bool>(), Some(&false));
        }
    }

    #[test]
    fn const_string_param_stays_text() {
        let value = param_value("transform.const_string", "42").expect("string");
        assert_eq!(value.downcast_ref::<String>(), Some(&"42".to_string()));
    }

    #[test]
    fn setting_of_any_other_node_stays_text() {
        // A setting is a string by contract, even when it reads like a number.
        let value = param_value("llm.chat", "0.7").expect("setting");
        assert_eq!(value.downcast_ref::<String>(), Some(&"0.7".to_string()));
    }
}
