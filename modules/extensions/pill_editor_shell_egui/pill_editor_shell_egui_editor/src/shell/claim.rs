//! The shell's claim on egui's shared contexts.
//!
//! # Responsibilities
//!
//! - [`claim_shell_context`]: the per-generation wrapper `run_frame` calls
//!   before its pass, which buys the once-per-generation rule here and the
//!   clearing itself from the session that owns the contexts.
//! - [`claim_context_once`]: the rule on its own, for a caller holding one
//!   context.
//!
//! # Design
//!
//! The egui `Context` belongs to the frontend, which never unloads, so it
//! survives a shell reload. egui's `Memory::data` map can still hold values
//! the previous generation created - a drag payload, a tooltip's state, a
//! dock's scratch entry - and a value whose type that generation defined
//! carries its drop code and vtable in the image the reload unmapped, so
//! touching it after the reload is a use-after-unmap.
//!
//! The first frame of every generation therefore clears every context the
//! session owns, main and popped-out alike
//! ([`EguiShellState::claim_generation`](pill_editor_core::session::EguiShellState::claim_generation)):
//! widget-local state (focus, scroll offsets, text-edit state) is not worth a
//! dangling pointer. Long-lived UI state lives in the session itself, a world
//! resource the shared model crate defines, which is what makes it safe across
//! generations.
//!
//! The "already claimed" flag is a `static` of this crate, and a static lives
//! in the module's own image: every generation starts unclaimed, and later
//! frames of the same generation are no-ops. That is also why the flag cannot
//! move to the model with the clearing: the model outlives the generation
//! whose claim it would be.

// Standard library
use std::sync::atomic::{AtomicBool, Ordering};

// Current crate
use pill_editor_shell_egui_editor::shell::prelude::*;
// Deliberately unused in this compilation: a patch crate replays this
// file's opening, and the first sibling this body names resolves through it.
#[allow(unused_imports)]
use pill_editor_shell_egui_editor::shell::*;

/// Whether this shell generation has cleared egui's temporary data yet.
///
/// Per image by construction: a reloaded shell starts from `false`, which is
/// what makes the claim run exactly once per generation.
static CONTEXT_CLAIMED: AtomicBool = AtomicBool::new(false);

/// Claim every egui context the session owns for this shell generation.
///
/// `run_frame` calls it before the egui pass, so no stale value survives into
/// the new generation's first draw. Answers what was cleared on the
/// generation's first frame and `None` on every later one.
pub fn claim_shell_context(
    session: &mut pill_editor_core::session::EguiShellState,
) -> Option<pill_editor_core::session::ContextClaim> {
    if CONTEXT_CLAIMED.swap(true, Ordering::AcqRel) {
        return None;
    }
    let claim = session.claim_generation();
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        entries = claim.main_entries,
        windows = session.popout_contexts.len(),
        cleared = claim.popout_windows,
        "[editor] shell context claimed"
    );
    Some(claim)
}

/// The once-per-generation rule, with its flag passed in.
///
/// The flag is a parameter so a test can run the rule against its own: two
/// tests sharing one process-wide static would make each other's expectations
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

    /// A fresh context holds nothing to clear, and the claim still marks the
    /// generation.
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
