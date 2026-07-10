//! The per-pane floating tool strip (UI-oracle anatomy; revised structural
//! commitment 4): a rounded, translucent vertical panel overlaid top-left inside
//! every canvas pane, one icon button per tool the pane's **view kind** offers,
//! thin separators between the oracle's tool groups, and an accent treatment on
//! the kind's active tool. Clicking a button sets THAT KIND's tool slot (all
//! panes of the kind follow — Blender per-view-kind tool memory) and focuses the
//! pane; the routing lives in `app/events.rs` under [`strip_target_of_key`]
//! (`crate::app::pane`).
//!
//! Applicability is structural: a tool a kind doesn't offer (Route on a
//! schematic) simply isn't rendered — no disabled buttons, no checks elsewhere.
//!
//! The panel El is keyed ([`PaneId::strip_panel_key`]) so its background swallows
//! pointer events within its own rect (a click between buttons must not start a
//! route on the canvas underneath); outside that rect events fall through the
//! stack to the canvas viewport, so pan/zoom is never intercepted. The glyphs are
//! app-supplied [`SvgIcon`]s (lucide-shaped strokes, `currentColor`) — the
//! damascene built-in vocabulary has no select/measure/route shapes — tinted
//! through the normal `text_color` channel.

use crate::app::{PaneId, ViewKind};
use crate::tool::Tool;
use damascene_core::prelude::*;
use std::sync::LazyLock;

/// The strip panel's translucent backdrop over the canvas (oracle `bg-2` at
/// ~85 % alpha), as a named token so themes can restyle it.
const STRIP_BG: Color = Color::srgb_token("eutectic.strip.bg", 0x0f, 0x0f, 0x12, 0xd9);

/// Lucide `mouse-pointer-2` — the Select arrow.
static SELECT_GLYPH: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse_current_color(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="m4 4 7.07 17 2.51-7.39L21 11.07z"/></svg>"##,
    )
    .expect("static select glyph parses")
});

/// Lucide `route` — start/end dots joined by an S-curve, the trace-drawing glyph.
static ROUTE_GLYPH: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse_current_color(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><circle cx="6" cy="19" r="3"/><path d="M9 19h8.5a3.5 3.5 0 0 0 0-7h-11a3.5 3.5 0 0 1 0-7H15"/><circle cx="18" cy="5" r="3"/></svg>"##,
    )
    .expect("static route glyph parses")
});

/// Lucide `ruler` — the Measure glyph.
static MEASURE_GLYPH: LazyLock<SvgIcon> = LazyLock::new(|| {
    SvgIcon::parse_current_color(
        r##"<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round" stroke-linejoin="round"><path d="M21.3 15.3a2.4 2.4 0 0 1 0 3.4l-2.6 2.6a2.4 2.4 0 0 1-3.4 0L2.7 8.7a2.4 2.4 0 0 1 0-3.4l2.6-2.6a2.4 2.4 0 0 1 3.4 0Z"/><path d="m14.5 12.5 2-2"/><path d="m11.5 9.5 2-2"/><path d="m8.5 6.5 2-2"/><path d="m17.5 15.5 2-2"/></svg>"##,
    )
    .expect("static measure glyph parses")
});

/// The strip glyph for `tool`.
fn glyph(tool: Tool) -> SvgIcon {
    match tool {
        Tool::Select => SELECT_GLYPH.clone(),
        Tool::Route => ROUTE_GLYPH.clone(),
        Tool::Measure => MEASURE_GLYPH.clone(),
    }
}

/// One strip button: a square icon button keyed to `pane`'s strip slot for
/// `tool`. The active tool gets the oracle's accent treatment — accent-tinted
/// glyph on a low-alpha accent fill with an accent hairline — expressed through
/// the theme's `INFO` token (the oracle's `#3b82f6` accent); inactive buttons
/// are ghosts with muted glyphs. Tooltip = the tool label.
fn strip_button(pane: PaneId, tool: Tool, active: bool) -> El {
    let b = icon_button(glyph(tool))
        .key(pane.strip_key(tool))
        .tooltip(tool.label())
        // Explicit radius = the panel's, so a filled (active) button's corners
        // follow the panel curve (CornerStackup lint invariant; the metrics-
        // resolved icon-button radius is slightly tighter than the panel's).
        .radius(tokens::RADIUS_MD);
    if active {
        b.ghost()
            .fill(tokens::INFO.with_alpha(0.15))
            .stroke(tokens::INFO.with_alpha(0.4))
            .text_color(tokens::INFO)
    } else {
        b.ghost()
    }
}

/// Build the floating tool strip for `pane` showing `view`'s tool groups with
/// `active` highlighted. The returned layer hugs its own rect, so — stacked over
/// the canvas — everything outside the panel falls through to the viewport. The
/// unkeyed outer wrapper only offsets the panel from the pane corner; its
/// padding fringe is click-through (only keyed nodes are hit targets).
pub(crate) fn tool_strip(pane: PaneId, view: ViewKind, active: Tool) -> El {
    let groups = view.strip_groups();
    let mut items: Vec<El> = Vec::new();
    for (i, group) in groups.iter().enumerate() {
        if i > 0 {
            // The oracle's thin separator between tool groups, at glyph width.
            items.push(
                divider()
                    .width(Size::Fixed(18.0))
                    .height(Size::Fixed(1.0))
                    .fill(tokens::BORDER),
            );
        }
        for &tool in group.iter() {
            items.push(strip_button(pane, tool, tool == active));
        }
    }
    let panel = column(items)
        .key(pane.strip_panel_key())
        .align(Align::Center)
        .gap(2.0)
        .padding(Sides::all(tokens::SPACE_1))
        .fill(STRIP_BG)
        .stroke(tokens::BORDER)
        // RADIUS_MD (8): with the SPACE_1 inset the buttons' own rounded corners
        // stay clear of the panel curve (the CornerStackup lint invariant).
        .radius(tokens::RADIUS_MD);
    // Offset the panel from the pane's top-left corner (oracle: 10 px inset).
    column([panel]).padding(Sides::all(tokens::SPACE_2))
}
