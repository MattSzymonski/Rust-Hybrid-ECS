//! The UI probe: a dev-only frontend that exercises the shared `pill_ui`
//! dylib without a project.
//!
//! # Responsibilities
//!
//! - [`requested`]: whether `PILL_UI_PROBE=1` asked for the probe.
//! - [`run`]: start a text system, shape and draw a sample into the draw
//!   list, and report fonts, glyphs, atlas regions and vertices through the
//!   log - the observable half of the `test_pill_ui.py` check that a dev
//!   host process loads exactly one `pill_ui.dll`.
//! - Style mode (`PILL_UI_PROBE_STYLE=<path>`): render the UI Gallery
//!   through a stylesheet, then re-read, re-parse and re-render it whenever
//!   the file changes for `PILL_UI_PROBE_STYLE_SECONDS` - the running proof
//!   that editing a stylesheet restyles a UI without a compile
//!   (`test_pill_ui_style.py`).
//!
//! # Design
//!
//! The probe runs before project resolution (see `main`): it needs no host,
//! project or window, only the library, so its test stays fast and
//! independent of which projects exist. It shapes with the bundled font, so
//! it behaves the same on a machine with no system fonts. After the `done`
//! token it holds the process open for a few seconds
//! (`PILL_UI_PROBE_HOLD_SECONDS`, default 5) so the test can inspect the
//! running process's loaded modules; `0` skips the hold.

// Standard library
use std::time::Duration;

// External crates
use pill_core::info;
use pill_core::math::Vector2f;
use pill_core::telemetry::telemetry_target;
use pill_ui::{paint, Atlas, Color, DrawList, Stylesheet, TextStyle, TextSystem, Theme, UiState};

// =============================================================================
// Constants
// =============================================================================

/// Environment variable that turns the probe on.
const UI_PROBE_ENVIRONMENT_VARIABLE: &str = "PILL_UI_PROBE";

/// Environment variable overriding the post-`done` hold, in seconds.
const UI_PROBE_HOLD_ENVIRONMENT_VARIABLE: &str = "PILL_UI_PROBE_HOLD_SECONDS";

/// Environment variable naming a stylesheet to render the gallery with.
const UI_PROBE_STYLE_ENVIRONMENT_VARIABLE: &str = "PILL_UI_PROBE_STYLE";

/// Environment variable bounding how long the style watch runs, in seconds.
const UI_PROBE_STYLE_SECONDS_ENVIRONMENT_VARIABLE: &str = "PILL_UI_PROBE_STYLE_SECONDS";

/// The hold applied when the environment does not override it: long enough
/// for the test to inspect the process, short enough not to annoy.
const DEFAULT_HOLD_SECONDS: f64 = 5.0;

/// How long the style watch runs when the environment does not say.
const DEFAULT_STYLE_SECONDS: f64 = 8.0;

/// How often the style watch checks the file's timestamp.
const STYLE_POLL: Duration = Duration::from_millis(100);

/// The sample drawn by the probe.
const SAMPLE_TEXT: &str = "The quick brown fox jumps over the lazy dog";

// =============================================================================
// Free Functions
// =============================================================================

/// Whether the environment asked for the probe.
pub fn requested() -> bool {
    std::env::var(UI_PROBE_ENVIRONMENT_VARIABLE).as_deref() == Ok("1")
}

/// Run the probe and report through the log; panics (and so exits non-zero)
/// when the library produced nothing drawable.
pub fn run() {
    let mut text = TextSystem::new();
    let mut atlas = Atlas::default();
    let mut list = DrawList::new();
    let style = TextStyle::default();
    let size = text.draw_text(
        &mut list,
        &mut atlas,
        SAMPLE_TEXT,
        Vector2f::new(4.0, 4.0),
        &style,
        Some(240.0),
        Color::WHITE,
    );
    let regions = atlas.take_updated_regions();
    let glyph_quads = list.vertices.len() / 4;

    info!(
        target: telemetry_target::UI,
        family = text.bundled_family(),
        "[ui probe] font"
    );
    info!(
        target: telemetry_target::UI,
        "[ui probe] shaped glyphs={} atlas-regions={} vertices={} size={}x{}",
        glyph_quads,
        regions.len(),
        list.vertices.len(),
        size.x,
        size.y
    );

    // The invariants the test waits on; a broken library exits non-zero.
    assert!(size.x > 0.0 && size.y > 0.0, "shaping produced no size");
    assert!(glyph_quads > 0, "drawing produced no glyph quads");
    assert!(!regions.is_empty(), "no atlas region was written");

    if let Ok(path) = std::env::var(UI_PROBE_STYLE_ENVIRONMENT_VARIABLE) {
        style_watch(&path);
    }

    info!(target: telemetry_target::UI, "[ui probe] done");

    let hold_seconds = std::env::var(UI_PROBE_HOLD_ENVIRONMENT_VARIABLE)
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(DEFAULT_HOLD_SECONDS);
    if hold_seconds > 0.0 {
        std::thread::sleep(Duration::from_secs_f64(hold_seconds));
    }
}

/// Render the gallery through the file at `path`, then re-render it every
/// time the file changes until the watch window closes.
fn style_watch(path: &str) {
    let seconds = std::env::var(UI_PROBE_STYLE_SECONDS_ENVIRONMENT_VARIABLE)
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(DEFAULT_STYLE_SECONDS);

    let mut state = UiState::new();
    let mut text = TextSystem::new();
    let mut theme_used = Theme::default();
    let mut atlas = Atlas::new(2048, 2048);

    let mut revision = 0u32;
    let render = |state: &mut UiState,
                  text: &mut TextSystem,
                  theme: &Theme,
                  atlas: &mut Atlas,
                  source: &str|
     -> (usize, usize, usize) {
        let mut sheet = Stylesheet::parse(source);
        state.input.begin_frame();
        let (output, layout) = pill_ui::layout::run_frame(
            state,
            text,
            theme,
            &mut sheet,
            Vector2f::new(440.0, 640.0),
            |ui| {
                pill_ui::gallery::ui_gallery(ui);
            },
        );
        let mut list = DrawList::new();
        paint(&output.tree, &layout, theme, state, text, atlas, &mut list);
        (
            sheet.rule_count(),
            sheet.reports().len(),
            list.vertices.len(),
        )
    };

    let Ok(mut source) = std::fs::read_to_string(path) else {
        info!(target: telemetry_target::UI, "[ui probe] style unreadable path={path}");
        return;
    };
    let (rules, reports, vertices) =
        render(&mut state, &mut text, &mut theme_used, &mut atlas, &source);
    info!(
        target: telemetry_target::UI,
        "[ui probe] styled revision={} rules={} reports={} vertices={}",
        revision,
        rules,
        reports,
        vertices
    );
    assert!(
        rules > 0 && vertices > 0,
        "the stylesheet produced no rules or nothing drew"
    );

    // The watch is bounded by sleep iterations (the workspace forbids
    // reading the wall clock outside the engine's time resources).
    let polls = (seconds / STYLE_POLL.as_secs_f64()).ceil() as u32;
    let mut stamp = file_stamp(path);
    for _ in 0..polls {
        std::thread::sleep(STYLE_POLL);
        let current = file_stamp(path);
        if current == stamp {
            continue;
        }
        stamp = current;
        let Ok(reloaded) = std::fs::read_to_string(path) else {
            continue;
        };
        source = reloaded;
        revision += 1;
        let (rules, reports, vertices) =
            render(&mut state, &mut text, &mut theme_used, &mut atlas, &source);
        info!(
            target: telemetry_target::UI,
            "[ui probe] restyled revision={} rules={} reports={} vertices={}",
            revision,
            rules,
            reports,
            vertices
        );
    }
}

/// The file's size and modified time, for change detection.
fn file_stamp(path: &str) -> Option<(u64, std::time::SystemTime)> {
    let metadata = std::fs::metadata(path).ok()?;
    Some((metadata.len(), metadata.modified().ok()?))
}
