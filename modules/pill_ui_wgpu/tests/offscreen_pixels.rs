//! Offscreen pixel tests: a [`DrawList`] drawn by [`UiRenderer`] on the
//! software adapter, read back and checked pixel by pixel.
//!
//! # Responsibilities
//!
//! - Proves the renderer's core promises without a window or a GPU: text and
//!   clipped rectangles land where the list says, external textures sample,
//!   the monitor scale factor scales geometry, and a mid-grey survives the
//!   sRGB round trip at the value a correct pipeline produces.
//!
//! # Design
//!
//! Every test asks for the fallback adapter first (WARP on Windows, lavapipe
//! on Linux) and skips with a note when the backend offers none, so the
//! suite never fails for a missing driver. The target is `Rgba8UnormSrgb` -
//! the format the shader is written for - and the pixels that come back are
//! compared with a small tolerance for the software rasterizer's rounding.

// External crates
use pill_core::math::Vector2f;
use pill_core::platform::futures::block_on;
use pill_ui::{Atlas, Color, DrawList, Rect, TextStyle, TextSystem};
use pill_ui_wgpu::{offscreen, UiRenderer};

/// The per-channel slack for the software rasterizer's rounding.
const TOLERANCE: u8 = 4;

/// A device for the test, software first: `None` when no adapter exists and
/// the test should skip.
fn gpu() -> Option<(wgpu::Device, wgpu::Queue)> {
    block_on(offscreen::request_device(true)).or_else(|| block_on(offscreen::request_device(false)))
}

/// One tightly packed RGBA8 pixel from a readback.
fn pixel(rows: &[u8], width: u32, x: u32, y: u32) -> [u8; 4] {
    let start = ((y * width + x) * 4) as usize;
    [
        rows[start],
        rows[start + 1],
        rows[start + 2],
        rows[start + 3],
    ]
}

/// Assert a pixel equals `expected` within [`TOLERANCE`].
fn assert_pixel(context: &str, rows: &[u8], width: u32, x: u32, y: u32, expected: [u8; 4]) {
    let actual = pixel(rows, width, x, y);
    let matches = actual
        .iter()
        .zip(expected.iter())
        .all(|(actual, expected)| actual.abs_diff(*expected) <= TOLERANCE);
    assert!(
        matches,
        "{context}: pixel ({x}, {y}) is {actual:?}, expected {expected:?} within {TOLERANCE}"
    );
}

/// Render `list` at its natural scale and return the pixels.
fn render(
    device: &wgpu::Device,
    queue: &wgpu::Queue,
    size: (u32, u32),
    scale_factor: f32,
    list: &DrawList,
) -> Vec<u8> {
    let mut renderer = UiRenderer::new(
        device,
        queue,
        wgpu::TextureFormat::Rgba8UnormSrgb,
        (128, 128),
    );
    block_on(offscreen::render_rgba(
        device,
        queue,
        &mut renderer,
        size,
        scale_factor,
        list,
        Some(Color::TRANSPARENT),
    ))
    .expect("the offscreen readback succeeds")
}

#[test]
fn renders_text_clipped_rectangles_and_external_textures() {
    let Some((device, queue)) = gpu() else {
        eprintln!("skipping: the backend offers no adapter");
        return;
    };
    let mut list = DrawList::new();
    // A red rectangle the blue fill will be clipped against.
    list.push_fill(
        Rect::new(Vector2f::new(8.0, 8.0), Vector2f::new(40.0, 40.0)),
        Color::rgba(1.0, 0.0, 0.0, 1.0),
    );
    // A blue fill through a clip: visible to x = 24, red-only beyond it.
    list.push_clip(Rect::new(
        Vector2f::new(0.0, 0.0),
        Vector2f::new(24.0, 64.0),
    ));
    list.push_fill(
        Rect::new(Vector2f::new(0.0, 0.0), Vector2f::new(64.0, 48.0)),
        Color::rgba(0.0, 0.0, 1.0, 1.0),
    );
    list.pop_clip();

    // An external texture: four quadrants, sampled through the registered id.
    let mut texture = Vec::with_capacity(8 * 8 * 4);
    for y in 0..8 {
        for x in 0..8 {
            let quadrant = match (x < 4, y < 4) {
                (true, true) => [255, 0, 0, 255],
                (false, true) => [0, 255, 0, 255],
                (true, false) => [0, 0, 255, 255],
                (false, false) => [255, 255, 255, 255],
            };
            texture.extend_from_slice(&quadrant);
        }
    }
    list.push_quad(
        pill_ui::TextureId::External(7),
        Rect::new(Vector2f::new(48.0, 4.0), Vector2f::new(60.0, 16.0)),
        Vector2f::ZERO,
        Vector2f::new(1.0, 1.0),
        Color::WHITE,
    );

    // White text below the rectangles, through the real atlas machinery.
    let mut text = TextSystem::new();
    let mut atlas = Atlas::new(128, 128);
    text.draw_text(
        &mut list,
        &mut atlas,
        "Pill",
        Vector2f::new(4.0, 48.0),
        &TextStyle::default(),
        None,
        Color::WHITE,
    );

    let mut renderer = UiRenderer::new(
        &device,
        &queue,
        wgpu::TextureFormat::Rgba8UnormSrgb,
        (128, 128),
    );
    renderer.set_external(&device, &queue, 7, (8, 8), &texture);
    renderer.upload_atlas(&device, &queue, &mut atlas);
    let rows = block_on(offscreen::render_rgba(
        &device,
        &queue,
        &mut renderer,
        (64, 64),
        1.0,
        &list,
        Some(Color::TRANSPARENT),
    ))
    .expect("the offscreen readback succeeds");

    // Inside the clip the blue fill repainted the red: blue.
    assert_pixel("clipped blue", &rows, 64, 16, 16, [0, 0, 255, 255]);
    // Right of the clip the red rectangle shows: red.
    assert_pixel("red beyond the clip", &rows, 64, 32, 16, [255, 0, 0, 255]);
    // The clip's edge is exact at x = 24.
    assert_pixel("last blue column", &rows, 64, 23, 30, [0, 0, 255, 255]);
    assert_pixel("first red column", &rows, 64, 25, 30, [255, 0, 0, 255]);
    // The external texture's quadrants, away from filtering edges.
    assert_pixel("external top-left", &rows, 64, 50, 6, [255, 0, 0, 255]);
    assert_pixel("external top-right", &rows, 64, 57, 6, [0, 255, 0, 255]);
    assert_pixel("external bottom-left", &rows, 64, 50, 12, [0, 0, 255, 255]);
    assert_pixel(
        "external bottom-right",
        &rows,
        64,
        57,
        12,
        [255, 255, 255, 255],
    );
    // The clear is transparent wherever nothing drew.
    assert_pixel("untouched", &rows, 64, 2, 62, [0, 0, 0, 0]);

    // The text row holds glyph ink: enough white pixels to be letters.
    let mut ink = 0usize;
    for y in 48..64 {
        for x in 0..40 {
            let [r, g, b, a] = pixel(&rows, 64, x, y);
            if r > 180 && g > 180 && b > 180 && a > 180 {
                ink += 1;
            }
        }
    }
    assert!(ink >= 4, "the text drew {ink} ink pixels; expected letters");
}

#[test]
fn mid_grey_survives_the_srgb_round_trip() {
    let Some((device, queue)) = gpu() else {
        eprintln!("skipping: the backend offers no adapter");
        return;
    };
    let mut list = DrawList::new();
    list.push_fill(
        Rect::new(Vector2f::new(8.0, 8.0), Vector2f::new(24.0, 24.0)),
        Color::rgba(0.5, 0.5, 0.5, 1.0),
    );
    let rows = render(&device, &queue, (32, 32), 1.0, &list);
    // 0.5 in sRGB is 128 on the wire: the shader converts to linear light
    // once, the sRGB target encodes once - a wrong pipeline shows ~55 or
    // ~188 here instead.
    assert_pixel("mid grey", &rows, 32, 16, 16, [128, 128, 128, 255]);
    assert_pixel("outside", &rows, 32, 2, 2, [0, 0, 0, 0]);
}

#[test]
fn scale_factor_scales_geometry() {
    let Some((device, queue)) = gpu() else {
        eprintln!("skipping: the backend offers no adapter");
        return;
    };
    let mut list = DrawList::new();
    list.push_fill(
        Rect::new(Vector2f::new(10.0, 10.0), Vector2f::new(20.0, 20.0)),
        Color::rgba(1.0, 0.0, 0.0, 1.0),
    );
    let rows = render(&device, &queue, (64, 64), 2.0, &list);
    // Logical (10..20) at scale 2 is physical (20..40).
    assert_pixel("scaled fill", &rows, 64, 30, 30, [255, 0, 0, 255]);
    assert_pixel("before the fill", &rows, 64, 5, 5, [0, 0, 0, 0]);
    assert_pixel("after the fill", &rows, 64, 45, 45, [0, 0, 0, 0]);
}

#[test]
fn rounded_corners_and_soft_shadows_shade_the_shape() {
    let Some((device, queue)) = gpu() else {
        eprintln!("skipping: the backend offers no adapter");
        return;
    };
    // A rounded fill: the corner outside the arc stays clear, the middle is
    // the fill's colour.
    let mut list = DrawList::new();
    list.push_rounded_fill(
        Rect::new(Vector2f::new(8.0, 8.0), Vector2f::new(40.0, 40.0)),
        Color::rgba(0.0, 1.0, 0.0, 1.0),
        8.0,
    );
    let rows = render(&device, &queue, (48, 48), 1.0, &list);
    assert_pixel("rounded corner", &rows, 48, 8, 8, [0, 0, 0, 0]);
    let fringe = pixel(&rows, 48, 9, 9);
    assert!(
        fringe[3] > 0 && fringe[3] < 255,
        "the corner edge is antialiased: {fringe:?}"
    );
    assert_pixel("rounded middle", &rows, 48, 24, 24, [0, 255, 0, 255]);

    // A soft fill: nearly clear at the boundary, solid `softness` inside.
    let mut list = DrawList::new();
    list.push_soft_fill(
        Rect::new(Vector2f::new(8.0, 8.0), Vector2f::new(40.0, 40.0)),
        Color::rgba(0.0, 0.0, 0.0, 1.0),
        0.0,
        16.0,
    );
    let rows = render(&device, &queue, (48, 48), 1.0, &list);
    let edge = pixel(&rows, 48, 24, 9);
    let deep = pixel(&rows, 48, 24, 24);
    assert!(edge[3] < 40, "the boundary stays nearly clear: {edge:?}");
    assert!(deep[3] > 240, "the core is solid: {deep:?}");
    assert!(deep[3] > edge[3]);
}
