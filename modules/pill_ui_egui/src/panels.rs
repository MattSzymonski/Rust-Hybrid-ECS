//! Editor panels registered by a module (W-0038 Stage 11).
//!
//! # Responsibilities
//!
//! - Hold the panels editor crates register: an id, a title, the crate that
//!   drew it, the function that draws it and a liveness marker.
//! - Tell a live panel from one whose module has been unloaded, so the shell
//!   can never call into an unloaded image.
//! - Give each panel a stable slot, which is what a dock tab stores, and keep
//!   the slot across the registering module's reload.
//! - Carry [`PanelContext`], the plain data a panel reads and the commands it
//!   leaves behind.
//!
//! # Design
//!
//! A world resource the frontend inserts and the shell reads each frame. A
//! panel's draw function is a plain `fn` pointer into the registering module's
//! image, so it is only ever called while the liveness marker the module holds
//! is still alive: `Arc::strong_count` is the answer to "is that image still
//! mapped", and the registry refuses to hand out a draw function otherwise.
//!
//! Registering is per id, not per crate: a module that reloads registers the
//! same id again, which replaces the entry (new function, new marker, new
//! epoch) and keeps the slot, so the user's dock layout survives the reload.
//! A module that unloads and never returns leaves its entry in place with a
//! dead marker - the tab says so rather than disappearing.

use std::sync::{Arc, Weak};

use pill_engine_core::{Entity, Resource};

/// What a module holds to keep its panels alive.
///
/// One marker per registration source: the registry stores a [`Weak`] to it,
/// so the moment the module's image goes away the strong count reaches zero
/// and every panel it registered is known to be dead. A unit struct on
/// purpose - it carries no state, only a lifetime.
#[derive(Debug)]
pub struct ModuleLiveness(());

impl ModuleLiveness {
    /// A marker for one registering crate.
    pub fn new() -> Arc<ModuleLiveness> {
        Arc::new(ModuleLiveness(()))
    }
}

impl Default for ModuleLiveness {
    fn default() -> Self {
        ModuleLiveness(())
    }
}

/// The draw function a panel registers.
pub type PanelDrawFn = fn(&mut egui::Ui, &mut PanelContext<'_>);

/// What a panel may ask the editor to do.
///
/// Plain data, queued into [`PanelContext::commands`] and applied by the
/// executable at the frame boundary, the same contract `EditorCommand`
/// follows: a panel never mutates the world itself, so a panel from a module
/// cannot leave the frame half-applied.
#[derive(Clone, Debug, PartialEq)]
pub enum PanelCommand {
    /// Make this entity the editor's selection.
    SelectEntity(Entity),
    /// Write one component field through the editor's recorded edit path, so
    /// the change is one undo step (W-0038 Stages 12 and 13). The value is
    /// text, parsed against the field's current value exactly as the
    /// Inspector's own rows parse it.
    SetField {
        /// The entity to edit.
        entity: Entity,
        /// The component's registered type name.
        component: String,
        /// The field's name.
        field: String,
        /// The new value, as the Inspector would have typed it.
        value: String,
    },
    /// Write one element of an array field, recorded the same way.
    SetArrayElement {
        /// The entity to edit.
        entity: Entity,
        /// The component's registered type name.
        component: String,
        /// The array field's name.
        field: String,
        /// The element's index.
        index: usize,
        /// The new element value, as the Inspector would have typed it.
        value: String,
    },
    /// Ask the shell to show a window of this panel's (W-0038 Stage 13).
    OpenWindow(PanelWindowRequest),
    /// Close one of this panel's windows.
    CloseWindow(String),
    /// Write a line the editor's log shows. A module's only way to say what
    /// it did without owning a log of its own.
    Log(String),
}

/// One window a panel asks the shell for.
///
/// The shell owns the window - its position, its lifetime and its closing on
/// the module's unload - so a module's request is plain data and its content
/// is drawn by the same panel that asked.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelWindowRequest {
    /// The window's identity within its panel.
    pub id: String,
    /// The title the shell draws in the window's bar.
    pub title: String,
    /// The window's first width in logical points.
    pub width: f32,
    /// The window's first height in logical points.
    pub height: f32,
    /// Whether the window is modal: a modal takes the editor's input until it
    /// is answered, and the shell answers it with Enter or Escape.
    pub modal: bool,
}

/// One row of the world's entities, as a panel sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelEntityRow {
    /// The entity's handle.
    pub entity: Entity,
    /// The list label the Entities tab shows.
    pub display: String,
    /// The joined component type names, which is how a panel finds the
    /// entities it cares about without naming a crate's type.
    pub archetype: String,
}

/// One component of the selected entity, as a panel sees it.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelComponentValues {
    /// The component's registered type name.
    pub component: String,
    /// The component's fields, in layout order, rendered as text.
    pub fields: Vec<PanelFieldValue>,
}

/// One field value, rendered for a panel.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelFieldValue {
    /// The field's name.
    pub name: String,
    /// The value, in the Inspector's text form.
    pub value: String,
}

/// The selected entity's components, as a panel sees them.
#[derive(Clone, Debug, PartialEq)]
pub struct PanelEntityDetail {
    /// The entity the detail describes.
    pub entity: Entity,
    /// The entity's components, in layout order.
    pub components: Vec<PanelComponentValues>,
}

/// What a panel reads and writes while it draws.
///
/// Borrowed for the call only: a panel must not keep any of it past the frame,
/// and none of it names a type from the panel's own crate, so nothing here can
/// outlive the module that drew it.
pub struct PanelContext<'a> {
    /// The editor's smoothed frame rate.
    pub frames_per_second: f32,
    /// The selected entity, when one is.
    pub selected_entity: Option<Entity>,
    /// The world's entities, when a panel asked for them.
    pub entities: &'a [PanelEntityRow],
    /// The selected entity's components, when there is a selection.
    pub detail: Option<&'a PanelEntityDetail>,
    /// The window this draw is for: `Some` when the panel is drawing one of
    /// its own windows, `None` when it is drawing its docked tab.
    pub window: Option<&'a str>,
    /// A window id the editor asked this panel to open - the scripted channel's
    /// stand-in for clicking the panel's own button (W-0038 Stage 13).
    pub requested_window: Option<&'a str>,
    /// The answer a modal window took, once: the panel acts on it and the
    /// shell forgets it.
    pub answer: Option<&'a PanelAnswer>,
    /// The commands this panel asks for; the executable applies them at the
    /// frame boundary.
    pub commands: &'a mut Vec<PanelCommand>,
}

/// The answer a modal window took, as the panel that asked reads it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PanelAnswer {
    /// The window that was answered.
    pub window: String,
    /// Whether the answer was Enter rather than Escape.
    pub accepted: bool,
}

/// One registered panel.
#[derive(Clone)]
pub struct PanelRegistration {
    /// The panel's identity: stable across reloads, and what the layout
    /// document stores.
    pub id: String,
    /// The tab's title.
    pub title: String,
    /// The crate that registered it, for the log and the failure text.
    pub source: String,
    /// The function that draws the panel's body.
    pub draw: PanelDrawFn,
    /// The module's liveness marker, dropped when its image unloads.
    liveness: Weak<ModuleLiveness>,
    /// Bumped every time this id is registered again.
    pub epoch: u64,
}

impl PanelRegistration {
    /// Whether the module that registered this panel is still loaded.
    pub fn alive(&self) -> bool {
        self.liveness.strong_count() > 0
    }
}

impl std::fmt::Debug for PanelRegistration {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PanelRegistration")
            .field("id", &self.id)
            .field("title", &self.title)
            .field("source", &self.source)
            .field("epoch", &self.epoch)
            .field("alive", &self.alive())
            .finish()
    }
}

/// One panel a module asks the editor to register.
///
/// A module cannot register into the registry itself: the registry binds each
/// entry to a liveness marker, and only the executable can own that marker's
/// strong handle - a marker kept in the module's own world resource would
/// outlive the module and make an unloaded draw function look alive. So a
/// module queues requests here during its `register` and the executable binds
/// them, which is also what makes a reload's stale entries go dead.
#[derive(Clone, Copy, Debug)]
pub struct PanelRequest {
    /// The panel's identity, stable across reloads.
    pub id: &'static str,
    /// The tab's title.
    pub title: &'static str,
    /// The crate registering it.
    pub source: &'static str,
    /// The function that draws the panel's body.
    pub draw: PanelDrawFn,
}

/// The panel requests queued since the executable last looked.
///
/// Drained, never read twice: a request from a generation that has since
/// unloaded must not be replayed into a live entry.
#[derive(Debug, Default)]
pub struct PanelRegistrations {
    /// The requests, oldest first.
    pub pending: Vec<PanelRequest>,
}

impl Resource for PanelRegistrations {}

impl PanelRegistrations {
    /// Queue one request.
    pub fn push(&mut self, request: PanelRequest) {
        self.pending.push(request);
    }

    /// Take everything queued since the last sync.
    pub fn drain(&mut self) -> Vec<PanelRequest> {
        std::mem::take(&mut self.pending)
    }
}

/// Every panel the loaded modules registered.
#[derive(Debug, Default)]
pub struct PanelRegistry {
    entries: Vec<PanelRegistration>,
    next_epoch: u64,
}

impl Resource for PanelRegistry {}

/// The commands this frame's panels asked for, drained by the executable at
/// the frame boundary.
///
/// A resource rather than a queue the shell owns: the shell is replaced on
/// every reload, and a command queued by a panel of the old generation must
/// still be applied by the executable that survives it.
#[derive(Debug, Default)]
pub struct PanelCommands {
    /// The commands, oldest first.
    pub commands: Vec<PanelCommand>,
}

impl Resource for PanelCommands {}

impl PanelCommands {
    /// Take everything queued since the last frame.
    pub fn drain(&mut self) -> Vec<PanelCommand> {
        std::mem::take(&mut self.commands)
    }

    /// Queue one command, as a panel body does through its `PanelContext`.
    pub fn push(&mut self, command: PanelCommand) {
        self.commands.push(command);
    }
}

impl PanelRegistry {
    /// Register a panel, replacing the entry with the same id.
    ///
    /// The slot - the index the dock tab names - stays the same across a
    /// reload, because the entry is replaced in place rather than appended.
    pub fn register(
        &mut self,
        id: &str,
        title: &str,
        source: &str,
        draw: PanelDrawFn,
        liveness: &Arc<ModuleLiveness>,
    ) -> u16 {
        self.next_epoch += 1;
        let epoch = self.next_epoch;
        let registration = PanelRegistration {
            id: id.to_string(),
            title: title.to_string(),
            source: source.to_string(),
            draw,
            liveness: Arc::downgrade(liveness),
            epoch,
        };
        if let Some(slot) = self.slot(id) {
            self.entries[slot as usize] = registration;
            return slot;
        }
        self.entries.push(registration);
        (self.entries.len() - 1) as u16
    }

    /// The slot a panel id occupies, when it is registered.
    pub fn slot(&self, id: &str) -> Option<u16> {
        self.entries
            .iter()
            .position(|entry| entry.id == id)
            .map(|index| index as u16)
    }

    /// The registration in one slot.
    pub fn get(&self, slot: u16) -> Option<&PanelRegistration> {
        self.entries.get(slot as usize)
    }

    /// The draw function of a live panel, `None` when its module unloaded.
    pub fn draw_of(&self, slot: u16) -> Option<PanelDrawFn> {
        self.get(slot).filter(|entry| entry.alive()).map(|e| e.draw)
    }

    /// The title to show for a slot: the panel's own while it is alive, and
    /// the "module not loaded" text once its module is gone.
    pub fn tab_title(&self, slot: u16) -> String {
        match self.get(slot) {
            Some(entry) if entry.alive() => entry.title.clone(),
            Some(entry) => format!("{} (module not loaded)", entry.title),
            None => "Module panel".to_string(),
        }
    }

    /// Every registration, in slot order.
    pub fn entries(&self) -> &[PanelRegistration] {
        &self.entries
    }

    /// How many panels are registered, live or not.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether no panel is registered.
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Whether the slot's module is still loaded.
    pub fn alive(&self, slot: u16) -> bool {
        self.get(slot).is_some_and(|entry| entry.alive())
    }

    /// Whether a live panel with this id is registered.
    pub fn has_live(&self, id: &str) -> bool {
        self.slot(id).is_some_and(|slot| self.alive(slot))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn draw_panel(ui: &mut egui::Ui, context: &mut PanelContext<'_>) {
        let _ = ui.label("panel");
        let _ = context.frames_per_second;
    }

    #[test]
    fn registering_assigns_slots_in_order() {
        let mut registry = PanelRegistry::default();
        let marker = ModuleLiveness::new();
        assert_eq!(registry.register("a", "A", "shell", draw_panel, &marker), 0);
        assert_eq!(registry.register("b", "B", "shell", draw_panel, &marker), 1);
        assert_eq!(registry.len(), 2);
        assert_eq!(registry.get(1).map(|entry| entry.id.as_str()), Some("b"));
    }

    #[test]
    fn re_registering_replaces_in_place_and_bumps_the_epoch() {
        let mut registry = PanelRegistry::default();
        let first = ModuleLiveness::new();
        let slot = registry.register("a", "A", "shell", draw_panel, &first);
        let epoch = registry.get(slot).map(|entry| entry.epoch).unwrap_or(0);
        let second = ModuleLiveness::new();
        assert_eq!(
            registry.register("a", "A again", "shell", draw_panel, &second),
            slot
        );
        assert_eq!(registry.len(), 1);
        assert!(registry.get(slot).is_some_and(|entry| entry.epoch > epoch));
        assert_eq!(registry.tab_title(slot), "A again");
    }

    #[test]
    fn a_dropped_marker_makes_the_panel_dead_without_removing_it() {
        let mut registry = PanelRegistry::default();
        let marker = ModuleLiveness::new();
        let slot = registry.register("a", "A", "shell", draw_panel, &marker);
        assert!(registry.alive(slot));
        assert!(registry.draw_of(slot).is_some());
        drop(marker);
        assert!(!registry.alive(slot));
        assert!(registry.draw_of(slot).is_none());
        assert_eq!(registry.tab_title(slot), "A (module not loaded)");
        assert_eq!(registry.len(), 1);
    }

    #[test]
    fn a_reload_brings_the_same_slot_back_to_life() {
        let mut registry = PanelRegistry::default();
        let marker = ModuleLiveness::new();
        let slot = registry.register("a", "A", "shell", draw_panel, &marker);
        drop(marker);
        assert!(!registry.alive(slot));
        let marker = ModuleLiveness::new();
        assert_eq!(
            registry.register("a", "A", "shell", draw_panel, &marker),
            slot
        );
        assert!(registry.alive(slot));
    }

    #[test]
    fn an_unknown_slot_has_no_draw_function() {
        let registry = PanelRegistry::default();
        assert!(registry.is_empty());
        assert!(registry.draw_of(9).is_none());
        assert!(!registry.alive(9));
        assert_eq!(registry.tab_title(9), "Module panel");
    }
}
