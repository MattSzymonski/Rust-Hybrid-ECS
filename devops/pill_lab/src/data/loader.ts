// DESCRIPTION: Measurement discovery and loading. The frontend never knows an
//   individual filename: it reads `measurements/index.json`, which `pill_lab.py`
//   regenerates from disk after every run, and fetches files by the paths that
//   manifest lists. The same `/measurements/` URL works in `vite dev` and in a
//   production build because `vite.config.ts` serves the directory under that
//   prefix in both modes.
//
// --- SCRIPT ---

import {
  CATEGORY_ORDER,
  SUPPORTED_SCHEMA_VERSION,
  type Manifest,
  type ManifestEntry,
  type Measurement,
  type MeasurementCategory,
} from '../types/measurement';

// Relative so the built `dist/` works from any subdirectory, matching the
// `base: './'` used by Vite.
const MEASUREMENTS_BASE = 'measurements';

/** Loaded measurements are cached by path; files are immutable once written. */
const measurementCache = new Map<string, Measurement>();

export class MeasurementLoadError extends Error {}

function emptyManifest(): Manifest {
  return {
    schema_version: SUPPORTED_SCHEMA_VERSION,
    generated: '',
    categories: { engine: [], hot_reload: [], cold_start: [] },
  };
}

/**
 * Fetches the manifest. A cache-buster query is appended so a measurement
 * written while the page is open appears on reload rather than being served
 * from the browser's HTTP cache.
 */
export async function loadManifest(): Promise<Manifest> {
  const response = await fetch(`${MEASUREMENTS_BASE}/index.json?t=${Date.now()}`);
  if (!response.ok) {
    throw new MeasurementLoadError(
      `Could not read measurements/index.json (HTTP ${response.status}). ` +
        `Run "python devops/pill_lab/pill_lab.py reindex" to create it.`,
    );
  }
  const manifest = (await response.json()) as Manifest;
  const normalized = emptyManifest();
  normalized.schema_version = manifest.schema_version ?? SUPPORTED_SCHEMA_VERSION;
  normalized.generated = manifest.generated ?? '';
  for (const category of CATEGORY_ORDER) {
    const entries = manifest.categories?.[category] ?? [];
    // Newest first regardless of how the manifest was ordered.
    normalized.categories[category] = [...entries].sort((left, right) =>
      right.timestamp.localeCompare(left.timestamp),
    );
  }
  return normalized;
}

/** Fetches and validates one measurement document, memoizing the result. */
export async function loadMeasurement(entry: ManifestEntry): Promise<Measurement> {
  const cached = measurementCache.get(entry.file);
  if (cached) return cached;

  const response = await fetch(`${MEASUREMENTS_BASE}/${entry.file}`);
  if (!response.ok) {
    throw new MeasurementLoadError(
      `Could not load ${entry.file} (HTTP ${response.status}). The manifest may be stale; ` +
        `run "python devops/pill_lab/pill_lab.py reindex".`,
    );
  }
  const measurement = (await response.json()) as Measurement;
  if (measurement.schema_version > SUPPORTED_SCHEMA_VERSION) {
    throw new MeasurementLoadError(
      `${entry.file} uses measurement schema v${measurement.schema_version}, but this ` +
        `frontend understands v${SUPPORTED_SCHEMA_VERSION}. Update the Pill Lab frontend.`,
    );
  }
  measurementCache.set(entry.file, measurement);
  return measurement;
}

/** Returns the first category that has any stored measurement. */
export function firstPopulatedCategory(manifest: Manifest): MeasurementCategory {
  return (
    CATEGORY_ORDER.find((category) => manifest.categories[category].length > 0) ?? 'engine'
  );
}
