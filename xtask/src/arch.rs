//! `cargo xtask check-arch`: the architecture rules from AGENTS.md.
//!
//! - R1: the dependency closure of `eval` (normal and build dependencies, all
//!   features, all platforms) stays inside `bms-model`, `rules`, and
//!   [`EVAL_ALLOWED_EXTERNAL`]. An allowlist, because a denylist cannot
//!   foresee every clock or I/O crate. This does not cover `std` APIs;
//!   `crates/eval/clippy.toml` rejects the listed `std` entry points
//!   (checked by `cargo xtask check-lints`).
//! - R2: only packages named in [`WINDOWS_ONLY_ALLOWED`] may set
//!   `[package.metadata.tapestry] windows-only = true`, and no other package
//!   may depend on a Windows-only package.
//! - R3: no package's `default` feature enables `dev-tools`, directly or by
//!   forwarding, and the resolved production build of each package in
//!   [`PRODUCTION`] enables `dev-tools` on no workspace package.
//! - R4: every workspace package's `license` is its entry in [`LICENSES`]
//!   (a package missing from the table fails), and no [`PERMISSIVE`] package
//!   declares a dependency of any kind on a [`GPL`] package.
//! - R5: [`GAME`] does not depend on [`LIBRARY`], directly or through
//!   another package, by a normal, build or dev dependency (design.md
//!   "Language and Crate Structure"): the player converts the library's
//!   types into the game's.
//! - R6 (design.md "beatoraja Lua Skins", Unreachable from play): no
//!   package with an `IN_PLAY` site but `player` and `skin-beatoraja` has
//!   `mlua` in its normal dependency closure; and no `player` file outside
//!   `src/screens/` names `mlua` or `skin_beatoraja::lua` in a `use` or a
//!   path (or a macro's tokens), aliases or glob-imports `skin_beatoraja`
//!   or `skin_beatoraja::lua`, or re-exports anything of `skin_beatoraja`.
//!   `use` trees are resolved to their full paths, grouped imports
//!   included; macros are not expanded. [`crate::privacy`] checks the
//!   privacy that keeps the runtime in `screens`.

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context as _, bail};
use cargo_metadata::{
    CargoOpt, DependencyKind, Metadata, MetadataCommand, Package, PackageId, Resolve, TargetKind,
};

/// The evaluator package checked by R1.
const EVAL: &str = "eval";
/// Workspace packages `eval` may depend on.
const EVAL_ALLOWED_WORKSPACE: &[&str] = &["bms-model", "rules"];
/// External crates `eval` may depend on. Additions are reviewed code changes.
const EVAL_ALLOWED_EXTERNAL: &[&str] = &[];
/// Packages allowed to be Windows-only (AGENTS.md).
pub const WINDOWS_ONLY_ALLOWED: &[&str] = &["input", "audio", "render", "player"];
/// The game core, checked by R5.
const GAME: &str = "game";
/// The library, which R5 keeps out of [`GAME`]'s dependencies.
const LIBRARY: &str = "library";
/// Feature that holds development-only code.
const DEV_TOOLS: &str = "dev-tools";
/// Production binaries and the target they ship for. Built with
/// `--no-default-features`.
const PRODUCTION: &[(&str, &str)] = &[("player", "x86_64-pc-windows-msvc")];
/// License of the L0-L2 crates and the development tooling.
const PERMISSIVE: &str = "MIT OR Apache-2.0";
/// License of L3 and above and `player`.
const GPL: &str = "GPL-3.0-or-later";
/// Every workspace package and its license (design.md "Licenses"). A new
/// package is added here with the license of its layer.
pub const LICENSES: &[(&str, &str)] = &[
    ("bms-model", PERMISSIVE),
    ("bms-parser", PERMISSIVE),
    ("rules", PERMISSIVE),
    ("rules-toml", PERMISSIVE),
    ("eval", PERMISSIVE),
    ("replay", PERMISSIVE),
    ("eval-cli", PERMISSIVE),
    ("xtask", PERMISSIVE),
    ("input", GPL),
    ("audio", GPL),
    ("game", GPL),
    ("library", GPL),
    ("skin-model", GPL),
    ("skin-beatoraja", GPL),
    ("render", GPL),
    ("player", GPL),
];

/// The license [`LICENSES`] gives package `name`.
pub fn expected_license(name: &str) -> Option<&'static str> {
    LICENSES
        .iter()
        .find(|(package, _)| *package == name)
        .map(|&(_, license)| license)
}

/// Whether a package sets `[package.metadata.tapestry] windows-only = true`.
pub fn is_windows_only(pkg: &Package) -> bool {
    pkg.metadata
        .get("tapestry")
        .and_then(|t| t.get("windows-only"))
        .and_then(|v| v.as_bool())
        == Some(true)
}

/// Runs every rule against the workspace at `manifest`. `cargo_args` are
/// passed to each nested cargo call (`--locked`, or `--offline` in tests).
/// Returns one diagnostic per violation.
pub fn run(manifest: &Path, cargo_args: &[&str]) -> anyhow::Result<Vec<String>> {
    let metadata = crate::metadata(
        MetadataCommand::new()
            .manifest_path(manifest)
            .features(CargoOpt::AllFeatures)
            .other_options(
                cargo_args
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect::<Vec<_>>(),
            ),
    )?;
    let mut diagnostics = check_metadata(&metadata)?;
    diagnostics.extend(check_player_sources(
        manifest.parent().context("the manifest has no folder")?,
    )?);
    for &(package, target) in PRODUCTION {
        diagnostics.extend(check_production_features(
            manifest, &metadata, package, target, cargo_args,
        )?);
    }
    Ok(diagnostics)
}

/// R1, R2, the static half of R3, and R4.
fn check_metadata(metadata: &Metadata) -> anyhow::Result<Vec<String>> {
    let resolve = metadata
        .resolve
        .as_ref()
        .context("cargo metadata returned no dependency graph")?;
    let graph = Graph::new(resolve);
    let members: Vec<&Package> = metadata.workspace_packages();
    let mut diagnostics = Vec::new();

    // R1
    match members.iter().find(|p| p.name.as_str() == EVAL) {
        None => diagnostics.push(format!("R1: workspace package `{EVAL}` not found")),
        Some(eval) => {
            for (id, path) in graph.closure(&eval.id, false) {
                let pkg = &metadata[&id];
                let name = pkg.name.as_str();
                let allowed = if metadata.workspace_members.contains(&id) {
                    EVAL_ALLOWED_WORKSPACE.contains(&name)
                } else {
                    EVAL_ALLOWED_EXTERNAL.contains(&name)
                };
                if !allowed {
                    diagnostics.push(format!(
                        "R1: `{EVAL}` depends on `{name}` ({}); the evaluator may depend only on \
                         {EVAL_ALLOWED_WORKSPACE:?} and EVAL_ALLOWED_EXTERNAL in xtask/src/arch.rs",
                        describe_path(metadata, &path),
                    ));
                }
            }
        }
    }

    // R2
    let windows_only: HashSet<&PackageId> = members
        .iter()
        .filter(|p| is_windows_only(p))
        .map(|p| &p.id)
        .collect();
    for pkg in &members {
        if windows_only.contains(&pkg.id) && !WINDOWS_ONLY_ALLOWED.contains(&pkg.name.as_str()) {
            diagnostics.push(format!(
                "R2: package `{}` is marked windows-only; only {WINDOWS_ONLY_ALLOWED:?} may be",
                pkg.name
            ));
        }
    }
    for pkg in &members {
        if windows_only.contains(&pkg.id) {
            continue;
        }
        for (id, path) in graph.closure(&pkg.id, true) {
            if windows_only.contains(&id) {
                diagnostics.push(format!(
                    "R2: portable package `{}` depends on Windows-only package `{}` ({})",
                    pkg.name,
                    metadata[&id].name,
                    describe_path(metadata, &path),
                ));
            }
        }
    }

    // R3 (static): `default` must not reach `dev-tools`.
    for pkg in &members {
        if let Some(chain) = default_reaches_dev_tools(&members, pkg) {
            diagnostics.push(format!(
                "R3: the `default` feature of `{}` enables `{DEV_TOOLS}` ({})",
                pkg.name,
                chain.join(" -> "),
            ));
        }
    }

    // R4
    for pkg in &members {
        let name = pkg.name.as_str();
        let expected = expected_license(name);
        match (expected, pkg.license.as_deref()) {
            (None, _) => diagnostics.push(format!(
                "R4: workspace package `{name}` is not in LICENSES in xtask/src/arch.rs"
            )),
            (Some(expected), None) => diagnostics.push(format!(
                "R4: package `{name}` has no `license`; expected `{expected}`"
            )),
            (Some(expected), Some(actual)) if actual != expected => diagnostics.push(format!(
                "R4: package `{name}` has license `{actual}`; expected `{expected}`"
            )),
            _ => {}
        }
        if expected != Some(PERMISSIVE) {
            continue;
        }
        // Declared dependencies, so optional and target-specific ones count
        // whether or not they resolve.
        for dep in &pkg.dependencies {
            if dep.path.is_some() && expected_license(&dep.name) == Some(GPL) {
                diagnostics.push(format!(
                    "R4: permissive package `{name}` has a {} dependency on GPL package `{}`",
                    dep.kind, dep.name,
                ));
            }
        }
    }

    // R5
    if let Some(game) = members.iter().find(|p| p.name.as_str() == GAME) {
        for (id, path) in graph.closure(&game.id, true) {
            if metadata[&id].name.as_str() == LIBRARY {
                diagnostics.push(format!(
                    "R5: `{GAME}` depends on `{LIBRARY}` ({}); the player converts the \
                     library's types into the game's",
                    describe_path(metadata, &path),
                ));
            }
        }
    }

    // R6 (metadata)
    let in_play = in_play_packages();
    for member in members.iter().filter(|p| in_play.contains(p.name.as_str())) {
        for (id, path) in graph.closure(&member.id, false) {
            if metadata[&id].name.as_str() == MLUA {
                diagnostics.push(format!(
                    "R6: in-play package `{}` depends on `{MLUA}` ({}); only {MLUA_ALLOWED:?} may (design.md \"beatoraja Lua Skins\")",
                    member.name,
                    describe_path(metadata, &path),
                ));
            }
        }
    }
    // R6 (metadata): the source scan knows the backend by its packages'
    // crate names, so no backend package names its library otherwise.
    for pkg in metadata
        .packages
        .iter()
        .filter(|p| BACKEND_PACKAGES.contains(&p.name.as_str()))
    {
        let canonical = pkg.name.replace('-', "_");
        for target in pkg.targets.iter().filter(|t| is_library(&t.kind)) {
            if target.name != canonical {
                diagnostics.push(format!(
                    "R6: `{}` names its library `{}`, not `{canonical}`; the backend's \
                     crates keep their package's name (design.md \"beatoraja Lua Skins\")",
                    pkg.name, target.name,
                ));
            }
        }
    }
    // R6 (metadata): nor does the player rename its dependency on one.
    if let Some(player) = members.iter().find(|p| p.name.as_str() == PLAYER) {
        for dep in &player.dependencies {
            if let Some(rename) = dep
                .rename
                .as_ref()
                .filter(|_| BACKEND_PACKAGES.contains(&dep.name.as_str()))
            {
                diagnostics.push(format!(
                    "R6: `{PLAYER}` names its dependency `{}` `{rename}`; the backend's \
                     dependencies keep their package's name (design.md \"beatoraja Lua Skins\")",
                    dep.name,
                ));
            }
        }
    }
    Ok(diagnostics)
}

/// Whether a target of `kinds` is a crate other packages link: any kind
/// but a binary, an example, a test, a bench or a build script (`lib`,
/// `rlib`, `dylib`, `cdylib`, `staticlib`, `proc-macro`).
fn is_library(kinds: &[TargetKind]) -> bool {
    kinds.iter().any(|k| {
        !matches!(
            k,
            TargetKind::Bin
                | TargetKind::Example
                | TargetKind::Test
                | TargetKind::Bench
                | TargetKind::CustomBuild
        )
    })
}

/// The package whose sources R6 scans.
const PLAYER: &str = "player";
/// The packages whose crates R6's source scan knows by name: a
/// `player` dependency on either keeps its package's name.
const BACKEND_PACKAGES: &[&str] = &["skin-beatoraja", MLUA];

/// The packages R6 lets reach `mlua`: the player, whose `screens` tree
/// holds the runtime, and the importer that defines it.
const MLUA_ALLOWED: &[&str] = &["player", "skin-beatoraja"];
/// The Lua crate R6 keeps out of the in-play packages.
const MLUA: &str = "mlua";
/// The `player` sources R6 scans, and the tree inside them that may name
/// the runtime, relative to the workspace root.
const PLAYER_SRC: &str = "crates/player/src";
const SCREENS: &str = "crates/player/src/screens";

/// R6 (metadata): the packages with an in-play site ([`crate::lints::IN_PLAY`]),
/// but [`MLUA_ALLOWED`].
fn in_play_packages() -> BTreeSet<&'static str> {
    crate::lints::IN_PLAY
        .iter()
        .filter_map(|s| {
            let path = match s {
                crate::lints::Site::File(p) | crate::lints::Site::Fns(p, _) => *p,
            };
            path.strip_prefix("crates/")?.split('/').next()
        })
        .filter(|p| !MLUA_ALLOWED.contains(p))
        .collect()
}

/// R6 (sources): every `player` file outside `screens` under `root`, each
/// use or path that names `mlua` or `skin_beatoraja::lua`, binding (an
/// `extern crate`, a `use` of the crate or of `lua` itself, an alias) or
/// glob of `skin_beatoraja` or `skin_beatoraja::lua`, and re-export of
/// anything of `skin_beatoraja`. Paths are compared with raw identifiers
/// read as plain ones and leading `crate`, `self` and `super` dropped, so
/// `crate::skin_beatoraja::lua` and `r#skin_beatoraja::r#lua` are the
/// backend too. Macros are not expanded; their tokens are scanned for such
/// a path.
///
/// A `#[path]` module (or a `cfg_attr` giving one) is refused, so no
/// module of the crate lies outside the scanned files, unless it is one of
/// [`PATH_MODULES`]; each of those targets' folders is scanned by the same
/// rules.
pub fn check_player_sources(root: &Path) -> anyhow::Result<Vec<String>> {
    let src = root.join(PLAYER_SRC);
    let mut diagnostics = Vec::new();
    if !src.is_dir() {
        return Ok(diagnostics);
    }
    let mut targets = Vec::new();
    scan_tree(
        root,
        &src,
        Some(&root.join(SCREENS)),
        &mut targets,
        &mut diagnostics,
    )?;
    for dir in targets {
        scan_tree(root, &dir, None, &mut Vec::new(), &mut diagnostics)?;
    }
    scan_screens(root, &root.join(SCREENS), &mut diagnostics)?;
    Ok(diagnostics)
}

/// R6 (sources) inside `screens`, which may name the backend: no item
/// visible outside its module (`pub`, `pub(crate)`, `pub(super)`,
/// `pub(in …)`) re-exports or aliases a path of `skin_beatoraja` or
/// `mlua`, a `use` or a `type`, so play code cannot reach the backend
/// through `screens`.
fn scan_screens(root: &Path, dir: &Path, diagnostics: &mut Vec<String>) -> anyhow::Result<()> {
    if !dir.is_dir() {
        return Ok(());
    }
    let mut stack = vec![dir.to_owned()];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .collect::<Result<_, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::path);
        for entry in entries {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let file = syn::parse_file(&text)
                    .with_context(|| format!("parsing {}", path.display()))?;
                let shown = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let mut scan = ScreensScan::default();
                syn::visit::Visit::visit_file(&mut scan, &file);
                diagnostics.extend(scan.found.into_iter().map(|(line, what)| {
                    format!(
                        "R6: {shown}:{line} {what} visible outside its module; `screens` keeps \
                         the backend to itself (design.md \"beatoraja Lua Skins\")"
                    )
                }));
            }
        }
    }
    Ok(())
}

/// Whether `segments` lie under `skin_beatoraja` or `mlua`.
fn is_backend_path(segments: &[String]) -> bool {
    unqualified(segments)
        .first()
        .is_some_and(|f| f == "skin_beatoraja" || f == MLUA)
}

/// Every full path a `use` tree names, raw identifiers read as plain.
fn use_paths(prefix: &mut Vec<String>, tree: &syn::UseTree, out: &mut Vec<(usize, Vec<String>)>) {
    match tree {
        syn::UseTree::Path(p) => {
            prefix.push(segment(&p.ident));
            use_paths(prefix, &p.tree, out);
            prefix.pop();
        }
        syn::UseTree::Group(g) => {
            for t in &g.items {
                use_paths(prefix, t, out);
            }
        }
        syn::UseTree::Name(n) => out.push((n.ident.span().start().line, full(prefix, &n.ident))),
        syn::UseTree::Rename(r) => out.push((r.ident.span().start().line, full(prefix, &r.ident))),
        syn::UseTree::Glob(g) => out.push((g.star_token.span.start().line, prefix.clone())),
    }
}

/// What R6 found inside `screens`: each backend path an item visible
/// outside its module re-exports or aliases.
#[derive(Default)]
struct ScreensScan {
    found: Vec<(usize, String)>,
}

impl<'ast> syn::visit::Visit<'ast> for ScreensScan {
    fn visit_item_use(&mut self, u: &'ast syn::ItemUse) {
        if !matches!(u.vis, syn::Visibility::Inherited) {
            let mut paths = Vec::new();
            use_paths(&mut Vec::new(), &u.tree, &mut paths);
            for (line, path) in paths.into_iter().filter(|(_, p)| is_backend_path(p)) {
                self.found
                    .push((line, format!("re-exports `{}`", path.join("::"))));
            }
        }
        syn::visit::visit_item_use(self, u);
    }

    fn visit_item_type(&mut self, t: &'ast syn::ItemType) {
        if !matches!(t.vis, syn::Visibility::Inherited) {
            let mut paths = PathCollect::default();
            syn::visit::Visit::visit_type(&mut paths, &t.ty);
            if paths.0.iter().any(|p| is_backend_path(p)) {
                self.found.push((
                    t.ident.span().start().line,
                    format!("aliases a backend type as `{}`", t.ident),
                ));
            }
        }
        syn::visit::visit_item_type(self, t);
    }
}

/// Every path a syntax tree names, raw identifiers read as plain.
#[derive(Default)]
struct PathCollect(Vec<Vec<String>>);

impl<'ast> syn::visit::Visit<'ast> for PathCollect {
    fn visit_path(&mut self, p: &'ast syn::Path) {
        self.0
            .push(p.segments.iter().map(|s| segment(&s.ident)).collect());
        syn::visit::visit_path(self, p);
    }
}

/// The `#[path]` modules of `player` R6 allows, each the test fixtures
/// another crate owns, used by `player`'s tests and declared
/// `#[cfg(test)]`: the file declaring it, the attribute's path, and its
/// owner. Each target's folder is scanned.
const PATH_MODULES: &[(&str, &str, &str)] = &[
    (
        "crates/player/src/main.rs",
        "../../render/tests/font_support/mod.rs",
        "render's test fixtures",
    ),
    (
        "crates/player/src/main.rs",
        "../../render/tests/common/mod.rs",
        "render's test fixtures",
    ),
    (
        "crates/player/src/main.rs",
        "../../render/tests/decl_support/mod.rs",
        "render's test fixtures",
    ),
    (
        "crates/player/src/main.rs",
        "../../render/tests/json_support/mod.rs",
        "render's test fixtures",
    ),
    (
        "crates/player/src/main.rs",
        "../../render/tests/outline_support/mod.rs",
        "render's test fixtures",
    ),
    (
        "crates/player/src/bests.rs",
        "../../library/tests/common/mod.rs",
        "library's test fixtures",
    ),
];

/// `p` with its `..` and `.` components resolved lexically.
fn lexical(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::ParentDir => {
                out.pop();
            }
            std::path::Component::CurDir => {}
            c => out.push(c),
        }
    }
    out
}

fn path_refusal(shown: &str, line: usize, value: Option<&str>) -> String {
    let what = value.map_or_else(
        || "a `cfg_attr` giving a module path".to_owned(),
        |v| format!("a `#[path = \"{v}\"]` module"),
    );
    format!(
        "R6: {shown}:{line} declares {what}; R6 scans the crate's own files, so only \
         the listed fixture modules may set a path (design.md \"beatoraja Lua Skins\")"
    )
}

/// Scans every `.rs` file under `dir` but `skip`. In the player's own
/// tree (`skip` given), a `#[path]` module of [`PATH_MODULES`] puts its
/// target's folder in `targets`; every other `#[path]` module is refused,
/// in a target's folder all of them.
fn scan_tree(
    root: &Path,
    dir: &Path,
    skip: Option<&Path>,
    targets: &mut Vec<PathBuf>,
    diagnostics: &mut Vec<String>,
) -> anyhow::Result<()> {
    let mut stack = vec![dir.to_owned()];
    while let Some(dir) = stack.pop() {
        let mut entries: Vec<_> = std::fs::read_dir(&dir)
            .with_context(|| format!("reading {}", dir.display()))?
            .collect::<Result<_, _>>()?;
        entries.sort_by_key(std::fs::DirEntry::path);
        for entry in entries {
            let path = entry.path();
            if skip.is_some_and(|s| path == s) {
                continue;
            }
            if path.is_dir() {
                stack.push(path);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path)
                    .with_context(|| format!("reading {}", path.display()))?;
                let file = syn::parse_file(&text)
                    .with_context(|| format!("parsing {}", path.display()))?;
                let shown = path
                    .strip_prefix(root)
                    .unwrap_or(&path)
                    .to_string_lossy()
                    .replace('\\', "/");
                let mut scan = SourceScan::default();
                syn::visit::Visit::visit_file(&mut scan, &file);
                diagnostics.extend(scan.found.into_iter().map(|(line, what)| {
                    format!(
                        "R6: {shown}:{line} {what}; only {SCREENS}/ names the Lua runtime \
                         (design.md \"beatoraja Lua Skins\")"
                    )
                }));
                for (line, value, test_only) in scan.paths {
                    let allowed = skip.is_some()
                        && test_only
                        && value.as_deref().is_some_and(|v| {
                            PATH_MODULES.iter().any(|(f, p, _)| *f == shown && *p == v)
                        });
                    match value {
                        Some(v) if allowed => {
                            let target = lexical(&path.parent().unwrap_or(root).join(v));
                            if let Some(folder) = target.parent().filter(|f| f.is_dir()) {
                                targets.push(folder.to_owned());
                            }
                        }
                        value => diagnostics.push(path_refusal(&shown, line, value.as_deref())),
                    }
                }
            }
        }
    }
    Ok(())
}

/// `segments` without its leading `crate`, `self` and `super`: the path
/// a name in the crate's root or a parent module resolves to when that
/// module binds the crate.
fn unqualified(segments: &[String]) -> &[String] {
    let skip = segments
        .iter()
        .take_while(|s| matches!(s.as_str(), "crate" | "self" | "super"))
        .count();
    segments.get(skip..).unwrap_or_default()
}

/// An identifier as a path segment: a raw identifier read as plain.
fn segment(ident: &proc_macro2::Ident) -> String {
    syn::ext::IdentExt::unraw(ident).to_string()
}

/// Whether `segments` name `mlua` or `skin_beatoraja::lua` or below.
fn names_lua(segments: &[String]) -> bool {
    match unqualified(segments) {
        [first, ..] if first == MLUA => true,
        [first, second, ..] => first == "skin_beatoraja" && second == "lua",
        _ => false,
    }
}

/// Whether `segments` is `skin_beatoraja` or `skin_beatoraja::lua`.
fn is_backend_root(segments: &[String]) -> bool {
    match unqualified(segments) {
        [only] => only == "skin_beatoraja",
        [first, second] => first == "skin_beatoraja" && second == "lua",
        _ => false,
    }
}

/// What R6's scan found in one file: per finding, its line and what it
/// is; and each module path set, its line, the path (`None` for one a
/// `cfg_attr` gives) and whether the module is `#[cfg(test)]`.
#[derive(Default)]
struct SourceScan {
    found: Vec<(usize, String)>,
    paths: Vec<(usize, Option<String>, bool)>,
}

/// An attribute's name, its raw identifier read as plain; `None` for a
/// path of more than one segment.
fn attr_name(path: &syn::Path) -> Option<String> {
    match path.segments.iter().collect::<Vec<_>>().as_slice() {
        [only] if path.leading_colon.is_none() => Some(segment(&only.ident)),
        _ => None,
    }
}

/// Calls `found` for each module path `meta` sets: a `path` attribute
/// (its value, `None` when under a `cfg_attr`), and every `path` a
/// `cfg_attr` gives, however nested. A `cfg_attr` R6 cannot read counts
/// as giving one.
fn module_paths(meta: &syn::Meta, conditional: bool, found: &mut impl FnMut(Option<String>)) {
    match attr_name(meta.path()).as_deref() {
        Some("path") => {
            let value = match meta {
                syn::Meta::NameValue(syn::MetaNameValue {
                    value:
                        syn::Expr::Lit(syn::ExprLit {
                            lit: syn::Lit::Str(s),
                            ..
                        }),
                    ..
                }) => s.value(),
                // A path R6 cannot read is never one of the listed.
                _ => String::new(),
            };
            found((!conditional).then_some(value));
        }
        Some("cfg_attr") => {
            let parsed = match meta {
                syn::Meta::List(l) => l
                    .parse_args_with(
                        syn::punctuated::Punctuated::<syn::Meta, syn::Token![,]>::parse_terminated,
                    )
                    .ok(),
                _ => None,
            };
            match parsed {
                // The predicate first, then the attributes it gives.
                Some(metas) => {
                    for m in metas.iter().skip(1) {
                        module_paths(m, true, found);
                    }
                }
                None => found(None),
            }
        }
        _ => {}
    }
}

impl SourceScan {
    /// Every path a `use` tree names, with its prefix.
    fn use_tree(&mut self, prefix: &mut Vec<String>, tree: &syn::UseTree, public: bool) {
        match tree {
            syn::UseTree::Path(p) => {
                prefix.push(segment(&p.ident));
                self.use_tree(prefix, &p.tree, public);
                prefix.pop();
            }
            syn::UseTree::Group(g) => {
                for t in &g.items {
                    self.use_tree(prefix, t, public);
                }
            }
            syn::UseTree::Name(n) => {
                let full = full(prefix, &n.ident);
                self.used(&full, n.ident.span(), public, None);
            }
            syn::UseTree::Rename(r) => {
                let full = full(prefix, &r.ident);
                self.used(&full, r.ident.span(), public, Some(&r.rename));
            }
            syn::UseTree::Glob(g) => {
                let line = g.star_token.span.start().line;
                if names_lua(prefix) || is_backend_root(prefix) {
                    self.found
                        .push((line, format!("glob-imports `{}::*`", prefix.join("::"))));
                } else if public && is_skin_crate(prefix) {
                    self.found
                        .push((line, format!("re-exports `{}::*`", prefix.join("::"))));
                }
            }
        }
    }

    fn used(
        &mut self,
        full: &[String],
        span: proc_macro2::Span,
        public: bool,
        rename: Option<&syn::Ident>,
    ) {
        let line = span.start().line;
        let name = full.join("::");
        if let Some(r) = rename.filter(|_| is_backend_root(full)) {
            self.found
                .push((line, format!("aliases `{name}` as `{r}`")));
        } else if names_lua(full) {
            self.found.push((line, format!("uses `{name}`")));
        } else if is_backend_root(full) {
            self.found.push((line, format!("binds `{name}`")));
        } else if public && is_skin_crate(full) {
            self.found.push((line, format!("re-exports `{name}`")));
        }
    }

    /// Paths in a macro's tokens: `mlua ::` or `skin_beatoraja :: lua`.
    fn tokens(&mut self, tokens: proc_macro2::TokenStream) {
        let flat: Vec<proc_macro2::TokenTree> = tokens.into_iter().collect();
        for (i, t) in flat.iter().enumerate() {
            match t {
                proc_macro2::TokenTree::Group(g) => self.tokens(g.stream()),
                proc_macro2::TokenTree::Ident(id) => {
                    let id = segment(id);
                    let after = |k: usize| flat.get(i + k);
                    let colons = matches!(
                        (after(1), after(2)),
                        (
                            Some(proc_macro2::TokenTree::Punct(a)),
                            Some(proc_macro2::TokenTree::Punct(b))
                        ) if a.as_char() == ':' && b.as_char() == ':'
                    );
                    let lua_next = matches!(
                        after(3),
                        Some(proc_macro2::TokenTree::Ident(n)) if segment(n) == "lua"
                    );
                    if colons && (id == MLUA || (id == "skin_beatoraja" && lua_next)) {
                        self.found.push((
                            t.span().start().line,
                            format!("names `{id}::…` in a macro's tokens"),
                        ));
                    }
                }
                _ => {}
            }
        }
    }
}

fn full(prefix: &[String], last: &syn::Ident) -> Vec<String> {
    let mut full = prefix.to_vec();
    if last != "self" {
        full.push(segment(last));
    }
    full
}

/// Whether `segments` is under `skin_beatoraja`.
fn is_skin_crate(segments: &[String]) -> bool {
    unqualified(segments)
        .first()
        .is_some_and(|f| f == "skin_beatoraja")
}

impl<'ast> syn::visit::Visit<'ast> for SourceScan {
    fn visit_item_use(&mut self, u: &'ast syn::ItemUse) {
        let public = !matches!(u.vis, syn::Visibility::Inherited);
        self.use_tree(&mut Vec::new(), &u.tree, public);
    }

    fn visit_item_extern_crate(&mut self, c: &'ast syn::ItemExternCrate) {
        let name = segment(&c.ident);
        let line = c.ident.span().start().line;
        let backend = name == MLUA || name == "skin_beatoraja";
        match &c.rename {
            Some((_, rename)) if backend => {
                self.found
                    .push((line, format!("aliases `{name}` as `{rename}`")));
            }
            None if name == MLUA => self.found.push((line, format!("uses `{MLUA}`"))),
            // Binds the crate in this module: `crate::skin_beatoraja::…`
            // from any file then names it.
            None if backend => self
                .found
                .push((line, format!("binds `{name}` with `extern crate`"))),
            _ => {}
        }
    }

    fn visit_path(&mut self, p: &'ast syn::Path) {
        let segments: Vec<String> = p.segments.iter().map(|s| segment(&s.ident)).collect();
        if names_lua(&segments) {
            let line = p
                .segments
                .first()
                .map_or(0, |s| s.ident.span().start().line);
            self.found
                .push((line, format!("names `{}`", segments.join("::"))));
        }
        syn::visit::visit_path(self, p);
    }

    fn visit_item_mod(&mut self, m: &'ast syn::ItemMod) {
        let line = m.ident.span().start().line;
        let test_only = m.attrs.iter().any(|a| {
            attr_name(a.path()).as_deref() == Some("cfg")
                && matches!(&a.meta, syn::Meta::List(l) if l.tokens.to_string() == "test")
        });
        for attr in &m.attrs {
            module_paths(&attr.meta, false, &mut |value| {
                self.paths.push((line, value, test_only));
            });
        }
        syn::visit::visit_item_mod(self, m);
    }

    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        self.tokens(m.tokens.clone());
        syn::visit::visit_macro(self, m);
    }
}

/// The resolved dependency graph: normal and build edges, plus dev edges
/// marked as such (followed only from the root, for R2).
struct Graph<'a> {
    /// `(dependency, is_dev_only)` per package.
    edges: HashMap<&'a PackageId, Vec<(&'a PackageId, bool)>>,
}

impl<'a> Graph<'a> {
    fn new(resolve: &'a Resolve) -> Self {
        let edges = resolve
            .nodes
            .iter()
            .map(|node| {
                let deps = node
                    .deps
                    .iter()
                    .filter_map(|dep| {
                        let non_dev = dep.dep_kinds.iter().any(|k| {
                            matches!(k.kind, DependencyKind::Normal | DependencyKind::Build)
                        });
                        let dev = dep
                            .dep_kinds
                            .iter()
                            .any(|k| k.kind == DependencyKind::Development);
                        (non_dev || dev).then_some((&dep.pkg, !non_dev))
                    })
                    .collect();
                (&node.id, deps)
            })
            .collect();
        Self { edges }
    }

    /// Every package reachable from `root` through normal and build edges,
    /// with the path from `root`. With `root_dev`, the root's own
    /// dev-dependencies are followed too (they are built by `cargo test`).
    fn closure(&self, root: &'a PackageId, root_dev: bool) -> Vec<(PackageId, Vec<PackageId>)> {
        let mut parent: HashMap<&PackageId, &PackageId> = HashMap::new();
        let mut seen: HashSet<&PackageId> = HashSet::from([root]);
        let mut queue = VecDeque::from([root]);
        let mut found = Vec::new();
        while let Some(id) = queue.pop_front() {
            for &(dep, dev_only) in self.edges.get(id).into_iter().flatten() {
                if dev_only && !(root_dev && id == root) {
                    continue;
                }
                if seen.insert(dep) {
                    parent.insert(dep, id);
                    queue.push_back(dep);
                    let mut path = vec![dep.clone()];
                    let mut cur = dep;
                    while let Some(&p) = parent.get(cur) {
                        path.push(p.clone());
                        cur = p;
                    }
                    path.reverse();
                    found.push((dep.clone(), path));
                }
            }
        }
        found
    }
}

fn describe_path(metadata: &Metadata, path: &[PackageId]) -> String {
    let names: Vec<&str> = path.iter().map(|id| metadata[id].name.as_str()).collect();
    format!("via {}", names.join(" -> "))
}

/// Follows the `default` feature of `start` through feature forwarding
/// (`feat`, `dep:x`, `x/feat`, `x?/feat`) into workspace packages. Returns
/// the chain that reaches a feature named `dev-tools`.
fn default_reaches_dev_tools(members: &[&Package], start: &Package) -> Option<Vec<String>> {
    let find = |name: &str| members.iter().copied().find(|p| p.name.as_str() == name);
    let mut seen: HashSet<(String, String)> = HashSet::new();
    let mut stack: Vec<(&Package, String, Vec<String>)> =
        vec![(start, "default".to_owned(), Vec::new())];
    while let Some((pkg, feature, chain)) = stack.pop() {
        if !seen.insert((pkg.name.to_string(), feature.clone())) {
            continue;
        }
        let mut chain = chain;
        chain.push(format!("{}/{}", pkg.name, feature));
        if feature == DEV_TOOLS {
            return Some(chain);
        }
        let Some(entries) = pkg.features.get(&feature) else {
            continue;
        };
        for entry in entries {
            // Resolve the entry to (dependency key, feature to enable on it).
            let (dep_key, dep_feature) = if let Some(dep) = entry.strip_prefix("dep:") {
                (dep, None)
            } else if let Some((dep, f)) = entry.split_once('/') {
                (dep.trim_end_matches('?'), Some(f))
            } else if pkg.features.contains_key(entry) {
                stack.push((pkg, entry.clone(), chain.clone()));
                continue;
            } else {
                (entry.as_str(), None)
            };
            let Some(dep) = pkg.dependencies.iter().find(|d| {
                d.kind != DependencyKind::Development
                    && d.rename.as_deref().unwrap_or(&d.name) == dep_key
            }) else {
                continue;
            };
            let Some(target) = find(&dep.name) else {
                continue;
            };
            let mut enabled: BTreeSet<String> = dep.features.iter().cloned().collect();
            if dep.uses_default_features {
                enabled.insert("default".to_owned());
            }
            enabled.extend(dep_feature.map(str::to_owned));
            for f in enabled {
                stack.push((target, f, chain.clone()));
            }
        }
    }
    None
}

/// R3 (resolved): the features `cargo build --release -p <package>
/// --no-default-features --target <target>` would enable. Dev-dependencies
/// are excluded, as in that build. Build-dependencies resolve for the host
/// running this check, so it must also run on the production host (Windows).
fn check_production_features(
    manifest: &Path,
    metadata: &Metadata,
    package: &str,
    target: &str,
    cargo_args: &[&str],
) -> anyhow::Result<Vec<String>> {
    let members: HashSet<&str> = metadata
        .workspace_packages()
        .iter()
        .map(|p| p.name.as_str())
        .collect();
    if !members.contains(package) {
        return Ok(vec![format!(
            "R3: production package `{package}` not found in the workspace"
        )]);
    }
    let output = Command::new(crate::cargo())
        .arg("tree")
        .arg("--manifest-path")
        .arg(manifest)
        .args(["-p", package, "--no-default-features", "--target", target])
        .args(["-e", "normal,build,features", "--prefix", "none"])
        .args(["--format", "{p}|{f}"])
        .args(cargo_args)
        .output()
        .context("failed to run cargo tree")?;
    if !output.status.success() {
        bail!(
            "cargo tree failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut offenders = BTreeSet::new();
    for line in stdout.lines() {
        // Package lines are `<name> v<version> (<source>)|<features>`;
        // feature-node lines have no `|`.
        let Some((pkg, features)) = line.split_once('|') else {
            continue;
        };
        let name = pkg.split_whitespace().next().unwrap_or_default();
        let enabled = features.trim().trim_end_matches("(*)").split(',');
        if members.contains(name) && enabled.map(str::trim).any(|f| f == DEV_TOOLS) {
            offenders.insert(name.to_owned());
        }
    }
    Ok(offenders
        .into_iter()
        .map(|name| {
            format!(
                "R3: production build of `{package}` (--no-default-features, {target}) \
                 enables `{DEV_TOOLS}` on `{name}`"
            )
        })
        .collect())
}

#[cfg(test)]
mod tests {
    //! Each test writes a fixture workspace (see `crate::fixture`), changes one
    //! thing, and runs the real `cargo metadata` / `cargo tree` on it.

    use std::path::Path;

    use super::run;
    use crate::fixture::{Crate, baseline, lib, set, write_workspace};

    fn check(crates: &[Crate]) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        write_workspace(dir.path(), crates);
        run_fixture(dir.path())
    }

    fn run_fixture(dir: &Path) -> Vec<String> {
        run(&dir.join("Cargo.toml"), &["--offline"]).unwrap()
    }

    /// Like `check`, with the `license` of package `name` set to `license`
    /// (`None` removes it) after the fixture writes the expected one.
    fn check_with_license(crates: &[Crate], name: &str, license: Option<&str>) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        write_workspace(dir.path(), crates);
        let manifest = dir.path().join("crates").join(name).join("Cargo.toml");
        let text = std::fs::read_to_string(&manifest).unwrap();
        let line = text.lines().find(|l| l.starts_with("license = ")).unwrap();
        let replacement = license
            .map(|l| format!("license = \"{l}\""))
            .unwrap_or_default();
        std::fs::write(&manifest, text.replacen(line, &replacement, 1)).unwrap();
        run_fixture(dir.path())
    }

    fn assert_single(diagnostics: &[String], expected: &[&str]) {
        assert_eq!(diagnostics.len(), 1, "{diagnostics:#?}");
        for part in expected {
            assert!(
                diagnostics[0].contains(part),
                "{part:?} not in {diagnostics:#?}"
            );
        }
    }

    #[test]
    fn baseline_is_clean() {
        assert_eq!(check(&baseline()), Vec::<String>::new());
    }

    #[test]
    fn missing_manifest_error_keeps_the_cargo_metadata_error() {
        let dir = tempfile::tempdir().unwrap();
        let e = run(&dir.path().join("Cargo.toml"), &["--offline"]).unwrap_err();
        let text = format!("{e:#}");
        assert!(
            text.starts_with("cargo metadata failed: `cargo metadata` exited with an error: "),
            "{text}"
        );
        assert!(
            matches!(
                e.root_cause().downcast_ref(),
                Some(cargo_metadata::Error::CargoMetadata { .. })
            ),
            "{e:?}"
        );
    }

    #[test]
    fn r1_rejects_optional_target_specific_dependency_of_eval() {
        let mut crates = baseline();
        set(
            &mut crates,
            "eval",
            "[dependencies]\nbms-model = { path = \"../bms-model\" }\n\
             rules = { path = \"../rules\" }\n\
             [target.'cfg(target_os = \"none\")'.dependencies]\n\
             bms-parser = { path = \"../bms-parser\", optional = true }\n",
        );
        assert_single(
            &check(&crates),
            &[
                "R1:",
                "`eval` depends on `bms-parser`",
                "via eval -> bms-parser",
            ],
        );
    }

    #[test]
    fn r1_rejects_transitive_dependency_of_eval() {
        let mut crates = baseline();
        set(
            &mut crates,
            "rules",
            "[build-dependencies]\nbms-parser = { path = \"../bms-parser\" }\n",
        );
        assert_single(&check(&crates), &["R1:", "via eval -> rules -> bms-parser"]);
    }

    #[test]
    fn r1_ignores_dev_dependencies_of_eval() {
        let mut crates = baseline();
        set(
            &mut crates,
            "eval",
            "[dependencies]\nrules = { path = \"../rules\" }\n\
             [dev-dependencies]\nbms-parser = { path = \"../bms-parser\" }\n",
        );
        assert_eq!(check(&crates), Vec::<String>::new());
    }

    #[test]
    fn r2_rejects_windows_only_label_outside_allowed_packages() {
        let mut crates = baseline();
        set(
            &mut crates,
            "game",
            "[package.metadata.tapestry]\nwindows-only = true\n",
        );
        assert_single(
            &check(&crates),
            &["R2:", "package `game` is marked windows-only"],
        );
    }

    #[test]
    fn r2_rejects_portable_package_depending_on_windows_only_package() {
        let mut crates = baseline();
        crates.push(lib(
            "render",
            "[package.metadata.tapestry]\nwindows-only = true\n",
        ));
        set(
            &mut crates,
            "game",
            "[dependencies]\nrender = { path = \"../render\" }\n",
        );
        assert_single(
            &check(&crates),
            &[
                "R2:",
                "portable package `game` depends on Windows-only package `render`",
            ],
        );
    }

    #[test]
    fn r2_rejects_dev_dependency_on_windows_only_package() {
        let mut crates = baseline();
        crates.push(lib(
            "render",
            "[package.metadata.tapestry]\nwindows-only = true\n",
        ));
        set(
            &mut crates,
            "game",
            "[dev-dependencies]\nrender = { path = \"../render\" }\n",
        );
        assert_single(
            &check(&crates),
            &["R2:", "`game` depends on Windows-only package `render`"],
        );
    }

    #[test]
    fn r3_rejects_dev_tools_in_default() {
        let mut crates = baseline();
        set(
            &mut crates,
            "player",
            "[package.metadata.tapestry]\nwindows-only = true\n\
             [features]\ndefault = [\"dev-tools\"]\ndev-tools = []\n",
        );
        assert_single(
            &check(&crates),
            &["R3:", "`default` feature of `player` enables `dev-tools`"],
        );
    }

    #[test]
    fn r3_rejects_dev_tools_forwarded_from_default() {
        let mut crates = baseline();
        crates.push(lib(
            "render",
            "[package.metadata.tapestry]\nwindows-only = true\n[features]\ndev-tools = []\n",
        ));
        set(
            &mut crates,
            "player",
            "[package.metadata.tapestry]\nwindows-only = true\n\
             [features]\ndefault = [\"debug\"]\ndebug = [\"render/dev-tools\"]\n\
             [dependencies]\nrender = { path = \"../render\" }\n",
        );
        assert_single(
            &check(&crates),
            &[
                "R3:",
                "`player`",
                "player/default -> player/debug -> render/dev-tools",
            ],
        );
    }

    #[test]
    fn r3_rejects_dev_tools_requested_by_normal_dependency_in_production() {
        let mut crates = baseline();
        crates.push(lib(
            "render",
            "[package.metadata.tapestry]\nwindows-only = true\n[features]\ndev-tools = []\n",
        ));
        set(
            &mut crates,
            "player",
            "[package.metadata.tapestry]\nwindows-only = true\n\
             [dependencies]\nrender = { path = \"../render\", features = [\"dev-tools\"] }\n",
        );
        assert_single(
            &check(&crates),
            &[
                "R3:",
                "production build of `player`",
                "enables `dev-tools` on `render`",
            ],
        );
    }

    #[test]
    fn r3_allows_dev_tools_requested_by_dev_dependency() {
        let mut crates = baseline();
        crates.push(lib(
            "render",
            "[package.metadata.tapestry]\nwindows-only = true\n[features]\ndev-tools = []\n",
        ));
        set(
            &mut crates,
            "player",
            "[package.metadata.tapestry]\nwindows-only = true\n\
             [dependencies]\nrender = { path = \"../render\" }\n\
             [dev-dependencies]\nrender = { path = \"../render\", features = [\"dev-tools\"] }\n",
        );
        assert_eq!(check(&crates), Vec::<String>::new());
    }

    /// Build-dependencies resolve for the host, not for `--target`, so this
    /// case is only visible when check-arch runs on a Windows host (the
    /// `windows` CI job runs it for that reason).
    #[cfg(windows)]
    #[test]
    fn r3_rejects_dev_tools_requested_by_windows_build_dependency() {
        let mut crates = baseline();
        crates.push(lib(
            "render",
            "[package.metadata.tapestry]\nwindows-only = true\n[features]\ndev-tools = []\n",
        ));
        set(
            &mut crates,
            "player",
            "[package.metadata.tapestry]\nwindows-only = true\n\
             [target.'cfg(windows)'.build-dependencies]\n\
             render = { path = \"../render\", features = [\"dev-tools\"] }\n",
        );
        let dir = tempfile::tempdir().unwrap();
        write_workspace(dir.path(), &crates);
        std::fs::write(dir.path().join("crates/player/build.rs"), "fn main() {}\n").unwrap();
        let diagnostics = run(&dir.path().join("Cargo.toml"), &["--offline"]).unwrap();
        assert_single(
            &diagnostics,
            &[
                "R3:",
                "production build of `player`",
                "enables `dev-tools` on `render`",
            ],
        );
    }

    #[test]
    fn r4_rejects_wrong_license() {
        assert_single(
            &check_with_license(&baseline(), "eval", Some("GPL-3.0-or-later")),
            &[
                "R4:",
                "package `eval` has license `GPL-3.0-or-later`; expected `MIT OR Apache-2.0`",
            ],
        );
    }

    #[test]
    fn r4_rejects_missing_license() {
        assert_single(
            &check_with_license(&baseline(), "game", None),
            &["R4:", "package `game` has no `license`"],
        );
    }

    #[test]
    fn r4_rejects_package_missing_from_table() {
        let mut crates = baseline();
        crates.push(lib("tool", "license = \"MIT OR Apache-2.0\"\n"));
        assert_single(
            &check(&crates),
            &["R4:", "workspace package `tool` is not in LICENSES"],
        );
    }

    /// `xtask` (permissive) declaring `game` (GPL) in each way R4 must see.
    #[test]
    fn r4_rejects_permissive_package_depending_on_gpl_package() {
        let cases = [
            ("[dependencies]\ngame = { path = \"../game\" }\n", "normal"),
            (
                "[build-dependencies]\ngame = { path = \"../game\" }\n",
                "build",
            ),
            ("[dev-dependencies]\ngame = { path = \"../game\" }\n", "dev"),
            (
                "[dependencies]\ngame = { path = \"../game\", optional = true }\n",
                "normal",
            ),
            (
                "[target.'cfg(target_os = \"none\")'.dependencies]\n\
                 game = { path = \"../game\" }\n",
                "normal",
            ),
        ];
        for (body, kind) in cases {
            let mut crates = baseline();
            set(&mut crates, "xtask", body);
            assert_single(
                &check(&crates),
                &[
                    "R4:",
                    &format!(
                        "permissive package `xtask` has a {kind} dependency on GPL package `game`"
                    ),
                ],
            );
        }
    }

    /// `game` reaching `library` directly, by a dev dependency, and
    /// through another package.
    #[test]
    fn r5_rejects_game_depending_on_library() {
        let cases = [
            (
                "game",
                "[dependencies]\nlibrary = { path = \"../library\" }\n",
                "via game -> library",
            ),
            (
                "game",
                "[dev-dependencies]\nlibrary = { path = \"../library\" }\n",
                "via game -> library",
            ),
            (
                "audio",
                "[dependencies]\nlibrary = { path = \"../library\" }\n",
                "via game -> audio -> library",
            ),
        ];
        for (package, body, path) in cases {
            let mut crates = baseline();
            set(&mut crates, package, body);
            assert_single(
                &check(&crates),
                &["R5:", "`game` depends on `library`", path],
            );
        }
    }

    #[test]
    fn r4_allows_gpl_package_depending_on_permissive_package() {
        let mut crates = baseline();
        set(
            &mut crates,
            "audio",
            "[dependencies]\nbms-model = { path = \"../bms-model\" }\n\
             [build-dependencies]\nbms-parser = { path = \"../bms-parser\" }\n\
             [dev-dependencies]\nxtask = { path = \"../xtask\" }\n",
        );
        assert_eq!(check(&crates), Vec::<String>::new());
    }

    /// The R6 diagnostics of `crates`.
    fn r6(crates: &[Crate]) -> Vec<String> {
        check(crates)
            .into_iter()
            .filter(|d| d.starts_with("R6:"))
            .collect()
    }

    #[test]
    fn r6_rejects_mlua_in_an_in_play_package_directly_and_through_another() {
        let mut crates = baseline();
        crates.push(lib("mlua", ""));
        set(
            &mut crates,
            "audio",
            "[dependencies]\nbms-model = { path = \"../bms-model\" }\n\
             mlua = { path = \"../mlua\" }\n",
        );
        let found = r6(&crates);
        assert_eq!(found.len(), 2, "{found:#?}");
        assert!(
            found
                .iter()
                .any(|d| d.contains("`audio` depends on `mlua` (via audio -> mlua)")),
            "{found:#?}"
        );
        assert!(
            found
                .iter()
                .any(|d| d.contains("`game` depends on `mlua` (via game -> audio -> mlua)")),
            "{found:#?}"
        );
    }

    /// A backend package whose library has another name would let the
    /// player name the backend under a name the source scan does not know.
    #[test]
    fn r6_rejects_a_backend_library_named_otherwise() {
        for (package, canonical) in [("skin-beatoraja", "skin_beatoraja"), ("mlua", "mlua")] {
            for kind in [
                "",
                "crate-type = [\"rlib\"]\n",
                "crate-type = [\"dylib\"]\n",
                "proc-macro = true\n",
            ] {
                let mut crates = baseline();
                crates.push(lib(package, &format!("[lib]\nname = \"backend\"\n{kind}")));
                set(
                    &mut crates,
                    "player",
                    &format!(
                        "[package.metadata.tapestry]\nwindows-only = true\n\
                         [features]\ndefault = []\ndev-tools = []\n\
                         [dependencies]\neval = {{ path = \"../eval\" }}\n\
                         {package} = {{ path = \"../{package}\" }}\n"
                    ),
                );
                let found = r6(&crates);
                assert_eq!(found.len(), 1, "{package} {kind}: {found:#?}");
                assert!(
                    found[0].contains(&format!(
                        "`{package}` names its library `backend`, not `{canonical}`"
                    )),
                    "{package} {kind}: {found:#?}"
                );
            }
        }
    }

    /// `screens` may name the backend, but nothing it makes visible
    /// outside its module may re-export or alias it: play code would name
    /// the backend through `crate::screens`.
    #[test]
    fn r6_refuses_backend_re_exports_out_of_screens() {
        for (text, what) in [
            (
                "pub(crate) use skin_beatoraja::lua as backend;\n",
                "re-exports `skin_beatoraja::lua`",
            ),
            ("pub use ::mlua::Lua;\n", "re-exports `mlua::Lua`"),
            (
                "pub(super) use skin_beatoraja::lua::*;\n",
                "re-exports `skin_beatoraja::lua`",
            ),
            (
                "pub(in crate::screens) use r#skin_beatoraja::{lua::runtime};\n",
                "re-exports `skin_beatoraja::lua::runtime`",
            ),
            (
                "pub(crate) type Rt = skin_beatoraja::lua::runtime::ScreenRuntime;\n",
                "aliases a backend type as `Rt`",
            ),
        ] {
            let found = scan_files(&[
                ("screens/mod.rs", text),
                (
                    "render_side.rs",
                    "fn f(_: crate::screens::backend::runtime::ScreenRuntime) {}\n",
                ),
            ]);
            assert_eq!(found.len(), 1, "{text}: {found:#?}");
            assert!(
                found[0].starts_with("R6: crates/player/src/screens/mod.rs:1 ")
                    && found[0].contains(what),
                "{text}: {found:#?}"
            );
        }
        // Private uses and a re-export of the crate's own items pass.
        let found = scan_files(&[(
            "screens/mod.rs",
            "use skin_beatoraja::lua::runtime::ScreenRuntime;\npub(crate) use script::Scripts;\n\
             type Rt = skin_beatoraja::lua::runtime::ScreenRuntime;\n",
        )]);
        assert_eq!(found, Vec::<String>::new());
    }

    /// A renamed backend dependency would let the player name the backend
    /// under a name the source scan does not know.
    #[test]
    fn r6_rejects_a_renamed_backend_dependency_of_the_player() {
        for dep in [
            "backend = { package = \"skin-beatoraja\", path = \"../skin-beatoraja\" }",
            "lua = { package = \"mlua\", path = \"../mlua\" }",
        ] {
            let mut crates = baseline();
            crates.push(lib("mlua", ""));
            crates.push(lib("skin-beatoraja", ""));
            set(
                &mut crates,
                "player",
                &format!(
                    "[package.metadata.tapestry]\nwindows-only = true\n\
                     [features]\ndefault = []\ndev-tools = []\n\
                     [dependencies]\neval = {{ path = \"../eval\" }}\n{dep}\n"
                ),
            );
            let found = r6(&crates);
            assert_eq!(found.len(), 1, "{dep}: {found:#?}");
            assert!(
                found[0].contains("`player` names its dependency"),
                "{found:#?}"
            );
        }
        // Unrenamed, it passes.
        let mut crates = baseline();
        crates.push(lib("skin-beatoraja", ""));
        set(
            &mut crates,
            "player",
            "[package.metadata.tapestry]\nwindows-only = true\n\
             [features]\ndefault = []\ndev-tools = []\n\
             [dependencies]\neval = { path = \"../eval\" }\n\
             skin-beatoraja = { path = \"../skin-beatoraja\" }\n",
        );
        assert_eq!(r6(&crates), Vec::<String>::new());
    }

    #[test]
    fn r6_lets_the_player_reach_mlua() {
        let mut crates = baseline();
        crates.push(lib("mlua", ""));
        set(
            &mut crates,
            "player",
            "[package.metadata.tapestry]\nwindows-only = true\n\
             [features]\ndefault = []\ndev-tools = []\n\
             [dependencies]\neval = { path = \"../eval\" }\nmlua = { path = \"../mlua\" }\n",
        );
        assert_eq!(r6(&crates), Vec::<String>::new());
    }

    /// R6's scan of a `player` tree holding `file` outside `screens`, and
    /// a `screens` file naming the runtime.
    fn scan(file: &str) -> Vec<String> {
        scan_files(&[("main.rs", file)])
    }

    /// R6's scan of a `player` tree holding `files` (relative to its
    /// `src`) outside `screens`, and a `screens` file naming the runtime.
    fn scan_files(files: &[(&str, &str)]) -> Vec<String> {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("crates/player/src");
        std::fs::create_dir_all(src.join("screens/script")).unwrap();
        std::fs::write(
            src.join("screens/script/tests.rs"),
            "use mlua::Lua;\nuse skin_beatoraja::lua::runtime::ScreenRuntime;\nuse skin_beatoraja::lua;\n",
        )
        .unwrap();
        for (path, text) in files {
            std::fs::write(src.join(path), text).unwrap();
        }
        super::check_player_sources(dir.path()).unwrap()
    }

    /// A `#[path]` module outside the listed fixtures is refused, whatever
    /// its target: a file outside the scanned tree, a non-`.rs` file, one
    /// given through `cfg_attr`, and a listed path declared from another
    /// file. A listed one is allowed, and its target's folder scanned.
    #[test]
    fn r6_refuses_path_modules_but_the_listed_fixtures() {
        for (file, text, what) in [
            (
                "main.rs",
                "#[path = \"../../../outside/helper.rs\"]\nmod helper;\n",
                "a `#[path = \"../../../outside/helper.rs\"]` module",
            ),
            (
                "main.rs",
                "#[path = \"helper.txt\"]\nmod helper;\n",
                "a `#[path = \"helper.txt\"]` module",
            ),
            (
                "main.rs",
                "#[cfg_attr(test, path = \"helper.rs\")]\nmod helper;\n",
                "a `cfg_attr` giving a module path",
            ),
            (
                "render_side.rs",
                "#[path = \"../../render/tests/common/mod.rs\"]\nmod render_common;\n",
                "a `#[path = \"../../render/tests/common/mod.rs\"]` module",
            ),
            (
                "main.rs",
                "#[cfg_attr(all(), cfg_attr(all(), path = \"../../../outside/helper.rs\"))]\nmod helper;\n",
                "a `cfg_attr` giving a module path",
            ),
            (
                "main.rs",
                "#[r#path = \"../../../outside/helper.rs\"]\nmod helper;\n",
                "a `#[path = \"../../../outside/helper.rs\"]` module",
            ),
            (
                "main.rs",
                "#[r#cfg_attr(test, r#path = \"helper.rs\")]\nmod helper;\n",
                "a `cfg_attr` giving a module path",
            ),
            (
                "main.rs",
                "#[path = \"../../render/tests/common/mod.rs\"]\nmod render_common;\n",
                "a `#[path = \"../../render/tests/common/mod.rs\"]` module",
            ),
        ] {
            let found = scan_files(&[(file, text)]);
            assert_eq!(found.len(), 1, "{text}: {found:#?}");
            assert!(
                found[0].starts_with(&format!("R6: crates/player/src/{file}:"))
                    && found[0].contains(what),
                "{text}: {found:#?}"
            );
        }
        // A listed fixture module is allowed; its folder is scanned.
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("crates/player/src");
        let fixture = dir.path().join("crates/render/tests/common");
        std::fs::create_dir_all(&src).unwrap();
        std::fs::create_dir_all(&fixture).unwrap();
        std::fs::write(
            src.join("main.rs"),
            "#[cfg(test)]\n#[path = \"../../render/tests/common/mod.rs\"]\nmod render_common;\n",
        )
        .unwrap();
        std::fs::write(fixture.join("mod.rs"), "pub mod more;\n").unwrap();
        std::fs::write(fixture.join("more.rs"), "").unwrap();
        assert_eq!(
            super::check_player_sources(dir.path()).unwrap(),
            Vec::<String>::new()
        );
        // Inside the fixture's folder, every path module is refused, a
        // nested or raw one too.
        for text in [
            "#[cfg(test)]\n#[path = \"../../render/tests/common/mod.rs\"]\nmod again;\n",
            "#[cfg_attr(all(), cfg_attr(all(), path = \"x.rs\"))]\nmod x;\n",
            "#[r#path = \"x.rs\"]\nmod x;\n",
        ] {
            std::fs::write(fixture.join("more.rs"), text).unwrap();
            let found = super::check_player_sources(dir.path()).unwrap();
            assert_eq!(found.len(), 1, "{text}: {found:#?}");
            assert!(
                found[0].starts_with("R6: crates/render/tests/common/more.rs:")
                    && found[0].contains("declares"),
                "{text}: {found:#?}"
            );
        }
        std::fs::write(fixture.join("more.rs"), "").unwrap();
        std::fs::write(
            fixture.join("more.rs"),
            "pub use skin_beatoraja::lua as backend;\n",
        )
        .unwrap();
        let found = super::check_player_sources(dir.path()).unwrap();
        assert_eq!(found.len(), 1, "{found:#?}");
        assert!(
            found[0].starts_with("R6: crates/render/tests/common/more.rs:1 aliases"),
            "{found:#?}"
        );
    }

    /// A crate root that binds the backend, and a sibling file that names
    /// it through the binding, a qualifier or raw identifiers: each file's
    /// form is found in that file (Codex S7-07b r1 #1).
    #[test]
    fn r6_rejects_bindings_qualifiers_and_raw_identifiers_across_files() {
        for (root, root_what, sibling, sibling_what) in [
            (
                "extern crate skin_beatoraja;\nmod render_side;\n",
                Some("binds `skin_beatoraja` with `extern crate`"),
                "use crate::skin_beatoraja::lua as backend;\nfn f(_: backend::runtime::ScreenRuntime) {}\n",
                "aliases `crate::skin_beatoraja::lua` as `backend`",
            ),
            (
                "extern crate skin_beatoraja;\nmod render_side;\n",
                Some("binds `skin_beatoraja` with `extern crate`"),
                "fn f(_: crate::skin_beatoraja::lua::runtime::ScreenRuntime) {}\n",
                "names `crate::skin_beatoraja::lua::runtime::ScreenRuntime`",
            ),
            (
                "pub(crate) use skin_beatoraja;\nmod render_side;\n",
                Some("binds `skin_beatoraja`"),
                "use crate::skin_beatoraja::lua::*;\n",
                "glob-imports `crate::skin_beatoraja::lua::*`",
            ),
            (
                "use ::skin_beatoraja::{self};\nmod render_side;\n",
                Some("binds `skin_beatoraja`"),
                "pub(crate) use super::skin_beatoraja::lua;\n",
                "uses `super::skin_beatoraja::lua`",
            ),
            (
                "mod render_side;\n",
                None,
                "use r#skin_beatoraja::r#lua::runtime::ScreenRuntime;\n",
                "uses `skin_beatoraja::lua::runtime::ScreenRuntime`",
            ),
            (
                "mod render_side;\n",
                None,
                "fn f() { let _ = r#mlua::Lua::new(); }\n",
                "names `mlua::Lua::new`",
            ),
            (
                "mod render_side;\n",
                None,
                "use r#skin_beatoraja as sb;\n",
                "aliases `skin_beatoraja` as `sb`",
            ),
            (
                "mod render_side;\n",
                None,
                "fn f() { m!(crate::r#skin_beatoraja::r#lua::runtime::ScreenRuntime); }\n",
                "names `skin_beatoraja::…` in a macro's tokens",
            ),
            (
                "mod render_side;\n",
                None,
                "pub(crate) use self::super::skin_beatoraja::Cancel;\n",
                "re-exports `self::super::skin_beatoraja::Cancel`",
            ),
        ] {
            let found = scan_files(&[("main.rs", root), ("render_side.rs", sibling)]);
            let in_file = |file: &str, what: &str| {
                found.iter().any(|d| {
                    d.starts_with(&format!("R6: crates/player/src/{file}:")) && d.contains(what)
                })
            };
            assert!(
                in_file("render_side.rs", sibling_what),
                "{sibling}: {found:#?}"
            );
            match root_what {
                Some(what) => assert!(in_file("main.rs", what), "{root}: {found:#?}"),
                None => assert!(
                    !found.iter().any(|d| d.contains("main.rs")),
                    "{root}: {found:#?}"
                ),
            }
            let expected = 1 + usize::from(root_what.is_some());
            assert_eq!(found.len(), expected, "{root} / {sibling}: {found:#?}");
        }
    }

    #[test]
    fn r6_lets_screens_and_the_crate_roots_items_be() {
        assert_eq!(scan(""), Vec::<String>::new());
        assert_eq!(
            scan(
                "use skin_beatoraja::{Cancel, LuaOutcome};\n\
                 fn f() { let _ = skin_beatoraja::vm_hooks::live(); println!(\"{}\", skin_beatoraja::limits::MAX_NOTES); }\n"
            ),
            Vec::<String>::new()
        );
    }

    #[test]
    fn r6_rejects_each_way_of_naming_the_backend_outside_screens() {
        for (file, what) in [
            ("use mlua::Lua;\n", "uses `mlua::Lua`"),
            ("use mlua;\n", "uses `mlua`"),
            (
                "use skin_beatoraja::lua::runtime::ScreenRuntime;\n",
                "uses `skin_beatoraja::lua::runtime::ScreenRuntime`",
            ),
            (
                "fn f(_: skin_beatoraja::lua::runtime::ScreenRuntime) {}\n",
                "names `skin_beatoraja::lua::runtime::ScreenRuntime`",
            ),
            (
                "fn f() { let _ = ::mlua::Lua::new(); }\n",
                "names `mlua::Lua::new`",
            ),
            (
                "use skin_beatoraja::lua as backend;\nfn f(_: backend::runtime::ScreenRuntime) {}\n",
                "aliases `skin_beatoraja::lua` as `backend`",
            ),
            (
                "use skin_beatoraja as sb;\n",
                "aliases `skin_beatoraja` as `sb`",
            ),
            (
                "use skin_beatoraja::{self as sb, Cancel};\n",
                "aliases `skin_beatoraja` as `sb`",
            ),
            (
                "extern crate skin_beatoraja as sb;\n",
                "aliases `skin_beatoraja` as `sb`",
            ),
            (
                "use skin_beatoraja::{lua::runtime::ScreenRuntime, Cancel};\n",
                "uses `skin_beatoraja::lua::runtime::ScreenRuntime`",
            ),
            (
                "use skin_beatoraja::*;\n",
                "glob-imports `skin_beatoraja::*`",
            ),
            (
                "use skin_beatoraja::lua::*;\n",
                "glob-imports `skin_beatoraja::lua::*`",
            ),
            (
                "pub use skin_beatoraja::Cancel;\n",
                "re-exports `skin_beatoraja::Cancel`",
            ),
            (
                "pub(crate) use skin_beatoraja::lua;\n",
                "uses `skin_beatoraja::lua`",
            ),
            (
                "fn f() { m!(skin_beatoraja::lua::runtime::ScreenRuntime); }\n",
                "names `skin_beatoraja::…` in a macro's tokens",
            ),
            (
                "mod inner { fn f() { use skin_beatoraja::lua::runtime; } }\n",
                "uses `skin_beatoraja::lua::runtime`",
            ),
        ] {
            let found = scan(file);
            assert_eq!(found.len(), 1, "{file}: {found:#?}");
            assert!(found[0].contains(what), "{file}: {found:#?}");
            assert!(
                found[0].starts_with("R6: crates/player/src/main.rs:"),
                "{found:#?}"
            );
        }
    }
}
