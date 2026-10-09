//! The UI Gallery: one panel function that shows every widget, in every
//! state, and every theme token.
//!
//! # Responsibilities
//!
//! - [`ui_gallery`]: the sample panel the plan pins down - it exercises every
//!   widget kind (built, laid out, painted and interactive), shows the
//!   disabled state of everything that has one, and renders the theme's
//!   tokens as swatches.
//!
//! # Design
//!
//! The gallery is ordinary panel code: any embedder (the UI probe, the
//! design review, the golden-image tests, the editor shell) calls it inside
//! its own frame. States that depend on input (hover, press, focus,
//! selection) appear as the pointer and Tab move through the gallery, which
//! is what the golden-image scenarios and the design review drive.

// External crates
use pill_core::math::{Vector2f, Vector3f};

// Current crate
use crate::draw::{Color, TextureId};
use crate::panel::{PanelBuilder, TreeNode};

/// Build the gallery into `ui`.
///
/// Layout it into whatever rectangle the caller owns; the sections are
/// disclosure panels so the panel stays usable at any height.
pub fn ui_gallery(ui: &mut PanelBuilder<'_>) {
    ui.heading("Pill UI Gallery");
    ui.label("Every widget in every state, and every theme token.");
    ui.separator();

    ui.collapsing("Buttons", true, |ui| {
        ui.button("Default");
        ui.button_enabled("Disabled", false);
        ui.row(|ui| {
            ui.button("Row A");
            ui.button("Row B");
        });
        ui.label("Hover, press and focus follow the pointer and Tab.");
    });

    ui.collapsing("Text", true, |ui| {
        ui.label("A label at the theme's body size.");
        ui.heading("A heading");
        let mut contents = String::from("editable text");
        ui.text_field("gallery text", &mut contents);
        let mut off = String::from("disabled field");
        ui.text_field_enabled("gallery text off", &mut off, false);
    });

    ui.collapsing("Toggles and values", true, |ui| {
        ui.checkbox("Checkbox, on", true);
        ui.checkbox("Checkbox, off", false);
        ui.checkbox_enabled("Checkbox, disabled", true, false);
        ui.slider("Slider", 0.4, 0.0..=1.0);
        ui.slider_enabled("Slider, disabled", 0.6, 0.0..=1.0, false);
        ui.drag_value("Drag value", 3.0, 1.0);
        ui.drag_value_enabled("Drag value, disabled", 1.5, 0.1, false);
    });

    ui.collapsing("Choice", true, |ui| {
        ui.combo_box("Combo box", 0, &["Translucent", "Opaque", "Cutout"]);
        ui.combo_box_enabled("Combo box, disabled", 1, &["One", "Two"], false);
    });

    ui.collapsing("Colour and vector", true, |ui| {
        ui.color_field("Colour", Color::rgba(0.9, 0.4, 0.2, 1.0));
        ui.color_field_enabled("Colour, disabled", Color::rgba(0.4, 0.4, 0.4, 1.0), false);
        ui.vector_field("Vector", Vector3f::new(1.0, -2.0, 0.5));
        ui.vector_field_enabled("Vector, disabled", Vector3f::ZERO, false);
    });

    ui.collapsing("Sections and scrolling", true, |ui| {
        ui.collapsing("Open section", true, |ui| {
            ui.label("Visible because the section is open.");
        });
        ui.collapsing("Closed section", false, |ui| {
            ui.label("Hidden until the header is clicked.");
        });
        ui.scroll_area("gallery scroll", 72.0, |ui| {
            for index in 0..6 {
                ui.label(&format!("Scrolled line {index}"));
            }
        });
    });

    ui.collapsing("Data", true, |ui| {
        ui.table(
            "gallery table",
            &["Name", "Value", "Kind"],
            &[
                &["speed", "1.5", "float"],
                &["tint", "#ff8844", "colour"],
                &["count", "12", "int"],
            ],
        );
        ui.tree(
            "gallery tree",
            &[
                TreeNode::with_children(
                    "Scene",
                    vec![
                        TreeNode::with_children("Lights", vec![TreeNode::new("Sun")]),
                        TreeNode::new("Camera"),
                    ],
                ),
                TreeNode::new("Settings"),
            ],
        );
    });

    ui.collapsing("Images", true, |ui| {
        ui.row(|ui| {
            ui.image(
                "gallery image",
                TextureId::External(0),
                Vector2f::new(24.0, 24.0),
            );
            ui.image_region(
                "gallery region",
                TextureId::External(0),
                Vector2f::new(0.0, 0.0),
                Vector2f::new(0.5, 0.5),
                Vector2f::new(24.0, 24.0),
            );
        });
    });

    ui.collapsing("Theme tokens", true, |ui| {
        let tokens = ui.theme().color_tokens();
        for (name, color) in tokens {
            ui.row(move |ui| {
                ui.swatch(name, color, Vector2f::new(16.0, 16.0));
                ui.label(name);
            });
        }
    });
}

// =============================================================================
// Tests
// =============================================================================

#[cfg(test)]
mod tests {
    use super::*;
    use crate::atlas::Atlas;
    use crate::draw::DrawList;
    use crate::layout::run_frame;
    use crate::panel::PanelNode;
    use crate::state::UiState;
    use crate::text::TextSystem;
    use crate::theme::Theme;

    /// The kind name of a node, one per widget the gallery must show.
    fn kind(node: &PanelNode) -> Option<&'static str> {
        Some(match node {
            PanelNode::Row { .. } => "row",
            PanelNode::Column { .. } => "column",
            PanelNode::Label { .. } => "label",
            PanelNode::Heading { .. } => "heading",
            PanelNode::Separator => "separator",
            PanelNode::Image { .. } => "image",
            PanelNode::Button { .. } => "button",
            PanelNode::Checkbox { .. } => "checkbox",
            PanelNode::Slider { .. } => "slider",
            PanelNode::DragValue { .. } => "drag value",
            PanelNode::TextField { .. } => "text field",
            PanelNode::ComboBox { .. } => "combo box",
            PanelNode::ColorField { .. } => "colour field",
            PanelNode::VectorField { .. } => "vector field",
            PanelNode::Collapsing { .. } => "collapsing",
            PanelNode::ScrollArea { .. } => "scroll area",
            PanelNode::Table { .. } => "table",
            PanelNode::TreeRow { .. } => "tree view",
        })
    }

    fn collect_kinds(node: &PanelNode, out: &mut Vec<&'static str>) {
        if let Some(name) = kind(node) {
            out.push(name);
        }
        for child in node.children() {
            collect_kinds(child, out);
        }
    }

    /// Render the gallery in one headless frame.
    fn gallery_frame(
        state: &mut UiState,
        text: &mut TextSystem,
    ) -> (
        crate::panel::PanelOutput,
        crate::layout::PanelLayout,
        DrawList,
    ) {
        state.input.begin_frame();
        let theme = Theme::default();
        let (output, layout) = run_frame(state, text, &theme, Vector2f::new(420.0, 2600.0), |ui| {
            ui_gallery(ui);
        });
        let mut atlas = Atlas::new(2048, 2048);
        let mut list = DrawList::new();
        crate::paint::paint(
            &output.tree,
            &layout,
            &theme,
            state,
            text,
            &mut atlas,
            &mut list,
        );
        (output, layout, list)
    }

    #[test]
    fn the_gallery_shows_every_widget_kind() {
        let mut state = UiState::new();
        let mut text = TextSystem::new();
        let (output, _, list) = gallery_frame(&mut state, &mut text);

        let mut kinds = Vec::new();
        collect_kinds(&output.tree.root, &mut kinds);
        for expected in [
            "label",
            "heading",
            "separator",
            "image",
            "button",
            "checkbox",
            "slider",
            "drag value",
            "text field",
            "combo box",
            "colour field",
            "vector field",
            "collapsing",
            "scroll area",
            "table",
            "tree view",
        ] {
            assert!(kinds.contains(&expected), "the gallery shows {expected}");
        }

        // Disabled states are part of the sample: the disabled button's
        // label colour and an enabled one's both appear.
        let theme = Theme::default();
        let has_disabled_text = list
            .vertices
            .iter()
            .any(|vertex| vertex.color == theme.text_disabled);
        assert!(has_disabled_text, "a disabled widget contributed paint");
        assert!(!list.is_empty());
    }

    #[test]
    fn the_gallery_shows_every_theme_token() {
        let mut state = UiState::new();
        let mut text = TextSystem::new();
        let (_, layout, list) = gallery_frame(&mut state, &mut text);

        let theme = Theme::default();
        for (name, color) in theme.color_tokens() {
            let sampled = list.vertices.iter().any(|vertex| vertex.color == color);
            assert!(sampled, "the {name} token has a swatch");
        }
        assert!(layout.interactive.len() > 10, "the gallery is interactive");
    }
}
