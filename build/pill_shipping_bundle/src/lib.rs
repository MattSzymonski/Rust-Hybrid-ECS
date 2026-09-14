//! Committed stub for the generated shipping bundle.
//!
//! The real bundle declares the project and every optional module its
//! `project_settings.yaml` selects, and exposes the `static_project()` entry
//! point `pill_standalone` calls under `static_project` / `static_csharp`.
//! It is written by `devops/tools/generate_shipping_bundle.py`, which
//! overwrites this file.
//!
//! This placeholder exists purely so the path dependency resolves. Cargo reads
//! every workspace manifest before it evaluates features, so without a
//! directory here even a plain debug build fails - see the note in
//! `pill_standalone/Cargo.toml`. Nothing links this stub: the features that
//! would pull the bundle in are off in a dev build, and a shipping build has
//! replaced the file by the time they are on.
//!
//! It is intentionally empty rather than a no-op `static_project()`: a
//! shipping build that somehow reached this stub should fail to compile with
//! a missing-function error, not silently launch with no project.
