//! The board Place tool's docked library-browser palette: live text filter,
//! library-grouped rows, and the owned-renderer preview card. Escape while the
//! filter is active follows Place-tool semantics: it disarms the part.

use crate::app::EutecticApp;
use crate::registry::LibraryPart;
use damascene_core::prelude::*;

pub(crate) const LIBRARY_FILTER_KEY: &str = "library-browser:filter";
const LIBRARY_ROW_PREFIX: &str = "library-browser:part:";

#[derive(Default)]
pub(crate) struct LibraryBrowserUi {
    pub(crate) query: String,
    pub(crate) selection: Selection,
    pub(crate) highlighted: Option<usize>,
}

pub(crate) fn library_part_key(index: usize) -> String {
    format!("{LIBRARY_ROW_PREFIX}{index}")
}

fn library_part_index(route: &str) -> Option<usize> {
    route.strip_prefix(LIBRARY_ROW_PREFIX)?.parse().ok()
}

impl EutecticApp {
    pub(crate) fn open_library_browser(&self) {
        self.library_browser_open.set(true);
        if self.library_browser_ui.borrow().highlighted.is_none() {
            self.library_browser_ui.borrow_mut().highlighted =
                self.domain.library_parts.first().map(|_| 0);
        }
        self.focus_requests
            .borrow_mut()
            .push(LIBRARY_FILTER_KEY.to_string());
    }

    pub(crate) fn highlighted_library_part(&self) -> Option<LibraryPart> {
        let index = self.library_browser_ui.borrow().highlighted?;
        self.domain.library_parts.get(index).cloned()
    }

    fn filtered_library_parts(&self) -> Vec<(usize, &LibraryPart)> {
        let query = self.library_browser_ui.borrow().query.trim().to_lowercase();
        self.domain
            .library_parts
            .iter()
            .enumerate()
            .filter(|(_, row)| {
                query.is_empty()
                    || row.part.to_lowercase().contains(&query)
                    || row.library.to_lowercase().contains(&query)
            })
            .collect()
    }

    /// Docked at the left edge of the pane region, matching the oracle. It is
    /// palette-like rather than modal: canvas hover/click and chrome remain
    /// live; only its focused text input captures typing.
    pub(crate) fn library_browser(&self) -> El {
        let filtered = self.filtered_library_parts();
        {
            let mut ui = self.library_browser_ui.borrow_mut();
            if !ui
                .highlighted
                .is_some_and(|index| filtered.iter().any(|(i, _)| *i == index))
            {
                ui.highlighted = filtered.first().map(|(index, _)| *index);
            }
        }
        let highlighted = self.library_browser_ui.borrow().highlighted;
        let mut rows = Vec::new();
        let mut previous_library: Option<&str> = None;
        for (index, entry) in filtered {
            if previous_library != Some(entry.library.as_str()) {
                rows.push(sidebar_group_label(entry.library.clone()));
                previous_library = Some(&entry.library);
            }
            let current = highlighted == Some(index);
            let label = if current {
                format!("◆  {}", entry.part)
            } else {
                format!("◇  {}", entry.part)
            };
            rows.push(
                sidebar_menu_button(label, current)
                    .key(library_part_key(index))
                    .width(Size::Fill(1.0)),
            );
        }
        if rows.is_empty() {
            rows.push(text("No matching parts").muted().caption());
        }

        let ui = self.library_browser_ui.borrow();
        let preview = if let Some(entry) = self.highlighted_library_part() {
            let mut content = Vec::new();
            if let Some((texture, size)) = self.library_preview_texture() {
                content.push(
                    surface(texture)
                        .surface_alpha(SurfaceAlpha::Opaque)
                        .width(Size::Fixed(size.0 as f32))
                        .height(Size::Fixed(size.1 as f32)),
                );
            } else {
                content.push(
                    column([
                        text(entry.part.clone()).mono(),
                        text("Footprint preview").muted(),
                    ])
                    .align(Align::Center)
                    .gap(tokens::SPACE_1)
                    .width(Size::Fill(1.0))
                    .height(Size::Fill(1.0)),
                );
            }
            column([
                stack(content)
                    .clip()
                    .fill(tokens::BACKGROUND)
                    .stroke(tokens::BORDER)
                    .radius(tokens::RADIUS_MD)
                    .width(Size::Fixed(236.0))
                    .height(Size::Fixed(120.0)),
                text(format!("{} · {}", entry.part, entry.library))
                    .mono()
                    .caption()
                    .muted(),
                text(
                    if self.armed_part_name().as_deref() == Some(entry.part.as_str()) {
                        "armed · click canvas to place"
                    } else {
                        "choose row to arm"
                    },
                )
                .caption()
                .muted(),
            ])
            .gap(tokens::SPACE_2)
        } else {
            column([text("Choose a part").muted()])
        };

        sidebar([
            toolbar_title("Library"),
            text_input_with(
                LIBRARY_FILTER_KEY,
                &ui.query,
                &ui.selection,
                TextInputOpts::default().placeholder("Search parts…"),
            )
            .width(Size::Fill(1.0)),
            scroll([column(rows)
                .gap(tokens::SPACE_1)
                .padding(Sides::x(tokens::RING_WIDTH))
                .width(Size::Fill(1.0))])
            .key("library-browser:list")
            .height(Size::Fill(1.0)),
            separator(),
            preview,
        ])
        .key("library-browser")
        .gap(tokens::SPACE_2)
        .padding(Sides::all(tokens::SPACE_2))
        .height(Size::Fill(1.0))
    }

    pub(crate) fn handle_library_browser_event(&mut self, event: &UiEvent) -> bool {
        if !self.library_browser_open.get() {
            return false;
        }

        {
            let mut ui = self.library_browser_ui.borrow_mut();
            if let Some(selection) = &event.selection {
                ui.selection = selection.clone();
            }
            let LibraryBrowserUi {
                query, selection, ..
            } = &mut *ui;
            if matches!(event.kind, UiEventKind::TextInput | UiEventKind::KeyDown)
                && !selection.is_within(LIBRARY_FILTER_KEY)
            {
                // Row/canvas events continue below; typing belongs elsewhere.
            } else if text_input::apply_event(query, selection, event, LIBRARY_FILTER_KEY) {
                return true;
            }
        }

        let Some(index) = event
            .route()
            .and_then(library_part_index)
            .filter(|index| *index < self.domain.library_parts.len())
        else {
            return false;
        };
        if event.kind == UiEventKind::PointerEnter {
            self.library_browser_ui.borrow_mut().highlighted = Some(index);
            return true;
        }
        if matches!(event.kind, UiEventKind::Click | UiEventKind::Activate) {
            self.library_browser_ui.borrow_mut().highlighted = Some(index);
            let entry = self.domain.library_parts[index].clone();
            self.arm_library_part(&entry);
            return true;
        }
        false
    }
}
