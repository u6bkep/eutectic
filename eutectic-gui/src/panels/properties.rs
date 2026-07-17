//! The properties inspector panel: an identity card + key/value rows for the
//! single selected entity, or the doc-stats card when nothing is selected.
//! Moved out of `app/panels.rs` as pure code motion (gui-module-split).

use crate::app::EutecticApp;
use crate::app::domain::DocStats;
use crate::inspector::{InspectorData, Row};
use crate::pick::SemanticId;
use damascene_core::prelude::*;

pub(crate) const POSITION_X_KEY: &str = "properties:position-x";
pub(crate) const POSITION_Y_KEY: &str = "properties:position-y";
pub(crate) const ROTATION_KEY: &str = "properties:rotation";
pub(crate) const TRACE_WIDTH_KEY: &str = "properties:trace-width";
pub(crate) const TRACE_LAYER_KEY: &str = "properties:trace-layer";

const NUMERIC_KEYS: [&str; 4] = [
    POSITION_X_KEY,
    POSITION_Y_KEY,
    ROTATION_KEY,
    TRACE_WIDTH_KEY,
];

/// Raw inspector text and caret ownership. The raw string survives incomplete
/// numeric states while the field is active; committing or reverting clears it
/// so the next build projects the authoritative document value again.
#[derive(Default)]
pub(crate) struct InspectorUi {
    pub(crate) raw: std::collections::BTreeMap<&'static str, String>,
    pub(crate) active: Option<&'static str>,
    pub(crate) subject: Option<SemanticId>,
}

impl EutecticApp {
    /// The Properties accordion body: an identity card + key/value rows for the single
    /// selected entity, or the m2 stats card when nothing is selected. Works regardless
    /// of which pane the selection came from (the selection is shared, semantic). This is
    /// the section content only — the accordion header is composed in `panels::sidebar`.
    pub(crate) fn inspector_body(&self) -> El {
        let doc = match &self.domain.doc {
            Ok(doc) => doc,
            Err(_) => return self.empty_inspector(),
        };
        let id = self.domain.selection.borrow().single().cloned();
        let Some(id) = id else {
            return self.empty_inspector();
        };
        let Some(data) = InspectorData::project(&id, doc, &self.domain.lib) else {
            return self.empty_inspector();
        };

        self.sync_inspector_subject(&id);

        let mut children: Vec<El> =
            vec![column([text(data.kind).muted().mono(), h3(data.primary)]).gap(tokens::SPACE_1)];
        for r in &data.rows {
            children.push(self.inspector_row(&id, r));
        }
        sidebar_group(children).width(Size::Fill(1.0))
    }

    fn inspector_row(&self, id: &SemanticId, row: &Row) -> El {
        match (id, row.key.as_str()) {
            (SemanticId::Part(_), "Position X") => {
                self.numeric_row("Position X", POSITION_X_KEY, &row.value, "mm")
            }
            (SemanticId::Part(_), "Position Y") => {
                self.numeric_row("Position Y", POSITION_Y_KEY, &row.value, "mm")
            }
            (SemanticId::Part(_), "Rotation") => {
                self.numeric_row("Rotation", ROTATION_KEY, &row.value, "°")
            }
            (SemanticId::Trace(_), "Width") => {
                self.numeric_row("Width", TRACE_WIDTH_KEY, &row.value, "mm")
            }
            (SemanticId::Trace(_), "Layer") => {
                field_row("Layer", button(row.value.clone()).key(TRACE_LAYER_KEY))
            }
            _ => field_row(row.key.clone(), text(row.value.clone()).mono()),
        }
    }

    fn numeric_row(&self, label: &str, key: &'static str, value: &str, unit: &str) -> El {
        let ui = self.inspector_ui.borrow();
        let raw = ui.raw.get(key).map(String::as_str).unwrap_or(value);
        let selection = self.lib_ui.borrow().selection.clone();
        field_row(
            label,
            row([
                text_input(key, raw, &selection).width(Size::Fill(1.0)),
                text(unit).muted().mono(),
            ])
            .gap(tokens::SPACE_1)
            .align(Align::Center)
            .width(Size::Fixed(150.0)),
        )
    }

    fn sync_inspector_subject(&self, id: &SemanticId) {
        let mut ui = self.inspector_ui.borrow_mut();
        if ui.subject.as_ref() != Some(id) {
            *ui = InspectorUi {
                subject: Some(id.clone()),
                ..InspectorUi::default()
            };
        }
    }

    /// Fold inspector text events, Enter/Escape, blur, and the trace-layer chip.
    /// Returns true only when the event belongs exclusively to the inspector; a
    /// blur commit returns false so the click that caused it can continue.
    pub(crate) fn handle_inspector_event(&mut self, event: &UiEvent) -> bool {
        let subject = self.domain.selection.borrow().single().cloned();
        let Some(subject) = subject else {
            *self.inspector_ui.borrow_mut() = InspectorUi::default();
            return false;
        };
        self.sync_inspector_subject(&subject);

        if event.is_click_or_activate(TRACE_LAYER_KEY) {
            if let SemanticId::Trace(id) = subject {
                self.cycle_trace_layer(id);
            }
            return true;
        }

        let routed = event.target_key().or_else(|| event.route());
        let target = NUMERIC_KEYS.into_iter().find(|key| routed == Some(*key));
        let active = self.inspector_ui.borrow().active;

        if event.kind == UiEventKind::Escape
            && let Some(active) = active
        {
            self.revert_inspector_raw(active);
            return true;
        }
        if event.kind == UiEventKind::Activate
            && let Some(active) = active
        {
            self.commit_inspector_raw(active);
            return true;
        }

        // Clicking elsewhere is blur: commit the old raw value, then allow the
        // click to proceed (possibly into another inspector field).
        if matches!(event.kind, UiEventKind::PointerDown | UiEventKind::Click)
            && let Some(active) = active
            && target != Some(active)
        {
            self.commit_inspector_raw(active);
        }

        let Some(key) = target
            .or(active
                .filter(|_| matches!(event.kind, UiEventKind::TextInput | UiEventKind::KeyDown)))
        else {
            return false;
        };
        let canonical = self.inspector_canonical(&subject, key);
        let mut ui = self.inspector_ui.borrow_mut();
        ui.active = Some(key);
        ui.raw.entry(key).or_insert(canonical);
        let mut lib_ui = self.lib_ui.borrow_mut();
        let changed = text_input::apply_event(
            ui.raw.get_mut(key).expect("seeded above"),
            &mut lib_ui.selection,
            event,
            key,
        );
        changed || matches!(event.kind, UiEventKind::PointerDown | UiEventKind::Click)
    }

    fn inspector_canonical(&self, subject: &SemanticId, key: &'static str) -> String {
        let Ok(doc) = &self.domain.doc else {
            return String::new();
        };
        let mm = eutectic_core::coord::MM as f64;
        match (subject, key) {
            (SemanticId::Part(id), POSITION_X_KEY) => {
                format!("{:.3}", doc.components[id].pos.value.x as f64 / mm)
            }
            (SemanticId::Part(id), POSITION_Y_KEY) => {
                format!("{:.3}", doc.components[id].pos.value.y as f64 / mm)
            }
            (SemanticId::Part(id), ROTATION_KEY) => {
                let degrees = crate::inspector::rotation_degrees(doc.components[id].orient);
                if (degrees - degrees.round()).abs() < 0.000_000_5 {
                    format!("{degrees:.0}")
                } else {
                    format!("{degrees:.3}")
                }
            }
            (SemanticId::Trace(id), TRACE_WIDTH_KEY) => {
                format!("{:.3}", doc.traces[id].width as f64 / mm)
            }
            _ => String::new(),
        }
    }

    fn revert_inspector_raw(&self, key: &'static str) {
        let mut ui = self.inspector_ui.borrow_mut();
        ui.raw.remove(key);
        ui.active = None;
        self.lib_ui.borrow_mut().selection = Selection::default();
    }

    fn commit_inspector_raw(&mut self, key: &'static str) {
        let (subject, parsed) = {
            let ui = self.inspector_ui.borrow();
            let parsed = ui
                .raw
                .get(key)
                .and_then(|raw| raw.trim().parse::<f64>().ok())
                .filter(|value| value.is_finite());
            (ui.subject.clone(), parsed)
        };
        if let (Some(subject), Some(value)) = (subject, parsed) {
            match (subject, key) {
                (SemanticId::Part(id), POSITION_X_KEY) => {
                    self.set_component_position_mm(&id, Some(value), None)
                }
                (SemanticId::Part(id), POSITION_Y_KEY) => {
                    self.set_component_position_mm(&id, None, Some(value))
                }
                (SemanticId::Part(id), ROTATION_KEY) => self.set_component_rotation_deg(&id, value),
                (SemanticId::Trace(id), TRACE_WIDTH_KEY) => self.set_trace_width_mm(id, value),
                _ => {}
            }
        }
        self.revert_inspector_raw(key);
    }

    #[cfg(test)]
    pub(crate) fn set_inspector_raw(&self, key: &'static str, raw: &str) {
        if let Some(subject) = self.domain.selection.borrow().single().cloned() {
            self.sync_inspector_subject(&subject);
            let mut ui = self.inspector_ui.borrow_mut();
            ui.active = Some(key);
            ui.raw.insert(key, raw.to_string());
        }
    }

    /// The inspector's empty state: the m2 doc stats, rendered as sidebar rows.
    fn empty_inspector(&self) -> El {
        match &self.domain.doc {
            Ok(doc) => {
                let s = DocStats::of(doc);
                let board = match s.board_mm {
                    Some((w, h)) => format!("{w:.1} x {h:.1} mm"),
                    None => "none".to_string(),
                };
                sidebar_group([
                    text("No selection").muted(),
                    field_row("Parts", text(s.parts.to_string()).mono()),
                    field_row("Nets", text(s.nets.to_string()).mono()),
                    field_row("Copper layers", text(s.layers.to_string()).mono()),
                    field_row("Board", text(board).mono()),
                ])
                .width(Size::Fill(1.0))
            }
            Err(_) => sidebar_group([text("No document").muted()]).width(Size::Fill(1.0)),
        }
    }
}
