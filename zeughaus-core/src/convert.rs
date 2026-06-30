//! Type converters: the runtime's answer to "connect a `u8` output to a `u32`
//! input". `Value` is type-erased, so Rust's `Into` cannot be dispatched across
//! an edge at runtime. Instead, plugins register converter functions keyed by
//! the declared pin type names, and the executor coerces a value as it crosses
//! an edge whose endpoints declare different types.
//!
//! Connection compatibility is then: same type, or one side is `any`, or a
//! converter exists. This keeps the ergonomics of implicit widening without
//! per-edge adapter nodes, while leaving the dependency explicit (the edge is
//! still there, it just carries a converter).

use std::collections::HashMap;

use crate::value::Value;

/// A converter takes the source value and returns the coerced value, or `None`
/// if the value was not the expected concrete type (a defensive no-op).
type Converter = Box<dyn Fn(&Value) -> Option<Value> + Send + Sync>;

/// Registry of type converters keyed by `(from_type, to_type)` declared pin
/// type names. Built once at startup from the plugins and shared (via `Arc`)
/// between the editor's connection validation and the runtime's coercion.
#[derive(Default)]
pub struct TypeConverters {
    map: HashMap<(String, String), Converter>,
}

impl TypeConverters {
    pub fn new() -> Self {
        Self::default()
    }

    /// Registers a raw converter between two declared type names.
    pub fn register<F>(&mut self, from: &str, to: &str, f: F)
    where
        F: Fn(&Value) -> Option<Value> + Send + Sync + 'static,
    {
        self.map.insert((from.to_string(), to.to_string()), Box::new(f));
    }

    /// Ergonomic helper: register a converter from concrete Rust type `A` to
    /// `B`, handling the downcast and re-wrap. Mirrors a `From<A> for B` impl.
    pub fn register_typed<A, B, F>(&mut self, from: &str, to: &str, f: F)
    where
        A: Clone + 'static,
        B: Clone + Send + Sync + 'static,
        F: Fn(A) -> B + Send + Sync + 'static,
    {
        self.register(from, to, move |v| v.downcast_ref::<A>().map(|a| Value::new(f(a.clone()))));
    }

    /// Whether a converter exists for this type pair.
    pub fn can_convert(&self, from: &str, to: &str) -> bool {
        self.map.contains_key(&(from.to_string(), to.to_string()))
    }

    /// Coerces `value` from `from` to `to`. Returns `None` if no converter is
    /// registered or the value did not hold the expected concrete type.
    pub fn convert(&self, from: &str, to: &str, value: &Value) -> Option<Value> {
        self.map.get(&(from.to_string(), to.to_string())).and_then(|f| f(value))
    }

    /// Whether an output of type `from` may connect to an input of type `to`:
    /// identical types, an `any` wildcard on either side, or a registered
    /// converter bridges them.
    pub fn compatible(&self, from: &str, to: &str) -> bool {
        from == to || from == "any" || to == "any" || self.can_convert(from, to)
    }

    /// A registry seeded with the common numeric widenings, so plugins that
    /// declare integer pins interoperate with `f64`/wider-integer pins out of
    /// the box. Plugins add their own domain converters on top.
    pub fn with_builtins() -> Self {
        let mut c = Self::new();
        // Unsigned widening into u64.
        c.register_typed::<u8, u64, _>("u8", "u64", |x| x as u64);
        c.register_typed::<u16, u64, _>("u16", "u64", |x| x as u64);
        c.register_typed::<u32, u64, _>("u32", "u64", |x| x as u64);
        // Signed widening into i64.
        c.register_typed::<i8, i64, _>("i8", "i64", |x| x as i64);
        c.register_typed::<i16, i64, _>("i16", "i64", |x| x as i64);
        c.register_typed::<i32, i64, _>("i32", "i64", |x| x as i64);
        // Into f64 (the transform plugin's lingua franca).
        c.register_typed::<i32, f64, _>("i32", "f64", |x| x as f64);
        c.register_typed::<i64, f64, _>("i64", "f64", |x| x as f64);
        c.register_typed::<u32, f64, _>("u32", "f64", |x| x as f64);
        c.register_typed::<u64, f64, _>("u64", "f64", |x| x as f64);
        c.register_typed::<f32, f64, _>("f32", "f64", |x| x as f64);
        c
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_and_convert_typed() {
        let mut c = TypeConverters::new();
        c.register_typed::<u8, u32, _>("u8", "u32", |x| x as u32);
        let out = c.convert("u8", "u32", &Value::new(7u8)).unwrap();
        assert_eq!(out.downcast_ref::<u32>(), Some(&7u32));
    }

    #[test]
    fn convert_wrong_concrete_type_is_none() {
        let mut c = TypeConverters::new();
        c.register_typed::<u8, u32, _>("u8", "u32", |x| x as u32);
        // Registered as u8->u32 but the value is actually a String.
        assert!(c.convert("u8", "u32", &Value::new("x".to_string())).is_none());
    }

    #[test]
    fn missing_converter_is_none() {
        let c = TypeConverters::new();
        assert!(c.convert("u8", "u32", &Value::new(1u8)).is_none());
        assert!(!c.can_convert("u8", "u32"));
    }

    #[test]
    fn compatible_rules() {
        let mut c = TypeConverters::new();
        c.register_typed::<u8, f64, _>("u8", "f64", |x| x as f64);
        assert!(c.compatible("f64", "f64")); // identical
        assert!(c.compatible("any", "f64")); // wildcard source
        assert!(c.compatible("f64", "any")); // wildcard target
        assert!(c.compatible("u8", "f64")); // via converter
        assert!(!c.compatible("String", "f64")); // nothing bridges
    }

    #[test]
    fn builtins_widen_numbers() {
        let c = TypeConverters::with_builtins();
        assert!(c.compatible("u8", "u64"));
        assert!(c.compatible("i32", "f64"));
        let out = c.convert("i32", "f64", &Value::new(5i32)).unwrap();
        assert_eq!(out.downcast_ref::<f64>(), Some(&5.0));
    }
}
