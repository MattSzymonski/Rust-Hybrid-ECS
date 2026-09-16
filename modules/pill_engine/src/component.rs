//! Component trait, type identification, and change-detection primitives.
//!
//! # Responsibilities
//!
//! - Defines the [`Component`] marker trait required by all ECS component types.
//! - Provides [`ComponentId`] for type-erased component identification.
//! - Implements [`Tick`] and [`ComponentTicks`] for frame-based change detection.
//! - Manages the [`ComponentRegistry`] that assigns bit indices for archetype masks.
//! - Defines [`ComponentMask`] (u128) for O(1) archetype matching.
//!
//! # Design
//!
//! Component types are registered at runtime and assigned a bit position in a
//! 128-bit mask. Archetypes carry a mask of their component set; queries build
//! a mask from their requested types and use bitwise AND to find matching
//! archetypes in O(1). Change detection uses a global tick counter bumped each
//! frame - components record the tick at which they were added/mutated, and
//! filters compare against the calling system's last-run tick.

// Standard library
use std::any::TypeId;
use std::collections::HashMap;

// Current crate
use crate::component_registry::ComponentFieldDescriptor;
use crate::error::WorldError;

// =============================================================================
// Component
// =============================================================================

/// Component marker trait - all components must be `'static` and [`Send`].
///
/// # Interior Mutability Warning
///
/// Components are accessed concurrently during parallel iteration. Although
/// `Component` only requires [`Send`] (not [`Sync`]), parallel queries may
/// create multiple `&T` references to the same component data across
/// threads. Avoid [`Cell`](std::cell::Cell), [`RefCell`](std::cell::RefCell),
/// or other interior-mutability types in component structs - they can
/// cause data races when read concurrently through shared references.
///
/// If you need mutable state inside a component accessed by multiple
/// systems, prefer splitting the mutable portion into a separate component
/// type and using `&mut T` queries (which the scheduler serializes
/// correctly).
///
/// # Examples
///
/// ```
/// # use pill_engine::component::Component;
/// struct Position { x: f32, y: f32 }
///
/// impl Component for Position {}
/// ```
pub trait Component: Send + 'static {
    /// Stable, cross-binary name for this component, or `None` for the
    /// ordinary per-binary identity.
    ///
    /// A component type linked into more than one binary in the same process -
    /// the host, the project, a module DLL - gets a different [`TypeId`] in
    /// each, because `TypeId` is a hash over the crate name, its `-C metadata`
    /// disambiguator and the type path, computed per compilation unit.
    /// Identified by `TypeId`, one type therefore becomes several components
    /// with several columns, and neither binary can see the other's entities.
    ///
    /// Declaring a name here replaces that identity with one derived from the
    /// name itself, which every binary computes identically and without any
    /// coordination. All of them then resolve to one [`ComponentId`], one mask
    /// bit, and one column.
    ///
    /// The name must be unique across the whole process, so it should be
    /// namespaced - `"pill_spline::Spline"`, not `"Spline"`. It must also be
    /// stable: it is written down rather than derived from
    /// [`std::any::type_name`], whose output is explicitly not guaranteed
    /// stable across compiler versions.
    ///
    /// Set it through `#[pill(shared)]` on `#[derive(PillComponent)]` rather
    /// than by hand; see [`ComponentId::of`] for what changes once it is set.
    ///
    /// This is a `where Self: Sized` method rather than an associated
    /// constant because an associated constant would make `Component` no
    /// longer dyn-compatible, and the engine's whole storage layer moves
    /// components as `dyn Component`.
    fn shared_name() -> Option<&'static str>
    where
        Self: Sized,
    {
        None
    }

    /// The stable identity [`Self::shared_name`] hashes to, or `None`.
    ///
    /// Defaulted in terms of `shared_name`, so an implementor only writes the
    /// name and the two cannot disagree. `#[derive(PillComponent)]` overrides
    /// it with a compile-time constant: `ComponentId::of` is called once per
    /// `get_component`, and hashing the name on every one of those is
    /// measurable - around 20% of that call - where folding it to a constant
    /// makes it exactly the load `TypeId::of` compiles to.
    ///
    /// Override it only with `shared_component_identity(Self::shared_name())`;
    /// any other value silently splits the component's identity from the name
    /// every other binary derives it from.
    fn shared_identity() -> Option<u128>
    where
        Self: Sized,
    {
        Self::shared_name().map(shared_component_identity)
    }
}

// =============================================================================
// Tick
// =============================================================================

/// Monotonically increasing counter used to detect when components change.
///
/// The World maintains a global tick that is bumped each frame (or on demand).
/// Each component instance carries its own `ComponentTicks` recording when it
/// was added and most recently mutated. Systems can later compare these to
/// their own `last_run` tick to find new or changed data.
#[repr(transparent)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Default)]
pub struct Tick(pub u32);

impl Tick {
    // -------------------------------------------------------------------------
    // Construction
    // -------------------------------------------------------------------------

    /// Constructs a tick with an explicit counter value.
    #[inline]
    pub const fn new(value: u32) -> Self {
        Self(value)
    }

    // -------------------------------------------------------------------------
    // Queries
    // -------------------------------------------------------------------------

    /// Returns the underlying counter value.
    #[inline]
    pub const fn get(self) -> u32 {
        self.0
    }

    /// Returns true if this tick is strictly newer than `last_run`
    /// (and not in the future relative to `this_run`).
    #[inline]
    pub fn is_newer_than(self, last_run: Tick, this_run: Tick) -> bool {
        self.0 > last_run.0 && self.0 <= this_run.0
    }
}

// =============================================================================
// ComponentTicks
// =============================================================================

/// Per-component-instance change-detection metadata.
///
/// Stored in a parallel `Vec<ComponentTicks>` next to each archetype's
/// component storage so that the metadata for row `i` lives at index `i`.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default)]
pub struct ComponentTicks {
    /// Tick at which this component was added to its current entity.
    pub added: Tick,
    /// Tick at which this component was most recently mutated through `Mut<T>`.
    pub changed: Tick,
}

impl ComponentTicks {
    // -------------------------------------------------------------------------
    // Construction
    // -------------------------------------------------------------------------

    /// Creates new ticks with both `added` and `changed` set to the given tick.
    #[inline]
    pub fn new(tick: Tick) -> Self {
        Self {
            added: tick,
            changed: tick,
        }
    }

    /// Was this component added between `last_run` and `this_run`?
    #[inline]
    pub fn is_added(&self, last_run: Tick, this_run: Tick) -> bool {
        self.added.is_newer_than(last_run, this_run)
    }

    /// Was this component changed (or added) between `last_run` and `this_run`?
    #[inline]
    pub fn is_changed(&self, last_run: Tick, this_run: Tick) -> bool {
        self.changed.is_newer_than(last_run, this_run)
    }

    // -------------------------------------------------------------------------
    // Mutations
    // -------------------------------------------------------------------------

    /// Sets the `changed` tick directly, bypassing the normal `Mut<T>` path.
    #[inline]
    pub fn set_changed(&mut self, tick: Tick) {
        self.changed = tick;
    }
}

// =============================================================================
// ComponentId
// =============================================================================

/// Type-erased identifier for a registered component type.
///
/// Native components retain their Rust [`TypeId`]. Runtime-defined components
/// use the stable 128-bit identity supplied by their external manifest. A
/// native component that declares [`Component::shared_name`] uses a stable
/// identity derived from that name instead of its `TypeId`, so every binary
/// that links the type arrives at the same id.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ComponentId {
    /// Component backed by a concrete Rust type, identified per binary.
    Native(TypeId),
    /// Component described at runtime by an external language manifest.
    Dynamic(u128),
    /// Component backed by a concrete Rust type that may be linked into more
    /// than one binary, identified by the stable name it declares.
    ///
    /// Storage is native, exactly as for [`Self::Native`] - only the identity
    /// differs - so [`Self::is_native_storage`] holds for both.
    Shared(u128),
}

/// Mix one 64-bit half of a stable component identity out of a name.
///
/// FNV-1a, a fixed function of the bytes alone: no seed, no runtime state, no
/// dependence on compiler version or process. That is what lets two separately
/// compiled binaries - and the managed runtime, which derives the same value
/// from the same canonical name - agree on an identity without exchanging
/// anything.
pub const fn component_name_hash(name: &str, offset: u64) -> u64 {
    let bytes = name.as_bytes();
    let mut hash = offset;
    let mut index = 0;
    while index < bytes.len() {
        hash ^= bytes[index] as u64;
        hash = hash.wrapping_mul(0x100000001b3);
        index += 1;
    }
    hash
}

/// Derive the stable 128-bit identity of a component from its declared name.
///
/// Two FNV-1a passes with different offsets, concatenated, so the 128-bit
/// space is actually used rather than a 64-bit hash being zero-padded into it.
pub const fn shared_component_identity(name: &str) -> u128 {
    let low = component_name_hash(name, 0xcbf29ce484222325);
    let high = component_name_hash(name, 0x84222325cbf29ce4);
    ((high as u128) << 64) | low as u128
}

/// `Hash` is written by hand rather than derived so a 128-bit identity feeds
/// the hasher as 64 bits.
///
/// This is the reasoning [`TypeId`] applies to itself: it too holds 128 bits
/// and hashes only half of them, because a hash map resolves collisions with
/// `Eq` - which still compares every bit - so the second 8-byte block buys no
/// correctness and costs a compression round. Deriving `Hash` here made a
/// shared component's lookups measurably slower than a native component's,
/// around 18% of `World::get_component`, which performs two of them per call.
///
/// The variant is mixed in as a salt rather than hashed as a separate value,
/// so distinguishing the variants stays free.
impl std::hash::Hash for ComponentId {
    #[inline]
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        match self {
            // `TypeId`'s own impl already does the 128-to-64 narrowing.
            Self::Native(type_id) => type_id.hash(state),
            Self::Dynamic(identity) => (*identity as u64 ^ DYNAMIC_HASH_SALT).hash(state),
            Self::Shared(identity) => (*identity as u64 ^ SHARED_HASH_SALT).hash(state),
        }
    }
}

/// Salts keeping the two 128-bit-identity variants from colliding with each
/// other on identical values. Arbitrary odd constants; only their difference
/// matters.
const DYNAMIC_HASH_SALT: u64 = 0x9e37_79b9_7f4a_7c15;
const SHARED_HASH_SALT: u64 = 0xbf58_476d_1ce4_e5b9;

impl ComponentId {
    /// Returns the [`ComponentId`] for the concrete Rust type `T`.
    ///
    /// For an ordinary component this is its [`TypeId`], which differs between
    /// binaries. For one that declares [`Component::shared_name`] it is the
    /// stable identity derived from that name, which does not - so every
    /// binary that links the type produces the same id here, and the engine
    /// gives them one bit and one column between them.
    ///
    /// The bound is [`Component`] rather than `'static` precisely so that
    /// distinction cannot be bypassed: there is no second entry point that
    /// could forget to consult the declaration.
    ///
    /// Costs the same as `TypeId::of` either way: a derived component's
    /// `shared_identity` is a compile-time constant, so this is a load in both
    /// arms rather than a hash in one of them.
    #[inline]
    pub fn of<T: Component>() -> Self {
        match T::shared_identity() {
            Some(identity) => Self::Shared(identity),
            None => Self::Native(TypeId::of::<T>()),
        }
    }

    /// Builds a dynamic component ID from the stable 128-bit identity
    /// supplied by an external runtime manifest.
    pub const fn dynamic(stable_id: u128) -> Self {
        Self::Dynamic(stable_id)
    }

    /// Wraps an existing Rust [`TypeId`] in a native component ID.
    pub const fn native(type_id: TypeId) -> Self {
        Self::Native(type_id)
    }

    /// Returns the wrapped [`TypeId`] if this component is identified by one.
    ///
    /// `None` for a dynamic component, which has no Rust type at all, and also
    /// for a shared one, which has a Rust type in every binary that links it
    /// but no single `TypeId` that names it. Callers asking "are these rows
    /// native storage?" want [`Self::is_native_storage`] instead; the callers
    /// that genuinely need a `TypeId` are the ones that should still get
    /// `None` here.
    pub const fn native_type_id(self) -> Option<TypeId> {
        match self {
            Self::Native(type_id) => Some(type_id),
            Self::Dynamic(_) | Self::Shared(_) => None,
        }
    }

    /// Whether this component's rows live in a native column rather than in a
    /// byte-oriented dynamic one.
    ///
    /// True for [`Self::Native`] and [`Self::Shared`] alike: shared identity
    /// changes how a component is *named*, never how it is stored.
    pub const fn is_native_storage(self) -> bool {
        matches!(self, Self::Native(_) | Self::Shared(_))
    }

    /// The stable identity behind a shared component id, if this is one.
    pub const fn shared_identity(self) -> Option<u128> {
        match self {
            Self::Shared(identity) => Some(identity),
            Self::Native(_) | Self::Dynamic(_) => None,
        }
    }
}

// =============================================================================
// ComponentLayout
// =============================================================================

/// The memory shape of one registered component type.
///
/// Recorded per [`ComponentId`] so a second registration of the same component
/// can be checked against the first. For an ordinary component that check is a
/// diagnostic; for a shared one it is a soundness requirement, because two
/// binaries reach one column through it and nothing else proves they agree
/// about what a row contains.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ComponentLayout {
    /// Byte size of one component value.
    pub size: usize,
    /// Byte alignment of one component value.
    pub align: usize,
    /// Structural hash over the declared field layout, or `None` when the
    /// component was registered without one (hand-registered or unit types).
    pub schema_hash: Option<u64>,
}

impl ComponentLayout {
    /// Build the layout record for `T` from its declared field descriptors.
    ///
    /// An empty `fields` slice records no schema hash rather than the hash of
    /// nothing, so a registration that simply carries no field metadata is not
    /// mistaken for one describing an empty struct.
    pub fn of<T: Component>(fields: &[ComponentFieldDescriptor]) -> Self {
        Self {
            size: std::mem::size_of::<T>(),
            align: std::mem::align_of::<T>(),
            schema_hash: (!fields.is_empty()).then(|| component_schema_hash(fields)),
        }
    }

    /// Whether `other` describes the same memory shape as this layout.
    ///
    /// Size and alignment must always agree. The structural hash is compared
    /// only when both records have one: a layout-less registration carries no
    /// evidence either way, and treating its absence as a mismatch would
    /// reject a hand-registered component that is in fact identical.
    ///
    /// Size and alignment alone are *not* layout - `{f32, f32}` and
    /// `{u32, u32}` agree on both and would misread each other's rows
    /// silently - which is exactly why the schema hash exists and why a shared
    /// component should always be registered with its field descriptors.
    pub fn is_compatible_with(&self, other: &Self) -> bool {
        if self.size != other.size || self.align != other.align {
            return false;
        }
        match (self.schema_hash, other.schema_hash) {
            (Some(left), Some(right)) => left == right,
            _ => true,
        }
    }
}

/// Structural hash over a component's declared field layout.
///
/// FNV-1a over the field descriptors rather than a `DefaultHasher`, because
/// this value is compared *between separately compiled binaries*: the standard
/// hasher's algorithm is explicitly unspecified and may change between Rust
/// releases, while this is a fixed function of the bytes.
///
/// Every field's name, type tag, offset, size, alignment and element count is
/// folded in, so a reordered, retyped, repadded or resized field all change the
/// result even when the struct's total size does not.
pub fn component_schema_hash(fields: &[ComponentFieldDescriptor]) -> u64 {
    // FNV-1a offset basis; `component_name_hash` continues the same chain, so
    // strings and integers mix into one running value.
    let mut hash = 0xcbf29ce484222325u64;
    for field in fields {
        hash = component_name_hash(field.name, hash);
        hash = component_name_hash(field.type_tag, hash);
        for value in [field.offset, field.size, field.align, field.element_count] {
            for byte in (value as u64).to_le_bytes() {
                hash ^= byte as u64;
                hash = hash.wrapping_mul(0x100000001b3);
            }
        }
    }
    hash
}

// =============================================================================
// ComponentMask
// =============================================================================

/// Bitmask for efficiently representing sets of components.
///
/// ## 128 Component Type Limit
///
/// Uses a `u128` internally, limiting the ECS to 128 unique component types.
/// This is a deliberate design tradeoff:
///
/// - O(1) archetype matching: Query matching is a simple bitwise AND
/// - 128 bits = 128 component types: Sufficient for most games
/// - No heap allocation: Masks are stack-allocated and Copy
///
/// If you hit the 128 limit, consider:
/// 1. Combining related components (e.g., Transform instead of Position + Rotation + Scale)
/// 2. Using marker components sparingly
/// 3. Restructuring to use fewer component types with interior variants
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct ComponentMask(u128);

impl ComponentMask {
    /// Constructs a mask with no bits set.
    pub const fn empty() -> Self {
        Self(0)
    }

    /// Set a bit in the mask.
    ///
    /// # Panics
    /// In debug builds, panics if `bit_index >= 128`.
    pub fn set(&mut self, bit_index: u8) {
        debug_assert!(
            bit_index < 128,
            "ComponentMask bit index {bit_index} out of range (max 127)"
        );
        self.0 |= 1u128 << bit_index;
    }

    /// Check if a specific bit is set (O(1) component type check).
    ///
    /// # Panics
    /// In debug builds, panics if `bit_index >= 128`.
    #[inline]
    pub fn has_bit(&self, bit_index: u8) -> bool {
        debug_assert!(
            bit_index < 128,
            "ComponentMask bit index {bit_index} out of range (max 127)"
        );
        (self.0 & (1u128 << bit_index)) != 0
    }

    /// Check if all bits in `other` are also set in this mask.
    ///
    /// Used in the query hot path to determine whether an archetype
    /// satisfies the component requirements of a query.
    #[inline]
    pub fn contains_all(&self, other: &ComponentMask) -> bool {
        (self.0 & other.0) == other.0
    }

    /// Bitwise AND of two masks - bits set in both inputs.
    #[inline]
    pub fn intersection(a: &ComponentMask, b: &ComponentMask) -> ComponentMask {
        ComponentMask(a.0 & b.0)
    }

    /// True if no bits are set.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.0 == 0
    }

    /// True if any bit set in `other` is also set in `self`.
    #[inline]
    pub fn intersects(&self, other: &ComponentMask) -> bool {
        (self.0 & other.0) != 0
    }

    /// Raw u128 bitfield - used to derive a unique [`ArchetypeId`].
    #[inline]
    pub(crate) fn bits(self) -> u128 {
        self.0
    }
}

// =============================================================================
// ComponentRegistry
// =============================================================================

/// Registry that maps component types to bit indices in the component mask.
///
/// Handles registration of component types and maintains the mapping needed
/// to convert between ComponentId and bit positions for efficient mask operations.
pub struct ComponentRegistry {
    /// Maps each registered [`ComponentId`] to its assigned bit index.
    id_to_bit: HashMap<ComponentId, u8>,
    /// Type name of each registered component, used for diagnostics and tooling.
    names: HashMap<ComponentId, String>,
    /// Size in bytes of each registered component type.
    layouts: HashMap<ComponentId, ComponentLayout>,
    /// Rust type name that claimed each shared identity, so a second claim by a
    /// *different* type can be told apart from the same type compiled twice.
    ///
    /// Owned rather than the `&'static str` `type_name` returns: that pointer
    /// lives in the declaring artifact's read-only data and dangles once a
    /// module DLL is unloaded, while the registry outlives every module.
    shared_declaring_types: HashMap<ComponentId, String>,
    /// Next bit index to assign to a newly registered component.
    next_bit: u8,
    /// Bit indices reclaimed by [`Self::remove`], reused before `next_bit`
    /// advances. Without this, a dynamic manifest that retires and introduces
    /// types across reloads walks `next_bit` upward until the 128-type limit
    /// aborts the process, even though live components never exceed a handful.
    free_bits: Vec<u8>,
}

/// Outcome of registering a component type.
///
/// Registration is idempotent, so "registered" and "was already registered"
/// are both successes - but they are different facts, and collapsing them is
/// how a stale recorded layout goes unnoticed after a reload replaces a
/// component's definition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[must_use = "registration reports whether the type was already present; use               `register_bit` if only the bit index is wanted"]
pub enum Registration {
    /// The type was not present and has been assigned this bit.
    Created(u8),
    /// The type was already registered under this bit; nothing changed.
    AlreadyPresent(u8),
}

impl Registration {
    /// The bit index, whichever case this is.
    #[must_use]
    pub fn bit(self) -> u8 {
        match self {
            Self::Created(bit) | Self::AlreadyPresent(bit) => bit,
        }
    }

    /// Whether this call is what created the registration.
    #[must_use]
    pub fn is_new(self) -> bool {
        matches!(self, Self::Created(_))
    }
}

impl ComponentRegistry {
    /// Creates an empty registry with no components registered.
    pub fn new() -> Self {
        Self {
            id_to_bit: HashMap::new(),
            names: HashMap::new(),
            layouts: HashMap::new(),
            shared_declaring_types: HashMap::new(),
            next_bit: 0,
            free_bits: Vec::new(),
        }
    }
}

impl Default for ComponentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl ComponentRegistry {
    /// Register a component type and assign it a bit index.
    ///
    /// Returns [`Registration::Created`] with a fresh bit, or
    /// [`Registration::AlreadyPresent`] with the existing one. Idempotent by
    /// design - the reload path re-runs `init`, which re-registers every type -
    /// but the two cases are distinguishable so a caller that cares can tell
    /// them apart. [`Self::register_bit`] discards the distinction for callers
    /// that do not.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::ComponentTypeLimitExceeded`] when the 128-type
    /// limit is reached and no bit has been reclaimed by
    /// [`Self::remove`](ComponentRegistry::remove). Registration is driven by
    /// user data, so exceeding the limit is a configuration outcome, not a
    /// programming error - it is reported rather than panicked on.
    pub fn register<T: Component>(&mut self) -> Result<Registration, WorldError> {
        self.register_with_layout::<T>(&[])
    }

    /// Register a component type together with its declared field layout.
    ///
    /// The layout is recorded so a later registration of the same component -
    /// a hot-reload generation, or a second binary that linked the same shared
    /// type - can be checked against it.
    ///
    /// # Errors
    ///
    /// In addition to [`Self::register`]'s error, returns
    /// [`WorldError::SharedComponentLayoutMismatch`] when a component with a
    /// declared shared identity is registered a second time with a different
    /// memory shape. That check is what makes reaching one column from two
    /// binaries sound, so it is an error rather than a debug assertion.
    pub fn register_with_layout<T: Component>(
        &mut self,
        fields: &[ComponentFieldDescriptor],
    ) -> Result<Registration, WorldError> {
        // Step 1: Return the existing bit index when the type is already
        // registered, after checking that both registrations describe the
        // same memory shape.
        let component_id = ComponentId::of::<T>();
        let layout = ComponentLayout::of::<T>(fields);
        if let Some(&bit) = self.id_to_bit.get(&component_id) {
            // A shared name is a process-wide identity, so two types holding
            // it are one component: one bit, one column, and every write
            // through either landing on the other's rows. When their layouts
            // also agree, nothing downstream can notice - the reads succeed and
            // return another component's data.
            //
            // The legitimate case this must not reject is one type compiled
            // into two binaries, which is the entire point of shared identity.
            // Those agree on the type's own name - `Spline` is `Spline` in
            // whichever artifact compiled it - while two different components
            // do not. Only the final path segment is compared, because the
            // module path differs between an in-process stand-in and the real
            // cross-binary case while the type's name does not.
            if let Some(shared_name) = T::shared_name() {
                let incoming = Self::declaring_type_name::<T>();
                if let Some(existing) = self.shared_declaring_types.get(&component_id) {
                    if existing != incoming {
                        return Err(WorldError::SharedComponentNameConflict {
                            shared_name: shared_name.to_string(),
                            existing_type: existing.clone(),
                            incoming_type: incoming.to_string(),
                        });
                    }
                }
            }
            let recorded = self.layouts.get(&component_id).copied();
            match (recorded, T::shared_name()) {
                // A shared component reaching this branch is the whole point
                // of the feature: a second binary registering the type the
                // first one already owns. Both will read and write the same
                // rows through their own `T`, so a layout disagreement is a
                // misread waiting to happen and must stop the registration.
                (Some(recorded), Some(shared_name)) if !recorded.is_compatible_with(&layout) => {
                    return Err(WorldError::SharedComponentLayoutMismatch {
                        shared_name: shared_name.to_string(),
                        type_name: std::any::type_name::<T>().to_string(),
                        existing_size: recorded.size,
                        existing_align: recorded.align,
                        incoming_size: layout.size,
                        incoming_align: layout.align,
                    });
                }
                // An ordinary component keeps the identity of its `TypeId`, so
                // a layout change here means a reload replaced the definition
                // while the compiler happened to reuse the id. The persistence
                // migration is what handles that; the record is refreshed so
                // it describes the definition now in force.
                _ => {}
            }
            // A layout-less re-registration (`register::<T>()`, fields `&[]`)
            // records no schema hash of its own. Carry a recorded one forward:
            // the record may gain evidence, never lose it - otherwise the
            // shared-layout check above takes its permissive arm forever, and
            // two binaries that disagree about a component's fields register
            // silently.
            let mut merged = layout;
            if merged.schema_hash.is_none() {
                if let Some(recorded) = recorded {
                    merged.schema_hash = recorded.schema_hash;
                }
            }
            self.layouts.insert(component_id, merged);
            return Ok(Registration::AlreadyPresent(bit));
        }
        // Step 2: Assign the next bit - either one reclaimed by `remove`, or a
        // fresh one. When neither is available the 128-type ceiling is hit,
        // which is reported as a typed error rather than an assert.
        let Some(bit) = self.allocate_bit(std::any::type_name::<T>()) else {
            return Err(WorldError::ComponentTypeLimitExceeded {
                type_name: std::any::type_name::<T>().to_string(),
                count: self.id_to_bit.len() as u8,
            });
        };
        // Step 3: Record the type's metadata under the assigned bit.
        self.id_to_bit.insert(component_id, bit);
        // A shared component is recorded under its declared name, not
        // `std::any::type_name`: the declared name is what both binaries agree
        // on, and it is what every name-keyed lookup - persistence, the C#
        // bindings, the editor - must find it by.
        self.names
            .insert(component_id, Self::registered_name::<T>());
        self.layouts.insert(component_id, layout);
        if T::shared_name().is_some() {
            self.shared_declaring_types
                .insert(component_id, Self::declaring_type_name::<T>().to_string());
        }
        Ok(Registration::Created(bit))
    }

    /// Register a component type and return its bit, ignoring whether it was
    /// already present.
    ///
    /// The common case: callers that only need the bit index.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::ComponentTypeLimitExceeded`] when the 128-type
    /// limit is reached, as [`Self::register`] does.
    pub fn register_bit<T: Component>(&mut self) -> Result<u8, WorldError> {
        self.register::<T>().map(Registration::bit)
    }

    /// [`Self::register_with_layout`] for callers that only need the bit.
    pub fn register_bit_with_layout<T: Component>(
        &mut self,
        fields: &[ComponentFieldDescriptor],
    ) -> Result<u8, WorldError> {
        self.register_with_layout::<T>(fields)
            .map(Registration::bit)
    }

    /// The Rust type's own name, without its module path.
    ///
    /// `pill_spline::Spline` and `tests::module_copy::Spline` both yield
    /// `Spline`. The module path is deliberately dropped: it is what differs
    /// between an in-process stand-in for the cross-binary case and the real
    /// thing, while the type's name is what two copies of one type always
    /// share.
    pub fn declaring_type_name<T: ?Sized>() -> &'static str {
        let path = std::any::type_name::<T>();
        path.rsplit("::").next().unwrap_or(path)
    }

    /// The name a component type is registered under.
    ///
    /// Its declared shared name when it has one, otherwise
    /// [`std::any::type_name`]. A shared component must be findable by the
    /// name both binaries wrote down rather than by one binary's rendering of
    /// its Rust path.
    pub fn registered_name<T: Component>() -> String {
        T::shared_name()
            .map(str::to_string)
            .unwrap_or_else(|| std::any::type_name::<T>().to_string())
    }

    /// Register a component whose concrete type is defined outside Rust.
    ///
    /// # Errors
    ///
    /// Returns [`WorldError::ComponentTypeLimitExceeded`] when the 128-type
    /// limit is reached and no bit has been reclaimed by [`Self::remove`].
    pub fn register_dynamic(
        &mut self,
        stable_id: u128,
        name: impl Into<String>,
        size: usize,
    ) -> Result<u8, WorldError> {
        // Step 1: Return the existing bit index when the stable ID is already registered.
        let component_id = ComponentId::dynamic(stable_id);
        if let Some(&bit) = self.id_to_bit.get(&component_id) {
            return Ok(bit);
        }

        // Step 2: Assign the next bit - reclaimed or fresh - reporting the
        // ceiling as a typed error instead of panicking.
        let name = name.into();
        let Some(bit) = self.allocate_bit(&name) else {
            return Err(WorldError::ComponentTypeLimitExceeded {
                type_name: name,
                count: self.id_to_bit.len() as u8,
            });
        };

        // Step 3: Record the dynamic component's metadata.
        self.id_to_bit.insert(component_id, bit);
        self.names.insert(component_id, name);
        // A dynamic component's alignment lives with its storage factory, and
        // its schema hash is carried by the manifest, so the registry records
        // only what it is asked for here.
        self.layouts.insert(
            component_id,
            ComponentLayout {
                size,
                align: 1,
                schema_hash: None,
            },
        );
        Ok(bit)
    }

    /// Reserve one bit index for a newly registered type, preferring a bit
    /// reclaimed by [`Self::remove`] over a fresh one.
    ///
    /// Returns `None` once both the reclaimed pool and the fresh range are
    /// exhausted - that is, at the 128-type ceiling.
    fn allocate_bit(&mut self, _for_type: &str) -> Option<u8> {
        if let Some(bit) = self.free_bits.pop() {
            return Some(bit);
        }
        if self.next_bit < 128 {
            let bit = self.next_bit;
            self.next_bit += 1;
            return Some(bit);
        }
        None
    }

    /// Get the bit index for a component ID, if registered.
    /// Forget a previously registered component type.
    ///
    /// Called when a reloaded module or project stops registering a type
    /// entirely and the host drops its orphaned data. Re-registering the same
    /// `TypeId` later simply allocates a fresh bit index again.
    ///
    /// The freed bit is returned to the reclaim pool, so a dynamic manifest
    /// that retires and introduces types across reloads reuses bits instead of
    /// walking `next_bit` toward the 128-type ceiling.
    pub fn remove(&mut self, component_id: &ComponentId) {
        if let Some(bit) = self.id_to_bit.remove(component_id) {
            self.free_bits.push(bit);
        }
        self.names.remove(component_id);
        self.layouts.remove(component_id);
        self.shared_declaring_types.remove(component_id);
    }

    /// Number of component types that can still be registered before the
    /// 128-type ceiling is reached, counting both reclaimed bits and the
    /// unused fresh range.
    ///
    /// The host reports this so exhaustion is visible before it becomes fatal.
    pub fn available_slots(&self) -> usize {
        self.free_bits.len() + (128 - usize::from(self.next_bit))
    }

    pub fn get_bit(&self, component_id: &ComponentId) -> Option<u8> {
        self.id_to_bit.get(component_id).copied()
    }

    /// Get the type name of a registered component.
    pub fn get_name(&self, component_id: &ComponentId) -> Option<&str> {
        self.names.get(component_id).map(|s| s.as_str())
    }

    /// Get the size in bytes of a registered component type.
    pub fn get_size(&self, component_id: &ComponentId) -> Option<usize> {
        self.layouts.get(component_id).map(|layout| layout.size)
    }

    /// Get the recorded memory layout of a registered component type.
    pub fn get_layout(&self, component_id: &ComponentId) -> Option<ComponentLayout> {
        self.layouts.get(component_id).copied()
    }

    /// Republish a dynamic component's whole registry layout.
    ///
    /// The registry record and the storage factory describe one column, so a
    /// relayout that moves alignment or the schema hash has to move both:
    /// `get_layout`/`get_size` read here, and the placeholder alignment
    /// registration started with would describe a layout that never existed.
    /// The bit index, the name and the id stay put - replacing the
    /// registration instead would allocate a fresh bit index, and that index is
    /// baked into archetype masks and scheduled access masks.
    pub(crate) fn update_dynamic_layout(
        &mut self,
        component_id: &ComponentId,
        layout: &crate::archetype::DynamicComponentLayout,
    ) {
        if let Some(record) = self.layouts.get_mut(component_id) {
            record.size = layout.size;
            record.align = layout.align;
            record.schema_hash = Some(layout.schema_hash);
        }
    }

    /// Check whether a component type has been registered.
    ///
    /// Returns `true` if `T` has been registered via [`register`](Self::register).
    pub fn is_registered<T: Component>(&self) -> bool {
        self.id_to_bit.contains_key(&ComponentId::of::<T>())
    }

    /// Iterate over all registered components.
    ///
    /// Yields `(ComponentId, bit_index, type_name)` for each registered
    /// component type.  Useful for debugging and tooling.
    pub fn registered_components(&self) -> impl Iterator<Item = (ComponentId, u8, &str)> {
        self.id_to_bit.iter().map(|(id, &bit)| {
            (
                *id,
                bit,
                self.names.get(id).map(|s| s.as_str()).unwrap_or("?"),
            )
        })
    }

    /// Number of registered component types.
    #[inline]
    pub fn len(&self) -> usize {
        self.id_to_bit.len()
    }

    /// Returns true if no component types are registered.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.id_to_bit.is_empty()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies that `Tick` is exactly 4 bytes (a single u32).
    #[test]
    fn tick_size() {
        assert_eq!(std::mem::size_of::<Tick>(), 4);
        assert_eq!(std::mem::align_of::<Tick>(), 4);
    }

    /// Verifies that `ComponentTicks` is exactly 8 bytes (two u32 fields).
    #[test]
    fn component_ticks_size() {
        assert_eq!(std::mem::size_of::<ComponentTicks>(), 8);
        assert_eq!(std::mem::align_of::<ComponentTicks>(), 4);
    }

    /// `ComponentRegistry::remove` fully unregisters a type so a later
    /// re-registration works again; the entry is completely gone in between.
    /// Pins the registry cleanup behind drop-at-detection (audit 3.2).
    #[test]
    fn remove_unregisters_and_reregistration_works() {
        #[derive(Clone, Debug)]
        struct ReRegisterTestComponent;
        impl Component for ReRegisterTestComponent {}
        trait_type_map::impl_trait_accessible!(dyn Component; ReRegisterTestComponent);

        let mut registry = ComponentRegistry::new();
        let component_id = ComponentId::of::<ReRegisterTestComponent>();

        let first = registry.register::<ReRegisterTestComponent>().unwrap();
        assert!(first.is_new(), "the first registration creates the bit");
        let first_bit = first.bit();
        assert!(registry.is_registered::<ReRegisterTestComponent>());
        assert_eq!(registry.get_bit(&component_id), Some(first_bit));

        // Registering the same type again reports that, rather than looking
        // identical to a fresh registration.
        let repeat = registry.register::<ReRegisterTestComponent>().unwrap();
        assert_eq!(repeat, Registration::AlreadyPresent(first_bit));
        assert!(!repeat.is_new());

        registry.remove(&component_id);
        assert!(!registry.is_registered::<ReRegisterTestComponent>());
        assert_eq!(registry.get_bit(&component_id), None);
        assert_eq!(registry.get_name(&component_id), None);
        assert_eq!(registry.get_size(&component_id), None);

        // Re-registering works and reuses the freed bit: `remove` returns the
        // bit to the reclaim pool, so a dynamic manifest that retires and
        // introduces types across reloads does not walk `next_bit` toward the
        // 128-type ceiling.
        let second = registry.register::<ReRegisterTestComponent>().unwrap();
        assert!(second.is_new(), "after removal it is a fresh registration");
        let second_bit = second.bit();
        assert!(registry.is_registered::<ReRegisterTestComponent>());
        assert_eq!(registry.get_bit(&component_id), Some(second_bit));
        assert_eq!(first_bit, second_bit, "the freed bit is reused");
    }

    /// Registering the 129th component type reports a typed error instead of
    /// panicking - the ceiling is a configuration outcome, not a programming
    /// error (audit 4.2).
    #[test]
    fn the_129th_component_type_errors_instead_of_panicking() {
        // Each tuple is a distinct fake type (distinct names), registered
        // dynamically so the test does not need 129 real Rust types.
        let mut registry = ComponentRegistry::new();
        for index in 0..128 {
            let result =
                registry.register_dynamic(index as u128 + 1, format!("Project.FakeType{index}"), 4);
            assert!(result.is_ok(), "slot {index} must register");
        }

        // One more than the ceiling: the registry reports the limit, naming
        // the offending type and the current count.
        let error = registry
            .register_dynamic(u128::MAX, "Project.OneTooMany", 4)
            .unwrap_err();
        assert_eq!(
            error,
            WorldError::ComponentTypeLimitExceeded {
                type_name: "Project.OneTooMany".to_string(),
                count: 128,
            }
        );

        // A freed bit reopens a slot, so the same registry accepts a new type
        // after `remove` - the ceiling is not a permanent dead end. The first
        // registered type (stable id 1) holds bit 0, so freeing it reopens bit 0.
        registry.remove(&ComponentId::dynamic(1));
        let reused = registry
            .register_dynamic(u128::MAX - 1, "Project.AfterFree", 4)
            .unwrap();
        assert_eq!(reused, 0, "the reclaimed bit is handed out again");
    }

    /// Reclaimed bits keep the registry from walking toward the 128-type
    /// ceiling during a churn-heavy editing session (audit 4.10).
    #[test]
    fn removed_bits_are_reclaimed_and_reported_as_headroom() {
        let mut registry = ComponentRegistry::new();
        for index in 0..8 {
            registry
                .register_dynamic(index as u128 + 10, format!("Project.C{index}"), 4)
                .unwrap();
        }
        assert_eq!(registry.available_slots(), 120);

        // Remove half the types: their bits rejoin the pool.
        for index in 0..4 {
            registry.remove(&ComponentId::dynamic(index as u128 + 10));
        }
        assert_eq!(registry.available_slots(), 124);

        // New registrations reuse the reclaimed bits (LIFO: the most recently
        // freed first) rather than consuming fresh ones.
        let new_bit = registry.register_dynamic(999, "Project.New", 4).unwrap();
        assert_eq!(new_bit, 3, "the most recently freed bit is reused first");
        assert_eq!(registry.available_slots(), 123);
    }

    /// A layout-less re-registration must not disarm the shared-layout check:
    /// the recorded schema hash survives it, so a later declaration with the
    /// same size but a different field shape is still refused.
    #[test]
    fn schema_hash_survives_layout_less_reregistration() {
        // Two copies of one shared type, as two binaries compile them: same
        // final type name (so the name check passes) and same size, differing
        // only in the declared field shape the schema hash covers.
        mod first_copy {
            use crate::component::Component;

            /// A copy of the shared type as one binary would declare it.
            #[derive(Clone, Debug)]
            pub struct SharedProbe {
                #[allow(dead_code)]
                pub value: u32,
            }
            impl Component for SharedProbe {
                fn shared_name() -> Option<&'static str> {
                    Some("audit::SharedProbe")
                }
            }
            trait_type_map::impl_trait_accessible!(dyn Component; SharedProbe);
        }

        mod second_copy {
            use crate::component::Component;

            /// The same type as another binary would declare it: same name,
            /// different field shape.
            #[derive(Clone, Debug)]
            pub struct SharedProbe {
                #[allow(dead_code)]
                pub value: u32,
            }
            impl Component for SharedProbe {
                fn shared_name() -> Option<&'static str> {
                    Some("audit::SharedProbe")
                }
            }
            trait_type_map::impl_trait_accessible!(dyn Component; SharedProbe);
        }

        static FIELDS_A: &[ComponentFieldDescriptor] = &[ComponentFieldDescriptor {
            name: "first",
            type_tag: "u32",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 0,
        }];
        static FIELDS_B: &[ComponentFieldDescriptor] = &[ComponentFieldDescriptor {
            name: "second",
            type_tag: "u32",
            offset: 0,
            size: 4,
            align: 4,
            element_count: 0,
        }];

        let mut registry = ComponentRegistry::new();
        let _first_declaration = registry
            .register_with_layout::<first_copy::SharedProbe>(FIELDS_A)
            .expect("the first declaration is recorded");

        // The old bug: this registration carries no fields, so it used to
        // overwrite the recorded hash with `None`, after which the check below
        // took its permissive arm forever.
        let _re_registration = registry
            .register::<first_copy::SharedProbe>()
            .expect("re-registration is idempotent");

        let error = registry
            .register_with_layout::<second_copy::SharedProbe>(FIELDS_B)
            .expect_err("the hash recorded by the first declaration must survive");
        assert!(matches!(
            error,
            WorldError::SharedComponentLayoutMismatch { .. }
        ));
    }
}
