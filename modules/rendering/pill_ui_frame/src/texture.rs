//! The versioned texture set a UI frame carries, so a backend that was
//! rebuilt mid-run re-uploads what it lacks instead of drawing nothing.
//!
//! # Responsibilities
//!
//! - [`UiTextureSet`]: every texture a frame's draw lists sample, each with a
//!   monotonically increasing revision, plus the textures to free.
//! - [`UiTexture`]: one texture's size, sampling and whole image at this
//!   revision, as owned plain data.
//!
//! # Design
//!
//! The revision is the reload rule: a painter keeps `texture -> revision` for
//! what it has applied and uploads whenever the carried revision is not the
//! applied one, so a painter that was rebuilt - and therefore remembers
//! nothing - uploads everything the set carries. That only works if every
//! entry can *rebuild* its texture, so an entry always carries the whole
//! image: one full-image copy per changed texture per frame, with nothing
//! replayed from history, so no back-channel and no cadence is involved.
//!
//! Every buffer is an [`Arc`] allocated by the producing side's own code and
//! handed over as plain data: a project reload may drop the producer while a
//! renderer still holds the frame, so the bytes must be owned by the data,
//! not borrowed from the producer.

// Standard library
use std::sync::Arc;

// Current crate
use crate::draw::TextureId;

// =============================================================================
// UiTextureFilter
// =============================================================================

/// How a backend samples a UI texture.
#[derive(Copy, Clone, Debug, Default, PartialEq, Eq)]
pub enum UiTextureFilter {
    /// One texel per pixel, sharp edges: glyph atlases and pixel art.
    #[default]
    Nearest,
    /// Smooth in between: photo-like textures and scaled drawings.
    Linear,
}

// =============================================================================
// UiTexture
// =============================================================================

/// One texture a frame's draw lists sample, at one revision.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UiTexture {
    /// Which draws sample it.
    pub id: TextureId,
    /// The image's width in texels.
    pub width: u32,
    /// The image's height in texels.
    pub height: u32,
    /// The content revision this entry carries; it increases whenever the
    /// texture's content changes.
    pub revision: u64,
    /// The whole image: tightly packed straight-alpha sRGB RGBA8 rows,
    /// `width * height * 4` bytes.
    pub pixels: Arc<[u8]>,
    /// How the texture is sampled.
    pub filter: UiTextureFilter,
}

// =============================================================================
// UiTextureSet
// =============================================================================

/// A frame's textures: what to carry to the backend, and what to free.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UiTextureSet {
    /// The textures this frame's draws use, one entry per texture.
    pub textures: Vec<UiTexture>,
    /// Textures the producer no longer uses; a backend drops them.
    pub frees: Vec<TextureId>,
}

impl UiTextureSet {
    /// An empty set.
    pub fn new() -> UiTextureSet {
        UiTextureSet::default()
    }

    /// Whether there is nothing to upload and nothing to free.
    pub fn is_empty(&self) -> bool {
        self.textures.is_empty() && self.frees.is_empty()
    }

    /// Forget the previous frame's set, keeping the allocations.
    pub fn clear(&mut self) {
        self.textures.clear();
        self.frees.clear();
    }

    /// Add the whole image of `id` at `revision`.
    ///
    /// `pixels` is straight-alpha sRGB RGBA8, `width * height * 4` bytes; a
    /// shorter or longer slice is a producer bug the backend would otherwise
    /// read past, so it is refused here rather than at the upload.
    pub fn push_full(
        &mut self,
        id: TextureId,
        width: u32,
        height: u32,
        revision: u64,
        filter: UiTextureFilter,
        pixels: Arc<[u8]>,
    ) {
        assert_eq!(
            pixels.len(),
            width as usize * height as usize * 4,
            "a whole-texture upload needs exactly width * height * 4 bytes"
        );
        self.textures.push(UiTexture {
            id,
            width,
            height,
            revision,
            pixels,
            filter,
        });
    }

    /// Free `id` in every backend that applied the set.
    pub fn push_free(&mut self, id: TextureId) {
        self.frees.push(id);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_full_upload_carries_the_image_and_its_revision() {
        let mut set = UiTextureSet::new();
        set.push_full(
            TextureId(4),
            2,
            2,
            7,
            UiTextureFilter::Nearest,
            Arc::from(vec![255u8; 16]),
        );
        let texture = &set.textures[0];
        assert_eq!(texture.pixels.len(), 16);
        assert_eq!((texture.id, texture.revision), (TextureId(4), 7));
    }

    #[test]
    fn a_wrongly_sized_upload_is_refused() {
        let outcome = std::panic::catch_unwind(|| {
            let mut set = UiTextureSet::new();
            set.push_full(
                TextureId(1),
                2,
                2,
                1,
                UiTextureFilter::Nearest,
                Arc::from(vec![0u8; 8]),
            );
        });
        assert!(outcome.is_err(), "a 2x2 image is 16 bytes, not 8");
    }

    #[test]
    fn freeing_a_texture_empties_the_set_only_with_no_uploads() {
        let mut set = UiTextureSet::new();
        set.push_free(TextureId(2));
        assert!(!set.is_empty());
        assert!(set.textures.is_empty());
        set.clear();
        assert!(set.is_empty());
    }
}
