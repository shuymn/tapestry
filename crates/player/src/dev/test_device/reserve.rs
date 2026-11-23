//! `tapestry test-device reserve <row> --out <dir> --build <id>`
//! (docs/test-device.md "Verdicts", reserved runs): reserves every run a
//! row compares before any of them starts. It makes the set's folder new
//! and writes its `reservation.jsonl`: one `reserved` record naming the
//! set's id, the row, the device's build, and for each of the row's roles
//! the row it runs, a fresh run id and its folder. Each role then runs as
//! `run <row> --set <dir> --role <role>`, and `check` binds each role to its
//! run only from this record.

use std::path::Path;

use anyhow::bail;
use serde_json::json;
use test_device::contract::{Binding, RESERVATION};

use super::owner;

/// Reserves the set of runs row `row` compares in the new folder `out`, for
/// the device's build `build`.
///
/// # Errors
///
/// A row that compares no set, a build that is not a bare commit, a folder
/// that exists, or a file that cannot be written.
pub fn reserve(row: &str, out: &Path, build: &str) -> anyhow::Result<()> {
    let Some(e) = super::check::expectation_of(row) else {
        bail!("no row `{row}`: this build compiles no expectation for it");
    };
    if e.binding != Binding::Set {
        bail!("row `{row}` is a single run: it reserves no set");
    }
    if !super::run::bare_commit(build) {
        bail!("`{build}` is not a bare commit id (12 hex digits)");
    }
    owner::reserve(out)?;
    let roles = e
        .roles
        .iter()
        .map(|r| {
            Ok(json!({
                "role": r.name, "row": r.row, "run": owner::random_hex(16)?, "dir": r.name,
            }))
        })
        .collect::<anyhow::Result<Vec<_>>>()?;
    let record = json!({
        "t": "reserved", "set": owner::random_hex(16)?, "row": row, "build": build, "roles": roles,
    });
    owner::write_new(&out.join(RESERVATION), format!("{record}\n").as_bytes())
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::*;

    const BUILD: &str = "0123456789ab";

    /// A set reserves each of its row's roles with the row it runs, a run id
    /// of its own and a folder named after it, for the given build; the
    /// record reads as `run --set` and `check` read it.
    #[test]
    fn a_set_reserves_each_role_of_its_row() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("set");
        reserve("p1-compare", &out, BUILD).unwrap();
        let s = owner::reservation(&out).unwrap().unwrap();
        assert_eq!((s.row.as_str(), s.build.as_str()), ("p1-compare", BUILD));
        let roles: Vec<(&str, &str, &str)> = s
            .roles
            .iter()
            .map(|r| (r.role.as_str(), r.row.as_str(), r.dir.as_str()))
            .collect();
        assert_eq!(
            roles,
            [
                ("A1", "p1", "A1"),
                ("A2", "p1", "A2"),
                ("R1", "p1-ref", "R1"),
                ("R2", "p1-ref", "R2"),
                ("R75", "p1-ref-75", "R75"),
            ]
        );
        let ids: BTreeSet<&str> = s
            .roles
            .iter()
            .map(|r| r.run.as_str())
            .chain([s.set.as_str()])
            .collect();
        assert_eq!(ids.len(), 6, "{ids:?}");
        assert!(ids.iter().all(|i| i.len() == 32));
        // A second reservation in the same folder is refused.
        assert!(reserve("p1-compare", &out, BUILD).is_err());
    }

    /// A row of single runs, an unknown row and a build that is not a bare
    /// commit are refused before any folder is made.
    #[test]
    fn a_row_without_a_set_is_refused() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("set");
        for (row, build) in [
            ("p1", BUILD),
            ("nope", BUILD),
            ("p1-compare", "0123456789ab-dirty"),
        ] {
            assert!(reserve(row, &out, build).is_err(), "{row} {build}");
            assert!(!out.exists(), "{row} {build}");
        }
    }

    /// A role of a set runs once, for its reserved row and build, and takes
    /// the reserved id; the folder is made by the run.
    #[test]
    fn a_role_runs_once() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("set");
        reserve("p1-compare", &out, BUILD).unwrap();
        owner::reserved(&out, "A1", "p1", BUILD).unwrap();
        assert!(owner::reserved(&out, "A1", "p1", BUILD).is_err());
    }

    #[test]
    fn a_role_refuses_another_row_or_build() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("set");
        reserve("p1-compare", &out, BUILD).unwrap();
        let e = owner::reserved(&out, "R1", "p1", BUILD).unwrap_err();
        assert!(
            format!("{e:#}").contains("reserved for row `p1-ref`"),
            "{e:#}"
        );
        let e = owner::reserved(&out, "R1", "p1-ref", "ba9876543210").unwrap_err();
        assert!(format!("{e:#}").contains("reserved for build"), "{e:#}");
        assert!(owner::reserved(&out, "Z9", "p1", BUILD).is_err());
        // Nothing of the refused roles was made.
        assert!(!out.join("R1").exists() && !out.join("Z9").exists());
    }

    #[test]
    fn the_run_takes_the_reserved_id() {
        let tmp = tempfile::tempdir().unwrap();
        let out = tmp.path().join("set");
        reserve("p1-compare", &out, BUILD).unwrap();
        let s = owner::reservation(&out).unwrap().unwrap();
        let (set, run, dir) = owner::reserved(&out, "R75", "p1-ref-75", BUILD).unwrap();
        let r = s.roles.iter().find(|r| r.role == "R75").unwrap();
        assert_eq!((set, run), (s.set.clone(), r.run.clone()));
        assert_eq!(dir, out.join("R75"));
        assert!(dir.is_dir());
    }
}
