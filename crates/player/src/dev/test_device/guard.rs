//! The guard (docs/test-device.md "Run Guard"): while a row's lease lasts,
//! every lease the runner holds is a slice of [`SLICE_MS`], granted and
//! renewed only after a good check, every [`POLL`]: the run's window in
//! front, its client area unchanged and inside the run's monitor (the
//! monitor read again at least every [`MONITOR_EVERY`]), the recorder's
//! process running, and the step thread's heartbeat no older than a third
//! of the slice, with no longer gap between two beats since the guard
//! started ([`Beat`] latches each gap as the step thread closes it, so a
//! gap no look saw still counts). Each grant names
//! the connection's token and each grant and renewal carries the deadline
//! that check bounds ([`clock::deadline`]); with no round trip on the
//! connection, nothing is granted. A bad check, or a command the link's
//! queue refuses, stops holds for good.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicI64, Ordering};
use std::thread::JoinHandle;
use std::time::Duration;

use test_device::clock::{self, Ping, ticks_to_us};
use test_device::protocol::{Command, Token};

use super::link::Grant;
use super::release::{Refused, State, Writer};

/// The length of every lease the runner grants.
pub const SLICE_MS: u32 = 250;
/// How often a held lease is renewed: a third of the slice.
pub const RENEW_EVERY_US: i64 = SLICE_MS as i64 * 1000 / 3;
/// How old the step thread's heartbeat may be: a third of the slice.
pub const BEAT_MAX_US: i64 = SLICE_MS as i64 * 1000 / 3;
/// How often the guard looks.
pub const POLL: Duration = Duration::from_millis(5);
/// How often the run's monitor is enumerated again.
pub const MONITOR_EVERY: Duration = Duration::from_millis(50);

/// The step thread's heartbeat: when it last beat, and the longest gap
/// between two beats since [`Beat::arm`], latched by the beat that closes
/// it. A look that reads the heartbeat only after the step thread beat
/// again still sees the gap.
#[derive(Debug)]
pub struct Beat {
    last: AtomicI64,
    worst: AtomicI64,
}

impl Beat {
    /// A heartbeat that beat at `now` (host counter ticks).
    pub const fn new(now: i64) -> Self {
        Self {
            last: AtomicI64::new(now),
            worst: AtomicI64::new(0),
        }
    }

    /// A beat at `now`: the gap it closes is latched.
    pub fn beat(&self, now: i64) {
        let before = self.last.swap(now, Ordering::AcqRel);
        self.worst
            .fetch_max(now.saturating_sub(before), Ordering::AcqRel);
    }

    /// Beats at `now` and forgets the gaps before: the guard's start, so a
    /// step that blocked before any lease (a rediscovery, the player's
    /// start) does not count.
    pub fn arm(&self, now: i64) {
        self.last.store(now, Ordering::Release);
        self.worst.store(0, Ordering::Release);
    }

    /// The heartbeat's age (ticks): the gap still open at `now()`, read
    /// after the last beat is, or the longest one closed since the guard
    /// armed it, whichever is longer.
    pub fn age(&self, now: impl FnOnce() -> i64) -> i64 {
        let last = self.last.load(Ordering::Acquire);
        let open = now().saturating_sub(last);
        open.max(self.worst.load(Ordering::Acquire))
    }
}

/// One look at the run, read fresh.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Look {
    /// When it was read (host counter ticks).
    pub at: i64,
    /// The run's window is in front.
    pub front: bool,
    /// The window's client area is the one it had, inside the run's
    /// monitor, which has the area it had.
    pub placed: bool,
    /// The step thread's heartbeat's age (ticks, [`Beat::age`]), read
    /// after the look's other reads.
    pub beat_age: i64,
    /// The recorder's process runs: with it gone, nothing would show a
    /// held input reaching another window.
    pub recording: bool,
}

impl Look {
    /// Why the look is bad, if it is.
    pub fn bad(&self, freq: i64) -> Option<String> {
        if !self.front {
            return Some("the run's window is not in front".into());
        }
        if !self.placed {
            return Some("the run's window or monitor moved".into());
        }
        if !self.recording {
            return Some("the recorder is not running".into());
        }
        let age = ticks_to_us(self.beat_age, freq);
        if age > BEAT_MAX_US {
            return Some(format!("the step thread's heartbeat is {age} µs old"));
        }
        None
    }
}

/// What the link has: the connection's token, its grant and round trips.
#[derive(Clone, Debug, Default)]
pub struct View {
    pub token: Option<Token>,
    pub grant: Option<Grant>,
    pub pings: Vec<Ping>,
    /// Holds have stopped.
    pub stopped: bool,
}

impl View {
    pub fn of(s: &State, stopped: bool) -> Self {
        Self {
            token: s.token,
            grant: s.grant,
            pings: s.pings.clone(),
            stopped,
        }
    }
}

/// What the guard does after a look.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Act {
    Idle,
    /// Queue this grant or renewal, derived from the look at `check`.
    Send {
        cmd: Command,
        check: i64,
    },
    /// Stop holds, for this cause.
    Stop(String),
}

/// The guard's state between looks.
#[derive(Clone, Debug)]
pub struct Guard {
    freq: i64,
    /// Until when (host counter ticks) the row's lease lasts.
    until: Option<i64>,
    /// The check the last grant or renewal queued came from.
    last: Option<i64>,
    lost: bool,
    /// The oldest heartbeat a judged look read (µs), and how many looks
    /// were judged: the measured margin under [`BEAT_MAX_US`].
    beat_max_us: i64,
    judged: u64,
}

impl Guard {
    pub const fn new(freq: i64) -> Self {
        Self {
            freq,
            until: None,
            last: None,
            lost: false,
            beat_max_us: 0,
            judged: 0,
        }
    }

    /// The oldest heartbeat a judged look read (µs) and the number of
    /// looks judged.
    pub const fn heartbeat(&self) -> (i64, u64) {
        (self.beat_max_us, self.judged)
    }

    /// The row asks for a lease lasting until `until` (host counter ticks).
    pub const fn want(&mut self, until: i64) {
        self.until = Some(until);
    }

    /// Whether the row's lease still lasts at `at` (the tests' stand-ins
    /// read it).
    #[cfg(test)]
    pub fn wanted(&self, at: i64) -> bool {
        !self.lost && self.until.is_some_and(|u| at < u)
    }

    /// Decides after one look.
    pub fn decide(&mut self, look: &Look, view: &View) -> Act {
        if self.lost || view.stopped {
            self.lost = true;
            return Act::Idle;
        }
        let Some(until) = self.until else {
            return Act::Idle;
        };
        self.judged += 1;
        self.beat_max_us = self.beat_max_us.max(ticks_to_us(look.beat_age, self.freq));
        if let Some(why) = look.bad(self.freq) {
            self.lost = true;
            return Act::Stop(why);
        }
        if look.at >= until {
            self.until = None;
            return Act::Idle;
        }
        let slice_ticks = i64::from(SLICE_MS) * self.freq / 1000;
        let renew_ticks = RENEW_EVERY_US * self.freq / 1_000_000;
        let since = self.last.map(|l| look.at - l);
        let checked = ticks_to_us(look.at, self.freq);
        let cmd = match (view.grant, since) {
            // Asked, not answered yet: the asked slice bounds the wait.
            (None, Some(s)) if s < slice_ticks => return Act::Idle,
            (None, Some(_)) => {
                self.lost = true;
                return Act::Stop("no grant within a slice of its request".into());
            }
            (None, None) => {
                let Some(token) = view.token else {
                    self.lost = true;
                    return Act::Stop("the connection has no token".into());
                };
                let Some(deadline) = clock::deadline(&view.pings, checked, SLICE_MS.into()) else {
                    // No round trip on this connection yet: wait for one.
                    return Act::Idle;
                };
                Command::Lease {
                    token,
                    ms: SLICE_MS,
                    deadline,
                }
            }
            (Some(_), Some(s)) if s < renew_ticks => return Act::Idle,
            (Some(g), _) => {
                let Some(deadline) = clock::deadline(&view.pings, checked, SLICE_MS.into()) else {
                    self.lost = true;
                    return Act::Stop("the connection's round trips no longer fit".into());
                };
                Command::Renew {
                    id: g.id(),
                    deadline,
                }
            }
        };
        self.last = Some(look.at);
        Act::Send {
            cmd,
            check: look.at,
        }
    }
}

/// What the guard reads of the run.
pub trait Watch: Send {
    fn look(&mut self) -> Look;
}

/// Where the guard sends: the link thread.
pub trait Sink: Send + Sync {
    fn enqueue(&self, cmd: Command, check: Option<i64>) -> Result<(), Refused>;
    fn stop_holds(&self, cause: &str);
    fn view(&self) -> View;
}

impl Sink for Writer {
    fn enqueue(&self, cmd: Command, check: Option<i64>) -> Result<(), Refused> {
        Self::enqueue(self, cmd, check)
    }
    fn stop_holds(&self, cause: &str) {
        Self::stop_holds(self, cause);
    }
    fn view(&self) -> View {
        let stopped = self.stopped().is_some();
        self.with_state(|s| View::of(s, stopped))
    }
}

/// The guard thread's handle: the row's lease and the thread's end.
#[derive(Clone)]
pub struct Handle {
    guard: Arc<std::sync::Mutex<Guard>>,
    end: Arc<AtomicBool>,
}

impl Handle {
    /// The row asks for a lease lasting until `until` (host counter ticks).
    pub fn want(&self, until: i64) {
        self.lock().want(until);
    }

    pub fn end(&self) {
        self.end.store(true, Ordering::Release);
    }

    /// [`Guard::heartbeat`].
    pub fn heartbeat(&self) -> (i64, u64) {
        self.lock().heartbeat()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Guard> {
        self.guard
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// One guard step: look, decide, act.
pub fn step(guard: &mut Guard, watch: &mut dyn Watch, sink: &dyn Sink) {
    let look = watch.look();
    match guard.decide(&look, &sink.view()) {
        Act::Idle => {}
        Act::Send { cmd, check } => match sink.enqueue(cmd, Some(check)) {
            Ok(()) => {}
            Err(Refused::Full) => {
                guard.lost = true;
                sink.stop_holds("the link's queue is full");
            }
            Err(Refused::Closed) => guard.lost = true,
        },
        Act::Stop(why) => sink.stop_holds(&format!("a lost check: {why}")),
    }
}

/// Ends the guard's thread, waits for it up to `wait`, and only then reads
/// its heartbeat statistics ([`Guard::heartbeat`]): a thread blocked in a
/// look holds the guard's lock, so a read before the join could outlast
/// the wait.
///
/// # Errors
///
/// The thread did not end within `wait`: no statistics are read.
pub fn stop(h: &Handle, thread: JoinHandle<()>, wait: Duration) -> Result<(i64, u64), String> {
    h.end();
    super::owner::join_within(thread, "the guard's thread", wait)?;
    Ok(h.heartbeat())
}

/// Starts the guard thread.
pub fn start(
    freq: i64,
    mut watch: Box<dyn Watch>,
    sink: Arc<dyn Sink>,
) -> (Handle, JoinHandle<()>) {
    let h = Handle {
        guard: Arc::new(std::sync::Mutex::new(Guard::new(freq))),
        end: Arc::new(AtomicBool::new(false)),
    };
    let h2 = h.clone();
    let thread = std::thread::spawn(move || {
        while !h2.end.load(Ordering::Acquire) {
            step(&mut h2.lock(), watch.as_mut(), sink.as_ref());
            std::thread::sleep(POLL);
        }
    });
    (h, thread)
}

#[cfg(test)]
mod tests {
    use std::sync::Mutex;

    use test_device::protocol::LeaseId;

    use super::*;

    /// A counter of 1 MHz: ticks are µs.
    const FREQ: i64 = 1_000_000;
    const TOKEN: Token = Token {
        boot: 0x0123_4567_89ab_cdef,
        count: 3,
    };
    const OTHER: Token = Token {
        boot: 0x0123_4567_89ab_cdef,
        count: 4,
    };

    fn pings() -> Vec<Ping> {
        (0..4)
            .map(|k| Ping {
                sent: k * 1000 - 50,
                device: 9_000_000 + u64::try_from(k * 1000).unwrap(),
                received: k * 1000 + 50,
            })
            .collect()
    }

    fn view(grant: bool) -> View {
        View {
            token: Some(TOKEN),
            grant: grant.then(|| Grant::test(LeaseId { token: TOKEN, n: 1 }, 1)),
            pings: pings(),
            stopped: false,
        }
    }

    fn good(at: i64) -> Look {
        Look {
            at,
            front: true,
            placed: true,
            beat_age: 0,
            recording: true,
        }
    }

    fn wanting() -> Guard {
        let mut g = Guard::new(FREQ);
        g.want(10_000_000);
        g
    }

    #[test]
    fn no_renewal_after_a_loss() {
        let mut g = wanting();
        assert!(matches!(
            g.decide(&good(5_000), &view(false)),
            Act::Send { .. }
        ));
        let mut bad = good(100_000);
        bad.front = false;
        assert!(matches!(g.decide(&bad, &view(true)), Act::Stop(_)));
        for at in (200_000..2_000_000).step_by(10_000) {
            assert_eq!(g.decide(&good(at), &view(true)), Act::Idle);
            assert_eq!(g.decide(&good(at), &view(false)), Act::Idle);
        }
    }

    /// Over every look a check can make, a grant or renewal is sent only
    /// after a good one, derived from it.
    #[test]
    fn a_grant_or_renewal_follows_a_good_check_only() {
        let mut sent = 0;
        for (front, recording) in [(false, true), (true, false), (true, true)] {
            for placed in [false, true] {
                for age in [0, BEAT_MAX_US, BEAT_MAX_US + 1, 1_000_000] {
                    for grant in [false, true] {
                        let mut g = wanting();
                        let look = Look {
                            at: 2_000_000,
                            front,
                            placed,
                            beat_age: age,
                            recording,
                        };
                        let ok = front && placed && recording && age <= BEAT_MAX_US;
                        match g.decide(&look, &view(grant)) {
                            Act::Send { check, .. } => {
                                assert!(ok, "{look:?}");
                                assert_eq!(check, look.at);
                                sent += 1;
                            }
                            Act::Stop(_) => assert!(!ok, "{look:?}"),
                            Act::Idle => panic!("{look:?}: neither sent nor stopped"),
                        }
                    }
                }
            }
        }
        // Positive control: the good looks did send.
        assert_eq!(sent, 2 * 2);
    }

    #[test]
    fn every_deadline_is_bound_by_its_check() {
        for grant in [false, true] {
            let mut g = wanting();
            let look = good(3_000_000);
            let Act::Send { cmd, check } = g.decide(&look, &view(grant)) else {
                panic!()
            };
            let want = clock::deadline(&pings(), check, SLICE_MS.into()).unwrap();
            let deadline = match cmd {
                Command::Lease { deadline, ms, .. } => {
                    assert_eq!(ms, SLICE_MS);
                    deadline
                }
                Command::Renew { deadline, .. } => deadline,
                c => panic!("{c}"),
            };
            assert_eq!(deadline, want);
            // A later check gives a later deadline: it is the check's.
            let mut g = wanting();
            let Act::Send { cmd: later, .. } = g.decide(&good(3_500_000), &view(grant)) else {
                panic!()
            };
            assert_ne!(later, cmd);
        }
    }

    #[test]
    fn every_grant_names_the_connection_s_token() {
        for token in [TOKEN, OTHER] {
            let mut g = wanting();
            let v = View {
                token: Some(token),
                ..view(false)
            };
            let Act::Send { cmd, .. } = g.decide(&good(1_000), &v) else {
                panic!()
            };
            assert!(
                matches!(cmd, Command::Lease { token: t, .. } if t == token),
                "{cmd}"
            );
        }
        let mut g = wanting();
        let v = View {
            token: None,
            ..view(false)
        };
        assert!(matches!(g.decide(&good(1_000), &v), Act::Stop(_)));
    }

    #[test]
    fn no_grant_without_a_round_trip_on_this_connection() {
        let mut g = wanting();
        let v = View {
            pings: Vec::new(),
            ..view(false)
        };
        for at in (0..1_000_000).step_by(5_000) {
            assert_eq!(g.decide(&good(at), &v), Act::Idle);
        }
        // Once a round trip is there, the grant follows.
        assert!(matches!(
            g.decide(&good(1_000_000), &view(false)),
            Act::Send { .. }
        ));
    }

    #[test]
    fn a_stale_heartbeat_stops_the_renewals() {
        let mut g = wanting();
        assert!(matches!(g.decide(&good(0), &view(false)), Act::Send { .. }));
        let mut at = 0;
        let mut renewals = 0;
        // The step thread beats until 500 ms, then stalls.
        loop {
            at += 5_000;
            let look = Look {
                beat_age: at - at.min(500_000),
                ..good(at)
            };
            match g.decide(&look, &view(true)) {
                Act::Send { .. } => renewals += 1,
                Act::Stop(why) => {
                    assert!(why.contains("heartbeat"), "{why}");
                    break;
                }
                Act::Idle => {}
            }
            assert!(at < 2_000_000, "never stopped");
        }
        assert_eq!(
            at,
            500_000 + BEAT_MAX_US + 5_000 - (BEAT_MAX_US + 5_000) % 5_000
        );
        assert!(renewals >= 5, "the beating stretch renewed: {renewals}");
        // The guard kept the oldest heartbeat it judged: the stopping
        // look's, and one look per 5 ms until then.
        assert_eq!(
            g.heartbeat(),
            (at - 500_000, 1 + u64::try_from(at / 5_000).unwrap())
        );
    }

    struct Full {
        stops: Mutex<Vec<String>>,
    }

    impl Sink for Full {
        fn enqueue(&self, _: Command, _: Option<i64>) -> Result<(), Refused> {
            Err(Refused::Full)
        }
        fn stop_holds(&self, cause: &str) {
            self.stops.lock().unwrap().push(cause.to_owned());
        }
        fn view(&self) -> View {
            view(false)
        }
    }

    struct Steady(i64);

    impl Watch for Steady {
        fn look(&mut self) -> Look {
            self.0 += 5_000;
            good(self.0)
        }
    }

    /// A gap the step thread closed before the guard looked still stops
    /// holds: the step thread stalls past the bound while the guard is
    /// descheduled, beats again, and only then does the guard look, its
    /// counter sampled first. The two threads meet at each point of that
    /// order. The control: a gap within the bound sends the renewal.
    #[test]
    fn a_gap_closed_before_the_look_still_stops_holds() {
        use std::sync::Barrier;
        for gap in [BEAT_MAX_US + 1, BEAT_MAX_US] {
            let beat = Arc::new(Beat::new(0));
            let meet = Arc::new(Barrier::new(2));
            let (b2, m2) = (Arc::clone(&beat), Arc::clone(&meet));
            let step = std::thread::spawn(move || {
                // The stall ends with this beat, after the guard sampled
                // its counter.
                m2.wait();
                b2.beat(gap);
                m2.wait();
            });
            let mut g = wanting();
            // The look: the counter first, then (after the beat) the
            // heartbeat.
            let at = gap - 1;
            meet.wait();
            meet.wait();
            let look = Look {
                beat_age: beat.age(|| at),
                ..good(at)
            };
            step.join().unwrap();
            match g.decide(&look, &view(true)) {
                Act::Stop(why) => {
                    assert!(gap > BEAT_MAX_US, "{why}");
                    assert!(why.contains("heartbeat"), "{why}");
                }
                Act::Send { .. } => assert!(gap <= BEAT_MAX_US, "{look:?}"),
                Act::Idle => panic!("{look:?}"),
            }
        }
        // Arming forgets a gap before the guard's start.
        let beat = Beat::new(0);
        beat.beat(1_000_000);
        beat.arm(1_000_000);
        assert_eq!(beat.age(|| 1_000_010), 10);
    }

    /// A guard thread blocked in a look holds the guard's lock: ending it
    /// returns within its wait, with no statistics, instead of waiting on
    /// that lock. The look and the stop meet at a point the test holds.
    #[test]
    fn stopping_a_blocked_guard_returns_within_its_wait() {
        use std::sync::mpsc;
        struct Stuck {
            entered: mpsc::Sender<()>,
            release: Arc<std::sync::Barrier>,
            n: i64,
        }
        impl Watch for Stuck {
            fn look(&mut self) -> Look {
                self.n += 5_000;
                if self.n == 10_000 {
                    let _ = self.entered.send(());
                    self.release.wait();
                }
                good(self.n)
            }
        }
        let (entered, looked) = mpsc::channel();
        let release = Arc::new(std::sync::Barrier::new(2));
        let (h, thread) = start(
            FREQ,
            Box::new(Stuck {
                entered,
                release: Arc::clone(&release),
                n: 0,
            }),
            Arc::new(Full {
                stops: Mutex::new(Vec::new()),
            }),
        );
        looked.recv_timeout(Duration::from_secs(5)).unwrap();
        let (done, stopped) = mpsc::channel();
        let h2 = h.clone();
        let stopper = std::thread::spawn(move || {
            let _ = done.send(stop(&h2, thread, Duration::from_millis(100)));
        });
        let r = stopped.recv_timeout(Duration::from_secs(2));
        release.wait();
        stopper.join().unwrap();
        let r = r.expect("the stop waited on the guard's lock past its wait");
        assert!(r.is_err(), "{r:?}");
        // The control: a guard not blocked stops with its statistics.
        let (h, thread) = start(
            FREQ,
            Box::new(Steady(0)),
            Arc::new(Full {
                stops: Mutex::new(Vec::new()),
            }),
        );
        assert!(stop(&h, thread, Duration::from_secs(2)).is_ok());
    }

    #[test]
    fn a_full_link_queue_is_a_loss() {
        let sink = Full {
            stops: Mutex::new(Vec::new()),
        };
        let mut g = wanting();
        let mut w = Steady(0);
        step(&mut g, &mut w, &sink);
        assert_eq!(*sink.stops.lock().unwrap(), ["the link's queue is full"]);
        // Nothing more is sent.
        for _ in 0..100 {
            step(&mut g, &mut w, &sink);
        }
        assert_eq!(sink.stops.lock().unwrap().len(), 1);
        assert!(!g.wanted(w.0));
    }
}
