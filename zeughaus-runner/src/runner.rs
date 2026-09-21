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

use weida::Publisher;
use zeughaus_core::{
    DomainPlugin, EdgeData, EdgeId, EdgeSemantic, Image, NodeConfig, NodeData, NodeId,
    PinDirection, Ty, TypeConverters, Value, encode_scalar, occupancy_winner,
};
use zeughaus_runtime::{DeferredWork, Graph, GraphEdge, GraphExecutor, GraphNode};
use zeughaus_link::{
    ErrorRow, OutputRow, RejectionRow, RuntimeEvent, Snapshot, TOPIC_EDGE, TOPIC_ERROR,
    TOPIC_OUTPUT,
};
use zeughaus_sync::{Store, SyncEvent};

use crate::feed::FrameRegistry;

/// Outcome of one node's deferred work, as reported back by the host loop.
pub type AsyncResult = Result<HashMap<String, Value>, String>;

pub struct Runner {
    /// The store connection, and the retry behind it. An `Option` inside,
    /// because "there is no store right now" is a state this process has to
    /// handle rather than one it can pretend away.
    store: Store,
    plugins: Vec<Box<dyn DomainPlugin>>,
    executor: GraphExecutor,
    /// Whether this process currently owns execution. Read back from the store
    /// on every batch rather than remembered, because ownership changes by
    /// another runner disappearing, not by a handshake.
    is_owner: bool,
    /// Which generation of ownership this process is on. Bumped every time
    /// ownership is gained, so deferred work dispatched under a previous one is
    /// recognizable when its result comes back. See [`Runner::deliver`].
    owner_epoch: u64,
    /// Role and runner count as last reported. A headless standby that logged
    /// nothing would be indistinguishable from one that is stuck, so the role is
    /// stated as soon as it is known and again whenever it or the number of
    /// connected runners changes.
    logged_role: Option<(bool, usize)>,
    /// What editors have been told: the diff baseline and the snapshot derived
    /// from it. Only what changed is sent, because a capture graph runs at
    /// frame rate and an unchanged value must cost nothing.
    published: Published,
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
    /// The parameter text this process last handed each node, so a row rewrite
    /// that changed nothing (a drag, a keystroke in another field) does not
    /// re-run the node. See [`Runner::apply_params`].
    applied_params: AppliedParams,
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
        store: Store,
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
            Box::new(zeughaus_db::DbPlugin),
            Box::new(zeughaus_record::RecordPlugin),
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
            store,
            plugins,
            executor,
            is_owner: false,
            owner_epoch: 0,
            logged_role: None,
            published: Published::default(),
            publisher,
            seq: 0,
            snapshot,
            logged_publish: None,
            pending_edges: Vec::new(),
            due: HashMap::new(),
            logged_errors: HashMap::new(),
            applied_params: AppliedParams::default(),
            unknown_types: HashSet::new(),
            logged_size: (0, 0),
            frames,
            endpoint: None,
        }
    }

    /// Reconnects the store if it went away, so an outage costs this process
    /// nothing more than the outage.
    ///
    /// Called once per turn of the host loop: the retry has to be somebody's
    /// decision, and it is the loop that also knows when to stop.
    pub fn poll_store(&mut self) {
        self.store.poll();
    }

    /// Re-reads who owns execution, and reports the role a human debugging this
    /// process needs before anything else.
    pub fn refresh_ownership(&mut self) {
        let Some(conn) = self.store.conn() else {
            // Without the store this process cannot learn that another runner
            // took over -- and one has: the module drops a runtime's row when
            // its client disconnects, so the graph belongs to somebody else by
            // now. Executing on would be two owners with one graph.
            self.lose_ownership();
            return;
        };
        let runners = zeughaus_sync::runtime_count(conn);
        if runners == 0 {
            // The runtime table has not reached this client yet, so the store
            // has said nothing about who executes. Staying a non-owner is the
            // safe reading: a runner that assumed ownership here would
            // double-execute a graph another one is already running.
            return;
        }
        let owner = self.store.conn().is_some_and(zeughaus_sync::is_owner);
        if self.logged_role != Some((owner, runners)) {
            self.logged_role = Some((owner, runners));
            if owner {
                eprintln!("[runner] executing this session ({runners} runner(s) connected)");
            } else {
                eprintln!("[runner] standby ({runners} runners connected): another one executes");
            }
        }
        if owner == self.is_owner {
            return;
        }
        if !owner {
            self.lose_ownership();
            return;
        }
        self.is_owner = true;
        // Ownership decides where editors are pointed: `owner_endpoint`
        // resolves the *owning* runtime's row, so a standby inheriting
        // execution becomes the row every editor reads and has to be sure its
        // address is in it.
        self.announce_endpoint();
        // A new generation of ownership: work this process dispatched under the
        // previous one belongs to a graph another runner has been executing
        // since, so its result is no longer this owner's to apply.
        self.owner_epoch += 1;
        // Ownership was just inherited, so this process has never run this
        // graph: every node is stale.
        for id in self.executor.graph.node_ids().collect::<Vec<_>>() {
            self.executor.mark_dirty(id);
        }
        // Nothing was published by this process, so its first pass republishes
        // everything. Values do not live in the store any more, so there is no
        // predecessor's row left behind to diff against and nothing to clear.
        self.published.clear();
        self.published.flush(self.seq, &self.snapshot);
    }

    /// Stops executing: another runtime has the graph, or this process can no
    /// longer tell that it does not.
    ///
    /// One path for both, because what has to happen is the same. The frames
    /// this process holds are the last ones it produced and another runtime is
    /// producing the real ones now, so its viewers are sent away rather than
    /// shown a still picture; and nothing it published is current, so the
    /// snapshot it serves must not answer with values it no longer produces.
    fn lose_ownership(&mut self) {
        if !self.is_owner {
            return;
        }
        self.is_owner = false;
        self.frames.clear();
        self.published.clear();
        self.published.flush(self.seq, &self.snapshot);
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
            // A reconnect is a new client to the store: a new `runtime` row
            // with no address in it, and a new position in the ownership
            // order. Announcing again is what keeps an editor from resolving
            // an owning runtime it cannot dial.
            SyncEvent::Connected => self.announce_endpoint(),
            // The store is gone, so the module has already dropped this
            // process's `runtime` row and handed the graph to the next runner.
            // Executing on would be two owners with one graph.
            SyncEvent::Disconnected => self.lose_ownership(),
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
        self.due
            .retain(|id, _| clocked.iter().any(|(c, _)| c == id));
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

    /// Which generation of ownership this process is currently executing under.
    ///
    /// Dispatched work carries it, so a result that comes back after ownership
    /// moved can be told from one this owner is still waiting for.
    pub fn owner_epoch(&self) -> u64 {
        self.owner_epoch
    }

    /// Applies the result of one node's deferred work, resuming the downstream
    /// nodes it was holding back.
    ///
    /// A result from an earlier ownership generation is dropped: delivering it
    /// runs every node downstream of it, and doing that while another runner
    /// owns the graph is the split brain the single-owner rule exists to
    /// prevent -- two processes inserting the same row, writing the same frame
    /// or issuing the same request. The node's pending state is cleared
    /// anyway, so if this process owns the graph again the node is retried
    /// rather than left waiting for a result that has been thrown away.
    pub fn deliver(&mut self, node_id: NodeId, epoch: u64, result: AsyncResult) -> DeferredWork {
        if !accepts_async_result(self.is_owner, self.owner_epoch, epoch) {
            eprintln!(
                "[runner] dropping {node_id}'s result from epoch {epoch} (now {}, owner: {})",
                self.owner_epoch, self.is_owner
            );
            self.executor.clear_pending(node_id);
            return DeferredWork::new();
        }
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
        // Failures are state like a value is, and they travel the same way: the
        // process that ran the node is not the process drawing it.
        self.publish_errors();
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
        // The press itself is the signal; the value only has to arrive. A node
        // that refuses it did not fire, so asking for a pass would report a
        // press that never happened -- and the editor that pressed has to be
        // told, which is what the node error is for.
        if let Err(e) = self.executor.set_parameter(id, "fire", Value::new(true)) {
            let message = format!("the trigger was refused: {e}");
            eprintln!("[runner] {id}: {message}");
            self.executor.report_error(id, message);
            return false;
        }
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

    /// Hands the node the parameters that actually changed.
    ///
    /// A node row is rewritten for reasons that have nothing to do with its
    /// settings -- a drag writes `x`/`y`, and the editor restates the whole
    /// parameter set on every keystroke -- and `set_parameter` marks the node
    /// and its entire downstream subtree dirty. Applying a value that did not
    /// change therefore re-ran the node: one LLM request per keystroke in a
    /// chat node's `model` field, one screen capture per drag. Diffing against
    /// what this process last applied is what makes a move cost nothing.
    fn apply_params(&mut self, id: NodeId, type_id: &str, params: &[(String, String)]) {
        let changed = self.applied_params.changed(id, params);
        if changed.is_empty() {
            return;
        }
        for (name, text) in changed {
            // Two ways a value does not arrive: the text is not a value of the
            // node's parameter type at all, or the node refuses it. Either way
            // the node keeps what it had, which is a node whose setting on
            // screen is not the setting it runs on -- so the editor that typed
            // it, and every other window, is told which setting and why.
            //
            // Its own event rather than a node error: nothing failed, the node
            // goes on running with the value it kept, and raising the failure
            // alarm for it put a red border around a working node and an
            // `ERROR:` line in every editor's status bar.
            let refusal = match param_value(type_id, &text) {
                None => Some(format!("{text:?} is not a value this node takes")),
                Some(value) => self
                    .executor
                    .set_parameter(id, &name, value)
                    .err()
                    .map(|e| e.to_string()),
            };
            match refusal {
                Some(message) => {
                    eprintln!("[runner] {id} ({type_id}) refused {name}={text:?}: {message}");
                    if self.published.reject(id, &name, message.clone()) && self.is_owner {
                        let seq = self.next_seq();
                        let event = RuntimeEvent::SettingRejected {
                            seq,
                            node_id: id.0,
                            key: name.clone(),
                            message,
                        };
                        self.emit(TOPIC_ERROR, event);
                    }
                }
                // The value arrived, so whatever was refused for this setting
                // is over. Said explicitly, because an editor cannot infer it:
                // a node that runs cleanly with a setting it once refused
                // never mentions that setting again.
                None => {
                    if self.published.accept(id, &name) && self.is_owner {
                        let seq = self.next_seq();
                        let event = RuntimeEvent::SettingAccepted {
                            seq,
                            node_id: id.0,
                            key: name.clone(),
                        };
                        self.emit(TOPIC_ERROR, event);
                    }
                }
            }
        }
        self.published.flush(self.seq, &self.snapshot);
        // A setting can decide a node's pins (a table's column list is one), so
        // the graph's declaration is re-read once the parameters are in. Without
        // it the edges into a node the editor already drew with new pins would
        // find nothing to attach to here.
        self.executor.refresh_pins(id);
    }

    fn apply_node_remove(&mut self, id: NodeId) {
        self.executor.remove_node(id);
        // The next `publish` only walks the nodes the graph still has, so it
        // has no reason to say anything about this one. Flushing here is what
        // keeps an editor that joins afterwards from being handed the outputs
        // of a node that is gone; the live stream needs nothing, because a
        // subscriber sees the node row disappear from the store.
        self.published.forget(id);
        self.published.flush(self.seq, &self.snapshot);
        self.logged_errors.remove(&id);
        self.applied_params.forget(id);
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

        // A data input pin holds at most one edge; a re-route arrives as a new
        // edge without a removal for the old one, and two editors can each
        // have drawn one. `incoming_edges` yields only data edges, so a
        // relation is untouched here -- a field referenced by several others
        // is the normal case for a primary key.
        //
        // The verdict is [`occupancy_winner`], the same rule every editor
        // applies, so this process executes the wire the windows draw. By
        // arrival order it could have kept the other one and then run the
        // graph nobody was looking at. An editor deletes the losing row; until
        // that arrives, dropping it locally is enough, and the removal event
        // then finds nothing left to do.
        let mut contenders: Vec<EdgeId> = self
            .executor
            .graph
            .incoming_edges(to_node)
            .filter(|eid| {
                self.executor
                    .graph
                    .edge(*eid)
                    .is_some_and(|e| e.to_pin == to_pin)
            })
            .collect();
        if !contenders.is_empty() {
            contenders.push(edge_id);
            let winner = occupancy_winner(contenders.iter().copied()).expect("not empty");
            for loser in contenders.into_iter().filter(|id| *id != winner) {
                self.executor.disconnect_edge(loser);
            }
            if winner != edge_id {
                return;
            }
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
    /// The snapshot follows the same baseline, so an editor that joins between
    /// two passes sees the set the live events describe.
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
            let previous = self.published.get(id);
            if previous == Some(&current) {
                continue;
            }
            // Collected while `previous` is borrowed, emitted after: the diff
            // reads the baseline and emitting takes the publisher mutably.
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
            self.published.set(id, current);
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
            }
        }
        self.published.flush(self.seq, &self.snapshot);
    }

    /// Publishes which nodes are failing, and why.
    ///
    /// The only path a node error has to an editor. A viewer does not execute,
    /// so it cannot discover a failure itself: without this the report lived
    /// in this process's log and the editor's error surface -- the status bar
    /// line, the red node border -- was drawn from a set that was always
    /// empty.
    ///
    /// Diffed against the same baseline the outputs use, so a node that keeps
    /// failing for the same reason costs nothing per pass, and the snapshot a
    /// late editor is served says exactly what the live events said.
    fn publish_errors(&mut self) {
        if !self.is_owner {
            return;
        }
        let current: HashMap<NodeId, String> = self
            .executor
            .errors()
            .map(|(id, message)| (id, message.to_string()))
            .collect();
        let changes = error_changes(self.published.errors(), &current);
        if changes.is_empty() {
            return;
        }
        for (id, message) in changes {
            let seq = self.next_seq();
            let event = match message {
                Some(message) => RuntimeEvent::NodeError {
                    seq,
                    node_id: id.0,
                    message,
                },
                None => RuntimeEvent::NodeErrorCleared { seq, node_id: id.0 },
            };
            self.emit(TOPIC_ERROR, event);
        }
        self.published.set_errors(current);
        self.published.flush(self.seq, &self.snapshot);
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
        // Nothing to announce to while the store is away; the reconnect
        // announces again through `SyncEvent::Connected`.
        let Some(conn) = self.store.conn() else {
            return;
        };
        if let Err(e) = zeughaus_sync::send_announce_endpoint(conn, url) {
            eprintln!("[runner] {e}");
        }
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

/// Whether an async result dispatched under `dispatched` may still be applied.
///
/// Applying one runs every node downstream of it, so it takes the two facts
/// that make that safe: this process owns the graph, and it is the same
/// ownership it dispatched the work under. A standby fails the first, and an
/// owner that lost and regained ownership in between fails the second -- in
/// that gap another runner ran the graph, so the result describes a pass
/// nobody is waiting for any more.
///
/// Pure so it can be tested: [`Runner`] needs a live store connection, and
/// this decision must not.
fn accepts_async_result(is_owner: bool, current: u64, dispatched: u64) -> bool {
    is_owner && current == dispatched
}

/// The parameter text last handed to each node, and the answer to "what in this
/// row is new".
///
/// A stored node row carries its position and its whole parameter set
/// together, and it is rewritten for either reason: a drag writes coordinates,
/// the editor restates every parameter on every keystroke. Since
/// `set_parameter` marks the node and its downstream dirty, re-applying an
/// unchanged value is a re-run -- and for a node with side effects that is a
/// second LLM request or a second capture for no reason at all.
///
/// Text rather than `Value` because that is what the store holds and what the
/// comparison has to be exact about; the conversion happens after the diff.
#[derive(Default)]
pub struct AppliedParams {
    applied: HashMap<NodeId, HashMap<String, String>>,
}

impl AppliedParams {
    /// The parameters of this row that differ from what was last applied,
    /// recorded as applied on the way out.
    ///
    /// A key the row no longer carries is forgotten rather than reset: a node
    /// can be told a value, never that it has none, so the honest bookkeeping
    /// is to treat that same text arriving again as a change. Every current
    /// derived parameter (`db_path`, `columns`, `relations`) is idempotent, so
    /// re-applying one costs a comparison inside the node and nothing else.
    pub fn changed(&mut self, node: NodeId, params: &[(String, String)]) -> Vec<(String, String)> {
        let applied = self.applied.entry(node).or_default();
        let mut changed = Vec::new();
        for (name, text) in params {
            if applied.get(name).is_some_and(|current| current == text) {
                continue;
            }
            applied.insert(name.clone(), text.clone());
            changed.push((name.clone(), text.clone()));
        }
        applied.retain(|name, _| params.iter().any(|(n, _)| n == name));
        changed
    }

    /// Forgets a node, so an id that comes back is a node this process has
    /// told nothing.
    pub fn forget(&mut self, node: NodeId) {
        self.applied.remove(&node);
    }
}

/// What an editor has to be told to get from `published` to `current`.
///
/// `Some(message)` is a failure it does not know about yet, or one whose
/// message changed; `None` is a recovery. A node that keeps failing for the
/// same reason yields nothing, which is what makes reporting per pass free at
/// frame rate.
///
/// Sorted by node id: two runners and two passes then describe the same change
/// in the same order, and a log or a test reads the same way twice.
fn error_changes(
    published: &HashMap<NodeId, String>,
    current: &HashMap<NodeId, String>,
) -> Vec<(NodeId, Option<String>)> {
    let mut changes: Vec<(NodeId, Option<String>)> = Vec::new();
    for (id, message) in current {
        if published.get(id) != Some(message) {
            changes.push((*id, Some(message.clone())));
        }
    }
    for id in published.keys() {
        if !current.contains_key(id) {
            changes.push((*id, None));
        }
    }
    changes.sort_by_key(|(id, _)| *id);
    changes
}

/// What editors have been told, in the two forms it is needed in: the
/// baseline the next pass is diffed against, and the snapshot a late joiner is
/// served.
///
/// One type because the two must not disagree. As separate fields they did: a
/// removed node was dropped from the baseline, the next pass had nothing to
/// say about a node the graph no longer holds, and the snapshot kept serving
/// its outputs to every editor that joined afterwards.
///
/// Failures are held beside the values for exactly that reason: an editor
/// joining late has to be told which nodes are broken by the same snapshot
/// that tells it the numbers, or it would draw a graph that looks healthy
/// until the next failure happens.
#[derive(Default)]
pub struct Published {
    baseline: HashMap<NodeId, HashMap<String, (String, String)>>,
    /// Why each failing node is failing, as editors were last told.
    errors: HashMap<NodeId, String>,
    /// Why each refused setting was refused, as editors were last told, keyed
    /// by the node and the setting. Held beside the failures because a late
    /// editor has to learn both from the one snapshot, and kept apart from
    /// them because a refused setting is not a failed run.
    rejections: HashMap<(NodeId, String), String>,
    /// Whether the snapshot still matches the baseline. Rebuilding is deferred
    /// because one pass touches many nodes and the snapshot only has to be
    /// current when it is read.
    stale: bool,
}

impl Published {
    /// What this node last published, for the diff.
    pub fn get(&self, node: NodeId) -> Option<&HashMap<String, (String, String)>> {
        self.baseline.get(&node)
    }

    /// Records a node's current output set.
    pub fn set(&mut self, node: NodeId, pins: HashMap<String, (String, String)>) {
        self.baseline.insert(node, pins);
        self.stale = true;
    }

    /// Forgets a node: its outputs, its failure and its refusals go with it.
    pub fn forget(&mut self, node: NodeId) {
        let had_rejections = self.rejections.keys().any(|(id, _)| *id == node);
        if self.baseline.remove(&node).is_some() | self.errors.remove(&node).is_some()
            || had_rejections
        {
            self.stale = true;
        }
        self.rejections.retain(|(id, _), _| *id != node);
    }

    /// Records that a node refused a setting. `true` when that is news, which
    /// is what decides whether an event is published: a node asked for the
    /// same refused text again says nothing new.
    pub fn reject(&mut self, node: NodeId, key: &str, message: String) -> bool {
        let slot = self.rejections.entry((node, key.to_string())).or_default();
        if *slot == message {
            return false;
        }
        *slot = message;
        self.stale = true;
        true
    }

    /// Records that a setting is no longer refused. `true` when it was.
    pub fn accept(&mut self, node: NodeId, key: &str) -> bool {
        let key = (node, key.to_string());
        if self.rejections.remove(&key).is_none() {
            return false;
        }
        self.stale = true;
        true
    }

    /// Which nodes editors were last told are failing, for the diff.
    pub fn errors(&self) -> &HashMap<NodeId, String> {
        &self.errors
    }

    /// Records the current set of failing nodes.
    pub fn set_errors(&mut self, errors: HashMap<NodeId, String>) {
        if self.errors != errors {
            self.stale = true;
        }
        self.errors = errors;
    }

    /// Forgets everything, as when this process stops owning execution.
    pub fn clear(&mut self) {
        if !self.baseline.is_empty() || !self.errors.is_empty() || !self.rejections.is_empty() {
            self.stale = true;
        }
        self.baseline.clear();
        self.errors.clear();
        self.rejections.clear();
    }

    /// Writes the snapshot if the baseline moved since the last call.
    ///
    /// Rebuilt whole rather than patched: the snapshot has to be exactly what
    /// the baseline says, or a late editor would inherit a value the live
    /// events already cleared.
    pub fn flush(&mut self, seq: u64, into: &Mutex<Snapshot>) {
        if !self.stale {
            return;
        }
        let outputs: Vec<OutputRow> = self
            .baseline
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
        let errors: Vec<ErrorRow> = self
            .errors
            .iter()
            .map(|(id, message)| ErrorRow {
                node_id: id.0,
                message: message.clone(),
            })
            .collect();
        let rejections: Vec<RejectionRow> = self
            .rejections
            .iter()
            .map(|((id, key), message)| RejectionRow {
                node_id: id.0,
                key: key.clone(),
                message: message.clone(),
            })
            .collect();
        *into.lock().unwrap_or_else(|e| e.into_inner()) = Snapshot {
            seq,
            outputs,
            errors,
            rejections,
        };
        self.stale = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pins(pin: &str, value: &str) -> HashMap<String, (String, String)> {
        HashMap::from([(pin.to_string(), ("float".to_string(), value.to_string()))])
    }

    fn served(snapshot: &Mutex<Snapshot>) -> Vec<(u64, String, String)> {
        let mut rows: Vec<(u64, String, String)> = snapshot
            .lock()
            .expect("snapshot")
            .outputs
            .iter()
            .map(|row| (row.node_id, row.pin.clone(), row.value.clone()))
            .collect();
        rows.sort();
        rows
    }

    /// A late editor is served the snapshot, so it must say exactly what the
    /// diff baseline says. A node that was removed is the case that broke: the
    /// next pass has nothing to report about a node the graph no longer holds,
    /// so if the snapshot kept its rows, every editor joining afterwards saw
    /// the outputs of a node that does not exist.
    #[test]
    fn forgetting_a_node_takes_its_outputs_out_of_the_snapshot() {
        let snapshot = Mutex::new(Snapshot::default());
        let mut published = Published::default();
        published.set(NodeId(1), pins("out", "1"));
        published.set(NodeId(2), pins("result", "2"));
        published.flush(7, &snapshot);
        assert_eq!(
            served(&snapshot),
            vec![
                (1, "out".to_string(), "1".to_string()),
                (2, "result".to_string(), "2".to_string())
            ]
        );
        assert_eq!(snapshot.lock().expect("snapshot").seq, 7);

        published.forget(NodeId(1));
        published.flush(8, &snapshot);
        assert_eq!(published.get(NodeId(1)), None);
        assert_eq!(
            served(&snapshot),
            vec![(2, "result".to_string(), "2".to_string())]
        );
        assert_eq!(snapshot.lock().expect("snapshot").seq, 8);
    }

    /// Losing ownership empties both: another runtime produces the real values
    /// now, and this one must not answer for them.
    #[test]
    fn clearing_empties_the_snapshot_too() {
        let snapshot = Mutex::new(Snapshot::default());
        let mut published = Published::default();
        published.set(NodeId(3), pins("out", "3"));
        published.flush(1, &snapshot);
        published.clear();
        published.flush(2, &snapshot);
        assert!(served(&snapshot).is_empty());
    }

    /// Flushing an unchanged baseline must not touch the snapshot: the seq it
    /// carries is what an editor compares live events against, and bumping it
    /// for nothing would make it drop events it needs.
    #[test]
    fn flushing_without_a_change_leaves_the_snapshot_alone() {
        let snapshot = Mutex::new(Snapshot::default());
        let mut published = Published::default();
        published.set(NodeId(4), pins("out", "4"));
        published.flush(5, &snapshot);
        published.flush(9, &snapshot);
        assert_eq!(snapshot.lock().expect("snapshot").seq, 5);
        // A removal that removes nothing is not a change either.
        published.forget(NodeId(99));
        published.flush(9, &snapshot);
        assert_eq!(snapshot.lock().expect("snapshot").seq, 5);
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

    fn params(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    /// The first sight of a node is all new: it has been told nothing yet.
    #[test]
    fn every_parameter_of_a_new_node_is_changed() {
        let mut applied = AppliedParams::default();
        let row = params(&[("model", "qwen"), ("base_url", "http://x")]);
        let mut changed = applied.changed(NodeId(1), &row);
        changed.sort();
        assert_eq!(
            changed,
            vec![
                ("base_url".to_string(), "http://x".to_string()),
                ("model".to_string(), "qwen".to_string())
            ]
        );
    }

    /// The case this exists for: a drag rewrites the row with the same
    /// parameters, and re-applying one would re-run the node and everything
    /// downstream of it.
    #[test]
    fn an_unchanged_row_changes_nothing() {
        let mut applied = AppliedParams::default();
        let row = params(&[("model", "qwen"), ("base_url", "http://x")]);
        applied.changed(NodeId(1), &row);
        assert!(applied.changed(NodeId(1), &row).is_empty());
    }

    /// One edited field is one parameter to apply, not the whole set.
    #[test]
    fn only_the_edited_parameter_is_changed() {
        let mut applied = AppliedParams::default();
        let before = params(&[("model", "qwen"), ("base_url", "http://x")]);
        let after = params(&[("model", "qwen3"), ("base_url", "http://x")]);
        applied.changed(NodeId(1), &before);
        assert_eq!(
            applied.changed(NodeId(1), &after),
            vec![("model".to_string(), "qwen3".to_string())]
        );
    }

    /// Two nodes are two baselines: one node's setting must not silence
    /// another's.
    #[test]
    fn nodes_are_tracked_apart() {
        let mut applied = AppliedParams::default();
        let row = params(&[("hz", "2")]);
        applied.changed(NodeId(1), &row);
        assert_eq!(
            applied.changed(NodeId(2), &row),
            vec![("hz".to_string(), "2".to_string())]
        );
    }

    /// A key that left the row is forgotten, so the same text arriving later
    /// is applied again -- the node was never told the parameter was dropped,
    /// and treating it as still applied would leave the two disagreeing.
    #[test]
    fn a_dropped_key_is_applied_again_when_it_returns() {
        let mut applied = AppliedParams::default();
        let both = params(&[("where", "x > 1"), ("limit", "3")]);
        let one = params(&[("limit", "3")]);
        applied.changed(NodeId(1), &both);
        assert!(applied.changed(NodeId(1), &one).is_empty());
        assert_eq!(
            applied.changed(NodeId(1), &both),
            vec![("where".to_string(), "x > 1".to_string())]
        );
    }

    /// A node id that comes back is a node this process has told nothing: its
    /// instance is new and holds its own defaults.
    #[test]
    fn forgetting_a_node_restates_everything() {
        let mut applied = AppliedParams::default();
        let row = params(&[("path", "/tmp/db.sqlite")]);
        applied.changed(NodeId(1), &row);
        applied.forget(NodeId(1));
        assert_eq!(
            applied.changed(NodeId(1), &row),
            vec![("path".to_string(), "/tmp/db.sqlite".to_string())]
        );
    }

    /// The owner applies what it is waiting for, and nothing else. A result
    /// from the generation before the handover would run every node downstream
    /// of it a second time, in parallel with the runner that owns the graph
    /// now.
    #[test]
    fn only_the_current_owner_applies_its_own_results() {
        assert!(accepts_async_result(true, 3, 3));
        // Lost ownership while the work was running.
        assert!(!accepts_async_result(false, 3, 3));
        // Lost and regained it: another runner ran the graph in between.
        assert!(!accepts_async_result(true, 4, 3));
        // A result stamped with an epoch this process has not reached cannot be
        // its own either.
        assert!(!accepts_async_result(true, 3, 4));
    }

    fn errors(pairs: &[(u64, &str)]) -> HashMap<NodeId, String> {
        pairs
            .iter()
            .map(|(id, message)| (NodeId(*id), message.to_string()))
            .collect()
    }

    /// The whole point of the diff: a node that keeps failing for the same
    /// reason is not news, so a graph with one broken node costs nothing per
    /// pass at frame rate. A changed message is news, and so is a recovery.
    #[test]
    fn only_a_new_changed_or_gone_error_is_reported() {
        let published = errors(&[(1, "no table wired"), (2, "cannot open /x")]);

        assert!(error_changes(&published, &published).is_empty());

        let current = errors(&[
            (1, "no table wired"),
            (2, "cannot open /y"),
            (3, "cycle detected"),
        ]);
        assert_eq!(
            error_changes(&published, &current),
            vec![
                (NodeId(2), Some("cannot open /y".to_string())),
                (NodeId(3), Some("cycle detected".to_string())),
            ]
        );

        // A node that stopped failing has to be said out loud, or a fixed node
        // keeps its red border for the session.
        assert_eq!(
            error_changes(&published, &errors(&[(1, "no table wired")])),
            vec![(NodeId(2), None)]
        );
    }

    /// A late editor learns which nodes are broken from the snapshot, or it
    /// draws a graph that looks healthy until the next failure happens.
    #[test]
    fn the_snapshot_carries_the_failing_nodes() {
        let snapshot = Mutex::new(Snapshot::default());
        let mut published = Published::default();
        published.set(NodeId(1), pins("out", "1"));
        published.set_errors(errors(&[(2, "no frame wired")]));
        published.flush(4, &snapshot);
        let served = snapshot.lock().expect("snapshot");
        assert_eq!(served.errors.len(), 1);
        assert_eq!(served.errors[0].node_id, 2);
        assert_eq!(served.errors[0].message, "no frame wired");
        drop(served);

        // Losing ownership answers for nothing any more, failures included.
        published.clear();
        published.flush(5, &snapshot);
        let served = snapshot.lock().expect("snapshot");
        assert!(served.errors.is_empty());
        assert!(served.outputs.is_empty());
    }

    /// A deleted node's failure goes with it: nothing would ever clear an
    /// error on a node no editor can see.
    #[test]
    fn forgetting_a_node_takes_its_error_too() {
        let snapshot = Mutex::new(Snapshot::default());
        let mut published = Published::default();
        published.set_errors(errors(&[(7, "no table wired")]));
        published.flush(1, &snapshot);
        published.forget(NodeId(7));
        published.flush(2, &snapshot);
        assert!(snapshot.lock().expect("snapshot").errors.is_empty());
    }
}
