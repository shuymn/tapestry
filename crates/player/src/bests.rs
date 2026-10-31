//! Personal bests on the game thread (design.md "Personal Bests"): the
//! record sink that counts each saved play into the library as its result
//! shows, and how a best is drawn, on the select screen and the end screen.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use game::{BestScore, BestView, Chain, LocalRecords, RecordRef, RecordSink, SaveError, SavedPlay};
use library::{Best, BestWriter, Contribution, Ineligible, Ingested, PlayId, Score, Verified};
use replay::PlayRecord;
use rules::RulePreset;

/// Saves records as [`LocalRecords`] does, and counts each saved play into
/// the bests through `bests`, the library's live writer. A play that
/// cannot be counted is logged, and a catch-up is requested so the next
/// pass counts it from its file.
pub struct BestsRecords {
    /// Where records are saved.
    pub local: LocalRecords,
    /// The data root's `records`, which play paths are relative to.
    pub dir: PathBuf,
    /// The library's live writer, shared by every chart's sink.
    pub bests: Arc<Mutex<BestWriter>>,
}

impl RecordSink for BestsRecords {
    fn save(&mut self, rec: &PlayRecord) -> Result<RecordRef, SaveError> {
        self.local.save(rec)
    }

    fn after_result(&mut self, play: &SavedPlay<'_>) -> Option<BestView> {
        let Ok(saved) = play.saved else {
            return Some(BestView::NotCounted(Ineligible::NotSaved.to_string()));
        };
        let began = Instant::now();
        let view = self.count(saved, play);
        let line = format!(
            "bests: the play was counted in {} ms",
            began.elapsed().as_millis()
        );
        println!("{line}");
        log::info!("{line}");
        Some(view)
    }
}

impl BestsRecords {
    fn count(&self, saved: &RecordRef, play: &SavedPlay<'_>) -> BestView {
        let record = play.record;
        // Refused by verification: no row, as the rebuild leaves none.
        let (Ok(preset), Ok(evaluation)) = (
            replay::verify::check_record(record),
            play.summary.evaluation.as_ref(),
        ) else {
            return BestView::NotCounted(Ineligible::Unverified.to_string());
        };
        let writer = || self.bests.lock().unwrap_or_else(PoisonError::into_inner);
        let failed = |cause: String| {
            let line = format!("bests: the play was not counted: {cause}");
            eprintln!("{line}");
            log::warn!("{line}");
            if let Err(e) = writer().request_pass() {
                let line = format!("bests: a catch-up cannot be requested: {}", Chain(&e));
                eprintln!("{line}");
                log::warn!("{line}");
            }
            BestView::NotCounted(format!("failed: {cause}"))
        };
        let Some(rel) = relative(&self.dir, &saved.path) else {
            return failed(format!("{} is not under the records", saved.path.display()));
        };
        let (size, mtime) = match std::fs::metadata(&saved.path) {
            Ok(meta) => library::stamp(&meta),
            Err(e) => return failed(e.to_string()),
        };
        let verified = Verified {
            completed: evaluation.completed,
            random: !play.chart.identity.random_results.is_empty(),
        };
        let c = Contribution::of(
            PlayId::of(rel, record, size, mtime),
            record,
            &verified,
            evaluation,
            &preset,
        );
        // The guard goes before a failure requests a catch-up through it.
        let ingested = writer().ingest(&c);
        match ingested {
            Ok(Ingested::Counted { previous, current }) => BestView::Counted {
                play: (evaluation.tally.ex_score, evaluation.tally.bp),
                previous: previous.map(|s| shown(&s, &preset)),
                current: shown(&current, &preset),
                ineligible: c.ineligible.map(|r| r.to_string()),
            },
            Ok(Ingested::AlreadyCounted) => BestView::NotCounted("counted before".to_owned()),
            Err(e) => failed(Chain(&e).to_string()),
        }
    }
}

/// `path` relative to `dir`, `/` separated; `None` when it is not under it
/// or not Unicode.
fn relative(dir: &Path, path: &Path) -> Option<String> {
    let parts: Option<Vec<&str>> = path
        .strip_prefix(dir)
        .ok()?
        .components()
        .map(|c| match c {
            Component::Normal(s) => s.to_str(),
            _ => None,
        })
        .collect();
    parts.map(|p| p.join("/"))
}

/// A stored score as the end screen shows it.
pub fn shown(score: &Score, preset: &RulePreset) -> BestScore {
    let best = score.best.as_ref();
    BestScore {
        plays: score.plays,
        ex: best.map(|b| b.ex),
        bp: best.map(|b| b.bp),
        lamp: best.map(|b| lamp_text(b, preset)),
    }
}

/// The lamp of `best` under `preset`: `FC` for a FULL COMBO, else the
/// cleared gauge with the highest `lamp` rank, uppercased, else `FAILED`.
pub fn lamp_text(best: &Best, preset: &RulePreset) -> String {
    if best.full_combo {
        return "FC".to_owned();
    }
    preset
        .gauges
        .iter()
        .enumerate()
        .filter(|&(i, _)| best.cleared(i))
        .max_by_key(|(_, g)| g.lamp)
        .map_or_else(|| "FAILED".to_owned(), |(_, g)| g.id.to_uppercase())
}

/// The end screen's line about the play's best: the best with the play's
/// difference from the previous one, or why it set none.
pub fn best_line(view: &BestView) -> String {
    match view {
        BestView::Counted {
            current,
            ineligible: Some(why),
            ..
        } => format!("BEST NOT UPDATED: {why}  PLAYS {}", current.plays),
        BestView::Counted {
            play,
            previous,
            current,
            ineligible: None,
        } => {
            let before = previous.as_ref().filter(|p| p.ex.is_some());
            let delta = |now: u32, then: Option<u32>| {
                then.map_or_else(String::new, |t| {
                    format!(" ({:+})", i64::from(now) - i64::from(t))
                })
            };
            let new = before.is_none_or(|p| {
                (p.ex, p.bp, p.lamp.as_ref()) != (current.ex, current.bp, current.lamp.as_ref())
            });
            format!(
                "BEST EX {}{}  BP {}{}  {}  PLAYS {}{}",
                current.ex.unwrap_or_default(),
                delta(play.0, before.and_then(|p| p.ex)),
                current.bp.unwrap_or_default(),
                delta(play.1, before.and_then(|p| p.bp)),
                current.lamp.as_deref().unwrap_or_default(),
                current.plays,
                if new { "  NEW BEST" } else { "" }
            )
        }
        BestView::NotCounted(why) => format!("BEST NOT UPDATED: {why}"),
    }
}

/// Columns of the select screen's lamp: the longest built-in gauge ID,
/// `ASSIST_EASY`; a longer one is cut.
pub const LAMP_COLUMNS: usize = 11;

/// The select screen's cell for a chart: its lamp in [`LAMP_COLUMNS`] and
/// its best EX in 5; blank when it was never played under the preset, `-`
/// when it was but has no best (a `#RANDOM` chart, or no eligible play).
pub fn chart_cell(score: Option<&Score>, preset: &RulePreset) -> String {
    let (lamp, ex) = match score {
        None => (String::new(), String::new()),
        Some(Score { best: None, .. }) => ("-".to_owned(), "-".to_owned()),
        Some(Score { best: Some(b), .. }) => (lamp_text(b, preset), b.ex.to_string()),
    };
    let lamp: String = lamp.chars().take(LAMP_COLUMNS).collect();
    format!("{lamp:LAMP_COLUMNS$} {ex:>5}")
}

#[cfg(test)]
mod tests {
    #[path = "../../../../library/tests/common/mod.rs"]
    mod common;

    use std::sync::{Arc, Mutex};

    use game::{BestScore, BestView, RecordSink as _};
    use library::{Best, Score};

    use super::{best_line, chart_cell, lamp_text, relative};

    /// A preset of four gauges listed out of rank order.
    fn ranked() -> rules::RulePreset {
        let mut p = rules::tapestry();
        let first = p.gauges[0].clone();
        p.gauges = [("normal", 2), ("assist_easy", 0), ("easy", 1), ("hard", 3)]
            .into_iter()
            .map(|(id, lamp)| rules::GaugeRule {
                id: id.to_owned(),
                lamp,
                ..first.clone()
            })
            .collect();
        p
    }

    fn cleared(gauges: &[usize], full_combo: bool) -> Best {
        let mut bits = vec![0_u8];
        for &i in gauges {
            bits[0] |= 1 << i;
        }
        Best {
            ex: 10,
            bp: 0,
            cleared: bits,
            full_combo,
        }
    }

    #[test]
    fn lamp_text_follows_rank_not_vector_order() {
        let p = ranked();
        // Easy (rank 1, third) and normal (rank 2, first) cleared.
        assert_eq!(lamp_text(&cleared(&[0, 2], false), &p), "NORMAL");
        assert_eq!(lamp_text(&cleared(&[1, 2], false), &p), "EASY");
        assert_eq!(lamp_text(&cleared(&[0, 3], false), &p), "HARD");
        assert_eq!(lamp_text(&cleared(&[0, 2], true), &p), "FC");
        assert_eq!(lamp_text(&cleared(&[], false), &p), "FAILED");
    }

    #[test]
    fn best_line_shows_difference_and_reason() {
        let score = |plays, ex, bp, lamp: &str| BestScore {
            plays,
            ex: Some(ex),
            bp: Some(bp),
            lamp: Some(lamp.to_owned()),
        };
        let new = BestView::Counted {
            play: (1234, 10),
            previous: Some(score(4, 1222, 13, "NORMAL")),
            current: score(5, 1234, 10, "HARD"),
            ineligible: None,
        };
        assert_eq!(
            best_line(&new),
            "BEST EX 1234 (+12)  BP 10 (-3)  HARD  PLAYS 5  NEW BEST"
        );
        let worse = BestView::Counted {
            play: (1200, 20),
            previous: Some(score(5, 1234, 10, "HARD")),
            current: score(6, 1234, 10, "HARD"),
            ineligible: None,
        };
        assert_eq!(
            best_line(&worse),
            "BEST EX 1234 (-34)  BP 10 (+10)  HARD  PLAYS 6"
        );
        let first = BestView::Counted {
            play: (50, 2),
            previous: None,
            current: score(1, 50, 2, "FAILED"),
            ineligible: None,
        };
        assert_eq!(
            best_line(&first),
            "BEST EX 50  BP 2  FAILED  PLAYS 1  NEW BEST"
        );
        let midway = BestView::Counted {
            play: (3, 1),
            previous: None,
            current: BestScore {
                plays: 1,
                ..BestScore::default()
            },
            ineligible: Some(library::Ineligible::EndedMidway.to_string()),
        };
        assert_eq!(
            crate::end_screen::fit(&best_line(&midway), 76),
            "BEST NOT UPDATED: ENDED MIDWAY  PLAYS 1"
        );
        let unsaved = BestView::NotCounted(library::Ineligible::NotSaved.to_string());
        assert_eq!(
            crate::end_screen::fit(&best_line(&unsaved), 76),
            "BEST NOT UPDATED: NOT SAVED"
        );
    }

    #[test]
    fn chart_cells_are_fixed_width() {
        let p = ranked();
        let scored = Score {
            plays: 2,
            best: Some(cleared(&[3], false)),
        };
        assert_eq!(chart_cell(Some(&scored), &p), "HARD           10");
        let random = Score {
            plays: 2,
            best: None,
        };
        assert_eq!(chart_cell(Some(&random), &p), "-               -");
        assert_eq!(chart_cell(None, &p), " ".repeat(17));
        let mut long = p.clone();
        long.gauges[3].id = "much_harder_than_hard".to_owned();
        assert_eq!(chart_cell(Some(&scored), &long), "MUCH_HARDER    10");
    }

    /// The sink over the fixture's records and library.
    fn sink(fx: &common::Fixture, lib: &library::Library) -> super::BestsRecords {
        super::BestsRecords {
            local: game::LocalRecords::new(fx.records.clone()),
            dir: fx.records.clone(),
            bests: Arc::new(Mutex::new(lib.best_writer().unwrap())),
        }
    }

    /// Saves `record` through `sink` and hands it the play as
    /// `finish_play` does.
    fn finish(sink: &mut super::BestsRecords, record: &replay::PlayRecord) -> BestView {
        let dir = tempfile::tempdir().unwrap();
        let chart =
            replay::verify::load_recorded_chart(record, common::CHART.as_bytes(), dir.path())
                .unwrap();
        let summary = game::ResultSummary {
            evaluation: Ok(replay::verify::evaluate_record(record, &chart, None)
                .unwrap()
                .evaluation),
            matches_live: true,
            recommended_offset: None,
        };
        let saved = sink.save(record);
        sink.after_result(&game::SavedPlay {
            record,
            saved: saved.as_ref(),
            summary: &summary,
            chart: &chart,
            preset: &rules::tapestry(),
        })
        .unwrap()
    }

    #[test]
    fn failed_save_sets_no_best() {
        let fx = common::Fixture::new();
        // A file named `records` blocks the records folder.
        std::fs::write(&fx.records, b"not a folder").unwrap();
        let lib = fx.open();
        let mut sink = sink(&fx, &lib);
        let view = finish(&mut sink, &common::played(0));
        assert_eq!(view, BestView::NotCounted("not saved".to_owned()));
        drop((sink, lib));
        let _lib = fx.open();
        assert_eq!(common::tables(&fx.db()), (Vec::new(), Vec::new()));
    }

    #[test]
    fn a_saved_play_is_counted_as_its_result_shows() {
        let fx = common::Fixture::new();
        let lib = fx.open();
        // The catch-up the new database requested completes.
        fx.pass(&lib);
        let mut sink = sink(&fx, &lib);
        let view = finish(&mut sink, &common::played(0));
        assert_eq!(best_line(&view), "BEST EX 6  BP 0  FC  PLAYS 1  NEW BEST");
        let view = finish(&mut sink, &common::played(30));
        assert!(best_line(&view).starts_with("BEST EX 6 (-"), "{view:?}");
        assert!(best_line(&view).ends_with("FC  PLAYS 2"), "{view:?}");
        // A database failure: the play is not counted, and a catch-up is
        // requested for its file.
        common::sql(
            &fx.db(),
            "CREATE TRIGGER fail BEFORE INSERT ON scores BEGIN SELECT RAISE(ABORT, 'x'); END;",
        );
        let before = lib.bests_pending().unwrap();
        assert!(!before);
        let BestView::NotCounted(why) = finish(&mut sink, &common::played(0)) else {
            panic!("counted");
        };
        assert!(why.starts_with("failed: "), "{why}");
        assert!(lib.bests_pending().unwrap());
    }

    #[test]
    fn play_paths_are_relative_and_slash_separated() {
        let dir = std::path::Path::new("data").join("records");
        assert_eq!(
            relative(&dir, &dir.join("ab").join("1-2.tprl")).as_deref(),
            Some("ab/1-2.tprl")
        );
        assert_eq!(relative(&dir, std::path::Path::new("elsewhere")), None);
    }
}
