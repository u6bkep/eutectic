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
use ecad_core::kicad::{
    apply_role_map, import_footprint_file, import_symbol_named, join_symbol_footprint,
};
use ecad_core::part::{PartDef, PartLib, PinRole};
use ecad_core::query::{Engine, Key};
use ecad_core::route::DesignRules;
use ecad_core::text::serialize;
use std::collections::BTreeMap;

const PARTS: &str = "poc/parts";

// ---------------------------------------------------------------------------
// Part building helpers
// ---------------------------------------------------------------------------

/// Import a footprint's geometry as a (role-less, Passive) PartDef.
fn fp(file: &str) -> PartDef {
    import_footprint_file(&format!("{PARTS}/{file}"))
        .unwrap_or_else(|e| panic!("import {file}: {e}"))
}

/// Re-label imported footprint pads with functional names + electrical roles,
/// keyed by pad *number* — a jellybean part with no symbol gets its roles from a
/// hand-map. This is just the library's [`apply_role_map`] (issue 0002's
/// lightweight role overlay); the example wraps it to panic on a typo'd map.
/// Assigning the *same* name to several pads is fine and intended — connecting that
/// name fans out to all of them (see the duplicate power pads on the RP2350 below).
fn relabel(part: PartDef, map: &[(&str, &str, PinRole)]) -> PartDef {
    apply_role_map(part, map).expect("role map references a missing pad")
}

fn build_lib() -> (PartLib, PartDef) {
    let mut lib = PartLib::new();

    // U1: RP2350A QFN-60 — authoritative symbol + footprint, joined through the
    // framework. The six IOVDD and three DVDD pads keep their shared functional
    // names: connecting "IOVDD" now fans out to all six pads (no uniquify hack).
    let sym = import_symbol_named(
        &std::fs::read_to_string(format!("{PARTS}/MCU_RaspberryPi.kicad_sym")).unwrap(),
        "RP2350A",
    )
    .expect("RP2350A symbol");
    let mcu_fp = fp("RP2350A_QFN-60.kicad_mod");
    let jr = join_symbol_footprint(&sym, &mcu_fp);
    assert!(
        jr.symbol_only.is_empty() && jr.footprint_only.is_empty(),
        "RP2350A join not clean: {:?} / {:?}",
        jr.symbol_only,
        jr.footprint_only
    );
    let rp2350 = jr.part;
    lib.insert("RP2350A".into(), rp2350.clone());

    // J1..J10: JST-SH 3-pin (pads 1,2,3,MP) — passive connector, no relabel.
    lib.insert("JST_SH".into(), fp("JST_SH_3pin_Horizontal.kicad_mod"));

    // U2: QSPI flash W25Q (SOIC-8). Pinout: 1=/CS 2=IO1(DO) 3=IO2(/WP) 4=GND
    // 5=IO0(DI) 6=CLK 7=IO3(/HOLD) 8=VCC.
    use PinRole::*;
    lib.insert(
        "W25Q".into(),
        relabel(
            fp("Flash_SOIC-8.kicad_mod"),
            &[
                ("1", "CS_N", Input),
                ("2", "IO1", Bidir),
                ("3", "IO2", Bidir),
                ("4", "GND", Passive),
                ("5", "IO0", Bidir),
                ("6", "CLK", Input),
                ("7", "IO3", Bidir),
                ("8", "VCC", PowerIn),
            ],
        ),
    );

    // Y1: 12 MHz crystal, 3225 4-pad. 1/3 = terminals, 2/4 = case GND.
    lib.insert(
        "XTAL".into(),
        relabel(
            fp("Crystal_3225.kicad_mod"),
            &[
                ("1", "X1", Passive),
                ("2", "GNDa", Passive),
                ("3", "X2", Passive),
                ("4", "GNDb", Passive),
            ],
        ),
    );

    // U3: 3.3 V LDO/reg (AP2112K-3.3, SOT-23-5): 1=VIN 2=GND 3=EN 4=NC 5=VOUT.
    lib.insert(
        "REG".into(),
        relabel(
            fp("Regulator_SOT-23-5.kicad_mod"),
            &[
                ("1", "VIN", PowerIn),
                ("2", "GND", Passive),
                ("3", "EN", Input),
                ("4", "NC", Passive),
                ("5", "VOUT", PowerOut),
            ],
        ),
    );

    // J11: USB-C receptacle (USB 2.0). The four VBUS pads, four GND pads, and the
    // two DP / two DM pads share a name each: connecting "VBUS"/"GND"/"DP"/"DM" fans
    // out to every physical pad (no per-pad distinct-name workaround needed).
    lib.insert(
        "USBC".into(),
        relabel(
            fp("USB_C_Receptacle.kicad_mod"),
            &[
                ("A1", "GND", Passive),
                ("A4", "VBUS", PowerIn),
                ("A5", "CC1", Passive),
                ("A6", "DP", Bidir),
                ("A7", "DM", Bidir),
                ("A8", "SBU1", Passive),
                ("A9", "VBUS", PowerIn),
                ("A12", "GND", Passive),
                ("B1", "GND", Passive),
                ("B4", "VBUS", PowerIn),
                ("B5", "CC2", Passive),
                ("B6", "DP", Bidir),
                ("B7", "DM", Bidir),
                ("B8", "SBU2", Passive),
                ("B9", "VBUS", PowerIn),
                ("B12", "GND", Passive),
                ("SH", "SHIELD", Passive),
            ],
        ),
    );

    // L1: core-buck inductor 3.3 uH (2020). R/C/inductor passives keep pads "1","2".
    lib.insert("IND".into(), fp("Inductor_2020.kicad_mod"));
    lib.insert("R".into(), fp("R_0402.kicad_mod"));
    lib.insert("C".into(), fp("C_0402.kicad_mod"));
    // SW1 BOOTSEL, SW2 RUN — 2-terminal tactile.
    lib.insert("BTN".into(), fp("Button_EVQP7A.kicad_mod"));
    // D1: WS2812B-2020 status LED. 1=VDD 2=DOUT 3=GND 4=DIN.
    lib.insert(
        "LED".into(),
        relabel(
            fp("LED_WS2812B.kicad_mod"),
            &[
                ("1", "VDD", PowerIn),
                ("2", "DOUT", Output),
                ("3", "GND", Passive),
                ("4", "DIN", Input),
            ],
        ),
    );

    (lib, rp2350)
}

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

fn main() {
    let (lib, rp2350) = build_lib();
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
    let text1 = serialize(doc);
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
