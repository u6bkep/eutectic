//! Library packages, slice 1: `load_library` over the real `poc/parts` package —
//! the equivalence spot-checks proving the manifest reproduces what the old
//! hand-rolled `build_lib()` built (footprint imports, a clean RP2350A symbol
//! join, role overlays with shared functional names).

use eutectic_core::library::load_library;
use eutectic_core::part::PinRole;
use std::path::PathBuf;

fn poc_parts() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../poc/parts")
}

#[test]
fn poc_library_package_matches_build_lib() {
    let lib = load_library(&poc_parts()).expect("poc/parts loads");

    // The full part roster, exactly the old build_lib() set.
    assert_eq!(
        lib.keys().map(String::as_str).collect::<Vec<_>>(),
        vec![
            "BTN", "C", "IND", "JST_SH", "LED", "R", "REG", "RP2350A", "USBC", "W25Q", "XTAL",
        ]
    );

    // RP2350A: symbol-joined cleanly (the loader errors on a dirty join, so presence
    // is the proof), with the QFN-60's 61 pads (60 signal/power + 1 EP) and the
    // shared functional names a symbol join brings: six IOVDD pads fan out.
    let mcu = &lib["RP2350A"];
    assert_eq!(mcu.pins.len(), 61);
    assert_eq!(
        mcu.pins.iter().filter(|p| p.name == "IOVDD").count(),
        6,
        "the six IOVDD pads share a functional name"
    );

    // W25Q role map: pin "1" is CS_N with role Input.
    let w25q = &lib["W25Q"];
    let p1 = w25q.pins.iter().find(|p| p.number == "1").expect("pad 1");
    assert_eq!(p1.name, "CS_N");
    assert_eq!(p1.role, PinRole::Input);

    // USBC: the A-side has exactly two VBUS-named pads (A4, A9), both PowerIn.
    let usbc = &lib["USBC"];
    let a_vbus: Vec<&str> = usbc
        .pins
        .iter()
        .filter(|p| p.name == "VBUS" && p.number.starts_with('A'))
        .map(|p| p.number.as_str())
        .collect();
    assert_eq!(a_vbus, vec!["A4", "A9"]);
    assert!(
        usbc.pins
            .iter()
            .filter(|p| p.name == "VBUS")
            .all(|p| p.role == PinRole::PowerIn)
    );

    // R: footprint-only, so every pin is Passive.
    assert!(
        lib["R"].pins.iter().all(|p| p.role == PinRole::Passive),
        "a footprint-only part imports all-Passive"
    );
}
