//! The scene grid: the infinite ground the view-bound surfaces draw.
//!
//! # Responsibilities
//!
//! - Carry a frontend's grid choice ([`SceneGrid`]: the switch and the
//!   distance between its lines) beside the surface camera bindings.
//! - Carry the grid one frame draws ([`FrameGrid`]: the lines' spacing and
//!   the surfaces that draw it) and resolve one from the other
//!   ([`frame_grid`]).
//!
//! # Design
//!
//! The grid is an editor's own furniture, not part of a world: it is drawn
//! on the surfaces bound to a view camera ([`CameraBinding::View`]) - the
//! editor's own viewports, whose camera is never an entity - and a surface
//! drawn through a world camera never shows it. That keeps a game's window
//! untouched however a frontend binds its cameras, while an editor gets the
//! grid by flipping one switch. The renderer draws it as a fullscreen
//! triangle whose fragment shader intersects each pixel's ray with the y=0
//! plane, so the grid has no size to run out of and nothing to rebuild as
//! the view moves - only its spacing, its fade and its colours are data.

// External crates
use std::collections::BTreeMap;

// Current crate
use crate::{CameraBinding, SurfaceCameras};

/// The cell size a grid falls back to when a frontend asks for an unusable
/// one: the shader divides by it, so zero or negative would paint nothing.
const DEFAULT_CELL_SIZE: f32 = 1.0;

/// The scene grid's switch and spacing, carried beside the surface camera
/// bindings.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SceneGrid {
    /// Whether the view-bound surfaces draw the grid.
    pub enabled: bool,
    /// The distance between its minor lines, in world units.
    pub cell_size: f32,
}

impl Default for SceneGrid {
    fn default() -> Self {
        // Off until a frontend asks: turning it on is an editor's decision,
        // so a game's window and every other consumer of the renderer never
        // see it.
        Self {
            enabled: false,
            cell_size: DEFAULT_CELL_SIZE,
        }
    }
}

/// The grid one frame draws: its spacing, and the surfaces that draw it.
#[derive(Debug, Clone, PartialEq)]
pub struct FrameGrid {
    /// The distance between the minor lines, in world units.
    pub cell_size: f32,
    /// The surfaces that draw it, ascending.
    pub surfaces: Vec<u32>,
}

/// Resolve the grid one frame draws from the camera bindings.
///
/// A surface draws the grid when it is bound to a view camera and the grid
/// is on. The result names every such surface - including the primary one,
/// when a frontend bound it that way - or nothing when the grid is off or no
/// surface is view-bound. A cell size the shader could not divide by falls
/// back to the default.
pub fn frame_grid(bindings: &SurfaceCameras, grid: SceneGrid) -> Option<FrameGrid> {
    if !grid.enabled {
        return None;
    }
    let surfaces = view_bound_surfaces(&bindings.bindings);
    if surfaces.is_empty() {
        return None;
    }
    let cell_size = if grid.cell_size > 0.0 && grid.cell_size.is_finite() {
        grid.cell_size
    } else {
        DEFAULT_CELL_SIZE
    };
    Some(FrameGrid {
        cell_size,
        surfaces,
    })
}

/// Every surface bound to a view camera, ascending by id.
fn view_bound_surfaces(bindings: &BTreeMap<u32, CameraBinding>) -> Vec<u32> {
    bindings
        .iter()
        .filter(|(_, binding)| matches!(binding, CameraBinding::View { .. }))
        .map(|(surface, _)| *surface)
        .collect()
}

/// The surfaces an editor's own viewports draw through: every surface bound
/// to a view camera, whether or not the grid is on (W-0049).
///
/// This is the same set [`frame_grid`] draws the grid on, named separately
/// because the gizmo handles and the selection outlines need the answer even
/// when the person turned the grid off.
pub fn editor_view_surfaces(cameras: &SurfaceCameras) -> Vec<u32> {
    view_bound_surfaces(&cameras.bindings)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SurfaceId;

    // External crates
    use pill_engine::common_components::TransformComponent;

    /// A view-bound surface, the way an editor binds its own viewport.
    fn view_binding() -> CameraBinding {
        CameraBinding::View {
            camera: crate::CameraComponent::default(),
            transform: TransformComponent::default(),
        }
    }

    /// The grid is off by default, and an off grid resolves to nothing.
    #[test]
    fn a_disabled_grid_resolves_to_nothing() {
        assert!(!SceneGrid::default().enabled);
        let mut cameras = SurfaceCameras::default();
        cameras.set(SurfaceId(1), Some(view_binding()));
        assert_eq!(frame_grid(&cameras, SceneGrid::default()), None);
    }

    /// With the grid on, exactly the view-bound surfaces draw it - the
    /// entity-bound ones never do.
    #[test]
    fn only_view_bound_surfaces_draw_the_grid() {
        let mut world = pill_engine::World::new();
        let camera_entity = world.reserve_entity();
        let mut cameras = SurfaceCameras::default();
        cameras.set(SurfaceId(1), Some(view_binding()));
        cameras.set(SurfaceId(2), Some(view_binding()));
        cameras.set(SurfaceId(3), Some(CameraBinding::Entity(camera_entity)));
        let grid = SceneGrid {
            enabled: true,
            cell_size: 0.5,
        };
        let resolved = frame_grid(&cameras, grid).expect("the grid resolves");
        assert_eq!(resolved.surfaces, vec![1, 2]);
        assert_eq!(resolved.cell_size, 0.5);
    }

    /// The primary surface draws the grid too, when a frontend bound it to a
    /// view camera.
    #[test]
    fn a_view_bound_primary_draws_the_grid() {
        let mut cameras = SurfaceCameras::default();
        cameras.set(SurfaceId::PRIMARY, Some(view_binding()));
        let grid = SceneGrid {
            enabled: true,
            cell_size: 1.0,
        };
        let resolved = frame_grid(&cameras, grid).expect("the grid resolves");
        assert_eq!(resolved.surfaces, vec![SurfaceId::PRIMARY.0]);
    }

    /// No view binding, or an unusable cell size, resolves the way the
    /// shader needs it: nothing, and a positive spacing.
    #[test]
    fn an_empty_binding_set_draws_nothing_and_a_bad_cell_falls_back() {
        let mut cameras = SurfaceCameras::default();
        let grid = SceneGrid {
            enabled: true,
            cell_size: 1.0,
        };
        assert_eq!(frame_grid(&cameras, grid), None);
        cameras.set(SurfaceId(1), Some(view_binding()));
        for cell_size in [0.0, -2.0, f32::NAN] {
            let grid = SceneGrid {
                enabled: true,
                cell_size,
            };
            let resolved = frame_grid(&cameras, grid).expect("the grid resolves");
            assert_eq!(resolved.cell_size, DEFAULT_CELL_SIZE);
        }
    }
}
