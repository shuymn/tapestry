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

use std::collections::{BTreeSet, HashMap, HashSet, VecDeque};
use std::path::Path;
use std::process::Command;

use anyhow::{Context as _, bail};
use cargo_metadata::{
    CargoOpt, DependencyKind, Metadata, MetadataCommand, Package, PackageId, Resolve,
};

/// The evaluator package checked by R1.
const EVAL: &str = "eval";
/// Workspace packages `eval` may depend on.
const EVAL_ALLOWED_WORKSPACE: &[&str] = &["bms-model", "rules"];
/// External crates `eval` may depend on. Additions are reviewed code changes.
const EVAL_ALLOWED_EXTERNAL: &[&str] = &[];
/// Packages allowed to be Windows-only (AGENTS.md).
pub const WINDOWS_ONLY_ALLOWED: &[&str] = &["input", "audio", "render", "player"];
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
    ("xtask", PERMISSIVE),
    ("input", GPL),
    ("audio", GPL),
    ("game", GPL),
    ("library", GPL),
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
    Ok(diagnostics)
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
}
