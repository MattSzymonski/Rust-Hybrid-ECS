//! The systems, resources and assets an artifact declares, as opposed to the
//! ones it registers by hand.
//!
//! # Responsibilities
//!
//! - Declare the descriptor types the `#[system]` attribute and the
//!   `#[derive(PillResource)]`/`#[derive(PillAsset)]` derives submit into
//!   [`inventory`], and the collections themselves.
//! - Turn those collections into registered systems, declared resources and
//!   declared asset types at the artifact's entry point, in a deterministic
//!   order.
//!
//! # Design
//!
//! This is the shape [`component_registry`](crate::component_registry) gives
//! components, applied to systems, resources and assets. A macro submits one
//! descriptor per declaration into the artifact's own collection, and the
//! entry point walks each collection once per generation. Because the
//! collections belong to the linked artifact, a reloaded image declares only
//! what its own sources declare, and an evicted image takes its descriptors
//! with it.
//!
//! Registration order decides the order conflicting systems run in, and
//! `inventory` order is link order rather than source order. The system
//! descriptor therefore carries an explicit order key and a name to break
//! ties, so the schedule a project gets is the schedule its declarations
//! describe. Resources and asset types have no execution order; they are
//! declared sorted by type name so the claim order is the same in every build.

// Current crate
use crate::{Engine, World};

// =============================================================================
// Types
// =============================================================================

/// One ECS system declared with `#[system]`.
pub struct PillSystemDescriptor {
    /// Name the system is registered under, which is what logs, the editor's
    /// Systems tab and a hot patch all use to find it.
    pub name: &'static str,
    /// Sort key; a lower value registers first. Equal values are ordered by
    /// [`Self::name`].
    pub order: i32,
    /// Whether the system runs in the post-update stage, after the command
    /// flush.
    pub post_update: bool,
    /// Registers the system with the engine.
    ///
    /// A function pointer rather than the system itself: the attribute emits
    /// the call, so the system's parameter tuple is inferred at its own
    /// declaration site, exactly as a hand-written `register_system` call
    /// would do.
    pub register: fn(&mut Engine),
}

inventory::collect!(PillSystemDescriptor);

/// One ECS resource declared with `#[derive(PillResource)]`.
pub struct PillResourceDescriptor {
    /// Fully-qualified type path: the declaring module's path plus the type's
    /// name.
    pub type_name: &'static str,
    /// Declares the resource with a world.
    ///
    /// A function pointer rather than the type itself: the derive emits the
    /// call, so whether the type is a plain resource or a manager resource is
    /// decided where the type is declared.
    pub register: fn(&mut World),
}

inventory::collect!(PillResourceDescriptor);

/// One asset type declared with `#[derive(PillAsset)]`.
pub struct PillAssetDescriptor {
    /// Fully-qualified type path: the declaring module's path plus the type's
    /// name.
    pub type_name: &'static str,
    /// Declares the asset type with a world.
    ///
    /// A function pointer rather than the type itself: the derive emits the
    /// call, so whether the type imports from a source file or is its own
    /// document file is decided where the type is declared.
    pub register: fn(&mut World),
}

inventory::collect!(PillAssetDescriptor);

// =============================================================================
// Free Functions
// =============================================================================

/// Registers every system this artifact declares.
///
/// Called by the artifact entry points before the artifact's own `register`
/// runs, so the same declarations reach a first load, every reload and a
/// statically linked build.
///
/// Systems are registered in `(order, name)` order. The engine appends each
/// one to its list and the scheduler batches by that order, so this is what
/// keeps the execution order of conflicting systems stable across builds
/// instead of following the linker.
pub fn register_all_systems(engine: &mut Engine) {
    // Step 1: Collect this artifact's declarations and sort them into the
    // order the schedule should run them in.
    let mut descriptors: Vec<&'static PillSystemDescriptor> =
        inventory::iter::<PillSystemDescriptor>().collect();
    descriptors.sort_by(|left, right| {
        left.order
            .cmp(&right.order)
            .then_with(|| left.name.cmp(right.name))
    });
    // Step 2: Register each one through the function the attribute emitted.
    for descriptor in descriptors {
        (descriptor.register)(engine);
    }
}

/// Declares every resource this artifact declares.
///
/// Called by the artifact entry points after [`register_all_systems`], so a
/// first load, every reload and a statically linked build all declare the same
/// types before the artifact's own `register` runs. That ordering is what lets
/// `insert_resource` in `register` find the declaration already in place: an
/// inserted value replaces a stored one, it does not declare the type.
///
/// Declarations are sorted by type name. Unlike systems they have no execution
/// order, and the sort keeps the claim order - and so the order any claim error
/// is recorded in - the same in every build.
pub fn register_all_resources(world: &mut World) {
    // Step 1: Collect this artifact's declarations in a stable order.
    let mut descriptors: Vec<&'static PillResourceDescriptor> =
        inventory::iter::<PillResourceDescriptor>().collect();
    descriptors.sort_by_key(|descriptor| descriptor.type_name);
    // Step 2: Declare each one through the function the derive emitted.
    for descriptor in descriptors {
        (descriptor.register)(world);
    }
}

/// Declares every asset type this artifact declares.
///
/// Called by the artifact entry points after [`register_all_resources`]. An
/// imported asset's declaration also records the type's import entry points,
/// which live in this image, so the declaration is what keeps the world's
/// import registry pointing at code that is still mapped.
///
pub fn register_all_assets(world: &mut World) {
    // Step 1: Collect this artifact's declarations in a stable order.
    let mut descriptors: Vec<&'static PillAssetDescriptor> =
        inventory::iter::<PillAssetDescriptor>().collect();
    descriptors.sort_by_key(|descriptor| descriptor.type_name);
    // Step 2: Declare each one through the function the derive emitted.
    for descriptor in descriptors {
        (descriptor.register)(world);
    }
}
