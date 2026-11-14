//! The top-level `finishmargin` and `fadeout` of a play skin: the play's
//! declared ending (design.md "Play Lifecycle", Audio tail), read from
//! JSON and Lua skins with or without a note destination, up to the
//! source's largest `int` with no cap, a negative value read as 0.

mod common;

use bms_model::TimeUs;
use common::import_body;
use skin_beatoraja::ImportError;
use skin_model::PlayEnding;

/// An image drawn once: a body with no note destination.
const DRAWN: &str = r#""image": [{"id": "i", "src": "a", "w": 64, "h": 64}],
    "destination": [{"id": "i", "dst": [{"w": 10, "h": 10}]}]"#;

/// The declared ending of a JSON play skin with `fields` before [`DRAWN`].
fn json(fields: &str) -> Result<PlayEnding, ImportError> {
    import_body(&format!("{fields} {DRAWN}"))
        .result
        .map(|i| i.skin.play_ending())
}

/// The declared ending of a Lua play skin whose configured pass runs
/// `body` on the table it returns, drawing image `i` once.
fn lua(body: &str) -> Result<PlayEnding, ImportError> {
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
    common::lua_import(d.path()).map(|r| r.imported.skin.play_ending())
}

const fn ending(finish_margin_ms: i64, fadeout_ms: i64) -> PlayEnding {
    PlayEnding {
        finish_margin: TimeUs(finish_margin_ms * 1_000),
        fadeout: TimeUs(fadeout_ms * 1_000),
    }
}

/// Each is its milliseconds in µs, widened before the product so the
/// whole `i32` range fits with no cap; each is at least 0, a negative
/// value read as 0. The skins have no note destination.
#[test]
fn a_play_skins_ending_is_read_in_microseconds() {
    for (margin, fadeout, read) in [
        (None, None, ending(0, 0)),
        (Some("3000"), Some("1000"), ending(3_000, 1_000)),
        (Some("3000"), None, ending(3_000, 0)),
        (None, Some("1200"), ending(0, 1_200)),
        (Some("-5"), Some("-5"), ending(0, 0)),
        (Some("3000"), Some("-1"), ending(3_000, 0)),
        (Some("12000"), Some("11000"), ending(12_000, 11_000)),
        (
            Some("2147483647"),
            Some("2147483647"),
            ending(2_147_483_647, 2_147_483_647),
        ),
        (Some("-2147483648"), Some("-2147483648"), ending(0, 0)),
    ] {
        let mut fields = String::new();
        let mut body = String::new();
        if let Some(v) = margin {
            fields.push_str(&format!(r#""finishmargin": {v},"#));
            body.push_str(&format!("skin.finishmargin = {v}\n"));
        }
        if let Some(v) = fadeout {
            fields.push_str(&format!(r#""fadeout": {v},"#));
            body.push_str(&format!("skin.fadeout = {v}\n"));
        }
        assert_eq!(json(&fields).unwrap(), read, "JSON {margin:?} {fadeout:?}");
        assert_eq!(lua(&body).unwrap(), read, "Lua {margin:?} {fadeout:?}");
    }
}

/// A malformed value is refused naming its field, as every top-level
/// integer is.
#[test]
fn a_malformed_ending_is_refused_with_its_field() {
    for field in ["finishmargin", "fadeout"] {
        for value in [r#""x""#, "2147483648", "-2147483649"] {
            let e = json(&format!(r#""{field}": {value},"#)).unwrap_err();
            match e {
                ImportError::Type { field: f, .. } | ImportError::Range { field: f, .. } => {
                    assert_eq!(f, field, "{value}");
                }
                other => panic!("{field} {value}: {other:?}"),
            }
        }
    }
}
