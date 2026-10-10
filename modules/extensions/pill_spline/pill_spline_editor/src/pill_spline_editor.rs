//! The spline extension's editor part.
//!
//! # Responsibilities
//!
//! - Registers through the extension ABI in an editor host, after `pill_spline`.
//! - Records each load in the persistable [`SplineEditorMarker`] resource and
//!   logs it, so tests can see that the editor crate loaded, when it reloaded,
//!   and that its state survived the reload.
//! - Registers the extension's editor commands (Stage 14) and runs the ones
//!   it owns, drained from the shell's command inbox.
//!
//! # Design
//!
//! Editor tools for splines (panels, commands) grow here; for now the crate
//! proves the editor-crate convention - found by its name in the extension's
//! folder, loaded only by an editor host, rebuilt whenever the runtime crate
//! it links reloads - and the command-ownership rule: the registrations are
//! keyed by this crate, so every reload replaces exactly this set, and the
//! handler is a system this crate registers, drained from the shared inbox
//! (the shell cannot call into a module's code across the reload boundary).

// External crates
use pill_editor_core::commands::{CommandInbox, CommandRegistry, CommandSpec, KeymapPreset};
use pill_engine::pill_module;
use pill_engine::{Engine, ResMut, Resource};
use serde::{Deserialize, Serialize};

// =============================================================================
// Resource
// =============================================================================

/// Proof that the spline editor crate is loaded: how many generations of it
/// have registered in this world.
///
/// Persistable, so the count survives the editor crate's own reloads: each
/// generation's build of the type is a different type to the process, and the
/// reload carries the value across by its persistence name.
/// Public so a test or another editor crate can read it.
#[derive(Debug, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct SplineEditorMarker {
    /// Number of times a generation of this crate registered.
    pub loaded_generations: u32,
    /// How many of this crate's commands ran, across every generation: the
    /// count crossing a reload is what proves the command survived it.
    #[serde(default)]
    pub commands_ran: u32,
}

impl Resource for SplineEditorMarker {
    /// One name for every generation's build of the type, so a reload's `init`
    /// finds the value the previous generation left behind.
    fn shared_name() -> Option<&'static str> {
        Some("pill_spline_editor::SplineEditorMarker")
    }
}

// =============================================================================
// Registration
// =============================================================================

/// Registers the editor crate against the host engine. Returns zero on success.
///
/// Idempotent across generations: the marker is created on the first load and
/// counted up on every reload.
/// Public so the generated `host_module_pill_spline_editor` wrapper can carry
/// it as `pill_module_init`.
#[pill_module]
pub fn register(engine: &mut Engine) -> u32 {
    let world = engine.world_mut();
    world.register_persistable_resource::<SplineEditorMarker>();
    let loaded_generations = match world.get_resource_mut::<SplineEditorMarker>() {
        Some(marker) => {
            marker.loaded_generations += 1;
            marker.loaded_generations
        }
        None => {
            world.insert_resource(SplineEditorMarker {
                loaded_generations: 1,
                commands_ran: 0,
            });
            1
        }
    };
    // NOTE: this line is asserted on by `devops/tests/test_editor_crates.py`.
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ECS,
        loaded_generations,
        spline_component_size = std::mem::size_of::<pill_spline::Spline>(),
        "[editor] pill_spline_editor loaded"
    );
    // The extension's editor commands (Stage 14). Keyed by this crate, so
    // each reload's registration replaces the previous generation's set;
    // the handler below drains the ones it owns from the shared inbox.
    pill_editor_core::commands::ensure_command_resources(world);
    if let Some(registry) = world.get_resource_mut::<CommandRegistry>() {
        registry.register(
            "pill_spline_editor",
            vec![
                CommandSpec::new("spline_editor.ping", "Spline Editor: Ping")
                    .menu(&["Spline", "Ping"])
                    .bind(KeymapPreset::Blender, "ctrl+j")
                    .bind(KeymapPreset::Unity, "ctrl+j")
                    .bind(KeymapPreset::Godot, "ctrl+j")
                    .bind(KeymapPreset::Unreal, "ctrl+j"),
            ],
        );
    }
    engine.register_system("pill_spline_editor_commands", spline_editor_commands);
    0
}

/// Runs the editor commands this crate owns, drained from the inbox.
///
/// The shell dispatches every executed command id into
/// [`CommandInbox`]; this system claims its own ids, bumps the marker's
/// counter - the count is what the tests read across a reload - and logs
/// each run.
fn spline_editor_commands(mut inbox: ResMut<CommandInbox>, mut marker: ResMut<SplineEditorMarker>) {
    let (Some(mut inbox), Some(mut marker)) = (inbox.get_mut(), marker.get_mut()) else {
        return;
    };
    let owned: Vec<String> = inbox
        .executed
        .iter()
        .filter(|id| id.starts_with("spline_editor."))
        .cloned()
        .collect();
    if owned.is_empty() {
        return;
    }
    inbox.executed.retain(|id| !owned.contains(id));
    for id in owned {
        marker.commands_ran += 1;
        // NOTE: asserted on by the editor's end-to-end suite.
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            id = %id,
            commands_ran = marker.commands_ran,
            "[spline_editor] command ran"
        );
    }
}
