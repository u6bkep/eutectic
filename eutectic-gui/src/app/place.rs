//! Board part placement: isolated-part preview elaboration, armed ghost state,
//! refdes allocation, and the source-first commit path.

use crate::app::{EutecticApp, PaneId};
use crate::registry::LibraryPart;
use crate::render::Scene;
use crate::tool::{Tool, translate_shape};
use eutectic_core::command::{Command, Transaction};
use eutectic_core::coord::Point;
use eutectic_core::doc::{Doc, Override, Strength};
use eutectic_core::geom::{Extent, Shape2D};
use eutectic_core::history::History;
use eutectic_core::id::EntityId;
use eutectic_core::ir::GenDirective;
use eutectic_core::part::PartLib;
use std::collections::{BTreeMap, BTreeSet};

/// The library row currently armed for repeated placement.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ArmedPart {
    pub(crate) library: String,
    pub(crate) part: String,
}

impl From<&LibraryPart> for ArmedPart {
    fn from(value: &LibraryPart) -> Self {
        ArmedPart {
            library: value.library.clone(),
            part: value.part.clone(),
        }
    }
}

/// Elaborate one part at the origin and lower it through the board producer.
/// The returned shapes are the same `world_features` geometry translated by
/// the placement overlay; the scene is the owned-renderer thumbnail input.
pub(crate) fn isolated_part_preview(
    part: &str,
    lib: &PartLib,
) -> Result<(Scene, Vec<Shape2D>), String> {
    let def = lib
        .get(part)
        .ok_or_else(|| format!("part `{part}` is not resolved"))?;
    let mut source = format!("inst __preview {part}\nfix __preview (0mm, 0mm)\n");
    // `world_features` emits pad copper through the authoritative netlist.
    // Give every physical pad a private preview net so an electrically
    // unconnected catalog item still renders its whole footprint.
    for (index, pin) in def.pins.iter().enumerate() {
        source.push_str(&format!("net __preview_{index} __preview.{}\n", pin.number));
    }
    let mut history = History::new(Doc::default());
    history
        .commit(
            Transaction::one(Command::LoadText(source)),
            lib,
            "part preview",
        )
        .map_err(format_diagnostics)?;
    let doc = history.doc();
    let stackup = eutectic_core::elaborate::stackup(&doc.source);
    let features = eutectic_core::route::world_features(
        doc,
        lib,
        &BTreeMap::new(),
        &eutectic_core::route::DesignRules::default(),
        &stackup,
    )?;
    let shapes = features
        .into_iter()
        .map(|feature| match feature.feature.extent {
            Extent::Prism { shape, .. } => shape,
        })
        .collect();
    let scene = crate::render::board_scene(doc, lib)?;
    Ok((scene, shapes))
}

fn format_diagnostics(diags: Vec<eutectic_core::diagnostic::Diagnostic>) -> String {
    diags
        .iter()
        .map(|d| format!("[{}] {}", d.code, d.message))
        .collect::<Vec<_>>()
        .join("\n")
}

impl EutecticApp {
    pub(crate) fn library_preview_data(
        &self,
        row: &LibraryPart,
    ) -> Result<(Scene, Vec<Shape2D>), String> {
        let key = (
            row.part.clone(),
            row.library.clone(),
            self.domain.catalog_generation,
        );
        if let Some(cached) = self.library_preview_data.borrow().get(&key) {
            return cached.clone();
        }
        let preview = isolated_part_preview(&row.part, &self.domain.catalog_lib);
        self.library_preview_data
            .borrow_mut()
            .insert(key, preview.clone());
        preview
    }

    /// Arm one browser row. The flyout deliberately stays open (the binding
    /// oracle keeps the list and preview visible beside the canvas).
    pub(crate) fn arm_library_part(&mut self, row: &LibraryPart) {
        let Some(_) = self.domain.catalog_lib.get(&row.part) else {
            return;
        };
        match self.library_preview_data(row) {
            Ok((_, shapes)) => {
                *self.armed_part.borrow_mut() = Some(ArmedPart::from(row));
                *self.place_shapes.borrow_mut() = shapes;
                self.clear_place_cursor();
                self.library_browser_open.set(true);
            }
            Err(error) => self.domain.edit.error = Some(error),
        }
    }

    /// Clear the armed part and its ghost without leaving Place mode.
    pub(crate) fn disarm_part(&self) {
        *self.armed_part.borrow_mut() = None;
        self.place_shapes.borrow_mut().clear();
        self.clear_place_cursor();
    }

    /// Drop only the live cursor ghost. Switching tools uses this while
    /// retaining the browser choice for a later return to Place.
    pub(crate) fn clear_place_cursor(&self) -> bool {
        self.place_cursor.replace(None).is_some()
    }

    /// Update the raw-cursor placement ghost in one board pane.
    pub(crate) fn hover_place_part(&self, pane: PaneId, at: Point) -> bool {
        if self.tool_for(crate::app::ViewKind::Board) != Tool::Place
            || self.armed_part.borrow().is_none()
        {
            return false;
        }
        let next = Some((pane, at));
        if self.place_cursor.get() != next {
            self.place_cursor.set(next);
            true
        } else {
            false
        }
    }

    /// Footprint ghost shapes at the current free-hover point for `pane`.
    pub(crate) fn place_ghost_shapes(&self, pane: PaneId) -> Vec<Shape2D> {
        let Some((owner, at)) = self.place_cursor.get() else {
            return Vec::new();
        };
        if owner != pane || self.tool_for(crate::app::ViewKind::Board) != Tool::Place {
            return Vec::new();
        }
        self.place_shapes
            .borrow()
            .iter()
            .map(|shape| translate_shape(shape, at))
            .collect()
    }

    /// Commit one placement at `at` and remain armed for repetition. The
    /// staged document is serialized so the transaction payload uses the
    /// canonical `inst` + `# overrides` (`pin`/`refdes`) form, then loaded in
    /// one command: exactly one undo unit.
    pub(crate) fn commit_armed_part(&mut self, at: Point) {
        let Some(armed) = self.armed_part.borrow().clone() else {
            return;
        };
        let text = match self.placement_text(&armed, at) {
            Ok(text) => text,
            Err(error) => {
                self.domain.edit.error = Some(error);
                return;
            }
        };
        // The chosen package may not have been a dependency before this
        // placement. Resolve the staged `use` declaration before asking the
        // held History to elaborate its one LoadText command.
        let (next_lib, next_notes, next_catalog, next_rows) = self.domain.resolve_lib(&text);
        let next_catalog_generation = self.domain.catalog_generation_for(&text);
        let prior_lib = std::mem::replace(&mut self.domain.lib, next_lib);
        let prior_notes = std::mem::replace(&mut self.domain.lib_notes, next_notes);
        let prior_catalog = std::mem::replace(&mut self.domain.catalog_lib, next_catalog);
        let prior_catalog_generation =
            std::mem::replace(&mut self.domain.catalog_generation, next_catalog_generation);
        let prior_rows = std::mem::replace(&mut self.domain.library_parts, next_rows);
        if let Err(error) =
            self.commit_edit(Transaction::one(Command::LoadText(text)), "place part")
        {
            self.domain.lib = prior_lib;
            self.domain.lib_notes = prior_notes;
            self.domain.catalog_lib = prior_catalog;
            self.domain.catalog_generation = prior_catalog_generation;
            self.domain.library_parts = prior_rows;
            self.domain.edit.error = Some(error);
        }
    }

    fn placement_text(&self, armed: &ArmedPart, at: Point) -> Result<String, String> {
        let part = armed.part.as_str();
        let doc = self
            .domain
            .doc
            .as_ref()
            .map_err(|error| format!("no document to edit: {error}"))?;
        let already_resolved = self.domain.lib.contains_key(part);
        let def = if already_resolved {
            self.domain.lib.get(part)
        } else {
            self.domain.catalog_lib.get(part)
        }
        .ok_or_else(|| format!("part `{part}` is no longer resolved"))?;
        let registry = eutectic_core::annotate::registry(&doc.source);
        let class = eutectic_core::annotate::class_of(def);
        let prefix = registry
            .get(&class)
            .and_then(|entry| entry.prefix.clone())
            .unwrap_or(class);
        let used_refdes: BTreeSet<String> =
            eutectic_core::annotate::refdes(doc, &self.domain.lib, &registry)
                .into_values()
                .chain(doc.refdes_pins.values().cloned())
                .collect();
        let authored_paths: BTreeSet<&str> =
            doc.source
                .iter()
                .filter_map(|directive| match directive {
                    GenDirective::Instance { path, .. }
                    | GenDirective::InstGenerative { path, .. } => Some(path.as_str()),
                    _ => None,
                })
                .collect();
        let refdes = (1_u32..=u32::MAX)
            .map(|number| format!("{prefix}{number}"))
            .find(|candidate| {
                !used_refdes.contains(candidate)
                    && !doc.components.contains_key(&EntityId::new(candidate))
                    && !authored_paths.contains(candidate.as_str())
            })
            .ok_or_else(|| format!("reference designator family `{prefix}` is exhausted"))?;
        let id = EntityId::new(&refdes);
        let mut staged = doc.clone();
        if !already_resolved
            && armed.library != "builtin"
            && !staged.source.iter().any(
                |directive| matches!(directive, GenDirective::Use { name } if name == &armed.library),
            )
        {
            let insert_at = staged
                .source
                .iter()
                .rposition(|directive| matches!(directive, GenDirective::Use { .. }))
                .map_or(0, |index| index + 1);
            staged.source.insert(
                insert_at,
                GenDirective::Use {
                    name: armed.library.clone(),
                },
            );
        }
        staged.source.push(GenDirective::Instance {
            path: refdes.clone(),
            part: part.to_string(),
            params: BTreeMap::new(),
            label: None,
        });
        staged.overrides.insert(
            id.clone(),
            Override {
                pos: Some(at),
                strength: Strength::Pin,
            },
        );
        // Pin the allocated refdes to its matching id: auto-annotation is
        // insertion-unstable, so later placements must not renumber this part.
        staged.refdes_pins.insert(id, refdes);
        Ok(eutectic_core::text::serialize(&staged))
    }

    /// Registry/source reloads may remove the armed row. Keep the tool mode,
    /// but disarm stale data before it can commit an unresolved part.
    pub(crate) fn reconcile_armed_part(&self) {
        let valid = self.armed_part.borrow().as_ref().is_none_or(|armed| {
            self.domain
                .library_parts
                .iter()
                .any(|row| row.library == armed.library && row.part == armed.part)
        });
        if !valid {
            self.disarm_part();
        }
    }

    #[cfg(test)]
    pub(crate) fn placement_text_for_test(
        &self,
        library: &str,
        part: &str,
        at: Point,
    ) -> Result<String, String> {
        self.placement_text(
            &ArmedPart {
                library: library.to_string(),
                part: part.to_string(),
            },
            at,
        )
    }

    pub(crate) fn armed_part_name(&self) -> Option<String> {
        self.armed_part
            .borrow()
            .as_ref()
            .map(|armed| armed.part.clone())
    }
}
