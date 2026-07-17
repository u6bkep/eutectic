//! Command palette: fuzzy jump-to results plus a data-driven app command registry.

use crate::app::canvas_pane::CamRequest;
use crate::app::pane::{SidebarSection, pane_index};
use crate::app::{EutecticApp, PaneId, ViewKind};
use crate::pick::{LayerId, SemanticId};
use damascene_core::prelude::*;
use eutectic_core::coord::{Nm, Point};
use eutectic_core::doc::{Doc, PinRef};
use eutectic_core::id::NetId;
use eutectic_core::schematic::{Provenance, Shape};

pub(crate) const PALETTE_TOGGLE_KEY: &str = "palette:toggle";
pub(crate) const PALETTE_INPUT_KEY: &str = "palette:input";
const PALETTE_MODAL_KEY: &str = "palette";
const PALETTE_RESULT_PREFIX: &str = "palette:result:";
const PALETTE_MENU_GATE: &str = "__palette_modal_gate";

#[derive(Default)]
pub(crate) struct PaletteUi {
    pub(crate) query: String,
    pub(crate) selection: Selection,
    pub(crate) highlighted: usize,
}

#[derive(Clone)]
enum PaletteAction {
    Jump(SemanticId),
    FitView,
    ToggleLayer(String),
    OpenSection(SidebarSection),
    Save,
}

struct PaletteItem {
    group: &'static str,
    label: String,
    detail: Option<String>,
    shortcut: Option<&'static str>,
    enabled: bool,
    action: PaletteAction,
}

struct PaletteCommand {
    label: String,
    shortcut: Option<&'static str>,
    enabled: bool,
    action: PaletteAction,
}

/// Case-insensitive subsequence matching (`gnd` matches `net G_N_D`).
pub(crate) fn fuzzy_match(query: &str, candidate: &str) -> bool {
    let mut haystack = candidate.chars().flat_map(char::to_lowercase);
    query
        .trim()
        .chars()
        .flat_map(char::to_lowercase)
        .all(|needle| haystack.any(|c| c == needle))
}

impl EutecticApp {
    pub(crate) fn set_palette_open(&self, open: bool) {
        self.palette_open.set(open);
        if open {
            let mut ui = self.palette_ui.borrow_mut();
            ui.query.clear();
            ui.selection = Selection::caret(PALETTE_INPUT_KEY, 0);
            ui.highlighted = 0;
            self.focus_requests
                .borrow_mut()
                .push(PALETTE_INPUT_KEY.to_string());
            self.libraries_open.set(false);
            *self.open_menu.borrow_mut() = Some(PALETTE_MENU_GATE.to_string());
        } else if self.open_menu.borrow().as_deref() == Some(PALETTE_MENU_GATE) {
            *self.open_menu.borrow_mut() = None;
        }
    }

    pub(crate) fn palette_modal(&self) -> El {
        let ui = self.palette_ui.borrow();
        let items = self.palette_items(&ui.query);
        let mut body = vec![
            text_input_with(
                PALETTE_INPUT_KEY,
                &ui.query,
                &ui.selection,
                TextInputOpts::default().placeholder("Type a command or jump to a net, part…"),
            )
            .width(Size::Fill(1.0)),
        ];

        if items.is_empty() {
            body.push(
                text("No matches")
                    .muted()
                    .text_align(TextAlign::Center)
                    .width(Size::Fill(1.0))
                    .padding(Sides::all(tokens::SPACE_4)),
            );
        } else {
            let mut rows = Vec::new();
            let mut group = "";
            for (index, item) in items.iter().enumerate() {
                if item.group != group {
                    group = item.group;
                    rows.push(sidebar_group_label(group));
                }
                let mut label = item.label.clone();
                if let Some(detail) = &item.detail {
                    label.push_str("    ");
                    label.push_str(detail);
                }
                if let Some(shortcut) = item.shortcut {
                    label.push_str("    ");
                    label.push_str(shortcut);
                }
                let row = button(label).width(Size::Fill(1.0));
                let row = if item.enabled {
                    let row = row.key(format!("{PALETTE_RESULT_PREFIX}{index}"));
                    if index == ui.highlighted {
                        row.primary()
                    } else {
                        row
                    }
                } else {
                    row.disabled()
                };
                rows.push(row);
            }
            body.push(
                scroll([column(rows).gap(tokens::SPACE_1)])
                    .height(Size::Fixed(320.0))
                    .scrollbar_gutter()
                    .width(Size::Fill(1.0)),
            );
        }

        overlay([
            scrim(format!("{PALETTE_MODAL_KEY}:dismiss")),
            modal_panel("Command palette", body)
                .width(Size::Fixed(560.0))
                .block_pointer(),
        ])
    }

    pub(crate) fn handle_palette_event(&mut self, event: &UiEvent) -> bool {
        if event.is_hotkey(PALETTE_TOGGLE_KEY) || event.is_click_or_activate(PALETTE_TOGGLE_KEY) {
            self.set_palette_open(!self.palette_open.get());
            return true;
        }
        if !self.palette_open.get() {
            return false;
        }
        if event.kind == UiEventKind::Escape
            || event.is_click_or_activate(&format!("{PALETTE_MODAL_KEY}:dismiss"))
        {
            self.set_palette_open(false);
            return true;
        }

        let items = {
            let ui = self.palette_ui.borrow();
            self.palette_items(&ui.query)
        };
        if event.kind == UiEventKind::KeyDown
            && let Some(key) = event.key_press.as_ref().map(|k| &k.logical)
        {
            match key {
                LogicalKey::Named(NamedKey::ArrowDown) => {
                    self.move_palette_highlight(&items, 1);
                    return true;
                }
                LogicalKey::Named(NamedKey::ArrowUp) => {
                    self.move_palette_highlight(&items, -1);
                    return true;
                }
                LogicalKey::Named(NamedKey::Enter) => {
                    let highlighted = self.palette_ui.borrow().highlighted;
                    if let Some(item) = items.get(highlighted).filter(|item| item.enabled) {
                        self.execute_palette_action(item.action.clone());
                    }
                    return true;
                }
                _ => {}
            }
        }

        let before = self.palette_ui.borrow().query.clone();
        {
            let mut ui = self.palette_ui.borrow_mut();
            let PaletteUi {
                query, selection, ..
            } = &mut *ui;
            if text_input::apply_event(query, selection, event, PALETTE_INPUT_KEY) {
                if *query != before {
                    ui.highlighted = 0;
                }
                return true;
            }
            if let Some(selection) = &event.selection {
                ui.selection = selection.clone();
            }
        }

        if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate)
            && let Some(index) = event
                .route()
                .and_then(|key| key.strip_prefix(PALETTE_RESULT_PREFIX))
                .and_then(|index| index.parse::<usize>().ok())
            && let Some(item) = items.get(index).filter(|item| item.enabled)
        {
            self.execute_palette_action(item.action.clone());
            return true;
        }
        true
    }

    fn move_palette_highlight(&self, items: &[PaletteItem], delta: isize) {
        let enabled: Vec<usize> = items
            .iter()
            .enumerate()
            .filter_map(|(i, item)| item.enabled.then_some(i))
            .collect();
        if enabled.is_empty() {
            self.palette_ui.borrow_mut().highlighted = 0;
            return;
        }
        let current = self.palette_ui.borrow().highlighted;
        let position = enabled.iter().position(|i| *i == current).unwrap_or(0);
        let next = (position as isize + delta).rem_euclid(enabled.len() as isize) as usize;
        self.palette_ui.borrow_mut().highlighted = enabled[next];
    }

    fn palette_items(&self, query: &str) -> Vec<PaletteItem> {
        let mut items = Vec::new();
        let derived = self.derived.borrow();
        for row in &derived.explorer.nets {
            let label = format!("net {}", row.label);
            let detail = format!("{} pads", row.count);
            if fuzzy_match(query, &format!("{label} {detail}")) {
                items.push(PaletteItem {
                    group: "Jump to",
                    label,
                    detail: Some(detail),
                    shortcut: None,
                    enabled: true,
                    action: PaletteAction::Jump(row.id.clone()),
                });
            }
        }
        for row in &derived.explorer.components {
            let label = format!("part {}", row.label);
            let detail = self.palette_component_detail(&row.id, &row.secondary);
            if fuzzy_match(query, &format!("{label} {detail}")) {
                items.push(PaletteItem {
                    group: "Jump to",
                    label,
                    detail: Some(detail),
                    shortcut: None,
                    enabled: true,
                    action: PaletteAction::Jump(row.id.clone()),
                });
            }
        }
        drop(derived);
        for command in self.palette_commands() {
            if fuzzy_match(query, &command.label) {
                items.push(PaletteItem {
                    group: "Commands",
                    label: command.label,
                    detail: None,
                    shortcut: command.shortcut,
                    enabled: command.enabled,
                    action: command.action,
                });
            }
        }
        items
    }

    fn palette_commands(&self) -> Vec<PaletteCommand> {
        let mut commands = vec![PaletteCommand {
            label: "Fit view".to_string(),
            shortcut: Some("F"),
            enabled: true,
            action: PaletteAction::FitView,
        }];
        if let Some(board) = &self.derived.borrow().board {
            for layer in board.layers.iter().rev() {
                if matches!(layer.id, LayerId::Slab(_)) {
                    let key = layer.id.key();
                    commands.push(PaletteCommand {
                        label: format!("Toggle {} visibility", layer.name),
                        shortcut: None,
                        enabled: true,
                        action: PaletteAction::ToggleLayer(key),
                    });
                }
            }
        }
        commands.extend([
            PaletteCommand {
                label: "Open Findings".to_string(),
                shortcut: None,
                enabled: true,
                action: PaletteAction::OpenSection(SidebarSection::Findings),
            },
            PaletteCommand {
                label: "Open Explorer".to_string(),
                shortcut: None,
                enabled: true,
                action: PaletteAction::OpenSection(SidebarSection::Explorer),
            },
            PaletteCommand {
                label: "Save".to_string(),
                shortcut: Some("Ctrl+S"),
                enabled: self.domain.source_path.is_some(),
                action: PaletteAction::Save,
            },
        ]);
        commands
    }

    fn execute_palette_action(&mut self, action: PaletteAction) {
        match action {
            PaletteAction::Jump(id) => self.jump_to(id),
            PaletteAction::FitView => {
                for pane in [PaneId::A, PaneId::B] {
                    self.request_pane_cam(pane, CamRequest::Fit);
                }
            }
            PaletteAction::ToggleLayer(key) => {
                if !self.hidden.borrow_mut().remove(&key) {
                    self.hidden.borrow_mut().insert(key);
                }
                self.style_rev.set(self.style_rev.get() + 1);
            }
            PaletteAction::OpenSection(section) => self.set_section_open(section, true),
            PaletteAction::Save => {
                if self.domain.source_path.is_some() {
                    self.save();
                }
            }
        }
        self.set_palette_open(false);
    }

    fn jump_to(&self, id: SemanticId) {
        self.domain.selection.borrow_mut().select_only(id.clone());
        let focused = self.focused_pane.get();
        let candidates = [
            focused,
            if focused == PaneId::A {
                PaneId::B
            } else {
                PaneId::A
            },
        ];
        for pane in candidates {
            if !self.pane_is_visible(pane) {
                continue;
            }
            let kind = self.panes.borrow()[pane_index(pane)].view;
            let center = match kind {
                ViewKind::Board => self.board_semantic_center(&id),
                ViewKind::Schematic => self.schematic_semantic_center(&id),
            };
            if let Some(center) = center {
                self.pane_center_on(pane, center);
                break;
            }
        }
    }

    fn pane_is_visible(&self, pane: PaneId) -> bool {
        self.maximized
            .get()
            .is_none_or(|maximized| maximized == pane)
    }

    fn board_semantic_center(&self, id: &SemanticId) -> Option<(f64, f64)> {
        let doc = self.domain.doc.as_ref().ok()?;
        if let SemanticId::Part(part) = id {
            let p = doc.components.get(part)?.pos.value;
            return Some((p.x as f64, p.y as f64));
        }
        let derived = self.derived.borrow();
        let candidates = &derived.board.as_ref()?.candidates;
        bbox_center(candidates.iter().filter_map(|candidate| {
            semantic_matches(id, &candidate.id, doc)
                .then(|| candidate.shape.bbox())
                .flatten()
        }))
    }

    fn schematic_semantic_center(&self, id: &SemanticId) -> Option<(f64, f64)> {
        let doc = self.domain.doc.as_ref().ok()?;
        if let SemanticId::Part(part) = id {
            let placement = doc.reflow_schematic(&self.domain.lib).remove(part)?;
            return Some((placement.center.x as f64, placement.center.y as f64));
        }
        let features = eutectic_core::schematic::schematic_features(doc, &self.domain.lib);
        bbox_center(features.features.iter().filter_map(|feature| {
            feature_matches(id, &feature.provenance, doc)
                .then(|| shape_bbox(&feature.shape))
                .flatten()
        }))
    }

    fn palette_component_detail(&self, id: &SemanticId, fallback: &str) -> String {
        let SemanticId::Part(part) = id else {
            return fallback.to_string();
        };
        let Ok(doc) = &self.domain.doc else {
            return fallback.to_string();
        };
        let Some(comp) = doc.components.get(part) else {
            return fallback.to_string();
        };
        let Some(def) = self.domain.lib.get(&comp.part) else {
            return comp.part.clone();
        };
        let value = eutectic_core::annotate::label(
            comp,
            def,
            &eutectic_core::annotate::registry(&doc.source),
        );
        if value == comp.part {
            comp.part.clone()
        } else {
            format!("{} · {value}", comp.part)
        }
    }
}

fn semantic_matches(wanted: &SemanticId, candidate: &SemanticId, doc: &Doc) -> bool {
    if wanted == candidate {
        return true;
    }
    let SemanticId::Net(net) = wanted else {
        return false;
    };
    semantic_net(candidate, doc).is_some_and(|candidate_net| candidate_net == net)
}

fn semantic_net<'a>(id: &SemanticId, doc: &'a Doc) -> Option<&'a NetId> {
    match id {
        SemanticId::Net(net) => doc.nets.get_key_value(net).map(|(net, _)| net),
        SemanticId::Trace(trace) => doc.traces.get(trace).map(|trace| &trace.net),
        SemanticId::Via(via) => doc.vias.get(via).map(|via| &via.net),
        SemanticId::Pour { net, .. } => doc.nets.get_key_value(net).map(|(net, _)| net),
        SemanticId::Pin { comp, pin } => net_of_pin(doc, &PinRef::new(comp, pin)),
        SemanticId::Part(_) => None,
    }
}

fn net_of_pin<'a>(doc: &'a Doc, pin: &PinRef) -> Option<&'a NetId> {
    doc.nets
        .iter()
        .find_map(|(net, data)| data.members.contains(pin).then_some(net))
}

fn feature_matches(wanted: &SemanticId, provenance: &Provenance, doc: &Doc) -> bool {
    match (wanted, provenance) {
        (SemanticId::Part(wanted), Provenance::Component(actual)) => wanted == actual,
        (
            SemanticId::Net(wanted),
            Provenance::Wire {
                net: Some(actual), ..
            },
        )
        | (SemanticId::Net(wanted), Provenance::NetTag { net: actual, .. }) => wanted == actual,
        (SemanticId::Net(wanted), Provenance::Pin { comp, pin }) => {
            net_of_pin(doc, &PinRef::new(comp, pin)).is_some_and(|actual| actual == wanted)
        }
        _ => false,
    }
}

fn shape_bbox(shape: &Shape) -> Option<(Point, Point)> {
    match shape {
        Shape::Polyline { pts, .. } | Shape::Polygon { pts, .. } => points_bbox(pts),
        Shape::Disc { center, radius } => Some((
            Point {
                x: center.x - radius,
                y: center.y - radius,
            },
            Point {
                x: center.x + radius,
                y: center.y + radius,
            },
        )),
        Shape::Text(run) => Some((run.at, run.at)),
    }
}

fn points_bbox(points: &[Point]) -> Option<(Point, Point)> {
    bbox_center_parts(points.iter().map(|point| (*point, *point)))
}

fn bbox_center(bounds: impl Iterator<Item = (Point, Point)>) -> Option<(f64, f64)> {
    let (min, max) = bbox_center_parts(bounds)?;
    Some(((min.x + max.x) as f64 / 2.0, (min.y + max.y) as f64 / 2.0))
}

fn bbox_center_parts(bounds: impl Iterator<Item = (Point, Point)>) -> Option<(Point, Point)> {
    let mut min = Point {
        x: Nm::MAX,
        y: Nm::MAX,
    };
    let mut max = Point {
        x: Nm::MIN,
        y: Nm::MIN,
    };
    let mut any = false;
    for (lo, hi) in bounds {
        min.x = min.x.min(lo.x);
        min.y = min.y.min(lo.y);
        max.x = max.x.max(hi.x);
        max.y = max.y.max(hi.y);
        any = true;
    }
    any.then_some((min, max))
}

#[cfg(test)]
mod tests {
    use super::fuzzy_match;

    #[test]
    fn fuzzy_match_is_case_insensitive_subsequence() {
        assert!(fuzzy_match("gnd", "net G_N_D"));
        assert!(fuzzy_match("fv", "Fit view"));
        assert!(!fuzzy_match("vft", "Fit view"));
    }

    #[test]
    fn fuzzy_match_empty_query_matches_everything() {
        assert!(fuzzy_match("", "anything"));
        assert!(fuzzy_match("   ", "anything"));
    }
}
