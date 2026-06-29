# Chip-Down Multi-SWD Probe — RP2350B (QFN-80) Design Spec

**Status:** RESEARCH / SPEC phase. No implementation here. This document is the
authoritative input for a later implementation phase that uses a custom ECAD framework.

**Project:** Re-spin of the `multi_probe_horizontal` workbench multi-SWD debug probe,
replacing the **Waveshare RP2350-Zero module** carrier with a **single PCB built around a
bare RP2350** plus the support circuitry the module previously integrated.

---

## 0. Sources used (so the next instance can re-verify)

**Local (authoritative for connectivity & pinout):**
- Design intent: `…/multi_probe_horizontal/HSIS.md`
- Connectivity intent: `…/multi_probe_horizontal/multi_probe_horizontal.kicad_sch`
- Module symbol: `…/multi_probe_horizontal/multiprobe.kicad_sym` (`multiprobe:RP2350-Zero`)
- Original PCB (board outline, layer count): `…/multi_probe_horizontal/multi_probe_horizontal.kicad_pcb`
- **RP2350 pinout source of truth:**
  `/home/ben/Documents/kalogon/git/Kalogon-KiCad-Repository/RaspberryPi/MCU_RaspberryPi_RP2350.kicad_sym`
  (symbol name `RP2350_80QFN`; internal text says **"RP2350B QFN-80"**)

**Web (support-circuit requirements — cited inline in §5):**
- **[HDG]** *Hardware design with RP2350* — https://datasheets.raspberrypi.com/rp2350/hardware-design-with-rp2350.pdf
- **[DS]** *RP2350 Datasheet* — https://datasheets.raspberrypi.com/rp2350/rp2350-datasheet.pdf
- **[P2]** *Raspberry Pi Pico 2 Datasheet* — https://datasheets.raspberrypi.com/pico/pico-2-datasheet.pdf
- Reference KiCad: *Minimal-KiCAD.zip* — https://datasheets.raspberrypi.com/rp2350/Minimal-KiCAD.zip (contains QFN-60 **and** QFN-80 Minimal boards)

> ⚠️ **Critical scope note (read first):** The original board used the Waveshare
> **RP2350-Zero = RP2350A (QFN-60, 30 GPIO, ADC on GP26–29)**. The local RP2350 symbol the
> task points at, and the QFN-80 footprint to reuse, are for the **RP2350B (QFN-80, GPIO0–47,
> ADC on GPIO40–47)**. This spec therefore targets the **RP2350B QFN-80**. That is a
> deliberate part change, not a like-for-like swap, and it removes the module's
> "only 20 GPIO exposed" constraint (see §1, §3, §6). Confirm with the user that QFN-80
> is intended (it is what the task text and the supplied symbol/footprint both say).

---

## 1. Overview & intent

A single bare **RP2350B (QFN-80)** acts as **10 independent SWD debug probes**, each exposed on a
**3-pin JST-SH 1.0 mm** connector wired **pin 1 = SWCLK, pin 2 = GND, pin 3 = SWDIO** (matches the
Raspberry Pi Debug Probe "D"/SWD cable). The board is USB-powered, programmed via BOOTSEL/UF2, and
enumerates over native USB as CMSIS-DAP v2 (one interface per probe). This is functionally identical
to the original; the only architectural change is that the support circuitry the Waveshare module
hid (3.3 V supply, crystal, QSPI boot flash, USB front-end, BOOT/RUN buttons, status LED) is now
explicit on-board (§5).

- **10 SWD channels (A–J)** on J1–J10. Direct-drive 3.3 V CMOS, no buffers / no level shifting /
  no series R / no ESD / no VTref / no nRESET — bench tool, per HSIS §2 & §9. Carry this forward.
- **No power to targets.** Targets self-powered; only GND is shared. (HSIS §2.)
- Firmware: `debugprobe` multiprobe build, `PROBE_COUNT=10`. Pin table must match §4.

Channel count **and** the per-channel GPIO assignment are preserved from the original (§4) so the
existing firmware pin table keeps working — even though QFN-80 would allow a cleaner sequential map
(see §4 note and §7-Q3).

---

## 2. Bill of materials

Quantities are the recommended minimum reference design. "Local" = file exists in the Kalogon git
tree; "KiCad std" = present in this machine's `/usr/share/kicad/footprints/` (9.0). Anything marked
**OPEN** has no confirmed local part and needs a decision.

| Ref(s) | Part | Value / MPN | Symbol (path) | Footprint (path) | Notes |
|---|---|---|---|---|---|
| U1 | RP2350B MCU | RP2350B (QFN-80) | **Local:** `Kalogon-KiCad-Repository/RaspberryPi/MCU_RaspberryPi_RP2350.kicad_sym` → symbol `RP2350_80QFN` | **Local:** `Kalogon-KiCad-Repository/RaspberryPi/RP2350_80QFN_minimal.pretty/RP2350-QFN-80-1EP_10x10_P0.4mm_EP3.4x3.4mm_ThermalVias.kicad_mod` (80 pads + EP + thermal vias) | Symbol's own default FP. Generic alt (no vias): `Orbiter-Ultra-Hardware-multi_probe/Orbiter_Ultra.pretty/QFN-80-1EP_10x10mm_P0.4mm_EP3.4x3.4mm.kicad_mod` |
| U2 | QSPI boot flash | Winbond W25Q-series (e.g. W25Q32 / W25Q128), 3.3 V | **OPEN** (no local flash symbol) | **KiCad std:** `Package_SON.pretty/Winbond_USON-8-1EP_3x2mm_P0.5mm_EP0.2x1.6mm.kicad_mod` (USON-8) or `Package_SO.pretty/SOIC-8_3.9x4.9mm_P1.27mm.kicad_mod` (SOIC-8) | Pico 2 = W25Q32RV (4 MB) [P2]; Minimal board = W25Q128JVS (16 MB) [HDG §3.1]. Pick size; package drives FP choice. |
| U3 | 3.3 V regulator | **OPEN** — see §5.1 / §7-Q2 | **OPEN** | **KiCad std:** SOT-23-5 (`Package_TO_SOT_SMD.pretty/SOT-23-5.kicad_mod`) for an AP2112K-3.3 / similar | Module integrated this; bare chip needs an external 5 V→3.3 V rail. ~300–500 mA. |
| Y1 | Crystal | 12.000 MHz, ABM8-272-T3, 10 pF load, ESR ≤50 Ω | **OPEN** (no local xtal symbol; use std 2-pin xtal + 2 GND) | **KiCad std:** `Crystal.pretty/Crystal_SMD_3225-4Pin_3.2x2.5mm.kicad_mod` | ABM8 = 3.2×2.5 mm 4-pad. [HDG §4] |
| L1 | Inductor (core VREG) | 3.3 µH (Abracon AOTA-B201610S3R3-101-T) | n/a | **Local candidate:** `Orbiter_Ultra.pretty/L_Taiyo-Yuden_MD-2020.kicad_mod` (2.0×2.0) or **KiCad std** 2016/2520 metric | ⚠️ RPi note: inductor part/orientation affects regulator stability; AOTA is 2016 metric (2.0×1.6). Verify FP matches chosen L. [HDG §2.1] |
| D1 | Status LED | WS2812B-2020 (addressable RGB) | **OPEN** (WS2812 symbol) | **Local:** `Orbiter_Ultra.pretty/LED_WS2812B-2020_PLCC4_2.0x2.0mm.kicad_mod` | Carries over module's GP16 status LED (HSIS §6). |
| J1–J10 | SWD headers ×10 | JST-SH 3-pin, top-entry, horizontal | **Local:** `multiprobe.kicad_sym`/`Connector_Generic:Conn_01x03` (generic 3-pin) | **Local:** `Orbiter-Ultra-Hardware-multi_probe/Orbiter_Ultra.pretty/JST_SH_SM03B-SRSS-TB_1x03-1MP_P1.00mm_Horizontal.kicad_mod` (also KiCad std `Connector_JST`) | Same FP the original used. |
| J11 | USB connector | USB-C receptacle (or micro-USB) | **OPEN** (USB-C symbol) | **Local:** `Kalogon-KiCad-Repository/ConnorsPCBLibraries/PCB Libraries/USB_C_Receptacle_GCT_USB4125.kicad_mod`; **KiCad std** `Connector_USB.pretty/USB_C_Receptacle_*` | Reference designs use micro-USB (no CC resistors); USB-C needs 2×5.1 kΩ CC. See §5.4. |
| SW1, SW2 | BOOTSEL, RUN buttons | SMD tactile | **OPEN** (switch symbol) | **Local:** `Kalogon-KiCad-Repository/ConnorsPCBLibraries/PCB Libraries/EVQP7A.kicad_mod` | HSIS §6 keeps both buttons. |
| R, C passives | see §5 | 0402 | **OPEN** (generic R/C symbols) | **KiCad std:** `Resistor_SMD.pretty/R_0402_1005Metric.kicad_mod`, `Capacitor_SMD.pretty/C_0402_1005Metric.kicad_mod` | Decoupling, straps, USB series, crystal caps, etc. |

**Open items at a glance:** symbols for flash, 3.3 V regulator, crystal, WS2812, USB-C, tactile
switch, and generic R/C are **not** confirmed in the Kalogon symbol libs (only footprints found, or
KiCad-std). The custom ECAD framework presumably supplies generic passive/IC primitives — confirm.
The only fully-local symbol+footprint pair is **U1 (RP2350)** and **J1–J10 (JST-SH)**.

---

## 3. RP2350B QFN-80 pinout (verified, extracted from the local symbol)

Source: `MCU_RaspberryPi_RP2350.kicad_sym`, symbol `RP2350_80QFN`. Pin **34** is named `SWD` in
the symbol = the chip's **SWDIO**. Pin 81 is the exposed pad (GND).

| # | Name | Type | | # | Name | Type |
|---|---|---|---|---|---|---|
| 1 | GPIO4 | bidir | | 42 | GPIO33 | bidir |
| 2 | GPIO5 | bidir | | 43 | GPIO34 | bidir |
| 3 | GPIO6 | bidir | | 44 | GPIO35 | bidir |
| 4 | GPIO7 | bidir | | 45 | GPIO36 | bidir |
| 5 | **IOVDD** | power_in | | 46 | GPIO37 | bidir |
| 6 | GPIO8 | bidir | | 47 | GPIO38 | bidir |
| 7 | GPIO9 | bidir | | 48 | GPIO39 | bidir |
| 8 | GPIO10 | bidir | | 49 | GPIO40_ADC0 | bidir |
| 9 | GPIO11 | bidir | | 50 | **IOVDD** | power_in |
| 10 | **DVDD** | power_in | | 51 | **DVDD** | power_in |
| 11 | GPIO12 | bidir | | 52 | GPIO41_ADC1 | bidir |
| 12 | GPIO13 | bidir | | 53 | GPIO42_ADC2 | bidir |
| 13 | GPIO14 | bidir | | 54 | GPIO43_ADC3 | bidir |
| 14 | GPIO15 | bidir | | 55 | GPIO44_ADC4 | bidir |
| 15 | **IOVDD** | power_in | | 56 | GPIO45_ADC5 | bidir |
| 16 | GPIO16 | bidir | | 57 | GPIO46_ADC6 | bidir |
| 17 | GPIO17 | bidir | | 58 | GPIO47_ADC7 | bidir |
| 18 | GPIO18 | bidir | | 59 | **ADC_AVDD** | power_in |
| 19 | GPIO19 | bidir | | 60 | **IOVDD** | power_in |
| 20 | GPIO20 | bidir | | 61 | **VREG_AVDD** | power_in |
| 21 | GPIO21 | bidir | | 62 | **VREG_PGND** | power_in |
| 22 | GPIO22 | bidir | | 63 | **VREG_LX** | power_out |
| 23 | GPIO23 | bidir | | 64 | **VREG_VIN** | power_in |
| 24 | **IOVDD** | power_in | | 65 | **VREG_FB** | input |
| 25 | GPIO24 | bidir | | 66 | **USB_DM** | bidir |
| 26 | GPIO25 | bidir | | 67 | **USB_DP** | bidir |
| 27 | GPIO26 | bidir | | 68 | **USB_OTP_VDD** | power_in |
| 28 | GPIO27 | bidir | | 69 | **QSPI_IOVDD** | power_in |
| 29 | **IOVDD** | power_in | | 70 | QSPI_SD3 | bidir |
| 30 | **XIN** | input | | 71 | QSPI_SCLK | output |
| 31 | **XOUT** | passive | | 72 | QSPI_SD0 | bidir |
| 32 | **DVDD** | power_in | | 73 | QSPI_SD2 | bidir |
| 33 | **SWCLK** | output | | 74 | QSPI_SD1 | bidir |
| 34 | **SWD** (SWDIO) | bidir | | 75 | QSPI_SS | bidir |
| 35 | **RUN** | input | | 76 | **IOVDD** | power_in |
| 36 | GPIO28 | bidir | | 77 | GPIO0 | bidir |
| 37 | GPIO29 | bidir | | 78 | GPIO1 | bidir |
| 38 | GPIO30 | bidir | | 79 | GPIO2 | bidir |
| 39 | GPIO31 | bidir | | 80 | GPIO3 | bidir |
| 40 | GPIO32 | bidir | | 81 | **GND** (EP) | power_in |
| 41 | **IOVDD** | power_in | | | | |

**Power-pin census (cross-checked against [DS] / [HDG] §2.2.1):**
- **IOVDD ×8:** pins 5, 15, 24, 29, 41, 50, 60, 76 (QFN-80 has 8; QFN-60 had 6 — extra decoupling, §5).
- **DVDD ×3:** pins 10, 32, 51.
- **Single-pin rails:** QSPI_IOVDD (69), USB_OTP_VDD (68), ADC_AVDD (59), VREG_AVDD (61).
- **Core regulator:** VREG_VIN (64), VREG_LX (63), VREG_FB (65), VREG_PGND (62), VREG_AVDD (61).
- **EP (81) = GND.**

> Cross-check note: the symbol exposes `GPIO40_ADC0…GPIO47_ADC7` (pins 49,52–58) — i.e. ADC lives on
> GPIO40–47 on the **B** variant. This differs from the RP2350A, where ADC was on GP26–29. It means
> the original channel-I/J GPIOs (GP26–29) are **plain digital** on this part — no ADC conflict. ✔

---

## 4. Net-by-net connectivity (netlist intent)

### 4.1 SWD probe channels — GPIO → JST-SH map (PRESERVED from original)

From HSIS §3 and the schematic (J1–J10, each `Conn_01x03`: pin1=SWCLK, pin2=GND, pin3=SWDIO).
SWCLK is always the even/lower GPIO, SWDIO the odd/higher. QFN-80 pin numbers added from §3.

| Probe | Hdr | Pin 1 = SWCLK (GPIO / QFN pin) | Pin 2 | Pin 3 = SWDIO (GPIO / QFN pin) | Net names |
|:--:|:--:|:--:|:--:|:--:|:--|
| A | J1 | GP0 / **77** | GND | GP1 / **78** | `A_SWCLK`, `A_SWDIO` |
| B | J2 | GP2 / **79** | GND | GP3 / **80** | `B_SWCLK`, `B_SWDIO` |
| C | J3 | GP4 / **1** | GND | GP5 / **2** | `C_SWCLK`, `C_SWDIO` |
| D | J4 | GP6 / **3** | GND | GP7 / **4** | `D_SWCLK`, `D_SWDIO` |
| E | J5 | GP8 / **6** | GND | GP9 / **7** | `E_SWCLK`, `E_SWDIO` |
| F | J6 | GP10 / **8** | GND | GP11 / **9** | `F_SWCLK`, `F_SWDIO` |
| G | J7 | GP12 / **11** | GND | GP13 / **12** | `G_SWCLK`, `G_SWDIO` |
| H | J8 | GP14 / **13** | GND | GP15 / **14** | `H_SWCLK`, `H_SWDIO` |
| I | J9 | GP26 / **27** | GND | GP27 / **28** | `I_SWCLK`, `I_SWDIO` |
| J | J10 | GP28 / **36** | GND | GP29 / **37** | `J_SWCLK`, `J_SWDIO` |

All 10 pin-2s tie to the common `GND` plane. These are **bit-banged via PIO** — any GPIO works;
the assignment is a firmware pin-table convention, not a hardware constraint.

> **Recommendation (§7-Q3):** Because the QFN-80 exposes a contiguous GPIO0–GPIO19, a *clean*
> sequential map (A=GP0/1 … J=GP18/19) is now possible and would let the firmware drop its custom
> pin table. **Default to preserving the table above** (zero firmware change, matches HSIS §3 which
> is declared "hardware-fixed"); offer the sequential option only if the user wants to re-baseline
> the firmware.

### 4.2 Power tree (see §5 for values/citations)

```
USB VBUS (5V) ──► U3 (3.3V reg) ──► +3V3 rail
                                       ├─► IOVDD ×8 (5,15,24,29,41,50,60,76)
                                       ├─► QSPI_IOVDD (69)
                                       ├─► USB_OTP_VDD (68)        [must always be powered — OTP]
                                       ├─► ADC_AVDD (59)
                                       ├─► VREG_AVDD (61)          [3.135–3.63 V window]
                                       └─► VREG_VIN (64)           [single-supply scheme, DS Fig 19]
RP2350 core buck:  VREG_LX(63) ─L1(3.3µH)─► +DVDD(1.1V) ─► DVDD ×3 (10,32,51); VREG_FB(65) senses DVDD
GND / VREG_PGND(62) / EP(81) ─► GND plane
```
Targets are **not** powered by this board (no VTref/3V3 out on the JST headers).

### 4.3 Crystal
`XIN(30)` ─ Y1 ─ `XOUT(31)`; 15 pF to GND on each side; 1 kΩ series on the XOUT drive side (§5.2).

### 4.4 QSPI boot flash (U2)
| RP2350 pin | Net | Flash pin |
|---|---|---|
| QSPI_SCLK (71) | `QSPI_SCLK` | CLK |
| QSPI_SS (75) | `QSPI_CS_N` | /CS |
| QSPI_SD0 (72) | `QSPI_SD0` | DI (IO0) |
| QSPI_SD1 (74) | `QSPI_SD1` | DO (IO1) |
| QSPI_SD2 (73) | `QSPI_SD2` | /WP (IO2) |
| QSPI_SD3 (70) | `QSPI_SD3` | /HOLD (IO3) |

Direct, short, no series R. 10 kΩ CS pull-up to +3V3 = **DNF** with a W25Q (internal pull-up
suffices); fit it for other flash (§5.3).

### 4.5 USB
`USB_DP(67)` and `USB_DM(66)` → 27 Ω series each → connector D+/D−. No external DP/DM pulls. USB-C:
add CC1/CC2 5.1 kΩ to GND (§5.4).

### 4.6 BOOTSEL / RUN
- BOOTSEL: SW1 shorts `QSPI_CS_N` (pin 75) to GND through **1 kΩ** (R6 equivalent).
- RUN: SW2 shorts `RUN` (pin 35) to GND. Internal ~50 kΩ pull-up; optional debounce cap.

### 4.7 Status LED
WS2812B `DIN` from a GPIO. **Recommend GP16 (pin 16)** to match the module's GP16 LED net so the
firmware status-LED code is unchanged (HSIS §6/§8). Power from +3V3, 100 nF local decoupling.

### 4.8 Probe-MCU debug (NEW, recommended)
The chip's own `SWCLK(33)` / `SWD/SWDIO(34)` are the RP2350's *own* debug port (for bringing up /
reflashing the probe MCU), **distinct** from the 10 probe channels. The original module did not break
these out (HSIS §6). **Recommend** routing them + GND + 3V3 to a small test-point group or header for
bring-up. Flag for user (§7-Q4).

---

## 5. Support circuit — official requirements (the module→chip delta detail)

All values from official RPi docs; citations inline. The whole point of the chip-down is that these
are now on-board instead of inside the Waveshare module.

### 5.1 Power rails & decoupling
- **External 3.3 V rail required.** RP2350's internal regulator only generates the **1.1 V core
  (DVDD)**; everything else (IOVDD, QSPI_IOVDD, USB_OTP_VDD, ADC_AVDD, VREG_AVDD, and VREG_VIN in the
  recommended scheme) runs from a single **3.135–3.63 V** supply [DS §6.1, §6.3.7 Fig 19]. The
  Waveshare module had this regulator built in; the bare-chip board must add one (U3, **OPEN** part).
- **Core buck regulator** (switching, ~200 mA, replaces RP2040's LDO) [HDG §1.1/§2.1]:
  - L1 = **3.3 µH** (Abracon AOTA-B201610S3R3-101-T) from VREG_LX(63) → DVDD net.
  - Input cap **4.7 µF** on VREG_VIN(64); output cap **4.7 µF** on DVDD node.
  - VREG_AVDD(61): **33 Ω series + 4.7 µF** RC filter to GND [HDG §2.1; DS §6.3.7].
  - VREG_FB(65) ties directly to the regulated DVDD output node. There is **no VREG_VOUT pin** — the
    LX→inductor→DVDD node *is* the core rail.
  - VREG_PGND(62) returns switching current straight to GND; layout-critical [HDG §2.1].
- **DVDD decoupling** [DS §6.1.3]: the two DVDD pins nearest the regulator → **100 nF** each; the
  furthest DVDD pin → **4.7 µF**.
- **IOVDD ×8:** **100 nF per pin** [DS §6.1.1]. (Minimal board shares one 100 nF across adjacent pins
  68/69 — acceptable but spec one-per-pin as default.) All GPIO share one IO voltage; no per-bank IO
  voltage [DS §6.1.1]. Default VOLTAGE_SELECT=0 valid for 2.5–3.3 V.
- **QSPI_IOVDD(69):** 3.3 V (match flash), **100 nF** [DS §6.1.2].
- **USB_OTP_VDD(68):** 3.3 V, **100 nF**. **Must be powered even if USB unused** (feeds OTP)
  [DS §6.1.4].
- **ADC_AVDD(59):** 3.3 V (≥2.97 V for full ADC perf), **100 nF** [DS §6.1.5]. Datasheet mandates
  only the 100 nF; a ferrite/RC filter is *optional* board practice, not required.
- **Power sequencing:** VREG_VIN + VREG_AVDD up together; bring DVDD up with/before ADC_AVDD
  [DS §6.1.8]. Single 3.3 V rail satisfies this naturally.

### 5.2 Crystal [HDG §4]
12.000 MHz **ABM8-272-T3**, ±30 ppm, 10 pF load, ESR ≤50 Ω, between XIN(30)/XOUT(31). Load caps
**15 pF** each to GND. **1 kΩ series resistor** on the drive side (tuned for 3.3 V IOVDD — reduce &
re-test if IOVDD < 3.3 V). Osc powered from IOVDD.

### 5.3 QSPI flash [HDG §3.1]
W25Q-family, 3.3 V. Direct short routing, **no series R**. CS pull-up 10 kΩ to 3.3 V = **DNF** with
W25Q (internal pull-up adequate); populate for other flash. QSPI_SD0–3 need no external pull-ups in
normal operation [HDG §3.3]. (If a 2nd QSPI device on GPIO0-CS is ever added, GPIO0 defaults to
pull-**down** → a 10 kΩ pull-up becomes mandatory — not used here.)

### 5.4 USB [HDG §5.1]
**27 Ω series resistors required** on USB_DP/USB_DM, placed close to the chip; target ~90 Ω
differential. **No external DP/DM pull-ups/downs** (built into RP2350 I/O). RPi reference designs use
**micro-USB (no CC resistors)**. If this board uses **USB-C** (as the Waveshare module did), add the
USB-C-spec **2×5.1 kΩ CC1/CC2 pull-downs to GND** — that requirement is from the USB-C spec, not from
any RPi reference design.

### 5.5 BOOTSEL / RUN [HDG §3.1, §5.4; P2]
- BOOTSEL button → QSPI_SS to GND via **1 kΩ**. Sampled at boot; low → USB mass-storage UF2 mode.
- RUN has internal ~50 kΩ pull-up; reset button simply shorts RUN to GND. No external pull-up/RC
  required by the reference (debounce cap optional).

### 5.6 SWD (probe-MCU debug) [HDG §5.3]
SWCLK/SWDIO straight to a header/test points — **no external components** on the reference designs.

### 5.7 QFN-80 (B) vs QFN-60 (A) support-circuit delta
Only difference vs the A-variant minimal circuit: **8 IOVDD pins instead of 6** → two extra 100 nF
caps. Regulator, crystal, USB, BOOTSEL, RUN circuits are identical [HDG §1.1, §2.1]. No multi-voltage
banks (Bank0/Bank1 are GPIO register banks, not IO-voltage domains) [DS §6.1.1].

---

## 6. Module → chip-down delta summary

| Function | Waveshare RP2350-Zero module provided | Chip-down board must provide |
|---|---|---|
| MCU | RP2350**A** (QFN-60, 30 GPIO) inside module | **RP2350B (QFN-80, 48 GPIO)** bare, U1 + EP/thermal vias |
| 3.3 V supply | On-module regulator from USB 5 V | **U3 external 5 V→3.3 V regulator** (OPEN part) |
| Core 1.1 V | On-module | On-board core buck: L1 3.3 µH + caps (§5.1) |
| 12 MHz crystal | On-module | **Y1 + 2×15 pF + 1 kΩ** (§5.2) |
| QSPI boot flash | On-module (4 MB) | **U2 W25Q** + optional DNF CS pull-up (§5.3) |
| USB front-end | Module USB-C, internal Rs | **J11 connector + 2×27 Ω**; if USB-C, +2×5.1 kΩ CC (§5.4) |
| BOOT / RUN buttons | On module top face | **SW1/SW2** + 1 kΩ BOOTSEL strap (§5.5) |
| Status LED | WS2812B on internal GP16 | **D1 WS2812B**, DIN on GP16 to keep firmware (§4.7) |
| GPIO availability | Only GP0–15 + GP26–29 (20) exposed | All GPIO0–47 available; map preserved but unconstrained (§4.1) |
| Probe-MCU SWD | Not broken out | **Recommend** add test points/header (§4.8) |
| SWD channels A–J | 10× JST-SH on carrier | **Unchanged: 10× JST-SH J1–J10** (§4.1) |

What stays the same: 10 channels, pin-1/2/3 = SWCLK/GND/SWDIO, direct-drive 3.3 V, no buffers/ESD/
VTref/nRESET, JST-SH footprint, CMSIS-DAP-v2 firmware, GP→channel map.

---

## 7. Board constraints

- **Original board outline (reference):** rectangle **34.0 mm × 32.75 mm** (Edge.Cuts x −17→+17,
  y −11.75→+21), corners squared, from `multi_probe_horizontal.kicad_pcb`. The chip-down board will
  likely grow modestly to fit the now-explicit support circuit; treat 34 mm width as a starting point,
  not a hard limit.
- **Layer count:** original is **2-layer** (only `F.Cu`/`B.Cu` populated; gerbers show F_Cu/B_Cu
  only). A QFN-80 with a switching core regulator and USB is feasible on 2 layers but **4-layer is
  recommended** (solid GND plane for the QFN EP return, the VREG_LX/PGND switching loop, and USB
  90 Ω diff pair). Flag as a decision (§7-Q5).
- **Connector edge placement:** original places JST-SH headers in two columns — **J1–J5 on the left
  edge, J6–J10 on the right edge** (schematic X≈60 vs X≈240; PCB confirms left/right split), cable
  exits outward. Preserve this ergonomics: SWD connectors on opposing board edges, USB on a third
  edge, BOOT/RUN buttons accessible on the top face.
- **QFN-80 thermal:** EP (pin 81) to GND with the thermal-via array (the chosen footprint includes
  thermal vias).
- **Mounting:** original module footprint had no explicit board mounting holes broken out; the
  carrier outline had rounded module cutouts only. Add mounting holes per enclosure needs (open).

---

## 8. Open questions / risks / assumptions

- **Q1 — Part variant (highest impact):** Confirm **RP2350B QFN-80** is intended. The original was
  RP2350**A** (QFN-60). Task text + supplied symbol + footprint all say QFN-80, so this spec targets
  it — but it is a real BOM change, not a drop-in. *Assumption: QFN-80 is correct.*
- **Q2 — 3.3 V regulator (U3) part:** No local symbol/footprint chosen. Need MPN + current rating
  (≥~300–500 mA for USB-powered 10-probe operation). Candidate: AP2112K-3.3 (SOT-23-5). What did the
  Waveshare module use, and should we match it? **Decision needed.**
- **Q3 — Preserve vs re-baseline GPIO map:** Default = preserve GP0–15/GP26–29 (no firmware change).
  QFN-80 enables a clean GP0–19 sequential map. Which does the user want? (§4.1)
- **Q4 — Probe-MCU SWD access:** Add a debug header/test points for the RP2350's own SWCLK/SWDIO?
  Strongly recommended for bring-up; original board lacked it. (§4.8)
- **Q5 — Layer count:** 2-layer (match original, cheaper) vs 4-layer (better for QFN-80 EP, the
  switching regulator loop, and USB diff pair). Recommend 4-layer. **Decision needed.**
- **Q6 — USB connector type:** USB-C (matches module, needs 2×5.1 kΩ CC) vs micro-USB (matches RPi
  reference, no CC). Local USB-C footprints exist; assume **USB-C**. Confirm.
- **Q7 — Flash size/package:** 4 MB (Pico 2 / module parity) vs 16 MB (Minimal board). USON-8 vs
  SOIC-8 footprint. Pick.
- **Q8 — Symbols missing locally:** flash, regulator, crystal, WS2812, USB-C, tactile switch, generic
  R/C have footprints (local or KiCad-std) but **no confirmed Kalogon symbol**. The custom ECAD
  framework must supply these primitives — verify before implementation.
- **Q9 — Inductor footprint:** L1 (AOTA 2016-metric) has no exact local footprint;
  `L_Taiyo-Yuden_MD-2020` (2.0×2.0) is a close candidate but is a *different* part. RPi warns
  inductor choice affects regulator stability — match the footprint to the actual L1 chosen.
- **Risk — ADC vs SWD pins:** On RP2350**B**, GP26–29 (channels I/J) are plain digital (ADC is
  GP40–47), so the preserved map has no ADC conflict. ✔ (Would have been a conflict on the A.)
- **Assumption:** Targets remain self-powered; board still supplies no VTref/3V3 and no nRESET on
  the JST headers (HSIS §2/§9 carried forward).
