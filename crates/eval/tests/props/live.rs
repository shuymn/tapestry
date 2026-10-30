//! A model of live evaluation driven by a delivery schedule: producers that
//! push into bounded queues and publish what they have queued, and the logic
//! thread's consumer step of design.md "Watermark", built from the crate's
//! public pieces.
//!
//! The consumer mirrors `game::Logic::advance_to` (crates/game/src/logic.rs)
//! step for step; `eval` may not depend on `game` (design.md "Licenses"), so
//! the model lives here. Each step of the model cites the step of that consumer
//! it mirrors.

use std::collections::VecDeque;

use bms_model::{Chart, TimeUs};
use eval::{
    EvalInput, Evaluator, GaugeState, InputKind, JudgeEvent, PhysicalInputEvent, Published,
    SourceId, SourceState, Staged, Tally, evaluation_time, merge, watermark,
};
use rules::RulePreset;

/// Where an open source publishes, within what it may publish: at least
/// its previous publication, below the receive time of its first event not
/// yet pushed, and at most the terminal watermark plus its offset.
#[derive(Clone, Copy, Debug)]
pub enum Publish {
    /// Keep the previous publication.
    Hold,
    /// The receive time of the last event pushed (W lands on input times).
    LastPushed,
    /// The highest allowed.
    Highest,
    /// A fraction (of `u16::MAX`) of the way from the previous publication
    /// to the highest allowed.
    Fraction(u16),
    /// The publication whose watermark contribution is this evaluation
    /// time (W lands on chosen deadlines and inputs).
    Eval(TimeUs),
}

/// What one source does in one step of the schedule.
#[derive(Clone, Copy, Debug)]
pub struct SourceStep {
    /// Events pushed before publishing, at most what the queue has room
    /// for.
    pub push: usize,
    /// Events pushed after the consumer read the publications and before
    /// it pops: they are beyond every publication it read.
    pub late_push: usize,
    /// Where the source publishes.
    pub publish: Publish,
    /// Move a pending disconnect one transition on: the old publication,
    /// then open at the disconnect's time, then closed there.
    pub close: bool,
}

/// A delivery schedule for one play.
#[derive(Clone, Debug)]
pub struct Delivery {
    /// Each source's producer queue capacity (in play, 4,096).
    pub capacity: Vec<usize>,
    /// The generated steps; source `i` takes `step[i % step.len()]`, and an
    /// empty step only wakes the consumer.
    pub steps: Vec<Vec<SourceStep>>,
    /// The order of the staged buffers in the slice passed to `merge`: a
    /// permutation of the source indices.
    pub staged_order: Vec<usize>,
    /// The common terminal watermark T.
    pub terminal: TimeUs,
}

/// Counts of the schedule's fragile spots, for the coverage test.
#[derive(Clone, Copy, Debug, Default)]
pub struct Stats {
    /// Pushes that left a producer queue at its capacity.
    pub queue_full: usize,
    /// Events popped beyond the publication read (they stay staged).
    pub popped_beyond: usize,
    /// Wakes that popped nothing from a source because its last popped
    /// event is beyond its publication.
    pub held_back: usize,
    /// Events pushed between the consumer's read and its pops.
    pub late_pushed: usize,
    /// Disconnects pushed.
    pub disconnects: usize,
    /// Reads of an open publication at a pending disconnect's time.
    pub open_at_disconnect: usize,
    /// Sources retired.
    pub retirements: usize,
    /// Inputs merged whose evaluation time equals the step's W.
    pub inputs_at_w: usize,
}

/// The result of a live run.
#[derive(Clone, Debug)]
pub struct Live {
    /// Every event, in output order.
    pub events: Vec<JudgeEvent>,
    /// The tally at the end.
    pub tally: Tally,
    /// The gauges at the end.
    pub gauges: Vec<GaugeState>,
    /// Whether evaluation completed.
    pub complete: bool,
    /// The watermark of each consumer step.
    pub watermarks: Vec<TimeUs>,
    /// The number of events emitted after each consumer step.
    pub step_ends: Vec<usize>,
    /// Fragile spots reached.
    pub stats: Stats,
}

/// A pending disconnect's publication, in the order it goes through.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Closure {
    /// No disconnect pushed.
    None,
    /// The disconnect at this receive time is queued; the old publication
    /// stands.
    Pending(TimeUs),
    /// Published open at the disconnect's time.
    OpenAt(TimeUs),
    /// Published closed at the disconnect's time.
    Closed,
}

/// One source's producer: its log, its queue and its publication.
struct Producer {
    log: Vec<PhysicalInputEvent>,
    next: usize,
    queue: VecDeque<PhysicalInputEvent>,
    capacity: usize,
    publication: Published,
    closure: Closure,
    offset: TimeUs,
}

impl Producer {
    /// Pushes up to `k` events, within the queue's capacity, and returns how
    /// many it pushed. A disconnect is the source's last event.
    fn push(&mut self, k: usize, stats: &mut Stats) -> usize {
        let mut pushed = 0;
        while pushed < k && self.queue.len() < self.capacity && self.closure == Closure::None {
            let Some(&e) = self.log.get(self.next) else {
                break;
            };
            self.queue.push_back(e);
            self.next += 1;
            pushed += 1;
            if self.queue.len() == self.capacity {
                stats.queue_full += 1;
            }
            if e.kind == InputKind::Disconnected {
                stats.disconnects += 1;
                self.closure = Closure::Pending(e.received);
            }
        }
        pushed
    }

    /// The highest publication allowed: below the first event not pushed
    /// (every event received at or before a publication is queued), and at
    /// most `terminal` plus the offset.
    fn highest(&self, terminal: TimeUs) -> i64 {
        let cap = terminal.0.saturating_add(self.offset.0);
        self.log
            .get(self.next)
            .map_or(cap, |e| cap.min(e.received.0.saturating_sub(1)))
    }

    /// An ordinary publication; a source with a pending disconnect only
    /// goes through its closure.
    fn publish(&mut self, choice: Publish, terminal: TimeUs) -> Result<(), String> {
        if self.closure != Closure::None {
            return Ok(());
        }
        let lo = self.publication.received_up_to.0;
        let hi = self.highest(terminal);
        if hi <= lo {
            return Ok(());
        }
        let target = match choice {
            Publish::Hold => return Ok(()),
            Publish::Highest => hi,
            Publish::LastPushed => match self.next.checked_sub(1).and_then(|i| self.log.get(i)) {
                Some(e) => e.received.0,
                None => return Ok(()),
            },
            Publish::Fraction(f) => {
                // From the start of the log rather than from i64::MIN.
                let floor = self.log.first().map_or(hi, |e| e.received.0 - 1);
                let from = lo.max(floor.min(hi));
                let span = i128::from(hi) - i128::from(from);
                let step = span * i128::from(f) / i128::from(u16::MAX);
                i64::try_from(i128::from(from) + step).unwrap()
            }
            Publish::Eval(t) => t.0.saturating_add(self.offset.0),
        };
        self.set(Published::open(TimeUs(target.clamp(lo, hi))))
    }

    /// The next closure transition, if a disconnect is pending.
    fn advance_closure(&mut self) -> Result<(), String> {
        match self.closure {
            Closure::None | Closure::Closed => Ok(()),
            Closure::Pending(t) => {
                self.closure = Closure::OpenAt(t);
                self.set(Published::open(t))
            }
            Closure::OpenAt(t) => {
                self.closure = Closure::Closed;
                self.set(Published::closed(t))
            }
        }
    }

    /// Replaces the publication, which only grows.
    fn set(&mut self, p: Published) -> Result<(), String> {
        if p.received_up_to < self.publication.received_up_to {
            return Err(format!(
                "publication moved back: {:?} after {:?}",
                p, self.publication
            ));
        }
        self.publication = p;
        Ok(())
    }
}

/// The consumer's state: what `Logic` keeps between steps.
struct Consumer<'a> {
    ev: Evaluator<'a>,
    device: &'a eval::DeviceProfile,
    /// The last publication read per source.
    published: Vec<Published>,
    /// Staged buffers, in the slice order `merge` gets.
    staged: Vec<Staged>,
    /// Each source's position in `staged`.
    position: Vec<usize>,
    /// The receive time of each source's last popped event (logic.rs
    /// `Source::last_popped`), kept across steps.
    last_popped: Vec<Option<TimeUs>>,
    /// Whether the last pop of each source found its queue empty.
    drained: Vec<bool>,
    merged: Vec<PhysicalInputEvent>,
    out: Vec<JudgeEvent>,
    watermarks: Vec<TimeUs>,
    step_ends: Vec<usize>,
}

impl Consumer<'_> {
    /// One consumer step, in the order of `Logic::advance_to`. `late`
    /// pushes events between the read and the pops.
    fn step(
        &mut self,
        producers: &mut [Producer],
        late: &[usize],
        stats: &mut Stats,
    ) -> Result<TimeUs, String> {
        // (1) Every publication first; a retired source is no longer read.
        let mut open = false;
        for (p, prod) in self.published.iter_mut().zip(producers.iter()) {
            if p.state != SourceState::Retired {
                *p = prod.publication;
                if matches!(prod.closure, Closure::OpenAt(_)) {
                    stats.open_at_disconnect += 1;
                }
            }
            open |= p.state == SourceState::Open;
        }
        // (3) The global watermark, from the publications alone. With no
        // source open the play would abort; the schedules keep source 0
        // open.
        let w = watermark(&self.published, self.device)
            .filter(|_| open)
            .ok_or("no source left open")?;
        // The producers keep running between the read and the pops.
        for (prod, &k) in producers.iter_mut().zip(late) {
            let pushed = prod.push(k, stats);
            stats.late_pushed += pushed;
        }
        // (2) Each source's guaranteed prefix: pop while the last popped
        // event is not beyond the publication read, so the first event
        // beyond it is popped and stays staged, and later wakes pop nothing
        // more until the publication reaches it.
        for (i, prod) in producers.iter_mut().enumerate() {
            let p = self.published[i];
            if p.state == SourceState::Retired {
                continue;
            }
            let up_to = p.received_up_to;
            self.drained[i] = false;
            let mut popped = 0;
            while self.last_popped[i].is_none_or(|t| t <= up_to) {
                let Some(e) = prod.queue.pop_front() else {
                    self.drained[i] = true;
                    break;
                };
                self.last_popped[i] = Some(e.received);
                if e.received > up_to {
                    stats.popped_beyond += 1;
                }
                self.staged[self.position[i]]
                    .push(e)
                    .map_err(|err| format!("staging source {i} refused {e:?}: {err:?}"))?;
                popped += 1;
            }
            if popped == 0 && !self.drained[i] {
                stats.held_back += 1;
            }
        }
        // (4) Merge through W; later events stay staged.
        merge(&mut self.staged, self.device, w, &mut self.merged);
        for e in self.merged.drain(..) {
            if evaluation_time(&e, self.device) == w {
                stats.inputs_at_w += 1;
            }
            self.ev
                .push_input(e)
                .map_err(|err| format!("push_input({e:?}) at W {w:?}: {err:?}"))?;
        }
        // (5) Advance; the watermark never moves back.
        if let Some(&prev) = self.watermarks.last()
            && w < prev
        {
            return Err(format!("watermark moved back: {w:?} after {prev:?}"));
        }
        self.ev
            .advance_to(w, &mut self.out)
            .map_err(|err| format!("advance_to({w:?}): {err:?}"))?;
        // Then retire each closed source that is drained.
        for i in 0..self.published.len() {
            let was = self.published[i].state;
            self.published[i].retire_if_drained(self.drained[i], &self.staged[self.position[i]]);
            if was != SourceState::Retired && self.published[i].state == SourceState::Retired {
                stats.retirements += 1;
            }
        }
        self.watermarks.push(w);
        self.step_ends.push(self.out.len());
        Ok(w)
    }

    /// What a further step could change.
    fn fingerprint(&self, producers: &[Producer]) -> Vec<i64> {
        let mut f = vec![i64::try_from(self.out.len()).unwrap()];
        for (i, prod) in producers.iter().enumerate() {
            f.extend([
                self.published[i].received_up_to.0,
                self.published[i].state as i64,
                i64::try_from(self.staged[self.position[i]].len()).unwrap(),
                i64::try_from(prod.queue.len()).unwrap(),
                i64::try_from(prod.next).unwrap(),
            ]);
        }
        f
    }
}

/// Runs `input` live under `delivery`. Every call the consumer makes must
/// succeed, the producers' queues stay within capacity, publications and
/// the watermark only grow, and the run must end with W at the terminal
/// watermark; otherwise the schedule or the model broke a rule, and the
/// error says which.
///
/// # Errors
///
/// The first rule broken.
pub fn live(
    chart: &Chart,
    rule: &RulePreset,
    input: &EvalInput,
    delivery: &Delivery,
) -> Result<Live, String> {
    let device = &input.device;
    let n = device.sources.len();
    let terminal = delivery.terminal;
    if delivery.capacity.len() != n || delivery.capacity.contains(&0) {
        return Err("one positive capacity per source".to_owned());
    }
    let mut order = delivery.staged_order.clone();
    order.sort_unstable();
    if order != (0..n).collect::<Vec<_>>() {
        return Err("staged_order is not a permutation".to_owned());
    }
    let mut producers: Vec<Producer> = (0..n)
        .map(|i| Producer {
            log: input
                .events
                .iter()
                .filter(|e| e.source.index() == i)
                .copied()
                .collect(),
            next: 0,
            queue: VecDeque::new(),
            capacity: delivery.capacity[i],
            publication: Published::open(TimeUs(i64::MIN)),
            closure: Closure::None,
            offset: device.sources[i].input_offset,
        })
        .collect();
    // Sized from the play's input budget, as `Logic::new` sizes them: the
    // evaluator's ring for every input, each staging buffer for its
    // source's inputs and one more.
    let budget = input.events.len() + n;
    let mut position = vec![0; n];
    for (pos, &i) in delivery.staged_order.iter().enumerate() {
        position[i] = pos;
    }
    let staged = delivery
        .staged_order
        .iter()
        .map(|&i| {
            Staged::with_capacity(
                SourceId(u16::try_from(i).unwrap()),
                producers[i].log.len() + 1,
            )
        })
        .collect();
    let ev = Evaluator::new(chart, rule, device, &input.start, None, budget)
        .map_err(|e| format!("Evaluator::new: {e:?}"))?;
    let mut consumer = Consumer {
        ev,
        device,
        published: vec![Published::open(TimeUs(i64::MIN)); n],
        staged,
        position,
        last_popped: vec![None; n],
        drained: vec![false; n],
        merged: Vec::with_capacity(budget),
        out: Vec::new(),
        watermarks: Vec::new(),
        step_ends: Vec::new(),
    };
    let mut stats = Stats::default();
    let no_late = vec![0; n];
    for step in &delivery.steps {
        let mut late = no_late.clone();
        if !step.is_empty() {
            for (i, prod) in producers.iter_mut().enumerate() {
                let s = step[i % step.len()];
                prod.push(s.push, &mut stats);
                prod.publish(s.publish, terminal)?;
                if s.close {
                    prod.advance_closure()?;
                }
                late[i] = s.late_push;
            }
        }
        consumer.step(&mut producers, &late, &mut stats)?;
        check_queues(&producers)?;
    }
    // The terminal phase: push what fits, publish as far as allowed,
    // complete every pending closure, and wake the consumer, until W is T
    // and a further step changes nothing.
    let events = input.events.len();
    let cap = events + 4 * n + 8;
    let mut done = false;
    for _ in 0..cap {
        let before = consumer.fingerprint(&producers);
        for prod in &mut producers {
            prod.push(usize::MAX, &mut stats);
            if prod.closure == Closure::None {
                prod.publish(Publish::Highest, terminal)?;
            } else {
                prod.advance_closure()?;
            }
        }
        let w = consumer.step(&mut producers, &no_late, &mut stats)?;
        check_queues(&producers)?;
        let settled = producers
            .iter()
            .all(|p| matches!(p.closure, Closure::None | Closure::Closed));
        if w == terminal && settled && consumer.fingerprint(&producers) == before {
            done = true;
            break;
        }
    }
    if !done {
        return Err(format!(
            "the terminal phase did not settle at {terminal:?} within {cap} wakes (last W {:?})",
            consumer.watermarks.last()
        ));
    }
    Ok(Live {
        tally: *consumer.ev.tally(),
        gauges: consumer.ev.gauges().to_vec(),
        complete: consumer.ev.is_complete(),
        events: consumer.out,
        watermarks: consumer.watermarks,
        step_ends: consumer.step_ends,
        stats,
    })
}

fn check_queues(producers: &[Producer]) -> Result<(), String> {
    for (i, p) in producers.iter().enumerate() {
        if p.queue.len() > p.capacity {
            return Err(format!("source {i}'s queue is beyond its capacity"));
        }
    }
    Ok(())
}
