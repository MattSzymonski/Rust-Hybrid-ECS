//! Minimal timing adapter for the transferred optional egui drawer.

#[derive(Default)]
pub struct Timer;

impl Timer {
    pub fn record(&mut self, _name: impl Into<String>) {}
    pub fn begin_context(&mut self, _name: impl Into<String>) {}
    pub fn end_context(&mut self) -> crate::error::Result<()> {
        Ok(())
    }
}
