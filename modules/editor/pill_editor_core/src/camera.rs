//! The scene viewports' orbit cameras and the mouse gestures that drive them.
//!
//! # Responsibilities
//!
//! - [`OrbitCamera`]: one scene view's view state - the pivot it orbits, its
//!   yaw, pitch and distance - and the eye and basis derived from them.
//! - The four gestures a camera answers - [`OrbitCamera::orbit`],
//!   [`OrbitCamera::pan`], [`OrbitCamera::dolly`] and [`OrbitCamera::zoom`]:
//!   orbit stops the pitch short of the poles, and dolly and zoom keep the
//!   distance inside its range.
//! - [`navigation_binding`]: which gesture a mouse button and the keyboard
//!   modifiers name under a keymap preset - Blender's by default.
//!
//! # Design
//!
//! Plain arrays and scalars: the executable owns the pointer and the
//! surface, and a camera's numbers are testable with no window. The presets
//! are a table rather than code inside the event handler, so a preset is one
//! row here and the preferences file only has to name it. A viewport's camera
//! lives beside its window (the scene views navigate independently), and only
//! the *active* viewport's camera receives a gesture.

// Current crate
use crate::commands::KeymapPreset;

/// The closest the eye may come to its pivot, in world units: any nearer and
/// the near plane would clip the pivot itself.
pub const MIN_DISTANCE: f32 = 0.25;

/// The farthest the eye may go from its pivot, in world units.
pub const MAX_DISTANCE: f32 = 2_000.0;

/// The pitch stops just short of the poles: at exactly +-90 degrees the
/// camera's right vector has no horizontal component left, and the yaw would
/// spin under the pointer.
pub const MAX_PITCH: f32 = std::f32::consts::FRAC_PI_2 - 0.02;

/// How far one pixel of an orbit drag turns the view, in radians.
pub const ORBIT_RADIANS_PER_PIXEL: f32 = 0.008;

/// How fast one pixel of a dolly drag changes the distance: the distance
/// scales by `2^(pixels * this)`, so a dolly feels the same at any distance.
pub const DOLLY_EXPONENT_PER_PIXEL: f32 = 0.006;

/// How much one wheel line moves the eye, as a distance ratio.
pub const ZOOM_RATIO_PER_LINE: f32 = 1.15;

/// The pitch the default camera looks down from, in radians.
const DEFAULT_PITCH: f32 = 0.30;

/// The distance the default camera keeps from its pivot, in world units.
const DEFAULT_DISTANCE: f32 = 16.0;

/// One scene viewport's orbit camera.
///
/// The camera never enters the world: the scene surface is bound to it as a
/// view camera, so project systems can neither see nor move it. Each scene
/// view keeps its own, which is what makes navigation "the active
/// viewport's" rather than the editor's.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OrbitCamera {
    /// The point the camera looks at and orbits around.
    pub pivot: [f32; 3],
    /// Rotation around the world's Y axis, in radians.
    pub yaw: f32,
    /// Downward tilt, in radians; positive looks down at the pivot.
    pub pitch: f32,
    /// Distance from the pivot.
    pub distance: f32,
}

impl Default for OrbitCamera {
    fn default() -> Self {
        Self {
            pivot: [0.0, 0.0, 0.0],
            yaw: 0.0,
            pitch: DEFAULT_PITCH,
            distance: DEFAULT_DISTANCE,
        }
    }
}

impl OrbitCamera {
    /// The direction the camera looks along.
    pub fn forward(&self) -> [f32; 3] {
        let (sin_yaw, cos_yaw) = self.yaw.sin_cos();
        let (sin_pitch, cos_pitch) = self.pitch.sin_cos();
        [-cos_pitch * sin_yaw, -sin_pitch, -cos_pitch * cos_yaw]
    }

    /// The camera's right direction, horizontal by construction: the gizmo's
    /// screen-space rays are built from it.
    pub fn right(&self) -> [f32; 3] {
        let forward = self.forward();
        let horizontal = (forward[0] * forward[0] + forward[2] * forward[2]).sqrt();
        if horizontal <= f32::EPSILON {
            return [1.0, 0.0, 0.0];
        }
        [-forward[2] / horizontal, 0.0, forward[0] / horizontal]
    }

    /// The camera's up direction: the plane's vertical, perpendicular to the
    /// look and right directions.
    pub fn up(&self) -> [f32; 3] {
        [
            self.right()[1] * self.forward()[2] - self.right()[2] * self.forward()[1],
            self.right()[2] * self.forward()[0] - self.right()[0] * self.forward()[2],
            self.right()[0] * self.forward()[1] - self.right()[1] * self.forward()[0],
        ]
    }

    /// Where the camera sits: the pivot, backed off along the look
    /// direction.
    pub fn eye(&self) -> [f32; 3] {
        let forward = self.forward();
        [
            self.pivot[0] - forward[0] * self.distance,
            self.pivot[1] - forward[1] * self.distance,
            self.pivot[2] - forward[2] * self.distance,
        ]
    }

    /// The camera's orientation as a quaternion built from yaw and pitch, in
    /// the transform's `[x, y, z, w]` layout.
    pub fn rotation(&self) -> [f32; 4] {
        let half_yaw = self.yaw * 0.5;
        let half_pitch = self.pitch * 0.5;
        let yaw_quaternion = [0.0, half_yaw.sin(), 0.0, half_yaw.cos()];
        let pitch_quaternion = [(-half_pitch).sin(), 0.0, 0.0, half_pitch.cos()];
        quaternion_product(yaw_quaternion, pitch_quaternion)
    }

    /// Orbit around the pivot by a drag's pixel delta: dragging right turns
    /// the view right, dragging down lifts it over the pivot.
    pub fn orbit(&mut self, delta_x: f32, delta_y: f32) {
        self.yaw -= delta_x * ORBIT_RADIANS_PER_PIXEL;
        self.pitch = (self.pitch + delta_y * ORBIT_RADIANS_PER_PIXEL).clamp(-MAX_PITCH, MAX_PITCH);
    }

    /// Pan the pivot with the view: the world follows the drag, so the pivot
    /// moves the other way. `world_per_pixel` is the height of one pixel at
    /// the pivot's depth, which the caller derives from the distance and the
    /// field of view.
    pub fn pan(&mut self, delta_x: f32, delta_y: f32, world_per_pixel: f32) {
        let right = self.right();
        let up = self.up();
        for index in 0..3 {
            self.pivot[index] += (up[index] * delta_y - right[index] * delta_x) * world_per_pixel;
        }
    }

    /// Dolly the eye along the view by a drag's pixel delta: dragging up
    /// moves closer, dragging down farther, at a rate that feels the same at
    /// any distance.
    pub fn dolly(&mut self, delta_y: f32) {
        let factor = 2.0f32.powf(delta_y * DOLLY_EXPONENT_PER_PIXEL);
        self.distance = (self.distance * factor).clamp(MIN_DISTANCE, MAX_DISTANCE);
    }

    /// Zoom by a wheel's line count: a positive count (wheel up) moves the
    /// eye closer, a negative one farther.
    pub fn zoom(&mut self, lines: f32) {
        let factor = ZOOM_RATIO_PER_LINE.powf(-lines);
        self.distance = (self.distance * factor).clamp(MIN_DISTANCE, MAX_DISTANCE);
    }
}

/// The mouse buttons a navigation gesture can bind to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CameraButton {
    /// The left button.
    Left,
    /// The middle button.
    Middle,
    /// The right button.
    Right,
}

/// The keyboard modifiers held during a press, as plain data: the event
/// handlers carry winit's modifiers, the preset table wants flags.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Modifiers {
    /// Shift held.
    pub shift: bool,
    /// Control held.
    pub control: bool,
    /// Alt held.
    pub alt: bool,
}

/// One camera navigation gesture.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CameraNavigation {
    /// Turn around the pivot.
    Orbit,
    /// Slide the pivot in the view's plane.
    Pan,
    /// Move the eye along the view.
    Dolly,
}

/// The gesture a press means under `preset`, or `None` when it is no
/// navigation chord (the press belongs to selection or the gizmo).
///
/// Blender and Godot orbit with the middle button and pan and dolly with
/// shift and control held; Unity and Unreal orbit with Alt and the left
/// button. Any other preset falls back to Blender's table.
pub fn navigation_binding(
    preset: KeymapPreset,
    button: CameraButton,
    modifiers: Modifiers,
) -> Option<CameraNavigation> {
    match preset {
        KeymapPreset::Unity | KeymapPreset::Unreal => match (button, modifiers.alt) {
            (CameraButton::Left, true) => Some(CameraNavigation::Orbit),
            (CameraButton::Middle, true) => Some(CameraNavigation::Pan),
            (CameraButton::Right, true) => Some(CameraNavigation::Dolly),
            _ => None,
        },
        _ => middle_button_table(button, modifiers),
    }
}

/// Blender's middle-button navigation, which Godot shares: the plain drag
/// orbits, shift pans, control dollies.
fn middle_button_table(button: CameraButton, modifiers: Modifiers) -> Option<CameraNavigation> {
    if button != CameraButton::Middle || modifiers.alt {
        return None;
    }
    match (modifiers.shift, modifiers.control) {
        (false, false) => Some(CameraNavigation::Orbit),
        (true, false) => Some(CameraNavigation::Pan),
        (false, true) => Some(CameraNavigation::Dolly),
        // Both held: no chord, so a stray press does nothing.
        (true, true) => None,
    }
}

/// The Hamilton product of two `[x, y, z, w]` quaternions.
fn quaternion_product(left: [f32; 4], right: [f32; 4]) -> [f32; 4] {
    let [left_x, left_y, left_z, left_w] = left;
    let [right_x, right_y, right_z, right_w] = right;
    [
        left_w * right_x + left_x * right_w + left_y * right_z - left_z * right_y,
        left_w * right_y - left_x * right_z + left_y * right_w + left_z * right_x,
        left_w * right_z + left_x * right_y - left_y * right_x + left_z * right_w,
        left_w * right_w - left_x * right_x - left_y * right_y - left_z * right_z,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    fn close(left: f32, right: f32) -> bool {
        (left - right).abs() < 1e-5
    }

    #[test]
    fn the_default_camera_looks_down_at_its_pivot() {
        let camera = OrbitCamera::default();
        let forward = camera.forward();
        assert!(forward[1] < 0.0, "the default looks down: {forward:?}");
        assert!(close(
            camera.eye()[1] - camera.pivot[1],
            DEFAULT_PITCH.sin() * camera.distance
        ));
        // The eye sits one distance from the pivot, on the far side of the
        // forward direction.
        let offset = [
            camera.eye()[0] - camera.pivot[0],
            camera.eye()[1] - camera.pivot[1],
            camera.eye()[2] - camera.pivot[2],
        ];
        let length = (offset[0] * offset[0] + offset[1] * offset[1] + offset[2] * offset[2]).sqrt();
        assert!(close(length, camera.distance), "eye distance {length}");
    }

    #[test]
    fn orbiting_turns_the_view_and_stops_at_the_poles() {
        let mut camera = OrbitCamera::default();
        let before = camera.forward();
        camera.orbit(40.0, 0.0);
        assert!(!close(camera.forward()[0], before[0]), "yaw moved the view");
        // The direction a horizontal drag turns the view is part of the
        // contract: a drag to the right turns the view to the right, which
        // swings the forward vector toward +X from a camera looking down -Z.
        assert!(camera.yaw < 0.0, "a rightward drag turned the view left");
        assert!(
            camera.forward()[0] > 0.0,
            "the view turned toward -X on a rightward drag"
        );
        // A drag far past the pole clamps instead of flipping.
        camera.orbit(0.0, 100_000.0);
        assert!(close(camera.pitch, MAX_PITCH));
        camera.orbit(0.0, -1_000_000.0);
        assert!(close(camera.pitch, -MAX_PITCH));
    }

    #[test]
    fn panning_moves_the_pivot_in_the_view_plane() {
        let mut camera = OrbitCamera {
            yaw: 0.0,
            pitch: 0.0,
            ..OrbitCamera::default()
        };
        // At pitch 0 the view looks along -Z, so right is +X and up is +Y.
        camera.pan(10.0, 0.0, 0.5);
        assert!(
            close(camera.pivot[0], -5.0),
            "drag right moves left: {:?}",
            camera.pivot
        );
        camera.pan(0.0, 10.0, 0.5);
        assert!(
            close(camera.pivot[1], 5.0),
            "drag down moves up: {:?}",
            camera.pivot
        );
        assert!(close(camera.pivot[2], 0.0), "pan stays in the view plane");
    }

    #[test]
    fn dolly_and_zoom_clamp_their_distances() {
        let mut camera = OrbitCamera::default();
        camera.dolly(100.0);
        assert!(camera.distance > 16.0, "dragging down backs off");
        camera.dolly(1_000_000.0);
        assert!(close(camera.distance, MAX_DISTANCE));
        camera.dolly(-1_000_000.0);
        assert!(close(camera.distance, MIN_DISTANCE));
        camera.zoom(1.0);
        assert!(
            camera.distance < MIN_DISTANCE * 1.5,
            "wheel up comes closer"
        );
        camera.zoom(-100.0);
        assert!(close(camera.distance, MAX_DISTANCE));
    }

    #[test]
    fn blender_navigation_is_the_default_table() {
        let modifiers = Modifiers::default();
        assert_eq!(
            navigation_binding(KeymapPreset::Blender, CameraButton::Middle, modifiers),
            Some(CameraNavigation::Orbit)
        );
        assert_eq!(
            navigation_binding(
                KeymapPreset::Blender,
                CameraButton::Middle,
                Modifiers {
                    shift: true,
                    ..Modifiers::default()
                }
            ),
            Some(CameraNavigation::Pan)
        );
        assert_eq!(
            navigation_binding(
                KeymapPreset::Blender,
                CameraButton::Middle,
                Modifiers {
                    control: true,
                    ..Modifiers::default()
                }
            ),
            Some(CameraNavigation::Dolly)
        );
        // Alt means "left-button navigation" in the presets that use it, so a
        // middle-button drag with Alt held navigates nothing - the guard that
        // used to be spelled as a rebinding of `modifiers`.
        assert_eq!(
            navigation_binding(
                KeymapPreset::Blender,
                CameraButton::Middle,
                Modifiers {
                    alt: true,
                    ..Modifiers::default()
                }
            ),
            None
        );
        // The left button is selection's, whatever the modifiers.
        assert_eq!(
            navigation_binding(KeymapPreset::Blender, CameraButton::Left, modifiers),
            None
        );
        // Godot shares Blender's middle-button table.
        assert_eq!(
            navigation_binding(KeymapPreset::Godot, CameraButton::Middle, modifiers),
            Some(CameraNavigation::Orbit)
        );
    }

    #[test]
    fn unity_navigation_uses_alt_and_the_left_button() {
        let alt = Modifiers {
            alt: true,
            ..Modifiers::default()
        };
        assert_eq!(
            navigation_binding(KeymapPreset::Unity, CameraButton::Left, alt),
            Some(CameraNavigation::Orbit)
        );
        assert_eq!(
            navigation_binding(KeymapPreset::Unity, CameraButton::Middle, alt),
            Some(CameraNavigation::Pan)
        );
        assert_eq!(
            navigation_binding(KeymapPreset::Unity, CameraButton::Right, alt),
            Some(CameraNavigation::Dolly)
        );
        assert_eq!(
            navigation_binding(
                KeymapPreset::Unity,
                CameraButton::Middle,
                Modifiers::default()
            ),
            None
        );
    }
}
