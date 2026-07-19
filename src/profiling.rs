// ----------------------------------------------------------------------------
// Profiling Abstraction Layer (Tracy Profiler)
// ----------------------------------------------------------------------------
//!
//! All profiling instrumentation lives here. Two feature levels:
//!
//! | Capability                | `tracing` | `tracing-minimal` | (none) |
//! |---------------------------|-----------|-------------------|--------|
//! | CPU zones                 | +         | +                 | -      |
//! | Zone text/details         | +         | -                 | -      |
//! | Native Tracy plots        | +         | -                 | -      |
//! | Messages (white)          | +         | -                 | -      |
//! | Warnings (orange)         | +         | -                 | -      |
//! | Errors (red)              | +         | -                 | -      |
//! | Thread naming             | +         | -                 | -      |
//! | Frame marks               | +         | +                 | -      |
//! | Secondary frames          | +         | +                 | -      |
//! | Non-continuous frames     | +         | +                 | -      |
//! | Call stack sampling       | +         | +                 | -      |
//! | Context switch tracing    | +         | -                 | -      |
//! | System info (CPU freq)    | +         | -                 | -      |
//! | On-demand collection      | +         | +                 | -      |
//! | Code transfer (asm)       | +         | -                 | -      |
//! | Allocation tracking       | +         | -                 | -      |
//!
//! # Setup
//!
//! Call `profile_init!()` once at the start of `main()` to activate Tracy.
//! Without it, all instrumentation silently no-ops even with the feature on.
//!
//! # Usage
//!
//! ```ignore
//! // Static name (zero-allocation, preferred):
//! let _zone = profile_scope!("movement");
//!
//! // Frame boundary:
//! profile_frame_mark!();
//!
//! // Secondary (named) frame for subsystems:
//! profile_secondary_frame_mark!("scripts");
//!
//! // Plot (identifier name → native Tracy graph):
//! profile_plot!(entity_count, world.entity_count() as f64);
//!
//! // Message / warn / error (warn=orange, error=red):
//! profile_message!("archetype created: {:?}", id);
//! profile_warn!("duplicate iterator label: {}", label);
//! profile_error!("command execution failed: {:?}", err);
//!
//! // Thread naming:
//! profile_thread!("worker_pool");
//!
//! // Non-continuous frame (one-shot operations):
//! let _setup = profile_non_continuous_frame!("engine_init");
//!
//! // Plot configuration (one-time setup):
//! profile_plot_config!(entity_count, PlotConfiguration::default()
//!     .format(PlotFormat::Number));
//! ```

// ============================================================================
// Compile-time safeguards
// ============================================================================

#[cfg(all(feature = "tracing", feature = "tracing-minimal"))]
compile_error!("`tracing` and `tracing-minimal` are mutually exclusive. Enable only one.");

// ============================================================================
// Tracy-enabled implementation (shared by both `tracing` and `tracing-minimal`)
// ============================================================================

#[cfg(any(feature = "tracing", feature = "tracing-minimal"))]
mod enabled {
    use std::fmt::Arguments;
    use tracy_client::Client;

    // Lazy-initialized client handle. The first call to `client()` starts Tracy.
    pub(crate) fn client() -> Client {
        use std::sync::OnceLock;
        static CLIENT: OnceLock<Client> = OnceLock::new();
        CLIENT.get_or_init(Client::start).clone()
    }

    /// Call once at startup to initialize Tracy. Idempotent - safe to call
    /// multiple times. Without this, all instrumentation silently no-ops.
    #[inline]
    pub fn init() {
        let _ = client();
    }

    /// RAII guard for a static-name CPU zone. Created by `profile_scope!("name")`.
    #[must_use = "zone closes on drop - bind to a variable"]
    pub struct TracyZone {
        #[allow(dead_code)]
        inner: Option<tracy_client::Span>,
    }

    impl TracyZone {
        #[doc(hidden)]
        #[inline]
        pub fn new_static(
            name: &'static str,
            function: &'static str,
            file: &'static str,
            line: u32,
        ) -> Self {
            let inner = Client::is_running()
                .then(|| client().span_alloc(Some(name), function, file, line, 0));
            Self { inner }
        }

        #[doc(hidden)]
        #[inline]
        pub fn new_dynamic(name: &str, function: &str, file: &str, line: u32) -> Self {
            let inner = Client::is_running()
                .then(|| client().span_alloc(Some(name), function, file, line, 0));
            Self { inner }
        }

        /// Same as [`new_dynamic`](Self::new_dynamic) but the name is
        /// built lazily via a closure.  The closure only runs when Tracy
        /// is running — no `format!()` allocation when profiling is off.
        #[doc(hidden)]
        #[inline]
        pub fn new_dynamic_lazy(
            name: impl FnOnce() -> String,
            function: &str,
            file: &str,
            line: u32,
        ) -> Self {
            let inner = Client::is_running().then(|| {
                let name = name();
                client().span_alloc(Some(&name), function, file, line, 0)
            });
            Self { inner }
        }

        /// Attach a diagnostic message to this zone. The text appears in
        /// Tracy's zone tooltip / detail view.
        ///
        /// Skips formatting and allocation when no profiler is connected.
        ///
        /// Prefer [`text_lazy`](Self::text_lazy) when the message is
        /// expensive to construct (e.g. calls `format!()` or
        /// `get_archetype_info()`).  With `text`, the caller builds the
        /// `Arguments` eagerly which can skew execution timing.
        #[inline]
        pub fn text(&self, msg: Arguments<'_>) {
            if let Some(span) = &self.inner {
                if Client::is_connected() {
                    let text = format!("{}", msg);
                    span.emit_text(&text);
                }
            }
        }

        /// Attach a lazily-built diagnostic message.  The closure is
        /// only invoked when Tracy is actively connected and capturing,
        /// so expensive operations (String allocation, archetype info
        /// formatting, etc.) are skipped during normal execution.
        ///
        /// Use this instead of [`text`](Self::text) for any message
        /// that requires an allocation or non-trivial computation.
        #[inline]
        pub fn text_lazy(&self, f: impl FnOnce() -> String) {
            if let Some(span) = &self.inner {
                if Client::is_connected() {
                    span.emit_text(&f());
                }
            }
        }
    }

    /// Mark end of a frame for Tracy's frame-time graphs.
    #[inline]
    pub fn frame_mark() {
        if Client::is_running() {
            tracy_client::frame_mark();
        }
    }

    /// Mark end of a secondary (named) continuous frame.
    #[inline]
    pub fn secondary_frame_mark(name: tracy_client::FrameName) {
        if Client::is_running() {
            client().secondary_frame_mark(name);
        }
    }

    /// RAII guard for a non-continuous frame. Frame ends on drop.
    #[must_use = "non-continuous frame ends on drop - bind to a variable"]
    pub struct NonContinuousFrame {
        #[allow(dead_code)]
        inner: Option<tracy_client::Frame>,
    }

    /// Begin a non-continuous frame (one-shot operation, not in a loop).
    #[inline]
    pub fn non_continuous_frame_begin(name: tracy_client::FrameName) -> NonContinuousFrame {
        let inner = Client::running().map(|c| c.non_continuous_frame(name));
        NonContinuousFrame { inner }
    }
}

// ============================================================================
// Full `tracing` feature - plots, messages, thread naming
// ============================================================================

#[cfg(feature = "tracing")]
pub mod tracing_extras {
    use std::fmt::Arguments;
    use tracy_client::Client;

    use super::enabled::client;

    /// Emit a data point on a named time-series plot.
    /// Uses Tracy's native plot system - renders as an actual graph in the UI.
    /// Skips work when no profiler is connected.
    #[inline]
    pub fn plot(name: tracy_client::PlotName, value: f64) {
        if Client::is_running() && Client::is_connected() {
            client().plot(name, value);
        }
    }

    /// Configure how a plot appears in the Tracy profiler UI.
    #[inline]
    pub fn plot_config(
        name: tracy_client::PlotName,
        configuration: tracy_client::PlotConfiguration,
    ) {
        if Client::is_running() {
            client().plot_config(name, configuration);
        }
    }

    /// Emit a diagnostic message (white). Skips formatting when no profiler is connected.
    #[inline]
    pub fn message(msg: Arguments<'_>) {
        if Client::is_running() && Client::is_connected() {
            let text = format!("{}", msg);
            client().message(&text, 0);
        }
    }

    /// Emit a warning message (orange, RGBA 0xFF8800FF).
    #[inline]
    pub fn warn(msg: Arguments<'_>) {
        if Client::is_running() && Client::is_connected() {
            let text = format!("{}", msg);
            client().color_message(&text, 0xFF8800FF, 0);
        }
    }

    /// Emit an error message (red, RGBA 0xFF0000FF).
    #[inline]
    pub fn error(msg: Arguments<'_>) {
        if Client::is_running() && Client::is_connected() {
            let text = format!("{}", msg);
            client().color_message(&text, 0xFF0000FF, 0);
        }
    }

    /// Set the display name of the current thread.
    /// Uses the safe `Client::set_thread_name` API instead of unsafe FFI.
    #[inline]
    pub fn set_thread_name(name: &str) {
        if Client::is_running() {
            client().set_thread_name(name);
        }
    }
}

// ============================================================================
// Disabled (no-op) implementation - neither feature enabled
// ============================================================================

#[cfg(not(any(feature = "tracing", feature = "tracing-minimal")))]
mod enabled {
    use std::fmt::Arguments;

    #[must_use = "zone closes on drop - bind to a variable"]
    pub struct TracyZone;

    impl TracyZone {
        #[doc(hidden)]
        #[inline(always)]
        pub fn new_static(
            _name: &'static str,
            _fn: &'static str,
            _file: &'static str,
            _line: u32,
        ) -> Self {
            Self
        }

        #[doc(hidden)]
        #[inline(always)]
        pub fn new_dynamic(_name: &str, _function: &str, _file: &str, _line: u32) -> Self {
            Self
        }

        #[inline(always)]
        pub fn new_dynamic_lazy(
            _name: impl FnOnce() -> String,
            _function: &str,
            _file: &str,
            _line: u32,
        ) -> Self {
            Self
        }

        #[inline(always)]
        pub fn text(&self, _msg: Arguments<'_>) {}

        #[inline(always)]
        pub fn text_lazy(&self, _f: impl FnOnce() -> String) {}
    }

    #[must_use = "non-continuous frame ends on drop - bind to a variable"]
    pub struct NonContinuousFrame;

    // Opaque stand-in for `tracy_client::FrameName` when the crate isn't linked.
    #[doc(hidden)]
    pub struct OpaqueFrameName;

    #[inline(always)]
    pub fn init() {}
    #[inline(always)]
    pub fn frame_mark() {}
    #[inline(always)]
    pub fn secondary_frame_mark(_name: OpaqueFrameName) {}
    #[inline(always)]
    pub fn non_continuous_frame_begin(_name: OpaqueFrameName) -> NonContinuousFrame {
        NonContinuousFrame
    }
}

pub use enabled::*;

// `tracing_extras` is only compiled under `tracing`. The macros reference it
// via `$crate::profiling::extras::*` which only exists under `#[cfg(feature = "tracing")]`.
#[cfg(feature = "tracing")]
pub use tracing_extras as extras;

// ============================================================================
// Public macros - the only API call sites use
// ============================================================================

/// Initializes the Tracy profiler client. Call once at startup.
/// Active under both `tracing` and `tracing-minimal`.
#[cfg(any(feature = "tracing", feature = "tracing-minimal"))]
#[macro_export]
macro_rules! profile_init {
    () => {
        $crate::profiling::init();
    };
}

#[cfg(not(any(feature = "tracing", feature = "tracing-minimal")))]
#[macro_export]
macro_rules! profile_init {
    () => {};
}

/// Creates a scoped CPU profiling zone.
///
/// Active under both `tracing` and `tracing-minimal`. Zone text/details
/// (the `[...]` form) only emit under `tracing`.
///
/// ```ignore
/// // Static name (zero-allocation, preferred):
/// let _zone = profile_scope!("movement");
///
/// // With details (only active under `tracing`, no-op under `tracing-minimal`):
/// let _zone = profile_scope!("movement", [("entities: {}", count)]);
///
/// // Dynamic name (use sparingly - allocates):
/// let _zone = profile_scope!("system: {}", name);
/// ```
#[macro_export]
macro_rules! profile_scope {
    ($name:literal) => {
        $crate::profiling::TracyZone::new_static($name, module_path!(), file!(), line!())
    };
    ($name:literal, [ $( $detail:tt ),* $(,)? ]) => {{
        let zone = $crate::profiling::TracyZone::new_static($name, module_path!(), file!(), line!());
        $( $crate::profile_scope_detail!(zone, $detail); )*
        zone
    }};
    ($fmt:literal $(, $fmt_arg:expr)* ; [ $( $detail:tt ),* $(,)? ]) => {{
        let zone = $crate::profiling::TracyZone::new_dynamic_lazy(
            || format!($fmt, $($fmt_arg),*),
            module_path!(),
            file!(),
            line!(),
        );
        $( $crate::profile_scope_detail!(zone, $detail); )*
        zone
    }};
    ($fmt:literal $(, $arg:expr)* $(,)?) => {
        $crate::profiling::TracyZone::new_dynamic_lazy(
            || format!($fmt, $($arg),*),
            module_path!(),
            file!(),
            line!(),
        )
    };
}

/// Internal helper: dispatches a single detail element to `zone.text()`.
/// Only active under `tracing` (no-op under `tracing-minimal`).
#[cfg(feature = "tracing")]
#[doc(hidden)]
#[macro_export]
macro_rules! profile_scope_detail {
    ($zone:ident, ($fmt:literal $(, $arg:expr)* $(,)?)) => {
        $zone.text(format_args!($fmt, $($arg),*));
    };
    ($zone:ident, $text:literal) => {
        $zone.text(format_args!($text));
    };
}

#[cfg(not(feature = "tracing"))]
#[doc(hidden)]
#[macro_export]
macro_rules! profile_scope_detail {
    ($zone:ident, ($fmt:literal $(, $arg:expr)* $(,)?)) => {};
    ($zone:ident, $text:literal) => {};
}

/// Marks the end of a frame. Call once per frame loop iteration.
/// Active under both `tracing` and `tracing-minimal`.
#[cfg(any(feature = "tracing", feature = "tracing-minimal"))]
#[macro_export]
macro_rules! profile_frame_mark {
    () => {
        $crate::profiling::frame_mark();
    };
}

#[cfg(not(any(feature = "tracing", feature = "tracing-minimal")))]
#[macro_export]
macro_rules! profile_frame_mark {
    () => {};
}

/// Marks the end of a secondary (named) continuous frame.
/// Active under both `tracing` and `tracing-minimal`.
#[cfg(any(feature = "tracing", feature = "tracing-minimal"))]
#[macro_export]
macro_rules! profile_secondary_frame_mark {
    ($name:literal) => {
        $crate::profiling::secondary_frame_mark(tracy_client::frame_name!($name));
    };
}

#[cfg(not(any(feature = "tracing", feature = "tracing-minimal")))]
#[macro_export]
macro_rules! profile_secondary_frame_mark {
    ($name:literal) => {};
}

/// Begins a non-continuous frame (one-shot operation).
/// Returns an RAII guard that ends the frame on drop.
/// Active under both `tracing` and `tracing-minimal`.
#[cfg(any(feature = "tracing", feature = "tracing-minimal"))]
#[macro_export]
macro_rules! profile_non_continuous_frame {
    ($name:literal) => {
        $crate::profiling::non_continuous_frame_begin(tracy_client::frame_name!($name))
    };
}

#[cfg(not(any(feature = "tracing", feature = "tracing-minimal")))]
#[macro_export]
macro_rules! profile_non_continuous_frame {
    ($name:literal) => {
        $crate::profiling::NonContinuousFrame
    };
}

/// Emits a data point on a named time-series plot.
/// Uses Tracy's native plot system - renders as an actual graph in the UI.
/// Only active under `tracing` (not `tracing-minimal`).
///
/// The plot name is a Rust identifier (not a string literal).
///
/// ```ignore
/// profile_plot!(entity_count, world.entity_count() as f64);
/// profile_plot!(frame_time_us, elapsed.as_micros() as f64);
/// ```
#[cfg(feature = "tracing")]
#[macro_export]
macro_rules! profile_plot {
    ($name:ident, $value:expr) => {
        $crate::profiling::extras::plot(tracy_client::plot_name!(stringify!($name)), $value as f64);
    };
}

#[cfg(not(feature = "tracing"))]
#[macro_export]
macro_rules! profile_plot {
    ($name:ident, $value:expr) => {};
}

/// Configures how a plot appears in the Tracy profiler UI.
/// Only active under `tracing`.
///
/// ```ignore
/// use tracy_client::{PlotConfiguration, PlotFormat};
/// profile_plot_config!(entity_count, PlotConfiguration::default()
///     .format(PlotFormat::Number));
/// ```
#[cfg(feature = "tracing")]
#[macro_export]
macro_rules! profile_plot_config {
    ($name:ident, $config:expr) => {
        $crate::profiling::extras::plot_config(
            tracy_client::plot_name!(stringify!($name)),
            $config,
        );
    };
}

#[cfg(not(feature = "tracing"))]
#[macro_export]
macro_rules! profile_plot_config {
    ($name:ident, $config:expr) => {};
}

/// Emits a diagnostic message (white) at the current point in the trace.
/// Only active under `tracing` (not `tracing-minimal`).
#[cfg(feature = "tracing")]
#[macro_export]
macro_rules! profile_message {
    ($($arg:tt)*) => {
        $crate::profiling::extras::message(format_args!($($arg)*));
    };
}

#[cfg(not(feature = "tracing"))]
#[macro_export]
macro_rules! profile_message {
    ($($arg:tt)*) => {};
}

/// Emits a warning message (orange) at the current point in the trace.
/// Only active under `tracing`.
#[cfg(feature = "tracing")]
#[macro_export]
macro_rules! profile_warn {
    ($($arg:tt)*) => {
        $crate::profiling::extras::warn(format_args!($($arg)*));
    };
}

#[cfg(not(feature = "tracing"))]
#[macro_export]
macro_rules! profile_warn {
    ($($arg:tt)*) => {};
}

/// Emits an error message (red) at the current point in the trace.
/// Only active under `tracing`.
#[cfg(feature = "tracing")]
#[macro_export]
macro_rules! profile_error {
    ($($arg:tt)*) => {
        $crate::profiling::extras::error(format_args!($($arg)*));
    };
}

#[cfg(not(feature = "tracing"))]
#[macro_export]
macro_rules! profile_error {
    ($($arg:tt)*) => {};
}

/// Sets the display name of the current thread in Tracy.
/// Only active under `tracing`.
#[cfg(feature = "tracing")]
#[macro_export]
macro_rules! profile_thread {
    ($name:expr) => {
        $crate::profiling::extras::set_thread_name($name);
    };
}

#[cfg(not(feature = "tracing"))]
#[macro_export]
macro_rules! profile_thread {
    ($name:expr) => {};
}
