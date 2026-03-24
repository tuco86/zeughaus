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
                        zeughaus_core::PinDefinition {
                            name: "a",
                            direction: zeughaus_core::PinDirection::Input,
                            data_mode: zeughaus_core::DataMode::Value,
                            pin_kind: zeughaus_core::PinKind::Trigger,
                            type_name: "f64",
                        },
                        zeughaus_core::PinDefinition {
                            name: "b",
                            direction: zeughaus_core::PinDirection::Input,
                            data_mode: zeughaus_core::DataMode::Value,
                            pin_kind: zeughaus_core::PinKind::Trigger,
                            type_name: "f64",
                        },
                        zeughaus_core::PinDefinition {
                            name: "result",
                            direction: zeughaus_core::PinDirection::Output,
                            data_mode: zeughaus_core::DataMode::Value,
                            pin_kind: zeughaus_core::PinKind::Sample,
                            type_name: "f64",
                        },
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
                        zeughaus_core::PinDefinition {
                            name: "input",
                            direction: zeughaus_core::PinDirection::Input,
                            data_mode: zeughaus_core::DataMode::Value,
                            pin_kind: zeughaus_core::PinKind::Trigger,
                            type_name: "f64",
                        },
                        zeughaus_core::PinDefinition {
                            name: "result",
                            direction: zeughaus_core::PinDirection::Output,
                            data_mode: zeughaus_core::DataMode::Value,
                            pin_kind: zeughaus_core::PinKind::Sample,
                            type_name: "f64",
                        },
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

