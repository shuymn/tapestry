//! Settings files, local records and the atomic write they share.

use std::fs;

use bms_model::{Lane, TimeUs};
use eval::{Binding, ControlId, SourceProfile};
use game::{
    DataRoot, DeviceSettings, KeyConfig, LaneKey, LocalRecords, PlayerSettings, RecordSink,
    Settings, SettingsError, load_settings, save_settings, write_atomic,
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
                lanes: vec![LaneKey {
                    control: 7,
                    lane: 4,
                }],
                retry: None,
                quit: Some(9),
            },
            input_offset_us: 4_500,
            audio_offset_us: -12_000,
            display_offset_us: 16_700,
            audio_device: Some("device-a".to_owned()),
            master_volume: 0.75,
        },
        player: PlayerSettings {
            hi_speed_milli: 2_350,
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
fn settings_round_trip_atomically() {
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
        "hi_speed_milli = 2350\npreset = \"wide.toml\"\ndisplayed_gauge = 1\n",
    )
    .unwrap();
    assert_eq!(Settings::load(&root).unwrap().player, s.player);
}

#[test]
fn unreadable_settings_are_errors_not_defaults() {
    let (_dir, root) = temp_root();
    fs::create_dir_all(root.path()).unwrap();
    fs::write(root.player_settings(), "hi_speed_milli = \"fast\"").unwrap();
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
                bindings: vec![Binding {
                    control: ControlId(7),
                    lane: lane(4)
                }],
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
    assert!(!dir.path().join("target.tmp").exists());
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
