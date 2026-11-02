//! Under which owner a module, the renderer or the project registers its ECS systems.
//!
//! # Responsibilities
//!
//! - Give each registering part the owner its ECS systems are recorded under.
//! - Run one entry point with that owner in effect.
//!
//! # Design
//!
//! An owner is the tag an ECS system carries to name what registered it: one
//! extension, the renderer, the project, or the engine for its own systems. A
//! system records whichever owner was in effect while the entry point that
//! declared it ran, which is why the caller has to set one around that call
//! ([`register_module`]).
//!
//! The owner decides what a reload clears, what a pause suspends, and what the
//! editor's system list labels a row with. A statically linked build and the
//! reloading host take it from [`extension_owner`] and [`renderer_owner`], so a
//! module registers under the same owner in either mode.

// External crates
use pill_engine::{Engine, SystemOwner};

// =============================================================================
// Owners
// =============================================================================

/// The project's scope: no owner set, so its ECS systems carry
/// [`SystemOwner::PROJECT`].
pub const PROJECT_SCOPE: Option<SystemOwner> = None;

/// The owner of the extension at `index`, counting from zero in load order
/// (with a renderer, its data crate is first).
pub fn extension_owner(index: usize) -> SystemOwner {
    SystemOwner::extension(index)
}

/// The owner of the systems a renderer registers.
///
/// Reserved by the engine rather than taken as "one past the extensions"
/// ([`SystemOwner::RENDERER`]). A host that skipped an extension keeps that
/// module's owner in reserve until it loads, so a count of the loaded
/// extensions does not name the next free index - deriving the renderer's owner
/// from it aliased the pending module's, and a reload of either subject then
/// cleared the other's systems.
pub fn renderer_owner() -> SystemOwner {
    SystemOwner::RENDERER
}

// =============================================================================
// Free Functions
// =============================================================================

/// Run `init` with `scope`'s owner in effect, returning the status it
/// reported.
///
/// Every ECS system `init` registers is recorded under that owner. With no
/// scope nothing is set, and the systems carry [`SystemOwner::PROJECT`] - the
/// owner the engine uses outside a module's registration.
pub fn register_module(
    engine: &mut Engine,
    scope: Option<SystemOwner>,
    init: impl FnOnce(&mut Engine) -> u32,
) -> u32 {
    match scope {
        Some(owner) => {
            engine.begin_module_registration(owner);
            let status = init(engine);
            engine.end_module_registration();
            status
        }
        None => init(engine),
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// Extensions are one-based, and the renderer's owner is reserved outside
    /// the extension range whatever the host loaded.
    #[test]
    fn owners_follow_the_load_order() {
        for index in 0..4usize {
            assert_eq!(extension_owner(index).0, index as u64 + 1);
            assert_ne!(renderer_owner(), extension_owner(index));
            assert_eq!(extension_owner(index).extension_index(), Some(index));
        }
        assert_eq!(renderer_owner(), SystemOwner::RENDERER);
        assert_eq!(renderer_owner().extension_index(), None);
        assert_ne!(renderer_owner(), SystemOwner::PROJECT);
        assert_ne!(renderer_owner(), SystemOwner::ENGINE);
    }

    /// A scoped entry point's status is returned, and the unscoped path still
    /// runs its entry point.
    #[test]
    fn a_scoped_entry_point_reports_its_status() {
        let mut engine = Engine::new();
        assert_eq!(
            register_module(&mut engine, Some(extension_owner(0)), |_| 7),
            7
        );
        assert_eq!(register_module(&mut engine, PROJECT_SCOPE, |_| 0), 0);
    }
}
