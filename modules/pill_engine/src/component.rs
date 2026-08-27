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
pub trait Component: Send + 'static {}

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
/// use the stable 128-bit identity supplied by their external manifest.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum ComponentId {
    /// Component backed by a concrete Rust type.
    Native(TypeId),
    /// Component described at runtime by an external language manifest.
    Dynamic(u128),
}

impl ComponentId {
    /// Returns the [`ComponentId`] for the concrete Rust type `T`.
    pub fn of<T: 'static>() -> Self {
        Self::Native(TypeId::of::<T>())
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

    /// Returns the wrapped [`TypeId`] if this is a native component ID, or
    /// `None` for dynamically registered components.
    pub const fn native_type_id(self) -> Option<TypeId> {
        match self {
            Self::Native(type_id) => Some(type_id),
            Self::Dynamic(_) => None,
        }
    }
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
    sizes: HashMap<ComponentId, usize>,
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
            sizes: HashMap::new(),
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
    ///
    /// # Panics (debug only)
    ///
    /// Panics when a type is re-registered with a different size than was
    /// recorded the first time. That means a hot reload replaced the
    /// definition without the registry noticing: the stored layout is now
    /// stale, and everything reading size from here - the byte-level bindings
    /// handed to C#, the persistence migration - would work from the old one.
    pub fn register<T: Component>(&mut self) -> Result<Registration, WorldError> {
        // Step 1: Return the existing bit index when the type is already registered.
        let component_id = ComponentId::of::<T>();
        if let Some(&bit) = self.id_to_bit.get(&component_id) {
            debug_assert_eq!(
                self.sizes.get(&component_id).copied(),
                Some(std::mem::size_of::<T>()),
                "component {} was re-registered with a different size; the                  recorded layout is stale",
                std::any::type_name::<T>()
            );
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
        self.names
            .insert(component_id, std::any::type_name::<T>().to_string());
        self.sizes.insert(component_id, std::mem::size_of::<T>());
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
        self.sizes.insert(component_id, size);
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
        self.sizes.remove(component_id);
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
        self.sizes.get(component_id).copied()
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
}
