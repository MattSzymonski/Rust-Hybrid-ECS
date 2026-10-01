//! Where log output goes on every target.
//!
//! # Responsibilities
//!
//! - Provide the terminal lane's writer: stdout on native, the browser console
//!   on the web.
//! - Open the file lane's writer: a daily rolling file written by a background
//!   thread on native; the web has no filesystem, so it reports an error.
//!
//! The telemetry builder names only the types and functions here, so the
//! subscriber stack is assembled the same way on every target.

// Standard library
use std::path::Path;

// External crates
#[cfg(target_arch = "wasm32")]
use tracing::{Level, Metadata};
#[cfg(target_arch = "wasm32")]
use tracing_subscriber::fmt::MakeWriter;

/// The terminal lane's writer on native: stdout.
#[cfg(not(target_arch = "wasm32"))]
pub type TerminalWriter = fn() -> std::io::Stdout;

/// The terminal lane's writer on the web: the browser console.
#[cfg(target_arch = "wasm32")]
pub type TerminalWriter = ConsoleWriter;

/// The file lane's writer on native: a non-blocking rolling file.
#[cfg(not(target_arch = "wasm32"))]
pub type FileWriter = tracing_appender::non_blocking::NonBlocking;

/// The file lane's writer on the web, which has no file lane: a sink.
#[cfg(target_arch = "wasm32")]
pub type FileWriter = fn() -> std::io::Sink;

/// Keeps the file lane's background writer alive; dropping it flushes the file.
#[cfg(not(target_arch = "wasm32"))]
pub type FileGuard = tracing_appender::non_blocking::WorkerGuard;

/// The web has no file lane, so there is nothing to keep alive.
#[cfg(target_arch = "wasm32")]
pub type FileGuard = ();

/// The writer the terminal lane formats into: stdout.
#[cfg(not(target_arch = "wasm32"))]
pub fn terminal_writer() -> TerminalWriter {
    std::io::stdout
}

/// The writer the terminal lane formats into: the browser console.
#[cfg(target_arch = "wasm32")]
pub fn terminal_writer() -> TerminalWriter {
    ConsoleWriter
}

/// Open the file lane: `file_name` in `directory`, rotated daily.
///
/// # Errors
///
/// Returns a description of the failure when the directory or file cannot be
/// created.
#[cfg(not(target_arch = "wasm32"))]
pub fn open_file_output(
    directory: &Path,
    file_name: &str,
) -> Result<(FileWriter, FileGuard), String> {
    let appender = tracing_appender::rolling::RollingFileAppender::builder()
        .rotation(tracing_appender::rolling::Rotation::DAILY)
        .filename_prefix(file_name)
        .build(directory)
        .map_err(|error| {
            format!(
                "cannot open log file `{file_name}` in `{}`: {error}",
                directory.display()
            )
        })?;
    Ok(tracing_appender::non_blocking(appender))
}

/// Open the file lane - which the web does not have.
///
/// # Errors
///
/// Always: the web has no filesystem to write logs to.
#[cfg(target_arch = "wasm32")]
pub fn open_file_output(
    directory: &Path,
    file_name: &str,
) -> Result<(FileWriter, FileGuard), String> {
    Err(format!(
        "cannot write log file `{file_name}` to `{}`: the web has no filesystem",
        directory.display()
    ))
}

/// The browser console as a `tracing` writer: each event becomes one console
/// message, at the console level that matches the event's severity.
#[cfg(target_arch = "wasm32")]
#[derive(Clone, Copy, Debug, Default)]
pub struct ConsoleWriter;

#[cfg(target_arch = "wasm32")]
impl<'writer> MakeWriter<'writer> for ConsoleWriter {
    type Writer = ConsoleLine;

    fn make_writer(&'writer self) -> Self::Writer {
        ConsoleLine::new(Level::INFO)
    }

    fn make_writer_for(&'writer self, metadata: &Metadata<'_>) -> Self::Writer {
        ConsoleLine::new(*metadata.level())
    }
}

/// One formatted event on its way to the browser console. The formatter
/// writes it in pieces; it is sent as one message when dropped.
#[cfg(target_arch = "wasm32")]
#[derive(Debug)]
pub struct ConsoleLine {
    level: Level,
    buffer: Vec<u8>,
}

#[cfg(target_arch = "wasm32")]
impl ConsoleLine {
    fn new(level: Level) -> Self {
        Self {
            level,
            buffer: Vec::new(),
        }
    }
}

#[cfg(target_arch = "wasm32")]
impl std::io::Write for ConsoleLine {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.buffer.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(target_arch = "wasm32")]
impl Drop for ConsoleLine {
    // Send the whole event, without the formatter's trailing newline, through
    // the console method of its severity so the browser can filter by level.
    fn drop(&mut self) {
        let text = String::from_utf8_lossy(&self.buffer);
        let message = wasm_bindgen::JsValue::from_str(text.trim_end());
        match self.level {
            Level::ERROR => web_sys::console::error_1(&message),
            Level::WARN => web_sys::console::warn_1(&message),
            Level::INFO => web_sys::console::info_1(&message),
            Level::DEBUG | Level::TRACE => web_sys::console::debug_1(&message),
        }
    }
}
