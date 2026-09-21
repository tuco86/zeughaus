//! What the editor knows about the runtime executing its graph: the values and
//! failures it reported, the video feeds it serves, and the particles those
//! deliveries leave on the wires.

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
    /// Which generation of this feed the task belongs to.
    epoch: u64,
    /// Newest frame accepted, with the order it arrived in.
    latest: Option<(FrameOrder, Image)>,
    _task: iced::task::Handle,
}

/// Everything this editor knows about the runtime executing its graph.
///
/// One struct rather than a dozen fields on [`App`], because it is one
/// subject: a runtime that moved invalidates all of it at once. A value, a
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
    /// Where the executing runtime last said it is reachable. Kept so a runner
    /// that restarted on another port, or vanished, is detectable: every live
    /// feed dialled the old address and has to be redialled.
    #[cfg(not(target_arch = "wasm32"))]
    pub endpoint: Option<Endpoint>,
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
    /// The task subscribed to the runtime's events. Aborts on drop, so
    /// replacing it is how the editor stops listening to a runtime that moved.
    #[cfg(not(target_arch = "wasm32"))]
    pub traffic: Option<iced::task::Handle>,
    /// Whether the event subscription is live. Values on screen are last-known
    /// while it is not, which the status bar says rather than leaving the user
    /// to guess.
    #[cfg(not(target_arch = "wasm32"))]
    pub traffic_live: bool,
    /// Which subscription the traffic on screen came from. Bumped whenever the
    /// task is replaced, so a message queued by the old one is recognizable.
    #[cfg(not(target_arch = "wasm32"))]
    pub traffic_epoch: u64,
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
        // The epoch is which subscription asked for them.
        if epoch != self.runtime.traffic_epoch {
            return;
        }
        match traffic {
            Traffic::Snapshot(snapshot) => {
                // The whole set replaces the whole set: a pin the snapshot does
                // not name produced nothing, which is what the editor dims.
                self.runtime.remote_outputs.clear();
                self.runtime.output_seq.clear();
                // The failing nodes replace the failing nodes, for the same
                // reason: a node the snapshot does not name is not failing.
                self.runtime.remote_errors = snapshot
                    .errors
                    .into_iter()
                    .map(|row| (NodeId(row.node_id), row.message))
                    .collect();
                self.runtime.error_seq = self
                    .runtime
                    .remote_errors
                    .keys()
                    .map(|id| (*id, snapshot.seq))
                    .collect();
                // And the refused settings replace the refused settings: what
                // the runtime holds is the whole truth about what it refused.
                self.setting_errors.clear();
                self.runtime.rejection_seq.clear();
                for row in snapshot.rejections {
                    let node = NodeId(row.node_id);
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
                    self.runtime
                        .output_seq
                        .insert((node, row.pin.clone()), snapshot.seq);
                    self.runtime
                        .remote_outputs
                        .entry(node)
                        .or_default()
                        .insert(row.pin, value);
                }
                self.runtime.traffic_live = true;
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
                if let Some(pins) = self.runtime.remote_outputs.get_mut(&node) {
                    pins.remove(&pin);
                }
                self.reapply_remote_outputs(node);
            }
            Traffic::Event(RuntimeEvent::Edge { edge_id, .. }) => {
                let now = iced::time::Instant::now();
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
                    self.runtime.remote_errors.insert(node, message);
                }
            }
            Traffic::Event(RuntimeEvent::NodeErrorCleared { seq, node_id }) => {
                let node = NodeId(node_id);
                if self.accept_error_seq(node, seq) {
                    self.runtime.remote_errors.remove(&node);
                }
            }
            // A refused setting is not a failed run, so it goes where the
            // reason belongs -- under the field -- and raises none of the
            // alarm a failure raises: no red border, no status-bar ERROR.
            Traffic::Event(RuntimeEvent::SettingRejected {
                seq,
                node_id,
                key,
                message,
            }) => {
                let node = NodeId(node_id);
                if self.accept_rejection_seq(node, &key, seq) {
                    self.setting_errors
                        .entry(node)
                        .or_default()
                        .insert(key, message);
                }
            }
            Traffic::Event(RuntimeEvent::SettingAccepted { seq, node_id, key }) => {
                let node = NodeId(node_id);
                if self.accept_rejection_seq(node, &key, seq) {
                    self.record_setting_error(node, &key, None);
                }
            }
            Traffic::Lost => {
                self.runtime.traffic_live = false;
                // Values stay on screen as last-known, because the status bar
                // says so and a number nobody claims is still the last one
                // anybody claimed. An error is not like that: it is a claim
                // about a run, made by a process that is no longer there to
                // make it, and a red border with nothing behind it is worse
                // than none.
                self.runtime.remote_errors.clear();
                self.runtime.error_seq.clear();
                // Same for a refusal: it is a claim about what the runtime
                // holds, and there is no runtime holding it any more.
                self.setting_errors.clear();
                self.runtime.rejection_seq.clear();
            }
        }
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

    /// Brings the event subscription and the live feeds in line with what the
    /// graph and the store now say.
    ///
    /// The one place anything is dialled, called after everything that can
    /// change the answer: a Display node's wire, its size, its existence, a
    /// graph reload, or where the runtime serves. A feed that is still wanted
    /// with the same request is left running -- restarting a video stream
    /// because the editor redrew would be a stutter the user can see.
    ///
    /// Stopping is by removal: every task handle aborts on drop.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn reconcile_runtime(&mut self) -> Task<Message> {
        let announced = self
            .store_conn()
            .and_then(zeughaus_sync::owner_endpoint)
            .map(Endpoint);
        // A runtime that moved, restarted or regenerated its identity
        // invalidates every address already dialled, so nothing survives it.
        let moved = announced != self.runtime.endpoint;
        if moved {
            self.runtime.endpoint = announced;
        }
        let mut tasks = Vec::new();
        // Also restarted when the endpoint is unchanged but no task is running:
        // the subscription is what carries every value, so a missing one is not
        // something to wait out.
        if moved || (self.runtime.traffic.is_none() && self.runtime.endpoint.is_some()) {
            self.runtime.traffic = None;
            self.runtime.traffic_live = false;
            // Anything the aborted task still has queued belongs to the
            // subscription that is being replaced.
            self.runtime.traffic_epoch += 1;
            self.runtime.remote_outputs.clear();
            self.runtime.output_seq.clear();
            // A failure belongs to the runtime that reported it; a different
            // one has not run anything yet.
            self.runtime.remote_errors.clear();
            self.runtime.error_seq.clear();
            // A particle in flight belongs to the runtime that sent it; the new
            // one has not delivered anything yet.
            self.runtime.particles.clear();
            // Whatever is on screen came from a runtime that is not there any
            // more, and the next snapshot is what replaces it.
            self.update_display_values();
            if let Some(endpoint) = self.runtime.endpoint.clone() {
                let epoch = self.runtime.traffic_epoch;
                let (task, handle) = Task::run(feed::events(endpoint), move |traffic| {
                    Message::Traffic(epoch, traffic)
                })
                .abortable();
                self.runtime.traffic = Some(handle.abort_on_drop());
                tasks.push(task);
            }
        }
        // The mux hangs off the same address and is replaced by the same
        // rule: a runner that moved owns different terminals, and one that
        // has no control task is not attached to anything.
        if moved || (self.mux.is_none() && self.runtime.endpoint.is_some()) {
            tasks.push(self.restart_mux());
        }
        // With nothing serving frames, a live feed would be reading a dead
        // stream and the frame it left behind is not what the graph shows.
        let wanted = match self.runtime.endpoint {
            None => HashMap::new(),
            Some(_) => self.wanted_feeds(),
        };
        // A feed's request is fixed for its lifetime, so a new tier is a new
        // feed rather than a renegotiation.
        let lost = self.retain_feeds(|key, live| !moved && wanted.get(key) == Some(&live.tier));
        if lost {
            self.update_display_values();
        }
        let Some(endpoint) = self.runtime.endpoint.clone() else {
            return Task::batch(tasks);
        };

        for (key, tier) in wanted {
            if self.runtime.feeds.contains_key(&key) {
                continue;
            }
            self.runtime.feed_epoch += 1;
            let epoch = self.runtime.feed_epoch;
            let spec = FeedSpec {
                endpoint: endpoint.clone(),
                key: key.clone(),
                epoch,
                tier,
            };
            let (task, handle) = Task::run(feed::frames(spec), Message::FeedFrame).abortable();
            self.runtime.feeds.insert(
                key,
                LiveFeed {
                    tier,
                    epoch,
                    latest: None,
                    _task: handle.abort_on_drop(),
                },
            );
            tasks.push(task);
        }
        Task::batch(tasks)
    }

    /// Which feeds the graph asks for, and at what ladder tier.
    ///
    /// Only image-typed source pins: a scalar already arrives as a runtime
    /// event, and streaming it as frames as well would be a second, slower
    /// path to the same number.
    #[cfg(not(target_arch = "wasm32"))]
    pub(super) fn wanted_feeds(&self) -> HashMap<FeedKey, u32> {
        let frame_ty = Ty::of::<Image>();
        let mut wanted: HashMap<FeedKey, u32> = HashMap::new();
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
                .and_modify(|shared| *shared = feed::widen(*shared, tier))
                .or_insert(tier);
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
