use std::sync::Arc;

use crate::ty::Ty;

/// What an input pin means to the node behind it, and to whoever draws it.
///
/// `Trigger` is an event pin: the node acts on what arrives there, and asks
/// [`InputSet::changed`](crate::InputSet::changed) which of its pins a value
/// was delivered on since it last ran. `Sample` is a state pin, read whenever
/// the node runs for any reason. The editor draws the two apart (square and
/// circle), so a graph shows which wire makes something happen.
///
/// Dirty propagation itself is uniform: every node downstream of a delivery is
/// rerun whatever its pins declare. Acting only on its own event is therefore
/// the node's decision, taken from `changed`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinKind {
    Trigger,
    Sample,
}

/// Which way a pin connects.
///
/// `Both` is neither: an edge between two `Both` pins is not dataflow at all
/// but a declared relationship between the two nodes -- see
/// [`PinDefinition::field`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PinDirection {
    Input,
    Output,
    Both,
}

/// One pin of a node.
///
/// Both the name and the type are owned, because pins are not always written by
/// a Rust author: a variadic node grows them, and a subgraph or a schema derives
/// them from its contents. `Arc` keeps the frequent clones (the editor snapshots
/// pin definitions per node edit) to a refcount bump.
#[derive(Debug, Clone, PartialEq)]
pub struct PinDefinition {
    pub name: Arc<str>,
    pub direction: PinDirection,
    pub pin_kind: PinKind,
    pub ty: Ty,
}

impl PinDefinition {
    /// An input pin. `kind` declares whether arriving data is the event the
    /// node acts on (`Trigger`) or state it reads when it runs (`Sample`).
    pub fn input(name: impl Into<Arc<str>>, ty: Ty, kind: PinKind) -> Self {
        Self {
            name: name.into(),
            direction: PinDirection::Input,
            pin_kind: kind,
            ty,
        }
    }

    /// An output pin carrying a single value.
    pub fn output(name: impl Into<Arc<str>>, ty: Ty) -> Self {
        Self {
            name: name.into(),
            direction: PinDirection::Output,
            pin_kind: PinKind::Sample,
            ty,
        }
    }

    /// A field pin: an endpoint an edge may attach to from either side.
    ///
    /// Neither an input nor an output, because the edge it carries is not a
    /// value in flight but a relationship between the two nodes it joins --
    /// two table fields wired together declare a foreign key. Which end is
    /// which follows from the fields, not from the direction the user dragged.
    ///
    /// The runtime therefore keeps such an edge out of execution entirely (see
    /// `Graph::is_dataflow` in `zeughaus-runtime`), which is what makes two
    /// tables referencing each other a legal graph rather than a cycle.
    pub fn field(name: impl Into<Arc<str>>, ty: Ty) -> Self {
        Self {
            name: name.into(),
            direction: PinDirection::Both,
            pin_kind: PinKind::Sample,
            ty,
        }
    }
}

/// What is currently connected to one of a node's input pins, handed to
/// [`crate::ExecutableNode::sync_pins`] so a node can shape its pins after its
/// connections: the incoming type is what a schema- or subgraph-driven node
/// needs, the name is enough for a variadic one.
#[derive(Debug, Clone, Copy)]
pub struct PinBinding<'a> {
    pub name: &'a str,
    pub ty: &'a Ty,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn constructors_set_the_usual_defaults() {
        let input = PinDefinition::input("model", Ty::opaque("KerasModel"), PinKind::Sample);
        assert_eq!(input.direction, PinDirection::Input);
        assert_eq!(input.pin_kind, PinKind::Sample);

        let output = PinDefinition::output("out", Ty::Float);
        assert_eq!(output.direction, PinDirection::Output);
        assert_eq!(output.pin_kind, PinKind::Sample);
        assert_eq!(output.ty, Ty::Float);

        let field = PinDefinition::field("customer_id", Ty::opaque("db.field"));
        assert_eq!(field.direction, PinDirection::Both);
        assert_eq!(field.pin_kind, PinKind::Sample);
    }
}
