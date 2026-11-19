//! The walk, its records and the comparison (docs/stage3.md §3, the
//! conversion report).

use std::collections::{BTreeMap, VecDeque};
use std::path::Path;

use skin_beatoraja::lua::facts::ImportFacts;
use skin_beatoraja::lua::survey::{Choice, Probe, Survey, survey_with};
use skin_beatoraja::{Cancel, FileSpec, Header, OffsetSpec, OptionItem, OptionSpec};
use skin_model::ScreenKind;

use super::compare::{Verdict, compare};
use super::record::{Record, Records, SkinEntry};
use super::walk::{self, Disk, Source};

/// A 64-pixel white PNG.
fn png() -> Vec<u8> {
    let mut out = std::io::Cursor::new(Vec::new());
    image::RgbaImage::from_pixel(64, 64, image::Rgba([255; 4]))
        .write_to(&mut out, image::ImageFormat::Png)
        .unwrap();
    out.into_inner()
}

/// A folder holding `files` (paths with `/`, folders made).
fn folder(files: &[(&str, &[u8])]) -> tempfile::TempDir {
    let d = tempfile::tempdir().unwrap();
    write(d.path(), files);
    d
}

fn write(root: &Path, files: &[(&str, &[u8])]) {
    for (name, bytes) in files {
        let p = root.join(name);
        std::fs::create_dir_all(p.parent().unwrap()).unwrap();
        std::fs::write(p, bytes).unwrap();
    }
}

/// The walk of `skins` (folder, entry) from disk.
fn walk_of(skins: &[(&Path, &str)]) -> (Records, String) {
    let list: Vec<(String, String)> = skins
        .iter()
        .map(|(d, e)| (d.to_str().unwrap().to_owned(), (*e).to_owned()))
        .collect();
    let entries: Vec<String> = list.iter().map(|(_, e)| e.clone()).collect();
    let (r, summary) = walk::walk(&mut Disk(list), &entries);
    // Every record a walk writes passes its own check.
    r.to_text().unwrap();
    (r, summary)
}

fn outcomes(r: &Records) -> Vec<&str> {
    r.record.iter().map(|r| r.outcome.as_str()).collect()
}

/// The play skin of the walk's first test: options `fc` (two items) and
/// `g` (three named items and one unnamed), an unnamed option, and the
/// file choice `part`, whose candidates `parts/bad.lua` (not Lua) and
/// `parts/good.lua` the configured pass runs.
const W1: &str = "local skin = { type = 0, name = \"t\", w = 1920, h = 1080,\n\
  property = {\n\
    { name = \"fc\", item = { { name = \"off\", op = 905 }, { name = \"on\", op = 906 } }, def = \"off\" },\n\
    { name = \"g\", item = { { name = \"a\", op = 910 }, { name = \"b\", op = 911 }, { op = 912 }, { name = \"c\", op = 913 } } },\n\
    { item = { { name = \"x\", op = 920 } } } },\n\
  filepath = { { name = \"part\", path = \"parts/*.lua\", def = \"good\" } } }\n\
if not skin_config then return skin end\n\
dofile(skin_config.get_path(\"parts/*.lua\"))\n\
skin.destination = { { id = -110, dst = { { w = 64, h = 64 } } } }\n\
return skin\n";

fn w1() -> tempfile::TempDir {
    folder(&[
        ("s.luaskin", W1.as_bytes()),
        ("parts/bad.lua", b"this is not lua ("),
        ("parts/good.lua", b"return 1"),
    ])
}

#[test]
fn the_walk_covers_the_default_and_each_catalogued_row() {
    let d = w1();
    let (r, summary) = walk_of(&[(d.path(), "s.luaskin")]);
    let rows: Vec<(usize, &str, Option<[usize; 2]>)> = r
        .record
        .iter()
        .map(|r| (r.configuration, r.toggle.as_str(), r.position))
        .collect();
    assert_eq!(
        rows,
        [
            (0, "default", None),
            (1, "option", Some([0, 0])),
            (2, "option", Some([0, 1])),
            (3, "option", Some([1, 0])),
            (4, "option", Some([1, 1])),
            (5, "option", Some([1, 3])),
            (6, "file", Some([0, 0])),
            (7, "file", Some([0, 1])),
        ]
    );
    assert_eq!(
        outcomes(&r),
        [
            "imported",
            "imported",
            "imported",
            "imported",
            "imported",
            "imported",
            "lua Syntax",
            "imported"
        ]
    );
    assert_eq!((r.skins, r.records), (1, 8));
    assert!(
        summary.starts_with(
            "skin 0: Play, configurations 8, imported 7, unselectable 0, inconclusive 0, \
             unstable 0\n  configuration 6 (file 0 0): lua Syntax\n"
        ),
        "{summary}"
    );
    assert!(
        summary.contains("walked 8 configurations over 1 skins, 7 imported, "),
        "{summary}"
    );
}

/// A JSON result skin: option `fc` and the file choice `num`, whose
/// candidate `z.png` is not an image and holds the EX score's digits.
fn json_skin() -> tempfile::TempDir {
    let def = br#"{"type": 7, "w": 1920, "h": 1080,
        "property": [{"name": "fc", "item": [{"name": "off", "op": 905}, {"name": "on", "op": 906}]}],
        "filepath": [{"name": "num", "path": "num/*.png", "def": "a"}],
        "source": [{"id": "a", "path": "a.png"}, {"id": "n", "path": "num/*.png"}],
        "image": [{"id": "dot", "src": "a", "x": 0, "y": 0, "w": 8, "h": 8}],
        "value": [{"id": "ex", "src": "n", "x": 0, "y": 0, "w": 40, "h": 8, "divx": 10, "digit": 4, "ref": 71}],
        "destination": [
            {"id": "ex", "dst": [{"x": 0, "y": 100, "w": 4, "h": 8}]},
            {"id": "dot", "op": [90], "dst": [{"x": 0, "y": 300, "w": 8, "h": 8}]}]}"#;
    let png = png();
    folder(&[
        ("s.json", def),
        ("a.png", &png),
        ("num/a.png", &png),
        ("num/z.png", b"not an image"),
    ])
}

#[test]
fn the_catalogue_lists_a_json_skins_rows() {
    let d = json_skin();
    let root = d.path().to_str().unwrap().to_owned();
    let out = super::catalogue(&[root, "s.json".to_owned()]);
    let rows: Vec<&str> = out.lines().skip(1).collect();
    assert_eq!(
        rows,
        [
            "option\tfc\toff",
            "option\tfc\ton",
            "file\tnum\ta.png",
            "file\tnum\tz.png"
        ]
    );
}

#[test]
fn the_walk_walks_a_json_skins_rows() {
    let d = json_skin();
    let (r, summary) = walk_of(&[(d.path(), "s.json")]);
    assert_eq!(r.skin[0].kind, "Result");
    let got: Vec<(&str, Option<[usize; 2]>, bool)> = r
        .record
        .iter()
        .map(|r| (r.toggle.as_str(), r.position, r.outcome == "imported"))
        .collect();
    assert_eq!(
        got,
        [
            ("default", None, true),
            ("option", Some([0, 0]), true),
            ("option", Some([0, 1]), true),
            ("file", Some([0, 0]), true),
            ("file", Some([0, 1]), false),
        ],
        "{summary}"
    );
    // Drawn over the dense view: a result skin's frames are recorded.
    assert!(r.record[0].frames.is_some());
}

#[test]
fn a_refused_header_is_one_record() {
    let d = folder(&[(
        "s.luaskin",
        b"return { type = 1, name = \"t\", w = 1920, h = 1080 }",
    )]);
    let (r, summary) = walk_of(&[(d.path(), "s.luaskin")]);
    assert_eq!(outcomes(&r), ["header unsupported screen kind"]);
    assert_eq!(r.skin[0].kind, "none");
    assert!(
        summary.starts_with("skin 0: none, configurations 1, imported 0,"),
        "{summary}"
    );
    // A stable header refusal compares equal.
    let (again, _) = walk_of(&[(d.path(), "s.luaskin")]);
    let v = compare(&r, &again).unwrap();
    assert_eq!((v.code(), v.unchanged), (0, 1), "{}", v.text);
}

#[test]
fn the_walk_imports_under_the_decided_facts() {
    // Autoplay (option 32), the judge class (option 182) and the play
    // level (number 96) read while the skin loads: each unavailable
    // without its fact, which stops the import.
    let play = folder(&[(
        "s.luaskin",
        b"local skin = { type = 0, name = \"t\", w = 1920, h = 1080 }\n\
          if not skin_config then return skin end\n\
          local ms = require(\"main_state\")\n\
          local a, c, l = ms.option(32), ms.option(182), ms.number(96)\n\
          skin.destination = { { id = -110, dst = { { w = 64, h = 64 } } } }\n\
          return skin\n",
    )]);
    let (r, _) = walk_of(&[(play.path(), "s.luaskin")]);
    assert_eq!(outcomes(&r), ["imported"]);
    assert_eq!(
        r.record[0].reads,
        [
            "Autoplay = Bool(false)",
            "JudgeClass = Class(Some(Normal))",
            "PlayLevel = Level(12)"
        ]
    );
    // A result skin reading the best's BP difference while it loads
    // imports over the dense view; the plain facts stop it.
    let text = "local skin = { type = 7, name = \"t\", w = 1920, h = 1080 }\n\
                if not skin_config then return skin end\n\
                local ms = require(\"main_state\")\n\
                local gap = ms.number(178) < 0 and 0 or 10\n\
                skin.source = { { id = \"a\", path = \"a.png\" } }\n\
                skin.image = { { id = \"dot\", src = \"a\", x = 0, y = 0, w = 8, h = 8 } }\n\
                skin.value = { { id = \"ex\", src = \"a\", x = 0, y = 0, w = 40, h = 8, divx = 10, \
                    digit = 4, ref = 71 } }\n\
                skin.destination = {\n\
                  { id = \"ex\", dst = { { x = 0, y = 100, w = 4, h = 8 } } },\n\
                  { id = \"dot\", op = { 90 }, dst = { { x = gap, y = 300, w = 8, h = 8 } } } }\n\
                return skin\n";
    let result = folder(&[("s.luaskin", text.as_bytes()), ("a.png", &png())]);
    let (r, _) = walk_of(&[(result.path(), "s.luaskin")]);
    assert_eq!(outcomes(&r), ["imported"]);
    let plain = survey_with(
        result.path(),
        "s.luaskin",
        &ImportFacts::unavailable(ScreenKind::Result),
        &Cancel::new(),
        false,
        &Choice::default(),
    );
    assert_ne!(plain.outcome, "imported");
}

/// A result skin with two runtime callbacks: a number's value and a
/// draw.
const RUNTIME: &str = "local skin = { type = 7, name = \"t\", w = 1920, h = 1080 }\n\
    if not skin_config then return skin end\n\
    local ms = require(\"main_state\")\n\
    local k = 0\n\
    skin.source = { { id = \"a\", path = \"a.png\" } }\n\
    skin.image = { { id = \"dot\", src = \"a\", x = 0, y = 0, w = 8, h = 8 } }\n\
    skin.value = { { id = \"ex\", src = \"a\", x = 0, y = 0, w = 40, h = 8, divx = 10, digit = 4, ref = 71 },\n\
      { id = \"r\", src = \"a\", x = 0, y = 0, w = 40, h = 8, divx = 10, digit = 4,\n\
        value = function() k = k + 1 return ms.number(71) end } }\n\
    skin.destination = {\n\
      { id = \"ex\", dst = { { x = 0, y = 100, w = 4, h = 8 } } },\n\
      { id = \"r\", dst = { { x = 0, y = 120, w = 4, h = 8 } } },\n\
      { id = \"dot\", draw = function() k = k + 1 return true end, dst = { { x = 0, y = 140, w = 8, h = 8 } } },\n\
      { id = \"dot\", op = { 90 }, dst = { { x = 0, y = 300, w = 8, h = 8 } } } }\n\
    return skin\n";

#[test]
fn the_walk_record_is_not_the_clocks() {
    let d = folder(&[("s.luaskin", RUNTIME.as_bytes()), ("a.png", &png())]);
    // The control: with no frame time, the frame's calls run out of it.
    skin_beatoraja::vm_hooks::set_frame_time(Some(std::time::Duration::ZERO));
    let facts = ImportFacts::unavailable(ScreenKind::Result);
    let s = super::survey_entry(
        d.path().to_str().unwrap(),
        "s.luaskin",
        &facts,
        &Cancel::new(),
        false,
        &Choice::default(),
    );
    let status = &s.runtime.unwrap().status;
    assert!(status.contains_key("budget"), "{status:?}");
    // The walk takes the clock out of the frame.
    skin_beatoraja::vm_hooks::set_frame_time(Some(std::time::Duration::ZERO));
    let (r, _) = walk_of(&[(d.path(), "s.luaskin")]);
    let rt = r.record[0].runtime.as_ref().unwrap();
    assert_eq!(rt.outputs, 2);
    assert_eq!(rt.status, BTreeMap::from([("ok".to_owned(), 2)]));
}

/// Options and file choices whose names repeat.
const REPEATS: &str = "local skin = { type = 0, name = \"t\", w = 1920, h = 1080,\n\
  property = {\n\
    { name = \"fc\", item = { { name = \"off\", op = 905 }, { name = \"on\", op = 906 }, { name = \"on\", op = 906 }, { name = \"on\", op = 907 } } },\n\
    { name = \"fc\", item = { { name = \"z\", op = 908 } } } },\n\
  filepath = { { name = \"p\", path = \"a/*.lua\", def = \"x\" }, { name = \"p\", path = \"b/*.lua\", def = \"x\" } } }\n\
if not skin_config then return skin end\n\
skin.destination = { { id = -110, dst = { { w = 64, h = 64 } } } }\n\
return skin\n";

fn repeats() -> tempfile::TempDir {
    folder(&[
        ("s.luaskin", REPEATS.as_bytes()),
        ("a/x.lua", b"return 1"),
        ("a/y.lua", b"return 1"),
        ("b/w.lua", b"return 1"),
        ("b/x.lua", b"return 1"),
    ])
}

#[test]
fn unselectable_rows_are_recorded_not_imported() {
    let d = repeats();
    let (r, summary) = walk_of(&[(d.path(), "s.luaskin")]);
    // Rows: fc's off, on, on (same op), on (another op); the second fc's
    // z; p's x.lua, y.lua; the second p's w.lua (not among the first p's
    // candidates, so the selector cannot name it) and x.lua (which sets
    // both p).
    assert_eq!(
        outcomes(&r),
        [
            "imported",
            "imported",
            "imported",
            "unselectable",
            "unselectable",
            "unselectable",
            "imported",
            "imported",
            "unselectable",
            "imported"
        ],
        "{summary}"
    );
    assert!(summary.contains("unselectable 4,"), "{summary}");
    let text = r.to_text().unwrap();
    assert_eq!(Records::read("new", &text).unwrap(), r);
}

#[test]
fn a_shared_file_selection_sets_every_choice_of_its_name() {
    let d = repeats();
    let picks = |candidate: &str| {
        let choice = Choice {
            files: vec![("p".to_owned(), candidate.to_owned())],
            ..Choice::default()
        };
        let s = survey_with(
            d.path(),
            "s.luaskin",
            &ImportFacts::unavailable(ScreenKind::Play),
            &Cancel::new(),
            true,
            &choice,
        );
        s.resolved.unwrap().files
    };
    let p = |c: &str| (Some("p".to_owned()), Some(c.to_owned()));
    // In both lists: both choices take it.
    assert_eq!(picks("x.lua"), [p("x.lua"), p("x.lua")]);
    // In the first list only: the second falls back to its default.
    assert_eq!(picks("y.lua"), [p("y.lua"), p("x.lua")]);
}

/// A change to a header, by what it changes.
type Change = (&'static str, Box<dyn Fn(&mut Header)>);

fn header() -> Header {
    Header {
        kind: ScreenKind::Play,
        name: Some("n".to_owned()),
        author: Some("a".to_owned()),
        size: [1920, 1080],
        options: vec![
            OptionSpec {
                name: Some("ab".to_owned()),
                items: vec![
                    OptionItem {
                        name: Some("x".to_owned()),
                        op: 905,
                    },
                    OptionItem {
                        name: Some("y".to_owned()),
                        op: 906,
                    },
                ],
                def: Some("c".to_owned()),
            },
            OptionSpec {
                name: Some("o".to_owned()),
                items: Vec::new(),
                def: None,
            },
        ],
        files: vec![FileSpec {
            name: Some("f".to_owned()),
            path: Some("p/*.png".to_owned()),
            def: Some("d".to_owned()),
            candidates: vec!["a.png".to_owned(), "b.png".to_owned()],
        }],
        offsets: vec![OffsetSpec {
            name: Some("off".to_owned()),
            id: 1,
            x: true,
            y: false,
            w: false,
            h: false,
            r: false,
            a: false,
        }],
    }
}

#[test]
fn the_digest_covers_the_whole_header_and_the_entry() {
    let base = walk::digest(&header(), "s.luaskin");
    assert_eq!(base, walk::digest(&header(), "s.luaskin"));
    assert_ne!(base, walk::digest(&header(), "t.luaskin"));
    let changes: Vec<Change> = vec![
        ("kind", Box::new(|h| h.kind = ScreenKind::Result)),
        ("size", Box::new(|h| h.size = [1280, 720])),
        ("name", Box::new(|h| h.name = None)),
        ("author", Box::new(|h| h.author = Some("b".to_owned()))),
        (
            "option name",
            Box::new(|h| h.options[0].name = Some("ac".to_owned())),
        ),
        ("option default", Box::new(|h| h.options[0].def = None)),
        (
            "item name",
            Box::new(|h| h.options[0].items[0].name = Some("z".to_owned())),
        ),
        ("item op", Box::new(|h| h.options[0].items[1].op = 907)),
        (
            "file name",
            Box::new(|h| h.files[0].name = Some("g".to_owned())),
        ),
        (
            "file path",
            Box::new(|h| h.files[0].path = Some("q/*.png".to_owned())),
        ),
        (
            "file default",
            Box::new(|h| h.files[0].def = Some("e".to_owned())),
        ),
        (
            "candidate",
            Box::new(|h| h.files[0].candidates[0] = "c.png".to_owned()),
        ),
        ("offset name", Box::new(|h| h.offsets[0].name = None)),
        ("offset id", Box::new(|h| h.offsets[0].id = 2)),
        ("offset flag", Box::new(|h| h.offsets[0].a = true)),
        // Encoding boundaries: a string moved across two fields, an
        // empty name against none, an item moved to the next list, and
        // two candidates whose concatenation is the same.
        (
            "candidate boundary",
            Box::new(|h| {
                h.files[0].candidates = vec!["a.p".to_owned(), "ngb.png".to_owned()];
            }),
        ),
        (
            "boundary",
            Box::new(|h| {
                h.options[0].name = Some("a".to_owned());
                h.options[0].def = Some("bc".to_owned());
            }),
        ),
        (
            "empty",
            Box::new(|h| h.options[1].name = Some(String::new())),
        ),
        (
            "moved item",
            Box::new(|h| {
                let i = h.options[0].items.pop().unwrap();
                h.options[1].items.push(i);
            }),
        ),
    ];
    for (what, change) in changes {
        let mut h = header();
        change(&mut h);
        assert_ne!(walk::digest(&h, "s.luaskin"), base, "{what}");
    }
}

/// What a scripted header read gives.
enum Read {
    /// The skin's own header, from disk.
    Disk,
    /// Another folder's header (the skin read differently).
    Other(String),
    /// A refusal's survey.
    Refused(Box<Survey>),
}

/// A source whose reads and surveys follow a script, then the disk's.
struct Script {
    disk: Disk,
    reads: VecDeque<Read>,
    surveys: VecDeque<Survey>,
    probes: usize,
    calls: usize,
}

impl Script {
    fn new(root: &Path, reads: Vec<Read>, surveys: Vec<Survey>) -> Self {
        Self {
            disk: Disk(vec![(
                root.to_str().unwrap().to_owned(),
                "s.luaskin".to_owned(),
            )]),
            reads: reads.into(),
            surveys: surveys.into(),
            probes: 0,
            calls: 0,
        }
    }
}

impl Source for Script {
    fn probe(&mut self, skin: usize) -> Result<Probe, Box<Survey>> {
        self.probes += 1;
        match self.reads.pop_front() {
            None | Some(Read::Disk) => self.disk.probe(skin),
            Some(Read::Other(root)) => Disk(vec![(root, "s.luaskin".to_owned())]).probe(0),
            Some(Read::Refused(s)) => Err(s),
        }
    }

    fn survey(&mut self, probe: &mut Probe, facts: &ImportFacts, choice: &Choice) -> Survey {
        self.calls += 1;
        match self.surveys.pop_front() {
            Some(s) => s,
            None => self.disk.survey(probe, facts, choice),
        }
    }
}

fn timeout() -> Survey {
    Survey {
        outcome: "lua Limit(Time)".to_owned(),
        timed_out: true,
        ..Survey::default()
    }
}

fn refused(label: &str) -> Survey {
    Survey {
        outcome: label.to_owned(),
        ..Survey::default()
    }
}

/// A play skin with no rows.
fn plain() -> tempfile::TempDir {
    folder(&[(
        "s.luaskin",
        b"local skin = { type = 0, name = \"t\", w = 1920, h = 1080 }\n\
          if not skin_config then return skin end\n\
          skin.destination = { { id = -110, dst = { { w = 64, h = 64 } } } }\n\
          return skin\n",
    )])
}

fn scripted(s: &mut Script) -> Records {
    walk::walk(s, &["s.luaskin".to_owned()]).0
}

#[test]
fn an_import_is_attempted_once() {
    let d = plain();
    let mut s = Script::new(d.path(), vec![], vec![]);
    assert_eq!(outcomes(&scripted(&mut s)), ["imported"]);
    assert_eq!((s.probes, s.calls), (1, 1));
}

#[test]
fn a_refusal_is_attempted_again() {
    let d = plain();
    let mut s = Script::new(
        d.path(),
        vec![],
        vec![refused("lua Error"), refused("lua Error")],
    );
    let r = scripted(&mut s);
    assert_eq!(outcomes(&r), ["lua Error"]);
    assert!(r.record[0].attempts.is_empty());
    assert_eq!((s.probes, s.calls), (2, 2));
}

#[test]
fn a_timeout_then_a_result_records_the_result() {
    let d = plain();
    let mut s = Script::new(d.path(), vec![], vec![timeout()]);
    assert_eq!(outcomes(&scripted(&mut s)), ["imported"]);
    assert_eq!(s.calls, 2);
}

#[test]
fn two_timeouts_are_inconclusive() {
    let d = plain();
    let mut s = Script::new(d.path(), vec![], vec![timeout(), timeout()]);
    let r = scripted(&mut s);
    assert_eq!(outcomes(&r), ["inconclusive"]);
    assert_eq!(r.record[0].attempts, ["lua Limit(Time)", "lua Limit(Time)"]);
    assert_eq!(s.calls, 2);
}

#[test]
fn attempts_that_disagree_are_unstable() {
    let d = plain();
    // A refusal, then an import.
    let mut s = Script::new(d.path(), vec![], vec![refused("lua Error")]);
    let r = scripted(&mut s);
    assert_eq!(outcomes(&r), ["unstable"]);
    assert_eq!(r.record[0].attempts, ["lua Error", "imported"]);
    // Two refusals with other labels.
    let mut s = Script::new(
        d.path(),
        vec![],
        vec![refused("lua Error"), refused("lua Syntax")],
    );
    assert_eq!(outcomes(&scripted(&mut s)), ["unstable"]);
    // The same label over another header.
    let other = w1();
    let mut s = Script::new(
        d.path(),
        vec![
            Read::Disk,
            Read::Other(other.path().to_str().unwrap().to_owned()),
        ],
        vec![refused("lua Error"), refused("lua Error")],
    );
    assert_eq!(outcomes(&scripted(&mut s)), ["unstable"]);
}

#[test]
fn a_probe_timeout_then_a_header_walks_the_skin() {
    let d = w1();
    let mut s = Script::new(d.path(), vec![Read::Refused(Box::new(timeout()))], vec![]);
    let r = scripted(&mut s);
    assert_eq!(r.records, 8);
    assert_eq!(r.skin[0].kind, "Play");
}

#[test]
fn two_probe_timeouts_are_an_inconclusive_skin() {
    let d = w1();
    let mut s = Script::new(
        d.path(),
        vec![
            Read::Refused(Box::new(timeout())),
            Read::Refused(Box::new(timeout())),
        ],
        vec![],
    );
    let r = scripted(&mut s);
    assert_eq!(outcomes(&r), ["inconclusive"]);
    assert_eq!(s.probes, 2);
}

#[test]
fn the_walk_refuses_to_write_a_string_outside_the_vocabulary() {
    let d = plain();
    let records = d.path().join("records.toml");
    let mut s = Script::new(
        d.path(),
        vec![],
        vec![refused("QZX marker"), refused("QZX marker")],
    );
    let done = super::walk_mode(records.to_str().unwrap(), &mut s, &["s.luaskin".to_owned()]);
    assert_eq!(done.code, 2);
    assert!(!records.exists());
    assert!(
        done.err
            .contains("skin 0 configuration 0: its outcome holds a string outside"),
        "{}",
        done.err
    );
    assert!(!done.err.contains("QZX") && !done.out.contains("QZX"));
}

/// A record of skin `skin`, configuration `k`, with `outcome`.
fn rec(skin: usize, k: usize, outcome: &str) -> Record {
    Record {
        skin,
        configuration: k,
        toggle: if k == 0 { "default" } else { "option" }.to_owned(),
        position: (k > 0).then(|| [0, k - 1]),
        kind: "Play".to_owned(),
        outcome: outcome.to_owned(),
        ..Record::default()
    }
}

/// Records of skins, each a digest and its records' outcomes.
fn run(skins: &[(&str, &[&str])]) -> Records {
    let mut r = Records::new(walk::FACTS, walk::RESULT_FACTS);
    for (i, (digest, list)) in skins.iter().enumerate() {
        let records: Vec<Record> = list.iter().enumerate().map(|(k, o)| rec(i, k, o)).collect();
        r.push_skin(
            SkinEntry {
                index: i,
                digest: (*digest).to_owned(),
                kind: "Play".to_owned(),
                configurations: records.len(),
            },
            records,
        );
    }
    r
}

fn verdict(kept: &Records, new: &Records) -> Verdict {
    compare(kept, new).unwrap()
}

#[test]
fn records_round_trip() {
    let d = w1();
    let (r, _) = walk_of(&[(d.path(), "s.luaskin")]);
    let text = r.to_text().unwrap();
    assert_eq!(Records::read("kept", &text).unwrap(), r);
}

#[test]
fn another_record_version_is_refused() {
    let text = run(&[("d", &["imported"])]).to_text().unwrap();
    for v in ["0", "2"] {
        let other = text.replacen("version = 1", &format!("version = {v}"), 1);
        let e = Records::read("kept", &other).unwrap_err();
        assert!(e.starts_with("kept: records of another version"), "{e}");
    }
}

#[test]
fn a_missing_field_is_refused() {
    let text = run(&[("d", &["imported"])]).to_text().unwrap();
    let missing = text.replacen("modules = 0\n", "", 1);
    let e = Records::read("new", &missing).unwrap_err();
    assert!(e.starts_with("new: line "), "{e}");
}

#[test]
fn an_unknown_field_is_refused_by_its_table_and_line() {
    let text = run(&[("d", &["imported"])]).to_text().unwrap();
    let at = text.find("[[record]]").unwrap();
    let line = text[..at].matches('\n').count() + 2;
    let unknown = text.replacen("[[record]]\n", "[[record]]\nextra = 1\n", 1);
    let e = Records::read("new", &unknown).unwrap_err();
    assert!(e.contains("in [[record]]"), "{e}");
    assert!(
        e.starts_with(&format!("new: line {line}")) || e.starts_with("new: line "),
        "{e}"
    );
}

#[test]
fn a_truncated_records_file_is_refused() {
    let text = run(&[("d", &["imported", "imported", "imported"])])
        .to_text()
        .unwrap();
    // Cut after the second complete record.
    let third = text.match_indices("[[record]]").nth(2).unwrap().0;
    let e = Records::read("kept", &text[..third]).unwrap_err();
    assert_eq!(e, "kept: declares 3 records and holds 2");
}

#[test]
fn a_repeated_configuration_is_refused() {
    let mut r = run(&[("d", &["imported", "imported"])]);
    r.record[1].configuration = 0;
    let e = Records::read("new", &r.to_text().unwrap()).unwrap_err();
    assert_eq!(
        e,
        "new: skin 0's configurations are not numbered 0 to 1 once each"
    );
}

#[test]
fn the_same_records_compare_unchanged() {
    let r = run(&[("d", &["imported", "lua Error"]), ("e", &["imported"])]);
    let v = verdict(&r, &r);
    assert_eq!((v.code(), v.unchanged, v.changed), (0, 3, 0), "{}", v.text);
    assert!(
        v.text.ends_with(
            "compared 3 configurations over 2 skins: 0 new failures, 0 not judged \
             (inconclusive or unstable), 0 skins to re-baseline, 0 changed, 3 unchanged\n"
        ),
        "{}",
        v.text
    );
}

#[test]
fn a_configuration_that_stops_importing_fails() {
    let kept = run(&[("d", &["imported", "imported"])]);
    let new = run(&[("d", &["imported", "critical asset"])]);
    let v = verdict(&kept, &new);
    assert_eq!((v.code(), v.failures), (1, 1));
    assert!(
        v.text.contains(
            "skin 0 configuration 1 (option 0 0): outcome imported -> critical asset: new failure\n"
        ),
        "{}",
        v.text
    );
    // A panic is a refusal too.
    let new = run(&[("d", &["imported", "panic"])]);
    assert_eq!(verdict(&kept, &new).code(), 1);
}

#[test]
fn a_changed_refusal_reason_fails() {
    let kept = run(&[("d", &["lua Error"])]);
    let new = run(&[("d", &["lua Syntax"])]);
    let v = verdict(&kept, &new);
    assert_eq!(v.code(), 1);
    assert!(
        v.text.contains(
            "skin 0 configuration 0 (default): outcome lua Error -> lua Syntax: new failure"
        ),
        "{}",
        v.text
    );
}

#[test]
fn refused_to_imported_is_reported_not_failed() {
    let v = verdict(
        &run(&[("d", &["lua Error"])]),
        &run(&[("d", &["imported"])]),
    );
    assert_eq!((v.code(), v.changed), (0, 1));
    assert!(
        v.text.contains("outcome lua Error -> imported\n"),
        "{}",
        v.text
    );
}

#[test]
fn a_moved_refusal_with_its_label_is_reported_not_failed() {
    let kept = run(&[("d", &["lua Error"])]);
    let mut new = kept.clone();
    new.record[0].at = Some([1, 7, 3]);
    let v = verdict(&kept, &new);
    assert_eq!((v.code(), v.changed), (0, 1));
    assert!(v.text.contains("at - -> [1, 7, 3]"), "{}", v.text);
}

#[test]
fn counts_and_stops_are_reported_not_failed() {
    let kept = run(&[("d", &["imported"])]);
    let mut new = kept.clone();
    new.record[0].notes.insert("Ignored op".to_owned(), 1);
    new.record[0].callbacks.insert("lowered".to_owned(), 1);
    new.record[0].runtime = Some(super::record::Runtime {
        stops: vec!["budget".to_owned()],
        stopped: Some("budget".to_owned()),
        ..super::record::Runtime::default()
    });
    let v = verdict(&kept, &new);
    assert_eq!((v.code(), v.failures, v.changed), (0, 0, 1), "{}", v.text);
    for line in [
        "notes Ignored op - -> 1",
        "callbacks lowered - -> 1",
        "runtime stopped - -> budget",
    ] {
        assert!(v.text.contains(line), "{line}: {}", v.text);
    }
}

#[test]
fn a_changed_catalogue_is_rebaselined() {
    let kept = run(&[("d", &["imported", "imported"]), ("e", &["imported"])]);
    // The first skin's catalogue changed, and its configuration 1 went
    // from imported to refused: not compared.
    let new = run(&[("D", &["imported", "lua Error"]), ("e", &["imported"])]);
    let v = verdict(&kept, &new);
    assert_eq!(
        (v.code(), v.failures, v.rebaseline),
        (2, 0, 1),
        "{}",
        v.text
    );
    assert!(
        v.text
            .starts_with("skin 0: catalogue changed, re-baseline\n"),
        "{}",
        v.text
    );
    // A real catalogue change: the digest of a header whose candidate or
    // path changed differs (`the_digest_covers_the_whole_header_and_the_entry`).
}

#[test]
fn a_missing_or_added_skin_is_rebaselined() {
    let one = run(&[("d", &["imported"])]);
    let two = run(&[("d", &["imported"]), ("e", &["imported"])]);
    let v = verdict(&two, &one);
    assert_eq!(v.code(), 2);
    assert!(
        v.text.contains("skin 1: missing from new, re-baseline"),
        "{}",
        v.text
    );
    let v = verdict(&one, &two);
    assert!(
        v.text.contains("skin 1: added in new, re-baseline"),
        "{}",
        v.text
    );
}

#[test]
fn reordered_skins_are_rebaselined() {
    let kept = run(&[("d", &["imported"]), ("e", &["imported"])]);
    let new = run(&[("e", &["imported"]), ("d", &["imported"])]);
    let v = verdict(&kept, &new);
    assert_eq!((v.code(), v.rebaseline), (2, 2), "{}", v.text);
}

#[test]
fn equal_digests_with_other_configurations_are_rebaselined() {
    let kept = run(&[("d", &["imported", "imported"])]);
    let fewer = run(&[("d", &["imported"])]);
    let v = verdict(&kept, &fewer);
    assert_eq!(v.code(), 2);
    assert!(
        v.text
            .contains("skin 0: configurations differ, re-baseline"),
        "{}",
        v.text
    );
    let mut moved = kept.clone();
    moved.record[1].position = Some([1, 0]);
    let v = verdict(&kept, &moved);
    assert_eq!((v.code(), v.rebaseline), (2, 1), "{}", v.text);
}

#[test]
fn a_changed_header_refusal_is_rebaselined() {
    let d = folder(&[(
        "s.luaskin",
        b"return { type = 1, name = \"t\", w = 1920, h = 1080 }",
    )]);
    let (kept, _) = walk_of(&[(d.path(), "s.luaskin")]);
    std::fs::write(d.path().join("s.luaskin"), "this is not lua (").unwrap();
    let (new, _) = walk_of(&[(d.path(), "s.luaskin")]);
    let v = verdict(&kept, &new);
    assert_eq!((v.code(), v.rebaseline), (2, 1), "{}", v.text);
}

#[test]
fn runs_under_other_facts_cannot_be_judged() {
    let kept = run(&[("d", &["imported"])]);
    let mut new = kept.clone();
    new.facts = "--autoplay on".to_owned();
    assert!(compare(&kept, &new).is_err());
}

#[test]
fn inconclusive_or_unstable_records_exit_2() {
    let imported = run(&[("d", &["imported"])]);
    for other in ["inconclusive", "unstable"] {
        let unjudged = run(&[("d", &[other])]);
        for (k, n) in [(&imported, &unjudged), (&unjudged, &imported)] {
            let v = verdict(k, n);
            assert_eq!((v.code(), v.unjudged, v.failures), (2, 1, 0), "{}", v.text);
            assert!(v.text.contains(": not judged\n"), "{}", v.text);
        }
    }
}

#[test]
fn a_new_failure_beside_an_unjudged_skin_exits_1() {
    let kept = run(&[("d", &["imported", "imported"]), ("e", &["imported"])]);
    let new = run(&[("d", &["inconclusive", "lua Error"]), ("E", &["imported"])]);
    let v = verdict(&kept, &new);
    assert_eq!(
        (v.failures, v.unjudged, v.rebaseline),
        (1, 1, 1),
        "{}",
        v.text
    );
    assert_eq!(v.code(), 1);
}

#[test]
fn catalogue_names_are_escaped() {
    assert_eq!(super::escaped("a\tb\nc\rd\\e"), "a\\tb\\nc\\rd\\\\e");
    let d = folder(&[(
        "s.luaskin",
        b"return { type = 0, name = \"t\", w = 1920, h = 1080,\n\
          property = { { name = \"o\\tp\", item = { { name = \"i\\nt\", op = 905 } } } } }",
    )]);
    let root = d.path().to_str().unwrap().to_owned();
    let out = super::catalogue(&[root, "s.luaskin".to_owned()]);
    assert_eq!(out.lines().nth(1), Some("option\to\\tp\ti\\nt"));
}

/// A writer the tests read back.
#[derive(Clone, Default)]
struct Shared(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Shared {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// The disk's skins, the first survey panicking with a marker.
struct Panicking {
    disk: Disk,
    panicked: bool,
}

impl Source for Panicking {
    fn probe(&mut self, skin: usize) -> Result<Probe, Box<Survey>> {
        self.disk.probe(skin)
    }

    fn survey(&mut self, probe: &mut Probe, facts: &ImportFacts, choice: &Choice) -> Survey {
        if !self.panicked {
            self.panicked = true;
            panic!("QZXpanic");
        }
        self.disk.survey(probe, facts, choice)
    }
}

/// The privacy fixture's entry: every name it gives, its strings, an
/// unknown field, an `id` and a Lua error carry the marker `QZX`. Its
/// option item `QZXother` makes its module raise; its file choice's
/// candidate `QZXbad.lua` is not Lua.
const MARKED: &str = "local skin = { type = 0, name = \"QZXname\", w = 1920, h = 1080, QZXfield = \"QZXvalue\",\n\
  property = { { name = \"QZXoption\", item = { { name = \"QZXitem\", op = 905 }, { name = \"QZXother\", op = 906 } } } },\n\
  filepath = { { name = \"QZXchoice\", path = \"QZXdir/*.lua\", def = \"QZXgood\" } } }\n\
if not skin_config then return skin end\n\
local m = require(\"QZXmodule\")\n\
dofile(skin_config.get_path(\"QZXdir/*.lua\"))\n\
skin.source = { { id = \"QZXsrc\", path = \"a.png\" } }\n\
skin.image = { { id = \"QZXid\", src = \"QZXsrc\", x = 0, y = 0, w = 8, h = 8 } }\n\
skin.destination = { { id = -110, dst = { { w = 64, h = 64 } } },\n\
  { id = \"QZXid\", draw = m.f, dst = { { w = 8, h = 8 } } } }\n\
return skin\n";

/// The walk's records, its summary, every comparison's output and error
/// text, and its panic hook's output hold nothing from a skin: no name,
/// path, string, `id` or Lua error (each marked `QZX`), and no digest,
/// though the records hold each skin's digest. The positive controls
/// show the markers reach the importer: the catalogue prints the option,
/// item, file-choice and candidate names; the `--local` report the
/// module's file name and the `id`; the raw import error the entry's
/// name; the panic's payload its message. The skin's own name, its
/// unknown field and its string values, and the Lua error's message,
/// reach no output of the report at all, so no channel controls them.
#[test]
fn walk_and_compare_output_hold_nothing_from_the_skin() {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("QZXfolder");
    let png = png();
    write(
        &root,
        &[
            ("QZXentry.luaskin", MARKED.as_bytes()),
            (
                "QZXmodule.lua",
                b"if skin_config.option[\"QZXoption\"] == 906 then error(\"QZXerror\") end\n\
                  return { f = function() return true end }",
            ),
            ("QZXdir/QZXgood.lua", b"return 1"),
            ("QZXdir/QZXbad.lua", b"QZX is not lua ("),
            ("a.png", &png),
            (
                "QZXrefused.luaskin",
                b"return { type = 1, name = \"QZXname2\", w = 1920, h = 1080 }",
            ),
        ],
    );
    let runs = base.path().join("QZXruns");
    std::fs::create_dir_all(&runs).unwrap();
    let kept = runs.join("QZXkept.toml");
    let new = runs.join("QZXnew.toml");
    let r = root.to_str().unwrap().to_owned();
    let skins = vec![
        (r.clone(), "QZXentry.luaskin".to_owned()),
        (r.clone(), "QZXrefused.luaskin".to_owned()),
    ];
    let entries: Vec<String> = skins.iter().map(|(_, e)| e.clone()).collect();
    let hook = Shared::default();
    super::quiet_panics(hook.clone());
    let mut src = Panicking {
        disk: Disk(skins.clone()),
        panicked: false,
    };
    let first = super::walk_mode(kept.to_str().unwrap(), &mut src, &entries);
    let _ = std::panic::take_hook();
    assert_eq!(first.code, 0, "{}", first.err);
    let hooked = String::from_utf8(hook.0.lock().unwrap().clone()).unwrap();
    assert_eq!(hooked, "panic\n");
    // A refusal located in a module, the file choice's refusal and the
    // panic's configuration, attempted again.
    let kept_records = Records::read("kept", &std::fs::read_to_string(&kept).unwrap()).unwrap();
    let skin0: Vec<&Record> = kept_records.record.iter().filter(|r| r.skin == 0).collect();
    assert!(
        skin0
            .iter()
            .any(|r| r.outcome == "lua Error" && r.at.is_some()),
        "{}",
        first.out
    );
    assert!(
        skin0.iter().any(|r| r.outcome == "lua Syntax"),
        "{}",
        first.out
    );
    assert!(
        skin0.iter().any(|r| r.outcome == "unstable"),
        "{}",
        first.out
    );
    // A dependency damaged: the default stops importing.
    write(&root, &[("QZXdir/QZXgood.lua", b"QZX is not lua (")]);
    let second = super::walk_mode(new.to_str().unwrap(), &mut Disk(skins), &entries);
    assert_eq!(second.code, 0, "{}", second.err);
    let compared = super::compare_mode(kept.to_str().unwrap(), new.to_str().unwrap());
    assert_eq!(compared.code, 1, "{}", compared.out);
    assert!(compared.out.contains(": new failure\n"), "{}", compared.out);
    let missing = super::compare_mode(
        runs.join("QZXmissing.toml").to_str().unwrap(),
        new.to_str().unwrap(),
    );
    assert_eq!(missing.code, 2);
    let malformed = runs.join("QZXmalformed.toml");
    std::fs::write(&malformed, "version = 1\nQZXbad = = 1\n").unwrap();
    let bad = super::compare_mode(malformed.to_str().unwrap(), new.to_str().unwrap());
    assert_eq!(bad.code, 2);
    assert!(bad.err.starts_with("kept: line 2: "), "{}", bad.err);
    let records = [
        std::fs::read_to_string(&kept).unwrap(),
        std::fs::read_to_string(&new).unwrap(),
    ];
    let digests: Vec<String> = kept_records.skin.iter().map(|s| s.digest.clone()).collect();
    assert_eq!(digests.len(), 2);
    for text in &records {
        assert!(!text.contains("QZX"), "{text}");
        for d in &digests {
            assert!(text.contains(&format!("digest = \"{d}\"")));
        }
    }
    let printed = [
        &first.out,
        &first.err,
        &second.out,
        &second.err,
        &compared.out,
        &compared.err,
        &missing.out,
        &missing.err,
        &bad.out,
        &bad.err,
        &hooked,
    ];
    for p in printed {
        assert!(!p.contains("QZX"), "{p}");
        for d in &digests {
            assert!(!p.contains(d.as_str()), "{p}");
        }
    }
    // The positive controls.
    write(&root, &[("QZXdir/QZXgood.lua", b"return 1")]);
    let catalogue = super::catalogue(&[r.clone(), "QZXentry.luaskin".to_owned()]);
    for name in [
        "QZXoption",
        "QZXitem",
        "QZXother",
        "QZXchoice",
        "QZXgood.lua",
        "QZXbad.lua",
    ] {
        assert!(catalogue.contains(name), "{name}: {catalogue}");
    }
    let local = survey_with(
        &root,
        "QZXentry.luaskin",
        &ImportFacts::unavailable(ScreenKind::Play),
        &Cancel::new(),
        true,
        &Choice::default(),
    );
    let local = super::report(0, &local);
    for name in ["QZXmodule.lua", "QZXid"] {
        assert!(local.contains(name), "{name}: {local}");
    }
    let raw = skin_beatoraja::read_lua_header(&root, "QZXmissing.luaskin", &Cancel::new())
        .err()
        .unwrap()
        .to_string();
    assert!(raw.contains("QZXmissing"), "{raw}");
    let payload = std::panic::catch_unwind(|| panic!("QZXpanic")).unwrap_err();
    assert_eq!(payload.downcast_ref::<&str>(), Some(&"QZXpanic"));
}
