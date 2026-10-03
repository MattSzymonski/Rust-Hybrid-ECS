//! The startup build plan: which modules the host builds before the project
//! runs, announced up front and counted off as each build starts.
//!
//! # Responsibilities
//!
//! - Log that hot reloading of a project is starting, with the numbered list
//!   of modules the host is about to build, as one multi-line entry
//!   ([`announce_plan`]).
//! - Log `[n/m] Building of <module> started` when one of those builds begins
//!   ([`announce_build`]), once per module.
//!
//! # Design
//!
//! The plan is process-wide because its builds start in different places: the
//! extensions and the project in `setup`, the renderer's GPU module only when a
//! window opens. A build of a module that is not (or no longer) in the plan -
//! every reload - is not announced here, and its caller logs it as before.

// Standard library
use std::sync::Mutex;

// External crates
use pill_core::info;
use pill_core::telemetry::{log_block, telemetry_target::HOT_RELOAD};

// =============================================================================
// Types
// =============================================================================

/// One module the startup will build, and what it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PlannedBuild {
    /// The module's name, as its build reports it.
    pub(crate) module: String,
    /// What it is: `extension`, `project` or `renderer`.
    pub(crate) kind: &'static str,
}

/// The startup builds not yet started, and how many the plan had.
struct BuildPlan {
    total: usize,
    started: usize,
    pending: Vec<PlannedBuild>,
}

/// The current plan; `None` before [`announce_plan`] and between runs.
static PLAN: Mutex<Option<BuildPlan>> = Mutex::new(None);

// =============================================================================
// Free Functions
// =============================================================================

/// Record the startup build plan and log it, as one multi-line entry.
///
/// `project_name` is the project's display name. Replaces any earlier plan.
pub(crate) fn announce_plan(project_name: &str, builds: Vec<PlannedBuild>) {
    info!(target: HOT_RELOAD, "{}", render_plan(project_name, &builds));
    let mut plan = PLAN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    *plan = Some(BuildPlan {
        total: builds.len(),
        started: 0,
        pending: builds,
    });
}

/// Log that the build of `module` is starting, when it is part of the startup
/// plan and has not started yet.
///
/// Returns `false` for any other build (every reload), so the caller logs it
/// its usual way instead.
pub(crate) fn announce_build(module: &str) -> bool {
    let mut plan = PLAN.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    let Some(current) = plan.as_mut() else {
        return false;
    };
    let Some(position) = current
        .pending
        .iter()
        .position(|build| build.module == module)
    else {
        return false;
    };
    current.pending.remove(position);
    current.started += 1;
    // Two blank lines set each build apart from the cargo output above it.
    // Printed to the terminal directly: an empty log event would still carry
    // the time, level and target.
    println!("\n");
    info!(
        target: HOT_RELOAD,
        "{}",
        render_step(current.started, current.total, module)
    );
    // Once every planned build has started, later builds are reloads.
    if current.pending.is_empty() {
        *plan = None;
    }
    true
}

/// The message [`announce_plan`] logs: a heading and the numbered modules.
fn render_plan(project_name: &str, builds: &[PlannedBuild]) -> String {
    let name_width = builds
        .iter()
        .map(|build| build.module.len())
        .max()
        .unwrap_or(0);
    // Numbered like the build steps that follow (`[n/total]`), with `n` padded
    // to the width of the total so the names line up.
    let total = builds.len();
    let number_width = total.to_string().len();
    let lines = builds.iter().enumerate().map(|(index, build)| {
        format!(
            "[{:>number_width$}/{total}] {:<name_width$}  {}",
            index + 1,
            build.module,
            build.kind
        )
    });
    log_block(
        &format!("Hot reloading of {project_name} is starting. Modules to build:"),
        lines,
    )
}

/// The `[n/m] Building of <module> started` message.
fn render_step(started: usize, total: usize, module: &str) -> String {
    format!("[{started}/{total}] ------------- Building of {module} started")
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;

    fn planned(module: &str, kind: &'static str) -> PlannedBuild {
        PlannedBuild {
            module: module.to_string(),
            kind,
        }
    }

    /// The banner names the project and lists every module, numbered in
    /// build order.
    #[test]
    fn the_plan_lists_every_module_in_order() {
        let text = render_plan(
            "Bouncing Balls",
            &[
                planned("pill_spline", "extension"),
                planned("project", "project"),
            ],
        );

        assert!(
            text.contains("Hot reloading of Bouncing Balls is starting"),
            "{text}"
        );
        let spline = text
            .find("[1/2] pill_spline")
            .expect("first module numbered 1");
        let project = text
            .find("[2/2] project")
            .expect("second module numbered 2");
        assert!(spline < project, "{text}");
    }

    /// Planned builds are counted off once each, in the order they start;
    /// anything else, and a second build of the same module, is left to the
    /// caller. The plan ends when its last build starts.
    #[test]
    fn planned_builds_are_announced_once_and_the_plan_then_ends() {
        announce_plan(
            "Test",
            vec![
                planned("module_a", "extension"),
                planned("module_b", "project"),
            ],
        );

        assert!(!announce_build("not_planned"));
        assert!(announce_build("module_b"));
        assert!(!announce_build("module_b"), "a reload of the same module");
        assert!(announce_build("module_a"));
        assert!(
            !announce_build("module_a"),
            "the plan is over once every build started"
        );
        assert_eq!(
            render_step(1, 4, "pill_spline"),
            "[1/4] Building of ------------- pill_spline started"
        );
    }
}
