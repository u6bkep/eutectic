//! M2 demo: override strength, decay, and constraint precedence — the
//! reconciliation "hard heart". Run with `cargo run --example m2`.
//!
//! Precedence: hard constraint (Fix) > Pin > Hint > generated default.
//! An override that stops changing the outcome is "ineffective": ineffective
//! hints decay (are collected); ineffective pins are flagged but kept; a pin a
//! constraint contradicts surfaces a loud conflict.

use ecad_core::command::{Command, Transaction};
use ecad_core::doc::Point;
use ecad_core::elaborate::{GenDirective, psu_module};
use ecad_core::history::History;
use ecad_core::id::EntityId;
use ecad_core::part::part_library;
use ecad_core::project::render;

fn rule(t: &str) {
    println!("\n========== {t} ==========");
}

fn main() {
    let lib = part_library();
    let dec0 = EntityId::new("psu.dec[0]");
    // dec[0]'s generated default is (10mm, 0).

    rule("Act A — a casual nudge is a weak HINT (sticks while it does something)");
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(psu_module(2))),
        &lib,
        "psu2",
    )
    .unwrap();
    h.commit(
        Transaction::one(Command::Nudge(dec0.clone(), Point::mm(5, 5))),
        &lib,
        "nudge",
    )
    .unwrap();
    println!("nudged dec[0] to (5mm,5mm):");
    print!("{}", render(h.doc()));
    println!("  -> held as [hint], not a hard pin.");

    rule("Act B — a hint that matches the default DECAYS (no pin accumulation)");
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(psu_module(2))),
        &lib,
        "psu2",
    )
    .unwrap();
    h.commit(
        Transaction::one(Command::Nudge(dec0.clone(), Point::mm(10, 0))),
        &lib,
        "noop-nudge",
    )
    .unwrap();
    println!("nudged dec[0] to (10mm,0) — exactly its default:");
    print!("{}", render(h.doc()));
    println!(
        "  override map now holds {} entries (the dead hint was collected).",
        h.doc().overrides.len()
    );

    rule("Act C — a hard CONSTRAINT outranks a hint; the now-useless hint decays");
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(psu_module(2))),
        &lib,
        "psu2",
    )
    .unwrap();
    h.commit(
        Transaction::one(Command::Nudge(dec0.clone(), Point::mm(5, 5))),
        &lib,
        "nudge",
    )
    .unwrap();
    let mut src = psu_module(2);
    src.push(GenDirective::Fix {
        path: "psu.dec[0]".into(),
        pos: Point::mm(8, 8),
    });
    println!("a mechanical datum fixes dec[0] at (8mm,8mm):");
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "fix")
        .unwrap();
    print!("{}", render(h.doc()));
    println!("  -> constraint wins ([fixed]); the casual hint quietly decays away.");

    rule("Act D — a PIN the constraint contradicts is surfaced LOUDLY and kept");
    let mut h = History::new(Default::default());
    h.commit(
        Transaction::one(Command::SetSource(psu_module(2))),
        &lib,
        "psu2",
    )
    .unwrap();
    h.commit(
        Transaction::one(Command::Pin(dec0.clone(), Point::mm(5, 5))),
        &lib,
        "pin",
    )
    .unwrap();
    let mut src = psu_module(2);
    src.push(GenDirective::Fix {
        path: "psu.dec[0]".into(),
        pos: Point::mm(8, 8),
    });
    println!("user explicitly pinned dec[0] at (5,5); datum then fixes it at (8,8):");
    h.commit(Transaction::one(Command::SetSource(src)), &lib, "fix")
        .unwrap();
    print!("{}", render(h.doc()));
    println!("  -> constraint wins physically, but the explicit pin is kept and the");
    println!("     conflict is reported until the user resolves it. Strength = how loudly");
    println!("     the override objects: hints yield silently, pins do not.");
}
