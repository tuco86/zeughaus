//! Back and Forward over the places the window showed, kept as an undo tree:
//! a new place after a Back branches off instead of dropping the way
//! forward, and every branch stays reachable from the tree view.
//!
//! A place is whatever the caller says it is (`P`); the tree only compares
//! places. Moves the user did not make by Back, Forward or a jump are
//! *visits*: a visit back to the parent is a Back step, one to a child a
//! Forward step, anything else a new child of the current place. A place
//! left within [`SETTLE`] of a visit to it was passed through on the way
//! somewhere else and is taken out again.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

/// The most places the tree keeps; the oldest branch goes first.
pub const MAX_PLACES: usize = 100;

/// A place left sooner than this after an automatic move to it was passed
/// through, not visited.
pub const SETTLE: Duration = Duration::from_millis(750);

/// The places, as a tree whose root is the oldest kept.
pub struct History<P> {
    /// Ascending by id; a parent's id is always smaller than its child's.
    nodes: Vec<Node<P>>,
    next_id: u64,
    current: Option<u64>,
    /// The last visit, while it can still turn out to be a pass-through.
    transient: Option<Transient>,
}

impl<P> Default for History<P> {
    fn default() -> Self {
        History {
            nodes: Vec::new(),
            next_id: 0,
            current: None,
            transient: None,
        }
    }
}

struct Node<P> {
    id: u64,
    parent: Option<u64>,
    /// The child Forward takes.
    forward: Option<u64>,
    place: P,
    label: String,
    visited: Instant,
}

/// What the last visit changed, so a pass-through can be undone.
struct Transient {
    from: u64,
    at: Instant,
    /// The node the visit created.
    created: Option<u64>,
    /// The node whose `forward` changed, and its old value.
    pointer: Option<(u64, Option<u64>)>,
}

/// How a row of the tree relates to the current place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mark {
    Current,
    /// An ancestor of the current place: what Back walks.
    Behind,
    /// On the way Forward takes from the current place.
    Ahead,
    Aside,
}

/// One node of the tree as a git-style graph draws it, newest on top.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Row {
    pub id: u64,
    /// The column the node's dot sits in.
    pub lane: usize,
    /// A line from the dot up to the row above.
    pub up: bool,
    /// A line from the dot down to the row below.
    pub down: bool,
    /// Lanes that pass this row without a node in it.
    pub through: Vec<usize>,
    /// Lanes of branches that end here, at their parent.
    pub joins: Vec<usize>,
    pub mark: Mark,
}

/// The tree as a restart hands it on.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Saved<P> {
    pub nodes: Vec<SavedNode<P>>,
    pub current: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SavedNode<P> {
    pub id: u64,
    pub parent: Option<u64>,
    pub forward: Option<u64>,
    pub place: P,
    pub label: String,
    /// Seconds since the last visit, when saved.
    pub age_secs: u64,
}

impl<P: Clone + PartialEq> History<P> {
    /// The place the window shows, as far as the tree knows.
    pub fn current(&self) -> Option<&P> {
        Some(&self.node(self.current?)?.place)
    }

    /// Node `id`'s place, its stored label and its last visit.
    pub fn place(&self, id: u64) -> Option<(&P, &str, Instant)> {
        let node = self.node(id)?;
        Some((&node.place, node.label.as_str(), node.visited))
    }

    pub fn len(&self) -> usize {
        self.nodes.len()
    }

    /// The window moved to `place` by itself or by a link: a step back or
    /// forward when it is the parent or a child of the current place, else
    /// a new child of it.
    pub fn visit(&mut self, place: P, label: String, now: Instant) {
        let Some(mut current) = self.current else {
            self.push(None, place, label, now);
            self.transient = None;
            return;
        };
        if let Some(node) = self.node_mut(current)
            && node.place == place
        {
            node.label = label;
            return;
        }
        if let Some(t) = self.transient.take()
            && now.saturating_duration_since(t.at) < SETTLE
        {
            if let Some(created) = t.created {
                self.remove(created);
            }
            if let Some((id, old)) = t.pointer
                && let Some(node) = self.node_mut(id)
            {
                node.forward = old;
            }
            self.current = Some(t.from);
            current = t.from;
            if let Some(node) = self.node_mut(current)
                && node.place == place
            {
                node.label = label;
                return;
            }
        }
        let parent = self.node(current).and_then(|node| node.parent);
        let target = parent
            .filter(|&parent| self.node(parent).is_some_and(|node| node.place == place))
            .map(|parent| (parent, parent, current))
            .or_else(|| {
                self.nodes
                    .iter()
                    .rev()
                    .find(|node| node.parent == Some(current) && node.place == place)
                    .map(|child| (child.id, current, child.id))
            });
        if let Some((to, owner, forward)) = target {
            // A step back sets the parent's way forward to where it came
            // from; a step forward sets the current one's to the child.
            let pointer = self.set_forward(owner, Some(forward));
            if let Some(node) = self.node_mut(to) {
                node.visited = now;
                node.label = label;
            }
            self.current = Some(to);
            self.transient = Some(Transient {
                from: current,
                at: now,
                created: None,
                pointer,
            });
            return;
        }
        let id = self.push(Some(current), place, label, now);
        let pointer = self.set_forward(current, Some(id));
        self.transient = Some(Transient {
            from: current,
            at: now,
            created: Some(id),
            pointer,
        });
        self.prune();
    }

    /// Steps back to the nearest ancestor that is `available`, remembering
    /// the way forward on every node passed. `None` changes nothing.
    pub fn back(&mut self, available: impl Fn(&P) -> bool, now: Instant) -> Option<P> {
        let target = self.back_target(&available)?;
        let mut child = self.current?;
        while let Some(parent) = self.node(child).and_then(|node| node.parent) {
            self.set_forward(parent, Some(child));
            if parent == target {
                break;
            }
            child = parent;
        }
        self.arrive(target, now)
    }

    /// Steps forward along the remembered way (else the newest child) to
    /// the first node that is `available`. `None` changes nothing.
    pub fn forward(&mut self, available: impl Fn(&P) -> bool, now: Instant) -> Option<P> {
        let target = self.forward_target(&available)?;
        let mut at = self.current?;
        while at != target {
            let Some(next) = self.next_forward(at) else {
                break;
            };
            self.set_forward(at, Some(next));
            at = next;
        }
        self.arrive(target, now)
    }

    pub fn can_back(&self, available: impl Fn(&P) -> bool) -> bool {
        self.back_target(&available).is_some()
    }

    pub fn can_forward(&self, available: impl Fn(&P) -> bool) -> bool {
        self.forward_target(&available).is_some()
    }

    /// Makes node `id` current, the way to it the way forward of each of
    /// its ancestors. `None` for an unknown id.
    pub fn jump(&mut self, id: u64, now: Instant) -> Option<P> {
        self.node(id)?;
        let mut child = id;
        while let Some(parent) = self.node(child).and_then(|node| node.parent) {
            self.set_forward(parent, Some(child));
            child = parent;
        }
        self.arrive(id, now)
    }

    /// One row per node, newest first, with the lanes of a git-style graph:
    /// a chain of first children shares its head's lane, every other child
    /// starts a lane of its own that ends at its parent's row.
    pub fn rows(&self) -> Vec<Row> {
        let count = self.nodes.len();
        let row_of = |id: u64| -> usize { count - 1 - self.index(id).expect("a node of the tree") };
        // The spans, as (lane, top, bottom, branch), one per chain.
        let mut spans: Vec<(usize, usize, usize, bool)> = Vec::new();
        let mut lane_of = vec![0usize; count];
        let mut top_of = vec![0usize; count];
        let mut bottom_of = vec![0usize; count];
        for head in &self.nodes {
            let parent = head.parent;
            let head_of_chain =
                parent.is_none_or(|parent| self.first_child(parent) != Some(head.id));
            if head_of_chain {
                let mut end = head.id;
                let mut members = vec![head.id];
                while let Some(child) = self.first_child(end) {
                    members.push(child);
                    end = child;
                }
                let top = row_of(end);
                let bottom = row_of(parent.unwrap_or(head.id));
                let lane = (0..)
                    .find(|&lane| {
                        !spans
                            .iter()
                            .any(|&(l, t, b, _)| l == lane && t <= bottom && top <= b)
                    })
                    .expect("a free lane");
                spans.push((lane, top, bottom, parent.is_some()));
                for member in members {
                    let at = self.index(member).expect("a node of the tree");
                    lane_of[at] = lane;
                    top_of[at] = top;
                    bottom_of[at] = bottom;
                }
            }
        }
        let behind = self.behind();
        let ahead = self.ahead();
        self.nodes
            .iter()
            .enumerate()
            .rev()
            .map(|(at, node)| {
                let r = count - 1 - at;
                let lane = lane_of[at];
                let mut through: Vec<usize> = spans
                    .iter()
                    .filter(|&&(l, t, b, _)| l != lane && t < r && r < b)
                    .map(|&(l, ..)| l)
                    .collect();
                through.sort_unstable();
                let mut joins: Vec<usize> = spans
                    .iter()
                    .filter(|&&(_, _, b, branch)| branch && b == r)
                    .map(|&(l, ..)| l)
                    .collect();
                joins.sort_unstable();
                let mark = if Some(node.id) == self.current {
                    Mark::Current
                } else if behind.contains(&node.id) {
                    Mark::Behind
                } else if ahead.contains(&node.id) {
                    Mark::Ahead
                } else {
                    Mark::Aside
                };
                Row {
                    id: node.id,
                    lane,
                    up: r > top_of[at],
                    down: r < bottom_of[at],
                    through,
                    joins,
                    mark,
                }
            })
            .collect()
    }

    /// The width of the graph `rows` draw, in lanes.
    pub fn lanes(rows: &[Row]) -> usize {
        rows.iter()
            .flat_map(|row| {
                std::iter::once(row.lane)
                    .chain(row.through.iter().copied())
                    .chain(row.joins.iter().copied())
            })
            .max()
            .map_or(0, |lane| lane + 1)
    }

    pub fn save(&self, now: Instant) -> Saved<P> {
        Saved {
            nodes: self
                .nodes
                .iter()
                .map(|node| SavedNode {
                    id: node.id,
                    parent: node.parent,
                    forward: node.forward,
                    place: node.place.clone(),
                    label: node.label.clone(),
                    age_secs: now.saturating_duration_since(node.visited).as_secs(),
                })
                .collect(),
            current: self.current,
        }
    }

    /// The tree `saved` describes, or an empty one when it is not a tree:
    /// a parent missing or younger than its child, a way forward that is no
    /// child, more than one root or a current place that is not there.
    pub fn load(saved: Saved<P>, now: Instant) -> History<P> {
        let mut nodes = saved.nodes;
        nodes.sort_by_key(|node| node.id);
        let ids_unique = nodes.windows(2).all(|pair| pair[0].id < pair[1].id);
        let known = |id: u64| nodes.binary_search_by_key(&id, |node| node.id).is_ok();
        let roots = nodes.iter().filter(|node| node.parent.is_none()).count();
        let valid = ids_unique
            && (nodes.is_empty() || roots == 1)
            && nodes.iter().all(|node| {
                node.parent
                    .is_none_or(|parent| parent < node.id && known(parent))
                    && node.forward.is_none_or(|forward| {
                        nodes
                            .binary_search_by_key(&forward, |n| n.id)
                            .is_ok_and(|at| nodes[at].parent == Some(node.id))
                    })
            })
            && (nodes.is_empty() || saved.current.is_some_and(known));
        if !valid || nodes.is_empty() {
            return History::default();
        }
        let next_id = nodes.last().map_or(0, |node| node.id + 1);
        let mut history = History {
            nodes: nodes
                .into_iter()
                .map(|node| Node {
                    id: node.id,
                    parent: node.parent,
                    forward: node.forward,
                    place: node.place,
                    label: node.label,
                    visited: now
                        .checked_sub(Duration::from_secs(node.age_secs))
                        .unwrap_or(now),
                })
                .collect(),
            next_id,
            current: saved.current,
            transient: None,
        };
        history.prune();
        history
    }

    fn index(&self, id: u64) -> Option<usize> {
        self.nodes.binary_search_by_key(&id, |node| node.id).ok()
    }

    fn node(&self, id: u64) -> Option<&Node<P>> {
        self.index(id).map(|at| &self.nodes[at])
    }

    fn node_mut(&mut self, id: u64) -> Option<&mut Node<P>> {
        self.index(id).map(|at| &mut self.nodes[at])
    }

    fn push(&mut self, parent: Option<u64>, place: P, label: String, now: Instant) -> u64 {
        let id = self.next_id;
        self.next_id += 1;
        self.nodes.push(Node {
            id,
            parent,
            forward: None,
            place,
            label,
            visited: now,
        });
        self.current = Some(id);
        id
    }

    fn remove(&mut self, id: u64) {
        if let Some(at) = self.index(id) {
            self.nodes.remove(at);
        }
    }

    /// Sets node `id`'s way forward and answers what to restore.
    fn set_forward(&mut self, id: u64, forward: Option<u64>) -> Option<(u64, Option<u64>)> {
        let node = self.node_mut(id)?;
        let old = node.forward;
        node.forward = forward;
        Some((id, old))
    }

    fn arrive(&mut self, id: u64, now: Instant) -> Option<P> {
        let node = self.node_mut(id)?;
        node.visited = now;
        let place = node.place.clone();
        self.current = Some(id);
        self.transient = None;
        Some(place)
    }

    fn first_child(&self, id: u64) -> Option<u64> {
        self.nodes
            .iter()
            .find(|node| node.parent == Some(id))
            .map(|node| node.id)
    }

    fn newest_child(&self, id: u64) -> Option<u64> {
        self.nodes
            .iter()
            .rev()
            .find(|node| node.parent == Some(id))
            .map(|node| node.id)
    }

    /// Where Forward goes from node `id`.
    fn next_forward(&self, id: u64) -> Option<u64> {
        self.node(id)?.forward.or_else(|| self.newest_child(id))
    }

    fn back_target(&self, available: &impl Fn(&P) -> bool) -> Option<u64> {
        let mut at = self.node(self.current?)?.parent;
        while let Some(id) = at {
            let node = self.node(id)?;
            if available(&node.place) {
                return Some(id);
            }
            at = node.parent;
        }
        None
    }

    fn forward_target(&self, available: &impl Fn(&P) -> bool) -> Option<u64> {
        let mut at = self.next_forward(self.current?);
        while let Some(id) = at {
            if available(&self.node(id)?.place) {
                return Some(id);
            }
            at = self.next_forward(id);
        }
        None
    }

    /// The ancestors of the current place.
    fn behind(&self) -> Vec<u64> {
        let mut ids = Vec::new();
        let mut at = self.current.and_then(|id| self.node(id)?.parent);
        while let Some(id) = at {
            ids.push(id);
            at = self.node(id).and_then(|node| node.parent);
        }
        ids
    }

    /// The way Forward takes from the current place.
    fn ahead(&self) -> Vec<u64> {
        let mut ids = Vec::new();
        let mut at = self.current.and_then(|id| self.next_forward(id));
        while let Some(id) = at {
            ids.push(id);
            at = self.next_forward(id);
        }
        ids
    }

    /// Drops the oldest branch's leaf while there are too many places;
    /// a tree that is one path loses its root instead.
    fn prune(&mut self) {
        while self.nodes.len() > MAX_PLACES {
            let leaf = self
                .nodes
                .iter()
                .map(|node| node.id)
                .find(|&id| Some(id) != self.current && self.first_child(id).is_none());
            let removed = match leaf {
                Some(id) => {
                    let parent = self.node(id).and_then(|node| node.parent);
                    self.remove(id);
                    if let Some(parent) = parent
                        && self
                            .node(parent)
                            .is_some_and(|node| node.forward == Some(id))
                    {
                        let newest = self.newest_child(parent);
                        self.set_forward(parent, newest);
                    }
                    id
                }
                None => {
                    let root = self.nodes.remove(0).id;
                    for node in &mut self.nodes {
                        if node.parent == Some(root) {
                            node.parent = None;
                        }
                    }
                    root
                }
            };
            if self.transient.as_ref().is_some_and(|t| {
                t.from == removed
                    || t.created == Some(removed)
                    || t.pointer.is_some_and(|(id, _)| id == removed)
            }) {
                self.transient = None;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(t0: Instant, millis: u64) -> Instant {
        t0 + Duration::from_millis(millis)
    }

    /// Visits `places` one second apart, starting at `t0`.
    fn visited(places: &[u32], t0: Instant) -> History<u32> {
        let mut history = History::default();
        for (n, &place) in places.iter().enumerate() {
            history.visit(place, String::new(), at(t0, n as u64 * 1000));
        }
        history
    }

    fn id_of(history: &History<u32>, place: u32) -> u64 {
        history
            .rows()
            .into_iter()
            .map(|row| row.id)
            .find(|&id| history.place(id).is_some_and(|(p, ..)| *p == place))
            .expect("the place is in the tree")
    }

    fn parent_of(history: &History<u32>, place: u32) -> Option<u32> {
        let parent = history.node(id_of(history, place))?.parent?;
        history.place(parent).map(|(p, ..)| *p)
    }

    #[test]
    fn back_then_a_new_place_branches_and_keeps_the_old_way_forward() {
        let t0 = Instant::now();
        let mut history = visited(&[1, 2, 3], t0);
        assert_eq!(history.back(|_| true, at(t0, 3000)), Some(2));
        history.visit(4, String::new(), at(t0, 4000));
        assert_eq!(parent_of(&history, 3), Some(2));
        assert_eq!(parent_of(&history, 4), Some(2));

        assert_eq!(history.back(|_| true, at(t0, 5000)), Some(2));
        assert_eq!(history.forward(|_| true, at(t0, 6000)), Some(4));

        let three = id_of(&history, 3);
        assert_eq!(history.jump(three, at(t0, 7000)), Some(3));
        assert_eq!(history.back(|_| true, at(t0, 8000)), Some(2));
        assert_eq!(history.forward(|_| true, at(t0, 9000)), Some(3));
    }

    #[test]
    fn returning_to_the_previous_place_is_a_back_step() {
        let t0 = Instant::now();
        let mut history = visited(&[1, 2, 1], t0);
        assert_eq!(history.len(), 2);
        assert_eq!(history.current(), Some(&1));
        assert_eq!(history.forward(|_| true, at(t0, 5000)), Some(2));
    }

    #[test]
    fn a_place_passed_within_the_settle_time_is_not_kept() {
        let t0 = Instant::now();
        let mut history = History::default();
        history.visit(1, String::new(), t0);
        history.visit(2, String::new(), at(t0, 1000));
        history.visit(3, String::new(), at(t0, 1200));
        assert_eq!(history.len(), 2);
        assert_eq!(parent_of(&history, 3), Some(1));

        let mut history = History::default();
        history.visit(1, String::new(), t0);
        history.visit(2, String::new(), at(t0, 1000));
        history.visit(1, String::new(), at(t0, 3000));
        history.visit(4, String::new(), at(t0, 3300));
        assert_eq!(parent_of(&history, 4), Some(2));
        assert_eq!(history.back(|_| true, at(t0, 5000)), Some(2));
        assert_eq!(history.back(|_| true, at(t0, 6000)), Some(1));
    }

    #[test]
    fn back_skips_places_that_are_gone() {
        let t0 = Instant::now();
        let mut history = visited(&[1, 2, 3], t0);
        assert!(!history.can_back(|p| *p == 3));
        assert_eq!(history.back(|p| *p != 2, at(t0, 5000)), Some(1));
        assert_eq!(history.forward(|_| true, at(t0, 6000)), Some(2));
    }

    #[test]
    fn the_oldest_branch_goes_first_beyond_the_limit() {
        let t0 = Instant::now();
        let mut history = visited(&[1, 1000], t0);
        assert_eq!(history.back(|_| true, at(t0, 2000)), Some(1));
        let last = MAX_PLACES as u32;
        for (n, place) in (2..=last).enumerate() {
            history.visit(place, String::new(), at(t0, 3000 + n as u64 * 1000));
        }
        assert_eq!(history.len(), MAX_PLACES);
        let rows = history.rows();
        assert!(
            rows.iter()
                .all(|row| history.place(row.id).is_some_and(|(p, ..)| *p != 1000))
        );
        assert_eq!(parent_of(&history, 1), None);
        assert_eq!(history.current(), Some(&last));
    }

    #[test]
    fn a_branch_takes_its_own_lane_and_joins_its_parent() {
        let t0 = Instant::now();
        let mut history = visited(&[1, 2], t0);
        assert_eq!(history.back(|_| true, at(t0, 2000)), Some(1));
        history.visit(3, String::new(), at(t0, 3000));
        let (one, two, three) = (id_of(&history, 1), id_of(&history, 2), id_of(&history, 3));
        let rows = history.rows();
        assert_eq!(
            rows,
            vec![
                Row {
                    id: three,
                    lane: 1,
                    up: false,
                    down: true,
                    through: vec![],
                    joins: vec![],
                    mark: Mark::Current,
                },
                Row {
                    id: two,
                    lane: 0,
                    up: false,
                    down: true,
                    through: vec![1],
                    joins: vec![],
                    mark: Mark::Aside,
                },
                Row {
                    id: one,
                    lane: 0,
                    up: true,
                    down: false,
                    through: vec![],
                    joins: vec![1],
                    mark: Mark::Behind,
                },
            ]
        );
        assert_eq!(History::<u32>::lanes(&rows), 2);
    }

    #[test]
    fn a_saved_history_comes_back_as_it_was() {
        let t0 = Instant::now();
        let mut history = visited(&[1, 2, 3], t0);
        history.back(|_| true, at(t0, 3000));
        history.visit(4, String::new(), at(t0, 4000));
        let now = at(t0, 5000);
        let saved = serde_json::to_string(&history.save(now)).unwrap();
        let mut back = History::load(serde_json::from_str(&saved).unwrap(), now);
        assert_eq!(back.current(), Some(&4));
        assert_eq!(back.rows(), history.rows());
        assert_eq!(back.back(|_| true, now), history.back(|_| true, now));
        assert_eq!(back.back(|_| true, now), history.back(|_| true, now));
        assert_eq!(back.forward(|_| true, now), history.forward(|_| true, now));
        assert_eq!(back.forward(|_| true, now), Some(4));

        let mut broken = history.save(now);
        broken.current = Some(99);
        assert_eq!(History::load(broken, now).len(), 0);
    }
}
