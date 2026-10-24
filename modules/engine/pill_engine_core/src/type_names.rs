//! Type names for the engine's own types, written under the public crate name.
//!
//! # Responsibilities
//!
//! - Give [`stable_type_name`](crate::type_names::stable_type_name), the
//!   [`std::any::type_name`] the engine uses wherever a type name becomes
//!   data: a registered component name, a persisted resource's key, the
//!   namespace of a generated C# mirror.
//!
//! # Design
//!
//! The engine's code lives in `pill_engine_core`, so `type_name` reports its
//! types as `pill_engine_core::common_components::Position`. Everything else
//! knows them by the public path, `pill_engine::common_components::Position`:
//! C# projects import the namespace `pill_engine.common_components`, and
//! snapshots and editor names were written under it.
//! [`stable_type_name`](crate::type_names::stable_type_name) keeps that path
//! by writing the core's crate name as `pill_engine`.
//!
//! Names of types outside the engine pass through unchanged and without an
//! allocation; a rewritten name is built once per type and kept for the
//! process lifetime.

// Standard library
use std::collections::HashMap;
use std::sync::{Mutex, OnceLock, PoisonError};

/// The crate name `type_name` reports for the engine's own types.
const CORE_CRATE_PREFIX: &str = "pill_engine_core::";

/// The crate name the rest of the engine and every project use instead.
const PUBLIC_CRATE_PREFIX: &str = "pill_engine::";

/// [`std::any::type_name`] of `T`, with every `pill_engine_core::` path
/// written as `pill_engine::`.
pub fn stable_type_name<T: ?Sized>() -> &'static str {
    let name = std::any::type_name::<T>();
    if !name.contains(CORE_CRATE_PREFIX) {
        return name;
    }
    rewritten_name(name)
}

/// The rewritten form of a `type_name` that names the core, built once per
/// distinct name and leaked so it can be handed out as `&'static str`.
fn rewritten_name(name: &'static str) -> &'static str {
    static REWRITTEN: OnceLock<Mutex<HashMap<&'static str, &'static str>>> = OnceLock::new();
    let mut rewritten = REWRITTEN
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    rewritten.entry(name).or_insert_with(|| {
        Box::leak(
            name.replace(CORE_CRATE_PREFIX, PUBLIC_CRATE_PREFIX)
                .into_boxed_str(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The engine's own types are named under the public crate name, generic
    /// arguments included.
    #[test]
    fn engine_types_use_the_public_crate_name() {
        assert_eq!(
            stable_type_name::<crate::Position>(),
            "pill_engine::common_components::Position"
        );
        assert_eq!(
            stable_type_name::<Vec<crate::Position>>(),
            "alloc::vec::Vec<pill_engine::common_components::Position>"
        );
    }

    /// Other types keep `type_name`'s spelling, and a repeated call returns the
    /// same string.
    #[test]
    fn other_types_are_unchanged_and_names_are_reused() {
        assert_eq!(stable_type_name::<u32>(), "u32");
        assert!(std::ptr::eq(
            stable_type_name::<crate::Position>(),
            stable_type_name::<crate::Position>()
        ));
    }
}
