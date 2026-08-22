//! Type converters: the runtime's answer to "connect an `int` output to a
//! `float` input". `Value` is type-erased, so Rust's `Into` cannot be dispatched
//! across an edge at runtime. Instead, plugins register converter functions
//! keyed by [`Ty`], and the executor coerces a value as it crosses an edge whose
//! endpoints declare different types.
//!
//! Connection compatibility is then: same type, either side is [`Ty::Any`], or a
//! converter exists. This keeps the ergonomics of implicit widening without
//! per-edge adapter nodes, while leaving the dependency explicit (the edge is
//! still there, it just carries a converter).
//!
//! Keys are derived from the Rust types in [`TypeConverters::register_typed`]
//! (`A::ty()` / `B::ty()`), so a converter cannot be registered under a type it
//! does not actually convert -- the failure mode of the previous string keys.

use std::collections::HashMap;

use crate::ty::{Ty, Typed};
use crate::value::Value;

/// A converter takes the source value and returns the coerced value, or `None`
/// if the value was not the expected concrete type (a defensive no-op).
type Converter = Box<dyn Fn(&Value) -> Option<Value> + Send + Sync>;

/// Registry of type converters keyed by `(from, to)`. Built once at startup from
/// the plugins and shared (via `Arc`) between the editor's connection validation
/// and the runtime's coercion, so "what may connect" and "what is coerced"
/// cannot disagree.
#[derive(Default)]
pub struct TypeConverters {
    map: HashMap<(Ty, Ty), Converter>,
}

impl TypeConverters {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a raw converter between two types.
    pub fn register<F>(&mut self, from: Ty, to: Ty, f: F)
    where
        F: Fn(&Value) -> Option<Value> + Send + Sync + 'static,
    {
        self.map.insert((from, to), Box::new(f));
    }

    /// Registers a converter from Rust type `A` to `B`, deriving both keys from
    /// the types themselves. Mirrors a `From<A> for B` impl.
    pub fn register_typed<A, B, F>(&mut self, f: F)
    where
        A: Typed,
        B: Typed,
        F: Fn(A) -> B + Send + Sync + 'static,
    {
        self.register(A::ty(), B::ty(), move |v| {
            v.downcast_ref::<A>().map(|a| Value::new(f(a.clone())))
        });
    }

    /// Whether a converter exists for this type pair.
    pub fn can_convert(&self, from: &Ty, to: &Ty) -> bool {
        self.map.contains_key(&(from.clone(), to.clone()))
    }

    /// Coerces `value` from `from` to `to`. Returns `None` if no converter is
    /// registered or the value did not hold the expected concrete type.
    pub fn convert(&self, from: &Ty, to: &Ty, value: &Value) -> Option<Value> {
        self.map
            .get(&(from.clone(), to.clone()))
            .and_then(|f| f(value))
    }

    /// Whether an output of type `from` may connect to an input of type `to`:
    /// identical types, an `any` wildcard on either side, or a registered
    /// converter bridges them.
    pub fn compatible(&self, from: &Ty, to: &Ty) -> bool {
        from == to || from.is_any() || to.is_any() || self.can_convert(from, to)
    }

    /// A registry seeded with the one widening every plugin can rely on. There
    /// is no ladder of narrower numeric types: the built-in scalars map 1:1 onto
    /// Rust types, so a node holding a `u8` converts at the emit site.
    pub fn with_builtins() -> Self {
        let mut c = Self::new();
        c.register_typed::<i64, f64, _>(|x| x as f64);
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_convert_typed() {
        let mut c = TypeConverters::new();
        c.register_typed::<i64, String, _>(|x| x.to_string());
        let out = c.convert(&Ty::Int, &Ty::Str, &Value::new(7i64)).unwrap();
        assert_eq!(out.downcast_ref::<String>().unwrap(), "7");
    }

    #[test]
    fn converter_rejects_wrong_payload() {
        let mut c = TypeConverters::new();
        c.register_typed::<i64, f64, _>(|x| x as f64);
        // Keyed int->float but the value is actually a string.
        assert!(
            c.convert(&Ty::Int, &Ty::Float, &Value::new("x".to_string()))
                .is_none()
        );
    }

    #[test]
    fn missing_converter_is_none() {
        let c = TypeConverters::new();
        assert!(c.convert(&Ty::Int, &Ty::Str, &Value::new(1i64)).is_none());
        assert!(!c.can_convert(&Ty::Int, &Ty::Str));
    }

    #[test]
    fn compatible_rules() {
        let mut c = TypeConverters::new();
        c.register_typed::<i64, f64, _>(|x| x as f64);
        assert!(c.compatible(&Ty::Float, &Ty::Float)); // identical
        assert!(c.compatible(&Ty::Any, &Ty::Float)); // wildcard source
        assert!(c.compatible(&Ty::Float, &Ty::Any)); // wildcard target
        assert!(c.compatible(&Ty::Int, &Ty::Float)); // via converter
        assert!(!c.compatible(&Ty::Str, &Ty::Float)); // nothing bridges
    }

    #[test]
    fn runtime_types_are_valid_keys() {
        // Domain types built at runtime take part in coercion like any other.
        let row = Ty::record("Row", vec![crate::ty::Field::new("id", Ty::Int)]);
        let mut c = TypeConverters::new();
        c.register(row.clone(), Ty::Str, |v| {
            v.downcast_ref::<String>().map(|s| Value::new(s.clone()))
        });
        assert!(c.compatible(&row, &Ty::Str));
        assert!(!c.compatible(&Ty::Str, &row));
    }

    #[test]
    fn builtins_widen_int_to_float() {
        let c = TypeConverters::with_builtins();
        assert!(c.compatible(&Ty::Int, &Ty::Float));
        let out = c.convert(&Ty::Int, &Ty::Float, &Value::new(5i64)).unwrap();
        assert_eq!(out.downcast_ref::<f64>(), Some(&5.0));
        // Not the other way round: narrowing is a decision, not a default.
        assert!(!c.compatible(&Ty::Float, &Ty::Int));
    }
}
