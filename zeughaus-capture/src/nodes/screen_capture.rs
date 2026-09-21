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
///
/// Which backend runs depends on the session, and their costs are orders of
/// magnitude apart:
///
/// - Wayland, stream already running: sample the newest PipeWire frame from
///   [`crate::screencast`]. No D-Bus, no decode, no deferral -- an `Arc` clone,
///   well under a millisecond, so this path emits synchronously.
/// - Wayland, first execution: the `ScreenCast` portal handshake (a consent
///   dialog on the very first run, then a restore token makes it silent) plus
///   the PipeWire connection and the first frame. Roughly a few hundred
///   milliseconds, so it is deferred.
/// - Wayland without a usable `ScreenCast` interface: the `Screenshot` portal
///   via [`crate::portal`], about 2 s per capture (~0.35 s round trip, the rest
///   PNG decoding). Correct but slow, and it repeats that cost every execution.
/// - X11, Windows, macOS: `scrap`, synchronous.
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
            // The fast path once the stream exists: the compositor has already
            // pushed a frame into `screencast`'s slot, so there is nothing to
            // wait for and deferring would only add a thread hop. Note that a
            // static screen produces no new buffers at all -- the frame kept
            // from the last change is still what the screen looks like.
            if crate::screencast::is_running()
                && let Some(frame) = crate::screencast::latest()
            {
                emit_frame(ctx, frame);
                return Ok(());
            }
            // Otherwise a portal round trip with a compositor on the other end,
            // possibly including a consent dialog, so it goes through the async
            // path instead of blocking the editor's UI thread for the duration.
            ctx.defer(Box::new(WaylandCapture));
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

/// The success shape for the paths that already hold a decoded [`Image`]: the
/// three numbers are derived from it rather than from whatever buffer produced
/// it, so `frame_size` always describes the pixels actually emitted.
#[cfg(target_os = "linux")]
fn emit_frame(ctx: &mut NodeContext, frame: Image) {
    let width = frame.width() as f64;
    let height = frame.height() as f64;
    let frame_size = frame.rgba().len() as f64;
    ctx.emit_typed("frame", frame);
    ctx.emit_typed("width", width);
    ctx.emit_typed("height", height);
    ctx.emit_typed("frame_size", frame_size);
    ctx.emit_typed("captured", true);
    ctx.flush();
}

/// How long to wait for the first frame once the ScreenCast stream is
/// connected. The compositor pushes one as soon as it has a client, so this only
/// has to cover a frame interval and the repack; it is a bound against a stream
/// that negotiated a format this code cannot repack and will therefore never
/// deliver anything.
#[cfg(target_os = "linux")]
const FIRST_FRAME_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

/// The deferred Wayland capture: whatever it takes to obtain the first frame,
/// then the same output shape the synchronous paths emit.
///
/// This runs only on the first execution in a session. Once the ScreenCast
/// stream exists, `execute` samples it synchronously and never gets here.
///
/// Failures come back as `captured = false` plus `error` rather than as an
/// `Err`, so a denied portal permission reads on the node's pins exactly like a
/// failed X11 grab instead of flagging the node as broken.
#[cfg(target_os = "linux")]
struct WaylandCapture;

#[cfg(target_os = "linux")]
impl AsyncWork for WaylandCapture {
    fn run(self: Box<Self>) -> Result<std::collections::HashMap<String, Value>> {
        // ScreenCast first, because it is the only path whose cost is paid once.
        // The Screenshot fallback stays because a session may simply not have
        // the ScreenCast interface -- an older xdg-desktop-portal, or a backend
        // that implements Screenshot only -- and 2 s per capture beats none.
        // A denied grant or a missing interface arrives here as the `Err` text,
        // never as a hang: both backends are timeout-bounded internally.
        let frame = match crate::screencast::wait_for_frame(FIRST_FRAME_TIMEOUT) {
            Ok(frame) => Ok(frame),
            Err(screencast) => {
                crate::portal::capture().map_err(|portal| both_failed(&screencast, &portal))
            }
        };

        let mut outputs = std::collections::HashMap::new();
        match frame {
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

/// The `error` pin text when neither Wayland backend produced a frame.
///
/// Both reasons are kept, ScreenCast first. Reporting only the fallback's would
/// hide why the fast path was unavailable, which is the one thing a user needs
/// to know: "denied or cancelled" is fixed by granting the permission, while
/// "no ScreenCast portal" is fixed by installing a newer portal backend, and the
/// Screenshot error alone distinguishes neither.
#[cfg(target_os = "linux")]
fn both_failed(screencast: &str, portal: &str) -> String {
    format!("{screencast}; screenshot fallback: {portal}")
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

    #[cfg(target_os = "linux")]
    #[test]
    fn both_failed_names_screencast_first_then_the_fallback() {
        // The shape a denied grant produces on the `error` pin when Screenshot
        // cannot save the capture either. The ScreenCast reason must survive:
        // "denied or cancelled" tells the user to grant the permission, and the
        // Screenshot message alone would not.
        let message = both_failed(
            "screencast: denied or cancelled: Cancelled",
            "portal: denied or cancelled: Cancelled",
        );
        assert_eq!(
            message,
            "screencast: denied or cancelled: Cancelled; \
             screenshot fallback: portal: denied or cancelled: Cancelled"
        );
        assert!(message.starts_with("screencast: "));
    }

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
        let mut ctx = NodeContext::new(NodeId::next());
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
