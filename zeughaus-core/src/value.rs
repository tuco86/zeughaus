use std::any::Any;
use std::fmt;

trait CloneableAny: Any + Send + Sync {
    fn clone_box(&self) -> Box<dyn CloneableAny>;
    fn as_any(&self) -> &dyn Any;
    fn as_any_mut(&mut self) -> &mut dyn Any;
    fn type_name(&self) -> &'static str;
}

impl<T: Clone + Send + Sync + 'static> CloneableAny for T {
    fn clone_box(&self) -> Box<dyn CloneableAny> {
        Box::new(self.clone())
    }

    fn as_any(&self) -> &dyn Any {
        self
    }

    fn as_any_mut(&mut self) -> &mut dyn Any {
        self
    }

    fn type_name(&self) -> &'static str {
        std::any::type_name::<T>()
    }
}

pub struct Value {
    inner: Box<dyn CloneableAny>,
}

impl Value {
    pub fn new<T: Clone + Send + Sync + 'static>(val: T) -> Self {
        Self {
            inner: Box::new(val),
        }
    }

    pub fn downcast_ref<T: 'static>(&self) -> Option<&T> {
        self.inner.as_any().downcast_ref()
    }

    pub fn downcast_mut<T: 'static>(&mut self) -> Option<&mut T> {
        self.inner.as_any_mut().downcast_mut()
    }

    pub fn type_name(&self) -> &'static str {
        self.inner.type_name()
    }

    pub fn is<T: 'static>(&self) -> bool {
        self.inner.as_any().is::<T>()
    }
}

impl Clone for Value {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone_box(),
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "Value({})", self.type_name())
    }
}

impl fmt::Display for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let any = self.inner.as_any();
        if let Some(v) = any.downcast_ref::<f64>() {
            write!(f, "{v}")
        } else if let Some(v) = any.downcast_ref::<String>() {
            write!(f, "{v}")
        } else if let Some(v) = any.downcast_ref::<bool>() {
            write!(f, "{v}")
        } else if let Some(v) = any.downcast_ref::<i64>() {
            write!(f, "{v}")
        } else if let Some(v) = any.downcast_ref::<i32>() {
            write!(f, "{v}")
        } else if let Some(v) = any.downcast_ref::<u64>() {
            write!(f, "{v}")
        } else {
            write!(f, "<{}>", self.type_name())
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
    fn wrong_type_returns_none() {
        let val = Value::new(42.0f64);
        assert_eq!(val.downcast_ref::<i32>(), None);
    }

    #[test]
    fn clone_preserves_value() {
        let val = Value::new(String::from("hello"));
        let cloned = val.clone();
        assert_eq!(cloned.downcast_ref::<String>().unwrap(), "hello");
    }

    #[test]
    fn type_name_works() {
        let val = Value::new(42.0f64);
        assert!(val.type_name().contains("f64"));
    }

    #[test]
    fn is_checks_type() {
        let val = Value::new(42.0f64);
        assert!(val.is::<f64>());
        assert!(!val.is::<String>());
    }

    #[test]
    fn downcast_mut_works() {
        let mut val = Value::new(42.0f64);
        *val.downcast_mut::<f64>().unwrap() = 99.0;
        assert_eq!(val.downcast_ref::<f64>(), Some(&99.0));
    }

    #[test]
    fn display_f64() {
        assert_eq!(format!("{}", Value::new(3.14f64)), "3.14");
    }

    #[test]
    fn display_string() {
        assert_eq!(format!("{}", Value::new("hello".to_string())), "hello");
    }

    #[test]
    fn display_bool() {
        assert_eq!(format!("{}", Value::new(true)), "true");
        assert_eq!(format!("{}", Value::new(false)), "false");
    }

    #[test]
    fn display_unknown_type() {
        let val = Value::new(vec![1u8, 2, 3]);
        let s = format!("{val}");
        assert!(s.starts_with('<'));
    }
}
