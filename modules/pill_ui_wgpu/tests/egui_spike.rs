//! W-0028 Stage 1's spike: egui's tessellated output paints through the
//! existing `pill_ui` renderer.
//!
//! # Responsibilities
//!
//! - The conversion from `egui::ClippedPrimitive` to `pill_draw::DrawList`:
//!   premultiplied colours, clip rects, texture identity and texture uploads.
//! - One offscreen render asserting the result reaches pixels: text inside a
//!   clip is drawn, everything outside it is not.
//! - A PNG of that render, written for eyeballing (the test prints its path).
//!
//! # Design
//!
//! This is a spike, not an API: it lives beside the crate rather than in it,
//! so `pill_draw` and `pill_ui_wgpu` stay untouched until Stage 2 settles the
//! texture contract. Stage 3 moves the conversion into `pill_game_ui`.
//!
//! egui tessellates premultiplied sRGB colours (gamma space), while
//! `pill_draw::Color` is straight alpha and the pipeline blends
//! `SrcAlpha`/`OneMinusSrcAlpha`, so the converter unpremultiplies per vertex
//! and per texel. Both sides then multiply in linear light in the shader,
//! which is what the engine's UI shader does for `pill_ui` already.

// External crates
use egui::{
    epaint::Primitive, Color32, Context, ImageData, RawInput, Rect as EguiRect,
    TextureId as EguiTextureId,
};
use pill_ui::{Color, DrawCommand, DrawList, Rect, TextureId, UiVertex};
use pill_ui_wgpu::offscreen;
use pill_ui_wgpu::renderer::UiRenderer;

// Current crate
use pill_core::math::Vector2f;

/// The colour the target starts with, so "nothing was drawn" has a value the
/// test can recognise.
const BACKGROUND: Color = Color::rgba(0.05, 0.06, 0.09, 1.0);

/// The spike's target, in points and in physical pixels at scale 1.
const TARGET_SIZE: (u32, u32) = (320, 200);

/// The clip the spike's child draws under, in points: a band near the top
/// left, well away from the labelled text's second line.
const CLIP: Rect = Rect {
    min: Vector2f { x: 0.0, y: 0.0 },
    max: Vector2f { x: 300.0, y: 40.0 },
};

/// The texture the spike registers through egui's own API: a 2x2 checker, so
/// "more than one texture in one delta" is exercised rather than assumed.
const TEXTURE_SIZE: (u32, u32) = (2, 2);

#[test]
fn egui_tessellation_paints_through_the_ui_renderer() {
    let Some((device, queue)) = block_on(offscreen::request_device(true))
        .or_else(|| block_on(offscreen::request_device(false)))
    else {
        // No adapter: the golden harness reports the same condition, and a
        // machine without WARP/lavapipe cannot answer this question.
        eprintln!("no wgpu adapter is available; skipping the egui spike");
        return;
    };
    let context = Context::default();
    let mut renderer = UiRenderer::new(
        &device,
        &queue,
        wgpu::TextureFormat::Rgba8UnormSrgb,
        (64, 64),
    );

    // Step 1: one headless pass drawing text, a second texture and a clipped
    // child. The second texture is created first so it rides the same delta as
    // the font atlas.
    let checker = context.load_texture(
        "spike checker",
        checker_image(),
        egui::TextureOptions::NEAREST,
    );
    let output = context.run_ui(
        RawInput {
            screen_rect: Some(EguiRect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(TARGET_SIZE.0 as f32, TARGET_SIZE.1 as f32),
            )),
            ..Default::default()
        },
        |ui| {
            egui::CentralPanel::default()
                .frame(egui::Frame::NONE)
                .show(ui, |ui| {
                    ui.label("Pill engine UI spike");
                    ui.image((checker.id(), egui::vec2(8.0, 8.0)));
                    ui.scope(|ui| {
                        ui.set_clip_rect(EguiRect::from_min_max(
                            egui::pos2(CLIP.min.x, CLIP.min.y),
                            egui::pos2(CLIP.max.x, CLIP.max.y),
                        ));
                        // This line sits at y ≈ 40, half inside the clip: the
                        // pixels above the band survive, the rest are scissored.
                        ui.label("a label whose lower half falls outside the clip");
                    });
                });
        },
    );
    let mut textures = output.textures_delta;
    assert!(
        textures.set.len() >= 2,
        "the spike needs the font atlas and its own texture in one delta, got {}",
        textures.set.len()
    );

    // Step 2: upload every texture the pass carried under the mapping the
    // converter uses. `pill_draw::TextureId::Atlas` stays the `pill_ui`
    // glyph atlas; egui's textures all arrive as externals.
    for (texture_id, deltas) in &textures.set {
        for delta in deltas {
            let ImageData::Color(image) = &delta.image;
            assert!(
                delta.pos.is_none(),
                "the spike only handles whole-texture uploads"
            );
            let rgba = unpremultiplied_rgba8(&image.pixels);
            renderer.set_external(
                &device,
                &queue,
                external_id(*texture_id),
                (image.size[0] as u32, image.size[1] as u32),
                &rgba,
            );
        }
    }
    // Every delta is applied; epaint panics on one dropped unapplied.
    textures.clear();

    // Step 3: convert the tessellated primitives and paint them.
    let primitives = context.tessellate(output.shapes, output.pixels_per_point);
    let mut list = DrawList::new();
    let drawn = convert(&primitives, &mut list);
    assert!(drawn > 0, "the pass produced no paint commands");
    let pixels = block_on(offscreen::render_rgba(
        &device,
        &queue,
        &mut renderer,
        TARGET_SIZE,
        output.pixels_per_point,
        &list,
        Some(BACKGROUND),
    ))
    .expect("the spike renders");

    // Step 4: the assertions the stage's acceptance criteria name.
    let background = background_rgba();
    let painted = count_painted(&pixels, background);
    assert!(
        painted > 40,
        "expected glyph and image pixels, only {painted} of {} differ from the background",
        TARGET_SIZE.0 * TARGET_SIZE.1
    );
    // Everything below the clip band must be untouched: the converter has to
    // translate egui's scissor rather than drop it.
    let below_clip = count_painted_in_rows(&pixels, background, 60, TARGET_SIZE.1);
    assert_eq!(
        below_clip, 0,
        "the clip leaked: {below_clip} pixels were drawn below y=60"
    );

    let path = std::env::temp_dir().join("pill_egui_spike.png");
    write_png(&path, TARGET_SIZE.0, TARGET_SIZE.1, &pixels);
    println!(
        "egui spike: {painted} painted pixels, image at {}",
        path.display()
    );
}

// =============================================================================
// Conversion
// =============================================================================

/// Append `primitives` to `list` and return how many draws it added.
///
/// Each primitive becomes one clipped draw: push the clip, the mesh's
/// vertices and indices, the draw naming the mesh's texture, pop. egui's
/// meshes are already triangle lists in points, so only the colour space and
/// the texture identity change - `local` and `shape` stay zeroed, which the
/// shader reads as "a plain, unrounded quad".
fn convert(primitives: &[egui::ClippedPrimitive], list: &mut DrawList) -> usize {
    let mut draws = 0;
    for primitive in primitives {
        let Primitive::Mesh(mesh) = &primitive.primitive else {
            // A paint callback is a GPU closure: nothing a neutral list can
            // carry, and nothing the game path offers.
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

/// egui's texture id as the number the renderer registers it under.
///
/// The two id spaces are disjoint so a texture can never be mistaken for
/// another: `Managed` gets the high bit set and `User` does not, and
/// `pill_draw::TextureId::Atlas` (the `pill_ui` glyph atlas) is never
/// produced here at all.
fn external_id(texture: EguiTextureId) -> u64 {
    match texture {
        EguiTextureId::Managed(id) => USER_TEXTURE_CEILING | (id & !USER_TEXTURE_CEILING),
        EguiTextureId::User(id) => id & !USER_TEXTURE_CEILING,
    }
}

/// The bit that separates egui's managed ids from its user ids.
const USER_TEXTURE_CEILING: u64 = 1 << 63;

/// egui's premultiplied sRGB vertex colour as a straight-alpha one.
fn straight_color(color: Color32) -> Color {
    let [r, g, b, a] = color.to_srgba_unmultiplied();
    Color::from_rgba8([r, g, b, a])
}

/// egui's premultiplied sRGB pixels as straight-alpha RGBA8 the renderer's
/// `set_external` expects.
fn unpremultiplied_rgba8(pixels: &[Color32]) -> Vec<u8> {
    let mut rgba = Vec::with_capacity(pixels.len() * 4);
    for pixel in pixels {
        rgba.extend_from_slice(&pixel.to_srgba_unmultiplied());
    }
    rgba
}

// =============================================================================
// Assertions and the PNG
// =============================================================================

/// The clear colour as the RGBA8 the readback returns.
///
/// The target is sRGB, so the clear value the renderer computed in linear
/// light comes back gamma-encoded; the comparison runs in that space, one
/// step of slack per channel.
fn background_rgba() -> [u8; 4] {
    let linear = |value: f32| {
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    let encode = |value: f32| {
        let value = if value <= 0.003_130_8 {
            value * 12.92
        } else {
            1.055 * value.powf(1.0 / 2.4) - 0.055
        };
        (value.clamp(0.0, 1.0) * 255.0).round() as u8
    };
    [
        encode(linear(BACKGROUND.r)),
        encode(linear(BACKGROUND.g)),
        encode(linear(BACKGROUND.b)),
        255,
    ]
}

/// How many pixels differ from `background` by more than one channel step.
fn count_painted(pixels: &[u8], background: [u8; 4]) -> usize {
    pixels
        .chunks_exact(4)
        .filter(|pixel| differs(pixel, background))
        .count()
}

/// How many pixels from row `first` on differ from `background`.
fn count_painted_in_rows(pixels: &[u8], background: [u8; 4], first: u32, height: u32) -> usize {
    let row_start = (first.min(height) as usize) * TARGET_SIZE.0 as usize * 4;
    count_painted(&pixels[row_start..], background)
}

/// Whether two RGBA8 pixels differ by more than one step in a channel.
fn differs(pixel: &[u8], background: [u8; 4]) -> bool {
    pixel
        .iter()
        .zip(background.iter())
        .any(|(a, b)| a.abs_diff(*b) > 1)
}

/// A 2x2 image: two light and two dark texels, so the draw is visible whether
/// or not it is sampled where the test expects.
fn checker_image() -> egui::ColorImage {
    egui::ColorImage {
        size: [TEXTURE_SIZE.0 as usize, TEXTURE_SIZE.1 as usize],
        source_size: egui::vec2(TEXTURE_SIZE.0 as f32, TEXTURE_SIZE.1 as f32),
        pixels: vec![
            Color32::from_rgb(220, 40, 40),
            Color32::from_rgb(40, 220, 40),
            Color32::from_rgb(40, 40, 220),
            Color32::from_rgb(230, 230, 230),
        ],
    }
}

/// Write `pixels` as an RGBA8 PNG at `path`.
fn write_png(path: &std::path::Path, width: u32, height: u32, pixels: &[u8]) {
    let file = std::fs::File::create(path).expect("the spike's PNG is writable");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("the PNG header writes");
    writer
        .write_image_data(pixels)
        .expect("the PNG data writes");
}

/// Block on `future` with the platform's executor.
fn block_on<F: std::future::Future>(future: F) -> F::Output {
    pill_core::platform::futures::block_on(future)
}
