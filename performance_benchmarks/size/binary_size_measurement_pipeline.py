#!/usr/bin/env python3
"""
Binary Size Measurement Pipeline — iterative binary-size optimization for Rust projects.

Single-file, zero-dependency pipeline. Uses only the Python standard library.
Measures Rust release binary size, section breakdown, per-crate contributions,
largest symbols, compressed size, and dependency metrics. Designed to be used
with the BINARY_SIZE_101.md guide.

Usage:
    python performance_benchmarks/cache/binary_size_measurement_pipeline.py doctor
    python performance_benchmarks/cache/binary_size_measurement_pipeline.py baseline -n initial
    python performance_benchmarks/cache/binary_size_measurement_pipeline.py measure -n opt_level_s -c initial
    python performance_benchmarks/cache/binary_size_measurement_pipeline.py compare --baseline initial --candidate opt_level_s
    python performance_benchmarks/cache/binary_size_measurement_pipeline.py analyze --run opt_level_s
    python performance_benchmarks/cache/binary_size_measurement_pipeline.py list
"""

import argparse
import gzip
import hashlib
import json
import math
import os
import platform
import re
import shlex
import shutil
import subprocess
import sys
import time
from datetime import datetime, timezone
from typing import Any, Dict, List, Optional, Sequence, Tuple

# ---------------------------------------------------------------------------
# Constants
# ---------------------------------------------------------------------------

SCRIPT_DIR = os.path.dirname(os.path.abspath(__file__))
PROJECT_ROOT = os.path.abspath(os.path.join(SCRIPT_DIR, "..", ".."))
ARTIFACT_ROOT = os.path.join(PROJECT_ROOT, "performance_benchmarks", "cache", "artifacts")
BINARY_SIZE_DIR = os.path.join(ARTIFACT_ROOT, "binary_size")
BASELINES_DIR = os.path.join(BINARY_SIZE_DIR, "baselines")
RUNS_DIR = os.path.join(BINARY_SIZE_DIR, "runs")
LATEST_DIR = os.path.join(BINARY_SIZE_DIR, "latest")

EXIT_SUCCESS = 0
EXIT_BUILD_FAILURE = 1
EXIT_CONFIG_ERROR = 2
EXIT_REGRESSION = 3
EXIT_NOT_COMPARABLE = 4

# ---------------------------------------------------------------------------
# CLI argument parsing
# ---------------------------------------------------------------------------

def build_parser() -> argparse.ArgumentParser:
    p = argparse.ArgumentParser(
        prog="binary_size_measurement_pipeline",
        description="Iterative binary-size optimization pipeline for Rust projects.",
    )
    p.add_argument("--verbose", action="store_true")
    p.add_argument("--quiet", action="store_true")
    p.add_argument("--json", action="store_true", dest="json_output")

    sub = p.add_subparsers(dest="command")

    # doctor
    sub.add_parser("doctor", help="Inspect environment and print diagnostic information")

    # baseline
    bp = sub.add_parser("baseline", help="Build, measure binary size, save named baseline")
    bp.add_argument("-n", "--name", required=True, help="Baseline name")
    bp.add_argument("--force", action="store_true", help="Overwrite existing baseline")
    _add_build_args(bp)

    # measure
    mp = sub.add_parser("measure", help="Measure current build and compare to baseline")
    mp.add_argument("-n", "--name", required=True, help="Candidate run name")
    mp.add_argument("-c", "--compare-to", help="Baseline name to compare against")
    mp.add_argument("--force", action="store_true", help="Overwrite existing run")
    _add_build_args(mp)

    # analyze
    ap = sub.add_parser("analyze", help="Deep analysis of an existing run")
    ap.add_argument("--run", required=True, help="Run name to analyze")

    # compare
    cp = sub.add_parser("compare", help="Compare two saved runs")
    cp.add_argument("--baseline", required=True)
    cp.add_argument("--candidate", required=True)
    cp.add_argument("--max-size-growth-percent", type=float, default=2.0,
                    help="Maximum acceptable binary size growth percentage (default: 2.0)")

    # run (convenience)
    rp = sub.add_parser("run", help="doctor + baseline + measure + compare")
    rp.add_argument("-n", "--name", required=True, help="Run name")
    rp.add_argument("-c", "--compare-to", help="Baseline to compare against")
    _add_build_args(rp)

    # list
    sub.add_parser("list", help="List saved baselines and candidate runs")

    return p


def _add_build_args(p: argparse.ArgumentParser) -> None:
    p.add_argument("--manifest-path", help="Path to Cargo.toml")
    p.add_argument("-p", "--package", default=None, help="Cargo package name (default: auto-detect)")
    p.add_argument("--bin", default=None, help="Specific binary to build (default: main binary)")
    p.add_argument("--profile", default="release", help="Cargo profile (default: release)")
    p.add_argument("--target", help="Target triple")
    p.add_argument("--target-dir", help="Target directory")
    p.add_argument("--features", nargs="*", default=[], help="Cargo features")
    p.add_argument("--all-features", action="store_true")
    p.add_argument("--no-default-features", action="store_true")
    p.add_argument("--locked", action="store_true", default=True)
    p.add_argument("--no-locked", action="store_true")
    p.add_argument("--timeout", type=int, default=300)
    p.add_argument("--environment", nargs="*", default=[], help="KEY=VALUE pairs")


# ---------------------------------------------------------------------------
# Utilities
# ---------------------------------------------------------------------------

_global_quiet = False

def log(msg: str, *, level: str = "info") -> None:
    """Print to stderr unless --quiet. level can be info, warn, error."""
    if _global_quiet and level not in ("error", "warn"):
        return
    prefix = {"info": "[*]", "warn": "[W]", "error": "[E]"}.get(level, "[*]")
    print(f"{prefix} {msg}", file=sys.stderr)


def run_cmd(cmd: List[str], **kwargs) -> subprocess.CompletedProcess:
    """Run a command safely. Logs the command and duration."""
    cmd_str = " ".join(shlex.quote(str(a)) for a in cmd)
    log(f"$ {cmd_str}")
    start = time.time()
    result = subprocess.run(cmd, **kwargs)
    elapsed = time.time() - start
    log(f"  -> exit={result.returncode} ({elapsed:.1f}s)")
    return result


def find_tool(name: str) -> Optional[str]:
    """Locate an executable on PATH."""
    return shutil.which(name)


def sha256_file(path: str) -> str:
    """Compute SHA-256 digest of a file."""
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(65536), b""):
            h.update(chunk)
    return h.hexdigest()


def json_load(path: str) -> dict:
    with open(path, "r") as f:
        return json.load(f)


def json_dump(obj: Any, path: str) -> None:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w") as f:
        json.dump(obj, f, indent=2, default=str)


def ensure_dir(path: str) -> str:
    os.makedirs(path, exist_ok=True)
    return path


def gzip_size(path: str) -> int:
    """Return gzip-compressed size of a file."""
    import io
    buf = io.BytesIO()
    with gzip.GzipFile(fileobj=buf, mode="wb") as compressor:
        with open(path, "rb") as f:
            compressor.write(f.read())
    return buf.tell()


# ---------------------------------------------------------------------------
# Statistics (shared with cache pipeline style)
# ---------------------------------------------------------------------------

def compute_simple_stats(values: Sequence[float]) -> dict:
    """Compute basic statistics for a list of values (used for per-crate breakdowns)."""
    if not values:
        return {"count": 0}
    n = len(values)
    return {
        "count": n,
        "min": min(values),
        "max": max(values),
        "mean": sum(values) / n,
        "sum": sum(values),
    }


# ---------------------------------------------------------------------------
# Environment diagnosis (doctor)
# ---------------------------------------------------------------------------

def cmd_doctor() -> int:
    sections = []

    def add_section(title: str, lines: List[str]):
        sections.append((title, lines))
        print(f"\n## {title}", file=sys.stderr)
        for line in lines:
            print(f"  {line}", file=sys.stderr)

    # Python
    add_section("Python", [
        f"version: {sys.version.split()[0]}",
        f"executable: {sys.executable}",
        f"implementation: {platform.python_implementation()}",
    ])

    # OS
    add_section("Operating System", [
        f"system: {platform.system()} {platform.release()}",
        f"version: {platform.version()}",
        f"architecture: {platform.machine()}",
        f"hostname: {platform.node()}",
        f"cpu_count: {os.cpu_count()}",
    ])

    # Rust toolchain
    rust_lines = []
    for tool in ["rustc", "cargo"]:
        p = find_tool(tool)
        if p:
            try:
                r = run_cmd([tool, "--version"], capture_output=True, text=True, timeout=10)
                rust_lines.append(f"{tool}: {r.stdout.strip()}")
            except Exception:
                rust_lines.append(f"{tool}: found but failed to query")
        else:
            rust_lines.append(f"{tool}: NOT FOUND")
    add_section("Rust Toolchain", rust_lines)

    # Git
    git_lines = []
    try:
        r = run_cmd(["git", "rev-parse", "HEAD"], capture_output=True, text=True, timeout=10, cwd=PROJECT_ROOT)
        git_lines.append(f"revision: {r.stdout.strip()}")
    except Exception:
        git_lines.append("revision: unavailable")
    try:
        r = run_cmd(["git", "status", "--porcelain"], capture_output=True, text=True, timeout=10, cwd=PROJECT_ROOT)
        git_lines.append(f"dirty: {len(r.stdout.strip()) > 0}")
    except Exception:
        pass
    add_section("Git", git_lines)

    # Binary-size analysis tools
    tools_to_check = {
        "cargo-bloat": "cargo bloat (largest symbols & per-crate size)",
        "llvm-size": "LLVM size (section breakdown)",
        "llvm-nm": "LLVM nm (symbol table)",
        "llvm-objdump": "LLVM objdump (disassembly)",
        "cargo-llvm-lines": "cargo llvm-lines (monomorphization analysis)",
        "gzip": "gzip (compressed size estimation)",
        "strip": "strip (symbol removal)",
    }
    tool_lines = []
    for tool, desc in tools_to_check.items():
        status = "AVAILABLE" if find_tool(tool) else "MISSING"
        tool_lines.append(f"{tool}: {status}  ({desc})")
    add_section("Binary-Size Analysis Tools", tool_lines)

    # Cargo.toml profile check
    profile_lines = []
    manifest_path = os.path.join(PROJECT_ROOT, "Cargo.toml")
    if os.path.exists(manifest_path):
        try:
            with open(manifest_path, "r") as f:
                content = f.read()
            # Look for [profile.release] section
            has_release_profile = "[profile.release]" in content
            has_lto = "lto" in content
            has_strip = "strip" in content
            has_panic = "panic" in content
            has_opt_level = "opt-level" in content
            has_codegen_units = "codegen-units" in content
            profile_lines.append(f"[profile.release] defined: {has_release_profile}")
            profile_lines.append(f"  lto: {'present' if has_lto else 'NOT SET (recommended)'}")
            profile_lines.append(f"  strip: {'present' if has_strip else 'NOT SET (recommended for distribution)'}")
            profile_lines.append(f"  panic: {'present' if has_panic else 'NOT SET (abort saves 5-15%)'}")
            profile_lines.append(f"  opt-level: {'present' if has_opt_level else 'NOT SET (defaults to 3)'}")
            profile_lines.append(f"  codegen-units: {'present' if has_codegen_units else 'NOT SET (1 recommended for size)'}")
        except Exception:
            profile_lines.append("Could not read Cargo.toml")
    add_section("Cargo.toml Release Profile", profile_lines)

    return EXIT_SUCCESS


# ---------------------------------------------------------------------------
# Build
# ---------------------------------------------------------------------------

class BuildResult:
    def __init__(self):
        self.success = False
        self.executable_path: Optional[str] = None
        self.executable_size: int = 0
        self.executable_sha256: str = ""
        self.duration_seconds: float = 0.0
        self.command: str = ""
        self.stderr: str = ""
        self.stdout: str = ""


def cmd_build(args: argparse.Namespace) -> Tuple[int, BuildResult]:
    br = _do_build(args)
    if br.success:
        print(f"Build OK: {br.executable_path}")
        print(f"  Size: {br.executable_size:,} bytes")
        print(f"  SHA-256: {br.executable_sha256}")
        print(f"  Duration: {br.duration_seconds:.1f}s")
        return EXIT_SUCCESS, br
    else:
        log(f"Build FAILED", level="error")
        if br.stderr:
            print(br.stderr, file=sys.stderr)
        return EXIT_BUILD_FAILURE, br


def _find_executable(manifest_dir: str, args: argparse.Namespace) -> Optional[str]:
    """Find the built executable. Tries JSON messages first, then common paths."""
    target_dir = args.target_dir or os.path.join(manifest_dir, "target")

    # Determine which binary to look for
    bin_name = args.bin
    if not bin_name:
        # Auto-detect from package name or Cargo.toml
        package = args.package
        if not package:
            # Read from Cargo.toml
            toml_path = args.manifest_path or os.path.join(manifest_dir, "Cargo.toml")
            try:
                with open(toml_path, "r") as f:
                    for line in f:
                        if line.strip().startswith("name"):
                            package = line.split("=")[1].strip().strip('"')
                            break
            except Exception:
                pass
        bin_name = package or "unknown"

    profile = args.profile
    target = args.target

    # Possible locations
    candidates = []
    for prof in [profile, "release" if profile == "bench" else profile]:
        if target:
            base = os.path.join(target_dir, target, prof)
        else:
            base = os.path.join(target_dir, prof)
        ext = ".exe" if os.name == "nt" else ""
        candidates.append(os.path.join(base, bin_name + ext))
        # Also check deps/ for benchmark binaries
        candidates.append(os.path.join(base, "deps", bin_name + ext))

    for c in candidates:
        if os.path.isfile(c):
            return c

    # Glob for deps/bin_name-* (Criterion suffixes)
    import glob
    for prof in [profile, "release" if profile == "bench" else profile]:
        if target:
            base = os.path.join(target_dir, target, prof, "deps")
        else:
            base = os.path.join(target_dir, prof, "deps")
        pattern = os.path.join(base, bin_name + "-*")
        matches = glob.glob(pattern)
        for m in matches:
            if os.path.isfile(m) and not m.endswith(".d") and os.access(m, os.X_OK):
                return m

    return None


def _do_build(args: argparse.Namespace) -> BuildResult:
    br = BuildResult()
    manifest_dir = os.path.dirname(os.path.abspath(args.manifest_path)) if args.manifest_path else PROJECT_ROOT

    # Build command: cargo build --release (not bench)
    cmd = ["cargo", "build"]
    if args.profile != "dev":
        cmd.extend(["--profile", args.profile])
    if args.bin:
        cmd.extend(["--bin", args.bin])
    if args.package:
        cmd.extend(["--package", args.package])
    if args.manifest_path:
        cmd.extend(["--manifest-path", args.manifest_path])
    if args.target:
        cmd.extend(["--target", args.target])
    if args.target_dir:
        cmd.extend(["--target-dir", args.target_dir])
    if args.all_features:
        cmd.append("--all-features")
    if args.no_default_features:
        cmd.append("--no-default-features")
    for f in (args.features or []):
        cmd.extend(["--features", f])
    if args.locked and not args.no_locked:
        cmd.append("--locked")
    cmd.extend(["--message-format", "json-render-diagnostics"])

    br.command = " ".join(shlex.quote(str(a)) for a in cmd)
    log(f"[build] {br.command}")

    start = time.time()
    proc = subprocess.run(cmd, cwd=manifest_dir, capture_output=True, text=True, timeout=args.timeout)
    br.duration_seconds = time.time() - start
    br.stderr = proc.stderr
    br.stdout = proc.stdout

    if proc.returncode != 0:
        return br

    # Parse JSON messages to find executable path
    for line in proc.stdout.strip().split("\n"):
        try:
            msg = json.loads(line)
        except json.JSONDecodeError:
            continue
        if msg.get("reason") == "compiler-artifact":
            target_info = msg.get("target", {})
            kinds = target_info.get("kind", [])
            if "bin" in kinds:
                exe_path = msg.get("executable")
                if exe_path and os.path.exists(exe_path):
                    br.executable_path = exe_path
                    br.executable_size = os.path.getsize(exe_path)
                    br.executable_sha256 = sha256_file(exe_path)
                    br.success = True
                    return br

    # Fallback: search common paths
    exe = _find_executable(manifest_dir, args)
    if exe and os.path.exists(exe):
        br.executable_path = exe
        br.executable_size = os.path.getsize(exe)
        br.executable_sha256 = sha256_file(exe)
        br.success = True

    return br


# ---------------------------------------------------------------------------
# Measurement: collect all binary-size metrics
# ---------------------------------------------------------------------------

class SizeMeasurement:
    def __init__(self):
        self.binary_path: str = ""
        self.binary_size_bytes: int = 0
        self.binary_sha256: str = ""
        self.compressed_size_bytes: int = 0
        self.sections: Dict[str, int] = {}
        self.per_crate: Dict[str, int] = {}
        self.largest_symbols: List[Dict[str, Any]] = []
        self.dependency_count: int = 0
        self.duplicate_dep_count: int = 0
        self.build_duration_seconds: float = 0.0
        self.warnings: List[str] = []


def collect_measurements(binary_path: str, manifest_dir: str, build_duration: float) -> SizeMeasurement:
    """Collect all binary-size metrics for a built executable."""
    m = SizeMeasurement()
    m.binary_path = binary_path
    m.binary_size_bytes = os.path.getsize(binary_path)
    m.binary_sha256 = sha256_file(binary_path)
    m.build_duration_seconds = build_duration

    # --- Compressed size ---
    try:
        m.compressed_size_bytes = gzip_size(binary_path)
    except Exception as e:
        m.warnings.append(f"gzip compression failed: {e}")

    # --- Section sizes via llvm-size ---
    if find_tool("llvm-size"):
        try:
            r = subprocess.run(["llvm-size", binary_path], capture_output=True, text=True, timeout=30,
                               cwd=manifest_dir)
            if r.returncode == 0:
                # Parse: "   text    data     bss     dec     hex filename"
                lines = r.stdout.strip().split("\n")
                for line in lines:
                    parts = line.split()
                    if len(parts) >= 4 and parts[0].isdigit():
                        m.sections["text"] = int(parts[0])
                        m.sections["data"] = int(parts[1])
                        m.sections["bss"] = int(parts[2]) if len(parts) > 2 else 0
                        break
        except Exception as e:
            m.warnings.append(f"llvm-size failed: {e}")
    else:
        m.warnings.append("llvm-size not found — section breakdown unavailable")

    # --- Per-crate breakdown via cargo bloat --crates ---
    if find_tool("cargo-bloat"):
        try:
            cmd = ["cargo", "bloat", "--release", "--crates", "-n", "30"]
            if manifest_dir != PROJECT_ROOT:
                cmd.extend(["--manifest-path", os.path.join(manifest_dir, "Cargo.toml")])
            r = subprocess.run(cmd, capture_output=True, text=True, timeout=120,
                               cwd=manifest_dir)
            if r.returncode == 0:
                # cargo bloat --crates output: lines like "34.8%  42.2% 147.8KiB std"
                for line in r.stdout.split("\n"):
                    # Match: percentage  percentage  size_with_unit  crate_name
                    # Use search (not match) because lines may have status text prepended
                    match = re.search(
                        r'[\d.]+%\s+[\d.]+%\s+([\d.]+)(KiB|MiB|B)\s+(\S+)',
                        line
                    )
                    if match:
                        size_val = float(match.group(1))
                        unit = match.group(2)
                        crate = match.group(3).strip()
                        if unit == "MiB":
                            size_bytes = int(size_val * 1024 * 1024)
                        elif unit == "KiB":
                            size_bytes = int(size_val * 1024)
                        else:
                            size_bytes = int(size_val)
                        if crate not in ("And", "filtered"):
                            m.per_crate[crate] = size_bytes
        except Exception as e:
            m.warnings.append(f"cargo bloat --crates failed: {e}")
    else:
        m.warnings.append("cargo-bloat not found — per-crate breakdown unavailable")

    # --- Largest symbols via cargo bloat ---
    if find_tool("cargo-bloat"):
        try:
            cmd = ["cargo", "bloat", "--release", "-n", "20"]
            if manifest_dir != PROJECT_ROOT:
                cmd.extend(["--manifest-path", os.path.join(manifest_dir, "Cargo.toml")])
            r = subprocess.run(cmd, capture_output=True, text=True, timeout=120,
                               cwd=manifest_dir)
            if r.returncode == 0:
                # Parse cargo-bloat output lines like:
                # "5.9%  7.2%  25.2KiB  ecs_hybrid  ecs_hybrid::main"
                for line in r.stdout.split("\n"):
                    match = re.search(
                        r'[\d.]+%\s+[\d.]+%\s+([\d.]+)(KiB|MiB|B)\s+(\S+)\s+(.+)',
                        line
                    )
                    if match:
                        size_val = float(match.group(1))
                        unit = match.group(2)
                        crate = match.group(3).strip()
                        name = match.group(4).strip()
                        if unit == "MiB":
                            size_bytes = int(size_val * 1024 * 1024)
                        elif unit == "KiB":
                            size_bytes = int(size_val * 1024)
                        else:
                            size_bytes = int(size_val)
                        m.largest_symbols.append({
                            "crate": crate,
                            "name": name,
                            "size_bytes": size_bytes,
                        })
        except Exception as e:
            m.warnings.append(f"cargo bloat failed: {e}")

    # --- Dependency count via cargo tree ---
    try:
        cmd = ["cargo", "tree", "--depth", "0", "-e", "no-dev"]
        if manifest_dir != PROJECT_ROOT:
            cmd.extend(["--manifest-path", os.path.join(manifest_dir, "Cargo.toml")])
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=30, cwd=manifest_dir)
        if r.returncode == 0:
            m.dependency_count = len([l for l in r.stdout.split("\n") if l.strip()]) - 1  # minus root
    except Exception as e:
        m.warnings.append(f"cargo tree failed: {e}")

    # --- Duplicate dependency count ---
    try:
        cmd = ["cargo", "tree", "-d"]
        if manifest_dir != PROJECT_ROOT:
            cmd.extend(["--manifest-path", os.path.join(manifest_dir, "Cargo.toml")])
        r = subprocess.run(cmd, capture_output=True, text=True, timeout=30, cwd=manifest_dir)
        if r.returncode == 0:
            m.duplicate_dep_count = len([l for l in r.stdout.split("\n") if l.strip()])
    except Exception as e:
        m.warnings.append(f"cargo tree -d failed: {e}")

    return m


def measurement_to_dict(m: SizeMeasurement) -> dict:
    """Serialize a SizeMeasurement to a JSON-compatible dict."""
    return {
        "binary_path": m.binary_path,
        "binary_size_bytes": m.binary_size_bytes,
        "binary_sha256": m.binary_sha256,
        "compressed_size_bytes": m.compressed_size_bytes,
        "sections": m.sections,
        "per_crate_bytes": m.per_crate,
        "largest_symbols": m.largest_symbols,
        "dependency_count": m.dependency_count,
        "duplicate_dep_count": m.duplicate_dep_count,
        "build_duration_seconds": m.build_duration_seconds,
        "warnings": m.warnings,
    }


def dict_to_summary(d: dict) -> str:
    """Produce a human-readable summary of measurement results."""
    lines = []
    lines.append(f"Binary size:       {d['binary_size_bytes']:,} bytes ({d['binary_size_bytes']/1024:.1f} KB)")
    lines.append(f"Compressed (gzip): {d['compressed_size_bytes']:,} bytes ({d['compressed_size_bytes']/1024:.1f} KB)")
    if d.get("sections"):
        sections = d["sections"]
        lines.append(f"Sections:          .text={sections.get('text', 0):,}  .data={sections.get('data', 0):,}  .bss={sections.get('bss', 0):,}")
    lines.append(f"Dependencies:      {d['dependency_count']} (duplicates: {d['duplicate_dep_count']})")
    lines.append(f"Build time:        {d['build_duration_seconds']:.1f}s")
    if d.get("per_crate_bytes"):
        lines.append("Top crates by .text size:")
        top = sorted(d["per_crate_bytes"].items(), key=lambda x: x[1], reverse=True)[:8]
        for crate, size in top:
            lines.append(f"  {crate:30s} {size:>8,} bytes ({size/1024:.1f} KB)")
    if d.get("largest_symbols"):
        lines.append("Largest symbols:")
        for sym in d["largest_symbols"][:10]:
            lines.append(f"  {sym['size_bytes']:>6,} B  {sym['crate']:20s} {sym['name']}")
    if d.get("warnings"):
        lines.append("Warnings:")
        for w in d["warnings"]:
            lines.append(f"  [!] {w}")
    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Metadata
# ---------------------------------------------------------------------------

def collect_metadata(args: argparse.Namespace) -> dict:
    """Collect environment metadata for reproducibility."""
    meta = {
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "rustc_version": "",
        "cargo_version": "",
        "target": args.target or "",
        "profile": args.profile,
        "features": args.features or [],
        "all_features": args.all_features,
        "no_default_features": args.no_default_features,
        "hostname": platform.node(),
        "platform": sys.platform,
        "python_version": sys.version.split()[0],
    }
    try:
        r = subprocess.run(["rustc", "--version"], capture_output=True, text=True, timeout=5)
        meta["rustc_version"] = r.stdout.strip()
    except Exception:
        pass
    try:
        r = subprocess.run(["cargo", "--version"], capture_output=True, text=True, timeout=5)
        meta["cargo_version"] = r.stdout.strip()
    except Exception:
        pass
    try:
        r = subprocess.run(["git", "rev-parse", "HEAD"], capture_output=True, text=True,
                           timeout=5, cwd=PROJECT_ROOT)
        meta["git_revision"] = r.stdout.strip()
    except Exception:
        pass
    return meta


# ---------------------------------------------------------------------------
# Baseline / Measure / Compare
# ---------------------------------------------------------------------------

def _measure_and_save(args: argparse.Namespace, run_name: str, run_dir: str,
                      compare_to: Optional[str] = None) -> int:
    """Build, measure, save to run_dir. Optionally compare to a baseline."""
    # Build
    exit_code, br = cmd_build(args)
    if exit_code != EXIT_SUCCESS:
        return exit_code

    # Collect measurements
    manifest_dir = os.path.dirname(os.path.abspath(args.manifest_path)) if args.manifest_path else PROJECT_ROOT
    measurement = collect_measurements(br.executable_path, manifest_dir, br.duration_seconds)
    mdict = measurement_to_dict(measurement)

    # Collect metadata
    meta = collect_metadata(args)
    meta["command"] = br.command
    meta["executable_sha256"] = br.executable_sha256

    # Save
    ensure_dir(run_dir)
    json_dump({"status": "success", "metadata": meta, "measurement": mdict},
              os.path.join(run_dir, "summary.json"))
    json_dump(meta, os.path.join(run_dir, "metadata.json"))

    # Write human-readable report
    report = f"""# Binary Size Measurement Report: {run_name}

**Timestamp:** {meta['timestamp']}
**Rust:** {meta.get('rustc_version', 'unknown')}
**Target:** {meta.get('target') or 'default'}
**Profile:** {meta['profile']}

## Binary Size
{dict_to_summary(mdict)}
"""
    with open(os.path.join(run_dir, "report.md"), "w") as f:
        f.write(report)

    # Update "latest" symlink-ish (copy)
    if os.path.exists(LATEST_DIR):
        shutil.rmtree(LATEST_DIR)
    shutil.copytree(run_dir, LATEST_DIR)

    print(f"\nCACHE_MEASUREMENT_RESULT={run_dir}")
    print(f"\n{dict_to_summary(mdict)}")

    # Compare if requested
    if compare_to:
        baseline_dir = os.path.join(BASELINES_DIR, compare_to)
        if not os.path.isdir(baseline_dir):
            # Check runs dir too
            baseline_dir = os.path.join(RUNS_DIR, compare_to)
        if os.path.isdir(baseline_dir):
            return _compare_dirs(baseline_dir, run_dir, args)
        else:
            log(f"Baseline '{compare_to}' not found. Saved as new baseline.", level="warn")
            return EXIT_SUCCESS

    return EXIT_SUCCESS


def _compare_dirs(baseline_dir: str, candidate_dir: str, args: argparse.Namespace) -> int:
    """Compare two saved measurement directories."""
    baseline = json_load(os.path.join(baseline_dir, "summary.json"))
    candidate = json_load(os.path.join(candidate_dir, "summary.json"))

    bm = baseline.get("measurement", baseline)
    cm = candidate.get("measurement", candidate)

    b_size = bm.get("binary_size_bytes", 0)
    c_size = cm.get("binary_size_bytes", 0)
    b_compressed = bm.get("compressed_size_bytes", 0)
    c_compressed = cm.get("compressed_size_bytes", 0)
    b_text = (bm.get("sections") or {}).get("text", 0)
    c_text = (cm.get("sections") or {}).get("text", 0)
    b_data = (bm.get("sections") or {}).get("data", 0)
    c_data = (cm.get("sections") or {}).get("data", 0)

    size_pct = ((c_size - b_size) / b_size * 100) if b_size else 0
    compressed_pct = ((c_compressed - b_compressed) / b_compressed * 100) if b_compressed else 0
    text_pct = ((c_text - b_text) / b_text * 100) if b_text else 0
    data_pct = ((c_data - b_data) / b_data * 100) if b_data else 0

    # Determine comparability
    comparable = True
    reasons = []
    if bm.get("binary_sha256") == cm.get("binary_sha256"):
        comparable = False
        reasons.append("Identical binary SHA-256 — same build")

    # Verdict
    max_growth = args.max_size_growth_percent
    if size_pct < -0.5:
        verdict = "IMPROVED"
    elif size_pct > max_growth:
        verdict = "REGRESSED"
    elif abs(size_pct) <= 0.5:
        verdict = "UNCHANGED"
    else:
        verdict = "INCONCLUSIVE"

    if not comparable:
        verdict = "NOT_COMPARABLE"

    lines = [
        f"\nComparison: {os.path.basename(baseline_dir)} -> {os.path.basename(candidate_dir)}",
        f"  Verdict:    {verdict}",
        f"  Comparable: {comparable}",
    ]
    if reasons:
        for r in reasons:
            lines.append(f"  Reason:     {r}")
    lines.append("  Differences:")
    lines.append(f"    binary_size:       {b_size:>10,} -> {c_size:>10,} bytes  ({size_pct:+.1f}%)")
    lines.append(f"    compressed (gzip): {b_compressed:>10,} -> {c_compressed:>10,} bytes  ({compressed_pct:+.1f}%)")
    lines.append(f"    .text section:     {b_text:>10,} -> {c_text:>10,} bytes  ({text_pct:+.1f}%)")
    lines.append(f"    .data section:     {b_data:>10,} -> {c_data:>10,} bytes  ({data_pct:+.1f}%)")

    # Per-crate comparison
    b_crates = bm.get("per_crate_bytes", {})
    c_crates = cm.get("per_crate_bytes", {})
    all_crates = sorted(set(list(b_crates.keys()) + list(c_crates.keys())))
    if all_crates:
        lines.append("  Per-crate changes:")
        for crate in all_crates:
            before = b_crates.get(crate, 0)
            after = c_crates.get(crate, 0)
            if before == 0 and after == 0:
                continue
            pct = ((after - before) / before * 100) if before else (100.0 if after else 0.0)
            if abs(pct) >= 1.0 or abs(after - before) >= 1024:
                lines.append(f"    {crate:30s} {before:>8,} -> {after:>8,} bytes  ({pct:+.1f}%)")

    print("\n".join(lines))

    if verdict == "IMPROVED":
        return EXIT_SUCCESS
    elif verdict == "REGRESSED":
        return EXIT_REGRESSION
    elif verdict == "NOT_COMPARABLE":
        return EXIT_NOT_COMPARABLE
    else:
        return EXIT_SUCCESS


# ---------------------------------------------------------------------------
# Command handlers
# ---------------------------------------------------------------------------

def cmd_baseline(args: argparse.Namespace) -> int:
    run_dir = os.path.join(BASELINES_DIR, args.name)
    if os.path.exists(run_dir) and not args.force:
        log(f"Baseline '{args.name}' already exists. Use --force to overwrite.", level="error")
        return EXIT_CONFIG_ERROR
    if os.path.exists(run_dir):
        shutil.rmtree(run_dir)
    log(f"[baseline] Building and measuring '{args.name}'...")
    return _measure_and_save(args, args.name, run_dir)


def cmd_measure(args: argparse.Namespace) -> int:
    run_dir = os.path.join(RUNS_DIR, args.name)
    if os.path.exists(run_dir) and not args.force:
        log(f"Run '{args.name}' already exists. Use --force to overwrite.", level="error")
        return EXIT_CONFIG_ERROR
    if os.path.exists(run_dir):
        shutil.rmtree(run_dir)
    log(f"[candidate] Building and measuring '{args.name}'...")
    return _measure_and_save(args, args.name, run_dir, args.compare_to)


def cmd_compare(args: argparse.Namespace) -> int:
    baseline_dir = os.path.join(BASELINES_DIR, args.baseline)
    if not os.path.isdir(baseline_dir):
        baseline_dir = os.path.join(RUNS_DIR, args.baseline)
    candidate_dir = os.path.join(RUNS_DIR, args.candidate)
    if not os.path.isdir(candidate_dir):
        candidate_dir = os.path.join(BASELINES_DIR, args.candidate)

    if not os.path.isdir(baseline_dir):
        log(f"Baseline '{args.baseline}' not found.", level="error")
        return EXIT_CONFIG_ERROR
    if not os.path.isdir(candidate_dir):
        log(f"Candidate '{args.candidate}' not found.", level="error")
        return EXIT_CONFIG_ERROR

    return _compare_dirs(baseline_dir, candidate_dir, args)


def cmd_analyze(args: argparse.Namespace) -> int:
    run_dir = os.path.join(RUNS_DIR, args.run)
    if not os.path.isdir(run_dir):
        run_dir = os.path.join(BASELINES_DIR, args.run)
    if not os.path.isdir(run_dir):
        log(f"Run '{args.run}' not found.", level="error")
        return EXIT_CONFIG_ERROR

    summary = json_load(os.path.join(run_dir, "summary.json"))
    m = summary.get("measurement", summary)
    meta = summary.get("metadata", {})

    print(f"\n=== Analysis: {args.run} ===")
    print(f"Timestamp: {meta.get('timestamp', 'unknown')}")
    print(f"Git: {meta.get('git_revision', 'unknown')[:12]}")
    print(f"Rust: {meta.get('rustc_version', 'unknown')}")
    print(f"Profile: {meta.get('profile', 'unknown')}")
    print()
    print(dict_to_summary(m))

    # Additional analysis: size per dependency, compression ratio
    size = m.get("binary_size_bytes", 0)
    compressed = m.get("compressed_size_bytes", 0)
    if size and compressed:
        ratio = (1 - compressed / size) * 100
        print(f"\nCompression ratio (gzip): {ratio:.1f}%")

    # Size distribution pie (text-based)
    sections = m.get("sections", {})
    total_section = sum(sections.values())
    if total_section > 0:
        print("\nSection distribution:")
        for name in ["text", "data", "bss"]:
            sec_size = sections.get(name, 0)
            bar_len = int(sec_size / total_section * 40) if total_section else 0
            print(f"  .{name:5s} {sec_size:>8,} bytes  {'█' * bar_len}")

    return EXIT_SUCCESS


def cmd_list(args: argparse.Namespace) -> int:
    print("\nBaselines:")
    if os.path.isdir(BASELINES_DIR):
        for name in sorted(os.listdir(BASELINES_DIR)):
            path = os.path.join(BASELINES_DIR, name)
            if os.path.isdir(path) and os.path.exists(os.path.join(path, "summary.json")):
                s = json_load(os.path.join(path, "summary.json"))
                size = s.get("measurement", s).get("binary_size_bytes", "?")
                print(f"  {name:30s}  {size:>10,} bytes")

    print("\nCandidate Runs:")
    if os.path.isdir(RUNS_DIR):
        for name in sorted(os.listdir(RUNS_DIR)):
            path = os.path.join(RUNS_DIR, name)
            if os.path.isdir(path) and os.path.exists(os.path.join(path, "summary.json")):
                s = json_load(os.path.join(path, "summary.json"))
                size = s.get("measurement", s).get("binary_size_bytes", "?")
                print(f"  {name:30s}  {size:>10,} bytes")

    return EXIT_SUCCESS


def cmd_run(args: argparse.Namespace) -> int:
    """Convenience: doctor + baseline + measure + compare."""
    # Doctor first
    cmd_doctor()

    # If no compare-to, save as baseline then measure
    if not args.compare_to:
        # Save as baseline
        run_dir = os.path.join(BASELINES_DIR, args.name)
        if os.path.exists(run_dir):
            shutil.rmtree(run_dir)
        exit_code = _measure_and_save(args, args.name, run_dir)
        if exit_code != EXIT_SUCCESS:
            return exit_code
        log(f"Baseline '{args.name}' saved.")
    else:
        # Measure and compare
        run_dir = os.path.join(RUNS_DIR, args.name)
        if os.path.exists(run_dir):
            shutil.rmtree(run_dir)
        exit_code = _measure_and_save(args, args.name, run_dir, args.compare_to)
        if exit_code != EXIT_SUCCESS and exit_code != EXIT_REGRESSION:
            return exit_code

    return EXIT_SUCCESS


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------

def main() -> int:
    global _global_quiet

    parser = build_parser()
    args = parser.parse_args()

    if not args.command:
        parser.print_help()
        return EXIT_CONFIG_ERROR

    _global_quiet = args.quiet

    if args.command == "doctor":
        return cmd_doctor()
    elif args.command == "baseline":
        return cmd_baseline(args)
    elif args.command == "measure":
        return cmd_measure(args)
    elif args.command == "compare":
        return cmd_compare(args)
    elif args.command == "analyze":
        return cmd_analyze(args)
    elif args.command == "list":
        return cmd_list(args)
    elif args.command == "run":
        return cmd_run(args)
    else:
        parser.print_help()
        return EXIT_CONFIG_ERROR


if __name__ == "__main__":
    sys.exit(main())
