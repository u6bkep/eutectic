//! PoC: chip-down RP2350A (QFN-60) multi-SWD debug probe board, authored entirely
//! through the ecad-core framework (parts -> netlist -> placement -> autoroute ->
//! DRC -> fab export). Run with `cargo run --example poc_multiprobe`.
//!
//! Design = a bare RP2350A acting as 10 independent SWD probes, each on a 3-pin
//! JST-SH header (pin1=SWCLK, pin2=GND, pin3=SWDIO), USB-powered, UF2/BOOTSEL.
//! Faithful to the original Waveshare-module-based probe but with the support
//! circuitry (3V3 reg, core buck L+C, 12 MHz crystal, QSPI flash, USB front-end,
//! buttons, status LED) made explicit on-board.
//!
//! USER DECISIONS honoured here:
//!  1. RP2350A / QFN-60 (GPIO0-29), sourced from KiCad's official library.
//!  2. Clean SEQUENTIAL GPIO map: chN -> GP(2N-2)/GP(2N-1); J1=GP0/1 ... J10=GP18/19.
//!  3. 4-layer stack-up *intent* (signal/GND/PWR/signal). NOTE: the autorouter is a
//!     2-layer grid router, so inner planes are documented intent, not routed copper.
//!  4. No probe-self-debug header; USB UF2 + BOOTSEL (+ RUN reset) only.

use ecad_core::autoroute::autoroute;
use ecad_core::command::{Command, Transaction};
use ecad_core::diagnostic::render;
use ecad_core::doc::{MM, Point};
use ecad_core::elaborate::{GenDirective as G, Source, board_rect};
use ecad_core::export::{excellon_drill, gerber_set, netlist, placement_csv, svg};
use ecad_core::history::History;
use ecad_core::kicad::{
    apply_role_map, import_footprint_file, import_symbol_named, join_symbol_footprint,
};
use ecad_core::part::{PartDef, PartLib, PinRole};
use ecad_core::query::{Engine, Key};
use ecad_core::route::DesignRules;
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

    // 4-layer-intent board outline (signal / GND / PWR / signal). 56 x 44 mm.
    b.s.push(board_rect(Point::mm(0, 0), Point::mm(56, 44)));

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
    b.inst("R_AVDD", "R");
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
    b.inst("R_X", "R");
    b.place("R_X", 16, 27);
    b.inst("C_X1", "C");
    b.place("C_X1", 15, 33);
    b.inst("C_X2", "C");
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
    b.inst("R_DP", "R");
    b.place("R_DP", 25, 8);
    b.inst("R_DM", "R");
    b.place("R_DM", 31, 8);
    b.inst("R_CC1", "R");
    b.place("R_CC1", 22, 5);
    b.inst("R_CC2", "R");
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
    b.inst("R_BOOT", "R");
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
        b.inst(&c, "C");
        b.near_pin(&c, "U1", pad, 3); // pull each decoupler within 3 mm of its pad
        b.net(rail, &[p(&c, "1")]);
        b.net("GND", &[p(&c, "2")]);
    }
    // Regulator in/out bulk caps.
    b.inst("C_IN", "C");
    b.place("C_IN", 10, 18);
    b.inst("C_OUT", "C");
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

    // Stage 5: export fab artifacts to poc/out/
    println!("\n== Stage 5: export fab artifacts -> poc/out/ ==");
    std::fs::create_dir_all("poc/out").unwrap();
    let mut wrote = vec![];
    let write = |name: &str, content: &str, wrote: &mut Vec<String>| {
        std::fs::write(format!("poc/out/{name}"), content).unwrap();
        wrote.push(name.to_string());
    };
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
    println!("  wrote: {}", wrote.join(", "));
}
