use skin_model::decision_timer;

use super::*;

/// The decision options with a constant, by id range and value
/// (docs/stage3.md §3, Decision).
const CONSTANTS: &[(i32, i32, bool)] = &[
    (32, 33, false),
    (50, 50, true),
    (51, 51, false),
    (160, 160, true),
    (161, 164, false),
    (280, 283, false),
    (289, 290, false),
    (1_008, 1_008, false),
];

/// Every id of each space has the table's class and no other id has
/// one: the explicit tables are exactly docs/stage3.md §3's.
#[test]
fn every_profile_id_has_its_class() {
    for id in -10..=3_000 {
        let want = match id {
            10..=16 => Some(Class::Bridge(())),
            1_003 => Some(Class::Omit),
            _ => None,
        };
        assert_eq!(text(id), want, "text {id}");
    }
    for id in -10..=3_000 {
        let want = match CONSTANTS.iter().find(|&&(a, b, _)| (a..=b).contains(&id)) {
            Some(&(.., v)) => Some(Class::Bridge(ResOption::Constant(v))),
            None if (180..=184).contains(&id) => Some(Class::Bridge(ResOption::JudgeClass)),
            None => None,
        };
        assert_eq!(option(id), want, "option {id}");
        // Each is a built-in the source's factory knows, so a numeric op
        // reaches the profile before the declared items.
        if want.is_some() {
            assert!(crate::import::ops::is_builtin(id), "option {id}");
        }
    }
    // The stage file, banner and back image wait for S10-12.
    for id in 190..=195 {
        assert_eq!(option(id), None, "option {id}");
    }
    assert_eq!(timer(1), Timer::Builtin(decision_timer::INPUT));
    assert_eq!(timer(2), Timer::Builtin(decision_timer::FADEOUT));
    assert_eq!(timer(0), Timer::Unset);
    let supported = (0..3_000)
        .filter(|&id| !matches!(timer(id), Timer::Unsupported))
        .count();
    assert_eq!(supported, 3);
}
