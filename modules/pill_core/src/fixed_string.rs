//! Inline, fixed-capacity strings for plain-data structs.
//!
//! # Responsibilities
//!
//! - Provide [`FixedString64`]: a UTF-8 string stored in the value itself,
//!   never on the heap.
//!
//! # Design
//!
//! A component or any other value the engine copies between artifacts must be
//! plain data: `#[repr(C)]`, no pointers, no owned allocation. A `String`
//! breaks that twice over. Its buffer belongs to the allocator of whichever
//! DLL created it, so a hot reload can leave one library freeing memory
//! another allocated; and a struct holding one has no meaningful C layout, so
//! it cannot be mirrored into C# or read across the module boundary.
//!
//! The answer is to inline the bytes. The value is `Copy`, has a stable
//! `#[repr(C)]` layout, and can be memcpy'd between archetypes exactly as a
//! `u32` field is. The cost is a ceiling on length, which is why the capacity
//! is in the type's name rather than hidden: a caller sees what it is choosing.
//!
//! ## Truncation rather than refusal
//!
//! [`FixedString64::new`] truncates a longer string instead of failing. A name
//! that does not fit is a content problem, not a reason to stop the frame, and
//! every call site would otherwise grow an error path it could not usefully
//! handle. Truncation always lands on a character boundary, so the result is
//! valid UTF-8 even when the input is cut mid-character - that is what lets
//! [`FixedString64::as_str`] be infallible.
//!
//! ## Serialization
//!
//! Serialized as the string it holds, not as its backing array, through
//! `#[serde(into/from)]`. Two reasons: `serde` implements its traits for
//! arrays only up to 32 elements, and a snapshot that says `"footstep"` rather
//! than 64 comma-separated bytes is one a person can read. The padding after
//! the live bytes carries no meaning and is not worth persisting.
//!
//! ## Usage
//!
//! ```
//! use pill_core::FixedString64;
//!
//! let name = FixedString64::new("footstep");
//! assert_eq!(name.as_str(), "footstep");
//! assert!(!name.is_empty());
//!
//! // Fits in a plain-data struct with no allocation anywhere.
//! #[repr(C)]
//! #[derive(Clone, Copy)]
//! struct AudioSource {
//!     volume: f32,
//!     sound: FixedString64,
//! }
//! ```

// External crates
use serde::{Deserialize, Serialize};

// =============================================================================
// FixedString64
// =============================================================================

/// Capacity of a [`FixedString64`], in bytes.
///
/// Bytes, not characters: a multi-byte character consumes several. 64 is long
/// enough for an asset name or a short path and keeps the struct one cache
/// line with room for a length.
pub const FIXED_STRING_64_CAPACITY: usize = 64;

/// A UTF-8 string of up to [`FIXED_STRING_64_CAPACITY`] bytes, stored inline.
///
/// `Copy` and `#[repr(C)]`, so it can live in a component, cross the module
/// boundary, and be mirrored into C# without any allocation. See the module
/// documentation for why a `String` cannot.
///
/// Comparison and hashing act on the live bytes only, so two values that
/// spell the same string are equal whatever their padding holds.
#[repr(C)]
#[derive(Clone, Copy, Serialize, Deserialize)]
#[serde(into = "String", from = "String")]
pub struct FixedString64 {
    /// UTF-8 bytes; only the first `length` are meaningful.
    ///
    /// The remainder is zeroed by every constructor, but nothing reads it, so
    /// two equal strings may still differ byte-for-byte here - which is why
    /// `PartialEq` is written by hand below.
    bytes: [u8; FIXED_STRING_64_CAPACITY],
    /// How many leading bytes of `bytes` are in use.
    ///
    /// `u8` suffices for a 64-byte capacity and keeps the struct compact.
    length: u8,
}

impl FixedString64 {
    /// The empty string.
    pub const fn empty() -> Self {
        Self {
            bytes: [0; FIXED_STRING_64_CAPACITY],
            length: 0,
        }
    }

    /// Store `text`, truncating at [`FIXED_STRING_64_CAPACITY`] bytes.
    ///
    /// Truncation steps back to a character boundary, so the stored value is
    /// always valid UTF-8. Use [`Self::try_new`] when a caller needs to know
    /// that the text did not fit.
    pub fn new(text: &str) -> Self {
        let limit = Self::boundary_at_or_below(text, text.len().min(FIXED_STRING_64_CAPACITY));
        let mut bytes = [0; FIXED_STRING_64_CAPACITY];
        bytes[..limit].copy_from_slice(&text.as_bytes()[..limit]);
        Self {
            bytes,
            length: limit as u8,
        }
    }

    /// Store `text`, or `None` when it does not fit.
    ///
    /// The checking counterpart of [`Self::new`], for a call site that would
    /// rather reject a name than silently shorten it - a content pipeline
    /// validating input, say.
    pub fn try_new(text: &str) -> Option<Self> {
        (text.len() <= FIXED_STRING_64_CAPACITY).then(|| Self::new(text))
    }

    /// The stored text.
    pub fn as_str(&self) -> &str {
        // Infallible in practice: every constructor truncates on a character
        // boundary, so the live bytes are always valid UTF-8. The fallback
        // keeps that an invariant rather than a panic if one is ever added
        // that does not.
        std::str::from_utf8(&self.bytes[..self.length as usize]).unwrap_or("")
    }

    /// The stored text as bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.length as usize]
    }

    /// How many bytes are in use.
    pub const fn len(&self) -> usize {
        self.length as usize
    }

    /// Whether the string is empty.
    pub const fn is_empty(&self) -> bool {
        self.length == 0
    }

    /// The largest character boundary of `text` at or below `limit`.
    ///
    /// Shared by the constructors so there is one definition of where a
    /// truncation may land.
    fn boundary_at_or_below(text: &str, limit: usize) -> usize {
        let mut limit = limit;
        while limit > 0 && !text.is_char_boundary(limit) {
            limit -= 1;
        }
        limit
    }
}

impl Default for FixedString64 {
    fn default() -> Self {
        Self::empty()
    }
}

// --- Conversions ---

/// The serde target; see the module documentation.
impl From<FixedString64> for String {
    fn from(value: FixedString64) -> Self {
        value.as_str().to_owned()
    }
}

/// Truncating as [`FixedString64::new`] does, so a value written by a build
/// with a larger capacity still loads.
impl From<String> for FixedString64 {
    fn from(value: String) -> Self {
        Self::new(&value)
    }
}

impl From<&str> for FixedString64 {
    fn from(value: &str) -> Self {
        Self::new(value)
    }
}

impl std::str::FromStr for FixedString64 {
    /// Truncation is not an error, so parsing one cannot fail.
    type Err = std::convert::Infallible;

    fn from_str(text: &str) -> Result<Self, Self::Err> {
        Ok(Self::new(text))
    }
}

impl AsRef<str> for FixedString64 {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl std::ops::Deref for FixedString64 {
    type Target = str;

    fn deref(&self) -> &str {
        self.as_str()
    }
}

// --- Comparison ---

/// Compares the live bytes only.
///
/// The padding past `length` is not part of the value: a string built by
/// `new` and the same string loaded from a snapshot must compare equal
/// whatever happens to sit behind them.
impl PartialEq for FixedString64 {
    fn eq(&self, other: &Self) -> bool {
        self.as_bytes() == other.as_bytes()
    }
}

impl Eq for FixedString64 {}

impl PartialEq<str> for FixedString64 {
    fn eq(&self, other: &str) -> bool {
        self.as_str() == other
    }
}

impl PartialEq<&str> for FixedString64 {
    fn eq(&self, other: &&str) -> bool {
        self.as_str() == *other
    }
}

/// Hashes the live bytes, so it agrees with the hand-written `PartialEq`.
impl std::hash::Hash for FixedString64 {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.as_str().hash(state);
    }
}

impl Ord for FixedString64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.as_str().cmp(other.as_str())
    }
}

impl PartialOrd for FixedString64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

// --- Formatting ---

impl std::fmt::Display for FixedString64 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// Prints the string, not the backing array - a 64-element byte dump in a log
/// line is noise, and the padding means nothing.
impl std::fmt::Debug for FixedString64 {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(formatter, "{:?}", self.as_str())
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A string within capacity is stored and read back unchanged.
    #[test]
    fn stores_and_returns_a_short_string() {
        let value = FixedString64::new("footstep");
        assert_eq!(value.as_str(), "footstep");
        assert_eq!(value.len(), 8);
        assert!(!value.is_empty());
    }

    /// The empty string is the default, and reads as empty rather than as
    /// whatever the zeroed buffer would decode to.
    #[test]
    fn empty_is_the_default() {
        let value = FixedString64::default();
        assert_eq!(value.as_str(), "");
        assert!(value.is_empty());
        assert_eq!(value.len(), 0);
        assert_eq!(FixedString64::empty(), value);
    }

    /// A string exactly at capacity fits whole; the boundary is inclusive.
    #[test]
    fn a_string_at_capacity_is_kept_whole() {
        let text = "a".repeat(FIXED_STRING_64_CAPACITY);
        let value = FixedString64::new(&text);
        assert_eq!(value.len(), FIXED_STRING_64_CAPACITY);
        assert_eq!(value.as_str(), text);
    }

    /// One byte past capacity is truncated rather than rejected.
    #[test]
    fn an_over_long_string_is_truncated() {
        let text = "a".repeat(FIXED_STRING_64_CAPACITY + 1);
        let value = FixedString64::new(&text);
        assert_eq!(value.len(), FIXED_STRING_64_CAPACITY);
        assert!(text.starts_with(value.as_str()));
    }

    /// The property that makes `as_str` infallible: a cut that would land
    /// mid-character steps back instead, so the result is always valid UTF-8.
    #[test]
    fn truncation_lands_on_a_character_boundary() {
        // Three bytes per character, so 64 is not a whole number of them and
        // a naive cut would split the 22nd.
        let text = "\u{4f60}".repeat(FIXED_STRING_64_CAPACITY);
        let value = FixedString64::new(&text);

        assert!(value.len() <= FIXED_STRING_64_CAPACITY);
        // 21 whole characters is 63 bytes; the 22nd would need 66.
        assert_eq!(value.len(), 63);
        assert_eq!(value.as_str().chars().count(), 21);
        assert!(text.starts_with(value.as_str()));
    }

    /// `try_new` is the checking counterpart, for a caller that would rather
    /// reject than shorten.
    #[test]
    fn try_new_refuses_what_does_not_fit() {
        assert!(FixedString64::try_new("footstep").is_some());
        assert!(FixedString64::try_new(&"a".repeat(FIXED_STRING_64_CAPACITY)).is_some());
        assert!(FixedString64::try_new(&"a".repeat(FIXED_STRING_64_CAPACITY + 1)).is_none());
    }

    /// Equality reads the live bytes only. Two values spelling one string must
    /// compare equal even if their padding differs - which it does here,
    /// because the longer string wrote bytes the shorter one never cleared.
    #[test]
    fn equality_ignores_padding() {
        let long = FixedString64::new("footstep_sound_effect");
        let mut same = FixedString64::new("footstep_sound_effect");
        // Dirty the padding the way a reused buffer would.
        same.bytes[30] = b'x';

        assert_eq!(long, same);

        use std::collections::hash_map::DefaultHasher;
        use std::hash::{Hash, Hasher};
        let hash_of = |value: &FixedString64| {
            let mut hasher = DefaultHasher::new();
            value.hash(&mut hasher);
            hasher.finish()
        };
        assert_eq!(hash_of(&long), hash_of(&same));
    }

    /// Comparing against a plain string is the common case and needs no
    /// conversion at the call site.
    #[test]
    fn compares_against_str() {
        let value = FixedString64::new("footstep");
        assert_eq!(value, "footstep");
        assert_ne!(value, "splash");
        assert_eq!(value.as_str(), "footstep");
    }

    /// Ordering follows the string, not the byte array, so a sorted list reads
    /// the way a person expects.
    #[test]
    fn orders_lexicographically() {
        let mut values = [
            FixedString64::new("cherry"),
            FixedString64::new("apple"),
            FixedString64::new("banana"),
        ];
        values.sort();
        assert_eq!(values[0], "apple");
        assert_eq!(values[1], "banana");
        assert_eq!(values[2], "cherry");
    }

    /// Serialized as a plain string, so a snapshot is readable and the
    /// 32-element array limit in serde never comes up.
    #[test]
    fn serializes_as_a_string() {
        let value = FixedString64::new("footstep");
        let json = serde_json::to_string(&value).expect("serialize");
        assert_eq!(json, "\"footstep\"");

        let restored: FixedString64 = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored, value);
    }

    /// A value written by a build with a larger capacity still loads, cut to
    /// what fits rather than failing the whole snapshot.
    #[test]
    fn deserializing_an_over_long_string_truncates() {
        let json = format!("\"{}\"", "a".repeat(FIXED_STRING_64_CAPACITY + 10));
        let restored: FixedString64 = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(restored.len(), FIXED_STRING_64_CAPACITY);
    }

    /// The point of the type: plain data, no pointer, stable layout.
    #[test]
    fn is_plain_data() {
        // One byte per capacity slot plus the length, rounded to alignment 1.
        assert_eq!(
            std::mem::size_of::<FixedString64>(),
            FIXED_STRING_64_CAPACITY + 1
        );
        assert_eq!(std::mem::align_of::<FixedString64>(), 1);

        // `Copy`, so an archetype move is a memcpy.
        let value = FixedString64::new("footstep");
        let copy = value;
        assert_eq!(value, copy);
    }

    /// `Deref` to `str` means the whole string API is available without an
    /// explicit `as_str` at every call.
    #[test]
    fn derefs_to_str() {
        let value = FixedString64::new("footstep");
        assert!(value.starts_with("foot"));
        assert_eq!(value.to_uppercase(), "FOOTSTEP");
    }

    /// `Display` writes the text; `Debug` quotes it rather than dumping the
    /// backing array.
    #[test]
    fn formats_as_its_text() {
        let value = FixedString64::new("footstep");
        assert_eq!(format!("{value}"), "footstep");
        assert_eq!(format!("{value:?}"), "\"footstep\"");
    }
}
