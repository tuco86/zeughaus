use scrap::{Capturer, Display};
use zeughaus_core::*;

/// Captures the primary screen once per execution.
/// Output: width, height, frame_size, captured (bool), error (String)
pub struct ScreenCaptureNode {
    pins: Vec<PinDefinition>,
}

impl Default for ScreenCaptureNode {
    fn default() -> Self {
        Self::new()
    }
}

impl ScreenCaptureNode {
    pub fn new() -> Self {
        Self {
            pins: vec![
                PinDefinition {
                    name: "trigger",
                    direction: PinDirection::Input,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Trigger,
                    type_name: "any",
                },
                PinDefinition {
                    name: "width",
                    direction: PinDirection::Output,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "f64",
                },
                PinDefinition {
                    name: "height",
                    direction: PinDirection::Output,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "f64",
                },
                PinDefinition {
                    name: "frame_size",
                    direction: PinDirection::Output,
                    data_mode: DataMode::Value,
                    pin_kind: PinKind::Sample,
                    type_name: "f64",
                },
                PinDefinition {
                    name: "captured",
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

impl ExecutableNode for ScreenCaptureNode {
    fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let display = match Display::primary() {
            Ok(d) => d,
            Err(e) => {
                ctx.emit_typed("width", 0.0f64);
                ctx.emit_typed("height", 0.0f64);
                ctx.emit_typed("frame_size", 0.0f64);
                ctx.emit_typed("captured", false);
                ctx.emit_typed("error", format!("display: {e}"));
                ctx.flush();
                return Ok(());
            }
        };

        let width = display.width() as f64;
        let height = display.height() as f64;

        let mut capturer = match Capturer::new(display) {
            Ok(c) => c,
            Err(e) => {
                ctx.emit_typed("width", width);
                ctx.emit_typed("height", height);
                ctx.emit_typed("frame_size", 0.0f64);
                ctx.emit_typed("captured", false);
                ctx.emit_typed("error", format!("capturer: {e}"));
                ctx.flush();
                return Ok(());
            }
        };

        let mut frame_size = 0.0f64;
        let mut captured = false;
        let mut last_error = String::new();

        for _ in 0..10 {
            match capturer.frame() {
                Ok(frame) => {
                    frame_size = frame.len() as f64;
                    captured = true;
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => {
                    last_error = format!("frame: {e}");
                    break;
                }
            }
        }

        ctx.emit_typed("width", width);
        ctx.emit_typed("height", height);
        ctx.emit_typed("frame_size", frame_size);
        ctx.emit_typed("captured", captured);
        ctx.emit_typed("error", last_error);
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }
}
