//! Generated shipping bundle - do not edit. Regenerated from
//! the project's `project_settings.yaml` by
//! `devops/tools/generate_shipping_bundle.py`.

use pill_host::{StaticModule, StaticProject, StaticProjectBackend};

/// Every selected optional module, in `project_settings.yaml` order.
pub const STATIC_MODULES: &[StaticModule] = &[
    StaticModule {
        name: "pill_spline",
        init: pill_spline::register,
    },
    StaticModule {
        name: "pill_dummy_math",
        init: pill_dummy_math::register,
    },
    StaticModule {
        name: "pill_dummy_text",
        init: pill_dummy_text::register,
    },
    StaticModule {
        name: "pill_dummy_color",
        init: pill_dummy_color::register,
    },
    StaticModule {
        name: "pill_dummy_timer",
        init: pill_dummy_timer::register,
    },
    StaticModule {
        name: "pill_dummy_random",
        init: pill_dummy_random::register,
    },
];

/// The project backend for this shipping project.
pub fn project_backend() -> StaticProjectBackend {
    StaticProjectBackend::Native {
        init: project::init,
    }
}

/// The complete shipping project: modules first, then the project.
pub fn static_project() -> StaticProject {
    StaticProject {
        name: "Bouncing Balls",
        backend: project_backend(),
        modules: STATIC_MODULES,
    }
}
