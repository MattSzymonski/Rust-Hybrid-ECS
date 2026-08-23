// DESCRIPTION: The top-level category selector in the sidebar. Each option
//   shows how many measurements exist for that category, so an empty category
//   is obvious before it is clicked.
//
// --- SCRIPT ---

import { element } from '../lib/dom';
import {
  CATEGORY_LABELS,
  CATEGORY_ORDER,
  type Manifest,
  type MeasurementCategory,
} from '../types/measurement';

export function renderCategoryPicker(
  manifest: Manifest,
  selected: MeasurementCategory,
  onSelect: (category: MeasurementCategory) => void,
): HTMLElement {
  const section = element('div', { class: 'sidebar-section' });
  section.appendChild(element('div', { class: 'sidebar-section-label', text: 'Category' }));

  const picker = element('div', { class: 'category-picker' });
  for (const category of CATEGORY_ORDER) {
    const count = manifest.categories[category].length;
    const classes = ['category-option'];
    if (category === selected) classes.push('active');
    if (count === 0) classes.push('empty');

    const button = element(
      'button',
      { class: classes.join(' '), type: 'button' },
      element('span', { text: CATEGORY_LABELS[category] }),
      element('span', { class: 'count', text: String(count) }),
    );
    button.addEventListener('click', () => onSelect(category));
    picker.appendChild(button);
  }

  section.appendChild(picker);
  return section;
}
