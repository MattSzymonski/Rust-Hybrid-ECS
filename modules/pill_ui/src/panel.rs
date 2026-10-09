//! Immediate-mode widget building: [`PanelBuilder`] produces a
//! [`PanelTree`] and this frame's [`PanelInput`]s.
//!
//! # Responsibilities
//!
//! - [`PanelBuilder`]: the widget calls a panel function makes, in the style
//!   of `ui.button("Bake")`.
//! - [`PanelTree`]: the owned, plain-data description of the frame's widgets
//!   that layout and painting consume.
//! - [`PanelInput`] and [`ClipboardRequest`]: what the frame reports back -
//!   activations, changes, escape, and clipboard traffic as plain data,
//!   never a platform call.
//! - Widget identity: ids derive from the build path and a widget's name, so
//!   per-id state survives the tree being rebuilt every frame.
//!
//! # Design
//!
//! A panel function runs once per frame between
//! [`UiInput::begin_frame`](crate::input::UiInput::begin_frame) and layout.
//! Widget calls decide what the *previous* frame's [rectangles](UiState) say
//! about the pointer and keys, mutate per-id memory (cursor, scroll,
//! expansion, selection), and append nodes to the tree; the tree is then
//! laid out and painted, and the fresh rectangles are what the next frame
//! interacts against. That one-frame lag is deliberate: a click in frame N
//! activates in frame N+1, which is the contract the plan's acceptance test
//! names.
//!
//! Closures (`ui.column(|ui| ...)`, `ui.collapsing(...)`) run immediately and
//! are never stored: the tree holds only plain data. `taffy` is never
//! visible here - layout maps this tree onto it in
//! [crate::layout]. Widgets never talk to the platform; clipboard access is
//! a request in the frame's output ([`ClipboardRequest`]), fulfilled by the
//! shell, and paste content re-enters as a normal text input event.

// Standard library
use std::collections::{BTreeMap, HashMap};

// External crates
use pill_core::math::{Vector2f, Vector3f};
use pill_engine::{KeyCode, MouseButton};

// Current crate
use crate::draw::{Color, Rect, TextureId};
use crate::input::UiInput;
use crate::state::{Interaction, UiState};
use crate::theme::Theme;
use crate::widget::{TextEditState, WidgetId, WidgetMemory};

/// How many logical pixels one wheel line scrolls.
const SCROLL_LINE_HEIGHT: f32 = 24.0;

// =============================================================================
// Frame results
// =============================================================================

/// A clipboard operation the frame asks the shell to perform.
///
/// `pill_ui` never calls a platform API (plan decision D28): the shell
/// fulfils these after the frame, and pasted text re-enters through
/// [`InputEvent::TextInput`](pill_engine::InputEvent) in a later frame,
/// where the focused text field consumes it like typed text.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClipboardRequest {
    /// Put `text` on the clipboard (copy or cut).
    SetText(String),
    /// Read the clipboard; the result arrives as a text input event.
    GetText,
}

/// Something a widget reported this frame.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum PanelInput {
    /// A button-like widget was activated (click, Enter, or keyboard
    /// equivalent).
    Activated(WidgetId),
    /// A value changed: a toggle flipped, a slider or drag moved, text was
    /// edited, a selection moved.
    Changed(WidgetId),
    /// Escape was pressed this frame; the shell closes its topmost dialog or
    /// menu.
    Cancelled,
}

/// One frame of building: the tree to lay out and paint, and what the
/// widgets reported.
#[derive(Clone, Debug)]
pub struct PanelOutput {
    /// The frame's widgets, as owned data.
    pub tree: PanelTree,
    /// Activations and changes, in the order they happened.
    pub inputs: Vec<PanelInput>,
    /// Clipboard operations for the shell, in order.
    pub clipboard: Vec<ClipboardRequest>,
}

// =============================================================================
// The tree
// =============================================================================

/// One node of the frame's widget tree.
///
/// Containers own their children; every interactive node carries its
/// [`WidgetId`]. This is the only form the rest of the system sees - layout
/// turns it into `taffy` styles, painting turns it into a draw list, and a
/// module could hand it across a boundary as plain data.
#[derive(Clone, Debug)]
pub enum PanelNode {
    /// A horizontal group of widgets.
    Row {
        /// The row's children, left to right.
        children: Vec<PanelNode>,
    },
    /// A vertical group of widgets.
    Column {
        /// The column's children, top to bottom.
        children: Vec<PanelNode>,
    },
    /// Static text.
    Label {
        /// The text to draw.
        text: String,
    },
    /// Section text, drawn larger.
    Heading {
        /// The text to draw.
        text: String,
    },
    /// A one-pixel horizontal rule.
    Separator,
    /// A textured rectangle: an atlas region or an external texture.
    Image {
        /// The widget's identity.
        id: WidgetId,
        /// The texture to sample.
        texture: TextureId,
        /// Top-left texture coordinate.
        uv_min: Vector2f,
        /// Bottom-right texture coordinate.
        uv_max: Vector2f,
        /// The rectangle's size in logical pixels.
        size: Vector2f,
        /// A colour multiplied into the sampled texels; white draws the
        /// texture as-is.
        color: Color,
    },
    /// A push button.
    Button {
        /// The widget's identity.
        id: WidgetId,
        /// The caption.
        label: String,
        /// Whether the button reacts at all.
        enabled: bool,
    },
    /// A labelled toggle.
    Checkbox {
        /// The widget's identity.
        id: WidgetId,
        /// The caption next to the box.
        label: String,
        /// The value the caller passed in, after this frame's input.
        checked: bool,
        /// Whether the checkbox reacts at all.
        enabled: bool,
    },
    /// A horizontal value slider.
    Slider {
        /// The widget's identity.
        id: WidgetId,
        /// The caption.
        label: String,
        /// The value the caller passed in, after this frame's input.
        value: f32,
        /// The low end of the range.
        min: f32,
        /// The high end of the range.
        max: f32,
        /// Whether the slider reacts at all.
        enabled: bool,
    },
    /// A label and a value the pointer drags or the arrow keys step.
    DragValue {
        /// The widget's identity.
        id: WidgetId,
        /// The caption; also the component name inside vector and colour
        /// fields.
        label: String,
        /// The value the caller passed in, after this frame's input.
        value: f32,
        /// How much one pixel of drag or one arrow press changes the value.
        step: f32,
        /// Whether the field reacts at all.
        enabled: bool,
    },
    /// A single-line text entry.
    TextField {
        /// The widget's identity.
        id: WidgetId,
        /// The text after this frame's edits.
        text: String,
        /// Whether the field reacts at all.
        enabled: bool,
    },
    /// A chooser over a fixed list of options.
    ComboBox {
        /// The widget's identity.
        id: WidgetId,
        /// The index of the shown option, after this frame's input.
        selected: usize,
        /// The options, as owned strings.
        options: Vec<String>,
        /// Whether the box reacts at all.
        enabled: bool,
    },
    /// An RGBA colour: a swatch and one drag field per component.
    ColorField {
        /// The widget's identity.
        id: WidgetId,
        /// The colour after this frame's input.
        color: Color,
        /// Whether the field reacts at all.
        enabled: bool,
        /// The component fields, painted over the node's rectangle.
        children: Vec<PanelNode>,
    },
    /// Three drag fields for a 3D vector.
    VectorField {
        /// The widget's identity.
        id: WidgetId,
        /// The value after this frame's input.
        value: Vector3f,
        /// Whether the field reacts at all.
        enabled: bool,
        /// The component fields, painted over the node's rectangle.
        children: Vec<PanelNode>,
    },
    /// A titled section whose contents hide when closed.
    Collapsing {
        /// The widget's identity.
        id: WidgetId,
        /// The header text.
        title: String,
        /// Whether the section is open, after this frame's input.
        open: bool,
        /// The section's contents when open.
        children: Vec<PanelNode>,
    },
    /// A clipped, vertically scrollable group.
    ScrollArea {
        /// The widget's identity.
        id: WidgetId,
        /// The viewport height in logical pixels; `None` takes the content's
        /// height and never scrolls.
        height: Option<f32>,
        /// The scrolled contents.
        children: Vec<PanelNode>,
    },
    /// A header row and uniform data rows, one selection.
    Table {
        /// The widget's identity.
        id: WidgetId,
        /// Column titles.
        headers: Vec<String>,
        /// The cell text, row-major.
        rows: Vec<Vec<String>>,
        /// The selected row, if any.
        selected: Option<usize>,
    },
    /// One visible row of a tree view.
    TreeRow {
        /// The row's identity, derived from the node's path and label.
        id: WidgetId,
        /// How deep the row sits; indentation is `depth * indent`.
        depth: u16,
        /// The row's label.
        label: String,
        /// Whether the row's children are shown.
        open: bool,
        /// Whether the row has children at all.
        has_children: bool,
        /// Whether this row is the tree's selection.
        selected: bool,
    },
}

impl PanelNode {
    /// The interactive id this node carries, if any.
    pub fn id(&self) -> Option<WidgetId> {
        match self {
            PanelNode::Row { .. }
            | PanelNode::Column { .. }
            | PanelNode::Label { .. }
            | PanelNode::Heading { .. }
            | PanelNode::Separator => None,
            PanelNode::Image { id, .. }
            | PanelNode::Button { id, .. }
            | PanelNode::Checkbox { id, .. }
            | PanelNode::Slider { id, .. }
            | PanelNode::DragValue { id, .. }
            | PanelNode::TextField { id, .. }
            | PanelNode::ComboBox { id, .. }
            | PanelNode::ColorField { id, .. }
            | PanelNode::VectorField { id, .. }
            | PanelNode::Collapsing { id, .. }
            | PanelNode::ScrollArea { id, .. }
            | PanelNode::Table { id, .. }
            | PanelNode::TreeRow { id, .. } => Some(*id),
        }
    }

    /// The node's child nodes, in paint order.
    pub fn children(&self) -> &[PanelNode] {
        match self {
            PanelNode::Row { children }
            | PanelNode::Column { children }
            | PanelNode::ColorField { children, .. }
            | PanelNode::VectorField { children, .. }
            | PanelNode::Collapsing { children, .. }
            | PanelNode::ScrollArea { children, .. } => children,
            _ => &[],
        }
    }
}

/// One node of a tree view, as the caller describes it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TreeNode {
    /// The row's label.
    pub label: String,
    /// The row's children, shown when the row is open.
    pub children: Vec<TreeNode>,
}

impl TreeNode {
    /// A leaf.
    pub fn new(label: &str) -> TreeNode {
        TreeNode {
            label: label.to_string(),
            children: Vec::new(),
        }
    }

    /// A node with children.
    pub fn with_children(label: &str, children: Vec<TreeNode>) -> TreeNode {
        TreeNode {
            label: label.to_string(),
            children,
        }
    }
}

/// One frame's widget tree.
#[derive(Clone, Debug)]
pub struct PanelTree {
    /// The panel's root: a vertical stack of everything the panel function
    /// built.
    pub root: PanelNode,
    /// Every focusable widget's id, in build order: the Tab order, and the
    /// order painting walks to draw focus rings.
    pub focus_order: Vec<WidgetId>,
}

// =============================================================================
// PanelBuilder
// =============================================================================

/// What one widget call learned from this frame's input.
#[derive(Copy, Clone, Debug, Default)]
struct FrameInteraction {
    /// The pointer went down on the widget this frame.
    pressed: bool,
    /// The widget is being dragged, or is about to be: the pointer went down
    /// on it and has not been released.
    held: bool,
    /// The pointer was released over the widget after going down on it.
    clicked: bool,
    /// The widget holds the keyboard focus.
    focused: bool,
}

/// The widget builder a panel function drives.
///
/// Lives for exactly one frame: [`PanelBuilder::new`] splits the state's
/// borrows, one call per widget, [`PanelBuilder::finish`] hands back the
/// tree and this frame's events.
pub struct PanelBuilder<'a> {
    /// This frame's input.
    input: &'a UiInput,
    /// Where widgets were last frame, for hit-testing.
    hit_rects: &'a BTreeMap<WidgetId, Rect>,
    /// The state a frame advances.
    interaction: &'a mut Interaction,
    /// Per-widget memory.
    memory: &'a mut BTreeMap<WidgetId, WidgetMemory>,
    /// The metrics and colours the panel is built for.
    theme: &'a Theme,
    /// The naming path of the container being built.
    path: Vec<String>,
    /// How many auto-named widgets of each kind the current group holds.
    level_counters: HashMap<String, u32>,
    /// Containers under construction; the last is the open group.
    stack: Vec<PanelNode>,
    /// Focusable ids, in build order.
    focusable: Vec<WidgetId>,
    /// Activations and changes, in order.
    inputs: Vec<PanelInput>,
    /// Clipboard requests, in order.
    clipboard: Vec<ClipboardRequest>,
    /// Whether a scroll area already claimed this frame's wheel.
    scroll_claimed: bool,
    /// Whether a widget already claimed this frame's press.
    press_claimed: bool,
}

impl<'a> PanelBuilder<'a> {
    /// Start a frame against `state`, using `theme` for metrics.
    ///
    /// The caller has already applied the frame's input with
    /// [`UiInput::begin_frame`](crate::input::UiInput::begin_frame).
    pub fn new(state: &'a mut UiState, theme: &'a Theme) -> PanelBuilder<'a> {
        PanelBuilder {
            input: &state.input,
            hit_rects: &state.hit_rects,
            interaction: &mut state.interaction,
            memory: &mut state.memory,
            theme,
            path: Vec::new(),
            level_counters: HashMap::new(),
            stack: vec![PanelNode::Column {
                children: Vec::new(),
            }],
            focusable: Vec::new(),
            inputs: Vec::new(),
            clipboard: Vec::new(),
            scroll_claimed: false,
            press_claimed: false,
        }
    }

    /// The theme this frame is built for.
    pub fn theme(&self) -> &Theme {
        self.theme
    }

    /// Finish the frame: apply frame-level keys, return the tree and events.
    ///
    /// Escape reports [`PanelInput::Cancelled`]; Tab and Shift+Tab move the
    /// focus through this frame's widgets; a press on the background clears
    /// the focus; a release anywhere ends any stale drag.
    pub fn finish(mut self) -> PanelOutput {
        if self.input.key_pressed(KeyCode::Escape) {
            self.inputs.push(PanelInput::Cancelled);
        }
        if self.input.key_pressed(KeyCode::Tab) {
            self.move_focus(self.input.modifiers().shift);
        }
        if self.input.mouse_button_pressed(MouseButton::Left) && !self.press_claimed {
            self.interaction.focus = None;
        }
        if self.input.mouse_button_released(MouseButton::Left) {
            self.interaction.active = None;
        }
        debug_assert_eq!(self.stack.len(), 1, "every group closes before finish");
        let root = self
            .stack
            .pop()
            .expect("the root column is always open until finish");
        PanelOutput {
            tree: PanelTree {
                root,
                focus_order: std::mem::take(&mut self.focusable),
            },
            inputs: self.inputs,
            clipboard: self.clipboard,
        }
    }

    // =========================================================================
    // Structure
    // =========================================================================

    /// Run `add` inside a named id scope: everything built inside derives
    /// its id from `name`, which keeps state stable when a group moves.
    pub fn scope(&mut self, name: &str, add: impl FnOnce(&mut Self)) {
        self.path.push(name.to_string());
        let saved_counters = std::mem::take(&mut self.level_counters);
        add(self);
        self.level_counters = saved_counters;
        self.path.pop();
    }

    /// Run `add` inside a horizontal group.
    pub fn row(&mut self, add: impl FnOnce(&mut Self)) {
        let segment = self.auto_name("row");
        self.push_group(
            segment,
            PanelNode::Row {
                children: Vec::new(),
            },
            add,
        );
    }

    /// Run `add` inside a horizontal group with a chosen name, for stable
    /// ids when groups move.
    pub fn row_named(&mut self, name: &str, add: impl FnOnce(&mut Self)) {
        self.push_group(
            name.to_string(),
            PanelNode::Row {
                children: Vec::new(),
            },
            add,
        );
    }

    /// Run `add` inside a vertical group.
    pub fn column(&mut self, add: impl FnOnce(&mut Self)) {
        let segment = self.auto_name("column");
        self.push_group(
            segment,
            PanelNode::Column {
                children: Vec::new(),
            },
            add,
        );
    }

    /// Run `add` inside a vertical group with a chosen name.
    pub fn column_named(&mut self, name: &str, add: impl FnOnce(&mut Self)) {
        self.push_group(
            name.to_string(),
            PanelNode::Column {
                children: Vec::new(),
            },
            add,
        );
    }

    // =========================================================================
    // Widgets
    // =========================================================================

    /// Static text.
    pub fn label(&mut self, text: &str) {
        self.add(PanelNode::Label {
            text: text.to_string(),
        });
    }

    /// Section text, drawn larger.
    pub fn heading(&mut self, text: &str) {
        self.add(PanelNode::Heading {
            text: text.to_string(),
        });
    }

    /// A one-pixel horizontal rule.
    pub fn separator(&mut self) {
        self.add(PanelNode::Separator);
    }

    /// A textured rectangle from the whole of `texture`.
    pub fn image(&mut self, name: &str, texture: TextureId, size: Vector2f) {
        self.image_region(name, texture, Vector2f::ZERO, Vector2f::ONE, size);
    }

    /// A textured rectangle from the `uv_min..uv_max` region of `texture`,
    /// at `size` logical pixels.
    pub fn image_region(
        &mut self,
        name: &str,
        texture: TextureId,
        uv_min: Vector2f,
        uv_max: Vector2f,
        size: Vector2f,
    ) {
        let id = self.id_of(name);
        self.add(PanelNode::Image {
            id,
            texture,
            uv_min,
            uv_max,
            size,
            color: Color::WHITE,
        });
    }

    /// A plain coloured rectangle: a theme token's sample, a legend mark.
    pub fn swatch(&mut self, name: &str, color: Color, size: Vector2f) {
        let id = self.id_of(name);
        self.add(PanelNode::Image {
            id,
            texture: TextureId::Solid,
            uv_min: Vector2f::ZERO,
            uv_max: Vector2f::ONE,
            size,
            color,
        });
    }

    /// A push button; whether it was activated by the previous frame's
    /// input.
    pub fn button(&mut self, label: &str) -> bool {
        self.button_enabled(label, true)
    }

    /// A push button that may be disabled.
    pub fn button_enabled(&mut self, label: &str, enabled: bool) -> bool {
        let id = self.id_of(label);
        let frame = self.interact(id, enabled);
        let enter = self.input.key_pressed(KeyCode::Enter);
        let activated = frame.clicked || (frame.focused && enter);
        if activated {
            self.inputs.push(PanelInput::Activated(id));
        }
        self.add(PanelNode::Button {
            id,
            label: label.to_string(),
            enabled,
        });
        activated
    }

    /// A labelled toggle; the value for the caller to keep.
    ///
    /// Clicking, or Enter/Space while focused, flips it. Callers that hold
    /// the value elsewhere assign the returned one.
    pub fn checkbox(&mut self, label: &str, checked: bool) -> bool {
        self.checkbox_enabled(label, checked, true)
    }

    /// A labelled toggle that may be disabled.
    pub fn checkbox_enabled(&mut self, label: &str, checked: bool, enabled: bool) -> bool {
        let id = self.id_of(label);
        let frame = self.interact(id, enabled);
        let keys = self.input.key_pressed(KeyCode::Enter) || self.input.key_pressed(KeyCode::Space);
        let activated = frame.clicked || (frame.focused && keys);
        let value = if activated { !checked } else { checked };
        if activated {
            self.inputs.push(PanelInput::Changed(id));
        }
        self.add(PanelNode::Checkbox {
            id,
            label: label.to_string(),
            checked: value,
            enabled,
        });
        value
    }

    /// A horizontal slider; the value for the caller to keep.
    pub fn slider(&mut self, label: &str, value: f32, range: std::ops::RangeInclusive<f32>) -> f32 {
        self.slider_enabled(label, value, range, true)
    }

    /// A horizontal slider that may be disabled.
    pub fn slider_enabled(
        &mut self,
        label: &str,
        value: f32,
        range: std::ops::RangeInclusive<f32>,
        enabled: bool,
    ) -> f32 {
        let id = self.id_of(label);
        let frame = self.interact(id, enabled);
        let (min, max) = (*range.start(), *range.end());
        let mut value = value.clamp(min, max);
        let mut changed = false;

        // Dragging places the value under the pointer's x within last
        // frame's rectangle.
        if (frame.pressed || frame.held) && max > min {
            if let (Some(rect), Some(pointer)) = (self.rect(id), self.input.pointer()) {
                if rect.width() > 0.0 {
                    let t = ((pointer.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
                    let candidate = min + t * (max - min);
                    if candidate != value {
                        value = candidate;
                        changed = true;
                    }
                }
            }
        }

        // Arrow keys nudge the focused slider by one percent of its range.
        if frame.focused && max > min {
            let step = (max - min) / 100.0;
            let delta = arrow_step_x(self.input, step);
            if delta != 0.0 {
                value = (value + delta).clamp(min, max);
                changed = true;
            }
        }

        if changed {
            self.inputs.push(PanelInput::Changed(id));
        }
        self.add(PanelNode::Slider {
            id,
            label: label.to_string(),
            value,
            min,
            max,
            enabled,
        });
        value
    }

    /// A value field the pointer drags or the arrow keys step; the value for
    /// the caller to keep.
    pub fn drag_value(&mut self, label: &str, value: f32, step: f32) -> f32 {
        self.drag_value_enabled(label, value, step, true)
    }

    /// A value field that may be disabled.
    pub fn drag_value_enabled(&mut self, label: &str, value: f32, step: f32, enabled: bool) -> f32 {
        let id = self.id_of(label);
        let (value, changed) = self.drag(id, value, step, enabled);
        if changed {
            self.inputs.push(PanelInput::Changed(id));
        }
        self.add(PanelNode::DragValue {
            id,
            label: label.to_string(),
            value,
            step,
            enabled,
        });
        value
    }

    /// A single-line text field editing `text` in place; whether it changed.
    ///
    /// The cursor and selection live in [`UiState`] under the field's id,
    /// so they survive frames and tree rebuilds. Ctrl+C/X emit
    /// [`ClipboardRequest::SetText`], Ctrl+V emits
    /// [`ClipboardRequest::GetText`].
    pub fn text_field(&mut self, name: &str, text: &mut String) -> bool {
        self.text_field_enabled(name, text, true)
    }

    /// A text field that may be disabled.
    pub fn text_field_enabled(&mut self, name: &str, text: &mut String, enabled: bool) -> bool {
        let id = self.id_of(name);
        let frame = self.interact(id, enabled);
        let mut changed = false;

        {
            let memory = self.memory.entry(id).or_default();
            let edit = memory
                .text
                .get_or_insert_with(|| TextEditState::at_end(text));
            if frame.focused {
                let modifiers = self.input.modifiers();
                // Presses and repeats in arrival order: held backspace
                // deletes like repeated presses.
                let keys: Vec<KeyCode> = self
                    .input
                    .keys_pressed()
                    .iter()
                    .chain(self.input.keys_repeated().iter())
                    .copied()
                    .collect();
                for key in keys {
                    match key {
                        KeyCode::Backspace => {
                            edit.backspace(text);
                            changed = true;
                        }
                        KeyCode::Delete => {
                            edit.delete(text);
                            changed = true;
                        }
                        KeyCode::ArrowLeft => edit.move_cursor(text, false, modifiers.shift),
                        KeyCode::ArrowRight => edit.move_cursor(text, true, modifiers.shift),
                        KeyCode::Home => edit.move_to_edge(text, false, modifiers.shift),
                        KeyCode::End => edit.move_to_edge(text, true, modifiers.shift),
                        KeyCode::KeyA if modifiers.control => edit.select_all(text),
                        KeyCode::KeyC if modifiers.control => {
                            if let Some(selected) = edit.selected_text(text) {
                                self.clipboard
                                    .push(ClipboardRequest::SetText(selected.to_string()));
                            }
                        }
                        KeyCode::KeyX if modifiers.control => {
                            if let Some(selected) = edit.selected_text(text) {
                                self.clipboard
                                    .push(ClipboardRequest::SetText(selected.to_string()));
                                edit.delete(text);
                                changed = true;
                            }
                        }
                        KeyCode::KeyV if modifiers.control => {
                            self.clipboard.push(ClipboardRequest::GetText);
                        }
                        _ => {}
                    }
                }
                let typed = self.input.text();
                if !typed.is_empty() {
                    edit.insert(text, typed);
                    changed = true;
                }
            }
        }

        if changed {
            self.inputs.push(PanelInput::Changed(id));
        }
        self.add(PanelNode::TextField {
            id,
            text: text.clone(),
            enabled,
        });
        changed
    }

    /// A chooser over `options`, showing `selected`; the index to keep.
    ///
    /// Activating it cycles to the next option; the pop-up list arrives with
    /// the shell's overlay layer, so Stage 8's box is a cycler.
    pub fn combo_box(&mut self, name: &str, selected: usize, options: &[&str]) -> usize {
        self.combo_box_enabled(name, selected, options, true)
    }

    /// A chooser that may be disabled.
    pub fn combo_box_enabled(
        &mut self,
        name: &str,
        selected: usize,
        options: &[&str],
        enabled: bool,
    ) -> usize {
        let id = self.id_of(name);
        let frame = self.interact(id, enabled);
        let enter = self.input.key_pressed(KeyCode::Enter);
        let activated = frame.clicked || (frame.focused && enter);
        let mut selected = if options.is_empty() {
            0
        } else {
            selected.min(options.len() - 1)
        };
        if activated && !options.is_empty() {
            selected = (selected + 1) % options.len();
            self.inputs.push(PanelInput::Changed(id));
        }
        self.add(PanelNode::ComboBox {
            id,
            selected,
            options: options.iter().map(|option| option.to_string()).collect(),
            enabled,
        });
        selected
    }

    /// An RGBA colour editor: a swatch and four component fields; the colour
    /// for the caller to keep.
    pub fn color_field(&mut self, name: &str, color: Color) -> Color {
        self.color_field_enabled(name, color, true)
    }

    /// An RGBA colour editor that may be disabled.
    pub fn color_field_enabled(&mut self, name: &str, color: Color, enabled: bool) -> Color {
        let id = self.id_of(name);
        self.interact(id, enabled);
        let mut color = color;
        let mut changed = false;

        let mut children = Vec::new();
        let components = [
            ("r", color.r * 255.0, 1.0),
            ("g", color.g * 255.0, 1.0),
            ("b", color.b * 255.0, 1.0),
            ("a", color.a, 0.01),
        ];
        for (index, (label, value, step)) in components.iter().enumerate() {
            let component_id = WidgetId::named(id, label);
            let (value, component_changed) = self.drag(component_id, *value, *step, enabled);
            if component_changed {
                match index {
                    0 => color.r = value / 255.0,
                    1 => color.g = value / 255.0,
                    2 => color.b = value / 255.0,
                    _ => color.a = value.clamp(0.0, 1.0),
                }
                changed = true;
            }
            children.push(PanelNode::DragValue {
                id: component_id,
                label: label.to_string(),
                value,
                step: *step,
                enabled,
            });
        }
        if changed {
            self.inputs.push(PanelInput::Changed(id));
        }
        self.add(PanelNode::ColorField {
            id,
            color,
            enabled,
            children,
        });
        color
    }

    /// A 3D vector editor: three drag fields; the value for the caller to
    /// keep.
    pub fn vector_field(&mut self, name: &str, value: Vector3f) -> Vector3f {
        self.vector_field_enabled(name, value, true)
    }

    /// A 3D vector editor that may be disabled.
    pub fn vector_field_enabled(&mut self, name: &str, value: Vector3f, enabled: bool) -> Vector3f {
        let id = self.id_of(name);
        self.interact(id, enabled);
        let mut value = value;
        let step = 0.1;
        let mut children = Vec::new();
        let mut changed = false;
        for (label, component) in [("x", value.x), ("y", value.y), ("z", value.z)] {
            let component_id = WidgetId::named(id, label);
            let (updated, component_changed) = self.drag(component_id, component, step, enabled);
            if component_changed {
                match label {
                    "x" => value.x = updated,
                    "y" => value.y = updated,
                    _ => value.z = updated,
                }
                changed = true;
            }
            children.push(PanelNode::DragValue {
                id: component_id,
                label: label.to_string(),
                value: updated,
                step,
                enabled,
            });
        }
        if changed {
            self.inputs.push(PanelInput::Changed(id));
        }
        self.add(PanelNode::VectorField {
            id,
            value,
            enabled,
            children,
        });
        value
    }

    /// A titled section; `add` runs only while it is open. Whether it is
    /// open, for the caller to keep if it wants to.
    ///
    /// The disclosure state itself lives in [`UiState`] under the id derived
    /// from `title`; `initially_open` is only the first default.
    pub fn collapsing(
        &mut self,
        title: &str,
        initially_open: bool,
        add: impl FnOnce(&mut Self),
    ) -> bool {
        let id = self.id_of(title);
        let frame = self.interact(id, true);
        let enter = self.input.key_pressed(KeyCode::Enter);
        let activated = frame.clicked || (frame.focused && enter);
        let open = {
            let memory = self.memory.entry(id).or_default();
            let open = *memory.open.get_or_insert(initially_open);
            let open = if activated { !open } else { open };
            memory.open = Some(open);
            open
        };
        if activated {
            self.inputs.push(PanelInput::Changed(id));
        }
        let node = PanelNode::Collapsing {
            id,
            title: title.to_string(),
            open,
            children: Vec::new(),
        };
        if open {
            self.push_group(title.to_string(), node, add);
        } else {
            self.add(node);
        }
        open
    }

    /// A clipped, vertically scrollable group `height` pixels tall.
    ///
    /// The content is laid out naturally; the wheel over the area adjusts
    /// the stored offset, which layout clamps to the content height.
    pub fn scroll_area(&mut self, name: &str, height: f32, add: impl FnOnce(&mut Self)) {
        let id = self.id_of(name);
        let node = PanelNode::ScrollArea {
            id,
            height: Some(height),
            children: Vec::new(),
        };
        self.push_group(name.to_string(), node, add);

        // The wheel is claimed by the innermost area under the pointer:
        // children ran before this check, so an inner area has had its turn.
        if !self.scroll_claimed {
            let under_pointer = self
                .rect(id)
                .zip(self.input.pointer())
                .map(|(rect, pointer)| rect.contains(pointer))
                .unwrap_or(false);
            if under_pointer {
                let lines = self.input.scroll_lines();
                let pixels = self.input.scroll_pixels();
                let delta = lines.y * SCROLL_LINE_HEIGHT + pixels.y;
                if delta != 0.0 {
                    let memory = self.memory.entry(id).or_default();
                    memory.scroll = (memory.scroll - delta).max(0.0);
                    self.scroll_claimed = true;
                }
            }
        }
    }

    /// A header row plus uniform data rows; the selected row, if any.
    ///
    /// Clicking a row selects it; Up/Down move the selection while the table
    /// is focused.
    pub fn table(&mut self, name: &str, headers: &[&str], rows: &[&[&str]]) -> Option<usize> {
        let id = self.id_of(name);
        let frame = self.interact(id, true);
        let mut selection = self.memory.entry(id).or_default().selection;
        let mut changed = false;

        if frame.clicked {
            if let (Some(rect), Some(pointer)) = (self.rect(id), self.input.pointer()) {
                let row_height = self.theme.item_height;
                let index = ((pointer.y - rect.min.y) / row_height).floor() - 1.0;
                if index >= 0.0 && (index as usize) < rows.len() {
                    selection = Some(index as usize);
                    changed = true;
                }
            }
        }

        if frame.focused && !rows.is_empty() {
            let delta = arrow_step_y(self.input);
            if delta != 0 {
                let last = rows.len().saturating_sub(1);
                let current = selection.unwrap_or(0) as isize;
                let moved = (current + delta as isize).clamp(0, last as isize) as usize;
                if selection != Some(moved) {
                    selection = Some(moved);
                    changed = true;
                }
            }
        }

        selection = selection.filter(|row| *row < rows.len());
        self.memory.entry(id).or_default().selection = selection;
        if changed {
            self.inputs.push(PanelInput::Changed(id));
        }
        self.add(PanelNode::Table {
            id,
            headers: headers.iter().map(|header| header.to_string()).collect(),
            rows: rows
                .iter()
                .map(|row| row.iter().map(|cell| cell.to_string()).collect())
                .collect(),
            selected: selection,
        });
        selection
    }

    /// A tree view over `nodes`; the selected row's widget id, if any.
    ///
    /// Rows start open; clicking a row selects it and toggles a parent's
    /// disclosure. Up/Down move the selection and Left/Right close and open
    /// the selected row while the tree is focused. Row ids derive from the
    /// path and label, so per-node state survives the tree being rebuilt.
    pub fn tree(&mut self, name: &str, nodes: &[TreeNode]) -> Option<WidgetId> {
        let id = self.id_of(name);
        let enter = self.input.key_pressed(KeyCode::Enter);

        let mut rows = Vec::new();
        self.collect_rows(id, nodes, 0, &mut rows);

        let mut selection = self.memory.entry(id).or_default().selection;
        if let Some(index) = selection {
            if index >= rows.len() {
                selection = None;
            }
        }

        for (index, row) in rows.iter().enumerate() {
            // Rows share the tree's keyboard focus: only one Tab stop.
            let frame = self.interact_as(row.id, id, true);
            let selected = selection == Some(index);
            let activated = frame.clicked || (selected && frame.focused && enter);
            if activated {
                if !selected {
                    selection = Some(index);
                    self.inputs.push(PanelInput::Changed(id));
                }
                self.inputs.push(PanelInput::Activated(row.id));
                if row.has_children {
                    let memory = self.memory.entry(row.id).or_default();
                    let open = !*memory.open.get_or_insert(true);
                    memory.open = Some(open);
                    self.inputs.push(PanelInput::Changed(row.id));
                }
            }
            self.add(PanelNode::TreeRow {
                id: row.id,
                depth: row.depth,
                label: row.label.clone(),
                open: row.open,
                has_children: row.has_children,
                selected,
            });
        }

        if self.interaction.focus == Some(id) && !rows.is_empty() {
            let delta = arrow_step_y(self.input);
            if delta != 0 {
                let last = rows.len() - 1;
                let current = selection.unwrap_or(0) as isize;
                let moved = (current + delta as isize).clamp(0, last as isize) as usize;
                if selection != Some(moved) {
                    selection = Some(moved);
                    self.inputs.push(PanelInput::Changed(id));
                }
            }
            if let Some(index) = selection {
                let row = &rows[index];
                if row.has_children {
                    let open_key = if self.input.key_pressed(KeyCode::ArrowLeft) {
                        Some(false)
                    } else if self.input.key_pressed(KeyCode::ArrowRight) {
                        Some(true)
                    } else {
                        None
                    };
                    if let Some(open) = open_key {
                        let memory = self.memory.entry(row.id).or_default();
                        if *memory.open.get_or_insert(true) != open {
                            memory.open = Some(open);
                            self.inputs.push(PanelInput::Changed(row.id));
                        }
                    }
                }
            }
        }

        self.memory.entry(id).or_default().selection = selection;
        selection.and_then(|index| rows.get(index).map(|row| row.id))
    }

    // =========================================================================
    // Internals
    // =========================================================================

    /// The id of the widget called `name` in the group being built.
    fn id_of(&self, name: &str) -> WidgetId {
        let mut parent = WidgetId::ROOT;
        for segment in &self.path {
            parent = WidgetId::named(parent, segment);
        }
        WidgetId::named(parent, name)
    }

    /// A name unique among widgets of `kind` in the current group.
    fn auto_name(&mut self, kind: &str) -> String {
        let count = self.level_counters.entry(kind.to_string()).or_insert(0);
        let name = format!("{kind} {count}");
        *count += 1;
        name
    }

    /// Append a finished node to the open group.
    fn add(&mut self, node: PanelNode) {
        let parent = self
            .stack
            .last_mut()
            .expect("a group is always open while building");
        match parent {
            PanelNode::Row { children }
            | PanelNode::Column { children }
            | PanelNode::ColorField { children, .. }
            | PanelNode::VectorField { children, .. }
            | PanelNode::Collapsing { children, .. }
            | PanelNode::ScrollArea { children, .. } => children.push(node),
            _ => unreachable!("leaf nodes never become the open group"),
        }
    }

    /// Build a group: push `node` as the open group under the path segment
    /// `segment`, run `add`, pop it back into its parent.
    fn push_group(&mut self, segment: String, node: PanelNode, add: impl FnOnce(&mut Self)) {
        self.path.push(segment);
        self.stack.push(node);
        let saved_counters = std::mem::take(&mut self.level_counters);
        add(self);
        self.level_counters = saved_counters;
        let node = self.stack.pop().expect("the pushed group is open");
        self.path.pop();
        self.add(node);
    }

    /// Decide what this frame's input did with `id`, advancing the drag and
    /// focus state. `id` also takes the keyboard focus when clicked.
    fn interact(&mut self, id: WidgetId, enabled: bool) -> FrameInteraction {
        self.interact_as(id, id, enabled)
    }

    /// Like [`Self::interact`], but clicking gives `focus_target` the
    /// keyboard focus (list rows share their list's focus).
    fn interact_as(
        &mut self,
        id: WidgetId,
        focus_target: WidgetId,
        enabled: bool,
    ) -> FrameInteraction {
        let hovered = enabled
            && self
                .rect(id)
                .zip(self.input.pointer())
                .map(|(rect, pointer)| rect.contains(pointer))
                .unwrap_or(false);
        let pressed = hovered && self.input.mouse_button_pressed(MouseButton::Left);
        if pressed {
            self.interaction.active = Some(id);
            self.interaction.focus = Some(focus_target);
            self.press_claimed = true;
        }
        let released = self.input.mouse_button_released(MouseButton::Left);
        let held =
            self.interaction.active == Some(id) && self.input.mouse_button_held(MouseButton::Left);
        let clicked = self.interaction.active == Some(id) && hovered && released;
        if clicked || (released && self.interaction.active == Some(id)) {
            self.interaction.active = None;
        }
        let focused = enabled && self.interaction.focus == Some(focus_target);
        if enabled && !self.focusable.contains(&focus_target) {
            self.focusable.push(focus_target);
        }
        FrameInteraction {
            pressed,
            held,
            clicked,
            focused,
        }
    }

    /// Where `id` was laid out last frame.
    fn rect(&self, id: WidgetId) -> Option<Rect> {
        self.hit_rects.get(&id).copied()
    }

    /// A drag field's value under `id`: dragging moves by pointer motion,
    /// arrow keys step. Returns the value and whether it changed.
    fn drag(&mut self, id: WidgetId, value: f32, step: f32, enabled: bool) -> (f32, bool) {
        let frame = self.interact(id, enabled);
        let mut value = value;
        let mut changed = false;
        if (frame.pressed || frame.held) && self.interaction.active == Some(id) {
            let motion = self.input.pointer_motion().x;
            if motion != 0.0 {
                value += motion * step;
                changed = true;
            }
        }
        if frame.focused {
            let delta = arrow_step_x(self.input, step);
            if delta != 0.0 {
                value += delta;
                changed = true;
            }
        }
        (value, changed)
    }

    /// Move the keyboard focus to the next (or previous) Tab stop.
    fn move_focus(&mut self, backwards: bool) {
        if self.focusable.is_empty() {
            self.interaction.focus = None;
            return;
        }
        let order = &self.focusable;
        let current = self
            .interaction
            .focus
            .and_then(|focused| order.iter().position(|id| *id == focused));
        let next = match current {
            None if backwards => order.len() - 1,
            None => 0,
            Some(0) if backwards => order.len() - 1,
            Some(index) if backwards => index - 1,
            Some(index) => (index + 1) % order.len(),
        };
        self.interaction.focus = Some(order[next]);
    }

    /// Add `nodes`' visible rows to `out`, depth first, honouring per-row
    /// disclosure state.
    fn collect_rows(
        &mut self,
        parent: WidgetId,
        nodes: &[TreeNode],
        depth: u16,
        out: &mut Vec<TreeRowInfo>,
    ) {
        for (index, node) in nodes.iter().enumerate() {
            let id = WidgetId::named(parent, &format!("{index} {}", node.label));
            let has_children = !node.children.is_empty();
            let open = *self.memory.entry(id).or_default().open.get_or_insert(true);
            out.push(TreeRowInfo {
                id,
                depth,
                label: node.label.clone(),
                open,
                has_children,
            });
            if open && has_children {
                self.collect_rows(id, &node.children, depth + 1, out);
            }
        }
    }
}

/// One visible tree row while the tree is being built.
struct TreeRowInfo {
    /// The row's id.
    id: WidgetId,
    /// How deep the row sits.
    depth: u16,
    /// The row's label.
    label: String,
    /// Whether the row's children are shown.
    open: bool,
    /// Whether the row has children.
    has_children: bool,
}

/// The horizontal arrow step this frame, in `step` units (left is -1).
fn arrow_step_x(input: &UiInput, step: f32) -> f32 {
    let mut presses = 0.0;
    if input.key_pressed(KeyCode::ArrowLeft) || input.key_repeated(KeyCode::ArrowLeft) {
        presses -= 1.0;
    }
    if input.key_pressed(KeyCode::ArrowRight) || input.key_repeated(KeyCode::ArrowRight) {
        presses += 1.0;
    }
    presses * step
}

/// The vertical arrow step this frame (up is -1).
fn arrow_step_y(input: &UiInput) -> i32 {
    let mut presses = 0;
    if input.key_pressed(KeyCode::ArrowUp) || input.key_repeated(KeyCode::ArrowUp) {
        presses -= 1;
    }
    if input.key_pressed(KeyCode::ArrowDown) || input.key_repeated(KeyCode::ArrowDown) {
        presses += 1;
    }
    presses
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::draw::Rect;
    use crate::widget::WidgetId;
    use pill_engine::{ButtonState, InputEvent};

    /// A test rectangle at the origin, big enough to click.
    fn rect_at(y: f32) -> Rect {
        Rect::new(Vector2f::new(0.0, y), Vector2f::new(200.0, y + 20.0))
    }

    /// Run one frame: queue `events`, start the frame, build `panel`.
    fn frame(
        state: &mut UiState,
        events: &[InputEvent],
        panel: impl FnOnce(&mut PanelBuilder),
    ) -> PanelOutput {
        for event in events {
            state.input.apply(event);
        }
        state.input.begin_frame();
        let theme = Theme::default();
        let mut builder = PanelBuilder::new(state, &theme);
        panel(&mut builder);
        builder.finish()
    }

    fn click(button: ButtonState) -> InputEvent {
        InputEvent::MouseButton {
            button: MouseButton::Left,
            state: button,
        }
    }

    fn key(key: KeyCode, state: ButtonState) -> InputEvent {
        InputEvent::Key { key, state }
    }

    fn pointer(x: f32, y: f32) -> InputEvent {
        InputEvent::CursorMoved {
            position: Vector2f::new(x, y),
        }
    }

    #[test]
    fn a_press_and_release_activate_next_frame() {
        let mut state = UiState::new();
        let id = WidgetId::from_name("Bake");

        // Frame 0: lay the button out (no input), then simulate the layout
        // that would have stored its rectangle.
        let mut activated = true;
        frame(&mut state, &[], |ui| {
            activated = ui.button("Bake");
        });
        assert!(!activated);
        state.hit_rects.insert(id, rect_at(0.0));

        // Frame 1: the press lands; the button must not fire yet.
        frame(
            &mut state,
            &[pointer(10.0, 5.0), click(ButtonState::Pressed)],
            |ui| {
                activated = ui.button("Bake");
            },
        );
        assert!(!activated, "a press alone does not activate");

        // Frame 2: the release activates.
        let output = frame(&mut state, &[click(ButtonState::Released)], |ui| {
            activated = ui.button("Bake");
        });
        assert!(activated, "the release in frame 2 activates the button");
        assert_eq!(output.inputs, vec![PanelInput::Activated(id)]);
    }

    #[test]
    fn checkbox_toggles_on_click() {
        let mut state = UiState::new();
        let id = WidgetId::from_name("Enabled");
        frame(&mut state, &[], |ui| {
            ui.checkbox("Enabled", false);
        });
        state.hit_rects.insert(id, rect_at(0.0));

        frame(
            &mut state,
            &[pointer(10.0, 5.0), click(ButtonState::Pressed)],
            |ui| {
                ui.checkbox("Enabled", false);
            },
        );
        let mut value = false;
        let output = frame(&mut state, &[click(ButtonState::Released)], |ui| {
            value = ui.checkbox("Enabled", false);
        });
        assert!(value, "the returned value is the flipped one");
        assert_eq!(output.inputs, vec![PanelInput::Changed(id)]);
    }

    #[test]
    fn text_edits_keep_cursor_state_across_frames() {
        let mut state = UiState::new();
        let id = WidgetId::from_name("Name");
        frame(&mut state, &[], |ui| {
            let mut text = String::from("hello");
            ui.text_field("Name", &mut text);
        });
        state.hit_rects.insert(id, rect_at(0.0));

        // Focus the field with a click.
        frame(
            &mut state,
            &[pointer(10.0, 5.0), click(ButtonState::Pressed)],
            |ui| {
                let mut text = String::from("hello");
                ui.text_field("Name", &mut text);
            },
        );
        frame(&mut state, &[click(ButtonState::Released)], |ui| {
            let mut text = String::from("hello");
            ui.text_field("Name", &mut text);
        });
        assert_eq!(state.focus(), Some(id));

        // Type at the end, then move home and type again: the cursor state
        // from the previous frame is what decides where the text lands.
        let mut text = String::from("hello");
        frame(
            &mut state,
            &[InputEvent::TextInput { text: "!".into() }],
            |ui| {
                ui.text_field("Name", &mut text);
            },
        );
        assert_eq!(text, "hello!");

        frame(
            &mut state,
            &[key(KeyCode::Home, ButtonState::Pressed)],
            |ui| {
                ui.text_field("Name", &mut text);
            },
        );
        frame(
            &mut state,
            &[InputEvent::TextInput { text: ">".into() }],
            |ui| {
                ui.text_field("Name", &mut text);
            },
        );
        assert_eq!(text, ">hello!");
        assert!(state.memory[&id].text.is_some(), "cursor state persists");
    }

    #[test]
    fn tab_moves_the_focus_in_build_order() {
        let mut state = UiState::new();
        let first = WidgetId::from_name("First");
        let second = WidgetId::from_name("Second");
        frame(&mut state, &[], |ui| {
            ui.button("First");
            ui.button("Second");
        });
        state.hit_rects.insert(first, rect_at(0.0));
        state.hit_rects.insert(second, rect_at(40.0));

        frame(
            &mut state,
            &[pointer(10.0, 5.0), click(ButtonState::Pressed)],
            |ui| {
                ui.button("First");
                ui.button("Second");
            },
        );
        assert_eq!(state.focus(), Some(first));

        frame(
            &mut state,
            &[
                key(KeyCode::Tab, ButtonState::Pressed),
                key(KeyCode::Tab, ButtonState::Released),
            ],
            |ui| {
                ui.button("First");
                ui.button("Second");
            },
        );
        assert_eq!(state.focus(), Some(second));

        frame(
            &mut state,
            &[
                key(KeyCode::Tab, ButtonState::Pressed),
                key(KeyCode::Tab, ButtonState::Released),
                InputEvent::ModifiersChanged {
                    modifiers: pill_engine::Modifiers {
                        shift: true,
                        ..Default::default()
                    },
                },
            ],
            |ui| {
                ui.button("First");
                ui.button("Second");
            },
        );
        assert_eq!(state.focus(), Some(first), "Shift+Tab steps back");
    }

    #[test]
    fn enter_activates_the_focused_button() {
        let mut state = UiState::new();
        let id = WidgetId::from_name("Bake");
        frame(&mut state, &[], |ui| {
            ui.button("Bake");
        });
        state.hit_rects.insert(id, rect_at(0.0));
        state.set_focus(id);

        let output = frame(
            &mut state,
            &[key(KeyCode::Enter, ButtonState::Pressed)],
            |ui| {
                ui.button("Bake");
            },
        );
        assert!(output.inputs.contains(&PanelInput::Activated(id)));
    }

    #[test]
    fn escape_reports_cancel() {
        let mut state = UiState::new();
        let output = frame(
            &mut state,
            &[key(KeyCode::Escape, ButtonState::Pressed)],
            |ui| {
                ui.label("anything");
            },
        );
        assert_eq!(output.inputs, vec![PanelInput::Cancelled]);
    }

    #[test]
    fn clipboard_requests_are_plain_data() {
        let mut state = UiState::new();
        let id = WidgetId::from_name("Name");
        frame(&mut state, &[], |ui| {
            let mut text = String::new();
            ui.text_field("Name", &mut text);
        });
        state.hit_rects.insert(id, rect_at(0.0));
        state.set_focus(id);

        let ctrl = pill_engine::Modifiers {
            control: true,
            ..Default::default()
        };
        let mut text = String::from("copy me");
        let output = frame(
            &mut state,
            &[
                InputEvent::ModifiersChanged { modifiers: ctrl },
                key(KeyCode::KeyA, ButtonState::Pressed),
                key(KeyCode::KeyC, ButtonState::Pressed),
                key(KeyCode::KeyV, ButtonState::Pressed),
            ],
            |ui| {
                ui.text_field("Name", &mut text);
            },
        );
        assert_eq!(
            output.clipboard,
            vec![
                ClipboardRequest::SetText("copy me".to_string()),
                ClipboardRequest::GetText,
            ]
        );
        assert_eq!(text, "copy me", "copy does not change the text");
    }

    #[test]
    fn widget_state_survives_rebuilding_the_tree() {
        let mut state = UiState::new();
        let id = WidgetId::from_name("Volume");
        frame(&mut state, &[], |ui| {
            ui.slider("Volume", 0.5, 0.0..=1.0);
        });
        state.hit_rects.insert(id, rect_at(0.0));

        // Press inside the slider, then drag: the value follows the pointer
        // through frames even though the tree is rebuilt every time.
        let mut value = 0.5;
        frame(
            &mut state,
            &[pointer(100.0, 5.0), click(ButtonState::Pressed)],
            |ui| {
                value = ui.slider("Volume", 0.5, 0.0..=1.0);
            },
        );
        frame(&mut state, &[pointer(150.0, 5.0)], |ui| {
            value = ui.slider("Volume", value, 0.0..=1.0);
        });
        assert!((value - 0.75).abs() < 0.01, "value follows the pointer");

        frame(&mut state, &[pointer(190.0, 5.0)], |ui| {
            value = ui.slider("Volume", value, 0.0..=1.0);
        });
        assert!((value - 0.95).abs() < 0.01);
    }

    #[test]
    fn collapsing_remembers_its_disclosure() {
        let mut state = UiState::new();
        let id = WidgetId::from_name("Details");
        frame(&mut state, &[], |ui| {
            ui.collapsing("Details", false, |ui| ui.label("hidden"));
        });
        state.hit_rects.insert(id, rect_at(0.0));

        // Click the header open.
        let mut open = false;
        frame(
            &mut state,
            &[pointer(10.0, 5.0), click(ButtonState::Pressed)],
            |ui| {
                ui.collapsing("Details", false, |ui| ui.label("hidden"));
            },
        );
        frame(&mut state, &[click(ButtonState::Released)], |ui| {
            open = ui.collapsing("Details", false, |ui| ui.label("hidden"));
        });
        assert!(open);

        // A rebuild with the default `false` must still be open: the state,
        // not the default, decides.
        frame(&mut state, &[], |ui| {
            open = ui.collapsing("Details", false, |ui| ui.label("hidden"));
        });
        assert!(open);
    }
}
