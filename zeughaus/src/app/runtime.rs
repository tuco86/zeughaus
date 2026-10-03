//! What the editor knows about the runtimes executing its graphs: the values
//! and failures they reported, the video feeds they serve, and the particles
//! those deliveries leave on the wires.
//!
//! Every connected runner is one [`RunnerLink`]. A node's values come from
//! the runner of its top-level graph, and a runner that moved or left takes
//! only its own graphs' values with it.

#[cfg(not(target_arch = "wasm32"))]
use std::collections::BTreeMap;
use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;

#[cfg(not(target_arch = "wasm32"))]
use iced::Task;
#[cfg(not(target_arch = "wasm32"))]
use zeughaus_core::{EdgeId, Image, Ty};
use zeughaus_core::{NodeId, Value};

use super::App;
#[cfg(not(target_arch = "wasm32"))]
use super::node_view::{DISPLAY_SIZE, is_display};
#[cfg(not(target_arch = "wasm32"))]
use crate::feed::{self, FeedKey, FeedSpec, FrameOrder};
#[cfg(not(target_arch = "wasm32"))]
use crate::message::Message;
#[cfg(not(target_arch = "wasm32"))]
use crate::transport::Endpoint;
#[cfg(not(target_arch = "wasm32"))]
use crate::workspace::RunnerKey;

/// How fast a particle travels along its cable, in world units per second.
/// Fast enough to read as a message in flight, slow enough to be seen on a
/// short wire.
#[cfg(not(target_arch = "wasm32"))]
pub(super) const PARTICLE_SPEED: f32 = 240.0;

/// Shortest gap between two particles on one edge: at most ten per second. A
/// 30 Hz source would otherwise smear into a solid line, which says less than
/// countable dots do.
#[cfg(not(target_arch = "wasm32"))]
const PARTICLE_MIN_GAP: std::time::Duration = std::time::Duration::from_millis(100);

/// Most particles kept per edge. A born time whose distance is past the cable
/// simply is not drawn, so the cap only bounds the memory a fast edge holds.
#[cfg(not(target_arch = "wasm32"))]
const PARTICLES_PER_EDGE: usize = 32;

/// One live feed: the task reading it, what it was asked for, and the newest
/// frame it produced.
///
/// The handle aborts on drop, which makes the map entry the feed's whole
/// lifetime: removing it here stops the task, and a feed can therefore not
/// outlive the node that wanted it. One that did would keep receiving 33 MB
/// frames for a node nobody can see.
#[cfg(not(target_arch = "wasm32"))]
pub(super) struct LiveFeed {
    /// The tier this feed was opened with. A resize that snaps to the same
    /// ladder tier leaves it unchanged, which is what keeps a drag from
    /// restarting a video stream.
    tier: u32,
    /// The runner serving it: a runner that moved or left ends its feeds.
    runner: RunnerKey,
    /// Which generation of this feed the task belongs to.
    epoch: u64,
    /// Newest frame accepted, with the order it arrived in.
    latest: Option<(FrameOrder, Image)>,
    _task: iced::task::Handle,
}

/// One connected runner as this editor talks to it.
#[cfg(not(target_arch = "wasm32"))]
pub(super) struct RunnerLink {
    /// Where it serves. A different one means every address dialled is stale.
    pub endpoint: Endpoint,
    /// What its section is called: host and the start of its fingerprint.
    pub label: String,
    /// The task subscribed to its events. Aborts on drop.
    pub traffic: Option<iced::task::Handle>,
    /// Whether that subscription is live. Values on screen are last-known
    /// while it is not, which the status bar says.
    pub traffic_live: bool,
    /// Which subscription the traffic on screen came from, drawn from the
    /// editor-wide counter so a message names exactly one link.
    pub traffic_epoch: u64,
    /// The CI machine's busy state as the runner last reported it; `None`
    /// for a runner without CI, and while its traffic is lost.
    pub machine: Option<zeughaus_link::MachineState>,
    /// The sequence that state came with. A snapshot sets it outright,
    /// because a restarted runner counts from zero again.
    pub machine_seq: u64,
}

/// Everything this editor knows about the runtimes executing its graphs.
///
/// One struct rather than a dozen fields on [`App`], because it is one
/// subject: a runtime that moved invalidates what it reported. A value, a
/// failure, a particle and a frame from a runner that is gone say nothing
/// about the one that replaced it.
///
/// The browser editor has no sync layer and therefore no runtime to hear:
/// what it reports stays empty, and the display code reads it either way
/// rather than through a second, value-less path.
#[derive(Default)]
pub(super) struct RuntimeView {
    /// The runtime's values as last reported, per node and pin. Kept by node
    /// rather than by wire because a value can arrive before the node row it
    /// belongs to -- and because this window computes nothing itself, so what
    /// the runtime said is the whole of what it can draw.
    pub remote_outputs: HashMap<NodeId, HashMap<String, Value>>,
    /// Why each node is failing, as last reported by the runtime. The only
    /// path a failure has to this window: the process that ran the node is not
    /// the one drawing it.
    pub remote_errors: HashMap<NodeId, String>,
    /// Every connected runner that serves an endpoint, by fingerprint.
    #[cfg(not(target_arch = "wasm32"))]
    pub links: BTreeMap<RunnerKey, RunnerLink>,
    /// Hands out the epochs of event subscriptions and mux attachments, one
    /// counter for both so a queued message names exactly one of them.
    #[cfg(not(target_arch = "wasm32"))]
    pub next_epoch: u64,
    /// Live video feeds, keyed by the source pin they carry rather than by the
    /// Display node drawing it: two nodes watching one pin need the same
    /// frame, so they share one feed.
    #[cfg(not(target_arch = "wasm32"))]
    pub feeds: HashMap<FeedKey, LiveFeed>,
    /// Hands out feed generations. A replaced feed numbers its frames from
    /// scratch, so the epoch -- not the sequence -- is what keeps a frame
    /// still in flight from the old one out of the new one.
    #[cfg(not(target_arch = "wasm32"))]
    pub feed_epoch: u64,
    /// Frames accepted since startup, for the status bar. A feed count alone
    /// cannot be told from a stall; a number that climbs can.
    #[cfg(not(target_arch = "wasm32"))]
    pub frames_received: u64,
    /// The newest sequence applied per (node, pin). See
    /// [`App::accept_output_seq`].
    #[cfg(not(target_arch = "wasm32"))]
    pub output_seq: HashMap<(NodeId, String), u64>,
    /// The newest sequence applied per node. Same guard as the outputs: the
    /// error topic is its own stream, so a stale report can arrive last.
    #[cfg(not(target_arch = "wasm32"))]
    pub error_seq: HashMap<NodeId, u64>,
    /// When each in-flight particle was born, per edge. One per delivered
    /// value, which is why an unchanged value still animates: traffic, not
    /// state.
    #[cfg(not(target_arch = "wasm32"))]
    pub particles: HashMap<EdgeId, std::collections::VecDeque<iced::time::Instant>>,
    /// The newest sequence applied per refused setting. Its own guard, keyed
    /// by (node, setting): a refusal is about one setting, so one setting's
    /// late report must not silence another's.
    #[cfg(not(target_arch = "wasm32"))]
    pub rejection_seq: HashMap<(NodeId, String), u64>,
    /// Which runner last reported something about a node. A report can
    /// arrive before the node's row or its graph's row, so the node's graph
    /// cannot say whose report it was; this can, when that runner goes.
    #[cfg(not(target_arch = "wasm32"))]
    pub reported_by: HashMap<NodeId, RunnerKey>,
    /// Which runner last reported a delivery on an edge, for the same reason.
    #[cfg(not(target_arch = "wasm32"))]
    pub edge_reported_by: HashMap<EdgeId, RunnerKey>,
}

impl App {
    /// Applies what the runtime reported: values, their absence, and the edges
    /// a value crossed.
    ///
    /// This is the only way a value ever reaches the editor -- it computes
    /// nothing itself -- and the only place particles are born.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn apply_traffic(&mut self, epoch: u64, traffic: crate::feed::Traffic) {
        use crate::feed::Traffic;
        use zeughaus_link::RuntimeEvent;

        // A task that has been replaced may still have messages queued: its
        // snapshot would clear the values the current runtime just delivered.
        // The epoch is which subscription asked for them, and so which runner.
        let Some(key) = self
            .runtime
            .links
            .iter()
            .find(|(_, link)| link.traffic_epoch == epoch)
            .map(|(key, _)| key.clone())
        else {
            return;
        };
        match traffic {
            Traffic::Snapshot(snapshot) => {
                // The whole set replaces the whole set this runner reported:
                // a pin the snapshot does not name produced nothing, which is
                // what the editor dims, and a node it does not name is not
                // failing. Another runner's graphs are not its to clear.
                self.forget_runner_values(&key, false);
                for row in snapshot.errors {
                    let node = NodeId(row.node_id);
                    self.runtime.reported_by.insert(node, key.clone());
                    self.runtime.error_seq.insert(node, snapshot.seq);
                    self.runtime.remote_errors.insert(node, row.message);
                }
                for row in snapshot.rejections {
                    let node = NodeId(row.node_id);
                    self.runtime.reported_by.insert(node, key.clone());
                    self.runtime
                        .rejection_seq
                        .insert((node, row.key.clone()), snapshot.seq);
                    self.setting_errors
                        .entry(node)
                        .or_default()
                        .insert(row.key, row.message);
                }
                for row in snapshot.outputs {
                    let Some(value) = zeughaus_core::decode_scalar(&row.ty, &row.value) else {
                        continue;
                    };
                    let node = NodeId(row.node_id);
                    self.runtime.reported_by.insert(node, key.clone());
                    self.runtime
                        .output_seq
                        .insert((node, row.pin.clone()), snapshot.seq);
                    self.runtime
                        .remote_outputs
                        .entry(node)
                        .or_default()
                        .insert(row.pin, value);
                }
                if let Some(link) = self.runtime.links.get_mut(&key) {
                    link.traffic_live = true;
                    link.machine = snapshot.machine;
                    link.machine_seq = snapshot.seq;
                }
                self.update_display_values();
            }
            Traffic::Event(RuntimeEvent::Output {
                seq,
                node_id,
                pin,
                ty,
                value,
            }) => {
                let node = NodeId(node_id);
                if !self.accept_output_seq(node, &pin, seq) {
                    return;
                }
                let Some(value) = zeughaus_core::decode_scalar(&ty, &value) else {
                    return;
                };
                self.runtime.reported_by.insert(node, key.clone());
                self.runtime
                    .remote_outputs
                    .entry(node)
                    .or_default()
                    .insert(pin, value);
                self.reapply_remote_outputs(node);
            }
            Traffic::Event(RuntimeEvent::OutputCleared { seq, node_id, pin }) => {
                let node = NodeId(node_id);
                if !self.accept_output_seq(node, &pin, seq) {
                    return;
                }
                self.runtime.reported_by.insert(node, key.clone());
                if let Some(pins) = self.runtime.remote_outputs.get_mut(&node) {
                    pins.remove(&pin);
                }
                self.reapply_remote_outputs(node);
            }
            Traffic::Event(RuntimeEvent::Edge { edge_id, .. }) => {
                let now = iced::time::Instant::now();
                self.runtime
                    .edge_reported_by
                    .insert(EdgeId(edge_id), key.clone());
                let particles = self.runtime.particles.entry(EdgeId(edge_id)).or_default();
                // A 30 Hz source would otherwise smear into a solid line, which
                // says less than a countable dot does.
                let too_soon = particles
                    .back()
                    .is_some_and(|born| now.duration_since(*born) < PARTICLE_MIN_GAP);
                if !too_soon {
                    particles.push_back(now);
                }
                while particles.len() > PARTICLES_PER_EDGE {
                    particles.pop_front();
                }
            }
            Traffic::Event(RuntimeEvent::NodeError {
                seq,
                node_id,
                message,
            }) => {
                let node = NodeId(node_id);
                if self.accept_error_seq(node, seq) {
                    self.runtime.reported_by.insert(node, key.clone());
                    self.runtime.remote_errors.insert(node, message);
                }
            }
            Traffic::Event(RuntimeEvent::NodeErrorCleared { seq, node_id }) => {
                let node = NodeId(node_id);
                if self.accept_error_seq(node, seq) {
                    self.runtime.reported_by.insert(node, key.clone());
                    self.runtime.remote_errors.remove(&node);
                }
            }
            // A refused setting is not a failed run, so it goes where the
            // reason belongs -- under the field -- and raises none of the
            // alarm a failure raises: no red border, no status-bar ERROR.
            Traffic::Event(RuntimeEvent::SettingRejected {
                seq,
                node_id,
                key: setting,
                message,
            }) => {
                let node = NodeId(node_id);
                if self.accept_rejection_seq(node, &setting, seq) {
                    self.runtime.reported_by.insert(node, key.clone());
                    self.setting_errors
                        .entry(node)
                        .or_default()
                        .insert(setting, message);
                }
            }
            Traffic::Event(RuntimeEvent::SettingAccepted {
                seq,
                node_id,
                key: setting,
            }) => {
                let node = NodeId(node_id);
                if self.accept_rejection_seq(node, &setting, seq) {
                    self.runtime.reported_by.insert(node, key.clone());
                    self.record_setting_error(node, &setting, None);
                }
            }
            Traffic::Event(RuntimeEvent::Machine { seq, mode, busy }) => {
                if let Some(link) = self.runtime.links.get_mut(&key)
                    && seq > link.machine_seq
                {
                    link.machine = Some(zeughaus_link::MachineState { mode, busy });
                    link.machine_seq = seq;
                }
            }
            Traffic::Lost => {
                if let Some(link) = self.runtime.links.get_mut(&key) {
                    link.traffic_live = false;
                    // A toggle for a machine nobody answers for would set
                    // nothing.
                    link.machine = None;
                }
                // Values stay on screen as last-known, because the status bar
                // says so and a number nobody claims is still the last one
                // anybody claimed. An error is not like that: it is a claim
                // about a run, made by a process that is no longer there to
                // make it, and a red border with nothing behind it is worse
                // than none. Same for a refusal.
                self.forget_runner_values(&key, true);
            }
        }
    }

    /// Forgets what `runner` reported about the nodes of its graphs, and
    /// about any node it reported on before that node's graph was known:
    /// their failures and refused settings, and unless `claims_only`, their
    /// values, sequences and the particles on their wires too.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn forget_runner_values(&mut self, runner: &RunnerKey, claims_only: bool) {
        let mut owned: std::collections::HashSet<NodeId> = self
            .nodes
            .keys()
            .copied()
            .filter(|id| self.runner_of(*id) == Some(runner.as_str()))
            .collect();
        owned.extend(
            self.runtime
                .reported_by
                .iter()
                .filter(|(_, by)| *by == runner)
                .map(|(id, _)| *id),
        );
        let runtime = &mut self.runtime;
        runtime.remote_errors.retain(|id, _| !owned.contains(id));
        runtime.error_seq.retain(|id, _| !owned.contains(id));
        runtime
            .rejection_seq
            .retain(|(id, _), _| !owned.contains(id));
        self.setting_errors.retain(|id, _| !owned.contains(id));
        if claims_only {
            return;
        }
        runtime.remote_outputs.retain(|id, _| !owned.contains(id));
        runtime.output_seq.retain(|(id, _), _| !owned.contains(id));
        for edge in self.edges.iter().filter(|e| owned.contains(&e.from_node)) {
            runtime.particles.remove(&edge.id);
        }
        runtime.edge_reported_by.retain(|edge, by| {
            let reported = by == runner;
            if reported {
                runtime.particles.remove(edge);
            }
            !reported
        });
        runtime.reported_by.retain(|id, _| !owned.contains(id));
    }

    /// Whether this report is newer than what was already applied for that pin.
    ///
    /// Pub/Sub messages travel on separate QUIC streams and the snapshot is
    /// fetched concurrently, so an older message can arrive last. Storing the
    /// sequence per pin is what keeps it from overwriting a fresh value.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn accept_output_seq(&mut self, node: NodeId, pin: &str, seq: u64) -> bool {
        let key = (node, pin.to_owned());
        if self
            .runtime
            .output_seq
            .get(&key)
            .is_some_and(|seen| *seen >= seq)
        {
            return false;
        }
        self.runtime.output_seq.insert(key, seq);
        true
    }

    /// The same guard for the error topic, keyed by node: an error report is
    /// about a whole run, not about one pin.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn accept_error_seq(&mut self, node: NodeId, seq: u64) -> bool {
        if self
            .runtime
            .error_seq
            .get(&node)
            .is_some_and(|seen| *seen >= seq)
        {
            return false;
        }
        self.runtime.error_seq.insert(node, seq);
        true
    }

    /// The same guard for a refused setting, keyed by node and setting: a
    /// refusal is about one setting, and two settings refused in one pass are
    /// two independent reports.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn accept_rejection_seq(&mut self, node: NodeId, key: &str, seq: u64) -> bool {
        let key = (node, key.to_owned());
        if self
            .runtime
            .rejection_seq
            .get(&key)
            .is_some_and(|seen| *seen >= seq)
        {
            return false;
        }
        self.runtime.rejection_seq.insert(key, seq);
        true
    }

    /// Why a node is failing, as the runtime reported it.
    ///
    /// The process that RAN the node is the only one that can know why it
    /// failed, and it is never this one: an editor that hears no runtime shows
    /// no failures.
    pub(super) fn node_error(&self, node: NodeId) -> Option<&str> {
        self.runtime.remote_errors.get(&node).map(String::as_str)
    }

    /// Every failing node with its message, in node order so the status bar
    /// does not name a different one on every redraw.
    pub(super) fn failing_nodes(&self) -> Vec<(NodeId, &str)> {
        let mut failing: Vec<(NodeId, &str)> = self
            .node_order
            .iter()
            .filter_map(|id| self.node_error(*id).map(|message| (*id, message)))
            .collect();
        failing.sort_by_key(|(id, _)| *id);
        failing
    }

    /// Redraws what one node's newly reported values change.
    ///
    /// A node whose row has not arrived yet is remembered anyway: the value is
    /// in the map, and it is drawn as soon as `apply_node_upsert` creates the
    /// node.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn reapply_remote_outputs(&mut self, node: NodeId) {
        if !self.nodes.contains_key(&node) {
            return;
        }
        self.update_displays_from(node);
    }

    /// Brings the runner links, their event subscriptions and muxes, and the
    /// live feeds in line with what the graph and the store now say.
    ///
    /// The one place anything is dialled, called after everything that can
    /// change the answer: a Display node's wire, its size, its existence, a
    /// graph reload, or which runners serve where. A feed that is still
    /// wanted with the same request is left running -- restarting a video
    /// stream because the editor redrew would be a stutter the user can see.
    ///
    /// Stopping is by removal: every task handle aborts on drop.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn reconcile_runtime(&mut self) -> Task<Message> {
        let announced = announced_runners(
            &self
                .store_conn()
                .map(zeughaus_sync::runtimes)
                .unwrap_or_default(),
        );
        let mut tasks = Vec::new();
        // A runner that left takes its values, its terminals and its section
        // with it; its graphs show as not running.
        let gone: Vec<RunnerKey> = self
            .runtime
            .links
            .keys()
            .filter(|key| !announced.iter().any(|a| &a.key == *key))
            .cloned()
            .collect();
        for key in &gone {
            self.runtime.links.remove(key);
            self.forget_runner_values(key, false);
            self.mux.remove(key);
            self.workspace.remove_section(key);
        }
        // Runners that moved, restarted or regenerated their identity: every
        // address already dialled there is stale.
        let mut moved: Vec<RunnerKey> = Vec::new();
        for runner in announced {
            self.workspace
                .ensure_runner(runner.key.clone(), runner.label.clone());
            let (restart_traffic, endpoint_changed) = match self.runtime.links.get_mut(&runner.key)
            {
                Some(link) => {
                    link.label = runner.label;
                    let changed = link.endpoint != runner.endpoint;
                    if changed {
                        link.endpoint = runner.endpoint;
                    }
                    // Also restarted when the endpoint is unchanged but no
                    // task is running: the subscription is what carries every
                    // value, so a missing one is not something to wait out.
                    (changed || link.traffic.is_none(), changed)
                }
                None => {
                    self.runtime.links.insert(
                        runner.key.clone(),
                        RunnerLink {
                            endpoint: runner.endpoint,
                            label: runner.label,
                            traffic: None,
                            traffic_live: false,
                            traffic_epoch: 0,
                            machine: None,
                            machine_seq: 0,
                        },
                    );
                    (true, true)
                }
            };
            if endpoint_changed {
                moved.push(runner.key.clone());
            }
            if restart_traffic {
                tasks.push(self.restart_traffic(&runner.key));
            }
            // The mux hangs off the same address and is replaced by the same
            // rule: a runner that moved owns different terminals, and one
            // that has no control task is not attached to anything.
            if endpoint_changed || !self.mux.contains_key(&runner.key) {
                tasks.push(self.restart_mux(&runner.key));
            }
        }
        if !gone.is_empty() || !moved.is_empty() {
            self.update_display_values();
            self.rebuild_palette();
        }
        let wanted = self.wanted_feeds();
        // A feed's request is fixed for its lifetime, so a new tier is a new
        // feed rather than a renegotiation; and a feed from a runner that
        // moved reads a dead stream.
        let lost = self.retain_feeds(|key, live| {
            !moved.contains(&live.runner)
                && wanted
                    .get(key)
                    .is_some_and(|(tier, runner)| *tier == live.tier && *runner == live.runner)
        });
        if lost {
            self.update_display_values();
        }
        for (key, (tier, runner)) in wanted {
            if self.runtime.feeds.contains_key(&key) {
                continue;
            }
            let Some(endpoint) = self.runtime.links.get(&runner).map(|l| l.endpoint.clone()) else {
                continue;
            };
            self.runtime.feed_epoch += 1;
            let epoch = self.runtime.feed_epoch;
            let spec = FeedSpec {
                endpoint,
                key: key.clone(),
                epoch,
                tier,
            };
            let (task, handle) = Task::run(feed::frames(spec), Message::FeedFrame).abortable();
            self.runtime.feeds.insert(
                key,
                LiveFeed {
                    tier,
                    runner,
                    epoch,
                    latest: None,
                    _task: handle.abort_on_drop(),
                },
            );
            tasks.push(task);
        }
        Task::batch(tasks)
    }

    /// Replaces one runner's event subscription. Whatever it reported is
    /// forgotten: the next snapshot is what replaces it.
    #[cfg(not(target_arch = "wasm32"))]
    fn restart_traffic(&mut self, key: &RunnerKey) -> Task<Message> {
        self.runtime.next_epoch += 1;
        let epoch = self.runtime.next_epoch;
        let Some(link) = self.runtime.links.get_mut(key) else {
            return Task::none();
        };
        // Anything the aborted task still has queued belongs to the
        // subscription that is being replaced.
        link.traffic = None;
        link.traffic_live = false;
        link.traffic_epoch = epoch;
        let endpoint = link.endpoint.clone();
        let (task, handle) = Task::run(feed::events(endpoint), move |traffic| {
            Message::Traffic(epoch, traffic)
        })
        .abortable();
        if let Some(link) = self.runtime.links.get_mut(key) {
            link.traffic = Some(handle.abort_on_drop());
        }
        self.forget_runner_values(key, false);
        self.update_display_values();
        task
    }

    /// The link a node's values, presses and feeds go through: its graph's
    /// runner, while that runner is connected.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn link_of(&self, node: NodeId) -> Option<(RunnerKey, &RunnerLink)> {
        let key = RunnerKey::new(self.runner_of(node)?);
        let link = self.runtime.links.get(&key)?;
        Some((key, link))
    }

    /// Which feeds the graph asks for, and at what ladder tier.
    ///
    /// Only image-typed source pins: a scalar already arrives as a runtime
    /// event, and streaming it as frames as well would be a second, slower
    /// path to the same number.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn wanted_feeds(&self) -> HashMap<FeedKey, (u32, RunnerKey)> {
        let frame_ty = Ty::of::<Image>();
        let mut wanted: HashMap<FeedKey, (u32, RunnerKey)> = HashMap::new();
        for (id, node) in &self.nodes {
            if !is_display(&node.type_id) {
                continue;
            }
            // A Display node has exactly one input, so its single incoming edge
            // names the pin it draws.
            let Some(edge) = self.edges.iter().find(|e| e.to_node == *id) else {
                continue;
            };
            let carries_frame = self
                .nodes
                .get(&edge.from_node)
                .and_then(|src| src.pin_defs.iter().find(|p| p.name == edge.from_pin.0))
                .is_some_and(|p| p.ty == frame_ty);
            if !carries_frame {
                continue;
            }
            // Served by the runner of the source's graph, and by nobody while
            // that runner is not connected.
            let Some((runner, _)) = self.link_of(edge.from_node) else {
                continue;
            };
            // Sized by what the node body draws, not by the frame's native size:
            // the runtime holds the frame, so it is the side that can cheaply
            // produce the half megabyte a preview needs instead of shipping
            // 33 MB for it.
            let tier =
                feed::requested_tier(self.node_sizes.get(id).copied().unwrap_or(DISPLAY_SIZE));
            let key = FeedKey {
                node_id: edge.from_node.0,
                pin: Arc::clone(&edge.from_pin.0),
            };
            wanted
                .entry(key)
                .and_modify(|(shared, _)| *shared = feed::widen(*shared, tier))
                .or_insert((tier, runner));
        }
        wanted
    }

    /// Drops every feed `keep` rejects, returning whether any of them was
    /// showing a frame -- that frame just left the screen, so the caller has to
    /// rebuild the display values.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn retain_feeds(
        &mut self,
        mut keep: impl FnMut(&FeedKey, &LiveFeed) -> bool,
    ) -> bool {
        let mut lost = false;
        self.runtime.feeds.retain(|key, live| {
            let stay = keep(key, live);
            lost |= !stay && live.latest.is_some();
            stay
        });
        lost
    }

    /// The frame a Display node currently shows, if a feed is delivering one.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn feed_frame(&self, node_id: NodeId) -> Option<&Image> {
        if self.runtime.feeds.is_empty() {
            return None;
        }
        if !self
            .nodes
            .get(&node_id)
            .is_some_and(|n| is_display(&n.type_id))
        {
            return None;
        }
        let edge = self.edges.iter().find(|e| e.to_node == node_id)?;
        let key = FeedKey {
            node_id: edge.from_node.0,
            pin: Arc::clone(&edge.from_pin.0),
        };
        self.runtime
            .feeds
            .get(&key)?
            .latest
            .as_ref()
            .map(|(_, image)| image)
    }

    /// Adopts a frame from a feed, if it is still the frame to show.
    ///
    /// Two are refused: one from a superseded generation (a resize or a
    /// reconnect opened a new feed, whose numbering restarts, so the epoch is
    /// what separates them), and one whose sequence is not newer than what is
    /// already on screen.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn apply_feed_frame(&mut self, frame: crate::feed::Frame) {
        let Some(live) = self.runtime.feeds.get_mut(&frame.key) else {
            return;
        };
        if live.epoch != frame.epoch {
            return;
        }
        let image = frame.image.clone();
        live.latest = Some((frame.order, frame.image));
        self.runtime.frames_received += 1;

        // Only the nodes drawing this feed change, so the whole display map is
        // not rebuilt: at 30 frames a second that would re-render every other
        // node's value text 30 times a second for nothing.
        let drawing: Vec<NodeId> = self
            .edges
            .iter()
            .filter(|e| e.from_node.0 == frame.key.node_id && e.from_pin.0 == frame.key.pin)
            .map(|e| e.to_node)
            .filter(|id| self.nodes.get(id).is_some_and(|n| is_display(&n.type_id)))
            .collect();
        for id in drawing {
            let display = self.frame_display(id, &image);
            self.display_values.insert(id, display);
        }
    }

    /// The status bar's error text: one failing node's message, plus a count
    /// when several failed. Empty while every node is fine.
    pub(super) fn node_error_summary(&self) -> String {
        let failing = self.failing_nodes();
        let Some((id, message)) = failing.first() else {
            return String::new();
        };
        let name = self
            .nodes
            .get(id)
            .map_or("node", |n| n.display_name.as_str());
        match failing.len() - 1 {
            0 => format!("{name}: {message}"),
            more => format!("{name}: {message} (+{more} more)"),
        }
    }
}

/// One runner as the store announces it.
#[cfg(not(target_arch = "wasm32"))]
struct Announced {
    key: RunnerKey,
    endpoint: Endpoint,
    label: String,
}

/// The runners the store's runtime rows announce, oldest first, keyed by the
/// fingerprint their URL pins. A row without a pinned fingerprint cannot be
/// told apart from another runner and is skipped; two rows with one
/// fingerprint (one `runner.pem`, two processes) are one runner, the older.
#[cfg(not(target_arch = "wasm32"))]
fn announced_runners(rows: &[zeughaus_sync::RuntimeRow]) -> Vec<Announced> {
    let mut out: Vec<Announced> = Vec::new();
    for row in rows {
        let Ok(addr) = weida::EndpointAddr::parse(&row.addr) else {
            continue;
        };
        let Some(fingerprint) = addr.peer.map(|fp| fp.to_string()) else {
            continue;
        };
        if out.iter().any(|a| a.key.as_str() == fingerprint) {
            continue;
        }
        // The host and the first eight hex digits after `sha256:`: enough to
        // tell two runners on one host apart at a glance.
        let short = fingerprint.get(7..15).unwrap_or(&fingerprint);
        out.push(Announced {
            key: RunnerKey::new(&fingerprint),
            endpoint: Endpoint(row.addr.clone()),
            label: format!("{} {short}", addr.host),
        });
    }
    out
}
