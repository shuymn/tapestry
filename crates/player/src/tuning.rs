//! The settings keys' values while the game runs (stage1.md PR16): the
//! window thread steps them on F1 to F8, the render thread reads hi-speed
//! and the display offset every frame, and the game thread forwards the
//! volume to the audio thread, takes the input offset into the next play
//! and writes the files when a play ends. Lock-free and allocation-free.

use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, AtomicU64, Ordering};

use game::{Adjust, DataRoot, Settings, Tuning, save_settings};

/// The four values, shared.
#[derive(Debug)]
pub struct LiveTuning {
    visible_time_ms: AtomicU32,
    volume_percent: AtomicU32,
    /// The volume as loaded, exact (`f32` bits), which plays until a
    /// volume key moves it to a whole percent.
    loaded_volume: AtomicU32,
    /// Whether a volume key has moved the volume.
    volume_moved: AtomicBool,
    display_offset_us: AtomicI64,
    input_offset_us: AtomicI64,
    /// Counts the changes, so readers notice one.
    version: AtomicU64,
    /// Whether the end screen shows: only it takes the input offset keys.
    end_screen: AtomicBool,
    /// Held by a step and by closing the end screen, so an input offset
    /// step is either in the next play's values or refused, never lost
    /// between the two. The render thread reads without it.
    step: Mutex<()>,
}

impl LiveTuning {
    /// The values `t`, with no end screen shown.
    pub fn new(t: Tuning) -> Self {
        Self {
            visible_time_ms: AtomicU32::new(t.visible_time_ms),
            volume_percent: AtomicU32::new(t.volume_percent),
            loaded_volume: AtomicU32::new(t.volume().to_bits()),
            volume_moved: AtomicBool::new(false),
            display_offset_us: AtomicI64::new(t.display_offset_us),
            input_offset_us: AtomicI64::new(t.input_offset_us),
            version: AtomicU64::new(0),
            end_screen: AtomicBool::new(false),
            step: Mutex::new(()),
        }
    }

    /// The values `t`, with the volume as loaded, `volume` (between whole
    /// percents, as `Tuning` keeps it), playing until a volume key moves it.
    pub fn with_loaded_volume(self, volume: f32) -> Self {
        self.loaded_volume
            .store(volume.to_bits(), Ordering::Release);
        self
    }

    /// The master volume to play: as loaded until a volume key moves it.
    pub fn volume(&self) -> f32 {
        if self.volume_moved.load(Ordering::Acquire) {
            self.get().volume()
        } else {
            f32::from_bits(self.loaded_volume.load(Ordering::Acquire))
        }
    }

    /// The current values. Only the window thread changes them, one at a
    /// time, so a read between two changes sees each value as it is.
    pub fn get(&self) -> Tuning {
        Tuning {
            visible_time_ms: self.visible_time_ms.load(Ordering::Acquire),
            volume_percent: self.volume_percent.load(Ordering::Acquire),
            display_offset_us: self.display_offset_us.load(Ordering::Acquire),
            input_offset_us: self.input_offset_us.load(Ordering::Acquire),
        }
    }

    /// How many changes there have been.
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Acquire)
    }

    /// One step of a settings key, from the window thread. The input
    /// offset keys work on the end screen only, so a play never changes
    /// its own input offset (D13). Returns whether a value changed.
    pub fn adjust(&self, a: Adjust) -> bool {
        let _step = self
            .step
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if a.is_input_offset() && !self.end_screen.load(Ordering::Acquire) {
            return false;
        }
        let old = self.get();
        let new = old.adjust(a);
        if new == old {
            return false;
        }
        self.visible_time_ms
            .store(new.visible_time_ms, Ordering::Release);
        if new.volume_percent != old.volume_percent {
            self.volume_percent
                .store(new.volume_percent, Ordering::Release);
            self.volume_moved.store(true, Ordering::Release);
        }
        self.display_offset_us
            .store(new.display_offset_us, Ordering::Release);
        self.input_offset_us
            .store(new.input_offset_us, Ordering::Release);
        self.version.fetch_add(1, Ordering::AcqRel);
        true
    }

    /// The end screen shows: the input offset keys work.
    pub fn open_end_screen(&self) {
        self.end_screen.store(true, Ordering::Release);
    }

    /// A play is about to start: the input offset keys stop working, and
    /// the values the play starts with are returned, in one step with
    /// respect to [`adjust`](Self::adjust).
    pub fn close_end_screen(&self) -> Tuning {
        let _step = self
            .step
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        self.end_screen.store(false, Ordering::Release);
        self.get()
    }
}

/// The settings keys' values as last delivered (to the audio thread and the
/// end screen): nothing, until the first delivery.
#[derive(Debug, Default)]
pub struct Delivered(Option<u64>);

impl Delivered {
    /// The values, if they changed since the last delivery or none was
    /// made yet.
    pub fn take_new(&mut self, tuning: &LiveTuning) -> Option<Tuning> {
        let version = tuning.version();
        if self.0 == Some(version) {
            return None;
        }
        self.0 = Some(version);
        Some(tuning.get())
    }
}

/// `device.toml` and `player.toml` as the settings keys leave them.
#[derive(Debug)]
pub struct SettingsFiles {
    root: DataRoot,
    /// What the files hold: as loaded, then as last written.
    written: Settings,
}

impl SettingsFiles {
    /// The files in `root`, holding `loaded`.
    pub const fn new(root: DataRoot, loaded: Settings) -> Self {
        Self {
            root,
            written: loaded,
        }
    }

    /// Writes the values `t` and the periods the audio output queues into
    /// the file that holds each, atomically and only when it changes.
    /// Called when a play ends and when the run ends, never during a play.
    /// A file that cannot be written keeps its old content and is tried
    /// again at the next call. Returns the files written.
    pub fn write(&mut self, t: Tuning, queued_periods: u32) -> Vec<&'static str> {
        let mut next = self.written.clone();
        t.apply_to(&mut next);
        next.device.queued_periods = queued_periods;
        let mut wrote = Vec::new();
        if next.device != self.written.device {
            let path = self.root.device_settings();
            match save_settings(&path, &next.device) {
                Ok(()) => {
                    self.written.device = next.device;
                    wrote.push("device.toml");
                }
                Err(e) => eprintln!("{}: {e}", path.display()),
            }
        }
        if next.player != self.written.player {
            let path = self.root.player_settings();
            match save_settings(&path, &next.player) {
                Ok(()) => {
                    self.written.player = next.player;
                    wrote.push("player.toml");
                }
                Err(e) => eprintln!("{}: {e}", path.display()),
            }
        }
        for name in &wrote {
            println!("settings: wrote {name}");
        }
        wrote
    }
}

#[cfg(test)]
mod tests {
    use game::{Adjust, Tuning};

    use super::{Delivered, LiveTuning, SettingsFiles};

    fn start() -> Tuning {
        Tuning {
            visible_time_ms: 600,
            volume_percent: 100,
            display_offset_us: 0,
            input_offset_us: 2_000,
        }
    }

    #[test]
    fn input_offset_keys_work_on_the_end_screen_only() {
        let t = LiveTuning::new(start());
        assert!(!t.adjust(Adjust::InputOffsetUp));
        assert!(!t.adjust(Adjust::InputOffsetDown));
        assert_eq!((t.get(), t.version()), (start(), 0));
        t.open_end_screen();
        assert!(t.adjust(Adjust::InputOffsetUp));
        assert_eq!(t.get().input_offset_us, 3_000);
        assert_eq!(t.version(), 1);
        // Closing it returns the values the next play starts with, and
        // refuses input offset steps from then on.
        assert_eq!(t.close_end_screen().input_offset_us, 3_000);
        assert!(!t.adjust(Adjust::InputOffsetDown));
        assert!(!t.adjust(Adjust::InputOffsetUp));
        assert_eq!(t.close_end_screen().input_offset_us, 3_000);
        assert_eq!(t.get().input_offset_us, 3_000);
    }

    /// A volume changed before the game thread first looks (during
    /// startup, after the engine took the loaded volume) is delivered at
    /// the first look; then only a new change is.
    #[test]
    fn the_first_look_delivers_a_change_made_before_it() {
        let t = LiveTuning::new(start());
        assert!(t.adjust(Adjust::VolumeDown));
        let mut seen = Delivered::default();
        assert_eq!(seen.take_new(&t).map(|v| v.volume_percent), Some(95));
        assert_eq!(seen.take_new(&t), None);
        assert!(t.adjust(Adjust::VolumeDown));
        assert_eq!(seen.take_new(&t).map(|v| v.volume_percent), Some(90));
        assert_eq!(seen.take_new(&t), None);
    }

    /// A volume between whole percents plays as loaded until a volume key
    /// moves it; other keys leave it.
    #[test]
    fn the_loaded_volume_plays_until_a_volume_key_moves_it() {
        let quiet = Tuning {
            volume_percent: 0,
            ..start()
        };
        let t = LiveTuning::new(quiet).with_loaded_volume(0.004);
        assert_eq!(t.volume(), 0.004);
        assert!(t.adjust(Adjust::HiSpeedUp));
        assert_eq!(t.volume(), 0.004);
        assert!(t.adjust(Adjust::VolumeUp));
        assert_eq!(t.volume(), 0.05);
        assert!(t.adjust(Adjust::VolumeDown));
        assert_eq!(t.volume(), 0.0);
        // Without a loaded volume, the whole percent plays.
        assert_eq!(LiveTuning::new(start()).volume(), 1.0);
    }

    #[test]
    fn a_step_at_a_bound_changes_nothing() {
        let t = LiveTuning::new(start());
        // Volume is at unity already.
        assert!(!t.adjust(Adjust::VolumeUp));
        assert_eq!(t.version(), 0);
        assert!(t.adjust(Adjust::HiSpeedUp));
        assert!(t.adjust(Adjust::DisplayOffsetDown));
        assert_eq!(
            t.get(),
            Tuning {
                visible_time_ms: 569,
                display_offset_us: -1_000,
                ..start()
            }
        );
        assert_eq!(t.version(), 2);
    }

    /// Only a changed file is written, and one that cannot be written is
    /// written at the next call.
    #[test]
    fn a_failed_write_is_tried_again_and_unchanged_files_are_left() {
        let dir = tempfile::tempdir().unwrap();
        let root = game::DataRoot::new(dir.path().to_owned());
        let loaded = game::Settings {
            device: game::DeviceSettings::default(),
            player: game::PlayerSettings::default(),
        };
        let mut files = SettingsFiles::new(root.clone(), loaded.clone());
        let t = Tuning::of(&loaded);
        assert!(files.write(t, 2).is_empty());
        assert!(!root.device_settings().exists());
        // `player.toml` cannot be written: a directory is in the way.
        std::fs::create_dir_all(root.player_settings().join("x")).unwrap();
        let faster = t.adjust(Adjust::HiSpeedUp);
        assert!(files.write(faster, 2).is_empty());
        std::fs::remove_dir_all(root.player_settings()).unwrap();
        assert_eq!(files.write(faster, 2), ["player.toml"]);
        assert!(files.write(faster, 2).is_empty());
        let s = game::Settings::load(&root).unwrap();
        assert_eq!(s.player.visible_time_ms, 569);
        assert!(!root.device_settings().exists());
        assert_eq!(
            files.write(faster.adjust(Adjust::VolumeDown), 2),
            ["device.toml"]
        );
    }

    /// A raised audio queue goes to `device.toml` alone; a write that fails
    /// is tried again at the next call, and the choice is not lost.
    #[test]
    fn a_raised_queue_is_written_to_device_toml_and_retried() {
        let dir = tempfile::tempdir().unwrap();
        let root = game::DataRoot::new(dir.path().to_owned());
        let loaded = game::Settings {
            device: game::DeviceSettings::default(),
            player: game::PlayerSettings::default(),
        };
        let mut files = SettingsFiles::new(root.clone(), loaded.clone());
        let t = Tuning::of(&loaded);
        // `device.toml` cannot be written: a directory is in the way.
        std::fs::create_dir_all(root.device_settings().join("x")).unwrap();
        assert!(files.write(t, 3).is_empty());
        std::fs::remove_dir_all(root.device_settings()).unwrap();
        assert_eq!(files.write(t, 3), ["device.toml"]);
        assert!(files.write(t, 3).is_empty());
        let s = game::Settings::load(&root).unwrap();
        assert_eq!(s.device.queued_periods, 3);
        assert!(!root.player_settings().exists());
        // Set back by hand and loaded again, the default is written as is.
        assert_eq!(files.write(t, 2), ["device.toml"]);
        assert_eq!(
            game::Settings::load(&root).unwrap().device.queued_periods,
            2
        );
    }
}
