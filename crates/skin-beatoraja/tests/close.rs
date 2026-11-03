//! The top-level `close`: Closing's declared duration (design.md "Play
//! Lifecycle", Closing), read from JSON and Lua skins with or without a
//! note destination.

mod common;

use bms_model::TimeUs;
use common::import_body;
use skin_beatoraja::ImportError;

/// An image drawn once: a body with no note destination.
const DRAWN: &str = r#""image": [{"id": "i", "src": "a", "w": 64, "h": 64}],
    "destination": [{"id": "i", "dst": [{"w": 10, "h": 10}]}]"#;

/// The declared duration of a JSON skin with `field` before [`DRAWN`].
fn json(field: &str) -> Result<Option<TimeUs>, ImportError> {
    import_body(&format!("{field} {DRAWN}"))
        .result
        .map(|i| i.skin.close())
}

/// A Lua play skin whose header pass runs `header` and whose configured
/// pass runs `body`, each on the table it returns, drawing image `i`
/// once.
fn lua(header: &str, body: &str) -> Result<Option<TimeUs>, ImportError> {
    let d = tempfile::tempdir().unwrap();
    let text = format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then {header} return skin end\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
         skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 64, h = 64 }} }}\n\
         skin.destination = {{ {{ id = \"i\", dst = {{ {{ x = 0, y = 0, w = 64, h = 64 }} }} }} }}\n\
         {body}\n\
         return skin\n"
    );
    common::write(d.path(), "s.luaskin", text.as_bytes());
    common::png(d.path(), "a.png", 64, 64);
    common::lua_import(d.path()).map(|r| r.imported.skin.close())
}

/// A positive `close` is its milliseconds in µs, widened before the
/// product so `i32::MAX` ms fits; absent, 0 or negative declares
/// nothing. The skins have no note destination.
#[test]
fn close_is_the_declared_duration() {
    let ms = |v: i64| Some(TimeUs(v * 1_000));
    for (value, declared) in [
        (None, None),
        (Some("0"), None),
        (Some("-1"), None),
        (Some("-2147483648"), None),
        (Some("1"), ms(1)),
        (Some("500"), ms(500)),
        (Some("3000"), ms(3_000)),
        (Some("2147483647"), Some(TimeUs(2_147_483_647_000))),
    ] {
        let field = value.map_or_else(String::new, |v| format!(r#""close": {v},"#));
        assert_eq!(json(&field).unwrap(), declared, "JSON {value:?}");
        let field = value.map_or_else(String::new, |v| format!("skin.close = {v}"));
        assert_eq!(lua("", &field).unwrap(), declared, "Lua {value:?}");
    }
    // A fraction truncates as every integer field does.
    assert_eq!(json(r#""close": 1500.9,"#).unwrap(), ms(1_500));
    assert_eq!(lua("", "skin.close = 1500.9").unwrap(), ms(1_500));
}

/// The configured pass's table is the definition: a `close` the header
/// pass alone sets is not read, one the configured pass sets is.
#[test]
fn a_header_only_close_is_not_read() {
    assert_eq!(lua("skin.close = 3000", "").unwrap(), None);
    assert_eq!(
        lua("skin.close = 3000", "skin.close = 2000").unwrap(),
        Some(TimeUs(2_000_000))
    );
}

/// A malformed `close` is refused exactly as `judgetimer`, the other
/// top-level integer, is: the wrong type or a value outside `i32`.
#[test]
fn a_malformed_close_is_refused_as_judgetimer_is() {
    /// The refusal's kind and field.
    fn why(e: ImportError) -> (&'static str, &'static str) {
        match e {
            ImportError::Type { field, .. } => ("type", field),
            ImportError::Range { field, .. } => ("range", field),
            other => panic!("{other:?}"),
        }
    }
    for value in [
        r#""x""#,
        "true",
        "[1]",
        "{}",
        "2147483648",
        "-2147483649",
        "1e10",
    ] {
        let close = why(json(&format!(r#""close": {value},"#)).unwrap_err());
        let judgetimer = import_body(&format!(r#""judgetimer": {value}, {DRAWN}"#)).err();
        assert_eq!(close.1, "close", "{value}");
        assert_eq!(close.0, why(judgetimer).0, "{value}");
    }
    for value in [r#""x""#, "true", "{}", "2147483648", "1e10"] {
        let close = why(lua("", &format!("skin.close = {value}")).unwrap_err());
        let judgetimer = why(lua("", &format!("skin.judgetimer = {value}")).unwrap_err());
        assert_eq!(close, (judgetimer.0, "close"), "{value}");
    }
}
