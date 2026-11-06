//! The returned table converted into the resolved representation: an
//! [`Arena`] whose file 0 is the table, with typed Lua scalars, typed
//! callables and their provenance ([`crate::tree::LuaText`]).
//!
//! Tables are read raw: entries by `next`, values by `rawget`, never a
//! metamethod. A table whose keys are exactly the integers `1..=n` is an
//! array, one whose keys are all strings an object, its members sorted by
//! their bytes (a Lua table has no order of its own). One with string keys
//! and others is [`Kind::Mixed`], an object of its string keys as the
//! source's Lua loader reads it (`LuaSkinLoader.java:187-205` at `0.8.9`):
//! the other entries are never read (not converted, charged as nodes or
//! captured), the import notes it, and the definition's readers refuse
//! it where an array, a condition or a choice branch is read. Sparse
//! integer keys alone, and keys that are neither strings nor integers
//! without a string key, are refused, as is a table met again inside
//! itself. A table reached twice is converted, and charged, at each
//! place. An empty table is neither: [`Kind::Empty`], which the
//! definition's readers read as the empty array or the empty object their
//! field expects. A number keeps its subtype and value: an integer as its
//! `i64`, a float as its `f64`, never through text. A string must be
//! UTF-8. A function becomes a typed callable, each instance once; any
//! other value is refused.
//!
//! Each value is converted at its position in the source's schema
//! ([`super::schema`]). In the configured pass, a function at a property
//! field or an `op` entry is a root, marked when it is first met there
//! (one met before only where the source never reads it is inert until
//! then); a string at a property field its own name loader does not
//! resolve is a script, its bytes charged as analysis work before the
//! recognizer reads them. Each table's unread entries and metatable are
//! summarized once, with its shape, and the summary applied at each place
//! it is met: unread entries holding a function, a string or a table
//! where the source lists every entry, or a metatable on a font fallback,
//! like a script, open the analysis at the first such node
//! ([`super::callback::Analysis::open`]).
//!
//! In the configured pass, each root skin closure is looked up
//! by its tag ([`super::instrument`]) and its captures are copied
//! ([`super::callback`]): its reader is called (the sandbox's generated
//! code, which only reads its bindings) and each value kept as owned data:
//! a scalar, a string interned (its bytes hashed as analysis work and
//! kept against [`MAX_DECODED_BYTES`] once), a host function, a host
//! module or a library function by identity, another closure instance
//! (whose captures are copied in turn), or a table by identity with
//! whether it has a metatable, whether it is the pass's own
//! `skin_config` recorded. Then the entries on the function's static
//! access paths are read raw and kept the same way, each step one unit
//! of analysis work: no other entry of a table is read. Each instance,
//! value, table and entry is charged before it is kept
//! ([`MAX_CAPTURES`]).
//!
//! The converted structure takes the parsed definition's bounds, each
//! checked before the value is added: [`MAX_DEPTH`], [`MAX_NODES`] (a
//! table's entries all charged before any is read), [`MAX_STRING_BYTES`]
//! and [`MAX_DECODED_BYTES`] for its keys and strings. It holds at most
//! [`MAX_CONVERTED_BYTES`].

use std::collections::HashMap;
use std::rc::Rc;

use mlua::{Function, Lua, Table, Value};
use skin_model::FileId;

use super::callback::{self, Analysis, Captured, Instance};
use super::limits::MAX_CAPTURES;
use super::schema::{Pos, ROOT_SCHEMA};
use super::syntax::Ast;
use super::{LuaLimit, LuaWhy, Pass};
use crate::error::{ImportError, InputLimit, Loc};
use crate::limits::{MAX_DECODED_BYTES, MAX_DEPTH, MAX_NODES, MAX_STRING_BYTES};
use crate::tree::{
    Arena, Callable, CallableKind, KEYED, Kind, LuaScalar, LuaText, NONE, Node, Text,
};

/// The tables the configured pass's captures are proven against.
struct Tags {
    ids: Table,
    readers: Table,
    /// The sandbox's library functions, to their index in
    /// [`super::sandbox::LIBS`].
    libs: Table,
    /// The provenance marker's records ([`super::instrument`]).
    marks: Table,
    /// The host module proxies: address, module, unchanged.
    proxies: Vec<(usize, u8, bool)>,
    config: Option<Table>,
}

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
#[cfg(feature = "dev-tools")]
pub(crate) const FIELDS: [&str; 34] = [
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
    "customEvents",
    "songlist",
    "liston",
    "listoff",
    "level",
    "lamp",
    "playerlamp",
    "rivallamp",
    "trophy",
    "label",
];

/// The functions a converted table holds, in conversion order: each
/// node, the field holding it ([`FIELDS`]), and what it is.
#[cfg(feature = "dev-tools")]
pub(crate) fn callables(
    arena: &Arena,
) -> impl Iterator<Item = (u32, &'static str, CallableKind)> + '_ {
    let lua = arena.lua.as_deref();
    callback::callable_nodes(arena).filter_map(move |(id, i)| {
        let kind = lua?.callables.get(i as usize)?.kind;
        Some((id, field(arena, id), kind))
    })
}

/// The field holding node `id` ([`FIELDS`]), or `other`.
#[cfg(feature = "dev-tools")]
pub(crate) fn field(arena: &Arena, id: u32) -> &'static str {
    let key: String = arena.key(id).map(Iterator::collect).unwrap_or_default();
    FIELDS
        .iter()
        .find(|f| **f == key)
        .copied()
        .unwrap_or("other")
}

/// Converts `value`, what the entry of `entry` returned, with `modules`
/// the files the pass loaded (the entry first), in `lua`, the pass's VM.
pub(super) fn convert(
    lua: &Lua,
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
    let tags = if pass.bridge.is_some() {
        let config = match lua.named_registry_value::<Value>(super::config::CONFIG) {
            Ok(Value::Table(t)) => Some(t),
            _ => None,
        };
        let r = (|| -> mlua::Result<Tags> {
            Ok(Tags {
                ids: lua.named_registry_value(super::sandbox::TAG_IDS)?,
                readers: lua.named_registry_value(super::sandbox::TAG_READERS)?,
                libs: lua.named_registry_value(super::sandbox::LIB_IDS)?,
                marks: lua.named_registry_value(super::sandbox::TAG_MARKS)?,
                proxies: super::host::proxies(lua)?,
                config,
            })
        })();
        Some(r.map_err(|e| ImportError::Lua {
            at: None,
            why: match e {
                mlua::Error::MemoryError(_) => LuaWhy::Limit(LuaLimit::Memory),
                _ => LuaWhy::Error,
            },
        })?)
    } else {
        None
    };
    let mut c = Conv {
        pass,
        tags,
        instance_of: HashMap::new(),
        pending: Vec::new(),
        table_of: HashMap::new(),
        shapes: HashMap::new(),
        module_index: (0_u16..)
            .zip(&modules)
            .map(|(i, m)| (m.as_str(), i))
            .collect(),
        analysis: Analysis::with_cancel(pass.ledger.cancel()),
        entry: entry.name(),
        nodes: Vec::new(),
        bytes: Vec::new(),
        values: Vec::new(),
        callables: Vec::new(),
        seen: HashMap::new(),
        path: Vec::new(),
        decoded: 0,
        retained: 0,
    };
    let top = c.table(&t, None, 1, Pos::Class(&ROOT_SCHEMA))?;
    // Closing the captured object graph is a phase of the analysis's
    // time budget.
    let clock = super::clock::Phase::Graph;
    c.analysis
        .clock
        .enter(clock, None)
        .map_err(|w| c.fatal(w))?;
    let r = c.capture();
    let left = c.analysis.clock.leave();
    r?;
    left.map_err(|w| c.fatal(w))?;
    #[cfg(test)]
    super::tests::converted_witness();
    let configured = c.tags.is_some();
    let Conv {
        nodes,
        bytes,
        values,
        callables,
        mut analysis,
        ..
    } = c;
    let analysis = configured.then(|| {
        analysis.files = std::mem::take(&mut *pass.files.borrow_mut());
        Box::new(analysis)
    });
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
        analysis,
    }));
    Ok((arena, top))
}

/// A span of the converted bytes.
type Span = (u32, u32);

/// A table's keys as [`Conv::enumerate`] read them.
#[derive(Clone, Debug)]
enum Shape {
    Empty,
    /// The keys `1..=n`.
    Array(usize),
    /// String keys, sorted by their bytes.
    Object(Rc<[Vec<u8>]>),
    /// String keys, sorted by their bytes, beside other keys left unread.
    Mixed(Rc<[Vec<u8>]>),
}

/// What a table holds that the conversion does not see, computed with its
/// [`Shape`] once a conversion by its identity, and applied at each place
/// it is met ([`Conv::table`]).
#[derive(Clone, Copy, Debug, Default)]
struct Summary {
    /// It is [`Shape::Mixed`] and an entry under a key that is not a
    /// string holds a function, a string or a table: what the source
    /// reads as an element where it lists every entry.
    unread: bool,
    /// It has a metatable.
    meta: bool,
}
struct Conv<'a> {
    pass: &'a Pass,
    tags: Option<Tags>,
    /// Closure instances by the function's address, and those whose
    /// captures are still to copy.
    instance_of: HashMap<usize, u32>,
    pending: Vec<(u32, Function)>,
    /// Captured tables, by address.
    table_of: HashMap<usize, u32>,
    /// Tables enumerated, by address.
    shapes: HashMap<usize, (Shape, Summary)>,
    /// The loaded modules' indices, by name: a callable's provenance is
    /// one hash of its source's name (at most 1 024 bytes).
    module_index: HashMap<&'a str, u16>,
    analysis: Analysis,
    entry: &'a str,
    nodes: Vec<Node>,
    bytes: Vec<u8>,
    values: Vec<LuaScalar>,
    callables: Vec<Callable>,
    /// Callables by the function's address.
    seen: HashMap<usize, u32>,
    /// The tables on the path from the root, by address.
    path: Vec<usize>,
    decoded: u64,
    /// Bytes kept beside the arena ([`Conv::retain`]).
    retained: u64,
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

    /// Value `v`, its key's span when it has one, at `depth` and at
    /// position `pos` of the source's schema.
    fn value(
        &mut self,
        v: &Value,
        span: Option<Span>,
        depth: u32,
        pos: Pos,
    ) -> Result<u32, ImportError> {
        match v {
            Value::Table(t) => self.table(t, span, depth, pos),
            Value::Boolean(b) => self.scalar(LuaScalar::Bool(*b), span),
            Value::Integer(n) => self.scalar(LuaScalar::Int(*n), span),
            Value::Number(n) => self.scalar(LuaScalar::Float(*n), span),
            Value::String(s) => {
                let (start, end) = self.text(&s.as_bytes())?;
                let id = self.scalar(LuaScalar::Str { start, end }, span)?;
                if let Pos::Prop(ser) = pos
                    && self.tags.is_some()
                {
                    self.script(id, ser, (start, end))?;
                }
                Ok(id)
            }
            Value::Function(f) => {
                let addr = f.to_pointer().addr();
                let index = match self.seen.get(&addr) {
                    Some(&i) => i,
                    None => {
                        let callable = self.callable(f, addr)?;
                        let i = u32::try_from(self.callables.len())
                            .map_err(|_| self.input(InputLimit::Nodes))?;
                        self.callables.push(callable);
                        self.seen.insert(addr, i);
                        i
                    }
                };
                if matches!(pos, Pos::Prop(_)) {
                    self.root(index, f)?;
                }
                self.scalar(LuaScalar::Callable(index), span)
            }
            _ => Err(self.lua(LuaWhy::Value)),
        }
    }

    /// Marks callable `i`, function `f`, as a root the first time it is
    /// met at a property field or an `op` entry, a skin closure tagged
    /// then: one met first where the source never reads it is inert until
    /// then.
    fn root(&mut self, i: u32, f: &Function) -> Result<(), ImportError> {
        let Some(&c) = self.callables.get(i as usize) else {
            return Ok(());
        };
        if c.root {
            return Ok(());
        }
        let instance = if c.kind == CallableKind::Lua {
            self.tagged(f)?.unwrap_or(NONE)
        } else {
            NONE
        };
        if let Some(c) = self.callables.get_mut(i as usize) {
            c.root = true;
            c.instance = instance;
        }
        Ok(())
    }

    /// The string at node `id`, span `text`, at a property field of
    /// serializer `ser`: a script root when the field's name loader does
    /// not resolve it, its bytes charged as analysis work before the
    /// recognizer reads them.
    fn script(
        &mut self,
        id: u32,
        ser: super::schema::Serializer,
        (start, end): Span,
    ) -> Result<(), ImportError> {
        let len = u64::from(end.saturating_sub(start));
        self.analysis
            .charge(len.saturating_add(1))
            .map_err(|w| self.fatal(w))?;
        let name = self
            .bytes
            .get(start as usize..end as usize)
            .and_then(|b| std::str::from_utf8(b).ok())
            .unwrap_or_default();
        if !ser.resolves(name) {
            #[cfg(any(test, feature = "dev-tools"))]
            {
                self.analysis.scripts = self.analysis.scripts.saturating_add(1);
            }
            self.open(id, super::Unsupported::Construct("a script"));
        }
        Ok(())
    }

    /// Records node `id` as the open site, for `why`, unless one came
    /// before it.
    fn open(&mut self, id: u32, why: super::Unsupported) {
        if self.analysis.open.get().is_none() {
            self.analysis.open.set(Some(callback::Open {
                site: callback::Site::Node(id),
                why,
            }));
        }
    }

    /// What function `f`, at `addr`, is and where it was defined.
    fn callable(&mut self, f: &mlua::Function, addr: usize) -> Result<Callable, ImportError> {
        if let Some(api) = self.pass.host_api_of(addr) {
            return Ok(Callable {
                module: None,
                line: 0,
                kind: CallableKind::Host(api),
                instance: NONE,
                root: false,
            });
        }
        let info = f.info();
        // The sandbox's own Lua (its chunks are named `=`) is library.
        let ours = info.source.as_deref().is_some_and(|s| s.starts_with('='));
        if info.what == "C" || ours {
            return Ok(Callable {
                module: None,
                line: 0,
                kind: CallableKind::Library,
                instance: NONE,
                root: false,
            });
        }
        let source = info.source.as_deref().and_then(|s| s.strip_prefix('@'));
        // One hash of the source's name, its bytes charged first.
        if let Some(s) = source {
            let len = u64::try_from(s.len()).unwrap_or(u64::MAX);
            self.analysis.charge(len).map_err(|w| self.fatal(w))?;
        }
        let module = source.and_then(|s| self.module_index.get(s).copied());
        Ok(Callable {
            module,
            line: info
                .line_defined
                .and_then(|l| u32::try_from(l).ok())
                .unwrap_or(0),
            kind: CallableKind::Lua,
            instance: NONE,
            root: false,
        })
    }

    /// The cancellation request, seen every [`CHECK_EVERY`] entries a
    /// loop over a table visits.
    fn poll(&self, seen: usize) -> Result<(), ImportError> {
        if seen.is_multiple_of(CHECK_EVERY) && seen > 0 && self.pass.ledger.cancelled() {
            return Err(self.lua(LuaWhy::Cancelled));
        }
        Ok(())
    }

    /// One capture charged before it is kept.
    fn charge(&self) -> Result<(), ImportError> {
        let n = self.pass.ledger.counts().captures;
        if n >= MAX_CAPTURES {
            return Err(self.lua(LuaWhy::Limit(LuaLimit::Captures)));
        }
        self.pass.ledger.update(|c| c.captures = n + 1);
        Ok(())
    }

    /// The instance of skin closure `f` when it is tagged: found, or
    /// added with its captures pending.
    fn tagged(&mut self, f: &Function) -> Result<Option<u32>, ImportError> {
        let Some(tags) = &self.tags else {
            return Ok(None);
        };
        let addr = f.to_pointer().addr();
        if let Some(&i) = self.instance_of.get(&addr) {
            return Ok(Some(i));
        }
        let id = match tags.ids.raw_get::<Value>(f.clone()) {
            Ok(Value::Integer(id)) => id,
            Ok(_) => return Ok(None),
            Err(e) => return Err(self.mlua(&e)),
        };
        self.charge()?;
        let (file, func) = (u16::try_from(id >> 32), u32::try_from(id & 0xffff_ffff));
        let (Ok(file), Ok(func)) = (file, func) else {
            return Ok(None);
        };
        let i = u32::try_from(self.analysis.instances.len())
            .map_err(|_| self.lua(LuaWhy::Limit(LuaLimit::Captures)))?;
        self.analysis.instances.push(Instance {
            file,
            func,
            values: None,
        });
        self.instance_of.insert(addr, i);
        self.pending.push((i, f.clone()));
        Ok(Some(i))
    }

    /// Copies the captures of every pending instance, and of the
    /// instances they capture.
    fn capture(&mut self) -> Result<(), ImportError> {
        #[cfg(test)]
        super::tests::capturing(true);
        let r = self.capture_pending();
        #[cfg(test)]
        super::tests::capturing(false);
        r?;
        self.reached();
        self.marks()
    }

    /// Copies the provenance marker's records, each charged as a capture
    /// before it is kept, and whether the host module's proxy is as the
    /// pass built it.
    fn marks(&mut self) -> Result<(), ImportError> {
        let Some(tags) = &self.tags else {
            return Ok(());
        };
        self.analysis.module_intact = tags
            .proxies
            .iter()
            .any(|&(_, m, intact)| m == callback::MAIN_STATE && intact);
        let marks = tags.marks.clone();
        for (seen, pair) in marks.pairs::<Value, Value>().enumerate() {
            self.poll(seen)?;
            let (k, v) = pair.map_err(|e| self.mlua(&e))?;
            // A test can spend the capture count first, so the real check
            // refuses; the site records what the real charge added.
            #[cfg(test)]
            let before = {
                if super::tests::fail_at("mark copy") {
                    self.pass.ledger.update(|c| c.captures = MAX_CAPTURES);
                }
                self.pass.ledger.counts().captures
            };
            self.charge()?;
            #[cfg(test)]
            {
                let after = self.pass.ledger.counts().captures;
                super::tests::site_charged("mark copy", after.saturating_sub(before));
                super::tests::site_did("mark copy");
            }
            if let (Value::Integer(k), Value::Boolean(v)) = (k, v) {
                self.analysis.marks.insert(k.cast_unsigned(), v);
            }
        }
        Ok(())
    }

    /// Gives each callable that is no root but that a root captured, so
    /// that the source calls it through that root, its instance: one
    /// hash a callable, each already charged as a node.
    fn reached(&mut self) {
        for (addr, &i) in &self.seen {
            if let Some(c) = self.callables.get_mut(i as usize)
                && !c.root
                && let Some(&inst) = self.instance_of.get(addr)
            {
                c.instance = inst;
            }
        }
    }

    fn capture_pending(&mut self) -> Result<(), ImportError> {
        while let Some((i, f)) = self.pending.pop() {
            if self.pass.ledger.cancelled() {
                return Err(self.lua(LuaWhy::Cancelled));
            }
            let Some(tags) = &self.tags else {
                return Ok(());
            };
            let reader = match tags.readers.raw_get::<Value>(f) {
                Ok(Value::Function(r)) => r,
                Ok(_) => continue,
                Err(e) => return Err(self.mlua(&e)),
            };
            // The reader is generated code that returns its bindings:
            // reading a binding runs no metamethod.
            let values = reader
                .call::<mlua::MultiValue>(())
                .map_err(|e| self.mlua(&e))?
                .into_vec();
            let Some(inst) = self.analysis.instances.get(i as usize).copied() else {
                continue;
            };
            let pass = self.pass;
            let files = pass.files.borrow();
            let Some(ast) = files.get(usize::from(inst.file)).map(|f| &f.ast) else {
                continue;
            };
            let Some(func) = ast.func(inst.func) else {
                continue;
            };
            // Each binding's position, hashed once a function.
            for (k, &up) in (0_u32..).zip(ast.list(func.upvalues)) {
                self.analysis
                    .slots
                    .entry((inst.file, inst.func, up))
                    .or_insert(k);
            }
            let start = u32::try_from(self.analysis.captured.len())
                .map_err(|_| self.lua(LuaWhy::Limit(LuaLimit::Captures)))?;
            let mut n = 0_u32;
            for v in &values {
                self.charge()?;
                let c = self.captured(v)?;
                self.analysis.captured.push(c);
                n += 1;
            }
            if let Some(x) = self.analysis.instances.get_mut(i as usize) {
                x.values = Some((start, n));
            }
            self.paths(ast, inst, &values)?;
        }
        Ok(())
    }

    /// Copies the entries on instance `inst`'s static access paths
    /// ([`super::syntax::Path`]), from its reader's `values`: each step one
    /// unit of analysis work and its key hashed, each entry not yet copied
    /// charged as a capture before it is read raw and kept. A step from
    /// anything but a captured table (a host module's proxy, a string, an
    /// unknown value) copies nothing, and nor do the steps after it.
    fn paths(&mut self, ast: &Ast, inst: Instance, values: &[Value]) -> Result<(), ImportError> {
        let Some(func) = ast.func(inst.func) else {
            return Ok(());
        };
        let paths = ast.paths(func);
        // Each step's table, when it reads one: at most the function's
        // paths, each charged as a tree node.
        let mut at: Vec<Option<(Table, u32)>> = Vec::with_capacity(paths.len());
        for p in paths {
            self.analysis.charge(1).map_err(|w| self.fatal(w))?;
            let from = if p.parent == super::syntax::NONE {
                let k = self
                    .analysis
                    .slots
                    .get(&(inst.file, inst.func, p.root))
                    .copied();
                match k.and_then(|k| values.get(k as usize)) {
                    Some(Value::Table(t)) => self
                        .table_of
                        .get(&t.to_pointer().addr())
                        .map(|&id| (t.clone(), id)),
                    _ => None,
                }
            } else {
                at.get(p.parent as usize).cloned().flatten()
            };
            let Some((t, id)) = from else {
                at.push(None);
                continue;
            };
            // The key hashed, its bytes charged first; an entry not yet
            // copied charged as a capture before its key is interned (hashed
            // and charged again) and it is read.
            let key = ast.str(p.key);
            self.charge_hash(key)?;
            let known = self
                .analysis
                .names
                .get(key)
                .and_then(|k| self.analysis.tables.get(id as usize)?.get(k))
                .copied();
            if known.is_none() {
                self.charge()?;
            }
            let v: Value = t
                .raw_get(mlua::BString::from(key))
                .map_err(|e| self.mlua(&e))?;
            let c = match known {
                Some(c) => c,
                None => {
                    let k = self.intern(key)?;
                    let c = self.captured(&v)?;
                    if let Some(m) = self.analysis.tables.get_mut(id as usize) {
                        m.insert(k, c);
                    }
                    c
                }
            };
            at.push(match (c, v) {
                (Captured::Table { id, .. }, Value::Table(t)) => Some((t, id)),
                _ => None,
            });
        }
        Ok(())
    }

    /// One captured value: a scalar, a string interned, a host function,
    /// a library function or a host module by identity, a tagged closure's
    /// instance, or a table by identity with whether it has a metatable
    /// (its entries copied only along static access paths).
    fn captured(&mut self, v: &Value) -> Result<Captured, ImportError> {
        Ok(match v {
            Value::Nil => Captured::Nil,
            Value::Boolean(b) => Captured::Bool(*b),
            Value::Integer(i) => Captured::Int(*i),
            Value::Number(x) => Captured::Float(x.to_bits()),
            Value::String(s) => {
                let s = s.as_bytes();
                if s.len() > MAX_STRING_BYTES {
                    Captured::Other
                } else {
                    Captured::Str(self.intern(&s)?)
                }
            }
            Value::Function(g) => {
                if let Some(api) = self.pass.host_api_of(g.to_pointer().addr()) {
                    Captured::Host(api)
                } else if let Some(lib) = self.lib(g)? {
                    Captured::Lib(lib)
                } else {
                    match self.tagged(g)? {
                        Some(j) => Captured::Closure(j),
                        None => Captured::Other,
                    }
                }
            }
            Value::Table(t) => {
                let addr = t.to_pointer().addr();
                let proxy = self
                    .tags
                    .as_ref()
                    .and_then(|tags| tags.proxies.iter().find(|p| p.0 == addr).copied());
                match proxy {
                    Some((_, m, true)) => Captured::Module(m),
                    Some(_) => Captured::Other,
                    None => Captured::Table {
                        id: self.table_id(t)?,
                        meta: t.metatable().is_some(),
                    },
                }
            }
            _ => Captured::Other,
        })
    }

    /// Library function `g`'s index in [`super::sandbox::LIBS`], when it is
    /// the sandbox's own: one raw read of the registry's table.
    fn lib(&self, g: &Function) -> Result<Option<u16>, ImportError> {
        let Some(tags) = &self.tags else {
            return Ok(None);
        };
        match tags.libs.raw_get::<Value>(g.clone()) {
            Ok(Value::Integer(i)) => Ok(u16::try_from(i).ok()),
            Ok(_) => Ok(None),
            Err(e) => Err(self.mlua(&e)),
        }
    }

    /// Table `t`'s index among the captured tables, found by its address
    /// or added (charged as a capture first) with no entry copied.
    fn table_id(&mut self, t: &Table) -> Result<u32, ImportError> {
        let addr = t.to_pointer().addr();
        if let Some(&id) = self.table_of.get(&addr) {
            return Ok(id);
        }
        self.charge()?;
        let id = u32::try_from(self.analysis.tables.len())
            .map_err(|_| self.lua(LuaWhy::Limit(LuaLimit::Captures)))?;
        self.analysis.tables.push(HashMap::new());
        self.table_of.insert(addr, id);
        let config = self.tags.as_ref().and_then(|tags| tags.config.as_ref());
        if config.is_some_and(|c| c.to_pointer() == t.to_pointer()) {
            self.analysis.config = Some(id);
        }
        Ok(id)
    }

    /// A hash of `s`, its bytes charged as analysis work before it is made.
    fn charge_hash(&self, s: &[u8]) -> Result<(), ImportError> {
        let len = u64::try_from(s.len()).unwrap_or(u64::MAX);
        self.analysis.charge(len).map_err(|w| self.fatal(w))
    }

    /// String `s`'s id among the pass's interned names: one hash, its
    /// bytes charged as analysis work first; a new one kept, its bytes
    /// charged against the retained bytes before it is copied.
    fn intern(&mut self, s: &[u8]) -> Result<u32, ImportError> {
        self.charge_hash(s)?;
        let len = u64::try_from(s.len()).unwrap_or(u64::MAX);
        if let Some(&id) = self.analysis.names.get(s) {
            return Ok(id);
        }
        self.retain(len)?;
        let id = u32::try_from(self.analysis.names.len())
            .map_err(|_| self.lua(LuaWhy::Limit(LuaLimit::Captures)))?;
        self.analysis.names.insert(Box::from(s), id);
        Ok(id)
    }

    /// `len` bytes the conversion keeps beside the arena (an enumerated
    /// table's keys, an interned option name), charged cumulatively before
    /// they are copied against [`MAX_DECODED_BYTES`], apart from the
    /// arena's own decoded bytes.
    fn retain(&mut self, len: u64) -> Result<(), ImportError> {
        if self.retained.saturating_add(len) > MAX_DECODED_BYTES {
            return Err(self.input(InputLimit::DecodedBytes));
        }
        self.retained += len;
        Ok(())
    }

    /// The refusal a walk's sentinel stands for: the time budget's
    /// unlocated, since no phase of the conversion is at a callable.
    fn fatal(&self, why: super::Unsupported) -> ImportError {
        let lua = callback::fatal(why).unwrap_or(LuaWhy::Limit(LuaLimit::AnalysisWork));
        if why == callback::TOO_LONG {
            return ImportError::Lua { at: None, why: lua };
        }
        self.lua(lua)
    }

    /// Table `t`'s keys, read once by `next`: their kind and count, an
    /// object's or a mixed table's string keys copied out (no handle per
    /// key stays on Lua's stack) and sorted by their bytes; no value is
    /// read. Each entry is counted against the node
    /// cap and each key's bytes against the decoded-bytes cap as it is
    /// met, and cancellation is seen every [`CHECK_EVERY`] entries and
    /// once the scan is done; `next`'s own walk over empty slots is
    /// bounded by the table's capacity in VM memory, met once per table.
    /// Its [`Summary`] comes from the same scan: each entry's value type
    /// read from the pair in hand, and the metatable, raw.
    fn enumerate(&mut self, t: &Table) -> Result<(Shape, Summary), ImportError> {
        let (mut n, mut ints) = (0_usize, 0_usize);
        let (mut min, mut max) = (i64::MAX, i64::MIN);
        let mut keys: Vec<Vec<u8>> = Vec::new();
        let mut held = false;
        for pair in t.pairs::<Value, Value>() {
            self.poll(n)?;
            if self.nodes.len().saturating_add(n) >= MAX_NODES {
                return Err(self.input(InputLimit::Nodes));
            }
            let (k, v) = pair.map_err(|e| self.mlua(&e))?;
            n += 1;
            if !matches!(k, Value::String(_)) {
                held |= matches!(v, Value::Function(_) | Value::String(_) | Value::Table(_));
            }
            match k {
                Value::String(s) => {
                    let s = s.as_bytes();
                    if s.len() > MAX_STRING_BYTES {
                        return Err(self.input(InputLimit::StringBytes));
                    }
                    // A retained copy, charged cumulatively before it is
                    // made: the arena's decoding of it is charged apart.
                    self.retain(u64::try_from(s.len()).unwrap_or(u64::MAX))?;
                    keys.push(s.to_vec());
                }
                Value::Integer(i) => {
                    ints += 1;
                    min = min.min(i);
                    max = max.max(i);
                }
                _ => {}
            }
        }
        if self.pass.ledger.cancelled() {
            return Err(self.lua(LuaWhy::Cancelled));
        }
        let shape = if n == 0 {
            Shape::Empty
        } else if !keys.is_empty() {
            let all = keys.len() == n;
            let keys = self.sorted(keys)?;
            if all {
                Shape::Object(keys)
            } else {
                Shape::Mixed(keys)
            }
        } else if ints == n {
            let count = i64::try_from(ints).unwrap_or(i64::MAX);
            if min != 1 || max != count {
                return Err(self.lua(LuaWhy::Sparse));
            }
            Shape::Array(ints)
        } else {
            return Err(self.lua(LuaWhy::Key));
        };
        let summary = Summary {
            unread: held && matches!(shape, Shape::Mixed(_)),
            meta: t.metatable().is_some(),
        };
        Ok((shape, summary))
    }

    /// `keys` sorted by their bytes, the bytes compared counted and the
    /// cancellation request seen every 64 KiB of them: once seen, the rest
    /// of the sort compares nothing.
    fn sorted(&self, mut keys: Vec<Vec<u8>>) -> Result<Rc<[Vec<u8>]>, ImportError> {
        let ledger = &self.pass.ledger;
        let (mut compared, mut stop) = (0_usize, false);
        keys.sort_unstable_by(|a, b| {
            if stop {
                return std::cmp::Ordering::Equal;
            }
            let before = compared;
            compared = compared.saturating_add(a.len().min(b.len()) + 1);
            if before >> 16 != compared >> 16 && ledger.cancelled() {
                stop = true;
                return std::cmp::Ordering::Equal;
            }
            a.cmp(b)
        });
        if stop {
            return Err(self.lua(LuaWhy::Cancelled));
        }
        Ok(Rc::from(keys))
    }

    /// Table `t`, its key's span when it has one, at `depth` and at
    /// position `pos`. Its [`Summary`] applies at this place whether or
    /// not the table was enumerated here: unread entries where the source
    /// lists every entry, or a metatable on a font fallback, open the
    /// analysis at its node. Each entry takes its position from `pos`: an
    /// element's where every entry is one, a class's field by its key (one
    /// search, charged first), and nothing the source reads elsewhere.
    fn table(
        &mut self,
        t: &Table,
        span: Option<Span>,
        depth: u32,
        pos: Pos,
    ) -> Result<u32, ImportError> {
        if depth > MAX_DEPTH {
            return Err(self.input(InputLimit::Depth));
        }
        let addr = t.to_pointer().addr();
        if self.path.contains(&addr) {
            return Err(self.lua(LuaWhy::Cycle));
        }
        // The table's shape, enumerated once a conversion by its identity:
        // a table met again through another reference is not scanned
        // again, while each reference is charged its nodes and key bytes.
        let (shape, summary) = match self.shapes.get(&addr) {
            Some(cached) => cached.clone(),
            None => {
                let scanned = self.enumerate(t)?;
                self.shapes.insert(addr, scanned.clone());
                scanned
            }
        };
        let (kind, n) = match &shape {
            Shape::Empty => (Kind::Empty, 0),
            Shape::Array(n) => (Kind::Array, *n),
            Shape::Object(keys) => (Kind::Object, keys.len()),
            Shape::Mixed(keys) => (Kind::Mixed, keys.len()),
        };
        if self.nodes.len().saturating_add(n) >= MAX_NODES {
            return Err(self.input(InputLimit::Nodes));
        }
        let id = self.node(kind, span, 0)?;
        if self.tags.is_some() {
            if pos.lists() && summary.unread {
                #[cfg(any(test, feature = "dev-tools"))]
                {
                    self.analysis.unread = self.analysis.unread.saturating_add(1);
                }
                self.open(id, super::Unsupported::Construct("an unread entry"));
            }
            if matches!(pos, Pos::FontFallback) && summary.meta {
                self.open(
                    id,
                    super::Unsupported::Capture("a font fallback's metatable"),
                );
            }
        }
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
        let element = pos.element();
        if kind == Kind::Array {
            for i in 1..=n {
                let v: Value = t.raw_get(i).map_err(|e| self.mlua(&e))?;
                let child = self.value(&v, None, depth + 1, element)?;
                link(self, child);
            }
        } else if let Shape::Object(keys) | Shape::Mixed(keys) = &shape {
            // Each key's bytes charged again for this reference as they
            // are copied into the arena; a mixed table's other keys are
            // never read.
            for k in keys.iter() {
                let span = self.text(k)?;
                let at = match pos {
                    Pos::Class(c) => {
                        self.analysis.charge(1).map_err(|w| self.fatal(w))?;
                        c.field(k)
                    }
                    _ => element,
                };
                let v: Value = t
                    .raw_get(mlua::BString::from(k.as_slice()))
                    .map_err(|e| self.mlua(&e))?;
                let child = self.value(&v, Some(span), depth + 1, at)?;
                link(self, child);
            }
        }
        self.path.pop();
        Ok(id)
    }
}
