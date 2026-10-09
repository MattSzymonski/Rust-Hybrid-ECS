//! GPU-side profiling: timestamps, occlusion queries and pipeline statistics.
//!
//! # Responsibilities
//!
//! - Own the query sets, the resolve buffers and the readbacks that turn them
//!   into numbers, for a renderer built on wgpu.
//!
//! # Design
//!
//! Query sets are allocated once with a per-frame cap; each frame resolves into
//! one slot of a three-deep ring, and the blocking readers take the slot one
//! frame back, so a readback never waits on the frame being recorded. Every
//! capability is optional: a device without the timestamp or pipeline
//! statistics features simply gets `None` where a query set would be.

// Standard library
use std::cell::Cell;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

// External crates
use pill_core::profiling::{GpuApi, GpuStatistics, GpuTimeline, GpuTimelineSpan};
use pill_core::telemetry::log_block;
use pill_core::{info, warn};
use wgpu::{
    Buffer, BufferDescriptor, BufferUsages, CommandEncoder, Device, Features,
    PipelineStatisticsTypes, PollType, QuerySet, QuerySetDescriptor, QueryType, Queue,
};

/*
This module provides GPU-side profiling capabilities using wgpu.

- Timestamp queries:
  Measures: Time taken by sections of code (eg. shadow pass, main pass, post-processing, etc).
  This is true GPU time, showing stalls and bottlenecks on GPU side
  Result: Vector of u64 ticks, which can be converted to milliseconds using the provided conversion function.
  Not available on the web (needs TIMESTAMP_QUERY feature)
  Learnings:
    - GPU bottlenecks: If a section takes too long, it indicates a GPU-side bottleneck.
    - Frame time analysis: Helps understand how much time each rendering stage takes.

- Occlusion queries:
  Measures: How many samples (pixels at 1x sampling) passed the depth/stencil tests
  during the draw region bracketed with begin_occlusion_query/end_occlusion_query.
  Result: Vector of u64 counts, one per query. Will be 0 if fully occluded.
  Learnings:
    - Visible coverage: the samples that survived depth testing are the pixels the
      pass actually shaded, so the count over a pass is its real coverage (not
      overdraw - fragments failing the depth test are never counted).
    - Visibility culling: a pass or object with a steady 0 is fully occluded by
      everything drawn before it.
    - Bound analysis: coverage far below the viewport means most of the scene is
      off-screen or hidden.

- Pipeline statistics:
  Collects various pipeline statistics like:
  - VERTEX_SHADER_INVOCATIONS - how many times the vertex shader ran (accounts for the vertex cache with indexed draws).
  - CLIPPER_INVOCATIONS - number of times the clipper stage was invoked (equals triangles output by the vertex stage).
  - CLIPPER_PRIMITIVES_OUT - primitives that survived clipping (triangles that actually proceed to rasterization).
  - FRAGMENT_SHADER_INVOCATIONS - how many fragments executed the fragment shader (per-sample with MSAA; GPUs often execute in 2×2 quads for derivatives).
  - COMPUTE_SHADER_INVOCATIONS - total compute shader invocations (dispatch count × workgroup size).
  Result: Vector of u64 values, one per statistic type requested.
  Not available on the web (needs PIPELINE_STATISTICS_QUERY feature)
*/

/// Environment variable that turns GPU profiling on (`PILL_GPU_PROFILE=1`).
const GPU_PROFILE_VARIABLE: &str = "PILL_GPU_PROFILE";

/// The device features GPU profiling needs: timestamps written between
/// passes, and pipeline statistics counted inside them.
pub(crate) const GPU_PROFILE_FEATURES: Features = Features::TIMESTAMP_QUERY
    .union(Features::TIMESTAMP_QUERY_INSIDE_ENCODERS)
    .union(Features::PIPELINE_STATISTICS_QUERY);

/// Whether `PILL_GPU_PROFILE` asks for GPU profiling. Off by default: the
/// queries cost GPU time, and the periodic readback stalls a frame.
pub(crate) fn gpu_profiling_requested() -> bool {
    std::env::var(GPU_PROFILE_VARIABLE).is_ok_and(|value| value != "0" && !value.is_empty())
}

/// How many frames' results can wait for their readback at once.
///
/// A frame whose ring slot is still waiting records no queries, so the ring
/// has to be deeper than the GPU runs behind: with three slots, a GPU-bound
/// frame uncapped by vsync got results for only every other frame. The slots
/// are a few hundred bytes each.
const FRAMES_IN_FLIGHT: usize = 8;

/// GPU-side profiling using wgpu query sets.
///
/// Owns one query set per category (timestamps, occlusion and pipeline
/// statistics) plus a ring of resolve buffers per category, so a readback can
/// map the previous frame while the current one still records.
pub struct Profiler {
    // Query sets (optional if feature unsupported)
    timestamp_query_set: Option<QuerySet>,
    timestamp_query_names: Vec<String>, // To store names associated with each timestamp
    pipeline_statistics_query_set: Option<QuerySet>,
    pipeline_statistics_types: PipelineStatisticsTypes,
    occlusion_query_set: Option<QuerySet>,

    // Maximum queries we allow per frame for each kind
    max_timestamp_queries: u32,
    max_pipeline_statistics_queries: u32,
    max_occlusion_queries: u32,

    // Rolling per-frame indices
    current_timestamp_query: Cell<u32>,
    current_pipeline_statistics_query: Cell<u32>,
    current_occlusion_query: Cell<u32>,

    // Resolve buffers (ring) for readback, one per in-flight frame
    timestamp_buffers: Vec<Option<ResolveSlot>>,
    pipeline_buffers: Vec<Option<ResolveSlot>>,
    occlusion_buffers: Vec<Option<ResolveSlot>>,

    // Bytes per query result set
    timestamp_queries_result_bytes: u64,
    pipeline_statistics_queries_result_bytes: u64,

    // Frame index for the ring
    frame_index: usize,

    // Conversion period (nanoseconds per timestamp tick)
    timestamp_period_ns: f32,

    // Tracy's GPU timeline, while a profiler is connected (see
    // `attach_timeline`), and the API it is labelled with once asked for.
    timeline: Option<GpuTimeline>,
    timeline_api: Option<GpuApi>,
    // Counts the timelines created, one per profiler connection, so a
    // frame's zones are only ever uploaded to the timeline they began on.
    timeline_generation: u64,
    // What each ring slot's frame recorded, until it is read back.
    frames: Vec<FrameRecord>,
    // Whether this frame records no queries, because its ring slot still
    // waits for an earlier frame's results.
    skipping_frame: bool,
    // The most recent results read back, for `log_latest`.
    latest_timings: Vec<(String, f32)>,
    latest_statistics: Vec<GpuStatistics>,
    // One sample count per occlusion query read back, in pass order.
    latest_occlusion: Vec<u64>,
}

/// Where one readback buffer's `map_async` stands.
const MAP_PENDING: u8 = 0;
const MAP_DONE: u8 = 1;
const MAP_FAILED: u8 = 2;

/// What one ring slot's frame recorded, kept until its results are read
/// back without blocking.
#[derive(Default)]
struct FrameRecord {
    /// One per timestamp pair: the Tracy zone its timestamps time, or
    /// `None` when Tracy had no zone to give.
    spans: Vec<Option<GpuTimelineSpan>>,
    /// The timeline generation the zones began on.
    timeline_generation: u64,
    timestamp_names: Vec<String>,
    timestamp_count: u32,
    statistics_count: u32,
    occlusion_count: u32,
    /// The timestamp, statistics and occlusion readbacks in flight, each `MAP_*`.
    timestamp_map: Option<Arc<AtomicU8>>,
    statistics_map: Option<Arc<AtomicU8>>,
    occlusion_map: Option<Arc<AtomicU8>>,
}

impl FrameRecord {
    /// Whether a readback was asked for and every one asked for finished.
    fn is_ready(&self) -> bool {
        let finished = |map: &Option<Arc<AtomicU8>>| {
            map.as_ref()
                .is_none_or(|state| state.load(Ordering::Acquire) != MAP_PENDING)
        };
        self.is_waiting()
            && finished(&self.timestamp_map)
            && finished(&self.statistics_map)
            && finished(&self.occlusion_map)
    }

    /// Whether this slot still waits for a readback.
    fn is_waiting(&self) -> bool {
        self.timestamp_map.is_some()
            || self.statistics_map.is_some()
            || self.occlusion_map.is_some()
    }
}

/// Starts mapping `buffer` for reading, returning where the map stands.
fn map_for_reading(buffer: &Buffer) -> Arc<AtomicU8> {
    let state = Arc::new(AtomicU8::new(MAP_PENDING));
    let callback_state = Arc::clone(&state);
    buffer
        .slice(..)
        .map_async(wgpu::MapMode::Read, move |result| {
            let outcome = if result.is_ok() { MAP_DONE } else { MAP_FAILED };
            callback_state.store(outcome, Ordering::Release);
        });
    state
}

/// The first `count` values of a mapped readback buffer, which is then
/// unmapped. Empty when the map failed, which leaves nothing to unmap.
fn take_mapped(buffer: &Buffer, state: &AtomicU8, count: usize) -> Vec<u64> {
    if state.load(Ordering::Acquire) != MAP_DONE {
        return Vec::new();
    }
    let data = buffer.slice(..).get_mapped_range();
    let values: &[u64] = bytemuck::cast_slice(&data);
    let taken = values[..count.min(values.len())].to_vec();
    drop(data);
    buffer.unmap();
    taken
}

impl Profiler {
    /// Creates a profiler against `device` and `queue`.
    ///
    /// The `max_*` arguments are per-frame caps: a recording past its
    /// cap is refused with a printed notice rather than overwriting an
    /// earlier query of the same kind. Query sets for the feature-gated
    /// categories are only created for the features the device was created
    /// with (an adapter offering one is not enough: the device has to have
    /// requested it, see [`GPU_PROFILE_FEATURES`]), so on a device without
    /// timestamp or pipeline statistics support the matching readers always
    /// answer `None`. Occlusion queries are core in wgpu and need no feature,
    /// so only the cap decides whether that set exists.
    pub fn new(
        device: &Device,
        queue: &Queue,
        max_timestamp_queries: u32, // Number of timestamp writes planned to record per frame (start/end of sections)
        max_pipeline_statistics_queries: u32, // Number of pipeline statistics queries per frame
        pipeline_statistics_types: PipelineStatisticsTypes, // Type of pipeline statistics to collect
        max_occlusion_queries: u32, // Number of occlusion queries per frame (one per geometry pass)
    ) -> Self {
        let features = device.features();
        // `write_timestamp` on an encoder is what this profiler calls, so the
        // inside-encoders bit is required, not just the query feature.
        let has_timestamps = features
            .contains(Features::TIMESTAMP_QUERY | Features::TIMESTAMP_QUERY_INSIDE_ENCODERS);
        let has_pipeline_statistics = features.contains(Features::PIPELINE_STATISTICS_QUERY);

        // Timestamp query set
        let timestamp_query_set = if has_timestamps && max_timestamp_queries > 0 {
            Some(device.create_query_set(&QuerySetDescriptor {
                label: Some("gpu_profiler.timestamp_query_set"),
                ty: QueryType::Timestamp,
                count: max_timestamp_queries,
            }))
        } else {
            None
        };

        // Pipeline statistics query set
        let pipeline_statistics_query_set =
            if has_pipeline_statistics && !pipeline_statistics_types.is_empty() {
                Some(device.create_query_set(&QuerySetDescriptor {
                    label: Some("gpu_profiler.pipeline_statistics_query_set"),
                    ty: QueryType::PipelineStatistics(pipeline_statistics_types),
                    count: max_pipeline_statistics_queries,
                }))
            } else {
                None
            };

        // Occlusion query set: one query per geometry pass counts the samples
        // that survived its depth and stencil tests. Core in wgpu, so no
        // feature check - only the cap can leave it out.
        let occlusion_query_set = if max_occlusion_queries > 0 {
            Some(device.create_query_set(&QuerySetDescriptor {
                label: Some("gpu_profiler.occlusion_query_set"),
                ty: QueryType::Occlusion,
                count: max_occlusion_queries,
            }))
        } else {
            None
        };

        // Bytes per query result entry
        let timestamp_queries_result_bytes = std::mem::size_of::<u64>() as u64;
        let pipeline_statistics_fields = pipeline_statistics_types.bits().count_ones() as u64;
        let pipeline_statistics_queries_result_bytes = if pipeline_statistics_fields == 0 {
            0
        } else {
            pipeline_statistics_fields * std::mem::size_of::<u64>() as u64
        };

        // Resolve buffers ring (created lazily on first use)
        let timestamp_buffers = (0..FRAMES_IN_FLIGHT).map(|_| None).collect();
        let pipeline_buffers = (0..FRAMES_IN_FLIGHT).map(|_| None).collect();
        let occlusion_buffers = (0..FRAMES_IN_FLIGHT).map(|_| None).collect();

        // Timestamp period
        let timestamp_period_ns = if has_timestamps {
            queue.get_timestamp_period()
        } else {
            0.0
        };

        Self {
            timestamp_query_set,
            timestamp_query_names: Vec::new(),
            pipeline_statistics_query_set,
            pipeline_statistics_types,
            occlusion_query_set,

            max_timestamp_queries,
            max_pipeline_statistics_queries: max_pipeline_statistics_queries.max(1),
            max_occlusion_queries: max_occlusion_queries.max(1),

            current_timestamp_query: Cell::new(0),
            current_pipeline_statistics_query: Cell::new(0),
            current_occlusion_query: Cell::new(0),

            timestamp_buffers,
            pipeline_buffers,
            occlusion_buffers,

            timestamp_queries_result_bytes,
            pipeline_statistics_queries_result_bytes,

            frame_index: 0,
            timestamp_period_ns,

            timeline: None,
            timeline_api: None,
            timeline_generation: 0,
            frames: (0..FRAMES_IN_FLIGHT)
                .map(|_| FrameRecord::default())
                .collect(),
            skipping_frame: false,
            latest_timings: Vec::new(),
            latest_statistics: Vec::new(),
            latest_occlusion: Vec::new(),
        }
    }

    /// Shows the timestamps in Tracy, on a GPU timeline labelled `api`.
    ///
    /// The timeline exists while a profiler is connected: Tracy drops every
    /// event until one connects, so a timeline created before that would
    /// never be announced, and the profiler would be sent zones of a
    /// timeline it does not know. [`Self::begin_frame`] creates it.
    pub fn attach_timeline(&mut self, api: GpuApi) {
        if self.timestamp_query_set.is_some() {
            self.timeline_api = Some(api);
        }
    }

    /// Creates the timeline when a profiler connects and drops it when the
    /// profiler goes, so every connection gets a timeline of its own.
    ///
    /// Tracy aligns GPU time with CPU time from one timestamp taken as the
    /// timeline is created, so creating one records a timestamp, submits
    /// it and waits for it: one stall per connection.
    fn follow_profiler_connection(&mut self, device: &Device, queue: &Queue) {
        let connected = pill_core::profiling::profiler_connected();
        if !connected && self.timeline.is_some() {
            self.forget_pending_spans();
            self.timeline = None;
        }
        let (true, None, Some(api), Some(query_set)) = (
            connected,
            &self.timeline,
            self.timeline_api,
            &self.timestamp_query_set,
        ) else {
            return;
        };
        let Some(now) = read_current_timestamp(device, queue, query_set) else {
            warn!(target: pill_core::telemetry::telemetry_target::RENDERING, "GPU profiler: could not read a starting timestamp, so Tracy shows no GPU timeline");
            self.timeline_api = None;
            return;
        };
        // Zones still waiting belong to the previous connection.
        self.forget_pending_spans();
        self.timeline = GpuTimeline::new("GPU", api, now as i64, self.timestamp_period_ns);
        self.timeline_generation += 1;
    }

    /// Lets go of every zone still waiting for its timestamps without
    /// telling Tracy: they began on a timeline the profiler no longer
    /// knows, and dropping one would send its end there.
    fn forget_pending_spans(&mut self) {
        for record in &mut self.frames {
            for span in record.spans.drain(..).flatten() {
                span.discard();
            }
        }
    }

    /// Call once at the start of frame.
    ///
    /// Hands the results earlier frames have finished reading back to Tracy
    /// first. When this frame's ring slot still waits for its readback (the
    /// GPU is more frames behind than the ring is deep), the frame records
    /// no queries rather than overwrite results not read yet.
    pub fn begin_frame(&mut self, device: &Device, queue: &Queue) {
        // Runs the map callbacks of whatever the GPU finished, without waiting.
        let _ = device.poll(PollType::Poll);
        self.collect_finished_frames();
        self.follow_profiler_connection(device, queue);
        self.skipping_frame = self.frames[self.frame_index].is_waiting();
        self.current_timestamp_query.set(0);
        self.current_pipeline_statistics_query.set(0);
        self.current_occlusion_query.set(0);
        self.timestamp_query_names.clear();
    }

    /// Call once at the end of frame, after the frame was submitted.
    ///
    /// Starts reading this frame's results back without waiting for them;
    /// a later [`Self::begin_frame`] collects them once the GPU is done. Then
    /// advances the ring, so the next frame resolves into another slot.
    pub fn end_frame(&mut self) {
        if !self.skipping_frame {
            let slot = self.frame_index;
            let timestamp_count = self.current_timestamp_query.get();
            let statistics_count = self.current_pipeline_statistics_query.get();
            let occlusion_count = self.current_occlusion_query.get();
            let timestamp_buffer = self.timestamp_buffers[slot]
                .as_ref()
                .map(|buffers| &buffers.readback);
            let statistics_buffer = self.pipeline_buffers[slot]
                .as_ref()
                .map(|buffers| &buffers.readback);
            let occlusion_buffer = self.occlusion_buffers[slot]
                .as_ref()
                .map(|buffers| &buffers.readback);
            let record = &mut self.frames[slot];
            record.timestamp_names = self.timestamp_query_names.clone();
            record.timestamp_count = timestamp_count;
            record.statistics_count = statistics_count;
            record.occlusion_count = occlusion_count;
            record.timestamp_map = timestamp_buffer
                .filter(|_| timestamp_count > 0)
                .map(map_for_reading);
            record.statistics_map = statistics_buffer
                .filter(|_| statistics_count > 0)
                .map(map_for_reading);
            record.occlusion_map = occlusion_buffer
                .filter(|_| occlusion_count > 0)
                .map(map_for_reading);
        }
        self.frame_index = (self.frame_index + 1) % FRAMES_IN_FLIGHT;
    }

    /// Reads back every ring slot whose results arrived: uploads its
    /// timestamps to its Tracy zones, plots its statistics, and keeps the
    /// timings, statistics and occlusion counts for [`Self::log_latest`].
    fn collect_finished_frames(&mut self) {
        for slot in 0..FRAMES_IN_FLIGHT {
            if !self.frames[slot].is_ready() {
                continue;
            }
            let mut record = std::mem::take(&mut self.frames[slot]);
            if record.timeline_generation != self.timeline_generation || self.timeline.is_none() {
                for span in record.spans.drain(..).flatten() {
                    span.discard();
                }
            }

            if let (Some(state), Some(buffers)) =
                (&record.timestamp_map, &self.timestamp_buffers[slot])
            {
                let ticks = take_mapped(&buffers.readback, state, record.timestamp_count as usize);
                let mut timings = Vec::new();
                for (pair_index, pair) in ticks.chunks_exact(2).enumerate() {
                    if let Some(Some(span)) = record.spans.get(pair_index) {
                        span.upload(pair[0] as i64, pair[1] as i64);
                    }
                    let name = record
                        .timestamp_names
                        .get(pair_index * 2)
                        .cloned()
                        .unwrap_or_else(|| format!("Section {pair_index}"));
                    timings.push((
                        name,
                        self.timestamp_ticks_to_ms(pair[1].saturating_sub(pair[0])),
                    ));
                }
                if !timings.is_empty() {
                    self.latest_timings = timings;
                }
            }

            if let (Some(state), Some(buffers)) =
                (&record.statistics_map, &self.pipeline_buffers[slot])
            {
                let stride = self.pipeline_statistics_types.bits().count_ones() as usize;
                let values = take_mapped(
                    &buffers.readback,
                    state,
                    record.statistics_count as usize * stride,
                );
                let statistics: Vec<GpuStatistics> = values
                    .chunks_exact(stride.max(1))
                    .map(|query| statistics_of(self.pipeline_statistics_types, query))
                    .collect();
                if !statistics.is_empty() {
                    // The plots show the frame: every geometry pass together.
                    let mut frame_total = GpuStatistics::default();
                    for pass in &statistics {
                        frame_total.vertex_invocations += pass.vertex_invocations;
                        frame_total.clipper_invocations += pass.clipper_invocations;
                        frame_total.clipper_primitives_out += pass.clipper_primitives_out;
                        frame_total.fragment_invocations += pass.fragment_invocations;
                    }
                    pill_core::profiling::plot_gpu_statistics(frame_total);
                    self.latest_statistics = statistics;
                }
            }

            if let (Some(state), Some(buffers)) =
                (&record.occlusion_map, &self.occlusion_buffers[slot])
            {
                let samples =
                    take_mapped(&buffers.readback, state, record.occlusion_count as usize);
                if !samples.is_empty() {
                    self.latest_occlusion = samples;
                }
            }
            // The record's zones drop here, every one uploaded.
        }
    }

    /// Logs the most recent pass timings, occlusion samples and pipeline
    /// statistics read back, without waiting for the GPU.
    ///
    /// `viewport_pixels` is the frame's drawable area in pixels; the occlusion
    /// block reports each geometry pass's samples against it. Pass `0` to
    /// report the raw counts without a percentage.
    pub fn log_latest(&self, viewport_pixels: u64) {
        if !self.latest_timings.is_empty() {
            let lines = self
                .latest_timings
                .iter()
                .map(|(label, ms)| format!("{label:<24}: {ms:6.3} ms"));
            info!(target: pill_core::telemetry::telemetry_target::RENDERING, "{}", log_block("GPU timestamps", lines));
        }
        if !self.latest_occlusion.is_empty() {
            let total: u64 = self.latest_occlusion.iter().sum();
            let coverage = if viewport_pixels > 0 {
                format!(
                    "{:.1}% of {viewport_pixels} px",
                    total as f64 * 100.0 / viewport_pixels as f64
                )
            } else {
                "viewport unknown".to_string()
            };
            let mut lines: Vec<String> = self
                .latest_occlusion
                .iter()
                .enumerate()
                .map(|(pass, samples)| format!("{:<24}: {samples} samples", format!("pass {pass}")))
                .collect();
            lines.push(format!("{:<24}: {total} samples ({coverage})", "visible"));
            info!(target: pill_core::telemetry::telemetry_target::RENDERING, "{}", log_block("GPU occlusion", lines));
        }
        if !self.latest_statistics.is_empty() {
            let lines = self
                .latest_statistics
                .iter()
                .enumerate()
                .flat_map(|(query, pass)| {
                    [
                        format!("query {query}:"),
                        format!("  {:>24}: {}", "VS invocations", pass.vertex_invocations),
                        format!(
                            "  {:>24}: {}",
                            "Clipper invocations", pass.clipper_invocations
                        ),
                        format!(
                            "  {:>24}: {}",
                            "Clipper primitives out", pass.clipper_primitives_out
                        ),
                        format!("  {:>24}: {}", "FS invocations", pass.fragment_invocations),
                    ]
                });
            info!(target: pill_core::telemetry::telemetry_target::RENDERING, "{}", log_block("GPU pipeline statistics", lines));
        }
    }

    // --- Timestamps ---

    /// Returns the timestamp query set, or `None` when the adapter exposes no
    /// timestamp support or the cap was zero.
    ///
    /// For callers that want to write timestamps themselves; prefer
    /// [`Self::write_timestamp`], which keeps the index and name bookkeeping
    /// the summaries rely on.
    pub fn get_timestamp_query_set(&self) -> Option<&QuerySet> {
        self.timestamp_query_set.as_ref()
    }

    /// Write a timestamp (returns its query index)
    /// Called before and after a region to time
    pub fn write_timestamp(&mut self, encoder: &mut CommandEncoder, name: &str) -> Option<u32> {
        if self.skipping_frame {
            return None;
        }
        if let Some(query_set) = &self.timestamp_query_set {
            // Check if there is space for another timestamp
            if self.current_timestamp_query.get() >= self.max_timestamp_queries {
                warn!(target: pill_core::telemetry::telemetry_target::RENDERING, "GPU profiler: timestamp queries are full for this frame");
                return None;
            }

            let index = self.current_timestamp_query.get();
            self.current_timestamp_query.set(index + 1);
            encoder.write_timestamp(query_set, index);

            // Store the name in parallel with the timestamp index
            self.timestamp_query_names.push(name.to_string());

            // Timestamps come in pairs around a region: the first opens its
            // Tracy zone, now, on this thread; the second ends it.
            let record = &mut self.frames[self.frame_index];
            record.timeline_generation = self.timeline_generation;
            if index.is_multiple_of(2) {
                let span = self
                    .timeline
                    .as_ref()
                    .and_then(|timeline| timeline.begin_span(name, file!(), line!()));
                record.spans.push(span);
            } else if let Some(Some(span)) = record.spans.last_mut() {
                span.end();
            }

            Some(index)
        } else {
            None
        }
    }

    /// Resolve all timestamps recorded so far this frame into the ring buffer
    pub fn resolve_timestamp_queries(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
    ) {
        if let Some(query_set) = &self.timestamp_query_set {
            let count = self.current_timestamp_query.get();
            if count == 0 {
                return;
            }
            let byte_len = self.timestamp_queries_result_bytes * count as u64;

            let index = self.frame_index;
            let slot = &mut self.timestamp_buffers[index];
            let buffers = ensure_buffer_slot(
                device,
                slot,
                byte_len,
                "gpu_profiler.timestamp_queries.resolve",
            );

            encoder.resolve_query_set(query_set, 0..count, &buffers.resolve, 0);

            // Resolved results are not mappable; copied into the buffer that is.

            encoder.copy_buffer_to_buffer(&buffers.resolve, 0, &buffers.readback, 0, byte_len);
        }
    }

    /// Convert a delta of timestamp ticks to milliseconds.
    pub fn timestamp_ticks_to_ms(&self, delta_ticks: u64) -> f32 {
        (delta_ticks as f32 * self.timestamp_period_ns) / 1_000_000.0
    }

    // --- Pipeline statistics ---

    /// Expose the pipeline stats query set (and mask) so you can begin/end in passes
    pub fn pipeline_statistics_query_set(&self) -> Option<(&QuerySet, PipelineStatisticsTypes)> {
        self.pipeline_statistics_query_set
            .as_ref()
            .map(|query_set| (query_set, self.pipeline_statistics_types))
    }

    /// Begin a pipeline statistics query in the pass; returns query index
    pub fn begin_pipeline_statistics_query(
        &self,
        render_pass: &mut wgpu::RenderPass<'_>,
    ) -> Option<u32> {
        if self.skipping_frame {
            return None;
        }
        if let Some(query_set) = &self.pipeline_statistics_query_set {
            // Check if there is space for another pipeline statistics query
            if self.current_pipeline_statistics_query.get() >= self.max_pipeline_statistics_queries
            {
                warn!(target: pill_core::telemetry::telemetry_target::RENDERING, "GPU profiler: pipeline statistics queries are full for this frame");
                return None;
            }

            let index = self.current_pipeline_statistics_query.get();
            self.current_pipeline_statistics_query.set(index + 1);
            render_pass.begin_pipeline_statistics_query(query_set, index);
            Some(index)
        } else {
            None
        }
    }

    /// Ends the pipeline statistics query most recently begun in
    /// `render_pass`.
    ///
    /// Pairs with [`Self::begin_pipeline_statistics_query`]; the counters
    /// become readable once the frame's queries are resolved.
    pub fn end_pipeline_statistics_query(&self, render_pass: &mut wgpu::RenderPass<'_>) {
        render_pass.end_pipeline_statistics_query();
    }

    /// Resolves the pipeline statistics recorded this frame into the current
    /// ring slot.
    ///
    /// A no-op when the feature is unsupported or no query was begun this
    /// frame, so a frame with nothing to resolve leaves its ring slot at
    /// whatever the previous resolve put there.
    pub fn resolve_pipeline_statistics_queries(
        &mut self,
        device: &Device,
        encoder: &mut CommandEncoder,
    ) {
        if let Some(query_set) = &self.pipeline_statistics_query_set {
            let count = self.current_pipeline_statistics_query.get();
            if count == 0 {
                return;
            }
            let byte_len = self.pipeline_statistics_queries_result_bytes * count as u64;

            let index = self.frame_index;
            let slot = &mut self.pipeline_buffers[index];
            let buffers = ensure_buffer_slot(
                device,
                slot,
                byte_len,
                "gpu_profiler.pipeline_statistics_queries.resolve",
            );

            encoder.resolve_query_set(query_set, 0..count, &buffers.resolve, 0);

            // Resolved results are not mappable; copied into the buffer that is.

            encoder.copy_buffer_to_buffer(&buffers.resolve, 0, &buffers.readback, 0, byte_len);
        }
    }

    // --- Occlusion ---

    /// Exposes the occlusion query set for a geometry pass's
    /// `RenderPassDescriptor.occlusion_query_set`.
    ///
    /// `None` when the feature is unsupported or the cap was zero. A pass
    /// only begins queries through [`Self::begin_occlusion_query`], which is
    /// also what refuses during a skipped frame.
    pub fn get_occlusion_query_set(&self) -> Option<&QuerySet> {
        self.occlusion_query_set.as_ref()
    }

    /// Begins the next occlusion query in `render_pass`; returns its index.
    ///
    /// One query per geometry pass counts the samples that passed the depth
    /// and stencil tests while it was open, so the pass's visible coverage is
    /// read back as one number.
    pub fn begin_occlusion_query(&self, render_pass: &mut wgpu::RenderPass<'_>) -> Option<u32> {
        if self.skipping_frame || self.occlusion_query_set.is_none() {
            return None;
        }
        if self.current_occlusion_query.get() >= self.max_occlusion_queries {
            warn!(target: pill_core::telemetry::telemetry_target::RENDERING, "GPU profiler: occlusion queries are full for this frame");
            return None;
        }
        let index = self.current_occlusion_query.get();
        self.current_occlusion_query.set(index + 1);
        render_pass.begin_occlusion_query(index);
        Some(index)
    }

    /// Ends the occlusion query most recently begun in `render_pass`.
    ///
    /// Pairs with [`Self::begin_occlusion_query`]; the sample counts become
    /// readable once the frame's queries are resolved.
    pub fn end_occlusion_query(&self, render_pass: &mut wgpu::RenderPass<'_>) {
        render_pass.end_occlusion_query();
    }

    /// Resolves the occlusion queries recorded this frame into the current
    /// ring slot.
    ///
    /// A no-op when the feature is unsupported or no query was begun this
    /// frame, mirroring [`Self::resolve_pipeline_statistics_queries`].
    pub fn resolve_occlusion_queries(&mut self, device: &Device, encoder: &mut CommandEncoder) {
        if let Some(query_set) = &self.occlusion_query_set {
            let count = self.current_occlusion_query.get();
            if count == 0 {
                return;
            }
            // One u64 per occlusion query.
            let byte_len = std::mem::size_of::<u64>() as u64 * count as u64;

            let index = self.frame_index;
            let slot = &mut self.occlusion_buffers[index];
            let buffers = ensure_buffer_slot(
                device,
                slot,
                byte_len,
                "gpu_profiler.occlusion_queries.resolve",
            );

            encoder.resolve_query_set(query_set, 0..count, &buffers.resolve, 0);

            // Resolved results are not mappable; copied into the buffer that is.

            encoder.copy_buffer_to_buffer(&buffers.resolve, 0, &buffers.readback, 0, byte_len);
        }
    }
}

/// One ring slot's buffers: queries resolve into `resolve`, which is copied
/// into `readback` for the CPU to map. wgpu only lets a mappable buffer be a
/// copy destination, so a query cannot resolve straight into one.
struct ResolveSlot {
    resolve: Buffer,
    readback: Buffer,
}

/// Returns the ring slot's buffers, creating or replacing them when they
/// cannot already hold `size` bytes.
///
/// A slot only ever grows: once a frame with many queries has asked for a
/// large buffer, later smaller frames reuse that allocation instead of
/// shrinking it.
#[inline]
fn ensure_buffer_slot<'a>(
    device: &wgpu::Device,
    slot: &'a mut Option<ResolveSlot>,
    size: u64,
    label: &str,
) -> &'a ResolveSlot {
    let need_new = slot
        .as_ref()
        .map(|buffers| buffers.resolve.size() < size)
        .unwrap_or(true);
    if need_new {
        let buffer = |usage: BufferUsages| {
            device.create_buffer(&BufferDescriptor {
                label: Some(label),
                size,
                usage,
                mapped_at_creation: false,
            })
        };
        *slot = Some(ResolveSlot {
            resolve: buffer(BufferUsages::QUERY_RESOLVE | BufferUsages::COPY_SRC),
            readback: buffer(BufferUsages::MAP_READ | BufferUsages::COPY_DST),
        });
    }
    slot.as_ref().unwrap()
}

/// One query's pipeline statistics, read in the order wgpu writes them: one
/// value per statistic `types` asked for, lowest flag first.
fn statistics_of(types: PipelineStatisticsTypes, query: &[u64]) -> GpuStatistics {
    let mut statistics = GpuStatistics::default();
    let mut values = query.iter().copied();
    for flag in types.iter() {
        let value = values.next().unwrap_or(0);
        if flag == PipelineStatisticsTypes::VERTEX_SHADER_INVOCATIONS {
            statistics.vertex_invocations = value;
        } else if flag == PipelineStatisticsTypes::CLIPPER_INVOCATIONS {
            statistics.clipper_invocations = value;
        } else if flag == PipelineStatisticsTypes::CLIPPER_PRIMITIVES_OUT {
            statistics.clipper_primitives_out = value;
        } else if flag == PipelineStatisticsTypes::FRAGMENT_SHADER_INVOCATIONS {
            statistics.fragment_invocations = value;
        }
    }
    statistics
}

/// The GPU's clock now, in timestamp ticks: one timestamp written, submitted
/// and waited for. `None` when the readback fails.
fn read_current_timestamp(device: &Device, queue: &Queue, query_set: &QuerySet) -> Option<u64> {
    let size = std::mem::size_of::<u64>() as u64;
    let buffer = |usage: BufferUsages| {
        device.create_buffer(&BufferDescriptor {
            label: Some("gpu_profiler.calibration"),
            size,
            usage,
            mapped_at_creation: false,
        })
    };
    let resolve = buffer(BufferUsages::QUERY_RESOLVE | BufferUsages::COPY_SRC);
    let readback = buffer(BufferUsages::MAP_READ | BufferUsages::COPY_DST);

    let mut encoder = device.create_command_encoder(&wgpu::CommandEncoderDescriptor {
        label: Some("gpu_profiler.calibration"),
    });
    encoder.write_timestamp(query_set, 0);
    encoder.resolve_query_set(query_set, 0..1, &resolve, 0);
    encoder.copy_buffer_to_buffer(&resolve, 0, &readback, 0, size);
    queue.submit(std::iter::once(encoder.finish()));

    let state = map_for_reading(&readback);
    device.poll(PollType::Wait).ok()?;
    take_mapped(&readback, &state, 1).first().copied()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statistics_are_read_in_flag_order() {
        let types = PipelineStatisticsTypes::VERTEX_SHADER_INVOCATIONS
            | PipelineStatisticsTypes::CLIPPER_PRIMITIVES_OUT
            | PipelineStatisticsTypes::FRAGMENT_SHADER_INVOCATIONS;

        let statistics = statistics_of(types, &[10, 20, 30]);

        assert_eq!(statistics.vertex_invocations, 10);
        assert_eq!(statistics.clipper_invocations, 0);
        assert_eq!(statistics.clipper_primitives_out, 20);
        assert_eq!(statistics.fragment_invocations, 30);
    }

    #[test]
    fn a_slot_is_ready_only_once_every_readback_it_asked_for_finished() {
        let pending = Arc::new(AtomicU8::new(MAP_PENDING));
        let mut record = FrameRecord {
            timestamp_map: Some(Arc::clone(&pending)),
            statistics_map: Some(Arc::new(AtomicU8::new(MAP_DONE))),
            ..FrameRecord::default()
        };
        assert!(record.is_waiting() && !record.is_ready());

        pending.store(MAP_FAILED, Ordering::Release);
        assert!(record.is_ready(), "a failed map still finishes the slot");

        record.timestamp_map = None;
        record.statistics_map = None;
        assert!(!record.is_waiting() && !record.is_ready());
    }
}
