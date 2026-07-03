//! Outline (TTF/OpenType) text — the second font slice (Decision 17, the continuation
//! of Decision 9). Where [`crate::font`] emits *centreline strokes* traced at a pen
//! width, this module emits **filled glyph outlines**: each glyph's TrueType contours
//! flatten to integer polygons, land in the [`region`](crate::region) kernel as
//! `outer ∖ counters`, and become one [`Shape2D::Area`] per glyph. Text then lowers
//! like any other filled graphic — silk export needs no new path.
//!
//! Authority is unchanged: the string + placement + height + layer stay the tier-1
//! truth; the `Area`s are derived tier-3 geometry, re-derived on every edit. A font is
//! a **user-supplied file** (the built-in stroke font stays the default); this is the
//! crate's only dependency (`ttf-parser`), confined to this module.
//!
//! # Metrics (match the stroke font's conventions)
//!
//! Glyphs scale so the **cap height** equals the authored `height` (not the em square,
//! which renders ~30 % smaller). The cap height in font units is taken from, in order:
//! the OS/2 `capHeight`; else the ink height of the `H` glyph; else `0.7 · unitsPerEm`.
//! Horizontal advance comes from `hmtx`. [`Justify::Left`] anchors the pen origin at the
//! local origin (baseline-left, like board text); [`Justify::Center`] centres the run's
//! **ink bounding box** on the origin — the *same* convention as
//! [`text_strokes`](crate::font::text_strokes), so swapping a footprint's font does not
//! shift its labels.
//!
//! # Winding
//!
//! TrueType outlines and the region kernel are **both** non-zero winding, but the kernel
//! labels outer rings CCW (positive area) and holes CW. TrueType's global handedness is
//! font-dependent, so after flattening we normalize once: if the glyph's total signed
//! area is negative, every ring is reversed. This flips the whole glyph, preserving the
//! *relative* orientation of counters (holes stay holes under non-zero winding) while
//! making outers read as CCW islands — what [`Region::islands`](crate::region::Region::islands)
//! / [`holes`](crate::region::Region::holes) and the reflection-safe
//! [`Shape2D::map_points`] expect.
//!
//! # Not covered (deliberate)
//!
//! Kerning (advances are per-glyph `hmtx` only — no `kern`/GPOS pair adjustment); font
//! embedding in the document; per-text font overrides (the font is doc-wide).
//!
//! # Caveats
//!
//! - **Glyph `Area`s are render/clearance-safe but not boolean-clean.** Flattened
//!   contours are self-intersection-robust for tessellation, drawing, and the tolerance
//!   DRC (which reads segments under the non-zero winding rule), but a glyph's rings may
//!   be non-simple or mutually overlapping (e.g. an italic that self-touches, or a
//!   diacritic overlapping its base). They therefore **must not enter the region boolean
//!   engine** (`union`/`difference`/`dilate`) without a normalization pass first.
//!   Consequently [`Shape2D::inflated`] with `d ≠ 0` is **not valid** on a glyph `Area`
//!   today: `d < 0` already panics (the region erosion guard), and `d > 0` would feed
//!   these rings to the offset kernel — don't. Placement (`map_points`) and bbox are
//!   fine; those never run booleans.
//! - **`from_path` resolves relative paths against the process CWD.** A `font` directive
//!   with a relative path is interpreted relative to wherever the tool runs, not the
//!   document's directory — prefer an **absolute path**. A document-relative base
//!   directory is future work.
//! - **The font is re-parsed per derivation.** Each `features`/silk pass calls
//!   [`resolve_font`](crate::elaborate::resolve_font), which reads and validates the file
//!   afresh (glyph lookups within one `text_regions` call reuse a single `Face`). Parsing
//!   only walks the table directory, so the cost is modest; a parsed-font cache is a
//!   deliberate follow-up, not built here.

use crate::doc::{Nm, Point};
use crate::font::Justify;
use crate::geom::{Path, Seg, Shape2D};
use crate::region::Region;

/// A parsed outline font. Owns the file bytes; a [`ttf_parser::Face`] borrows them and is
/// re-parsed per operation (parsing only validates the table directory — cheap). Built
/// once per lowering and reused across every glyph of every label.
#[derive(Clone, Debug)]
pub struct TtfFont {
    data: Vec<u8>,
}

impl TtfFont {
    /// Parse a font from its raw bytes, validating the table directory. `Err` carries a
    /// human-readable reason (the caller degrades to the stroke font — never fails the
    /// doc).
    pub fn from_bytes(data: Vec<u8>) -> Result<TtfFont, String> {
        // Validate up-front so later `face()` calls can rely on it; discard the borrowed
        // face (it cannot outlive this scope alongside owned `data`).
        ttf_parser::Face::parse(&data, 0).map_err(|e| format!("{e}"))?;
        Ok(TtfFont { data })
    }

    /// Load a font from a filesystem path. `Err` distinguishes I/O (missing/unreadable)
    /// from parse failure so the diagnostic can say which. A **relative** `path` resolves
    /// against the process working directory (not the document's dir) — prefer an absolute
    /// path; a doc-relative base is future work.
    pub fn from_path(path: &std::path::Path) -> Result<TtfFont, String> {
        let data =
            std::fs::read(path).map_err(|e| format!("cannot read `{}`: {e}", path.display()))?;
        TtfFont::from_bytes(data)
    }

    /// The borrowed face. Safe to `expect` — [`from_bytes`](TtfFont::from_bytes) already
    /// parsed the same bytes successfully.
    fn face(&self) -> ttf_parser::Face<'_> {
        ttf_parser::Face::parse(&self.data, 0).expect("validated in TtfFont::from_bytes")
    }
}

/// Lower `string` to one [`Shape2D::Area`] per inked glyph in a **local** frame at world
/// `height`, honouring `justify` — the outline-font sibling of
/// [`text_strokes`](crate::font::text_strokes). Space and other ink-less glyphs advance
/// the pen but emit no shape. An unknown character (no `cmap` entry) renders the font's
/// `.notdef` when it has ink, else a [`fallback_box`] (the outline analogue of the stroke
/// font's fallback box), so it is visibly wrong rather than dropped. Lowercase is looked
/// up directly — a real font stops the stroke font's case-folding.
pub fn text_regions(string: &str, height: Nm, justify: Justify, font: &TtfFont) -> Vec<Shape2D> {
    let face = font.face();
    let cap = cap_height_units(&face).max(1);
    // Chord tolerance for curve flattening, scaled to the text height: ~1 % of cap
    // height, floored at 1 nm. Coarser than the stroke font would need but far finer
    // than any silk process — glyph curves read as smooth, segment counts stay modest.
    let tol = (height / 100).max(1);

    let mut shapes: Vec<Shape2D> = Vec::new();
    let mut pen_units: i64 = 0; // pen x, in font units (advance accumulates before scaling)
    for ch in string.chars() {
        let gid = face.glyph_index(ch);
        let dx = scale(pen_units, height, cap);
        match gid {
            Some(g) => {
                if let Some(area) = glyph_area(&face, g, height, cap, tol, dx) {
                    shapes.push(area);
                }
                pen_units += face.glyph_hor_advance(g).unwrap_or(0) as i64;
            }
            None => {
                // No cmap entry: `.notdef` (glyph 0) if it carries ink, else a fallback box.
                let notdef = ttf_parser::GlyphId(0);
                if let Some(area) = glyph_area(&face, notdef, height, cap, tol, dx) {
                    shapes.push(area);
                    pen_units += face.glyph_hor_advance(notdef).unwrap_or(0) as i64;
                } else {
                    let (area, adv_nm) = fallback_box(height, dx);
                    shapes.push(area);
                    // Advance is already in world nm; back it into font units so the shared
                    // accumulator stays consistent.
                    pen_units += mul_div_round(adv_nm, cap, height);
                }
            }
        }
    }

    if justify == Justify::Center
        && let Some((lo, hi)) = union_bbox(&shapes)
    {
        let (ox, oy) = ((lo.x + hi.x) / 2, (lo.y + hi.y) / 2);
        shapes = shapes
            .into_iter()
            .map(|s| {
                s.map_points(|p| Point {
                    x: p.x - ox,
                    y: p.y - oy,
                })
            })
            .collect();
    }
    shapes
}

/// The cap height in font units, per the documented cascade: OS/2 `capHeight`, else the
/// `H` glyph's ink height, else `0.7 · unitsPerEm`.
fn cap_height_units(face: &ttf_parser::Face) -> i64 {
    if let Some(c) = face.capital_height()
        && c > 0
    {
        return c as i64;
    }
    if let Some(g) = face.glyph_index('H')
        && let Some(bb) = face.glyph_bounding_box(g)
        && bb.y_max > bb.y_min
    {
        return (bb.y_max - bb.y_min) as i64;
    }
    (face.units_per_em() as i64 * 7) / 10
}

/// Outline glyph `g`, flatten its contours to a normalized [`Region`], and wrap it as a
/// [`Shape2D::Area`] offset by `dx` (world nm, the pen position). `None` when the glyph
/// has no ink (space, empty `.notdef`) or degenerates to < 3 vertices per ring.
fn glyph_area(
    face: &ttf_parser::Face,
    g: ttf_parser::GlyphId,
    height: Nm,
    cap: i64,
    tol: Nm,
    dx: Nm,
) -> Option<Shape2D> {
    let mut out = Outliner {
        height,
        cap,
        dx,
        cur: None,
        contours: Vec::new(),
    };
    face.outline_glyph(g, &mut out)?;
    out.flush();

    let rings: Vec<Vec<Point>> = out
        .contours
        .iter()
        .map(|c| c.flatten(tol))
        .filter(|r| r.len() >= 3)
        .collect();
    if rings.is_empty() {
        return None;
    }
    let mut region = Region::new(rings);
    // Normalize global winding so outers read CCW (positive) / counters CW — see the
    // module-level winding note. A single reversal preserves the non-zero-winding fill.
    if region.area2() < 0 {
        for ring in &mut region.rings {
            ring.reverse();
        }
    }
    Some(Shape2D::Area { region })
}

/// Accumulates a glyph's contours as integer-`Nm` [`Path`]s while `ttf-parser` walks the
/// outline. Each incoming font-unit coordinate is scaled to nm and rounded **once**
/// (deterministic IEEE round) at capture, so the downstream integer de Casteljau flatten
/// stays exact.
struct Outliner {
    height: Nm,
    cap: i64,
    dx: Nm,
    cur: Option<Path>,
    contours: Vec<Path>,
}

impl Outliner {
    /// Font-unit `(x, y)` → world-nm [`Point`], with the glyph's pen offset `dx` on x.
    fn map(&self, x: f32, y: f32) -> Point {
        Point {
            x: scale_f(x, self.height, self.cap) + self.dx,
            y: scale_f(y, self.height, self.cap),
        }
    }
    /// Finish the current contour (if it has any edges) into `contours`.
    fn flush(&mut self) {
        if let Some(p) = self.cur.take()
            && !p.segs.is_empty()
        {
            self.contours.push(p);
        }
    }
}

impl ttf_parser::OutlineBuilder for Outliner {
    fn move_to(&mut self, x: f32, y: f32) {
        self.flush();
        self.cur = Some(Path {
            start: self.map(x, y),
            segs: Vec::new(),
        });
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let end = self.map(x, y);
        if let Some(p) = &mut self.cur {
            p.segs.push(Seg::Line { end });
        }
    }
    fn quad_to(&mut self, x1: f32, y1: f32, x: f32, y: f32) {
        let ctrl = self.map(x1, y1);
        let end = self.map(x, y);
        if let Some(p) = &mut self.cur {
            p.segs.push(Seg::Quadratic { ctrl, end });
        }
    }
    fn curve_to(&mut self, x1: f32, y1: f32, x2: f32, y2: f32, x: f32, y: f32) {
        let c1 = self.map(x1, y1);
        let c2 = self.map(x2, y2);
        let end = self.map(x, y);
        if let Some(p) = &mut self.cur {
            p.segs.push(Seg::Cubic { c1, c2, end });
        }
    }
    fn close(&mut self) {
        self.flush();
    }
}

/// The outline analogue of the stroke font's fallback box (an unknown glyph with no
/// `.notdef` ink): a rectangular **frame** `Area` at cap height, proportioned like the
/// stroke cell (body 4 wide of a 6-tall cap, ~1/8-height wall). Returns the shape offset
/// by `dx` and its advance in world nm.
fn fallback_box(height: Nm, dx: Nm) -> (Shape2D, Nm) {
    let w = height * 4 / 6; // stroke cell body: 4 units wide of a 6-unit cap height
    let t = (height / 8).max(1); // wall thickness ≈ the stroke pen width
    let (x0, x1, y0, y1) = (dx, dx + w, 0, height);
    // Outer ring CCW; inner ring CW (a hole) — the frame reads through as a box outline.
    let outer = vec![
        Point { x: x0, y: y0 },
        Point { x: x1, y: y0 },
        Point { x: x1, y: y1 },
        Point { x: x0, y: y1 },
    ];
    let inner = vec![
        Point {
            x: x0 + t,
            y: y0 + t,
        },
        Point {
            x: x0 + t,
            y: y1 - t,
        },
        Point {
            x: x1 - t,
            y: y1 - t,
        },
        Point {
            x: x1 - t,
            y: y0 + t,
        },
    ];
    let region = Region::new(vec![outer, inner]);
    let advance = height; // stroke GLYPH_ADVANCE (6) at cap-height scale (÷6) = height
    (Shape2D::Area { region }, advance)
}

/// Bounding box over every vertex of every shape (the run's ink extent), or `None` for an
/// all-ink-less run.
fn union_bbox(shapes: &[Shape2D]) -> Option<(Point, Point)> {
    let mut acc: Option<(Point, Point)> = None;
    for s in shapes {
        if let Shape2D::Area { region } = s
            && let Some((lo, hi)) = region.bbox()
        {
            acc = Some(match acc {
                None => (lo, hi),
                Some((alo, ahi)) => (
                    Point {
                        x: alo.x.min(lo.x),
                        y: alo.y.min(lo.y),
                    },
                    Point {
                        x: ahi.x.max(hi.x),
                        y: ahi.y.max(hi.y),
                    },
                ),
            });
        }
    }
    acc
}

/// `round(v · height / cap)` for a font-unit coordinate `v` — the single rounding from
/// font units to nm. `f64` for a platform-deterministic round; `v` is integral for
/// `glyf` (lossless) and rounds to the font-unit grid for fractional CFF coordinates.
fn scale_f(v: f32, height: Nm, cap: i64) -> Nm {
    ((v as f64) * (height as f64) / (cap as f64)).round() as Nm
}

/// `round(v · height / cap)` for an integer font-unit value (advances), in exact i128.
fn scale(v: i64, height: Nm, cap: i64) -> Nm {
    mul_div_round(v, height, cap)
}

/// `round(a · b / d)` in i128 with round-half-away-from-zero, for positive `d`.
fn mul_div_round(a: i64, b: i64, d: i64) -> Nm {
    let num = a as i128 * b as i128;
    let d = d as i128;
    let half = d / 2;
    let r = if num >= 0 {
        (num + half) / d
    } else {
        (num - half) / d
    };
    r as Nm
}

// ----------------------------------------------------------------------------
// Test fixture: a minimal, hand-assembled TrueType font.
//
// Tests must not depend on system fonts, so we synthesize a valid TTF byte array in
// code (self-documenting, deterministic, no checked-in binary). It carries the required
// tables (head, maxp, hhea, hmtx, cmap fmt-12, loca long, glyf) and four glyphs:
//   0 `.notdef`  — empty (no ink), so an unknown char exercises the fallback box.
//   1 `H`        — a solid box (straight lines); also the cap-height source (no OS/2).
//   2 `O`        — an outer ring + a counter (a hole), to prove islands stay islands.
//   3 `o`        — a quadratic circle (off-curve points), exercising quad flattening;
//                  a lowercase glyph, to prove a real font stops case-folding.
// ----------------------------------------------------------------------------

/// Build the minimal test font (see the module fixture note). `pub(crate)` so lowering
/// tests in other modules can drive real TTF text.
#[cfg(test)]
pub(crate) fn build_test_ttf() -> Vec<u8> {
    fn be16(v: u16, out: &mut Vec<u8>) {
        out.extend_from_slice(&v.to_be_bytes());
    }
    fn bei16(v: i16, out: &mut Vec<u8>) {
        out.extend_from_slice(&v.to_be_bytes());
    }
    fn be32(v: u32, out: &mut Vec<u8>) {
        out.extend_from_slice(&v.to_be_bytes());
    }

    // One simple glyph from absolute coordinates. `on` marks on-curve points; all coords
    // are emitted as plain int16 deltas (no short/same optimization).
    fn simple_glyph(ends: &[u16], on: &[bool], xs: &[i16], ys: &[i16]) -> Vec<u8> {
        let (xmin, ymin, xmax, ymax) = (
            *xs.iter().min().unwrap(),
            *ys.iter().min().unwrap(),
            *xs.iter().max().unwrap(),
            *ys.iter().max().unwrap(),
        );
        let mut g = Vec::new();
        bei16(ends.len() as i16, &mut g); // numberOfContours
        bei16(xmin, &mut g);
        bei16(ymin, &mut g);
        bei16(xmax, &mut g);
        bei16(ymax, &mut g);
        for &e in ends {
            be16(e, &mut g);
        }
        be16(0, &mut g); // instructionLength
        for &o in on {
            g.push(if o { 0x01 } else { 0x00 }); // ON_CURVE_POINT / off-curve
        }
        let mut prev = 0i16;
        for &x in xs {
            bei16(x - prev, &mut g);
            prev = x;
        }
        prev = 0;
        for &y in ys {
            bei16(y - prev, &mut g);
            prev = y;
        }
        while g.len() % 2 != 0 {
            g.push(0); // word-align each glyph
        }
        g
    }

    // Glyph outlines (font units, unitsPerEm = 1000).
    let g_notdef: Vec<u8> = Vec::new(); // empty: no ink
    let g_h = simple_glyph(
        &[3],
        &[true, true, true, true],
        &[0, 600, 600, 0],
        &[0, 0, 700, 700],
    );
    let g_o = simple_glyph(
        &[3, 7],
        &[true, true, true, true, true, true, true, true],
        &[0, 600, 600, 0, 150, 150, 450, 450],
        &[0, 0, 700, 700, 150, 550, 550, 150],
    );
    // A quadratic circle: on-curve at the 4 mid-edges, off-curve at the 4 corners.
    let g_o_lower = simple_glyph(
        &[7],
        &[true, false, true, false, true, false, true, false],
        &[500, 500, 250, 0, 0, 0, 250, 500],
        &[350, 600, 600, 600, 350, 100, 100, 100],
    );

    // glyf table + long loca offsets (numGlyphs + 1).
    let mut glyf = Vec::new();
    let mut loca_offsets = vec![0u32];
    for g in [&g_notdef, &g_h, &g_o, &g_o_lower] {
        glyf.extend_from_slice(g);
        loca_offsets.push(glyf.len() as u32);
    }
    let mut loca = Vec::new();
    for o in &loca_offsets {
        be32(*o, &mut loca);
    }

    // head (indexToLocFormat = 1 → long loca).
    let mut head = Vec::new();
    be16(1, &mut head); // majorVersion
    be16(0, &mut head); // minorVersion
    be32(0x0001_0000, &mut head); // fontRevision
    be32(0, &mut head); // checkSumAdjustment (ttf-parser ignores)
    be32(0x5F0F_3CF5, &mut head); // magicNumber
    be16(0, &mut head); // flags
    be16(1000, &mut head); // unitsPerEm
    be32(0, &mut head); // created (hi/lo)
    be32(0, &mut head);
    be32(0, &mut head); // modified
    be32(0, &mut head);
    bei16(0, &mut head); // xMin
    bei16(0, &mut head); // yMin
    bei16(600, &mut head); // xMax
    bei16(700, &mut head); // yMax
    be16(0, &mut head); // macStyle
    be16(8, &mut head); // lowestRecPPEM
    bei16(2, &mut head); // fontDirectionHint
    bei16(1, &mut head); // indexToLocFormat = long
    bei16(0, &mut head); // glyphDataFormat

    // maxp version 1.0.
    let mut maxp = Vec::new();
    be32(0x0001_0000, &mut maxp); // version
    be16(4, &mut maxp); // numGlyphs
    be16(8, &mut maxp); // maxPoints
    be16(2, &mut maxp); // maxContours
    for _ in 0..11 {
        be16(0, &mut maxp); // remaining 11 maxima → 14 u16 fields total (32-byte v1.0 table)
    }

    // hhea (numberOfHMetrics = 4).
    let mut hhea = Vec::new();
    be16(1, &mut hhea);
    be16(0, &mut hhea);
    bei16(800, &mut hhea); // ascender
    bei16(-200, &mut hhea); // descender
    bei16(0, &mut hhea); // lineGap
    be16(700, &mut hhea); // advanceWidthMax
    bei16(0, &mut hhea); // minLeftSideBearing
    bei16(0, &mut hhea); // minRightSideBearing
    bei16(600, &mut hhea); // xMaxExtent
    bei16(1, &mut hhea); // caretSlopeRise
    bei16(0, &mut hhea); // caretSlopeRun
    bei16(0, &mut hhea); // caretOffset
    for _ in 0..4 {
        bei16(0, &mut hhea); // reserved
    }
    bei16(0, &mut hhea); // metricDataFormat
    be16(4, &mut hhea); // numberOfHMetrics

    // hmtx (advanceWidth, lsb) for each of 4 glyphs.
    let mut hmtx = Vec::new();
    for adv in [600u16, 700, 700, 550] {
        be16(adv, &mut hmtx);
        bei16(0, &mut hmtx); // lsb
    }

    // cmap: one format-12 subtable mapping H/O/o.
    let mut cmap = Vec::new();
    be16(0, &mut cmap); // version
    be16(1, &mut cmap); // numTables
    be16(3, &mut cmap); // platformID (Windows)
    be16(10, &mut cmap); // encodingID (UCS-4)
    be32(12, &mut cmap); // offset to subtable
    let groups: [(u32, u32); 3] = [(0x48, 1), (0x4F, 2), (0x6F, 3)]; // H, O, o → glyph ids
    be16(12, &mut cmap); // format
    be16(0, &mut cmap); // reserved
    be32(16 + 12 * groups.len() as u32, &mut cmap); // length
    be32(0, &mut cmap); // language
    be32(groups.len() as u32, &mut cmap); // numGroups
    for (code, gid) in groups {
        be32(code, &mut cmap); // startCharCode
        be32(code, &mut cmap); // endCharCode
        be32(gid, &mut cmap); // startGlyphID
    }

    // Assemble: offset table + directory (tags sorted ascending) + 4-byte-aligned tables.
    let tables: [(&[u8; 4], Vec<u8>); 7] = [
        (b"cmap", cmap),
        (b"glyf", glyf),
        (b"head", head),
        (b"hhea", hhea),
        (b"hmtx", hmtx),
        (b"loca", loca),
        (b"maxp", maxp),
    ];
    let num_tables = tables.len() as u16;
    let mut font = Vec::new();
    be32(0x0001_0000, &mut font); // sfntVersion (TrueType)
    be16(num_tables, &mut font);
    // searchRange / entrySelector / rangeShift for numTables = 7 (largest pow2 ≤ 7 is 4).
    be16(64, &mut font); // searchRange = 4 * 16
    be16(2, &mut font); // entrySelector = log2(4)
    be16(num_tables * 16 - 64, &mut font); // rangeShift

    let dir_len = 12 + tables.len() * 16;
    let mut body_offset = dir_len;
    let mut offsets = Vec::new();
    for (_, data) in &tables {
        offsets.push(body_offset as u32);
        body_offset += data.len();
        body_offset = (body_offset + 3) & !3; // 4-byte align next table
    }
    for (i, (tag, data)) in tables.iter().enumerate() {
        font.extend_from_slice(*tag);
        be32(0, &mut font); // checksum (ttf-parser does not verify)
        be32(offsets[i], &mut font);
        be32(data.len() as u32, &mut font); // length (unpadded)
    }
    for (i, (_, data)) in tables.iter().enumerate() {
        debug_assert_eq!(font.len(), offsets[i] as usize, "table offset mismatch");
        font.extend_from_slice(data);
        while font.len() % 4 != 0 {
            font.push(0);
        }
    }
    font
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::region::signed_area2;

    /// The hand-built fixture parses, and its metrics come out as designed: cap height
    /// falls back to the `H` ink height (700 units, no OS/2), so at height 700 000 nm the
    /// scale is exactly 1000 nm / font-unit.
    #[test]
    fn fixture_parses_and_scales() {
        let font = TtfFont::from_bytes(build_test_ttf()).expect("fixture must parse");
        let face = font.face();
        assert_eq!(face.units_per_em(), 1000);
        assert_eq!(face.capital_height(), None, "fixture has no OS/2");
        assert_eq!(cap_height_units(&face), 700, "cap height from H ink");
    }

    /// `H` is one solid island: a single ring, positive (CCW) area, spanning the scaled
    /// box exactly (600→600 000 nm wide, 700→700 000 nm = the authored height tall).
    #[test]
    fn solid_glyph_is_one_ccw_island() {
        let font = TtfFont::from_bytes(build_test_ttf()).unwrap();
        let shapes = text_regions("H", 700_000, Justify::Left, &font);
        assert_eq!(shapes.len(), 1);
        let Shape2D::Area { region } = &shapes[0] else {
            panic!("expected an Area");
        };
        assert_eq!(region.rings.len(), 1, "H has no counter");
        assert!(signed_area2(&region.rings[0]) > 0, "outer ring is CCW");
        let (lo, hi) = region.bbox().unwrap();
        assert_eq!((lo.x, lo.y), (0, 0));
        assert_eq!((hi.x, hi.y), (600_000, 700_000));
    }

    /// `O` is an outer ring plus a counter: two rings, opposite winding, and the centre of
    /// the annulus is *outside* the filled region (the hole reads through).
    #[test]
    fn counter_glyph_has_a_hole() {
        let font = TtfFont::from_bytes(build_test_ttf()).unwrap();
        let shapes = text_regions("O", 700_000, Justify::Left, &font);
        let Shape2D::Area { region } = &shapes[0] else {
            panic!("expected an Area");
        };
        assert_eq!(region.rings.len(), 2, "outer + counter");
        assert!(signed_area2(&region.rings[0]) > 0, "outer CCW");
        assert!(signed_area2(&region.rings[1]) < 0, "counter CW");
        // Centre of the glyph sits in the counter → not filled.
        assert!(!region.contains_point(Point {
            x: 300_000,
            y: 350_000
        }));
        // A point in the outer wall → filled.
        assert!(region.contains_point(Point {
            x: 50_000,
            y: 350_000
        }));
    }

    /// The quadratic-circle glyph `o` flattens to a smooth many-vertex ring (the off-curve
    /// control points forced subdivision) that stays within its scaled bounding box.
    #[test]
    fn quadratic_glyph_flattens_smoothly() {
        let font = TtfFont::from_bytes(build_test_ttf()).unwrap();
        let shapes = text_regions("o", 700_000, Justify::Left, &font);
        let Shape2D::Area { region } = &shapes[0] else {
            panic!("expected an Area");
        };
        assert!(
            region.rings[0].len() > 8,
            "quadratics subdivided into many segments, got {}",
            region.rings[0].len()
        );
        let (lo, hi) = region.bbox().unwrap();
        // Circle spans x 0..500, y 100..600 in font units → ×1000 nm.
        assert!(
            lo.x >= 0 && hi.x <= 500_000,
            "x within box: {}..{}",
            lo.x,
            hi.x
        );
        assert!(
            lo.y >= 100_000 && hi.y <= 600_000,
            "y within box: {}..{}",
            lo.y,
            hi.y
        );
    }

    /// A real font stops case-folding: `o` and `O` resolve to *different* glyphs (the
    /// stroke font would fold them together).
    #[test]
    fn lowercase_is_not_case_folded() {
        let font = TtfFont::from_bytes(build_test_ttf()).unwrap();
        let upper = text_regions("O", 700_000, Justify::Left, &font);
        let lower = text_regions("o", 700_000, Justify::Left, &font);
        let (Shape2D::Area { region: u }, Shape2D::Area { region: l }) = (&upper[0], &lower[0])
        else {
            panic!("areas");
        };
        assert_ne!(u.bbox(), l.bbox(), "distinct glyphs, distinct extents");
    }

    /// An unknown character (no cmap entry) with an ink-less `.notdef` renders the
    /// fallback box: one framed `Area` (outer + hole).
    #[test]
    fn unknown_char_renders_fallback_box() {
        let font = TtfFont::from_bytes(build_test_ttf()).unwrap();
        let shapes = text_regions("@", 700_000, Justify::Left, &font);
        assert_eq!(shapes.len(), 1);
        let Shape2D::Area { region } = &shapes[0] else {
            panic!("expected an Area");
        };
        assert_eq!(region.rings.len(), 2, "a box frame: outer + hole");
    }

    /// Multi-glyph runs advance the pen: `HH` produces two boxes, the second offset right
    /// by the `H` advance (700 units → 700 000 nm).
    #[test]
    fn advance_places_successive_glyphs() {
        let font = TtfFont::from_bytes(build_test_ttf()).unwrap();
        let shapes = text_regions("HH", 700_000, Justify::Left, &font);
        assert_eq!(shapes.len(), 2);
        let bb = |s: &Shape2D| {
            let Shape2D::Area { region } = s else {
                panic!()
            };
            region.bbox().unwrap()
        };
        let (lo0, _) = bb(&shapes[0]);
        let (lo1, _) = bb(&shapes[1]);
        assert_eq!(lo0.x, 0);
        assert_eq!(lo1.x, 700_000, "second H advanced by hmtx");
    }

    /// A mixed run yields one inked `Area` per glyph (no dropping, no merging).
    #[test]
    fn mixed_run_yields_one_area_per_glyph() {
        let font = TtfFont::from_bytes(build_test_ttf()).unwrap();
        let shapes = text_regions("HOo", 700_000, Justify::Left, &font);
        assert_eq!(shapes.len(), 3, "three inked glyphs");
    }

    /// Centre justification puts the run's ink bbox on the origin (both axes), matching the
    /// stroke font's convention.
    #[test]
    fn center_justify_centres_ink_bbox() {
        let font = TtfFont::from_bytes(build_test_ttf()).unwrap();
        let shapes = text_regions("H", 700_000, Justify::Center, &font);
        let Shape2D::Area { region } = &shapes[0] else {
            panic!()
        };
        let (lo, hi) = region.bbox().unwrap();
        assert_eq!(lo.x, -(hi.x), "x centred: {} vs {}", lo.x, hi.x);
        assert_eq!(lo.y, -(hi.y), "y centred: {} vs {}", lo.y, hi.y);
    }
}
