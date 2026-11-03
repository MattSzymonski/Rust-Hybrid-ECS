//! Type names for the engine's own types, written under the public crate name.
//!
//! # Responsibilities
//!
//! - [`stable_type_name`](crate::type_names::stable_type_name): the type name
//!   the engine stores as data. A component or a persisted resource registers
//!   under it unless it declares a shared name of its own.
//!
//! # Design
//!
//! The engine's code lives in `pill_engine_core`, so `std::any::type_name`
//! reports one of its types as
//! `pill_engine_core::common_components::TransformComponent`. Code outside the
//! engine names the same type by the public path,
//! `pill_engine::common_components::TransformComponent`. C# projects import
//! the namespace `pill_engine.common_components`, and the component names and
//! persisted keys the managed side receives are spelled that way.
//! [`stable_type_name`](crate::type_names::stable_type_name) produces the
//! public form by writing the core's crate name as `pill_engine`.
//!
//! A type outside the engine keeps the name `std::any::type_name` gives it,
//! and no allocation is made for it. A rewritten name is built once per
//! distinct type name and kept for the rest of the program's life.

// Standard library
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};

// =============================================================================
// Constants
// =============================================================================

/// The crate prefix [`std::any::type_name`] gives the engine's own types.
const CORE_CRATE_PREFIX: &str = "pill_engine_core::";

/// The crate prefix a type is reported under outside the engine.
const PUBLIC_CRATE_PREFIX: &str = "pill_engine::";

// =============================================================================
// Free Functions
// =============================================================================

/// [`std::any::type_name`] of `T`, with every `pill_engine_core::` path
/// written as `pill_engine::`.
///
/// Only a name that mentions the core's crate is rewritten. Any other name is
/// returned as it is, without an allocation.
pub fn stable_type_name<T: ?Sized>() -> &'static str {
    let type_name = std::any::type_name::<T>();
    if !type_name.contains(CORE_CRATE_PREFIX) {
        return type_name;
    }
    rewritten_type_name(type_name)
}

/// The rewritten form of a core type name, built once per distinct name.
///
/// The rewritten text is kept for the rest of the program's life, so it can be
/// returned as `&'static str`.
fn rewritten_type_name(type_name: &'static str) -> &'static str {
    static REWRITTEN_NAMES: OnceLock<Mutex<HashMap<&'static str, &'static str>>> = OnceLock::new();
    let mut rewritten_names = REWRITTEN_NAMES
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    rewritten_names.entry(type_name).or_insert_with(|| {
        Box::leak(
            type_name
                .replace(CORE_CRATE_PREFIX, PUBLIC_CRATE_PREFIX)
                .into_boxed_str(),
        )
    })
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine's own types are named under the public crate name, including
    /// when one appears as a generic argument.
    #[test]
    fn engine_types_use_the_public_crate_name() {
        assert_eq!(
            stable_type_name::<crate::TransformComponent>(),
            "pill_engine::common_components::TransformComponent"
        );
        assert_eq!(
            stable_type_name::<Vec<crate::TransformComponent>>(),
            "alloc::vec::Vec<pill_engine::common_components::TransformComponent>"
        );
    }

    /// A type outside the engine keeps the spelling `std::any::type_name`
    /// gives it, and two calls for the same type return the same string.
    #[test]
    fn other_types_are_unchanged_and_names_are_reused() {
        assert_eq!(stable_type_name::<u32>(), "u32");
        assert!(std::ptr::eq(
            stable_type_name::<crate::TransformComponent>(),
            stable_type_name::<crate::TransformComponent>()
        ));
    }
}
