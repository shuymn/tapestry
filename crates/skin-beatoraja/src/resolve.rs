//! Resolution: the `include` and `if`/`value`/`values` wrappers expanded
//! for one set of enabled options, field by field, by the type each field
//! expects.
//!
//! The source enables the wrappers only for the types it registers
//! serializers for (`JsonSkinSerializer.java:34-97` at the `0.8.9` tag);
//! of those the header reads `Property`, `Filepath` and `Offset` and their
//! arrays. Every other field, `category` and option items included, is
//! read literally or not at all: a wrapper there is an ordinary member and
//! nothing it names is read.
//!
//! - An array field (`ArraySerializer.read`, JsonSkinSerializer.java:278-
//!   324): each item that is an object with `if` and `value` or `values`
//!   adds, when its condition holds, its `value` as one record and then
//!   its `values` as an array field; an object with `include` adds the
//!   included file read as the array field; any other item is one record.
//!   An object with `include` in place of the array is the included
//!   array; any other object is one record; a string or number is no
//!   records.
//! - A record (`ObjectSerializer.read`, JsonSkinSerializer.java:220-269):
//!   an array is a choice, its first item whose `if` holds giving its
//!   `value` as the record (an item that is no object holds with no
//!   value); an object with `include` is the included file read as a
//!   record; anything else is the record itself.
//! - A condition (`testOption`, JsonSkinSerializer.java:176-211): absent
//!   holds; a number `n` holds when `n` is enabled, `-n` when `n` is not;
//!   an array holds when each item does, an item that is an array holding
//!   when any of its numbers does; anything else fails.
//! - Wrapper names match ignoring ASCII case and the first one wins
//!   (`JsonValue.get`); ordinary fields match exactly and the last wins.
//!
//! An include resolves from the folder of the file that names it
//! (JsonSkinSerializer.java:236). Its name may step up with `..` but not
//! above the skin folder; a rooted name is refused before it is joined. A
//! file already on the chain of includes is a cycle, found by its
//! canonical identity before it is parsed. Intentional policies, where the
//! source differs: a missing include is an error (the source skips it), a
//! name with `*` is refused (the source substitutes file choices into
//! it), and only the last of duplicate ordinary fields is resolved (the
//! source reads every one and keeps the last).

use std::collections::{BTreeSet, HashMap};
use std::path::Path;

use skin_model::{FileId, Root, RootError};

use crate::error::{ImportError, InputLimit, Loc};
use crate::json;
use crate::limits::{
    Limits, MAX_FILE_BYTES, MAX_INCLUDE_DEPTH, MAX_INCLUDE_PARSES, MAX_INPUT_BYTES,
};
use crate::tree::{Arena, ESCAPED, Kind, NONE, Scalar, Text, Unescape};

/// The type number of a 7-key play skin (`SkinType.java:12` at `0.8.9`).
const PLAY_7: i32 = 0;

/// A definition file read and parsed, not resolved. Resolving it
/// ([`resolve`]) adds the included files it reaches and drops them again
/// at the next resolution, so it can be resolved again with other
/// options.
pub struct Definition {
    root: Root,
    pub(crate) arena: Arena,
    entry: FileId,
    entry_nodes: usize,
    entry_bytes: u64,
    top: u32,
    limits: Limits,
    pub(crate) run: Run,
}

/// The counters of one resolution.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Run {
    input_bytes: u64,
    parses: u32,
    decoded: u64,
    expanded: usize,
    pub(crate) entries: usize,
}

/// A definition resolved for one set of options: the header's fields'
/// records, in order, as nodes of the definition's arena.
pub struct Resolved<'d> {
    pub(crate) def: &'d mut Definition,
    pub(crate) name: Option<u32>,
    pub(crate) author: Option<u32>,
    pub(crate) size: [u32; 2],
    pub(crate) options: Vec<u32>,
    pub(crate) files: Vec<u32>,
    pub(crate) offsets: Vec<u32>,
}

/// The folder part of a normalised name.
fn dir_of(name: &str) -> String {
    name.rsplit_once('/')
        .map_or(String::new(), |(d, _)| d.to_owned())
}

/// Reads `name` from `root` with what remains of the input allowance,
/// at most [`MAX_FILE_BYTES`]. `at` gives the error's location, built
/// only on failure.
fn read_file(
    root: &Root,
    name: &str,
    used: u64,
    at: impl FnOnce() -> Option<Loc>,
) -> Result<(Vec<u8>, FileId), ImportError> {
    let max = MAX_FILE_BYTES.min(MAX_INPUT_BYTES.saturating_sub(used));
    root.read(name, max).map_err(|source| match source {
        RootError::TooLarge { len, .. } => ImportError::Input {
            at: at(),
            limit: if len > MAX_FILE_BYTES {
                InputLimit::FileBytes
            } else {
                InputLimit::InputBytes
            },
        },
        source => ImportError::Read {
            at: at(),
            name: name.to_owned(),
            source,
        },
    })
}

/// `n` truncated toward zero, if that is inside `i32`.
#[expect(
    clippy::cast_possible_truncation,
    reason = "the value is finite, whole and inside i32, checked just before"
)]
fn truncated(n: f64) -> Option<i32> {
    let t = n.trunc();
    (t.is_finite() && t >= f64::from(i32::MIN) && t <= f64::from(i32::MAX)).then_some(t as i32)
}

/// Whether `s` is an optional sign and decimal digits: an integer, if out
/// of range.
fn integer_shaped(s: &str) -> bool {
    let digits = s.strip_prefix(['+', '-']).unwrap_or(s);
    !digits.is_empty() && digits.bytes().all(|b| b.is_ascii_digit())
}

impl Definition {
    /// Reads and parses the entry file `file`, a name relative to the skin
    /// folder `root`.
    ///
    /// # Errors
    ///
    /// The folder or the file cannot be read, the file is over
    /// [`MAX_FILE_BYTES`], or it does not parse.
    pub fn read(root: &Path, file: &str) -> Result<Self, ImportError> {
        Self::read_with(root, file, Limits::STANDARD)
    }

    pub(crate) fn read_with(root: &Path, file: &str, limits: Limits) -> Result<Self, ImportError> {
        let root = Root::new(root).map_err(|source| ImportError::Read {
            at: None,
            name: root.display().to_string(),
            source,
        })?;
        let (bytes, entry) = read_file(&root, file, 0, || None)?;
        let entry_bytes = u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let mut arena = Arena::new();
        arena.files.push(Text {
            name: entry.name().to_owned(),
            dir: dir_of(entry.name()),
            bytes: bytes.into_boxed_slice(),
        });
        let top = json::parse(&mut arena, 0, 0)?;
        Ok(Self {
            root,
            entry_nodes: arena.nodes.len(),
            arena,
            entry,
            entry_bytes,
            top,
            limits,
            run: Run::default(),
        })
    }

    pub(crate) fn root(&self) -> &Root {
        &self.root
    }

    /// The entry file's top-level object.
    pub(crate) fn top(&self) -> u32 {
        self.top
    }

    /// The entry file's folder: file choices resolve from it.
    pub(crate) fn entry_dir(&self) -> &str {
        &self.arena.files[0].dir
    }

    pub(crate) fn loc(&self, id: u32) -> Loc {
        self.arena.loc(id)
    }

    pub(crate) fn type_error(
        &self,
        id: u32,
        field: &'static str,
        expected: &'static str,
    ) -> ImportError {
        ImportError::Type {
            at: self.loc(id),
            field,
            expected,
        }
    }

    /// Node `id`'s text decoded, its decoded length charged before it is
    /// allocated.
    fn decode(&mut self, id: u32, escaped: bool) -> Result<String, ImportError> {
        let raw = self.arena.raw(id);
        let len: usize = Unescape::new(raw, escaped).map(char::len_utf8).sum();
        let len64 = u64::try_from(len).unwrap_or(u64::MAX);
        if self.run.decoded.saturating_add(len64) > self.limits.decoded_bytes {
            return Err(ImportError::Input {
                at: Some(self.loc(id)),
                limit: InputLimit::DecodedBytes,
            });
        }
        self.run.decoded += len64;
        let mut s = String::with_capacity(len);
        s.extend(Unescape::new(self.arena.raw(id), escaped));
        Ok(s)
    }

    /// A string field (`Json.readValue` to `String`, Json.java:1014 and
    /// JsonValue.java:172-186): a number as written, a boolean as `true`
    /// or `false`, `null` as absent.
    pub(crate) fn string(
        &mut self,
        id: u32,
        field: &'static str,
    ) -> Result<Option<String>, ImportError> {
        match self.arena.scalar(id) {
            Scalar::Object | Scalar::Array => Err(self.type_error(id, field, "a string")),
            Scalar::Null => Ok(None),
            Scalar::Bool(_) | Scalar::Long(_) | Scalar::Double(_) => {
                let escaped = self.arena.node(id).flags & ESCAPED != 0;
                self.decode(id, escaped).map(Some)
            }
            Scalar::Str { escaped, .. } => self.decode(id, escaped).map(Some),
        }
    }

    /// An `int` field (Json.java:1011 and 1034, JsonValue.java:238-251):
    /// a number token truncated toward zero, or a string that parses as a
    /// decimal integer. Outside `i32` is refused (an intentional policy:
    /// the source wraps or saturates).
    pub(crate) fn int(&mut self, id: u32, field: &'static str) -> Result<i32, ImportError> {
        let range = |d: &Self| ImportError::Range {
            at: d.loc(id),
            field,
        };
        match self.arena.scalar(id) {
            Scalar::Long(raw) => raw
                .parse::<i64>()
                .ok()
                .and_then(|v| i32::try_from(v).ok())
                .ok_or_else(|| range(self)),
            Scalar::Double(raw) => raw
                .parse::<f64>()
                .ok()
                .and_then(truncated)
                .ok_or_else(|| range(self)),
            Scalar::Str { raw, escaped } => {
                let decoded;
                let s = if escaped {
                    decoded = self.decode(id, true)?;
                    decoded.as_str()
                } else {
                    raw
                };
                match s.parse::<i32>() {
                    Ok(v) => Ok(v),
                    Err(_) if integer_shaped(s) => Err(range(self)),
                    Err(_) => Err(self.type_error(id, field, "an integer")),
                }
            }
            Scalar::Null | Scalar::Bool(_) | Scalar::Object | Scalar::Array => {
                Err(self.type_error(id, field, "an integer"))
            }
        }
    }

    /// A `boolean` field (Json.java:1042): `true`, or a string equal to
    /// `true` ignoring case; any other string or number is false.
    pub(crate) fn bool(&self, id: u32, field: &'static str) -> Result<bool, ImportError> {
        match self.arena.scalar(id) {
            Scalar::Bool(b) => Ok(b),
            Scalar::Long(_) | Scalar::Double(_) => Ok(false),
            Scalar::Str { raw, escaped } => Ok(Unescape::new(raw, escaped)
                .map(|c| c.to_ascii_lowercase())
                .eq("true".chars())),
            Scalar::Null | Scalar::Object | Scalar::Array => {
                Err(self.type_error(id, field, "a boolean"))
            }
        }
    }

    /// The members of record `id` named in `fields`, the last of each
    /// (case-sensitive), in `fields`' order; none when the record is no
    /// object (the source's record of defaults).
    pub(crate) fn fields<const N: usize>(&self, id: u32, fields: [&str; N]) -> [Option<u32>; N] {
        if self.arena.node(id).kind != Kind::Object {
            return [None; N];
        }
        fields.map(|f| self.arena.last_member(id, f))
    }
}

/// Resolves `def` with the options in `enabled`: the header's top-level
/// fields, the skin type and size checked first, then the options, file
/// choices and offsets with their wrappers expanded, reading the files
/// they include. The previous resolution's included files are dropped
/// first and every count of [`crate::limits`] starts again from the
/// entry file.
///
/// # Errors
///
/// The root is no object, the type is not the 7-key play type, the size
/// is not one of [`crate::SIZES`], an include cannot be read or parsed or
/// forms a cycle, a field has the wrong type, or an input bound is
/// reached.
pub fn resolve<'d>(
    def: &'d mut Definition,
    enabled: &BTreeSet<i32>,
) -> Result<Resolved<'d>, ImportError> {
    def.arena.nodes.truncate(def.entry_nodes);
    def.arena.files.truncate(1);
    def.run = Run {
        input_bytes: def.entry_bytes,
        ..Run::default()
    };
    let top = def.top;
    if def.arena.node(top).kind != Kind::Object {
        return Err(def.type_error(top, "(root)", "an object"));
    }
    // The top level is read literally, ordinary fields only
    // (JSONSkinLoader.java:98-102, `JsonSkin.Skin` has no serializer).
    let [ty, w, h, name, author, property, filepath, offset] = def.fields(
        top,
        [
            "type", "w", "h", "name", "author", "property", "filepath", "offset",
        ],
    );
    // A missing type is -1 and gives no header (JsonSkin.java:9,
    // JSONSkinLoader.java:118).
    let Some(ty) = ty else {
        return Err(ImportError::NotPlay7 { at: None });
    };
    if def.int(ty, "type")? != PLAY_7 {
        return Err(ImportError::NotPlay7 {
            at: Some(def.loc(ty)),
        });
    }
    // 1280×720 when omitted (JsonSkin.java:12-13).
    let w_v = w.map_or(Ok(1280), |id| def.int(id, "w"))?;
    let h_v = h.map_or(Ok(720), |id| def.int(id, "h"))?;
    let size = crate::header::SIZES
        .iter()
        .find(|&&[sw, sh]| i64::from(sw) == i64::from(w_v) && i64::from(sh) == i64::from(h_v))
        .copied()
        .ok_or_else(|| ImportError::Resolution {
            at: w.or(h).map(|id| def.loc(id)),
            w: w_v,
            h: h_v,
        })?;
    let mut r = Resolver {
        chain: vec![def.entry.clone()],
        def,
        enabled,
        strict: None,
    };
    let mut lists = [Vec::new(), Vec::new(), Vec::new()];
    for (field, out) in [property, filepath, offset].into_iter().zip(&mut lists) {
        if let Some(id) = field {
            r.array(id, 2, out)?;
        }
    }
    let [options, files, offsets] = lists;
    Ok(Resolved {
        def: r.def,
        name,
        author,
        size,
        options,
        files,
        offsets,
    })
}

/// The top-level fields the import reads, in [`Tables::arrays`] order.
/// Every one has an array serializer (JsonSkinSerializer.java:73-97)
/// except `bpmgraph`, `hiddenCover`, `liftCover` and `pmchara`, which are
/// read literally ([`LITERAL`]).
pub(crate) const ARRAYS: [&str; 21] = [
    "source",
    "font",
    "image",
    "imageset",
    "value",
    "floatvalue",
    "text",
    "slider",
    "graph",
    "gaugegraph",
    "judgegraph",
    "bpmgraph",
    "hiterrorvisualizer",
    "timingvisualizer",
    "timingdistributiongraph",
    "hiddenCover",
    "liftCover",
    "judge",
    "pmchara",
    "customTimers",
    "destination",
];

/// The fields of [`ARRAYS`] with no array serializer: their items are
/// records as written, no wrapper expanded.
const LITERAL: [&str; 4] = ["bpmgraph", "hiddenCover", "liftCover", "pmchara"];

/// The top-level records the import reads (`ObjectSerializer`), in
/// [`Tables::records`] order.
pub(crate) const RECORDS: [&str; 4] = ["gauge", "practice", "bga", "note"];

/// The definition's fields the import reads, resolved for one set of
/// options: each array field's records, each record field's record, and
/// each destination's `dst` keys.
#[derive(Debug, Default)]
pub(crate) struct Tables {
    pub(crate) arrays: [Vec<u32>; ARRAYS.len()],
    pub(crate) records: [Option<u32>; RECORDS.len()],
    /// The keys of every destination, in destination order.
    pub(crate) keys: Vec<u32>,
    /// Per destination: its keys' end in `keys`.
    pub(crate) key_ends: Vec<u32>,
    /// The nested arrays of the play objects, by the field's node: the
    /// `note` record's `dst`, `group`, `bpm`, `stop` and `time`, each
    /// judge's `images` and `numbers`, and the `dst` of every destination
    /// in them (`JsonSkinSerializer.java:69-94`: `Destination[]` and
    /// `Animation[]` have array serializers). Primitive arrays stay
    /// literal.
    pub(crate) nested: HashMap<u32, Vec<u32>>,
}

impl Resolved<'_> {
    /// Resolves the fields the import reads with the options in `enabled`,
    /// the same ones this resolution used: the registered arrays and
    /// records, and each destination's `dst` (an `Animation[]` with its own
    /// array serializer). Literal arrays keep their items as written.
    ///
    /// # Errors
    ///
    /// What resolution refuses: an include that cannot be read or parsed
    /// or forms a cycle, a field of the wrong type, an input bound.
    pub(crate) fn tables(&mut self, enabled: &BTreeSet<i32>) -> Result<Tables, ImportError> {
        let top = self.def.top;
        let def = &mut *self.def;
        let mut r = Resolver {
            chain: vec![def.entry.clone()],
            def,
            enabled,
            strict: None,
        };
        let mut t = Tables::default();
        for (name, out) in ARRAYS.iter().zip(&mut t.arrays) {
            let Some(id) = r.def.arena.last_member(top, name) else {
                continue;
            };
            if LITERAL.contains(name) {
                if r.def.arena.node(id).kind == Kind::Array {
                    let items: Vec<u32> = r.def.arena.children(id).collect();
                    for item in items {
                        r.emit(item, out)?;
                    }
                }
            } else {
                r.array(id, 2, out)?;
            }
        }
        for (name, out) in RECORDS.iter().zip(&mut t.records) {
            if let Some(id) = r.def.arena.last_member(top, name) {
                *out = r.record(id, 2)?;
            }
        }
        if let Some(note) = RECORDS
            .iter()
            .position(|n| *n == "note")
            .and_then(|i| t.records.get(i).copied().flatten())
        {
            let [dst, group, bpm, stop, time] =
                r.def.fields(note, ["dst", "group", "bpm", "stop", "time"]);
            if let Some(dst) = dst {
                r.nested_array(dst, 3, &mut t.nested)?;
            }
            if let Some(group) = group {
                r.groups(group, 3, &mut t.nested)?;
            }
            for list in [bpm, stop, time].into_iter().flatten() {
                r.nested_destinations(list, 3, &mut t.nested)?;
            }
        }
        if let Some(i) = ARRAYS.iter().position(|n| *n == "judge") {
            let judges = t.arrays.get(i).cloned().unwrap_or_default();
            for judge in judges {
                let [images, numbers] = r.def.fields(judge, ["images", "numbers"]);
                for list in [images, numbers].into_iter().flatten() {
                    r.nested_destinations(list, 4, &mut t.nested)?;
                }
            }
        }
        let [.., destinations] = &t.arrays;
        let destinations = destinations.clone();
        for d in destinations {
            if let Some(dst) = r.def.fields(d, ["dst"])[0] {
                r.array(dst, 4, &mut t.keys)?;
            }
            t.key_ends
                .push(u32::try_from(t.keys.len()).unwrap_or(u32::MAX));
        }
        Ok(t)
    }
}

struct Resolver<'d, 'e> {
    def: &'d mut Definition,
    enabled: &'e BTreeSet<i32>,
    /// The entry file and the includes being resolved, outermost first.
    chain: Vec<FileId>,
    /// The field whose structure is checked while it resolves: a value
    /// other than `null` where an array belongs is refused under its name
    /// rather than read as no records.
    strict: Option<&'static str>,
}

impl Resolver<'_, '_> {
    fn arena(&self) -> &Arena {
        &self.def.arena
    }

    /// The array field `id` at `depth`, resolved into `nested` under it.
    fn nested_array(
        &mut self,
        id: u32,
        depth: u32,
        nested: &mut HashMap<u32, Vec<u32>>,
    ) -> Result<(), ImportError> {
        let mut out = Vec::new();
        self.array(id, depth, &mut out)?;
        // The list stays live for the whole import: drop the growth slack.
        out.shrink_to_fit();
        nested.insert(id, out);
        Ok(())
    }

    /// A `Destination[]` field at `depth`, and each destination's `dst`.
    fn nested_destinations(
        &mut self,
        id: u32,
        depth: u32,
        nested: &mut HashMap<u32, Vec<u32>>,
    ) -> Result<(), ImportError> {
        self.nested_array(id, depth, nested)?;
        // Each destination's `dst` is another node, so `id`'s list stays
        // as resolved while they are added.
        let mut i = 0;
        while let Some(&d) = nested.get(&id).and_then(|l| l.get(i)) {
            i += 1;
            if let Some(dst) = self.def.fields(d, ["dst"])[0] {
                self.nested_array(dst, depth + 2, nested)?;
            }
        }
        Ok(())
    }

    /// The `note` record's `group` field at `depth` (`Destination[]`,
    /// `JsonSkinSerializer.java:69-94`), and each group's `dst`, resolved
    /// as [`Self::nested_destinations`] does with their structure
    /// checked once the wrappers have chosen: the field and each `dst`
    /// an array, each group and each key an object. A wrapper that
    /// chooses nothing leaves its list empty.
    fn groups(
        &mut self,
        id: u32,
        depth: u32,
        nested: &mut HashMap<u32, Vec<u32>>,
    ) -> Result<(), ImportError> {
        self.strict_array(id, depth, "group", nested)?;
        let mut i = 0;
        while let Some(&d) = nested.get(&id).and_then(|l| l.get(i)) {
            i += 1;
            if self.arena().node(d).kind != Kind::Object {
                return Err(self.def.type_error(d, "group", "an object"));
            }
            let Some(dst) = self.def.fields(d, ["dst"])[0] else {
                continue;
            };
            self.strict_array(dst, depth + 2, "dst", nested)?;
            let bad = nested
                .get(&dst)
                .and_then(|keys| {
                    keys.iter()
                        .find(|&&k| self.arena().node(k).kind != Kind::Object)
                })
                .copied();
            if let Some(k) = bad {
                return Err(self.def.type_error(k, "dst", "an object"));
            }
        }
        Ok(())
    }

    /// [`Self::nested_array`] with `field`'s structure checked
    /// ([`Self::strict`]).
    fn strict_array(
        &mut self,
        id: u32,
        depth: u32,
        field: &'static str,
        nested: &mut HashMap<u32, Vec<u32>>,
    ) -> Result<(), ImportError> {
        self.strict = Some(field);
        let r = self.nested_array(id, depth, nested);
        self.strict = None;
        r
    }

    /// Adds `record` to `out`, counting it first.
    fn emit(&mut self, record: u32, out: &mut Vec<u32>) -> Result<(), ImportError> {
        if self.def.run.expanded >= self.def.limits.expanded {
            return Err(ImportError::Input {
                at: Some(self.def.loc(record)),
                limit: InputLimit::Expanded,
            });
        }
        self.def.run.expanded += 1;
        out.push(record);
        Ok(())
    }

    /// An array field at depth `depth` into `out`.
    fn array(&mut self, id: u32, depth: u32, out: &mut Vec<u32>) -> Result<(), ImportError> {
        if let Some(member) = self.arena().first_member_ci(id, "include") {
            return self.include(member, depth, |r, root, d| r.array(root, d, out));
        }
        let kind = self.arena().node(id).kind;
        match kind {
            Kind::Array => {
                let mut next = self.arena().node(id).first_child;
                while next != NONE {
                    let item = next;
                    next = self.arena().node(item).next_sibling;
                    self.item(item, depth + 1, out)?;
                }
                Ok(())
            }
            Kind::Object => match self.record(id, depth)? {
                Some(rec) => self.emit(rec, out),
                None => Ok(()),
            },
            Kind::Quoted | Kind::Bare => match self.strict {
                Some(field) if !matches!(self.arena().scalar(id), Scalar::Null) => {
                    Err(self.def.type_error(id, field, "an array"))
                }
                _ => Ok(()),
            },
        }
    }

    /// One item of an array field.
    fn item(&mut self, item: u32, depth: u32, out: &mut Vec<u32>) -> Result<(), ImportError> {
        let a = self.arena();
        let cond = a.first_member_ci(item, "if");
        let value = a.first_member_ci(item, "value");
        let values = a.first_member_ci(item, "values");
        if let Some(cond) = cond.filter(|_| value.is_some() || values.is_some()) {
            if self.test(Some(cond))? {
                if let Some(v) = value
                    && let Some(rec) = self.record(v, depth + 1)?
                {
                    self.emit(rec, out)?;
                }
                if let Some(vs) = values {
                    self.array(vs, depth + 1, out)?;
                }
            }
            return Ok(());
        }
        if let Some(member) = self.arena().first_member_ci(item, "include") {
            return self.include(member, depth, |r, root, d| r.array(root, d, out));
        }
        match self.record(item, depth)? {
            Some(rec) => self.emit(rec, out),
            None => Ok(()),
        }
    }

    /// A record at depth `depth`: the literal node, or none when a choice
    /// selects nothing.
    fn record(&mut self, id: u32, depth: u32) -> Result<Option<u32>, ImportError> {
        if let Some(member) = self.arena().first_member_ci(id, "include") {
            return self.include(member, depth, |r, root, d| r.record(root, d));
        }
        let kind = self.arena().node(id).kind;
        match kind {
            Kind::Array => {
                let mut next = self.arena().node(id).first_child;
                while next != NONE {
                    let branch = next;
                    next = self.arena().node(branch).next_sibling;
                    let cond = self.arena().first_member_ci(branch, "if");
                    if self.test(cond)? {
                        return match self.arena().first_member_ci(branch, "value") {
                            Some(v) => self.record(v, depth + 2),
                            None => Ok(None),
                        };
                    }
                }
                Ok(None)
            }
            _ => Ok(Some(id)),
        }
    }

    /// Whether the pinned reader made node `id` a number.
    fn is_number(&self, id: u32) -> bool {
        self.arena().scalar(id).is_number()
    }

    /// Whether condition `cond` holds; an array's items are tested in
    /// order up to the first that fails.
    fn test(&mut self, cond: Option<u32>) -> Result<bool, ImportError> {
        let Some(cond) = cond else {
            return Ok(true);
        };
        if self.is_number(cond) {
            return self.test_number(cond);
        }
        if self.arena().node(cond).kind != Kind::Array {
            return Ok(false);
        }
        let mut next = self.arena().node(cond).first_child;
        while next != NONE {
            let item = next;
            next = self.arena().node(item).next_sibling;
            let held = if self.is_number(item) {
                self.test_number(item)?
            } else if self.arena().node(item).kind == Kind::Array {
                let mut held = false;
                let mut alt = self.arena().node(item).first_child;
                while alt != NONE {
                    let one = alt;
                    alt = self.arena().node(one).next_sibling;
                    if self.is_number(one) && self.test_number(one)? {
                        held = true;
                        break;
                    }
                }
                held
            } else {
                false
            };
            if !held {
                return Ok(false);
            }
        }
        Ok(true)
    }

    /// `n` enabled for `n >= 0`, `-n` not enabled for `n < 0`; `i32::MIN`,
    /// which has no negation in `i32`, is a range error (the source's
    /// wrapping negation is not reproduced).
    fn test_number(&mut self, id: u32) -> Result<bool, ImportError> {
        let op = self.def.int(id, "if")?;
        if op >= 0 {
            return Ok(self.enabled.contains(&op));
        }
        let neg = op.checked_neg().ok_or_else(|| ImportError::Range {
            at: self.def.loc(id),
            field: "if",
        })?;
        Ok(!self.enabled.contains(&neg))
    }

    /// Reads, parses and resolves the file that `member`, the first
    /// `include` of an object at `depth`, names, through `then` with its
    /// root and the root's depth.
    fn include<T>(
        &mut self,
        member: u32,
        depth: u32,
        then: impl FnOnce(&mut Self, u32, u32) -> Result<T, ImportError>,
    ) -> Result<T, ImportError> {
        // The location is built only for an error: it rescans the file.
        let Some(name) = self.def.string(member, "include")? else {
            return Err(self.def.type_error(member, "include", "a file name"));
        };
        // Both counts are reserved before anything is read.
        if self.chain.len() > MAX_INCLUDE_DEPTH as usize {
            return Err(ImportError::Input {
                at: Some(self.def.loc(member)),
                limit: InputLimit::IncludeDepth,
            });
        }
        if self.def.run.parses >= MAX_INCLUDE_PARSES {
            return Err(ImportError::Input {
                at: Some(self.def.loc(member)),
                limit: InputLimit::IncludeParses,
            });
        }
        self.def.run.parses += 1;
        if name.starts_with(['/', '\\']) {
            return Err(ImportError::Read {
                at: Some(self.def.loc(member)),
                name,
                source: RootError::Rejected,
            });
        }
        if name.contains('*') {
            return Err(ImportError::IncludeWildcard {
                at: self.def.loc(member),
            });
        }
        let file = self.arena().node(member).file;
        let dir = &self.arena().files[usize::from(file)].dir;
        let joined = if dir.is_empty() {
            name
        } else {
            format!("{dir}/{name}")
        };
        let (bytes, id) = read_file(&self.def.root, &joined, self.def.run.input_bytes, || {
            Some(self.def.loc(member))
        })?;
        if self.chain.contains(&id) {
            let mut chain: Vec<String> = self.chain.iter().map(|f| f.name().to_owned()).collect();
            chain.push(id.name().to_owned());
            return Err(ImportError::IncludeCycle {
                at: self.def.loc(member),
                chain,
            });
        }
        self.def.run.input_bytes += u64::try_from(bytes.len()).unwrap_or(u64::MAX);
        let index = u16::try_from(self.arena().files.len()).map_err(|_| ImportError::Input {
            at: Some(self.def.loc(member)),
            limit: InputLimit::IncludeParses,
        })?;
        self.def.arena.files.push(Text {
            name: id.name().to_owned(),
            dir: dir_of(id.name()),
            bytes: bytes.into_boxed_slice(),
        });
        let root = json::parse(&mut self.def.arena, index, depth)?;
        self.chain.push(id);
        let result = then(self, root, depth + 1);
        self.chain.pop();
        result
    }
}

#[cfg(test)]
mod tests;
