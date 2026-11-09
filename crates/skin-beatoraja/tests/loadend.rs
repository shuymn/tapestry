//! The top-level `loadend`: the play skin's declared least length of its
//! loading presentation (design.md "Play Lifecycle", the loading state),
//! read from JSON and Lua play skins next to `close`.

mod common;

use bms_model::TimeUs;
use common::import_body;
use skin_beatoraja::ImportError;

/// An image drawn once: a body with no note destination.
const DRAWN: &str = r#""image": [{"id": "i", "src": "a", "w": 64, "h": 64}],
    "destination": [{"id": "i", "dst": [{"w": 10, "h": 10}]}]"#;

/// The declared `loadend` of a JSON skin with `field` before [`DRAWN`].
fn json(field: &str) -> Result<Option<TimeUs>, ImportError> {
    import_body(&format!("{field} {DRAWN}"))
        .result
        .map(|i| i.skin.loadend())
}

/// The declared `loadend` of a Lua play skin whose configured pass runs
/// `body` on the table it returns, drawing image `i` once.
fn lua(body: &str) -> Result<Option<TimeUs>, ImportError> {
    let d = tempfile::tempdir().unwrap();
    let text = format!(
        "local skin = {{ type = 0, name = \"t\", w = 1920, h = 1080 }}\n\
         if not skin_config then return skin end\n\
         skin.source = {{ {{ id = \"a\", path = \"a.png\" }} }}\n\
         skin.image = {{ {{ id = \"i\", src = \"a\", x = 0, y = 0, w = 64, h = 64 }} }}\n\
         skin.destination = {{ {{ id = \"i\", dst = {{ {{ x = 0, y = 0, w = 64, h = 64 }} }} }} }}\n\
         {body}\n\
         return skin\n"
    );
    common::write(d.path(), "s.luaskin", text.as_bytes());
    common::png(d.path(), "a.png", 64, 64);
    common::lua_import(d.path()).map(|r| r.imported.skin.loadend())
}

/// `loadend` is its milliseconds in µs, widened before the product so
/// `i32::MAX` ms fits, with no cap; a negative value is 0, and absent
/// declares nothing.
#[test]
fn loadend_is_the_declared_least_length() {
    let ms = |v: i64| Some(TimeUs(v * 1_000));
    for (value, declared) in [
        (None, None),
        (Some("-1"), ms(0)),
        (Some("-2147483648"), ms(0)),
        (Some("0"), ms(0)),
        (Some("500"), ms(500)),
        (Some("1500"), ms(1_500)),
        (Some("2147483647"), Some(TimeUs(2_147_483_647_000))),
    ] {
        let field = value.map_or_else(String::new, |v| format!(r#""loadend": {v},"#));
        assert_eq!(json(&field).unwrap(), declared, "JSON {value:?}");
        let field = value.map_or_else(String::new, |v| format!("skin.loadend = {v}"));
        assert_eq!(lua(&field).unwrap(), declared, "Lua {value:?}");
    }
}

/// `loadend` and `close` are read side by side, each into its own field.
#[test]
fn loadend_is_read_next_to_close() {
    let skin = import_body(&format!(r#""close": 700, "loadend": 2000, {DRAWN}"#))
        .result
        .unwrap()
        .skin;
    assert_eq!(skin.close(), Some(TimeUs(700_000)));
    assert_eq!(skin.loadend(), Some(TimeUs(2_000_000)));
}

/// A malformed `loadend` is refused as `close` is: the wrong type or a
/// value outside `i32`, naming the field.
#[test]
fn a_malformed_loadend_is_refused_as_close_is() {
    /// The refusal's kind and field.
    fn why(e: ImportError) -> (&'static str, &'static str) {
        match e {
            ImportError::Type { field, .. } => ("type", field),
            ImportError::Range { field, .. } => ("range", field),
            other => panic!("{other:?}"),
        }
    }
    for value in [r#""x""#, "true", "[1]", "2147483648", "-2147483649"] {
        let loadend = why(json(&format!(r#""loadend": {value},"#)).unwrap_err());
        let close = why(import_body(&format!(r#""close": {value}, {DRAWN}"#))
            .result
            .unwrap_err());
        assert_eq!(loadend, (close.0, "loadend"), "{value}");
    }
}
