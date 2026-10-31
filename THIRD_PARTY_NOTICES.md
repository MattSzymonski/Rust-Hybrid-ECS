# Third-party notices

This file collects the licences of third-party assets this repository
redistributes. Each section names the files, their licence and where the copy
came from.

## Phosphor Icons (web fonts, regular and duotone weights)

- Files: `modules/frontends/editor/pill_editor_core/assets/Phosphor.ttf`,
  `modules/frontends/editor/pill_editor_core/assets/Phosphor-Duotone.ttf` and
  `modules/frontends/editor/pill_editor_core/assets/Phosphor-LICENSE.txt` (the MIT text).
- Licence: MIT (Copyright (c) 2023 Phosphor Icons).
- Source: the `@phosphor-icons/web` package, version 2.1.1, from
  `https://unpkg.com/@phosphor-icons/web@2.1.1/src/{regular/Phosphor.ttf,
  duotone/Phosphor-Duotone.ttf}` (fetched 2026-10-17; SHA-256
  `06b91e022b7ee899a63efced879392a74f0bacbda54e4467e9f663220d173a10` and
  `80e6f0d6fa8ce96a9933f48aaabddc592c5d2b52caf8f010a7e54a17c74084bf`).
- Used by: `pill_editor_core`'s icon module - the egui editor's tab titles,
  toolbars, menus and the floating toolbox draw their icons and their
  duotone two-layer form from these fonts (`icons::Icon`).

## Inter (variable font, optical size and weight axes)

- Files: `modules/frontends/editor/pill_editor_core/assets/Inter-Variable.ttf` and
  `modules/frontends/editor/pill_editor_core/assets/Inter-OFL.txt` (the SIL Open Font
  License 1.1 text).
- Licence: SIL Open Font License 1.1 (Copyright (c) 2016 The Inter Project
  Authors).
- Source: Google Fonts' `ofl/inter` directory, from
  `https://raw.githubusercontent.com/google/fonts/main/ofl/inter/Inter%5Bopsz,wght%5D.ttf`
  (fetched 2026-10-26; SHA-256
  `29160a80ff49ddcab2c97711247e08b1fab27a484a329ce8b813d820dc559031`).
- Used by: `pill_editor_core`'s style module - the editor's proportional
  family (titles, labels, bodies) is Inter, installed as a built-in family
  ahead of egui's own fonts.

## JetBrains Mono (variable font, weight axis)

- Files: `modules/frontends/editor/pill_editor_core/assets/JetBrainsMono-Variable.ttf`
  and `modules/frontends/editor/pill_editor_core/assets/JetBrainsMono-OFL.txt` (the SIL
  Open Font License 1.1 text).
- Licence: SIL Open Font License 1.1 (Copyright (c) 2020 The JetBrains Mono
  Project Authors).
- Source: Google Fonts' `ofl/jetbrainsmono` directory, from
  `https://raw.githubusercontent.com/google/fonts/main/ofl/jetbrainsmono/JetBrainsMono%5Bwght%5D.ttf`
  (fetched 2026-10-26; SHA-256
  `48715a42ec242c21e9f02692891e147d022299a52e48d5e413e1a942193ffeda`).
- Used by: `pill_editor_core`'s style module - the editor's monospace family
  (console, statistics, numeric fields) is JetBrains Mono, installed as a
  built-in family ahead of egui's own fonts.

## Silkscreen

- Files: `examples/circus_demo/res/fonts/Silkscreen-Regular.ttf` and
  `examples/circus_demo/res/fonts/Silkscreen-OFL.txt` (the SIL Open Font
  License 1.1 text).
- Licence: SIL Open Font License 1.1 (Copyright (c) 2001 The Silkscreen
  Project Authors, with Reserved Font Name "Silkscreen").
- Source: Google Fonts' `ofl/silkscreen` directory, from
  `https://raw.githubusercontent.com/google/fonts/main/ofl/silkscreen/Silkscreen-Regular.ttf`
  (fetched 2026-10-26; SHA-256
  `c845473330b94c2079ce9af01c51ac8ba2d99c24f4d14c039843bbb8e642ebd8`).
- Used by: `circus_demo`'s theme - its code default names this file as the
  game UI's body font, which is what proves a project can ship and load a
  face of its own (`pill_ui_theme::FontAsset`).
