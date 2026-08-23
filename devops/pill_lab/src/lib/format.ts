// DESCRIPTION: Value formatting for the Pill Lab UI. Python stores raw numbers
//   in their native units (nanoseconds for Criterion, milliseconds for reload
//   and build phases); every human-readable rendering happens here, so unit
//   handling exists once and cannot drift between report views.
//
// --- SCRIPT ---

/** Formats a nanosecond duration with the largest sensible unit. */
export function formatNanoseconds(nanoseconds: number): string {
  const value = Number(nanoseconds);
  if (!Number.isFinite(value)) return '-';
  if (value < 1_000) return `${value.toFixed(2)} ns`;
  if (value < 1_000_000) return `${(value / 1_000).toFixed(2)} µs`;
  if (value < 1_000_000_000) return `${(value / 1_000_000).toFixed(2)} ms`;
  return `${(value / 1_000_000_000).toFixed(2)} s`;
}

/** Formats a millisecond duration, switching to seconds past one second. */
export function formatMilliseconds(milliseconds: number | undefined | null): string {
  if (milliseconds === undefined || milliseconds === null || !Number.isFinite(milliseconds)) {
    return '-';
  }
  if (milliseconds < 1) return `${(milliseconds * 1000).toFixed(0)} µs`;
  if (milliseconds < 1_000) return `${milliseconds.toFixed(milliseconds < 10 ? 2 : 0)} ms`;
  if (milliseconds < 60_000) return `${(milliseconds / 1_000).toFixed(2)} s`;
  const minutes = Math.floor(milliseconds / 60_000);
  const seconds = (milliseconds % 60_000) / 1_000;
  return `${minutes}m ${seconds.toFixed(0)}s`;
}

/** Formats a second-valued duration the same way as milliseconds. */
export function formatSeconds(seconds: number | undefined | null): string {
  if (seconds === undefined || seconds === null) return '-';
  return formatMilliseconds(seconds * 1000);
}

/** Formats a ratio (0.042) as a signed percentage ("+4.20%"). */
export function formatRatioPercent(ratio: number): string {
  return `${(ratio * 100).toFixed(2).replace(/^(?!-)/, '+')}%`;
}

/** Formats an already-percentage number as a signed percentage. */
export function formatPercent(percent: number): string {
  return `${percent.toFixed(2).replace(/^(?!-)/, '+')}%`;
}

/** Formats a per-second throughput with a K/M/G suffix. */
export function formatThroughput(value: number | null | undefined): string {
  if (value === null || value === undefined || !Number.isFinite(value)) return '';
  if (value >= 1_000_000_000) return `${(value / 1_000_000_000).toFixed(2)} G/s`;
  if (value >= 1_000_000) return `${(value / 1_000_000).toFixed(2)} M/s`;
  if (value >= 1_000) return `${(value / 1_000).toFixed(2)} K/s`;
  return `${value.toFixed(2)} /s`;
}

/** Formats a byte count for the measurement picker. */
export function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`;
  if (bytes < 1024 * 1024) return `${(bytes / 1024).toFixed(1)} KB`;
  return `${(bytes / (1024 * 1024)).toFixed(1)} MB`;
}

/**
 * Renders a stored ISO timestamp as `YYYY-MM-DD HH:MM:SS`.
 *
 * The stored string already carries the machine's UTC offset, so it is parsed
 * and re-rendered rather than string-sliced, keeping the displayed time in the
 * reader's own timezone.
 */
export function formatTimestamp(isoTimestamp: string): string {
  const parsed = new Date(isoTimestamp);
  if (Number.isNaN(parsed.getTime())) return isoTimestamp;
  const pad = (value: number): string => String(value).padStart(2, '0');
  return (
    `${parsed.getFullYear()}-${pad(parsed.getMonth() + 1)}-${pad(parsed.getDate())} ` +
    `${pad(parsed.getHours())}:${pad(parsed.getMinutes())}:${pad(parsed.getSeconds())}`
  );
}

/** Renders a timestamp as a compact relative age ("3h ago"). */
export function formatRelativeAge(isoTimestamp: string): string {
  const parsed = new Date(isoTimestamp);
  if (Number.isNaN(parsed.getTime())) return '';
  const seconds = (Date.now() - parsed.getTime()) / 1000;
  if (seconds < 90) return 'just now';
  if (seconds < 3600) return `${Math.round(seconds / 60)}m ago`;
  if (seconds < 86400) return `${Math.round(seconds / 3600)}h ago`;
  return `${Math.round(seconds / 86400)}d ago`;
}

/** Splits `group/parameter` into its name and trailing parameter badge. */
export function splitBenchmarkId(fullId: string): { name: string; parameter: string } {
  const lastSlash = fullId.lastIndexOf('/');
  if (lastSlash < 0) return { name: fullId, parameter: '' };
  return { name: fullId.slice(0, lastSlash), parameter: fullId.slice(lastSlash + 1) };
}

/** Turns a snake_case identifier into a Title Cased label. */
export function humanizeIdentifier(identifier: string): string {
  return identifier
    .replace(/_/g, ' ')
    .replace(/\b\w/g, (character) => character.toUpperCase());
}
