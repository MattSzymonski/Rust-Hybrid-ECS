#!/usr/bin/env python3

# REQUIREMENTS: Python 3.8+, Rust toolchain (cargo) on PATH, PyYAML (the build
#               report reads the project's settings file; the bundle generator
#               needs it too). Run from anywhere; the script locates the
#               repository root itself.

# DESCRIPTION: Builds the workspace's release profile, which a plain
#   `cargo build --release` cannot do.
#
#   The obstacle is `-C prefer-dynamic` in `modules/.cargo/config.toml`. It is
#   there so the host executable and every optional module share one copy of
#   `pill_engine`, which is what keeps its statics, thread-locals and tracing
#   dispatcher single-instance across the DLL boundary. A release binary links
#   everything into one image and needs none of that - and rustc refuses
#   `-C prefer-dynamic` together with the release profile's `lto = "fat"` when
#   targeting Windows, so the flag has to go for a release build to happen at
#   all.
#
#   Cargo offers no way to make `build.rustflags` conditional on the profile.
#   `--config build.rustflags=[]` does not work either: cargo MERGES config
#   arrays under the same key, so an empty list joins with the existing one and
#   changes nothing. Clearing `RUSTFLAGS` in the environment is the only
#   mechanism that does, which is why this is a script rather than a cargo
#   alias - an alias cannot set environment variables.
#
#   Everything else about the build is ordinary cargo. With no package scoping
#   the script defaults to the shipping host: the project comes from
#   `PROJECT_PATH` (or `--project`), the shipping bundle is regenerated from
#   its `project_settings.yaml`, and `pill_standalone` is built with
#   `--no-default-features --features static_project`. Hot reload is a
#   development tool and must not ship, so there is no other way to
#   release-build the host.
#
#   A project whose scripting language is C# (a `*.csproj` in its root, like
#   `examples/project_cs`) is built with `static_csharp` instead: the bundle
#   generator emits the managed backend, `dotnet build -c Release` produces the
#   project assembly (and the C# runtime it references) before cargo runs, and
#   the managed sidecars are copied alongside the shipping binary. Pass
#   `--csharp-aot` to switch that posture to NativeAOT: `dotnet publish
#   -p:PublishAot=true` merges the loader, gameplay code and a trimmed runtime
#   into one self-contained native library (no .NET install, no JIT), the
#   bundle emits `CSharpAot`, and the host loads the library directly.
#
#   Build output lands with the project: cargo's target directory is redirected
#   to `<project_root>/build/build_meta/pill_build_data`, and the finished
#   artifacts are copied to `<project_root>/build/<timestamp>/` (e.g.
#   build/01-09-2026_16-57).
#
#   On success the script prints a build analysis report: the project (name,
#   path, author, description, version, modules), the target platform and
#   toolchain, the profile and features, the git state, and the copied
#   artifacts with sizes and SHA-256 checksums.

# USAGE: devops/ci_cd/build_release.py [cargo arguments...]
#          (no arguments)      Build the shipping host release (project from
#                              PROJECT_PATH, bundle regenerated; static_project
#                              for a native project, static_csharp for a
#                              managed C# project)
#          --project <path>    Project directory (workspace-relative) whose
#                              project_settings.yaml drives the shipping bundle
#                              (defaults to PROJECT_PATH)
#          --profile <name>    Build a different release profile
#                              (release-fast, release-with-debug)
#          --analyze_size      After a successful shipping build, print a
#                              detailed size analysis of the executable: the
#                              PE section layout (always), then per-crate and
#                              top-function attribution via cargo-bloat when
#                              it is installed (cargo install cargo-bloat).
#          --csharp-aot        For a managed C# project, ship the NativeAOT
#                              posture: `dotnet publish -p:PublishAot=true`
#                              produces one self-contained native library (no
#                              .NET runtime install required) instead of the
#                              framework-dependent `dotnet build` output.
#          -p <package>        Release-build a specific package; targeting
#                              pill_standalone always forces the shipping posture
#          Any other argument is passed straight to cargo.

# EXAMPLE USAGE:
#   set PROJECT_PATH=examples/project_rs
#   python devops/ci_cd/build_release.py                        # native shipping host release
#   set PROJECT_PATH=examples/project_cs
#   python devops/ci_cd/build_release.py                        # managed (C#) shipping host release
#   python devops/ci_cd/build_release.py --csharp-aot           # managed (C#) NativeAOT self-contained release
#   python devops/ci_cd/build_release.py --profile release-fast # shipping host, throughput profile
#   python devops/ci_cd/build_release.py -p pill_engine         # release-build any package

# --- SCRIPT ---

# Standard library
import datetime
import hashlib
import json
import os
import platform
import shutil
import struct
import subprocess
import sys
import time
from pathlib import Path

# The analysis report uses box-drawing glyphs and a check mark; Windows
# defaults stdout to the ANSI codepage (cp1252), which cannot encode them, so
# force UTF-8 on any stream that supports reconfiguration (Python 3.7+).
if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8")


# REMOVE_MISC_ARTIFACTS: strip the dated output folder down to what the build
# actually needs to RUN. Debug symbol files (`.pdb`) and linker side products
# (`.exp`, `.lib`) are never loaded at startup - a PDB is only consulted when
# a debugger attaches or a crash dump is symbolicated, and import/export
# libraries only matter when other code links against the DLL by name. They
# are safe to drop because every release build regenerates them from source;
# deleting them keeps the bundle to just the shipping binary, the std
# sidecars, and (for C# projects) the managed assemblies. Keep this True for
# a lean artifact folder; set it False to preserve full debugging support in
# the shipped output (e.g. when collecting a symbolicated crash dump from a
# customer machine).
REMOVE_MISC_ARTIFACTS = True

# File extensions a shipped bundle never needs at runtime (see the constant
# above). Matched case-insensitively against the copied artifact names.
MISC_ARTIFACT_SUFFIXES = (".pdb", ".exp", ".lib")


def collect_requested_features(arguments: list) -> set:
    """Collects the feature names named across `--features` / `-F` flags.

    A feature flag may appear several times and may carry a comma-separated
    list, so this walks the argument list rather than matching a single value.
    """
    requested_features = set()
    index = 0
    while index < len(arguments):
        argument = arguments[index]
        if argument in ("--features", "-F"):
            index += 1
            if index < len(arguments):
                requested_features.update(arguments[index].split(","))
        elif argument.startswith("--features="):
            requested_features.update(argument[len("--features=") :].split(","))
        index += 1
    return requested_features


def extract_project_argument(arguments: list) -> tuple:
    """Pulls `--project <path>` (or `--project=<path>`) out of the forwarded
    cargo arguments, returning (project_path_or_None, remaining_arguments)."""
    project_path = None
    remaining = []
    index = 0
    while index < len(arguments):
        argument = arguments[index]
        if argument == "--project":
            if index + 1 >= len(arguments):
                raise ValueError("--project requires a value")
            project_path = arguments[index + 1]
            index += 2
            continue
        if argument.startswith("--project="):
            project_path = argument[len("--project=") :]
            index += 1
            continue
        remaining.append(argument)
        index += 1
    return project_path, remaining


def host_will_build(arguments: list) -> bool:
    """Whether this invocation compiles the `pill_standalone` host.

    The host is built when `--package`/`-p` names it, when `--workspace` (or
    `--all`) selects it, or when no package scoping is given at all (cargo then
    builds every workspace member). `--exclude pill_standalone` opts it out in
    every case.
    """
    packages = []
    excluded = set()
    index = 0
    while index < len(arguments):
        argument = arguments[index]
        if argument in ("-p", "--package"):
            index += 1
            if index < len(arguments):
                packages.append(arguments[index])
        elif argument.startswith("--package="):
            packages.append(argument[len("--package=") :])
        elif argument == "--exclude":
            index += 1
            if index < len(arguments):
                excluded.add(arguments[index])
        elif argument.startswith("--exclude="):
            excluded.add(argument[len("--exclude=") :])
        index += 1
    if "pill_standalone" in excluded:
        return False
    if packages:
        return "pill_standalone" in packages
    # No package scoping (or `--workspace`): cargo builds every workspace member.
    return True


def package_selection_present(arguments: list) -> bool:
    """Whether the invocation already scopes which packages cargo builds.

    Any `-p`/`--package`, `--workspace`/`--all`, or `--exclude` counts; without
    one of these the script applies its shipping-host default.
    """
    return any(
        argument in ("-p", "--package", "--workspace", "--all", "--exclude")
        or argument.startswith("--package=")
        or argument.startswith("--exclude=")
        for argument in arguments
    )


def explicitly_targets_host(arguments: list) -> bool:
    """Whether `pill_standalone` is named by `-p`/`--package`."""
    index = 0
    while index < len(arguments):
        argument = arguments[index]
        if argument in ("-p", "--package"):
            if index + 1 < len(arguments) and arguments[index + 1] == "pill_standalone":
                return True
        elif argument == "--package=pill_standalone":
            return True
        index += 1
    return False


def shipping_posture_feature(project_root) -> str:
    """The static feature matching the project's scripting language.

    A project whose root contains a `.csproj` is managed and ships as
    `static_csharp`; anything else (no project at all, or a `Cargo.toml`)
    ships as `static_project`.
    """
    if (
        project_root is not None
        and project_root.is_dir()
        and any(project_root.glob("*.csproj"))
    ):
        return "static_csharp"
    return "static_project"


def dotnet_rid() -> str:
    """Maps the build platform to a .NET runtime identifier for AOT publish."""
    system = platform.system().lower()
    machine = platform.machine().lower()
    arm = machine in ("arm64", "aarch64")
    if system == "windows":
        return "win-arm64" if arm else "win-x64"
    if system == "darwin":
        return "osx-arm64" if arm else "osx-x64"
    return "linux-arm64" if arm else "linux-x64"


def resolve_project_root(repository_root: Path, project_path: str) -> Path:
    """Resolves the project directory, honouring both path conventions.

    The host resolves PROJECT_PATH against the working directory (so
    `../examples/project_rs` works from the workspace dir); the shipping tools
    have historically resolved it against the repository root (so
    `examples/project_rs` works from anywhere). The working directory is tried
    first, then the repository root, so both spellings work from either place.
    """
    cwd_candidate = (Path.cwd() / project_path).resolve()
    if cwd_candidate.is_dir():
        return cwd_candidate
    return (repository_root / project_path).resolve()


def apply_shipping_host_default(arguments: list, project_root) -> list:
    """Forces the shipping posture for the host.

    With no package scoping the script defaults to the shipping host build,
    choosing the static feature from the project's scripting language; an
    explicit `-p pill_standalone` is forced to the shipping posture too, so a
    release build of the host can never carry `hot_reload`. Other packages keep
    their arguments as given.
    """
    if not package_selection_present(arguments):
        return [
            "--package",
            "pill_standalone",
            "--no-default-features",
            "--features",
            shipping_posture_feature(project_root),
        ] + arguments
    if explicitly_targets_host(arguments):
        injected = []
        if "--no-default-features" not in arguments:
            injected.append("--no-default-features")
        requested = collect_requested_features(arguments)
        if not requested & {"static_project", "static_csharp"}:
            injected += ["--features", shipping_posture_feature(project_root)]
        if injected:
            return injected + arguments
    return arguments


def copy_shipping_artifacts(
    target_directory: Path, artifacts_directory: Path, build_binary_name: str
) -> list:
    """Copies the built shipping binary and its sidecars into a dated directory.

    The binary and PDB are renamed to the project's `build_binary_name`;
    sidecars keep theirs. Returns the names copied (empty when the build
    produced nothing to copy).
    """
    release_directory = target_directory / "release"
    extension = ".exe" if os.name == "nt" else ""
    artifacts_directory.mkdir(parents=True, exist_ok=True)
    copied = []

    def copy_renamed(source: Path, target_name: str) -> None:
        if source.is_file():
            shutil.copy2(source, artifacts_directory / target_name)
            copied.append(target_name)

    copy_renamed(
        release_directory / f"pill_standalone{extension}",
        f"{build_binary_name}{extension}",
    )
    copy_renamed(release_directory / "pill_standalone.pdb", f"{build_binary_name}.pdb")
    for sidecar in sorted(release_directory.glob("std-*.dll")):
        copy_renamed(sidecar, sidecar.name)
    return copied


def copy_managed_artifacts(
    project_root: Path,
    workspace_root: Path,
    artifacts_directory: Path,
    managed_assembly_name: str,
    aot: bool = False,
    rid: str = "win-x64",
) -> list:
    """Copies the managed project and runtime assemblies into the artifact dir.

    A `static_csharp` host loads these by the workspace paths the bundle baked
    in at compile time, so the copies document what shipped rather than being
    what the binary loads; the artifact folder stays a complete record either
    way. With `aot=True` the managed side is one self-contained native library
    from the `dotnet publish` output instead. Returns the names copied (empty
    when nothing was produced).
    """
    copied = []
    if aot:
        # NativeAOT posture: a single native library (embedded trimmed runtime)
        # plus its PDB, straight from the publish output.
        publish_output = (
            project_root / "bin" / "Release" / "net8.0" / rid / "publish"
        )
        for source_name in (f"{managed_assembly_name}.dll", f"{managed_assembly_name}.pdb"):
            source = publish_output / source_name
            if source.is_file():
                shutil.copy2(source, artifacts_directory / source_name)
                copied.append(source_name)
        return copied
    # The project assembly (plus its PDB), from `dotnet build -c Release`.
    project_output = project_root / "bin" / "Release" / "net8.0"
    for source_name in (f"{managed_assembly_name}.dll", f"{managed_assembly_name}.pdb"):
        source = project_output / source_name
        if source.is_file():
            shutil.copy2(source, artifacts_directory / source_name)
            copied.append(source_name)
    # The C# runtime the project references, from the same dotnet build; it
    # lives in the engine workspace, not under the project.
    runtime_output = workspace_root / "pill_csharp_runtime" / "bin" / "Release" / "net8.0"
    for source_name in ("csharp_runtime.dll", "csharp_runtime.runtimeconfig.json"):
        source = runtime_output / source_name
        if source.is_file():
            shutil.copy2(source, artifacts_directory / source_name)
            copied.append(source_name)
    return copied


def remove_misc_artifacts(
    artifacts_directory: Path, copied_artifacts: list
) -> list:
    """Deletes the files a shipped build never loads at runtime.

    Every entry in `copied_artifacts` whose name ends in one of the
    `MISC_ARTIFACT_SUFFIXES` suffixes (debug symbols, linker side products) is
    removed from the dated output folder. Returns the filtered list so the
    build report reflects exactly what remains.
    """
    kept = []
    for name in copied_artifacts:
        if name.lower().endswith(MISC_ARTIFACT_SUFFIXES):
            artifact_path = artifacts_directory / name
            try:
                artifact_path.unlink()
            except OSError:
                # A file that vanished on its own is fine - nothing to clean.
                pass
            continue
        kept.append(name)
    return kept


def load_project_settings(project_root: Path) -> dict:
    """Loads the project's settings file for the build report.

    Returns {} when the file is missing or unreadable; the report then simply
    omits the fields the file would have supplied.
    """
    settings_path = project_root / "project_settings.yaml"
    if not settings_path.is_file():
        return {}
    try:
        import yaml
    except ImportError:
        return {}
    try:
        with settings_path.open(encoding="utf-8") as handle:
            data = yaml.safe_load(handle) or {}
    except yaml.YAMLError:
        return {}
    return data if isinstance(data, dict) else {}


def human_readable_size(size_bytes: int) -> str:
    """Formats a byte count into a compact human-readable string (e.g. 1.5 MB)."""
    size = float(size_bytes)
    for unit in ("B", "KB", "MB", "GB", "TB"):
        if size < 1024 or unit == "TB":
            if unit == "B":
                return f"{int(size)} B"
            return f"{size:.1f} {unit}"
        size /= 1024
    return f"{size_bytes} B"


def format_duration(seconds: float) -> str:
    """Formats an elapsed time as `Xm Ys` (or just `Ys` under a minute)."""
    whole_seconds = int(seconds)
    minutes, remainder = divmod(whole_seconds, 60)
    if minutes:
        return f"{minutes}m {remainder}s"
    return f"{remainder}s"


def rustc_verbose_info() -> dict:
    """Parses `rustc -vV` output into a dict (host, release, ...); {} on failure."""
    try:
        result = subprocess.run(
            ["rustc", "-vV"], capture_output=True, text=True, timeout=30
        )
    except (OSError, subprocess.SubprocessError):
        return {}
    info = {}
    for line in result.stdout.splitlines():
        key, separator, value = line.partition(":")
        if separator:
            info[key.strip()] = value.strip()
    return info


def target_platform() -> str:
    """Returns the rustc host target triple (e.g. x86_64-pc-windows-msvc).

    Falls back to the OS name and machine architecture when rustc cannot be
    queried.
    """
    host = rustc_verbose_info().get("host")
    if host:
        return host
    return f"{platform.system()} {platform.machine()}"


def toolchain_versions() -> str:
    """Returns rustc and cargo version strings, e.g. `rustc 1.87.0, cargo 1.87.0`."""
    rustc_release = rustc_verbose_info().get("release", "")
    rustc_version = f"rustc {rustc_release}" if rustc_release else ""
    try:
        cargo_result = subprocess.run(
            ["cargo", "-V"], capture_output=True, text=True, timeout=30
        )
        cargo_version = cargo_result.stdout.strip()
    except (OSError, subprocess.SubprocessError):
        cargo_version = ""
    return ", ".join(part for part in (rustc_version, cargo_version) if part)


def git_info(repository_root: Path) -> str:
    """Returns `branch@commit (dirty|clean)` for the repository.

    Returns an empty string when git is unavailable or the folder is not a
    repository, so the report simply omits the line.
    """

    def capture(arguments: list) -> str:
        result = subprocess.run(
            ["git", "-C", str(repository_root), *arguments],
            capture_output=True,
            text=True,
            timeout=30,
        )
        return result.stdout.strip()

    try:
        branch = capture(["rev-parse", "--abbrev-ref", "HEAD"])
        commit = capture(["rev-parse", "--short", "HEAD"])
    except (OSError, subprocess.SubprocessError):
        return ""
    if not branch or not commit:
        return ""
    try:
        dirty = bool(capture(["status", "--porcelain"]))
    except (OSError, subprocess.SubprocessError):
        dirty = False
    state = "dirty" if dirty else "clean"
    return f"{branch}@{commit} ({state})"


def extract_profile(arguments: list) -> str:
    """Returns the cargo profile from `--profile <name>`, or `release`."""
    index = 0
    while index < len(arguments):
        argument = arguments[index]
        if argument == "--profile":
            if index + 1 < len(arguments):
                return arguments[index + 1]
        elif argument.startswith("--profile="):
            return argument[len("--profile=") :]
        index += 1
    return "release"


def sha256_checksum(path: Path) -> str:
    """Returns the lowercase hex SHA-256 digest of a file's contents."""
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def render_tree_lines(root_label: str, branches: list) -> list:
    """Renders a two-level box-drawing tree: a root label with branches, each
    optionally holding leaf children. The last branch's children are indented
    with spaces, the others with a vertical bar.

    Example:
        root
        ├─ first
        └─ last
           ├─ a
           └─ b
    """
    lines = [root_label]
    branch_count = len(branches)
    for branch_index, (label, children) in enumerate(branches):
        last_branch = branch_index == branch_count - 1
        lines.append(("└─ " if last_branch else "├─ ") + label)
        if not children:
            continue
        child_prefix = "   " if last_branch else "│  "
        child_count = len(children)
        for child_index, child in enumerate(children):
            connector = "└─ " if child_index == child_count - 1 else "├─ "
            lines.append(child_prefix + connector + child)
    return lines


# =============================================================================
# Artifact size analysis (--analyze_size)
# =============================================================================

# Crate families for the size report's grouping, matched against cargo bloat's
# per-crate names. Anything `pill_`-prefixed that is not an engine library is
# treated as an optional module.
ENGINE_LIBRARIES = {
    "pill_engine",
    "pill_core",
    "pill_host",
    "pill_standalone",
    "pill_engine_macros",
    "pill_core_macros",
    "pill_hot_scan",
    "pill_editor",
}
PROJECT_CRATES = {"project", "pill_shipping_bundle"}
STANDARD_LIBRARY_CRATES = {
    "core",
    "alloc",
    "std",
    "compiler_builtins",
    "std_detect",
    "std_float",
    "panic_unwind",
    "panic_abort",
    "unwind",
    "hashbrown",
    "rustc_demangle",
}


def read_pe_sections(executable: Path) -> list:
    """Parses the PE section table of a Windows executable.

    Returns a list of {name, raw_size, virtual_size} dicts, empty for non-PE
    files. Pure stdlib (struct), so the on-disk layout is always available
    even when no external size tool is installed.
    """
    try:
        with executable.open("rb") as handle:
            data = handle.read()
    except OSError:
        return []
    if len(data) < 0x40 or data[:2] != b"MZ":
        return []
    pe_offset = struct.unpack_from("<I", data, 0x3C)[0]
    if data[pe_offset : pe_offset + 4] != b"PE\0\0":
        return []
    coff_offset = pe_offset + 4
    number_of_sections = struct.unpack_from("<H", data, coff_offset + 2)[0]
    size_of_optional_header = struct.unpack_from("<H", data, coff_offset + 16)[0]
    section_table_offset = coff_offset + 20 + size_of_optional_header
    sections = []
    for index in range(number_of_sections):
        entry = section_table_offset + index * 40
        name = data[entry : entry + 8].rstrip(b"\0").decode("ascii", errors="replace")
        virtual_size = struct.unpack_from("<I", data, entry + 8)[0]
        raw_size = struct.unpack_from("<I", data, entry + 16)[0]
        sections.append(
            {"name": name, "raw_size": raw_size, "virtual_size": virtual_size}
        )
    return sections


def run_cargo_bloat_json(
    workspace_directory: Path, target_directory: Path, features: list, extra: list
) -> tuple:
    """Runs `cargo bloat` with JSON output and returns (data, error).

    RUSTFLAGS is cleared exactly as for the build itself, otherwise the
    internal `cargo build` fails on `-C prefer-dynamic` with fat LTO. Returns
    (None, message) when cargo bloat is missing, its build fails, or its
    output is not JSON.
    """
    environment = dict(os.environ, RUSTFLAGS="")
    command = [
        "cargo",
        "bloat",
        "--release",
        "-p",
        "pill_standalone",
        "--no-default-features",
        "--features",
        " ".join(sorted(features)),
        "--target-dir",
        str(target_directory),
        *extra,
    ]
    try:
        result = subprocess.run(
            command,
            cwd=str(workspace_directory),
            env=environment,
            capture_output=True,
            text=True,
        )
    except OSError as error:
        return None, f"cargo bloat not available: {error}"
    if result.returncode != 0:
        return None, result.stderr.strip()
    try:
        return json.loads(result.stdout), None
    except json.JSONDecodeError as error:
        return None, f"unexpected cargo bloat output: {error}"


def group_bloat_crates(crate_items: list) -> list:
    """Buckets cargo-bloat crates into engine / optional modules / project /
    standard library / third-party groups, each sorted by size descending.

    Returns a list of (group_title, [(name, size), ...]) tuples.
    """
    buckets = {
        "engine": [],
        "modules": [],
        "project": [],
        "std": [],
        "third_party": [],
    }
    for item in crate_items:
        name = item.get("name", "")
        size = item.get("size", 0)
        if name in ENGINE_LIBRARIES:
            buckets["engine"].append((name, size))
        elif name in PROJECT_CRATES:
            buckets["project"].append((name, size))
        elif name in STANDARD_LIBRARY_CRATES:
            buckets["std"].append((name, size))
        elif name.startswith("pill_"):
            buckets["modules"].append((name, size))
        else:
            buckets["third_party"].append((name, size))
    for key in buckets:
        buckets[key].sort(key=lambda pair: pair[1], reverse=True)
    return [
        ("engine libraries", buckets["engine"]),
        ("optional modules", buckets["modules"]),
        ("project", buckets["project"]),
        ("standard library", buckets["std"]),
        ("third-party crates", buckets["third_party"]),
    ]


def print_size_group(title: str, items: list, total_bytes: int, limit: int = None) -> None:
    """Prints one group of the crate table with sizes and percentages."""
    shown = items if limit is None else items[:limit]
    if not shown:
        return
    print(f"    {title}")
    for name, size in shown:
        print(
            f"      {name:<26} {human_readable_size(size):>10}  "
            f"{100.0 * size / total_bytes:>6.1f}%"
        )
    subtotal = sum(size for _, size in shown)
    print(
        f"      {'---':<26} {human_readable_size(subtotal):>10}  "
        f"{100.0 * subtotal / total_bytes:>6.1f}%"
    )


def truncate_name(name: str, max_chars: int) -> str:
    """Truncates a long symbol name for table display, keeping the tail."""
    if len(name) <= max_chars:
        return name
    return "..." + name[-(max_chars - 3) :]


def analyze_artifact_size(
    artifact_path: Path,
    workspace_directory: Path,
    target_directory: Path,
    features: list,
) -> None:
    """Prints the detailed size analysis of a built executable.

    Part 1 is the PE section layout (always available, pure stdlib). Parts 2
    and 3 - per-crate and top-function attribution - come from cargo bloat
    and are skipped with an install hint when it is unavailable.
    """
    if not artifact_path.is_file():
        print(f"size analysis skipped: {artifact_path} not found", file=sys.stderr)
        return
    file_bytes = artifact_path.stat().st_size
    print()
    print(f"size analysis: {artifact_path}")
    print(f"  file size: {human_readable_size(file_bytes)}")

    # Part 1: PE section layout - where the bytes actually live on disk.
    sections = read_pe_sections(artifact_path)
    if sections:
        print()
        print("  PE sections (on-disk layout)")
        accounted = 0
        for section in sections:
            raw_size = section["raw_size"]
            if raw_size == 0:
                continue
            accounted += raw_size
            print(
                f"    {section['name']:<10} {human_readable_size(raw_size):>10}  "
                f"{100.0 * raw_size / file_bytes:>6.1f}%"
            )
        if accounted < file_bytes:
            remainder = file_bytes - accounted
            print(
                f"    {'(other)':<10} {human_readable_size(remainder):>10}  "
                f"{100.0 * remainder / file_bytes:>6.1f}%"
            )
        print(f"    {'---':<10} {human_readable_size(file_bytes):>10}  100.0%")

    # Part 2: per-crate attribution via cargo bloat.
    print()
    print(
        "  gathering per-crate attribution (cargo bloat; the first run may "
        "rebuild with debug info for symbol names)...",
        file=sys.stderr,
    )
    crates_data, crates_error = run_cargo_bloat_json(
        workspace_directory,
        target_directory,
        features,
        ["--crates", "--split-std", "-n", "0", "--message-format", "json"],
    )
    if crates_data is None:
        print()
        print("  (crate and function attribution skipped: cargo bloat unavailable)")
        if crates_error:
            print(f"    {crates_error.splitlines()[-1]}")
        print("    install it with: cargo install cargo-bloat")
        return
    text_bytes = crates_data.get("text-section-size") or file_bytes
    print()
    print(f"  crates (.text attribution, text size {human_readable_size(text_bytes)})")
    grouped = group_bloat_crates(crates_data.get("crates") or [])
    for group_title, group_items in grouped:
        if group_title == "third-party crates":
            print_size_group(group_title, group_items, text_bytes, limit=15)
        else:
            print_size_group(group_title, group_items, text_bytes)

    # Part 3: the largest functions in the binary.
    functions_data, _ = run_cargo_bloat_json(
        workspace_directory,
        target_directory,
        features,
        ["-n", "20", "--message-format", "json"],
    )
    if functions_data is not None:
        function_items = functions_data.get("functions") or []
        if function_items:
            print()
            print("  top functions (.text)")
            for item in function_items:
                name = item.get("name", "")
                size = item.get("size", 0)
                print(f"    {human_readable_size(size):>10}  {truncate_name(name, 92)}")


def main() -> int:
    """Runs the cargo release build with the workspace's RUSTFLAGS cleared."""
    # Step 0: start the clock - the final report includes total build time.
    start_time = time.monotonic()

    # Step 1: locate the repository root, two levels above this script, so the
    # build works from any working directory.
    script_directory = Path(__file__).resolve().parent
    repository_root = script_directory.parent.parent
    workspace_directory = repository_root / "modules"

    # Step 2: fail loudly if the workspace is not where the layout expects it.
    if not (workspace_directory / "Cargo.toml").is_file():
        print(f"error: no workspace at {workspace_directory}", file=sys.stderr)
        return 1

    # Step 3: resolve the project path from `--project` or `PROJECT_PATH`; it
    # is only required when the shipping bundle must be regenerated.
    # `--analyze_size` and `--csharp-aot` are this script's own flags, not
    # cargo arguments, so they are pulled out before anything is forwarded to
    # cargo.
    arguments = sys.argv[1:]
    analyze_size = "--analyze_size" in arguments
    arguments = [argument for argument in arguments if argument != "--analyze_size"]
    aot = "--csharp-aot" in arguments
    arguments = [argument for argument in arguments if argument != "--csharp-aot"]
    try:
        project_path, arguments = extract_project_argument(arguments)
    except ValueError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    if not project_path:
        project_path = os.environ.get("PROJECT_PATH", "")

    # The project root, resolved up front so the shipping default below can
    # pick the static feature matching the project's scripting language.
    project_root = (
        resolve_project_root(repository_root, project_path) if project_path else None
    )

    # Step 4: apply the shipping-host default, so a plain invocation builds the
    # shipping host and an explicit `-p pill_standalone` can never be hot-reload.
    arguments = apply_shipping_host_default(arguments, project_root)

    # Step 5: validate the effective feature set before any build work starts.
    requested_features = collect_requested_features(arguments)
    shipping_posture = requested_features & {"static_project", "static_csharp"}
    if aot and shipping_posture != {"static_csharp"}:
        print(
            "error: --csharp-aot requires a managed C# project (static_csharp "
            "posture); point PROJECT_PATH at a directory with a .csproj",
            file=sys.stderr,
        )
        return 1
    # Where a shipping build's output lands: cargo's target dir under the
    # project's build/build_meta, and dated artifact copies under build/<date>.
    target_directory = None
    artifacts_directory = None
    build_binary_name = ""
    if shipping_posture:
        if "--no-default-features" not in arguments:
            print(
                "error: the shipping postures (`static_project` / `static_csharp`) "
                "need `--no-default-features`, otherwise the default `hot_reload` "
                "feature stays on and the binary ships reloading code.",
                file=sys.stderr,
            )
            return 1
        if requested_features & {"hot_reload", "hot_patch"}:
            print(
                "error: a shipping build cannot combine "
                f"{sorted(shipping_posture)} with `hot_reload`/`hot_patch`.",
                file=sys.stderr,
            )
            return 1
        if not project_path:
            print(
                "error: no project path: set PROJECT_PATH or pass --project "
                "(the shipping bundle is generated from the project's "
                "project_settings.yaml).",
                file=sys.stderr,
            )
            return 1
        # `project_root` was already resolved before the shipping default was
        # applied, so the posture feature and the output directories agree.
        target_directory = project_root / "build" / "build_meta" / "pill_build_data"
        artifacts_directory = (
            project_root
            / "build"
            / datetime.datetime.now().strftime("%d-%m-%Y_%H-%M")
        )
        # Regenerate the shipping bundle from the project's settings file, so
        # the static build always reflects `project_settings.yaml`. The bundle
        # is gitignored build output; this pre-step is what makes it exist.
        generator_script = (
            repository_root / "devops" / "tools" / "generate_shipping_bundle.py"
        )
        # The requested features are forwarded so the bundle can enable the
        # project's matching features (e.g. `rendering`) in the static build.
        # The generator receives the project path relative to the repository
        # root (it resolves against it itself), so a cwd-relative PROJECT_PATH
        # cannot leak `..` into the generated bundle's paths.
        generator_project_path = os.path.relpath(project_root, repository_root)
        generator_command = [sys.executable, str(generator_script), generator_project_path]
        if aot:
            generator_command += ["--csharp-aot", "--rid", dotnet_rid()]
        for feature in sorted(requested_features):
            generator_command += ["--feature", feature]
        generated = subprocess.run(generator_command, cwd=str(repository_root))
        if generated.returncode != 0:
            return generated.returncode
        # The generator validated the project `build_binary_name` and recorded
        # it in the shared build scratch dir; it names the copied artifacts.
        binary_name_file = (
            repository_root / "build" / "build_meta" / "build_binary_name.txt"
        )
        if not binary_name_file.is_file():
            print(
                "error: build/build_meta/build_binary_name.txt missing (the "
                "generator did not record a build binary name)",
                file=sys.stderr,
            )
            return 1
        build_binary_name = binary_name_file.read_text(encoding="utf-8").strip()

    # Step 5b: a managed shipping build needs the project assembly (and the C#
    # runtime it references) produced before the host is linked - the
    # `static_csharp` backend loads prebuilt assemblies, it never compiles. The
    # bundle declared the modules in Rust, so this dotnet build is the whole
    # managed side of the shipping binary. The output directories are derived
    # from the manifest exactly as the generated bundle's CSharp config does.
    if shipping_posture == {"static_csharp"}:
        managed_manifests = sorted(project_root.glob("*.csproj"))
        if not managed_manifests:
            print(
                f"error: no .csproj in managed project root {project_root}",
                file=sys.stderr,
            )
            return 1
        if shutil.which("dotnet") is None:
            print(
                "error: a C# shipping build needs the .NET SDK (dotnet) on PATH",
                file=sys.stderr,
            )
            return 1
        if aot:
            # NativeAOT posture: publish the project with PublishAot so the
            # loader, the gameplay code, and the trimmed runtime merge into one
            # native library. `EnablePillAot` turns on the source generator
            # (which emits the direct system registry) and the root export
            # forwarders; the RID selects the platform's publish output dir.
            print(
                "Publishing the managed project with NativeAOT "
                f"(dotnet publish -c Release -r {dotnet_rid()} -p:PublishAot=true)."
            )
            managed_build = subprocess.run(
                [
                    "dotnet",
                    "publish",
                    str(managed_manifests[0]),
                    "-c",
                    "Release",
                    "-r",
                    dotnet_rid(),
                    "-p:EnablePillAot=true",
                    "--nologo",
                ],
                cwd=str(repository_root),
            )
        else:
            print("Building the managed project assembly (dotnet build -c Release).")
            managed_build = subprocess.run(
                [
                    "dotnet",
                    "build",
                    str(managed_manifests[0]),
                    "-c",
                    "Release",
                    "--nologo",
                ],
                cwd=str(repository_root),
            )
        if managed_build.returncode != 0:
            return managed_build.returncode

    # The project's settings file drives the build report; read it up front so
    # the final analysis can name the project, its author and its description.
    project_settings = load_project_settings(project_root) if project_root else {}

    # Step 6: refuse a release build of the host that escaped the shipping
    # default (e.g. `--workspace` without the posture).
    if host_will_build(arguments) and not shipping_posture:
        print(
            "error: a release build of `pill_standalone` is always the shipping "
            "posture - `hot_reload` is a development tool and must not ship. "
            "Scope the build away from the host (e.g. `-p pill_engine` or "
            "`--exclude pill_standalone`).",
            file=sys.stderr,
        )
        return 1

    # Step 7: `--profile` may already be among the forwarded arguments; adding
    # `--release` as well would make cargo reject the pair.
    profile_already_chosen = any(
        argument == "--profile" or argument.startswith("--profile=")
        for argument in arguments
    )
    profile_arguments = [] if profile_already_chosen else ["--release"]

    print("Building the release profile with RUSTFLAGS cleared.")
    print(f"  workspace: {workspace_directory}")
    if target_directory is not None:
        print(f"  target: {target_directory}")

    # Step 8: an empty RUSTFLAGS replaces `build.rustflags` outright rather
    # than merging with it, which is exactly what removing `-C prefer-dynamic`
    # requires. A shipping build also redirects cargo's target dir into the
    # project's build/build_meta/pill_build_data.
    environment = dict(os.environ, RUSTFLAGS="")
    if target_directory is not None:
        environment["CARGO_TARGET_DIR"] = str(target_directory)
    # `--offline`: every dependency is a path dep or already cached, and cargo
    # then never touches the registry - whose package-cache lock rust-analyzer
    # can hold for long stretches, which would stall the build with no output.
    command = ["cargo", "build", "--offline", *profile_arguments, *arguments]
    completed = subprocess.run(
        command, cwd=str(workspace_directory), env=environment
    )

    # Step 9: mirror `set -e` from the shell version - a failed build is a
    # failed script, whatever cargo printed. On success, copy the shipping
    # artifacts into the dated output directory.
    copied_artifacts = []
    if completed.returncode == 0 and target_directory is not None:
        copied_artifacts = copy_shipping_artifacts(
            target_directory, artifacts_directory, build_binary_name
        )
        # The managed side of a `static_csharp` build: the project assembly and
        # the C# runtime it references (or, with --csharp-aot, the single
        # self-contained native library), recorded alongside the shipping
        # binary.
        if shipping_posture == {"static_csharp"}:
            managed_assembly_name = managed_manifests[0].stem
            copied_artifacts += copy_managed_artifacts(
                project_root,
                workspace_directory,
                artifacts_directory,
                managed_assembly_name,
                aot=aot,
                rid=dotnet_rid(),
            )
        # Drop the debug symbols and linker side products now that every
        # artifact is in place, so the dated folder holds only what runs
        # (see REMOVE_MISC_ARTIFACTS above).
        if REMOVE_MISC_ARTIFACTS:
            copied_artifacts = remove_misc_artifacts(
                artifacts_directory, copied_artifacts
            )

    # Step 10: print the build analysis as a box-drawing tree - the project is
    # the root, the build facts are its branches, and the artifacts (with
    # sizes and checksums) close the tree.
    if completed.returncode == 0:
        branches = []
        if project_root is not None:
            for key in ("author", "version"):
                value = project_settings.get(key)
                if value:
                    branches.append((f"{key}: {value}", None))
        branches.append((f"platform: {target_platform()}", None))
        branches.append((f"profile: {extract_profile(arguments)}", None))
        features = sorted(requested_features)
        branches.append(
            (f"features: {', '.join(features) if features else '(none)'}", None)
        )
        toolchain = toolchain_versions()
        if toolchain:
            branches.append((f"toolchain: {toolchain}", None))
        repository_git = git_info(repository_root)
        if repository_git:
            branches.append((f"git: {repository_git}", None))
        branches.append(
            (f"duration: {format_duration(time.monotonic() - start_time)}", None)
        )
        if project_root is not None:
            description = project_settings.get("description")
            if description:
                branches.append(("description:", [str(description)]))
            modules = project_settings.get("modules") or []
            if modules:
                branches.append(("modules:", [str(name) for name in modules]))
        if copied_artifacts:
            artifact_entries = []
            for name in copied_artifacts:
                artifact_path = artifacts_directory / name
                size = (
                    artifact_path.stat().st_size if artifact_path.is_file() else 0
                )
                artifact_entries.append((name, artifact_path, size))
            total_size = sum(size for _, _, size in artifact_entries)
            name_width = max(len(name) for name, _, _ in artifact_entries)
            artifact_lines = [f"path: {artifacts_directory}"]
            for name, artifact_path, size in artifact_entries:
                checksum = sha256_checksum(artifact_path)
                artifact_lines.append(
                    f"{name:<{name_width}}  {human_readable_size(size)}  "
                    f"(sha256 {checksum})"
                )
            artifact_lines.append(
                f"{'':<{name_width}}  total: {human_readable_size(total_size)}"
            )
            branches.append(("artifacts:", artifact_lines))

        root_label = (
            project_settings.get("name") if project_root is not None else "Build"
        )
        print("✓ Build finished successfully")
        print()
        for line in render_tree_lines(str(root_label), branches):
            print(line)

    # Step 11: optional deep-dive into what makes up the executable.
    if analyze_size and completed.returncode == 0:
        if copied_artifacts:
            executable_names = [
                name for name in copied_artifacts if name.lower().endswith(".exe")
            ]
            if executable_names:
                analyze_artifact_size(
                    artifacts_directory / executable_names[0],
                    workspace_directory,
                    target_directory,
                    sorted(requested_features),
                )
            else:
                print(
                    "--analyze_size: no executable was copied in this build; "
                    "nothing to analyze.",
                    file=sys.stderr,
                )
        else:
            print(
                "--analyze_size skipped: this build produced no shipping "
                "artifacts (only shipping host builds are analyzed).",
                file=sys.stderr,
            )
    return completed.returncode


if __name__ == "__main__":
    sys.exit(main())
