//! The tessellated output to `pill_draw`: meshes, clips and texture ids.
//!
//! # Responsibilities
//!
//! - [`convert`]: append egui's clipped primitives to a [`DrawList`] and
//!   return how many draws they became.
//! - [`external_id`] and [`straight_color`]: the two conversions the draw
//!   vocabulary needs - texture identity and colour space.
//!
//! # Design
//!
//! egui tessellates premultiplied sRGB colour into triangle lists in *points*,
//! while `pill_draw::Color` is straight alpha and `pill_ui_wgpu` blends
//! `SrcAlpha`, so the converter unpremultiplies per vertex and per texel (see
//! the W-0028 Stage 1 decision record, Q2). Vertices are not scaled: the
//! painter takes the monitor scale and applies it itself, which keeps one
//! `pixels_per_point` per frame (D8).
//!
//! The vertex's `local` and `shape` fields stay zeroed - egui emits its own
//! antialiasing - and the shader reads a zero radius and softness as "a plain
//! quad", so no rounded-corner work is undone twice.

// External crates
use egui::{ClippedPrimitive, Color32, TextureId as EguiTextureId};
use pill_core::math::Vector2f;
use pill_draw::{Color, DrawCommand, DrawList, Rect, TextureId, UiVertex};

/// The bit that separates egui's managed texture ids from its user ids.
///
/// `pill_draw::TextureId::Atlas` is reserved for the `pill_ui` glyph atlas, so
/// every egui texture arrives as an external and the two id spaces cannot
/// collide.
const USER_TEXTURE_CEILING: u64 = 1 << 63;

/// Append `primitives` to `list` and return how many draws it added.
///
/// Each primitive becomes one clipped draw: push the clip, the mesh's vertices
/// and indices, the draw naming the mesh's texture, pop. An empty mesh and a
/// paint callback both contribute nothing - a callback is a GPU closure, which
/// no neutral list can carry.
pub fn convert(primitives: &[ClippedPrimitive], list: &mut DrawList) -> usize {
    let mut draws = 0;
    for primitive in primitives {
        let egui::epaint::Primitive::Mesh(mesh) = &primitive.primitive else {
            continue;
        };
        if mesh.indices.is_empty() {
            continue;
        }
        let base = list.vertices.len() as u32;
        for vertex in &mesh.vertices {
            list.vertices.push(UiVertex {
                position: Vector2f::new(vertex.pos.x, vertex.pos.y),
                uv: Vector2f::new(vertex.uv.x, vertex.uv.y),
                color: straight_color(vertex.color),
                local: Vector2f::new(0.0, 0.0),
                shape: [0.0; 4],
            });
        }
        let first_index = list.indices.len() as u32;
        list.indices
            .extend(mesh.indices.iter().map(|index| index + base));
        list.push_clip(Rect {
            min: Vector2f::new(primitive.clip_rect.min.x, primitive.clip_rect.min.y),
            max: Vector2f::new(primitive.clip_rect.max.x, primitive.clip_rect.max.y),
        });
        list.commands.push(DrawCommand::Draw {
            texture: TextureId::External(external_id(mesh.texture_id)),
            first_index,
            index_count: mesh.indices.len() as u32,
        });
        list.pop_clip();
        draws += 1;
    }
    draws
}

/// egui's texture id as the number the painter registers it under.
pub fn external_id(texture: EguiTextureId) -> u64 {
    match texture {
        EguiTextureId::Managed(id) => USER_TEXTURE_CEILING | (id & !USER_TEXTURE_CEILING),
        EguiTextureId::User(id) => id & !USER_TEXTURE_CEILING,
    }
}

/// egui's premultiplied sRGB vertex colour as a straight-alpha one.
pub fn straight_color(color: Color32) -> Color {
    Color::from_rgba8(color.to_srgba_unmultiplied())
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::epaint::Mesh;

    /// A one-triangle mesh in egui's terms: three vertices, one texture.
    fn mesh(texture_id: EguiTextureId, color: Color32) -> ClippedPrimitive {
        let mut mesh = Mesh::with_texture(texture_id);
        for (x, y) in [(0.0, 0.0), (10.0, 0.0), (0.0, 10.0)] {
            mesh.vertices.push(egui::epaint::Vertex {
                pos: egui::pos2(x, y),
                uv: egui::pos2(0.0, 0.0),
                color,
            });
        }
        mesh.indices = vec![0, 1, 2];
        ClippedPrimitive {
            clip_rect: egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(10.0, 10.0)),
            primitive: egui::epaint::Primitive::Mesh(mesh),
        }
    }

    #[test]
    fn one_mesh_becomes_one_clipped_draw() {
        let mut list = DrawList::new();
        let draws = convert(
            &[mesh(EguiTextureId::Managed(0), Color32::WHITE)],
            &mut list,
        );
        assert_eq!(draws, 1);
        assert_eq!(list.vertices.len(), 3);
        assert_eq!(list.indices, vec![0, 1, 2]);
        assert_eq!(
            list.commands,
            vec![
                DrawCommand::PushClip(Rect {
                    min: Vector2f::new(0.0, 0.0),
                    max: Vector2f::new(10.0, 10.0),
                }),
                DrawCommand::Draw {
                    texture: TextureId::External(USER_TEXTURE_CEILING),
                    first_index: 0,
                    index_count: 3,
                },
                DrawCommand::PopClip,
            ]
        );
    }

    #[test]
    fn a_second_mesh_offsets_its_indices() {
        let mut list = DrawList::new();
        let primitives = [
            mesh(EguiTextureId::Managed(0), Color32::WHITE),
            mesh(EguiTextureId::User(5), Color32::WHITE),
        ];
        let draws = convert(&primitives, &mut list);
        assert_eq!(draws, 2);
        assert_eq!(list.vertices.len(), 6);
        assert_eq!(list.indices, vec![0, 1, 2, 3, 4, 5]);
        match &list.commands[4] {
            DrawCommand::Draw {
                texture,
                first_index,
                index_count,
            } => {
                assert_eq!(*texture, TextureId::External(5));
                assert_eq!(*first_index, 3);
                assert_eq!(*index_count, 3);
            }
            other => panic!("expected the second draw, got {other:?}"),
        }
    }

    #[test]
    fn premultiplied_vertex_colour_is_unpremultiplied() {
        // Half-transparent red in egui's premultiplied space.
        let color = Color32::from_rgba_premultiplied(128, 0, 0, 128);
        let straight = straight_color(color);
        assert!((straight.r - 1.0).abs() < 0.01, "red comes back at full");
        assert!((straight.a - 128.0 / 255.0).abs() < 0.01);
    }

    #[test]
    fn the_two_id_spaces_do_not_collide() {
        assert_eq!(external_id(EguiTextureId::Managed(0)), 1 << 63);
        assert_eq!(external_id(EguiTextureId::User(0)), 0);
        assert_eq!(external_id(EguiTextureId::User(7)), 7);
        assert_ne!(
            external_id(EguiTextureId::Managed(7)),
            external_id(EguiTextureId::User(7))
        );
    }

    #[test]
    fn an_empty_mesh_and_a_callback_add_nothing() {
        let mut empty = Mesh::with_texture(EguiTextureId::Managed(0));
        empty.vertices.clear();
        let primitives = [
            ClippedPrimitive {
                clip_rect: egui::Rect::NOTHING,
                primitive: egui::epaint::Primitive::Mesh(empty),
            },
            ClippedPrimitive {
                clip_rect: egui::Rect::NOTHING,
                primitive: egui::epaint::Primitive::Callback(egui::PaintCallback {
                    rect: egui::Rect::NOTHING,
                    callback: std::sync::Arc::new(()),
                }),
            },
        ];
        let mut list = DrawList::new();
        assert_eq!(convert(&primitives, &mut list), 0);
        assert!(list.is_empty());
    }
}
