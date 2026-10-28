//! Bounded in-process log capture for the editor console.
//!
//! # Responsibilities
//!
//! - [`LogBuffer`]: a clonable handle to a bounded ring of owned log lines.
//! - [`CaptureLayer`]: a tracing layer that copies each event into the ring.
//!
//! # Design
//!
//! The ring is bounded by line count; the oldest lines drop first. Every
//! line carries a monotonically increasing sequence number, so a reader
//! fetches only what is new ([`LogBuffer::since`]) and never holds a lock
//! while formatting. Lines are owned strings: nothing borrowed from a
//! module's image ever enters the ring, which is what lets the editor shell
//! (a reloadable module itself) read lines emitted by other modules that may
//! have unloaded since.

// Standard library
use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex};

// External crates
use tracing::{Event, Level, Subscriber};
use tracing_log::NormalizeEvent;
use tracing_subscriber::layer::{Context, Layer};

// Current crate
use super::StyledFieldVisitor;

/// Lines a capture ring holds when nothing else is asked for.
pub const DEFAULT_CAPTURE_CAPACITY: usize = 4096;

/// One captured line, owned.
#[derive(Clone, Debug, PartialEq)]
pub struct LogLine {
    /// Monotonic sequence number, unique within the buffer.
    pub seq: u64,
    /// Severity, low to high: 1 trace, 2 debug, 3 info, 4 warn, 5 error.
    pub level: u8,
    /// The tracing target (the crate or module path).
    pub target: String,
    /// The formatted message, structured fields appended as `name=value`.
    pub message: String,
}

/// The severity ordering [`LogLine::level`] uses.
pub fn level_value(level: Level) -> u8 {
    match level {
        Level::TRACE => 1,
        Level::DEBUG => 2,
        Level::INFO => 3,
        Level::WARN => 4,
        Level::ERROR => 5,
    }
}

/// The ring itself, behind the shared handle.
struct Ring {
    capacity: usize,
    next_seq: u64,
    lines: VecDeque<LogLine>,
}

/// A clonable handle to a bounded ring of captured log lines.
#[derive(Clone)]
pub struct LogBuffer {
    inner: Arc<Mutex<Ring>>,
}

impl Default for LogBuffer {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_CAPTURE_CAPACITY)
    }
}

impl LogBuffer {
    /// A buffer that keeps at most `capacity` lines (at least one).
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(Ring {
                capacity: capacity.max(1),
                next_seq: 1,
                lines: VecDeque::new(),
            })),
        }
    }

    /// Take the ring, recovering it from a poison.
    ///
    /// A poisoned ring still holds every line written before the panic, and the
    /// console is the surface that has to show what went wrong - so a panic
    /// must not turn the panel permanently blank. Every accessor recovers the
    /// same way, which is also what the crate's other locks do.
    fn ring(&self) -> std::sync::MutexGuard<'_, Ring> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Append one line, dropping the oldest beyond the capacity.
    pub fn push(&self, level: u8, target: &str, message: String) {
        let mut ring = self.ring();
        let seq = ring.next_seq;
        ring.next_seq += 1;
        ring.lines.push_back(LogLine {
            seq,
            level,
            target: target.to_string(),
            message,
        });
        while ring.lines.len() > ring.capacity {
            ring.lines.pop_front();
        }
    }

    /// Every line with a sequence number greater than `after`, oldest first.
    pub fn since(&self, after: u64) -> Vec<LogLine> {
        let ring = self.ring();
        ring.lines
            .iter()
            .filter(|line| line.seq > after)
            .cloned()
            .collect()
    }

    /// The sequence number of the newest line (0 while empty).
    pub fn latest_seq(&self) -> u64 {
        self.ring().next_seq - 1
    }

    /// Lines currently held.
    pub fn len(&self) -> usize {
        self.ring().lines.len()
    }

    /// Whether the ring holds no lines.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Lines the ring can hold before dropping the oldest.
    pub fn capacity(&self) -> usize {
        self.ring().capacity
    }
}

impl fmt::Debug for LogBuffer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LogBuffer")
            .field("len", &self.len())
            .field("capacity", &self.capacity())
            .finish()
    }
}

/// A tracing layer that copies events at or above a level into a
/// [`LogBuffer`].
pub struct CaptureLayer {
    buffer: LogBuffer,
    minimum_level: u8,
}

impl CaptureLayer {
    /// A layer capturing events at or above `minimum_level`.
    pub fn new(buffer: LogBuffer, minimum_level: Level) -> Self {
        Self {
            buffer,
            minimum_level: level_value(minimum_level),
        }
    }
}

impl<S: Subscriber> Layer<S> for CaptureLayer {
    fn on_event(&self, event: &Event<'_>, _context: Context<'_, S>) {
        let metadata = event.metadata();
        let level = level_value(*metadata.level());
        if level < self.minimum_level {
            return;
        }
        let mut visitor = StyledFieldVisitor::plain(event.is_log());
        event.record(&mut visitor);
        self.buffer
            .push(level, metadata.target(), visitor.into_line());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A captured line is the formatter's line: same fields, same value
    /// spelling, bridged `log.*` fields dropped.
    ///
    /// The two lanes share one visitor, so this pins the shape they inherit
    /// rather than a second renderer's idea of it.
    #[test]
    fn a_captured_line_matches_the_formatter_line_shape() {
        use tracing_subscriber::layer::SubscriberExt;

        let buffer = LogBuffer::with_capacity(8);
        let subscriber =
            tracing_subscriber::registry().with(CaptureLayer::new(buffer.clone(), Level::INFO));
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(
                target: "engine::resources",
                module = "pill_spline",
                "texture created"
            );
        });

        let lines = buffer.since(0);
        assert_eq!(lines.len(), 1, "one event, one line");
        assert_eq!(
            lines[0].message, "texture created  module=\"pill_spline\"",
            "a string field keeps its `Debug` form, as the terminal lane shows it"
        );
    }

    /// A poisoned ring still serves the lines written before the panic.
    ///
    /// The console is the surface built to show what went wrong, so a panic
    /// elsewhere must not blank it for the rest of the session.
    #[test]
    fn a_poisoned_ring_still_returns_its_lines() {
        let buffer = LogBuffer::with_capacity(8);
        buffer.push(3, "test", "before the panic".to_string());

        // Poison the ring: a thread panics while holding the guard.
        let poisoned = buffer.clone();
        let _ = std::thread::spawn(move || {
            let _guard = poisoned.inner.lock().expect("the ring is unpoisoned here");
            panic!("a writer panicked while holding the guard");
        })
        .join();
        assert!(buffer.inner.is_poisoned(), "the ring is poisoned");

        assert_eq!(buffer.len(), 1);
        assert_eq!(buffer.latest_seq(), 1);
        assert_eq!(buffer.since(0).len(), 1);
        buffer.push(3, "test", "after the panic".to_string());
        assert_eq!(buffer.since(1).len(), 1, "capture continues after a poison");
    }

    #[test]
    fn the_ring_drops_oldest_and_tracks_sequence() {
        let buffer = LogBuffer::with_capacity(3);
        for index in 0..5u32 {
            buffer.push(3, "test", format!("line {index}"));
        }
        assert_eq!(buffer.len(), 3);
        let lines = buffer.since(0);
        assert_eq!(lines.len(), 3);
        assert_eq!(lines[0].message, "line 2");
        assert_eq!(lines[2].message, "line 4");
        assert_eq!(buffer.latest_seq(), 5);

        let new = buffer.since(3);
        assert_eq!(new.len(), 2);
        assert_eq!(new[0].message, "line 3");
    }

    #[test]
    fn since_returns_only_newer_lines() {
        let buffer = LogBuffer::with_capacity(8);
        buffer.push(4, "engine::ecs", "first".to_string());
        let watermark = buffer.latest_seq();
        buffer.push(5, "project", "second".to_string());
        let new = buffer.since(watermark);
        assert_eq!(new.len(), 1);
        assert_eq!(new[0].target, "project");
        assert_eq!(new[0].level, 5);
    }

    #[test]
    fn an_empty_buffer_reports_zero() {
        let buffer = LogBuffer::default();
        assert_eq!(buffer.latest_seq(), 0);
        assert!(buffer.since(0).is_empty());
        assert_eq!(buffer.capacity(), DEFAULT_CAPTURE_CAPACITY);
    }
}
