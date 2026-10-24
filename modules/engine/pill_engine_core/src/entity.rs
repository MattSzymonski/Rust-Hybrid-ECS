//! Lightweight entity handles with generation-based invalidation.
//!
//! # Responsibilities
//!
//! - Defines the [`Entity`] handle type used throughout the ECS.
//! - Defines [`EntityId`], the stable identity an [`Entity`] carries for a
//!   lifetime longer than the handle (a saved reference, a network message).
//! - Provides generation-counter logic to prevent dangling-handle bugs.
//! - Keeps the handle compact (16 bytes) for cheap copy and storage.
//!
//! # Design
//!
//! Entities are not objects - they are 64-bit IDs paired with a 32-bit
//! generation counter. When an entity is destroyed, its ID is recycled via
//! a free list and the generation is incremented. Any old handle still
//! holding the previous generation will fail validation, preventing
//! use-after-free bugs without reference counting or garbage collection.
//!
//! A handle is therefore cheap but meaningless outside its run: the slot it
//! names is reused. [`EntityId`] is the opposite trade - 128 random bits that
//! are never reused, so data written for a later run can name one entity
//! without depending on the order the world happened to spawn them in.

// Standard library
use std::fmt;

// =============================================================================
// Entity
// =============================================================================

/// Lightweight handle referencing a collection of components in the [`World`].
///
/// Entities are 16 bytes, [`Copy`], and cheap to pass by value. The
/// generation counter disambiguates recycled IDs:
///
/// ```
/// # use pill_engine::*;
/// # #[derive(Debug, Clone)] struct Health(f32);
/// # impl Component for Health {}
/// # #[derive(Debug, Clone)] struct Damage(f32);
/// # impl Component for Damage {}
/// # let mut world = World::new();
/// # world.register_component::<Health>();
/// # world.register_component::<Damage>();
/// let enemy = world.create_entity().with(Health(100.0)).build().unwrap();
/// world.destroy_entity(enemy);
///
/// let bullet = world.create_entity().with(Damage(10.0)).build().unwrap();
///
/// // Old handle is safely invalidated:
/// assert!(!world.is_entity_valid(enemy));
/// ```
#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Entity {
    /// Unique numeric identifier for this entity slot.
    ///
    /// IDs are recycled when entities are destroyed to prevent unbounded growth.
    /// The same ID may be reused for different entities over time, distinguished
    /// by the `generation` field.
    pub(crate) id: u64,

    /// Generation counter to distinguish reused entity IDs.
    ///
    /// Incremented each time an entity ID is recycled. This allows detecting
    /// "stale" entity handles that reference a destroyed entity whose ID was
    /// reused for a new entity. A handle is valid only if both id AND generation
    /// match the current entity at that slot.
    pub(crate) generation: u32,
}

// =============================================================================
// Entity - Inherent Implementations
// =============================================================================

impl Entity {
    // -------------------------------------------------------------------------
    // Construction
    // -------------------------------------------------------------------------

    /// Creates an entity handle for use in test fixtures only.
    #[cfg(test)]
    pub(crate) fn new_for_test(id: u64, generation: u32) -> Self {
        Self { id, generation }
    }

    /// Rebuilds a handle from its parts, for callers that persist handles
    /// across process restarts and check them against the world themselves
    /// (the editor's state recovery): the world answers `false` from
    /// [`World::is_entity_valid`] until a live entity carries both numbers
    /// again.
    #[inline]
    pub const fn from_raw(id: u64, generation: u32) -> Self {
        Self { id, generation }
    }

    // -------------------------------------------------------------------------
    // Property accessors
    // -------------------------------------------------------------------------

    /// Returns the numeric slot identifier for this entity.
    ///
    /// Not unique on its own - pair with [`generation`](Self::generation)
    /// for a fully unique handle.
    #[inline]
    pub fn id(self) -> u64 {
        self.id
    }

    /// Returns the generation counter that disambiguates recycled entity IDs.
    #[inline]
    pub fn generation(self) -> u32 {
        self.generation
    }
}

// =============================================================================
// EntityId
// =============================================================================

/// Stable 128-bit identity of one entity, for references that outlive a run.
///
/// An [`Entity`] is a slot and a generation: cheap, and meaningless to a world
/// that did not issue it. An `EntityId` is the identity data meant for a later
/// run carries instead - a scene file, a spawn table, a saved selection - so a
/// reference finds its entity again after the world is rebuilt and the slots
/// are handed out in a different order.
///
/// The id is random, not derived from the slot: an id derived from the slot
/// would name whoever took that slot next, and a stale reference would then
/// address the wrong entity instead of nothing. It is assigned when the entity
/// is created, unless the creator supplies one (a scene load does), and it is
/// indexable both ways by the world that holds it.
///
/// # Examples
///
/// ```
/// # use pill_engine::*;
/// # let mut world = World::new();
/// let entity = world.create_entity().build().unwrap();
/// let id = world.entity_id(entity).expect("the entity has one");
/// assert_eq!(world.entity_by_id(id), Some(entity));
/// ```
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct EntityId(u128);

impl EntityId {
    /// Wrap an id a file or a spawn table already recorded.
    pub const fn new(value: u128) -> Self {
        Self(value)
    }

    /// Derive an id from a name, reusing the shared-identity hash.
    ///
    /// What the world falls back to when the operating system has no random
    /// source: unique per name, and reproducible from the same name.
    pub const fn from_name(name: &str) -> Self {
        Self(crate::component::shared_component_identity(name))
    }

    /// The raw 128-bit value.
    pub const fn value(self) -> u128 {
        self.0
    }

    /// A new id from the operating system's random source.
    ///
    /// # Errors
    ///
    /// Returns the `getrandom` error when the platform has no usable random
    /// source; the world then mints one from a counter and warns once.
    pub fn random() -> Result<Self, getrandom::Error> {
        let mut bytes = [0u8; 16];
        getrandom::fill(&mut bytes)?;
        Ok(Self(u128::from_le_bytes(bytes)))
    }

    /// Parse the 32-digit hexadecimal form [`Display`](std::fmt::Display)
    /// writes, or `None` for any other text.
    pub fn parse(text: &str) -> Option<Self> {
        // Exactly 32 hex digits: `from_str_radix` alone would also accept a
        // sign and a shorter string.
        if text.len() != 32 || !text.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            return None;
        }
        u128::from_str_radix(text, 16).ok().map(Self)
    }
}

impl fmt::Display for EntityId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:032x}", self.0)
    }
}

// Serialized as the 32-digit hex string rather than a number: a 128-bit integer
// does not survive JSON number handling in most other tools, C# included.
impl serde::Serialize for EntityId {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> serde::Deserialize<'de> for EntityId {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let text = String::deserialize(deserializer)?;
        Self::parse(&text).ok_or_else(|| {
            serde::de::Error::custom(format!(
                "`{text}` is not an entity id (expected 32 hexadecimal digits)"
            ))
        })
    }
}

// =============================================================================
// Entity - Trait Implementations
// =============================================================================

impl fmt::Display for Entity {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}v{}", self.id, self.generation)
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Verifies that `Entity` is 16 bytes with 8-byte alignment.
    #[test]
    fn entity_size_and_alignment() {
        assert_eq!(
            std::mem::size_of::<Entity>(),
            16,
            "Entity size changed - check field order"
        );
        assert_eq!(std::mem::align_of::<Entity>(), 8);
    }

    /// An id round-trips through the hex form the saved data uses, and refuses
    /// anything that is not exactly 32 hex digits.
    #[test]
    fn an_entity_id_round_trips_through_hex() {
        let id = EntityId::new(0x0123_4567_89ab_cdef_fedc_ba98_7654_3210);
        assert_eq!(
            id.to_string(),
            "0123456789abcdeffedcba9876543210",
            "an id prints as 32 hex digits"
        );
        assert_eq!(EntityId::parse(&id.to_string()), Some(id));
        assert_eq!(EntityId::parse("0123"), None, "short text is not an id");
        assert_eq!(EntityId::parse(&"z".repeat(32)), None, "not hex");
    }

    /// Random ids are not derived from anything about the entity, so two
    /// worlds spawning the same entities do not produce the same ids.
    #[test]
    fn random_ids_differ() {
        let first = EntityId::random().expect("the platform has a random source");
        let second = EntityId::random().expect("the platform has a random source");
        assert_ne!(first, second);
    }

    /// Ids serialize as the hex string, not as a JSON number, so a 128-bit
    /// value survives a reader with 64-bit numbers.
    #[test]
    fn an_entity_id_serializes_as_hex_text() {
        let id = EntityId::new(7);
        let text = serde_json::to_string(&id).expect("an id serializes");
        assert_eq!(text, "\"00000000000000000000000000000007\"");
        assert_eq!(serde_json::from_str::<EntityId>(&text).ok(), Some(id));
    }
}
