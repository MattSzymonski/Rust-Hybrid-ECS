//! Archetype-based component storage with SoA layout.
//!
//! An archetype is a unique combination of component types. All entities with
//! the same set of components are stored together in the same archetype for
//! cache-friendly iteration.
//!
//! # Responsibilities
//!
//! - Group entities by their exact component set into [`Archetype`] instances.
//! - Own the contiguous, type-erased component storage for each archetype,
//!   both native (`ComponentColumns`) and dynamically laid out
//!   (`DynamicColumn`).
//! - Track change-detection [`ComponentTicks`] for every component instance.
//!
//! # Design
//!
//! Archetypes use a Structure of Arrays (SoA) layout rather than an
//! Array of Structures (AoS). This means components of the same type are
//! stored contiguously in memory:
//!
//! ```text
//! Archetype [Position, Velocity]
//! ┌─────────────────────────────────────────────────┐
//! │ Entities:    [E1,     E2,     E3,     E4    ]   │
//! │ Positions:   [Pos1,   Pos2,   Pos3,   Pos4  ]   │
//! │ Velocities:  [Vel1,   Vel2,   Vel3,   Vel4  ]   │
//! └─────────────────────────────────────────────────┘
//! ```
//!
//! When iterating over all entities with Position+Velocity:
//! - SoA (this design): Sequential memory access, excellent cache utilization
//! - AoS alternative: Scattered access, poor cache performance
//!
//! The tradeoff is that accessing all components of a single entity requires
//! multiple array lookups, but this is rare compared to bulk iteration
//! in ECS-style approaches.
//!
//! Entity removal uses swap-remove to keep arrays dense: the removed entity
//! is swapped with the last entity in each component array, the last element
//! (now the removed entity's data) is popped, and the swapped entity's
//! location is updated in the `entity_locations` map. This keeps arrays
//! dense without gaps, maintaining O(1) removal.
//!
//! # Invariants
//!
//! The raw-byte storage rests on four rules, each enforced at the boundary
//! that can check it:
//!
//! - **Element identity.** [`ColumnIdentity`] records how much the compiler
//!   still vouches for a column: an exact `TypeId` (native), size and alignment
//!   with the registry having compared the full field layout (shared), or no
//!   Rust type at all (descriptor). The typed accessors enforce the first two
//!   and [`DynamicColumn::get`] panics on a mismatch; a descriptor column never
//!   receives a typed call, because no Rust type names its rows.
//! - **Alignment hosting.** A column's buffer is allocated with the row's
//!   alignment, so a row is aligned for the type the column was built for.
//!   Registration refuses a layout change the old buffer cannot host (the 4.18
//!   guard in `world.rs`): rows spawned into the old column would otherwise be
//!   read through the incoming type at the old stride.
//! - **Ops discipline.** Everything that depends on what the bytes mean goes
//!   through [`ColumnOps`], which is plain data a reload refreshes before any
//!   row is touched. `drop_range` runs exactly once per initialized row - on
//!   truncation, on `Drop`, and never for a column whose `trivial_drop` is set
//!   - and [`Blittability`] is the witness that makes a plain-data claim
//!   checkable rather than assumed.
//! - **Row lifecycle.** `len` counts initialized rows: growth leaves the new
//!   tail uninitialized until a write fills it, and `swap_remove` moves the tail
//!   row over the hole before dropping the length, so no row is read or dropped
//!   twice.

// Standard library
use std::alloc::{alloc, dealloc, handle_alloc_error, Layout};
use std::collections::HashMap;
use std::ptr::NonNull;

// External crates

// Current crate
use crate::component::{Component, ComponentId, ComponentMask, ComponentTicks};
use crate::entity::Entity;
use crate::error::WorldError;

// =============================================================================
// StorageFactory
// =============================================================================

/// Strategy for creating component storage for a specific component type.
///
/// Registered per [`ComponentId`] and consulted when an [`Archetype`] is
/// created so the archetype can allocate storage without knowing the
/// concrete component types.
pub enum StorageFactory {
    /// Creates a type-erased native storage column inside the archetype's
    /// [`ComponentColumns`].
    ///
    /// Carries only data (type id, layout, per-type function table), never a
    /// closure: the column is stored as a concrete [`DynamicColumn`] with no
    /// trait-object vtable, so it survives module unloads; the engine
    /// refreshes its function table on every reload.
    Native(NativeColumnInfo),
    /// Carries the runtime layout of a component owned by another language.
    Dynamic(DynamicComponentLayout),
}

// =============================================================================
// Blittability
// =============================================================================

/// Evidence that a dynamic component's rows are plain old data.
///
/// The dynamic-component design rests on one premise: a row is
/// bitwise-movable, owns nothing, and can be freed without running
/// destructors. `DynamicColumn` copies rows with `ptr::copy`, frees buffers
/// without touching the elements, and is `Send + Sync` on that basis - each
/// of those is only sound while the premise holds. The witness makes the
/// premise a value the engine stores next to the layout it protects, so a
/// column cannot be built without one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Blittability {
    /// Private unit field: the only ways in are the constructors below.
    _private: (),
}

impl Blittability {
    /// The witness a manifest-driven registration carries.
    ///
    /// `pill_host::csharp::components` calls this only after its
    /// `BLITTABLE_FIELD_TYPES` check rejected every field type that is not a
    /// blittable value type, so holding one of these means the fields were
    /// vetted. The name records where the check ran.
    pub fn from_manifest_fields() -> Self {
        Self { _private: () }
    }

    /// The witness for shapes written out in this crate's tests.
    ///
    /// Test registrations name value-type fields literally; production code
    /// outside the crate reaches the checkout through
    /// [`Self::from_manifest_fields`].
    #[cfg(test)]
    pub(crate) fn engine_verified() -> Self {
        Self { _private: () }
    }

    /// Claims the witness without a check.
    ///
    /// # Safety
    ///
    /// Every field of the component must be a blittable value type: no
    /// pointers, references or other owners. A caller that gets this wrong
    /// hands `DynamicColumn` ownership it will never release.
    pub unsafe fn assume() -> Self {
        Self { _private: () }
    }
}

// =============================================================================
// NativeColumnInfo
// =============================================================================

/// Everything an archetype needs to build a column for a Rust component type,
/// as plain data.
///
/// The engine's replacement for `ErasedVecStorageInfo`, and it exists for the
/// same reason that type carries data rather than a closure: a factory that
/// captured a generation's code would keep an unmapped image's vtable alive.
/// It differs in dropping the `dyn Component` upcasts, which nothing in this
/// engine ever called.
#[derive(Debug, Clone, Copy)]
pub struct NativeColumnInfo {
    /// Runtime identity of the element type.
    pub type_id: std::any::TypeId,
    /// Size of one row in bytes.
    pub size: usize,
    /// Alignment of one row in bytes.
    pub align: usize,
    /// Whether the element is identified by layout instead of by `TypeId`,
    /// for a type compiled into more than one binary.
    pub shared: bool,
    /// Per-type behaviour, regenerated by whichever image registered last.
    pub ops: ColumnOps,
}

impl NativeColumnInfo {
    /// Describe the column for a concrete Rust element type.
    pub fn of<T: 'static>(shared: bool) -> Self {
        Self {
            type_id: std::any::TypeId::of::<T>(),
            size: std::mem::size_of::<T>(),
            align: std::mem::align_of::<T>(),
            shared,
            ops: ColumnOps::of::<T>(),
        }
    }
}

impl std::fmt::Debug for ColumnOps {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ColumnOps")
            .field("trivial_drop", &self.trivial_drop)
            .finish()
    }
}

// =============================================================================
// ColumnIdentity
// =============================================================================

/// What a column requires of a `T` before it will hand out a typed reference.
///
/// The three cases are the three ways this engine knows a component type, and
/// they are ordered by how much the compiler still vouches for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ColumnIdentity {
    /// An ordinary Rust component: only the exact `TypeId` may read the rows.
    Native(std::any::TypeId),
    /// A Rust component compiled into more than one binary, so each binary has
    /// a different `TypeId` for one type and the layout is the only check left.
    ///
    /// The caller takes on what `TypeId` was discharging: matching size and
    /// alignment is necessary but not sufficient, so the registry compares the
    /// full field layout before a column is created with this identity.
    Shared,
    /// No Rust type names these rows; they are reached as bytes.
    ///
    /// A typed accessor on such a column is a programming error, not a runtime
    /// condition - in a monoglot project nothing has a `T` to ask with.
    Descriptor,
}

// =============================================================================
// ColumnOps
// =============================================================================

/// Per-type behaviour for one column, as plain data.
///
/// This is the interface half of the raw-byte storage model: a column holds
/// bytes plus a descriptor, and everything that depends on what those bytes
/// *mean* goes through here. Two flavours implement it.
///
/// - **Generated**, for a compile-time Rust type: [`ColumnOps::of`] builds the
///   table from `T`'s own glue, so drop runs destructors and default runs
///   `Default::default`.
/// - **Generic**, for a descriptor-only type declared in another language:
///   [`ColumnOps::blittable`] needs no per-type code at all, because the
///   validated vocabulary is plain data - drop is a no-op, move is `memcpy`,
///   default is a zero fill.
///
/// Like [`ErasedVecStorageInfo`], the table is *data*, never a trait object:
/// a column that outlives the DLL that filled it can be re-pointed at the new
/// generation's table instead of dangling into an unmapped image.
#[derive(Clone, Copy)]
pub struct ColumnOps {
    /// Drop `count` initialized rows starting at `ptr`, or nothing for plain
    /// data.
    ///
    /// # Safety
    ///
    /// `ptr` must address `count` live, correctly aligned rows of this
    /// column's element type.
    pub drop_range: unsafe fn(*mut u8, usize),
    /// Whether [`Self::drop_range`] does nothing.
    ///
    /// Read rather than called on the hot paths: a POD column skips the call
    /// entirely instead of paying an indirect jump to an empty function.
    pub trivial_drop: bool,
}

/// Drop nothing: the element type owns no resources.
///
/// # Safety
///
/// Trivially safe - it reads neither argument.
unsafe fn drop_range_of_nothing(_ptr: *mut u8, _count: usize) {}

/// Drop `count` values of type `T` starting at `ptr`.
///
/// # Safety
///
/// `ptr` must address `count` live, correctly aligned `T` values.
unsafe fn drop_range_of<T>(ptr: *mut u8, count: usize) {
    if count == 0 || !std::mem::needs_drop::<T>() {
        return;
    }
    // SAFETY: the caller guarantees `count` live, aligned `T` values at `ptr`.
    unsafe {
        std::ptr::drop_in_place(std::ptr::slice_from_raw_parts_mut(ptr.cast::<T>(), count));
    }
}

impl ColumnOps {
    /// The generated table for a concrete Rust element type.
    pub fn of<T: 'static>() -> Self {
        Self {
            drop_range: drop_range_of::<T>,
            trivial_drop: !std::mem::needs_drop::<T>(),
        }
    }

    /// The generic table for plain-data rows described only by a descriptor.
    ///
    /// Requires the same [`Blittability`] witness the column does, so a table
    /// claiming there is nothing to drop cannot be built without the evidence
    /// that made that true.
    pub fn blittable(_witness: Blittability) -> Self {
        Self {
            drop_range: drop_range_of_nothing,
            trivial_drop: true,
        }
    }
}

// =============================================================================
// DynamicComponentLayout
// =============================================================================

/// Runtime layout for a component whose concrete type is owned by another language.
///
/// Describes the memory footprint of an opaque component column so its rows
/// can be copied in and out as raw bytes without knowing the concrete type.
#[derive(Debug, Clone)]
pub struct DynamicComponentLayout {
    /// Size in bytes of a single component instance.
    pub size: usize,
    /// Alignment in bytes required by a single component instance.
    pub align: usize,
    /// Hash identifying the component's schema across language boundaries.
    pub schema_hash: u64,
    /// Evidence that rows of this shape are plain old data.
    ///
    /// Carried so the column that stores the rows never has to re-derive the
    /// premise: `Send`, `Sync` and the destructor-free paths all cite it.
    pub blittability: Blittability,
}

impl DynamicComponentLayout {
    /// Builds a layout, checking that size and alignment can describe storage.
    ///
    /// The single constructor: every layout carries a [`Blittability`], so no
    /// column exists without one.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DynamicSizeZero`],
    /// [`WorldError::DynamicAlignmentInvalid`] or
    /// [`WorldError::DynamicLayoutInvalid`] when the size and alignment cannot
    /// describe an allocation.
    pub fn new(
        size: usize,
        align: usize,
        schema_hash: u64,
        blittability: Blittability,
    ) -> Result<Self, WorldError> {
        validate_dynamic_layout(size, align)?;
        Ok(Self {
            size,
            align,
            schema_hash,
            blittability,
        })
    }
}

/// Check that a size and alignment can describe dynamic storage.
///
/// Shared by registration and relayout so the two can never disagree about what
/// a usable layout is. The errors name the offending layout rather than the
/// caller, because both callers hand the same three facts to the same engine.
pub(crate) fn validate_dynamic_layout(size: usize, align: usize) -> Result<(), WorldError> {
    if size == 0 {
        return Err(WorldError::DynamicSizeZero);
    }
    if align == 0 || !align.is_power_of_two() {
        return Err(WorldError::DynamicAlignmentInvalid);
    }
    if Layout::from_size_align(size, align).is_err() {
        return Err(WorldError::DynamicLayoutInvalid);
    }
    Ok(())
}

// =============================================================================
// DynamicFieldPlan
// =============================================================================

/// One top-level field of a layout, as a migration plan sees it.
///
/// Only the top level, by design: a nested struct field moves as a single
/// block, so a change inside it cannot silently reinterpret its members. The
/// fields a plan maps are exactly the names the two layouts agree on.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LayoutField<'a> {
    /// Field name. Matching is by name, never by position.
    pub name: &'a str,
    /// Type tag from the engine's field vocabulary (`f32`, `u32`,
    /// `struct:<path>`, ...).
    ///
    /// Matching considers this as well as the name, because a name alone
    /// cannot tell a field that moved from one that changed meaning: an `i32`
    /// and an `f32` are both four bytes, so copying between them reinterprets
    /// the bits into a different number rather than preserving a value.
    pub type_tag: &'a str,
    /// Byte offset of the field inside a row.
    pub offset: usize,
    /// Byte size of the field.
    pub size: usize,
}

/// Where one field of a new layout takes its bytes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FieldSource {
    /// Copy from this byte offset in the old row.
    OldOffset(usize),
    /// Leave the field zeroed. The relayout zeroes every row before it applies
    /// the plan, so a field with no source is defined rather than stale.
    ZeroFill,
}

/// One instruction in a migration plan.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlannedField {
    /// Byte offset of the field in the new row.
    pub offset: usize,
    /// Number of bytes written at `offset`.
    pub bytes: usize,
    /// Where those bytes come from.
    pub source: FieldSource,
}

/// A byte-level plan for moving rows from one layout of a component to another.
///
/// Data, not code. The caller computes it from the two field lists it already
/// has - the managed manifest carries field names, offsets and sizes, so the
/// C# path builds one per changed component - and the engine applies it to every
/// stored row. Anything the plan does not cover is left zero.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct DynamicFieldPlan {
    /// The instructions, in the order they are applied.
    fields: Vec<PlannedField>,
    /// Names of fields the plan reset because their type changed.
    ///
    /// Kept so the caller can say so: a field that silently becomes a
    /// different number is the failure this exists to prevent, and a field
    /// that resets without a word is only a smaller version of the same
    /// problem.
    retyped: Vec<String>,
}

impl DynamicFieldPlan {
    /// An empty plan: every byte of every new row is zero.
    #[must_use]
    pub fn new() -> Self {
        Self {
            fields: Vec::new(),
            retyped: Vec::new(),
        }
    }

    /// Fields this plan reset because their declared type changed.
    pub fn retyped_fields(&self) -> &[String] {
        &self.retyped
    }

    /// Append one instruction.
    pub fn push(&mut self, offset: usize, bytes: usize, source: FieldSource) {
        self.fields.push(PlannedField {
            offset,
            bytes,
            source,
        });
    }

    /// Build a plan from the top-level fields of the old and new layout.
    ///
    /// Fields are matched **by name**, so a reorder keeps every value with its
    /// own name. A field only the new layout has becomes a zero fill; a field
    /// only the old layout had is left out, which is how a removed field's
    /// bytes leave the rows. A name repeated in one list matches its first
    /// occurrence and the rest are ignored, which keeps the plan deterministic
    /// for a malformed input rather than depending on iteration order.
    ///
    /// A matched pair whose sizes disagree copies the smaller size and leaves
    /// the remainder zeroed, so a field that grew keeps its bytes and gains a
    /// defined tail instead of reading past the old row.
    #[must_use]
    pub fn between(old: &[LayoutField<'_>], new: &[LayoutField<'_>]) -> Self {
        let mut plan = Self::new();
        for new_field in new {
            match old
                .iter()
                .find(|old_field| old_field.name == new_field.name)
            {
                // A name match whose type also matches carries its bytes over.
                Some(old_field) if old_field.type_tag == new_field.type_tag => plan.push(
                    new_field.offset,
                    old_field.size.min(new_field.size),
                    FieldSource::OldOffset(old_field.offset),
                ),
                // A name match whose type changed is reset rather than copied.
                //
                // Copying would reinterpret the bits: an `i32` of 5 read as an
                // `f32` is 7e-45, which is not a migration of the value but a
                // different number wearing its name. Resetting matches what the
                // native lane already does for the same edit - serde fails to
                // read the old shape and falls back to the type's default - and
                // keeps an ordinary retype from costing a host restart. The
                // field is recorded so the caller can report it.
                Some(old_field) => {
                    plan.retyped.push(new_field.name.to_owned());
                    let _ = old_field;
                    plan.push(new_field.offset, new_field.size, FieldSource::ZeroFill);
                }
                None => plan.push(new_field.offset, new_field.size, FieldSource::ZeroFill),
            }
        }
        plan
    }

    /// The instructions, in application order.
    #[must_use]
    pub fn fields(&self) -> &[PlannedField] {
        &self.fields
    }

    /// Whether the plan carries no instructions.
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.fields.is_empty()
    }

    /// Check every instruction against the two row sizes.
    ///
    /// Callers run this before touching a row, so a plan that does not fit is
    /// refused while the storage is still untouched. The error is
    /// [`WorldError::DynamicRowInvalid`]: the plan is layout-level data and
    /// carries no component id for a richer message.
    pub fn validate(&self, old_size: usize, new_size: usize) -> Result<(), WorldError> {
        for field in &self.fields {
            if out_of_bounds(field.offset, field.bytes, new_size) {
                return Err(WorldError::DynamicRowInvalid);
            }
            if let FieldSource::OldOffset(offset) = field.source {
                if out_of_bounds(offset, field.bytes, old_size) {
                    return Err(WorldError::DynamicRowInvalid);
                }
            }
        }
        Ok(())
    }
}

/// Whether `bytes` written at `offset` leaves a row of `size` bytes.
///
/// A helper rather than two `checked_add` expressions inline: the same question
/// is asked of the source and the destination of every instruction.
fn out_of_bounds(offset: usize, bytes: usize, size: usize) -> bool {
    match offset.checked_add(bytes) {
        Some(end) => end > size,
        None => true,
    }
}

// =============================================================================
// ComponentColumns
// =============================================================================

/// The native component columns of one archetype, keyed by [`ComponentId`].
///
/// This replaced a `TraitTypeMap` keyed by [`TypeId`](std::any::TypeId). That
/// map was already storing a concrete `ErasedVecStorage` per entry rather than
/// a trait object, so the only thing it contributed was the key - and `TypeId`
/// is the wrong key here. A component type linked into two binaries has two
/// `TypeId`s, so the binary that did not create the column could not find it,
/// while the engine identifies the same component by one [`ComponentId`] every
/// other structure is already keyed on: the registry's bit, the archetype's
/// `component_types`, the tick vectors, the storage factories. Keying the
/// columns the same way makes a shared component reachable from either binary
/// and removes a translation step from every lookup.
#[derive(Default)]
pub struct ComponentColumns {
    /// One contiguous column per native component in the archetype.
    columns: HashMap<ComponentId, DynamicColumn>,
}

impl ComponentColumns {
    /// Creates an empty set of columns.
    pub fn new() -> Self {
        Self {
            columns: HashMap::new(),
        }
    }

    /// Creates an empty set of columns sized for `capacity` component types.
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            columns: HashMap::with_capacity(capacity),
        }
    }

    /// Returns the column for `component_id`, or `None` when the archetype
    /// does not store that component.
    #[inline]
    pub fn get(&self, component_id: ComponentId) -> Option<&DynamicColumn> {
        self.columns.get(&component_id)
    }

    /// Returns the column for `component_id` mutably.
    #[inline]
    pub fn get_mut(&mut self, component_id: ComponentId) -> Option<&mut DynamicColumn> {
        self.columns.get_mut(&component_id)
    }

    /// Returns the column storing `T`.
    ///
    /// # Panics
    ///
    /// Panics when the archetype does not store `T`, which means the caller
    /// reached a column the archetype's mask says is not there.
    #[inline]
    pub fn column_of<T: Component>(&self) -> &DynamicColumn {
        self.get(ComponentId::of::<T>())
            .unwrap_or_else(|| missing_column::<T>())
    }

    /// Returns the column storing `T`, mutably.
    ///
    /// # Panics
    ///
    /// Panics under the same condition as [`Self::column_of`].
    #[inline]
    pub fn column_of_mut<T: Component>(&mut self) -> &mut DynamicColumn {
        self.get_mut(ComponentId::of::<T>())
            .unwrap_or_else(|| missing_column::<T>())
    }

    /// Inserts a column for `component_id`.
    ///
    /// # Panics
    ///
    /// Panics when a column for that component already exists; an archetype
    /// builds each of its columns exactly once.
    pub fn insert(&mut self, component_id: ComponentId, column: DynamicColumn) {
        let replaced = self.columns.insert(component_id, column);
        assert!(
            replaced.is_none(),
            "component {component_id:?} already has a column in this archetype"
        );
    }

    /// Removes and returns the column for `component_id`.
    pub fn remove(&mut self, component_id: ComponentId) -> Option<DynamicColumn> {
        self.columns.remove(&component_id)
    }

    /// Whether a column exists for `component_id`.
    #[inline]
    pub fn contains(&self, component_id: ComponentId) -> bool {
        self.columns.contains_key(&component_id)
    }

    /// Number of columns stored.
    pub fn len(&self) -> usize {
        self.columns.len()
    }

    /// Whether no columns are stored.
    pub fn is_empty(&self) -> bool {
        self.columns.is_empty()
    }
}

/// Report a lookup for a column the archetype does not have.
///
/// Split out of the accessors so the panic path stays off their inlined fast
/// path, and `#[cold]` so the branch predictor is told which way this goes.
#[cold]
#[inline(never)]
fn missing_column<T: Component>() -> ! {
    panic!(
        "archetype has no column for component {} ({:?}); the archetype's mask \
         and its columns disagree, or the component was never registered",
        std::any::type_name::<T>(),
        ComponentId::of::<T>(),
    )
}

// =============================================================================
// DynamicColumn
// =============================================================================

/// Aligned, densely packed storage for a type-erased component column.
///
/// Owns a raw heap allocation whose element size and alignment come from a
/// [`DynamicComponentLayout`]. Rows are written and read as raw bytes, which
/// lets components defined in other languages share storage with native ones.
pub struct DynamicColumn {
    /// Runtime layout of the stored element type.
    layout: DynamicComponentLayout,
    /// Per-type behaviour, replaceable when a reload brings newer glue.
    ops: ColumnOps,
    /// What a typed accessor must prove before it may read these rows.
    identity: ColumnIdentity,
    /// Pointer to the heap allocation, or dangling before the first growth.
    data: NonNull<u8>,
    /// Number of initialized rows.
    len: usize,
    /// Number of rows the current allocation can hold.
    capacity: usize,
}

impl DynamicColumn {
    /// Creates an empty column for the given runtime layout.
    ///
    /// No heap allocation is made until the first row is pushed.
    ///
    /// This is a POD-only container: rows are copied as raw bytes and the
    /// buffer is freed without running element destructors, so the layout
    /// must describe a blittable value type carrying a [`Blittability`]
    /// witness. The layout's own validity is re-checked here, in every build
    /// profile, so a directly constructed column cannot defer the discovery
    /// of a degenerate layout to its first growth.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DynamicSizeZero`],
    /// [`WorldError::DynamicAlignmentInvalid`] or
    /// [`WorldError::DynamicLayoutInvalid`] when the layout cannot describe
    /// an allocation.
    pub fn new(layout: DynamicComponentLayout) -> Result<Self, WorldError> {
        validate_dynamic_layout(layout.size, layout.align)?;
        let ops = ColumnOps::blittable(layout.blittability);
        Ok(Self {
            layout,
            ops,
            identity: ColumnIdentity::Descriptor,
            data: NonNull::dangling(),
            len: 0,
            capacity: 0,
        })
    }

    /// Creates an empty column for a compile-time Rust element type.
    ///
    /// The counterpart of [`Self::new`]: same buffer, same descriptor, but the
    /// ops table is generated from `T` and typed accessors are allowed.
    ///
    /// `shared` selects [`ColumnIdentity::Shared`], for a type compiled into
    /// more than one binary; see that variant for what the caller must have
    /// checked first.
    ///
    /// # Errors
    ///
    /// As [`Self::new`], when size and alignment cannot describe an allocation.
    pub fn new_native<T: 'static>(schema_hash: u64, shared: bool) -> Result<Self, WorldError> {
        Self::from_native_info(NativeColumnInfo::of::<T>(shared), schema_hash)
    }

    /// Creates an empty column from a native description.
    ///
    /// The non-generic twin of [`Self::new_native`], for the archetype, which
    /// builds columns from a registered factory and has no `T` in scope.
    ///
    /// # Errors
    ///
    /// Returns a layout error when size and alignment cannot describe an
    /// allocation.
    pub fn from_native_info(info: NativeColumnInfo, schema_hash: u64) -> Result<Self, WorldError> {
        // SAFETY: never consulted for a native column - its drop glue comes
        // from `info.ops` - but the layout type requires a witness, so the
        // claim is made where it is provably unused.
        let blittability = unsafe { Blittability::assume() };
        let layout = DynamicComponentLayout::new(info.size, info.align, schema_hash, blittability)?;
        Ok(Self {
            layout,
            ops: info.ops,
            identity: if info.shared {
                ColumnIdentity::Shared
            } else {
                ColumnIdentity::Native(info.type_id)
            },
            data: NonNull::dangling(),
            len: 0,
            capacity: 0,
        })
    }

    /// The per-type table this column drops and moves its rows through.
    pub fn ops(&self) -> ColumnOps {
        self.ops
    }

    /// Point the column at a newer generation's table.
    ///
    /// A column outlives the image that filled it, so its drop glue has to be
    /// re-aimed at code that is still mapped. The migration path additionally
    /// aims it at the *retiring* table before dropping old rows, so they are
    /// released by the generation that allocated them.
    pub fn refresh_ops(&mut self, ops: ColumnOps) {
        self.ops = ops;
    }

    /// What a typed accessor must prove before reading these rows.
    pub fn identity(&self) -> ColumnIdentity {
        self.identity
    }

    /// Panic unless `T` may be used to read this column's rows.
    ///
    /// The check mirrors the identity's own promise: an exact `TypeId` for a
    /// native column, layout equality for a shared one, and nothing at all for
    /// a descriptor column, which has no typed reading.
    fn assert_element_type<T: 'static>(&self) {
        match self.identity {
            ColumnIdentity::Native(type_id) => assert!(
                type_id == std::any::TypeId::of::<T>(),
                "column holds a different component type than the requested one"
            ),
            ColumnIdentity::Shared => assert!(
                std::mem::size_of::<T>() == self.layout.size
                    && std::mem::align_of::<T>() == self.layout.align,
                "shared column layout {}/{} does not match the requested type's {}/{}",
                self.layout.size,
                self.layout.align,
                std::mem::size_of::<T>(),
                std::mem::align_of::<T>()
            ),
            ColumnIdentity::Descriptor => {
                panic!("a descriptor-only column has no Rust type; reach its rows as bytes")
            }
        }
    }

    /// Returns the number of initialized rows in this column.
    pub fn len(&self) -> usize {
        self.len
    }

    /// Whether this column holds no rows.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Returns the size in bytes of a single stored component instance.
    pub fn element_size(&self) -> usize {
        self.layout.size
    }

    /// Returns the alignment in bytes required by a single stored instance.
    pub fn alignment(&self) -> usize {
        self.layout.align
    }

    /// Returns the schema hash identifying the stored component type.
    pub fn schema_hash(&self) -> u64 {
        self.layout.schema_hash
    }

    /// Returns a raw pointer to the underlying buffer.
    ///
    /// The pointer is dangling before the first allocation and is invalidated
    /// by any later reallocation or by dropping the column.
    pub fn as_mut_ptr(&mut self) -> *mut u8 {
        self.data.as_ptr()
    }

    /// Appends a zero-initialized row to this column.
    ///
    /// Grows the column when needed and leaves the new row's bytes zeroed.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DynamicLayoutInvalid`] when the element layout
    /// cannot describe the next allocation.
    pub fn push_zeroed(&mut self) -> Result<(), WorldError> {
        self.reserve_one()?;
        // SAFETY: reserve_one guarantees one writable, correctly aligned slot.
        unsafe {
            std::ptr::write_bytes(
                self.data.as_ptr().add(self.len * self.layout.size),
                0,
                self.layout.size,
            )
        };
        self.len += 1;
        Ok(())
    }

    /// Appends a row containing a copy of the given bytes.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DynamicSizeMismatch`] when `bytes` does not
    /// contain exactly `element_size()` bytes, or
    /// [`WorldError::DynamicLayoutInvalid`] when the element layout cannot
    /// describe the next allocation.
    pub fn push_bytes(&mut self, bytes: &[u8]) -> Result<(), WorldError> {
        if bytes.len() != self.layout.size {
            return Err(WorldError::DynamicSizeMismatch);
        }
        self.reserve_one()?;
        // SAFETY: source and destination are valid for exactly one element and
        // cannot overlap because the source is outside this column's spare slot.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.data.as_ptr().add(self.len * self.layout.size),
                self.layout.size,
            );
        }
        self.len += 1;
        Ok(())
    }

    /// Copies the row at `index` from another column and appends it here.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DynamicSizeMismatch`] when the two columns have
    /// different element sizes, [`WorldError::DynamicRowInvalid`] when `index`
    /// is out of bounds of `source`, and [`WorldError::DynamicLayoutInvalid`]
    /// when the element layout cannot describe the next allocation. All three
    /// travel the reporting path: a drifted column must not abort a frame from
    /// inside the command flush, where the caller can hand the error back.
    pub fn push_from(&mut self, source: &Self, index: usize) -> Result<(), WorldError> {
        if self.layout.size != source.layout.size {
            return Err(WorldError::DynamicSizeMismatch);
        }
        if index >= source.len {
            return Err(WorldError::DynamicRowInvalid);
        }
        self.reserve_one()?;
        // SAFETY: both slots are allocated, aligned, non-overlapping columns.
        unsafe {
            std::ptr::copy_nonoverlapping(
                source.data.as_ptr().add(index * source.layout.size),
                self.data.as_ptr().add(self.len * self.layout.size),
                self.layout.size,
            );
        }
        self.len += 1;
        Ok(())
    }

    /// Overwrites the row at `index` with a copy of the given bytes.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DynamicRowInvalid`] when `index` is out of
    /// bounds or `bytes` does not contain exactly `element_size()` bytes.
    pub fn set_bytes(&mut self, index: usize, bytes: &[u8]) -> Result<(), WorldError> {
        if index >= self.len || bytes.len() != self.layout.size {
            return Err(WorldError::DynamicRowInvalid);
        }
        // SAFETY: the checked row is initialized and bytes has one element's size.
        unsafe {
            std::ptr::copy_nonoverlapping(
                bytes.as_ptr(),
                self.data.as_ptr().add(index * self.layout.size),
                self.layout.size,
            );
        }
        Ok(())
    }

    /// Returns the raw bytes of the row at `index`, or `None` when out of bounds.
    pub fn bytes(&self, index: usize) -> Option<&[u8]> {
        if index >= self.len {
            return None;
        }
        // SAFETY: the row is initialized and lives for the returned shared borrow.
        Some(unsafe {
            std::slice::from_raw_parts(
                self.data.as_ptr().add(index * self.layout.size),
                self.layout.size,
            )
        })
    }

    // ---------------------------------------------------------------------
    // Typed access
    //
    // Available only where a Rust type names the rows; `assert_element_type`
    // is what separates the two lanes at the point of use. The arithmetic is
    // the same either way - `data + index * elem_size` - so a typed read costs
    // exactly what a byte read costs.
    // ---------------------------------------------------------------------

    /// Append a typed value, growing the column when needed.
    ///
    /// # Panics
    ///
    /// If `T` is not this column's element type, or the column cannot grow.
    /// Growth failure is a panic rather than an error because a native
    /// column's layout was validated at registration, so the only way to get
    /// here is a capacity overflow the callers - command flush, migration,
    /// archetype move - have no recovery for. The erased column this replaces
    /// aborted through `handle_alloc_error` in the same situation.
    pub fn push<T: 'static>(&mut self, value: T) {
        self.assert_element_type::<T>();
        self.reserve_one()
            .expect("a native column must be able to grow for one more row");
        // SAFETY: `reserve_one` guaranteed capacity for one more row, and the
        // element type was just checked, so the slot is correctly aligned and
        // sized for `T`. `write` does not drop the uninitialized destination.
        unsafe {
            self.data
                .as_ptr()
                .add(self.len * self.layout.size)
                .cast::<T>()
                .write(value);
        }
        self.len += 1;
    }

    /// Shared reference to one row, bounds- and type-checked.
    ///
    /// # Panics
    ///
    /// If `T` is not this column's element type, or `index` is out of range.
    pub fn get<T: 'static>(&self, index: usize) -> &T {
        self.assert_element_type::<T>();
        assert!(index < self.len, "component index {index} out of range");
        // SAFETY: index and element type were both just checked.
        unsafe { self.get_unchecked::<T>(index) }
    }

    /// Mutable twin of [`Self::get`].
    ///
    /// # Panics
    ///
    /// As [`Self::get`].
    pub fn get_mut<T: 'static>(&mut self, index: usize) -> &mut T {
        self.assert_element_type::<T>();
        assert!(index < self.len, "component index {index} out of range");
        // SAFETY: index and element type were both just checked.
        unsafe { self.get_mut_unchecked::<T>(index) }
    }

    /// Shared reference to one row with no checks.
    ///
    /// # Safety
    ///
    /// `T` must be this column's element type and `index` must be in range.
    /// The query layer resolves both once per archetype rather than per row,
    /// which is why this exists.
    pub unsafe fn get_unchecked<T: 'static>(&self, index: usize) -> &T {
        // SAFETY: the caller guarantees the type and the index.
        unsafe { &*self.data.as_ptr().add(index * self.layout.size).cast::<T>() }
    }

    /// Mutable twin of [`Self::get_unchecked`].
    ///
    /// # Safety
    ///
    /// As [`Self::get_unchecked`], and no other reference to the row may
    /// exist for the lifetime of the returned one.
    pub unsafe fn get_mut_unchecked<T: 'static>(&mut self, index: usize) -> &mut T {
        // SAFETY: the caller guarantees the type, the index and the exclusivity.
        unsafe { &mut *self.data.as_ptr().add(index * self.layout.size).cast::<T>() }
    }

    /// The rows as a typed slice.
    ///
    /// # Panics
    ///
    /// If `T` is not this column's element type.
    pub fn as_slice<T: 'static>(&self) -> &[T] {
        self.assert_element_type::<T>();
        if self.len == 0 {
            return &[];
        }
        // SAFETY: the element type was checked and `len` rows are initialized.
        unsafe { std::slice::from_raw_parts(self.data.as_ptr().cast::<T>(), self.len) }
    }

    /// Mutable twin of [`Self::as_slice`].
    ///
    /// # Panics
    ///
    /// As [`Self::as_slice`].
    pub fn as_mut_slice<T: 'static>(&mut self) -> &mut [T] {
        self.assert_element_type::<T>();
        if self.len == 0 {
            return &mut [];
        }
        // SAFETY: as above, with exclusivity from `&mut self`.
        unsafe { std::slice::from_raw_parts_mut(self.data.as_ptr().cast::<T>(), self.len) }
    }

    /// Remove one row by swapping the last into its place, returning it.
    ///
    /// # Panics
    ///
    /// If `T` is not this column's element type, or `index` is out of range.
    pub fn swap_remove<T: 'static>(&mut self, index: usize) -> T {
        self.assert_element_type::<T>();
        assert!(index < self.len, "component index {index} out of range");
        // SAFETY: the row is initialized and the type was checked, so it can be
        // moved out; the tail row is then moved over the hole and the length
        // drops, so no row is ever read twice or dropped twice.
        unsafe {
            let base = self.data.as_ptr();
            let taken = base.add(index * self.layout.size).cast::<T>().read();
            let last = self.len - 1;
            if index != last {
                std::ptr::copy_nonoverlapping(
                    base.add(last * self.layout.size),
                    base.add(index * self.layout.size),
                    self.layout.size,
                );
            }
            self.len = last;
            taken
        }
    }

    /// Reserve capacity for at least `additional` more rows.
    ///
    /// Allocates the exact target rather than doubling toward it: a caller
    /// that says how many rows are coming knows better than the growth policy.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DynamicLayoutInvalid`] when the capacity or the
    /// allocation layout it implies cannot be represented.
    pub fn reserve_rows(&mut self, additional: usize) -> Result<(), WorldError> {
        let target = self
            .len
            .checked_add(additional)
            .ok_or(WorldError::DynamicLayoutInvalid)?;
        self.grow_to(target)
    }

    /// Size of one row in bytes, under the name the erased column used.
    pub fn elem_size(&self) -> usize {
        self.layout.size
    }

    /// Alignment of one row in bytes, under the name the erased column used.
    pub fn elem_align(&self) -> usize {
        self.layout.align
    }

    /// Read pointer to one row's bytes.
    ///
    /// The byte-level counterpart of the typed accessors, for a caller that
    /// has established a row's layout by other means - the renderer reads a
    /// shared component this way, because the component it wants was validated
    /// by name and layout rather than by `TypeId`.
    ///
    /// Returns `None` when `index` is out of range.
    pub fn row_ptr(&self, index: usize) -> Option<*const u8> {
        if index >= self.len {
            return None;
        }
        // SAFETY: the index is in range, so the offset stays inside the
        // allocation's initialized prefix.
        Some(unsafe { self.data.as_ptr().add(index * self.layout.size) })
    }

    /// Raw read pointer to the first row.
    pub fn raw_ptr(&self) -> *const u8 {
        self.data.as_ptr()
    }

    /// Runtime identity of the element type, or `None` for a descriptor column.
    pub fn element_type_id(&self) -> Option<std::any::TypeId> {
        match self.identity {
            ColumnIdentity::Native(type_id) => Some(type_id),
            ColumnIdentity::Shared | ColumnIdentity::Descriptor => None,
        }
    }

    /// Whether the element type is identified by layout rather than `TypeId`.
    pub fn has_shared_identity(&self) -> bool {
        matches!(self.identity, ColumnIdentity::Shared)
    }

    /// Typed read pointer to the first row.
    ///
    /// # Panics
    ///
    /// If `T` is not this column's element type.
    pub fn as_ptr<T: 'static>(&self) -> *const T {
        self.assert_element_type::<T>();
        self.data.as_ptr().cast::<T>()
    }

    /// Iterate the rows as `T`.
    ///
    /// # Panics
    ///
    /// If `T` is not this column's element type.
    pub fn iter<T: 'static>(&self) -> impl Iterator<Item = &T> {
        self.as_slice::<T>().iter()
    }

    /// Reserve capacity for `additional` more rows of `T`.
    ///
    /// # Panics
    ///
    /// If `T` is not this column's element type, or the capacity cannot
    /// describe an allocation - the erased column panicked here too, and the
    /// one caller reserves a capacity it chose.
    pub fn reserve<T: 'static>(&mut self, additional: usize) {
        self.assert_element_type::<T>();
        self.reserve_rows(additional)
            .expect("reserved capacity must describe a valid allocation");
    }

    /// Rewrite every row into a new layout, following `plan`.
    ///
    /// The column keeps its row count and its row order, so indices callers
    /// hold - every `EntityLocation` that names a row among them - stay valid.
    /// Anything the plan does not cover is zeroed, which is how a field added to
    /// a foreign-language component starts at a defined value instead of a byte
    /// left over from the previous shape.
    ///
    /// Rows are transformed through a scratch copy, so a plan that moves fields
    /// inside a row cannot read a byte it has already overwritten. The buffer is
    /// rewritten in place when the element size and alignment are unchanged and
    /// reallocated at the same capacity otherwise.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DynamicSizeZero`],
    /// [`WorldError::DynamicAlignmentInvalid`] or
    /// [`WorldError::DynamicLayoutInvalid`] when the new layout cannot describe
    /// storage, and [`WorldError::DynamicRowInvalid`] when a planned field reads
    /// or writes past the edge of a row. Nothing is modified in either case.
    pub fn relayout(
        &mut self,
        layout: DynamicComponentLayout,
        plan: &DynamicFieldPlan,
    ) -> Result<usize, WorldError> {
        validate_dynamic_layout(layout.size, layout.align)?;
        plan.validate(self.layout.size, layout.size)?;
        let previous_size = self.layout.size;
        Ok(self.relayout_validated(layout, plan, previous_size))
    }

    /// [`Self::relayout`] for a caller that has already validated.
    ///
    /// Infallible by construction: the only failure modes are the layout and
    /// plan checks above, and both are deterministic functions of data the
    /// caller can check once for every column it is about to rewrite. That is
    /// what lets a world-level relayout refuse a bad plan before it touches the
    /// first column, instead of leaving some columns migrated and some not.
    pub(crate) fn relayout_validated(
        &mut self,
        layout: DynamicComponentLayout,
        plan: &DynamicFieldPlan,
        previous_size: usize,
    ) -> usize {
        // The plan was validated against `previous_size`, so that - not this
        // column's current element size - is what the scratch copy and the
        // source stride have to be. A disagreement is an internal break: the
        // world-level relayout verifies every column before calling here.
        debug_assert_eq!(
            previous_size, self.layout.size,
            "a column's element size disagrees with the layout its plan was validated against"
        );
        let rows = self.len;
        let old_size = previous_size;
        let old_align = self.layout.align;
        let old_data = self.data;
        let in_place = layout.size == old_size && layout.align == old_align;

        // Step 1: Prepare the destination buffer. An unchanged shape keeps the
        // buffer it has; anything else gets a fresh one at the same capacity,
        // because a relayout is not a reason to re-grow on the next push.
        let mut new_data = old_data;
        if !in_place {
            new_data = if self.capacity == 0 {
                NonNull::dangling()
            } else {
                let bytes = layout
                    .size
                    .checked_mul(self.capacity)
                    .expect("dynamic column too large");
                let allocation = Layout::from_size_align(bytes, layout.align)
                    .expect("dynamic layout validated by the caller");
                // SAFETY: the allocation has non-zero size, checked above.
                let pointer = unsafe { alloc(allocation) };
                NonNull::new(pointer).unwrap_or_else(|| handle_alloc_error(allocation))
            };
        }

        // Step 2: Transform one row at a time. The scratch copy is what makes
        // this correct in place as well as across buffers: every source byte is
        // read before the destination row is zeroed.
        if rows != 0 {
            let mut scratch = vec![0_u8; old_size.max(layout.size)];
            for row in 0..rows {
                // SAFETY: `row < rows` counts only initialized rows, and both
                // buffers are allocated for at least `rows` rows.
                unsafe {
                    let source = old_data.as_ptr().add(row * old_size);
                    std::ptr::copy_nonoverlapping(source, scratch.as_mut_ptr(), old_size);
                    let destination = new_data.as_ptr().add(row * layout.size);
                    std::ptr::write_bytes(destination, 0, layout.size);
                    for field in plan.fields() {
                        let FieldSource::OldOffset(offset) = field.source else {
                            continue;
                        };
                        std::ptr::copy_nonoverlapping(
                            scratch.as_ptr().add(offset),
                            destination.add(field.offset),
                            field.bytes,
                        );
                    }
                }
            }
        }

        // Step 3: Release the buffer that is no longer the column's, and adopt
        // the new shape. `capacity != 0` is the same "is there an allocation"
        // test `Drop` uses, so a column that never allocated never reaches
        // `dealloc`.
        if !in_place {
            if self.capacity != 0 {
                // SAFETY: this is the live allocation, created with this layout.
                unsafe {
                    dealloc(
                        old_data.as_ptr(),
                        Layout::from_size_align_unchecked(old_size * self.capacity, old_align),
                    );
                }
            }
            self.data = new_data;
        }
        self.layout = layout;
        rows
    }

    /// Removes the row at `index` by swapping in the last row.
    ///
    /// Keeps the column dense and runs in O(1), but does not preserve row
    /// ordering.
    ///
    /// # Panics
    ///
    /// Panics when `index` is out of bounds.
    pub fn swap_remove_discard(&mut self, index: usize) {
        assert!(index < self.len);
        // Run the release hook, if one was registered, before the row's bytes
        // are overwritten by the swap-in. With no hook this is the same pure
        // byte move it has always been.
        if !self.ops.trivial_drop {
            // SAFETY: index < len, so the row is initialized and addressable.
            unsafe { (self.ops.drop_range)(self.data.as_ptr().add(index * self.layout.size), 1) };
        }
        let last = self.len - 1;
        if index != last {
            // SAFETY: both rows are within this allocation; copy permits overlap.
            unsafe {
                std::ptr::copy(
                    self.data.as_ptr().add(last * self.layout.size),
                    self.data.as_ptr().add(index * self.layout.size),
                    self.layout.size,
                );
            }
        }
        self.len = last;
    }

    /// Ensures capacity for at least one more row, growing the buffer when full.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DynamicLayoutInvalid`] when the doubled capacity
    /// or the allocation layout it implies cannot be represented. Both used to
    /// panic instead, which a caller could not route around.
    fn reserve_one(&mut self) -> Result<(), WorldError> {
        // Step 1: Early-exit when the column still has a spare slot.
        if self.len < self.capacity {
            return Ok(());
        }

        // Step 2: Compute the doubled capacity and hand the rest to `grow_to`.
        //
        // The floor is applied *after* doubling. Applying it before made the
        // first allocation eight rows rather than four, over-allocating every
        // manifest-driven column on first use.
        let new_capacity = self
            .capacity
            .checked_mul(2)
            .ok_or(WorldError::DynamicLayoutInvalid)?
            .max(4);
        self.grow_to(new_capacity)
    }

    /// Reallocate the buffer to hold exactly `new_capacity` rows.
    ///
    /// The single growth implementation: [`Self::reserve_one`] picks the
    /// doubling policy and [`Self::reserve_rows`] picks an exact target, but
    /// both allocate and migrate through here, so the two can never disagree
    /// about how a buffer is moved.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DynamicLayoutInvalid`] when the capacity or the
    /// allocation layout it implies cannot be represented.
    fn grow_to(&mut self, new_capacity: usize) -> Result<(), WorldError> {
        if new_capacity <= self.capacity {
            return Ok(());
        }
        let new_layout = Layout::from_size_align(
            self.layout
                .size
                .checked_mul(new_capacity)
                .ok_or(WorldError::DynamicLayoutInvalid)?,
            self.layout.align,
        )
        .map_err(|_| WorldError::DynamicLayoutInvalid)?;

        // Step 3: Allocate the new buffer and migrate the existing rows.
        // SAFETY: new_layout has non-zero size because component size is validated.
        let new_data = unsafe { alloc(new_layout) };
        let new_data = NonNull::new(new_data).unwrap_or_else(|| handle_alloc_error(new_layout));
        if self.len != 0 {
            // SAFETY: both allocations are valid and non-overlapping.
            unsafe {
                std::ptr::copy_nonoverlapping(
                    self.data.as_ptr(),
                    new_data.as_ptr(),
                    self.len * self.layout.size,
                );
                dealloc(
                    self.data.as_ptr(),
                    Layout::from_size_align_unchecked(
                        self.layout.size * self.capacity,
                        self.layout.align,
                    ),
                );
            }
        }

        // Step 4: Adopt the new allocation as the column's buffer.
        self.data = new_data;
        self.capacity = new_capacity;
        Ok(())
    }
}

// SAFETY: Two premises, each enforced by named code rather than asserted here.
//
// 1. Every field of a dynamic component is a blittable value type. The
//    evidence is held by the engine: every `DynamicComponentLayout` carries a
//    `Blittability` witness, the host builds its own only after
//    `BLITTABLE_FIELD_TYPES` vetted the manifest's fields, and no column can
//    be constructed without a layout. This is what makes the raw `ptr::copy`
//    in `swap_remove` and the destructor-free paths below correct: there is no
//    ownership to duplicate or release.
// 2. Access is serialised by the same scheduler rules as native columns - see
//    `SystemAccess::conflicts_with`, which only takes its bitmask fast path when
//    both systems' access masks are complete.
unsafe impl Send for DynamicColumn {}
// SAFETY: Shared access to a column only permits reading rows, which is sound
// under the same two premises as `Send` above: rows are blittable value types
// and access is serialised by the scheduler, so no write can race a read.
unsafe impl Sync for DynamicColumn {}

impl Drop for DynamicColumn {
    /// Releases every row through the column's ops table, then frees the
    /// buffer itself.
    ///
    /// One release path for both lanes. A descriptor column's table reports
    /// `trivial_drop`, so this is the destructor-free teardown the POD design
    /// promises and the loop is skipped outright; a native column's table runs
    /// the element type's own glue.
    ///
    /// Which table that is matters during a reload: a column is re-pointed at
    /// the *retiring* generation's table before it is dropped, so old rows are
    /// released by the code that allocated them rather than by the arriving
    /// generation's.
    fn drop(&mut self) {
        if !self.ops.trivial_drop && self.len != 0 {
            // SAFETY: rows 0..len are initialized, correctly aligned for the
            // element type, and the table is the one that wrote them.
            unsafe { (self.ops.drop_range)(self.data.as_ptr(), self.len) };
        }
        if self.capacity != 0 {
            // SAFETY: this is the live allocation created by reserve_one.
            unsafe {
                dealloc(
                    self.data.as_ptr(),
                    Layout::from_size_align_unchecked(
                        self.layout.size * self.capacity,
                        self.layout.align,
                    ),
                );
            }
        }
    }
}

// =============================================================================
// ArchetypeId
// =============================================================================

/// ArchetypeId uniquely identifies an archetype by its component mask.
///
/// Derived from the archetype's [`ComponentMask`], guaranteeing a 1:1
/// mapping without a separate lookup table.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ArchetypeId(
    /// The packed component mask bits identifying this archetype.
    pub u128,
);

// =============================================================================
// Archetype
// =============================================================================

/// Core storage unit grouping entities with the same component set.
///
/// Uses a Structure of Arrays (SoA) layout for cache-friendly bulk iteration.
pub struct Archetype {
    /// Unique identifier derived from this archetype's component mask.
    pub id: ArchetypeId,
    /// Component types stored in this archetype, in registration order.
    ///
    /// Still needed for iteration and lookup.
    pub component_types: Vec<ComponentId>,
    /// Bitmask of the stored component types for fast query matching.
    pub component_mask: ComponentMask,
    /// Component storage, keyed by component id.
    ///
    /// One map for both lanes. A column built for a Rust type and one built
    /// from a manifest descriptor are the same type - bytes plus a descriptor
    /// plus an ops table - so there is nothing left for a second map to hold
    /// apart. Which constructor built a column is recorded in its identity,
    /// not in where it is stored.
    pub component_storages: ComponentColumns,
    /// Entities currently stored in this archetype.
    pub entities: Vec<Entity>,
    /// Per-component-instance change-detection metadata.
    ///
    /// For each `ComponentId` in `component_types`, the matching
    /// `Vec<ComponentTicks>` is kept in lockstep with the underlying
    /// component storage: row `i` of the tick vec corresponds to row `i`
    /// of the component vec for the same entity. Maintenance happens in
    /// `World` whenever entities are inserted, moved between archetypes,
    /// or destroyed.
    pub component_ticks: HashMap<ComponentId, Vec<ComponentTicks>>,
}

impl Archetype {
    /// Creates a new archetype with storage for the specified component types.
    ///
    /// `storage_factories` provides a way to create storage for each component
    /// type by [`ComponentId`], allowing archetype creation without knowing the
    /// concrete types.
    ///
    /// # Panics
    ///
    /// Panics when a component type has no entry in `storage_factories`, which
    /// happens when `world.register_component::<T>()` was not called for it.
    pub fn new(
        id: ArchetypeId,
        component_types: Vec<ComponentId>,
        component_mask: ComponentMask,
        storage_factories: &HashMap<ComponentId, StorageFactory>,
    ) -> Self {
        // Step 1: Pre-size the storage maps for the component count.
        let component_count = component_types.len();
        let _zone = crate::profile_scope!(
            "archetype new",
            [("Component types in this archetype: {}", component_count)]
        );
        let mut component_storages = ComponentColumns::with_capacity(component_count);
        let mut component_ticks: HashMap<ComponentId, Vec<ComponentTicks>> =
            HashMap::with_capacity(component_count);

        // Step 2: Create storage for each component type using its factory.
        for &component_id in &component_types {
            let factory = storage_factories.get(&component_id)
                .unwrap_or_else(|| panic!(
                    "Component type {:?} not registered. Call world.register_component::<T>() first.",
                    component_id
                ));
            match factory {
                StorageFactory::Native(info) => {
                    // Build the erased column from the registered type
                    // description. It is stored as a concrete
                    // `DynamicColumn` (no trait-object vtable), so the
                    // column stays valid across module unloads, and under the
                    // component's id rather than its `TypeId`, so a component
                    // shared between binaries resolves to it from either one.
                    component_storages.insert(
                        component_id,
                        DynamicColumn::from_native_info(*info, 0)
                            .expect("a registered native layout must describe an allocation"),
                    );
                }
                StorageFactory::Dynamic(layout) => {
                    // Both paths into the registry - `register_dynamic_component`
                    // and `relayout_dynamic_component` - validate the layout
                    // before it is stored, so the column can only echo that
                    // verdict here.
                    let column = DynamicColumn::new(layout.clone())
                        .expect("registration validated this layout");
                    component_storages.insert(component_id, column);
                }
            }
            component_ticks.insert(component_id, Vec::new());
        }

        // Step 3: Emit allocation telemetry and assemble the archetype.
        crate::profile_message!(
            "archetype {:?} allocated with {} component storage columns for up to 0 entities",
            id,
            component_count,
        );

        Self {
            id,
            component_types,
            component_mask,
            component_storages,
            entities: Vec::new(),
            component_ticks,
        }
    }

    /// Checks whether this archetype stores the component type at the given mask bit.
    ///
    /// Uses the bitmask for O(1) lookup instead of a linear search through
    /// the component types.
    #[inline]
    pub fn has_component_bit(&self, bit: u8) -> bool {
        self.component_mask.has_bit(bit)
    }

    /// Checks whether this archetype stores the specified component type.
    ///
    /// Note: This uses an O(n) linear search. Prefer `has_component_bit` with
    /// a pre-looked-up bit index for hot paths.
    pub fn has_component<T: Component>(&self) -> bool {
        self.component_types.contains(&ComponentId::of::<T>())
    }

    /// Checks whether this archetype contains every component a query requires.
    ///
    /// Called in the query setup hot path for every archetype.
    #[inline]
    pub fn matches_mask(&self, required_mask: &ComponentMask) -> bool {
        self.component_mask.contains_all(required_mask)
    }

    /// Returns the number of entities in this archetype.
    #[inline]
    pub fn len(&self) -> usize {
        self.entities.len()
    }

    /// Returns the number of entities in this archetype (alias for [`Self::len`]).
    ///
    /// Provided for API consistency with other collection types.
    #[inline]
    pub fn entity_count(&self) -> usize {
        self.entities.len()
    }

    /// Returns `true` when this archetype contains no entities.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.entities.is_empty()
    }

    /// Builds a human-readable summary of this archetype.
    ///
    /// Includes the archetype ID, the entity count, and the names of the
    /// stored component types. Unknown component IDs render as "Unknown".
    pub fn get_archetype_info(&self, registry: &crate::component::ComponentRegistry) -> String {
        let component_names: Vec<String> = self
            .component_types
            .iter()
            .map(|component_id| {
                registry
                    .get_name(component_id)
                    .unwrap_or("Unknown")
                    .to_string()
            })
            .collect();

        format!(
            "Archetype {:?}: {} entities, components: [{}]",
            self.id,
            self.entities.len(),
            component_names.join(", ")
        )
    }

    /// Prints information about this archetype (component names and entity count).
    #[cold]
    pub fn print_info(&self, registry: &crate::component::ComponentRegistry) {
        let info = self.get_archetype_info(registry);
        println!("{}", info);
    }

    /// Estimate the memory footprint of this archetype in bytes.
    ///
    /// Sums entity IDs, component column capacities x element sizes,
    /// and change-detection tick vectors.
    pub fn memory_estimate(&self, registry: &crate::component::ComponentRegistry) -> usize {
        // Step 1: Account for the entity ID array (16 bytes per entity).
        let mut total = self.entities.capacity() * std::mem::size_of::<crate::entity::Entity>();

        // Step 2: Account for each component column (entity count as a lower bound).
        for &component_id in &self.component_types {
            if let Some(size) = registry.get_size(&component_id) {
                // We can't inspect the Vec's capacity through the trait object,
                // so we use entity count as a lower bound. The actual Vec capacity
                // may be larger due to pre-allocation.
                total += self.entities.len() * size;
            }
        }

        // Step 3: Account for the change-detection tick vectors (8 bytes per tick).
        total += self.component_types.len() * self.entities.capacity() * 8;

        // Step 4: Approximate the HashMap overhead for the tick lookup.
        total += self.component_types.len() * 64;

        total
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// One row of a two-field layout: `a` at 0, `b` at 4.
    fn two_fields() -> DynamicComponentLayout {
        DynamicComponentLayout {
            size: 8,
            align: 4,
            schema_hash: 1,
            blittability: Blittability::engine_verified(),
        }
    }

    fn column_with_rows(rows: &[[u8; 8]]) -> DynamicColumn {
        let mut column = DynamicColumn::new(two_fields()).expect("a valid layout");
        for row in rows {
            column.push_bytes(row).expect("row matches the layout");
        }
        column
    }

    /// A layout carries its witness, and a layout with a release hook has that
    /// hook run once per live row - by `Drop` and by `swap_remove`.
    #[test]
    fn a_layout_keeps_its_witness_and_a_column_releases_through_its_ops() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let constructed =
            DynamicComponentLayout::new(4, 4, 7, Blittability::from_manifest_fields())
                .expect("a plain layout");
        assert_eq!(
            constructed.blittability,
            Blittability::from_manifest_fields(),
            "the constructor stores the witness it was handed"
        );

        // Release is the ops table's job now, not a hook on the layout: one
        // path serves a descriptor column (which reports `trivial_drop` and is
        // skipped) and a native one (which runs the element type's glue).
        // Counting calls holds both call sites - `Drop` and
        // `swap_remove_discard` - to one release per row.
        static RELEASES: AtomicUsize = AtomicUsize::new(0);
        unsafe fn count_releases(_rows: *mut u8, count: usize) {
            RELEASES.fetch_add(count, Ordering::SeqCst);
        }

        let layout = DynamicComponentLayout::new(4, 4, 7, Blittability::from_manifest_fields())
            .expect("a valid layout");
        let mut column = DynamicColumn::new(layout).expect("a column");
        column.refresh_ops(ColumnOps {
            drop_range: count_releases,
            trivial_drop: false,
        });

        column
            .push_zeroed()
            .expect("the first row grows the column");
        column.push_zeroed().expect("the second row fits");
        column.swap_remove_discard(0);
        assert_eq!(
            RELEASES.load(Ordering::SeqCst),
            1,
            "swap_remove_discard releases the row it overwrites"
        );

        drop(column);
        assert_eq!(
            RELEASES.load(Ordering::SeqCst),
            2,
            "Drop releases the one row still live"
        );
    }

    #[test]
    fn plan_between_matches_fields_by_name_not_by_position() {
        let old = [
            LayoutField {
                name: "a",
                type_tag: "u32",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "b",
                type_tag: "u32",
                offset: 4,
                size: 4,
            },
        ];
        // The same two fields, swapped in the new layout.
        let new = [
            LayoutField {
                name: "b",
                type_tag: "u32",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "a",
                type_tag: "u32",
                offset: 4,
                size: 4,
            },
        ];

        let plan = DynamicFieldPlan::between(&old, &new);

        assert_eq!(
            plan.fields(),
            &[
                PlannedField {
                    offset: 0,
                    bytes: 4,
                    source: FieldSource::OldOffset(4),
                },
                PlannedField {
                    offset: 4,
                    bytes: 4,
                    source: FieldSource::OldOffset(0),
                },
            ]
        );
    }

    #[test]
    fn plan_between_zeroes_the_new_fields_and_omits_the_removed_ones() {
        let old = [
            LayoutField {
                name: "kept",
                type_tag: "u32",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "removed",
                type_tag: "u32",
                offset: 4,
                size: 4,
            },
        ];
        let new = [
            LayoutField {
                name: "kept",
                type_tag: "u32",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "added",
                type_tag: "u32",
                offset: 4,
                size: 8,
            },
        ];

        let plan = DynamicFieldPlan::between(&old, &new);

        // The removed field leaves no instruction at all, and the added one is
        // an explicit zero fill rather than a missing entry.
        assert_eq!(
            plan.fields(),
            &[
                PlannedField {
                    offset: 0,
                    bytes: 4,
                    source: FieldSource::OldOffset(0),
                },
                PlannedField {
                    offset: 4,
                    bytes: 8,
                    source: FieldSource::ZeroFill,
                },
            ]
        );
    }

    #[test]
    fn plan_between_copies_the_smaller_of_two_sizes() {
        let old = [LayoutField {
            name: "grew",
            type_tag: "u32",
            offset: 0,
            size: 4,
        }];
        let new = [LayoutField {
            name: "grew",
            type_tag: "u32",
            offset: 0,
            size: 8,
        }];
        assert_eq!(
            DynamicFieldPlan::between(&old, &new).fields()[0].bytes,
            4,
            "the tail of a grown field has to come from the zero fill, not the old row"
        );

        let old = [LayoutField {
            name: "shrank",
            type_tag: "u32",
            offset: 0,
            size: 8,
        }];
        let new = [LayoutField {
            name: "shrank",
            type_tag: "u32",
            offset: 0,
            size: 4,
        }];
        assert_eq!(DynamicFieldPlan::between(&old, &new).fields()[0].bytes, 4);
    }

    #[test]
    fn relayout_moves_rows_through_the_plan() {
        // Two rows, `a` and `b` each holding a distinct u32.
        let mut column = column_with_rows(&[[1, 0, 0, 0, 2, 0, 0, 0], [3, 0, 0, 0, 4, 0, 0, 0]]);
        let old = [
            LayoutField {
                name: "a",
                type_tag: "u32",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "b",
                type_tag: "u32",
                offset: 4,
                size: 4,
            },
        ];
        // `b` first, then `a`, then a new eight-byte field.
        let new = [
            LayoutField {
                name: "b",
                type_tag: "u32",
                offset: 0,
                size: 4,
            },
            LayoutField {
                name: "a",
                type_tag: "u32",
                offset: 4,
                size: 4,
            },
            LayoutField {
                name: "added",
                type_tag: "u32",
                offset: 8,
                size: 8,
            },
        ];
        let plan = DynamicFieldPlan::between(&old, &new);

        let rows = column
            .relayout(
                DynamicComponentLayout {
                    size: 16,
                    align: 8,
                    schema_hash: 2,
                    blittability: Blittability::engine_verified(),
                },
                &plan,
            )
            .expect("the plan fits both layouts");

        assert_eq!(rows, 2);
        assert_eq!(
            column.bytes(0).unwrap(),
            [2, 0, 0, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0].as_slice()
        );
        assert_eq!(
            column.bytes(1).unwrap(),
            [4, 0, 0, 0, 3, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0].as_slice()
        );
        assert_eq!(column.element_size(), 16);
        assert_eq!(column.schema_hash(), 2);
    }

    #[test]
    fn relayout_of_an_equal_shape_rewrites_rows_in_place() {
        let mut column = column_with_rows(&[[1, 0, 0, 0, 2, 0, 0, 0]]);
        let before = column.as_mut_ptr();
        let plan = DynamicFieldPlan::between(
            &[
                LayoutField {
                    name: "a",
                    type_tag: "u32",
                    offset: 0,
                    size: 4,
                },
                LayoutField {
                    name: "b",
                    type_tag: "u32",
                    offset: 4,
                    size: 4,
                },
            ],
            &[
                LayoutField {
                    name: "b",
                    type_tag: "u32",
                    offset: 0,
                    size: 4,
                },
                LayoutField {
                    name: "a",
                    type_tag: "u32",
                    offset: 4,
                    size: 4,
                },
            ],
        );

        column
            .relayout(
                DynamicComponentLayout {
                    size: 8,
                    align: 4,
                    schema_hash: 3,
                    blittability: Blittability::engine_verified(),
                },
                &plan,
            )
            .expect("an equal-shape relayout of a fitting plan");

        assert_eq!(
            column.as_mut_ptr(),
            before,
            "an unchanged shape must not reallocate: pointers into it may be live"
        );
        // The scratch copy is what makes this correct: `b` overwrites `a`'s
        // bytes before `a` has been read out of the same buffer.
        assert_eq!(
            column.bytes(0).unwrap(),
            [2, 0, 0, 0, 1, 0, 0, 0].as_slice()
        );
    }

    #[test]
    fn relayout_keeps_the_row_count_and_capacity() {
        let mut column = column_with_rows(&[[1, 0, 0, 0, 2, 0, 0, 0], [3, 0, 0, 0, 4, 0, 0, 0]]);
        let plan = DynamicFieldPlan::new();

        column
            .relayout(
                DynamicComponentLayout {
                    size: 4,
                    align: 4,
                    schema_hash: 4,
                    blittability: Blittability::engine_verified(),
                },
                &plan,
            )
            .expect("an empty plan always fits");

        assert_eq!(column.len(), 2);
        assert_eq!(column.bytes(0).unwrap(), [0, 0, 0, 0].as_slice());
        assert_eq!(column.bytes(1).unwrap(), [0, 0, 0, 0].as_slice());
        // Capacity was kept, so the next push still has its spare slot.
        column
            .push_bytes(&[9, 0, 0, 0])
            .expect("fits the kept capacity");
        assert_eq!(column.len(), 3);
    }

    #[test]
    fn relayout_refuses_a_plan_that_leaves_a_row() {
        let mut column = column_with_rows(&[[1, 0, 0, 0, 2, 0, 0, 0]]);
        let mut plan = DynamicFieldPlan::new();
        plan.push(4, 8, FieldSource::OldOffset(0));

        let result = column.relayout(
            DynamicComponentLayout {
                size: 8,
                align: 4,
                schema_hash: 1,
                blittability: Blittability::engine_verified(),
            },
            &plan,
        );

        assert!(matches!(result, Err(WorldError::DynamicRowInvalid)));
        assert_eq!(
            column.bytes(0).unwrap(),
            [1, 0, 0, 0, 2, 0, 0, 0].as_slice(),
            "a refused plan must not touch the rows"
        );
    }

    #[test]
    fn relayout_refuses_a_layout_that_cannot_be_allocated() {
        let mut column = column_with_rows(&[]);
        assert!(matches!(
            column.relayout(
                DynamicComponentLayout {
                    size: 0,
                    align: 4,
                    schema_hash: 1,
                    blittability: Blittability::engine_verified()
                },
                &DynamicFieldPlan::new()
            ),
            Err(WorldError::DynamicSizeZero)
        ));
        assert!(matches!(
            column.relayout(
                DynamicComponentLayout {
                    size: 4,
                    align: 3,
                    schema_hash: 1,
                    blittability: Blittability::engine_verified()
                },
                &DynamicFieldPlan::new()
            ),
            Err(WorldError::DynamicAlignmentInvalid)
        ));
    }
    // =========================================================================
    // Unified column: identity, ops and typed access
    // =========================================================================

    /// A native element type with real drop glue, so a POD column and a typed
    /// one can be told apart by behaviour rather than by construction.
    #[derive(Debug, PartialEq)]
    struct DropCounted(u32);

    thread_local! {
        static DROPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    }

    impl Drop for DropCounted {
        fn drop(&mut self) {
            DROPS.with(|count| count.set(count.get() + 1));
        }
    }

    /// A native column round-trips typed values through the byte buffer.
    #[test]
    fn a_native_column_pushes_and_reads_typed_rows() {
        let mut column = DynamicColumn::new_native::<u64>(7, false).expect("layout");
        column.push::<u64>(11);
        column.push::<u64>(22);

        assert_eq!(column.len(), 2);
        assert_eq!(*column.get::<u64>(0), 11);
        assert_eq!(*column.get::<u64>(1), 22);
        assert_eq!(column.as_slice::<u64>(), &[11, 22]);

        *column.get_mut::<u64>(0) = 33;
        assert_eq!(*column.get::<u64>(0), 33);
        assert_eq!(column.swap_remove::<u64>(0), 33);
        assert_eq!(column.as_slice::<u64>(), &[22]);
    }

    /// The element-type check refuses a `T` the column does not hold.
    #[test]
    #[should_panic(expected = "different component type")]
    fn a_native_column_refuses_a_foreign_type() {
        let column = DynamicColumn::new_native::<u64>(7, false).expect("layout");
        let _ = column.as_slice::<u32>();
    }

    /// A descriptor-only column has no typed reading at all.
    #[test]
    #[should_panic(expected = "descriptor-only column has no Rust type")]
    fn a_descriptor_column_refuses_typed_access() {
        let layout =
            DynamicComponentLayout::new(8, 4, 7, Blittability::engine_verified()).expect("layout");
        let column = DynamicColumn::new(layout).expect("column");
        let _ = column.as_slice::<u64>();
    }

    /// A shared column checks layout instead of `TypeId`, because the two
    /// binaries that reach it have different ids for one type.
    #[test]
    fn a_shared_column_accepts_a_layout_compatible_type() {
        let mut column = DynamicColumn::new_native::<u64>(7, true).expect("layout");
        column.push::<u64>(5);
        // A different type of the same shape is what the second binary's `T`
        // looks like from here.
        assert_eq!(*column.get::<i64>(0), 5);
    }

    /// ...and still refuses one whose layout disagrees.
    #[test]
    #[should_panic(expected = "shared column layout")]
    fn a_shared_column_refuses_a_layout_mismatch() {
        let column = DynamicColumn::new_native::<u64>(7, true).expect("layout");
        let _ = column.as_slice::<u32>();
    }

    /// The generic ops table reports that there is nothing to drop, and the
    /// generated one reports the truth about its element type.
    #[test]
    fn ops_report_the_drop_behaviour_of_their_lane() {
        let layout =
            DynamicComponentLayout::new(4, 4, 7, Blittability::engine_verified()).expect("layout");
        assert!(
            DynamicColumn::new(layout)
                .expect("column")
                .ops()
                .trivial_drop
        );
        assert!(
            DynamicColumn::new_native::<u64>(7, false)
                .expect("column")
                .ops()
                .trivial_drop
        );
        assert!(
            !DynamicColumn::new_native::<DropCounted>(7, false)
                .expect("column")
                .ops()
                .trivial_drop
        );
    }

    /// The generated table drops exactly the rows it is given, once each.
    #[test]
    fn generated_ops_drop_each_row_once() {
        let mut column = DynamicColumn::new_native::<DropCounted>(7, false).expect("column");
        column.push(DropCounted(1));
        column.push(DropCounted(2));
        DROPS.with(|count| count.set(0));

        let ops = column.ops();
        // SAFETY: the column holds exactly two initialized `DropCounted` rows,
        // and nothing reads them afterwards.
        unsafe { (ops.drop_range)(column.as_mut_ptr(), 2) };
        assert_eq!(DROPS.with(std::cell::Cell::get), 2);

        // The rows were consumed by hand, so the column must not drop them again.
        std::mem::forget(column);
    }

    /// Reserving space does not change what the column contains.
    #[test]
    fn reserving_rows_preserves_contents() {
        let mut column = DynamicColumn::new_native::<u32>(7, false).expect("column");
        column.push::<u32>(9);
        column.reserve_rows(64).expect("reserve");
        assert!(column.capacity >= 65);
        assert_eq!(column.as_slice::<u32>(), &[9]);
        column.push::<u32>(10);
        assert_eq!(column.as_slice::<u32>(), &[9, 10]);
    }
    /// A descriptor column that gains alignment reallocates rather than
    /// reinterpreting its old stride.
    ///
    /// This is the descriptor-lane counterpart of audit item 4.18, and it is
    /// why that lane needs no registration-time refusal of its own. The native
    /// hazard was a column re-homed onto a newer ops table and then *read
    /// through the incoming type* at the old stride. A descriptor column has no
    /// typed reading at all, and a relayout that changes size or alignment
    /// moves every row into a freshly allocated, correctly aligned buffer, so
    /// the misaligned read the guard exists to prevent is unrepresentable here.
    #[test]
    fn a_descriptor_relayout_that_widens_alignment_reallocates() {
        // Two 4-byte fields, align 4 - the shape audit 4.18 widened.
        let old =
            DynamicComponentLayout::new(12, 4, 1, Blittability::engine_verified()).expect("layout");
        let mut column = DynamicColumn::new(old).expect("column");
        column
            .push_bytes(&[1, 0, 0, 0, 2, 0, 0, 0, 3, 0, 0, 0])
            .expect("push");
        column
            .push_bytes(&[4, 0, 0, 0, 5, 0, 0, 0, 6, 0, 0, 0])
            .expect("push");

        // Widen to align 8, the case the native lane refuses.
        let new =
            DynamicComponentLayout::new(16, 8, 2, Blittability::engine_verified()).expect("layout");
        let mut plan = DynamicFieldPlan::new();
        plan.push(0, 4, FieldSource::OldOffset(0));
        plan.push(8, 4, FieldSource::OldOffset(4));
        let migrated = column.relayout(new, &plan).expect("relayout");

        assert_eq!(migrated, 2, "both rows migrate");
        assert_eq!(column.len(), 2, "row count and order survive");
        assert_eq!(column.alignment(), 8);
        assert_eq!(
            column.as_mut_ptr() as usize % 8,
            0,
            "the new buffer really is 8-aligned, which is what the native lane could not promise"
        );
        assert_eq!(&column.bytes(0).expect("row")[0..4], &[1, 0, 0, 0]);
        assert_eq!(&column.bytes(0).expect("row")[8..12], &[2, 0, 0, 0]);
        assert_eq!(&column.bytes(1).expect("row")[0..4], &[4, 0, 0, 0]);
    }
    /// A field that keeps its name but changes type is reset, not reinterpreted.
    #[test]
    fn plan_between_resets_a_field_whose_type_changed() {
        let old = [LayoutField {
            name: "health",
            type_tag: "i32",
            offset: 0,
            size: 4,
        }];
        let new = [LayoutField {
            name: "health",
            type_tag: "f32",
            offset: 0,
            size: 4,
        }];

        let plan = DynamicFieldPlan::between(&old, &new);

        // Copying would have carried the bits across: `5i32` read as `f32` is
        // 7e-45, a different number wearing the same name.
        assert_eq!(
            plan.fields(),
            &[PlannedField {
                offset: 0,
                bytes: 4,
                source: FieldSource::ZeroFill,
            }],
            "a retyped field takes the zero fill, not the old bytes"
        );
        assert_eq!(
            plan.retyped_fields(),
            &["health".to_string()],
            "and the reset is recorded so the caller can report it"
        );
    }

    /// A field that keeps its name and type still carries its bytes over.
    #[test]
    fn plan_between_copies_a_field_whose_type_is_unchanged() {
        let old = [LayoutField {
            name: "health",
            type_tag: "i32",
            offset: 8,
            size: 4,
        }];
        let new = [LayoutField {
            name: "health",
            type_tag: "i32",
            offset: 0,
            size: 4,
        }];

        let plan = DynamicFieldPlan::between(&old, &new);

        assert_eq!(
            plan.fields(),
            &[PlannedField {
                offset: 0,
                bytes: 4,
                source: FieldSource::OldOffset(8),
            }],
            "a moved field follows its name"
        );
        assert!(
            plan.retyped_fields().is_empty(),
            "nothing was reset, so nothing is reported"
        );
    }
}
