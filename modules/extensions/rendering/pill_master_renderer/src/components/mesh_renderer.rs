//! What an entity draws: a mesh and the material that shades it.
//!
//! # Responsibilities
//!
//! - Define [`MeshRendererComponent`], the pair of handles that makes an
//!   entity drawable, and [`MeshRendererComponentBuilder`], the chain that
//!   assembles one.
//!
//! # Design
//!
//! Shared and persistable, so the managed mirror binds the same two handles
//! and the host carries them across reload generations. The shared name is
//! pinned rather than derived from the module path; see [`crate::components`].

// External crates
use pill_engine::{Handle, PillComponent, World};
use serde::{Deserialize, Serialize};

// Current crate
use crate::assets::{Material, Mesh};

// --- Builder ---

/// Assembles a [`MeshRendererComponent`] one handle at a time.
///
/// The chain starts from the component's `Default`, so a caller names only the
/// handles it actually has and `build` returns a value ready to attach.
pub struct MeshRendererComponentBuilder {
    component: MeshRendererComponent,
}

impl MeshRendererComponentBuilder {
    /// Sets the mesh the entity draws with.
    pub fn mesh(mut self, mesh: &Handle<Mesh>) -> Self {
        self.component.mesh = *mesh;
        self
    }

    /// Sets the material that shades the mesh.
    pub fn material(mut self, material: &Handle<Material>) -> Self {
        self.component.material = *material;
        self
    }

    /// Finishes the chain and returns the assembled component.
    pub fn build(self) -> MeshRendererComponent {
        self.component
    }
}

// --- Component ---

/// What an entity draws: a mesh paired with the material that shades it.
///
/// The extraction system collects every entity carrying this and emits one
/// instance per surviving pair, skipping any whose handles no longer resolve
/// in the asset manager, so a dangling handle draws nothing instead of failing
/// the frame. The shared name is pinned explicitly instead of being derived
/// from the module path, keeping the identity stable across the split into
/// separate files.
#[repr(C)]
#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PillComponent)]
// Pinned to the name this type carried before it moved into `components/`. A
// derived name follows `module_path!()`, so re-deriving it would register a
// different component and orphan every column a live world holds for this one.
#[pill(
    shared = "pill_master_renderer::component::MeshRendererComponent",
    persistable
)]
pub struct MeshRendererComponent {
    /// Mesh this entity draws.
    pub mesh: Handle<Mesh>,
    /// Material its instances are shaded with; each batch finds its pipeline
    /// through this material's shader.
    pub material: Handle<Material>,
}

impl MeshRendererComponent {
    /// Starts a builder for an entity that should draw.
    pub fn builder() -> MeshRendererComponentBuilder {
        MeshRendererComponentBuilder {
            component: Self::default(),
        }
    }
}

// --- Registration ---

/// Registers [`MeshRendererComponent`] with the world, field layout included.
///
/// The derive generates `__pill_register_MeshRendererComponent` as a private
/// item of this module, so [`crate::components::register_components`] reaches
/// it through here rather than by path.
pub(crate) fn register(world: &mut World) {
    __pill_register_MeshRendererComponent(world);
}
