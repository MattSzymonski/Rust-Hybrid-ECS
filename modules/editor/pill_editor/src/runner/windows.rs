//! The editor's windows: surfaces, child windows, popouts and presentation.
//!
//! # Responsibilities
//!
//! - Create and place each scene view's child window and the game view's, and
//!   keep every renderer surface bound to the window that shows it.
//! - Run each popped-out tab's own egui pass and present it.
//! - Present one editor frame: the composition overlay when it exists, the
//!   window-surface posture when it does not, and the fallback UI when no
//!   shell module is loaded.
//!
//! # Design
//!
//! A surface lives and dies with its window, so the two are handled together:
//! attaching, placing and hiding one is one method's job. The composition
//! overlay is best-effort by design - a platform that refuses it leaves the
//! window-surface posture, which is why every overlay step is retried on a
//! cadence rather than treated as fatal.

// Current crate
use super::*;

impl EditorWindow {
    /// Handle a main-window resize.
    pub(super) fn resize(&mut self, width: u32, height: u32) {
        self.size = (width.max(1), height.max(1));
        // The primary surface lives on the game view's window when it
        // exists; only the fallback posture sizes it from the main window.
        if self.game_surface.is_none() {
            self.host
                .resize_surface(SurfaceId::PRIMARY, width.max(1), height.max(1));
        }
        if let Some(overlay) = &mut self.overlay {
            overlay.resize(&self.device, self.size);
        }
        self.place_views();
    }

    /// One scene view's client-area origin in the main window's client
    /// coordinates, in physical pixels: the position its last placement
    /// applied. A resize never moves the window, so the origin stands even
    /// while the placement cache is cleared mid-layout; a drag across the
    /// separator therefore never sees the pointer jump.
    pub(super) fn view_origin(&self, view: usize) -> (f32, f32) {
        match &self.views[view] {
            Some(scene) => (scene.window.origin.0 as f32, scene.window.origin.1 as f32),
            None => (0.0, 0.0),
        }
    }

    /// Handle a scene window's resize: the view's surface reconfigures with
    /// its window - a stale render target draws the scene at the wrong size
    /// inside the tab. The placement cache is cleared, so the next layout pass
    /// re-applies the dock's rectangle.
    pub(super) fn resize_scene(&mut self, view: usize, width: u32, height: u32) {
        let Some(scene) = &mut self.views[view] else {
            return;
        };
        scene.window.placed = None;
        let (width, height) = (width.max(1), height.max(1));
        self.host.resize_surface(scene.surface_id, width, height);
        info!(
            target: telemetry_target::ENGINE,
            width,
            height,
            view,
            "[editor] scene surface resized"
        );
    }

    /// Handle the game window's resize: the primary surface reconfigures
    /// with its window, like the scene view surfaces - a stale render
    /// target draws the project at the wrong size inside the Game tab. The
    /// placement cache is cleared, so the next layout pass re-applies the
    /// dock's rectangle.
    pub(super) fn resize_game(&mut self, width: u32, height: u32) {
        let Some(game) = &mut self.game_surface else {
            return;
        };
        game.placed = None;
        let (width, height) = (width.max(1), height.max(1));
        self.host.resize_surface(SurfaceId::PRIMARY, width, height);
        info!(
            target: telemetry_target::ENGINE,
            width,
            height,
            "[editor] game surface resized"
        );
    }

    /// The game window's client-area origin in the main window's client
    /// coordinates, in physical pixels: the same bridge the scene views
    /// use for pointer positions.
    pub(super) fn game_origin(&self) -> (f32, f32) {
        match &self.game_surface {
            Some(game) => (game.origin.0 as f32, game.origin.1 as f32),
            None => (0.0, 0.0),
        }
    }

    /// Open the scene views the dock reports that this editor does not hold
    /// yet.
    ///
    /// The shell reports one slot per view index the dock carries, so a view
    /// the person just added is the list's last slot. Each new view gets its
    /// own window, surface and camera here - the same three the startup views
    /// get - and starts hidden: the placement that follows this frame shows
    /// it.
    pub(super) fn ensure_scene_views(&mut self, count: usize, event_loop: &ActiveEventLoop) {
        let wanted = count.min(pill_editor_core::boundary::SCENE_VIEWS_MAX);
        while self.views.len() < wanted {
            let view = self.views.len();
            self.cameras.push(OrbitCamera::default());
            self.left_down.push(false);
            self.camera_dirty.push(true);
            self.scene_cursor.push(None);
            let opened = SceneView::open(event_loop, &self.window, view);
            if let Some(mut scene) = opened {
                let size = scene.window.size();
                scene.attach(&mut self.host, &self.cameras[view], size);
                info!(
                    target: telemetry_target::ENGINE,
                    view,
                    surface = scene.surface_id.0,
                    "[editor] scene view opened"
                );
                self.views.push(Some(scene));
            } else {
                // A view without a window keeps its slots: the dock still
                // holds its tab, and the tab shows the UI's chips alone.
                self.views.push(None);
            }
        }
    }

    /// Place the first scene view at the dock's default layout's center
    /// inside the client area, until the shell's first scene rect arrives;
    /// the other views stay hidden until their tabs show.
    pub(super) fn place_views(&mut self) {
        let Some(Some(scene)) = self.views.first_mut() else {
            return;
        };
        let (width, height) = (self.size.0 as i32, self.size.1 as i32);
        let margin = (SCENE_DEFAULT_MARGIN * self.scale_factor as f64) as i32;
        let side = (width as f64 * SIDE_COLUMN_FRACTION as f64) as i32;
        let top = (SCENE_DEFAULT_TOP_MARGIN * self.scale_factor as f64) as i32 + margin;
        let bottom = (height as f64 * BOTTOM_ROW_FRACTION as f64) as i32;
        let left = side + margin;
        let width = (width - 2 * side - 2 * margin).max(1) as u32;
        let height = (height - top - bottom - margin).max(1) as u32;
        scene.window.place(left, top, width, height);
    }

    /// Give a popped-out scene view its top-level window, placed at the
    /// pop-out's screen position. The first call replaces its
    /// child window with a top-level one - a child can never reach another
    /// monitor - and later calls just follow the pop-out as it moves.
    pub(super) fn pop_view_window(
        &mut self,
        event_loop: &ActiveEventLoop,
        view: usize,
        x: i32,
        y: i32,
        width: u32,
        height: u32,
    ) {
        let needs_window = self
            .views
            .get(view)
            .and_then(|scene| scene.as_ref())
            .is_some_and(|scene| !scene.popped);
        if needs_window {
            let title = format!("Pill Editor Scene {} (popped out)", view + 1);
            match SurfaceWindow::open_top_level(event_loop, &title) {
                Ok(window) => {
                    let host = &mut self.host;
                    if let Some(Some(scene)) = self.views.get_mut(view) {
                        scene.rebind_window(host, window, true);
                    }
                }
                Err(error) => {
                    warn!(
                        target: telemetry_target::ENGINE,
                        view,
                        error = %error,
                        "[editor] popped-out scene window refused"
                    );
                    return;
                }
            }
        }
        if let Some(Some(scene)) = self.views.get_mut(view) {
            scene.window.place_screen(x, y, width, height);
        }
    }

    /// Give a scene view its child window back when its tab returns to the
    /// dock. A view the platform forced to a detached window is left alone:
    /// that window is its dock window, not a pop-out's.
    pub(super) fn ensure_view_docked(&mut self, event_loop: &ActiveEventLoop, view: usize) {
        let popped = self
            .views
            .get(view)
            .and_then(|scene| scene.as_ref())
            .is_some_and(|scene| scene.popped);
        if !popped {
            return;
        }
        let title = format!("Pill Editor Scene {}", view + 1);
        match SurfaceWindow::open(event_loop, &self.window, &title, "scene") {
            Ok(window) => {
                let host = &mut self.host;
                if let Some(Some(scene)) = self.views.get_mut(view) {
                    scene.rebind_window(host, window, false);
                }
            }
            Err(error) => warn!(
                target: telemetry_target::ENGINE,
                view,
                error = %error,
                "[editor] scene view redock refused"
            ),
        }
    }

    /// Place one scene view at the dock's scene rectangle, in logical
    /// points, converted to the client area's physical pixels.
    pub(super) fn place_view_rect(&mut self, view: usize, rect: ShellRect) {
        let (x, y, width, height) = self.view_rect_pixels(rect);
        let Some(scene) = &mut self.views[view] else {
            return;
        };
        scene.window.place(x, y, width, height);
    }

    /// Place the game window at the dock's game rectangle, in logical
    /// points, converted to the client area's physical pixels.
    pub(super) fn place_game_rect(&mut self, rect: ShellRect) {
        let (x, y, width, height) = self.view_rect_pixels(rect);
        let Some(game) = &mut self.game_surface else {
            return;
        };
        game.place(x, y, width, height);
    }

    /// A dock rectangle in logical points as the client area's physical
    /// pixels, clamped inside it: the dock's rect can overshoot the window
    /// while a split is being dragged.
    pub(super) fn view_rect_pixels(&self, rect: ShellRect) -> (i32, i32, u32, u32) {
        scaled_view_rect(rect, self.scale_factor, self.size)
    }

    /// Hide one scene view's window: its tab is not shown this pass
    /// (another tab of its group is active, or the tab was closed).
    pub(super) fn hide_view(&mut self, view: usize) {
        if let Some(scene) = &mut self.views[view] {
            scene.window.hide();
        }
    }

    /// Hide the game window: its tab is not shown this pass, so no child
    /// window may paint over the tab that is.
    pub(super) fn hide_game(&mut self) {
        if let Some(game) = &mut self.game_surface {
            game.hide();
        }
    }

    /// Route one scene window's event: activate the viewport on any press,
    /// start camera navigation on its chord, start a gizmo drag or ask a
    /// pick on a left press, feed a drag in flight its moves, and end what
    /// the release ends.
    ///
    /// The viewport that owns an interaction keeps it: winit reports a
    /// drag's moves to the window whose press started it even when the
    /// pointer leaves it, so a gizmo drag or a navigation that crosses the
    /// viewport's edge carries on and no other viewport takes the pointer
    /// mid-drag.
    pub(super) fn scene_window_event(&mut self, view: usize, event: &WindowEvent) {
        match event {
            WindowEvent::CursorMoved { position, .. } => {
                let point = (position.x as f32, position.y as f32);
                self.scene_cursor[view] = Some(point);
                if let Some(drag) = &self.camera_drag {
                    if drag.view == view {
                        self.update_camera_navigation(point);
                        return;
                    }
                }
                if let Some(drag) = &self.gizmo_drag {
                    if drag.view == view {
                        self.update_gizmo_drag(point.0, point.1);
                    }
                    return;
                }
                self.update_hover(view, point);
            }
            WindowEvent::MouseInput { state, button, .. } => {
                let pressed = *state == winit::event::ElementState::Pressed;
                let button = match button {
                    winit::event::MouseButton::Left => CameraButton::Left,
                    winit::event::MouseButton::Middle => CameraButton::Middle,
                    winit::event::MouseButton::Right => CameraButton::Right,
                    _ => return,
                };
                if button == CameraButton::Left {
                    self.left_down[view] = pressed;
                }
                if !pressed {
                    if button == CameraButton::Left {
                        self.release_scene(view);
                    }
                    if self
                        .camera_drag
                        .as_ref()
                        .is_some_and(|drag| drag.view == view)
                    {
                        self.end_camera_navigation();
                    }
                    return;
                }
                // A press over the overlay's chips is the UI's: no activation
                // and no scene interaction.
                if self.scene_point_is_ui(view) {
                    return;
                }
                self.activate_view(view);
                let Some(point) = self.scene_cursor[view] else {
                    return;
                };
                if let Some(navigation) = self.navigation_for(button) {
                    self.begin_camera_navigation(view, navigation, point);
                    return;
                }
                if button == CameraButton::Left {
                    self.press_scene(view, point.0, point.1);
                }
            }
            WindowEvent::MouseWheel { delta, .. } => {
                let lines = match delta {
                    winit::event::MouseScrollDelta::LineDelta(_, lines) => *lines,
                    winit::event::MouseScrollDelta::PixelDelta(position) => {
                        position.y as f32 / 40.0
                    }
                };
                if lines == 0.0 {
                    return;
                }
                // The wheel zooms the viewport it arrived over without
                // taking the active slot: a click is what activates.
                self.zoom_camera(view, lines);
            }
            _ => {}
        }
    }

    /// Make `view` the active viewport: the one camera navigation, the
    /// gizmo's handle length and the viewport commands address. Only a
    /// press inside a scene view reaches here - a click on the editor's own
    /// UI never activates a viewport.
    pub(super) fn activate_view(&mut self, view: usize) {
        if self.active_view == view {
            return;
        }
        self.active_view = view;
        info!(
            target: telemetry_target::ENGINE,
            view,
            "[editor] active viewport"
        );
    }

    /// Reconcile the popped-out windows with the shell's frames and paint
    /// them.
    ///
    /// The shell reports one frame per popped-out tab, so the frames *are* the
    /// window set: a tab that appears gets a window, a tab that disappears has
    /// its window closed, and each remaining frame is painted into its own
    /// swapchain through its own renderer. The pass ran in the shell, so
    /// nothing here touches egui's context.
    pub(super) fn present_popouts(
        &mut self,
        event_loop: &ActiveEventLoop,
        frames: &mut [pill_editor_core::boundary::PopoutFrame],
    ) {
        for frame in frames.iter() {
            if self.popouts.contains_key(&frame.tab) {
                continue;
            }
            self.open_popout(event_loop, &frame.tab, &frame.title);
        }
        // A tab that redocked (or was closed) loses its window.
        let live: Vec<String> = frames.iter().map(|frame| frame.tab.clone()).collect();
        let stale: Vec<String> = self
            .popouts
            .keys()
            .filter(|tab| !live.contains(tab))
            .cloned()
            .collect();
        for tab in stale {
            if self.popouts.remove(&tab).is_some() {
                info!(
                    target: telemetry_target::ENGINE,
                    tab = tab.as_str(),
                    "[editor] popout closed"
                );
            }
        }
        for frame in frames.iter() {
            let Some(popout) = self.popouts.get(&frame.tab) else {
                continue;
            };
            // A popped-out Scene tab reports its viewport in the pop-out's own
            // coordinates; the view's window follows it onto that window's
            // screen position, which is what lets it cross monitors.
            if frame.frame.scene_rects.iter().all(|rect| rect.is_none()) {
                continue;
            }
            let scale_factor = popout.window.scale_factor() as f32;
            let origin = popout
                .window
                .inner_position()
                .unwrap_or(PhysicalPosition::new(0, 0));
            let placements: Vec<(usize, i32, i32, u32, u32)> = frame
                .frame
                .scene_rects
                .iter()
                .enumerate()
                .filter_map(|(view, rect)| {
                    let rect = (*rect)?;
                    Some((
                        view,
                        origin.x + (rect.x * scale_factor) as i32,
                        origin.y + (rect.y * scale_factor) as i32,
                        (rect.width * scale_factor).max(1.0) as u32,
                        (rect.height * scale_factor).max(1.0) as u32,
                    ))
                })
                .collect();
            for (view, x, y, width, height) in placements {
                self.pop_view_window(event_loop, view, x, y, width, height);
            }
        }
        for frame in frames.iter_mut() {
            let Some(popout) = self.popouts.get_mut(&frame.tab) else {
                continue;
            };
            let textures = std::mem::take(&mut frame.frame.textures);
            match popout.surface.render(
                &self.device,
                &self.queue,
                &mut popout.renderer,
                &frame.frame.meshes,
                textures,
                frame.frame.pixels_per_point,
            ) {
                Ok(drew) => {
                    // The suite's observation point that the window is live,
                    // logged once so a broken painter shows up as a missing
                    // line rather than a silently painted-over window.
                    if drew && !popout.presented {
                        popout.presented = true;
                        info!(
                            target: telemetry_target::ENGINE,
                            tab = frame.tab.as_str(),
                            "[editor] popout presented"
                        );
                    }
                }
                Err(error) => warn!(
                    target: telemetry_target::ENGINE,
                    tab = frame.tab.as_str(),
                    error = %error,
                    "[editor] popout frame failed"
                ),
            }
        }
    }

    /// Open one popped-out tab's window: a plain top-level window with its own
    /// swapchain and its own egui renderer.
    pub(super) fn open_popout(&mut self, event_loop: &ActiveEventLoop, tab: &str, title: &str) {
        let attributes = Window::default_attributes()
            .with_title(title)
            .with_inner_size(POPOUT_INITIAL_SIZE);
        let window = match event_loop.create_window(attributes) {
            Ok(window) => window,
            Err(error) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    tab,
                    error = %error,
                    "[editor] popout window refused"
                );
                return;
            }
        };
        let window = Arc::new(window);
        let size = window.inner_size();
        let surface = match OverlaySurface::new_window(
            &self.instance,
            &self.adapter,
            &self.device,
            Arc::clone(&window),
            EDITOR_FORMAT,
            (size.width.max(1), size.height.max(1)),
        ) {
            Ok(surface) => surface,
            Err(error) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    tab,
                    error = %error,
                    "[editor] popout surface refused"
                );
                return;
            }
        };
        let renderer = egui_wgpu::Renderer::new(
            &self.device,
            EDITOR_FORMAT,
            egui_wgpu::RendererOptions::default(),
        );
        info!(
            target: telemetry_target::ENGINE,
            tab,
            width = size.width,
            height = size.height,
            "[editor] popout opened"
        );
        self.popouts.insert(
            tab.to_string(),
            PopoutWindow {
                window,
                surface,
                renderer,
                presented: false,
            },
        );
    }

    /// Refresh each popped-out window's input record: its geometry and frame
    /// rate this frame. Its events were appended as its window reported them.
    pub(super) fn publish_popout_geometry(&mut self) {
        if self.popouts.is_empty() {
            return;
        }
        let geometry: Vec<(String, f32, f32, f32, bool)> = self
            .popouts
            .iter()
            .map(|(tab, popout)| {
                let scale_factor = popout.window.scale_factor() as f32;
                let size = popout.window.inner_size();
                (
                    tab.clone(),
                    size.width as f32 / scale_factor.max(0.01),
                    size.height as f32 / scale_factor.max(0.01),
                    scale_factor,
                    popout.window.has_focus(),
                )
            })
            .collect();
        let time = self.started.elapsed().as_secs_f64();
        let frames_per_second = self.fps;
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(inputs) = world.get_resource_mut::<EguiPopoutInputs>() else {
            return;
        };
        for (tab, width, height, scale_factor, focused) in geometry {
            let record = inputs.by_tab.entry(tab).or_default();
            record.screen_size = pill_core::math::Vector2f::new(width.max(1.0), height.max(1.0));
            record.scale_factor = scale_factor;
            record.focused = focused;
            record.time = time;
            record.frames_per_second = frames_per_second;
        }
    }

    /// Close `tab`'s window and redock its tab: a window close is a redock,
    /// so the tab is never lost by closing it.
    pub(super) fn close_popout(&mut self, tab: &str) {
        if self.popouts.remove(tab).is_none() {
            return;
        }
        // The tab comes back through the dock's own rule, matched by wire name
        // (the shell owns the tab enum; this side only knows the name).
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(state) = world.get_resource_mut::<EguiShellState>() {
            if let Some(editor_tab) = EditorTab::from_wire(tab) {
                state.dock.redock(editor_tab);
            }
        }
        info!(
            target: telemetry_target::ENGINE,
            tab,
            "[editor] popout closed"
        );
    }

    /// Append one event to `tab`'s input record, the window's own cursor kept
    /// alongside it.
    pub(super) fn popout_event(&mut self, tab: &str, event: InputEvent) {
        let cursor = match &event {
            InputEvent::CursorMoved { position } => Some(*position),
            InputEvent::CursorLeft => None,
            _ => None,
        };
        let world = self.host.host_mut().engine_mut().world_mut();
        let Some(inputs) = world.get_resource_mut::<EguiPopoutInputs>() else {
            return;
        };
        let record = inputs.by_tab.entry(tab.to_string()).or_default();
        match &event {
            InputEvent::CursorMoved { .. } => record.cursor = cursor,
            InputEvent::CursorLeft => record.cursor = None,
            _ => {}
        }
        record.events.push(event);
    }

    /// Clear every popped-out window's event list once the frame that read it
    /// has been presented, so no event is replayed into a second pass.
    pub(super) fn clear_popout_events(&mut self) {
        if self.popouts.is_empty() {
            return;
        }
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(inputs) = world.get_resource_mut::<EguiPopoutInputs>() {
            for record in inputs.by_tab.values_mut() {
                record.events.clear();
            }
        }
    }

    /// Draw the shell's frame through the overlay; without a shell module,
    /// draw the runner's own fallback.
    pub(super) fn present_ui(&mut self, event_loop: &ActiveEventLoop) {
        // Step 1: take the shell frame, falling back to the runner's own
        // minimal pass while no shell module is loaded.
        let frame = {
            let world = self.host.host_mut().engine_mut().world_mut();
            match world.get_resource_mut::<EguiShellFrame>() {
                Some(resource) => std::mem::take(&mut *resource),
                None => self.fallback_frame(),
            }
        };
        let EguiShellFrame {
            meshes,
            textures,
            pixels_per_point,
            wants_pointer,
            wants_keyboard,
            cursor_icon,
            clipboard_text,
            scene_rects,
            game_rect,
            overlay_regions,
            ime_allowed,
            ime_rect,
            drag_out,
            // The menu bar's quit is drained at the frame boundary (Step 7b),
            // not while the frame is presented.
            quit_requested: _,
            // egui's own cadence: the redraw decision at the end of the frame
            // honours it.
            repaint_after,
            // One frame per popped-out tab: each paints into
            // its own window, below.
            mut popout_frames,
        } = frame;
        self.repaint_after = repaint_after;
        // Each scene viewport follows its dock rectangle, so the viewports
        // track the layout when the user drags a splitter; a viewport whose
        // tab is not shown hides its window. The list is one slot per view
        // the dock holds, so a view the person just added opens here - on the
        // frame its tab first drew - and a view whose tab was closed keeps
        // its window hidden rather than dropped.
        self.ensure_scene_views(scene_rects.len(), event_loop);
        for view in 0..self.views.len() {
            match scene_rects.get(view).copied().flatten() {
                Some(rect) => {
                    // A view whose tab is back in the dock needs its child
                    // window again.
                    self.ensure_view_docked(event_loop, view);
                    self.place_view_rect(view, rect);
                }
                None => self.hide_view(view),
            }
        }
        // The game viewport follows its own rectangle the same way: the
        // primary surface's window sits where the Game tab's body is, and
        // disappears while another tab of its group is active.
        match game_rect {
            Some(rect) => self.place_game_rect(rect),
            None => self.hide_game(),
        }
        // The chips the shell drew over the viewports, kept for the scene
        // click routing: a click inside them is the UI's, not the scene's.
        self.overlay_regions = overlay_regions;
        // Step 2: the routing decision is visible in the log on change.
        if wants_pointer != self.claimed_pointer || wants_keyboard != self.claimed_keyboard {
            info!(
                target: telemetry_target::ENGINE,
                pointer = wants_pointer,
                keyboard = wants_keyboard,
                "[editor] egui input claims changed"
            );
            self.claimed_pointer = wants_pointer;
            self.claimed_keyboard = wants_keyboard;
        }
        self.apply_cursor_icon(cursor_icon);
        // An IME the shell asked for: the window takes the
        // candidate window, and the caret's rectangle places it. A text field
        // that loses focus turns it off again.
        self.apply_ime(ime_allowed, ime_rect);
        // A tile dragged past the window edge: the OS gets
        // the file through an OLE drag, and the editor's thread runs the modal
        // loop until the drop ends.
        if let Some(path) = drag_out {
            self.start_drag_out(&path);
        }
        if let Some(text) = clipboard_text {
            // egui's own copy (a text field's selection) leaves under the same
            // registered format the structured copy uses.
            match crate::clipboard::write(&text) {
                Ok(()) => info!(
                    target: telemetry_target::ENGINE,
                    bytes = text.len(),
                    "[editor] clipboard written"
                ),
                Err(message) => warn!(
                    target: telemetry_target::ENGINE,
                    message = message.as_str(),
                    "[editor] clipboard write failed"
                ),
            }
        }
        // Step 2b: the popped-out windows - create the ones
        // the shell reported for the first time, close the ones it stopped
        // reporting, and paint each frame into its own swapchain.
        self.present_popouts(event_loop, &mut popout_frames);
        // Step 3: draw through the overlay; retry a missing overlay rarely,
        // so a transient compositor failure heals.
        if self.overlay.is_none() {
            self.retry_overlay();
        }
        let Some(overlay) = &mut self.overlay else {
            return;
        };
        if let Err(error) = overlay.render(
            &self.device,
            &self.queue,
            &mut self.egui_renderer,
            &meshes,
            textures,
            pixels_per_point,
        ) {
            warn!(
                target: telemetry_target::ENGINE,
                error = %error,
                "[editor] overlay frame failed"
            );
        }
        // The popped-out windows' events have been through their passes: the
        // next frame starts with none.
        self.clear_popout_events();
    }

    /// The safe-mode pass, used while no shell module is loaded: the build
    /// errors that stopped it, with their `file:line`, and a Retry that asks
    /// the host to rebuild the module. The scene keeps rendering behind this
    /// panel - the editor's own frame is untouched.
    pub(super) fn fallback_frame(&mut self) -> EguiShellFrame {
        // The fallback paints the same theme the shell would: a layer change
        // reaches this posture too, and a theme change is a reason to draw
        // once.
        if self.fallback_theme_generation != self.theme_generation {
            self.fallback_theme_generation = self.theme_generation;
            pill_editor_core::style::apply_theme(
                &self.fallback_context,
                &self.theme,
                &self.theme_layers,
                self.theme_generation,
                &self.theme_fonts,
            );
        }
        if !self.reported_fallback {
            self.reported_fallback = true;
            info!(
                target: telemetry_target::ENGINE,
                module = SHELL_MODULE_NAME,
                problems = self.notices.problems.len(),
                "[editor] safe mode"
            );
        }
        let logical_width = self.size.0 as f32 / self.scale_factor;
        let logical_height = self.size.1 as f32 / self.scale_factor;
        let raw_input = egui::RawInput {
            screen_rect: Some(egui::Rect::from_min_size(
                egui::Pos2::ZERO,
                egui::vec2(logical_width, logical_height),
            )),
            ..Default::default()
        };
        let context = self.fallback_context.clone();
        let mut retry = false;
        let problems: Vec<(String, String, String)> = self
            .notices
            .problems
            .iter()
            .map(|row| {
                let location = match (&row.problem.file, row.problem.line) {
                    (Some(file), Some(line)) => format!("{file}:{line}"),
                    (Some(file), None) => file.clone(),
                    _ => "-".to_string(),
                };
                (
                    row.module.clone(),
                    location,
                    format!("{} {}", row.problem.severity.name(), row.problem.message),
                )
            })
            .collect();
        let output = context.run_ui(raw_input, |ui| {
            egui::CentralPanel::default().show(ui, |ui| {
                ui.heading("Pill Editor - safe mode");
                ui.label(format!(
                    "the shell module ({SHELL_MODULE_NAME}) is not loaded; the scene \
                     keeps rendering behind this panel"
                ));
                ui.separator();
                if problems.is_empty() {
                    ui.weak("no build error was reported");
                } else {
                    egui::ScrollArea::vertical()
                        .max_height(360.0)
                        .auto_shrink([false, true])
                        .show(ui, |ui| {
                            for (module, location, text) in &problems {
                                ui.label(egui::RichText::new(module).strong());
                                ui.label(
                                    egui::RichText::new(format!("  {location}  {text}"))
                                        .monospace(),
                                );
                            }
                        });
                }
                ui.separator();
                if ui
                    .add(egui::Button::new("Retry"))
                    .on_hover_text("rebuild and load the shell module")
                    .clicked()
                {
                    retry = true;
                }
            });
        });
        if retry {
            self.retry_shell_module();
        }
        frame_from_output(&context, output)
    }

    /// Try to create the composition overlay again, at most every 120 frames.
    ///
    /// The window posture is not retried: it was either asked for
    /// (`PILL_EDITOR_NO_OVERLAY`) or already taken as the fallback, and a
    /// composition overlay is the posture that failed.
    pub(super) fn retry_overlay(&mut self) {
        if self.no_overlay {
            return;
        }
        if self.frame_index < self.overlay_retry_frame + 120 {
            return;
        }
        self.overlay_retry_frame = self.frame_index;
        match OverlaySurface::new(
            &self.instance,
            &self.adapter,
            &self.device,
            Arc::clone(&self.window),
            EDITOR_FORMAT,
            self.size,
        ) {
            Ok(overlay) => {
                info!(
                    target: telemetry_target::ENGINE,
                    "[editor] composition overlay created"
                );
                self.overlay = Some(overlay);
            }
            Err(error) => {
                warn!(
                    target: telemetry_target::ENGINE,
                    error = %error,
                    "[editor] composition overlay still unavailable"
                );
            }
        }
    }

    /// Ask the host to rebuild the shell module, and say how it went.
    ///
    /// `retry_extension` runs the same load a first start would, or asks a
    /// loaded generation to rebuild as if its sources were saved, so a fix
    /// made while the editor was in safe mode lands without a restart.
    pub(super) fn retry_shell_module(&mut self) {
        let outcome = self.host.host_mut().retry_extension(SHELL_MODULE_NAME);
        match outcome {
            Ok(()) => {
                info!(
                    target: telemetry_target::ENGINE,
                    module = SHELL_MODULE_NAME,
                    "[editor] safe mode retry requested"
                );
                // The next frame looks the shell up again; a generation that
                // loaded in the meantime takes over without another step.
                self.reported_fallback = false;
            }
            Err(error) => warn!(
                target: telemetry_target::ENGINE,
                module = SHELL_MODULE_NAME,
                error = error.as_str(),
                "[editor] safe mode retry failed"
            ),
        }
    }

    /// Set the cursor shape egui asked for, when it maps onto one.
    ///
    /// The resize shapes matter as much as the pointing hand: the
    /// dock reports one over a splitter, and without it the widened grab band
    /// is invisible to the person dragging it.
    pub(super) fn apply_cursor_icon(&mut self, icon: egui::CursorIcon) {
        if icon != self.last_cursor {
            info!(
                target: telemetry_target::ENGINE,
                icon = format!("{icon:?}").as_str(),
                "[editor] cursor icon"
            );
            self.last_cursor = icon;
        }
        let shape = match icon {
            egui::CursorIcon::Default => winit::window::CursorIcon::Default,
            egui::CursorIcon::PointingHand => winit::window::CursorIcon::Pointer,
            egui::CursorIcon::Text => winit::window::CursorIcon::Text,
            egui::CursorIcon::Crosshair => winit::window::CursorIcon::Crosshair,
            egui::CursorIcon::Grab => winit::window::CursorIcon::Grab,
            egui::CursorIcon::Grabbing => winit::window::CursorIcon::Grabbing,
            egui::CursorIcon::ResizeHorizontal => winit::window::CursorIcon::EwResize,
            egui::CursorIcon::ResizeVertical => winit::window::CursorIcon::NsResize,
            egui::CursorIcon::ResizeNeSw => winit::window::CursorIcon::NeswResize,
            egui::CursorIcon::ResizeNwSe => winit::window::CursorIcon::NwseResize,
            egui::CursorIcon::ResizeEast => winit::window::CursorIcon::EResize,
            egui::CursorIcon::ResizeWest => winit::window::CursorIcon::WResize,
            egui::CursorIcon::ResizeNorth => winit::window::CursorIcon::NResize,
            egui::CursorIcon::ResizeSouth => winit::window::CursorIcon::SResize,
            egui::CursorIcon::ResizeColumn => winit::window::CursorIcon::ColResize,
            egui::CursorIcon::ResizeRow => winit::window::CursorIcon::RowResize,
            _ => return,
        };
        self.window.set_cursor(shape);
    }

    /// Apply the shell's IME state to the main window.
    ///
    /// egui decides when a text field wants the input method and where its
    /// caret is; this executable is what owns the window, so it is the only
    /// place that can pass those decisions to the platform. The rect is in
    /// logical points, which is the window's IME coordinate space.
    pub(super) fn apply_ime(
        &mut self,
        allowed: bool,
        rect: Option<pill_editor_core::boundary::ShellRect>,
    ) {
        let window = self.window.clone();
        if allowed != self.ime_allowed {
            window.set_ime_allowed(allowed);
            self.ime_allowed = allowed;
            info!(
                target: telemetry_target::ENGINE,
                allowed,
                "[editor] ime allowed changed"
            );
        }
        if !allowed {
            return;
        }
        if let Some(rect) = rect {
            if self.ime_rect != Some((rect.x, rect.y, rect.width, rect.height)) {
                self.ime_rect = Some((rect.x, rect.y, rect.width, rect.height));
                window.set_ime_cursor_area(
                    winit::dpi::LogicalPosition::new(rect.x as f64, rect.y as f64),
                    winit::dpi::LogicalSize::new(rect.width as f64, rect.height as f64),
                );
                info!(
                    target: telemetry_target::ENGINE,
                    x = rect.x,
                    y = rect.y,
                    width = rect.width,
                    height = rect.height,
                    "[editor] ime cursor area"
                );
            }
        }
    }
}
