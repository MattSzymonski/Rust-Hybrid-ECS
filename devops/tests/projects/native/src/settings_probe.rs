//! A manager whose tuning comes from the project's `settings/` directory.
//!
//! # Responsibilities
//!
//! - Declares a settings surface - a file stem, a complete default document and
//!   an applier - so the host's settings pipeline has a subject it can create a
//!   file for and apply.
//! - Prints the values it is asked to apply, and why it refused a document, so
//!   the manager-settings suite can see the live state through the host log.
//!
//! # Design
//!
//! The fixture's whole job is to be observable. Its applier keeps what it reads,
//! logs one line per apply, and refuses any key it does not know - a misspelled
//! key must be a refusal, not a silent no-op, which is the failure a person
//! cannot see.
//!
//! It is a shared resource, so the value survives a reload: the host then
//! re-applies the file to the regenerated manager, which is what the suite
//! checks after an unrelated source edit.

// Standard library
use std::sync::atomic::{AtomicU32, Ordering};

// External crates
use pill_engine::{Engine, Resource, SettingsDocument, SettingsOutcome};

// =============================================================================
// Constants
// =============================================================================

/// Printed by the applier, once per applied document.
pub const APPLIED_TOKEN: &str = "[test] fixture manager applied";

/// Printed by the applier when it refuses a document.
pub const REFUSED_TOKEN: &str = "[test] fixture manager refused";

/// The keys the applier understands.
const KNOWN_KEYS: [&str; 3] = ["enabled", "gain", "label"];

/// Counts applied documents in this process.
static APPLIED_COUNT: AtomicU32 = AtomicU32::new(0);

// =============================================================================
// FixtureManager
// =============================================================================

/// The fixture's manager: three knobs and an applier that reports them.
pub struct FixtureManager {
    /// A flag the document sets.
    pub enabled: bool,
    /// A number the document sets.
    pub gain: f32,
    /// A text value the document sets.
    pub label: String,
}

impl FixtureManager {
    /// The values [`ManagerResource::default_settings`] describes.
    fn new() -> Self {
        Self {
            enabled: true,
            gain: 1.0,
            label: "fixture".to_owned(),
        }
    }
}

impl Resource for FixtureManager {
    /// A shared name, so the manager keeps one world id across a reload. An id
    /// built from `TypeId` differs in every generation, and the value would be
    /// retired instead of kept.
    fn shared_name() -> Option<&'static str> {
        Some("native::settings_probe::FixtureManager")
    }
}

impl pill_engine::ManagerResource for FixtureManager {
    fn settings_name() -> &'static str {
        "fixture_manager"
    }

    /// Every key, with the value it takes when the project does not touch it.
    fn default_settings() -> &'static str {
        "# Fixture manager settings.\n\
         # Every key the manager reads, with the value it takes when untouched.\n\
         enabled: true\n\
         gain: 1.0\n\
         label: fixture\n"
    }

    fn apply_settings(&mut self, settings: &SettingsDocument) -> SettingsOutcome {
        let unknown_keys = settings.unknown_keys(&KNOWN_KEYS);
        if !unknown_keys.is_empty() {
            pill_engine::tracing::info!("{REFUSED_TOKEN} unknown_keys={unknown_keys:?}");
            return SettingsOutcome::Refused;
        }
        // A key the document leaves out keeps the value the manager has.
        if let Some(enabled) = settings.bool("enabled") {
            self.enabled = enabled;
        }
        if let Some(gain) = settings.float("gain") {
            self.gain = gain as f32;
        }
        if let Some(label) = settings.text("label") {
            self.label = label.to_owned();
        }
        let applied_count = APPLIED_COUNT.fetch_add(1, Ordering::Relaxed) + 1;
        pill_engine::tracing::info!(
            "{APPLIED_TOKEN} enabled={} gain={} label={} applied_total={applied_count}",
            self.enabled,
            self.gain,
            self.label
        );
        SettingsOutcome::Applied
    }
}

// =============================================================================
// Registration
// =============================================================================

/// Registers the fixture manager, keeping a live value across a reload.
///
/// The value is inserted only when the world has none, so a kept manager
/// survives a reload; the registration runs on every generation so the stored
/// settings applier always points at code that is still mapped.
pub fn register(engine: &mut Engine) {
    if engine.world().get_resource::<FixtureManager>().is_none() {
        engine
            .world_mut()
            .insert_manager_resource(FixtureManager::new());
    } else {
        engine
            .world_mut()
            .register_manager_resource::<FixtureManager>();
    }
}
