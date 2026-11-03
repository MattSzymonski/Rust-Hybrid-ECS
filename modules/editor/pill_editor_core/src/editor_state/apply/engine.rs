//! Applies the editor's engine controls: pause and step, the gizmo's tool,
//! space and snapping, the scene grid's switch, and a system's on/off flag.
//!
//! # Responsibilities
//!
//! - Apply those controls, and answer a system toggle with the inverse that
//!   switches the system back.
//!
//! # Design
//!
//! One `match` over this family's commands, called from
//! [`EditorCommand::apply_one`](super::EditorCommand). A command that changed
//! something returns a [`CommandOutcome`]: the inverse the undo history
//! records, or `plain()` when there is nothing to undo. A command from another
//! family never arrives here; it hits `unreachable!`.

// External crates
use pill_engine::Engine;

// Current crate
use super::CommandOutcome;
use super::*;

// =============================================================================
// Free Functions
// =============================================================================

/// Applies one engine command.
///
/// # Errors
///
/// Returns a message naming the command and the reason when:
///
/// - a system toggle names a registration index that has no system;
/// - the gizmo state cannot be read back after this function created it;
/// - the surface cameras cannot be read back after this function created
///   them.
///
/// # Panics
///
/// Panics when `command` belongs to another family; `EditorCommand::apply_one`
/// routes only engine commands here.
pub(super) fn apply(
    engine: &mut Engine,
    command: &EditorCommand,
) -> Result<CommandOutcome, String> {
    match command {
        EditorCommand::SetSystemEnabled { index, enabled } => {
            let was_enabled = engine
                .system_snapshots()
                .get(*index)
                .map(|system| system.enabled)
                .ok_or_else(|| format!("{command:?}: no system at registration index {index}"))?;
            if engine.set_system_enabled_at(*index, *enabled) {
                Ok(CommandOutcome::inverse(EditorCommand::SetSystemEnabled {
                    index: *index,
                    enabled: was_enabled,
                }))
            } else {
                Err(format!(
                    "{command:?}: no system at registration index {index}"
                ))
            }
        }

        EditorCommand::SetPaused { paused } => {
            engine.set_systems_paused(*paused);
            Ok(CommandOutcome::plain())
        }

        EditorCommand::SetGizmoTool { mode, space, snap } => {
            let world = engine.world_mut();
            if world.get_resource::<crate::gizmo::GizmoState>().is_none() {
                world.insert_resource(crate::gizmo::GizmoState::default());
            }
            let Some(gizmo_state) = world.get_resource_mut::<crate::gizmo::GizmoState>() else {
                return Err(format!("{command:?}: the gizmo state could not be claimed"));
            };
            // A field the command leaves as `None` keeps its current value, so
            // a row that changes one setting leaves the other two alone.
            if let Some(mode) = mode {
                gizmo_state.settings.mode = *mode;
            }
            if let Some(space) = space {
                gizmo_state.settings.space = *space;
            }
            if let Some(snap) = snap {
                gizmo_state.settings.snap = *snap;
            }
            Ok(CommandOutcome::plain())
        }

        EditorCommand::SetGrid { enabled } => {
            let world = engine.world_mut();
            if world
                .get_resource::<pill_renderer_api::SurfaceCameras>()
                .is_none()
            {
                world.insert_resource(pill_renderer_api::SurfaceCameras::default());
            }
            let Some(surface_cameras) =
                world.get_resource_mut::<pill_renderer_api::SurfaceCameras>()
            else {
                return Err(format!(
                    "{command:?}: the surface cameras could not be claimed"
                ));
            };
            // Log only a real change, so sending the current state again adds
            // no line.
            if surface_cameras.grid.enabled != *enabled {
                surface_cameras.grid.enabled = *enabled;
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    enabled = *enabled,
                    "[editor] scene grid"
                );
            }
            Ok(CommandOutcome::plain())
        }

        EditorCommand::StepOnce => {
            engine.request_single_step();
            Ok(CommandOutcome::plain())
        }

        other => unreachable!("{:?} is not a engine command", other.family()),
    }
}
