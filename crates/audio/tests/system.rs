//! The system voices (design.md "Audio", system sounds): starts overlap in
//! a fixed pool and are summed onto the chart's output at the system volume
//! with one clamp; chart operations leave them alone; the decision tone's
//! waveform is the one it always had; and each start resolves against the
//! generation current at its number, taken only through the step's cutoff.

mod system_rig;

use std::sync::Arc;

use audio::{
    AudioEngine, Channel, GenId, RecordingOutput, SYSTEM_VOICES, SimConfig, SimDevice, Sound,
    SoundBank, SoundCmd, SystemGeneration, SystemKind, SystemVoices,
};
use bms_model::TimeUs;
use system_rig::{RATE, Rig, constant, mix};

const PERIOD: u32 = 480;

/// A generation `id` holding `kinds`.
fn generation(id: u64, kinds: &[(SystemKind, Arc<Sound>)]) -> SystemGeneration {
    kinds
        .iter()
        .fold(SystemGeneration::new(GenId(id), RATE), |g, (k, s)| {
            g.with(*k, Arc::clone(s))
        })
}

/// S7, S8: starts of one kind overlap, each in its own voice, summed at the
/// system volume; a full pool refuses a start without touching the voices
/// sounding; the sum is clamped once, after every voice.
#[test]
fn starts_overlap_the_pool_bounds_and_one_clamp() {
    // Three starts of a constant 0.2 at volume 0.45: 0.27.
    let level = 6_554_i16;
    let mut voices = SystemVoices::new(RATE);
    let mut rig = Rig::new();
    rig.install(
        1,
        generation(1, &[(SystemKind::Scratch, constant(level, 4_800))]),
    );
    for seq in 2..=4 {
        rig.start(seq, SystemKind::Scratch);
    }
    rig.apply(&mut voices, 4);
    assert_eq!(voices.sounding(), 3);
    let a = f32::from(level) / 32_768.0 * 0.45;
    let want = a + a + a;
    let out = mix(&mut voices, 480, 0.45);
    assert!((want - 0.27).abs() < 1e-4);
    assert!(
        out.iter().all(|&x| (x - want).abs() < 1e-6),
        "{:?}",
        &out[..4]
    );

    // 32 voices at 32 phases of a ramp, then a 33rd start: refused and
    // counted, and the 32 sound on exactly as without it.
    let ramp = Arc::new(Sound::mono(
        (0..48_000)
            .map(|i| i16::try_from(i % 3_000).unwrap() - 1_500)
            .collect(),
    ));
    let run = |thirty_third: bool| {
        let mut voices = SystemVoices::new(RATE);
        let mut rig = Rig::new();
        rig.install(
            1,
            generation(1, &[(SystemKind::Scratch, Arc::clone(&ramp))]),
        );
        rig.apply(&mut voices, 1);
        for k in 0..SYSTEM_VOICES as u64 {
            rig.start(2 + k, SystemKind::Scratch);
            rig.apply(&mut voices, 2 + k);
            mix(&mut voices, 3, 1.0 / 64.0);
        }
        assert_eq!(voices.sounding(), SYSTEM_VOICES);
        if thirty_third {
            rig.start(100, SystemKind::Scratch);
            rig.apply(&mut voices, 100);
        }
        let out = mix(&mut voices, 480, 1.0 / 64.0);
        (voices.refused(), voices.sounding(), out)
    };
    let (refused, sounding, with) = run(true);
    let (none, _, without) = run(false);
    assert_eq!((refused, none, sounding), (1, 0, SYSTEM_VOICES));
    assert!(
        with.iter()
            .zip(&without)
            .all(|(a, b)| a.to_bits() == b.to_bits()),
        "a voice changed when the pool was full"
    );

    // +0.75, +0.75, -0.75 at volume 1: 0.75 with one clamp at the end.
    let mut voices = SystemVoices::new(RATE);
    let mut rig = Rig::new();
    rig.install(
        1,
        generation(
            1,
            &[
                (SystemKind::Scratch, constant(24_576, 4_800)),
                (SystemKind::FolderOpen, constant(24_576, 4_800)),
                (SystemKind::FolderClose, constant(-24_576, 4_800)),
            ],
        ),
    );
    rig.start(2, SystemKind::Scratch);
    rig.start(3, SystemKind::FolderOpen);
    rig.start(4, SystemKind::FolderClose);
    rig.apply(&mut voices, 4);
    let out = mix(&mut voices, 480, 1.0);
    assert!(out.iter().all(|&x| x == 0.75), "{:?}", &out[..4]);
}

/// An engine with nothing of a chart to play.
fn silent_engine() -> AudioEngine {
    let empty = Arc::new(SoundBank::from_sounds(RATE, Vec::new()));
    AudioEngine::new(empty, &[], RATE, PERIOD, 64, 64).unwrap()
}

/// Steps a silent engine past its priming wakes, applies `setup` to its
/// system voices, and steps it `steps` more times, calling `before(e, k,
/// at)` before step `k` with the last timed wake: the samples from then
/// on.
fn system_run(
    setup: impl FnOnce(&mut SystemVoices, &mut Rig),
    steps: usize,
    mut before: impl FnMut(&mut AudioEngine, usize, TimeUs),
) -> (AudioEngine, Vec<f32>) {
    let mut e = silent_engine();
    let dev = SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0)));
    let mut out = RecordingOutput::new(dev, RATE as usize * 4);
    let (_tx, mut rx) = rtrb::RingBuffer::<SoundCmd>::new(4);
    let mut at = TimeUs(0);
    for _ in 0..3 {
        if let Some(t) = e.step(&mut out, &mut rx).unwrap().at {
            at = t;
        }
    }
    let mut rig = Rig::new();
    setup(e.system_mut(), &mut rig);
    let from = out.samples().len();
    for k in 0..steps {
        before(&mut e, k, at);
        if let Some(t) = e.step(&mut out, &mut rx).unwrap().at {
            at = t;
        }
    }
    (e, out.samples()[from..].to_vec())
}

/// S8: a loop, a one-shot and the tone sound on, sample for sample,
/// through a bank's replacement, a stop, a restart, a seek, and a
/// ring-out to its end: none of them touches a system voice.
#[test]
fn system_voices_survive_chart_operations() {
    let setup = |voices: &mut SystemVoices, rig: &mut Rig| {
        let ramp = Arc::new(Sound::mono(
            (0..4_800)
                .map(|i| i16::try_from(i % 600).unwrap() * 20)
                .collect(),
        ));
        rig.install(
            1,
            generation(
                1,
                &[
                    (SystemKind::Select, ramp),
                    (SystemKind::Scratch, constant(-3_000, 24_000)),
                ],
            ),
        );
        rig.looped(2, SystemKind::Select, GenId(1));
        rig.start(3, SystemKind::Scratch);
        rig.tone = true;
        rig.apply(voices, 3);
        assert_eq!(voices.sounding(), 2);
        assert!(voices.tone_sounding());
    };
    let (_, alone) = system_run(setup, 70, |_, _, _| {});
    assert!(alone.iter().any(|&x| x != 0.0));
    let (e, through) = system_run(setup, 70, |e, k, at| match k {
        5 => {
            let bank = Arc::new(SoundBank::from_sounds(
                RATE,
                vec![Sound::mono(vec![1_000; 480])],
            ));
            e.replace(bank, audio::BgmSchedule::new(Vec::new()))
                .expect("same rate");
        }
        10 => e.stop(),
        15 => e.restart(at),
        20 => {
            let plan =
                audio::SeekPlan::new(&e.seek_source(), &[], e.frame_line(), TimeUs(10_000_000))
                    .unwrap();
            assert_eq!(e.seek(&plan), Ok(()));
        }
        25 => {
            e.restart(at);
            e.ring_out(TimeUs(0), TimeUs(20_000));
        }
        _ => {}
    });
    assert!(e.mixer().rung_out(), "the ring-out reached its end");
    assert_eq!(e.system().sounding(), 1, "the loop sounds on");
    assert!(
        through
            .iter()
            .zip(&alone)
            .all(|(a, b)| a.to_bits() == b.to_bits()),
        "a chart operation changed the system voices"
    );
    assert_eq!(through.len(), alone.len());
}

/// FNV-1a over the samples' bits.
fn checksum(samples: &[f32]) -> u64 {
    samples.iter().fold(0xcbf2_9ce4_8422_2325, |h, x| {
        (h ^ u64::from(x.to_bits())).wrapping_mul(0x0100_0000_01b3)
    })
}

/// S11: the decision tone, asked for through the control step, is the
/// tone the engine synthesized before the system voices existed: 28 800
/// frames at 48 kHz, at system volumes 1 and 0.45, against values captured
/// from that engine (32 frames' left samples and a checksum of every
/// sample), both channels alike, silent after its end.
#[test]
fn the_tone_waveform_is_unchanged() {
    let reference: [(f32, u64, [u32; 32]); 2] = [
        (
            1.0,
            0xb77c_9026_739c_bc95,
            [
                1_002_489_860,
                3_188_322_522,
                1_012_127_427,
                3_178_571_799,
                1_042_162_064,
                3_184_608_490,
                1_008_669_958,
                3_174_443_944,
                1_038_635_914,
                3_180_845_097,
                1_004_789_261,
                3_170_928_973,
                1_033_982_757,
                3_176_084_724,
                999_849_442,
                3_165_153_694,
                1_028_551_372,
                3_170_966_547,
                993_549_885,
                3_158_635_117,
                1_021_849_807,
                3_163_215_440,
                985_418_091,
                3_149_705_401,
                1_011_761_488,
                3_152_065_077,
                973_566_427,
                3_135_807_844,
                994_957_801,
                3_131_348_250,
                946_827_089,
                3_096_640_848,
            ],
        ),
        (
            0.45,
            0x0afd_9a90_ee71_e7d3,
            [
                992_837_840,
                3_178_777_378,
                1_002_350_511,
                3_168_576_532,
                1_032_737_128,
                3_174_848_415,
                999_238_789,
                3_164_861_463,
                1_028_724_732,
                3_171_461_361,
                994_907_301,
                3_160_890_762,
                1_024_536_891,
                3_166_338_165,
                990_461_464,
                3_155_661_377,
                1_018_809_783,
                3_160_958_395,
                983_953_002,
                3_148_955_797,
                1_011_939_514,
                3_153_717_289,
                975_795_526,
                3_140_080_192,
                1_002_021_166,
                3_142_203_900,
                963_890_416,
                3_125_894_669,
                985_220_126,
                3_121_881_034,
                936_869_321,
                3_087_288_929,
            ],
        ),
    ];
    for (volume, sum, picks) in reference {
        let mut e = silent_engine();
        e.set_volume(Channel::System, volume).unwrap();
        let dev = SimDevice::new(SimConfig::steady(RATE, PERIOD, TimeUs(0)));
        let mut out = RecordingOutput::new(dev, RATE as usize * 4);
        let (_tx, mut rx) = rtrb::RingBuffer::<SoundCmd>::new(4);
        for _ in 0..3 {
            e.step(&mut out, &mut rx).unwrap();
        }
        let from = out.samples().len();
        let mut rig = Rig::new();
        rig.tone = true;
        assert!(rig.apply(e.system_mut(), 0).tone);
        for _ in 0..61 {
            e.step(&mut out, &mut rx).unwrap();
        }
        assert!(!e.tone_sounding());
        let tone = &out.samples()[from..from + 57_600];
        assert_eq!(checksum(tone), sum, "volume {volume}");
        let got: Vec<u32> = (0..32).map(|k| tone[2 * (k * 900 + 7)].to_bits()).collect();
        assert_eq!(got, picks, "volume {volume}");
        assert!(tone.chunks(2).all(|c| c[0].to_bits() == c[1].to_bits()));
        assert!(out.samples()[from + 57_600..].iter().all(|&x| x == 0.0));
    }
}

/// The decision sound's level in a generation's `decide`.
const A_DECIDE: i16 = 8_192;
const B_DECIDE: i16 = 16_384;

/// The left sample the voices make now at volume 1.
fn level(voices: &mut SystemVoices) -> f32 {
    mix(voices, 1, 1.0)[0]
}

fn with_decide(id: u64, level: i16) -> SystemGeneration {
    generation(id, &[(SystemKind::Decide, constant(level, 48_000))])
}

/// Voices with generation A (id 1) installed, `decide` at `A_DECIDE` when
/// `a_has` it.
fn with_a(a_has: bool) -> (SystemVoices, Rig) {
    let mut voices = SystemVoices::new(RATE);
    let mut rig = Rig::new();
    let a = if a_has {
        with_decide(1, A_DECIDE)
    } else {
        generation(1, &[])
    };
    rig.install(1, a);
    rig.apply(&mut voices, 1);
    (voices, rig)
}

/// S7, S13: a decision sound plays the `decide` of the generation current
/// at its number and the tone when that one has none, whichever ring its
/// install and its start came in; starts and installs are taken in number
/// order only up to the snapshot's cutoff, and a merge ends at an install
/// that finds the retired slot taken, holding everything numbered above it
/// and the tone request.
#[test]
fn decide_resolves_against_the_generation_it_plays_from() {
    let b = |has: bool| {
        if has {
            with_decide(2, B_DECIDE)
        } else {
            generation(2, &[])
        }
    };
    let a_level = f32::from(A_DECIDE) / 32_768.0;
    let b_level = f32::from(B_DECIDE) / 32_768.0;

    // (i) A has it, B lacks it; start 10, install B 11: A's, from A
    // retired.
    let (mut voices, mut rig) = with_a(true);
    rig.start(10, SystemKind::Decide);
    rig.install(11, b(false));
    rig.apply(&mut voices, 11);
    assert_eq!(voices.current(), Some(GenId(2)));
    assert_eq!(voices.retired(), Some(GenId(1)));
    assert!(!voices.tone_sounding());
    assert_eq!(level(&mut voices), a_level);

    // (ii) the same generations, install B 10, start 11: the tone.
    let (mut voices, mut rig) = with_a(true);
    rig.install(10, b(false));
    rig.start(11, SystemKind::Decide);
    rig.apply(&mut voices, 11);
    assert!(voices.tone_sounding());
    assert_eq!(voices.sounding(), 0);
    assert_eq!(rig.returned(), [GenId(1)]);

    // (iii) A lacks it, B has it; start 10, install 11: the tone.
    let (mut voices, mut rig) = with_a(false);
    rig.start(10, SystemKind::Decide);
    rig.install(11, b(true));
    rig.apply(&mut voices, 11);
    assert!(voices.tone_sounding());
    assert_eq!(voices.sounding(), 0);

    // (iv) install 10, start 11: B's.
    let (mut voices, mut rig) = with_a(false);
    rig.install(10, b(true));
    rig.start(11, SystemKind::Decide);
    rig.apply(&mut voices, 11);
    assert!(!voices.tone_sounding());
    assert_eq!(level(&mut voices), b_level);

    // (v) two installs in one snapshot: B 10, decide 11, the empty install
    // 12, decide 13. 11 is below the empty install's stop and sounds
    // nothing; 13 is the tone; A and B both go back.
    let (mut voices, mut rig) = with_a(true);
    rig.install(10, b(true));
    rig.start(11, SystemKind::Decide);
    rig.install(12, SystemGeneration::empty(GenId(12)));
    rig.start(13, SystemKind::Decide);
    let report = rig.apply(&mut voices, 13);
    assert_eq!(report.installed, Some(GenId(12)));
    assert_eq!(voices.current(), Some(GenId(12)));
    assert_eq!(voices.sounding(), 0, "neither A's nor B's decide");
    assert!(voices.tone_sounding());
    assert_eq!(rig.returned(), [GenId(1), GenId(2)]);

    // Again behind a taken retired slot: R (id 1) holds a loop, A (id 3)
    // is current, the stops are raised to 12 and the tone is asked for.
    // The step applies nothing of 10-13, ends R's loop, sends R back at
    // its sweep and holds the tone; the next applies 10-13 and then starts
    // it.
    let mut voices = SystemVoices::new(RATE);
    let mut rig = Rig::new();
    rig.install(
        1,
        generation(1, &[(SystemKind::Select, constant(1_000, 48_000))]),
    );
    rig.looped(2, SystemKind::Select, GenId(1));
    rig.install(3, with_decide(3, A_DECIDE));
    rig.apply(&mut voices, 3);
    assert_eq!(
        (voices.current(), voices.retired()),
        (Some(GenId(3)), Some(GenId(1)))
    );
    rig.install(10, b(true));
    rig.start(11, SystemKind::Decide);
    rig.install(12, SystemGeneration::empty(GenId(12)));
    rig.start(13, SystemKind::Decide);
    for kind in SystemKind::ALL {
        rig.stop(12, kind);
    }
    rig.tone = true;
    let report = rig.apply(&mut voices, 13);
    assert!(report.blocked);
    assert_eq!(report.installed, None);
    assert_eq!(voices.install_waits(), 1);
    assert_eq!(voices.current(), Some(GenId(3)));
    assert_eq!(voices.sounding(), 0, "R's loop ended");
    assert!(
        !voices.tone_sounding(),
        "the tone waits for the empty install"
    );
    assert!(voices.tone_held());
    assert_eq!(rig.installs_rx.slots(), 2);
    assert_eq!(rig.starts_rx.slots(), 2);
    rig.sweep(&mut voices);
    assert_eq!(voices.retired(), None);
    let report = rig.apply(&mut voices, 13);
    assert!(!report.blocked);
    assert_eq!(report.installed, Some(GenId(12)));
    assert!(voices.tone_sounding());
    assert!(!voices.tone_held());
    assert_eq!(voices.sounding(), 0);
    assert_eq!(voices.install_waits(), 1);
    assert_eq!(rig.returned(), [GenId(1), GenId(3), GenId(2)]);

    // (vi) the cutoff: B 10 and decide 11 in their rings, cutoff 9:
    // neither is taken; cutoff 11: B's.
    let (mut voices, mut rig) = with_a(true);
    rig.install(10, b(true));
    rig.start(11, SystemKind::Decide);
    let report = rig.apply(&mut voices, 9);
    assert_eq!((report.installed, report.taken), (None, 0));
    assert_eq!(voices.current(), Some(GenId(1)));
    assert_eq!(voices.sounding(), 0);
    rig.apply(&mut voices, 11);
    assert_eq!(voices.current(), Some(GenId(2)));
    assert_eq!(level(&mut voices), b_level);
    // A straddler numbered 9, from another producer, arriving the step
    // after B's install: against B.
    let (mut voices, mut rig) = with_a(true);
    rig.install(10, b(true));
    rig.apply(&mut voices, 10);
    rig.start(9, SystemKind::Decide);
    rig.apply(&mut voices, 10);
    assert_eq!(voices.sounding(), 1);
    assert_eq!(level(&mut voices), b_level);
}
