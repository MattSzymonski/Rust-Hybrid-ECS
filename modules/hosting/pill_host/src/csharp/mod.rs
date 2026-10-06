//! C# development tooling over the `pill_csharp_bridge` backend.
//!
//! # Responsibilities
//!
//! - Generates the C# mirror structs for the components extensions expose.
//! - Recompiles the project in-process with Roslyn on a hot reload.
//! - Pairs the bridge's [`CSharpRuntime`] with that compiler as the host's
//!   managed project, [`CSharpProject`].
//! - Re-exports the bridge items the rest of the host names, under one path.
//!
//! # Design
//!
//! Running C# - hosting .NET, the ABI, components, systems and the reload
//! poll - is the bridge's, and a C# shipping bundle links it without this
//! crate. What stays here only exists while developing: generated mirrors and
//! the compiler. The whole module is `hot_reload` only.

// Standard library
use std::path::Path;

// External crates
use pill_core::error::CSharpError;
use pill_core::platform::Instant;
use pill_engine::Engine;

// Current crate
use crate::watcher::SourceTrigger;

/// Host-side generation of the C# mirror structs for exposed module components.
mod codegen;
/// In-process Roslyn compilation of the C# project, replacing MSBuild on reload.
mod fast_compile;

// =============================================================================
// Re-exports
// =============================================================================

pub(crate) use pill_csharp_bridge::{
    accessor_operation_name, accessor_rows, exposed_components_from_names, publish_asset_exports,
    publish_mirror_methods, CSharpRuntime, ModuleExposedComponent, ResolvedFieldAccessor,
    ResolvedMirrorMethod, POLL_REJECTED, POLL_RELOADED,
};

/// Generate the C# mirror file for extension components.
pub(crate) use codegen::generate_module_components_csharp;
/// What one in-process compile attempt produced, reported to the reload path.
pub(crate) use fast_compile::FastCompileOutcome;

// =============================================================================
// CSharpProject
// =============================================================================

/// A running C# project in the reloading host: the bridge's runtime plus the
/// in-process compiler that rebuilds its assembly.
pub(crate) struct CSharpProject {
    /// The managed runtime running the project's systems.
    runtime: CSharpRuntime,
    /// The in-process Roslyn compiler, when one could be loaded.
    ///
    /// `None` leaves every reload on the ordinary `dotnet build` path, the
    /// fallback when the compiler cannot be built or loaded.
    fast_compiler: Option<fast_compile::FastCompiler>,
    /// Timing of the managed reload currently in flight, when one is armed.
    ///
    /// Armed when the frame loop consumes the source signal that starts a
    /// reload and finished when the loader swaps the new assembly in, so the
    /// log can state the whole save-to-swap span instead of the compile
    /// alone.
    reload_timing: Option<ManagedReloadTiming>,
}

impl CSharpProject {
    /// Start the project through the bridge, loading the in-process compiler
    /// into the same .NET runtime as soon as it boots.
    ///
    /// The compiler is loaded before the project starts, not after, so its
    /// background warmup overlaps startup; see [`CSharpRuntime::start`].
    ///
    /// # Errors
    ///
    /// Returns what [`CSharpRuntime::start`] returns. A compiler that cannot be
    /// loaded is not an error: it is logged, and reloads run a full build.
    pub(crate) fn start(
        engine: &mut Engine,
        workspace_root: &Path,
        config: &pill_csharp_bridge::CSharpModuleConfig,
        module_exposed: &[ModuleExposedComponent],
        mirror_methods: &[ResolvedMirrorMethod],
    ) -> Result<Self, CSharpError> {
        let mut fast_compiler = None;
        let runtime = CSharpRuntime::start(
            engine,
            workspace_root,
            config,
            module_exposed,
            mirror_methods,
            &mut |dotnet| {
                fast_compiler = fast_compile::FastCompiler::try_new(dotnet, workspace_root, config);
            },
        )?;
        Ok(Self {
            runtime,
            fast_compiler,
            reload_timing: None,
        })
    }

    /// Poll the collectible loader; see [`CSharpRuntime::poll_reload`].
    ///
    /// # Errors
    ///
    /// Returns what [`CSharpRuntime::poll_reload`] returns.
    pub(crate) fn poll_reload(&mut self, engine: &mut Engine) -> Result<u8, CSharpError> {
        self.runtime.poll_reload(engine)
    }

    /// Recompile the project in-process, when a compiler could be loaded.
    ///
    /// `None` means there is no fast path at all and the caller must build
    /// normally; a [`FastCompileOutcome::Unavailable`] means the fast path
    /// exists but cannot answer this particular reload.
    pub(crate) fn fast_compile(
        &self,
        workspace_root: &Path,
        watch_directory: &str,
    ) -> Option<FastCompileOutcome> {
        let outcome = self
            .fast_compiler
            .as_ref()?
            .compile(workspace_root, watch_directory);
        // This compile wrote the assembly itself, so the loader's next poll
        // need not wait for the file to settle.
        if matches!(outcome, FastCompileOutcome::Compiled { .. }) {
            self.runtime.notify_assembly_replaced();
        }
        Some(outcome)
    }

    /// Arm reload timing for the reload this signal starts.
    ///
    /// `trigger` is the project watcher's record of the save that fired the
    /// signal; `None` - a reload the pipeline queued itself, such as a module
    /// swap that changed the mirror surface - times from now and reports no
    /// detection delay, because there is no file save to attribute it to.
    pub(crate) fn arm_reload_timing(&mut self, trigger: Option<SourceTrigger>) {
        self.reload_timing = Some(ManagedReloadTiming {
            detected_at: trigger.map_or_else(Instant::now, |trigger| trigger.fired_at),
            detection_delay_ms: trigger.map_or(f64::NAN, |trigger| trigger.detection_delay_ms),
            triggered_at: Instant::now(),
            rebuilt: None,
        });
    }

    /// Record that the assembly rebuild finished, and how it was produced.
    ///
    /// Called on both compile routes: the in-process compiler (`"roslyn"`)
    /// and the full `dotnet build` fallback (`"msbuild"`). The build phase of
    /// the armed span ends here.
    pub(crate) fn record_assembly_rebuilt(&mut self, kind: &'static str) {
        if let Some(timing) = &mut self.reload_timing {
            timing.rebuilt = Some(RebuiltAssembly {
                at: Instant::now(),
                kind,
            });
        }
    }

    /// Drop an armed timing whose reload produced no swap.
    ///
    /// A failed or cancelled build replaces nothing; keeping the arm would
    /// attribute a later swap - whichever attempt produced it - to this dead
    /// signal.
    pub(crate) fn abandon_reload_timing(&mut self) {
        self.reload_timing = None;
    }

    /// Finish an armed timing at the swap and hand back its breakdown.
    ///
    /// `None` when no timing was armed. A swap with no recorded build - the
    /// loader picking up an assembly this host did not time - reports `NaN`
    /// for the build phase rather than inventing one.
    pub(crate) fn finish_reload_timing(&mut self) -> Option<ManagedReloadSummary> {
        let timing = self.reload_timing.take()?;
        let swapped_at = Instant::now();
        let signal_ms = milliseconds(swapped_at.duration_since(timing.detected_at));
        let queue_ms = milliseconds(timing.triggered_at.duration_since(timing.detected_at));
        let (build_ms, swap_ms, build_kind) = match timing.rebuilt {
            Some(rebuilt) => (
                milliseconds(rebuilt.at.duration_since(timing.triggered_at)),
                milliseconds(swapped_at.duration_since(rebuilt.at)),
                rebuilt.kind,
            ),
            None => (
                f64::NAN,
                milliseconds(swapped_at.duration_since(timing.triggered_at)),
                "unknown",
            ),
        };
        // From the edited file's own timestamp when the watcher could read
        // one, with the signal plus everything after it; the signal span alone
        // is all that is measured when it could not.
        let total_ms = if timing.detection_delay_ms.is_finite() {
            timing.detection_delay_ms + signal_ms
        } else {
            signal_ms
        };
        Some(ManagedReloadSummary {
            total_ms,
            detect_ms: timing.detection_delay_ms,
            queue_ms,
            build_ms,
            swap_ms,
            build_kind,
        })
    }
}

/// One managed reload's armed timing, filled in as its phases complete.
struct ManagedReloadTiming {
    /// Instant the project watcher fired the signal this reload came from.
    detected_at: Instant,
    /// The save-to-signal delay the watcher measured, debounce included; `NaN`
    /// when the trigger carried none.
    detection_delay_ms: f64,
    /// Instant the frame loop consumed the signal and armed this timing.
    triggered_at: Instant,
    /// The finished assembly, once a build route recorded it.
    rebuilt: Option<RebuiltAssembly>,
}

/// A finished assembly rebuild: when it was ready and what produced it.
struct RebuiltAssembly {
    /// Instant the rebuild returned.
    at: Instant,
    /// How the assembly was produced: `"roslyn"` in-process, `"msbuild"` the
    /// full `dotnet build` fallback.
    kind: &'static str,
}

/// A finished managed reload, broken down by phase, in milliseconds.
pub(crate) struct ManagedReloadSummary {
    /// Save to swap: the whole operation. Measured from the edited file's own
    /// timestamp when the watcher could read one, else from the signal - and
    /// then `detect_ms` is `NaN`.
    pub(crate) total_ms: f64,
    /// Save to signal, debounce included; `NaN` when unmeasured.
    pub(crate) detect_ms: f64,
    /// Signal to the frame loop consuming it.
    pub(crate) queue_ms: f64,
    /// Consume to rebuilt assembly; `NaN` when no rebuild was recorded.
    pub(crate) build_ms: f64,
    /// Rebuilt assembly to the loader's swap.
    pub(crate) swap_ms: f64,
    /// What produced the assembly: `"roslyn"` or `"msbuild"`.
    pub(crate) build_kind: &'static str,
}

/// `duration` in milliseconds.
fn milliseconds(duration: std::time::Duration) -> f64 {
    duration.as_secs_f64() * 1000.0
}
