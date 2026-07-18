//! The per-machine library registry + `use`-name resolution (app layer, §9).
//!
//! `docs/architecture.md` §9: a document declares its libraries by **name**
//! (`use NAME`); binding a name to a directory is the *app's* job, and the
//! binding lives at the app layer — never in the engine, never in committed
//! artifacts. This module owns that binding:
//!
//! 1. [`Registry`] — the hand-rolled name → absolute-path file (one entry per
//!    line), loaded/saved over an **explicit path** so tests inject scratch
//!    files and only `main.rs` computes the real per-machine default
//!    (`$XDG_CONFIG_HOME/eutectic/libraries`, falling back to
//!    `$HOME/.config/eutectic/libraries`).
//! 2. [`resolve`] — the (re)load-time resolution step: parse the source, walk
//!    its `use` names through the registry, [`eutectic_core::library::load_library`]
//!    each hit, union them **in source order** with the built-in toy library
//!    appended last (real libraries shadow toy names; a doc with no `use` gets
//!    exactly the toy lib, unchanged). Every failure — an unregistered name, a
//!    package that fails to load, a union collision — degrades to a [`LibNote`]
//!    the findings panel renders; the doc still loads with whatever resolved
//!    (the permissive philosophy).
//! 3. [`row_status`] — the Libraries-menu diagnostic: what state is each
//!    registry row in (loads OK with N parts / path missing / manifest error),
//!    independent of whether any open doc uses it.
//!
//! # Registry file format (hand-rolled, `.eut`-family line grammar)
//!
//! ```text
//! # eutectic library registry: NAME <absolute path>
//! poc /home/me/boards/poc/parts
//! kicad /home/me/lib/kicad packages   # ← paths may contain spaces
//! ```
//!
//! One entry per line: the first whitespace-separated token is the name, the
//! **rest of the line (trimmed)** is the path — so paths may contain spaces.
//! Full-line `#` comments and blank lines are ignored. A duplicate name: the
//! **last** entry wins (a later edit overrides an earlier one). Values must be
//! absolute paths; [`Registry::set`] rejects relative ones — the anti-leak rule
//! (machine-local paths must never travel) is enforced at this boundary.

use eutectic_core::part::PartLib;
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

/// The per-machine name → library-directory registry. Values are absolute
/// paths (enforced by [`set`](Registry::set) and re-checked on
/// [`load`](Registry::load)). Held as a `BTreeMap` so iteration — and the
/// saved file — is deterministic (sorted by name).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Registry {
    entries: BTreeMap<String, PathBuf>,
}

impl Registry {
    /// An empty registry (the first-run state).
    pub fn new() -> Registry {
        Registry::default()
    }

    /// Load a registry from `path`. A **missing file is an empty registry, not
    /// an error** (first run — nothing has been registered yet). Any other IO
    /// failure, a malformed line (a name with no path), or a relative path is
    /// an `Err` with the offending line number.
    pub fn load(path: &Path) -> Result<Registry, String> {
        let text = match std::fs::read_to_string(path) {
            Ok(t) => t,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Registry::new());
            }
            Err(e) => return Err(format!("reading {}: {e}", path.display())),
        };
        Self::parse(&text).map_err(|e| format!("{}: {e}", path.display()))
    }

    /// Parse the registry file format (see the module docs). Duplicate names:
    /// last wins.
    fn parse(text: &str) -> Result<Registry, String> {
        let mut entries = BTreeMap::new();
        for (i, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((name, rest)) = line.split_once(char::is_whitespace) else {
                return Err(format!(
                    "line {}: expected `NAME <absolute path>`, got `{line}`",
                    i + 1
                ));
            };
            let value = rest.trim();
            if value.is_empty() {
                return Err(format!("line {}: entry `{name}` has no path", i + 1));
            }
            let path = PathBuf::from(value);
            if !path.is_absolute() {
                return Err(format!(
                    "line {}: `{name}` maps to relative path `{value}` — registry paths must be absolute",
                    i + 1
                ));
            }
            // Last wins: a later line overrides an earlier one (documented).
            entries.insert(name.to_string(), path);
        }
        Ok(Registry { entries })
    }

    /// Save the registry to `path` **atomically**: write a temp file in the
    /// same directory, then rename over the target — a crash mid-save never
    /// leaves a half-written registry. Parent directories are created on
    /// first save.
    pub fn save(&self, path: &Path) -> Result<(), String> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("creating {}: {e}", parent.display()))?;
        }
        let mut out = String::from("# eutectic library registry: NAME <absolute path>\n");
        for (name, dir) in &self.entries {
            out.push_str(name);
            out.push(' ');
            out.push_str(&dir.display().to_string());
            out.push('\n');
        }
        // Temp file in the SAME directory so the rename is not cross-device.
        let tmp = path.with_extension("tmp");
        std::fs::write(&tmp, out).map_err(|e| format!("writing {}: {e}", tmp.display()))?;
        std::fs::rename(&tmp, path)
            .map_err(|e| format!("renaming {} over {}: {e}", tmp.display(), path.display()))
    }

    /// The directory registered under `name`, if any.
    pub fn get(&self, name: &str) -> Option<&Path> {
        self.entries.get(name).map(PathBuf::as_path)
    }

    /// Register `name` → `path`, replacing any existing entry. Rejects an
    /// empty or whitespace-containing name (a name is one file token) and a
    /// **relative path** — machine-local bindings must be absolute so a saved
    /// registry never depends on a working directory (the anti-leak rule
    /// lives at this boundary).
    pub fn set(&mut self, name: &str, path: &Path) -> Result<(), String> {
        if name.is_empty() {
            return Err("library name must not be empty".to_string());
        }
        if name.contains(char::is_whitespace) || name.starts_with('#') {
            return Err(format!(
                "library name `{name}` must be a single token (no whitespace, not starting with `#`)"
            ));
        }
        if path.as_os_str().is_empty() {
            return Err("library path must not be empty".to_string());
        }
        if !path.is_absolute() {
            return Err(format!(
                "library path `{}` is relative — registry paths must be absolute",
                path.display()
            ));
        }
        self.entries.insert(name.to_string(), path.to_path_buf());
        Ok(())
    }

    /// Remove the entry under `name`; `true` if it existed.
    pub fn remove(&mut self, name: &str) -> bool {
        self.entries.remove(name).is_some()
    }

    /// Iterate the entries, sorted by name (deterministic).
    pub fn iter(&self) -> impl Iterator<Item = (&str, &Path)> {
        self.entries.iter().map(|(n, p)| (n.as_str(), p.as_path()))
    }

    /// How many entries are registered.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Is the registry empty?
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

// ---------------------------------------------------------------------------
// Resolution notes (findings-panel data) + the resolve step.
// ---------------------------------------------------------------------------

/// A `use`-name that resolved to nothing in the registry.
pub const W_LIB_UNREGISTERED: &str = "W_LIB_UNREGISTERED";
/// A registered package directory that failed [`eutectic_core::library::load_library`].
pub const W_LIB_LOAD: &str = "W_LIB_LOAD";
/// A part-name collision reported by [`eutectic_core::library::union`] (first wins).
pub const W_LIB_COLLISION: &str = "W_LIB_COLLISION";

/// One GUI-side library-resolution note — **data, not an error** (§9's
/// permissive rule applied to resolution). Rendered by the findings panel as an
/// informational warning row (no geometry, no navigation) and by the Libraries
/// menu as the per-row diagnostic context. Codes are the `W_LIB_*` constants
/// above, distinct from every engine code so the DRC chip's counts stay honest
/// about what is a board problem vs. a library-binding problem.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibNote {
    /// One of [`W_LIB_UNREGISTERED`] / [`W_LIB_LOAD`] / [`W_LIB_COLLISION`].
    pub code: &'static str,
    /// The human-readable message.
    pub message: String,
}

/// One placeable part in the resolved library union, retaining the package
/// that won first-wins resolution. Rows are ordered by package resolution
/// order and then part name, so the browser can group without re-reading the
/// registry or guessing ownership from the flattened [`PartLib`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LibraryPart {
    pub library: String,
    pub part: String,
}

impl LibraryPart {
    pub(crate) fn from_lib(library: &str, lib: &PartLib) -> Vec<LibraryPart> {
        lib.keys()
            .map(|part| LibraryPart {
                library: library.to_string(),
                part: part.clone(),
            })
            .collect()
    }
}

/// Resolve a source text's `use` names into the [`PartLib`] to elaborate with,
/// plus the resolution notes. The (re)load-time step shared by the initial load
/// and every reload — a reload may add or remove `use` lines, so this re-runs
/// each time.
///
/// - Parse the source ([`eutectic_core::text::parse`]) and take its `use` names in
///   source order ([`eutectic_core::library::use_names`]). A source that does not
///   parse contributes no names (the elaborate step will surface the parse
///   errors itself — resolution stays silent rather than double-reporting).
/// - Each name: look up the registry, [`load_library`] the directory. A missing
///   registry entry or a load failure becomes a [`LibNote`], never an error.
/// - [`union`] the successes in source order, with the built-in toy
///   [`part_library`] appended **last** — real libraries shadow toy names, and
///   a doc with no `use` lines resolves to exactly the toy lib (every existing
///   fixture unchanged). Collision notes from the union become [`LibNote`]s.
///
/// [`load_library`]: eutectic_core::library::load_library
/// [`union`]: eutectic_core::library::union
/// [`part_library`]: eutectic_core::part::part_library
pub fn resolve(source: &str, registry: &Registry) -> (PartLib, Vec<LibNote>) {
    let (lib, notes, _, _) = resolve_with_index(source, registry);
    (lib, notes)
}

/// [`resolve`] plus the browser-facing catalog union and ownership index. The
/// catalog loads every registered package in registry order, then `builtin`;
/// the index contains exactly the parts that survive first-wins unioning, so a
/// shadowed duplicate is listed only under its winning package.
pub(crate) fn resolve_with_index(
    source: &str,
    registry: &Registry,
) -> (PartLib, Vec<LibNote>, PartLib, Vec<LibraryPart>) {
    let mut notes: Vec<LibNote> = Vec::new();
    let mut libs: Vec<(String, PartLib)> = Vec::new();

    if let Ok(parsed) = eutectic_core::text::parse(source) {
        for name in eutectic_core::library::use_names(&parsed.source) {
            match registry.get(&name) {
                None => notes.push(LibNote {
                    code: W_LIB_UNREGISTERED,
                    message: format!(
                        "library `{name}` is not in the registry; register it in the Libraries menu"
                    ),
                }),
                Some(dir) => match eutectic_core::library::load_library(dir) {
                    Ok(lib) => libs.push((name, lib)),
                    Err(e) => notes.push(LibNote {
                        code: W_LIB_LOAD,
                        message: format!("library `{name}` ({}): {e}", dir.display()),
                    }),
                },
            }
        }
    }

    // The built-in toy library is appended LAST: `union` is first-wins, so any
    // real library shadows a toy part of the same name, and a doc with no `use`
    // resolves to exactly the toy lib.
    libs.push(("builtin".to_string(), eutectic_core::part::part_library()));
    let (lib, collisions) = eutectic_core::library::union(&libs);
    notes.extend(collisions.into_iter().map(|message| LibNote {
        code: W_LIB_COLLISION,
        message,
    }));

    // The placement catalog is wider than the document's dependency union:
    // every successfully loaded registry package is discoverable, in registry
    // order, even before the document has authored `use NAME`. Choosing such a
    // row adds that declaration in the same source-first placement transaction.
    let mut catalog_libs = Vec::new();
    for (name, dir) in registry.iter() {
        if let Ok(parts) = eutectic_core::library::load_library(dir) {
            catalog_libs.push((name.to_string(), parts));
        }
    }
    catalog_libs.push(("builtin".to_string(), eutectic_core::part::part_library()));
    let mut seen = BTreeSet::new();
    let mut index = Vec::new();
    for (library, parts) in &catalog_libs {
        for part in parts.keys() {
            if seen.insert(part.clone()) {
                index.push(LibraryPart {
                    library: library.clone(),
                    part: part.clone(),
                });
            }
        }
    }
    let (catalog, _) = eutectic_core::library::union(&catalog_libs);
    (lib, notes, catalog, index)
}

// ---------------------------------------------------------------------------
// Per-row status for the Libraries menu.
// ---------------------------------------------------------------------------

/// The load state of one registry row — the Libraries menu's per-row
/// diagnostic ("why is my library broken"), shown for **every** row whether or
/// not the current doc uses it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowStatus {
    /// The package loads cleanly and provides this many parts.
    Ok { parts: usize },
    /// The directory (or its `eutectic.lib` manifest) does not exist.
    Missing,
    /// The manifest exists but the package failed to load (the message).
    Error(String),
}

/// Compute the [`RowStatus`] of one registry path by attempting the same
/// [`eutectic_core::library::load_library`] the resolver uses. Distinguishes a
/// missing directory/manifest (the common "moved my checkout" case) from a
/// manifest that exists but fails to load.
pub fn row_status(dir: &Path) -> RowStatus {
    if !dir.join(eutectic_core::library::MANIFEST_NAME).exists() {
        return RowStatus::Missing;
    }
    match eutectic_core::library::load_library(dir) {
        Ok(lib) => RowStatus::Ok { parts: lib.len() },
        Err(e) => RowStatus::Error(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A scratch directory under the system temp dir, removed on drop. Tests
    /// NEVER touch the real per-user config location (the registry API takes
    /// its path explicitly — only `main.rs` computes the default).
    struct Scratch(PathBuf);

    impl Scratch {
        fn new(tag: &str) -> Scratch {
            let dir = std::env::temp_dir().join(format!(
                "eutectic-registry-test-{tag}-{}",
                std::process::id()
            ));
            std::fs::create_dir_all(&dir).expect("create scratch dir");
            Scratch(dir)
        }
        fn path(&self, name: &str) -> PathBuf {
            self.0.join(name)
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Missing file = empty registry (first run), NOT an error.
    #[test]
    fn load_missing_file_is_empty() {
        let s = Scratch::new("missing");
        let reg = Registry::load(&s.path("does-not-exist")).expect("missing file loads");
        assert!(reg.is_empty());
    }

    /// The line grammar: comments + blanks ignored, the path is the rest of
    /// the line (spaces allowed), duplicate names last-wins.
    #[test]
    fn parse_grammar() {
        let reg = Registry::parse(
            "# a comment\n\
             \n\
             poc /home/me/poc/parts\n\
             spacey /home/me/my lib dir\n\
             poc /home/me/other/parts\n",
        )
        .expect("parses");
        assert_eq!(reg.len(), 2);
        assert_eq!(
            reg.get("spacey"),
            Some(Path::new("/home/me/my lib dir")),
            "the path is the rest of the line — spaces allowed"
        );
        assert_eq!(
            reg.get("poc"),
            Some(Path::new("/home/me/other/parts")),
            "duplicate name: last wins"
        );
    }

    /// Malformed lines and relative paths are load errors with line numbers.
    #[test]
    fn parse_rejects_bad_lines() {
        let e = Registry::parse("lonely\n").unwrap_err();
        assert!(e.contains("line 1"), "{e}");
        let e = Registry::parse("ok /abs\nrel relative/path\n").unwrap_err();
        assert!(e.contains("line 2") && e.contains("absolute"), "{e}");
    }

    /// set/get/remove/iterate + the absolute-path and single-token guards.
    #[test]
    fn set_get_remove() {
        let mut reg = Registry::new();
        reg.set("poc", Path::new("/abs/poc")).expect("absolute ok");
        assert_eq!(reg.get("poc"), Some(Path::new("/abs/poc")));
        // Replace (set overrides).
        reg.set("poc", Path::new("/abs/poc2")).unwrap();
        assert_eq!(reg.get("poc"), Some(Path::new("/abs/poc2")));

        assert!(reg.set("poc", Path::new("relative")).is_err());
        assert!(reg.set("", Path::new("/abs")).is_err());
        assert!(reg.set("two words", Path::new("/abs")).is_err());
        assert!(reg.set("#hash", Path::new("/abs")).is_err());
        assert!(reg.set("x", Path::new("")).is_err());

        assert!(reg.remove("poc"));
        assert!(!reg.remove("poc"), "already gone");
        assert!(reg.get("poc").is_none());
    }

    /// Save → load round-trips, creating parent dirs on first save; the temp
    /// file does not linger.
    #[test]
    fn save_load_roundtrip_creates_parents() {
        let s = Scratch::new("roundtrip");
        let file = s.path("nested/config/libraries");
        let mut reg = Registry::new();
        reg.set("a", Path::new("/abs/a")).unwrap();
        reg.set("spacey", Path::new("/abs/with space")).unwrap();
        reg.save(&file).expect("save creates parents");

        let back = Registry::load(&file).expect("loads back");
        assert_eq!(back, reg);
        assert!(
            !file.with_extension("tmp").exists(),
            "atomic save leaves no temp file"
        );
    }

    /// Do two libs provide the same part set? (`PartDef` has no `PartialEq`;
    /// the part-name set is the honest comparison for resolution tests.)
    fn same_parts(a: &PartLib, b: &PartLib) -> bool {
        a.keys().eq(b.keys())
    }

    /// Resolution: no `use` lines → exactly the toy lib, zero notes (every
    /// existing doc/fixture unchanged).
    #[test]
    fn resolve_no_use_is_toy_lib() {
        let (lib, notes) = resolve("inst C1 Cap\n", &Registry::new());
        assert!(same_parts(&lib, &eutectic_core::part::part_library()));
        assert!(notes.is_empty());
    }

    /// Resolution: an unregistered `use` name degrades to a W_LIB_UNREGISTERED
    /// note; the toy lib still resolves (the doc still loads).
    #[test]
    fn resolve_unregistered_name_notes() {
        let (lib, notes) = resolve("use nolib\ninst C1 Cap\n", &Registry::new());
        assert!(same_parts(&lib, &eutectic_core::part::part_library()));
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].code, W_LIB_UNREGISTERED);
        assert!(notes[0].message.contains("nolib"), "{}", notes[0].message);
    }

    /// Resolution: a registered name whose directory fails to load degrades to
    /// a W_LIB_LOAD note carrying the loader's message.
    #[test]
    fn resolve_broken_package_notes() {
        let s = Scratch::new("broken");
        let dir = s.path("libdir");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("eutectic.lib"),
            "part Bogus footprint=missing.kicad_mod\n",
        )
        .unwrap();
        let mut reg = Registry::new();
        reg.set("bad", &dir).unwrap();
        let (lib, notes) = resolve("use bad\n", &reg);
        assert!(
            same_parts(&lib, &eutectic_core::part::part_library()),
            "toy lib still resolves"
        );
        assert_eq!(notes.len(), 1);
        assert_eq!(notes[0].code, W_LIB_LOAD);
        assert!(notes[0].message.contains("bad"), "{}", notes[0].message);
    }

    /// Resolution: a real package resolves through the registry and its parts
    /// land in the lib, shadowing nothing (poc names don't overlap the toy
    /// lib). Uses the in-repo poc package — the same one the poc smoke test
    /// reads.
    #[test]
    fn resolve_real_package_through_registry() {
        let poc_parts = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../poc/parts");
        let poc_parts = poc_parts.canonicalize().expect("poc/parts exists");
        let mut reg = Registry::new();
        reg.set("poc", &poc_parts).unwrap();
        let (lib, notes) = resolve("use poc\ninst U1 RP2350A\n", &reg);
        assert!(lib.contains_key("RP2350A"), "poc parts resolved");
        assert!(
            lib.contains_key("Cap"),
            "toy lib still appended (docs with no use keep working)"
        );
        assert!(notes.is_empty(), "clean resolve has no notes: {notes:?}");
    }

    /// Row status distinguishes missing / broken / ok.
    #[test]
    fn row_status_states() {
        let s = Scratch::new("status");
        assert_eq!(row_status(&s.path("nope")), RowStatus::Missing);

        let broken = s.path("broken");
        std::fs::create_dir_all(&broken).unwrap();
        std::fs::write(
            broken.join("eutectic.lib"),
            "part X footprint=absent.kicad_mod\n",
        )
        .unwrap();
        assert!(matches!(row_status(&broken), RowStatus::Error(_)));

        let poc_parts = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../poc/parts");
        match row_status(&poc_parts) {
            RowStatus::Ok { parts } => assert!(parts > 0, "poc provides parts"),
            other => panic!("poc/parts must load OK, got {other:?}"),
        }
    }
}
