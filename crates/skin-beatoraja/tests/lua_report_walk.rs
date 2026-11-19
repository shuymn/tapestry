//! The conversion report's walk and comparison as a developer runs them
//! (docs/stage3.md §3, the conversion report): the process's exit codes,
//! and no skin name in what it prints.

use std::path::Path;
use std::process::{Command, Output};

const EXE: &str = env!("CARGO_BIN_EXE_lua_report");

/// A play skin whose option item `QZXon` loads the module `QZXpart.lua`.
const ENTRY: &str = "local skin = { type = 0, name = \"QZXname\", w = 1920, h = 1080,\n\
  property = { { name = \"QZXopt\", item = { { name = \"QZXoff\", op = 905 }, { name = \"QZXon\", op = 906 } } } } }\n\
if not skin_config then return skin end\n\
if skin_config.option[\"QZXopt\"] == 906 then require(\"QZXpart\") end\n\
skin.destination = { { id = -110, dst = { { w = 64, h = 64 } } } }\n\
return skin\n";

fn run(args: &[&str]) -> (i32, String, String) {
    let Output {
        status,
        stdout,
        stderr,
    } = Command::new(EXE).args(args).output().unwrap();
    (
        status.code().unwrap(),
        String::from_utf8(stdout).unwrap(),
        String::from_utf8(stderr).unwrap(),
    )
}

fn path(p: &Path) -> &str {
    p.to_str().unwrap()
}

#[test]
fn the_walk_and_the_comparison_exit_by_what_they_found() {
    let base = tempfile::tempdir().unwrap();
    let root = base.path().join("QZXskin");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("QZXentry.luaskin"), ENTRY).unwrap();
    std::fs::write(root.join("QZXpart.lua"), "return 1").unwrap();
    let runs = base.path().join("QZXruns");
    std::fs::create_dir_all(&runs).unwrap();
    let records = |name: &str| runs.join(format!("QZX{name}.toml"));
    let walk = |name: &str| {
        run(&[
            "--walk",
            path(&records(name)),
            path(&root),
            "QZXentry.luaskin",
        ])
    };
    let mut printed = Vec::new();
    let (code, out, err) = walk("a");
    assert_eq!(code, 0, "{err}");
    assert!(
        out.starts_with("skin 0: Play, configurations 3, imported 3,"),
        "{out}"
    );
    printed.push(out + &err);
    let (code, out, err) = walk("b");
    assert_eq!(code, 0, "{err}");
    printed.push(out + &err);
    // Two walks of one build compare unchanged.
    let (code, out, err) = run(&["--compare", path(&records("a")), path(&records("b"))]);
    assert_eq!(code, 0, "{out}{err}");
    assert!(out.contains(" 0 new failures, "), "{out}");
    printed.push(out + &err);
    // A dependency of configuration 2 damaged: a new failure.
    std::fs::write(root.join("QZXpart.lua"), "not lua (").unwrap();
    let (code, _, err) = walk("c");
    assert_eq!(code, 0, "{err}");
    let (code, out, err) = run(&["--compare", path(&records("a")), path(&records("c"))]);
    assert_eq!(code, 1, "{out}{err}");
    assert!(
        out.contains(
            "skin 0 configuration 2 (option 0 1): outcome imported -> lua Syntax: new failure\n"
        ),
        "{out}"
    );
    printed.push(out + &err);
    // The header changed: the skin is re-baselined.
    std::fs::write(
        root.join("QZXentry.luaskin"),
        ENTRY
            .replace("op = 906", "op = 907")
            .replace("== 906", "== 907"),
    )
    .unwrap();
    std::fs::write(root.join("QZXpart.lua"), "return 1").unwrap();
    let (code, _, err) = walk("d");
    assert_eq!(code, 0, "{err}");
    let (code, out, err) = run(&["--compare", path(&records("a")), path(&records("d"))]);
    assert_eq!(code, 2, "{out}{err}");
    assert!(
        out.starts_with("skin 0: catalogue changed, re-baseline\n"),
        "{out}"
    );
    printed.push(out + &err);
    // A truncated kept run cannot be judged.
    let text = std::fs::read_to_string(records("a")).unwrap();
    let cut = text.rfind("[[record]]").unwrap();
    std::fs::write(records("cut"), &text[..cut]).unwrap();
    let (code, out, err) = run(&["--compare", path(&records("cut")), path(&records("b"))]);
    assert_eq!(code, 2, "{out}{err}");
    assert_eq!(err.trim_end(), "kept: declares 3 records and holds 2");
    printed.push(out + &err);
    // Fact flags and `--local` are not the walk's.
    for flags in [
        &[
            "--walk",
            "--local",
            path(&records("e")),
            path(&root),
            "QZXentry.luaskin",
        ][..],
        &[
            "--walk",
            path(&records("e")),
            "--level",
            "3",
            path(&root),
            "QZXentry.luaskin",
        ],
        &["--walk", path(&records("e")), path(&root)],
    ] {
        let (code, _, _) = run(flags);
        assert_eq!(code, 2, "{flags:?}");
    }
    assert!(!records("e").exists());
    for p in printed {
        assert!(!p.contains("QZX"), "{p}");
    }
}
