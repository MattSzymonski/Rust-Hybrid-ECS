#!/usr/bin/env python3

# REQUIREMENTS: Python 3.8+, PyYAML installed, Rust toolchain (cargo) on PATH.
#               Run from anywhere; the repository root is located from this
#               script.

# DESCRIPTION: Generates the shipping bundle crate for the static (shipping)
#   posture of the engine host. The bundle is the single crate `pill_standalone`
#   links under `shipping`: it declares the project and every extension
#    selected by the project's `project_settings.yaml` as ordinary Rust
#   dependencies, and exposes the `StaticModule` / `StaticProject` registration
#   the static-link path initializes.
#
#   The project's scripting language is read from its manifest: a `Cargo.toml`
#   is a native Rust project, a `*.csproj` is a managed C# project. A managed
#   project has no cargo dependency to declare; its `project_backend()` instead
#   returns an external backend, the C# bridge's `CSharpBackend::coreclr`,
#   resolving the assemblies `dotnet build` produced against the engine
#   workspace root. With `--csharp-aot` it is `CSharpBackend::native_aot` and the
#   project subdirectory points at the `dotnet publish` NativeAOT output, which
#   the runtime loads directly (no hostfxr, no installed .NET).
#
#   Cargo resolves dependencies before build scripts run, so a `build.rs`
#   cannot pull modules in from a YAML file; this generator runs before cargo
#   instead (as a pre-step of the release build) and writes the bundle crate
#   under `<repository_root>/build/pill_shipping_bundle/`. The location is
#   project-agnostic (cargo needs a static path, and only one shipping binary
#   is built at a time), so `pill_standalone`'s manifest never names a specific
#   project.
#
#   A project with a `res` directory also gets a `build.rs` in the bundle: it
#   packs that directory with `pill_assets::write_asset_pack` at build time, and
#   the bundle embeds the pack as `StaticProject::asset_pack`, which the
#   runtime mounts in the engine's asset store. A shipped game then reads its
#   assets from its own binary, wherever it runs.
#
#   With `--web` it also writes the web app crate, `build/pill_web_app`: a
#   `cdylib` whose `#[wasm_bindgen(start)]` shim runs the bundle's project
#   through `pill_web` on the page's canvas. It has a `[workspace]` of its own,
#   so the engine workspace's native feature choices and `.cargo` settings stay
#   out of the wasm build. `devops/tools/build_web.py` builds it. The host links it by path (like the project itself), so no
#   workspace edit is needed; the folder is gitignored, and regeneration is
#   content-based: unchanged output is not rewritten, so a stable tree shows
#   no diff.
#
# USAGE: python devops/tools/generate_shipping_bundle.py [--feature <name>...]
#                                                     [project_path]
#          --feature <name>  project feature to enable on the `project`
#                            dependency in the bundle (repeatable; names the
#                            project does not declare are ignored). The
#                            release build forwards its requested features,
#                            so e.g. `--feature rendering` makes the static
#                            build link the project's renderer components.
#          --csharp-aot      emit the NativeAOT backend (`native_aot`) instead
#                            of the hostfxr one, for a managed project whose
#                            assembly was published with `dotnet publish
#                            -p:PublishAot=true`.
#          --rid <rid>       runtime identifier for the NativeAOT publish
#                            output path (default win-x64; only meaningful
#                            with --csharp-aot).
#          --web             also write the web app crate for a browser build
#                            (implies `--feature rendering`; Rust projects only).
#          [project_path]    workspace-relative path to the project directory
#                            (e.g. examples/project_rs). When omitted, the
#                            PROJECT_PATH environment variable is used - the
#                            same resolution the host uses at startup.
#
# EXAMPLE USAGE:
#   python devops/tools/generate_shipping_bundle.py examples/project_rs
#   python devops/tools/generate_shipping_bundle.py --feature rendering examples/project_rs
#   python devops/tools/generate_shipping_bundle.py examples/project_cs
#   python devops/tools/generate_shipping_bundle.py --web examples/master_renderer_test
#   set PROJECT_PATH=examples/project_rs
#   python devops/tools/generate_shipping_bundle.py

# --- SCRIPT ---

# Standard library
import os
import sys
from pathlib import Path

# Third-party
import yaml

# The generated crate's name and its location relative to the REPOSITORY root.
# The location is shared by every project, so `pill_standalone`'s manifest
# path stays stable; only the contents change per project.
BUNDLE_CRATE_NAME = "pill_shipping_bundle"
BUNDLE_DIRECTORY = Path("build") / BUNDLE_CRATE_NAME

# File names the generator reads and writes.
PROJECT_SETTINGS_FILE_NAME = "project_settings.yaml"
PROJECT_MANIFEST_FILE_NAME = "Cargo.toml"
EXTENSION_DIRECTORY = Path("modules") / "extensions"
# The crates a bundle names: the runtime always (the static-link types), and the
# C# bridge for a managed project (its external project backend). Never the
# development host - a shipping build does not compile it at all.
RUNTIME_CRATE_DIRECTORY = Path("modules") / "hosting" / "pill_runtime"
CSHARP_BRIDGE_CRATE_DIRECTORY = Path("modules") / "csharp" / "pill_csharp_bridge"
# The renderer, chosen by the project's `renderer:` setting exactly as the host
# chooses it (`pill_host::config`): absent means the master renderer, `none`
# means no renderer. Its data crate, `<renderer>_data`, is linked as the first
# static module in every shipping build, headless included; a windowed build
# also links the GPU crate `<renderer>` and hands the host its entry points as
# `StaticRenderer`.
DEFAULT_RENDERER = "pill_master_renderer"
NO_RENDERER = "none"
RENDERER_DATA_SUFFIX = "_data"
# The requested feature that makes a shipping build windowed.
RENDERING_FEATURE = "rendering"
# The project's asset directory, packed into the binary when it exists, and the
# crate whose build-time writer packs it.
ASSETS_DIRECTORY_NAME = "res"
ASSETS_CRATE_DIRECTORY = Path("modules") / "pill_assets"
ASSET_PACK_FILE_NAME = "assets.pillpack"
BUILD_SCRIPT_FILE_NAME = "build.rs"
# The web app crate `--web` writes, the frontend it runs on, and the id of the
# canvas its page provides (`devops/tools/web/index.html`).
WEB_APP_CRATE_NAME = "pill_web_app"
WEB_APP_DIRECTORY = Path("build") / WEB_APP_CRATE_NAME
WEB_CRATE_DIRECTORY = Path("modules") / "frontents" / "pill_web"
WEB_CANVAS_ID = "pill-canvas"

# Managed (C#) project constants, mirroring `pill_host::config` so a generated
# bundle resolves assemblies exactly where `dotnet build` produced them. The
# host derives the same four values in `csharp_from_manifest`; a shipping build
# has no project path to read, so the generator states them instead.
CSHARP_RUNTIME_ASSEMBLY_NAME = "csharp_runtime"
CSHARP_RUNTIME_OUTPUT_SUBDIRECTORY = "csharp/pill_csharp_runtime/bin/Release/net8.0"
CSHARP_TARGET_FRAMEWORK = "net8.0"
# The engine workspace root, against which the managed config's output
# subdirectories are resolved (the workspace manifest globs `modules/*`).
WORKSPACE_DIRECTORY = Path("modules")


def repository_root() -> Path:
    """Returns the repository root, three levels above this script."""
    return Path(__file__).resolve().parent.parent.parent


def load_module_list(project_root: Path) -> list:
    """Loads the extension list from the project's settings file.

    Raises FileNotFoundError when the settings file is missing.
    """
    settings_path = project_root / PROJECT_SETTINGS_FILE_NAME
    if not settings_path.is_file():
        raise FileNotFoundError(
            f"no {PROJECT_SETTINGS_FILE_NAME} in project root {project_root}"
        )
    with settings_path.open(encoding="utf-8") as handle:
        data = yaml.safe_load(handle) or {}
    modules = data.get("modules") or []
    return [str(name) for name in modules]


def load_renderer(project_root: Path):
    """Returns the renderer the project selects, or None for `renderer: none`.

    Reads the settings file's `renderer:` key; absent means DEFAULT_RENDERER.
    """
    settings_path = project_root / PROJECT_SETTINGS_FILE_NAME
    with settings_path.open(encoding="utf-8") as handle:
        data = yaml.safe_load(handle) or {}
    name = str(data.get("renderer") or DEFAULT_RENDERER).strip()
    return None if name == NO_RENDERER else name


# The log levels the `logging:` section accepts, as `pill_runtime::LoggingSettings`
# reads them.
LOG_LEVELS = {"off", "error", "warn", "info", "debug", "trace"}

# The timestamp formats `logging: timestamp:` accepts, as
# `pill_core::telemetry::TimestampFormat` reads them.
TIMESTAMP_FORMATS = {"date_time", "time"}


def check_log_level(settings_path: Path, where: str, level) -> str:
    """Returns `level` lowercased, or raises ValueError naming `where`."""
    text = str(level).strip().lower()
    if text not in LOG_LEVELS:
        raise ValueError(
            f"{settings_path}: `logging:` {where} has level `{level}`; use "
            "off, error, warn, info, debug or trace"
        )
    return text


def load_logging(project_root: Path):
    """Loads the `logging:` section as
    (level, timestamp, source_location, [(target, level), ...]).

    `level`, `timestamp` and `source_location` are None when the section does
    not set them.

    Validated here exactly as the development host validates it, so the
    generated bundle only ever carries settings the runtime can read. Raises
    ValueError on an unknown key, level, timestamp format, or target.
    """
    settings_path = project_root / PROJECT_SETTINGS_FILE_NAME
    with settings_path.open(encoding="utf-8") as handle:
        data = yaml.safe_load(handle) or {}
    section = data.get("logging") or {}
    if not isinstance(section, dict):
        raise ValueError(f"{settings_path}: `logging:` must be a mapping")
    unknown = set(section) - {"level", "timestamp", "source_location", "targets"}
    if unknown:
        raise ValueError(f"{settings_path}: `logging:` has unknown keys {sorted(unknown)}")
    level = section.get("level")
    level = None if level is None else check_log_level(settings_path, "`level`", level)
    timestamp = section.get("timestamp")
    if timestamp is not None:
        timestamp = str(timestamp).strip().lower()
        if timestamp not in TIMESTAMP_FORMATS:
            raise ValueError(
                f"{settings_path}: `logging:` has timestamp `{section['timestamp']}`; "
                "use date_time or time"
            )
    source_location = section.get("source_location")
    if source_location is not None and not isinstance(source_location, bool):
        raise ValueError(
            f"{settings_path}: `logging:` has source_location `{source_location}`; "
            "use true or false"
        )
    targets = []
    for target, target_level in (section.get("targets") or {}).items():
        target = str(target)
        if not target or not all(c.isascii() and (c.isalnum() or c in "_-.:") for c in target):
            raise ValueError(f"{settings_path}: `logging:` target `{target}` is not a log target")
        targets.append((target, check_log_level(settings_path, f"target `{target}`", target_level)))
    # The host reads the targets into a sorted map; the bundle keeps that order.
    targets.sort()
    return level, timestamp, source_location, targets


def build_logging_literal(logging) -> str:
    """Renders the `StaticLogging` expression for the loaded `logging:` section."""
    level, timestamp, source_location, targets = logging
    if level is None and timestamp is None and source_location is None and not targets:
        return "StaticLogging::NONE"
    level_text = "None" if level is None else f'Some("{level}")'
    timestamp_text = "None" if timestamp is None else f'Some("{timestamp}")'
    location_text = "None" if source_location is None else f"Some({str(source_location).lower()})"
    pairs = ", ".join(f'("{target}", "{target_level}")' for target, target_level in targets)
    return (
        f"StaticLogging {{ level: {level_text}, timestamp: {timestamp_text}, "
        f"source_location: {location_text}, targets: &[{pairs}] }}"
    )


def modules_with_renderer_data(renderer, modules: list) -> list:
    """Puts the renderer's data crate first in the module list, once."""
    if renderer is None:
        return list(modules)
    data = renderer + RENDERER_DATA_SUFFIX
    return [data] + [name for name in modules if name.strip() != data]


def load_project_name(project_root: Path) -> str:
    """Loads the required project `name` from the settings file.

    The name drives the artifact filename and the window title, so a settings
    file without one is an error, not a silent default.

    Raises FileNotFoundError when the settings file is missing, ValueError when
    it does not declare a `name`.
    """
    settings_path = project_root / PROJECT_SETTINGS_FILE_NAME
    if not settings_path.is_file():
        raise FileNotFoundError(
            f"no {PROJECT_SETTINGS_FILE_NAME} in project root {project_root}"
        )
    with settings_path.open(encoding="utf-8") as handle:
        data = yaml.safe_load(handle) or {}
    name = str(data.get("name") or "").strip()
    if not name:
        raise ValueError(
            f"{settings_path} must declare a `name` (used for the window title)"
        )
    return name


def load_build_binary_name(project_root: Path) -> str:
    """Loads the required `build_binary_name` from the settings file.

    It is the artifact file base, so it must contain only letters, digits and
    underscores (no spaces or special characters). Raises FileNotFoundError
    when the settings file is missing, ValueError when it is missing/invalid.
    """
    settings_path = project_root / PROJECT_SETTINGS_FILE_NAME
    if not settings_path.is_file():
        raise FileNotFoundError(
            f"no {PROJECT_SETTINGS_FILE_NAME} in project root {project_root}"
        )
    with settings_path.open(encoding="utf-8") as handle:
        data = yaml.safe_load(handle) or {}
    value = str(data.get("build_binary_name") or "").strip()
    if not value or not all(
        c.isascii() and (c.isalnum() or c == "_") for c in value
    ):
        raise ValueError(
            f"{settings_path} must declare a `build_binary_name` with only "
            "letters, digits and underscores (no spaces or special characters)"
        )
    return value


def load_project_package_name(project_root: Path) -> str:
    """Reads the project package name from its Cargo.toml.

    The manifest is scanned line-by-line for the `[package]` table's `name`,
    which is all the generator needs; no TOML parser is required.
    """
    manifest_path = project_root / PROJECT_MANIFEST_FILE_NAME
    if not manifest_path.is_file():
        raise FileNotFoundError(
            f"no {PROJECT_MANIFEST_FILE_NAME} in project root {project_root}"
        )
    in_package_table = False
    for line in manifest_path.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_package_table = stripped == "[package]"
            continue
        if in_package_table and stripped.startswith("name"):
            value = stripped.split("=", 1)[1].strip().strip('"').strip("'")
            return value
    raise ValueError(f"no package name in {manifest_path}")


def load_project_features(project_root: Path) -> set:
    """Reads the feature names the project declares in its Cargo.toml [features].

    The bundle can enable a project feature only when the project declares it,
    so the generator filters the requested names against this table. Parsed
    line-by-line like the package name; no TOML parser is required. A managed
    project declares no cargo features, so this is only called for native ones.
    """
    manifest_path = project_root / PROJECT_MANIFEST_FILE_NAME
    if not manifest_path.is_file():
        raise FileNotFoundError(
            f"no {PROJECT_MANIFEST_FILE_NAME} in project root {project_root}"
        )
    in_features_table = False
    features = set()
    for line in manifest_path.read_text(encoding="utf-8").splitlines():
        stripped = line.strip()
        if stripped.startswith("["):
            in_features_table = stripped == "[features]"
            continue
        if in_features_table and stripped and not stripped.startswith("#"):
            name = stripped.split("=", 1)[0].strip().strip('"').strip("'")
            if name:
                features.add(name)
    return features


def find_csproj_manifest(project_root: Path) -> Path:
    """Locates the single `.csproj` manifest inside a project directory.

    Raises ValueError when the directory contains no `.csproj` file.
    """
    matches = sorted(project_root.glob("*.csproj"))
    if not matches:
        raise ValueError(f"no .csproj file found in project root {project_root}")
    return matches[0]


def project_kind(project_root: Path) -> str:
    """Returns 'native' for a Cargo crate, 'managed' for a dotnet-built project.

    A project declares its scripting language by its manifest: a `Cargo.toml`
    is native, a `*.csproj` is managed. Anything else is an error, because the
    generator cannot name a project it cannot build.
    """
    if (project_root / PROJECT_MANIFEST_FILE_NAME).is_file():
        return "native"
    if find_csproj_manifest(project_root):
        return "managed"
    raise ValueError(
        f"project root {project_root} has neither {PROJECT_MANIFEST_FILE_NAME} "
        "nor a .csproj file; a shipping project must be one or the other"
    )


def manifest_relative_path(from_directory: Path, target: Path) -> str:
    """Computes a Cargo `path` dependency relative to `from_directory`, using /."""
    return os.path.relpath(target, from_directory).replace(os.sep, "/")


def build_cargo_manifest(
    bundle_directory: Path,
    project_root: Path,
    package_name: str,
    modules: list,
    project_features: set,
    root: Path,
    managed: bool = False,
    renderer=None,
    packs_assets: bool = False,
) -> str:
    """Builds the generated bundle's Cargo.toml text.

    `renderer` is the GPU crate a windowed build links, or None.
    `packs_assets` adds the asset packer the bundle's build script runs.
    """
    lines = [
        "[package]",
        f'name = "{BUNDLE_CRATE_NAME}"',
        'version = "0.0.0"',
        'edition = "2021"',
        # Without assets there is no build script. Saying so explicitly makes
        # cargo ignore a `build.rs` left behind by an earlier project's bundle
        # (it is gitignored, so restoring the stub does not remove it).
        *([] if packs_assets else ["build = false"]),
        "",
        "[dependencies]",
        # The runtime, so the table can name StaticModule / StaticProjectBackend.
        "pill_runtime = { path = "
        f'"{manifest_relative_path(bundle_directory, root / RUNTIME_CRATE_DIRECTORY)}" }}',
    ]
    if managed:
        # The C# bridge, whose backend starts the managed project.
        lines.append(
            "pill_csharp_bridge = { path = "
            f'"{manifest_relative_path(bundle_directory, root / CSHARP_BRIDGE_CRATE_DIRECTORY)}" }}'
        )
    if not managed:
        # The native project itself, so `project::init` is nameable. Any
        # requested feature the project actually declares is enabled so the
        # static binary matches a dev build; cargo does not propagate the
        # host's features here. `rendering` used to be such a feature and no
        # longer is: the renderer left `pill_engine`, so a windowed ship is
        # requested on the HOST (`pill_standalone/rendering`) and the filter
        # below simply drops the name here. A managed project is a dotnet
        # assembly, so it has no cargo dependency to declare.
        # The feature list is built outside the f-string: nesting the same
        # quote character inside an f-string expression is only valid on
        # Python 3.12+, and this script supports 3.8+.
        feature_names = ", ".join(f'"{name}"' for name in sorted(project_features))
        feature_clause = f", features = [{feature_names}]" if project_features else ""
        # Keyed by the project's own package name: the generated source names
        # the crate as `<package_name>::init`, so a fixed `project` key only
        # ever worked for a project whose package happens to be called that.
        lines.append(
            f'{package_name} = {{ path = "{manifest_relative_path(bundle_directory, project_root)}"'
            + feature_clause
            + " }",
        )
    for module in modules:
        module_directory = root / EXTENSION_DIRECTORY / module
        relative_path = manifest_relative_path(bundle_directory, module_directory)
        lines.append(
            f'{module} = {{ path = "{relative_path}", default-features = false }}'
        )
    if renderer:
        # The renderer, linked statically: the bundle is the only crate that
        # depends on it for real, and hands the host its two entry points.
        # `module-abi` stays off - nothing loads this binary as a module.
        renderer_path = manifest_relative_path(
            bundle_directory, root / EXTENSION_DIRECTORY / renderer
        )
        lines.append(
            f'{renderer} = {{ path = "{renderer_path}", default-features = false }}'
        )
    if packs_assets:
        # The build script packs the project's `res` directory with it.
        assets_path = manifest_relative_path(bundle_directory, root / ASSETS_CRATE_DIRECTORY)
        lines += ["", "[build-dependencies]", f'pill_assets = {{ path = "{assets_path}" }}']
    return "\n".join(lines) + "\n"


def build_build_script(bundle_directory: Path, assets_directory: Path) -> str:
    """Builds the bundle's build.rs, which packs `assets_directory` into OUT_DIR.

    The directory is named relative to the bundle, like every dependency path,
    so the generated file does not change with the checkout's location.
    """
    relative_assets = manifest_relative_path(bundle_directory, assets_directory)
    lines = [
        "//! Generated shipping bundle build script - do not edit. Regenerated by",
        "//! `devops/tools/generate_shipping_bundle.py`.",
        "//!",
        "//! # Responsibilities",
        "//!",
        "//! - Pack the project's `res` directory into `OUT_DIR`, where `src/lib.rs`",
        "//!   embeds it as the project's asset pack.",
        "",
        "fn main() {",
        "    let manifest_directory = std::path::PathBuf::from(env!(\"CARGO_MANIFEST_DIR\"));",
        f'    let source = manifest_directory.join("{relative_assets}");',
        "    let output = std::path::PathBuf::from(std::env::var(\"OUT_DIR\").expect(\"cargo sets OUT_DIR\"))",
        f'        .join("{ASSET_PACK_FILE_NAME}");',
        "    let packed = pill_assets::write_asset_pack(&source, &output)",
        "        .unwrap_or_else(|error| panic!(\"cannot pack {}: {error}\", source.display()));",
        "    // The directory itself too, so an added or removed file repacks.",
        "    println!(\"cargo:rerun-if-changed={}\", source.display());",
        "    for file in packed {",
        "        println!(\"cargo:rerun-if-changed={}\", file.display());",
        "    }",
        "}",
    ]
    return "\n".join(lines) + "\n"


def build_library_source(
    package_name: str,
    project_name: str,
    modules: list,
    kind: str,
    project_path: str,
    bundle_directory: Path,
    workspace_root: Path,
    aot: bool = False,
    rid: str = "win-x64",
    renderer=None,
    packs_assets: bool = False,
    logging=(None, None, None, []),
) -> str:
    """Builds the generated bundle's src/lib.rs text.

    `renderer` is the GPU crate a windowed build links, or None.
    `packs_assets` embeds the asset pack the build script wrote.
    `logging` is the validated `logging:` section, from `load_logging`.
    """
    lines = [
        "//! Generated shipping bundle - do not edit. Regenerated from",
        "//! the project's `project_settings.yaml` by",
        "//! `devops/tools/generate_shipping_bundle.py`.",
        "//!",
        "//! # Responsibilities",
        "//!",
        "//! - Link the project, its modules and its renderer into one binary, and",
        "//!   describe them as the `StaticProject` the shipping frontends run.",
        "",
        "use pill_runtime::{",
        "    StaticLogging, StaticModule, StaticProject, StaticProjectBackend, StaticRenderer,",
        "};",
        "",
        "/// Every selected extension: the renderer's data crate first, then",
        "/// `project_settings.yaml` order.",
        # One entry per line regardless of count. rustfmt collapses a
        # single-element array onto one line, so without this the generated
        # file fails `cargo fmt --check` for any project selecting exactly one
        # module - a repository-wide gate failing on output nobody edits.
        "#[rustfmt::skip]",
        "pub const STATIC_MODULES: &[StaticModule] = &[",
    ]
    for module in modules:
        lines += [
            "    StaticModule {",
            f'        name: "{module}",',
            f"        init: {module}::register,",
            "    },",
        ]
    lines += [
        "];",
        "",
        "/// The project backend for this shipping project.",
    ]
    if kind == "managed":
        # The managed backend resolves its assemblies against the engine
        # workspace root, which is where `dotnet build` produced them. The
        # emitted root is that workspace expressed relative to this bundle
        # crate, so no absolute path is ever compiled in.
        workspace_relative_path = manifest_relative_path(bundle_directory, workspace_root)
        if aot:
            # The NativeAOT posture loads one published native library; the
            # project subdirectory points at the `dotnet publish` output.
            project_subdirectory = (
                f"../{project_path}/bin/Release/{CSHARP_TARGET_FRAMEWORK}/{rid}/publish"
            )
            constructor = "native_aot"
        else:
            project_subdirectory = f"../{project_path}/bin/Release/{CSHARP_TARGET_FRAMEWORK}"
            constructor = "coreclr"
        # The runtime names no C# type: the bridge's backend is handed over as
        # an external project backend, which the runtime starts after the
        # extensions and keeps alive beside the engine.
        lines += [
            "pub fn project_backend() -> StaticProjectBackend {",
            "    StaticProjectBackend::External(std::sync::Arc::new(",
            f"        pill_csharp_bridge::CSharpBackend::{constructor}(",
            "            pill_csharp_bridge::CSharpModuleConfig::new(",
            f'                "{CSHARP_RUNTIME_ASSEMBLY_NAME}",',
            f'                "{CSHARP_RUNTIME_OUTPUT_SUBDIRECTORY}",',
            f'                "{package_name}",',
            f'                "{project_subdirectory}",',
            "            ),",
            f'            std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("{workspace_relative_path}"),',
            "        ),",
            "    ))",
            "}",
        ]
    else:
        lines += [
            "pub fn project_backend() -> StaticProjectBackend {",
            "    StaticProjectBackend::Native {",
            f"        init: {package_name}::init,",
            "    }",
            "}",
        ]
    lines += [
        "",
        "/// The renderer this binary links, or `None` for a headless build.",
        "pub fn static_renderer() -> Option<StaticRenderer> {",
    ]
    if renderer:
        lines += [
            "    Some(StaticRenderer {",
            f"        init: {renderer}::register,",
            f"        attach: {renderer}::attach,",
            "    })",
        ]
    else:
        lines += ["    None"]
    lines += [
        "}",
        "",
        "/// The `logging:` section of `project_settings.yaml`.",
        # Skipped for the same reason as the module list: one generated line,
        # whatever its length, rather than whatever shape rustfmt would pick.
        "#[rustfmt::skip]",
        f"const LOGGING: StaticLogging = {build_logging_literal(logging)};",
        "",
        "/// The complete shipping project: modules first, then the project.",
        "pub fn static_project() -> StaticProject {",
        "    StaticProject {",
        # The settings display name, not the crate name: this is what the host
        # logs and what the window title carries.
        f'        name: "{project_name}",',
        "        backend: project_backend(),",
        "        modules: STATIC_MODULES,",
        "        renderer: static_renderer(),",
        (
            f'        asset_pack: Some(include_bytes!(concat!(env!("OUT_DIR"), "/{ASSET_PACK_FILE_NAME}"))),'
            if packs_assets
            else "        asset_pack: None,"
        ),
        "        logging: LOGGING,",
        "    }",
        "}",
    ]
    return "\n".join(lines) + "\n"


def build_web_app_manifest(web_app_directory: Path, root: Path) -> str:
    """Builds the web app crate's Cargo.toml text.

    The empty `[workspace]` makes the crate its own workspace root: the wasm
    build resolves features for this crate alone, and cargo does not look for
    an enclosing workspace.
    """
    web_path = manifest_relative_path(web_app_directory, root / WEB_CRATE_DIRECTORY)
    bundle_path = manifest_relative_path(web_app_directory, root / BUNDLE_DIRECTORY)
    lines = [
        "[package]",
        f'name = "{WEB_APP_CRATE_NAME}"',
        'version = "0.0.0"',
        'edition = "2021"',
        "",
        "[lib]",
        'crate-type = ["cdylib"]',
        "",
        "[dependencies]",
        f'pill_web = {{ path = "{web_path}" }}',
        f'{BUNDLE_CRATE_NAME} = {{ path = "{bundle_path}" }}',
        'wasm-bindgen = "0.2"',
        "",
        "# Its own workspace: nothing of the engine workspace's native builds takes",
        "# part in this one.",
        "[workspace]",
    ]
    return "\n".join(lines) + "\n"


def build_web_app_source() -> str:
    """Builds the web app crate's src/lib.rs: the browser entry point."""
    lines = [
        "//! Generated web app - do not edit. Regenerated by",
        "//! `devops/tools/generate_shipping_bundle.py --web`.",
        "//!",
        "//! # Responsibilities",
        "//!",
        "//! - Run the shipping bundle's project in the page's canvas when the",
        "//!   browser has instantiated this module.",
        "",
        "use wasm_bindgen::prelude::wasm_bindgen;",
        "",
        "/// The entry point the browser calls once the module is instantiated.",
        "#[wasm_bindgen(start)]",
        "pub fn start() {",
        f'    pill_web::run({BUNDLE_CRATE_NAME}::static_project(), "{WEB_CANVAS_ID}");',
        "}",
    ]
    return "\n".join(lines) + "\n"


def write_if_changed(path: Path, content: str) -> bool:
    """Writes content only when it differs; returns True when written."""
    if path.is_file() and path.read_text(encoding="utf-8") == content:
        return False
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_text(content, encoding="utf-8")
    return True


def main() -> int:
    """Generates the shipping bundle crate for the given project path.

    The project path comes from the command-line argument, or from the
    PROJECT_PATH environment variable when no argument is given - the same
    resolution the host uses at startup.
    """
    # Parse `--feature <name>` (repeatable), `--csharp-aot`, `--rid <rid>`,
    # `--web`, and the optional project path.
    arguments = sys.argv[1:]
    requested_features = []
    aot = False
    web = False
    rid = "win-x64"
    positional = []
    index = 0
    while index < len(arguments):
        argument = arguments[index]
        if argument == "--feature":
            index += 1
            if index >= len(arguments):
                print("error: --feature requires a value", file=sys.stderr)
                return 2
            requested_features.append(arguments[index])
        elif argument.startswith("--feature="):
            requested_features.append(argument[len("--feature=") :])
        elif argument == "--csharp-aot":
            aot = True
        elif argument == "--web":
            # A browser build always draws: it links the renderer.
            web = True
            requested_features.append(RENDERING_FEATURE)
        elif argument == "--rid":
            index += 1
            if index >= len(arguments):
                print("error: --rid requires a value", file=sys.stderr)
                return 2
            rid = arguments[index]
        elif argument.startswith("--rid="):
            rid = argument[len("--rid=") :]
        else:
            positional.append(argument)
        index += 1
    if len(positional) > 1:
        print(
            "usage: generate_shipping_bundle.py [--feature <name>...] [project_path]",
            file=sys.stderr,
        )
        return 2
    project_path = positional[0] if positional else os.environ.get("PROJECT_PATH", "")
    if not project_path:
        print(
            "error: no project path: pass it as an argument or set PROJECT_PATH "
            "(e.g. PROJECT_PATH=examples/project_rs).",
            file=sys.stderr,
        )
        return 1

    root = repository_root()
    # The host resolves PROJECT_PATH against the working directory, so
    # `../examples/project_rs` works from the workspace dir; the generator
    # historically resolved it against the repository root, so
    # `examples/project_rs` works from anywhere. Try the working directory
    # first, then the repository root, so both spellings work.
    cwd_candidate = (Path.cwd() / project_path).resolve()
    project_root = (
        cwd_candidate if cwd_candidate.is_dir() else (root / project_path).resolve()
    )
    if not project_root.is_dir():
        print(f"error: no project directory at {project_root}", file=sys.stderr)
        return 1
    # Canonicalize to a repository-root-relative path: the bundle locates the
    # project with it, and the managed backend's output subdirectory is derived
    # from it against the engine workspace root. Forward slashes keep the path
    # valid when it is emitted inside generated Rust string literals.
    project_path = os.path.relpath(project_root, root).replace(os.sep, "/")

    # Step 1: read the project's scripting language, module selection, package
    # name, and required display name + artifact binary name. A native project
    # names its crate in Cargo.toml; a managed project names its assembly in
    # the .csproj file stem.
    try:
        kind = project_kind(project_root)
        renderer_name = load_renderer(project_root)
        modules = modules_with_renderer_data(renderer_name, load_module_list(project_root))
        project_name = load_project_name(project_root)
        build_binary_name = load_build_binary_name(project_root)
        if kind == "managed":
            package_name = find_csproj_manifest(project_root).stem
        else:
            package_name = load_project_package_name(project_root)
    except (FileNotFoundError, ValueError, yaml.YAMLError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    if web and kind == "managed":
        print(
            "error: --web supports Rust projects only; a C# project cannot run in "
            f"a browser yet, and {project_path} is a managed (`.csproj`) project",
            file=sys.stderr,
        )
        return 1
    if aot and kind != "managed":
        print(
            "error: --csharp-aot requires a managed (`.csproj`) project; "
            f"{project_path} is a native Rust project",
            file=sys.stderr,
        )
        return 1

    # Step 2: validate every selected module exists under modules/extensions/,
    # the renderer's data crate included, and the GPU crate a windowed build
    # links.
    linked_renderer = (
        renderer_name if renderer_name and RENDERING_FEATURE in requested_features else None
    )
    missing_modules = [
        name
        for name in modules + ([linked_renderer] if linked_renderer else [])
        if not (root / EXTENSION_DIRECTORY / name).is_dir()
    ]
    if missing_modules:
        print(
            f"error: modules not found under {EXTENSION_DIRECTORY}: "
            f"{', '.join(missing_modules)}",
            file=sys.stderr,
        )
        return 1

    # Step 3: keep only the requested project features the project declares, so
    # an unknown or host-only feature is ignored rather than erroring. This is
    # what makes `--feature rendering` still work: it is a host feature now, so
    # it is dropped here and reaches cargo through the host instead. A managed
    # project declares no cargo features, so requested names are never applied.
    declared_features = (
        load_project_features(project_root) if kind == "native" else set()
    )
    project_features = set(requested_features) & declared_features
    if requested_features and not project_features and kind == "native":
        print(
            f"note: none of {sorted(requested_features)} are declared by the "
            "project; linking it without extra features",
            file=sys.stderr,
        )

    # Step 4: render and write the bundle files plus the artifact-name record
    # (content-based, so a stable tree shows no diff). The bundle and the
    # binary-name record land in the shared `<repo>/build/` scratch location,
    # not under the project, so `pill_standalone`'s manifest path stays static.
    bundle_directory = root / BUNDLE_DIRECTORY
    # A project with a `res` directory ships it packed inside the binary.
    assets_directory = project_root / ASSETS_DIRECTORY_NAME
    packs_assets = assets_directory.is_dir()
    cargo_manifest = build_cargo_manifest(
        bundle_directory,
        project_root,
        package_name,
        modules,
        project_features,
        root,
        managed=(kind == "managed"),
        renderer=linked_renderer,
        packs_assets=packs_assets,
    )
    library_source = build_library_source(
        package_name,
        project_name,
        modules,
        kind,
        project_path,
        bundle_directory,
        root / WORKSPACE_DIRECTORY,
        aot=aot and kind == "managed",
        rid=rid,
        renderer=linked_renderer,
        packs_assets=packs_assets,
        logging=load_logging(project_root),
    )
    wrote_manifest = write_if_changed(
        bundle_directory / PROJECT_MANIFEST_FILE_NAME, cargo_manifest
    )
    wrote_source = write_if_changed(bundle_directory / "src" / "lib.rs", library_source)
    # The build script exists only while there is a `res` to pack; a stale one
    # from the previous project would pack the wrong directory.
    build_script_path = bundle_directory / BUILD_SCRIPT_FILE_NAME
    if packs_assets:
        wrote_source |= write_if_changed(
            build_script_path, build_build_script(bundle_directory, assets_directory)
        )
    elif build_script_path.is_file():
        build_script_path.unlink()
        wrote_source = True
    project_name_path = root / "build" / "build_meta" / "build_binary_name.txt"
    wrote_name = write_if_changed(project_name_path, build_binary_name + "\n")

    print(f"shipping bundle: {os.path.relpath(bundle_directory, root)}")
    print("  regenerated (changed)" if wrote_manifest or wrote_source else "  unchanged")

    # Step 5: the web app crate, for a browser build.
    if web:
        web_app_directory = root / WEB_APP_DIRECTORY
        wrote_web = write_if_changed(
            web_app_directory / PROJECT_MANIFEST_FILE_NAME,
            build_web_app_manifest(web_app_directory, root),
        )
        wrote_web |= write_if_changed(
            web_app_directory / "src" / "lib.rs", build_web_app_source()
        )
        print(f"web app: {os.path.relpath(web_app_directory, root)}")
        print("  regenerated (changed)" if wrote_web else "  unchanged")
    print(f"build binary name: {build_binary_name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
