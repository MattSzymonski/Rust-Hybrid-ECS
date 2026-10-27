//! A GPU buffer rewritten every surface, kept between frames.
//!
//! # Responsibilities
//!
//! - [`StreamBuffer`]: upload one surface's bytes into a buffer that is reused
//!   while it is large enough and regrown when a frame outgrows it.
//!
//! # Design
//!
//! The overlay's geometry and the UI painter's lists are rebuilt on the
//! processor each surface, so both need a buffer that is rewritten rather than
//! allocated per draw. This keeps one buffer. A queue write lands before the
//! next submission and every surface submits its own frame, so a buffer
//! written once per surface serves every surface of a frame - which also means
//! a caller must write it at most once per submission.

/// A buffer rewritten every surface, grown when a frame outgrows it.
pub(crate) struct StreamBuffer {
    label: &'static str,
    usage: wgpu::BufferUsages,
    buffer: Option<wgpu::Buffer>,
    capacity: u64,
}

impl StreamBuffer {
    /// An empty stream for buffers of `usage` (`VERTEX`, `INDEX`, ...); the
    /// first write allocates.
    pub(crate) fn new(label: &'static str, usage: wgpu::BufferUsages) -> Self {
        Self {
            label,
            usage,
            buffer: None,
            capacity: 0,
        }
    }

    /// Upload `bytes`; returns the buffer holding them, or `None` when empty.
    pub(crate) fn write(
        &mut self,
        device: &wgpu::Device,
        queue: &wgpu::Queue,
        bytes: &[u8],
    ) -> Option<&wgpu::Buffer> {
        if bytes.is_empty() {
            return None;
        }
        let needed = bytes.len() as u64;
        if self.buffer.is_none() || needed > self.capacity {
            self.capacity = needed.next_power_of_two().max(4096);
            self.buffer = Some(device.create_buffer(&wgpu::BufferDescriptor {
                label: Some(self.label),
                size: self.capacity,
                usage: self.usage | wgpu::BufferUsages::COPY_DST,
                mapped_at_creation: false,
            }));
        }
        let buffer = self.buffer.as_ref()?;
        queue.write_buffer(buffer, 0, bytes);
        Some(buffer)
    }
}
