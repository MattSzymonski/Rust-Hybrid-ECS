//! Singleton resources stored in the [`World`], not attached to entities.
//!
//! # Responsibilities
//!
//! - Defines the [`Resource`] marker trait for global/shared state types.
//! - Provides [`ResourceId`], which names a resource type and stays the same
//!   across artifacts when the type declares a shared name.
//! - Implements [`ResHandle`] - a lightweight, copyable handle for deferred resource access.
//! - Owns [`ErasedResource`], the storage one resource value lives in.
//!
//! # Design
//!
//! A resource holds global state: time, input, configuration, an asset store.
//! Systems reach it through `Res<T>` (read-only) and `ResMut<T>` (read and
//! write) parameters, and [`ResHandle<T>`] is a zero-cost typed handle that
//! can be stored and passed around without borrowing the [`World`].
//!
//! # Examples
//!
//! ```no_run
//! # use pill_engine::*;
//! # let mut world = World::new();
//! #[derive(Debug)]
//! struct ProjectTime { delta_seconds: f32, elapsed_seconds: f32 }
//! impl Resource for ProjectTime {}
//! world.insert_resource(ProjectTime { delta_seconds: 0.016, elapsed_seconds: 0.0 });
//!
//! // A handle is cheap and copyable.
//! let handle = ResHandle::<ProjectTime>::new();
//! let time = handle.get(&world).unwrap();
//!
//! // Systems take the same resource through Res/ResMut.
//! fn my_system(time: Res<ProjectTime>) {
//!     println!("Elapsed: {}", time.get().unwrap().elapsed_seconds);
//! }
//! ```

// Standard library
use std::alloc::Layout;
use std::any::TypeId;
use std::marker::PhantomData;
use std::ptr::NonNull;

// Current crate
use pill_core::error;

use crate::archetype::FieldPlan;
use crate::error::WorldError;
use crate::world::World;

// =============================================================================
// Resource
// =============================================================================

/// Marks a type as a resource: one value of it lives in the [`World`].
///
/// Unlike components, resources are not attached to entities. They hold
/// global state such as time, input or configuration, and must be
/// `Send + Sync` so systems can reach them in parallel.
///
/// # Examples
///
/// ```
/// use pill_engine::Resource;
///
/// struct ProjectTime {
///     delta_seconds: f32,
///     elapsed_seconds: f32,
/// }
///
/// impl Resource for ProjectTime {}
/// ```
pub trait Resource: Send + Sync + 'static {
    /// A stable name for this resource, or `None` to be identified by its
    /// [`TypeId`] inside this binary only.
    ///
    /// This is the resource counterpart of
    /// [`Component::shared_name`](crate::component::Component::shared_name),
    /// and it exists for the same reason: when two artifacts compile the type
    /// with different inputs, each gets its own [`TypeId`], and the same type
    /// would become two resources that cannot see each other. A shared name
    /// gives both artifacts one identity.
    ///
    /// The name must be unique across the whole process, so namespace it -
    /// `"pill_spline::SplineSettings"`, not `"Settings"`.
    ///
    /// Most resources do not need this: a type every artifact compiles
    /// identically already has the same `TypeId` everywhere, which is why
    /// `Time` and `AssetManager` cross the boundary untouched.
    fn shared_name() -> Option<&'static str>
    where
        Self: Sized,
    {
        None
    }

    /// The stable identity that [`Self::shared_name`] turns into, or `None`.
    ///
    /// The default derives it from `shared_name`, so an implementor writes
    /// only the name and the two cannot disagree. Override it only with
    /// `shared_component_identity(Self::shared_name())`: any other value would
    /// give this resource an identity different from the one every other
    /// artifact derives from the same name.
    fn shared_identity() -> Option<u128>
    where
        Self: Sized,
    {
        Self::shared_name().map(crate::component::shared_component_identity)
    }

    /// A hash of this resource's field layout, or `None`.
    ///
    /// This is the resource counterpart of a component's schema hash. Size and
    /// alignment cannot tell `{u32, u32}` from `{f32, f32}`, so a shared
    /// resource whose declaring types have different field types is caught
    /// here instead of being read through the wrong `T`.
    ///
    /// The default is `None`, and a claim compares the hash only when both
    /// sides carry one, so a type that does not need the check does not have
    /// to make one up. When the type has a field vocabulary, derive the value
    /// with [`component_schema_hash`](crate::component::component_schema_hash).
    fn shared_schema_hash() -> Option<u64>
    where
        Self: Sized,
    {
        None
    }
}

// =============================================================================
// ResourceId
// =============================================================================

/// Identifies one resource type.
///
/// This is either the Rust [`TypeId`], or, for a resource that declares
/// [`Resource::shared_name`], a stable identity derived from that name, so
/// artifacts that compiled the type with different inputs still agree.
///
/// `Hash` is written by hand below rather than derived; the impl says why.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ResourceId {
    /// Resource identified by its Rust [`TypeId`], which differs between
    /// artifacts that compiled the type with different inputs.
    Native(TypeId),
    /// Resource identified by the stable name it declares, which every
    /// artifact computes identically.
    Shared(u128),
}

impl ResourceId {
    /// Returns the [`ResourceId`] identifying the resource type `T`.
    ///
    /// For an ordinary resource this is its [`TypeId`]. For one declaring
    /// [`Resource::shared_name`] it is the stable identity derived from that
    /// name, so every artifact that links the type produces the same id.
    #[inline]
    pub fn of<T: Resource>() -> Self {
        match T::shared_identity() {
            Some(identity) => Self::Shared(identity),
            None => Self::Native(TypeId::of::<T>()),
        }
    }

    /// The stable identity behind a shared resource id, if this is one.
    pub const fn shared_identity(self) -> Option<u128> {
        match self {
            Self::Shared(identity) => Some(identity),
            Self::Native(_) => None,
        }
    }
}

/// `Hash` is written by hand so a 128-bit identity reaches the hasher as 64
/// bits, exactly as [`ComponentId`](crate::component::ComponentId) does, and
/// for the same reason: a hash map resolves a collision with `Eq`, which still
/// compares every bit, so the second 8-byte block buys no correctness and
/// costs a compression round.
impl std::hash::Hash for ResourceId {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            // `TypeId`'s own impl already narrows 128 bits to 64.
            Self::Native(type_id) => type_id.hash(state),
            Self::Shared(identity) => (*identity as u64 ^ SHARED_RESOURCE_HASH_SALT).hash(state),
        }
    }
}

/// Salt keeping a shared identity from colliding with a `TypeId`-derived hash
/// on an identical low half. Arbitrary odd constant.
const SHARED_RESOURCE_HASH_SALT: u64 = 0xd6e8_feb8_6659_fd93;

// =============================================================================
// ResHandle
// =============================================================================

/// A cheap, typed handle to a resource in the [`World`].
///
/// The handle stores only the resource's type and nothing else, so it is
/// zero-sized. It is `Copy`, `Clone`, `Send` and `Sync`, which means it can be
/// stored or passed to another phase and resolved later without borrowing the
/// [`World`].
///
/// # Examples
/// ```no_run
/// # use pill_engine::*;
/// #[derive(Debug)]
/// struct Score(u32);
/// impl Resource for Score {}
///
/// // Create a handle
/// let handle = ResHandle::<Score>::new();
///
/// // Insert the resource
/// # let mut world = World::new();
/// world.insert_resource(Score(0));
///
/// // Use the handle to access the resource
/// let score = handle.get(&world).unwrap();
/// assert_eq!(score.0, 0);
///
/// // Mutably access via handle
/// let score = handle.get_mut(&mut world).unwrap();
/// score.0 += 10;
/// ```
pub struct ResHandle<T: Resource> {
    /// Type-level marker carrying no data; the handle is zero-sized at runtime.
    _phantom: PhantomData<T>,
}

impl<T: Resource> ResHandle<T> {
    /// Creates a new handle for a resource type.
    pub fn new() -> Self {
        Self {
            _phantom: PhantomData,
        }
    }

    /// Returns the [`ResourceId`] of this handle's resource type.
    pub fn id(&self) -> ResourceId {
        ResourceId::of::<T>()
    }

    /// Returns a shared reference to the resource from the [`World`].
    ///
    /// Returns `None` if the resource has not been inserted.
    pub fn get<'w>(&self, world: &'w World) -> Option<&'w T> {
        world.get_resource::<T>()
    }

    /// Returns a mutable reference to the resource from the [`World`].
    ///
    /// Returns `None` if the resource has not been inserted.
    pub fn get_mut<'w>(&self, world: &'w mut World) -> Option<&'w mut T> {
        world.get_resource_mut::<T>()
    }

    /// Checks whether the resource exists in the [`World`].
    pub fn exists(&self, world: &World) -> bool {
        world.has_resource::<T>()
    }
}

impl<T: Resource> Default for ResHandle<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Resource> Clone for ResHandle<T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<T: Resource> Copy for ResHandle<T> {}

// SAFETY: a `ResHandle<T>` holds only `PhantomData<T>`. It never owns or
// refers to a value of type `T`, so there is no `T` data to move, share or
// alias between threads, and the handle is sound as `Send` for any `T`.
unsafe impl<T: Resource> Send for ResHandle<T> {}
// SAFETY: as for `Send` above - a handle carries no `T` data at all, so
// sharing one between threads cannot race or alias anything.
unsafe impl<T: Resource> Sync for ResHandle<T> {}

impl<T: Resource> std::fmt::Debug for ResHandle<T> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "ResHandle<{}>", std::any::type_name::<T>())
    }
}

// =============================================================================
// ErasedResource
// =============================================================================

/// Everything an [`ErasedResource`] needs to know about its value's type, as
/// plain data.
///
/// The table holds function pointers rather than a vtable, for the same reason
/// [`ColumnOps`](crate::archetype::ColumnOps) does on the component side. A
/// `Box<dyn Any>` keeps its destructor in a vtable owned by the artifact that
/// created the value, and a resource outlives the module that inserted it -
/// nothing clears resources on reload - so that destructor could end up
/// pointing into an unmapped image. Plain data can be replaced instead, right
/// before the code it points at goes away.
///
/// One table answers every question about a registration: the drop, the size
/// and alignment a box needs for its own allocation, and, for a declaration
/// from another language, the schema hash its manifest carried. The drop takes
/// no row count, unlike a component column's `drop_range`, because a box holds
/// exactly one value rather than a run of rows.
#[derive(Clone, Copy)]
pub struct ErasedResourceOps {
    /// Drop the initialized value at `ptr`.
    ///
    /// # Safety
    ///
    /// `ptr` must point at a live, correctly aligned value of the type this
    /// table was built for.
    pub drop_in_place: unsafe fn(*mut u8),
    /// Size in bytes of the values this table describes.
    pub size: usize,
    /// Alignment in bytes of the values this table describes.
    pub align: usize,
    /// Schema hash the declaring manifest carried, for a foreign declaration.
    /// `None` for a Rust type, whose layout *is* its type.
    pub schema_hash: Option<u64>,
    /// Whether this table drops nothing because its values are blittable bytes
    /// owned by another language.
    ///
    /// [`World::rehome_resources`] leaves such a box's table alone - there is
    /// no newer code to point it at, and no destructor to keep valid - and the
    /// byte accessors and the relayout path read this flag to refuse a Rust
    /// value.
    pub foreign: bool,
}

impl ErasedResourceOps {
    /// Builds the table for a concrete Rust resource type.
    pub fn of<T: Resource>() -> Self {
        Self {
            drop_in_place: drop_in_place_of::<T>,
            size: std::mem::size_of::<T>(),
            align: std::mem::align_of::<T>(),
            schema_hash: None,
            foreign: false,
        }
    }

    /// Builds the table for a declaration from another language.
    ///
    /// The drop is deliberately a no-op. A foreign resource is a run of
    /// blittable bytes that owns nothing: the C# manifest path allows only
    /// unmanaged value types, so there is no destructor to run and no artifact
    /// whose code could go away.
    pub fn foreign(size: usize, align: usize, schema_hash: u64) -> Self {
        Self {
            drop_in_place: drop_nothing,
            size,
            align,
            schema_hash: Some(schema_hash),
            foreign: true,
        }
    }
}

/// Drop the `T` at `ptr`.
///
/// # Safety
///
/// `ptr` must point at a live, correctly aligned `T`.
unsafe fn drop_in_place_of<T>(ptr: *mut u8) {
    // SAFETY: guaranteed by the caller; the box upholds it.
    unsafe { std::ptr::drop_in_place(ptr.cast::<T>()) };
}

/// Drop nothing.
///
/// # Safety
///
/// Always sound: a foreign resource owns no allocation and holds no value that
/// needs releasing, which is the whole point of the blittable-only rule.
unsafe fn drop_nothing(_ptr: *mut u8) {}

/// One resource value, stored without a trait object.
///
/// A `Box<dyn Any + Send + Sync>` cannot serve, for two reasons:
///
/// 1. `Any::downcast_ref` compares [`TypeId`], which differs between artifacts
///    that compiled the same type with different inputs.
/// 2. A `Box<dyn Any>`'s vtable - including its destructor - lives in the image
///    of the artifact that created the value. Resources are never cleared on
///    reload, so retiring a module that owned one leaves a destructor pointing
///    into an unmapped image. The table here is plain data and can be re-homed.
///
/// The allocation is owned by this value and released on drop.
pub struct ErasedResource {
    /// Heap allocation holding the value, or dangling for a zero-sized type.
    value_allocation: NonNull<u8>,
    /// Runtime type identity of the stored value, as the creating artifact
    /// sees it, or `None` when the value has no Rust type at all.
    type_id: Option<TypeId>,
    /// Size in bytes of the stored value.
    size: usize,
    /// Alignment in bytes of the stored value.
    align: usize,
    /// Whether the stored value is identified by its layout rather than by
    /// `type_id`; set when the resource declares a shared name.
    has_shared_identity: bool,
    /// Per-type function table; replaceable via [`Self::refresh_ops`].
    function_table: ErasedResourceOps,
}

// SAFETY: a Rust box holds a `Resource`, which is `Send + Sync`, and a foreign
// box holds plain bytes. In both cases the box owns the only pointer to its
// value, so moving the box between threads moves the value with it.
unsafe impl Send for ErasedResource {}
// SAFETY: as for `Send` above - the box owns its value exclusively, so a shared
// borrow of the box cannot alias a mutable one.
unsafe impl Sync for ErasedResource {}

impl ErasedResource {
    /// Move `resource_value` into a fresh erased box.
    pub fn new<T: Resource>(resource_value: T) -> Self {
        let size = std::mem::size_of::<T>();
        let align = std::mem::align_of::<T>();
        // SAFETY: the allocation is `size` bytes at `align`, and the write
        // below fills it completely before it is read.
        let value_allocation = Self::allocate(size, align, |pointer| unsafe {
            std::ptr::write(pointer.cast::<T>(), resource_value);
        });
        Self {
            value_allocation,
            type_id: Some(TypeId::of::<T>()),
            size,
            align,
            // A shared resource is reached from an artifact whose `TypeId` for
            // the type differs, so its identity check has to be layout-based;
            // see `holds`.
            has_shared_identity: T::shared_name().is_some(),
            function_table: ErasedResourceOps::of::<T>(),
        }
    }

    /// Move another language's blittable bytes into a fresh erased box.
    ///
    /// The value has no Rust type: its identity is the shared name its
    /// declaration carried, so the box is identified by layout and `holds`
    /// compares layout - which is all a foreign declaration can offer. The box
    /// owns a copy of `bytes` and releases nothing on drop, because blittable
    /// bytes own nothing and there is no artifact whose code could go away.
    ///
    /// # Panics
    ///
    /// Panics when `size` and `align` cannot describe an allocation. Aborts
    /// when the allocation itself fails.
    pub fn new_foreign(bytes: &[u8], align: usize, schema_hash: u64) -> Self {
        let size = bytes.len();
        // SAFETY: the allocation is `bytes.len()` bytes, and the copy fills it
        // completely before it is read.
        let value_allocation = Self::allocate(size, align, |pointer| unsafe {
            std::ptr::copy_nonoverlapping(bytes.as_ptr(), pointer, size);
        });
        Self {
            value_allocation,
            // No Rust type names this value, which is exactly what the
            // layout-based `holds` branch above is for.
            type_id: None,
            size,
            align,
            has_shared_identity: true,
            function_table: ErasedResourceOps::foreign(size, align, schema_hash),
        }
    }

    /// Allocate a value slot of `size` bytes at `align` and fill it.
    ///
    /// A zero-sized value needs no allocation: `drop_in_place` on a
    /// dangling-but-aligned pointer is well defined for one.
    ///
    /// # Panics
    ///
    /// Panics when `size` and `align` cannot describe an allocation. Aborts
    /// when the allocation itself fails.
    fn allocate(size: usize, align: usize, fill: impl FnOnce(*mut u8)) -> NonNull<u8> {
        if size == 0 {
            return NonNull::dangling();
        }
        let layout = Layout::from_size_align(size, align).expect("valid resource layout");
        // SAFETY: `layout` has non-zero size, checked above.
        let pointer = unsafe { std::alloc::alloc(layout) };
        let Some(pointer) = NonNull::new(pointer) else {
            std::alloc::handle_alloc_error(layout);
        };
        fill(pointer.as_ptr());
        pointer
    }

    /// Runtime type identity of the stored value, or `None` for a foreign one.
    ///
    /// A foreign resource is a run of bytes with no Rust type anywhere in the
    /// process; its identity is the shared name it declared, reached through
    /// [`ResourceId`] instead.
    pub fn type_id(&self) -> Option<TypeId> {
        self.type_id
    }

    /// Size in bytes of the stored value.
    pub fn size(&self) -> usize {
        self.size
    }

    /// Alignment in bytes of the stored value.
    pub fn align(&self) -> usize {
        self.align
    }

    /// Whether this box holds another language's blittable bytes.
    ///
    /// The answer comes from the box's own creation: a Rust value was stored
    /// with a `TypeId`, a foreign payload without one. It never comes from the
    /// function table, which a re-home can replace, so a table cannot turn a
    /// Rust value into a foreign one.
    pub fn is_foreign(&self) -> bool {
        self.type_id.is_none()
    }

    /// The stored value as raw bytes.
    ///
    /// These are the bytes another language reads and writes, and what a
    /// migration plan moves. They only mean something to a caller that knows
    /// the registered layout: for a Rust value they are its fields at their
    /// offsets, and reading them does not make the value any less the type it
    /// is.
    pub fn bytes(&self) -> &[u8] {
        // SAFETY: the box owns a live byte run of this size, and the borrow is
        // tied to `&self`.
        unsafe { std::slice::from_raw_parts(self.value_allocation.as_ptr(), self.size) }
    }

    /// The stored value as raw bytes, mutably.
    ///
    /// A caller writing here is responsible for writing a value the stored type
    /// accepts - the same trust the managed mirror path places in its generated
    /// writers, which write component rows in place.
    pub fn bytes_mut(&mut self) -> &mut [u8] {
        // SAFETY: as above, and `&mut self` makes the borrow exclusive.
        unsafe { std::slice::from_raw_parts_mut(self.value_allocation.as_ptr(), self.size) }
    }

    /// Rewrites the stored payload into a new shape, following `plan`.
    ///
    /// Only a foreign payload may change size, because a Rust value's size is
    /// its type; `World::relayout_foreign_resource` refuses that before this
    /// runs. The old bytes are copied aside first, the same way a descriptor
    /// column migration works, so a plan that moves a field onto bytes it still
    /// occupies reads the original value instead of what it just wrote.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::DescriptorRowInvalid`] when an instruction falls
    /// outside the old or the new payload. Nothing is modified in that case.
    ///
    /// # Panics
    ///
    /// Panics when `size` and `align` cannot describe an allocation.
    pub(crate) fn migrate_bytes(
        &mut self,
        size: usize,
        align: usize,
        plan: &FieldPlan,
    ) -> Result<(), WorldError> {
        plan.validate(self.size, size)?;

        // Read the old payload out first: a plan is free to move a field onto
        // bytes the field it came from still occupies.
        let mut scratch = vec![0_u8; self.size.max(size)];
        // SAFETY: the box holds `self.size` initialized bytes, and the scratch
        // buffer is at least that long.
        unsafe {
            std::ptr::copy_nonoverlapping(
                self.value_allocation.as_ptr(),
                scratch.as_mut_ptr(),
                self.size,
            );
        }

        if size != self.size || align != self.align {
            if self.size != 0 {
                let old_layout =
                    Layout::from_size_align(self.size, self.align).expect("valid resource layout");
                // SAFETY: this is the live allocation, created with this layout.
                unsafe { std::alloc::dealloc(self.value_allocation.as_ptr(), old_layout) };
            }
            // SAFETY: the allocation is `size` bytes at `align`, and the writes
            // below fill it before anything reads it.
            self.value_allocation = Self::allocate(size, align, |_pointer| {});
            self.size = size;
            self.align = align;
        }

        // SAFETY: the destination holds `size` bytes, the plan was checked
        // against both sizes, and the scratch buffer holds the old payload.
        unsafe {
            std::ptr::write_bytes(self.value_allocation.as_ptr(), 0, size);
            plan.apply(
                &scratch,
                std::slice::from_raw_parts_mut(self.value_allocation.as_ptr(), size),
            );
        }

        // Update the box's own table to describe the new shape. A foreign
        // box's table is never refreshed from the resource factories - that is
        // what stops a Rust type sharing the name from handing its drop to
        // these bytes - so this is the one place it changes.
        self.function_table =
            ErasedResourceOps::foreign(size, align, self.function_table.schema_hash.unwrap_or(0));
        Ok(())
    }

    /// Replaces the per-type function table.
    ///
    /// The owner uses this to point the stored drop at code that is still
    /// mapped, after the artifact that supplied it was reloaded or retired. A
    /// foreign box is never refreshed this way: see [`Self::is_foreign`] and
    /// `World::rehome_resources`.
    ///
    /// The new table must agree with how the box was created. Installing a
    /// foreign table over a Rust value, or the reverse, would give the value a
    /// drop it never had, so the swap is refused and logged instead.
    pub fn refresh_ops(&mut self, function_table: ErasedResourceOps) {
        // Keeping the existing table is the safe choice: the value is still
        // released by whatever produced it. `rehome_resources` never lets this
        // case through, so reaching it means a caller made a mistake.
        if function_table.foreign != self.type_id.is_none() {
            error!(
                target: pill_core::telemetry::telemetry_target::ECS,
                foreign_table = function_table.foreign,
                foreign_box = self.type_id.is_none(),
                "a resource's function table disagrees with the ownership its box was created with; keeping the existing table"
            );
            return;
        }
        self.function_table = function_table;
    }

    /// Whether this box is identified by layout rather than by `TypeId`.
    pub fn has_shared_identity(&self) -> bool {
        self.has_shared_identity
    }

    /// Whether the stored value can be read as a `T`.
    ///
    /// An ordinary resource demands the exact `TypeId` it was stored under. A
    /// shared one cannot: another artifact's `TypeId` for the same type differs,
    /// so it compares layout instead, which still rejects an outright wrong `T`.
    ///
    /// Layout alone is weaker than `TypeId` - `{f32, f32}` and `{u32, u32}`
    /// agree on size and alignment - which is why a shared resource's name and
    /// shape are guarded at registration (see `World::register_resource`).
    pub fn holds<T: Resource>(&self) -> bool {
        if self.has_shared_identity {
            self.size == std::mem::size_of::<T>() && self.align == std::mem::align_of::<T>()
        } else {
            self.type_id == Some(TypeId::of::<T>())
        }
    }

    /// Borrow the stored value as a `T`, or `None` when it is another type.
    pub fn get<T: Resource>(&self) -> Option<&T> {
        if !self.holds::<T>() {
            return None;
        }
        // SAFETY: the check above establishes that the allocation holds a live,
        // correctly aligned `T`.
        Some(unsafe { &*self.value_allocation.as_ptr().cast::<T>() })
    }

    /// Borrow the stored value mutably, or `None` when it is another type.
    pub fn get_mut<T: Resource>(&mut self) -> Option<&mut T> {
        if !self.holds::<T>() {
            return None;
        }
        // SAFETY: as for `get`, and `&mut self` makes the borrow exclusive.
        Some(unsafe { &mut *self.value_allocation.as_ptr().cast::<T>() })
    }

    /// Move the stored value out, consuming the box.
    ///
    /// This is the only path where ownership leaves the box, and the only one
    /// that must **not** run the drop: the value goes to the caller, who
    /// becomes responsible for it.
    ///
    /// # Errors
    ///
    /// Returns the box unchanged, as `Err`, when it holds another type, so a
    /// refused take does not destroy the value.
    pub fn take<T: Resource>(self) -> Result<T, Self> {
        if !self.holds::<T>() {
            return Err(self);
        }
        // The value moves out, so this box must release its allocation without
        // dropping the contents. `ManuallyDrop` suppresses `Drop for
        // ErasedResource`; the deallocation below replaces it.
        let undropped_box = std::mem::ManuallyDrop::new(self);
        // SAFETY: the check above establishes a live `T` at `value_allocation`,
        // and `ManuallyDrop` guarantees nothing else reads or drops it.
        let taken_value =
            unsafe { std::ptr::read(undropped_box.value_allocation.as_ptr().cast::<T>()) };
        if undropped_box.size != 0 {
            let layout = Layout::from_size_align(undropped_box.size, undropped_box.align)
                .expect("valid resource layout");
            // SAFETY: the allocation came from `alloc` with this layout in
            // `new`, and the value has been moved out, so nothing live is freed.
            unsafe { std::alloc::dealloc(undropped_box.value_allocation.as_ptr(), layout) };
        }
        Ok(taken_value)
    }
}

impl std::fmt::Debug for ErasedResource {
    /// Reports what the box knows about the value, since the value itself is
    /// opaque here.
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ErasedResource")
            .field("type_id", &self.type_id)
            .field("size", &self.size)
            .field("align", &self.align)
            .finish_non_exhaustive()
    }
}

impl Drop for ErasedResource {
    fn drop(&mut self) {
        // SAFETY: the box owns a live value of the type its table was built
        // for, and this runs exactly once.
        unsafe { (self.function_table.drop_in_place)(self.value_allocation.as_ptr()) };
        if self.size != 0 {
            let layout =
                Layout::from_size_align(self.size, self.align).expect("valid resource layout");
            // SAFETY: the allocation came from `alloc` with this layout in
            // `new`, and the value was just dropped.
            unsafe { std::alloc::dealloc(self.value_allocation.as_ptr(), layout) };
        }
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(Debug, PartialEq)]
    struct Score(u32);
    impl Resource for Score {}

    #[derive(Debug, PartialEq)]
    struct ProjectTime {
        delta_seconds: f32,
        elapsed_seconds: f32,
    }
    impl Resource for ProjectTime {}

    /// Verifies that `ResourceId` stores the correct TypeId internally.
    #[test]
    fn test_resource_id() {
        let score_id = ResourceId::of::<Score>();
        let same_score_id = ResourceId::of::<Score>();
        let project_time_id = ResourceId::of::<ProjectTime>();

        assert_eq!(score_id, same_score_id);
        assert_ne!(score_id, project_time_id);
    }

    /// Verifies that `ResHandle::new()` creates a handle for the correct type.
    #[test]
    fn test_res_handle_new() {
        let handle = ResHandle::<Score>::new();
        assert_eq!(handle.id(), ResourceId::of::<Score>());
    }

    /// Verifies that `ResHandle::default()` creates a valid zero-cost handle.
    #[test]
    fn test_res_handle_default() {
        let handle = ResHandle::<Score>::default();
        assert_eq!(handle.id(), ResourceId::of::<Score>());
    }

    /// Verifies that `ResHandle` implements Copy and Clone correctly.
    #[test]
    fn test_res_handle_copy_clone() {
        let handle = ResHandle::<Score>::new();
        let first_copy = handle;
        let second_copy = handle;
        // All handles refer to the same resource type
        assert_eq!(handle.id(), first_copy.id());
        assert_eq!(handle.id(), second_copy.id());
    }

    /// Verifies that `ResHandle` implements Debug for diagnostic output.
    #[test]
    fn test_res_handle_debug() {
        let handle = ResHandle::<Score>::new();
        let debug_str = format!("{:?}", handle);
        assert!(debug_str.contains("ResHandle"));
        assert!(debug_str.contains("Score"));
    }

    /// Verifies that `ResHandle::get()` returns the resource when it exists.
    #[test]
    fn test_res_handle_get() {
        let mut world = World::new();
        let handle = ResHandle::<Score>::new();

        // Resource doesn't exist yet
        assert!(!handle.exists(&world));
        assert!(handle.get(&world).is_none());

        world.insert_resource(Score(42));

        // Now accessible via handle
        assert!(handle.exists(&world));
        assert_eq!(handle.get(&world).unwrap().0, 42);
    }

    /// Verifies that `ResHandle::get_mut()` allows mutable access to an existing resource.
    #[test]
    fn test_res_handle_get_mut() {
        let mut world = World::new();
        let handle = ResHandle::<Score>::new();

        world.insert_resource(Score(0));

        // Mutate via handle
        handle.get_mut(&mut world).unwrap().0 += 10;
        assert_eq!(handle.get(&world).unwrap().0, 10);

        handle.get_mut(&mut world).unwrap().0 += 5;
        assert_eq!(handle.get(&world).unwrap().0, 15);
    }

    /// Verifies that `ResHandle::get()` returns None for a missing resource.
    #[test]
    fn test_res_handle_missing_resource() {
        let world = World::new();
        let handle = ResHandle::<Score>::new();

        assert!(!handle.exists(&world));
        assert!(handle.get(&world).is_none());
    }

    /// Verifies that handles for different resource types are independent.
    #[test]
    fn test_multiple_handles_different_types() {
        let mut world = World::new();

        let score_handle = ResHandle::<Score>::new();
        let time_handle = ResHandle::<ProjectTime>::new();

        world.insert_resource(Score(100));
        world.insert_resource(ProjectTime {
            delta_seconds: 0.016,
            elapsed_seconds: 0.0,
        });

        assert_eq!(score_handle.get(&world).unwrap().0, 100);
        assert_eq!(time_handle.get(&world).unwrap().delta_seconds, 0.016);

        // Different types, different IDs
        assert_ne!(score_handle.id(), time_handle.id());
    }

    // =========================================================================
    // ErasedResource
    // =========================================================================

    /// Counts its own drops, so a double drop or a leak is visible rather than
    /// merely suspected.
    ///
    /// The counter is owned per instance rather than being a `static`: the
    /// harness runs these tests in parallel, and a shared counter makes every
    /// `before + 1` assertion race with the others.
    #[derive(Debug)]
    struct Tracked {
        carried_value: u32,
        drop_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    }
    impl Resource for Tracked {}

    impl Drop for Tracked {
        fn drop(&mut self) {
            self.drop_count
                .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        }
    }

    /// A fresh `Tracked` and the counter watching it.
    fn tracked(carried_value: u32) -> (Tracked, std::sync::Arc<std::sync::atomic::AtomicUsize>) {
        let drop_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
        (
            Tracked {
                carried_value,
                drop_count: std::sync::Arc::clone(&drop_count),
            },
            drop_count,
        )
    }

    /// Reads one probe's counter.
    fn drops_of(counter: &std::sync::atomic::AtomicUsize) -> usize {
        counter.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// A second resource type with the same layout as [`Tracked`], to show the
    /// type check is not merely a size comparison.
    #[derive(Debug)]
    struct Lookalike {
        _same_layout_field: u32,
    }
    impl Resource for Lookalike {}

    /// A zero-sized resource, which takes the no-allocation path.
    #[derive(Debug)]
    struct ZeroSized;
    impl Resource for ZeroSized {}

    /// A value survives the round trip into the box and back out by reference.
    #[test]
    fn an_erased_value_can_be_stored_and_borrowed() {
        let mut erased = ErasedResource::new(tracked(7).0);

        assert_eq!(erased.get::<Tracked>().unwrap().carried_value, 7);
        erased.get_mut::<Tracked>().unwrap().carried_value = 9;
        assert_eq!(erased.get::<Tracked>().unwrap().carried_value, 9);
    }

    /// The box holds one type and says so; a same-sized other type is refused.
    #[test]
    fn an_erased_box_refuses_another_type_with_an_identical_layout() {
        let mut erased = ErasedResource::new(tracked(1).0);

        assert!(erased.holds::<Tracked>());
        assert!(!erased.holds::<Lookalike>());
        assert!(erased.get::<Lookalike>().is_none());
        assert!(erased.get_mut::<Lookalike>().is_none());
    }

    /// Dropping the box drops the value exactly once.
    #[test]
    fn dropping_an_erased_box_drops_the_value_once() {
        let (probe, drop_count) = tracked(1);
        drop(ErasedResource::new(probe));
        assert_eq!(drops_of(&drop_count), 1);
    }

    /// Taking the value out moves it to the caller and must NOT also drop it.
    ///
    /// The delicate path: the box has to release its allocation without running
    /// the drop, or the value is dropped twice.
    #[test]
    fn taking_an_erased_value_out_moves_it_exactly_once() {
        let (probe, drop_count) = tracked(42);
        let erased = ErasedResource::new(probe);

        let taken = erased.take::<Tracked>().expect("the box holds a Tracked");
        assert_eq!(taken.carried_value, 42);
        assert_eq!(
            drops_of(&drop_count),
            0,
            "the value is alive in the caller's hands; nothing dropped yet"
        );

        drop(taken);
        assert_eq!(
            drops_of(&drop_count),
            1,
            "the value must be dropped exactly once, by its new owner"
        );
    }

    /// A refused take gives the box back rather than destroying the value.
    #[test]
    fn a_refused_take_returns_the_erased_box_intact() {
        let (probe, drop_count) = tracked(3);
        let erased = ErasedResource::new(probe);

        let returned = erased
            .take::<Lookalike>()
            .expect_err("the box does not hold a Lookalike");
        assert_eq!(
            drops_of(&drop_count),
            0,
            "a refused take must not drop anything"
        );
        assert_eq!(returned.get::<Tracked>().unwrap().carried_value, 3);

        drop(returned);
        assert_eq!(drops_of(&drop_count), 1);
    }

    /// A zero-sized resource takes the no-allocation path and still round trips.
    #[test]
    fn a_zero_sized_erased_resource_round_trips() {
        let erased = ErasedResource::new(ZeroSized);
        assert_eq!(erased.size(), 0);
        assert!(erased.holds::<ZeroSized>());
        erased.take::<ZeroSized>().expect("a ZeroSized comes back");
    }

    /// The box reports the stored type's layout, which is what re-homing and
    /// the identity checks are built on.
    #[test]
    fn an_erased_box_reports_the_stored_types_layout() {
        let erased = ErasedResource::new(tracked(0).0);
        assert_eq!(erased.size(), std::mem::size_of::<Tracked>());
        assert_eq!(erased.align(), std::mem::align_of::<Tracked>());
        assert_eq!(erased.type_id(), Some(TypeId::of::<Tracked>()));
    }

    /// Replacing the function table leaves the stored value alone, which is the
    /// whole point of re-homing across a reload.
    #[test]
    fn refreshing_an_erased_boxs_ops_leaves_the_value_alone() {
        let (probe, drop_count) = tracked(11);
        let mut erased = ErasedResource::new(probe);

        erased.refresh_ops(ErasedResourceOps::of::<Tracked>());

        assert_eq!(erased.get::<Tracked>().unwrap().carried_value, 11);
        assert_eq!(drops_of(&drop_count), 0, "nothing was dropped");

        drop(erased);
        assert_eq!(drops_of(&drop_count), 1);
    }
}
