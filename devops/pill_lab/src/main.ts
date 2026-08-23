// DESCRIPTION: Pill Lab application shell.
//
//   Holds the whole app state in one small object - selected category, run and
//   baseline - and re-renders the sidebar and main column whenever it changes.
//   There is no state library and no router: three fields and a render function
//   are enough for a dashboard this size, and the selection is mirrored into
//   the URL hash so a particular view can be linked or reloaded in place.
//
// --- SCRIPT ---

import './styles/tokens.css';
import './styles/base.css';
import './styles/report.css';
import './styles/tests.css';

import { renderCategoryPicker } from './components/CategoryPicker';
import { renderColdStartReport } from './components/ColdStartReport';
import { renderEngineReport } from './components/EngineReport';
import { renderEnvironments } from './components/EnvironmentInfo';
import { renderHotReloadReport } from './components/HotReloadReport';
import { renderMeasurementHeader } from './components/MeasurementHeader';
import { renderMeasurementPicker } from './components/MeasurementPicker';
import { renderBenchmarksRuns } from './components/BenchmarkRunner';
import type { ReportView } from './components/ReportView';
import { renderTestsApp } from './components/TestsView';
import { firstPopulatedCategory, loadManifest, loadMeasurement } from './data/loader';
import { clear, element } from './lib/dom';
import {
  CATEGORY_LABELS,
  CATEGORY_ORDER,
  type Manifest,
  type ManifestEntry,
  type Measurement,
  type MeasurementCategory,
} from './types/measurement';

type AppTab = 'benchmarks' | 'tests';

interface ApplicationState {
  manifest: Manifest;
  category: MeasurementCategory;
  selectedFile: string | null;
  baselineFile: string | null;
  tab: AppTab;
}

const sidebarElement = document.getElementById('sidebar') as HTMLElement;
const mainElement = document.getElementById('main') as HTMLElement;

let state: ApplicationState | null = null;
/** The mounted report, kept so its chart instances can be released. */
let activeReport: ReportView | null = null;

// =============================================================================
// URL state
// =============================================================================

/** Reads `#category|run|baseline` (benchmarks) or `#tests` from the hash. */
function readHash(): Partial<
  Pick<ApplicationState, 'category' | 'selectedFile' | 'baselineFile' | 'tab'>
> {
  // `|` separates the three fields. Some clients percent-encode it when a URL
  // is copied around, so it is normalized before splitting; the fields
  // themselves stay encoded until after the split.
  const raw = window.location.hash.replace(/^#/, '').replace(/%7c/gi, '|');
  if (raw === 'tests') return { tab: 'tests' };
  if (!raw) return {};
  const [category, selected, baseline] = raw.split('|').map(decodeURIComponent);
  const result: ReturnType<typeof readHash> = {};
  if (category && (CATEGORY_ORDER as string[]).includes(category)) {
    result.category = category as MeasurementCategory;
  }
  if (selected) result.selectedFile = selected;
  if (baseline) result.baselineFile = baseline;
  return result;
}

/** Writes the current selection back into the hash without adding history. */
function writeHash(current: ApplicationState): void {
  const next = current.tab === 'tests' ? '#tests' : [
    current.category,
    current.selectedFile ?? '',
    current.baselineFile ?? '',
  ].map(encodeURIComponent).join('|').replace(/^/, '#');
  if (window.location.hash !== next) {
    window.history.replaceState(null, '', next);
  }
}

// =============================================================================
// Tab switcher (rendered in the sidebar, under the brand)
// =============================================================================

/**
 * Builds the Benchmarks / Tests switcher shown directly under the sidebar
 * brand. Returns a fresh element so each render can re-insert it after the
 * brand; the sidebar is rebuilt on every state change.
 */
function tabSwitcher(): HTMLElement {
  const bar = element('div', { class: 'tab-switcher' });
  for (const tab of (['benchmarks', 'tests'] as AppTab[])) {
    const button = element(
      'button',
      {
        class: `tab-button ${state?.tab === tab ? 'active' : ''}`,
        type: 'button',
        text: tab === 'benchmarks' ? 'Benchmarks' : 'Tests',
      },
    );
    button.addEventListener('click', () => selectTab(tab));
    bar.appendChild(button);
  }
  return bar;
}

function selectTab(tab: AppTab): void {
  if (!state || state.tab === tab) return;
  state.tab = tab;
  void render();
}

// =============================================================================
// Rendering
// =============================================================================

function sidebarBrand(): HTMLElement {
  return element(
    'div',
    { class: 'sidebar-brand' },
    element('img', {
      class: 'sidebar-logo',
      alt: 'Pill Engine',
      src: 'https://raw.githubusercontent.com/MattSzymonski/Pill-Engine/main/media/logo/pill_logo_white.png',
    }),
    element('span', { class: 'sidebar-lab', text: 'LAB' }),
  );
}

/** Renders the empty state shown when a category has no measurements yet. */
function emptyState(category: MeasurementCategory): HTMLElement {
  const commands: Record<MeasurementCategory, string> = {
    engine: 'python devops/pill_lab/pill_lab.py engine',
    hot_reload: 'python devops/pill_lab/pill_lab.py hot-reload',
    cold_start: 'python devops/pill_lab/pill_lab.py cold-start',
  };
  return element(
    'div',
    { class: 'empty-state' },
    element('h2', { text: `No ${CATEGORY_LABELS[category]} measurements yet` }),
    element('p', {
      text: 'Run the measurement once and reload this page - the manifest is regenerated automatically.',
    }),
    element('pre', { text: commands[category] }),
  );
}

function errorState(message: string): HTMLElement {
  return element(
    'div',
    { class: 'empty-state error-state' },
    element('h2', { text: 'Could not load measurement data' }),
    element('p', { text: message }),
  );
}

/** Dispatches to the report renderer for the measurement's category. */
function renderReport(
  measurement: Measurement,
  baseline: Measurement | null,
): ReportView {
  switch (measurement.category) {
    case 'engine':
      return renderEngineReport(
        measurement,
        baseline?.category === 'engine' ? baseline : null,
      );
    case 'hot_reload':
      return renderHotReloadReport(
        measurement,
        baseline?.category === 'hot_reload' ? baseline : null,
      );
    case 'cold_start':
      return renderColdStartReport(
        measurement,
        baseline?.category === 'cold_start' ? baseline : null,
      );
  }
}

async function render(): Promise<void> {
  if (!state) return;

  // ---- View dispatch ----
  if (state.tab === 'tests') {
    // Tests reuse the same two-column shell as Benchmarks: the sidebar lists
    // the suites, the main column shows the selected suite's detail + live
    // terminal.
    activeReport?.onUnmount?.();
    activeReport = null;
    writeHash(state);
    clear(sidebarElement);
    clear(mainElement);
    sidebarElement.appendChild(sidebarBrand());
    sidebarElement.appendChild(tabSwitcher());
    const testsSidebar = element('div', { class: 'tests-sidebar sidebar-scroll' });
    sidebarElement.appendChild(testsSidebar);
    const testsMain = element('div', { class: 'tests-main' });
    mainElement.appendChild(testsMain);
    await renderTestsApp(testsSidebar, testsMain);
    return;
  }

  const current = state;
  const entries = current.manifest.categories[current.category];

  activeReport?.onUnmount?.();
  activeReport = null;

  // ---- Sidebar ----
  clear(sidebarElement);
  sidebarElement.appendChild(sidebarBrand());
  sidebarElement.appendChild(tabSwitcher());
  sidebarElement.appendChild(
    renderCategoryPicker(current.manifest, current.category, (category) => {
      selectCategory(category);
    }),
  );
  // Only the currently selected category's benchmark appears here (the run
  // button and terminal live in the main panel, mirroring the Tests tab).
  const benchmarkSidebar = element('div');
  sidebarElement.appendChild(benchmarkSidebar);
  const sidebarScroll = element('div', { class: 'sidebar-scroll' });
  sidebarElement.appendChild(sidebarScroll);

  // ---- Main ----
  clear(mainElement);
  writeHash(current);

  // Column order, top to bottom: the measurement title, the measurement /
  // compare pickers, one environment panel below each picker (run and
  // baseline, side by side, with a link indicator between them), the run
  // panel with its terminal, then the report. The title and environments
  // need the loaded measurement, so they render into placeholders once the
  // async load completes; the pickers and run panel render immediately.
  const titleHost = element('div');
  mainElement.appendChild(titleHost);

  const pickers = element('div', { class: 'benchmark-pickers' });
  pickers.appendChild(
    renderMeasurementPicker({
      entries,
      selectedFile: current.selectedFile,
      baselineFile: current.baselineFile,
      onSelect: (file) => {
        current.selectedFile = file;
        // A baseline pointing at the newly selected run is no longer valid.
        if (current.baselineFile === file) current.baselineFile = null;
        void render();
      },
      onSelectBaseline: (file) => {
        current.baselineFile = file;
        void render();
      },
    }),
  );
  mainElement.appendChild(pickers);

  const environmentsHost = element('div');
  mainElement.appendChild(environmentsHost);

  const benchmarkPanel = element('div');
  mainElement.appendChild(benchmarkPanel);

  // A freshly finished benchmark writes a new measurement, so reload the
  // manifest and re-render for the new run to appear immediately.
  void renderBenchmarksRuns(benchmarkSidebar, benchmarkPanel, current.category, () => {
    void refreshManifest();
  });

  if (entries.length === 0 || !current.selectedFile) {
    mainElement.appendChild(emptyState(current.category));
    return;
  }

  const entry = entries.find((candidate) => candidate.file === current.selectedFile);
  if (!entry) {
    mainElement.appendChild(errorState(`Measurement ${current.selectedFile} is not in the manifest.`));
    return;
  }

  let measurement: Measurement;
  let baseline: Measurement | null = null;
  try {
    measurement = await loadMeasurement(entry);
    const baselineEntry = current.baselineFile
      ? entries.find((candidate) => candidate.file === current.baselineFile)
      : undefined;
    if (baselineEntry) baseline = await loadMeasurement(baselineEntry);
  } catch (error) {
    mainElement.appendChild(errorState((error as Error).message));
    return;
  }

  // A stale render can finish after the user has already changed selection.
  if (state !== current || current.selectedFile !== entry.file) return;

  titleHost.appendChild(renderMeasurementHeader(measurement));
  environmentsHost.appendChild(renderEnvironments(measurement, baseline));

  const report = renderReport(measurement, baseline);
  mainElement.appendChild(report.content);
  if (report.sidebar) sidebarScroll.appendChild(report.sidebar);
  // Mount after insertion: charts need real layout to size themselves.
  report.onMount?.();
  activeReport = report;
}

function selectCategory(category: MeasurementCategory): void {
  if (!state) return;
  state.category = category;
  const entries = state.manifest.categories[category];
  state.selectedFile = entries[0]?.file ?? null;
  state.baselineFile = entries[1]?.file ?? null;
  void render();
}

/** Reloads the measurement manifest after a benchmark run wrote a new one. */
async function refreshManifest(): Promise<void> {
  if (!state) return;
  try {
    state.manifest = await loadManifest();
  } catch {
    // Keep the current manifest when the refresh fails; the run still wrote
    // its measurement, and a page reload will pick it up.
  }
  await render();
}

/** Picks the initial run/baseline for a category: newest, and the one before. */
function defaultSelection(
  entries: ManifestEntry[],
): { selectedFile: string | null; baselineFile: string | null } {
  return {
    selectedFile: entries[0]?.file ?? null,
    baselineFile: entries[1]?.file ?? null,
  };
}

async function start(): Promise<void> {
  let manifest: Manifest;
  try {
    manifest = await loadManifest();
  } catch (error) {
    clear(mainElement);
    clear(sidebarElement);
    sidebarElement.appendChild(sidebarBrand());
    mainElement.appendChild(errorState((error as Error).message));
    return;
  }

  const fromHash = readHash();
  const tab = fromHash.tab ?? 'benchmarks';
  const category = fromHash.category ?? firstPopulatedCategory(manifest);
  const entries = manifest.categories[category];
  const defaults = defaultSelection(entries);

  // A hash selection is only honoured when the file still exists.
  const selectedFile =
    fromHash.selectedFile && entries.some((entry) => entry.file === fromHash.selectedFile)
      ? fromHash.selectedFile
      : defaults.selectedFile;
  const baselineFile =
    fromHash.baselineFile &&
    fromHash.baselineFile !== selectedFile &&
    entries.some((entry) => entry.file === fromHash.baselineFile)
      ? fromHash.baselineFile
      : defaults.baselineFile === selectedFile
        ? null
        : defaults.baselineFile;

  state = { manifest, category, selectedFile, baselineFile, tab };
  await render();
}

void start();

// Support pasting a hash URL or editing it in the address bar: the tab
// buttons navigate through selectTab, which uses replaceState (no history
// entry, no hashchange), so this listener only fires for genuine URL changes.
window.addEventListener('hashchange', () => {
  if (!state) return;
  const fromHash = readHash();
  if (fromHash.tab) state.tab = fromHash.tab;
  if (fromHash.tab !== 'tests') {
    if (fromHash.category) state.category = fromHash.category;
    if (fromHash.selectedFile) state.selectedFile = fromHash.selectedFile;
    if (fromHash.baselineFile !== undefined) state.baselineFile = fromHash.baselineFile;
  }
  void render();
});
