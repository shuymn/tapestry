//! One file decoded on its own for a pick of system sounds
//! ([`audio::decode_one`]), under the budget the pick's files share.

mod common;

use audio::{BankLimits, Cap, Decoded, FileRefusal, PickBudget, decode_one};
use common::{put, ramp, tmp, wav};

fn sound(got: Result<Decoded, FileRefusal>) -> audio::Sound {
    match got {
        Ok(Decoded::Sound { sound, .. }) => sound,
        other => panic!("not a sound: {other:?}"),
    }
}

/// ST6: the calls of one pick share its budget. Two files of 600 bytes of
/// PCM each against 1 KiB: the first decodes and is charged, the second is
/// over the cap and charges nothing. Garbage is undecodable. A 22 050 Hz
/// sound is converted to 48 kHz with its length rounded up. A second file
/// with the first's bytes is the same contents: nothing decoded or charged.
#[test]
fn decode_one_shares_its_budget() {
    let dir = tmp();
    let limits = BankLimits::default();
    let never = || false;
    let first = put(dir.path(), "first.wav", &wav(48_000, 1, &ramp(300, 1, 3)));
    let second = put(dir.path(), "second.wav", &wav(48_000, 1, &ramp(300, 2, 5)));
    let budget = PickBudget::new(1_024);
    let one = sound(decode_one(&first, 48_000, &budget, &limits, &never));
    assert_eq!((one.frames(), one.bytes()), (300, 600));
    assert_eq!(budget.used(), 600);
    assert_eq!(
        decode_one(&second, 48_000, &budget, &limits, &never),
        Err(FileRefusal::OverCap(Cap::BankMemory))
    );
    assert_eq!(budget.used(), 600, "the refused file's charge is returned");

    let copy = put(dir.path(), "copy.wav", &wav(48_000, 1, &ramp(300, 1, 3)));
    match decode_one(&copy, 48_000, &budget, &limits, &never) {
        Ok(Decoded::Same { .. }) => {}
        other => panic!("the copy was decoded again: {other:?}"),
    }
    assert_eq!(budget.used(), 600, "the copy is charged once");

    let garbage = put(dir.path(), "garbage.ogg", b"OggS not really a stream");
    let big = PickBudget::new(128 * 1024 * 1024);
    assert!(
        matches!(
            decode_one(&garbage, 48_000, &big, &limits, &never),
            Err(FileRefusal::Undecodable(_))
        ),
        "garbage decoded"
    );
    assert_eq!(big.used(), 0);

    let n: u64 = 22_050 + 7;
    let low = put(
        dir.path(),
        "low.wav",
        &wav(22_050, 1, &ramp(usize::try_from(n).unwrap(), 0, 11)),
    );
    let converted = sound(decode_one(&low, 48_000, &big, &limits, &never));
    assert_eq!(converted.frames(), (n * 48_000).div_ceil(22_050));
    assert_eq!(big.used(), converted.bytes());

    let missing = dir.path().join("missing.wav");
    assert_eq!(
        decode_one(&missing, 48_000, &big, &limits, &never),
        Err(FileRefusal::Io(std::io::ErrorKind::NotFound))
    );
}

/// A kept sound's charge fits in what remains or is refused whole.
#[test]
fn a_budget_charge_past_what_remains_is_refused() {
    let budget = PickBudget::new(1_000);
    assert_eq!(budget.charge(600), Ok(()));
    assert_eq!(
        budget.charge(401),
        Err(FileRefusal::OverCap(Cap::BankMemory))
    );
    assert_eq!(budget.charge(400), Ok(()));
    assert_eq!((budget.used(), budget.limit()), (1_000, 1_000));
}

/// A folder in a file's place is not read: Windows refuses to open it, and
/// elsewhere its opened handle is refused as not a regular file.
#[test]
fn a_folder_is_not_a_sound() {
    let dir = tmp();
    let folder = dir.path().join("clear.wav");
    std::fs::create_dir(&folder).unwrap();
    let budget = PickBudget::new(1_024);
    let got = decode_one(&folder, 48_000, &budget, &BankLimits::default(), &|| false);
    if cfg!(windows) {
        assert!(matches!(got, Err(FileRefusal::Io(_))), "{got:?}");
    } else {
        assert_eq!(
            got,
            Err(FileRefusal::Undecodable("not a regular file".into()))
        );
    }
}

/// A device opened in a file's place is refused on its handle before it is
/// read: unchecked, the null device reads as nothing and the endless one
/// up to the file cap.
#[test]
fn a_device_is_not_a_sound() {
    let device = if cfg!(windows) { "NUL" } else { "/dev/zero" };
    let budget = PickBudget::new(1_024);
    let got = decode_one(
        std::path::Path::new(device),
        48_000,
        &budget,
        &BankLimits::default(),
        &|| false,
    );
    assert_eq!(
        got,
        Err(FileRefusal::Undecodable("not a regular file".into()))
    );
}

/// A file with the same bytes as a refused one is decoded and refused on
/// its own, never taken as the same contents.
#[test]
fn a_copy_of_a_refused_file_is_refused_too() {
    let dir = tmp();
    let limits = BankLimits::default();
    let budget = PickBudget::new(128 * 1024 * 1024);
    for name in ["clear.wav", "fail.wav"] {
        let path = put(dir.path(), name, b"RIFF not really a wave");
        let got = decode_one(&path, 48_000, &budget, &limits, &|| false);
        assert!(
            matches!(got, Err(FileRefusal::Undecodable(_))),
            "{name}: {got:?}"
        );
    }
    let big = put(dir.path(), "big.wav", &wav(48_000, 1, &ramp(300, 1, 3)));
    let small = PickBudget::new(100);
    for name in ["big.wav", "big-copy.wav"] {
        let path = dir.path().join(name);
        if name != "big.wav" {
            std::fs::copy(&big, &path).unwrap();
        }
        assert_eq!(
            decode_one(&path, 48_000, &small, &limits, &|| false),
            Err(FileRefusal::OverCap(Cap::BankMemory)),
            "{name}"
        );
    }
}
