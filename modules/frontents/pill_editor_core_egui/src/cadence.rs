//! Redraw cadence (W-0038 Stage 9): when the editor owes a frame.
//!
//! # Responsibilities
//!
//! - Name every reason a frame is owed: input, a scripted command, a changed
//!   snapshot, a resize, something moving, or egui's own repaint request.
//! - Decide whether the loop draws at once or sleeps for the idle poll.
//! - Count the frames it drew, how many of them were demanded, and how often
//!   it waited.
//! - Read `PILL_EDITOR_CONTINUOUS_REDRAW`, the switch that restores the
//!   always-draw posture a profile or a suite wants.
//!
//! # Design
//!
//! The decision is plain data so it can be tested without an event loop: the
//! frontend gathers this frame's [`FrameDemand`] and asks a [`Cadence`] what
//! to do next. Nothing here touches a window, a clock or the world.

use std::time::Duration;

/// The environment variable that forces every frame.
pub const CONTINUOUS_REDRAW_ENV: &str = "PILL_EDITOR_CONTINUOUS_REDRAW";

/// How long an idle editor sleeps before it looks at the world again.
///
/// Nothing wakes a waiting loop when the *host* changes something - a notice,
/// a build finishing, a console line - because those arrive on the host's own
/// threads, so the idle state polls at this pace instead of sleeping until
/// input arrives. It is the Console tab's worst-case latency.
pub const IDLE_POLL: Duration = Duration::from_millis(250);

/// What asks for the next frame.
///
/// Each flag is set by whoever knows about it - the event handler for input, a
/// resize and scripted commands, the frame itself for the snapshot it
/// published and for egui's repaint request - and cleared once the frame that
/// honoured it has run.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct FrameDemand {
    /// A window event that is not a redraw arrived (pointer, keys, focus,
    /// wheel, a drop of files).
    pub input: bool,
    /// A scripted command arrived from the test channel.
    pub scripted: bool,
    /// The snapshot this frame published differs from the last one.
    pub snapshot: bool,
    /// The window's size or scale changed.
    pub resized: bool,
    /// Something is moving: the engine is not paused, a camera navigation or
    /// a scripted drag is in flight, or a toast is counting down.
    pub animating: bool,
    /// egui asked for a repaint at its own pace.
    pub repaint_requested: bool,
}

impl FrameDemand {
    /// Whether any reason to draw is set.
    pub fn any(&self) -> bool {
        self.input
            || self.scripted
            || self.snapshot
            || self.resized
            || self.animating
            || self.repaint_requested
    }

    /// Whether the frame must be drawn again with no delay.
    pub fn urgent(&self) -> bool {
        self.input || self.scripted || self.snapshot || self.resized
    }
}

/// When the loop draws next.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FrameWake {
    /// Draw as soon as the loop gets the turn.
    Immediate,
    /// Sleep this long first; input still wakes the loop sooner.
    After(Duration),
}

/// The editor's redraw counters and its mode.
#[derive(Clone, Debug)]
pub struct Cadence {
    continuous: bool,
    /// Frames drawn since startup.
    frames: u64,
    /// Frames drawn because something asked for them.
    demanded: u64,
    /// Times the loop went to sleep instead of drawing.
    waits: u64,
}

impl Cadence {
    /// A cadence with an explicit mode.
    pub fn new(continuous: bool) -> Self {
        Self {
            continuous,
            frames: 0,
            demanded: 0,
            waits: 0,
        }
    }

    /// A cadence the environment chose: continuous when the variable is set
    /// to anything but an empty string or `0`.
    pub fn from_environment() -> Self {
        let continuous = std::env::var(CONTINUOUS_REDRAW_ENV)
            .map(|value| !value.is_empty() && value != "0")
            .unwrap_or(false);
        Self::new(continuous)
    }

    /// Whether every frame is drawn regardless of demand.
    pub fn continuous(&self) -> bool {
        self.continuous
    }

    /// Frames drawn since startup.
    pub fn frames(&self) -> u64 {
        self.frames
    }

    /// Frames drawn because something demanded one.
    pub fn demanded(&self) -> u64 {
        self.demanded
    }

    /// How many times the loop slept between frames.
    pub fn waits(&self) -> u64 {
        self.waits
    }

    /// Count the frame the loop just drew.
    pub fn record_frame(&mut self, demand: FrameDemand) {
        self.frames += 1;
        if self.continuous || demand.any() {
            self.demanded += 1;
        }
    }

    /// Count a sleep the loop took.
    pub fn record_wait(&mut self) {
        self.waits += 1;
    }

    /// Decide when to draw next.
    ///
    /// A continuous cadence always draws at once. Otherwise the frame's own
    /// evidence decides: an urgent reason draws now, egui's repaint request
    /// draws when it asked to, and no reason at all sleeps for [`IDLE_POLL`].
    pub fn decide(&self, demand: FrameDemand) -> FrameWake {
        if self.continuous || demand.urgent() {
            FrameWake::Immediate
        } else if demand.repaint_requested {
            FrameWake::Immediate
        } else {
            FrameWake::After(IDLE_POLL)
        }
    }
}

impl Default for Cadence {
    fn default() -> Self {
        Self::new(false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_continuous_cadence_always_draws_at_once() {
        let cadence = Cadence::new(true);
        assert_eq!(cadence.decide(FrameDemand::default()), FrameWake::Immediate);
    }

    #[test]
    fn each_urgent_reason_draws_at_once() {
        let cadence = Cadence::new(false);
        for demand in [
            FrameDemand {
                input: true,
                ..FrameDemand::default()
            },
            FrameDemand {
                scripted: true,
                ..FrameDemand::default()
            },
            FrameDemand {
                snapshot: true,
                ..FrameDemand::default()
            },
            FrameDemand {
                resized: true,
                ..FrameDemand::default()
            },
        ] {
            assert!(demand.any());
            assert_eq!(cadence.decide(demand), FrameWake::Immediate);
        }
    }

    #[test]
    fn a_repaint_request_draws_at_once() {
        let cadence = Cadence::new(false);
        let demand = FrameDemand {
            repaint_requested: true,
            ..FrameDemand::default()
        };
        assert_eq!(cadence.decide(demand), FrameWake::Immediate);
    }

    #[test]
    fn an_idle_editor_sleeps_for_the_poll_interval() {
        let cadence = Cadence::new(false);
        assert_eq!(
            cadence.decide(FrameDemand::default()),
            FrameWake::After(IDLE_POLL)
        );
    }

    #[test]
    fn only_a_demand_makes_an_animation_reason_urgent() {
        let demand = FrameDemand {
            animating: true,
            ..FrameDemand::default()
        };
        assert!(demand.any());
        assert!(!demand.urgent());
        let cadence = Cadence::new(false);
        assert_eq!(cadence.decide(demand), FrameWake::After(IDLE_POLL));
    }

    #[test]
    fn the_counters_separate_demanded_frames_from_idle_ones() {
        let mut cadence = Cadence::new(false);
        cadence.record_frame(FrameDemand {
            input: true,
            ..FrameDemand::default()
        });
        cadence.record_frame(FrameDemand::default());
        cadence.record_wait();
        assert_eq!(cadence.frames(), 2);
        assert_eq!(cadence.demanded(), 1);
        assert_eq!(cadence.waits(), 1);
    }
}
