use zeughaus_core::*;

/// Injects a DLL into a target process.
/// Input: pid (f64), dll_path (String)
/// Output: success (bool), error (String)
///
/// Guard: Only injects once per unique (pid, dll_path) combination.
/// Changing either input resets and allows a new injection.
pub struct DllInjectNode {
    last_pid: u32,
    last_path: String,
    last_success: bool,
    last_error: String,
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
            last_pid: 0,
            last_path: String::new(),
            last_success: false,
            last_error: String::new(),
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

        // Only inject if inputs changed since last execution
        if pid == self.last_pid && dll_path == self.last_path {
            ctx.emit_typed("success", self.last_success);
            ctx.emit_typed("error", self.last_error.clone());
            ctx.flush();
            return Ok(());
        }

        self.last_pid = pid;
        self.last_path = dll_path.clone();

        // Native DLL injection is not available in this build.
        self.last_success = false;
        self.last_error = "DLL injection is not available in this build".to_string();

        ctx.emit_typed("success", self.last_success);
        ctx.emit_typed("error", self.last_error.clone());
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }
}
