//! Panel registration: what an editor crate registers, and how a
//! registration dies with its binary.
//!
//! # Responsibilities
//!
//! - Defines [`EditorPanelRegistry`], the world resource holding every
//!   registered panel, and [`register_editor_panel`], the helper an editor
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
use crate::EditorRect;

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
}

/// The function an editor panel is built from.
///
/// A plain `fn` pointer, so the body live-patches and the pointer never
/// carries captured state.
pub type PanelBuildFn = fn(&mut PanelBuilder<'_>, &mut PanelContext<'_>);

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
}

impl EditorPanelRegistry {
    /// Claim the resource across a reload, keeping live registrations.
    pub fn ensure(world: &mut pill_engine_core::World) {
        world.register_resource::<EditorPanelRegistry>();
        if world.get_resource::<EditorPanelRegistry>().is_none() {
            world.insert_resource(EditorPanelRegistry::default());
        }
    }

    /// Register one panel for `owner`, replacing that owner's generation.
    ///
    /// Replacing the owner's marker is what retires the owner's previous
    /// registrations: their `Weak` handles lose their last strong count
    /// here, and [`Self::panel`] never returns a dead one.
    pub fn register(&mut self, owner: &str, id: &str, title: &str, build: PanelBuildFn) {
        self.prune();
        let marker = Arc::new(());
        self.generations
            .insert(owner.to_string(), Arc::clone(&marker));
        self.entries.push(EditorPanel {
            owner: owner.to_string(),
            id: id.to_string(),
            title: title.to_string(),
            build,
            alive: Arc::downgrade(&marker),
        });
    }

    /// The newest live registration of `id`.
    pub fn panel(&self, id: &str) -> Option<PanelBuildFn> {
        self.entries
            .iter()
            .rev()
            .find(|entry| entry.id == id && entry.alive.strong_count() > 0)
            .map(|entry| entry.build)
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
// register_editor_panel
// =============================================================================

/// Register one panel for `owner`, claiming the registry if needed.
///
/// The helper an editor crate calls from its registration; it also inserts
/// the registry resource when the calling module registers before the shell
/// has (load order is not fixed).
pub fn register_editor_panel(
    engine: &mut Engine,
    owner: &str,
    id: &str,
    title: &str,
    build: PanelBuildFn,
) {
    let world = engine.world_mut();
    EditorPanelRegistry::ensure(world);
    if let Some(registry) = world.get_resource_mut::<EditorPanelRegistry>() {
        registry.register(owner, id, title, build);
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

    #[test]
    fn re_registering_an_owner_retires_its_previous_generation() {
        let mut registry = EditorPanelRegistry::default();
        registry.register("module_a", "tool", "Tool", paint_marker);
        let first = registry.panel("tool").expect("first registration");
        assert_eq!(first as usize, paint_marker as usize);

        registry.register("module_a", "tool", "Tool", paint_other);
        let second = registry.panel("tool").expect("second registration");
        assert_eq!(second as usize, paint_other as usize);
        // The old generation is dead, even though its entry still sits in
        // the list until the next prune.
        assert_eq!(registry.list().len(), 1);
        assert_eq!(registry.list()[0].title, "Tool");
    }

    #[test]
    fn another_owners_registration_stays_alive() {
        let mut registry = EditorPanelRegistry::default();
        registry.register("module_a", "a_tool", "A", paint_marker);
        registry.register("module_b", "b_tool", "B", paint_other);
        registry.register("module_a", "a_tool", "A", paint_other);
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
}
