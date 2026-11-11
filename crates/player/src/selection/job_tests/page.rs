//! The practice page's jobs on the real rig (design.md "State Model", the
//! practice page; "Load Pipeline"): begun at the page's read, advanced by
//! its passes with no input, joined or decoded again by a Start, settled
//! when the chart is left. Every interleaving is met at the jobs' probe
//! ([`PageAt`]) or the load's ([`Probe`]), never by timing.

use std::path::{Path, PathBuf};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use bms_model::Speed;
use game::coord::{Class, JobKind, Scope, Spawner};
use game::{AppRig as _, Loading};

use super::{
    Bench, Decodes, Probe, at_each_barrier, begun, bench, decodes_with, hook, installed, loaded,
    on_rig, outcome, practiced, sel, until, watchdog, witnessed, write_sound,
};
use crate::page_jobs::{PageAt, PageJobs, PageProbe};
use crate::play::PlayerRig;
use crate::render_side::tests::Installers;

/// A chart `dir/song/a.bms` of its own title `tag` (its resolved hash its
/// own, so its analyses are counted apart), playing `key.wav`.
fn song(dir: &Path, tag: &str) -> PathBuf {
    let songs = dir.join("song");
    std::fs::create_dir_all(&songs).unwrap();
    write_sound(dir, 3_000);
    let path = songs.join("a.bms");
    let text = format!("#PLAYER 1\n#TITLE {tag}\n#BPM 120\n#WAV01 key.wav\n#00101:01\n#00111:01\n");
    std::fs::write(&path, text).unwrap();
    path
}

/// A hold armed at a point: where it says it holds, and what lets it go.
type Held = (PageAt, Sender<()>, Receiver<()>);

/// Something done once at a point.
type Action = (PageAt, Box<dyn FnOnce() + Send>);

/// The page jobs' probe: what each said, holds at armed points, one-shot
/// actions and panics.
#[derive(Default)]
struct Page {
    seen: Mutex<Vec<PageAt>>,
    holds: Mutex<Vec<Held>>,
    actions: Mutex<Vec<Action>>,
    panics: Mutex<Vec<PageAt>>,
}

impl Page {
    fn probe(self: &Arc<Self>) -> PageProbe {
        let page = Arc::clone(self);
        Arc::new(move |at| page.at(at))
    }

    fn at(&self, at: PageAt) {
        self.seen.lock().unwrap().push(at);
        let action = {
            let mut actions = self.actions.lock().unwrap();
            let i = actions.iter().position(|(a, _)| *a == at);
            i.map(|i| actions.remove(i).1)
        };
        if let Some(action) = action {
            action();
        }
        let hold = {
            let mut holds = self.holds.lock().unwrap();
            let i = holds.iter().position(|(a, ..)| *a == at);
            i.map(|i| holds.remove(i))
        };
        if let Some((_, tell, go)) = hold {
            let _ = tell.send(());
            let _ = go.recv();
        }
        let panics = {
            let mut panics = self.panics.lock().unwrap();
            let i = panics.iter().position(|a| *a == at);
            i.map(|i| panics.remove(i))
        };
        assert!(panics.is_none(), "a page job's panic at {at:?} (test)");
    }

    /// Holds the next `at`: says when it holds, waits for the sender.
    fn hold(&self, at: PageAt) -> (Receiver<()>, Sender<()>) {
        let (tell, held) = mpsc::channel();
        let (go, wait) = mpsc::channel();
        self.holds.lock().unwrap().push((at, tell, wait));
        (held, go)
    }

    /// Runs `f` at the next `at`, before any hold there.
    fn on(&self, at: PageAt, f: impl FnOnce() + Send + 'static) {
        self.actions.lock().unwrap().push((at, Box::new(f)));
    }

    /// Panics at the next `at`, after any hold there.
    fn panic_at(&self, at: PageAt) {
        self.panics.lock().unwrap().push(at);
    }

    fn count(&self, at: PageAt) -> usize {
        self.seen
            .lock()
            .unwrap()
            .iter()
            .filter(|a| **a == at)
            .count()
    }
}

/// A bench for a practised chart of its own title `tag`, the page jobs
/// probed by `page`, their threads in `mode`.
fn page_bench(
    dir: &Path,
    tag: &str,
    decodes: Decodes,
    spawner: Spawner,
    page: &Arc<Page>,
    mode: Arc<dyn crate::priority::ThreadMode>,
) -> (Bench, PathBuf) {
    let path = song(dir, tag);
    let mut b = bench(dir, decodes, spawner);
    let s = b.sel.as_mut().unwrap();
    s.page = PageJobs::new(mode);
    s.page.probe = Some(page.probe());
    (b, path)
}

fn plain(dir: &Path, tag: &str, page: &Arc<Page>) -> (Bench, PathBuf) {
    page_bench(
        dir,
        tag,
        Decodes::default(),
        Spawner::SYSTEM,
        page,
        Arc::new(crate::priority::Unchanged),
    )
}

/// The page's passes until its jobs have all been taken.
fn passes_until_idle(rig: &mut PlayerRig<'_>) {
    until("the page's jobs taken", || {
        rig.poll_page().unwrap();
        let p = &sel(rig).page;
        !p.decoding() && !p.measuring()
    });
}

/// The resolved hash of the chart at `path`, what its analyses count by.
fn hash(dir: &Path, path: &Path) -> [u8; 32] {
    loaded(dir, path, Speed::NORMAL)
        .chart()
        .identity
        .resolved_hash
}

/// P1: the page's read begins the 1× bank as an owned job of base class
/// speculative, unpromoted: held at its first file, it holds a
/// speculative permit and no foreground one.
#[test]
fn the_page_begins_its_1x_bank_under_page_policy() {
    let dir = tempfile::tempdir().unwrap();
    let page = Arc::new(Page::default());
    let (mut b, path) = plain(dir.path(), "p1", &page);
    let (held, go) = page.hold(PageAt::File(0));
    let read = loaded(dir.path(), &path, Speed::NORMAL);
    on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| {
            rig.practice_read(&read, &mut game::Decision::off())
                .unwrap();
            held.recv_timeout(watchdog()).expect("the page's decode");
            assert_eq!(b.coord.held(Class::Speculative), 1);
            assert_eq!(b.coord.held(Class::Foreground), 0);
            go.send(()).unwrap();
            passes_until_idle(rig);
            assert!(sel(rig).page.kept().is_some());
        },
    );
    assert!(begun(&b.decodes).is_empty(), "no scoped decode");
}

/// P1c: with no input, a bank finished after the page's read is looked up
/// by the next pass, which on a miss begins the measurement; a later pass
/// keeps it, so the chart is measured once, by the page.
#[test]
fn the_page_advances_its_jobs_without_input() {
    let dir = tempfile::tempdir().unwrap();
    let page = Arc::new(Page::default());
    let (mut b, path) = plain(dir.path(), "p1c", &page);
    let (held, go) = page.hold(PageAt::File(0));
    let read = loaded(dir.path(), &path, Speed::NORMAL);
    let hash = hash(dir.path(), &path);
    on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| {
            rig.practice_read(&read, &mut game::Decision::off())
                .unwrap();
            held.recv_timeout(watchdog()).expect("the page's decode");
            rig.poll_page().unwrap();
            assert!(!sel(rig).page.measuring(), "nothing published yet");
            go.send(()).unwrap();
            until("the measurement begun", || {
                rig.poll_page().unwrap();
                sel(rig).page.measuring()
            });
            passes_until_idle(rig);
            assert!(sel(rig).page.gain_known());
            let (chart, bank) = super::key_now(&read);
            assert!(
                sel(rig)
                    .shelf
                    .look_up_page(&chart, &bank, "a.bms")
                    .is_none(),
                "the measurement kept"
            );
        },
    );
    assert_eq!(crate::loudness::analyses::of(hash), 1);
}

/// P1d: a Start's quit leaves the page bank promoted, so the Start is
/// responsible for it; its panic, published later on the page, ends the
/// run at the next pass.
#[test]
fn a_responsible_page_panic_ends_the_run_at_the_next_pass() {
    let dir = tempfile::tempdir().unwrap();
    let page = Arc::new(Page::default());
    let (mut b, path) = plain(dir.path(), "p1d", &page);
    let (held, go) = page.hold(PageAt::File(0));
    page.panic_at(PageAt::File(0));
    let read = loaded(dir.path(), &path, Speed::NORMAL);
    let start = practiced(dir.path(), &path, Speed::NORMAL);
    on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| {
            rig.practice_read(&read, &mut game::Decision::off())
                .unwrap();
            held.recv_timeout(watchdog()).expect("the page's decode");
            rig.controls.command(game::Command::Quit);
            assert_eq!(
                outcome(rig.load_chart(&start, &mut game::Decision::off())),
                Loading::Cancelled
            );
            while game::Rig::take(rig).is_some() {}
            go.send(()).unwrap();
            let mut ended = None;
            until("the run's end", || {
                ended = rig.poll_page().err();
                ended.is_some()
            });
            assert!(
                matches!(
                    ended,
                    Some(game::LiveError::Load(game::coord::LoadFailure {
                        job: JobKind::Bank,
                        cause: game::coord::Failure::Panicked(_),
                    }))
                ),
                "{ended:?}"
            );
        },
    );
}

/// P4: the page bank's outcome. A gain already known: no measurement. A
/// miss: the page measures. A bank refused (a sound past the length cap):
/// the refusal is kept, and the Start is refused with it. A refused mode
/// (the job's own cancel): the Start decodes it again.
#[test]
fn the_page_gain_after_hit_miss_refusal_or_cancel() {
    // A hit: measured before, by an earlier page of the same chart.
    let dir = tempfile::tempdir().unwrap();
    let page = Arc::new(Page::default());
    let (mut b, path) = plain(dir.path(), "p4-hit", &page);
    let read = loaded(dir.path(), &path, Speed::NORMAL);
    let hash = hash(dir.path(), &path);
    on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| {
            rig.practice_read(&read, &mut game::Decision::off())
                .unwrap();
            passes_until_idle(rig);
            rig.left_chart().unwrap();
            rig.entered_selection();
            rig.practice_read(&read, &mut game::Decision::off())
                .unwrap();
            until("the bank kept", || {
                rig.poll_page().unwrap();
                sel(rig).page.kept().is_some()
            });
            assert!(sel(rig).page.gain_known(), "a hit");
            assert!(!sel(rig).page.measuring());
        },
    );
    assert_eq!(crate::loudness::analyses::of(hash), 1, "measured once");

    // A miss is P1c's. A refusal:
    let dir = tempfile::tempdir().unwrap();
    let page = Arc::new(Page::default());
    let (mut b, path) = plain(dir.path(), "p4-refused", &page);
    let mut bytes = crate::loudness::fixture::wav(&crate::loudness::fixture::square(0.01, 1_000));
    let data: u32 = 4_800_001 * 2;
    bytes[4..8].copy_from_slice(&(36 + data).to_le_bytes());
    bytes[40..44].copy_from_slice(&data.to_le_bytes());
    std::fs::write(dir.path().join("song").join("key.wav"), bytes).unwrap();
    let read = loaded(dir.path(), &path, Speed::NORMAL);
    let start = practiced(dir.path(), &path, Speed::NORMAL);
    let got = on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| {
            rig.practice_read(&read, &mut game::Decision::off())
                .unwrap();
            passes_until_idle(rig);
            rig.load_chart(&start, &mut game::Decision::off())
        },
    );
    assert!(
        matches!(
            got,
            Err(super::ChartLoadError::Sounds(
                audio::BankError::OverCap { .. }
            ))
        ),
        "{got:?}"
    );
    assert!(begun(&b.decodes).is_empty(), "the Start decoded nothing");

    // A refused mode: the job runs no more, and the Start decodes it.
    let dir = tempfile::tempdir().unwrap();
    let page = Arc::new(Page::default());
    let mode = Arc::new(crate::background::tests::Mode::default());
    mode.refuse_entry_from
        .store(1, std::sync::atomic::Ordering::SeqCst);
    let (mut b, path) = page_bench(
        dir.path(),
        "p4-cancel",
        Decodes::default(),
        Spawner::SYSTEM,
        &page,
        mode,
    );
    let read = loaded(dir.path(), &path, Speed::NORMAL);
    let start = practiced(dir.path(), &path, Speed::NORMAL);
    on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| {
            rig.practice_read(&read, &mut game::Decision::off())
                .unwrap();
            passes_until_idle(rig);
            assert!(sel(rig).page.kept().is_none(), "nothing kept");
            assert_eq!(
                outcome(rig.load_chart(&start, &mut game::Decision::off())),
                Loading::Ready
            );
            assert!(installed(rig, &start));
        },
    );
    assert_eq!(begun(&b.decodes), [Speed::NORMAL], "decoded by the Start");
}

/// P6, P16a: a Start at 1× joins the page bank queued (the speculative
/// permit held elsewhere, as by the background) or running (held at its
/// file until promoted): promoted, it is admitted on a foreground permit,
/// decodes once, and is installed with no decode of the Start's own.
#[test]
fn a_start_at_1x_joins_the_bank_in_each_state() {
    for queued in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let page = Arc::new(Page::default());
        let (mut b, path) = plain(dir.path(), "p6", &page);
        let lease = queued.then(|| b.coord.try_lease(Class::Speculative).unwrap());
        let (held, go) = page.hold(PageAt::File(0));
        let read = loaded(dir.path(), &path, Speed::NORMAL);
        let start = practiced(dir.path(), &path, Speed::NORMAL);
        on_rig(
            b.sel.take().unwrap(),
            &Installers::default(),
            false,
            |rig| {
                rig.practice_read(&read, &mut game::Decision::off())
                    .unwrap();
                if queued {
                    assert_eq!(b.coord.held(Class::Speculative), 1, "the lease only");
                    let (coord, controls) = (b.coord.clone(), rig.controls);
                    std::thread::scope(|s| {
                        let admitted = s.spawn(move || {
                            // Never admitted, the Start is ended with the
                            // quit key, so the test fails here and does
                            // not wait for ever.
                            if held.recv_timeout(watchdog() / 2).is_err() {
                                controls.command(game::Command::Quit);
                                return false;
                            }
                            assert_eq!(coord.held(Class::Speculative), 1, "the lease only");
                            go.send(()).unwrap();
                            true
                        });
                        let got = outcome(rig.load_chart(&start, &mut game::Decision::off()));
                        assert!(
                            admitted.join().unwrap(),
                            "the queued bank was never admitted by the Start's promotion"
                        );
                        assert_eq!(got, Loading::Ready);
                    });
                } else {
                    held.recv_timeout(watchdog()).expect("running");
                    page.on(PageAt::Promoted(JobKind::Bank), move || {
                        go.send(()).unwrap()
                    });
                    assert_eq!(
                        outcome(rig.load_chart(&start, &mut game::Decision::off())),
                        Loading::Ready
                    );
                }
                assert!(installed(rig, &start), "queued: {queued}");
            },
        );
        drop(lease);
        assert_eq!(
            page.count(PageAt::Decoded),
            1,
            "one decode, queued: {queued}"
        );
        assert!(begun(&b.decodes).is_empty(), "queued: {queued}");
        assert_eq!(b.coord.held(Class::Foreground), 0);
        assert_eq!(witnessed(&b).1, Vec::<String>::new());
    }
}

/// P7: the page bank joined and published, the quit pressed at the
/// release barrier, before the load's look: it is not installed.
#[test]
fn a_finished_bank_is_not_installed_after_a_quit() {
    let dir = tempfile::tempdir().unwrap();
    let page = Arc::new(Page::default());
    let decodes = decodes_with(hook(|p, controls, _| {
        if p == Probe::Barrier {
            controls.command(game::Command::Quit);
        }
    }));
    let (mut b, path) = page_bench(
        dir.path(),
        "p7",
        decodes,
        Spawner::SYSTEM,
        &page,
        Arc::new(crate::priority::Unchanged),
    );
    let read = loaded(dir.path(), &path, Speed::NORMAL);
    let start = practiced(dir.path(), &path, Speed::NORMAL);
    on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| {
            // The read's own barrier passes before the hook is armed.
            sel(rig).decodes = None;
            rig.practice_read(&read, &mut game::Decision::off())
                .unwrap();
            sel(rig).decodes = Some(Arc::clone(&b.decodes));
            passes_until_idle(rig);
            assert_eq!(
                outcome(rig.load_chart(&start, &mut game::Decision::off())),
                Loading::Cancelled
            );
            assert!(!installed(rig, &start));
        },
    );
    assert_eq!(page.count(PageAt::Decoded), 1);
}

/// P8: a quit while the Start waits for the page bank leaves it to the
/// page, demoted, its decode going on; the chart's choice is not
/// advanced, and the next Start takes the same decode.
#[test]
fn a_quit_while_joining_keeps_the_page_job_for_the_next_start() {
    let dir = tempfile::tempdir().unwrap();
    let page = Arc::new(Page::default());
    let (mut b, path) = plain(dir.path(), "p8", &page);
    let (held, go) = page.hold(PageAt::File(0));
    let read = loaded(dir.path(), &path, Speed::NORMAL);
    let start = practiced(dir.path(), &path, Speed::NORMAL);
    on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| {
            rig.practice_read(&read, &mut game::Decision::off())
                .unwrap();
            held.recv_timeout(watchdog()).expect("the page's decode");
            let choice = b.coord.current(Scope::Choice);
            rig.controls.command(game::Command::Quit);
            assert_eq!(
                outcome(rig.load_chart(&start, &mut game::Decision::off())),
                Loading::Cancelled
            );
            while game::Rig::take(rig).is_some() {}
            assert_eq!(b.coord.current(Scope::Choice), choice, "the choice stays");
            assert!(sel(rig).page.decoding(), "kept for the page");
            go.send(()).unwrap();
            assert_eq!(
                outcome(rig.load_chart(&start, &mut game::Decision::off())),
                Loading::Ready
            );
            assert!(installed(rig, &start));
        },
    );
    assert_eq!(page.count(PageAt::Decoded), 1, "the same decode");
    assert!(begun(&b.decodes).is_empty());
}

/// P8b, P14: a page bank the Start joined panics: inside the Start's wait,
/// or after a quit left it to the page and a second Start finds it
/// published. Either way the Start was responsible: the run ends.
#[test]
fn a_panic_in_a_joined_page_job_ends_the_run() {
    for after_quit in [false, true] {
        let dir = tempfile::tempdir().unwrap();
        let page = Arc::new(Page::default());
        let (mut b, path) = plain(dir.path(), "p8b", &page);
        let (held, go) = page.hold(PageAt::File(0));
        page.panic_at(PageAt::File(0));
        let read = loaded(dir.path(), &path, Speed::NORMAL);
        let start = practiced(dir.path(), &path, Speed::NORMAL);
        let got = on_rig(
            b.sel.take().unwrap(),
            &Installers::default(),
            false,
            |rig| {
                rig.practice_read(&read, &mut game::Decision::off())
                    .unwrap();
                held.recv_timeout(watchdog()).expect("the page's decode");
                if after_quit {
                    rig.controls.command(game::Command::Quit);
                    assert_eq!(
                        outcome(rig.load_chart(&start, &mut game::Decision::off())),
                        Loading::Cancelled
                    );
                    while game::Rig::take(rig).is_some() {}
                    go.send(()).unwrap();
                } else {
                    page.on(PageAt::Promoted(JobKind::Bank), move || {
                        go.send(()).unwrap()
                    });
                }
                outcome(rig.load_chart(&start, &mut game::Decision::off()))
            },
        );
        assert!(
            matches!(
                got,
                Loading::Failed(game::coord::LoadFailure {
                    job: JobKind::Bank,
                    cause: game::coord::Failure::Panicked(_),
                })
            ),
            "after a quit: {after_quit}: {got:?}"
        );
    }
}

/// P13, P14b: a page job that fails before any Start is responsible for
/// it is done again by the Start: its thread refused (owned jobs only),
/// or its panic published on the page.
#[test]
fn a_speculative_failure_is_redone_by_the_start() {
    for refused in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let page = Arc::new(Page::default());
        let spawner = if refused {
            Spawner::refusing_owned(&[JobKind::Bank])
        } else {
            page.panic_at(PageAt::File(0));
            Spawner::SYSTEM
        };
        let (mut b, path) = page_bench(
            dir.path(),
            "p13",
            Decodes::default(),
            spawner,
            &page,
            Arc::new(crate::priority::Unchanged),
        );
        let read = loaded(dir.path(), &path, Speed::NORMAL);
        let start = practiced(dir.path(), &path, Speed::NORMAL);
        on_rig(
            b.sel.take().unwrap(),
            &Installers::default(),
            false,
            |rig| {
                rig.practice_read(&read, &mut game::Decision::off())
                    .unwrap();
                passes_until_idle(rig);
                assert_eq!(
                    outcome(rig.load_chart(&start, &mut game::Decision::off())),
                    Loading::Ready
                );
                assert!(installed(rig, &start));
            },
        );
        assert_eq!(begun(&b.decodes), [Speed::NORMAL], "refused: {refused}");
    }
}

/// P9, P10, P19: a Start at another speed. With the gain unknown it joins
/// the page's 1× bank, measures on it, and only then runs the barrier and
/// its own decode; a quit during that decode keeps the gain, so the next
/// Start at that speed measures nothing; with the gain known it goes
/// straight to the barrier.
#[test]
fn another_speed_measures_on_1x_before_its_own_decode() {
    let dir = tempfile::tempdir().unwrap();
    let page = Arc::new(Page::default());
    let order = Arc::new(Mutex::new(Vec::new()));
    let (seen, quit_once) = (
        Arc::clone(&order),
        Arc::new(std::sync::atomic::AtomicBool::new(true)),
    );
    let decodes = decodes_with(hook(move |p, controls, _| {
        if matches!(p, Probe::BeforeMeasure | Probe::Barrier | Probe::Decode(_)) {
            seen.lock().unwrap().push(p);
        }
        if p == Probe::Decode(Speed::FASTEST)
            && quit_once.swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            controls.command(game::Command::Quit);
        }
    }));
    let (mut b, path) = page_bench(
        dir.path(),
        "p10",
        decodes,
        Spawner::SYSTEM,
        &page,
        Arc::new(crate::priority::Unchanged),
    );
    let read = loaded(dir.path(), &path, Speed::NORMAL);
    let fast = practiced(dir.path(), &path, Speed::FASTEST);
    let hash = hash(dir.path(), &path);
    on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| {
            rig.practice_read(&read, &mut game::Decision::off())
                .unwrap();
            order.lock().unwrap().clear();
            // No page pass: the Start finds the bank and the gain unknown.
            assert_eq!(
                outcome(rig.load_chart(&fast, &mut game::Decision::off())),
                Loading::Cancelled
            );
            while game::Rig::take(rig).is_some() {}
            assert_eq!(
                *order.lock().unwrap(),
                [
                    Probe::BeforeMeasure,
                    Probe::Barrier,
                    Probe::Decode(Speed::FASTEST)
                ],
                "1×, its measurement, the barrier, then the speed's decode"
            );
            assert!(sel(rig).page.gain_known(), "kept across the quit");
            assert!(
                sel(rig).page.kept().is_none(),
                "the barrier dropped the 1× bank"
            );
            order.lock().unwrap().clear();
            assert_eq!(
                outcome(rig.load_chart(&fast, &mut game::Decision::off())),
                Loading::Ready
            );
            assert!(installed(rig, &fast));
            assert_eq!(
                *order.lock().unwrap(),
                [Probe::Barrier, Probe::Decode(Speed::FASTEST)],
                "the gain known: straight to the barrier"
            );
        },
    );
    assert_eq!(
        crate::loudness::analyses::of(hash),
        1,
        "measured once, on 1×"
    );
    assert_eq!(page.count(PageAt::Decoded), 1, "one 1× decode, the page's");
    assert_eq!(witnessed(&b).1, Vec::<String>::new());
}

/// P11, P17: Back with the page's jobs running, finished or published and
/// not taken: the choice advances and every job is settled before the
/// selection is entered; nothing of the page is kept, its bank is freed,
/// and nothing is held.
#[test]
fn back_settles_the_page_jobs_in_each_state() {
    for running in [true, false] {
        let dir = tempfile::tempdir().unwrap();
        let page = Arc::new(Page::default());
        let (mut b, path) = plain(dir.path(), "p11", &page);
        let hold = running.then(|| page.hold(PageAt::File(0)));
        let read = loaded(dir.path(), &path, Speed::NORMAL);
        on_rig(
            b.sel.take().unwrap(),
            &Installers::default(),
            false,
            |rig| {
                rig.practice_read(&read, &mut game::Decision::off())
                    .unwrap();
                let choice = b.coord.current(Scope::Choice);
                if let Some((held, go)) = hold {
                    held.recv_timeout(watchdog()).expect("running");
                    // Let go: its next look finds the choice's cancel.
                    go.send(()).unwrap();
                } else {
                    until("published", || {
                        sel(rig).page.decoding()
                            && b.coord.held(Class::Speculative) == 0
                            && b.witness.now().1 == 1
                    });
                }
                rig.left_chart().unwrap();
                rig.entered_selection();
                assert_ne!(b.coord.current(Scope::Choice), choice);
                let p = &sel(rig).page;
                assert!(!p.decoding() && !p.measuring() && p.kept().is_none());
                assert!(p.chart_of(&path).is_none());
            },
        );
        assert_eq!(b.witness.now(), (0, 0), "running: {running}");
        assert_eq!(b.coord.held(Class::Speculative), 0);
        assert_eq!(b.coord.held(Class::Foreground), 0);
    }
}

/// B2': the page's read runs the release barrier before its decode: the
/// played bank is alive at the barrier and none after it; the witness
/// records no violation.
#[test]
fn page_entry_releases_the_played_bank_first() {
    let dir = tempfile::tempdir().unwrap();
    let page = Arc::new(Page::default());
    let slot = Arc::new(Mutex::new(None));
    let (hook, seen) = at_each_barrier(&slot);
    let (mut b, path) = page_bench(
        dir.path(),
        "b2",
        decodes_with(hook),
        Spawner::SYSTEM,
        &page,
        Arc::new(crate::priority::Unchanged),
    );
    *slot.lock().unwrap() = Some(Arc::clone(&b.witness));
    let played = loaded(dir.path(), &path, Speed::NORMAL);
    on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| {
            assert_eq!(
                outcome(rig.load_chart(&played, &mut game::Decision::off())),
                Loading::Ready
            );
            rig.practice_read(&played, &mut game::Decision::off())
                .unwrap();
            passes_until_idle(rig);
        },
    );
    assert_eq!(*seen.lock().unwrap(), [(0, 0), (0, 1)]);
    assert_eq!(witnessed(&b), (2, Vec::new()));
    slot.lock().unwrap().take();
}

/// B5', B5'': Starts at 1×, 1×, another speed and 1× again. The second at
/// 1× installs the page's bank again with no decode; after the other
/// speed released it, the return to 1× decodes afresh. Every barrier finds
/// only the allowed bank alive.
#[test]
fn a_start_at_1x_reuses_the_page_bank_until_another_speed_releases_it() {
    let dir = tempfile::tempdir().unwrap();
    let page = Arc::new(Page::default());
    let (mut b, path) = plain(dir.path(), "b5", &page);
    let read = loaded(dir.path(), &path, Speed::NORMAL);
    on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| {
            rig.practice_read(&read, &mut game::Decision::off())
                .unwrap();
            passes_until_idle(rig);
            for speed in [Speed::NORMAL, Speed::NORMAL, Speed::FASTEST, Speed::NORMAL] {
                let start = practiced(dir.path(), &path, speed);
                assert_eq!(
                    outcome(rig.load_chart(&start, &mut game::Decision::off())),
                    Loading::Ready,
                    "{speed:?}"
                );
                assert!(installed(rig, &start), "{speed:?}");
            }
        },
    );
    assert_eq!(page.count(PageAt::Decoded), 1, "the page's one 1× decode");
    assert_eq!(
        begun(&b.decodes),
        [Speed::FASTEST, Speed::NORMAL],
        "the other speed's, then a fresh 1×"
    );
    assert_eq!(witnessed(&b).1, Vec::<String>::new());
}

/// What a hook saw of the witness at each decode's start: the decode's
/// speed, its decodes in flight and the banks alive.
type AtDecode = Arc<Mutex<Vec<(Speed, usize, usize)>>>;

/// A hook that records, at each decode's start on the Bank job, what the
/// witness sees, and its record.
fn at_each_decode(
    witness: &Arc<Mutex<Option<Arc<crate::bank_witness::BankWitness>>>>,
) -> (Option<super::Hook>, AtDecode) {
    let seen = AtDecode::default();
    let (w, s) = (Arc::clone(witness), Arc::clone(&seen));
    let hook = hook(move |p, _, _| {
        if let Probe::Decode(speed) = p
            && let Some(w) = w.lock().unwrap().as_ref()
        {
            let (in_flight, alive) = w.now();
            s.lock().unwrap().push((speed, in_flight, alive));
        }
    });
    (hook, seen)
}

/// The returns from another speed decode 1× afresh only once the release
/// barrier has freed the speed's bank: at each decode's start no bank is
/// alive. A refused unload at that barrier refuses the Start before any
/// decode.
#[test]
fn a_return_to_1x_releases_the_speed_bank_before_its_decode() {
    let dir = tempfile::tempdir().unwrap();
    let page = Arc::new(Page::default());
    let slot = Arc::new(Mutex::new(None));
    let (hook, seen) = at_each_decode(&slot);
    let (mut b, path) = page_bench(
        dir.path(),
        "fold3",
        decodes_with(hook),
        Spawner::SYSTEM,
        &page,
        Arc::new(crate::priority::Unchanged),
    );
    *slot.lock().unwrap() = Some(Arc::clone(&b.witness));
    let read = loaded(dir.path(), &path, Speed::NORMAL);
    on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| {
            rig.practice_read(&read, &mut game::Decision::off())
                .unwrap();
            passes_until_idle(rig);
            for speed in [Speed::FASTEST, Speed::NORMAL] {
                let start = practiced(dir.path(), &path, speed);
                assert_eq!(
                    outcome(rig.load_chart(&start, &mut game::Decision::off())),
                    Loading::Ready,
                    "{speed:?}"
                );
            }
            assert_eq!(
                *seen.lock().unwrap(),
                [(Speed::FASTEST, 1, 0), (Speed::NORMAL, 1, 0)],
                "a bank alive at a decode's start"
            );
            // A refused unload: no decode.
            seen.lock().unwrap().clear();
            let fast = practiced(dir.path(), &path, Speed::FASTEST);
            assert_eq!(
                outcome(rig.load_chart(&fast, &mut game::Decision::off())),
                Loading::Ready
            );
            seen.lock().unwrap().clear();
            sel(rig).unload_fault = Some(game::AudioLoadError::Busy);
            let start = practiced(dir.path(), &path, Speed::NORMAL);
            let got = rig.load_chart(&start, &mut game::Decision::off());
            assert!(
                matches!(
                    got,
                    Err(super::ChartLoadError::Audio(game::AudioLoadError::Busy))
                ),
                "{got:?}"
            );
            assert_eq!(*seen.lock().unwrap(), [], "decoded past a refused unload");
        },
    );
    slot.lock().unwrap().take();
}

/// The page's sounds replaced after the page measured them: a Start at
/// another speed finds its bank's key is not the one the gain is of, and
/// measures the sounds as they are on 1× again before its own decode, so
/// it plays at their gain, not at the headroom.
#[test]
fn sounds_replaced_after_the_page_measured_are_measured_again() {
    let dir = tempfile::tempdir().unwrap();
    let page = Arc::new(Page::default());
    let order = Arc::new(Mutex::new(Vec::new()));
    let seen = Arc::clone(&order);
    let decodes = decodes_with(hook(move |p, _, _| {
        if matches!(p, Probe::BeforeMeasure | Probe::Barrier | Probe::Decode(_)) {
            seen.lock().unwrap().push(p);
        }
    }));
    let (mut b, path) = page_bench(
        dir.path(),
        "fold2",
        decodes,
        Spawner::SYSTEM,
        &page,
        Arc::new(crate::priority::Unchanged),
    );
    let read = loaded(dir.path(), &path, Speed::NORMAL);
    let hash = hash(dir.path(), &path);
    on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| {
            rig.practice_read(&read, &mut game::Decision::off())
                .unwrap();
            passes_until_idle(rig);
            assert!(sel(rig).page.gain_known());
            assert_eq!(crate::loudness::analyses::of(hash), 1);
            write_sound(dir.path(), 9_000);
            order.lock().unwrap().clear();
            let fast = practiced(dir.path(), &path, Speed::FASTEST);
            assert_eq!(
                outcome(rig.load_chart(&fast, &mut game::Decision::off())),
                Loading::Ready
            );
            assert_eq!(
                crate::loudness::analyses::of(hash),
                2,
                "the replaced sounds were not measured"
            );
            assert_eq!(
                *order.lock().unwrap(),
                [
                    Probe::Barrier,
                    Probe::Decode(Speed::FASTEST),
                    Probe::Barrier,
                    Probe::Decode(Speed::NORMAL),
                    Probe::BeforeMeasure,
                    Probe::Barrier,
                    Probe::Decode(Speed::FASTEST),
                ],
                "the speed's bank dropped, then 1×, its measurement, the barrier and the speed again"
            );
            assert_ne!(
                sel(rig).shelf.gain(),
                audio::Gain::HEADROOM,
                "played at the headroom"
            );
            let (chart, bank) = super::key_now(&read);
            assert!(
                sel(rig)
                    .shelf
                    .look_up_page(&chart, &bank, "a.bms")
                    .is_none(),
                "the new sounds' measurement kept"
            );
        },
    );
    assert_eq!(witnessed(&b).1, Vec::<String>::new());
}

/// A failure of the input or audio thread that comes after the leave's
/// settle took its first, healthy, look at the terminal causes bounds it:
/// a page bank that never stops (held at its file) is given up within the
/// stop bound on the settle's clock, and the chart is left.
#[test]
fn a_failure_during_the_leave_bounds_its_settle() {
    let dir = tempfile::tempdir().unwrap();
    let page = Arc::new(Page::default());
    let (mut b, path) = plain(dir.path(), "fold1", &page);
    let (held, go) = page.hold(PageAt::File(0));
    let read = loaded(dir.path(), &path, Speed::NORMAL);
    on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| {
            rig.practice_read(&read, &mut game::Decision::off())
                .unwrap();
            held.recv_timeout(watchdog()).expect("the page's decode");
            let base = std::time::Instant::now();
            let reads = Arc::new(std::sync::atomic::AtomicU64::new(0));
            let r = Arc::clone(&reads);
            sel(rig).page.set_clock(Arc::new(move || {
                base + std::time::Duration::from_millis(
                    100 * r.fetch_add(1, std::sync::atomic::Ordering::SeqCst),
                )
            }));
            let lost = Arc::clone(&rig.audio_lost);
            // The failure comes right after the settle's first look, which
            // found none.
            let fails = Arc::clone(&lost);
            page.on(PageAt::Looked, move || {
                fails.store(true, std::sync::atomic::Ordering::Release);
            });
            let returned = std::sync::atomic::AtomicBool::new(false);
            let bounded = std::thread::scope(|s| {
                let helper = s.spawn(|| {
                    let began = std::time::Instant::now();
                    while !returned.load(std::sync::atomic::Ordering::SeqCst)
                        && began.elapsed() < watchdog() / 2
                    {
                        std::thread::park_timeout(std::time::Duration::from_millis(1));
                    }
                    let bounded = returned.load(std::sync::atomic::Ordering::SeqCst);
                    // Let the job go either way, so nothing waits for ever.
                    go.send(()).unwrap();
                    bounded
                });
                let left = rig.left_chart();
                returned.store(true, std::sync::atomic::Ordering::SeqCst);
                assert!(left.is_ok(), "{left:?}");
                helper.join().unwrap()
            });
            assert!(
                bounded,
                "the settle was not bounded by the failure that came after it began"
            );
            lost.store(false, std::sync::atomic::Ordering::Release);
        },
    );
}

/// A Start's quit leaves the page bank promoted, so the Start is
/// responsible for it; Back then settles it, and it panics during that
/// settle: the panic is not dropped with the chart, and the run ends.
#[test]
fn a_responsible_panic_during_the_leave_ends_the_run() {
    let dir = tempfile::tempdir().unwrap();
    let page = Arc::new(Page::default());
    let (mut b, path) = plain(dir.path(), "fold4", &page);
    let (held, go) = page.hold(PageAt::File(0));
    page.panic_at(PageAt::File(0));
    let read = loaded(dir.path(), &path, Speed::NORMAL);
    let start = practiced(dir.path(), &path, Speed::NORMAL);
    let left = on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| {
            rig.practice_read(&read, &mut game::Decision::off())
                .unwrap();
            held.recv_timeout(watchdog()).expect("the page's decode");
            rig.controls.command(game::Command::Quit);
            assert_eq!(
                outcome(rig.load_chart(&start, &mut game::Decision::off())),
                Loading::Cancelled
            );
            while game::Rig::take(rig).is_some() {}
            assert!(sel(rig).page.decoding(), "kept for the page, unpublished");
            let coord = b.coord.clone();
            let choice = coord.current(Scope::Choice);
            std::thread::scope(|s| {
                s.spawn(move || {
                    // Published only once the leave has begun.
                    until("the leave began", || coord.current(Scope::Choice) != choice);
                    go.send(()).unwrap();
                });
                rig.left_chart()
            })
        },
    );
    assert!(
        matches!(
            left,
            Err(game::LiveError::Load(game::coord::LoadFailure {
                job: JobKind::Bank,
                cause: game::coord::Failure::Panicked(_),
            }))
        ),
        "the panic dropped with the chart: {left:?}"
    );
}

/// The page's measurement fails (its analysis panics, caught as a
/// failure): the gain stays unknown and the page does not measure again
/// on its later passes; the next Start measures it, once, and keeps it.
#[test]
fn a_failed_page_measurement_is_measured_again_by_the_start() {
    let dir = tempfile::tempdir().unwrap();
    let page = Arc::new(Page::default());
    page.panic_at(PageAt::Block);
    let (mut b, path) = plain(dir.path(), "fold5", &page);
    let read = loaded(dir.path(), &path, Speed::NORMAL);
    let start = practiced(dir.path(), &path, Speed::NORMAL);
    let hash = hash(dir.path(), &path);
    on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| {
            rig.practice_read(&read, &mut game::Decision::off())
                .unwrap();
            passes_until_idle(rig);
            // The page tries once: its next passes measure nothing.
            assert_eq!(
                crate::loudness::analyses::of(hash),
                1,
                "the page measured again"
            );
            for _ in 0..3 {
                rig.poll_page().unwrap();
                assert!(!sel(rig).page.measuring(), "the page measured again");
            }
            assert_eq!(
                crate::loudness::analyses::of(hash),
                1,
                "the page measured again"
            );
            assert!(!sel(rig).page.gain_known(), "a failure is not a known gain");
            assert_eq!(
                outcome(rig.load_chart(&start, &mut game::Decision::off())),
                Loading::Ready
            );
            assert_eq!(
                crate::loudness::analyses::of(hash),
                2,
                "the Start did not measure again"
            );
            let (chart, bank) = super::key_now(&read);
            assert!(
                sel(rig)
                    .shelf
                    .look_up_page(&chart, &bank, "a.bms")
                    .is_none(),
                "the Start's measurement kept"
            );
        },
    );
}

/// The sounds replaced while the page still measures the old ones: a
/// Start at another speed joins that measurement, then its speed's bank
/// shows other sounds, so it measures them on 1× again before it plays,
/// not at the headroom.
#[test]
fn sounds_replaced_during_the_page_measurement_are_measured_again() {
    let dir = tempfile::tempdir().unwrap();
    let page = Arc::new(Page::default());
    let (mut b, path) = plain(dir.path(), "r2-gain", &page);
    let (held, go) = page.hold(PageAt::Block);
    page.on(PageAt::Promoted(JobKind::Measure), move || {
        go.send(()).unwrap()
    });
    let read = loaded(dir.path(), &path, Speed::NORMAL);
    let hash = hash(dir.path(), &path);
    on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| {
            rig.practice_read(&read, &mut game::Decision::off())
                .unwrap();
            until("the page measures", || {
                rig.poll_page().unwrap();
                sel(rig).page.measuring()
            });
            held.recv_timeout(watchdog())
                .expect("inside the page's measurement");
            assert!(!sel(rig).page.gain_known(), "the gain unknown at the Start");
            write_sound(dir.path(), 9_000);
            let fast = practiced(dir.path(), &path, Speed::FASTEST);
            assert_eq!(
                outcome(rig.load_chart(&fast, &mut game::Decision::off())),
                Loading::Ready
            );
            assert_eq!(
                crate::loudness::analyses::of(hash),
                2,
                "the replaced sounds were not measured"
            );
            assert_ne!(
                sel(rig).shelf.gain(),
                audio::Gain::HEADROOM,
                "played at the headroom"
            );
        },
    );
    assert_eq!(witnessed(&b).1, Vec::<String>::new());
}

/// A Start's quit leaves the page bank promoted; the run then ends (a
/// close) before the page looks at it again, and it panics while the run's
/// end settles it: the run ends with that panic, after its cleanup.
#[test]
fn a_responsible_panic_at_the_runs_end_ends_the_run_with_it() {
    let dir = tempfile::tempdir().unwrap();
    let page = Arc::new(Page::default());
    let (mut b, path) = plain(dir.path(), "r2-end", &page);
    let (held, go) = page.hold(PageAt::File(0));
    page.panic_at(PageAt::File(0));
    let read = loaded(dir.path(), &path, Speed::NORMAL);
    let start = practiced(dir.path(), &path, Speed::NORMAL);
    let ended = on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| {
            rig.practice_read(&read, &mut game::Decision::off())
                .unwrap();
            held.recv_timeout(watchdog()).expect("the page's decode");
            rig.controls.command(game::Command::Quit);
            assert_eq!(
                outcome(rig.load_chart(&start, &mut game::Decision::off())),
                Loading::Cancelled
            );
            while game::Rig::take(rig).is_some() {}
            // Published only once the run's end looks at it.
            page.on(PageAt::Looked, move || go.send(()).unwrap());
            rig.page_at_end()
        },
    );
    assert!(
        matches!(
            ended,
            Err(game::coord::LoadFailure {
                job: JobKind::Bank,
                cause: game::coord::Failure::Panicked(_),
            })
        ),
        "the panic dropped at the run's end: {ended:?}"
    );
}

/// A page bank the Start was responsible for publishes its bank, then its
/// thread panics as it ends: found by the page's next pass, the panic ends
/// the run; the bank is not kept.
#[test]
fn a_panic_after_publication_ends_the_run_through_the_pass() {
    panic_after_publication(false);
}

/// As [`a_panic_after_publication_ends_the_run_through_the_pass`], found
/// by a Start that joins the bank.
#[test]
fn a_panic_after_publication_ends_the_run_through_the_start() {
    panic_after_publication(true);
}

/// A panic after publication of a page bank a Start was responsible for,
/// found through a Start that joins it (`joined`) or the page's pass.
fn panic_after_publication(joined: bool) {
    {
        let dir = tempfile::tempdir().unwrap();
        let page = Arc::new(Page::default());
        let (mut b, path) = plain(dir.path(), "r2-after", &page);
        let (held, go) = page.hold(PageAt::File(0));
        sel_of(&mut b).page.panic_after_publication = true;
        let read = loaded(dir.path(), &path, Speed::NORMAL);
        let start = practiced(dir.path(), &path, Speed::NORMAL);
        let failure = on_rig(
            b.sel.take().unwrap(),
            &Installers::default(),
            false,
            |rig| {
                rig.practice_read(&read, &mut game::Decision::off())
                    .unwrap();
                held.recv_timeout(watchdog()).expect("the page's decode");
                if joined {
                    page.on(PageAt::Promoted(JobKind::Bank), move || {
                        go.send(()).unwrap()
                    });
                    match outcome(rig.load_chart(&start, &mut game::Decision::off())) {
                        Loading::Failed(f) => Some(f),
                        other => panic!("the panic after publication was dropped: {other:?}"),
                    }
                } else {
                    rig.controls.command(game::Command::Quit);
                    assert_eq!(
                        outcome(rig.load_chart(&start, &mut game::Decision::off())),
                        Loading::Cancelled
                    );
                    while game::Rig::take(rig).is_some() {}
                    go.send(()).unwrap();
                    let mut ended = None;
                    until("the page's pass finds the panic", || {
                        match rig.poll_page() {
                            Err(game::LiveError::Load(f)) => ended = Some(f),
                            Err(e) => panic!("{e:?}"),
                            Ok(()) => assert!(
                                sel(rig).page.kept().is_none(),
                                "the panic after publication was dropped"
                            ),
                        }
                        ended.is_some()
                    });
                    ended
                }
            },
        );
        assert!(
            matches!(
                failure,
                Some(game::coord::LoadFailure {
                    job: JobKind::Bank,
                    cause: game::coord::Failure::Panicked(_),
                })
            ),
            "joined: {joined}: {failure:?}"
        );
    }
}

/// The bench's selection, before the rig takes it.
fn sel_of(b: &mut Bench) -> &mut super::Selection {
    b.sel.as_mut().unwrap()
}

/// The page's measurement whose thread is refused is tried once: later
/// passes begin none, and the Start measures.
#[test]
fn a_refused_page_measurement_is_tried_once() {
    let dir = tempfile::tempdir().unwrap();
    let page = Arc::new(Page::default());
    let (mut b, path) = page_bench(
        dir.path(),
        "r2-tried",
        Decodes::default(),
        Spawner::refusing_owned(&[JobKind::Measure]),
        &page,
        Arc::new(crate::priority::Unchanged),
    );
    let read = loaded(dir.path(), &path, Speed::NORMAL);
    let start = practiced(dir.path(), &path, Speed::NORMAL);
    let hash = hash(dir.path(), &path);
    on_rig(
        b.sel.take().unwrap(),
        &Installers::default(),
        false,
        |rig| {
            rig.practice_read(&read, &mut game::Decision::off())
                .unwrap();
            passes_until_idle(rig);
            for _ in 0..3 {
                rig.poll_page().unwrap();
            }
            assert_eq!(page.count(PageAt::Measuring), 1, "the page tried again");
            assert_eq!(
                outcome(rig.load_chart(&start, &mut game::Decision::off())),
                Loading::Ready
            );
            assert_eq!(
                crate::loudness::analyses::of(hash),
                1,
                "measured by the Start"
            );
        },
    );
}

/// The page's read waits for the chart's background measurement before
/// its 1× bank begins (design.md "Load Pipeline", the release barrier):
/// held at its first block, the measurement is promoted by the wait, and
/// the page's decode has not begun while it is held, so the two banks are
/// never alive together; once it ended the read goes on and the page's
/// bank is made. The read serves the decision screen meanwhile.
#[test]
fn the_page_waits_for_the_charts_background_measurement() {
    use std::sync::atomic::Ordering;
    use std::time::Instant;
    let dir = tempfile::tempdir().unwrap();
    let page = Arc::new(Page::default());
    let (mut b, path) = plain(dir.path(), "pbg", &page);
    let s = b.sel.take().unwrap();
    let worker = crate::shelf::tests::worker(&s.shelf);
    worker.queue(crate::shelf::tests::job(&path));
    let wanted = worker.wanted(&path);
    assert_eq!(b.stepper.said(), crate::background::tests::Said::Block(0));
    let read = loaded(dir.path(), &path, Speed::NORMAL);
    let b = b;
    on_rig(s, &Installers::default(), false, |rig| {
        let got = std::thread::scope(|scope| {
            scope.spawn(|| {
                // The read waits for the measurement, held at its block: a
                // read that does not is found when the bound runs out, the
                // measurement then let go so nothing waits for it.
                let began = Instant::now();
                while !wanted() && began.elapsed() < watchdog() {
                    std::thread::park_timeout(std::time::Duration::from_millis(1));
                }
                let waited = wanted();
                let alone = page.seen.lock().unwrap().is_empty();
                if waited {
                    // Its check finds it promoted by the wait.
                    while b.mode.leaves.load(Ordering::SeqCst) == 0 {
                        b.stepper.step();
                        assert!(matches!(
                            b.stepper.said(),
                            crate::background::tests::Said::Block(_)
                        ));
                    }
                }
                b.stepper.finish();
                assert!(waited, "the page's read did not wait for the measurement");
                assert!(
                    alone,
                    "the page's decode began beside the background's bank"
                );
            });
            let now = bms_model::TimeSource::now(&rig.clock);
            let mut decision = game::Decision::begin(now, Some(game::DECISION_TIME), true);
            outcome(rig.practice_read(&read, &mut decision))
        });
        assert_eq!(got, Loading::Ready);
        passes_until_idle(rig);
        assert!(sel(rig).page.kept().is_some());
    });
}
