//! The result skin's startup probe ([`result_probe`]) on self-made skins
//! (design.md "beatoraja Lua Skins", the result skin's preload): its mode
//! and provenance against the kept probe's rule and the runtime permits,
//! a read of an attempt input, and a skin that reads no fact of the viewed
//! result.

use skin_beatoraja::{FactId, vm_hooks};
use skin_model::Layout;

use super::serial;
use crate::imports::result_probe;
use crate::result_skin::tests::{LIMITS, pending, runtime_drawn_skin, skin_reading};
use crate::result_skin::{ProbeOutcome, StartupProbe};
use crate::screens::Scripts;

/// The images of a skin whose import reads only what `reads` reads.
const DOT: &str = "{ { id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 } }";

/// The probe `o`, else a panic naming what came instead.
fn probe(o: ProbeOutcome) -> Box<StartupProbe> {
    match o {
        ProbeOutcome::Probe(p) => p,
        other => panic!("{other:?}"),
    }
}

/// The cells `p`'s layout needs, under its own variant.
fn cells(p: &StartupProbe) -> usize {
    let skin = &p.imported().imported.skin;
    let variant = skin.variant([1_920, 1_080], &Layout::AUTHOR);
    skin.sheet().survey_cells(variant).len()
}

/// The startup probe's mode: with the kept probe's rule met and a permit
/// free it is imported kept, holding the cell only the runtime's callback
/// draws, its runtime and permit gone before it returns; with the rule
/// unmet it is imported plain with no permit taken; with the rule met and
/// both permits held it is imported plain and recorded plain.
#[test]
fn the_startup_probe_records_the_mode_it_took() {
    let _s = serial();
    let d = runtime_drawn_skin();
    let p = pending(d.path(), LIMITS);
    let scripts = Scripts::new();
    let free = scripts.permits_free();

    let (live, passes) = (vm_hooks::live(), vm_hooks::passes());
    let kept = probe(result_probe(&p, &scripts, true, || false).unwrap());
    assert!(kept.kept(), "kept with a permit free");
    assert_eq!(vm_hooks::passes() - passes, 1, "imported once");
    assert_eq!(vm_hooks::live(), live, "its runtime dropped");
    assert_eq!(scripts.permits_free(), free, "its permit released");

    let plain = probe(result_probe(&p, &scripts, false, || false).unwrap());
    assert!(!plain.kept(), "plain when the rule is unmet");
    assert_eq!(scripts.permits_free(), free, "no permit taken");
    assert!(
        cells(&plain) < cells(&kept),
        "no runtime import: the plain probe lacks the runtime's cell"
    );

    let held = [
        scripts.result_permit(&[]).unwrap(),
        scripts.result_permit(&[]).unwrap(),
    ];
    let without = probe(result_probe(&p, &scripts, true, || false).unwrap());
    drop(held);
    assert!(!without.kept(), "plain, recorded plain, with no permit");
    assert_eq!(cells(&without), cells(&plain));
}

/// A startup probe that reads an attempt input stops at it: a skin
/// reading the chart's play level (number 96) records that fact.
#[test]
fn a_startup_probe_reading_an_attempt_input_stops() {
    let _s = serial();
    let d = skin_reading("local nr = ms.number(96)", "", DOT);
    let p = pending(d.path(), LIMITS);
    let o = result_probe(&p, &Scripts::new(), true, || false).unwrap();
    assert!(
        matches!(o, ProbeOutcome::Stopped(FactId::ResultNumber(96))),
        "{o:?}"
    );
}

/// A startup probe that reads no fact of the viewed result is a bundle
/// for every view and is not preloaded.
#[test]
fn a_startup_probe_reading_no_view_fact_is_a_bundle() {
    let _s = serial();
    let d = skin_reading("local nr = 0", "", DOT);
    let p = pending(d.path(), LIMITS);
    let o = result_probe(&p, &Scripts::new(), false, || false).unwrap();
    assert!(matches!(o, ProbeOutcome::Bundle), "{o:?}");
}
