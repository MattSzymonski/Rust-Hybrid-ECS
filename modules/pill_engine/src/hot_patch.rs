//! Hot-patch machinery, re-exported so generated code keeps one path.
//!
//! # Responsibilities
//!
//! - Re-exports every item of [`pill_hot_runtime`] under the path the
//!   `#[pill_hot]`/`#[pill_hot_fn]` macros and `pill_hot_scan` emit.
//! - Owns the signature-identity helpers, which need this crate's system traits.
//!
//! # Design
//!
//! Two independent code generators emit `::pill_engine::hot_patch::...` into
//! user crates: `pill_engine_macros` and the build-script inventory produced by
//! `pill_hot_scan`. That path behaves as a published interface even though
//! nothing declares it one, so the machinery moved into its own crate and this
//! module keeps the path stable. The re-export is permanent by design.
//!
//! What did not move is below: [`signature_hash`], [`signature_hash_of`] and
//! [`local_implementation_address`] are bounded by
//! [`SystemParamFunction`](crate::system::SystemParamFunction), which is this
//! crate's own trait. They are about systems, not about patching, so keeping
//! them here is what let everything else leave.

// External crates
pub use pill_hot_runtime::*;

// Current crate
use crate::system::{SystemParam, SystemParamFunction};

// =============================================================================
// Signature identity
// =============================================================================

/// Stable identity of a system's call signature.
///
/// Derived from the parameter tuple and return type names, which is exactly
/// what changes when a signature changes. Computed once per registration, so
/// its cost never reaches the frame loop.
///
/// Type names rather than [`TypeId`](std::any::TypeId) because a patch is a
/// separately compiled artifact: it must be able to compute the same value from
/// the same source text, and `type_name` is what both sides can agree on.
pub fn signature_hash<F, Input>() -> u64
where
    F: SystemParamFunction<Input>,
    Input: SystemParam,
{
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    std::any::type_name::<Input>().hash(&mut hasher);
    std::any::type_name::<F::Output>().hash(&mut hasher);
    hasher.finish()
}

/// Signature identity of a system, inferred from the function itself.
///
/// The turbofish form of [`signature_hash`] cannot infer a function-item type
/// from nothing, so this takes the function by reference instead. Convenient
/// wherever the function is in scope as a value.
pub fn signature_hash_of<F, Input>(_function: &F) -> u64
where
    F: SystemParamFunction<Input>,
    Input: SystemParam,
{
    signature_hash::<F, Input>()
}

/// The fully-qualified path of a system function, as `#[pill_hot]` names it.
///
/// A function-item type's name is its path - `project::physics_system` - which
/// is exactly what `module_path!() + "::" + fn name` produces in the attribute.
/// That agreement is what lets the host patch by the function's real path even
/// though [`Engine::register_system`](crate::Engine::register_system) also
/// records a separate, arbitrary display name (`"ball_physics"`).
///
/// Returns `None` for anything that is not a plain function item - a closure,
/// for instance, whose `type_name` is a compiler-generated placeholder with no
/// stable path a patch could name.
pub fn function_path<F>() -> Option<String> {
    let name = std::any::type_name::<F>();
    // Closures render as `crate::outer::{{closure}}`, and generic instantiations
    // carry angle brackets; neither is a name a patch can be keyed by.
    if name.contains('{') || name.contains('<') || !name.contains("::") {
        return None;
    }
    Some(name.to_string())
}

/// Address of the dispatch entry point for a locally compiled system function.
///
/// This is the value [`HotSlot::install`] expects. A real patch resolves the
/// equivalent address from a freshly compiled library's exports instead; this
/// helper covers the in-process case, which is what tests and any statically
/// linked replacement need.
pub fn local_implementation_address<F, Input>(_function: &F) -> usize
where
    F: SystemParamFunction<Input>,
    Input: SystemParam,
{
    <F as SystemParamFunction<Input>>::run as *const () as usize
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// `Input` must be inferable from the function value alone, with no
    /// turbofish. A concrete function-item type satisfies exactly one arity of
    /// [`SystemParamFunction`], so the solver has a unique choice - and the
    /// `#[pill_hot]` macro relies on this, because reconstructing the parameter
    /// tuple from syntax would have to strip patterns and guess lifetimes.
    #[test]
    fn input_infers_from_the_function_value_alone() {
        fn nullary() {}
        fn fallible() -> Result<(), crate::error::SystemError> {
            Ok(())
        }

        assert_ne!(local_implementation_address(&nullary), 0);
        assert_ne!(local_implementation_address(&fallible), 0);
        assert_ne!(
            signature_hash_of(&nullary),
            signature_hash_of(&fallible),
            "differing return types must produce differing signatures"
        );
    }

    #[test]
    fn slot_reports_its_baseline() {
        let slot = HotSlot::new();
        slot.initialize(0x1234, 0xABCD);
        assert_eq!(slot.current(), 0x1234);
        assert_eq!(slot.signature_hash(), 0xABCD);
    }

    /// A slot shared with a system's closure observes installs made through the
    /// registry - the property the whole design depends on.
    #[test]
    fn registry_and_closure_share_one_slot() {
        let mut registry = HotPatchRegistry::new();
        let slot = Arc::new(HotSlot::new());
        slot.initialize(0x1000, 0x7);
        registry.insert("movement", Arc::clone(&slot));

        registry
            .get("movement")
            .unwrap()
            .install("movement", 0x2000, 0x7)
            .expect("install");

        assert_eq!(slot.current(), 0x2000, "the closure's handle must see it");
    }

    // -------------------------------------------------------------------------
    // Registry
    // -------------------------------------------------------------------------
    //
    // `#[pill_hot]` cannot be used inside `pill_engine` itself: its generated
    // code refers to `::pill_engine`, which does not resolve in the defining
    // crate. These tests therefore submit a descriptor by hand, exactly as the
    // macro would, which is the same approach `component_registry` takes. The
    // macro's own expansion is covered downstream, where it can actually run.

    /// Stands in for a `#[pill_hot]` system.
    fn registry_probe_system() {}

    fn registry_probe_descriptor() -> (usize, u64) {
        (
            local_implementation_address(&registry_probe_system),
            signature_hash_of(&registry_probe_system),
        )
    }

    inventory::submit! {
        PillHotFunctionDescriptor {
            qualified_name: "pill_engine::hot_patch::tests::registry_probe_system",
            resolve: registry_probe_descriptor,
        }
    }

    #[test]
    fn registry_resolves_a_submitted_function() {
        let (address, hash) =
            resolve_hot_function("pill_engine::hot_patch::tests::registry_probe_system")
                .expect("the submitted descriptor must be discoverable");

        assert_ne!(address, 0, "a resolved address must be callable");
        assert_eq!(
            address,
            local_implementation_address(&registry_probe_system),
            "the registry must report the same dispatch address the engine registers"
        );
        assert_eq!(hash, signature_hash_of(&registry_probe_system));
    }

    #[test]
    fn registry_lists_its_functions() {
        assert!(
            hot_function_names().any(|name| name.ends_with("registry_probe_system")),
            "a submitted function must appear in the listing"
        );
    }

    /// Two systems with different signatures hash differently; the same
    /// signature hashes identically across calls.
    #[test]
    fn signature_hash_distinguishes_signatures() {
        fn takes_nothing() {}
        fn takes_nothing_too() {}

        let first = signature_hash::<fn(), ()>();
        let second = signature_hash::<fn(), ()>();
        assert_eq!(first, second, "same signature must hash stably");

        let _ = (takes_nothing, takes_nothing_too);
    }
}

// =============================================================================
// Integration tests
// =============================================================================

/// End-to-end checks that a system registered through the normal engine API can
/// have its implementation replaced, and that everything around it stays put.
#[cfg(all(test, feature = "hot_patch"))]
mod integration_tests {
    use super::*;
    use crate::error::SystemError;
    use crate::{Engine, SystemOwner};

    /// Declares an isolated observable counter plus the two system bodies that
    /// drive it.
    ///
    /// One set per test: the harness runs tests in parallel, so a single shared
    /// counter would see other tests' increments and make any assertion about
    /// how many times a system ran meaningless.
    macro_rules! isolated_systems {
        ($name:ident) => {
            // Each instantiation uses only the helpers its own test needs.
            #[allow(dead_code)]
            mod $name {
                use std::sync::atomic::{AtomicU32, Ordering};

                static OBSERVED: AtomicU32 = AtomicU32::new(0);

                pub fn original() {
                    OBSERVED.fetch_add(1, Ordering::SeqCst);
                }

                pub fn replacement() {
                    OBSERVED.fetch_add(10, Ordering::SeqCst);
                }

                pub fn observed() -> u32 {
                    OBSERVED.load(Ordering::SeqCst)
                }
            }
        };
    }

    isolated_systems!(replaces_behavior);
    isolated_systems!(registration_intact);
    isolated_systems!(signature_refused);
    isolated_systems!(unknown_refused);
    isolated_systems!(cleared_slots);
    isolated_systems!(second_patch);
    isolated_systems!(rolled_back);

    /// A different signature, used to prove the gate refuses it.
    fn differently_shaped_system() -> Result<(), SystemError> {
        Ok(())
    }

    fn sequential_engine() -> Engine {
        let mut engine = Engine::new();
        engine.set_parallel_execution(false);
        engine
    }

    /// The headline behavior: a patched system runs new code on the next frame.
    #[test]
    fn patch_replaces_behavior_on_the_next_frame() {
        use replaces_behavior as systems;
        let mut engine = sequential_engine();
        engine.register_system("counter", systems::original);

        engine.process_frame().expect("frame");
        assert_eq!(systems::observed(), 1, "baseline runs");

        engine
            .hot_patch(
                "counter",
                local_implementation_address::<_, ()>(&systems::replacement),
                signature_hash_of::<_, ()>(&systems::replacement),
            )
            .expect("patch accepted");

        engine.process_frame().expect("frame");
        assert_eq!(
            systems::observed(),
            11,
            "patched body must run, continuing from the previous value"
        );
    }

    /// The system's slot count and enabled state survive a patch: only the
    /// implementation moved.
    #[test]
    fn patch_leaves_the_registration_intact() {
        use registration_intact as systems;
        let mut engine = sequential_engine();
        engine.register_system("counter", systems::original);
        let slots_before = engine.hot_patch_registry().len();

        engine
            .hot_patch(
                "counter",
                local_implementation_address::<_, ()>(&systems::replacement),
                signature_hash_of::<_, ()>(&systems::replacement),
            )
            .expect("patch accepted");

        assert_eq!(
            engine.hot_patch_registry().len(),
            slots_before,
            "no system was added or removed"
        );
        assert_eq!(engine.is_system_enabled("counter"), Some(true));
    }

    /// A signature change must be refused, and the running code must survive it.
    #[test]
    fn patch_with_a_changed_signature_is_refused() {
        use signature_refused as systems;
        let mut engine = sequential_engine();
        engine.register_system("counter", systems::original);
        engine.process_frame().expect("frame");

        let error = engine
            .hot_patch(
                "counter",
                local_implementation_address::<_, ()>(&differently_shaped_system),
                signature_hash_of::<_, ()>(&differently_shaped_system),
            )
            .expect_err("a changed signature must be refused");
        assert!(matches!(error, HotPatchError::SignatureMismatch { .. }));

        engine.process_frame().expect("frame");
        assert_eq!(
            systems::observed(),
            2,
            "the original implementation must still be running"
        );
    }

    #[test]
    fn patching_an_unregistered_system_is_refused() {
        use unknown_refused as systems;
        let mut engine = sequential_engine();
        engine.register_system("counter", systems::original);

        let error = engine
            .hot_patch(
                "not_registered",
                local_implementation_address::<_, ()>(&systems::replacement),
                signature_hash_of::<_, ()>(&systems::replacement),
            )
            .expect_err("unknown system must be refused");
        assert!(matches!(error, HotPatchError::UnknownSystem { .. }));
    }

    /// Clearing a module's systems must also retire their slots, so a late
    /// patch cannot install into a system that no longer runs.
    #[test]
    fn cleared_systems_stop_being_patchable() {
        use cleared_slots as systems;
        let mut engine = sequential_engine();
        let owner = SystemOwner::optional_module(0);

        // Distinct functions, so each system gets its own function-path alias
        // and the two do not share a registry entry.
        engine.begin_module_registration(owner);
        engine.register_system("module_counter", systems::original);
        engine.end_module_registration();
        engine.register_system("project_counter", systems::replacement);

        assert!(engine.hot_patch_registry().get("module_counter").is_some());
        assert!(engine.hot_patch_registry().get("project_counter").is_some());

        engine.clear_systems_owned_by(owner);

        assert!(
            engine.hot_patch_registry().get("module_counter").is_none(),
            "a cleared system's slot must be forgotten"
        );
        assert!(
            engine.hot_patch_registry().get("project_counter").is_some(),
            "another owner's system must survive"
        );

        let error = engine
            .hot_patch(
                "module_counter",
                local_implementation_address::<_, ()>(&systems::replacement),
                signature_hash_of::<_, ()>(&systems::replacement),
            )
            .expect_err("a cleared system must not be patchable");
        assert!(matches!(error, HotPatchError::UnknownSystem { .. }));
    }

    /// A system is patchable by its function path as well as its display name.
    ///
    /// The two routinely differ - the example project registers
    /// `physics_system` as `"ball_physics"` - and a generated patch only knows
    /// the path, because that is what `#[pill_hot]` derives.
    #[test]
    fn a_system_is_patchable_by_its_function_path() {
        use registration_intact as systems;
        let mut engine = sequential_engine();
        engine.register_system("a_display_name", systems::original);

        let path = function_path::<fn()>();
        assert!(path.is_none(), "a bare fn pointer type has no path");

        // The path the engine recorded for this registration.
        let recorded: Vec<String> = engine
            .hot_patch_registry()
            .names()
            .map(|name| name.to_string())
            .collect();
        let function_path_entry = recorded
            .iter()
            .find(|name| name.ends_with("::original"))
            .expect("the function path must be registered alongside the display name");

        assert!(
            engine
                .hot_patch_registry()
                .get(function_path_entry)
                .is_some(),
            "both names must resolve to a slot"
        );

        // Clearing through one name must retire the other too.
        engine.clear_systems();
        assert!(engine.hot_patch_registry().is_empty());
    }

    /// A system can be returned to the code its artifact was built with, after
    /// any number of patches.
    ///
    /// This is generation zero of a rollback. It works because the slot records
    /// the registration address separately from the current one - without that,
    /// the first patch would make the original unreachable.
    #[test]
    fn a_system_can_be_rolled_back_to_its_baseline() {
        use rolled_back as systems;
        let mut engine = sequential_engine();
        engine.register_system("counter", systems::original);

        let (baseline, baseline_hash) = engine
            .hot_patch_baseline("counter")
            .expect("a registered system must record a baseline");

        // Three generations, as a live-coding session produces.
        for _ in 0..3 {
            engine
                .hot_patch(
                    "counter",
                    local_implementation_address::<_, ()>(&systems::replacement),
                    signature_hash_of::<_, ()>(&systems::replacement),
                )
                .expect("patch");
        }
        engine.process_frame().expect("frame");
        assert_eq!(systems::observed(), 10, "the patch must be running");

        // The baseline is still reachable and still installable.
        engine
            .hot_patch("counter", baseline, baseline_hash)
            .expect("rollback to the baseline must be accepted");
        engine.process_frame().expect("frame");
        assert_eq!(
            systems::observed(),
            11,
            "generation zero must run the original body"
        );
    }

    /// Patching twice runs the newest implementation, which is what a second
    /// edit in a live-coding session does.
    #[test]
    fn a_second_patch_supersedes_the_first() {
        use second_patch as systems;
        let mut engine = sequential_engine();
        engine.register_system("counter", systems::original);

        engine
            .hot_patch(
                "counter",
                local_implementation_address::<_, ()>(&systems::replacement),
                signature_hash_of::<_, ()>(&systems::replacement),
            )
            .expect("first patch");
        engine.process_frame().expect("frame");
        assert_eq!(systems::observed(), 10);

        engine
            .hot_patch(
                "counter",
                local_implementation_address::<_, ()>(&systems::original),
                signature_hash_of::<_, ()>(&systems::original),
            )
            .expect("second patch, back to the original");
        engine.process_frame().expect("frame");
        assert_eq!(
            systems::observed(),
            11,
            "the newest implementation must win"
        );
    }
}
