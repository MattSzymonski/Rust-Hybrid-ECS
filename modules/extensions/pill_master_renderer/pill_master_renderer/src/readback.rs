//! Mapping a GPU buffer back to the CPU without stalling a frame.
//!
//! # Responsibilities
//!
//! - Start a buffer's mapping once the frame that wrote it was submitted
//!   ([`MapState::start`]).
//! - Poll that mapping without waiting, hand the mapped bytes to a decoder, and
//!   report one of three outcomes ([`MapState::poll`]).
//! - Own a readback buffer together with its mapping for a user that has one
//!   ([`MappedReadback`]).
//!
//! # Design
//!
//! Reading a buffer back cannot block a frame, so the work is split: the copy
//! is recorded while the frame is built, [`MapState::start`] asks for the map
//! only after the submission (a buffer asked to map before its copy is
//! submitted makes the submission fail), and a later poll decodes whatever has
//! arrived. Neither a failed map nor bytes that cannot be read may leave a
//! caller waiting, so both answer [`Readback::Failed`] and the caller answers
//! its request with nothing.
//!
//! The state lives behind an [`Arc`] because the map callback runs on the
//! device's own thread while the frame path polls it.

// Standard library
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

// External crates
use wgpu::Buffer;

/// Where a buffer's mapping stands, as the callback reports it.
const MAPPING_PENDING: u8 = 0;
const MAPPING_DONE: u8 = 1;
const MAPPING_FAILED: u8 = 2;

/// What one [`MapState::poll`] found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Readback<T> {
    /// The GPU has not finished the copy yet: poll again later.
    Pending,
    /// The map succeeded and the bytes were decoded.
    Done(T),
    /// The map or the read failed: nothing will arrive for this one.
    Failed,
}

/// What the device's map callback reported.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MapOutcome {
    /// The map is still under way.
    Pending,
    /// The buffer is mapped and readable.
    Mapped,
    /// The device refused to map the buffer.
    Failed,
}

/// One buffer's mapping, shared with the callback that reports it.
pub(crate) struct MapState {
    state: Arc<AtomicU8>,
}

impl MapState {
    /// Asks for `buffer` to be mapped for reading.
    ///
    /// Only call this once the submission that wrote the buffer is in: a map
    /// asked for earlier makes that submission fail.
    pub(crate) fn start(buffer: &Buffer) -> Self {
        let state = Arc::new(AtomicU8::new(MAPPING_PENDING));
        let reported = Arc::clone(&state);
        buffer
            .slice(..)
            .map_async(wgpu::MapMode::Read, move |result| {
                let outcome = if result.is_ok() {
                    MAPPING_DONE
                } else {
                    MAPPING_FAILED
                };
                reported.store(outcome, Ordering::Release);
            });
        Self { state }
    }

    /// Whether the map has finished, whether it succeeded or failed.
    ///
    /// This is what a ring of readbacks tests before it reads: a slot is free
    /// once nothing it asked for is still mapping.
    pub(crate) fn finished(&self) -> bool {
        self.outcome() != MapOutcome::Pending
    }

    /// What the device's callback last reported.
    pub(crate) fn outcome(&self) -> MapOutcome {
        match self.state.load(Ordering::Acquire) {
            MAPPING_DONE => MapOutcome::Mapped,
            MAPPING_FAILED => MapOutcome::Failed,
            _ => MapOutcome::Pending,
        }
    }

    /// Polls the mapping without waiting.
    ///
    /// On success the bytes are handed to `read` once and the buffer is
    /// unmapped; a map the device refused answers [`Readback::Failed`] without
    /// ever touching the buffer.
    pub(crate) fn poll<T>(&self, buffer: &Buffer, read: impl FnOnce(&[u8]) -> T) -> Readback<T> {
        match self.outcome() {
            MapOutcome::Pending => Readback::Pending,
            MapOutcome::Failed => Readback::Failed,
            MapOutcome::Mapped => read_mapped(buffer, read),
        }
    }

    /// A state whose callback already reported `outcome`, for the tests that
    /// drive the failure branches without a device.
    #[cfg(test)]
    pub(crate) fn at_outcome(outcome: MapOutcome) -> Self {
        let value = match outcome {
            MapOutcome::Pending => MAPPING_PENDING,
            MapOutcome::Mapped => MAPPING_DONE,
            MapOutcome::Failed => MAPPING_FAILED,
        };
        Self {
            state: Arc::new(AtomicU8::new(value)),
        }
    }
}

/// Reads a mapped buffer through `read`, unmapping it either way.
///
/// A read the device refuses answers [`Readback::Failed`] rather than leaving
/// the buffer mapped, which would refuse every later use of it.
pub(crate) fn read_mapped<T>(buffer: &Buffer, read: impl FnOnce(&[u8]) -> T) -> Readback<T> {
    match buffer.slice(..).get_mapped_range() {
        Ok(bytes) => {
            let value = read(&bytes);
            drop(bytes);
            buffer.unmap();
            Readback::Done(value)
        }
        Err(_) => {
            buffer.unmap();
            Readback::Failed
        }
    }
}

/// A readback buffer that remembers its own mapping.
pub(crate) struct MappedReadback {
    buffer: Buffer,
    state: Option<MapState>,
}

impl MappedReadback {
    /// Wraps a buffer created with `MAP_READ`, with nothing mapped yet.
    pub(crate) fn new(buffer: Buffer) -> Self {
        Self {
            buffer,
            state: None,
        }
    }

    /// The buffer, for the copies a frame records into it.
    pub(crate) fn buffer(&self) -> &Buffer {
        &self.buffer
    }

    /// Asks for the map, unless one is already under way.
    pub(crate) fn start(&mut self) {
        if self.state.is_none() {
            self.state = Some(MapState::start(&self.buffer));
        }
    }

    /// Polls the map without waiting, decoding through `read` once.
    pub(crate) fn poll<T>(&mut self, read: impl FnOnce(&[u8]) -> T) -> Readback<T> {
        let Some(state) = self.state.as_ref() else {
            return Readback::Pending;
        };
        let outcome = state.poll(&self.buffer, read);
        if !matches!(outcome, Readback::Pending) {
            self.state = None;
        }
        outcome
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_failed_map_is_finished_and_answers_nothing() {
        let state = MapState::at_outcome(MapOutcome::Failed);

        assert!(state.finished(), "a failed map is finished, not pending");
        assert_eq!(state.outcome(), MapOutcome::Failed);
    }

    #[test]
    fn a_pending_map_is_not_finished() {
        let state = MapState::at_outcome(MapOutcome::Pending);

        assert!(!state.finished());
        assert_eq!(state.outcome(), MapOutcome::Pending);
    }

    #[test]
    fn a_mapped_readback_is_a_read_away_from_its_bytes() {
        let state = MapState::at_outcome(MapOutcome::Mapped);

        assert!(state.finished());
        assert_eq!(state.outcome(), MapOutcome::Mapped);
    }
}
