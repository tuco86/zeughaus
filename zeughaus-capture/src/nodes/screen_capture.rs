use scrap::{Capturer, Display};
use zeughaus_core::*;

/// Captures the primary screen once per execution.
/// Output: width (f64), height (f64), frame_size (f64 bytes), captured (bool)
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
            ],
        }
    }
}

impl ExecutableNode for ScreenCaptureNode {
    fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        let display = match Display::primary() {
            Ok(d) => d,
            Err(e) => {
                eprintln!("ScreenCapture: display error: {e}");
                ctx.emit_typed("width", 0.0f64);
                ctx.emit_typed("height", 0.0f64);
                ctx.emit_typed("frame_size", 0.0f64);
                ctx.emit_typed("captured", false);
                ctx.flush();
                return Ok(());
            }
        };

        let width = display.width() as f64;
        let height = display.height() as f64;

        let mut capturer = match Capturer::new(display) {
            Ok(c) => c,
            Err(e) => {
                eprintln!("ScreenCapture: capturer error: {e}");
                ctx.emit_typed("width", width);
                ctx.emit_typed("height", height);
                ctx.emit_typed("frame_size", 0.0f64);
                ctx.emit_typed("captured", false);
                ctx.flush();
                return Ok(());
            }
        };

        // Try to grab a frame. May need a brief wait for the first frame.
        let mut frame_size = 0.0f64;
        let mut captured = false;

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
                    eprintln!("ScreenCapture: frame error: {e}");
                    break;
                }
            }
        }

        ctx.emit_typed("width", width);
        ctx.emit_typed("height", height);
        ctx.emit_typed("frame_size", frame_size);
        ctx.emit_typed("captured", captured);
        ctx.flush();
        Ok(())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }
}
