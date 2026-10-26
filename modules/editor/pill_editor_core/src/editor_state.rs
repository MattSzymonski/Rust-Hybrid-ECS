//! Editor-owned state and commands over the engine.
//!
//! This module is the only place in `pill_editor` that talks to the engine's
//! world and systems. It depends on `pill_engine` directly; `pill_host` only
//! hands the engine over through `RenderingHost::engine()/engine_mut()`.
//!
//! # Responsibilities
//!
//! - Own the tiered [`EditorSnapshot`] types rebuilt from a live `&Engine`.
//! - Own the [`EditorCommand`] queue applied at the frame boundary.
//!
//! # Design
//!
//! - **Reads** are tiered snapshots ([`EditorSnapshot`]) rebuilt from a live
//!   `&Engine` at the frame boundary. Nothing is cached between refreshes
//!   except `Entity` handles and the window layout, so hot reload can never
//!   leave stale metadata behind: components are addressed by name and
//!   re-resolved per capture.
//! - **Mutations** are queued [`EditorCommand`]s flushed in one batch at the
//!   frame boundary, before the frame runs. Every engine call this dispatches
//!   is all-or-nothing; failures are returned as `(index, message)` pairs for
//!   the Console panel, never partially applied.

// Standard library
use std::path::{Path, PathBuf};

// External crates
use pill_engine::component_registry::ComponentFieldDescriptor;
use pill_engine::{Engine, Entity, FieldValue, SystemOwner};

/// One line of the Hierarchy list: cheap to build, rebuilt every refresh.
#[derive(Clone, Debug, PartialEq)]
pub struct EntityListEntry {
    /// The generation-tagged handle; also the selection identity.
    pub entity: Entity,
    /// `"{id}v{generation}"` display label.
    pub display: String,
    /// Number of components attached.
    pub component_count: usize,
    /// Joined component type names, for the list subtitle.
    pub archetype_label: String,
}

impl EntityListEntry {
    /// The label one entity is listed and referenced by.
    ///
    /// The Hierarchy list, an entity reference row and the picker all name an
    /// entity this way, so a reference reads the same wherever it is shown.
    pub fn display_of(entity: Entity) -> String {
        format!("{}v{}", entity.id(), entity.generation())
    }
}

/// One attached component of the selected entity, with its current values.
#[derive(Clone, Debug, PartialEq)]
pub struct ComponentView {
    /// Registered component type name (the stable cross-reload key).
    pub type_name: String,
    /// Whether the component is persistable (schema-migrated across reloads).
    pub persistable: bool,
    /// Whether the component has a non-empty field layout and is editable.
    pub editable: bool,
    /// Field layout descriptors (owned clone, refreshed per capture).
    pub fields: Vec<ComponentFieldDescriptor>,
    /// One current value per field, parallel to `fields`.
    pub values: Vec<FieldValue>,
    /// What each asset reference field names, parallel to `values`: `None`
    /// for a field that is not an asset reference (W-0048). Resolved when
    /// the view is captured, since the shell sees no asset store.
    pub asset_labels: Vec<Option<AssetReferenceLabel>>,
    /// What each entity reference field names, parallel to `values`: `None`
    /// for a field that is not an entity reference (W-0048 Stage 6). Resolved
    /// when the view is captured, for the same reason.
    pub entity_labels: Vec<Option<EntityReferenceLabel>>,
}

/// What an asset reference field currently names, as the Inspector shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AssetReferenceLabel {
    /// The field is cleared: it holds the invalid handle.
    Unset,
    /// The field addresses a live asset, named by its path (or the name its
    /// creator gave it).
    Named(String),
    /// The field addresses a live asset that has no name.
    Unnamed,
    /// The field addresses nothing live: the asset was removed or the handle
    /// is stale.
    Missing,
}

impl AssetReferenceLabel {
    /// The label of an asset value, read through the world's import registry.
    pub fn of(world: &pill_engine::World, value: &FieldValue) -> Option<Self> {
        let FieldValue::Asset {
            asset_type,
            index,
            generation,
        } = value
        else {
            return None;
        };
        if (*index, *generation) == (u32::MAX, u32::MAX) {
            return Some(Self::Unset);
        }
        let identity = asset_type.as_deref().and_then(|asset_type| {
            let registry = world.get_resource::<pill_engine::ImportRegistry>()?;
            let assets = world.get_resource::<pill_engine::AssetManager>()?;
            registry.asset_identity(assets, asset_type, *index, *generation)
        });
        Some(match identity {
            Some(identity) => match identity.name {
                Some(name) => Self::Named(name),
                None => Self::Unnamed,
            },
            None => Self::Missing,
        })
    }

    /// The text the Inspector draws for this label.
    pub fn text(&self) -> &str {
        match self {
            Self::Unset => "None",
            Self::Named(name) => name,
            Self::Unnamed => "(unnamed)",
            Self::Missing => "(missing)",
        }
    }
}

/// What an entity reference field currently names, as the Inspector shows it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EntityReferenceLabel {
    /// The field is cleared: it holds the null handle.
    Unset,
    /// The field addresses a live entity, shown by its Hierarchy label.
    Live(String),
    /// The field addresses nothing live: the entity was destroyed, or the
    /// handle names a slot whose occupant has since been replaced.
    Missing,
}

impl EntityReferenceLabel {
    /// The label of an entity value, checked against the world.
    pub fn of(world: &pill_engine::World, value: &FieldValue) -> Option<Self> {
        let FieldValue::Entity { id, generation } = value else {
            return None;
        };
        let entity = Entity::from_raw(*id, *generation);
        if entity.is_null() {
            return Some(Self::Unset);
        }
        Some(if world.is_entity_valid(entity) {
            Self::Live(EntityListEntry::display_of(entity))
        } else {
            Self::Missing
        })
    }

    /// The text the Inspector draws for this label.
    pub fn text(&self) -> &str {
        match self {
            Self::Unset => "None",
            Self::Live(display) => display,
            Self::Missing => "(missing)",
        }
    }
}

/// What an entity reference row is waiting for: the field a scene click should
/// name, and which entity it belongs to.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EntityPickTarget {
    /// The entity the field belongs to.
    pub entity: Entity,
    /// Registered component type name.
    pub component: String,
    /// Field name.
    pub field: String,
}

/// The world resource carrying an armed entity pick (W-0048 Stage 6).
///
/// The Inspector row arms it through [`EditorCommand::ArmEntityPick`] and the
/// executable that answers scene picks reads it: the shell draws rows and has
/// no world, and the picking code lives in the executable, so a world resource
/// is the one place both can reach.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EntityPickRequest {
    /// The field the next scene click should name, if a row is waiting.
    pub target: Option<EntityPickTarget>,
}

impl EntityPickRequest {
    /// Take the armed target, leaving the request clear: the one caller that
    /// consumes it is the click that answers it.
    pub fn take(&mut self) -> Option<EntityPickTarget> {
        self.target.take()
    }
}

impl pill_engine::Resource for EntityPickRequest {}

/// The expensive tier of the snapshot: only built for the selection.
#[derive(Clone, Debug, PartialEq)]
pub struct EntityDetail {
    /// The entity this detail describes.
    pub entity: Entity,
    /// Every attached component with its current field values.
    pub components: Vec<ComponentView>,
}

/// How many selected entities the snapshot captures a detail for (W-0038
/// Stage 16). A selection past this size still edits the primary and the
/// entities it has details for; the cap keeps a Ctrl+A over a large scene from
/// rebuilding hundreds of component views every frame.
pub const SELECTED_DETAILS_MAX: usize = 64;

/// One row of the Systems tab.
#[derive(Clone, Debug, PartialEq)]
pub struct SystemSummary {
    /// Registration index; the unambiguous toggle key.
    pub index: usize,
    /// Registration name.
    pub name: String,
    /// Human label of the owning module ("project", a module name, or
    /// "unknown").
    pub owner_label: String,
    /// Whether the system participates in frame execution.
    pub enabled: bool,
    /// Whether a hot-patch dispatch slot exists (host built with `hot_patch`).
    pub hot_patchable: bool,
    /// Whether another system shares this name.
    pub name_is_ambiguous: bool,
}

/// One row of the Assets panel: a file or folder the browser knows about.
#[derive(Clone, Debug, PartialEq)]
pub struct AssetListing {
    /// Project-relative path, forward slashes.
    pub path: String,
    /// A short kind label: the asset type, `file`, or `folder`.
    pub kind: String,
    /// Whether the entry is a standalone asset (it holds its own header).
    pub standalone: bool,
    /// Whether a `.meta` sidecar - or a standalone header - carries a guid;
    /// a sourced file without one can get a written sidecar (Stage 25).
    pub has_meta: bool,
    /// The asset's guid when the file names one, in its sidecar or its own
    /// header (W-0049): what a settings document's reference writes, and the
    /// key that names the asset behind such a reference.
    pub guid: Option<String>,
}

/// One standalone asset type a new file can be created for (Stage 25): a
/// row of the create dialog, straight from the loaded modules'
/// registrations.
#[derive(Clone, Debug, PartialEq)]
pub struct StandaloneTypeInfo {
    /// The registered type name, written to the file's `asset_type`.
    pub type_name: String,
    /// The last `::` segment of the type name, for the dialog row.
    pub display_name: String,
    /// The file extension, without the dot.
    pub extension: String,
}

/// The selected asset's detail for the Inspector (Stage 17): its row data
/// and the import settings document the editable rows build on.
#[derive(Clone, Debug, PartialEq)]
pub struct AssetDetail {
    /// Project-relative path, forward slashes.
    pub path: String,
    /// A short kind label: the asset type, `file`, or `folder`.
    pub kind: String,
    /// Whether the entry is a standalone asset (its own document).
    pub standalone: bool,
    /// The import settings document (a standalone asset's body, or the
    /// sidecar's `settings`); `None` for folders and unknown extensions.
    pub settings: Option<serde_json::Value>,
}

/// What a click on an entity row means (W-0038 Stage 16).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SelectionClick {
    /// Replace the selection with the clicked entity.
    Reset,
    /// Add the clicked entity to the selection (Ctrl).
    Add,
    /// Select every row between the primary and the clicked one (Shift).
    Range,
}

/// The editor's shared selection, read by every data panel and the scene.
///
/// One entity set, one asset slot and one resource slot: the Hierarchy and the
/// viewports select entities, the Assets panel selects files, and the
/// Inspector follows whichever slot holds - a resource selection is exclusive
/// with the other two.
///
/// The entity slot is a **set** (W-0038 Stage 16): the last entity in it is the
/// primary, which is what the gizmo acts on, what the scene outlines most
/// brightly and what the Inspector edits first. Ctrl extends the set, Shift
/// extends it over the Entities list's range, and a plain click replaces it.
#[derive(Clone, Default)]
pub struct EditorSelection {
    /// The selected entities, in the order they were selected; the last is
    /// the primary.
    entities: Vec<Entity>,
    /// The selected asset's project-relative path, when one is.
    pub asset: Option<String>,
    /// The selected world resource's stable name, when one is (W-0038
    /// Stage 14). A resource selection is exclusive with the other two: the
    /// Inspector shows one thing at a time.
    pub resource: Option<String>,
}

impl EditorSelection {
    /// The set a click produces, given the list's own row order.
    ///
    /// Pure so the shell (which reads the modifiers) and the scripted channel
    /// (which names the mode) share one rule for what a click means. A range
    /// click needs the primary as its anchor and both rows present in `rows`;
    /// without them it behaves like a plain click.
    pub fn clicked(&self, click: SelectionClick, entity: Entity, rows: &[Entity]) -> Vec<Entity> {
        match click {
            SelectionClick::Reset => vec![entity],
            SelectionClick::Add => {
                let mut entities: Vec<Entity> = self
                    .entities
                    .iter()
                    .copied()
                    .filter(|selected| *selected != entity)
                    .collect();
                entities.push(entity);
                entities
            }
            SelectionClick::Range => {
                let anchor = self.primary();
                let indices = anchor
                    .and_then(|anchor| rows.iter().position(|row| *row == anchor))
                    .zip(rows.iter().position(|row| *row == entity));
                match indices {
                    Some((anchor_index, entity_index)) => {
                        let (first, last) = if anchor_index <= entity_index {
                            (anchor_index, entity_index)
                        } else {
                            (entity_index, anchor_index)
                        };
                        rows[first..=last].to_vec()
                    }
                    // A range needs an anchor the list shows; without one the
                    // click is a plain one.
                    None => vec![entity],
                }
            }
        }
    }

    /// The entity the gizmo acts on and the Inspector edits first: the one
    /// selected last.
    pub fn primary(&self) -> Option<Entity> {
        self.entities.last().copied()
    }

    /// Every selected entity, in selection order.
    pub fn entities(&self) -> &[Entity] {
        &self.entities
    }

    /// Whether `entity` is selected.
    pub fn contains(&self, entity: Entity) -> bool {
        self.entities.contains(&entity)
    }

    /// Replace the whole entity selection.
    pub fn set_entities(&mut self, entities: Vec<Entity>) {
        self.entities = entities;
    }

    /// Select exactly one entity.
    pub fn select_only(&mut self, entity: Entity) {
        self.entities.clear();
        self.entities.push(entity);
    }

    /// Add one entity to the selection, making it the primary.
    ///
    /// Adding an entity that is already selected only makes it primary, so a
    /// Ctrl-click on a selected row does not lose the rest of the set.
    pub fn add_entity(&mut self, entity: Entity) {
        self.entities.retain(|selected| *selected != entity);
        self.entities.push(entity);
    }

    /// Deselect one entity without disturbing the others.
    pub fn remove_entity(&mut self, entity: Entity) {
        self.entities.retain(|selected| *selected != entity);
    }

    /// Drop the entity selection.
    pub fn clear_entities(&mut self) {
        self.entities.clear();
    }
}

impl pill_engine::Resource for EditorSelection {}

/// The complete per-refresh view the editor renders.
///
/// Owned by [`EditorContext`](crate::EditorContext) and handed out by clone;
/// every dock and pop-out window polls its own copy, like `Stats` does.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EditorSnapshot {
    /// Host reload/rollback/patch counter at capture time.
    pub revision: u64,
    /// Live entities, cheapest tier.
    pub entities: Vec<EntityListEntry>,
    /// Detail of the selected entity, if the selection is still alive.
    pub detail: Option<EntityDetail>,
    /// The detail of every selected entity, in selection order (W-0038
    /// Stage 16). The Inspector draws the primary's rows and reads this to
    /// find which other selected entities a component edit can reach, and
    /// which rows the selection disagrees on. Bounded by
    /// [`SELECTED_DETAILS_MAX`].
    pub selected_details: Vec<EntityDetail>,
    /// Registered systems with owner labels.
    pub systems: Vec<SystemSummary>,
    /// Registered components with a usable field layout (the Inspector's
    /// add picker).
    pub components: Vec<RegisteredComponent>,
    /// The asset browser's rows (the Assets panel).
    pub assets: Vec<AssetListing>,
    /// The live standalone asset types (Stage 25): what a create dialog may
    /// offer.
    pub standalone_types: Vec<StandaloneTypeInfo>,
    /// The selected asset's detail (Stage 17): the Inspector's asset
    /// section.
    pub asset_detail: Option<AssetDetail>,
    /// Whether the engine's system phase is paused.
    pub paused: bool,
    /// The gizmo tool's settings (mode, space, snapping); the Scene tab's
    /// context menu checks its items against them.
    pub gizmo: crate::gizmo::GizmoSettings,
    /// Whether the scene grid is on (Stage 23); the toolbox's grid row and
    /// the Scene tab's menu check it against the renderer's switch.
    pub grid_enabled: bool,
    /// The preferences file's values and the current project's remembered
    /// entry (Stage 20); the Preferences tab's rows read them.
    pub preferences: crate::preferences::PreferencesView,
    /// The theme the editor's UI paints with (W-0127 Stage 6): the code
    /// default merged with the user's and the project's layers.
    pub theme: pill_ui_theme::Theme,
    /// How many themes the executable has resolved. The shell installs
    /// [`Self::theme`] when this differs from the generation it applied.
    pub theme_generation: u64,
    /// The layers [`Self::theme`] was merged from, in apply order; the code
    /// default is always first.
    pub theme_layers: Vec<crate::style::ThemeLayerInfo>,
    /// The font bytes the theme's layers resolved (W-0127 Stage 7), installed
    /// with the theme.
    pub theme_fonts: crate::kit::style::ThemeFonts,
    /// How many steps the undo history holds; the menu bar's Undo row greys
    /// out at zero.
    pub undo_depth: usize,
    /// How many steps the redo history holds.
    pub redo_depth: usize,
    /// What the title bar shows for the open scene: the file name, or
    /// `untitled`, with a trailing `*` while it has unsaved edits (W-0048
    /// Stage 9).
    pub scene_title: String,
    /// Whether a scene file is open; Save greys out when nothing is.
    pub scene_open: bool,
    /// Where Save As writes by default: the open scene's path, or
    /// `scenes/untitled.scene` for a world that was never saved.
    pub scene_save_path: String,
    /// Every registered command in registration order (W-0038 Stage 2): the
    /// menu bar draws a row per command whose menu path names the menu, and
    /// a spec that is not here has no row.
    pub commands: Vec<crate::commands::CommandView>,
    /// The Console tab's filters and newest rows (W-0038 Stage 5).
    pub console: crate::console::ConsoleView,
    /// Build errors and warnings, grouped by the module that reported them
    /// (W-0038 Stage 6).
    pub problems: Vec<crate::notices::ProblemRow>,
    /// Command failures from the last apply batch, newest first.
    pub errors: Vec<String>,
    /// The editable world resources and their field values (W-0038 Stage 14).
    pub resources: Vec<ResourceView>,
}

/// One editable resource, as the Inspector's resource section shows it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ResourceView {
    /// The resource's stable name, the key a write addresses.
    pub name: String,
    /// The section's label.
    pub label: String,
    /// The editable fields, in row order.
    pub fields: Vec<ResourceFieldView>,
}

/// One editable field of a resource.
#[derive(Clone, Debug, PartialEq)]
pub struct ResourceFieldView {
    /// The field's name, the key a write addresses.
    pub field: String,
    /// The row's label.
    pub label: String,
    /// The value, as the row's widget shows it.
    pub value: FieldValue,
}

impl pill_engine::Resource for EditorSnapshot {}

impl EditorSnapshot {
    /// Rebuild every tier from a live engine. Never mutates the world.
    pub fn capture_list(
        engine: &Engine,
        revision: u64,
        extension_names: &[String],
        errors: Vec<String>,
    ) -> Self {
        Self {
            revision,
            entities: Self::capture_entities(engine),
            detail: None,
            selected_details: Vec::new(),
            systems: Self::capture_systems(engine, extension_names),
            components: Self::capture_components(engine),
            assets: Vec::new(),
            standalone_types: Vec::new(),
            asset_detail: None,
            paused: engine.systems_paused(),
            gizmo: engine
                .world()
                .get_resource::<crate::gizmo::GizmoState>()
                .map(|state| state.settings)
                .unwrap_or_default(),
            grid_enabled: engine
                .world()
                .get_resource::<pill_renderer_api::SurfaceCameras>()
                .map(|cameras| cameras.grid.enabled)
                .unwrap_or(false),
            preferences: crate::preferences::PreferencesView::default(),
            // The code default until the executable resolves its layers
            // (W-0127 Stage 6): every postures' first frame paints the editor
            // look rather than the crate's neutral fallback.
            theme: crate::style::default_theme(),
            theme_generation: 1,
            theme_layers: vec![crate::style::ThemeLayerInfo::code()],
            theme_fonts: crate::kit::style::ThemeFonts::default(),
            undo_depth: 0,
            redo_depth: 0,
            scene_title: engine
                .world()
                .get_resource::<crate::scene_workflow::SceneSession>()
                .map(crate::scene_workflow::SceneSession::title)
                .unwrap_or_else(|| "untitled".to_string()),
            scene_open: engine
                .world()
                .get_resource::<crate::scene_workflow::SceneSession>()
                .is_some_and(|session| session.open.is_some()),
            scene_save_path: engine
                .world()
                .get_resource::<crate::scene_workflow::SceneSession>()
                .map(crate::scene_workflow::default_save_path)
                .unwrap_or_else(|| "scenes/untitled.scene".to_string()),
            commands: engine
                .world()
                .get_resource::<crate::commands::CommandRegistry>()
                .map(crate::commands::CommandRegistry::views)
                .unwrap_or_default(),
            console: engine
                .world()
                .get_resource::<crate::console::ConsoleState>()
                .map(crate::console::ConsoleState::view)
                .unwrap_or_default(),
            problems: engine
                .world()
                .get_resource::<crate::notices::EditorNotices>()
                .map(|notices| notices.problems.clone())
                .unwrap_or_default(),
            errors,
            resources: Self::capture_resources(engine),
        }
    }

    /// The editable resources' values, read through their curated tables.
    ///
    /// The engine keeps no resource reflection, so a resource that is not in
    /// [`crate::resources::EDITABLE_RESOURCES`] is not captured at all - there
    /// is nothing to read it through.
    fn capture_resources(engine: &Engine) -> Vec<ResourceView> {
        let world = engine.world();
        let Some(settings) = world
            .get_resource::<crate::gizmo::GizmoState>()
            .map(|state| state.settings)
        else {
            return Vec::new();
        };
        crate::resources::EDITABLE_RESOURCES
            .iter()
            .map(|entry| ResourceView {
                name: entry.name.to_string(),
                label: entry.label.to_string(),
                fields: entry
                    .fields
                    .iter()
                    .filter_map(|(field, label)| {
                        crate::gizmo::resource_field_value(&settings, entry.name, field)
                            .ok()
                            .map(|value| ResourceFieldView {
                                field: field.to_string(),
                                label: label.to_string(),
                                value,
                            })
                    })
                    .collect(),
            })
            .collect()
    }

    /// The entity tier - the only expensive one: one row per live entity,
    /// each with owned label strings. A 200k-entity world spends real time
    /// here in debug builds. The editor captures it every frame, because the
    /// rows must follow a system that spawns, despawns or re-composes an
    /// entity; caching it needs a change signal the world does not expose yet
    /// (W-0089's follow-up: `entity_count` alone misses a spawn and a despawn
    /// in the same frame).
    pub fn capture_entities(engine: &Engine) -> Vec<EntityListEntry> {
        engine
            .world()
            .entity_rows()
            .into_iter()
            .map(|row| {
                let display = EntityListEntry::display_of(row.entity);
                let archetype_label = row.components.join(", ");
                let component_count = row.components.len();
                EntityListEntry {
                    entity: row.entity,
                    display,
                    component_count,
                    archetype_label,
                }
            })
            .collect()
    }

    /// The system tier: one summary per registered system; cheap.
    pub fn capture_systems(engine: &Engine, extension_names: &[String]) -> Vec<SystemSummary> {
        engine
            .system_snapshots()
            .into_iter()
            .map(|system| SystemSummary {
                index: system.index,
                owner_label: owner_label(system.owner, extension_names),
                name: system.name,
                enabled: system.enabled,
                hot_patchable: system.hot_patchable,
                name_is_ambiguous: system.name_is_ambiguous,
            })
            .collect()
    }

    /// The component tier: the registry's usable field layouts; cheap.
    pub fn capture_components(engine: &Engine) -> Vec<RegisteredComponent> {
        registered_components(engine.world())
    }

    /// Rebuild the expensive detail tier for one selected entity.
    pub fn capture_detail(engine: &Engine, entity: Entity) -> Option<EntityDetail> {
        let world = engine.world();
        if !world.is_entity_valid(entity) {
            return None;
        }
        let component_names = world.entity_component_names(entity)?;
        let components = component_names
            .into_iter()
            .filter_map(|type_name| {
                let component_id = world.resolve_entity_component_id(entity, &type_name)?;
                let persistable = world.component_is_persistable(component_id);
                let (fields, editable) = match world.component_field_layout(component_id) {
                    Some(layout) if !layout.is_empty() => (layout.to_vec(), true),
                    _ => (Vec::new(), false),
                };
                // Values are only decodable when a layout exists; a component
                // with no layout still appears, read-only.
                let values = if editable {
                    world
                        .read_component_fields(entity, &type_name)
                        .unwrap_or_default()
                } else {
                    Vec::new()
                };
                let asset_labels = values
                    .iter()
                    .map(|value| AssetReferenceLabel::of(world, value))
                    .collect();
                let entity_labels = values
                    .iter()
                    .map(|value| EntityReferenceLabel::of(world, value))
                    .collect();
                Some(ComponentView {
                    type_name,
                    persistable,
                    editable,
                    fields,
                    values,
                    asset_labels,
                    entity_labels,
                })
            })
            .collect();
        Some(EntityDetail { entity, components })
    }
}

/// The asset value `reference` names - a path relative to `res`, or a guid in
/// the world's asset database - loading the asset first when it is not loaded.
///
/// The value carries the type the asset loaded as, so the field write that
/// follows refuses an asset of another type than the field references.
fn resolve_asset_reference(engine: &mut Engine, reference: &str) -> Result<FieldValue, String> {
    let world = engine.world_mut();
    let registry = world
        .get_resource::<pill_engine::ImportRegistry>()
        .cloned()
        .ok_or("no asset types are registered")?;
    let path = match pill_engine::AssetGuid::parse(reference) {
        Some(guid) => world
            .get_resource::<pill_engine::AssetDatabase>()
            .and_then(|database| database.by_guid(guid))
            .map(|record| record.path.clone())
            .ok_or_else(|| format!("no asset in the project has the guid {guid}"))?,
        None => reference.to_owned(),
    };
    let assets = world
        .get_resource_mut::<pill_engine::AssetManager>()
        .ok_or("the world has no asset store")?;
    let outcome = registry
        .load_by_path(
            assets,
            std::path::Path::new(&path),
            pill_engine::MetadataPolicy::CreateIfMissing,
        )
        .map_err(|error| error.to_string())?;
    Ok(FieldValue::Asset {
        asset_type: Some(outcome.type_name),
        index: outcome.index,
        generation: outcome.generation,
    })
}

/// Label a system owner using the host's extension names.
///
/// `SystemOwner(0)` is the project, `SystemOwner(n)` labels module `n - 1`;
/// anything out of range renders as "unknown".
fn owner_label(owner: SystemOwner, extension_names: &[String]) -> String {
    match owner.0 {
        0 => "project".to_string(),
        index => extension_names
            .get(index as usize - 1)
            .cloned()
            .unwrap_or_else(|| "unknown".to_string()),
    }
}

/// One component to place on a newly created entity.
#[derive(Clone, Debug, PartialEq)]
pub struct ComponentSeed {
    /// Registered component type name.
    pub type_name: String,
    /// Field values to write over the zero-initialised image. Fields left out
    /// stay zero.
    pub fields: Vec<(String, FieldValue)>,
}

/// One entry of the Inspector's "add component" picker.
#[derive(Clone, Debug, PartialEq)]
pub struct RegisteredComponent {
    /// Registered type name (the stable cross-reload key).
    pub type_name: String,
    /// True when some generation of this type has a non-empty field layout,
    /// so the editor can zero-initialise an image of it (the §2.2 rule).
    pub addable: bool,
}

/// Registered component types, deduplicated by name, for the add picker.
///
/// The registry legitimately remembers one entry per reload generation of a
/// type name, so a type is offered once and is addable when any of its
/// generations carries a field layout.
pub fn registered_components(world: &pill_engine::World) -> Vec<RegisteredComponent> {
    use std::collections::HashMap;
    let mut addable_by_name: HashMap<String, bool> = HashMap::new();
    for (name, component_id) in world.registered_components() {
        let addable = world
            .component_field_layout(component_id)
            .is_some_and(|layout| !layout.is_empty());
        addable_by_name
            .entry(name)
            .and_modify(|flag| *flag |= addable)
            .or_insert(addable);
    }
    let mut options = addable_by_name
        .into_iter()
        .map(|(type_name, addable)| RegisteredComponent { type_name, addable })
        .collect::<Vec<_>>();
    options.sort_by(|left, right| left.type_name.cmp(&right.type_name));
    options
}

/// A queued editor mutation, flushed at the frame boundary.
#[derive(Clone, Debug, PartialEq)]
pub enum EditorCommand {
    /// Create an entity carrying the seeded components (empty vec = empty).
    CreateEntity {
        /// Components to attach; each must have a registered field layout.
        components: Vec<ComponentSeed>,
    },
    /// Destroy one entity.
    DestroyEntity {
        /// The entity to remove.
        entity: Entity,
    },
    /// Add a zero-initialised component to a live entity.
    AddComponent {
        /// The target entity.
        entity: Entity,
        /// Registered component type name.
        component: String,
    },
    /// Remove a component from a live entity (removing the last one destroys
    /// the entity).
    RemoveComponent {
        /// The target entity.
        entity: Entity,
        /// Registered component type name.
        component: String,
    },
    /// Re-create a destroyed entity from captured component images.
    ///
    /// The world assigns a fresh handle; the recording layer reports the
    /// `(previous, assigned)` pair so the history remaps older entries to it.
    /// Components without a field layout cannot be rebuilt and are absent
    /// from `components`: an entity carrying one restores without it.
    RestoreEntity {
        /// The handle the entity had before it was destroyed.
        previous: Entity,
        /// Component images to recreate, values included.
        components: Vec<ComponentSeed>,
    },
    /// Re-attach one component with captured field values, the inverse of
    /// removing it.
    RestoreComponent {
        /// The target entity.
        entity: Entity,
        /// Registered component type name.
        component: String,
        /// Field values to write over the zero-initialised image.
        fields: Vec<(String, FieldValue)>,
    },
    /// Write one field of a component.
    SetField {
        /// The target entity.
        entity: Entity,
        /// Registered component type name.
        component: String,
        /// Field name.
        field: String,
        /// New value.
        value: FieldValue,
    },
    /// Point an asset reference field at the asset `reference` names - a path
    /// relative to `res` or a 32-digit guid - or clear it with `None`
    /// (W-0048).
    ///
    /// Resolved at the frame boundary, where the asset is loaded first when
    /// it is not loaded yet; it then writes like a [`Self::SetField`], whose
    /// inverse it records.
    SetFieldAsset {
        /// The target entity.
        entity: Entity,
        /// Registered component type name.
        component: String,
        /// Field name.
        field: String,
        /// The asset's path or guid, or `None` to clear the reference.
        reference: Option<String>,
    },
    /// Point an entity reference field at `reference`, or clear it with `None`
    /// (W-0048 Stage 6).
    ///
    /// The handle is checked against the world when the batch is applied, so a
    /// reference to an entity that has gone by then is refused rather than
    /// written; a write records the [`Self::SetField`] that restores what the
    /// field held.
    SetFieldEntity {
        /// The target entity.
        entity: Entity,
        /// Registered component type name.
        component: String,
        /// Field name.
        field: String,
        /// The entity to reference, or `None` to clear the reference.
        reference: Option<Entity>,
    },
    /// Wait for the next scene click to name an entity reference field
    /// (W-0048 Stage 6).
    ///
    /// The row's target button arms it and the scene click answers it; neither
    /// is an edit, so nothing is recorded until the click names an entity.
    ArmEntityPick {
        /// The entity the field belongs to.
        entity: Entity,
        /// Registered component type name.
        component: String,
        /// Field name.
        field: String,
    },
    /// Write one element of a fixed-size array field.
    SetArrayElement {
        /// The target entity.
        entity: Entity,
        /// Registered component type name.
        component: String,
        /// Array field name.
        field: String,
        /// Element index.
        index: usize,
        /// New element value.
        value: FieldValue,
    },
    /// Enable or disable a system by registration index.
    SetSystemEnabled {
        /// Registration index from the systems snapshot.
        index: usize,
        /// Desired state.
        enabled: bool,
    },
    /// Pause or resume the engine's system phase.
    SetPaused {
        /// Desired pause state.
        paused: bool,
    },
    /// Choose what the transform gizmo edits (Stage 20): the tool's mode,
    /// its space, or whether dragging snaps. Absent fields keep their value.
    SetGizmoTool {
        /// The mode to select.
        mode: Option<crate::gizmo::GizmoMode>,
        /// The space to select.
        space: Option<crate::gizmo::GizmoSpace>,
        /// The snapping toggle's new state.
        snap: Option<bool>,
    },
    /// Turn the scene grid on or off (Stage 23): the infinite ground the
    /// editor's view-bound surfaces draw.
    SetGrid {
        /// The grid's new state.
        enabled: bool,
    },
    /// Run exactly one system phase on the next frame, then pause again.
    StepOnce,
    /// Copy files from outside the project into the assets directory (a
    /// drag-and-drop import, Stage 18). Undo removes the copies; a file of
    /// the same name is overwritten.
    ImportFiles {
        /// Absolute source paths, as the OS reported them.
        sources: Vec<String>,
        /// The destination directory (the project's asset root).
        destination: String,
    },
    /// Delete the copies an import created (its inverse, and a redoable step
    /// of its own); it carries the origins so redo can copy them again.
    RemoveFiles {
        /// The created copies, absolute paths.
        paths: Vec<String>,
        /// The original source paths, the redo's sources.
        sources: Vec<String>,
        /// Where the copies live (the redo's destination).
        destination: String,
    },
    /// Create a standalone asset of `type_name` called `name` in `folder`
    /// (Stage 25), both relative to the asset directory.
    ///
    /// The file is the type's default document with a fresh guid, written so
    /// an existing file is never replaced, then imported at once; its
    /// inverse is the deletion of the new file.
    CreateStandaloneAsset {
        /// The registered standalone type's name.
        type_name: String,
        /// The destination folder, `/`-separated and relative; empty for the
        /// asset directory's own root.
        folder: String,
        /// The name the user typed; the type's naming rules add the
        /// extension.
        name: String,
    },
    /// Delete the asset file at `path`, relative to the asset directory,
    /// together with its `.meta` sidecar (Stage 25).
    ///
    /// The apply captures both files' bytes before deleting them; the
    /// inverse is the restore that puts them back, the sidecar's guid
    /// included, so the asset keeps its identity across an undo.
    DeleteAssetFile {
        /// The asset's path relative to the asset directory.
        path: String,
    },
    /// Restore the asset files a delete captured (Stage 25): the file's
    /// bytes, and the sidecar's when it had one. The inverse is the delete,
    /// so the pair round-trips; like the other restore commands this is
    /// never a recorded edit of its own.
    RestoreAssetFile {
        /// The asset's path relative to the asset directory.
        path: String,
        /// The file's bytes, exactly as they were.
        bytes: Vec<u8>,
        /// The `.meta` sidecar's bytes, when one existed.
        meta: Option<Vec<u8>>,
    },
    /// Move or rename the asset at `from` to `to`, both relative to the
    /// asset directory (Stage 25).
    ///
    /// The `.meta` sidecar travels with the file and the asset watcher
    /// follows the move, so the loaded asset keeps its handle and guid; the
    /// inverse moves it back.
    MoveAssetFile {
        /// The asset's current path relative to the asset directory.
        from: String,
        /// Where it goes, relative to the asset directory.
        to: String,
    },
    /// Write the `.meta` sidecar the sourced asset at `path` is missing
    /// (Stage 25), with the type's defaults and a fresh guid. A no-op - and
    /// so nothing recorded - where a sidecar exists; the inverse deletes
    /// the written sidecar.
    WriteAssetMeta {
        /// The asset's path relative to the asset directory.
        path: String,
    },
    /// Replace the import settings of the asset at `path` - or a standalone
    /// asset's own document - with `settings` (Stage 25). The inverse
    /// carries the previous value, which the apply reads before writing.
    SetAssetSettings {
        /// The asset's path relative to the asset directory.
        path: String,
        /// The new settings (or document) body.
        settings: serde_json::Value,
    },
    /// Set one field of one editor-editable resource (Stage 25).
    ///
    /// The engine keeps no resource reflection, so the resources this can
    /// name are curated in [`crate::gizmo`], field by field; the inverse
    /// carries the previous value.
    SetResourceField {
        /// The resource's stable name (its type path).
        resource: String,
        /// The field's name within the resource.
        field: String,
        /// The new value.
        value: FieldValue,
    },
    /// Set the shared selection (Stage 25). Consecutive selection changes
    /// merge into one history step through a coalesce key; the inverse
    /// carries the previous pair, so undo goes back one selection at a
    /// time.
    SetSelection {
        /// The selected entities, in selection order (the last is primary);
        /// empty selects none.
        entities: Vec<Entity>,
        /// The selected asset's path, when one is.
        asset: Option<String>,
    },
    /// Select one world resource for the Inspector (W-0038 Stage 14).
    ///
    /// Its own command rather than a third field of [`EditorCommand::SetSelection`]:
    /// selecting a resource clears the entity and asset slots, and an undo of
    /// one selection must not resurrect an unrelated one.
    SelectResource {
        /// The resource's stable name, `None` to clear the selection.
        resource: Option<String>,
    },
    /// Restore all three selection slots as one recorded step.
    ///
    /// The inverse a resource selection applies: picking a resource clears the
    /// entity and asset slots, so undoing it has to bring them back rather than
    /// leave the Inspector pointing at nothing.
    RestoreSelection {
        /// The entity slots to restore.
        entities: Vec<Entity>,
        /// The asset slot to restore.
        asset: Option<String>,
        /// The resource slot to restore.
        resource: Option<String>,
    },
    /// Write preferences file values (Stage 20): one `(key, text)` pair per
    /// edited row, applied as a read-modify-write of the preferences
    /// document so keys other writers own survive. A pair whose text is
    /// blank writes nothing; the file's live reader applies the values
    /// (history limits, snapping steps) on its next look.
    SetPreferences {
        /// `(key, value)` pairs; keys are the file's own, dots nest.
        values: Vec<(String, String)>,
    },
    /// Start an empty scene: every entity goes, no file is open (W-0048
    /// Stage 9).
    ///
    /// A document action, not an edit: it is not recorded, and the history it
    /// would otherwise fill names entities this world no longer has.
    SceneNew,
    /// Open the scene at `path`, replacing the world's entities.
    SceneOpen {
        /// The scene's path below the asset directory.
        path: String,
    },
    /// Write the world to the open scene, keeping its guid.
    SceneSave,
    /// Write the world to `path`, taking over that file's guid when it exists
    /// and a fresh one otherwise.
    SceneSaveAs {
        /// The scene's path below the asset directory.
        path: String,
    },
}

/// What one applied command hands back to the recording layer.
struct CommandOutcome {
    /// The command that reverses this one, when one exists.
    inverse: Option<EditorCommand>,
    /// A `(previous, assigned)` handle pair when the command re-created an
    /// entity; the history remaps older entries to the new handle.
    remap: Option<(Entity, Entity)>,
}

impl CommandOutcome {
    /// A successful command that produced an inverse.
    fn inverse(inverse: EditorCommand) -> Self {
        Self {
            inverse: Some(inverse),
            remap: None,
        }
    }

    /// A successful command without an inverse (engine controls, restores).
    fn plain() -> Self {
        Self {
            inverse: None,
            remap: None,
        }
    }
}

/// The mounted asset directory the asset commands resolve their relative
/// paths against.
fn asset_authoring_root() -> Result<PathBuf, String> {
    pill_engine::asset_store::mounted_directory().ok_or("no asset directory is mounted".to_owned())
}

/// `path` as a `/`-separated string, the form asset commands carry.
fn asset_slash(path: &Path) -> String {
    path.to_string_lossy().replace('\\', "/")
}

/// Write `bytes` to `file` through a temporary sibling, so a reader never
/// sees a half-written file; the parent directory is created when missing.
fn write_asset_bytes(file: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = file.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("cannot create `{}`: {error}", parent.display()))?;
    }
    let file_name = file
        .file_name()
        .ok_or_else(|| format!("`{}` has no file name", file.display()))?;
    let temporary = file.with_file_name(format!(
        ".{}.{}.tmp",
        file_name.to_string_lossy(),
        std::process::id()
    ));
    std::fs::write(&temporary, bytes)
        .map_err(|error| format!("cannot write `{}`: {error}", temporary.display()))?;
    std::fs::rename(&temporary, file).map_err(|error| {
        let _ = std::fs::remove_file(&temporary);
        format!("cannot write `{}`: {error}", file.display())
    })
}

/// The result of [`EditorCommand::apply_recorded`].
#[derive(Debug, Default)]
pub struct RecordedApply {
    /// One `(index, message)` per failed command.
    pub failures: Vec<(usize, String)>,
    /// Inverses of the successful edits, in undo order (reverse application
    /// order).
    pub inverses: Vec<EditorCommand>,
    /// True when every command succeeded and produced an inverse; only then
    /// may the batch be recorded as one undo step.
    pub fully_invertible: bool,
    /// `(previous, assigned)` handle pairs from entity restores.
    pub remaps: Vec<(Entity, Entity)>,
}

impl EditorCommand {
    /// Whether this command is an undoable edit.
    ///
    /// Engine controls (pause, step) and the restore commands themselves are
    /// not edits: they never enter the history. A system toggle is an edit -
    /// it is persisted project state - so it records.
    pub fn recordable(&self) -> bool {
        matches!(
            self,
            EditorCommand::CreateEntity { .. }
                | EditorCommand::DestroyEntity { .. }
                | EditorCommand::AddComponent { .. }
                | EditorCommand::RemoveComponent { .. }
                | EditorCommand::SetField { .. }
                | EditorCommand::SetFieldAsset { .. }
                | EditorCommand::SetFieldEntity { .. }
                | EditorCommand::SetArrayElement { .. }
                | EditorCommand::ArmEntityPick { .. }
                | EditorCommand::SetSystemEnabled { .. }
                | EditorCommand::ImportFiles { .. }
                | EditorCommand::RemoveFiles { .. }
                | EditorCommand::CreateStandaloneAsset { .. }
                | EditorCommand::DeleteAssetFile { .. }
                | EditorCommand::MoveAssetFile { .. }
                | EditorCommand::WriteAssetMeta { .. }
                | EditorCommand::SetAssetSettings { .. }
                | EditorCommand::SetResourceField { .. }
                | EditorCommand::SetSelection { .. }
                | EditorCommand::SelectResource { .. }
                | EditorCommand::RestoreSelection { .. }
        )
    }

    /// Component and field identifiers this command addresses.
    ///
    /// Field identifiers are `"component.field"`. The history keeps them per
    /// entry so a reload can drop entries whose types or fields are gone.
    pub fn touched_identifiers(&self) -> Vec<String> {
        let mut identifiers = Vec::new();
        match self {
            EditorCommand::SetField {
                component, field, ..
            }
            | EditorCommand::SetFieldAsset {
                component, field, ..
            }
            | EditorCommand::SetFieldEntity {
                component, field, ..
            }
            | EditorCommand::ArmEntityPick {
                component, field, ..
            }
            | EditorCommand::SetArrayElement {
                component, field, ..
            } => {
                identifiers.push(component.clone());
                identifiers.push(format!("{component}.{field}"));
            }
            EditorCommand::AddComponent { component, .. }
            | EditorCommand::RemoveComponent { component, .. }
            | EditorCommand::RestoreComponent { component, .. } => {
                identifiers.push(component.clone());
            }
            EditorCommand::CreateEntity { components }
            | EditorCommand::RestoreEntity { components, .. } => {
                for seed in components {
                    identifiers.push(seed.type_name.clone());
                    for (field, _) in &seed.fields {
                        identifiers.push(format!("{}.{}", seed.type_name, field));
                    }
                }
            }
            EditorCommand::DestroyEntity { .. }
            | EditorCommand::SetSystemEnabled { .. }
            | EditorCommand::SetPaused { .. }
            | EditorCommand::SetGizmoTool { .. }
            | EditorCommand::SetGrid { .. }
            | EditorCommand::StepOnce
            | EditorCommand::SetPreferences { .. }
            | EditorCommand::ImportFiles { .. }
            | EditorCommand::RemoveFiles { .. }
            | EditorCommand::CreateStandaloneAsset { .. }
            | EditorCommand::DeleteAssetFile { .. }
            | EditorCommand::RestoreAssetFile { .. }
            | EditorCommand::MoveAssetFile { .. }
            | EditorCommand::WriteAssetMeta { .. }
            | EditorCommand::SetAssetSettings { .. }
            | EditorCommand::SetResourceField { .. }
            | EditorCommand::SetSelection { .. }
            | EditorCommand::SelectResource { .. }
            | EditorCommand::RestoreSelection { .. }
            | EditorCommand::SceneNew
            | EditorCommand::SceneOpen { .. }
            | EditorCommand::SceneSave
            | EditorCommand::SceneSaveAs { .. } => {}
        }
        identifiers
    }

    /// Replace one entity handle with another wherever this command names it.
    ///
    /// Used after an undo re-created a destroyed entity under a fresh handle:
    /// every remaining history entry must follow the entity to its new
    /// identity, or a later undo would target the dead handle.
    pub fn remap_entity(&mut self, previous: Entity, assigned: Entity) {
        match self {
            EditorCommand::DestroyEntity { entity }
            | EditorCommand::AddComponent { entity, .. }
            | EditorCommand::RemoveComponent { entity, .. }
            | EditorCommand::RestoreComponent { entity, .. }
            | EditorCommand::SetField { entity, .. }
            | EditorCommand::SetFieldAsset { entity, .. }
            | EditorCommand::SetFieldEntity { entity, .. }
            | EditorCommand::ArmEntityPick { entity, .. }
            | EditorCommand::SetArrayElement { entity, .. } => {
                if *entity == previous {
                    *entity = assigned;
                }
            }
            EditorCommand::RestoreEntity { previous: old, .. } => {
                if *old == previous {
                    *old = assigned;
                }
            }
            EditorCommand::SetSelection { entities, .. }
            | EditorCommand::RestoreSelection { entities, .. } => {
                for entity in entities.iter_mut() {
                    if *entity == previous {
                        *entity = assigned;
                    }
                }
            }
            EditorCommand::CreateEntity { .. }
            | EditorCommand::SetSystemEnabled { .. }
            | EditorCommand::SetPaused { .. }
            | EditorCommand::SetGizmoTool { .. }
            | EditorCommand::SetGrid { .. }
            | EditorCommand::StepOnce
            | EditorCommand::SetPreferences { .. }
            | EditorCommand::ImportFiles { .. }
            | EditorCommand::RemoveFiles { .. }
            | EditorCommand::CreateStandaloneAsset { .. }
            | EditorCommand::DeleteAssetFile { .. }
            | EditorCommand::RestoreAssetFile { .. }
            | EditorCommand::MoveAssetFile { .. }
            | EditorCommand::WriteAssetMeta { .. }
            | EditorCommand::SetAssetSettings { .. }
            | EditorCommand::SetResourceField { .. }
            | EditorCommand::SelectResource { .. }
            | EditorCommand::SceneNew
            | EditorCommand::SceneOpen { .. }
            | EditorCommand::SceneSave
            | EditorCommand::SceneSaveAs { .. } => {}
        }
    }

    /// Apply a batch at the frame boundary.
    ///
    /// Returns one `(index, message)` per failed command so the Console panel
    /// can surface it. A failure never aborts the rest of the batch and never
    /// leaves partial state: every engine call this dispatches is
    /// all-or-nothing, and structural commands validate before they commit.
    pub fn apply(engine: &mut Engine, commands: &[EditorCommand]) -> Vec<(usize, String)> {
        Self::apply_recorded(engine, commands).failures
    }

    /// Apply a batch and compute the inverse of every edit it performed.
    ///
    /// This is the single application path: the plain [`Self::apply`] only
    /// discards the recording. A command is only recorded when it succeeded
    /// and produced an inverse, so an undo can never half-restore a batch;
    /// [`RecordedApply::fully_invertible`] reports whether that held.
    ///
    /// The batch runs against its own copy because a restore assigns a fresh
    /// handle mid-batch: every command behind it - and every inverse already
    /// collected - follows the entity to the new handle, exactly like the
    /// history entries do. Without that, a delete recorded as `destroy, clear
    /// the selection` would undo its clear before the restore exists and the
    /// selection would come back pointing at nothing.
    pub fn apply_recorded(engine: &mut Engine, commands: &[EditorCommand]) -> RecordedApply {
        let mut outcome = RecordedApply {
            fully_invertible: true,
            ..RecordedApply::default()
        };
        let mut working = commands.to_vec();
        for index in 0..working.len() {
            if !working[index].recordable() {
                outcome.fully_invertible = false;
            }
            match Self::apply_one(engine, &working[index]) {
                Ok(applied) => {
                    // An edit that succeeded is an unsaved change; the
                    // document commands clear or set the marker themselves.
                    if working[index].recordable() {
                        crate::scene_workflow::mark_dirty(engine.world_mut());
                    }
                    if let Some(inverse) = applied.inverse {
                        outcome.inverses.push(inverse);
                    } else {
                        outcome.fully_invertible = false;
                    }
                    if let Some((previous, assigned)) = applied.remap {
                        for later in &mut working[index + 1..] {
                            later.remap_entity(previous, assigned);
                        }
                        for inverse in &mut outcome.inverses {
                            inverse.remap_entity(previous, assigned);
                        }
                        outcome.remaps.push((previous, assigned));
                    }
                }
                Err(message) => {
                    outcome.failures.push((index, message));
                    outcome.fully_invertible = false;
                }
            }
        }
        // Undo walks the batch backwards, so hand the inverses back reversed.
        outcome.inverses.reverse();
        outcome
    }

    /// Capture a live entity's component images for a later restore.
    ///
    /// Only components with a field layout can be rebuilt; a layout-less
    /// component is skipped, so restoring an entity that carried one brings
    /// back everything the editor can read and describe.
    fn capture_seeds(engine: &Engine, entity: Entity) -> Result<Vec<ComponentSeed>, String> {
        let detail = EditorSnapshot::capture_detail(engine, entity)
            .ok_or_else(|| format!("entity {entity} is not alive"))?;
        let mut seeds = Vec::new();
        for view in detail.components {
            if !view.editable {
                continue;
            }
            let fields = view
                .fields
                .iter()
                .map(|descriptor| descriptor.name.to_string())
                .zip(view.values.iter().cloned())
                .collect();
            seeds.push(ComponentSeed {
                type_name: view.type_name,
                fields,
            });
        }
        Ok(seeds)
    }

    /// Apply one command; the message is empty on success.
    fn apply_one(engine: &mut Engine, command: &EditorCommand) -> Result<CommandOutcome, String> {
        match command {
            EditorCommand::SelectResource { resource } => {
                let world = engine.world_mut();
                if world.get_resource::<EditorSelection>().is_none() {
                    world.insert_resource(EditorSelection::default());
                }
                let Some(selection) = world.get_resource_mut::<EditorSelection>() else {
                    return Err(format!("{command:?}: the selection could not be claimed"));
                };
                let previous = selection.resource.clone();
                // One thing at a time in the Inspector: picking a resource
                // drops the entity and asset halves, and their undo restores
                // them through their own commands.
                let previous_entities = selection.entities().to_vec();
                let previous_asset = selection.asset.clone();
                selection.resource = resource.clone();
                if resource.is_some() {
                    selection.clear_entities();
                    selection.asset = None;
                }
                // NOTE: asserted on by the egui editor's suite: the selection a
                // resource row makes, and the one its undo restores.
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    resource = resource.as_deref().unwrap_or("none"),
                    "[editor] resource selected"
                );
                Ok(CommandOutcome::inverse(EditorCommand::RestoreSelection {
                    entities: previous_entities,
                    asset: previous_asset,
                    resource: previous,
                }))
            }

            EditorCommand::RestoreSelection {
                entities,
                asset,
                resource,
            } => {
                let world = engine.world_mut();
                if world.get_resource::<EditorSelection>().is_none() {
                    world.insert_resource(EditorSelection::default());
                }
                let Some(selection) = world.get_resource_mut::<EditorSelection>() else {
                    return Err(format!("{command:?}: the selection could not be claimed"));
                };
                let previous = EditorCommand::RestoreSelection {
                    entities: selection.entities().to_vec(),
                    asset: selection.asset.clone(),
                    resource: selection.resource.clone(),
                };
                selection.set_entities(entities.clone());
                selection.asset = asset.clone();
                selection.resource = resource.clone();
                // NOTE: asserted on by the egui editor's suite: an undone
                // resource selection brings the entity slot back, not just an
                // empty resource slot.
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    entity = selection
                        .primary()
                        .map(|entity| entity.to_string())
                        .unwrap_or_else(|| "none".to_string())
                        .as_str(),
                    resource = resource.as_deref().unwrap_or("none"),
                    entities = selection.entities().len(),
                    "[editor] selection restored"
                );
                Ok(CommandOutcome::inverse(previous))
            }

            EditorCommand::SetField {
                entity,
                component,
                field,
                value,
            } => {
                // Read the old value first: it is what the inverse restores,
                // and a field that cannot be read cannot be written either.
                let previous = engine
                    .world()
                    .read_component_field(*entity, component, field)
                    .map_err(|error| format!("{command:?}: {error}"))?;
                engine
                    .world_mut()
                    .write_component_field(*entity, component, field, value.clone())
                    .map_err(|error| format!("{command:?}: {error}"))?;
                Ok(CommandOutcome::inverse(EditorCommand::SetField {
                    entity: *entity,
                    component: component.clone(),
                    field: field.clone(),
                    value: previous,
                }))
            }

            EditorCommand::SetFieldAsset {
                entity,
                component,
                field,
                reference,
            } => {
                let previous = engine
                    .world()
                    .read_component_field(*entity, component, field)
                    .map_err(|error| format!("{command:?}: {error}"))?;
                let FieldValue::Asset { asset_type, .. } = &previous else {
                    return Err(format!("{command:?}: `{field}` is not an asset reference"));
                };
                let value = match reference {
                    None => FieldValue::Asset {
                        asset_type: asset_type.clone(),
                        index: u32::MAX,
                        generation: u32::MAX,
                    },
                    Some(reference) => resolve_asset_reference(engine, reference)
                        .map_err(|error| format!("{command:?}: {error}"))?,
                };
                engine
                    .world_mut()
                    .write_component_field(*entity, component, field, value)
                    .map_err(|error| format!("{command:?}: {error}"))?;
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    component = component.as_str(),
                    field = field.as_str(),
                    asset = reference.as_deref().unwrap_or("none"),
                    "[editor] asset reference set"
                );
                Ok(CommandOutcome::inverse(EditorCommand::SetField {
                    entity: *entity,
                    component: component.clone(),
                    field: field.clone(),
                    value: previous,
                }))
            }

            EditorCommand::ArmEntityPick {
                entity,
                component,
                field,
            } => {
                let world = engine.world_mut();
                let request = world
                    .get_resource_mut::<crate::editor_state::EntityPickRequest>()
                    .ok_or("the world has no entity pick request slot")?;
                request.target = Some(crate::editor_state::EntityPickTarget {
                    entity: *entity,
                    component: component.clone(),
                    field: field.clone(),
                });
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    entity = %entity,
                    component = component.as_str(),
                    field = field.as_str(),
                    "[editor] inspector entity pick armed"
                );
                Ok(CommandOutcome::plain())
            }

            EditorCommand::SetFieldEntity {
                entity,
                component,
                field,
                reference,
            } => {
                let previous = engine
                    .world()
                    .read_component_field(*entity, component, field)
                    .map_err(|error| format!("{command:?}: {error}"))?;
                if !matches!(previous, FieldValue::Entity { .. }) {
                    return Err(format!("{command:?}: `{field}` is not an entity reference"));
                }
                let value = match reference {
                    None => FieldValue::Entity {
                        id: Entity::NULL.id(),
                        generation: Entity::NULL.generation(),
                    },
                    Some(target) => {
                        // A reference to an entity that has gone since the row
                        // was drawn is refused here rather than written as a
                        // dangling handle: the row would show it missing, and
                        // the undo step would restore a reference to nothing.
                        if !engine.world().is_entity_valid(*target) {
                            return Err(format!(
                                "{command:?}: no live entity carries the reference {}",
                                EntityListEntry::display_of(*target)
                            ));
                        }
                        FieldValue::Entity {
                            id: target.id(),
                            generation: target.generation(),
                        }
                    }
                };
                engine
                    .world_mut()
                    .write_component_field(*entity, component, field, value)
                    .map_err(|error| format!("{command:?}: {error}"))?;
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    component = component.as_str(),
                    field = field.as_str(),
                    reference = reference
                        .map(EntityListEntry::display_of)
                        .unwrap_or_else(|| "none".to_owned()),
                    "[editor] entity reference set"
                );
                Ok(CommandOutcome::inverse(EditorCommand::SetField {
                    entity: *entity,
                    component: component.clone(),
                    field: field.clone(),
                    value: previous,
                }))
            }

            EditorCommand::SetArrayElement {
                entity,
                component,
                field,
                index,
                value,
            } => {
                // The whole array reads back as one value; the old element is
                // the inverse's new value.
                let current = engine
                    .world()
                    .read_component_field(*entity, component, field)
                    .map_err(|error| format!("{command:?}: {error}"))?;
                let FieldValue::Array(elements) = current else {
                    return Err(format!("{command:?}: `{field}` is not an array field"));
                };
                let previous = elements
                    .get(*index)
                    .cloned()
                    .ok_or_else(|| format!("{command:?}: element {index} is outside the array"))?;
                engine
                    .world_mut()
                    .write_component_array_element(*entity, component, field, *index, value.clone())
                    .map_err(|error| format!("{command:?}: {error}"))?;
                Ok(CommandOutcome::inverse(EditorCommand::SetArrayElement {
                    entity: *entity,
                    component: component.clone(),
                    field: field.clone(),
                    index: *index,
                    value: previous,
                }))
            }

            EditorCommand::SetSystemEnabled { index, enabled } => {
                let previous = engine
                    .system_snapshots()
                    .get(*index)
                    .map(|system| system.enabled)
                    .ok_or_else(|| {
                        format!("{command:?}: no system at registration index {index}")
                    })?;
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

            EditorCommand::ImportFiles {
                sources,
                destination,
            } => {
                // Validate every source before copying anything, so one bad
                // path in a multi-file drop changes nothing.
                for source in sources {
                    if !std::path::Path::new(source).is_file() {
                        return Err(format!("{command:?}: `{source}` is not a file"));
                    }
                }
                std::fs::create_dir_all(destination)
                    .map_err(|error| format!("{command:?}: {error}"))?;
                let mut created: Vec<String> = Vec::new();
                for source in sources {
                    let file_name = std::path::Path::new(source)
                        .file_name()
                        .ok_or_else(|| format!("{command:?}: `{source}` has no file name"))?;
                    let target = std::path::Path::new(destination).join(file_name);
                    // A source that already is the destination file - a drop
                    // of something the project already has - is nothing to
                    // copy; copying a file onto itself would at best fail and
                    // at worst truncate it.
                    let source_path = std::path::Path::new(source);
                    let same = match (source_path.canonicalize(), target.canonicalize()) {
                        (Ok(source), Ok(target)) => source == target,
                        _ => source_path == target,
                    };
                    if same {
                        continue;
                    }
                    if let Err(error) = std::fs::copy(source, &target) {
                        // Best-effort rollback of this command's own copies:
                        // a partial import must not stay on disk unrecorded.
                        for path in &created {
                            let _ = std::fs::remove_file(path);
                        }
                        return Err(format!("{command:?}: {error}"));
                    }
                    created.push(target.to_string_lossy().into_owned());
                }
                if created.is_empty() {
                    // Every source was already in place: nothing changed, and
                    // nothing is recorded.
                    return Ok(CommandOutcome::plain());
                }
                Ok(CommandOutcome::inverse(EditorCommand::RemoveFiles {
                    paths: created,
                    sources: sources.clone(),
                    destination: destination.clone(),
                }))
            }

            EditorCommand::RemoveFiles {
                paths,
                sources,
                destination,
            } => {
                for path in paths {
                    match std::fs::remove_file(path) {
                        Ok(()) => {}
                        // Already gone is the state undo wanted anyway.
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => return Err(format!("{command:?}: {error}")),
                    }
                }
                Ok(CommandOutcome::inverse(EditorCommand::ImportFiles {
                    sources: sources.clone(),
                    destination: destination.clone(),
                }))
            }

            EditorCommand::CreateStandaloneAsset {
                type_name,
                folder,
                name,
            } => {
                let root = asset_authoring_root()?;
                let registry = engine
                    .world()
                    .get_resource::<pill_engine::ImportRegistry>()
                    .cloned()
                    .ok_or_else(|| format!("{command:?}: no asset types are registered"))?;
                // The store is taken before the file is written: an engine
                // without one must refuse the create, not leave a file no
                // asset manager will ever hold.
                let assets = engine
                    .world_mut()
                    .get_resource_mut::<pill_engine::AssetManager>()
                    .ok_or_else(|| format!("{command:?}: the engine has no AssetManager"))?;
                let extension = registry
                    .standalone_types()
                    .into_iter()
                    .find(|(registered, _)| registered == type_name)
                    .map(|(_, extension)| extension)
                    .ok_or_else(|| {
                        format!("{command:?}: `{type_name}` is not a standalone asset type")
                    })?;
                let folder = folder.trim().trim_matches('/');
                let relative_folder = if folder.is_empty() {
                    PathBuf::new()
                } else {
                    pill_engine::asset_metadata::checked_relative(folder)
                        .map_err(|message| format!("{command:?}: {message}"))?
                };
                let directory = root.join(&relative_folder);
                if !directory.is_dir() {
                    // A project whose asset directory does not exist yet gets
                    // its root made on demand; a named folder must exist, the
                    // same refusal the host's browser states.
                    if relative_folder.as_os_str().is_empty() {
                        std::fs::create_dir_all(&directory)
                            .map_err(|error| format!("{command:?}: {error}"))?;
                    } else {
                        return Err(format!(
                            "{command:?}: the folder `{}` does not exist",
                            asset_slash(&relative_folder)
                        ));
                    }
                }
                let file_name = pill_engine::asset_metadata::standalone_file_name(name, &extension)
                    .map_err(|message| format!("{command:?}: {message}"))?;
                let relative = relative_folder.join(&file_name);
                let destination = root.join(&relative);
                if destination.exists() {
                    return Err(format!(
                        "{command:?}: `{}` already exists",
                        asset_slash(&relative)
                    ));
                }
                let document = registry
                    .default_document(type_name)
                    .map_err(|error| format!("{command:?}: {error}"))?;
                let guid = pill_engine::AssetGuid::random().map_err(|error| {
                    format!("{command:?}: no random source for a guid: {error}")
                })?;
                let file = serde_json::json!({
                    "format_version": pill_engine::asset_metadata::METADATA_FORMAT_VERSION,
                    "asset_type": type_name,
                    "guid": guid,
                    "asset": document,
                });
                let mut bytes = serde_json::to_vec_pretty(&file)
                    .map_err(|error| format!("{command:?}: {error}"))?;
                bytes.push(b'\n');
                let sidecar = pill_engine::asset_metadata::metadata_path_for(&relative);
                let sidecar_existed = sidecar
                    .as_ref()
                    .is_some_and(|meta| root.join(meta).exists());
                pill_engine::asset_store::write_new(&destination, &bytes)
                    .map_err(|error| format!("{command:?}: {error}"))?;
                if let Err(error) = registry.import(
                    assets,
                    &relative,
                    pill_engine::MetadataPolicy::ReadIfPresent,
                ) {
                    // The file is only an asset once it imports, so a refusal
                    // takes the write back rather than leaving a file the
                    // project cannot load and no inverse covers.
                    let _ = std::fs::remove_file(&destination);
                    if let Some(meta) = sidecar.filter(|_| !sidecar_existed) {
                        let _ = std::fs::remove_file(root.join(meta));
                    }
                    return Err(format!(
                        "{command:?}: `{}` did not load: {error}",
                        asset_slash(&relative)
                    ));
                }
                Ok(CommandOutcome::inverse(EditorCommand::DeleteAssetFile {
                    path: asset_slash(&relative),
                }))
            }

            EditorCommand::DeleteAssetFile { path } => {
                let root = asset_authoring_root()?;
                let relative = pill_engine::asset_metadata::checked_relative(path)
                    .map_err(|message| format!("{command:?}: {message}"))?;
                let file = root.join(&relative);
                let bytes = std::fs::read(&file)
                    .map_err(|error| format!("{command:?}: cannot read `{path}`: {error}"))?;
                let meta_relative = pill_engine::asset_metadata::metadata_path_for(&relative);
                let meta = match &meta_relative {
                    Some(meta) => match std::fs::read(root.join(meta)) {
                        Ok(bytes) => Some(bytes),
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                        Err(error) => return Err(format!("{command:?}: {error}")),
                    },
                    None => None,
                };
                std::fs::remove_file(&file)
                    .map_err(|error| format!("{command:?}: cannot delete `{path}`: {error}"))?;
                if let Some(meta) = meta_relative {
                    match std::fs::remove_file(root.join(&meta)) {
                        Ok(()) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => {
                            // The asset file is already gone; put it back, so a
                            // delete that cannot finish leaves nothing behind.
                            return Err(match write_asset_bytes(&file, &bytes) {
                                Ok(()) => format!(
                                    "{command:?}: cannot delete `{path}`'s sidecar, so nothing was deleted: {error}"
                                ),
                                Err(restore) => format!(
                                    "{command:?}: cannot delete `{path}`'s sidecar ({error}) and cannot restore the asset: {restore}"
                                ),
                            });
                        }
                    }
                }
                // The file is gone, so the asset it held must go with it: a
                // re-created path would otherwise collide with the loaded
                // value on the guid the create writes (W-0122).
                if let Some(registry) = engine
                    .world()
                    .get_resource::<pill_engine::ImportRegistry>()
                    .cloned()
                {
                    if let Some(assets) = engine
                        .world_mut()
                        .get_resource_mut::<pill_engine::AssetManager>()
                    {
                        registry.remove_loaded(&mut *assets, &relative);
                    }
                }
                Ok(CommandOutcome::inverse(EditorCommand::RestoreAssetFile {
                    path: path.clone(),
                    bytes,
                    meta,
                }))
            }

            EditorCommand::RestoreAssetFile { path, bytes, meta } => {
                let root = asset_authoring_root()?;
                let relative = pill_engine::asset_metadata::checked_relative(path)
                    .map_err(|message| format!("{command:?}: {message}"))?;
                write_asset_bytes(&root.join(&relative), bytes)
                    .map_err(|message| format!("{command:?}: {message}"))?;
                if let Some(meta_relative) =
                    pill_engine::asset_metadata::metadata_path_for(&relative)
                {
                    let meta_path = root.join(&meta_relative);
                    match meta {
                        Some(meta) => write_asset_bytes(&meta_path, meta)
                            .map_err(|message| format!("{command:?}: {message}"))?,
                        // A file that had no sidecar restores without one;
                        // anything sitting there now is an orphan.
                        None => {
                            let _ = std::fs::remove_file(&meta_path);
                        }
                    }
                }
                Ok(CommandOutcome::inverse(EditorCommand::DeleteAssetFile {
                    path: path.clone(),
                }))
            }

            EditorCommand::MoveAssetFile { from, to } => {
                let root = asset_authoring_root()?;
                let from_relative = pill_engine::asset_metadata::checked_relative(from)
                    .map_err(|message| format!("{command:?}: {message}"))?;
                let to_relative = pill_engine::asset_metadata::checked_relative(to)
                    .map_err(|message| format!("{command:?}: {message}"))?;
                let destination = root.join(&to_relative);
                if let Some(parent) = destination.parent() {
                    std::fs::create_dir_all(parent)
                        .map_err(|error| format!("{command:?}: {error}"))?;
                }
                if destination.exists() {
                    return Err(format!(
                        "{command:?}: `{}` already exists",
                        asset_slash(&to_relative)
                    ));
                }
                pill_assets::move_asset(&root.join(&from_relative), &destination)
                    .map_err(|error| format!("{command:?}: {error}"))?;
                Ok(CommandOutcome::inverse(EditorCommand::MoveAssetFile {
                    from: to.clone(),
                    to: from.clone(),
                }))
            }

            EditorCommand::WriteAssetMeta { path } => {
                let root = asset_authoring_root()?;
                let relative = pill_engine::asset_metadata::checked_relative(path)
                    .map_err(|message| format!("{command:?}: {message}"))?;
                let Some(meta_relative) = pill_engine::asset_metadata::metadata_path_for(&relative)
                else {
                    return Err(format!("{command:?}: `{path}` cannot have a sidecar"));
                };
                let meta_path = root.join(&meta_relative);
                if meta_path.is_file() {
                    // A sidecar that carries a guid is not missing: nothing
                    // changes, nothing is recorded. One left empty or
                    // unreadable by an interrupted write counts as missing
                    // and is replaced; the undo removes the replacement.
                    let usable = std::fs::read_to_string(&meta_path)
                        .ok()
                        .and_then(|text| serde_json::from_str::<serde_json::Value>(&text).ok())
                        .and_then(|document| {
                            document
                                .get("guid")
                                .and_then(|guid| guid.as_str())
                                .map(str::to_owned)
                        })
                        .is_some();
                    if usable {
                        return Ok(CommandOutcome::plain());
                    }
                    std::fs::remove_file(&meta_path)
                        .map_err(|error| format!("{command:?}: {error}"))?;
                }
                let registry = engine
                    .world()
                    .get_resource::<pill_engine::ImportRegistry>()
                    .cloned()
                    .ok_or_else(|| format!("{command:?}: no asset types are registered"))?;
                let assets = engine
                    .world_mut()
                    .get_resource_mut::<pill_engine::AssetManager>()
                    .ok_or_else(|| format!("{command:?}: the engine has no AssetManager"))?;
                registry
                    .ensure_metadata(assets, &relative)
                    .map_err(|error| format!("{command:?}: {error}"))?;
                Ok(CommandOutcome::inverse(EditorCommand::DeleteAssetFile {
                    path: asset_slash(&meta_relative),
                }))
            }

            EditorCommand::SetAssetSettings { path, settings } => {
                let root = asset_authoring_root()?;
                let relative = pill_engine::asset_metadata::checked_relative(path)
                    .map_err(|message| format!("{command:?}: {message}"))?;
                let registry = engine
                    .world()
                    .get_resource::<pill_engine::ImportRegistry>()
                    .cloned()
                    .ok_or_else(|| format!("{command:?}: no asset types are registered"))?;
                let extension = relative
                    .extension()
                    .and_then(|extension| extension.to_str())
                    .unwrap_or_default()
                    .to_owned();
                let type_name = registry
                    .type_for_extension(&extension)
                    .ok_or_else(|| {
                        format!("{command:?}: no asset type imports `.{extension}` files")
                    })?
                    .to_owned();
                let normalized = registry
                    .normalize_settings_json(&type_name, settings.clone())
                    .map_err(|error| format!("{command:?}: {error}"))?;
                let previous = registry
                    .settings_json(&relative)
                    .map_err(|error| format!("{command:?}: {error}"))?;
                let standalone = registry.is_standalone_extension(&extension);
                let body_field = if standalone { "asset" } else { "settings" };
                let file = if standalone {
                    root.join(&relative)
                } else {
                    let meta_relative = pill_engine::asset_metadata::metadata_path_for(&relative)
                        .ok_or_else(|| {
                        format!("{command:?}: `{path}` cannot have a sidecar")
                    })?;
                    let file = root.join(&meta_relative);
                    if !file.is_file() {
                        // The import writes the sidecar with the type's
                        // defaults and a guid, which the write below then
                        // replaces.
                        let assets = engine
                            .world_mut()
                            .get_resource_mut::<pill_engine::AssetManager>()
                            .ok_or_else(|| {
                                format!("{command:?}: the engine has no AssetManager")
                            })?;
                        registry
                            .import(
                                assets,
                                &relative,
                                pill_engine::MetadataPolicy::CreateIfMissing,
                            )
                            .map_err(|error| format!("{command:?}: {error}"))?;
                    }
                    file
                };
                let text = std::fs::read_to_string(&file).map_err(|error| {
                    format!("{command:?}: cannot read `{}`: {error}", file.display())
                })?;
                let mut document: serde_json::Value =
                    serde_json::from_str(&text).map_err(|error| {
                        format!(
                            "{command:?}: `{}` is not valid JSON: {error}",
                            file.display()
                        )
                    })?;
                let object = document.as_object_mut().ok_or_else(|| {
                    format!("{command:?}: `{}` is not a JSON object", file.display())
                })?;
                object.insert(body_field.to_owned(), normalized);
                let mut bytes = serde_json::to_vec_pretty(&document)
                    .map_err(|error| format!("{command:?}: {error}"))?;
                bytes.push(b'\n');
                write_asset_bytes(&file, &bytes)
                    .map_err(|message| format!("{command:?}: {message}"))?;
                Ok(CommandOutcome::inverse(EditorCommand::SetAssetSettings {
                    path: path.clone(),
                    settings: previous,
                }))
            }

            EditorCommand::SetResourceField {
                resource,
                field,
                value,
            } => {
                let world = engine.world_mut();
                if world.get_resource::<crate::gizmo::GizmoState>().is_none() {
                    world.insert_resource(crate::gizmo::GizmoState::default());
                }
                let Some(state) = world.get_resource_mut::<crate::gizmo::GizmoState>() else {
                    return Err(format!("{command:?}: the gizmo state could not be claimed"));
                };
                let previous =
                    crate::gizmo::set_resource_field(&mut state.settings, resource, field, value)
                        .map_err(|message| format!("{command:?}: {message}"))?;
                Ok(CommandOutcome::inverse(EditorCommand::SetResourceField {
                    resource: resource.clone(),
                    field: field.clone(),
                    value: previous,
                }))
            }

            EditorCommand::SetSelection { entities, asset } => {
                let world = engine.world_mut();
                if world.get_resource::<EditorSelection>().is_none() {
                    world.insert_resource(EditorSelection::default());
                }
                // A selection that names an entity the world no longer
                // carries drops that entity (Stage 28: a recovered selection
                // from before a crash is checked here exactly like one the
                // history trapped across a destroy); the asset half still
                // applies.
                let mut taken = Vec::with_capacity(entities.len());
                for entity in entities {
                    if world.is_entity_valid(*entity) {
                        taken.push(*entity);
                    } else {
                        // NOTE: asserted on by the editor's recovery suite.
                        pill_core::info!(
                            target: pill_core::telemetry::telemetry_target::ENGINE,
                            entity = %entity,
                            "[editor] selection command dropped; the entity no longer exists"
                        );
                    }
                }
                let Some(selection) = world.get_resource_mut::<EditorSelection>() else {
                    return Err(format!("{command:?}: the selection could not be claimed"));
                };
                let previous = EditorCommand::SetSelection {
                    entities: selection.entities().to_vec(),
                    asset: selection.asset.clone(),
                };
                selection.set_entities(taken);
                selection.asset = asset.clone();
                Ok(CommandOutcome::inverse(previous))
            }

            EditorCommand::SceneNew => {
                let removed = crate::scene_workflow::new_scene(engine);
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    entities = removed,
                    "[editor] scene new"
                );
                Ok(CommandOutcome::plain())
            }

            EditorCommand::SceneOpen { path } => {
                crate::scene_workflow::open_scene(engine, path)
                    .map_err(|error| format!("{command:?}: {error}"))?;
                Ok(CommandOutcome::plain())
            }

            EditorCommand::SceneSave => {
                crate::scene_workflow::write_scene(engine, None)
                    .map_err(|error| format!("{command:?}: {error}"))?;
                Ok(CommandOutcome::plain())
            }

            EditorCommand::SceneSaveAs { path } => {
                let path = crate::scene_workflow::scene_file_name(path);
                crate::scene_workflow::write_scene(engine, Some(&path))
                    .map_err(|error| format!("{command:?}: {error}"))?;
                Ok(CommandOutcome::plain())
            }

            EditorCommand::SetPreferences { values } => {
                // Read-modify-write, so the per-project gizmo entry and any
                // key another writer owns survive; blanks write nothing, so
                // an untouched row keeps the consumer's own default.
                let directory = crate::layout::layout_directory();
                let mut document = crate::preferences::read_document(&directory)
                    .map_err(|message| format!("{command:?}: {message}"))?;
                let mut written = 0usize;
                for (key, text) in values {
                    if text.trim().is_empty() {
                        continue;
                    }
                    written += 1;
                    crate::preferences::set_key(
                        &mut document,
                        key,
                        crate::preferences::parse_value(text),
                    )
                    .map_err(|message| format!("{command:?}: {message}"))?;
                }
                crate::preferences::write_document(&directory, &document)
                    .map_err(|message| format!("{command:?}: {message}"))?;
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ENGINE,
                    keys = written,
                    "[editor] preferences saved"
                );
                Ok(CommandOutcome::plain())
            }

            EditorCommand::DestroyEntity { entity } => {
                // Capture the images before the destroy: they are the inverse.
                let seeds = Self::capture_seeds(engine, *entity)?;
                if engine.world_mut().destroy_entity(*entity) {
                    Ok(CommandOutcome::inverse(EditorCommand::RestoreEntity {
                        previous: *entity,
                        components: seeds,
                    }))
                } else {
                    Err(format!("{command:?}: entity was already gone"))
                }
            }

            EditorCommand::CreateEntity { components } => {
                // Step 1: Validate and compose every component image before
                // touching the world, so a bad seed changes nothing.
                let mut native: Vec<Box<dyn pill_engine::commands::ComponentAdder>> = Vec::new();
                let mut descriptor_components = Vec::new();
                for seed in components {
                    // Build the image first; this validates the layout rule.
                    let bytes = engine
                        .world()
                        .build_component_image(&seed.type_name, &seed.fields)
                        .map_err(|error| format!("{command:?}: {error}"))?;
                    // An ambiguous name reports itself rather than resolving
                    // to a guessed column, so the editor never writes bytes
                    // into the wrong component's storage.
                    let component_id = engine
                        .world()
                        .resolve_component_id_by_name_any(&seed.type_name)
                        .map_err(|error| format!("{command:?}: {error}"))?
                        .ok_or_else(|| {
                            format!("{command:?}: `{}` is not registered", seed.type_name)
                        })?;
                    if component_id.is_native_storage() {
                        native.push(Box::new(pill_engine::commands::ByteComponentAdder::new(
                            component_id,
                            bytes,
                        )));
                    } else {
                        descriptor_components.push((component_id, bytes));
                    }
                }

                // Step 2: Reserve the handle and queue the creation inside the
                // same closure that holds the world; the closure records the
                // assigned handle so the inverse can name it.
                let slot: std::sync::Arc<std::sync::Mutex<Option<Entity>>> =
                    std::sync::Arc::new(std::sync::Mutex::new(None));
                let writer = std::sync::Arc::clone(&slot);
                engine.queue_deferred_commands(move |world, queue| {
                    let entity = world.reserve_entity();
                    if let Ok(mut cell) = writer.lock() {
                        *cell = Some(entity);
                    }
                    queue.create_mixed_entity(entity, native, descriptor_components);
                });
                engine
                    .flush_deferred_commands()
                    .map_err(|errors| format!("{command:?}: {errors:?}"))?;
                let assigned = slot
                    .lock()
                    .ok()
                    .and_then(|cell| *cell)
                    .ok_or_else(|| format!("{command:?}: the creation reserved no handle"))?;
                Ok(CommandOutcome::inverse(EditorCommand::DestroyEntity {
                    entity: assigned,
                }))
            }

            EditorCommand::AddComponent { entity, component } => {
                // Adding is only offered for components with a registered
                // field layout: the zero-initialisation rule says an image
                // without one cannot be safely zero-filled. Adding targets an
                // entity that does not carry the component yet; attaching it
                // twice is an error, not a no-op.
                if engine
                    .world()
                    .resolve_entity_component_id(*entity, component)
                    .is_some()
                {
                    return Err(format!(
                        "{command:?}: `{component}` is already on the entity"
                    ));
                }
                let bytes = engine
                    .world()
                    .build_component_image(component, &[])
                    .map_err(|error| format!("{command:?}: {error}"))?;
                let component_id = engine
                    .world()
                    .resolve_component_id_by_name_any(component)
                    .map_err(|error| format!("{command:?}: {error}"))?
                    .ok_or_else(|| format!("{command:?}: `{component}` is not registered"))?;
                engine.queue_deferred_commands(move |_world, queue| {
                    if component_id.is_native_storage() {
                        queue.add_component_adder_to_entity(
                            *entity,
                            Box::new(pill_engine::commands::ByteComponentAdder::new(
                                component_id,
                                bytes,
                            )),
                        );
                    } else {
                        queue.add_descriptor_component_to_entity(*entity, component_id, bytes);
                    }
                });
                engine
                    .flush_deferred_commands()
                    .map_err(|errors| format!("{command:?}: {errors:?}"))?;
                Ok(CommandOutcome::inverse(EditorCommand::RemoveComponent {
                    entity: *entity,
                    component: component.clone(),
                }))
            }

            EditorCommand::RemoveComponent { entity, component } => {
                let component_id = engine
                    .world()
                    .resolve_entity_component_id(*entity, component)
                    .ok_or_else(|| format!("{command:?}: `{component}` is not on the entity"))?;
                // Capture the values first: the inverse re-attaches the
                // component with them. A layout-less component cannot be
                // rebuilt from images, so its removal is not invertible.
                let names: Vec<String> = engine
                    .world()
                    .component_field_layout(component_id)
                    .map(|layout| layout.iter().map(|d| d.name.to_string()).collect())
                    .unwrap_or_default();
                let removed = if names.is_empty() {
                    None
                } else {
                    let values = engine
                        .world()
                        .read_component_fields(*entity, component)
                        .map_err(|error| format!("{command:?}: {error}"))?;
                    Some(
                        names
                            .into_iter()
                            .zip(values)
                            .collect::<Vec<(String, FieldValue)>>(),
                    )
                };
                engine.queue_deferred_commands(move |_world, queue| {
                    queue.remove_component_by_id(*entity, component_id);
                });
                engine
                    .flush_deferred_commands()
                    .map_err(|errors| format!("{command:?}: {errors:?}"))?;
                Ok(match removed {
                    Some(fields) => CommandOutcome::inverse(EditorCommand::RestoreComponent {
                        entity: *entity,
                        component: component.clone(),
                        fields,
                    }),
                    None => CommandOutcome::plain(),
                })
            }

            EditorCommand::RestoreEntity {
                previous,
                components,
            } => {
                // Restores go through the creation path, which is what makes
                // the assigned handle observable; the inverse this produced
                // (a destroy of the fresh handle) is deliberately dropped: a
                // restore is already part of the history, never an entry.
                let create = EditorCommand::CreateEntity {
                    components: components.clone(),
                };
                let created = Self::apply_one(engine, &create)?;
                let Some(EditorCommand::DestroyEntity { entity: assigned }) = created.inverse
                else {
                    return Err(format!("{command:?}: the restore reserved no handle"));
                };
                Ok(CommandOutcome {
                    inverse: None,
                    remap: Some((*previous, assigned)),
                })
            }

            EditorCommand::RestoreComponent {
                entity,
                component,
                fields,
            } => {
                // Zero-initialised attach first, then the captured values, so
                // the restore reuses exactly the validation of the commands
                // the UI issues.
                let add = EditorCommand::AddComponent {
                    entity: *entity,
                    component: component.clone(),
                };
                Self::apply_one(engine, &add)?;
                for (field, value) in fields {
                    let set = EditorCommand::SetField {
                        entity: *entity,
                        component: component.clone(),
                        field: field.clone(),
                        value: value.clone(),
                    };
                    Self::apply_one(engine, &set)?;
                }
                Ok(CommandOutcome::plain())
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A click replaces the selection, Ctrl adds to it, and Shift covers the
    /// rows between the anchor and the clicked row (W-0038 Stage 16).
    #[test]
    fn selection_clicks_reset_add_and_range() {
        let rows: Vec<Entity> = (1..=4).map(|id| Entity::from_raw(id, 0)).collect();
        let (a, b, c, d) = (rows[0], rows[1], rows[2], rows[3]);

        let selection = EditorSelection::default();
        assert_eq!(selection.clicked(SelectionClick::Reset, b, &rows), vec![b]);

        // Ctrl adds, and re-adding an entity already selected only makes it
        // primary rather than duplicating it.
        let mut selection = EditorSelection::default();
        selection.set_entities(vec![a]);
        let added = selection.clicked(SelectionClick::Add, c, &rows);
        assert_eq!(added, vec![a, c]);
        selection.set_entities(added);
        assert_eq!(selection.clicked(SelectionClick::Add, a, &rows), vec![c, a]);

        // Shift ranges over the list's own order, in either direction.
        let mut selection = EditorSelection::default();
        selection.set_entities(vec![a, b]);
        assert_eq!(
            selection.clicked(SelectionClick::Range, d, &rows),
            vec![b, c, d]
        );
        assert_eq!(selection.clicked(SelectionClick::Range, b, &rows), vec![b]);

        // A range with no anchor the list shows is a plain click.
        let empty = EditorSelection::default();
        assert_eq!(empty.clicked(SelectionClick::Range, c, &rows), vec![c]);
    }

    /// A plain component with a field layout, so entity create and field
    /// writes have something to target.
    #[repr(C)]
    #[derive(Debug, Clone, Copy, PartialEq)]
    struct DemoComponent {
        strength: f32,
        enabled: bool,
    }
    impl pill_engine::Component for DemoComponent {}

    /// Hand-written layout mirroring what `#[derive(PillComponent)]` emits.
    static DEMO_FIELDS: &[ComponentFieldDescriptor] = &[
        ComponentFieldDescriptor {
            name: "strength",
            type_tag: "f32",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        },
        ComponentFieldDescriptor {
            name: "enabled",
            type_tag: "bool",
            offset: 4,
            size: 1,
            align: 1,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        },
    ];

    fn demo_component_name() -> String {
        std::any::type_name::<DemoComponent>().to_string()
    }

    fn demo_engine() -> Engine {
        let mut engine = Engine::new();
        engine
            .world_mut()
            .register_component_with_layout::<DemoComponent>(DEMO_FIELDS);
        engine
    }

    #[test]
    fn snapshot_list_reflects_live_entities() {
        let mut engine = demo_engine();
        let entity = engine
            .world_mut()
            .create_entity()
            .with(DemoComponent {
                strength: 2.0,
                enabled: true,
            })
            .build()
            .expect("entity builds");

        let snapshot = EditorSnapshot::capture_list(&engine, 0, &[], Vec::new());
        assert_eq!(snapshot.entities.len(), 1);
        assert_eq!(snapshot.entities[0].entity, entity);
        assert_eq!(snapshot.entities[0].component_count, 1);

        // Detail capture reads the component values.
        let detail = EditorSnapshot::capture_detail(&engine, entity).expect("detail");
        assert_eq!(detail.components.len(), 1);
        assert!(detail.components[0].type_name.contains("DemoComponent"));
        assert_eq!(
            detail.components[0].values,
            vec![FieldValue::F32(2.0), FieldValue::Bool(true)]
        );
    }

    #[test]
    fn apply_changes_real_state_and_reports_failures() {
        let mut engine = demo_engine();
        let entity = engine
            .world_mut()
            .create_entity()
            .with(DemoComponent {
                strength: 1.0,
                enabled: false,
            })
            .build()
            .expect("entity builds");
        let component = demo_component_name();

        let failures = EditorCommand::apply(
            &mut engine,
            &[
                EditorCommand::SetField {
                    entity,
                    component: component.clone(),
                    field: "strength".to_string(),
                    value: FieldValue::F32(5.0),
                },
                EditorCommand::SetField {
                    entity,
                    component: component.clone(),
                    field: "strength".to_string(),
                    value: FieldValue::U32(3),
                },
            ],
        );
        // One bad value type; the good write still applied.
        assert_eq!(failures.len(), 1);
        let value = engine
            .world()
            .read_component_field(entity, &component, "strength")
            .expect("read");
        assert_eq!(value, FieldValue::F32(5.0));
    }

    /// A component with a fixed-size scalar array, for element writes.
    #[repr(C)]
    #[derive(Debug, Clone, Copy, PartialEq)]
    struct ArrayComponent {
        values: [i16; 3],
    }
    impl pill_engine::Component for ArrayComponent {}

    static ARRAY_FIELDS: &[ComponentFieldDescriptor] = &[ComponentFieldDescriptor {
        name: "values",
        type_tag: "array:i16",
        offset: 0,
        size: 6,
        align: 2,
        element_count: 3,
        decorators: std::borrow::Cow::Borrowed(&[]),
    }];

    fn array_component_name() -> String {
        std::any::type_name::<ArrayComponent>().to_string()
    }

    #[test]
    fn create_entity_command_builds_seeded_components() {
        let mut engine = demo_engine();
        let component = demo_component_name();
        let failures = EditorCommand::apply(
            &mut engine,
            &[EditorCommand::CreateEntity {
                components: vec![ComponentSeed {
                    type_name: component.clone(),
                    fields: vec![
                        ("strength".to_string(), FieldValue::F32(9.0)),
                        ("enabled".to_string(), FieldValue::Bool(true)),
                    ],
                }],
            }],
        );
        assert!(failures.is_empty(), "create failed: {failures:?}");

        let snapshot = EditorSnapshot::capture_list(&engine, 0, &[], Vec::new());
        assert_eq!(snapshot.entities.len(), 1);
        let entity = snapshot.entities[0].entity;
        let detail = EditorSnapshot::capture_detail(&engine, entity).expect("detail");
        assert_eq!(
            detail.components[0].values,
            vec![FieldValue::F32(9.0), FieldValue::Bool(true)]
        );
    }

    #[test]
    fn destroy_entity_command_removes_row_and_queued_edits_error() {
        let mut engine = demo_engine();
        let component = demo_component_name();
        let entity = engine
            .world_mut()
            .create_entity()
            .with(DemoComponent {
                strength: 1.0,
                enabled: true,
            })
            .build()
            .expect("entity builds");

        let failures =
            EditorCommand::apply(&mut engine, &[EditorCommand::DestroyEntity { entity }]);
        assert!(failures.is_empty(), "destroy failed: {failures:?}");
        let snapshot = EditorSnapshot::capture_list(&engine, 0, &[], Vec::new());
        assert!(snapshot.entities.is_empty());

        // Editing a dead entity surfaces an error instead of panicking.
        let failures = EditorCommand::apply(
            &mut engine,
            &[EditorCommand::SetField {
                entity,
                component: component.clone(),
                field: "strength".to_string(),
                value: FieldValue::F32(2.0),
            }],
        );
        assert_eq!(failures.len(), 1);
        assert!(failures[0].1.contains("is not alive"));
    }

    /// The editor's generic field write reaches a component's registered
    /// field layout - the path the Inspector uses to edit values live.
    ///
    /// The copied version of this test wrote through the renderer's real
    /// `DirectionalLightComponent`; that pin needs the renderer data in this
    /// crate's test graph, which at the time dragged the `pill_ui` stack in
    /// beside the editor's egui library and rustc refused the mixed formats (W-0017
    /// Stage 5; `pill_ui` retired in W-0043). Follow-up: re-pin against the
    /// renderer's real component once its data can join this test graph.
    #[test]
    fn set_field_updates_a_registered_layout() {
        let mut engine = demo_engine();
        let entity = engine
            .world_mut()
            .create_entity()
            .with(DemoComponent {
                strength: 1.0,
                enabled: true,
            })
            .build()
            .unwrap();
        let name = demo_component_name();
        let failures = EditorCommand::apply(
            &mut engine,
            &[EditorCommand::SetField {
                entity,
                component: name.clone(),
                field: "strength".into(),
                value: FieldValue::F32(0.8),
            }],
        );
        assert!(failures.is_empty(), "{failures:?}");
        assert!(engine
            .world()
            .read_component_fields(entity, &name)
            .unwrap()
            .contains(&FieldValue::F32(0.8)));
    }

    #[test]
    fn add_and_remove_component_commands_round_trip() {
        let mut engine = demo_engine();
        let array_component = array_component_name();
        engine
            .world_mut()
            .register_component_with_layout::<ArrayComponent>(ARRAY_FIELDS);
        let entity = engine
            .world_mut()
            .create_entity()
            .with(DemoComponent {
                strength: 0.0,
                enabled: false,
            })
            .build()
            .expect("entity builds");

        // AddComponent zero-initialises a component with a field layout.
        let failures = EditorCommand::apply(
            &mut engine,
            &[EditorCommand::AddComponent {
                entity,
                component: array_component.clone(),
            }],
        );
        assert!(failures.is_empty(), "add failed: {failures:?}");
        let detail = EditorSnapshot::capture_detail(&engine, entity).expect("detail");
        assert_eq!(detail.components.len(), 2);
        let added = detail
            .components
            .iter()
            .find(|view| view.type_name == array_component)
            .expect("added component visible");
        assert_eq!(
            added.values,
            vec![FieldValue::Array(vec![
                FieldValue::I16(0),
                FieldValue::I16(0),
                FieldValue::I16(0),
            ])]
        );

        // Adding the same component twice is refused.
        let failures = EditorCommand::apply(
            &mut engine,
            &[EditorCommand::AddComponent {
                entity,
                component: array_component.clone(),
            }],
        );
        assert_eq!(failures.len(), 1);

        // RemoveComponent detaches it again, leaving the other component.
        let failures = EditorCommand::apply(
            &mut engine,
            &[EditorCommand::RemoveComponent {
                entity,
                component: array_component.clone(),
            }],
        );
        assert!(failures.is_empty(), "remove failed: {failures:?}");
        let detail = EditorSnapshot::capture_detail(&engine, entity).expect("detail");
        assert_eq!(detail.components.len(), 1);
        assert!(detail.components[0].type_name.contains("DemoComponent"));
    }

    #[test]
    fn set_array_element_command_writes_one_element() {
        let mut engine = demo_engine();
        let component = array_component_name();
        engine
            .world_mut()
            .register_component_with_layout::<ArrayComponent>(ARRAY_FIELDS);
        let entity = engine
            .world_mut()
            .create_entity()
            .with(ArrayComponent { values: [1, 2, 3] })
            .build()
            .expect("entity builds");

        let failures = EditorCommand::apply(
            &mut engine,
            &[EditorCommand::SetArrayElement {
                entity,
                component: component.clone(),
                field: "values".to_string(),
                index: 1,
                value: FieldValue::I16(9),
            }],
        );
        assert!(failures.is_empty(), "array write failed: {failures:?}");
        let value = engine
            .world()
            .read_component_field(entity, &component, "values")
            .expect("read");
        assert_eq!(
            value,
            FieldValue::Array(vec![
                FieldValue::I16(1),
                FieldValue::I16(9),
                FieldValue::I16(3)
            ])
        );
    }

    #[test]
    fn pause_step_and_system_toggle_commands_drive_the_engine() {
        let runs = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let counter = std::sync::Arc::clone(&runs);
        let mut engine = Engine::new();
        engine.register_system("duplicate", move || {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        });
        engine.begin_module_registration(SystemOwner::extension(0));
        engine.register_system("duplicate", || {});
        engine.end_module_registration();

        // Pause via command; frames stop running systems.
        let failures =
            EditorCommand::apply(&mut engine, &[EditorCommand::SetPaused { paused: true }]);
        assert!(failures.is_empty());
        engine.process_frame().unwrap();
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 0);

        // One step command runs exactly one phase.
        let failures = EditorCommand::apply(&mut engine, &[EditorCommand::StepOnce]);
        assert!(failures.is_empty());
        engine.process_frame().unwrap();
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 1);
        engine.process_frame().unwrap();
        assert_eq!(runs.load(std::sync::atomic::Ordering::SeqCst), 1);

        // Index-based toggle disables exactly one of the two same-named
        // systems.
        let failures = EditorCommand::apply(
            &mut engine,
            &[EditorCommand::SetSystemEnabled {
                index: 0,
                enabled: false,
            }],
        );
        assert!(failures.is_empty());
        let after = engine.system_snapshots();
        assert!(!after[0].enabled);
        assert!(after[1].enabled);
    }

    /// Apply a recorded batch, undo it, and check a field's value.
    fn round_trip_field(engine: &mut Engine, entity: Entity, component: &str, value: FieldValue) {
        let before = engine
            .world()
            .read_component_field(entity, component, "strength")
            .expect("read before");
        let command = EditorCommand::SetField {
            entity,
            component: component.to_string(),
            field: "strength".to_string(),
            value: value.clone(),
        };
        let recorded = EditorCommand::apply_recorded(engine, std::slice::from_ref(&command));
        assert!(recorded.failures.is_empty());
        assert!(recorded.fully_invertible);
        assert_eq!(recorded.inverses.len(), 1);

        // Undo restores the previous value; redo applies the edit again.
        let undo = EditorCommand::apply_recorded(engine, &recorded.inverses);
        assert!(undo.failures.is_empty());
        let restored = engine
            .world()
            .read_component_field(entity, component, "strength")
            .expect("read restored");
        assert_eq!(restored, before);
        let redo = EditorCommand::apply_recorded(engine, std::slice::from_ref(&command));
        assert!(redo.failures.is_empty());
        let reapplied = engine
            .world()
            .read_component_field(entity, component, "strength")
            .expect("read reapplied");
        assert_eq!(reapplied, value);
    }

    #[test]
    fn recorded_apply_inverts_field_edits() {
        let mut engine = demo_engine();
        let entity = engine
            .world_mut()
            .create_entity()
            .with(DemoComponent {
                strength: 2.0,
                enabled: true,
            })
            .build()
            .expect("entity builds");
        let component = demo_component_name();
        round_trip_field(&mut engine, entity, &component, FieldValue::F32(7.5));
    }

    #[test]
    fn recorded_apply_inverts_array_element_edits() {
        let mut engine = demo_engine();
        engine
            .world_mut()
            .register_component_with_layout::<ArrayComponent>(ARRAY_FIELDS);
        let entity = engine
            .world_mut()
            .create_entity()
            .with(ArrayComponent {
                values: [10, 20, 30],
            })
            .build()
            .expect("entity builds");
        let component = array_component_name();

        let edit = EditorCommand::SetArrayElement {
            entity,
            component: component.clone(),
            field: "values".to_string(),
            index: 1,
            value: FieldValue::I16(99),
        };
        let recorded = EditorCommand::apply_recorded(&mut engine, std::slice::from_ref(&edit));
        assert!(recorded.failures.is_empty(), "{:?}", recorded.failures);
        assert!(recorded.fully_invertible);

        let undo = EditorCommand::apply_recorded(&mut engine, &recorded.inverses);
        assert!(undo.failures.is_empty());
        let values = engine
            .world()
            .read_component_field(entity, &component, "values")
            .expect("read array");
        assert_eq!(
            values,
            FieldValue::Array(vec![
                FieldValue::I16(10),
                FieldValue::I16(20),
                FieldValue::I16(30)
            ])
        );

        // An element outside the array is never applied, so it is never
        // recorded either.
        let bad = EditorCommand::SetArrayElement {
            entity,
            component,
            field: "values".to_string(),
            index: 9,
            value: FieldValue::I16(1),
        };
        let recorded = EditorCommand::apply_recorded(&mut engine, std::slice::from_ref(&bad));
        assert_eq!(recorded.failures.len(), 1);
        assert!(!recorded.fully_invertible);
        assert!(recorded.inverses.is_empty());
    }

    #[test]
    fn recorded_apply_inverts_create_and_destroy_with_restore() {
        let mut engine = demo_engine();
        let component = demo_component_name();

        // Create: the inverse destroys the very handle the creation took.
        let create = EditorCommand::CreateEntity {
            components: vec![ComponentSeed {
                type_name: component.clone(),
                fields: vec![
                    ("strength".to_string(), FieldValue::F32(4.5)),
                    ("enabled".to_string(), FieldValue::Bool(true)),
                ],
            }],
        };
        let recorded = EditorCommand::apply_recorded(&mut engine, std::slice::from_ref(&create));
        assert!(recorded.failures.is_empty(), "{:?}", recorded.failures);
        let [EditorCommand::DestroyEntity { entity: created }] = recorded.inverses.as_slice()
        else {
            panic!(
                "create must invert to a destroy, got {:?}",
                recorded.inverses
            );
        };
        let created = *created;
        let undo = EditorCommand::apply_recorded(&mut engine, &recorded.inverses);
        assert!(undo.failures.is_empty());
        assert!(!engine.world().is_entity_valid(created));

        // Destroy: the inverse restores the entity, with its values, under a
        // fresh handle reported as a remap.
        let entity = engine
            .world_mut()
            .create_entity()
            .with(DemoComponent {
                strength: 6.5,
                enabled: false,
            })
            .build()
            .expect("entity builds");
        let destroy = EditorCommand::DestroyEntity { entity };
        let recorded = EditorCommand::apply_recorded(&mut engine, std::slice::from_ref(&destroy));
        assert!(recorded.failures.is_empty(), "{:?}", recorded.failures);
        let [EditorCommand::RestoreEntity { previous, .. }] = recorded.inverses.as_slice() else {
            panic!(
                "destroy must invert to a restore, got {:?}",
                recorded.inverses
            );
        };
        assert_eq!(*previous, entity);
        let undo = EditorCommand::apply_recorded(&mut engine, &recorded.inverses);
        assert!(undo.failures.is_empty(), "{:?}", undo.failures);
        let [(old, restored)] = undo.remaps.as_slice() else {
            panic!("restore must report the remap, got {:?}", undo.remaps);
        };
        assert_eq!(*old, entity);
        let strength = engine
            .world()
            .read_component_field(*restored, &component, "strength")
            .expect("restored value");
        assert_eq!(strength, FieldValue::F32(6.5));

        // The remap rewrites a command naming the dead handle.
        let mut later = destroy.clone();
        later.remap_entity(entity, *restored);
        assert_eq!(later, EditorCommand::DestroyEntity { entity: *restored });
    }

    #[test]
    fn recorded_apply_inverts_component_add_and_remove_with_values() {
        let mut engine = demo_engine();
        engine
            .world_mut()
            .register_component_with_layout::<ArrayComponent>(ARRAY_FIELDS);
        let entity = engine
            .world_mut()
            .create_entity()
            .with(DemoComponent {
                strength: 3.0,
                enabled: true,
            })
            .build()
            .expect("entity builds");
        let component = array_component_name();

        // Add inverts to remove.
        let add = EditorCommand::AddComponent {
            entity,
            component: component.clone(),
        };
        let recorded = EditorCommand::apply_recorded(&mut engine, std::slice::from_ref(&add));
        assert!(recorded.failures.is_empty(), "{:?}", recorded.failures);
        assert!(matches!(
            recorded.inverses.as_slice(),
            [EditorCommand::RemoveComponent { .. }]
        ));
        let undo = EditorCommand::apply_recorded(&mut engine, &recorded.inverses);
        assert!(undo.failures.is_empty());

        // Remove inverts to a restore that brings the values back.
        let failures = EditorCommand::apply(
            &mut engine,
            &[EditorCommand::AddComponent {
                entity,
                component: component.clone(),
            }],
        );
        assert!(failures.is_empty());
        let failures = EditorCommand::apply(
            &mut engine,
            &[EditorCommand::SetArrayElement {
                entity,
                component: component.clone(),
                field: "values".to_string(),
                index: 2,
                value: FieldValue::I16(42),
            }],
        );
        assert!(failures.is_empty());

        let remove = EditorCommand::RemoveComponent {
            entity,
            component: component.clone(),
        };
        let recorded = EditorCommand::apply_recorded(&mut engine, std::slice::from_ref(&remove));
        assert!(recorded.failures.is_empty(), "{:?}", recorded.failures);
        let undo = EditorCommand::apply_recorded(&mut engine, &recorded.inverses);
        assert!(undo.failures.is_empty(), "{:?}", undo.failures);
        let values = engine
            .world()
            .read_component_field(entity, &component, "values")
            .expect("values restored");
        assert_eq!(
            values,
            FieldValue::Array(vec![
                FieldValue::I16(0),
                FieldValue::I16(0),
                FieldValue::I16(42)
            ])
        );
    }

    #[test]
    fn recorded_apply_marks_engine_controls_unrecordable() {
        let mut engine = demo_engine();
        let recorded = EditorCommand::apply_recorded(
            &mut engine,
            &[
                EditorCommand::SetPaused { paused: true },
                EditorCommand::StepOnce,
            ],
        );
        assert!(recorded.failures.is_empty());
        // Controls never enter the history as steps.
        assert!(!recorded.fully_invertible);
        assert!(recorded.inverses.is_empty());
    }

    #[test]
    fn recorded_delete_group_restores_the_entity_and_the_selection() {
        let mut engine = demo_engine();
        let entity = engine
            .world_mut()
            .create_entity()
            .with(DemoComponent {
                strength: 6.5,
                enabled: true,
            })
            .build()
            .expect("entity builds");
        // The user had this entity (and a file) selected.
        let failures = EditorCommand::apply(
            &mut engine,
            &[EditorCommand::SetSelection {
                entities: vec![entity],
                asset: Some("textures/x.png".to_string()),
            }],
        );
        assert!(failures.is_empty(), "{failures:?}");

        // The delete the menu records: clear the selection, then destroy.
        let (commands, label) =
            crate::entities::entity_delete(entity, Some("textures/x.png".to_string()));
        assert_eq!(label, "delete entity");
        let recorded = EditorCommand::apply_recorded(&mut engine, &commands);
        assert!(recorded.failures.is_empty(), "{:?}", recorded.failures);
        assert!(recorded.fully_invertible);
        assert!(!engine.world().is_entity_valid(entity));
        let selection = engine
            .world()
            .get_resource::<EditorSelection>()
            .expect("the delete wrote the selection resource");
        assert_eq!(selection.primary(), None, "the delete clears the selection");

        // The undo restores the entity under a fresh handle, and the batch's
        // own selection command follows it there.
        let undo = EditorCommand::apply_recorded(&mut engine, &recorded.inverses);
        assert!(undo.failures.is_empty(), "{:?}", undo.failures);
        let [(previous, restored)] = undo.remaps.as_slice() else {
            panic!(
                "the delete's undo must report the restore's remap, got {:?}",
                undo.remaps
            );
        };
        assert_eq!(*previous, entity);
        let selection = engine
            .world()
            .get_resource::<EditorSelection>()
            .expect("selection resource");
        assert_eq!(
            selection.primary(),
            Some(*restored),
            "the selection must follow the restored entity to its fresh handle"
        );
        assert_eq!(selection.asset.as_deref(), Some("textures/x.png"));
        let strength = engine
            .world()
            .read_component_field(*restored, &demo_component_name(), "strength")
            .expect("restored value");
        assert_eq!(strength, FieldValue::F32(6.5));
    }

    /// A note file as an asset, for the asset reference tests (W-0048).
    #[derive(Debug)]
    struct Note;
    impl pill_engine::asset::Asset for Note {}
    trait_type_map::impl_trait_accessible!(dyn pill_engine::asset::Asset; Note);

    impl pill_engine::asset_metadata::ImportedAsset for Note {
        type ImportSettings = NoteSettings;
        const SOURCE_EXTENSIONS: &'static [&'static str] = &["note"];

        fn metadata_type_name() -> &'static str {
            "pill_editor_core::tests::Note"
        }

        fn import(
            _name: &str,
            _bytes: &[u8],
            _settings: &NoteSettings,
        ) -> pill_engine::AssetLoadResult<Self> {
            Ok(Self)
        }
    }

    /// Another imported type, so a wrong-type pick has something to name.
    #[derive(Debug)]
    struct Card;
    impl pill_engine::asset::Asset for Card {}
    trait_type_map::impl_trait_accessible!(dyn pill_engine::asset::Asset; Card);

    impl pill_engine::asset_metadata::ImportedAsset for Card {
        type ImportSettings = NoteSettings;
        const SOURCE_EXTENSIONS: &'static [&'static str] = &["card"];

        fn metadata_type_name() -> &'static str {
            "pill_editor_core::tests::Card"
        }

        fn import(
            _name: &str,
            _bytes: &[u8],
            _settings: &NoteSettings,
        ) -> pill_engine::AssetLoadResult<Self> {
            Ok(Self)
        }
    }

    /// The test assets' (empty) import settings.
    #[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
    struct NoteSettings {}

    /// A component with one note reference, laid out as the derive does.
    #[repr(C)]
    #[derive(Debug, Clone, Copy)]
    struct NoteHolder {
        note: pill_engine::Handle<Note>,
    }
    impl pill_engine::Component for NoteHolder {}

    static NOTE_HOLDER_FIELDS: &[ComponentFieldDescriptor] = &[ComponentFieldDescriptor {
        name: "note",
        type_tag: "struct:Handle<Note>",
        offset: 0,
        size: 8,
        align: 4,
        element_count: 0,
        decorators: std::borrow::Cow::Borrowed(&[]),
    }];

    /// One `.meta` sidecar's text: the header naming `asset_type` and `guid`.
    fn sidecar(asset_type: &str, guid: &str) -> String {
        serde_json::json!({
            "format_version": 1,
            "asset_type": asset_type,
            "guid": guid,
            "settings": {},
        })
        .to_string()
    }

    /// An engine with both test asset types, a mounted scratch `res` holding
    /// `a.note`, `b.note` and `c.card` (each with a `.meta`), the asset
    /// database over it, and one entity whose note field references nothing.
    fn reference_engine(label: &str) -> (Engine, Entity, String, std::path::PathBuf) {
        let root = std::env::temp_dir().join(format!(
            "pill-editor-asset-reference-{label}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        for (file, guid, asset_type) in [
            (
                "a.note",
                "000000000000000000000000000000a1",
                "pill_editor_core::tests::Note",
            ),
            (
                "b.note",
                "000000000000000000000000000000b1",
                "pill_editor_core::tests::Note",
            ),
            (
                "c.card",
                "000000000000000000000000000000c1",
                "pill_editor_core::tests::Card",
            ),
        ] {
            std::fs::write(root.join(file), "x").unwrap();
            std::fs::write(root.join(format!("{file}.meta")), sidecar(asset_type, guid)).unwrap();
        }
        pill_engine::asset_store::mount_directory(&root);
        let mut engine = Engine::new();
        let world = engine.world_mut();
        world.register_imported_asset::<Note>();
        world.register_imported_asset::<Card>();
        world.register_component_with_layout::<NoteHolder>(NOTE_HOLDER_FIELDS);
        world.record_asset_field_type(
            pill_engine::ComponentId::of::<NoteHolder>(),
            "note",
            "pill_editor_core::tests::Note",
        );
        let registry = world
            .get_resource::<pill_engine::ImportRegistry>()
            .unwrap()
            .clone();
        world.insert_resource(pill_engine::AssetDatabase::from_directory(&root, &registry));
        let entity = world
            .create_entity()
            .with(NoteHolder {
                note: pill_engine::Handle::INVALID,
            })
            .build()
            .unwrap();
        let name = pill_engine::component::ComponentRegistry::registered_name::<NoteHolder>();
        (engine, entity, name, root)
    }

    /// The note field's label as the Inspector would show it.
    fn note_label(engine: &Engine, entity: Entity) -> AssetReferenceLabel {
        let detail = EditorSnapshot::capture_detail(engine, entity).unwrap();
        let view = detail
            .components
            .iter()
            .find(|view| view.type_name.contains("NoteHolder"))
            .unwrap();
        view.asset_labels[0]
            .clone()
            .expect("an asset field has a label")
    }

    /// One `SetFieldAsset` for the note field.
    fn pick(entity: Entity, component: &str, reference: Option<&str>) -> EditorCommand {
        EditorCommand::SetFieldAsset {
            entity,
            component: component.to_owned(),
            field: "note".to_owned(),
            reference: reference.map(str::to_owned),
        }
    }

    /// Picking an unloaded asset by path loads it and points the field at it;
    /// a guid names an asset as well as its path; the recorded inverse
    /// restores the previous reference; `None` clears.
    ///
    /// The two asset reference tests mount one directory each, so they run as
    /// one test: the mounted directory is process-wide.
    #[test]
    fn asset_picks_load_assign_undo_clear_and_refuse() {
        let _mount = asset_mount_lock();
        let (mut engine, entity, name, root) = reference_engine("pick");
        assert_eq!(note_label(&engine, entity), AssetReferenceLabel::Unset);

        let applied =
            EditorCommand::apply_recorded(&mut engine, &[pick(entity, &name, Some("a.note"))]);
        assert!(applied.failures.is_empty(), "{:?}", applied.failures);
        assert_eq!(
            note_label(&engine, entity),
            AssetReferenceLabel::Named("a.note".to_owned())
        );

        let by_guid = EditorCommand::apply_recorded(
            &mut engine,
            &[pick(
                entity,
                &name,
                Some("000000000000000000000000000000b1"),
            )],
        );
        assert!(by_guid.failures.is_empty(), "{:?}", by_guid.failures);
        assert_eq!(
            note_label(&engine, entity),
            AssetReferenceLabel::Named("b.note".to_owned())
        );

        // Undo: the inverse is a plain field write of the previous handle.
        let undo = EditorCommand::apply_recorded(&mut engine, &by_guid.inverses);
        assert!(undo.failures.is_empty(), "{:?}", undo.failures);
        assert_eq!(
            note_label(&engine, entity),
            AssetReferenceLabel::Named("a.note".to_owned())
        );

        // Another asset type, a path the project lacks and an unknown guid
        // are refused, and the field keeps what it had.
        for reference in ["c.card", "missing.note", "000000000000000000000000000000ff"] {
            let refused =
                EditorCommand::apply_recorded(&mut engine, &[pick(entity, &name, Some(reference))]);
            assert_eq!(refused.failures.len(), 1, "{reference} is refused");
            assert_eq!(
                note_label(&engine, entity),
                AssetReferenceLabel::Named("a.note".to_owned())
            );
        }

        let cleared = EditorCommand::apply_recorded(&mut engine, &[pick(entity, &name, None)]);
        assert!(cleared.failures.is_empty(), "{:?}", cleared.failures);
        assert_eq!(note_label(&engine, entity), AssetReferenceLabel::Unset);
        std::fs::remove_dir_all(&root).unwrap();
    }

    /// A component holding a reference to another entity, as the derive lays
    /// one out (W-0048 Stage 6).
    #[repr(C)]
    #[derive(Debug, Clone, Copy, Default)]
    struct TargetHolder {
        target: Entity,
        weight: f32,
    }
    impl pill_engine::Component for TargetHolder {}

    static TARGET_HOLDER_FIELDS: &[ComponentFieldDescriptor] = &[
        ComponentFieldDescriptor {
            name: "target",
            type_tag: "entity",
            offset: 0,
            size: std::mem::size_of::<Entity>(),
            align: 8,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        },
        ComponentFieldDescriptor {
            name: "weight",
            type_tag: "f32",
            offset: 16,
            size: 4,
            align: 4,
            element_count: 0,
            decorators: std::borrow::Cow::Borrowed(&[]),
        },
    ];

    /// An engine with the holder registered, one holder (referencing nothing)
    /// and two entities it can be pointed at.
    fn holder_engine() -> (Engine, Entity, Entity, Entity) {
        let mut engine = Engine::new();
        let world = engine.world_mut();
        world.register_component_with_layout::<TargetHolder>(TARGET_HOLDER_FIELDS);
        let first = world.create_entity().build().unwrap();
        let second = world.create_entity().build().unwrap();
        let holder = world
            .create_entity()
            .with(TargetHolder {
                target: Entity::NULL,
                weight: 0.5,
            })
            .build()
            .unwrap();
        (engine, holder, first, second)
    }

    /// The holder's reference label as the Inspector would show it.
    fn target_label(engine: &Engine, entity: Entity) -> EntityReferenceLabel {
        let detail = EditorSnapshot::capture_detail(engine, entity).unwrap();
        let view = detail
            .components
            .iter()
            .find(|view| view.type_name.contains("TargetHolder"))
            .unwrap();
        view.entity_labels[0]
            .clone()
            .expect("an entity field has a label")
    }

    /// One `SetFieldEntity` for the target field.
    fn retarget(entity: Entity, component: &str, reference: Option<Entity>) -> EditorCommand {
        EditorCommand::SetFieldEntity {
            entity,
            component: component.to_owned(),
            field: "target".to_owned(),
            reference,
        }
    }

    /// Arming a row records the field a scene press names, and the request the
    /// runner takes is what the press turns into one recorded step - the same
    /// `entity_edit` the row's own picker and drop queue (W-0048 Stage 6).
    #[test]
    fn an_armed_row_answers_the_scene_press_with_one_recorded_step() {
        let (mut engine, holder, first, second) = holder_engine();
        crate::commands::ensure_command_resources(engine.world_mut());
        let name = pill_engine::component::ComponentRegistry::registered_name::<TargetHolder>();
        let armed = EditorCommand::ArmEntityPick {
            entity: holder,
            component: name.clone(),
            field: "target".to_owned(),
        };
        let outcome = EditorCommand::apply_recorded(&mut engine, &[armed]);
        assert!(outcome.failures.is_empty(), "{:?}", outcome.failures);
        let target = engine
            .world_mut()
            .get_resource_mut::<EntityPickRequest>()
            .and_then(|request| request.take())
            .expect("the arm left a target for the press");
        assert_eq!(target.entity, holder);
        assert_eq!(target.field, "target");
        assert!(
            engine
                .world_mut()
                .get_resource_mut::<EntityPickRequest>()
                .and_then(|request| request.take())
                .is_none(),
            "the press takes the request once"
        );

        // What the runner does with the answer: one recorded step, which
        // undoes back to the reference the field held.
        let (edit, label) = crate::inspector::entity_edit(
            target.entity,
            &target.component,
            &target.field,
            Some(second),
        );
        assert_eq!(label, "set TargetHolder.target");
        let applied = EditorCommand::apply_recorded(&mut engine, &[edit]);
        assert!(applied.failures.is_empty(), "{:?}", applied.failures);
        assert_eq!(
            target_label(&engine, holder),
            EntityReferenceLabel::Live(EntityListEntry::display_of(second))
        );
        let undone = EditorCommand::apply_recorded(&mut engine, &applied.inverses);
        assert!(undone.failures.is_empty(), "{:?}", undone.failures);
        assert_eq!(target_label(&engine, holder), EntityReferenceLabel::Unset);
        assert_ne!(first, second);
    }

    /// A reference reads as unset, names its entity by the Hierarchy label,
    /// writes, undoes and clears; a dangling reference reads as missing, and a
    /// reference to an entity that is gone is refused rather than written.
    #[test]
    fn entity_references_read_write_undo_clear_and_go_missing() {
        let (mut engine, holder, first, second) = holder_engine();
        let name = pill_engine::component::ComponentRegistry::registered_name::<TargetHolder>();
        assert_eq!(target_label(&engine, holder), EntityReferenceLabel::Unset);

        let applied =
            EditorCommand::apply_recorded(&mut engine, &[retarget(holder, &name, Some(first))]);
        assert!(applied.failures.is_empty(), "{:?}", applied.failures);
        assert_eq!(
            target_label(&engine, holder),
            EntityReferenceLabel::Live(EntityListEntry::display_of(first))
        );
        assert_eq!(
            engine
                .world()
                .get_component::<TargetHolder>(holder)
                .unwrap()
                .target,
            first
        );

        let rewritten =
            EditorCommand::apply_recorded(&mut engine, &[retarget(holder, &name, Some(second))]);
        assert!(rewritten.failures.is_empty(), "{:?}", rewritten.failures);
        assert_eq!(
            target_label(&engine, holder),
            EntityReferenceLabel::Live(EntityListEntry::display_of(second))
        );

        // Undo: the inverse is a plain field write of the previous handle.
        let undo = EditorCommand::apply_recorded(&mut engine, &rewritten.inverses);
        assert!(undo.failures.is_empty(), "{:?}", undo.failures);
        assert_eq!(
            target_label(&engine, holder),
            EntityReferenceLabel::Live(EntityListEntry::display_of(first))
        );

        // The target dies: the row shows the reference as missing, because the
        // field still holds the handle it was given.
        let _ = engine.world_mut().destroy_entity(second);
        let pointed_at_dead =
            EditorCommand::apply_recorded(&mut engine, &[retarget(holder, &name, Some(second))]);
        assert_eq!(
            pointed_at_dead.failures.len(),
            1,
            "a dead handle is refused"
        );

        let _ = engine.world_mut().destroy_entity(first);
        assert_eq!(target_label(&engine, holder), EntityReferenceLabel::Missing);

        let cleared = EditorCommand::apply_recorded(&mut engine, &[retarget(holder, &name, None)]);
        assert!(cleared.failures.is_empty(), "{:?}", cleared.failures);
        assert_eq!(target_label(&engine, holder), EntityReferenceLabel::Unset);
    }

    /// The mounted asset directory is process-wide, so the tests that mount
    /// one take this in turn.
    fn asset_mount_lock() -> std::sync::MutexGuard<'static, ()> {
        static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
        LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// A standalone type that refuses every document, so a create writes its
    /// file and then fails to import it (W-0088).
    #[derive(Debug)]
    struct Refused;
    impl pill_engine::asset::Asset for Refused {}
    trait_type_map::impl_trait_accessible!(dyn pill_engine::asset::Asset; Refused);

    impl pill_engine::asset_standalone::StandaloneAsset for Refused {
        type Document = RefusedDocument;
        const FILE_EXTENSION: &'static str = "refused";

        fn metadata_type_name() -> &'static str {
            "pill_editor_core::tests::Refused"
        }

        fn from_document(
            _name: &str,
            _document: RefusedDocument,
            _assets: &pill_engine::AssetManager,
        ) -> pill_engine::AssetLoadResult<Self> {
            Err(pill_engine::AssetLoadError::Decode {
                label: "refused".to_owned(),
                detail: "the test type refuses every document".to_owned(),
            })
        }
    }

    /// The refusing type's (empty) document.
    #[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
    struct RefusedDocument {}

    /// A create whose import refuses the file it just wrote leaves no file,
    /// and a delete that cannot remove the sidecar puts the asset back
    /// (W-0088).
    #[test]
    fn a_failed_create_or_delete_leaves_the_directory_as_it_found_it() {
        let _mount = asset_mount_lock();
        let root =
            std::env::temp_dir().join(format!("pill-editor-asset-rollback-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        pill_engine::asset_store::mount_directory(&root);

        let mut engine = Engine::new();
        engine.world_mut().register_standalone_asset::<Refused>();

        let failures = EditorCommand::apply(
            &mut engine,
            &[EditorCommand::CreateStandaloneAsset {
                type_name: "pill_editor_core::tests::Refused".to_owned(),
                folder: String::new(),
                name: "broken".to_owned(),
            }],
        );
        assert_eq!(failures.len(), 1, "the refusing import fails the create");
        assert!(
            failures[0].1.contains("refuses every document"),
            "{:?}",
            failures[0].1
        );
        assert!(
            !root.join("broken.refused").exists(),
            "a create whose import refused the file must not leave it behind"
        );

        // A delete removes the asset file and then its sidecar. A sidecar
        // another handle holds without delete sharing is the one state where
        // the second step fails after the first one succeeded; a platform that
        // cannot set that up has no way to reach the arm's rollback.
        #[cfg(windows)]
        {
            use std::os::windows::fs::OpenOptionsExt;

            const FILE_SHARE_READ: u32 = 0x0000_0001;
            let asset = root.join("kept.refused");
            std::fs::write(&asset, "{\"asset_type\": \"refused\"}").unwrap();
            let meta = root.join("kept.refused.meta");
            std::fs::write(
                &meta,
                sidecar(
                    "pill_editor_core::tests::Refused",
                    "000000000000000000000000000000d1",
                ),
            )
            .unwrap();
            let held = std::fs::OpenOptions::new()
                .read(true)
                .share_mode(FILE_SHARE_READ)
                .open(&meta)
                .unwrap();
            assert!(
                std::fs::remove_file(&meta).is_err(),
                "the sidecar must be unremovable for this test"
            );

            let failures = EditorCommand::apply(
                &mut engine,
                &[EditorCommand::DeleteAssetFile {
                    path: "kept.refused".to_owned(),
                }],
            );
            assert_eq!(
                failures.len(),
                1,
                "the unremovable sidecar fails the delete"
            );
            assert!(
                asset.exists(),
                "a delete that could not finish must put the asset back: {}",
                failures[0].1
            );
            drop(held);
        }

        let _ = std::fs::remove_dir_all(&root);
    }

    /// A created asset that an undo deleted comes back when the step is
    /// redone: the delete unloads the asset with its file, so the redo's
    /// create can import the fresh file instead of colliding on a guid the
    /// store still holds (W-0122).
    #[test]
    fn a_redone_create_writes_the_file_again() {
        let _mount = asset_mount_lock();
        let root =
            std::env::temp_dir().join(format!("pill-editor-asset-redo-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        pill_engine::asset_store::mount_directory(&root);

        let mut engine = Engine::new();
        pill_ui_theme::register(engine.world_mut());
        engine
            .world_mut()
            .insert_resource(crate::history::CommandHistory::default());
        engine
            .world_mut()
            .insert_resource(crate::commands::PendingEditorCommands::default());

        let create = EditorCommand::CreateStandaloneAsset {
            type_name: "pill_ui_theme::ThemeOverrides".to_owned(),
            folder: String::new(),
            name: "probe".to_owned(),
        };
        let recorded = EditorCommand::apply_recorded(&mut engine, std::slice::from_ref(&create));
        assert!(recorded.failures.is_empty(), "{:?}", recorded.failures);
        let path = root.join("probe.theme");
        assert!(path.exists(), "the create writes the file");
        {
            let world = engine.world_mut();
            let history = world
                .get_resource_mut::<crate::history::CommandHistory>()
                .unwrap();
            history.push(
                crate::history::HistoryEntry::engine(
                    "create asset probe".to_owned(),
                    vec![create],
                    recorded.inverses,
                ),
                0,
                crate::history::CommandHistory::DEFAULT_COALESCE_WINDOW,
            );
        }

        let undo = crate::drain::step_history(&mut engine, crate::drain::HistoryDirection::Undo);
        assert!(
            matches!(undo, crate::drain::HistoryOutcome::Queued { .. }),
            "{undo:?}"
        );
        crate::drain::apply_pending(&mut engine, 1);
        assert!(!path.exists(), "the undo deletes the file");

        let redo = crate::drain::step_history(&mut engine, crate::drain::HistoryDirection::Redo);
        assert!(
            matches!(redo, crate::drain::HistoryOutcome::Queued { .. }),
            "{redo:?}"
        );
        let replay = {
            let world = engine.world_mut();
            let pending = world
                .get_resource_mut::<crate::commands::PendingEditorCommands>()
                .unwrap();
            std::mem::take(&mut pending.batches)
        };
        assert_eq!(replay.len(), 1, "the redo queues one batch");
        let recorded = EditorCommand::apply_recorded(&mut engine, &replay[0].commands);
        assert!(recorded.failures.is_empty(), "{:?}", recorded.failures);
        assert!(path.exists(), "the redo writes the file again");

        let _ = std::fs::remove_dir_all(&root);
    }
}
