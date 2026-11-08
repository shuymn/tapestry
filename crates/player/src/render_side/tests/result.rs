//! The result screen's presentations on a real screen, on WARP through the
//! render thread (the S10-09i design's C2, C5a, C5c, C10, C12, C17c, C17d,
//! C19 and C22): what each frame shows is one view's, the barrier keeps
//! the view presented, a refusal draws the built-in page in its frame and
//! disables the kind for the run, and a screen built again draws on from
//! the revision presented.

use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bms_model::TimeUs;
use game::Preparation;
use input::QpcClock;
use skin_model::{AtlasLimits, Quad, ResultChart, ResultLabels, ScreenKind};

use super::{measure, view, wait_until};
use crate::menu_screen::MenuScreen;
use crate::render_side::{EndPage, MakeScreen, NoFrames, RenderLink, Screen, spawn};
use crate::result_present::{Overlay, ResultPresentation, ViewKey, ViewPayload};

/// What a frame drew: the skin's quads and the strip's lines, or the
/// built-in page's lines.
#[derive(Clone, Debug, PartialEq)]
enum Drawn {
    Skin(Vec<Quad>, usize),
    Page(Vec<String>),
}

/// The frames' pacing: while `pace` is set each frame wait takes one of
/// `permits`, holding until there is one.
#[derive(Default)]
struct Gate {
    pace: AtomicBool,
    permits: AtomicU32,
    waits: AtomicU32,
    held: AtomicBool,
}

impl Gate {
    fn hold(&self) {
        self.pace.store(true, Ordering::Release);
        assert!(wait_until(|| self.held.load(Ordering::Acquire)));
    }

    /// One frame, then held again in the next wait.
    fn step(&self) {
        let before = self.waits.load(Ordering::Acquire);
        self.permits.store(1, Ordering::Release);
        assert!(wait_until(|| self.waits.load(Ordering::Acquire) > before
            && self.held.load(Ordering::Acquire)
            && self.permits.load(Ordering::Acquire) == 0));
    }

    fn release(&self) {
        self.pace.store(false, Ordering::Release);
    }
}

/// Faults a test turns on.
#[derive(Default)]
struct Faults {
    /// The next result frame submitted fails as a lost device.
    lose: AtomicBool,
    /// The next result frame is refused by the skin.
    refuse: AtomicBool,
    /// A screen built again refuses to restore the result's installation.
    refuse_restore: AtomicBool,
    /// The next result text prepared is refused by the skin.
    refuse_text: AtomicBool,
    /// The next built-in page drawn fails as a lost device.
    lose_page: AtomicBool,
    /// The next frame wait times out.
    time_out: AtomicBool,
    /// The next strip prepared for a view prepared fails as a lost device.
    fail_strip: AtomicBool,
    /// The next chart load fails as a lost device.
    fail_load: AtomicBool,
    /// The next play selection releases the result's installation, then
    /// fails as a lost device (as play's admission releases it first).
    release_then_fail: AtomicBool,
    /// The next built-in page drawn panics.
    panic_page: AtomicBool,
    /// The next installation of the result kind panics.
    panic_install: AtomicBool,
    /// The next chart load (an attempt's preparation) releases the
    /// result's installation, then fails as a lost device.
    release_then_fail_load: AtomicBool,
    /// The screen's drop holds while set; `dropping` says it reached it.
    hold_drop: AtomicBool,
    dropping: AtomicBool,
}

impl Drop for Recorded {
    fn drop(&mut self) {
        self.faults.dropping.store(true, Ordering::Release);
        while self.faults.hold_drop.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(1));
        }
    }
}

/// A WARP menu screen that records what each result frame drew.
struct Recorded {
    menu: MenuScreen,
    gate: Arc<Gate>,
    faults: Arc<Faults>,
    drawn: Arc<Mutex<Vec<Drawn>>>,
}

impl Screen for Recorded {
    fn load(
        &mut self,
        chart: &bms_model::Chart,
        section: Option<&eval::Section>,
    ) -> Result<(), render::RenderError> {
        if self.faults.fail_load.swap(false, Ordering::AcqRel) {
            return Err(render::RenderError::SurfaceLost);
        }
        if self
            .faults
            .release_then_fail_load
            .swap(false, Ordering::AcqRel)
        {
            self.menu.remove(ScreenKind::Result);
            return Err(render::RenderError::SurfaceLost);
        }
        self.menu.load(chart, section)
    }
    fn device(&self) -> u64 {
        self.menu.device()
    }
    fn commit(&mut self, token: render::PreparedToken) -> Result<(), render::CommitError> {
        self.menu.commit(token)
    }
    fn discard(&mut self) {
        self.menu.discard();
    }
    fn install(
        &mut self,
        kind: ScreenKind,
        skin: &Arc<render::SkinBundle>,
        request: u64,
    ) -> Result<Preparation, render::RenderError> {
        assert!(
            !(kind == ScreenKind::Result
                && self.faults.panic_install.swap(false, Ordering::AcqRel)),
            "the render thread panics in the upload (a test's fault)"
        );
        if kind == ScreenKind::Play && self.faults.release_then_fail.swap(false, Ordering::AcqRel) {
            self.menu.remove(ScreenKind::Result);
            return Err(render::RenderError::SurfaceLost);
        }
        self.menu.install(kind, skin, request)
    }
    fn installation(&self, kind: ScreenKind) -> Option<render::Installation> {
        self.menu.installation(kind)
    }
    fn restore(
        &mut self,
        kind: ScreenKind,
        installation: &render::Installation,
    ) -> Result<(), render::RenderError> {
        if kind == ScreenKind::Result && self.faults.refuse_restore.load(Ordering::Acquire) {
            return Err(render::RenderError::Skin(render::SkinError {
                reason: "the restore was refused".to_owned(),
            }));
        }
        self.menu.restore(kind, installation)
    }
    fn remove(&mut self, kind: ScreenKind) {
        self.menu.remove(kind);
    }
    fn install_pages(
        &mut self,
        kind: ScreenKind,
        pages: &Arc<skin_model::PackedPages>,
        request: u64,
    ) -> Result<Preparation, render::RenderError> {
        self.menu.install_pages(kind, pages, request)
    }
    fn restore_pages(
        &mut self,
        kind: ScreenKind,
        pages: &Arc<skin_model::PackedPages>,
    ) -> Result<(), render::RenderError> {
        if kind == ScreenKind::Result && self.faults.refuse_restore.load(Ordering::Acquire) {
            return Err(render::RenderError::Skin(render::SkinError {
                reason: "the restore was refused".to_owned(),
            }));
        }
        self.menu.restore_pages(kind, pages)
    }
    fn disable_result(&mut self) {
        self.menu.disable_result();
    }
    fn drain_retired(&mut self) -> Option<skin_model::AtlasId> {
        self.menu.drain_retired()
    }
    fn installed_pages(&self, kind: ScreenKind) -> Option<Arc<skin_model::PackedPages>> {
        self.menu.installed_pages(kind)
    }
    fn retained_atlases(&self, kind: ScreenKind) -> [Option<skin_model::AtlasId>; 3] {
        self.menu.retained_atlases(kind)
    }
    fn prepare_result(
        &mut self,
        skin: &Arc<render::SkinBundle>,
        state: &mut skin_model::ResultScreen,
        view: &skin_model::ResultView,
        labels: &ResultLabels,
    ) -> Result<Result<(), String>, render::RenderError> {
        if self.faults.refuse_text.swap(false, Ordering::AcqRel) {
            self.menu.refuse_next_result(true);
        }
        self.menu.prepare_result(skin, state, view, labels)
    }
    fn install_result_text(&mut self) -> bool {
        self.menu.install_result_text()
    }
    fn prepare_result_strip(
        &mut self,
        which: usize,
        lines: &[&str],
    ) -> Result<(), render::RenderError> {
        if which == crate::render_side::STRIP_PREPARED
            && self.faults.fail_strip.swap(false, Ordering::AcqRel)
        {
            return Err(render::RenderError::SurfaceLost);
        }
        self.menu.prepare_result_strip(which, lines)
    }
    fn use_result_strip(&mut self, which: usize) {
        self.menu.use_result_strip(which);
    }
    fn build_result(
        &mut self,
        state: &mut skin_model::ResultScreen,
        frame: &skin_model::ResultFrame<'_>,
    ) -> Result<(), skin_model::BuildError> {
        if self.faults.refuse.swap(false, Ordering::AcqRel) {
            self.menu.refuse_next_result(false);
        }
        self.menu.build_result(state, frame)
    }
    fn submit_result(&mut self) -> Result<render::FrameTiming, render::RenderError> {
        if self.faults.lose.swap(false, Ordering::AcqRel) {
            return Err(render::RenderError::SurfaceLost);
        }
        let t = self.menu.submit_result()?;
        let quads = self.menu.result_quads().unwrap_or_default();
        let strip = self.menu.result_strip();
        self.drawn.lock().unwrap().push(Drawn::Skin(quads, strip));
        Ok(t)
    }
    fn end(&mut self, page: &EndPage) -> Result<bool, render::RenderError> {
        if self.faults.lose_page.swap(false, Ordering::AcqRel) {
            return Err(render::RenderError::SurfaceLost);
        }
        assert!(
            !self.faults.panic_page.swap(false, Ordering::AcqRel),
            "the render thread panics (a test's fault)"
        );
        let p = self.menu.end(page)?;
        self.drawn
            .lock()
            .unwrap()
            .push(Drawn::Page(page.lines.clone()));
        Ok(p)
    }
    fn wait(&self) -> bool {
        let gate = &self.gate;
        gate.waits.fetch_add(1, Ordering::AcqRel);
        while gate.pace.load(Ordering::Acquire) {
            if gate
                .permits
                .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                .is_ok()
            {
                break;
            }
            gate.held.store(true, Ordering::Release);
            std::thread::sleep(Duration::from_millis(1));
        }
        gate.held.store(false, Ordering::Release);
        if self.faults.time_out.swap(false, Ordering::AcqRel) {
            return false;
        }
        !self.menu.paced() || self.menu.wait()
    }
    fn paced(&self) -> bool {
        self.gate.pace.load(Ordering::Acquire) || self.menu.paced()
    }
    fn build(&mut self, f: &render::FrameInput<'_>) -> Result<(), skin_model::BuildError> {
        self.menu.build(f)
    }
    fn draw(&mut self) -> Result<render::FrameTiming, render::RenderError> {
        self.menu.draw()
    }
    fn text(&mut self, lines: &[&str]) -> Result<bool, render::RenderError> {
        self.menu.text(lines)
    }
    fn resize(&mut self, width: u32, height: u32) {
        self.menu.resize(width, height);
    }
    fn target(&self) -> [u32; 2] {
        self.menu.target()
    }
    fn atlas_limits(&self) -> Option<AtlasLimits> {
        self.menu.atlas_limits()
    }
    fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
        self.menu.read_back()
    }
}

/// A render thread on recorded WARP screens.
pub(crate) struct Rig {
    link: RenderLink,
    gate: Arc<Gate>,
    faults: Arc<Faults>,
    drawn: Arc<Mutex<Vec<Drawn>>>,
}

impl Rig {
    pub(crate) fn new() -> Self {
        let gate = Arc::new(Gate::default());
        let faults = Arc::new(Faults::default());
        let drawn = Arc::new(Mutex::new(Vec::new()));
        let (g, f, d) = (Arc::clone(&gate), Arc::clone(&faults), Arc::clone(&drawn));
        let make: MakeScreen = Box::new(move || {
            let r = render::Renderer::new(
                render::Target::Offscreen {
                    width: 1_920,
                    height: 1_080,
                },
                render::AdapterChoice::Warp,
            )?;
            Ok(Box::new(Recorded {
                menu: MenuScreen::new(r, Some(render::text::Fonts::runtime()?)),
                gate: Arc::clone(&g),
                faults: Arc::clone(&f),
                drawn: Arc::clone(&d),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        Self {
            link,
            gate,
            faults,
            drawn,
        }
    }

    fn drawn(&self) -> Vec<Drawn> {
        self.drawn.lock().unwrap().clone()
    }

    fn clear(&self) {
        self.drawn.lock().unwrap().clear();
    }

    /// Waits until the last frame drew `want`.
    fn until(&self, want: impl Fn(&Drawn) -> bool) -> bool {
        wait_until(|| self.drawn.lock().unwrap().last().is_some_and(&want))
    }
}

/// A fixed Lua result skin (it reads no result fact): its EX number, the
/// marker every view draws differently, and a status image.
pub(crate) fn fixed_skin() -> (tempfile::TempDir, Arc<render::SkinBundle>) {
    lua_skin("", "")
}

/// [`fixed_skin`] with `texts` texts of the chart's title (text 10) in a
/// bitmap font of one glyph, `A`: a view's title is drawn as its glyphs.
pub(crate) fn text_skin(texts: usize) -> (tempfile::TempDir, Arc<render::SkinBundle>) {
    let elements: String = (0..texts)
        .map(|i| format!("{{ id = \"t{i}\", font = \"f\", size = 100, ref = 10 }},"))
        .collect();
    let places: String = (0..texts)
        .map(|i| {
            format!(
                "{{ id = \"t{i}\", dst = {{ {{ x = {}, y = {}, w = 200, h = 10 }} }} }},",
                (i % 8) * 220,
                500 + (i / 8) * 12
            )
        })
        .collect();
    lua_skin(
        &format!(
            "skin.font = {{ {{ id = \"f\", path = \"f.fnt\" }} }}\n\
             skin.text = {{ {elements} }}\n"
        ),
        &places,
    )
}

/// A fixed Lua result skin as [`fixed_skin`], with `head` before its
/// destinations and `places` among them; the bitmap font `f.fnt` beside
/// it.
fn lua_skin(head: &str, places: &str) -> (tempfile::TempDir, Arc<render::SkinBundle>) {
    let d = tempfile::tempdir().unwrap();
    let image = |name: &str, w: u32, h: u32| {
        let file = std::fs::File::create(d.path().join(name)).unwrap();
        let mut e = png::Encoder::new(std::io::BufWriter::new(file), w, h);
        e.set_color(png::ColorType::Rgba);
        e.set_depth(png::BitDepth::Eight);
        let data: Vec<u8> = (0..w * h).flat_map(|_| [9, 9, 9, 255]).collect();
        e.write_header().unwrap().write_image_data(&data).unwrap();
    };
    image("s.png", 64, 16);
    image("f.png", 128, 128);
    std::fs::write(
        d.path().join("f.fnt"),
        "info face=\"solid\" size=100 padding=0,0,0,0\n\
         common lineHeight=100 base=100 scaleW=128 scaleH=128 pages=1\n\
         page id=0 file=\"f.png\"\n\
         char id=65 x=0 y=0 width=100 height=100 xoffset=0 yoffset=0 xadvance=100 \
         page=0 chnl=15\n",
    )
    .unwrap();
    std::fs::write(
        d.path().join("s.luaskin"),
        format!(
            "local skin = {{ type = 7, name = \"t\", w = 1920, h = 1080 }}\n\
             if not skin_config then return skin end\n\
             skin.source = {{ {{ id = \"s\", path = \"s.png\" }} }}\n\
             skin.image = {{ {{ id = \"dot\", src = \"s\", x = 0, y = 0, w = 8, h = 8 }} }}\n\
             skin.value = {{ {{ id = \"ex\", src = \"s\", x = 0, y = 8, w = 40, h = 8, divx = 10, \
                              digit = 4, ref = 71 }} }}\n\
             {head}\
             skin.destination = {{\n\
               {{ id = \"ex\", dst = {{ {{ x = 0, y = 100, w = 4, h = 8 }} }} }},\n\
               {{ id = \"dot\", op = {{ 90 }}, dst = {{ {{ x = 0, y = 300, w = 8, h = 8 }} }} }},\n\
               {places}\n\
             }}\n\
             return skin\n"
        ),
    )
    .unwrap();
    let choice = game::SkinChoice {
        root: d.path().to_owned(),
        entry: "s.luaskin".to_owned(),
        options: Default::default(),
        files: Default::default(),
    };
    let target = crate::imports::ImportTarget {
        limits: AtlasLimits {
            max_side: 4_096,
            max_layers: 16,
            reserved_layers: render::RESERVED_LAYERS,
        },
        size: [1_920, 1_080],
        default_font: || Ok(crate::render_outline::default_font()),
    };
    // The importer's tests count its VMs and jobs: one import at a time
    // with them.
    let serial = crate::imports::tests::serial();
    let built = crate::imports::build(
        ScreenKind::Result,
        &choice,
        false,
        &target,
        &skin_beatoraja::Cancel::new(),
    );
    drop(serial);
    match built {
        crate::imports::KindOutcome::Bundle(b, _) => (d, b),
        other => panic!("{other:?}"),
    }
}

/// A view payload `key` with EX score `ex`, its view lines `lines` and a
/// built-in page whose view rows say `v<key>`.
fn payload(
    key: ViewKey,
    skin: Option<&Arc<render::SkinBundle>>,
    ex: u32,
    lines: &[&str],
) -> Arc<ViewPayload> {
    titled(key, skin, ex, lines, "")
}

/// [`payload`] of a chart titled `title`.
fn titled(
    key: ViewKey,
    skin: Option<&Arc<render::SkinBundle>>,
    ex: u32,
    lines: &[&str],
    title: &str,
) -> Arc<ViewPayload> {
    let mut view = crate::result_skin::probe_view(ResultChart::default());
    view.score.tally.ex_score = ex;
    Arc::new(ViewPayload {
        key,
        skin: skin.map(Arc::clone),
        why: None,
        view,
        labels: ResultLabels {
            title: title.to_owned(),
            ..ResultLabels::default()
        },
        lines: lines.iter().map(|l| (*l).to_owned()).collect(),
        page: EndPage {
            lines: (0..25).map(|i| format!("v{key} {i}")).collect(),
            graphs: None,
        },
    })
}

/// Sends `payload` as revision `revision` of `lifetime` with `overlay`.
fn show(
    link: &RenderLink,
    lifetime: u64,
    revision: u64,
    payload: &Arc<ViewPayload>,
    overlay: &Overlay,
) {
    link.show_result(ResultPresentation {
        lifetime,
        revision,
        request: link.next_request(),
        payload: Arc::clone(payload),
        overlay: overlay.clone(),
    });
}

/// The skin's own quads of a frame, the strip's bands left out.
fn skin_quads(q: &[Quad]) -> Vec<Quad> {
    q.iter().filter(|q| q.layer != 0).copied().collect()
}

fn skin_frame(d: &Drawn) -> Option<(&[Quad], usize)> {
    match d {
        Drawn::Skin(q, n) => Some((q, *n)),
        Drawn::Page(_) => None,
    }
}

/// Installs the fixed skin as the result kind's and begins a lifetime.
fn installed(rig: &Rig, skin: &Arc<render::SkinBundle>) -> u64 {
    assert_eq!(
        rig.link
            .select(ScreenKind::Result, Some(Arc::clone(skin)))
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    rig.link.begin_result()
}

/// While views switch fast, every frame draws one view's number and that
/// view's strip lines, never parts of two, and the newest view is drawn
/// in the end (C2; C3 under one bundle; C12's bands).
#[test]
fn every_frame_draws_one_views_number_and_strip() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    let a = payload(1, Some(&skin), 1_111, &[]);
    let b = payload(
        2,
        Some(&skin),
        2_222,
        &["VIEWING B", "NO INPUT AFTER 1.000 S"],
    );
    let overlay = Overlay::default();
    show(&rig.link, lifetime, 1, &a, &overlay);
    assert!(rig.until(|d| skin_frame(d).is_some()));
    let Some(Drawn::Skin(ref_a, 0)) = rig.drawn().last().cloned() else {
        panic!("{:?}", rig.drawn().last());
    };
    show(&rig.link, lifetime, 2, &b, &overlay);
    assert!(rig.until(|d| skin_frame(d).is_some_and(|(_, n)| n == 2)));
    let Some(Drawn::Skin(ref_b, 2)) = rig.drawn().last().cloned() else {
        panic!();
    };
    assert_ne!(ref_a, ref_b, "the views' numbers differ");
    rig.clear();
    for i in 0..20_u64 {
        let p = if i % 2 == 0 { &a } else { &b };
        show(&rig.link, lifetime, 3 + i, p, &overlay);
        std::thread::sleep(Duration::from_millis(3));
    }
    // The last shown, b, is drawn in the end.
    assert!(rig.until(|d| *d == Drawn::Skin(ref_b.clone(), 2)));
    // The status follows the frame.
    assert!(
        wait_until(|| rig.link.result_status().shown == Some((2, true))),
        "{:?}",
        rig.link.result_status()
    );
    for d in rig.drawn() {
        let (q, n) = skin_frame(&d).expect("only the skin draws");
        assert!(
            (q == ref_a.as_slice() && n == 0) || (q == ref_b.as_slice() && n == 2),
            "a frame of two views: {n} lines"
        );
    }
    let status = rig.link.result_status();
    assert_eq!(status.lifetime, lifetime);
    assert_eq!(status.installed, Some(skin.atlas_id()));
    // An overlay alone (a notice) merges over the view presented.
    let notice = Overlay {
        lines: vec!["NOTICE".to_owned()],
        ..Overlay::default()
    };
    show(&rig.link, lifetime, 30, &b, &notice);
    assert!(rig.until(|d| {
        skin_frame(d).is_some_and(|(q, n)| n == 3 && skin_quads(q) == skin_quads(&ref_b))
    }));
    assert_eq!(rig.link.result_status().revision, 30);
    rig.link.exit();
}

/// The barrier keeps the view presented when it is set, a newer view
/// prepared before the frame's wait included; only the overlay merges,
/// its notice drawn and the newer view's own line not (C5a, C5c). A frame
/// the skin refuses then draws the built-in page in that frame, with the
/// pinned view's rows and the newest page's notice, and disables the
/// result kind for the run (C17c).
#[test]
fn the_barrier_keeps_the_view_and_a_refusal_draws_its_page() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    let a = payload(1, Some(&skin), 1_111, &[]);
    let b = payload(2, Some(&skin), 2_222, &["NO INPUT AFTER 1.000 S"]);
    show(&rig.link, lifetime, 1, &a, &Overlay::default());
    assert!(rig.until(|d| skin_frame(d).is_some()));
    let Some(Drawn::Skin(ref_a, 0)) = rig.drawn().last().cloned() else {
        panic!();
    };
    // b is taken and prepared before the next frame's wait, held there.
    rig.gate.hold();
    show(&rig.link, lifetime, 2, &b, &Overlay::default());
    rig.gate.step();
    assert_eq!(rig.link.pin_result(TimeUs(5)), Some(1));
    let notice = Overlay {
        lines: vec!["NOTICE".to_owned()],
        fadeout: Some(TimeUs(5)),
        ..Overlay::default()
    };
    show(&rig.link, lifetime, 3, &b, &notice);
    rig.clear();
    rig.gate.release();
    assert!(rig.until(|d| skin_frame(d).is_some_and(|(_, n)| n == 1)));
    std::thread::sleep(Duration::from_millis(100));
    for d in rig.drawn() {
        let (q, n) = skin_frame(&d).expect("the skin draws");
        assert_eq!(skin_quads(q), ref_a, "the pinned view's number");
        assert!(n <= 1, "never the newer view's line");
    }
    let status = rig.link.result_status();
    assert_eq!(status.pinned, Some(1));
    assert_eq!(status.shown, Some((1, true)));
    // The newest page carries the notice in a row of its own.
    let mut latest: Vec<String> = (0..25).map(|i| format!("n {i}")).collect();
    latest[5] = "NOTICE ROW".to_owned();
    rig.link.show_end(EndPage {
        lines: latest,
        graphs: None,
    });
    assert!(rig.link.fence(Duration::from_secs(10)));
    rig.faults.refuse.store(true, Ordering::Release);
    assert!(rig.until(|d| matches!(d, Drawn::Page(_))));
    let Some(Drawn::Page(page)) = rig.drawn().last().cloned() else {
        panic!();
    };
    assert_eq!(page[2], "v1 2", "the pinned view's rows");
    assert_eq!(page[5], "NOTICE ROW");
    assert!(
        page.iter().any(|l| l.starts_with("RESULT SKIN NOT SHOWN")),
        "{page:?}"
    );
    assert!(wait_until(|| rig.link.result_status().disabled.is_some()));
    // Disabled for the run: a selection is refused, and later views draw
    // the built-in page.
    assert!(matches!(
        rig.link
            .select(ScreenKind::Result, Some(Arc::clone(&skin)))
            .wait()
            .unwrap(),
        Preparation::Refused(_)
    ));
    rig.clear();
    let c = payload(3, Some(&skin), 3_333, &[]);
    show(&rig.link, lifetime, 4, &c, &Overlay::default());
    std::thread::sleep(Duration::from_millis(100));
    assert!(rig.drawn().iter().all(|d| matches!(d, Drawn::Page(_))));
    rig.link.exit();
}

/// A screen built again during a pinned outro draws the same revision
/// again, its number and strip, the lifetime going on (C22); one whose
/// result restore is refused disables the kind and draws the pinned
/// view's built-in page (C17d, C23's render half).
#[test]
fn a_screen_built_again_draws_the_revision_presented() {
    for refuse in [false, true] {
        let (_dir, skin) = fixed_skin();
        let rig = Rig::new();
        let lifetime = installed(&rig, &skin);
        let a = payload(1, Some(&skin), 1_111, &["VIEWING A"]);
        show(&rig.link, lifetime, 1, &a, &Overlay::default());
        assert!(rig.until(|d| skin_frame(d).is_some()));
        let Some(Drawn::Skin(ref_a, 1)) = rig.drawn().last().cloned() else {
            panic!();
        };
        assert_eq!(rig.link.pin_result(TimeUs(5)), Some(1));
        let revision = rig.link.result_status().revision;
        rig.faults.refuse_restore.store(refuse, Ordering::Release);
        rig.faults.lose.store(true, Ordering::Release);
        assert!(wait_until(|| rig.link.failed()));
        rig.clear();
        rig.link.prepare().unwrap();
        if refuse {
            assert!(rig.until(|d| matches!(d, Drawn::Page(p) if p[2] == "v1 2")));
            let why = rig.link.result_status().disabled.unwrap();
            assert!(why.contains("not restored"), "{why}");
        } else {
            assert!(rig.until(|d| *d == Drawn::Skin(ref_a.clone(), 1)));
            let status = rig.link.result_status();
            assert_eq!((status.revision, status.lifetime), (revision, lifetime));
            assert_eq!(status.pinned, Some(1));
        }
        rig.link.exit();
    }
}

/// The tests that hold the result's uploads, one at a time: the hold is
/// the process's.
static HOLDS: Mutex<()> = Mutex::new(());

/// An upload held on the render thread is withdrawn at once when its
/// pending selection is dropped, and once released installs nothing: the
/// status names no installation and retains nothing of it (C10's render
/// half).
#[test]
fn a_withdrawn_upload_installs_nothing_when_released() {
    let _held = HOLDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    crate::dev::hold_result_uploads(true);
    let pending = rig
        .link
        .upload_result(crate::result_skin::Upload::Pages(Arc::clone(
            skin.atlas_pages(),
        )));
    assert!(wait_until(crate::dev::result_upload_holding));
    let at = Instant::now();
    drop(pending);
    assert!(
        at.elapsed() < Duration::from_millis(50),
        "withdrawn at once"
    );
    crate::dev::hold_result_uploads(false);
    assert!(rig.link.fence(Duration::from_secs(10)));
    let status = rig.link.result_status();
    assert_eq!(status.installed, None);
    assert!(!status.retained.contains(&skin.atlas_id()), "{status:?}");
    // The positive control: the same upload answered installs its pages.
    let pending = rig
        .link
        .upload_result(crate::result_skin::Upload::Pages(Arc::clone(
            skin.atlas_pages(),
        )));
    let answer = pending.wait();
    assert_eq!(answer.unwrap(), Preparation::Ready);
    assert!(rig.link.fence(Duration::from_secs(10)));
    assert_eq!(rig.link.result_status().installed, Some(skin.atlas_id()));
    rig.link.exit();
}

/// A layout staged on the pages installed by the upload draws with no
/// upload of its own: the pages-only installation, then a view of a
/// bundle on the same atlas, drawn (C8's player half, D2's after-commit
/// step with P).
#[test]
fn a_view_draws_on_the_pages_uploaded() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let pending = rig
        .link
        .upload_result(crate::result_skin::Upload::Pages(Arc::clone(
            skin.atlas_pages(),
        )));
    let answer = pending.wait();
    assert_eq!(answer.unwrap(), Preparation::Ready);
    let lifetime = rig.link.begin_result();
    show(
        &rig.link,
        lifetime,
        1,
        &payload(1, Some(&skin), 42, &[]),
        &Overlay::default(),
    );
    assert!(rig.until(|d| skin_frame(d).is_some_and(|(q, _)| !q.is_empty())));
    let status = rig.link.result_status();
    assert_eq!(status.installed, Some(skin.atlas_id()));
    assert_eq!(status.retained, [skin.atlas_id()]);
    rig.link.exit();
}

/// A presentation of an ended lifetime is not drawn: the result left, a
/// late one draws nothing (C4 on the render thread).
#[test]
fn an_ended_lifetimes_presentation_is_not_drawn() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    rig.link.end_result();
    show(
        &rig.link,
        lifetime,
        1,
        &payload(1, Some(&skin), 1, &[]),
        &Overlay::default(),
    );
    assert!(rig.link.fence(Duration::from_secs(10)));
    std::thread::sleep(Duration::from_millis(100));
    assert!(rig.drawn().is_empty(), "{:?}", rig.drawn());
    rig.link.exit();
}

/// The view presented shown again while another is prepared, its frame's
/// wait timed out, is the newest: the one prepared is dropped, never
/// committed after it (C2's newest drawn).
#[test]
fn the_view_presented_again_drops_the_one_prepared() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    let a = payload(1, Some(&skin), 1_111, &[]);
    let b = payload(2, Some(&skin), 2_222, &["VIEWING B"]);
    show(&rig.link, lifetime, 1, &a, &Overlay::default());
    assert!(rig.until(|d| skin_frame(d).is_some()));
    let Some(Drawn::Skin(ref_a, 0)) = rig.drawn().last().cloned() else {
        panic!("{:?}", rig.drawn().last());
    };
    // b is prepared and held at the next frame's wait, which times out.
    rig.gate.hold();
    show(&rig.link, lifetime, 2, &b, &Overlay::default());
    rig.gate.step();
    rig.faults.time_out.store(true, Ordering::Release);
    show(&rig.link, lifetime, 3, &a, &Overlay::default());
    rig.gate.step();
    rig.clear();
    rig.gate.release();
    assert!(rig.until(|d| skin_frame(d).is_some()));
    std::thread::sleep(Duration::from_millis(100));
    for d in rig.drawn() {
        assert_eq!(d, Drawn::Skin(ref_a.clone(), 0), "only a draws");
    }
    assert_eq!(rig.link.result_status().shown, Some((1, true)));
    rig.link.exit();
}

/// A view committed on the pages the upload installed is what a screen
/// built again restores: its layout draws again, the kind not disabled
/// (the S10-09i design's D2 and D4, "Recovery").
#[test]
fn a_view_on_uploaded_pages_is_restored_with_its_layout() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    assert!(presented_on(
        &rig,
        crate::result_skin::Upload::Pages(Arc::clone(skin.atlas_pages())),
        &skin
    ));
    let Some(Drawn::Skin(before, _)) = rig.drawn().last().cloned() else {
        panic!("{:?}", rig.drawn().last());
    };
    rig.faults.lose.store(true, Ordering::Release);
    assert!(wait_until(|| rig.link.failed()));
    rig.clear();
    rig.link.prepare().unwrap();
    assert!(
        rig.until(|d| { skin_frame(d).is_some_and(|(q, _)| skin_quads(q) == skin_quads(&before)) })
    );
    let status = rig.link.result_status();
    assert_eq!(status.disabled, None);
    assert_eq!(status.installed, Some(skin.atlas_id()));
    rig.link.exit();
}

/// Each view committed on another atlas (a union) becomes what the kind
/// keeps: the atlas it replaced is released once drained, so only the one
/// presented is retained, and a screen built again draws the newest (D2,
/// §2.1 Q3).
#[test]
fn each_union_releases_the_atlas_it_replaces() {
    let skins: Vec<_> = (0..3).map(|_| fixed_skin()).collect();
    let rig = Rig::new();
    let lifetime = installed(&rig, &skins[0].1);
    let mut last = Vec::new();
    for (i, (_, skin)) in skins.iter().enumerate() {
        let key = u64::try_from(i).unwrap() + 1;
        let ex = 1_000 * u32::try_from(key).unwrap();
        show(
            &rig.link,
            lifetime,
            key,
            &payload(key, Some(skin), ex, &[]),
            &Overlay::default(),
        );
        assert!(
            wait_until(|| {
                let s = rig.link.result_status();
                s.shown == Some((key, true)) && s.retained == [skin.atlas_id()]
            }),
            "{:?}",
            rig.link.result_status()
        );
        assert_eq!(rig.link.result_status().installed, Some(skin.atlas_id()));
        assert!(rig.until(|d| skin_frame(d).is_some()));
        let Some(Drawn::Skin(q, _)) = rig.drawn().last().cloned() else {
            panic!();
        };
        last = q;
    }
    rig.faults.lose.store(true, Ordering::Release);
    assert!(wait_until(|| rig.link.failed()));
    rig.clear();
    rig.link.prepare().unwrap();
    assert!(
        rig.until(|d| { skin_frame(d).is_some_and(|(q, _)| skin_quads(q) == skin_quads(&last)) })
    );
    let status = rig.link.result_status();
    assert_eq!(status.retained, [skins[2].1.atlas_id()]);
    assert_eq!(status.disabled, None);
    rig.link.exit();
}

/// The first presentation's text refused on a healthy device draws the
/// built-in page of that very view, its rows and the reason, in the frame
/// it would have drawn, and disables the kind (C17c): never an empty page.
#[test]
fn a_refused_first_presentation_draws_its_own_page() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    rig.faults.refuse_text.store(true, Ordering::Release);
    show(
        &rig.link,
        lifetime,
        1,
        &payload(1, Some(&skin), 1_111, &[]),
        &Overlay::default(),
    );
    assert!(wait_until(|| !rig.drawn().is_empty()));
    let Some(Drawn::Page(page)) = rig.drawn().first().cloned() else {
        panic!("{:?}", rig.drawn().first());
    };
    assert_eq!(page.get(2).map(String::as_str), Some("v1 2"), "{page:?}");
    assert!(
        page.iter().any(|l| l.starts_with("RESULT SKIN NOT SHOWN")),
        "{page:?}"
    );
    assert!(wait_until(|| rig.link.result_status().disabled.is_some()));
    assert_eq!(rig.link.result_status().shown, Some((1, false)));
    rig.link.exit();
}

/// A newer view's text refused on a healthy device, while a view is
/// presented, draws the built-in page of the view presented, never the
/// refused one's rows, and disables the kind (D1, C17c).
#[test]
fn a_refused_later_view_draws_the_presented_views_page() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    show(
        &rig.link,
        lifetime,
        1,
        &payload(1, Some(&skin), 1_111, &[]),
        &Overlay::default(),
    );
    assert!(rig.until(|d| skin_frame(d).is_some()));
    rig.faults.refuse_text.store(true, Ordering::Release);
    show(
        &rig.link,
        lifetime,
        2,
        &payload(2, Some(&skin), 2_222, &[]),
        &Overlay::default(),
    );
    assert!(rig.until(|d| matches!(d, Drawn::Page(_))));
    std::thread::sleep(Duration::from_millis(100));
    for d in rig.drawn() {
        if let Drawn::Page(page) = d {
            assert_eq!(page[2], "v1 2", "the presented view's rows: {page:?}");
        }
    }
    assert!(rig.link.result_status().disabled.is_some());
    rig.link.exit();
}

/// A result restore refused on a screen built again before any result was
/// presented disables the kind for the run all the same (C17d): a later
/// selection of it is refused.
#[test]
fn a_restore_refused_before_any_result_disables_the_kind() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    assert_eq!(
        rig.link
            .select(ScreenKind::Result, Some(Arc::clone(&skin)))
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    rig.faults.refuse_restore.store(true, Ordering::Release);
    rig.faults.lose_page.store(true, Ordering::Release);
    rig.link.show_end(EndPage {
        lines: vec!["page".to_owned()],
        graphs: None,
    });
    assert!(wait_until(|| rig.link.failed()));
    rig.link.prepare().unwrap();
    let why = rig
        .link
        .result_status()
        .disabled
        .expect("disabled for the run");
    assert!(why.contains("not restored"), "{why}");
    rig.faults.refuse_restore.store(false, Ordering::Release);
    assert!(matches!(
        rig.link
            .select(ScreenKind::Result, Some(Arc::clone(&skin)))
            .wait()
            .unwrap(),
        Preparation::Refused(_)
    ));
    rig.link.exit();
}

/// While views switch fast between two skin views and a built-in one,
/// every frame draws one view's number, text and strip, or one view's
/// built-in rows, never parts of two (C2's text and built-in rows).
#[test]
fn every_frame_draws_one_views_text_and_rows() {
    let (_dir, skin) = text_skin(1);
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    let a = titled(1, Some(&skin), 1_111, &[], "A");
    let b = titled(2, Some(&skin), 2_222, &["VIEWING B"], "AAA");
    let c = payload(3, None, 0, &["LINE C"]);
    show(&rig.link, lifetime, 1, &a, &Overlay::default());
    assert!(rig.until(|d| skin_frame(d).is_some()));
    let Some(Drawn::Skin(ref_a, 0)) = rig.drawn().last().cloned() else {
        panic!("{:?}", rig.drawn().last());
    };
    show(&rig.link, lifetime, 2, &b, &Overlay::default());
    assert!(rig.until(|d| skin_frame(d).is_some_and(|(_, n)| n == 1)));
    let Some(Drawn::Skin(ref_b, 1)) = rig.drawn().last().cloned() else {
        panic!();
    };
    // The title's glyphs: one for A, three for B, besides the numbers.
    assert_eq!(
        skin_quads(&ref_b).len(),
        skin_quads(&ref_a).len() + 2,
        "the views' texts differ"
    );
    rig.clear();
    for i in 0..30_u64 {
        let p = [&a, &b, &c][usize::try_from(i % 3).unwrap()];
        show(&rig.link, lifetime, 3 + i, p, &Overlay::default());
        std::thread::sleep(Duration::from_millis(3));
    }
    // The last shown, c, draws its built-in page in the end.
    assert!(rig.until(|d| matches!(d, Drawn::Page(_))));
    let mut pages = 0;
    for d in rig.drawn() {
        match d {
            Drawn::Skin(q, n) => assert!(
                (q == ref_a && n == 0) || (q == ref_b && n == 1),
                "a skin frame of two views: {n} lines"
            ),
            Drawn::Page(p) => {
                pages += 1;
                for row in [1, 2, 3, 4, 8, 9, 10, 11, 13, 14, 15] {
                    assert_eq!(p[row], format!("v3 {row}"), "{p:?}");
                }
            }
        }
    }
    assert!(pages > 0);
    rig.link.exit();
}

/// A pages upload of other pages releases the installed result first; if
/// it is then withdrawn (or refused), the kind keeps nothing, not the
/// installation it released (the record read from the screen): nothing
/// is reported installed or retained, and a screen built again restores
/// nothing of it.
#[test]
fn a_withdrawn_pages_replacement_leaves_nothing_installed() {
    let _held = HOLDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_a_dir, a) = fixed_skin();
    let (_b_dir, b) = fixed_skin();
    let rig = Rig::new();
    assert_eq!(
        rig.link
            .select(ScreenKind::Result, Some(Arc::clone(&a)))
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    assert_eq!(rig.link.result_status().installed, Some(a.atlas_id()));
    crate::dev::hold_result_uploads(true);
    let pending = rig
        .link
        .upload_result(crate::result_skin::Upload::Pages(Arc::clone(
            b.atlas_pages(),
        )));
    assert!(wait_until(crate::dev::result_upload_holding));
    drop(pending);
    crate::dev::hold_result_uploads(false);
    assert!(rig.link.fence(Duration::from_secs(10)));
    let status = rig.link.result_status();
    assert_eq!(status.installed, None, "{status:?}");
    assert!(!status.retained.contains(&a.atlas_id()), "{status:?}");
    assert!(!status.retained.contains(&b.atlas_id()), "{status:?}");
    rig.link.exit();
}

/// Play's staging that released the result's installation and then fails
/// on the device leaves the kind with nothing to restore: the record is
/// read from the screen before it is dropped.
#[test]
fn a_release_before_a_device_error_is_kept() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    assert_eq!(
        rig.link
            .select(ScreenKind::Result, Some(Arc::clone(&skin)))
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    rig.faults.release_then_fail.store(true, Ordering::Release);
    assert!(
        rig.link
            .select(ScreenKind::Play, Some(Arc::clone(&skin)))
            .wait()
            .is_err()
    );
    assert_eq!(rig.link.result_status().installed, None);
    rig.link.prepare().unwrap();
    let status = rig.link.result_status();
    assert_eq!(status.installed, None, "{status:?}");
    assert_eq!(status.retained, [], "{status:?}");
    rig.link.exit();
}

/// An attempt's preparation that released the result's installation and
/// then fails on the device leaves the kind with nothing to restore: the
/// record is read from the screen before it is dropped.
#[test]
fn a_release_before_an_attempts_device_error_is_kept() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    assert_eq!(
        rig.link
            .select(ScreenKind::Result, Some(Arc::clone(&skin)))
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    let chart = Arc::new(bms_model::ChartBuilder::new(120_000).build().unwrap());
    rig.link.load(Arc::clone(&chart), None).unwrap();
    rig.faults
        .release_then_fail_load
        .store(true, Ordering::Release);
    assert!(
        rig.link
            .prepare_attempt(super::attempt(&chart, None), || false)
            .is_err()
    );
    assert_eq!(rig.link.result_status().installed, None);
    rig.link.prepare().unwrap();
    let status = rig.link.result_status();
    assert_eq!(status.installed, None, "{status:?}");
    assert_eq!(status.retained, [], "{status:?}");
    rig.link.exit();
}

/// The page a new lifetime's first presentation carries is the end page
/// exactly as it is sent, its settings lines included, through the real
/// `present_view`: its first fallback frame, before that page arrives,
/// draws every one of its rows.
#[test]
fn a_new_lifetimes_page_base_has_its_settings_lines() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let faults = Arc::clone(&rig.faults);
    let drawn = Arc::clone(&rig.drawn);
    let Rig { link, .. } = rig;
    let settings = game::Settings {
        device: game::DeviceSettings::default(),
        player: game::PlayerSettings::default(),
    };
    let dir = tempfile::tempdir().unwrap();
    crate::tests::on_rig_with(settings, link, |p, _| {
        p.result_skin = crate::result_skin::ResultSkin::new(Some(
            crate::result_skin::ResultStart::Fixed(Arc::clone(&skin)),
        ));
        let chart = ResultChart::default();
        let _ = p.result_skin.image_stage(chart, None, || false, || false);
        let report = crate::end_screen::tests::report(rules::tapestry());
        let screen = crate::end_screen::tests::screen(dir.path(), &report);
        let _ = p
            .result_skin
            .request(crate::result_skin::view_facts(&screen, chart));
        p.result_chart = Some(chart);
        let base = EndPage {
            lines: (0..20).map(|i| format!("base {i}")).collect(),
            graphs: None,
        };
        p.end_page = Some(base.clone());
        faults.refuse_text.store(true, Ordering::Release);
        p.present_view(&report, &screen);
        let sent =
            crate::end_screen::with_settings(&base, p.controls.tuning().get(), p.speed_line());
        assert!(
            sent.lines.len() > base.lines.len(),
            "settings lines: {sent:?}"
        );
        assert!(wait_until(|| drawn
            .lock()
            .unwrap()
            .iter()
            .any(|d| matches!(d, Drawn::Page(_)))));
        let page = drawn
            .lock()
            .unwrap()
            .iter()
            .find_map(|d| match d {
                Drawn::Page(p) => Some(p.clone()),
                Drawn::Skin(..) => None,
            })
            .unwrap();
        for (i, want) in sent.lines.iter().enumerate() {
            assert!(
                page[i] == *want || page[i].starts_with("RESULT SKIN NOT SHOWN"),
                "row {i}: {:?}, sent {want:?}",
                page[i]
            );
        }
    });
}

/// A result restore refused by a screen built again whose chart load then
/// fails still disables the kind for the run: the next build, which has
/// nothing of it left to restore, does not forget it.
#[test]
fn a_restore_refusal_stands_when_the_build_fails() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    assert_eq!(
        rig.link
            .select(ScreenKind::Result, Some(Arc::clone(&skin)))
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    let chart = Arc::new(bms_model::ChartBuilder::new(120_000).build().unwrap());
    rig.link.load(Arc::clone(&chart), None).unwrap();
    rig.faults.lose_page.store(true, Ordering::Release);
    rig.link.show_end(EndPage {
        lines: vec!["page".to_owned()],
        graphs: None,
    });
    assert!(wait_until(|| rig.link.failed()));
    rig.faults.refuse_restore.store(true, Ordering::Release);
    rig.faults.fail_load.store(true, Ordering::Release);
    // The screen built again with the chart: the result's restore
    // refused, then the chart's load fails.
    assert!(rig.link.load(Arc::clone(&chart), None).is_err());
    rig.faults.refuse_restore.store(false, Ordering::Release);
    rig.link.load(chart, None).unwrap();
    let why = rig
        .link
        .result_status()
        .disabled
        .expect("disabled for the run");
    assert!(why.contains("not restored"), "{why}");
    rig.link.exit();
}

/// A view presented (A) with a newer one prepared (B) when the screen
/// fails, and the result's restore refused on the screen built again:
/// the built-in page drawn is A's, the one presented, never B's (D1).
#[test]
fn a_refused_restore_draws_the_presented_view_not_the_prepared() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    show(
        &rig.link,
        lifetime,
        1,
        &payload(1, Some(&skin), 1_111, &[]),
        &Overlay::default(),
    );
    assert!(rig.until(|d| skin_frame(d).is_some()));
    rig.faults.refuse_restore.store(true, Ordering::Release);
    rig.faults.fail_strip.store(true, Ordering::Release);
    show(
        &rig.link,
        lifetime,
        2,
        &payload(2, Some(&skin), 2_222, &[]),
        &Overlay::default(),
    );
    assert!(wait_until(|| rig.link.failed()));
    rig.clear();
    rig.link.prepare().unwrap();
    assert!(rig.until(|d| matches!(d, Drawn::Page(_))));
    std::thread::sleep(Duration::from_millis(100));
    for d in rig.drawn() {
        let Drawn::Page(page) = d else {
            panic!("the skin drew after its restore was refused");
        };
        assert_eq!(page[2], "v1 2", "the presented view's rows: {page:?}");
    }
    assert!(rig.link.result_status().disabled.is_some());
    rig.link.exit();
}

/// The first fallback frame of a new lifetime draws that lifetime's own
/// page, whole, before its `End` arrives: no row of the previous attempt's
/// page shows (D1).
#[test]
fn a_new_lifetimes_first_fallback_has_no_earlier_row() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let first = installed(&rig, &skin);
    rig.link.show_end(EndPage {
        lines: (0..25).map(|i| format!("old {i}")).collect(),
        graphs: None,
    });
    show(
        &rig.link,
        first,
        1,
        &payload(1, Some(&skin), 1_111, &[]),
        &Overlay::default(),
    );
    assert!(rig.until(|d| skin_frame(d).is_some()));
    rig.link.end_result();
    let second = rig.link.begin_result();
    rig.clear();
    rig.faults.refuse_text.store(true, Ordering::Release);
    show(
        &rig.link,
        second,
        1,
        &payload(2, Some(&skin), 2_222, &[]),
        &Overlay::default(),
    );
    assert!(rig.until(|d| matches!(d, Drawn::Page(_))));
    let Some(Drawn::Page(page)) = rig
        .drawn()
        .into_iter()
        .find(|d| matches!(d, Drawn::Page(_)))
    else {
        unreachable!();
    };
    assert!(
        page.iter().all(|l| !l.starts_with("old ")),
        "a row of the previous attempt: {page:?}"
    );
    assert_eq!(page[0], "v2 0", "{page:?}");
    rig.link.exit();
}

/// A panic on the render thread is a screen failure the game sees: the
/// link reports it failed before the screen's resources are destroyed
/// (its teardown held here), with no reply outstanding to latch it.
#[test]
fn a_render_thread_panic_is_a_failure() {
    let rig = Rig::new();
    rig.faults.hold_drop.store(true, Ordering::Release);
    rig.faults.panic_page.store(true, Ordering::Release);
    rig.link.show_end(EndPage {
        lines: vec!["page".to_owned()],
        graphs: None,
    });
    assert!(wait_until(|| rig.faults.dropping.load(Ordering::Acquire)));
    assert!(rig.link.failed(), "failed before the teardown");
    rig.faults.hold_drop.store(false, Ordering::Release);
}

/// A panic on the render thread during the after-commit upload is a
/// failure the game sees when the upload's wait returns, while the
/// screen's teardown is still held: the screen counts as failed, so the
/// preparation's last check stops before recording opens.
#[test]
fn a_panic_in_the_upload_fails_the_screen_before_its_teardown() {
    use game::Rig as _;
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let faults = Arc::clone(&rig.faults);
    let Rig { link, .. } = rig;
    let settings = game::Settings {
        device: game::DeviceSettings::default(),
        player: game::PlayerSettings::default(),
    };
    crate::tests::on_rig_with(settings, link, |p, _| {
        p.result_skin = crate::result_skin::ResultSkin::new(Some(
            crate::result_skin::ResultStart::Fixed(Arc::clone(&skin)),
        ));
        let _ = p
            .result_skin
            .image_stage(ResultChart::default(), None, || false, || false);
        faults.hold_drop.store(true, Ordering::Release);
        faults.panic_install.store(true, Ordering::Release);
        p.upload_result();
        let failed = p.render.failed();
        let preflight_stops = p.screen_failed();
        assert!(failed, "the failure is seen when the wait returns");
        assert!(preflight_stops, "recording does not open");
        assert!(wait_until(|| faults.dropping.load(Ordering::Acquire)));
        assert!(p.render.failed(), "with the teardown held");
        faults.hold_drop.store(false, Ordering::Release);
    });
}

/// A reply found disconnected latches the screen's failure before its
/// error is returned, whatever the teardown's order.
#[test]
fn a_disconnected_reply_latches_the_failure() {
    let rig = Rig::new();
    let (tx, rx) = std::sync::mpsc::sync_channel(1);
    drop(tx);
    let mut pending = crate::render_side::PendingSelect {
        request: rig.link.next_request(),
        rx,
        deadline: Instant::now() + Duration::from_secs(10),
        shared: Arc::clone(&rig.link.shared),
        settled: false,
    };
    assert!(!rig.link.failed());
    assert!(matches!(
        pending.poll(Duration::from_millis(5)),
        Some(Err(crate::render_side::ScreenError::Ended))
    ));
    assert!(rig.link.failed());
    rig.link.exit();
}

/// The commit point and the build of a result frame allocate nothing
/// (C11), with the skin at its text capacity (every text element filled,
/// a new title each cycle) and all 13 strip lines: each cycle commits a
/// view prepared on the installed pages with its lines and its strip,
/// replacing the text installed, merges an overlay, and builds the skin's
/// frame with the strip; then a frame the skin refuses, the refusal
/// carried out unformatted. The preparation and the submission stay
/// outside the count; a vector made inside it is counted (the control).
/// Unoptimized builds count the standard library's own allocations: CI
/// runs it in release.
#[test]
fn result_frame_alloc() {
    use super::super::{
        HeldView, PreparedView, ResultDrawn, ResultSide, Run, STRIP_PREPARED, STRIP_PRESENTED,
        Shared,
    };
    // The widget quads' cap (16 384) admits 63 texts of 256 glyphs each
    // beside the number and the image: the skin at its text capacity.
    let texts = skin_model::MAX_TEXT_ELEMENTS - 1;
    let (_dir, skin) = text_skin(texts);
    let r = render::Renderer::new(
        render::Target::Offscreen {
            width: 1_920,
            height: 1_080,
        },
        render::AdapterChoice::Warp,
    )
    .unwrap();
    let mut screen = MenuScreen::new(r, Some(render::text::Fonts::runtime().unwrap()));
    let token = |request, screen: &MenuScreen| render::PreparedToken {
        request,
        device: screen.device(),
        kind: ScreenKind::Result,
    };
    assert_eq!(
        screen.install(ScreenKind::Result, &skin, 1).unwrap(),
        Preparation::Ready
    );
    screen.commit(token(1, &screen)).unwrap();
    let shared = Shared::new();
    let mut side = ResultSide::new(1);
    let lines: Vec<String> = (0..13).map(|i| format!("FACT {i}")).collect();
    let strip: Vec<&str> = lines.iter().map(String::as_str).collect();
    let mut counts = Vec::new();
    for cycle in 0..6_u64 {
        let request = cycle + 2;
        let title = "A".repeat(usize::try_from(cycle).unwrap() % 3 + 1);
        let p = titled(
            cycle,
            Some(&skin),
            1_000 + u32::try_from(cycle).unwrap(),
            &[],
            &title,
        );
        assert_eq!(
            screen.install(ScreenKind::Result, &skin, request).unwrap(),
            Preparation::Ready
        );
        let mut state = skin.skin().prepare_result();
        screen
            .prepare_result(&skin, &mut state, &p.view, &p.labels)
            .unwrap()
            .unwrap();
        screen.prepare_result_strip(STRIP_PREPARED, &strip).unwrap();
        screen
            .prepare_result_strip(STRIP_PRESENTED, &strip)
            .unwrap();
        let overlay = Overlay {
            lines: lines.clone(),
            ..Overlay::default()
        };
        side.prepared = Some(PreparedView {
            held: HeldView {
                revision: cycle + 1,
                payload: Arc::clone(&p),
                overlay: Overlay::default(),
                state: Some(state),
                page: None,
            },
            token: Some(token(request, &screen)),
        });
        side.overlay = Some((cycle + 1, overlay));
        let before = crate::alloc_count::this_thread();
        let drawn = Run::result_frame(&mut screen, Some(&mut side), &shared, TimeUs(1_000_000));
        counts.push(crate::alloc_count::this_thread() - before);
        assert_eq!(drawn, ResultDrawn::Built);
        assert_eq!(screen.result_strip(), 13);
        let quads = screen
            .result_quads()
            .unwrap()
            .iter()
            .filter(|q| q.layer != 0)
            .count();
        assert!(quads >= texts + 5, "every text drawn: {quads} quads");
        screen.submit_result().unwrap();
        side.dropped = [None, None];
        let _ = screen.drain_retired();
    }
    // A frame the skin refuses allocates nothing either.
    screen.refuse_next_result(false);
    let before = crate::alloc_count::this_thread();
    let drawn = Run::result_frame(&mut screen, Some(&mut side), &shared, TimeUs(2_000_000));
    counts.push(crate::alloc_count::this_thread() - before);
    assert!(matches!(drawn, ResultDrawn::Refused(_)), "{drawn:?}");
    // The control: the count sees an allocation in the same window.
    let before = crate::alloc_count::this_thread();
    let made = std::hint::black_box(vec![0_u8; 64]);
    let control = crate::alloc_count::this_thread() - before;
    drop(made);
    assert!(control > 0, "the count sees allocations");
    assert!(counts.iter().all(|&n| n == 0), "{counts:?}");
}

/// C10's flow half through the real `PlayerRig` and its WARP render
/// thread: while the render thread holds the after-commit upload, a quit,
/// a close or an input thread's failure ends the rig's wait within the
/// slice bound, the attempt's upload withdrawn; the end of loading then
/// does not wait on the render thread, still held; released, the late
/// staging installs nothing and nothing of it is retained.
#[test]
fn a_held_upload_is_withdrawn_by_the_flow() {
    use game::Rig as _;
    let _held = HOLDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    for stop in ["quit", "close", "input"] {
        let (_dir, skin) = fixed_skin();
        let Rig { link, .. } = Rig::new();
        let settings = game::Settings {
            device: game::DeviceSettings::default(),
            player: game::PlayerSettings::default(),
        };
        crate::tests::on_rig_with(settings, link, |p, _| {
            p.result_skin = crate::result_skin::ResultSkin::new(Some(
                crate::result_skin::ResultStart::Fixed(Arc::clone(&skin)),
            ));
            let _ = p
                .result_skin
                .image_stage(ResultChart::default(), None, || false, || false);
            crate::dev::hold_result_uploads(true);
            let controls = p.controls;
            let fail = p.input.failer();
            let waited = std::thread::scope(|s| {
                let stopper = s.spawn(|| {
                    assert!(wait_until(crate::dev::result_upload_holding));
                    let at = Instant::now();
                    match stop {
                        "quit" => controls.command(game::Command::Quit),
                        "close" => controls.close(),
                        _ => fail(),
                    }
                    at
                });
                p.upload_result();
                let returned = Instant::now();
                let at = stopper.join().unwrap();
                returned.duration_since(at)
            });
            // One slice, which a timed wait on Windows rounds up to the
            // system timer's period (about 15.6 ms), and the scheduling of
            // two threads on a loaded machine.
            println!("{stop}: the wait ended {waited:?} after");
            assert!(
                waited < crate::render_side::SELECT_SLICE + Duration::from_millis(50),
                "{stop}: the wait ended {waited:?} after"
            );
            assert!(p.result_withdrawn, "{stop}");
            assert!(crate::dev::result_upload_holding(), "{stop}: still held");
            let ended = Instant::now();
            p.end_loading().unwrap();
            assert!(
                ended.elapsed() < Duration::from_millis(100),
                "{stop}: the end of loading waited"
            );
            crate::dev::hold_result_uploads(false);
            assert!(p.render.fence(Duration::from_secs(10)));
            let status = p.render.result_status();
            assert_eq!(status.installed, None, "{stop}");
            assert!(
                !status.retained.contains(&skin.atlas_id()),
                "{stop}: {status:?}"
            );
        });
    }
}

impl Rig {
    pub(crate) fn exit(self) {
        self.link.exit();
    }
}

/// Uploads `upload` through `rig`'s link, then presents a view of
/// `bundle` in a new lifetime: whether the skin drew it, with the
/// result's installation the uploaded atlas.
pub(crate) fn presented_on(
    rig: &Rig,
    upload: crate::result_skin::Upload,
    bundle: &Arc<render::SkinBundle>,
) -> bool {
    let atlas = upload.atlas();
    let pending = rig.link.upload_result(upload);
    let answer = pending.wait();
    if !matches!(answer, Ok(Preparation::Ready)) {
        return false;
    }
    let lifetime = rig.link.begin_result();
    show(
        &rig.link,
        lifetime,
        1,
        &payload(1, Some(bundle), 7, &[]),
        &Overlay::default(),
    );
    rig.until(|d| skin_frame(d).is_some_and(|(q, _)| !skin_quads(q).is_empty()))
        && rig.link.result_status().installed == Some(atlas)
}
