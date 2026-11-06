//! The select screen's two presentations (design.md "Rendering and
//! Skins", Screen skins): every change of the screen makes one
//! [`SelectionUpdate`], which carries both the built-in page and what a
//! selection skin draws (the view, the player's labels, the strip's text
//! and the timers' starts), so either draws at once whichever is in use.
//!
//! - **The slot.** Updates go through a [`SelectionSlot`] holding the
//!   newest only, with at most one notification outstanding: the game
//!   thread replaces the value and signals only when no signal waits, and
//!   the render thread clears the flag before it takes the value, so one
//!   update and one message exist whatever the key rate.
//! - **Lifetimes.** Each entry into selection is a lifetime; its updates
//!   are numbered. The render thread takes only a newer `(lifetime,
//!   revision)`, and leaving selection empties the slot and ends the
//!   lifetime, so no update of an ended lifetime is drawn, one taken
//!   before the end included (the render thread checks again after its
//!   frame wait).
//! - **Logical state and resources.** The skin's runtime state for a
//!   lifetime ([`SelectionLogic`]: its timers' and `when` starts, fields
//!   and strings) is the render thread's, apart from the renderer: made
//!   fresh at each entry, kept across a renderer built again, whose text
//!   is prepared again over it.
//! - **The strip.** The search box with focus, a notice, and for
//!   [`PRESET_SHOWN`] after a preset step the preset's name are drawn by
//!   the player over the skin ([`StripText`]), the IME's composition
//!   measured there as on the page.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;

use bms_model::TimeUs;
use render::SkinBundle;
use skin_model::{
    MasterVolume, SelectionLabels, SelectionOrder, SelectionSettings, SelectionTimers,
    SelectionView, SpeedReference, SpeedView,
};

use crate::menu_screen::{Caret, LineStyle, MenuLine, MenuPage};
use crate::tuning::LiveTuning;

/// How long the strip shows the preset's name after a preset step.
pub const PRESET_SHOWN: TimeUs = TimeUs(3_000_000);

/// What one change of the select screen gives the render thread.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SelectionUpdate {
    /// The entry into selection it belongs to, from 1.
    pub lifetime: u64,
    /// Its number within the lifetime.
    pub revision: u64,
    /// The lists that arrived (and orders that changed) within the
    /// lifetime.
    pub list_revision: u64,
    /// The built-in page: drawn without a skin, and when a skin fails.
    pub page: MenuPage,
    /// The rows around the cursor that the skin's song list places.
    pub view: SelectionView,
    /// The player's own strings the skin draws.
    pub labels: SelectionLabels,
    /// What the strip says.
    pub strip: StripText,
    /// The timers' starts.
    pub events: SelectionEvents,
    /// The player's settings the skin shows, as the game thread knows
    /// them; each frame takes the volume and the display offset as they
    /// are then ([`live_settings`]). `None` until the flow gives them.
    pub settings: Option<SelectionSettings>,
}

impl SelectionUpdate {
    /// Whether this update is newer than `other`: a later lifetime, or a
    /// later revision of the same one.
    #[must_use]
    pub const fn newer_than(&self, other: &Self) -> bool {
        self.lifetime > other.lifetime
            || (self.lifetime == other.lifetime && self.revision > other.revision)
    }
}

/// The strip's text: the search line while the search box has focus,
/// with where its composition lies, the notice, and the chosen preset's
/// name.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StripText {
    /// The search line and its caret's bytes in it, while the box has
    /// focus.
    pub search: Option<(String, usize, usize)>,
    /// The notice, if any.
    pub notice: Option<String>,
    /// The chosen preset's name.
    pub preset: String,
}

impl StripText {
    /// The strip of `page` (its search line while it has a caret, and its
    /// notice line when it says something) and `preset`.
    #[must_use]
    pub fn of(page: &MenuPage, preset: &str) -> Self {
        let search = page.caret.and_then(|c| {
            let line = page.lines.get(c.line)?;
            Some((line.text.clone(), c.start, c.end))
        });
        let notice = page
            .lines
            .iter()
            .find(|l| l.style == LineStyle::Notice && !l.text.is_empty())
            .map(|l| l.text.clone());
        Self {
            search,
            notice,
            preset: preset.to_owned(),
        }
    }

    /// The strip's lines as a page, top down: the search line with its
    /// caret, then the notice.
    #[must_use]
    pub fn page(&self) -> MenuPage {
        let mut lines = Vec::with_capacity(2);
        let mut caret = None;
        if let Some((text, start, end)) = &self.search {
            caret = Some(Caret {
                line: 0,
                start: *start,
                end: *end,
            });
            lines.push(MenuLine {
                text: text.clone(),
                style: LineStyle::Search,
            });
        }
        if let Some(notice) = &self.notice {
            lines.push(MenuLine {
                text: notice.clone(),
                style: LineStyle::Notice,
            });
        }
        MenuPage { lines, caret }
    }
}

/// When the select screen's timers started, on the clock the game and
/// render threads share.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct SelectionEvents {
    /// The entry into selection: timer `screen`.
    pub entered: TimeUs,
    /// The entry, or the cursor's row or the place changing since: timer
    /// `change`.
    pub changed: TimeUs,
    /// Each panel's opening while it is open (none yet).
    pub panels: [Option<TimeUs>; 3],
    /// The latest preset step the flow applied in the lifetime.
    pub preset_step: Option<TimeUs>,
}

impl SelectionEvents {
    /// The skin's built-in timers. Allocates nothing.
    #[must_use]
    pub const fn timers(&self) -> SelectionTimers {
        SelectionTimers {
            screen: Some(self.entered),
            change: Some(self.changed),
            panels: self.panels,
        }
    }

    /// Whether the strip shows the preset's name at `now`: from a preset
    /// step until [`PRESET_SHOWN`] after it. Allocates nothing.
    #[must_use]
    pub const fn preset_shown(&self, now: TimeUs) -> bool {
        match self.preset_step {
            Some(t) => now.0 >= t.0 && now.0 - t.0 < PRESET_SHOWN.0,
            None => false,
        }
    }
}

/// `s` with the master gain that plays and the display offset as
/// `tuning` holds them now: what a selection frame shows of the settings
/// keys (F3 to F6), read on the render thread. Allocates nothing.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
#[must_use]
pub fn live_settings(s: &SelectionSettings, tuning: &LiveTuning) -> SelectionSettings {
    let offset = tuning.display_offset_us() / 1_000;
    SelectionSettings {
        display_offset_ms: i32::try_from(offset).unwrap_or(if offset < 0 {
            i32::MIN
        } else {
            i32::MAX
        }),
        master_volume: MasterVolume::of_gain(tuning.volume()),
        ..*s
    }
}

/// The settings a selection skin shows, as the game thread knows them
/// (docs/stage3.md §3, Selection): the play options in force (`options`,
/// none before any), the chosen `preset`'s gauge shown for the settings'
/// `gauge` as an attempt resolves it, the list's `order`, and the volume
/// and the display offset `tuning` holds now.
pub fn settings_of(
    options: Option<game::PlayOptions>,
    preset: Option<&rules::RulePreset>,
    gauge: usize,
    order: SelectionOrder,
    tuning: &LiveTuning,
) -> Option<SelectionSettings> {
    let options = options?;
    let hispeed = options.hispeed();
    let speed = match hispeed.mode {
        game::SpeedMode::Classic => SpeedView::Classic,
        game::SpeedMode::Floating => SpeedView::Floating {
            reference: match hispeed.reference {
                game::HiSpeedReference::Start => SpeedReference::Start,
                game::HiSpeedReference::Maximum => SpeedReference::Maximum,
                game::HiSpeedReference::Main => SpeedReference::Main,
                game::HiSpeedReference::Minimum => SpeedReference::Minimum,
            },
            green: hispeed.green_target,
        },
    };
    let covers = options.covers();
    let gauge = preset.and_then(|p| {
        let policy = game::DefaultPolicy {
            displayed: gauge,
            on_failure: game::FailureAction::default(),
        };
        p.gauges
            .get(game::resolve_displayed(&policy, p).0)
            .map(skin_model::GaugeLook::of)
    });
    let s = SelectionSettings {
        display_offset_ms: 0,
        master_volume: MasterVolume::of_gain(0.0),
        speed,
        order,
        gauge,
        covers_on: [covers.sudden.on, covers.lift.on, covers.hidden.on],
    };
    Some(live_settings(&s, tuning))
}

/// The newest update and the lifetime it may belong to, between the game
/// and render threads.
#[derive(Debug, Default)]
pub struct SelectionSlot {
    value: Option<Box<SelectionUpdate>>,
    /// A notification was sent and the value not taken since.
    notified: bool,
    /// The lifetime current, 0 outside selection.
    active: u64,
}

impl SelectionSlot {
    /// Puts `update` in the slot, replacing the one not taken; `true`
    /// when the render thread must be told (no notification outstanding).
    /// An update of a lifetime not current is dropped.
    pub fn offer(&mut self, update: SelectionUpdate) -> bool {
        if update.lifetime == 0 || update.lifetime != self.active {
            return false;
        }
        self.value = Some(Box::new(update));
        !std::mem::replace(&mut self.notified, true)
    }

    /// The update waiting, taken; the notification is over either way.
    pub fn take(&mut self) -> Option<Box<SelectionUpdate>> {
        self.notified = false;
        self.value.take()
    }

    /// Lifetime `lifetime` begins: nothing of another is kept.
    pub fn enter(&mut self, lifetime: u64) {
        self.active = lifetime;
        self.value = None;
    }

    /// Selection is left: the slot empties and no lifetime is current.
    pub fn leave(&mut self) {
        self.active = 0;
        self.value = None;
    }

    /// Whether `lifetime` is the current one. Allocates nothing: the
    /// render thread asks it right before a selection frame's build.
    #[must_use]
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub const fn current(&self, lifetime: u64) -> bool {
        lifetime != 0 && self.active == lifetime
    }
}

/// The selection skin's runtime state for one lifetime and installation,
/// on the render thread: made fresh by its skin
/// ([`skin_model::Skin::prepare_selection`]), filled by each update.
#[derive(Debug)]
pub struct SelectionLogic {
    /// The installation's bundle it was made by.
    pub bundle: Arc<SkinBundle>,
    /// The state.
    pub sel: skin_model::Selection,
    /// Its number, never another state's: text prepared over another
    /// state is prepared again.
    pub id: u64,
}

impl SelectionLogic {
    /// A fresh state of `bundle`'s skin, numbered `id`.
    #[must_use]
    pub fn new(bundle: &Arc<SkinBundle>, id: u64) -> Self {
        Self {
            sel: bundle.skin().prepare_selection(),
            bundle: Arc::clone(bundle),
            id,
        }
    }

    /// Applies `settings` (none before the flow gives them) to the state,
    /// with the volume and the display offset as `tuning` holds them now
    /// when there is one: once a frame, so F3 to F6 show at the next one
    /// with no view. Allocates nothing.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    pub fn apply_settings(
        &mut self,
        settings: Option<&SelectionSettings>,
        tuning: Option<&LiveTuning>,
    ) {
        let Some(s) = settings else {
            return;
        };
        let s = tuning.map_or(*s, |t| live_settings(s, t));
        // Another skin's state is refused alike by the build, which
        // draws nothing of it.
        let _ = self
            .bundle
            .skin()
            .update_selection_settings(&mut self.sel, &s);
    }
}

/// What restarts timer `change` when it changes: the place, the cursor's
/// row, the order and the query (whose results arrive with it).
type ChangeKey = (
    skin_model::SelectionPlace,
    Option<skin_model::RowKey>,
    usize,
    String,
);

/// The select screen's updates as the game thread makes them: the
/// lifetime and its numbers, the timers' starts, and the installed
/// skin's song list, which the view is made for.
#[derive(Debug, Default)]
pub struct SkinFeed {
    /// The current lifetime, 0 before the first entry.
    pub lifetime: u64,
    revision: u64,
    /// The lists that arrived in the lifetime.
    pub list_revision: u64,
    entered: TimeUs,
    changed: TimeUs,
    key: Option<ChangeKey>,
    /// The latest preset step the flow applied in the lifetime.
    pub preset_step: Option<TimeUs>,
    /// The installed selection skin's song list: its center slot and its
    /// placements; `None` without a skin.
    pub window: Option<(usize, usize)>,
    /// The settings the flow gave at the entry and at each preset change
    /// ([`settings_of`]), each update with the list's order.
    pub settings: Option<SelectionSettings>,
}

impl SkinFeed {
    /// Selection is entered at `now`: a new lifetime, its timers starting
    /// there.
    pub fn enter(&mut self, now: TimeUs) {
        self.lifetime = self.lifetime.saturating_add(1);
        self.revision = 0;
        self.list_revision = 0;
        self.entered = now;
        self.changed = now;
        self.key = None;
        self.preset_step = None;
    }

    /// The update of `screen` showing `page` at `now`: its view the rows
    /// around the cursor the skin's song list places, `center` above and
    /// the rest below, with no heading (none without a skin); timer
    /// `change` restarts when the place, the cursor's row, the order or
    /// the query changed since the last update.
    pub fn update(
        &mut self,
        screen: &crate::select::SelectScreen,
        page: MenuPage,
        now: TimeUs,
    ) -> SelectionUpdate {
        let (before, after) = self.window.map_or((0, 0), |(center, slots)| {
            (center, slots.saturating_sub(1).saturating_sub(center))
        });
        let view = screen.view(before, after, false);
        let row = view
            .cursor
            .and_then(|c| view.rows.get(c))
            .map(|r| r.key.clone());
        let key = (view.place.clone(), row, screen.order(), view.query.clone());
        if self.key.as_ref().is_some_and(|k| *k != key) {
            self.changed = now;
        }
        self.key = Some(key);
        self.revision = self.revision.saturating_add(1);
        SelectionUpdate {
            lifetime: self.lifetime,
            revision: self.revision,
            list_revision: self.list_revision,
            strip: StripText::of(&page, screen.preset()),
            page,
            view,
            labels: crate::select::labels(),
            events: SelectionEvents {
                entered: self.entered,
                changed: self.changed,
                panels: [None; 3],
                preset_step: self.preset_step,
            },
            settings: self.settings.map(|s| SelectionSettings {
                order: screen.order_view(),
                ..s
            }),
        }
    }
}

/// What a selection frame did: its timing, and the skin's refusal when
/// it refused (its frame or its text), the page drawn instead.
#[derive(Clone, Debug)]
pub struct SelectionDrawn {
    pub timing: render::FrameTiming,
    pub refused: Option<String>,
}

/// A self-made JSON selection skin (tests): a song list of seven
/// placements around center 3, each an 8×8 image with its row's title in
/// font `f.ttf`, the place's text (1000) and the search's (30).
#[cfg(test)]
pub(crate) fn fixture() -> tempfile::TempDir {
    fixture_of(false)
}

/// [`fixture`] with the settings drawn after it, in this order: number
/// 57 (the master gain's), number 12 (the display offset) and graph 17
/// (the master gain, 1 000 wide at y 100) (dev-tools tests).
#[cfg(all(test, feature = "dev-tools"))]
pub(crate) fn settings_fixture() -> tempfile::TempDir {
    fixture_of(true)
}

#[cfg(test)]
fn fixture_of(settings: bool) -> tempfile::TempDir {
    use std::fmt::Write as _;

    let dir = tempfile::tempdir().unwrap();
    let d = dir.path();
    let file = std::fs::File::create(d.join("s.png")).unwrap();
    let mut e = png::Encoder::new(std::io::BufWriter::new(file), 16, 16);
    e.set_color(png::ColorType::Rgba);
    e.set_depth(png::BitDepth::Eight);
    let white = vec![255_u8; 16 * 16 * 4];
    e.write_header().unwrap().write_image_data(&white).unwrap();
    let font = crate::font_support::build(&crate::font_support::cjk("F"));
    std::fs::write(d.join("f.ttf"), font).unwrap();
    let mut slots = String::new();
    for i in 0..7 {
        let sep = if i == 0 { "" } else { "," };
        let y = 900 - i * 100;
        write!(
            slots,
            r#"{sep}{{"id": "set", "dst": [{{"x": 1000, "y": {y}, "w": 800, "h": 80}}]}}"#
        )
        .unwrap();
    }
    let (widgets, drawn) = if settings {
        (
            r#""value": [{"id": "n57", "src": "s", "x": 0, "y": 0, "w": 10, "h": 1, "divx": 10, "digit": 3, "ref": 57},
                         {"id": "n12", "src": "s", "x": 0, "y": 0, "w": 10, "h": 1, "divx": 10, "digit": 3, "ref": 12}],
               "graph": [{"id": "vg", "src": "s", "x": 0, "y": 0, "w": 8, "h": 8, "type": 17, "angle": 0}],"#,
            r#",{"id": "n57", "dst": [{"x": 0, "y": 200, "w": 10, "h": 10}]},
               {"id": "n12", "dst": [{"x": 0, "y": 150, "w": 10, "h": 10}]},
               {"id": "vg", "dst": [{"x": 0, "y": 100, "w": 1000, "h": 10}]}"#,
        )
    } else {
        ("", "")
    };
    let json = format!(
        r#"{{"type": 5, "w": 1920, "h": 1080,
            "source": [{{"id": "s", "path": "s.png"}}],
            "font": [{{"id": "f", "path": "f.ttf"}}],
            "image": [{{"id": "b", "src": "s", "x": 0, "y": 0, "w": 8, "h": 8}}],
            "imageset": [{{"id": "set", "images": ["b", "b", "b", "b", "b", "b"]}}], {widgets}
            "text": [{{"id": "t", "font": "f", "size": 30, "ref": 10}},
                     {{"id": "place", "font": "f", "size": 30, "ref": 1000}},
                     {{"id": "q", "font": "f", "size": 30, "ref": 30}}],
            "songlist": {{"id": "list", "center": 3, "liston": [{slots}], "listoff": [{slots}],
                          "text": [{{"id": "t", "dst": [{{"x": 10, "y": 10, "w": 700, "h": 40}}]}}]}},
            "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}},
                            {{"id": "place", "dst": [{{"x": 100, "y": 1000, "w": 800, "h": 40}}]}},
                            {{"id": "q", "dst": [{{"x": 100, "y": 950, "w": 800, "h": 40}}]}}{drawn}]}}"#
    );
    std::fs::write(d.join("s.json"), json).unwrap();
    dir
}

/// [`fixture`] built as the selection kind for `r` (dev-tools tests).
#[cfg(all(test, feature = "dev-tools"))]
pub(crate) fn fixture_bundle(r: &render::Renderer, dir: &std::path::Path) -> Arc<SkinBundle> {
    let choice = game::SkinChoice {
        root: dir.to_owned(),
        entry: "s.json".to_owned(),
        options: Default::default(),
        files: Default::default(),
    };
    let target = crate::imports::ImportTarget {
        limits: r.atlas_limits(),
        size: [1_920, 1_080],
        default_font: || Ok(crate::render_outline::default_font()),
    };
    let cancel = skin_beatoraja::Cancel::new();
    match crate::imports::build(
        skin_model::ScreenKind::Selection,
        &choice,
        false,
        &target,
        &cancel,
    ) {
        crate::imports::KindOutcome::Bundle(b, _) => b,
        other => panic!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use bms_model::TimeUs;

    use super::{SelectionEvents, SelectionSlot, StripText};
    use crate::select::{MenuInput, MenuKey, SelectScreen};

    /// The preset's name shows from its step until 3 s after it, not at
    /// 3 s; before any step, never.
    #[test]
    fn the_preset_name_shows_for_three_seconds() {
        let t = 5_000_000;
        let e = SelectionEvents {
            preset_step: Some(TimeUs(t)),
            ..SelectionEvents::default()
        };
        assert!(e.preset_shown(TimeUs(t)));
        assert!(e.preset_shown(TimeUs(t + 2_999_999)));
        assert!(!e.preset_shown(TimeUs(t + 3_000_000)));
        assert!(!e.preset_shown(TimeUs(t - 1)));
        assert!(!SelectionEvents::default().preset_shown(TimeUs(t)));
    }

    /// The settings a skin shows are the play options in force, the
    /// preset's displayed gauge as an attempt resolves it, the settings
    /// keys' values and, in each update, the list's order; none before
    /// the options are known.
    #[test]
    fn the_settings_are_the_players_own() {
        use skin_model::{GaugeLook, MasterVolume, SelectionOrder, SpeedReference, SpeedView};
        let tuning = crate::tuning::LiveTuning::new(game::Tuning {
            volume_percent: 53,
            display_offset_us: -12_000,
            input_offset_us: 0,
        });
        let mut hispeed = game::HiSpeedSettings::default();
        hispeed.reference = game::HiSpeedReference::Main;
        hispeed.green_target = 450;
        let mut covers = game::Covers::default();
        covers.lift.on = true;
        let options = game::PlayOptions::new(hispeed, covers).unwrap();
        let preset = rules::tapestry();
        // A gauge past the preset's resolves as an attempt does.
        for gauge in [0, 1, 99] {
            let policy = game::DefaultPolicy {
                displayed: gauge,
                on_failure: game::FailureAction::default(),
            };
            let shown = &preset.gauges[game::resolve_displayed(&policy, &preset).0];
            let s = super::settings_of(
                Some(options),
                Some(&preset),
                gauge,
                SelectionOrder::Title,
                &tuning,
            )
            .unwrap();
            assert_eq!(s.gauge, Some(GaugeLook::of(shown)), "{gauge}");
            assert_eq!(
                s.speed,
                SpeedView::Floating {
                    reference: SpeedReference::Main,
                    green: 450
                }
            );
            assert_eq!(s.covers_on, [covers.sudden.on, true, covers.hidden.on]);
            assert_eq!(s.display_offset_ms, -12);
            // 53 % plays as the f32 0.53, which the source shows as 52.
            assert_eq!(s.master_volume, MasterVolume::of_gain(0.53));
            assert_eq!(s.master_volume.number, 52);
        }
        assert_eq!(
            super::settings_of(None, Some(&preset), 0, SelectionOrder::Title, &tuning),
            None
        );
        let s = super::settings_of(Some(options), None, 0, SelectionOrder::Title, &tuning);
        assert_eq!(s.unwrap().gauge, None);
        // The update takes the list's order.
        let mut feed = super::SkinFeed {
            settings: s,
            ..super::SkinFeed::default()
        };
        feed.enter(TimeUs(0));
        let mut screen = SelectScreen::new(Vec::new());
        screen.input(MenuInput::Key(MenuKey::Right));
        let page = screen.page("");
        let u = feed.update(&screen, page, TimeUs(0));
        assert_eq!(u.settings.unwrap().order, SelectionOrder::Artist);
    }

    /// The slot keeps the newest of a lifetime with one notification,
    /// and nothing of a lifetime not current.
    #[test]
    fn the_slot_keeps_the_newest_with_one_notification() {
        let update = |lifetime, revision| super::SelectionUpdate {
            lifetime,
            revision,
            list_revision: 0,
            page: crate::menu_screen::MenuPage::default(),
            view: skin_model::SelectionView::default(),
            labels: crate::select::labels(),
            strip: StripText::default(),
            events: SelectionEvents::default(),
            settings: None,
        };
        let mut slot = SelectionSlot::default();
        assert!(!slot.offer(update(1, 1)));
        slot.enter(1);
        assert!(slot.offer(update(1, 1)));
        for r in 2..=100 {
            assert!(!slot.offer(update(1, r)));
        }
        assert_eq!(slot.take().map(|u| u.revision), Some(100));
        assert!(slot.offer(update(1, 101)));
        slot.leave();
        assert!(slot.take().is_none());
        assert!(!slot.offer(update(1, 102)));
        assert!(!slot.current(1));
    }

    /// The strip shows the search line only while the box has focus, with
    /// its composition, and the notice when there is one.
    #[test]
    fn the_strip_shows_the_search_with_focus_and_the_notice() {
        let mut s = SelectScreen::new(Vec::new());
        assert_eq!(StripText::of(&s.page(""), "p").page().lines.len(), 0);
        s.input(MenuInput::Key(MenuKey::Tab));
        s.input(MenuInput::Text("abc".to_owned()));
        s.input(MenuInput::Preedit("かな".to_owned()));
        s.set_notice(Some("refused".to_owned()));
        let strip = StripText::of(&s.page(""), "p");
        let page = strip.page();
        assert_eq!(page.lines.len(), 2);
        let caret = page.caret.unwrap();
        let line = &page.lines[0].text;
        assert_eq!(&line[caret.start..caret.end], "かな");
        assert!(line[..caret.start].ends_with("abc"));
        assert_eq!(page.lines[1].text, "refused");
        assert_eq!(strip.preset, "p");
    }
}
