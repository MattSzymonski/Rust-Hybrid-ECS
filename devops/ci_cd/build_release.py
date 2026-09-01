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
#                              PROJECT_PATH, bundle regenerated, static_project)
#          --project <path>    Project directory (workspace-relative) whose
#                              project_settings.yaml drives the shipping bundle
#                              (defaults to PROJECT_PATH)
#          --profile <name>    Build a different release profile
#                              (release-fast, release-with-debug)
#          -p <package>        Release-build a specific package; targeting
#                              pill_standalone always forces the shipping posture
#          Any other argument is passed straight to cargo.

# EXAMPLE USAGE:
#   set PROJECT_PATH=examples/project_rs
#   python devops/ci_cd/build_release.py                        # shipping host release
#   python devops/ci_cd/build_release.py --profile release-fast # shipping host, throughput profile
#   python devops/ci_cd/build_release.py -p pill_engine         # release-build any package

# --- SCRIPT ---

# Standard library
import datetime
import hashlib
import os
import platform
import shutil
import subprocess
import sys
import time
from pathlib import Path

# The analysis report uses box-drawing glyphs and a check mark; Windows
# defaults stdout to the ANSI codepage (cp1252), which cannot encode them, so
# force UTF-8 on any stream that supports reconfiguration (Python 3.7+).
if hasattr(sys.stdout, "reconfigure"):
    sys.stdout.reconfigure(encoding="utf-8")


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


# The posture the script always builds the host with: no hot reload, project
# and modules linked in statically.
SHIPPING_HOST_ARGUMENTS = [
    "--package",
    "pill_standalone",
    "--no-default-features",
    "--features",
    "static_project",
]


def apply_shipping_host_default(arguments: list) -> list:
    """Forces the shipping posture for the host.

    With no package scoping the script defaults to the shipping host build; an
    explicit `-p pill_standalone` is forced to the shipping posture too, so a
    release build of the host can never carry `hot_reload`. Other packages keep
    their arguments as given.
    """
    if not package_selection_present(arguments):
        return SHIPPING_HOST_ARGUMENTS + arguments
    if explicitly_targets_host(arguments):
        injected = []
        if "--no-default-features" not in arguments:
            injected.append("--no-default-features")
        requested = collect_requested_features(arguments)
        if not requested & {"static_project", "static_csharp"}:
            injected += ["--features", "static_project"]
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
    arguments = sys.argv[1:]
    try:
        project_path, arguments = extract_project_argument(arguments)
    except ValueError as error:
        print(f"error: {error}", file=sys.stderr)
        return 1
    if not project_path:
        project_path = os.environ.get("PROJECT_PATH", "")

    # Step 4: apply the shipping-host default, so a plain invocation builds the
    # shipping host and an explicit `-p pill_standalone` can never be hot-reload.
    arguments = apply_shipping_host_default(arguments)

    # Step 5: validate the effective feature set before any build work starts.
    requested_features = collect_requested_features(arguments)
    shipping_posture = requested_features & {"static_project", "static_csharp"}
    # Where a shipping build's output lands: cargo's target dir under the
    # project's build/build_meta, and dated artifact copies under build/<date>.
    target_directory = None
    artifacts_directory = None
    build_binary_name = ""
    project_root = None
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
        project_root = (repository_root / project_path).resolve()
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
        generator_command = [sys.executable, str(generator_script), project_path]
        for feature in sorted(requested_features):
            generator_command += ["--feature", feature]
        generated = subprocess.run(generator_command, cwd=str(repository_root))
        if generated.returncode != 0:
            return generated.returncode
        # The generator validated the project `build_binary_name` and recorded
        # it; it names the copied artifacts.
        binary_name_file = (
            project_root / "build" / "build_meta" / "build_binary_name.txt"
        )
        if not binary_name_file.is_file():
            print(
                "error: build_meta/build_binary_name.txt missing (the generator "
                "did not record a build binary name)",
                file=sys.stderr,
            )
            return 1
        build_binary_name = binary_name_file.read_text(encoding="utf-8").strip()

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
    return completed.returncode


if __name__ == "__main__":
    sys.exit(main())
