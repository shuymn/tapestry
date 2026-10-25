//! Test fixtures: small path-only workspaces mirroring the real one, so tests
//! run the real `cargo metadata` / `cargo tree` (offline) on them.

use std::fs;
use std::path::Path;

/// `(package, Cargo.toml body after [package], source file)`.
pub type Crate = (&'static str, String, &'static str);

pub fn lib(name: &'static str, body: &str) -> Crate {
    (name, body.to_owned(), "src/lib.rs")
}

/// The real workspace's shape: the portable library crates, `player`, and
/// `xtask`.
pub fn baseline() -> Vec<Crate> {
    vec![
        lib("bms-model", ""),
        lib(
            "bms-parser",
            "[dependencies]\nbms-model = { path = \"../bms-model\" }\n",
        ),
        lib(
            "rules",
            "[dependencies]\nbms-model = { path = \"../bms-model\" }\n",
        ),
        lib(
            "eval",
            "[dependencies]\nbms-model = { path = \"../bms-model\" }\n\
             rules = { path = \"../rules\" }\n",
        ),
        (
            "player",
            "[package.metadata.tapestry]\nwindows-only = true\n\
             [features]\ndefault = []\ndev-tools = []\n\
             [dependencies]\neval = { path = \"../eval\" }\n"
                .to_owned(),
            "src/main.rs",
        ),
        lib("xtask", ""),
    ]
}

/// Replaces the Cargo.toml body of package `name`.
pub fn set(crates: &mut [Crate], name: &str, body: &str) {
    crates.iter_mut().find(|c| c.0 == name).unwrap().1 = body.to_owned();
}

pub fn write_workspace(dir: &Path, crates: &[Crate]) {
    fs::write(
        dir.join("Cargo.toml"),
        "[workspace]\nresolver = \"3\"\nmembers = [\"crates/*\"]\n",
    )
    .unwrap();
    for (name, body, src) in crates {
        let root = dir.join("crates").join(name);
        fs::create_dir_all(root.join("src")).unwrap();
        fs::write(
            root.join("Cargo.toml"),
            format!(
                "[package]\nname = \"{name}\"\nversion = \"0.0.0\"\nedition = \"2024\"\n\
                 publish = false\n{body}"
            ),
        )
        .unwrap();
        fs::write(root.join(src), "").unwrap();
    }
}
