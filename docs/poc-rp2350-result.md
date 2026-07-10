# Chip-Down Multi-SWD Probe — RP2350A QFN-60 — Implementation Result

End-to-end build of the chip-down RP2350 multi-SWD debug probe board, authored
entirely through the `eutectic-core` framework. This is both the PoC deliverable and
the framework's end-to-end stress test. The whole flow is one runnable example:

    cargo run --example poc_multiprobe

It sources parts, authors the netlist+placement, elaborates, ERC-checks,
autoroutes, DRCs, and writes the fab fileset to `poc/out/`.

> **Scope vs spec.** The research spec (`poc-rp2350-spec.md`) targeted the
> RP2350**B** QFN-80. The user amended this to the faithful RP2350**A** QFN-60
> (GPIO0–29), a clean sequential GPIO→header map, 4-layer intent, and no
> probe-self-debug header. This document reflects those amendments.

---

## Stage 1 — RP2350A QFN-60 sourcing (the hard gate): PASSED

The faithful A/QFN-60 had no local symbol/footprint, so it was sourced from
**KiCad's official libraries** (the most authoritative freely-available KiCad
pinout) and vendored under `poc/parts/`:

- **Symbol:** `poc/parts/MCU_RaspberryPi.kicad_sym`, symbol `RP2350A`
  (KiCad official `MCU_RaspberryPi.kicad_sym`, `version 20251024`). Its own
  `Footprint` property names `Package_DFN_QFN:QFN-60-1EP_7x7mm_P0.4mm_EP3.4x3.4mm`
  and its `Datasheet` property is the official RP2350 datasheet URL.
- **Footprint:** `poc/parts/RP2350A_QFN-60.kicad_mod` (KiCad official
  `QFN-60-1EP_7x7mm_P0.4mm_EP3.4x3.4mm_ThermalVias`).

**Verified through THIS framework** (`import_symbol_named` + `import_footprint_file`
+ `join_symbol_footprint`; also a guarded unit test
`kicad::tests::rp2350a_qfn60_join_if_present`):

- Clean **61/61 join** — 60 signal/power pads + the exposed pad, `symbol_only`
  and `footprint_only` both empty.
- Pin names are **real RP2350 functions** with real electrical roles, e.g.
  `GPIO0…GPIO25` (Bidir), `GPIO26/ADC0…GPIO29/ADC3`, `IOVDD`/`DVDD` (PowerIn),
  `VREG_LX` (PowerOut), `VREG_VIN`/`VREG_AVDD`/`VREG_PGND`/`VREG_FB`, `XIN`/`XOUT`,
  `SWCLK`/`SWDIO`, `RUN`, `USB_DP`/`USB_DM`, `USB_OTP_VDD`, `QSPI_SCLK`/`~{QSPI_SS}`/
  `QSPI_SD0..3`/`QSPI_IOVDD`, `ADC_AVDD`, `GND` (EP, pad 61).
- **Power-pin census matches the QFN-60 datasheet:** IOVDD ×6 (pads 1,11,20,30,38,45),
  DVDD ×3 (pads 6,23,39). These pads share a functional name — see Friction #1.

No pin numbers were hand-fabricated. The gate condition ("authoritatively obtain
AND verify the pinout") is met.

---

## Stage 2 — Part set

All parts are vendored under `poc/parts/` and built in `build_lib()`. Where a
symbol+footprint pair existed (only the RP2350A), the framework's join was used;
the jellybean parts have **no symbol in any local library**, so their footprint
geometry was imported and pads were hand-relabelled with functional names + roles
(`relabel()` keyed by pad number — see Friction #2).

| Ref(s) | Part | Footprint (vendored) | How built |
|---|---|---|---|
| U1 | RP2350A MCU | `RP2350A_QFN-60.kicad_mod` (QFN-60, EP+thermal vias) | symbol+footprint join, power names uniquified |
| U2 | W25Q QSPI flash | `Flash_SOIC-8.kicad_mod` (SOIC-8) | footprint + relabel (CS_N/CLK/IO0..3/VCC/GND) |
| U3 | 3.3 V reg (AP2112K-3.3) | `Regulator_SOT-23-5.kicad_mod` | footprint + relabel (VIN/GND/EN/NC/VOUT) |
| Y1 | 12 MHz crystal | `Crystal_3225.kicad_mod` (3225 4-pad) | footprint + relabel (X1/X2/GNDa/GNDb) |
| L1 | core-buck inductor 3.3 µH | `Inductor_2020.kicad_mod` | footprint (pads 1/2) |
| J1–J10 | SWD JST-SH 3-pin | `JST_SH_3pin_Horizontal.kicad_mod` | footprint (pads 1/2/3/MP) |
| J11 | USB-C receptacle (USB2.0) | `USB_C_Receptacle.kicad_mod` (HRO TYPE-C-31-M-12) | footprint + relabel (VBUS/GND/CC/DP/DM ×… ) |
| D1 | WS2812B status LED | `LED_WS2812B.kicad_mod` (PLCC4 2020) | footprint + relabel (VDD/DOUT/GND/DIN) |
| SW1, SW2 | BOOTSEL, RUN buttons | `Button_EVQP7A.kicad_mod` | footprint (pads 1/2) |
| R×7, C×18 | passives | `R_0402.kicad_mod`, `C_0402.kicad_mod` | footprint (pads 1/2, Passive) |

> The spec's first-choice local USB-C footprint (`USB_C_Receptacle_GCT_USB4125`)
> turned out to be a **power-only** breakout (pads VBUS/CC/GND/shield only — **no
> D+/D−**), unusable for a USB-data device. Swapped to KiCad-std
> `USB_C_Receptacle_HRO_TYPE-C-31-M-12`, which exposes A6/B6 (D+) and A7/B7 (D−).

**BOM totals: 44 components** — U1–U3, Y1, L1 (5); J1–J11 (11); D1 (1);
SW1/SW2 (2); 18 caps; 7 resistors.

---

## Stage 3 — Design authored (netlist)

`build_source()` emits the whole board as a generative `Source` (instances +
`Fix`/`Place`/`NearPin` placement + `ConnectPins` nets). Elaboration: **44
components, 44 nets, ERC clean (0 violations).**

### GPIO → header map (clean sequential, user decision #2)

`chN: SWCLK = GP(2N−2), SWDIO = GP(2N−1)`; each JST-SH is pin1=SWCLK, pin2=GND,
pin3=SWDIO. J1–J5 on the left board edge, J6–J10 on the right (cables exit
outward, per spec ergonomics).

| Ch | Hdr | SWCLK | SWDIO | | Ch | Hdr | SWCLK | SWDIO |
|:--:|:--:|:--:|:--:|---|:--:|:--:|:--:|:--:|
| A | J1 | GP0 | GP1 | | F | J6 | GP10 | GP11 |
| B | J2 | GP2 | GP3 | | G | J7 | GP12 | GP13 |
| C | J3 | GP4 | GP5 | | H | J8 | GP14 | GP15 |
| D | J4 | GP6 | GP7 | | I | J9 | GP16 | GP17 |
| E | J5 | GP8 | GP9 | | J | J10 | GP18 | GP19 |

This uses GP0–GP19. The original module's status-LED convention was GP16, but
GP16 is now a SWD channel, so the **WS2812 DIN moves to GP20** (`LED_DIN`). GP21–29
remain free.

### Net summary (44 nets)

- **Power:** `VBUS` (5 V from USB-C 4 pads → reg VIN, EN tied to VIN), `+3V3`
  (reg VOUT → IOVDD ×6, QSPI_IOVDD, USB_OTP_VDD, ADC_AVDD, VREG_VIN, flash VCC,
  LED VDD, all 3.3 V decoupling), `+DVDD` (1.1 V core: L1 → DVDD ×3 + VREG_FB +
  core decoupling), `VREG_LX` (switch node U1→L1), `VREG_AVDD` (33 Ω from +3V3 +
  4.7 µF), `GND` (54 pins incl. EP, VREG_PGND, USB GND/shield, all cap returns,
  10 header GNDs+mounts).
- **Core buck:** VREG_LX → L1 3.3 µH → +DVDD; VREG_FB senses +DVDD; PGND→GND.
- **Crystal:** XIN–Y1–(1 kΩ series)–XOUT, 2× load caps to GND.
- **QSPI bus:** SCLK/CS_N/SD0–3 direct U1↔U2.
- **USB:** USB_DP/USB_DM → 27 Ω → connector D±; CC1/CC2 5.1 kΩ to GND.
- **BOOTSEL:** 1 kΩ from `QSPI_CS_N` → SW1 → GND. **RUN:** SW2 → GND.
- **10 SWD channels** as above.

Decoupling = one 100 nF per power pin, each pulled within 3 mm of its pin via
`NearPin` to the exact MCU pad; plus reg in/out bulk caps and crystal load caps.

---

## Stage 4 — Place / autoroute / DRC (honest numbers)

- **Placement:** big parts `Fix`/`Place`d (QFN centre datum, USB-C bottom edge,
  headers on side edges); 14 decouplers `NearPin`'d onto their power pad. The
  least-change solver converged. (Decouplers cluster tightly around the 7×7 mm QFN
  and some overlap — no courtyard/keepout constraint exists to spread them; see
  Friction #5.)
- **Autoroute (default rules: 0.15 mm width/clearance, 0.45 mm grid pitch):**
  **19 of 44 nets routed** (101 traces, 26 vias); **25 unrouted.**
  - Routed: `+3V3`, `+DVDD`, `VBUS`, several SWD channels (A,B,F-partial,I), USB
    `CC1`/`CC2`/`DP_CONN`/`DM_CONN`, `QSPI_CS_N`/`QSPI_SD1`, `BOOT_SW`, `RUN`,
    `XTAL2`.
  - Unrouted: `GND` (54-pin net — should be a plane, see Friction #3), most QSPI
    (`SCLK`/`SD0`/`SD2`/`SD3`), `USB_DP`/`USB_DM` (the 27 Ω-to-QFN stubs),
    `XIN`/`XOUT`, `VREG_LX`/`VREG_AVDD`, `LED_DIN`, and SWD channels C,D,E,F,G,H,J.
- **DRC after routing: 30 violations** = **25 unrouted** + **5 clearance**, 0
  min-width. The 5 clearance breaches (the router's "clearance clean by
  construction" claim **failing** in practice):
  ```
  Clearance { a: +3V3,    b: GND,     layer: Top }
  Clearance { a: +DVDD,   b: J_SWCLK, layer: Bottom }
  Clearance { a: B_SWCLK, b: B_SWDIO, layer: Top }
  Clearance { a: CC1,     b: VBUS,    layer: Top }
  Clearance { a: DM_CONN, b: DP_CONN, layer: Top }
  ```

**This is the expected, truthful outcome** for a basic greedy grid router on a
0.4 mm-pitch QFN-60 fanout + switching reg + USB. It is **not DRC-clean and is not
presented as such.** The *value* is showing exactly where the framework falls
short on a real board (next section).

---

## Stage 5 — Exported artifacts (`poc/out/`)

All written by the example, all pure/deterministic functions of the routed `Doc`:

- `netlist.txt` — 44 nets, connectivity check artifact.
- `placement.csv` — pick-and-place (ref, part, x/y mm, rotation).
- `board.svg` — board sketch: outline, components+pads, routed copper.
- `board-F_Cu.gbr`, `board-B_Cu.gbr` — RS-274X copper (traces draw, pads/vias flash).
- `board-Edge_Cuts.gbr` — 56×44 mm outline.
- `board.drl` — Excellon drill (via holes).

Gerbers are syntactically valid RS-274X (`%FSLAX46Y46*%`, `%MOMM*%`, aperture
table, D01/D02/D03, `M02*`) but, per the framework's own note, are **not yet
validated against a real Gerber viewer**, and 4-layer intent is not represented
(only F_Cu/B_Cu exist — see Friction #3).

---

## Framework friction (the key feedback deliverable)

Concrete things that were awkward, missing, or error-prone driving the agent-first
API to build a real board. Ordered by how much they bit.

> **RESOLVED** (findings 1 & 2, branch `feat/pin-identity`, issues 0001/0002): net
> membership now keys on **pad identity** (pad number); a functional name is a
> *selector* that fans out to every matching pad at connection time. A
> `Key::Floating` query reports any pad on no net and not no-connect; an unknown pin
> in a connection is a hard elaboration error; `kicad::apply_role_map` overlays
> roles onto a bare footprint. The PoC below now nets all 6 IOVDD / 3 DVDD pads with
> single name connects and runs 0 floating pads — `uniquify()` is gone. The findings
> are preserved here as the record of what the build surfaced.

**1. Pins are keyed by name, and real MCUs have duplicate power-pin names.**
The RP2350A has 6 pads named `IOVDD` and 3 named `DVDD`. A net references a pin by
`(component, name)`, and `pin_offset`/`pin_world` resolve **by name, first match
wins**. Net members are a `BTreeSet<PinRef>`, so you literally *cannot* add
`("U1","IOVDD")` six times — five power pads would silently float. I had to
post-process the joined `PartDef` to rename duplicates to `IOVDD_1`, `IOVDD_11`, …
(`uniquify()`). This is a footgun: without noticing it, a board would tape out with
5 of 6 IOVDD pads unconnected and **ERC/DRC would not catch it** (DRC ratsnest only
checks pins that are net members). The model needs net membership keyed by **pad
number / stable pin id**, not functional name — or at least the join should refuse
to produce a part with duplicate pin names.

**2. No way to attach roles/names to a bare footprint short of authoring a full
symbol.** Every jellybean part (flash, reg, crystal, USB-C, LED, passives) had a
footprint but no symbol anywhere local. `import_footprint` gives every pad
`PinRole::Passive` and `name == number`. To net "the flash CLK pin" or assign
PowerIn for ERC, I wrote a `relabel(part, &[(num,name,role)])` helper by hand for
each part. A first-class "footprint + a small inline pin-map → roled PartDef" API
(or a `PartDef` builder) would remove a lot of boilerplate that every real board
needs. Relatedly, `ConnectPins` does **not validate** that a referenced pin name
exists on the part — a typo (`"VOUT"` vs `"Vout"`) silently creates a dangling pin
that's dropped at route time. A pin-existence check at elaboration would catch a
whole class of authoring bugs.

**3. No copper pours / planes / multilayer routing — fatal for a real board.**
The biggest single gap. `GND` here is a 54-pin net; on any real 4-layer board it's
a plane on an inner layer. The framework has no pour/zone concept, so GND must be
routed as discrete traces, which the autorouter (correctly) fails to do. The
`Layer` enum has `Inner(u8)`, but the autorouter is a **2-layer grid** and ignores
inner layers, and Gerber export only emits F_Cu/B_Cu — so "4-layer intent" is
purely documentary. Power integrity (the QFN EP thermal/return path, the VREG_LX
switching loop) cannot be expressed at all.

> **RESOLVED** (the *silent* part, branch `feat/geometry`, issues 0006 + 0003):
> DRC is now pad-aware (copper has real extent, stage 3), and the autorouter
> verifies its own output against that clearance and drops nets it can't route
> cleanly. The clearance breach is gone — the router now honestly reports far fewer
> routed nets (4, not a lying 19) and introduces **0** clearance violations; the
> ~16 that remain are pre-routing pad-pad placement clashes (finding 5 / issue
> 0005). What's *not* fixed is the routing *capability* at fine pitch (escape
> routing / finer grid / rip-up — issue 0008): the router still can't fan out a
> 0.4 mm QFN, it just no longer lies about it.

**4. The grid autorouter cannot fan out a fine-pitch QFN, and its clearance
guarantee silently breaks there.** Grid pitch = `via_pad + clearance` = 0.45 mm,
but the QFN-60 pad pitch is 0.40 mm and USB-C/0402 pads are closer still. Different
nets' pads collapse onto the same/adjacent grid nodes, so (a) most MCU-touching
nets are unroutable and rolled back, and (b) the "distinct nets never share a node
⇒ clearance falls out of pitch" invariant **does not hold for off-grid pads** — the
router emitted **5 real clearance violations** between adjacent fine-pitch pads it
believed were safe. A fine-pitch fanout needs escape-routing / non-grid pad access,
which this router doesn't have. (Honest and expected per the brief — but the
silent clearance breach, vs. an honest "can't route", is the surprising part.)

> **RESOLVED** (branch `feat/keepout`, issue 0005): parts now have a courtyard
> (origin-centred bbox of their pad copper + margin) and elaboration emits a
> `NoOverlap` solver constraint for every component pair, so overlapping courtyards
> are pushed apart (AABB min-translation; fixed-fixed overlaps reported). On this PoC
> it cuts the now-visible pad-pad clashes from 16 → 1 (the residual +3V3/GND pair is
> the *approximate* solver, 0007). Orientation is still not optimised by the solver.

**5. Placement has no courtyard / overlap-avoidance primitive.** `NearPin` pulls a
decoupler onto its pad, but nothing stops 14 caps stacking on top of each other (or
on the QFN) — `MinSep` is pairwise and component-centroid only, so spreading N caps
around a part means N² explicit constraints. A "keepout"/courtyard-aware non-overlap
(even just "these components must not overlap their footprint bboxes") is needed
before placement output is manufacturable. The solver also doesn't optimise
orientation, so caps can't be auto-rotated to fit an escape pattern.

**6. Netlist authoring ergonomics: no buses/typed interfaces from imported parts.**
The typed-interface story (the headline "serial-swap-unrepresentable" feature) only
works for the hand-authored toy library; `join_symbol_footprint` produces discrete
roled pins, never `InterfaceDef`s. So a 6-wire QSPI bus or a USB diff pair is
authored pin-by-pin with no type safety — exactly the error-prone hand-wiring the
architecture set out to eliminate. Authoring ~44 nets is a lot of `ConnectPins`
calls; I built a small `Builder` (merge-by-net-name, `p(c,pin)` helper) to make it
bearable, which suggests the raw API wants a higher-level net-builder.

**Smaller notes.** `import_part` only joins the *first* symbol in a multi-symbol
library; for `MCU_RaspberryPi.kicad_sym` (which has both RP2350A and RP2350B) I had
to use `import_symbol_named` + `join_symbol_footprint` manually (the strict
`import_part` convenience can't reach a named symbol). `gerber_set` already includes
`board.drl`, so calling `excellon_drill` too is redundant (harmless). Pad geometry
is render-only — DRC treats every pad as a zero-radius point, so pad-to-pad
clearance on the actual copper is never checked.

---

## Bottom line

- **Stage-1 gate passed:** authoritative RP2350A QFN-60 pinout obtained from KiCad
  official libs and verified through the framework (clean 61/61 join, real
  names+roles), with a regression test.
- A **complete, ERC-clean 44-component / 44-net** chip-down probe design was
  authored, placed, and partially routed entirely through the framework's
  command/generative API, and a full fab fileset was exported.
- **Routing is intentionally not clean:** 19/44 nets routed, 5 clearance
  violations, GND/QSPI/USB/crystal unrouted — an accurate picture of a basic grid
  router meeting a 0.4 mm QFN with no plane support. The friction list above is the
  primary finding: the load-bearing gaps for real boards are **name-keyed pins with
  duplicate power names, no plane/pour/multilayer, no fine-pitch escape routing, and
  no courtyard-based overlap avoidance.**

## Round 3 — schematic authoring (Decision 20 view + def-embedded layout)

Round 3 exercised the **schematic front** (Decision 20): the second derived view of the
same netlist truth — an authored `row`/`column`/`sym` flow tree, reflowed to coordinates,
rendered to SVG. Two features landed this round and were driven end-to-end by authoring the
capstone: **def-embedded layout stamping** (a `def` may carry a `schematic { … }` fragment
that is stamped per instance — reused circuits render identically everywhere) and
**refdes headers** (the schematic header shows the annotated designator `C3 (C)`, not the
raw instance path). The invariant held: the doc-level `schematic { … }` block round-trips
byte-lossless through `serialize → LoadText → serialize`, and the pipeline continues on the
parsed doc.

**Artifacts:** `poc/out/schematic.svg` (the 44-component board schematic — all placed, no
bin overflow) and `poc/out/schematic-def-demo.svg` (three RC channels stamped from one def,
byte-identical relative geometry).

### Capstone structure — defs used, and why the board itself is flat

The board's 10 SWD channels genuinely repeat *at the netlist level* (two signal nets + GND
per JST header), but each channel is a **single connector** — there is no internal
multi-part sub-circuit to encapsulate, and each channel already differs (distinct GPIO map,
distinct net names). Folding the ten flat `inst`s into a `def` would (a) buy nothing
structurally — a one-connector def has a trivial fragment — and (b) change the component
count and cascade through every downstream stage's assertions. **So the board schematic is
authored flat** (a doc-level layout tree organizing power / MCU+support+decoupling /
channel bank / user-I/O), and the def-embedded-layout FEATURE is demonstrated in a
**separate small section** (`CHANNEL_DEF_DEMO` in the example): a per-channel RC-input
filter `def` with an embedded `schematic { … }` fragment, instantiated three times. This is
called out plainly rather than contrived into the board. **See F-def-fit below.**

### Findings ledger (F1–F10, plus the named F-def-fit aside)

Recorded while authoring. **F7 is now surfaced** (a `W_SCHEMATIC` — see below); the rest are
open, except the two trivial one-liners noted. Repro is against
`cargo run --example poc_multiprobe` / the schematic SVGs.

- **F1 — the schematic layout tree has no programmatic ingest (no `SetSchematic`).** A
  `SchematicLayout` can only enter a `Doc` by parsing a `schematic { … }` **text** block via
  `LoadText`; there is no `Command` to set it (the symmetric partner to `SetSource`). The
  capstone works around this by serializing the tree (`serialize_schematic_block`, a new
  `pub` one-liner added this round) and appending it to the source text. *Direction:* add a
  `Command::SetSchematic(SchematicLayout)` so a GUI / programmatic author isn't forced
  through text. (The text round-trip is the *check*, not the only authoring path it should be.)

- **F2 — reflow packs boxes by extent only; it ignores pin-label width.** Sibling symbols
  are spaced by their box `w`/`h`, but each box's pin **names** hang off the edges (and net
  tags past the stubs). On the RP2350A (long functional pin names both sides) the decoupling
  bank overprinted the QFN's pin labels until the `mcu` row gap was hand-cranked to 30 mm.
  *Repro:* set the `mcu` gap back to 6 mm; the C1–C14 headers land on top of the QFN's
  right-edge names. *Direction:* fold a per-side label-extent estimate into `symbol_extent`
  (or reserve a label gutter), so a default gap produces non-overlapping siblings.

- **F3 — net tags at facing pins overprint.** Where two symbols sit close with stubs
  pointing at each other (R.pin2 ↔ C.pin1 in the def-demo; the JST channel tags), both pins'
  net-name tags render at nearly the same point and collide (`ch0.node`/`node` overlap in
  `schematic-def-demo.svg`; the JST `SWCLK`/`GND`/`SWDIO` tags overlap in `schematic.svg`).
  *Direction:* the renderer should offset a tag along the stub by text length, or suppress
  the tag on one side of a drawn wire.

- **F4 — wire waypoints are absolute schematic-space coordinates the author can't predict.**
  A `wire … via (x,y)` bend is an absolute nm coordinate, but **reflow** decides where the
  symbols land, so an author has no way to choose a sensible waypoint without first running
  reflow and reading back positions. The capstone's first crystal wires (waypoints guessed
  at `(40,120)`) drew huge diagonal spikes across the whole sheet; they were changed to
  straight segments. *Direction:* waypoints want to be **relative** (to a pin, or to the
  wire's own endpoints) rather than absolute, or the grammar needs a pin-anchored elbow.

- **F5 — straight wires cut through symbol bodies.** With no routing, a `wire` is a literal
  straight segment between two pin positions; when reflow places the endpoints on opposite
  sides of an intervening symbol (Y1 → U1 across the support column), the wire crosses that
  symbol's box. This is honest ("a wire is a dumb picture", §20d) but reads as a short.
  *Direction:* even a one-elbow auto-dogleg around box bounds would help; full wire routing
  is explicitly out of scope, but crossing *its own* group's boxes is avoidable.

- **F6 — no "place every member of net/group" affordance; every part is named by hand.** The
  14 decoupling caps and 6 series resistors each needed an explicit `sym Cn` / `sym R_x`
  line. There is no `sym-net +3V3` or "flow the rest here" construct, so a real board's
  hundreds of passives would be hundreds of hand-written `sym` lines (or they silently fall
  to the unplaced bin). *Direction:* a container that enumerates a net's members, or a
  "remaining unplaced" sink container that captures the bin into a named group.

- **F7 — `rot`/`dx`/`dy` on a def-instance `sym` are ignored (now surfaced).** A def instance
  expands to a *group*, not a leaf box, so an authored cardinal rotation or pinned offset on
  `sym <instance>` has no v1 meaning and is dropped by reflow (documented on
  `expand_def_syms`). **Fixed this round:** `validate` now emits a non-blocking `W_SCHEMATIC`
  ("`rot`/`dx`/`dy` on def-instance `sym <path>` is ignored") so the drop is never invisible.
  *Direction (still open):* a group-level transform that actually rotates/offsets the whole
  fragment's coordinate frame, rather than just warning.

- **F8 — Comment/Blank round-trip is asymmetric on a leading space.** A `LayoutNode::Comment`
  stores text *without* the leading `# ` separator; the serializer re-adds `# `. Authoring a
  `Comment(" text")` (with a leading space) serializes to `#  text` (two spaces), which the
  parser then strips back to one — a **lossy** round-trip caught by the capstone's byte
  compare (fixed by dropping the authored leading space). *Direction:* normalize on parse (or
  on construction), so a stray leading space can't silently break losslessness.

- **F9 — refdes header uses the *instance path* class, and a def-internal passive gets the
  library part's class, not a schematic-friendly prefix.** In the def-demo the internal
  `Rs`/`Cf` annotate to `R1`/`C1` correctly, but only because the parts are named `R`/`C`
  (whose class prefix matches). The test-lib `Cap` class prefixes with its own name (`Cap1`),
  so a part's *name* leaking into the refdes prefix is a latent surprise. *Direction:* this
  is really an annotate/registry concern surfaced by the schematic header — the class
  registry should own the prefix independent of the part name. (Not new to Round 3; the
  header just made it visible.)

- **F-def-fit — the def-reuse feature wants sub-circuits with internal structure; this board
  hasn't got them.** As above: the capstone's repeats are single connectors, so the genuine
  def-with-layout demonstration had to be a separate synthetic circuit. *Not a framework
  bug* — a note that the *demo vehicle* (this particular board) under-exercises the feature;
  a board with, e.g., a repeated per-channel level-shifter + RC + ESD cluster would show it
  properly. The mechanism itself is verified (three channels, byte-identical geometry).

- **F10 — `sym <def-instance>` for a def with NO fragment hard-errors with a misleading
  message.** `validate` treats a def-instance path as legal only when it is a `def_fragments`
  **key** (i.e. the def carried a `schematic` block). A `sym` naming a *fragmentless* def
  instance is neither a component, a DNP path, nor a fragment key, so it aborts with
  `sym <path> names no component instance` — technically true (there's nothing to place) but
  confusing, since the author wrote a real instance path. The scoped behaviour is "don't
  write a sym for a fragmentless def; its internals bin automatically", but the error should
  say *that*, not "names no component". *Direction:* thread the full def-instance set (not
  just fragment keys) into `validate` so a fragmentless def-instance sym gets a tailored
  `W_SCHEMATIC` ("def instance has no layout fragment; its components bin individually")
  instead of a hard typo error. Deferred — needs the instance set surfaced from elaboration.

**Fixes applied this round (flagged, not deferred):** the `pub`
`serialize_schematic_block` wrapper (F1's workaround — a one-line re-export of the existing
private `serialize_layout`); the F8 leading-space normalization *in the capstone author*
(not the parser — the parser fix is still open); the **F7 ignored-attr `W_SCHEMATIC`**
(above); and making the **fragment-nesting depth cap** honest — `validate` now rejects a
fragment nesting past `MAX_FRAGMENT_DEPTH` with a hard `E_SCHEMATIC` (`fragment_depth`)
rather than letting reflow truncate the over-deep subtree silently. Chosen an error, not a
warning: past the cap the schematic genuinely cannot render the tail, the same fault class
`elaborate::MAX_DEF_DEPTH` treats as an error.

**Worked cleanly (no friction):** def-fragment stamping + path prefixing; the doc-wins
override precedence (`W_SCHEMATIC` when a doc-level `sym` supersedes a fragment placement);
totality (every one of 44 components drawn); the refdes header wiring; and the byte-lossless
round-trip with both a doc-level `schematic` block **and** a def-embedded fragment present.
