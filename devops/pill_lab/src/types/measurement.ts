// DESCRIPTION: The measurement JSON contract between `pill_lab.py` and this
//   frontend. Every field here is written by the Python runners; nothing is
//   computed at load time. Category payloads are separate interfaces joined by
//   a discriminated union on `category`, so a report component receives a
//   fully typed payload instead of a bag of optional fields.
//
// --- SCRIPT ---

/**
 * Highest measurement schema version this frontend understands. Older files
 * still load - every version bump so far has been additive - but a newer one
 * is refused rather than rendered with fields it does not know about.
 */
export const SUPPORTED_SCHEMA_VERSION = 2;

export type MeasurementCategory = 'engine' | 'hot_reload' | 'cold_start';

export const CATEGORY_ORDER: MeasurementCategory[] = ['engine', 'hot_reload', 'cold_start'];

export const CATEGORY_LABELS: Record<MeasurementCategory, string> = {
  engine: 'Engine Performance',
  hot_reload: 'Hot Reloading',
  cold_start: 'Cold Start',
};

// =============================================================================
// Common envelope
// =============================================================================

export interface GitMetadata {
  available: boolean;
  commit?: string;
  commit_short?: string;
  branch?: string;
  dirty?: boolean;
  dirty_file_count?: number;
  subject?: string;
  commit_date?: string;
}

export interface EnvironmentMetadata {
  os?: string;
  os_version?: string;
  architecture?: string;
  hostname?: string;
  python?: string;
  cpu?: string;
  logical_cpus?: number;
  physical_cores?: number;
  cpu_max_mhz?: number;
  l2_cache_kb?: number;
  l3_cache_kb?: number;
  l2_cache?: string;
  l3_cache?: string;
  ram_gb?: number;
  rustc?: string;
  rustc_commit?: string;
  cargo?: string;
  active_toolchain?: string;
  host_triple?: string;
  dotnet?: string;
}

export interface CommandMetadata {
  argv?: string[];
  cwd?: string;
  duration_seconds?: number;
  exit_code?: number;
  skipped?: boolean;
  profile_overrides?: boolean;
  rustflags?: string;
  driver?: string;
}

interface MeasurementEnvelope<Category extends MeasurementCategory, Payload> {
  schema_version: number;
  category: Category;
  timestamp: string;
  label: string;
  tool: { name: string; version: string };
  git: GitMetadata;
  environment: EnvironmentMetadata;
  command: CommandMetadata;
  notes: string[];
  measurement: Payload;
}

// =============================================================================
// Engine Performance
// =============================================================================

export interface EstimateBlock {
  point: number;
  lower: number;
  upper: number;
}

export interface BenchmarkChange {
  percent: number;
  lower: number;
  upper: number;
  direction: ChangeDirection;
}

export type ChangeDirection = 'improved' | 'regressed' | 'unchanged';

export interface BenchmarkEntry {
  id: string;
  group: string;
  group_prefix: string;
  parameter: string;
  entity_count: number | null;
  mean_ns: number;
  median_ns: number;
  std_dev_ns: number;
  min_ns: number | null;
  max_ns: number | null;
  iteration_count: number;
  outlier_count: number;
  throughput: number | null;
  throughput_unit: string;
  run_timestamp: string;
  /**
   * Exact mtime of the benchmark's Criterion estimates file (schema v2+).
   * Two measurements sharing this value read the same Criterion output, so the
   * benchmark was not re-run between them; the CLI's `compare` uses it to
   * exclude carried-over results.
   */
  run_epoch?: number | null;
  estimates: Partial<Record<'mean' | 'median' | 'std_dev' | 'slope', EstimateBlock>>;
  /** Per-sample times in microseconds, thinned to at most 400 points. */
  samples_us: number[];
  outlier_flags: boolean[];
  /** The previous Criterion run's samples, when one was saved. */
  base_samples_us: number[];
  /** Criterion's own change versus its immediately preceding run. */
  change: BenchmarkChange | null;
}

export interface EngineBenchmarkGroup {
  prefix: string;
  label: string;
  benchmark_ids: string[];
}

export interface EngineMeasurementPayload {
  criterion_directory: string;
  bench_targets: string[];
  quick: boolean;
  profile: string;
  benchmark_count: number;
  groups: EngineBenchmarkGroup[];
  benchmarks: BenchmarkEntry[];
}

export type EngineMeasurement = MeasurementEnvelope<'engine', EngineMeasurementPayload>;

// =============================================================================
// Hot Reloading
// =============================================================================

export interface HotReloadIteration {
  index: number;
  wall_ms: number;
  build_ms?: number;
  stage_ms?: number;
  load_ms?: number;
  init_ms?: number;
  migrate_ms?: number;
  rebuilt_crates?: string[];
}

export interface HotReloadSummary {
  iterations: number;
  min_ms: number;
  avg_ms: number;
  median_ms: number;
  max_ms: number;
  phase_averages?: Partial<Record<'build_ms' | 'stage_ms' | 'load_ms' | 'init_ms' | 'migrate_ms', number>>;
}

export interface HotReloadCase {
  name: string;
  session: string;
  description: string;
  iterations: HotReloadIteration[];
  summary: HotReloadSummary;
}

export interface HotReloadStartup {
  wall_ms: number;
  host_elapsed_ms?: number;
  builds?: number;
  up_to_date_skips?: number;
}

export interface HotReloadSession {
  name: string;
  title: string;
  startup: HotReloadStartup | null;
}

export interface HotReloadMeasurementPayload {
  harness: string;
  iterations: number;
  warmup: boolean;
  wall_time_definition: string;
  sessions: HotReloadSession[];
  cases: HotReloadCase[];
}

export type HotReloadMeasurement = MeasurementEnvelope<'hot_reload', HotReloadMeasurementPayload>;

// =============================================================================
// Cold Start
// =============================================================================

export interface CargoTimingUnit {
  name: string;
  version: string;
  mode: string;
  target: string;
  duration_seconds: number;
  start_seconds: number;
}

export interface CargoTimings {
  report_file: string;
  total_seconds: number;
  unit_count: number;
  unit_seconds_total: number;
  parallelism: number | null;
  units: CargoTimingUnit[];
  units_truncated: number;
}

export interface ColdStartCase {
  name: string;
  kind: 'build' | 'startup';
  description: string;
  command: string[];
  duration_ms: number;
  package?: string;
  exit_code?: number;
  cargo_timings?: CargoTimings;
  host_elapsed_ms?: number;
  builds?: number;
  up_to_date_skips?: number;
  repetitions?: number;
  min_ms?: number;
  avg_ms?: number;
  max_ms?: number;
  samples_ms?: number[];
}

export interface ColdStartCleanRecord {
  performed: boolean;
  argv?: string[];
  duration_ms?: number;
  packages?: string[];
  reason?: string;
}

export interface ColdStartMeasurementPayload {
  package: string;
  clean_scope: 'packages' | 'workspace' | 'none';
  cleans: ColdStartCleanRecord[];
  cleaned_packages: string[];
  incremental_trigger: string;
  cases: ColdStartCase[];
}

export type ColdStartMeasurement = MeasurementEnvelope<'cold_start', ColdStartMeasurementPayload>;

// =============================================================================
// Union + manifest
// =============================================================================

export type Measurement = EngineMeasurement | HotReloadMeasurement | ColdStartMeasurement;

export interface ManifestSummary {
  benchmark_count?: number;
  regressed_count?: number;
  improved_count?: number;
  case_count?: number;
  slowest_avg_ms?: number | null;
  clean_build_ms?: number | null;
}

export interface ManifestEntry {
  file: string;
  category: MeasurementCategory;
  timestamp: string;
  label: string;
  schema_version: number;
  git_commit_short: string;
  git_branch: string;
  git_dirty: boolean;
  size_bytes: number;
  summary: ManifestSummary;
}

export interface Manifest {
  schema_version: number;
  generated: string;
  categories: Record<MeasurementCategory, ManifestEntry[]>;
}
