//! M1 demo: drives the engine through the load-bearing behaviours and narrates
//! what each one demonstrates. Run with `cargo run --example m1`.

use ecad_core::command::{Command, Transaction};
use ecad_core::doc::Point;
use ecad_core::elaborate::{GenDirective, Source, psu_module};
use ecad_core::history::History;
use ecad_core::id::EntityId;
use ecad_core::part::part_library;
use ecad_core::project::render;
use ecad_core::query::{Engine, Key};

fn rule(title: &str) {
    println!("\n========== {title} ==========");
}

fn main() {
    let lib = part_library();

    // -- Act 1: typed interfaces make the serial swap unrepresentable -----------
    rule("Act 1 — typed interface connection (tx<->rx crossing is automatic)");
    let uart: Source = vec![
        GenDirective::Instance {
            path: "mcu".into(),
            part: "MCU".into(),
        },
        GenDirective::Instance {
            path: "sens".into(),
            part: "Sensor".into(),
        },
        GenDirective::ConnectInterface {
            a: ("mcu".into(), "uart".into()),
            b: ("sens".into(), "uart".into()),
        },
    ];
    let mut h = History::new(Default::default());
    h.commit(Transaction::one(Command::SetSource(uart)), &lib, "uart")
        .unwrap();
    print!("{}", render(h.doc()));
    println!("  -> note tx pairs with rx; wiring tx-to-tx is not expressible.");

    // -- Act 2: ERC is a typecheck over roles -----------------------------------
    rule("Act 2 — ERC as a query over pin roles");
    let contention: Source = vec![
        GenDirective::Instance {
            path: "reg1".into(),
            part: "LDO".into(),
        },
        GenDirective::Instance {
            path: "reg2".into(),
            part: "LDO".into(),
        },
        GenDirective::ConnectPins {
            net: "VBUS".into(),
            pins: vec![
                ("reg1".into(), "VOUT".into()),
                ("reg2".into(), "VOUT".into()),
            ],
        },
    ];
    let mut h2 = History::new(Default::default());
    h2.commit(
        Transaction::one(Command::SetSource(contention)),
        &lib,
        "contend",
    )
    .unwrap();
    let mut eng2 = Engine::new();
    let erc = eng2.query(h2.doc(), &lib, Key::Erc);
    println!("  ERC violations: {:?}", erc.as_erc());
    println!("  (two PowerOut pins driving one net — caught.)");

    // -- Act 3: incremental query engine: skip & early cutoff -------------------
    rule("Act 3 — incremental recompute (dependency-skip and early-cutoff)");
    let mut h3 = History::new(Default::default());
    h3.commit(
        Transaction::one(Command::SetSource(psu_module(2))),
        &lib,
        "psu2",
    )
    .unwrap();
    let mut eng = Engine::new();
    eng.query(h3.doc(), &lib, Key::Erc);
    println!(
        "  initial query     -> Netlist ran {}x, ERC ran {}x",
        eng.count(Key::Netlist),
        eng.count(Key::Erc)
    );

    h3.commit(
        Transaction::one(Command::Nudge(EntityId::new("psu.dec[0]"), Point::mm(5, 5))),
        &lib,
        "nudge",
    )
    .unwrap();
    eng.query(h3.doc(), &lib, Key::Erc);
    println!(
        "  after geometry nudge -> Netlist ran {}x, ERC ran {}x  (both skipped: connectivity untouched)",
        eng.count(Key::Netlist),
        eng.count(Key::Erc)
    );

    let mut src = psu_module(2);
    src.push(GenDirective::Instance {
        path: "psu.spare".into(),
        part: "Cap".into(),
    });
    h3.commit(Transaction::one(Command::SetSource(src)), &lib, "spare")
        .unwrap();
    eng.query(h3.doc(), &lib, Key::Erc);
    println!(
        "  after unconnected add -> Netlist ran {}x, ERC ran {}x  (Netlist recomputed, value identical, ERC cut off)",
        eng.count(Key::Netlist),
        eng.count(Key::Erc)
    );

    // -- Act 4: override survives re-elaboration; orphans surface ---------------
    rule("Act 4 — generative reconciliation (minimal perturbation + orphan surfacing)");
    let mut h4 = History::new(Default::default());
    h4.commit(
        Transaction::one(Command::SetSource(psu_module(3))),
        &lib,
        "psu3",
    )
    .unwrap();
    h4.commit(
        Transaction::one(Command::Nudge(
            EntityId::new("psu.dec[1]"),
            Point::mm(42, 7),
        )),
        &lib,
        "pin dec1",
    )
    .unwrap();
    println!("  pinned psu.dec[1] at (42mm,7mm), then grew the generator 3 -> 5 caps:");
    h4.commit(
        Transaction::one(Command::SetSource(psu_module(5))),
        &lib,
        "psu5",
    )
    .unwrap();
    print!("{}", render(h4.doc()));
    println!("  -> dec[1] kept its pinned spot; the rest sit at generated defaults.");

    println!("\n  now shrink the generator 5 -> 1 so dec[1] no longer exists:");
    h4.commit(
        Transaction::one(Command::SetSource(psu_module(1))),
        &lib,
        "psu1",
    )
    .unwrap();
    print!("{}", render(h4.doc()));
    println!("  -> the orphaned override is surfaced as a conflict, not silently dropped.");

    // -- Act 5: version DAG ------------------------------------------------------
    rule("Act 5 — version DAG (undo)");
    println!("  components now: {}", h4.doc().components.len());
    h4.undo();
    println!(
        "  after undo:     {}  (back to the 5-cap version)",
        h4.doc().components.len()
    );
}
