//! The editor window's frame, one named phase at a time.
//!
//! # Responsibilities
//!
//! - Run one editor frame: input, publish, world, answers, present - in that
//!   order, each phase a method of its own.
//! - Decide what the next frame owes: the event loop's wake, the redraw
//!   cadence and the reason the frame asked.
//!
//! # Design
//!
//! The phases run on one borrow of the window, so this split is organizational
//! rather than a decoupling: each phase names the step it performs, and the
//! `frame` method is the list of them in order. The publish-before-run and
//! consume-after-run invariant the engine relies on is stated once, in
//! `frame`'s doc.

// Current crate
use super::*;

impl EditorWindow {
    /// Run one world frame: input, the shell's pass, the overlay draw.
    ///
    /// Returns whether the run should end. The event loop is what a scene
    /// view the dock opened this frame needs: a child window is created
    /// through it.
    ///
    /// The phases are named because their order is load-bearing: the resources
    /// a pass reads are published before [`Self::run_world`], the answers that
    /// pass produced are consumed after it, and presentation is last. Anything
    /// a later phase needs from an earlier one is a field or a return value,
    /// never a call that happens to sit above it.
    pub(super) fn frame(&mut self, event_loop: &ActiveEventLoop) -> Result<bool, EditorError> {
        self.frame_index += 1;
        let frame_started = Instant::now();
        self.begin_frame(frame_started);
        self.publish_frame_inputs();
        self.service_editor_requests();
        self.publish_frame_resources();
        self.run_world()?;
        self.consume_frame_answers();
        self.present_frame(event_loop);
        self.end_frame(frame_started);
        Ok(self.quit)
    }

    /// Step 1: the frame rate heartbeat, for the person reading the log.
    pub(super) fn begin_frame(&mut self, frame_started: Instant) {
        // Step 1: the frame rate heartbeat, for the person reading the log.
        let now = frame_started;
        if let Some(last) = self.last_frame_time {
            let delta_time = now.duration_since(last).as_secs_f32();
            if delta_time > 0.0 {
                let instant = 1.0 / delta_time;
                self.fps = if self.fps > 0.0 {
                    self.fps * (1.0 - FPS_SMOOTHING) + instant * FPS_SMOOTHING
                } else {
                    instant
                };
            }
        }
        self.last_frame_time = Some(now);
        if self.frame_index.is_multiple_of(FRAME_HEARTBEAT_FRAMES)
            || self
                .last_heartbeat
                .is_none_or(|at| now.duration_since(at).as_secs_f32() >= FRAME_HEARTBEAT_SECONDS)
        {
            self.last_heartbeat = Some(now);
            let overlay_frames = self
                .overlay
                .as_ref()
                .map(OverlaySurface::frames)
                .unwrap_or(0);
            let idle_frame_millis = self.idle_frame_millis_max;
            self.idle_frame_millis_max = 0.0;
            info!(
                target: telemetry_target::ENGINE,
                fps = self.fps,
                frame = self.frame_index,
                overlay_frames,
                idle_frame_millis,
                "[editor] frame heartbeat"
            );
        }
    }

    /// Step 2: this frame's inputs, and the state the shell's pass reads.
    ///
    /// The window events become engine input, the cameras and badges the frame
    /// draws are published, and the shell's own input record is filled before
    /// the pass that reads it.
    pub(super) fn publish_frame_inputs(&mut self) {
        // Step 2: the window events of the last request become engine input
        // events; the shell's egui pass reads the same input, so the UI and
        // the game see one event stream.
        for event in self.pending_events.drain(..) {
            self.host.host_mut().engine_mut().push_input_event(event);
        }
        // Step 2a: any camera a navigation moved is re-published, so the
        // renderer draws the frame with the view the user just steered.
        self.publish_cameras();
        // Step 2b: a scripted drag in flight takes its next stage - the
        // press, the move and the release, one per frame.
        self.advance_scripted_drag();
        // Step 2c: what the host did since the last frame - a reload, a patch,
        // a failure, a rollback - becomes the notices the shell draws toasts
        // and Problems rows from.
        self.fold_host_notices();
        // Step 2c2: where each entity lands in each visible scene view, for
        // the shell's badges.
        self.publish_badges();
        // Step 2d: the document's title is reported when it moves, so the log
        // names the scene the session holds; the window's own title names the
        // project.
        self.report_scene_title();
        // Step 3: the shell boundary's input resource.
        if let Some(input) = self
            .host
            .host_mut()
            .engine_mut()
            .world_mut()
            .get_resource_mut::<EguiShellInput>()
        {
            // The modifier state opens the frame's stream, so the bridge
            // starts from what is held rather than from nothing.
            let mut events = Vec::with_capacity(self.shell_events.len() + 1);
            events.push(InputEvent::ModifiersChanged {
                modifiers: EngineModifiers {
                    shift: self.modifiers.shift,
                    control: self.modifiers.control,
                    alt: self.modifiers.alt,
                    super_key: false,
                },
            });
            events.append(&mut self.shell_events);
            // A paste chord in this frame's events carries the OS clipboard's
            // text into egui, so a focused text field pastes.
            let paste_text = paste_chord(&events).then(|| {
                crate::clipboard::read()
                    .ok()
                    .flatten()
                    .filter(|text| !text.is_empty())
            });
            *input = EguiShellInput {
                screen_size: pill_core::math::Vector2f::new(
                    self.size.0 as f32 / self.scale_factor,
                    self.size.1 as f32 / self.scale_factor,
                ),
                scale_factor: self.scale_factor,
                focused: self.focused,
                time: self.started.elapsed().as_secs_f64(),
                frames_per_second: self.fps,
                events,
                cursor: self.shell_cursor,
                paste_text: paste_text.flatten(),
                badges: self.badges.clone(),
                // The row geometry tokens exist for the scripted channel; a
                // person's editor logs none of them.
                scripted: self.test_channel.is_some(),
            };
        }
        // Step 3a2: each popped-out window's own input record: its geometry,
        // focus and frame rate this frame. Its events are appended as its
        // window reports them and cleared when the frame that read them is
        // presented, so a record never replays an event.
        self.publish_popout_geometry();
    }

    /// Step 3b-3d: what the editor asked for since the last frame.
    ///
    /// The queued [`EditorCommand`]s drain here - the frame boundary their
    /// contract names - then the watchers, the panels and the clipboard, all
    /// before the snapshot so a tab never reads a frame-stale state.
    pub(super) fn service_editor_requests(&mut self) {
        // Step 3b: apply the engine mutations the shell's assets actions and
        // the scripted channel queued - the frame boundary the
        // `EditorCommand` contract names. A failed command never aborts the
        // rest of the batch.
        self.apply_pending_commands();
        // Step 3c: the gizmo's persistence and the preferences file's live
        // reader, both before the snapshot so the tabs read the state this
        // frame's edits left.
        self.persist_gizmo_settings();
        self.watch_preferences();
        self.watch_theme();
        self.watch_keybindings();
        self.publish_console_lines();
        self.drain_problem_open();
        // Step 3d: the panels the loaded modules asked to register, then what
        // the panels drawn last frame asked for. The sync is
        // before the snapshot so a tab's title is never a frame stale.
        // The recovery document a killed run left behind is applied before the
        // dock is read, and the current state is written
        // back once its arrangement or selection changed.
        self.apply_recovered_state();
        self.autosave_recovery();
        self.sync_panels();
        self.apply_panel_commands();
        // The clipboard the shell queued last frame: the OS
        // half is this executable's, applied here like every other queued
        // request.
        self.apply_clipboard();
    }

    /// Steps 4-5: the resources the frame reads, published before it runs.
    pub(super) fn publish_frame_resources(&mut self) {
        // Step 4: the frame's snapshot for the shell's data tabs, when the
        // shell loaded and registered its resources.
        self.publish_snapshot();
        // Step 5: the world resources the frame draws and picks with,
        // published before it runs: the shared selection's outline set (one
        // write per change) and the gizmo handles for it.
        self.publish_selection_outline();
        self.publish_gizmo_geometry();
    }

    /// Step 6: run the world frame, the shell's egui pass inside it.
    pub(super) fn run_world(&mut self) -> Result<(), EditorError> {
        // Step 6: run the world frame; the shell's egui pass runs inside it
        // (a registered post-update system) and leaves the frame resource
        // for presentation.
        self.host.run_one_frame().map_err(EditorError::Renderer)?;
        Ok(())
    }

    /// Step 7: the answers this frame's pass produced.
    pub(super) fn consume_frame_answers(&mut self) {
        // Step 7: answers to scene picks that arrived during the frame
        // move the shared selection; the outline follows on the next
        // publish.
        self.drain_scene_picks();
        // Step 7a: the asset previews the renderer read back, and the next one
        // to ask for.
        self.apply_asset_previews();
        // Step 7b: the menu bar's requests - undo/redo, a window switch and a
        // quit - taken through the same paths the scripted channel uses.
        self.drain_menu_requests();
    }

    /// Step 8: draw the shell frame through the overlay, which is last.
    pub(super) fn present_frame(&mut self, event_loop: &ActiveEventLoop) {
        // Step 8: draw the shell frame through the overlay.
        self.present_ui(event_loop);
    }

    /// Steps 9-10: the scripted channel, the deadline and the next wake.
    pub(super) fn end_frame(&mut self, frame_started: Instant) {
        // Step 9: the scripted channel, then the auto-quit deadline.
        self.poll_test_channel();
        if let Some(seconds) = self.test_seconds {
            if self.started.elapsed().as_secs_f64() >= seconds {
                info!(
                    target: telemetry_target::ENGINE,
                    "[editor] test deadline reached; quitting"
                );
                self.request_quit();
            }
        }
        // Step 10: what the loop does next - draw again at
        // once when anything asked for a frame, or sleep for the idle poll
        // and let this frame's evidence expire.
        let frame_millis = frame_started.elapsed().as_secs_f32() * 1000.0;
        if self.polling {
            self.idle_frame_millis_max = self.idle_frame_millis_max.max(frame_millis);
        }
        self.decide_next_wake(frame_millis);
    }

    /// Decide when the loop draws next, from what asked for this frame.
    ///
    /// The demand is cleared here rather than at the start of the next frame:
    /// an event that arrives between the two sets it again, and the loop
    /// already knows to draw at once. A frame that leaves the loop polling
    /// logs `[editor] frame idle` once per transition, with the cost of the
    /// frame that settled.
    pub(super) fn decide_next_wake(&mut self, frame_millis: f32) {
        let demand = FrameDemand {
            animating: self.animating(),
            repaint_requested: self.repaint_after.is_some(),
            ..self.demand
        };
        self.cadence.record_frame(demand);
        self.last_demand = demand;
        self.next_wake = self.cadence.decide(demand);
        let polling = matches!(self.next_wake, FrameWake::After(_));
        if polling {
            self.cadence.record_wait();
        }
        if polling && !self.polling {
            info!(
                target: telemetry_target::ENGINE,
                poll_millis = pill_editor_core::cadence::IDLE_POLL.as_millis() as u64,
                frame_millis,
                "[editor] frame idle"
            );
        }
        self.polling = polling;
        self.demand = FrameDemand::default();
        self.repaint_after = None;
    }

    /// Whether something is moving, so the next frame is already owed.
    ///
    /// A paused project stops animating; a camera navigation and a scripted
    /// drag still move something, and a toast's countdown changes the frame's
    /// pixels as it expires.
    pub(super) fn animating(&self) -> bool {
        let running = !self.host.host().engine().systems_paused();
        running || self.camera_drag.is_some() || self.scripted_drag.is_some()
    }

    /// The wake the last frame left for the loop.
    pub(super) fn next_wake(&self) -> FrameWake {
        self.next_wake
    }
}
