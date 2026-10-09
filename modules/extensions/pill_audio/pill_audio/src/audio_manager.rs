//! The [`AudioManager`] resource: the output device and its sink pools.
//!
//! # Responsibilities
//!
//! - Owns the rodio output stream, which must outlive everything playing
//!   through it.
//! - Hands out and reclaims sinks from two fixed pools, one per
//!   [`SoundType`].
//!
//! # Design
//!
//! A resource, because there is exactly one audio device. The old engine
//! called this a "global component"; this engine has no such concept, and a
//! singleton is a resource.
//!
//! Sinks are pooled rather than created on demand: creating one allocates a
//! mixer channel, and doing that while a frame is being assembled is work on
//! a path that must not stall. The pool size is therefore also the ceiling on
//! concurrent voices.

// External crates
use pill_engine::Resource;

// Current crate
use crate::sound::Sound;
use crate::sound_type::SoundType;

// =============================================================================
// Constants
// =============================================================================

/// Sinks the manager creates for non-positional playback.
///
/// A sink is one voice, so this is the ceiling on concurrent 2D sounds. Fixed
/// at construction because rodio allocates a mixer channel per sink and a pool
/// that grows mid-frame would allocate on the audio path.
pub const DEFAULT_AMBIENT_SINK_COUNT: usize = 16;

/// Sinks the manager creates for positional playback.
pub const DEFAULT_SPATIAL_SINK_COUNT: usize = 16;

/// Half-distance between the listener's ears, in world units.
///
/// rodio derives its stereo panning from the distance between each ear and the
/// emitter, so this value is the whole of the panning strength.
pub const EAR_SEPARATION: f32 = 1.0;

// =============================================================================
// AudioManager
// =============================================================================

/// Owns the output stream and the pools of sinks that play through it.
///
/// A resource, because there is exactly one audio device. It must outlive
/// every sound playing through it: dropping the stream stops all audio, which
/// is why this is a resource and not a value a system constructs per frame.
///
/// Sinks are pooled rather than created on demand. Creating one allocates a
/// mixer channel, and doing that while a frame is being assembled is work on
/// the path that must not stall.
pub struct AudioManager {
    /// The output stream. Never read after construction, but dropping it
    /// silences every sink, so it is held for its lifetime alone.
    _stream: rodio::OutputStream,
    /// Handle used to build sinks against the stream.
    stream_handle: rodio::OutputStreamHandle,
    /// Non-positional sinks, indexed by the value in [`AudioSourceComponent::sink`].
    ambient: Vec<rodio::Sink>,
    /// Positional sinks, indexed the same way.
    spatial: Vec<rodio::SpatialSink>,
    /// Ambient sink indices not currently assigned to a source.
    free_ambient: Vec<u32>,
    /// Spatial sink indices not currently assigned to a source.
    free_spatial: Vec<u32>,
}

// SAFETY: every field is `Send` in rodio 0.20 except `OutputStream`, which is
// not because the underlying cpal stream is tied to the thread that created
// it on some backends. The engine only ever reaches this resource from a
// system, and the scheduler serialises `ResMut<AudioManager>` against every
// other accessor, so the value is touched by one thread at a time. What it
// must not do is *move* between threads while a sink plays - which the
// resource store never does: a resource is written once into world memory and
// read in place thereafter.
unsafe impl Send for AudioManager {}
// SAFETY: as above; shared access is serialised by the same analysis.
unsafe impl Sync for AudioManager {}

impl Resource for AudioManager {}

impl AudioManager {
    /// Open the default output device and build both sink pools.
    ///
    /// Returns `None` when no audio device is available - a headless CI
    /// runner, a container, a machine with audio disabled. That is an ordinary
    /// condition rather than an error: [`register`] simply does not install
    /// the resource, and [`audio_system`] does nothing without it.
    pub fn new(ambient_count: usize, spatial_count: usize) -> Option<Self> {
        let (stream, stream_handle) = rodio::OutputStream::try_default().ok()?;

        let mut ambient = Vec::with_capacity(ambient_count);
        for _ in 0..ambient_count {
            ambient.push(rodio::Sink::try_new(&stream_handle).ok()?);
        }

        let mut spatial = Vec::with_capacity(spatial_count);
        for _ in 0..spatial_count {
            spatial.push(
                rodio::SpatialSink::try_new(
                    &stream_handle,
                    [0.0, 0.0, 0.0],
                    [-EAR_SEPARATION, 0.0, 0.0],
                    [EAR_SEPARATION, 0.0, 0.0],
                )
                .ok()?,
            );
        }

        // Handed out from the back, so the pools drain in index order.
        let free_ambient = (0..ambient_count as u32).rev().collect();
        let free_spatial = (0..spatial_count as u32).rev().collect();

        Some(Self {
            _stream: stream,
            stream_handle,
            ambient,
            spatial,
            free_ambient,
            free_spatial,
        })
    }

    /// Open the default device with the default pool sizes.
    pub fn with_defaults() -> Option<Self> {
        Self::new(DEFAULT_AMBIENT_SINK_COUNT, DEFAULT_SPATIAL_SINK_COUNT)
    }

    /// Take a free sink index from the pool `sound_type` names.
    ///
    /// `None` when every sink of that kind is busy, which caps concurrent
    /// voices rather than allocating under load.
    pub fn acquire(&mut self, sound_type: SoundType) -> Option<u32> {
        match sound_type {
            SoundType::Ambient => self.free_ambient.pop(),
            SoundType::Spatial => self.free_spatial.pop(),
        }
    }

    /// Return a sink to its pool, stopping whatever it was playing.
    ///
    /// Stopping is what makes the sink reusable: a sink still holding a
    /// decoder would resume the previous sound when the next source appended
    /// to it. Returning an index twice is refused rather than corrupting the
    /// pool - a double free would hand one sink to two sources.
    pub fn release(&mut self, sound_type: SoundType, sink: u32) {
        let free = match sound_type {
            SoundType::Ambient => &mut self.free_ambient,
            SoundType::Spatial => &mut self.free_spatial,
        };
        if free.contains(&sink) {
            return;
        }
        match sound_type {
            SoundType::Ambient => {
                if let Some(handle) = self.ambient.get(sink as usize) {
                    handle.stop();
                }
            }
            SoundType::Spatial => {
                if let Some(handle) = self.spatial.get(sink as usize) {
                    handle.stop();
                }
            }
        }
        free.push(sink);
    }

    /// Whether the sink is still draining audio.
    pub fn is_playing(&self, sound_type: SoundType, sink: u32) -> bool {
        match sound_type {
            SoundType::Ambient => self
                .ambient
                .get(sink as usize)
                .is_some_and(|handle| !handle.empty() && !handle.is_paused()),
            SoundType::Spatial => self
                .spatial
                .get(sink as usize)
                .is_some_and(|handle| !handle.empty() && !handle.is_paused()),
        }
    }

    /// Whether the sink has drained everything appended to it.
    pub fn is_finished(&self, sound_type: SoundType, sink: u32) -> bool {
        match sound_type {
            SoundType::Ambient => self
                .ambient
                .get(sink as usize)
                .is_none_or(rodio::Sink::empty),
            SoundType::Spatial => self
                .spatial
                .get(sink as usize)
                .is_none_or(rodio::SpatialSink::empty),
        }
    }

    /// Queue `sound` on `sink` and begin playing at `volume`.
    pub(crate) fn start(
        &self,
        sound_type: SoundType,
        sink: u32,
        sound: &Sound,
        volume: f32,
    ) -> bool {
        let Some(decoder) = sound.decoder() else {
            return false;
        };
        match sound_type {
            SoundType::Ambient => {
                let Some(handle) = self.ambient.get(sink as usize) else {
                    return false;
                };
                handle.append(decoder);
                handle.set_volume(volume);
                handle.play();
            }
            SoundType::Spatial => {
                let Some(handle) = self.spatial.get(sink as usize) else {
                    return false;
                };
                handle.append(decoder);
                handle.set_volume(volume);
                handle.play();
            }
        }
        true
    }

    /// Halt a sink without releasing it.
    pub(crate) fn pause(&self, sound_type: SoundType, sink: u32) {
        match sound_type {
            SoundType::Ambient => {
                if let Some(handle) = self.ambient.get(sink as usize) {
                    handle.pause();
                }
            }
            SoundType::Spatial => {
                if let Some(handle) = self.spatial.get(sink as usize) {
                    handle.pause();
                }
            }
        }
    }

    /// Set a playing sink's volume.
    pub(crate) fn set_volume(&self, sound_type: SoundType, sink: u32, volume: f32) {
        match sound_type {
            SoundType::Ambient => {
                if let Some(handle) = self.ambient.get(sink as usize) {
                    handle.set_volume(volume);
                }
            }
            SoundType::Spatial => {
                if let Some(handle) = self.spatial.get(sink as usize) {
                    handle.set_volume(volume);
                }
            }
        }
    }

    /// Move a spatial sink's emitter to a world position.
    pub(crate) fn set_emitter(&self, sink: u32, position: [f32; 3]) {
        if let Some(handle) = self.spatial.get(sink as usize) {
            handle.set_emitter_position(position);
        }
    }

    /// Move every spatial sink's ears.
    ///
    /// The ears belong to the listener, not to a sound, so they are set on all
    /// sinks at once rather than per source.
    pub(crate) fn set_ears(&self, left: [f32; 3], right: [f32; 3]) {
        for handle in &self.spatial {
            handle.set_left_ear_position(left);
            handle.set_right_ear_position(right);
        }
    }

    /// The stream handle, for code building its own sinks.
    pub fn stream_handle(&self) -> &rodio::OutputStreamHandle {
        &self.stream_handle
    }

    /// How many sinks of each kind are unassigned, as `(ambient, spatial)`.
    pub fn free_counts(&self) -> (usize, usize) {
        (self.free_ambient.len(), self.free_spatial.len())
    }
}
