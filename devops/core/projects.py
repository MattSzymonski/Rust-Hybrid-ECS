"""
Project and path configuration shared by every devops suite, benchmark and gate step.

REQUIREMENTS: Python 3.8+ (standard library only).

DESCRIPTION
    One module names the projects the devops scripts drive and the files inside
    them the scripts read or edit. A suite imports a name; it does not spell a
    path of its own, so relocating, adding or renaming a project is an edit here
    and nowhere else under `devops/`.

    `EXAMPLES_ROOT` is the single knob for the repository's game projects: every
    example path below derives from it. The fixture crate the functional suites
    drive lives under `devops/tests/` and derives from `TESTS_ROOT`.

    Each project is exported in two forms, because the callers need different
    ones:

      * `*_ROOT` - an absolute `Path` to the project directory, for reading and
        writing files;
      * `*_PATH` - the `PROJECT_PATH` value the host receives. The host resolves
        it against its working directory, which is the Cargo workspace
        (`modules/`), and accepts POSIX separators on every platform.

    A tool that resolves its own argument against the repository root (the
    shipping generator, the web build, a suite that writes a command line) takes
    `repository_relative(<root>)` instead of a `*_PATH` value.

USAGE
  from core.projects import PROJECT_RS_ROOT, PROJECT_RS_PATH

  With `devops/` on `sys.path`; see `core.paths.ensure_devops_on_path`. Suites
  that import `core.suite_common` receive every public name below through its
  star import.

--- SCRIPT ---
"""

import os
from pathlib import Path

from .paths import MODULES_ROOT, REPOSITORY_ROOT, TESTS_ROOT

# The examples directory, relative to the repository root. It is the single
# knob: the root below, every project path, and the globs and display paths of
# the guards that scan it all derive from this value.
EXAMPLES_RELATIVE_PATH = "examples/local"
EXAMPLES_ROOT = REPOSITORY_ROOT / EXAMPLES_RELATIVE_PATH

# Every project directory is named by its manifest, so this is the file a
# discovery walk looks for.
PROJECT_SETTINGS_FILENAME = "project_settings.yaml"


def host_project_path(project_root: Path) -> str:
    """Returns the `PROJECT_PATH` value for a project directory.

    The host resolves `PROJECT_PATH` against its working directory, which every
    caller in `devops/` sets to the Cargo workspace.
    """
    return os.path.relpath(project_root, MODULES_ROOT).replace(os.sep, "/")


def repository_relative(path: Path) -> str:
    """Returns a repository-root-relative POSIX path, for a tool argument."""
    return os.path.relpath(path, REPOSITORY_ROOT).replace(os.sep, "/")


def project_settings_yaml(project_root: Path) -> Path:
    """Returns a project's settings file path."""
    return project_root / PROJECT_SETTINGS_FILENAME


# =============================================================================
# The native example: the minimal scene the native scenarios drive
# =============================================================================

PROJECT_RS_ROOT = EXAMPLES_ROOT / "project_rs"
PROJECT_RS_PATH = host_project_path(PROJECT_RS_ROOT)
PROJECT_RS_SRC = PROJECT_RS_ROOT / "src"
PROJECT_RS_LIB_RS = PROJECT_RS_SRC / "lib.rs"
# The constant the hot-reload harness edits to force a project reload.
PROJECT_RS_SETTINGS_RS = PROJECT_RS_SRC / "settings.rs"
PROJECT_RS_RES = PROJECT_RS_ROOT / "res"

# The name the native suites and benchmarks use: the project is `project_rs`,
# and its role in a scenario is "the native one".
NATIVE_PROJECT_ROOT = PROJECT_RS_ROOT
NATIVE_PROJECT_PATH = PROJECT_RS_PATH


# =============================================================================
# The managed example: the C# bridge, analyzer and mirror suites
# =============================================================================

PROJECT_CS_ROOT = EXAMPLES_ROOT / "project_cs"
PROJECT_CS_PATH = host_project_path(PROJECT_CS_ROOT)
PROJECT_CS_SRC = PROJECT_CS_ROOT / "src"
PROJECT_CS_CSPROJ = PROJECT_CS_ROOT / "project_cs.csproj"
PROJECT_CS_COMPONENTS_CS = PROJECT_CS_SRC / "Components.cs"
PROJECT_CS_SYSTEMS_CS = PROJECT_CS_SRC / "Systems.cs"
# The probe file the bridge suite writes and deletes.
PROJECT_CS_BRIDGE_PROBE_CS = PROJECT_CS_SRC / "BridgeProbe.cs"

# The managed counterpart's role name, matching `NATIVE_PROJECT_ROOT`.
CSHARP_PROJECT_ROOT = PROJECT_CS_ROOT
CSHARP_PROJECT_PATH = PROJECT_CS_PATH


# =============================================================================
# The managed benchmark scene: iteration timing only, no renderer
# =============================================================================

PROJECT_CS_BENCH_ROOT = EXAMPLES_ROOT / "project_cs_bench"
PROJECT_CS_BENCH_PATH = host_project_path(PROJECT_CS_BENCH_ROOT)
PROJECT_CS_BENCH_SRC = PROJECT_CS_BENCH_ROOT / "src"


# =============================================================================
# The renderer's test project: the fullest asset and material set
# =============================================================================

MASTER_RENDERER_TEST_ROOT = EXAMPLES_ROOT / "master_renderer_test"
MASTER_RENDERER_TEST_PATH = host_project_path(MASTER_RENDERER_TEST_ROOT)
MASTER_RENDERER_TEST_RES = MASTER_RENDERER_TEST_ROOT / "res"


# =============================================================================
# The themed example: the UI theme, game UI and editor suites
# =============================================================================

CIRCUS_DEMO_ROOT = EXAMPLES_ROOT / "circus_demo"
CIRCUS_DEMO_PATH = host_project_path(CIRCUS_DEMO_ROOT)
CIRCUS_DEMO_SRC = CIRCUS_DEMO_ROOT / "src"
CIRCUS_DEMO_LIB_RS = CIRCUS_DEMO_SRC / "lib.rs"
# The override the theme suite edits, twice, to prove a live re-apply.
CIRCUS_DEMO_ACCENT_THEME = CIRCUS_DEMO_ROOT / "res" / "ui" / "accent.theme"
# The face the editor's project theme layer installs.
CIRCUS_DEMO_FONT = CIRCUS_DEMO_ROOT / "res" / "fonts" / "Silkscreen-Regular.ttf"


# =============================================================================
# The editor suite's project: a scene large enough to exercise navigation
# =============================================================================

ITALIAN_BRAINROT_ROOT = EXAMPLES_ROOT / "italian_brainrot"
ITALIAN_BRAINROT_PATH = host_project_path(ITALIAN_BRAINROT_ROOT)
ITALIAN_BRAINROT_SCENE_RS = ITALIAN_BRAINROT_ROOT / "src" / "scene.rs"
ITALIAN_BRAINROT_RES = ITALIAN_BRAINROT_ROOT / "res"

# The managed port of the same scene: comment-lint scope only.
ITALIAN_BRAINROT_CS_ROOT = EXAMPLES_ROOT / "italian_brainrot_cs"
ITALIAN_BRAINROT_CS_SRC = ITALIAN_BRAINROT_CS_ROOT / "src"


# =============================================================================
# The suite fixture: the Rust crate the functional scenarios hot-reload
# =============================================================================

FIXTURE_PROJECT_ROOT = TESTS_ROOT / "project"
FIXTURE_PROJECT_PATH = host_project_path(FIXTURE_PROJECT_ROOT)
# The fixture's entry source: the file the native and migration scenarios edit,
# and the one whose restore settles with a project reload rather than a module
# reload.
PROJECT_SANDBOX_LIB_RS = FIXTURE_PROJECT_ROOT / "src" / "lib.rs"
