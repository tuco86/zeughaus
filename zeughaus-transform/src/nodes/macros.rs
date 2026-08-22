/// Generates a binary f64 node with two inputs and one output.
/// Default pin names are "a", "b" -> "result".
macro_rules! binary_f64_node {
    ($name:ident, $default_a:expr, $default_b:expr, $op:expr) => {
        pub struct $name {
            pins: Vec<zeughaus_core::PinDefinition>,
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl $name {
            pub fn new() -> Self {
                Self {
                    pins: vec![
                        zeughaus_core::PinDefinition::input(
                            "a",
                            zeughaus_core::Ty::Float,
                            zeughaus_core::PinKind::Trigger,
                        ),
                        zeughaus_core::PinDefinition::input(
                            "b",
                            zeughaus_core::Ty::Float,
                            zeughaus_core::PinKind::Trigger,
                        ),
                        zeughaus_core::PinDefinition::output("result", zeughaus_core::Ty::Float),
                    ],
                }
            }
        }

        impl zeughaus_core::ExecutableNode for $name {
            fn execute(
                &mut self,
                inputs: &zeughaus_core::InputSet,
                ctx: &mut zeughaus_core::NodeContext,
            ) -> zeughaus_core::Result<()> {
                let a: f64 = inputs.get("a").unwrap_or($default_a);
                let b: f64 = inputs.get("b").unwrap_or($default_b);
                let op: fn(f64, f64) -> f64 = $op;
                ctx.emit_typed("result", op(a, b));
                ctx.flush();
                Ok(())
            }

            fn pin_definitions(&self) -> &[zeughaus_core::PinDefinition] {
                &self.pins
            }
        }
    };
}

/// Generates a unary f64 node with one input and one output.
/// Pin names: "input" -> "result".
macro_rules! unary_f64_node {
    ($name:ident, $op:expr) => {
        pub struct $name {
            pins: Vec<zeughaus_core::PinDefinition>,
        }

        impl Default for $name {
            fn default() -> Self {
                Self::new()
            }
        }

        impl $name {
            pub fn new() -> Self {
                Self {
                    pins: vec![
                        zeughaus_core::PinDefinition::input(
                            "input",
                            zeughaus_core::Ty::Float,
                            zeughaus_core::PinKind::Trigger,
                        ),
                        zeughaus_core::PinDefinition::output("result", zeughaus_core::Ty::Float),
                    ],
                }
            }
        }

        impl zeughaus_core::ExecutableNode for $name {
            fn execute(
                &mut self,
                inputs: &zeughaus_core::InputSet,
                ctx: &mut zeughaus_core::NodeContext,
            ) -> zeughaus_core::Result<()> {
                let v: f64 = inputs.get("input").unwrap_or(0.0);
                let op: fn(f64) -> f64 = $op;
                ctx.emit_typed("result", op(v));
                ctx.flush();
                Ok(())
            }

            fn pin_definitions(&self) -> &[zeughaus_core::PinDefinition] {
                &self.pins
            }
        }
    };
}

