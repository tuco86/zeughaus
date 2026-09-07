use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::ty::Ty;

/// What an output pin produces. Only `Value` is currently used.
/// `Stream` is reserved for high-frequency data (see DESIGN.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DataMode {
    Stream,
    Value,
}

/// How an input pin consumes data. Not yet enforced by the executor.
/// `Trigger` causes node execution, `Sample` reads passively (see DESIGN.md).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PinKind {
    Trigger,
    Sample,
}

/// Which way a pin connects.
///
/// `Both` is neither: an edge between two `Both` pins is not dataflow at all
/// but a declared relationship between the two nodes -- see
/// [`PinDefinition::field`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PinDefinition {
    pub name: Arc<str>,
    pub direction: PinDirection,
    pub data_mode: DataMode,
    pub pin_kind: PinKind,
    pub ty: Ty,
}

impl PinDefinition {
    /// An input pin. `kind` decides whether arriving data runs the node
    /// (`Trigger`) or is only read when it runs for another reason (`Sample`).
    pub fn input(name: impl Into<Arc<str>>, ty: Ty, kind: PinKind) -> Self {
        Self {
            name: name.into(),
            direction: PinDirection::Input,
            data_mode: DataMode::Value,
            pin_kind: kind,
            ty,
        }
    }

    /// An output pin carrying a single value.
    pub fn output(name: impl Into<Arc<str>>, ty: Ty) -> Self {
        Self {
            name: name.into(),
            direction: PinDirection::Output,
            data_mode: DataMode::Value,
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
            data_mode: DataMode::Value,
            pin_kind: PinKind::Sample,
            ty,
        }
    }

    /// Declares continuous, high-frequency data instead of a single value.
    pub fn streaming(mut self) -> Self {
        self.data_mode = DataMode::Stream;
        self
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
        assert_eq!(input.data_mode, DataMode::Value);
        assert_eq!(input.pin_kind, PinKind::Sample);

        let output = PinDefinition::output("out", Ty::Float);
        assert_eq!(output.direction, PinDirection::Output);
        assert_eq!(output.pin_kind, PinKind::Sample);
        assert_eq!(output.ty, Ty::Float);

        let field = PinDefinition::field("customer_id", Ty::opaque("db.field"));
        assert_eq!(field.direction, PinDirection::Both);
        assert_eq!(field.pin_kind, PinKind::Sample);

        assert_eq!(
            PinDefinition::output("frame", Ty::Any).streaming().data_mode,
            DataMode::Stream
        );
    }

    #[test]
    fn pins_with_runtime_types_survive_serialization() {
        let pin = PinDefinition::output(
            "row",
            Ty::record(
                "Customer",
                vec![crate::ty::Field::new("id", Ty::Int)],
            ),
        );
        let json = serde_json::to_string(&pin).unwrap();
        assert_eq!(serde_json::from_str::<PinDefinition>(&json).unwrap(), pin);
    }
}
