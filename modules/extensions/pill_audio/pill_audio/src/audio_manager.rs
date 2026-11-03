//! The [`AudioManager`] resource: the audio output device and its sink pools.
//!
//! # Responsibilities
//!
//! - Owns the rodio output stream. The stream must stay open while any sound
//!   plays through it, because dropping it stops every sound.
//! - Hands out a sink from one of two fixed pools, one pool per [`SoundType`],
//!   and takes it back when the sound using it ends.
//!
//! # Design
//!
//! There is exactly one audio device. A value a system builds per frame would
//! be dropped at the end of that frame, which silences the sound it just
//! started, so the device lives in a resource instead.
//!
//! The sinks are pooled instead of created on demand. Creating one makes rodio
//! allocate a mixer channel, and doing that while a frame is being built would
//! put an allocation on the audio path, which must not stall. The size of a
//! pool is therefore also the limit on how many voices play at once.

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
/// One sink is one voice, so this is also the limit on how many non-positional
/// sounds play at once. The pool is built once, in [`AudioManager::new`],
/// because rodio allocates a mixer channel per sink and a pool that grew
/// mid-frame would allocate on the audio path.
pub const DEFAULT_AMBIENT_SINK_COUNT: usize = 16;

/// Sinks the manager creates for positional playback, and so the limit on how
/// many positional sounds play at once.
pub const DEFAULT_SPATIAL_SINK_COUNT: usize = 16;

/// Half the distance between the listener's ears, in world units.
///
/// rodio pans a spatial sink by the distance between each ear and the emitter,
/// so this value decides how strong the stereo effect is.
pub const EAR_SEPARATION: f32 = 1.0;

// =============================================================================
// AudioManager
// =============================================================================

/// Owns the output stream and the pooled sinks that play through it.
///
/// The manager must outlive every sound playing through it, because dropping
/// the stream stops all audio. It is a resource for that reason, and because
/// there is exactly one audio device.
pub struct AudioManager {
    /// The output stream. Nothing reads it after construction, but dropping it
    /// silences every sink, so it is held for its lifetime alone.
    _stream: rodio::OutputStream,
    /// Handle used to build sinks against the stream. Also handed out by
    /// [`AudioManager::stream_handle`].
    stream_handle: rodio::OutputStreamHandle,
    /// Non-positional sinks, indexed by the sink index a source stores in
    /// [`AudioSourceComponent::sink`].
    ambient_sinks: Vec<rodio::Sink>,
    /// Positional sinks, indexed the same way.
    spatial_sinks: Vec<rodio::SpatialSink>,
    /// Indices of the ambient sinks that no source holds.
    free_ambient_sink_indices: Vec<u32>,
    /// Indices of the spatial sinks that no source holds.
    free_spatial_sink_indices: Vec<u32>,
}

// SAFETY: every field is `Send` in rodio 0.20 except `OutputStream`, which is
// not, because the cpal stream inside it belongs to the thread that created it
// on some platforms. The engine only reaches this resource from a system, and
// the scheduler serialises `ResMut<AudioManager>` against every other
// accessor, so one thread touches the value at a time. It must also not move
// between threads while a sink plays, and the resource store never moves it: a
// resource is written once into world memory and read in place from then on.
unsafe impl Send for AudioManager {}
// SAFETY: `Sync` needs nothing beyond what `Send` above argues - the scheduler
// hands out one accessor at a time, so two threads never share the value.
unsafe impl Sync for AudioManager {}

impl Resource for AudioManager {}

impl AudioManager {
    /// Opens the default output device and builds both sink pools.
    ///
    /// Returns `None` when there is no audio device: a headless CI runner, a
    /// container, a machine with audio turned off. That is an ordinary
    /// condition rather than an error. [`crate::register`] then installs no
    /// manager, and [`crate::audio_system`] only drains queued sound loads.
    pub fn new(ambient_sink_count: usize, spatial_sink_count: usize) -> Option<Self> {
        // Step 1: Open the default output device.
        let (stream, stream_handle) = rodio::OutputStream::try_default().ok()?;

        // Step 2: Build one sink per voice in each pool.
        let mut ambient_sinks = Vec::with_capacity(ambient_sink_count);
        for _ in 0..ambient_sink_count {
            ambient_sinks.push(rodio::Sink::try_new(&stream_handle).ok()?);
        }

        let mut spatial_sinks = Vec::with_capacity(spatial_sink_count);
        for _ in 0..spatial_sink_count {
            spatial_sinks.push(
                rodio::SpatialSink::try_new(
                    &stream_handle,
                    [0.0, 0.0, 0.0],
                    [-EAR_SEPARATION, 0.0, 0.0],
                    [EAR_SEPARATION, 0.0, 0.0],
                )
                .ok()?,
            );
        }

        // Step 3: Fill the free lists back to front, so `pop` hands out index
        // 0 first and the pools run in index order.
        let free_ambient_sink_indices = (0..ambient_sink_count as u32).rev().collect();
        let free_spatial_sink_indices = (0..spatial_sink_count as u32).rev().collect();

        Some(Self {
            _stream: stream,
            stream_handle,
            ambient_sinks,
            spatial_sinks,
            free_ambient_sink_indices,
            free_spatial_sink_indices,
        })
    }

    /// Opens the default device with the default pool sizes.
    pub fn with_defaults() -> Option<Self> {
        Self::new(DEFAULT_AMBIENT_SINK_COUNT, DEFAULT_SPATIAL_SINK_COUNT)
    }

    /// Takes a free sink index from the pool that `sound_type` names.
    ///
    /// Returns `None` when every sink of that kind is busy, which limits the
    /// number of voices instead of allocating more under load.
    pub fn acquire(&mut self, sound_type: SoundType) -> Option<u32> {
        match sound_type {
            SoundType::Ambient => self.free_ambient_sink_indices.pop(),
            SoundType::Spatial => self.free_spatial_sink_indices.pop(),
        }
    }

    /// Returns a sink to its pool, stopping whatever it was playing.
    ///
    /// Stopping is what makes the sink reusable. rodio queues a new source
    /// behind whatever the sink still holds, so leftover audio from the last
    /// sound would play first; a stopped sink ends its current sound and drops
    /// the queued ones within a few milliseconds. Handing back an index the
    /// pool already holds is refused, because a double free would give one
    /// sink to two sources.
    pub fn release(&mut self, sound_type: SoundType, sink_index: u32) {
        let unassigned_sink_indices = match sound_type {
            SoundType::Ambient => &mut self.free_ambient_sink_indices,
            SoundType::Spatial => &mut self.free_spatial_sink_indices,
        };
        if unassigned_sink_indices.contains(&sink_index) {
            return;
        }
        match sound_type {
            SoundType::Ambient => {
                if let Some(pooled_sink) = self.ambient_sinks.get(sink_index as usize) {
                    pooled_sink.stop();
                }
            }
            SoundType::Spatial => {
                if let Some(pooled_sink) = self.spatial_sinks.get(sink_index as usize) {
                    pooled_sink.stop();
                }
            }
        }
        unassigned_sink_indices.push(sink_index);
    }

    /// Returns whether the sink has audio queued and is not paused.
    ///
    /// An index with no sink of that kind reports `false`.
    pub fn is_playing(&self, sound_type: SoundType, sink_index: u32) -> bool {
        match sound_type {
            SoundType::Ambient => self
                .ambient_sinks
                .get(sink_index as usize)
                .is_some_and(|pooled_sink| !pooled_sink.empty() && !pooled_sink.is_paused()),
            SoundType::Spatial => self
                .spatial_sinks
                .get(sink_index as usize)
                .is_some_and(|pooled_sink| !pooled_sink.empty() && !pooled_sink.is_paused()),
        }
    }

    /// Returns whether the sink has drained everything appended to it.
    ///
    /// An index with no sink of that kind counts as finished.
    pub fn is_finished(&self, sound_type: SoundType, sink_index: u32) -> bool {
        match sound_type {
            SoundType::Ambient => self
                .ambient_sinks
                .get(sink_index as usize)
                .is_none_or(rodio::Sink::empty),
            SoundType::Spatial => self
                .spatial_sinks
                .get(sink_index as usize)
                .is_none_or(rodio::SpatialSink::empty),
        }
    }

    /// Queues `sound` on `sink_index` and starts playing it at `volume`.
    ///
    /// Returns `false` and leaves the sink untouched when the bytes cannot be
    /// decoded or when `sink_index` names no sink of that kind.
    pub(crate) fn start(
        &self,
        sound_type: SoundType,
        sink_index: u32,
        sound: &Sound,
        volume: f32,
    ) -> bool {
        let Some(decoder) = sound.decoder() else {
            return false;
        };
        match sound_type {
            SoundType::Ambient => {
                let Some(pooled_sink) = self.ambient_sinks.get(sink_index as usize) else {
                    return false;
                };
                pooled_sink.append(decoder);
                pooled_sink.set_volume(volume);
                pooled_sink.play();
            }
            SoundType::Spatial => {
                let Some(pooled_sink) = self.spatial_sinks.get(sink_index as usize) else {
                    return false;
                };
                pooled_sink.append(decoder);
                pooled_sink.set_volume(volume);
                pooled_sink.play();
            }
        }
        true
    }

    /// Halts a sink without returning it to the pool.
    pub(crate) fn pause(&self, sound_type: SoundType, sink_index: u32) {
        match sound_type {
            SoundType::Ambient => {
                if let Some(pooled_sink) = self.ambient_sinks.get(sink_index as usize) {
                    pooled_sink.pause();
                }
            }
            SoundType::Spatial => {
                if let Some(pooled_sink) = self.spatial_sinks.get(sink_index as usize) {
                    pooled_sink.pause();
                }
            }
        }
    }

    /// Sets a sink's playback volume, where 1.0 is the sound's own level.
    pub(crate) fn set_volume(&self, sound_type: SoundType, sink_index: u32, volume: f32) {
        match sound_type {
            SoundType::Ambient => {
                if let Some(pooled_sink) = self.ambient_sinks.get(sink_index as usize) {
                    pooled_sink.set_volume(volume);
                }
            }
            SoundType::Spatial => {
                if let Some(pooled_sink) = self.spatial_sinks.get(sink_index as usize) {
                    pooled_sink.set_volume(volume);
                }
            }
        }
    }

    /// Moves a spatial sink's emitter to a world position.
    pub(crate) fn set_emitter(&self, sink_index: u32, emitter_position: [f32; 3]) {
        if let Some(pooled_sink) = self.spatial_sinks.get(sink_index as usize) {
            pooled_sink.set_emitter_position(emitter_position);
        }
    }

    /// Moves the ears of every spatial sink.
    ///
    /// The ears belong to the listener, not to a sound, so they are set on
    /// every spatial sink at once instead of per source.
    pub(crate) fn set_ears(&self, left_ear_position: [f32; 3], right_ear_position: [f32; 3]) {
        for pooled_sink in &self.spatial_sinks {
            pooled_sink.set_left_ear_position(left_ear_position);
            pooled_sink.set_right_ear_position(right_ear_position);
        }
    }

    /// Returns the stream handle, for code that builds its own sinks.
    pub fn stream_handle(&self) -> &rodio::OutputStreamHandle {
        &self.stream_handle
    }

    /// Returns how many sinks of each kind are unassigned, as
    /// `(ambient, spatial)`.
    pub fn free_counts(&self) -> (usize, usize) {
        (
            self.free_ambient_sink_indices.len(),
            self.free_spatial_sink_indices.len(),
        )
    }
}
