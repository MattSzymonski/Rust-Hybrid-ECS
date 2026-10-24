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

/// The three translation planes, as the axis pairs they span.
pub const PLANE_PAIRS: [(usize, usize); 3] = [(0, 1), (0, 2), (1, 2)];

/// The handle one translation plane is pickable by: the square between its
/// two axes. Planes only exist on the translation tool.
pub const fn plane_handle(first: usize, second: usize) -> u32 {
    let index = match (first, second) {
        (0, 1) => 0,
        (0, 2) => 1,
        (1, 2) => 2,
        _ => 0,
    };
    4 + index as u32
}

/// What a handle id addresses.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandleKind {
    /// One axis of one mode.
    Axis {
        /// The mode the axis belongs to.
        mode: GizmoMode,
        /// The axis index, `0..=2` for X, Y and Z.
        axis: usize,
    },
    /// A translation plane, spanned by two axes.
    Plane {
        /// The first axis of the plane.
        first: usize,
        /// The second axis of the plane.
        second: usize,
    },
}

impl HandleKind {
    /// The axis a single-axis handle drags along; `None` for a plane.
    pub fn axis(self) -> Option<(GizmoMode, usize)> {
        match self {
            HandleKind::Axis { mode, axis } => Some((mode, axis)),
            HandleKind::Plane { .. } => None,
        }
    }

    /// The axis pair a plane handle spans; `None` for a single axis.
    pub fn plane(self) -> Option<(usize, usize)> {
        match self {
            HandleKind::Plane { first, second } => Some((first, second)),
            HandleKind::Axis { .. } => None,
        }
    }
}

/// What a handle id addresses; `None` for a foreign id.
///
/// Ids are stable numbers: `1..=3` translate axes, `4..=6` translation
/// planes, `11..=13` rotate rings, `21..=23` scale axes.
pub fn handle_kind(handle: u32) -> Option<HandleKind> {
    match handle {
        1..=3 => Some(HandleKind::Axis {
            mode: GizmoMode::Translate,
            axis: (handle - 1) as usize,
        }),
        4..=6 => {
            let (first, second) = PLANE_PAIRS[(handle - 4) as usize];
            Some(HandleKind::Plane { first, second })
        }
        11..=13 => Some(HandleKind::Axis {
            mode: GizmoMode::Rotate,
            axis: (handle - 11) as usize,
        }),
        21..=23 => Some(HandleKind::Axis {
            mode: GizmoMode::Scale,
            axis: (handle - 21) as usize,
        }),
        _ => None,
    }
}

/// Which mode and axis a handle belongs to; `None` for a foreign id or a
/// plane handle.
pub fn handle_axis(handle: u32) -> Option<(GizmoMode, usize)> {
    handle_kind(handle).and_then(HandleKind::axis)
}

/// The world axes, in handle order.
pub const AXES: [[f32; 3]; 3] = [[1.0, 0.0, 0.0], [0.0, 1.0, 0.0], [0.0, 0.0, 1.0]];

/// The colour one axis draws in: X red, Y green, Z blue.
pub const AXIS_COLORS: [[f32; 4]; 3] = [
    [0.90, 0.25, 0.20, 1.0],
    [0.35, 0.80, 0.30, 1.0],
    [0.30, 0.50, 0.95, 1.0],
];

/// How much a hovered handle brightens, and how much an active one dims
/// under its hover state: the two feedback states the handles show.
pub const HOVER_BRIGHTEN: f32 = 1.35;
/// The active handle's multiplier against its base colour.
pub const ACTIVE_DIM: f32 = 0.70;

/// The plane squares' alpha: translucent, so the axis arrows behind them
/// still read.
pub const PLANE_ALPHA: f32 = 0.55;

/// Where a plane square's centre sits along both of its axes, as a fraction
/// of the gizmo's length, and its half-size.
pub const PLANE_OFFSET: f32 = 0.32;
pub const PLANE_HALF: f32 = 0.09;

/// The largest factor a scale handle may grow to during a drag, and the
/// smallest it may shrink to: past these the handle would leave the view or
/// vanish into the origin.
pub const SCALE_HANDLE_MAX_RATIO: f32 = 3.0;
pub const SCALE_HANDLE_MIN_RATIO: f32 = 0.2;

/// Which handle the pointer is over and which one a drag holds, so the
/// geometry can be built in the state the user is looking at.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct HandleStates {
    /// The handle under the pointer, if one is.
    pub hovered: Option<u32>,
    /// The handle a drag holds, if one does.
    pub active: Option<u32>,
}

impl HandleStates {
    /// The state one handle draws in: active wins over hovered.
    pub fn of(self, handle: u32) -> HandleState {
        if self.active == Some(handle) {
            HandleState::Active
        } else if self.hovered == Some(handle) {
            HandleState::Hovered
        } else {
            HandleState::Idle
        }
    }
}

/// The visual state of one handle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HandleState {
    /// Normal appearance.
    Idle,
    /// Under the pointer: brighter.
    Hovered,
    /// Held by a drag: dimmer than the hover state.
    Active,
}

/// `base` adjusted for the state `handle` is in.
pub fn styled_color(base: [f32; 4], handle: u32, states: HandleStates) -> [f32; 4] {
    match states.of(handle) {
        HandleState::Idle => base,
        HandleState::Hovered => [
            (base[0] * HOVER_BRIGHTEN).min(1.0),
            (base[1] * HOVER_BRIGHTEN).min(1.0),
            (base[2] * HOVER_BRIGHTEN).min(1.0),
            base[3],
        ],
        HandleState::Active => [
            base[0] * ACTIVE_DIM,
            base[1] * ACTIVE_DIM,
            base[2] * ACTIVE_DIM,
            base[3],
        ],
    }
}

/// The colour a plane square draws in: its two axes' colours averaged, at
/// [`PLANE_ALPHA`].
pub fn plane_color(first: usize, second: usize) -> [f32; 4] {
    let left = AXIS_COLORS[first];
    let right = AXIS_COLORS[second];
    [
        (left[0] + right[0]) * 0.5,
        (left[1] + right[1]) * 0.5,
        (left[2] + right[2]) * 0.5,
        PLANE_ALPHA,
    ]
}

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

/// Build the handles of `mode`, `length` long, centred on `origin`, in the
/// idle state: the plain form of [`build_geometry_styled`].
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
    build_geometry_styled(mode, origin, basis, [length; 3], HandleStates::default())
}

/// Build the handles of `mode` centred on `origin`, each axis `lengths[axis]`
/// long, coloured for the hover and drag states.
///
/// The per-axis lengths are the scale gizmo's dynamic handles: a scale drag
/// passes the factor it is writing, so the dragged axis grows and shrinks
/// with the object's scale while the drag runs. The translation tool also
/// builds its three plane squares here, between each pair of axes.
pub fn build_geometry_styled(
    mode: GizmoMode,
    origin: [f32; 3],
    basis: [[f32; 3]; 3],
    lengths: [f32; 3],
    states: HandleStates,
) -> GizmoGeometry {
    let mut geometry = GizmoGeometry::default();
    for (index, axis) in basis.iter().enumerate() {
        let handle = axis_handle(mode, index);
        let color = styled_color(AXIS_COLORS[index], handle, states);
        let length = lengths[index].max(1e-4);
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
    if mode == GizmoMode::Translate {
        for (first, second) in PLANE_PAIRS {
            let handle = plane_handle(first, second);
            let color = styled_color(plane_color(first, second), handle, states);
            let length = (lengths[first] + lengths[second]) * 0.5;
            plane_square(
                &mut geometry.triangles,
                origin,
                basis[first],
                basis[second],
                length,
                color,
                handle,
            );
        }
    }
    geometry
}

/// Two triangles forming one translation plane's square, centred
/// [`PLANE_OFFSET`] of `length` along both of the plane's axes.
fn plane_square(
    triangles: &mut Vec<GizmoTriangle>,
    origin: [f32; 3],
    first: [f32; 3],
    second: [f32; 3],
    length: f32,
    color: [f32; 4],
    handle: u32,
) {
    let offset = length * PLANE_OFFSET;
    let half = length * PLANE_HALF;
    let center = add(origin, add(scale(first, offset), scale(second, offset)));
    let corner = |along_first: f32, along_second: f32| {
        add(
            center,
            add(
                scale(first, along_first * half),
                scale(second, along_second * half),
            ),
        )
    };
    let corners = [
        corner(-1.0, -1.0),
        corner(1.0, -1.0),
        corner(1.0, 1.0),
        corner(-1.0, 1.0),
    ];
    triangles.push(([corners[0], corners[1], corners[2]], color, handle));
    triangles.push(([corners[0], corners[2], corners[3]], color, handle));
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
// Plane drags, dynamic scale handles and the drag guides
// =============================================================================

/// `point`'s coordinates along the plane's two axes, relative to `origin`.
///
/// A plane drag records both at the press and reads them again on every
/// move, so the object follows the cursor without jumping to it - the same
/// rule [`closest_axis_point`] follows for one axis. The axes are the
/// gizmo's basis vectors, so world and local space both work.
pub fn plane_coordinates(
    origin: [f32; 3],
    first: [f32; 3],
    second: [f32; 3],
    point: [f32; 3],
) -> (f32, f32) {
    let relative = sub(point, origin);
    (dot(relative, first), dot(relative, second))
}

/// The per-axis handle lengths a scale drag draws: the dragged axis follows
/// the object's scale factor against the drag's original scale, the others
/// keep `reference`.
///
/// The ratio is clamped to the handle range and guarded against a zero
/// original scale, so a degenerate object still draws a usable handle, and a
/// non-finite ratio (a zero scale written by a command mid-drag) falls back
/// to the reference length rather than a NaN geometry.
pub fn scale_handle_lengths(
    reference: f32,
    original: [f32; 3],
    current: [f32; 3],
    axis: usize,
) -> [f32; 3] {
    let mut lengths = [reference; 3];
    if axis >= 3 {
        return lengths;
    }
    let ratio = if original[axis].abs() > 1e-4 {
        current[axis] / original[axis]
    } else {
        1.0
    };
    let ratio = if ratio.is_finite() {
        ratio.clamp(SCALE_HANDLE_MIN_RATIO, SCALE_HANDLE_MAX_RATIO)
    } else {
        1.0
    };
    lengths[axis] = reference * ratio;
    lengths
}

/// The two ends of the axis constraint guide through `origin`, `span` long
/// on both sides: the line a single-axis drag draws along the axis it
/// constrains.
pub fn axis_guide(origin: [f32; 3], axis: [f32; 3], span: f32) -> ([f32; 3], [f32; 3]) {
    let axis = normalize(axis).unwrap_or(AXES[0]);
    (
        sub(origin, scale(axis, span)),
        add(origin, scale(axis, span)),
    )
}

/// The dashes of a dotted line from `start` to `end`: `dash`-long segments
/// separated by the same gap, ending exactly at `end`.
///
/// The dash length is a world distance the caller derives from the camera
/// distance, so the dots keep a steady look on screen. A very long line is
/// cut to [`DOTTED_SEGMENTS_MAX`] dashes rather than building thousands of
/// them.
pub fn dotted_segments(start: [f32; 3], end: [f32; 3], dash: f32) -> Vec<([f32; 3], [f32; 3])> {
    let total = distance(start, end);
    if total <= 1e-5 || dash <= 1e-5 {
        return Vec::new();
    }
    let step = dash * 2.0;
    let count = ((total / step).ceil() as usize).clamp(1, DOTTED_SEGMENTS_MAX);
    let direction = scale(sub(end, start), 1.0 / total);
    let mut segments = Vec::with_capacity(count);
    for index in 0..count {
        let from = total * index as f32 / count as f32;
        // The last dash reaches the end exactly, so the line always shows
        // where the pivot is now, whatever the dash count rounds to.
        let to = if index + 1 == count {
            total
        } else {
            (from + dash).min(total)
        };
        segments.push((
            add(start, scale(direction, from)),
            add(start, scale(direction, to)),
        ));
    }
    segments
}

/// The most dashes [`dotted_segments`] builds for one line.
pub const DOTTED_SEGMENTS_MAX: usize = 96;

/// The handle a cursor ray hovers: the nearest handle whose line the ray
/// passes within `radius` world units of, or whose triangle it enters.
///
/// The test runs against the geometry the frame draws - the exact numbers
/// the renderer turns into pixels - so the highlight cannot disagree with
/// what is on screen.
pub fn hit_test(
    geometry: &GizmoGeometry,
    ray_origin: [f32; 3],
    ray_direction: [f32; 3],
    radius: f32,
) -> Option<u32> {
    let mut best: Option<(f32, u32)> = None;
    let mut consider = |along: f32, handle: u32| {
        let better = match best {
            None => true,
            Some((current, _)) => along < current,
        };
        if better {
            best = Some((along, handle));
        }
    };
    for (start, end, _, handle) in &geometry.lines {
        if let Some((along, distance)) =
            ray_segment_distance(ray_origin, ray_direction, *start, *end)
        {
            if distance <= radius {
                consider(along, *handle);
            }
        }
    }
    for (corners, _, handle) in &geometry.triangles {
        if let Some(along) = ray_triangle_intersection(ray_origin, ray_direction, *corners) {
            consider(along, *handle);
        }
    }
    best.map(|(_, handle)| handle)
}

/// How far along the ray and how far from it the closest approach to the
/// segment `start..end` is; `None` when the ray points away from the
/// segment entirely.
fn ray_segment_distance(
    ray_origin: [f32; 3],
    ray_direction: [f32; 3],
    start: [f32; 3],
    end: [f32; 3],
) -> Option<(f32, f32)> {
    let direction = normalize(ray_direction)?;
    let segment = sub(end, start);
    let segment_length = dot(segment, segment).sqrt();
    if segment_length <= 1e-6 {
        let along = dot(sub(start, ray_origin), direction);
        if along < 0.0 {
            return None;
        }
        let point = add(ray_origin, scale(direction, along));
        return Some((along, distance(point, start)));
    }
    let segment = scale(segment, 1.0 / segment_length);
    let offset = sub(ray_origin, start);
    let a = dot(direction, direction);
    let b = dot(direction, segment);
    let c = dot(segment, segment);
    let d = dot(direction, offset);
    let e = dot(segment, offset);
    let denominator = a * c - b * b;
    let (along, position) = if denominator.abs() < 1e-6 {
        // Parallel: measure from the ray's own origin.
        (0.0f32, (e / c).clamp(0.0, segment_length))
    } else {
        let along = (b * e - c * d) / denominator;
        let position = (a * e - b * d) / denominator;
        (along.max(0.0), position.clamp(0.0, segment_length))
    };
    let ray_point = add(ray_origin, scale(direction, along));
    let segment_point = add(start, scale(segment, position));
    Some((along, distance(ray_point, segment_point)))
}

/// How far along the ray a triangle is hit, by Möller-Trumbore; `None` for a
/// miss, a back face or a triangle behind the ray's origin.
pub fn ray_triangle_intersection(
    ray_origin: [f32; 3],
    ray_direction: [f32; 3],
    corners: [[f32; 3]; 3],
) -> Option<f32> {
    let direction = normalize(ray_direction)?;
    let edge_one = sub(corners[1], corners[0]);
    let edge_two = sub(corners[2], corners[0]);
    let pvec = cross(direction, edge_two);
    let determinant = dot(edge_one, pvec);
    if determinant.abs() < 1e-7 {
        return None;
    }
    let inverse = 1.0 / determinant;
    let tvec = sub(ray_origin, corners[0]);
    let u = dot(tvec, pvec) * inverse;
    if !(0.0..=1.0).contains(&u) {
        return None;
    }
    let qvec = cross(tvec, edge_one);
    let v = dot(direction, qvec) * inverse;
    if v < 0.0 || u + v > 1.0 {
        return None;
    }
    let along = dot(edge_two, qvec) * inverse;
    (along > 1e-4).then_some(along)
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

    /// The translation tool carries one square per axis pair, and its handle
    /// ids name the pair.
    #[test]
    fn the_translation_tool_builds_three_plane_squares() {
        let origin = [0.0, 0.0, 0.0];
        let length = 2.0;
        let geometry = build_geometry(GizmoMode::Translate, origin, AXES, length);
        // Three axes plus three planes, two triangles per plane.
        let plane_handles: Vec<u32> = (4..=6).collect();
        for handle in plane_handles {
            let count = geometry
                .triangles
                .iter()
                .filter(|(_, _, id)| *id == handle)
                .count();
            assert_eq!(count, 2, "plane {handle} is one square");
        }
        // The XY square sits at the offset along +X and +Y.
        let expected = length * PLANE_OFFSET;
        let xy = geometry
            .triangles
            .iter()
            .find(|(_, _, id)| *id == plane_handle(0, 1))
            .expect("the XY square exists");
        // The first triangle's corners 0 and 2 are the square's diagonal.
        let center = [
            (xy.0[0][0] + xy.0[2][0]) * 0.5,
            (xy.0[0][1] + xy.0[2][1]) * 0.5,
            (xy.0[0][2] + xy.0[2][2]) * 0.5,
        ];
        assert!(center[0] > 0.0 && center[1] > 0.0, "center {center:?}");
        assert!(
            (center[0] - expected).abs() < 1e-4 && (center[1] - expected).abs() < 1e-4,
            "center {center:?}"
        );
        assert!(center[2].abs() < 1e-5, "the XY square stays at z = 0");
        // The plane handles resolve back to their axis pairs.
        assert_eq!(
            handle_kind(plane_handle(1, 2)),
            Some(HandleKind::Plane {
                first: 1,
                second: 2
            })
        );
        assert_eq!(handle_kind(4).and_then(HandleKind::axis), None);
        assert_eq!(
            handle_kind(2).and_then(HandleKind::axis),
            Some((GizmoMode::Translate, 1))
        );
    }

    /// Hover brightens a handle, an active drag dims it under that.
    #[test]
    fn handle_states_brighten_then_dim() {
        let base = AXIS_COLORS[0];
        let states = HandleStates {
            hovered: Some(1),
            active: None,
        };
        let hovered = styled_color(base, 1, states);
        assert!(hovered[0] > base[0] || hovered[1] > base[1] || hovered[2] > base[2]);
        let active = HandleStates {
            hovered: Some(1),
            active: Some(1),
        };
        let dragging = styled_color(base, 1, active);
        assert!(
            dragging[0] < base[0],
            "the active handle dims: {dragging:?}"
        );
        assert!(
            dragging[0] < hovered[0],
            "active is dimmer than hovered: {dragging:?} vs {hovered:?}"
        );
        // Other handles keep their normal look.
        assert_eq!(styled_color(base, 2, states), base);
        // A plane handle's colour is its axes' average, translucent.
        let plane = plane_color(0, 2);
        assert!(plane[3] < 1.0, "plane squares are translucent");
        assert!((plane[0] - (AXIS_COLORS[0][0] + AXIS_COLORS[2][0]) * 0.5).abs() < 1e-5);
    }

    /// A scale handle follows the scale factor, clamped at both ends.
    #[test]
    fn scale_handles_follow_the_scale_factor() {
        let reference = 2.0;
        let original = [1.0, 1.0, 1.0];
        let grown = scale_handle_lengths(reference, original, [2.0, 1.0, 1.0], 0);
        assert!((grown[0] - 4.0).abs() < 1e-5, "grown {grown:?}");
        assert!((grown[1] - reference).abs() < 1e-5, "others stay");
        let shrunk = scale_handle_lengths(reference, original, [0.5, 1.0, 1.0], 0);
        assert!((shrunk[0] - 1.0).abs() < 1e-5, "shrunk {shrunk:?}");
        // The clamps hold against extreme and degenerate scales.
        let huge = scale_handle_lengths(reference, original, [1000.0, 1.0, 1.0], 0);
        assert!((huge[0] - reference * SCALE_HANDLE_MAX_RATIO).abs() < 1e-5);
        let tiny = scale_handle_lengths(reference, original, [0.0001, 1.0, 1.0], 0);
        assert!((tiny[0] - reference * SCALE_HANDLE_MIN_RATIO).abs() < 1e-5);
        let zero_original = scale_handle_lengths(reference, [0.0, 1.0, 1.0], [1.0, 1.0, 1.0], 0);
        assert!((zero_original[0] - reference).abs() < 1e-5);
        let not_finite = scale_handle_lengths(reference, original, [f32::NAN, 1.0, 1.0], 0);
        assert!((not_finite[0] - reference).abs() < 1e-5);
    }

    /// The guides: a symmetric axis line and evenly spaced dashes.
    #[test]
    fn guides_span_both_directions_and_dash_evenly() {
        let (start, end) = axis_guide([1.0, 0.0, 0.0], AXES[1], 5.0);
        assert_eq!(start, [1.0, -5.0, 0.0]);
        assert_eq!(end, [1.0, 5.0, 0.0]);
        let dashes = dotted_segments([0.0, 0.0, 0.0], [10.0, 0.0, 0.0], 1.0);
        assert_eq!(dashes.len(), 5, "dash and gap are equal: {dashes:?}");
        assert_eq!(dashes[0].0, [0.0, 0.0, 0.0]);
        assert_eq!(dashes.last().expect("a dash").1, [10.0, 0.0, 0.0]);
        for (from, to) in &dashes[..dashes.len() - 1] {
            assert!((distance(*from, *to) - 1.0).abs() < 1e-4, "dash length");
        }
        // The last dash reaches the end, so it may run up to twice as long.
        let last = dashes.last().expect("a dash");
        let last_length = distance(last.0, last.1);
        assert!(
            last_length >= 1.0 && last_length <= 2.0 + 1e-4,
            "last dash {last_length}"
        );
        // A very long line is capped rather than building thousands of dashes.
        let capped = dotted_segments([0.0, 0.0, 0.0], [10_000.0, 0.0, 0.0], 0.01);
        assert_eq!(capped.len(), DOTTED_SEGMENTS_MAX);
        assert!(dotted_segments([0.0; 3], [0.0; 3], 1.0).is_empty());
    }

    /// The hover test agrees with the geometry: lines within the radius and
    /// triangles on the ray both answer their handle.
    #[test]
    fn hit_testing_finds_lines_planes_and_nothing() {
        let origin = [0.0, 0.0, 0.0];
        let geometry = build_geometry(GizmoMode::Translate, origin, AXES, 2.0);
        // A ray from above, aimed at the X axis a little past the origin.
        let hit = hit_test(&geometry, [1.0, 5.0, 0.0], [0.0, -1.0, 0.0], 0.05);
        assert_eq!(hit, Some(axis_handle(GizmoMode::Translate, 0)));
        // Straight down through the XY square, away from any line.
        let offset = 2.0 * PLANE_OFFSET;
        let plane = hit_test(&geometry, [offset, offset, 5.0], [0.0, 0.0, -1.0], 0.02);
        assert_eq!(plane, Some(plane_handle(0, 1)), "the XY square answers");
        // A ray into empty space finds nothing.
        let miss = hit_test(&geometry, [10.0, 10.0, 10.0], [0.0, 1.0, 0.0], 0.05);
        assert_eq!(miss, None);
    }
}
