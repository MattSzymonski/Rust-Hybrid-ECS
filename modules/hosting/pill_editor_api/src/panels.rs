//! Panel registration: what an editor crate registers, and how a
//! registration dies with its binary.
//!
//! # Responsibilities
//!
//! - Defines [`EditorPanelRegistry`], the world resource holding every
//!   registered panel, and [`register_editor_panels`], the helper an editor
//!   crate calls from its registration.
//! - Defines [`PanelContext`] / [`PanelWork`]: the read-only snapshot a panel
//!   draws from and the plain-data work it hands back (queued undoable
//!   commands, command ids for its owning module).
//! - Enforces the liveness rule: a registration never outlives the binary
//!   its function pointer points into.
//!
//! # Design
//!
//! The registry is a world resource so both the registering modules and the
//! shell read one identity; it follows the import registry's `Weak` marker
//! pattern (see `pill_engine_core/src/asset_import_registry.rs`). Each
//! owner's registration stores a `Weak<()>` whose strong half lives in the
//! registry's per-owner marker map: re-registering an owner replaces its
//! marker, dropping the previous strong count, so every registration of that
//! owner's earlier generation is dead by construction. The shell only ever
//! calls the newest live registration, so a reloaded module's old function
//! pointers are never invoked. A `fn` pointer is stored, not a closure, so
//! the panel body stays live-patchable like any other function.
//!
//! Everything a panel hands back is owned plain data ([`PanelWork`]): the
//! shell drains it into its own queues after the panel function returns, so
//! no borrowed value from a module's image crosses the frame.

// Standard library
use std::collections::BTreeMap;
use std::sync::{Arc, Weak};

// External crates
use pill_engine_core::{Engine, Resource};
use pill_ui::panel::PanelBuilder;

// Current crate
use crate::{EditorRect, EditorWindowButton, EditorWindowKind, EditorWindowRequest};

// =============================================================================
// PanelContext
// =============================================================================

/// The read-only data a panel draws from, plus the work it hands back.
///
/// Panels never mutate the world directly: edits are queued as
/// [`pill_editor_core::editor_state::EditorCommand`] batches (undoable, when
/// recorded) or as command ids their owning module handles in its own
/// system.
pub struct PanelContext<'a> {
    /// The editor's per-frame snapshot: entities, systems, pause state.
    pub snapshot: &'a pill_editor_core::editor_state::EditorSnapshot,
    /// The console's capture ring, for panels that show log lines.
    pub logs: &'a pill_core::telemetry::capture::LogBuffer,
    /// The shared selection, for panels that follow it.
    pub selection: &'a pill_editor_core::editor_state::EditorSelection,
    /// The panel's dock rectangle in logical pixels, for size-aware layouts.
    pub rect: EditorRect,
    /// Persistent text buffers by widget id, owned by the shell.
    pub text_buffers: &'a mut std::collections::BTreeMap<String, String>,
    /// Work the panel hands back to the shell, drained after the build.
    pub work: &'a mut PanelWork,
}

/// Plain-data work a panel produces during its build.
#[derive(Default)]
pub struct PanelWork {
    /// Undoable command batches, queued for the frame boundary.
    pub batches: Vec<pill_editor_core::commands::PendingCommandBatch>,
    /// Command ids for the shell's dispatch (module-owned commands
    /// included).
    pub commands: Vec<String>,
    /// An entity the panel selected, applied to the shared selection.
    pub select_entity: Option<pill_engine_core::Entity>,
    /// An asset path the panel selected, applied to the shared selection.
    pub select_asset: Option<String>,
    /// A new minimum console severity to apply (the Console's filter).
    pub console_level: Option<u8>,
    /// A new console channel filter: 0 all, 1 engine, 2 game.
    pub console_channel: Option<u8>,
    /// The console's search text after an edit, for the shell's log line.
    pub console_search: Option<String>,
    /// New preferences JSON to write to the preferences file (D31).
    pub preferences: Option<String>,
    /// A component the panel asked to copy to the clipboard, by type name
    /// (the Inspector's per-component Copy button, Stage 18).
    pub copy_component: Option<String>,
    /// An asset row held down, by its path relative to the asset root
    /// (Stage 18): the executable turns a drag that leaves the window into
    /// an OS drag of that file. Set only while the row is held.
    pub drag_candidate: Option<String>,
    /// A file the panel asked to open in the developer's editor, from a
    /// Problems row (Stage 19).
    pub open_file: Option<crate::EditorOpenRequest>,
    /// Free-standing windows the panel asked for (Stage 24): tool windows
    /// and modal dialogs, opened by the shell, which owns their lifetime.
    pub windows: Vec<EditorWindowRequest>,
    /// A window the panel asks the shell to close (Stage 24), by its id.
    pub close_window: Option<String>,
    /// An asset file the panel asks deleted (Stage 25), by its path relative
    /// to the asset directory; the shell queues the recorded command.
    pub delete_asset: Option<String>,
    /// An asset whose missing `.meta` sidecar the panel asks written
    /// (Stage 25).
    pub write_asset_meta: Option<String>,
}

impl PanelWork {
    /// Queue an edit the history records as one undo step.
    pub fn edit(
        &mut self,
        commands: Vec<pill_editor_core::editor_state::EditorCommand>,
        label: impl Into<String>,
    ) {
        self.batches
            .push(pill_editor_core::commands::PendingCommandBatch {
                commands,
                record: true,
                label: label.into(),
                coalesce_key: None,
            });
    }

    /// Queue a command id for the shell's dispatch.
    pub fn command(&mut self, id: impl Into<String>) {
        self.commands.push(id.into());
    }

    /// Queue an edit whose successive keystrokes merge into one undo step
    /// while they arrive back to back (the same rule slider drags use).
    pub fn edit_coalesced(
        &mut self,
        commands: Vec<pill_editor_core::editor_state::EditorCommand>,
        label: impl Into<String>,
        coalesce_key: impl Into<String>,
    ) {
        self.batches
            .push(pill_editor_core::commands::PendingCommandBatch {
                commands,
                record: true,
                label: label.into(),
                coalesce_key: Some(coalesce_key.into()),
            });
    }

    /// Ask for an entity to become the shared selection.
    pub fn select(&mut self, entity: pill_engine_core::Entity) {
        self.select_entity = Some(entity);
    }

    /// Ask for an asset path to become the shared selection.
    pub fn select_asset(&mut self, path: impl Into<String>) {
        self.select_asset = Some(path.into());
    }

    /// Ask for one component of the selection's entity to be copied.
    pub fn copy_component(&mut self, type_name: impl Into<String>) {
        self.copy_component = Some(type_name.into());
    }

    /// Offer an asset row to an OS drag while it is held.
    pub fn drag_candidate(&mut self, source: impl Into<String>) {
        self.drag_candidate = Some(source.into());
    }

    /// Ask for a file to be opened in the developer's editor.
    pub fn open_file(&mut self, request: crate::EditorOpenRequest) {
        self.open_file = Some(request);
    }

    /// Ask the shell for a free-standing tool window (Stage 24) showing a
    /// registered panel. A window with this id already open is left alone,
    /// so a button that asks every time it is pressed opens it once.
    pub fn open_tool_window(
        &mut self,
        id: impl Into<String>,
        title: impl Into<String>,
        width: f32,
        height: f32,
        panel: impl Into<String>,
    ) {
        self.windows.push(EditorWindowRequest {
            id: id.into(),
            kind: EditorWindowKind::Tool,
            title: title.into(),
            width,
            height,
            panel: panel.into(),
            buttons: Vec::new(),
            draw: pill_ui::DrawList::default(),
        });
    }

    /// Ask the shell for a modal dialog (Stage 24) showing a registered
    /// panel above its buttons. The first button is the default: Enter
    /// answers it, Escape dismisses the modal without a command.
    pub fn open_modal(
        &mut self,
        id: impl Into<String>,
        title: impl Into<String>,
        width: f32,
        height: f32,
        panel: impl Into<String>,
        buttons: Vec<EditorWindowButton>,
    ) {
        self.windows.push(EditorWindowRequest {
            id: id.into(),
            kind: EditorWindowKind::Modal,
            title: title.into(),
            width,
            height,
            panel: panel.into(),
            buttons,
            draw: pill_ui::DrawList::default(),
        });
    }

    /// Ask the shell to close a window this panel opened (Stage 24), by
    /// its id.
    pub fn close_window(&mut self, id: impl Into<String>) {
        self.close_window = Some(id.into());
    }

    /// Ask for the asset at `path` (relative to the asset directory) to be
    /// deleted (Stage 25), through a recorded, undoable command.
    pub fn delete_asset(&mut self, path: impl Into<String>) {
        self.delete_asset = Some(path.into());
    }

    /// Ask for the missing `.meta` sidecar of the asset at `path` to be
    /// written (Stage 25), through a recorded, undoable command.
    pub fn write_asset_meta(&mut self, path: impl Into<String>) {
        self.write_asset_meta = Some(path.into());
    }
}

/// The function an editor panel is built from.
///
/// A plain `fn` pointer, so the body live-patches and the pointer never
/// carries captured state.
pub type PanelBuildFn = fn(&mut PanelBuilder<'_>, &mut PanelContext<'_>);

/// One panel a module registers (Stage 16): its id, the title the dock and
/// the Window menu show, and the build function its binary holds.
///
/// A generation registers its whole set through
/// [`EditorPanelRegistry::register`], one call per load - see there for why
/// the set travels together.
pub struct EditorPanelSpec<'a> {
    /// The panel's stable id, what a `Module(id)` tab and the registry name.
    pub id: &'a str,
    /// The title shown on the tab and in the Window menu.
    pub title: &'a str,
    /// The build function, inside the registering binary.
    pub build: PanelBuildFn,
}

impl<'a> EditorPanelSpec<'a> {
    /// One panel's registration data.
    pub fn new(id: &'a str, title: &'a str, build: PanelBuildFn) -> EditorPanelSpec<'a> {
        EditorPanelSpec { id, title, build }
    }
}

// =============================================================================
// EditorPanelRegistry
// =============================================================================

/// One live registration.
struct EditorPanel {
    /// The registering crate's name, for logs and cleanup.
    owner: String,
    /// The panel's stable id, what a `Module(id)` tab and the registry name.
    id: String,
    /// The title shown on the tab and in the Window menu.
    title: String,
    /// The build function, inside the registering binary.
    build: PanelBuildFn,
    /// Alive exactly while its owner's current generation marker is.
    alive: Weak<()>,
    /// The registration number this entry was stamped with. It changes
    /// exactly when a reload replaces the entry, which is how a window
    /// keyed to a panel tells the old generation's code from the new
    /// one's ([`EditorPanelRegistry::panel_epoch`]).
    epoch: u64,
}

/// A listing of one live panel, for menus and logs.
#[derive(Clone, Debug, PartialEq)]
pub struct EditorPanelInfo {
    /// The panel's id.
    pub id: String,
    /// The panel's title.
    pub title: String,
    /// The registering crate's name.
    pub owner: String,
}

/// Every panel registered by every module, in the world.
#[derive(Default)]
pub struct EditorPanelRegistry {
    entries: Vec<EditorPanel>,
    /// The strong half of each owner's current generation marker. Replacing
    /// one drops the previous strong count, killing its registrations.
    generations: BTreeMap<String, Arc<()>>,
    /// The number the next registration is stamped with; every registration
    /// takes one, so an entry's epoch names exactly one generation's build
    /// of one panel.
    next_epoch: u64,
}

impl EditorPanelRegistry {
    /// Claim the resource across a reload, keeping live registrations.
    pub fn ensure(world: &mut pill_engine_core::World) {
        world.register_resource::<EditorPanelRegistry>();
        if world.get_resource::<EditorPanelRegistry>().is_none() {
            world.insert_resource(EditorPanelRegistry::default());
        }
    }

    /// Register one generation's panels for `owner`, replacing that owner's
    /// previous set.
    ///
    /// The owner's whole set arrives in one call, like the command
    /// registry's, so the generation marker is replaced exactly once per
    /// load: every panel of the previous generation retires here - the ones
    /// the new generation still has, and the ones it dropped - and nothing
    /// of the old code stays reachable through the registry.
    pub fn register(&mut self, owner: &str, panels: &[EditorPanelSpec<'_>]) {
        self.prune();
        let marker = Arc::new(());
        self.generations
            .insert(owner.to_string(), Arc::clone(&marker));
        for panel in panels {
            self.next_epoch += 1;
            self.entries.push(EditorPanel {
                owner: owner.to_string(),
                id: panel.id.to_string(),
                title: panel.title.to_string(),
                build: panel.build,
                alive: Arc::downgrade(&marker),
                epoch: self.next_epoch,
            });
        }
    }

    /// The newest live registration of `id`.
    pub fn panel(&self, id: &str) -> Option<PanelBuildFn> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.id == id && entry.alive.strong_count() > 0)
            .map(|entry| entry.build)
    }

    /// The registration number of the newest live registration of `id`,
    /// `None` when the panel is not registered right now (Stage 24).
    ///
    /// A window keyed to a panel stores this; a value that changed while
    /// the window stayed open means a reload replaced the code that draws
    /// it, and `None` means the registration died (its module unloaded, or
    /// dropped the panel) - in both cases the old window must not draw
    /// through the stale pointer.
    pub fn panel_epoch(&self, id: &str) -> Option<u64> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.id == id && entry.alive.strong_count() > 0)
            .map(|entry| entry.epoch)
    }

    /// Every live panel, newest registration per id first.
    pub fn list(&self) -> Vec<EditorPanelInfo> {
        let mut listings: Vec<EditorPanelInfo> = Vec::new();
        for entry in self.entries.iter().rev() {
            if entry.alive.strong_count() == 0 {
                continue;
            }
            if listings.iter().any(|known| known.id == entry.id) {
                continue;
            }
            listings.push(EditorPanelInfo {
                id: entry.id.clone(),
                title: entry.title.clone(),
                owner: entry.owner.clone(),
            });
        }
        listings.sort_by(|left, right| left.id.cmp(&right.id));
        listings
    }

    /// A signature of the live set, for change detection by the shell.
    pub fn signature(&self) -> Vec<(String, String, String)> {
        self.list()
            .into_iter()
            .map(|info| (info.id, info.title, info.owner))
            .collect()
    }

    /// Drop dead registrations, keeping the entry list bounded across
    /// reloads.
    pub fn prune(&mut self) {
        self.entries.retain(|entry| entry.alive.strong_count() > 0);
    }
}

impl Resource for EditorPanelRegistry {}

// =============================================================================
// register_editor_panels
// =============================================================================

/// Register one generation's panels for `owner`, claiming the registry if
/// needed (Stage 16).
///
/// The helper an editor crate calls from its registration; it also inserts
/// the registry resource when the calling module registers before the shell
/// has (load order is not fixed). The whole set goes in one call:
/// registering twice would replace the owner's generation marker twice, and
/// the second call would retire the first call's panels.
pub fn register_editor_panels(engine: &mut Engine, owner: &str, panels: &[EditorPanelSpec<'_>]) {
    let world = engine.world_mut();
    EditorPanelRegistry::ensure(world);
    if let Some(registry) = world.get_resource_mut::<EditorPanelRegistry>() {
        registry.register(owner, panels);
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use pill_ui::panel::PanelBuilder;

    fn paint_marker(builder: &mut PanelBuilder<'_>, _context: &mut PanelContext<'_>) {
        builder.label("marker");
    }

    fn paint_other(builder: &mut PanelBuilder<'_>, _context: &mut PanelContext<'_>) {
        builder.label("other");
    }

    fn spec<'a>(id: &'a str, title: &'a str, build: PanelBuildFn) -> EditorPanelSpec<'a> {
        EditorPanelSpec::new(id, title, build)
    }

    #[test]
    fn re_registering_an_owner_retires_its_previous_generation() {
        let mut registry = EditorPanelRegistry::default();
        registry.register("module_a", &[spec("tool", "Tool", paint_marker)]);
        let first = registry.panel("tool").expect("first registration");
        assert_eq!(first as usize, paint_marker as usize);

        registry.register("module_a", &[spec("tool", "Tool", paint_other)]);
        let second = registry.panel("tool").expect("second registration");
        assert_eq!(second as usize, paint_other as usize);
        // The old generation is dead, even though its entry still sits in
        // the list until the next prune.
        assert_eq!(registry.list().len(), 1);
        assert_eq!(registry.list()[0].title, "Tool");
    }

    #[test]
    fn a_generation_registers_its_whole_set_at_once() {
        let mut registry = EditorPanelRegistry::default();
        registry.register(
            "module_a",
            &[
                spec("tool", "Tool", paint_marker),
                spec("body", "Body", paint_other),
            ],
        );
        assert!(registry.panel("tool").is_some());
        assert!(registry.panel("body").is_some());

        // The next generation keeps one panel and drops the other: the
        // kept one is the new build function, the dropped one resolves to
        // nothing instead of lingering with its old pointer.
        registry.register("module_a", &[spec("tool", "Tool", paint_other)]);
        assert_eq!(
            registry.panel("tool").expect("kept registration") as usize,
            paint_other as usize
        );
        assert!(registry.panel("body").is_none());
        assert_eq!(registry.list().len(), 1);
    }

    #[test]
    fn another_owners_registration_stays_alive() {
        let mut registry = EditorPanelRegistry::default();
        registry.register("module_a", &[spec("a_tool", "A", paint_marker)]);
        registry.register("module_b", &[spec("b_tool", "B", paint_other)]);
        registry.register("module_a", &[spec("a_tool", "A", paint_other)]);
        let listings = registry.list();
        assert_eq!(listings.len(), 2);
        assert!(registry.panel("b_tool").is_some());
        assert!(registry.panel("a_tool").is_some());
    }

    #[test]
    fn unregistered_ids_resolve_to_nothing() {
        let registry = EditorPanelRegistry::default();
        assert!(registry.panel("missing").is_none());
        assert!(registry.list().is_empty());
        assert!(registry.signature().is_empty());
    }

    #[test]
    fn work_queues_batches_and_commands() {
        let mut work = PanelWork::default();
        work.edit(Vec::new(), "noop");
        work.command("editor.pause_toggle");
        assert_eq!(work.batches.len(), 1);
        assert!(work.batches[0].record);
        assert_eq!(work.batches[0].label, "noop");
        assert_eq!(work.commands, vec!["editor.pause_toggle".to_string()]);
    }

    #[test]
    fn panel_epoch_tracks_re_registration_and_death() {
        let mut registry = EditorPanelRegistry::default();
        registry.register("module_a", &[spec("tool", "Tool", paint_marker)]);
        let first = registry.panel_epoch("tool").expect("first registration");
        registry.register("module_a", &[spec("tool", "Tool", paint_other)]);
        let second = registry.panel_epoch("tool").expect("second registration");
        assert_ne!(first, second, "a re-registration takes a new epoch");

        // The owner's next registration replaces its marker, which retires
        // every earlier entry: a window keyed to the old id must find None
        // and close, not draw through the old pointer.
        registry.register("module_a", &[spec("other", "Other", paint_marker)]);
        assert!(registry.panel_epoch("tool").is_none());
        assert!(registry.panel_epoch("other").is_some());
    }

    #[test]
    fn work_queues_window_requests() {
        let mut work = PanelWork::default();
        work.open_tool_window("window_one", "Detached Tool", 320.0, 240.0, "panel_one");
        work.open_modal(
            "modal_one",
            "Confirm",
            380.0,
            170.0,
            "panel_two",
            vec![EditorWindowButton {
                label: "Re-seed".to_string(),
                command: "spline_editor.reseed".to_string(),
            }],
        );
        work.close_window("window_gone");
        assert_eq!(work.windows.len(), 2);
        assert_eq!(work.windows[0].kind, EditorWindowKind::Tool);
        assert_eq!(work.windows[0].panel, "panel_one");
        assert_eq!(work.windows[0].width, 320.0);
        assert_eq!(work.windows[1].kind, EditorWindowKind::Modal);
        assert_eq!(work.windows[1].buttons[0].command, "spline_editor.reseed");
        assert_eq!(work.close_window.as_deref(), Some("window_gone"));
    }
}
