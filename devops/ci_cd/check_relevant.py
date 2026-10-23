#!/usr/bin/env python3
"""
REQUIREMENTS:
  - Python 3.8+ (standard library only)
  - git on PATH, run inside the repository
  - cargo on PATH (only for checks that compile or test Rust)

DESCRIPTION:
  Runs only the checks a change set needs, instead of the whole gate.
  Computes the changed files (working tree versus a base revision, untracked
  files included), matches them against a rule table of repository areas, and
  runs each area's checks - formatting/lint on Rust changes, the crate's unit
  tests, the named devops suites and the few verify.py steps that guard the
  area. A file that no rule recognizes falls back to `verify.py --quick`, so
  an unknown area can never pass unchecked. Editor suites get the editor build
  (the uplift) prepended automatically.

  This is the inner loop after a change; the full gate (`verify.py`) and the
  milestone tiers stay for stage boundaries and milestones. See
  `devops/README.md` for when to use which.

USAGE:
  python devops/ci_cd/check_relevant.py [--dry-run] [--keep-going]
                                        [--base REF] [--paths P1,P2,...]
                                        [--list-rules]

  --dry-run        print the selected checks with their reasons, run nothing
  --keep-going     run the remaining checks after a failure
  --base REF       diff against REF (default: HEAD)
  --paths P1,P2    check these paths (repository-relative) instead of the
                   working tree's change set
  --list-rules     print the rule table and exit

  Exit status: 0 when every selected check passes, 1 when one fails, 2 when
  the change set or a check's invocation is itself invalid.

EXAMPLE USAGE:
  python devops/ci_cd/check_relevant.py --dry-run
  python devops/ci_cd/check_relevant.py
  python devops/ci_cd/check_relevant.py --base origin/main
  python devops/ci_cd/check_relevant.py --paths modules/pill_draw/src/frame.rs

--- SCRIPT ---
"""

import argparse
import fnmatch
import subprocess
import sys
import time
from dataclasses import dataclass, field
from pathlib import Path
from typing import Dict, Iterable, List, Optional, Sequence, Tuple

REPOSITORY_ROOT = Path(__file__).resolve().parents[2]
MODULES = "modules"

# Paths that never carry code: documentation and local-only or workflow
# state. A change that touches only these needs no checks.
IGNORED_PREFIXES = (
    ".construction_site/",
    ".agents/",
    ".claude/",
    ".github/",
    "local/",
    "docs/",
)
IGNORED_SUFFIXES = (".md",)


@dataclass
class Check:
    """One runnable check: a label, a command, and why it was selected."""

    label: str
    command: Tuple[str, ...]
    cwd: str = "."
    reasons: List[str] = field(default_factory=list)

    def key(self) -> Tuple[str, ...]:
        return self.command


def verify_check(step: str, reason: str) -> Check:
    """A named step of the CI gate (`verify.py --only <step>`)."""
    return Check(
        label=f"verify:{step}",
        command=("python", "devops/ci_cd/verify.py", "--only", step),
        reasons=[reason],
    )


def suite_check(name: str, reason: str) -> Check:
    """A devops suite (`run_all.py --only <file>`)."""
    return Check(
        label=f"suite:{name}",
        command=("python", "devops/tests/run_all.py", "--only", name),
        reasons=[reason],
    )


def cargo_check(label: str, arguments: Sequence[str], reason: str) -> Check:
    """A cargo invocation run from the workspace root (`modules/`)."""
    return Check(
        label=f"cargo:{label}",
        command=("cargo", *arguments),
        cwd=MODULES,
        reasons=[reason],
    )


def matches(path: str, pattern: str) -> bool:
    """Whether `path` (posix) falls under a rule glob; `dir/**` is a prefix."""
    if pattern.endswith("/**"):
        return path.startswith(pattern[: -len("/**") + 1])
    return fnmatch.fnmatch(path, pattern)


def area_checks(paths: Sequence[str]) -> List[Check]:
    """
    The checks every touched area needs, in run order.

    The table is deliberately coarse: one entry per repository area, each a
    handful of checks that together cover that area's contract. Rules may
    select overlapping checks; the caller deduplicates by command.
    """
    selected: List[Check] = []

    def has(*patterns: str) -> List[str]:
        return [p for p in paths if any(matches(p, pattern) for pattern in patterns)]

    def add(check: Check, matched: Sequence[str]) -> None:
        if matched:
            check.reasons = [f"{check.label} because {', '.join(sorted(set(matched))[:4])}"]
            selected.append(check)

    rust_changed = has("**/*.rs")

    # Formatting and the coding-standards lint run for any Rust change: the
    # two together take seconds and guard every area at once.
    if rust_changed:
        selected.append(verify_check("fmt", "Rust files changed"))
        selected.append(verify_check("coding-standards", "Rust files changed"))

    # The shared draw data every UI producer and the renderer contract name:
    # its own tests (the frame slot's among them), plus the game UI, which
    # builds the lists and publishes them into the slot.
    if has("modules/pill_draw/**"):
        selected.append(cargo_check("test pill_draw",
                                    ("test", "-p", "pill_draw", "-p", "pill_game_ui", "--offline"),
                                    "shared draw data changed"))

    # The UI painter: `pill_draw` lists to wgpu. Its own tests cover the
    # texture-set and clip paths; the renderer's tests cover what it is handed
    # each frame, so both run.
    if has("modules/pill_draw_wgpu/**"):
        selected.append(cargo_check("test pill_draw_wgpu",
                                    ("test", "-p", "pill_draw_wgpu", "-p", "pill_master_renderer",
                                     "--offline"),
                                    "UI painter changed"))

    # The egui editor: the shared model crate, the UI library, the reloadable
    # shell module and the frontend. Its suites are the crates' own unit
    # tests plus the egui editor suite; the editor build uplift is added
    # below, since only that build re-freezes the shared dylibs.
    if has("modules/frontents/pill_editor_core_egui/**", "modules/frontents/pill_editor_egui/**",
           "modules/extensions/pill_editor_shell_egui/**", "modules/pill_editor_ui/**"):
        selected.append(cargo_check("test egui editor crates",
                                    ("test", "-p", "pill_editor_core_egui", "-p", "pill_editor_ui",
                                     "-p", "pill_editor_shell_egui_editor", "--offline"),
                                    "egui editor crates changed"))
        selected.append(suite_check("test_editor_egui.py", "egui editor changed"))

    # pill_game_ui: the in-game egui UI (W-0028). Its own unit tests cover the
    # input bridge, the tessellator and the texture mirror; the painter it
    # feeds is `pill_draw_wgpu`, whose tests cover the draw lists. The slot it
    # publishes into is `pill_draw`'s, so a change here also moves what every
    # module drains.
    if has("modules/pill_game_ui/**"):
        selected.append(cargo_check("test pill_game_ui", ("test", "-p", "pill_game_ui", "--offline"),
                                    "pill_game_ui changed"))
        selected.append(cargo_check("test the frame slot",
                                    ("test", "-p", "pill_draw", "--offline"),
                                    "pill_game_ui changed"))

    # The editor: the shell module, frontend and the editor-side crates.
    # The frontend-facing suites are picked per area; run_all handles the rest.
    # The egui editor: the shared model crate, the UI library, the reloadable
    # shell module and the frontend. Its suites are the crates' own unit
    # tests plus the egui editor suite; the editor build uplift is added
    # below, since only that build re-freezes the shared dylibs.
    if has("modules/frontents/pill_editor_core_egui/**", "modules/frontents/pill_editor_egui/**",
           "modules/extensions/pill_editor_shell_egui/**", "modules/pill_editor_ui/**"):
        selected.append(cargo_check("test egui editor crates",
                                    ("test", "-p", "pill_editor_core_egui", "-p", "pill_editor_ui",
                                     "-p", "pill_editor_shell_egui_editor", "--offline"),
                                    "egui editor crates changed"))
        selected.append(suite_check("test_editor_egui.py", "egui editor changed"))

    # The browser frontend: the crate only compiles for wasm, so its check is
    # the wasm target, and the page smoke suite builds a project and draws it
    # in a headless browser.
    if has("modules/frontents/pill_web/**", "devops/tools/build_web.py", "devops/tools/web/**"):
        selected.append(cargo_check("check pill_web (wasm)",
                                    ("check", "-p", "pill_web", "--target", "wasm32-unknown-unknown",
                                     "--offline"),
                                    "browser frontend changed"))
        selected.append(suite_check("test_web_smoke.py", "browser frontend changed"))

    # Engine core and the shared crates below it: unit tests plus the
    # hot-reload regression net that runs a host against real projects.
    if has("modules/pill_core/**"):
        selected.append(cargo_check("test pill_core", ("test", "-p", "pill_core", "--offline"), "pill_core changed"))
        selected.append(suite_check("test_editor_revision.py", "pill_core changed"))
    if has("modules/pill_engine_core/**", "modules/pill_engine/**", "modules/pill_engine_macros/**"):
        selected.append(cargo_check("test pill_engine_core",
                                    ("test", "-p", "pill_engine_core", "-p", "pill_engine", "--offline"),
                                    "engine core changed"))
        selected.append(suite_check("test_hot_reload_suite.py", "engine core changed"))

    # The host: its unit tests and the reload suites that exercise the
    # development postures end to end.
    if has("modules/hosting/pill_host/**", "modules/hosting/pill_hot_scan/**"):
        selected.append(cargo_check("test pill_host",
                                    ("test", "-p", "pill_host", "--features", "hot_patch", "--offline"),
                                    "host changed"))
        selected.append(suite_check("test_hot_reload_suite.py", "host reload path changed"))
        selected.append(suite_check("test_hot_patch_coverage.py", "host patch path changed"))
    if has("modules/hosting/pill_runtime/**", "modules/hosting/pill_renderer_api/**"):
        selected.append(cargo_check("check shipping", ("check", "-p", "pill_standalone",
                                                       "--no-default-features", "--features", "shipping",
                                                       "--offline"), "runtime changed"))
        selected.append(suite_check("test_shipping_smoke.py", "runtime changed"))

    # Extensions: reload through the host, so the reload suite is the net.
    if has("modules/extensions/**"):
        selected.append(suite_check("test_hot_reload_suite.py", "extension changed"))

    # The renderer's data crate and GPU module: boundaries plus the asset
    # reload path that recooks shaders and reattaches the renderer.
    if has("modules/extensions/pill_master_renderer/**"):
        selected.append(verify_check("renderer-boundaries", "renderer changed"))
        selected.append(suite_check("test_hot_reload_assets.py", "renderer or shader data changed"))

    # The C# half of the ABI.
    if has("modules/csharp/**", "modules/pill_csharp_compiler/**", "modules/pill_csharp_runtime/**",
           "**/*.cs", "**/*.csproj"):
        selected.append(suite_check("test_csharp_bridge.py", "C# bridge changed"))

    # Manifests move feature resolution; the drift check is the one guard
    # that compares every spawned build's features. Lockfile churn alone
    # (host-regenerated members) changes no features and is skipped.
    if has("**/Cargo.toml", "modules/.cargo/config.toml", "modules/clippy.toml"):
        selected.append(verify_check("engine-feature-drift", "manifests changed"))
        selected.append(verify_check("engine-layout", "manifests changed"))
        selected.append(verify_check("wrapper-entry-points", "manifests changed"))

    # The suites and the gate are code too: their own rules plus the suite
    # that was touched (a manual-tier suite is only suggested, never run).
    # A changed suite's tokens are checked against the Rust sources that
    # must print them, which is what test_log_contract.py exists for.
    if has("devops/tests/**"):
        selected.append(verify_check("suite-standards", "devops suites changed"))
        selected.append(suite_check("test_log_contract.py", "suite log tokens changed"))
        for path in has("devops/tests/test_*.py"):
            name = path.rsplit("/", 1)[-1]
            selected.append(suite_check(name, f"{name} changed"))
    if has("devops/ci_cd/**", "devops/core/**"):
        selected.append(verify_check("fmt", "devops infrastructure changed"))

    # Examples are projects, not library code: their suites need release
    # builds, so they are suggested rather than run.
    examples = has("examples/**")
    if examples:
        print(f"  note: {len(examples)} file(s) under examples/ changed; "
              "run `python devops/tests/run_all.py --only test_examples.py` when shipping matters")

    # A code change outside every rule above still needs a home: fall back to
    # the quick gate so no area silently skips its checks.
    known = rust_changed or selected or examples
    code_suffixes = (".rs", ".py", ".cs", ".csproj", ".toml", ".css", ".hlsl", ".wgsl", ".json", ".yaml", ".yml", ".sh", ".ps1")
    unknown = [p for p in paths if p.endswith(code_suffixes) and not any(
        matches(p, pattern) for _, patterns in RULE_GLOBS for pattern in patterns)]
    if unknown:
        selected.append(Check(
            label="fallback quick gate",
            command=("python", "devops/ci_cd/verify.py", "--quick"),
            reasons=[f"unrecognized area: {', '.join(sorted(unknown)[:4])}"],
        ))
    elif not known:
        print("  note: no recognized code area changed; running nothing")

    return selected


# The globs every rule looks at, in the same order as `area_checks`. Kept
# beside it so "is this path recognized" and "what does it select" cannot
# drift apart; `--list-rules` prints exactly this.
RULE_GLOBS: List[Tuple[str, Tuple[str, ...]]] = [
    ("pill-draw", ("modules/pill_draw/**",)),
    ("pill-draw-wgpu", ("modules/pill_draw_wgpu/**",)),
    ("pill-game-ui", ("modules/pill_game_ui/**",)),
    ("egui-editor", ("modules/frontents/pill_editor_core_egui/**", "modules/frontents/pill_editor_egui/**",
                     "modules/extensions/pill_editor_shell_egui/**", "modules/pill_editor_ui/**")),
    ("web-frontend", ("modules/frontents/pill_web/**", "devops/tools/build_web.py", "devops/tools/web/**")),
    ("engine-core", ("modules/pill_engine_core/**", "modules/pill_engine/**", "modules/pill_engine_macros/**")),
    ("pill-core", ("modules/pill_core/**",)),
    ("host", ("modules/hosting/pill_host/**", "modules/hosting/pill_hot_scan/**")),
    ("runtime", ("modules/hosting/pill_runtime/**", "modules/hosting/pill_renderer_api/**")),
    ("extensions", ("modules/extensions/**",)),
    ("csharp", ("modules/csharp/**", "modules/pill_csharp_compiler/**", "modules/pill_csharp_runtime/**",
                "**/*.cs", "**/*.csproj")),
    ("manifests", ("**/Cargo.toml", "modules/.cargo/config.toml", "modules/clippy.toml")),
    ("devops-suites", ("devops/tests/**",)),
    ("devops-infrastructure", ("devops/ci_cd/**", "devops/core/**")),
    ("examples", ("examples/**",)),
]


def select_checks(paths: Sequence[str]) -> List[Check]:
    """
    Merge the area checks for `paths`, deduplicating commands and keeping a
    stable run order: cheap checks first, then each area's in table order,
    and the editor suites - with their uplift - at the very end.
    """
    merged: Dict[Tuple[str, ...], Check] = {}
    ordered: List[Check] = []
    for check in area_checks(paths):
        existing = merged.get(check.key())
        if existing is None:
            merged[check.key()] = check
            ordered.append(check)
        else:
            existing.reasons.extend(check.reasons)

    # Cheap, always-first checks: formatting and lint should fail in seconds,
    # not after a host has been rebuilt.
    def rank(check: Check) -> int:
        if check.label == "verify:fmt":
            return 0
        if check.label == "verify:coding-standards":
            return 1
        return 2

    ordered.sort(key=rank)

    # Editor suites run last, and the uplift runs immediately before them:
    # every cargo test or check rewrites the shared dylibs with that
    # invocation's feature unification, and an editor binary built against a
    # later-overwritten dylib dies at process start (0xC0000139
    # STATUS_ENTRYPOINT_NOT_FOUND). Nothing may run between the two.
    editor_rows = [check for check in ordered if check.label.startswith("suite:test_editor_")]
    if editor_rows:
        ordered = [check for check in ordered if not check.label.startswith("suite:test_editor_")]
        uplift = cargo_check("editor build", ("build", "-p", "pill_editor_egui", "--offline"),
                             "editor suites selected")
        uplift.reasons = ["editor suites need the frontend built on the current dylibs"]
        ordered.append(uplift)
        ordered.extend(editor_rows)
    return ordered


def run_git(arguments: Sequence[str]) -> str:
    """Run a git command in the repository and return stdout."""
    result = subprocess.run(
        ("git", *arguments),
        cwd=REPOSITORY_ROOT,
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(result.stderr.strip() or f"git {' '.join(arguments)} failed")
    return result.stdout


def changed_paths(base: str) -> List[str]:
    """
    The working tree's change set against `base`: modified, added, deleted
    and renamed tracked files plus every untracked file, documentation aside.
    """
    tracked = run_git(("diff", "--name-only", "--diff-filter=ACMRD", base)).splitlines()
    untracked = run_git(("ls-files", "--others", "--exclude-standard")).splitlines()
    paths = []
    for path in sorted({line.strip() for line in tracked + untracked if line.strip()}):
        if path.startswith(IGNORED_PREFIXES):
            continue
        if path.endswith(IGNORED_SUFFIXES):
            continue
        paths.append(path)
    return paths


def print_rules() -> None:
    """Print the rule table: one line per area with its globs."""
    for name, globs in RULE_GLOBS:
        print(f"  {name:<22} {', '.join(globs)}")


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--dry-run", action="store_true", help="print the selected checks and run nothing")
    parser.add_argument("--keep-going", action="store_true", help="run the remaining checks after a failure")
    parser.add_argument("--base", default="HEAD", help="diff against this revision (default: HEAD)")
    parser.add_argument("--paths", metavar="P1,P2,...", help="check these paths instead of the change set")
    parser.add_argument("--list-rules", action="store_true", help="print the rule table and exit")
    arguments = parser.parse_args(argv)

    if arguments.list_rules:
        print("Check rules (path area -> checks):")
        print_rules()
        return 0

    if arguments.paths:
        paths = [p.strip() for p in arguments.paths.split(",") if p.strip()]
    else:
        try:
            paths = changed_paths(arguments.base)
        except RuntimeError as error:
            print(f"ERROR: {error}", file=sys.stderr)
            return 2

    if not paths:
        print("change set: empty - nothing to check")
        return 0
    print(f"change set: {len(paths)} file(s) against {arguments.base}")
    for path in paths[:20]:
        print(f"  {path}")
    if len(paths) > 20:
        print(f"  ... and {len(paths) - 20} more")

    checks = select_checks(paths)
    if not checks:
        print("no checks selected")
        return 0

    print(f"\nselected checks ({len(checks)}):")
    for check in checks:
        reason = "; ".join(check.reasons)
        print(f"  {check.label:<28} {reason}")

    if arguments.dry_run:
        print("\ndry run: nothing executed")
        return 0

    failures = 0
    for index, check in enumerate(checks, start=1):
        print(f"\n=== [{index}/{len(checks)}] {check.label} ===")
        print(f"$ {' '.join(check.command)}  (cwd: {check.cwd})")
        started = time.monotonic()
        result = subprocess.run(check.command, cwd=REPOSITORY_ROOT / check.cwd)
        elapsed = time.monotonic() - started
        status = "PASS" if result.returncode == 0 else "FAIL"
        print(f"--- {status} {check.label} ({elapsed:.1f}s)")
        if result.returncode != 0:
            failures += 1
            if not arguments.keep_going:
                print(f"\nstopping after {check.label}; --keep-going runs the rest")
                break

    print(f"\n=== summary: {len(checks) - failures} passed, {failures} failed ===")
    return 1 if failures else 0


if __name__ == "__main__":
    sys.exit(main())
