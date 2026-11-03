//! A configured pass's chunks rewritten so that every closure is tagged
//! as it is created (design.md "beatoraja Lua Skins", per-frame
//! closures): each function constructor `F` of a file's tree
//! ([`super::syntax`]) becomes `T(id, F, R)`, where `T` is the sandbox's
//! tagger, `id` the constructor's identity and `R` its capture reader, a
//! closure written in the same scope as `F` that returns the current
//! values of the bindings `F` reads from outside itself
//! (`function() return a, b, _ENV end`), or `nil` when it reads none.
//! The tagger keeps `R` beside the closure, so the reader called after
//! the pass observes the final bindings; no upvalue is inspected by
//! privilege.
//!
//! `local function f` becomes `local f; f = T(...)` and `function a.b:c`
//! becomes `a.b.c = T(id, function(self, ...)`, as the manual defines
//! them. The chunk is wrapped as `local T = ...; return function(...)`
//! and `end`, on its first and past its last line, so that the tagger
//! reaches it as a parameter no skin can name: `T` is a name the source
//! does not contain. Every line keeps its number: text is only inserted
//! within lines, and a rewritten function name keeps its line breaks.

use super::syntax::{Ast, CHUNK_ENV, DeclKind, Form};

/// The most bytes an instrumented chunk adds to its source's: past it the
/// candidate is refused.
pub(crate) const MAX_ADDED_BYTES: usize = 1 << 24;

/// The tagger's name in `src`: a name it does not contain.
fn tagger(src: &[u8]) -> String {
    (0_u32..)
        .map(|n| format!("tapestry_tag{n}"))
        .find(|t| !src.windows(t.len()).any(|w| w == t.as_bytes()))
        .unwrap_or_default()
}

/// One edit at a byte offset: text inserted, replacing `skip` bytes.
struct Edit {
    at: usize,
    /// Ends before starts at the same offset.
    order: u8,
    skip: usize,
    text: String,
}

/// The Lua line breaks in `b`.
fn lines(b: &[u8]) -> usize {
    let mut n = 0;
    let mut i = 0;
    while let Some(&c) = b.get(i) {
        if c == b'\n' || c == b'\r' {
            n += 1;
            if matches!(b.get(i + 1), Some(&d) if (d == b'\n' || d == b'\r') && d != c) {
                i += 1;
            }
        }
        i += 1;
    }
    n
}

fn text(src: &[u8], (s, e): (u32, u32)) -> &str {
    src.get(s as usize..e as usize)
        .and_then(|b| std::str::from_utf8(b).ok())
        .unwrap_or("")
}

/// `src` instrumented with the tags of file `file`, or `None` when it
/// would grow past [`MAX_ADDED_BYTES`].
pub(crate) fn instrument(src: &[u8], ast: &Ast, file: u16) -> Option<Vec<u8>> {
    let t = tagger(src);
    let mut edits: Vec<Edit> = Vec::new();
    let mut added = 0_usize;
    for (index, f) in ast.funcs.iter().enumerate().skip(1) {
        let id = (u64::from(file) << 32) | u64::try_from(index).ok()?;
        let reader = reader(src, ast, index)?;
        let (start, open) = (f.start as usize, f.open as usize);
        let head = |text: String| -> Edit {
            let breaks = lines(src.get(start..open).unwrap_or_default());
            Edit {
                at: start,
                order: 1,
                skip: open - start,
                text: text + &"\n".repeat(breaks),
            }
        };
        let edit = match f.form {
            Form::Expr => Edit {
                at: start,
                order: 1,
                skip: 0,
                text: format!("{t}({id},"),
            },
            Form::Local { decl } => {
                let name = text(src, ast.decl(decl)?.name);
                head(format!("local {name};{name}={t}({id},function("))
            }
            Form::Named { names, method } => {
                let parts = ast.list(names);
                let mut target = String::new();
                for pair in parts.chunks(2) {
                    let [s, e] = pair else { return None };
                    if !target.is_empty() {
                        target.push('.');
                    }
                    target.push_str(text(src, (*s, *e)));
                }
                let mut h = format!("{target}={t}({id},function(");
                if method {
                    h.push_str("self");
                    if f.params.1 > 1 || f.vararg {
                        h.push(',');
                    }
                }
                head(h)
            }
        };
        added = added.saturating_add(edit.text.len() + reader.len() + 2);
        edits.push(edit);
        edits.push(Edit {
            at: f.end as usize,
            order: 0,
            skip: 0,
            text: format!(",{reader})"),
        });
        if added > MAX_ADDED_BYTES {
            return None;
        }
    }
    edits.sort_by_key(|e| (e.at, e.order));
    let prefix = format!("local {t}=...;return function(...) ");
    let mut out = Vec::with_capacity(src.len() + added + prefix.len() + 4);
    out.extend_from_slice(prefix.as_bytes());
    let mut at = 0;
    for e in &edits {
        out.extend_from_slice(src.get(at..e.at)?);
        out.extend_from_slice(e.text.as_bytes());
        at = e.at + e.skip;
    }
    out.extend_from_slice(src.get(at..)?);
    out.extend_from_slice(b"\nend");
    Some(out)
}

/// The capture reader of function `index`: `nil`, or a function
/// returning its upvalues in their order.
fn reader(src: &[u8], ast: &Ast, index: usize) -> Option<String> {
    let f = ast.funcs.get(index)?;
    let ups = ast.list(f.upvalues);
    if f.unread || ups.is_empty() {
        return Some("nil".to_owned());
    }
    let mut r = String::from("function() return ");
    for (i, &up) in ups.iter().enumerate() {
        if i > 0 {
            r.push(',');
        }
        if up == CHUNK_ENV {
            r.push_str("_ENV");
            continue;
        }
        let d = ast.decl(up)?;
        if d.kind == DeclKind::SelfParam {
            r.push_str("self");
        } else {
            r.push_str(text(src, d.name));
        }
    }
    r.push_str(" end");
    Some(r)
}

#[cfg(test)]
mod tests {
    use super::instrument;
    use crate::lua::syntax::parse;

    fn run(src: &str) -> String {
        let mut n = 0;
        let ast = parse(src.as_bytes(), &mut n, &|| false).unwrap();
        String::from_utf8(instrument(src.as_bytes(), &ast, 1).unwrap()).unwrap()
    }

    #[test]
    fn constructors_are_tagged_with_their_readers() {
        let out = run("local a = 1\nlocal f = function() return a end\nreturn f");
        assert_eq!(
            out,
            "local tapestry_tag0=...;return function(...) local a = 1\n\
             local f = tapestry_tag0(4294967297,function() return a end,function() return a end)\n\
             return f\nend"
        );
        // Globals are read through `_ENV`; a name the source uses is not
        // the tagger's.
        let out = run("local tapestry_tag0 = 1 g = function() return x end");
        assert!(out.starts_with("local tapestry_tag1=...;"), "{out}");
        assert!(out.contains("function() return _ENV end)"), "{out}");
    }

    #[test]
    fn statements_keep_their_lines() {
        let out = run(
            "local m = {}\nfunction m.a\n.b:c(x)\nreturn self, x, m\nend\nlocal function f() return f end",
        );
        assert_eq!(
            out,
            "local tapestry_tag0=...;return function(...) local m = {}\n\
             m.a.b.c=tapestry_tag0(4294967297,function(self,\nx)\nreturn self, x, m\nend,function() return m end)\n\
             local f;f=tapestry_tag0(4294967298,function() return f end,function() return f end)\nend"
        );
    }
}
