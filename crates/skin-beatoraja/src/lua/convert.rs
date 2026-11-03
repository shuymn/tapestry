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
//! In the configured pass, each skin closure the table holds is looked up
//! by its tag ([`super::instrument`]) and its captures are copied
//! ([`super::callback`]): its reader is called (the sandbox's generated
//! code, which only reads its bindings) and each value kept as an owned
//! integer, boolean, host function or module by identity, another
//! closure instance (whose captures are copied in turn), or the `_ENV` it
//! reads globals through, inspected raw: whether its `skin_config` is the
//! pass's own table, no metatable on the way, and the integer entries of
//! `skin_config.option`. Each instance, value and entry is charged before
//! it is kept ([`MAX_CAPTURES`]).
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

use super::callback::{self, Analysis, Captured, EnvCapture, Instance};
use super::limits::MAX_CAPTURES;
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
#[cfg(feature = "dev-tools")]
pub(crate) fn callables(
    arena: &Arena,
) -> impl Iterator<Item = (u32, &'static str, CallableKind)> + '_ {
    let lua = arena.lua.as_deref();
    callback::callable_nodes(arena).filter_map(move |(id, i)| {
        let kind = lua?.callables.get(i as usize)?.kind;
        let key: String = arena.key(id).map(Iterator::collect).unwrap_or_default();
        let field = FIELDS
            .iter()
            .find(|f| **f == key)
            .copied()
            .unwrap_or("other");
        Some((id, field, kind))
    })
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
        env_of: HashMap::new(),
        option_set_of: HashMap::new(),
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
    let top = c.table(&t, None, 1)?;
    c.capture()?;
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
}

struct Conv<'a> {
    pass: &'a Pass,
    tags: Option<Tags>,
    /// Closure instances by the function's address, and those whose
    /// captures are still to copy.
    instance_of: HashMap<usize, u32>,
    pending: Vec<(u32, Function)>,
    /// `_ENV` captures by the table's address.
    env_of: HashMap<usize, u32>,
    /// Option tables read, by address.
    option_set_of: HashMap<usize, u32>,
    /// Tables enumerated, by address.
    shapes: HashMap<usize, Shape>,
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
                        let callable = self.callable(f, addr)?;
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
    fn callable(&mut self, f: &mlua::Function, addr: usize) -> Result<Callable, ImportError> {
        if let Some(api) = self.pass.host_api_of(addr) {
            return Ok(Callable {
                module: None,
                line: 0,
                kind: CallableKind::Host(api),
                instance: NONE,
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
            });
        }
        let source = info.source.as_deref().and_then(|s| s.strip_prefix('@'));
        // One hash of the source's name, its bytes charged first.
        if let Some(s) = source {
            let len = u64::try_from(s.len()).unwrap_or(u64::MAX);
            self.analysis.charge(len).map_err(|w| self.fatal(w))?;
        }
        let module = source.and_then(|s| self.module_index.get(s).copied());
        let instance = self.tagged(f)?.unwrap_or(NONE);
        Ok(Callable {
            module,
            line: info
                .line_defined
                .and_then(|l| u32::try_from(l).ok())
                .unwrap_or(0),
            kind: CallableKind::Lua,
            instance,
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
        r
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
                .map_err(|e| self.mlua(&e))?;
            let Some(inst) = self.analysis.instances.get(i as usize).copied() else {
                continue;
            };
            let ups: Vec<(u32, bool)> = {
                let files = self.pass.files.borrow();
                let Some(ast) = files.get(usize::from(inst.file)).map(|f| &f.ast) else {
                    continue;
                };
                let Some(func) = ast.func(inst.func) else {
                    continue;
                };
                ast.list(func.upvalues)
                    .iter()
                    .map(|&u| (u, callback::is_env(ast, u)))
                    .collect()
            };
            // Each binding's position, hashed once a function.
            for (k, &(up, _)) in (0_u32..).zip(&ups) {
                self.analysis
                    .slots
                    .entry((inst.file, inst.func, up))
                    .or_insert(k);
            }
            let start = u32::try_from(self.analysis.captured.len())
                .map_err(|_| self.lua(LuaWhy::Limit(LuaLimit::Captures)))?;
            let mut n = 0_u32;
            for (k, v) in values.into_iter().enumerate() {
                self.charge()?;
                let env = ups.get(k).is_some_and(|&(_, env)| env);
                let c = self.captured(v, env)?;
                self.analysis.captured.push(c);
                n += 1;
            }
            if let Some(x) = self.analysis.instances.get_mut(i as usize) {
                x.values = Some((start, n));
            }
        }
        Ok(())
    }

    /// One captured value, `env` when its binding is an `_ENV`.
    fn captured(&mut self, v: Value, env: bool) -> Result<Captured, ImportError> {
        Ok(match v {
            Value::Nil => Captured::Nil,
            Value::Boolean(b) => Captured::Bool(b),
            Value::Integer(i) => Captured::Int(i),
            Value::Function(g) => {
                if let Some(api) = self.pass.host_api_of(g.to_pointer().addr()) {
                    Captured::Host(api)
                } else {
                    match self.tagged(&g)? {
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
                    None if env => Captured::Env(self.env(&t)?),
                    None => Captured::Other,
                }
            }
            _ => Captured::Other,
        })
    }

    /// The capture of `_ENV` table `t`, inspected raw once.
    fn env(&mut self, t: &Table) -> Result<u32, ImportError> {
        let addr = t.to_pointer().addr();
        if let Some(&i) = self.env_of.get(&addr) {
            return Ok(i);
        }
        let mut cap = EnvCapture::default();
        let config = self.tags.as_ref().and_then(|tags| tags.config.clone());
        let option = (|| {
            if t.metatable().is_some() {
                return Err("_ENV's metatable");
            }
            let Ok(Value::Table(sc)) = t.raw_get::<Value>("skin_config") else {
                return Err("skin_config");
            };
            if config.is_none_or(|c| c.to_pointer() != sc.to_pointer()) {
                return Err("skin_config");
            }
            if sc.metatable().is_some() {
                return Err("skin_config's metatable");
            }
            let Ok(Value::Table(option)) = sc.raw_get::<Value>("option") else {
                return Err("skin_config.option");
            };
            if option.metatable().is_some() {
                return Err("skin_config.option's metatable");
            }
            Ok(option)
        })();
        match option {
            Err(why) => cap.unproven = Some(why),
            Ok(option) => cap.options = self.option_set(&option)?,
        }
        self.charge()?;
        let i = u32::try_from(self.analysis.envs.len())
            .map_err(|_| self.lua(LuaWhy::Limit(LuaLimit::Captures)))?;
        self.analysis.envs.push(cap);
        self.env_of.insert(addr, i);
        Ok(i)
    }

    /// The integer entries of option table `t`, read once a pass by its
    /// address: each name hashed (its bytes charged as analysis work) and
    /// interned once (its bytes charged as decoded bytes), the entries
    /// kept by name id.
    fn option_set(&mut self, t: &Table) -> Result<u32, ImportError> {
        let addr = t.to_pointer().addr();
        if let Some(&i) = self.option_set_of.get(&addr) {
            return Ok(i);
        }
        let mut set = HashMap::new();
        for (seen, pair) in t.pairs::<Value, Value>().enumerate() {
            self.poll(seen)?;
            // Every entry is charged, whatever it holds.
            self.charge()?;
            let (k, v) = pair.map_err(|e| self.mlua(&e))?;
            let (Value::String(k), Value::Integer(v)) = (k, v) else {
                continue;
            };
            let k = k.as_bytes();
            if k.len() > MAX_STRING_BYTES {
                continue;
            }
            let len = u64::try_from(k.len()).unwrap_or(u64::MAX);
            self.analysis.charge(len).map_err(|w| self.fatal(w))?;
            let id = match self.analysis.option_names.get(&*k) {
                Some(&id) => id,
                None => {
                    self.retain(len)?;
                    let id = u32::try_from(self.analysis.option_names.len())
                        .map_err(|_| self.lua(LuaWhy::Limit(LuaLimit::Captures)))?;
                    self.analysis.option_names.insert(Box::from(&*k), id);
                    id
                }
            };
            set.insert(id, v);
        }
        let i = u32::try_from(self.analysis.option_sets.len())
            .map_err(|_| self.lua(LuaWhy::Limit(LuaLimit::Captures)))?;
        self.analysis.option_sets.push(set);
        self.option_set_of.insert(addr, i);
        Ok(i)
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

    /// The refusal a walk's sentinel stands for.
    fn fatal(&self, why: super::Unsupported) -> ImportError {
        self.lua(callback::fatal(why).unwrap_or(LuaWhy::Limit(LuaLimit::AnalysisWork)))
    }

    /// Table `t`'s keys, read once by `next`: their kind and count, an
    /// object's keys copied out (no handle per key stays on Lua's stack)
    /// and sorted by their bytes. Each entry is counted against the node
    /// cap and each key's bytes against the decoded-bytes cap as it is
    /// met, and cancellation is seen every [`CHECK_EVERY`] entries and
    /// once the scan is done; `next`'s own walk over empty slots is
    /// bounded by the table's capacity in VM memory, met once per table.
    fn enumerate(&mut self, t: &Table) -> Result<Shape, ImportError> {
        let (mut n, mut ints) = (0_usize, 0_usize);
        let (mut min, mut max) = (i64::MAX, i64::MIN);
        let mut keys: Vec<Vec<u8>> = Vec::new();
        for pair in t.pairs::<Value, Value>() {
            self.poll(n)?;
            if self.nodes.len().saturating_add(n) >= MAX_NODES {
                return Err(self.input(InputLimit::Nodes));
            }
            let (k, _) = pair.map_err(|e| self.mlua(&e))?;
            n += 1;
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
                _ => return Err(self.lua(LuaWhy::Key)),
            }
        }
        if self.pass.ledger.cancelled() {
            return Err(self.lua(LuaWhy::Cancelled));
        }
        if n == 0 {
            Ok(Shape::Empty)
        } else if keys.len() == n {
            // Sorted by their bytes, the bytes compared counted and the
            // cancellation request seen every 64 KiB of them: once seen,
            // the rest of the sort compares nothing.
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
            Ok(Shape::Object(Rc::from(keys)))
        } else if ints == n {
            let count = i64::try_from(n).unwrap_or(i64::MAX);
            if min != 1 || max != count {
                return Err(self.lua(LuaWhy::Sparse));
            }
            Ok(Shape::Array(n))
        } else {
            Err(self.lua(LuaWhy::Mixed))
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
        // The table's shape, enumerated once a conversion by its identity:
        // a table met again through another reference is not scanned
        // again, while each reference is charged its nodes and key bytes.
        let shape = match self.shapes.get(&addr) {
            Some(shape) => shape.clone(),
            None => {
                let shape = self.enumerate(t)?;
                self.shapes.insert(addr, shape.clone());
                shape
            }
        };
        let (kind, n) = match &shape {
            Shape::Empty => (Kind::Empty, 0),
            Shape::Array(n) => (Kind::Array, *n),
            Shape::Object(keys) => (Kind::Object, keys.len()),
        };
        if self.nodes.len().saturating_add(n) >= MAX_NODES {
            return Err(self.input(InputLimit::Nodes));
        }
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
        } else if let Shape::Object(keys) = &shape {
            // Each key's bytes charged again for this reference as they
            // are copied into the arena.
            for k in keys.iter() {
                let span = self.text(k)?;
                let v: Value = t
                    .raw_get(mlua::BString::from(k.as_slice()))
                    .map_err(|e| self.mlua(&e))?;
                let child = self.value(&v, Some(span), depth + 1)?;
                link(self, child);
            }
        }
        self.path.pop();
        Ok(id)
    }
}
