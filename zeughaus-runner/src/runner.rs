//! The executing half of the runner: turns store events into graph mutations,
//! runs the graph and publishes what it computed.
//!
//! Kept apart from `main.rs` so the decisions that are pure -- which press is a
//! new one, what type a stored parameter string becomes -- can be tested
//! without a server.
//!
//! Two representations of the graph live here. Every store row is mirrored,
//! because which graph a node belongs to -- and so whether this runner
//! executes it -- is decided by walking its parents, which may be anyone's.
//! Only the nodes in this runner's scope reach the [`GraphExecutor`], and
//! type ids and pins are read back out of it rather than mirrored beside it.

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use weida::Publisher;
use zeughaus_core::{
    DomainPlugin, EdgeData, EdgeId, Image, NodeData, NodeId, PinDirection, Press, Ty,
    TypeConverters, Value, encode_scalar, occupancy_winner,
};
use zeughaus_link::{
    ErrorRow, MachineState, OutputRow, RejectionRow, RuntimeEvent, Snapshot, TOPIC_EDGE,
    TOPIC_ERROR, TOPIC_MACHINE, TOPIC_OUTPUT,
};
use zeughaus_runtime::{DeferredWork, GraphExecutor};
use zeughaus_sync::{Store, SyncEvent};

use crate::feed::FrameRegistry;
use crate::jobs::JobHost;
use crate::mux::GraphSync;

/// Outcome of one node's deferred work, as reported back by the host loop.
pub type AsyncResult = Result<HashMap<String, Value>, String>;

pub struct Runner {
    /// The store connection, and the retry behind it. An `Option` inside,
    /// because "there is no store right now" is a state this process has to
    /// handle rather than one it can pretend away.
    store: Store,
    plugins: Vec<Box<dyn DomainPlugin>>,
    executor: GraphExecutor,
    /// Whether this process executes its graphs: the store is connected and
    /// this runner has an endpoint, whose fingerprint is what a graph's
    /// `runner` column names. Read back on every batch, because a store outage
    /// ends it without a handshake.
    executing: bool,
    /// Which generation of execution this process is on. Bumped every time
    /// execution starts, so deferred work dispatched under a previous one is
    /// recognizable when its result comes back. See [`Runner::deliver`].
    owner_epoch: u64,
    /// Role and runner count as last reported. A headless runner that logged
    /// nothing would be indistinguishable from one that is stuck, so the role is
    /// stated as soon as it is known and again whenever it or the number of
    /// connected runners changes.
    logged_role: Option<(bool, usize)>,
    /// This runner's own fingerprint (`sha256:<hex>`), taken from its endpoint
    /// URL. A top-level graph whose `runner` names it is executed here; `None`
    /// (no transport) executes nothing.
    fingerprint: Option<String>,
    /// Every node row in the store, in scope or not. Scope is decided by
    /// walking parents to the top-level graph, and that walk needs the nodes
    /// this process does not execute as much as the ones it does.
    nodes: HashMap<NodeId, NodeData>,
    /// Every edge row in the store. An edge between nodes that leave scope is
    /// dropped from the executor with them, and this is what brings it back
    /// when they return.
    edges: HashMap<EdgeId, EdgeData>,
    /// Whether a node row changed since the last [`Runner::rescope`].
    scope_dirty: bool,
    /// Whether the first subscription has applied, so the node mirror is the
    /// store's content and not a part of it.
    synced: bool,
    /// Whether the adoption of root-level nodes without a runner is still to
    /// be checked. Set by the first subscription, cleared once checked, so a
    /// legacy session is migrated at most once per process.
    adopt_pending: bool,
    /// Whether the adoption check already ran in this process.
    adopt_checked: bool,
    /// Whether the set of top-level graphs or any node name or id changed
    /// since [`Runner::graphs_changed`] was last asked.
    graphs_changed: bool,
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
        host: Option<Arc<JobHost>>,
        fingerprint: Option<String>,
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
            // Last, and the only plugin that needs this process: a job node
            // runs a process, and without a mux there is nothing to run it
            // in, so a runner with no transport registers the same detached
            // plugin an editor does and every job node refuses.
            match host {
                Some(host) => Box::new(zeughaus_job::JobPlugin::new(host)) as Box<dyn DomainPlugin>,
                None => Box::new(zeughaus_job::JobPlugin::detached()),
            },
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

        let executor = GraphExecutor::new(converters);

        Self {
            store,
            plugins,
            executor,
            executing: false,
            owner_epoch: 0,
            logged_role: None,
            fingerprint,
            nodes: HashMap::new(),
            edges: HashMap::new(),
            scope_dirty: false,
            synced: false,
            adopt_pending: false,
            adopt_checked: false,
            graphs_changed: false,
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

    /// Re-reads whether this process executes, and reports the role a human
    /// debugging this process needs before anything else.
    pub fn refresh_ownership(&mut self) {
        let Some(conn) = self.store.conn() else {
            // Without the store this process cannot see its graphs change hands
            // or be deleted, so it must not run them on a picture that may be
            // stale.
            self.lose_ownership();
            return;
        };
        let runners = zeughaus_sync::runtime_count(conn);
        if runners == 0 {
            // The runtime table has not reached this client yet: the
            // subscription is not applied, and nothing is known about which
            // graphs exist.
            return;
        }
        let executing = self.fingerprint.is_some();
        if self.logged_role != Some((executing, runners)) {
            self.logged_role = Some((executing, runners));
            if executing {
                eprintln!("[runner] executing its graphs ({runners} runner(s) connected)");
            } else {
                eprintln!("[runner] not executing: no endpoint, so no graph is this runner's");
            }
        }
        if executing == self.executing {
            return;
        }
        if !executing {
            self.lose_ownership();
            return;
        }
        self.executing = true;
        self.announce_endpoint();
        // A new generation: work this process dispatched before it stopped
        // executing belongs to a pass nobody waits for any more.
        self.owner_epoch += 1;
        // Execution just (re)started, so nothing this process holds has been
        // run under it: every node is stale.
        for id in self.executor.graph().node_ids().collect::<Vec<_>>() {
            self.executor.mark_dirty(id);
        }
        // Nothing was published under this generation, so its first pass
        // republishes everything.
        self.published.clear();
        self.published.flush(self.seq, &self.snapshot);
    }

    /// Stops executing: the store is gone, so this process can no longer tell
    /// whether its graphs still exist or still belong to it.
    ///
    /// The frames this process holds are the last ones it produced, so its
    /// viewers are sent away rather than shown a still picture; and nothing it
    /// published is current, so the snapshot it serves must not answer with
    /// values it no longer produces.
    fn lose_ownership(&mut self) {
        if !self.executing {
            return;
        }
        self.executing = false;
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
            // Connectivity is read from the client cache, not from the event.
            SyncEvent::RuntimesChanged => {}
            SyncEvent::SubscriptionApplied => {
                self.synced = true;
                if !self.adopt_checked {
                    self.adopt_pending = true;
                }
                // The subscription is the first look this process gets at its
                // own `runtime` row, and a reconnect recreates that row without
                // the endpoint. Re-announcing here is what keeps an editor from
                // resolving a runtime with an empty address.
                self.announce_endpoint();
            }
            // A reconnect is a new client to the store: a new `runtime` row
            // with no address in it. Announcing again is what keeps an editor
            // from resolving a runtime it cannot dial.
            SyncEvent::Connected => self.announce_endpoint(),
            // The store is gone: see `lose_ownership`.
            SyncEvent::Disconnected => self.lose_ownership(),
        }
    }

    /// Whether the first subscription has applied, so the graphs this process
    /// knows are the store's and not a part of them.
    pub fn synced(&self) -> bool {
        self.synced
    }

    /// Moves root-level nodes that belong to no runner into a graph of this
    /// runner's, once per process.
    ///
    /// Sessions from before graphs had owners keep their nodes at the root,
    /// where no runner executes them. Only the longest-connected runner asks,
    /// so two runners starting together do not create two graphs; the reducer
    /// does nothing when there is nothing left to adopt. While another runner
    /// is older the check stays pending: that runner may predate adoption, and
    /// a runtime leaving is a store event, so the next pass asks again.
    fn adopt_root_nodes(&mut self) {
        if !self.adopt_pending {
            return;
        }
        let Some(conn) = self.store.conn() else {
            return;
        };
        // The subscription can apply before this process's own `runtime` row
        // arrives; until it has, "not the oldest runner" is not an answer.
        if !zeughaus_sync::runtime_joined(conn) {
            return;
        }
        let Some(fingerprint) = self.fingerprint.as_deref() else {
            self.adopt_pending = false;
            self.adopt_checked = true;
            return;
        };
        // From the cache, not from `self.nodes`: the subscription is reported
        // applied before its rows' events, which may not have arrived yet.
        let orphans = zeughaus_sync::root_orphans(conn);
        if orphans == 0 {
            self.adopt_pending = false;
            self.adopt_checked = true;
            return;
        }
        if !zeughaus_sync::is_owner(conn) {
            return;
        }
        let graph = NodeId::next();
        eprintln!("[runner] adopting {orphans} root-level node(s) into graph {graph}");
        if let Err(e) = zeughaus_sync::send_adopt_root_nodes(conn, graph.0, fingerprint) {
            eprintln!("[runner] {e}");
        }
        self.adopt_pending = false;
        self.adopt_checked = true;
    }

    /// Brings the scope and the edges in line with the store events applied
    /// so far. Cheap when nothing changed, so it may run more than once a turn.
    pub fn reconcile(&mut self) {
        self.adopt_root_nodes();
        self.rescope();
        self.resolve_pending_edges();
    }

    /// Runs one pass: execute the dirty nodes and publish what changed.
    /// Returns the work that has to run off-thread.
    ///
    /// A runner that does not execute does nothing at all: it cannot tell
    /// whether its graphs still exist.
    pub fn pass(&mut self) -> DeferredWork {
        self.reconcile();
        self.log_size();
        if !self.executing {
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
    /// A runner that does not execute holds no schedule at all: a deadline it
    /// kept while idle would fire a burst the moment it started.
    pub fn mark_due_ticks(&mut self) -> bool {
        if !self.executing {
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
        if !accepts_async_result(self.executing, self.owner_epoch, epoch) {
            eprintln!(
                "[runner] dropping {node_id}'s result from epoch {epoch} (now {}, executing: {})",
                self.owner_epoch, self.executing
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
        if self.executing {
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
    /// A runner ignores a press for a node it does not execute: another runner
    /// owns that graph, and a press this process fired would run it twice.
    ///
    /// `payload` is free text the pressing side attached -- a webhook body, a
    /// branch name -- and `external` whether it pressed from outside an
    /// editor; both reach the node as the [`Press`] value of its `fire`
    /// parameter. A node that only wants the event ignores them, which is why
    /// a bare press (`None`) is an empty payload rather than a second
    /// parameter nobody reads.
    pub fn trigger(&mut self, node_id: u64, payload: Option<String>, external: bool) -> bool {
        let id = NodeId(node_id);
        if !self.executing || !self.in_scope(id) || self.executor.graph().node(id).is_none() {
            eprintln!("[runner] ignoring trigger for {id}");
            return false;
        }
        eprintln!("[runner] firing {id}");
        let press = Press {
            payload: payload.unwrap_or_default(),
            external,
        };
        // A node that refuses the press did not fire, so asking for a pass
        // would report a press that never happened -- and the editor that
        // pressed has to be told, which is what the node error is for.
        if let Err(e) = self.executor.set_parameter(id, "fire", Value::new(press)) {
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

    /// Tells editors the CI machine's busy state when it changed. Cheap to
    /// call every turn: an unchanged state publishes nothing.
    pub fn report_machine(&mut self, state: MachineState) {
        if !self.published.set_machine(state) {
            return;
        }
        let seq = self.next_seq();
        self.emit(
            TOPIC_MACHINE,
            RuntimeEvent::Machine {
                seq,
                mode: state.mode,
                busy: state.busy,
            },
        );
        self.published.flush(self.seq, &self.snapshot);
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
        let previous = self.nodes.insert(id, nd.clone());
        let graph_shape_changed = previous.as_ref().is_none_or(|p| {
            p.parent != nd.parent || p.runner != nd.runner || p.display_name != nd.display_name
        });
        if graph_shape_changed {
            self.graphs_changed = true;
            // A parent or owner change can move a whole subtree in or out of
            // this runner's scope, which only a full walk finds.
            self.scope_dirty = true;
        }
        if self.in_scope(id) {
            self.add_to_executor(nd);
        } else if self.executor.graph().node(id).is_some() {
            self.drop_from_executor(id);
        }
    }

    /// Adds a node row to the executor, or applies its parameters if the node
    /// is already there.
    fn add_to_executor(&mut self, nd: NodeData) {
        let id = NodeId(nd.id);
        if self.executor.graph().node(id).is_some() {
            // A row is rewritten for reasons execution does not care about --
            // a drag writes coordinates -- so only the parameters are applied.
            self.apply_params(id, &nd.type_id, &nd.params);
            return;
        }

        let Some(node) = self.plugins.iter().find_map(|p| p.create_node(&nd.type_id)) else {
            if self.unknown_types.insert(nd.id) {
                eprintln!(
                    "[runner] unknown node type '{}' for {id}: it will not execute here",
                    nd.type_id
                );
            }
            return;
        };
        // Marks the node dirty, so the pass after this batch runs it.
        self.executor.add_node(id, nd.type_id.clone(), node);

        // The node instance already holds its own setting defaults; the stored
        // params are the user's deviations from them.
        self.apply_params(id, &nd.type_id, &nd.params);
        // Edges touching this node were dropped when it left scope, or never
        // added; the mirror is what brings them back.
        for ed in self.edges.values() {
            let touches = ed.from_node == id.0 || ed.to_node == id.0;
            if touches && !self.pending_edges.iter().any(|p| p.id == ed.id) {
                self.pending_edges.push(ed.clone());
            }
        }
    }

    /// Whether `id` belongs to a top-level graph this runner executes.
    fn in_scope(&self, id: NodeId) -> bool {
        in_scope_of(&self.nodes, self.fingerprint.as_deref(), id)
    }

    /// Brings the executor in line with the scope after node rows changed:
    /// nodes that entered scope are added, nodes that left it are removed.
    ///
    /// A full walk rather than per-row bookkeeping because one row decides the
    /// scope of a whole subtree (a graph's owner, a container's parent), and
    /// rows of one subscription arrive in no particular order.
    fn rescope(&mut self) {
        if !self.scope_dirty {
            return;
        }
        self.scope_dirty = false;
        let ids: Vec<NodeId> = self.nodes.keys().copied().collect();
        for id in ids {
            let inside = self.in_scope(id);
            let present = self.executor.graph().node(id).is_some();
            if inside && !present && !self.unknown_types.contains(&id.0) {
                let nd = self.nodes[&id].clone();
                self.add_to_executor(nd);
            } else if !inside && present {
                self.drop_from_executor(id);
            }
        }
        let stale: Vec<NodeId> = self
            .executor
            .graph()
            .node_ids()
            .filter(|id| !self.nodes.contains_key(id))
            .collect();
        for id in stale {
            self.drop_from_executor(id);
        }
    }

    /// Whether the set of top-level graphs, a node name or a node id changed
    /// since the last call. Asking clears it.
    pub fn graphs_changed(&mut self) -> bool {
        std::mem::take(&mut self.graphs_changed)
    }

    /// What the terminal mux needs to keep its graph panes in line with the
    /// store: this runner's top-level graphs, every node's name and id.
    pub fn graph_sync(&self) -> GraphSync {
        let me = self.fingerprint.as_deref().filter(|me| !me.is_empty());
        let mut owned: Vec<u64> = self
            .nodes
            .values()
            .filter(|node| node.parent == 0 && me == Some(node.runner.as_str()))
            .map(|node| node.id)
            .collect();
        owned.sort_unstable();
        GraphSync {
            owned,
            names: self
                .nodes
                .values()
                .map(|node| (node.id, node.display_name.clone()))
                .collect(),
            exists: self.nodes.keys().map(|id| id.0).collect(),
        }
    }

    /// Hands the node the parameters that actually changed.
    ///
    /// A node row is rewritten for reasons that have nothing to do with its
    /// settings -- a drag writes `x`/`y`, and the editor restates the whole
    /// parameter set on every keystroke -- and `set_parameter` marks the node
    /// and its entire downstream subtree dirty. Applying a value that did not
    /// change is therefore a re-run of the node: one LLM request per keystroke
    /// in a chat node's `model` field, one screen capture per drag. Diffing
    /// against what this process last applied is what makes a move cost
    /// nothing.
    ///
    /// Every value reaches the node as the text the user typed. A node that
    /// needs a number parses it and refuses what is not one (see
    /// `SettingDef`), which is the one place that knows what its setting
    /// means.
    fn apply_params(&mut self, id: NodeId, type_id: &str, params: &[(String, String)]) {
        let changed = self.applied_params.changed(id, params);
        if changed.is_empty() {
            return;
        }
        for (name, text) in changed {
            // A node that refuses the text keeps the value it had, which is a
            // node whose setting on screen is not the setting it runs on -- so
            // the editor that typed it, and every other window, is told which
            // setting and why.
            //
            // Its own event rather than a node error: nothing failed, the node
            // goes on running with the value it kept, and raising the failure
            // alarm for it puts a red border around a working node and an
            // `ERROR:` line in every editor's status bar.
            let refusal = self
                .executor
                .set_parameter(id, &name, Value::new(text.to_string()))
                .err()
                .map(|e| e.to_string());
            match refusal {
                Some(message) => {
                    eprintln!("[runner] {id} ({type_id}) refused {name}={text:?}: {message}");
                    if self.published.reject(id, &name, message.clone()) && self.executing {
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
                    if self.published.accept(id, &name) && self.executing {
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
        if self.nodes.remove(&id).is_some() {
            self.graphs_changed = true;
            self.scope_dirty = true;
        }
        self.drop_from_executor(id);
    }

    /// Removes a node from execution: it was deleted, or it left this runner's
    /// scope.
    fn drop_from_executor(&mut self, id: NodeId) {
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
        self.due.remove(&id);
    }

    /// Retries the edges whose endpoints were missing when they arrived. An
    /// edge that is still unresolvable stays queued, because the node row it
    /// names may simply be in a later batch -- unless both of its nodes are
    /// known and one of them belongs to another runner, which no later batch
    /// changes without a node row that requeues it.
    fn resolve_pending_edges(&mut self) {
        if self.pending_edges.is_empty() {
            return;
        }
        for ed in std::mem::take(&mut self.pending_edges) {
            let (from, to) = (NodeId(ed.from_node), NodeId(ed.to_node));
            let known = self.nodes.contains_key(&from) && self.nodes.contains_key(&to);
            if known && !(self.in_scope(from) && self.in_scope(to)) {
                continue;
            }
            self.connect(ed);
        }
    }

    fn apply_edge_insert(&mut self, ed: EdgeData) {
        self.edges.insert(EdgeId(ed.id), ed.clone());
        self.connect(ed);
    }

    /// Adds an edge to the executor, or queues it until its nodes are there.
    fn connect(&mut self, ed: EdgeData) {
        let edge_id = EdgeId(ed.id);
        if self.executor.graph().edge(edge_id).is_some() {
            return;
        }
        let from_node = NodeId(ed.from_node);
        let to_node = NodeId(ed.to_node);
        // A subscription applies as one burst of row callbacks with no ordering
        // between tables, so an edge routinely arrives before the nodes it
        // names. Dropping it would lose that wire for the life of the process --
        // the row never changes again, so no second event would ever bring it
        // back -- hence it waits for its endpoints instead. An edge between
        // known nodes of which one is another runner's has nothing to wait
        // for: the mirror requeues it if that node ever enters scope.
        if self.executor.graph().node(from_node).is_none()
            || self.executor.graph().node(to_node).is_none()
        {
            let known = self.nodes.contains_key(&from_node) && self.nodes.contains_key(&to_node);
            if !known || (self.in_scope(from_node) && self.in_scope(to_node)) {
                self.pending_edges.push(ed);
            }
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
            .graph()
            .incoming_edges(to_node)
            .filter(|eid| {
                self.executor
                    .graph()
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

        // Seeds the edge from the source's cached output and dirties only the
        // target's subtree, so the source is not re-run (no duplicate LLM call
        // or capture just because a wire appeared).
        self.executor
            .add_edge(edge_id, from_node, from_pin, to_node, to_pin);
        // A variadic target (e.g. merge) grows an input once the last one fills.
        self.executor.sync_node_pins(to_node);
    }

    fn apply_edge_remove(&mut self, id: EdgeId) {
        self.edges.remove(&id);
        self.pending_edges.retain(|ed| ed.id != id.0);
        let to_node = self.executor.graph().edge(id).map(|e| e.to_node);
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
        if !self.executing {
            return;
        }
        for id in self.executor.graph().node_ids().collect::<Vec<_>>() {
            let Some(node) = self.executor.graph().node(id) else {
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
        if !self.executing {
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
        if !self.executing {
            return;
        }
        let frame_ty = Ty::of::<Image>();
        let mut pins: Vec<(NodeId, &str, Option<&Image>)> = Vec::new();
        for id in self.executor.graph().node_ids() {
            let Some(node) = self.executor.graph().node(id) else {
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
            self.executor.graph().node_count(),
            self.executor.graph().edge_count(),
        );
        if size == self.logged_size {
            return;
        }
        self.logged_size = size;
        eprintln!("[runner] graph: {} nodes, {} edges", size.0, size.1);
    }
}

/// Whether an async result dispatched under `dispatched` may still be applied.
///
/// Applying one runs every node downstream of it, so it takes the two facts
/// that make that safe: this process executes, and it is the same generation
/// of execution it dispatched the work under. A runner that lost the store
/// fails the first, and one that lost and regained it in between fails the
/// second -- in that gap its graphs may have changed hands, so the result
/// describes a pass nobody is waiting for any more.
///
/// Pure so it can be tested: [`Runner`] needs a live store connection, and
/// this decision must not.
fn accepts_async_result(executing: bool, current: u64, dispatched: u64) -> bool {
    executing && current == dispatched
}

/// How deep a parent chain may be before it is taken for a loop. No graph is
/// nested this deep by hand, and a store row pointing back at its own subtree
/// must not hang the loop.
const MAX_DEPTH: usize = 64;

/// Whether `id` belongs to a top-level graph whose `runner` is `me`. A runner
/// without a fingerprint, and a graph that names none, own nothing.
fn in_scope_of(nodes: &HashMap<NodeId, NodeData>, me: Option<&str>, id: NodeId) -> bool {
    let Some(me) = me.filter(|me| !me.is_empty()) else {
        return false;
    };
    top_level(nodes, id).is_some_and(|g| nodes.get(&g).is_some_and(|n| n.runner == me))
}

/// The top-level graph `id` lives in: the ancestor whose parent is `0`.
///
/// `None` while the chain is broken (an ancestor's row has not arrived yet)
/// or loops. Either way the node is in nobody's scope for now, which is the
/// safe answer: executing it could run another runner's graph.
fn top_level(nodes: &HashMap<NodeId, NodeData>, id: NodeId) -> Option<NodeId> {
    let mut current = id;
    for _ in 0..MAX_DEPTH {
        let node = nodes.get(&current)?;
        if node.parent == 0 {
            return Some(current);
        }
        current = NodeId(node.parent);
    }
    None
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
/// One type because the two must not disagree. Kept apart they would: a
/// removed node drops out of the baseline, the next pass has nothing to say
/// about a node the graph does not hold, and a snapshot of its own would go
/// on serving that node's outputs to every editor joining afterwards.
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
    /// The CI machine's busy state, as editors were last told. Not about the
    /// graph, so [`Published::clear`] keeps it.
    machine: Option<MachineState>,
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

    /// Records the CI machine's state. `true` when that is news.
    pub fn set_machine(&mut self, state: MachineState) -> bool {
        if self.machine == Some(state) {
            return false;
        }
        self.machine = Some(state);
        self.stale = true;
        true
    }

    /// Forgets everything about the graphs, as when this process stops
    /// owning execution. The machine is not the graphs'.
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
            machine: self.machine,
        };
        self.stale = false;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: u64, parent: u64, runner: &str) -> (NodeId, NodeData) {
        (
            NodeId(id),
            NodeData {
                id,
                type_id: "graph.sub".to_string(),
                display_name: String::new(),
                x: 0.0,
                y: 0.0,
                parent,
                params: Vec::new(),
                runner: runner.to_string(),
            },
        )
    }

    /// A node's graph is found through any depth of containers; a parent that
    /// has not arrived and a chain that loops both leave it in no graph.
    #[test]
    fn top_level_walks_parents_and_refuses_broken_chains() {
        let nodes = HashMap::from([
            row(1, 0, "sha256:aa"),
            row(2, 1, ""),
            row(3, 2, ""),
            row(10, 99, ""),
            row(20, 21, ""),
            row(21, 20, ""),
        ]);
        assert_eq!(top_level(&nodes, NodeId(1)), Some(NodeId(1)));
        assert_eq!(top_level(&nodes, NodeId(3)), Some(NodeId(1)));
        assert_eq!(top_level(&nodes, NodeId(10)), None, "unknown parent");
        assert_eq!(top_level(&nodes, NodeId(20)), None, "loop");
        assert_eq!(top_level(&nodes, NodeId(404)), None, "unknown node");
    }

    /// Two runners see the same rows and each executes only the subtree of
    /// the graph that names it; a runner without a fingerprint executes
    /// nothing, and a root-level node without a runner belongs to nobody.
    #[test]
    fn scope_follows_the_graph_owner() {
        let nodes = HashMap::from([
            row(1, 0, "sha256:aa"),
            row(2, 1, ""),
            row(5, 0, "sha256:bb"),
            row(6, 5, ""),
            row(7, 6, ""),
            row(9, 0, ""),
        ]);
        let a: Vec<u64> = [1, 2, 5, 6, 7, 9]
            .into_iter()
            .filter(|id| in_scope_of(&nodes, Some("sha256:aa"), NodeId(*id)))
            .collect();
        let b: Vec<u64> = [1, 2, 5, 6, 7, 9]
            .into_iter()
            .filter(|id| in_scope_of(&nodes, Some("sha256:bb"), NodeId(*id)))
            .collect();
        assert_eq!(a, vec![1, 2]);
        assert_eq!(b, vec![5, 6, 7]);
        assert!(!in_scope_of(&nodes, None, NodeId(9)));
        assert!(
            !in_scope_of(&nodes, Some(""), NodeId(9)),
            "no runner is nobody's"
        );
    }

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
    /// diff baseline says. A removed node is the case that decides it: the
    /// next pass has nothing to report about a node the graph no longer
    /// holds, so a snapshot keeping its rows would serve the outputs of a
    /// node that does not exist to every editor joining afterwards.
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
