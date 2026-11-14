//! A sound-set candidate that is a FIFO (design.md "Audio", system sounds;
//! "Threat Model", T18). Its own test binary: making the FIFO runs
//! `mkfifo`, and a process forked from a test binary shares that binary's
//! open files until it executes, the sound-sets lock included, so a test
//! in `sound_sets` that reopens the lock would find it held.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::MetadataExt as _;
use std::path::Path;
use std::process::Command;
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use audio::{BankLimits, GenId, SystemKind};
use game::sound_sets::{Refused, Resolved, SetPick, Source, Why, resolve};

fn links(path: &Path) -> Option<u64> {
    fs::metadata(path).ok().map(|m| m.nlink())
}

/// ST5: a FIFO candidate is refused without being opened, named once, and
/// the next location is tried.
// Covers: T18
#[test]
fn a_fifo_is_refused_unopened() {
    let base = tempfile::Builder::new()
        .prefix("sound-sets-fifo")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let sound = base.path().join("sound/set");
    let defaults = base.path().join("defaultsound");
    fs::create_dir_all(&sound).unwrap();
    fs::create_dir_all(&defaults).unwrap();
    let fifo = sound.join("o-open.wav");
    let status = Command::new("mkfifo")
        .arg(&fifo)
        .status()
        .expect("this test needs `mkfifo`");
    assert!(status.success());
    fs::write(defaults.join("o-open.wav"), b"RIFF").unwrap();
    let pick = SetPick {
        sound_set: Some(sound.clone()),
        bgm_set: None,
        defaults: defaults.clone(),
        id: GenId(7),
        rate: 48_000,
        limits: BankLimits::default(),
        skipped: Vec::new(),
    };
    // Resolution opens nothing, so the FIFO cannot block it.
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        let _ = tx.send(resolve(&pick, links, &mut |_| false));
    });
    let r = rx
        .recv_timeout(Duration::from_secs(10))
        .expect("resolution blocked");
    assert_eq!(
        r.refused,
        [Refused {
            path: fifo.canonicalize().unwrap(),
            why: Why::NotAFile,
        }]
    );
    match &r.kinds[SystemKind::OptionOpen.index()] {
        Resolved::Found { source, .. } => assert_eq!(*source, Source::Defaults),
        other => panic!("{other:?}"),
    }
}
