//! Minimal 2D sprite renderer (wgpu-backed), gated behind the `rendering` feature.
//!
//! # Responsibilities
//!
//! - Defines [`Position`] and [`Sprite`] components describing where and how
//!   to draw an entity as a colored rectangle.
//! - Provides [`SpriteRenderer`], the low-level sprite pipeline used by the
//!   engine-owned [`Renderer`](crate::Renderer).
//!
//! # Design
//!
//! This module intentionally stays tiny: it draws colored rectangles, not a
//! general sprite/texture pipeline. [`crate::Renderer`] owns the normal window
//! surface/device/queue lifecycle. Advanced integrations such as the editor
//! may still drive `SpriteRenderer` directly when they must attach to a surface
//! whose lifetime is owned by another UI framework.

// External crates
use trait_type_map::impl_trait_accessible;
use wgpu::util::DeviceExt;

// Current crate
use crate::component::Component;
use crate::world::World;

// =============================================================================
// Constants
// =============================================================================

/// WGSL source for the sprite render pipeline.
///
/// Declares the `Viewport` uniform, the per-instance vertex inputs, and both
/// the vertex and fragment entry points for instanced quad rendering.
const SHADER_SOURCE: &str = r#"
struct Viewport {
    size: vec2<f32>,
};
@group(0) @binding(0) var<uniform> viewport: Viewport;

struct InstanceInput {
    @location(0) position: vec2<f32>,
    @location(1) size: vec2<f32>,
    @location(2) color: vec4<f32>,
};

struct VertexOutput {
    @builtin(position) clip_position: vec4<f32>,
    @location(0) color: vec4<f32>,
};

// Unit quad corners, expanded per-instance by `size` and offset by `position`.
var<private> CORNERS: array<vec2<f32>, 6> = array<vec2<f32>, 6>(
    vec2<f32>(0.0, 0.0),
    vec2<f32>(1.0, 0.0),
    vec2<f32>(0.0, 1.0),
    vec2<f32>(0.0, 1.0),
    vec2<f32>(1.0, 0.0),
    vec2<f32>(1.0, 1.0),
);

@vertex
fn vs_main(@builtin(vertex_index) vertex_index: u32, instance: InstanceInput) -> VertexOutput {
    let corner = CORNERS[vertex_index];
    let pixel_position = instance.position + corner * instance.size;
    // Pixel space (origin top-left, +y down) -> NDC (origin center, +y up).
    let ndc = vec2<f32>(
        (pixel_position.x / viewport.size.x) * 2.0 - 1.0,
        1.0 - (pixel_position.y / viewport.size.y) * 2.0,
    );

    var out: VertexOutput;
    out.clip_position = vec4<f32>(ndc, 0.0, 1.0);
    out.color = instance.color;
    return out;
}

@fragment
fn fs_main(in: VertexOutput) -> @location(0) vec4<f32> {
    return in.color;
}
"#;

// =============================================================================
// Components
// =============================================================================

/// World-space position of an entity's top-left draw origin, in pixels.
///
/// Position is a renderer component, resolved by the sprite pipeline through
/// the shared ABI by stable type name rather than by Rust `TypeId`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct Position {
    /// Horizontal pixel coordinate of the draw origin.
    pub x: f32,
    /// Vertical pixel coordinate of the draw origin.
    pub y: f32,
}
impl Component for Position {}
impl_trait_accessible!(dyn Component; Position);

/// Plain RGBA color, backend-agnostic (0.0-1.0 per channel).
///
/// `#[repr(C)]` with normalized float channels so the byte layout is shared
/// with the C# runtime as part of the renderer ABI.
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Color {
    /// Red channel.
    pub r: f32,
    /// Green channel.
    pub g: f32,
    /// Blue channel.
    pub b: f32,
    /// Alpha channel.
    pub a: f32,
}
impl Component for Color {}
impl_trait_accessible!(dyn Component; Color);

impl Color {
    /// Opaque white, the default fill color of [`Sprite`].
    pub const WHITE: Color = Color {
        r: 1.0,
        g: 1.0,
        b: 1.0,
        a: 1.0,
    };

    /// Construct a color from its red, green, blue, and alpha channels.
    pub const fn new(r: f32, g: f32, b: f32, a: f32) -> Self {
        Self { r, g, b, a }
    }
}

impl Default for Color {
    fn default() -> Self {
        Self::WHITE
    }
}

/// Axis-aligned colored rectangle drawn at an entity's [`Position`].
///
/// The quad spans `width` by `height` pixels starting at the entity's draw
/// origin and is filled with `color`.
#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct Sprite {
    /// Quad width in pixels.
    pub width: f32,
    /// Quad height in pixels.
    pub height: f32,
    /// Fill color of the quad.
    pub color: Color,
}
impl Component for Sprite {}
impl_trait_accessible!(dyn Component; Sprite);

impl Default for Sprite {
    fn default() -> Self {
        Self {
            width: 16.0,
            height: 16.0,
            color: Color::WHITE,
        }
    }
}

/// Physical-pixel rectangle within a render target.
///
/// Sprite positions are interpreted relative to this rectangle's top-left
/// corner. The GPU viewport maps their local coordinates into the rectangle,
/// while a matching scissor prevents drawing outside it.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct RenderViewport {
    /// Left edge of the rectangle, in physical pixels.
    pub x: u32,
    /// Top edge of the rectangle, in physical pixels.
    pub y: u32,
    /// Horizontal extent of the rectangle, in physical pixels.
    pub width: u32,
    /// Vertical extent of the rectangle, in physical pixels.
    pub height: u32,
}

impl RenderViewport {
    /// Construct a physical-pixel viewport rectangle.
    pub const fn new(x: u32, y: u32, width: u32, height: u32) -> Self {
        Self {
            x,
            y,
            width,
            height,
        }
    }

    /// Construct a viewport covering an entire render target.
    pub const fn full(width: u32, height: u32) -> Self {
        Self::new(0, 0, width, height)
    }

    /// Clamp this rectangle to a render target, returning `None` when empty.
    pub fn clamped_to(self, target_width: u32, target_height: u32) -> Option<Self> {
        let x = self.x.min(target_width);
        let y = self.y.min(target_height);
        let width = self.width.min(target_width.saturating_sub(x));
        let height = self.height.min(target_height.saturating_sub(y));

        (width > 0 && height > 0).then_some(Self::new(x, y, width, height))
    }
}

/// Logical coordinate space mapped into a physical [`RenderViewport`].
///
/// Keeping this separate from the swapchain dimensions lets an embedded project
/// keep a stable coordinate system while its dock panel is resized. The GPU
/// viewport performs the final scaling into the panel rectangle.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct VirtualResolution {
    /// Horizontal extent of the project coordinate space.
    pub width: f32,
    /// Vertical extent of the project coordinate space.
    pub height: f32,
}

impl VirtualResolution {
    /// Construct a logical scene resolution.
    pub const fn new(width: f32, height: f32) -> Self {
        Self { width, height }
    }

    /// Return whether both dimensions can safely be used by the projection.
    pub fn is_valid(self) -> bool {
        self.width.is_finite() && self.height.is_finite() && self.width > 0.0 && self.height > 0.0
    }
}

// =============================================================================
// GPU instance data
// =============================================================================

/// Per-instance data uploaded to the GPU for one sprite quad.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct SpriteInstance {
    /// Top-left position in pixels.
    position: [f32; 2],
    /// Width/height in pixels.
    size: [f32; 2],
    /// RGBA color.
    color: [f32; 4],
}

/// Uniform holding the viewport size, used to convert pixel coordinates to
/// normalized device coordinates in the vertex shader.
#[repr(C)]
#[derive(Debug, Clone, Copy, bytemuck::Pod, bytemuck::Zeroable)]
struct ViewportUniform {
    /// Viewport width in pixels.
    width: f32,
    /// Viewport height in pixels.
    height: f32,
    /// Padding to 16-byte alignment required by WGSL uniform buffers.
    _padding: [f32; 2],
}

// =============================================================================
// SpriteRenderer
// =============================================================================

/// Draws every `(Position, Sprite)` entity as an instanced colored quad.
///
/// Owns a render pipeline and reusable GPU buffers; does not own a window,
/// surface, device, or queue - callers create those and pass them in.
pub struct SpriteRenderer {
    /// Compiled render pipeline for the sprite shader.
    pipeline: wgpu::RenderPipeline,
    /// GPU buffer holding the viewport uniform.
    viewport_buffer: wgpu::Buffer,
    /// Bind group exposing the viewport uniform to the vertex stage.
    viewport_bind_group: wgpu::BindGroup,
    /// GPU vertex buffer holding per-instance sprite data.
    instance_buffer: wgpu::Buffer,
    /// Number of `SpriteInstance` slots currently allocated on the GPU.
    instance_capacity: usize,
}

impl SpriteRenderer {
    /// Initial instance-buffer capacity, in sprites. Grown on demand.
    const INITIAL_CAPACITY: usize = 256;

    /// Build the render pipeline for the given device and target format.
    ///
    /// `target_format` must match the surface/texture format the renderer
    /// will draw into.
    pub fn new(device: &wgpu::Device, target_format: wgpu::TextureFormat) -> Self {
        let shader = device.create_shader_module(wgpu::ShaderModuleDescriptor {
            label: Some("sprite shader"),
            source: wgpu::ShaderSource::Wgsl(SHADER_SOURCE.into()),
        });

        let viewport_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sprite viewport uniform"),
            size: std::mem::size_of::<ViewportUniform>() as u64,
            usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        let bind_group_layout = device.create_bind_group_layout(&wgpu::BindGroupLayoutDescriptor {
            label: Some("sprite viewport bind group layout"),
            entries: &[wgpu::BindGroupLayoutEntry {
                binding: 0,
                visibility: wgpu::ShaderStages::VERTEX,
                ty: wgpu::BindingType::Buffer {
                    ty: wgpu::BufferBindingType::Uniform,
                    has_dynamic_offset: false,
                    min_binding_size: None,
                },
                count: None,
            }],
        });

        let viewport_bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            label: Some("sprite viewport bind group"),
            layout: &bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0,
                resource: viewport_buffer.as_entire_binding(),
            }],
        });

        let pipeline_layout = device.create_pipeline_layout(&wgpu::PipelineLayoutDescriptor {
            label: Some("sprite pipeline layout"),
            bind_group_layouts: &[&bind_group_layout],
            push_constant_ranges: &[],
        });

        let instance_layout = wgpu::VertexBufferLayout {
            array_stride: std::mem::size_of::<SpriteInstance>() as u64,
            step_mode: wgpu::VertexStepMode::Instance,
            attributes: &wgpu::vertex_attr_array![
                0 => Float32x2, // position
                1 => Float32x2, // size
                2 => Float32x4, // color
            ],
        };

        let pipeline = device.create_render_pipeline(&wgpu::RenderPipelineDescriptor {
            label: Some("sprite pipeline"),
            layout: Some(&pipeline_layout),
            vertex: wgpu::VertexState {
                module: &shader,
                entry_point: Some("vs_main"),
                buffers: &[instance_layout],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            },
            fragment: Some(wgpu::FragmentState {
                module: &shader,
                entry_point: Some("fs_main"),
                targets: &[Some(wgpu::ColorTargetState {
                    format: target_format,
                    blend: Some(wgpu::BlendState::ALPHA_BLENDING),
                    write_mask: wgpu::ColorWrites::ALL,
                })],
                compilation_options: wgpu::PipelineCompilationOptions::default(),
            }),
            primitive: wgpu::PrimitiveState::default(),
            depth_stencil: None,
            multisample: wgpu::MultisampleState::default(),
            multiview: None,
            cache: None,
        });

        let instance_buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("sprite instance buffer"),
            size: (Self::INITIAL_CAPACITY * std::mem::size_of::<SpriteInstance>()) as u64,
            usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
            mapped_at_creation: false,
        });

        Self {
            pipeline,
            viewport_buffer,
            viewport_bind_group,
            instance_buffer,
            instance_capacity: Self::INITIAL_CAPACITY,
        }
    }

    /// Draw every `(Position, Sprite)` entity in `world` into `view`.
    ///
    /// `viewport_width`/`viewport_height` are the render target's size in
    /// pixels, used to map pixel coordinates to clip space.
    pub fn render(
        &mut self,
        world: &mut World,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        viewport_width: u32,
        viewport_height: u32,
    ) {
        self.render_in_viewport(
            world,
            device,
            queue,
            view,
            RenderViewport::full(viewport_width, viewport_height),
        );
    }

    /// Draw sprites within one physical region of a larger render target.
    ///
    /// The render pass still clears the complete target to transparent so UI
    /// frameworks can composite their opaque panels above it. Sprite pixels
    /// are transformed and clipped to `viewport`.
    pub fn render_in_viewport(
        &mut self,
        world: &mut World,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        viewport: RenderViewport,
    ) {
        self.render_in_viewport_with_resolution(
            world,
            device,
            queue,
            view,
            viewport,
            VirtualResolution::new(viewport.width.max(1) as f32, viewport.height.max(1) as f32),
        );
    }

    /// Draw sprites in a physical viewport using a stable logical resolution.
    ///
    /// Sprite positions and sizes are interpreted in `virtual_resolution`.
    /// wgpu then scales that complete coordinate space to fill `viewport`, and
    /// the scissor rectangle prevents pixels from escaping the dock panel.
    pub fn render_in_viewport_with_resolution(
        &mut self,
        world: &mut World,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        view: &wgpu::TextureView,
        viewport: RenderViewport,
        virtual_resolution: VirtualResolution,
    ) {
        debug_assert!(virtual_resolution.is_valid());

        // Step 1: Collect the instanced quad data for every sprite in the world.
        let instances = collect_sprite_instances(world);

        // Step 2: Upload the viewport uniform so the vertex shader can map
        // pixel coordinates into normalized device coordinates.
        queue.write_buffer(
            &self.viewport_buffer,
            0,
            bytemuck::bytes_of(&ViewportUniform {
                width: virtual_resolution.width,
                height: virtual_resolution.height,
                _padding: [0.0, 0.0],
            }),
        );

        // Step 3: Upload the instance data, growing the GPU buffer to a power
        // of two when the world grows beyond the current capacity.
        if !instances.is_empty() {
            if instances.len() > self.instance_capacity {
                self.instance_capacity = instances.len().next_power_of_two();
                self.instance_buffer =
                    device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                        label: Some("sprite instance buffer"),
                        contents: bytemuck::cast_slice(&instances),
                        usage: wgpu::BufferUsages::VERTEX | wgpu::BufferUsages::COPY_DST,
                    });
            } else {
                queue.write_buffer(&self.instance_buffer, 0, bytemuck::cast_slice(&instances));
            }
        }

        // Step 4: Encode and submit a render pass that clears the target and
        // draws the instanced quads inside the scissored viewport region.
        let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
            label: Some("sprite render encoder"),
        });

        {
            let mut render_pass = encoder.begin_render_pass(&wgpu::RenderPassDescriptor {
                label: Some("sprite render pass"),
                color_attachments: &[Some(wgpu::RenderPassColorAttachment {
                    view,
                    resolve_target: None,
                    ops: wgpu::Operations {
                        load: wgpu::LoadOp::Clear(wgpu::Color::TRANSPARENT),
                        store: wgpu::StoreOp::Store,
                    },
                    depth_slice: None,
                })],
                depth_stencil_attachment: None,
                timestamp_writes: None,
                occlusion_query_set: None,
            });

            if !instances.is_empty() && viewport.width > 0 && viewport.height > 0 {
                render_pass.set_pipeline(&self.pipeline);
                render_pass.set_viewport(
                    viewport.x as f32,
                    viewport.y as f32,
                    viewport.width as f32,
                    viewport.height as f32,
                    0.0,
                    1.0,
                );
                render_pass.set_scissor_rect(
                    viewport.x,
                    viewport.y,
                    viewport.width,
                    viewport.height,
                );
                render_pass.set_bind_group(0, &self.viewport_bind_group, &[]);
                render_pass.set_vertex_buffer(0, self.instance_buffer.slice(..));
                render_pass.draw(0..6, 0..instances.len() as u32);
            }
        }

        queue.submit(Some(encoder.finish()));
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Collect renderer components across the native project-module ABI boundary.
///
/// A hot-loaded Rust project and the host executable can assign different
/// `TypeId` values to the same `pill_engine` type. Renderer components are a
/// deliberately shared ABI, so resolve their columns by stable type name and
/// verify their C layouts instead of issuing a host-typed ECS query.
fn collect_sprite_instances(world: &World) -> Vec<SpriteInstance> {
    collect_sprite_instances_named(
        world,
        std::any::type_name::<Position>(),
        std::any::type_name::<Sprite>(),
    )
}

/// Type-erased implementation separated from the public renderer names so its
/// cross-`TypeId` behavior can be covered by an ordinary unit test.
fn collect_sprite_instances_named(
    world: &World,
    position_name: &str,
    sprite_name: &str,
) -> Vec<SpriteInstance> {
    let registry = &world.component_registry;
    let mut instances = Vec::new();

    for archetype in world.archetypes.values() {
        // Step 1: Resolve each column among the components actually present in
        // this archetype by shared type name and size. This also supports
        // entities retained from older DLL generations whose component IDs
        // differ from the current module.
        let position_id = archetype.component_types.iter().copied().find(|id| {
            registry.get_name(id) == Some(position_name)
                && registry.get_size(id) == Some(std::mem::size_of::<Position>())
        });
        let sprite_id = archetype.component_types.iter().copied().find(|id| {
            registry.get_name(id) == Some(sprite_name)
                && registry.get_size(id) == Some(std::mem::size_of::<Sprite>())
        });
        let (Some(position_id), Some(sprite_id)) = (position_id, sprite_id) else {
            continue;
        };

        // Step 2: Fetch the type-erased trait storage backing each resolved
        // component column so rows can be read without a host-typed query.
        let (Some(position_type_id), Some(sprite_type_id)) =
            (position_id.native_type_id(), sprite_id.native_type_id())
        else {
            continue;
        };
        let Some(position_storage) = archetype
            .component_storages
            .get_trait_storage(position_type_id)
        else {
            continue;
        };
        let Some(sprite_storage) = archetype
            .component_storages
            .get_trait_storage(sprite_type_id)
        else {
            continue;
        };

        // Step 3: Copy each row's layout-validated component data into an
        // instance record for the GPU.
        //
        // SAFETY: Both shared types are `#[repr(C)]` and `Copy`, and their
        // sizes were verified against this crate's `Position`/`Sprite` above,
        // so each read from the type-erased trait storage yields a valid value
        // of the target type even when its native TypeId originated in another
        // DLL. `row_count` caps the loop at every storage length, keeping the
        // `get(row)` accesses in bounds.
        let row_count = archetype
            .entity_count()
            .min(position_storage.len())
            .min(sprite_storage.len());
        for row in 0..row_count {
            let position = unsafe { read_shared_component::<Position>(position_storage.get(row)) };
            let sprite = unsafe { read_shared_component::<Sprite>(sprite_storage.get(row)) };
            instances.push(SpriteInstance {
                position: [position.x, position.y],
                size: [sprite.width, sprite.height],
                color: [
                    sprite.color.r,
                    sprite.color.g,
                    sprite.color.b,
                    sprite.color.a,
                ],
            });
        }
    }

    instances
}

/// Copy one layout-validated shared component out of type-erased storage.
///
/// # Safety
///
/// `component` must point to a value with the same `repr(C)` layout and size
/// as `T`. Callers establish this through the shared component name and size.
unsafe fn read_shared_component<T: Copy>(component: &dyn Component) -> T {
    let data = component as *const dyn Component as *const T;
    // SAFETY: Guaranteed by the caller. `read_unaligned` also avoids relying
    // on alignment information that is not present in ComponentRegistry.
    unsafe { data.read_unaligned() }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod shared_component_tests {
    use super::*;
    use trait_type_map::impl_trait_accessible;

    /// Layout-compatible stand-in with a different TypeId than Position.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ForeignPosition {
        x: f32,
        y: f32,
    }

    impl Component for ForeignPosition {}
    impl_trait_accessible!(dyn Component; ForeignPosition);

    /// Layout-compatible stand-in with a different TypeId than Sprite.
    #[repr(C)]
    #[derive(Clone, Copy)]
    struct ForeignSprite {
        width: f32,
        height: f32,
        color: Color,
    }

    impl Component for ForeignSprite {}
    impl_trait_accessible!(dyn Component; ForeignSprite);

    /// Shared renderer layouts can be read even when their native TypeIds
    /// originate from a different compilation unit.
    #[test]
    fn collects_layout_compatible_components_with_foreign_type_ids() {
        let mut world = World::new();
        world.register_component::<ForeignPosition>();
        world.register_component::<ForeignSprite>();
        world
            .create_entity()
            .with(ForeignPosition { x: 12.0, y: 34.0 })
            .with(ForeignSprite {
                width: 56.0,
                height: 78.0,
                color: Color::new(0.1, 0.2, 0.3, 0.4),
            })
            .build()
            .unwrap();

        let instances = collect_sprite_instances_named(
            &world,
            std::any::type_name::<ForeignPosition>(),
            std::any::type_name::<ForeignSprite>(),
        );

        assert_eq!(instances.len(), 1);
        assert_eq!(instances[0].position, [12.0, 34.0]);
        assert_eq!(instances[0].size, [56.0, 78.0]);
        assert_eq!(instances[0].color, [0.1, 0.2, 0.3, 0.4]);
    }

    /// Projection dimensions must be finite and strictly positive.
    #[test]
    fn virtual_resolution_rejects_invalid_projection_dimensions() {
        assert!(VirtualResolution::new(800.0, 600.0).is_valid());
        assert!(!VirtualResolution::new(0.0, 600.0).is_valid());
        assert!(!VirtualResolution::new(800.0, f32::NAN).is_valid());
        assert!(!VirtualResolution::new(f32::INFINITY, 600.0).is_valid());
    }
}
