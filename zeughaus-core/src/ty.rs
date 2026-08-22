//! Runtime type system: the domain type of a pin or a value, constructible at
//! runtime.
//!
//! Pin types used to be `&'static str` labels compared by string equality. That
//! limited the tool to domains whose types are known when the Rust code is
//! compiled -- a user-designed database schema, a subgraph's exposed pins or an
//! inferred tensor shape had no way to name themselves. `Ty` is a structural
//! description instead, and its composites (`Ty::list`, `Ty::record`) are built
//! at runtime, so a node can derive its pins from data.
//!
//! Plugin-owned Rust types stay nominal via [`Ty::Opaque`]: `KerasModel` and
//! `Conversation` are matched by name, not structure, because their meaning is
//! their Rust implementation, not their field layout.
//!
//! The built-in scalars are a bijection with their Rust representation
//! (`Bool <-> bool`, `Int <-> i64`, `Float <-> f64`, `Str <-> String`). That
//! bijection is what makes [`crate::Value::ty`] trustworthy: the declared type
//! and the boxed Rust type can no longer disagree, which was the failure mode of
//! the old string labels. Narrower Rust numerics (`u8`, `f32`) are deliberately
//! not pin types -- a node converts at the emit site (`v as f64`) instead of
//! relying on a widening converter to paper over the mismatch.

use std::fmt;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

/// The type of a pin or a value.
///
/// Cheap to clone: composites share their payload through `Arc`.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum Ty {
    /// Wildcard. Connects to anything and is never coerced -- the type is
    /// whatever flows through (see the `flow.hold` node).
    Any,
    Bool,
    Int,
    Float,
    Str,
    List(Arc<Ty>),
    Option(Arc<Ty>),
    Record(Arc<Record>),
    /// A plugin-owned Rust type, matched by name.
    Opaque(Arc<str>),
}

/// A named product type with ordered fields. Field order is significant: it is
/// the order a schema, a subgraph or a table declares them in.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Record {
    pub name: Arc<str>,
    pub fields: Vec<Field>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct Field {
    pub name: Arc<str>,
    pub ty: Ty,
}

impl Ty {
    /// The type of a Rust type that declares one.
    pub fn of<T: Typed>() -> Ty {
        T::ty()
    }

    pub fn list(item: Ty) -> Ty {
        Ty::List(Arc::new(item))
    }

    pub fn option(inner: Ty) -> Ty {
        Ty::Option(Arc::new(inner))
    }

    pub fn opaque(name: impl Into<Arc<str>>) -> Ty {
        Ty::Opaque(name.into())
    }

    pub fn record(name: impl Into<Arc<str>>, fields: Vec<Field>) -> Ty {
        Ty::Record(Arc::new(Record {
            name: name.into(),
            fields,
        }))
    }

    /// Whether this is the wildcard. Wildcards bypass both connection checking
    /// and coercion.
    pub fn is_any(&self) -> bool {
        matches!(self, Ty::Any)
    }
}

impl Field {
    pub fn new(name: impl Into<Arc<str>>, ty: Ty) -> Self {
        Self {
            name: name.into(),
            ty,
        }
    }
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Ty::Any => f.write_str("any"),
            Ty::Bool => f.write_str("bool"),
            Ty::Int => f.write_str("int"),
            Ty::Float => f.write_str("float"),
            Ty::Str => f.write_str("str"),
            Ty::List(item) => write!(f, "[{item}]"),
            Ty::Option(inner) => write!(f, "{inner}?"),
            Ty::Record(rec) => f.write_str(&rec.name),
            Ty::Opaque(name) => f.write_str(name),
        }
    }
}

/// A Rust type that can travel through the graph as a [`crate::Value`].
///
/// Implementing this is how a plugin introduces a domain type. The `ty()` return
/// value is the single source of truth for both the pin declaration and the
/// runtime tag on values, so the two cannot drift apart.
pub trait Typed: Clone + Send + Sync + 'static {
    fn ty() -> Ty;

    /// Structural view of a value, for generic display and inspection.
    ///
    /// Scalars and containers report their contents; nominal plugin types keep
    /// their contents private (the default). A type whose `ty()` is a
    /// [`Ty::Record`] should report [`Repr::Record`] with the same field order,
    /// otherwise generic consumers cannot line the two up.
    fn repr(&self) -> Repr<'_> {
        Repr::Opaque
    }
}

/// Borrowed structural view of a value. Produced by [`Typed::repr`] and walked
/// by generic consumers (value display in the editor, and later inspection
/// widgets and capture).
#[derive(Debug, Clone, PartialEq)]
pub enum Repr<'a> {
    Bool(bool),
    Int(i64),
    Float(f64),
    Str(&'a str),
    List(Vec<Repr<'a>>),
    Record(Vec<(&'a str, Repr<'a>)>),
    /// Nothing (an empty `Option`).
    Nothing,
    /// A nominal type that does not expose its structure.
    Opaque,
}

impl Typed for bool {
    fn ty() -> Ty {
        Ty::Bool
    }
    fn repr(&self) -> Repr<'_> {
        Repr::Bool(*self)
    }
}

impl Typed for i64 {
    fn ty() -> Ty {
        Ty::Int
    }
    fn repr(&self) -> Repr<'_> {
        Repr::Int(*self)
    }
}

impl Typed for f64 {
    fn ty() -> Ty {
        Ty::Float
    }
    fn repr(&self) -> Repr<'_> {
        Repr::Float(*self)
    }
}

impl Typed for String {
    fn ty() -> Ty {
        Ty::Str
    }
    fn repr(&self) -> Repr<'_> {
        Repr::Str(self)
    }
}

impl<T: Typed> Typed for Vec<T> {
    fn ty() -> Ty {
        Ty::list(T::ty())
    }
    fn repr(&self) -> Repr<'_> {
        Repr::List(self.iter().map(Typed::repr).collect())
    }
}

impl<T: Typed> Typed for Option<T> {
    fn ty() -> Ty {
        Ty::option(T::ty())
    }
    fn repr(&self) -> Repr<'_> {
        match self {
            Some(v) => v.repr(),
            None => Repr::Nothing,
        }
    }
}

impl fmt::Display for Repr<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Repr::Bool(v) => write!(f, "{v}"),
            Repr::Int(v) => write!(f, "{v}"),
            Repr::Float(v) => write!(f, "{v}"),
            Repr::Str(v) => f.write_str(v),
            Repr::Nothing => f.write_str("-"),
            Repr::Opaque => f.write_str("..."),
            Repr::List(items) => {
                f.write_str("[")?;
                for (i, item) in items.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{item}")?;
                }
                f.write_str("]")
            }
            Repr::Record(fields) => {
                f.write_str("{")?;
                for (i, (name, value)) in fields.iter().enumerate() {
                    if i > 0 {
                        f.write_str(", ")?;
                    }
                    write!(f, "{name}: {value}")?;
                }
                f.write_str("}")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scalars_map_to_their_rust_type() {
        assert_eq!(Ty::of::<f64>(), Ty::Float);
        assert_eq!(Ty::of::<i64>(), Ty::Int);
        assert_eq!(Ty::of::<bool>(), Ty::Bool);
        assert_eq!(Ty::of::<String>(), Ty::Str);
    }

    #[test]
    fn containers_nest() {
        assert_eq!(Ty::of::<Vec<f64>>(), Ty::list(Ty::Float));
        assert_eq!(
            Ty::of::<Option<Vec<String>>>(),
            Ty::option(Ty::list(Ty::Str))
        );
    }

    #[test]
    fn record_built_at_runtime_is_a_first_class_type() {
        // The point of the whole exercise: a type nobody wrote in Rust.
        let customer = Ty::record(
            "Customer",
            vec![
                Field::new("id", Ty::Int),
                Field::new("name", Ty::Str),
                Field::new("balance", Ty::option(Ty::Float)),
            ],
        );
        let same = Ty::record(
            "Customer",
            vec![
                Field::new("id", Ty::Int),
                Field::new("name", Ty::Str),
                Field::new("balance", Ty::option(Ty::Float)),
            ],
        );
        assert_eq!(customer, same);

        // Field order is part of the type.
        let reordered = Ty::record(
            "Customer",
            vec![
                Field::new("name", Ty::Str),
                Field::new("id", Ty::Int),
                Field::new("balance", Ty::option(Ty::Float)),
            ],
        );
        assert_ne!(customer, reordered);
    }

    #[test]
    fn opaque_is_nominal() {
        assert_eq!(Ty::opaque("KerasModel"), Ty::opaque("KerasModel"));
        assert_ne!(Ty::opaque("KerasModel"), Ty::opaque("Conversation"));
    }

    #[test]
    fn display_reads_like_a_signature() {
        assert_eq!(Ty::list(Ty::Float).to_string(), "[float]");
        assert_eq!(Ty::option(Ty::Str).to_string(), "str?");
        assert_eq!(Ty::opaque("KerasModel").to_string(), "KerasModel");
        assert_eq!(
            Ty::record("Point", vec![Field::new("x", Ty::Float)]).to_string(),
            "Point"
        );
    }

    #[test]
    fn types_survive_serialization() {
        // A runtime-built type must round-trip, or a subgraph's derived pins
        // could not be saved.
        let ty = Ty::list(Ty::record(
            "Order",
            vec![
                Field::new("id", Ty::Int),
                Field::new("total", Ty::Float),
                Field::new("note", Ty::option(Ty::Str)),
            ],
        ));
        let json = serde_json::to_string(&ty).unwrap();
        assert_eq!(serde_json::from_str::<Ty>(&json).unwrap(), ty);
    }

    #[test]
    fn repr_walks_containers() {
        assert_eq!(vec![1.5f64, 2.5].repr().to_string(), "[1.5, 2.5]");
        assert_eq!(Some(3i64).repr().to_string(), "3");
        assert_eq!(None::<i64>.repr(), Repr::Nothing);
    }
}
