// DESCRIPTION: Baseline comparison between two stored measurements.
//
//   Direction semantics are explicit and set per metric. Every metric Pill Lab
//   currently compares is a latency or duration, where lower is better - so a
//   negative delta is an improvement and a positive delta is a regression.
//   Throughput would be the opposite, which is why `betterWhen` is a parameter
//   rather than an assumption baked into the maths.
//
//   Deltas below `NOISE_THRESHOLD` are reported as "no meaningful change"
//   instead of being dressed up as a result: benchmark and build timings on a
//   developer machine simply do not resolve below a couple of percent.
//
// --- SCRIPT ---

import type { ChangeDirection } from '../types/measurement';

/**
 * Relative change below which a difference is treated as noise.
 *
 * Mirrored by `NOISE_THRESHOLD` in `devops/core/compare.py`, which powers
 * the `pill_lab.py compare` command. Change both together or the UI and the
 * CLI will disagree about what counts as a regression.
 */
export const NOISE_THRESHOLD = 0.02;

export type BetterWhen = 'lower' | 'higher';

export interface Delta {
  /** Signed relative change, current versus baseline (0.042 = +4.2%). */
  ratio: number;
  absolute: number;
  current: number;
  baseline: number;
  direction: ChangeDirection;
  /** Ready-to-render phrase, e.g. "4.2% slower". */
  label: string;
}

/**
 * Computes the delta of a current value against a baseline.
 *
 * Returns null when either side is missing or the baseline is zero, so a
 * metric that cannot be compared is simply not shown rather than rendered as
 * an infinite or nonsensical change.
 */
export function computeDelta(
  current: number | null | undefined,
  baseline: number | null | undefined,
  betterWhen: BetterWhen = 'lower',
): Delta | null {
  if (current === null || current === undefined) return null;
  if (baseline === null || baseline === undefined) return null;
  if (!Number.isFinite(current) || !Number.isFinite(baseline) || baseline === 0) return null;

  const ratio = (current - baseline) / Math.abs(baseline);
  const magnitude = Math.abs(ratio);
  let direction: ChangeDirection = 'unchanged';
  if (magnitude >= NOISE_THRESHOLD) {
    const currentIsBetter = betterWhen === 'lower' ? current < baseline : current > baseline;
    direction = currentIsBetter ? 'improved' : 'regressed';
  }

  return {
    ratio,
    absolute: current - baseline,
    current,
    baseline,
    direction,
    label: describeDelta(ratio, direction, betterWhen),
  };
}

/** Builds the human phrase for a delta, matching its direction semantics. */
function describeDelta(
  ratio: number,
  direction: ChangeDirection,
  betterWhen: BetterWhen,
): string {
  if (direction === 'unchanged') return 'no meaningful change';
  const percent = Math.abs(ratio * 100).toFixed(1);
  if (betterWhen === 'lower') {
    return direction === 'improved' ? `${percent}% faster` : `${percent}% slower`;
  }
  return direction === 'improved' ? `${percent}% higher` : `${percent}% lower`;
}

/** Indexes a list of records by a key selector, for baseline lookups. */
export function indexBy<Item>(items: Item[], key: (item: Item) => string): Map<string, Item> {
  const index = new Map<string, Item>();
  for (const item of items) index.set(key(item), item);
  return index;
}

/** Counts how many deltas fall into each direction. */
export function tallyDirections(deltas: (Delta | null)[]): Record<ChangeDirection, number> {
  const tally: Record<ChangeDirection, number> = { improved: 0, regressed: 0, unchanged: 0 };
  for (const delta of deltas) {
    if (delta) tally[delta.direction] += 1;
  }
  return tally;
}
