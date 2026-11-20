//! R7 of `cargo xtask check-arch` (design.md "Memory Budget", the
//! uploader): every byte the render thread puts on the GPU queue goes
//! through one write function.
//!
//! - No workspace crate's sources call a method named `write_texture` or
//!   `write_buffer` outside [`QUEUE_WRITER`] (render's `upload` module): the
//!   uploader's write function and its unpaced forms are the only callers
//!   of the queue's writes.
//! - A call of `write_unpaced` or `write_buffer_unpaced` (the forms that
//!   read no admission gate) sits only in a function [`UNPACED`] lists, by
//!   file and name: a write that is no uploader payload's. A new caller is
//!   either an uploader payload, written through the gated form, or added
//!   here with its reason.
//!
//! Test code is not scanned: files named `tests.rs` or `*_tests.rs`, files
//! under a `tests` folder, and items under a `cfg` that names `test`.
//! Macros are not expanded.

use std::path::Path;

use anyhow::Context as _;

/// The one module that calls the queue's writes.
pub const QUEUE_WRITER: &str = "crates/render/src/upload.rs";

/// The functions that may write without the uploader's gate, by file and
/// name, each a write that is no uploader payload's.
pub const UNPACED: &[(&str, &str)] = &[
    // A selection pool's creation and row write ("Rendering and Skins",
    // chart images): its own bounds and completion.
    ("crates/render/src/chart_pool.rs", "create"),
    ("crates/render/src/chart_pool.rs", "write"),
    // The decision labels' fill: 1 MiB a boundary, its own completion.
    ("crates/render/src/decision_labels.rs", "write_rows"),
    // A chart set published whole, in a call that waits for it.
    ("crates/render/src/renderer.rs", "publish_committed_chart"),
    ("crates/render/src/renderer.rs", "publish_staged_chart"),
    ("crates/render/src/renderer.rs", "write_chart_set"),
    // The frame's own instance and projection buffers.
    ("crates/render/src/renderer.rs", "draw_on"),
    // The placeholder texture, made with the pipelines.
    ("crates/render/src/renderer.rs", "pipelines"),
    // The built-in atlas alone, and an array uploaded whole (a renderer
    // built again restoring its installations).
    ("crates/render/src/skin_atlas.rs", "builtin"),
    ("crates/render/src/skin_atlas.rs", "make"),
    // An attempt's text prepared in one call, which waits for it.
    ("crates/render/src/text_upload.rs", "prepare"),
];

/// Every R7 violation in the workspace at `root`.
///
/// # Errors
///
/// A folder or file that cannot be read, or a file that does not parse.
pub fn check(root: &Path) -> anyhow::Result<Vec<String>> {
    let crates = root.join("crates");
    let mut diagnostics = Vec::new();
    if !crates.is_dir() {
        return Ok(diagnostics);
    }
    let mut stack = Vec::new();
    for entry in
        std::fs::read_dir(&crates).with_context(|| format!("reading {}", crates.display()))?
    {
        let src = entry?.path().join("src");
        if src.is_dir() {
            stack.push(src);
        }
    }
    let mut files = Vec::new();
    while let Some(dir) = stack.pop() {
        for entry in
            std::fs::read_dir(&dir).with_context(|| format!("reading {}", dir.display()))?
        {
            let path = entry?.path();
            let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
            if path.is_dir() {
                if name != "tests" {
                    stack.push(path);
                }
            } else if name.ends_with(".rs") && name != "tests.rs" && !name.ends_with("_tests.rs") {
                files.push(path);
            }
        }
    }
    files.sort();
    for path in files {
        let text = std::fs::read_to_string(&path)
            .with_context(|| format!("reading {}", path.display()))?;
        let shown = path
            .strip_prefix(root)
            .unwrap_or(&path)
            .to_string_lossy()
            .replace('\\', "/");
        diagnostics.extend(scan(&shown, &text).with_context(|| format!("parsing {shown}"))?);
    }
    Ok(diagnostics)
}

/// The R7 violations in one file's `text`, shown as `shown`.
///
/// # Errors
///
/// A file that does not parse.
pub fn scan(shown: &str, text: &str) -> anyhow::Result<Vec<String>> {
    let file = syn::parse_file(text)?;
    let mut visit = Scan {
        shown,
        fns: Vec::new(),
        found: Vec::new(),
    };
    syn::visit::Visit::visit_file(&mut visit, &file);
    Ok(visit.found)
}

struct Scan<'a> {
    shown: &'a str,
    /// The functions the visit is inside, innermost last.
    fns: Vec<String>,
    found: Vec<String>,
}

/// Whether `attrs` hold a `cfg` naming `test`.
fn test_only(attrs: &[syn::Attribute]) -> bool {
    attrs.iter().any(|a| {
        a.path().is_ident("cfg")
            && a.meta.require_list().is_ok_and(|l| {
                l.tokens
                    .to_string()
                    .split(|c: char| !c.is_alphanumeric() && c != '_')
                    .any(|t| t == "test")
            })
    })
}

impl<'ast> syn::visit::Visit<'ast> for Scan<'_> {
    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        if !test_only(&m.attrs) {
            syn::visit::visit_item_mod(self, m);
        }
    }

    fn visit_item_fn(&mut self, f: &'ast syn::ItemFn) {
        if test_only(&f.attrs) {
            return;
        }
        self.fns.push(f.sig.ident.to_string());
        syn::visit::visit_item_fn(self, f);
        self.fns.pop();
    }

    fn visit_impl_item_fn(&mut self, f: &'ast syn::ImplItemFn) {
        if test_only(&f.attrs) {
            return;
        }
        self.fns.push(f.sig.ident.to_string());
        syn::visit::visit_impl_item_fn(self, f);
        self.fns.pop();
    }

    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        if !test_only(&i.attrs) {
            syn::visit::visit_item_impl(self, i);
        }
    }

    fn visit_expr_method_call(&mut self, c: &'ast syn::ExprMethodCall) {
        let name = c.method.to_string();
        if (name == "write_texture" || name == "write_buffer") && self.shown != QUEUE_WRITER {
            self.found.push(format!(
                "R7: {}:{} calls the queue's `{name}` outside {QUEUE_WRITER}; every write goes \
                 through the uploader's write function (design.md \"Memory Budget\", the uploader)",
                self.shown,
                c.method.span().start().line
            ));
        }
        syn::visit::visit_expr_method_call(self, c);
    }

    fn visit_expr_call(&mut self, c: &'ast syn::ExprCall) {
        if let syn::Expr::Path(p) = &*c.func
            && let Some(last) = p.path.segments.last()
        {
            let name = last.ident.to_string();
            if name == "write_unpaced" || name == "write_buffer_unpaced" {
                let inside = self.fns.last().map_or("", String::as_str);
                if !UNPACED.contains(&(self.shown, inside)) {
                    self.found.push(format!(
                        "R7: {}:{} calls `{name}` in `{inside}`, which xtask's `UNPACED` does not \
                         list: an uploader payload's write goes through the gated `write` \
                         (design.md \"Memory Budget\", the uploader)",
                        self.shown,
                        last.ident.span().start().line
                    ));
                }
            }
        }
        syn::visit::visit_expr_call(self, c);
    }
}

#[cfg(test)]
mod tests {
    use super::{QUEUE_WRITER, scan};

    /// The queue's writes outside the writer, and an unpaced write from an
    /// unlisted function, are found; the writer's own calls, a listed
    /// function's and test code are not.
    // Revert: the method-name check dropped -> the first case finds nothing.
    #[test]
    fn queue_writes_stay_in_the_writer() {
        let outside = "fn f(q: &Q) { q.write_texture(a, b, c, d); q.write_buffer(a, 0, b); }";
        let found = scan("crates/render/src/bga.rs", outside).unwrap();
        assert_eq!(found.len(), 2, "{found:#?}");
        assert!(
            found[0]
                .starts_with("R7: crates/render/src/bga.rs:1 calls the queue's `write_texture`")
        );
        assert!(scan(QUEUE_WRITER, outside).unwrap().is_empty());
        let unpaced = "impl A { fn step(&self) { crate::upload::write_unpaced(g, &b); } }";
        let found = scan("crates/render/src/renderer.rs", unpaced).unwrap();
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(found[0].contains("`write_unpaced` in `step`"), "{found:#?}");
        let listed = "impl A { fn draw_on(&self) { upload::write_buffer_unpaced(g, b, 0, x); } }";
        assert!(
            scan("crates/render/src/renderer.rs", listed)
                .unwrap()
                .is_empty()
        );
        let tests = "#[cfg(test)]\nmod t { fn f(q: &Q) { q.write_texture(a, b, c, d); } }\n\
                     #[cfg(all(test, windows))]\nfn g(q: &Q) { q.write_buffer(a, 0, b); }";
        assert!(scan("crates/render/src/bga.rs", tests).unwrap().is_empty());
    }
}
