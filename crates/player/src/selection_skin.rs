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
//! - **The strip.** The search box with focus and a notice are drawn by
//!   the player over the skin ([`StripText`]), the IME's composition
//!   measured there as on the page; with neither, nothing is drawn over
//!   it.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::Arc;

use bms_model::TimeUs;
use game::Channel;
use render::SkinBundle;
use skin_model::{
    SelectionLabels, SelectionOrder, SelectionSettings, SelectionTimers, SelectionView,
    SpeedReference, SpeedView, VolumeReading,
};

use crate::menu_screen::{Caret, LineStyle, MenuLine, MenuPage};
use crate::screens::ScriptSlot;
use crate::tuning::LiveTuning;

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
/// with where its composition lies, and the notice.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct StripText {
    /// The search line and its caret's bytes in it, while the box has
    /// focus.
    pub search: Option<(String, usize, usize)>,
    /// The notice, if any.
    pub notice: Option<String>,
}

impl StripText {
    /// The strip of `page`: its search line while it has a caret, and its
    /// notice line when it says something.
    #[must_use]
    pub fn of(page: &MenuPage) -> Self {
        let search = page.caret.and_then(|c| {
            let line = page.lines.get(c.line)?;
            Some((line.text.clone(), c.start, c.end))
        });
        let notice = page
            .lines
            .iter()
            .find(|l| l.style == LineStyle::Notice && !l.text.is_empty())
            .map(|l| l.text.clone());
        Self { search, notice }
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
    /// Each panel's opening while it is open: START, SELECT or both held
    /// ([`SkinFeed::panels`]).
    pub panels: [Option<TimeUs>; 3],
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
}

/// `s` with the three volumes that play and the display offset as
/// `tuning` holds them now: what a selection frame shows of the settings
/// keys (F3 to F6) and the system volume, read on the render thread.
/// Allocates nothing.
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
        system_volume: VolumeReading::of_gain(tuning.volume(Channel::System)),
        key_volume: VolumeReading::of_gain(tuning.volume(Channel::Key)),
        background_volume: VolumeReading::of_gain(tuning.volume(Channel::Background)),
        ..*s
    }
}

/// The settings a selection skin shows, as the game thread knows them
/// (docs/stage3.md §3, Selection): the play options in force (`options`,
/// none before any), the chosen `preset`'s gauge shown for the settings'
/// `gauge` as an attempt resolves it and its press assignment, the
/// failure handling `on_failure`, the list's `order`, and the three
/// volumes and the display offset `tuning` holds now.
pub fn settings_of(
    options: Option<game::PlayOptions>,
    preset: Option<&rules::RulePreset>,
    gauge: usize,
    on_failure: game::FailureAction,
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
        system_volume: VolumeReading::of_gain(0.0),
        key_volume: VolumeReading::of_gain(0.0),
        background_volume: VolumeReading::of_gain(0.0),
        speed,
        order,
        gauge,
        covers_on: [covers.sudden.on, covers.lift.on, covers.hidden.on],
        play_to_end: on_failure == game::FailureAction::PlayToEnd,
        assignment: preset.map(|p| p.assignment),
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
    /// The installation's runtime, while the screen runs it
    /// (docs/stage3.md S7-07): dropped with the state, it parks.
    pub script: ScriptSlot,
    /// Why that runtime stopped, once a visit reported it: the strip's
    /// notice for as long as this state lasts, a screen built again
    /// after a device loss included (design.md "beatoraja Lua Skins").
    pub(crate) script_stop: Option<crate::screens::ScriptStop>,
    /// The pointer's point in target pixels for the next frame's hover;
    /// none outside the window. The render thread sets it each frame.
    pub pointer: Option<[f32; 2]>,
}

impl SelectionLogic {
    /// A fresh state of `bundle`'s skin, numbered `id`.
    #[must_use]
    pub fn new(bundle: &Arc<SkinBundle>, id: u64) -> Self {
        Self {
            sel: bundle.skin().prepare_selection(),
            bundle: Arc::clone(bundle),
            id,
            script: ScriptSlot::default(),
            script_stop: None,
            pointer: None,
        }
    }

    /// This state, its screen's runtime held in `script`.
    #[must_use]
    pub fn with_script(mut self, script: ScriptSlot) -> Self {
        self.script = script;
        self
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
    /// The installed selection skin's song list: its center slot and its
    /// placements; `None` without a skin.
    pub window: Option<(usize, usize)>,
    /// The settings the flow gave at the entry and at each preset change
    /// ([`settings_of`]), each update with the list's order.
    pub settings: Option<SelectionSettings>,
    /// The panel open and when it opened.
    panel: Option<(usize, TimeUs)>,
    /// The generation of the panels' level last read in the lifetime.
    seen: Option<u64>,
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
        self.seen = None;
    }

    /// The lifetime and revision of the update published last
    /// (dev-tools: a selection capture is armed with it).
    #[cfg(feature = "dev-tools")]
    #[must_use]
    pub const fn published(&self) -> (u64, u64) {
        (self.lifetime, self.revision)
    }

    /// The panels' level as the input thread published it, read at `now`
    /// with the search box's focus (`searching`): the panel START, SELECT
    /// or both open (design.md "Input", Menu input), none while the search
    /// box has focus. A panel opening, changing, held again after a change
    /// the level counted since the last read, or open at the lifetime's
    /// first read starts at `now`. `true` when the panel changed.
    pub fn panels(&mut self, level: input::Panels, searching: bool, now: TimeUs) -> bool {
        let want = if searching { None } else { level.open() };
        let again = self.seen != Some(level.generation);
        self.seen = Some(level.generation);
        let open = self.panel.map(|(p, _)| p);
        if want == open && (want.is_none() || !again) {
            return false;
        }
        self.panel = want.map(|p| (p, now));
        true
    }

    /// The panel open, if any.
    #[must_use]
    pub fn panel(&self) -> Option<usize> {
        self.panel.map(|(p, _)| p)
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
        let mut view = screen.view(before, after, false);
        view.list_revision = self.list_revision;
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
            strip: StripText::of(&page),
            page,
            view,
            labels: crate::select::labels(),
            events: SelectionEvents {
                entered: self.entered,
                changed: self.changed,
                panels: std::array::from_fn(|p| {
                    self.panel.filter(|&(open, _)| open == p).map(|(_, at)| at)
                }),
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
    /// Whether the frame drew the skin, not the built-in page: what the
    /// pointer is hit-tested on.
    pub skinned: bool,
}

/// A self-made JSON selection skin (tests): a song list of seven
/// placements around center 3, each an 8×8 image with its row's title in
/// font `f.ttf`, the place's text (1000) and the search's (30).
#[cfg(test)]
pub(crate) fn fixture() -> tempfile::TempDir {
    fixture_of("", "")
}

/// [`fixture`] with the settings drawn after it, in this order: number
/// 57 (the system volume's), number 12 (the display offset) and graph 17
/// (the system volume, 1 000 wide at y 100) (dev-tools tests).
#[cfg(all(test, feature = "dev-tools"))]
pub(crate) fn settings_fixture() -> tempfile::TempDir {
    fixture_of(
        r#""value": [{"id": "n57", "src": "s", "x": 0, "y": 0, "w": 10, "h": 1, "divx": 10, "digit": 3, "ref": 57},
                     {"id": "n12", "src": "s", "x": 0, "y": 0, "w": 10, "h": 1, "divx": 10, "digit": 3, "ref": 12}],
           "graph": [{"id": "vg", "src": "s", "x": 0, "y": 0, "w": 8, "h": 8, "type": 17, "angle": 0}],"#,
        r#",{"id": "n57", "dst": [{"x": 0, "y": 200, "w": 10, "h": 10}]},
           {"id": "n12", "dst": [{"x": 0, "y": 150, "w": 10, "h": 10}]},
           {"id": "vg", "dst": [{"x": 0, "y": 100, "w": 1000, "h": 10}]}"#,
    )
}

/// [`fixture`] with six images drawn after it, in this order: one on
/// each of options 21, 22 and 23 (panels 1, 2 and 3 open), and one on
/// each of timers 21, 22 and 23 (the panels' openings) (dev-tools tests).
#[cfg(all(test, feature = "dev-tools"))]
pub(crate) fn panels_fixture() -> tempfile::TempDir {
    fixture_of(
        "",
        r#",{"id": "b", "op": [21], "dst": [{"x": 0, "y": 300, "w": 8, "h": 8}]},
           {"id": "b", "op": [22], "dst": [{"x": 0, "y": 320, "w": 8, "h": 8}]},
           {"id": "b", "op": [23], "dst": [{"x": 0, "y": 340, "w": 8, "h": 8}]},
           {"id": "b", "timer": 21, "dst": [{"time": 0, "x": 0, "y": 360, "w": 8, "h": 8},
                                            {"time": 100000, "x": 100, "y": 360, "w": 8, "h": 8}]},
           {"id": "b", "timer": 22, "dst": [{"time": 0, "x": 0, "y": 380, "w": 8, "h": 8},
                                            {"time": 100000, "x": 100, "y": 380, "w": 8, "h": 8}]},
           {"id": "b", "timer": 23, "dst": [{"time": 0, "x": 0, "y": 400, "w": 8, "h": 8},
                                            {"time": 100000, "x": 100, "y": 400, "w": 8, "h": 8}]}"#,
    )
}

/// The fixture with `widgets` declared and `drawn` destinations after its
/// own.
#[cfg(test)]
fn fixture_of(widgets: &str, drawn: &str) -> tempfile::TempDir {
    let dir = fixture_files();
    let slots = fixture_slots();
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
    std::fs::write(dir.path().join("s.json"), json).unwrap();
    dir
}

/// A folder holding the fixtures' image `s.png` (16×16, white) and font
/// `f.ttf`.
#[cfg(test)]
fn fixture_files() -> tempfile::TempDir {
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
    dir
}

/// The fixtures' seven song list placements: slot `i` at x 1000, y-down
/// top `100 + 100·i`, 800×80.
#[cfg(test)]
fn fixture_slots() -> String {
    use std::fmt::Write as _;

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
    slots
}

/// A selection skin taking the pointer (dev-tools tests), on a 1920×1080
/// canvas whose y-down rects (left, top, width, height) are, in draw
/// order: [`fixture`]'s song list (slot `i` at 1000, `100 + 100·i`,
/// 800×80, center 3), every slot clickable, in slot order; search text 30
/// right-aligned at x 900, top 40, 400×40 (its input box 500 to 900);
/// action images with click type 0: event 12 at 100, 200, event 15 at 100,
/// 300 and event 315 at 100, 400, each 100×50; the list slider (type 1)
/// sliding right 1 000 from 100, 900, 20×40 (its travel x 100 to 1 100,
/// y 900 to 940); event 13 over that travel at 500, 905, 40×30; events 13,
/// 331 and 340 over slot 4 at 1300, 1400 and 1500, top 510, each 60×40;
/// events 330 and 332 at 100, 500 and 100, 600, each 100×50; the volume
/// sliders, each sliding right 200, 20×40: 17 (the system volume) from
/// 1050, 610 and 19 (the background volume) from 1400, 620, both over
/// slot 5, and 18 (the key volume) from 1050, 520, over slot 4; and an
/// image at 100, 1000, 179×37 drawn only while the pointer is inside its
/// region, by a `mouseRect` {0, 0, 179, 37} as a hover button's. Without
/// `volume`, no volume slider is drawn.
#[cfg(test)]
#[cfg(feature = "dev-tools")]
pub(crate) fn pointer_fixture(volume: bool) -> tempfile::TempDir {
    pointer_fixture_with(volume, false)
}

/// [`pointer_fixture`], with `holes` two bar images whose file is missing:
/// every slot's folder image (1), and slot 1's image 0, which hides that
/// slot.
#[cfg(test)]
#[cfg(feature = "dev-tools")]
pub(crate) fn pointer_fixture_with(volume: bool, holes: bool) -> tempfile::TempDir {
    let dir = fixture_files();
    let mut slots = fixture_slots();
    let (sets, missing) = if holes {
        // Slot 1's arms: the second of the seven.
        let first = slots.find(r#""id": "set""#).unwrap() + 1;
        let second = first + slots[first..].find(r#""id": "set""#).unwrap();
        slots.replace_range(second..second + 11, r#""id": "odd""#);
        (
            r#"{"id": "set", "images": ["b", "x", "b", "b", "b", "b"]},
               {"id": "odd", "images": ["x", "b"]}"#,
            r#", {"id": "m", "path": "m.png"}"#,
        )
    } else {
        (
            r#"{"id": "set", "images": ["b", "b", "b", "b", "b", "b"]}"#,
            "",
        )
    };
    // A destination of `id` whose y-down rect is `[x, top, w, h]`.
    let dst = |id: &str, [x, top, w, h]: [i32; 4], more: &str| {
        let y = 1_080 - top - h;
        format!(r#", {{"id": "{id}", {more} "dst": [{{"x": {x}, "y": {y}, "w": {w}, "h": {h}}}]}}"#)
    };
    let act = |id: &str, event: u16| {
        format!(
            r#", {{"id": "{id}", "src": "s", "x": 0, "y": 0, "w": 8, "h": 8, "act": {event}, "click": 0}}"#
        )
    };
    let images: String = [
        act("a12", 12),
        act("a13", 13),
        act("a15", 15),
        act("a315", 315),
        act("a330", 330),
        act("a331", 331),
        act("a332", 332),
        act("a340", 340),
        if holes {
            r#", {"id": "x", "src": "m", "x": 0, "y": 0, "w": 8, "h": 8}"#.to_owned()
        } else {
            String::new()
        },
    ]
    .concat();
    let drawn: String = [
        dst("q", [900, 40, 400, 40], ""),
        // A second search text, and event 13 over each box, inside it and
        // outside the other.
        dst("q2", [1_000, 0, 300, 30], ""),
        dst("a13", [600, 45, 40, 20], ""),
        dst("a13", [1_050, 5, 40, 20], ""),
        dst("a12", [100, 200, 100, 50], ""),
        dst("a15", [100, 300, 100, 50], ""),
        dst("a315", [100, 400, 100, 50], ""),
        dst("ls", [100, 900, 20, 40], ""),
        dst("a13", [500, 905, 40, 30], ""),
        dst("a13", [1_300, 510, 60, 40], ""),
        dst("a331", [1_400, 510, 60, 40], ""),
        dst("a340", [1_500, 510, 60, 40], ""),
        dst("a330", [100, 500, 100, 50], ""),
        dst("a332", [100, 600, 100, 50], ""),
        if volume {
            [
                dst("vol", [1_050, 610, 20, 40], ""),
                dst("vol18", [1_050, 520, 20, 40], ""),
                dst("vol19", [1_400, 620, 20, 40], ""),
            ]
            .concat()
        } else {
            String::new()
        },
        dst(
            "b",
            [100, 1_000, 179, 37],
            r#""mouseRect": {"x": 0, "y": 0, "w": 179, "h": 37},"#,
        ),
    ]
    .concat();
    let json = format!(
        r#"{{"type": 5, "w": 1920, "h": 1080,
            "source": [{{"id": "s", "path": "s.png"}}{missing}],
            "font": [{{"id": "f", "path": "f.ttf"}}],
            "image": [{{"id": "b", "src": "s", "x": 0, "y": 0, "w": 8, "h": 8}}{images}],
            "imageset": [{sets}],
            "slider": [{{"id": "ls", "src": "s", "x": 0, "y": 0, "w": 8, "h": 8, "type": 1, "angle": 1, "range": 1000}},
                       {{"id": "vol", "src": "s", "x": 0, "y": 0, "w": 8, "h": 8, "type": 17, "angle": 1, "range": 200, "changeable": true}},
                       {{"id": "vol18", "src": "s", "x": 0, "y": 0, "w": 8, "h": 8, "type": 18, "angle": 1, "range": 200, "changeable": true}},
                       {{"id": "vol19", "src": "s", "x": 0, "y": 0, "w": 8, "h": 8, "type": 19, "angle": 1, "range": 200, "changeable": true}}],
            "text": [{{"id": "t", "font": "f", "size": 30, "ref": 10}},
                     {{"id": "q", "font": "f", "size": 30, "ref": 30, "align": 2}},
                     {{"id": "q2", "font": "f", "size": 30, "ref": 30}}],
            "songlist": {{"id": "list", "center": 3, "liston": [{slots}], "listoff": [{slots}],
                          "clickable": [0, 1, 2, 3, 4, 5, 6],
                          "text": [{{"id": "t", "dst": [{{"x": 10, "y": 10, "w": 700, "h": 40}}]}}]}},
            "destination": [{{"id": "list", "dst": [{{"x": 0, "y": 0, "w": 0, "h": 0}}]}}{drawn}]}}"#
    );
    std::fs::write(dir.path().join("s.json"), json).unwrap();
    dir
}

/// [`fixture`] with the same skin in Lua beside it as `s.luaskin`, its
/// place and search texts read through callbacks that lower (tests).
#[cfg(test)]
pub(crate) fn lua_fixture() -> tempfile::TempDir {
    let dir = fixture();
    let lua = r#"local skin = { type = 5, name = "t", w = 1920, h = 1080 }
if not skin_config then return skin end
local ms = require("main_state")
skin.source = { { id = "s", path = "s.png" } }
skin.font = { { id = "f", path = "f.ttf" } }
skin.image = { { id = "b", src = "s", x = 0, y = 0, w = 8, h = 8 } }
skin.imageset = { { id = "set", images = { "b", "b", "b", "b", "b", "b" } } }
skin.text = {
  { id = "t", font = "f", size = 30, ref = 10 },
  { id = "place", font = "f", size = 30, value = function() return ms.text(1000) end },
  { id = "q", font = "f", size = 30, value = function() return ms.text(30) end },
}
local function slots()
  local s = {}
  for i = 0, 6 do
    s[#s + 1] = { id = "set", dst = { { x = 1000, y = 900 - i * 100, w = 800, h = 80 } } }
  end
  return s
end
skin.songlist = { id = "list", center = 3, liston = slots(), listoff = slots(),
  text = { { id = "t", dst = { { x = 10, y = 10, w = 700, h = 40 } } } } }
skin.destination = {
  { id = "list", dst = { { x = 0, y = 0, w = 0, h = 0 } } },
  { id = "place", dst = { { x = 100, y = 1000, w = 800, h = 40 } } },
  { id = "q", dst = { { x = 100, y = 950, w = 800, h = 40 } } },
}
return skin
"#;
    std::fs::write(dir.path().join("s.luaskin"), lua).unwrap();
    dir
}

/// [`lua_fixture`] with two more entries whose callbacks only the
/// screen's runtime runs (docs/stage3.md S7-07): `rt.luaskin` draws one
/// more image, at 1500, 500, while its `draw` callback (counting its
/// calls in an upvalue) returns true, and `stop.luaskin` draws it under a
/// callback that never returns (dev-tools tests).
#[cfg(test)]
#[cfg(feature = "dev-tools")]
pub(crate) fn runtime_fixture() -> tempfile::TempDir {
    let dir = lua_fixture();
    let lua = std::fs::read_to_string(dir.path().join("s.luaskin")).unwrap();
    for (name, body) in [
        ("rt.luaskin", "calls = calls + 1; return calls > 0"),
        ("stop.luaskin", "while true do end"),
        // A host read in each call: the time budget is checked at it.
        (
            "host.luaskin",
            "calls = calls + 1; return ms.number(77) >= 0",
        ),
    ] {
        let tail = format!(
            r#"local calls = 0
skin.image[#skin.image + 1] = {{ id = "rt", src = "s", x = 0, y = 0, w = 8, h = 8 }}
skin.destination[#skin.destination + 1] = {{ id = "rt",
  draw = function() {body} end,
  dst = {{ {{ x = 1500, y = 500, w = 16, h = 16 }} }} }}
return skin
"#
        );
        let text = lua.replacen("\nreturn skin\n", &format!("\n{tail}"), 1);
        std::fs::write(dir.path().join(name), text).unwrap();
    }
    dir
}

/// [`fixture_bundle`], with the runtime the import kept for the screen.
#[cfg(all(test, feature = "dev-tools"))]
pub(crate) fn fixture_scripted(
    limits: skin_model::AtlasLimits,
    dir: &std::path::Path,
    entry: &str,
) -> (Arc<SkinBundle>, ScriptSlot) {
    let choice = game::SkinChoice {
        root: dir.to_owned(),
        entry: entry.to_owned(),
        options: Default::default(),
        files: Default::default(),
    };
    let target = crate::imports::ImportTarget {
        limits,
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
        crate::imports::KindOutcome::Bundle(b, _, script) => (b, script),
        other => panic!("{other:?}"),
    }
}

/// The fixture's skin `entry` in `dir` built as the selection kind for `r`
/// (dev-tools tests).
#[cfg(all(test, feature = "dev-tools"))]
pub(crate) fn fixture_bundle(
    r: &render::Renderer,
    dir: &std::path::Path,
    entry: &str,
) -> Arc<SkinBundle> {
    let choice = game::SkinChoice {
        root: dir.to_owned(),
        entry: entry.to_owned(),
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
        crate::imports::KindOutcome::Bundle(b, ..) => b,
        other => panic!("{other:?}"),
    }
}

#[cfg(test)]
mod tests {
    use bms_model::TimeUs;

    use super::{SelectionEvents, SelectionSlot, StripText};
    use crate::select::{MenuInput, MenuKey, SelectScreen};

    /// The settings a skin shows are the play options in force, the
    /// preset's displayed gauge as an attempt resolves it, the settings
    /// keys' values and, in each update, the list's order; none before
    /// the options are known.
    #[test]
    fn the_settings_are_the_players_own() {
        use skin_model::{GaugeLook, SelectionOrder, SpeedReference, SpeedView, VolumeReading};
        let tuning = crate::tuning::LiveTuning::new(game::Tuning {
            system: game::Volume {
                percent: 53,
                intent: false,
            },
            key: game::Volume {
                percent: 20,
                intent: false,
            },
            background: game::Volume {
                percent: 99,
                intent: false,
            },
            display_offset_us: -12_000,
            input_offset_us: 0,
        });
        let hispeed = game::HiSpeedSettings {
            reference: game::HiSpeedReference::Main,
            green_target: 450,
            ..game::HiSpeedSettings::default()
        };
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
                game::FailureAction::Close,
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
            assert_eq!(s.system_volume, VolumeReading::of_gain(0.53));
            assert_eq!(s.system_volume.number, 52);
            assert!(!s.play_to_end);
            assert_eq!(s.assignment, Some(rules::Assignment::NearestTiesEarlier));
        }
        // The failure handling the files hold, and no assignment without a
        // preset.
        let s = super::settings_of(
            Some(options),
            None,
            0,
            game::FailureAction::PlayToEnd,
            SelectionOrder::Title,
            &tuning,
        )
        .unwrap();
        assert!(s.play_to_end);
        assert_eq!(s.assignment, None);
        assert_eq!(
            super::settings_of(
                None,
                Some(&preset),
                0,
                game::FailureAction::Close,
                SelectionOrder::Title,
                &tuning
            ),
            None
        );
        let s = super::settings_of(
            Some(options),
            None,
            0,
            game::FailureAction::Close,
            SelectionOrder::Title,
            &tuning,
        );
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
        assert_eq!(StripText::of(&s.page("")).page().lines.len(), 0);
        s.input(MenuInput::Key(MenuKey::Tab));
        s.input(MenuInput::Text("abc".to_owned()));
        s.input(MenuInput::Preedit("かな".to_owned()));
        s.set_notice(Some("refused".to_owned()));
        let strip = StripText::of(&s.page(""));
        let page = strip.page();
        assert_eq!(page.lines.len(), 2);
        let caret = page.caret.unwrap();
        let line = &page.lines[0].text;
        assert_eq!(&line[caret.start..caret.end], "かな");
        assert!(line[..caret.start].ends_with("abc"));
        assert_eq!(page.lines[1].text, "refused");
    }
}
