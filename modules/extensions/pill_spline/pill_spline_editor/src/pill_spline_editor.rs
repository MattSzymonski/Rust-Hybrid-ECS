//! The spline extension's editor part.
//!
//! # Responsibilities
//!
//! - Registers through the extension ABI in an editor host, after `pill_spline`.
//! - Records each load in the persistable [`SplineEditorMarker`] resource and
//!   logs it, so tests can see that the editor crate loaded, when it reloaded,
//!   and that its state survived the reload.
//!
//! # Design
//!
//! Editor tools for splines (panels, commands) grow here; for now the crate
//! only proves the editor-crate convention: found by its name in the
//! extension's folder, loaded only by an editor host, rebuilt whenever the
//! runtime crate it links reloads.

// External crates
use pill_engine::pill_module;
use pill_engine::{Engine, Resource};
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
    0
}
