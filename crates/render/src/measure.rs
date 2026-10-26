//! Pure frame and latency measurement (D11): whole-play signed sample
//! histograms with their percentiles, DXGI frame statistics paired to frames by present count,
//! lost refreshes from the statistics' refresh counts, the table that
//! gives each judgment the display time of the first presented frame that
//! drew it, and the counting rules of design.md "Frame Pacing in Refreshes"
//! for a display capture.
//!
//! Missing or invalid evidence is never a value: every figure that may lack
//! it is an `Option`, `None` meaning N/A. Nothing here allocates after
//! construction.

use std::collections::VecDeque;

/// p50, p99 and max of a play's samples.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Summary {
    /// Every sample, exact.
    pub count: u64,
    /// The median (nearest rank), within [`RELATIVE_ERROR`].
    pub p50: i64,
    /// The 99th percentile (nearest rank), within [`RELATIVE_ERROR`].
    pub p99: i64,
    /// The largest sample, exact.
    pub max: i64,
}

/// Magnitudes below this are counted exactly, one bucket each.
const EXACT: i64 = 64;
/// Buckets per power of two above [`EXACT`].
const SUB_BITS: u32 = 5;
/// The powers of two with sub-buckets: 2^6 up to 2^27 µs (134 s).
const FIRST_OCTAVE: u32 = 6;
const LAST_OCTAVE: u32 = 26;
/// Buckets of one sign; the last holds magnitudes beyond 2^27 µs.
const BUCKETS: usize = EXACT as usize + ((LAST_OCTAVE - FIRST_OCTAVE + 1) << SUB_BITS) as usize + 1;

/// The largest error of a percentile against the exact one, relative to
/// it, for magnitudes up to 2^27 µs (134 s): half a sub-bucket of 1/32 of
/// its power of two. Larger ones share the last bucket and read as the
/// exact maximum (or minimum, for negative samples).
pub const RELATIVE_ERROR: f64 = 1.0 / 64.0;

/// A whole play's samples as a fixed log-linear histogram of each sign:
/// exact below 64 in magnitude, then 32 buckets per power of two up to
/// 134 s, so a percentile is within [`RELATIVE_ERROR`] of the exact one,
/// while the count, the maximum and the minimum are exact. Recording and
/// summarizing allocate nothing.
#[derive(Clone, Debug)]
pub struct Samples {
    /// Samples at or above 0, by magnitude.
    positive: Box<[u64; BUCKETS]>,
    /// Samples below 0, by magnitude.
    negative: Box<[u64; BUCKETS]>,
    count: u64,
    max: i64,
    min: i64,
}

impl Default for Samples {
    fn default() -> Self {
        Self::new()
    }
}

impl Samples {
    /// No samples.
    #[must_use]
    pub fn new() -> Self {
        Self {
            positive: Box::new([0; BUCKETS]),
            negative: Box::new([0; BUCKETS]),
            count: 0,
            max: i64::MIN,
            min: i64::MAX,
        }
    }

    /// Forgets every sample.
    pub fn clear(&mut self) {
        self.positive.fill(0);
        self.negative.fill(0);
        self.count = 0;
        self.max = i64::MIN;
        self.min = i64::MAX;
    }

    /// Adds one sample.
    pub fn push(&mut self, v: i64) {
        if v < 0 {
            self.negative[bucket(v.unsigned_abs())] += 1;
        } else {
            self.positive[bucket(v.unsigned_abs())] += 1;
        }
        self.count += 1;
        self.max = self.max.max(v);
        self.min = self.min.min(v);
    }

    /// Samples pushed.
    #[must_use]
    pub const fn count(&self) -> u64 {
        self.count
    }

    /// The summary, or `None` (N/A) without any sample.
    #[must_use]
    pub fn summary(&self) -> Option<Summary> {
        if self.count == 0 {
            return None;
        }
        Some(Summary {
            count: self.count,
            p50: self.percentile(50),
            p99: self.percentile(99),
            max: self.max,
        })
    }

    /// The `p`th percentile by nearest rank: the middle of the bucket that
    /// holds the sample of that rank, within the minimum and the maximum.
    /// Negative buckets come first, the largest magnitude first.
    fn percentile(&self, p: u64) -> i64 {
        let rank = (p * self.count).div_ceil(100).max(1);
        let mut seen = 0;
        let negative = self
            .negative
            .iter()
            .enumerate()
            .rev()
            .map(|(i, &n)| (n, -middle(i)));
        let positive = self
            .positive
            .iter()
            .enumerate()
            .map(|(i, &n)| (n, middle(i)));
        for (n, v) in negative.chain(positive) {
            seen += n;
            if seen >= rank {
                return v.clamp(self.min, self.max);
            }
        }
        self.max
    }
}

/// The bucket of magnitude `m`.
fn bucket(m: u64) -> usize {
    if m < EXACT as u64 {
        return m as usize;
    }
    let octave = 63 - m.leading_zeros();
    if octave > LAST_OCTAVE {
        return BUCKETS - 1;
    }
    let sub = (m >> (octave - SUB_BITS)) & ((1 << SUB_BITS) - 1);
    EXACT as usize + (((octave - FIRST_OCTAVE) << SUB_BITS) as usize) + sub as usize
}

/// The middle magnitude of bucket `i`; the last one has none and reads as
/// the largest magnitude (clamped to the exact extreme by the caller).
fn middle(i: usize) -> i64 {
    if i < EXACT as usize {
        return i as i64;
    }
    if i == BUCKETS - 1 {
        return i64::MAX;
    }
    let k = (i - EXACT as usize) as u32;
    let octave = FIRST_OCTAVE + (k >> SUB_BITS);
    let sub = i64::from(k & ((1 << SUB_BITS) - 1));
    let width = 1_i64 << (octave - SUB_BITS);
    (1_i64 << octave) + sub * width + width / 2
}

/// One `IDXGISwapChain::GetFrameStatistics` result, raw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameStatistics {
    /// The present count of the latest present that reached a vblank.
    pub present_count: u32,
    /// The vblank count when that present reached the screen.
    pub present_refresh_count: u32,
    /// The vblank count of the sync below.
    pub sync_refresh_count: u32,
    /// QPC ticks at that vblank.
    pub sync_qpc_time: i64,
}

/// A refresh count gap above this rejects a sample (spike
/// `window.rs`; D11).
pub const MAX_REFRESH_GAP: u32 = 4;

/// The display time of the sample's present in QPC ticks:
/// `SyncQPCTime − (SyncRefreshCount − PresentRefreshCount) × period`.
#[must_use]
pub fn display_qpc(s: &FrameStatistics, period_qpc: i64) -> i64 {
    let gap = s.sync_refresh_count.wrapping_sub(s.present_refresh_count);
    s.sync_qpc_time - i64::from(gap) * period_qpc
}

/// Keeps the frame statistics samples D11 accepts: the call succeeded,
/// `PresentCount` advanced since the last accepted sample, and the refresh
/// count gap is at most [`MAX_REFRESH_GAP`]. A sample with no sync yet
/// (`SyncQPCTime` 0, as a new swap chain's first samples on the
/// development PC read) is rejected too: its refresh counts are zeros,
/// not counts.
#[derive(Clone, Copy, Debug, Default)]
pub struct StatsFilter {
    last: Option<u32>,
}

impl StatsFilter {
    /// The sample if it is accepted; `read` is `None` when the call failed.
    pub fn accept(&mut self, read: Option<FrameStatistics>) -> Option<FrameStatistics> {
        let s = read?;
        let gap = s.sync_refresh_count.wrapping_sub(s.present_refresh_count);
        if self.last == Some(s.present_count) || gap > MAX_REFRESH_GAP || s.sync_qpc_time == 0 {
            return None;
        }
        self.last = Some(s.present_count);
        Some(s)
    }

    /// Forgets the last present count (a new swap chain counts from 0).
    pub fn reset(&mut self) {
        self.last = None;
    }
}

/// Lost refreshes from frame statistics (D11) at k = 1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct RefreshCount {
    /// Refresh opportunities lost over the counted intervals.
    pub lost: u64,
    /// Intervals counted: consecutive present counts both with accepted
    /// samples.
    pub counted: u64,
    /// Intervals without that evidence (N/A, never counted as lost).
    pub unavailable: u64,
    /// The longest single-interval loss.
    pub longest: u64,
}

impl RefreshCount {
    /// The lost refreshes, or `None` (N/A) when no interval was counted.
    #[must_use]
    pub const fn lost(&self) -> Option<u64> {
        if self.counted == 0 {
            None
        } else {
            Some(self.lost)
        }
    }
}

/// How the swap chain presents, as configured: the conditions under which
/// lost refreshes can be counted (design.md "Applicability").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Presentation {
    /// Vblanks per present: 1 for vsync on every refresh, 0 without vsync.
    pub sync_interval: u32,
    /// Whether presents may tear.
    pub tearing: bool,
}

impl Presentation {
    /// Whether refreshes can be counted at k = 1: vsync on every refresh
    /// and no tearing. A fixed refresh rate (no VRR) is checked per
    /// interval from the syncs instead.
    #[must_use]
    pub const fn countable(self) -> bool {
        self.sync_interval == 1 && !self.tearing
    }
}

/// Counts lost refreshes from accepted samples: an interval of `n`
/// refreshes between consecutive present counts loses `n − 1`
/// (design.md "Counting a displayed interval", k = 1). Intervals whose two
/// ends do not both have accepted samples are N/A, and so are intervals
/// without a verified fixed refresh grid: no refresh period known, or two
/// syncs whose times disagree with their refresh counts by more than a
/// tenth of a refresh (VRR, or a composed window).
#[derive(Clone, Copy, Debug, Default)]
pub struct RefreshLedger {
    /// The last accepted sample.
    last: Option<FrameStatistics>,
    count: RefreshCount,
}

impl RefreshLedger {
    /// Forgets everything.
    pub fn clear(&mut self) {
        *self = Self::default();
    }

    /// The figures so far.
    #[must_use]
    pub const fn count(&self) -> RefreshCount {
        self.count
    }

    /// One accepted sample, in the order they were taken; `period_qpc` is
    /// one refresh when the presentation is countable and the period known.
    pub fn accepted(&mut self, s: &FrameStatistics, period_qpc: Option<i64>) {
        if let Some(last) = self.last {
            let presents = s.present_count.wrapping_sub(last.present_count);
            let on_grid = period_qpc.is_some_and(|t| {
                let n = i64::from(s.sync_refresh_count.wrapping_sub(last.sync_refresh_count));
                n > 0 && (s.sync_qpc_time - last.sync_qpc_time - n * t).abs() <= t / 10
            });
            if presents == 1 && !on_grid {
                self.count.unavailable += 1;
            } else if presents == 1 {
                let n = s
                    .present_refresh_count
                    .wrapping_sub(last.present_refresh_count);
                let lost = u64::from(n.saturating_sub(1));
                self.count.counted += 1;
                self.count.lost += lost;
                self.count.longest = self.count.longest.max(lost);
            } else if presents <= u32::MAX / 2 {
                self.count.unavailable += u64::from(presents);
            } else {
                // The count went back: a new swap chain. The interval across
                // it has no evidence.
                self.count.unavailable += 1;
            }
        }
        self.last = Some(*s);
    }
}

/// What the table knows of one judgment.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OnScreenLatency {
    /// Not in the table (never drawn, or replaced by a later one).
    Unknown,
    /// Drawn, its frame's display time not known yet.
    Pending,
    /// Display time of the first presented frame that drew it minus its
    /// origin, µs.
    Shown(i64),
    /// Its frame has no accepted sample (N/A).
    Unavailable,
}

#[derive(Clone, Copy, Debug)]
struct Slot {
    seq: u64,
    origin: i64,
    /// The present count of the first presented frame that drew it.
    frame: Option<u32>,
    state: OnScreenLatency,
}

const EMPTY: Slot = Slot {
    seq: u64::MAX,
    origin: 0,
    frame: None,
    state: OnScreenLatency::Unknown,
};

/// The press → on screen table (D11), keyed by `JudgeEvent::seq`: each
/// judgment gets the present count of the first presented frame that drew
/// it, and then that frame's display time from its accepted frame
/// statistics sample, or N/A when it has none. Never predicted.
///
/// The table holds `capacity` judgments by `seq` modulo the capacity, in
/// the order they were drawn. A judgment still unresolved when its slot is
/// taken again, or when the order is full, is N/A.
#[derive(Clone, Debug)]
pub struct OnScreen {
    slots: Vec<Slot>,
    /// Unresolved judgments in drawing order: those of presented frames,
    /// then those drawn in frames not presented yet.
    order: VecDeque<u64>,
    latencies: Samples,
    unavailable: u64,
}

impl OnScreen {
    /// A table for `capacity` judgments in flight.
    #[must_use]
    pub fn with_capacity(capacity: usize) -> Self {
        let capacity = capacity.max(1);
        Self {
            slots: vec![EMPTY; capacity],
            order: VecDeque::with_capacity(capacity),
            latencies: Samples::new(),
            unavailable: 0,
        }
    }

    /// Forgets every judgment.
    pub fn clear(&mut self) {
        self.slots.fill(EMPTY);
        self.order.clear();
        self.latencies.clear();
        self.unavailable = 0;
    }

    fn slot(&self, seq: u64) -> usize {
        (seq % self.slots.len() as u64) as usize
    }

    /// Judgment `seq` with `origin` (µs) is drawn in the frame being built.
    pub fn drawn(&mut self, seq: u64, origin: i64) {
        let i = self.slot(seq);
        if self.slots[i].state == OnScreenLatency::Pending {
            self.unavailable += 1;
        }
        self.slots[i] = Slot {
            seq,
            origin,
            frame: None,
            state: OnScreenLatency::Pending,
        };
        if self.order.len() == self.order.capacity()
            && let Some(old) = self.order.pop_front()
        {
            self.give_up(old);
        }
        self.order.push_back(seq);
    }

    /// The frame built since the last call was presented with
    /// `present_count`: it is the first presented frame of what it drew.
    pub fn presented(&mut self, present_count: u32) {
        for k in (0..self.order.len()).rev() {
            let seq = self.order[k];
            let i = self.slot(seq);
            if self.slots[i].seq != seq {
                continue;
            }
            if self.slots[i].frame.is_some() {
                break;
            }
            self.slots[i].frame = Some(present_count);
        }
    }

    /// An accepted sample: present `present_count` was displayed at
    /// `display_us` (on the origins' timeline). Judgments of earlier
    /// frames still unresolved have no sample and are N/A.
    pub fn sample(&mut self, present_count: u32, display_us: i64) {
        while let Some(&seq) = self.order.front() {
            let i = self.slot(seq);
            let slot = self.slots[i];
            if slot.seq == seq && slot.state == OnScreenLatency::Pending {
                // Drawn but not presented yet: it waits, as everything after
                // it.
                let Some(frame) = slot.frame else { return };
                let ahead = present_count.wrapping_sub(frame);
                if ahead == 0 {
                    let latency = display_us - slot.origin;
                    self.slots[i].state = OnScreenLatency::Shown(latency);
                    self.latencies.push(latency);
                } else if ahead <= u32::MAX / 2 {
                    self.give_up(seq);
                } else {
                    // The sample is older than this frame: it waits.
                    return;
                }
            }
            self.order.pop_front();
        }
    }

    /// The frame built since the last call failed, or was presented
    /// without a present count to pair it with: what it drew is N/A, while
    /// earlier frames keep their pairings.
    pub fn presented_unknown(&mut self) {
        for k in (0..self.order.len()).rev() {
            let seq = self.order[k];
            let i = self.slot(seq);
            if self.slots[i].seq != seq {
                continue;
            }
            if self.slots[i].frame.is_some() {
                break;
            }
            self.give_up(seq);
        }
    }

    /// Everything not resolved yet is N/A (the play's measurement ended or
    /// the swap chain was replaced).
    pub fn finish(&mut self) {
        while let Some(seq) = self.order.pop_front() {
            self.give_up(seq);
        }
    }

    fn give_up(&mut self, seq: u64) {
        let i = self.slot(seq);
        if self.slots[i].seq == seq && self.slots[i].state == OnScreenLatency::Pending {
            self.slots[i].state = OnScreenLatency::Unavailable;
            self.unavailable += 1;
        }
    }

    /// What the table knows of judgment `seq`.
    #[must_use]
    pub fn latency(&self, seq: u64) -> OnScreenLatency {
        let slot = self.slots[self.slot(seq)];
        if slot.seq == seq {
            slot.state
        } else {
            OnScreenLatency::Unknown
        }
    }

    /// The resolved latencies.
    pub fn latencies(&mut self) -> &mut Samples {
        &mut self.latencies
    }

    /// Judgments whose frame had no accepted sample.
    #[must_use]
    pub const fn unavailable(&self) -> u64 {
        self.unavailable
    }
}

/// The timestamp tolerance of an off-grid interval, ns (design.md
/// "Off-grid intervals").
pub const OFF_GRID_TOLERANCE_NS: i64 = 500_000;

/// The figures of one span counted from a display capture (design.md
/// "Frame Pacing in Refreshes").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SpanCount {
    /// Refresh opportunities lost inside the span, off-grid intervals
    /// included.
    pub lost: u64,
    /// Expected opportunities: the target grid anchored at the
    /// predecessor, inside the span.
    pub expected: u64,
    /// The longest single-interval loss inside the span.
    pub longest: u64,
    /// Off-grid intervals whose later display time lies in the span.
    pub off_grid: u64,
}

/// Counts the span `[a, b)` (ns) over the display times of displayed
/// frames `displayed` (ns, ascending) at refresh period `period_ns` and
/// target `k` refreshes. `None` (N/A, never 0) when the capture lacks the
/// span's predecessor (the last displayed frame before `a`) or successor
/// (the first at or after `b`), or when `covered` says it lacks a present
/// the game issued in the span.
#[must_use]
pub fn count_span(
    displayed: &[i64],
    (a, b): (i64, i64),
    period_ns: i64,
    k: u32,
    covered: bool,
) -> Option<SpanCount> {
    if !covered || period_ns <= 0 || k == 0 || a >= b {
        return None;
    }
    let pred = displayed.iter().rposition(|&d| d < a)?;
    let succ = displayed.iter().position(|&d| d >= b)?;
    let step = period_ns * i64::from(k);
    let p = displayed[pred];
    // Grid points p + j·step inside [a, b): j from ceil((a − p) / step) to
    // ceil((b − p) / step) − 1.
    let first = (a - p + step - 1).div_euclid(step);
    let past = (b - p + step - 1).div_euclid(step);
    let mut out = SpanCount {
        lost: 0,
        expected: u64::try_from(past - first).unwrap_or(0),
        longest: 0,
        off_grid: 0,
    };
    for w in displayed[pred..=succ].windows(2) {
        let (d0, d1) = (w[0], w[1]);
        let delta = d1 - d0;
        // n = floor(Δ / T + 1/2), a half rounding up.
        let n = (2 * delta + period_ns).div_euclid(2 * period_ns);
        let lost_here = if n > i64::from(k) {
            n.div_euclid(i64::from(k)) + i64::from(n % i64::from(k) != 0) - 1
        } else {
            0
        };
        let inside = (1..=lost_here)
            .filter(|j| (a..b).contains(&(d0 + j * step)))
            .count() as u64;
        let mut interval = inside;
        if (delta - n * period_ns).abs() > OFF_GRID_TOLERANCE_NS && (a..b).contains(&d1) {
            out.off_grid += 1;
            interval += 1;
        }
        out.lost += interval;
        out.longest = out.longest.max(interval);
    }
    Some(out)
}
