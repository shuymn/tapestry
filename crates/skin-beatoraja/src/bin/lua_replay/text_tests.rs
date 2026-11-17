//! A play text whose value the import folded to a literal, against its
//! closure (design.md "beatoraja Lua Skins", the reference executor): the
//! literal cleaned as the widget cleans it, the closure's string respelled
//! and then cleaned, the reference VM's whole floats counted.

use skin_beatoraja::lua::facts::Fact;
use skin_beatoraja::lua::replay::Recorded;
use skin_model::{ChartText, Layout};
use tempfile::TempDir;

use crate::observed::{Observed, Outcome, Point, Raw, intern};
use crate::production::compare::{NOT_FOLDED, TEXT_SPELLED, clean};
use crate::report::Report;
use crate::script::Script;
use crate::tests::{folder, options};
use crate::{Options, Stop, replay};

/// GenericTheme's TOTAL readout, written fresh.
const READOUT: &str = "function() local t = ms.number(368) local n = ms.number(74) \
     local b = math.max(260.0, 7.605 * n / (0.01 * n + 6.5)) \
     local p = t / b * 100 p = math.ceil(p * 100) / 100 \
     return t .. \" (\" .. p .. \"%)\" end";

/// A text whose empty field falls back to a string: lowered to a field,
/// not to a literal.
const FIELD_OR: &str = "function() local tx = ms.text(13) \
     if not tx or tx == \"\" then return \"none\" else return tx end end";

/// A skin folder with one text per value, each on its own destination, in
/// an outline font.
fn texts_folder(values: &[&str]) -> TempDir {
    let texts: Vec<String> = values
        .iter()
        .enumerate()
        .map(|(k, v)| format!("{{ id = \"t{k}\", font = \"f\", size = 10, value = {v} }}"))
        .collect();
    let dsts: String = (0..values.len())
        .map(|k| {
            format!(
                "table.insert(skin.destination, {{ id = \"t{k}\", dst = {{ {{ y = {}, w = 64, h = 10 }} }} }})\n",
                k * 20
            )
        })
        .collect();
    let d = folder(&format!(
        "skin.font = {{ {{ id = \"f\", path = \"f.ttf\" }} }}\n\
         skin.text = {{ {} }}\n\
         {dsts}",
        texts.join(", ")
    ));
    std::fs::write(d.path().join("f.ttf"), [0, 1, 0, 0, 0]).unwrap();
    d
}

/// The replay of `values` under `o`, as `lua_replay` runs it.
fn replayed(values: &[&str], o: &Options) -> Report {
    let d = texts_folder(values);
    match replay(d.path(), "s.luaskin", o) {
        Ok(r) => r,
        Err(Stop::Failed(e)) => panic!("failed: {e}"),
        Err(Stop::Differs) => panic!("the imports differ"),
    }
}

/// What the import of `values` under `o` gives the player: each text
/// value's recorded literal, in record order, and the skin's text
/// requests.
fn imported(values: &[&str], o: &Options) -> (Vec<Option<String>>, Vec<String>) {
    let d = texts_folder(values);
    let (rec, skin) = import(&d, o);
    let requests = skin
        .text_requests(&ChartText::default(), [1_920, 1_080], &Layout::AUTHOR)
        .into_iter()
        .map(|r| r.text)
        .collect();
    (literals(&rec), requests)
}

fn import(d: &TempDir, o: &Options) -> (Recorded, skin_model::Skin) {
    let cancel = skin_beatoraja::Cancel::new();
    let h = skin_beatoraja::read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let config = skin_beatoraja::Config::named(h.header(), [], []).unwrap();
    let (out, rec) = h.import_recorded(&config, &o.import_facts(), &cancel);
    let skin_beatoraja::LuaOutcome::Imported(i) = out.unwrap() else {
        panic!("pending");
    };
    (rec, i.imported.skin)
}

/// Each text value occurrence's literal, the last record of a node.
fn literals(rec: &Recorded) -> Vec<Option<String>> {
    let mut by_node: Vec<(u32, Option<String>)> = Vec::new();
    for o in rec.occurrences.iter().filter(|o| o.field == "value") {
        let l = o.lowered.as_ref().and_then(|l| l.literal.clone());
        match by_node.iter_mut().find(|(n, _)| *n == o.node) {
            Some(e) => e.1 = l,
            None => by_node.push((o.node, l)),
        }
    }
    by_node.into_iter().map(|(_, l)| l).collect()
}

/// [`replayed`] with production's record and the reference's observation
/// changed before they are compared.
fn tampered(
    values: &[&str],
    o: &Options,
    rec_edit: impl FnOnce(&mut Recorded),
    obs_edit: impl FnOnce(&mut Observed),
) -> Report {
    let d = texts_folder(values);
    let (mut rec, skin) = import(&d, o);
    rec_edit(&mut rec);
    let cancel = skin_beatoraja::Cancel::new();
    let h = skin_beatoraja::read_lua_header(d.path(), "s.luaskin", &cancel).unwrap();
    let config = skin_beatoraja::Config::named(h.header(), [], []).unwrap();
    let facts = o.import_facts();
    let (run, inputs) = crate::production::record(&skin, rec, &o.script, Some(12)).unwrap();
    let mut obs = crate::reference::run_rounds(
        &inputs,
        || {
            let (_, rec, vm) = h.import_reference(&config, &facts, &cancel);
            Ok(crate::reference::Fresh {
                vm: vm.unwrap(),
                items: rec.items,
                sources: rec.sources,
            })
        },
        None,
    )
    .unwrap();
    obs.root = d.path().to_path_buf();
    obs_edit(&mut obs);
    crate::production::compare::compare(&run, &obs, &[])
}

/// The play frames a report built: every frame but loading and the page.
fn play_frames(r: &Report) -> usize {
    r.kinds
        .iter()
        .filter(|(k, _)| !matches!(**k, "loading" | "page"))
        .map(|(_, (n, _))| n)
        .sum()
}

/// The text strings agreed, over every kind of frame.
fn texts_agreed(r: &Report) -> usize {
    r.texts.values().sum()
}

/// Passes, with exactly one text agreed on each play frame, READY, play
/// and the tail among them, the reference's whole floats counted.
fn agrees_on_every_play_frame(r: &Report) {
    let text = r.text(0, true, true);
    assert!(r.passes(), "{text}");
    assert_eq!(texts_agreed(r), play_frames(r), "{text}");
    for kind in ["ready", "play", "tail"] {
        assert!(r.texts.get(kind).is_some_and(|&n| n > 0), "{kind}: {text}");
    }
    assert!(
        r.intended.get(TEXT_SPELLED).is_some_and(|&n| n > 0),
        "{text}"
    );
}

/// GenericTheme's readout over the script chart's facts agrees with its
/// closure on every play frame of `full`, `mixed` and every round of
/// `rounds`, the reference writing the total as a float; the import's
/// literal is the chart's string and the player is asked for it. A text
/// lowered to a field with its fallback is counted, never compared.
#[test]
fn a_folded_text_agrees_with_its_closure() {
    for script in [Script::full(), Script::mixed(), Script::rounds()] {
        let name = script.name;
        let o = options(script);
        let r = replayed(&[READOUT, FIELD_OR], &o);
        agrees_on_every_play_frame(&r);
        assert_eq!(
            r.buckets.get(NOT_FOLDED).copied(),
            Some(play_frames(&r)),
            "{name}: {}",
            r.text(0, true, true)
        );
        if name == "rounds" {
            // Every round's play frames are among those agreed above.
            assert!(r.rounds.len() > 1, "{}", r.text(0, true, true));
            assert!(r.kinds.get("rearmed ready").is_some_and(|k| k.0 > 0));
        }
    }
    // 7 notes and no `#TOTAL`: 260, the whole chart's 100 %.
    let (lit, req) = imported(&[READOUT], &options(Script::full()));
    assert_eq!(lit, [Some("260 (100%)".to_owned())]);
    assert_eq!(req, ["260 (100%)"]);
    // 13 notes: the same total, the same string.
    let (lit, _) = imported(&[READOUT], &options(Script::mixed()));
    assert_eq!(lit, [Some("260 (100%)".to_owned())]);
}

/// Each cleaning twin: the record keeps the fold before cleaning, the
/// player is asked for it cleaned, and the comparison agrees with the
/// closure's string respelled before it is cleaned.
#[test]
fn a_folded_text_is_compared_as_the_player_receives_it() {
    let a = |n: usize| "a".repeat(n);
    let cases: Vec<(String, String, Vec<String>)> = vec![
        // 257 bytes: the request is cut to 256; the reference's 261 bytes
        // respell to 257 before the same cut.
        (
            format!(
                "function() local t = ms.number(368) return \"{}\" .. t .. \"/\" .. t end",
                a(250)
            ),
            format!("{}260/260", a(250)),
            vec![format!("{}260/26", a(250))],
        ),
        // Control characters dropped.
        (
            "function() local t = ms.number(368) return \"a\\tb\\127\" .. t end".to_owned(),
            "a\tb\u{7f}260".to_owned(),
            vec!["ab260".to_owned()],
        ),
        // A multibyte character past the cut is dropped whole.
        (
            format!(
                "function() local t = ms.number(368) return t .. \"/\" .. t .. \"{}\" .. \"é\" end",
                a(248)
            ),
            format!("260/260{}é", a(248)),
            vec![format!("260/260{}", a(248))],
        ),
        // One byte shorter, it stays.
        (
            format!(
                "function() local t = ms.number(368) return t .. \"/\" .. t .. \"{}\" .. \"é\" end",
                a(247)
            ),
            format!("260/260{}é", a(247)),
            vec![format!("260/260{}é", a(247))],
        ),
    ];
    for (value, literal, requests) in cases {
        let o = options(Script::full());
        let r = replayed(&[&value], &o);
        agrees_on_every_play_frame(&r);
        let (lit, req) = imported(&[&value], &o);
        // The comparator's cleaning is the text the player is asked for.
        assert_eq!(req, [clean(&literal)]);
        assert_eq!(lit, [Some(literal)]);
        assert_eq!(req, requests);
    }
    // A fold the widget cleans away: no request, and the closure's string
    // cleans away too.
    let value = "function() return \"\\t\" .. \"\\n\" end";
    let o = options(Script::full());
    let r = replayed(&[value], &o);
    assert!(r.passes(), "{}", r.text(0, true, true));
    assert_eq!(texts_agreed(&r), play_frames(&r));
    let (lit, req) = imported(&[value], &o);
    assert_eq!(lit, [Some("\t\n".to_owned())]);
    assert!(req.is_empty(), "{req:?}");
}

/// The text findings of `r`, each with its detail.
fn text_findings(r: &Report) -> Vec<(String, Option<String>)> {
    r.findings
        .iter()
        .filter(|f| f.what.starts_with("text"))
        .map(|f| (f.what.clone(), f.detail.clone()))
        .collect()
}

/// Wrong strings on either side are found: a chart's `#TOTAL` folds and
/// agrees through the respelling, and a production string rounded the
/// other way, a reference string written otherwise, and production facts
/// from another chart are each a finding on every play frame; with the
/// total unavailable nothing is compared.
#[test]
fn text_mutations_are_found() {
    let mut script = Script::full();
    script.total = Some(300);
    let o = options(script);
    let r = replayed(&[READOUT], &o);
    agrees_on_every_play_frame(&r);
    let (lit, _) = imported(&[READOUT], &o);
    assert_eq!(lit, [Some("300 (115.39%)".to_owned())]);
    // Production's ceil folded as floor.
    let r = tampered(
        &[READOUT],
        &o,
        |rec| {
            for occ in &mut rec.occurrences {
                if let Some(l) = occ.lowered.as_mut().filter(|l| l.literal.is_some()) {
                    l.literal = Some("300 (115.38%)".to_owned());
                }
            }
        },
        |_| {},
    );
    let found = text_findings(&r);
    assert_eq!(found.len(), play_frames(&r), "{}", r.text(0, true, true));
    assert!(found.iter().all(|f| *f
        == (
            "text: production and reference differ".to_owned(),
            Some("production `300 (115.38%)`, reference `300 (115.39%)`".to_owned())
        )));
    // The reference's ` (` written `(`.
    let r = tampered(
        &[READOUT],
        &o,
        |_| {},
        |obs| {
            for f in &mut obs.frames {
                for p in &mut f.points {
                    if let Point::Ran {
                        outcome: Outcome::Value(Raw::Text(s)),
                        ..
                    } = p
                    {
                        *s = intern(&s.replace(" (", "("));
                    }
                }
            }
        },
    );
    let found = text_findings(&r);
    assert_eq!(found.len(), play_frames(&r), "{}", r.text(0, true, true));
    assert!(found.iter().all(|f| f.1.as_deref()
        == Some("production `300 (115.39%)`, reference `300(115.39%)`")));
    // Production's facts from another chart, by the flag: 1 000 notes
    // where the frames' chart has 7.
    let args: Vec<String> = ["--notes", "1000", "--script", "full"]
        .map(str::to_owned)
        .to_vec();
    let (mut o, rest) = crate::parse(&args).unwrap();
    assert!(rest.is_empty());
    o.facts = options(Script::full()).facts;
    let r = replayed(&[READOUT], &o);
    let found = text_findings(&r);
    assert_eq!(found.len(), play_frames(&r), "{}", r.text(0, true, true));
    assert!(
        found
            .iter()
            .all(|f| f.1.as_deref() == Some("production `260 (56.42%)`, reference `260 (100%)`"))
    );
    // The total unavailable, by the flag: the read stays, the text is not
    // folded, and nothing is compared.
    let args: Vec<String> = ["--total", "unavailable"].map(str::to_owned).to_vec();
    let (mut o, _) = crate::parse(&args).unwrap();
    o.facts = options(Script::full()).facts;
    assert_eq!(o.import_facts().total, Fact::Unavailable);
    let r = replayed(&[READOUT], &o);
    assert!(r.passes(), "{}", r.text(0, true, true));
    assert!(r.texts.is_empty(), "{}", r.text(0, true, true));
    assert_eq!(
        r.buckets.get(NOT_FOLDED).copied(),
        Some(play_frames(&r)),
        "{}",
        r.text(0, true, true)
    );
}

/// The respelling drops `.0` only from a whole float's digits: a digit,
/// `.0`, then no digit.
#[test]
fn only_whole_floats_are_respelled() {
    use crate::production::compare::respell;
    for (s, want) in [
        ("260.0 (100.0%)", "260 (100%)"),
        ("1.05", "1.05"),
        ("x.0", "x.0"),
        ("-5.0", "-5"),
        ("2.00", "2.00"),
        (".0", ".0"),
        ("0.0", "0"),
        ("", ""),
    ] {
        assert_eq!(respell(s), want, "{s}");
    }
}
