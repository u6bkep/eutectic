//! The properties inspector projection (mockup right-sidebar anatomy).
//!
//! Projects the semantic selection into an **identity card** (kind label + primary id)
//! plus **key/value rows**, every value pulled live from the doc / elaborated data —
//! nothing hardcoded. No selection ⇒ the caller shows the m2 stats card (the empty
//! state). This module is a pure projection `SemanticId + Doc → InspectorData`; the
//! El rendering is a thin fold over that, so the projection is unit-testable without a
//! render pass.

use crate::pick::SemanticId;
use eutectic_core::coord::{MM, Nm};
use eutectic_core::doc::{Doc, Orient, PinRef};
use eutectic_core::part::PartLib;

/// One inspector key/value row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Row {
    /// The field label (left column).
    pub key: String,
    /// The formatted value (right column, mono).
    pub value: String,
}

impl Row {
    fn new(key: impl Into<String>, value: impl Into<String>) -> Row {
        Row {
            key: key.into(),
            value: value.into(),
        }
    }
}

/// The projected inspector contents for a selection: an identity card + rows. Pure
/// data — the El builder folds this into widgets. `net` is surfaced separately so the
/// status bar can show the selected net without re-deriving it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InspectorData {
    /// Short kind label for the identity card (e.g. `"Part"`, `"Trace"`, `"Net"`).
    pub kind: String,
    /// The primary id shown large in the identity card (refdes, net name, `T#`, …).
    pub primary: String,
    /// The key/value rows.
    pub rows: Vec<Row>,
    /// The net this selection belongs to (for the status-bar net chip), if any.
    pub net: Option<String>,
}

/// Format one nm coordinate in mm.
fn mm_value(value: Nm) -> String {
    format!("{:.3}", value as f64 / MM as f64)
}

/// Planar quaternion angle in degrees without the integer rounding used by
/// [`Orient::to_deg`]'s compact display projection.
pub(crate) fn rotation_degrees(orient: Orient) -> f64 {
    let (w, x, y, z) = (
        orient.w as f64,
        orient.x as f64,
        orient.y as f64,
        orient.z as f64,
    );
    (2.0 * (w * z + x * y))
        .atan2(w * w + x * x - y * y - z * z)
        .to_degrees()
        .rem_euclid(360.0)
}

fn degrees_value(orient: Orient) -> String {
    let degrees = rotation_degrees(orient);
    if (degrees - degrees.round()).abs() < 0.000_000_5 {
        format!("{degrees:.0}")
    } else {
        format!("{degrees:.3}")
    }
}

/// Format a nm coordinate pair as `x, y` in mm.
fn xy_mm(x: Nm, y: Nm) -> String {
    format!("{}, {} mm", mm_value(x), mm_value(y))
}

/// The polyline length of a trace path in mm (sum of segment lengths, i128 exact
/// before the sqrt).
fn trace_length_mm(path: &[eutectic_core::coord::Point]) -> f64 {
    let mm = MM as f64;
    let mut total = 0.0;
    for w in path.windows(2) {
        let dx = (w[1].x - w[0].x) as i128;
        let dy = (w[1].y - w[0].y) as i128;
        total += ((dx * dx + dy * dy) as f64).sqrt();
    }
    total / mm
}

/// The net a pin belongs to, if any (a forward scan of the doc's nets).
fn pin_net(doc: &Doc, pr: &PinRef) -> Option<String> {
    doc.nets
        .iter()
        .find(|(_, net)| net.members.contains(pr))
        .map(|(nid, _)| nid.to_string())
}

impl InspectorData {
    /// Project `id` into inspector data against the doc + library. `None` when the id
    /// no longer resolves (e.g. a stale selection after re-elaboration) — the caller
    /// then shows the empty state, never a crash.
    pub fn project(id: &SemanticId, doc: &Doc, lib: &PartLib) -> Option<InspectorData> {
        match id {
            SemanticId::Part(eid) => {
                let c = doc.components.get(eid)?;
                let def = lib.get(&c.part);
                let pin_count = def.map(|d| d.pins.len()).unwrap_or(0);
                let mut rows = vec![
                    Row::new("Refdes", eid.as_str()),
                    Row::new("Part", c.part.clone()),
                    Row::new("Position X", mm_value(c.pos.value.x)),
                    Row::new("Position Y", mm_value(c.pos.value.y)),
                    Row::new("Rotation", degrees_value(c.orient)),
                    Row::new("Pins", pin_count.to_string()),
                ];
                // Per-pin net (cheap enough for the small parts in scope): one row per
                // pin, showing its net membership.
                if let Some(def) = def {
                    for pin in &def.pins {
                        let pr = PinRef::new(eid, &pin.number);
                        let net = pin_net(doc, &pr).unwrap_or_else(|| "-".to_string());
                        rows.push(Row::new(format!("  pin {}", pin.name), net));
                    }
                }
                Some(InspectorData {
                    kind: "Part".to_string(),
                    primary: eid.as_str().to_string(),
                    rows,
                    net: None,
                })
            }
            SemanticId::Trace(tid) => {
                let t = doc.traces.get(tid)?;
                let rows = vec![
                    Row::new("Net", t.net.to_string()),
                    Row::new("Layer", t.layer.clone()),
                    Row::new("Width", mm_value(t.width)),
                    Row::new("Length", format!("{:.3} mm", trace_length_mm(&t.path))),
                    Row::new("Vertices", t.path.len().to_string()),
                ];
                Some(InspectorData {
                    kind: "Trace".to_string(),
                    primary: format!("T{}", tid.0),
                    rows,
                    net: Some(t.net.to_string()),
                })
            }
            SemanticId::Via(vid) => {
                let v = doc.vias.get(vid)?;
                let span = match &v.span {
                    Some((a, b)) => format!("{a} - {b}"),
                    None => "through".to_string(),
                };
                let rows = vec![
                    Row::new("Net", v.net.to_string()),
                    Row::new("At", xy_mm(v.at.x, v.at.y)),
                    Row::new("Drill", format!("{:.3} mm", v.drill as f64 / MM as f64)),
                    Row::new("Pad", format!("{:.3} mm", v.pad as f64 / MM as f64)),
                    Row::new("Span", span),
                ];
                Some(InspectorData {
                    kind: "Via".to_string(),
                    primary: format!("V{}", vid.0),
                    rows,
                    net: Some(v.net.to_string()),
                })
            }
            SemanticId::Pour { net, layer } => {
                // Member count = pins on this net (the pour's connectivity reach).
                let members = doc.nets.get(net).map(|n| n.members.len()).unwrap_or(0);
                let rows = vec![
                    Row::new("Net", net.to_string()),
                    Row::new("Layer", layer.clone()),
                    Row::new("Net members", members.to_string()),
                ];
                Some(InspectorData {
                    kind: "Pour".to_string(),
                    primary: net.to_string(),
                    rows,
                    net: Some(net.to_string()),
                })
            }
            SemanticId::Pin { comp, pin } => {
                // `pin` is the pad *number* (the `PinRef` / net-membership join key), so
                // this lookup keys nets correctly. The functional name is a derived label:
                // find the `PinDef` by number and show its name when it differs.
                let c = doc.components.get(comp)?;
                let pr = PinRef::new(comp, pin);
                let net = pin_net(doc, &pr);
                let name = lib
                    .get(&c.part)
                    .and_then(|d| d.pins.iter().find(|p| &p.number == pin))
                    .map(|p| p.name.clone());
                let pin_label = match &name {
                    Some(n) if n != pin => format!("{pin} ({n})"),
                    _ => pin.clone(),
                };
                let rows = vec![
                    Row::new("Component", comp.as_str()),
                    Row::new("Pin", pin_label),
                    Row::new("Part", c.part.clone()),
                    Row::new(
                        "Net",
                        net.clone().unwrap_or_else(|| "(unconnected)".to_string()),
                    ),
                ];
                Some(InspectorData {
                    kind: "Pin".to_string(),
                    primary: format!("{}.{}", comp.as_str(), pin),
                    rows,
                    net,
                })
            }
            SemanticId::Net(nid) => {
                let n = doc.nets.get(nid)?;
                let rows = vec![
                    Row::new("Net", nid.to_string()),
                    Row::new("Members", n.members.len().to_string()),
                ];
                Some(InspectorData {
                    kind: "Net".to_string(),
                    primary: nid.to_string(),
                    rows,
                    net: Some(nid.to_string()),
                })
            }
        }
    }
}
