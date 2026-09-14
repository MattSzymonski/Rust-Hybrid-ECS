//! Frame timing, maintained by the engine and readable from any system.
//!
//! # Responsibilities
//!
//! - Defines [`Time`], the engine-owned clock resource.
//! - Tracks elapsed time since project start, the previous frame's duration,
//!   and the delta driving this frame.
//!
//! # Design
//!
//! [`Time`] is inserted by [`Engine::new`](crate::engine::Engine::new) and
//! advanced by the engine itself at the top of every frame, before any system
//! runs. Systems only ever read it through `Res<Time>`.
//!
//! It is advanced directly by `process_frame` rather than by a registered
//! system, and that is deliberate. The scheduler builds its execution graph
//! independently of registration order - see
//! [`build_execution_graph`](crate::scheduler::SystemScheduler::build_execution_graph) -
//! so a "time system" could be batched after the systems that read it, handing
//! them values a frame stale. Advancing the clock outside the system set is the
//! only way to guarantee every system in a frame sees the same, current time.
//!
//! Durations are reported both as `f32` seconds, which is what gameplay
//! integration wants, and as whole milliseconds, which is what a HUD or a log
//! line wants without re-deriving it at each call site.

// Standard library
use std::time::{Duration, Instant};

// Current crate
use crate::resource::Resource;

// =============================================================================
// Constants
// =============================================================================

/// Largest delta the clock will report, in seconds.
///
/// A breakpoint, a hot reload, or a slow first frame can stretch real elapsed
/// time arbitrarily. Passing that through unclamped is how a physics step
/// teleports a body through a wall, so the delta saturates here instead. Real
/// elapsed time is unaffected: [`Time::elapsed`] keeps counting wall clock.
const MAXIMUM_DELTA_SECONDS: f32 = 0.1;

// =============================================================================
// Time
// =============================================================================

/// Frame timing for the running project.
///
/// Inserted automatically by the engine; a project never constructs one.
/// Read it from any system:
///
/// ```no_run
/// # use pill_engine::*;
/// fn movement(time: Res<Time>) {
///     let Some(time) = time.get() else { return };
///     let _step = 100.0 * time.delta_seconds();
/// }
/// ```
///
/// Every value describes the frame currently being processed, and stays fixed
/// for its whole duration, so two systems in one frame always agree.
pub struct Time {
    /// When the clock started, i.e. when the engine was created.
    startup: Instant,
    /// When the current frame began.
    frame_start: Instant,
    /// Wall-clock time from `startup` to the current frame's start.
    elapsed: Duration,
    /// Unclamped duration of the frame before this one.
    last_frame: Duration,
    /// Clamped `last_frame`, the value gameplay should integrate with.
    delta: Duration,
}

impl Resource for Time {}

impl Time {
    /// Start a clock whose origin is now.
    ///
    /// The first frame reports a zero delta: no frame has completed yet, so
    /// there is no honest duration to report and a fabricated one would show up
    /// as a visible jump on frame one.
    pub fn new() -> Self {
        let now = Instant::now();
        Self {
            startup: now,
            frame_start: now,
            elapsed: Duration::ZERO,
            last_frame: Duration::ZERO,
            delta: Duration::ZERO,
        }
    }

    /// Advance to a new frame, measuring the one that just ended.
    ///
    /// Called by the engine at the top of each frame, before systems run.
    pub(crate) fn advance(&mut self) {
        let now = Instant::now();
        self.last_frame = now.duration_since(self.frame_start);
        self.delta = Duration::from_secs_f32(
            self.last_frame.as_secs_f32().min(MAXIMUM_DELTA_SECONDS),
        );
        self.frame_start = now;
        self.elapsed = now.duration_since(self.startup);
    }

    // -------------------------------------------------------------------------
    // Elapsed since start
    // -------------------------------------------------------------------------

    /// Milliseconds elapsed since the project started.
    ///
    /// `u128` because a millisecond count from a long-running session outgrows
    /// `u64` only after ~584 million years, but `Duration::as_millis` returns
    /// `u128` and narrowing it here would be a silent truncation in the one
    /// place a session could be long.
    pub fn elapsed_milliseconds(&self) -> u128 {
        self.elapsed.as_millis()
    }

    /// Seconds elapsed since the project started.
    pub fn elapsed_seconds(&self) -> f32 {
        self.elapsed.as_secs_f32()
    }

    /// Time elapsed since the project started.
    pub fn elapsed(&self) -> Duration {
        self.elapsed
    }

    // -------------------------------------------------------------------------
    // Previous frame
    // -------------------------------------------------------------------------

    /// How long the previous frame took, in milliseconds.
    ///
    /// Unclamped, unlike [`Self::delta_seconds`]: this is a measurement, so a
    /// frame that really took 400 ms reports 400 ms. Use it for diagnostics
    /// and frame-time display, not for integrating motion.
    pub fn last_frame_milliseconds(&self) -> f32 {
        self.last_frame.as_secs_f32() * 1000.0
    }

    /// How long the previous frame took.
    pub fn last_frame(&self) -> Duration {
        self.last_frame
    }

    // -------------------------------------------------------------------------
    // Delta
    // -------------------------------------------------------------------------

    /// Seconds to advance simulation by this frame.
    ///
    /// The previous frame's duration, clamped to [`MAXIMUM_DELTA_SECONDS`] so a
    /// stalled frame cannot make a simulation step through geometry. This is
    /// the value gameplay should multiply by.
    pub fn delta_seconds(&self) -> f32 {
        self.delta.as_secs_f32()
    }

    /// Milliseconds to advance simulation by this frame.
    pub fn delta_milliseconds(&self) -> f32 {
        self.delta.as_secs_f32() * 1000.0
    }

    /// Duration to advance simulation by this frame.
    pub fn delta(&self) -> Duration {
        self.delta
    }
}

impl Default for Time {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for Time {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Time")
            .field("elapsed_ms", &self.elapsed_milliseconds())
            .field("last_frame_ms", &self.last_frame_milliseconds())
            .field("delta_ms", &self.delta_milliseconds())
            .finish()
    }
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    /// A fresh clock reports no elapsed time and no delta: nothing has run yet,
    /// so a non-zero first delta would be fabricated.
    #[test]
    fn a_new_clock_starts_at_zero() {
        let time = Time::new();

        assert_eq!(time.elapsed_milliseconds(), 0);
        assert_eq!(time.delta_seconds(), 0.0);
        assert_eq!(time.last_frame_milliseconds(), 0.0);
    }

    /// Advancing measures the frame that just ended and moves elapsed forward.
    #[test]
    fn advancing_measures_the_previous_frame() {
        let mut time = Time::new();
        std::thread::sleep(Duration::from_millis(12));
        time.advance();

        assert!(
            time.last_frame_milliseconds() >= 10.0,
            "a ~12ms frame should report at least 10ms, got {}",
            time.last_frame_milliseconds()
        );
        assert!(time.elapsed_milliseconds() >= 10);
        assert!(time.delta_seconds() > 0.0);
    }

    /// Elapsed time accumulates across frames rather than resetting.
    #[test]
    fn elapsed_accumulates_across_frames() {
        let mut time = Time::new();

        std::thread::sleep(Duration::from_millis(5));
        time.advance();
        let after_first = time.elapsed_milliseconds();

        std::thread::sleep(Duration::from_millis(5));
        time.advance();
        let after_second = time.elapsed_milliseconds();

        assert!(
            after_second > after_first,
            "elapsed must grow: {after_first} then {after_second}"
        );
    }

    /// A stalled frame is clamped for simulation but reported honestly for
    /// diagnostics - the distinction the two accessors exist to draw.
    #[test]
    fn delta_is_clamped_while_the_measurement_is_not() {
        let mut time = Time::new();
        // Longer than MAXIMUM_DELTA_SECONDS (100ms), as a breakpoint or reload
        // stall would be.
        std::thread::sleep(Duration::from_millis(150));
        time.advance();

        assert!(
            time.delta_seconds() <= MAXIMUM_DELTA_SECONDS,
            "delta must be clamped, got {}",
            time.delta_seconds()
        );
        assert!(
            time.last_frame_milliseconds() >= 140.0,
            "the raw measurement must not be clamped, got {}",
            time.last_frame_milliseconds()
        );
    }

    /// Seconds and millisecond accessors describe the same instant.
    #[test]
    fn second_and_millisecond_accessors_agree() {
        let mut time = Time::new();
        std::thread::sleep(Duration::from_millis(20));
        time.advance();

        let from_seconds = time.elapsed_seconds() * 1000.0;
        let from_millis = time.elapsed_milliseconds() as f32;
        assert!(
            (from_seconds - from_millis).abs() < 2.0,
            "{from_seconds} and {from_millis} should agree within rounding"
        );

        assert!((time.delta_milliseconds() - time.delta_seconds() * 1000.0).abs() < 0.001);
    }

    /// Values stay fixed between advances, so every system in one frame agrees.
    #[test]
    fn values_are_stable_within_a_frame() {
        let mut time = Time::new();
        std::thread::sleep(Duration::from_millis(5));
        time.advance();

        let elapsed = time.elapsed_milliseconds();
        let delta = time.delta_seconds();
        std::thread::sleep(Duration::from_millis(5));

        assert_eq!(time.elapsed_milliseconds(), elapsed);
        assert_eq!(time.delta_seconds(), delta);
    }
}
