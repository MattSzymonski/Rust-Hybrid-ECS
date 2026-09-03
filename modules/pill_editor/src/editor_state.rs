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
}

/// The expensive tier of the snapshot: only built for the selection.
#[derive(Clone, Debug, PartialEq)]
pub struct EntityDetail {
    /// The entity this detail describes.
    pub entity: Entity,
    /// Every attached component with its current field values.
    pub components: Vec<ComponentView>,
}

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
    /// Registered systems with owner labels.
    pub systems: Vec<SystemSummary>,
    /// Whether the engine's system phase is paused.
    pub paused: bool,
    /// Command failures from the last apply batch, newest first.
    pub errors: Vec<String>,
}

impl EditorSnapshot {
    /// Rebuild the cheap tiers (entities, systems, pause state) from a live
    /// engine. Never mutates the world.
    pub fn capture_list(
        engine: &Engine,
        revision: u64,
        optional_module_names: &[String],
        errors: Vec<String>,
    ) -> Self {
        let entities = engine
            .world()
            .entity_rows()
            .into_iter()
            .map(|row| {
                let display = format!("{}v{}", row.entity.id(), row.entity.generation());
                let archetype_label = row.components.join(", ");
                let component_count = row.components.len();
                EntityListEntry {
                    entity: row.entity,
                    display,
                    component_count,
                    archetype_label,
                }
            })
            .collect();
        let systems = engine
            .system_snapshots()
            .into_iter()
            .map(|system| SystemSummary {
                index: system.index,
                owner_label: owner_label(system.owner, optional_module_names),
                name: system.name,
                enabled: system.enabled,
                hot_patchable: system.hot_patchable,
                name_is_ambiguous: system.name_is_ambiguous,
            })
            .collect();
        Self {
            revision,
            entities,
            detail: None,
            systems,
            paused: engine.systems_paused(),
            errors,
        }
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
                Some(ComponentView {
                    type_name,
                    persistable,
                    editable,
                    fields,
                    values,
                })
            })
            .collect();
        Some(EntityDetail { entity, components })
    }
}

/// Label a system owner using the host's optional-module names.
///
/// `SystemOwner(0)` is the project, `SystemOwner(n)` labels module `n - 1`;
/// anything out of range renders as "unknown".
fn owner_label(owner: SystemOwner, optional_module_names: &[String]) -> String {
    match owner.0 {
        0 => "project".to_string(),
        index => optional_module_names
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
    /// Run exactly one system phase on the next frame, then pause again.
    StepOnce,
}

impl EditorCommand {
    /// Apply a batch at the frame boundary.
    ///
    /// Returns one `(index, message)` per failed command so the Console panel
    /// can surface it. A failure never aborts the rest of the batch and never
    /// leaves partial state: every engine call this dispatches is
    /// all-or-nothing, and structural commands validate before they commit.
    pub fn apply(engine: &mut Engine, commands: &[EditorCommand]) -> Vec<(usize, String)> {
        let mut failures = Vec::new();
        for (index, command) in commands.iter().enumerate() {
            if let Err(message) = Self::apply_one(engine, command) {
                failures.push((index, message));
            }
        }
        failures
    }

    /// Apply one command; the message is empty on success.
    fn apply_one(engine: &mut Engine, command: &EditorCommand) -> Result<(), String> {
        match command {
            EditorCommand::SetField {
                entity,
                component,
                field,
                value,
            } => engine
                .world_mut()
                .write_component_field(*entity, component, field, value.clone())
                .map_err(|error| format!("{command:?}: {error}")),

            EditorCommand::SetArrayElement {
                entity,
                component,
                field,
                index,
                value,
            } => engine
                .world_mut()
                .write_component_array_element(*entity, component, field, *index, value.clone())
                .map_err(|error| format!("{command:?}: {error}")),

            EditorCommand::SetSystemEnabled { index, enabled } => {
                if engine.set_system_enabled_at(*index, *enabled) {
                    Ok(())
                } else {
                    Err(format!(
                        "{command:?}: no system at registration index {index}"
                    ))
                }
            }

            EditorCommand::SetPaused { paused } => {
                engine.set_systems_paused(*paused);
                Ok(())
            }

            EditorCommand::StepOnce => {
                engine.request_single_step();
                Ok(())
            }

            EditorCommand::DestroyEntity { entity } => {
                if engine.world_mut().destroy_entity(*entity) {
                    Ok(())
                } else {
                    Err(format!("{command:?}: entity was already gone"))
                }
            }

            EditorCommand::CreateEntity { components } => {
                // Step 1: Validate and compose every component image before
                // touching the world, so a bad seed changes nothing.
                let mut native: Vec<Box<dyn pill_engine::commands::ComponentAdder>> = Vec::new();
                let mut dynamic = Vec::new();
                for seed in components {
                    // Build the image first; this validates the layout rule.
                    let bytes = engine
                        .world()
                        .build_component_image(&seed.type_name, &seed.fields)
                        .map_err(|error| format!("{command:?}: {error}"))?;
                    let component_id = engine
                        .world()
                        .resolve_component_id_by_name_any(&seed.type_name)
                        .ok_or_else(|| {
                            format!("{command:?}: `{}` is not registered", seed.type_name)
                        })?;
                    if component_id.native_type_id().is_some() {
                        native.push(Box::new(pill_engine::commands::ByteComponentAdder::new(
                            component_id,
                            bytes,
                        )));
                    } else {
                        dynamic.push((component_id, bytes));
                    }
                }

                // Step 2: Reserve the handle and queue the creation inside the
                // same closure that holds the world.
                engine.queue_deferred_commands(move |world, queue| {
                    let entity = world.reserve_entity();
                    queue.create_mixed_entity(entity, native, dynamic);
                });
                engine
                    .flush_deferred_commands()
                    .map_err(|errors| format!("{command:?}: {errors:?}"))
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
                    .ok_or_else(|| format!("{command:?}: `{component}` is not registered"))?;
                engine.queue_deferred_commands(move |_world, queue| {
                    if component_id.native_type_id().is_some() {
                        queue.add_component_adder_to_entity(
                            *entity,
                            Box::new(pill_engine::commands::ByteComponentAdder::new(
                                component_id,
                                bytes,
                            )),
                        );
                    } else {
                        queue.add_dynamic_component_to_entity(*entity, component_id, bytes);
                    }
                });
                engine
                    .flush_deferred_commands()
                    .map_err(|errors| format!("{command:?}: {errors:?}"))
            }

            EditorCommand::RemoveComponent { entity, component } => {
                let component_id = engine
                    .world()
                    .resolve_entity_component_id(*entity, component)
                    .ok_or_else(|| format!("{command:?}: `{component}` is not on the entity"))?;
                engine.queue_deferred_commands(move |_world, queue| {
                    queue.remove_component_by_id(*entity, component_id);
                });
                engine
                    .flush_deferred_commands()
                    .map_err(|errors| format!("{command:?}: {errors:?}"))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A plain component with a field layout, so entity create and field
    /// writes have something to target.
    #[repr(C)]
    #[derive(Debug, Clone, Copy, PartialEq)]
    struct DemoComponent {
        strength: f32,
        enabled: bool,
    }
    impl pill_engine::Component for DemoComponent {}
    trait_type_map::impl_trait_accessible!(dyn pill_engine::Component; DemoComponent);

    /// Hand-written layout mirroring what `#[derive(PillComponent)]` emits.
    static DEMO_FIELDS: &[ComponentFieldDescriptor] = &[
        ComponentFieldDescriptor {
            name: "strength",
            type_tag: "f32",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 0,
        },
        ComponentFieldDescriptor {
            name: "enabled",
            type_tag: "bool",
            offset: 4,
            size: 1,
            align: 1,
            element_count: 0,
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
    trait_type_map::impl_trait_accessible!(dyn pill_engine::Component; ArrayComponent);

    static ARRAY_FIELDS: &[ComponentFieldDescriptor] = &[ComponentFieldDescriptor {
        name: "values",
        type_tag: "array:i16",
        offset: 0,
        size: 6,
        align: 2,
        element_count: 3,
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

    /// The editor's generic field write reaches the real shared renderer
    /// `Sprite` layout (flattened `color.r` … `color.a` at absolute offsets),
    /// which is the path the Inspector uses to repaint sprites live.
    #[test]
    fn set_field_on_renderer_sprite_repaints_color_channels() {
        use pill_engine::render::{Color, Sprite};

        let mut engine = Engine::new();
        engine.world_mut().register_component::<Sprite>();
        let entity = engine
            .world_mut()
            .create_entity()
            .with(Sprite {
                width: 40.0,
                height: 30.0,
                color: Color::new(1.0, 0.0, 0.0, 1.0),
            })
            .build()
            .expect("entity builds");
        let sprite_name = std::any::type_name::<Sprite>().to_string();

        let failures = EditorCommand::apply(
            &mut engine,
            &[
                EditorCommand::SetField {
                    entity,
                    component: sprite_name.clone(),
                    field: "width".to_string(),
                    value: FieldValue::F32(96.0),
                },
                EditorCommand::SetField {
                    entity,
                    component: sprite_name.clone(),
                    field: "color.r".to_string(),
                    value: FieldValue::F32(0.1),
                },
                EditorCommand::SetField {
                    entity,
                    component: sprite_name.clone(),
                    field: "color.g".to_string(),
                    value: FieldValue::F32(0.2),
                },
                EditorCommand::SetField {
                    entity,
                    component: sprite_name.clone(),
                    field: "color.b".to_string(),
                    value: FieldValue::F32(0.3),
                },
                EditorCommand::SetField {
                    entity,
                    component: sprite_name.clone(),
                    field: "color.a".to_string(),
                    value: FieldValue::F32(0.5),
                },
            ],
        );
        assert!(failures.is_empty(), "sprite writes failed: {failures:?}");

        // The write landed on the real component: width and the four channels
        // are updated at their flattened layout offsets.
        let values = engine
            .world()
            .read_component_fields(entity, &sprite_name)
            .expect("read sprite");
        assert_eq!(
            values,
            vec![
                FieldValue::F32(96.0),
                FieldValue::F32(30.0),
                FieldValue::F32(0.1),
                FieldValue::F32(0.2),
                FieldValue::F32(0.3),
                FieldValue::F32(0.5),
            ]
        );
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
        engine.begin_module_registration(SystemOwner::optional_module(0));
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
}
