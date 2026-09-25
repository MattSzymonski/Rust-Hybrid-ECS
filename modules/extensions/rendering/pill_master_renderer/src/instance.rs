use crate::resources::Vertex;

use crate::component::TransformComponent;
use pill_core::math::Matrix3f;

// --- Instance ---

#[repr(C)]
#[derive(Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Instance {
    pub(crate) transform: Matrix3f, // It is matrix3 because we only need the rotation componen
}

impl Instance {
    pub fn new(transform_component: &TransformComponent) -> Instance {
        let rotation = glam::Quat::from_array(transform_component.rotation);
        let rotation = if rotation.is_finite() && rotation.length_squared() > 1e-8 {
            rotation.normalize()
        } else {
            glam::Quat::IDENTITY
        };
        // The vertex shader builds `rot_z * rot_y * rot_x` from the three
        // numbers it is handed, so the CPU has to hand it angles in that
        // convention. It used to send `axis * angle`, which the shader read as
        // three Euler angles - a rotation about (x, y, z) that has nothing to
        // do with the quaternion it came from, and one that lost the winding
        // past half a turn. `ZYX` is the order the shader's multiplication
        // spells out, and glam returns the angles in reverse: (z, y, x).
        let (z, y, x) = rotation.to_euler(glam::EulerRot::ZYX);
        Instance {
            transform: Matrix3f::from_cols(
                transform_component.translation.into(),
                glam::Vec3::new(x, y, z),
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
                }, // Model matrix (mat4 takes up 4 vertex slots as it is technically 4 vec4s. We need to define a slot for each vec4)
                   //     wgpu::VertexAttribute {
                   //         offset: 0,
                   //         shader_location: 5,
                   //         format: wgpu::VertexFormat::Float32x4,
                   //     },
                   //     wgpu::VertexAttribute {
                   //         offset: mem::size_of::<[f32; 4]>() as wgpu::BufferAddress,
                   //         shader_location: 6,
                   //         format: wgpu::VertexFormat::Float32x4,
                   //     },
                   //     wgpu::VertexAttribute {
                   //         offset: mem::size_of::<[f32; 8]>() as wgpu::BufferAddress,
                   //         shader_location: 7,
                   //         format: wgpu::VertexFormat::Float32x4,
                   //     },
                   //     wgpu::VertexAttribute {
                   //         offset: mem::size_of::<[f32; 12]>() as wgpu::BufferAddress,
                   //         shader_location: 8,
                   //         format: wgpu::VertexFormat::Float32x4,
                   //     },

                   //     // Normal matrix
                   //     wgpu::VertexAttribute {
                   //         offset: mem::size_of::<[f32; 16]>() as wgpu::BufferAddress,
                   //         shader_location: 9,
                   //         format: wgpu::VertexFormat::Float32x3,
                   //     },
                   //     wgpu::VertexAttribute {
                   //         offset: mem::size_of::<[f32; 19]>() as wgpu::BufferAddress,
                   //         shader_location: 10,
                   //         format: wgpu::VertexFormat::Float32x3,
                   //     },
                   //     wgpu::VertexAttribute {
                   //         offset: mem::size_of::<[f32; 22]>() as wgpu::BufferAddress,
                   //         shader_location: 11,
                   //         format: wgpu::VertexFormat::Float32x3,
                   //     },
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The rotation the vertex shader builds from the three numbers it is
    /// handed, spelled the way the shader spells it: `rot_z * rot_y * rot_x`.
    fn shader_rotation(angles: glam::Vec3) -> glam::Mat3 {
        glam::Mat3::from_rotation_z(angles.z)
            * glam::Mat3::from_rotation_y(angles.y)
            * glam::Mat3::from_rotation_x(angles.x)
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
}
