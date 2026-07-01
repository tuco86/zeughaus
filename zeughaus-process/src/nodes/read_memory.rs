use zeughaus_core::*;

/// Reads memory from a remote process.
/// Input: pid (f64), address (f64), size (f64)
/// Output: hex (String representation), bytes_read (f64), success (bool)
pub struct ReadMemoryNode {
    pins: Vec<PinDefinition>,
}

impl Default for ReadMemoryNode {
    fn default() -> Self {
        Self::new()
    }
}

impl ReadMemoryNode {
    pub fn new() -> Self {
        Self {
            pins: vec![
                PinDefinition {
                    name: "pid",
                    direction: PinDirection::Input,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Trigger,
                    type_name: "f64",
                },
                PinDefinition {
                    name: "address",
                    direction: PinDirection::Input,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "f64",
                },
                PinDefinition {
                    name: "size",
                    direction: PinDirection::Input,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "f64",
                },
                PinDefinition {
                    name: "hex",
                    direction: PinDirection::Output,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "String",
                },
                PinDefinition {
                    name: "bytes_read",
                    direction: PinDirection::Output,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "f64",
                },
                PinDefinition {
                    name: "success",
                    direction: PinDirection::Output,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "bool",
                },
                PinDefinition {
                    name: "error",
                    direction: PinDirection::Output,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "String",
                },
            ],
        }
    }
}

impl ExecutableNode for ReadMemoryNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let pid_f64: f64 = inputs.get("pid").unwrap_or(0.0);
        let addr_f64: f64 = inputs.get("address").unwrap_or(0.0);

        if pid_f64 <= 0.0 || addr_f64 <= 0.0 {
            ctx.emit_typed("hex", String::new());
            ctx.emit_typed("bytes_read", 0.0f64);
            ctx.emit_typed("success", false);
            ctx.emit_typed("error", "Missing pid or address".to_string());
            ctx.flush();
            return Ok(());
        }

        // Native process memory reading is not available in this build.
        ctx.emit_typed("hex", String::new());
        ctx.emit_typed("bytes_read", 0.0f64);
        ctx.emit_typed("success", false);
        ctx.emit_typed("error", "Memory reading is not available in this build".to_string());

        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }
}
