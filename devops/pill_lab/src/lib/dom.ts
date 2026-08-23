// DESCRIPTION: Minimal DOM construction helpers. The frontend builds elements
//   directly rather than pulling in a framework, so these exist to keep the
//   component code readable: `element` for creation, `clear` for replacement,
//   and `text` for safe interpolation of measurement values.
//
// --- SCRIPT ---

type Attributes = Record<string, string | number | boolean | undefined | null>;
type Child = Node | string | null | undefined | false;

/**
 * Creates an element with attributes and children.
 *
 * `class` and `text` are handled specially; everything else becomes a plain
 * attribute. Children are appended as text nodes when given as strings, so
 * measurement values can never be interpreted as markup.
 */
export function element<Tag extends keyof HTMLElementTagNameMap>(
  tag: Tag,
  attributes: Attributes = {},
  ...children: Child[]
): HTMLElementTagNameMap[Tag] {
  const created = document.createElement(tag);
  for (const [name, value] of Object.entries(attributes)) {
    if (value === undefined || value === null || value === false) continue;
    if (name === 'class') {
      created.className = String(value);
    } else if (name === 'text') {
      created.textContent = String(value);
    } else if (name === 'html') {
      created.innerHTML = String(value);
    } else {
      created.setAttribute(name, String(value));
    }
  }
  appendChildren(created, children);
  return created;
}

/** Appends a list of children, skipping the falsy entries conditionals produce. */
export function appendChildren(parent: HTMLElement, children: Child[]): void {
  for (const child of children) {
    if (child === null || child === undefined || child === false) continue;
    parent.appendChild(typeof child === 'string' ? document.createTextNode(child) : child);
  }
}

/** Removes every child of a container before it is re-rendered. */
export function clear(container: HTMLElement): void {
  while (container.firstChild) container.removeChild(container.firstChild);
}

/** Builds a `<tr>` from cell definitions, for the many small stat tables. */
export function tableRow(
  cells: (Child | { text: Child; class?: string; colSpan?: number })[],
  rowClass?: string,
): HTMLTableRowElement {
  const row = element('tr', rowClass ? { class: rowClass } : {});
  for (const cell of cells) {
    if (cell && typeof cell === 'object' && 'text' in cell) {
      const created = element('td', {
        class: cell.class,
        colspan: cell.colSpan,
      });
      appendChildren(created, [cell.text]);
      row.appendChild(created);
    } else {
      const created = element('td');
      appendChildren(created, [cell as Child]);
      row.appendChild(created);
    }
  }
  return row;
}

/** Builds a `<thead>` row of column labels. */
export function tableHead(labels: (string | { text: string; class?: string })[]): HTMLTableSectionElement {
  const head = element('thead');
  const row = element('tr');
  for (const label of labels) {
    if (typeof label === 'string') {
      row.appendChild(element('th', { text: label }));
    } else {
      row.appendChild(element('th', { class: label.class, text: label.text }));
    }
  }
  head.appendChild(row);
  return head;
}

/**
 * Wires an element to scroll another element into view on click.
 *
 * Used instead of `href="#id"` anchors: the location hash carries the app's
 * category/run/baseline selection, and a fragment link would overwrite it, so
 * reloading the page after clicking a benchmark would lose the selection.
 */
export function scrollToOnClick(trigger: HTMLElement, targetId: string): void {
  trigger.addEventListener('click', (event) => {
    event.preventDefault();
    document.getElementById(targetId)?.scrollIntoView({ behavior: 'smooth', block: 'start' });
  });
}
