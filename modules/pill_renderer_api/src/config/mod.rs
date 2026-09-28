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
//!   [`pbr_pipeline`] with its [`post_processing`] half - so a project can run a
//!   frame without declaring one, each with its shaders in a `shaders/` folder
//!   beside it.
//! - Declare every cooked shader file once ([`ConfigShaderFile`]), and which
//!   shader asset each pair of them builds ([`all_shader_sources`]), so the
//!   embedded text and a runtime reload of it can never name different files.
//!
//! # Design
//!
//! Every pipeline here is an ordinary `RenderingPipeline` asset. Nothing about
//! them is special to the renderer beyond the fact that it installs one, and
//! each `install` is idempotent - it returns what the store already holds rather
//! than adding a second copy - which is what lets the renderer's own
//! registration call one of them on every generation.
//!
//! [`pbr_pipeline`] is the default: [`register`](crate::register) installs it and
//! points [`RenderingManager`](crate::RenderingManager) at it. A project that
//! wants a different frame calls `set_pipeline` after registering, and one that
//! wants the renderer's own fallback chain clears the manager instead.

/// Declares a [`ConfigShaderFile`] constant for a cooked file under `src/config/`.
///
/// One literal feeds both halves: `include_str!` embeds the file at build time,
/// and the same path lets a runtime reload find it again. Written as a macro
/// because `include_str!` only accepts a literal, never a `const`.
macro_rules! config_shader_file {
    ($(#[$attribute:meta])* $visibility:vis $name:ident, $path:literal) => {
        $(#[$attribute])*
        $visibility const $name: $crate::config::ConfigShaderFile = $crate::config::ConfigShaderFile {
            relative_path: $path,
            embedded_source: include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/src/config/", $path)),
        };
    };
}

pub mod pbr_pipeline;
pub mod post_processing;
/// The shader trees and the routine that cooks them, shared with `build.rs`.
///
/// `build.rs` reads the inputs this reports and the reload reads the rebuilt
/// outputs, so each compilation leaves part of it unread.
#[cfg(feature = "shader-hot-reload")]
#[allow(dead_code)]
pub(crate) mod shader_roots;
pub mod simple_pipeline;

/// A cooked WGSL file the renderer's own pipelines are built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ConfigShaderFile {
    /// Path relative to `src/config/`, e.g. `pbr_pipeline/shaders/pbr_fragment.wgsl`.
    pub relative_path: &'static str,
    /// The file's text as `include_str!` embedded it when the crate was built.
    pub embedded_source: &'static str,
}

/// Which cooked files a named shader asset of the renderer's is built from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ShaderSourceRecord {
    /// The name the shader asset is stored under.
    pub shader_asset_name: &'static str,
    /// Its vertex stage.
    pub vertex: ConfigShaderFile,
    /// Its fragment stage.
    pub fragment: ConfigShaderFile,
}

config_shader_file!(
    /// The vertex stage every lit pass in the crate starts from.
    pub DEFAULT_VERTEX, "common_shaders/default_vertex.wgsl"
);

/// Every shader asset the renderer's own pipelines install, with its files.
///
/// Used by the development shader reload to map an edited file back to the
/// assets built from it; the pipelines' `install` functions build from the
/// same constants, so the two cannot drift.
pub fn all_shader_sources() -> impl Iterator<Item = &'static ShaderSourceRecord> {
    pbr_pipeline::SHADER_SOURCES
        .iter()
        .chain(post_processing::SHADER_SOURCES)
        .chain(simple_pipeline::SHADER_SOURCES)
}

/// Instances one draw command covers.
///
/// The drawer splits a frame's queue into chunks of this size and gives each
/// chunk its own region of the instance buffer, so it is a batching choice, not
/// a limit on how many instances a frame may draw.
pub const INSTANCE_BATCH_SIZE: usize = 10000;

/// Starting capacity of the drawer's staging instance vector.
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

    /// Every record names a shader the pipelines really install, built from
    /// exactly the files the record points at.
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

        for record in all_shader_sources() {
            let shader = assets
                .get_by_name::<Shader>(record.shader_asset_name)
                .unwrap_or_else(|| panic!("{} is not installed", record.shader_asset_name));
            assert_eq!(shader.vertex_wgsl, record.vertex.embedded_source);
            assert_eq!(shader.fragment_wgsl, record.fragment.embedded_source);
        }
    }

    /// The embedded text is the file on disk, so a path typo cannot hide
    /// behind an `include_str!` of some other file.
    #[test]
    fn every_declared_file_is_the_file_it_names() {
        let config_directory = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/config");
        for record in all_shader_sources() {
            for file in [record.vertex, record.fragment] {
                let on_disk = std::fs::read_to_string(config_directory.join(file.relative_path))
                    .unwrap_or_else(|error| panic!("{}: {error}", file.relative_path));
                assert_eq!(on_disk, file.embedded_source, "{}", file.relative_path);
            }
        }
    }
}
