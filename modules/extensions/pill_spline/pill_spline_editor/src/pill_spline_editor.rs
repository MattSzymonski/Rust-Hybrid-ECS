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
use pill_core::math::Vector3f;
use pill_editor_api::panels::{register_editor_panels, EditorPanelSpec, PanelContext};
use pill_editor_api::EditorWindowButton;
use pill_editor_core::commands::{
    CommandInbox, CommandRegistry, CommandSpec, KeymapPreset, PendingEditorCommands,
};
use pill_editor_core::editor_state::{EditorCommand, EntityListEntry};
use pill_engine::pill_module;
use pill_engine::{
    pill_hot_fn, DebugDraw, DebugStyle, Engine, Entity, FieldValue, Query, ResMut, Resource,
};
use pill_spline::{Spline, MAX_CONTROL_POINTS};
use pill_ui::panel::PanelBuilder;
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
                CommandSpec::new("spline_editor.reseed", "Re-seed Splines")
                    .menu(&["Spline", "Re-seed Splines"]),
                CommandSpec::new("spline_editor.add_point", "Add Control Point")
                    .menu(&["Spline", "Add Control Point"]),
            ],
        );
    }
    // The Spline Tool panel (Stage 16) and the modal's body panel
    // (Stage 24), registered as one set: a generation registers its panels
    // in a single call, so this reload replaces exactly this set, and a
    // panel a later generation drops retires with it. `spline_tool` shows
    // in the dock and detaches into a tool window of its own; the modal's
    // buttons are part of the window request, not of `spline_reseed`.
    register_editor_panels(
        engine,
        "pill_spline_editor",
        &[
            EditorPanelSpec::new("spline_tool", "Spline Tool", spline_tool_panel),
            EditorPanelSpec::new("spline_reseed", "Spline Re-seed", spline_reseed_panel),
        ],
    );
    engine.register_system("pill_spline_editor_commands", spline_editor_commands);
    0
}

/// The Spline Tool: the world's splines, with the extension's actions.
///
/// Panels never mutate the world directly: the buttons queue the crate's
/// command ids, drained by [`spline_editor_commands`] in its own system.
/// Marked `#[pill_hot_fn]` so a body-only edit to the panel live-patches
/// (D19).
#[pill_hot_fn]
pub fn spline_tool_panel(builder: &mut PanelBuilder<'_>, context: &mut PanelContext<'_>) {
    builder.heading("Spline Tool");
    builder.separator();
    let splines: Vec<&EntityListEntry> = context
        .snapshot
        .entities
        .iter()
        .filter(|entry| {
            entry
                .archetype_label
                .split(", ")
                .any(|name| name.ends_with("::Spline"))
        })
        .collect();
    builder.label(&format!("{} splines in the world", splines.len()));
    for entry in splines.iter().take(8) {
        builder.row(|row| {
            row.label(&entry.display);
        });
    }
    builder.separator();
    let reseed = builder.button("Re-seed Splines");
    let add_point = builder.button("Add Control Point");
    if reseed {
        context.work.command("spline_editor.reseed");
    }
    if add_point {
        context.work.command("spline_editor.add_point");
    }
    // The free-standing windows (Stage 24): this button detaches the tool
    // into a window of its own, and this one asks before re-seeding through
    // a modal. Both name registered panels - the tool window shows this
    // panel, the modal shows `spline_reseed` above its request's buttons.
    let detach = builder.button_named("spline.open_window", "Detach Tool Window");
    let confirm = builder.button_named("spline.confirm_reseed", "Re-seed (confirm)");
    if detach {
        context.work.open_tool_window(
            "spline_tool_window",
            "Spline Tool",
            320.0,
            240.0,
            "spline_tool",
        );
    }
    if confirm {
        context.work.open_modal(
            "spline_reseed_confirm",
            "Re-seed Splines",
            380.0,
            170.0,
            "spline_reseed",
            vec![
                EditorWindowButton {
                    label: "Re-seed".to_string(),
                    command: "spline_editor.reseed".to_string(),
                },
                EditorWindowButton {
                    label: "Cancel".to_string(),
                    command: String::new(),
                },
            ],
        );
    }
    builder.label("Control points draw in the scene's debug overlay.");
}

/// The modal's body (Stage 24): what the confirm is about. The buttons
/// below it come from the window request, answered by the shell.
#[pill_hot_fn]
pub fn spline_reseed_panel(builder: &mut PanelBuilder<'_>, _context: &mut PanelContext<'_>) {
    builder.heading("Re-seed Splines");
    builder.separator();
    builder.label("This rewrites every spline's control points.");
    builder.label("The simulation keeps running while this dialog is open.");
}

/// Runs the editor commands this crate owns, drained from the inbox, and
/// keeps the splines' control points in the debug overlay.
///
/// The shell dispatches every executed command id into [`CommandInbox`];
/// this system claims its own ids. `add_point` queues a recorded
/// `SetField` on the first spline's `control_point_count` (undoable through
/// the shared history); `re-seed` rewrites the control points directly,
/// which - like any direct world mutation - is not undoable (the rule
/// Stage 15 states for module tools).
fn spline_editor_commands(
    mut inbox: ResMut<CommandInbox>,
    mut marker: ResMut<SplineEditorMarker>,
    mut splines: Query<(Entity, &mut Spline)>,
    mut debug: ResMut<DebugDraw>,
    mut pending: ResMut<PendingEditorCommands>,
) {
    let (Some(mut inbox), Some(mut marker), Some(mut debug), Some(mut pending)) = (
        inbox.get_mut(),
        marker.get_mut(),
        debug.get_mut(),
        pending.get_mut(),
    ) else {
        return;
    };
    let owned: Vec<String> = inbox
        .executed
        .iter()
        .filter(|id| id.starts_with("spline_editor."))
        .cloned()
        .collect();
    for id in owned {
        inbox.executed.retain(|candidate| candidate != &id);
        marker.commands_ran += 1;
        match id.as_str() {
            "spline_editor.add_point" => {
                // One recorded field write on the first spline; the runner
                // applies it at the frame boundary and the history keeps it.
                // The write is NOT done here: the recorded command is the
                // only writer, so the inverse it computes sees the pre-edit
                // value.
                if let Some((entity, spline)) = splines.iter_mut().next() {
                    let new_count = (spline.control_point_count + 1).min(MAX_CONTROL_POINTS as u32);
                    pending.push_recorded(
                        vec![EditorCommand::SetField {
                            entity,
                            component: std::any::type_name::<Spline>().to_string(),
                            field: "control_point_count".to_string(),
                            value: FieldValue::U32(new_count),
                        }],
                        "spline_editor.add_point",
                        None,
                    );
                    // NOTE: asserted on by the editor's end-to-end suite.
                    pill_core::info!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        entity = %entity,
                        control_points = new_count,
                        "[spline_editor] control point added"
                    );
                }
            }
            "spline_editor.reseed" => {
                let mut reseeded = 0;
                for (_, mut spline) in splines.iter_mut() {
                    reseed_control_points(&mut spline);
                    reseeded += 1;
                }
                // NOTE: asserted on by the editor's end-to-end suite.
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    splines = reseeded,
                    control_points = MAX_CONTROL_POINTS,
                    "[spline_editor] reseeded"
                );
            }
            _ => {}
        }
        // NOTE: asserted on by the editor's end-to-end suite.
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            id = %id,
            commands_ran = marker.commands_ran,
            "[spline_editor] command ran"
        );
    }
    // Every run, keep the control points visible in the debug overlay: a
    // polyline through the used points plus a small cross per point. The
    // OneFrame lifetime means the next collection drops them.
    let style = DebugStyle::default();
    for (_, spline) in splines.iter_mut() {
        let used = (spline.control_point_count as usize).min(MAX_CONTROL_POINTS);
        for index in 0..used {
            let point = spline.control_points[index];
            debug.line(
                [point.x - 0.15, point.y, point.z],
                [point.x + 0.15, point.y, point.z],
                style,
            );
            debug.line(
                [point.x, point.y - 0.15, point.z],
                [point.x, point.y + 0.15, point.z],
                style,
            );
            if index + 1 < used {
                let next = spline.control_points[index + 1];
                debug.line([point.x, point.y, point.z], [next.x, next.y, next.z], style);
            }
        }
    }
}

/// Rewrites a spline's control points as a re-seed: a flat spiral over the
/// origin, deterministic so tests can read it back.
fn reseed_control_points(spline: &mut Spline) {
    for (index, point) in spline.control_points.iter_mut().enumerate() {
        let angle = index as f32 * std::f32::consts::TAU / MAX_CONTROL_POINTS as f32;
        let radius = 2.0 + index as f32 * 0.25;
        *point = Vector3f::new(angle.cos() * radius, 0.0, angle.sin() * radius);
    }
    spline.control_point_count = MAX_CONTROL_POINTS as u32;
}

// panels suite window reload check
