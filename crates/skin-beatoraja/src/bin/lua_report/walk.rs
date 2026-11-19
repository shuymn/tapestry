//! The walk (docs/stage3.md §3, the conversion report): each skin's
//! default configuration and each catalogue row toggled alone, under the
//! decided import facts, written as records ([`super::record`]) and
//! summarised per skin. A configuration that did not import, or whose
//! import a wall-clock limit stopped, is attempted once more: two attempts
//! that disagree make it `unstable`, two stopped by the clock
//! `inconclusive`.

use std::fmt::Write as _;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::PathBuf;

use sha2::{Digest, Sha256};
use skin_beatoraja::lua::facts::ImportFacts;
use skin_beatoraja::lua::survey::{Choice, Probe, Survey, probe, survey_probe};
use skin_beatoraja::{Cancel, Header};
use skin_model::ScreenKind;

use super::label::{self, Label};
use super::record::{Record, Records, SkinEntry};

/// The import facts every configuration is walked under.
pub const FACTS: &str = "--autoplay off --class normal --level 12";
/// What a result skin's configurations add to [`FACTS`].
pub const RESULT_FACTS: &str = "--result dense";

/// The runtime frame's time while walking: out of reach, so its call
/// statuses are not the clock's to decide.
const FRAME_TIME: std::time::Duration = std::time::Duration::from_secs(3_600);

/// A catalogue row: what `--catalogue` prints and the walk toggles alone.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Row {
    /// Item `item` of option `option`, by their indices in the header.
    Option { option: usize, item: usize },
    /// Candidate `candidate` of file choice `file`.
    File { file: usize, candidate: usize },
}

/// `header`'s rows in catalogue order: each named item of each named
/// option, then each candidate of each named file choice.
pub fn rows(header: &Header) -> Vec<Row> {
    let mut rows = Vec::new();
    for (option, o) in header.options.iter().enumerate() {
        if o.name.is_none() {
            continue;
        }
        for (item, i) in o.items.iter().enumerate() {
            if i.name.is_some() {
                rows.push(Row::Option { option, item });
            }
        }
    }
    for (file, f) in header.files.iter().enumerate() {
        if f.name.is_none() {
            continue;
        }
        for candidate in 0..f.candidates.len() {
            rows.push(Row::File { file, candidate });
        }
    }
    rows
}

/// The names `row` gives the configuration, as `--option` and `--file`
/// take them: `None` for a row the selector cannot reach (an option or
/// item whose name an earlier one already has, which `Config::named`
/// would take instead, or a candidate of a file choice whose name an
/// earlier one has and which that earlier one does not list).
pub fn choice(header: &Header, row: &Row) -> Option<Choice> {
    let mut c = Choice::default();
    match *row {
        Row::Option { option, item } => {
            let o = header.options.get(option)?;
            let name = o.name.as_deref()?;
            let item_name = o.items.get(item)?.name.as_deref()?;
            let first_option = header
                .options
                .iter()
                .position(|x| x.name.as_deref() == Some(name))?;
            let first_item = o
                .items
                .iter()
                .position(|x| x.name.as_deref() == Some(item_name))?;
            if first_option != option || first_item != item {
                return None;
            }
            c.options.push((name.to_owned(), item_name.to_owned()));
        }
        Row::File { file, candidate } => {
            let f = header.files.get(file)?;
            let name = f.name.as_deref()?;
            let candidate = f.candidates.get(candidate)?;
            // `Config::named` checks the candidate against the first file
            // choice of that name; the selection then sets every choice of
            // that name.
            let first = header
                .files
                .iter()
                .find(|x| x.name.as_deref() == Some(name))?;
            if !first.candidates.contains(candidate) {
                return None;
            }
            c.files.push((name.to_owned(), candidate.clone()));
        }
    }
    Some(c)
}

/// A row's toggle and its position, as a record holds them.
fn coordinates(row: Option<&Row>) -> (&'static str, Option<[usize; 2]>) {
    match row {
        None => ("default", None),
        Some(Row::Option { option, item }) => ("option", Some([*option, *item])),
        Some(Row::File { file, candidate }) => ("file", Some([*file, *candidate])),
    }
}

/// Appends `bytes` length-prefixed.
fn put(buf: &mut Vec<u8>, bytes: &[u8]) {
    buf.extend_from_slice(&(bytes.len() as u64).to_le_bytes());
    buf.extend_from_slice(bytes);
}

/// Appends an optional string: a presence byte, then the string.
fn put_opt(buf: &mut Vec<u8>, s: Option<&str>) {
    match s {
        None => buf.push(0),
        Some(s) => {
            buf.push(1);
            put(buf, s.as_bytes());
        }
    }
}

fn put_num(buf: &mut Vec<u8>, n: i64) {
    buf.extend_from_slice(&n.to_le_bytes());
}

/// `bytes` in lower-case hexadecimal.
pub fn hex(bytes: &[u8]) -> String {
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// A skin as the walk names it: its folder's name and its entry. Both go
/// into its digest (never printed), so that two folders with one header
/// and entry stay two skins whatever their order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Skin {
    pub folder: String,
    pub entry: String,
}

impl Skin {
    /// The skin at `root` with `entry`: the folder's own name, resolved
    /// when `root` ends in `.` or `..`.
    pub fn at(root: &str, entry: &str) -> Self {
        let path = std::path::Path::new(root);
        let folder = std::fs::canonicalize(path)
            .ok()
            .and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned()))
            .or_else(|| path.file_name().map(|n| n.to_string_lossy().into_owned()))
            .unwrap_or_else(|| root.to_owned());
        Self {
            folder,
            entry: entry.to_owned(),
        }
    }

    fn put(&self, b: &mut Vec<u8>) {
        put(b, self.folder.as_bytes());
        put(b, self.entry.replace('\\', "/").as_bytes());
    }
}

/// The catalogue digest of `header` read as `skin`: SHA-256 over a
/// length-prefixed encoding of the folder's name, the entry and every
/// header field.
pub fn digest(header: &Header, skin: &Skin) -> String {
    let mut b = Vec::new();
    put(&mut b, b"header");
    skin.put(&mut b);
    put(&mut b, format!("{:?}", header.kind).as_bytes());
    put_num(&mut b, i64::from(header.size[0]));
    put_num(&mut b, i64::from(header.size[1]));
    put_opt(&mut b, header.name.as_deref());
    put_opt(&mut b, header.author.as_deref());
    put_num(&mut b, header.options.len() as i64);
    for o in &header.options {
        put_opt(&mut b, o.name.as_deref());
        put_opt(&mut b, o.def.as_deref());
        put_num(&mut b, o.items.len() as i64);
        for i in &o.items {
            put_opt(&mut b, i.name.as_deref());
            put_num(&mut b, i64::from(i.op));
        }
    }
    put_num(&mut b, header.files.len() as i64);
    for f in &header.files {
        put_opt(&mut b, f.name.as_deref());
        put_opt(&mut b, f.path.as_deref());
        put_opt(&mut b, f.def.as_deref());
        put_num(&mut b, f.candidates.len() as i64);
        for c in &f.candidates {
            put(&mut b, c.as_bytes());
        }
    }
    put_num(&mut b, header.offsets.len() as i64);
    for o in &header.offsets {
        put_opt(&mut b, o.name.as_deref());
        put_num(&mut b, i64::from(o.id));
        for flag in [o.x, o.y, o.w, o.h, o.r, o.a] {
            b.push(u8::from(flag));
        }
    }
    hex(&Sha256::digest(&b))
}

/// The digest standing for a header that was not read: its outcome and
/// the skin, so that a stable refusal compares equal.
fn refusal_digest(outcome: &str, skin: &Skin) -> String {
    let mut b = Vec::new();
    put(&mut b, b"refused");
    skin.put(&mut b);
    put(&mut b, outcome.as_bytes());
    hex(&Sha256::digest(&b))
}

/// What the walk imports through: a header read and a survey of it, so
/// tests can supply both.
pub trait Source {
    /// One header read of skin `skin`: the header, or the survey of its
    /// refusal.
    fn probe(&mut self, skin: usize) -> Result<Probe, Box<Survey>>;
    /// The survey of `probe`'s header with `choice` under `facts`.
    fn survey(&mut self, probe: &mut Probe, facts: &ImportFacts, choice: &Choice) -> Survey;
}

/// The skins on disk, `(root, entry)` each.
pub struct Disk(pub Vec<(String, String)>);

impl Source for Disk {
    fn probe(&mut self, skin: usize) -> Result<Probe, Box<Survey>> {
        let Some((root, entry)) = self.0.get(skin) else {
            return Err(Box::new(panicked()));
        };
        let p = probe(&PathBuf::from(root), entry, &Cancel::new());
        match p.refused() {
            Some(refused) => Err(Box::new(refused)),
            None => Ok(p),
        }
    }

    fn survey(&mut self, probe: &mut Probe, facts: &ImportFacts, choice: &Choice) -> Survey {
        survey_probe(probe, facts, &Cancel::new(), false, choice)
    }
}

/// A panic's survey: the outcome `panic` alone.
fn panicked() -> Survey {
    Survey {
        outcome: "panic".to_owned(),
        ..Survey::default()
    }
}

/// One attempt: a header read, then (when read) the survey of `choice`;
/// the digest the read gave (`None` when refused) and the survey.
fn attempt(
    src: &mut dyn Source,
    index: usize,
    skin: &Skin,
    facts: &ImportFacts,
    choice: &Choice,
) -> (Option<String>, Survey) {
    let mut p = match catch_unwind(AssertUnwindSafe(|| src.probe(index))) {
        Ok(Ok(p)) => p,
        Ok(Err(refused)) => return (None, *refused),
        Err(_) => return (None, panicked()),
    };
    let d = p.header().map(|h| digest(h, skin));
    let s = catch_unwind(AssertUnwindSafe(|| src.survey(&mut p, facts, choice)))
        .unwrap_or_else(|_| panicked());
    (d, s)
}

/// Whether an attempt read another header than `digest`'s: another
/// digest, or (unless the clock stopped the read) none.
fn other_header(digest: &str, a: &(Option<String>, Survey)) -> bool {
    match a.0.as_deref() {
        Some(d) => d != digest,
        None => !a.1.timed_out,
    }
}

/// Two attempts settled into one record's survey and outcome: two that
/// read different headers are `unstable`, whether or not the clock
/// stopped either; otherwise a timed-out attempt is no outcome, two that
/// timed out are `inconclusive`, two completed ones that disagree are
/// `unstable`, and else the first completed one is the record.
fn settle(
    digest: &str,
    a: (Option<String>, Survey),
    b: (Option<String>, Survey),
) -> (Survey, Vec<Label>) {
    if other_header(digest, &a) || other_header(digest, &b) {
        return undecided("unstable", &a.1, &b.1);
    }
    match (a.1.timed_out, b.1.timed_out) {
        (true, true) => undecided("inconclusive", &a.1, &b.1),
        (true, false) => (b.1, Vec::new()),
        (false, true) => (a.1, Vec::new()),
        (false, false) if a.1.outcome == b.1.outcome => (a.1, Vec::new()),
        (false, false) => undecided("unstable", &a.1, &b.1),
    }
}

/// A record that is neither attempt's: `outcome` (`unstable` or
/// `inconclusive`), with both attempts' outcomes.
fn undecided(outcome: &str, a: &Survey, b: &Survey) -> (Survey, Vec<Label>) {
    (
        Survey {
            outcome: outcome.to_owned(),
            kind: a.kind,
            ..Survey::default()
        },
        vec![label::outcome(a), label::outcome(b)],
    )
}

/// The facts a skin of `kind` is walked under, and the configuration they
/// carry (a result skin's dense view).
fn facts_for(kind: ScreenKind) -> (ImportFacts, Choice) {
    let mut line = FACTS.to_owned();
    if kind == ScreenKind::Result {
        line.push(' ');
        line.push_str(RESULT_FACTS);
    }
    let args: Vec<String> = line.split_whitespace().map(str::to_owned).collect();
    match super::facts(&args) {
        Some((facts, choice, _, _)) => (facts, choice),
        None => unreachable!("the walk's own facts parse"),
    }
}

/// A skin's header for the walk: its first read, read once more when a
/// wall-clock limit refused it; the probe, or the survey a refusal gives
/// (`inconclusive` when both reads timed out) and the attempts' outcomes.
fn header_of(src: &mut dyn Source, skin: usize) -> Result<Probe, Box<(Survey, Vec<Label>)>> {
    let mut read = || match catch_unwind(AssertUnwindSafe(|| src.probe(skin))) {
        Ok(r) => r,
        Err(_) => Err(Box::new(panicked())),
    };
    let refused = match read() {
        Ok(p) => return Ok(p),
        Err(refused) if refused.timed_out => refused,
        Err(refused) => return Err(Box::new((*refused, Vec::new()))),
    };
    match read() {
        Ok(p) => Ok(p),
        Err(again) if again.timed_out => Err(Box::new((
            Survey {
                outcome: "inconclusive".to_owned(),
                ..Survey::default()
            },
            vec![label::outcome(&refused), label::outcome(&again)],
        ))),
        Err(again) => Err(Box::new((*again, Vec::new()))),
    }
}

/// Walks every skin of `src` (`skins`, in argument order) and returns
/// the records, marked as `build`'s, and the summary to print.
pub fn walk(src: &mut dyn Source, skins: &[Skin], build: &str) -> (Records, String) {
    skin_beatoraja::vm_hooks::set_frame_time(Some(FRAME_TIME));
    let start = std::time::Instant::now();
    let mut records = Records::new(FACTS, RESULT_FACTS, build);
    let mut summary = String::new();
    let (mut walked, mut imported_total) = (0, 0);
    for (skin, named) in skins.iter().enumerate() {
        let mut list = Vec::new();
        // A source's read that gives no header is a refusal like any other
        // (a panic's), so the skin keeps its record.
        let read = header_of(src, skin).and_then(|p| match p.header().cloned() {
            Some(h) => Ok((p, h)),
            None => Err(Box::new((panicked(), Vec::new()))),
        });
        let (digest, kind) = match read {
            Err(refused) => {
                let (s, attempts) = *refused;
                let d = refusal_digest(&s.outcome, named);
                list.push(Record::of(skin, 0, coordinates(None), &s, attempts));
                (d, None)
            }
            Ok((mut p, header)) => {
                let d = digest(&header, named);
                let (facts, base) = facts_for(header.kind);
                let all = rows(&header);
                let configurations = std::iter::once(None)
                    .chain(all.iter().map(Some))
                    .enumerate();
                for (k, row) in configurations {
                    let choice = match row {
                        None => Some(base.clone()),
                        Some(r) => choice(&header, r).map(|c| Choice {
                            result: base.result.clone(),
                            ..c
                        }),
                    };
                    let Some(choice) = choice else {
                        let s = Survey {
                            outcome: "unselectable".to_owned(),
                            kind: Some(header.kind),
                            ..Survey::default()
                        };
                        list.push(Record::of(skin, k, coordinates(row), &s, Vec::new()));
                        continue;
                    };
                    // The first attempt reuses the walk's read of the
                    // header; a refusal or a timeout is attempted again,
                    // reading the header again.
                    let first = (
                        Some(d.clone()),
                        catch_unwind(AssertUnwindSafe(|| src.survey(&mut p, &facts, &choice)))
                            .unwrap_or_else(|_| panicked()),
                    );
                    let (s, attempts) = if first.1.outcome == "imported" && !first.1.timed_out {
                        (first.1, Vec::new())
                    } else {
                        let second = attempt(src, skin, named, &facts, &choice);
                        settle(&d, first, second)
                    };
                    list.push(Record::of(skin, k, coordinates(row), &s, attempts));
                }
                (d, Some(header.kind))
            }
        };
        let count = |o: &str| list.iter().filter(|r| r.outcome == o).count();
        let imported = count("imported");
        let kind_label = label::kind(kind);
        let _ = writeln!(
            summary,
            "skin {skin}: {kind_label}, configurations {}, imported {imported}, \
             unselectable {}, inconclusive {}, unstable {}",
            list.len(),
            count("unselectable"),
            count("inconclusive"),
            count("unstable")
        );
        for r in list.iter().filter(|r| r.outcome != "imported") {
            let _ = writeln!(
                summary,
                "  configuration {} ({}): {}",
                r.configuration,
                r.toggle_label(),
                r.outcome
            );
        }
        walked += list.len();
        imported_total += imported;
        records.push_skin(
            SkinEntry {
                index: skin,
                digest,
                kind: kind_label,
                configurations: list.len(),
            },
            list,
        );
    }
    let _ = writeln!(
        summary,
        "walked {walked} configurations over {} skins, {imported_total} imported, {:.0} s",
        skins.len(),
        start.elapsed().as_secs_f64()
    );
    (records, summary)
}
