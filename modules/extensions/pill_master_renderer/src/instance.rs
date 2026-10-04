//! One instance's transform in the layout the vertex shader reads it, and the
//! descriptor the vertex buffer is bound with.
//!
//! # Responsibilities
//!
//! - Carry one queued instance's model matrix, packed into the three `float4`
//!   rows the shader reads ([`Instance`]).
//! - Build that matrix from a [`TransformComponent`] (`Instance::new`).
//! - Build every queued instance's matrix for a frame, in queue order, on
//!   the shared thread pool ([`build_queued_instances`]).
//! - Describe the vertex buffer layout for the instance step mode: three
//!   attributes at consecutive offsets, advanced once per instance.
//!
//! # Design
//!
//! The matrix is built once per instance on the CPU and the shader only
//! assembles it. It used to be the other way round: the CPU decomposed the
//! rotation into Euler angles (three inverse trigonometric calls) and every
//! vertex rebuilt the matrix from them (six more, and four 4x4 products), so
//! a mesh drawn 50,000 times paid for the same matrix once per vertex.
//!
//! Only the top three rows of the affine matrix are sent; the fourth is always
//! `0, 0, 0, 1` and the shader supplies it. `#[repr(C)]` over three `[f32; 4]`
//! rows keeps the layout padding-free, so the offsets the descriptor declares
//! match the attributes the shader expects.

// External crates
use glam::{Quat, Vec3};
use pill_core::rayon::prelude::*;

// Current crate
use crate::components::TransformComponent;
use crate::frame::RenderInstance;
use crate::render_queue::RenderQueueItem;
use crate::resources::Vertex;

// --- Instance ---

/// One instance's model matrix, in the layout the vertex shader reads it.
///
/// The renderer builds one per queued instance into the buffer the draws bind,
/// and the shader reads each as three `float4` attributes.
#[repr(C)]
#[derive(Copy, Clone, Debug, PartialEq, bytemuck::Pod, bytemuck::Zeroable)]
pub struct Instance {
    /// The top three rows of the world-from-model matrix: row `i` holds the
    /// `i`th component of the scaled rotation axes, then of the translation.
    pub(crate) model_rows: [[f32; 4]; 3],
}

impl Instance {
    /// Builds one instance's model matrix from an entity's transform
    /// component: scale, then rotation, then translation.
    ///
    /// A rotation that is not finite or has no length (an unset component, or
    /// one a script broke) draws unrotated instead of producing a degenerate
    /// matrix.
    pub fn new(transform_component: &TransformComponent) -> Instance {
        let rotation = Quat::from_array(transform_component.rotation);
        let rotation = if rotation.is_finite() && rotation.length_squared() > 1e-8 {
            rotation.normalize()
        } else {
            Quat::IDENTITY
        };
        let model = glam::Affine3A::from_scale_rotation_translation(
            Vec3::from_array(transform_component.scale),
            rotation,
            Vec3::from_array(transform_component.translation),
        );
        // glam stores the matrix by column; the shader wants it by row.
        let axes = [
            model.matrix3.x_axis,
            model.matrix3.y_axis,
            model.matrix3.z_axis,
        ];
        let translation = model.translation;
        let row = |index: usize| {
            [
                axes[0][index],
                axes[1][index],
                axes[2][index],
                translation[index],
            ]
        };
        Instance {
            model_rows: [row(0), row(1), row(2)],
        }
    }
}

/// Fewest instances one parallel task builds. Small enough to spread a
/// frame's instances across the pool, large enough that scheduling a task
/// costs far less than the work in it.
const INSTANCES_PER_TASK: usize = 2048;

/// The model matrix of every queued instance, in queue order, into `output`.
///
/// Queue order is draw order, so the result is uploaded as it is and a draw
/// addresses its instances by their queue positions. The work runs on Rayon's
/// global pool, the one inside `pill_core.dll` every DLL shares: its threads
/// are already running, and none of them is left parked in this DLL's code
/// when the host reloads it, because the call returns only once every task
/// has finished.
pub(crate) fn build_queued_instances(
    render_queue: &[RenderQueueItem],
    frame_instances: &[RenderInstance],
    output: &mut Vec<Instance>,
) {
    output.clear();
    output.resize(render_queue.len(), bytemuck::Zeroable::zeroed());
    output
        .par_chunks_mut(INSTANCES_PER_TASK)
        .zip(render_queue.par_chunks(INSTANCES_PER_TASK))
        .for_each(|(output_part, queue_part)| {
            for (instance, item) in output_part.iter_mut().zip(queue_part) {
                *instance = Instance::new(&frame_instances[item.entity_index as usize].transform);
            }
        });
}

impl Vertex for Instance {
    fn data_layout_descriptor<'a>() -> wgpu::VertexBufferLayout<'a> {
        use std::mem;
        const ROW_SIZE: wgpu::BufferAddress = mem::size_of::<[f32; 4]>() as wgpu::BufferAddress;
        wgpu::VertexBufferLayout {
            array_stride: mem::size_of::<Instance>() as wgpu::BufferAddress,
            // The shader moves to the next instance's rows once per instance,
            // not once per vertex.
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &[
                wgpu::VertexAttribute {
                    // Model matrix row 0; slangc maps TEXCOORD1 to @location(1).
                    offset: 0,
                    shader_location: 1,
                    format: wgpu::VertexFormat::Float32x4,
                },
                wgpu::VertexAttribute {
                    // Model matrix row 1; slangc maps TEXCOORD2 to @location(2).
                    offset: ROW_SIZE,
                    shader_location: 2,
                    format: wgpu::VertexFormat::Float32x4,
                },
                wgpu::VertexAttribute {
                    // Model matrix row 2; slangc maps TEXCOORD3 to @location(3).
                    offset: 2 * ROW_SIZE,
                    shader_location: 3,
                    format: wgpu::VertexFormat::Float32x4,
                },
            ],
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The world position the shader computes for a model-space `point`: the
    /// three rows dotted with `(point, 1)`, which is what `mul(model, v)` does
    /// with the fourth row fixed at `0, 0, 0, 1`.
    fn shader_transform(instance: &Instance, point: Vec3) -> Vec3 {
        let homogeneous = point.extend(1.0);
        let row =
            |index: usize| glam::Vec4::from_array(instance.model_rows[index]).dot(homogeneous);
        Vec3::new(row(0), row(1), row(2))
    }

    fn largest_difference(left: Vec3, right: Vec3) -> f32 {
        (left - right).abs().max_element()
    }

    #[test]
    fn the_shader_applies_the_rotation_the_component_holds() {
        for rotation in [
            Quat::IDENTITY,
            Quat::from_rotation_x(0.4),
            Quat::from_rotation_y(2.0),
            Quat::from_rotation_z(-1.2),
            Quat::from_rotation_y(0.7) * Quat::from_rotation_x(0.3),
            // Past half a turn: an older encoding lost the winding here, and a
            // spinning model came back rotated the other way.
            Quat::from_rotation_y(4.5),
        ] {
            let instance = Instance::new(&TransformComponent {
                translation: [0.0; 3],
                rotation: rotation.to_array(),
                scale: [1.0; 3],
            });
            for point in [Vec3::X, Vec3::Y, Vec3::Z, Vec3::new(0.3, -1.2, 2.5)] {
                let difference =
                    largest_difference(shader_transform(&instance, point), rotation * point);
                assert!(
                    difference < 1e-5,
                    "the shader's point differs by {difference} for {rotation:?}"
                );
            }
        }
    }

    #[test]
    fn scale_applies_before_rotation_and_translation_after() {
        let rotation = Quat::from_rotation_z(std::f32::consts::FRAC_PI_2);
        let instance = Instance::new(&TransformComponent {
            translation: [1.0, 2.0, 3.0],
            rotation: rotation.to_array(),
            scale: [4.0, 5.0, 6.0],
        });

        // +X scaled by 4, turned a quarter about Z onto +Y, then moved.
        let moved = shader_transform(&instance, Vec3::X);
        assert!(
            largest_difference(moved, Vec3::new(1.0, 6.0, 3.0)) < 1e-5,
            "{moved:?}"
        );
    }

    #[test]
    fn a_broken_rotation_draws_unrotated() {
        let instance = Instance::new(&TransformComponent {
            translation: [0.0; 3],
            rotation: [0.0; 4],
            scale: [1.0; 3],
        });

        assert_eq!(shader_transform(&instance, Vec3::X), Vec3::X);
    }

    #[test]
    fn the_instance_layout_matches_the_shader_attributes() {
        // The shader declares three `float4` attributes at consecutive offsets.
        // `Instance` has to stay exactly twelve floats with no padding, and the
        // layout has to keep reading them at those raw offsets - a field added
        // here or a format changed there would otherwise show up as garbled
        // transforms rather than as a compile error.
        assert_eq!(std::mem::size_of::<Instance>(), 48);

        let layout = <Instance as Vertex>::data_layout_descriptor();

        assert_eq!(layout.array_stride, 48);
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
                (0, 1, wgpu::VertexFormat::Float32x4),
                (16, 2, wgpu::VertexFormat::Float32x4),
                (32, 3, wgpu::VertexFormat::Float32x4),
            ]
        );
    }

    #[test]
    fn instances_split_across_tasks_keep_queue_order() {
        let count = INSTANCES_PER_TASK * 3 + 7;
        let frame_instances: Vec<RenderInstance> = (0..count)
            .map(|index| RenderInstance {
                transform: TransformComponent {
                    translation: [index as f32, 0.0, 0.0],
                    rotation: Quat::IDENTITY.to_array(),
                    scale: [1.0; 3],
                },
                mesh: 0,
                material: 0,
                rendering_order: 0,
            })
            .collect();
        // The queue in reverse entity order.
        let queue: Vec<RenderQueueItem> = (0..count as u32)
            .rev()
            .map(|entity_index| RenderQueueItem {
                key: 0,
                entity_index,
            })
            .collect();
        let mut output = Vec::new();

        build_queued_instances(&queue, &frame_instances, &mut output);

        assert_eq!(output.len(), count);
        for (position, instance) in output.iter().enumerate() {
            assert_eq!(instance.model_rows[0][3], (count - 1 - position) as f32);
        }
    }

    #[test]
    fn built_instances_follow_queue_order() {
        let frame_instances: Vec<RenderInstance> = (0..3)
            .map(|index| RenderInstance {
                transform: TransformComponent {
                    translation: [index as f32, 0.0, 0.0],
                    rotation: Quat::IDENTITY.to_array(),
                    scale: [1.0; 3],
                },
                mesh: 0,
                material: 0,
                rendering_order: 0,
            })
            .collect();
        let queue = [2, 0, 1].map(|entity_index| RenderQueueItem {
            key: 0,
            entity_index,
        });
        let mut output = Vec::new();

        build_queued_instances(&queue, &frame_instances, &mut output);

        let translations: Vec<f32> = output
            .iter()
            .map(|instance| instance.model_rows[0][3])
            .collect();
        assert_eq!(translations, [2.0, 0.0, 1.0]);
    }
}
