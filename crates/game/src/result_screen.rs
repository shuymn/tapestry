//! The result screen's model (design.md "Play Lifecycle", "Extension
//! Boundaries"): the presets a play can be viewed under, the view, and a
//! summary of the play under each preset viewed.
//!
//! - **Entries** are the shelf's, identified by kind and name ([`EntryId`]),
//!   with the preset the play used pinned as played when no listed preset
//!   has the record's hash.
//! - **Landing** on an entry resolves its status: the play's own summary
//!   for the record's hash, a cached summary, a file that never loaded, an
//!   evaluation over the budget, or else an evaluation now, on the caller's
//!   thread, condensed into a [`ViewSummary`] and cached, the least
//!   recently viewed summary evicted first when the cache is full.
//! - **Graphs** of every view are condensed on one time axis, fixed when
//!   the screen is built ([`Axis`]).
//! - **A sync** with the shelf, when its revision moved, rebuilds the
//!   entries, works out what changed for each by comparing the two
//!   snapshots, keeps the latest notices, and lands the view again on its
//!   entry, or on the play's own preset when its entry went away.
//!
//! The view never changes the play preset. Everything here runs on the
//! result screen, where allocation is allowed.

use std::collections::VecDeque;
use std::sync::Arc;
use std::time::{Duration, Instant};

use bms_model::{Chart, TimeUs};
use eval::{Evaluation, Tally};
use replay::PlayRecord;
use replay::verify::Coverage;
use rules::{GaugeValue, RulePreset};
use rules_toml::PresetFileError;

use crate::presets::Shelf;
use crate::result_graphs::{Axis, ResultGraphs, listed};
use crate::results::{ReEvaluate, ReEvaluationError, ResultContext};
use crate::session::PlayReport;

/// The most summaries the screen keeps; the least recently viewed goes
/// first, never the viewed one.
pub const RESULT_CACHE: usize = 32;

/// The gauges a summary shows, in the preset's order.
pub const SHOWN_GAUGES: usize = 6;

/// The notices the screen keeps: the latest ones.
pub const NOTICES: usize = 2;

/// What an entry is: names are not identities, since a refused user file
/// can have a built-in's stem.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum EntryId {
    /// A built-in, by name.
    BuiltIn(String),
    /// A user preset file, by stem.
    UserFile(String),
    /// The preset the play used, by its hash, listed while no shelf entry
    /// has that hash.
    AsPlayed([u8; 32]),
}

/// One preset the play can be viewed under.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewEntry {
    /// What it is.
    pub id: EntryId,
    /// Its name.
    pub name: String,
    /// Whether it is the shelf's chosen preset, which the next play uses.
    pub chosen: bool,
    /// The preset, unless its file never loaded.
    pub preset: Option<Arc<RulePreset>>,
    /// The preset's hash.
    pub hash: Option<[u8; 32]>,
    /// Why its file did not load at the last check, if it did not.
    pub error: Option<PresetFileError>,
}

/// A listed gauge at the end of a view's evaluation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GaugeEnd {
    /// Its index in the viewed preset.
    pub gauge: u16,
    /// Its value.
    pub value: GaugeValue,
    /// Whether it is cleared.
    pub cleared: bool,
    /// When a survival gauge failed, if it did.
    pub failed_at: Option<TimeUs>,
    /// It started at its start value, the practice record holding no
    /// value it accepts.
    pub started_at_default: bool,
}

/// The bytes a [`ViewSummary`] holds on the heap: its graphs' one
/// allocation, the reference counts included. The same for every play.
pub const SUMMARY_BYTES: usize = size_of::<ArcBlock>();

/// The layout of an `Arc<ResultGraphs>`'s allocation: two counts, then
/// the value.
#[expect(dead_code, reason = "measured for its size only")]
struct ArcBlock {
    strong: usize,
    weak: usize,
    value: ResultGraphs,
}

/// What a view shows of the play under one preset: the totals, the listed
/// gauges, what the input covers, the gauges' graphs and the deviations.
/// [`SUMMARY_BYTES`] on the heap and a fixed size inline, whatever the
/// play.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ViewSummary {
    /// Counts, EX score, BP, max combo and empty POORs.
    pub tally: Tally,
    /// The listed gauges, in the list's order
    /// ([`listed`](crate::result_graphs::listed)): the preset's first six,
    /// or its first five with the emphasized one in the fifth slot when it
    /// is not among them.
    pub gauges: [Option<GaugeEnd>; SHOWN_GAUGES],
    /// How many gauges are not listed.
    pub more_gauges: u16,
    /// What the recorded input covers.
    pub coverage: Coverage,
    /// Whether this is the play as it was judged.
    pub as_played: bool,
    /// The listed gauges' graphs and the press deviations.
    pub graphs: Arc<ResultGraphs>,
}

impl ViewSummary {
    /// The summary of `e`, evaluated under `preset`, on `axis`: `defaults`
    /// says per gauge whether it started at its start value (empty for a
    /// full play), and the gauge of ID `displayed`, if the preset has one,
    /// is emphasized, or else the preset's first. Allocates only the
    /// graphs, [`SUMMARY_BYTES`].
    #[must_use]
    pub fn of(
        e: &Evaluation,
        preset: &RulePreset,
        axis: Axis,
        coverage: Coverage,
        as_played: bool,
        defaults: &[bool],
        displayed: Option<&str>,
    ) -> Self {
        let count = e.gauges.len().min(preset.gauges.len());
        let emphasized = (count > 0).then(|| {
            displayed
                .and_then(|id| preset.gauges.iter().take(count).position(|g| g.id == id))
                .unwrap_or(0)
        });
        let (list, n) = listed(count, emphasized);
        let list = list.get(..n).unwrap_or_default();
        let mut gauges = [None; SHOWN_GAUGES];
        for (slot, &i) in gauges.iter_mut().zip(list) {
            *slot = e.gauges.get(i).map(|g| GaugeEnd {
                gauge: u16::try_from(i).unwrap_or(u16::MAX),
                value: g.value,
                cleared: g.cleared,
                failed_at: g.failed_at,
                started_at_default: defaults.get(i).copied().unwrap_or(false),
            });
        }
        let graphs = ResultGraphs::of(e, &preset.gauges, list, emphasized, axis, coverage);
        Self {
            tally: e.tally,
            gauges,
            more_gauges: u16::try_from(e.gauges.len().saturating_sub(n)).unwrap_or(u16::MAX),
            coverage,
            as_played,
            graphs: Arc::new(graphs),
        }
    }
}

/// What the view shows, by precedence.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ViewStatus<'a> {
    /// The play recorded nothing, whatever is viewed.
    NothingRecorded,
    /// The viewed file never loaded.
    CannotLoad(&'a PresetFileError),
    /// Evaluating under the viewed preset needs more than the budget.
    OverBudget {
        /// The bytes it needs.
        needed: usize,
        /// The budget.
        budget: usize,
    },
    /// The play could not be evaluated under it.
    Failed(&'a ReEvaluationError),
    /// The play under it.
    Ready(&'a ViewSummary),
}

/// Something that changed on the shelf while the result shows.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Notice {
    /// The viewed or chosen entry of this name loaded a new version.
    Reloaded(String),
    /// The chosen preset of this name reloaded: the next play uses it.
    NextPlayUsesIt(String),
    /// The viewed entry of this name went away: the view is back on the
    /// play's own preset.
    Removed(String),
    /// A file failed to load, newly or with another error.
    Failed(PresetFileError),
    /// The presets folder could not be listed.
    ListingFailed(PresetFileError),
}

/// How the view landed.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Landed {
    Nothing,
    CannotLoad,
    OverBudget {
        needed: usize,
        budget: usize,
    },
    /// A summary by hash: the play's own, or the cache's.
    Hash([u8; 32]),
}

/// A cached summary of the play under the preset of `hash`.
#[derive(Clone, Debug)]
struct Cached {
    hash: [u8; 32],
    summary: Result<ViewSummary, ReEvaluationError>,
    used: u64,
}

/// The result screen of one play.
#[derive(Clone, Debug)]
pub struct ResultScreen {
    entries: Vec<ViewEntry>,
    cursor: usize,
    /// The shelf's revision the entries were built from.
    revision: u64,
    listing_error: Option<PresetFileError>,
    /// The hash of the preset the play used.
    played: [u8; 32],
    /// The play's own summary; `None` without a record.
    own: Option<Result<ViewSummary, ReEvaluationError>>,
    /// The time axis every view is condensed on: from the play's start to
    /// the as-played final deadline, or, when the play has no own
    /// evaluation, the first what-if's.
    axis: Option<Axis>,
    /// The ID of the gauge the play displayed.
    displayed: Option<String>,
    cache: Vec<Cached>,
    uses: u64,
    landed: Landed,
    notices: VecDeque<Notice>,
    budget: usize,
    evaluations: u64,
    last_evaluation: Option<Duration>,
}

impl ResultScreen {
    /// The screen of `report`'s play of `chart`, listing `shelf`, the view on
    /// the play's own preset; an evaluation needing more than `budget`
    /// bytes is refused.
    #[must_use]
    pub fn new(report: &PlayReport, shelf: &Shelf, chart: &Chart, budget: usize) -> Self {
        let played = report
            .record
            .as_ref()
            .map_or_else(|| replay::preset_hash(&report.preset), |r| r.preset.sha256);
        let displayed = report
            .preset
            .gauges
            .get(report.displayed_gauge.0)
            .map(|g| g.id.clone());
        let origin = report
            .record
            .as_ref()
            .and_then(|r| r.play_type.practice())
            .map_or(TimeUs(0), |p| p.start);
        let axis = report
            .result
            .as_ref()
            .and_then(|s| s.evaluation.as_ref().ok())
            .map(|e| Axis {
                origin,
                end: e.final_deadline,
            });
        let own = report
            .record
            .as_ref()
            .zip(report.result.as_ref())
            .map(|(rec, s)| {
                let coverage = if rec.status.end == replay::EndStatus::EndedMidway {
                    Coverage::EndedMidway(rec.status.finalized)
                } else {
                    Coverage::Recorded
                };
                s.evaluation
                    .as_ref()
                    .map(|e| {
                        let axis = Axis {
                            origin,
                            end: e.final_deadline,
                        };
                        ViewSummary::of(
                            e,
                            &report.preset,
                            axis,
                            coverage,
                            true,
                            &[],
                            displayed.as_deref(),
                        )
                    })
                    .map_err(Clone::clone)
            });
        let mut screen = Self {
            entries: Vec::new(),
            cursor: 0,
            revision: shelf.revision(),
            listing_error: shelf.listing_error().cloned(),
            played,
            own,
            axis,
            displayed,
            cache: Vec::new(),
            uses: 0,
            landed: Landed::Nothing,
            notices: VecDeque::new(),
            budget,
            evaluations: 0,
            last_evaluation: None,
        };
        screen.entries = screen.snapshot(report, shelf);
        // A folder that could not be listed before the result showed is
        // still not listed: said on entry, as a later failure would be.
        if let Some(e) = shelf.listing_error() {
            screen.notify(Notice::ListingFailed(e.clone()));
        }
        screen.cursor = screen.played_entry();
        screen.land(report, chart);
        screen
    }

    /// The entries, in the order the view steps through them.
    #[must_use]
    pub fn entries(&self) -> &[ViewEntry] {
        &self.entries
    }

    /// The viewed entry's index.
    #[must_use]
    pub const fn cursor(&self) -> usize {
        self.cursor
    }

    /// The hash of the preset the play used: its entry is its own.
    #[must_use]
    pub const fn played(&self) -> [u8; 32] {
        self.played
    }

    /// The viewed entry.
    #[must_use]
    pub fn viewed(&self) -> Option<&ViewEntry> {
        self.entries.get(self.cursor)
    }

    /// What the view shows.
    #[must_use]
    pub fn status(&self) -> ViewStatus<'_> {
        match &self.landed {
            Landed::Nothing => ViewStatus::NothingRecorded,
            Landed::CannotLoad => self
                .viewed()
                .and_then(|e| e.error.as_ref())
                .map_or(ViewStatus::NothingRecorded, ViewStatus::CannotLoad),
            &Landed::OverBudget { needed, budget } => ViewStatus::OverBudget { needed, budget },
            Landed::Hash(h) => {
                let summary = if *h == self.played {
                    self.own.as_ref()
                } else {
                    self.cache.iter().find(|c| c.hash == *h).map(|c| &c.summary)
                };
                match summary {
                    Some(Ok(s)) => ViewStatus::Ready(s),
                    Some(Err(e)) => ViewStatus::Failed(e),
                    None => ViewStatus::NothingRecorded,
                }
            }
        }
    }

    /// The viewed file's standing error while its last good version is
    /// viewed.
    #[must_use]
    pub fn file_error(&self) -> Option<&PresetFileError> {
        self.viewed()
            .filter(|e| e.preset.is_some())
            .and_then(|e| e.error.as_ref())
    }

    /// The latest notices, oldest first.
    pub fn notices(&self) -> impl Iterator<Item = &Notice> {
        self.notices.iter()
    }

    /// How many evaluations the screen ran.
    #[must_use]
    pub const fn evaluations(&self) -> u64 {
        self.evaluations
    }

    /// How long the latest evaluation took.
    #[must_use]
    pub const fn last_evaluation(&self) -> Option<Duration> {
        self.last_evaluation
    }

    /// The time axis every view is condensed on, once any evaluation
    /// fixed it.
    #[must_use]
    pub const fn axis(&self) -> Option<Axis> {
        self.axis
    }

    /// How many summaries are cached.
    #[must_use]
    pub fn cached(&self) -> usize {
        self.cache.len()
    }

    /// Moves the view `by` entries, wrapping at both ends, and lands it.
    /// Whether anything shown changed.
    pub fn step(&mut self, by: isize, report: &PlayReport, chart: &Chart) -> bool {
        let n = self.entries.len();
        if n <= 1 {
            return false;
        }
        let shift = by.rem_euclid(isize::try_from(n).unwrap_or(isize::MAX));
        self.cursor = (self.cursor + shift.unsigned_abs()) % n;
        self.land(report, chart);
        true
    }

    /// Catches up with `shelf` if its revision moved since the last sync:
    /// rebuilds the entries, notes what changed, and lands the view again
    /// on its entry, or on the play's own preset when its entry went away.
    /// Whether anything shown changed.
    pub fn sync(&mut self, report: &PlayReport, shelf: &Shelf, chart: &Chart) -> bool {
        if shelf.revision() == self.revision {
            return false;
        }
        self.revision = shelf.revision();
        let new = self.snapshot(report, shelf);
        let old = std::mem::replace(&mut self.entries, new);
        let viewed = old.get(self.cursor).map(|e| e.id.clone());
        match (shelf.listing_error(), self.listing_error.take()) {
            (Some(e), before) if before.as_ref() != Some(e) => {
                self.notify(Notice::ListingFailed(e.clone()));
            }
            (None, Some(_)) => self
                .notices
                .retain(|n| !matches!(n, Notice::ListingFailed(_))),
            _ => {}
        }
        self.listing_error = shelf.listing_error().cloned();
        let mut found = Vec::new();
        for new in &self.entries {
            let Some(before) = old.iter().find(|o| o.id == new.id) else {
                found.extend(new.error.clone().map(Notice::Failed));
                continue;
            };
            let reloaded = new.hash.is_some() && new.hash != before.hash;
            if reloaded && (new.chosen || viewed.as_ref() == Some(&new.id)) {
                found.push(Notice::Reloaded(new.name.clone()));
            }
            if reloaded && new.chosen {
                found.push(Notice::NextPlayUsesIt(new.name.clone()));
            }
            match (&new.error, &before.error) {
                (Some(e), b) if b.as_ref() != Some(e) => found.push(Notice::Failed(e.clone())),
                // Recovered: the file's failure notices go.
                (None, Some(b)) => self
                    .notices
                    .retain(|n| !matches!(n, Notice::Failed(f) if f.path == b.path)),
                _ => {}
            }
        }
        for notice in found {
            self.notify(notice);
        }
        let at = viewed.and_then(|id| self.entries.iter().position(|e| e.id == id));
        self.cursor = match at {
            Some(i) => i,
            None => {
                // The pinned entry goes when a listed preset has the play's
                // hash again: that is no removal.
                if let Some(o) = old.get(self.cursor)
                    && !matches!(o.id, EntryId::AsPlayed(_))
                {
                    self.notify(Notice::Removed(o.name.clone()));
                }
                self.played_entry()
            }
        };
        self.land(report, chart);
        true
    }

    fn notify(&mut self, notice: Notice) {
        if self.notices.len() == NOTICES {
            self.notices.pop_front();
        }
        self.notices.push_back(notice);
    }

    /// The entry of the play's own preset.
    fn played_entry(&self) -> usize {
        self.entries
            .iter()
            .position(|e| e.hash == Some(self.played))
            .unwrap_or(0)
    }

    /// The shelf's entries, with the play's preset pinned when none has
    /// its hash, before the entry of its name.
    fn snapshot(&self, report: &PlayReport, shelf: &Shelf) -> Vec<ViewEntry> {
        let mut entries: Vec<ViewEntry> = shelf
            .entries()
            .into_iter()
            .map(|e| ViewEntry {
                id: if e.built_in {
                    EntryId::BuiltIn(e.name.to_owned())
                } else {
                    EntryId::UserFile(e.name.to_owned())
                },
                name: e.name.to_owned(),
                chosen: e.chosen,
                hash: e.preset.map(replay::preset_hash),
                preset: e.preset.map(|p| Arc::new(p.clone())),
                error: e.error.cloned(),
            })
            .collect();
        if !entries.iter().any(|e| e.hash == Some(self.played)) {
            let at = entries
                .iter()
                .position(|e| e.name == report.preset.id)
                .unwrap_or(entries.len());
            entries.insert(
                at,
                ViewEntry {
                    id: EntryId::AsPlayed(self.played),
                    name: report.preset.id.clone(),
                    chosen: false,
                    preset: Some(Arc::new(report.preset.clone())),
                    hash: Some(self.played),
                    error: None,
                },
            );
        }
        entries
    }

    /// Resolves the viewed entry's status, evaluating the play under it
    /// when nothing has yet.
    fn land(&mut self, report: &PlayReport, chart: &Chart) {
        let Some(record) = &report.record else {
            self.landed = Landed::Nothing;
            return;
        };
        let Some(entry) = self.entries.get(self.cursor) else {
            self.landed = Landed::Nothing;
            return;
        };
        let (Some(preset), Some(hash)) = (entry.preset.clone(), entry.hash) else {
            self.landed = Landed::CannotLoad;
            return;
        };
        self.landed = Landed::Hash(hash);
        if hash == self.played {
            return;
        }
        self.uses += 1;
        if let Some(c) = self.cache.iter_mut().find(|c| c.hash == hash) {
            c.used = self.uses;
            return;
        }
        if let Some(needed) = what_if_bytes(chart, &preset, record)
            && needed > self.budget
        {
            self.landed = Landed::OverBudget {
                needed,
                budget: self.budget,
            };
            return;
        }
        // Evicted before the new summary is condensed: the cache never
        // holds more than its capacity.
        if self.cache.len() >= RESULT_CACHE
            && let Some(lru) = self
                .cache
                .iter()
                .enumerate()
                .min_by_key(|(_, c)| c.used)
                .map(|(i, _)| i)
        {
            self.cache.swap_remove(lru);
        }
        let began = Instant::now();
        let evaluated = ReEvaluate::under(
            &ResultContext {
                record,
                chart,
                preset: &report.preset,
                live: &report.live,
            },
            &preset,
        );
        self.last_evaluation = Some(began.elapsed());
        self.evaluations += 1;
        // Condensed at once: the event stream and the histories go with
        // the evaluation.
        let summary = evaluated.map(|r| {
            let axis = *self.axis.get_or_insert(Axis {
                origin: record.play_type.practice().map_or(TimeUs(0), |p| p.start),
                end: r.evaluation.final_deadline,
            });
            ViewSummary::of(
                &r.evaluation,
                &preset,
                axis,
                r.coverage,
                r.as_played,
                &r.started_at_default,
                self.displayed.as_deref(),
            )
        });
        self.cache.push(Cached {
            hash,
            summary,
            used: self.uses,
        });
    }
}

/// The bytes evaluating `record` of `chart` under `preset` and condensing
/// it hold at their peak: `replay::verify::batch_bytes`, the estimate a
/// rebuild admits each record with, and the summary built while the
/// evaluation is alive ([`SUMMARY_BYTES`]); `None` when the evaluator
/// refuses them, which the evaluation then reports. Every lane object
/// counts, a section's record included.
#[must_use]
pub fn what_if_bytes(chart: &Chart, preset: &RulePreset, record: &PlayRecord) -> Option<usize> {
    replay::verify::batch_bytes(record, chart, preset)
        .ok()
        .map(|b| b.saturating_add(SUMMARY_BYTES))
}
