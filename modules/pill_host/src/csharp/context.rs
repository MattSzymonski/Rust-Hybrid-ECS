//! Thread-local access scope installed around one scheduled C# system.
//!
//! # Responsibilities
//!
//! - Publish the active world, queue, and bindings for managed callbacks.
//! - Reject nested or out-of-scope managed invocation.
//! - Clear the complete thread-local scope when the invocation ends.
//!
//! # Design
//!
//! All invocation state is bundled into one [`ActiveScopeData`] value held in
//! a single thread-local [`Cell`]. Installation and teardown are each one
//! assignment, which makes the scope panic-atomic by construction: a panic
//! can never leave a half-installed set of raw pointers behind.

// Standard library
use std::cell::{Cell, RefCell};
use std::collections::HashSet;

// External crates
use pill_core::error;
use pill_engine::archetype::ArchetypeId;
use pill_engine::commands::CommandQueue;
use pill_engine::{Entity, World};

// Current crate
use super::abi::NativeSystemAccess;
use super::components::{ComponentBindings, StableComponentId};

// =============================================================================
// Constants
// =============================================================================

thread_local! {
    /// Complete managed invocation scope for the current thread.
    ///
    /// `None` means no C# system is executing on this thread.
    static ACTIVE_SCOPE: Cell<Option<ActiveScopeData>> = const { Cell::new(None) };
    /// Handles reserved during this invocation and not yet consumed by create.
    static ACTIVE_RESERVED: RefCell<HashSet<Entity>> = RefCell::new(HashSet::new());
    /// Archetype ids served to managed code during this invocation.
    ///
    /// The entity path (`mode == 2` of the archetype chunk callback) answers
    /// only for these: a managed term reaches an archetype through validated
    /// component or entity access first, and without that precondition the
    /// entity path would enumerate any component set the world holds.
    static OBSERVED_ARCHETYPES: RefCell<HashSet<ArchetypeId>> = RefCell::new(HashSet::new());
}

// =============================================================================
// Types + Impls
// =============================================================================

/// One complete managed invocation scope.
///
/// Bundling every pointer into a single value keeps installation and teardown
/// to one [`Cell::set`] call each, so no panic can ever strand a partially
/// installed scope on this thread.
#[derive(Clone, Copy)]
struct ActiveScopeData {
    world: *mut World,
    queue: *mut CommandQueue,
    access: (*const NativeSystemAccess, usize),
    bindings: *const ComponentBindings,
    uses_commands: bool,
}

/// Guards the thread-local invocation scope for exactly one scheduled managed
/// system.
///
/// Created by the scope installers and dropped at the end of the managed
/// invocation, this guard clears every raw pointer from the thread-local scope
/// even when managed execution unwinds through a panic.
pub(super) struct ActiveSystemGuard;

impl ActiveSystemGuard {
    /// Installs a query-only invocation scope used by native bridge tests.
    ///
    /// Production systems use [`Self::set_with_commands`] because the scheduler
    /// always supplies a queue even when the managed signature omits Commands.
    #[cfg(test)]
    pub(super) fn set(
        world: &mut World,
        access: &[NativeSystemAccess],
        bindings: &ComponentBindings,
    ) -> Option<Self> {
        Self::set_inner(world, std::ptr::null_mut(), access, bindings, false)
    }

    /// Publishes one scheduled system's world, command queue, bindings, and
    /// reflected access declaration for synchronous managed callbacks.
    pub(super) fn set_with_commands(
        world: &mut World,
        queue: &mut CommandQueue,
        access: &[NativeSystemAccess],
        bindings: &ComponentBindings,
        uses_commands: bool,
    ) -> Option<Self> {
        Self::set_inner(world, queue, access, bindings, uses_commands)
    }

    /// Installs the complete scope in one assignment after rejecting nested
    /// invocation, which would overwrite active raw pointers.
    ///
    /// Returns `None` when a scope is already installed on this thread. That is
    /// a managed-side programming error - a C# system re-entering the host -
    /// and it used to be an `assert!`. Panicking here unwinds through the .NET
    /// frames that called in, which is undefined behaviour at the FFI boundary,
    /// so the condition is reported instead of raised.
    ///
    /// The nested-invocation check is the only fallible step and it runs before
    /// any mutation, so a rejection leaves no stale state behind.
    fn set_inner(
        world: &mut World,
        queue: *mut CommandQueue,
        access: &[NativeSystemAccess],
        bindings: &ComponentBindings,
        uses_commands: bool,
    ) -> Option<Self> {
        // Step 1: Reject nested invocation before touching any thread-local.
        let already_active = ACTIVE_SCOPE.with(|slot| slot.get().is_some());
        if already_active {
            error!(
                target: pill_core::telemetry::telemetry_target::ECS,
                "nested managed ECS system invocation rejected; a managed system                  re-entered the host while one was already running"
            );
            return None;
        }

        // Step 2: Clear reservations from the previous invocation without
        // panicking. A reservation borrow can only exist while a scope is
        // active, which Step 1 has already rejected, so this is belt-and-braces.
        ACTIVE_RESERVED.with(|slot| {
            if let Ok(mut reserved) = slot.try_borrow_mut() {
                reserved.clear();
            }
        });
        // Every invocation starts with no archetype observed: the entity path
        // answers for what THIS system reached through a validated term.
        OBSERVED_ARCHETYPES.with(|slot| {
            if let Ok(mut observed) = slot.try_borrow_mut() {
                observed.clear();
            }
        });

        // Step 3: Commit the whole scope in one assignment. Cell::set cannot
        // panic, so from this point on the guard's Drop owns the teardown.
        ACTIVE_SCOPE.with(|slot| {
            slot.set(Some(ActiveScopeData {
                world: world as *mut World,
                queue,
                access: (access.as_ptr(), access.len()),
                bindings: bindings as *const ComponentBindings,
                uses_commands,
            }));
        });
        Some(Self)
    }
}

impl Drop for ActiveSystemGuard {
    /// Returns unconsumed reservations to the entity allocator and clears the
    /// complete scope before the scheduler's native borrows expire.
    fn drop(&mut self) {
        ACTIVE_SCOPE.with(|scope_slot| {
            let world_pointer = scope_slot.get().map(|scope| scope.world);
            ACTIVE_RESERVED.with(|reserved_slot| {
                if let Ok(mut reserved) = reserved_slot.try_borrow_mut() {
                    // Reserved handles never consumed by a create are returned
                    // to the allocator, so speculative managed allocations
                    // cannot leak entity generation slots.
                    if let Some(world) = world_pointer.filter(|pointer| !pointer.is_null()) {
                        // SAFETY: the scope is still installed for the
                        // duration of this drop, so the world pointer remains
                        // valid until the scope is cleared below.
                        let world = unsafe { &mut *world };
                        for entity in reserved.drain() {
                            world.release_entity(entity);
                        }
                    }
                }
            });
            OBSERVED_ARCHETYPES.with(|observed_slot| {
                if let Ok(mut observed) = observed_slot.try_borrow_mut() {
                    observed.clear();
                }
            });
            scope_slot.set(None);
        });
    }
}

// =============================================================================
// Free Functions
// =============================================================================

/// Runs a callback with the active world, deferred queue, component bindings,
/// and reservation set only when the system declared a Commands parameter.
pub(super) fn with_active_command_context<R>(
    f: impl FnOnce(&mut World, &mut CommandQueue, &ComponentBindings, &mut HashSet<Entity>) -> R,
) -> Option<R> {
    ACTIVE_SCOPE.with(|scope_slot| {
        let scope = scope_slot.get()?;
        if !scope.uses_commands {
            return None;
        }
        let world = scope.world;
        let queue = scope.queue;
        let bindings = scope.bindings;
        if world.is_null() || queue.is_null() || bindings.is_null() {
            return None;
        }
        ACTIVE_RESERVED.with(|reserved| {
            // SAFETY: ActiveSystemGuard installs the scope for one scheduled
            // invocation and clears it before its native borrows expire.
            Some(unsafe {
                f(
                    &mut *world,
                    &mut *queue,
                    &*bindings,
                    &mut reserved.borrow_mut(),
                )
            })
        })
    })
}

/// Runs a callback with the world belonging to the active managed invocation,
/// returning `None` when called outside scheduler-controlled execution.
pub(super) fn with_active_world<R>(f: impl FnOnce(&mut World) -> R) -> Option<R> {
    ACTIVE_SCOPE.with(|slot| {
        let pointer = slot.get().map(|scope| scope.world)?;
        // SAFETY: ActiveSystemGuard installs this pointer immediately before
        // managed invocation and clears it before the borrowed world expires.
        (!pointer.is_null()).then(|| unsafe { f(&mut *pointer) })
    })
}

/// Reports whether a managed system scope exists without dereferencing its world.
pub(super) fn active_world_exists() -> bool {
    ACTIVE_SCOPE.with(|slot| slot.get().is_some_and(|scope| !scope.world.is_null()))
}

/// Runs a callback with both the active world and its stable component binding
/// table, which must always be installed and cleared as one guard scope.
pub(super) fn with_active_context<R>(
    f: impl FnOnce(&mut World, &ComponentBindings) -> R,
) -> Option<R> {
    ACTIVE_SCOPE.with(|slot| {
        let scope = slot.get()?;
        if scope.world.is_null() || scope.bindings.is_null() {
            return None;
        }
        // SAFETY: ActiveSystemGuard installs and clears the complete scope
        // for exactly the managed invocation.
        Some(unsafe { f(&mut *scope.world, &*scope.bindings) })
    })
}

/// Record that an archetype was served to managed code through a validated
/// term during the active invocation.
///
/// Called by the chunk callbacks on every successful component or entity
/// lookup; the entity path then serves this archetype's entity column later in
/// the same invocation.
pub(super) fn record_observed_archetype(archetype: ArchetypeId) {
    OBSERVED_ARCHETYPES.with(|slot| {
        if let Ok(mut observed) = slot.try_borrow_mut() {
            observed.insert(archetype);
        }
    });
}

/// Whether a validated term has already served this archetype in the active
/// invocation.
///
/// `None` means no managed system is active on this thread, matching
/// [`access_is_authorized`]'s answer for the same condition.
pub(super) fn archetype_was_observed(archetype: ArchetypeId) -> Option<bool> {
    ACTIVE_SCOPE.with(|slot| {
        slot.get()?;
        OBSERVED_ARCHETYPES.with(|observed| {
            Some(
                observed
                    .try_borrow()
                    .is_ok_and(|observed| observed.contains(&archetype)),
            )
        })
    })
}

/// Checks whether the active system declared the requested component mode.
///
/// A write declaration also permits reads; a read declaration never permits
/// writes. `None` means no managed system is currently active on this thread.
pub(super) fn access_is_authorized(key: StableComponentId, requested_mode: u8) -> Option<bool> {
    ACTIVE_SCOPE.with(|slot| {
        let (pointer, len) = slot.get()?.access;
        if pointer.is_null() {
            return None;
        }
        // SAFETY: ActiveSystemGuard stores a slice owned by the registered
        // system closure and clears the scope before that invocation returns.
        let accesses = unsafe { std::slice::from_raw_parts(pointer, len) };
        Some(accesses.iter().any(|access| {
            StableComponentId::from_halves(access.component_key, access.component_key_high) == key
                && (requested_mode == 0 || access.mode == requested_mode)
        }))
    })
}
