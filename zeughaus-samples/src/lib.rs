//! The sample feed: how a scaled video signal leaves the runtime and reaches a
//! viewer.
//!
//! Graph state travels through SpacetimeDB, but a frame does not: a 3840x2160
//! RGBA frame is 33 MB, and a state store is the wrong pipe for it. Frames
//! travel over their own connection instead, and this crate is the wire format
//! both ends speak -- pure data and pure pixel math, no I/O, so the runtime's
//! server and the editor's client cannot disagree about the protocol.
//!
//! # Shape
//!
//! One feed is one long-lived exchange: the viewer sends a [`FeedRequest`] once,
//! naming the node, the pin and the size it will actually draw, and the runtime
//! then writes [`FrameHeader`]-prefixed frames until the viewer stops reading.
//!
//! That is a standing request rather than one request per repaint, because a
//! video signal at display rate would otherwise pay a round trip per frame --
//! bearable on a LAN, ruinous across the internet. It stays a *pull* in the way
//! that matters: the viewer states the terms, the viewer cancels, and a viewer
//! that reads slowly gets fewer frames rather than a growing backlog, because
//! the writer only ever sends what is current when the stream has room.
//!
//! # Why the runtime scales
//!
//! The runtime holds the frame, so it is the side that can cheaply produce the
//! ~0.5 MB a node body actually draws instead of shipping 33 MB for it. A local
//! and a remote viewer are the same case with the same request; nothing about
//! this path knows the difference.
//!
//! Sizes snap to a [`ladder`] of tiers so two viewers of similar size share one
//! scaled result instead of each paying for their own.

use serde::{Deserialize, Serialize};
use zeughaus_core::Image;

/// What a viewer asks for: one node's output pin, at the size it will draw.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FeedRequest {
    pub node_id: u64,
    pub pin: String,
    /// The width the viewer will draw. Snapped up to a [`ladder`] tier by the
    /// runtime; `0` means "no limit", i.e. the frame at its own resolution.
    pub width: u32,
    /// The height the viewer will draw. Same rules as `width`.
    pub height: u32,
    /// Upper bound on frames per second. The runtime may send fewer -- the
    /// source may be slower, or the stream may be full -- but never more, so a
    /// viewer can ask for a preview rate without watching a 60 Hz source at
    /// 60 Hz.
    pub max_fps: u16,
}

impl FeedRequest {
    /// A feed of `pin` on `node_id`, drawn at `width` x `height`.
    pub fn new(node_id: u64, pin: impl Into<String>, width: u32, height: u32) -> Self {
        Self {
            node_id,
            pin: pin.into(),
            width,
            height,
            max_fps: 30,
        }
    }

    pub fn with_max_fps(mut self, max_fps: u16) -> Self {
        self.max_fps = max_fps;
        self
    }

    /// The frame interval this request asks for. `None` when unthrottled.
    pub fn frame_interval(&self) -> Option<std::time::Duration> {
        (self.max_fps > 0)
            .then(|| std::time::Duration::from_micros(1_000_000 / u64::from(self.max_fps)))
    }

    /// Encodes the request. One JSON line, because it is sent once per feed and
    /// being able to read it in a packet dump is worth more than the bytes.
    pub fn encode(&self) -> Vec<u8> {
        let mut out = serde_json::to_vec(self).unwrap_or_else(|_| b"{}".to_vec());
        out.push(b'\n');
        out
    }

    /// Decodes a request. `None` for anything malformed: the bytes come from
    /// another process and a viewer must not be able to panic the runtime.
    pub fn decode(bytes: &[u8]) -> Option<Self> {
        let line = bytes.split(|b| *b == b'\n').next()?;
        serde_json::from_slice(line).ok()
    }
}

/// Fixed-size prefix in front of every frame's pixels.
///
/// Fixed and binary, unlike the request: this one is parsed per frame, and the
/// stream is already typed by the endpoint it arrived on, so there is nothing
/// left to negotiate per frame.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FrameHeader {
    /// Monotonic per feed. A viewer that reconnects cannot assume it continues
    /// where it left off, only that later frames carry larger numbers.
    pub seq: u64,
    pub width: u32,
    pub height: u32,
}

impl FrameHeader {
    /// Wire size of the header.
    pub const LEN: usize = 16;

    pub fn new(seq: u64, width: u32, height: u32) -> Self {
        Self { seq, width, height }
    }

    /// Bytes of RGBA payload that follow this header.
    pub fn payload_len(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    pub fn encode(&self) -> [u8; Self::LEN] {
        let mut out = [0u8; Self::LEN];
        out[0..8].copy_from_slice(&self.seq.to_le_bytes());
        out[8..12].copy_from_slice(&self.width.to_le_bytes());
        out[12..16].copy_from_slice(&self.height.to_le_bytes());
        out
    }

    /// Reads a header. `None` when the geometry could not describe a frame --
    /// a zero dimension, or a payload that would not fit in memory. Refusing
    /// here is what keeps a hostile sender from making the reader allocate on
    /// its behalf.
    pub fn decode(bytes: &[u8; Self::LEN]) -> Option<Self> {
        let header = Self {
            seq: u64::from_le_bytes(bytes[0..8].try_into().ok()?),
            width: u32::from_le_bytes(bytes[8..12].try_into().ok()?),
            height: u32::from_le_bytes(bytes[12..16].try_into().ok()?),
        };
        let plausible = header.width > 0
            && header.height > 0
            && header.width <= MAX_DIMENSION
            && header.height <= MAX_DIMENSION;
        plausible.then_some(header)
    }
}

/// Largest frame edge this protocol carries. Well past 8K, and small enough
/// that `width * height * 4` cannot overflow a `usize` on a 32-bit viewer.
pub const MAX_DIMENSION: u32 = 16384;

/// Frame heights a feed is served at.
///
/// A tier ladder rather than the viewer's exact size: two viewers drawing a node
/// at 190 and 210 pixels tall both get the 240-line version, so the runtime
/// scales once and sends the same bytes twice. Exact sizes would mean a
/// separate scale per viewer for a difference nobody can see at preview size.
pub mod ladder {
    /// The tiers, in ascending order. Heights, because video is described that
    /// way and the aspect ratio comes from the source.
    pub const TIERS: [u32; 5] = [240, 360, 480, 720, 1080];

    /// The tier a viewer asking for `height` is served at, or `None` for "send
    /// it at source resolution" -- either because the viewer asked for no limit
    /// (`0`) or because it wants more than the largest tier.
    pub fn tier_for(height: u32) -> Option<u32> {
        if height == 0 {
            return None;
        }
        TIERS.iter().copied().find(|tier| *tier >= height)
    }
}

/// Scales `frame` so it fits inside `max_width` x `max_height`, preserving
/// aspect ratio. Returns the frame unchanged when it already fits.
///
/// Box-averaging, not nearest-neighbour: a 4K desktop reduced to a node body is
/// an 8:1 reduction, and point sampling turns text into noise. Samples per
/// output pixel are capped ([`MAX_SAMPLES_PER_AXIS`]) so the cost stays
/// proportional to the *output* size -- otherwise a 480x270 preview of a 4K
/// frame would read all 33 MB, which is the work this exists to avoid.
pub fn scale_to_fit(frame: &Image, max_width: u32, max_height: u32) -> Image {
    let (src_w, src_h) = (frame.width(), frame.height());
    if max_width == 0 || max_height == 0 || (src_w <= max_width && src_h <= max_height) {
        return frame.clone();
    }
    // One ratio for both axes keeps the aspect; the smaller ratio is the one
    // that fits.
    let ratio = f64::min(
        f64::from(max_width) / f64::from(src_w),
        f64::from(max_height) / f64::from(src_h),
    );
    let dst_w = ((f64::from(src_w) * ratio).round() as u32).max(1);
    let dst_h = ((f64::from(src_h) * ratio).round() as u32).max(1);
    Image::from_rgba(dst_w, dst_h, resample(frame, dst_w, dst_h))
}

/// Upper bound on source samples per output pixel and axis. Sixteen samples per
/// output pixel is enough to stop aliasing from dominating; more is spending
/// bandwidth on a preview.
pub const MAX_SAMPLES_PER_AXIS: u32 = 4;

fn resample(frame: &Image, dst_w: u32, dst_h: u32) -> Vec<u8> {
    let (src_w, src_h) = (frame.width(), frame.height());
    let src = frame.rgba();
    let mut out = Vec::with_capacity(dst_w as usize * dst_h as usize * 4);
    // How many source pixels one output pixel covers, capped.
    let step_x = (src_w / dst_w).clamp(1, MAX_SAMPLES_PER_AXIS);
    let step_y = (src_h / dst_h).clamp(1, MAX_SAMPLES_PER_AXIS);
    let samples = u32::from(step_x as u16) * u32::from(step_y as u16);

    for y in 0..dst_h {
        // Top-left source pixel of this output pixel's box.
        let src_y0 = (y as u64 * src_h as u64 / dst_h as u64) as u32;
        for x in 0..dst_w {
            let src_x0 = (x as u64 * src_w as u64 / dst_w as u64) as u32;
            let mut acc = [0u32; 4];
            for sy in 0..step_y {
                let py = (src_y0 + sy).min(src_h - 1);
                for sx in 0..step_x {
                    let px = (src_x0 + sx).min(src_w - 1);
                    let i = (py as usize * src_w as usize + px as usize) * 4;
                    acc[0] += u32::from(src[i]);
                    acc[1] += u32::from(src[i + 1]);
                    acc[2] += u32::from(src[i + 2]);
                    acc[3] += u32::from(src[i + 3]);
                }
            }
            out.push((acc[0] / samples) as u8);
            out.push((acc[1] / samples) as u8);
            out.push((acc[2] / samples) as u8);
            out.push((acc[3] / samples) as u8);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Image {
        let pixels = rgba.repeat(width as usize * height as usize);
        Image::from_rgba(width, height, pixels)
    }

    #[test]
    fn request_round_trips() {
        let request = FeedRequest::new(7, "frame", 480, 270).with_max_fps(15);
        let decoded = FeedRequest::decode(&request.encode()).unwrap();
        assert_eq!(decoded, request);
    }

    /// The bytes come from another process, so every malformed shape has to be a
    /// `None` rather than a panic.
    #[test]
    fn malformed_request_is_rejected() {
        assert!(FeedRequest::decode(b"").is_none());
        assert!(FeedRequest::decode(b"not json\n").is_none());
        assert!(FeedRequest::decode(b"{\"node_id\":1}\n").is_none());
    }

    #[test]
    fn frame_header_round_trips() {
        let header = FrameHeader::new(42, 480, 270);
        let bytes = header.encode();
        assert_eq!(bytes.len(), FrameHeader::LEN);
        assert_eq!(FrameHeader::decode(&bytes), Some(header));
        assert_eq!(header.payload_len(), 480 * 270 * 4);
    }

    #[test]
    fn implausible_geometry_is_refused() {
        let zero = FrameHeader::new(1, 0, 270).encode();
        assert_eq!(FrameHeader::decode(&zero), None);
        let huge = FrameHeader::new(1, MAX_DIMENSION + 1, 270).encode();
        assert_eq!(FrameHeader::decode(&huge), None);
    }

    #[test]
    fn ladder_snaps_up_and_reports_native() {
        assert_eq!(ladder::tier_for(190), Some(240));
        assert_eq!(ladder::tier_for(240), Some(240));
        assert_eq!(ladder::tier_for(241), Some(360));
        // Above the ladder: source resolution, same as asking for no limit.
        assert_eq!(ladder::tier_for(2160), None);
        assert_eq!(ladder::tier_for(0), None);
    }

    /// Two viewers of similar size must land on the same tier -- that sharing is
    /// the whole reason the ladder exists.
    #[test]
    fn nearby_sizes_share_a_tier() {
        assert_eq!(ladder::tier_for(190), ladder::tier_for(210));
    }

    #[test]
    fn scaling_preserves_aspect_ratio() {
        let frame = solid(3840, 2160, [10, 20, 30, 255]);
        let scaled = scale_to_fit(&frame, 480, 480);
        assert_eq!((scaled.width(), scaled.height()), (480, 270));
    }

    #[test]
    fn a_frame_that_fits_is_not_touched() {
        let frame = solid(320, 180, [1, 2, 3, 255]);
        let scaled = scale_to_fit(&frame, 480, 270);
        assert!(std::sync::Arc::ptr_eq(frame.rgba(), scaled.rgba()));
    }

    #[test]
    fn no_limit_returns_the_source() {
        let frame = solid(64, 32, [4, 5, 6, 255]);
        let scaled = scale_to_fit(&frame, 0, 0);
        assert!(std::sync::Arc::ptr_eq(frame.rgba(), scaled.rgba()));
    }

    #[test]
    fn scaling_a_solid_colour_keeps_the_colour() {
        let frame = solid(800, 600, [200, 100, 50, 255]);
        let scaled = scale_to_fit(&frame, 200, 200);
        assert_eq!(&scaled.rgba()[0..4], &[200, 100, 50, 255]);
        assert_eq!(scaled.rgba().len(), scaled.width() as usize * scaled.height() as usize * 4);
    }

    /// Averaging must actually average: a checkerboard reduced 2:1 is uniform
    /// grey, where point sampling would keep one of the two colours.
    #[test]
    fn averaging_mixes_neighbouring_pixels() {
        let mut pixels = Vec::new();
        for y in 0..4u32 {
            for x in 0..4u32 {
                let v = if (x + y) % 2 == 0 { 0 } else { 255 };
                pixels.extend_from_slice(&[v, v, v, 255]);
            }
        }
        let frame = Image::from_rgba(4, 4, pixels);
        let scaled = scale_to_fit(&frame, 2, 2);
        assert_eq!((scaled.width(), scaled.height()), (2, 2));
        for px in scaled.rgba().chunks_exact(4) {
            assert_eq!(px[0], 127, "each output pixel averages two black and two white");
            assert_eq!(px[3], 255);
        }
    }

    #[test]
    fn frame_interval_follows_max_fps() {
        let request = FeedRequest::new(1, "frame", 480, 270).with_max_fps(30);
        assert_eq!(
            request.frame_interval(),
            Some(std::time::Duration::from_micros(33_333))
        );
        assert_eq!(request.with_max_fps(0).frame_interval(), None);
    }
}
