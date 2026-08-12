//! Headless host — runs the engine hot-reload loop with no window or
//! rendering.
//!
//! # Responsibilities
//!
//! - Initialise the shared [`host`] runtime from environment configuration.
//! - Tick the engine loop indefinitely, printing FPS and entity count each
//!   time [`run_one_frame`](host::run_one_frame) produces a status report.

use host::{run_one_frame, setup, GameModuleConfig};

/// Run the engine loop as fast as the FPS limiter allows, printing a
/// status line on every completed frame.
pub fn run() -> Result<(), Box<dyn std::error::Error>> {
    let mut host = setup(GameModuleConfig::from_environment())?;
    loop {
        if let Some(report) = run_one_frame(&mut host) {
            println!(
                "  {:>6.0} FPS | {:>5} entities",
                report.fps, report.entity_count
            );
        }
    }
}
