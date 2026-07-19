#!/usr/bin/env python3
"""
Assembly Analysis Pipeline — ECS Hot-Path Auditor
=================================================
REQUIREMENTS: cargo-asm (cargo install cargo-asm)
              Python 3.9+
              Git (for diffing)

DESCRIPTION:
  Builds assembly for key ECS hot-loop functions via cargo-asm,
  parses the output to detect performance anti-patterns, and
  produces a ranked report of findings with fix suggestions.

USAGE:
  python assembly_analysis.py [--release] [--features profiling] [--output report.md]

EXAMPLE USAGE:
  python assembly_analysis.py --release --output performance_benchmarks/reports/asm_report.md
"""

import argparse
import json
import os
import re
import subprocess
import sys
import tempfile
from collections import Counter
from dataclasses import dataclass, field
from pathlib import Path
from typing import Optional

# ---------------------------------------------------------------------------
# Configuration — which functions to analyze and what to look for
# ---------------------------------------------------------------------------

# Each entry: (label, cargo-asm function spec, severity weight, binary type)
# binary: "lib" = library crate, "bench" = benchmark binary name
# Specs use the generic form with full trait paths — this is what cargo-asm
# matches against mangled symbol names in the binary.
ANALYSIS_TARGETS = [
    # --- Hot loops — require bench binary (monomorphised generics) ---
    (
        "Query iterator next() — generic hot loop",
        "QueryIterMut<Q,F> as core::iter::traits::iterator::Iterator>::next",
        10,
        "query_iteration",
    ),
    (
        "Query iterator — advance_archetype (cold path)",
        "QueryIterMut<Q,F>::advance_archetype",
        8,
        "query_iteration",
    ),
    # --- Medium priority — lib binary (needs ecs_hybrid:: prefix) ---
    (
        "Tick filter — TickFilterState",
        "ecs_hybrid::query::filter::TickFilterState",
        7,
        "lib",
    ),
    (
        "Component fetch — fetch_with_state (all impls)",
        "QueryTarget>::fetch_with_state",
        6,
        "lib",
    ),
    (
        "Query — iter_mut + par_iter_mut + archetype_matches",
        "ecs_hybrid::query::query::Query",
        6,
        "lib",
    ),
    # --- Lower priority — lib binary ---
    (
        "Engine — run_systems_parallel",
        "ecs_hybrid::engine::Engine::run_systems_parallel",
        5,
        "lib",
    ),
    (
        "Engine — process_frame",
        "ecs_hybrid::engine::Engine::process_frame",
        4,
        "lib",
    ),
    (
        "Scheduler — build_execution_graph",
        "ecs_hybrid::scheduler::SystemScheduler::build_execution_graph",
        3,
        "lib",
    ),
    (
        "World — move_entity_to_archetype",
        "ecs_hybrid::world::World::move_entity_to_archetype",
        3,
        "lib",
    ),
]

# ---------------------------------------------------------------------------
# Data types
# ---------------------------------------------------------------------------


@dataclass
class Finding:
    """A single performance concern found in the assembly."""

    category: str  # e.g. "call_in_loop", "atomic_op", "bounds_check"
    severity: str  # "high", "medium", "low"
    description: str
    location: str  # e.g. "line 42-48", ".Linner loop body"
    suggestion: str


@dataclass
class FunctionReport:
    """Analysis results for one function."""

    label: str
    function_spec: str
    success: bool
    error_message: str = ""
    instruction_count: int = 0
    call_count: int = 0
    lock_count: int = 0
    simd_instruction_count: int = 0
    scalar_float_count: int = 0
    branch_count: int = 0
    spill_count: int = 0
    loop_count: int = 0
    findings: list[Finding] = field(default_factory=list)
    raw_first_100_lines: str = ""


# ---------------------------------------------------------------------------
# Assembly parser
# ---------------------------------------------------------------------------


def parse_assembly(asm_text: str) -> dict:
    """Extract metrics from cargo-asm output."""
    lines = asm_text.split("\n")

    # Count instruction types
    instructions: list[str] = []
    calls: list[str] = []
    locks: list[str] = []
    simd: list[str] = []
    scalar_float: list[str] = []
    branches: list[str] = []
    spills: list[str] = []
    loops: list[str] = []

    # Regex patterns
    # An assembly instruction line typically looks like:
    #   \tmov rax, [rdi]    or    mov rax, [rdi]
    #   with optional leading whitespace and a label like .L123:
    instruction_pattern = re.compile(
        r"^\s*(?:[.]?[A-Za-z_][A-Za-z0-9_.]*:\s*)?\s*([a-z][a-z.]+)\s"
    )

    # SIMD: SSE/AVX floating-point instructions (packed or scalar with xmm/ymm)
    simd_pattern = re.compile(
        r"\b(?:mov[sa][ps]|add[ps]s|sub[ps]s|mul[ps]s|div[ps]s|"
        r"v?mov[sa][ps]|v?add[ps]s|v?sub[ps]s|v?mul[ps]s|v?div[ps]s|"
        r"cmp[ps]s|min[ps]s|max[ps]s|sqrt[ps]s|rcp[ps]s|rsqrt[ps]s)\b"
    )

    scalar_float_pattern = re.compile(
        r"\b(?:movss|addss|subss|mulss|divss|cmpss|minss|maxss|"
        r"vmovss|vaddss|vsubss|vmulss|vdivss)\b"
    )

    branch_pattern = re.compile(r"\b(?:jmp|je|jne|jz|jnz|ja|jae|jb|jbe|jg|jge|jl|jle|jo|jno|js|jns|call|ret)\b")

    spill_pattern = re.compile(r"mov\s+.*,\s*\[rsp[+\\-]")

    loop_label_pattern = re.compile(r"^\s*\.L[A-Za-z0-9_]+:\s*$")

    current_function = ""
    in_code_section = False

    for line in lines:
        # Track section headers from cargo-asm
        if line.startswith("Disassembly of") or line.startswith(";"):
            continue

        # Rust source interleave (--rust flag) starts with //
        if line.strip().startswith("//"):
            continue

        # Detect loop labels
        if loop_label_pattern.match(line.strip()):
            loops.append(line.strip())

        # Match instruction
        m = instruction_pattern.match(line)
        if m:
            instr = m.group(1)
            instructions.append(instr)

            if instr == "call":
                calls.append(line.strip())
            if instr.startswith("lock") or "lock " in line:
                locks.append(line.strip())
            if simd_pattern.search(line):
                simd.append(line.strip())
            if scalar_float_pattern.search(line):
                scalar_float.append(line.strip())
            if branch_pattern.search(line) and instr != "call":
                branches.append(line.strip())
            if spill_pattern.search(line):
                spills.append(line.strip())

    return {
        "instruction_count": len(instructions),
        "call_count": len(calls),
        "call_examples": calls[:10],
        "lock_count": len(locks),
        "lock_examples": locks[:5],
        "simd_count": len(simd),
        "scalar_float_count": len(scalar_float),
        "branch_count": len(branches),
        "spill_count": len(spills),
        "loop_count": len(loops),
    }


# ---------------------------------------------------------------------------
# Analysis rules
# ---------------------------------------------------------------------------


def analyze_function(label: str, asm_text: str, severity_weight: int) -> FunctionReport:
    """Run all analysis rules on a function's assembly output."""
    if not asm_text or asm_text.startswith("Error"):
        return FunctionReport(
            label=label,
            function_spec="",
            success=False,
            error_message=asm_text or "No output",
        )

    metrics = parse_assembly(asm_text)
    findings: list[Finding] = []
    lines = asm_text.split("\n")

    # --- Rule 1: call instructions in the output ---
    if metrics["call_count"] > 0:
        severity = "high" if metrics["call_count"] > 5 else "medium"
        findings.append(
            Finding(
                category="call_instructions",
                severity=severity,
                description=f"{metrics['call_count']} `call` instruction(s) found. "
                "Calls in hot-path code block inlining and add overhead.",
                location="throughout function",
                suggestion="Check that LTO is enabled (lto='fat'). "
                "Ensure called functions are marked #[inline] or are generic (monomorphised). "
                f"First 10 calls: {metrics['call_examples'][:10]}",
            )
        )

    # --- Rule 2: lock prefix (atomic operations) ---
    if metrics["lock_count"] > 0:
        findings.append(
            Finding(
                category="atomic_operations",
                severity="high",
                description=f"{metrics['lock_count']} `lock`-prefixed instruction(s) (atomic ops). "
                "Atomics are memory barriers (~20-50 cycles each).",
                location="throughout function",
                suggestion="Check for Arc::clone(), Mutex::lock(), or atomic counters in hot paths. "
                "Replace with non-atomic alternatives where possible. "
                f"Examples: {metrics['lock_examples'][:5]}",
            )
        )

    # --- Rule 3: SIMD vs scalar float ratio ---
    if metrics["scalar_float_count"] > 0 and metrics["simd_count"] == 0:
        findings.append(
            Finding(
                category="no_simd_vectorisation",
                severity="medium",
                description=f"{metrics['scalar_float_count']} scalar float ops, 0 SIMD (packed) ops. "
                "Loop not auto-vectorised.",
                location="hot loop body",
                suggestion="LLVM could not vectorise. Check: (1) no branches in loop body, "
                "(2) contiguous memory access, (3) no function calls in loop, "
                "(4) no loop-carried dependencies. "
                "Run 'cargo rustc -- -C remark=loop-vectorize' for LLVM's reason.",
            )
        )

    simd_pct = (
        (metrics["simd_count"] / max(metrics["simd_count"] + metrics["scalar_float_count"], 1))
        * 100
    )
    if metrics["scalar_float_count"] > 20 and simd_pct < 20:
        findings.append(
            Finding(
                category="low_simd_ratio",
                severity="low",
                description=f"SIMD ratio: {simd_pct:.0f}% ({metrics['simd_count']} packed / "
                f"{metrics['scalar_float_count']} scalar float ops).",
                location="throughout function",
                suggestion="Investigate auto-vectorisation blockers (see §14 of ASSEMBLY_101.md).",
            )
        )

    # --- Rule 4: register spills ---
    if metrics["spill_count"] > 10:
        severity = "medium" if metrics["spill_count"] > 30 else "low"
        findings.append(
            Finding(
                category="register_spills",
                severity=severity,
                description=f"{metrics['spill_count']} stack spill(s) detected. "
                "Register pressure — function has more live variables than registers.",
                location="throughout function",
                suggestion="Split large functions. Extract cold paths with #[inline(never)]. "
                "Reduce temporary variables in hot loop.",
            )
        )

    # --- Rule 5: bounds check patterns ---
    bounds_check_pattern = re.compile(r"cmp\s+.*,\s*\[rdi\+(?:8|16)\]")
    jae_after_cmp = False
    for i, line in enumerate(lines):
        if bounds_check_pattern.search(line):
            # Check if next instruction is a conditional jump (bounds check branch)
            if i + 1 < len(lines) and re.search(r"\b(?:jae|ja|jb)\b", lines[i + 1]):
                jae_after_cmp = True
                break
    if jae_after_cmp:
        findings.append(
            Finding(
                category="bounds_checks_present",
                severity="medium",
                description="Bounds check pattern (cmp + jae before memory load) detected.",
                location="before load instructions",
                suggestion="LLVM could not eliminate bounds checks. "
                "Ensure loop structure clearly bounds the index. "
                "Use iterators or get_unchecked() if bounds are provably correct.",
            )
        )

    # --- Rule 6: function size (icache pressure) ---
    if metrics["instruction_count"] > 500:
        findings.append(
            Finding(
                category="large_function",
                severity="low",
                description=f"Function is large ({metrics['instruction_count']} instructions). "
                "May cause instruction cache pressure.",
                location="entire function",
                suggestion="Consider splitting into smaller functions. "
                "Mark cold paths with #[cold] or #[inline(never)].",
            )
        )

    # --- Rule 7: zero loops in expected hot-path ---
    if severity_weight >= 7 and metrics["loop_count"] == 0 and metrics["instruction_count"] > 20:
        findings.append(
            Finding(
                category="no_loops_detected",
                severity="low",
                description="No loop labels detected in a function expected to contain loops.",
                location="entire function",
                suggestion="Function may have been fully unrolled or inlined. "
                "Verify with --rust flag that the expected loop is present.",
            )
        )

    # --- Rule 8: lots of branches (mispredict risk) ---
    branch_rate = metrics["branch_count"] / max(metrics["instruction_count"], 1) * 100
    if branch_rate > 25:
        findings.append(
            Finding(
                category="high_branch_density",
                severity="medium",
                description=f"Branch density: {branch_rate:.0f}% ({metrics['branch_count']} branches / "
                f"{metrics['instruction_count']} instructions). "
                "High branch density increases mispredict risk.",
                location="throughout function",
                suggestion="Consider converting unpredictable branches to conditional moves (cmov) "
                "or restructuring control flow. LLVM does this automatically when profitable.",
            )
        )

    # --- Compile the report ---
    first_lines = "\n".join(lines[:100]) if len(lines) > 0 else ""

    return FunctionReport(
        label=label,
        function_spec="",
        success=True,
        instruction_count=metrics["instruction_count"],
        call_count=metrics["call_count"],
        lock_count=metrics["lock_count"],
        simd_instruction_count=metrics["simd_count"],
        scalar_float_count=metrics["scalar_float_count"],
        branch_count=metrics["branch_count"],
        spill_count=metrics["spill_count"],
        loop_count=metrics["loop_count"],
        findings=findings,
        raw_first_100_lines=first_lines,
    )


# ---------------------------------------------------------------------------
# Build phase — run cargo-asm for each target
# ---------------------------------------------------------------------------


def build_assembly(
    function_spec: str, binary: str, args: argparse.Namespace
) -> tuple[str, str]:
    """Run cargo-asm and return (stdout, stderr)."""
    cmd = ["cargo", "asm"]
    if binary == "lib":
        cmd.append("--lib")
    else:
        cmd.extend(["--bench", binary])
    if args.release:
        cmd.append("--release")
    cmd.extend(["--rust", "--intel", "--simplify"])
    if args.features:
        cmd.extend(["--features", args.features])

    cmd.append(function_spec)

    try:
        result = subprocess.run(
            cmd,
            capture_output=True,
            text=True,
            timeout=120,
            cwd=args.project_root,
        )
        stdout = result.stdout
        stderr = result.stderr
        # cargo-asm returns "Try one of those..." with sequence numbers
        # when there are multiple matches. Pick the first (usually the
        # most relevant monomorphisation).
        if "Try one of those" in stdout:
            cmd.append("0")
            result2 = subprocess.run(
                cmd, capture_output=True, text=True, timeout=120,
                cwd=args.project_root,
            )
            stdout = result2.stdout
            stderr = result2.stderr
        return stdout, stderr
    except subprocess.TimeoutExpired:
        return "", "Timeout: cargo asm took >120s"
    except FileNotFoundError:
        return "", "Error: cargo-asm not found. Install with: cargo install cargo-asm"


def build_all(
    targets: list[tuple[str, str, int, str]], args: argparse.Namespace
) -> list[tuple[str, str, str, int, str]]:
    """Build assembly for all targets.
    Returns [(label, spec, asm_text, weight, binary)]."""
    results = []
    print(f"\n{'='*60}")
    print(f"BUILD PHASE: Generating assembly for {len(targets)} functions")
    print(f"{'='*60}\n")

    for i, (label, spec, weight, binary) in enumerate(targets):
        print(f"[{i+1}/{len(targets)}] {label}")
        print(f"    spec: {spec}  (binary: {binary})")
        stdout, stderr = build_assembly(spec, binary, args)
        if stderr:
            print(f"    WARNING: {stderr.strip()[:120]}")
        if stdout and "Error" not in stdout and "Try one of those" not in stdout:
            line_count = len(stdout.split("\n"))
            print(f"    OK — {line_count} lines of assembly")
        else:
            print(f"    FAILED — {stderr.strip()[:120] if stderr else 'no matching symbols'}")
            stdout = f"Error: {stderr}" if stderr else "Error: no matching symbols"
        results.append((label, spec, stdout, weight, binary))

    return results


# ---------------------------------------------------------------------------
# Report generation
# ---------------------------------------------------------------------------


def generate_report(
    reports: list[FunctionReport],
    build_results: list[tuple[str, str, str, int, str]],
    args: argparse.Namespace,
) -> str:
    """Generate a Markdown report from analysis results."""
    lines: list[str] = []

    lines.append("# Assembly Analysis Report — ECS Hot-Path Audit")
    lines.append("")
    lines.append(f"**Generated:** {subprocess.run(['date', '+%Y-%m-%d %H:%M:%S'], capture_output=True, text=True, shell=True).stdout.strip() or 'unknown'}")
    lines.append(f"**Profile:** {'bench' if not args.release else 'release'}")
    lines.append(f"**Features:** {args.features or 'none'}")
    lines.append(f"**Project root:** {args.project_root}")
    lines.append("")

    # --- Executive summary ---
    lines.append("## Executive Summary")
    lines.append("")

    total_findings = sum(len(r.findings) for r in reports if r.success)
    high_findings = sum(1 for r in reports if r.success for f in r.findings if f.severity == "high")
    medium_findings = sum(1 for r in reports if r.success for f in r.findings if f.severity == "medium")
    low_findings = sum(1 for r in reports if r.success for f in r.findings if f.severity == "low")
    failed = sum(1 for r in reports if not r.success)
    successful = sum(1 for r in reports if r.success)

    lines.append(f"| Metric | Value |")
    lines.append(f"|--------|-------|")
    lines.append(f"| Functions analyzed | {len(reports)} |")
    lines.append(f"| Successfully built | {successful} |")
    lines.append(f"| Failed to build | {failed} |")
    lines.append(f"| **Total findings** | **{total_findings}** |")
    lines.append(f"| ├─ High severity | {high_findings} |")
    lines.append(f"| ├─ Medium severity | {medium_findings} |")
    lines.append(f"| └─ Low severity | {low_findings} |")
    lines.append("")

    if high_findings > 0:
        lines.append("### Critical Issues (High Severity)")
        lines.append("")
        for r in reports:
            if not r.success:
                continue
            for f in r.findings:
                if f.severity == "high":
                    lines.append(f"- **[{r.label}]** {f.description}")
                    lines.append(f"  → {f.suggestion}")
        lines.append("")

    # --- Per-function detail ---
    lines.append("## Per-Function Analysis")
    lines.append("")

    # Sort by severity weight (hot loops first)
    report_order = sorted(
        zip(reports, build_results),
        key=lambda x: x[1][3],
        reverse=True,
    )

    for report, (label, spec, _, weight, binary) in report_order:
        binary_flag = f"--bench {binary}" if binary != "lib" else "--lib"
        lines.append(f"### {label} (weight={weight})")
        lines.append("")
        lines.append(f"```bash")
        lines.append(f"cargo asm {binary_flag} --release --rust --intel --simplify \\")
        lines.append(f'    "{spec}"')
        lines.append(f"```")
        lines.append("")

        if not report.success:
            lines.append(f"**❌ BUILD FAILED:** {report.error_message}")
            lines.append("")
            continue

        # Metrics table
        lines.append("| Metric | Value | Threshold | Status |")
        lines.append("|--------|-------|-----------|--------|")
        lines.append(f"| Instructions | {report.instruction_count} | < 500 ideal | {'⚠️' if report.instruction_count > 500 else '✅'} |")
        lines.append(f"| `call` instructions | {report.call_count} | **0 in hot loop** | {'❌' if report.call_count > 0 else '✅'} |")
        lines.append(f"| `lock` (atomic) ops | {report.lock_count} | **0** | {'❌' if report.lock_count > 0 else '✅'} |")
        lines.append(f"| SIMD (packed) ops | {report.simd_instruction_count} | > 0 ideal | {'⚠️' if report.simd_instruction_count == 0 and report.scalar_float_count > 10 else '✅'} |")
        lines.append(f"| Scalar float ops | {report.scalar_float_count} | — | — |")
        lines.append(f"| Branches | {report.branch_count} | < 25% density | {'⚠️' if report.branch_count / max(report.instruction_count, 1) > 0.25 else '✅'} |")
        lines.append(f"| Stack spills | {report.spill_count} | < 10 ideal | {'⚠️' if report.spill_count > 10 else '✅'} |")
        lines.append(f"| Loop labels | {report.loop_count} | > 0 | {'⚠️' if report.loop_count == 0 and weight >= 7 else '✅'} |")
        lines.append("")

        if report.findings:
            lines.append("#### Findings")
            lines.append("")
            for f in report.findings:
                severity_icon = {"high": "🔴", "medium": "🟡", "low": "🟢"}.get(f.severity, "⚪")
                lines.append(f"{severity_icon} **[{f.severity.upper()}] {f.category}**")
                lines.append(f"  - {f.description}")
                lines.append(f"  - Location: {f.location}")
                lines.append(f"  - Suggestion: {f.suggestion}")
                lines.append("")
        else:
            lines.append("✅ **No issues detected.**")
            lines.append("")

        # Raw assembly excerpt
        lines.append("<details>")
        lines.append("<summary>Assembly excerpt (first 100 lines)</summary>")
        lines.append("")
        lines.append("```asm")
        lines.append(report.raw_first_100_lines if report.raw_first_100_lines else "(empty)")
        lines.append("```")
        lines.append("</details>")
        lines.append("")

    # --- Summary table ---
    lines.append("## Findings Summary Table")
    lines.append("")
    lines.append("| Function | Instr | Calls | Locks | SIMD | Branches | Spills | Loops | Findings |")
    lines.append("|----------|------:|------:|------:|-----:|---------:|-------:|------:|---------:|")
    for report, (label, _, _, _, _) in report_order:
        if not report.success:
            lines.append(
                f"| {label[:40]} | — | — | — | — | — | — | — | ❌ BUILD FAILED |"
            )
        else:
            finding_summary = ", ".join(
                f"{f.severity[0].upper()}:{f.category.split('_')[0]}"
                for f in report.findings[:3]
            ) or "—"
            lines.append(
                f"| {label[:40]} | {report.instruction_count} | {report.call_count} | "
                f"{report.lock_count} | {report.simd_instruction_count} | {report.branch_count} | "
                f"{report.spill_count} | {report.loop_count} | {finding_summary} |"
            )
    lines.append("")

    # --- Recommendations ---
    lines.append("## Top Recommendations")
    lines.append("")

    all_findings = [f for r in reports if r.success for f in r.findings]
    by_category: dict[str, list[Finding]] = {}
    for f in all_findings:
        by_category.setdefault(f.category, []).append(f)

    ranked_categories = sorted(by_category.items(), key=lambda x: len(x[1]), reverse=True)

    for i, (category, findings_list) in enumerate(ranked_categories[:5]):
        lines.append(f"{i+1}. **{category.replace('_', ' ').title()}** "
                     f"({len(findings_list)} occurrence(s))")
        lines.append(f"   - {findings_list[0].suggestion}")
        lines.append("")

    return "\n".join(lines)


# ---------------------------------------------------------------------------
# Main
# ---------------------------------------------------------------------------


def main():
    parser = argparse.ArgumentParser(
        description="Assembly Analysis Pipeline — ECS Hot-Path Auditor"
    )
    parser.add_argument(
        "--release",
        action="store_true",
        default=True,
        help="Use release profile (default). Use --no-release for bench profile.",
    )
    parser.add_argument(
        "--no-release",
        action="store_false",
        dest="release",
        help="Use bench profile (faster builds).",
    )
    parser.add_argument(
        "--features",
        type=str,
        default=None,
        help="Feature flags to pass to cargo (e.g. 'profiling').",
    )
    parser.add_argument(
        "--output",
        type=str,
        default="performance_benchmarks/reports/asm_report.md",
        help="Output Markdown report path.",
    )
    parser.add_argument(
        "--project-root",
        type=str,
        default=None,
        help="Project root directory. Auto-detected if not specified.",
    )
    parser.add_argument(
        "--json",
        type=str,
        default=None,
        help="Also output machine-readable JSON to this path.",
    )
    parser.add_argument(
        "--quick",
        action="store_true",
        help="Only analyze top-5 hot-loop functions (fast mode).",
    )
    args = parser.parse_args()

    # Auto-detect project root
    if args.project_root is None:
        script_dir = Path(__file__).resolve().parent
        # Walk up to find Cargo.toml
        current = script_dir
        for _ in range(10):
            if (current / "Cargo.toml").exists():
                args.project_root = str(current)
                break
            current = current.parent
        else:
            args.project_root = str(script_dir)

    print(f"Project root: {args.project_root}")
    os.chdir(args.project_root)

    # Verify cargo-asm is available
    try:
        subprocess.run(["cargo", "asm", "--version"], capture_output=True, check=True)
    except (subprocess.CalledProcessError, FileNotFoundError):
        print("ERROR: cargo-asm not found. Install with: cargo install cargo-asm")
        sys.exit(1)

    # --- Build phase ---
    targets = ANALYSIS_TARGETS[:5] if args.quick else ANALYSIS_TARGETS
    print(f"Analyzing {len(targets)} function(s)...")
    build_results = build_all(list(targets), args)

    # --- Analysis phase ---
    reports: list[FunctionReport] = []
    for label, spec, asm_text, weight, binary in build_results:
        report = analyze_function(label, asm_text, weight)
        report.function_spec = spec
        reports.append(report)

    # --- Report phase ---
    print(f"\n{'='*60}")
    print("ANALYSIS COMPLETE")
    print(f"{'='*60}")

    total = sum(len(r.findings) for r in reports if r.success)
    high = sum(1 for r in reports if r.success for f in r.findings if f.severity == "high")
    medium = sum(1 for r in reports if r.success for f in r.findings if f.severity == "medium")
    low = sum(1 for r in reports if r.success for f in r.findings if f.severity == "low")
    print(f"Total findings: {total} ({high} high, {medium} medium, {low} low)")

    report_text = generate_report(reports, build_results, args)

    # Write report
    output_path = Path(args.output)
    output_path.parent.mkdir(parents=True, exist_ok=True)
    output_path.write_text(report_text, encoding="utf-8")
    print(f"\nReport written to: {output_path}")

    # JSON output (optional)
    if args.json:
        json_data = {
            "summary": {
                "functions_analyzed": len(reports),
                "successful": sum(1 for r in reports if r.success),
                "failed": sum(1 for r in reports if not r.success),
                "total_findings": total,
                "high_severity": high,
                "medium_severity": medium,
                "low_severity": low,
            },
            "functions": [
                {
                    "label": r.label,
                    "spec": r.function_spec,
                    "success": r.success,
                    "error": r.error_message if not r.success else None,
                    "metrics": {
                        "instructions": r.instruction_count,
                        "calls": r.call_count,
                        "locks": r.lock_count,
                        "simd_ops": r.simd_instruction_count,
                        "scalar_float_ops": r.scalar_float_count,
                        "branches": r.branch_count,
                        "spills": r.spill_count,
                        "loops": r.loop_count,
                    },
                    "findings": [
                        {
                            "category": f.category,
                            "severity": f.severity,
                            "description": f.description,
                            "suggestion": f.suggestion,
                        }
                        for f in r.findings
                    ],
                }
                for r in reports
            ],
        }
        Path(args.json).write_text(json.dumps(json_data, indent=2))
        print(f"JSON data written to: {args.json}")

    # Exit code reflects whether high-severity issues were found
    if high > 0:
        print("\n⚠️  High-severity issues detected. Review the report.")
        sys.exit(1)
    else:
        print("\n✅ No high-severity issues found.")


if __name__ == "__main__":
    main()
