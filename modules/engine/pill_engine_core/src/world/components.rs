//! Component registration, storage access, and per-entity attachment.
//!
//! # Responsibilities
//!
//! - Registers component types of all three kinds - a Rust type, a type shared
//!   between DLLs, and a layout described at runtime by a manifest - and keeps
//!   the storage description that a reload replaces.
//! - Reshapes and re-homes live columns across a reload. A relayout rewrites a
//!   column's rows in a new shape, a remap moves them onto a successor
//!   registration, and the re-homing pass points every column's function table
//!   at code that is still mapped.
//! - Reads and writes one entity's component, and adds or removes one, which
//!   moves the entity to another archetype.
//!
//! # Design
//!
//! Registration is data, not code. A component's storage description is a
//! layout plus a table of function pointers, never a closure compiled into the
//! DLL that registered it, so a column survives the DLL that filled it and the
//! table is replaced on reload. The three kinds differ only in what proves a
//! row holds the right type - an exact `TypeId`, an identity derived from a
//! name, or just a validated layout - and all three share one column type.

use super::*;

// =============================================================================
// ColumnChunk
// =============================================================================

/// One archetype's raw rows of one component, as a language binding reads them.
///
/// The four language-binding chunk accessors return this type. Because every
/// field is named, a binding reads `len` as a row count and `element_size` as
/// the row stride, instead of remembering their order in a tuple.
pub struct ColumnChunk<'a> {
    /// The id of the archetype the rows belong to.
    pub archetype: ArchetypeId,
    /// The column's contiguous row buffer, one `element_size` block per row.
    pub data: *mut u8,
    /// Rows in the column.
    pub len: usize,
    /// Bytes per row, read from the live column.
    pub element_size: usize,
    /// The column's change ticks, one per row.
    pub ticks: &'a mut [ComponentTicks],
}

// =============================================================================
// World - Components
// =============================================================================

impl World {
    /// The first shared component in `declared` whose registered layout differs
    /// from the layout the reading binary's type declares, if any.
    ///
    /// `declared` lists each shared component a query targets together with the
    /// schema hash its type declares ([`crate::QueryTarget::declared_shared_schemas`]).
    /// A component whose registration carries no hash is skipped, because there
    /// is no evidence either way; the column's size and alignment are still
    /// checked. Each mismatch is logged once per component and declared hash,
    /// because the query that hits it is usually rebuilt every frame.
    pub(crate) fn find_stale_shared_reader(
        &mut self,
        declared: &[(ComponentId, u64)],
        query_type: &str,
    ) -> Option<ComponentId> {
        for &(component_id, declared_hash) in declared {
            let Some(registered_hash) = self
                .component_registry
                .get_layout(&component_id)
                .and_then(|layout| layout.schema_hash)
            else {
                continue;
            };
            if registered_hash == declared_hash {
                continue;
            }
            // Log each mismatched pair only once; the refusal itself happens on
            // every call.
            if !self
                .reported_schema_mismatches
                .contains(&(component_id, declared_hash))
            {
                self.reported_schema_mismatches
                    .push((component_id, declared_hash));
                let component = self
                    .component_registry
                    .get_name(&component_id)
                    .unwrap_or("<unnamed>")
                    .to_owned();
                error!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    component = %component,
                    query = query_type,
                    declared_hash = format!("{declared_hash:#018x}"),
                    registered_hash = format!("{registered_hash:#018x}"),
                    "query refused: its binary was built against a different layout of this shared component than the one registered now; it matches nothing until that binary is rebuilt"
                );
            }
            return Some(component_id);
        }
        None
    }

    /// Reserve capacity for at least `additional` instances of component `T`
    /// in every archetype that currently contains `T`.
    ///
    /// This is a hint, not a promise: every archetype holding `T` reserves
    /// `additional` rows, so the total reserved can be more than the number
    /// asked for. Call it after registering components and after creating the
    /// archetypes you intend to populate.
    pub fn reserve_components<T>(&mut self, additional: usize)
    where
        T: Component,
    {
        let component_id = ComponentId::of::<T>();
        for archetype in self.archetypes.values_mut() {
            if archetype.component_types.contains(&component_id) {
                let storage = archetype.component_storages.column_of_mut::<T>();
                storage.reserve::<T>(additional);
            }
        }
    }

    /// Return one component chunk together with its parallel change-tick column.
    ///
    /// Language bindings use this form when exposing writable component data.
    /// Both slices have the same length, and row `i` in `ticks` describes row
    /// `i` in the returned component slice. `chunk_index` counts the archetypes
    /// that hold `T` in the map's iteration order, so it is only stable for the
    /// pass it is read in.
    pub fn component_chunk_with_ticks_mut<T>(
        &mut self,
        chunk_index: usize,
    ) -> Option<(ArchetypeId, &mut [T], &mut [ComponentTicks])>
    where
        T: Component,
    {
        let component_id = ComponentId::of::<T>();
        let archetype = self
            .archetypes
            .values_mut()
            .filter(|archetype| archetype.component_types.contains(&component_id))
            .nth(chunk_index)?;
        let archetype_id = archetype.id;
        let (components, ticks) = archetype
            .component_storages
            .column_of_mut::<T>()
            .rows_and_ticks_mut::<T>();
        Some((archetype_id, components, ticks))
    }

    /// Return one component chunk from one already-known archetype, with its
    /// parallel change-tick column.
    ///
    /// The archetype-scoped twin of [`Self::component_chunk_with_ticks_mut`]: a
    /// binding that already has an archetype id reads the chunk directly
    /// instead of counting chunks until the archetypes match.
    pub fn component_chunk_with_ticks_mut_in_archetype<T>(
        &mut self,
        archetype_id: ArchetypeId,
    ) -> Option<(ArchetypeId, &mut [T], &mut [ComponentTicks])>
    where
        T: Component,
    {
        let component_id = ComponentId::of::<T>();
        let archetype = self.archetypes.get_mut(&archetype_id)?;
        if !archetype.component_types.contains(&component_id) {
            return None;
        }
        let (components, ticks) = archetype
            .component_storages
            .column_of_mut::<T>()
            .rows_and_ticks_mut::<T>();
        Some((archetype_id, components, ticks))
    }

    /// Register a component type with the World.
    ///
    /// Call it for every component type before that type is used.
    ///
    /// A component does not need `Clone`. An archetype move copies its row byte
    /// for byte, so a type that must not be cloned - a handle, a guard,
    /// anything whose duplication would mean something - registers like any
    /// other.
    pub fn register_component<T>(&mut self)
    where
        T: Component,
    {
        self.register_component_inner::<T>(&[]);
    }

    /// Shared registration body for the layout-less and layout-carrying entry
    /// points.
    ///
    /// `fields` is the compile-time field layout, when the caller has one. The
    /// registry records it, so a second registration of the same component can
    /// be checked against the first. For an ordinary component that check is a
    /// diagnostic; for a shared one it is the soundness check.
    ///
    /// # Shared components
    ///
    /// A component that declares [`Component::shared_name`] gets the same
    /// [`ComponentId`] in every DLL that links it. A second DLL's registration
    /// therefore finds the bit already taken and **binds to the existing
    /// column** instead of allocating a new one: the registry reports
    /// `AlreadyPresent`, no new bit is used, and entities spawned from either
    /// DLL get the same archetype. All of that follows from the ids being equal,
    /// so there is no moment when two DLLs share a column but not a mask bit.
    pub(crate) fn register_component_inner<T>(
        &mut self,
        fields: &'static [crate::component_registry::ComponentFieldDescriptor],
    ) where
        T: Component,
    {
        self.register_component_inner_superseding::<T>(fields, false);
    }

    /// [`Self::register_component_inner`] for a registration the host announced
    /// as replacing an earlier generation of the same subject, which may give a
    /// shared component a new layout (see
    /// [`crate::component::ComponentRegistry::register_with_layout_superseding`]).
    pub(crate) fn register_component_inner_superseding<T>(
        &mut self,
        fields: &'static [crate::component_registry::ComponentFieldDescriptor],
        superseding: bool,
    ) where
        T: Component,
    {
        let _zone = crate::profile_scope!(
            "register component",
            [(
                "Component type being registered: {}",
                std::any::type_name::<T>()
            )]
        );
        let component_id = ComponentId::of::<T>();
        let type_name = crate::component::ComponentRegistry::registered_name::<T>();

        // Step 1: Register the component - bit index, name and layout. This
        // calls `register_with_layout_superseding` rather than `register`: the
        // world does not act on whether the type was already present, and
        // registering again is normal here because a hot reload re-runs every
        // `init`.
        let bit = match self
            .component_registry
            .register_with_layout_superseding::<T>(fields, superseding)
            .map(crate::component::Registration::bit)
        {
            Ok(bit) => bit,
            Err(error) => {
                // Hitting the 128-type ceiling or a shared-layout disagreement
                // is not a programming error: both follow from what the user's
                // binaries declare. So the failure is logged, recorded for the
                // init entry point, and not a panic. The caller (project or
                // module init) fails the whole reload when
                // `component_registry::register_all_components` drains it.
                error!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    type_name = %type_name,
                    error = %error,
                    remaining = self.component_registry.available_slots(),
                    "component registration failed"
                );
                self.record_registration_error(error);
                return;
            }
        };
        // Step 2: Warn while slots are still free but running out, so the
        // ceiling shows up before registration starts failing.
        let remaining = self.component_registry.available_slots();
        if remaining <= REGISTRATION_HEADROOM_WARNING_THRESHOLD {
            warn!(
                target: pill_core::telemetry::telemetry_target::ECS,
                remaining,
                type_name = %type_name,
                "component-type headroom is low; the 128-type ceiling is approaching"
            );
        }

        // Step 3: Record the registration in order, so the host can list every
        // type one module's init registered - plain or persistable. That is how
        // a type dropped from a reloaded module is told apart from one only
        // downgraded to a plain component. The id is stored beside the name
        // because a rebuilt DLL gets a fresh `TypeId` for each name: comparing
        // ids is what separates a failed generation's leftovers from a rollback
        // generation's re-registration.
        self.component_registration_log.push((
            type_name,
            component_id,
            self.component_registration_sequence,
        ));
        self.component_registration_sequence = self.component_registration_sequence.wrapping_add(1);

        // Step 4: Store the storage factory as plain data - type id, layout and
        // a per-type function table - instead of a closure compiled into this
        // DLL. `Archetype::new` then builds a concrete `ComponentColumn` with no
        // vtable, and the table is replaced on every reload, so a column
        // survives a DLL unload.
        //
        // A shared component's column is built with `of_shared`, so its
        // element-type check compares the layout instead of the `TypeId`. That
        // is where the compiler stops checking the type and the registry's
        // layout check above takes over: the second DLL's `T` has a different
        // `TypeId` for the same type, and only the recorded size, alignment and
        // schema hash show that it really is the same type.
        //
        // Registering again replaces the factory, so the function table points
        // at the newest loaded DLL. That is the same last-writer-wins rule the
        // reload path relies on, and `rehome_native_columns` points live
        // columns at the new table afterwards.
        let native_column_info = if let Some(shared) = T::shared_name() {
            // One line per registration, so a declared identity shows up in the
            // startup and reload logs rather than only in the periodic report,
            // which needs frames.
            info!(
                target: pill_core::telemetry::telemetry_target::ECS,
                shared,
                bit,
                "component registered under a declared shared identity"
            );
            crate::archetype::NativeColumnInfo::of::<T>(true)
        } else {
            crate::archetype::NativeColumnInfo::of::<T>(false)
        };
        // Keep the table that is about to be replaced: the migration that
        // consumes old-layout columns has to drop them through the code that
        // allocated them, and by then every column already carries this
        // registration's table. See `migrate_component_column_in_place`.
        if let Some(StorageFactory::Native(previous)) = self.storage_factories.get(&component_id) {
            self.retired_native_storage_ops
                .insert(component_id, previous.ops);
        }
        self.storage_factories
            .insert(component_id, StorageFactory::Native(native_column_info));
    }

    /// Drain the first registration failure recorded by
    /// [`Self::register_component`], if any.
    ///
    /// The artifact-wide registration loop calls this after running every
    /// descriptor, so the generated `init` can fail the reload with a non-zero
    /// status when the component-type ceiling was hit. That makes a startup
    /// failure diagnosable instead of leaving the world half registered.
    pub fn take_registration_error(&mut self) -> Option<WorldError> {
        self.registration_error.take()
    }

    /// Record a registration failure so the artifact-wide registration loop
    /// fails the reload the same way it does for the component-type ceiling.
    ///
    /// The first failure wins: later registrations in the same pass are often
    /// knock-on effects of the first, and the first one is the diagnosable one.
    pub(crate) fn record_registration_error(&mut self, error: WorldError) {
        if self.registration_error.is_none() {
            self.registration_error = Some(error);
        }
    }

    /// Register a component together with its compile-time field layout, so
    /// the C# mirror codegen can emit a typed struct. Components registered
    /// without field metadata (hand-registered, descriptor or unit types) keep
    /// the opaque ABI-blob mirror.
    ///
    /// Nested `struct:` fields become dotted leaf rows when the layout is
    /// recorded for the editor; see `record_component_field_layout`.
    pub fn register_component_with_layout<T>(
        &mut self,
        fields: &'static [crate::component_registry::ComponentFieldDescriptor],
    ) where
        T: Component,
    {
        self.register_component_inner::<T>(fields);
        self.record_component_field_layout(ComponentId::of::<T>(), fields);
    }

    /// Record a runtime-described layout for a descriptor component.
    ///
    /// Overwrites any previous layout for the same id, so a manifest reload
    /// replaces the old layout instead of adding to it. The C# backend uses
    /// this so managed components become field-inspectable in the editor.
    ///
    /// # Errors
    ///
    /// Returns [`ComponentFieldError::UnsupportedField`] for a descriptor that
    /// reaches past the component's registered size, or names a container tag.
    ///
    /// A descriptor row is raw bytes with no Rust value in it, so the accessors
    /// must be able to trust the bounds and the tag. The size check keeps a read
    /// inside the row. The tag check stops a `(pointer, length)` pair from being
    /// followed in a row that never held one.
    pub fn register_component_descriptor_with_layout(
        &mut self,
        component_id: ComponentId,
        fields: Vec<crate::component_registry::ComponentFieldDescriptor>,
    ) -> Result<(), crate::component_field::ComponentFieldError> {
        let component_size = self.component_layout(component_id).map(|(size, _)| size);
        for field in &fields {
            let Some(end) = field.offset.checked_add(field.size) else {
                return Err(
                    crate::component_field::ComponentFieldError::UnsupportedField {
                        field: field.name.to_string(),
                        reason: "the field's range overflows",
                    },
                );
            };
            if component_size.is_some_and(|size| end > size) {
                return Err(
                    crate::component_field::ComponentFieldError::UnsupportedField {
                        field: field.name.to_string(),
                        reason: "the field extends past the component's registered size",
                    },
                );
            }
            if !component_id.is_native_storage()
                && crate::component_field::is_container_tag(field.type_tag)
            {
                return Err(crate::component_field::ComponentFieldError::UnsupportedField {
                    field: field.name.to_string(),
                    reason: "a container tag is read as a native pointer and length, which a descriptor row does not hold",
                });
            }
        }
        self.component_field_layouts
            .insert(component_id, ComponentFieldLayout::from_owned(fields));
        Ok(())
    }

    /// Point every native column's per-type function table at the newest code.
    ///
    /// The host calls this after each generation's `init` and before migration.
    /// A column stores function pointers into the DLL that created it, so those
    /// pointers dangle once that DLL is evicted from the reload graveyard. This
    /// pass takes each column's table from the newest factory registered for its
    /// component id, so:
    ///
    /// - an unchanged type points at the newly loaded generation (mapped),
    /// - a type whose schema changed points at the previous generation (mapped
    ///   until the migration drops its columns),
    /// - a type-erased (foreign-language) column is left alone.
    pub fn rehome_native_columns(&mut self) {
        // Step 1: Copy the current per-type function tables out first, so the
        // immutable borrow of `storage_factories` does not overlap the mutable
        // borrow of `archetypes` below.
        let factory_ops: HashMap<ComponentId, crate::archetype::ColumnOps> = self
            .storage_factories
            .iter()
            .filter_map(|(component_id, factory)| match factory {
                StorageFactory::Native(info) => Some((*component_id, info.ops)),
                StorageFactory::Descriptor(_) => None,
            })
            .collect();

        // Step 2: Give every column whose component id has a native factory the
        // table that factory holds.
        for archetype in self.archetypes.values_mut() {
            for &component_id in &archetype.component_types {
                let Some(&ops) = factory_ops.get(&component_id) else {
                    continue;
                };
                if let Some(column) = archetype.component_storages.get_mut(component_id) {
                    column.refresh_ops(ops);
                }
            }
        }
    }

    /// Counts the native columns the world still stores although no factory
    /// describes their component id.
    ///
    /// A column can outlive its registration: when a type is registered again
    /// under a fresh id - the rebuilt DLL's "same name, fresh `TypeId`" case -
    /// the old id is never removed, so the archetype keeps the id in its mask
    /// and the column in its storage, but no factory describes the type any
    /// more. The host's reload path checks this count before it unmaps a
    /// retired image, because a column's drop table lives in the image that
    /// produced it.
    pub fn columns_without_factory(&self) -> usize {
        self.archetypes
            .values()
            .flat_map(|archetype| archetype.component_types.iter())
            .filter(|component_id| {
                component_id.is_native_storage()
                    && !self.storage_factories.contains_key(component_id)
            })
            .count()
    }

    /// Drop every native column whose component id has no factory left.
    ///
    /// The id-keyed counterpart of `drop_forgotten_components`. The name-keyed
    /// sweep cannot reach these, because the name resolves to whichever
    /// registration claimed it last, while the stale column keeps the id the
    /// previous generation created. Each affected component is removed from
    /// every entity that carries it, which empties the archetypes holding it
    /// and drops their columns through the table that is still mapped while
    /// this runs. Returns the number of orphaned component ids dropped.
    pub fn drop_columns_without_factory(&mut self) -> usize {
        // Step 1: Collect the orphaned ids across every archetype. Deduplicated
        // by hand: the list is a handful of entries at most, and a `HashSet`
        // would hide the order.
        let mut orphaned_ids: Vec<ComponentId> = Vec::new();
        for archetype in self.archetypes.values() {
            for &component_id in &archetype.component_types {
                if component_id.is_native_storage()
                    && !self.storage_factories.contains_key(&component_id)
                    && !orphaned_ids.contains(&component_id)
                {
                    orphaned_ids.push(component_id);
                }
            }
        }

        // Step 2: Remove each orphaned component from every entity carrying it.
        // The entities are collected up front, so the mutable borrows during
        // removal never overlap the iteration, exactly as in
        // `drop_forgotten_components`.
        for component_id in &orphaned_ids {
            let entities: Vec<Entity> = self
                .entity_locations
                .iter()
                .filter(|(_, location)| {
                    self.archetypes
                        .get(&location.archetype_id)
                        .is_some_and(|archetype| archetype.component_types.contains(component_id))
                })
                .map(|(entity, _)| *entity)
                .collect();
            for entity in entities {
                let _ = self.remove_component_by_id(entity, *component_id);
            }
        }
        orphaned_ids.len()
    }

    /// Register an unmanaged component described by an external language.
    ///
    /// Re-registering the same layout and name under the same `stable_id` does
    /// nothing and returns the existing [`ComponentId`].
    ///
    /// `blittability` is the caller's word that every field of the component is
    /// a blittable value type, and it is required rather than optional. The
    /// storage built from this layout copies rows with `ptr::copy`, frees them
    /// without running element destructors, and is shared across threads, so no
    /// check this function could run would make an owning layout safe.
    /// [`Blittability`] documents what each constructor proves.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DescriptorStableIdZero`] for a zero `stable_id`,
    /// [`WorldError::DescriptorSizeZero`] for a zero `size`,
    /// [`WorldError::DescriptorAlignmentInvalid`] for a zero or non-power-of-two
    /// `align`, [`WorldError::DescriptorLayoutInvalid`] for an oversized layout,
    /// [`WorldError::DescriptorAlreadyRegistered`] when the `stable_id` is
    /// already taken by a different layout or name,
    /// [`WorldError::ComponentNameCollision`] when another registered component
    /// already claims the name and still holds rows, and
    /// [`WorldError::ComponentTypeLimitExceeded`] when the registry is full.
    pub fn register_component_descriptor(
        &mut self,
        stable_id: u128,
        name: impl Into<String>,
        size: usize,
        align: usize,
        schema_hash: u64,
        blittability: Blittability,
    ) -> Result<ComponentId, WorldError> {
        if stable_id == 0 {
            return Err(WorldError::DescriptorStableIdZero);
        }
        // The same three checks a relayout runs, so a layout this refuses can
        // never be one the storage would accept later.
        validate_component_layout(size, align)?;
        let name = name.into();
        let component_id = ComponentId::descriptor(stable_id);
        if let Some(existing) = self.storage_factories.get(&component_id) {
            return match existing {
                StorageFactory::Descriptor(layout)
                    if layout.size == size
                        && layout.align == align
                        && layout.schema_hash == schema_hash
                        && self.component_registry.get_name(&component_id) == Some(&name) =>
                {
                    Ok(component_id)
                }
                _ => Err(WorldError::DescriptorAlreadyRegistered),
            };
        }
        // A name already claimed by a live column belongs to a different
        // component, not to an earlier generation of this one. A manifest reload
        // derives `stable_id` from the same full name, so it takes the
        // idempotent path above instead of reaching here. A second component
        // under that name would make every later lookup by name ambiguous, and
        // managed code binds by name, so the collision is reported now instead
        // of producing a binding to the wrong column.
        if let Some((existing_id, live_rows)) = self.live_component_with_name(&name, component_id) {
            return Err(WorldError::ComponentNameCollision {
                type_name: name,
                existing_id,
                incoming_id: component_id,
                live_rows,
            });
        }

        // The registry reports the 128-type ceiling as a typed error carrying
        // the offending name and the current count, rather than panicking. Pass
        // it on.
        self.component_registry
            .register_descriptor(stable_id, name)?;
        self.storage_factories.insert(
            component_id,
            StorageFactory::Descriptor(ColumnLayout::new(size, align, schema_hash, blittability)?),
        );
        Ok(component_id)
    }

    /// Replace a descriptor component's layout and migrate every stored row.
    ///
    /// This is the only way a descriptor component's storage shape may change
    /// after registration. The component keeps its id, its registry bit and its
    /// archetype membership, because a fresh registration would allocate a new
    /// bit index, and that index is stored in archetype masks and scheduled
    /// access masks. Only the columns' element layout and the recorded size
    /// change.
    ///
    /// `plan` says where each byte of a new row comes from ([`FieldPlan::between`]
    /// builds one from two field lists); anything it does not cover is left
    /// zero. Row order and change ticks are kept, so entity locations stay valid
    /// and no system sees a spurious `Added`. The new layout and the plan are
    /// checked before the first column is touched, so a rejected call leaves the
    /// world exactly as it was.
    ///
    /// Returns the number of rows migrated.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DescriptorSizeZero`],
    /// [`WorldError::DescriptorAlignmentInvalid`] or
    /// [`WorldError::DescriptorLayoutInvalid`] when the new layout cannot describe
    /// storage, [`WorldError::DescriptorComponentNotRegistered`] when the id is not
    /// a registered descriptor component, [`WorldError::DescriptorStorageMissing`]
    /// when an archetype lists the component without a column,
    /// [`WorldError::ComponentColumnLayoutMismatch`] when a column's element size
    /// disagrees with the registered layout, and [`WorldError::DescriptorRowInvalid`]
    /// when a planned field falls outside a row of either layout.
    ///
    /// # Panics
    ///
    /// Panics only if an archetype listed by the verification pass has lost the
    /// column by the time it is migrated, which is an internal-invariant break.
    pub fn relayout_descriptor_component(
        &mut self,
        component_id: ComponentId,
        size: usize,
        align: usize,
        schema_hash: u64,
        plan: &FieldPlan,
    ) -> Result<usize, WorldError> {
        validate_component_layout(size, align)?;

        // The previous layout is what the plan's source offsets are measured
        // against, so it has to be the one the columns are actually using.
        let Some(StorageFactory::Descriptor(previous)) = self.storage_factories.get(&component_id)
        else {
            return Err(WorldError::DescriptorComponentNotRegistered { id: component_id });
        };
        let previous_size = previous.size;
        plan.validate(previous_size, size)?;

        // Every archetype holding the component, including the ones with no
        // rows: their columns must carry the new element layout, so the next
        // entity added to them is stored in the new shape.
        // A relayout keeps `blittability`: the shape changes, but the promise
        // about what a row owns does not.
        let layout = ColumnLayout {
            size,
            align,
            schema_hash,
            blittability: previous.blittability,
        };

        // Step 1: Check every column before touching any of them. The plan and
        // the factory were checked above; the remaining risk is a column that
        // disagrees with the factory, which is the desync a partly applied
        // relayout leaves behind. So the whole set is checked first, and the
        // error names the column that disagrees.
        let mut target_archetype_ids = Vec::new();
        for (archetype_id, archetype) in &self.archetypes {
            if !archetype.component_types.contains(&component_id) {
                continue;
            }
            let Some(column) = archetype.component_storages.get(component_id) else {
                return Err(WorldError::DescriptorStorageMissing {
                    component_id,
                    archetype_id: *archetype_id,
                });
            };
            if column.element_size() != previous_size {
                return Err(WorldError::ComponentColumnLayoutMismatch {
                    component_id,
                    archetype_id: *archetype_id,
                    expected: previous_size,
                    actual: column.element_size(),
                });
            }
            target_archetype_ids.push(*archetype_id);
        }

        // Step 2: Migrate. Every column here was verified a moment ago and
        // nothing in between reshapes the map, so the lookups cannot miss.
        let mut migrated_row_count = 0;
        for archetype_id in target_archetype_ids {
            let archetype = self
                .archetypes
                .get_mut(&archetype_id)
                .expect("the verification pass listed only present archetypes");
            let column = archetype
                .component_storages
                .get_mut(component_id)
                .expect("the verification pass checked the column is present");
            migrated_row_count += column.relayout_validated(layout.clone(), plan, previous_size);
        }

        // Store the new shape in the one record that describes storage: the
        // factory the chunk accessors and the size readers consult. The columns
        // just reshaped carry the same shape.
        self.storage_factories
            .insert(component_id, StorageFactory::Descriptor(layout));
        Ok(migrated_row_count)
    }

    /// Move every row of one descriptor component into another and retire the
    /// source registration.
    ///
    /// This is how a managed manifest renames a type. A successor registration
    /// derives its id from its own name, so the rows cannot stay where they are
    /// the way a relayout keeps them. Each row is reshaped through `plan` - the
    /// same [`FieldPlan`] a relayout uses, measured from the source's field
    /// layout to the successor's - and added to the successor before it is
    /// removed from the source. An entity whose only component this is
    /// therefore survives, instead of being destroyed when the removal empties
    /// it.
    ///
    /// The source registration is then retired: its name claim, its registry
    /// entry and its storage factory go, and its bit is released when no
    /// archetype with live rows still references it.
    ///
    /// Returns the number of rows moved.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DescriptorRemapSelf`] when both ids are equal,
    /// [`WorldError::DescriptorComponentNotRegistered`] when either id is not a
    /// registered descriptor component,
    /// [`WorldError::DescriptorComponentAlreadyPresent`] when the successor
    /// already carries rows (merging two row sets would make the plan's source
    /// side wrong for half its input), [`WorldError::DescriptorRowInvalid`] when
    /// the plan does not fit the two layouts,
    /// [`WorldError::DescriptorStorageMissing`] when an archetype lists the
    /// source without a column, and
    /// [`WorldError::DescriptorComponentMissing`] when the removal of a moved
    /// row finds the source column gone.
    pub fn remap_descriptor_component(
        &mut self,
        source_component_id: ComponentId,
        successor_component_id: ComponentId,
        plan: &FieldPlan,
    ) -> Result<usize, WorldError> {
        if source_component_id == successor_component_id {
            return Err(WorldError::DescriptorRemapSelf);
        }
        let source_size = match self.storage_factories.get(&source_component_id) {
            Some(StorageFactory::Descriptor(layout)) => layout.size,
            _ => {
                return Err(WorldError::DescriptorComponentNotRegistered {
                    id: source_component_id,
                })
            }
        };
        let successor_size = match self.storage_factories.get(&successor_component_id) {
            Some(StorageFactory::Descriptor(layout)) => layout.size,
            _ => {
                return Err(WorldError::DescriptorComponentNotRegistered {
                    id: successor_component_id,
                })
            }
        };
        plan.validate(source_size, successor_size)?;
        if self.live_row_count(successor_component_id) != 0 {
            return Err(WorldError::DescriptorComponentAlreadyPresent);
        }

        // Step 1: Read and reshape every row before the first mutation, so a
        // row the plan cannot describe fails before anything has moved. The
        // plan was validated against the registered sizes, so the slices in
        // here are in bounds.
        let mut row_moves: Vec<(Entity, Vec<u8>)> = Vec::new();
        for (archetype_id, archetype) in &self.archetypes {
            if !archetype.component_types.contains(&source_component_id) {
                continue;
            }
            let Some(column) = archetype.component_storages.get(source_component_id) else {
                return Err(WorldError::DescriptorStorageMissing {
                    component_id: source_component_id,
                    archetype_id: *archetype_id,
                });
            };
            for (row, entity) in archetype.entities.iter().enumerate() {
                let source_bytes = column.bytes(row).ok_or(WorldError::DescriptorRowInvalid)?;
                let mut successor_bytes = vec![0_u8; successor_size];
                plan.apply(source_bytes, &mut successor_bytes);
                row_moves.push((*entity, successor_bytes));
            }
        }

        // Step 2: Move every row. Add first, remove second: the two columns
        // coexist for a moment and the entity never loses its last component.
        let mut moved_row_count = 0;
        for (entity, successor_bytes) in row_moves {
            self.add_descriptor_component(entity, successor_component_id, &successor_bytes)?;
            self.remove_component_by_id(entity, source_component_id)
                .map_err(|_| WorldError::DescriptorComponentMissing)?;
            moved_row_count += 1;
        }

        // Step 3: The source holds no rows now; retire it so its name can be
        // declared again and its bit returns to the pool. Every archetype that
        // carried it was emptied by Step 2, which is the condition the
        // retirement's bit rule needs.
        self.retire_registration(source_component_id);
        Ok(moved_row_count)
    }

    /// Return a raw descriptor component column for language bindings.
    ///
    /// `chunk_index` counts the archetypes that carry the component, in the
    /// map's iteration order, so it is only stable for the pass it is read in.
    pub fn descriptor_component_chunk_mut(
        &mut self,
        component_id: ComponentId,
        chunk_index: usize,
    ) -> Option<ColumnChunk<'_>> {
        let archetype = self
            .archetypes
            .values_mut()
            .filter(|archetype| archetype.component_types.contains(&component_id))
            .nth(chunk_index)?;
        let archetype_id = archetype.id;
        let column = archetype.component_storages.get_mut(component_id)?;
        let element_size = column.element_size();
        let (data, len, ticks) = column.raw_rows_and_ticks_mut();
        Some(ColumnChunk {
            archetype: archetype_id,
            data,
            len,
            element_size,
            ticks,
        })
    }

    /// Return a raw native component column for language bindings.
    ///
    /// The native twin of [`Self::descriptor_component_chunk_mut`]: the
    /// contiguous row buffer of a native (Rust-registered) component as raw
    /// bytes, so the C# backend can expose components an extension registered
    /// without naming their concrete Rust type.
    ///
    /// The returned pointer is valid only for the active managed-system
    /// invocation and must not be retained beyond it.
    pub fn native_component_chunk_mut(
        &mut self,
        component_id: ComponentId,
        chunk_index: usize,
    ) -> Option<ColumnChunk<'_>> {
        if !component_id.is_native_storage() {
            return None;
        }
        let archetype = self
            .archetypes
            .values_mut()
            .filter(|archetype| archetype.component_types.contains(&component_id))
            .nth(chunk_index)?;
        let archetype_id = archetype.id;
        let column = archetype.component_storages.get_mut(component_id)?;
        let element_size = column.element_size();
        let (data, len, ticks) = column.raw_rows_and_ticks_mut();
        Some(ColumnChunk {
            archetype: archetype_id,
            data,
            len,
            element_size,
            ticks,
        })
    }

    /// Return a raw descriptor component column from one already-known archetype.
    ///
    /// The archetype-scoped twin of [`Self::descriptor_component_chunk_mut`].
    pub fn descriptor_component_chunk_in_archetype(
        &mut self,
        component_id: ComponentId,
        archetype_id: ArchetypeId,
    ) -> Option<ColumnChunk<'_>> {
        let archetype = self.archetypes.get_mut(&archetype_id)?;
        let column = archetype.component_storages.get_mut(component_id)?;
        let element_size = column.element_size();
        let (data, len, ticks) = column.raw_rows_and_ticks_mut();
        Some(ColumnChunk {
            archetype: archetype_id,
            data,
            len,
            element_size,
            ticks,
        })
    }

    /// Return a raw native component column from one already-known archetype.
    ///
    /// The archetype-scoped twin of [`Self::native_component_chunk_mut`].
    pub fn native_component_chunk_in_archetype(
        &mut self,
        component_id: ComponentId,
        archetype_id: ArchetypeId,
    ) -> Option<ColumnChunk<'_>> {
        if !component_id.is_native_storage() {
            return None;
        }
        let archetype = self.archetypes.get_mut(&archetype_id)?;
        if !archetype.component_types.contains(&component_id) {
            return None;
        }
        let column = archetype.component_storages.get_mut(component_id)?;
        let element_size = column.element_size();
        let (data, len, ticks) = column.raw_rows_and_ticks_mut();
        Some(ColumnChunk {
            archetype: archetype_id,
            data,
            len,
            element_size,
            ticks,
        })
    }

    /// Returns the total number of rows every archetype stores for one component
    /// id.
    ///
    /// Zero means nothing uses the component any more, which is what a
    /// superseded hot-reload generation looks like once its entities have
    /// migrated away. A non-zero count means the column is still live, so its
    /// registration cannot be treated as stale and discarded.
    pub fn live_row_count(&self, component_id: ComponentId) -> usize {
        self.archetypes
            .values()
            .filter(|archetype| archetype.component_types.contains(&component_id))
            .map(|archetype| archetype.entities.len())
            .sum()
    }

    /// Whether any archetype still lists this component id.
    ///
    /// The host's cleanup after a failed generation uses this, in debug builds,
    /// to assert that a leftover id is really gone - its rows, columns and
    /// tables - before the image that defined it leaves the graveyard.
    pub fn any_archetype_lists_component(&self, component_id: ComponentId) -> bool {
        self.archetypes
            .values()
            .any(|archetype| archetype.component_types.contains(&component_id))
    }

    /// The first registered component other than `excluding` that claims
    /// `type_name` and still holds rows, with that row count.
    ///
    /// Both collision guards answer one question: is a same-name registration a
    /// superseded generation or a live peer? Live rows settle part of it - a
    /// peer's rows are the ones that would be lost - but not all of it, because
    /// a generation being retired still holds its rows while its replacement
    /// registers. So the host announces the retiring generation's names before
    /// init (see [`Self::supersede_persist_registrations`]), and this lookup
    /// supplies the candidate that announcement is judged against.
    pub(crate) fn live_component_with_name(
        &self,
        type_name: &str,
        excluding: ComponentId,
    ) -> Option<(ComponentId, usize)> {
        self.component_registry
            .registered_components()
            .filter(|(id, _, name)| *name == type_name && *id != excluding)
            .map(|(id, _, _)| (id, self.live_row_count(id)))
            .find(|(_, live_rows)| *live_rows > 0)
    }

    /// Every registered component that claims `type_name`, in registration
    /// order.
    ///
    /// Unlike [`Self::live_component_with_name`] this neither requires live
    /// rows nor excludes an id. A rebuilt DLL usually gets the compiler's id
    /// back for an unchanged type, so a superseding registration's predecessor
    /// is often the entry *with* the incoming id, and older generations can
    /// still be listed under the same name. Every candidate has to be
    /// considered, not just one of them.
    pub(crate) fn component_ids_with_name(&self, type_name: &str) -> Vec<ComponentId> {
        self.component_registry
            .registered_components()
            .filter(|(_, _, name)| *name == type_name)
            .map(|(id, _, _)| id)
            .collect()
    }

    /// Resolve a component id from its registered type name, without the
    /// persistable-only filter.
    ///
    /// The C# backend uses this to map an extension's exposed component name
    /// (e.g. `pill_spline::Spline`) to its native [`ComponentId`], so a
    /// byte-level binding can be created without naming the concrete type.
    ///
    /// Unlike the persistable resolver, this has no `persist_inserters` filter
    /// to shrink the candidate set, so a name claimed by two registrations
    /// arrives here with both still visible. A bit index says nothing about
    /// age: `allocate_bit` hands out bits freed by `remove` before it advances
    /// `next_bit`, so the highest bit can belong to the older registration. An
    /// ambiguous name is therefore reported as
    /// [`WorldError::ComponentNameAmbiguous`], rather than binding managed code
    /// silently to one of the two columns.
    ///
    /// A component declared with a shared identity cannot reach this state:
    /// every DLL that links it computes the same [`ComponentId`], so there is
    /// only ever one registration to find.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::ComponentNameAmbiguous`] when more than one
    /// registration claims the name.
    pub fn resolve_component_id_by_name_any(
        &self,
        type_name: &str,
    ) -> Result<Option<ComponentId>, WorldError> {
        let mut candidates = self
            .component_registry
            .registered_components()
            .filter(|(_, _, name)| *name == type_name)
            .map(|(id, _, _)| id);

        let Some(first) = candidates.next() else {
            return Ok(None);
        };
        let extra_candidate_count = candidates.count();
        if extra_candidate_count > 0 {
            return Err(WorldError::ComponentNameAmbiguous {
                type_name: type_name.to_string(),
                count: extra_candidate_count + 1,
            });
        }
        Ok(Some(first))
    }

    /// Read one runtime-defined component row as raw bytes.
    pub fn descriptor_component_bytes(
        &self,
        entity: Entity,
        component_id: ComponentId,
    ) -> Option<&[u8]> {
        let location = self.entity_locations.get(&entity)?;
        self.archetypes
            .get(&location.archetype_id)?
            .component_storages
            .get(component_id)?
            .bytes(location.index_in_archetype)
    }

    /// Returns an immutable reference to a component on an entity.
    ///
    /// Returns `None` if the entity doesn't exist or doesn't have the component.
    pub fn get_component<T>(&self, entity: Entity) -> Option<&T>
    where
        T: Component,
    {
        let _zone = crate::profile_scope!(
            "get component",
            [(
                "Target entity: {:?}, Component type: {}",
                entity,
                std::any::type_name::<T>()
            )]
        );

        // Step 1: Look up the component's registry bit. The archetype check
        // below then needs one mask test instead of a search.
        let component_id = ComponentId::of::<T>();
        let bit = self.component_registry.get_bit(&component_id)?;

        // Step 2: Find the entity's location and its archetype, and confirm
        // that the archetype's mask lists the component.
        let location = self.entity_locations.get(&entity)?;
        let archetype = self.archetypes.get(&location.archetype_id)?;
        if !archetype.has_component_bit(bit) {
            return None;
        }

        // Step 3: Read the entity's row from the column.
        Some(
            archetype
                .component_storages
                .column_of::<T>()
                .get::<T>(location.index_in_archetype),
        )
    }

    /// Returns a mutable reference to a component on an entity.
    ///
    /// Returns `None` if the entity doesn't exist or doesn't have the component.
    pub fn get_component_mut<T>(&mut self, entity: Entity) -> Option<&mut T>
    where
        T: Component,
    {
        let _zone = crate::profile_scope!(
            "get component mut",
            [(
                "Target entity: {:?}, Component type (mutable): {}",
                entity,
                std::any::type_name::<T>()
            )]
        );

        // Step 1: Look up the component's registry bit. The archetype check
        // below then needs one mask test instead of a search.
        let component_id = ComponentId::of::<T>();
        let bit = self.component_registry.get_bit(&component_id)?;

        // Step 2: Find the entity's location and its archetype, and confirm
        // that the archetype's mask lists the component.
        let location = self.entity_locations.get(&entity)?;
        let archetype_id = location.archetype_id;
        let index = location.index_in_archetype;
        let archetype = self.archetypes.get_mut(&archetype_id)?;
        if !archetype.has_component_bit(bit) {
            return None;
        }

        // Step 3: Take the mutable row.
        Some(
            archetype
                .component_storages
                .column_of_mut::<T>()
                .get_mut::<T>(index),
        )
    }

    /// Returns a raw mutable pointer to a component on an entity.
    ///
    /// `ScriptContext` uses this to avoid aliasing problems when a script
    /// accesses components of its own type. Returning a raw pointer instead of
    /// `&mut T` opts out of Rust's `noalias` assumption.
    ///
    /// Returns `None` if the entity doesn't exist or doesn't have the component.
    pub(crate) fn get_component_ptr_mut<T>(&mut self, entity: Entity) -> Option<*mut T>
    where
        T: Component,
    {
        // Step 1: Look up the component's registry bit. The archetype check
        // below then needs one mask test instead of a search.
        let component_id = ComponentId::of::<T>();
        let bit = self.component_registry.get_bit(&component_id)?;

        // Step 2: Find the entity's location and its archetype, and confirm
        // that the archetype's mask lists the component.
        let location = self.entity_locations.get(&entity)?;
        let archetype_id = location.archetype_id;
        let index = location.index_in_archetype;
        let archetype = self.archetypes.get_mut(&archetype_id)?;
        if !archetype.has_component_bit(bit) {
            return None;
        }

        // Step 3: Take the raw pointer to the row. Going through `get_mut` and
        // casting avoids handing out a `&mut` that could alias.
        let storage = archetype.component_storages.column_of_mut::<T>();
        Some(storage.get_mut::<T>(index) as *mut T)
    }

    /// Remove a component from an entity, moving it to a new archetype.
    ///
    /// If the entity's last component is removed, the entity is destroyed
    /// instead of migrated to an empty archetype.
    ///
    /// # Errors
    ///
    /// Returns [`RemoveComponentError::EntityNotFound`] if the entity does
    /// not exist, and [`RemoveComponentError::ComponentNotFound`] if the
    /// entity does not carry the component `T`.
    pub fn remove_component<T: Component>(
        &mut self,
        entity: Entity,
    ) -> Result<(), RemoveComponentError> {
        self.remove_component_by_id(entity, ComponentId::of::<T>())
    }

    /// Add a component the world addresses by [`ComponentId`], migrating the
    /// entity to the archetype that gains it.
    ///
    /// The one implementation behind [`Self::add_component`],
    /// [`Self::add_descriptor_component`] and the command queue's deferred add,
    /// so a rule added here (the duplicate refusal, the registration refusal)
    /// holds on every path that adds a component.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::EntityNotFound`] when the entity has no location,
    /// [`WorldError::ArchetypeMissing`] when its recorded archetype is gone,
    /// [`WorldError::DescriptorComponentAlreadyPresent`] when it already
    /// carries the component, [`WorldError::DescriptorComponentNotRegistered`]
    /// when this world never registered the id, and whatever the migration or
    /// `attach` reports.
    pub(crate) fn try_add_component_by_id<F>(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
        attach: F,
    ) -> Result<(), WorldError>
    where
        F: FnOnce(&mut ComponentColumns) -> Result<(), WorldError>,
    {
        // Step 1: Locate the entity and reject a stale archetype reference.
        let location = *self
            .entity_locations
            .get(&entity)
            .ok_or(WorldError::EntityNotFound)?;
        let Some(old_archetype) = self.archetypes.get(&location.archetype_id) else {
            return Err(WorldError::ArchetypeMissing {
                entity,
                archetype_id: location.archetype_id,
            });
        };

        // Step 2: Refuse one the entity already carries.
        if old_archetype.component_types.contains(&component_id) {
            return Err(WorldError::DescriptorComponentAlreadyPresent);
        }

        // Step 3: Refuse a type this world never registered. Registration is
        // what allocates the archetype bit, so an unregistered type would
        // resolve the destination back to the entity's current archetype -
        // storing nothing while reporting success.
        if !self.storage_factories.contains_key(&component_id) {
            return Err(WorldError::DescriptorComponentNotRegistered { id: component_id });
        }

        // Step 4: Build the destination component set with the id appended.
        let mut new_component_ids = Vec::with_capacity(old_archetype.component_types.len() + 1);
        new_component_ids.extend_from_slice(&old_archetype.component_types);
        new_component_ids.push(component_id);
        new_component_ids.sort();

        // Step 5: Migrate the entity. Existing components are carried by the
        // move; the only thing left to write is the component being added.
        self.move_entity_to_archetype(entity, new_component_ids, attach)
    }

    /// Remove a component the world addresses by [`ComponentId`], migrating the
    /// entity or destroying it when nothing else remains.
    ///
    /// The one implementation behind [`Self::remove_component_by_id`] and the
    /// command queue's deferred remove.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::EntityNotFound`] when the entity has no location,
    /// [`WorldError::ArchetypeMissing`] when its recorded archetype is gone,
    /// [`WorldError::DescriptorComponentMissing`] when it does not carry the
    /// component, and whatever the migration reports.
    pub(crate) fn try_remove_component_by_id(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
    ) -> Result<(), WorldError> {
        // Step 1: Locate the entity and reject a stale archetype reference.
        let location = *self
            .entity_locations
            .get(&entity)
            .ok_or(WorldError::EntityNotFound)?;
        let Some(old_archetype) = self.archetypes.get(&location.archetype_id) else {
            return Err(WorldError::ArchetypeMissing {
                entity,
                archetype_id: location.archetype_id,
            });
        };

        // Step 2: Refuse a component the entity does not carry, and build the
        // surviving set.
        if !old_archetype.component_types.contains(&component_id) {
            return Err(WorldError::DescriptorComponentMissing);
        }
        let new_component_ids: Vec<ComponentId> = old_archetype
            .component_types
            .iter()
            .filter(|&id| *id != component_id)
            .cloned()
            .collect();

        // Step 3: An entity with no remaining components is destroyed instead
        // of migrated to an empty archetype. It may already be gone, which is
        // not an error here.
        if new_component_ids.is_empty() {
            let _ = self.destroy_entity(entity);
            return Ok(());
        }

        // Step 4: Migrate the surviving components. The move carries them; the
        // removed one is left behind in the source archetype and released
        // there.
        self.move_entity_to_archetype(entity, new_component_ids, |_| Ok(()))
    }

    /// Remove a component identified by [`ComponentId`] from one entity.
    ///
    /// This is the non-generic core of [`Self::remove_component`]; the host
    /// uses it when dropping data for a type it can no longer name statically
    /// (a component a reloaded module stopped registering).
    ///
    /// # Panics
    ///
    /// Panics when the migration itself fails: the entity and its archetype
    /// are validated before it starts, so a failure there is an
    /// internal-invariant break, not a caller error.
    pub(crate) fn remove_component_by_id(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
    ) -> Result<(), RemoveComponentError> {
        let _zone =
            crate::profile_scope!("remove component", [("Entity being mutated: {:?}", entity)]);
        self.try_remove_component_by_id(entity, component_id)
            .map_err(|error| match error {
                WorldError::EntityNotFound | WorldError::ArchetypeMissing { .. } => {
                    RemoveComponentError::EntityNotFound
                }
                WorldError::DescriptorComponentMissing => RemoveComponentError::ComponentNotFound,
                // The entity and its archetype were validated before the
                // migration started, so a failure here is an
                // internal-invariant break. The descriptor paths propagate the
                // same failure as a typed error instead, because their inputs
                // come from outside the engine.
                other => {
                    panic!("internal invariant broken while migrating entity {entity:?}: {other}")
                }
            })
    }

    /// Add a component to an existing entity, moving it to a new archetype.
    ///
    /// Existing components are preserved during the migration.
    ///
    /// # Errors
    ///
    /// Returns [`AddComponentError::EntityNotFound`] if the entity does not
    /// exist, [`AddComponentError::ComponentAlreadyExists`] if the entity
    /// already carries the component `T`, and
    /// [`AddComponentError::ComponentNotRegistered`] if `T` was never
    /// registered with this world.
    ///
    /// # Panics
    ///
    /// Panics when the migration itself fails, which the checks above make an
    /// internal-invariant break rather than a caller error.
    pub fn add_component<T>(
        &mut self,
        entity: Entity,
        component: T,
    ) -> Result<(), AddComponentError>
    where
        T: Component,
    {
        let _zone = crate::profile_scope!(
            "add component",
            [(
                "Target entity: {:?}, Component type being added: {}",
                entity,
                std::any::type_name::<T>()
            )]
        );
        self.try_add_component_by_id(entity, ComponentId::of::<T>(), |new_storage| {
            new_storage.column_of_mut::<T>().push::<T>(component);
            Ok(())
        })
        .map_err(|error| match error {
            WorldError::EntityNotFound | WorldError::ArchetypeMissing { .. } => {
                AddComponentError::EntityNotFound
            }
            WorldError::DescriptorComponentAlreadyPresent => {
                AddComponentError::ComponentAlreadyExists
            }
            WorldError::DescriptorComponentNotRegistered { id } => {
                AddComponentError::ComponentNotRegistered { id }
            }
            // The entity and its archetype were validated before the migration
            // started, so a failure here is an internal-invariant break.
            other => panic!("internal invariant broken while migrating entity {entity:?}: {other}"),
        })
    }

    /// Add a runtime-defined component and migrate the entity's other columns.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::EntityNotFound`] if the entity does not exist,
    /// [`WorldError::DescriptorComponentAlreadyPresent`] if the entity already
    /// carries the component, [`WorldError::DescriptorComponentNotRegistered`]
    /// if the component was never registered, and
    /// [`WorldError::DescriptorByteLengthMismatch`] if `bytes` does not match
    /// the registered layout size.
    pub(crate) fn add_descriptor_component(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
        bytes: &[u8],
    ) -> Result<(), WorldError> {
        // The byte-length refusal is about the payload, not the component set,
        // so it is checked here; the set-level checks live in the shared core.
        let expected_size = match self.storage_factories.get(&component_id) {
            Some(StorageFactory::Descriptor(layout)) => layout.size,
            _ => return Err(WorldError::DescriptorComponentNotRegistered { id: component_id }),
        };
        if bytes.len() != expected_size {
            return Err(WorldError::DescriptorByteLengthMismatch { id: component_id });
        }
        // The migration carries every existing column; the arriving component
        // is the one column the destination has and the source does not, so it
        // is written by the attach step rather than patched in afterwards.
        self.try_add_component_by_id(entity, component_id, |new_storage| {
            // The destination archetype was built from the id set, which names
            // this component, so the column is there; the refusal covers a
            // registry/storage desync rather than a reachable state.
            let Some(column) = new_storage.get_mut(component_id) else {
                return Err(WorldError::DescriptorComponentMissing);
            };
            column.push_bytes(bytes)
        })
    }

    /// Replace the bytes of an existing runtime-defined component row.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::EntityNotFound`] when the entity has no location,
    /// [`WorldError::DescriptorComponentMissing`] when its archetype has no
    /// column for the component, [`WorldError::ColumnRowsAreNotPlainData`] when
    /// the column holds values that own resources, and
    /// [`WorldError::DescriptorRowInvalid`] when `bytes` is not exactly one row.
    pub(crate) fn set_descriptor_component_bytes(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
        bytes: &[u8],
    ) -> Result<(), WorldError> {
        let location = *self
            .entity_locations
            .get(&entity)
            .ok_or(WorldError::EntityNotFound)?;
        self.archetypes
            .get_mut(&location.archetype_id)
            .and_then(|archetype| archetype.component_storages.get_mut(component_id))
            .ok_or(WorldError::DescriptorComponentMissing)?
            .set_bytes(location.index_in_archetype, bytes)
    }

    /// Add a zero-initialized runtime-defined component.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DescriptorComponentNotRegistered`] if the component
    /// was never registered, plus any error reported by
    /// [`add_descriptor_component`](Self::add_descriptor_component).
    pub fn add_descriptor_component_default(
        &mut self,
        entity: Entity,
        component_id: ComponentId,
    ) -> Result<(), WorldError> {
        let size = match self.storage_factories.get(&component_id) {
            Some(StorageFactory::Descriptor(layout)) => layout.size,
            _ => return Err(WorldError::DescriptorComponentNotRegistered { id: component_id }),
        };
        self.add_descriptor_component(entity, component_id, &vec![0; size])
    }

    /// Read-only view of the component registry.
    ///
    /// Exposed for the same reason as [`World::archetypes_iter`]: resolving a
    /// component column by stable type name and size rather than by a `TypeId`,
    /// which differs between the host and a hot-loaded DLL.
    #[inline]
    pub fn component_registry(&self) -> &ComponentRegistry {
        &self.component_registry
    }
}
