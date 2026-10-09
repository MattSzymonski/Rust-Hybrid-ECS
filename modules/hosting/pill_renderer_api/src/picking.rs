//! Selection outlines and picking: which entities a renderer outlines, and
//! which entity or gizmo handle is under a pixel.
//!
//! # Responsibilities
//!
//! - Name the entities to outline ([`SelectionOutlines`], a world resource).
//! - Ask what is under a pixel of a surface ([`PickRequests`]) and receive the
//!   answer frames later ([`PickResults`]), as plain data.
//!
//! # Design
//!
//! Picking reads a GPU buffer back, and a readback never stalls a frame: a
//! request made now is answered when the GPU has finished, a frame or more
//! later. Each request carries an id so its answer can be matched however late
//! it arrives. A gizmo handle drawn with [`pill_engine::DebugDraw::gizmo_line`]
//! or [`pill_engine::DebugDraw::gizmo_triangle`] is drawn over the scene in the
//! pick buffer too, so a handle in front of an entity is what a pick finds.

// External crates
use pill_engine::{Entity, Resource};

// Current crate
use crate::SurfaceId;

/// The entities a renderer outlines, in one colour.
#[derive(Debug, Clone)]
pub struct SelectionOutlines {
    /// The entities outlined; an entity that draws nothing outlines nothing.
    pub entities: Vec<Entity>,
    /// The outline colour, linear RGBA.
    pub color: [f32; 4],
}

impl Default for SelectionOutlines {
    fn default() -> Self {
        Self {
            entities: Vec::new(),
            color: [1.0, 0.55, 0.1, 1.0],
        }
    }
}

impl Resource for SelectionOutlines {
    fn shared_name() -> Option<&'static str> {
        Some("pill_renderer_api::picking::SelectionOutlines")
    }
}

/// One request to find what is under a pixel.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickRequest {
    /// The id its answer carries.
    pub id: u64,
    /// The surface the pixel belongs to.
    pub surface: SurfaceId,
    /// Physical pixels from the surface's left edge.
    pub x: u32,
    /// Physical pixels from the surface's top edge.
    pub y: u32,
}

/// What a pick found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PickHit {
    /// Nothing is drawn there, or the pixel is outside the surface.
    Nothing,
    /// The entity whose mesh is nearest at that pixel.
    Entity(Entity),
    /// The gizmo handle drawn there.
    Handle(u32),
}

/// The answer to one [`PickRequest`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PickResult {
    /// The request it answers.
    pub request: PickRequest,
    /// What is drawn there.
    pub hit: PickHit,
}

/// The picks asked for and not yet handed to a renderer.
///
/// A world resource: a frontend or an editor tool pushes requests, the frame
/// carries them to the renderer, and the answers arrive in [`PickResults`].
#[derive(Debug, Clone, Default)]
pub struct PickRequests {
    /// Requests waiting for the next frame.
    pub pending: Vec<PickRequest>,
    /// The id the next request gets.
    next_id: u64,
}

impl PickRequests {
    /// Ask what is under pixel (`x`, `y`) of `surface`; returns the id its
    /// answer will carry.
    pub fn request(&mut self, surface: SurfaceId, x: u32, y: u32) -> u64 {
        self.next_id += 1;
        let id = self.next_id;
        self.pending.push(PickRequest { id, surface, x, y });
        id
    }
}

impl Resource for PickRequests {
    fn shared_name() -> Option<&'static str> {
        Some("pill_renderer_api::picking::PickRequests")
    }
}

/// The answers renderers delivered and nobody took yet.
#[derive(Debug, Clone, Default)]
pub struct PickResults {
    /// Answers, oldest first.
    pub results: Vec<PickResult>,
}

impl PickResults {
    /// Take every answer delivered so far.
    pub fn take(&mut self) -> Vec<PickResult> {
        std::mem::take(&mut self.results)
    }
}

impl Resource for PickResults {
    fn shared_name() -> Option<&'static str> {
        Some("pill_renderer_api::picking::PickResults")
    }
}

/// What the pick buffer holds at a pixel: `0` for nothing, `1 + index` for the
/// frame's instance at `index`, and a gizmo handle with the top bit set.
pub const PICK_HANDLE_BIT: u32 = 0x8000_0000;

/// Decode one pick-buffer value against the instances' entities of the frame
/// it was drawn from.
pub fn decode_pick_value(value: u32, instance_entities: &[Entity]) -> PickHit {
    if value == 0 {
        PickHit::Nothing
    } else if value & PICK_HANDLE_BIT != 0 {
        PickHit::Handle(value & !PICK_HANDLE_BIT)
    } else {
        instance_entities
            .get(value as usize - 1)
            .map_or(PickHit::Nothing, |entity| PickHit::Entity(*entity))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Request ids count up, so answers can be matched.
    #[test]
    fn request_ids_are_unique() {
        let mut requests = PickRequests::default();
        let first = requests.request(SurfaceId::PRIMARY, 1, 2);
        let second = requests.request(SurfaceId(1), 3, 4);
        assert_ne!(first, second);
        assert_eq!(requests.pending.len(), 2);
    }

    /// Zero is nothing, the top bit a handle, anything else an instance.
    #[test]
    fn pick_values_decode() {
        let mut world = pill_engine::World::new();
        let entity = world.reserve_entity();
        assert_eq!(decode_pick_value(0, &[entity]), PickHit::Nothing);
        assert_eq!(decode_pick_value(1, &[entity]), PickHit::Entity(entity));
        assert_eq!(decode_pick_value(2, &[entity]), PickHit::Nothing);
        assert_eq!(
            decode_pick_value(PICK_HANDLE_BIT | 42, &[entity]),
            PickHit::Handle(42)
        );
    }
}
