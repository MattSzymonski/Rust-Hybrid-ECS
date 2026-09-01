#!/usr/bin/env python3

# REQUIREMENTS: Python 3.8+, PyYAML installed, Rust toolchain (cargo) on PATH.
#               Run from anywhere; the repository root is located from this
#               script.

# DESCRIPTION: Generates the shipping bundle crate for the static (shipping)
#   posture of the engine host. The bundle is the single crate `pill_standalone`
#   links under `static_project`: it declares the project and every optional
#   module selected by the project's `project_settings.yaml` as ordinary Rust
#   dependencies, and exposes the `StaticModule` / `StaticProject` registration
#   the static-link path initializes.
#
#   The project's scripting language is read from its manifest: a `Cargo.toml`
#   is a native Rust project, a `*.csproj` is a managed C# project. A managed
#   project has no cargo dependency to declare; its `project_backend()` instead
#   returns the `StaticProjectBackend::CSharp` configuration, resolving the
#   assemblies `dotnet build` produced against the engine workspace root.
#
#   Cargo resolves dependencies before build scripts run, so a `build.rs`
#   cannot pull modules in from a YAML file; this generator runs before cargo
#   instead (as a pre-step of the release build) and writes the bundle crate
#   under `<repository_root>/build/pill_shipping_bundle/`. The location is
#   project-agnostic (cargo needs a static path, and only one shipping binary
#   is built at a time), so `pill_standalone`'s manifest never names a specific
#   project. The host links it by path (like the project itself), so no
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
#          [project_path]    workspace-relative path to the project directory
#                            (e.g. examples/project_rs). When omitted, the
#                            PROJECT_PATH environment variable is used - the
#                            same resolution the host uses at startup.
#
# EXAMPLE USAGE:
#   python devops/tools/generate_shipping_bundle.py examples/project_rs
#   python devops/tools/generate_shipping_bundle.py --feature rendering examples/project_rs
#   python devops/tools/generate_shipping_bundle.py examples/project_cs
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
OPTIONAL_MODULE_DIRECTORY = Path("modules") / "optional"
HOST_CRATE_DIRECTORY = Path("modules") / "pill_host"

# Managed (C#) project constants, mirroring `pill_host::config` so a generated
# bundle resolves assemblies exactly where `dotnet build` produced them. The
# host derives the same four values in `csharp_from_manifest`; a shipping build
# has no project path to read, so the generator states them instead.
CSHARP_RUNTIME_ASSEMBLY_NAME = "csharp_runtime"
CSHARP_RUNTIME_OUTPUT_SUBDIRECTORY = "pill_csharp_runtime/bin/Release/net8.0"
CSHARP_TARGET_FRAMEWORK = "net8.0"
# The engine workspace root, against which the managed config's output
# subdirectories are resolved (the workspace manifest globs `modules/*`).
WORKSPACE_DIRECTORY = Path("modules")


def repository_root() -> Path:
    """Returns the repository root, three levels above this script."""
    return Path(__file__).resolve().parent.parent.parent


def load_module_list(project_root: Path) -> list:
    """Loads the optional-module list from the project's settings file.

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
) -> str:
    """Builds the generated bundle's Cargo.toml text."""
    lines = [
        "[package]",
        f'name = "{BUNDLE_CRATE_NAME}"',
        'version = "0.0.0"',
        'edition = "2021"',
        "",
        "[dependencies]",
        # The host, so the table can name StaticModule / StaticProjectBackend.
        # default-features = false keeps hot_reload off, because those types
        # exist only in the static posture.
        "pill_host = { path = "
        f'"{manifest_relative_path(bundle_directory, root / HOST_CRATE_DIRECTORY)}", '
        'default-features = false }',
    ]
    if not managed:
        # The native project itself, so `project::init` is nameable. Requested
        # project features (e.g. rendering) are enabled so the static binary
        # matches a windowed dev build; cargo does not propagate the host's
        # features here. A managed project is a dotnet assembly, so it has no
        # cargo dependency to declare.
        lines.append(
            f'project = {{ path = "{manifest_relative_path(bundle_directory, project_root)}"'
            + (
                f", features = [{', '.join(f'"{name}"' for name in sorted(project_features))}]"
                if project_features
                else ""
            )
            + " }",
        )
    for module in modules:
        module_directory = root / OPTIONAL_MODULE_DIRECTORY / module
        relative_path = manifest_relative_path(bundle_directory, module_directory)
        lines.append(
            f'{module} = {{ path = "{relative_path}", default-features = false }}'
        )
    return "\n".join(lines) + "\n"


def build_library_source(
    package_name: str,
    project_name: str,
    modules: list,
    kind: str,
    project_path: str,
    bundle_directory: Path,
    workspace_root: Path,
) -> str:
    """Builds the generated bundle's src/lib.rs text."""
    lines = [
        "//! Generated shipping bundle - do not edit. Regenerated from",
        "//! the project's `project_settings.yaml` by",
        "//! `devops/tools/generate_shipping_bundle.py`.",
        "",
        "use pill_host::{StaticModule, StaticProject, StaticProjectBackend};",
        "",
        "/// Every selected optional module, in `project_settings.yaml` order.",
        "pub const STATIC_MODULES: &[StaticModule] = &[",
    ]
    for module in modules:
        lines.append(f'    StaticModule {{ name: "{module}", init: {module}::register }},')
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
        lines += [
            "pub fn project_backend() -> StaticProjectBackend {",
            "    StaticProjectBackend::CSharp {",
            "        config: pill_host::CSharpModuleConfig::new(",
            f'            "{CSHARP_RUNTIME_ASSEMBLY_NAME}",',
            f'            "{CSHARP_RUNTIME_OUTPUT_SUBDIRECTORY}",',
            f'            "{package_name}",',
            f'            "../{project_path}/bin/Release/{CSHARP_TARGET_FRAMEWORK}",',
            "        ),",
            f'        root: std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("{workspace_relative_path}"),',
            "    }",
            "}",
        ]
    else:
        lines += [
            "pub fn project_backend() -> StaticProjectBackend {",
            f"    StaticProjectBackend::Native {{ init: {package_name}::init }}",
            "}",
        ]
    lines += [
        "",
        "/// The complete shipping project: modules first, then the project.",
        "pub fn static_project() -> StaticProject {",
        "    StaticProject {",
        # The settings display name, not the crate name: this is what the host
        # logs and what the window title carries.
        f'        name: "{project_name}",',
        "        backend: project_backend(),",
        "        modules: STATIC_MODULES,",
        "    }",
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
    # Parse `--feature <name>` (repeatable) and the optional project path.
    arguments = sys.argv[1:]
    requested_features = []
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
        modules = load_module_list(project_root)
        project_name = load_project_name(project_root)
        build_binary_name = load_build_binary_name(project_root)
        if kind == "managed":
            package_name = find_csproj_manifest(project_root).stem
        else:
            package_name = load_project_package_name(project_root)
    except (FileNotFoundError, ValueError, yaml.YAMLError) as error:
        print(f"error: {error}", file=sys.stderr)
        return 1

    # Step 2: validate every selected module exists under modules/optional/.
    missing_modules = [
        name
        for name in modules
        if not (root / OPTIONAL_MODULE_DIRECTORY / name).is_dir()
    ]
    if missing_modules:
        print(
            f"error: modules not found under {OPTIONAL_MODULE_DIRECTORY}: "
            f"{', '.join(missing_modules)}",
            file=sys.stderr,
        )
        return 1

    # Step 3: keep only the requested project features the project declares, so
    # an unknown or host-only feature is ignored rather than erroring. A managed
    # project declares no cargo features, so requested names are simply not
    # applied (the host-side feature, e.g. `rendering`, still reaches cargo).
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
    cargo_manifest = build_cargo_manifest(
        bundle_directory,
        project_root,
        package_name,
        modules,
        project_features,
        root,
        managed=(kind == "managed"),
    )
    library_source = build_library_source(
        package_name,
        project_name,
        modules,
        kind,
        project_path,
        bundle_directory,
        root / WORKSPACE_DIRECTORY,
    )
    wrote_manifest = write_if_changed(
        bundle_directory / PROJECT_MANIFEST_FILE_NAME, cargo_manifest
    )
    wrote_source = write_if_changed(bundle_directory / "src" / "lib.rs", library_source)
    project_name_path = root / "build" / "build_meta" / "build_binary_name.txt"
    wrote_name = write_if_changed(project_name_path, build_binary_name + "\n")

    print(f"shipping bundle: {os.path.relpath(bundle_directory, root)}")
    print("  regenerated (changed)" if wrote_manifest or wrote_source else "  unchanged")
    print(f"build binary name: {build_binary_name}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
