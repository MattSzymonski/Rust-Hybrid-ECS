//! Packed sort key used by the transferred mesh drawer.

use crate::slot_map::{RendererMaterialHandle, RendererMeshHandle, RendererShaderHandle};

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub struct RenderQueueItem {
    pub key: u64,
    pub entity_index: u32,
}

#[derive(Clone, Copy, Debug)]
pub struct RenderQueueKeyFields {
    pub order: u8,
    pub shader_index: u8,
    pub shader_version: u8,
    pub material_index: u8,
    pub material_version: u8,
    pub mesh_index: u8,
    pub mesh_version: u8,
}

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
}
