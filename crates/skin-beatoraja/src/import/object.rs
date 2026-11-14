//! Object tables by id, built on first use, in the source's lookup order
//! (`JsonSkinObjectLoader.java:38-~560`, `PlaySkinObjectLoader.java` at
//! `0.8.9`): the first record of an id wins in each table, and the first
//! table that has the id wins, whether or not its object's file, texture
//! or font loads (design.md "Differences from the source", imported
//! skins). The play loader sets `note` and keeps looking, so a later play
//! object of the same id overrides it (`JsonPlaySkinObjectLoader.java:
//! 37-181`): `note` is looked up last. The selection loader looks up the
//! common objects, the gauge among them, then its `songlist`
//! (`JsonSelectSkinObjectLoader.java:30-37`): the play loader's own
//! objects are never looked up there. The result and decision loaders
//! look up the common objects alone. A play destination whose id the
//! judge or the note also holds is refused by the caller
//! (`Cx::destination`), which asks [`Objects::holds`]; so is failure
//! presentation that draws nothing of its first object while a later
//! table holds its id ([`Objects::held_after`]).

use std::collections::HashMap;

use skin_model::ScreenKind;

use super::report::Object;
use crate::error::ImportError;
use crate::resolve::{ARRAYS, Definition, RECORDS, Tables};

/// The common objects' entries of [`ORDER`]: up to the gauge.
const COMMON: usize = 14;
/// The selection loader's `songlist` entry of [`ORDER`].
const SONG_LIST: usize = 21;

/// The destination lookup order, by table name (an array field's or a
/// record field's, [`records`]).
const ORDER: [(&str, Object); 22] = [
    ("image", Object::Image),
    ("imageset", Object::ImageSet),
    ("value", Object::Value),
    ("floatvalue", Object::FloatValue),
    ("text", Object::Text),
    ("slider", Object::Slider),
    ("graph", Object::Graph),
    ("gaugegraph", Object::GaugeGraph),
    ("judgegraph", Object::JudgeGraph),
    ("bpmgraph", Object::BpmGraph),
    ("hiterrorvisualizer", Object::HitErrorVisualizer),
    ("timingvisualizer", Object::TimingVisualizer),
    ("timingdistributiongraph", Object::TimingDistribution),
    ("gauge", Object::Gauge),
    ("hiddenCover", Object::HiddenCover),
    ("liftCover", Object::LiftCover),
    ("practice", Object::Practice),
    ("bga", Object::Bga),
    ("judge", Object::Judge),
    ("pmchara", Object::PmChara),
    ("note", Object::Note),
    ("songlist", Object::SongList),
];

/// Whether a `kind` screen's loader looks up entry `slot` of [`ORDER`].
fn looked_up(kind: ScreenKind, slot: usize) -> bool {
    match kind {
        ScreenKind::Selection => slot < COMMON || slot == SONG_LIST,
        // The result and decision loaders add no object of their own
        // (`JsonResultSkinObjectLoader.java`,
        // `JsonDecideSkinObjectLoader.java`).
        ScreenKind::Result | ScreenKind::Decision => slot < COMMON,
        ScreenKind::Play => slot < SONG_LIST,
    }
}

/// The records of a table: an array field's, or a record field's one.
pub(crate) fn records<'t>(tables: &'t Tables, name: &str) -> &'t [u32] {
    if let Some(i) = ARRAYS.iter().position(|a| *a == name) {
        return tables.arrays.get(i).map_or(&[], Vec::as_slice);
    }
    if let Some(i) = RECORDS.iter().position(|r| *r == name) {
        return tables
            .records
            .get(i)
            .and_then(Option::as_ref)
            .map_or(&[], std::slice::from_ref);
    }
    &[]
}

/// A record's `id`, decoded; `None` when it has none.
pub(crate) fn id_of(def: &mut Definition, rec: u32) -> Result<Option<String>, ImportError> {
    match def.fields(rec, ["id"]) {
        [Some(f)] => def.string(f, "id"),
        _ => Ok(None),
    }
}

/// An id to record map over `recs`; with `last`, the last record of an
/// id wins, else the first.
pub(crate) fn index(
    def: &mut Definition,
    recs: &[u32],
    last: bool,
) -> Result<HashMap<String, u32>, ImportError> {
    let mut map = HashMap::with_capacity(recs.len());
    for &rec in recs {
        if let Some(id) = id_of(def, rec)? {
            if last {
                map.insert(id, rec);
            } else {
                map.entry(id).or_insert(rec);
            }
        }
    }
    Ok(map)
}

/// The lookup tables, each built when the walk first needs it.
#[derive(Debug, Default)]
pub(crate) struct Objects {
    maps: [Option<HashMap<String, u32>>; ORDER.len()],
}

impl Objects {
    /// The object destination id `id` names on a `kind` screen: the kind
    /// and its record, from the first table the screen looks up that has
    /// a record of the id.
    pub(crate) fn lookup(
        &mut self,
        def: &mut Definition,
        tables: &Tables,
        id: &str,
        kind: ScreenKind,
    ) -> Result<Option<(Object, u32)>, ImportError> {
        for (i, (slot, &(name, object))) in self.maps.iter_mut().zip(&ORDER).enumerate() {
            if !looked_up(kind, i) {
                continue;
            }
            if slot.is_none() {
                *slot = Some(index(def, records(tables, name), false)?);
            }
            if let Some(&rec) = slot.as_ref().and_then(|m| m.get(id)) {
                return Ok(Some((object, rec)));
            }
        }
        Ok(None)
    }

    /// Whether `object`'s table has a record of id `id`. Unlike
    /// [`Self::lookup`] it reports no record's error: a table that does
    /// not index (an `id` that is an object, an array or a Lua float, or
    /// the decoded-byte bound) is scanned record by record, uncached, its
    /// error left to the lookup that reaches that table.
    pub(crate) fn holds(
        &mut self,
        def: &mut Definition,
        tables: &Tables,
        id: &str,
        object: Object,
    ) -> bool {
        let Some((slot, &(name, _))) = ORDER.iter().enumerate().find(|(_, (_, o))| *o == object)
        else {
            return false;
        };
        let Some(map) = self.maps.get_mut(slot) else {
            return false;
        };
        if map.is_none() {
            match index(def, records(tables, name), false) {
                Ok(m) => *map = Some(m),
                Err(_) => {
                    return records(tables, name)
                        .iter()
                        .any(|&rec| matches!(id_of(def, rec), Ok(Some(s)) if s == id));
                }
            }
        }
        map.as_ref().is_some_and(|m| m.contains_key(id))
    }

    /// Whether a table a `kind` screen looks up after `first`'s has a
    /// record of id `id` ([`Self::holds`], no record's error reported).
    pub(crate) fn held_after(
        &mut self,
        def: &mut Definition,
        tables: &Tables,
        id: &str,
        kind: ScreenKind,
        first: Object,
    ) -> bool {
        let Some(start) = ORDER.iter().position(|&(_, o)| o == first) else {
            return false;
        };
        ORDER
            .iter()
            .enumerate()
            .skip(start + 1)
            .filter(|&(slot, _)| looked_up(kind, slot))
            .any(|(_, &(_, object))| self.holds(def, tables, id, object))
    }
}

/// The table name of `object`'s records, `None` for an object no table
/// holds (a built-in image, a movie).
pub(crate) fn table_name(object: Object) -> Option<&'static str> {
    ORDER
        .iter()
        .find(|&&(_, o)| o == object)
        .map(|&(name, _)| name)
}
