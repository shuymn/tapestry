//! Personal bests on the game thread (design.md "Personal Bests"): the
//! record sink that counts each saved play into the library as its result
//! shows, and how a best is drawn, on the select screen and the end screen.

use std::path::{Component, Path, PathBuf};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Instant;

use game::{
    BestKey, BestLamp, BestLookup, BestScore, BestView, Chain, LocalRecords, RecordRef, RecordSink,
    SaveError, SavedPlay,
};
use library::{
    Best, BestWriter, Contribution, Ineligible, Ingested, Key, LN_MODE, PlayId, Score, Verified,
};
use replay::PlayRecord;
use rules::RulePreset;
use skin_model::{GaugeLook, OwnLamp, RowScore};

/// Saves records as [`LocalRecords`] does, and counts each saved play into
/// the bests through `bests`, the library's live writer. A play that
/// cannot be counted is logged, and a catch-up is requested so the next
/// pass counts it from its file.
pub struct BestsRecords {
    /// Where records are saved.
    local: LocalRecords,
    /// The same folder, which play paths are relative to.
    dir: PathBuf,
    /// The library's live writer, shared by every chart's sink.
    bests: Arc<Mutex<BestWriter>>,
}

impl BestsRecords {
    /// Saves under `records` (the data root's `records`) and counts through
    /// `bests`.
    pub fn new(records: PathBuf, bests: Arc<Mutex<BestWriter>>) -> Self {
        Self {
            local: LocalRecords::new(records.clone()),
            dir: records,
            bests,
        }
    }
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

    /// The key's score through the live writer's connection, one indexed
    /// read; a failed read is logged and leaves the best unavailable.
    fn best_before(&mut self, key: &BestKey, preset: &RulePreset) -> BestLookup {
        let read = self
            .bests
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .score(&Key {
                preset_sha256: key.preset_sha256,
                ln_mode: LN_MODE,
                chart_sha256: key.chart_sha256,
            });
        match read {
            Ok(score) => lookup(score.as_ref(), preset),
            Err(e) => {
                let line = format!("bests: the best cannot be read: {}", Chain(&e));
                eprintln!("{line}");
                log::warn!("{line}");
                BestLookup::Unavailable
            }
        }
    }
}

/// A key's stored score under `preset` as a result view compares with it:
/// known to be empty without a row or a best.
pub fn lookup(score: Option<&Score>, preset: &RulePreset) -> BestLookup {
    match score {
        Some(s) if s.best.is_some() => BestLookup::Known(shown(s, preset)),
        _ => BestLookup::KnownEmpty,
    }
}

impl BestsRecords {
    fn count(&self, saved: &RecordRef, play: &SavedPlay<'_>) -> BestView {
        let record = play.record;
        // No played attempt: no row, as the rebuild leaves none.
        if !record.played() {
            return BestView::NotCounted(Ineligible::NoPlayedAttempt.to_string());
        }
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
        };
        let c = Contribution::of(
            PlayId::of(rel, record, size, mtime),
            record,
            &verified,
            evaluation,
            play.chart,
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
        clear: best.map(|b| best_lamp(own_lamp(b, preset))),
    }
}

/// A lamp as `game` types it, without its look.
pub fn best_lamp(lamp: OwnLamp) -> BestLamp {
    match lamp {
        OwnLamp::FullCombo => BestLamp::FullCombo,
        OwnLamp::Cleared { gauge, .. } => BestLamp::Cleared { gauge },
        OwnLamp::Failed => BestLamp::Failed,
    }
}

/// A typed lamp's text under `preset`, as [`lamp_name`].
pub fn clear_name(lamp: BestLamp, preset: &RulePreset) -> String {
    match lamp {
        BestLamp::FullCombo => "FC".to_owned(),
        BestLamp::Failed => "FAILED".to_owned(),
        BestLamp::Cleared { gauge } => preset
            .gauges
            .get(usize::from(gauge))
            .map_or_else(String::new, |g| g.id.to_uppercase()),
    }
}

/// The lamp of `best` under `preset`: a FULL COMBO, else the cleared gauge
/// with the highest `lamp` rank, with its look, else failed.
pub fn own_lamp(best: &Best, preset: &RulePreset) -> OwnLamp {
    if best.full_combo {
        return OwnLamp::FullCombo;
    }
    preset
        .gauges
        .iter()
        .enumerate()
        .filter(|&(i, _)| best.cleared(i))
        .max_by_key(|(_, g)| g.lamp)
        .and_then(|(i, g)| {
            Some(OwnLamp::Cleared {
                gauge: u16::try_from(i).ok()?,
                rank: g.lamp,
                look: GaugeLook::of(g),
            })
        })
        .unwrap_or(OwnLamp::Failed)
}

/// A lamp's text under `preset`: `FC`, the cleared gauge's ID uppercased, or
/// `FAILED`.
pub fn lamp_name(lamp: OwnLamp, preset: &RulePreset) -> String {
    clear_name(best_lamp(lamp), preset)
}

/// The text of `best`'s lamp under `preset` ([`own_lamp`], [`lamp_name`]).
#[cfg(test)]
pub fn lamp_text(best: &Best, preset: &RulePreset) -> String {
    lamp_name(own_lamp(best, preset), preset)
}

/// A chart's stored score under `preset` as a row shows it: unplayed when
/// there is none.
pub fn row_score(score: Option<&Score>, preset: &RulePreset) -> RowScore {
    match score {
        None => RowScore::Unplayed,
        Some(Score {
            plays, best: None, ..
        }) => RowScore::NoBest { plays: *plays },
        Some(Score {
            plays,
            best: Some(b),
            ..
        }) => RowScore::Best {
            plays: *plays,
            ex: b.ex,
            bp: b.bp,
            lamp: own_lamp(b, preset),
        },
    }
}

/// The end screen's line about the play's best under its `preset`: the
/// best with the play's difference from the previous one, or why it set
/// none.
pub fn best_line(view: &BestView, preset: &RulePreset) -> String {
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
            let new = before
                .is_none_or(|p| (p.ex, p.bp, p.clear) != (current.ex, current.bp, current.clear));
            format!(
                "BEST EX {}{}  BP {}{}  {}  PLAYS {}{}",
                current.ex.unwrap_or_default(),
                delta(play.0, before.and_then(|p| p.ex)),
                current.bp.unwrap_or_default(),
                delta(play.1, before.and_then(|p| p.bp)),
                current
                    .clear
                    .map(|c| clear_name(c, preset))
                    .unwrap_or_default(),
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

/// The select screen's cell for a chart's row score: its lamp in
/// [`LAMP_COLUMNS`] under `preset` and its best EX in 5; blank when it was
/// never played under the preset, `-` when it was but no play set a best.
pub fn chart_cell(score: &RowScore, preset: Option<&RulePreset>) -> String {
    let (lamp, ex) = match (score, preset) {
        (RowScore::NoBest { .. }, _) => ("-".to_owned(), "-".to_owned()),
        (RowScore::Best { ex, lamp, .. }, Some(p)) => (lamp_name(*lamp, p), ex.to_string()),
        (RowScore::Unplayed | RowScore::Best { .. }, _) => (String::new(), String::new()),
    };
    let lamp: String = lamp.chars().take(LAMP_COLUMNS).collect();
    format!("{lamp:LAMP_COLUMNS$} {ex:>5}")
}

// The library's bests fixture, declared at the file level so its path is
// relative to this file's folder, which exists on every platform.
#[cfg(test)]
#[path = "../../library/tests/common/mod.rs"]
mod common;

#[cfg(test)]
mod tests {
    use std::sync::{Arc, Mutex};

    use super::common;

    use game::{BestLamp, BestScore, BestView, RecordSink as _};
    use library::{Best, Score};

    use skin_model::{GaugeLook, OwnLamp};

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
            ..Default::default()
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

    /// The typed lamp agrees with its text in every case of
    /// [`lamp_text_follows_rank_not_vector_order`], and keeps the rank.
    #[test]
    fn own_lamp_agrees_with_lamp_text() {
        use super::{lamp_name, own_lamp};
        let p = ranked();
        let cleared_as = |gauge, rank, look| OwnLamp::Cleared { gauge, rank, look };
        for (gauges, fc, want) in [
            (&[0, 2][..], false, cleared_as(0, 2, GaugeLook::Normal)),
            (&[1, 2][..], false, cleared_as(2, 1, GaugeLook::Easy)),
            (&[0, 3][..], false, cleared_as(3, 3, GaugeLook::Hard)),
            (&[0, 2][..], true, OwnLamp::FullCombo),
            (&[][..], false, OwnLamp::Failed),
        ] {
            let best = cleared(gauges, fc);
            assert_eq!(own_lamp(&best, &p), want, "{gauges:?}");
            assert_eq!(lamp_name(want, &p), lamp_text(&best, &p), "{gauges:?}");
        }
    }

    /// A cleared lamp carries its gauge's look: each built-in preset's
    /// gauges by their ids, and a gauge of another id by its kind.
    #[test]
    fn own_lamp_carries_the_gauge_look() {
        use super::own_lamp;
        let look_of = |p: &rules::RulePreset, i: usize| match own_lamp(&cleared(&[i], false), p) {
            OwnLamp::Cleared { gauge, look, .. } => {
                assert_eq!(usize::from(gauge), i);
                look
            }
            other => panic!("gauge {i}: {other:?}"),
        };
        let looks = |p: &rules::RulePreset| -> Vec<(String, GaugeLook)> {
            p.gauges
                .iter()
                .enumerate()
                .map(|(i, g)| (g.id.clone(), look_of(p, i)))
                .collect()
        };
        let named = |pairs: &[(&str, GaugeLook)]| -> Vec<(String, GaugeLook)> {
            pairs.iter().map(|&(id, l)| (id.to_owned(), l)).collect()
        };
        let mut want = named(&[
            ("normal", GaugeLook::Normal),
            ("assist_easy", GaugeLook::AssistEasy),
            ("easy", GaugeLook::Easy),
            ("hard", GaugeLook::Hard),
            ("ex_hard", GaugeLook::ExHard),
            ("hazard", GaugeLook::Hazard),
        ]);
        let by_id = |p: &rules::RulePreset| {
            let mut v = looks(p);
            v.sort_by(|a, b| a.0.cmp(&b.0));
            v
        };
        want.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(by_id(&rules::beatoraja_style()), want);
        assert_eq!(by_id(&rules::lr2_style()), want);
        assert_eq!(
            looks(&rules::tapestry()),
            named(&[("normal", GaugeLook::Normal), ("survival", GaugeLook::Hard)])
        );
        // Other ids: by kind, an end threshold as normal, survival as hard.
        let mut custom = rules::tapestry();
        for g in &mut custom.gauges {
            g.id = format!("custom_{}", g.id);
        }
        assert_eq!(look_of(&custom, 0), GaugeLook::Normal);
        assert_eq!(look_of(&custom, 1), GaugeLook::Hard);
    }

    #[test]
    fn best_line_shows_difference_and_reason() {
        // Normal is gauge 0 of the ranked preset, hard gauge 3.
        let p = ranked();
        let score = |plays, ex, bp, lamp: &str| BestScore {
            plays,
            ex: Some(ex),
            bp: Some(bp),
            clear: Some(match lamp {
                "NORMAL" => BestLamp::Cleared { gauge: 0 },
                "HARD" => BestLamp::Cleared { gauge: 3 },
                _ => BestLamp::Failed,
            }),
        };
        let new = BestView::Counted {
            play: (1234, 10),
            previous: Some(score(4, 1222, 13, "NORMAL")),
            current: score(5, 1234, 10, "HARD"),
            ineligible: None,
        };
        assert_eq!(
            best_line(&new, &p),
            "BEST EX 1234 (+12)  BP 10 (-3)  HARD  PLAYS 5  NEW BEST"
        );
        let worse = BestView::Counted {
            play: (1200, 20),
            previous: Some(score(5, 1234, 10, "HARD")),
            current: score(6, 1234, 10, "HARD"),
            ineligible: None,
        };
        assert_eq!(
            best_line(&worse, &p),
            "BEST EX 1234 (-34)  BP 10 (+10)  HARD  PLAYS 6"
        );
        let first = BestView::Counted {
            play: (50, 2),
            previous: None,
            current: score(1, 50, 2, "FAILED"),
            ineligible: None,
        };
        assert_eq!(
            best_line(&first, &p),
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
            crate::end_screen::fit(&best_line(&midway, &p), 76),
            "BEST NOT UPDATED: ENDED MIDWAY  PLAYS 1"
        );
        let unsaved = BestView::NotCounted(library::Ineligible::NotSaved.to_string());
        assert_eq!(
            crate::end_screen::fit(&best_line(&unsaved, &p), 76),
            "BEST NOT UPDATED: NOT SAVED"
        );
    }

    #[test]
    fn chart_cells_are_fixed_width() {
        let p = ranked();
        let scored = Score {
            plays: 2,
            clears: 0,
            best: Some(cleared(&[3], false)),
        };
        let cell = |score: Option<&Score>, p| chart_cell(&super::row_score(score, p), Some(p));
        assert_eq!(cell(Some(&scored), &p), "HARD           10");
        let no_best = Score {
            plays: 2,
            clears: 0,
            best: None,
        };
        assert_eq!(cell(Some(&no_best), &p), "-               -");
        assert_eq!(cell(None, &p), " ".repeat(17));
        let mut long = p.clone();
        long.gauges[3].id = "much_harder_than_hard".to_owned();
        assert_eq!(cell(Some(&scored), &long), "MUCH_HARDER    10");
    }

    /// The sink over the fixture's records and library.
    fn sink(fx: &common::Fixture, lib: &library::Library) -> super::BestsRecords {
        super::BestsRecords::new(
            fx.records.clone(),
            Arc::new(Mutex::new(lib.best_writer().unwrap())),
        )
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
        assert_eq!(
            best_line(&view, &rules::tapestry()),
            "BEST EX 6  BP 0  FC  PLAYS 1  NEW BEST"
        );
        let view = finish(&mut sink, &common::played(30));
        assert!(
            best_line(&view, &rules::tapestry()).starts_with("BEST EX 6 (-"),
            "{view:?}"
        );
        assert!(
            best_line(&view, &rules::tapestry()).ends_with("FC  PLAYS 2"),
            "{view:?}"
        );
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

    /// A view's best before the play, read through the live writer: known
    /// empty without a row, then the counted best with its typed lamp,
    /// and known empty again under another preset's key.
    #[test]
    fn the_best_before_is_read_typed_through_the_writer() {
        use game::{BestKey, BestLookup};
        let fx = common::Fixture::new();
        let lib = fx.open();
        fx.pass(&lib);
        let mut sink = sink(&fx, &lib);
        let record = common::played(0);
        let preset = rules::tapestry();
        let key = BestKey {
            chart_sha256: record.chart.identity.sha256,
            preset_sha256: record.preset.sha256,
        };
        assert_eq!(sink.best_before(&key, &preset), BestLookup::KnownEmpty);
        finish(&mut sink, &record);
        assert_eq!(
            sink.best_before(&key, &preset),
            BestLookup::Known(BestScore {
                plays: 1,
                ex: Some(6),
                bp: Some(0),
                clear: Some(BestLamp::FullCombo),
            })
        );
        let other = BestKey {
            preset_sha256: [7; 32],
            ..key
        };
        assert_eq!(sink.best_before(&other, &preset), BestLookup::KnownEmpty);
        // A read the database refuses leaves the best unavailable.
        common::sql(&fx.db(), "DROP TABLE scores;");
        assert_eq!(sink.best_before(&key, &preset), BestLookup::Unavailable);
    }

    /// A stored score converts by its best: none or one without a best is
    /// known empty, a best carries its lamp by rank.
    #[test]
    fn stored_scores_convert_to_lookups() {
        use game::BestLookup;
        let p = ranked();
        assert_eq!(super::lookup(None, &p), BestLookup::KnownEmpty);
        let no_best = Score {
            plays: 3,
            clears: 0,
            best: None,
        };
        assert_eq!(super::lookup(Some(&no_best), &p), BestLookup::KnownEmpty);
        let scored = Score {
            plays: 3,
            clears: 0,
            best: Some(cleared(&[0, 3], false)),
        };
        assert_eq!(
            super::lookup(Some(&scored), &p),
            BestLookup::Known(BestScore {
                plays: 3,
                ex: Some(10),
                bp: Some(0),
                clear: Some(BestLamp::Cleared { gauge: 3 }),
            })
        );
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
