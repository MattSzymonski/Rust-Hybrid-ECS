//! How a module, the project and the renderer register with the engine.
//!
//! # Responsibilities
//!
//! - Name the [`SystemOwner`] each registering subject gets: extensions by
//!   load index, the renderer after them, the project unscoped.
//! - Run one entry point under its subject's registration scope.
//!
//! # Design
//!
//! A statically linked build and the reloading host register the same
//! subjects in the same order, and both build that order from these functions,
//! so the owners a system is attributed to - which drive scheduler ordering,
//! clearing and diagnostics - cannot drift between the two. Only how an entry
//! point is reached differs: a direct call here, a DLL export through the
//! engine API table in the host.

// External crates
use pill_engine::{Engine, SystemOwner};

// =============================================================================
// Owners
// =============================================================================

/// The owner of the extension loaded at `index`, counting from zero in load
/// order (the renderer's data crate is always the first).
pub fn extension_owner(index: usize) -> SystemOwner {
    SystemOwner::extension(index)
}

/// The owner of the renderer: the next one after the `extension_count`
/// extensions, as if it were one more extension.
pub fn renderer_owner(extension_count: usize) -> SystemOwner {
    SystemOwner::extension(extension_count)
}

/// The project's registration scope: none.
///
/// The project owns the scheduler outright rather than contributing a
/// module's worth of systems, so its registrations are not scoped; its systems
/// carry [`SystemOwner::PROJECT`].
pub const PROJECT_SCOPE: Option<SystemOwner> = None;

// =============================================================================
// Free Functions
// =============================================================================

/// Run one entry point under `scope`, returning the status it reported.
///
/// With a scope, every system the entry point registers is attributed to that
/// owner, which is what lets a reload clear exactly one subject's systems.
/// `init` is the entry point: a linked function, or a call through a loaded
/// library's export.
pub fn register_scoped(
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

    /// Extensions are one-based in load order, and the renderer takes the
    /// owner right after the last one.
    #[test]
    fn owners_follow_the_load_order() {
        for index in 0..4usize {
            assert_eq!(extension_owner(index).0, index as u64 + 1);
        }
        assert_eq!(renderer_owner(3), extension_owner(3));
        assert_ne!(renderer_owner(3), SystemOwner::PROJECT);
    }

    /// A scoped entry point's status is returned as reported, and the scope
    /// is closed again afterwards whatever it reported.
    #[test]
    fn a_scoped_entry_point_reports_its_status() {
        let mut engine = Engine::new();
        assert_eq!(
            register_scoped(&mut engine, Some(extension_owner(0)), |_| 7),
            7
        );
        assert_eq!(register_scoped(&mut engine, PROJECT_SCOPE, |_| 0), 0);
    }
}
