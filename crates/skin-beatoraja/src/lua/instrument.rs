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
//! them. A constructor that ends a statement is followed by `;`: `T(...)`
//! is a call, which a following `(` would otherwise extend, where the
//! constructor itself ended the statement. The chunk is wrapped as
//! `local T = ...; return function(...)` and `end`, on its first and past
//! its last line, so that the tagger reaches it as a parameter no skin
//! can name: `T` is `tapestry_tag<n>` with the least `n` the source does
//! not use as a name. Every line keeps its number: text is only inserted
//! within lines, and a rewritten function name keeps its line breaks. A
//! chunk without a constructor is not rewritten.

use super::syntax::{Ast, CHUNK_ENV, DeclKind, Form, TAG_PREFIX};

/// The most bytes an instrumented chunk adds to its source's: past it the
/// candidate is refused.
pub(crate) const MAX_ADDED_BYTES: usize = 1 << 24;

/// The most bytes the wrapper adds to a chunk: its prefix (with the
/// tagger's name) and its `end`.
const WRAPPER_BYTES: usize = 128;

/// How many constructors are rewritten, or edits copied, between two
/// looks at the cancellation request.
const CHECK_EVERY: usize = 4_096;
/// How many bytes are added between two looks at it.
const CHECK_BYTES: usize = 1 << 16;

/// Why a chunk was not instrumented.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Stop {
    /// It would grow past [`MAX_ADDED_BYTES`].
    Generated,
    Cancelled,
}

/// The tagger's name: `tapestry_tag<n>` with the least `n` that is not
/// among `used`, in time linear in `used`.
fn tagger(used: &[u64]) -> String {
    let mut taken = vec![false; used.len() + 1];
    for &n in used {
        if let Some(slot) = usize::try_from(n).ok().and_then(|i| taken.get_mut(i)) {
            *slot = true;
        }
    }
    let n = taken.iter().position(|t| !t).unwrap_or(used.len());
    format!("{}{n}", String::from_utf8_lossy(TAG_PREFIX))
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

/// `src` instrumented with the tags of file `file`, looking at
/// `cancelled` every [`CHECK_EVERY`] constructors.
pub(crate) fn instrument(
    src: &[u8],
    ast: &Ast,
    file: u16,
    budget: usize,
    cancelled: &dyn Fn() -> bool,
) -> Result<Vec<u8>, Stop> {
    // The bytes it may add: [`MAX_ADDED_BYTES`] a file, and what is left
    // of the pass's rewritten-source allowance (`budget`) once the source
    // itself and the wrapper are counted.
    let cap = MAX_ADDED_BYTES.min(
        budget
            .checked_sub(src.len() + WRAPPER_BYTES)
            .ok_or(Stop::Generated)?,
    );
    let t = tagger(&ast.tag_names);
    let mut edits: Vec<Edit> = Vec::new();
    let mut added = 0_usize;
    let mut next_check = CHECK_BYTES;
    for (index, f) in ast.funcs.iter().enumerate().skip(1) {
        // Seen every so many constructors and every so many bytes added.
        if (index % CHECK_EVERY == 0 || added >= next_check) && cancelled() {
            return Err(Stop::Cancelled);
        }
        if added >= next_check {
            next_check = added.saturating_add(CHECK_BYTES);
        }
        let id = (u64::from(file) << 32) | u64::try_from(index).map_err(|_| Stop::Generated)?;
        let room = cap.saturating_sub(added);
        let reader = reader(src, ast, index, room).ok_or(Stop::Generated)?;
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
                let name = text(src, ast.decl(decl).ok_or(Stop::Generated)?.name);
                head(format!("local {name};{name}={t}({id},function("))
            }
            Form::Named { names, method } => {
                let parts = ast.list(names);
                let mut target = String::new();
                for pair in parts.chunks(2) {
                    let [s, e] = pair else {
                        return Err(Stop::Generated);
                    };
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
        // A constructor that ended its statement still ends it.
        let ends = ast.stmt_ends.binary_search(&f.end).is_ok();
        let close = if ends { ");" } else { ")" };
        added = added.saturating_add(edit.text.len() + reader.len() + 3);
        edits.push(edit);
        edits.push(Edit {
            at: f.end as usize,
            order: 0,
            skip: 0,
            text: format!(",{reader}{close}"),
        });
        if added > cap {
            return Err(Stop::Generated);
        }
    }
    edits.sort_by_key(|e| (e.at, e.order));
    let prefix = format!("local {t}=...;return function(...) ");
    let mut out = Vec::with_capacity(src.len() + added + prefix.len() + 4);
    out.extend_from_slice(prefix.as_bytes());
    let mut at = 0;
    for (k, e) in edits.iter().enumerate() {
        if k % CHECK_EVERY == 0 && cancelled() {
            return Err(Stop::Cancelled);
        }
        out.extend_from_slice(src.get(at..e.at).ok_or(Stop::Generated)?);
        out.extend_from_slice(e.text.as_bytes());
        at = e.at + e.skip;
    }
    out.extend_from_slice(src.get(at..).ok_or(Stop::Generated)?);
    out.extend_from_slice(b"\nend");
    Ok(out)
}

/// The name an upvalue is read by in a reader.
fn up_name<'a>(src: &'a [u8], ast: &Ast, up: u32) -> Option<&'a str> {
    if up == CHUNK_ENV {
        return Some("_ENV");
    }
    let d = ast.decl(up)?;
    Some(if d.kind == DeclKind::SelfParam {
        "self"
    } else {
        text(src, d.name)
    })
}

/// The capture reader of function `index`: `nil`, or a function
/// returning its upvalues in their order; `None` when it would be longer
/// than `room`, measured before it is built.
fn reader(src: &[u8], ast: &Ast, index: usize, room: usize) -> Option<String> {
    let f = ast.funcs.get(index)?;
    let ups = ast.list(f.upvalues);
    if f.unread || ups.is_empty() {
        return Some("nil".to_owned());
    }
    let mut len = "function() return  end".len();
    for &up in ups {
        len = len.saturating_add(up_name(src, ast, up)?.len() + 1);
    }
    if len > room {
        return None;
    }
    let mut r = String::with_capacity(len);
    r.push_str("function() return ");
    for (i, &up) in ups.iter().enumerate() {
        if i > 0 {
            r.push(',');
        }
        r.push_str(up_name(src, ast, up)?);
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
        String::from_utf8(instrument(src.as_bytes(), &ast, 1, usize::MAX >> 1, &|| false).unwrap())
            .unwrap()
    }

    #[test]
    fn constructors_are_tagged_with_their_readers() {
        let out = run("local a = 1\nlocal f = function() return a end\nreturn f");
        assert_eq!(
            out,
            "local tapestry_tag0=...;return function(...) local a = 1\n\
             local f = tapestry_tag0(4294967297,function() return a end,function() return a end);\n\
             return f\nend"
        );
        // Globals are read through `_ENV`; a name the source uses is not
        // the tagger's, but one only in a comment or a string is.
        let out = run("local tapestry_tag0 = 1 g = function() return x end");
        assert!(out.starts_with("local tapestry_tag1=...;"), "{out}");
        assert!(out.contains("function() return _ENV end);"), "{out}");
        let out = run("-- tapestry_tag0\ng = \"tapestry_tag0\" f = function() end");
        assert!(out.starts_with("local tapestry_tag0=...;"), "{out}");
        // A constructor inside an expression is not followed by `;`.
        let out = run("f(function() end)\nreturn function() end");
        assert!(out.contains("end,nil))\n"), "{out}");
        assert!(
            out.contains("return tapestry_tag0(4294967298,function() end,nil)\nend"),
            "{out}"
        );
    }

    #[test]
    fn the_tagger_s_name_is_found_in_linear_time() {
        // Names the source uses push the tagger's past them; the same
        // words in comments do not, and finding it does not rescan the
        // source per candidate.
        let names: String = (0..20_000)
            .map(|i| format!("tapestry_tag{i} = 1\n"))
            .collect();
        let comments: String = (0..20_000)
            .map(|i| format!("-- tapestry_tag{i}\n"))
            .collect();
        let start = std::time::Instant::now();
        let out = run(&format!("{names}{comments}f = function() end"));
        assert!(
            out.starts_with("local tapestry_tag20000=...;"),
            "{}",
            &out[..40]
        );
        let out = run(&format!("{comments}f = function() end"));
        assert!(out.starts_with("local tapestry_tag0=...;"));
        assert!(start.elapsed() < std::time::Duration::from_secs(10));
    }

    #[test]
    fn statements_keep_their_lines() {
        let out = run(
            "local m = {}\nfunction m.a\n.b:c(x)\nreturn self, x, m\nend\nlocal function f() return f end",
        );
        assert_eq!(
            out,
            "local tapestry_tag0=...;return function(...) local m = {}\n\
             m.a.b.c=tapestry_tag0(4294967297,function(self,\nx)\nreturn self, x, m\nend,function() return m end);\n\
             local f;f=tapestry_tag0(4294967298,function() return f end,function() return f end);\nend"
        );
    }
}
