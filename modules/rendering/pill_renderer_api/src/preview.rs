//! Asset previews (W-0049 Stage 11): what an editor asks a renderer to draw
//! off-screen, and the pixels it gets back.
//!
//! # Responsibilities
//!
//! - Name the asset one preview draws ([`PreviewAsset`]) and the request that
//!   carries it ([`PreviewRequest`], [`PreviewRequests`]).
//! - Carry the pixels back ([`PreviewImage`], [`PreviewResults`]).
//!
//! # Design
//!
//! A preview is asked for through the world, like a pick: the editor writes a
//! request into [`PreviewRequests`], the renderer draws one request per frame
//! into its own small target and reads it back between frames, and the
//! frontend moves the pixels into [`PreviewResults`]. Nothing touches the
//! disk, and nothing on the editor side needs a GPU device.

use pill_engine::{Resource, World};

/// The asset one preview draws.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum PreviewAsset {
    /// A 2D texture file, drawn as a quad that keeps its aspect.
    ///
    /// The file is named rather than resolved to an asset: a project brings
    /// textures in under its own names and may not have loaded one at all, so
    /// a guid cannot say which pixels belong to a tile. Decoding the file also
    /// means a re-imported texture shows its new pixels.
    Texture {
        /// The file to decode, as the frontend resolves it (absolute).
        path: String,
    },
    /// A material, drawn on a preview sphere.
    Material {
        /// The asset's guid, in hex.
        guid: String,
    },
    /// A mesh, drawn with the renderer's own default shading.
    ///
    /// Named by its file for the same reason a texture is: the project may
    /// have loaded it under a name of its own, or not at all.
    Mesh {
        /// The file to decode, as the frontend resolves it (absolute).
        path: String,
    },
}

/// One preview asked for.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewRequest {
    /// The id the answer names this request by.
    pub id: u64,
    /// The asset to draw.
    pub asset: PreviewAsset,
}

/// The previews waiting for the renderer.
#[derive(Default)]
pub struct PreviewRequests {
    /// Every request asked for since the last frame.
    pub pending: Vec<PreviewRequest>,
    /// The next id [`Self::request`] hands out.
    next_id: u64,
}

impl PreviewRequests {
    /// Queue one preview of `asset` and return the id its answer carries.
    pub fn request(&mut self, asset: PreviewAsset) -> u64 {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        self.pending.push(PreviewRequest { id, asset });
        id
    }
}

impl Resource for PreviewRequests {
    fn shared_name() -> Option<&'static str> {
        Some("pill_renderer_api::preview::PreviewRequests")
    }
}

/// One preview that came back.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PreviewImage {
    /// The id of the request this answers.
    pub id: u64,
    /// The image's width in texels.
    pub width: u32,
    /// The image's height in texels.
    pub height: u32,
    /// Tightly packed RGBA8 rows, top row first.
    pub rgba: Vec<u8>,
}

impl PreviewImage {
    /// The answer to a request that was taken but could not be drawn: the id,
    /// and no pixels.
    ///
    /// Every taken request is answered. A caller waiting for an answer that
    /// never comes - an undecodable file, a frame that drew nothing, a
    /// renderer that was still busy - waits forever, and an editor that asks
    /// one question at a time stops asking. A refusal says "no preview for
    /// this", so the caller clears its bookkeeping and moves on.
    pub fn refused(id: u64) -> Self {
        Self {
            id,
            width: 0,
            height: 0,
            rgba: Vec::new(),
        }
    }

    /// Whether this answer carries no pixels because the request was refused.
    pub fn is_empty(&self) -> bool {
        self.rgba.is_empty()
    }
}

/// The previews the renderer finished.
#[derive(Default)]
pub struct PreviewResults {
    /// Every answer handed over since the editor last drained them.
    pub results: Vec<PreviewImage>,
}

impl PreviewResults {
    /// Take the answers waiting.
    pub fn take(&mut self) -> Vec<PreviewImage> {
        std::mem::take(&mut self.results)
    }
}

impl Resource for PreviewResults {
    fn shared_name() -> Option<&'static str> {
        Some("pill_renderer_api::preview::PreviewResults")
    }
}

/// Inserts both preview resources, keeping what a reload already put there.
pub fn ensure_preview_resources(world: &mut World) {
    if !world.has_resource::<PreviewRequests>() {
        world.insert_resource(PreviewRequests::default());
    }
    if !world.has_resource::<PreviewResults>() {
        world.insert_resource(PreviewResults::default());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A refused answer names its request and carries no pixels, so a caller's
    /// in-flight guard clears instead of waiting for a preview that will never
    /// come.
    #[test]
    fn a_refused_preview_names_its_request_and_carries_no_pixels() {
        let refused = PreviewImage::refused(7);
        assert_eq!(refused.id, 7);
        assert!(refused.is_empty());
        assert!(!PreviewImage {
            id: 8,
            width: 1,
            height: 1,
            rgba: vec![0, 0, 0, 255],
        }
        .is_empty());
    }
}
