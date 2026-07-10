//! Library packages: a manifest-driven part-library directory the engine loads.
//!
//! A **library package** is a directory containing a manifest file named `eutectic.lib`
//! plus the asset files it references (KiCad footprints / symbol files). Documents
//! declare the libraries they depend on with `use <name>` directives
//! ([`GenDirective::Use`]); the *caller* resolves each name to a directory, loads it
//! with [`load_library`], merges the results with [`union`], and passes the resulting
//! [`PartLib`] into `commit`/`elaborate` exactly as before — the engine's lib
//! threading is unchanged, and elaboration treats `use` as inert. A part name the
//! final lib does not provide degrades to a non-blocking `W_UNRESOLVED_PART` finding
//! (the document still loads).
//!
//! # Manifest grammar (`eutectic.lib`)
//!
//! Line-based, hand-rolled — the same family as the `.eut` grammar. `#` starts a
//! comment (stripped to end of line; this grammar has no quoted strings, so plain
//! first-`#` stripping is exact); blank lines are ignored. Tokens are
//! whitespace-split, so **filenames containing spaces are unsupported**.
//!
//! ```text
//! # a footprint-only part (all pins Passive):
//! part R footprint=R_0402.kicad_mod
//!
//! # a symbol-joined part (symbol=<file>:<symbol-name>; the join must be CLEAN —
//! # any symbol-only or footprint-only pin is a load error):
//! part RP2350A footprint=RP2350A_QFN-60.kicad_mod symbol=MCU_RaspberryPi.kicad_sym:RP2350A
//!
//! # either form may open a role block: `role <pad-number> <name> <kind>` lines,
//! # closed by a lone `}`. Kinds: power_in|power_out|output|input|bidir|passive.
//! # Roles are applied AFTER any symbol join; a number may repeat across pads
//! # (every pad with that number is renamed), but a number matching NO pad errors.
//! part LED footprint=LED_WS2812B.kicad_mod {
//!   role 1 VDD power_in
//!   role 2 DOUT output
//!   role 3 GND passive
//!   role 4 DIN input
//! }
//! ```
//!
//! Relative paths resolve against the manifest's directory. An **absolute path in
//! the manifest is a parse error** — a deliberate hygiene guard: the design of
//! record is that absolute paths must never appear in committed artifacts (a
//! library package must be relocatable; the document itself commits only `use`
//! *names*). Duplicate part names, unknown `key=` tokens, unknown role kinds,
//! role numbers matching no pad, and missing/unreadable files are all errors.
//!
//! This is an **import boundary**, so the API returns `Err(String)` (first error
//! wins, messages carry the manifest line number where applicable) — the same
//! convention as [`crate::kicad`].

use crate::ir::GenDirective;
use crate::kicad::{
    apply_role_map, import_footprint_file, import_symbol_named, join_symbol_footprint,
};
use crate::part::{PartLib, PinRole};
use std::collections::BTreeMap;
use std::path::Path;

/// The manifest filename every library package carries at its directory root.
pub const MANIFEST_NAME: &str = "eutectic.lib";

/// One parsed `part` declaration from a manifest (pre-build, paths still relative).
#[derive(Debug)]
struct PartDecl {
    name: String,
    /// Manifest line of the `part` header (for error messages).
    line: usize,
    footprint: String,
    /// `symbol=<rel-path>:<symbol-name>`, when present.
    symbol: Option<(String, String)>,
    /// `role <number> <name> <kind>` entries, in authored order.
    roles: Vec<(String, String, PinRole)>,
}

/// Parse a role `kind` token into a [`PinRole`].
fn parse_role_kind(tok: &str) -> Result<PinRole, String> {
    Ok(match tok {
        "power_in" => PinRole::PowerIn,
        "power_out" => PinRole::PowerOut,
        "output" => PinRole::Output,
        "input" => PinRole::Input,
        "bidir" => PinRole::Bidir,
        "passive" => PinRole::Passive,
        other => {
            return Err(format!(
                "unknown role kind `{other}` (expected power_in|power_out|output|input|bidir|passive)"
            ));
        }
    })
}

/// Reject an absolute or package-escaping manifest path (the hygiene guard — see
/// the module docs: a committed library package must be relocatable and
/// self-contained, so its manifest may reference assets only *within* itself).
fn check_relative(rel: &str, lineno: usize) -> Result<(), String> {
    if Path::new(rel).is_absolute() {
        return Err(format!(
            "line {lineno}: absolute path `{rel}` is not allowed in a manifest \
             (paths are relative to the manifest's directory; absolute paths must \
             never appear in committed artifacts)"
        ));
    }
    if Path::new(rel)
        .components()
        .any(|c| matches!(c, std::path::Component::ParentDir))
    {
        return Err(format!(
            "line {lineno}: path `{rel}` escapes the library package via `..` \
             (a package must be self-contained: assets live beside the manifest)"
        ));
    }
    Ok(())
}

/// Parse manifest text into part declarations. Pure over the text (no filesystem),
/// so the grammar is unit-testable; [`load_library`] does the file loading.
fn parse_manifest(text: &str) -> Result<Vec<PartDecl>, String> {
    let mut decls: Vec<PartDecl> = Vec::new();
    let mut seen: BTreeMap<String, usize> = BTreeMap::new();
    // Some(index into decls) while inside an open `{ … }` role block.
    let mut open: Option<usize> = None;

    for (i, raw) in text.lines().enumerate() {
        let lineno = i + 1;
        // No quoted strings in this grammar, so plain first-# stripping is exact.
        let line = raw.split('#').next().unwrap_or("");
        let toks: Vec<&str> = line.split_whitespace().collect();
        if toks.is_empty() {
            continue;
        }
        match (toks[0], open) {
            ("}", Some(_)) => {
                if toks.len() != 1 {
                    return Err(format!("line {lineno}: `}}` must stand alone"));
                }
                open = None;
            }
            ("}", None) => {
                return Err(format!("line {lineno}: `}}` with no open role block"));
            }
            ("role", Some(idx)) => {
                if toks.len() != 4 {
                    return Err(format!(
                        "line {lineno}: expected `role <pad-number> <name> <kind>`"
                    ));
                }
                let kind = parse_role_kind(toks[3]).map_err(|e| format!("line {lineno}: {e}"))?;
                decls[idx]
                    .roles
                    .push((toks[1].to_string(), toks[2].to_string(), kind));
            }
            ("role", None) => {
                return Err(format!(
                    "line {lineno}: `role` is only valid inside a part's `{{ … }}` block"
                ));
            }
            ("part", Some(_)) => {
                return Err(format!(
                    "line {lineno}: `part` inside an unclosed role block (missing `}}`?)"
                ));
            }
            ("part", None) => {
                // `part NAME footprint=REL [symbol=REL:SYMBOL_NAME] [{]`
                let mut toks = &toks[1..];
                let opens = toks.last() == Some(&"{");
                if opens {
                    toks = &toks[..toks.len() - 1];
                }
                let [name, kvs @ ..] = toks else {
                    return Err(format!(
                        "line {lineno}: expected `part <name> footprint=<file> \
                         [symbol=<file>:<symbol-name>] [{{]`"
                    ));
                };
                if let Some(prev) = seen.get(*name) {
                    return Err(format!(
                        "line {lineno}: duplicate part `{name}` (first declared on line {prev})"
                    ));
                }
                let mut footprint: Option<String> = None;
                let mut symbol: Option<(String, String)> = None;
                for kv in kvs {
                    if let Some(rel) = kv.strip_prefix("footprint=") {
                        check_relative(rel, lineno)?;
                        footprint = Some(rel.to_string());
                    } else if let Some(v) = kv.strip_prefix("symbol=") {
                        let Some((rel, sym)) = v.rsplit_once(':') else {
                            return Err(format!(
                                "line {lineno}: symbol needs `symbol=<file>:<symbol-name>`, \
                                 got `{kv}`"
                            ));
                        };
                        if rel.is_empty() || sym.is_empty() {
                            return Err(format!(
                                "line {lineno}: symbol needs `symbol=<file>:<symbol-name>`, \
                                 got `{kv}`"
                            ));
                        }
                        check_relative(rel, lineno)?;
                        symbol = Some((rel.to_string(), sym.to_string()));
                    } else {
                        return Err(format!("line {lineno}: unknown token `{kv}` in `part`"));
                    }
                }
                let Some(footprint) = footprint else {
                    return Err(format!(
                        "line {lineno}: part `{name}` is missing `footprint=<file>` \
                         (the footprint is the geometry truth — every part has one)"
                    ));
                };
                seen.insert((*name).to_string(), lineno);
                decls.push(PartDecl {
                    name: (*name).to_string(),
                    line: lineno,
                    footprint,
                    symbol,
                    roles: Vec::new(),
                });
                if opens {
                    open = Some(decls.len() - 1);
                }
            }
            (other, _) => {
                return Err(format!(
                    "line {lineno}: unknown manifest directive `{other}` \
                     (expected `part`, `role`, or `}}`)"
                ));
            }
        }
    }
    if let Some(idx) = open {
        return Err(format!(
            "part `{}` (line {}): role block is never closed (missing `}}`)",
            decls[idx].name, decls[idx].line
        ));
    }
    Ok(decls)
}

/// Load a library package from `dir`: parse `dir/eutectic.lib` and build every declared
/// part — footprint import, optional clean symbol join, optional role overlay — into
/// a [`PartLib`]. Import boundary: `Err(String)`, first error wins, messages name
/// the manifest line where applicable. See the module docs for the grammar.
pub fn load_library(dir: &Path) -> Result<PartLib, String> {
    let manifest = dir.join(MANIFEST_NAME);
    let text = std::fs::read_to_string(&manifest)
        .map_err(|e| format!("read {}: {e}", manifest.display()))?;
    let decls = parse_manifest(&text).map_err(|e| format!("{}: {e}", manifest.display()))?;

    let mut lib = PartLib::new();
    for decl in decls {
        let ctx = |what: &str| {
            format!(
                "{}: line {}: part `{}`: {what}",
                manifest.display(),
                decl.line,
                decl.name
            )
        };
        let fp_path = dir.join(&decl.footprint);
        let fp_str = fp_path
            .to_str()
            .ok_or_else(|| ctx(&format!("non-UTF-8 path `{}`", fp_path.display())))?;
        let mut part = import_footprint_file(fp_str)
            .map_err(|e| ctx(&format!("footprint `{}`: {e}", decl.footprint)))?;

        if let Some((rel, sym_name)) = &decl.symbol {
            let sym_path = dir.join(rel);
            let sym_text = std::fs::read_to_string(&sym_path)
                .map_err(|e| ctx(&format!("read symbol `{rel}`: {e}")))?;
            let sym = import_symbol_named(&sym_text, sym_name)
                .map_err(|e| ctx(&format!("symbol `{rel}:{sym_name}`: {e}")))?;
            let jr = join_symbol_footprint(&sym, &part);
            // The join must be CLEAN (mirroring the poc's build_lib assertion): a pin
            // only the symbol names, or a pad only the footprint carries, means the
            // pairing is wrong — a load error, never a silently partial part.
            if !jr.symbol_only.is_empty() || !jr.footprint_only.is_empty() {
                return Err(ctx(&format!(
                    "symbol `{rel}:{sym_name}` does not join cleanly \
                     (symbol-only pins: {:?}; footprint-only pads: {:?})",
                    jr.symbol_only, jr.footprint_only
                )));
            }
            part = jr.part;
        }

        if !decl.roles.is_empty() {
            let map: Vec<(&str, &str, PinRole)> = decl
                .roles
                .iter()
                .map(|(num, name, kind)| (num.as_str(), name.as_str(), *kind))
                .collect();
            part = apply_role_map(part, &map).map_err(|e| ctx(&e))?;
        }

        lib.insert(decl.name, part);
    }
    Ok(lib)
}

/// Merge libraries **in the given order** into one [`PartLib`]. On a part-name
/// collision the *first* provider wins (deterministic — the order of `libs` is the
/// precedence order, and each lib's parts iterate in `BTreeMap` order); every
/// collision is reported in the returned notes as
/// `` part `X` provided by both `A` and `B`; `A` wins ``.
pub fn union(libs: &[(String, PartLib)]) -> (PartLib, Vec<String>) {
    let mut out = PartLib::new();
    let mut provider: BTreeMap<String, &str> = BTreeMap::new();
    let mut notes = Vec::new();
    for (lib_name, lib) in libs {
        for (part, def) in lib {
            match provider.get(part.as_str()) {
                Some(first) => notes.push(format!(
                    "part `{part}` provided by both `{first}` and `{lib_name}`; `{first}` wins"
                )),
                None => {
                    provider.insert(part.clone(), lib_name);
                    out.insert(part.clone(), def.clone());
                }
            }
        }
    }
    (out, notes)
}

/// Enumerate the `use <name>` library declarations of a source program, in source
/// order, first occurrence wins (a repeated `use` of the same name is collapsed —
/// re-loading the same package would only self-collide in [`union`]). This is the
/// resolver's entry point: map each name to a package directory, [`load_library`]
/// each, [`union`] them in this order, and pass the result in as the [`PartLib`].
pub fn use_names(directives: &[GenDirective]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for d in directives {
        if let GenDirective::Use { name } = d
            && !out.iter().any(|n| n == name)
        {
            out.push(name.clone());
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::part::{PartDef, PinDef};

    /// A minimal in-memory PartDef (no geometry) for union tests.
    fn stub(name: &str) -> PartDef {
        PartDef {
            name: name.into(),
            pins: vec![PinDef {
                name: "1".into(),
                number: "1".into(),
                role: PinRole::Passive,
                offset: crate::doc::Point { x: 0, y: 0 },
                pad: None,
            }],
            interfaces: BTreeMap::new(),
            graphics: Vec::new(),
            texts: Vec::new(),
            courtyard: None,
            class: None,
        }
    }

    fn lib_of(parts: &[&str]) -> PartLib {
        parts.iter().map(|p| ((*p).to_string(), stub(p))).collect()
    }

    // ---- manifest grammar --------------------------------------------------

    #[test]
    fn manifest_parses_all_forms() {
        let text = "\
# comment line
part R footprint=R_0402.kicad_mod   # trailing comment

part MCU footprint=fp.kicad_mod symbol=syms.kicad_sym:RP2350A
part LED footprint=led.kicad_mod {
  role 1 VDD power_in
  role 2 DOUT output
}
";
        let decls = parse_manifest(text).unwrap();
        assert_eq!(decls.len(), 3);
        assert_eq!(decls[0].name, "R");
        assert_eq!(decls[0].footprint, "R_0402.kicad_mod");
        assert!(decls[0].symbol.is_none() && decls[0].roles.is_empty());
        assert_eq!(
            decls[1].symbol,
            Some(("syms.kicad_sym".to_string(), "RP2350A".to_string()))
        );
        assert_eq!(
            decls[2].roles,
            vec![
                ("1".to_string(), "VDD".to_string(), PinRole::PowerIn),
                ("2".to_string(), "DOUT".to_string(), PinRole::Output),
            ]
        );
    }

    #[test]
    fn manifest_rejects_absolute_path() {
        // The hygiene guard: absolute paths must never appear in committed artifacts.
        let err = parse_manifest("part R footprint=/abs/R.kicad_mod\n").unwrap_err();
        assert!(err.contains("absolute path"), "got: {err}");
        let err =
            parse_manifest("part M footprint=f.kicad_mod symbol=/abs/s.kicad_sym:M\n").unwrap_err();
        assert!(err.contains("absolute path"), "got: {err}");
    }

    #[test]
    fn manifest_rejects_parent_dir_traversal() {
        // Self-containment: a `..` path escapes the package directory.
        let err = parse_manifest("part R footprint=../outside/R.kicad_mod\n").unwrap_err();
        assert!(err.contains("escapes the library package"), "got: {err}");
        let err = parse_manifest("part M footprint=f.kicad_mod symbol=a/../../s.kicad_sym:M\n")
            .unwrap_err();
        assert!(err.contains("escapes the library package"), "got: {err}");
    }

    #[test]
    fn manifest_rejects_duplicate_part() {
        let err = parse_manifest("part R footprint=a.kicad_mod\npart R footprint=b.kicad_mod\n")
            .unwrap_err();
        assert!(err.contains("duplicate part `R`"), "got: {err}");
        assert!(err.contains("line 2"), "got: {err}");
    }

    #[test]
    fn manifest_rejects_unknown_key_kind_and_stray_lines() {
        assert!(
            parse_manifest("part R footprint=a.kicad_mod color=red\n")
                .unwrap_err()
                .contains("unknown token `color=red`")
        );
        assert!(
            parse_manifest("part R footprint=a.kicad_mod {\n role 1 X driver\n}\n")
                .unwrap_err()
                .contains("unknown role kind `driver`")
        );
        assert!(
            parse_manifest("role 1 X passive\n")
                .unwrap_err()
                .contains("only valid inside")
        );
        assert!(parse_manifest("}\n").unwrap_err().contains("no open role"));
        assert!(
            parse_manifest("frobnicate\n")
                .unwrap_err()
                .contains("unknown manifest directive")
        );
        assert!(
            parse_manifest("part R footprint=a.kicad_mod {\n role 1 X passive\n")
                .unwrap_err()
                .contains("never closed")
        );
        assert!(
            parse_manifest("part R\n")
                .unwrap_err()
                .contains("missing `footprint=")
        );
    }

    #[test]
    fn missing_manifest_is_an_error() {
        let err = load_library(Path::new("/nonexistent/library/dir")).unwrap_err();
        assert!(err.contains("eutectic.lib"), "got: {err}");
    }

    // ---- union -------------------------------------------------------------

    #[test]
    fn union_is_first_wins_and_deterministic() {
        let a = lib_of(&["R", "C"]);
        let mut b = lib_of(&["C", "L"]);
        // Distinguish B's `C` so we can prove A's copy won.
        b.get_mut("C").unwrap().name = "C-from-B".into();

        let libs = vec![("A".to_string(), a), ("B".to_string(), b)];
        let (merged, notes) = union(&libs);
        assert_eq!(
            merged.keys().collect::<Vec<_>>(),
            vec!["C", "L", "R"],
            "merged lib is the name union"
        );
        assert_eq!(
            merged["C"].name, "C",
            "first provider (A) wins the collision"
        );
        assert_eq!(
            notes,
            vec!["part `C` provided by both `A` and `B`; `A` wins".to_string()]
        );

        // Determinism: the same input yields byte-identical notes every time.
        let (_, notes2) = union(&libs);
        assert_eq!(notes, notes2);

        // Order is precedence: swapping the list flips the winner.
        let swapped = vec![libs[1].clone(), libs[0].clone()];
        let (merged_sw, notes_sw) = union(&swapped);
        assert_eq!(merged_sw["C"].name, "C-from-B");
        assert_eq!(
            notes_sw,
            vec!["part `C` provided by both `B` and `A`; `B` wins".to_string()]
        );
    }

    // ---- use_names ---------------------------------------------------------

    #[test]
    fn use_names_enumerates_in_source_order_first_wins() {
        let src = vec![
            GenDirective::Use { name: "poc".into() },
            GenDirective::Board {
                outline: crate::geom::Shape2D::rect(crate::doc::Point { x: 0, y: 0 }, 1000, 1000),
            },
            GenDirective::Use {
                name: "jellybean".into(),
            },
            GenDirective::Use { name: "poc".into() }, // repeat collapses
        ];
        assert_eq!(use_names(&src), vec!["poc", "jellybean"]);
        assert!(use_names(&[]).is_empty());
    }

    // ---- PartLib equality (PartDef: PartialEq) -----------------------------

    /// Loading the same package twice yields byte-for-byte equal libraries — the
    /// `PartDef: PartialEq` derive (a gui incidental finding: resolution tests want
    /// to assert two `PartLib`s are equal). `PartLib` is `BTreeMap<String, PartDef>`,
    /// so this exercises the whole `PartDef` field chain (pins, interfaces, graphics,
    /// texts, courtyard) through the derived `PartialEq`.
    #[test]
    fn load_library_twice_yields_equal_libs() {
        let dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../poc/parts");
        let a = load_library(&dir).expect("poc parts load");
        let b = load_library(&dir).expect("poc parts load again");
        assert_eq!(a, b, "the same package must load to equal PartLibs");
        assert!(!a.is_empty(), "the poc package defines parts");
    }
}
