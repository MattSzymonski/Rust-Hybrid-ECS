//! The camera values the shaders read, and the GPU resources that carry them.
//!
//! # Responsibilities
//!
//! - Pack the camera position and the view-projection matrix into the layout
//!   the shaders' `CameraParams` constant buffer expects
//!   ([`CameraParametersData`]).
//! - Replace projection inputs no perspective matrix can use - a field of view
//!   outside `(0, 180)`, a non-positive near plane, a far plane at or inside
//!   the near one - and name each replacement, because a NaN matrix produces a
//!   blank frame with nothing to trace.
//! - Own the uniform buffer and the bind group that every shader reading the
//!   camera binds ([`RendererCamera`]).
//!
//! # Design
//!
//! The value and the GPU objects are separate: [`CameraParametersData`] is the
//! `#[repr(C)]` layout the shaders read, [`RendererCamera`] the buffer and bind
//! group that hold it. The matrices are rebuilt from the camera and transform
//! components on every update rather than tracked, so a moved entity is always
//! followed; the projection inputs are sanitized on the same pass, and the
//! result names what was replaced. Substitutions are reported once per change
//! rather than once per frame, so a value the game never fixes does not flood
//! the log.

// External crates
use pill_core::math::{Matrix4f, Vector3f, Vector4f};
use wgpu::util::DeviceExt;

// Current crate
use crate::{
    component::{CameraComponent, TransformComponent},
    error::Result,
};

// --- Camera Uniform ---

/// The camera values the shaders read, in the layout of the HLSL
/// `CameraParams` constant buffer.
///
/// The two sides are one layout written twice: a field changed here has to be
/// changed in `shaders/include/common.hlsl` as well, or the shader reads the
/// next value from wherever the padding lands.
#[repr(C)]
#[derive(Debug, Copy, Clone, bytemuck::Pod, bytemuck::Zeroable)]
pub struct CameraParametersData {
    /// Camera position in world space.
    ///
    /// The shader declares three components; the fourth slot is kept at zero
    /// because it is the padding that puts the matrix where both sides expect
    /// it.
    pub position: Vector4f,
    /// View and projection combined, applied to world-space positions by the
    /// vertex shader.
    pub view_projection_matrix: Matrix4f,
}

impl Default for CameraParametersData {
    fn default() -> Self {
        Self::new()
    }
}

impl CameraParametersData {
    /// The zero value: the camera at the origin with an identity
    /// view-projection, replaced on the first update.
    ///
    /// `Default` goes through here, so both ways of building the parameters
    /// agree on what "empty" means.
    pub fn new() -> Self {
        Self {
            position: Vector4f::ZERO,
            view_projection_matrix: Matrix4f::IDENTITY,
        }
    }

    /// Rebuilds the uniform values from the camera and transform components.
    ///
    /// `aspect` is the drawing viewport's width over its height, passed in
    /// rather than read here so the projection follows the viewport the
    /// renderer actually draws into. Returns the description of any projection
    /// input that had to be replaced, or `None` when the component's values
    /// were all usable.
    pub fn update_data(
        &mut self,
        camera_component: &CameraComponent,
        transform_component: &TransformComponent,
        aspect: f32,
    ) -> Option<String> {
        // Update position
        self.position = Vector4f::new(
            transform_component.translation[0],
            transform_component.translation[1],
            transform_component.translation[2],
            0.0,
        );

        // Update view-projection. The projection inputs are sanitized first:
        // an impossible field of view or near/far pair would otherwise become
        // infinities and NaNs in the matrix, which paint nothing and leave no
        // error to look at.
        let (vertical_fov, near, far, anomaly) = sanitized_projection(camera_component);
        self.view_projection_matrix =
            CameraParametersData::calculate_projection_matrix(vertical_fov, near, far, aspect)
                * CameraParametersData::calculate_view_matrix(transform_component);
        anomaly
    }

    /// Builds the view matrix looking from the transform's position along its
    /// forward axis.
    ///
    /// The rotation decides where forward points, and a degenerate one would
    /// spread NaNs through every basis vector of the matrix; the fallback
    /// keeps the camera at a finite pose instead.
    fn calculate_view_matrix(transform_component: &TransformComponent) -> Matrix4f {
        let position = Vector3f::from_array(transform_component.translation);
        let rotation = glam::Quat::from_array(transform_component.rotation);
        // Only a finite, non-zero quaternion normalizes into a usable basis;
        // anything else would decide the view direction with NaNs.
        let rotation = if rotation.is_finite() && rotation.length_squared() > 1e-8 {
            rotation.normalize()
        } else {
            glam::Quat::IDENTITY
        };
        glam::camera::rh::view::look_to_mat4(
            position,
            rotation * Vector3f::NEG_Z,
            rotation * Vector3f::Y,
        )
    }

    /// Builds the perspective matrix for the 0-to-1 depth range wgpu uses.
    ///
    /// The DirectX convention matches that range, which is why glam is asked
    /// for it rather than for the OpenGL one. The inputs arrive already
    /// sanitized, so `perspective` never divides by a zero angle or a
    /// non-positive plane distance.
    fn calculate_projection_matrix(
        vertical_fov: f32,
        near: f32,
        far: f32,
        aspect: f32,
    ) -> Matrix4f {
        glam::camera::rh::proj::directx::perspective(vertical_fov.to_radians(), aspect, near, far)
    }
}

/// A projection a perspective matrix can actually use, with a reason.
///
/// `perspective` divides by `tan(fov / 2)` and by `far - near`, so a field of
/// view of zero, a non-positive near plane, or a far plane at or inside the
/// near one produces infinities and NaNs - a frame that paints nothing with no
/// error to show for it. Each unusable value is replaced with the component's
/// default (the far plane relative to the near one, because a world may
/// legitimately use a near plane well past the default far plane), and the
/// substitutions come back named so the caller can say what it did.
fn sanitized_projection(camera_component: &CameraComponent) -> (f32, f32, f32, Option<String>) {
    let mut reasons = Vec::new();
    let default = CameraComponent::default();

    let vertical_fov = if camera_component.vertical_fov.is_finite()
        && camera_component.vertical_fov > 0.0
        && camera_component.vertical_fov < 180.0
    {
        camera_component.vertical_fov
    } else {
        reasons.push(format!(
            "vertical_fov {} is outside (0, 180)",
            camera_component.vertical_fov
        ));
        default.vertical_fov
    };

    let near = if camera_component.near.is_finite() && camera_component.near > 0.0 {
        camera_component.near
    } else {
        reasons.push(format!("near {} is not positive", camera_component.near));
        default.near
    };

    let far = if camera_component.far.is_finite() && camera_component.far > near {
        camera_component.far
    } else {
        reasons.push(format!(
            "far {} is not beyond near {near}",
            camera_component.far
        ));
        near + default.far
    };

    (
        vertical_fov,
        near,
        far,
        (!reasons.is_empty()).then(|| reasons.join("; ")),
    )
}

// --- Camera ---

/// The GPU-side resources of one camera: its uniform values, the buffer they
/// are written to, and the bind group that exposes the buffer to shaders.
///
/// Built once and then only updated in place: [`RendererCamera::update`]
/// rewrites the buffer's contents each frame, so moving the camera costs one
/// buffer write and no bind group rebuild.
#[derive(Debug)]
pub struct RendererCamera {
    /// The values for the current frame, rewritten by
    /// [`RendererCamera::update`] before they are written to the buffer.
    pub parameters_data: CameraParametersData,
    /// Uniform buffer holding `parameters_data`.
    pub parameters_uniform_buffer: wgpu::Buffer,
    /// Layout the bind group was created from, kept alongside it.
    pub bind_group_layout: wgpu::BindGroupLayout,
    /// Bound at group 1 by every shader that reads the camera.
    pub bind_group: wgpu::BindGroup,
    /// Whether the last update reported unusable projection values, so the
    /// warning fires once and then stays quiet until the values become usable
    /// again, instead of once per frame.
    projection_reported: bool,
}

impl RendererCamera {
    /// Creates a camera on `device`, with its bind group built from
    /// `camera_bind_group_layout`.
    ///
    /// The layout is the one the renderer creates at startup and hands to
    /// both the camera and every camera-reading shader, which is what makes
    /// the bind group compatible with those pipelines.
    ///
    /// # Errors
    ///
    /// Never returns an error: buffer and bind group creation are wgpu calls
    /// that report nothing to check, and `Ok` is the only value produced. The
    /// `Result` keeps the call site uniform with the renderer's other resource
    /// constructors.
    pub fn new(
        device: &wgpu::Device,
        camera_bind_group_layout: wgpu::BindGroupLayout,
    ) -> Result<Self> {
        let parameters_data = CameraParametersData::new();

        let parameters_uniform_buffer =
            device.create_buffer_init(&wgpu::util::BufferInitDescriptor {
                label: Some("camera_parameters_buffer"),
                contents: bytemuck::cast_slice(&[parameters_data]),
                usage: wgpu::BufferUsages::UNIFORM | wgpu::BufferUsages::COPY_DST,
            });

        let bind_group = device.create_bind_group(&wgpu::BindGroupDescriptor {
            layout: &camera_bind_group_layout,
            entries: &[wgpu::BindGroupEntry {
                binding: 0, // (set = X, binding = 0)
                resource: parameters_uniform_buffer.as_entire_binding(),
            }],
            label: Some("camera_parameters_bind_group"),
        });

        let camera = Self {
            parameters_data,
            parameters_uniform_buffer,
            bind_group_layout: camera_bind_group_layout,
            bind_group,
            projection_reported: false,
        };

        Ok(camera)
    }

    /// Uploads the camera's values for this frame to the uniform buffer.
    ///
    /// Called once per frame with the drawing viewport's aspect ratio. An
    /// unusable projection value is reported when it first appears rather than
    /// on every frame it persists, so a broken camera produces one warning,
    /// not a stream of them.
    pub fn update(
        &mut self,
        queue: &wgpu::Queue,
        camera_component: &CameraComponent,
        transform_component: &TransformComponent,
        aspect: f32,
    ) {
        let anomaly =
            self.parameters_data
                .update_data(camera_component, transform_component, aspect);
        match anomaly {
            Some(reason) if !self.projection_reported => {
                pill_core::warn!(
                    target: pill_core::telemetry::telemetry_target::RENDERING,
                    "camera projection has unusable values ({reason}); the renderer substitutes defaults"
                );
                self.projection_reported = true;
            }
            None => self.projection_reported = false,
            Some(_) => {}
        }
        queue.write_buffer(
            &self.parameters_uniform_buffer,
            0,
            bytemuck::cast_slice(&[self.parameters_data]),
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usable_projection_values_come_back_unchanged() {
        let camera = CameraComponent {
            vertical_fov: 45.0,
            near: 0.5,
            far: 250.0,
            ..CameraComponent::default()
        };

        let (vertical_fov, near, far, anomaly) = sanitized_projection(&camera);

        assert_eq!((vertical_fov, near, far), (45.0, 0.5, 250.0));
        assert!(anomaly.is_none());
    }

    #[test]
    fn impossible_projection_values_are_replaced_and_named() {
        let camera = CameraComponent {
            vertical_fov: 0.0,
            near: -1.0,
            far: f32::NAN,
            ..CameraComponent::default()
        };

        let (vertical_fov, near, far, anomaly) = sanitized_projection(&camera);

        assert_eq!(vertical_fov, 60.0);
        assert_eq!(near, 0.1);
        assert!(far > near && far.is_finite());
        let anomaly = anomaly.expect("every replaced value is named");
        assert!(anomaly.contains("vertical_fov"));
        assert!(anomaly.contains("near"));
        assert!(anomaly.contains("far"));
    }

    #[test]
    fn a_far_plane_inside_the_near_plane_is_pushed_past_it() {
        let camera = CameraComponent {
            near: 10.0,
            far: 5.0,
            ..CameraComponent::default()
        };

        let (_, near, far, anomaly) = sanitized_projection(&camera);

        assert_eq!(near, 10.0);
        assert!(far > near);
        assert!(anomaly.is_some_and(|reason| reason.contains("far")));
    }
}
