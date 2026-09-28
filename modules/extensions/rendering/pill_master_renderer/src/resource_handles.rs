//! The renderer's GPU resource handles.
//!
//! # Responsibilities
//!
//! - Name one handle type per GPU resource kind the renderer caches: material,
//!   mesh, camera, texture and shader.
//!
//! # Design
//!
//! Every handle is the same [`KeyData`](pill_core::slot_map::KeyData) wrapper,
//! differing only in name and in which slot map accepts it, so
//! [`define_slot_key!`](pill_core::define_slot_key) stamps out each type with
//! its accessors and its `SlotKey` impl. The arena those handles address is
//! generic and lives in [`pill_core::slot_map`]; only the resource names belong
//! to the renderer, which is why this module holds types and no storage.

pill_core::define_slot_key!(RendererMaterialHandle);
pill_core::define_slot_key!(RendererMeshHandle);
pill_core::define_slot_key!(RendererCameraHandle);
pill_core::define_slot_key!(RendererTextureHandle);
pill_core::define_slot_key!(RendererShaderHandle);
