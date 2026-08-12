//! Game-module configuration shared by every host frontend.
//!
//! # Responsibilities
//!
//! - Describes how a game module is built, watched, and loaded.
//! - Provides the standard Rust, C#, and integration-test configurations.
//! - Selects a configuration from the host process environment.

// =============================================================================
// Types
// =============================================================================

/// Backend-specific output information for a hot-reloadable game module.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub enum GameModuleBackend {
    /// A native shared library exporting `game_init` and `game_update`.
    NativeLibrary {
        /// Library name without the platform prefix or suffix.
        library_name: &'static str,
        /// Output subdirectory relative to the workspace root.
        output_subdirectory: &'static str,
    },
    /// A managed game assembly loaded through the stable `csharp_runtime` host.
    CSharp(CSharpModuleConfig),
}

/// Output locations and assembly names used by the managed game backend.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct CSharpModuleConfig {
    /// Name of the runtime assembly that hosts the collectible loader.
    pub runtime_assembly_name: &'static str,
    /// Output subdirectory for the runtime assembly, relative to the workspace root.
    pub runtime_output_subdirectory: &'static str,
    /// Name of the game assembly loaded by the runtime.
    pub game_assembly_name: &'static str,
    /// Output subdirectory for the game assembly, relative to the workspace root.
    pub game_output_subdirectory: &'static str,
}

/// Configuration for a hot-reloadable game module.
///
/// Describes how to build the module, where to find its output, and which
/// source directories to watch. Change the fields here to support Rust, C,
/// C++, Zig, or any other language that produces a compatible game module.
///
/// `#[non_exhaustive]` allows new fields to be added without breaking
/// frontends; prefer the provided constructors over struct literals.
#[non_exhaustive]
#[derive(Debug, Clone)]
pub struct GameModuleConfig {
    /// Human-readable name used in log messages.
    pub name: &'static str,

    /// Directory to watch for source changes, relative to the workspace root.
    pub watch_directory: &'static str,

    /// Build command whose first element is the program and rest are arguments.
    pub build_command: &'static [&'static str],

    /// How the built module is loaded and executed.
    pub backend: GameModuleBackend,
}

impl GameModuleConfig {
    /// Verify that the configuration is internally consistent.
    ///
    /// # Errors
    ///
    /// Returns a description of the first invalid field.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.name.is_empty() {
            return Err("name must not be empty");
        }
        if self.watch_directory.is_empty() {
            return Err("watch_directory must not be empty");
        }
        if self.build_command.is_empty() {
            return Err("build_command must not be empty");
        }
        Ok(())
    }

    /// Default configuration for a Rust `cdylib` game module built with Cargo.
    ///
    /// When the host is built with the `rendering` feature, the game module is
    /// built with the same feature so both sides share renderer components.
    #[cfg(not(feature = "rendering"))]
    pub const fn rust_default() -> Self {
        Self {
            name: "game-rs",
            watch_directory: "game_rs/src",
            build_command: &["cargo", "build", "--package", "game"],
            backend: GameModuleBackend::NativeLibrary {
                library_name: "game",
                output_subdirectory: "target/debug",
            },
        }
    }

    /// See the non-`rendering` variant above.
    #[cfg(feature = "rendering")]
    pub const fn rust_default() -> Self {
        Self {
            name: "game-rs",
            watch_directory: "game_rs/src",
            build_command: &[
                "cargo",
                "build",
                "--package",
                "game",
                "--features",
                "rendering",
            ],
            backend: GameModuleBackend::NativeLibrary {
                library_name: "game",
                output_subdirectory: "target/debug",
            },
        }
    }

    /// Default scheduler-integrated C# game loaded through `csharp_runtime`.
    pub const fn csharp_default() -> Self {
        Self {
            name: "game-csharp",
            watch_directory: "game_cs/src",
            build_command: &[
                "dotnet",
                "build",
                "game_cs/game_cs.csproj",
                "-c",
                "Release",
                "--nologo",
            ],
            backend: GameModuleBackend::CSharp(CSharpModuleConfig {
                runtime_assembly_name: "csharp_runtime",
                runtime_output_subdirectory: "csharp_runtime/bin/Release/net8.0",
                game_assembly_name: "game_cs",
                game_output_subdirectory: "game_cs/bin/Release/net8.0",
            }),
        }
    }

    /// Configuration for the dedicated integration-test game crate.
    pub const fn tests_game() -> Self {
        Self {
            name: "tests-game",
            watch_directory: "tests/game/src",
            build_command: &["cargo", "build", "--manifest-path", "tests/game/Cargo.toml"],
            backend: GameModuleBackend::NativeLibrary {
                library_name: "game",
                output_subdirectory: "tests/game/target/debug",
            },
        }
    }

    /// Pick module configuration from the environment, defaulting to Rust.
    ///
    /// Unrecognized values are reported instead of silently launching the
    /// Rust module, so typos cannot hide behind a working default.
    pub fn from_environment() -> Self {
        match std::env::var("ECS_HOT_RELOAD_MODULE") {
            Ok(value) if value.eq_ignore_ascii_case("tests-game") => Self::tests_game(),
            Ok(value)
                if value.eq_ignore_ascii_case("csharp")
                    || value.eq_ignore_ascii_case("game-csharp") =>
            {
                Self::csharp_default()
            }
            Ok(value)
                if value.eq_ignore_ascii_case("rust") || value.eq_ignore_ascii_case("game-rs") =>
            {
                Self::rust_default()
            }
            Ok(value) => {
                eprintln!(
                    "[host] Unknown ECS_HOT_RELOAD_MODULE value {value:?}; using the Rust module. \
                     Expected one of: rust, game-rs, csharp, game-csharp, tests-game."
                );
                Self::rust_default()
            }
            Err(_) => Self::rust_default(),
        }
    }
}
