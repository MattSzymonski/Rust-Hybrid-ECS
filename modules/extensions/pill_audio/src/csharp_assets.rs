//! The C# bridge's sound import: one named export for `Sound`.
//!
//! # Responsibilities
//!
//! - Import a sound from `res` through its metadata file for a managed caller
//!   ([`pill_audio_import_sound`]), through [`pill_engine::asset_ffi`].
//! - Offer it by name: as a `#[no_mangle]` export when this crate is a loaded
//!   module (`module-abi`), and through a [`PillExportDescriptor`] when it is
//!   linked statically.
//!
//! # Design
//!
//! The export lives here, beside `Sound`, because this is where the type's
//! `import` is compiled; the host finds it by name, as it finds the renderer
//! data crate's texture and mesh imports, and forwards the managed call with
//! the active invocation's world.

// External crates
use pill_engine::asset_ffi::{import_for_ffi, NativeImportedAsset};
use pill_engine::component_registry::{ExportAddress, PillExportDescriptor};
use pill_engine::World;

// Current crate
use crate::Sound;

/// Imports the sound at `path` (relative to `res`) through its `.meta` file
/// and writes the result to `output`; see
/// [`import_for_ffi`](pill_engine::asset_ffi::import_for_ffi) for the
/// arguments and status codes.
///
/// # Safety
///
/// The contract of [`import_for_ffi`](pill_engine::asset_ffi::import_for_ffi).
#[cfg_attr(feature = "module-abi", no_mangle)]
pub unsafe extern "C" fn pill_audio_import_sound(
    world: *mut World,
    path: *const u8,
    path_length: u32,
    policy: u8,
    settings: *const u8,
    settings_length: u32,
    output: *mut NativeImportedAsset,
) -> u8 {
    // SAFETY: forwarded unchanged from this function's contract.
    unsafe {
        import_for_ffi::<Sound>(
            world,
            path,
            path_length,
            policy,
            settings,
            settings_length,
            output,
        )
    }
}

// Offer the function to a host that links this crate statically; a loaded
// module offers it through the `#[no_mangle]` export above instead.
pill_engine::submit! {
    PillExportDescriptor {
        name: "pill_audio_import_sound",
        address: ExportAddress(pill_audio_import_sound as *const ()),
    }
}

#[cfg(test)]
mod tests {
    /// A binary linking this crate finds the export by name, as a shipping
    /// host does.
    #[test]
    fn the_sound_import_is_found_by_name() {
        assert!(pill_engine::component_registry::find_export("pill_audio_import_sound").is_some());
    }
}
