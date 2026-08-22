//! Wayland screen capture through xdg-desktop-portal's `ScreenCast` interface.
//!
//! [`crate::portal`] captures through `Screenshot`: one D-Bus round trip and one
//! PNG per frame, about 2 s on a 3840x2160 screen. `ScreenCast` moves the cost
//! to a one-time handshake and then hands out raw mapped buffers over PipeWire,
//! so a capture becomes a clone of an `Arc` -- microseconds, with no encode or
//! decode anywhere in the path.
//!
//! The shape that buys that: everything here is process-wide and started once.
//! A portal session and its PipeWire stream are not per-capture resources -- the
//! handshake shows a consent dialog, and re-running it per frame would prompt
//! the user once a second. So the session is opened on first use and then kept
//! for the lifetime of the process, a dedicated thread runs the PipeWire loop,
//! and callers only ever read the newest frame it has produced.
//!
//! Only the newest frame is kept, deliberately. A compositor pushes buffers at
//! its own rate while a graph may sample at 1 Hz; queueing them would grow
//! without bound at 33 MB per 4K frame. Overwriting a single slot also gives the
//! right semantics for "capture the screen now".
//!
//! Note that PipeWire only delivers a buffer when the screen actually changes.
//! On a static desktop no `process` callback runs for seconds at a time, so
//! [`latest`] keeps returning the last frame received rather than `None`: the
//! screen has not changed, so that frame is still what the screen looks like.

use std::ffi::OsString;
use std::io::Write;
use std::os::fd::OwnedFd;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::sync::{LazyLock, Mutex};
use std::time::{Duration, Instant};

use ashpd::desktop::screencast::{CursorMode, Screencast, SelectSourcesOptions, SourceType};
use ashpd::desktop::{PersistMode, Session};
use ashpd::enumflags2::BitFlags;
use pipewire as pw;
use pw::spa;
use zeughaus_core::Image;

/// How long the portal handshake may take. It contains a user interaction (the
/// consent dialog on the first run), so this is generous compared to
/// [`crate::portal`]'s timeout -- but it is still bounded, because a wedged
/// portal must not leave a node pending forever.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(60);

/// How long to wait for the PipeWire stream to reach `Streaming` after the
/// portal handed over the remote FD. This is machine-local plumbing with no
/// user in the loop, so it either happens promptly or it is broken.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(5);

/// The newest frame the PipeWire thread has produced, or `None` before the first
/// one arrives.
///
/// Written only by the stream's `process` callback and read by capture nodes on
/// their own threads. The lock is held for a pointer swap, never across the
/// conversion: the RGBA repack happens outside it so a slow reader can never
/// stall the realtime callback.
static LATEST: Mutex<Option<Image>> = Mutex::new(None);

/// Whether the handshake has already run, and how it went.
///
/// A failure is cached rather than retried. If this session has no `ScreenCast`
/// interface, or the user denied the request, retrying on every execution would
/// mean a denied dialog reappearing once per graph tick. The caller falls back
/// to [`crate::portal`] instead, which is slow but works.
static START: Mutex<StartState> = Mutex::new(StartState::Idle);

/// The portal session, parked for the lifetime of the process.
///
/// The stream lives exactly as long as this session: dropping it -- or letting a
/// future `ashpd` grow a `Drop` that calls `Close` -- would tear the PipeWire
/// node down under the loop thread, which would then sit in `run()` forever with
/// nothing to deliver.
static SESSION: Mutex<Option<Session<Screencast>>> = Mutex::new(None);

enum StartState {
    Idle,
    Running,
    Failed(String),
}

/// Opens the capture stream, once per process. Cheap and idempotent on every
/// call after the first.
///
/// Returns when the stream is connected and streaming, which is the point from
/// which frames start arriving; the first one still takes a compositor frame
/// interval, so callers that need a frame use [`wait_for_frame`].
pub fn start() -> Result<(), String> {
    // Poisoning carries no state worth protecting here, and a panicking start
    // must not permanently disable capture for the rest of the process.
    let mut state = START.lock().unwrap_or_else(|e| e.into_inner());
    match &*state {
        StartState::Running => return Ok(()),
        StartState::Failed(message) => return Err(message.clone()),
        StartState::Idle => {}
    }
    match open_stream() {
        Ok(()) => {
            *state = StartState::Running;
            Ok(())
        }
        Err(message) => {
            *state = StartState::Failed(message.clone());
            Err(message)
        }
    }
}

/// Whether [`start`] has already succeeded, so a caller can sample without
/// risking the handshake's D-Bus round trip on a latency-sensitive thread.
pub fn is_running() -> bool {
    matches!(
        &*START.lock().unwrap_or_else(|e| e.into_inner()),
        StartState::Running
    )
}

/// The newest frame, or `None` if none has arrived yet. Never blocks on the
/// compositor: this is a lock and a refcount bump.
pub fn latest() -> Option<Image> {
    LATEST.lock().unwrap_or_else(|e| e.into_inner()).clone()
}

/// Starts the stream if needed and returns the first frame it produces.
///
/// The poll loop exists because PipeWire's `process` callback runs on the loop
/// thread and there is nothing to await from here; the interval is short enough
/// to be invisible next to the handshake it follows.
pub fn wait_for_frame(timeout: Duration) -> Result<Image, String> {
    start()?;
    let deadline = Instant::now() + timeout;
    loop {
        if let Some(frame) = latest() {
            return Ok(frame);
        }
        if Instant::now() >= deadline {
            return Err(format!(
                "screencast: no frame within {} ms of the stream starting",
                timeout.as_millis()
            ));
        }
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// The handshake plus the loop thread. Called under the [`START`] lock, so it
/// runs at most once concurrently.
fn open_stream() -> Result<(), String> {
    let (fd, node_id) = crate::portal::runtime()?.block_on(handshake())?;

    let (ready_tx, ready_rx) = mpsc::channel();
    // A dedicated thread, not a pooled one: `MainLoop::run` never returns, and
    // the PipeWire objects it owns are `!Send` single-threaded types that must
    // all be created and destroyed on the thread that polls the loop.
    std::thread::Builder::new()
        .name("zeughaus-screencast".to_string())
        .spawn(move || {
            if let Err(message) = run_loop(fd, node_id, &ready_tx) {
                let _ = ready_tx.send(Err(message));
            }
        })
        .map_err(|e| format!("screencast: cannot spawn pipewire thread: {e}"))?;

    match ready_rx.recv_timeout(CONNECT_TIMEOUT) {
        Ok(result) => result,
        Err(_) => Err(format!(
            "screencast: pipewire stream not streaming within {} s",
            CONNECT_TIMEOUT.as_secs()
        )),
    }
}

/// Why a handshake attempt failed, and whether the restore token could be the
/// reason.
///
/// This distinction is the whole point of the type. Discarding the token on any
/// failure looks harmless and is not: a portal that is not running yet, or a
/// D-Bus session that briefly went away, has said nothing about the token, and
/// throwing it away there costs the user a consent dialog for an outage that had
/// nothing to do with their grant. Only a refusal -- the portal answered, and
/// said no -- implicates it.
struct HandshakeError {
    message: String,
    refused: bool,
}

impl HandshakeError {
    /// The portal could not be reached, or never answered.
    fn unreachable(message: String) -> Self {
        Self {
            message,
            refused: false,
        }
    }

    /// The portal answered and refused the request.
    fn refused(message: String) -> Self {
        Self {
            message,
            refused: true,
        }
    }

    /// Whether a stale restore token is a plausible cause of this failure, and
    /// therefore whether it is worth discarding and asking the user afresh.
    fn implicates_token(&self) -> bool {
        self.refused
    }
}

/// Runs the portal handshake and yields the PipeWire remote FD and node id.
///
/// A stored token is offered first. If the portal refuses it -- a reboot, a
/// revoked grant, a changed monitor layout -- the token is discarded and the
/// handshake retried once without it, so a dead token costs the user a fresh
/// dialog rather than a capture error. Any other failure keeps the token and
/// fails immediately: retrying an unreachable portal cannot help, and the token
/// is still the user's valid grant for when it comes back.
async fn handshake() -> Result<(OwnedFd, u32), String> {
    let token = read_token(token_path());
    if token.is_some() {
        match request_session(token.as_deref()).await {
            Ok(result) => return Ok(result),
            Err(error) if !error.implicates_token() => return Err(error.message),
            Err(_) => {
                let _ = std::fs::remove_file(token_path());
            }
        }
    }
    request_session(None).await.map_err(|e| e.message)
}

async fn request_session(restore_token: Option<&str>) -> Result<(OwnedFd, u32), HandshakeError> {
    let request = async {
        let proxy = Screencast::new().await.map_err(|e| {
            HandshakeError::unreachable(format!("screencast: no ScreenCast portal: {e}"))
        })?;
        let session = proxy.create_session(Default::default()).await.map_err(|e| {
            HandshakeError::unreachable(format!("screencast: cannot create session: {e}"))
        })?;

        proxy
            .select_sources(
                &session,
                SelectSourcesOptions::default()
                    // A monitor, not a window: this node captures the screen,
                    // and a window source would make the frame depend on which
                    // window the user happened to pick in the dialog.
                    .set_sources(BitFlags::from(SourceType::Monitor))
                    // One monitor: the node emits a single `Image`, so a second
                    // stream would have nowhere to go.
                    .set_multiple(false)
                    // The pointer is part of what a capture is for -- a
                    // screenshot the user took to show where they are pointing
                    // is useless without it. `Embedded` composites it into the
                    // frame; `Metadata` would hand it over as a separate buffer
                    // this pipeline has no way to draw.
                    .set_cursor_mode(CursorMode::Embedded)
                    // Ask the compositor to remember the grant beyond this
                    // process, so the consent dialog is a one-time cost rather
                    // than a per-launch one.
                    .set_persist_mode(PersistMode::ExplicitlyRevoked)
                    .set_restore_token(restore_token),
            )
            .await
            .map_err(|e| {
                HandshakeError::unreachable(format!("screencast: cannot select sources: {e}"))
            })?
            .response()
            .map_err(|e| {
                HandshakeError::refused(format!("screencast: source selection refused: {e}"))
            })?;

        let streams = proxy
            .start(&session, None, Default::default())
            .await
            .map_err(|e| {
                HandshakeError::unreachable(format!("screencast: cannot start session: {e}"))
            })?
            .response()
            // Where a user pressing Cancel on the consent dialog lands, and also
            // where a token the compositor no longer honours lands.
            .map_err(|e| {
                HandshakeError::refused(format!("screencast: denied or cancelled: {e}"))
            })?;

        // The portal mints a fresh token per start, so this must be written back
        // every time, not just when there was none before.
        if let Some(token) = streams.restore_token() {
            write_token(token_path(), token);
        }

        let stream = streams.streams().first().ok_or_else(|| {
            HandshakeError::refused("screencast: portal returned no stream".to_string())
        })?;
        let node_id = stream.pipe_wire_node_id();

        let fd = proxy
            .open_pipe_wire_remote(&session, Default::default())
            .await
            .map_err(|e| {
                HandshakeError::unreachable(format!(
                    "screencast: cannot open pipewire remote: {e}"
                ))
            })?;

        *SESSION.lock().unwrap_or_else(|e| e.into_inner()) = Some(session);
        Ok((fd, node_id))
    };

    match tokio::time::timeout(HANDSHAKE_TIMEOUT, request).await {
        Ok(result) => result,
        // A timeout is not a refusal: the portal may simply be slow to start, and
        // the token is still good.
        Err(_) => Err(HandshakeError::unreachable(format!(
            "screencast: no response within {} s",
            HANDSHAKE_TIMEOUT.as_secs()
        ))),
    }
}

/// Owns every PipeWire object and never returns while the stream is healthy.
///
/// `ready` reports the one transition callers care about -- reaching `Streaming`
/// or failing -- so [`open_stream`] can distinguish "connected, frames coming"
/// from "the remote refused us" instead of leaving a node to time out.
fn run_loop(
    fd: OwnedFd,
    node_id: u32,
    ready: &mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    pw::init();

    let mainloop = pw::main_loop::MainLoopBox::new(None)
        .map_err(|e| format!("screencast: pipewire loop: {e}"))?;
    let context = pw::context::ContextBox::new(mainloop.loop_(), None)
        .map_err(|e| format!("screencast: pipewire context: {e}"))?;
    // `connect_fd`, not `connect`: the portal's FD is the only handle that
    // carries the permission for this node. A plain connection to the user's
    // session daemon would be rejected for a screen node.
    let core = context
        .connect_fd(fd, None)
        .map_err(|e| format!("screencast: pipewire remote: {e}"))?;

    let stream = pw::stream::StreamBox::new(
        &core,
        "zeughaus-capture",
        pw::properties::properties! {
            *pw::keys::MEDIA_TYPE => "Video",
            *pw::keys::MEDIA_CATEGORY => "Capture",
            *pw::keys::MEDIA_ROLE => "Screen",
        },
    )
    .map_err(|e| format!("screencast: pipewire stream: {e}"))?;

    let state_ready = ready.clone();
    let _listener = stream
        .add_local_listener_with_user_data(Negotiated::default())
        .state_changed(move |_, _, _, new| match new {
            pw::stream::StreamState::Streaming => {
                let _ = state_ready.send(Ok(()));
            }
            pw::stream::StreamState::Error(message) => {
                let _ = state_ready.send(Err(format!("screencast: stream error: {message}")));
            }
            _ => {}
        })
        .param_changed(|_, negotiated, id, param| {
            let Some(param) = param else { return };
            if id != spa::param::ParamType::Format.as_raw() {
                return;
            }
            let Ok((media_type, media_subtype)) = spa::param::format_utils::parse_format(param)
            else {
                return;
            };
            if media_type != spa::param::format::MediaType::Video
                || media_subtype != spa::param::format::MediaSubtype::Raw
            {
                return;
            }
            let mut info = spa::param::video::VideoInfoRaw::new();
            if info.parse(param).is_err() {
                return;
            }
            // The format the compositor settled on is the only place the frame
            // geometry and channel order are stated. Buffers carry a size and a
            // stride but no layout, so a `process` callback with no negotiated
            // format has no way to interpret its bytes and must drop them.
            *negotiated = PixelOrder::from_spa(info.format()).map(|order| Format {
                order,
                width: info.size().width,
                height: info.size().height,
            });
        })
        .process(|stream, negotiated| {
            let Some(format) = *negotiated else { return };
            let Some(mut buffer) = stream.dequeue_buffer() else {
                return;
            };
            let datas = buffer.datas_mut();
            let Some(data) = datas.first_mut() else {
                return;
            };
            let chunk = data.chunk();
            let (offset, size, stride) = (
                chunk.offset() as usize,
                chunk.size() as usize,
                chunk.stride(),
            );
            // A negative stride means bottom-up rows, which no compositor emits
            // for screen capture; treating it as a huge unsigned number would
            // index far out of the mapping, so refuse it instead.
            if stride <= 0 {
                return;
            }
            let Some(mapped) = data.data() else { return };
            let Some(plane) = mapped.get(offset..offset + size) else {
                return;
            };
            let Some(rgba) = to_rgba(format, stride as usize, plane) else {
                return;
            };
            let image = Image::from_rgba(format.width, format.height, rgba);
            *LATEST.lock().unwrap_or_else(|e| e.into_inner()) = Some(image);
        })
        .register()
        .map_err(|e| format!("screencast: cannot register stream callbacks: {e}"))?;

    let values = enum_format();
    let mut params = [spa::pod::Pod::from_bytes(&values)
        .ok_or_else(|| "screencast: malformed format pod".to_string())?];
    stream
        .connect(
            spa::utils::Direction::Input,
            Some(node_id),
            pw::stream::StreamFlags::AUTOCONNECT | pw::stream::StreamFlags::MAP_BUFFERS,
            &mut params,
        )
        .map_err(|e| format!("screencast: cannot connect stream: {e}"))?;

    // Runs until the process exits. There is no shutdown path on purpose: the
    // stream is a process-wide resource and the loop owning it is what keeps
    // frames flowing.
    mainloop.run();
    Ok(())
}

/// The `EnumFormat` pod offered to the compositor: raw video in any of the four
/// packed 32-bit orders [`PixelOrder`] can repack.
///
/// The size and framerate are wide ranges rather than fixed values because the
/// compositor knows the monitor's geometry and refresh rate and this code does
/// not; pinning them would make negotiation fail on any screen that disagrees.
fn enum_format() -> Vec<u8> {
    use spa::param::format::{FormatProperties, MediaSubtype, MediaType};
    use spa::param::video::VideoFormat;
    use spa::utils::{Fraction, Rectangle};

    let object = spa::pod::object!(
        spa::utils::SpaTypes::ObjectParamFormat,
        spa::param::ParamType::EnumFormat,
        spa::pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        spa::pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        spa::pod::property!(
            FormatProperties::VideoFormat,
            Choice,
            Enum,
            Id,
            VideoFormat::BGRx,
            VideoFormat::BGRx,
            VideoFormat::BGRA,
            VideoFormat::RGBx,
            VideoFormat::RGBA,
        ),
        spa::pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle {
                width: 1920,
                height: 1080
            },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 16384,
                height: 16384
            }
        ),
        spa::pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: 30, denom: 1 },
            Fraction { num: 0, denom: 1 },
            Fraction {
                num: 1000,
                denom: 1
            }
        ),
    );

    spa::pod::serialize::PodSerializer::serialize(
        std::io::Cursor::new(Vec::new()),
        &spa::pod::Value::Object(object),
    )
    .expect("serializing a fixed pod into a Vec cannot fail")
    .0
    .into_inner()
}

/// The negotiated format, or `None` until `param_changed` has seen one this
/// code can repack.
type Negotiated = Option<Format>;

#[derive(Clone, Copy)]
struct Format {
    order: PixelOrder,
    width: u32,
    height: u32,
}

/// The byte order of a packed 32-bit pixel, which is the only thing that
/// distinguishes the formats accepted here.
///
/// The four SPA formats collapse to two cases: whether the red and blue bytes
/// are swapped relative to [`Image`]'s RGBA. The `x` variants differ from the
/// `A` variants only in whether the fourth byte means anything, and since the
/// conversion forces it opaque either way, they need no separate handling.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum PixelOrder {
    /// Bytes in memory are B, G, R, then alpha or padding.
    Bgra,
    /// Bytes in memory are R, G, B, then alpha or padding.
    Rgba,
}

impl PixelOrder {
    fn from_spa(format: spa::param::video::VideoFormat) -> Option<Self> {
        use spa::param::video::VideoFormat;
        match format {
            VideoFormat::BGRx | VideoFormat::BGRA => Some(Self::Bgra),
            VideoFormat::RGBx | VideoFormat::RGBA => Some(Self::Rgba),
            _ => None,
        }
    }
}

/// Repacks one PipeWire plane into the tight RGBA8 buffer [`Image`] requires.
///
/// Two traps, both of which corrupt silently rather than fail: a compositor's
/// stride is aligned (a 4K frame commonly arrives with 15360 usable bytes per
/// row inside a wider allocation), so keeping the padding skews every row after
/// the first; and BGRx is the format nearly every Wayland compositor offers
/// first, so copying bytes through unswapped tints the whole capture blue.
///
/// Returns `None` when the plane is shorter than the geometry claims, which is
/// the one case where producing an [`Image`] would panic on the length
/// invariant. A truncated buffer is better dropped -- the next frame is
/// milliseconds away.
///
/// The fourth channel is forced opaque rather than copied: in the `x` formats
/// that byte is undefined and is usually zero, and trusting it renders the whole
/// capture invisible. A screen has no transparency worth preserving anyway.
fn to_rgba(format: Format, stride: usize, plane: &[u8]) -> Option<Vec<u8>> {
    let row_bytes = format.width as usize * 4;
    if stride < row_bytes {
        return None;
    }
    let height = format.height as usize;
    // The last row need only be `row_bytes` long: a compositor is free to stop
    // the mapping at the final pixel instead of padding past it.
    if height == 0 || plane.len() < stride * (height - 1) + row_bytes {
        return None;
    }

    let mut out = Vec::with_capacity(row_bytes * height);
    for y in 0..height {
        let row = &plane[y * stride..y * stride + row_bytes];
        match format.order {
            PixelOrder::Bgra => {
                for px in row.as_chunks::<4>().0 {
                    out.extend_from_slice(&[px[2], px[1], px[0], 0xFF]);
                }
            }
            PixelOrder::Rgba => {
                for px in row.as_chunks::<4>().0 {
                    out.extend_from_slice(&[px[0], px[1], px[2], 0xFF]);
                }
            }
        }
    }
    Some(out)
}

/// Where the restore token lives.
///
/// The token is a compositor grant, not configuration and not a cache: losing it
/// costs one consent dialog and nothing else, which is exactly what the state
/// directory is for.
static TOKEN_PATH: LazyLock<PathBuf> =
    LazyLock::new(|| token_path_in(std::env::var_os("XDG_STATE_HOME"), std::env::var_os("HOME")));

fn token_path() -> &'static Path {
    &TOKEN_PATH
}

/// Resolves the token path from the environment, per the XDG base directory
/// spec: `XDG_STATE_HOME` wins, `$HOME/.local/state` is the defined fallback.
///
/// An unset or relative `XDG_STATE_HOME` must be ignored rather than used, and a
/// process with neither variable gets a path under the temporary directory: a
/// token that vanishes on reboot still saves every prompt until then, and having
/// no path at all would mean special-casing "cannot persist" through every
/// caller for no gain.
fn token_path_in(xdg_state_home: Option<OsString>, home: Option<OsString>) -> PathBuf {
    let base = xdg_state_home
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            home.map(PathBuf::from)
                .filter(|p| p.is_absolute())
                .map(|home| home.join(".local/state"))
        })
        .unwrap_or_else(std::env::temp_dir);
    base.join("zeughaus").join("screencast_token")
}

/// Reads a previously stored token. Any failure means "no token": an unreadable
/// or empty file is indistinguishable in effect from a first run.
fn read_token(path: &Path) -> Option<String> {
    let token = std::fs::read_to_string(path).ok()?;
    let token = token.trim();
    (!token.is_empty()).then(|| token.to_string())
}

/// Stores a token, best effort. A read-only state directory costs a consent
/// dialog per launch, which must not fail the capture that just succeeded.
///
/// Written through a temporary file in the same directory and renamed, so a
/// crash mid-write cannot leave a truncated token behind -- that would be
/// silently rejected later and prompt the user with no explanation.
fn write_token(path: &Path, token: &str) {
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    let temp = path.with_extension("tmp");
    let written = std::fs::File::create(&temp).and_then(|mut f| f.write_all(token.as_bytes()));
    if written.is_err() || std::fs::rename(&temp, path).is_err() {
        let _ = std::fs::remove_file(&temp);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_refusal_implicates_the_restore_token() {
        // A refusal is the portal saying no to this request, which a dead token
        // can cause -- so it is worth discarding and prompting afresh.
        assert!(
            HandshakeError::refused("screencast: denied or cancelled: Cancelled".to_string())
                .implicates_token()
        );
        // These are the portal being absent, slow, or unreachable. Discarding a
        // good token here would cost the user a consent dialog for an outage
        // that had nothing to do with their grant -- which is exactly the
        // regression this predicate exists to prevent.
        for message in [
            "screencast: no ScreenCast portal: I/O error",
            "screencast: cannot create session: I/O error",
            "screencast: cannot select sources: I/O error",
            "screencast: cannot start session: I/O error",
            "screencast: no response within 60 s",
            "screencast: cannot open pipewire remote: I/O error",
        ] {
            assert!(
                !HandshakeError::unreachable(message.to_string()).implicates_token(),
                "{message} must not discard the token"
            );
        }
    }

    #[test]
    fn handshake_error_keeps_its_message_verbatim() {
        // The message is what reaches the node's `error` pin, so neither
        // constructor may decorate it.
        let refused = HandshakeError::refused("screencast: denied or cancelled: x".to_string());
        assert_eq!(refused.message, "screencast: denied or cancelled: x");
        let unreachable = HandshakeError::unreachable("screencast: no portal".to_string());
        assert_eq!(unreachable.message, "screencast: no portal");
    }

    fn format(order: PixelOrder, width: u32, height: u32) -> Format {
        Format {
            order,
            width,
            height,
        }
    }

    #[test]
    fn bgra_swaps_red_and_blue_and_forces_opacity() {
        // 2x2 with 4 bytes of row padding: stride 12, row_bytes 8. The source
        // alpha bytes (4, 8, 12, 16) must not survive -- a BGRx compositor
        // leaves them zero and the capture would render invisible.
        let plane = vec![
            1, 2, 3, 4, 5, 6, 7, 8, 0xDE, 0xAD, 0xBE, 0xEF, // row 0 + padding
            9, 10, 11, 12, 13, 14, 15, 16, 0xDE, 0xAD, 0xBE, 0xEF, // row 1 + padding
        ];
        let out = to_rgba(format(PixelOrder::Bgra, 2, 2), 12, &plane).unwrap();
        assert_eq!(
            out,
            vec![3, 2, 1, 0xFF, 7, 6, 5, 0xFF, 11, 10, 9, 0xFF, 15, 14, 13, 0xFF]
        );
    }

    #[test]
    fn rgba_keeps_channel_order_and_forces_opacity() {
        let plane = vec![
            1, 2, 3, 4, 5, 6, 7, 8, 0xDE, 0xAD, 0xBE, 0xEF, // row 0 + padding
            9, 10, 11, 12, 13, 14, 15, 16, 0xDE, 0xAD, 0xBE, 0xEF, // row 1 + padding
        ];
        let out = to_rgba(format(PixelOrder::Rgba, 2, 2), 12, &plane).unwrap();
        assert_eq!(
            out,
            vec![1, 2, 3, 0xFF, 5, 6, 7, 0xFF, 9, 10, 11, 0xFF, 13, 14, 15, 0xFF]
        );
    }

    #[test]
    fn every_accepted_spa_format_maps_to_an_order() {
        use spa::param::video::VideoFormat;
        assert_eq!(
            PixelOrder::from_spa(VideoFormat::BGRx),
            Some(PixelOrder::Bgra)
        );
        assert_eq!(
            PixelOrder::from_spa(VideoFormat::BGRA),
            Some(PixelOrder::Bgra)
        );
        assert_eq!(
            PixelOrder::from_spa(VideoFormat::RGBx),
            Some(PixelOrder::Rgba)
        );
        assert_eq!(
            PixelOrder::from_spa(VideoFormat::RGBA),
            Some(PixelOrder::Rgba)
        );
        // Planar YUV is negotiable in general but is not offered by
        // `enum_format`, so a buffer in it must never be repacked as if it were
        // packed RGB.
        assert_eq!(PixelOrder::from_spa(VideoFormat::I420), None);
    }

    #[test]
    fn output_length_matches_the_image_invariant() {
        let plane = vec![0u8; 3 * 16];
        let out = to_rgba(format(PixelOrder::Bgra, 2, 3), 16, &plane).unwrap();
        // Panics if the packing is wrong.
        let image = Image::from_rgba(2, 3, out);
        assert_eq!((image.width(), image.height()), (2, 3));
    }

    #[test]
    fn last_row_may_stop_at_the_final_pixel() {
        // stride 16, 3 rows, but the mapping ends after row 2's 8 usable bytes.
        let plane = vec![0u8; 16 * 2 + 8];
        assert!(to_rgba(format(PixelOrder::Bgra, 2, 3), 16, &plane).is_some());
    }

    #[test]
    fn short_plane_and_short_stride_are_refused() {
        let plane = vec![0u8; 16 * 2 + 7];
        assert!(to_rgba(format(PixelOrder::Bgra, 2, 3), 16, &plane).is_none());
        // stride below width * 4 cannot describe the claimed geometry.
        let plane = vec![0u8; 64];
        assert!(to_rgba(format(PixelOrder::Bgra, 4, 4), 12, &plane).is_none());
    }

    #[test]
    fn token_round_trips_through_the_file() {
        let dir = std::env::temp_dir().join(format!(
            "zeughaus-screencast-token-{}-{}",
            std::process::id(),
            line!()
        ));
        let path = dir.join("screencast_token");

        assert_eq!(read_token(&path), None, "no file means no token");

        write_token(&path, "restore-token-abc123");
        assert_eq!(read_token(&path), Some("restore-token-abc123".to_string()));

        // The portal mints a fresh token on every start, so overwriting must
        // replace rather than append.
        write_token(&path, "restore-token-def456");
        assert_eq!(read_token(&path), Some("restore-token-def456".to_string()));

        // A rejected token is removed, and that must read back as a first run
        // rather than as an error.
        std::fs::remove_file(&path).unwrap();
        assert_eq!(read_token(&path), None);

        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn empty_token_file_reads_as_no_token() {
        let dir = std::env::temp_dir().join(format!(
            "zeughaus-screencast-token-{}-{}",
            std::process::id(),
            line!()
        ));
        let path = dir.join("screencast_token");
        write_token(&path, "");
        assert!(path.exists());
        assert_eq!(read_token(&path), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn token_path_prefers_xdg_state_home() {
        assert_eq!(
            token_path_in(Some("/run/state".into()), Some("/home/u".into())),
            PathBuf::from("/run/state/zeughaus/screencast_token")
        );
    }

    #[test]
    fn token_path_falls_back_to_home_local_state() {
        assert_eq!(
            token_path_in(None, Some("/home/u".into())),
            PathBuf::from("/home/u/.local/state/zeughaus/screencast_token")
        );
        // A relative XDG_STATE_HOME is invalid per the spec and must be ignored,
        // not joined onto the current directory.
        assert_eq!(
            token_path_in(Some("relative".into()), Some("/home/u".into())),
            PathBuf::from("/home/u/.local/state/zeughaus/screencast_token")
        );
    }

    #[test]
    fn token_path_without_home_stays_absolute() {
        let path = token_path_in(None, None);
        assert!(path.is_absolute());
        assert!(path.ends_with("zeughaus/screencast_token"));
    }
}
