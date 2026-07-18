//! The application menu bar (UI oracle, region 1): the top strip carrying the
//! `File / Edit / View / Place / Route / Inspect / Tools / Help` menus on the
//! left and the document status cluster on the right (filename + dirty dot,
//! per-source findings chips, reload-error / edit-error chips — moved here from
//! the toolbar).
//!
//! Built on damascene's [`menubar`](damascene_core::menubar) widget: the row
//! carries a [`menubar_trigger`] per menu, and the open menu's rows render as a
//! root-level anchored popover ([`menu_overlay`](EutecticApp::menu_overlay), stacked
//! by `build`). The open-menu slot lives in [`EutecticApp::open_menu`] and is folded
//! by [`menubar::apply_event`](damascene_core::menubar::apply_event) in
//! `on_event`.
//!
//! ## Wired vs disabled
//!
//! The row enumeration is the oracle's in full (absence is not allowed). Rows
//! backed by real app/engine behavior are keyed to their existing action routes:
//! open/recent/save/revert/history, deterministic exports, focused zoom,
//! units/grid, Findings, Libraries, Delete/Rotate, autoroute, Quit, and the
//! Help dialogs. Everything else renders as a visible-but-inert
//! [`disabled`](damascene_core::prelude) row (muted, unfocusable, no route).
//!
//! Save / Revert additionally require a source path (the m6 save model — an
//! in-memory doc has nowhere to write / re-read); without one they render
//! disabled, exactly like the toolbar's Save affordance.

use crate::app::EutecticApp;
use crate::app::autoroute::{AUTOROUTE_BOARD_KEY, AUTOROUTE_NET_KEY};
use crate::app::libraries::LIBRARIES_TOGGLE_KEY;
use crate::app::open::{OPEN_KEY, OPEN_RECENT_KEY, RECENT_POPOVER_KEY, recent_item_key};
use crate::app::pane::{REDO_KEY, SAVE_KEY, UNDO_KEY, findings_chip_key};
use crate::chrome::actions::{
    EXPORT_GERBERS_KEY, EXPORT_SVG_KEY, FINDINGS_PANEL_KEY, GRID_TOGGLE_KEY, QUIT_KEY,
    UNITS_TOGGLE_KEY, ZOOM_IN_KEY, ZOOM_OUT_KEY,
};
use crate::chrome::dialogs::{ABOUT_KEY, KEYMAP_KEY};
use crate::findings::FindingSource;
use damascene_core::prelude::*;
use eutectic_core::diagnostic::Severity;

/// The controlled-menubar key every trigger / dismiss route is namespaced under
/// (`"menubar:menu:file"`, `"menubar:menu:file:dismiss"`, …). Folded by
/// [`menubar::apply_event`](damascene_core::menubar::apply_event).
pub(crate) const MENUBAR_KEY: &str = "menubar";

/// The `View ▸ Fit` route key + the toolbar's fit icon key — fit every pane's
/// camera (the same action the old toolbar `Fit` button dispatched).
pub(crate) const FIT_KEY: &str = "fit";

/// The `File ▸ Revert to Saved` route key — reload the document from disk,
/// discarding in-memory edits ([`EutecticApp::revert_to_saved`]).
pub(crate) const REVERT_KEY: &str = "revert";

/// Edit ▸ Delete and the bare Delete chord route.
pub(crate) const DELETE_KEY: &str = "delete";

/// Edit ▸ Rotate and the bare R chord route.
pub(crate) const ROTATE_KEY: &str = "rotate";

/// View ▸ Snap to Grid's app-wide toggle route.
pub(crate) const SNAP_TO_GRID_KEY: &str = "display:snap-to-grid:toggle";

/// Place ▸ Part from Library… — select the board Place tool and open its
/// library-browser palette (the same action as the strip's Place button).
pub(crate) const PLACE_PART_KEY: &str = "place-part";

/// One menu row. The enumeration is the oracle's; `Wired` rows carry the existing
/// route key they dispatch to (so a click routes exactly like the retired
/// toolbar button did), `Disabled` rows are visible-but-inert, `Separator` is a
/// divider.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum MenuRow {
    /// A divider between row groups.
    Separator,
    /// A live row: label, optional shortcut hint, and the on-event route key it
    /// dispatches to.
    Wired {
        /// Display label.
        label: &'static str,
        /// Trailing keyboard-shortcut hint (shown only where a real chord is
        /// wired — Ctrl+O / Ctrl+S / Ctrl+Z / Ctrl+Shift+Z).
        shortcut: Option<&'static str>,
        /// The route key this row emits (an existing `on_event` action).
        action: &'static str,
    },
    /// A live submenu parent: it has an action route and the oracle's trailing arrow,
    /// but no keyboard-shortcut hint.
    Submenu {
        /// Display label.
        label: &'static str,
        /// The route that opens the nested popover.
        action: &'static str,
    },
    /// A visible-but-inert row (functionality not in this slice): label, the
    /// oracle's shortcut/keystroke hint (inert documentation), and whether it is
    /// a submenu (`arrow`, shown with a trailing chevron).
    Disabled {
        /// Display label.
        label: &'static str,
        /// The oracle's keystroke hint, rendered inert.
        shortcut: Option<&'static str>,
        /// Submenu indicator (Open Recent, Active Layer).
        arrow: bool,
    },
}

/// One top-level menu: its value token (lowercase, used in the routed trigger
/// key), display label, and rows.
pub(crate) struct MenuDef {
    /// Lowercase token in `menubar:menu:{value}` and the [`EutecticApp::open_menu`] slot.
    pub value: &'static str,
    /// Display label on the trigger.
    pub label: &'static str,
    /// The menu's rows, top to bottom.
    pub rows: Vec<MenuRow>,
}

/// The full menu enumeration (the oracle's `buildMenus`, region-for-region). The
/// wired subset dispatches to existing actions; everything else is a disabled
/// row. This is a pure data function so the menu model can be unit-tested without
/// a render.
pub(crate) fn menu_defs() -> Vec<MenuDef> {
    use MenuRow::{Disabled as Dis, Separator as Sep, Submenu, Wired};
    // Shorthand constructors.
    let dis = |label, shortcut| Dis {
        label,
        shortcut,
        arrow: false,
    };
    let arrow = |label| Dis {
        label,
        shortcut: None,
        arrow: true,
    };
    vec![
        MenuDef {
            value: "file",
            label: "File",
            rows: vec![
                Wired {
                    label: "Open…",
                    shortcut: Some("Ctrl+O"),
                    action: OPEN_KEY,
                },
                Submenu {
                    label: "Open Recent",
                    action: OPEN_RECENT_KEY,
                },
                Wired {
                    label: "Save",
                    shortcut: Some("Ctrl+S"),
                    action: SAVE_KEY,
                },
                Wired {
                    label: "Revert to Saved",
                    shortcut: None,
                    action: REVERT_KEY,
                },
                Sep,
                Wired {
                    label: "Export Gerbers…",
                    shortcut: None,
                    action: EXPORT_GERBERS_KEY,
                },
                Wired {
                    label: "Export SVG…",
                    shortcut: None,
                    action: EXPORT_SVG_KEY,
                },
                Sep,
                Wired {
                    label: "Libraries…",
                    shortcut: None,
                    action: LIBRARIES_TOGGLE_KEY,
                },
                Sep,
                Wired {
                    label: "Quit",
                    shortcut: None,
                    action: QUIT_KEY,
                },
            ],
        },
        MenuDef {
            value: "edit",
            label: "Edit",
            rows: vec![
                Wired {
                    label: "Undo",
                    shortcut: Some("Ctrl+Z"),
                    action: UNDO_KEY,
                },
                Wired {
                    label: "Redo",
                    shortcut: Some("Ctrl+Shift+Z"),
                    action: REDO_KEY,
                },
                Sep,
                Wired {
                    label: "Delete",
                    shortcut: Some("Del"),
                    action: DELETE_KEY,
                },
                Wired {
                    label: "Rotate",
                    shortcut: Some("R"),
                    action: ROTATE_KEY,
                },
                Sep,
                dis("Copy", None),
                dis("Paste", None),
                Sep,
                dis("Command Palette…", Some("Ctrl+K")),
            ],
        },
        MenuDef {
            value: "view",
            label: "View",
            rows: vec![
                dis("Split Right", None),
                dis("Split Down", None),
                dis("Close Pane", None),
                dis("Pop Out Pane", Some("roadmap")),
                Sep,
                Wired {
                    label: "Fit",
                    shortcut: None,
                    action: FIT_KEY,
                },
                Wired {
                    label: "Zoom In",
                    shortcut: Some("Ctrl++ / Ctrl+="),
                    action: ZOOM_IN_KEY,
                },
                Wired {
                    label: "Zoom Out",
                    shortcut: Some("Ctrl+-"),
                    action: ZOOM_OUT_KEY,
                },
                Sep,
                dis("Flip Board (bottom view)", None),
                Wired {
                    label: "Grid: dots / lines",
                    shortcut: None,
                    action: GRID_TOGGLE_KEY,
                },
                Wired {
                    label: "Snap to Grid",
                    shortcut: None,
                    action: SNAP_TO_GRID_KEY,
                },
                Wired {
                    label: "Units: mm / in",
                    shortcut: None,
                    action: UNITS_TOGGLE_KEY,
                },
                Sep,
                Wired {
                    label: "Findings Panel",
                    shortcut: None,
                    action: FINDINGS_PANEL_KEY,
                },
            ],
        },
        MenuDef {
            value: "place",
            label: "Place",
            rows: vec![
                Wired {
                    label: "Part from Library…",
                    shortcut: None,
                    action: PLACE_PART_KEY,
                },
                dis("Wire", Some("W")),
                dis("Net Label", Some("L")),
                dis("Power Symbol", None),
                Sep,
                dis("Def Instance…", None),
            ],
        },
        MenuDef {
            value: "route",
            label: "Route",
            rows: vec![
                dis("Route Trace", Some("X")),
                dis("Place Via", Some("V")),
                dis("Copper Pour", None),
                Sep,
                arrow("Active Layer"),
                Sep,
                Wired {
                    label: "Autoroute Net",
                    shortcut: None,
                    action: AUTOROUTE_NET_KEY,
                },
                Wired {
                    label: "Autoroute Board",
                    shortcut: None,
                    action: AUTOROUTE_BOARD_KEY,
                },
            ],
        },
        MenuDef {
            value: "inspect",
            label: "Inspect",
            rows: vec![
                dis("Findings", None),
                dis("Measure", Some("M")),
                dis("Dimension", None),
                Sep,
                dis("Net Explorer", None),
                dis("Board Statistics", None),
            ],
        },
        MenuDef {
            value: "tools",
            label: "Tools",
            rows: vec![
                Wired {
                    label: "Libraries…",
                    shortcut: None,
                    action: LIBRARIES_TOGGLE_KEY,
                },
                dis("Command Palette…", Some("Ctrl+K")),
                Sep,
                dis("Preferences…", None),
            ],
        },
        MenuDef {
            value: "help",
            label: "Help",
            rows: vec![
                Wired {
                    label: "Keymap",
                    shortcut: None,
                    action: KEYMAP_KEY,
                },
                Wired {
                    label: "About eutectic",
                    shortcut: None,
                    action: ABOUT_KEY,
                },
            ],
        },
    ]
}

impl EutecticApp {
    /// The menu-bar strip (oracle region 1): the eight menu triggers on the left,
    /// then the document status cluster on the right — filename + dirty dot, the
    /// per-source findings chips, and the reload / edit-error chips. Full width so
    /// it reads as one top bar; the open menu itself renders as a root overlay
    /// ([`menu_overlay`](Self::menu_overlay)).
    pub(crate) fn menubar_bar(&self) -> El {
        let open = self.open_menu.borrow();
        let mut items: Vec<El> = menu_defs()
            .iter()
            .map(|d| {
                menubar_trigger(
                    MENUBAR_KEY,
                    d.value,
                    d.label,
                    open.as_deref() == Some(d.value),
                )
            })
            .collect();
        items.push(spacer());
        items.push(self.menubar_status_cluster());
        toolbar(items)
            .gap(tokens::SPACE_1)
            .padding(Sides::xy(tokens::SPACE_3, tokens::SPACE_1))
            .width(Size::Fill(1.0))
    }

    /// The right-hand status cluster of the menu bar: the filename badge (dirty-dot
    /// suffixed), the per-source findings chips, and the persistent reload-error /
    /// edit-error chips (moved here from the toolbar).
    fn menubar_status_cluster(&self) -> El {
        let mut name = self
            .domain
            .filename
            .clone()
            .unwrap_or_else(|| "untitled".into());
        // The dirty marker (m6): commits not yet written to the file show as a
        // bullet on the filename badge, cleared by Save / external reload.
        if self.dirty() {
            name.push_str(" •");
        }
        let mut cluster: Vec<El> = vec![badge(name).info()];
        cluster.extend(self.findings_chips());
        if let Some(err) = &self.domain.reload_error {
            cluster.push(reload_error_chip(err));
        }
        // The save/commit failure chip (m6): persists until the next success.
        if let Some(err) = &self.domain.edit.error {
            let first = err.lines().next().unwrap_or(err);
            cluster.push(badge(format!("edit failed: {first}")).destructive());
        }
        if let Some(notice) = self.chrome_notice.borrow().as_ref() {
            let chip = badge(notice.message.clone());
            cluster.push(if notice.error {
                chip.destructive()
            } else {
                chip.success()
            });
        }
        row(cluster).gap(tokens::SPACE_2).align(Align::Center)
    }

    /// The open menu's anchored popover, or `None` when every menu is closed.
    /// Rendered as a root-level overlay by `build` (stacked over the viewer, like
    /// the Libraries modal) so it escapes the menu bar's own clip and anchors
    /// below its trigger.
    pub(crate) fn menu_overlay(&self) -> Option<El> {
        let open = self.open_menu.borrow();
        let value = open.as_deref()?;
        let def = menu_defs().into_iter().find(|d| d.value == value)?;
        // Save / Revert need a source path to act on (the m6 save model).
        let has_path = self.domain.source_path.is_some();
        let focused_kind =
            self.panes.borrow()[crate::app::pane::pane_index(self.focused_pane.get())].view;
        let board_focused = focused_kind == crate::app::ViewKind::Board;
        let can_delete = board_focused && self.can_delete_selection();
        let can_rotate = board_focused && self.can_rotate_selection();
        let can_autoroute_net = self.can_autoroute_selection();
        let has_doc = self.domain.doc.is_ok();
        let availability = MenuAvailability {
            has_path,
            can_delete,
            can_rotate,
            can_autoroute_net,
            has_doc,
        };
        let rows: Vec<El> = def
            .rows
            .iter()
            .map(|r| {
                menu_row_el(
                    r,
                    self.display_units().label(),
                    self.grid_style().label(),
                    self.snap_to_grid(),
                    availability,
                )
            })
            .collect();
        Some(menubar_menu(MENUBAR_KEY, value, rows))
    }

    /// The nested File ▸ Open Recent menu. Damascene's menubar surface is a
    /// single-level primitive, so the submenu is a second stock popover
    /// anchored to the keyed parent row.
    pub(crate) fn recent_menu_overlay(&self) -> Option<El> {
        if !self.recent_open.get() || self.open_menu.borrow().as_deref() != Some("file") {
            return None;
        }
        let rows: Vec<El> = if self.recents.borrow().paths().is_empty() {
            vec![menu_item("(none)", None).disabled()]
        } else {
            self.recents
                .borrow()
                .paths()
                .iter()
                .enumerate()
                .map(|(index, path)| {
                    menu_item(&path.display().to_string(), None).key(recent_item_key(index))
                })
                .collect()
        };
        Some(popover(
            RECENT_POPOVER_KEY,
            Anchor::right_of_key(OPEN_RECENT_KEY),
            menubar_content(rows),
        ))
    }

    /// The per-source findings chips (oracle menu-bar chrome): one chip per
    /// [`FindingSource`] that has findings this revision, in DRC/ERC/NET/LIB order,
    /// each reading `"NAME n"` (n = total findings for that source) and tinted by the
    /// source's worst severity — red (`destructive`) if any error, amber (`warning`)
    /// otherwise, both through the theme's semantic colors. When every source is clean
    /// a single neutral `"✓"` chip is shown instead. Every chip (including the ✓ one)
    /// is a click-to-toggle-the-findings-panel affordance keyed distinctly. Reads the
    /// cached findings — never recomputes.
    pub(crate) fn findings_chips(&self) -> Vec<El> {
        let findings = &self.derived.borrow().findings;
        // A clickable chip: keyed + focusable + pointer cursor, so a click routes to the
        // app (handled as a findings-panel toggle) exactly like the panel's Hide/Show.
        let chip = |label: String, tag: &str| {
            badge(label)
                .key(findings_chip_key(tag))
                .focusable()
                .cursor(Cursor::Pointer)
        };
        let mut chips: Vec<El> = Vec::new();
        for source in FindingSource::all() {
            let Some((count, worst)) = findings.source_summary(source) else {
                continue;
            };
            let c = chip(format!("{} {count}", source.label()), source.label());
            chips.push(match worst {
                Severity::Error => c.destructive(),
                _ => c.warning(),
            });
        }
        if chips.is_empty() {
            // All sources clean → a single neutral ✓ chip, still click-to-toggle.
            chips.push(chip("✓".to_string(), "ok").muted());
        }
        chips
    }
}

/// Availability inputs shared by menu-row rendering.
#[derive(Clone, Copy)]
struct MenuAvailability {
    has_path: bool,
    can_delete: bool,
    can_rotate: bool,
    can_autoroute_net: bool,
    has_doc: bool,
}

/// Render one [`MenuRow`] into a menu-panel El. Wired rows carry their route key;
/// Save / Revert downgrade to disabled without a source path; disabled rows are
/// muted + inert (no key), submenu rows carry a trailing chevron.
fn menu_row_el(
    row: &MenuRow,
    units_label: &'static str,
    grid_label: &'static str,
    snap_to_grid: bool,
    available: MenuAvailability,
) -> El {
    match *row {
        MenuRow::Separator => menubar_separator(),
        MenuRow::Wired {
            label,
            shortcut,
            action,
        } => {
            // Save / Revert are only actionable with a file to write / re-read.
            let unavailable = matches!(
                action,
                SAVE_KEY | REVERT_KEY | EXPORT_GERBERS_KEY | EXPORT_SVG_KEY
            ) && !available.has_path
                || (action == DELETE_KEY && !available.can_delete)
                || (action == ROTATE_KEY && !available.can_rotate)
                || (action == AUTOROUTE_NET_KEY && !available.can_autoroute_net)
                || (action == AUTOROUTE_BOARD_KEY && !available.has_doc);
            let trailing = match action {
                UNITS_TOGGLE_KEY => Some(units_label),
                GRID_TOGGLE_KEY => Some(grid_label),
                SNAP_TO_GRID_KEY if snap_to_grid => Some("✓"),
                _ => shortcut,
            };
            let el = menu_item(label, trailing);
            if unavailable {
                el.disabled()
            } else {
                el.key(action)
            }
        }
        MenuRow::Submenu { label, action } => submenu_item(label).key(action),
        MenuRow::Disabled {
            label,
            shortcut,
            arrow,
        } => {
            if arrow {
                submenu_item(label).disabled()
            } else {
                menu_item(label, shortcut).disabled()
            }
        }
    }
}

/// A submenu row uses a dedicated trailing arrow affordance, not the keyboard-
/// shortcut hint slot.
fn submenu_item(label: &str) -> El {
    menubar_item([menubar_item_label(label), spacer(), text("›").muted()])
}

/// A menu row body: label, plus an optional trailing shortcut/hint slot.
fn menu_item(label: &str, shortcut: Option<&str>) -> El {
    match shortcut {
        Some(s) => menubar_item_with_shortcut(label, s),
        None => menubar_item([menubar_item_label(label)]),
    }
}

/// The persistent reload-error chip (m5): an unmissable destructive badge shown
/// whenever the *freshest* source failed to parse/elaborate while the last-good
/// doc stays rendered. Not a toast — it persists until a good reload clears
/// `reload_error`. Compacts the multi-line diagnostic to its first line.
fn reload_error_chip(err: &str) -> El {
    let first = err.lines().next().unwrap_or(err);
    badge(format!("reload failed: {first}")).destructive()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fixtures::{dirty_doc, drc_violation};

    /// Collect every `.text` in an El subtree (depth-first) — for asserting where
    /// a label / chip renders.
    fn texts(el: &El, out: &mut Vec<String>) {
        if let Some(t) = &el.text {
            out.push(t.clone());
        }
        for c in &el.children {
            texts(c, out);
        }
    }

    fn all_texts(el: &El) -> Vec<String> {
        let mut v = Vec::new();
        texts(el, &mut v);
        v
    }

    /// Find the first keyed descendant carrying `label` as its text (or a child's
    /// text), returning its `key`. Used to assert a menu row's route.
    fn find_row_key(menu: &El, label: &str) -> Option<Option<String>> {
        fn walk(el: &El, label: &str) -> Option<Option<String>> {
            let hit = el.text.as_deref() == Some(label)
                || el.children.iter().any(|c| c.text.as_deref() == Some(label));
            if hit && matches!(&el.kind, Kind::Custom(n) if *n == "menubar_item") {
                return Some(el.key.clone());
            }
            for c in &el.children {
                if let Some(k) = walk(c, label) {
                    return Some(k);
                }
            }
            None
        }
        walk(menu, label)
    }

    /// The live action surface is explicit; every other row is disabled or a
    /// separator. Libraries appears in both File and Tools.
    #[test]
    fn wired_rows_carry_their_existing_action_keys() {
        // `MenuRow::Disabled` has no `action` field by construction, so collecting
        // the wired and submenu routes is the whole live surface.
        let wired: Vec<&str> = menu_defs()
            .into_iter()
            .flat_map(|d| d.rows)
            .filter_map(|r| match r {
                MenuRow::Wired { action, .. } | MenuRow::Submenu { action, .. } => Some(action),
                _ => None,
            })
            .collect();

        // The distinct wired routes are exactly the same routes the retired
        // toolbar buttons dispatched to.
        let mut set: Vec<&str> = wired.clone();
        set.sort_unstable();
        set.dedup();
        let mut want = vec![
            ABOUT_KEY,
            AUTOROUTE_BOARD_KEY,
            AUTOROUTE_NET_KEY,
            EXPORT_GERBERS_KEY,
            EXPORT_SVG_KEY,
            DELETE_KEY,
            FIT_KEY,
            FINDINGS_PANEL_KEY,
            GRID_TOGGLE_KEY,
            KEYMAP_KEY,
            LIBRARIES_TOGGLE_KEY,
            OPEN_KEY,
            OPEN_RECENT_KEY,
            PLACE_PART_KEY,
            QUIT_KEY,
            REDO_KEY,
            REVERT_KEY,
            ROTATE_KEY,
            SNAP_TO_GRID_KEY,
            SAVE_KEY,
            UNITS_TOGGLE_KEY,
            UNDO_KEY,
            ZOOM_IN_KEY,
            ZOOM_OUT_KEY,
        ];
        want.sort_unstable();
        assert_eq!(
            set, want,
            "wired menu rows must match the implemented chrome surface"
        );
        // Libraries is wired in two menus (File + Tools); everything else once.
        assert_eq!(
            wired.iter().filter(|a| **a == LIBRARIES_TOGGLE_KEY).count(),
            2
        );
    }

    #[test]
    fn open_recent_is_a_submenu_with_an_arrow_not_a_shortcut() {
        let row = menu_defs()
            .into_iter()
            .find(|menu| menu.value == "file")
            .unwrap()
            .rows
            .into_iter()
            .find(|row| {
                matches!(
                    row,
                    MenuRow::Submenu {
                        label: "Open Recent",
                        ..
                    }
                )
            })
            .expect("Open Recent submenu row");

        let rendered = menu_row_el(
            &row,
            "mm",
            "Dots",
            true,
            MenuAvailability {
                has_path: true,
                can_delete: false,
                can_rotate: false,
                can_autoroute_net: false,
                has_doc: true,
            },
        );

        assert_eq!(
            find_row_key(&rendered, "Open Recent"),
            Some(Some(OPEN_RECENT_KEY.into()))
        );
        assert!(all_texts(&rendered).iter().any(|text| text == "›"));
    }

    /// Delete and Rotate stay visible but inert without a compatible board
    /// selection, then carry their shared action routes when selection permits.
    #[test]
    fn edit_mutation_rows_are_disabled_by_selection_state() {
        let rows = menu_defs()
            .into_iter()
            .find(|menu| menu.value == "edit")
            .expect("Edit menu")
            .rows;
        let delete = rows
            .iter()
            .find(|row| matches!(row, MenuRow::Wired { action, .. } if *action == DELETE_KEY))
            .expect("Delete row");
        let rotate = rows
            .iter()
            .find(|row| matches!(row, MenuRow::Wired { action, .. } if *action == ROTATE_KEY))
            .expect("Rotate row");

        assert_eq!(
            find_row_key(
                &menu_row_el(
                    delete,
                    "mm",
                    "Dots",
                    true,
                    MenuAvailability {
                        has_path: true,
                        can_delete: false,
                        can_rotate: false,
                        can_autoroute_net: false,
                        has_doc: true,
                    },
                ),
                "Delete",
            ),
            Some(None)
        );
        assert_eq!(
            find_row_key(
                &menu_row_el(
                    rotate,
                    "mm",
                    "Dots",
                    true,
                    MenuAvailability {
                        has_path: true,
                        can_delete: false,
                        can_rotate: false,
                        can_autoroute_net: false,
                        has_doc: true,
                    },
                ),
                "Rotate",
            ),
            Some(None)
        );
        assert_eq!(
            find_row_key(
                &menu_row_el(
                    delete,
                    "mm",
                    "Dots",
                    true,
                    MenuAvailability {
                        has_path: true,
                        can_delete: true,
                        can_rotate: true,
                        can_autoroute_net: false,
                        has_doc: true,
                    },
                ),
                "Delete",
            ),
            Some(Some(DELETE_KEY.to_string()))
        );
        assert_eq!(
            find_row_key(
                &menu_row_el(
                    rotate,
                    "mm",
                    "Dots",
                    true,
                    MenuAvailability {
                        has_path: true,
                        can_delete: true,
                        can_rotate: true,
                        can_autoroute_net: false,
                        has_doc: true,
                    },
                ),
                "Rotate",
            ),
            Some(Some(ROTATE_KEY.to_string()))
        );
    }

    /// All eight oracle menus are present, in order — absence of a menu is not
    /// allowed (the enumeration is the oracle's).
    #[test]
    fn all_eight_oracle_menus_present_in_order() {
        let labels: Vec<&str> = menu_defs().iter().map(|d| d.label).collect();
        assert_eq!(
            labels,
            [
                "File", "Edit", "View", "Place", "Route", "Inspect", "Tools", "Help"
            ]
        );
    }

    /// A wired row renders with its route key; a disabled row renders inert (no
    /// key, not focusable) so it emits nothing when clicked — over a doc WITH a
    /// source path (so Save/Revert are live).
    #[test]
    fn menu_overlay_keys_wired_rows_and_inerts_disabled_rows() {
        // `dirty_doc` carries a source path, so Save / Revert are live.
        let app = dirty_doc();
        assert!(
            app.domain.source_path.is_some(),
            "the dirty_doc fixture must carry a source path"
        );
        app.set_open_menu(Some("file"));
        let menu = app.menu_overlay().expect("file menu open");

        // Libraries is wired → keyed to the toggle route.
        assert_eq!(
            find_row_key(&menu, "Libraries…"),
            Some(Some(LIBRARIES_TOGGLE_KEY.to_string())),
            "the Libraries row must dispatch to the Libraries toggle"
        );
        assert_eq!(
            find_row_key(&menu, "Save"),
            Some(Some(SAVE_KEY.to_string())),
            "the Save row must dispatch to the save action"
        );
        // Open is now wired to the native-dialog mailbox path.
        assert_eq!(
            find_row_key(&menu, "Open…"),
            Some(Some(OPEN_KEY.to_string()))
        );
        assert_eq!(
            find_row_key(&menu, "Quit"),
            Some(Some(QUIT_KEY.to_string()))
        );
    }

    /// The filename badge + per-source findings chips render in the MENU BAR, and
    /// the icon toolbar carries neither (they moved off the toolbar); the toolbar
    /// carries the static Units chip.
    #[test]
    fn status_cluster_lives_in_the_menubar_not_the_toolbar() {
        let app = drc_violation();
        let filename = app.domain.filename.clone().expect("fixture has a filename");

        let menubar_texts = all_texts(&app.menubar_bar());
        let toolbar_texts = all_texts(&app.viewer_toolbar());

        assert!(
            menubar_texts.iter().any(|t| t.contains(&filename)),
            "the filename badge renders in the menu bar: {menubar_texts:?}"
        );
        assert!(
            menubar_texts.iter().any(|t| t.starts_with("DRC ")),
            "the DRC findings chip renders in the menu bar: {menubar_texts:?}"
        );
        assert!(
            !toolbar_texts.iter().any(|t| t.contains(&filename)),
            "the filename must NOT render in the toolbar: {toolbar_texts:?}"
        );
        assert!(
            !toolbar_texts.iter().any(|t| t.starts_with("DRC")),
            "findings chips must NOT render in the toolbar: {toolbar_texts:?}"
        );
        assert!(
            toolbar_texts.iter().any(|t| t == "Units: mm"),
            "the toolbar carries the static Units chip: {toolbar_texts:?}"
        );
    }
}
