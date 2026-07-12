// ----------------------------------------------------------------------------
// Profiling Abstraction Layer (Tracy Profiler)
// ----------------------------------------------------------------------------
//!
//! All profiling instrumentation lives here. When the `tracy` feature is
//! disabled, every macro and type compiles to zero-cost no-ops.
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
//! // Dynamic name (for system names, etc.):
//! let _zone = profile_scope_dyn!(registered.name);
//!
//! // Frame boundary:
//! profile_frame_mark!();
//!
//! // Plot:
//! profile_plot!("entity_count", world.entity_count() as f64);
//!
//! // Message:
//! profile_message!("archetype created: {:?}", id);
//!
//! // Thread naming:
//! profile_thread!("worker_pool");
//! ```

// ============================================================================
// Tracy-enabled implementation
// ============================================================================

#[cfg(feature = "tracy")]
mod enabled {
    use std::fmt::Arguments;
    use tracy_client::Client;

    // Lazy-initialized client handle. The first call to `client()` starts Tracy.
    fn client() -> Client {
        use std::sync::OnceLock;
        static CLIENT: OnceLock<Client> = OnceLock::new();
        CLIENT.get_or_init(Client::start).clone()
    }

    /// Call once at startup to initialize Tracy. Idempotent — safe to call
    /// multiple times. Without this, all instrumentation silently no-ops.
    #[inline]
    pub fn init() {
        let _ = client();
    }

    /// RAII guard for a static-name CPU zone. Created by `profile_scope!("name")`.
    #[must_use = "zone closes on drop — bind to a variable"]
    pub struct TracyZone {
        #[allow(dead_code)]
        inner: Option<tracy_client::Span>,
    }

    impl TracyZone {
        #[doc(hidden)]
        #[inline]
        pub fn new_static(name: &'static str, _callstack: u16) -> Self {
            let inner = Client::is_running().then(|| client().span_alloc(Some(name), "", "", 0, 0));
            Self { inner }
        }

        #[doc(hidden)]
        #[inline]
        pub fn new_dynamic(name: &str, function: &str, file: &str, line: u32) -> Self {
            let inner = Client::is_running()
                .then(|| client().span_alloc(Some(name), function, file, line, 0));
            Self { inner }
        }
    }

    /// Mark end of a frame for Tracy's frame-time graphs.
    #[inline]
    pub fn frame_mark() {
        if Client::is_running() {
            tracy_client::frame_mark();
        }
    }

    /// Emit a plot data point for a time-series graph.
    /// Note: plot name must be a compile-time literal for full Tracy support.
    /// Currently emits a message with the value instead (Tracy plot API
    /// requires compile-time names for full fidelity).
    #[inline]
    pub fn plot(name: &str, value: f64) {
        if Client::is_running() {
            // Tracy's plot system requires compile-time PlotName constants.
            // Emit as a formatted message for now; full plot integration
            // requires plot_name! macro usage at call sites.
            let text = format!("{} = {:.2}", name, value);
            client().message(&text, 0);
        }
    }

    /// Emit a diagnostic message.
    #[inline]
    pub fn message(msg: Arguments<'_>) {
        if Client::is_running() {
            let text = format!("{}", msg);
            client().message(&text, 0);
        }
    }

    /// Set the display name of the current thread.
    #[inline]
    pub fn set_thread_name(name: &str) {
        if Client::is_running() {
            let c_name = std::ffi::CString::new(name).unwrap_or_default();
            unsafe {
                tracy_client::internal::set_thread_name(c_name.as_ptr().cast());
            }
        }
    }
}

// ============================================================================
// Disabled (no-op) implementation
// ============================================================================

#[cfg(not(feature = "tracy"))]
mod enabled {
    use std::fmt::Arguments;

    #[must_use = "zone closes on drop — bind to a variable"]
    pub struct TracyZone;

    impl TracyZone {
        #[doc(hidden)]
        #[inline(always)]
        pub fn new_static(_name: &'static str, _callstack: u16) -> Self {
            Self
        }

        #[doc(hidden)]
        #[inline(always)]
        pub fn new_dynamic(_name: &str, _function: &str, _file: &str, _line: u32) -> Self {
            Self
        }
    }

    #[inline(always)]
    pub fn init() {}
    #[inline(always)]
    pub fn frame_mark() {}
    #[inline(always)]
    pub fn plot(_name: &str, _value: f64) {}
    #[inline(always)]
    pub fn message(_msg: Arguments<'_>) {}
    #[inline(always)]
    pub fn set_thread_name(_name: &str) {}
}

pub use enabled::*;

// ============================================================================
// Public macros — the only API call sites use
// ============================================================================

/// Initializes the Tracy profiler client. Call once at startup.
/// Idempotent and safe to call even when the `tracy` feature is disabled.
#[macro_export]
macro_rules! profile_init {
    () => {
        $crate::profiling::init();
    };
}

/// Creates a scoped CPU profiling zone with a **static** (compile-time) name.
///
/// Preferred form. Zero heap allocation — the name is embedded in the binary.
///
/// ```ignore
/// fn movement_system(query: Query<...>) {
///     let _zone = profile_scope!("movement");
///     for (pos, vel) in query.iter_mut() { ... }
/// }
/// ```
#[macro_export]
macro_rules! profile_scope {
    ($name:literal) => {
        $crate::profiling::TracyZone::new_static($name, 0)
    };
}

/// Creates a scoped CPU profiling zone with a **dynamic** (runtime) name.
///
/// For system names, entity IDs, etc. Slightly more expensive (allocates).
///
/// ```ignore
/// let _zone = profile_scope_dyn!(registered.name);
/// ```
#[macro_export]
macro_rules! profile_scope_dyn {
    ($name:expr) => {
        $crate::profiling::TracyZone::new_dynamic($name, module_path!(), file!(), line!())
    };
}

/// Marks the end of a frame. Call once per frame loop iteration.
#[macro_export]
macro_rules! profile_frame_mark {
    () => {
        $crate::profiling::frame_mark();
    };
}

/// Emits a single data point on a named time-series plot.
///
/// ```ignore
/// profile_plot!("entity_count", world.entity_count() as f64);
/// ```
#[macro_export]
macro_rules! profile_plot {
    ($name:expr, $value:expr) => {
        $crate::profiling::plot($name, $value as f64);
    };
}

/// Emits a diagnostic message at the current point in the trace.
///
/// ```ignore
/// profile_message!("archetype {:?} created", id);
/// ```
#[macro_export]
macro_rules! profile_message {
    ($($arg:tt)*) => {
        $crate::profiling::message(format_args!($($arg)*));
    };
}

/// Sets the display name of the current thread in Tracy.
///
/// ```ignore
/// profile_thread!("main");
/// ```
#[macro_export]
macro_rules! profile_thread {
    ($name:expr) => {
        $crate::profiling::set_thread_name($name);
    };
}
