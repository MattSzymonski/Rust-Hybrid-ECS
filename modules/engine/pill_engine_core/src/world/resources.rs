//! Resource storage: registration, claims, foreign values and re-homing.
//!
//! # Responsibilities
//!
//! - Stores one value per resource type, native or foreign, and hands it out
//!   through `Res<T>` and `ResMut<T>`.
//! - Decides which type owns a shared name when two binaries both declare it,
//!   so the second one reads the same value instead of creating its own.
//! - Re-homes a live resource's function table across a reload and retires the
//!   claims a retiring generation leaves behind.
//! - Declares the asset types a generation owns, and prunes the import registry
//!   once a retired generation's registrations are dead.
//!
//! # Design
//!
//! A resource is a singleton, so it has no archetype and no row: the store is a
//! map from id to a box that holds the value behind a small table of function
//! pointers (drop, size, alignment), not a trait object's vtable. Component
//! columns work the same way and for the same reason: the table can be
//! re-pointed at the newest generation, so the destructor that runs always
//! belongs to code that is still mapped.
//!
//! A foreign resource is the same store with no Rust type behind it: plain
//! bytes plus a declared layout. That is what lets managed code own a resource
//! the host has never heard of.

// External crates
use trait_type_map::TraitAccessible;

// Current crate
use super::*;
use crate::asset::{Asset, AssetManager};

// =============================================================================
// World - Resource Management
// =============================================================================

impl World {
    /// Number of resources the world holds.
    pub fn resource_count(&self) -> usize {
        self.resources.len()
    }

    /// Inserts a resource: a value that exists once and is not attached to any
    /// entity.
    ///
    /// Replaces an existing value of the same type. If another type has already
    /// claimed this resource's shared name, the claim is refused instead: the
    /// error is recorded for the caller to drain, and **nothing is stored**.
    /// Storing it anyway would leave a live resource with a shape no reader
    /// accepts, or - when the two layouts happen to match - let one type read
    /// another type's bytes.
    pub fn insert_resource<T: Resource>(&mut self, resource: T) {
        let _zone = crate::profile_scope!(
            "insert resource",
            [(
                "Resource type being inserted: {}",
                std::any::type_name::<T>()
            )]
        );
        let id = ResourceId::of::<T>();
        if !self.claim_shared_resource_name::<T>() {
            return;
        }
        let tick = Tick::new(self.change_tick);
        self.note_resource_registration(id);
        self.resources.insert(id, ErasedResource::new(resource));
        self.resource_ticks.insert(id, ComponentTicks::new(tick));
        // Record the table too, so a later reload can re-home this value even
        // if the next generation never inserts it again.
        self.resource_factories
            .insert(id, ErasedResourceOps::of::<T>());
    }

    /// Returns a shared reference to the resource, or `None` when it is not
    /// stored.
    pub fn get_resource<T: Resource>(&self) -> Option<&T> {
        let _zone = crate::profile_scope!(
            "get resource",
            [(
                "Resource type being accessed (immutable): {}",
                std::any::type_name::<T>()
            )]
        );
        self.resources
            .get(&ResourceId::of::<T>())
            .and_then(ErasedResource::get::<T>)
    }

    /// Returns a mutable reference to the resource, or `None` when it is not
    /// stored.
    ///
    /// Prefer [`get_resource_mut_tracked`] for system parameters: it also marks
    /// the resource changed when the value is mutated.
    pub fn get_resource_mut<T: Resource>(&mut self) -> Option<&mut T> {
        self.resources
            .get_mut(&ResourceId::of::<T>())
            .and_then(ErasedResource::get_mut::<T>)
    }

    /// Returns the resource mutably, with change tracking.
    ///
    /// The result is a [`Mut<'_, T>`] wrapping the value and its
    /// [`ComponentTicks`]. Mutating through `DerefMut` sets `ticks.changed` to
    /// the current world tick, which is how [`ResMut`](crate::query::ResMut)
    /// reports a change.
    ///
    /// # Panics
    ///
    /// Panics when the resource has no change-tick entry. `insert_resource`
    /// writes the value and its ticks together, so that state means the two
    /// maps have gone out of sync.
    pub fn get_resource_mut_tracked<T: Resource>(&mut self) -> Option<Mut<'_, T>> {
        let _zone = crate::profile_scope!(
            "get resource mut tracked",
            [(
                "Resource type being accessed (mutable, tracked): {}",
                std::any::type_name::<T>()
            )]
        );
        let id = ResourceId::of::<T>();
        let value: &mut T = self
            .resources
            .get_mut(&id)
            .and_then(ErasedResource::get_mut::<T>)?;
        let ticks: &mut ComponentTicks = self.resource_ticks.get_mut(&id).unwrap_or_else(|| {
            // `insert_resource` writes `resources` and `resource_ticks`
            // together, so a resource in one map and not the other means the
            // two have gone out of sync. The panic names the resource so the
            // reader can find it.
            panic!(
                "resource {id:?} exists in resources but has no change-tick \
                     column; resource_ticks has fallen out of sync with resources"
            )
        });
        let this_run = Tick::new(self.change_tick);
        Some(Mut::new(value, ticks, this_run))
    }

    /// Declares that this artifact owns the resource type `T`, without
    /// inserting a value.
    ///
    /// The resource equivalent of [`Self::register_component`]. A resource's
    /// stored drop function points into the artifact that inserted the value.
    /// A reloaded generation that keeps the existing value never calls
    /// `insert_resource`, so without this call nothing would contribute a table
    /// pointing at code that is still mapped.
    ///
    /// Call it from a module's `init` for every resource type the module owns.
    /// It is idempotent, so a reload that re-runs `init` can call it again. A
    /// refused claim records its error and leaves the stored table alone, so a
    /// generation that disagrees about a name's owner or shape cannot re-point
    /// the live resource at its own code before its init fails.
    pub fn register_resource<T: Resource>(&mut self) {
        self.register_resource_claiming::<T>();
    }

    /// [`Self::register_resource`], reporting whether the claim was accepted.
    ///
    /// The public call drops the refusal because its callers cannot act on one:
    /// the error is recorded, and the `init` that raised it fails. The
    /// persistable registration does care, because a refused generation must not
    /// install a serializer that reads the live value through code that is
    /// about to be discarded.
    pub(crate) fn register_resource_claiming<T: Resource>(&mut self) -> bool {
        let id = ResourceId::of::<T>();
        if !self.claim_shared_resource_name::<T>() {
            return false;
        }
        self.resource_factories
            .insert(id, ErasedResourceOps::of::<T>());
        self.note_resource_registration(id);
        true
    }

    /// Declares a resource defined by another language, without storing a
    /// value.
    ///
    /// The foreign-language counterpart of [`Self::register_resource`], for a
    /// type this process has no Rust definition of. The id is a hash of the
    /// declared name, the same value [`Resource::shared_name`] produces for a
    /// Rust type, so a managed declaration and a Rust type that write the same
    /// string share one resource. The scheduler sees them as one too, because
    /// it tracks resources by id.
    ///
    /// The payload is blittable bytes (plain bytes that can be copied as they
    /// are), described by `size` and `align` and stored by
    /// [`Self::insert_foreign_resource_bytes`]. Nothing here runs a destructor,
    /// so a foreign value outlives the assembly that produced it.
    ///
    /// `declaring_type` is the declaring language's own name for the type (the
    /// managed full name, for the C# path), recorded and compared only against
    /// another Rust declaration (see [`Self::claim_shared_resource`]).
    ///
    /// # Errors
    ///
    /// [`WorldError::ForeignResourceNameEmpty`] for an empty name,
    /// [`WorldError::ForeignResourceLayoutInvalid`] for a layout that cannot
    /// describe an allocation, [`WorldError::SharedResourceNameConflict`] when
    /// two Rust types disagree about the name,
    /// [`WorldError::SharedResourceIdentityCollision`] when two different names
    /// hash to one identity, [`WorldError::SharedResourceLayoutMismatch`] when
    /// the declared layout disagrees with the one already claimed,
    /// [`WorldError::SharedResourceSchemaMismatch`] when both declarations
    /// carry a field-shape hash and the two hashes differ, and
    /// [`WorldError::ForeignResourceHoldsRustValue`] when a Rust value is
    /// stored under the id.
    pub fn register_foreign_resource(
        &mut self,
        name: &str,
        declaring_type: &str,
        size: usize,
        align: usize,
        schema_hash: u64,
    ) -> Result<ResourceId, WorldError> {
        if name.is_empty() {
            return Err(WorldError::ForeignResourceNameEmpty);
        }
        if !is_valid_foreign_layout(size, align) {
            return Err(WorldError::ForeignResourceLayoutInvalid { size, align });
        }
        let id = ResourceId::Shared(crate::component::shared_component_identity(name));
        if let Some(error) = self.claim_shared_resource(
            id,
            name,
            declaring_type,
            size,
            align,
            true,
            Some(schema_hash),
        ) {
            return Err(error);
        }
        // A stored Rust value fixes what is under the id - its destructor is
        // bound to its type - so a foreign declaration cannot take the id over.
        // `relayout_foreign_resource` refuses the same state; refusing here too
        // means the disagreement never reaches `rehome_resources`.
        if self
            .resources
            .get(&id)
            .is_some_and(|value| !value.is_foreign())
        {
            return Err(WorldError::ForeignResourceHoldsRustValue { id });
        }
        self.resource_factories
            .insert(id, ErasedResourceOps::foreign(size, align, schema_hash));
        self.note_resource_registration(id);
        Ok(id)
    }

    /// Stores a value for a shared resource, from another language's bytes.
    ///
    /// Replaces a foreign payload under the id and sets the change tick, because
    /// a value appearing counts as a change. A Rust value is never replaced:
    /// its destructor is bound to its type, so accepting foreign bytes would drop
    /// it through code that never allocated it. That refusal is
    /// [`WorldError::ForeignResourceFactoryIsNative`].
    ///
    /// # Errors
    ///
    /// [`WorldError::SharedResourceNotRegistered`] when the id is not a
    /// registered shared resource, [`WorldError::ForeignResourceFactoryIsNative`]
    /// when the id's table is a Rust type's, and
    /// [`WorldError::ForeignResourceBytesMismatch`] when the payload is not
    /// exactly the registered size.
    pub fn insert_foreign_resource_bytes(
        &mut self,
        id: ResourceId,
        bytes: &[u8],
    ) -> Result<(), WorldError> {
        let Some(resource_ops) = self.shared_resource_ops(id) else {
            return Err(WorldError::SharedResourceNotRegistered { id });
        };
        if !resource_ops.foreign {
            return Err(WorldError::ForeignResourceFactoryIsNative { id });
        }
        if bytes.len() != resource_ops.size {
            return Err(WorldError::ForeignResourceBytesMismatch {
                id,
                expected: resource_ops.size,
                actual: bytes.len(),
            });
        }
        let tick = Tick::new(self.change_tick);
        self.resources.insert(
            id,
            ErasedResource::new_foreign(
                bytes,
                resource_ops.align,
                resource_ops.schema_hash.unwrap_or(0),
            ),
        );
        self.resource_ticks.insert(id, ComponentTicks::new(tick));
        self.note_resource_registration(id);
        Ok(())
    }

    /// The stored bytes of a shared resource, for another language to read.
    ///
    /// `None` when the id is not a registered shared resource, when nothing is
    /// stored under it yet, or when the stored value is a Rust one. Bytes are
    /// only handed out for foreign payloads, where the declared layout is all
    /// the meaning a value has.
    pub fn foreign_resource_bytes(&self, id: ResourceId) -> Option<&[u8]> {
        self.foreign_resource_ops(id)?;
        self.resources
            .get(&id)
            .filter(|resource| resource.is_foreign())
            .map(ErasedResource::bytes)
    }

    /// The stored bytes of a shared resource, mutably, with its ticks.
    ///
    /// The `changed` tick is set when the bytes are handed out, not on the first
    /// write: a caller that receives raw bytes has no wrapper that could notice
    /// a write, so the resource counts as changed from the moment of the borrow.
    ///
    /// A caller writing here is responsible for writing a value the stored type
    /// accepts, exactly as a generated mirror is for the component rows it
    /// writes in place.
    ///
    /// A Rust value refuses, as it does for [`Self::foreign_resource_bytes`],
    /// and the refusal costs nothing: the `changed` tick is not set for a view
    /// that is never handed out.
    pub fn foreign_resource_bytes_mut(
        &mut self,
        id: ResourceId,
    ) -> Option<(&mut [u8], &mut ComponentTicks)> {
        self.foreign_resource_ops(id)?;
        if !self.resources.get(&id)?.is_foreign() {
            return None;
        }
        let changed = Tick::new(self.change_tick);
        let ticks = self.resource_ticks.get_mut(&id)?;
        ticks.set_changed(changed);
        let bytes = self.resources.get_mut(&id)?.bytes_mut();
        Some((bytes, ticks))
    }

    /// The address of a Rust value stored under `id`, for a mirrored call that
    /// passes the resource as `&T` or `&mut T`.
    ///
    /// The counterpart of [`Self::foreign_resource_bytes_mut`] for Rust values:
    /// the caller does not get bytes to interpret. It gets the address of the
    /// value, which a generated forwarding function casts back to the type it
    /// was compiled against. A `write` borrow sets the `changed` tick when it is
    /// handed out, for the same reason the foreign view does.
    ///
    /// `None` when nothing is stored under `id`, or when the stored value is a
    /// foreign payload - there is no Rust type there to cast to.
    pub fn native_resource_pointer(&mut self, id: ResourceId, write: bool) -> Option<*mut u8> {
        if self.resources.get(&id)?.is_foreign() {
            return None;
        }
        if write {
            let changed = Tick::new(self.change_tick);
            if let Some(ticks) = self.resource_ticks.get_mut(&id) {
                ticks.set_changed(changed);
            }
        }
        Some(self.resources.get_mut(&id)?.bytes_mut().as_mut_ptr())
    }

    /// The declared layout of a foreign resource: size, alignment, schema
    /// hash.
    ///
    /// `None` when `id` is not a registered foreign declaration.
    /// [`Self::remap_foreign_resource`] reads it to find the payload sizes its
    /// plan converts between.
    pub fn foreign_resource_layout(&self, id: ResourceId) -> Option<(usize, usize, u64)> {
        let resource_ops = self.resource_factories.get(&id).copied()?;
        if !resource_ops.foreign {
            return None;
        }
        Some((
            resource_ops.size,
            resource_ops.align,
            resource_ops.schema_hash?,
        ))
    }

    /// Replaces a foreign resource's declared layout, migrating its value.
    ///
    /// The resource counterpart of [`Self::relayout_descriptor_component`], and
    /// a much smaller one: a resource holds a single value, so there are no
    /// archetypes and no rows to reshape. `plan` comes from the same two field
    /// lists the component path uses ([`FieldPlan::between`]), and any bytes it
    /// does not cover are left zero.
    ///
    /// Only a foreign payload is rewritten. A Rust value's shape *is* its type,
    /// so a declaration that disagrees with it is a conflict to report, not a
    /// migration to perform.
    ///
    /// Returns 1 when a value was rewritten and 0 when only the declaration
    /// moved, which is the case for a resource nobody has stored yet.
    ///
    /// # Errors
    ///
    /// [`WorldError::ForeignResourceNotRegistered`] when the id is not a
    /// registered foreign declaration,
    /// [`WorldError::ForeignResourceLayoutInvalid`] for a layout that cannot
    /// describe an allocation,
    /// [`WorldError::ForeignResourcePlanOutOfBounds`] when the plan does not fit
    /// the old or the new payload, and
    /// [`WorldError::ForeignResourceHoldsRustValue`] when a Rust value is stored
    /// under the id.
    pub fn relayout_foreign_resource(
        &mut self,
        id: ResourceId,
        size: usize,
        align: usize,
        schema_hash: u64,
        plan: &FieldPlan,
    ) -> Result<usize, WorldError> {
        // Step 1: Check the new declaration against the id's recorded table.
        if !self
            .resource_factories
            .get(&id)
            .is_some_and(|resource_ops| resource_ops.foreign)
        {
            return Err(WorldError::ForeignResourceNotRegistered { id });
        }
        if !is_valid_foreign_layout(size, align) {
            return Err(WorldError::ForeignResourceLayoutInvalid { size, align });
        }
        if self
            .resources
            .get(&id)
            .is_some_and(|value| !value.is_foreign())
        {
            return Err(WorldError::ForeignResourceHoldsRustValue { id });
        }

        // Step 2: Move the recorded claim to the new shape.
        let next_resource_ops = ErasedResourceOps::foreign(size, align, schema_hash);
        // The claim records the shape a new declaration is checked against, so
        // it moves with the table on both paths. If it stayed behind, the
        // migrated shape could not be declared again, and a stale declaration
        // could put the old table back.
        if let Some(claim) = self.shared_resource_claims.get_mut(&id) {
            claim.size = size;
            claim.align = align;
            claim.schema_hash = Some(schema_hash);
        }
        // Step 3: Reshape a stored payload, or record the new table alone when
        // no value is stored yet.
        let Some(value) = self.resources.get_mut(&id) else {
            self.resource_factories.insert(id, next_resource_ops);
            self.resource_field_layouts.remove(&id);
            return Ok(0);
        };
        value
            .migrate_bytes(size, align, plan)
            .map_err(|_error| WorldError::ForeignResourcePlanOutOfBounds { id })?;
        self.resource_factories.insert(id, next_resource_ops);
        // The stored field layout described the shape that just changed, so it
        // is removed instead of being read against the migrated bytes. The
        // declarer registers it again, exactly as it does for a component.
        self.resource_field_layouts.remove(&id);
        Ok(1)
    }

    /// Moves a foreign resource's value onto another id and retires the source
    /// declaration.
    ///
    /// The resource counterpart of [`Self::remap_descriptor_component`], and
    /// the rename step of a managed resource manifest: a renamed type declares
    /// a new name, and with it a new id, so the stored value is reshaped through
    /// `plan` (built from the source's field list and the successor's), written
    /// under the successor, and the source is then dropped. A payload the
    /// successor already holds is replaced; in the common case the successor is
    /// freshly declared and holds an all-zero seed.
    ///
    /// The source's claim count moves to the successor, so the value is released
    /// only when no other subject still declares the old name, and the successor
    /// ends up protected exactly as the source was.
    ///
    /// Returns 1 when a value was moved and 0 when the source had none stored,
    /// which is the case for a resource nobody has written yet.
    ///
    /// # Errors
    ///
    /// [`WorldError::ForeignResourceRemapSelf`] when both ids are equal,
    /// [`WorldError::ForeignResourceNotRegistered`] when either id is not a
    /// registered foreign declaration,
    /// [`WorldError::ForeignResourcePlanOutOfBounds`] when the plan does not
    /// fit the source or the successor payload, and
    /// [`WorldError::ForeignResourceHoldsRustValue`] when a Rust value is
    /// stored under either id.
    pub fn remap_foreign_resource(
        &mut self,
        old_id: ResourceId,
        new_id: ResourceId,
        plan: &FieldPlan,
    ) -> Result<usize, WorldError> {
        if old_id == new_id {
            return Err(WorldError::ForeignResourceRemapSelf);
        }
        let Some((old_size, _, _)) = self.foreign_resource_layout(old_id) else {
            return Err(WorldError::ForeignResourceNotRegistered { id: old_id });
        };
        let Some((new_size, _, _)) = self.foreign_resource_layout(new_id) else {
            return Err(WorldError::ForeignResourceNotRegistered { id: new_id });
        };
        if self
            .resources
            .get(&old_id)
            .is_some_and(|value| !value.is_foreign())
            || self
                .resources
                .get(&new_id)
                .is_some_and(|value| !value.is_foreign())
        {
            return Err(WorldError::ForeignResourceHoldsRustValue { id: old_id });
        }

        // Step 1: Read and reshape the stored value before anything moves.
        // `FieldPlan::validate` reports its own error type (it is written for
        // component rows), so the refusal is translated into the resource's
        // variant here.
        let mut moved_value_count = 0;
        if let Some(old_bytes) = self.foreign_resource_bytes(old_id).map(<[u8]>::to_vec) {
            plan.validate(old_size, new_size)
                .map_err(|_| WorldError::ForeignResourcePlanOutOfBounds { id: old_id })?;
            let mut new_bytes = vec![0_u8; new_size];
            plan.apply(&old_bytes, &mut new_bytes);
            // Step 2: Write it under the successor. The insert stamps the
            // change tick, so a reader observes the move as the change it is.
            self.insert_foreign_resource_bytes(new_id, &new_bytes)?;
            moved_value_count = 1;
        }

        // Step 3: Move the claims and drop the source. The count moves in one
        // step rather than being released one claim at a time: whoever declared
        // the old name still needs the resource under its new one.
        let claims = self.resource_claim_counts.remove(&old_id).unwrap_or(0);
        if claims > 0 {
            *self.resource_claim_counts.entry(new_id).or_insert(0) += claims;
        }
        self.drop_resources(&[old_id]);
        Ok(moved_value_count)
    }

    /// The per-type table of a registered shared resource, if the id names one.
    ///
    /// Only a shared id has a table another language can reach: a private
    /// resource is identified by its `TypeId`, which no other language can name,
    /// so it also has no byte view to hand out. The `?` on
    /// [`ResourceId::shared_identity`] is what returns `None` for a private id.
    fn shared_resource_ops(&self, id: ResourceId) -> Option<ErasedResourceOps> {
        id.shared_identity()?;
        self.resource_factories.get(&id).copied()
    }

    /// The per-type table of a registered shared resource, when that table
    /// belongs to foreign bytes.
    ///
    /// The byte accessors go through this one. `shared_resource_ops` only says
    /// whether the id is shared at all, which is all the foreign insert needs to
    /// tell a private id from one whose table belongs to a Rust type. A byte
    /// view needs more: it is only meaningful for a payload with no Rust type
    /// anywhere, so the table must say foreign here, and the stored box is
    /// checked as well before any bytes are handed out.
    fn foreign_resource_ops(&self, id: ResourceId) -> Option<ErasedResourceOps> {
        self.shared_resource_ops(id)
            .filter(|resource_ops| resource_ops.foreign)
    }

    /// Stamps one resource registration, keeping one entry per id.
    ///
    /// The stamp is the id's most recent registration. After an artifact's
    /// `init`, the host compares [`Self::resource_ids_registered_since`] with
    /// the list the previous generation produced, to learn which resources the
    /// new generation still claims. A generation keeps a claim by registering
    /// the type again, through `insert_resource` or `register_resource`. One
    /// entry per id keeps the map as small as the number of distinct resources,
    /// and the comparison reports each id once.
    fn note_resource_registration(&mut self, id: ResourceId) {
        self.resource_registration_stamps
            .insert(id, self.resource_registration_sequence);
        self.resource_registration_sequence = self.resource_registration_sequence.wrapping_add(1);
    }

    /// Sequence marker to capture before an artifact's `init`, so the resource
    /// types that `init` registers can be enumerated afterwards.
    ///
    /// Mirrors [`Self::component_registration_sequence`].
    pub fn resource_registration_sequence(&self) -> u64 {
        self.resource_registration_sequence
    }

    /// Resource ids registered at or after `sequence`.
    ///
    /// The comparison is `>=`: a registration reads the current value and then
    /// increments it, so one happening immediately after the capture carries
    /// exactly the captured value.
    pub fn resource_ids_registered_since(&self, sequence: u64) -> Vec<ResourceId> {
        let mut ids: Vec<ResourceId> = self
            .resource_registration_stamps
            .iter()
            .filter(|(_, stamped)| **stamped >= sequence)
            .map(|(id, _)| *id)
            .collect();
        ids.sort_unstable();
        ids
    }

    /// Names declared by the shared resources this world holds a claim for,
    /// sorted and deduplicated.
    ///
    /// The resource counterpart of the component registry's shared-identity
    /// list, and what the ECS report prints. A shared resource's whole point is
    /// that no `TypeId` names it, so the declared name is all there is to
    /// report.
    #[must_use]
    pub fn shared_resource_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .shared_resource_claims
            .values()
            .map(|claim| claim.shared_name.clone())
            .collect();
        names.sort_unstable();
        names.dedup();
        names
    }

    /// Checks and records which type owns a shared resource name.
    ///
    /// Returns whether the caller may go on to register. An ordinary resource
    /// always may: its id *is* its `TypeId`, so a second claim on one id comes
    /// from the same type. A shared claim may when it is accepted, and may not
    /// when it was refused; a refusal also records its error.
    ///
    /// A refusal must stop the caller before it touches stored state, not only
    /// fail the init afterwards. A failed reload rolls the generation back by
    /// re-running the previous `init`, which re-registers the types it owned
    /// rather than restoring values. A value written under a refused claim would
    /// therefore outlive the generation that wrote it, in a shape nothing can
    /// read.
    ///
    /// A shared resource's id comes from a name written down in code, so two
    /// unrelated types can reach the same id. The last segment of the declaring
    /// type's path tells them apart: two copies of one type agree on it, two
    /// different types do not. `ComponentRegistry` applies the same rule to
    /// shared components.
    fn claim_shared_resource_name<T: Resource>(&mut self) -> bool {
        let Some(shared_name) = T::shared_name() else {
            return true;
        };
        match self.claim_shared_resource(
            ResourceId::of::<T>(),
            shared_name,
            crate::component::ComponentRegistry::declaring_type_name::<T>(),
            std::mem::size_of::<T>(),
            std::mem::align_of::<T>(),
            false,
            <T as Resource>::shared_schema_hash(),
        ) {
            None => true,
            Some(error) => {
                // Recorded here rather than in the core: the Rust path is the
                // one whose refusal has to fail the init that raised it. A
                // foreign declaration's caller gets the error back and acts on
                // it itself.
                self.record_registration_error(error);
                false
            }
        }
    }

    /// The type-independent half of [`Self::claim_shared_resource_name`].
    ///
    /// A declaration from another language has no Rust type to ask, so it hands
    /// over the same facts its manifest carries. `foreign` decides one thing:
    /// whether the declaring names are compared. A foreign declaration has no
    /// Rust name to compare with a final path segment, and writing the shared
    /// name down is itself its statement of which resource it means, so for it
    /// the layout is the check.
    ///
    /// The recorded claim keeps its Rust declarer when a foreign declaration
    /// joins, since that record is what a later Rust arrival is compared
    /// against; a Rust arrival to a foreign claim does take the record, for the
    /// same reason in reverse. This function does not record the error it
    /// returns: the Rust wrapper above records it as a registration error,
    /// while a foreign declaration's caller acts on the refusal itself.
    // Seven facts describe one declaration, each read from the manifest or the
    // Rust type, and none can be derived from the others, so the caller passes
    // every one of them.
    #[allow(clippy::too_many_arguments)]
    fn claim_shared_resource(
        &mut self,
        id: ResourceId,
        shared_name: &str,
        declaring_type: &str,
        size: usize,
        align: usize,
        foreign: bool,
        schema_hash: Option<u64>,
    ) -> Option<WorldError> {
        let incoming = SharedResourceClaim {
            shared_name: shared_name.to_string(),
            declaring_type: declaring_type.to_string(),
            foreign,
            size,
            align,
            schema_hash,
        };
        if let Some(existing) = self.shared_resource_claims.get(&id) {
            // The id is a hash of the name, so two different names sharing it
            // are an identity collision: one slot would answer to both. The
            // recorded string is what tells the two apart.
            if existing.shared_name != shared_name {
                let error = WorldError::SharedResourceIdentityCollision {
                    shared_name: shared_name.to_string(),
                    existing_name: existing.shared_name.clone(),
                };
                error!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    shared_name = %shared_name,
                    existing_name = %existing.shared_name,
                    error = %error,
                    "two shared resource names hash to one identity"
                );
                return Some(error);
            }
            if !existing.foreign
                && !incoming.foreign
                && existing.declaring_type != incoming.declaring_type
            {
                // Logged as well as returned, exactly as `register_component`
                // logs its own failures: the log line is what names the cause
                // to whoever reads the host output.
                let error = WorldError::SharedResourceNameConflict {
                    shared_name: shared_name.to_string(),
                    existing_type: existing.declaring_type.clone(),
                    incoming_type: incoming.declaring_type.clone(),
                };
                error!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    shared_name = %shared_name,
                    existing_type = %existing.declaring_type,
                    incoming_type = %incoming.declaring_type,
                    error = %error,
                    "shared resource name claimed by two different types"
                );
                return Some(error);
            }
            if existing.size != incoming.size || existing.align != incoming.align {
                let error = WorldError::SharedResourceLayoutMismatch {
                    shared_name: shared_name.to_string(),
                    existing_size: existing.size,
                    existing_align: existing.align,
                    incoming_size: incoming.size,
                    incoming_align: incoming.align,
                };
                error!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    shared_name = %shared_name,
                    existing_size = existing.size,
                    existing_align = existing.align,
                    incoming_size = incoming.size,
                    incoming_align = incoming.align,
                    error = %error,
                    "shared resource registered with two different layouts"
                );
                return Some(error);
            }
            // Both sides carrying a hash means both declared a field shape, and
            // equal size/align is exactly the case a hash catches: {u32, u32}
            // and {f32, f32} agree on layout and not on fields.
            if let (Some(existing_hash), Some(incoming_hash)) =
                (existing.schema_hash, incoming.schema_hash)
            {
                if existing_hash != incoming_hash {
                    let error = WorldError::SharedResourceSchemaMismatch {
                        shared_name: shared_name.to_string(),
                        existing_hash,
                        incoming_hash,
                    };
                    error!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        shared_name = %shared_name,
                        error = %error,
                        "shared resource registered with two different field shapes"
                    );
                    return Some(error);
                }
            }
        }
        match self.shared_resource_claims.get(&id) {
            None => {
                self.shared_resource_claims.insert(id, incoming);
            }
            // A foreign declaration does not overwrite the record: keeping a
            // Rust declarer's name is what lets a later Rust arrival still be
            // compared against one. A Rust arrival to a foreign claim does take
            // it, because from then on there is a Rust name worth comparing.
            Some(existing) if existing.foreign && !incoming.foreign => {
                self.shared_resource_claims.insert(id, incoming);
            }
            Some(_) => {}
        }
        None
    }

    /// Re-homes every stored resource's per-type function table.
    ///
    /// The resource counterpart of [`Self::rehome_native_columns`], called at
    /// the same point in the reload: a resource's drop function belongs to
    /// whichever artifact inserted it, and that artifact's image is about to be
    /// evicted from the reload graveyard, so the table is refreshed while every
    /// image is still mapped.
    ///
    /// Three cases are left alone. A type that no artifact registered this
    /// generation has nothing newer to point at - a retired owner's resource,
    /// which [`Self::drop_resources`] releases while the retiring image is still
    /// mapped. A foreign box's table drops nothing and points at this crate's
    /// code; refreshing it from the factories would let a Rust type sharing the
    /// name hand its destructor to bytes it does not own. And a table that
    /// claims foreign ownership over a Rust value - a state the registration
    /// paths refuse, though a stale table could still reach it - is reported
    /// rather than installed, since the value would otherwise leak without a
    /// word.
    pub fn rehome_resources(&mut self) {
        for (id, resource) in &mut self.resources {
            if resource.is_foreign() {
                continue;
            }
            if let Some(&resource_ops) = self.resource_factories.get(id) {
                if resource_ops.foreign {
                    error!(
                        target: pill_core::telemetry::telemetry_target::ECS,
                        resource = ?id,
                        "a Rust resource is declared foreign; keeping its own drop"
                    );
                    continue;
                }
                resource.refresh_ops(resource_ops);
            }
        }
    }

    /// Declares an asset type the calling generation owns.
    ///
    /// The asset counterpart of [`Self::register_component`]: an asset column's
    /// per-type table is code from the artifact that filled it, so an owner that
    /// can be reloaded declares its asset types during registration, and
    /// [`Self::rehome_assets`] later points those columns at the generation that
    /// is still mapped. Using the type ([`AssetManager::add`]) declares it too,
    /// so this call only matters for a type the generation owns but does not
    /// touch in this run.
    pub fn register_asset<T>(&mut self)
    where
        T: Asset + TraitAccessible<dyn Asset>,
    {
        match self.get_resource_mut::<AssetManager>() {
            Some(assets) => assets.register::<T>(),
            None => {
                let mut assets = AssetManager::new();
                assets.register::<T>();
                self.insert_resource(assets);
            }
        }
    }

    /// Declares an asset type that can be imported from a source file, by
    /// extension, through the world's
    /// [`ImportRegistry`](crate::asset_import_registry::ImportRegistry).
    ///
    /// Declares the asset type ([`Self::register_asset`]) and records `T`'s
    /// import entry points, which are compiled into the calling binary. Call it
    /// in the registration of the module that owns `T`, on every generation.
    ///
    /// The entry points point into the calling binary, so this also inserts a
    /// marker resource that the calling generation claims. When the host retires
    /// that claim - because the module stopped registering `T` or was unloaded -
    /// it drops the marker while the image is still mapped, and the registry
    /// stops calling into it.
    ///
    /// A type whose extension another live type already imports is refused
    /// with [`WorldError::ImportExtensionClaimedTwice`], recorded as a
    /// registration error so the registering `init` fails.
    pub fn register_imported_asset<T>(&mut self)
    where
        T: crate::asset_metadata::ImportedAsset,
    {
        self.register_asset::<T>();
        self.record_import_registration::<T>(|registry, alive| registry.register::<T>(alive));
    }

    /// Declares an asset type stored as its own file in `res` - a standalone
    /// asset such as a material - so it loads by its file extension through
    /// the world's [`ImportRegistry`](crate::asset_import_registry::ImportRegistry).
    ///
    /// The standalone counterpart of [`Self::register_imported_asset`], with the
    /// same liveness marker and the same refusal of an extension another type
    /// already imports.
    pub fn register_standalone_asset<T>(&mut self)
    where
        T: crate::asset_standalone::StandaloneAsset,
    {
        self.register_asset::<T>();
        self.record_import_registration::<T>(|registry, alive| {
            registry.register_standalone::<T>(alive)
        });
    }

    /// Records one import registration of `T` with `register`, behind a fresh
    /// liveness marker this generation claims.
    fn record_import_registration<T: 'static>(
        &mut self,
        register: impl FnOnce(
            &mut crate::asset_import_registry::ImportRegistry,
            std::sync::Weak<()>,
        )
            -> Result<(), crate::asset_import_registry::ImportRegistrationError>,
    ) {
        use crate::asset_import_registry::{
            ImportRegistrationError, ImportRegistrationMarker, ImportRegistry,
        };

        let marker = ImportRegistrationMarker::<T>::new();
        // The registry is claimed on every generation, not only by the first
        // insert. A reload that kept the registry but did not claim it would
        // read as this module no longer owning it, and the host would drop it
        // once the init finished.
        self.register_resource::<ImportRegistry>();
        if self.get_resource::<ImportRegistry>().is_none() {
            self.insert_resource(ImportRegistry::default());
        }
        let import_registration = register(
            self.get_resource_mut::<ImportRegistry>()
                .expect("inserted just above"),
            marker.alive(),
        );
        match import_registration {
            // Replacing a previous generation's marker ends that generation's
            // registration; the one just recorded holds this marker instead.
            Ok(()) => self.insert_resource(marker),
            Err(ImportRegistrationError::ExtensionClaimedTwice {
                extension,
                registered_type,
                incoming_type,
            }) => self.record_registration_error(WorldError::ImportExtensionClaimedTwice {
                extension,
                registered_type,
                incoming_type,
            }),
        }
    }

    /// Re-homes every asset column's per-type function table.
    ///
    /// The third member of the re-homing family, called by the reload beside
    /// [`Self::rehome_native_columns`] and [`Self::rehome_resources`] and for
    /// the same reason: the asset store outlives every reload, so a column whose
    /// table still belongs to a retiring image has to be re-pointed while that
    /// image is mapped.
    ///
    /// This also prunes the import registry. The reload drops the marker
    /// resources of a retired generation's import registrations before
    /// re-homing, so this is when those registrations become dead; pruning here
    /// removes them at once instead of at the next registration. A dead entry is
    /// never called either way, so this only stops the registry from holding
    /// stale entries.
    pub fn rehome_assets(&mut self) {
        if let Some(assets) = self.get_resource_mut::<AssetManager>() {
            assets.rehome();
        }
        if let Some(registry) =
            self.get_resource_mut::<crate::asset_import_registry::ImportRegistry>()
        {
            registry.prune();
        }
    }

    /// Drops the named resources, releasing their values.
    ///
    /// The resource counterpart of
    /// [`drop_forgotten_components`](Self::drop_forgotten_components), with the
    /// same timing requirement: **call it while the artifact that inserted the
    /// value is still mapped.** A resource holds a drop function belonging to
    /// that artifact, so dropping it after the image is evicted would call
    /// through a dangling pointer.
    ///
    /// The host uses it when a reloaded module stops registering a resource
    /// type the retiring generation owned. Compare
    /// [`resource_ids_registered_since`](Self::resource_ids_registered_since)
    /// across the two generations to find those ids. An id that another subject
    /// still claims through [`retain_resource_claims`](Self::retain_resource_claims)
    /// is skipped, because the value's drop function belongs to the generation
    /// that inserted it, not necessarily to the subject retiring now.
    ///
    /// Returns how many were actually present and dropped.
    pub fn drop_resources(&mut self, ids: &[ResourceId]) -> usize {
        let mut dropped_resource_count = 0;
        for id in ids {
            // Another subject's claim keeps the value alive.
            if self
                .resource_claim_counts
                .get(id)
                .is_some_and(|count| *count > 0)
            {
                continue;
            }
            if self.resources.remove(id).is_some() {
                dropped_resource_count += 1;
            }
            // The id is unregistered now, so a later registration is a fresh
            // one and has to be visible to the next "registered since" diff.
            self.remove_resource_records(*id);
        }
        dropped_resource_count
    }

    /// Records one claim per id on a resource this subject registered.
    ///
    /// [`drop_resources`](Self::drop_resources) consults these counts before
    /// releasing an id that one subject retired. A shared name can be registered
    /// by several subjects, and only the last of them to let go should destroy
    /// the value.
    pub fn retain_resource_claims(&mut self, ids: &[ResourceId]) {
        for id in ids {
            *self.resource_claim_counts.entry(*id).or_insert(0) += 1;
        }
    }

    /// Drops one claim per id, releasing a value once its last claim goes.
    ///
    /// A claim an id never had is a no-op, and a release that takes the count to
    /// zero removes the entry, so a later `retain` starts a fresh count.
    pub fn release_resource_claims(&mut self, ids: &[ResourceId]) {
        for id in ids {
            if let Some(claim_count) = self.resource_claim_counts.get_mut(id) {
                *claim_count = claim_count.saturating_sub(1);
                if *claim_count == 0 {
                    self.resource_claim_counts.remove(id);
                }
            }
        }
    }

    /// Removes every per-id record this world keeps for a resource.
    ///
    /// One function decides which maps a removal clears, so a removal path cannot
    /// leave one behind; a stale `resource_field_layouts` entry, for example,
    /// would apply the previous value's offsets to a value registered under the
    /// same id again. The claim count is deliberately left alone: it belongs to
    /// [`release_resource_claims`](Self::release_resource_claims), and
    /// [`drop_resources`](Self::drop_resources) checks it before calling here.
    fn remove_resource_records(&mut self, id: ResourceId) {
        self.resource_ticks.remove(&id);
        self.resource_factories.remove(&id);
        self.resource_field_layouts.remove(&id);
        self.shared_resource_claims.remove(&id);
        self.resource_registration_stamps.remove(&id);
    }

    /// Removes a resource and returns it if it existed.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::SharedResourceHoldsAnotherType`] when the id holds
    /// a value this `T` cannot be read as - the case a shared, name-derived id
    /// makes reachable. The value is taken first and the box is put back on a
    /// refusal, so nothing about the stored resource changes: not its value, its
    /// ticks, its factory or its claim.
    pub fn remove_resource<T: Resource>(&mut self) -> Result<Option<T>, WorldError> {
        let id = ResourceId::of::<T>();
        let Some(erased_resource) = self.resources.remove(&id) else {
            return Ok(None);
        };
        let value = match erased_resource.take::<T>() {
            Ok(value) => value,
            Err(erased_resource) => {
                // Put the box back unchanged: dropping it here would take the
                // value, its ticks, its factory and its claim with it.
                self.resources.insert(id, erased_resource);
                return Err(WorldError::SharedResourceHoldsAnotherType {
                    id,
                    requested_type: std::any::type_name::<T>(),
                });
            }
        };
        self.remove_resource_records(id);
        Ok(Some(value))
    }

    /// Checks whether a resource exists *and* can be read as `T`.
    ///
    /// This is the same question [`Self::get_resource`] answers: under a shared
    /// id, a value of a different shape is not a `T`, so existence and
    /// readability agree here instead of one check reporting the id and the
    /// other the readable shape. [`Self::resource_holder`] answers the shape
    /// question, for a caller that needs to know what is stored.
    #[must_use]
    pub fn has_resource<T: Resource>(&self) -> bool {
        self.resources
            .get(&ResourceId::of::<T>())
            .is_some_and(|erased_resource| erased_resource.holds::<T>())
    }

    /// The stored box under `T`'s resource id, whatever shape it holds.
    ///
    /// For a caller whose id exists but whose type does not read: the box itself
    /// answers `holds::<T>()` and `has_shared_identity()`. `None` means the id
    /// has no resource at all.
    #[must_use]
    pub fn resource_holder<T: Resource>(&self) -> Option<&crate::resource::ErasedResource> {
        self.resources.get(&ResourceId::of::<T>())
    }

    /// Takes one resource's debug write lock and reports an overlapping holder.
    ///
    /// [`ResMut`](crate::query::ResMut) acquires the lock when it is built and
    /// releases it when it drops, so the lock covers exactly one system's access.
    ///
    /// The receiver is `&self`, not `&mut self`, because the systems of a
    /// parallel batch hold aliasing `&mut World`s and still need to call this.
    #[cfg(debug_assertions)]
    pub(crate) fn debug_acquire_resource_lock(&self, id: ResourceId) {
        let newly_inserted = self.debug_resource_write_locks().insert(id);
        debug_assert!(
            newly_inserted,
            "Resource {id:?} is already mutably borrowed by another live \
             `ResMut` - possible scheduler bug or concurrent system access"
        );
    }

    /// Releases one resource's debug write lock; see
    /// [`Self::debug_acquire_resource_lock`].
    #[cfg(debug_assertions)]
    pub(crate) fn debug_release_resource_lock(&self, id: ResourceId) {
        self.debug_resource_write_locks().remove(&id);
    }

    /// Clears the set of mutably-borrowed resources.
    ///
    /// [`Engine::process_frame`](crate::engine::Engine::process_frame) calls
    /// this at the start of every frame, so the isolation check only looks at
    /// concurrent access within a single frame.
    #[cfg(debug_assertions)]
    pub(crate) fn debug_clear_resource_locks(&mut self) {
        self.debug_resource_write_locks().clear();
    }

    /// The debug write-lock set, taken even when a panicking holder poisoned
    /// it: the set is bookkeeping for an assertion, and a poisoned one is
    /// still the best record there is.
    #[cfg(debug_assertions)]
    fn debug_resource_write_locks(
        &self,
    ) -> std::sync::MutexGuard<'_, std::collections::HashSet<ResourceId>> {
        self.debug_resource_write_locks
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}
