//! Generated shipping bundle - do not edit. Regenerated from
//! the project's `project_settings.yaml` by
//! `devops/tools/generate_shipping_bundle.py`.
//!
//! # Responsibilities
//!
//! - Link the project, its modules and its renderer into one binary, and
//!   describe them as the `StaticProject` the shipping frontends run.

use pill_runtime::{
    StaticLogging, StaticModule, StaticProject, StaticProjectBackend, StaticRenderer,
};

/// Every selected extension: the renderer's data crate first, then
/// `project_settings.yaml` order.
#[rustfmt::skip]
pub const STATIC_MODULES: &[StaticModule] = &[
    StaticModule {
        name: "pill_master_renderer_data",
        init: pill_master_renderer_data::register,
    },
];

/// The project backend for this shipping project.
pub fn project_backend() -> StaticProjectBackend {
    StaticProjectBackend::Native {
        init: italian_brainrot::init,
    }
}

/// The renderer this binary links, or `None` for a headless build.
pub fn static_renderer() -> Option<StaticRenderer> {
    Some(StaticRenderer {
        init: pill_master_renderer::register,
        attach: pill_master_renderer::attach,
    })
}

/// The `logging:` section of `project_settings.yaml`.
#[rustfmt::skip]
const LOGGING: StaticLogging = StaticLogging::NONE;

/// The complete shipping project: modules first, then the project.
pub fn static_project() -> StaticProject {
    StaticProject {
        name: "Italian Brainrot",
        backend: project_backend(),
        modules: STATIC_MODULES,
        renderer: static_renderer(),
        asset_pack: Some(include_bytes!(concat!(env!("OUT_DIR"), "/assets.pillpack"))),
        logging: LOGGING,
    }
}
