//! The headless run loop: no window, no renderer.
//!
//! # Responsibilities
//!
//! - Run the project frame after frame until the process exits.
//! - Report setup failures, the only failure a headless run can have.
//!
//! # Design
//!
//! The loop is written against [`FrameDriver`], so it drives whichever driver
//! [`crate::posture`] compiled. A headless frame cannot fail, so
//! [`RunError`] converts the driver's `Infallible` error and the loop itself
//! never returns.
//!
//! This file is compiled only in a headless build: `main.rs` declares it
//! instead of `windowed_standalone` while the `rendering` feature is off.

// External crates
use pill_player::FrameDriver;

// Current crate
use crate::error::RunError;
use crate::posture;

/// Run the project continuously without creating a native window.
///
/// Setup is the only fallible step: a headless driver's frame cannot fail, so
/// once the driver exists the loop runs until the process exits.
///
/// # Errors
///
/// Returns [`RunError`] if setup fails, such as when the project cannot be
/// built, loaded or initialized.
pub fn run(project: posture::Project) -> Result<(), RunError> {
    let mut driver = posture::setup(project)?;
    loop {
        driver.run_frame()?;
    }
}
