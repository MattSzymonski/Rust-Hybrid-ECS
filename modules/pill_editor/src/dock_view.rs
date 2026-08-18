//! Dioxus view layer for the geometry-driven dock workspace.

use dioxus::prelude::*;

use crate::layout::{
    Axis, DockLocation, DropTarget, LayoutAction, LayoutMetrics, LayoutModel, LayoutNode,
    LayoutSnapshot, NodeId, PanelKind, Rect,
};
use crate::{Stats, StatsWidget};

pub(crate) const DOCK_CSS: &str = include_str!("../assets/dock_layout.css");

#[derive(Debug, Clone, Copy, PartialEq)]
enum PointerDrag {
    Splitter {
        row: NodeId,
        index: usize,
        axis: Axis,
        start: f64,
        first_weight: f32,
        second_weight: f32,
        pair_pixels: f64,
    },
    Tab {
        tab: NodeId,
        origin_x: f64,
        origin_y: f64,
        active: bool,
        target: Option<ActiveDrop>,
    },
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum ActiveDrop {
    Insert {
        tabset: NodeId,
        index: usize,
        preview: Rect,
    },
    Dock(DropTarget),
}

impl ActiveDrop {
    fn preview(self) -> Rect {
        match self {
            Self::Insert { preview, .. } => preview,
            Self::Dock(target) => target.preview,
        }
    }
}

/// Render tabset chrome, stable panel hosts, and splitters from one snapshot.
#[component]
pub fn DockView(
    mut model: Signal<LayoutModel>,
    snapshot: LayoutSnapshot,
    stats: Signal<Stats>,
    on_undock: EventHandler<PanelKind>,
) -> Element {
    let mut drag = use_signal(|| None::<PointerDrag>);
    let mut context_menu = use_signal(|| None::<(f64, f64)>);
    let model_value = model.read();
    let move_snapshot = snapshot.clone();
    let active_tabset = model_value.active_tabset.or_else(|| {
        model_value
            .nodes
            .iter()
            .find_map(|(id, node)| matches!(node, LayoutNode::TabSet(_)).then_some(*id))
    });
    let closed_panels = [
        PanelKind::Hierarchy,
        PanelKind::Inspector,
        PanelKind::Console,
        PanelKind::Statistics,
    ]
    .into_iter()
    .filter(|panel| {
        !model_value
            .nodes
            .values()
            .any(|node| matches!(node, LayoutNode::Tab(tab) if tab.panel == *panel))
    })
    .collect::<Vec<_>>();
    let preview = match *drag.read() {
        Some(PointerDrag::Tab {
            active: true,
            target: Some(target),
            ..
        }) => Some(target.preview()),
        _ => None,
    };

    rsx! {
        style { dangerous_inner_html: DOCK_CSS }
        div {
            class: "dock-root",
            onclick: move |_| context_menu.set(None),
            oncontextmenu: move |event| {
                // Suppress the WebView/browser menu and place the editor menu
                // at the pointer in the same logical coordinates as the DOM.
                event.prevent_default();
                let position = event.client_coordinates();
                context_menu.set(Some((position.x, position.y)));
            },
            onpointermove: move |event| {
                let position = event.client_coordinates();
                let Some(state) = *drag.peek() else {
                    return;
                };
                match state {
                    PointerDrag::Splitter {
                        row,
                        index,
                        axis,
                        start,
                        first_weight,
                        second_weight,
                        pair_pixels,
                    } => {
                        let coordinate = match axis {
                            Axis::Horizontal => position.x,
                            Axis::Vertical => position.y,
                        };
                        let pair_weight = first_weight + second_weight;
                        let delta_weight = ((coordinate - start) / pair_pixels.max(1.0)) as f32
                            * pair_weight;
                        let minimum_pixels = match axis {
                            Axis::Horizontal => LayoutMetrics::default().minimum_pane_width,
                            Axis::Vertical => LayoutMetrics::default().minimum_pane_height,
                        };
                        let minimum_weight = (minimum_pixels / pair_pixels.max(1.0)) as f32
                            * pair_weight;
                        let first = (first_weight + delta_weight)
                            .clamp(minimum_weight.min(pair_weight * 0.45), pair_weight * 0.9);
                        let second = pair_weight - first;
                        apply_layout_action(
                            model,
                            LayoutAction::ResizeSplit {
                                row,
                                splitter_index: index,
                                first_weight: first,
                                second_weight: second,
                            },
                            false,
                        );
                    }
                    PointerDrag::Tab {
                        tab,
                        origin_x,
                        origin_y,
                        active,
                        ..
                    } => {
                        let active = active
                            || (position.x - origin_x).hypot(position.y - origin_y) >= 5.0;
                        let target = active
                            .then(|| active_drop_at(&move_snapshot, position.x, position.y))
                            .flatten();
                        drag.set(Some(PointerDrag::Tab {
                            tab,
                            origin_x,
                            origin_y,
                            active,
                            target,
                        }));
                    }
                }
            },
            onpointerup: move |_| {
                let state = *drag.peek();
                drag.set(None);
                if matches!(state, Some(PointerDrag::Splitter { .. })) {
                    crate::layout::save(&model.peek());
                    return;
                }
                let Some(PointerDrag::Tab {
                    tab,
                    active: true,
                    target: Some(target),
                    ..
                }) = state else {
                    return;
                };
                let action = match target {
                    ActiveDrop::Insert {
                        tabset,
                        index,
                        ..
                    } => LayoutAction::MoveTab {
                        tab,
                        target_tabset: tabset,
                        insertion_index: index,
                    },
                    ActiveDrop::Dock(target) if target.location == DockLocation::Center => {
                        let insertion_index = model
                            .peek()
                            .tabset(target.tabset)
                            .map(|tabset| tabset.tabs.len())
                            .unwrap_or(0);
                        LayoutAction::MoveTab {
                            tab,
                            target_tabset: target.tabset,
                            insertion_index,
                        }
                    }
                    ActiveDrop::Dock(target) => LayoutAction::DockTab {
                        tab,
                        target_tabset: target.tabset,
                        location: target.location,
                        ratio: 0.5,
                    },
                };
                apply_layout_action(model, action, true);
            },
            onpointercancel: move |_| drag.set(None),
            onpointerleave: move |_| drag.set(None),
            onkeydown: move |event| {
                if event.data.key().to_string() == "Escape" {
                    drag.set(None);
                    context_menu.set(None);
                }
            },

            for (tab_id, node) in &model_value.nodes {
                if let LayoutNode::Tab(tab) = node {
                    {
                        let rect = snapshot.selected_panel_rects.get(tab_id).copied();
                        let visible = rect.is_some();
                        let rect = rect.unwrap_or_default();
                        let panel_class = if tab.panel == PanelKind::Scene {
                            "dock-panel-host dock-panel-scene"
                        } else {
                            "dock-panel-host dock-checker"
                        };
                        rsx! {
                            div {
                                key: "panel-{tab_id:?}",
                                id: tab_id.dom_id("dock-panel"),
                                class: panel_class,
                                role: "tabpanel",
                                aria_labelledby: tab_id.dom_id("dock-tab"),
                                style: panel_style(rect, visible),
                                PanelContent { panel: tab.panel, stats }
                            }
                        }
                    }
                }
            }

            for (tabset_id, node) in &model_value.nodes {
                if let LayoutNode::TabSet(tabset) = node {
                    if let Some(rect) = snapshot.node_rects.get(tabset_id).copied() {
                        div {
                            key: "tabset-{tabset_id:?}",
                            class: "dock-tabset",
                            style: rect_style(rect),
                            div {
                                class: "dock-tab-strip",
                                role: "tablist",
                                for tab_id in &tabset.tabs {
                                    if let Some(tab) = model_value.tab(*tab_id) {
                                        {
                                            let selected = tabset.selected == Some(*tab_id);
                                            let selected_text = if selected { "true" } else { "false" };
                                            let tab_index = if selected { "0" } else { "-1" };
                                            let selected_tab = *tab_id;
                                            let sibling_tabs = tabset.tabs.clone();
                                            let sibling_index = sibling_tabs
                                                .iter()
                                                .position(|tab| *tab == selected_tab)
                                                .unwrap_or(0);
                                            rsx! {
                                                button {
                                                    key: "tab-{tab_id:?}",
                                                    id: tab_id.dom_id("dock-tab"),
                                                    class: "dock-tab",
                                                    role: "tab",
                                                    aria_selected: selected_text,
                                                    aria_controls: tab_id.dom_id("dock-panel"),
                                                    tabindex: tab_index,
                                                    onpointerdown: move |event| {
                                                        let position = event.client_coordinates();
                                                        drag.set(Some(PointerDrag::Tab {
                                                            tab: selected_tab,
                                                            origin_x: position.x,
                                                            origin_y: position.y,
                                                            active: false,
                                                            target: None,
                                                        }));
                                                    },
                                                    onclick: move |_| {
                                                        apply_layout_action(
                                                            model,
                                                            LayoutAction::SelectTab { tab: selected_tab },
                                                            true,
                                                        );
                                                    },
                                                    onkeydown: move |event| {
                                                        let direction = match event.data.key().to_string().as_str() {
                                                            "ArrowLeft" => -1_isize,
                                                            "ArrowRight" => 1_isize,
                                                            _ => return,
                                                        };
                                                        event.prevent_default();
                                                        let count = sibling_tabs.len() as isize;
                                                        let next = (sibling_index as isize + direction)
                                                            .rem_euclid(count) as usize;
                                                        let next_tab = sibling_tabs[next];
                                                        apply_layout_action(
                                                            model,
                                                            LayoutAction::SelectTab {
                                                                tab: next_tab,
                                                            },
                                                            true,
                                                        );
                                                        let focus_id = next_tab.dom_id("dock-tab");
                                                        spawn(async move {
                                                            let script = format!(
                                                                "document.getElementById('{focus_id}')?.focus();"
                                                            );
                                                            let _ = document::eval(&script).await;
                                                        });
                                                    },
                                                    span { "{tab.title}" }
                                                    {
                                                        let detach_tab = *tab_id;
                                                        let detach_panel = tab.panel;
                                                        rsx! {
                                                            span {
                                                                class: "dock-undock",
                                                                role: "button",
                                                                aria_label: "Open tab in a separate window",
                                                                title: "Open in new window",
                                                                onpointerdown: move |event| {
                                                                    event.stop_propagation();
                                                                },
                                                                onclick: move |event| {
                                                                    event.stop_propagation();
                                                                    if apply_layout_action(
                                                                        model,
                                                                        LayoutAction::DetachTab { tab: detach_tab },
                                                                        false,
                                                                    ) {
                                                                        on_undock.call(detach_panel);
                                                                    }
                                                                },
                                                                "↗"
                                                            }
                                                        }
                                                    }
                                                    if tab.closeable {
                                                        {
                                                            let close_tab = *tab_id;
                                                            rsx! {
                                                                span {
                                                                    class: "dock-close",
                                                                    role: "button",
                                                                    aria_label: "Close tab",
                                                                    onpointerdown: move |event| {
                                                                        event.stop_propagation();
                                                                    },
                                                                    onclick: move |event| {
                                                                        event.stop_propagation();
                                                                        apply_layout_action(
                                                                            model,
                                                                            LayoutAction::CloseTab { tab: close_tab },
                                                                            true,
                                                                        );
                                                                    },
                                                                    "x"
                                                                }
                                                            }
                                                        }
                                                    }
                                                }
                                            }
                                        }
                                    }
                                }
                            }
                        }
                    }
                }
            }

            for splitter in &snapshot.splitters {
                {
                    let orientation_class = match splitter.axis {
                        Axis::Horizontal => "dock-splitter dock-splitter-horizontal",
                        Axis::Vertical => "dock-splitter dock-splitter-vertical",
                    };
                    let orientation = match splitter.axis {
                        Axis::Horizontal => "vertical",
                        Axis::Vertical => "horizontal",
                    };
                    let row = model_value.row(splitter.row).unwrap();
                    let first = &row.children[splitter.index];
                    let second = &row.children[splitter.index + 1];
                    let first_weight = first.weight;
                    let second_weight = second.weight;
                    let first_rect = snapshot.node_rects[&first.node];
                    let second_rect = snapshot.node_rects[&second.node];
                    let pair_pixels = match splitter.axis {
                        Axis::Horizontal => first_rect.width + second_rect.width,
                        Axis::Vertical => first_rect.height + second_rect.height,
                    };
                    let splitter_data = *splitter;
                    rsx! {
                        div {
                            key: "splitter-{splitter:?}",
                            class: orientation_class,
                            role: "separator",
                            aria_orientation: orientation,
                            tabindex: "0",
                            style: rect_style(splitter.rect),
                            onpointerdown: move |event| {
                                event.prevent_default();
                                let position = event.client_coordinates();
                                let start = match splitter_data.axis {
                                    Axis::Horizontal => position.x,
                                    Axis::Vertical => position.y,
                                };
                                drag.set(Some(PointerDrag::Splitter {
                                    row: splitter_data.row,
                                    index: splitter_data.index,
                                    axis: splitter_data.axis,
                                    start,
                                    first_weight,
                                    second_weight,
                                    pair_pixels,
                                }));
                            },
                            onkeydown: move |event| {
                                let delta = match (
                                    splitter_data.axis,
                                    event.data.key().to_string().as_str(),
                                ) {
                                    (Axis::Horizontal, "ArrowLeft")
                                    | (Axis::Vertical, "ArrowUp") => -1.0_f32,
                                    (Axis::Horizontal, "ArrowRight")
                                    | (Axis::Vertical, "ArrowDown") => 1.0_f32,
                                    _ => return,
                                };
                                event.prevent_default();
                                let pair = first_weight + second_weight;
                                let step = pair * 0.02 * delta;
                                let first = (first_weight + step).clamp(pair * 0.1, pair * 0.9);
                                apply_layout_action(
                                    model,
                                    LayoutAction::ResizeSplit {
                                        row: splitter_data.row,
                                        splitter_index: splitter_data.index,
                                        first_weight: first,
                                        second_weight: pair - first,
                                    },
                                    true,
                                );
                            },
                        }
                    }
                }
            }

            if let Some(preview) = preview {
                div {
                    class: "dock-drop-preview",
                    style: rect_style(preview),
                }
            }

            div {
                class: "dock-layout-tools",
                for panel in closed_panels {
                    if let Some(target_tabset) = active_tabset {
                        button {
                            class: "dock-tool-button",
                            onclick: move |_| {
                                apply_layout_action(
                                    model,
                                    LayoutAction::OpenTab {
                                        panel,
                                        target_tabset,
                                    },
                                    true,
                                );
                            },
                            "+ {panel.title()}"
                        }
                    }
                }
                button {
                    class: "dock-tool-button",
                    onclick: move |_| {
                        apply_layout_action(model, LayoutAction::ResetLayout, true);
                    },
                    "Reset Layout"
                }
            }

            if let Some((x, y)) = *context_menu.read() {
                div {
                    class: "dock-context-menu",
                    role: "menu",
                    style: format!("left:{x:.3}px;top:{y:.3}px;"),
                    oncontextmenu: move |event| event.prevent_default(),
                    button {
                        class: "dock-context-menu-item",
                        role: "menuitem",
                        onclick: move |_| context_menu.set(None),
                        "hello1"
                    }
                    button {
                        class: "dock-context-menu-item",
                        role: "menuitem",
                        onclick: move |_| context_menu.set(None),
                        "hello2"
                    }
                }
            }
        }
    }
}

/// Instantiate editor content from its durable panel identity.
#[component]
pub(crate) fn PanelContent(panel: PanelKind, stats: Signal<Stats>) -> Element {
    match panel {
        PanelKind::Scene => rsx! { ViewportFps { stats } },
        PanelKind::Statistics => rsx! { StatsWidget { stats } },
        PanelKind::Hierarchy | PanelKind::Inspector | PanelKind::Console => rsx! {
            div {
                class: "dock-panel-placeholder",
                div { class: "dock-panel-title", "{panel.title()}" }
                div { "Panel content will be added here." }
            }
        },
    }
}

/// Display FPS over the native Scene viewport without subscribing its parent.
#[component]
fn ViewportFps(stats: Signal<Stats>) -> Element {
    let stats = stats.read();

    rsx! {
        div {
            class: "dock-viewport-fps",
            "{stats.fps:.0} FPS"
        }
    }
}

fn rect_style(rect: Rect) -> String {
    format!(
        "left:{:.3}px;top:{:.3}px;width:{:.3}px;height:{:.3}px;",
        rect.x, rect.y, rect.width, rect.height
    )
}

fn panel_style(rect: Rect, visible: bool) -> String {
    if visible {
        rect_style(rect)
    } else {
        "display:none;".to_string()
    }
}

/// Prefer exact tab-strip insertion positions over tabset body docking zones.
fn active_drop_at(snapshot: &LayoutSnapshot, x: f64, y: f64) -> Option<ActiveDrop> {
    if let Some(button) = snapshot
        .tab_buttons
        .iter()
        .find(|button| button.rect.contains(x, y))
    {
        let after = x >= button.rect.x + button.rect.width / 2.0;
        let insertion_x = if after {
            button.rect.x + button.rect.width
        } else {
            button.rect.x
        };
        return Some(ActiveDrop::Insert {
            tabset: button.tabset,
            index: button.index + usize::from(after),
            preview: Rect::new(
                insertion_x - 1.5,
                button.rect.y + 2.0,
                3.0,
                button.rect.height - 4.0,
            ),
        });
    }

    snapshot.drop_target_at(x, y).map(ActiveDrop::Dock)
}

fn apply_layout_action(
    mut model: Signal<LayoutModel>,
    action: LayoutAction,
    persist: bool,
) -> bool {
    let result = {
        let mut model_value = model.write();
        model_value.apply(action)
    };
    match result {
        Ok(_) if persist => {
            crate::layout::save(&model.peek());
            true
        }
        Ok(_) => true,
        Err(error) => {
            eprintln!("[editor] Dock action rejected: {error}");
            false
        }
    }
}
