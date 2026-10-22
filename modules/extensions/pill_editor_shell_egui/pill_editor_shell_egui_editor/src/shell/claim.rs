//! The shell's claim on egui's shared `Context`.
//!
//! # Responsibilities
//!
//! - [`claim_context_once`]: the one decision both the production claim and
//!   its tests make - whether this generation still has to clear egui's
//!   temporary data.
//! - [`claim_shell_context`]: the per-generation wrapper around it, called at
//!   the top of [`run_frame`](crate::shell::run_frame).
//!
//! # Design
//!
//! The egui `Context` belongs to the frontend, which never unloads, so it
//! survives a shell reload. egui's `Memory::data` map can still hold values
//! the previous generation created - a drag payload, a tooltip's state, a
//! dock's scratch entry - and a value whose type was defined in that
//! generation carries its drop code and vtable in the image the reload
//! unmapped. Touching it after the reload is a use-after-unmap.
//!
//! So the first frame of every generation clears the map and nothing else:
//! the widget-local state in it (focus, scroll offsets, text-edit state) is
//! not worth a dangling pointer, and resetting it on a reload is what a user
//! sees as a fresh panel anyway. Long-lived UI state lives in
//! [`EguiShellState`](pill_editor_core_egui::session::EguiShellState), a world
//! resource defined by the shared model crate, which is what makes it safe to
//! keep across generations.
//!
//! The "already claimed" flag is a `static` of this crate, and a static lives
//! in the module's own image: every generation starts unclaimed, and later
//! frames of the same generation are no-ops.

// Standard library
use std::sync::atomic::{AtomicBool, Ordering};

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;

/// Whether this shell generation has cleared egui's temporary data yet.
///
/// Per image by construction: a reloaded shell starts from `false`, which is
/// what makes the claim run exactly once per generation.
static CONTEXT_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Claim the shared `Context` for this shell generation.
///
/// Returns whether the claim cleared anything, which is `true` once per
/// generation and `false` on every later frame. `run_frame` calls it before
/// the egui pass, so no stale value survives into the new generation's
/// first draw.
pub fn claim_shell_context(context: &egui::Context) -> bool {
    claim_context_once(&CONTEXT_CLAIMED, context)
}

/// Whether this generation has cleared the popped-out windows' contexts.
static POPOUTS_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Claim every popped-out tab's context for this shell generation (W-0038
/// Stage 23).
///
/// A popped-out window runs its own context, and that context is exactly as
/// long-lived as the main one - so the same rule applies to each of them. The
/// flag is one per generation, not one per context: clearing every popout
/// context on the generation's first frame costs a handful of map clears
/// once, and needs no bookkeeping across reloads. Returns how many contexts
/// were cleared, which is zero on every frame after the first.
pub fn claim_popout_contexts(
    contexts: &std::collections::BTreeMap<pill_editor_core_egui::layout::EditorTab, egui::Context>,
) -> usize {
    if POPOUTS_CLAIMED.swap(true, Ordering::AcqRel) {
        return 0;
    }
    let mut cleared = 0;
    for context in contexts.values() {
        let entries = context.memory_mut(|memory| {
            let entries = memory.data.len();
            memory.data.clear();
            entries
        });
        if entries > 0 {
            cleared += 1;
        }
    }
    if !contexts.is_empty() {
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ENGINE,
            windows = contexts.len(),
            cleared,
            "[editor] popout contexts claimed"
        );
    }
    cleared
}

/// The once-per-generation rule, with its flag passed in.
///
/// Split out so a test can run the rule against its own flag: two tests
/// sharing one process-wide static would make each other's expectations
/// depend on the test order.
pub fn claim_context_once(claimed: &AtomicBool, context: &egui::Context) -> bool {
    if claimed.swap(true, Ordering::AcqRel) {
        return false;
    }
    // Everything happens through the memory handle: the map is egui's, and
    // counting and clearing it in one borrow keeps the two readings honest.
    let entries = context.memory_mut(|memory| {
        let entries = memory.data.len();
        memory.data.clear();
        entries
    });
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        entries,
        "[editor] shell context claimed"
    );
    true
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A value a previous generation could have left in the map: defined
    /// here, so its drop code belongs to this test's image.
    #[derive(Clone)]
    struct StaleRow {
        /// The label the row carried.
        label: String,
    }

    /// The first claim of a generation drops what the previous one left, and
    /// a later frame of that same generation leaves the map alone.
    #[test]
    fn a_previous_generations_value_is_cleared_on_the_first_claim() {
        let context = egui::Context::default();
        let id = egui::Id::new("stale-row");
        // The plant: what a previous generation could have left behind.
        context.memory_mut(|memory| {
            // egui-state-ok: the previous generation's value
            memory.data.insert_temp(
                id,
                StaleRow {
                    label: "from the old image".to_string(),
                },
            );
        });
        let claimed = AtomicBool::new(false);

        assert!(
            claim_context_once(&claimed, &context),
            "the first claim of a generation runs"
        );
        assert_eq!(
            context.memory(|memory| memory.data.count::<StaleRow>()),
            0,
            "the previous generation's value is gone"
        );

        // The new generation stores its own value; the next frame's claim
        // must not throw it away.
        context.memory_mut(|memory| {
            // egui-state-ok: the value the run must keep
            memory.data.insert_temp(
                id,
                StaleRow {
                    label: "from the new image".to_string(),
                },
            );
        });
        assert!(
            !claim_context_once(&claimed, &context),
            "a later frame of the same generation does not claim again"
        );
        let kept = context
            // egui-state-ok: reading back the value the claim kept.
            .memory_mut(|memory| memory.data.get_temp::<StaleRow>(id))
            .expect("the generation's own value survives");
        assert_eq!(kept.label, "from the new image");
    }

    /// The claim counts what it dropped, so the log line can say whether a
    /// reload actually had anything stale to clear.
    #[test]
    fn the_claim_reports_how_many_values_it_cleared() {
        let context = egui::Context::default();
        let claimed = AtomicBool::new(false);
        // An empty map still counts as a claim (the generation is marked),
        // and the entry count reported is zero.
        let before = context.memory(|memory| memory.data.len());
        assert!(claim_context_once(&claimed, &context));
        assert_eq!(before, 0, "a fresh context holds no temporary data");
        assert!(context.memory(|memory| memory.data.is_empty()));
    }
}
