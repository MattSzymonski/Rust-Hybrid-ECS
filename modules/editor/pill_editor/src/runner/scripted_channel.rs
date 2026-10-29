//! The scripted test channel: the verbs, their dumps, and the injections.
//!
//! # Responsibilities
//!
//! - Read the commands a suite appends to `PILL_EDITOR_TEST_COMMANDS` and run
//!   each through [`run_scripted`](EditorWindow::run_scripted).
//! - Answer a command with a log line a suite can wait for, or refuse it
//!   through the one rejection token a suite treats as failure.
//! - Inject the input a suite cannot produce otherwise: keys, clicks, drags
//!   and the snapshots the dumps read.
//!
//! # Design
//!
//! Development only. The grammar itself is `pill_editor_core::scripted`, which
//! the browser editor shares; this module is the native executable's dispatch
//! and its dumps. Every verb here exists to observe something a real press
//! could not reach.
//!
//! Refusals come in two kinds. A line whose own target is wrong - a handle no
//! entity answers to, a field that is not editable, a value the field's kind
//! cannot read, a drag asked to start while another is in flight - is refused
//! through the one rejection token, so a suite's typo fails the way a
//! malformed verb does instead of waiting out a timeout. A line that is right
//! but has arrived before the shell module attached keeps its own warning:
//! the same line works a moment later, so it was never a refused line.

// Current crate
use super::*;

/// Refuses one scripted line, naming it and why it cannot run.
///
/// Every refusal - an unknown verb, an unknown target, a missing or malformed
/// argument, a handle that resolves to nothing - prints this one token, so a
/// suite can tell a rejected line from a slow answer without pinning twenty
/// messages, and `EditorSession` raises on it the moment it lands instead of
/// timing out.
fn reject_scripted(command: &str, reason: &str) {
    warn!(
        target: telemetry_target::ENGINE,
        command,
        reason,
        "[editor] scripted command rejected"
    );
}

/// Refuses a fallible scripted step's failure on the one rejection token.
///
/// The handlers that answer `Result<(), String>` fail for reasons about the
/// line itself - a handle that resolves to nothing, a field that is not
/// editable, a value the field's kind cannot read. Both this and
/// [`reject_scripted`] take no window, because a refusal is about the line: an
/// arm can hand the handler's own result straight here without borrowing the
/// window a second time.
fn refuse_on(line: &str, outcome: Result<(), String>) {
    if let Err(reason) = outcome {
        reject_scripted(line, &reason);
    }
}

impl EditorWindow {
    /// Advance a scripted drag one frame: the press first, then the move,
    /// then the release. The completion token marks the pointer free.
    pub(super) fn advance_scripted_drag(&mut self) {
        let Some(drag) = self.scripted_drag.as_ref() else {
            return;
        };
        let (from, to, stage) = (drag.from, drag.to, drag.stage);
        match stage {
            ScriptedDragStage::PressPending => {
                self.queue_input(InputEvent::CursorMoved {
                    position: pill_core::math::Vector2f::new(from.0, from.1),
                });
                self.queue_input(InputEvent::MouseButton {
                    button: MouseButton::Left,
                    state: ButtonState::Pressed,
                });
                if let Some(drag) = self.scripted_drag.as_mut() {
                    drag.stage = ScriptedDragStage::MovePending;
                }
            }
            ScriptedDragStage::MovePending => {
                self.queue_input(InputEvent::CursorMoved {
                    position: pill_core::math::Vector2f::new(to.0, to.1),
                });
                if let Some(drag) = self.scripted_drag.as_mut() {
                    drag.stage = ScriptedDragStage::ReleasePending;
                }
            }
            ScriptedDragStage::ReleasePending => {
                self.queue_input(InputEvent::MouseButton {
                    button: MouseButton::Left,
                    state: ButtonState::Released,
                });
                self.scripted_drag = None;
                info!(
                    target: telemetry_target::ENGINE,
                    from_x = from.0,
                    from_y = from.1,
                    to_x = to.0,
                    to_y = to.1,
                    "[editor] pointer drag injected"
                );
            }
        }
    }

    /// Log the asset browser's rows: the count first, then one line per
    /// row with its kind and flags - the suite's window into the listing.
    pub(super) fn dump_assets(&mut self) {
        info!(
            target: telemetry_target::ENGINE,
            count = self.asset_entries.len(),
            "[test] assets"
        );
        for row in &self.asset_entries {
            // The root and the flag go last: the suites' patterns read the
            // other fields first, in this order.
            info!(
                target: telemetry_target::ENGINE,
                path = row.key().as_str(),
                kind = row.kind.text(),
                standalone = row.standalone,
                has_meta = row.has_meta,
                root = row.root.as_str(),
                read_only = row.read_only,
                "[test] asset"
            );
        }
    }

    /// Log the Assets tab's open folder and the rows its grid lists there -
    /// the suite's window into the tree and grid navigation, which needs no
    /// pixel clicks.
    pub(super) fn dump_browser(&mut self) {
        let folder = {
            let world = self.host.host_mut().engine_mut().world_mut();
            match world.get_resource::<EguiShellState>() {
                Some(state) => state.browser.folder.clone(),
                None => String::new(),
            }
        };
        let entries = pill_editor_core::browser::children(&self.asset_entries, &folder);
        info!(
            target: telemetry_target::ENGINE,
            folder = folder.as_str(),
            count = entries.len(),
            "[test] browser"
        );
        for row in entries {
            info!(
                target: telemetry_target::ENGINE,
                path = row.path.as_str(),
                kind = row.kind.text(),
                "[test] browser item"
            );
        }
    }

    /// Log the redraw counters, for the suites.
    ///
    /// Two dumps separated by an idle stretch tell the posture apart: with the
    /// loop waiting, the frame count grows by the idle poll's pace, not by the
    /// display's.
    pub(super) fn dump_cadence(&mut self) {
        let paused = self.host.host().engine().systems_paused();
        info!(
            target: telemetry_target::ENGINE,
            continuous = self.cadence.continuous(),
            frames = self.cadence.frames(),
            demanded = self.cadence.demanded(),
            waits = self.cadence.waits(),
            paused,
            input = self.last_demand.input,
            scripted = self.last_demand.scripted,
            snapshot = self.last_demand.snapshot,
            resized = self.last_demand.resized,
            repaint = self.last_demand.repaint_requested,
            animating = self.last_demand.animating,
            "[test] cadence"
        );
    }

    /// Log the Console tab's filters and the rows they leave, for the suites.
    pub(super) fn dump_console(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(state) = world.get_resource::<ConsoleState>() else {
            return;
        };
        let view = state.view();
        // The per-channel counts are what proves a project's own line reached
        // the history: the game channel only fills when something outside the
        // engine logs.
        let engine = state
            .lines
            .iter()
            .filter(|line| line.channel == ConsoleChannel::Engine)
            .count();
        let game = state.lines.len() - engine;
        let first = view
            .lines
            .first()
            .map(|line| format!("{} {}: {}", line.level_name(), line.target, line.message))
            .unwrap_or_default();
        info!(
            target: telemetry_target::ENGINE,
            level = pill_editor_core::console::level_name(view.minimum_level),
            channel = view.channel.map_or("both", ConsoleChannel::name),
            search = view.search.as_str(),
            total = view.total,
            visible = view.visible,
            engine,
            game,
            first = first.as_str(),
            "[test] console"
        );
    }

    /// Log the store's own assets - the ones no file backs, which every picker
    /// offers beside the listing's rows: the suite's window into what a picker
    /// can reach beyond the project's `res`.
    pub(super) fn dump_engine_assets(&mut self) {
        let rows = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world
                .get_resource::<EditorSnapshot>()
                .map(|snapshot| snapshot.engine_assets.clone())
                .unwrap_or_default()
        };
        info!(
            target: telemetry_target::ENGINE,
            count = rows.len(),
            "[test] engine assets"
        );
        for row in &rows {
            info!(
                target: telemetry_target::ENGINE,
                kind = row.kind.as_str(),
                name = row.name.as_str(),
                guid = row.guid.as_deref().unwrap_or(""),
                "[test] engine asset"
            );
        }
    }

    /// Log the entity list and each row's component names, for the suites: a
    /// collection test finds its subject this way.
    pub(super) fn dump_entities(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let entities = world
            .get_resource::<EditorSnapshot>()
            .map(|snapshot| snapshot.entities.clone())
            .unwrap_or_default();
        info!(
            target: telemetry_target::ENGINE,
            count = entities.len(),
            "[test] entities"
        );
        for entry in &entities {
            info!(
                target: telemetry_target::ENGINE,
                display = entry.display.as_str(),
                components = entry.archetype_label.as_str(),
                "[test] entity"
            );
        }
    }

    /// Log one component field's value: the suite's window into the world
    /// (the scripted `dump field <entity> <component> <field>`); the
    /// component name may be abbreviated to its `::Name` suffix.
    pub(super) fn dump_field(&mut self, entity_text: &str, component: &str, field: &str) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let resolved = resolve_display_entity(world, entity_text)
            .and_then(|entity| resolve_component_name(world, component).map(|name| (entity, name)));
        let Some((entity, component)) = resolved else {
            warn!(
                target: telemetry_target::ENGINE,
                entity = entity_text,
                component,
                field,
                "[test] field unreadable"
            );
            return;
        };
        match world.read_component_field(entity, &component, field) {
            Ok(value) => info!(
                target: telemetry_target::ENGINE,
                value = ?value,
                "[test] field"
            ),
            Err(_) => warn!(
                target: telemetry_target::ENGINE,
                entity = entity_text,
                component = component.as_str(),
                field,
                "[test] field unreadable"
            ),
        }
    }

    /// Log the scene grid's state: the switch, the spacing and
    /// the surfaces the renderer draws it on - the same resolution the
    /// renderer makes from the same bindings.
    pub(super) fn dump_grid(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let cameras = world.get_resource::<SurfaceCameras>();
        let grid = cameras.map(|cameras| cameras.grid).unwrap_or_default();
        // The surfaces the renderer draws it on are the frame's editor views -
        // the view-bound set - and it draws them only while the switch is on.
        let surfaces = match cameras {
            Some(cameras) if grid.enabled => editor_view_surfaces(cameras)
                .iter()
                .map(|surface| surface.0.to_string())
                .collect::<Vec<_>>()
                .join(","),
            _ => String::new(),
        };
        info!(
            target: telemetry_target::ENGINE,
            enabled = grid.enabled,
            cell = grid.cell_size,
            surfaces = %surfaces,
            "[test] scene grid"
        );
    }

    /// Log the module panels, for the suites.
    pub(super) fn dump_panels(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(registry) = world.get_resource::<PanelRegistry>() else {
            return;
        };
        let budget = world
            .get_resource::<EguiShellState>()
            .map(|state| state.panel_budget.clone())
            .unwrap_or_default();
        info!(
            target: telemetry_target::ENGINE,
            count = registry.len(),
            live = registry
                .entries()
                .iter()
                .filter(|entry| entry.alive())
                .count(),
            "[test] panels"
        );
        for (slot, entry) in registry.entries().iter().enumerate() {
            let timing = budget.get(slot as u16);
            info!(
                target: telemetry_target::ENGINE,
                slot,
                id = entry.id.as_str(),
                source = entry.source.as_str(),
                alive = entry.alive(),
                epoch = entry.epoch,
                micros = timing.map(|timing| timing.micros).unwrap_or(0.0),
                passes = timing.map(|timing| timing.passes).unwrap_or(0),
                slow = budget.is_slow(slot as u16),
                title = registry.tab_title(slot as u16).as_str(),
                "[test] panel"
            );
        }
    }

    /// Log the preferences file's rows as the Preferences tab's view holds
    /// them, then the project key and its remembered entry - the tab's
    /// exact data, so a suite can assert a save or a tool choice without
    /// reading the tab's pixels.
    pub(super) fn dump_preferences(&mut self) {
        let view = PreferencesView::from_document(
            &self.preferences_document,
            self.gizmo_project.as_deref(),
        );
        info!(
            target: telemetry_target::ENGINE,
            keymap_preset = view.text("keymap_preset"),
            code_editor_command = view.text("code_editor_command"),
            autosave_interval_seconds = view.text("autosave_interval_seconds"),
            snapping_translation = view.text("snapping_steps.translation"),
            snapping_rotation_degrees = view.text("snapping_steps.rotation_degrees"),
            snapping_scale = view.text("snapping_steps.scale"),
            history_step_cap = view.text("history_step_cap"),
            history_memory_budget_mb = view.text("history_memory_budget_mb"),
            "[test] preferences"
        );
        info!(
            target: telemetry_target::ENGINE,
            project = view.project.as_deref().unwrap_or("-"),
            remembered = view.project_remembered,
            mode = ?view.project_gizmo.mode,
            space = ?view.project_gizmo.space,
            snap = ?view.project_gizmo.snap,
            "[test] preferences project"
        );
    }

    /// Log the Problems rows, for the suites.
    pub(super) fn dump_problems(&mut self) {
        info!(
            target: telemetry_target::ENGINE,
            count = self.notices.problems.len(),
            "[test] problems"
        );
        for row in self.notices.problems.iter().take(10) {
            info!(
                target: telemetry_target::ENGINE,
                module = row.module.as_str(),
                severity = row.problem.severity.name(),
                file = row.problem.file.as_deref().unwrap_or(""),
                line = row.problem.line.unwrap_or(0),
                text = row.problem.message.as_str(),
                "[test] problem row"
            );
        }
    }

    /// Log the editable resources and their values, for the suites.
    pub(super) fn dump_resources(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let resources = world
            .get_resource::<EditorSnapshot>()
            .map(|snapshot| snapshot.resources.clone())
            .unwrap_or_default();
        info!(
            target: telemetry_target::ENGINE,
            count = resources.len(),
            "[test] resources"
        );
        for view in &resources {
            info!(
                target: telemetry_target::ENGINE,
                name = view.name.as_str(),
                label = view.label.as_str(),
                fields = view.fields.len(),
                "[test] resource"
            );
            for field in &view.fields {
                info!(
                    target: telemetry_target::ENGINE,
                    resource = view.name.as_str(),
                    field = field.field.as_str(),
                    label = field.label.as_str(),
                    value = inspector::value_text(&field.value).as_str(),
                    "[test] resource field"
                );
            }
        }
    }

    /// Log every scene view's placed rectangle, in the main window's
    /// physical pixels, for the suite's scripted clicks, and the game
    /// window's own rectangle when it exists.
    pub(super) fn dump_scenes(&mut self) {
        for view in 0..self.views.len() {
            let Some(scene) = &self.views[view] else {
                continue;
            };
            let (origin_x, origin_y) = self.view_origin(view);
            let (width, height) = scene.window.size();
            info!(
                target: telemetry_target::ENGINE,
                view,
                x = origin_x as i32,
                y = origin_y as i32,
                width,
                height,
                visible = scene.window.visible,
                "[editor] scene view rect"
            );
        }
        if let Some(game) = &self.game_surface {
            let (width, height) = game.size();
            info!(
                target: telemetry_target::ENGINE,
                x = game.origin.0,
                y = game.origin.1,
                width,
                height,
                visible = game.visible,
                "[editor] game view rect"
            );
        }
    }

    /// Log the selected entities, primary last (`dump selection`).
    pub(super) fn dump_selection(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let (entities, asset) = world
            .get_resource::<EditorSelection>()
            .map(|selection| (selection.entities().to_vec(), selection.asset.clone()))
            .unwrap_or_default();
        info!(
            target: telemetry_target::ENGINE,
            count = entities.len(),
            asset = asset.as_deref().unwrap_or("none"),
            primary = entities
                .last()
                .map(|entity| entity.to_string())
                .unwrap_or_else(|| "none".to_string())
                .as_str(),
            "[test] selection"
        );
        for entity in &entities {
            info!(
                target: telemetry_target::ENGINE,
                entity = %entity,
                "[test] selected entity"
            );
        }
    }

    /// Log the live standalone types the create dialog offers; the suite
    /// reads the first one to drive a create.
    pub(super) fn dump_standalone_types(&mut self) {
        for info_type in &self.standalone_types {
            info!(
                target: telemetry_target::ENGINE,
                type_name = info_type.type_name.as_str(),
                display = info_type.display_name.as_str(),
                extension = info_type.extension.as_str(),
                "[test] standalone type"
            );
        }
    }

    /// Log the Statistics tab's numbers, for the suites.
    pub(super) fn dump_stats(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(state) = world.get_resource::<EguiShellState>() else {
            return;
        };
        let stats = &state.stats;
        info!(
            target: telemetry_target::ENGINE,
            fps = stats.frames_per_second,
            shell_us = stats.shell_micros,
            passes = stats.shell_passes,
            tabs = stats.tabs.len(),
            "[test] stats"
        );
        for entry in &stats.tabs {
            info!(
                target: telemetry_target::ENGINE,
                name = entry.tab.name(),
                us = entry.micros,
                passes = entry.passes,
                "[test] stats tab"
            );
        }
    }

    /// Log every registered system with the state the Systems tab draws,
    /// then the engine's pause flag - the tab's exact data, so a suite can
    /// assert a toggle or a pause without reading the tab's pixels.
    pub(super) fn dump_systems(&mut self) {
        let (systems, paused) = {
            let world = self.host.host_mut().engine_mut().world_mut();
            match world.get_resource::<EditorSnapshot>() {
                Some(snapshot) => (snapshot.systems.clone(), snapshot.paused),
                None => (Vec::new(), false),
            }
        };
        info!(
            target: telemetry_target::ENGINE,
            count = systems.len(),
            paused,
            "[test] systems"
        );
        for system in &systems {
            info!(
                target: telemetry_target::ENGINE,
                index = system.index,
                name = system.name.as_str(),
                owner = system.owner_label.as_str(),
                enabled = system.enabled,
                hot_patchable = system.hot_patchable,
                ambiguous = system.name_is_ambiguous,
                "[test] system"
            );
        }
    }

    /// Log the dock's surfaces and tabs, for the suite's assertions.
    pub(super) fn dump_tabs(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(state) = world.get_resource::<EguiShellState>() else {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] dump needs the shell module's state"
            );
            return;
        };
        let surfaces = state.dock.state().surfaces_count();
        info!(
            target: telemetry_target::ENGINE,
            surfaces,
            "[editor] egui dock dump"
        );
        // One line per leaf, so a suite can tell a restored layout from the
        // default: which tabs a group holds and which of them is in front.
        let leaves: Vec<(usize, usize, String, String)> = state
            .dock
            .state()
            .iter_all_nodes()
            .filter_map(|(path, node)| match node {
                pill_editor_core::egui_dock::Node::Leaf(leaf) => {
                    let tabs = leaf
                        .tabs
                        .iter()
                        .map(|tab| tab.name())
                        .collect::<Vec<_>>()
                        .join(",");
                    let active = leaf
                        .tabs
                        .get(leaf.active.0)
                        .map_or_else(String::new, |tab| tab.name().to_string());
                    Some((path.surface.0, path.node.0, active, tabs))
                }
                _ => None,
            })
            .collect();
        for (surface, node, active, tabs) in leaves {
            info!(
                target: telemetry_target::ENGINE,
                surface,
                node,
                active = %active,
                tabs = %tabs,
                "[editor] egui dock leaf"
            );
        }
        // Popped-out tabs are not in the tree, so they get their own lines:
        // a suite reads which tabs left the dock and which came back.
        let popouts: Vec<String> = state.dock.popouts().iter().map(|tab| tab.name()).collect();
        for tab in popouts {
            info!(
                target: telemetry_target::ENGINE,
                tab = tab.as_str(),
                "[editor] egui dock popout"
            );
        }
    }
    /// Log the theme the editor paints with, the layers it was merged from and
    /// the generation that produced it (`dump theme`).
    ///
    /// This is the executable's own resolution - what the shell installs - so
    /// a suite can assert a layer change without reading the tab's pixels.
    pub(super) fn dump_theme(&mut self) {
        let user = self
            .theme_layers
            .iter()
            .find(|info| info.layer == ThemeLayer::User)
            .map_or("none", ThemeLayerInfo::path_text);
        let project = self
            .theme_layers
            .iter()
            .find(|info| info.layer == ThemeLayer::Project)
            .map_or("none", ThemeLayerInfo::path_text);
        info!(
            target: telemetry_target::ENGINE,
            layers = %layers_text(&self.theme_layers),
            user = %user,
            project = %project,
            generation = self.theme_generation,
            accent = %self.theme.palette.accent.hex(),
            panel = %self.theme.palette.panel.hex(),
            corner = self.theme.shape.corner_radius,
            "[test] theme"
        );
    }

    /// Log the active viewport, the interaction state, the hovered handle
    /// and every view's camera: the suite's window into navigation and the
    /// viewport routing, with no pixel clicks.
    pub(super) fn dump_viewport(&mut self) {
        let hovered = self
            .hovered_handle
            .map(|handle| handle.to_string())
            .unwrap_or_else(|| "-".to_string());
        info!(
            target: telemetry_target::ENGINE,
            active = self.active_view,
            interaction = self.interaction_name(),
            hovered = hovered.as_str(),
            keymap = %self.keymap,
            "[test] viewport"
        );
        for view in 0..self.views.len() {
            let camera = self.cameras[view];
            let eye = camera.eye();
            let pivot = camera.pivot;
            info!(
                target: telemetry_target::ENGINE,
                view,
                eye = format!("{:.3},{:.3},{:.3}", eye[0], eye[1], eye[2]).as_str(),
                pivot = format!("{:.3},{:.3},{:.3}", pivot[0], pivot[1], pivot[2]).as_str(),
                distance = camera.distance,
                yaw = camera.yaw,
                pitch = camera.pitch,
                "[test] viewport camera"
            );
        }
    }

    /// Log the panel windows on screen, for the suites.
    pub(super) fn dump_windows(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let windows = world
            .get_resource::<EguiShellState>()
            .map(|state| state.panel_windows.windows.clone())
            .unwrap_or_default();
        info!(
            target: telemetry_target::ENGINE,
            count = windows.len(),
            "[test] windows"
        );
        for window in &windows {
            info!(
                target: telemetry_target::ENGINE,
                id = window.id.as_str(),
                source = window.source.as_str(),
                slot = window.slot,
                modal = window.modal,
                "[test] window"
            );
        }
    }

    /// Inject a full left click at a point of the window's client area, in
    /// its physical pixels, through the same engine input seam a real click
    /// enters: the shell's egui pass hit-tests and routes it, so an overlay
    /// widget takes it while a point with no widget still reaches the
    /// scene. Press and release arrive in one input frame, which the engine
    /// reports as both edges; the egui bridge forwards them in order.
    pub(super) fn inject_click(&mut self, x: f32, y: f32) {
        let position = pill_core::math::Vector2f::new(x, y);
        self.queue_input(InputEvent::CursorMoved { position });
        self.queue_input(InputEvent::MouseButton {
            button: MouseButton::Left,
            state: ButtonState::Pressed,
        });
        self.queue_input(InputEvent::MouseButton {
            button: MouseButton::Left,
            state: ButtonState::Released,
        });
        info!(
            target: telemetry_target::ENGINE,
            x,
            y,
            "[editor] click injected"
        );
        // A click inside a visible scene viewport activates it and asks what
        // is under it, through the same press the window events route: the
        // pick's answer moves the shared selection, and the outline follows.
        // A point over the overlay's chips is the UI's, not the scene's.
        if let Some(view) = self.scene_view_at(x, y) {
            let scale = self.scale_factor;
            let over_ui = self
                .overlay_regions
                .iter()
                .any(|region| region.contains(x / scale, y / scale));
            if !over_ui {
                let (origin_x, origin_y) = self.view_origin(view);
                info!(
                    target: telemetry_target::ENGINE,
                    view,
                    x,
                    y,
                    "[editor] scene view click"
                );
                // A scripted click is a press and a release in one command:
                // the pick answers later with the button already up, so a
                // handle under the point does not start a drag.
                self.activate_view(view);
                self.press_scene(view, x - origin_x, y - origin_y);
                self.release_scene(view);
            }
        }
    }

    /// Inject a keyboard chord through the same engine input seam a real key
    /// enters: the modifiers, the press, the typed text and the release, in
    /// the order a window reports them.
    ///
    /// The editor's own tool shortcuts read winit events rather than the
    /// engine's input stream, so the chord also runs the handler a real press
    /// runs - without that, `key w` would reach the shell but never switch
    /// the tool.
    pub(super) fn inject_key(&mut self, chord: &KeyChord) {
        let held = EngineModifiers {
            shift: chord.shift,
            control: chord.control,
            alt: chord.alt,
            super_key: chord.super_key,
        };
        self.queue_input(InputEvent::ModifiersChanged { modifiers: held });
        self.queue_input(InputEvent::Key {
            key: chord.key,
            state: ButtonState::Pressed,
        });
        if let Some(character) = chord.typed_character() {
            self.queue_input(InputEvent::TextInput {
                text: character.to_string(),
            });
        }
        self.queue_input(InputEvent::Key {
            key: chord.key,
            state: ButtonState::Released,
        });
        // The chord releases its modifiers as it ends: a later injection must
        // not inherit a held Control from this one.
        self.queue_input(InputEvent::ModifiersChanged {
            modifiers: EngineModifiers {
                shift: false,
                control: false,
                alt: false,
                super_key: false,
            },
        });
        info!(
            target: telemetry_target::ENGINE,
            chord = chord.label().as_str(),
            "[editor] key injected"
        );
        // A scripted chord runs what a real press runs: the same
        // registry-resolved command, with the modifiers the chord named.
        self.run_bound_command(
            chord.key,
            EngineModifiers {
                shift: chord.shift,
                control: chord.control,
                alt: chord.alt,
                super_key: chord.super_key,
            },
        );
    }

    /// Inject a full right click at a point of the window's client area,
    /// through the same engine input seam a real click enters: the shell's
    /// egui pass hit-tests and routes it, so a context menu opens over
    /// whatever is under the pointer. No scene pick: a right click is the
    /// UI's, never the scene's.
    pub(super) fn inject_rclick(&mut self, x: f32, y: f32) {
        let position = pill_core::math::Vector2f::new(x, y);
        self.queue_input(InputEvent::CursorMoved { position });
        self.queue_input(InputEvent::MouseButton {
            button: MouseButton::Right,
            state: ButtonState::Pressed,
        });
        self.queue_input(InputEvent::MouseButton {
            button: MouseButton::Right,
            state: ButtonState::Released,
        });
        info!(
            target: telemetry_target::ENGINE,
            x,
            y,
            "[editor] right click injected"
        );
    }

    /// The viewport interaction state's name: one owner at a time, so the
    /// dump and the logs say who has the pointer.
    pub(super) fn interaction_name(&self) -> &'static str {
        if self.camera_drag.is_some() {
            return "navigating";
        }
        match self.gizmo_drag.as_ref().map(|drag| drag.kind) {
            Some(HandleKind::Plane { .. }) => "dragging-plane",
            Some(HandleKind::Axis { .. }) => "dragging-axis",
            None if self.hovered_handle.is_some() => "hovering",
            None => "idle",
        }
    }

    /// Run the scripted commands appended since the last frame.
    ///
    /// The grammar lives in [`pill_editor_core::scripted`]: this parses each
    /// line and dispatches it, and a line that cannot be parsed takes the one
    /// refusal path, so a suite sees it at once instead of waiting out a
    /// timeout.
    ///
    /// What a line queues is applied before the next line runs: one append
    /// reaches the loop as one batch, so without this an append that edits and
    /// then reads would answer the value the edit replaced.
    pub(super) fn poll_test_channel(&mut self) {
        let lines = match &mut self.test_channel {
            Some(channel) => channel.poll(),
            None => return,
        };
        for line in lines {
            // A scripted line arrives between frames; the loop must not stay
            // asleep over it.
            self.demand.scripted = true;
            match ScriptedCommand::parse(&line) {
                Ok(command) => self.run_scripted(&line, command),
                Err(error) => reject_scripted(&line, &error.reason),
            }
            self.apply_pending_commands();
        }
    }

    /// Queue one scripted event: to the engine, and to the shell's list too.
    ///
    /// A scripted click or key stands in for a user's, so the UI must see it
    /// the way a real event arrives - through the shell's own list - while
    /// the game still receives what a user's would.
    pub(super) fn queue_input(&mut self, event: InputEvent) {
        if let InputEvent::CursorMoved { position } = event {
            // The scripted pointer is the shell's pointer too: the seed the
            // next frame carries has to match what the shell just saw.
            self.shell_cursor = Some(position);
        }
        self.host
            .host_mut()
            .engine_mut()
            .push_input_event(event.clone());
        self.shell_events.push(event);
    }

    /// Run one parsed scripted command.
    ///
    /// No grammar is left here: an arm names its side effect, so the verbs,
    /// their arguments and their refusal sentences have exactly one home.
    pub(super) fn run_scripted(&mut self, line: &str, command: ScriptedCommand) {
        match command {
            ScriptedCommand::Quit => {
                info!(target: telemetry_target::ENGINE, "[editor] quit requested");
                self.request_quit();
            }
            ScriptedCommand::Undock(tab) => self.undock_tab(tab),
            ScriptedCommand::ActivateTab(tab) => self.activate_tab(tab),
            ScriptedCommand::Palette(request) => self.scripted_palette(request),
            ScriptedCommand::ConsoleFilter(level) => self.scripted_console_filter(&level),
            ScriptedCommand::ConsoleSearch(text) => self.scripted_console_search(text),
            ScriptedCommand::ProblemOpen(index) => self.scripted_problem_open(index),
            ScriptedCommand::SafeModeRetry => self.retry_shell_module(),
            ScriptedCommand::PanelUnload(source) => self.scripted_panel_unload(&source),
            ScriptedCommand::PanelRestore(source) => self.scripted_panel_restore(&source),
            ScriptedCommand::PanelWindow { slot, id } => self.scripted_panel_window(slot, &id),
            ScriptedCommand::PanelField {
                component,
                field,
                value,
            } => refuse_on(line, self.scripted_panel_field(&component, &field, &value)),
            ScriptedCommand::Reveal(path) => {
                let world = self.host.host_mut().engine_mut().world_mut();
                if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
                    pending.reveal(path.clone());
                }
                info!(
                    target: telemetry_target::ENGINE,
                    path = path.as_str(),
                    "[editor] reveal queued"
                );
            }
            ScriptedCommand::PickReference {
                asset,
                key,
                candidate,
            } => {
                // The choice a picker row makes: the candidate's own guid,
                // written through the same settings edit the row queues.
                let guid = self
                    .asset_entries
                    .iter()
                    .find(|row| row.path == candidate || row.key() == candidate)
                    .and_then(|row| row.guid.clone());
                match guid {
                    Some(guid) => {
                        info!(
                            target: telemetry_target::ENGINE,
                            key = key.as_str(),
                            candidate = candidate.as_str(),
                            guid = guid.as_str(),
                            "[editor] reference pick queued"
                        );
                        self.pick_reference(&asset, &key, &guid);
                    }
                    None => {
                        reject_scripted(line, "pick reference needs a candidate carrying a guid")
                    }
                }
            }
            ScriptedCommand::Click { x, y } => self.inject_click(x, y),
            ScriptedCommand::RightClick { x, y } => self.inject_rclick(x, y),
            ScriptedCommand::Drag { from, to } => {
                refuse_on(line, self.start_scripted_drag(from, to))
            }
            ScriptedCommand::GizmoDrag { target, delta } => {
                match self.scripted_gizmo_handle(&target) {
                    Some(handle) => self.start_synthetic_gizmo_drag(handle, Some(delta)),
                    None => reject_scripted(line, "gizmo drag needs a known axis or plane"),
                }
            }
            ScriptedCommand::GizmoPress { target } => match self.scripted_gizmo_handle(&target) {
                Some(handle) => self.start_synthetic_gizmo_drag(handle, None),
                None => reject_scripted(line, "gizmo press needs a known axis or plane"),
            },
            ScriptedCommand::GizmoRelease => refuse_on(line, self.scripted_gizmo_release()),
            ScriptedCommand::GizmoTool(mode) => self.set_gizmo_tool(mode),
            ScriptedCommand::Key(chord) => self.inject_key(&chord),
            ScriptedCommand::Dump(dump) => match dump {
                Dump::Tabs => self.dump_tabs(),
                Dump::Scenes => self.dump_scenes(),
                Dump::Viewport => self.dump_viewport(),
                Dump::Grid => self.dump_grid(),
                Dump::Assets => self.dump_assets(),
                Dump::EngineAssets => self.dump_engine_assets(),
                Dump::Browser => self.dump_browser(),
                Dump::StandaloneTypes => self.dump_standalone_types(),
                Dump::Systems => self.dump_systems(),
                Dump::Preferences => self.dump_preferences(),
                Dump::Theme => self.dump_theme(),
                Dump::Console => self.dump_console(),
                Dump::Problems => self.dump_problems(),
                Dump::Stats => self.dump_stats(),
                Dump::Cadence => self.dump_cadence(),
                Dump::Panels => self.dump_panels(),
                Dump::Windows => self.dump_windows(),
                Dump::Resources => self.dump_resources(),
                Dump::Entities => self.dump_entities(),
                Dump::Selection => self.dump_selection(),
                Dump::Field {
                    entity,
                    component,
                    field,
                } => self.dump_field(&entity, &component, &field),
            },
            ScriptedCommand::Browser(folder) => self.scripted_browser(&folder),
            ScriptedCommand::Camera { navigation, delta } => {
                self.scripted_camera(navigation, delta.0, delta.1)
            }
            ScriptedCommand::CameraZoom(lines) => self.zoom_camera(self.active_view, lines),
            ScriptedCommand::Hover { view, x, y } => {
                refuse_on(line, self.scripted_hover(view, x, y))
            }
            ScriptedCommand::HoverUi { x, y } => self.scripted_hover_ui(x, y),
            ScriptedCommand::Drop(path) => self.drop_files(vec![std::path::PathBuf::from(path)]),
            ScriptedCommand::HoverFile(path) => self.preview_drop(std::path::Path::new(&path)),
            ScriptedCommand::DragOut(path) => self.start_drag_out(&path),
            ScriptedCommand::SelectAsset(path) => self.scripted_select_asset(&path),
            ScriptedCommand::SelectEntity(handle) => {
                refuse_on(line, self.scripted_select_entity(&handle))
            }
            ScriptedCommand::SelectAdd(handle) => refuse_on(
                line,
                self.scripted_select_extend(&handle, SelectionClick::Add),
            ),
            ScriptedCommand::SelectRange(handle) => refuse_on(
                line,
                self.scripted_select_extend(&handle, SelectionClick::Range),
            ),
            ScriptedCommand::SelectResource(name) => self.scripted_select_resource(&name),
            ScriptedCommand::EditResource {
                resource,
                field,
                value,
            } => refuse_on(line, self.scripted_edit_resource(&resource, &field, &value)),
            ScriptedCommand::Toolbar(action) => self.scripted_toolbar(action),
            ScriptedCommand::SceneNew => self.scripted_scene(EditorCommand::SceneNew),
            ScriptedCommand::SceneOpen(path) => {
                self.scripted_scene(EditorCommand::SceneOpen { path })
            }
            ScriptedCommand::SceneSave(path) => match path {
                Some(path) => self.scripted_scene(EditorCommand::SceneSaveAs { path }),
                None => self.scripted_scene(EditorCommand::SceneSave),
            },
            ScriptedCommand::DialogSetName(text) => self.scripted_dialog_set_name(&text),
            ScriptedCommand::DialogSubmit => self.scripted_dialog_submit(),
            ScriptedCommand::EditAsset(edit) => self.scripted_asset_edit(edit),
            ScriptedCommand::PickEntity {
                entity,
                component,
                field,
            } => refuse_on(line, self.scripted_pick_entity(&entity, &component, &field)),
            ScriptedCommand::EditSet {
                entity,
                component,
                field,
                value,
            } => refuse_on(
                line,
                self.scripted_edit_set(&entity, &component, &field, &value),
            ),
            ScriptedCommand::EditArray {
                entity,
                component,
                field,
                index,
                value,
            } => refuse_on(
                line,
                self.scripted_edit_array(&entity, &component, &field, index, &value),
            ),
            ScriptedCommand::EditSelection {
                component,
                field,
                value,
            } => refuse_on(
                line,
                self.scripted_edit_selection(&component, &field, &value),
            ),
            ScriptedCommand::EditAssetSettings { path, key, value } => {
                refuse_on(line, self.scripted_asset_settings(&path, &key, &value))
            }
            ScriptedCommand::EditSystem { index, enabled } => {
                self.scripted_edit_system(index, enabled)
            }
            ScriptedCommand::EditPreference { key, value } => {
                self.scripted_edit_preference(&key, &value)
            }
            ScriptedCommand::EditComponent {
                entity,
                component,
                add,
            } => refuse_on(line, self.scripted_component_edit(&entity, &component, add)),
            ScriptedCommand::Undo => self.scripted_undo(),
            ScriptedCommand::Redo => self.scripted_redo(),
            ScriptedCommand::Copy => self.scripted_clipboard(ClipboardAction::Copy),
            ScriptedCommand::Cut => self.scripted_clipboard(ClipboardAction::Cut),
            ScriptedCommand::Paste => self.scripted_clipboard(ClipboardAction::Paste),
            ScriptedCommand::PauseToggle => self.scripted_pause_toggle(),
            ScriptedCommand::NewSceneView => refuse_on(line, self.scripted_new_scene_view()),
            ScriptedCommand::Popout { tab, redock } => {
                refuse_on(line, self.scripted_popout(tab, redock))
            }
            ScriptedCommand::Step => self.scripted_step(),
            ScriptedCommand::EntityCreate => self.scripted_entity_create(),
            ScriptedCommand::EntityDelete => self.scripted_entity_delete(),
            ScriptedCommand::SnapToggle => {
                let snap = self.gizmo_settings().snap;
                self.scripted_gizmo_tool(None, None, Some(!snap))
            }
            ScriptedCommand::SpaceToggle => {
                let space = self.gizmo_settings().space;
                let next = match space {
                    GizmoSpace::World => GizmoSpace::Local,
                    GizmoSpace::Local => GizmoSpace::World,
                };
                self.scripted_gizmo_tool(None, Some(next), None)
            }
        }
    }

    /// One `edit asset` command, queued as the recorded edit the Assets tab's
    /// actions also queue.
    pub(super) fn scripted_asset_edit(&mut self, edit: AssetEdit) {
        let (command_value, label) = match edit {
            AssetEdit::Create {
                type_name,
                folder,
                name,
            } => (
                EditorCommand::CreateStandaloneAsset {
                    folder: if folder == "-" { String::new() } else { folder },
                    name: name.clone(),
                    type_name,
                    overwrite: None,
                },
                format!("create asset {name}"),
            ),
            AssetEdit::Delete { path } => (
                EditorCommand::DeleteAssetFile { path: path.clone() },
                format!("delete asset {path}"),
            ),
            AssetEdit::Move { from, to } => (
                EditorCommand::MoveAssetFile {
                    from: from.clone(),
                    to,
                },
                format!("move asset {from}"),
            ),
            AssetEdit::Meta { path } => (
                EditorCommand::WriteAssetMeta { path: path.clone() },
                format!("write meta {path}"),
            ),
        };
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(vec![command_value], label, None);
        }
    }

    /// One scripted asset-settings edit (`edit asset-settings <path> <key>
    /// <value>`): the same recorded `SetAssetSettings` batch the Inspector's
    /// settings rows queue, parsed against the leaf's current JSON kind.
    pub(super) fn scripted_asset_settings(
        &mut self,
        path: &str,
        key: &str,
        value_text: &str,
    ) -> Result<(), String> {
        let key_path = inspector::parse_settings_key_path(key)?;
        let Some(document) = self.read_asset_settings(path) else {
            return Err(format!("no asset at '{path}' has a settings document"));
        };
        // A leaf the document does not hold yet reads as text: that is the
        // unbound-slot case, and the picker's value is a guid string.
        let kind = inspector::settings_field(&document, &key_path)
            .cloned()
            .unwrap_or_else(|| serde_json::Value::String(String::new()));
        let value = inspector::parse_settings_value(value_text, &kind)?;
        let mut patched = document;
        // The same inserting rule the Inspector's settings rows use.
        inspector::patch_settings_inserting(&mut patched, &key_path, value)?;
        self.queue_settings_value(path, key, patched);
        Ok(())
    }

    /// Open a folder in the Assets tab's browser (`root` opens the asset
    /// root), through the shell's session state - the scripted channel's
    /// stand-in for clicking the folder tree.
    pub(super) fn scripted_browser(&mut self, folder: &str) {
        let folder = if folder == "root" { "" } else { folder };
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(state) = world.get_resource_mut::<EguiShellState>() else {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] browser navigation needs the shell module's state"
            );
            return;
        };
        if !state.browser.open(folder) {
            info!(
                target: telemetry_target::ENGINE,
                folder,
                "[editor] browser folder unchanged"
            );
        }
    }

    /// Apply a scripted camera gesture to the active viewport, through the
    /// same math a drag uses.
    pub(super) fn scripted_camera(
        &mut self,
        navigation: CameraNavigation,
        delta_x: f32,
        delta_y: f32,
    ) {
        let view = self.active_view;
        self.apply_camera_gesture(view, navigation, delta_x, delta_y);
        info!(
            target: telemetry_target::ENGINE,
            view,
            navigation = camera_navigation_name(navigation),
            "[editor] camera gesture applied"
        );
    }

    /// One scripted clipboard command (`command editor.copy/cut/paste`): the
    /// same request the Edit menu's rows queue, so the test channel drives the
    /// menu's own path.
    pub(super) fn scripted_clipboard(&mut self, action: ClipboardAction) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let selection = world
            .get_resource::<EditorSelection>()
            .cloned()
            .unwrap_or_default();
        let snapshot = world
            .get_resource::<EditorSnapshot>()
            .cloned()
            .unwrap_or_default();
        let summary = match world.get_resource_mut::<EditorClipboard>() {
            Some(clipboard) => clipboard::queue_request(clipboard, action, &selection, &snapshot),
            None => None,
        };
        // The cut's delete is queued from its own borrow: the two resources
        // cannot be held mutably at once.
        if action == ClipboardAction::Cut {
            if let (Some((commands, label)), Some(pending)) = (
                clipboard::cut_commands(&selection),
                world.get_resource_mut::<PendingEditorCommands>(),
            ) {
                pending.push_recorded(commands, label, None);
            }
        }
        info!(
            target: telemetry_target::ENGINE,
            action = match action {
                ClipboardAction::Copy => "copy",
                ClipboardAction::Cut => "cut",
                ClipboardAction::Paste => "paste",
            },
            payload = summary.as_deref().unwrap_or("nothing"),
            "[editor] clipboard queued"
        );
    }

    /// Run `edit add <entity> <component>` / `edit remove <entity>
    /// <component>` from the scripted channel: the same recorded component
    /// attach or detach the Inspector's menu items queue.
    pub(super) fn scripted_component_edit(
        &mut self,
        entity_text: &str,
        component_text: &str,
        add: bool,
    ) -> Result<(), String> {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(entity) = resolve_display_entity(world, entity_text) else {
            return Err(format!("no entity named '{entity_text}'"));
        };
        let Some(component) = resolve_component_name(world, component_text) else {
            return Err(format!("no component named '{component_text}'"));
        };
        let (edit, label) = if add {
            inspector::component_add(entity, &component)
        } else {
            inspector::component_remove(entity, &component)
        };
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(vec![edit], label, None);
        }
        info!(
            target: telemetry_target::ENGINE,
            component = component.as_str(),
            add,
            "[editor] component edit queued"
        );
        Ok(())
    }

    /// Run `console filter <level>`: the lowest level the Console tab shows,
    /// which the next frame applies to the history.
    pub(super) fn scripted_console_filter(&mut self, level: &str) {
        let Some(value) = pill_editor_core::console::level_from_name(level) else {
            warn!(
                target: telemetry_target::ENGINE,
                level,
                "[editor] console filter ignored"
            );
            return;
        };
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(state) = world.get_resource_mut::<EguiShellState>() {
            state.console_filters.minimum_level = value;
        }
        info!(
            target: telemetry_target::ENGINE,
            level,
            "[editor] console filter"
        );
    }

    /// Run `console search <text>`: the Console tab's substring filter.
    pub(super) fn scripted_console_search(&mut self, text: String) {
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(state) = world.get_resource_mut::<EguiShellState>() {
            state.console_filters.search = text.clone();
        }
        info!(
            target: telemetry_target::ENGINE,
            search = text.as_str(),
            "[editor] console search"
        );
    }

    /// Set the open dialog's name field - what typing into the window's
    /// text field does.
    pub(super) fn scripted_dialog_set_name(&mut self, name: &str) {
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(state) = world.get_resource_mut::<EguiShellState>() {
            match state.asset_dialog.as_mut() {
                Some(AssetDialog::Create { name: field, .. })
                | Some(AssetDialog::Rename { name: field, .. }) => {
                    *field = name.to_string();
                }
                None => warn!(
                    target: telemetry_target::ENGINE,
                    "[editor] dialog set without an open dialog"
                ),
            }
        }
    }

    /// Run the open dialog's submit button: the dialog is taken out of the
    /// session, submitted, and put back, because the submit needs the
    /// pending queue - a second mutable resource the world cannot lend
    /// alongside the session.
    pub(super) fn scripted_dialog_submit(&mut self) {
        let mut dialog = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world
                .get_resource_mut::<EguiShellState>()
                .and_then(|state| state.asset_dialog.take())
        };
        if dialog.is_none() {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] dialog submit without an open dialog"
            );
            return;
        }
        {
            let world = self.host.host_mut().engine_mut().world_mut();
            if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
                match dialog.as_ref() {
                    Some(AssetDialog::Create { .. }) => {
                        assets::submit_create(&mut dialog, pending);
                    }
                    Some(AssetDialog::Rename { .. }) => {
                        assets::submit_rename(&mut dialog, pending);
                    }
                    None => {}
                }
            }
        }
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(state) = world.get_resource_mut::<EguiShellState>() {
            state.asset_dialog = dialog;
        }
    }

    /// One scripted array-element edit (`edit array <entity> <component>
    /// <field> <index> <value>`): the same recorded `SetArrayElement` batch
    /// an inspector element widget queues.
    pub(super) fn scripted_edit_array(
        &mut self,
        entity_text: &str,
        component_text: &str,
        field: &str,
        index: usize,
        value_text: &str,
    ) -> Result<(), String> {
        let (entity, component, current) =
            self.resolve_edit_target(entity_text, component_text, field)?;
        let recorded =
            inspector::element_write(entity, &component, field, &current, index, value_text)?;
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(recorded.commands, recorded.label, Some(recorded.coalesce));
        }
        Ok(())
    }

    /// One scripted preferences row (`edit preference <key> <value...>`):
    /// the same `SetPreferences` batch the tab's Save button queues, with
    /// the one row. The value is everything after the key, so a text value
    /// keeps its spaces.
    pub(super) fn scripted_edit_preference(&mut self, key: &str, value: &str) {
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push(vec![EditorCommand::SetPreferences {
                values: vec![(key.to_string(), value.to_string())],
            }]);
        }
        info!(
            target: telemetry_target::ENGINE,
            key,
            value,
            "[editor] preference edit queued"
        );
    }

    /// `edit resource <field> <value>`: one recorded `SetResourceField`.
    pub(super) fn scripted_edit_resource(
        &mut self,
        resource: &str,
        field: &str,
        value_text: &str,
    ) -> Result<(), String> {
        let world = self.host.host_mut().engine_mut().world_mut();
        let current = world.get_resource::<GizmoState>().and_then(|state| {
            pill_editor_core::gizmo::resource_field_value(&state.settings, resource, field).ok()
        });
        let Some(current) = current else {
            return Err(format!("'{resource}' has no editable field '{field}'"));
        };
        let value = inspector::parse_field_value(value_text, &current)?;
        let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() else {
            return Ok(());
        };
        pending.push_recorded(
            vec![EditorCommand::SetResourceField {
                resource: resource.to_string(),
                field: field.to_string(),
                value,
            }],
            format!("set {resource}.{field}"),
            Some(format!("resource.{resource}.{field}")),
        );
        info!(
            target: telemetry_target::ENGINE,
            resource,
            field,
            value = value_text,
            "[editor] resource edit queued"
        );
        Ok(())
    }

    /// One scripted whole-field edit over the whole selection (`edit selection
    /// <component> <field> <value>`): the recorded batch an Inspector row
    /// queues when several entities are selected.
    ///
    /// The value parses against the primary's field, and the write reaches
    /// every selected entity whose detail carries the component, so an
    /// `EditorCommand::SetField` step is recorded once for the set and undoes
    /// in one step.
    pub(super) fn scripted_edit_selection(
        &mut self,
        component_text: &str,
        field: &str,
        value_text: &str,
    ) -> Result<(), String> {
        let (primary, details) = {
            let world = self.host.host_mut().engine_mut().world_mut();
            let Some(primary) = world
                .get_resource::<EditorSelection>()
                .and_then(|selection| selection.primary())
            else {
                return Err("edit selection needs a selected entity".to_string());
            };
            let details = world
                .get_resource::<EditorSnapshot>()
                .map(|snapshot| snapshot.selected_details.clone())
                .unwrap_or_default();
            (primary, details)
        };
        let (entity, component, current) =
            self.resolve_edit_target(&primary.to_string(), component_text, field)?;
        let targets = inspector::selection_targets(&details, entity, &component);
        let recorded =
            inspector::selection_write(&targets, entity, &component, field, &current, value_text)?;
        let written = recorded.commands.len();
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(recorded.commands, recorded.label, Some(recorded.coalesce));
        }
        info!(
            target: telemetry_target::ENGINE,
            component = component.as_str(),
            field,
            entities = written,
            "[editor] selection field edit queued"
        );
        Ok(())
    }

    /// One scripted whole-field edit (`edit set <entity> <component>
    /// <field> <value>`): the same recorded `SetField` batch an inspector
    /// widget queues, parsed against the field's current kind.
    pub(super) fn scripted_edit_set(
        &mut self,
        entity_text: &str,
        component_text: &str,
        field: &str,
        value_text: &str,
    ) -> Result<(), String> {
        let (entity, component, current) =
            self.resolve_edit_target(entity_text, component_text, field)?;
        let recorded = inspector::field_write(entity, &component, field, &current, value_text)?;
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(recorded.commands, recorded.label, Some(recorded.coalesce));
        }
        Ok(())
    }

    /// Run `edit system <index> on|off` from the scripted channel: the same
    /// recorded, undoable toggle the Systems tab's state buttons queue.
    pub(super) fn scripted_edit_system(&mut self, index: usize, enabled: bool) {
        let (edit, label) = systems::system_toggle(index, enabled);
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(vec![edit], label, None);
        }
        info!(
            target: telemetry_target::ENGINE,
            index,
            enabled,
            "[editor] system edit queued"
        );
    }

    /// `editor.entity_create` from the scripted channel: the recorded
    /// one-empty-entity edit the Entities menu's New Entity queues.
    pub(super) fn scripted_entity_create(&mut self) {
        let (command, label) = entities::entity_create();
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(vec![command], label, None);
        }
        info!(
            target: telemetry_target::ENGINE,
            "[editor] new entity queued"
        );
    }

    /// `editor.entity_delete`: the selected entity's recorded delete (the
    /// destroy and the selection clear as one step), or a note when the
    /// selection holds no entity.
    pub(super) fn scripted_entity_delete(&mut self) {
        let (entity, asset) = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world
                .get_resource::<EditorSelection>()
                .map(|selection| (selection.primary(), selection.asset.clone()))
                .unwrap_or((None, None))
        };
        let Some(entity) = entity else {
            info!(
                target: telemetry_target::ENGINE,
                "[editor] delete entity skipped nothing selected"
            );
            return;
        };
        let (commands, label) = entities::entity_delete(entity, asset);
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(commands, label, None);
        }
        info!(
            target: telemetry_target::ENGINE,
            entity = %entity,
            "[editor] delete entity queued"
        );
    }

    /// Resolve a scripted handle name to its id: `x`/`y`/`z` name the
    /// active tool's axes, `xy`/`xz`/`yz` the translation planes.
    pub(super) fn scripted_gizmo_handle(&mut self, name: &str) -> Option<u32> {
        let settings = self.gizmo_settings();
        if let Some(axis) = parse_gizmo_axis(name) {
            return Some(gizmo::axis_handle(settings.mode, axis));
        }
        let (first, second) = match name {
            "xy" => (0, 1),
            "xz" => (0, 2),
            "yz" => (1, 2),
            _ => return None,
        };
        if settings.mode != GizmoMode::Translate {
            warn!(
                target: telemetry_target::ENGINE,
                mode = gizmo_mode_name(settings.mode),
                "[editor] plane handles belong to the translate tool"
            );
            return None;
        }
        Some(gizmo::plane_handle(first, second))
    }

    /// Finish the drag a `gizmo press` started, like a mouse release.
    pub(super) fn scripted_gizmo_release(&mut self) -> Result<(), String> {
        if self.gizmo_drag.is_none() {
            return Err("gizmo release needs a drag in flight".to_string());
        }
        self.synthetic_gizmo_press = None;
        self.end_gizmo_drag();
        Ok(())
    }

    /// `editor.tool_*` from the scripted channel: queue the same
    /// `SetGizmoTool` edit the toolbox rows and the viewport menu queue
    /// (applied, never recorded - a tool choice is a control, not an edit).
    pub(super) fn scripted_gizmo_tool(
        &mut self,
        mode: Option<GizmoMode>,
        space: Option<GizmoSpace>,
        snap: Option<bool>,
    ) {
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push(vec![EditorCommand::SetGizmoTool { mode, space, snap }]);
        }
    }

    /// Move one view's pointer and update the hovered handle - the scripted
    /// channel's stand-in for a real pointer move.
    pub(super) fn scripted_hover(&mut self, view: usize, x: f32, y: f32) -> Result<(), String> {
        if view >= self.views.len() {
            return Err(format!("there is no scene view {view}"));
        }
        self.scene_cursor[view] = Some((x, y));
        self.update_hover(view, (x, y));
        info!(
            target: telemetry_target::ENGINE,
            view,
            x,
            y,
            "[editor] hover injected"
        );
        Ok(())
    }

    /// Move the shell's own pointer, as a real move arrives.
    ///
    /// egui learns a pointer position only from an event, so a suite that
    /// wants a tooltip or a resize cursor moves the pointer here. The carried
    /// cursor keeps the position on later frames, exactly like a real move
    /// that stopped moving.
    pub(super) fn scripted_hover_ui(&mut self, x: f32, y: f32) {
        let position = pill_core::math::Vector2f::new(x, y);
        self.shell_cursor = Some(position);
        self.shell_events.push(InputEvent::CursorMoved { position });
        self.demand.input = true;
        info!(
            target: telemetry_target::ENGINE,
            x,
            y,
            "[editor] pointer hovered"
        );
    }

    /// Dock one more scene view, the Window menu's own request: the tab goes
    /// in exactly where the menu would put it, and the executable's activation
    /// path brings it to the front.
    pub(super) fn scripted_new_scene_view(&mut self) -> Result<(), String> {
        let next = {
            let world = self.host.host_mut().engine_mut().world_mut();
            world.get_resource::<EguiShellState>().map(|state| {
                (
                    state.dock.next_scene_view(),
                    state.dock.can_open_scene_view(),
                )
            })
        };
        // The shell not being attached yet is a moment, not a refused line:
        // the same command works once it is.
        let Some((tab, fits)) = next else {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] scene view needs the shell module's state"
            );
            return Ok(());
        };
        if !fits {
            return Err(format!(
                "the editor shows at most {} scene views",
                pill_editor_core::boundary::SCENE_VIEWS_MAX
            ));
        }
        self.activate_tab(tab);
        info!(
            target: telemetry_target::ENGINE,
            tab = tab.name().as_str(),
            view = tab.scene_view().unwrap_or_default(),
            "[editor] scene view added"
        );
        Ok(())
    }

    /// Run `palette <query>` / `palette run` / `palette close` from the
    /// scripted channel.
    ///
    /// This touches the palette's model state only; the shell's pass draws the
    /// popup and runs the selected row, so a scripted run and a click take the
    /// same action path.
    pub(super) fn scripted_palette(&mut self, request: PaletteRequest) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(state) = world.get_resource_mut::<EguiShellState>() else {
            return;
        };
        match request {
            PaletteRequest::Query(query) => {
                state.palette.open = true;
                state.palette.query = query;
                state.palette.selected = 0;
                state.palette.logged_query = None;
                info!(
                    target: telemetry_target::ENGINE,
                    query = state.palette.query.as_str(),
                    "[editor] palette opened"
                );
            }
            PaletteRequest::Run => state.palette.run_requested = true,
            PaletteRequest::Close => {
                state.palette.open = false;
                state.palette.run_requested = false;
                info!(
                    target: telemetry_target::ENGINE,
                    "[editor] palette dismissed"
                );
            }
        }
    }

    /// Queue the component-field write a module panel's own row would queue
    /// (`panel field <component> <field> <value>`).
    ///
    /// The row is a drag value the test channel cannot press, so the write is
    /// injected where the row puts it: the shared `PanelCommands` queue, applied
    /// by [`Self::apply_panel_commands`] at the next frame boundary. What a test
    /// drives is therefore the panel contract itself - the queue, the recorded
    /// field edit and its undo step - not a second write path.
    pub(super) fn scripted_panel_field(
        &mut self,
        component: &str,
        field: &str,
        value: &str,
    ) -> Result<(), String> {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(entity) = world
            .get_resource::<EditorSelection>()
            .and_then(|selection| selection.primary())
        else {
            return Err("panel field needs a selected entity".to_string());
        };
        let mut queued = false;
        if let Some(commands) = world.get_resource_mut::<PanelCommands>() {
            commands.push(PanelCommand::SetField {
                entity,
                component: component.to_string(),
                field: field.to_string(),
                value: value.to_string(),
            });
            queued = true;
        }
        if queued {
            info!(
                target: telemetry_target::ENGINE,
                entity = %entity,
                component,
                field,
                value,
                "[editor] panel field queued"
            );
        }
        Ok(())
    }

    /// `panel restore <source>`: bind that module's panels again.
    ///
    /// The requests the executable kept are registered against a fresh marker,
    /// which is what a reload's re-registration does.
    pub(super) fn scripted_panel_restore(&mut self, source: &str) {
        if self.panel_markers.contains_key(source) {
            info!(
                target: telemetry_target::ENGINE,
                source,
                restored = 0,
                "[editor] panel source restored"
            );
            return;
        }
        let marker = ModuleLiveness::new();
        let kept: Vec<PanelRequest> = self
            .panel_requests
            .iter()
            .copied()
            .filter(|request| request.source == source)
            .collect();
        let world = self.host.host_mut().engine_mut().world_mut();
        let mut restored = 0;
        if let Some(registry) = world.get_resource_mut::<PanelRegistry>() {
            for request in &kept {
                registry.register(
                    request.id,
                    request.title,
                    request.source,
                    request.draw,
                    &marker,
                );
                restored += 1;
            }
        }
        if restored > 0 {
            self.panel_markers.insert(source.to_string(), marker);
        }
        info!(
            target: telemetry_target::ENGINE,
            source,
            restored,
            "[editor] panel source restored"
        );
    }

    /// `panel unload <source>`: drop one module's liveness marker.
    ///
    /// This is the state a module's unload leaves the registry in - the marker
    /// is what the unload would drop - so the tab reads "module not loaded"
    /// and the panel's draw function is no longer handed out.
    pub(super) fn scripted_panel_unload(&mut self, source: &str) {
        let dropped = self.panel_markers.remove(source).is_some();
        info!(
            target: telemetry_target::ENGINE,
            source,
            dropped,
            "[editor] panel source unloaded"
        );
    }

    /// `panel window <slot> <id>`: ask a panel to open one of its windows.
    ///
    /// The panel decides what the id means, so this drives the same code its
    /// button does.
    pub(super) fn scripted_panel_window(&mut self, slot: u16, id: &str) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(state) = world.get_resource_mut::<EguiShellState>() else {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] panel window request needs the shell module's state"
            );
            return;
        };
        state.panel_window_ping = Some((slot, id.to_string()));
        info!(
            target: telemetry_target::ENGINE,
            slot,
            window = id,
            "[editor] panel window requested"
        );
    }

    /// Run `editor.pause_toggle` from the scripted channel: queue the flip
    /// of the engine's pause flag - applied, never recorded, the same walk
    /// the Systems tab's Pause/Resume button takes.
    pub(super) fn scripted_pause_toggle(&mut self) {
        let paused = self.host.host_mut().engine_mut().systems_paused();
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push(vec![systems::pause_toggle_command(paused)]);
        }
        info!(
            target: telemetry_target::ENGINE,
            paused = !paused,
            "[editor] pause toggled"
        );
    }

    /// Arm an entity reference row (`pick entity <entity> <component>
    /// <field>`): the same `ArmEntityPick` the row's target button queues, so
    /// the next scene press names the field's reference.
    pub(super) fn scripted_pick_entity(
        &mut self,
        entity_text: &str,
        component_text: &str,
        field: &str,
    ) -> Result<(), String> {
        let (entity, component, current) =
            self.resolve_edit_target(entity_text, component_text, field)?;
        if !matches!(current, FieldValue::Entity { .. }) {
            return Err(format!("'{field}' does not hold an entity reference"));
        }
        let command = EditorCommand::ArmEntityPick {
            entity,
            component,
            field: field.to_owned(),
        };
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push(vec![command]);
        }
        Ok(())
    }

    /// `popout <tab>` / `popout <tab> redock`: the same dock
    /// move the tab menu makes, driven from the scripted channel.
    pub(super) fn scripted_popout(&mut self, tab: EditorTab, redock: bool) -> Result<(), String> {
        if tab == EditorTab::Game {
            return Err("the game view is the primary surface's window".to_string());
        }
        let world = self.host.host_mut().engine_mut().world_mut();
        // The shell not being attached yet is a moment, not a refused line.
        let Some(state) = world.get_resource_mut::<EguiShellState>() else {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] popout needs the shell module's state"
            );
            return Ok(());
        };
        let moved = if redock {
            state.dock.redock(tab)
        } else {
            state.dock.pop_out(tab)
        };
        if moved {
            info!(
                target: telemetry_target::ENGINE,
                tab = tab.name().as_str(),
                redock,
                "[editor] popout moved"
            );
        } else {
            return Err(format!(
                "the {} tab is already {}",
                tab.name(),
                if redock { "docked" } else { "out" }
            ));
        }
        Ok(())
    }

    /// Run `problem open <index>`: the same request a click on the row makes.
    ///
    /// The request is drained at once rather than on the next frame: a suite
    /// that sends this line waits for the editor it launches, and one frame of
    /// delay would make the assertion race the spawn.
    pub(super) fn scripted_problem_open(&mut self, index: usize) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let found = world
            .get_resource_mut::<EguiShellState>()
            .map(|state| {
                state.problem_open = Some(index);
            })
            .is_some();
        info!(
            target: telemetry_target::ENGINE,
            index,
            found,
            "[editor] problem open scripted"
        );
        self.drain_problem_open();
    }

    /// Run `editor.redo` from the scripted channel: the same walk, queueing
    /// the next step's forward batch.
    pub(super) fn scripted_redo(&mut self) {
        drain::step_history(self.host.host_mut().engine_mut(), HistoryDirection::Redo);
    }

    /// `scene new|open|save` from the scripted channel: the same command the
    /// File menu queues, applied at the next frame boundary.
    ///
    /// Document commands are not recorded - they replace the world rather than
    /// edit it - so the batch is queued unrecorded.
    pub(super) fn scripted_scene(&mut self, command: EditorCommand) {
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push(vec![command]);
        }
    }

    /// The scripted asset selection: the same shared resource an Assets row
    /// click writes, logged so a suite can watch either path.
    pub(super) fn scripted_select_asset(&mut self, path: &str) {
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(selection) = world.get_resource_mut::<EditorSelection>() {
            selection.asset = Some(path.to_string());
        }
        info!(
            target: telemetry_target::ENGINE,
            path,
            "[editor] asset selected"
        );
    }

    /// The scripted entity selection: the same recorded step the Entities
    /// tab's row click queues, resolved against the live entity table.
    ///
    /// A screen-space click cannot re-select an entity a gizmo drag moved;
    /// the handle a pick logged still resolves here.
    pub(super) fn scripted_select_entity(&mut self, handle: &str) -> Result<(), String> {
        // A plain click is the reset shape of the same rule, so the two scripted
        // verbs and the pointer path share one path.
        self.scripted_select_extend(handle, SelectionClick::Reset)
    }

    /// The scripted selection extension (`select add` / `select range`): the
    /// set a Ctrl- or Shift-click on the Entities list produces, recorded as
    /// the same single `SetSelection` step the row click queues.
    ///
    /// The click rule itself lives in the core's `EditorSelection::clicked`, so
    /// the pointer path and this channel cannot disagree about what a click
    /// means; what the modifiers do is the shell's `click_from_modifiers`.
    pub(super) fn scripted_select_extend(
        &mut self,
        handle: &str,
        click: SelectionClick,
    ) -> Result<(), String> {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(entity) = resolve_display_entity(world, handle) else {
            return Err(format!("no entity named '{handle}'"));
        };
        // The Entities list's row order, which a range click reads.
        let rows: Vec<Entity> = world
            .get_resource::<EditorSnapshot>()
            .map(|snapshot| snapshot.entities.iter().map(|row| row.entity).collect())
            .unwrap_or_default();
        let Some(selection) = world.get_resource::<EditorSelection>() else {
            return Ok(());
        };
        let entities = selection.clicked(click, entity, &rows);
        let asset = selection.asset.clone();
        let count = entities.len();
        let label = match click {
            SelectionClick::Reset if count <= 1 => "select entity",
            _ => "select entities",
        };
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(
                vec![EditorCommand::SetSelection { entities, asset }],
                label,
                Some("editor.selection".to_string()),
            );
        }
        info!(
            target: telemetry_target::ENGINE,
            entity = %entity,
            count,
            "[editor] selection changed"
        );
        Ok(())
    }

    /// `select resource <name>`: the recorded selection a resource row makes.
    pub(super) fn scripted_select_resource(&mut self, name: &str) {
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() else {
            return;
        };
        pending.push_recorded(
            vec![EditorCommand::SelectResource {
                resource: Some(name.to_string()),
            }],
            format!("select resource {name}"),
            None,
        );
        info!(
            target: telemetry_target::ENGINE,
            resource = name,
            "[editor] resource selection queued"
        );
    }

    /// Run `editor.step` from the scripted channel: queue one single step -
    /// applied, never recorded, the same walk the Step button takes.
    pub(super) fn scripted_step(&mut self) {
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push(vec![EditorCommand::StepOnce]);
        }
        info!(target: telemetry_target::ENGINE, "[editor] step queued");
    }

    /// One Assets toolbar button, driven by the scripted channel: the same
    /// core helpers the buttons call, enablement rules included.
    pub(super) fn scripted_toolbar(&mut self, action: ToolbarAction) {
        // The rows, the types and the selection the helpers read, cloned
        // out first: the world's mutable resources are borrowed below.
        let (rows, types, selected) = {
            let world = self.host.host_mut().engine_mut().world_mut();
            let (rows, types) = match world.get_resource::<EditorSnapshot>() {
                Some(snapshot) => (snapshot.assets.clone(), snapshot.standalone_types.clone()),
                None => (Vec::new(), Vec::new()),
            };
            let selected = world
                .get_resource::<EditorSelection>()
                .and_then(|selection| selection.asset.clone());
            (rows, types, selected)
        };
        // A view of the selection for the shared asset helpers: only the asset
        // half is theirs, and the entity set is empty because nothing here
        // acts on entities.
        let mut selection = EditorSelection::default();
        selection.asset = selected;
        match action {
            ToolbarAction::Create => {
                let world = self.host.host_mut().engine_mut().world_mut();
                if let Some(state) = world.get_resource_mut::<EguiShellState>() {
                    assets::open_create(&mut state.asset_dialog, &selection, &rows, &types);
                }
            }
            ToolbarAction::Rename => {
                let world = self.host.host_mut().engine_mut().world_mut();
                if let Some(state) = world.get_resource_mut::<EguiShellState>() {
                    assets::open_rename(&mut state.asset_dialog, &selection, &rows);
                }
            }
            ToolbarAction::Delete => {
                let world = self.host.host_mut().engine_mut().world_mut();
                if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
                    assets::delete_selection(pending, &selection, &rows);
                }
            }
            ToolbarAction::Meta => {
                let world = self.host.host_mut().engine_mut().world_mut();
                if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
                    assets::write_meta_selection(pending, &selection, &rows);
                }
            }
        }
    }

    /// Run `editor.undo` from the scripted channel: the shared core walk pops
    /// the newest step and queues its inverse - applied, never recorded, the
    /// same walk the browser editor and the native editor take.
    pub(super) fn scripted_undo(&mut self) {
        drain::step_history(self.host.host_mut().engine_mut(), HistoryDirection::Undo);
    }

    /// Queue a scripted left-button drag: the press, the move
    /// and the release arrive one per frame through
    /// [`Self::advance_scripted_drag`], the shape egui's drag detection
    /// needs. The same engine input seam a real drag enters, so a panel's
    /// drag handle reacts exactly as it does for a user.
    pub(super) fn start_scripted_drag(
        &mut self,
        from: (f32, f32),
        to: (f32, f32),
    ) -> Result<(), String> {
        if self.scripted_drag.is_some() {
            return Err("a scripted drag is already in flight".to_string());
        }
        self.scripted_drag = Some(ScriptedDrag {
            from,
            to,
            stage: ScriptedDragStage::PressPending,
        });
        info!(
            target: telemetry_target::ENGINE,
            from_x = from.0,
            from_y = from.1,
            to_x = to.0,
            to_y = to.1,
            "[editor] pointer drag queued"
        );
        Ok(())
    }
}
