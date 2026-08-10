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

// =============================================================================
// Component
// =============================================================================

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
}

// =============================================================================
// ComponentId
// =============================================================================

/// Type-erased identifier for a registered component type.
///
/// Can be converted to/from [`TypeKey`] for code that needs to be generic
/// over both component and resource type identifiers.
///
/// [`TypeKey`]: crate::scheduler::TypeKey
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ComponentId(pub TypeId);

impl ComponentId {
    pub fn of<T: Component>() -> Self {
        ComponentId(TypeId::of::<T>())
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
    id_to_bit: HashMap<ComponentId, u8>,
    names: HashMap<ComponentId, String>,
    sizes: HashMap<ComponentId, usize>,
    next_bit: u8,
}

impl ComponentRegistry {
    pub fn new() -> Self {
        Self {
            id_to_bit: HashMap::new(),
            names: HashMap::new(),
            sizes: HashMap::new(),
            next_bit: 0,
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
    /// Returns the bit index, or the existing index if already registered.
    pub fn register<T: Component>(&mut self) -> u8 {
        let component_id = ComponentId::of::<T>();
        if let Some(&bit) = self.id_to_bit.get(&component_id) {
            return bit;
        }
        assert!(
            self.next_bit < 128,
            "Component type limit exceeded: cannot register {} (max 128 component types). \
             Consider combining related components or using a component with interior data variants.",
            std::any::type_name::<T>()
        );
        let bit = self.next_bit;
        self.id_to_bit.insert(component_id, bit);
        self.names
            .insert(component_id, std::any::type_name::<T>().to_string());
        self.sizes.insert(component_id, std::mem::size_of::<T>());
        self.next_bit += 1;
        bit
    }

    /// Get the bit index for a component ID, if registered.
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
