//! The portable errors are leaves: each message is the whole diagnostic,
//! and none has a source.

use std::error::Error;

use audio::{BankError, Cap, EngineError, Found, InvalidVolume, MAX_RATE, MIN_RATE, OutputError};
use bms_model::SoundId;

#[track_caller]
fn assert_leaf(e: &dyn Error, message: &str) {
    assert_eq!(e.to_string(), message);
    assert!(e.source().is_none(), "{e:?}");
}

#[test]
fn bank_errors_name_the_cap_and_how_it_was_found() {
    assert_leaf(
        &BankError::RateOutOfRange { rate: 7 },
        &format!("the device rate 7 Hz is outside {MIN_RATE}-{MAX_RATE} Hz"),
    );
    let over = |cap, found| BankError::OverCap {
        cap,
        found,
        sound: SoundId(3),
        amount: 9,
        limit: 8,
    };
    assert_leaf(
        &over(Cap::SoundLength, Found::Declared),
        "sound 03 exceeds the sound length in frames cap before decoding: 9 > 8",
    );
    assert_leaf(
        &over(Cap::BankMemory, Found::WhileDecoding),
        "sound 03 exceeds the decoded sound memory in bytes cap while decoding: 9 > 8",
    );
    assert_leaf(
        &over(Cap::FileSize, Found::Declared),
        "sound 03 exceeds the sound file size in bytes cap before decoding: 9 > 8",
    );
}

#[test]
fn engine_output_and_volume_errors_are_leaves() {
    assert_leaf(
        &EngineError::ZeroRateOrPeriod,
        "the output's rate or period is zero",
    );
    assert_leaf(
        &EngineError::RateMismatch {
            bank: 44_100,
            output: 48_000,
        },
        "the sound bank is at 44100 Hz but the output runs at 48000 Hz",
    );
    assert_leaf(&OutputError::DeviceLost, "the audio device went away");
    assert_leaf(
        &OutputError::Stalled,
        "the audio device stopped signaling buffer events",
    );
    assert_leaf(
        &OutputError::Failed(0x8000_4005_u32 as i32),
        "the audio device failed (0x80004005)",
    );
    assert_leaf(
        &InvalidVolume(2.5),
        &format!("volume 2.5 is outside 0 to {}", audio::MAX_VOLUME),
    );
}
