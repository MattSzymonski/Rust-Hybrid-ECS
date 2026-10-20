//! Reading the pick buffer back without stalling a frame.
//!
//! # Responsibilities
//!
//! - Copy each requested pixel of the pick buffer into a mappable buffer
//!   ([`PickReadback::copy_requested_pixels`]).
//! - Map those buffers once the GPU has finished with them, and turn what
//!   they hold into answers ([`PickReadback::collect`]).
//!
//! # Design
//!
//! A frame records the copies and never waits for them: the buffer is mapped
//! after the frame is submitted, and a later [`PickReadback::collect`] polls
//! the device without blocking and decodes whatever has arrived. A pixel
//! holds a queue position, so each readback keeps the entities of the queue it
//! was drawn from; the answer stays right even when later frames reorder the
//! queue.

// Standard library
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Arc;

// External crates
use pill_engine::Entity;
use pill_renderer_api::{decode_pick_value, PickHit, PickRequest, PickResult};

/// Bytes between two copied pixels: wgpu copies whole rows, aligned to this.
const SLOT_BYTES: u64 = wgpu::COPY_BYTES_PER_ROW_ALIGNMENT as u64;

/// The mapping state of one readback, shared with the map callback.
const MAPPING_PENDING: u8 = 0;
const MAPPING_DONE: u8 = 1;
const MAPPING_FAILED: u8 = 2;

/// The copies one frame recorded for one surface.
struct PendingReadback {
    /// One slot of [`SLOT_BYTES`] per request, in request order.
    buffer: wgpu::Buffer,
    /// The requests, one per slot.
    requests: Vec<PickRequest>,
    /// The entity at each queue position of the frame drawn.
    entities: Vec<Entity>,
    /// Where the map callback reports, once mapping has started.
    state: Option<Arc<AtomicU8>>,
}

/// Every pick on its way back from the GPU, and the answers that arrived.
#[derive(Default)]
pub(super) struct PickReadback {
    /// Readbacks recorded, not answered yet.
    pending: Vec<PendingReadback>,
    /// Answers ready to hand out.
    finished: Vec<PickResult>,
}

impl PickReadback {
    /// Answer `request` right away.
    pub fn answer(&mut self, request: PickRequest, hit: PickHit) {
        self.finished.push(PickResult { request, hit });
    }

    /// Record a copy of each request's pixel out of `pick_texture`; a pixel
    /// outside the texture is answered at once with nothing.
    pub fn copy_requested_pixels(
        &mut self,
        device: &wgpu::Device,
        encoder: &mut wgpu::CommandEncoder,
        pick_texture: &wgpu::Texture,
        requests: Vec<PickRequest>,
        entities: Vec<Entity>,
    ) {
        let size = pick_texture.size();
        let (inside, outside): (Vec<PickRequest>, Vec<PickRequest>) = requests
            .into_iter()
            .partition(|request| request.x < size.width && request.y < size.height);
        for request in outside {
            self.answer(request, PickHit::Nothing);
        }
        if inside.is_empty() {
            return;
        }
        let buffer = device.create_buffer(&wgpu::BufferDescriptor {
            label: Some("pick_readback"),
            size: SLOT_BYTES * inside.len() as u64,
            usage: wgpu::BufferUsages::COPY_DST | wgpu::BufferUsages::MAP_READ,
            mapped_at_creation: false,
        });
        for (slot, request) in inside.iter().enumerate() {
            encoder.copy_texture_to_buffer(
                wgpu::TexelCopyTextureInfo {
                    texture: pick_texture,
                    mip_level: 0,
                    origin: wgpu::Origin3d {
                        x: request.x,
                        y: request.y,
                        z: 0,
                    },
                    aspect: wgpu::TextureAspect::All,
                },
                wgpu::TexelCopyBufferInfo {
                    buffer: &buffer,
                    layout: wgpu::TexelCopyBufferLayout {
                        offset: SLOT_BYTES * slot as u64,
                        bytes_per_row: None,
                        rows_per_image: None,
                    },
                },
                wgpu::Extent3d {
                    width: 1,
                    height: 1,
                    depth_or_array_layers: 1,
                },
            );
        }
        self.pending.push(PendingReadback {
            buffer,
            requests: inside,
            entities,
            state: None,
        });
    }

    /// Start mapping every readback recorded since the last call.
    ///
    /// Called after the frame that recorded them was submitted: a buffer
    /// asked to map before its copy is submitted makes the submission fail.
    pub fn start_mapping(&mut self) {
        for readback in self
            .pending
            .iter_mut()
            .filter(|readback| readback.state.is_none())
        {
            let state = Arc::new(AtomicU8::new(MAPPING_PENDING));
            let reported = Arc::clone(&state);
            readback
                .buffer
                .slice(..)
                .map_async(wgpu::MapMode::Read, move |result| {
                    let value = if result.is_ok() {
                        MAPPING_DONE
                    } else {
                        MAPPING_FAILED
                    };
                    reported.store(value, Ordering::Release);
                });
            readback.state = Some(state);
        }
    }

    /// Every answer that has arrived: polls the device without waiting and
    /// decodes each readback the GPU has finished.
    pub fn collect(&mut self, device: &wgpu::Device) -> Vec<PickResult> {
        if !self.pending.is_empty() {
            // Not waiting: an unfinished readback is answered on a later call.
            let _ = device.poll(wgpu::PollType::Poll);
        }
        let mut still_pending = Vec::with_capacity(self.pending.len());
        for readback in self.pending.drain(..) {
            let state = readback
                .state
                .as_ref()
                .map_or(MAPPING_PENDING, |state| state.load(Ordering::Acquire));
            match state {
                MAPPING_DONE => {
                    match readback.buffer.slice(..).get_mapped_range() {
                        Ok(bytes) => {
                            let hits = (0..readback.requests.len())
                                .map(|slot| decode_slot(&bytes, slot, &readback.entities))
                                .collect::<Vec<_>>();
                            drop(bytes);
                            readback.buffer.unmap();
                            for (request, hit) in readback.requests.into_iter().zip(hits) {
                                self.finished.push(PickResult { request, hit });
                            }
                        }
                        Err(_) => readback.buffer.unmap(),
                    }
                }
                // A readback the GPU could not deliver answers nothing rather
                // than leaving its caller waiting forever.
                MAPPING_FAILED => {
                    for request in readback.requests {
                        self.finished.push(PickResult {
                            request,
                            hit: PickHit::Nothing,
                        });
                    }
                }
                _ => still_pending.push(readback),
            }
        }
        self.pending = still_pending;
        std::mem::take(&mut self.finished)
    }
}

/// What slot `slot` of a mapped readback holds.
fn decode_slot(bytes: &[u8], slot: usize, entities: &[Entity]) -> PickHit {
    let start = slot * SLOT_BYTES as usize;
    let value = bytes.get(start..start + 4).map_or(0, |value| {
        u32::from_le_bytes([value[0], value[1], value[2], value[3]])
    });
    decode_pick_value(value, entities)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Each slot is read at its own aligned offset.
    #[test]
    fn slots_decode_at_their_offsets() {
        let mut world = pill_engine::World::new();
        let first = world.reserve_entity();
        let second = world.reserve_entity();
        let mut bytes = vec![0u8; SLOT_BYTES as usize * 3];
        bytes[0..4].copy_from_slice(&2u32.to_le_bytes());
        bytes[SLOT_BYTES as usize..SLOT_BYTES as usize + 4].copy_from_slice(&0u32.to_le_bytes());
        let handle = pill_renderer_api::PICK_HANDLE_BIT | 7;
        bytes[2 * SLOT_BYTES as usize..2 * SLOT_BYTES as usize + 4]
            .copy_from_slice(&handle.to_le_bytes());
        let entities = [first, second];
        assert_eq!(decode_slot(&bytes, 0, &entities), PickHit::Entity(second));
        assert_eq!(decode_slot(&bytes, 1, &entities), PickHit::Nothing);
        assert_eq!(decode_slot(&bytes, 2, &entities), PickHit::Handle(7));
    }

    /// An answer given at once is handed out by the next collect.
    #[test]
    fn immediate_answers_are_collected() {
        let mut readback = PickReadback::default();
        let request = PickRequest {
            id: 3,
            surface: pill_renderer_api::SurfaceId::PRIMARY,
            x: 1,
            y: 1,
        };
        readback.answer(request, PickHit::Nothing);
        assert_eq!(readback.finished.len(), 1);
        assert_eq!(std::mem::take(&mut readback.finished)[0].request.id, 3);
    }
}
