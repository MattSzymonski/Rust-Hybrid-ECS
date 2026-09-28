//! Packed sort key used by the transferred mesh drawer.
//!
//! # Responsibilities
//!
//! - Pack a draw's material order and the shader, material, and mesh it binds
//!   into a single `u64` the render queue sorts with one comparison
//!   ([`compose_render_queue_key`]).
//! - Unpack that key back into the fields the mesh drawer needs to rebuild the
//!   resource handles a queued draw binds ([`decompose_render_queue_key`]).
//! - Carry each frame instance's queue entry: the packed key plus the index of
//!   the instance it draws, so the queue stays cheap to sort and walk
//!   ([`RenderQueueItem`]).
//!
//! # Design
//!
//! Ascending sort order of the packed key is what puts draws that share a
//! shader, material, and mesh next to each other, which is what the mesh
//! drawer batches into single instanced draws. The layout leads with the
//! material order byte so the sort bands a pass by draw order first, then
//! pairs each slot index with the version its handle was issued at, so keys
//! from different incarnations of one slot stay apart.
//!
//! Each slot index and version is packed as a single byte, so a key truncates
//! any version above 255 to its low byte - a known limitation tracked
//! separately. Compose and decompose are exact inverses for values that fit
//! in a byte; beyond it the key aliases.

// Current crate
use crate::resource_handles::{RendererMaterialHandle, RendererMeshHandle, RendererShaderHandle};

/// One queued draw: a packed state key plus the instance it draws.
///
/// The renderer builds one item per frame instance, so a sorted queue
/// describes the whole frame's drawing. The derived ordering compares the key
/// first and falls back to `entity_index`, keeping instances that share state
/// in frame order within their batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RenderQueueItem {
    /// Packed draw key: the material order and the shader, material, and mesh
    /// the draw binds, as packed by [`compose_render_queue_key`].
    pub key: u64,
    /// Index of the instance this item draws, into the frame's instance
    /// vector, where the mesh drawer reads its transform from.
    pub entity_index: u32,
}

/// The fields a [`RenderQueueItem`] key packs, one per byte.
///
/// The mesh drawer decomposes the key of every queued item to rebuild the
/// shader, material, and mesh handles its draw binds. Every field is one byte
/// wide, matching the packed layout.
#[derive(Clone, Copy, Debug)]
pub struct RenderQueueKeyFields {
    /// Material rendering order, recovered from the key's inverted top byte.
    pub order: u8,
    /// Slot index of the shader the draw binds.
    pub shader_index: u8,
    /// Version the shader handle was issued at.
    pub shader_version: u8,
    /// Slot index of the material the draw binds.
    pub material_index: u8,
    /// Version the material handle was issued at.
    pub material_version: u8,
    /// Slot index of the mesh the draw binds.
    pub mesh_index: u8,
    /// Version the mesh handle was issued at.
    pub mesh_version: u8,
}

/// Packs a draw's material order and resource identity into one sortable
/// `u64`, most significant byte first:
///
/// - bits 56-63: material rendering order, stored inverted (`u8::MAX - order`);
/// - bits 48-55: shader slot index;
/// - bits 40-47: version the shader handle was issued at;
/// - bits 32-39: material slot index;
/// - bits 24-31: version the material handle was issued at;
/// - bits 16-23: mesh slot index;
/// - bits 8-15: version the mesh handle was issued at;
/// - bits 0-7: unused, always zero.
///
/// Pairing each slot index with its handle version makes a key name one
/// incarnation of a slot, and the order byte leads the key so the sort bands a
/// pass's draws by material order before it groups resource runs.
///
/// The packing is lossy: every index and version occupies a single byte, so a
/// value above 255 truncates to its low byte and aliases a lower one. This is
/// a known limitation tracked separately, so do not read a key back as a
/// substitute for the handle it was packed from.
pub fn compose_render_queue_key(
    order: u8,
    shader: RendererShaderHandle,
    material: RendererMaterialHandle,
    mesh: RendererMeshHandle,
) -> u64 {
    let shader = shader.data();
    let material = material.data();
    let mesh = mesh.data();
    (u64::from(u8::MAX - order) << 56)
        | (u64::from(shader.index as u8) << 48)
        | (u64::from(shader.version.get() as u8) << 40)
        | (u64::from(material.index as u8) << 32)
        | (u64::from(material.version.get() as u8) << 24)
        | (u64::from(mesh.index as u8) << 16)
        | (u64::from(mesh.version.get() as u8) << 8)
}

/// Recovers the fields [`compose_render_queue_key`] packed into a key.
///
/// The mesh drawer calls this for every queued item to rebuild the resource
/// handles the draw binds. Each field comes back one byte wide, so an index or
/// version that did not fit in a byte returns aliased to its low bits, per the
/// limitation noted on [`compose_render_queue_key`].
pub fn decompose_render_queue_key(key: u64) -> RenderQueueKeyFields {
    RenderQueueKeyFields {
        order: u8::MAX - (key >> 56) as u8,
        shader_index: (key >> 48) as u8,
        shader_version: (key >> 40) as u8,
        material_index: (key >> 32) as u8,
        material_version: (key >> 24) as u8,
        mesh_index: (key >> 16) as u8,
        mesh_version: (key >> 8) as u8,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    #[test]
    fn packed_queue_key_round_trips_all_fields() {
        let shader = RendererShaderHandle::new(3, NonZeroU32::new(4).unwrap());
        let material = RendererMaterialHandle::new(5, NonZeroU32::new(6).unwrap());
        let mesh = RendererMeshHandle::new(7, NonZeroU32::new(8).unwrap());

        let fields =
            decompose_render_queue_key(compose_render_queue_key(9, shader, material, mesh));

        assert_eq!(fields.order, 9);
        assert_eq!(fields.shader_index, 3);
        assert_eq!(fields.shader_version, 4);
        assert_eq!(fields.material_index, 5);
        assert_eq!(fields.material_version, 6);
        assert_eq!(fields.mesh_index, 7);
        assert_eq!(fields.mesh_version, 8);
    }

    #[test]
    fn a_larger_order_sorts_earlier_in_the_composed_key() {
        let shader = RendererShaderHandle::new(3, NonZeroU32::new(4).unwrap());
        let material = RendererMaterialHandle::new(5, NonZeroU32::new(6).unwrap());
        let mesh = RendererMeshHandle::new(7, NonZeroU32::new(8).unwrap());

        let order_200 = compose_render_queue_key(200, shader, material, mesh);
        let order_10 = compose_render_queue_key(10, shader, material, mesh);

        assert!(
            order_200 < order_10,
            "the queue sorts ascending on this key, so the larger order is drawn first"
        );
    }
}
