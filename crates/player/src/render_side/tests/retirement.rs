//! The render loop's retirement machinery on fake screens (design.md
//! "Memory Budget"): the handoff table's places named by the result's
//! report until the worker's release is read, the game woken when one
//! leaves, and READY's cutoff given, kept and ended.

use super::*;
use crate::render_side::CutoffPhase;

/// The wakes READY's cutoff gave, the report's, its publications and the
/// release reads.
fn counts(link: &super::super::RenderLink) -> [u64; 4] {
    link.retire_counts()
}

/// A render thread on new installers drawing a menu page, so its
/// boundaries run every frame, with a game wake counted.
fn drawing() -> (super::super::RenderLink, Arc<AtomicU64>) {
    let link = Installers::default().spawn_bare();
    let woken = Arc::new(AtomicU64::new(0));
    let w = Arc::clone(&woken);
    link.set_game_wake(Arc::new(move || {
        w.fetch_add(1, Ordering::AcqRel);
    }));
    link.send(RenderMsg::Menu(Box::default()));
    let reads = counts(&link)[3];
    assert!(
        wait_until(|| counts(&link)[3] > reads + 2),
        "boundaries run"
    );
    (link, woken)
}

/// A result place's pages stay in the report until the worker dropped
/// them and the release was read; their leaving publishes the report
/// and wakes the game thread.
#[test]
fn a_handed_result_atlas_stays_reported_until_its_release_is_read() {
    let (link, woken) = drawing();
    let gate = link.worker_gate();
    gate.hold(true);
    let pages = Arc::clone(bundle().atlas_pages());
    let id = pages.id();
    let weak = Arc::downgrade(&pages);
    link.seed_handoff(ScreenKind::Result, pages);
    assert!(wait_until(|| link.result_status().retained.contains(&id)));
    // Held by the worker: still named, frames after frames.
    let reads = counts(&link)[3];
    assert!(wait_until(|| counts(&link)[3] > reads + 3));
    assert!(link.result_status().retained.contains(&id));
    assert!(weak.upgrade().is_some(), "the worker holds the pages");
    let [_, report_wakes, _, _] = counts(&link);
    let game_wakes = woken.load(Ordering::Acquire);
    gate.hold(false);
    assert!(wait_until(|| !link.result_status().retained.contains(&id)));
    assert!(weak.upgrade().is_none(), "the worker dropped the pages");
    assert_eq!(
        counts(&link)[1],
        report_wakes + 1,
        "one wake for the id leaving"
    );
    assert!(woken.load(Ordering::Acquire) > game_wakes);
    link.exit();
}

/// READY's cutoff asked with no entry filled is given and wakes the game;
/// under it no release is read and nothing is published, frames going
/// on; the game's next screen ends it and the reads resume.
#[test]
fn the_cutoff_is_given_kept_and_ended_by_the_next_screen() {
    let (link, woken) = drawing();
    assert_ne!(link.cutoff_phase(), CutoffPhase::CutOff);
    let before = woken.load(Ordering::Acquire);
    link.ask_cutoff();
    assert!(wait_until(|| link.cutoff_phase() == CutoffPhase::CutOff));
    // Cut off by the render thread's own look, or by the game's swap
    // while it waited for its frame: the first wakes the game.
    let [cutoff_wakes, _, published, reads] = counts(&link);
    assert!(cutoff_wakes <= 1 && woken.load(Ordering::Acquire) >= before + cutoff_wakes);
    let frames = link.shared.clock_paced.load(Ordering::SeqCst);
    assert!(wait_until(|| link
        .shared
        .clock_paced
        .load(Ordering::SeqCst)
        > frames + 3));
    assert_eq!(counts(&link)[3], reads, "no release read under the cutoff");
    assert_eq!(counts(&link)[2], published, "nothing published under it");
    assert_eq!(link.cutoff_phase(), CutoffPhase::CutOff);
    // The next screen ends it.
    link.send(RenderMsg::Menu(Box::default()));
    assert!(wait_until(|| counts(&link)[3] > reads));
    assert_ne!(link.cutoff_phase(), CutoffPhase::CutOff);
    link.exit();
}

/// V6's cutoff case (design.md "Scope of No Allocation", "Memory
/// Budget"), on a real menu screen on WARP: a result atlas's pages in a
/// handoff place with the worker's gate shut, the cutoff asked once the
/// loading lifetime paused and acknowledged; the gate opened before `Play`
/// so the place's flag is set, the Idle loop left for several
/// `CANCEL_POLL` periods, a deferred opening's resumed loading frames and
/// its pause, then `Play`, play frames and `Finish`. From the
/// acknowledgement to `Finish`'s turn, the boundaries' release reads,
/// polls, sends and publications allocate nothing (a positive control
/// counted in the same span), no release is read and nothing published,
/// and the id stays in `retained` until `Finish`'s own pass reads the
/// release and publishes without it.
// Revert: the cutoff keyed on the render mode (`cut_off` read as
// `matches!(mode, Mode::Play { .. })` for the boundary's reads and
// publication) -> the Idle pass the place's flag wakes reads the release
// and publishes before `Play`: "a release read under the cutoff".
#[test]
fn the_cutoff_reads_and_publishes_nothing_until_finish_alloc() {
    let _turn = crate::warp_turn::take();
    let chart = ChartBuilder::new(120_000)
        .tap(Lane::new(1).unwrap(), TimeUs(60_000_000))
        .build()
        .unwrap();
    let make: MakeScreen = Box::new(|| {
        let r = render::Renderer::new(
            render::Target::Offscreen {
                width: 1_920,
                height: 1_080,
            },
            render::AdapterChoice::Warp,
        )?;
        Ok(Box::new(crate::menu_screen::MenuScreen::new(
            r,
            Some(render::text::Fonts::runtime()?),
        )) as Box<dyn Screen>)
    });
    let clock = QpcClock::start();
    let shared_chart = Arc::new(chart.clone());
    let link = spawn(
        make,
        Some(Arc::clone(&shared_chart)),
        None,
        clock,
        view(),
        measure(clock),
        Box::new(NoFrames),
    )
    .unwrap();
    link.set_launch_elapsed(Some(Duration::from_secs(3_600)));
    let gate = link.worker_gate();
    gate.hold(true);
    let pages = Arc::clone(bundle().atlas_pages());
    let id = pages.id();
    let weak = Arc::downgrade(&pages);
    link.seed_handoff(ScreenKind::Result, pages);
    let frames = || link.shared.stamps[0].load(Ordering::SeqCst);
    link.loading().show(one_line("L"));
    assert!(wait_until(|| frames() > 2));
    let ready = link.prepare_attempt(attempt(&shared_chart, None), || false);
    assert_eq!(ready.unwrap(), Preparation::Ready);
    link.commit_attempt().unwrap();
    assert!(wait_until(|| link.result_status().retained.contains(&id)));
    // The end of loading (a gated opening's pause), then READY's cutoff.
    link.pause_loading().unwrap();
    link.ask_cutoff();
    assert!(wait_until(|| link.cutoff_phase() == CutoffPhase::CutOff));
    let [_, _, published, reads] = counts(&link);
    let [allocations, control] = link.boundary_allocations();
    // The worker drops the pages and sets the place's flag.
    gate.hold(false);
    assert!(wait_until(|| weak.upgrade().is_none()));
    std::thread::sleep(super::super::CANCEL_POLL * 6);
    // A deferred opening: its loading frames again, then its pause. The
    // positive control: the resumption's pass allocates once in its
    // counted span.
    link.arm_boundary_control();
    link.resume_loading();
    let resumed = frames();
    assert!(wait_until(|| frames() > resumed + 3));
    assert!(
        link.boundary_allocations()[1] > control,
        "the control not counted"
    );
    link.pause_loading().unwrap();
    let (rule, device) = (rules::tapestry(), DeviceProfile::default());
    let (_logic, side) = Logic::<SourceReader>::new(
        &chart,
        &rule,
        &device,
        &InputStart::default(),
        Vec::new(),
        LogicConfig {
            inputs: 0,
            effects: 16,
        },
        Vec::new(),
    )
    .unwrap();
    link.send(RenderMsg::Play {
        state_origin: TimeUs(0),
        side,
        gauge: 0,
        measure: play_measure(TimeUs(clock.now().0 + 50_000)),
    });
    let playing = frames();
    assert!(wait_until(|| frames() > playing + 5));
    assert_eq!(counts(&link)[3], reads, "a release read under the cutoff");
    assert_eq!(counts(&link)[2], published, "published under the cutoff");
    assert_eq!(
        link.boundary_allocations()[0],
        allocations,
        "the boundaries allocated under the cutoff"
    );
    assert!(link.result_status().retained.contains(&id));
    assert_eq!(link.cutoff_phase(), CutoffPhase::CutOff);
    // Finish's own pass reads the release and publishes without it.
    let _ = link.finish(clock.now());
    assert!(wait_until(|| !link.result_status().retained.contains(&id)));
    assert!(counts(&link)[3] > reads);
    link.exit();
}
