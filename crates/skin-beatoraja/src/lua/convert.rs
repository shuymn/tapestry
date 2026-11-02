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
//! table is neither: [`Kind::Empty`], which the definition's readers read
//! as the empty array or the empty object their field expects. A number keeps its subtype and value: an integer
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

/// The fields a callable's location names; any other is `other`. The
/// format's own field names, never a skin's.
pub(crate) const FIELDS: [&str; 24] = [
    "draw",
    "value",
    "timer",
    "op",
    "act",
    "event",
    "ref",
    "text",
    "id",
    "src",
    "x",
    "y",
    "w",
    "h",
    "a",
    "r",
    "g",
    "b",
    "angle",
    "loop",
    "blend",
    "isRefNum",
    "mouseRect",
    "customTimers",
];

/// The functions a converted table holds, in conversion order: each
/// node, the field holding it ([`FIELDS`]), and what it is.
pub(crate) fn callables(
    arena: &Arena,
) -> impl Iterator<Item = (u32, &'static str, CallableKind)> + '_ {
    let lua = arena.lua.as_deref();
    (0_u32..).zip(&arena.nodes).filter_map(move |(id, n)| {
        let lua = lua?;
        if n.kind != Kind::Lua {
            return None;
        }
        let LuaScalar::Callable(i) = *lua.values.get(n.end as usize)? else {
            return None;
        };
        let kind = lua.callables.get(i as usize)?.kind;
        let key: String = arena.key(id).map(Iterator::collect).unwrap_or_default();
        let field = FIELDS
            .iter()
            .find(|f| **f == key)
            .copied()
            .unwrap_or("other");
        Some((id, field, kind))
    })
}

/// Refuses a converted table that holds a function, at the first one's
/// definition: until per-frame functions are analysed (S7-03), no
/// function is accepted, frozen or given a fallback, wherever it is.
pub(crate) fn refuse_callables(arena: &Arena) -> Result<(), ImportError> {
    match callables(arena).next() {
        Some((id, field, _)) => Err(ImportError::Lua {
            at: Some(arena.loc(id)),
            why: LuaWhy::Callable { field },
        }),
        None => Ok(()),
    }
}

/// The top-level fields the header pass reads ([`crate::resolve`]): the
/// type, the size, the name and author, and the options, file choices and
/// offsets, with all they hold.
const HEADER_FIELDS: [&str; 8] = [
    "type", "w", "h", "name", "author", "property", "filepath", "offset",
];

/// Refuses the header pass's table when a field the header pass reads
/// ([`HEADER_FIELDS`]) holds a function, at the first one's definition. A
/// function elsewhere passes: the header pass never reads it, and the
/// configured pass refuses it ([`refuse_callables`]), so the report sees
/// what else the configured pass meets.
pub(crate) fn refuse_header_callables(arena: &Arena, top: u32) -> Result<(), ImportError> {
    let mut stack: Vec<u32> = arena
        .children(top)
        .filter(|&c| {
            let key: String = arena.key(c).map(Iterator::collect).unwrap_or_default();
            HEADER_FIELDS.contains(&key.as_str())
        })
        .collect();
    let mut first: Option<u32> = None;
    while let Some(id) = stack.pop() {
        stack.extend(arena.children(id));
        let is_callable = arena.node(id).kind == Kind::Lua
            && matches!(arena.scalar(id), crate::tree::Scalar::Callable);
        if is_callable && first.is_none_or(|f| id < f) {
            first = Some(id);
        }
    }
    match first.and_then(|id| callables(arena).find(|&(c, ..)| c == id)) {
        Some((id, field, _)) => Err(ImportError::Lua {
            at: Some(arena.loc(id)),
            why: LuaWhy::Callable { field },
        }),
        None => Ok(()),
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
    let top = c.table(&t, None, 1)?;
    #[cfg(test)]
    super::tests::converted_witness();
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

    /// Value `v`, its key's span when it has one, at
    /// `depth`.
    fn value(&mut self, v: &Value, span: Option<Span>, depth: u32) -> Result<u32, ImportError> {
        match v {
            Value::Table(t) => self.table(t, span, depth),
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

    /// Table `t`, its key's span when it has one, at `depth`.
    fn table(&mut self, t: &Table, span: Option<Span>, depth: u32) -> Result<u32, ImportError> {
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
        let kind = if n == 0 {
            Kind::Empty
        } else if strings == n {
            Kind::Object
        } else if ints == n {
            let count = i64::try_from(n).unwrap_or(i64::MAX);
            if min != 1 || max != count {
                return Err(self.lua(LuaWhy::Sparse));
            }
            Kind::Array
        } else {
            return Err(self.lua(LuaWhy::Mixed));
        };
        let id = self.node(kind, span, 0)?;
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
        if kind == Kind::Array {
            for i in 1..=n {
                let v: Value = t.raw_get(i).map_err(|e| self.mlua(&e))?;
                let child = self.value(&v, None, depth + 1)?;
                link(self, child);
            }
        } else if kind == Kind::Object {
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
                let child = self.value(&v, Some(span), depth + 1)?;
                link(self, child);
            }
        }
        self.path.pop();
        Ok(id)
    }
}
