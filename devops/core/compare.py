"""
Measurement comparison for the command line.

REQUIREMENTS: Python 3.8+ (standard library only).

DESCRIPTION
    Turns two stored measurements into a normalized list of metric deltas that
    can be printed as text or emitted as JSON. This is what makes the
    measure -> change -> measure -> compare loop usable without a browser: the
    Vite UI and this module answer the same question, one visually and one in
    a terminal or an agent's stdout.

    DIRECTION SEMANTICS. Every metric Pill Lab stores is a duration, so lower
    is better: a negative delta is an improvement, a positive one a
    regression. `better_when` is carried per metric anyway, so a
    higher-is-better metric (throughput) can be added later without inverting
    anything by hand.

    NOISE VERSUS SIGNAL. Two things must both hold before a change is called
    real:

      1. the relative change exceeds `NOISE_THRESHOLD`, and
      2. it is large compared to the spread of the samples behind it.

    (2) uses a standard-error heuristic, not a rigorous hypothesis test: the
    difference of means is divided by the combined standard error of the two
    runs, and a score of 2 or more is treated as significant. It exists
    because a developer machine routinely produces double-digit percentage
    swings between two identical runs - our own back-to-back `--quick`
    benchmark runs differed by up to 75% on individual benchmarks. A metric
    without dispersion data (a single-iteration timing) can never be marked
    significant, and is reported as such rather than guessed at.

    METRICS THAT DID NOT RUN. Criterion keeps its whole output tree in one
    directory, so `engine --bench <one target>` still reads back every other
    benchmark's previous result. Those carry an identical `run_epoch` on both
    sides of a comparison and are reported as "not re-run" instead of being
    counted as unchanged - otherwise a narrow run would look like broad
    evidence that nothing regressed.

    MIRRORED LOGIC. `src/lib/compare.ts` implements the same delta maths and
    the same threshold for the web UI. The two must stay in agreement; the
    threshold constant is duplicated deliberately and each file names the
    other. The significance heuristic is CLI-only, because the UI shows the
    confidence intervals directly instead.

--- SCRIPT ---
"""

import json
import statistics
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Dict, List, Optional, Sequence, Tuple

from . import CATEGORY_LABELS
from .paths import MEASUREMENTS_ROOT

# Relative change below which a difference is reported as noise. Mirrored by
# `NOISE_THRESHOLD` in `src/lib/compare.ts`; change both together.
NOISE_THRESHOLD = 0.02

# Standard-error multiples a difference must exceed to count as significant.
SIGNIFICANCE_SCORE = 2.0

# Minimum samples on BOTH sides before significance is claimed at all.
#
# A standard error computed from two or three samples is not informative: two
# back-to-back runs of the same code with Criterion's `--quick` (2 samples)
# produced 20 "significant" regressions on this workspace purely from
# scheduling noise. Below this count the comparison reports the change and
# says the sample size is too small, rather than dressing noise up as signal.
MINIMUM_SAMPLES_FOR_SIGNIFICANCE = 5


# =============================================================================
# Metric model
# =============================================================================


@dataclass
class MetricRow:
    """One comparable number extracted from a measurement.

    `key` is the identity used to pair a metric across two runs, so it must be
    stable between runs (a benchmark id, a case name), while `label` is only
    for display.
    """

    key: str
    label: str
    group: str
    unit: str
    value: float
    std_dev: Optional[float] = None
    samples: Optional[int] = None
    better_when: str = "lower"
    # Identifies the underlying measurement run for this specific metric. When
    # both sides carry the same stamp the metric was never re-measured between
    # the two measurements, and comparing it says nothing. Criterion keeps its
    # whole output tree in one directory, so `--bench <one target>` still
    # captures every other benchmark's previous result unchanged.
    freshness_stamp: Optional[float] = None


@dataclass
class MetricDelta:
    """One metric compared against its baseline counterpart."""

    key: str
    label: str
    group: str
    unit: str
    current: float
    baseline: float
    ratio: float
    absolute: float
    direction: str
    significant: bool
    significance_score: Optional[float]
    note: str

    def to_json(self) -> Dict[str, Any]:
        """Returns the delta as a plain dict for `--json` output."""
        return {
            "key": self.key,
            "label": self.label,
            "group": self.group,
            "unit": self.unit,
            "current": round(self.current, 4),
            "baseline": round(self.baseline, 4),
            "ratio": round(self.ratio, 6),
            "percent": round(self.ratio * 100.0, 3),
            "absolute": round(self.absolute, 4),
            "direction": self.direction,
            "significant": self.significant,
            "significance_score": (
                round(self.significance_score, 3)
                if self.significance_score is not None
                else None
            ),
            "note": self.note,
        }


@dataclass
class ComparisonResult:
    """The full comparison between two measurements of one category."""

    category: str
    current_info: Dict[str, Any]
    baseline_info: Dict[str, Any]
    deltas: List[MetricDelta] = field(default_factory=list)
    only_in_current: List[str] = field(default_factory=list)
    only_in_baseline: List[str] = field(default_factory=list)
    # Metrics both runs share but neither re-measured; excluded from every
    # total so they cannot be mistaken for evidence that nothing changed.
    not_rerun: List[str] = field(default_factory=list)
    threshold: float = NOISE_THRESHOLD

    @property
    def regressed(self) -> List[MetricDelta]:
        """Deltas that got worse, worst first."""
        return sorted(
            (delta for delta in self.deltas if delta.direction == "regressed"),
            key=lambda delta: abs(delta.ratio),
            reverse=True,
        )

    @property
    def improved(self) -> List[MetricDelta]:
        """Deltas that got better, largest gain first."""
        return sorted(
            (delta for delta in self.deltas if delta.direction == "improved"),
            key=lambda delta: abs(delta.ratio),
            reverse=True,
        )

    @property
    def unchanged(self) -> List[MetricDelta]:
        """Deltas inside the noise threshold."""
        return [delta for delta in self.deltas if delta.direction == "unchanged"]

    @property
    def significant_regressions(self) -> List[MetricDelta]:
        """Regressions that also cleared the significance heuristic."""
        return [delta for delta in self.regressed if delta.significant]

    def to_json(self) -> Dict[str, Any]:
        """Returns the whole comparison as a plain dict for `--json`."""
        return {
            "category": self.category,
            "category_label": CATEGORY_LABELS.get(self.category, self.category),
            "current": self.current_info,
            "baseline": self.baseline_info,
            "threshold_percent": round(self.threshold * 100.0, 3),
            "significance_score_required": SIGNIFICANCE_SCORE,
            "totals": {
                "compared": len(self.deltas),
                "regressed": len(self.regressed),
                "improved": len(self.improved),
                "unchanged": len(self.unchanged),
                "significant_regressions": len(self.significant_regressions),
                "only_in_current": len(self.only_in_current),
                "only_in_baseline": len(self.only_in_baseline),
                "not_rerun": len(self.not_rerun),
            },
            "metrics": [delta.to_json() for delta in self.deltas],
            "only_in_current": self.only_in_current,
            "only_in_baseline": self.only_in_baseline,
            "not_rerun": self.not_rerun,
        }


# =============================================================================
# Delta computation
# =============================================================================


def _standard_error(std_dev: Optional[float], samples: Optional[int]) -> Optional[float]:
    """Returns the standard error of a mean, or None without enough data.

    The sample floor is what stops a two-sample run from producing a
    confident-looking answer; see `MINIMUM_SAMPLES_FOR_SIGNIFICANCE`.
    """
    if std_dev is None or samples is None or std_dev <= 0:
        return None
    if samples < MINIMUM_SAMPLES_FOR_SIGNIFICANCE:
        return None
    return std_dev / (samples ** 0.5)


def compare_rows(current: MetricRow, baseline: MetricRow) -> Optional[MetricDelta]:
    """Compares one metric against its baseline counterpart.

    Returns None when the baseline value is zero or non-finite, so a metric
    that cannot produce a meaningful ratio is simply left out rather than
    reported as an infinite change.
    """
    if baseline.value == 0 or current.value != current.value:
        return None

    ratio = (current.value - baseline.value) / abs(baseline.value)
    magnitude = abs(ratio)

    direction = "unchanged"
    if magnitude >= NOISE_THRESHOLD:
        current_is_better = (
            current.value < baseline.value
            if current.better_when == "lower"
            else current.value > baseline.value
        )
        direction = "improved" if current_is_better else "regressed"

    # Combine both runs' standard errors; without dispersion on either side
    # the change cannot be called significant, only reported.
    current_error = _standard_error(current.std_dev, current.samples)
    baseline_error = _standard_error(baseline.std_dev, baseline.samples)
    score: Optional[float] = None
    note = ""
    if current_error is not None and baseline_error is not None:
        combined = (current_error ** 2 + baseline_error ** 2) ** 0.5
        if combined > 0:
            score = abs(current.value - baseline.value) / combined
    if score is None:
        smallest = min(
            value
            for value in (current.samples or 0, baseline.samples or 0)
        )
        note = (
            f"only {smallest} sample(s) - too few to judge significance"
            if smallest
            else "no dispersion data - significance unknown"
        )

    significant = direction != "unchanged" and score is not None and score >= SIGNIFICANCE_SCORE
    if direction != "unchanged" and score is not None and not significant:
        note = "within run-to-run spread"

    return MetricDelta(
        key=current.key,
        label=current.label,
        group=current.group,
        unit=current.unit,
        current=current.value,
        baseline=baseline.value,
        ratio=ratio,
        absolute=current.value - baseline.value,
        direction=direction,
        significant=significant,
        significance_score=score,
        note=note,
    )


# =============================================================================
# Per-category metric extraction
# =============================================================================


def _engine_rows(measurement: Dict[str, Any]) -> List[MetricRow]:
    """Extracts one row per Criterion benchmark (mean time, nanoseconds)."""
    rows: List[MetricRow] = []
    for benchmark in measurement.get("benchmarks", []) or []:
        rows.append(
            MetricRow(
                key=benchmark["id"],
                label=benchmark["id"],
                group=benchmark.get("group_prefix", "benchmarks"),
                unit="ns",
                value=float(benchmark.get("mean_ns", 0.0)),
                std_dev=benchmark.get("std_dev_ns"),
                samples=benchmark.get("iteration_count"),
                freshness_stamp=benchmark.get("run_epoch"),
            )
        )
    return rows


def _hot_reload_rows(measurement: Dict[str, Any]) -> List[MetricRow]:
    """Extracts reload wall times, their phase averages and host startup.

    Phases are included as their own rows so a comparison says not just "the
    reload got slower" but which part of it did.
    """
    rows: List[MetricRow] = []
    for case in measurement.get("cases", []) or []:
        summary = case.get("summary", {}) or {}
        iterations = case.get("iterations", []) or []
        walls = [entry["wall_ms"] for entry in iterations if "wall_ms" in entry]
        rows.append(
            MetricRow(
                key=case["name"],
                label=case["name"],
                group="reload",
                unit="ms",
                value=float(summary.get("avg_ms", 0.0)),
                std_dev=statistics.stdev(walls) if len(walls) > 1 else None,
                samples=len(walls) if walls else None,
            )
        )
        for phase, phase_value in (summary.get("phase_averages") or {}).items():
            phase_samples = [
                entry[phase] for entry in iterations if entry.get(phase) is not None
            ]
            rows.append(
                MetricRow(
                    key=f"{case['name']}.{phase}",
                    label=f"{case['name']} / {phase[:-3]}",
                    group="phase",
                    unit="ms",
                    value=float(phase_value),
                    std_dev=(
                        statistics.stdev(phase_samples) if len(phase_samples) > 1 else None
                    ),
                    samples=len(phase_samples) if phase_samples else None,
                )
            )

    for session in measurement.get("sessions", []) or []:
        startup = session.get("startup")
        if not startup:
            continue
        rows.append(
            MetricRow(
                key=f"startup:{session['name']}",
                label=f"startup ({session['name']})",
                group="startup",
                unit="ms",
                value=float(startup.get("wall_ms", 0.0)),
            )
        )
    return rows


def _cold_start_rows(measurement: Dict[str, Any]) -> List[MetricRow]:
    """Extracts one row per cold-start case, plus cargo unit counts."""
    rows: List[MetricRow] = []
    for case in measurement.get("cases", []) or []:
        samples = case.get("samples_ms")
        rows.append(
            MetricRow(
                key=case["name"],
                label=case["name"],
                group=case.get("kind", "case"),
                unit="ms",
                value=float(case.get("duration_ms", 0.0)),
                std_dev=(
                    statistics.stdev(samples) if samples and len(samples) > 1 else None
                ),
                samples=len(samples) if samples else None,
            )
        )
        # A build that suddenly compiles more units explains a slower build,
        # so the count is compared alongside the duration.
        timings = case.get("cargo_timings")
        if timings and timings.get("unit_count"):
            rows.append(
                MetricRow(
                    key=f"{case['name']}.units",
                    label=f"{case['name']} / units compiled",
                    group="cargo",
                    unit="units",
                    value=float(timings["unit_count"]),
                )
            )
    return rows


EXTRACTORS = {
    "engine": _engine_rows,
    "hot_reload": _hot_reload_rows,
    "cold_start": _cold_start_rows,
}


def extract_rows(envelope: Dict[str, Any]) -> List[MetricRow]:
    """Extracts the comparable metrics from a measurement envelope."""
    extractor = EXTRACTORS.get(envelope.get("category", ""))
    if extractor is None:
        return []
    return extractor(envelope.get("measurement", {}) or {})


# =============================================================================
# Measurement selection
# =============================================================================


def load_envelope(relative_file: str) -> Dict[str, Any]:
    """Loads one measurement by its manifest-relative path."""
    path = MEASUREMENTS_ROOT / relative_file
    return json.loads(path.read_text(encoding="utf-8"))


def resolve_selector(
    entries: Sequence[Dict[str, Any]], selector: str, role: str
) -> Dict[str, Any]:
    """Resolves a run selector against a category's history, newest first.

    Accepts `latest`/`newest`, `previous`/`prev`, a zero-based index, or any
    substring of the filename or timestamp. A substring matching several runs
    is an error listing the candidates rather than an arbitrary pick.
    """
    if not entries:
        raise RuntimeError(f"No measurements available to use as {role}.")

    normalized = selector.strip().lower()
    if normalized in ("latest", "newest", "current"):
        return entries[0]
    if normalized in ("previous", "prev", "baseline"):
        if len(entries) < 2:
            raise RuntimeError(
                f"Only one measurement exists; there is no previous run to use as {role}."
            )
        return entries[1]

    if normalized.lstrip("-").isdigit():
        index = int(normalized)
        if index < 0 or index >= len(entries):
            raise RuntimeError(
                f"{role} index {index} is out of range (0..{len(entries) - 1}, newest first)."
            )
        return entries[index]

    matches = [
        entry
        for entry in entries
        if normalized in entry["file"].lower() or normalized in entry["timestamp"].lower()
    ]
    if not matches:
        raise RuntimeError(
            f"No measurement matches {selector!r} for {role}. "
            f"Available: {', '.join(entry['file'] for entry in entries[:5])}"
        )
    if len(matches) > 1:
        listed = ", ".join(entry["file"] for entry in matches[:5])
        raise RuntimeError(f"{selector!r} matches {len(matches)} measurements: {listed}")
    return matches[0]


def describe_entry(entry: Dict[str, Any], envelope: Dict[str, Any]) -> Dict[str, Any]:
    """Builds the compact run description shown at the top of a comparison."""
    git = envelope.get("git", {}) or {}
    return {
        "file": entry["file"],
        "timestamp": envelope.get("timestamp", entry.get("timestamp", "")),
        "label": envelope.get("label", ""),
        "commit": git.get("commit_short", ""),
        "branch": git.get("branch", ""),
        "dirty": bool(git.get("dirty", False)),
    }


# =============================================================================
# Comparison
# =============================================================================


def compare_measurements(
    category: str,
    current_entry: Dict[str, Any],
    baseline_entry: Dict[str, Any],
) -> ComparisonResult:
    """Compares two stored measurements of the same category."""
    current_envelope = load_envelope(current_entry["file"])
    baseline_envelope = load_envelope(baseline_entry["file"])

    current_rows = {row.key: row for row in extract_rows(current_envelope)}
    baseline_rows = {row.key: row for row in extract_rows(baseline_envelope)}

    result = ComparisonResult(
        category=category,
        current_info=describe_entry(current_entry, current_envelope),
        baseline_info=describe_entry(baseline_entry, baseline_envelope),
    )

    for key, row in current_rows.items():
        baseline_row = baseline_rows.get(key)
        if baseline_row is None:
            result.only_in_current.append(key)
            continue
        # An identical freshness stamp means both measurements read the same
        # underlying result: the benchmark simply was not re-run, so its
        # "no change" carries no information and is reported separately.
        if (
            row.freshness_stamp is not None
            and row.freshness_stamp == baseline_row.freshness_stamp
        ):
            result.not_rerun.append(key)
            continue
        delta = compare_rows(row, baseline_row)
        if delta is not None:
            result.deltas.append(delta)
    result.only_in_baseline = [key for key in baseline_rows if key not in current_rows]
    result.only_in_current.sort()
    result.only_in_baseline.sort()
    result.not_rerun.sort()
    return result


# =============================================================================
# Rendering
# =============================================================================


def format_value(value: float, unit: str) -> str:
    """Formats a metric value with the largest sensible unit."""
    if unit == "units":
        return f"{value:.0f}"
    if unit == "ns":
        if value < 1_000:
            return f"{value:.2f}ns"
        if value < 1_000_000:
            return f"{value / 1_000:.2f}us"
        if value < 1_000_000_000:
            return f"{value / 1_000_000:.2f}ms"
        return f"{value / 1_000_000_000:.2f}s"
    if value < 1_000:
        return f"{value:.1f}ms"
    return f"{value / 1_000:.2f}s"


def _delta_line(delta: MetricDelta) -> str:
    """Renders one delta as a fixed-width, greppable line."""
    percent = f"{delta.ratio * 100:+.2f}%"
    flag = "significant" if delta.significant else (delta.note or "")
    return (
        f"    {percent:>9}  {delta.label:<44}  "
        f"{format_value(delta.current, delta.unit):>10} <- "
        f"{format_value(delta.baseline, delta.unit):<10}"
        + (f"  [{flag}]" if flag else "")
    )


def render_text(result: ComparisonResult, top: int = 15) -> str:
    """Renders a comparison as a terminal report.

    The wording is deliberately explicit ("slower" / "faster" / "no meaningful
    change") so the output can be read literally, by a person or by an agent,
    without inferring what the sign of a percentage means.
    """
    lines: List[str] = []
    label = CATEGORY_LABELS.get(result.category, result.category)
    lines.append("=" * 78)
    lines.append(f"  PILL LAB COMPARE - {label}")
    lines.append("=" * 78)

    for role, info in (("current", result.current_info), ("baseline", result.baseline_info)):
        dirty = " dirty" if info["dirty"] else ""
        lines.append(
            f"  {role:<9} {info['timestamp']}  {info['commit']}{dirty}  {info['file']}"
        )
        if info["label"]:
            lines.append(f"            {info['label']}")

    lines.append(
        f"  threshold {result.threshold * 100:.1f}%  |  significance: "
        f"|delta| >= {SIGNIFICANCE_SCORE:.0f} combined standard errors"
    )
    lines.append("")

    totals = result.to_json()["totals"]
    lines.append(
        f"  {totals['compared']} metrics compared - "
        f"{totals['regressed']} regressed, {totals['improved']} improved, "
        f"{totals['unchanged']} unchanged"
    )
    lines.append(
        f"  {totals['significant_regressions']} regression(s) clear the significance bar"
    )
    if result.not_rerun:
        # Stated before the results, because a reader who misses this can
        # conclude "nothing changed" from benchmarks that never ran.
        lines.append("")
        lines.append(
            f"  NOTE: {len(result.not_rerun)} metric(s) were NOT re-run between these "
            "two measurements"
        )
        lines.append(
            "        (same Criterion output on both sides) and are excluded above. "
            "Narrowing"
        )
        lines.append(
            "        a run with --bench still captures every other benchmark's "
            "previous result."
        )
    lines.append("")

    def section(title: str, deltas: List[MetricDelta]) -> None:
        """Appends one titled block, truncated to `top` entries."""
        if not deltas:
            return
        lines.append(f"  {title} ({len(deltas)})")
        shown = deltas if top <= 0 else deltas[:top]
        for delta in shown:
            lines.append(_delta_line(delta))
        if len(deltas) > len(shown):
            lines.append(f"    ... {len(deltas) - len(shown)} more (use --top 0 for all)")
        lines.append("")

    section("REGRESSED (slower)", result.regressed)
    section("IMPROVED (faster)", result.improved)

    if result.unchanged:
        lines.append(
            f"  NO MEANINGFUL CHANGE ({len(result.unchanged)}) - "
            f"within {result.threshold * 100:.1f}%"
        )
        lines.append("")

    if result.only_in_current:
        lines.append(f"  ONLY IN CURRENT ({len(result.only_in_current)})")
        for key in result.only_in_current[: max(top, 5)]:
            lines.append(f"    + {key}")
        lines.append("")
    if result.only_in_baseline:
        lines.append(f"  ONLY IN BASELINE ({len(result.only_in_baseline)})")
        for key in result.only_in_baseline[: max(top, 5)]:
            lines.append(f"    - {key}")
        lines.append("")

    if result.not_rerun:
        lines.append(f"  NOT RE-RUN ({len(result.not_rerun)}) - carried over, not compared")
        for key in result.not_rerun[: max(top, 5)]:
            lines.append(f"    = {key}")
        if len(result.not_rerun) > max(top, 5):
            lines.append(f"    ... {len(result.not_rerun) - max(top, 5)} more")
        lines.append("")

    if not result.deltas:
        lines.append("  No metrics could be paired between these two runs.")
        lines.append("")

    return "\n".join(lines)


def render_markdown(result: ComparisonResult, top: int = 15) -> str:
    """Renders a comparison as a Markdown table, for pasting into a PR."""
    label = CATEGORY_LABELS.get(result.category, result.category)
    lines = [
        f"### {label}",
        "",
        f"`{result.current_info['file']}` vs `{result.baseline_info['file']}`",
        "",
        "| Change | Metric | Current | Baseline | Significant |",
        "| --- | --- | ---: | ---: | --- |",
    ]
    ordered = result.regressed + result.improved
    shown = ordered if top <= 0 else ordered[:top]
    for delta in shown:
        lines.append(
            f"| {delta.ratio * 100:+.2f}% | {delta.label} | "
            f"{format_value(delta.current, delta.unit)} | "
            f"{format_value(delta.baseline, delta.unit)} | "
            f"{'yes' if delta.significant else 'no'} |"
        )
    if not shown:
        lines.append("| - | no changes beyond the noise threshold | | | |")
    return "\n".join(lines)
