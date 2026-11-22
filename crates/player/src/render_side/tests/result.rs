//! The result screen's presentations on a real screen, on WARP through the
//! render thread (the S10-09i design's C2, C5a, C5c, C10, C12, C17c, C17d,
//! C19 and C22): what each frame shows is one view's, the barrier keeps
//! the view presented, a refusal draws the built-in page in its frame and
//! disables the kind for the run, and a screen built again draws on from
//! the revision presented.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bms_model::TimeUs;
use game::Preparation;
use input::QpcClock;
use skin_model::{AtlasLimits, Quad, ResultChart, ResultLabels, ScreenKind};

use super::{measure, view, wait_until};
use crate::menu_screen::MenuScreen;
use crate::render_side::{EndPage, FrameSink, MakeScreen, NoFrames, RenderLink, Screen, spawn};
use crate::result_present::{Overlay, ResultPresentation, ViewKey, ViewPayload};

/// What a frame drew: the skin's quads and the strip's lines, or the
/// built-in page's lines.
#[derive(Clone, Debug, PartialEq)]
pub(super) enum Drawn {
    Skin(Vec<Quad>, usize),
    Page(Vec<String>),
}

/// The swap chain's grant a boundary looks at without waiting while the
/// frames are paced and the target visible: set by a test, taken by the
/// look that finds it (one test uses it; each test runs in a process of
/// its own).
static GRANT: AtomicBool = AtomicBool::new(false);

/// The frames' pacing: while `pace` is set each frame wait takes one of
/// `permits`, holding until there is one.
#[derive(Default)]
pub(super) struct Gate {
    pace: AtomicBool,
    permits: AtomicU32,
    waits: AtomicU32,
    held: AtomicBool,
}

impl Gate {
    pub(super) fn hold(&self) {
        self.pace.store(true, Ordering::Release);
        assert!(wait_until(|| self.held.load(Ordering::Acquire)));
    }

    /// One frame, then held again in the next wait.
    pub(super) fn step(&self) {
        let before = self.waits.load(Ordering::Acquire);
        self.permits.store(1, Ordering::Release);
        assert!(wait_until(|| self.waits.load(Ordering::Acquire) > before
            && self.held.load(Ordering::Acquire)
            && self.permits.load(Ordering::Acquire) == 0));
    }

    pub(super) fn release(&self) {
        self.pace.store(false, Ordering::Release);
    }
}

/// A result frame built: its time, its `screen` start, and the `screen`
/// timer's ms its state held as the build began.
pub(super) type Build = (TimeUs, Option<TimeUs>, Option<i32>);

/// A test's setup run on the menu screen's renderer ([`Faults::on_renderer`]).
pub(crate) type OnRenderer = Box<dyn FnOnce(&mut render::Renderer) + Send>;

/// Faults a test turns on.
#[derive(Default)]
pub(super) struct Faults {
    /// The texels the next result installation is charged
    /// (`Renderer::state_charge`), so only releasing it admits a play skin
    /// at the atlas cap; 0 for its own charge.
    pub(super) charge_result: std::sync::atomic::AtomicU64,
    /// The next result frame submitted fails as a lost device.
    pub(super) lose: AtomicBool,
    /// What becomes of each result frame, the skin's or a page, in order,
    /// in place of what the screen gave (a window's outcomes offscreen).
    pub(super) outcomes: Mutex<std::collections::VecDeque<render::FrameOutcome>>,
    /// Run inside the next result frame's submit, the skin's or a page's.
    pub(super) during_submit: Mutex<Option<Box<dyn FnOnce() + Send>>>,
    /// The next result frame is refused by the skin.
    pub(super) refuse: AtomicBool,
    /// A screen built again refuses to restore the result's installation.
    refuse_restore: AtomicBool,
    /// The next result text prepared is refused by the skin.
    refuse_text: AtomicBool,
    /// The next built-in page drawn fails as a lost device.
    lose_page: AtomicBool,
    /// The next frame wait times out.
    pub(super) time_out: AtomicBool,
    /// The next strip prepared for a view prepared fails as a lost device.
    pub(super) fail_strip: AtomicBool,
    /// The next view prepared takes this long, ms (the whole frames'
    /// preparation before a wait).
    slow_prepare: std::sync::atomic::AtomicU64,
    /// When the slow preparation's sleep began and ended.
    slept: Mutex<Option<(Instant, Instant)>>,
    /// The slow preparation then arms `lose`: the frame committing that
    /// view fails as a lost device.
    lose_after_prepare: AtomicBool,
    /// Skin frames submitted.
    submits: std::sync::atomic::AtomicU64,
    /// At each status publication, after a frame's cleanup: the frames
    /// submitted and the presents the render thread had timed.
    cleanups: Mutex<Vec<(u64, u64)>>,
    /// The next this many chart loads fail as a lost device.
    fail_load: std::sync::atomic::AtomicU32,
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
    /// The last result frame built: its time and timer 2's start.
    timers: Mutex<Option<(TimeUs, Option<TimeUs>)>>,
    /// Every result frame built, in order: its time, its `screen` start,
    /// and the `screen` timer's ms its state held as the build began (the
    /// runtime's reading: the frame's visit prepared it).
    pub(super) builds: Mutex<Vec<Build>>,
    /// Every result frame built: its time, its readouts, and the year and
    /// the uptime's seconds its state held as the build began (what the
    /// frame's visit prepared).
    pub(super) uptimes: Mutex<Vec<(TimeUs, skin_model::AppReadouts, [i32; 2])>>,
    /// The strip's text per slot as prepared, and the one in use.
    strips: Mutex<[Vec<String>; 3]>,
    strip: Mutex<Vec<String>>,
    /// While it holds points (x, y), each result frame submitted is read
    /// back and its pixels there kept in `samples`, with the frame's index
    /// in what was drawn.
    pub(super) sample: Mutex<Vec<[usize; 2]>>,
    pub(super) samples: Mutex<Vec<(usize, Vec<[u8; 3]>)>>,
    /// Stagings discarded.
    pub(super) discards: AtomicU32,
    /// The page font reads as not in hand, as a screen made for plays only
    /// whose read has not ended: the pending presentation draws its
    /// bitmap line.
    pub(super) no_font: AtomicBool,
    /// The renderer's result chart writes that wrote pixels, after the
    /// last write asked, and each write's table.
    pub(super) chart_writes: std::sync::atomic::AtomicU64,
    pub(super) chart_tables: Mutex<Vec<skin_model::chart_image::ChartTable>>,
    /// The next result chart write that would write pixels is refused.
    pub(super) refuse_chart: AtomicBool,
    /// The next commit is refused (as one for another device), its
    /// staging left as it is.
    pub(super) refuse_commit: AtomicBool,
    /// Whether the last commit found the staging its token names.
    pub(super) commit_found: AtomicBool,
    /// The next installation of the result kind is refused.
    pub(super) refuse_install: AtomicBool,
    /// The next result staging answers cancelled.
    pub(super) cancel_install: AtomicBool,
    /// The next result chart write fails as a lost device.
    pub(super) lose_chart: AtomicBool,
    /// Each uploader step's completion is held unseen this long after its
    /// submission, µs; 0 as the device answers.
    pub(crate) step_delay_us: std::sync::atomic::AtomicU64,
    /// The chart-image bands the menu screen's renderer wrote, as of its
    /// last boundary.
    pub(crate) chart_bands: std::sync::atomic::AtomicU64,
    /// The device is destroyed at the boundary that wrote the first
    /// chart-image band.
    pub(crate) lose_after_band: AtomicBool,
    /// The writes of the staging's first atlas steps on the menu screen's
    /// renderer, each step's count in order, as of its last boundary.
    pub(crate) step_log: Mutex<Vec<u64>>,
    /// The result-kind stagings asked in steps, each look counted.
    pub(crate) stepped_installs: std::sync::atomic::AtomicU64,
    /// The next result-kind staging made in steps has its next atlas step
    /// refused by the device (development builds).
    pub(crate) refuse_step: AtomicBool,
    /// The device is destroyed at the next boundary with a result-kind
    /// staging stepping.
    pub(crate) lose_in_staging: AtomicBool,
    /// A result-kind staging was left stepping by its last look.
    pub(crate) result_stepping: AtomicBool,
    /// A result-kind staging that creates an array of its own is taken
    /// first at a boundary its allowance admits, once it has room, as the
    /// menu screen's renderer answers (`install_creates`, `install_room`);
    /// at once otherwise.
    pub(crate) step_creations: AtomicBool,
    /// Run on the menu screen's renderer at the next upload boundary, in
    /// order (a test's setup of charges and pools on the render thread).
    pub(crate) on_renderer: Mutex<Vec<OnRenderer>>,
    /// The next selection installation's chart-image pool arrays refused
    /// this many times, as the device refuses one.
    pub(crate) refuse_pools: AtomicU32,
}

impl Recorded {
    /// The test's one-shot faults of an installation of `kind`: an answer
    /// in place of the installation, or `None` to install.
    fn install_faults(
        &mut self,
        kind: ScreenKind,
        skin: &Arc<render::SkinBundle>,
    ) -> Option<Result<Preparation, render::RenderError>> {
        assert!(
            !(kind == ScreenKind::Result
                && self.faults.panic_install.swap(false, Ordering::AcqRel)),
            "the render thread panics in the upload (a test's fault)"
        );
        if kind == ScreenKind::Result && self.faults.refuse_install.swap(false, Ordering::AcqRel) {
            return Some(Ok(crate::render_side::refusal(
                "the test refused the staging",
            )));
        }
        if kind == ScreenKind::Result && self.faults.cancel_install.swap(false, Ordering::AcqRel) {
            return Some(Ok(Preparation::Cancelled));
        }
        if kind == ScreenKind::Selection {
            let n = self.faults.refuse_pools.swap(0, Ordering::AcqRel);
            if n > 0 {
                self.menu.renderer_mut().refuse_next_pools(n);
            }
        }
        if kind == ScreenKind::Play && self.faults.release_then_fail.swap(false, Ordering::AcqRel) {
            self.menu.remove(ScreenKind::Result);
            return Some(Err(render::RenderError::SurfaceLost));
        }
        let charge = if kind == ScreenKind::Result {
            self.faults.charge_result.swap(0, Ordering::AcqRel)
        } else {
            0
        };
        if charge > 0 {
            self.menu.renderer_mut().state_charge(skin, charge);
        }
        None
    }
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
        if self
            .faults
            .fail_load
            .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
            .is_ok()
        {
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
        if self.faults.refuse_commit.swap(false, Ordering::AcqRel) {
            return Err(render::CommitError::Device);
        }
        self.faults
            .commit_found
            .store(self.menu.staged_token() == Some(token), Ordering::Release);
        self.menu.commit(token)
    }
    fn discard(&mut self) {
        self.faults.discards.fetch_add(1, Ordering::AcqRel);
        self.menu.discard();
    }
    fn discard_token(&mut self, token: render::PreparedToken) {
        if self.menu.staged_token() == Some(token) {
            self.faults.discards.fetch_add(1, Ordering::AcqRel);
        }
        self.menu.discard_token(token);
    }
    fn staged_token(&self) -> Option<render::PreparedToken> {
        self.menu.staged_token()
    }
    fn prepare_attempt(
        &mut self,
        a: &render::AttemptInput<'_>,
        skin: Option<&Arc<render::SkinBundle>>,
        request: u64,
    ) -> Option<Result<Preparation, render::RenderError>> {
        // A specialization stages on the menu screen; an attempt with the
        // skin in use loads through this screen's faults, as before.
        if skin.is_some() {
            let delay = self.faults.step_delay_us.load(Ordering::Acquire);
            self.menu
                .renderer_mut()
                .delay_steps((delay > 0).then(|| Duration::from_micros(delay)));
            return self.menu.prepare_attempt(a, skin, request);
        }
        Some(match self.load(a.chart, a.section()) {
            Ok(()) => Ok(Preparation::Ready),
            Err(render::RenderError::Skin(e)) => Ok(super::super::refusal(e.reason)),
            Err(e) => Err(e),
        })
    }
    fn upload_boundary(&mut self, past: &mut dyn render::UploadGate) -> render::Uploaded {
        let setups = std::mem::take(&mut *self.faults.on_renderer.lock().unwrap());
        for f in setups {
            f(self.menu.renderer_mut());
        }
        if self.faults.result_stepping.swap(false, Ordering::AcqRel)
            && self.faults.lose_in_staging.swap(false, Ordering::AcqRel)
        {
            self.menu.renderer_mut().gpu().device().destroy();
        }
        let up = self.menu.upload_boundary(past);
        let (log, _) = self.menu.renderer_mut().step_log();
        log.clone_into(
            &mut self
                .faults
                .step_log
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
        let bands = self.menu.renderer_mut().chart_bands();
        self.faults.chart_bands.store(bands, Ordering::Release);
        if bands > 0 && self.faults.lose_after_band.swap(false, Ordering::AcqRel) {
            self.menu.renderer_mut().gpu().device().destroy();
        }
        up
    }
    fn upload_state(&self) -> render::UploadState {
        self.menu.upload_state()
    }
    fn upload_waited(&self) -> bool {
        self.menu.upload_waited()
    }
    fn wait_step(&mut self, timeout: std::time::Duration) -> Result<bool, render::RenderError> {
        self.menu.wait_step(timeout)
    }
    fn publish_chart_images(
        &mut self,
        target: render::ChartTarget,
        images: [Option<&skin_model::chart_image::Fitted>; 3],
    ) -> Result<skin_model::chart_image::ChartTable, render::RenderError> {
        self.menu.publish_chart_images(target, images)
    }
    fn begin_chart_images(
        &mut self,
        target: render::ChartTarget,
        set: &Arc<crate::chart_images::ChartImages>,
    ) -> Result<Option<skin_model::chart_image::ChartTable>, render::RenderError> {
        self.menu.begin_chart_images(target, set)
    }
    fn cancel_chart_images(&mut self) {
        self.menu.cancel_chart_images();
    }
    fn publish_chart_absent(
        &mut self,
        target: render::ChartTarget,
    ) -> Result<skin_model::chart_image::ChartTable, render::RenderError> {
        self.menu.publish_chart_absent(target)
    }
    fn invalidate_chart_images(&mut self) {
        self.menu.invalidate_chart_images();
    }
    fn chart_key(&self) -> Option<skin_model::chart_image::ChartKey> {
        self.menu.chart_key()
    }
    fn write_result_chart(
        &mut self,
        target: render::ResultChartTarget,
        images: [Option<&skin_model::chart_image::Fitted>; 3],
        set: u64,
    ) -> Result<skin_model::chart_image::ChartTable, render::RenderError> {
        if self.faults.refuse_chart.swap(false, Ordering::AcqRel) {
            self.menu.renderer_mut().refuse_next_result_chart();
        }
        if self.faults.lose_chart.swap(false, Ordering::AcqRel) {
            return Err(render::RenderError::DeviceLost);
        }
        let out = self.menu.write_result_chart(target, images, set);
        self.faults.chart_writes.store(
            self.menu.renderer_mut().result_chart_writes(),
            Ordering::Release,
        );
        if let Ok(t) = &out {
            self.faults.chart_tables.lock().unwrap().push(*t);
        }
        out
    }
    fn install(
        &mut self,
        kind: ScreenKind,
        skin: &Arc<render::SkinBundle>,
        request: u64,
    ) -> Result<Preparation, render::RenderError> {
        if let Some(answer) = self.install_faults(kind, skin) {
            return answer;
        }
        self.menu.install(kind, skin, request)
    }
    fn install_creates(&self, kind: ScreenKind, skin: &Arc<render::SkinBundle>) -> bool {
        self.faults.step_creations.load(Ordering::Acquire) && self.menu.install_creates(kind, skin)
    }
    fn install_room(&self, kind: ScreenKind, skin: &Arc<render::SkinBundle>) -> bool {
        !self.faults.step_creations.load(Ordering::Acquire) || self.menu.install_room(kind, skin)
    }
    fn install_stepped(
        &mut self,
        kind: ScreenKind,
        skin: &Arc<render::SkinBundle>,
        request: u64,
    ) -> Option<Result<Preparation, render::RenderError>> {
        if let Some(answer) = self.install_faults(kind, skin) {
            return Some(answer);
        }
        if kind == ScreenKind::Result {
            self.faults.stepped_installs.fetch_add(1, Ordering::AcqRel);
        }
        let answer = self.menu.install_stepped(kind, skin, request);
        #[cfg(feature = "dev-tools")]
        if kind == ScreenKind::Result
            && answer.is_none()
            && self.faults.refuse_step.swap(false, Ordering::AcqRel)
        {
            self.menu.renderer_mut().refuse_next_step();
        }
        if kind == ScreenKind::Result && answer.is_none() {
            self.faults.result_stepping.store(true, Ordering::Release);
        }
        answer
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
    fn begin_pages(
        &mut self,
        pages: &Arc<skin_model::PackedPages>,
        ticket: u64,
    ) -> Result<Option<render::PagesBegun>, render::RenderError> {
        self.menu.begin_pages(pages, ticket)
    }
    fn publish_pages(&mut self) -> Option<u64> {
        self.menu.publish_pages()
    }
    fn withdraw_pages(&mut self) -> Option<u64> {
        self.menu.withdraw_pages()
    }
    fn take_result_release(&mut self) -> Option<[u64; 3]> {
        self.menu.take_result_release()
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
    fn installed_pages(&self, kind: ScreenKind) -> Option<Arc<skin_model::PackedPages>> {
        self.menu.installed_pages(kind)
    }
    fn retained_atlases(
        &self,
        kind: ScreenKind,
    ) -> [Option<skin_model::AtlasId>; render::RETAINED_ATLASES] {
        // The report reads the screen's atlases once a publication.
        self.faults.cleanups.lock().unwrap().push((
            self.faults.submits.load(Ordering::Acquire),
            crate::dev::presents_marked(),
        ));
        self.menu.retained_atlases(kind)
    }
    fn poll_completion(&mut self) -> Result<(), render::RenderError> {
        self.menu.poll_completion()
    }
    fn set_destroy_allowed(&mut self, allowed: bool) {
        self.menu.set_destroy_allowed(allowed);
    }
    fn filled(&self) -> u32 {
        self.menu.filled()
    }
    fn destroy_due(&mut self, heavy: bool) -> render::Destroyed {
        self.menu.destroy_due(heavy)
    }
    fn due_heavy(&self) -> bool {
        self.menu.due_heavy()
    }
    fn pending_completion(&self) -> bool {
        self.menu.pending_completion()
    }
    fn pool_owed(&self) -> bool {
        self.menu.pool_owed()
    }
    fn make_owed_pool(&mut self) -> bool {
        self.menu.make_owed_pool()
    }
    fn take_handoff(&mut self) -> Option<render::Handoff> {
        self.menu.take_handoff()
    }
    fn return_handoff(&mut self, entry: usize, cpu: render::CpuParts) -> bool {
        self.menu.return_handoff(entry, cpu)
    }
    fn result_changes(&self) -> u64 {
        self.menu.result_changes()
    }
    fn heavy_ops(&self) -> render::HeavyOps {
        self.menu.heavy_ops()
    }
    fn grant_ready(&self) -> bool {
        self.menu.grant_ready()
    }
    fn bga_in_use(&self) -> bool {
        self.menu.bga_in_use()
    }
    fn entries_holding_pages(&self) -> [bool; render::RETIREMENT_ENTRIES] {
        self.menu.entries_holding_pages()
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
        let slow = self.faults.slow_prepare.swap(0, Ordering::AcqRel);
        if slow > 0 {
            let began = Instant::now();
            std::thread::sleep(Duration::from_millis(slow));
            *self.faults.slept.lock().unwrap() = Some((began, Instant::now()));
            if self.faults.lose_after_prepare.swap(false, Ordering::AcqRel) {
                self.faults.lose.store(true, Ordering::Release);
            }
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
        if let Some(t) = self.faults.strips.lock().unwrap().get_mut(which) {
            *t = lines.iter().map(|l| (*l).to_owned()).collect();
        }
        self.menu.prepare_result_strip(which, lines)
    }
    fn use_result_strip(&mut self, which: usize) {
        if let Some(t) = self.faults.strips.lock().unwrap().get(which) {
            self.faults.strip.lock().unwrap().clone_from(t);
        }
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
        *self.faults.timers.lock().unwrap() = Some((frame.now, frame.timers.fadeout));
        let prepared = state.runtime_parts().1.first().copied();
        let held = {
            let fields = &*state.runtime_parts().0;
            let at = |slot: u16| fields.get(usize::from(slot)).copied().unwrap_or(-1);
            [
                at(skin_model::result_field::CLOCK_YEAR),
                at(skin_model::result_field::APP_UPTIME_S),
            ]
        };
        self.faults
            .builds
            .lock()
            .unwrap()
            .push((frame.now, frame.timers.screen, prepared));
        self.faults
            .uptimes
            .lock()
            .unwrap()
            .push((frame.now, frame.app, held));
        self.menu.build_result(state, frame)
    }
    fn submit_result(&mut self) -> Result<render::FrameTiming, render::RenderError> {
        if self.faults.lose.swap(false, Ordering::AcqRel) {
            return Err(render::RenderError::SurfaceLost);
        }
        let mut t = self.menu.submit_result()?;
        if let Some(f) = self.faults.during_submit.lock().unwrap().take() {
            f();
        }
        if let Some(o) = self.faults.outcomes.lock().unwrap().pop_front() {
            t.outcome = o;
        }
        self.faults.submits.fetch_add(1, Ordering::AcqRel);
        let quads = self.menu.result_quads().unwrap_or_default();
        let strip = self.menu.result_strip();
        let points = self.faults.sample.lock().unwrap().clone();
        let mut drawn = self.drawn.lock().unwrap();
        if !points.is_empty()
            && let Some((w, _, pixels)) = self.menu.read_back()
        {
            let w = usize::try_from(w).unwrap();
            let rgb = points
                .iter()
                .map(|&[x, y]| {
                    let at = (y * w + x) * 4;
                    [pixels[at], pixels[at + 1], pixels[at + 2]]
                })
                .collect();
            self.faults.samples.lock().unwrap().push((drawn.len(), rgb));
        }
        drawn.push(Drawn::Skin(quads, strip));
        Ok(t)
    }
    fn end(&mut self, page: &EndPage) -> Result<render::FrameOutcome, render::RenderError> {
        if self.faults.lose_page.swap(false, Ordering::AcqRel) {
            return Err(render::RenderError::SurfaceLost);
        }
        assert!(
            !self.faults.panic_page.swap(false, Ordering::AcqRel),
            "the render thread panics (a test's fault)"
        );
        let mut p = self.menu.end(page)?;
        if let Some(f) = self.faults.during_submit.lock().unwrap().take() {
            f();
        }
        if let Some(o) = self.faults.outcomes.lock().unwrap().pop_front() {
            p = o;
        }
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
    fn grant_probe(&self) -> Option<render::GrantProbe> {
        (self.gate.pace.load(Ordering::Acquire) && !self.menu.minimized())
            .then_some(render::GrantProbe::Flag(&GRANT))
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
    fn text_frame(&mut self, lines: &[&str]) -> Result<render::FrameTiming, render::RenderError> {
        self.menu.text_frame(lines)
    }
    fn page_font_ready(&mut self) -> bool {
        !self.faults.no_font.load(Ordering::Acquire) && self.menu.page_font_ready()
    }
    fn minimized(&self) -> bool {
        self.menu.minimized()
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
    pub(super) link: RenderLink,
    clock: QpcClock,
    pub(super) gate: Arc<Gate>,
    pub(super) faults: Arc<Faults>,
    pub(super) drawn: Arc<Mutex<Vec<Drawn>>>,
}

impl Rig {
    pub(crate) fn new() -> Self {
        Self::with_sink(Box::new(NoFrames))
    }

    /// [`new`](Self::new) with `frames` as its frame sink.
    pub(crate) fn with_sink(frames: Box<dyn FrameSink>) -> Self {
        let gate = Arc::new(Gate::default());
        let faults = Arc::new(Faults::default());
        let drawn = Arc::new(Mutex::new(Vec::new()));
        let (g, f, d) = (Arc::clone(&gate), Arc::clone(&faults), Arc::clone(&drawn));
        let make: MakeScreen = Box::new(move |_| {
            let mut r = render::Renderer::new(
                render::Target::Offscreen {
                    width: 1_920,
                    height: 1_080,
                },
                render::AdapterChoice::Warp,
            )?;
            // The tests count the frames drawn and read what they
            // acknowledged: a frame skipped past the renderer's 100 ms
            // wait for the one before on WARP would be recorded here yet
            // acknowledge nothing, which a loaded machine would decide.
            // Faults inject the outcomes a test needs.
            r.set_offscreen_frame_wait(None);
            Ok(Box::new(Recorded {
                menu: MenuScreen::new(r, Some(render::text::Fonts::runtime()?)),
                gate: Arc::clone(&g),
                faults: Arc::clone(&f),
                drawn: Arc::clone(&d),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(make, None, None, clock, view(), measure(clock), frames).unwrap();
        Self {
            link,
            clock,
            gate,
            faults,
            drawn,
        }
    }

    pub(super) fn drawn(&self) -> Vec<Drawn> {
        self.drawn.lock().unwrap().clone()
    }

    pub(super) fn clear(&self) {
        self.drawn.lock().unwrap().clear();
    }

    /// Waits until the last frame drew `want`.
    pub(super) fn until(&self, want: impl Fn(&Drawn) -> bool) -> bool {
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
    let (d, b, _) = text_skin_scripted(texts, "");
    (d, b)
}

/// [`text_skin`] with `places` among its destinations, and the runtime
/// its import kept.
fn text_skin_scripted(
    texts: usize,
    more: &str,
) -> (
    tempfile::TempDir,
    Arc<render::SkinBundle>,
    crate::screens::ScriptSlot,
) {
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
    lua_skin_scripted(
        &format!(
            "local calls = 0\n\
             skin.font = {{ {{ id = \"f\", path = \"f.fnt\" }} }}\n\
             skin.text = {{ {elements} }}\n"
        ),
        &format!("{places}{more}"),
    )
}

/// A fixed Lua result skin as [`fixed_skin`], with `head` before its
/// destinations and `places` among them; the bitmap font `f.fnt` beside
/// it.
fn lua_skin(head: &str, places: &str) -> (tempfile::TempDir, Arc<render::SkinBundle>) {
    let (d, b, _) = lua_skin_scripted(head, places);
    (d, b)
}

/// [`lua_skin`], with the runtime its import kept.
pub(super) fn lua_skin_scripted(
    head: &str,
    places: &str,
) -> (
    tempfile::TempDir,
    Arc<render::SkinBundle>,
    crate::screens::ScriptSlot,
) {
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
        crate::imports::SkinSession::default(),
        &target,
        &skin_beatoraja::Cancel::new(),
    );
    drop(serial);
    match built {
        crate::imports::KindOutcome::Bundle(b, _, script) => (d, b, script),
        other => panic!("{other:?}"),
    }
}

/// [`fixed_skin`] with a dot at y = 700 whose draw runs `body` in the
/// runtime: drawn only while the runtime runs and answers true.
fn scripted_skin(
    body: &str,
) -> (
    tempfile::TempDir,
    Arc<render::SkinBundle>,
    crate::screens::ScriptSlot,
) {
    lua_skin_scripted(
        "local calls = 0\n",
        &format!(
            "{{ id = \"dot\", draw = function() {body} end, \
             dst = {{ {{ x = 0, y = 700, w = 8, h = 8 }} }} }},"
        ),
    )
}

/// A view payload `key` with EX score `ex`, its view lines `lines` and a
/// built-in page whose view rows say `v<key>`.
pub(super) fn payload(
    key: ViewKey,
    skin: Option<&Arc<render::SkinBundle>>,
    ex: u32,
    lines: &[&str],
) -> Arc<ViewPayload> {
    titled(key, skin, ex, lines, "")
}

/// [`payload`] of a chart titled `title`.
pub(super) fn titled(
    key: ViewKey,
    skin: Option<&Arc<render::SkinBundle>>,
    ex: u32,
    lines: &[&str],
    title: &str,
) -> Arc<ViewPayload> {
    let mut view = crate::result_skin::probe_view(ResultChart::default());
    view.score.tally.ex_score = ex;
    Arc::new(ViewPayload {
        chart: None,
        key,
        skin: skin.map(Arc::clone),
        script: None,
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
            #[cfg(feature = "dev-tools")]
            id: None,
        },
    })
}

/// Sends `payload` as revision `revision` of `lifetime` with `overlay`.
pub(super) fn show(
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
pub(super) fn skin_quads(q: &[Quad]) -> Vec<Quad> {
    q.iter().filter(|q| q.layer != 0).copied().collect()
}

pub(super) fn skin_frame(d: &Drawn) -> Option<(&[Quad], usize)> {
    match d {
        Drawn::Skin(q, n) => Some((q, *n)),
        Drawn::Page(_) => None,
    }
}

/// Installs the fixed skin as the result kind's and begins a lifetime.
pub(super) fn installed(rig: &Rig, skin: &Arc<render::SkinBundle>) -> u64 {
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

/// A sink that times each taking of the result and ends, in a transition
/// recorder, the preparation of a play that ended before the result.
#[cfg(feature = "dev-tools")]
struct Taking {
    recorder: crate::dev::transitions::Recorder,
    calls: Arc<Mutex<Vec<TimeUs>>>,
}

#[cfg(feature = "dev-tools")]
impl FrameSink for Taking {
    fn frame(&mut self, _screen: &dyn Screen, _drawn: &crate::measure::Drawn<'_>) {}

    fn result_taken(&mut self, at: TimeUs) {
        self.calls.lock().unwrap().push(at);
        self.recorder.result_taken(at);
    }
}

/// The result's preparation ends at whichever the render thread takes
/// first, a result skin's presentation (`ResultChanged`) or the built-in
/// result's page (`End`), in either order: both are taken, and the one
/// preparing mark ends at the first.
#[cfg(feature = "dev-tools")]
#[test]
fn result_preparing_ends_at_the_first_taker() {
    use crate::dev::transitions::{MarkKind, Record, Recorder, Transitions};
    for skin_first in [true, false] {
        let (_dir, skin) = fixed_skin();
        let book = Transitions::with_capacity(16);
        let recorder = Recorder::new(book.clone());
        recorder.play_ended(TimeUs(1));
        let calls = Arc::new(Mutex::new(Vec::new()));
        let rig = Rig::with_sink(Box::new(Taking {
            recorder,
            calls: Arc::clone(&calls),
        }));
        let lifetime = installed(&rig, &skin);
        let a = payload(1, Some(&skin), 1_111, &[]);
        let end = || {
            rig.link.show_end(EndPage {
                lines: vec!["end".to_owned()],
                graphs: None,
                id: None,
            });
        };
        let taken = |n: usize| wait_until(|| calls.lock().unwrap().len() >= n);
        if skin_first {
            show(&rig.link, lifetime, 1, &a, &Overlay::default());
            assert!(taken(1), "the skin's presentation is taken");
            end();
        } else {
            end();
            assert!(taken(1), "the built-in page is taken");
            show(&rig.link, lifetime, 1, &a, &Overlay::default());
        }
        assert!(taken(2), "{skin_first}: {:?}", calls.lock().unwrap());
        rig.link.exit();
        let first = calls.lock().unwrap()[0];
        let marks: Vec<_> = book
            .records()
            .into_iter()
            .filter_map(|r| match r {
                Record::Mark(m) if m.kind == MarkKind::Preparing => Some(m),
                _ => None,
            })
            .collect();
        let [m] = marks[..] else {
            panic!("{skin_first}: one preparation: {marks:?}");
        };
        assert_eq!((m.from, m.to), (TimeUs(1), first), "{skin_first}");
    }
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
    assert_eq!(rig.link.pin_result(TimeUs(5), None).unwrap().0, Some(1));
    let notice = Overlay {
        lines: vec!["NOTICE".to_owned()],
        fadeout: Some(TimeUs(5)),
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
        #[cfg(feature = "dev-tools")]
        id: None,
    });
    assert!(rig.link.fence(Duration::from_secs(10)));
    // Only the refused frame runs: the next waits before its commit
    // point, so the timing is cleared by the refusal itself (T8).
    rig.gate.hold();
    rig.faults.refuse.store(true, Ordering::Release);
    rig.gate.step();
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
    assert_eq!(rig.link.presented_timing(), None, "no skin draws (T8)");
    rig.gate.release();
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
        assert_eq!(rig.link.pin_result(TimeUs(5), None).unwrap().0, Some(1));
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
    assert_eq!(rig.link.result_status().installed, None);
    // The withdrawn staging's array waits in its own retirement entry
    // until a boundary destroys it (design.md "Memory Budget"): it goes.
    assert!(
        wait_until(|| !rig.link.result_status().retained.contains(&skin.atlas_id())),
        "{:?}",
        rig.link.result_status()
    );
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

/// The result's cleanup discards only its own staging: a view prepared
/// and left uncommitted (its frame's wait timed out), the result left and
/// the loading state shown before its cleanup ran, then a play attempt
/// staged; the cleanup the next preparation runs leaves the play's
/// staging, which its commit then finds.
#[test]
fn the_result_s_cleanup_leaves_a_play_staging() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let chart = Arc::new(bms_model::ChartBuilder::new(120_000).build().unwrap());
    rig.link.load(Arc::clone(&chart), None).unwrap();
    // A declarative play skin selected, and another staged with the play.
    let target = crate::render_common::offscreen(1_920, 1_080);
    let (_play_dir, play) = crate::render_json::bundle(&target);
    let (_staged_dir, staged) = crate::render_json::bundle(&target);
    assert_eq!(
        rig.link
            .select(ScreenKind::Play, Some(play))
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    let lifetime = installed(&rig, &skin);
    let a = payload(1, Some(&skin), 1_111, &[]);
    let b = payload(2, Some(&skin), 2_222, &[]);
    show(&rig.link, lifetime, 1, &a, &Overlay::default());
    assert!(rig.until(|d| skin_frame(d).is_some()));
    // b is prepared and held at the next frame's wait, which times out.
    rig.gate.hold();
    show(&rig.link, lifetime, 2, &b, &Overlay::default());
    rig.gate.step();
    rig.faults.time_out.store(true, Ordering::Release);
    // Queued behind the wait: the result left, the loading state shown,
    // a play attempt staged.
    rig.link.end_result();
    rig.link.loading().show(super::one_line("loading"));
    let mut attempt = rig.link.begin_attempt(
        super::titled(&chart),
        super::super::AttemptSkin::Specialized(staged),
    );
    rig.gate.release();
    let answer = loop {
        if let Some(a) = attempt.wait_until(None, || false) {
            break a.unwrap();
        }
    };
    assert_eq!(answer.preparation, Preparation::Ready);
    // The next preparation runs the result's cleanup first.
    rig.link.prepare().unwrap();
    rig.link.commit_attempt().unwrap();
    assert!(
        rig.faults.commit_found.load(Ordering::Acquire),
        "the play's staging was discarded by the result's cleanup"
    );
    rig.link.exit();
}

/// A result view newer than every request supersedes the play attempt
/// staged before it: the play's staging, which the view's own replaces in
/// the screen's one slot, is discarded with its answer, never left
/// answered behind a newer request.
#[test]
fn a_newer_result_view_supersedes_a_play_staging() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let chart = Arc::new(bms_model::ChartBuilder::new(120_000).build().unwrap());
    rig.link.load(Arc::clone(&chart), None).unwrap();
    let target = crate::render_common::offscreen(1_920, 1_080);
    let (_play_dir, play) = crate::render_json::bundle(&target);
    let (_staged_dir, staged) = crate::render_json::bundle(&target);
    assert_eq!(
        rig.link
            .select(ScreenKind::Play, Some(play))
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    let lifetime = installed(&rig, &skin);
    assert_eq!(
        rig.link
            .prepare_attempt_with(
                super::titled(&chart),
                super::super::AttemptSkin::Specialized(staged),
                || false,
            )
            .unwrap(),
        Preparation::Ready
    );
    let discards = rig.faults.discards.load(Ordering::Acquire);
    show(
        &rig.link,
        lifetime,
        1,
        &payload(1, Some(&skin), 1_111, &[]),
        &Overlay::default(),
    );
    assert!(rig.until(|d| skin_frame(d).is_some()));
    assert_eq!(
        rig.faults.discards.load(Ordering::Acquire),
        discards + 1,
        "the play's staging discarded with its answer"
    );
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
    // The refusal publishes the disabled kind before the page's frame is
    // counted, and the frame is recorded before it returns: what it shows
    // is published after both.
    assert!(wait_until(|| rig.link.result_status().shown.is_some()));
    assert_eq!(rig.link.result_status().shown, Some((1, false)));
    rig.link.exit();
}

/// With the transition probe on, a result whose first view is the
/// built-in page, its skin's text refused, has its first frames probed
/// (`rf:*`) as a skin's first view has: the first view committed, with a
/// staging or without.
#[test]
fn the_probe_marks_the_first_frames_of_a_built_in_first_view() {
    use crate::dev::loop_probe::tests::{marks, turn_on};
    turn_on();
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
    assert!(matches!(rig.drawn().first(), Some(Drawn::Page(_))));
    let left = || -> Vec<i64> {
        marks()
            .iter()
            .filter(|m| m.1 == "rf:frame_left")
            .map(|m| m.2)
            .collect()
    };
    assert!(
        wait_until(|| left().len() == 3),
        "the first view's frames not probed: {:?}",
        marks()
    );
    assert_eq!(left(), [2, 1, 0]);
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
        #[cfg(feature = "dev-tools")]
        id: None,
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
    // `a` waits in the entry the begin's release filled, `b` in the
    // withdrawn staging's, each until a boundary destroys it (design.md
    // "Memory Budget"): both go.
    assert!(
        wait_until(|| {
            let s = rig.link.result_status();
            !s.retained.contains(&a.atlas_id()) && !s.retained.contains(&b.atlas_id())
        }),
        "{:?}",
        rig.link.result_status()
    );
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
            crate::result_skin::ResultStart::Fixed(Arc::clone(&skin), None),
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
            #[cfg(feature = "dev-tools")]
            id: None,
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
        #[cfg(feature = "dev-tools")]
        id: None,
    });
    assert!(wait_until(|| rig.link.failed()));
    rig.faults.refuse_restore.store(true, Ordering::Release);
    // The load and its rebuild with the chart both lost.
    rig.faults.fail_load.store(2, Ordering::Release);
    // The screen built again with the chart: the result's restore
    // refused, then the chart's load fails, twice.
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
        #[cfg(feature = "dev-tools")]
        id: None,
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
        #[cfg(feature = "dev-tools")]
        id: None,
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
            crate::result_skin::ResultStart::Fixed(Arc::clone(&skin), None),
        ));
        let _ = p
            .result_skin
            .image_stage(ResultChart::default(), None, || false, || false);
        faults.hold_drop.store(true, Ordering::Release);
        faults.panic_install.store(true, Ordering::Release);
        p.result_step();
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
    let _warp = crate::warp_turn::take();
    // The widget quads' cap (16 384) admits 63 texts of 256 glyphs each
    // beside the number and the images: the skin at its text capacity,
    // with a dot its runtime draws (S7 C9: the runtime's step between the
    // two counted spans).
    let texts = skin_model::MAX_TEXT_ELEMENTS - 1;
    let (_dir, skin, script) = text_skin_scripted(
        texts,
        "{ id = \"dot\", draw = function() calls = calls + 1 return calls > 0 end, \
         dst = { { x = 0, y = 700, w = 8, h = 8 } } },",
    );
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
    let scripts = crate::screens::Scripts::new();
    let g = scripts.offer_result(script.untimed(), true);
    assert!(g.is_some(), "the dot's draw runs in the runtime");
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
        let p = Arc::new(ViewPayload {
            chart: None,
            script: g,
            ..(*p).clone()
        });
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
                script: scripts.result_slot(g),
            },
            token: Some(token(request, &screen)),
        });
        side.overlay = Some((cycle + 1, overlay));
        let now = TimeUs(1_000_000 + 16_000 * i64::try_from(cycle).unwrap());
        let before = crate::alloc_count::this_thread();
        let fadeout =
            Run::result_commit(&mut screen, Some(&mut side), &shared, &scripts, TimeUs(0));
        let commit = crate::alloc_count::this_thread() - before;
        Run::result_visit(
            Some(&mut side),
            now,
            fadeout,
            &skin_model::AppReadouts::LAUNCH,
        );
        assert!(
            side.presented.as_ref().is_some_and(|h| h.script.runs()),
            "the runtime ran between the spans"
        );
        let before = crate::alloc_count::this_thread();
        let drawn = Run::result_build(
            &mut screen,
            Some(&mut side),
            now,
            fadeout,
            false,
            &skin_model::AppReadouts::LAUNCH,
        );
        counts.push(commit + crate::alloc_count::this_thread() - before);
        assert_eq!(drawn, ResultDrawn::Built);
        assert_eq!(screen.result_strip(), 13);
        let quads = screen
            .result_quads()
            .unwrap()
            .iter()
            .filter(|q| q.layer != 0)
            .count();
        assert!(
            quads >= texts + 6,
            "every text and the dot drawn: {quads} quads"
        );
        screen.submit_result().unwrap();
        side.dropped = [None, None];
        // What the commit retired goes before the next cycle, outside
        // the spans counted.
        crate::render_side::settle_entries(&mut screen);
    }
    // A frame the skin refuses allocates nothing either.
    screen.refuse_next_result(false);
    let before = crate::alloc_count::this_thread();
    let fadeout = Run::result_commit(&mut screen, Some(&mut side), &shared, &scripts, TimeUs(0));
    let drawn = Run::result_build(
        &mut screen,
        Some(&mut side),
        TimeUs(2_000_000),
        fadeout,
        false,
        &skin_model::AppReadouts::LAUNCH,
    );
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
                crate::result_skin::ResultStart::Fixed(Arc::clone(&skin), None),
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
                p.result_step();
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
            assert_eq!(p.render.result_status().installed, None, "{stop}");
            // The withdrawn staging's array waits in its retirement entry
            // until a boundary destroys it (design.md "Memory Budget").
            assert!(
                wait_until(|| !p.render.result_status().retained.contains(&skin.atlas_id())),
                "{stop}: {:?}",
                p.render.result_status()
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

/// A fixed skin with `head` and `places` (as [`lua_skin`]) shown through
/// the real `PlayerRig` on a WARP rig sharing its clock: `f` is given the
/// rig, the result's report and screen, the faults and what was drawn,
/// once the skin draws.
fn on_shown_result(
    head: &str,
    places: &str,
    f: impl FnOnce(
        &mut crate::play::PlayerRig<'_>,
        &game::PlayReport,
        &game::ResultScreen,
        &Faults,
        &Mutex<Vec<Drawn>>,
    ),
) {
    let (_dir, skin) = lua_skin(head, places);
    let rig = Rig::new();
    let clock = rig.clock;
    let drawn = Arc::clone(&rig.drawn);
    let faults = Arc::clone(&rig.faults);
    let Rig { link, .. } = rig;
    let settings = game::Settings {
        device: game::DeviceSettings::default(),
        player: game::PlayerSettings::default(),
    };
    let dir = tempfile::tempdir().unwrap();
    crate::tests::on_rig_hooked(settings, link, clock, |p, _, _| {
        p.result_skin = crate::result_skin::ResultSkin::new(Some(
            crate::result_skin::ResultStart::Fixed(Arc::clone(&skin), None),
        ));
        let chart = ResultChart::default();
        let _ = p.result_skin.image_stage(chart, None, || false, || false);
        let report = crate::end_screen::tests::report(rules::tapestry());
        let screen = crate::end_screen::tests::screen(dir.path(), &report);
        let _ = p
            .result_skin
            .request(crate::result_skin::view_facts(&screen, chart));
        p.result_chart = Some(chart);
        p.end_page = Some(EndPage {
            lines: vec!["base".to_owned()],
            graphs: None,
            #[cfg(feature = "dev-tools")]
            id: None,
        });
        p.present_view(&report, &screen);
        assert!(
            wait_until(|| drawn
                .lock()
                .unwrap()
                .iter()
                .any(|d| matches!(d, Drawn::Skin(..)))),
            "the skin draws"
        );
        f(p, &report, &screen, &faults, &drawn);
    });
}

/// The last skin frame drawn.
fn last_skin(drawn: &Mutex<Vec<Drawn>>) -> Option<Vec<Quad>> {
    drawn.lock().unwrap().iter().rev().find_map(|d| match d {
        Drawn::Skin(q, _) => Some(q.clone()),
        Drawn::Page(_) => None,
    })
}

/// Holds the result skin's settles while it lives, and lets them go when
/// dropped, a panic's unwinding included, so a failing test strands no
/// settle.
struct SettleHold;

impl SettleHold {
    fn new() -> Self {
        crate::dev::hold_result_settles(true);
        Self
    }
}

impl Drop for SettleHold {
    fn drop(&mut self) {
        crate::dev::hold_result_settles(false);
    }
}

/// The fade-out dot's quad in [`TIMER2_DOT`]'s skins.
fn fading(q: &Quad) -> bool {
    q.rect[0] == 100.0
}

/// A dot whose alpha rises from 0 to 255 over 2 s of timer 2, then holds
/// (`loop = 2000`: the default loop of 0 would wrap it every 2 s).
const TIMER2_DOT: &str = "{ id = \"dot\", timer = 2, loop = 2000, dst = { { time = 0, x = 100, y = 600, w = 8, h = 8, a = 0 }, \
     { time = 2000, x = 100, y = 600, w = 8, h = 8, a = 255 } } },";

/// The fade-out dot's alpha in the last skin frame drawn.
fn fade_alpha(drawn: &Mutex<Vec<Drawn>>) -> Option<f32> {
    last_skin(drawn)?
        .iter()
        .find(|q| fading(q))
        .map(|q| q.color[3])
}

/// C5b through the real `PlayerRig`: a leaving command's outro begun
/// (`begin_outro`), then its `accept_leaving`, whose settle of the result
/// skin holds: meanwhile every frame keeps the pinned view, and the
/// fade-out (timer 2) rises.
#[test]
fn a_held_settle_keeps_the_pinned_view_while_it_fades() {
    use game::Rig as _;
    on_shown_result("", TIMER2_DOT, |p, _, _, faults, drawn| {
        let view: Vec<Quad> = last_skin(drawn).unwrap();
        assert!(!view.iter().any(fading), "no fade before the outro");
        assert!(p.result_timing().is_some(), "the skin's timing applies");
        let hold = SettleHold::new();
        let at = p.now();
        assert!(
            matches!(p.begin_outro(at, None), game::Outro::Begun(Some(_))),
            "the pinned view's timing"
        );
        let frames = std::thread::scope(|s| {
            let watch = s.spawn(move || {
                let hold = hold;
                assert!(wait_until(crate::dev::result_settle_holding));
                drawn.lock().unwrap().clear();
                std::thread::sleep(Duration::from_millis(600));
                let frames = drawn.lock().unwrap().clone();
                drop(hold);
                frames
            });
            p.accept_leaving(game::MenuCommand::Back);
            watch.join().unwrap()
        });
        assert!(
            frames.len() >= 3,
            "frames drew during the hold: {}",
            frames.len()
        );
        let mut alphas = Vec::new();
        for d in &frames {
            let Drawn::Skin(q, _) = d else {
                panic!("the skin draws during the hold: {d:?}");
            };
            let rest: Vec<Quad> = q.iter().filter(|q| !fading(q)).copied().collect();
            assert_eq!(rest, view, "the pinned view's quads");
            let fade = q.iter().find(|q| fading(q)).expect("the fade draws");
            alphas.push(fade.color[3]);
        }
        assert!(alphas.windows(2).all(|w| w[0] <= w[1]), "{alphas:?}");
        assert!(alphas.last() > alphas.first(), "the fade rises: {alphas:?}");
        // Timer 2 itself: started at the pin, and running (how far it ran
        // is the rising fade above; the last frame may be built up to a
        // frame before the hold's sleep ends, so no wall-clock bound).
        let (now, start) = faults.timers.lock().unwrap().expect("a frame built");
        assert_eq!(start, Some(at), "timer 2 starts at the pin");
        assert!(now.0 > at.0, "{now:?} {at:?}");
        assert_eq!(p.render.result_status().pinned, Some(1));
    });
}

/// Through the real `PlayerRig`, once a command is accepted (a 5 s outro,
/// and a 1 ms one already over, as after a settle held past it): a view
/// shown again asks for no layout, though it does before (D4). A screen
/// built again keeps the lifetime, the skin's timing and timer 2's
/// progress (C22: also when the screen is lost after the fade has ended,
/// which a restart would show as a lower alpha), until the flow serves
/// the command, after which the next screen's preparation leaves; one
/// whose result restore is refused
/// draws the pinned view's built-in page and takes the timing away, so
/// the flow serves the command at its next look (C23's timing half; the
/// flow's half is `game`'s fallback-during-an-outro test).
#[test]
fn a_screen_built_again_mid_outro_keeps_or_drops_the_timing() {
    use game::Rig as _;
    // The fade read `wait` ms after the outro began, and again `after` ms
    // after the screen is built again: one case reads at 1.8 s and past
    // 2 s, across the end of the fade, and one builds the screen again
    // after the fade has finished.
    for (fadeout, refuse, wait, after) in [
        (5_000, false, 400, 0),
        (5_000, true, 400, 0),
        (1, false, 400, 0),
        (5_000, false, 1_800, 400),
        (5_000, false, 2_200, 0),
    ] {
        on_shown_result(
            &format!("skin.fadeout = {fadeout}\n"),
            TIMER2_DOT,
            |p, report, screen, faults, drawn| {
                let t = p.result_timing().expect("the skin's timing");
                assert_eq!(t.outro, TimeUs(fadeout * 1_000));
                let asked = p.result_skin.requests();
                p.show_end(report, screen);
                assert_eq!(
                    p.result_skin.requests(),
                    asked + 1,
                    "a view asks for its layout"
                );
                // The result handed over shows at the look that reads its
                // first picture's acknowledgement.
                let end = Instant::now() + Duration::from_secs(20);
                while !p.result_view_shown() {
                    assert!(Instant::now() < end, "the result never showed");
                    p.idle_wait(TimeUs(p.now().0 + 20_000));
                }
                // The view shown again, waiting until then, is presented
                // before the pin, so the pin holds it.
                assert!(
                    wait_until(|| p.render.result_status().shown.map(|s| s.0) == Some(2)),
                    "the second view is presented after the acknowledgement"
                );
                let lifetime = p.render.result_status().lifetime;
                let at = p.now();
                assert_eq!(
                    p.begin_outro(at, None),
                    game::Outro::Begun(Some(t)),
                    "the pinned view's timing"
                );
                p.accept_leaving(game::MenuCommand::Back);
                let asked = p.result_skin.requests();
                p.show_end(report, screen);
                assert_eq!(p.result_skin.requests(), asked, "no layout while leaving");
                std::thread::sleep(Duration::from_millis(wait));
                let before = fade_alpha(drawn).expect("the fade draws");
                assert!(before > 0.0, "{before}");
                let (then, start) = faults.timers.lock().unwrap().expect("a frame built");
                assert_eq!(start, Some(at), "timer 2 starts at the pin");
                faults.refuse_restore.store(refuse, Ordering::Release);
                faults.lose.store(true, Ordering::Release);
                assert!(wait_until(|| p.screen_failed()));
                drawn.lock().unwrap().clear();
                p.prepare_screen().unwrap();
                assert!(p.presenter.current(), "the lifetime goes on mid-outro");
                if refuse {
                    assert!(wait_until(|| p.render.result_status().disabled.is_some()));
                    assert!(wait_until(|| drawn
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|d| matches!(d, Drawn::Page(_)))));
                    assert_eq!(p.result_timing(), None, "served at the next look");
                } else {
                    assert!(wait_until(|| last_skin(drawn).is_some()));
                    std::thread::sleep(Duration::from_millis(after));
                    let after = fade_alpha(drawn).expect("the fade draws");
                    // Timer 2 itself goes on from the pin, not the rebuild.
                    let (now, restarted) = faults.timers.lock().unwrap().expect("a frame built");
                    assert_eq!(restarted, Some(at), "timer 2 keeps its start");
                    assert!(now > then, "{then:?} then {now:?}");
                    let all: Vec<Option<f32>> = drawn
                        .lock()
                        .unwrap()
                        .iter()
                        .map(|d| match d {
                            Drawn::Skin(q, _) => q.iter().find(|q| fading(q)).map(|q| q.color[3]),
                            Drawn::Page(_) => None,
                        })
                        .collect();
                    let b = *p.render.shared.barrier();
                    assert!(
                        after >= before,
                        "timer 2 goes on ({fadeout} ms outro): {before} then {after}; {all:?}; at {at:?} now {:?} barrier {b:?}",
                        p.now()
                    );
                    assert_eq!(p.result_timing(), Some(t));
                    assert_eq!(p.render.result_status().pinned, Some(2));
                }
                // Shown again after the recovery: still no layout, and
                // the same lifetime.
                let asked = p.result_skin.requests();
                p.show_end(report, screen);
                assert_eq!(p.result_skin.requests(), asked);
                assert_eq!(p.render.result_status().lifetime, lifetime);
                assert!(p.presenter.current());
                // Served: the next screen's preparation leaves.
                p.serve_leaving(game::MenuCommand::Back);
                p.prepare_screen().unwrap();
                assert!(!p.presenter.current(), "left once served");
            },
        );
    }
}

/// The timing is the presented view's, not the newest published one's: a
/// built-in view B published and prepared while skin view A is presented
/// leaves A's timing, which the pin gives with A; once B is presented
/// there is none (the S10-09i i-4 state table's T7).
#[test]
fn the_timing_is_the_presented_views() {
    for pin in [false, true] {
        let (_dir, skin) = fixed_skin();
        let rig = Rig::new();
        let lifetime = installed(&rig, &skin);
        let want = Some(skin.skin().result_timing());
        assert_eq!(rig.link.presented_timing(), None, "nothing presented");
        let a = payload(1, Some(&skin), 1_111, &[]);
        let b = payload(2, None, 2_222, &[]);
        show(&rig.link, lifetime, 1, &a, &Overlay::default());
        assert!(rig.until(|d| skin_frame(d).is_some()));
        assert_eq!(rig.link.presented_timing(), want);
        rig.gate.hold();
        show(&rig.link, lifetime, 2, &b, &Overlay::default());
        rig.gate.step();
        assert_eq!(rig.link.presented_timing(), want, "B is not presented");
        if pin {
            assert_eq!(rig.link.pin_result(TimeUs(5), None), Some((Some(1), want)));
            rig.clear();
            rig.gate.release();
            assert!(rig.until(|d| skin_frame(d).is_some()));
            assert_eq!(rig.link.presented_timing(), want, "A stays pinned");
            // A new lifetime: what the render thread still draws of the
            // old one gives no timing.
            rig.link.begin_result();
            rig.clear();
            rig.link.show_end(EndPage::default());
            assert!(rig.link.fence(Duration::from_secs(10)));
            assert!(wait_until(|| !rig.drawn().is_empty()));
            assert_eq!(rig.link.presented_timing(), None, "another lifetime's");
        } else {
            rig.gate.release();
            assert!(rig.until(|d| matches!(d, Drawn::Page(_))));
            assert!(wait_until(|| rig.link.presented_timing().is_none()));
        }
        rig.link.exit();
    }
}

/// Holds one link's result frames after their lifetime's check while it
/// lives, and lets them go when dropped, a panic's unwinding included.
struct FrameHold<'a>(&'a RenderLink);

impl<'a> FrameHold<'a> {
    fn new(link: &'a RenderLink) -> Self {
        link.shared.result_frame_hold[0].store(true, Ordering::SeqCst);
        Self(link)
    }

    fn holding(&self) -> bool {
        self.0.shared.result_frame_hold[1].load(Ordering::SeqCst)
    }
}

impl Drop for FrameHold<'_> {
    fn drop(&mut self) {
        self.0.shared.result_frame_hold[0].store(false, Ordering::SeqCst);
    }
}

/// The gate and the pin judge one view (T13): with built-in view A
/// sampled (no gate), skin view B, whose input time is 5 s, commits before
/// the pin; a press then is refused under the pin's lock, nothing set,
/// and one after B's input time is pinned with B's timing.
#[test]
fn the_gate_and_the_pin_judge_one_view() {
    let (_dir, skin) = lua_skin("skin.input = 5000\n", "");
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    let a = payload(1, None, 1_111, &[]);
    let b = payload(2, Some(&skin), 2_222, &[]);
    show(&rig.link, lifetime, 1, &a, &Overlay::default());
    assert!(rig.until(|d| matches!(d, Drawn::Page(_))));
    assert_eq!(rig.link.presented_timing(), None, "A: no gate sampled");
    show(&rig.link, lifetime, 2, &b, &Overlay::default());
    assert!(rig.until(|d| skin_frame(d).is_some()));
    let t = skin.skin().result_timing();
    assert_eq!(t.input, TimeUs(5_000_000));
    // The input time counts from the origin A's commit set; the flow's
    // start, read before it, is earlier.
    let o = rig.link.presented_origin().expect("A committed");
    let start = TimeUs(0);
    assert_eq!(
        rig.link.pin_result(TimeUs(o.0 + 1_000_000), Some(start)),
        None
    );
    assert_eq!(rig.link.result_status().pinned, None);
    assert_eq!(
        rig.link.pin_result(TimeUs(o.0 + 5_000_000), Some(start)),
        Some((Some(2), Some(t)))
    );
    rig.link.exit();
}

/// A frame of the old result held after its lifetime's check while a new
/// lifetime begins records the old view's timing at its commit point; the
/// new lifetime does not read it (T12).
#[test]
fn a_frame_of_the_old_lifetime_gives_the_new_one_no_timing() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    let a = payload(1, Some(&skin), 1_111, &[]);
    show(&rig.link, lifetime, 1, &a, &Overlay::default());
    assert!(rig.until(|d| skin_frame(d).is_some()));
    assert!(rig.link.presented_timing().is_some());
    let hold = FrameHold::new(&rig.link);
    let notice = Overlay {
        lines: vec!["NOTICE".to_owned()],
        ..Overlay::default()
    };
    show(&rig.link, lifetime, 2, &a, &notice);
    assert!(wait_until(|| hold.holding()));
    rig.link.begin_result();
    drop(hold);
    assert!(rig.link.fence(Duration::from_secs(10)));
    assert_eq!(rig.link.presented_timing(), None);
    rig.link.exit();
}

/// An end page the render thread takes while it still holds a view of a
/// lifetime that ended (a frame of it drawn after the new lifetime began)
/// draws: the old view does not stand in for it, and its lifetime's check
/// leaves nothing undrawn.
#[test]
fn an_end_page_after_the_lifetime_ended_draws() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    let a = payload(1, Some(&skin), 1_111, &[]);
    show(&rig.link, lifetime, 1, &a, &Overlay::default());
    assert!(rig.until(|d| skin_frame(d).is_some()));
    let hold = FrameHold::new(&rig.link);
    assert!(wait_until(|| hold.holding()));
    rig.link.begin_result();
    rig.link.show_end(EndPage {
        lines: vec!["page".to_owned()],
        graphs: None,
        #[cfg(feature = "dev-tools")]
        id: None,
    });
    drop(hold);
    assert!(rig.until(|d| *d == Drawn::Page(vec!["page".to_owned()])));
    assert_eq!(rig.link.presented_timing(), None);
    rig.link.exit();
}

/// A rig whose render thread draws the decision page `decision` while it
/// still holds a view of a result lifetime that ended: a frame of it,
/// held after its lifetime's check, is let go once the result was left
/// and the page sent, so no frame of it checks again.
fn decision_over_an_ended_result() -> (tempfile::TempDir, Rig) {
    let (dir, skin) = fixed_skin();
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    let a = payload(1, Some(&skin), 1_111, &[]);
    show(&rig.link, lifetime, 1, &a, &Overlay::default());
    assert!(rig.until(|d| skin_frame(d).is_some()));
    let hold = FrameHold::new(&rig.link);
    assert!(wait_until(|| hold.holding()));
    rig.link.end_result();
    rig.link.show_decision(
        crate::menu_screen::MenuPage {
            lines: vec![crate::menu_screen::MenuLine {
                text: "decision".to_owned(),
                style: crate::menu_screen::LineStyle::Title,
            }],
            caret: None,
        },
        None,
    );
    drop(hold);
    assert_eq!(rig.link.mode(), "menu:decision");
    (dir, rig)
}

/// The screen's recovery under the decision page draws that page on: the
/// view of a result lifetime that ended does not take its place (design.md
/// "Play Lifecycle": a recovery draws the current state's presentation).
#[test]
fn a_recovery_keeps_the_decision_page_over_an_ended_result() {
    let (_dir, rig) = decision_over_an_ended_result();
    rig.link.prepare().unwrap();
    assert_eq!(rig.link.mode(), "menu:decision");
    rig.link.exit();
}

/// A result signal that takes nothing leaves the mode as it was: the view
/// of a lifetime that ended does not take the decision page's place.
#[test]
fn an_empty_result_signal_keeps_the_decision_page() {
    let (_dir, rig) = decision_over_an_ended_result();
    rig.link.send(crate::render_side::RenderMsg::ResultChanged);
    assert_eq!(rig.link.mode(), "menu:decision");
    rig.link.exit();
}

/// A command accepted on a built-in view pins it all the same: a skin view
/// prepared before the frame's wait never commits after it, and the pin
/// gives no timing, so the command acts at once (T2).
#[test]
fn a_built_in_view_is_pinned_over_a_prepared_skin() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    let a = payload(1, None, 1_111, &[]);
    let b = payload(2, Some(&skin), 2_222, &[]);
    show(&rig.link, lifetime, 1, &a, &Overlay::default());
    assert!(rig.until(|d| matches!(d, Drawn::Page(_))));
    rig.gate.hold();
    show(&rig.link, lifetime, 2, &b, &Overlay::default());
    rig.gate.step();
    assert_eq!(rig.link.pin_result(TimeUs(5), None), Some((Some(1), None)));
    rig.clear();
    rig.gate.release();
    assert!(rig.until(|d| matches!(d, Drawn::Page(_))));
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        rig.drawn().iter().all(|d| matches!(d, Drawn::Page(_))),
        "the prepared skin view never draws"
    );
    let status = rig.link.result_status();
    assert_eq!((status.pinned, status.shown), (Some(1), Some((1, false))));
    assert_eq!(rig.link.presented_timing(), None);
    rig.link.exit();

    // Accepted before anything was presented: the barrier holds all the
    // same, and a skin view arriving after never draws.
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    assert_eq!(rig.link.pin_result(TimeUs(5), None), Some((None, None)));
    rig.clear();
    show(&rig.link, lifetime, 1, &b, &Overlay::default());
    assert!(wait_until(|| !rig.drawn().is_empty()), "a frame drew");
    std::thread::sleep(Duration::from_millis(100));
    assert!(
        rig.drawn().iter().all(|d| skin_frame(d).is_none()),
        "nothing of the skin draws"
    );
    assert_eq!(rig.link.result_status().shown, None);
    rig.link.exit();
}

/// The fixture's runtime offered to the rig's cell, retained as the fixed
/// skin's, with a flag its VM's destruction sets.
fn offer(
    rig: &Rig,
    script: crate::screens::ScriptSlot,
) -> (crate::screens::ScriptGen, Arc<AtomicBool>) {
    let gone = Arc::new(AtomicBool::new(false));
    script.on_vm_drop(&gone);
    let g = rig.link.scripts().offer_result(script, true).unwrap();
    (g, gone)
}

/// [`payload`] of the view `key` of generation `g`.
fn scripted(
    key: ViewKey,
    skin: &Arc<render::SkinBundle>,
    g: Option<crate::screens::ScriptGen>,
    lines: &[&str],
) -> Arc<ViewPayload> {
    let ex = 1_000 * u32::try_from(key).unwrap();
    let p = payload(key, Some(skin), ex, lines);
    Arc::new(ViewPayload {
        chart: None,
        script: g,
        ..(*p).clone()
    })
}

/// C3 on result (s7-07-design §3.4): the presented view's runtime runs
/// from its first frame, between the commit point and the build; a switch
/// keeping the layout keeps the same VM, running, and a view of no
/// generation draws the callback's constant.
#[test]
fn a_result_runtime_runs_from_the_first_frame_across_a_switch() {
    let (_dir, skin, script) = scripted_skin("calls = calls + 1 return calls > 0");
    let rig = Rig::new();
    let (g, gone) = offer(&rig, script.untimed());
    let lifetime = installed(&rig, &skin);
    show(
        &rig.link,
        lifetime,
        1,
        &scripted(1, &skin, None, &[]),
        &Overlay::default(),
    );
    assert!(rig.until(|d| skin_frame(d).is_some()));
    let Some(Drawn::Skin(without, _)) = rig.drawn().last().cloned() else {
        panic!();
    };
    let dots = |q: &[Quad]| skin_quads(q).len();
    rig.clear();
    show(
        &rig.link,
        lifetime,
        2,
        &scripted(2, &skin, Some(g), &[]),
        &Overlay::default(),
    );
    assert!(rig.until(|d| skin_frame(d).is_some_and(|(q, _)| dots(q) == dots(&without) + 1)));
    // Every frame without the dot is view 1's (its own EX): view 2's
    // first frame already drew the runtime's answer.
    for d in rig.drawn() {
        if let Some((q, _)) = skin_frame(&d)
            && dots(q) == dots(&without)
        {
            assert_eq!(q, without.as_slice(), "view 2 drawn without its runtime");
        }
    }
    rig.clear();
    show(
        &rig.link,
        lifetime,
        3,
        &scripted(3, &skin, Some(g), &["B"]),
        &Overlay::default(),
    );
    assert!(
        rig.until(|d| skin_frame(d).is_some_and(|(q, n)| n == 1 && dots(q) == dots(&without) + 1))
    );
    for d in rig.drawn() {
        if let Some((q, _)) = skin_frame(&d) {
            assert_eq!(dots(q), dots(&without) + 1, "never a frame without it");
        }
    }
    assert!(!gone.load(Ordering::Acquire), "the same VM");
    assert!(rig.link.scripts().result_live(Some(g)));
    rig.link.exit();
}

/// C8 and O4 on result (plan r2 MEDIUM): a runtime stopping with the strip
/// full, no new presentation coming, puts its notice on the strip: the
/// strip is prepared again, still 13 lines; the notice stays through an
/// overlay update and a device loss's rebuild, the VM kept.
#[test]
fn a_stopped_runtime_leads_a_full_strip() {
    let (_dir, skin, script) = scripted_skin("while true do end");
    let rig = Rig::new();
    let (g, gone) = offer(&rig, script);
    let lifetime = installed(&rig, &skin);
    let lines: Vec<String> = (0..13).map(|i| format!("FACT {i}")).collect();
    let lines: Vec<&str> = lines.iter().map(String::as_str).collect();
    let p = scripted(1, &skin, Some(g), &lines);
    show(&rig.link, lifetime, 1, &p, &Overlay::default());
    assert!(rig.until(|d| skin_frame(d).is_some_and(|(_, n)| n == 13)));
    let strip = || rig.faults.strip.lock().unwrap().clone();
    let notice = crate::screens::ScriptStop::Budget.notice();
    // Frames past the budget stop it; its notice leads, the last fact
    // yields.
    assert!(wait_until(
        || strip().first().map(String::as_str) == Some(notice)
    ));
    let stopped = strip();
    let facts: Vec<String> = lines.iter().take(12).map(|l| (*l).to_owned()).collect();
    assert_eq!(stopped.get(1..), Some(facts.as_slice()));
    show(&rig.link, lifetime, 2, &p, &Overlay::default());
    assert!(wait_until(|| rig.link.result_status().revision == 2));
    rig.clear();
    assert!(rig.until(|d| skin_frame(d).is_some_and(|(_, n)| n == 13)));
    assert_eq!(strip(), stopped, "an overlay update keeps it");
    rig.faults.lose.store(true, Ordering::Release);
    assert!(wait_until(|| rig.link.failed()));
    rig.faults.strip.lock().unwrap().clear();
    rig.clear();
    rig.link.prepare().unwrap();
    assert!(rig.until(|d| skin_frame(d).is_some_and(|(_, n)| n == 13)));
    assert_eq!(strip(), stopped, "a rebuild keeps it");
    assert!(!gone.load(Ordering::Acquire));
    assert_eq!(
        rig.link.scripts().result_state(g),
        crate::screens::GenState::Live { stopped: true }
    );
    rig.link.exit();
}

/// The run's disable retires the presented runtime, after the frame, and
/// the parking before a play parks it: nothing runs in the interval.
#[test]
fn a_disabled_result_retires_its_runtime() {
    let (_dir, skin, script) = scripted_skin("calls = calls + 1 return calls > 0");
    let rig = Rig::new();
    let (g, gone) = offer(&rig, script);
    let lifetime = installed(&rig, &skin);
    show(
        &rig.link,
        lifetime,
        1,
        &scripted(1, &skin, Some(g), &[]),
        &Overlay::default(),
    );
    assert!(rig.until(|d| skin_frame(d).is_some()));
    assert!(
        !rig.link.scripts().result_parked(g),
        "the presented view runs it"
    );
    assert!(rig.link.park_scripts());
    assert!(
        rig.link.scripts().result_parked(g),
        "parked before the play"
    );
    let interval = rig.link.scripts().open_interval();
    assert!(!gone.load(Ordering::Acquire));
    let _closed = interval.close();
    rig.faults.refuse.store(true, Ordering::Release);
    assert!(wait_until(|| rig.link.result_status().disabled.is_some()));
    assert!(wait_until(|| gone.load(Ordering::Acquire)));
    assert!(!rig.link.scripts().result_live(Some(g)));
    rig.link.exit();
}

/// Waits until `rig` drew `n` more skin frames.
fn frames(rig: &Rig, n: usize) -> Vec<Vec<Quad>> {
    let seen = rig.drawn().len();
    assert!(wait_until(|| rig.drawn().len() >= seen + n));
    rig.drawn()
        .into_iter()
        .skip(seen)
        .filter_map(|d| skin_frame(&d).map(|(q, _)| q.to_vec()))
        .collect()
}

/// C5, C6, C11 on result (code r1 M3): views of one generation switching
/// fast (the presented, prepared and replaced holders taken), the parking
/// before a play parks the runtime; while the play's interval is open the
/// screen draws on with the callback's constant and no call is made; after
/// the close the runtime resumes, its calls going on; the close's token
/// destroys it.
#[test]
fn a_result_runtime_rests_through_a_play_and_resumes() {
    let (_dir, skin, script) = scripted_skin("calls = calls + 1 return calls > 0");
    let rig = Rig::new();
    let (g, gone) = offer(&rig, script.untimed());
    let lifetime = installed(&rig, &skin);
    for key in 1..=4 {
        show(
            &rig.link,
            lifetime,
            key,
            &scripted(key, &skin, Some(g), &[]),
            &Overlay::default(),
        );
    }
    let dots = |q: &[Quad]| skin_quads(q).len();
    let running = frames(&rig, 3);
    let with = running.last().map(|q| dots(q)).unwrap();
    // As a retry leaves the result (the lifetime ends) before the play's
    // preparation asks for the parking: no result frame takes it again.
    rig.link.end_result();
    assert!(rig.link.park_scripts());
    let scripts = rig.link.scripts();
    assert!(scripts.result_parked(g));
    let calls = scripts.result_calls(g).unwrap();
    assert!(calls >= 3, "{calls}");
    let interval = scripts.open_interval();
    let lifetime = rig.link.begin_result();
    show(
        &rig.link,
        lifetime,
        5,
        &scripted(5, &skin, Some(g), &[]),
        &Overlay::default(),
    );
    let during = frames(&rig, 4);
    assert!(!during.is_empty(), "the view drew in the interval");
    for q in during {
        assert_eq!(
            dots(&q),
            with - 1,
            "the constant while the interval is open"
        );
    }
    assert_eq!(
        scripts.result_calls(g),
        Some(calls),
        "no call in the interval"
    );
    let closed = interval.close();
    assert!(wait_until(|| rig
        .drawn()
        .last()
        .and_then(skin_frame)
        .is_some_and(|(q, _)| dots(q) == with)));
    assert!(rig.link.park_scripts());
    assert!(scripts.result_calls(g).unwrap() > calls, "resumed");
    assert!(!gone.load(Ordering::Acquire));
    // The run's end, the interval closed: the token destroys it.
    assert!(scripts.retire(crate::screens::Quiet::Closed(closed)));
    assert!(gone.load(Ordering::Acquire));
    rig.link.exit();
}

/// C13 on result: the render thread panicking while a play's interval is
/// open (READY through Closing) destroys no result VM; the game thread's
/// retirement with the close's token does.
#[test]
fn a_render_panic_in_a_play_leaves_the_result_runtime_to_the_game_thread() {
    let (_dir, skin, script) = scripted_skin("calls = calls + 1 return calls > 0");
    let rig = Rig::new();
    let (g, gone) = offer(&rig, script);
    let lifetime = installed(&rig, &skin);
    show(
        &rig.link,
        lifetime,
        1,
        &scripted(1, &skin, Some(g), &[]),
        &Overlay::default(),
    );
    let _ = frames(&rig, 3);
    assert!(rig.link.park_scripts());
    let scripts = rig.link.scripts().clone();
    let interval = scripts.open_interval();
    rig.link.send(crate::render_side::RenderMsg::Panic);
    assert!(wait_until(|| scripts.orphaned()));
    assert!(!gone.load(Ordering::Acquire));
    assert!(scripts.inactive().is_none());
    assert!(scripts.retire(crate::screens::Quiet::Closed(interval.close())));
    assert!(gone.load(Ordering::Acquire));
    assert_eq!(rig.link.exit(), crate::render_side::RenderEnd::Panicked);
}

/// C13 on result: a retirement while the view runs is terminal: the
/// running slot gives its runtime back, which is destroyed; a recovery
/// (a device lost, the screen built again) draws the callback's constant,
/// and a new lifetime's view of the retired generation never commits:
/// neither resumes it.
#[test]
fn a_retired_result_runtime_is_not_resumed() {
    let (_dir, skin, script) = scripted_skin("calls = calls + 1 return calls > 0");
    let rig = Rig::new();
    let (g, gone) = offer(&rig, script.untimed());
    let lifetime = installed(&rig, &skin);
    show(
        &rig.link,
        lifetime,
        1,
        &scripted(1, &skin, Some(g), &[]),
        &Overlay::default(),
    );
    let dots = |q: &[Quad]| skin_quads(q).len();
    let with = frames(&rig, 3).last().map(|q| dots(q)).unwrap();
    let scripts = rig.link.scripts().clone();
    assert!(scripts.retire(crate::screens::Quiet::Inactive(scripts.inactive().unwrap())));
    assert!(
        wait_until(|| gone.load(Ordering::Acquire)),
        "given back and destroyed"
    );
    let after = frames(&rig, 3);
    assert_eq!(after.last().map(|q| dots(q)), Some(with - 1));
    rig.faults.lose.store(true, Ordering::Release);
    assert!(wait_until(|| rig.link.failed()));
    rig.clear();
    rig.link.prepare().unwrap();
    assert!(rig.until(|d| skin_frame(d).is_some_and(|(q, _)| dots(q) == with - 1)));
    let next = rig.link.begin_result();
    show(
        &rig.link,
        next,
        1,
        &scripted(2, &skin, Some(g), &[]),
        &Overlay::default(),
    );
    // Every frame drawn so far taken, the one after them drawn: no frame
    // of the old lifetime arrives after the clear.
    assert!(rig.link.fence(Duration::from_secs(10)));
    rig.clear();
    // A view of the retired generation never commits: the new lifetime
    // draws the built-in page, and no frame runs the runtime.
    assert!(
        rig.until(|d| matches!(d, Drawn::Page(_))),
        "the new lifetime draws the built-in page"
    );
    assert!(rig.drawn().iter().filter_map(skin_frame).count() == 0);
    assert!(!scripts.result_live(Some(g)));
    rig.link.exit();
}

/// The render link's lifetimes reach the cell (code r3 LOW): a view
/// presented in a lifetime protects its generation while that lifetime is
/// current, `end_result` ends that before the next lifetime begins, and
/// the next lifetime starts with nothing presented.
#[test]
fn the_links_lifetimes_scope_the_presented_protection() {
    let (_dir, skin, script) = scripted_skin("calls = calls + 1 return calls > 0");
    let rig = Rig::new();
    let gone = Arc::new(AtomicBool::new(false));
    script.on_vm_drop(&gone);
    let scripts = rig.link.scripts().clone();
    let g = scripts.offer_result(script, false).unwrap();
    let lifetime = installed(&rig, &skin);
    scripts.set_published(Some(g));
    show(
        &rig.link,
        lifetime,
        1,
        &scripted(1, &skin, Some(g), &[]),
        &Overlay::default(),
    );
    assert!(wait_until(|| scripts.presented() == Some(g)));
    scripts.set_published(None);
    assert!(scripts.protected(g), "presented in the current lifetime");
    rig.link.end_result();
    assert_eq!(scripts.presented(), None, "no lifetime is current");
    assert!(!scripts.protected(g));
    let _next = rig.link.begin_result();
    assert_eq!(scripts.presented(), None);
    assert!(!scripts.protected(g));
    rig.link.exit();
}

/// The whole frames' build mark (dev-tools, the S7-07d gate) is set
/// before the result skin's build begins, where the frame's `cpu` starts
/// counting: the preparation span never holds the build.
#[test]
fn the_build_mark_precedes_the_result_build() {
    use super::super::{HeldView, PreparedView, ResultDrawn, ResultSide, Run, Shared};
    let (_dir, skin, script) = text_skin_scripted(
        1,
        "{ id = \"dot\", draw = function() calls = calls + 1 return calls > 0 end, \
         dst = { { x = 0, y = 700, w = 8, h = 8 } } },",
    );
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
    let scripts = crate::screens::Scripts::new();
    let g = scripts.offer_result(script, true);
    let mut side = ResultSide::new(1);
    let p = titled(0, Some(&skin), 1_000, &[], "A");
    let p = Arc::new(ViewPayload {
        chart: None,
        script: g,
        ..(*p).clone()
    });
    assert_eq!(
        screen.install(ScreenKind::Result, &skin, 2).unwrap(),
        Preparation::Ready
    );
    let mut state = skin.skin().prepare_result();
    screen
        .prepare_result(&skin, &mut state, &p.view, &p.labels)
        .unwrap()
        .unwrap();
    side.prepared = Some(PreparedView {
        held: HeldView {
            revision: 1,
            payload: Arc::clone(&p),
            overlay: Overlay::default(),
            state: Some(state),
            page: None,
            script: scripts.result_slot(g),
        },
        token: Some(token(2, &screen)),
    });
    for k in 0..5_i64 {
        let now = TimeUs(1_000_000 + 16_000 * k);
        let fadeout =
            Run::result_commit(&mut screen, Some(&mut side), &shared, &scripts, TimeUs(0));
        Run::result_visit(
            Some(&mut side),
            now,
            fadeout,
            &skin_model::AppReadouts::LAUNCH,
        );
        let _ = crate::dev::take_build_mark();
        let _ = render::take_build_start();
        let drawn = Run::result_build(
            &mut screen,
            Some(&mut side),
            now,
            fadeout,
            false,
            &skin_model::AppReadouts::LAUNCH,
        );
        assert_eq!(drawn, ResultDrawn::Built);
        screen.submit_result().unwrap();
        let mark = crate::dev::take_build_mark().expect("the build marked");
        let start = render::take_build_start().expect("the build began");
        assert!(mark <= start, "frame {k}: the build began before its mark");
        side.dropped = [None, None];
        // What the commit retired goes before the next cycle, outside
        // the spans counted.
        crate::render_side::settle_entries(&mut screen);
    }
}

/// What the whole frames of `lifetime` were charged with after the slow
/// preparation was lost and the screen recovered: no frame's preparation
/// before its wait overlaps the slow preparation's sleep, and a frame
/// after the sleep (the recovery's) was charged; and no frame was charged
/// more preparation than ran in its window, so the charged time is the
/// one recorded. The charges are told apart by when they ran.
fn assert_slow_preparation_uncharged(rig: &Rig, lifetime: u64) {
    let (began, ended) = rig
        .faults
        .slept
        .lock()
        .unwrap()
        .expect("the slow preparation ran");
    let probe = rig.link.frames_probe();
    assert_eq!(probe.result.0, Some(lifetime));
    assert_eq!(probe.charged.len(), probe.result.1.len());
    let charged = || probe.charged.iter().flatten();
    assert!(
        charged().all(|&(from, to, _)| to < began || ended < from),
        "a frame was charged with the lost preparation ({began:?}..{ended:?}): {probe:?}"
    );
    assert!(
        charged().all(|&(from, to, d)| d <= to.duration_since(from)),
        "a frame was charged more than its window: {probe:?}"
    );
    assert!(
        charged().any(|&(from, _, _)| ended < from),
        "no frame after the recovery was charged: {probe:?}"
    );
}

/// The whole frames (dev-tools, the S7-07d gate): a view whose 80 ms
/// preparation is followed by a lost device on its commit leaves none of
/// that preparation to the frames after the recovery.
#[test]
fn a_lost_attempts_preparation_is_not_charged_after_recovery() {
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
    rig.faults.slow_prepare.store(80, Ordering::Release);
    rig.faults.lose_after_prepare.store(true, Ordering::Release);
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
    assert!(rig.until(|d| skin_frame(d).is_some()));
    assert_slow_preparation_uncharged(&rig, lifetime);
    rig.link.exit();
}

/// The whole frames (dev-tools, the S7-07d gate): a view whose 80 ms
/// preparation fails on its strip as a lost device leaves none of that
/// preparation to the frames after the recovery.
#[test]
fn a_failed_preparation_is_not_charged_after_recovery() {
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
    rig.faults.slow_prepare.store(80, Ordering::Release);
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
    assert!(rig.until(|d| skin_frame(d).is_some()));
    assert_slow_preparation_uncharged(&rig, lifetime);
    rig.link.exit();
}

/// The whole frames (dev-tools, the S7-07d gate): a result frame's present
/// is timed before its cleanup, so the cleanup, which publishes the
/// status, sees every frame submitted already timed.
#[test]
fn a_result_present_is_timed_before_its_cleanup() {
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
    assert!(wait_until(|| rig.faults.submits.load(Ordering::Acquire) > 5));
    let cleanups = rig.faults.cleanups.lock().unwrap().clone();
    assert!(cleanups.iter().any(|&(s, _)| s > 0), "{cleanups:?}");
    assert!(
        cleanups.iter().all(|&(submits, timed)| timed >= submits),
        "a cleanup ran before its frame's present was timed: {cleanups:?}"
    );
    rig.link.exit();
}

/// The whole frames (dev-tools, the S7-07d gate): when the render thread
/// ends on a displayed result, that result lifetime's frames are still
/// reported to `tapestry measure select`.
#[test]
fn a_results_frames_are_reported_when_the_render_thread_ends() {
    // The skin's import takes the serial lock itself: imported first.
    let (_dir, skin) = fixed_skin();
    let _serial = crate::imports::tests::serial();
    crate::dev::measure_screens();
    let rig = Rig::new();
    // A lifetime other tests' rigs rarely reach, so the record found is
    // this one's.
    for _ in 0..6 {
        rig.link.begin_result();
        rig.link.end_result();
    }
    let lifetime = installed(&rig, &skin);
    show(
        &rig.link,
        lifetime,
        1,
        &payload(1, Some(&skin), 1_111, &[]),
        &Overlay::default(),
    );
    assert!(rig.until(|d| skin_frame(d).is_some()));
    rig.link.exit();
    let kept = crate::dev::measured_screens();
    assert!(
        kept.screens
            .iter()
            .any(|c| c.screen == "result" && c.lifetime == lifetime),
        "lifetime {lifetime}: {:?}",
        kept.screens
    );
}

/// The result frames built so far.
fn builds(rig: &Rig) -> Vec<Build> {
    rig.faults.builds.lock().unwrap().clone()
}

/// Waits until at least `n` result frames were built.
fn built(rig: &Rig, n: usize) -> bool {
    wait_until(|| rig.faults.builds.lock().unwrap().len() >= n)
}

/// The result's origin is the first committed view's sampled frame
/// instant (design.md "Rendering and Skins", Timers): the frame that
/// commits it reads `screen` at 0, and every later frame of the lifetime
/// keeps that start.
#[test]
fn the_first_committed_frame_reads_screen_at_zero() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    let a = payload(1, Some(&skin), 1_111, &[]);
    show(&rig.link, lifetime, 1, &a, &Overlay::default());
    assert!(built(&rig, 4));
    let b = builds(&rig);
    let (first, screen, _) = b[0];
    assert_eq!(screen, Some(first), "the first frame reads 0");
    assert!(b.iter().all(|&(_, s, _)| s == Some(first)), "{b:?}");
    assert_eq!(rig.link.presented_origin(), Some(first));
    rig.link.exit();
}

/// A first commit whose frame is never presented (its submission lost)
/// keeps its origin: the frames drawn after the screen is built again read
/// `screen` from that commit's instant.
#[test]
fn an_unpresented_first_commit_keeps_its_origin() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    rig.faults.lose.store(true, Ordering::Release);
    let a = payload(1, Some(&skin), 1_111, &[]);
    show(&rig.link, lifetime, 1, &a, &Overlay::default());
    assert!(wait_until(|| rig.link.failed()));
    assert!(
        rig.drawn().iter().all(|d| skin_frame(d).is_none()),
        "never presented"
    );
    rig.link.prepare().unwrap();
    // Frames drawn after the screen is built again.
    assert!(rig.until(|d| skin_frame(d).is_some()));
    assert!(built(&rig, 3));
    let b = builds(&rig);
    let (lost, screen, _) = b[0];
    assert_eq!(screen, Some(lost));
    assert!(
        b[1..]
            .iter()
            .all(|&(now, s, _)| s == Some(lost) && now > lost),
        "{b:?}"
    );
    assert_eq!(rig.link.presented_origin(), Some(lost));
    rig.link.exit();
}

/// A built-in first view starts the origin; a skin view committed after it
/// inherits the elapsed time instead of starting its own.
#[test]
fn a_skin_after_a_built_in_view_inherits_its_origin() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    let a = payload(1, None, 1_111, &[]);
    show(&rig.link, lifetime, 1, &a, &Overlay::default());
    assert!(rig.until(|d| matches!(d, Drawn::Page(_))));
    let o = rig
        .link
        .presented_origin()
        .expect("the built-in view committed");
    assert!(builds(&rig).is_empty(), "no skin frame yet");
    let b = payload(2, Some(&skin), 2_222, &[]);
    show(&rig.link, lifetime, 2, &b, &Overlay::default());
    assert!(built(&rig, 3));
    let b = builds(&rig);
    assert!(b[0].0 > o, "{b:?}");
    assert!(b.iter().all(|&(_, s, _)| s == Some(o)), "{o:?} {b:?}");
    assert_eq!(rig.link.presented_origin(), Some(o));
    rig.link.exit();
}

/// The skin's kept runtime and its build read one origin: a callback the
/// importer could not lower (its upvalue changes each call) runs on the
/// kept VM from the lifetime's first frame, and on every frame the
/// `screen` timer its host read is the time since the first commit, the
/// build's `screen` start, as frames advance.
#[test]
fn the_runtime_and_the_build_read_one_origin() {
    let (_dir, skin, script) = scripted_skin("calls = calls + 1 return calls > 0");
    assert!(script.holds(), "the callback runs on a kept runtime");
    let rig = Rig::new();
    let (g, _gone) = offer(&rig, script.untimed());
    let lifetime = installed(&rig, &skin);
    show(
        &rig.link,
        lifetime,
        1,
        &scripted(1, &skin, Some(g), &[]),
        &Overlay::default(),
    );
    let ran = || {
        rig.link
            .shared
            .result_runtime_timers
            .lock()
            .unwrap()
            .clone()
    };
    // Frames until one the runtime ran is 3 ms or more after the first.
    assert!(wait_until(|| {
        let r = ran();
        let ran_at: Vec<i64> = r
            .iter()
            .filter(|(_, t)| t.is_some())
            .map(|(now, _)| now.0)
            .collect();
        ran_at
            .first()
            .zip(ran_at.last())
            .is_some_and(|(f, l)| l - f >= 3_000)
    }));
    assert!(rig.link.scripts().result_live(Some(g)));
    let b = builds(&rig);
    let origin = b[0].0;
    let ran = ran();
    let mut readings = Vec::new();
    for &(now, timer) in &ran {
        let Some(&(_, screen, prepared)) = b.iter().find(|x| x.0 == now) else {
            continue;
        };
        let ms = i32::try_from((now.0 - origin.0) / 1_000).unwrap();
        assert_eq!(screen, Some(origin), "{b:?}");
        assert_eq!(timer, Some(ms), "the runtime's reading: {ran:?}");
        assert_eq!(prepared, Some(ms), "{b:?}");
        readings.push(ms);
    }
    assert!(readings.first() == Some(&0), "{readings:?}");
    assert!(readings.windows(2).all(|w| w[0] <= w[1]), "{readings:?}");
    assert!(readings.last().is_some_and(|&l| l >= 3), "{readings:?}");
    rig.link.exit();
}

/// The origin survives a switch to another view and a screen built again.
#[test]
fn the_origin_survives_a_switch_and_a_screen_built_again() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    let a = payload(1, Some(&skin), 1_111, &[]);
    show(&rig.link, lifetime, 1, &a, &Overlay::default());
    assert!(rig.until(|d| skin_frame(d).is_some()));
    let o = rig.link.presented_origin().expect("A committed");
    let b = payload(2, Some(&skin), 2_222, &["B"]);
    show(&rig.link, lifetime, 2, &b, &Overlay::default());
    assert!(rig.until(|d| skin_frame(d).is_some_and(|(_, n)| n == 1)));
    rig.faults.lose.store(true, Ordering::Release);
    assert!(wait_until(|| rig.link.failed()));
    rig.link.prepare().unwrap();
    let n = rig.faults.submits.load(Ordering::Acquire);
    assert!(wait_until(
        || rig.faults.submits.load(Ordering::Acquire) >= n + 3
    ));
    assert_eq!(rig.link.result_status().shown, Some((2, true)));
    let b = builds(&rig);
    assert!(b.iter().all(|&(_, s, _)| s == Some(o)), "{o:?} {b:?}");
    assert_eq!(rig.link.presented_origin(), Some(o));
    rig.link.exit();
}

/// A new lifetime takes its own origin: none while a frame of the old one
/// is held past its check, none after that frame's stale commit, and the
/// new lifetime's first commit's instant after it.
#[test]
fn a_new_lifetime_takes_its_own_origin() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    let a = payload(1, Some(&skin), 1_111, &[]);
    show(&rig.link, lifetime, 1, &a, &Overlay::default());
    assert!(rig.until(|d| skin_frame(d).is_some()));
    assert!(rig.link.presented_origin().is_some());
    let hold = FrameHold::new(&rig.link);
    let notice = Overlay {
        lines: vec!["NOTICE".to_owned()],
        ..Overlay::default()
    };
    show(&rig.link, lifetime, 2, &a, &notice);
    assert!(wait_until(|| hold.holding()));
    let next = rig.link.begin_result();
    assert_eq!(rig.link.presented_origin(), None, "before the release");
    drop(hold);
    assert!(rig.link.fence(Duration::from_secs(10)));
    assert_eq!(rig.link.presented_origin(), None, "after the stale commit");
    rig.faults.builds.lock().unwrap().clear();
    show(&rig.link, next, 1, &a, &Overlay::default());
    assert!(built(&rig, 1));
    let (first, screen, _) = builds(&rig)[0];
    assert_eq!(screen, Some(first));
    assert_eq!(rig.link.presented_origin(), Some(first));
    rig.link.exit();
}

/// A pressed command's gate counts from the first committed frame under
/// the pin's lock, though the flow read its start before that commit.
#[test]
fn the_pressed_gate_counts_from_the_first_committed_frame() {
    let (_dir, skin) = lua_skin("skin.input = 5000\n", "");
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    let a = payload(1, Some(&skin), 1_111, &[]);
    show(&rig.link, lifetime, 1, &a, &Overlay::default());
    assert!(rig.until(|d| skin_frame(d).is_some()));
    let o = rig.link.presented_origin().expect("A committed");
    // The flow's start, read before the commit: earlier than the origin.
    let start = TimeUs(0);
    assert!(o.0 >= 1_000, "{o:?}");
    assert_eq!(
        rig.link.pin_result(TimeUs(o.0 + 4_999_000), Some(start)),
        None
    );
    assert_eq!(
        rig.link.pin_result(TimeUs(o.0 + 5_000_000), Some(start)),
        Some((Some(1), Some(skin.skin().result_timing())))
    );
    rig.link.exit();
}

/// A refusal on a healthy device disables the kind: the timing goes, so
/// commands act at once, and the origin stays.
#[test]
fn a_refusal_keeps_the_origin_and_removes_the_timing() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    let a = payload(1, Some(&skin), 1_111, &[]);
    show(&rig.link, lifetime, 1, &a, &Overlay::default());
    assert!(rig.until(|d| skin_frame(d).is_some()));
    let o = rig.link.presented_origin().expect("A committed");
    assert!(rig.link.presented_timing().is_some());
    rig.faults.refuse.store(true, Ordering::Release);
    assert!(wait_until(|| rig.link.result_status().disabled.is_some()));
    assert_eq!(rig.link.presented_timing(), None);
    assert_eq!(rig.link.presented_origin(), Some(o));
    rig.link.exit();
}

/// A result frame's time and its application readouts are one clock
/// reading, as a selection frame's are (design.md "Rendering and Skins",
/// play readouts): each frame's time is its readouts' uptime on the clock
/// started at launch, though the frame stalls 2 ms after that reading;
/// and the state the frame's visit prepared, which the build draws, holds
/// those readouts.
#[test]
fn a_result_frames_time_and_readouts_are_one_reading() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    rig.link.shared.result_stall.store(true, Ordering::Release);
    let a = payload(1, Some(&skin), 1_111, &[]);
    show(&rig.link, lifetime, 1, &a, &Overlay::default());
    assert!(wait_until(|| rig.faults.uptimes.lock().unwrap().len() >= 5));
    rig.link.exit();
    let seen = rig.faults.uptimes.lock().unwrap().clone();
    for (now, app, held) in seen {
        assert_eq!(app.uptime_us, now.0, "{now:?}");
        let second = i32::try_from(app.uptime_us / 1_000_000 % 60).unwrap();
        assert_eq!(held, [i32::from(app.local.year), second], "{now:?}");
    }
}

/// Holds `rig`'s frames from now on, before any is drawn: the next frame
/// waits for [`Gate::step`].
fn paced(rig: &Rig) {
    rig.gate.pace.store(true, Ordering::Release);
}

/// V5's render half (design.md "State Model", system sounds, Result): a
/// Result entry is acknowledged at its first result picture, a frame whose
/// commit point holds a view of the entry, the skin's or a built-in one
/// (an attempt's fallback), once that frame ends shown or deliberately not
/// shown (a window minimized or occluded); a frame that is to be tried
/// again (a swap chain image timed out or outdated, the offscreen pacer's
/// skip) or that was not built acknowledges nothing, and the next one
/// does. The acknowledgement wakes the thread the entry named.
#[test]
fn a_result_entry_is_acknowledged_at_its_first_counted_picture() {
    use render::FrameOutcome::{Invisible, NotBuilt, Retry, Shown};
    let (_dir, skin) = fixed_skin();
    for (skinned, first) in [
        (true, Retry),
        (true, NotBuilt),
        (true, Invisible),
        (true, Shown),
        (false, Retry),
        (false, Invisible),
    ] {
        let case = format!("skinned {skinned}, first {first:?}");
        let rig = Rig::new();
        let lifetime = installed(&rig, &skin);
        let entry = rig.link.enter_result_entry(Some(std::thread::current()));
        assert_eq!(rig.link.result_acknowledged(), 0, "{case}");
        rig.faults.outcomes.lock().unwrap().push_back(first);
        paced(&rig);
        let view = payload(1, skinned.then_some(&skin), 1_111, &[]);
        show(&rig.link, lifetime, 1, &view, &Overlay::default());
        assert!(
            wait_until(|| rig.gate.held.load(Ordering::Acquire)),
            "{case}"
        );
        assert_eq!(rig.link.result_acknowledged(), 0, "{case}: before a frame");
        rig.gate.step();
        let drew = rig.drawn();
        assert_eq!(drew.len(), 1, "{case}: {drew:?}");
        assert_eq!(skin_frame(&drew[0]).is_some(), skinned, "{case}: {drew:?}");
        let counted = matches!(first, Invisible | Shown);
        assert_eq!(
            rig.link.result_acknowledged(),
            if counted { entry } else { 0 },
            "{case}: the first frame"
        );
        rig.gate.step();
        assert_eq!(
            rig.link.result_acknowledged(),
            entry,
            "{case}: the next frame"
        );
        // The acknowledgement woke this thread: its token is waiting.
        let began = Instant::now();
        std::thread::park_timeout(Duration::from_secs(20));
        assert!(began.elapsed() < Duration::from_secs(10), "{case}: woken");
        rig.gate.release();
        rig.link.exit();
    }
}

/// V5's render half (design.md "State Model", system sounds, Result): the
/// built-in result page acknowledges an entry without a result lifetime
/// at its first frame; an entry whose lifetime began is acknowledged at
/// its first committed view, not at a page drawn before it, and until then
/// the acknowledgement stays the earlier entry's.
#[test]
fn an_end_page_acknowledges_only_an_entry_without_a_lifetime() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let page = || EndPage {
        lines: vec!["page".to_owned()],
        graphs: None,
        #[cfg(feature = "dev-tools")]
        id: None,
    };
    let first = rig.link.enter_result_entry(None);
    rig.link.show_end(page());
    assert!(rig.until(|d| matches!(d, Drawn::Page(_))));
    assert!(wait_until(|| rig.link.result_acknowledged() == first));
    rig.link.clear_end();
    let lifetime = installed(&rig, &skin);
    let second = rig.link.enter_result_entry(None);
    rig.clear();
    rig.link.show_end(page());
    assert!(wait_until(|| rig.drawn().len() >= 3), "{:?}", rig.drawn());
    assert!(rig.drawn().iter().all(|d| matches!(d, Drawn::Page(_))));
    assert_eq!(
        rig.link.result_acknowledged(),
        first,
        "a page of the lifetime"
    );
    show(
        &rig.link,
        lifetime,
        1,
        &payload(1, Some(&skin), 1_111, &[]),
        &Overlay::default(),
    );
    assert!(rig.until(|d| skin_frame(d).is_some()));
    assert!(wait_until(|| rig.link.result_acknowledged() == second));
    rig.link.exit();
}

/// With the transition probe on, an entry whose built-in page shows with
/// no result lifetime has its first three frames probed (`rf:*`), once:
/// its later frames are not, nor a page shown again with no entry entered
/// (the entry's first picture was shown), and the next such entry's
/// first three are.
#[test]
fn the_probe_marks_the_first_frames_of_each_built_in_entry() {
    use crate::dev::loop_probe::tests::{marks, turn_on};
    turn_on();
    let rig = Rig::new();
    let page = || EndPage {
        lines: vec!["page".to_owned()],
        graphs: None,
        #[cfg(feature = "dev-tools")]
        id: None,
    };
    let left = || -> Vec<i64> {
        marks()
            .iter()
            .filter(|m| m.1 == "rf:frame_left")
            .map(|m| m.2)
            .collect()
    };
    rig.link.enter_result_entry(None);
    rig.link.show_end(page());
    assert!(
        wait_until(|| left().len() == 3),
        "the entry's frames not probed: {:?}",
        left()
    );
    rig.clear();
    assert!(wait_until(|| rig.drawn().len() >= 3), "{:?}", rig.drawn());
    assert_eq!(left(), [2, 1, 0], "probed past its first frames");
    rig.link.clear_end();
    rig.clear();
    rig.link.show_end(page());
    assert!(wait_until(|| rig.drawn().len() >= 3), "{:?}", rig.drawn());
    assert_eq!(left(), [2, 1, 0], "a page of an entry already shown probed");
    rig.link.clear_end();
    rig.link.enter_result_entry(None);
    rig.link.show_end(page());
    assert!(
        wait_until(|| left().len() == 6),
        "the next entry's frames not probed: {:?}",
        left()
    );
    assert_eq!(left(), [2, 1, 0, 2, 1, 0]);
    rig.link.exit();
}

/// With the transition probe on, a built-in entry's first frame tried again
/// (not presented, so the entry not yet acknowledged) does not arm its
/// probed frames (`rf:*`) again: its frames are probed three times, from
/// the frame tried again on.
#[test]
fn the_probe_arms_a_built_in_entry_once_across_a_frame_tried_again() {
    use crate::dev::loop_probe::tests::{marks, turn_on};
    turn_on();
    let rig = Rig::new();
    let left = || -> Vec<i64> {
        marks()
            .iter()
            .filter(|m| m.1 == "rf:frame_left")
            .map(|m| m.2)
            .collect()
    };
    let entry = rig.link.enter_result_entry(None);
    rig.faults
        .outcomes
        .lock()
        .unwrap()
        .push_back(render::FrameOutcome::Retry);
    rig.link.show_end(EndPage {
        lines: vec!["page".to_owned()],
        graphs: None,
        #[cfg(feature = "dev-tools")]
        id: None,
    });
    assert!(wait_until(|| rig.link.result_acknowledged() == entry));
    assert!(wait_until(|| left().len() >= 3), "{:?}", left());
    rig.clear();
    assert!(wait_until(|| rig.drawn().len() >= 3), "{:?}", rig.drawn());
    assert_eq!(left(), [2, 1, 0], "armed again at the frame tried again");
    rig.link.exit();
}

/// V5's render half (design.md "State Model", system sounds, Result): a
/// frame acknowledges the entry it draws a picture of, not one entered
/// while it draws, the skin's view and a built-in one alike, frames
/// stepped one at a time. As a retry does, the first entry's lifetime ends
/// before the next entry's view comes, so the first entry's view never
/// acknowledges the next entry; the next entry's own view, a different
/// one, does. The acknowledgement is written once per entry, a later
/// counted frame of the same entry writing nothing.
#[test]
fn a_frame_acknowledges_the_entry_it_draws() {
    let (_dir, skin) = fixed_skin();
    for skinned in [true, false] {
        let rig = Rig::new();
        let shared = Arc::clone(&rig.link.shared);
        let entered = Arc::new(AtomicU64::new(0));
        let next = Arc::clone(&entered);
        // Inside the first frame's draw: the first entry's lifetime ends
        // (the slot empties, as the retry's leaving does) and the next
        // entry enters.
        *rig.faults.during_submit.lock().unwrap() = Some(Box::new(move || {
            shared.result_slot().enter(0);
            next.store(shared.result_entries.enter(None), Ordering::Release);
        }));
        let lifetime = installed(&rig, &skin);
        let first = rig.link.enter_result_entry(None);
        let entries = &rig.link.shared.result_entries;
        // Frames go one at a time: each step draws one.
        paced(&rig);
        let skin_of = skinned.then_some(&skin);
        let a = payload(1, skin_of, 1_111, &["first entry"]);
        show(&rig.link, lifetime, 1, &a, &Overlay::default());
        let case = format!("skinned {skinned}");
        assert!(
            wait_until(|| rig.gate.held.load(Ordering::Acquire)),
            "{case}"
        );
        rig.gate.step();
        assert_eq!(rig.drawn().len(), 1, "{case}: {:?}", rig.drawn());
        let second = entered.load(Ordering::Acquire);
        assert_eq!(second, first + 1, "{case}: entered while drawing");
        assert_eq!(
            rig.link.result_acknowledged(),
            first,
            "{case}: the entry drawn"
        );
        // The next entry's own lifetime and view: the frame held meanwhile
        // draws it, not the first entry's ended one, and acknowledges the
        // next entry.
        let next_lifetime = rig.link.begin_result();
        let b = payload(2, skin_of, 2_222, &["next entry"]);
        show(&rig.link, next_lifetime, 1, &b, &Overlay::default());
        rig.gate.step();
        let drawn = rig.drawn();
        assert_eq!(drawn.len(), 2, "{case}: {drawn:?}");
        assert_ne!(drawn[0], drawn[1], "{case}: the next entry's own view");
        assert_eq!(
            rig.link.result_acknowledged(),
            second,
            "{case}: the next entry's view"
        );
        for _ in 0..3 {
            rig.gate.step();
        }
        let drawn = rig.drawn();
        assert_eq!(drawn.len(), 5, "{case}: {drawn:?}");
        // No frame after the first draws the first entry's view, so none of
        // it acknowledged the next entry.
        assert!(
            drawn[1..].iter().all(|d| *d == drawn[1]),
            "{case}: only the next entry's view after the first frame: {drawn:?}"
        );
        assert_eq!(
            entries.publications.load(Ordering::Relaxed),
            2,
            "{case}: one write per entry"
        );
        rig.gate.release();
        rig.link.exit();
    }
}

/// V5's render half (design.md "State Model", system sounds, Result): the
/// render thread prints what the result shows once it has let the result's
/// status go, so a game thread reading the status is not held behind a
/// stalled print.
#[test]
fn the_result_shown_print_lets_the_status_go() {
    use crate::render_side::{SHOWN_PRINT_HELD, SHOWN_PRINT_HOLD};
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    SHOWN_PRINT_HELD.store(false, Ordering::Release);
    SHOWN_PRINT_HOLD.store(true, Ordering::Release);
    show(
        &rig.link,
        lifetime,
        1,
        &payload(1, Some(&skin), 1_111, &[]),
        &Overlay::default(),
    );
    let held = wait_until(|| SHOWN_PRINT_HELD.load(Ordering::Acquire));
    let free = rig.link.shared.result_status.try_lock().is_ok();
    SHOWN_PRINT_HOLD.store(false, Ordering::Release);
    assert!(held, "the print was reached");
    assert!(free, "the status was let go before the print");
    rig.link.exit();
}

/// V5's render half (design.md "State Model", system sounds, Result): the
/// first result frame lost with its device acknowledges nothing; the first
/// frame of the screen built again does.
#[test]
fn a_first_frame_lost_is_acknowledged_after_its_recovery() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let lifetime = installed(&rig, &skin);
    let entry = rig.link.enter_result_entry(None);
    rig.faults.lose.store(true, Ordering::Release);
    show(
        &rig.link,
        lifetime,
        1,
        &payload(1, Some(&skin), 1_111, &[]),
        &Overlay::default(),
    );
    assert!(wait_until(|| rig.link.failed()));
    assert_eq!(rig.link.result_acknowledged(), 0, "the lost frame");
    rig.link.prepare().unwrap();
    assert!(rig.until(|d| skin_frame(d).is_some()));
    assert!(wait_until(|| rig.link.result_acknowledged() == entry));
    rig.link.exit();
}

/// The pages ticket's answer, polled as the game thread does.
pub(crate) fn pages_answer(
    t: &mut crate::render_side::PendingPages,
) -> crate::render_side::PagesAnswer {
    let end = Instant::now() + Duration::from_secs(30);
    while Instant::now() < end {
        if let Some(a) = t.poll(Duration::from_millis(5)) {
            return a.unwrap();
        }
    }
    panic!("the pages ticket was never answered");
}

/// V16a, the publication first: the pages published before the owner's
/// withdrawal are the result's pages-only installation, and the
/// withdrawal reports them published, which is final.
#[test]
fn a_pages_ticket_published_first_is_reported_published() {
    let _held = HOLDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let mut t = rig.link.begin_result_pages(Arc::clone(skin.atlas_pages()));
    assert!(wait_until(
        || rig.link.result_status().installed == Some(skin.atlas_id())
    ));
    assert!(t.withdraw(), "published before the withdrawal");
    assert!(!t.withdraw(), "final");
    assert!(t.poll(Duration::ZERO).is_none(), "settled");
    rig.link.exit();
}

/// V16a, before the begin: a ticket withdrawn before the render thread
/// takes it releases, reserves and creates nothing; the earlier
/// installation stays installed.
#[test]
fn a_pages_ticket_withdrawn_before_its_begin_releases_nothing() {
    let _held = HOLDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_a_dir, a) = fixed_skin();
    let (_b_dir, b) = fixed_skin();
    let rig = Rig::new();
    let mut first = rig.link.begin_result_pages(Arc::clone(a.atlas_pages()));
    assert_eq!(
        pages_answer(&mut first),
        crate::render_side::PagesAnswer::Ready
    );
    // The render thread held in a selection of `a`'s pages (installed: it
    // keeps them), `b`'s ticket queued behind it and withdrawn.
    crate::dev::hold_result_uploads(true);
    let held = rig
        .link
        .upload_result(crate::result_skin::Upload::Pages(Arc::clone(
            a.atlas_pages(),
        )));
    assert!(wait_until(crate::dev::result_upload_holding));
    let mut late = rig.link.begin_result_pages(Arc::clone(b.atlas_pages()));
    assert!(!late.withdraw(), "nothing published");
    crate::dev::hold_result_uploads(false);
    assert_eq!(held.wait().unwrap(), Preparation::Ready);
    assert!(rig.link.fence(Duration::from_secs(10)));
    let status = rig.link.result_status();
    assert_eq!(status.installed, Some(a.atlas_id()), "{status:?}");
    assert!(!status.retained.contains(&b.atlas_id()), "{status:?}");
    rig.link.exit();
}

/// V16a, after the begin: the begin's release stands, nothing is
/// published, the ticket answers cancelled, and both arrays retire.
#[test]
fn a_pages_ticket_withdrawn_after_its_begin_keeps_the_release() {
    let _held = HOLDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_a_dir, a) = fixed_skin();
    let (_b_dir, b) = fixed_skin();
    let rig = Rig::new();
    let mut first = rig.link.begin_result_pages(Arc::clone(a.atlas_pages()));
    assert_eq!(
        pages_answer(&mut first),
        crate::render_side::PagesAnswer::Ready
    );
    // Held after the withdrawn mark was read: the begin then runs.
    crate::dev::hold_result_uploads(true);
    let mut t = rig.link.begin_result_pages(Arc::clone(b.atlas_pages()));
    assert!(wait_until(crate::dev::result_upload_holding));
    assert!(!t.withdraw(), "nothing published");
    crate::dev::hold_result_uploads(false);
    assert!(rig.link.fence(Duration::from_secs(10)));
    let status = rig.link.result_status();
    assert_eq!(status.installed, None, "{status:?}");
    assert!(
        wait_until(|| {
            let s = rig.link.result_status();
            !s.retained.contains(&a.atlas_id()) && !s.retained.contains(&b.atlas_id())
        }),
        "{:?}",
        rig.link.result_status()
    );
    rig.link.exit();
}

/// Recovery: the screen dropped with the upload in hand answers its
/// ticket lost, never published.
#[test]
fn a_pages_ticket_in_hand_when_the_screen_drops_is_lost() {
    let _held = HOLDS
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let chart = Arc::new(bms_model::ChartBuilder::new(120_000).build().unwrap());
    crate::dev::hold_result_uploads(true);
    let mut t = rig.link.begin_result_pages(Arc::clone(skin.atlas_pages()));
    assert!(wait_until(crate::dev::result_upload_holding));
    // Queued behind the begin, which the hold lets go meanwhile: a chart
    // load that fails on the device.
    rig.faults.fail_load.store(1, Ordering::Release);
    std::thread::scope(|s| {
        s.spawn(|| {
            std::thread::sleep(Duration::from_millis(100));
            crate::dev::hold_result_uploads(false);
        });
        // Built again after the failure, the load answers as it may.
        let _ = rig.link.load(Arc::clone(&chart), None);
    });
    assert_eq!(pages_answer(&mut t), crate::render_side::PagesAnswer::Lost);
    rig.link.exit();
}

/// §5.3: a pages begin taken while the attempt is staged takes no request
/// and supersedes nothing: the attempt answers ready and commits, and
/// the pages publish beside it.
#[test]
fn a_pages_begin_leaves_the_attempts_staging() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let chart = Arc::new(bms_model::ChartBuilder::new(120_000).build().unwrap());
    rig.link.load(Arc::clone(&chart), None).unwrap();
    let target = crate::render_common::offscreen(1_920, 1_080);
    let (_play_dir, play) = crate::render_json::bundle(&target);
    let (_staged_dir, staged) = crate::render_json::bundle(&target);
    assert_eq!(
        rig.link
            .select(ScreenKind::Play, Some(play))
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    rig.link.loading().show(super::one_line("loading"));
    let mut attempt = rig.link.begin_attempt(
        super::titled(&chart),
        super::super::AttemptSkin::Specialized(staged),
    );
    let mut t = rig.link.begin_result_pages(Arc::clone(skin.atlas_pages()));
    let answer = loop {
        if let Some(a) = attempt.wait_until(None, || false) {
            break a.unwrap();
        }
    };
    assert_eq!(answer.preparation, Preparation::Ready);
    assert_eq!(pages_answer(&mut t), crate::render_side::PagesAnswer::Ready);
    rig.link.commit_attempt().unwrap();
    assert!(rig.faults.commit_found.load(Ordering::Acquire));
    assert_eq!(rig.link.result_status().installed, Some(skin.atlas_id()));
    rig.link.end_loading().unwrap();
    rig.link.exit();
}

/// H1 (design.md "Load Pipeline", one heavy operation a frame interval):
/// play's staging and the result pages' array, asked back to back, are
/// each created at a boundary the allowance admits, play's first, in two
/// intervals: the pages' creation begins only after a refill or the
/// clock's pace from play's.
// Revert: the pages begun in their message's turn (`owe_pages` calling
// `begin_pages` at once) -> no pages creation at a boundary: "the pages
// created outside the allowance".
#[test]
fn play_and_the_pages_are_created_in_two_intervals_play_first() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let chart = Arc::new(bms_model::ChartBuilder::new(120_000).build().unwrap());
    rig.link.load(Arc::clone(&chart), None).unwrap();
    let target = crate::render_common::offscreen(1_920, 1_080);
    let (_play_dir, play) = crate::render_json::bundle(&target);
    let (_staged_dir, staged) = crate::render_json::bundle(&target);
    assert_eq!(
        rig.link
            .select(ScreenKind::Play, Some(play))
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    rig.link.loading().show(super::one_line("loading"));
    rig.link.record_heavy(64);
    let mut attempt = rig.link.begin_attempt(
        super::titled(&chart),
        super::super::AttemptSkin::Specialized(staged),
    );
    let mut t = rig.link.begin_result_pages(Arc::clone(skin.atlas_pages()));
    let answer = loop {
        if let Some(a) = attempt.wait_until(None, || false) {
            break a.unwrap();
        }
    };
    assert_eq!(answer.preparation, Preparation::Ready);
    assert_eq!(pages_answer(&mut t), crate::render_side::PagesAnswer::Ready);
    use crate::render_side::HeavyKind;
    let made: Vec<_> = rig
        .link
        .heavy_record()
        .into_iter()
        .filter(|r| matches!(r.kind, HeavyKind::PlayCreation | HeavyKind::PagesCreation))
        .collect();
    assert_eq!(
        made.iter().map(|r| r.kind).collect::<Vec<_>>(),
        [HeavyKind::PlayCreation, HeavyKind::PagesCreation],
        "the pages created outside the allowance"
    );
    let (play, pages) = (made[0], made[1]);
    assert!(
        pages.refills > play.refills || pages.start >= play.end + Duration::from_millis(16),
        "the pages created in play's interval: {play:?} {pages:?}"
    );
    rig.link.commit_attempt().unwrap();
    rig.link.end_loading().unwrap();
    rig.link.exit();
}

/// H1 (r2): a result bundle's staging whose atlas step the device refuses,
/// or whose device is lost while it steps, ends at that boundary with its
/// one end: the selection waiting for it is asked again in that pass and
/// answered, never left to the pending selection's 30 s bound.
// Revert: the end not reported (`out.staged` left `None` where a step
// retired the staging, or the loss only breaking the boundary's loop) ->
// "answered at its end" (the wait runs to its bound).
#[cfg(feature = "dev-tools")]
fn a_result_bundle_staging_ended_answers_the_selection(lose: bool) {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    if lose {
        rig.faults.lose_in_staging.store(true, Ordering::Release);
    } else {
        rig.faults.refuse_step.store(true, Ordering::Release);
    }
    let start = Instant::now();
    let answer = rig
        .link
        .upload_result(crate::result_skin::Upload::Bundle(Arc::clone(&skin)))
        .wait();
    assert!(
        start.elapsed() < Duration::from_secs(10),
        "answered at its end: {:?}",
        start.elapsed()
    );
    assert!(
        rig.faults.stepped_installs.load(Ordering::Acquire) >= 2,
        "asked again"
    );
    if !lose {
        assert!(!matches!(answer, Ok(Preparation::Ready)), "{answer:?}");
    }
}

#[cfg(feature = "dev-tools")]
#[test]
fn a_refused_result_bundle_step_answers_the_selection() {
    a_result_bundle_staging_ended_answers_the_selection(false);
}

#[cfg(feature = "dev-tools")]
#[test]
fn a_lost_device_under_a_result_bundle_staging_answers_the_selection() {
    a_result_bundle_staging_ended_answers_the_selection(true);
}

/// H3 (design.md "Load Pipeline", screen skins across the flow): a result
/// skin's bundle (a chart-only layout) is staged in the uploader's steps:
/// taken at a boundary the allowance admits, asked again until its last
/// step's completion, and answered ready then, installed.
// Revert: the result kind's `Select` staged by the blocking install ->
// "a blocking install".
#[test]
fn a_result_bundle_is_staged_in_the_uploaders_steps() {
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    rig.link.record_heavy(64);
    let answer = rig
        .link
        .upload_result(crate::result_skin::Upload::Bundle(Arc::clone(&skin)))
        .wait()
        .unwrap();
    assert_eq!(answer, Preparation::Ready);
    assert!(
        rig.faults.stepped_installs.load(Ordering::Acquire) >= 2,
        "a blocking install"
    );
    assert!(
        rig.link
            .heavy_record()
            .iter()
            .any(|r| r.kind == crate::render_side::HeavyKind::SelectCreation),
        "taken outside the allowance"
    );
    assert!(wait_until(
        || rig.link.result_status().installed == Some(skin.atlas_id())
    ));
    rig.link.exit();
}

/// The result frames' paths a sink saw, in order.
#[cfg(feature = "dev-tools")]
#[derive(Clone, Default)]
struct Paths(Arc<Mutex<Vec<Option<crate::measure::ResultPath>>>>);

#[cfg(feature = "dev-tools")]
impl FrameSink for Paths {
    fn frame(&mut self, _screen: &dyn Screen, drawn: &crate::measure::Drawn<'_>) {
        if drawn.timing.presented() {
            self.0.lock().unwrap().push(drawn.result.map(|r| r.path));
        }
    }
}

/// A view switch whose union fits only once a retired array is destroyed
/// (the installation before the lifetime, charged the whole shared limit,
/// retired at the first view's commit): its creation is not taken while
/// that entry stands, so it waits on no destruction in its call; the
/// shown view draws on every frame meanwhile, with no pending
/// presentation or built-in page; the entry is destroyed at a boundary of
/// its own, the creation comes at a later one, and no presenting pass
/// waits for the GPU (V19 (k), the switch; V9b (c)).
#[cfg(feature = "dev-tools")]
#[test]
fn a_switch_needing_a_union_waits_for_the_boundarys_destruction() {
    use crate::measure::ResultPath;
    use crate::render_side::HeavyKind;
    let (_x0d, x0) = fixed_skin();
    let (_ad, a) = fixed_skin();
    let (_bd, b) = fixed_skin();
    let paths = Paths::default();
    let rig = Rig::with_sink(Box::new(paths.clone()));
    rig.faults.step_creations.store(true, Ordering::Release);
    rig.faults
        .charge_result
        .store(render::SHARED_TEXELS, Ordering::Release);
    rig.link.hold_destruction(true);
    let lifetime = installed(&rig, &x0);
    show(
        &rig.link,
        lifetime,
        1,
        &payload(1, Some(&a), 1_000, &[]),
        &Overlay::default(),
    );
    assert!(
        wait_until(|| rig.link.result_status().shown == Some((1, true))),
        "the first view: {:?}",
        rig.link.result_status()
    );
    assert!(rig.until(|d| skin_frame(d).is_some()));
    rig.link.record_heavy(256);
    render::probe::count_gpu_waits(Some(false));
    let from = paths.0.lock().unwrap().len();
    // Frame by frame from here: the switch is taken, then four frames
    // follow its admission with the retired entry standing.
    rig.gate.hold();
    let taken = rig.link.messages_taken();
    show(
        &rig.link,
        lifetime,
        2,
        &payload(2, Some(&b), 2_000, &[]),
        &Overlay::default(),
    );
    let end = Instant::now() + Duration::from_secs(20);
    while rig.link.messages_taken() == taken {
        assert!(Instant::now() < end, "the switch never taken");
        rig.gate.step();
    }
    // The retired entry stands: no creation, the first view drawn on each
    // frame.
    for i in 1..=4 {
        let before = paths.0.lock().unwrap().len();
        rig.gate.step();
        let seen = paths.0.lock().unwrap().clone();
        assert!(
            seen.len() > before && seen[before..].iter().all(|p| *p == Some(ResultPath::Skin)),
            "frame {i} after the switch's admission not the shown view's: {:?}",
            &seen[before..]
        );
        assert_eq!(
            rig.link.result_status().shown,
            Some((1, true)),
            "the switch committed while its admission needed a destruction no boundary made"
        );
        assert!(
            rig.link
                .heavy_record()
                .iter()
                .all(|r| r.kind != HeavyKind::ResultCreation),
            "the switch's creation taken with no room: {:?}",
            rig.link.heavy_record()
        );
    }
    rig.gate.release();
    // The boundaries destroy it, then the creation follows.
    rig.link.hold_destruction(false);
    assert!(
        wait_until(|| rig.link.result_status().shown == Some((2, true))),
        "the switch: {:?}",
        rig.link.result_status()
    );
    let presenting: Vec<_> = render::probe::gpu_waits()
        .into_iter()
        .filter(|w| w.pass == render::probe::PASS_PRESENTING)
        .collect();
    render::probe::count_gpu_waits(None);
    let heavy = rig.link.heavy_record();
    let destroyed = heavy
        .iter()
        .position(|r| r.kind == HeavyKind::Destruction)
        .expect("the retired entry destroyed at a boundary");
    let created = heavy
        .iter()
        .position(|r| r.kind == HeavyKind::ResultCreation)
        .expect("the switch's creation at a boundary");
    assert!(
        destroyed < created && heavy[created].start >= heavy[destroyed].end,
        "the creation before the destruction, or in its boundary: {heavy:?}"
    );
    let seen = paths.0.lock().unwrap().clone();
    let seen = seen.get(from..).unwrap_or(&[]);
    assert!(
        seen.iter().all(|p| *p == Some(ResultPath::Skin)),
        "a frame not the shown view's while the switch staged: {seen:?}"
    );
    assert_eq!(presenting, Vec::new(), "a blocking wait presenting");
    rig.link.exit();
}

/// A swap-paced window minimized past the result's hold bound, the menu
/// font not in hand: the pending presentation's bitmap frames end
/// invisible, so each refills the frame interval's allowance as a shown
/// one does. A first view's creation spends it; that result is left
/// before its first picture and the next one's hold begins, whose first
/// view's creation waits for a refill that only bitmap frames give: it is
/// created while the window stays minimized (design.md "Load Pipeline",
/// the allowance; "Rendering and Skins", the result's hold). A newer
/// payload of one lifetime waits for its first picture, so the second
/// creation is the next lifetime's.
#[cfg(feature = "dev-tools")]
#[test]
fn a_minimized_bitmap_frame_refills_the_allowance_for_a_second_creation() {
    use crate::render_side::{Extent, HeavyKind};
    let (_x0d, x0) = fixed_skin();
    let (_ad, a) = fixed_skin();
    let (_bd, b) = fixed_skin();
    let rig = Rig::new();
    rig.faults.step_creations.store(true, Ordering::Release);
    let lifetime = installed(&rig, &x0);
    let extent = Arc::new(Extent::default());
    extent.publish(1_920, 1_080);
    rig.link.follow(&extent);
    // Paced as a swap chain, every grant given.
    rig.gate.permits.store(u32::MAX, Ordering::Release);
    rig.gate.pace.store(true, Ordering::Release);
    rig.faults.no_font.store(true, Ordering::Release);
    // A cleared page drawing as the outgoing screen, then the hold, past
    // its bound at once: the bitmap line.
    rig.link
        .send(crate::render_side::RenderMsg::Menu(Box::default()));
    rig.link.set_first_frame_hold(TimeUs(200_000));
    rig.link.set_hold_clock(Some(200_001));
    rig.link.hold_result(bms_model::TimeSource::now(&rig.clock));
    let bitmap = rig.link.bitmap_frames();
    assert!(
        wait_until(|| rig.link.bitmap_frames() > bitmap + 2),
        "no bitmap frame past the bound"
    );
    extent.publish(0, 0);
    let bitmap = rig.link.bitmap_frames();
    assert!(
        wait_until(|| rig.link.bitmap_frames() > bitmap + 2),
        "no bitmap frame minimized"
    );
    rig.link.record_heavy(64);
    rig.link.pause_result_staging(true);
    show(
        &rig.link,
        lifetime,
        1,
        &payload(1, Some(&a), 1_000, &[]),
        &Overlay::default(),
    );
    let creations = |rig: &Rig| {
        rig.link
            .heavy_record()
            .iter()
            .filter(|r| r.kind == HeavyKind::ResultCreation)
            .count()
    };
    assert!(
        wait_until(|| creations(&rig) >= 1),
        "the first view's creation"
    );
    let bitmap = rig.link.bitmap_frames();
    rig.link.end_result();
    rig.link.hold_result(bms_model::TimeSource::now(&rig.clock));
    let next = rig.link.begin_result();
    show(
        &rig.link,
        next,
        2,
        &payload(2, Some(&b), 2_000, &[]),
        &Overlay::default(),
    );
    let end = Instant::now() + Duration::from_secs(10);
    while creations(&rig) < 2 {
        assert!(
            Instant::now() < end,
            "the next result's creation never admitted, {} bitmap frames drawn minimized since: {:?}",
            rig.link.bitmap_frames() - bitmap,
            rig.link.heavy_record()
        );
        std::thread::sleep(Duration::from_millis(2));
    }
    assert!(rig.link.bitmap_frames() > bitmap, "no bitmap frame between");
    rig.link.pause_result_staging(false);
    rig.gate.release();
    rig.link.exit();
}

/// Waits until the render loop waits at `gate`, at most 20 s.
#[cfg(feature = "dev-tools")]
fn held_at(gate: &crate::render_side::TestGate, what: &str) {
    assert!(
        wait_until(|| gate.waiting()),
        "the render thread never held {what}"
    );
}

/// A swap-paced window drawing the loading state, the result's pages
/// owed: the pass's boundary takes the swap chain's grant, and a minimize
/// applies in the same pass, after that boundary. The grant is kept for
/// the restore, and the pages are created and written while the window
/// stays minimized, paced by the clock (design.md "Memory Budget", the
/// uploader's admission gate; "Resize").
#[cfg(feature = "dev-tools")]
#[test]
fn a_grant_kept_across_a_minimize_lets_the_pages_stage_while_minimized() {
    use crate::render_side::{Extent, HeavyKind, PagesAnswer};
    let (_dir, skin) = fixed_skin();
    let rig = Rig::new();
    let chart = Arc::new(bms_model::ChartBuilder::new(120_000).build().unwrap());
    rig.link.load(Arc::clone(&chart), None).unwrap();
    let extent = Arc::new(Extent::default());
    extent.publish(1_920, 1_080);
    rig.link.follow(&extent);
    // Paced as a swap chain, every frame granted.
    rig.gate.permits.store(u32::MAX, Ordering::Release);
    rig.gate.pace.store(true, Ordering::Release);
    rig.link.loading().show(super::one_line("loading"));
    let [after_boundary, _, after_draw] = &rig.link.shared.frame_gates;
    // The pages owed from the next pass on.
    after_boundary.hold(true);
    held_at(after_boundary, "after a boundary");
    rig.link.record_heavy(64);
    let mut t = rig.link.begin_result_pages(Arc::clone(skin.atlas_pages()));
    // This pass draws; the next one's boundary takes the pages and finds
    // the grant, then holds before its frame reads the window's size.
    after_draw.hold(true);
    after_boundary.hold(false);
    held_at(after_draw, "after its frame");
    after_boundary.hold(true);
    GRANT.store(true, Ordering::SeqCst);
    after_draw.hold(false);
    held_at(after_boundary, "after the next boundary");
    assert!(
        !GRANT.load(Ordering::SeqCst),
        "the boundary never looked at the grant"
    );
    // Minimized from this pass's frame on.
    extent.publish(0, 0);
    let minimized = Instant::now();
    after_boundary.hold(false);
    let end = Instant::now() + Duration::from_secs(10);
    let answer = loop {
        if let Some(a) = t.poll(Duration::from_millis(5)) {
            break a.unwrap();
        }
        assert!(
            Instant::now() < end,
            "minimized, the pages never staged: {:?}",
            rig.link.heavy_record()
        );
    };
    assert_eq!(answer, PagesAnswer::Ready);
    assert!(
        rig.link
            .heavy_record()
            .iter()
            .any(|r| r.kind == HeavyKind::PagesCreation && r.start >= minimized),
        "the pages not created while minimized: {:?}",
        rig.link.heavy_record()
    );
    // The pages' steps are the uploader's only ones here (none of a
    // staging's atlas).
    assert!(
        rig.link.step_records().iter().any(|r| r.at >= minimized),
        "no step of the pages written while minimized: {:?}",
        rig.link.step_records()
    );
    extent.publish(1_920, 1_080);
    rig.gate.release();
    rig.link.end_loading().unwrap();
    rig.link.exit();
}

/// A view switch whose union fits only once the selection's chart-image
/// pool yields optional layers (a reduction), while the smaller pool a
/// regrowth replaced still stands retired beside the live one: the
/// reduction retires the live pool, one retired pool at a time, so the old
/// one must be destroyed first. The switch's creation is not taken while
/// it stands, and the shown view draws meanwhile; a boundary destroys it,
/// then the creation retires the live pool on its own allowance. No
/// destruction runs inside the creation's call, and no presenting pass
/// waits for the GPU (V19 (k), the switch; V9b (c)).
#[cfg(feature = "dev-tools")]
#[test]
fn a_switch_reducing_the_pool_waits_for_the_retired_pools_destruction() {
    let (_ad, a) = fixed_skin();
    let (_bd, b) = fixed_skin();
    switch_reducing_the_pool(&a, &b);
}

/// [`a_switch_reducing_the_pool_waits_for_the_retired_pools_destruction`]
/// for a rebind, a layout on the installed array that creates none: it is
/// taken at the first pass its staging needs no destruction, never refused
/// for want of room, and draws once the boundary destroyed the retired
/// pool.
#[cfg(feature = "dev-tools")]
#[test]
fn a_rebind_reducing_the_pool_waits_for_the_retired_pools_destruction() {
    let (_ad, skin) = fixed_skin();
    let [a, b] = super::result_chart::pair(&skin);
    switch_reducing_the_pool(&a, &b);
}

/// The switch from `a` to `b` of the two tests above; a creation's heavy
/// record checked when `b` creates an array.
#[cfg(feature = "dev-tools")]
fn switch_reducing_the_pool(a: &Arc<render::SkinBundle>, b: &Arc<render::SkinBundle>) {
    use crate::measure::ResultPath;
    use crate::render_side::HeavyKind;
    type Setup = (Option<u32>, Option<u32>, render::Admission, usize);
    type After = (u64, Vec<render::DestroyRecord>);
    let creates = a.atlas_id() != b.atlas_id();
    let (_x0d, x0) = fixed_skin();
    let sel_dir = crate::selection_skin::chart_fixture();
    let limits = crate::render_common::offscreen(1_920, 1_080).atlas_limits();
    let (sel, _script) =
        crate::selection_skin::fixture_scripted(limits, sel_dir.path(), "chart.luaskin");
    let paths = Paths::default();
    let rig = Rig::with_sink(Box::new(paths.clone()));
    rig.faults.step_creations.store(true, Ordering::Release);
    let lifetime = installed(&rig, &x0);
    // The selection's pool made at its firm layer alone: the device
    // refuses its whole array once.
    rig.faults.refuse_pools.store(1, Ordering::Release);
    assert_eq!(
        rig.link
            .select(ScreenKind::Selection, Some(Arc::clone(&sel)))
            .wait()
            .unwrap(),
        Preparation::Ready
    );
    rig.link.record_heavy(256);
    show(
        &rig.link,
        lifetime,
        1,
        &payload(1, Some(a), 1_000, &[]),
        &Overlay::default(),
    );
    assert!(
        wait_until(|| rig.link.result_status().shown == Some((1, true))),
        "the first view: {:?}",
        rig.link.result_status()
    );
    // The installation before the lifetime, retired at the first view's
    // commit, destroyed at a boundary before the pool's pair is set up.
    assert!(wait_until(|| rig
        .link
        .heavy_record()
        .iter()
        .any(|r| r.kind == HeavyKind::Destruction)));
    rig.link.hold_destruction(true);
    // The pool regrown: the live pool beside the firm-layer one it
    // replaced, retired and left standing; b charged so it fits once the
    // live pool keeps one layer fewer.
    let setup: Arc<Mutex<Option<Setup>>> = Arc::default();
    let s2 = Arc::clone(&setup);
    let b2 = Arc::clone(b);
    rig.faults
        .on_renderer
        .lock()
        .unwrap()
        .push(Box::new(move |r| {
            let before = r.pool().map(|p| p.layers);
            let grown = r.regrow_pool().map(|p| p.layers);
            let adm = r.admission();
            let kept = u64::from(grown.unwrap_or(0).saturating_sub(2));
            let others = adm.committed_total() - adm.charge(ScreenKind::Result);
            r.state_candidate_charge(
                &b2,
                render::SHARED_TEXELS - others - adm.filled - kept * adm.firm,
            );
            *s2.lock().unwrap() = Some((before, grown, adm, r.destructions_recorded()));
        }));
    assert!(wait_until(|| setup.lock().unwrap().is_some()));
    let (before, grown, adm, recorded) = setup.lock().unwrap().take().unwrap();
    assert_eq!(before, Some(1), "the pool at its firm layer before");
    let grown = grown.expect("a live pool");
    assert!(grown >= 2, "the regrowth: {grown} layers");
    assert!(
        adm.retired_pool > 0,
        "no retired pool beside the live one: {adm:?}"
    );
    assert_eq!(adm.filled, 0, "an entry still charged: {adm:?}");
    rig.link.record_heavy(256);
    render::probe::count_gpu_waits(Some(false));
    let from = paths.0.lock().unwrap().len();
    show(
        &rig.link,
        lifetime,
        2,
        &payload(2, Some(b), 2_000, &[]),
        &Overlay::default(),
    );
    // The retired pool stands: no creation, the first view drawn.
    std::thread::sleep(Duration::from_millis(300));
    assert_eq!(
        rig.link.result_status().shown,
        Some((1, true)),
        "the switch committed while the retired pool stood"
    );
    assert!(
        rig.link
            .heavy_record()
            .iter()
            .all(|r| r.kind != HeavyKind::ResultCreation),
        "the switch's creation taken with the retired pool standing: {:?}",
        rig.link.heavy_record()
    );
    let waiting = paths.0.lock().unwrap().len();
    assert!(waiting > from + 3, "frames stopped while the switch waited");
    // A boundary destroys it, then the creation follows.
    rig.link.hold_destruction(false);
    assert!(
        wait_until(|| rig.link.result_status().shown == Some((2, true))),
        "the switch: {:?}",
        rig.link.result_status()
    );
    let presenting: Vec<_> = render::probe::gpu_waits()
        .into_iter()
        .filter(|w| w.pass == render::probe::PASS_PRESENTING)
        .collect();
    render::probe::count_gpu_waits(None);
    // What the creation did to the pool, and every destruction since the
    // setup, read on the render thread.
    let after: Arc<Mutex<Option<After>>> = Arc::default();
    let a2 = Arc::clone(&after);
    rig.faults
        .on_renderer
        .lock()
        .unwrap()
        .push(Box::new(move |r| {
            let adm = r.admission();
            let all: Vec<_> = r.destructions().copied().collect();
            let n = (r.destructions_recorded() - recorded).min(all.len());
            *a2.lock().unwrap() = Some((
                adm.firm + adm.pool,
                all.get(all.len() - n..).unwrap_or(&[]).to_vec(),
            ));
        }));
    assert!(wait_until(|| after.lock().unwrap().is_some()));
    let (pooled, destroyed) = after.lock().unwrap().take().unwrap();
    assert!(
        pooled < u64::from(grown) * adm.firm,
        "the switch's creation did not reduce the pool: {pooled} texels of {grown} layers"
    );
    assert!(
        destroyed
            .iter()
            .any(|d| d.class == render::Class::RetiredPool),
        "the retired pool never destroyed: {destroyed:?}"
    );
    assert!(
        destroyed.iter().all(|d| !d.in_call),
        "a destruction inside a creation's call: {destroyed:?}"
    );
    let heavy = rig.link.heavy_record();
    let destroyed_at = heavy
        .iter()
        .position(|r| r.kind == HeavyKind::Destruction)
        .expect("the retired pool destroyed at a boundary");
    if creates {
        let created = heavy
            .iter()
            .position(|r| r.kind == HeavyKind::ResultCreation)
            .expect("the switch's creation at a boundary");
        assert!(
            destroyed_at < created && heavy[created].start >= heavy[destroyed_at].end,
            "the creation before the destruction, or in its boundary: {heavy:?}"
        );
    }
    let seen = paths.0.lock().unwrap().clone();
    let seen = seen.get(from..).unwrap_or(&[]);
    assert!(
        seen.iter().all(|p| *p == Some(ResultPath::Skin)),
        "a frame not the shown view's while the switch staged: {seen:?}"
    );
    assert_eq!(presenting, Vec::new(), "a blocking wait presenting");
    rig.link.exit();
}

/// A result frame's path and the view it presented.
#[cfg(feature = "dev-tools")]
type Seen = (crate::measure::ResultPath, Option<u64>);

/// The result frames' paths and the view each presented, a sink saw, in
/// order.
#[cfg(feature = "dev-tools")]
#[derive(Clone, Default)]
struct Views(Arc<Mutex<Vec<Seen>>>);

#[cfg(feature = "dev-tools")]
impl FrameSink for Views {
    fn frame(&mut self, _screen: &dyn Screen, drawn: &crate::measure::Drawn<'_>) {
        if drawn.timing.presented()
            && let Some(r) = drawn.result
        {
            self.0.lock().unwrap().push((r.path, r.view));
        }
    }
}

/// V19 (i) (design.md "beatoraja Lua Skins", the result skin): a second
/// view payload published while the first view's staging is held commits
/// after the first: the first committed view is the first payload's, the
/// second stages once the first picture is acknowledged, and every frame
/// from the first picture to the second's commit is the first view's.
/// With `replaced`, the installation before the lifetime (X0) is charged
/// the whole shared limit, so the second fits only once X0's entry, filled
/// at the first view's commit, is destroyed: that destruction comes after
/// the first view's creation, and the second's creation after it.
#[cfg(feature = "dev-tools")]
fn a_second_payload_waits_for_the_first_picture(replaced: bool) {
    use crate::measure::ResultPath;
    use crate::render_side::HeavyKind;
    let (_x0d, x0) = fixed_skin();
    let (_ad, a) = fixed_skin();
    let (_bd, b) = fixed_skin();
    let views = Views::default();
    let rig = Rig::with_sink(Box::new(views.clone()));
    if replaced {
        rig.faults.step_creations.store(true, Ordering::Release);
        rig.faults
            .charge_result
            .store(render::SHARED_TEXELS, Ordering::Release);
    }
    let lifetime = installed(&rig, &x0);
    rig.link.record_heavy(256);
    rig.link.pause_result_staging(true);
    show(
        &rig.link,
        lifetime,
        1,
        &payload(1, Some(&a), 1_000, &[]),
        &Overlay::default(),
    );
    assert!(
        wait_until(|| views
            .0
            .lock()
            .unwrap()
            .iter()
            .any(|(p, _)| *p == ResultPath::Pending)),
        "the first view's staging never held"
    );
    let taken = rig.link.messages_taken();
    show(
        &rig.link,
        lifetime,
        2,
        &payload(2, Some(&b), 2_000, &[]),
        &Overlay::default(),
    );
    assert!(wait_until(|| rig.link.messages_taken() > taken));
    let pending = views.0.lock().unwrap().len();
    assert!(wait_until(|| views.0.lock().unwrap().len() > pending + 3));
    rig.link.pause_result_staging(false);
    // The status is published before the frame reaches the sink: the
    // second's frame is waited for there.
    assert!(
        wait_until(|| views.0.lock().unwrap().iter().any(|(_, v)| *v == Some(2))),
        "the second never committed: {:?}",
        rig.link.result_status()
    );
    assert_eq!(rig.link.result_status().shown, Some((2, true)));
    let seen = views.0.lock().unwrap().clone();
    let first = seen
        .iter()
        .position(|(p, _)| *p == ResultPath::Skin)
        .unwrap();
    assert_eq!(
        seen[first].1,
        Some(1),
        "the first committed view not the first payload's: {seen:?}"
    );
    let second = seen.iter().position(|(_, v)| *v == Some(2)).unwrap();
    assert!(
        seen[first..second]
            .iter()
            .all(|f| *f == (ResultPath::Skin, Some(1))),
        "a frame not the first view's before the second committed: {seen:?}"
    );
    if replaced {
        let heavy = rig.link.heavy_record();
        let created: Vec<usize> = heavy
            .iter()
            .enumerate()
            .filter(|(_, r)| r.kind == HeavyKind::ResultCreation)
            .map(|(i, _)| i)
            .collect();
        let destroyed = heavy
            .iter()
            .position(|r| r.kind == HeavyKind::Destruction)
            .expect("X0's entry destroyed at a boundary");
        let [a_made, b_made] = created[..] else {
            panic!("two result creations: {heavy:?}");
        };
        assert!(
            a_made < destroyed && destroyed < b_made && heavy[b_made].start >= heavy[destroyed].end,
            "the second created before X0's destruction: {heavy:?}"
        );
    }
    rig.link.exit();
}

#[cfg(feature = "dev-tools")]
#[test]
fn a_second_payload_before_the_first_picture_waits_for_it() {
    a_second_payload_waits_for_the_first_picture(false);
}

#[cfg(feature = "dev-tools")]
#[test]
fn a_second_payload_needing_the_replaced_installations_room_waits_for_it() {
    a_second_payload_waits_for_the_first_picture(true);
}

/// The lifetime's first view whose commit is refused falls back to its
/// built-in page (design.md "beatoraja Lua Skins", the result skin): no
/// built-in end page with nothing presented, the view's own page next, and
/// it is the lifetime's first committed view.
#[cfg(feature = "dev-tools")]
#[test]
fn a_first_view_whose_commit_is_refused_falls_back_to_its_page() {
    use crate::measure::ResultPath;
    let (_x0d, x0) = fixed_skin();
    let (_ad, a) = fixed_skin();
    let views = Views::default();
    let rig = Rig::with_sink(Box::new(views.clone()));
    let lifetime = installed(&rig, &x0);
    rig.faults.refuse_commit.store(true, Ordering::Release);
    show(
        &rig.link,
        lifetime,
        1,
        &payload(1, Some(&a), 1_000, &[]),
        &Overlay::default(),
    );
    // The status is published before the frame reaches the sink: the
    // page's frame is waited for there.
    assert!(
        wait_until(|| views
            .0
            .lock()
            .unwrap()
            .contains(&(ResultPath::BuiltIn, Some(1)))),
        "the first view not committed as its page: {:?}",
        rig.link.result_status()
    );
    assert_eq!(rig.link.result_status().shown, Some((1, false)));
    assert!(
        !rig.faults.refuse_commit.load(Ordering::Acquire),
        "never refused"
    );
    let seen = views.0.lock().unwrap().clone();
    let page = seen
        .iter()
        .position(|f| *f == (ResultPath::BuiltIn, Some(1)))
        .expect("the view's page");
    assert!(
        seen[..page].iter().all(|(p, _)| *p == ResultPath::Pending),
        "a frame other than pending before the view's page: {seen:?}"
    );
    rig.link.exit();
}
