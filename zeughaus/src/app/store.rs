//! The shared store: local edits on their way out, remote rows on their way
//! in, and the parameters this editor derives for the database nodes.
//!
//! Native only, like the store client itself: the browser editor has no sync
//! layer, so nothing here is compiled for it.

use std::sync::Arc;
use std::time::Instant;

use iced::Point;
use zeughaus_core::{EdgeData, EdgeId, NodeData, NodeId, occupancy_winner};

use super::App;
use super::graph::{EditorEdge, relation_references_to};
use crate::message::{Message, PinLabel};

/// One local edit on its way to the store.
///
/// The payload is owned and captured when the edit was made, not read again at
/// send time. That matters only for a replay after a reconnect: the store's
/// snapshot arrives on the same connection and may already have overwritten
/// this window's state with the older shared value, and re-reading state then
/// would send the store its own stale value back.
#[derive(Debug, Clone)]
pub(super) enum Outbound {
    Node(NodeData),
    Params(NodeId, Vec<(String, String)>),
    Move(NodeId, f32, f32),
    Delete(NodeId),
    Connect(EdgeData),
    Disconnect(EdgeId),
}

/// Calls the reducer one queued edit means.
fn send_outbound(
    conn: &zeughaus_sync::module_bindings::DbConnection,
    edit: &Outbound,
) -> Result<(), String> {
    match edit {
        Outbound::Node(nd) => zeughaus_sync::send_create_node(conn, nd),
        Outbound::Params(id, params) => zeughaus_sync::send_set_params(conn, id.0, params),
        Outbound::Move(id, x, y) => zeughaus_sync::send_move_node(conn, id.0, *x, *y),
        Outbound::Delete(id) => zeughaus_sync::send_delete_node(conn, id.0),
        Outbound::Connect(e) => zeughaus_sync::send_connect_edge(conn, e),
        Outbound::Disconnect(id) => zeughaus_sync::send_disconnect_edge(conn, id.0),
    }
}

/// An editor edge as the store's row.
pub(super) fn edge_data(e: &EditorEdge) -> EdgeData {
    EdgeData {
        id: e.id.0,
        from_node: e.from_node.0,
        from_pin: e.from_pin.to_string(),
        to_node: e.to_node.0,
        to_pin: e.to_pin.to_string(),
    }
}

impl App {
    /// Records the name a table had before this commit, so the runner can
    /// rename the table in the file instead of creating a second one.
    ///
    /// The editor is the only process that sees the edit: the store holds one
    /// row per node, and a runner handed a row with a new name cannot tell a
    /// rename from a table it has never heard of. `was` is the value the store
    /// held, so however many keystrokes produced the new name, this is the one
    /// name the file can still be under.
    pub(super) fn settle_table_rename(&mut self, node: NodeId, key: &str, was: &str) {
        if key != "name"
            || self
                .nodes
                .get(&node)
                .is_none_or(|n| n.type_id != "db.table")
        {
            return;
        }
        // A name the node refused renamed nothing: it kept the one it had, and
        // recording a rename away from it would name a table that is still in
        // use.
        if self
            .setting_errors
            .get(&node)
            .is_some_and(|errors| errors.contains_key(key))
        {
            return;
        }
        let now = self.setting_or_default(node, "name");
        // Nothing to rename from: the name did not change, or there was none.
        let from = if was.is_empty() || was == now {
            String::new()
        } else {
            was.to_string()
        };
        if self.setting_or_default(node, zeughaus_db::RENAMED_FROM) == from {
            return;
        }
        self.apply_setting(node, zeughaus_db::RENAMED_FROM, from);
    }

    /// Commits one node's held-back settings to the store: the wires they
    /// moved, the parameters they are derived into, and the row itself.
    pub(super) fn commit_node(&mut self, node: NodeId, owed: crate::pending::Owed) {
        // Sorted so a run of edits produces the same sequence of store calls
        // in every window that replays it.
        let mut keys: Vec<(String, String)> = owed.was.into_iter().collect();
        keys.sort();
        for (key, was) in keys {
            self.settle_relations(node, &key, &was);
            self.settle_table_rename(node, &key, &was);
        }
        // A database's path reaches its children, a table's columns reach
        // everything it feeds, and its name is what the tables referencing it
        // name in their foreign keys.
        self.derive_db_params(node);
        self.derive_db_dependents(node);
        self.push_params(node);
    }

    /// Commits every node whose settings have been quiet long enough. Called
    /// from the sync poll, which is the editor's only clock while it is idle.
    pub(super) fn commit_settled(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        let due = self
            .pending
            .settled(Instant::now(), crate::pending::DEBOUNCE);
        for (node, owed) in due {
            self.commit_node(node, owed);
        }
    }

    /// Commits everything now, because something is about to read or change
    /// the shared graph and must not see a store the local view has outgrown.
    pub(super) fn flush_pending(&mut self) {
        if self.pending.is_empty() {
            return;
        }
        for (node, owed) in self.pending.drain() {
            self.commit_node(node, owed);
        }
    }

    /// The live store connection, or `None` while there is none.
    ///
    /// `None` is a state, not a failure: the host may be restarting, and this
    /// window keeps working on a graph it can no longer share.
    pub(super) fn store_conn(&self) -> Option<&zeughaus_sync::module_bindings::DbConnection> {
        self.stdb.as_ref().and_then(zeughaus_sync::Store::conn)
    }

    /// Whether this window's edits are reaching the shared graph.
    pub(super) fn store_live(&self) -> bool {
        self.stdb
            .as_ref()
            .is_some_and(zeughaus_sync::Store::is_live)
    }

    /// Serializes one node's current data (id, type, position, params) for a
    /// reducer call.
    pub(super) fn node_data(&self, id: NodeId) -> Option<NodeData> {
        let node = self.nodes.get(&id)?;
        let params: Vec<(String, String)> = self
            .node_settings
            .get(&id)
            .map(|settings| {
                settings
                    .iter()
                    .map(|(key, value)| (key.clone(), value.clone()))
                    .collect()
            })
            .unwrap_or_default();
        Some(NodeData {
            id: id.0,
            type_id: node.type_id.clone(),
            display_name: node.display_name.clone(),
            x: node.position.x,
            y: node.position.y,
            params,
            parent: node.parent.0,
        })
    }

    // Send: local edits -> reducers. Guarded by `applying_remote` so a change
    // applied from the store does not echo back as a new reducer call.
    //
    // Every one of them goes through [`Self::dispatch`], which tries the store
    // and keeps the edit when it cannot: a reducer call that failed is a log
    // line nobody reads, and a session's worth of work must not go with the
    // host.
    pub(super) fn push_node(&mut self, id: NodeId) {
        if let Some(nd) = self.node_data(id) {
            self.dispatch(Outbound::Node(nd));
        }
    }

    pub(super) fn push_params(&mut self, id: NodeId) {
        if let Some(nd) = self.node_data(id) {
            self.dispatch(Outbound::Params(id, nd.params));
        }
    }

    pub(super) fn push_move(&mut self, id: NodeId, x: f32, y: f32) {
        self.dispatch(Outbound::Move(id, x, y));
    }

    pub(super) fn push_delete(&mut self, id: NodeId) {
        self.dispatch(Outbound::Delete(id));
    }

    pub(super) fn push_edge(&mut self, e: EdgeData) {
        self.dispatch(Outbound::Connect(e));
    }

    pub(super) fn push_edge_remove(&mut self, id: EdgeId) {
        self.dispatch(Outbound::Disconnect(id));
    }

    /// Sends one edit to the store, or keeps it until the store is back.
    ///
    /// Silent while a remote change is being applied: that would echo the
    /// change straight back as a new reducer call.
    pub(super) fn dispatch(&mut self, edit: Outbound) {
        if self.applying_remote {
            return;
        }
        let sent = self
            .stdb
            .as_ref()
            .and_then(zeughaus_sync::Store::conn)
            .map(|conn| send_outbound(conn, &edit));
        match sent {
            Some(Ok(())) => {}
            Some(Err(e)) => {
                // Once per distinct message: a store that refuses every call
                // would otherwise print one line per keystroke.
                if self.logged_sends.insert(e.clone()) {
                    eprintln!("[stdb] {e}");
                }
                self.outbox.push(edit);
            }
            // No connection at all: nothing to say that the status bar is not
            // already saying.
            None => self.outbox.push(edit),
        }
    }

    /// Replays what the store never received, oldest first.
    ///
    /// Order is the whole point: a node has to exist before an edge names it,
    /// and a delete has to come after the create it undoes. An edit that fails
    /// again stays queued, and everything after it stays behind it.
    pub(super) fn flush_outbox(&mut self) {
        if self.outbox.is_empty() {
            return;
        }
        let Some(conn) = self.stdb.as_ref().and_then(zeughaus_sync::Store::conn) else {
            return;
        };
        let queued = std::mem::take(&mut self.outbox);
        let total = queued.len();
        let mut kept: Vec<Outbound> = Vec::new();
        for edit in queued {
            if !kept.is_empty() {
                kept.push(edit);
                continue;
            }
            if let Err(e) = send_outbound(conn, &edit) {
                eprintln!("[stdb] {e} (keeping {} edits)", total - kept.len());
                kept.push(edit);
            }
        }
        if kept.is_empty() {
            eprintln!("[stdb] {total} local edit(s) reached the store");
        }
        self.outbox = kept;
    }

    // Receive: drain queued remote events and apply them to the editor.

    /// Settles which wire owns a single-slot input pin, deleting the losers
    /// here and in the store. Returns whether `arriving` won.
    ///
    /// Only the remote path needs it: a local connect cannot land on an
    /// occupied input (`can_connect` rejects it), but two windows can each
    /// draw one without seeing the other. The verdict comes from
    /// [`occupancy_winner`] rather than from arrival order, so every window
    /// and every runner keeps the same wire -- ordering by arrival leaves each
    /// window with whichever row reached it last, and a window opened
    /// afterwards with a third answer.
    ///
    /// The deletion is pushed even while a remote change is being applied.
    /// That guard is there to stop an echo, and this is not one: the row
    /// deleted is a different edge than the one that arrived. Every window
    /// that sees both rows issues the same delete and the reducer is
    /// idempotent, so agreeing is cheap and leaving the row is not -- nothing
    /// would ever remove it and it would outlive every view that dropped it.
    pub(super) fn resolve_input_occupancy(
        &mut self,
        arriving: EdgeId,
        to_node: NodeId,
        to_pin: &str,
    ) -> bool {
        let mut contenders: Vec<EdgeId> = self
            .edges
            .iter()
            .filter(|e| e.to_node == to_node && e.to_pin.as_str() == to_pin)
            .map(|e| e.id)
            .collect();
        if contenders.is_empty() {
            return true;
        }
        contenders.push(arriving);
        let winner = occupancy_winner(contenders.iter().copied()).expect("contenders is not empty");
        for loser in contenders.into_iter().filter(|id| *id != winner) {
            self.forget_edge(loser);
            // Not through `dispatch`: that is silent while a remote change is
            // being applied, which is right for an echo and wrong here -- the
            // row deleted is a different edge than the one that arrived. If
            // the store is gone the loser's row goes with it anyway.
            if let Some(conn) = self.store_conn() {
                let _ = zeughaus_sync::send_disconnect_edge(conn, loser.0);
            }
        }
        winner == arriving
    }

    pub(super) fn drain_sync(&mut self) {
        let mut events = Vec::new();
        if let Some(rx) = &self.sync_rx {
            while let Ok(ev) = rx.try_recv() {
                events.push(ev);
            }
        }
        if events.is_empty() {
            return;
        }
        self.applying_remote = true;
        for ev in events {
            self.apply_sync_event(ev);
        }
        // The node an edge was waiting for may have been in this very batch.
        self.resolve_pending_edges();
        self.applying_remote = false;
        // After the guard, not during: deriving a database node's parameters is
        // this editor's own decision and has to reach the store, which
        // `push_params` refuses while a remote change is being applied. It is
        // also the whole batch that decides the answer -- the table a node
        // reads its columns from may have arrived in it.
        self.derive_all_db_params();
        // A batch that added an edge changed what every node it touches shows:
        // the value was already known, the wire to carry it was not. Nothing
        // else refreshes the display map on the remote path, so without this a
        // graph built by another window sits there with the wires drawn and
        // the bodies empty until the next value arrives.
        self.update_display_values();
    }

    pub(super) fn apply_sync_event(&mut self, ev: zeughaus_sync::SyncEvent) {
        use zeughaus_sync::SyncEvent;
        match ev {
            SyncEvent::NodeUpsert(nd) => self.apply_node_upsert(nd),
            SyncEvent::NodeRemove(id) => self.apply_node_remove(NodeId(id)),
            SyncEvent::EdgeInsert(ed) => self.apply_edge_insert(ed),
            SyncEvent::EdgeRemove(id) => self.apply_edge_remove(EdgeId(id)),
            SyncEvent::RuntimesChanged => self.apply_runtimes_changed(),
            // The batch may have brought this editor's first look at the
            // runtime table, which is where the endpoint to dial comes from.
            SyncEvent::SubscriptionApplied => {}
            // The store is back: whatever this window edited while it was gone
            // has not reached it, and now can.
            SyncEvent::Connected => self.flush_outbox(),
            // Said in the status bar rather than here: `Store::is_live` is the
            // authority, and it answers without waiting for an event.
            SyncEvent::Disconnected => {}
        }
    }

    /// Re-reads whether anything is executing the graph.
    ///
    /// The editor is not a candidate -- it registers as `Role::Viewer` and never
    /// appears in the runtime table -- so this is purely informational. It is
    /// still the difference between a live number and a stale one, which is the
    /// one thing a user must not have to guess about.
    pub(super) fn apply_runtimes_changed(&mut self) {
        if let Some(conn) = self.store_conn() {
            self.runtimes = zeughaus_sync::runtime_count(conn);
        }
    }

    pub(super) fn apply_node_upsert(&mut self, nd: NodeData) {
        let id = NodeId(nd.id);
        if self.nodes.contains_key(&id) {
            if let Some(en) = self.nodes.get_mut(&id) {
                en.position = Point::new(nd.x, nd.y);
            }
            self.apply_params(id, &nd.params);
        } else {
            self.insert_node_from_data(&nd);
            // A value can arrive before the node row it belongs to: this is
            // where the one that was waiting is applied.
            self.reapply_remote_outputs(id);
        }
        // A boundary node arriving from another window is a pin its container
        // gains here, and a renamed one is a pin that changed name.
        self.refresh_container_pins(NodeId(nd.parent));
        // The database parameters this node needs are derived once the whole
        // batch has been applied (`derive_all_db_params`): they have to reach
        // the store, and `push_params` is silent while a remote change is
        // being applied.
    }

    /// The relations a table's fields declare, one `field -> table.field` per
    /// line, as the `relations` parameter carries them.
    ///
    /// Only the referencing side of each relation writes a line; which side
    /// that is follows from [`relation_references_to`].
    ///
    /// Sorted, because this text is an output the runner compares: a set that
    /// reordered itself would look like a change on every pass.
    pub(super) fn relations_of(&self, node: NodeId) -> String {
        let mut lines: Vec<String> = Vec::new();
        for edge in &self.edges {
            if !self.is_relation(edge) {
                continue;
            }
            let to_is_key = relation_references_to(edge.from_pin.as_str(), edge.to_pin.as_str());
            let (referencing, referenced) = if to_is_key {
                (
                    (edge.from_node, &edge.from_pin),
                    (edge.to_node, &edge.to_pin),
                )
            } else {
                (
                    (edge.to_node, &edge.to_pin),
                    (edge.from_node, &edge.from_pin),
                )
            };
            if referencing.0 != node {
                continue;
            }
            let table = self.setting_or_default(referenced.0, "name");
            if table.is_empty() {
                continue;
            }
            lines.push(format!(
                "{} -> {}.{}",
                referencing.1.as_str(),
                table,
                referenced.1.as_str()
            ));
        }
        lines.sort();
        lines.dedup();
        lines.join("\n")
    }

    /// Fills in the parameters a database node cannot know by itself: which
    /// file it works on, the columns of the table it is wired to, and the
    /// relations a table's fields declare.
    ///
    /// Derived rather than typed twice. A node inside a `db.database` works on
    /// that database, an insert wired to a table has that table's columns, and
    /// a wire between two field pins is a foreign key -- restating any of them
    /// by hand is a chance for the two to disagree. They are ordinary
    /// parameters from the runner's point of view, so nothing on that side has
    /// to know they were derived, and no wire between fields has to reach it.
    pub(super) fn derive_db_params(&mut self, node: NodeId) {
        let Some(type_id) = self.nodes.get(&node).map(|n| n.type_id.clone()) else {
            return;
        };
        if !type_id.starts_with("db.") {
            return;
        }
        let mut derived: Vec<(String, String)> = Vec::new();

        let parent = self.nodes.get(&node).map(|n| n.parent).unwrap_or(NodeId(0));
        // An empty path is the honest answer for a node outside a database:
        // the node reports that instead of creating a file somewhere.
        let path = if self
            .nodes
            .get(&parent)
            .is_some_and(|p| p.type_id == "db.database")
        {
            self.setting_or_default(parent, "path")
        } else {
            String::new()
        };
        derived.push((zeughaus_db::DB_PATH.to_string(), path));

        if matches!(type_id.as_str(), "db.insert" | "db.query") {
            let source = self
                .edges
                .iter()
                .find(|e| e.to_node == node && e.to_pin.as_str() == "table")
                .map(|e| e.from_node)
                .filter(|from| {
                    self.nodes
                        .get(from)
                        .is_some_and(|n| n.type_id == "db.table")
                });
            let columns = source
                .map(|table| self.setting_or_default(table, "columns"))
                .unwrap_or_default();
            derived.push(("columns".to_string(), columns));
        }

        if type_id == "db.table" {
            derived.push((zeughaus_db::RELATIONS.to_string(), self.relations_of(node)));
        }

        let mut changed = false;
        for (key, value) in derived {
            if self
                .node_settings
                .get(&node)
                .and_then(|s| s.get(&key))
                .is_some_and(|current| *current == value)
            {
                continue;
            }
            self.apply_setting(node, &key, value);
            changed = true;
        }
        if changed {
            // The store carries the parameter to the runner, which derives
            // nothing itself: an insert's pins follow its column list, and
            // that list is a parameter like any other.
            self.push_params(node);
        }
    }

    /// Re-derives the database parameters of every node that depends on this
    /// one: the children of a database, everything a table feeds, and every
    /// table it is in a relation with (whose foreign key names this one).
    pub(super) fn derive_db_dependents(&mut self, node: NodeId) {
        let Some(type_id) = self.nodes.get(&node).map(|n| n.type_id.clone()) else {
            return;
        };
        let dependents: Vec<NodeId> = match type_id.as_str() {
            "db.database" => self.children(node).map(|child| child.id).collect(),
            "db.table" => self
                .edges
                .iter()
                .filter(|e| {
                    (e.from_node == node && e.to_pin.as_str() == "table") || self.is_relation(e)
                })
                .filter_map(|e| match (e.from_node == node, e.to_node == node) {
                    (true, _) => Some(e.to_node),
                    (_, true) => Some(e.from_node),
                    _ => None,
                })
                .collect(),
            _ => return,
        };
        for dependent in dependents {
            self.derive_db_params(dependent);
        }
    }

    /// Re-derives every database node's parameters.
    ///
    /// Cheap and idempotent: a node whose derived values are unchanged writes
    /// nothing. Called once per applied batch, because a batch can move any of
    /// the three things the answer depends on -- the node's parent, a
    /// database's path, a table's columns.
    pub(super) fn derive_all_db_params(&mut self) {
        let database_nodes: Vec<NodeId> = self
            .node_order
            .iter()
            .filter(|id| {
                self.nodes
                    .get(id)
                    .is_some_and(|node| node.type_id.starts_with("db."))
            })
            .copied()
            .collect();
        for node in database_nodes {
            self.derive_db_params(node);
        }
    }

    /// Adopts the parameters a row carries. Every one of them is text the node
    /// parses, so a refusal lands under the field exactly as it does for a
    /// value typed in this window.
    pub(super) fn apply_params(&mut self, id: NodeId, params: &[(String, String)]) {
        for (name, text) in params {
            // A key this window still owes the store is one the user is
            // typing: the shared value for it is older than what is on screen
            // (usually this window's own echo, one debounce behind), and
            // adopting it snaps the field back mid-word and then pushes the
            // snapped-back text. The remote value is taken for that key the
            // next time the row arrives with nothing owed.
            if self.pending.owes(id, name) {
                continue;
            }
            self.apply_setting(id, name, text.clone());
        }
    }

    pub(super) fn apply_node_remove(&mut self, id: NodeId) {
        let parent = self.nodes.get(&id).map(|node| node.parent);
        self.nodes.remove(&id);
        self.node_order.retain(|n| *n != id);
        for edge in self
            .edges
            .iter()
            .filter(|e| e.from_node == id || e.to_node == id)
        {
            self.runtime.particles.remove(&edge.id);
        }
        self.edges.retain(|e| e.from_node != id && e.to_node != id);
        self.reindex_edges();
        self.instances.remove(&id);
        self.node_settings.remove(&id);
        self.setting_errors.remove(&id);
        // A value whose producer is gone is not a value any more, and node ids
        // are never reused, so nothing can inherit it. Same for what this
        // window still owed the store: the row it would have updated is gone.
        {
            self.pending.take(id);
            self.runtime.remote_outputs.remove(&id);
            self.runtime.output_seq.retain(|(node, _), _| *node != id);
            self.runtime.remote_errors.remove(&id);
            self.runtime.error_seq.remove(&id);
            self.runtime
                .rejection_seq
                .retain(|(node, _), _| *node != id);
        }
        // A removed boundary node is a pin its container loses. A removed
        // container's tab closes with it, once this update settles.
        if let Some(parent) = parent {
            self.refresh_container_pins(parent);
        }
        self.cameras.remove(&id);
    }

    pub(super) fn apply_edge_insert(&mut self, ed: EdgeData) {
        let edge_id = EdgeId(ed.id);
        if self.edges.iter().any(|e| e.id == edge_id) {
            return;
        }
        let from_node = NodeId(ed.from_node);
        let to_node = NodeId(ed.to_node);
        // An edge naming a node this window does not have yet is kept, not
        // dropped: a subscription applies as one burst with no ordering between
        // tables, so edges routinely arrive before their nodes. Dropping them
        // leaves a freshly opened editor showing a fraction of the wires.
        if !self.nodes.contains_key(&from_node) || !self.nodes.contains_key(&to_node) {
            self.pending_edges.push(ed);
            return;
        }
        let from_pin: Arc<str> = Arc::from(ed.from_pin.as_str());
        let to_pin: Arc<str> = Arc::from(ed.to_pin.as_str());
        // A field pin is not a single-slot input: a primary key is referenced
        // by many, so a relation landing on it displaces nothing.
        if !self.is_field_pin(to_node, &to_pin)
            && !self.resolve_input_occupancy(edge_id, to_node, &to_pin)
        {
            // This wire lost the pin to one already there. It is gone from the
            // store by now, so there is nothing left to draw.
            return;
        }
        self.edges.push(EditorEdge {
            id: edge_id,
            from_node,
            from_pin: PinLabel(from_pin),
            to_node,
            to_pin: PinLabel(to_pin),
        });
        self.reindex_edges();
        self.resync_pins(to_node);
    }

    pub(super) fn apply_edge_remove(&mut self, id: EdgeId) {
        self.pending_edges.retain(|ed| ed.id != id.0);
        if let Some(pos) = self.edges.iter().position(|e| e.id == id) {
            let edge = self.edges.remove(pos);
            self.reindex_edges();
            self.resync_pins(edge.to_node);
            // The wire the particles were riding is gone.
            self.runtime.particles.remove(&edge.id);
        }
    }

    /// Retries edges that named a node this window did not have yet. Called once
    /// per drained batch, because the node they were waiting for may have been
    /// in the same batch.
    pub(super) fn resolve_pending_edges(&mut self) {
        if self.pending_edges.is_empty() {
            return;
        }
        for ed in std::mem::take(&mut self.pending_edges) {
            self.apply_edge_insert(ed);
        }
    }
}

/// Whether handling this message reads or changes the shared graph, and
/// therefore has to see the store the local view already shows.
///
/// The settings-editing messages are absent by design: holding them back from
/// the store until the typing stops is exactly what this list protects.
/// `SyncPoll` is absent too -- it commits what has settled instead, and
/// flushing there would make the delay meaningless.
pub(super) fn observes_store(message: &Message) -> bool {
    matches!(
        message,
        Message::EdgeConnected { .. }
            | Message::EdgeDisconnected { .. }
            | Message::OpenGraph(_)
            | Message::AutoLayout
            | Message::GroupMoved { .. }
            | Message::CloneNodes(_)
            // `DeleteNodes` is missing on purpose: it flushes itself, after
            // dropping what the doomed nodes owed.
            | Message::SpawnNode { .. }
            | Message::NodeTriggered { .. }
            | Message::SaveGraph
            | Message::GraphLoaded(_)
    )
}
