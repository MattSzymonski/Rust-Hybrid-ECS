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
use pill_ui::{Atlas, Color, DrawList, TextStyle, TextSystem};

// =============================================================================
// Constants
// =============================================================================

/// Environment variable that turns the probe on.
const UI_PROBE_ENVIRONMENT_VARIABLE: &str = "PILL_UI_PROBE";

/// Environment variable overriding the post-`done` hold, in seconds.
const UI_PROBE_HOLD_ENVIRONMENT_VARIABLE: &str = "PILL_UI_PROBE_HOLD_SECONDS";

/// The hold applied when the environment does not override it: long enough
/// for the test to inspect the process, short enough not to annoy.
const DEFAULT_HOLD_SECONDS: f64 = 5.0;

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

    info!(target: telemetry_target::UI, "[ui probe] done");

    let hold_seconds = std::env::var(UI_PROBE_HOLD_ENVIRONMENT_VARIABLE)
        .ok()
        .and_then(|value| value.parse::<f64>().ok())
        .unwrap_or(DEFAULT_HOLD_SECONDS);
    if hold_seconds > 0.0 {
        std::thread::sleep(Duration::from_secs_f64(hold_seconds));
    }
}
