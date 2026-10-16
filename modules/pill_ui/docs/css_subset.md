# The CSS subset

`pill_ui` styles its widgets with stylesheets written in a deliberately
small CSS subset. The parser lives in `modules/pill_ui/src/css/`; this
document is the contract: what parses, what it does, and what happens to
everything else.

## Loading

```rust
let mut sheet = Stylesheet::parse(include_str!("../assets/default.css"));
// or a project file, reloaded as its bytes change:
let mut sheet = Stylesheet::parse(&std::fs::read_to_string(path)?);
```

`Stylesheet::parse` never fails: everything the subset refuses becomes a
`Report { kind, subject, detail, line, column }` in `sheet.reports()`,
naming the property or selector and where it sits. A frame resolves styles
through `layout::compute`/`run_frame`; `sheet.clear_cache()` after a reload
(a fresh parse has an empty cache anyway).

The two sheets shipped today:

- `assets/default.css` - the built-in theme in the subset (plan decision
  D7): the approved Stage 10 design (W-0004 v4), the same values as
  `Theme::default()`, exactly. A UI with no sheet falls back to the code's
  values; a project sheet loads after this one and overrides it.
- `assets/editor_dock.css` - the native port of the old web editor's
  `dock_layout.css` (`modules/frontents/pill_editor/assets`). It parses with
  zero reports.

## Selectors

| Syntax                                          | Meaning                                                                                                                                                                                                                              |
| ----------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------ |
| `type`                                          | The node's type: `row`, `column`, `label`, `heading`, `separator`, `image`, `button`, `checkbox`, `slider`, `drag-value`, `text-field`, `combo-box`, `color-field`, `vector-field`, `collapsing`, `scroll-area`, `table`, `tree-row` |
| `.class`                                        | A class attached with `ui.class("...")` (applies to the next widget or group)                                                                                                                                                        |
| `#name`                                         | The name the widget was built with (`ui.button("Bake")` is `#Bake`; containers are named by their group)                                                                                                                             |
| `a b`                                           | Descendant: `b` anywhere inside `a`                                                                                                                                                                                                  |
| `a > b`                                         | Child: `b` a direct child of `a`                                                                                                                                                                                                     |
| `a, b`                                          | Either selector                                                                                                                                                                                                                      |
| `*`                                             | Anything                                                                                                                                                                                                                             |
| `:hover` `:active` `:focus-visible` `:disabled` | The widget's state, from the frame's input and the widget's `enabled` flag                                                                                                                                                           |
| `:root`                                         | The variable scope - `:root { --x: ...; }`; matches no widget                                                                                                                                                                        |

Specificity is (ids, classes-and-pseudos, types); `!important` beats it.
Later rules win ties. Not in the subset: attribute selectors (`[...]`),
functional pseudo-classes (`:not(...)`), pseudo-elements, `@media` and
every other at-rule - all reported by name.

## Values

Lengths: `12px`, `0`, bare numbers (pixels), `50%` where meaningful, `auto`.
Sizes also take `max-content` / `min-content`. Colours: `#rgb`, `#rrggbb`,
`#rrggbbaa`, `rgb(...)` / `rgba(...)` (numbers or percentages), a small
named set, `transparent`. `var(--name[, fallback])` resolves at load time
from `:root` (in any source order, depth-limited); an unresolvable var is
reported. `inherit` copies the parent's computed value for that property;
`unset` does the same for the inheritable set and nothing elsewhere. No
`calc()`, no `em`/`rem`, no gradients.

## Properties

Layout (mapped onto `taffy`):

- `display` (`flex`, `block`, `none`), `flex-direction`, `flex-grow`,
  `flex-shrink`, `flex-basis`, `flex` (shorthand), `align-items`,
  `justify-content`, `gap`, `row-gap`, `column-gap`.
- `width`, `height`, `min-width`, `min-height`, `max-width`, `max-height`.
- `padding` (+ side forms), `margin` (+ side forms, `auto` included),
  `box-sizing`.
- `border` / `border-{top,right,bottom,left}` shorthands and their
  `-width` / `-style` / `-color` forms; only `solid` and `none` styles.
- `position` (`relative`, `absolute`; `fixed` behaves as absolute inside the
  surface), `inset` / `top` / `right` / `bottom` / `left`.
- `overflow`, `overflow-x`, `overflow-y`: any non-`visible` value clips the
  node's children to its rectangle; `auto`/`scroll` additionally mark the
  node scrollable (wheel scrolling itself is the scroll-area widget's,
  which the shell's panels use).

Paint (honoured today):

- `background` / `background-color` - container fills, button fills, image
  washes.
- `color` - label, heading and button text; inherited.
- `font-size`, `font-family`, `line-height`, `font` shorthand - text
  metrics, measured and painted identically; inherited. `font-weight` is
  parsed and stored; the bundled font has one instance until Stage 10.
- `text-align` (`left`, `center`, `right`), `white-space` (`normal`,
  `nowrap`), `text-overflow` (`clip`, `ellipsis`; ellipsis cuts a `nowrap`
  line to the longest prefix that fits with `…` appended).
- `border*` - per-side strokes; `outline` + `outline-offset` - the
  `:focus-visible` ring; `opacity` - multiplied down the subtree.
- `border-radius` - rounded fills; the shader carries the radius, controls
  round by the theme's token by default; per-corner radii are not in the
  subset.
- `box-shadow` - a non-inset shadow paints behind the node (the blur's half
  extent approximates the gaussian spread); `inset` shadows are kept and
  not drawn.
- `pointer-events` - `none` removes the node (and, being inherited, its
  subtree) from hit-testing, same as `display: none` removing it entirely.
- `scrollbar-width` - a scroll area's bar strip thickness in pixels.
- `scrollbar-thumb` / `scrollbar-track` - the colours of a scroll area's
  bar; the theme's tokens apply where a rule does not set them. Bars show
  only while an axis overflows.

Parsed and stored, visible once their layer lands:

- `z-index` - needs the overlay layer (Stages 13+); everything draws in
  tree order.
- `cursor` - needs the platform cursor; `user-select` - text fields are the
  only selectable text.

Not in the subset (reported by name): `float`, `clear`, `text-shadow`,
`transition`, `animation`, `transform`, `filter`, `background-image`,
`background-size`, `background-position`, `touch-action`, `content`,
`gap`-adjacent logical properties, `@`-anything.

## The dock port's differences

`assets/editor_dock.css` carries a header enumerating them; in short:
`html, body` and `position: fixed` are gone (the shell owns the window),
`[aria-selected="true"]` became `.selected`, the checkerboard gradients are
dropped (a viewport concern), and `touch-action` / `background-image` /
`background-size` / `background-position` are dropped as out of subset.

## Reloading

A stylesheet is plain bytes: the shell (Stage 13) loads it through the
engine's asset flow as a `Stylesheet` imported asset (`.css`), and the dev
probe re-reads the file, re-parses and re-renders on change
(`PILL_UI_PROBE_STYLE`). Nothing recompiles; the cache is per sheet, so a
reloaded sheet recomputes from scratch.
