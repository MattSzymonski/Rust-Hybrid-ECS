//! Layout facts every pipeline the renderer builds shares, and the two frames
//! the renderer ships with.
//!
//! # Responsibilities
//!
//! - Name the bind group each kind of shader data occupies: the convention the
//!   engine's shaders declare and the drawers bind against.
//! - Size the instance batch the mesh drawer accumulates and uploads in one
//!   command.
//! - Hold the pipelines the renderer defines itself - [`simple_pipeline`], and
//!   [`pbr_pipeline`] with its [`post_processing`] and [`skybox`] parts - so a
//!   project can run a frame without declaring one, each with its stages under a
//!   `shaders/` directory in the crate's `shader_sources/` tree.
//! - Import every one of their shaders from its `.hlsl` source and `.hlsl.meta`
//!   ([`import_shader`]), the same route a project's own shaders take, and name
//!   the cooked files each was built from ([`all_shader_sources`]) so a runtime
//!   reload of a source reaches the assets built from it.
//!
//! # Design
//!
//! Every pipeline here is an ordinary `RenderingPipeline` asset. Nothing about
//! them is special to the renderer beyond the fact that it installs one, and
//! each `install` is idempotent - it returns what the store already holds rather
//! than adding a second copy - which is what lets the renderer's own
//! registration call one of them on every generation.
//!
//! One construction route, not two: a built-in shader is imported exactly as a
//! project's shader is, so its slots, its bindings and the guid material files
//! name live beside its source in a `.hlsl.meta`, and the store lists it as the
//! file it is. The only stage text this crate embeds is the pair the GPU module's
//! fallback shader is built from ([`DEFAULT_VERTEX`] and
//! [`simple_pipeline::DEFAULT_LIT_FRAGMENT`]), which has no store of its own to
//! be read from.
//!
//! [`pbr_pipeline`] is the default: [`register`](crate::register) installs it and
//! points [`RenderingManager`](crate::RenderingManager) at it. A project that
//! wants a different frame calls `set_pipeline` after registering, and one that
//! wants the renderer's own fallback chain clears the manager instead.

/// Declares a [`ConfigShaderFile`] constant for a cooked file under `shader_sources/`.
///
/// One literal feeds both halves: `include_str!` embeds the file at build time,
/// and the same path lets a runtime reload find it again. Written as a macro
/// because `include_str!` only accepts a literal, never a `const`.
macro_rules! config_shader_file {
    ($(#[$attribute:meta])* $visibility:vis $name:ident, $path:literal) => {
        $(#[$attribute])*
        $visibility const $name: $crate::config::ConfigShaderFile = $crate::config::ConfigShaderFile {
            relative_path: $path,
            embedded_source: include_str!(concat!(
                env!("CARGO_MANIFEST_DIR"),
                "/shader_sources/",
                $path
            )),
        };
    };
}

/// The PBR frame: the lit geometry pass and the post-processing half it
/// composes.
pub mod pbr_pipeline;
/// The post-processing passes the PBR frame composes.
pub mod post_processing;
/// The built-in lit pipeline, the fallback a project can opt into.
pub mod simple_pipeline;
/// The sky pass drawn where the lit pass wrote nothing.
pub mod skybox;

// Standard library

// External crates
use pill_engine::{AssetGuid, AssetImport, AssetManager, Handle, MetadataPolicy};

// Current crate
use crate::assets::Shader;

/// A cooked WGSL file the renderer embeds at build time.
///
/// Only the two stages the GPU module's fallback shader is built from are
/// embedded: every other built-in shader is imported from its source like a
/// project's, so its text is read from the store rather than compiled into
/// this crate.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigShaderFile {
    /// Path relative to the crate's `shader_sources/`, e.g.
    /// `pbr_pipeline/shaders/pbr_fragment.wgsl`.
    pub relative_path: &'static str,
    /// The file's text as `include_str!` embedded it when the crate was built.
    pub embedded_source: &'static str,
}

/// Which cooked files a built-in shader asset was built from.
///
/// One entry per shader the renderer's own pipelines install, so the
/// development shader reload can map an edited cooked file back to the assets
/// built from it. The paths are the cooked `.wgsl` files the host hands over,
/// not the `.hlsl` sources it cooked them from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShaderSourceRecord {
    /// The name the shader asset is stored under: the path of the `.hlsl`
    /// source it was imported from, because an imported asset is named by the
    /// path it came from.
    pub shader_asset_name: &'static str,
    /// Its vertex stage.
    pub vertex_path: &'static str,
    /// Its fragment stage.
    pub fragment_path: &'static str,
}

config_shader_file!(
    /// The vertex stage every lit pass in the crate starts from.
    pub DEFAULT_VERTEX, "common_shaders/default_vertex.wgsl"
);

/// Import one of the renderer's own shaders, or hand back the one a store
/// already holds.
///
/// The path is the `.hlsl` source, relative to this crate's `shader_sources/`;
/// the settings come from the `.hlsl.meta` beside it, so the slots, the
/// bindings and the guid are declared once, next to the source, exactly as a
/// project's shader declares them. A meta carrying a guid other than `guid` is
/// a packaging fault and stops here: the store would otherwise hold a shader no
/// material file could name.
///
/// # Errors
///
/// Returns the store's import error when the source or its meta will not load.
pub(crate) fn import_shader(
    assets: &mut AssetManager,
    path: &str,
    guid: AssetGuid,
) -> Result<Handle<Shader>, Box<dyn std::error::Error>> {
    if let Some(handle) = assets.handle_by_name::<Shader>(path) {
        return Ok(handle);
    }
    let imported = assets.import(AssetImport::<Shader>::new(
        path,
        MetadataPolicy::ReadIfPresent,
    ))?;
    assert_eq!(
        imported.guid,
        Some(guid),
        "`{path}` must carry the guid its module declares"
    );
    Ok(imported.handle)
}

/// Every shader asset the renderer's own pipelines install, with its files.
///
/// Used by the development shader reload to map an edited file back to the
/// assets built from it; the pipelines' `install` functions import from the
/// same paths, so the two cannot drift.
pub fn all_shader_sources() -> impl Iterator<Item = &'static ShaderSourceRecord> {
    pbr_pipeline::SHADER_SOURCES
        .iter()
        .chain(post_processing::SHADER_SOURCES)
        .chain(simple_pipeline::SHADER_SOURCES)
        .chain(skybox::SHADER_SOURCES)
}

/// Instances one draw command covers.
///
/// The drawer splits a frame's queue into chunks of this size and gives each
/// chunk its own region of the instance buffer, so it is a batching choice, not
/// a limit on how many instances a frame may draw.
pub const INSTANCE_BATCH_SIZE: usize = 10000;

/// Initial capacity for a staging vector of instances, sized like one batch.
pub const INITIAL_INSTANCE_VECTOR_CAPACITY: usize = 10000;

/// Bind group the engine's per-frame parameters occupy in every shader.
pub const ENGINE_PARAMETERS_BIND_GROUP_LAYOUT_INDEX: u32 = 0;
/// Bind group the active camera's parameters occupy in every shader.
pub const CAMERA_PARAMETERS_BIND_GROUP_LAYOUT_INDEX: u32 = 1;
/// Bind group a material's, or a pass's, uniform parameters occupy.
pub const MATERIAL_PARAMETERS_BIND_GROUP_LAYOUT_INDEX: u32 = 2;
/// Bind group a material's, or a pass's, textures occupy.
pub const MATERIAL_TEXTURES_BIND_GROUP_LAYOUT_INDEX: u32 = 3;

#[cfg(test)]
mod tests {
    use super::*;
    use crate::assets::Shader;
    use pill_engine::{AssetManager, Engine};
    use std::path::PathBuf;

    /// The crate's own `shader_sources/` directory.
    fn sources_directory() -> PathBuf {
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("shader_sources")
    }

    /// Every record names a shader the pipelines really install, and the two
    /// cooked files its stages came from are the ones the record points at.
    ///
    /// This is what keeps the reload table honest: a record whose paths moved
    /// would send an edited file nowhere, and a meta naming a different vertex
    /// stage would be caught here rather than as a shader that draws wrong.
    #[test]
    fn every_shader_source_record_matches_an_installed_shader() {
        let mut engine = Engine::new();
        crate::register(&mut engine);
        let assets = engine
            .world_mut()
            .get_resource_mut::<AssetManager>()
            .expect("the engine inserts the store");
        // `register` installs the PBR chain; the simple one is opt-in.
        simple_pipeline::install(assets).expect("a free name");

        let sources = sources_directory();
        for record in all_shader_sources() {
            let shader = assets
                .get_by_name::<Shader>(record.shader_asset_name)
                .unwrap_or_else(|| panic!("{} is not installed", record.shader_asset_name));
            for path in [record.vertex_path, record.fragment_path] {
                let on_disk = std::fs::read_to_string(sources.join(path))
                    .unwrap_or_else(|error| panic!("{path}: {error}"));
                let stage = if path == record.vertex_path {
                    &shader.vertex_wgsl
                } else {
                    &shader.fragment_wgsl
                };
                assert_eq!(stage, &on_disk, "{}", record.shader_asset_name);
            }
        }
    }

    /// The two embedded stages are the files they name, so a path typo cannot
    /// hide behind an `include_str!` of some other file.
    #[test]
    fn every_embedded_stage_is_the_file_it_names() {
        let sources = sources_directory();
        for file in [DEFAULT_VERTEX, simple_pipeline::DEFAULT_LIT_FRAGMENT] {
            let on_disk = std::fs::read_to_string(sources.join(file.relative_path))
                .unwrap_or_else(|error| panic!("{}: {error}", file.relative_path));
            assert_eq!(on_disk, file.embedded_source, "{}", file.relative_path);
        }
    }
}
