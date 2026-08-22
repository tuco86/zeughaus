use std::any::Any;
use std::fmt;

use crate::ty::{Repr, Ty, Typed};

/// Object-safe view of a [`Typed`] payload: clone it, downcast it, describe it.
trait TypedAny: Any + Send + Sync {
    fn clone_box(&self) -> Box<dyn TypedAny>;
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn repr(&self) -> Repr<'_>;
}

impl<T: Typed> TypedAny for T {
    fn clone_box(&self) -> Box<dyn TypedAny> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn repr(&self) -> Repr<'_> {
        Typed::repr(self)
    }
}

/// A value travelling through the graph: a type-erased payload plus the domain
/// type it was created with.
///
/// The tag comes from `T::ty()`, never from a caller-supplied string, so a
/// value's declared type and its actual Rust type cannot disagree. That is what
/// lets the executor coerce on the value's own type instead of trusting the
/// source pin's declaration.
pub struct Value {
    ty: Ty,
    inner: Box<dyn TypedAny>,
}

impl Value {
    pub fn new<T: Typed>(val: T) -> Self {
        Self {
            ty: T::ty(),
            inner: Box::new(val),
        }
    }

    /// The value's domain type.
    pub fn ty(&self) -> &Ty {
        &self.ty
    }

    /// Structural view for generic display and inspection. Nominal plugin types
    /// report [`Repr::Opaque`].
    pub fn repr(&self) -> Repr<'_> {
        self.inner.repr()
    }

    pub fn downcast_ref<T: 'static>(&self) -> Option<&T> {
        self.inner.as_any().downcast_ref()
    }

    pub fn downcast_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.inner.as_any_mut().downcast_mut()
    }

    pub fn is<T: 'static>(&self) -> bool {
        self.inner.as_any().is::<T>()
    }
}

impl Clone for Value {
    fn clone(&self) -> Self {
        Self {
            ty: self.ty.clone(),
            inner: self.inner.clone_box(),
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Value({}: {})", self.ty, self.repr())
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.repr() {
            // A nominal type has nothing to show but its name.
            Repr::Opaque => write!(f, "<{}>", self.ty),
            repr => write!(f, "{repr}"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip_f64() {
        let val = Value::new(42.0f64);
        assert_eq!(val.downcast_ref::<f64>(), Some(&42.0));
    }

    #[test]
    fn wrong_type_downcast_fails() {
        let val = Value::new(42.0f64);
        assert_eq!(val.downcast_ref::<String>(), None);
    }

    #[test]
    fn tag_comes_from_the_rust_type() {
        assert_eq!(Value::new(1.0f64).ty(), &Ty::Float);
        assert_eq!(Value::new(1i64).ty(), &Ty::Int);
        assert_eq!(Value::new(true).ty(), &Ty::Bool);
        assert_eq!(Value::new("x".to_string()).ty(), &Ty::Str);
        assert_eq!(Value::new(vec![1.0f64]).ty(), &Ty::list(Ty::Float));
    }

    #[test]
    fn clone_preserves_payload_and_type() {
        let val = Value::new("hello".to_string());
        let copy = val.clone();
        assert_eq!(copy.downcast_ref::<String>().unwrap(), "hello");
        assert_eq!(copy.ty(), &Ty::Str);
    }

    #[test]
    fn downcast_mut_edits_in_place() {
        let mut val = Value::new(1.0f64);
        *val.downcast_mut::<f64>().unwrap() = 2.0;
        assert_eq!(val.downcast_ref::<f64>(), Some(&2.0));
    }

    #[test]
    fn display_uses_the_structural_view() {
        assert_eq!(Value::new(42.0f64).to_string(), "42");
        assert_eq!(Value::new("text".to_string()).to_string(), "text");
        assert_eq!(Value::new(true).to_string(), "true");
        assert_eq!(Value::new(7i64).to_string(), "7");
        assert_eq!(Value::new(vec![1i64, 2]).to_string(), "[1, 2]");
    }

    #[test]
    fn opaque_values_display_as_their_type_name() {
        #[derive(Clone)]
        struct Model;
        impl Typed for Model {
            fn ty() -> Ty {
                Ty::opaque("Model")
            }
        }
        assert_eq!(Value::new(Model).to_string(), "<Model>");
    }
}
