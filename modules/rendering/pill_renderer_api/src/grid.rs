//! The scene grid: the infinite ground the view-bound surfaces draw.
//!
//! # Responsibilities
//!
//! - Carry a frontend's grid choice ([`SceneGrid`]: the switch and the
//!   distance between its lines) beside the surface camera bindings.
//! - Resolve the spacing the grid one frame draws ([`frame_grid`]) and name
//!   the surfaces that draw it ([`editor_view_surfaces`]).
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
//!
//! The frame names the grid by its spacing alone: the surfaces that draw it are
//! the frame's editor views, one list rather than two that could disagree.

// Standard library
use std::collections::BTreeMap;

// Current crate
use crate::{CameraBinding, SurfaceCameras, SurfaceId};

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

/// Resolves the grid one frame draws from the camera bindings.
///
/// A surface draws the grid when it is bound to a view camera and the grid
/// is on; the surfaces are the frame's [`editor_view_surfaces`], so this
/// answers only the spacing. `None` means no grid: the switch is off, or no
/// surface is view-bound. A cell size the shader could not divide by falls
/// back to the default.
pub fn frame_grid(bindings: &SurfaceCameras) -> Option<f32> {
    if !bindings.grid.enabled {
        return None;
    }
    if view_bound_surfaces(&bindings.bindings).is_empty() {
        return None;
    }
    let cell_size = bindings.grid.cell_size;
    Some(if cell_size > 0.0 && cell_size.is_finite() {
        cell_size
    } else {
        DEFAULT_CELL_SIZE
    })
}

/// Every surface bound to a view camera, ascending by id.
fn view_bound_surfaces(bindings: &BTreeMap<SurfaceId, CameraBinding>) -> Vec<SurfaceId> {
    bindings
        .iter()
        .filter(|(_, binding)| matches!(binding, CameraBinding::View { .. }))
        .map(|(surface, _)| *surface)
        .collect()
}

/// The surfaces an editor's own viewports draw through: every surface bound
/// to a view camera, whether or not the grid is on.
///
/// These are the surfaces the grid is drawn on, and the ones that carry an
/// editor's chrome - the gizmo handles and the selection outlines - which need
/// the answer even when the grid is off. The frame carries the list once, as
/// its editor views.
pub fn editor_view_surfaces(cameras: &SurfaceCameras) -> Vec<SurfaceId> {
    view_bound_surfaces(&cameras.bindings)
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(frame_grid(&cameras), None);
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
        cameras.grid = SceneGrid {
            enabled: true,
            cell_size: 0.5,
        };
        assert_eq!(frame_grid(&cameras), Some(0.5));
        assert_eq!(
            editor_view_surfaces(&cameras),
            vec![SurfaceId(1), SurfaceId(2)]
        );
    }

    /// The primary surface draws the grid too, when a frontend bound it to a
    /// view camera.
    #[test]
    fn a_view_bound_primary_draws_the_grid() {
        let mut cameras = SurfaceCameras::default();
        cameras.set(SurfaceId::PRIMARY, Some(view_binding()));
        cameras.grid = SceneGrid {
            enabled: true,
            cell_size: 1.0,
        };
        assert_eq!(frame_grid(&cameras), Some(1.0));
        assert_eq!(editor_view_surfaces(&cameras), vec![SurfaceId::PRIMARY]);
    }

    /// No view binding, or an unusable cell size, resolves the way the
    /// shader needs it: nothing, and a positive spacing.
    #[test]
    fn an_empty_binding_set_draws_nothing_and_a_bad_cell_falls_back() {
        let mut cameras = SurfaceCameras {
            grid: SceneGrid {
                enabled: true,
                cell_size: 1.0,
            },
            ..SurfaceCameras::default()
        };
        assert_eq!(frame_grid(&cameras), None);
        cameras.set(SurfaceId(1), Some(view_binding()));
        for cell_size in [0.0, -2.0, f32::NAN] {
            cameras.grid = SceneGrid {
                enabled: true,
                cell_size,
            };
            assert_eq!(frame_grid(&cameras), Some(DEFAULT_CELL_SIZE));
        }
    }
}
