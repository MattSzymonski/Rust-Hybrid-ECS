//! Development shader reload, data half: re-cooked WGSL in, shader assets out.
//!
//! # Responsibilities
//!
//! - Map one re-cooked WGSL file back to the shader assets built from it,
//!   through the same table the pipelines embed their WGSL from.
//! - Put the new text into those assets, which moves their content version, so
//!   the GPU module rebuilds exactly those shaders on its next sync.
//! - Offer that as `pill_render_data_shader_changed`, a named C-ABI export the
//!   development host calls.
//!
//! # Design
//!
//! Watching and cooking are the host's: a watcher thread there cooks the
//! crate's `shaders/` with `pill_assets::cook_shader_tree` - the routine this
//! crate's build script runs - and hands each re-cooked file to the main thread,
//! which calls this export from the data module generation that is current at
//! that moment. The data crate stays free of `notify` and of the cooker, and a
//! reload of it never leaves a watcher thread running code in a retired image.
//!
//! A shader edit is an asset edit. The renderer's pipelines store their shaders
//! as `Shader` assets, and the GPU module rebuilds any asset whose content
//! version moved, so nothing here touches a device, a pipeline or a bind group.
//! An asset is only rewritten when the new text differs from what it holds, so
//! a save that changes nothing costs no GPU work.
//!
//! Compiled into native development builds and the tests: a shipping build has
//! no sources to watch and no host to deliver edits.

// Standard library
use std::slice;

// External crates
use pill_engine::{AssetManager, World};

// Current crate
use crate::assets::Shader;
use crate::config::{all_shader_sources, ShaderSourceRecord};

/// What [`pill_render_data_shader_changed`] returns when it could not look at
/// the assets at all: a null world, a world without an `AssetManager`, or a
/// path or text that is not UTF-8.
pub const SHADER_CHANGE_REFUSED: u32 = u32::MAX;

/// Replace one stage's WGSL in every recorded shader asset built from
/// `relative_path`, and return the names of the assets whose text changed.
///
/// `relative_path` is relative to the crate's `shaders/`, with `/` separators,
/// as the records spell it. A record whose asset is not in the store - a
/// pipeline this project never installed - is skipped.
pub fn apply_shader_source<'a>(
    assets: &mut AssetManager,
    relative_path: &str,
    wgsl: &str,
    records: impl IntoIterator<Item = &'a ShaderSourceRecord>,
) -> Vec<&'static str> {
    let mut updated = Vec::new();
    for record in records {
        let feeds_vertex = record.vertex.relative_path == relative_path;
        let feeds_fragment = record.fragment.relative_path == relative_path;
        if !feeds_vertex && !feeds_fragment {
            continue;
        }
        let Some(current) = assets.get_by_name::<Shader>(record.shader_asset_name) else {
            continue;
        };
        let unchanged = (!feeds_vertex || current.vertex_wgsl == wgsl)
            && (!feeds_fragment || current.fragment_wgsl == wgsl);
        if unchanged {
            continue;
        }
        // The mutable borrow is what moves the asset's content version, so it
        // is taken only for a shader whose text really changed.
        if let Some(shader) = assets.get_by_name_mut::<Shader>(record.shader_asset_name) {
            if feeds_vertex {
                shader.vertex_wgsl = wgsl.to_owned();
            }
            if feeds_fragment {
                shader.fragment_wgsl = wgsl.to_owned();
            }
            updated.push(record.shader_asset_name);
        }
    }
    updated
}

/// Put one re-cooked WGSL file into the shader assets built from it, in
/// `world`'s `AssetManager`.
///
/// `path` is the cooked file relative to the crate's `shaders/`, with `/`
/// separators (`pbr_pipeline/shaders/pbr_fragment.wgsl`); `wgsl` is its text.
/// Returns how many shader assets changed, or [`SHADER_CHANGE_REFUSED`].
///
/// # Safety
///
/// `world` must be null or point at a live `World` no one else is using for
/// the call's duration. `path`/`wgsl` must reference their declared lengths in
/// readable memory, unless the matching length is zero.
#[no_mangle]
pub unsafe extern "C" fn pill_render_data_shader_changed(
    world: *mut World,
    path: *const u8,
    path_len: usize,
    wgsl: *const u8,
    wgsl_len: usize,
) -> u32 {
    // SAFETY: forwarded from the caller's contract.
    let (Some(path), Some(wgsl)) = (unsafe { read_text(path, path_len) }, unsafe {
        read_text(wgsl, wgsl_len)
    }) else {
        return SHADER_CHANGE_REFUSED;
    };
    // SAFETY: the caller guarantees `world` is null or live and unshared.
    let Some(world) = (unsafe { world.as_mut() }) else {
        return SHADER_CHANGE_REFUSED;
    };
    let Some(assets) = world.get_resource_mut::<AssetManager>() else {
        return SHADER_CHANGE_REFUSED;
    };
    let updated = apply_shader_source(assets, path, wgsl, all_shader_sources());
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::HOT_RELOAD,
        path,
        shaders = ?updated,
        "shader source applied"
    );
    updated.len() as u32
}

/// Borrow `length` bytes at `pointer` as UTF-8 text; `None` when they are not.
///
/// # Safety
///
/// `pointer` must reference `length` readable bytes, unless `length` is zero.
unsafe fn read_text<'a>(pointer: *const u8, length: usize) -> Option<&'a str> {
    if length == 0 {
        return Some("");
    }
    if pointer.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees `length` readable bytes at `pointer`.
    let bytes = unsafe { slice::from_raw_parts(pointer, length) };
    std::str::from_utf8(bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::ConfigShaderFile;

    /// A record for one vertex and one fragment file, stored as `test.shader`.
    const RECORD: ShaderSourceRecord = ShaderSourceRecord {
        shader_asset_name: "test.shader",
        vertex: ConfigShaderFile {
            relative_path: "test/test_vertex.wgsl",
            embedded_source: "vertex one",
        },
        fragment: ConfigShaderFile {
            relative_path: "test/test_fragment.wgsl",
            embedded_source: "fragment one",
        },
    };

    /// A store holding `test.shader`, built from the embedded text.
    fn store_with_shader() -> (AssetManager, pill_engine::Handle<Shader>) {
        let mut assets = AssetManager::new();
        let shader = Shader::new("test")
            .with_wgsl(
                RECORD.vertex.embedded_source,
                RECORD.fragment.embedded_source,
            )
            .build()
            .expect("both stages set");
        let handle = assets
            .add_named(RECORD.shader_asset_name, shader)
            .expect("a free name");
        (assets, handle)
    }

    #[test]
    fn an_edited_stage_replaces_its_text_and_moves_the_version() {
        let (mut assets, handle) = store_with_shader();
        let before = assets.content_version(handle);

        let updated = apply_shader_source(
            &mut assets,
            "test/test_fragment.wgsl",
            "fragment two",
            [&RECORD],
        );

        assert_eq!(updated, ["test.shader"]);
        let shader = assets.get(handle).expect("still live");
        assert_eq!(shader.fragment_wgsl, "fragment two");
        assert_eq!(shader.vertex_wgsl, "vertex one");
        assert_ne!(assets.content_version(handle), before);
    }

    #[test]
    fn unchanged_text_leaves_the_version_alone() {
        let (mut assets, handle) = store_with_shader();
        let before = assets.content_version(handle);

        let updated = apply_shader_source(
            &mut assets,
            "test/test_vertex.wgsl",
            "vertex one",
            [&RECORD],
        );

        assert!(updated.is_empty());
        assert_eq!(assets.content_version(handle), before);
    }

    #[test]
    fn a_path_no_record_names_changes_nothing() {
        let (mut assets, handle) = store_with_shader();
        let before = assets.content_version(handle);

        let updated = apply_shader_source(&mut assets, "other/other.wgsl", "text", [&RECORD]);

        assert!(updated.is_empty());
        assert_eq!(assets.content_version(handle), before);
    }

    #[test]
    fn a_shader_the_store_does_not_hold_is_skipped() {
        let mut assets = AssetManager::new();

        assert!(
            apply_shader_source(&mut assets, "test/test_vertex.wgsl", "text", [&RECORD]).is_empty()
        );
    }

    #[test]
    fn the_export_writes_through_a_world() {
        // One of the crate's own records, so the export's table finds it.
        let record = all_shader_sources()
            .next()
            .expect("the crate records shaders");
        let mut assets = AssetManager::new();
        let shader = Shader::new("recorded")
            .with_wgsl(
                record.vertex.embedded_source,
                record.fragment.embedded_source,
            )
            .build()
            .expect("both stages set");
        let handle = assets
            .add_named(record.shader_asset_name, shader)
            .expect("a free name");
        let mut world = World::new();
        world.insert_resource(assets);
        let path = record.fragment.relative_path;
        let text = "edited fragment";

        // SAFETY: a live, unshared world and two valid UTF-8 buffers.
        let changed = unsafe {
            pill_render_data_shader_changed(
                &mut world,
                path.as_ptr(),
                path.len(),
                text.as_ptr(),
                text.len(),
            )
        };

        assert_eq!(changed, 1);
        let assets = world.get_resource::<AssetManager>().expect("inserted");
        let shader = assets.get(handle).expect("still live");
        assert_eq!(shader.fragment_wgsl, text);
        assert_eq!(shader.vertex_wgsl, record.vertex.embedded_source);
    }

    #[test]
    fn the_export_refuses_a_null_world_and_non_utf8_text() {
        let path = "a.wgsl";
        let invalid = [0xff_u8, 0xfe];

        // SAFETY: a null world is part of the contract; the buffers are valid.
        let null_world = unsafe {
            pill_render_data_shader_changed(
                std::ptr::null_mut(),
                path.as_ptr(),
                path.len(),
                path.as_ptr(),
                path.len(),
            )
        };
        let mut world = World::new();
        // SAFETY: a live world; `invalid` is two readable bytes.
        let bad_text = unsafe {
            pill_render_data_shader_changed(
                &mut world,
                path.as_ptr(),
                path.len(),
                invalid.as_ptr(),
                invalid.len(),
            )
        };

        assert_eq!(null_world, SHADER_CHANGE_REFUSED);
        assert_eq!(bad_text, SHADER_CHANGE_REFUSED);
    }
}
