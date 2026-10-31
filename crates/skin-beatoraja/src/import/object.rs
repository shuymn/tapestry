//! Object tables by id, built on first use, in the source's lookup order
//! (`JsonSkinObjectLoader.java:38-~560`, `PlaySkinObjectLoader.java` at
//! `0.8.9`): the first record of an id wins in each table, and the first
//! table that has the id wins. The play loader sets `note` and keeps
//! looking, so a later play object of the same id overrides it
//! (`JsonPlaySkinObjectLoader.java:37-181`): `note` is looked up last.

use std::collections::HashMap;

use super::report::Object;
use crate::error::ImportError;
use crate::resolve::{ARRAYS, Definition, RECORDS, Tables};

/// The destination lookup order, by table name (an array field's or a
/// record field's, [`records`]).
const ORDER: [(&str, Object); 21] = [
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
];

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
    /// The object destination id `id` names: the kind and its record.
    pub(crate) fn lookup(
        &mut self,
        def: &mut Definition,
        tables: &Tables,
        id: &str,
    ) -> Result<Option<(Object, u32)>, ImportError> {
        for (slot, &(name, object)) in self.maps.iter_mut().zip(&ORDER) {
            if slot.is_none() {
                *slot = Some(index(def, records(tables, name), false)?);
            }
            if let Some(&rec) = slot.as_ref().and_then(|m| m.get(id)) {
                return Ok(Some((object, rec)));
            }
        }
        Ok(None)
    }

    /// Whether the `note` record has id `id`.
    pub(crate) fn is_note(
        &mut self,
        def: &mut Definition,
        tables: &Tables,
        id: &str,
    ) -> Result<bool, ImportError> {
        let slot = ORDER.len() - 1;
        if let Some(m) = self.maps.get_mut(slot) {
            if m.is_none() {
                *m = Some(index(def, records(tables, "note"), false)?);
            }
            return Ok(m.as_ref().is_some_and(|m| m.contains_key(id)));
        }
        Ok(false)
    }
}
