"""
Shared Component Identity Integration Test for Rust-Hybrid-ECS.

REQUIREMENTS
  - Python 3.8+
  - Rust toolchain (cargo)
  - Run from workspace root or any path (script resolves paths itself)

DESCRIPTION
    Pins the one property that makes a component type usable from two binaries
    at once: `pill_spline::Spline` is linked BOTH by the project (which depends
    on the crate directly, so it can write `Query<&Spline>`) and by the module
    DLL the host loads alongside it. Those are separate compilation units, so
    each gets its own `TypeId` for what the programmer wrote as one type, and
    identifying the component by `TypeId` therefore splits it into two
    components with two columns that cannot see each other's entities.

    `#[pill(shared)]` replaces that identity with one derived from a declared
    name, which both binaries compute identically and without coordination.

    Two scenarios, and the second is what gives the first its meaning:

    1. `shared_identity_binds_both_binaries` - with the attribute in place, the
       engine's own ECS report shows exactly ONE `pill_spline::Spline` column
       and lists the component under `shared identity`. One column is the whole
       claim.

    2. `distinct_type_ids_are_proven_by_removing_it` - the control. Removing
       `#[pill(shared)]` must make startup FAIL with the peer-collision error,
       because the two registrations now arrive under different `TypeId`s with
       the same type name while the first still holds live rows.

       This is what proves the `TypeId`s genuinely differ rather than happening
       to coincide. If they were equal, the second registration would be an
       ordinary idempotent re-registration and nothing would be reported; the
       collision can only happen because they are not.

       It is also the regression this guards: before the collision guard
       existed, that second registration silently evicted the first's persist
       entries, and every row the evicted side owned was dropped at the next
       hot reload with no error at all.

USAGE
  python tests/test_shared_component_identity.py [--timeout-scale S]
                                                 [--scenario NAME]

EXAMPLE USAGE
  python tests/test_shared_component_identity.py
  python tests/test_shared_component_identity.py --timeout-scale 2
  python tests/test_shared_component_identity.py --scenario shared_identity_binds_both_binaries

--- SCRIPT ---
"""

import argparse
import os
import re
import subprocess
import sys
import time
from pathlib import Path
from typing import List, Optional, Tuple

# Standalone-runnable: put `devops/` on `sys.path` before reaching `core`, so
# the suite works from any working directory without a package import.
sys.path.insert(0, str(Path(__file__).resolve().parent.parent))

# Shared paths, tokens, print wrapper, OutputMonitor, process helpers.
from core import suite_common as common  # noqa: E402
from core.suite_common import *  # noqa: E402,F401,F403

# =============================================================================
# Configuration
# =============================================================================

SPLINE_LIB_RS = MODULES_ROOT / "extensions" / "pill_spline" / "src" / "lib.rs"
PROJECT_LIB_RS = WORKSPACE_ROOT / "examples" / "project_rs" / "src" / "lib.rs"

# The component whose identity this suite is about, spelled as the engine
# registers it. With `#[pill(shared)]` and no explicit name, the derive uses
# `module_path!()::TypeName`, which for a crate-root type is the same string
# `std::any::type_name` produces - so nothing keyed on the name moves.
SHARED_COMPONENT_NAME = "pill_spline::Spline"

# The attribute under test, and the form it takes with sharing removed.
SHARED_ATTRIBUTE = "#[pill(persistable, shared)]"
UNSHARED_ATTRIBUTE = "#[pill(persistable)]"

# The project must load `pill_spline` as a module *and* link it directly, which
# is exactly the arrangement that produces two `TypeId`s. `examples/project_rs`
# does both, so this suite drives it rather than the fixture project.
TEST_PROJECT_SETTINGS = """\
name: "Shared Identity Test"
build_binary_name: "SharedIdentityTest"
modules:
  - "pill_spline"
"""

# Emitted by the engine's built-in diagnostics system once per second.
ECS_REPORT_TOKEN = "ECS state"
SHARED_IDENTITY_HEADING = "shared identity"

# Emitted by `World::register_persistable_component` when a second live
# registration claims one type name; see `WorldError::ComponentNameCollision`.
COLLISION_TOKEN = "two live registrations claim one component type name"
# The host turns a non-zero project init into a setup failure.
SETUP_FAILED_TOKEN = "Host setup failed"

# The resource scenario declares a resource in the module, inserts it there, and
# reads it from the project. Both edits are removed again by the backup
# registry, and the probe's value is arbitrary but distinctive.
RESOURCE_PROBE_TOKEN = "SHARED RESOURCE PROBE"
RESOURCE_PROBE_VALUE = "4242"

RESOURCE_DECLARATION = """\
/// Injected by `test_shared_component_identity.py` - a resource declared in the
/// module crate, which the project also links, so it is compiled twice with
/// different features exactly as `Spline` is.
#[derive(Debug, Default)]
pub struct SharedProbeSettings {
    pub value: u32,
}
impl pill_engine::Resource for SharedProbeSettings {
    fn shared_name() -> Option<&'static str> {
        Some("pill_spline::SharedProbeSettings")
    }
}

"""

RESOURCE_INSERT = """\
    // Injected by the shared-identity suite: the module inserts a resource of
    // its own type, which the project then reads through its own copy.
    engine
        .world_mut()
        .insert_resource(SharedProbeSettings { value: 4242 });

    // Injected: a system of the module's own that writes that resource. The
    // project registers one for the same resource, so the scheduler has to keep
    // the two apart - one id, one slot, one writer at a time. The line reports
    // that this side's system ran once the resource was actually reachable,
    // which is a different failure from the scheduler's.
    let writer_probe_frames = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    engine.register_system(
        "shared_probe_writer",
        move |mut settings: ResMut<SharedProbeSettings>| {
            if let Some(mut settings) = settings.get_mut() {
                settings.value += 1;
            }
            if writer_probe_frames
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                == 64
            {
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    "SHARED RESOURCE WRITER MODULE"
                );
            }
        },
    );
"""

RESOURCE_PROBE = """\
    // Injected by the shared-identity suite.
    {
        let seen = engine
            .world()
            .get_resource::<pill_spline::SharedProbeSettings>()
            .map(|settings| settings.value);
        pill_core::info!(
            target: pill_core::telemetry::telemetry_target::ECS,
            value = ?seen,
            "SHARED RESOURCE PROBE"
        );
    }

    // Injected: the project's own writer for the module's shared resource. Both
    // writers resolve to one id, so the scheduler must never batch them; if it
    // did, the debug write lock inside `ResMut::new` would report the overlap
    // while this line proves the project's side had a slot to write to.
    let writer_probe_frames = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));
    engine.register_system(
        "project_shared_probe_writer",
        move |mut settings: ResMut<pill_spline::SharedProbeSettings>| {
            if let Some(mut settings) = settings.get_mut() {
                settings.value += 1;
            }
            if writer_probe_frames
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed)
                == 64
            {
                pill_core::info!(
                    target: pill_core::telemetry::telemetry_target::ECS,
                    "SHARED RESOURCE WRITER PROJECT"
                );
            }
        },
    );
"""

# Each side's writer reports with its own token, so a missing line identifies the
# side that never ran rather than only that something did not happen.
RESOURCE_WRITER_MODULE_TOKEN = "SHARED RESOURCE WRITER MODULE"
RESOURCE_WRITER_PROJECT_TOKEN = "SHARED RESOURCE WRITER PROJECT"

# Emitted by `World::debug_acquire_resource_lock` when two live `ResMut`s hold one
# resource - which is what a scheduler that batched the two writers would produce.
SCHEDULER_OVERLAP_TOKEN = "already mutably borrowed"

# The control scenario for the resource guards: a *different* type in the
# project claims the shared name the module already declared. The claim is made
# from the artifact's own registration code, so what this pins is that the
# conflict reaches the init that raised it - being recorded is not enough.
RESOURCE_CONFLICT_TOKEN = "shared resource name claimed by two different types"

RESOURCE_CONFLICT_DECLARATION = """\
/// Injected by `test_shared_component_identity.py` - a second type claiming the
/// shared name the module declared, which registration must refuse.
#[derive(Debug, Default)]
pub struct ConflictingProbeSettings {
    pub value: u32,
}
impl pill_engine::Resource for ConflictingProbeSettings {
    fn shared_name() -> Option<&'static str> {
        Some("pill_spline::SharedProbeSettings")
    }
}

"""

RESOURCE_CONFLICT_INSERT = """\
    // Injected by the shared-identity suite: a rival claim on a shared name the
    // module already owns.
    engine
        .world_mut()
        .insert_resource(ConflictingProbeSettings { value: 7 });
"""

# The engine's own resources are inserted before any artifact runs, so nothing
# re-inserts them: if one is retired by a reload it stays gone. The probe logs
# once per generation, from the module's own `register`.
TIME_PROBE_TOKEN = "TIME SURVIVAL PROBE"

TIME_PROBE = """\
    // Injected by the shared-identity suite: an engine-owned resource has to
    // outlive every reload, including one that retires a generation.
    pill_core::info!(
        target: pill_core::telemetry::telemetry_target::ECS,
        time_present = engine.world().has_resource::<pill_engine::Time>(),
        "TIME SURVIVAL PROBE"
    );
"""

# The rollback scenario claims a resource only the *incoming* generation knows
# about, then fails its init. The previous image has neither the type nor the
# insert, so nothing can re-claim the value during the rollback - which is what
# makes the host release it, while the failing image is still mapped.
ROLLBACK_TOKEN = "new generation failed to initialize; rolling back"
RETIRED_RESOURCE_TOKEN = "dropped resources claimed only by the failed generation"

ROLLBACK_RESOURCE_DECLARATION = """\
/// Injected by `test_shared_component_identity.py` - a type only the generation
/// that fails to initialize knows about.
#[derive(Debug, Default)]
pub struct RollbackProbeSettings {
    pub value: u32,
}
impl pill_engine::Resource for RollbackProbeSettings {}

"""

ROLLBACK_RESOURCE_INSERT_AND_FAIL = """\
    // Injected by the shared-identity suite: claim a resource of this
    // generation's own, register a system of its own, then fail. Both the value
    // and the system's box carry code from the failing image, which is what the
    // release has to drop before that image goes.
    engine
        .world_mut()
        .insert_resource(RollbackProbeSettings { value: 9 });
    engine.register_system(
        "failed_generation_probe",
        |_: pill_engine::Res<RollbackProbeSettings>| {},
    );
    return 1;
"""

# The module's sample-offset constant, edited to trigger a body-only reload.
# A plain `f32` literal, so exactly one line matches.
SAMPLE_OFFSET_PATTERN = re.compile(
    r"^(\s*)const SAMPLE_VERTICAL_OFFSET:\s*f32\s*=\s*([0-9.]+)\s*;?\s*$", re.MULTILINE
)

# `pill_spline::register` counts the splines already in the world before topping
# up to its demo count, and logs the number. After a reload it must be 1: the
# rows the retiring generation created survived into the new one.
SURVIVED_TOKEN = "existing=1"

STARTUP_TIMEOUT = 120
PREBUILD_TIMEOUT = 420
REPORT_TIMEOUT = 90
# How long to wait for the optional ECS-report confirmation in scenario 1.
REPORT_SAMPLE_TIMEOUT = 20
COLLISION_TIMEOUT = 150
RELOAD_TIMEOUT_SECONDS = 180

ORIGINAL_PROJECT_SETTINGS: Optional[str] = None

# A report line naming one stored column, e.g.
#   "│    bit   0  pill_spline::Spline    1 x 200 B │"
COLUMN_LINE_PATTERN = re.compile(r"^\W*bit\s+(\d+)\s+(\S+)")


# =============================================================================
# Host configuration
# =============================================================================


def install_test_project_settings() -> None:
    """Backs up the example project's settings and installs this suite's own.

    The optional-module list is read only from this file, so the suite writes a
    minimal one loading just `pill_spline`, keeping the scenario independent of
    whatever modules the project happens to list.
    """
    global ORIGINAL_PROJECT_SETTINGS
    settings_path = project_settings_yaml(NATIVE_PROJECT_ROOT)
    ORIGINAL_PROJECT_SETTINGS = (
        settings_path.read_text(encoding="utf-8") if settings_path.exists() else None
    )
    settings_path.write_text(TEST_PROJECT_SETTINGS, encoding="utf-8")


def restore_project_settings() -> None:
    """Restores the project's settings file, or removes this suite's own."""
    settings_path = project_settings_yaml(NATIVE_PROJECT_ROOT)
    if ORIGINAL_PROJECT_SETTINGS is None:
        settings_path.unlink(missing_ok=True)
    else:
        settings_path.write_text(ORIGINAL_PROJECT_SETTINGS, encoding="utf-8")


def set_shared_identity(enabled: bool) -> None:
    """Puts `#[pill(shared)]` on the spline component, or takes it off.

    Always writes, even when the text is already what is wanted, because the
    write is what makes the file newer than the artifact built from it. The
    host decides whether to rebuild a module by comparing source and artifact
    timestamps, and `BackupRegistry.restore_all` deliberately restores the
    ORIGINAL mtime - so after a previous run the source can look older than a
    DLL compiled from the opposite setting, and the host would skip the rebuild
    and load a module whose attribute does not match the source on disk.

    Written atomically, as every source edit in these suites is, so the host's
    watcher never sees a half-written file.
    """
    content = read_source(SPLINE_LIB_RS)
    wanted = SHARED_ATTRIBUTE if enabled else UNSHARED_ATTRIBUTE
    other = UNSHARED_ATTRIBUTE if enabled else SHARED_ATTRIBUTE
    if wanted not in content:
        if content.count(other) != 1:
            raise RuntimeError(
                f"Expected exactly one {other!r} in {SPLINE_LIB_RS.name}, "
                f"found {content.count(other)}"
            )
        content = content.replace(other, wanted, 1)
    atomic_write(SPLINE_LIB_RS, content)


def edit_sample_offset() -> None:
    """Changes the module's sample-offset constant, triggering a reload.

    A body-only value change: it alters what the module computes without
    touching any component's shape, so the reload takes the ordinary path
    rather than a schema migration - which is what this scenario wants to
    observe the shared column surviving.
    """
    content = read_source(SPLINE_LIB_RS)
    matches = list(SAMPLE_OFFSET_PATTERN.finditer(content))
    if len(matches) != 1:
        raise RuntimeError(
            f"Expected exactly one SAMPLE_VERTICAL_OFFSET line, found {len(matches)}"
        )
    match = matches[0]
    new_offset = (float(match.group(2)) + 1.0) % 10.0
    new_line = f"{match.group(1)}const SAMPLE_VERTICAL_OFFSET: f32 = {new_offset:.1f};"
    atomic_write(SPLINE_LIB_RS, content.replace(match.group(0), new_line, 1))


def restore_then_install(*paths: Path) -> None:
    """Rewinds captured sources before a scenario installs its own edits.

    The suite restores once at the end of a run rather than between scenarios, so
    a scenario injecting into a file an earlier one already injected into would
    compile two copies of the same declaration. Rewinding first makes each
    scenario independent of the order it runs in, and the fresh mtime a restore
    leaves behind is what makes the host rebuild from it.
    """
    for path in paths:
        BACKUP.restore_one(path)


def install_shared_resource_in_module() -> None:
    """Declares the shared resource in the module and inserts it there.

    Split out because two scenarios need this half: one reads the value from the
    project, the other claims its name from the project and expects to be
    refused.
    """
    restore_then_install(SPLINE_LIB_RS)
    module = read_source(SPLINE_LIB_RS)
    anchor = "// =============================================================================\n// Component\n"
    if anchor not in module:
        raise RuntimeError(f"Component section anchor missing from {SPLINE_LIB_RS.name}")
    module = module.replace(anchor, RESOURCE_DECLARATION + anchor, 1)

    register_anchor = "pub fn register(engine: &mut Engine) -> u32 {\n"
    if module.count(register_anchor) != 1:
        raise RuntimeError(f"`register` anchor missing from {SPLINE_LIB_RS.name}")
    module = module.replace(register_anchor, register_anchor + RESOURCE_INSERT, 1)
    atomic_write(SPLINE_LIB_RS, module)


def install_resource_probe() -> None:
    """Declares a shared resource in the module and reads it from the project.

    The module inserts it during `register`; the project reads it during `init`,
    which runs afterwards. Both files are captured by the backup registry, so
    the edits come out again whatever the scenario does.
    """
    install_shared_resource_in_module()

    restore_then_install(PROJECT_LIB_RS)
    project = read_source(PROJECT_LIB_RS)
    init_anchor = "pub fn init(engine: &mut Engine) -> u32 {\n"
    if project.count(init_anchor) != 1:
        raise RuntimeError(f"`init` anchor missing from {PROJECT_LIB_RS.name}")
    atomic_write(PROJECT_LIB_RS, project.replace(init_anchor, init_anchor + RESOURCE_PROBE, 1))


def install_resource_conflict() -> None:
    """Installs the module's shared resource, then a rival claim in the project.

    Modules load before the project, so the module's claim is the first one and
    the project is the artifact whose registration has to be refused.

    The declaration goes *above* the `#[pill_project]` attribute: an item
    inserted between an attribute and the function it annotates is a parse
    error, since the attribute expects a function next.
    """
    install_shared_resource_in_module()

    restore_then_install(PROJECT_LIB_RS)
    project = read_source(PROJECT_LIB_RS)
    attribute_anchor = "#[pill_project]\npub fn init(engine: &mut Engine) -> u32 {\n"
    init_anchor = "pub fn init(engine: &mut Engine) -> u32 {\n"
    if project.count(attribute_anchor) != 1:
        raise RuntimeError(f"`#[pill_project]` anchor missing from {PROJECT_LIB_RS.name}")
    if project.count(init_anchor) != 1:
        raise RuntimeError(f"`init` anchor missing from {PROJECT_LIB_RS.name}")
    project = project.replace(
        attribute_anchor, RESOURCE_CONFLICT_DECLARATION + attribute_anchor, 1
    )
    project = project.replace(init_anchor, init_anchor + RESOURCE_CONFLICT_INSERT, 1)
    atomic_write(PROJECT_LIB_RS, project)


def install_time_probe() -> None:
    """Logs whether the engine's own `Time` resource exists, once per generation.

    `register` runs on every load and every reload, so it is the *second*
    reload's line that shows whether the first retired a resource the module
    never owned: a retired resource is dropped at the end of that transaction,
    after that generation's `register` has already logged.
    """
    restore_then_install(SPLINE_LIB_RS)
    module = read_source(SPLINE_LIB_RS)
    register_anchor = "pub fn register(engine: &mut Engine) -> u32 {\n"
    if module.count(register_anchor) != 1:
        raise RuntimeError(f"`register` anchor missing from {SPLINE_LIB_RS.name}")
    atomic_write(SPLINE_LIB_RS, module.replace(register_anchor, register_anchor + TIME_PROBE, 1))


def install_failing_reload_with_resource() -> None:
    """Edits the module so its next generation claims a resource, then fails.

    The declaration and the insert arrive in one edit, so the previous
    generation - still mapped, and the one the rollback restores - has neither
    and cannot re-claim the id. That is what leaves the value's drop function as
    the only thing pointing into the image about to be unmapped.

    The project is rewound too, though this scenario does not edit it: a rival
    shared-name claim left behind by an earlier scenario would fail the project's
    init at startup, and this scenario needs a host that starts.
    """
    restore_then_install(SPLINE_LIB_RS, PROJECT_LIB_RS)
    module = read_source(SPLINE_LIB_RS)
    anchor = "// =============================================================================\n// Component\n"
    if anchor not in module:
        raise RuntimeError(f"Component section anchor missing from {SPLINE_LIB_RS.name}")
    module = module.replace(anchor, ROLLBACK_RESOURCE_DECLARATION + anchor, 1)

    register_anchor = "pub fn register(engine: &mut Engine) -> u32 {\n"
    if module.count(register_anchor) != 1:
        raise RuntimeError(f"`register` anchor missing from {SPLINE_LIB_RS.name}")
    module = module.replace(
        register_anchor, register_anchor + ROLLBACK_RESOURCE_INSERT_AND_FAIL, 1
    )
    atomic_write(SPLINE_LIB_RS, module)


# =============================================================================
# Process helpers
# =============================================================================


def launch_host() -> Tuple[subprocess.Popen, OutputMonitor]:
    """Starts the headless host against `examples/project_rs`."""
    process_environment = os.environ.copy()
    process_environment["PROJECT_PATH"] = "../examples/project_rs"
    # `rendering` is required, not a preference. `examples/project_rs` links
    # `pill_master_renderer` directly, so building the project drags wgpu into
    # its dependency graph and turns on features in crates `pill_core` also
    # depends on. The host selects itself as a cargo anchor to unify features
    # for that build, so an anchor without `rendering` resolves `pill_core`
    # differently from the project and the project DLL fails to load with "The
    # specified procedure could not be found" (os error 127). See
    # `apply_cargo_host_overrides` in `pill_host/src/build_runner.rs`.
    return launch_process(
        ["cargo", "run", "--package", "pill_standalone", "--features", "rendering"],
        MODULES_ROOT,
        process_environment,
    )


def build_host() -> bool:
    """Builds the standalone host once so a launch does not pay for it."""
    print("\n  [PREP] Building pill_standalone...")
    try:
        result = subprocess.run(
            ["cargo", "build", "-p", "pill_standalone", "--features", "rendering"],
            cwd=str(MODULES_ROOT),
            capture_output=True,
            text=True,
            timeout=PREBUILD_TIMEOUT,
        )
    except subprocess.TimeoutExpired:
        print(f"  [FAIL] Pre-build timed out after {PREBUILD_TIMEOUT} seconds.")
        return False
    except FileNotFoundError:
        print("  [FAIL] 'cargo' not found. Is Rust installed and on PATH?")
        return False

    if result.returncode != 0:
        print("  [FAIL] Pre-build failed:")
        print(result.stderr[-2000:])
        return False

    print("  [OK] Host built.")
    return True


# =============================================================================
# Report parsing
# =============================================================================


def extract_latest_report(output: str) -> Optional[List[str]]:
    """Returns the lines of the last complete ECS state report in `output`.

    A report is bounded by the block's own borders, so a partially flushed one
    at the tail is skipped rather than parsed into a wrong answer - which is
    the failure mode that makes an output-matching suite report the wrong
    reason.
    """
    lines = output.splitlines()
    starts = [index for index, line in enumerate(lines) if ECS_REPORT_TOKEN in line]
    for start in reversed(starts):
        for end in range(start + 1, len(lines)):
            if "└" in lines[end]:
                return lines[start : end + 1]
    return None


def columns_named(report: List[str], component_name: str) -> List[int]:
    """Returns the mask bits of every stored column with this component name.

    One entry means one column, which is the property under test. Two would
    mean the two binaries each got their own.
    """
    bits: List[int] = []
    for line in report:
        match = COLUMN_LINE_PATTERN.search(line)
        if match and match.group(2) == component_name:
            bits.append(int(match.group(1)))
    return bits


def lists_under_shared_identity(report: List[str], component_name: str) -> bool:
    """Whether the report's `shared identity` section names this component."""
    within_section = False
    for line in report:
        if SHARED_IDENTITY_HEADING in line:
            within_section = True
            continue
        if within_section:
            # Sections are separated by the block's horizontal rules.
            if "├" in line or "└" in line:
                within_section = False
            elif component_name in line:
                return True
    return False


# =============================================================================
# Scenarios
# =============================================================================


def scenario_shared_identity_binds_both_binaries() -> bool:
    """With `#[pill(shared)]`, the two registrations become one component.

    The observable is the *absence* of the peer collision, which is decisive
    only because the control scenario shows the same arrangement producing one:
    the guard fires whenever two different `ComponentId`s carry one type name
    while the first still holds rows. Sharing removes the second id, so silence
    here means the project's registration bound to the module's column rather
    than allocating its own.

    Deliberately not asserted through the per-second ECS report: that needs
    sustained frames, and a windowed host presents its first frame and then
    waits for input under this harness, so the report may never arrive. It is
    still parsed when one does appear, as a direct confirmation of the column
    count, and its absence is reported rather than passed over in silence.
    """
    print("\n  [TEST] Shared identity: project and module bind one column.")
    set_shared_identity(True)

    process, monitor = launch_host()
    try:
        start_index = monitor.line_count
        if not monitor.wait_for(STARTUP_TOKEN, STARTUP_TIMEOUT):
            # Name the likely cause rather than only the symptom: a failure
            # to bind shows up here as a startup that never happens, because
            # the collision aborts the project's init before the loop.
            if COLLISION_TOKEN in monitor.output_since(0):
                print(
                    "  [FAIL] Startup aborted on a peer collision for "
                    f"{SHARED_COMPONENT_NAME}. The two registrations did NOT "
                    "bind to one component, so shared identity is not in force."
                )
            else:
                print("  [FAIL] Host did not reach the project loop.")
            print(f"  Output tail:\n{monitor.output_since(0)[-2000:]}")
            return False
        print("  [OK] Host started with the project and the module loaded.")

        output = monitor.output_since(0)

        # The claim. Reaching the loop already means the project's init
        # returned success, so a collision could not have been reported - but
        # assert it explicitly, because that is the fact under test.
        if COLLISION_TOKEN in output:
            print(
                "  [FAIL] A peer collision was reported despite shared identity: "
                "the two registrations did not bind to one component."
            )
            return False
        print("  [OK] No peer collision: both registrations resolved to one component.")

        if SETUP_FAILED_TOKEN in output:
            print("  [FAIL] Host setup failed even though startup was reached.")
            return False
        print("  [OK] The project's init succeeded with the module already loaded.")

        # Direct confirmation when frames happen to run: exactly one column for
        # the component, listed under the report's `shared identity` section.
        if monitor.wait_for(ECS_REPORT_TOKEN, REPORT_SAMPLE_TIMEOUT, start_index):
            report = extract_latest_report(monitor.output_since(start_index))
            if report is None:
                print("  [FAIL] An ECS report was printed but could not be parsed.")
                return False
            bits = columns_named(report, SHARED_COMPONENT_NAME)
            if len(bits) != 1:
                print(
                    f"  [FAIL] Expected exactly 1 {SHARED_COMPONENT_NAME} column, "
                    f"found {len(bits)} (mask bits {bits}). Two means the project "
                    f"and the module each got their own."
                )
                print("  Report:\n" + "\n".join(report))
                return False
            if not lists_under_shared_identity(report, SHARED_COMPONENT_NAME):
                print(
                    f"  [FAIL] {SHARED_COMPONENT_NAME} is not listed under "
                    f"'{SHARED_IDENTITY_HEADING}', so it resolved to a per-binary "
                    f"identity rather than a declared one."
                )
                print("  Report:\n" + "\n".join(report))
                return False
            print(
                f"  [OK] ECS report confirms one column at mask bit {bits[0]}, "
                f"under '{SHARED_IDENTITY_HEADING}'."
            )
        else:
            # Said out loud rather than skipped quietly: the pass rests on the
            # collision evidence above, and this line records that the extra
            # confirmation was unavailable this run.
            print(
                f"  [NOTE] No ECS report within {REPORT_SAMPLE_TIMEOUT}s "
                f"(a windowed host runs frames only while it has input); the "
                f"column-count confirmation was skipped."
            )

        print("  [PASS] One type, two binaries, one component.")
        return True
    finally:
        common.terminate_process(process, monitor)


def scenario_distinct_type_ids_are_proven_by_removing_it() -> bool:
    """Without the attribute, the two registrations collide.

    The collision is only possible because the two `TypeId`s differ: equal ones
    would make the second registration idempotent and silent.
    """
    print("\n  [TEST] Control: removing shared identity must collide.")
    set_shared_identity(False)

    process, monitor = launch_host()
    try:
        # The host is expected to fail setup and exit, so this waits for the
        # exit rather than for a token: `wait_for*` returns `None` as soon as
        # the process is gone, whether or not the token was printed, and the
        # collision is printed milliseconds before that exit.
        deadline = time.monotonic() + COLLISION_TIMEOUT
        reached_loop = False
        while time.monotonic() < deadline:
            if STARTUP_TOKEN in monitor.output_since(0):
                reached_loop = True
                break
            if not monitor.process_alive():
                break
            time.sleep(0.2)

        output = monitor.output_since(0)

        if reached_loop:
            print(
                "  [FAIL] The host started cleanly without `#[pill(shared)]`. "
                "Two registrations of one type name should have collided, so "
                "either the two `TypeId`s no longer differ (and this suite is "
                "not testing what it claims), or the peer-collision guard in "
                "`register_persistable_component` has stopped reporting."
            )
            return False

        if COLLISION_TOKEN not in output:
            print("  [FAIL] No peer collision reported, and no project loop either.")
            print(f"  Output tail:\n{output[-2000:]}")
            return False
        print("  [OK] Peer collision reported, so the two `TypeId`s genuinely differ.")

        if SHARED_COMPONENT_NAME not in output:
            print(f"  [FAIL] The collision did not name {SHARED_COMPONENT_NAME}.")
            return False
        print(f"  [OK] The collision names {SHARED_COMPONENT_NAME}.")

        # The registration must fail the init rather than evicting the peer,
        # which is the silent data-loss path this guard replaced.
        if SETUP_FAILED_TOKEN not in output:
            print(
                "  [FAIL] The collision was reported but setup continued; it must "
                "fail the init rather than evict the peer's persist entries."
            )
            print(f"  Output tail:\n{output[-2000:]}")
            return False
        print("  [OK] Setup failed loudly instead of silently dropping the peer's rows.")

        print("  [PASS] Distinct `TypeId`s confirmed; the guard catches the collision.")
        return True
    finally:
        common.terminate_process(process, monitor)


def scenario_a_shared_component_survives_a_module_reload() -> bool:
    """A shared component's rows survive a real DLL swap.

    The combination nothing else covers. `test_hot_reload_suite.py` reloads
    `pill_spline` against the fixture project, which does not link the crate,
    so only one binary registers `Spline`. Here the project links it as well,
    so the reload makes BOTH binaries re-register the same shared component
    against a column that already holds rows.

    That is also the only path exercising `rehome_native_columns` with two
    registrants: a column stores function pointers into the artifact that
    created it, and the reload re-points them at the most recently registered
    generation. With one registrant there is nothing to compete; with two there
    is.
    """
    print("\n  [TEST] Hot reload: a shared component's rows survive a DLL swap.")
    set_shared_identity(True)
    install_time_probe()

    process, monitor = launch_host()
    try:
        if not monitor.wait_for(STARTUP_TOKEN, STARTUP_TIMEOUT):
            print("  [FAIL] Host did not reach the project loop.")
            print(f"  Output tail:\n{monitor.output_since(0)[-2000:]}")
            return False
        print("  [OK] Host started; the module has created its demo spline.")

        start_index = monitor.line_count
        print("  [TEST] Editing the module to trigger a reload...")
        edit_sample_offset()

        if not monitor.wait_for(
            MODULE_RELOAD_COMPLETE_TOKEN, RELOAD_TIMEOUT_SECONDS, start_index
        ):
            print("  [FAIL] The module reload never completed.")
            print(f"  Output tail:\n{monitor.output_since(start_index)[-2000:]}")
            return False
        print("  [OK] Module reloaded from a freshly built DLL.")

        output = monitor.output_since(start_index)

        # The point of the scenario: the rows outlived the swap.
        if SURVIVED_TOKEN not in output:
            print(
                "  [FAIL] The reloaded module did not report "
                f"'{SURVIVED_TOKEN}', so the spline rows did not survive the "
                "swap - the column was lost or re-seeded from scratch."
            )
            print(f"  Output tail:\n{output[-2000:]}")
            return False
        print(f"  [OK] The reloaded module reports '{SURVIVED_TOKEN}': rows survived.")

        # Re-registering from both binaries must not look like a collision.
        if COLLISION_TOKEN in output:
            print(
                "  [FAIL] Re-registration after the reload was reported as a peer "
                "collision; the reloaded generation did not bind to the column."
            )
            return False
        print("  [OK] Both binaries re-registered without a collision.")

        # A second swap, because the first is the one that could have retired a
        # resource this module never owned - and the drop happens at the end of
        # that transaction, after its `register` had already logged. The next
        # generation's line is therefore the first that can show the loss.
        second_start = monitor.line_count
        print("  [TEST] Editing again to drive a second reload...")
        edit_sample_offset()

        if not monitor.wait_for(
            MODULE_RELOAD_COMPLETE_TOKEN, RELOAD_TIMEOUT_SECONDS, second_start
        ):
            print("  [FAIL] The second module reload never completed.")
            print(f"  Output tail:\n{monitor.output_since(second_start)[-2000:]}")
            return False
        print("  [OK] Second reload complete.")

        probes = [
            line
            for line in monitor.output_since(second_start).splitlines()
            if TIME_PROBE_TOKEN in line
        ]
        if not probes:
            print("  [FAIL] The module's probe did not run on the second reload.")
            return False
        if "time_present=false" in probes[-1]:
            print(
                "  [FAIL] `Time` is gone after a reload: an engine-owned resource "
                "was retired by a generation that never owned it. Nothing "
                "re-inserts it, so it stays gone for the rest of the session."
            )
            print(f"  Probe: {probes[-1].strip()}")
            return False
        if "time_present=true" not in probes[-1]:
            print(f"  [FAIL] Unreadable probe line.\n  Probe: {probes[-1].strip()}")
            return False
        print("  [OK] `Time` survived both reloads.")

        # A column whose function table was left pointing into the unmapped
        # image would fault here rather than report anything.
        if has_crash_signals(monitor.output_since(0)):
            print("  [FAIL] The host crashed during or after the reload.")
            print(f"  Output tail:\n{output[-2000:]}")
            return False
        if not monitor.process_alive():
            print("  [FAIL] The host exited during the reload.")
            return False
        print("  [OK] The host is alive and un-crashed after the swap.")

        print("  [PASS] A shared column survives a reload with two registrants.")
        return True
    finally:
        common.terminate_process(process, monitor)


def scenario_a_shared_resource_crosses_the_boundary() -> bool:
    """A resource declared in a module is readable from the project.

    Resources have the identity problem components had, and one the components
    never did: `ResourceId` was a bare `TypeId`, and the value lived in a
    `Box<dyn Any>` whose `downcast_ref` compares `TypeId` too. A resource
    defined in `pill_spline` - compiled twice with different features - was
    therefore invisible across the boundary, silently, as `None`.

    This is the only scenario that exercises two genuinely different `TypeId`s
    for one resource type; nothing in-process can, because two distinct Rust
    types in one binary are exactly what the guard is designed to reject.

    It also covers the scheduler's side of that identity, which is the half no
    in-process test reaches across two DLLs: each binary registers a system that
    writes the same shared resource, and the pair has to be serialized.
    """
    print("\n  [TEST] A shared resource declared in a module reaches the project.")
    set_shared_identity(True)
    install_resource_probe()

    process, monitor = launch_host()
    try:
        if not monitor.wait_for(STARTUP_TOKEN, STARTUP_TIMEOUT):
            print("  [FAIL] Host did not reach the project loop.")
            print(f"  Output tail:\n{monitor.output_since(0)[-2000:]}")
            return False
        print("  [OK] Host started with the module's resource inserted.")

        output = monitor.output_since(0)
        probe = next(
            (line for line in output.splitlines() if RESOURCE_PROBE_TOKEN in line), None
        )
        if probe is None:
            print("  [FAIL] The project never reported reading the resource.")
            print(f"  Output tail:\n{output[-2000:]}")
            return False

        if "None" in probe:
            print(
                "  [FAIL] The project could not see the module's resource: "
                "the two artifacts resolved it to different ids, or the stored "
                "value refused the project's copy of the type."
            )
            print(f"  Probe: {probe.strip()}")
            return False
        if RESOURCE_PROBE_VALUE not in probe:
            print(f"  [FAIL] The project read an unexpected value.\n  Probe: {probe.strip()}")
            return False
        print(f"  [OK] The project read the module's value ({RESOURCE_PROBE_VALUE}).")

        if COLLISION_TOKEN in output:
            print("  [FAIL] A collision was reported while registering the resource.")
            return False
        print("  [OK] No collision: both copies resolved to one resource.")

        # Both binaries registered a writer for that one resource, so both have to
        # be able to run it: the scheduler must keep them out of one batch, and
        # each side's own line is what shows its writes reached the shared slot.
        # A batched pair fails differently - the debug write lock reports the
        # overlap and takes the host down - so the absence of that report is part
        # of the claim rather than a detail.
        for token, side in (
            (RESOURCE_WRITER_MODULE_TOKEN, "module"),
            (RESOURCE_WRITER_PROJECT_TOKEN, "project"),
        ):
            if not monitor.wait_for(token, REPORT_SAMPLE_TIMEOUT, 0):
                print(
                    f"  [FAIL] The {side}'s writer system never reported running, so "
                    "its writes never reached the shared slot."
                )
                print(f"  Output tail:\n{monitor.output_since(0)[-2000:]}")
                return False
        print("  [OK] Both binaries' writers ran against the one resource.")

        if SCHEDULER_OVERLAP_TOKEN in monitor.output_since(0) or has_crash_signals(
            monitor.output_since(0)
        ):
            print(
                "  [FAIL] The host reported two live mutable borrows of the shared "
                "resource: its two writers were batched in parallel."
            )
            return False
        if not monitor.process_alive():
            print("  [FAIL] The host exited while the writers were running.")
            return False
        print("  [OK] The scheduler kept the two writers apart, with no fault.")

        print("  [PASS] A module's resource is readable and writable from both binaries.")
        return True
    finally:
        common.terminate_process(process, monitor)


def scenario_a_conflicting_resource_claim_fails_setup() -> bool:
    """A rival claim on a shared resource name fails the init that made it.

    The guard fires from the artifact's own registration code, which runs after
    the engine's component-registration drain, so recording the conflict is not
    enough: the generated init has to read it back and fail, or the host runs on
    with two types sharing one resource slot - and the second insert silently
    replacing the first type's value.

    Nothing in-process covers this half. The integration tests call
    `World::take_registration_error` directly, which says nothing about whether
    an `init` wrapper ever does.
    """
    print("\n  [TEST] A rival claim on the module's shared resource name fails setup.")
    install_resource_conflict()

    process, monitor = launch_host()
    try:
        # The host is expected to fail setup and exit, so this waits for the
        # exit rather than for a token, exactly as the component control does.
        deadline = time.monotonic() + COLLISION_TIMEOUT
        reached_loop = False
        while time.monotonic() < deadline:
            if STARTUP_TOKEN in monitor.output_since(0):
                reached_loop = True
                break
            if not monitor.process_alive():
                break
            time.sleep(0.2)

        output = monitor.output_since(0)

        if reached_loop:
            print(
                "  [FAIL] The host started with two types claiming one shared "
                "resource name: the conflict was recorded and never read."
            )
            return False

        if RESOURCE_CONFLICT_TOKEN not in output:
            print("  [FAIL] No shared-resource name conflict was reported.")
            print(f"  Output tail:\n{output[-2000:]}")
            return False
        print("  [OK] The conflict was reported, naming the claim and the claimant.")

        if SETUP_FAILED_TOKEN not in output:
            print(
                "  [FAIL] The conflict was reported but setup continued; the "
                "generated init never read the recorded error."
            )
            print(f"  Output tail:\n{output[-2000:]}")
            return False
        print("  [OK] Setup failed instead of running with the second claim in place.")

        print("  [PASS] A resource conflict raised from user code fails the init.")
        return True
    finally:
        common.terminate_process(process, monitor)


def scenario_a_failed_reload_drops_its_own_resource() -> bool:
    """A generation that fails to initialize leaves no resource behind.

    The failing image is unmapped the moment the rollback returns, so a resource
    value that generation inserted points its drop at code that is about to go.
    The rollback therefore has to release it - and can only do so while the
    image is still mapped, which is why this is asserted against a live host
    rather than in-process.

    The type is declared by the failing generation alone, so nothing re-claims
    it during the rollback and the release has to happen.
    """
    print("\n  [TEST] Rollback: a failed generation's own resource is released.")
    set_shared_identity(True)
    # Rewind before the host starts rather than after: a rival shared-name claim
    # left by an earlier scenario would fail this startup, and the first
    # generation has to succeed for there to be a rollback to observe.
    restore_then_install(SPLINE_LIB_RS, PROJECT_LIB_RS)

    process, monitor = launch_host()
    try:
        if not monitor.wait_for(STARTUP_TOKEN, STARTUP_TIMEOUT):
            print("  [FAIL] Host did not reach the project loop.")
            print(f"  Output tail:\n{monitor.output_since(0)[-2000:]}")
            return False
        print("  [OK] Host started on a generation that owns no such resource.")

        start_index = monitor.line_count
        print("  [TEST] Editing the module so its next init claims a resource and fails...")
        install_failing_reload_with_resource()

        if not monitor.wait_for(ROLLBACK_TOKEN, RELOAD_TIMEOUT_SECONDS, start_index):
            print("  [FAIL] The failing generation was never rolled back.")
            print(f"  Output tail:\n{monitor.output_since(start_index)[-2000:]}")
            return False
        print("  [OK] The host detected the failure and rolled back.")

        output = monitor.output_since(start_index)
        if MODULE_RELOAD_COMPLETE_TOKEN in output:
            print("  [FAIL] The failed generation was reported as a completed reload.")
            return False

        if RETIRED_RESOURCE_TOKEN not in output:
            print(
                "  [FAIL] The value the failed generation inserted was not "
                "released; its drop still points into the image being unmapped."
            )
            print(f"  Output tail:\n{output[-2000:]}")
            return False
        print("  [OK] The failed generation's resource was released while mapped.")

        if has_crash_signals(monitor.output_since(0)) or not monitor.process_alive():
            print("  [FAIL] The host did not survive the rollback.")
            return False
        print("  [OK] The host is alive on the previous generation.")

        print("  [PASS] A failed generation leaves nothing pointing into its image.")
        return True
    finally:
        common.terminate_process(process, monitor)


def scenario_a_failing_module_start_releases_its_resource() -> bool:
    """A module whose very first generation fails leaves nothing behind.

    The same release as the reload rollback, one step earlier: there is no
    previous generation to keep, so the failed generation's systems and the
    world go with it - and both have to go while its image is still mapped.
    Get that wrong and the host prints why it is stopping and *then* faults,
    which is how a diagnosable startup failure turned into a missing one.
    """
    print("\n  [TEST] Startup: a module that fails its first generation is released.")
    set_shared_identity(True)
    install_failing_reload_with_resource()

    process, monitor = launch_host()
    try:
        deadline = time.monotonic() + COLLISION_TIMEOUT
        reached_loop = False
        while time.monotonic() < deadline:
            if STARTUP_TOKEN in monitor.output_since(0):
                reached_loop = True
                break
            if not monitor.process_alive():
                break
            time.sleep(0.2)

        output = monitor.output_since(0)

        if reached_loop:
            print("  [FAIL] The host started with a module whose init reported failure.")
            return False

        if SETUP_FAILED_TOKEN not in output:
            print("  [FAIL] Setup neither failed nor reached the loop.")
            print(f"  Output tail:\n{output[-2000:]}")
            return False
        print("  [OK] Setup failed on the module's non-zero status.")

        if has_crash_signals(output):
            print(
                "  [FAIL] The host faulted while tearing the failed generation "
                "down: its image was unmapped with something still pointing into it."
            )
            print(f"  Output tail:\n{output[-2000:]}")
            return False
        print("  [OK] The teardown completed without a fault.")

        print("  [PASS] A failed module generation leaves nothing pointing into its image.")
        return True
    finally:
        common.terminate_process(process, monitor)


SCENARIOS = {
    "shared_identity_binds_both_binaries": scenario_shared_identity_binds_both_binaries,
    "distinct_type_ids_are_proven_by_removing_it": (
        scenario_distinct_type_ids_are_proven_by_removing_it
    ),
    "a_shared_component_survives_a_module_reload": (
        scenario_a_shared_component_survives_a_module_reload
    ),
    "a_shared_resource_crosses_the_boundary": (
        scenario_a_shared_resource_crosses_the_boundary
    ),
    "a_conflicting_resource_claim_fails_setup": (
        scenario_a_conflicting_resource_claim_fails_setup
    ),
    "a_failed_reload_drops_its_own_resource": (
        scenario_a_failed_reload_drops_its_own_resource
    ),
    "a_failing_module_start_releases_its_resource": (
        scenario_a_failing_module_start_releases_its_resource
    ),
}


# =============================================================================
# CLI
# =============================================================================


def main() -> None:
    """Parses arguments and runs the shared-identity scenarios."""
    parser = argparse.ArgumentParser(
        description="Shared component identity integration test for Rust-Hybrid-ECS"
    )
    parser.add_argument(
        "--timeout-scale",
        type=float,
        default=1.0,
        help="Multiply all timeouts for slow machines (default: 1.0)",
    )
    parser.add_argument(
        "--scenario",
        choices=sorted(SCENARIOS),
        help="Run one scenario instead of all of them",
    )
    args = parser.parse_args()

    if args.timeout_scale <= 0:
        print("ERROR: --timeout-scale must be > 0")
        sys.exit(1)

    if not SPLINE_LIB_RS.exists():
        print(f"ERROR: Missing module source: {SPLINE_LIB_RS}")
        sys.exit(1)

    global STARTUP_TIMEOUT, PREBUILD_TIMEOUT, REPORT_TIMEOUT, COLLISION_TIMEOUT
    global REPORT_SAMPLE_TIMEOUT, RELOAD_TIMEOUT_SECONDS
    STARTUP_TIMEOUT = int(STARTUP_TIMEOUT * args.timeout_scale)
    PREBUILD_TIMEOUT = int(PREBUILD_TIMEOUT * args.timeout_scale)
    REPORT_TIMEOUT = int(REPORT_TIMEOUT * args.timeout_scale)
    COLLISION_TIMEOUT = int(COLLISION_TIMEOUT * args.timeout_scale)
    REPORT_SAMPLE_TIMEOUT = int(REPORT_SAMPLE_TIMEOUT * args.timeout_scale)
    RELOAD_TIMEOUT_SECONDS = int(RELOAD_TIMEOUT_SECONDS * args.timeout_scale)

    selected = (
        [(args.scenario, SCENARIOS[args.scenario])]
        if args.scenario
        else list(SCENARIOS.items())
    )

    print("=" * 66)
    print("  Shared Component Identity Integration Test")
    print(f"  Workspace: {WORKSPACE_ROOT}")
    print(f"  Component: {SHARED_COMPONENT_NAME}")
    print(f"  Scenarios: {len(selected)}")
    print(f"  Time scale: {args.timeout_scale}x")
    print("=" * 66)

    # The suite edits the module source and the project's settings; both are
    # captured now and restored whatever happens below.
    BACKUP.capture(SPLINE_LIB_RS)
    BACKUP.capture(PROJECT_LIB_RS)
    kill_stale_hosts()
    install_test_project_settings()

    results = []
    try:
        if not build_host():
            sys.exit(1)
        for name, scenario in selected:
            try:
                results.append((name, scenario()))
            finally:
                kill_stale_hosts()
    finally:
        print("\n  [CLEANUP] Restoring module source and project settings...")
        # `reset_mtime=False` deliberately: this suite toggles an attribute that
        # changes what the compiled module MEANS, so the restored source must look
        # newer than whatever was built from the opposite setting. Putting the
        # original timestamp back would leave the tree poisoned - the host would
        # judge the stale DLL fresh and load a module whose identity does not
        # match the source on disk.
        BACKUP.restore_all(reset_mtime=False)
        restore_project_settings()
        kill_stale_hosts()
        print("  [OK] Restored.")

    print("\n" + "=" * 66)
    for name, passed in results:
        print(f"  {'PASS' if passed else 'FAIL'}  {name}")
    all_passed = bool(results) and all(passed for _, passed in results)
    print("  TEST PASSED" if all_passed else "  TEST FAILED")
    print("=" * 66)
    sys.exit(0 if all_passed else 1)


if __name__ == "__main__":
    run_suite_with_timing(main)
