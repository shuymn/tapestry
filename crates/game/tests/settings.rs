//! Settings files, local records and the atomic write they share.

use std::fs;

use bms_model::{Lane, TimeUs};
use eval::{Binding, ControlId, SourceProfile};
use game::{
    Adjust, DataRoot, DeviceSettings, KeyConfig, KeyError, LaneKey, LocalRecords, PlayerSettings,
    RecordSink, Role, Settings, SettingsError, Tuning, load_settings, save_settings, write_atomic,
};
use input::DeviceKind;

fn temp_root() -> (tempfile::TempDir, DataRoot) {
    let dir = tempfile::Builder::new()
        .prefix("settings")
        .tempdir_in(env!("CARGO_TARGET_TMPDIR"))
        .unwrap();
    let root = DataRoot::new(dir.path().join("data"));
    (dir, root)
}

/// Settings with every field away from its default, each value distinct.
fn changed() -> Settings {
    Settings {
        device: DeviceSettings {
            keyboard: KeyConfig {
                lanes: vec![
                    LaneKey {
                        control: 0x1e,
                        lane: 1,
                    },
                    LaneKey {
                        control: 0x39,
                        lane: 0,
                    },
                ],
                retry: Some(0x13),
                quit: None,
            },
            controller: KeyConfig {
                lanes: vec![
                    LaneKey {
                        control: 7,
                        lane: 4,
                    },
                    LaneKey {
                        control: 9,
                        lane: 6,
                    },
                ],
                retry: None,
                quit: None,
            },
            input_offset_us: 4_500,
            audio_offset_us: -12_000,
            display_offset_us: 16_700,
            audio_device: Some("device-a".to_owned()),
            master_volume: 0.75,
        },
        player: PlayerSettings {
            visible_time_ms: 2_350,
            preset: "wide.toml".to_owned(),
            displayed_gauge: 1,
        },
    }
}

#[test]
fn missing_files_give_the_defaults() {
    let (_dir, root) = temp_root();
    let s = Settings::load(&root).unwrap();
    assert_eq!(s.device, DeviceSettings::default());
    assert_eq!(s.player, PlayerSettings::default());
    assert_eq!(s.player.preset, "custom.toml");
}

#[test]
fn device_and_player_settings_round_trip() {
    let (_dir, root) = temp_root();
    let s = changed();
    s.save(&root).unwrap();
    assert_eq!(Settings::load(&root).unwrap(), s);
    // Only the two files: no temporary file is left behind.
    let mut names: Vec<String> = fs::read_dir(root.path())
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    assert_eq!(names, ["device.toml", "player.toml"]);
    // A hand-written file in the same units reads the same.
    fs::write(
        root.player_settings(),
        "visible_time_ms = 2350\npreset = \"wide.toml\"\ndisplayed_gauge = 1\n",
    )
    .unwrap();
    assert_eq!(Settings::load(&root).unwrap().player, s.player);

    // The settings keys' values, stepped and written as the game does at
    // the end of a play, come back the same: hi-speed up twice (2350 −
    // 2 × 25 = 2300 ms), volume down once (75 − 5 = 70 %), display offset
    // up once (16.7 + 1 = 17.7 ms) and input offset down three times
    // (4.5 − 3 = 1.5 ms).
    let mut t = Tuning::of(&s);
    for a in [
        Adjust::HiSpeedUp,
        Adjust::HiSpeedUp,
        Adjust::VolumeDown,
        Adjust::DisplayOffsetUp,
        Adjust::InputOffsetDown,
        Adjust::InputOffsetDown,
        Adjust::InputOffsetDown,
    ] {
        t = t.adjust(a);
    }
    let mut stepped = s.clone();
    t.apply_to(&mut stepped);
    stepped.save(&root).unwrap();
    let back = Settings::load(&root).unwrap();
    assert_eq!(back, stepped);
    assert_eq!(back.player.visible_time_ms, 2_300);
    assert_eq!(back.device.master_volume, 0.7);
    assert_eq!(back.device.display_offset_us, 17_700);
    assert_eq!(back.device.input_offset_us, 1_500);
    // Everything else is as it was.
    assert_eq!(back.device.keyboard, s.device.keyboard);
    assert_eq!(back.device.controller, s.device.controller);
    assert_eq!(back.device.audio_offset_us, -12_000);
    assert_eq!(back.player.preset, "wide.toml");
    assert_eq!(Tuning::of(&back), t);
}

#[test]
fn unreadable_settings_are_errors_not_defaults() {
    let (_dir, root) = temp_root();
    fs::create_dir_all(root.path()).unwrap();
    fs::write(root.player_settings(), "visible_time_ms = \"fast\"").unwrap();
    assert!(matches!(
        load_settings::<PlayerSettings>(&root.player_settings()),
        Err(SettingsError::Parse(_))
    ));
    fs::write(root.player_settings(), "volume = 3\n").unwrap();
    assert!(matches!(
        load_settings::<PlayerSettings>(&root.player_settings()),
        Err(SettingsError::Parse(e)) if e.contains("volume")
    ));
    // A directory where the file should be.
    fs::create_dir_all(root.device_settings()).unwrap();
    assert!(matches!(
        load_settings::<DeviceSettings>(&root.device_settings()),
        Err(SettingsError::Io { .. })
    ));
    assert!(matches!(
        save_settings(&root.device_settings(), &DeviceSettings::default()),
        Err(SettingsError::Io { .. })
    ));
}

#[test]
fn key_config_gives_profiles_and_recorded_controls() {
    let s = changed().device;
    assert_eq!(
        s.keyboard.recorded(),
        vec![ControlId(0x1e), ControlId(0x39), ControlId(0x13)]
    );
    assert_eq!(s.controller.recorded(), vec![ControlId(7), ControlId(9)]);
    let profile = s.profile(&[DeviceKind::Controller, DeviceKind::Keyboard]);
    let lane = |n| Lane::new(n).unwrap();
    assert_eq!(
        profile.sources,
        vec![
            SourceProfile {
                input_offset: TimeUs(4_500),
                bindings: vec![
                    Binding {
                        control: ControlId(7),
                        lane: lane(4)
                    },
                    Binding {
                        control: ControlId(9),
                        lane: lane(6)
                    },
                ],
            },
            SourceProfile {
                input_offset: TimeUs(4_500),
                bindings: vec![
                    Binding {
                        control: ControlId(0x1e),
                        lane: lane(1)
                    },
                    Binding {
                        control: ControlId(0x39),
                        lane: lane(0)
                    },
                ],
            },
        ]
    );
    // A lane the game does not have binds nothing.
    let mut keys = s.keyboard;
    keys.lanes.push(LaneKey {
        control: 0x50,
        lane: 8,
    });
    assert_eq!(keys.bindings().len(), 2);
}

#[test]
fn default_key_config_binds_every_lane_once() {
    let keys = DeviceSettings::default().keyboard;
    let mut lanes: Vec<u8> = keys.lanes.iter().map(|k| k.lane).collect();
    lanes.sort_unstable();
    assert_eq!(lanes, [0, 1, 2, 3, 4, 5, 6, 7]);
    let mut controls: Vec<u32> = keys.lanes.iter().map(|k| k.control).collect();
    controls.extend(keys.retry);
    controls.extend(keys.quit);
    controls.sort_unstable();
    controls.dedup();
    assert_eq!(controls.len(), 10);
}

#[test]
fn failed_rename_leaves_no_temporary_file() {
    let (dir, _root) = temp_root();
    // The target is a directory holding a file, so the rename fails.
    let target = dir.path().join("target");
    fs::create_dir_all(target.join("inside")).unwrap();
    assert!(write_atomic(&target, b"bytes").is_err());
    assert_eq!(names(dir.path()), ["target"]);
    // Without its directory, nothing is written.
    assert!(write_atomic(&dir.path().join("missing").join("file"), b"bytes").is_err());
}

fn names(dir: &std::path::Path) -> Vec<String> {
    let mut names: Vec<String> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    names
}

#[test]
fn two_writers_never_replace_each_others_records() {
    let (_dir, root) = temp_root();
    let rec = |writer: usize, i: usize| replay::PlayRecord {
        versions: replay::Versions {
            app: format!("writer {writer} play {i}"),
            ..sample().versions
        },
        ..sample()
    };
    let saved: Vec<Vec<std::path::PathBuf>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..2)
            .map(|w| {
                let dir = root.records();
                s.spawn(move || {
                    let mut records = LocalRecords::new(dir);
                    (0..25)
                        .map(|i| records.save(&rec(w, i)).unwrap().path)
                        .collect()
                })
            })
            .collect();
        handles.into_iter().map(|h| h.join().unwrap()).collect()
    });
    let mut all: Vec<_> = saved.concat();
    all.sort();
    all.dedup();
    assert_eq!(all.len(), 50);
    // Each file holds the record its writer saved there.
    for (w, paths) in saved.iter().enumerate() {
        for (i, path) in paths.iter().enumerate() {
            assert_eq!(fs::read(path).unwrap(), replay::encode(&rec(w, i)));
        }
    }
    let dir = all[0].parent().unwrap();
    let mut numbers: Vec<u64> = names(dir)
        .iter()
        .map(|n| n.trim_end_matches(".tprl").parse().unwrap())
        .collect();
    numbers.sort_unstable();
    assert_eq!(numbers, (1..=50).collect::<Vec<u64>>());
}

#[test]
fn a_record_numbered_last_leaves_no_number_to_save_under() {
    let (_dir, root) = temp_root();
    let mut records = LocalRecords::new(root.records());
    let first = records.save(&sample()).unwrap().path;
    let dir = first.parent().unwrap().to_owned();
    fs::rename(&first, dir.join(format!("{}.tprl", u64::MAX))).unwrap();
    assert!(matches!(
        records.save(&sample()),
        Err(game::SaveError::Io {
            kind: std::io::ErrorKind::Other,
            ..
        })
    ));
    assert_eq!(names(&dir).len(), 1);
}

#[test]
fn saving_gives_up_after_the_bound_of_taken_numbers() {
    let (_dir, root) = temp_root();
    let mut records = LocalRecords::new(root.records());
    let first = records.save(&sample()).unwrap().path;
    let dir = first.parent().unwrap().to_owned();
    fs::remove_file(&first).unwrap();
    // Names taken by something other than a record file: the scan does not
    // count them, and publishing moves past each.
    let taken = |n: u64| fs::create_dir(dir.join(format!("{n}.tprl"))).unwrap();
    for n in 1..game::SAVE_ATTEMPTS {
        taken(n);
    }
    let last = records.save(&sample()).unwrap().path;
    assert_eq!(last, dir.join(format!("{}.tprl", game::SAVE_ATTEMPTS)));
    fs::remove_file(&last).unwrap();
    taken(game::SAVE_ATTEMPTS);
    assert!(matches!(
        records.save(&sample()),
        Err(game::SaveError::Io {
            kind: std::io::ErrorKind::AlreadyExists,
            ..
        })
    ));
    // No temporary file is left either way.
    assert!(names(&dir).iter().all(|n| n.ends_with(".tprl")));
}

#[test]
fn records_are_numbered_per_chart() {
    let (_dir, root) = temp_root();
    let mut records = LocalRecords::new(root.records());
    let rec = |sha: u8| replay::PlayRecord {
        chart: replay::ChartRecord {
            identity: bms_model::ChartIdentity {
                sha256: [sha; 32],
                ..Default::default()
            },
            ..Default::default()
        },
        ..sample()
    };
    let a1 = records.save(&rec(0xab)).unwrap();
    // Files that are not numbered records do not count.
    let dir = a1.path.parent().unwrap().to_owned();
    fs::write(dir.join("notes.txt"), b"x").unwrap();
    fs::write(dir.join("7.tmp"), b"x").unwrap();
    let a2 = records.save(&rec(0xab)).unwrap();
    let b1 = records.save(&rec(0xcd)).unwrap();
    let name = |p: &std::path::Path| p.file_name().unwrap().to_str().unwrap().to_owned();
    assert_eq!(name(&a1.path), "1.tprl");
    assert_eq!(name(&a2.path), "2.tprl");
    assert_eq!(name(&b1.path), "1.tprl");
    assert_eq!(dir.file_name().unwrap().to_str().unwrap(), "ab".repeat(32));
    // A number beyond a gap continues from the highest.
    fs::write(dir.join("9.tprl"), b"x").unwrap();
    assert_eq!(name(&records.save(&rec(0xab)).unwrap().path), "10.tprl");
}

fn sample() -> replay::PlayRecord {
    replay::PlayRecord {
        versions: replay::Versions {
            evaluator: 1,
            app: "test".to_owned(),
            provenance: replay::BuildProvenance::Development,
        },
        chart: replay::ChartRecord::default(),
        preset: replay::PresetRecord::of(&rules::custom()),
        session_policy: "default".to_owned(),
        devices: replay::DeviceRecord {
            sources: Vec::new(),
            audio_offset: TimeUs(0),
            display_offset: TimeUs(0),
        },
        play_type: replay::PlayType::Full,
        start: eval::InputStart::default(),
        events: Vec::new(),
        arrivals: Vec::new(),
        status: replay::Status {
            end: replay::EndStatus::PlayedToEnd,
            finalized: TimeUs(0),
            incomplete: replay::Incomplete::default(),
        },
    }
}

/// Writes both files and loads them as the game does.
fn load_written(
    device: &DeviceSettings,
    player: &PlayerSettings,
) -> Result<Settings, SettingsError> {
    let (_dir, root) = temp_root();
    save_settings(&root.device_settings(), device).unwrap();
    save_settings(&root.player_settings(), player).unwrap();
    Settings::load(&root)
}

#[track_caller]
fn assert_invalid(device: &DeviceSettings, player: &PlayerSettings, says: &str) {
    match load_written(device, player) {
        Err(SettingsError::Invalid(e)) => assert!(e.contains(says), "{e} lacks {says}"),
        other => panic!("{other:?} for {says}"),
    }
}

fn with(f: impl Fn(&mut DeviceSettings)) -> DeviceSettings {
    let mut d = DeviceSettings::default();
    f(&mut d);
    d
}

fn lane_key(lane: u8) -> u32 {
    DeviceSettings::default()
        .keyboard
        .lanes
        .iter()
        .find(|k| k.lane == lane)
        .unwrap()
        .control
}

fn set_lane(d: &mut DeviceSettings, lane: u8, control: u32) {
    d.keyboard
        .lanes
        .iter_mut()
        .find(|k| k.lane == lane)
        .unwrap()
        .control = control;
}

#[test]
fn key_configs_that_reuse_a_control_or_bind_a_reserved_key_are_refused_on_load() {
    let player = PlayerSettings::default();
    assert!(load_written(&DeviceSettings::default(), &player).is_ok());

    // One key on two lanes, on a lane and retry, on a lane and quit, and
    // on retry and quit.
    let cases = [
        (
            with(|d| set_lane(d, 5, lane_key(2))),
            KeyError::Overlap {
                control: lane_key(2),
                first: Role::Lane(2),
                second: Role::Lane(5),
            },
        ),
        (
            with(|d| d.keyboard.retry = Some(lane_key(3))),
            KeyError::Overlap {
                control: lane_key(3),
                first: Role::Lane(3),
                second: Role::Retry,
            },
        ),
        (
            with(|d| d.keyboard.quit = Some(lane_key(0))),
            KeyError::Overlap {
                control: lane_key(0),
                first: Role::Lane(0),
                second: Role::Quit,
            },
        ),
        (
            with(|d| d.keyboard.quit = d.keyboard.retry),
            KeyError::Overlap {
                control: 0x0e,
                first: Role::Retry,
                second: Role::Quit,
            },
        ),
    ];
    for (d, e) in &cases {
        assert_eq!(d.keyboard.check(DeviceKind::Keyboard), Err(*e));
        assert_invalid(d, &player, "device.toml: keyboard key config");
        assert_invalid(d, &player, "is bound to both");
    }

    // The settings keys F1 (first) and F8 (last) and the keep key F12, as
    // a lane, retry and quit; F9 is free.
    let reserved = [
        (with(|d| set_lane(d, 1, 0x3b)), 0x3b, "hi-speed"),
        (
            with(|d| d.keyboard.retry = Some(0x42)),
            0x42,
            "input offset",
        ),
        (
            with(|d| d.keyboard.quit = Some(0x58)),
            0x58,
            "keeping a binding",
        ),
    ];
    for (d, control, what) in &reserved {
        assert!(matches!(
            d.keyboard.check(DeviceKind::Keyboard),
            Err(KeyError::Reserved { control: c, .. }) if c == *control
        ));
        assert_invalid(d, &player, what);
    }
    assert!(load_written(&with(|d| set_lane(d, 1, 0x43)), &player).is_ok());

    // A lane the game does not have.
    let lane_8 = with(|d| d.keyboard.lanes[0].lane = 8);
    assert_eq!(
        lane_8.keyboard.check(DeviceKind::Keyboard),
        Err(KeyError::NoSuchLane(8))
    );
    assert_invalid(&lane_8, &player, "lane 8 does not exist");

    // Controllers: lanes may use any control, F1's and F12's numbers
    // included, but not one control twice, and never retry or quit.
    let pad = |lanes: &[(u32, u8)], retry, quit| {
        with(|d| {
            d.controller = KeyConfig {
                lanes: lanes
                    .iter()
                    .map(|&(control, lane)| LaneKey { control, lane })
                    .collect(),
                retry,
                quit,
            };
        })
    };
    assert!(load_written(&pad(&[(0x3b, 1), (0x58, 2)], None, None), &player).is_ok());
    assert_invalid(
        &pad(&[(4, 1), (4, 6)], None, None),
        &player,
        "device.toml: controller key config: control 0x4 is bound to both key 1 and key 6",
    );
    assert_invalid(
        &pad(&[(4, 1)], Some(5), None),
        &player,
        "retry takes keyboard keys only",
    );
    assert_invalid(
        &pad(&[(4, 1)], None, Some(6)),
        &player,
        "quit takes keyboard keys only",
    );
}

#[test]
fn settings_outside_their_ranges_are_refused_on_load() {
    let player = PlayerSettings::default();
    let device = DeviceSettings::default();
    let p = |ms: u32| PlayerSettings {
        visible_time_ms: ms,
        ..player.clone()
    };
    // The edges load; one step past each does not.
    for ok in [
        with(|d| d.input_offset_us = 1_000_000),
        with(|d| d.input_offset_us = -1_000_000),
        with(|d| d.display_offset_us = 1_000_000),
        with(|d| d.display_offset_us = -1_000_000),
        with(|d| d.master_volume = 0.0),
        with(|d| d.master_volume = 1.0),
    ] {
        assert!(load_written(&ok, &player).is_ok(), "{ok:?}");
    }
    let refused = [
        (
            with(|d| d.input_offset_us = 1_000_001),
            "input_offset_us 1000001",
        ),
        (
            with(|d| d.input_offset_us = -1_000_001),
            "input_offset_us -1000001",
        ),
        (
            with(|d| d.display_offset_us = 1_000_001),
            "display_offset_us 1000001",
        ),
        (
            with(|d| d.display_offset_us = -1_000_001),
            "display_offset_us -1000001",
        ),
        (with(|d| d.master_volume = 1.01), "master_volume 1.01"),
        (with(|d| d.master_volume = -0.01), "master_volume -0.01"),
        (with(|d| d.master_volume = f32::NAN), "master_volume NaN"),
    ];
    for (d, says) in &refused {
        assert_invalid(d, &player, says);
    }
    for ok in [100, 4_000] {
        assert!(load_written(&device, &p(ok)).is_ok(), "{ok}");
    }
    assert_invalid(
        &device,
        &p(99),
        "player.toml: visible_time_ms 99 is outside 100 to 4000",
    );
    assert_invalid(&device, &p(4_001), "visible_time_ms 4001");
}

#[test]
fn settings_keys_step_and_stop_at_their_bounds() {
    // F1 to F8, nothing either side; F12 is reserved too.
    let keys: Vec<u32> = Adjust::ALL.iter().map(|a| a.key()).collect();
    assert_eq!(keys, [0x3b, 0x3c, 0x3d, 0x3e, 0x3f, 0x40, 0x41, 0x42]);
    for a in Adjust::ALL {
        assert_eq!(Adjust::from_key(a.key()), Some(a));
        assert!(game::reserved_key(a.key()).is_some());
    }
    for free in [0x3a, 0x43, 0x01, 0x0e] {
        assert_eq!(Adjust::from_key(free), None);
        assert_eq!(game::reserved_key(free), None);
    }
    assert!(game::reserved_key(game::KEEP_KEY).is_some());

    let t = Tuning {
        visible_time_ms: 600,
        volume_percent: 40,
        display_offset_us: 2_000,
        input_offset_us: -3_000,
    };
    // Each key moves its own value only.
    let steps = [
        (
            Adjust::HiSpeedDown,
            Tuning {
                visible_time_ms: 625,
                ..t
            },
        ),
        (
            Adjust::HiSpeedUp,
            Tuning {
                visible_time_ms: 575,
                ..t
            },
        ),
        (
            Adjust::VolumeDown,
            Tuning {
                volume_percent: 35,
                ..t
            },
        ),
        (
            Adjust::VolumeUp,
            Tuning {
                volume_percent: 45,
                ..t
            },
        ),
        (
            Adjust::DisplayOffsetDown,
            Tuning {
                display_offset_us: 1_000,
                ..t
            },
        ),
        (
            Adjust::DisplayOffsetUp,
            Tuning {
                display_offset_us: 3_000,
                ..t
            },
        ),
        (
            Adjust::InputOffsetDown,
            Tuning {
                input_offset_us: -4_000,
                ..t
            },
        ),
        (
            Adjust::InputOffsetUp,
            Tuning {
                input_offset_us: -2_000,
                ..t
            },
        ),
    ];
    for (a, after) in steps {
        assert_eq!(t.adjust(a), after, "{a:?}");
    }

    // Hi-speed stops at 100 ms (fastest) and 4000 ms (slowest), also from
    // values off the 25 ms grid.
    let vis = |ms| Tuning {
        visible_time_ms: ms,
        ..t
    };
    assert_eq!(vis(100).adjust(Adjust::HiSpeedUp).visible_time_ms, 100);
    assert_eq!(vis(110).adjust(Adjust::HiSpeedUp).visible_time_ms, 100);
    assert_eq!(
        vis(4_000).adjust(Adjust::HiSpeedDown).visible_time_ms,
        4_000
    );
    assert_eq!(
        vis(3_990).adjust(Adjust::HiSpeedDown).visible_time_ms,
        4_000
    );
    assert_eq!(vis(600).visible_time(), TimeUs(600_000));
    // Volume stops at 0 % and at unity.
    let vol = |p| Tuning {
        volume_percent: p,
        ..t
    };
    assert_eq!(vol(0).adjust(Adjust::VolumeDown).volume_percent, 0);
    assert_eq!(vol(3).adjust(Adjust::VolumeDown).volume_percent, 0);
    assert_eq!(vol(100).adjust(Adjust::VolumeUp).volume_percent, 100);
    assert_eq!(vol(98).adjust(Adjust::VolumeUp).volume_percent, 100);
    assert_eq!(vol(0).volume(), 0.0);
    assert_eq!(vol(100).volume(), 1.0);
    // Offsets stop at ±1 s.
    let off = |us| Tuning {
        display_offset_us: us,
        input_offset_us: us,
        ..t
    };
    let top = off(999_500);
    assert_eq!(
        top.adjust(Adjust::DisplayOffsetUp).display_offset_us,
        1_000_000
    );
    assert_eq!(top.adjust(Adjust::InputOffsetUp).input_offset_us, 1_000_000);
    let bottom = off(-999_500);
    assert_eq!(
        bottom.adjust(Adjust::DisplayOffsetDown).display_offset_us,
        -1_000_000
    );
    assert_eq!(
        bottom.adjust(Adjust::InputOffsetDown).input_offset_us,
        -1_000_000
    );
}
