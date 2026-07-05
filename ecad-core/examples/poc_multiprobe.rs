//! PoC round 2: chip-down RP2350A (QFN-60) multi-SWD debug probe board, authored
//! entirely through the ecad-core framework and rebuilt as a real **4-layer** design.
//! Run with `cargo run --example poc_multiprobe`.
//!
//! Design = a bare RP2350A acting as 10 independent SWD probes, each on a 3-pin
//! JST-SH header (pin1=SWCLK, pin2=GND, pin3=SWDIO), USB-powered, UF2/BOOTSEL.
//! Faithful to the original Waveshare-module-based probe but with the support
//! circuitry (3V3 reg, core buck L+C, 12 MHz crystal, QSPI flash, USB front-end,
//! buttons, status LED) made explicit on-board.
//!
//! Round 2 exercises the *current* pipeline end to end — the round-1 netlist/topology
//! is kept verbatim (it was good); what is new is everything the geometry-model
//! convergence added since:
//!  - a real **4-layer stackup** authored as `Slab` directives (F.Cu / In1.Cu /
//!    In2.Cu / B.Cu + F/B mask + F/B silk + F/B fab), so fab SVGs + real per-slab
//!    Gerbers materialise;
//!  - **inner-layer copper planes**: GND poured full-board on In1.Cu, +3V3 on In2.Cu;
//!  - a **text round-trip through `LoadText`** as the pipeline (serialize the doc,
//!    parse it back, re-elaborate, and run place/route on the *parsed* doc) — the
//!    code+lockfile model applied to the capstone;
//!  - authored **NPTH mounting holes**, board **title/rev silk text**, R/C **value
//!    labels** via the class registry, and a **rounded board outline**;
//!  - a **PromoteRoutes** step so the `# routes` state zone carries mixed
//!    pinned/free provenance.
//!
//! USER DECISIONS honoured here:
//!  1. RP2350A / QFN-60 (GPIO0-29), sourced from KiCad's official library.
//!  2. Clean SEQUENTIAL GPIO map: chN -> GP(2N-2)/GP(2N-1); J1=GP0/1 ... J10=GP18/19.
//!  3. A real 4-layer stack (signal / GND / PWR / signal) — inner planes are now poured
//!     copper, not documentary intent. (The router is still a 2-layer grid — it routes
//!     on the outer layers and drops a via to a plane; the plane connectivity is honest
//!     copper the ratsnest checks.)
//!  4. No probe-self-debug header; USB UF2 + BOOTSEL (+ RUN reset) only.

use ecad_core::autoroute::autoroute;
use ecad_core::command::{Command, Transaction};
use ecad_core::diagnostic::render;
use ecad_core::doc::{MM, Point};
use ecad_core::elaborate::{GenDirective as G, RegionDecl, Source};
use ecad_core::export::{excellon_drill, fab_svg_set, gerber_set, netlist, placement_csv, svg};
use ecad_core::geom::{
    BOARD_THICKNESS, COPPER_THICKNESS, MASK_THICKNESS, Material, Role, SILK_THICKNESS, Shape2D,
    Slab, ZRange,
};
use ecad_core::history::History;
use ecad_core::id::NetId;
use ecad_core::library::load_library;
use ecad_core::query::{Engine, Key};
use ecad_core::route::DesignRules;
use ecad_core::schematic::{
    Align, Container, Direction, LayoutNode, SchematicLayout, Symbol, Wire, WireEnd,
};
use ecad_core::schematic_svg::schematic_svg;
use ecad_core::text::{serialize, serialize_schematic_block};
use std::collections::BTreeMap;

/// The `poc` library package: the manifest-driven directory the `use poc`
/// directive below names. Loaded through the engine's [`load_library`] — the old
/// hand-rolled `build_lib()` now lives as data in `poc/parts/ecad.lib`.
const PARTS: &str = "poc/parts";

// ---------------------------------------------------------------------------
// Source authoring (the netlist + placement program)
// ---------------------------------------------------------------------------

type Pin = (String, String);
fn p(c: &str, pin: &str) -> Pin {
    (c.to_string(), pin.to_string())
}

/// Accumulates GenDirectives and named nets (merging members that share a name).
struct Builder {
    s: Source,
    nets: BTreeMap<String, Vec<Pin>>,
    net_order: Vec<String>,
}
impl Builder {
    fn new() -> Self {
        Builder {
            s: Vec::new(),
            nets: BTreeMap::new(),
            net_order: Vec::new(),
        }
    }
    fn inst(&mut self, path: &str, part: &str) {
        self.s.push(G::Instance {
            path: path.into(),
            part: part.into(),
            params: std::collections::BTreeMap::new(),
            label: None,
        });
    }
    /// Instantiate a passive carrying a `value` param — the class registry's seeded
    /// `{value}` template renders it as the silk label (Decision 14). Same as `inst`
    /// but with `p:value=<v>` set.
    fn inst_val(&mut self, path: &str, part: &str, value: &str) {
        let mut params = std::collections::BTreeMap::new();
        params.insert("value".to_string(), value.to_string());
        self.s.push(G::Instance {
            path: path.into(),
            part: part.into(),
            params,
            label: None,
        });
    }
    /// One board-stackup slab (a named z-interval + role + material).
    fn slab(&mut self, name: &str, z: ZRange, role: Role, material: Option<&str>) {
        self.s.push(G::Slab(Slab {
            name: name.into(),
            z,
            role,
            material: material.map(Material::named),
        }));
    }
    /// A full-board copper pour on a named copper slab, carrying a net (an inner plane).
    fn plane(&mut self, net: &str, layer: &str, min: Point, max: Point) {
        self.s.push(G::Region(RegionDecl {
            shape: Shape2D::polygon(vec![
                Point { x: min.x, y: min.y },
                Point { x: max.x, y: min.y },
                Point { x: max.x, y: max.y },
                Point { x: min.x, y: max.y },
            ]),
            role: Role::Conductor,
            net: Some(net.into()),
            layer: layer.into(),
        }));
    }
    /// An authored NPTH mounting hole (Decision 16b).
    fn hole(&mut self, x: i64, y: i64, dia_mm_x10: i64) {
        self.s.push(G::Hole {
            center: Point::mm(x, y),
            dia: dia_mm_x10 * MM / 10,
        });
    }
    /// Board-level silk text (title, revision, etc.) at `at`, cap-height `h_mm_x10`.
    fn text(&mut self, string: &str, x: i64, y: i64, h_mm_x10: i64, layer: &str) {
        self.s.push(G::Text {
            string: string.into(),
            at: Point::mm(x, y),
            height: h_mm_x10 * MM / 10,
            layer: layer.into(),
            orient: ecad_core::doc::Orient::IDENTITY,
        });
    }
    fn place(&mut self, path: &str, x: i64, y: i64) {
        self.s.push(G::Place {
            path: path.into(),
            pos: Point::mm(x, y),
        });
    }
    fn fix(&mut self, path: &str, x: i64, y: i64) {
        self.s.push(G::Fix {
            path: path.into(),
            pos: Point::mm(x, y),
        });
    }
    fn near_pin(&mut self, a: &str, bc: &str, bp: &str, within_mm: i64) {
        self.s.push(G::NearPin {
            a: a.into(),
            b_comp: bc.into(),
            b_pin: bp.into(),
            within: within_mm * MM,
        });
    }
    /// Add pins to a named net (creating it on first use).
    fn net(&mut self, name: &str, pins: &[Pin]) {
        if !self.nets.contains_key(name) {
            self.net_order.push(name.to_string());
        }
        self.nets
            .entry(name.to_string())
            .or_default()
            .extend_from_slice(pins);
    }
    fn finish(mut self) -> Source {
        for name in &self.net_order {
            self.s.push(G::ConnectPins {
                net: name.clone(),
                pins: self.nets[name].clone(),
            });
        }
        self.s
    }
}

fn build_source() -> Source {
    let mut b = Builder::new();

    // --- library declaration (library packages, slice 1) --------------------
    // The FIRST source directive: the document declares — by NAME, never a path —
    // the library package its parts come from, so the serialized board.ecad opens
    // with `use poc` and is resolvable standalone. Inert to elaboration; `main`
    // below resolves the name to `poc/parts` and loads it.
    b.s.push(G::Use { name: "poc".into() });

    // --- 4-layer stackup (Decision 13) --------------------------------------
    // Honest z per side: bottom copper at [0,C]; two inner copper planes in the core;
    // top copper at [T-C, T]; mask + silk + fab extend contiguously outward each side.
    // The two dielectric cores/prepreg between the copper layers are Substrate slabs.
    // (Named z-intervals resolved away at elaboration; the same machinery as the
    // built-in default_2layer, extended to 4 copper layers + fab.)
    let (t, c) = (BOARD_THICKNESS, COPPER_THICKNESS);
    let (mask, silk) = (MASK_THICKNESS, SILK_THICKNESS);
    // Split the core into three dielectric bands with the two inner planes between.
    // Inner planes sit ~1/3 and ~2/3 through the board body.
    let in1_lo = t / 3;
    let in2_lo = 2 * t / 3;
    b.slab(
        "B.SilkS",
        ZRange::new(-mask - silk, -mask),
        Role::Marking,
        Some("ink"),
    );
    b.slab(
        "B.Mask",
        ZRange::new(-mask, 0),
        Role::Mask,
        Some("soldermask"),
    );
    b.slab("B.Cu", ZRange::new(0, c), Role::Conductor, Some("copper"));
    b.slab(
        "core1",
        ZRange::new(c, in1_lo),
        Role::Substrate,
        Some("FR4"),
    );
    b.slab(
        "In1.Cu",
        ZRange::new(in1_lo, in1_lo + c),
        Role::Conductor,
        Some("copper"),
    );
    b.slab(
        "core2",
        ZRange::new(in1_lo + c, in2_lo),
        Role::Substrate,
        Some("FR4"),
    );
    b.slab(
        "In2.Cu",
        ZRange::new(in2_lo, in2_lo + c),
        Role::Conductor,
        Some("copper"),
    );
    b.slab(
        "core3",
        ZRange::new(in2_lo + c, t - c),
        Role::Substrate,
        Some("FR4"),
    );
    b.slab(
        "F.Cu",
        ZRange::new(t - c, t),
        Role::Conductor,
        Some("copper"),
    );
    b.slab(
        "F.Mask",
        ZRange::new(t, t + mask),
        Role::Mask,
        Some("soldermask"),
    );
    b.slab(
        "F.SilkS",
        ZRange::new(t + mask, t + mask + silk),
        Role::Marking,
        Some("ink"),
    );
    // Fab drawing layers (Decision 15): zero-height Datum slabs just outboard of silk.
    // Their presence is what makes footprint fab graphics + fab SVGs materialise.
    let fab_top = t + mask + silk;
    let fab_bot = -mask - silk;
    b.slab("F.Fab", ZRange::new(fab_top, fab_top), Role::Datum, None);
    b.slab("B.Fab", ZRange::new(fab_bot, fab_bot), Role::Datum, None);

    // --- rounded-corner board outline, 56 x 44 mm, 3 mm corner radius -------
    // (Round-2 finding: the corner radius does NOT survive text serialization — see the
    // findings ledger; the outline is still authored honestly here.)
    b.s.push(G::Board {
        outline: Shape2D::round_rect(Point::mm(28, 22), 56 * MM, 44 * MM, 3 * MM),
    });

    // --- inner-layer planes (Task 2) ----------------------------------------
    // GND on In1.Cu, +3V3 on In2.Cu, each a full-board pour. The pour fill knocks out
    // foreign copper at clearance; a via that drops onto the plane joins the net's
    // island (ratsnest connectivity through the plane). Inset 0.5 mm from the edge.
    b.plane("GND", "In1.Cu", Point::mm(1, 1), Point::mm(55, 43));
    b.plane("+3V3", "In2.Cu", Point::mm(1, 1), Point::mm(55, 43));

    // --- core instances + coarse placement ---------------------------------
    b.inst("U1", "RP2350A");
    b.fix("U1", 28, 24); // QFN at board centre, mechanical datum
    b.inst("U2", "W25Q");
    b.place("U2", 38, 30);
    b.inst("Y1", "XTAL");
    b.place("Y1", 18, 30);
    b.inst("U3", "REG");
    b.place("U3", 14, 14);
    b.inst("L1", "IND");
    b.place("L1", 22, 14);
    b.inst("J11", "USBC");
    b.fix("J11", 28, 3); // USB-C on bottom edge
    b.inst("D1", "LED");
    b.place("D1", 40, 14);
    b.inst("SW1", "BTN"); // BOOTSEL
    b.place("SW1", 40, 38);
    b.inst("SW2", "BTN"); // RUN / reset
    b.place("SW2", 16, 38);

    // --- 10 SWD JST-SH headers on the two side edges -----------------------
    // J1-J5 left edge, J6-J10 right edge (cables exit outward, per spec ergonomics).
    let left_y = [7, 14, 21, 28, 35];
    let right_y = [7, 14, 21, 28, 35];
    for (i, &y) in left_y.iter().enumerate() {
        b.inst(&format!("J{}", i + 1), "JST_SH");
        b.fix(&format!("J{}", i + 1), 3, y);
    }
    for (i, &y) in right_y.iter().enumerate() {
        b.inst(&format!("J{}", i + 6), "JST_SH");
        b.fix(&format!("J{}", i + 6), 53, y);
    }

    // --- SWD channels: clean sequential GP map ------------------------------
    // chN: SWCLK = GP(2N-2), SWDIO = GP(2N-1). J pin1=SWCLK, pin2=GND, pin3=SWDIO.
    let ch_letters = ["A", "B", "C", "D", "E", "F", "G", "H", "I", "J"];
    for (ch, l) in ch_letters.iter().enumerate() {
        let j = format!("J{}", ch + 1);
        let clk_gp = format!("GPIO{}", 2 * ch);
        let dio_gp = format!("GPIO{}", 2 * ch + 1);
        b.net(&format!("{l}_SWCLK"), &[p("U1", &clk_gp), p(&j, "1")]);
        b.net(&format!("{l}_SWDIO"), &[p("U1", &dio_gp), p(&j, "3")]);
        b.net("GND", &[p(&j, "2"), p(&j, "MP")]); // header GND + mounting
    }

    // --- power rails --------------------------------------------------------
    // VBUS (5V) from USB-C -> regulator input. "VBUS" fans out to all four pads.
    b.net(
        "VBUS",
        &[
            p("J11", "VBUS"),
            p("U3", "VIN"),
            p("U3", "EN"), // EN tied high to VIN -> always on
        ],
    );
    // +3V3 rail from regulator output to every 3.3 V consumer. "IOVDD" fans out to
    // all six IOVDD pads — the duplicate-power-pin case that used to silently float.
    b.net(
        "+3V3",
        &[
            p("U3", "VOUT"),
            p("U1", "IOVDD"),
            p("U1", "QSPI_IOVDD"),
            p("U1", "USB_OTP_VDD"),
            p("U1", "ADC_AVDD"),
            p("U1", "VREG_VIN"),
            p("U2", "VCC"),
            p("D1", "VDD"),
        ],
    );

    // Core buck: VREG_LX -> L1 -> +DVDD (1.1V); VREG_FB senses +DVDD; PGND->GND.
    b.net("VREG_LX", &[p("U1", "VREG_LX"), p("L1", "1")]);
    // "DVDD" fans out to all three DVDD pads.
    b.net(
        "+DVDD",
        &[p("L1", "2"), p("U1", "VREG_FB"), p("U1", "DVDD")],
    );
    b.net("GND", &[p("U1", "GND"), p("U1", "VREG_PGND")]);

    // VREG_AVDD: 33R from +3V3 + 4.7uF to GND (RC filter).
    b.inst_val("R_AVDD", "R", "33");
    b.place("R_AVDD", 24, 18);
    b.net("+3V3", &[p("R_AVDD", "1")]);
    b.net("VREG_AVDD", &[p("R_AVDD", "2"), p("U1", "VREG_AVDD")]);

    // --- QSPI flash bus -----------------------------------------------------
    b.net("QSPI_SCLK", &[p("U1", "QSPI_SCLK"), p("U2", "CLK")]);
    b.net("QSPI_CS_N", &[p("U1", "~{QSPI_SS}"), p("U2", "CS_N")]);
    b.net("QSPI_SD0", &[p("U1", "QSPI_SD0"), p("U2", "IO0")]);
    b.net("QSPI_SD1", &[p("U1", "QSPI_SD1"), p("U2", "IO1")]);
    b.net("QSPI_SD2", &[p("U1", "QSPI_SD2"), p("U2", "IO2")]);
    b.net("QSPI_SD3", &[p("U1", "QSPI_SD3"), p("U2", "IO3")]);
    b.net("GND", &[p("U2", "GND")]);

    // --- crystal: XIN-Y1-XOUT, 1k series on XOUT side, 15pF load caps -------
    b.inst_val("R_X", "R", "1k");
    b.place("R_X", 16, 27);
    b.inst_val("C_X1", "C", "15pF");
    b.place("C_X1", 15, 33);
    b.inst_val("C_X2", "C", "15pF");
    b.place("C_X2", 21, 33);
    b.net("XIN", &[p("U1", "XIN"), p("Y1", "X1"), p("C_X1", "1")]);
    b.net("XOUT", &[p("U1", "XOUT"), p("R_X", "1")]);
    b.net("XTAL2", &[p("R_X", "2"), p("Y1", "X2"), p("C_X2", "1")]);
    b.net(
        "GND",
        &[
            p("Y1", "GNDa"),
            p("Y1", "GNDb"),
            p("C_X1", "2"),
            p("C_X2", "2"),
        ],
    );

    // --- USB front-end: 27R series on DP/DM, CC 5.1k pulldowns --------------
    b.inst_val("R_DP", "R", "27");
    b.place("R_DP", 25, 8);
    b.inst_val("R_DM", "R", "27");
    b.place("R_DM", 31, 8);
    b.inst_val("R_CC1", "R", "5.1k");
    b.place("R_CC1", 22, 5);
    b.inst_val("R_CC2", "R", "5.1k");
    b.place("R_CC2", 34, 5);
    b.net("USB_DP", &[p("U1", "USB_DP"), p("R_DP", "1")]);
    b.net("USB_DM", &[p("U1", "USB_DM"), p("R_DM", "1")]);
    b.net("DP_CONN", &[p("R_DP", "2"), p("J11", "DP")]);
    b.net("DM_CONN", &[p("R_DM", "2"), p("J11", "DM")]);
    b.net("CC1", &[p("J11", "CC1"), p("R_CC1", "1")]);
    b.net("CC2", &[p("J11", "CC2"), p("R_CC2", "1")]);
    b.net(
        "GND",
        &[
            p("R_CC1", "2"),
            p("R_CC2", "2"),
            p("J11", "GND"), // fans out to all four GND pads
            p("J11", "SHIELD"),
            p("U3", "GND"),
            p("D1", "GND"),
        ],
    );

    // --- BOOTSEL: 1k from QSPI_CS_N -> SW1 -> GND. RUN: SW2 RUN -> GND. ------
    b.inst_val("R_BOOT", "R", "1k");
    b.place("R_BOOT", 44, 34);
    b.net("QSPI_CS_N", &[p("R_BOOT", "1")]);
    b.net("BOOT_SW", &[p("R_BOOT", "2"), p("SW1", "1")]);
    b.net("GND", &[p("SW1", "2")]);
    b.net("RUN", &[p("U1", "RUN"), p("SW2", "1")]);
    b.net("GND", &[p("SW2", "2")]);

    // --- status LED on GP20 (GP16 was the module convention, but the sequential
    //     map now uses GP0-19 for the 10 channels, so the LED moves to GP20). ---
    b.net("LED_DIN", &[p("U1", "GPIO20"), p("D1", "DIN")]);

    // --- decoupling: one cap per power pin, placed near *that* pad ----------
    // (rail net, mcu pad, value-label-only). Each cap p1->rail, p2->GND. The cap
    // joins the rail by name; placement targets a *specific* pad, so the six IOVDD /
    // three DVDD pads are referenced by pad NUMBER (a name there would fan out — the
    // name selects the rail, the number selects the individual pad to sit beside).
    let decaps: &[(&str, &str)] = &[
        ("+3V3", "1"), // IOVDD pads, by pad number
        ("+3V3", "11"),
        ("+3V3", "20"),
        ("+3V3", "30"),
        ("+3V3", "38"),
        ("+3V3", "45"),
        ("+3V3", "QSPI_IOVDD"),
        ("+3V3", "USB_OTP_VDD"),
        ("+3V3", "ADC_AVDD"),
        ("+3V3", "VREG_VIN"),
        ("+DVDD", "6"), // DVDD pads, by pad number
        ("+DVDD", "23"),
        ("+DVDD", "39"),
        ("VREG_AVDD", "VREG_AVDD"),
    ];
    for (i, (rail, pad)) in decaps.iter().enumerate() {
        let c = format!("C{i}");
        b.inst_val(&c, "C", "100nF");
        b.near_pin(&c, "U1", pad, 3); // pull each decoupler within 3 mm of its pad
        b.net(rail, &[p(&c, "1")]);
        b.net("GND", &[p(&c, "2")]);
    }
    // Regulator in/out bulk caps.
    b.inst_val("C_IN", "C", "1uF");
    b.place("C_IN", 10, 18);
    b.inst_val("C_OUT", "C", "1uF");
    b.place("C_OUT", 18, 18);
    b.net("VBUS", &[p("C_IN", "1")]);
    b.net("GND", &[p("C_IN", "2")]);
    b.net("+3V3", &[p("C_OUT", "1")]);
    b.net("GND", &[p("C_OUT", "2")]);

    // --- intentional no-connects --------------------------------------------
    // Pads deliberately left open, declared so the completeness check (issue 0001)
    // stays clean rather than just quiet: this probe does not self-debug (user
    // decision #4), so the RP2350's own SWD pins are open; GP21-29 are unused on the
    // sequential map; USB sideband, the LED-chain output, and the regulator NC pin
    // are unused by design.
    b.s.push(G::NoConnect {
        pins: vec![
            p("U1", "SWCLK"),
            p("U1", "SWDIO"),
            p("U1", "GPIO21"),
            p("U1", "GPIO22"),
            p("U1", "GPIO23"),
            p("U1", "GPIO24"),
            p("U1", "GPIO25"),
            p("U1", "GPIO26/ADC0"),
            p("U1", "GPIO27/ADC1"),
            p("U1", "GPIO28/ADC2"),
            p("U1", "GPIO29/ADC3"),
            p("D1", "DOUT"),
            p("J11", "SBU1"),
            p("J11", "SBU2"),
            p("U3", "NC"),
        ],
    });

    // --- mechanical: 4x M2.5 NPTH mounting holes near the corners (Task 4) --
    // 2.7 mm clearance holes, inset 4 mm from each corner. Authored `hole` directives
    // (Decision 16b) → full-stackup non-plated `Void` → board-NPTH.drl. (Round-2
    // finding: these do NOT yet knock out the mask or the inner-layer planes, and DRC
    // does not flag copper intruding on them — see the findings ledger. The plane pours
    // above therefore fill over the holes; honest, and surfaced.)
    b.hole(4, 4, 27);
    b.hole(52, 4, 27);
    b.hole(4, 40, 27);
    b.hole(52, 40, 27);

    // --- silk title block (Task 4) ------------------------------------------
    // Board title + revision as authored F.SilkS text (Decision 9). Placed in the open
    // area below the QFN. Cap-height 2.0 / 1.5 mm.
    b.text("RP2350A MULTI-SWD PROBE", 16, 20, 20, "F.SilkS");
    b.text("REV B  4-LAYER", 20, 24, 15, "F.SilkS");

    b.finish()
}

// ---------------------------------------------------------------------------
// Schematic authoring (Round 3 / Task 4) — the doc-level layout tree
// ---------------------------------------------------------------------------
//
// The schematic is the *second derived view* of the same netlist truth (Decision 20): an
// authored `row`/`column`/`sym` flow tree, reflowed to coordinates and rendered to SVG. It
// is board-independent — it never touches placement or routing. There is no `SetSchematic`
// command (a finding — see the Round-3 ledger); the only ingest is a `schematic { … }`
// text block, so `main` serializes this tree with `serialize_schematic_block`, appends it
// to the source text, and lets the round-trip parse it back (which is exactly what proves
// the block is byte-lossless).

/// A placed `sym` leaf for an instance path (identity orientation, no pinned offset — the
/// common "just flow it" case).
fn sym(path: &str) -> LayoutNode {
    LayoutNode::Symbol(Symbol {
        path: path.into(),
        rot: ecad_core::doc::Orient::IDENTITY,
        dx: 0,
        dy: 0,
    })
}

/// A named container (`row`/`column`) with a main-axis gap (mm) and cross-axis alignment.
fn group(
    dir: Direction,
    name: &str,
    gap_mm: i64,
    align: Align,
    children: Vec<LayoutNode>,
) -> LayoutNode {
    LayoutNode::Container(Container {
        dir,
        name: Some(name.into()),
        gap: gap_mm * MM,
        align,
        children,
    })
}

/// The doc-level board schematic (Decision 20): a top row of functional sections, each a
/// named column — power, the MCU + its support, the SWD channel bank (the 10 repeated
/// headers, in two columns of five like the physical edges), and the USB/user I/O. A
/// couple of presentational wires with waypoints draw the crystal net for readability
/// (§20d — a wire is a *picture* of the netlist, carrying no connectivity of its own).
fn build_board_schematic() -> SchematicLayout {
    // The SWD header bank: J1–J5 (left edge) and J6–J10 (right edge), each a column so the
    // reused-connector structure reads as two stacks — the visual echo of the board edges.
    let jcol = |lo: usize, hi: usize, name: &str| -> LayoutNode {
        let syms = (lo..=hi).map(|i| sym(&format!("J{i}"))).collect();
        group(Direction::Column, name, 3, Align::Center, syms)
    };

    // Power section: the regulator, its input/output caps, the buck inductor, the LED.
    let power = group(
        Direction::Column,
        "power",
        4,
        Align::Center,
        vec![sym("U3"), sym("C_IN"), sym("C_OUT"), sym("L1"), sym("D1")],
    );

    // The MCU decoupling bank (C0–C13): one cap per power pin. A `column` of the fourteen
    // caps beside the QFN — the layout tree has no "place every member of net +3V3"
    // affordance, so each is named by hand (a Round-3 finding); a 2-wide grid keeps the
    // stack from running off the page.
    let decouple = {
        let mut rows = Vec::new();
        for pair in (0..14).step_by(2) {
            let mut r = vec![sym(&format!("C{pair}"))];
            if pair + 1 < 14 {
                r.push(sym(&format!("C{}", pair + 1)));
            }
            rows.push(group(
                Direction::Row,
                &format!("dec{pair}"),
                12,
                Align::Center,
                r,
            ));
        }
        group(Direction::Column, "decoupling", 2, Align::Center, rows)
    };

    // MCU section: the QFN in the middle, its close-in support (flash + crystal + the
    // crystal's load caps / bias resistor) stacked on one side and the decoupling bank on
    // the other. A wire draws the crystal terminals to XIN/XOUT with a waypoint bend.
    // The row gap here is deliberately wide (30 mm): the reflow packs sibling boxes by
    // their box extent ONLY — it does not reserve room for the pin-name labels that hang
    // off each box edge (a Round-3 finding). The RP2350A has long functional pin names on
    // both sides, so without a generous manual gap the decoupling bank overprints them.
    let mcu = group(
        Direction::Row,
        "mcu",
        30,
        Align::Center,
        vec![
            group(
                Direction::Column,
                "mcu-support",
                4,
                Align::Center,
                vec![sym("U2"), sym("Y1"), sym("C_X1"), sym("C_X2"), sym("R_X")],
            ),
            sym("U1"),
            decouple,
        ],
    );

    let channels = group(
        Direction::Row,
        "swd-channels",
        6,
        Align::Start,
        vec![jcol(1, 5, "ch-left"), jcol(6, 10, "ch-right")],
    );

    // User I/O + the series/pull resistors that ride the connector nets (USB CC/DM/DP
    // terminations, the ADC bias, the BOOTSEL series R). Grouped here rather than left in
    // the bin so the view is complete — again each resistor named by hand.
    let series_r = group(
        Direction::Column,
        "series-r",
        2,
        Align::Center,
        vec![
            sym("R_CC1"),
            sym("R_CC2"),
            sym("R_DM"),
            sym("R_DP"),
            sym("R_AVDD"),
            sym("R_BOOT"),
        ],
    );
    let user_io = group(
        Direction::Column,
        "user-io",
        4,
        Align::Center,
        vec![sym("J11"), sym("SW1"), sym("SW2"), series_r],
    );

    // Presentational crystal wires (§20d): Y1 pin1/pin3 (X1/X2) to the MCU XIN/XOUT. These
    // agree with the netlist (XIN / XOUT nets), so they draw silently — a wire that
    // disagreed would earn W_SCHEMATIC_WIRE. Drawn as straight segments: authored waypoints
    // are absolute schematic-space coordinates, but reflow decides where the symbols land,
    // so an author has no way to pick a sensible bend without reading back the reflowed
    // positions first (a Round-3 finding — waypoints want to be relative or pin-anchored).
    let wire = |ac: &str, ap: &str, bc: &str, bp: &str| -> LayoutNode {
        LayoutNode::Wire(Wire {
            a: WireEnd {
                comp: ac.into(),
                pin: ap.into(),
            },
            b: WireEnd {
                comp: bc.into(),
                pin: bp.into(),
            },
            waypoints: vec![],
        })
    };

    SchematicLayout {
        roots: vec![
            LayoutNode::Comment(
                "RP2350A multi-SWD probe — board schematic (Decision 20 view)".into(),
            ),
            group(
                Direction::Row,
                "board",
                12,
                Align::Start,
                vec![power, mcu, channels, user_io],
            ),
            LayoutNode::Blank,
            LayoutNode::Comment("crystal net, drawn for readability (agrees with XIN/XOUT)".into()),
            wire("Y1", "1", "U1", "XIN"),
            wire("Y1", "3", "U1", "XOUT"),
        ],
    }
}

// ---------------------------------------------------------------------------
// Def-embedded layout demonstration (Round 3 / Task 1) — a SEPARATE section
// ---------------------------------------------------------------------------
//
// The capstone board's 10 SWD channels repeat at the NETLIST level (two signal nets + GND
// per JST header), but each channel is a *single connector* with no internal multi-part
// sub-circuit — there is nothing structural to encapsulate in a `def`, and folding the ten
// flat `inst`s into defs would change the component count and cascade through every
// downstream stage assertion (a finding — F-def-fit in the ledger). So the def-embedded
// layout FEATURE (Decision 20 inside a def: a layout fragment stamped per instance, reused
// circuits rendering identically everywhere) is demonstrated here as its own small board:
// a per-channel RC input filter `def` with an embedded `schematic { … }` fragment,
// instantiated three times. This authors as text (the whole point — the fragment must
// round-trip), elaborates, and reflows to a schematic where all three channels render with
// byte-identical relative geometry.

const CHANNEL_DEF_DEMO: &str = "\
def rc_input {
  inst Rs R
  inst Cf C
  net node Rs.2 Cf.1
  port sig = Rs.1
  port flt = Cf.2
  schematic {
    row gap=5mm {
      sym Rs
      sym Cf
    }
  }
}
inst ch0 rc_input
inst ch1 rc_input
inst ch2 rc_input
net GND ch0.flt ch1.flt ch2.flt
schematic {
  column gap=6mm {
    sym ch0
    sym ch1
    sym ch2
  }
}
";

// ---------------------------------------------------------------------------

fn main() {
    // The `use poc` directive in the source names this library package; the caller
    // (us) resolves the name to its directory and loads it — libraries are data now.
    let lib = load_library(std::path::Path::new(PARTS))
        .unwrap_or_else(|e| panic!("load library package `poc` from {PARTS}: {e}"));
    let rp2350 = &lib["RP2350A"];
    println!("== Stage 1: RP2350A QFN-60 sourced + verified through framework ==");
    println!(
        "  RP2350A joined pins: {} (60 signal/power + 1 EP)",
        rp2350.pins.len()
    );

    let src = build_source();
    let n_inst = src
        .iter()
        .filter(|d| matches!(d, G::Instance { .. }))
        .count();
    let n_nets = src
        .iter()
        .filter(|d| matches!(d, G::ConnectPins { .. }))
        .count();
    println!("\n== Stage 3: design authored ==");
    println!("  components: {n_inst}   nets: {n_nets}");

    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "poc")
        .unwrap();
    let doc = h.doc();
    println!("  elaborated components: {}", doc.components.len());
    println!("  elaborated nets:       {}", doc.nets.len());
    let rep = &doc.report;
    if !rep.pin_conflicts.is_empty() || !rep.orphaned.is_empty() {
        println!(
            "  recon report: conflicts={:?} orphaned={:?}",
            rep.pin_conflicts, rep.orphaned
        );
    }

    // == Stage 3b: text round-trip is the pipeline (Decision 13/18) ==========
    // Serialize the whole authored doc to canonical text, LoadText it back into a fresh
    // History (re-parse + re-elaborate), and confirm the round-trip is lossless by
    // re-serializing and byte-comparing. THEN run place/route/DRC/export on the *parsed*
    // doc — this is the code+lockfile model applied to the capstone: the text file is
    // the authoritative artifact, and everything downstream consumes what parsed back.
    println!("\n== Stage 3b: text round-trip (serialize -> parse -> re-serialize) ==");
    // The doc-level schematic layout tree (Decision 20 / Task 4) is authored in Rust
    // (`build_board_schematic`) but ingests only as text — there is no `SetSource`-style
    // command for it (a Round-3 finding). So serialize the tree to its canonical
    // `schematic { … }` block and append it to the source text: it now rides the SAME
    // round-trip as everything else, which is exactly what proves the block is byte-lossless
    // with a schematic present (the Task-3 invariant).
    let schematic_block = serialize_schematic_block(&build_board_schematic());
    let text1 = format!("{}{}", serialize(doc), schematic_block);
    let mut h2 = History::new(Default::default());
    h2.commit(
        Transaction::one(Command::LoadText(text1.clone())),
        &lib,
        "load",
    )
    .unwrap();
    let text2 = serialize(h2.doc());
    if text1 == text2 {
        // FIXPOINT, not full fidelity: serialize(parse(serialize(doc))) == serialize(doc)
        // holds, so parse/serialize are stable inverses of each other. It does NOT prove
        // the doc->text projection is lossless — the board's corner radius is dropped at
        // serialization (F4), and since it is already gone from `text1`, both sides agree
        // and the byte-compare is blind to that loss. So: the text round-trip is a stable
        // fixpoint; a rounded outline is silently flattened to a sharp polygon upstream.
        println!(
            "  FIXPOINT: parse/serialize are stable inverses, byte-identical ({} bytes). \
             NOTE: the doc->text projection drops the board corner radius (F4), so this \
             check cannot see that loss.",
            text1.len()
        );
    } else {
        // Surface the divergence honestly rather than panicking — a lossy round-trip is a
        // finding, not a demo failure. Report the first differing line.
        let (mut ln, mut shown) = (0usize, 0usize);
        for (a, b) in text1.lines().zip(text2.lines()) {
            ln += 1;
            if a != b && shown < 3 {
                println!("  LOSSY at line {ln}:\n    was: {a}\n    now: {b}");
                shown += 1;
            }
        }
        println!(
            "  LOSSY: {} vs {} bytes, {} vs {} lines",
            text1.len(),
            text2.len(),
            text1.lines().count(),
            text2.lines().count()
        );
    }
    // Continue the pipeline on the *parsed* doc (h2), not the original — the text file is
    // truth. Re-check elaboration held.
    let mut h = h2;
    let doc = h.doc();
    println!(
        "  parsed doc: {} components, {} nets",
        doc.components.len(),
        doc.nets.len()
    );

    // == Stage 3c: def-embedded schematic layout (Task 1) ====================
    // A SEPARATE small board (see CHANNEL_DEF_DEMO's rationale): a per-channel RC-input
    // `def` carrying a Decision-20 `schematic { … }` fragment, instantiated three times.
    // The fragment is stamped per instance (paths prefixed `ch0.Rs`, `ch1.Rs`, …), so the
    // doc-level `sym ch0` expands to that group and all three channels reflow to identical
    // relative geometry — the "reused circuit renders identically everywhere" payoff.
    println!("\n== Stage 3c: def-embedded schematic layout (stamped per instance) ==");
    {
        let mut hd = History::new(Default::default());
        hd.commit(
            Transaction::one(Command::LoadText(CHANNEL_DEF_DEMO.to_string())),
            &lib,
            "def-demo",
        )
        .expect("def-with-layout demo commits cleanly");
        let dd = hd.doc();
        // Byte-lossless round-trip WITH the def's embedded fragment present.
        let rt = serialize(dd);
        let mut hd2 = History::new(Default::default());
        hd2.commit(Transaction::one(Command::LoadText(rt.clone())), &lib, "rt")
            .unwrap();
        let lossless = rt == serialize(hd2.doc());
        println!(
            "  3 channels stamped from one def; def_fragments keyed: {:?}",
            dd.def_fragments.keys().collect::<Vec<_>>()
        );
        println!(
            "  embedded-fragment round-trip: {}",
            if lossless { "byte-lossless" } else { "LOSSY" }
        );
        // Reflow and confirm identical relative internal geometry across the three stamps.
        let placed = dd.reflow_schematic(&lib);
        let offset = |ch: &str| {
            use ecad_core::id::EntityId;
            let r = placed[&EntityId::new(format!("{ch}.Rs"))].center;
            let c = placed[&EntityId::new(format!("{ch}.Cf"))].center;
            (c.x - r.x, c.y - r.y)
        };
        let (o0, o1, o2) = (offset("ch0"), offset("ch1"), offset("ch2"));
        println!(
            "  per-channel R->C offset (nm): ch0={o0:?} ch1={o1:?} ch2={o2:?} -> identical={}",
            o0 == o1 && o1 == o2
        );
        std::fs::create_dir_all("poc/out").unwrap();
        std::fs::write("poc/out/schematic-def-demo.svg", schematic_svg(dd, &lib)).unwrap();
        println!("  wrote poc/out/schematic-def-demo.svg");
    }

    // ERC
    let mut eng = Engine::new();
    let erc = eng.query(doc, &lib, Key::Erc);
    println!("  ERC violations: {}", erc.as_erc().len());
    print!("{}", render(erc.as_erc()));
    // Connectivity completeness (issue 0001): every pad that is on no net and not
    // marked no-connect. With pad-identity keying, ALL six IOVDD / three DVDD pads
    // are accounted for; what remains here is genuinely unconnected pads (unused
    // GPIOs etc.) that a finished design would route or NC — surfaced, not silent.
    let floats = eng.query(doc, &lib, Key::Floating);
    println!("  floating pads: {}", floats.as_floating().len());
    print!("{}", render(floats.as_floating()));

    // Stage 4: route + DRC
    let rules = DesignRules::default();
    println!("\n== Stage 4: place + autoroute + DRC ==");
    let before = eng.query(doc, &lib, Key::Drc).as_drc().to_vec();
    let unrouted_before = before
        .iter()
        .filter(|v| matches!(v, ecad_core::route::Violation::Unrouted { .. }))
        .count();
    println!(
        "  DRC before routing: {} violations ({unrouted_before} unrouted nets)",
        before.len()
    );

    let result = autoroute(doc, &lib, &rules);
    let traces = result
        .commands
        .iter()
        .filter(|c| matches!(c, Command::AddTrace(..)))
        .count();
    let vias = result
        .commands
        .iter()
        .filter(|c| matches!(c, Command::AddVia(..)))
        .count();
    println!(
        "  autoroute: {} routed nets, {} unrouted nets ({traces} traces, {vias} vias)",
        result.routed.len(),
        result.unrouted.len()
    );
    // Pre-verify capability vs. post-verify reality: the greedy search connects far more
    // nets (and drops stitching vias) than survive verify's clash pruning — the gap is the
    // cost of the fenced greedy-no-rip-up model (issue 0008), the signal for that
    // discussion. See AutorouteResult::stats.
    let s = &result.stats;
    println!(
        "  pre-verify search: {} routed, {} commands ({} vias) -> after verify+reconcile: {} routed, {} vias",
        s.pre_verify_routed,
        s.pre_verify_commands,
        s.pre_verify_vias,
        result.routed.len(),
        vias
    );
    println!("  routed:   {:?}", result.routed);
    println!("  unrouted: {:?}", result.unrouted);

    if !result.commands.is_empty() {
        h.commit(Transaction(result.commands), &lib, "autoroute")
            .unwrap();
    }

    // == Stage 4b: PromoteRoutes — freeze a couple of nets (Decision 18) =====
    // The autorouter's traces land as `free` (router-owned, rip-up-able). Promoting a
    // net flips its traces/vias to `pinned` (hand-blessed, immovable) — the "freeze"
    // move that makes partial reroute low-friction. We promote up to the first two
    // routed nets, then show the `# routes` zone carries MIXED provenance.
    let promote: Vec<NetId> = result.routed.iter().take(2).cloned().collect();
    if !promote.is_empty() {
        h.commit(
            Transaction::one(Command::PromoteRoutes {
                nets: promote.clone(),
            }),
            &lib,
            "promote",
        )
        .unwrap();
        println!("\n== Stage 4b: PromoteRoutes (freeze {:?}) ==", promote);
        let d = h.doc();
        use ecad_core::doc::Provenance;
        let pinned = d
            .traces
            .values()
            .filter(|t| t.prov == Provenance::Pinned)
            .count();
        let free = d
            .traces
            .values()
            .filter(|t| t.prov == Provenance::Free)
            .count();
        println!("  traces: {pinned} pinned (promoted), {free} free (router-owned)");
        // Prove the provenance survives serialization: the `# routes` zone shows both
        // `free`-tagged and bare (pinned-default) lines.
        let txt = serialize(d);
        let route_lines: Vec<&str> = txt
            .lines()
            .skip_while(|l| *l != "# routes")
            .filter(|l| l.starts_with("route ") || l.starts_with("via "))
            .collect();
        let n_free = route_lines.iter().filter(|l| l.ends_with(" free")).count();
        let n_pinned = route_lines.len() - n_free;
        println!(
            "  # routes zone: {} lines -> {} pinned (bare), {} free (tagged)",
            route_lines.len(),
            n_pinned,
            n_free
        );
    }

    let doc = h.doc();
    let after = eng.query(doc, &lib, Key::Drc).as_drc().to_vec();
    let (clr, mw, un): (Vec<_>, Vec<_>, Vec<_>) = {
        use ecad_core::route::Violation::*;
        let mut clr = vec![];
        let mut mw = vec![];
        let mut un = vec![];
        for v in &after {
            match v {
                Clearance { .. } => clr.push(v.clone()),
                MinWidth { .. } => mw.push(v.clone()),
                Unrouted { .. } => un.push(v.clone()),
                // Keep-out / edge-clearance violations (issue 0023) fall outside this
                // demo's three-way summary; count them under none of the buckets.
                Keepout { .. } | EdgeClearance { .. } => {}
            }
        }
        (clr, mw, un)
    };
    println!(
        "  DRC after routing: {} total -> {} clearance, {} min-width, {} unrouted",
        after.len(),
        clr.len(),
        mw.len(),
        un.len()
    );
    for v in &clr {
        println!("    {v:?}");
    }
    // Plane connectivity (Task 2 focus): the GND / +3V3 planes are real copper on the
    // inner layers, but the router is a 2-layer grid that does not drop stitching vias
    // from outer-layer pads down to an inner plane. So the plane sits as one big island
    // and the pads that should tie to it remain on their own islands — the net reports
    // as many islands (unrouted) despite the plane existing. Surface the island counts.
    // Whether each plane net is DRC-connected (1 island ⇒ absent from `un`) or still
    // fragmented. A pad geometrically over its own net's pour joins that island even with
    // no explicit stitching via (the pad-under-pour incidence), so a plane *does* provide
    // connectivity for overlapping pads — but pads outside the (knocked-out) fill, or a
    // fill fragmented by knockouts, leave islands. Honest per-plane status.
    for plane in ["GND", "+3V3"] {
        let islands = un.iter().find_map(|v| match v {
            ecad_core::route::Violation::Unrouted { net, islands } if net.to_string() == plane => {
                Some(*islands)
            }
            _ => None,
        });
        match islands {
            Some(n) => println!(
                "    plane {plane}: {n} islands (fragmented — some pads not over the fill / fill split by knockouts)"
            ),
            None => println!(
                "    plane {plane}: connected (1 island — pads-over-pour incidence tied them without stitching vias)"
            ),
        }
    }

    // Stage 5: export fab artifacts to poc/out/
    println!("\n== Stage 5: export fab artifacts -> poc/out/ ==");
    std::fs::create_dir_all("poc/out").unwrap();
    let mut wrote = vec![];
    let write = |name: &str, content: &str, wrote: &mut Vec<String>| {
        std::fs::write(format!("poc/out/{name}"), content).unwrap();
        wrote.push(name.to_string());
    };
    // The canonical text projection of the final routed doc (the authoritative artifact
    // — the "source file"). Includes the `# routes` zone with mixed provenance.
    write("board.ecad", &serialize(doc), &mut wrote);
    write("netlist.txt", &netlist(doc), &mut wrote);
    write("placement.csv", &placement_csv(doc), &mut wrote);
    write("board.svg", &svg(doc, &lib).unwrap(), &mut wrote);
    // The schematic view (Decision 20 / Task 4): the second derived projection of the same
    // netlist truth, rendered from the round-tripped doc's `schematic` tree. A tracked
    // artifact like the Gerbers. Every component is drawn (§20c totality) — any not named
    // by the layout tree lands in the derived unplaced bin below a labelled divider.
    write("schematic.svg", &schematic_svg(doc, &lib), &mut wrote);
    // gerber_set already includes the split drill file(s) (== excellon_drill(doc, lib));
    // call it explicitly too only to assert the two agree.
    let gset = gerber_set(doc, &lib).unwrap();
    for (name, content) in excellon_drill(doc, &lib) {
        assert_eq!(
            gset.iter()
                .find(|(n, _)| *n == name)
                .map(|(_, c)| c.as_str()),
            Some(content.as_str())
        );
    }
    for (name, content) in gset {
        write(&name, &content, &mut wrote);
    }
    // Fab-drawing SVGs (Decision 15) — empty unless the stackup authors a fab slab.
    for (name, content) in fab_svg_set(doc, &lib).unwrap() {
        write(&name, &content, &mut wrote);
    }
    println!("  wrote: {}", wrote.join(", "));
}
