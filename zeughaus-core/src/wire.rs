//! Wire form of a value: the text pair a value becomes when it is replicated
//! between editor windows.
//!
//! Exactly one window executes the graph (the owner); every other window is a
//! viewer that displays what the owner published. A viewer therefore needs the
//! owner's output values, and the only channel between windows is the shared
//! state store -- which carries rows of scalars, not Rust values. This module is
//! the translation.
//!
//! It lives outside [`crate::value`] because the wire format is a policy, not
//! part of the value model: which types are worth shipping, and in what text
//! form, is a decision about replication that changes independently of what a
//! `Value` is. `value.rs` stays free of it.

use crate::ty::{Repr, Ty};
use crate::value::Value;

/// Portable form of a scalar value: `(type tag, text)`, where the tag is the
/// value's own [`Ty`] rendered by its `Display` (`bool`, `int`, `float`, `str`).
///
/// Only the four scalars travel. Everything else is `None` on purpose:
///
/// - An [`Ty::Opaque`] plugin type's meaning *is* its Rust implementation, so
///   there is nothing to serialise that the other side could reconstruct.
/// - Bulk payloads must not pass through a state store at all: one 4K RGBA
///   frame is 33 MB, per frame, replicated to every subscriber. Frames stay on
///   the owning runtime and get their own transport.
/// - Lists and records are not refused for a reason of principle, only of
///   scope: no consumer needs them yet, and a composite text encoding would
///   have to answer escaping and nesting questions this format deliberately
///   does not raise.
///
/// The tag and the payload must agree, which is why both [`Value::ty`] and
/// [`Value::repr`] are matched: an `Option<f64>` reports [`Repr::Float`] but its
/// type is `float?`, and emitting that pair would produce a row no decoder
/// accepts.
///
/// Non-finite floats are refused. `NaN` and the infinities are the residue of a
/// computation that already went wrong upstream; a viewer showing nothing (see
/// [`decode_scalar`]) is a more honest rendering than a row that reads `NaN`,
/// and it keeps the format's value set closed -- everything [`decode_scalar`]
/// yields can be encoded again.
pub fn encode_scalar(value: &Value) -> Option<(String, String)> {
    let text = match (value.ty(), value.repr()) {
        (Ty::Bool, Repr::Bool(v)) => v.to_string(),
        (Ty::Int, Repr::Int(v)) => v.to_string(),
        // `{:?}` on f64 is the shortest text that parses back to the same bits,
        // and it always keeps the point (`1.0`, never `1`) so the type tag and
        // the payload cannot drift apart on the way home.
        (Ty::Float, Repr::Float(v)) if v.is_finite() => format!("{v:?}"),
        (Ty::Str, Repr::Str(v)) => v.to_string(),
        _ => return None,
    };
    Some((value.ty().to_string(), text))
}

/// Rebuilds the value that [`encode_scalar`] wrote.
///
/// An unknown tag or text that does not parse yields `None` rather than a
/// panic: the rows arrive from another process, so a truncated, stale or
/// hand-edited row is a normal input here, and it must cost the reading editor
/// nothing more than a pin without a value.
pub fn decode_scalar(ty: &str, text: &str) -> Option<Value> {
    match ty {
        "bool" => text.parse::<bool>().ok().map(Value::new),
        "int" => text.parse::<i64>().ok().map(Value::new),
        "float" => text
            .parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .map(Value::new),
        "str" => Some(Value::new(text.to_string())),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ty::Typed;

    /// Stands in for a nominal plugin type: cloneable, and structureless from
    /// the outside (the default `repr`).
    #[derive(Clone)]
    struct Conversation;

    impl Typed for Conversation {
        fn ty() -> Ty {
            Ty::opaque("Conversation")
        }
    }

    /// Encodes, decodes, and hands back what came out.
    fn round_trip(value: Value) -> (String, Value) {
        let (ty, text) = encode_scalar(&value).expect("scalar must encode");
        let decoded = decode_scalar(&ty, &text).expect("own output must decode");
        assert_eq!(decoded.ty(), value.ty(), "type tag survives the wire");
        (ty, decoded)
    }

    #[test]
    fn each_scalar_round_trips() {
        let (tag, v) = round_trip(Value::new(true));
        assert_eq!(tag, "bool");
        assert_eq!(v.downcast_ref::<bool>(), Some(&true));

        let (tag, v) = round_trip(Value::new(-7i64));
        assert_eq!(tag, "int");
        assert_eq!(v.downcast_ref::<i64>(), Some(&-7));

        let (tag, v) = round_trip(Value::new(0.5f64));
        assert_eq!(tag, "float");
        assert_eq!(v.downcast_ref::<f64>(), Some(&0.5));

        let (tag, v) = round_trip(Value::new("hello, wire".to_string()));
        assert_eq!(tag, "str");
        assert_eq!(
            v.downcast_ref::<String>().map(String::as_str),
            Some("hello, wire")
        );
    }

    #[test]
    fn integral_float_stays_a_float() {
        let (tag, text) = encode_scalar(&Value::new(1.0f64)).unwrap();
        // A bare "1" would decode as an int under any tag-sniffing consumer and
        // would make the pin change type between windows.
        assert_eq!((tag.as_str(), text.as_str()), ("float", "1.0"));

        let decoded = decode_scalar(&tag, &text).unwrap();
        assert_eq!(decoded.ty(), &Ty::Float);
        assert_eq!(decoded.downcast_ref::<f64>(), Some(&1.0));
    }

    #[test]
    fn awkward_floats_round_trip_bit_exactly() {
        for original in [
            -0.1f64,
            -0.0,
            f64::MIN,
            f64::MAX,
            f64::MIN_POSITIVE,
            5e-324, // smallest subnormal
            1.234_567_890_123_456_7e300,
            std::f64::consts::PI,
        ] {
            let (tag, text) = encode_scalar(&Value::new(original)).unwrap();
            assert_eq!(tag, "float");
            let back = *decode_scalar(&tag, &text)
                .unwrap()
                .downcast_ref::<f64>()
                .unwrap();
            assert_eq!(
                back.to_bits(),
                original.to_bits(),
                "{original:?} came back as {back:?} via {text:?}"
            );
        }
    }

    #[test]
    fn non_finite_floats_do_not_travel() {
        for v in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(encode_scalar(&Value::new(v)).is_none());
        }
        // ... and the decoder refuses them too, so the set of values the wire
        // can carry is the same in both directions.
        for text in ["NaN", "inf", "-inf"] {
            assert!(decode_scalar("float", text).is_none(), "{text}");
        }
    }

    #[test]
    fn unknown_tag_is_rejected() {
        assert!(decode_scalar("Conversation", "anything").is_none());
        assert!(decode_scalar("[float]", "[1.0]").is_none());
        assert!(decode_scalar("", "").is_none());
        assert!(decode_scalar("Bool", "true").is_none(), "tags are exact");
    }

    #[test]
    fn garbage_text_is_rejected() {
        assert!(decode_scalar("bool", "yes").is_none());
        assert!(decode_scalar("int", "1.5").is_none());
        assert!(decode_scalar("int", "").is_none());
        assert!(decode_scalar("float", "not a number").is_none());
        assert!(decode_scalar("float", "1.0.0").is_none());
        // Any text is a valid string, including the empty one.
        assert_eq!(
            decode_scalar("str", "").unwrap().downcast_ref::<String>(),
            Some(&String::new())
        );
    }

    #[test]
    fn composites_and_opaque_types_stay_home() {
        assert!(encode_scalar(&Value::new(vec![1.0f64, 2.0])).is_none());
        assert!(encode_scalar(&Value::new(Vec::<f64>::new())).is_none());
        assert!(encode_scalar(&Value::new(Conversation)).is_none());
    }

    #[test]
    fn scalar_payload_under_a_non_scalar_type_is_refused() {
        // `Option<f64>` reports Repr::Float, but its type is `float?`; encoding
        // it would emit a tag no decoder accepts.
        assert!(encode_scalar(&Value::new(Some(1.0f64))).is_none());
        assert!(encode_scalar(&Value::new(Option::<f64>::None)).is_none());
    }
}
