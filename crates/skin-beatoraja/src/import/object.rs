//! Object tables by id, built on first use, in the source's lookup order
//! (`JsonSkinObjectLoader.java:38-~560`, `PlaySkinObjectLoader.java` at
//! `0.8.9`): the first record of an id wins in each table, and the first
//! table that has the id wins.

use std::collections::HashMap;

use super::report::Object;
use crate::error::ImportError;
use crate::resolve::{ARRAYS, Definition, RECORDS, Tables};

/// Where an object table comes from.
#[derive(Clone, Copy, Debug)]
enum From {
    Array(&'static str),
    Record(&'static str),
}

/// The destination lookup order.
const ORDER: [(From, Object); 21] = [
    (From::Array("image"), Object::Image),
    (From::Array("imageset"), Object::ImageSet),
    (From::Array("value"), Object::Value),
    (From::Array("floatvalue"), Object::FloatValue),
    (From::Array("text"), Object::Text),
    (From::Array("slider"), Object::Slider),
    (From::Array("graph"), Object::Graph),
    (From::Array("gaugegraph"), Object::GaugeGraph),
    (From::Array("judgegraph"), Object::JudgeGraph),
    (From::Array("bpmgraph"), Object::BpmGraph),
    (
        From::Array("hiterrorvisualizer"),
        Object::HitErrorVisualizer,
    ),
    (From::Array("timingvisualizer"), Object::TimingVisualizer),
    (
        From::Array("timingdistributiongraph"),
        Object::TimingDistribution,
    ),
    (From::Record("gauge"), Object::Gauge),
    (From::Record("note"), Object::Note),
    (From::Array("hiddenCover"), Object::HiddenCover),
    (From::Array("liftCover"), Object::LiftCover),
    (From::Record("practice"), Object::Practice),
    (From::Record("bga"), Object::Bga),
    (From::Array("judge"), Object::Judge),
    (From::Array("pmchara"), Object::PmChara),
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
        for (slot, &(from, object)) in self.maps.iter_mut().zip(&ORDER) {
            if slot.is_none() {
                let name = match from {
                    From::Array(n) | From::Record(n) => n,
                };
                *slot = Some(index(def, records(tables, name), false)?);
            }
            if let Some(&rec) = slot.as_ref().and_then(|m| m.get(id)) {
                return Ok(Some((object, rec)));
            }
        }
        Ok(None)
    }
}
