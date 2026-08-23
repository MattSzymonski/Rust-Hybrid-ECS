// DESCRIPTION: The run and baseline selectors. Both list the same history
//   (newest first); the baseline list additionally offers "none" and excludes
//   the currently selected run, so a measurement can never be compared with
//   itself.
//
// --- SCRIPT ---

import { element } from '../lib/dom';
import { formatTimestamp } from '../lib/format';
import type { ManifestEntry } from '../types/measurement';

/** Builds the one-line description shown for a run in a dropdown. */
function optionLabel(entry: ManifestEntry): string {
  const time = formatTimestamp(entry.timestamp);
  const commit = entry.git_commit_short ? ` · ${entry.git_commit_short}` : '';
  const dirty = entry.git_dirty ? '*' : '';
  const summary = summaryFragment(entry);
  return `${time}${commit}${dirty}${summary ? ` · ${summary}` : ''}`;
}

/** Condenses the manifest summary into a few characters of context. */
function summaryFragment(entry: ManifestEntry): string {
  const summary = entry.summary ?? {};
  if (entry.category === 'engine' && summary.benchmark_count !== undefined) {
    return `${summary.benchmark_count} benches`;
  }
  if (entry.category === 'hot_reload' && summary.case_count !== undefined) {
    return `${summary.case_count} cases`;
  }
  if (entry.category === 'cold_start' && summary.clean_build_ms) {
    return `build ${(summary.clean_build_ms / 1000).toFixed(0)}s`;
  }
  return '';
}

export interface MeasurementPickerOptions {
  entries: ManifestEntry[];
  selectedFile: string | null;
  baselineFile: string | null;
  onSelect: (file: string) => void;
  onSelectBaseline: (file: string | null) => void;
}

export function renderMeasurementPicker(options: MeasurementPickerOptions): HTMLElement {
  const container = element('div', { class: 'measurement-picker-grid' });

  const runSection = element('div', { class: 'sidebar-section' });
  runSection.appendChild(
    element('div', {
      class: 'sidebar-section-label',
      text: `Measurement (${options.entries.length})`,
    }),
  );
  const runSelect = element('select', { class: 'run-select' }) as HTMLSelectElement;
  if (options.entries.length === 0) {
    runSelect.disabled = true;
    runSelect.appendChild(element('option', { text: 'No measurements yet' }));
  }
  for (const entry of options.entries) {
    const option = element('option', {
      value: entry.file,
      text: optionLabel(entry),
    }) as HTMLOptionElement;
    option.selected = entry.file === options.selectedFile;
    runSelect.appendChild(option);
  }
  runSelect.addEventListener('change', () => options.onSelect(runSelect.value));
  runSection.appendChild(runSelect);
  container.appendChild(runSection);

  const baselineSection = element('div', { class: 'sidebar-section' });
  baselineSection.appendChild(
    element('div', { class: 'sidebar-section-label', text: 'Compare against' }),
  );
  const baselineSelect = element('select', { class: 'run-select' }) as HTMLSelectElement;
  const noneOption = element('option', { value: '', text: 'No baseline' }) as HTMLOptionElement;
  noneOption.selected = !options.baselineFile;
  baselineSelect.appendChild(noneOption);

  // Only runs other than the current one can serve as a baseline.
  const candidates = options.entries.filter((entry) => entry.file !== options.selectedFile);
  for (const entry of candidates) {
    const option = element('option', {
      value: entry.file,
      text: optionLabel(entry),
    }) as HTMLOptionElement;
    option.selected = entry.file === options.baselineFile;
    baselineSelect.appendChild(option);
  }
  baselineSelect.disabled = candidates.length === 0;
  baselineSelect.addEventListener('change', () =>
    options.onSelectBaseline(baselineSelect.value || null),
  );
  baselineSection.appendChild(baselineSelect);
  baselineSection.appendChild(
    element('div', {
      class: 'baseline-hint',
      text:
        candidates.length === 0
          ? 'Run the category again to unlock comparison.'
          : 'Timing metrics are lower-is-better; changes under 2% are reported as noise.',
    }),
  );
  container.appendChild(baselineSection);

  return container;
}
