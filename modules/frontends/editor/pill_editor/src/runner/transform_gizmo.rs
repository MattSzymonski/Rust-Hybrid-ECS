//! The transform gizmo: its handles' geometry, its drags and their history.
//!
//! # Responsibilities
//!
//! - Publish the handles' geometry and screen projections for the active tool,
//!   and answer which handle a pixel is over.
//! - Run a drag: the press's ray, the per-move write, and the release that
//!   records one undoable step.
//! - Draw the guides an active drag shows.
//!
//! # Design
//!
//! The drag's math lives in `pill_editor_core::gizmo`; this module owns the
//! world, the camera and the history. Every move is computed from the press's
//! transform rather than the previous move's, so a snapped drag cannot
//! accumulate rounding, and the release writes the press's value back before
//! queueing its recorded step - the command's apply arm takes its inverse from
//! the world's current value.

// Current crate
use super::*;

impl EditorWindow {
    /// Draw the gizmo handles for the selection, with their hover and drag
    /// states, the translation planes, the dynamic scale handles and the drag
    /// guides.
    ///
    /// The contract's own primitives with handle ids set: the same frame the
    /// renderer draws is the frame it picks, so a handle a drag grabs is
    /// always the handle that was on screen. Handles keep a constant screen
    /// size - their world length follows the active view's camera distance -
    /// and the frame they are drawn into is one frame's worth: nothing to
    /// clear here. The geometry is kept for the hover test, so what the
    /// pointer sees and what the highlight names cannot disagree.
    pub(super) fn publish_gizmo_geometry(&mut self) {
        // Read the frame's inputs first: the world borrow must end before
        // the camera helpers (which borrow `self`) run.
        let (settings, entity, transform) = {
            let world = self.host.host_mut().engine_mut().world_mut();
            let settings = world
                .get_resource::<GizmoState>()
                .map(|state| state.settings)
                .unwrap_or_default();
            let selected = world
                .get_resource::<EditorSelection>()
                .and_then(|selection| selection.primary());
            match selected.and_then(|entity| {
                read_transform(world, entity).map(|transform| (entity, transform))
            }) {
                Some((entity, transform)) => (settings, entity, transform),
                None => {
                    // No handles on screen: the next selection logs its
                    // anchor at once instead of on the heartbeat.
                    self.gizmo_logged = None;
                    self.gizmo_geometry = GizmoGeometry::default();
                    self.gizmo_reference = 0.0;
                    self.hovered_handle = None;
                    return;
                }
            }
        };
        let origin = transform.translation;
        let basis = gizmo_basis(settings.space, &transform);
        let view = self.active_view;
        let length = self.gizmo_length(view, origin);
        let states = HandleStates {
            hovered: self.hovered_handle,
            active: self.gizmo_drag.as_ref().map(|drag| drag.handle),
        };
        // A scale drag grows or shrinks the dragged axis' handle with the
        // factor it is writing: the handle's length is derived from the
        // press's scale every frame, never from the previous frame's, so it
        // cannot drift.
        let lengths = match self.gizmo_drag.as_ref() {
            Some(drag) => match drag.kind {
                HandleKind::Axis {
                    mode: GizmoMode::Scale,
                    axis,
                } => gizmo::scale_handle_lengths(
                    drag.reference,
                    drag.original.scale,
                    transform.scale,
                    axis,
                ),
                _ => [length; 3],
            },
            None => [length; 3],
        };
        let geometry = gizmo::build_geometry_styled(settings.mode, origin, basis, lengths, states);
        let anchor = self.project_to_screen(origin);
        let guides = self.gizmo_drag.as_ref().map(|drag| {
            (
                drag.kind,
                drag.basis,
                drag.start_pivot,
                drag.current_pivot,
                drag.reference,
            )
        });
        let world = self.host.host_mut().engine_mut().world_mut();
        if world.get_resource::<DebugDraw>().is_none() {
            world.insert_resource(DebugDraw::new(true));
        }
        if let Some(debug) = world.get_resource_mut::<DebugDraw>() {
            for (start, end, color, handle) in &geometry.lines {
                debug.gizmo_line(*start, *end, *color, *handle);
            }
            for (corners, color, handle) in &geometry.triangles {
                debug.gizmo_triangle(*corners, *color, *handle);
            }
            if let Some((kind, basis, start_pivot, current_pivot, reference)) = guides {
                draw_gizmo_guides(debug, kind, basis, start_pivot, current_pivot, reference);
            }
        }
        let guide_state = self.gizmo_drag.as_ref().map(|drag| drag.kind);
        let dotted = guide_state.is_some_and(|kind| gizmo_drag_mode(kind) == GizmoMode::Translate);
        let axis_guide = guide_state.and_then(HandleKind::axis).map(|(_, axis)| axis);
        if self.gizmo_guides != Some(guide_state.is_some()) {
            self.gizmo_guides = Some(guide_state.is_some());
            match axis_guide {
                Some(axis) => info!(
                    target: telemetry_target::ENGINE,
                    axis = gizmo_axis_name(axis),
                    dotted,
                    "[editor] gizmo guides"
                ),
                None => info!(
                    target: telemetry_target::ENGINE,
                    guides = guide_state.is_some(),
                    dotted,
                    "[editor] gizmo guides"
                ),
            }
        }
        self.gizmo_geometry = geometry;
        self.gizmo_reference = length;
        // The anchor line, on a change or on the periodic cadence: what a
        // developer (and the suite) reads to know where the handles are.
        if let Some((x, y)) = anchor {
            let header = (settings.mode, settings.space, x as i32, y as i32);
            if self.gizmo_logged != Some(header)
                || self.frame_index.is_multiple_of(GIZMO_HEARTBEAT_FRAMES)
            {
                self.gizmo_logged = Some(header);
                let geometry = &self.gizmo_geometry;
                info!(
                    target: telemetry_target::ENGINE,
                    entity = %entity,
                    mode = gizmo_mode_name(settings.mode),
                    space = gizmo_space_name(settings.space),
                    x,
                    y,
                    handles = geometry.lines.len() + geometry.triangles.len(),
                    snapped = settings.snap,
                    "[editor] gizmo anchor"
                );
            }
        }
    }

    /// The tool shortcuts: a press that the active keymap binds to a tool
    /// command selects that tool.
    ///
    /// The registry owns which key means what, so a preset or a keybindings
    /// override rebinds the editor without a restart; an unbound key falls
    /// through to the rest of the editor and the game.
    pub(super) fn gizmo_shortcut(&mut self, event: &winit::event::KeyEvent) {
        if event.state != winit::event::ElementState::Pressed || event.repeat {
            return;
        }
        let winit::keyboard::PhysicalKey::Code(code) = event.physical_key else {
            return;
        };
        let Some(key) = pill_input::winit_events::key_code(code) else {
            return;
        };
        let held = EngineModifiers {
            shift: self.modifiers.shift,
            control: self.modifiers.control,
            alt: self.modifiers.alt,
            super_key: false,
        };
        self.run_bound_command(key, held);
    }

    /// Switch the gizmo's mode; the shortcuts and the scripted `gizmo` verb
    /// land here.
    pub(super) fn set_gizmo_tool(&mut self, mode: GizmoMode) {
        let world = self.host.host_mut().engine_mut().world_mut();
        if world.get_resource::<GizmoState>().is_none() {
            world.insert_resource(GizmoState::default());
        }
        let Some(state) = world.get_resource_mut::<GizmoState>() else {
            return;
        };
        if state.settings.mode == mode {
            return;
        }
        state.settings.mode = mode;
        info!(
            target: telemetry_target::ENGINE,
            mode = gizmo_mode_name(mode),
            "[editor] gizmo tool"
        );
    }

    /// How long the handles are, in world units, at `origin` in `view`: a
    /// constant fraction of that view's camera distance, so the gizmo
    /// neither shrinks away nor swallows the scene.
    pub(super) fn gizmo_length(&self, view: usize, origin: [f32; 3]) -> f32 {
        (gizmo::distance(self.cameras[view].eye(), origin) * GIZMO_SCREEN_FRACTION)
            .clamp(0.05, 50.0)
    }

    /// One scene view's surface size in physical pixels.
    pub(super) fn scene_pixel_size(&self, view: usize) -> Option<(f32, f32)> {
        let scene = self.views.get(view)?.as_ref()?;
        let (width, height) = scene.window.size();
        Some((width as f32, height as f32))
    }

    /// The cursor ray through one physical pixel of one scene view's
    /// surface.
    ///
    /// Built from the editor camera's basis and the contract camera's
    /// vertical field of view - the same numbers the renderer projects
    /// with, so a pick and a drag agree about what a pixel looks at.
    pub(super) fn scene_ray(&self, view: usize, x: f32, y: f32) -> Option<([f32; 3], [f32; 3])> {
        let (width, height) = self.scene_pixel_size(view)?;
        let (tan, aspect) = projection_terms(width, height);
        let camera = &self.cameras[view];
        let forward = camera.forward();
        let right = camera.right();
        let up = gizmo::cross(right, forward);
        let ndc_x = x / width * 2.0 - 1.0;
        let ndc_y = 1.0 - y / height * 2.0;
        let direction = gizmo::add(
            forward,
            gizmo::add(
                gizmo::scale(right, ndc_x * tan * aspect),
                gizmo::scale(up, ndc_y * tan),
            ),
        );
        gizmo::normalize(direction).map(|direction| (camera.eye(), direction))
    }

    /// Where a world point lands on the active scene view's surface, in
    /// physical pixels.
    pub(super) fn project_to_screen(&self, point: [f32; 3]) -> Option<(f32, f32)> {
        self.project_to_screen_in(self.active_view, point)
    }

    /// Where a world point lands on one scene view's surface, in physical
    /// pixels.
    pub(super) fn project_to_screen_in(&self, view: usize, point: [f32; 3]) -> Option<(f32, f32)> {
        let (width, height) = self.scene_pixel_size(view)?;
        let (tan, aspect) = projection_terms(width, height);
        let camera = &self.cameras[view];
        let eye = camera.eye();
        let forward = camera.forward();
        let right = camera.right();
        let up = gizmo::cross(right, forward);
        let relative = gizmo::sub(point, eye);
        let depth = gizmo::dot(relative, forward);
        if depth <= 1e-4 {
            return None;
        }
        let ndc_x = gizmo::dot(relative, right) / (depth * tan * aspect);
        let ndc_y = gizmo::dot(relative, up) / (depth * tan);
        Some(((ndc_x + 1.0) * 0.5 * width, (1.0 - ndc_y) * 0.5 * height))
    }

    /// Begin a drag from a handle pick: the press pixel's ray records the
    /// parameter or angle every later move is measured against.
    pub(super) fn begin_gizmo_drag(
        &mut self,
        view: usize,
        handle: u32,
        press_x: f32,
        press_y: f32,
    ) {
        let Some(kind) = gizmo::handle_kind(handle) else {
            warn!(
                target: telemetry_target::ENGINE,
                handle,
                "[editor] gizmo drag refused: unknown handle"
            );
            return;
        };
        let (settings, entity, original) = {
            let world = self.host.host_mut().engine_mut().world_mut();
            let settings = world
                .get_resource::<GizmoState>()
                .map(|state| state.settings)
                .unwrap_or_default();
            let selected = world
                .get_resource::<EditorSelection>()
                .and_then(|selection| selection.primary());
            let Some(entity) = selected else {
                return;
            };
            let Some(original) = read_transform(world, entity) else {
                return;
            };
            (settings, entity, original)
        };
        let Some((ray_origin, ray_direction)) = self.scene_ray(view, press_x, press_y) else {
            return;
        };
        let basis = gizmo_basis(settings.space, &original);
        let origin = original.translation;
        let (start_value, start_plane) = match kind {
            HandleKind::Axis {
                mode: GizmoMode::Rotate,
                axis,
            } => {
                let Some(point) =
                    gizmo::ray_plane_point(origin, basis[axis], ray_origin, ray_direction)
                else {
                    warn!(
                        target: telemetry_target::ENGINE,
                        "[editor] gizmo drag refused: the ray is parallel to the ring"
                    );
                    return;
                };
                (gizmo::plane_angle(origin, basis[axis], point), None)
            }
            HandleKind::Axis { axis, .. } => {
                let Some(value) =
                    gizmo::closest_axis_point(origin, basis[axis], ray_origin, ray_direction)
                else {
                    warn!(
                        target: telemetry_target::ENGINE,
                        "[editor] gizmo drag refused: the ray is parallel to the axis"
                    );
                    return;
                };
                (value, None)
            }
            HandleKind::Plane { first, second } => {
                // The plane is the one the pair spans: its normal is the
                // remaining axis, whichever space the basis is in.
                let normal = basis[3 - first - second];
                let Some(point) = gizmo::ray_plane_point(origin, normal, ray_origin, ray_direction)
                else {
                    warn!(
                        target: telemetry_target::ENGINE,
                        "[editor] gizmo drag refused: the ray is parallel to the plane"
                    );
                    return;
                };
                let (along_first, along_second) =
                    gizmo::plane_coordinates(origin, basis[first], basis[second], point);
                (0.0, Some((along_first, along_second)))
            }
        };
        let reference = self.gizmo_length(view, origin);
        self.gizmo_drag = Some(GizmoDrag {
            view,
            handle,
            kind,
            entity,
            original,
            basis,
            start_value,
            start_plane,
            start_pivot: origin,
            current_pivot: origin,
            reference,
            last_value: None,
            written: None,
        });
        // The grabbed handle is the active one from here on, so it draws
        // dimmed for as long as the drag runs.
        self.set_hovered_handle(view, Some(handle));
        match kind.plane() {
            Some((first, second)) => info!(
                target: telemetry_target::ENGINE,
                mode = gizmo_mode_name(GizmoMode::Translate),
                plane = gizmo_plane_name(first, second),
                entity = %entity,
                handle,
                "[editor] gizmo drag started"
            ),
            None => info!(
                target: telemetry_target::ENGINE,
                mode = gizmo_mode_name(gizmo::handle_axis(handle).map(|(mode, _)| mode).unwrap_or(GizmoMode::Translate)),
                axis = gizmo_axis_name(kind.axis().map(|(_, axis)| axis).unwrap_or(0)),
                entity = %entity,
                handle,
                "[editor] gizmo drag started"
            ),
        }
    }

    /// Apply one move of the drag in flight: re-read the ray, take the delta
    /// against the press, snap it when the tool snaps, and write the values
    /// straight into the world. A drag records no undo entry.
    ///
    /// Every write is computed from the press's transform, never from the
    /// previous frame's, so a snapped or clamped drag cannot accumulate
    /// rounding - and the drag's basis is the one the handles were built in,
    /// so a local-space drag keeps following the object's own axes.
    pub(super) fn update_gizmo_drag(&mut self, cursor_x: f32, cursor_y: f32) {
        let Some(drag) = self.gizmo_drag.as_ref() else {
            return;
        };
        let (view, kind, entity) = (drag.view, drag.kind, drag.entity);
        let (original, basis) = (drag.original, drag.basis);
        let (start_value, start_plane) = (drag.start_value, drag.start_plane);
        let reference = drag.reference;
        let Some((ray_origin, ray_direction)) = self.scene_ray(view, cursor_x, cursor_y) else {
            return;
        };
        let settings = self.gizmo_settings();
        let origin = original.translation;
        let (written, pivot) = match kind {
            HandleKind::Axis {
                mode: GizmoMode::Rotate,
                axis,
            } => {
                let Some(point) =
                    gizmo::ray_plane_point(origin, basis[axis], ray_origin, ray_direction)
                else {
                    return;
                };
                let delta =
                    gizmo::wrap_angle(gizmo::plane_angle(origin, basis[axis], point) - start_value);
                let degrees = if settings.snap {
                    gizmo::snap_value(delta.to_degrees(), settings.rotate_step_degrees)
                } else {
                    delta.to_degrees()
                };
                // The axis is the basis' own, so a local-mode drag turns
                // around the object's axis and a world-mode drag around the
                // world's.
                let turn = gizmo::axis_angle_quaternion(basis[axis], degrees.to_radians());
                let combined = quaternion_product(turn, original.rotation);
                let written = self.write_rotation(entity, combined);
                (written.then_some(combined), origin)
            }
            HandleKind::Axis {
                mode: GizmoMode::Translate,
                axis,
            } => {
                let Some(value) =
                    gizmo::closest_axis_point(origin, basis[axis], ray_origin, ray_direction)
                else {
                    return;
                };
                let mut delta = value - start_value;
                if settings.snap {
                    delta = gizmo::snap_value(delta, settings.translate_step);
                }
                // The displacement is along the handle's axis, not a world
                // component: in local space the two differ as soon as the
                // object is rotated.
                let translation =
                    gizmo::add(original.translation, gizmo::scale(basis[axis], delta));
                let written = self.write_translation(entity, translation);
                (
                    written.then_some([translation[0], translation[1], translation[2], 1.0]),
                    translation,
                )
            }
            HandleKind::Axis {
                mode: GizmoMode::Scale,
                axis,
            } => {
                let Some(value) =
                    gizmo::closest_axis_point(origin, basis[axis], ray_origin, ray_direction)
                else {
                    return;
                };
                let factor = 1.0 + (value - start_value) / reference.max(1e-4);
                let mut scaled = original.scale[axis] * factor;
                if settings.snap {
                    scaled = gizmo::snap_value(scaled, settings.scale_step);
                }
                // A zero or negative scale is a degenerate mesh; the drag
                // stops at the smallest positive value instead.
                let scaled = scaled.max(0.01);
                let mut values = original.scale;
                values[axis] = scaled;
                let written = self.write_scale(entity, values);
                (written.then_some([scaled, 0.0, 0.0, 0.0]), origin)
            }
            HandleKind::Plane { first, second } => {
                let Some((start_first, start_second)) = start_plane else {
                    return;
                };
                let normal = basis[3 - first - second];
                let Some(point) = gizmo::ray_plane_point(origin, normal, ray_origin, ray_direction)
                else {
                    return;
                };
                let (along_first, along_second) =
                    gizmo::plane_coordinates(origin, basis[first], basis[second], point);
                let (mut delta_first, mut delta_second) =
                    (along_first - start_first, along_second - start_second);
                if settings.snap {
                    delta_first = gizmo::snap_value(delta_first, settings.translate_step);
                    delta_second = gizmo::snap_value(delta_second, settings.translate_step);
                }
                let translation = gizmo::add(
                    gizmo::add(
                        original.translation,
                        gizmo::scale(basis[first], delta_first),
                    ),
                    gizmo::scale(basis[second], delta_second),
                );
                let written = self.write_translation(entity, translation);
                (
                    written.then_some([delta_first, delta_second, 0.0, 0.0]),
                    translation,
                )
            }
        };
        if let Some(drag) = self.gizmo_drag.as_mut() {
            if let Some(written) = written {
                drag.last_value = Some(written);
            }
            drag.current_pivot = pivot;
        }
    }

    /// Write a translation straight into the world; false when a component
    /// refused the write.
    pub(super) fn write_translation(&mut self, entity: Entity, translation: [f32; 3]) -> bool {
        let written = self.write_transform_field(entity, "translation", &translation);
        if written {
            self.note_gizmo_write("translation", &translation);
        }
        written
    }

    /// Write a rotation straight into the world.
    pub(super) fn write_rotation(&mut self, entity: Entity, rotation: [f32; 4]) -> bool {
        let written = self.write_transform_field(entity, "rotation", &rotation);
        if written {
            self.note_gizmo_write("rotation", &rotation);
        }
        written
    }

    /// Write a scale straight into the world.
    pub(super) fn write_scale(&mut self, entity: Entity, scale: [f32; 3]) -> bool {
        let written = self.write_transform_field(entity, "scale", &scale);
        if written {
            self.note_gizmo_write("scale", &scale);
        }
        written
    }

    /// Note what the drag in flight last wrote, so its release can record one
    /// step for it. A restore writes with no drag in flight and notes nothing.
    pub(super) fn note_gizmo_write(&mut self, field: &'static str, values: &[f32]) {
        if let Some(drag) = self.gizmo_drag.as_mut() {
            drag.written = Some((field, values.to_vec()));
        }
    }

    /// Write one transform field's components, in order; false when any of
    /// them was refused.
    ///
    /// A refusal is logged here, where the entity, field and element index are
    /// still known: the callers only see a `false` folded into "the move did
    /// not land", which reads exactly like a drag that never moved.
    pub(super) fn write_transform_field(
        &mut self,
        entity: Entity,
        field: &str,
        values: &[f32],
    ) -> bool {
        let world = self.host.host_mut().engine_mut().world_mut();
        let mut written = true;
        for (index, value) in values.iter().enumerate() {
            if let Err(error) = world.write_component_array_element(
                entity,
                TRANSFORM_SHARED_NAME,
                field,
                index,
                FieldValue::F32(*value),
            ) {
                warn!(
                    target: telemetry_target::ENGINE,
                    entity = %entity,
                    field = %field,
                    index,
                    error = %error,
                    "[editor] gizmo write refused"
                );
                written = false;
            }
        }
        written
    }

    /// Write the press's whole transform back: a cancelled drag leaves the
    /// object exactly where it started. False when any of the three writes was
    /// refused, so the cancel can say so instead of reporting a clean restore.
    pub(super) fn restore_transform(
        &mut self,
        entity: Entity,
        transform: &TransformComponent,
    ) -> bool {
        let mut restored = self.write_translation(entity, transform.translation);
        restored &= self.write_rotation(entity, transform.rotation);
        restored &= self.write_scale(entity, transform.scale);
        restored
    }

    /// Turn the drag that just ended into one recorded step, so `Ctrl+Z` after
    /// a drag undoes it the way it undoes every other edit.
    ///
    /// The press's value is written back first, and that write is load-bearing:
    /// the command's apply arm takes its inverse from the world's current
    /// value, which the drag has already overwritten with the final one.
    /// Queued on top of that, the step would record "final -> final" and the
    /// first undo would consume a history entry and change nothing.
    pub(super) fn record_gizmo_drag(&mut self, drag: &GizmoDrag, mode: GizmoMode) {
        let Some((field, values)) = drag.written.as_ref() else {
            return;
        };
        let original = match *field {
            "rotation" => drag.original.rotation.to_vec(),
            "scale" => drag.original.scale.to_vec(),
            _ => drag.original.translation.to_vec(),
        };
        let Some(edits) = gizmo::release_edits(&original, values) else {
            return;
        };
        self.write_transform_field(drag.entity, field, &original);
        let commands = edits
            .into_iter()
            .map(|(index, value)| {
                inspector::element_edit(
                    drag.entity,
                    TRANSFORM_SHARED_NAME,
                    field,
                    index,
                    FieldValue::F32(value),
                )
                .0
            })
            .collect();
        let label = format!("gizmo {}", gizmo_mode_name(mode));
        let world = self.host.host_mut().engine_mut().world_mut();
        if let Some(pending) = world.get_resource_mut::<PendingEditorCommands>() {
            pending.push_recorded(commands, label, None);
        }
        info!(
            target: telemetry_target::ENGINE,
            entity = %drag.entity,
            field = %field,
            "[editor] gizmo drag recorded"
        );
    }

    /// End the drag in flight, reporting what it wrote. The guides and the
    /// active highlight go with it: the handles return to their idle (or
    /// hovered) look on the next frame.
    pub(super) fn end_gizmo_drag(&mut self) {
        let Some(drag) = self.gizmo_drag.take() else {
            return;
        };
        let settings = self.gizmo_settings();
        let mode = gizmo_drag_mode(drag.kind);
        match (drag.kind.plane(), drag.last_value) {
            (Some((first, second)), Some(value)) => info!(
                target: telemetry_target::ENGINE,
                mode = gizmo_mode_name(mode),
                plane = gizmo_plane_name(first, second),
                entity = %drag.entity,
                value = ?value,
                snapped = settings.snap,
                "[editor] gizmo drag ended"
            ),
            (Some((first, second)), None) => info!(
                target: telemetry_target::ENGINE,
                mode = gizmo_mode_name(mode),
                plane = gizmo_plane_name(first, second),
                entity = %drag.entity,
                "[editor] gizmo drag ended without a move"
            ),
            (None, Some(value)) => info!(
                target: telemetry_target::ENGINE,
                mode = gizmo_mode_name(mode),
                axis = gizmo_axis_name(drag.kind.axis().map(|(_, axis)| axis).unwrap_or(0)),
                entity = %drag.entity,
                value = ?value,
                snapped = settings.snap,
                "[editor] gizmo drag ended"
            ),
            (None, None) => info!(
                target: telemetry_target::ENGINE,
                mode = gizmo_mode_name(mode),
                axis = gizmo_axis_name(drag.kind.axis().map(|(_, axis)| axis).unwrap_or(0)),
                entity = %drag.entity,
                "[editor] gizmo drag ended without a move"
            ),
        }
        self.record_gizmo_drag(&drag, mode);
        self.set_hovered_handle(drag.view, None);
    }

    /// Cancel the drag in flight: the press's transform is written back and
    /// the drag dropped, so a focus loss or a viewport change leaves no
    /// half-applied manipulation (or its guides) behind.
    pub(super) fn cancel_gizmo_drag(&mut self) {
        let Some(drag) = self.gizmo_drag.take() else {
            return;
        };
        let restored = self.restore_transform(drag.entity, &drag.original);
        if !restored {
            warn!(
                target: telemetry_target::ENGINE,
                entity = %drag.entity,
                "[editor] gizmo drag restore failed"
            );
        }
        info!(
            target: telemetry_target::ENGINE,
            mode = gizmo_mode_name(gizmo_drag_mode(drag.kind)),
            entity = %drag.entity,
            "[editor] gizmo drag canceled"
        );
        self.set_hovered_handle(drag.view, None);
    }

    /// Drop every interaction in flight: the focus was lost, so no press or
    /// release is coming to finish them.
    pub(super) fn cancel_interactions(&mut self, reason: &str) {
        if self.camera_drag.take().is_some() {
            info!(
                target: telemetry_target::ENGINE,
                reason,
                "[editor] camera navigation canceled"
            );
        }
        self.cancel_gizmo_drag();
        self.press_pick = None;
        self.left_down.fill(false);
        self.set_hovered_handle(self.active_view, None);
    }

    /// Press on one handle of the active tool (the scripted channel): the
    /// press pixel is the handle's grab point projected, so the pick that
    /// answers it proves the same geometry the renderer draws.
    pub(super) fn start_synthetic_gizmo_drag(&mut self, handle: u32, delta: Option<(f32, f32)>) {
        let Some(kind) = gizmo::handle_kind(handle) else {
            warn!(
                target: telemetry_target::ENGINE,
                handle,
                "[editor] gizmo drag refused: unknown handle"
            );
            return;
        };
        if self.gizmo_drag.is_some() {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] gizmo drag refused: one is already in flight"
            );
            return;
        }
        let Some(view) = self.visible_scene_view() else {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] gizmo drag refused: no scene view is visible"
            );
            return;
        };
        let (settings, transform) = {
            let world = self.host.host_mut().engine_mut().world_mut();
            let settings = world
                .get_resource::<GizmoState>()
                .map(|state| state.settings)
                .unwrap_or_default();
            let selected = world
                .get_resource::<EditorSelection>()
                .and_then(|selection| selection.primary());
            let Some(entity) = selected else {
                warn!(
                    target: telemetry_target::ENGINE,
                    "[editor] gizmo drag refused: nothing is selected"
                );
                return;
            };
            let Some(transform) = read_transform(world, entity) else {
                warn!(
                    target: telemetry_target::ENGINE,
                    entity = %entity,
                    "[editor] gizmo drag refused: the entity has no transform"
                );
                return;
            };
            (settings, transform)
        };
        let basis = gizmo_basis(settings.space, &transform);
        let length = self.gizmo_length(view, transform.translation);
        // Each handle is grabbed where its own geometry carries it: a
        // rotation ring on its radius at the diagonal between two axes (no
        // other ring passes a pixel away), a plane square at its centre,
        // and a straight handle just short of its tip.
        let grab = match kind {
            HandleKind::Axis { axis, .. } => match gizmo_drag_mode(kind) {
                GizmoMode::Rotate => gizmo::add(
                    transform.translation,
                    gizmo::scale(
                        gizmo::normalize(gizmo::add(basis[(axis + 1) % 3], basis[(axis + 2) % 3]))
                            .unwrap_or(basis[axis]),
                        length,
                    ),
                ),
                _ => gizmo::add(
                    transform.translation,
                    gizmo::scale(basis[axis], length * GIZMO_PRESS_FRACTION),
                ),
            },
            HandleKind::Plane { first, second } => gizmo::add(
                transform.translation,
                gizmo::add(
                    gizmo::scale(basis[first], length * gizmo::PLANE_OFFSET),
                    gizmo::scale(basis[second], length * gizmo::PLANE_OFFSET),
                ),
            ),
        };
        let Some((x, y)) = self.project_to_screen_in(view, grab) else {
            warn!(
                target: telemetry_target::ENGINE,
                "[editor] gizmo drag refused: the handle is behind the camera"
            );
            return;
        };
        self.synthetic_gizmo_pending = delta;
        self.synthetic_gizmo_hold = delta.is_none();
        self.synthetic_gizmo_press = None;
        self.synthetic_gizmo_view = Some(view);
        match kind.plane() {
            Some((first, second)) => info!(
                target: telemetry_target::ENGINE,
                plane = gizmo_plane_name(first, second),
                x,
                y,
                "[editor] gizmo drag press"
            ),
            None => info!(
                target: telemetry_target::ENGINE,
                axis = gizmo_axis_name(kind.axis().map(|(_, axis)| axis).unwrap_or(0)),
                x,
                y,
                "[editor] gizmo drag press"
            ),
        }
        // The press pixel projects onto the handle's own geometry, but a
        // one-pixel line straddles pixel rows: the press asks the exact
        // pixel and its four neighbours, and the first that names a handle
        // wins.
        let (centre_x, centre_y) = (x.round() as i64, y.round() as i64);
        self.synthetic_press_ids.clear();
        for (pixel_x, pixel_y) in [
            (centre_x, centre_y),
            (centre_x - 1, centre_y),
            (centre_x + 1, centre_y),
            (centre_x, centre_y - 1),
            (centre_x, centre_y + 1),
        ] {
            if let Some(id) =
                self.queue_scene_pick_at(view, pixel_x.max(0) as u32, pixel_y.max(0) as u32)
            {
                self.synthetic_press_ids.push(id);
            }
        }
    }

    /// The gizmo settings from the world, defaults when no state exists.
    pub(super) fn gizmo_settings(&mut self) -> GizmoSettings {
        let world = self.host.host_mut().engine_mut().world_mut();
        world
            .get_resource::<GizmoState>()
            .map(|state| state.settings)
            .unwrap_or_default()
    }
}
