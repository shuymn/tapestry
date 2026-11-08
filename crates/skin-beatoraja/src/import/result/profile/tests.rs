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
/// 51 N, 34 D.
const NUMERIC: &[(i32, i32, C)] = &[
    (21, 25, C::D),
    (45, 49, C::N),
    (71, 72, C::N),
    (74, 76, C::N),
    (80, 89, C::N),
    (90, 91, C::N),
    (96, 96, C::N),
    (101, 103, C::N),
    (105, 108, C::N),
    (110, 116, C::N),
    (128, 128, C::N),
    (150, 150, C::N),
    (151, 151, C::D),
    (152, 156, C::N),
    (157, 158, C::D),
    (170, 172, C::N),
    (173, 173, C::D),
    (174, 174, C::N),
    (175, 175, C::D),
    (176, 178, C::N),
    (179, 180, C::D),
    (182, 182, C::D),
    (183, 184, C::N),
    (368, 368, C::N),
    (370, 377, C::N),
    (380, 399, C::D),
    (407, 407, C::N),
    (410, 427, C::N),
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
    assert_eq!((count(C::N), count(C::D)), (83, 33));
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
            71 | 101 | 171 => Some(EssentialPart::ResultScore),
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

/// 72 is twice the notes; 85 to 89 each grade's whole percent of the
/// notes, hidden with no notes; 376 and 377 the standard deviation's whole
/// ms and hundredths (`IntegerPropertyFactory.java:127-129`, `:434-440`,
/// `:503-509`).
#[test]
fn derived_result_numbers_read_as_the_source() {
    use skin_model::result_field as f;
    let mut a = [0; f::COUNT];
    for (field, v) in [
        (f::RESULT_VALID, 1),
        (f::CHART_NOTES_VALID, 1),
        (f::CHART_NOTES, 900),
        (f::COUNT_PERFECT, 600),
        (f::COUNT_GREAT, 299),
        (f::COUNT_POOR, 1),
        (f::TIMING_VALID, 1),
        (f::TIMING_SD_X100, 1_234),
    ] {
        a[usize::from(field)] = v;
    }
    let get = |f: &[i32; f::COUNT], id| match number(id) {
        Some(Class::Bridge(v)) => read_number(&v, &ResultFields(*f)),
        _ => Read::Unavailable,
    };
    for (id, want) in [
        (72, 1_800),
        (85, 66),
        (86, 33),
        (87, 0),
        (89, 0),
        (376, 12),
        (377, 34),
    ] {
        assert_eq!(get(&a, id), Read::Value(want), "{id}");
    }
    a[usize::from(f::CHART_NOTES)] = 0;
    for id in 85..=89 {
        assert_eq!(get(&a, id), Read::Unavailable, "{id}");
    }
    a[usize::from(f::RESULT_VALID)] = 0;
    assert_eq!(get(&a, 72), Read::Unavailable);
}
