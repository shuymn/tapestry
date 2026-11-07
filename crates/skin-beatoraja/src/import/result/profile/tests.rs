use skin_model::{RESULT, Scope, Ty, compile};

use super::*;

/// The class letters of the profile table (design §A; docs/stage3.md §3,
/// Result): N, D, one row per id.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum C {
    N,
    D,
    /// A truthful constant (class D with its value).
    K(i32),
}

fn of<T>(c: Option<Class<T>>) -> Option<C> {
    c.map(|c| match c {
        Class::Bridge(_) => C::N,
        Class::Unavailable | Class::Omit => C::D,
    })
}

/// An image index's class: a bridge that is a whole number is a constant.
fn of_index(c: Option<Class<&'static str>>) -> Option<C> {
    match c {
        Some(Class::Bridge(e)) => Some(e.parse().map_or(C::N, C::K)),
        c => of(c),
    }
}

fn rows(spans: &[(i32, i32, C)]) -> Vec<(i32, C)> {
    spans
        .iter()
        .flat_map(|&(a, b, c)| (a..=b).map(move |id| (id, c)))
        .collect()
}

/// §A's numeric ids and the source's bundled skin's five more (S10-09i):
/// 49 N, 36 D.
const NUMERIC: &[(i32, i32, C)] = &[
    (21, 25, C::D),
    (71, 71, C::N),
    (74, 76, C::N),
    (90, 91, C::N),
    (96, 96, C::N),
    (102, 103, C::N),
    (107, 107, C::N),
    (110, 114, C::N),
    (150, 150, C::N),
    (151, 151, C::D),
    (152, 152, C::N),
    (153, 153, C::D),
    (154, 154, C::N),
    (157, 158, C::D),
    (170, 172, C::N),
    (173, 173, C::D),
    (175, 175, C::D),
    (176, 176, C::N),
    (178, 178, C::N),
    (179, 180, C::D),
    (182, 182, C::D),
    (183, 184, C::N),
    (368, 368, C::N),
    (370, 371, C::N),
    (372, 373, C::D),
    (374, 375, C::N),
    (380, 399, C::D),
    (407, 407, C::N),
    (410, 425, C::N),
    (1163, 1164, C::N),
];

/// §A's texts: 4 N, 13 D.
const TEXT: &[(i32, i32, C)] = &[
    (3, 3, C::D),
    (10, 10, C::N),
    (12, 12, C::N),
    (14, 14, C::N),
    (16, 16, C::N),
    (120, 129, C::D),
    (1003, 1003, C::D),
    (1020, 1020, C::D),
];

/// §A's image indices: 2 N, 9 constants.
const IMAGE_INDEX: &[(i32, i32, C)] = &[(42, 42, C::K(0)), (370, 371, C::N), (450, 457, C::K(0))];

/// §A's built-in options: 40 N (180 to 184 the judgment class fact),
/// then the 22 D constants with their values.
const OPTION_N: &[(i32, i32)] = &[
    (90, 91),
    (150, 155),
    (160, 164),
    (177, 177),
    (180, 184),
    (300, 307),
    (320, 327),
    (330, 330),
    (332, 332),
    (335, 335),
    (2244, 2245),
];
const OPTION_D: &[(i32, i32, bool)] = &[
    (50, 50, true),
    (51, 51, false),
    (191, 191, false),
    (196, 196, true),
    (197, 198, false),
    (280, 283, false),
    (289, 290, false),
    (1008, 1008, false),
    (1196, 1196, true),
    (1197, 1198, false),
    (1199, 1199, true),
    (1200, 1201, false),
    (1202, 1202, true),
    (1203, 1204, false),
];

/// C15: every id of §A gets its class, and no other id in a wide range has
/// one: the explicit tables are exactly §A's.
#[test]
fn every_profile_id_has_its_class() {
    for (name, table, f) in [
        (
            "numeric",
            NUMERIC,
            (|id| of(number(id))) as fn(i32) -> Option<C>,
        ),
        ("text", TEXT, |id| of(text(id))),
        ("image index", IMAGE_INDEX, |id| of_index(image_index(id))),
        ("event", &[(42, 42, C::D)], |id| of(event(id))),
    ] {
        let want = rows(table);
        for id in -10..=3_000 {
            let expected = want.iter().find(|&&(i, _)| i == id).map(|&(_, c)| c);
            assert_eq!(f(id), expected, "{name} {id}");
        }
    }
    let count = |c| {
        (-10..=3_000)
            .filter(|&id| of(number(id)) == Some(c))
            .count()
    };
    assert_eq!((count(C::N), count(C::D)), (49, 36));
    let n = OPTION_N.iter().flat_map(|&(a, b)| a..=b);
    let d = OPTION_D.iter().flat_map(|&(a, b, _)| a..=b);
    // 160 to 164 are N (constant per kind) with constant values.
    assert_eq!((n.clone().count(), d.clone().count()), (40, 22));
    for id in 0..=3_000 {
        let class = option(id);
        if n.clone().any(|i| i == id) {
            assert!(matches!(class, Some(Class::Bridge(_))), "option {id}");
        } else if let Some(&(.., v)) = OPTION_D.iter().find(|&&(a, b, _)| (a..=b).contains(&id)) {
            assert_eq!(
                class,
                Some(Class::Bridge(ResOption::Constant(v))),
                "option {id}"
            );
        } else {
            assert_eq!(class, None, "option {id}");
        }
    }
    // Each is a built-in the source's factory knows, so a numeric op
    // reaches the profile before the declared items.
    for id in n.clone().chain(d.clone()) {
        assert!(crate::import::ops::is_builtin(id), "option {id}");
    }
    assert_eq!(option(160), Some(Class::Bridge(ResOption::Constant(true))));
    for id in 180..=184 {
        assert_eq!(option(id), Some(Class::Bridge(ResOption::JudgeClass)));
    }
    assert_eq!(timer(2), Timer::Builtin(result_timer::FADEOUT));
    for id in [0, 173, 174] {
        assert_eq!(timer(id), Timer::Unset, "{id}");
    }
    let supported = (0..3_000)
        .filter(|&id| !matches!(timer(id), Timer::Unsupported))
        .count();
    assert_eq!(supported, 4);
    // The essential parts: the EX score's numbers and the clear type.
    let part = |id| match number(id) {
        Some(Class::Bridge(v)) => v.part,
        _ => None,
    };
    for id in 0..=3_000 {
        let want = match id {
            71 | 171 => Some(EssentialPart::ResultScore),
            370 => Some(EssentialPart::ResultStatus),
            _ => None,
        };
        assert_eq!(part(id), want, "{id}");
    }
    assert!(status_index(370) && !status_index(371));
}

/// Every bridge compiles against the result schema with its type, its
/// validity a boolean, and its declared range is its expression's.
#[test]
fn every_bridge_compiles_against_the_result_schema() {
    let scope = Scope::new(&RESULT);
    for id in 0..=2_000 {
        if let Some(Class::Bridge(v)) = number(id) {
            compile(v.expr, &scope, Ty::Int).unwrap_or_else(|e| panic!("{id}: {e}"));
            compile(v.guard, &scope, Ty::Bool).unwrap();
            assert!(v.range.0 <= v.range.1, "{id}");
            assert!(v.range.0 >= i64::from(i32::MIN), "{id}");
            assert!(v.range.1 <= i64::from(i32::MAX), "{id}");
        }
        if let Some(Class::Bridge(ResOption::Expr(e))) = option(id) {
            compile(e, &scope, Ty::Bool).unwrap();
            compile(&format!("!({e})"), &scope, Ty::Bool).unwrap();
        }
        if let Some(Class::Bridge(e)) = image_index(id) {
            compile(e, &scope, Ty::Int).unwrap();
        }
    }
}

/// The mean's whole ms and hundredths truncate toward 0 with the mean's
/// sign, as the source's (`IntegerPropertyFactory.java:422-433`): −1.49
/// ms is −1 and −49, where floor division would give −2 and 51.
#[test]
fn the_mean_truncates_toward_zero() {
    let mut f = [0; skin_model::result_field::COUNT];
    f[usize::from(skin_model::result_field::RESULT_VALID)] = 1;
    f[usize::from(skin_model::result_field::TIMING_VALID)] = 1;
    let at = |mean: i32| {
        let mut f = f;
        f[usize::from(skin_model::result_field::TIMING_MEAN_X100)] = mean;
        let fields = ResultFields(f);
        let get = |id| match number(id) {
            Some(Class::Bridge(v)) => read_number(&v, &fields),
            _ => Read::Unavailable,
        };
        (get(374), get(375))
    };
    assert_eq!(at(-149), (Read::Value(-1), Read::Value(-49)));
    assert_eq!(at(149), (Read::Value(1), Read::Value(49)));
    assert_eq!(at(-5), (Read::Value(0), Read::Value(-5)));
    assert_eq!(at(-14_900), (Read::Value(-149), Read::Value(0)));
    // No samples: unavailable.
    let none = ResultFields([0; skin_model::result_field::COUNT]);
    let Some(Class::Bridge(v)) = number(374) else {
        panic!()
    };
    assert_eq!(read_number(&v, &none), Read::Unavailable);
}
