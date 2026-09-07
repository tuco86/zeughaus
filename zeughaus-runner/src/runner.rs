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
use std::sync::Arc;
use std::time::{Duration, Instant};

use zeughaus_core::{
    DomainPlugin, EdgeData, EdgeId, EdgeSemantic, Image, NodeConfig, NodeData, NodeId,
    PinDirection, Ty, TypeConverters, Value, encode_scalar,
};
use zeughaus_runtime::{DeferredWork, Graph, GraphEdge, GraphExecutor, GraphNode};
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
    triggers: TriggerLog,
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
    pub fn new(conn: DbConnection, frames: Arc<FrameRegistry>) -> Self {
        // The same plugin set the native editor registers. Both sides must agree
        // on what exists and what may connect: the editor validates a drag
        // against these converters, this process coerces the value that then
        // crosses the edge.
        let plugins: Vec<Box<dyn DomainPlugin>> = vec![
            Box::new(zeughaus_transform::TransformPlugin),
            Box::new(zeughaus_ml::MlPlugin),
            Box::new(zeughaus_flow::FlowPlugin),
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
            triggers: TriggerLog::default(),
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
            return;
        }
        // Ownership was just inherited, so this process has never run this
        // graph: every node is stale.
        for id in self.executor.graph.node_ids().collect::<Vec<_>>() {
            self.executor.mark_dirty(id);
        }
        // The rows the previous owner left are what every viewer is showing
        // right now, so they are the baseline this runner's first pass is
        // diffed against. Starting from nothing instead would leave a value
        // this process does not produce sitting in the store forever: the diff
        // would have no record of the pin, so nothing would ever clear it.
        self.published = store_outputs(&self.conn);
    }

    pub fn apply(&mut self, event: SyncEvent) {
        match event {
            SyncEvent::NodeUpsert(nd) => self.apply_node_upsert(nd),
            SyncEvent::NodeRemove(id) => self.apply_node_remove(NodeId(id)),
            SyncEvent::EdgeInsert(ed) => self.apply_edge_insert(ed),
            SyncEvent::EdgeRemove(id) => self.apply_edge_remove(EdgeId(id)),
            // Ownership is read from the client cache, not from the event.
            SyncEvent::RuntimesChanged => {}
            // This process is the producer of those rows; it has no use for
            // them, and adopting its own publications would overwrite freshly
            // computed values with their echo.
            SyncEvent::OutputsChanged => {}
            // A press only counts if it happened while this process was alive.
            // The rows the snapshot already held are adopted below, so the same
            // event arriving here compares equal and fires nothing.
            SyncEvent::TriggerRequested { node_id, count } => {
                self.triggers.request(node_id, count)
            }
            // The snapshot's presses belong to whichever runtime was alive when
            // they were made. Read from the cache rather than inferred from
            // event order: the SDK does not specify whether `on_applied` runs
            // before or after the row callbacks, and it turned out to run
            // first -- so a flag flipped here would have adopted nothing and
            // every restart re-pressed every button.
            SyncEvent::SubscriptionApplied => {
                for (node_id, count) in zeughaus_sync::pending_triggers(&self.conn) {
                    self.triggers.adopt(node_id, count);
                }
                // The snapshot is also the first look this process gets at its
                // own `runtime` row, and a reconnect recreates that row without
                // the endpoint. Re-announcing here is what keeps an editor from
                // resolving an owning runtime with an empty address.
                self.announce_endpoint();
            }
        }
    }

    /// Runs one pass: fire pending presses, execute the dirty nodes, publish
    /// what changed. Returns the work that has to run off-thread.
    ///
    /// A standby does nothing at all. Executing "just the cheap nodes" would
    /// double-run every side effect in the graph, which is the split brain the
    /// single-owner rule exists to remove.
    pub fn pass(&mut self) -> DeferredWork {
        self.resolve_pending_edges();
        self.fire_pending_triggers();
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
        self.refresh_frames();
    }

    /// Fires every press that has not been handled yet.
    ///
    /// A standby marks them handled without firing: the owner is firing them
    /// right now, and a later takeover must not replay a press that already
    /// ran. A press for a node this process does not have stays unhandled --
    /// the node row may simply not have arrived yet, and the next batch (the one
    /// carrying it) fires it.
    fn fire_pending_triggers(&mut self) {
        let due: Vec<u64> = self
            .triggers
            .pending()
            .filter(|id| self.executor.graph.node(NodeId(*id)).is_some())
            .collect();
        for raw in due {
            self.triggers.mark_handled(raw);
            if !self.is_owner {
                continue;
            }
            let id = NodeId(raw);
            eprintln!("[runner] firing {id}");
            // The press itself is the signal; the value only has to arrive.
            let _ = self.executor.set_parameter(id, "fire", Value::new(true));
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
        self.triggers.forget(id.0);
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
    /// Only what changed is sent. A pin that lost its value forces a clear of
    /// that node's rows before the remaining pins are re-published, because
    /// absence is a state a viewer has to be able to reach -- otherwise a stale
    /// number would outlive the run that produced it.
    fn publish(&mut self) {
        if !self.is_owner {
            return;
        }
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
            let dropped =
                previous.is_some_and(|prev| prev.keys().any(|pin| !current.contains_key(pin)));
            if dropped {
                zeughaus_sync::send_clear_node_outputs(&self.conn, id.0);
            }
            for (pin, (ty, text)) in &current {
                let unchanged = !dropped
                    && previous.and_then(|prev| prev.get(pin)) == Some(&(ty.clone(), text.clone()));
                if unchanged {
                    continue;
                }
                zeughaus_sync::send_publish_output(&self.conn, id.0, pin, ty, text);
            }
            self.published.insert(id, current);
        }
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

/// The published outputs currently in the store, in the shape
/// [`Runner::published`] diffs against.
fn store_outputs(conn: &DbConnection) -> HashMap<NodeId, HashMap<String, (String, String)>> {
    let mut per_node: HashMap<NodeId, HashMap<String, (String, String)>> = HashMap::new();
    for (node_id, pin, ty, value) in zeughaus_sync::published_outputs(conn) {
        per_node
            .entry(NodeId(node_id))
            .or_default()
            .insert(pin, (ty, value));
    }
    per_node
}

/// Which manual-trigger count has been requested and which has been handled,
/// per node.
///
/// The store keeps one row per node holding a monotonic press count, and a
/// re-subscription replays that row. Firing on every event would turn one press
/// into one press per reconnect, so the count that was handled is what a new
/// event is compared against. Requests are kept apart from the fired ones
/// because a press can arrive before the node it names.
#[derive(Default)]
pub struct TriggerLog {
    requested: HashMap<u64, u64>,
    handled: HashMap<u64, u64>,
}

impl TriggerLog {
    /// Records a press. The count only ever moves forward: an out-of-order
    /// replay of an older row must not undo a newer press.
    pub fn request(&mut self, node_id: u64, count: u64) {
        let slot = self.requested.entry(node_id).or_insert(count);
        *slot = (*slot).max(count);
    }

    /// Records a press as already handled, without firing it. Used for the rows
    /// the store already held when this process connected.
    pub fn adopt(&mut self, node_id: u64, count: u64) {
        self.request(node_id, count);
        self.mark_handled(node_id);
    }

    /// Nodes with a press that has not been handled yet.
    pub fn pending(&self) -> impl Iterator<Item = u64> + '_ {
        self.requested
            .iter()
            .filter(|(node_id, count)| self.handled.get(node_id) < Some(count))
            .map(|(node_id, _)| *node_id)
    }

    /// Marks the node's outstanding press as handled.
    pub fn mark_handled(&mut self, node_id: u64) {
        if let Some(count) = self.requested.get(&node_id) {
            self.handled.insert(node_id, *count);
        }
    }

    /// Drops a removed node's history. Node ids are process-unique, so nothing
    /// can inherit the count.
    pub fn forget(&mut self, node_id: u64) {
        self.requested.remove(&node_id);
        self.handled.remove(&node_id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending(log: &TriggerLog) -> Vec<u64> {
        let mut ids: Vec<u64> = log.pending().collect();
        ids.sort_unstable();
        ids
    }

    #[test]
    fn first_press_is_pending() {
        let mut log = TriggerLog::default();
        log.request(7, 1);
        assert_eq!(pending(&log), vec![7]);
    }

    #[test]
    fn replayed_row_does_not_refire() {
        let mut log = TriggerLog::default();
        log.request(7, 3);
        log.mark_handled(7);
        assert!(pending(&log).is_empty());

        // A re-subscription replays the same row.
        log.request(7, 3);
        assert!(pending(&log).is_empty());
    }

    /// A runner that starts into a graph with an old press must not fire it: the
    /// press was aimed at whichever runtime was alive at the time.
    #[test]
    fn adopted_press_never_fires() {
        let mut log = TriggerLog::default();
        log.adopt(7, 4);
        assert!(pending(&log).is_empty());

        // A real press after startup still counts.
        log.request(7, 5);
        assert_eq!(pending(&log), vec![7]);
    }

    #[test]
    fn later_press_fires_again() {
        let mut log = TriggerLog::default();
        log.request(7, 1);
        log.mark_handled(7);
        log.request(7, 2);
        assert_eq!(pending(&log), vec![7]);
        log.mark_handled(7);
        assert!(pending(&log).is_empty());
    }

    #[test]
    fn stale_replay_after_newer_press_is_ignored() {
        let mut log = TriggerLog::default();
        log.request(7, 5);
        log.mark_handled(7);
        // An older count arriving late must not resurrect a handled press.
        log.request(7, 4);
        assert!(pending(&log).is_empty());
    }

    #[test]
    fn presses_are_tracked_per_node() {
        let mut log = TriggerLog::default();
        log.request(1, 1);
        log.request(2, 1);
        log.mark_handled(1);
        assert_eq!(pending(&log), vec![2]);
    }

    #[test]
    fn forgotten_node_has_no_pending_press() {
        let mut log = TriggerLog::default();
        log.request(9, 1);
        log.forget(9);
        assert!(pending(&log).is_empty());
    }

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
