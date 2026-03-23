use std::path::PathBuf;

use zeughaus_core::*;

/// Injects a DLL into a target process.
/// Input: pid (f64), dll_path (String)
/// Output: success (bool), error (String)
///
/// Uses CreateRemoteThread + LoadLibraryW injection from tamagotchi-injector.
pub struct DllInjectNode {
    pins: Vec<PinDefinition>,
}

impl Default for DllInjectNode {
    fn default() -> Self {
        Self::new()
    }
}

impl DllInjectNode {
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
                    name: "dll_path",
                    direction: PinDirection::Input,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "String",
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

impl ExecutableNode for DllInjectNode {
    fn execute(&mut self, inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let pid_f64: f64 = inputs.get("pid").unwrap_or(0.0);
        let dll_path: String = inputs.get("dll_path").unwrap_or_default();

        if pid_f64 <= 0.0 || dll_path.is_empty() {
            ctx.emit_typed("success", false);
            ctx.emit_typed("error", "Missing pid or dll_path".to_string());
            ctx.flush();
            return Ok(());
        }

        let pid = pid_f64 as u32;
        let path = PathBuf::from(&dll_path);

        match tamagotchi_injector::inject::dll_inject(&path, pid) {
            Ok(()) => {
                ctx.emit_typed("success", true);
                ctx.emit_typed("error", String::new());
            }
            Err(e) => {
                ctx.emit_typed("success", false);
                ctx.emit_typed("error", e.to_string());
            }
        }

        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }
}
