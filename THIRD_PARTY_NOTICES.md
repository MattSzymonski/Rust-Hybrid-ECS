# Third-party notices

This file collects the licences of third-party assets this repository
redistributes. Each section names the files, their licence and where the copy
came from.

## Geist (variable)

- Files: `modules/pill_ui/assets/Geist-Variable.ttf` and
  `modules/pill_ui/assets/Geist-LICENSE.txt` (the full SIL Open Font License
  1.1 text).
- Licence: SIL Open Font License 1.1.
- Source: the `google/fonts` repository,
  `https://raw.githubusercontent.com/google/fonts/main/ofl/geist/Geist%5Bwght%5D.ttf`
  (fetched 2026-10-09; SHA-256
  `73894e0448cae90a92b6c2f8732b7bb9acb7b94c418bff559dad4a18e1de9659`).
- Used by: `pill_ui`'s bundled default font - the guaranteed fallback when a
  machine offers no system fonts - and the editor's UI font (W-0004 Stage 10,
  chosen by the person).

## Phosphor Icons (regular weight, curated subset)

- Files: `modules/pill_ui/assets/icons/*.svg` (the editor's curated subset)
  and `modules/pill_ui/assets/icons/LICENSE.txt` (the MIT text).
- Licence: MIT.
- Source: the `phosphor-icons/core` repository,
  `https://raw.githubusercontent.com/phosphor-icons/core/main/assets/regular/<name>.svg`
  (fetched 2026-10-09).
- Used by: `pill_ui`'s bundled editor icon set (`icons::EDITOR_ICONS`); the
  sources are white so a draw's vertex colour tints them.
