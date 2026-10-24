# Third-party notices

This file collects the licences of third-party assets this repository
redistributes. Each section names the files, their licence and where the copy
came from.

## Phosphor Icons (web fonts, regular and duotone weights)

- Files: `modules/editor/pill_editor_core/assets/Phosphor.ttf`,
  `modules/editor/pill_editor_core/assets/Phosphor-Duotone.ttf` and
  `modules/editor/pill_editor_core/assets/Phosphor-LICENSE.txt` (the MIT text).
- Licence: MIT (Copyright (c) 2023 Phosphor Icons).
- Source: the `@phosphor-icons/web` package, version 2.1.1, from
  `https://unpkg.com/@phosphor-icons/web@2.1.1/src/{regular/Phosphor.ttf,
  duotone/Phosphor-Duotone.ttf}` (fetched 2026-10-17; SHA-256
  `06b91e022b7ee899a63efced879392a74f0bacbda54e4467e9f663220d173a10` and
  `80e6f0d6fa8ce96a9933f48aaabddc592c5d2b52caf8f010a7e54a17c74084bf`).
- Used by: `pill_editor_core`'s icon module - the egui editor's tab titles,
  toolbars, menus and the floating toolbox draw their icons and their
  duotone two-layer form from these fonts (`icons::Icon`).
