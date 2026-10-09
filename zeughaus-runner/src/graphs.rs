//! The graph document this runner holds, persists and serves.
//!
//! [`Document`] is the pure part: every edit is applied to it and answers with
//! the changes it made, which is what editors and the executor are told.
//! [`GraphService`] wraps it with the state directory (one `graphs/<id>.zgh`
//! per top-level graph), the revision counter and the exchange every attached
//! editor holds on [`GRAPH_PATH`](zeughaus_link::GRAPH_PATH).
//!
//! The runner's document is the only truth: an editor's copy is a cache of it,
//! and an editor that falls behind or is refused attaches again and starts from
//! a fresh document instead of repairing its copy.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio::sync::{mpsc, watch};
use weida::{IncomingRequest, OutgoingTransfer, Replier, TransferMeta};
use zeughaus_core::document::{EdgeData, GraphDocument, NodeData};
use zeughaus_link::{GRAPH_MAJOR, GraphChange, GraphEdit, GraphMessage};

use crate::transport::principal_of;

/// How deep a parent chain may be before it is taken for a loop. No graph is
/// nested this deep by hand, and a file pointing back at its own subtree must
/// not hang the runner.
const MAX_DEPTH: usize = 64;

/// How many applied changes are kept for editors that are a few revisions
/// behind. One further back is sent the whole document instead.
const LOG_LEN: usize = 4096;

/// The shortest time between two writes of the graph files: a drag produces an
/// edit per frame, and a file per frame is what the disk does not need.
const WRITE_INTERVAL: Duration = Duration::from_secs(1);

/// The only node type a top-level node may have.
const GRAPH_TYPE: &str = "graph.sub";

/// What one edit did.
#[derive(Debug, Default)]
pub struct Applied {
    /// In the order they must be applied.
    pub changes: Vec<GraphChange>,
    /// The top-level graphs whose file has to be rewritten, found before a
    /// removal takes effect: a deleted graph is among them, and its file goes.
    pub graphs: BTreeSet<u64>,
}

/// Every node and edge, by id. No I/O.
#[derive(Debug, Default)]
pub struct Document {
    nodes: BTreeMap<u64, NodeData>,
    edges: BTreeMap<u64, EdgeData>,
}

impl Document {
    /// Applies one edit, or says why it was refused. A refused edit changes
    /// nothing.
    pub fn apply(&mut self, edit: GraphEdit) -> Result<Applied, String> {
        match edit {
            GraphEdit::CreateNode { node } => {
                if self.nodes.contains_key(&node.id) {
                    return Err(format!("node {} exists", node.id));
                }
                if node.parent != 0 && !self.nodes.contains_key(&node.parent) {
                    return Err(format!("parent {} does not exist", node.parent));
                }
                if node.parent == 0 && node.type_id != GRAPH_TYPE {
                    return Err(format!("a top-level node must be a {GRAPH_TYPE}"));
                }
                let id = node.id;
                self.nodes.insert(id, node.clone());
                Ok(Applied {
                    changes: vec![GraphChange::NodeUpsert { node }],
                    graphs: self.graphs_of([id]),
                })
            }
            GraphEdit::MoveNode { id, x, y } => Ok(self.update(id, |node| {
                node.x = x;
                node.y = y;
            })),
            GraphEdit::SetParams { id, params } => Ok(self.update(id, |node| node.params = params)),
            GraphEdit::RenameNode { id, display_name } => {
                Ok(self.update(id, |node| node.display_name = display_name))
            }
            GraphEdit::DeleteNode { id } => Ok(self.delete(id)),
            GraphEdit::ConnectEdge { edge } => {
                if self.edges.contains_key(&edge.id) {
                    return Err(format!("edge {} exists", edge.id));
                }
                for end in [edge.from_node, edge.to_node] {
                    if !self.nodes.contains_key(&end) {
                        return Err(format!("node {end} does not exist"));
                    }
                }
                let graphs = self.graphs_of([edge.from_node, edge.to_node]);
                self.edges.insert(edge.id, edge.clone());
                Ok(Applied {
                    changes: vec![GraphChange::EdgeInsert { edge }],
                    graphs,
                })
            }
            GraphEdit::DisconnectEdge { id } => {
                let Some(edge) = self.edges.remove(&id) else {
                    return Ok(Applied::default());
                };
                Ok(Applied {
                    changes: vec![GraphChange::EdgeRemove { id }],
                    graphs: self.graphs_of([edge.from_node, edge.to_node]),
                })
            }
        }
    }

    /// Updates a node in place. A node that is gone is a race with a delete,
    /// which is normal and changes nothing.
    fn update(&mut self, id: u64, change: impl FnOnce(&mut NodeData)) -> Applied {
        let Some(node) = self.nodes.get_mut(&id) else {
            return Applied::default();
        };
        change(node);
        let node = node.clone();
        Applied {
            changes: vec![GraphChange::NodeUpsert { node }],
            graphs: self.graphs_of([id]),
        }
    }

    /// Removes a node, everything inside it and every edge that touched any
    /// of them. A container IS its contents: removing the container alone
    /// would leave children parented to a node that no longer exists.
    ///
    /// Edges go first and descendants before ancestors, so an editor applying
    /// the changes in order never holds an edge without its nodes or a node
    /// without its parent.
    fn delete(&mut self, id: u64) -> Applied {
        if !self.nodes.contains_key(&id) {
            return Applied::default();
        }
        let mut children: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for node in self.nodes.values() {
            children.entry(node.parent).or_default().push(node.id);
        }
        let mut order = Vec::new();
        post_order(id, &children, &mut order);
        let doomed: BTreeSet<u64> = order.iter().copied().collect();

        let edges: Vec<EdgeData> = self
            .edges
            .values()
            .filter(|e| doomed.contains(&e.from_node) || doomed.contains(&e.to_node))
            .cloned()
            .collect();
        let mut ends = vec![id];
        for edge in &edges {
            ends.extend([edge.from_node, edge.to_node]);
        }
        let graphs = self.graphs_of(ends);

        let mut changes = Vec::with_capacity(edges.len() + order.len());
        for edge in &edges {
            self.edges.remove(&edge.id);
            changes.push(GraphChange::EdgeRemove { id: edge.id });
        }
        for node in order {
            self.nodes.remove(&node);
            changes.push(GraphChange::NodeRemove { id: node });
        }
        Applied { changes, graphs }
    }

    /// The top-level graph `id` lives in: the ancestor whose parent is `0`.
    /// `None` for an unknown node or a chain that loops.
    pub fn top_level(&self, id: u64) -> Option<u64> {
        let mut current = id;
        for _ in 0..MAX_DEPTH {
            let node = self.nodes.get(&current)?;
            if node.parent == 0 {
                return Some(current);
            }
            current = node.parent;
        }
        None
    }

    fn graphs_of(&self, ids: impl IntoIterator<Item = u64>) -> BTreeSet<u64> {
        ids.into_iter()
            .filter_map(|id| self.top_level(id))
            .collect()
    }

    /// The nodes below `roots` and the roots themselves, parents before
    /// children.
    fn breadth_first(&self, roots: Vec<u64>) -> Vec<&NodeData> {
        let mut children: BTreeMap<u64, Vec<u64>> = BTreeMap::new();
        for node in self.nodes.values() {
            children.entry(node.parent).or_default().push(node.id);
        }
        let mut out = Vec::with_capacity(self.nodes.len());
        let mut queue: VecDeque<u64> = roots.into();
        while let Some(id) = queue.pop_front() {
            if let Some(node) = self.nodes.get(&id) {
                out.push(node);
            }
            if let Some(below) = children.get(&id) {
                queue.extend(below.iter().copied());
            }
        }
        out
    }

    /// One top-level graph: its node, its descendants, and the edges with both
    /// ends inside. Nodes parents-first, which is the order a reader can
    /// insert them in.
    pub fn subtree(&self, top: u64) -> GraphDocument {
        let nodes: Vec<NodeData> = self.breadth_first(vec![top]).into_iter().cloned().collect();
        let inside: BTreeSet<u64> = nodes.iter().map(|n| n.id).collect();
        let edges = self
            .edges
            .values()
            .filter(|e| inside.contains(&e.from_node) && inside.contains(&e.to_node))
            .cloned()
            .collect();
        GraphDocument { nodes, edges }
    }

    /// Everything: nodes parents-first from the roots, then edges by id.
    pub fn to_document(&self) -> GraphDocument {
        let roots = self
            .nodes
            .values()
            .filter(|n| n.parent == 0)
            .map(|n| n.id)
            .collect();
        GraphDocument {
            nodes: self.breadth_first(roots).into_iter().cloned().collect(),
            edges: self.edges.values().cloned().collect(),
        }
    }

    /// Merges named documents, in the given order, into one. Whatever cannot
    /// be part of a consistent document is skipped and reported; the second
    /// element holds one line per skipped row, naming the document.
    pub fn from_documents(docs: Vec<(String, GraphDocument)>) -> (Document, Vec<String>) {
        let mut warnings = Vec::new();
        let mut candidates: BTreeMap<u64, (&str, &NodeData)> = BTreeMap::new();
        for (name, doc) in &docs {
            for node in &doc.nodes {
                if let std::collections::btree_map::Entry::Vacant(slot) = candidates.entry(node.id)
                {
                    slot.insert((name, node));
                } else {
                    warnings.push(format!("{name}: skipping node {}: duplicate id", node.id));
                }
            }
        }

        // Accepted from the roots down, so a node is in only when its whole
        // parent chain is, and a loop of parents never is.
        let mut document = Document::default();
        for (id, (name, node)) in &candidates {
            if node.parent == 0 && node.type_id != GRAPH_TYPE {
                warnings.push(format!(
                    "{name}: skipping node {id}: a top-level node must be a {GRAPH_TYPE}"
                ));
            }
        }
        loop {
            let before = document.nodes.len();
            for (id, (_, node)) in &candidates {
                if document.nodes.contains_key(id) {
                    continue;
                }
                let parent_in = if node.parent == 0 {
                    node.type_id == GRAPH_TYPE
                } else {
                    document.nodes.contains_key(&node.parent)
                };
                if parent_in {
                    document.nodes.insert(*id, (*node).clone());
                }
            }
            if document.nodes.len() == before {
                break;
            }
        }
        for (id, (name, node)) in &candidates {
            if !document.nodes.contains_key(id) && node.parent != 0 {
                warnings.push(format!(
                    "{name}: skipping node {id}: parent {} is absent",
                    node.parent
                ));
            }
        }

        for (name, doc) in &docs {
            for edge in &doc.edges {
                if document.edges.contains_key(&edge.id) {
                    warnings.push(format!("{name}: skipping edge {}: duplicate id", edge.id));
                    continue;
                }
                let missing = [edge.from_node, edge.to_node]
                    .into_iter()
                    .find(|n| !document.nodes.contains_key(n));
                if let Some(node) = missing {
                    warnings.push(format!(
                        "{name}: skipping edge {}: node {node} is absent",
                        edge.id
                    ));
                    continue;
                }
                document.edges.insert(edge.id, edge.clone());
            }
        }
        (document, warnings)
    }
}

fn post_order(id: u64, children: &BTreeMap<u64, Vec<u64>>, out: &mut Vec<u64>) {
    if let Some(below) = children.get(&id) {
        for child in below {
            post_order(*child, children, out);
        }
    }
    out.push(id);
}

struct State {
    doc: Document,
    revision: u64,
    log: VecDeque<(u64, GraphChange)>,
    /// Top-level graphs whose file is behind the document.
    dirty: BTreeSet<u64>,
    last_write: Instant,
}

struct Inner {
    state: Mutex<State>,
    revision: watch::Sender<u64>,
    to_runner: Sender<GraphChange>,
    /// `<state-dir>/graphs`.
    dir: PathBuf,
}

/// What an attached editor still has to be sent.
enum Pending {
    Changes(Vec<(u64, GraphChange)>),
    /// The log no longer reaches back far enough: the document as of the
    /// revision.
    Resync(u64, GraphDocument),
}

/// The runner's document, its files and its editors.
#[derive(Clone)]
pub struct GraphService {
    inner: Arc<Inner>,
}

impl GraphService {
    /// Reads `<state-dir>/graphs/*.zgh` in file-name order. The merged
    /// document is returned for the runner's initial apply; every later change
    /// reaches the runner through the receiver.
    ///
    /// A file that cannot be read or parsed is logged and left alone: it is
    /// never rewritten or deleted, so a typo in a hand edit costs nothing.
    pub fn load(state_dir: &Path) -> (GraphService, Receiver<GraphChange>, GraphDocument) {
        let dir = state_dir.join("graphs");
        let mut files: Vec<PathBuf> = match std::fs::read_dir(&dir) {
            Ok(entries) => entries
                .filter_map(|entry| entry.ok().map(|e| e.path()))
                .filter(|path| path.extension().is_some_and(|ext| ext == "zgh"))
                .collect(),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Vec::new(),
            Err(e) => {
                eprintln!("[graph] cannot read {}: {e}", dir.display());
                Vec::new()
            }
        };
        files.sort_by(|a, b| a.file_name().cmp(&b.file_name()));
        let mut docs = Vec::new();
        for path in files {
            let parsed = std::fs::read(&path)
                .map_err(|e| e.to_string())
                .and_then(|bytes| {
                    serde_json::from_slice::<GraphDocument>(&bytes).map_err(|e| e.to_string())
                });
            match parsed {
                Ok(doc) => {
                    let name = path
                        .file_name()
                        .map_or_else(String::new, |n| n.to_string_lossy().into_owned());
                    docs.push((name, doc));
                }
                Err(e) => eprintln!("[graph] cannot read {}: {e}", path.display()),
            }
        }
        let (doc, warnings) = Document::from_documents(docs);
        for warning in &warnings {
            eprintln!("[graph] {warning}");
        }
        let initial = doc.to_document();
        let graphs = initial.nodes.iter().filter(|n| n.parent == 0).count();
        eprintln!(
            "[graph] loaded {graphs} graph(s), {} nodes, {} edges from {}",
            initial.nodes.len(),
            initial.edges.len(),
            dir.display()
        );

        let (to_runner, receiver) = std::sync::mpsc::channel();
        let (revision, _) = watch::channel(0);
        let service = GraphService {
            inner: Arc::new(Inner {
                state: Mutex::new(State {
                    doc,
                    revision: 0,
                    log: VecDeque::new(),
                    dirty: BTreeSet::new(),
                    last_write: Instant::now(),
                }),
                revision,
                to_runner,
                dir,
            }),
        };
        (service, receiver, initial)
    }

    fn state(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Applies an edit from `principal`: on success the changes are logged
    /// under new revisions, handed to the runner and announced to every
    /// attached editor.
    pub fn edit(&self, principal: &str, edit: GraphEdit) -> Result<(), String> {
        let note = match &edit {
            GraphEdit::DeleteNode { id } => Some(format!("delete node {id}")),
            GraphEdit::DisconnectEdge { id } => Some(format!("disconnect edge {id}")),
            _ => None,
        };
        let revision = {
            let mut state = self.state();
            let applied = match state.doc.apply(edit) {
                Ok(applied) => applied,
                Err(message) => {
                    drop(state);
                    eprintln!("[graph] {principal}: refused: {message}");
                    return Err(message);
                }
            };
            if let Some(note) = note {
                eprintln!("[graph] {principal}: {note}");
            }
            if applied.changes.is_empty() {
                return Ok(());
            }
            for change in applied.changes {
                state.revision += 1;
                let revision = state.revision;
                state.log.push_back((revision, change.clone()));
                if state.log.len() > LOG_LEN {
                    state.log.pop_front();
                }
                // The runner is gone only when the process is ending.
                let _ = self.inner.to_runner.send(change);
            }
            state.dirty.extend(applied.graphs);
            state.revision
        };
        self.inner.revision.send_replace(revision);
        Ok(())
    }

    /// Writes the graph files that are behind the document: when `force`, or
    /// when the last write was at least a second ago. A write that failed is
    /// tried again on the next call.
    pub fn persist(&self, force: bool) {
        let (writes, removals) = {
            let mut state = self.state();
            if state.dirty.is_empty() || (!force && state.last_write.elapsed() < WRITE_INTERVAL) {
                return;
            }
            let dirty = std::mem::take(&mut state.dirty);
            state.last_write = Instant::now();
            let mut writes = Vec::new();
            let mut removals = Vec::new();
            for id in dirty {
                if state.doc.nodes.get(&id).is_some_and(|n| n.parent == 0) {
                    writes.push((id, state.doc.subtree(id)));
                } else {
                    removals.push(id);
                }
            }
            (writes, removals)
        };
        let mut failed = Vec::new();
        for (id, doc) in writes {
            let path = self.file_of(id);
            if let Err(e) = crate::files::write_json(&path, &doc) {
                eprintln!("[graph] cannot write {}: {e}", path.display());
                failed.push(id);
            }
        }
        for id in removals {
            let path = self.file_of(id);
            match std::fs::remove_file(&path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => {
                    eprintln!("[graph] cannot remove {}: {e}", path.display());
                    failed.push(id);
                }
            }
        }
        if !failed.is_empty() {
            self.state().dirty.extend(failed);
        }
    }

    fn file_of(&self, id: u64) -> PathBuf {
        self.inner.dir.join(format!("{id}.zgh"))
    }

    /// The document and the revision it is at, as one consistent pair.
    fn snapshot(&self) -> (u64, GraphDocument) {
        let state = self.state();
        (state.revision, state.doc.to_document())
    }

    /// What an editor that has been sent everything up to `sent` is owed.
    fn since(&self, sent: u64) -> Pending {
        let state = self.state();
        let reaches = match state.log.front() {
            Some((first, _)) => *first <= sent + 1,
            None => state.revision == sent,
        };
        if !reaches {
            return Pending::Resync(state.revision, state.doc.to_document());
        }
        Pending::Changes(
            state
                .log
                .iter()
                .filter(|(revision, _)| *revision > sent)
                .cloned()
                .collect(),
        )
    }

    /// Accepts exchanges until the replier goes away, which for this process
    /// means never.
    pub async fn accept(self, replier: Replier) {
        loop {
            match replier.accept().await {
                Ok(request) => {
                    let service = self.clone();
                    tokio::spawn(async move { service.serve(request).await });
                }
                Err(e) => {
                    eprintln!("[graph] stopped accepting: {e}");
                    return;
                }
            }
        }
    }

    /// One editor's exchange, from its `Attach` to the end of either half.
    async fn serve(self, mut request: IncomingRequest) {
        let Some(principal) = principal_of(request.meta()) else {
            eprintln!("[graph] refused an exchange from a peer that proved no identity");
            request.refuse(weida::ErrorCode::Rejected).await;
            return;
        };
        let mut body = request.take_body();
        let canceled = request.canceled();
        let first = match read_message(&mut body).await {
            Ok(Some(message)) => message,
            Ok(None) => {
                request.refuse(weida::ErrorCode::Rejected).await;
                return;
            }
            Err(e) => {
                eprintln!("[graph] {principal}: unreadable first frame: {e}");
                request.refuse(weida::ErrorCode::Rejected).await;
                return;
            }
        };
        let mut reply = match request.reply(TransferMeta::default()).await {
            Ok(reply) => reply,
            Err(e) => {
                eprintln!("[graph] {principal}: cannot open the reply half: {e}");
                return;
            }
        };
        match first {
            GraphMessage::Attach { major } if major == GRAPH_MAJOR => {}
            other => {
                let message = match other {
                    GraphMessage::Attach { major } => {
                        format!("graph protocol {major} is not {GRAPH_MAJOR}")
                    }
                    _ => "the first frame must be Attach".to_owned(),
                };
                let _ = write_message(
                    &mut reply,
                    &GraphMessage::Refused {
                        request: 0,
                        message,
                    },
                )
                .await;
                let _ = reply.finish();
                return;
            }
        }

        if let Err(e) = self.exchange(&principal, body, &mut reply, canceled).await {
            eprintln!("[graph] {principal}: {e}");
        }
        let _ = reply.finish();
    }

    async fn exchange(
        &self,
        principal: &str,
        mut body: weida::IncomingTransfer,
        reply: &mut OutgoingTransfer,
        canceled: impl Future<Output = ()>,
    ) -> Result<(), String> {
        // Subscribed before the snapshot: a change landing in between wakes
        // the loop below, which then finds nothing newer than the snapshot.
        let mut revision = self.inner.revision.subscribe();
        revision.borrow_and_update();
        let (mut sent, document) = self.snapshot();
        write_message(
            reply,
            &GraphMessage::Attached {
                revision: sent,
                document,
            },
        )
        .await?;

        // Frames are read on their own task and forwarded whole: a read that
        // is dropped half-way by a `select!` loses the bytes it consumed.
        let (frames, mut incoming) = mpsc::channel::<GraphMessage>(64);
        let reader_principal = principal.to_owned();
        let reader = tokio::spawn(async move {
            loop {
                match read_message(&mut body).await {
                    Ok(Some(message)) => {
                        if frames.send(message).await.is_err() {
                            return;
                        }
                    }
                    Ok(None) => return,
                    Err(e) => {
                        eprintln!("[graph] {reader_principal}: {e}");
                        return;
                    }
                }
            }
        });

        let mut canceled = std::pin::pin!(canceled);
        let result = loop {
            tokio::select! {
                () = &mut canceled => break Ok(()),
                changed = revision.changed() => {
                    if changed.is_err() {
                        break Ok(());
                    }
                    revision.borrow_and_update();
                    match self.since(sent) {
                        Pending::Changes(changes) => {
                            for (number, change) in changes {
                                let message = GraphMessage::Changed { revision: number, change };
                                if let Err(e) = write_message(reply, &message).await {
                                    reader.abort();
                                    return Err(e);
                                }
                                sent = number;
                            }
                        }
                        Pending::Resync(number, document) => {
                            let message = GraphMessage::Attached { revision: number, document };
                            if let Err(e) = write_message(reply, &message).await {
                                reader.abort();
                                return Err(e);
                            }
                            sent = number;
                        }
                    }
                }
                message = incoming.recv() => match message {
                    Some(GraphMessage::Edit { request, edit }) => {
                        if let Err(message) = self.edit(principal, edit) {
                            let refused = GraphMessage::Refused { request, message };
                            if let Err(e) = write_message(reply, &refused).await {
                                break Err(e);
                            }
                        }
                    }
                    Some(_) => break Err("an unexpected frame on a graph exchange".to_owned()),
                    None => break Ok(()),
                },
            }
        };
        reader.abort();
        result
    }
}

/// Reads one frame. `None` when the stream finished at a frame boundary.
async fn read_message<R: AsyncRead + Unpin>(
    reader: &mut R,
) -> Result<Option<GraphMessage>, String> {
    let mut header = [0u8; 4];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(format!("read: {e}")),
    }
    let len = GraphMessage::body_len(header)?;
    let mut body = vec![0u8; len];
    reader
        .read_exact(&mut body)
        .await
        .map_err(|e| format!("read: {e}"))?;
    GraphMessage::decode(&body).map(Some)
}

/// Writes one frame.
async fn write_message(
    writer: &mut OutgoingTransfer,
    message: &GraphMessage,
) -> Result<(), String> {
    let bytes = message.encode()?;
    writer
        .write_all(&bytes)
        .await
        .map_err(|e| format!("write: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    use weida::{ClientTls, EndpointAddr, Runtime, RuntimeConfig, Trust};
    use zeughaus_link::{GRAPH_PATH, credentials};

    use crate::transport::Transport;

    static NEXT: AtomicU64 = AtomicU64::new(0);

    /// A state directory that removes itself.
    struct TempDir(PathBuf);

    impl TempDir {
        fn new(tag: &str) -> TempDir {
            let mut path = std::env::temp_dir();
            path.push(format!(
                "zeughaus-graphs-{tag}-{}-{}",
                std::process::id(),
                NEXT.fetch_add(1, Ordering::Relaxed)
            ));
            let _ = std::fs::remove_dir_all(&path);
            TempDir(path)
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn node(id: u64, parent: u64) -> NodeData {
        NodeData {
            id,
            type_id: if parent == 0 {
                GRAPH_TYPE.to_owned()
            } else {
                "transform.add".to_owned()
            },
            display_name: format!("n{id}"),
            x: 0.0,
            y: 0.0,
            parent,
            params: Vec::new(),
        }
    }

    fn edge(id: u64, from: u64, to: u64) -> EdgeData {
        EdgeData {
            id,
            from_node: from,
            from_pin: "out".to_owned(),
            to_node: to,
            to_pin: "in".to_owned(),
        }
    }

    fn create(node: NodeData) -> GraphEdit {
        GraphEdit::CreateNode { node }
    }

    fn connect(edge: EdgeData) -> GraphEdit {
        GraphEdit::ConnectEdge { edge }
    }

    /// Graphs 1 (a container 2 holding 3, plus 4) and 5, with edges 10 (3 -> 4),
    /// 11 (4 -> 3) and 12 (3 -> 6, into the other graph).
    fn sample() -> Document {
        let mut doc = Document::default();
        for edit in [
            create(node(1, 0)),
            create(node(2, 1)),
            create(node(3, 2)),
            create(node(4, 1)),
            create(node(5, 0)),
            create(node(6, 5)),
            connect(edge(11, 4, 3)),
            connect(edge(10, 3, 4)),
            connect(edge(12, 3, 6)),
        ] {
            doc.apply(edit).expect("sample edit");
        }
        doc
    }

    #[test]
    fn deleting_a_container_removes_what_it_holds_edges_first() {
        let mut doc = sample();
        let applied = doc.apply(GraphEdit::DeleteNode { id: 2 }).expect("delete");
        assert_eq!(
            applied.changes,
            vec![
                GraphChange::EdgeRemove { id: 10 },
                GraphChange::EdgeRemove { id: 11 },
                GraphChange::EdgeRemove { id: 12 },
                GraphChange::NodeRemove { id: 3 },
                GraphChange::NodeRemove { id: 2 },
            ]
        );
        assert_eq!(applied.graphs, BTreeSet::from([1, 5]));
        let rest = doc.to_document();
        let ids: Vec<u64> = rest.nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids, vec![1, 5, 4, 6]);
        assert!(rest.edges.is_empty());
    }

    #[test]
    fn deleting_a_graph_removes_its_nodes_deepest_first() {
        let mut doc = sample();
        let applied = doc.apply(GraphEdit::DeleteNode { id: 1 }).expect("delete");
        let removed: Vec<u64> = applied
            .changes
            .iter()
            .filter_map(|c| match c {
                GraphChange::NodeRemove { id } => Some(*id),
                _ => None,
            })
            .collect();
        assert_eq!(removed, vec![3, 2, 4, 1]);
        assert!(applied.graphs.contains(&1));
        assert_eq!(doc.top_level(1), None);
    }

    #[test]
    fn edits_of_a_missing_node_change_nothing() {
        let mut doc = sample();
        for edit in [
            GraphEdit::MoveNode {
                id: 99,
                x: 1.0,
                y: 2.0,
            },
            GraphEdit::SetParams {
                id: 99,
                params: Vec::new(),
            },
            GraphEdit::RenameNode {
                id: 99,
                display_name: "x".into(),
            },
            GraphEdit::DeleteNode { id: 99 },
            GraphEdit::DisconnectEdge { id: 99 },
        ] {
            let applied = doc.apply(edit).expect("not an error");
            assert!(applied.changes.is_empty());
        }
    }

    #[test]
    fn a_node_edit_emits_the_whole_node() {
        let mut doc = sample();
        let applied = doc
            .apply(GraphEdit::MoveNode {
                id: 3,
                x: 4.0,
                y: 5.0,
            })
            .expect("move");
        let GraphChange::NodeUpsert { node } = &applied.changes[0] else {
            panic!("expected an upsert");
        };
        assert_eq!((node.id, node.x, node.y, node.parent), (3, 4.0, 5.0, 2));
        assert_eq!(applied.graphs, BTreeSet::from([1]));
    }

    #[test]
    fn create_and_connect_refusals() {
        let mut doc = sample();
        assert_eq!(doc.apply(create(node(3, 2))).unwrap_err(), "node 3 exists");
        assert_eq!(
            doc.apply(create(node(40, 77))).unwrap_err(),
            "parent 77 does not exist"
        );
        let mut loose = node(41, 0);
        loose.type_id = "transform.add".to_owned();
        assert_eq!(
            doc.apply(create(loose)).unwrap_err(),
            "a top-level node must be a graph.sub"
        );
        assert_eq!(
            doc.apply(connect(edge(10, 3, 4))).unwrap_err(),
            "edge 10 exists"
        );
        assert_eq!(
            doc.apply(connect(edge(50, 3, 99))).unwrap_err(),
            "node 99 does not exist"
        );
        assert_eq!(
            doc.apply(connect(edge(51, 98, 3))).unwrap_err(),
            "node 98 does not exist"
        );
    }

    #[test]
    fn a_subtree_holds_one_graph_and_its_inner_edges() {
        let doc = sample();
        let one = doc.subtree(1);
        let ids: Vec<u64> = one.nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids, vec![1, 2, 4, 3]);
        let edges: Vec<u64> = one.edges.iter().map(|e| e.id).collect();
        assert_eq!(edges, vec![10, 11]);
    }

    #[test]
    fn merging_skips_a_duplicate_id_and_an_orphan_with_a_warning() {
        let a = GraphDocument {
            nodes: vec![node(1, 0), node(2, 1)],
            edges: vec![edge(10, 1, 2)],
        };
        let b = GraphDocument {
            nodes: vec![node(1, 0), node(7, 99), node(8, 9), node(9, 8), node(5, 0)],
            edges: vec![edge(10, 1, 5), edge(11, 5, 99)],
        };
        let (doc, warnings) =
            Document::from_documents(vec![("a.zgh".into(), a), ("b.zgh".into(), b)]);
        let ids: Vec<u64> = doc.to_document().nodes.iter().map(|n| n.id).collect();
        assert_eq!(ids, vec![1, 5, 2]);
        let edges: Vec<u64> = doc.to_document().edges.iter().map(|e| e.id).collect();
        assert_eq!(edges, vec![10]);
        let all = warnings.join("\n");
        assert!(
            all.contains("b.zgh: skipping node 1: duplicate id"),
            "{all}"
        );
        assert!(
            all.contains("b.zgh: skipping node 7: parent 99 is absent"),
            "{all}"
        );
        assert!(
            all.contains("skipping node 8"),
            "a parent loop is never reachable: {all}"
        );
        assert!(
            all.contains("b.zgh: skipping edge 10: duplicate id"),
            "{all}"
        );
        assert!(all.contains("b.zgh: skipping edge 11"), "{all}");
    }

    #[test]
    fn persisted_graphs_load_back_one_file_per_graph() {
        let dir = TempDir::new("persist");
        let (service, _changes, initial) = GraphService::load(&dir.0);
        assert!(initial.nodes.is_empty());
        for edit in [
            create(node(1, 0)),
            create(node(2, 1)),
            create(node(5, 0)),
            create(node(6, 5)),
            connect(edge(10, 2, 1)),
        ] {
            service.edit("test", edit).expect("edit");
        }
        service.persist(true);

        let mut files: Vec<String> = std::fs::read_dir(dir.0.join("graphs"))
            .expect("graphs dir")
            .map(|e| e.expect("entry").file_name().to_string_lossy().into_owned())
            .collect();
        files.sort();
        assert_eq!(files, vec!["1.zgh", "5.zgh"]);

        let (_, expected) = service.snapshot();
        let (_again, _rx, loaded) = GraphService::load(&dir.0);
        assert_eq!(loaded, expected);

        service
            .edit("test", GraphEdit::DeleteNode { id: 1 })
            .expect("delete");
        service.persist(true);
        assert!(!dir.0.join("graphs").join("1.zgh").exists());
        assert!(dir.0.join("graphs").join("5.zgh").exists());
    }

    #[test]
    fn an_unreadable_file_is_skipped_and_left_alone() {
        let dir = TempDir::new("broken");
        std::fs::create_dir_all(dir.0.join("graphs")).expect("dir");
        let broken = dir.0.join("graphs").join("1.zgh");
        std::fs::write(&broken, "not json").expect("write");
        let (service, _rx, initial) = GraphService::load(&dir.0);
        assert!(initial.nodes.is_empty());
        service.edit("test", create(node(2, 0))).expect("edit");
        service.persist(true);
        assert_eq!(std::fs::read_to_string(&broken).expect("kept"), "not json");
    }

    fn loopback() -> std::net::SocketAddr {
        std::net::SocketAddr::from(([127, 0, 0, 1], 0))
    }

    struct Editor {
        request: OutgoingTransfer,
        reply: weida::IncomingTransfer,
    }

    impl Editor {
        async fn send(&mut self, message: GraphMessage) {
            write_message(&mut self.request, &message)
                .await
                .expect("write");
        }

        async fn next(&mut self) -> GraphMessage {
            tokio::time::timeout(Duration::from_secs(20), read_message(&mut self.reply))
                .await
                .expect("a frame within 20 s")
                .expect("a readable frame")
                .expect("an open stream")
        }
    }

    async fn attach(requester: &weida::Requester) -> Editor {
        let (mut request, reply) = requester.open(TransferMeta::default()).await.expect("open");
        write_message(&mut request, &GraphMessage::Attach { major: GRAPH_MAJOR })
            .await
            .expect("attach");
        let reply = reply.recv().await.expect("reply half");
        Editor { request, reply }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn editors_see_each_others_changes_and_only_the_sender_a_refusal() {
        let dir = TempDir::new("protocol");
        let transport = Transport::start(loopback(), &dir.0).await.expect("start");
        let (graphs, _changes, _initial) = GraphService::load(&dir.0);
        let replier = transport
            .listener()
            .replier(GRAPH_PATH)
            .expect("graph endpoint");
        tokio::spawn(graphs.clone().accept(replier));

        let mut addr = EndpointAddr::parse(transport.url()).expect("announced url");
        addr.path = GRAPH_PATH.to_owned();
        let url = addr.to_string();
        let identity = credentials::load_client_identity(&dir.0).expect("client identity");
        let runtime = Runtime::new(RuntimeConfig::default()).expect("client runtime");
        let mut editors = Vec::new();
        for _ in 0..2 {
            let requester = runtime
                .requester(ClientTls::new(Trust::by_address()).with_identity(identity.clone()));
            requester.connect(&url).await.expect("connect");
            editors.push((attach(&requester).await, requester));
        }
        let [(a, _ra), (b, _rb)] = &mut editors[..] else {
            unreachable!()
        };

        for editor in [&mut *a, &mut *b] {
            assert_eq!(
                editor.next().await,
                GraphMessage::Attached {
                    revision: 0,
                    document: GraphDocument::default()
                }
            );
        }

        a.send(GraphMessage::Edit {
            request: 1,
            edit: create(node(1, 0)),
        })
        .await;
        let created = GraphMessage::Changed {
            revision: 1,
            change: GraphChange::NodeUpsert { node: node(1, 0) },
        };
        assert_eq!(a.next().await, created);
        assert_eq!(b.next().await, created);

        a.send(GraphMessage::Edit {
            request: 2,
            edit: connect(edge(9, 1, 404)),
        })
        .await;
        assert_eq!(
            a.next().await,
            GraphMessage::Refused {
                request: 2,
                message: "node 404 does not exist".to_owned()
            }
        );

        // B's next frame is the following change, so no refusal came its way.
        a.send(GraphMessage::Edit {
            request: 3,
            edit: create(node(2, 0)),
        })
        .await;
        let second = GraphMessage::Changed {
            revision: 2,
            change: GraphChange::NodeUpsert { node: node(2, 0) },
        };
        assert_eq!(a.next().await, second);
        assert_eq!(b.next().await, second);
    }
}
