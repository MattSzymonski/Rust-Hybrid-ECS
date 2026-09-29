//! One instance's transform in the layout the vertex shader reads it, and the
//! descriptor the vertex buffer is bound with.
//!
//! # Responsibilities
//!
//! - Carry one queued instance's transform, packed into the three `float3`
//!   slots the shader reads ([`Instance`]).
//! - Encode a [`TransformComponent`] into that layout, negating the
//!   decomposed Euler angles so the shader's row-first multiplication lands
//!   on the rotation the component describes (`Instance::new`).
//! - Describe the vertex buffer layout for the instance step mode: three
//!   attributes at consecutive offsets, advanced once per instance.
//!
//! # Design
//!
//! `#[repr(C)]` over a single [`Matrix3f`] keeps the layout padding-free:
//! each column is one `float3` at a fixed offset, and the raw offsets the
//! descriptor declares match what the shader's attributes expect. The angle
//! negation happens when the instance is built, so the shader stays the plain
//! row-first multiplication it is.

// External crates
use pill_core::math::Matrix3f;

// Current crate
use crate::components::TransformComponent;
use crate::resources::Vertex;

// --- Instance ---

/// One instance's transform, in the layout the vertex shader reads it.
///
/// The mesh drawer accumulates one per queued instance into the buffer the
/// draws bind, and the shader reads each as three `float3` attributes.
#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Instance {
    /// The instance transform: the columns carry the translation, rotation,
    /// and scale the shader rebuilds its model matrix from.
    pub(crate) transform: Matrix3f, // It is matrix3 because we only need the rotation component
}

impl Instance {
    /// Builds one instance's transform from an entity's transform component.
    ///
    /// Translation and scale go through as they are; the quaternion rotation
    /// is decomposed into Euler angles and negated, because the shader
    /// multiplies row-first and would otherwise apply the inverse.
    pub fn new(transform_component: &TransformComponent) -> Instance {
        let rotation = glam::Quat::from_array(transform_component.rotation);
        let rotation = if rotation.is_finite() && rotation.length_squared() > 1e-8 {
            rotation.normalize()
        } else {
            glam::Quat::IDENTITY
        };
        // The vertex shader builds `(rot_x * rot_y) * rot_z` from the three
        // numbers it is handed and multiplies vectors row-first (`v * M`),
        // which WGSL defines as `transpose(M) * v` - so what it renders is the
        // rotation with every angle negated. Handing it the plain angles would
        // show the inverse of the quaternion; the CPU negates them so the
        // shader's transpose cancels out. It used to send `axis * angle`,
        // which the shader read as three Euler angles - a rotation about
        // (x, y, z) that had nothing to do with the quaternion it came from,
        // and one that lost the winding past half a turn. `ZYX` is the
        // decomposition order the shader's multiplication spells out, and glam
        // returns the angles in reverse: (z, y, x).
        let (z, y, x) = rotation.to_euler(glam::EulerRot::ZYX);
        Instance {
            transform: Matrix3f::from_cols(
                transform_component.translation.into(),
                glam::Vec3::new(-x, -y, -z),
                transform_component.scale.into(),
            ),
        }
    }
}

impl Vertex for Instance {
    fn data_layout_descriptor<'a>() -> wgpu::VertexBufferLayout<'a> {
        use std::mem;
        wgpu::VertexBufferLayout {
            array_stride: mem::size_of::<Instance>() as wgpu::BufferAddress,
            // We need to switch from using a step mode of Vertex to Instance
            // This means that shaders will only change to use the next instance when the shader starts processing a new instance
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                wgpu::VertexAttribute {
                    // Instance transform position
                    // slangc maps TEXCOORD1 → @location(1)
                    offset: 0,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x3,
                },
                wgpu::VertexAttribute {
                    // Instance transform rotation
                    // slangc maps TEXCOORD2 → @location(2)
                    offset: mem::size_of::<[f32; 3]>() as wgpu::BufferAddress,
                    shader_location: 2,
                    format: wgpu::VertexFormat::Float32x3,
                },
                wgpu::VertexAttribute {
                    // Instance transform scale
                    // slangc maps TEXCOORD3 → @location(3)
                    offset: mem::size_of::<[f32; 6]>() as wgpu::BufferAddress,
                    shader_location: 3,
                    format: wgpu::VertexFormat::Float32x3,
                },
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rotation the vertex shader effectively applies, spelled the way the
    /// shader spells it: it builds `(rot_x * rot_y) * rot_z` and multiplies
    /// vectors row-first, so the matrix a vertex feels is the transpose of
    /// that product.
    fn shader_rotation(angles: glam::Vec3) -> glam::Mat3 {
        (glam::Mat3::from_rotation_x(angles.x)
            * glam::Mat3::from_rotation_y(angles.y)
            * glam::Mat3::from_rotation_z(angles.z))
        .transpose()
    }

    fn instance_of(rotation: glam::Quat) -> Instance {
        Instance::new(&TransformComponent {
            translation: [0.0; 3],
            rotation: rotation.to_array(),
            scale: [1.0; 3],
        })
    }

    #[test]
    fn the_angles_the_shader_is_given_rebuild_the_rotation_they_came_from() {
        for rotation in [
            glam::Quat::IDENTITY,
            glam::Quat::from_rotation_x(0.4),
            glam::Quat::from_rotation_y(2.0),
            glam::Quat::from_rotation_z(-1.2),
            glam::Quat::from_rotation_y(0.7) * glam::Quat::from_rotation_x(0.3),
            // Past half a turn: the encoding this replaced lost the winding
            // here, and a spinning model came back rotated the other way.
            glam::Quat::from_rotation_y(4.5),
        ] {
            let instance = instance_of(rotation);
            let rebuilt = shader_rotation(instance.transform.y_axis);
            let expected = glam::Mat3::from_quat(rotation);

            let difference = (rebuilt - expected)
                .to_cols_array()
                .iter()
                .fold(0.0f32, |worst, value| worst.max(value.abs()));
            assert!(
                difference < 1e-4,
                "the shader's matrix differs by {difference} for {rotation:?}"
            );
        }
    }

    #[test]
    fn the_transform_keeps_the_translation_and_scale_it_was_given() {
        let instance = Instance::new(&TransformComponent {
            translation: [1.0, 2.0, 3.0],
            rotation: glam::Quat::IDENTITY.to_array(),
            scale: [4.0, 5.0, 6.0],
        });

        assert_eq!(instance.transform.x_axis, glam::Vec3::new(1.0, 2.0, 3.0));
        assert_eq!(instance.transform.z_axis, glam::Vec3::new(4.0, 5.0, 6.0));
        assert_eq!(instance.transform.y_axis, glam::Vec3::ZERO);
    }

    #[test]
    fn the_instance_layout_matches_the_shader_attributes() {
        // The shader declares three `float3` attributes at consecutive offsets.
        // `Instance` has to stay exactly three floats each with no padding, and
        // the layout has to keep reading them at those raw offsets - a field
        // added here or a format widened there would otherwise show up as
        // garbled transforms rather than as a compile error.
        assert_eq!(std::mem::size_of::<Instance>(), 36);

        let layout = <Instance as Vertex>::data_layout_descriptor();

        assert_eq!(layout.array_stride, 36);
        assert_eq!(layout.step_mode, wgpu::VertexStepMode::Instance);
        let attributes: Vec<(u64, u32, wgpu::VertexFormat)> = layout
            .attributes
            .iter()
            .map(|attribute| {
                (
                    attribute.offset,
                    attribute.shader_location,
                    attribute.format,
                )
            })
            .collect();
        assert_eq!(
            attributes,
            [
                (0, 1, wgpu::VertexFormat::Float32x3),
                (12, 2, wgpu::VertexFormat::Float32x3),
                (24, 3, wgpu::VertexFormat::Float32x3),
            ]
        );
    }
}
