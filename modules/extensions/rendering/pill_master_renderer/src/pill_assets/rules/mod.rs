//! Built-in native asset conversion rules.
//!
//! # Responsibilities
//!
//! - Exposes shader, image, mesh, and environment converters.
//! - Assembles the default rule collection used by the cooker and watcher.
//!
//! # Design
//!
//! Each converter implements [`Rule`]. The pipeline uses declared dependencies
//! to order execution; in particular, image and procedural panorama generation
//! must finish before the IBL rule discovers cooked environment textures.

// Current crate
use crate::pill_assets::Rule;

// =============================================================================
// Converters
// =============================================================================

pub mod equirect_to_ibl;
pub mod glb_to_cooked_mesh;
pub mod hlsl_to_wgsl;
pub mod obj_to_cooked_mesh;
pub mod png_to_cooked_tex;
pub mod procedural_equirect;

pub use equirect_to_ibl::EquirectToIBL;
pub use glb_to_cooked_mesh::GlbToCookedMesh;
pub use hlsl_to_wgsl::HlslToWgsl;
pub use obj_to_cooked_mesh::ObjToCookedMesh;
pub use png_to_cooked_tex::PngToCookedTex;
pub use procedural_equirect::ProceduralEquirect;

// =============================================================================
// Default Rule Set
// =============================================================================

/// Built-in rule set used by the native cooker and its watcher.
pub fn default_rules() -> Vec<Box<dyn Rule>> {
    vec![
        Box::new(HlslToWgsl),
        Box::new(PngToCookedTex),
        Box::new(ProceduralEquirect), // generates *_equirect.cooked_tex from *.procedural_equirect
        Box::new(EquirectToIBL),      // generates IBL maps from *_equirect.cooked_tex
        Box::new(ObjToCookedMesh),
        Box::new(GlbToCookedMesh),
    ]
}
