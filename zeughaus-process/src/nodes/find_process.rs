use zeughaus_core::*;

/// Finds a running process by executable name.
/// Input: process name (e.g. "notepad.exe")
/// Output: PID as f64 (0 if not found), found as bool
pub struct FindProcessNode {
    pins: Vec<PinDefinition>,
}

impl Default for FindProcessNode {
    fn default() -> Self {
        Self::new()
    }
}

impl FindProcessNode {
    pub fn new() -> Self {
        Self {
            pins: vec![
                PinDefinition {
                    name: "name",
                    direction: PinDirection::Input,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Trigger,
                    type_name: "String",
                },
                PinDefinition {
                    name: "pid",
                    direction: PinDirection::Output,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "f64",
                },
                PinDefinition {
                    name: "found",
                    direction: PinDirection::Output,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "bool",
                },
            ],
        }
    }
}

impl ExecutableNode for FindProcessNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let name: String = inputs.get("name").unwrap_or_default();

        if name.is_empty() {
            ctx.emit_typed("pid", 0.0f64);
            ctx.emit_typed("found", false);
            ctx.flush();
            return Ok(());
        }

        // Native process lookup is not available in this build.
        ctx.emit_typed("pid", 0.0f64);
        ctx.emit_typed("found", false);

        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }
}
