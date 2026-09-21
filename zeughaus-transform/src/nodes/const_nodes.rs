//! The three constant sources: a literal the user types, on one output pin.
//!
//! Each declares a single `value` setting, so the editor configures a constant
//! the same way it configures any other node -- the text the user typed is
//! handed to `set_parameter` and the node parses it. Nothing outside this file
//! knows that `transform.const_*` holds a literal.

use zeughaus_core::*;

pub struct ConstF64Node {
    value: f64,
    pins: Vec<PinDefinition>,
}

impl ConstF64Node {
    pub fn new(value: f64) -> Self {
        Self {
            value,
            pins: vec![PinDefinition::output("value", Ty::Float)],
        }
    }
}

impl ExecutableNode for ConstF64Node {
    fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        ctx.emit_typed("value", self.value);
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    fn settings(&self) -> Vec<SettingDef> {
        vec![SettingDef::new("value", "0")]
    }

    fn set_parameter(&mut self, name: &str, value: Value) -> Result<()> {
        if name != "value" {
            return Ok(());
        }
        if let Some(v) = value.downcast_ref::<f64>() {
            self.value = *v;
        } else if let Some(text) = value.downcast_ref::<String>() {
            self.value = text.trim().parse::<f64>().map_err(|_| {
                ZeughausError::InvalidParameter(format!("{text:?} is not a number"))
            })?;
        }
        Ok(())
    }
}

pub struct ConstBoolNode {
    value: bool,
    pins: Vec<PinDefinition>,
}

impl ConstBoolNode {
    pub fn new(value: bool) -> Self {
        Self {
            value,
            pins: vec![PinDefinition::output("value", Ty::Bool)],
        }
    }
}

impl ExecutableNode for ConstBoolNode {
    fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        ctx.emit_typed("value", self.value);
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    fn settings(&self) -> Vec<SettingDef> {
        vec![SettingDef::new("value", "false")]
    }

    fn set_parameter(&mut self, name: &str, value: Value) -> Result<()> {
        if name != "value" {
            return Ok(());
        }
        if let Some(v) = value.downcast_ref::<bool>() {
            self.value = *v;
        } else if let Some(text) = value.downcast_ref::<String>() {
            // An empty field reads as false so a half-typed setting is not an
            // error the user has to clear before typing the rest.
            self.value = match text.trim() {
                "true" | "1" => true,
                "false" | "0" | "" => false,
                _ => {
                    return Err(ZeughausError::InvalidParameter(format!(
                        "{text:?} is not true or false"
                    )));
                }
            };
        }
        Ok(())
    }
}

pub struct ConstStringNode {
    value: String,
    pins: Vec<PinDefinition>,
}

impl ConstStringNode {
    pub fn new(value: impl Into<String>) -> Self {
        Self {
            value: value.into(),
            pins: vec![PinDefinition::output("value", Ty::Str)],
        }
    }
}

impl ExecutableNode for ConstStringNode {
    fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        ctx.emit_typed("value", self.value.clone());
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }

    fn settings(&self) -> Vec<SettingDef> {
        vec![SettingDef::new("value", "")]
    }

    fn set_parameter(&mut self, name: &str, value: Value) -> Result<()> {
        if name == "value"
            && let Some(text) = value.downcast_ref::<String>()
        {
            self.value = text.clone();
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(node: &mut dyn ExecutableNode) -> std::collections::HashMap<String, Value> {
        let mut ctx = NodeContext::new(NodeId(1));
        node.execute(&InputSet::new(), &mut ctx).unwrap();
        ctx.take_outputs()
    }

    #[test]
    fn f64_emits_configured_value() {
        let mut node = ConstF64Node::new(42.0);
        assert_eq!(run(&mut node)["value"].downcast_ref::<f64>(), Some(&42.0));
    }

    #[test]
    fn f64_set_from_typed_value() {
        let mut node = ConstF64Node::new(0.0);
        node.set_parameter("value", Value::new(99.0f64)).unwrap();
        assert_eq!(run(&mut node)["value"].downcast_ref::<f64>(), Some(&99.0));
    }

    #[test]
    fn f64_set_from_text() {
        let mut node = ConstF64Node::new(0.0);
        node.set_parameter("value", Value::new(" 1.5 ".to_string()))
            .unwrap();
        assert_eq!(run(&mut node)["value"].downcast_ref::<f64>(), Some(&1.5));
    }

    #[test]
    fn f64_refuses_unparsable_text() {
        let mut node = ConstF64Node::new(7.0);
        let error = node
            .set_parameter("value", Value::new("x".to_string()))
            .expect_err("refused");
        assert!(matches!(error, ZeughausError::InvalidParameter(_)));
        assert!(error.to_string().contains("is not a number"));
        // A refused setting leaves the node running on the last good value.
        assert_eq!(run(&mut node)["value"].downcast_ref::<f64>(), Some(&7.0));
    }

    #[test]
    fn bool_emits_configured_value() {
        let mut node = ConstBoolNode::new(true);
        assert_eq!(run(&mut node)["value"].downcast_ref::<bool>(), Some(&true));
    }

    #[test]
    fn bool_set_from_typed_value() {
        let mut node = ConstBoolNode::new(false);
        node.set_parameter("value", Value::new(true)).unwrap();
        assert_eq!(run(&mut node)["value"].downcast_ref::<bool>(), Some(&true));
    }

    #[test]
    fn bool_set_from_text() {
        let mut node = ConstBoolNode::new(false);
        for text in ["true", "1"] {
            node.set_parameter("value", Value::new(text.to_string()))
                .unwrap();
            assert_eq!(
                run(&mut node)["value"].downcast_ref::<bool>(),
                Some(&true),
                "{text}"
            );
        }
        for text in ["false", "0", ""] {
            node.set_parameter("value", Value::new(text.to_string()))
                .unwrap();
            assert_eq!(
                run(&mut node)["value"].downcast_ref::<bool>(),
                Some(&false),
                "{text}"
            );
        }
    }

    #[test]
    fn bool_refuses_unparsable_text() {
        let mut node = ConstBoolNode::new(true);
        let error = node
            .set_parameter("value", Value::new("yes".to_string()))
            .expect_err("refused");
        assert!(matches!(error, ZeughausError::InvalidParameter(_)));
        assert!(error.to_string().contains("is not true or false"));
        assert_eq!(run(&mut node)["value"].downcast_ref::<bool>(), Some(&true));
    }

    #[test]
    fn string_emits_configured_value() {
        let mut node = ConstStringNode::new("hello");
        assert_eq!(
            run(&mut node)["value"].downcast_ref::<String>().unwrap(),
            "hello"
        );
    }

    #[test]
    fn string_takes_text_as_is() {
        let mut node = ConstStringNode::new("");
        node.set_parameter("value", Value::new(" world ".to_string()))
            .unwrap();
        assert_eq!(
            run(&mut node)["value"].downcast_ref::<String>().unwrap(),
            " world "
        );
    }

    #[test]
    fn every_const_declares_its_value_setting() {
        for (node, default) in [
            (
                Box::new(ConstF64Node::new(0.0)) as Box<dyn ExecutableNode>,
                "0",
            ),
            (Box::new(ConstBoolNode::new(false)), "false"),
            (Box::new(ConstStringNode::new("")), ""),
        ] {
            let settings = node.settings();
            assert_eq!(settings.len(), 1);
            assert_eq!(&*settings[0].name, "value");
            assert_eq!(&*settings[0].default, default);
            assert_eq!(&*settings[0].placeholder, default);
        }
    }
}
