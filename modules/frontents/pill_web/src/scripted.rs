//! The scripted channel the editor postures share (W-0004 Stage 30,
//! W-0029): the page's one entry point for driving the editor without
//! input events.
//!
//! # Responsibilities
//!
//! - [`editor_command`]: the wasm export the editor page calls with one
//!   scripted line per call.
//! - [`take_scripted_lines`]: the queue drain a running runner calls once a
//!   frame.
//!
//! # Design
//!
//! One queue for both editor postures: a build links exactly one runner, so
//! there is never a second consumer, and the page keeps a single
//! `editor_command` export whatever the build's frontend. Single-threaded
//! like every wasm module.

// Standard library
use std::cell::RefCell;

// External crates
use wasm_bindgen::prelude::wasm_bindgen;

// Lines the page scripted, drained by the next frame.
thread_local! {
    static SCRIPTED_LINES: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

/// The scripted channel's entry point: one line per call, drained by the
/// next frame of whichever editor runner this build links.
#[wasm_bindgen]
pub fn editor_command(line: &str) {
    SCRIPTED_LINES.with(|lines| lines.borrow_mut().push(line.to_owned()));
}

/// Take the lines queued since the last frame.
pub fn take_scripted_lines() -> Vec<String> {
    SCRIPTED_LINES.with(|lines| std::mem::take(&mut *lines.borrow_mut()))
}
