//! The engine family's applier.
//!
//! The engine controls: pause, step, the gizmo tool, the grid and a
//! system toggle.
//!
//! # Responsibilities
//!
//! - Apply the engine controls: pause, step, the gizmo tool, the grid and a
//!   system's enabled flag.
//!
//! # Design
//!
//! One `match` over the family's own variants, called from
//! [`EditorCommand::apply_one`](super::EditorCommand). Every arm ends in a
//! [`CommandOutcome`]: the inverse the history records, or `plain()` for a
//! command that cannot be undone.

use pill_engine::Engine;

use super::CommandOutcome;
use super::*;

/// Apply one engine command.
pub(super) fn apply(
    engine: &mut Engine,
    command: &EditorCommand,
) -> Result<CommandOutcome, String> {
    match command {
        EditorCommand::SetSystemEnabled { index, enabled } => {
            let previous = engine
                .system_snapshots()
                .get(*index)
                .map(|system| system.enabled)
                .ok_or_else(|| format!("{command:?}: no system at registration index {index}"))?;
            if engine.set_system_enabled_at(*index, *enabled) {
                Ok(CommandOutcome::inverse(EditorCommand::SetSystemEnabled {
                    index: *index,
                    enabled: previous,
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
            let Some(state) = world.get_resource_mut::<crate::gizmo::GizmoState>() else {
                return Err(format!("{command:?}: the gizmo state could not be claimed"));
            };
            if let Some(mode) = mode {
                state.settings.mode = *mode;
            }
            if let Some(space) = space {
                state.settings.space = *space;
            }
            if let Some(snap) = snap {
                state.settings.snap = *snap;
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
            let Some(cameras) = world.get_resource_mut::<pill_renderer_api::SurfaceCameras>()
            else {
                return Err(format!(
                    "{command:?}: the surface cameras could not be claimed"
                ));
            };
            if cameras.grid.enabled != *enabled {
                cameras.grid.enabled = *enabled;
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
