//! What a frame's draws bind, and the key the queue sorts them by.
//!
//! # Responsibilities
//!
//! - Carry one queued draw per frame instance: the shader, material and mesh
//!   handles it binds, the instance it draws, and the order key the queue
//!   sorts on ([`RenderQueueItem`]).
//! - Pack a draw's material order and the handles' slot identity into the
//!   `u64` the queue sorts with one comparison
//!   ([`compose_render_queue_key`]).
//!
//! # Design
//!
//! Ascending sort order of the key is what puts draws that share a shader,
//! material, and mesh next to each other, which is what the mesh drawer
//! batches into single instanced draws. The key exists for that comparison
//! alone: it holds each index and version in one byte, so a value above 255
//! truncates, and nothing reads a key back as a handle. The handles a draw
//! binds travel beside it unchanged, so an asset whose slot version passed 255
//! still draws with its own objects.

// Standard library
use std::cmp::Ordering;

// Current crate
use crate::resources::{RendererMaterialHandle, RendererMeshHandle, RendererShaderHandle};

/// One queued draw: the handles it binds, the instance it draws, and its sort
/// key.
///
/// The renderer builds one item per frame instance, so a sorted queue
/// describes the whole frame's drawing. Ordering compares the key first and
/// the instance index second, which keeps instances that share state in frame
/// order within their batch; the handles take no part in it.
#[derive(Clone, Copy, Debug)]
pub struct RenderQueueItem {
    /// Sort key: the material order and the handles' slot identity, as packed
    /// by [`compose_render_queue_key`]. Compared, never read back.
    pub key: u64,
    /// Index of the instance this item draws, into the frame's instance
    /// vector, where the mesh drawer reads its transform from.
    pub frame_instance_index: u32,
    /// Shader the draw binds.
    pub shader: RendererShaderHandle,
    /// Material the draw binds.
    pub material: RendererMaterialHandle,
    /// Mesh the draw binds.
    pub mesh: RendererMeshHandle,
}

impl PartialEq for RenderQueueItem {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for RenderQueueItem {}

impl PartialOrd for RenderQueueItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for RenderQueueItem {
    /// The queue's order: the sort key first, then the frame instance.
    fn cmp(&self, other: &Self) -> Ordering {
        self.key
            .cmp(&other.key)
            .then_with(|| self.frame_instance_index.cmp(&other.frame_instance_index))
    }
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
/// incarnation of a slot.
///
/// The key is for sorting only. Every index and version occupies a single
/// byte, so a value above 255 truncates to its low byte and aliases a lower
/// one; a key never names the handles the draw binds, which travel on
/// [`RenderQueueItem`] instead.
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::num::NonZeroU32;

    /// A material that has been rebuilt 256 times keeps its own handle.
    ///
    /// A long editing session re-inserts a material on every content change,
    /// and the slot map hands the freed slot back with a bumped version, so a
    /// version past 255 is reachable. The queue carries the handle itself, so
    /// nothing is lost to the key's byte per field.
    #[test]
    fn a_material_at_slot_version_256_keeps_its_handle() {
        let shader = RendererShaderHandle::new(3, NonZeroU32::new(4).unwrap());
        let material = RendererMaterialHandle::new(5, NonZeroU32::new(256).unwrap());
        let mesh = RendererMeshHandle::new(7, NonZeroU32::new(8).unwrap());

        let item = RenderQueueItem {
            key: compose_render_queue_key(0, shader, material, mesh),
            frame_instance_index: 0,
            shader,
            material,
            mesh,
        };

        assert_eq!(item.material.data().version.get(), 256);
        assert_eq!(item.shader, shader);
        assert_eq!(item.mesh, mesh);
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

    #[test]
    fn the_key_decides_the_order_and_the_instance_breaks_a_tie() {
        let shader = RendererShaderHandle::new(3, NonZeroU32::new(4).unwrap());
        let material = RendererMaterialHandle::new(5, NonZeroU32::new(6).unwrap());
        let mesh = RendererMeshHandle::new(7, NonZeroU32::new(8).unwrap());
        let key = compose_render_queue_key(0, shader, material, mesh);
        let item = |frame_instance_index| RenderQueueItem {
            key,
            frame_instance_index,
            shader,
            material,
            mesh,
        };

        assert!(item(0) < item(1), "the same state keeps frame order");
        assert_eq!(item(4), item(4));
    }
}
