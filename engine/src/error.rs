//! Typed error system for the ECS engine, generated from one semantic
//! message definition per variant.
//!
//! # Responsibilities
//!
//! - Declare the engine's subsystem errors ([`WorldError`],
//!   [`CommandError`], [`AddComponentError`], [`RemoveComponentError`],
//!   [`BuildError`], [`RendererError`]).
//! - Compose them transparently into the top-level [`EngineError`] together
//!   with the shared host errors from `pill_core`.
//!
//! # Design
//!
//! Every variant carries a single `#[message(...)]` definition rendered
//! through the `pill_core` diagnostics runtime. Error enums never import a
//! styling crate, never convert sources to text, and compose with `?`
//! through transparent `#[from]` wrappers. Frontends receive the composed
//! [`EngineError`] at their boundary and report it exactly once.

// External crates
use pill_core_macros::engine_error;

// Current crate
use crate::{ComponentId, Entity};

// =============================================================================
// World Errors
// =============================================================================

/// Storage and migration failures of the archetype world.
#[engine_error(namespace = engine::world, runtime = ::pill_core::error)]
pub enum WorldError {
    /// The stable ID of a dynamic component cannot be zero.
    #[message("dynamic component stable ID cannot be zero")]
    DynamicStableIdZero,

    /// The size of a dynamic component cannot be zero.
    #[message("dynamic component size cannot be zero")]
    DynamicSizeZero,

    /// The alignment of a dynamic component must be a non-zero power of two.
    #[message("dynamic component alignment must be a non-zero power of two")]
    DynamicAlignmentInvalid,

    /// The size and alignment pair does not form a valid memory layout.
    #[message("dynamic component size and alignment do not form a valid layout")]
    DynamicLayoutInvalid,

    /// The stable ID is already registered with a different name or schema.
    #[message("dynamic component stable ID is already registered with another name or schema")]
    DynamicAlreadyRegistered,

    /// The world's component type limit has been reached.
    #[message("component type limit exceeded (max 128)")]
    ComponentTypeLimitExceeded,

    /// A dynamic entity must carry at least one component.
    #[message("a dynamic entity must contain at least one component")]
    DynamicEntityEmpty,

    /// A dynamic entity cannot contain the same component twice.
    #[message("a dynamic entity cannot contain duplicate components")]
    DynamicDuplicateComponent,

    /// The component ID was never registered as dynamic storage.
    #[message("dynamic component ", debug_value(id), " is not registered")]
    DynamicComponentNotRegistered { id: ComponentId },

    /// The supplied bytes do not match the component's registered layout.
    #[message(
        "dynamic component ",
        debug_value(id),
        " byte length does not match its manifest"
    )]
    DynamicByteLengthMismatch { id: ComponentId },

    /// The entity does not exist in the world.
    #[message("entity not found")]
    EntityNotFound,

    /// The entity already carries the dynamic component being added.
    #[message("entity already contains the dynamic component")]
    DynamicComponentAlreadyPresent,

    /// The entity does not carry the dynamic component being removed or set.
    #[message("entity does not contain the dynamic component")]
    DynamicComponentMissing,

    /// A byte copy was rejected by the dynamic storage column.
    #[message("dynamic component row or byte length is invalid")]
    DynamicRowInvalid,

    /// A byte copy length does not match the registered element size.
    #[message("dynamic component byte length does not match its registered size")]
    DynamicSizeMismatch,
}

// =============================================================================
// Command Errors
// =============================================================================

/// Error returned when a deferred command cannot be executed.
///
/// Command errors are non-fatal by default — the engine logs them and
/// continues. Set `Engine::should_exit_on_error` to `true` for strict mode
/// where any command failure stops the frame immediately.
#[engine_error(namespace = engine::commands, runtime = ::pill_core::error)]
pub enum CommandError {
    /// The target entity no longer exists in the world.
    #[message("entity ", debug_value(entity), " not found for ", value(operation))]
    EntityNotFound {
        entity: Entity,
        operation: &'static str,
    },

    /// The entity already possesses the component being added.
    #[message(
        "entity ",
        debug_value(entity),
        " already has component ",
        debug_value(component_id)
    )]
    ComponentAlreadyExists {
        entity: Entity,
        component_id: ComponentId,
    },

    /// The entity does not have the component being removed.
    #[message(
        "entity ",
        debug_value(entity),
        " does not have component ",
        debug_value(component_id)
    )]
    ComponentNotFound {
        entity: Entity,
        component_id: ComponentId,
    },
}

// =============================================================================
// Component Add/Remove/Builder Errors
// =============================================================================

/// Error type for `add_component` operations.
#[engine_error(namespace = engine::world, runtime = ::pill_core::error)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum AddComponentError {
    /// The entity does not exist (was destroyed or never created).
    #[message("entity not found")]
    EntityNotFound,

    /// The entity already has a component of this type.
    #[message("component already exists on entity")]
    ComponentAlreadyExists,
}

/// Error type for `remove_component` operations.
#[engine_error(namespace = engine::world, runtime = ::pill_core::error)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum RemoveComponentError {
    /// The entity does not exist (was destroyed or never created).
    #[message("entity not found")]
    EntityNotFound,

    /// The entity does not have a component of this type.
    #[message("component not found on entity")]
    ComponentNotFound,
}

/// Error type for `EntityBuilder::build` when a component was not registered.
#[engine_error(namespace = engine::world, runtime = ::pill_core::error)]
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum BuildError {
    /// One or more component types were not registered with the world.
    /// Call `world.register_component::<T>()` for each type first.
    #[message(
        "component ",
        debug_value(id),
        " not registered — call world.register_component::<T>() first"
    )]
    ComponentNotRegistered { id: ComponentId },
}

// =============================================================================
// Persistence Errors
// =============================================================================

/// Migration failures of persistable component columns.
#[engine_error(namespace = engine::persistence, runtime = ::pill_core::error)]
pub enum PersistenceError {
    /// The component type is not registered in the current world.
    #[message(
        "component type ",
        name_style(type_name),
        " is not registered in the current world"
    )]
    ComponentTypeUnregistered { type_name: String },

    /// No deserializer is registered for the component type.
    #[message(
        "no deserializer is registered for component type ",
        name_style(type_name)
    )]
    DeserializerMissing { type_name: String },

    /// No inserter is registered for the component type.
    #[message("no inserter is registered for component type ", name_style(type_name))]
    InserterMissing { type_name: String },

    /// The persisted bytes could not be decoded into the new schema.
    #[message("deserialization failed for component ", debug_value(component_id))]
    DeserializationFailed { component_id: ComponentId },

    /// The old component column could not be removed from its archetype.
    #[message(
        "removing the old storage of component ",
        debug_value(component_id),
        " failed"
    )]
    StorageRemovalFailed { component_id: ComponentId },

    /// No storage factory is registered for the component.
    #[message(
        "no storage factory is registered for component ",
        debug_value(component_id)
    )]
    StorageFactoryMissing { component_id: ComponentId },

    /// The component has no native Rust storage to migrate.
    #[message(
        "component ",
        debug_value(component_id),
        " has no native Rust storage to migrate"
    )]
    NativeStorageExpected { component_id: ComponentId },

    /// No component copier is registered for the component.
    #[message(
        "no component copier is registered for component ",
        debug_value(component_id)
    )]
    CopierMissing { component_id: ComponentId },

    /// The destination archetype vanished immediately after creation.
    #[message("destination archetype missing after creation")]
    DestinationArchetypeMissing,
}

// =============================================================================
// Renderer Errors
// =============================================================================

/// Rendering initialization or presentation failures of the wgpu backend.
#[cfg(feature = "rendering")]
#[engine_error(namespace = engine::renderer, runtime = ::pill_core::error)]
pub enum RendererError {
    /// The GPU surface could not be created for the supplied window.
    #[message("failed to create the GPU surface")]
    SurfaceCreation {
        #[source]
        source: wgpu::CreateSurfaceError,
    },

    /// No compatible GPU adapter could be found.
    #[message("failed to find a compatible GPU adapter")]
    AdapterRequest {
        #[source]
        source: wgpu::RequestAdapterError,
    },

    /// The GPU device could not be created from the adapter.
    #[message("failed to create the GPU device")]
    DeviceCreation {
        #[source]
        source: wgpu::RequestDeviceError,
    },

    /// The surface exposes no texture formats.
    #[message("GPU surface exposes no texture formats")]
    NoTextureFormats,

    /// The surface exposes no alpha modes.
    #[message("GPU surface exposes no alpha modes")]
    NoAlphaModes,

    /// The frame texture could not be acquired for a fatal reason.
    #[message("failed to acquire the GPU surface texture")]
    SurfaceTextureFailed {
        #[source]
        source: wgpu::SurfaceError,
    },
}

// =============================================================================
// Engine Error Composition
// =============================================================================

/// Transparent composition of every engine subsystem error plus the shared
/// host errors from `pill_core`.
///
/// Subsystem functions return their narrowest meaningful error;
/// [`EngineError`] exists as the composition boundary where `?` crosses
/// subsystems and frontends.
#[engine_error(namespace = engine, runtime = ::pill_core::error)]
pub enum EngineError {
    /// A world storage or migration operation failed.
    #[transparent]
    World(#[from] WorldError),

    /// A deferred command could not be applied.
    #[transparent]
    Commands(#[from] CommandError),

    /// A persisted component migration failed.
    #[transparent]
    Persistence(#[from] PersistenceError),

    /// The renderer failed to initialize or draw a frame.
    #[cfg(feature = "rendering")]
    #[transparent]
    Renderer(#[from] RendererError),

    /// The windowed frontend failed to create its event loop or window.
    #[cfg(feature = "rendering")]
    #[transparent]
    Frontend(#[from] ::pill_core::error::FrontendError),

    /// A subsystem of the shared host crate failed.
    #[transparent]
    Host(#[from] ::pill_core::error::HostError),
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use miette::Diagnostic as _;
    use pill_core::error::EngineMessage as _;

    /// Plain rendering preserves the semantic values of a world error.
    #[test]
    fn world_error_renders_plain_with_values() {
        let error = WorldError::DynamicComponentNotRegistered {
            id: ComponentId::dynamic(7),
        };
        assert!(error.to_plain_message().contains("dynamic component"));
        assert!(error.to_plain_message().contains("is not registered"));
    }

    /// Command errors derive their diagnostic code from namespace and variant.
    #[test]
    fn command_error_code_derives_from_namespace_and_variant() {
        let error = CommandError::EntityNotFound {
            entity: Entity::new_for_test(3, 1),
            operation: "destroy",
        };
        assert_eq!(
            error.code().map(|code| code.to_string()).as_deref(),
            Some("engine::commands::entity_not_found")
        );
    }

    /// Transparent composition preserves the leaf message end to end.
    #[test]
    fn engine_error_composition_keeps_the_leaf_message() {
        let world_error = WorldError::EntityNotFound;
        let engine_error: EngineError = world_error.into();
        assert_eq!(engine_error.to_string(), "entity not found");
        assert_eq!(engine_error.to_plain_message(), "entity not found");
    }

    /// The entity builder error keeps its Copy semantics across composition.
    #[test]
    fn build_error_remains_copy() {
        let error = BuildError::ComponentNotRegistered {
            id: ComponentId::dynamic(9),
        };
        let copied = error;
        assert_eq!(error, copied);
    }

    /// Renderer unit variants produce their diagnostic code.
    #[cfg(feature = "rendering")]
    #[test]
    fn renderer_error_code_derives_from_namespace_and_variant() {
        assert_eq!(
            RendererError::NoTextureFormats
                .code()
                .map(|code| code.to_string())
                .as_deref(),
            Some("engine::renderer::no_texture_formats")
        );
    }
}
