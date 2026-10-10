//! The golden-image harness (plan decision D26, level 2): render the widget
//! gallery and a sample panel offscreen and compare them to stored images.
//!
//! # Responsibilities
//!
//! - Renders every scene in [`scenes`] with [`UiRenderer`] on the software
//!   adapter, at the exact size the scene declares, through the Stage 10
//!   default stylesheet ([`pill_ui::DEFAULT_SHEET`]).
//! - Compares the pixels to `goldens/<scene>.png` with a per-channel
//!   tolerance and a small budget of tolerated outlier pixels; `--update`
//!   regenerates the stored images after an intended visual change.
//!
//! # Design
//!
//! The scenes are ordinary panel builders - later stages add their panels to
//! the list. The images live beside this crate and are committed; a change
//! that moves pixels without being intended fails the run, and the failure
//! message says how far off the render was. Everything about a scene is
//! deterministic: bundled font, software rasterization, fixed size, scale
//! factor 1.
//!
//! # Usage
//!
//! ```text
//! cd modules
//! cargo run -p pill_ui_wgpu --example golden              # compare (exit 1 on a difference)
//! cargo run -p pill_ui_wgpu --example golden -- --update  # regenerate after an intended change
//! ```

// Standard library
use std::path::Path;

// External crates
use pill_core::math::Vector2f;
use pill_core::platform::futures::block_on;
use pill_ui::{
    paint, run_frame, Atlas, Color, DrawList, PanelBuilder, Rect, Stylesheet, TextSystem, Theme,
    TreeNode, UiState,
};
use pill_ui_wgpu::{offscreen, UiRenderer};

// =============================================================================
// Constants
// =============================================================================

/// How far one channel may differ before the pixel counts as changed.
const CHANNEL_TOLERANCE: u8 = 8;

/// How many pixels may exceed the tolerance before the scene fails; software
/// rasterizers round differently at edges, a visual change moves thousands.
const MAX_DIFFERING_FRACTION: f64 = 0.001;

// =============================================================================
// Scenes
// =============================================================================

/// One golden-image scene: a name, a size in logical pixels, and the panel
/// builder that draws it.
struct Scene {
    name: &'static str,
    width: u32,
    height: u32,
    build: fn(&mut PanelBuilder<'_>),
}

/// Every scene, in comparison order.
fn scenes() -> Vec<Scene> {
    vec![
        Scene {
            name: "widgets",
            width: 440,
            height: 640,
            build: golden_widgets,
        },
        Scene {
            name: "panel",
            width: 360,
            height: 300,
            build: golden_panel,
        },
    ]
}

/// The widget gallery: every widget in every state, every theme token.
fn golden_widgets(ui: &mut PanelBuilder<'_>) {
    pill_ui::ui_gallery(ui);
}

/// A sample panel tree: the shape an editor panel takes - a heading, an
/// action row, disclosure sections with controls, and data widgets.
fn golden_panel(ui: &mut PanelBuilder<'_>) {
    ui.heading("Scene");
    ui.separator();
    ui.row(|ui| {
        ui.button("Save");
        ui.button("Load");
        ui.button_enabled("Add", false);
    });
    ui.collapsing("Camera", true, |ui| {
        ui.slider("Field of view", 0.6, 0.1..=1.2);
        ui.drag_value("Distance", 8.0, 0.1);
        ui.checkbox("Orthographic", false);
    });
    ui.collapsing("Entities", true, |ui| {
        ui.table(
            "panel table",
            &["Name", "Kind"],
            &[&["Sun", "light"], &["Ground", "mesh"], &["Player", "mesh"]],
        );
        ui.tree(
            "panel tree",
            &[
                TreeNode::with_children(
                    "World",
                    vec![TreeNode::new("Sun"), TreeNode::new("Ground")],
                ),
                TreeNode::new("UI"),
            ],
        );
    });
    let mut name = String::from("Player");
    ui.text_field("panel name", &mut name);
    ui.combo_box("Shading", 0, &["Lit", "Unlit", "Wireframe"]);
}

// =============================================================================
// Main
// =============================================================================

fn main() {
    let update = std::env::args().any(|argument| argument == "--update");
    let Some((device, queue)) = block_on(offscreen::request_device(true))
        .or_else(|| block_on(offscreen::request_device(false)))
    else {
        eprintln!("no wgpu adapter is available; install the software adapter (WARP, lavapipe)");
        std::process::exit(2);
    };

    let directory = Path::new(env!("CARGO_MANIFEST_DIR")).join("goldens");
    let mut failures = 0;
    let mut updated = 0;
    for scene in scenes() {
        let pixels = render_scene(&device, &queue, &scene);
        let path = directory.join(format!("{}.png", scene.name));
        if update {
            write_png(&path, scene.width, scene.height, &pixels);
            println!("updated {}", path.display());
            updated += 1;
            continue;
        }
        match compare_with_golden(&path, scene.width, scene.height, &pixels) {
            Ok(report) => println!("PASS {} {report}", scene.name),
            Err(message) => {
                println!("FAIL {} {message}", scene.name);
                failures += 1;
            }
        }
    }
    if update {
        println!("updated {updated} golden image(s); run again without --update to compare");
        return;
    }
    if failures > 0 {
        eprintln!("FAIL: {failures} scene(s) differ from the stored images");
        std::process::exit(1);
    }
    println!("PASS: every scene matches its golden image");
}

// =============================================================================
// Rendering
// =============================================================================

/// Render one scene through the default stylesheet and return its pixels.
fn render_scene(device: &wgpu::Device, queue: &wgpu::Queue, scene: &Scene) -> Vec<u8> {
    let mut state = UiState::new();
    let mut text = TextSystem::new();
    let theme = Theme::default();
    let mut sheet = Stylesheet::parse(pill_ui::DEFAULT_SHEET);
    let mut atlas = Atlas::new(2048, 2048);
    let mut renderer = UiRenderer::new(
        device,
        queue,
        wgpu::TextureFormat::Rgba8UnormSrgb,
        (2048, 2048),
    );

    state.input.begin_frame();
    let available = Vector2f::new(scene.width as f32, scene.height as f32);
    let (output, layout) = run_frame(
        &mut state,
        &mut text,
        &theme,
        &mut sheet,
        available,
        scene.build,
    );
    state.end_frame();
    let mut list = DrawList::new();
    // The window fill an embedder paints behind the panel (the theme's
    // window colour, the same value as `--bg-window` in the stylesheet), so
    // the image shows the approved whole-surface look.
    list.push_fill(Rect::from_min_size(Vector2f::ZERO, available), theme.window);
    paint(
        &output.tree,
        &layout,
        &theme,
        &mut state,
        &mut text,
        &mut atlas,
        &mut list,
    );
    renderer.upload_atlas(queue, &mut atlas);
    block_on(offscreen::render_rgba(
        device,
        queue,
        &mut renderer,
        (scene.width, scene.height),
        1.0,
        &list,
        Some(Color::TRANSPARENT),
    ))
    .expect("the offscreen readback succeeds")
}

// =============================================================================
// Golden comparison
// =============================================================================

/// Compare `pixels` to the stored image at `path`.
///
/// `Ok` carries the comparison report, `Err` why the scene fails.
fn compare_with_golden(
    path: &Path,
    width: u32,
    height: u32,
    pixels: &[u8],
) -> Result<String, String> {
    let (stored_width, stored_height, stored) = read_png(path)?;
    if (stored_width, stored_height) != (width, height) {
        return Err(format!(
            "stored image is {stored_width}x{stored_height}, the scene renders {width}x{height}; regenerate with --update"
        ));
    }
    let mut differing = 0usize;
    let mut worst = 0u8;
    for (rendered, stored) in pixels.chunks_exact(4).zip(stored.chunks_exact(4)) {
        let mut pixel_differs = false;
        for channel in 0..4 {
            let difference = rendered[channel].abs_diff(stored[channel]);
            worst = worst.max(difference);
            if difference > CHANNEL_TOLERANCE {
                pixel_differs = true;
            }
        }
        if pixel_differs {
            differing += 1;
        }
    }
    let total = (width * height) as usize;
    let allowed = (total as f64 * MAX_DIFFERING_FRACTION).ceil() as usize;
    if differing > allowed {
        return Err(format!(
            "{differing} of {total} pixels differ (worst channel delta {worst}); regenerate with --update if the change is intended"
        ));
    }
    Ok(format!(
        "({differing}/{total} pixels over tolerance, worst delta {worst})"
    ))
}

/// Write `rgba` as an 8-bit RGBA PNG, creating the directory on first use.
fn write_png(path: &Path, width: u32, height: u32, rgba: &[u8]) {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).expect("the goldens directory is creatable");
    }
    let file = std::fs::File::create(path).expect("the golden image is creatable");
    let mut encoder = png::Encoder::new(std::io::BufWriter::new(file), width, height);
    encoder.set_color(png::ColorType::Rgba);
    encoder.set_depth(png::BitDepth::Eight);
    let mut writer = encoder.write_header().expect("the PNG header is writable");
    writer
        .write_image_data(rgba)
        .expect("the PNG data is writable");
}

/// Read an 8-bit RGBA PNG back as tightly packed rows.
fn read_png(path: &Path) -> Result<(u32, u32, Vec<u8>), String> {
    let file = std::fs::File::open(path)
        .map_err(|error| format!("cannot open {}: {error}", path.display()))?;
    let decoder = png::Decoder::new(std::io::BufReader::new(file));
    let mut reader = decoder.read_info().map_err(|error| error.to_string())?;
    let mut buffer = vec![0; reader.output_buffer_size()];
    let info = reader
        .next_frame(&mut buffer)
        .map_err(|error| error.to_string())?;
    buffer.truncate(info.buffer_size());
    Ok((info.width, info.height, buffer))
}
