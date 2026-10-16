//! Transform gizmos: handle geometry, drag math and snapping (Stage 20).
//!
//! # Responsibilities
//!
//! - Name the tool's modes ([`GizmoMode`]: translate, rotate, scale) and
//!   spaces ([`GizmoSpace`]: world, local), and the settings a toolbar edits
//!   ([`GizmoSettings`]).
//! - Build the handle geometry a renderer draws and picks, one handle id per
//!   axis of the active mode ([`build_geometry`]).
//! - The drag math: project a cursor ray onto the dragged axis or rotation
//!   plane, and snap the result to the configured step.
//!
//! # Design
//!
//! Pure functions over plain arrays. The geometry and the math live here and
//! the editor executable owns the camera and the world, so a drag is testable
//! without a renderer or a window - the acceptance test drives the same
//! functions with a headless contract stub. Handle ids are stable numbers
//! (`1..=3` translate, `11..=13` rotate, `21..=23` scale), so a pick's
//! `Handle(id)` names both the mode it belongs to and its axis
//! ([`handle_axis`]), and the renderer never needs to know what a handle
//! means.

// External crates
use pill_engine::FieldValue;
use serde::{Deserialize, Serialize};

// =============================================================================
// State
// =============================================================================

/// The gizmo tool's live state: the settings the editor draws and drags with.
///
/// A world resource rather than shell state, because the executable owns the
/// drag - it holds the camera and the pick answers - while the shell owns the
/// commands and the preferences file. Both sides meet here, so a reload of
/// either keeps the other's view, and the draw, the pick and the drag can
/// never disagree about which handles are on screen.
#[derive(Clone, Copy, Debug, Default)]
pub struct GizmoState {
    /// What the tool edits, in which space, and how it snaps.
    pub settings: GizmoSettings,
}

impl pill_engine::Resource for GizmoState {
    fn shared_name() -> Option<&'static str> {
        Some(RESOURCE_NAME)
    }
}

// =============================================================================
// Resource fields (Stage 25)
// =============================================================================

/// The name [`GizmoState`] answers to in `SetResourceField` - the same string
/// the resource registers under, so a command written by hand finds it.
pub const RESOURCE_NAME: &str = "pill_editor_core::gizmo::GizmoState";

/// Set one field of the gizmo settings from a field value, and return the
/// previous value for the command's inverse.
///
/// The engine keeps no resource reflection, so the writable fields are named
/// here, one arm each; [`resource_field_value`] reads them back through the
/// same table. An enum field travels as its `U32` discriminant, in
/// declaration order, and a step as `F32`.
pub fn set_resource_field(
    settings: &mut GizmoSettings,
    resource: &str,
    field: &str,
    value: &FieldValue,
) -> Result<FieldValue, String> {
    check_resource(resource)?;
    match field {
        "settings.mode" => {
            let previous = settings.mode;
            settings.mode = mode_from_index(field_u32(field, value)?)?;
            Ok(FieldValue::U32(mode_index(previous)))
        }
        "settings.space" => {
            let previous = settings.space;
            settings.space = space_from_index(field_u32(field, value)?)?;
            Ok(FieldValue::U32(space_index(previous)))
        }
        "settings.snap" => {
            let previous = settings.snap;
            settings.snap = match value {
                FieldValue::Bool(value) => *value,
                other => return Err(format!("`{field}` expects a Bool, got {other:?}")),
            };
            Ok(FieldValue::Bool(previous))
        }
        "settings.translate_step" => swap_f32(&mut settings.translate_step, field, value),
        "settings.rotate_step_degrees" => swap_f32(&mut settings.rotate_step_degrees, field, value),
        "settings.scale_step" => swap_f32(&mut settings.scale_step, field, value),
        _ => Err(format!(
            "`{field}` is not an editable field of `{RESOURCE_NAME}` (settings.mode, \
             settings.space, settings.snap, settings.translate_step, \
             settings.rotate_step_degrees, settings.scale_step)"
        )),
    }
}

/// Read one editable field of the gizmo settings: the reading half of the
/// table [`set_resource_field`] writes through.
pub fn resource_field_value(
    settings: &GizmoSettings,
    resource: &str,
    field: &str,
) -> Result<FieldValue, String> {
    check_resource(resource)?;
    match field {
        "settings.mode" => Ok(FieldValue::U32(mode_index(settings.mode))),
        "settings.space" => Ok(FieldValue::U32(space_index(settings.space))),
        "settings.snap" => Ok(FieldValue::Bool(settings.snap)),
        "settings.translate_step" => Ok(FieldValue::F32(settings.translate_step)),
        "settings.rotate_step_degrees" => Ok(FieldValue::F32(settings.rotate_step_degrees)),
        "settings.scale_step" => Ok(FieldValue::F32(settings.scale_step)),
        _ => Err(format!(
            "`{field}` is not a readable field of `{RESOURCE_NAME}`"
        )),
    }
}

/// Refuse any resource but the gizmo state, naming it in the message.
fn check_resource(resource: &str) -> Result<(), String> {
    if resource == RESOURCE_NAME {
        Ok(())
    } else {
        Err(format!(
            "`{resource}` is not an editable resource (`{RESOURCE_NAME}` is)"
        ))
    }
}

/// A `U32` field value, accepting the wider unsigned forms too.
fn field_u32(field: &str, value: &FieldValue) -> Result<u32, String> {
    match value {
        FieldValue::U32(value) => Ok(*value),
        FieldValue::U64(value) => {
            u32::try_from(*value).map_err(|_| format!("`{field}` does not fit a u32"))
        }
        other => Err(format!("`{field}` expects a U32, got {other:?}")),
    }
}

/// Swap one `f32` field and hand back its previous value.
fn swap_f32(slot: &mut f32, field: &str, value: &FieldValue) -> Result<FieldValue, String> {
    let next = match value {
        FieldValue::F32(value) => *value,
        FieldValue::F64(value) => *value as f32,
        other => return Err(format!("`{field}` expects an F32, got {other:?}")),
    };
    let previous = *slot;
    *slot = next;
    Ok(FieldValue::F32(previous))
}

/// The `U32` discriminant of a mode, in declaration order.
fn mode_index(mode: GizmoMode) -> u32 {
    match mode {
        GizmoMode::Translate => 0,
        GizmoMode::Rotate => 1,
        GizmoMode::Scale => 2,
    }
}

/// The mode a discriminant names.
fn mode_from_index(index: u32) -> Result<GizmoMode, String> {
    match index {
        0 => Ok(GizmoMode::Translate),
        1 => Ok(GizmoMode::Rotate),
        2 => Ok(GizmoMode::Scale),
        _ => Err("`settings.mode` is 0 (translate), 1 (rotate) or 2 (scale)".to_owned()),
    }
}

/// The `U32` discriminant of a space, in declaration order.
fn space_index(space: GizmoSpace) -> u32 {
    match space {
        GizmoSpace::World => 0,
        GizmoSpace::Local => 1,
    }
}

/// The space a discriminant names.
fn space_from_index(index: u32) -> Result<GizmoSpace, String> {
    match index {
        0 => Ok(GizmoSpace::World),
        1 => Ok(GizmoSpace::Local),
        _ => Err("`settings.space` is 0 (world) or 1 (local)".to_owned()),
    }
}

// =============================================================================
// Settings
// =============================================================================

/// Which transform a gizmo edits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GizmoMode {
    /// Move the entity along one axis.
    Translate,
    /// Turn the entity around one axis.
    Rotate,
    /// Scale the entity along one axis.
    Scale,
}

/// What "the axes" mean: the world's, or the entity's own.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GizmoSpace {
    /// The world's X/Y/Z.
    World,
    /// The entity's rotation applied to those axes.
    Local,
}

/// The tool's state: what it edits, in which space, and how it snaps.
///
/// Lives in the preferences file (D31: snapping steps included), so a step is
/// editable by hand and the panel and the file never disagree.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GizmoSettings {
    /// The active mode.
    #[serde(default = "default_mode")]
    pub mode: GizmoMode,
    /// Whether the handles align with the entity's own orientation.
    #[serde(default = "default_space")]
    pub space: GizmoSpace,
    /// Whether dragging snaps.
    #[serde(default)]
    pub snap: bool,
    /// Translate snapping step, in world units.
    #[serde(default = "default_translate_step")]
    pub translate_step: f32,
    /// Rotate snapping step, in degrees.
    #[serde(default = "default_rotate_step")]
    pub rotate_step_degrees: f32,
    /// Scale snapping step, as a factor increment.
    #[serde(default = "default_scale_step")]
    pub scale_step: f32,
}

fn default_mode() -> GizmoMode {
    GizmoMode::Translate
}

fn default_space() -> GizmoSpace {
    GizmoSpace::World
}

fn default_translate_step() -> f32 {
    0.5
}

fn default_rotate_step() -> f32 {
    15.0
}

fn default_scale_step() -> f32 {
    0.1
}

impl Default for GizmoSettings {
    fn default() -> Self {
        Self {
            mode: default_mode(),
            space: default_space(),
            snap: false,
            translate_step: default_translate_step(),
            rotate_step_degrees: default_rotate_step(),
            scale_step: default_scale_step(),
        }
    }
}

impl GizmoSettings {
    /// The snapping step that belongs to `mode`.
    pub fn step_for(&self, mode: GizmoMode) -> f32 {
        match mode {
            GizmoMode::Translate => self.translate_step,
            GizmoMode::Rotate => self.rotate_step_degrees,
            GizmoMode::Scale => self.scale_step,
        }
    }
}

// =============================================================================
// Handle ids
// =============================================================================

/// The handle one axis of a mode is pickable by.
pub const fn axis_handle(mode: GizmoMode, axis: usize) -> u32 {
    match mode {
        GizmoMode::Translate => 1 + axis as u32,
        GizmoMode::Rotate => 11 + axis as u32,
        GizmoMode::Scale => 21 + axis as u32,
    }
}

/// Which mode and axis a handle belongs to; `None` for a foreign id.
pub fn handle_axis(handle: u32) -> Option<(GizmoMode, usize)> {
    match handle {
        1..=3 => Some((GizmoMode::Translate, (handle - 1) as usize)),
        11..=13 => Some((GizmoMode::Rotate, (handle - 11) as usize)),
        21..=23 => Some((GizmoMode::Scale, (handle - 21) as usize)),
        _ => None,
    }
}

/// The world axes, in handle order.
pub const AXES: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// The colour one axis draws in: X red, Y green, Z blue.
pub const AXIS_COLORS: [[f32; 4]; 3] = [
    [0.90, 0.25, 0.20, 1.0],
    [0.35, 0.80, 0.30, 1.0],
    [0.30, 0.50, 0.95, 1.0],
];

// =============================================================================
// Geometry
// =============================================================================

/// One gizmo line: its two endpoints, its colour, and its handle id.
pub type GizmoLine = ([f32; 3], [f32; 3], [f32; 4], u32);

/// One gizmo triangle: its three points, its colour, and its handle id.
pub type GizmoTriangle = ([[f32; 3]; 3], [f32; 4], u32);

/// The handle geometry one gizmo draws: contract primitives, handle ids set.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct GizmoGeometry {
    /// Handle lines.
    pub lines: Vec<GizmoLine>,
    /// Handle triangles (arrow and cube heads).
    pub triangles: Vec<GizmoTriangle>,
}

/// Build the handles of `mode`, `length` long, centred on `origin`.
///
/// `basis` is the frame the axes draw in - the world axes, or the entity's
/// rotation applied to them when the gizmo is in local space - so the caller
/// decides the space once and the geometry needs no quaternions.
pub fn build_geometry(
    mode: GizmoMode,
    origin: [f32; 3],
    basis: [[f32; 3]; 3],
    length: f32,
) -> GizmoGeometry {
    let mut geometry = GizmoGeometry::default();
    for (index, axis) in basis.iter().enumerate() {
        let handle = axis_handle(mode, index);
        let color = AXIS_COLORS[index];
        let tip = add(origin, scale(*axis, length));
        match mode {
            GizmoMode::Translate => {
                geometry.lines.push((origin, tip, color, handle));
                // The arrow head: a ring of triangles around the tip, opened
                // towards the tip's side.
                head_triangles(&mut geometry.triangles, origin, *axis, tip, color, handle);
            }
            GizmoMode::Scale => {
                geometry.lines.push((origin, tip, color, handle));
                cube_triangles(
                    &mut geometry.triangles,
                    tip,
                    *axis,
                    length * 0.08,
                    color,
                    handle,
                );
            }
            GizmoMode::Rotate => {
                // A full ring per axis, its segments all carrying the ring's
                // handle so any part of it is grabbable.
                let (u, v) = plane_axes(*axis);
                let segments = 48;
                let mut previous = ring_point(origin, u, v, length, 0.0);
                for step in 1..=segments {
                    let angle = step as f32 / segments as f32 * std::f32::consts::TAU;
                    let point = ring_point(origin, u, v, length, angle);
                    geometry.lines.push((previous, point, color, handle));
                    previous = point;
                }
            }
        }
    }
    geometry
}

/// One point of a rotation ring.
fn ring_point(origin: [f32; 3], u: [f32; 3], v: [f32; 3], radius: f32, angle: f32) -> [f32; 3] {
    let (sin, cos) = angle.sin_cos();
    add(origin, add(scale(u, radius * cos), scale(v, radius * sin)))
}

/// Four triangles forming a square head around the tip, flattened along the
/// axis so the head reads as an arrow from any side.
fn head_triangles(
    triangles: &mut Vec<GizmoTriangle>,
    origin: [f32; 3],
    axis: [f32; 3],
    tip: [f32; 3],
    color: [f32; 4],
    handle: u32,
) {
    let (u, v) = plane_axes(axis);
    let back = add(origin, scale(sub(tip, origin), 0.8));
    let half = distance(origin, tip) * 0.06;
    let corners = [
        add(add(back, scale(u, half)), scale(v, half)),
        add(add(back, scale(u, -half)), scale(v, half)),
        add(add(back, scale(u, -half)), scale(v, -half)),
        add(add(back, scale(u, half)), scale(v, -half)),
    ];
    for index in 0..4 {
        let next = (index + 1) % 4;
        triangles.push(([corners[index], corners[next], tip], color, handle));
    }
}

/// Twelve triangles forming a small cube around `center`.
fn cube_triangles(
    triangles: &mut Vec<GizmoTriangle>,
    center: [f32; 3],
    axis: [f32; 3],
    half: f32,
    color: [f32; 4],
    handle: u32,
) {
    let (u, v) = plane_axes(axis);
    let corner = |along_u: f32, along_v: f32, along_axis: f32| {
        add(
            center,
            add(
                add(scale(u, along_u * half), scale(v, along_v * half)),
                scale(axis, along_axis * half),
            ),
        )
    };
    let corners = [
        corner(-1.0, -1.0, -1.0),
        corner(1.0, -1.0, -1.0),
        corner(1.0, 1.0, -1.0),
        corner(-1.0, 1.0, -1.0),
        corner(-1.0, -1.0, 1.0),
        corner(1.0, -1.0, 1.0),
        corner(1.0, 1.0, 1.0),
        corner(-1.0, 1.0, 1.0),
    ];
    // Six faces, two triangles each.
    for (a, b, c, d) in [
        (0, 1, 2, 3),
        (4, 5, 6, 7),
        (0, 1, 5, 4),
        (1, 2, 6, 5),
        (2, 3, 7, 6),
        (3, 0, 4, 7),
    ] {
        triangles.push(([corners[a], corners[b], corners[c]], color, handle));
        triangles.push(([corners[a], corners[c], corners[d]], color, handle));
    }
}

// =============================================================================
// Drag math
// =============================================================================

/// How far along `axis` the point closest to the cursor ray sits, relative to
/// `origin`, or `None` when the ray is parallel to the axis.
///
/// This is the closest-approach parameter of two lines: the axis through
/// `origin`, and the cursor ray. A translation drag applies the difference
/// between the value recorded when the drag began and the value now, so the
/// entity follows the cursor without jumping to it.
pub fn closest_axis_point(
    origin: [f32; 3],
    axis: [f32; 3],
    ray_origin: [f32; 3],
    ray_direction: [f32; 3],
) -> Option<f32> {
    let axis = normalize(axis)?;
    let direction = normalize(ray_direction)?;
    let w0 = sub(origin, ray_origin);
    let a = dot(axis, axis);
    let b = dot(axis, direction);
    let c = dot(direction, direction);
    let d = dot(axis, w0);
    let e = dot(direction, w0);
    let denominator = a * c - b * b;
    if denominator.abs() < 1e-6 {
        return None;
    }
    Some((b * e - c * d) / denominator)
}

/// Where the cursor ray meets the plane through `origin` whose normal is
/// `normal`, or `None` when the ray runs parallel to it.
pub fn ray_plane_point(
    origin: [f32; 3],
    normal: [f32; 3],
    ray_origin: [f32; 3],
    ray_direction: [f32; 3],
) -> Option<[f32; 3]> {
    let normal = normalize(normal)?;
    let denominator = dot(normal, ray_direction);
    if denominator.abs() < 1e-6 {
        return None;
    }
    let t = dot(normal, sub(origin, ray_origin)) / denominator;
    if t < 0.0 {
        return None;
    }
    Some(add(ray_origin, scale(ray_direction, t)))
}

/// The angle of `point` around `axis`, in the ring's own basis, in radians.
///
/// The drag records this at the press and reads it again on every move; the
/// difference is the rotation, so no jump at the first frame.
pub fn plane_angle(origin: [f32; 3], axis: [f32; 3], point: [f32; 3]) -> f32 {
    let (u, v) = plane_axes(axis);
    let relative = sub(point, origin);
    dot(relative, v).atan2(dot(relative, u))
}

/// Wrap an angle difference into `(-PI, PI]`, so a drag past the seam keeps
/// turning the short way.
pub fn wrap_angle(angle: f32) -> f32 {
    let mut wrapped = angle % std::f32::consts::TAU;
    if wrapped > std::f32::consts::PI {
        wrapped -= std::f32::consts::TAU;
    } else if wrapped <= -std::f32::consts::PI {
        wrapped += std::f32::consts::TAU;
    }
    wrapped
}

/// Round `value` to the nearest multiple of `step`; a non-positive step
/// leaves it alone.
pub fn snap_value(value: f32, step: f32) -> f32 {
    if step <= 0.0 {
        return value;
    }
    (value / step).round() * step
}

/// The `[x, y, z, w]` quaternion of `radians` around `axis`.
///
/// A zero-length axis is the identity: a drag with no axis cannot happen,
/// but a caller must not get a NaN rotation from one.
pub fn axis_angle_quaternion(axis: [f32; 3], radians: f32) -> [f32; 4] {
    let Some(axis) = normalize(axis) else {
        return [0.0, 0.0, 0.0, 1.0];
    };
    let half = radians * 0.5;
    let (sin, cos) = half.sin_cos();
    [axis[0] * sin, axis[1] * sin, axis[2] * sin, cos]
}

/// Rotate `vector` by the `[x, y, z, w]` quaternion.
pub fn rotate_vector(quaternion: [f32; 4], vector: [f32; 3]) -> [f32; 3] {
    let [x, y, z, w] = quaternion;
    let length = (x * x + y * y + z * z + w * w).sqrt();
    if length <= f32::EPSILON {
        return vector;
    }
    let (x, y, z, w) = (x / length, y / length, z / length, w / length);
    // v' = v + 2 * cross(q.xyz, cross(q.xyz, v) + w * v)
    let axis = [x, y, z];
    let first = cross(axis, vector);
    let second = cross(axis, add(first, scale(vector, w)));
    add(vector, scale(second, 2.0))
}

// =============================================================================
// Small vector helpers
// =============================================================================

/// The three axes of the plane whose normal is `normal`, as two unit vectors.
pub fn plane_axes(normal: [f32; 3]) -> ([f32; 3], [f32; 3]) {
    let normal = normalize(normal).unwrap_or([0.0, 1.0, 0.0]);
    let helper = if normal[0].abs() < 0.9 {
        [1.0, 0.0, 0.0]
    } else {
        [0.0, 1.0, 0.0]
    };
    let u = normalize(cross(helper, normal)).unwrap_or([1.0, 0.0, 0.0]);
    let v = cross(normal, u);
    (u, v)
}

/// `left + right`, per component.
pub fn add(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [left[0] + right[0], left[1] + right[1], left[2] + right[2]]
}

/// `left - right`, per component.
pub fn sub(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [left[0] - right[0], left[1] - right[1], left[2] - right[2]]
}

/// `vector * factor`.
pub fn scale(vector: [f32; 3], factor: f32) -> [f32; 3] {
    [vector[0] * factor, vector[1] * factor, vector[2] * factor]
}

/// The dot product.
pub fn dot(left: [f32; 3], right: [f32; 3]) -> f32 {
    left[0] * right[0] + left[1] * right[1] + left[2] * right[2]
}

/// The cross product.
pub fn cross(left: [f32; 3], right: [f32; 3]) -> [f32; 3] {
    [
        left[1] * right[2] - left[2] * right[1],
        left[2] * right[0] - left[0] * right[2],
        left[0] * right[1] - left[1] * right[0],
    ]
}

/// `vector` scaled to unit length; `None` for a zero-length vector.
pub fn normalize(vector: [f32; 3]) -> Option<[f32; 3]> {
    let length = (vector[0] * vector[0] + vector[1] * vector[1] + vector[2] * vector[2]).sqrt();
    if length <= f32::EPSILON {
        return None;
    }
    Some([vector[0] / length, vector[1] / length, vector[2] / length])
}

/// The distance between two points.
pub fn distance(left: [f32; 3], right: [f32; 3]) -> f32 {
    let delta = sub(left, right);
    dot(delta, delta).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The handles of each mode are distinguishable and land on their axis.
    #[test]
    fn every_mode_builds_its_handles() {
        let origin = [1.0, 2.0, 3.0];
        let geometry = build_geometry(GizmoMode::Translate, origin, AXES, 2.0);
        assert_eq!(geometry.lines.len(), 3);
        assert!(!geometry.triangles.is_empty());
        for (index, (start, end, _, handle)) in geometry.lines.iter().enumerate() {
            assert_eq!(*start, origin);
            assert_eq!(*handle, axis_handle(GizmoMode::Translate, index));
            assert!((distance(*end, add(origin, scale(AXES[index], 2.0)))).abs() < 1e-5);
        }
        let rings = build_geometry(GizmoMode::Rotate, origin, AXES, 1.0);
        assert_eq!(
            rings.lines.iter().filter(|(_, _, _, h)| *h == 11).count(),
            48,
            "the X ring's segments all carry its handle"
        );
        assert!(rings.triangles.is_empty(), "rings carry no triangles");
        let cubes = build_geometry(GizmoMode::Scale, origin, AXES, 1.0);
        assert!(!cubes.triangles.is_empty(), "scale has cube heads");

        // A pick's handle names its mode and axis.
        assert_eq!(handle_axis(2), Some((GizmoMode::Translate, 1)));
        assert_eq!(handle_axis(13), Some((GizmoMode::Rotate, 2)));
        assert_eq!(handle_axis(21), Some((GizmoMode::Scale, 0)));
        assert_eq!(handle_axis(0), None);
        assert_eq!(handle_axis(99), None);
    }

    /// Snapping rounds to the step and a missing step leaves the value.
    #[test]
    fn snapping_rounds_to_the_step() {
        assert_eq!(snap_value(0.74, 0.5), 0.5);
        assert_eq!(snap_value(0.9, 0.5), 1.0);
        assert_eq!(snap_value(-0.74, 0.5), -0.5);
        assert_eq!(snap_value(7.3, 0.0), 7.3);
        assert_eq!(snap_value(16.0, 15.0), 15.0);
    }

    /// The closest point on an axis to a ray is where the drag reads its
    /// position from.
    #[test]
    fn the_axis_follows_the_cursor_ray() {
        // Straight down through x = 3: the closest point on X is 3.
        let t = closest_axis_point(
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [3.0, 10.0, 0.0],
            [0.0, -1.0, 0.0],
        )
        .expect("not parallel");
        assert!((t - 3.0).abs() < 1e-4, "t = {t}");
        // A ray parallel to the axis has no closest point.
        assert!(closest_axis_point(
            [0.0, 0.0, 0.0],
            [1.0, 0.0, 0.0],
            [0.0, 5.0, 0.0],
            [1.0, 0.0, 0.0],
        )
        .is_none());
    }

    /// The rotation plane answers with the angle around the ring's axis.
    #[test]
    fn the_ring_angle_reads_the_plane() {
        // The Y ring's basis starts at +Z (`plane_axes`), so the +Z point is
        // angle zero and +X a quarter turn later; the sign of that quarter
        // turn is the basis's business, its magnitude is not.
        let axis = [0.0, 1.0, 0.0];
        let along_start = plane_angle([0.0, 0.0, 0.0], axis, [0.0, 0.0, 1.0]);
        assert!(along_start.abs() < 1e-4, "start = {along_start}");
        let quarter = plane_angle([0.0, 0.0, 0.0], axis, [1.0, 0.0, 0.0]);
        assert!(
            (quarter.abs() - std::f32::consts::FRAC_PI_2).abs() < 1e-4,
            "quarter = {quarter}"
        );
        // Crossing the seam keeps turning the short way.
        assert!((wrap_angle(350.0_f32.to_radians()) + 10.0_f32.to_radians()).abs() < 1e-4);
        // A ray straight down hits the horizontal plane; a parallel one does
        // not.
        assert!(ray_plane_point(
            [0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [4.0, 9.0, 5.0],
            [0.0, -1.0, 0.0]
        )
        .is_some());
        assert!(ray_plane_point(
            [0.0, 0.0, 0.0],
            [0.0, 1.0, 0.0],
            [4.0, 9.0, 5.0],
            [1.0, 0.0, 0.0]
        )
        .is_none());
    }

    /// A local gizmo's axes are the entity's own, rotated.
    #[test]
    fn local_axes_follow_the_entities_rotation() {
        // 180 degrees around Y: the entity's X points along -X.
        let rotated = rotate_vector([0.0, 1.0, 0.0, 0.0], [1.0, 0.0, 0.0]);
        assert!((rotated[0] + 1.0).abs() < 1e-4, "rotated = {rotated:?}");
        // The identity leaves the axes alone.
        assert_eq!(
            rotate_vector([0.0, 0.0, 0.0, 1.0], [0.0, 1.0, 0.0]),
            [0.0, 1.0, 0.0]
        );
        // A quarter turn around Y, built from the angle: X goes to -Z.
        let quarter = axis_angle_quaternion([0.0, 1.0, 0.0], std::f32::consts::FRAC_PI_2);
        let turned = rotate_vector(quarter, [1.0, 0.0, 0.0]);
        assert!(
            turned[2] + 1.0 < 1e-4 && turned[2] + 1.0 > -1e-4,
            "turned = {turned:?}"
        );
        // A zero axis is the identity, not a NaN.
        let fallback = axis_angle_quaternion([0.0; 3], 1.0);
        assert_eq!(fallback, [0.0, 0.0, 0.0, 1.0]);
    }
}
