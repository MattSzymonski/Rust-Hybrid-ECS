//! The native calls the managed side of the mirror ABI needs beside the
//! trampolines themselves.
//!
//! # Responsibilities
//!
//! - Drain the two text channels `pill_engine::mirror` keeps per thread - the
//!   last error and the returned string - into managed strings.
//! - Hand a mirrored call the address of a Rust-owned resource
//!   (`ResMut<AssetManager>`), under the running invocation's declared access.
//! - Read a file through the engine's asset store for `AssetLoader.Load()`.
//! - Build the mirror-method table of a statically linked binary from its own
//!   inventory, for the shipping posture.
//!
//! # Design
//!
//! None of this names a renderer or audio type: every extension's C# surface
//! is generated from its own mirror attributes and reaches its own trampolines
//! by address. What lives here is engine-wide - the world's resources, the
//! asset store, and the channels every trampoline writes to.
//!
//! A string or byte run handed out here stays valid until the next call of the
//! same kind on the same thread: it is parked in a thread-local, and managed
//! code copies it before it does anything else.

// Standard library
use std::cell::RefCell;
use std::ffi::c_void;

// External crates
use pill_engine::component_registry::mirror_method_descriptors;
use pill_engine::{AssetLoader, ResourceId};

// Current crate
use super::components::StableComponentId;
use super::context::{
    guard_managed_callback, native_resource_access_is_authorized, with_active_world,
};
use super::ResolvedMirrorMethod;

// =============================================================================
// Status Codes
// =============================================================================

/// The call succeeded.
const STATUS_OK: u8 = 0;
/// The thing asked for does not exist: no text in the channel, or no value of
/// the resource in the world.
const STATUS_MISSING: u8 = 1;
/// The running system did not declare this access.
const STATUS_ACCESS_NOT_DECLARED: u8 = 2;
/// No managed invocation is running on this thread.
const STATUS_NO_ACTIVE_SCOPE: u8 = 3;
/// A panic was caught inside the call.
const STATUS_PANICKED: u8 = 4;
/// The caller passed no output buffer.
const STATUS_NULL_OUTPUT: u8 = 5;

/// `ffi_take_mirror_text` kind for the last error.
const TEXT_LAST_ERROR: u8 = 0;
/// `ffi_take_mirror_text` kind for the returned string.
const TEXT_RETURN_STRING: u8 = 1;

thread_local! {
    /// The text handed out by the last `ffi_take_mirror_text` on this thread.
    static HELD_TEXT: RefCell<String> = const { RefCell::new(String::new()) };
    /// The bytes handed out by the last `ffi_asset_loader_read` on this thread.
    static HELD_BYTES: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
}

// =============================================================================
// Text Channels
// =============================================================================

/// Take the last error (`kind` 0) or the returned string (`kind` 1) of the
/// mirrored call that just ran on this thread.
///
/// Writes a pointer and a length the managed side copies before its next
/// call; status `0` handed text out, `1` the channel was empty, `5` an output
/// was null.
pub(super) extern "C" fn ffi_take_mirror_text(
    kind: u8,
    out_data: *mut *const u8,
    out_length: *mut u32,
) -> u8 {
    if out_data.is_null() || out_length.is_null() {
        return STATUS_NULL_OUTPUT;
    }
    guard_managed_callback("take_mirror_text", STATUS_PANICKED, || {
        let text = match kind {
            TEXT_LAST_ERROR => pill_engine::mirror::take_last_error(),
            TEXT_RETURN_STRING => pill_engine::mirror::take_return_string(),
            _ => None,
        };
        let Some(text) = text else {
            return STATUS_MISSING;
        };
        HELD_TEXT.with(|held| {
            let mut held = held.borrow_mut();
            *held = text;
            // SAFETY: both outputs were checked non-null above, and the
            // managed caller passes writable locals.
            unsafe {
                *out_data = held.as_ptr();
                *out_length = u32::try_from(held.len()).unwrap_or(u32::MAX);
            }
        });
        STATUS_OK
    })
}

// =============================================================================
// Native Resources
// =============================================================================

/// Write the address of the Rust-owned resource whose shared name hashes to
/// `low`/`high`, for a mirrored call that takes it as `&T` (`mode` 0) or
/// `&mut T` (`mode` 1).
///
/// Status `0` wrote it, `1` the world holds no Rust value under that name,
/// `2` the running system did not declare the access, `3` no invocation is
/// running, `5` the output was null. A startup holds the world exclusively
/// and is always authorized.
pub(super) extern "C" fn ffi_get_native_resource(
    low: u64,
    high: u64,
    mode: u8,
    output: *mut *mut c_void,
) -> u8 {
    if output.is_null() {
        return STATUS_NULL_OUTPUT;
    }
    guard_managed_callback("get_native_resource", STATUS_PANICKED, || {
        let key = StableComponentId::from_halves(low, high);
        match native_resource_access_is_authorized(key, mode) {
            None => return STATUS_NO_ACTIVE_SCOPE,
            Some(false) => return STATUS_ACCESS_NOT_DECLARED,
            Some(true) => {}
        }
        let pointer = with_active_world(|world| {
            world.native_resource_pointer(ResourceId::Shared(key.0), mode == 1)
        });
        match pointer {
            None => STATUS_NO_ACTIVE_SCOPE,
            Some(None) => STATUS_MISSING,
            Some(Some(pointer)) => {
                // SAFETY: checked non-null above; the managed caller passes a
                // writable local.
                unsafe { *output = pointer.cast() };
                STATUS_OK
            }
        }
    })
}

// =============================================================================
// Asset Store
// =============================================================================

/// Read the file at `path` (relative to `res`, through the mounted packs) for
/// `AssetLoader.Load()`.
///
/// Writes a pointer and a length the managed side copies before its next
/// call; status `0` read it, `1` the read failed with its message in the
/// last-error channel, `5` an output was null.
pub(super) extern "C" fn ffi_asset_loader_read(
    path: *const u8,
    path_length: u32,
    out_data: *mut *const u8,
    out_length: *mut u32,
) -> u8 {
    if out_data.is_null() || out_length.is_null() {
        return STATUS_NULL_OUTPUT;
    }
    guard_managed_callback("asset_loader_read", STATUS_PANICKED, || {
        let path = if path_length == 0 || path.is_null() {
            String::new()
        } else {
            // SAFETY: the managed caller pins `path_length` bytes for the call.
            let bytes = unsafe { std::slice::from_raw_parts(path, path_length as usize) };
            String::from_utf8_lossy(bytes).into_owned()
        };
        match AssetLoader::Path(path.into()).load() {
            Ok(bytes) => {
                HELD_BYTES.with(|held| {
                    let mut held = held.borrow_mut();
                    *held = bytes;
                    // SAFETY: both outputs were checked non-null above.
                    unsafe {
                        *out_data = held.as_ptr();
                        *out_length = u32::try_from(held.len()).unwrap_or(u32::MAX);
                    }
                });
                STATUS_OK
            }
            Err(error) => {
                pill_engine::mirror::set_last_error(error.to_string());
                STATUS_MISSING
            }
        }
    })
}

// =============================================================================
// Static Mirror Table
// =============================================================================

/// The mirror-method table of this binary, from its own inventory.
///
/// A shipping build links every extension into one image, so the descriptors
/// they submitted are this artifact's and carry their trampolines' addresses;
/// no module has to be loaded to read them. The development host builds the
/// same rows from each loaded module's copy instead.
pub(super) fn static_mirror_methods() -> Vec<ResolvedMirrorMethod> {
    mirror_method_descriptors()
        .into_iter()
        .map(|descriptor| ResolvedMirrorMethod {
            type_name: descriptor.type_name.to_string(),
            method_name: descriptor.name.to_string(),
            return_tag: descriptor.return_tag.to_string(),
            arg_tags: descriptor
                .arg_tags
                .iter()
                .map(|tag| tag.to_string())
                .collect(),
            arg_names: descriptor
                .arg_names
                .iter()
                .map(|name| name.to_string())
                .collect(),
            address: descriptor.address.0 as usize,
            is_free_function: descriptor.is_free_function,
            crate_name: descriptor.crate_name.to_string(),
            receiver: descriptor.receiver.to_string(),
            owner_kind: descriptor.owner_kind.to_string(),
        })
        .collect()
}

/// Whether a resolved row is one managed code can call.
///
/// An enum's or a resource's type row (`__type`) describes the type and has
/// no trampoline; an object's type row is its drop and does. Only rows with an
/// address go into the table managed code calls through.
pub(super) fn is_callable(row: &ResolvedMirrorMethod) -> bool {
    row.address != 0
}
