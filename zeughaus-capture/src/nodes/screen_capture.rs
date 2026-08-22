use scrap::{Capturer, Display};
use zeughaus_core::*;

/// Captures the primary screen once per execution.
///
/// Emits only what actually happened. On success: `frame`, `width`, `height`,
/// `frame_size`, `captured = true`. On failure: `captured = false` and `error`.
/// The editor dims output pins that produced no value, so the placeholder zeros
/// this node used to emit on the failure paths were actively misleading -- a
/// `width` of 0 reads as a measurement, an absent `width` reads as "no
/// capture".
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
                PinDefinition::input("trigger", Ty::Any, PinKind::Trigger),
                PinDefinition::output("frame", Ty::of::<Image>()),
                PinDefinition::output("width", Ty::Float),
                PinDefinition::output("height", Ty::Float),
                PinDefinition::output("frame_size", Ty::Float),
                PinDefinition::output("captured", Ty::Bool),
                PinDefinition::output("error", Ty::Str),
            ],
        }
    }
}

impl ExecutableNode for ScreenCaptureNode {
    fn execute(&mut self, _inputs: &InputSet, ctx: &mut NodeContext) -> Result<()> {
        // A Wayland session has to go through the portal: `scrap` reads the X11
        // root window, which under Wayland exists but stays black, so the X11
        // path would report a perfectly sized all-zero frame as a success.
        #[cfg(target_os = "linux")]
        if std::env::var_os("WAYLAND_DISPLAY").is_some() {
            // The portal is a D-Bus round trip with a compositor on the other
            // end, so it goes through the async path instead of blocking the
            // editor's UI thread for the duration.
            ctx.defer(Box::new(PortalCapture));
            return Ok(());
        }

        let display = match Display::primary() {
            Ok(d) => d,
            Err(e) => return fail(ctx, format!("display: {e}")),
        };

        let width = display.width() as u32;
        let height = display.height() as u32;
        // A zero dimension would make the stride division below divide by zero
        // and cannot describe a capturable surface anyway.
        if width == 0 || height == 0 {
            return fail(
                ctx,
                format!("display: degenerate geometry {width}x{height}"),
            );
        }

        let mut capturer = match Capturer::new(display) {
            Ok(c) => c,
            Err(e) => return fail(ctx, format!("capturer: {e}")),
        };

        for _ in 0..10 {
            match capturer.frame() {
                Ok(frame) => {
                    // `scrap` gives no stride accessor, so derive it from the
                    // buffer: the frame is `height` rows of `stride` bytes.
                    let stride = frame.len() / height as usize;
                    if stride < width as usize * 4 {
                        return fail(
                            ctx,
                            format!(
                                "frame: short buffer, {} bytes for {width}x{height}",
                                frame.len()
                            ),
                        );
                    }
                    let frame_size = frame.len() as f64;
                    let rgba = bgra_to_rgba(width, height, stride, &frame);
                    ctx.emit_typed("frame", Image::from_rgba(width, height, rgba));
                    ctx.emit_typed("width", width as f64);
                    ctx.emit_typed("height", height as f64);
                    ctx.emit_typed("frame_size", frame_size);
                    ctx.emit_typed("captured", true);
                    ctx.flush();
                    return Ok(());
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(e) => return fail(ctx, format!("frame: {e}")),
            }
        }

        fail(ctx, "frame: no frame ready after 10 attempts".to_string())
    }

    fn pin_definitions(&self) -> &[PinDefinition] {
        &self.pins
    }
}

/// The one failure shape, shared by every early exit: the two pins that mean
/// "no frame", and nothing else. Emitting a zeroed `frame_size` here would be
/// indistinguishable downstream from a real capture of that size.
fn fail(ctx: &mut NodeContext, message: String) -> Result<()> {
    ctx.emit_typed("captured", false);
    ctx.emit_typed("error", message);
    ctx.flush();
    Ok(())
}

/// The deferred Wayland capture: one portal round trip, then the same output
/// shape the synchronous path emits.
///
/// Failures come back as `captured = false` plus `error` rather than as an
/// `Err`, so a denied portal permission reads on the node's pins exactly like a
/// failed X11 grab instead of flagging the node as broken.
#[cfg(target_os = "linux")]
struct PortalCapture;

#[cfg(target_os = "linux")]
impl AsyncWork for PortalCapture {
    fn run(self: Box<Self>) -> Result<std::collections::HashMap<String, Value>> {
        let mut outputs = std::collections::HashMap::new();
        match crate::portal::capture() {
            Ok(frame) => {
                outputs.insert("width".to_string(), Value::new(frame.width() as f64));
                outputs.insert("height".to_string(), Value::new(frame.height() as f64));
                outputs.insert(
                    "frame_size".to_string(),
                    Value::new(frame.rgba().len() as f64),
                );
                outputs.insert("captured".to_string(), Value::new(true));
                outputs.insert("frame".to_string(), Value::new(frame));
            }
            Err(message) => {
                outputs.insert("captured".to_string(), Value::new(false));
                outputs.insert("error".to_string(), Value::new(message));
            }
        }
        Ok(outputs)
    }
}

/// Repacks a `scrap` frame into the tight RGBA8 buffer [`Image`] requires.
///
/// Two mismatches have to be fixed here, and both fail silently if missed:
/// `scrap` hands out pixels as BGRA (copied straight through, every image comes
/// out blue-tinted), and its rows are padded to the capturer's stride, which
/// can exceed `width * 4` (keeping the padding skews every row after the
/// first). `src` must hold `height` rows of `stride` bytes with
/// `stride >= width * 4`; the caller checks that.
///
/// The fourth channel is forced opaque rather than copied: several backends
/// hand out BGRX, where that byte is undefined and often zero. Trusting it
/// makes the whole capture render fully transparent, and a screen capture has
/// no meaningful transparency to preserve.
fn bgra_to_rgba(width: u32, height: u32, stride: usize, src: &[u8]) -> Vec<u8> {
    let row_bytes = width as usize * 4;
    let mut out = Vec::with_capacity(row_bytes * height as usize);
    for y in 0..height as usize {
        let row = &src[y * stride..y * stride + row_bytes];
        for px in row.as_chunks::<4>().0 {
            out.extend_from_slice(&[px[2], px[1], px[0], 0xFF]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn frame_is_the_first_output_pin() {
        let node = ScreenCaptureNode::new();
        let outputs: Vec<&str> = node
            .pin_definitions()
            .iter()
            .filter(|p| p.direction == PinDirection::Output)
            .map(|p| &*p.name)
            .collect();
        assert_eq!(
            outputs,
            [
                "frame",
                "width",
                "height",
                "frame_size",
                "captured",
                "error"
            ]
        );
        let frame = &node.pin_definitions()[1];
        assert_eq!(frame.ty, Ty::opaque("image"));
    }

    #[test]
    fn bgra_to_rgba_drops_padding_swaps_channels_and_forces_opacity() {
        // 2x2 BGRA with 4 bytes of row padding: stride 12, row_bytes 8. The
        // source alpha bytes (4, 8, 12, 16) must NOT survive -- a BGRX backend
        // would otherwise render the frame invisible.
        let src = vec![
            1, 2, 3, 4, 5, 6, 7, 8, 0xDE, 0xAD, 0xBE, 0xEF, // row 0 + padding
            9, 10, 11, 12, 13, 14, 15, 16, 0xDE, 0xAD, 0xBE, 0xEF, // row 1 + padding
        ];
        let out = bgra_to_rgba(2, 2, 12, &src);
        assert_eq!(
            out,
            vec![
                3, 2, 1, 0xFF, 7, 6, 5, 0xFF, 11, 10, 9, 0xFF, 15, 14, 13, 0xFF
            ]
        );
    }

    #[test]
    fn bgra_to_rgba_output_fits_image_exactly() {
        let src = vec![0u8; 3 * 16];
        let out = bgra_to_rgba(2, 3, 16, &src);
        // Would panic if the packing were wrong.
        let img = Image::from_rgba(2, 3, out);
        assert_eq!((img.width(), img.height()), (2, 3));
    }

    #[test]
    fn failure_emits_only_captured_and_error() {
        let mut ctx = NodeContext::new(NodeId::next(), 0);
        fail(&mut ctx, "display: none".to_string()).unwrap();
        let outputs = ctx.take_outputs();
        assert_eq!(outputs.len(), 2);
        assert_eq!(outputs["captured"].downcast_ref::<bool>(), Some(&false));
        assert_eq!(
            outputs["error"]
                .downcast_ref::<String>()
                .map(String::as_str),
            Some("display: none")
        );
    }
}
