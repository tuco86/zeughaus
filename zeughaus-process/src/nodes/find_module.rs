use zeughaus_core::*;

/// Finds a loaded module in a remote process.
/// Input: pid (f64), module_name (String, e.g. "game.dll")
/// Output: base_address (f64), size (f64), path (String), found (bool)
pub struct FindModuleNode {
    pins: Vec<PinDefinition>,
}

impl Default for FindModuleNode {
    fn default() -> Self {
        Self::new()
    }
}

impl FindModuleNode {
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
                    name: "module_name",
                    direction: PinDirection::Input,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "String",
                },
                PinDefinition {
                    name: "base",
                    direction: PinDirection::Output,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "f64",
                },
                PinDefinition {
                    name: "size",
                    direction: PinDirection::Output,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "f64",
                },
                PinDefinition {
                    name: "path",
                    direction: PinDirection::Output,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "String",
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

impl ExecutableNode for FindModuleNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let pid_f64: f64 = inputs.get("pid").unwrap_or(0.0);
        let module_name: String = inputs.get("module_name").unwrap_or_default();

        if pid_f64 <= 0.0 || module_name.is_empty() {
            ctx.emit_typed("base", 0.0f64);
            ctx.emit_typed("size", 0.0f64);
            ctx.emit_typed("path", String::new());
            ctx.emit_typed("found", false);
            ctx.flush();
            return Ok(());
        }

        let pid = pid_f64 as u32;
        match tamagotchi_injector::process::find_module_info(pid, &module_name) {
            Some((base, size, path)) => {
                ctx.emit_typed("base", base as f64);
                ctx.emit_typed("size", size as f64);
                ctx.emit_typed("path", path);
                ctx.emit_typed("found", true);
            }
            None => {
                ctx.emit_typed("base", 0.0f64);
                ctx.emit_typed("size", 0.0f64);
                ctx.emit_typed("path", String::new());
                ctx.emit_typed("found", false);
            }
        }

        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }
}
