//! The returned table converted into the resolved representation: an
//! [`Arena`] whose file 0 is the table, with typed Lua scalars, typed
//! callables and their provenance ([`crate::tree::LuaText`]).
//!
//! Tables are read raw: entries by `next`, values by `rawget`, never a
//! metamethod. A table whose keys are exactly the integers `1..=n` is an
//! array, one whose keys are all strings an object, its members sorted by
//! their bytes (a Lua table has no order of its own); mixed, sparse and
//! other keys are refused, as is a table met again inside itself. A table
//! reached twice is converted, and charged, at each place. An empty
//! table takes the kind its place expects: an array for the fields that
//! hold arrays ([`TOP_ARRAYS`], [`NESTED_ARRAYS`], a condition's items),
//! an object otherwise. A number keeps its subtype and value: an integer
//! as its `i64`, a float as its `f64`, never through text. A string must
//! be UTF-8. A function becomes a typed callable, each instance once; any
//! other value is refused.
//!
//! The converted structure takes the parsed definition's bounds, each
//! checked before the value is added: [`MAX_DEPTH`], [`MAX_NODES`] (a
//! table's entries all charged before any is read), [`MAX_STRING_BYTES`]
//! and [`MAX_DECODED_BYTES`] for its keys and strings. It holds at most
//! [`MAX_CONVERTED_BYTES`].

use std::collections::HashMap;

use mlua::{Table, Value};
use skin_model::FileId;

use super::{LuaWhy, Pass};
use crate::error::{ImportError, InputLimit, Loc};
use crate::limits::{MAX_DECODED_BYTES, MAX_DEPTH, MAX_NODES, MAX_STRING_BYTES};
use crate::resolve::ARRAYS;
use crate::tree::{
    Arena, Callable, CallableKind, KEYED, Kind, LuaScalar, LuaText, NONE, Node, Text,
};

/// How many values are converted between two looks at the cancellation
/// request.
pub(crate) const CHECK_EVERY: usize = 4_096;

/// The most bytes a converted table holds: a node, a scalar and a
/// callable for every node, and the decoded bytes.
pub const MAX_CONVERTED_BYTES: u64 = MAX_NODES as u64
    * (size_of::<Node>() + size_of::<LuaScalar>() + size_of::<Callable>()) as u64
    + MAX_DECODED_BYTES;

/// The top-level fields besides [`ARRAYS`] that hold arrays: the
/// header's.
const HEADER_ARRAYS: [&str; 3] = ["property", "filepath", "offset"];

/// The nested fields that hold arrays: destinations' and wrappers' lists,
/// the play objects' lists, the note parts and the literal arrays.
const NESTED_ARRAYS: [&str; 34] = [
    "dst",
    "op",
    "offsets",
    "offset",
    "item",
    "values",
    "if",
    "group",
    "bpm",
    "stop",
    "time",
    "images",
    "numbers",
    "nodes",
    "expansionrate",
    "size",
    "fallback",
    "note",
    "lnstart",
    "lnend",
    "lnbody",
    "lnbodyActive",
    "lnactive",
    "hcnstart",
    "hcnend",
    "hcnbody",
    "hcnactive",
    "hcnbodyActive",
    "hcndamage",
    "hcnbodyMiss",
    "hcnreactive",
    "hcnbodyReactive",
    "mine",
    "hidden",
];

/// Whether an empty table under `key` at `depth` (the root at 1), or an
/// item of an array under `parent`, holds an array.
fn empty_is_array(key: Option<&[u8]>, parent: Option<&[u8]>, depth: u32) -> bool {
    let is = |list: &[&str], k: &[u8]| list.iter().any(|f| f.as_bytes() == k);
    match key {
        Some(k) if depth == 2 => is(&ARRAYS, k) || is(&HEADER_ARRAYS, k),
        Some(k) => is(&NESTED_ARRAYS, k) || k == b"processed",
        None => depth > 1 && parent == Some(b"if"),
    }
}

/// Converts `value`, what the entry of `entry` returned, with `modules`
/// the files the pass loaded (the entry first).
pub(super) fn convert(
    pass: &Pass,
    value: Value,
    entry: &FileId,
    modules: Vec<String>,
) -> Result<(Arena, u32), ImportError> {
    #[cfg(test)]
    super::tests::conversion_witness();
    let Value::Table(t) = value else {
        return Err(ImportError::Lua {
            at: Some(Loc {
                file: entry.name().to_owned(),
                line: 0,
                col: 0,
            }),
            why: LuaWhy::Returned,
        });
    };
    let mut c = Conv {
        pass,
        entry: entry.name(),
        modules: &modules,
        nodes: Vec::new(),
        bytes: Vec::new(),
        values: Vec::new(),
        callables: Vec::new(),
        seen: HashMap::new(),
        path: Vec::new(),
        decoded: 0,
    };
    let top = c.table(&t, None, None, 1)?;
    let Conv {
        nodes,
        bytes,
        values,
        callables,
        ..
    } = c;
    let name = entry.name().to_owned();
    let dir = name
        .rsplit_once('/')
        .map_or(String::new(), |(d, _)| d.to_owned());
    let mut arena = Arena::new();
    arena.nodes = nodes;
    arena.files.push(Text {
        name,
        dir,
        bytes: bytes.into_boxed_slice(),
    });
    arena.lua = Some(Box::new(LuaText {
        values,
        modules,
        callables,
    }));
    Ok((arena, top))
}

/// A span of the converted bytes.
type Span = (u32, u32);

struct Conv<'a> {
    pass: &'a Pass,
    entry: &'a str,
    modules: &'a [String],
    nodes: Vec<Node>,
    bytes: Vec<u8>,
    values: Vec<LuaScalar>,
    callables: Vec<Callable>,
    /// Callables by the function's address.
    seen: HashMap<usize, u32>,
    /// The tables on the path from the root, by address.
    path: Vec<usize>,
    decoded: u64,
}

impl Conv<'_> {
    /// Where the next node goes: the entry, line 0, its ordinal.
    fn here(&self) -> Loc {
        Loc {
            file: self.entry.to_owned(),
            line: 0,
            col: u32::try_from(self.nodes.len().saturating_add(1)).unwrap_or(u32::MAX),
        }
    }

    fn lua(&self, why: LuaWhy) -> ImportError {
        ImportError::Lua {
            at: Some(self.here()),
            why,
        }
    }

    /// A failure of the VM while it is read: an allocation, or a key the
    /// table does not hold as listed.
    fn mlua(&self, e: &mlua::Error) -> ImportError {
        match e {
            mlua::Error::MemoryError(_) => self.lua(LuaWhy::Limit(super::LuaLimit::Memory)),
            _ => self.lua(LuaWhy::Key),
        }
    }

    fn input(&self, limit: InputLimit) -> ImportError {
        ImportError::Input {
            at: Some(self.here()),
            limit,
        }
    }

    /// Copies `s` into the bytes, charged first: its span.
    fn text(&mut self, s: &[u8]) -> Result<(u32, u32), ImportError> {
        if s.len() > MAX_STRING_BYTES {
            return Err(self.input(InputLimit::StringBytes));
        }
        let len = u64::try_from(s.len()).unwrap_or(u64::MAX);
        if self.decoded.saturating_add(len) > MAX_DECODED_BYTES {
            return Err(self.input(InputLimit::DecodedBytes));
        }
        if std::str::from_utf8(s).is_err() {
            return Err(self.lua(LuaWhy::NotUtf8));
        }
        self.decoded += len;
        let start =
            u32::try_from(self.bytes.len()).map_err(|_| self.input(InputLimit::DecodedBytes))?;
        self.bytes.extend_from_slice(s);
        let end =
            u32::try_from(self.bytes.len()).map_err(|_| self.input(InputLimit::DecodedBytes))?;
        Ok((start, end))
    }

    /// Adds a node, counted first; its key's span, if any.
    fn node(&mut self, kind: Kind, key: Option<(u32, u32)>, end: u32) -> Result<u32, ImportError> {
        if self.nodes.len() >= MAX_NODES {
            return Err(self.input(InputLimit::Nodes));
        }
        if self.nodes.len().is_multiple_of(CHECK_EVERY) && self.pass.ledger.cancelled() {
            return Err(self.lua(LuaWhy::Cancelled));
        }
        let id = u32::try_from(self.nodes.len()).map_err(|_| self.input(InputLimit::Nodes))?;
        let (key_start, key_end) = key.unwrap_or((0, 0));
        self.nodes.push(Node {
            start: id,
            end,
            key_start,
            key_end,
            first_child: NONE,
            next_sibling: NONE,
            file: 0,
            kind,
            flags: if key.is_some() { KEYED } else { 0 },
        });
        Ok(id)
    }

    /// A scalar node.
    fn scalar(&mut self, v: LuaScalar, key: Option<(u32, u32)>) -> Result<u32, ImportError> {
        let index = u32::try_from(self.values.len()).map_err(|_| self.input(InputLimit::Nodes))?;
        let id = self.node(Kind::Lua, key, index)?;
        self.values.push(v);
        Ok(id)
    }

    /// Value `v` under `key` (its name, and its span once copied), an
    /// item of an array under `parent` when it has no key, at `depth`.
    fn value(
        &mut self,
        v: &Value,
        key: Option<(&[u8], (u32, u32))>,
        parent: Option<&[u8]>,
        depth: u32,
    ) -> Result<u32, ImportError> {
        let span = key.map(|(_, s)| s);
        let name = key.map(|(k, _)| k);
        match v {
            Value::Table(t) => self.table(t, name.map(|n| (n, span)), parent, depth),
            Value::Boolean(b) => self.scalar(LuaScalar::Bool(*b), span),
            Value::Integer(n) => self.scalar(LuaScalar::Int(*n), span),
            Value::Number(n) => self.scalar(LuaScalar::Float(*n), span),
            Value::String(s) => {
                let (start, end) = self.text(&s.as_bytes())?;
                self.scalar(LuaScalar::Str { start, end }, span)
            }
            Value::Function(f) => {
                let addr = f.to_pointer().addr();
                let index = match self.seen.get(&addr) {
                    Some(&i) => i,
                    None => {
                        let callable = self.callable(f, addr);
                        let i = u32::try_from(self.callables.len())
                            .map_err(|_| self.input(InputLimit::Nodes))?;
                        self.callables.push(callable);
                        self.seen.insert(addr, i);
                        i
                    }
                };
                self.scalar(LuaScalar::Callable(index), span)
            }
            _ => Err(self.lua(LuaWhy::Value)),
        }
    }

    /// What function `f`, at `addr`, is and where it was defined.
    fn callable(&self, f: &mlua::Function, addr: usize) -> Callable {
        if let Some(api) = self.pass.host_api_of(addr) {
            return Callable {
                module: None,
                line: 0,
                kind: CallableKind::Host(api),
            };
        }
        let info = f.info();
        if info.what == "C" {
            return Callable {
                module: None,
                line: 0,
                kind: CallableKind::Library,
            };
        }
        let module = info
            .source
            .as_deref()
            .and_then(|s| s.strip_prefix('@'))
            .and_then(|s| self.modules.iter().position(|m| m == s))
            .and_then(|i| u16::try_from(i).ok());
        Callable {
            module,
            line: info
                .line_defined
                .and_then(|l| u32::try_from(l).ok())
                .unwrap_or(0),
            kind: CallableKind::Lua,
        }
    }

    /// Table `t` under `key` (its name and span), an item under `parent`
    /// when it has none, at `depth`.
    fn table(
        &mut self,
        t: &Table,
        key: Option<(&[u8], Option<Span>)>,
        parent: Option<&[u8]>,
        depth: u32,
    ) -> Result<u32, ImportError> {
        if depth > MAX_DEPTH {
            return Err(self.input(InputLimit::Depth));
        }
        let addr = t.to_pointer().addr();
        if self.path.contains(&addr) {
            return Err(self.lua(LuaWhy::Cycle));
        }
        // The keys classified and counted before anything is kept.
        let (mut n, mut strings, mut ints) = (0_usize, 0_usize, 0_usize);
        let (mut min, mut max) = (i64::MAX, i64::MIN);
        let mut key_bytes = 0_u64;
        for pair in t.pairs::<Value, Value>() {
            let (k, _) = pair.map_err(|e| self.mlua(&e))?;
            n += 1;
            match k {
                Value::String(s) => {
                    strings += 1;
                    let len = s.as_bytes().len();
                    if len > MAX_STRING_BYTES {
                        return Err(self.input(InputLimit::StringBytes));
                    }
                    key_bytes = key_bytes.saturating_add(u64::try_from(len).unwrap_or(u64::MAX));
                }
                Value::Integer(i) => {
                    ints += 1;
                    min = min.min(i);
                    max = max.max(i);
                }
                _ => return Err(self.lua(LuaWhy::Key)),
            }
        }
        if self.nodes.len().saturating_add(n) >= MAX_NODES {
            return Err(self.input(InputLimit::Nodes));
        }
        if self.decoded.saturating_add(key_bytes) > MAX_DECODED_BYTES {
            return Err(self.input(InputLimit::DecodedBytes));
        }
        let array = if n == 0 {
            empty_is_array(key.map(|(k, _)| k), parent, depth)
        } else if strings == n {
            false
        } else if ints == n {
            let count = i64::try_from(n).unwrap_or(i64::MAX);
            if min != 1 || max != count {
                return Err(self.lua(LuaWhy::Sparse));
            }
            true
        } else {
            return Err(self.lua(LuaWhy::Mixed));
        };
        let id = self.node(
            if array { Kind::Array } else { Kind::Object },
            key.and_then(|(_, s)| s),
            0,
        )?;
        self.path.push(addr);
        let mut prev = NONE;
        let mut link = |c: &mut Self, child: u32| {
            if prev == NONE {
                c.nodes[id as usize].first_child = child;
            } else {
                c.nodes[prev as usize].next_sibling = child;
            }
            prev = child;
        };
        let name = key.map(|(k, _)| k.to_vec());
        if array {
            for i in 1..=n {
                let v: Value = t.raw_get(i).map_err(|e| self.mlua(&e))?;
                let child = self.value(&v, None, name.as_deref(), depth + 1)?;
                link(self, child);
            }
        } else {
            // The keys' bytes, charged above with the entries, copied out
            // so that no handle per key stays on Lua's stack.
            let mut keys: Vec<Vec<u8>> = Vec::with_capacity(n);
            for pair in t.pairs::<mlua::LuaString, Value>() {
                let (k, _) = pair.map_err(|e| self.mlua(&e))?;
                keys.push(k.as_bytes().to_vec());
            }
            keys.sort_unstable();
            for k in keys {
                let span = self.text(&k)?;
                let v: Value = t
                    .raw_get(mlua::BString::from(k.clone()))
                    .map_err(|e| self.mlua(&e))?;
                let child = self.value(&v, Some((&k, span)), None, depth + 1)?;
                link(self, child);
            }
        }
        self.path.pop();
        Ok(id)
    }
}

#[cfg(test)]
mod tests {
    use super::empty_is_array;

    #[test]
    fn empty_tables_follow_their_place() {
        assert!(!empty_is_array(None, None, 1));
        assert!(empty_is_array(Some(b"destination"), None, 2));
        assert!(empty_is_array(Some(b"property"), None, 2));
        assert!(!empty_is_array(Some(b"note"), None, 2));
        assert!(empty_is_array(Some(b"note"), None, 3));
        assert!(empty_is_array(Some(b"dst"), None, 3));
        assert!(!empty_is_array(Some(b"mouseRect"), None, 3));
        assert!(empty_is_array(None, Some(b"if"), 4));
        assert!(!empty_is_array(None, Some(b"image"), 3));
    }
}
