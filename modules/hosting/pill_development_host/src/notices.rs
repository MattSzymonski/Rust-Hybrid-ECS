//! Host activity notes for an attached editor frontend.
//!
//! # Responsibilities
//!
//! - Define [`HostNotice`]: what happened, in editor-agnostic terms.
//! - Queue notices from any `pill_development_host` module, capped and allocation-light.
//! - Hand the queue to the frontend's per-frame drain ([`take`]).
//!
//! # Design
//!
//! The host already reports reloads, patches, failures and rollbacks through
//! log lines. An editor frontend cannot read those, so the same events are
//! queued here as plain data and the frontend turns them into its own notice
//! resource. The queue is process-global for the same reason the analytics
//! collector is: the events are raised from `&mut DevHost` paths, from reload
//! reports and from entry points that hold no host reference, and the drain
//! wants them all in one place. A host with no editor attached never drains;
//! the cap keeps that bounded.
//!
//! A notice carries owned strings only. It is queued on the frame thread and
//! drained on the frame thread (the build runner's reader thread never writes
//! here), so no ordering machinery beyond the mutex is needed.

// Standard library
use std::collections::VecDeque;
use std::sync::Mutex;

/// One event the host reports to an attached editor frontend.
#[derive(Clone, Debug)]
pub enum HostNotice {
    /// A module's reload ended without a swap: its build failed, the new
    /// library could not load, or validation refused it. `output` is what can
    /// be shown about why - the captured compiler text when there was a
    /// build, the error otherwise.
    ModuleFailed { module: String, output: String },
    /// A module built, loaded, initialized and swapped in.
    Reloaded { module: String, build_ms: u64 },
    /// A `#[pill_hot]` function's patch went live.
    PatchApplied {
        /// The patched function, spelled as the classifier named it.
        function: String,
        /// The activation span the analytics report records.
        milliseconds: f64,
    },
    /// A function's patch attempt failed; the previous implementation still
    /// runs.
    PatchFailed {
        /// The function whose patch failed.
        function: String,
        /// The failure's detail text, as the patch pipeline reported it.
        detail: String,
    },
    /// A function was rolled back to an earlier generation, by request or
    /// automatically after repeated faults.
    RolledBack {
        /// The rolled-back function.
        function: String,
        /// The generation now executing.
        to_generation: u32,
        /// Whether the rollback happened without a developer asking for it.
        automatic: bool,
    },
}

/// How many notices wait for a drain before the oldest are dropped.
///
/// Generous enough that a burst (a reload plus its patches) arrives whole,
/// small enough that a host nobody drains stays cheap.
const NOTICE_CAPACITY: usize = 256;

/// The queue [`push`] fills and [`take`] empties.
static NOTICES: Mutex<VecDeque<HostNotice>> = Mutex::new(VecDeque::new());

/// Queue one notice for the frontend's next drain.
pub(crate) fn push(notice: HostNotice) {
    // One line per host event: the queue is bounded by real activity, and a
    // frontend that never drains it needs to be visible from the log alone.
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ENGINE,
        kind = kind_name(&notice),
        "[editor] host notice"
    );
    let mut queue = NOTICES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    if queue.len() >= NOTICE_CAPACITY {
        queue.pop_front();
    }
    queue.push_back(notice);
}

/// The log name of a notice's kind.
fn kind_name(notice: &HostNotice) -> &'static str {
    match notice {
        HostNotice::ModuleFailed { .. } => "module_failed",
        HostNotice::Reloaded { .. } => "reloaded",
        HostNotice::PatchApplied { .. } => "patch_applied",
        HostNotice::PatchFailed { .. } => "patch_failed",
        HostNotice::RolledBack { .. } => "rolled_back",
    }
}

/// Take every queued notice, oldest first.
///
/// The editor executable calls this once per frame and folds the result into
/// its notice resource; a host without an editor never calls it.
pub fn take() -> Vec<HostNotice> {
    let mut queue = NOTICES
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    queue.drain(..).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Serializes the two tests that share the process-global queue.
    ///
    /// `cargo test` runs them on parallel threads, so without this the
    /// capacity test's pushes land between the order test's two pushes and
    /// its drain, failing both. nextest gives every test its own process and
    /// never sees the race.
    static QUEUE_TEST_LOCK: Mutex<()> = Mutex::new(());

    /// The queue hands notices back in order and empties on take.
    #[test]
    fn notices_drain_in_order() {
        let _serialized = QUEUE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Whatever an earlier test left behind is not this test's business.
        let _ = take();
        push(HostNotice::Reloaded {
            module: "pill_spline".to_string(),
            build_ms: 120,
        });
        push(HostNotice::PatchApplied {
            function: "orbit".to_string(),
            milliseconds: 12.5,
        });
        let taken = take();
        assert_eq!(taken.len(), 2);
        assert!(
            matches!(&taken[0], HostNotice::Reloaded { module, .. } if module == "pill_spline")
        );
        assert!(
            matches!(&taken[1], HostNotice::PatchApplied { function, .. } if function == "orbit")
        );
        assert!(take().is_empty());
    }

    /// The queue drops the oldest notices past its cap, never the newest.
    #[test]
    fn the_cap_drops_oldest_first() {
        let _serialized = QUEUE_TEST_LOCK
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        // Start from a known-empty queue, for the same reason.
        let _ = take();
        for index in 0..NOTICE_CAPACITY + 4 {
            push(HostNotice::Reloaded {
                module: format!("module_{index}"),
                build_ms: 0,
            });
        }
        let taken = take();
        assert_eq!(taken.len(), NOTICE_CAPACITY);
        let Some(HostNotice::Reloaded { module, .. }) = taken.first() else {
            panic!("first notice is a reload");
        };
        assert_eq!(module, "module_4");
    }
}
