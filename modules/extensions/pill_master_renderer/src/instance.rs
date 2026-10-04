//! One instance's transform in the layout the vertex shader reads it, and the
//! descriptor the vertex buffer is bound with.
//!
//! # Responsibilities
//!
//! - Carry one queued instance's model matrix, packed into the three `float4`
//!   rows the shader reads ([`Instance`]).
//! - Build that matrix from a [`TransformComponent`] (`Instance::new`).
//! - Build every queued instance's matrix for a frame, in queue order, on as
//!   many threads as makes that fastest ([`InstanceBuilder`]).
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

// Standard library
use std::sync::OnceLock;

// External crates
use glam::{Quat, Vec3};
use pill_core::platform::Instant;

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

/// How long the first extra thread takes to start working, in nanoseconds.
///
/// Measured in Tracy on Windows: the first thread a frame starts begins about
/// 450 microseconds after the call, and every further one about 150 after the
/// one before it, because the calling thread creates them one after another.
const FIRST_THREAD_START_NANOSECONDS: f64 = 450_000.0;

/// How much later each further thread starts than the one before it.
const NEXT_THREAD_START_NANOSECONDS: f64 = 150_000.0;

/// How strongly one frame's measurement moves the remembered per-instance
/// cost: a little, so one slow frame does not flip the thread count.
const COST_SMOOTHING: f64 = 0.1;

/// Builds the model matrix of every queued instance, splitting the work
/// across threads only when that is faster.
///
/// The work is split across scoped threads, which are joined before
/// [`InstanceBuilder::build`] returns. A thread pool would be faster to start,
/// but this code lives in a DLL the host reloads: a pool owned by it (Rayon's
/// global one included, which every DLL linking `pill_engine` has its own copy
/// of) would keep its threads parked inside code that is unmapped after the
/// swap.
///
/// Starting threads is slow, so the thread count follows the work: each count
/// is costed as the time its last thread takes to start plus its share of
/// the work, and the cheapest wins. The work comes from the per-instance cost
/// measured on earlier frames. With 50,000 instances an optimized build needs
/// about half a millisecond and builds them inline, while a debug build
/// needs about eleven and uses around nine threads.
#[derive(Debug, Default)]
pub(crate) struct InstanceBuilder {
    /// Measured cost of building one instance, smoothed over frames; zero
    /// until the first frame measures it.
    nanoseconds_per_instance: f64,
}

impl InstanceBuilder {
    /// The model matrix of every queued instance, in queue order, into
    /// `output`.
    ///
    /// Queue order is draw order, so the result is uploaded as it is and a
    /// draw addresses its instances by their queue positions.
    pub(crate) fn build(
        &mut self,
        render_queue: &[RenderQueueItem],
        frame_instances: &[RenderInstance],
        output: &mut Vec<Instance>,
    ) {
        output.clear();
        output.resize(render_queue.len(), bytemuck::Zeroable::zeroed());

        // Builds one contiguous part of the queue into the matching part of
        // the output.
        let build = |queue_part: &[RenderQueueItem], output_part: &mut [Instance]| {
            for (item, instance) in queue_part.iter().zip(output_part) {
                *instance = Instance::new(&frame_instances[item.entity_index as usize].transform);
            }
        };

        let thread_count = self.thread_count(render_queue.len());
        let part_length = render_queue.len().div_ceil(thread_count).max(1);
        let build = &build;
        // The calling thread's own part, timed to keep the cost estimate
        // current.
        let mut own_part_measurement = None;
        std::thread::scope(|scope| {
            let mut parts = render_queue
                .chunks(part_length)
                .zip(output.chunks_mut(part_length));
            let own_part = parts.next();
            for (queue_part, output_part) in parts {
                scope.spawn(move || {
                    let _zone = pill_core::profile_scope!(
                        "build instances part",
                        [("{} instances", queue_part.len())]
                    );
                    build(queue_part, output_part);
                });
            }
            // Built after the spawns, so the other threads are already
            // starting while this one works.
            if let Some((queue_part, output_part)) = own_part {
                let started = Instant::now();
                build(queue_part, output_part);
                own_part_measurement = Some((started.elapsed().as_nanos(), queue_part.len()));
            }
        });

        if let Some((elapsed_nanoseconds, built)) = own_part_measurement {
            self.record(elapsed_nanoseconds as f64 / built as f64);
        }
    }

    /// Threads to build `instance_count` instances with: one until a frame
    /// has measured the cost, then the count that finishes soonest.
    fn thread_count(&self, instance_count: usize) -> usize {
        let total_nanoseconds = self.nanoseconds_per_instance * instance_count as f64;
        // When the last of `thread_count` threads finishes its share.
        let finish_time = |thread_count: usize| {
            let last_start = match thread_count {
                1 => 0.0,
                _ => {
                    FIRST_THREAD_START_NANOSECONDS
                        + (thread_count - 2) as f64 * NEXT_THREAD_START_NANOSECONDS
                }
            };
            last_start + total_nanoseconds / thread_count as f64
        };
        (1..=available_threads())
            .min_by(|left, right| finish_time(*left).total_cmp(&finish_time(*right)))
            .unwrap_or(1)
    }

    /// Folds one frame's measured per-instance cost into the estimate.
    fn record(&mut self, nanoseconds_per_instance: f64) {
        self.nanoseconds_per_instance = if self.nanoseconds_per_instance == 0.0 {
            nanoseconds_per_instance
        } else {
            self.nanoseconds_per_instance
                + (nanoseconds_per_instance - self.nanoseconds_per_instance) * COST_SMOOTHING
        };
    }
}

/// How many threads the machine runs at once, asked once and remembered.
fn available_threads() -> usize {
    static AVAILABLE_THREADS: OnceLock<usize> = OnceLock::new();
    *AVAILABLE_THREADS.get_or_init(|| {
        std::thread::available_parallelism()
            .map(|threads| threads.get())
            .unwrap_or(1)
    })
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

    /// A builder that has measured `nanoseconds_per_instance`.
    fn builder_measuring(nanoseconds_per_instance: f64) -> InstanceBuilder {
        InstanceBuilder {
            nanoseconds_per_instance,
        }
    }

    #[test]
    fn work_cheaper_than_starting_threads_stays_on_the_calling_thread() {
        // Nothing measured yet, and an optimized build's ~8 ns per instance.
        assert_eq!(InstanceBuilder::default().thread_count(50_000), 1);
        assert_eq!(builder_measuring(8.0).thread_count(50_000), 1);
    }

    #[test]
    fn work_worth_splitting_uses_several_threads() {
        // A debug build's ~220 ns per instance: 11 ms of work, which nine
        // threads finish soonest, unless the machine runs fewer.
        let expected = 9.min(available_threads());
        assert_eq!(builder_measuring(220.0).thread_count(50_000), expected);
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

        InstanceBuilder::default().build(&queue, &frame_instances, &mut output);

        let translations: Vec<f32> = output
            .iter()
            .map(|instance| instance.model_rows[0][3])
            .collect();
        assert_eq!(translations, [2.0, 0.0, 1.0]);
    }
}
