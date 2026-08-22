//! Wayland screen capture through xdg-desktop-portal.
//!
//! `scrap` reads the X11 root window. Under a Wayland compositor that window
//! exists (XWayland provides it) but holds no Wayland content, so scrap returns
//! a correctly sized, entirely black frame -- a silent failure that looks like a
//! working capture. The portal is the only interface a Wayland compositor
//! exposes for this, so a Wayland session must go through it.
//!
//! This uses the portal's `Screenshot` interface rather than `ScreenCast`:
//! one request, one image, which is exactly the node's "capture once per
//! execution" contract. ScreenCast would add a PipeWire client and a stream
//! negotiation for a frame rate nothing consumes yet; it is the right backend
//! once a capture node streams.
//!
//! The portal writes the screenshot to a file (KDE puts it in the user's
//! Pictures directory) and returns its URI, so this reads the file and then
//! deletes it: a graph that captures on a timer would otherwise fill that
//! directory with thousands of PNGs.
//!
//! Cost, measured on a 3840x2160 screen: about 2 s per capture, of which the
//! portal round trip is ~0.35 s and the rest is PNG decoding. The PNG is the
//! portal's own format choice, so the only way past it is the ScreenCast path
//! with its raw PipeWire buffers.

use std::path::{Path, PathBuf};
use std::sync::{LazyLock, Mutex};
use std::time::Duration;

use zeughaus_core::Image;

/// How long a single portal round trip may take before it is reported as a
/// failure. A local request answers in roughly 300 ms; the timeout exists
/// because a wedged portal would otherwise leave the node pending forever, with
/// its whole downstream blocked and no error anywhere.
const PORTAL_TIMEOUT: Duration = Duration::from_secs(10);

/// The one runtime every portal request in this crate shares, for the lifetime
/// of the process.
///
/// This must NOT be a runtime per capture. `ashpd` caches its D-Bus connection
/// process-wide, so the connection belongs to whichever runtime created it:
/// dropping that runtime leaves the cached connection with nothing driving it,
/// and the next request waits for a response that can never arrive. Verified
/// the hard way -- a runtime per call captures once and then hangs forever.
///
/// One worker thread, not the current-thread flavour: several capture nodes in
/// one graph run their deferred work on separate blocking threads, and
/// concurrent `block_on` calls on a current-thread runtime fight over who
/// drives the scheduler. A worker thread drives the connection regardless of
/// who is waiting.
///
/// [`crate::screencast`] drives its handshake on this same runtime rather than
/// building its own, for the reason above: whichever backend talks to the portal
/// first creates the cached connection, and a second runtime would leave the
/// other backend's requests unanswerable.
static RUNTIME: LazyLock<std::io::Result<tokio::runtime::Runtime>> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .worker_threads(1)
        .enable_all()
        .build()
});

/// Serializes the requests themselves. A screen is a single resource: two nodes
/// capturing in the same pass mean two screenshots, one after the other, which
/// is also what keeps the portal's own dialog and permission handling sane.
static SERIAL: Mutex<()> = Mutex::new(());

pub(crate) fn runtime() -> Result<&'static tokio::runtime::Runtime, String> {
    RUNTIME
        .as_ref()
        .map_err(|e| format!("portal: no runtime: {e}"))
}

/// Captures the whole screen. `Err` carries a message for the node's `error`
/// pin, never a panic: a missing portal is a normal environment, not a bug.
pub fn capture() -> Result<Image, String> {
    // Poisoning carries no state here (the guard protects ordering, not data),
    // so a panicking capture must not disable every later one.
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let path = runtime()?.block_on(request_screenshot())?;
    let decoded = decode(&path);
    // Best effort: the file is ours (this request created it), but failing to
    // remove it must not fail the capture.
    let _ = std::fs::remove_file(&path);
    decoded
}

/// Asks the portal for a screenshot and resolves the response URI to a path.
///
/// `interactive(false)` skips the customization dialog; the portal still
/// enforces its own permission for that, and a denial surfaces as an error
/// here rather than as a hang.
async fn request_screenshot() -> Result<PathBuf, String> {
    let request = async {
        ashpd::desktop::screenshot::Screenshot::request()
            .interactive(false)
            .modal(false)
            .send()
            .await
            .map_err(|e| format!("portal: request failed: {e}"))?
            .response()
            .map_err(|e| format!("portal: denied or cancelled: {e}"))
    };
    let response = tokio::time::timeout(PORTAL_TIMEOUT, request)
        .await
        .map_err(|_| {
            format!(
                "portal: no response within {}s",
                PORTAL_TIMEOUT.as_secs()
            )
        })??;
    file_uri_to_path(response.uri().as_str())
}

/// Turns the portal's `file://` URI into a path.
///
/// `ashpd::Uri` is a validated string, not a parsed URL, so the percent
/// decoding is on us -- a Pictures directory containing a space arrives as
/// `%20` and would otherwise be a "file not found".
fn file_uri_to_path(uri: &str) -> Result<PathBuf, String> {
    let Some(encoded) = uri.strip_prefix("file://") else {
        return Err(format!("portal: expected a file uri, got '{uri}'"));
    };
    let decoded = percent_encoding::percent_decode_str(encoded)
        .decode_utf8()
        .map_err(|e| format!("portal: uri is not utf-8: {e}"))?;
    Ok(PathBuf::from(decoded.as_ref()))
}

/// Decodes the portal's PNG into the tight RGBA8 buffer [`Image`] requires.
fn decode(path: &Path) -> Result<Image, String> {
    let rgba = image::open(path)
        .map_err(|e| format!("portal: cannot read {}: {e}", path.display()))?
        .into_rgba8();
    let (width, height) = rgba.dimensions();
    Ok(Image::from_rgba(width, height, rgba.into_raw()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_uri_decodes_percent_escapes() {
        let path = file_uri_to_path("file:///home/u/My%20Pictures/shot%201.png").unwrap();
        assert_eq!(path, PathBuf::from("/home/u/My Pictures/shot 1.png"));
    }

    #[test]
    fn plain_file_uri_maps_straight_to_a_path() {
        let path = file_uri_to_path("file:///tmp/shot.png").unwrap();
        assert_eq!(path, PathBuf::from("/tmp/shot.png"));
    }

    /// A portal that hands back a document-store or http uri is a case this
    /// backend does not handle, and must say so instead of building a nonsense
    /// path.
    #[test]
    fn a_non_file_uri_is_rejected() {
        assert!(file_uri_to_path("https://example.invalid/shot.png").is_err());
    }
}
