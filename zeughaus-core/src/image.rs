//! The `Image` value type: one decoded RGBA8 frame travelling through the graph.
//!
//! Pixels are kept in an `Arc<[u8]>` rather than a `Vec<u8>` because the
//! executor clones a value once per downstream edge, and frames are big: a 4K
//! frame is ~33 MB, so a `Vec` would turn every fan-out into a 33 MB memcpy.
//! Sharing the buffer makes cloning an `Image` a refcount bump. The buffer is
//! copied exactly once, when it enters the graph via [`Image::from_rgba`].

use std::sync::Arc;

use crate::ty::{Ty, Typed};

/// A decoded image: tightly packed RGBA8, row-major, top row first.
///
/// "Tightly packed" is the invariant of the type: `rgba` is exactly
/// `width * height * 4` bytes with no row padding, so a consumer indexes a
/// pixel arithmetically and never has to carry a stride. Producers that read
/// from a padded source (screen capturers usually do) repack before
/// constructing.
#[derive(Clone)]
pub struct Image {
    width: u32,
    height: u32,
    rgba: Arc<[u8]>,
}

impl Image {
    /// Wraps a tightly packed RGBA8 buffer.
    ///
    /// # Panics
    ///
    /// If `rgba.len()` is not `width * height * 4`. Unlike the fallible lookups
    /// elsewhere in this crate, a length mismatch is not a runtime condition a
    /// caller can react to -- it means the producer computed its stride or its
    /// channel count wrong. Storing it anyway would hand every consumer skewed
    /// pixels far from the actual bug.
    pub fn from_rgba(width: u32, height: u32, rgba: Vec<u8>) -> Self {
        let expected = width as usize * height as usize * 4;
        assert_eq!(
            rgba.len(),
            expected,
            "Image::from_rgba: {width}x{height} RGBA needs {expected} bytes"
        );
        Self {
            width,
            height,
            rgba: rgba.into(),
        }
    }

    pub fn width(&self) -> u32 {
        self.width
    }

    pub fn height(&self) -> u32 {
        self.height
    }

    /// The shared pixel buffer. Handed out as the `Arc` itself, not as a plain
    /// slice, so a consumer that needs to keep the pixels can retain them
    /// without copying.
    pub fn rgba(&self) -> &Arc<[u8]> {
        &self.rgba
    }
}

/// `Image` is a nominal type: an edge only has to agree that it carries an
/// image. It keeps the default [`crate::Repr::Opaque`] because a structural
/// view of it would be millions of byte fields -- there is nothing a generic
/// consumer could usefully do with that, and building it would defeat the
/// whole point of sharing the buffer.
impl Typed for Image {
    fn ty() -> Ty {
        Ty::opaque("image")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ty::Repr;

    #[test]
    fn from_rgba_exposes_geometry_and_pixels() {
        let img = Image::from_rgba(2, 1, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(img.width(), 2);
        assert_eq!(img.height(), 1);
        assert_eq!(&**img.rgba(), &[1, 2, 3, 4, 5, 6, 7, 8]);
    }

    #[test]
    #[should_panic(expected = "needs 16 bytes")]
    fn from_rgba_rejects_mismatched_buffer() {
        Image::from_rgba(2, 2, vec![0; 12]);
    }

    #[test]
    fn clone_shares_the_pixel_buffer() {
        let img = Image::from_rgba(1, 1, vec![9, 9, 9, 9]);
        let copy = img.clone();
        assert!(Arc::ptr_eq(img.rgba(), copy.rgba()));
    }

    #[test]
    fn ty_is_opaque_image() {
        assert_eq!(Ty::of::<Image>(), Ty::opaque("image"));
        assert_eq!(Ty::of::<Image>().to_string(), "image");
    }

    #[test]
    fn repr_stays_opaque() {
        let img = Image::from_rgba(1, 1, vec![0, 0, 0, 255]);
        assert_eq!(img.repr(), Repr::Opaque);
    }
}
