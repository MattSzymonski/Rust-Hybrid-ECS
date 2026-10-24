//! Per-DLL hot-patch registries: `#[pill_hot]` functions, `#[pill_hot_fn]`
//! slots, and the build-script address inventory.
//!
//! # Responsibilities
//!
//! - Re-export the engine core's hot-patch machinery
//!   (`pill_engine_core::hot_patch`) under this path.
//! - Declare the registries the macros and the build-script inventory submit
//!   into ([`PillHotSlotDescriptor`],
//!   [`PillFunctionAddress`](crate::hot_patch::PillFunctionAddress),
//!   [`PillHotFunctionDescriptor`]) and every function that reads them.
//!
//! # Design
//!
//! These registries answer questions about one DLL ("the function's address
//! in THIS artifact"), so they live in this crate, which every DLL embeds,
//! and not in the engine core. Each DLL's copy of these functions walks that
//! DLL's own lists.

/// Everything that is the same for every DLL: slots, the patch registry,
/// signature hashing, prologue patching.
pub use pill_engine_core::hot_patch::*;

// =============================================================================
// Registry of `#[pill_hot_fn]` slots
// =============================================================================

/// One `#[pill_hot_fn]` declared in this artifact.
pub struct PillHotSlotDescriptor {
    /// Fully-qualified path, as `module_path!() + "::" + fn name`.
    pub qualified_name: &'static str,
    /// The dispatcher's slot, so a host can redirect it.
    pub slot: &'static PlainSlot,
    /// The signature as written, used as the compatibility gate.
    ///
    /// Text rather than a `TypeId`, because a patch is a separately compiled
    /// artifact and both sides must derive the same value from the same source.
    pub signature: &'static str,
    /// Address of the body itself, reached through a fn pointer because casting
    /// a function to `usize` is not permitted in the constant context that
    /// builds this struct.
    ///
    /// The body and not the dispatcher: a patch hands this address to every
    /// artifact holding a copy of the function, and naming the dispatcher would
    /// make each call take an extra hop through a slot that is never installed.
    ///
    /// `None` for an inherent method declared in a running artifact. A method's
    /// body cannot be hoisted into a separately addressable function, because
    /// every item inside a method body is barred from naming `Self`
    /// (`error[E0401]`) - so the body stays inline in the dispatcher and has no
    /// symbol of its own. Only a patch needs an address, and a patch names the
    /// receiver type concretely, so it always supplies one.
    pub implementation_address: Option<fn() -> usize>,
}

inventory::collect!(PillHotSlotDescriptor);

/// Redirect a `#[pill_hot_fn]` declared in THIS artifact.
///
/// # Errors
///
/// Returns [`HotPatchError::UnknownSystem`] when this artifact declares no such
/// function, [`HotPatchError::NullAddress`] for a null replacement, and
/// [`HotPatchError::SignatureMismatch`] when the signature text differs - which
/// is what stops a reshaped function being installed behind call sites compiled
/// for the old shape.
pub fn install_plain_function(
    qualified_name: &str,
    address: usize,
    signature: &str,
) -> Result<(), HotPatchError> {
    let descriptor = inventory::iter::<PillHotSlotDescriptor>
        .into_iter()
        .find(|descriptor| descriptor.qualified_name == qualified_name)
        .ok_or_else(|| HotPatchError::UnknownSystem {
            name: qualified_name.to_string(),
        })?;

    if address == 0 {
        return Err(HotPatchError::NullAddress {
            name: qualified_name.to_string(),
        });
    }
    if descriptor.signature != signature {
        return Err(HotPatchError::SignatureMismatch {
            name: qualified_name.to_string(),
            expected: text_hash(descriptor.signature),
            found: text_hash(signature),
        });
    }
    descriptor.slot.install(address);
    Ok(())
}

/// Return one `#[pill_hot_fn]` to the body compiled into this artifact.
///
/// # Errors
///
/// Returns [`HotPatchError::UnknownSystem`] when this artifact declares no such
/// function.
pub fn reset_plain_function(qualified_name: &str) -> Result<(), HotPatchError> {
    let descriptor = inventory::iter::<PillHotSlotDescriptor>
        .into_iter()
        .find(|descriptor| descriptor.qualified_name == qualified_name)
        .ok_or_else(|| HotPatchError::UnknownSystem {
            name: qualified_name.to_string(),
        })?;
    descriptor.slot.reset();
    Ok(())
}

/// Names of every `#[pill_hot_fn]` this artifact declares.
pub fn plain_function_names() -> impl Iterator<Item = &'static str> {
    inventory::iter::<PillHotSlotDescriptor>
        .into_iter()
        .map(|descriptor| descriptor.qualified_name)
}

/// The signature text this artifact recorded for a `#[pill_hot_fn]`.
///
/// Callers pass this straight back to [`install_plain_function`] rather than
/// reconstructing it: the exact spelling comes from `stringify!` inside the
/// macro, so writing it by hand is guesswork that fails the gate on a stray
/// space. A patch artifact derives its own copy from the same source through
/// the same macro, which is what makes the two comparable.
pub fn plain_function_signature(qualified_name: &str) -> Option<&'static str> {
    inventory::iter::<PillHotSlotDescriptor>
        .into_iter()
        .find(|descriptor| descriptor.qualified_name == qualified_name)
        .map(|descriptor| descriptor.signature)
}

/// Address and signature of a `#[pill_hot_fn]` this artifact declares.
///
/// This is the pair a host needs in order to install this artifact's
/// implementation into another artifact's copy of the same function: the
/// address to jump to, and the signature text that copy compares against its
/// own before accepting it.
pub fn plain_function_entry(qualified_name: &str) -> Option<(usize, &'static str)> {
    let descriptor = inventory::iter::<PillHotSlotDescriptor>
        .into_iter()
        .find(|descriptor| descriptor.qualified_name == qualified_name)?;
    // `None` means this artifact declares the function but cannot address its
    // body - true of every inherent method outside a patch. Reporting nothing
    // is correct: the caller is asking a patch where its replacement lives.
    let address = descriptor.implementation_address?;
    Some((address(), descriptor.signature))
}

// =============================================================================
// Macro-free function inventory (SPIKE)
// =============================================================================

/// One function this artifact can report the address of, contributed by a
/// crate's build script rather than by an attribute.
///
/// This is the macro-free half of the Live++ style approach: a build script
/// scans the crate's own sources, finds every function, and emits one of these
/// per function. Nothing in the source is annotated, and because `inventory`
/// collects per artifact, a DLL ends up with an entry for every function in
/// every crate linked into it - which is exactly the fan-out a multi-artifact
/// engine needs.
pub struct PillFunctionAddress {
    /// Fully-qualified path, as `module_path!() + "::" + fn name`.
    pub qualified_name: &'static str,
    /// The function's address in THIS artifact.
    ///
    /// A fn pointer rather than a `usize` because casting a function to an
    /// integer is not permitted in the constant context that builds this.
    pub address: fn() -> usize,
    /// The declaration as written, with whitespace collapsed.
    ///
    /// The compatibility gate for the prologue route, which has no other one:
    /// overwriting a function's first bytes cannot check anything about the
    /// replacement, so the check has to happen before the write. A host compares
    /// this against the signature it read from the edited source and refuses
    /// when they differ.
    pub signature: &'static str,
}

inventory::collect!(PillFunctionAddress);

/// Address of one function inside this artifact, by qualified path.
pub fn function_address(qualified_name: &str) -> Option<usize> {
    inventory::iter::<PillFunctionAddress>
        .into_iter()
        .find(|entry| entry.qualified_name == qualified_name)
        .map(|entry| (entry.address)())
}

/// The declaration this artifact was built with, by qualified path.
pub fn function_signature(qualified_name: &str) -> Option<&'static str> {
    inventory::iter::<PillFunctionAddress>
        .into_iter()
        .find(|entry| entry.qualified_name == qualified_name)
        .map(|entry| entry.signature)
}

/// Every function this artifact can report, for diagnostics.
pub fn function_addresses() -> impl Iterator<Item = (&'static str, usize)> {
    inventory::iter::<PillFunctionAddress>
        .into_iter()
        .map(|entry| (entry.qualified_name, (entry.address)()))
}

/// How many of this artifact's registered functions have a known extent.
///
/// Diagnostic: a prologue patch can only overwrite a function whose length the
/// exception directory records, so this says how much of an artifact is
/// reachable by that route at all.
pub fn functions_with_known_extent() -> (usize, usize) {
    let mut known = 0usize;
    let mut total = 0usize;
    for (_, address) in function_addresses() {
        total += 1;
        if function_extent(address).is_some() {
            known += 1;
        }
    }
    (known, total)
}

/// Stable hash of a signature string, used only to report a mismatch.
fn text_hash(text: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    hasher.finish()
}

// =============================================================================
// Compile-time registry of hot-patchable functions
// =============================================================================

/// One function declared with `#[pill_hot]`.
///
/// Submitted into this artifact's registry by the attribute macro. Every field
/// is const-constructible so the descriptor can live in a static, matching how
/// [`PillComponentDescriptor`](crate::component_registry::PillComponentDescriptor)
/// works.
///
/// The registry is **per linked artifact**: the host executable, each extension
///  DLL and the project DLL each carry exactly the descriptors their own
/// sources declared. That is what makes it correct across a reload - a
/// generation that stops declaring a function simply stops submitting it, and
/// an evicted DLL takes its descriptors with it.
pub struct PillHotFunctionDescriptor {
    /// Fully-qualified path, as `module_path!() + "::" + fn name`.
    pub qualified_name: &'static str,
    /// Resolves this function's dispatch address and signature identity.
    ///
    /// A function rather than two constants because both values require the
    /// generic machinery in [`local_implementation_address`] and
    /// [`signature_hash_of`], which cannot run in a `const`.
    pub resolve: fn() -> (usize, u64),
}

inventory::collect!(PillHotFunctionDescriptor);

/// Look up a hot-patchable function declared in THIS artifact.
///
/// Returns its dispatch address and signature hash, or `None` when no
/// `#[pill_hot]` function carries that qualified name. A host calls the
/// equivalent through the artifact's exported resolver rather than directly.
pub fn resolve_hot_function(qualified_name: &str) -> Option<(usize, u64)> {
    inventory::iter::<PillHotFunctionDescriptor>
        .into_iter()
        .find(|descriptor| descriptor.qualified_name == qualified_name)
        .map(|descriptor| (descriptor.resolve)())
}

/// Every hot-patchable function this artifact declares, for diagnostics.
pub fn hot_function_names() -> impl Iterator<Item = &'static str> {
    inventory::iter::<PillHotFunctionDescriptor>
        .into_iter()
        .map(|descriptor| descriptor.qualified_name)
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

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
    fn registry_reports_nothing_for_an_unknown_name() {
        assert!(resolve_hot_function("nothing::declares::this").is_none());
    }

    #[test]
    fn registry_lists_its_functions() {
        assert!(
            hot_function_names().any(|name| name.ends_with("registry_probe_system")),
            "a submitted function must appear in the listing"
        );
    }
}
