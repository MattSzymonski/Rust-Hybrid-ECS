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
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::layer::{Context, Layer};

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

    /// Append one line, dropping the oldest beyond the capacity.
    pub fn push(&self, level: u8, target: &str, message: String) {
        let Ok(mut ring) = self.inner.lock() else {
            return;
        };
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
        let Ok(ring) = self.inner.lock() else {
            return Vec::new();
        };
        ring.lines
            .iter()
            .filter(|line| line.seq > after)
            .cloned()
            .collect()
    }

    /// The sequence number of the newest line (0 while empty).
    pub fn latest_seq(&self) -> u64 {
        match self.inner.lock() {
            Ok(ring) => ring.next_seq - 1,
            Err(_) => 0,
        }
    }

    /// Lines currently held.
    pub fn len(&self) -> usize {
        match self.inner.lock() {
            Ok(ring) => ring.lines.len(),
            Err(_) => 0,
        }
    }

    /// Whether the ring holds no lines.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Lines the ring can hold before dropping the oldest.
    pub fn capacity(&self) -> usize {
        match self.inner.lock() {
            Ok(ring) => ring.capacity,
            Err(_) => 0,
        }
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
        let mut visitor = MessageVisitor::default();
        event.record(&mut visitor);
        self.buffer.push(level, metadata.target(), visitor.finish());
    }
}

/// Renders one event into the console's line shape.
#[derive(Default)]
struct MessageVisitor {
    message: String,
    fields: String,
    has_message: bool,
}

impl MessageVisitor {
    fn finish(self) -> String {
        if self.fields.is_empty() {
            return self.message;
        }
        if !self.has_message {
            return self.fields;
        }
        format!("{}  {}", self.message, self.fields)
    }

    fn push_field(&mut self, field: &Field, rendered: String) {
        if !self.fields.is_empty() {
            self.fields.push(' ');
        }
        self.fields.push_str(field.name());
        self.fields.push('=');
        self.fields.push_str(&rendered);
    }
}

impl Visit for MessageVisitor {
    fn record_str(&mut self, field: &Field, value: &str) {
        if field.name() == "message" {
            self.message = value.to_string();
            self.has_message = true;
        } else {
            self.push_field(field, value.to_string());
        }
    }

    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.push_field(field, format!("{value:?}"));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
