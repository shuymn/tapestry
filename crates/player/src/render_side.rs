//! The render thread (design.md "Overall Architecture", "Rendering and Skins"):
//! it owns the screen, readies it for each play on request and says when it is
//! ready, draws each play from the logic step's effects and play view at the
//! play's chart time, and draws the end screen between plays and the select
//! screen ([`crate::menu_screen`]) on song selection. The end screen's page
//! goes through a slot a newer page replaces ([`RenderLink::show_end`]), so
//! however far the render thread falls behind, one page waits for it and
//! it holds one, the graphs of each shared with the result screen's cache;
//! leaving the end screen empties the slot ([`RenderLink::clear_end`]), and
//! a signal whose page went with it draws nothing.
//! With selection the
//! chart is not known at the start: each chosen chart is loaded on request
//! ([`RenderMsg::Load`]) and stays until the next. From a chart's choice
//! to its READY state the play screen is drawn in its loading state, through
//! the chart's load and the play's preparation, whose answers come between
//! its frames: the skin's loading frame ([`Screen::loading`]) with the
//! load's lines over it. The lines go through a slot as the end screen's
//! page does, where newer lines replace those not yet taken and at most
//! one signal waits in the channel ([`LoadingSink`]), so a slow render
//! thread never falls behind on the load's progress. With them goes the
//! loading lifetime's whole state, accumulated on the producers' side
//! ([`crate::loading_state`]) and numbered: the slot and the render
//! thread take only a newer one, and each loading frame is built from the
//! newest on the loading clock, from the origin stamped when the lifetime
//! began ([`RenderLink::begin_loading`]). A window target
//! paces frames on the swap chain's waitable object; an offscreen one at about
//! 60 Hz at most, each frame waiting for the GPU to finish the one before
//! or skipped while it has not.
//!
//! The window's physical size reaches the render thread through a slot
//! that holds the newest only ([`Extent`]): the window thread publishes
//! every resize and DPI change there and wakes the render thread with at
//! most one message waiting, and the render thread applies the newest
//! between frames, again to a screen built anew. While the window is
//! minimized (a zero side) frames go on being built, unpaced and
//! unpresented, and messages and handshakes are answered as ever; the
//! newest nonzero size is applied when it comes back.
//!
//! A frame that fails (the surface was lost) drops the screen and tells
//! the game thread, which ends the play; the next play's preparation builds
//! the screen again, and if that fails the run ends with the error. A frame
//! the skin refuses ([`Screen::build`]) also ends the play, but keeps the
//! screen and its resources: nothing is drawn or built again for that play,
//! and the next play's preparation readies the same screen afresh.
//!
//! An attempt's preparation is a request the render thread answers
//! between frames, numbered by the link: a screen may take several polls
//! to answer ([`Screen::prepare_attempt`]), and loading frames go on
//! meanwhile. What a ready preparation staged is installed only by the
//! commit of that request ([`RenderLink::commit_attempt`]), after the
//! game's last checks; a cancelled, refused, timed out or superseded
//! request is discarded, so an answer that comes late never installs.
//! Each screen kind's declarative installation committed is kept apart
//! from the screen, its loading presentation with it: a screen built
//! again after a failure restores exactly them from memory
//! ([`Screen::restore`]), play first, `when` starts included, never
//! reading their files again; a non-play kind whose restore is refused on
//! a healthy device is dropped, and its screen draws its built-in
//! presentation.
//!
//! A declarative skin is selected by an acknowledged transaction
//! ([`RenderLink::select`]): its chart-independent installation is staged
//! and committed on the render thread under the same request numbers as
//! the attempts' preparations, so the newer of the two always wins, and a
//! selection the game thread withdrew (cancelled, timed out, abandoned)
//! installs nothing however late it completes. A loading frame the skin
//! refuses draws nothing, keeps the screen and is not built again in its
//! loading lifetime; the end of loading reports the refusal, so recording
//! does not open ([`RenderLink::end_loading`]).

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use bms_model::{Chart, Speed, TimeSource, TimeUs};
use eval::{JudgeEvent, Section};
use game::{Chain, Preparation, PreparationRefusal, RenderSide, ResultGraphs};
use input::QpcClock;
use render::measure::{FrameStatistics, Presentation};
use render::{
    AttemptInput, CommitError, FrameInput, FrameTiming, Installation, LoadingFrame, PreparedToken,
    RenderError, SkinBundle,
};
use skin_model::{AtlasId, BuildError, PackedPages, PlayContext, ScreenKind};

use crate::attempt_context::AttemptContext;
use crate::loading_state::{LifetimeState, LoadInput, LoadingUpdate};
use crate::loading_text::LoadingText;
#[cfg(test)]
use crate::loading_text::Stage;
use crate::measure::{Drawn, Figures, Measure, PlayMeasure};
use crate::menu_screen::{MenuPage, SPLASH};
use crate::result_present::{
    Overlay, ResultBarrier, ResultPresentation, ResultReport, ResultSlot, ViewKey, ViewPayload,
};
use crate::screens::{Quiet, ScriptNotes, ScriptSlot, ScriptStop, Scripts, ViewCommit};
use crate::selection_skin::{SelectionDrawn, SelectionLogic, SelectionSlot, SelectionUpdate};
use crate::tuning::LiveTuning;
use crate::ui_text::{Label, Message};

/// What draws: the renderer, or a double in tests.
pub trait Screen: Send {
    /// Prepares `chart` and resets the play state; with a practice round's
    /// `section`, only its targets are drawn.
    ///
    /// # Errors
    ///
    /// Why it cannot draw the chart.
    fn load(&mut self, chart: &Chart, section: Option<&Section>) -> Result<(), RenderError>;
    /// Prepares the attempt `a` (its chart, its context and its labels,
    /// borrowed for this call only) for preparation request `request`,
    /// staging `skin`'s installation when one is chosen, else readying the
    /// skin in use; nothing it stages draws before its
    /// [`commit`](Self::commit). `None` while the answer is not ready: it
    /// is asked again between frames with the same attempt, until it
    /// answers or the request is discarded. A skin that cannot draw the
    /// attempt refuses it, and the screen stays usable for the menus. The
    /// default prepares with [`load`](Self::load), the section's targets
    /// only for a section, and takes its skin's refusal as the attempt's;
    /// it refuses a declarative skin, which it cannot draw.
    ///
    /// # Errors
    ///
    /// Why the screen or its device cannot draw at all: the screen must be
    /// built again.
    fn prepare_attempt(
        &mut self,
        a: &AttemptInput<'_>,
        skin: Option<&Arc<SkinBundle>>,
        request: u64,
    ) -> Option<Result<Preparation, RenderError>> {
        let _ = request;
        if skin.is_some() {
            return Some(Ok(refusal("this screen draws no declarative skin")));
        }
        Some(match self.load(a.chart, a.section()) {
            Ok(()) => Ok(Preparation::Ready),
            Err(RenderError::Skin(e)) => Ok(refusal(e.reason)),
            Err(e) => Err(e),
        })
    }
    /// Identifies the device this screen draws with: a screen built again
    /// has another. The default is 0.
    fn device(&self) -> u64 {
        0
    }
    /// Installs what the preparation of `token` staged, whole, between
    /// frames; with nothing staged (the skin in use was readied in place)
    /// only the token's device is checked. The default installs nothing.
    ///
    /// # Errors
    ///
    /// Why it was not installed: nothing changes.
    fn commit(&mut self, token: PreparedToken) -> Result<(), CommitError> {
        if token.device == self.device() {
            Ok(())
        } else {
            Err(CommitError::Device)
        }
    }
    /// Drops what a preparation staged and was not committed. The default
    /// has nothing to drop.
    fn discard(&mut self) {}
    /// Publishes the chart's images into `target`
    /// ([`render::Renderer::publish_chart_images`]). The default draws no
    /// declarative skin, so it has no chart layer.
    ///
    /// # Errors
    ///
    /// As the renderer's; [`RenderError::NoChartLayer`] by default.
    fn publish_chart_images(
        &mut self,
        target: render::ChartTarget,
        images: [Option<&skin_model::chart_image::Fitted>; 2],
    ) -> Result<skin_model::chart_image::ChartTable, RenderError> {
        let _ = (target, images);
        Err(RenderError::NoChartLayer)
    }
    /// Publishes the chart's images absent into `target`
    /// ([`render::Renderer::publish_chart_absent`]).
    ///
    /// # Errors
    ///
    /// As the renderer's; [`RenderError::NoChartLayer`] by default.
    fn publish_chart_absent(
        &mut self,
        target: render::ChartTarget,
    ) -> Result<skin_model::chart_image::ChartTable, RenderError> {
        let _ = target;
        Err(RenderError::NoChartLayer)
    }
    /// Makes the chart's images unavailable: their generation or target
    /// is no longer current ([`render::Renderer::invalidate_chart_images`]).
    /// The default has none.
    fn invalidate_chart_images(&mut self) {}
    /// The live key of the committed play installation's chart layer
    /// ([`render::Renderer::chart_key`]); `None` by default, which has no
    /// chart layer.
    fn chart_key(&self) -> Option<skin_model::chart_image::ChartKey> {
        None
    }
    /// Stages `skin`'s chart-independent installation of `kind` for
    /// request `request` (its atlas, clips, buffers and loading
    /// presentation, no attempt): it installs nothing until its
    /// [`commit`](Self::commit), and every installation in use stays
    /// meanwhile. A skin the screen cannot install, past the texels the
    /// screens share included, refuses it, every installation in use
    /// kept. The default refuses: it draws no declarative skin.
    ///
    /// # Errors
    ///
    /// Why the screen or its device cannot draw at all: the screen must be
    /// built again.
    fn install(
        &mut self,
        kind: ScreenKind,
        skin: &Arc<SkinBundle>,
        request: u64,
    ) -> Result<Preparation, RenderError> {
        let _ = (kind, skin, request);
        Ok(refusal("this screen draws no declarative skin"))
    }
    /// `kind`'s committed declarative installation, its loading
    /// presentation with it, kept for a screen built again; `None` for
    /// the simple skin. The default has none.
    fn installation(&self, kind: ScreenKind) -> Option<Installation> {
        let _ = kind;
        None
    }
    /// Installs the committed `installation` of `kind` on a screen built
    /// again after a failure, from memory, with no attempt prepared: the
    /// very loading presentation, `when` starts included. The default
    /// installs nothing.
    ///
    /// # Errors
    ///
    /// Why it could not be installed: a refusal on a healthy device
    /// ([`RenderError::is_refusal`]) or the screen's failure.
    fn restore(
        &mut self,
        kind: ScreenKind,
        installation: &Installation,
    ) -> Result<(), RenderError> {
        let _ = (kind, installation);
        Ok(())
    }
    /// Drops `kind`'s committed installation, so its screen draws its
    /// built-in presentation; play's is never removed. The default has
    /// nothing to drop.
    fn remove(&mut self, kind: ScreenKind) {
        let _ = kind;
    }
    /// Stages the result kind's pages-only installation of `pages` for
    /// request `request` ([`render::Renderer::install_pages`]): nothing
    /// is uploaded when they are installed. Other pages release the
    /// result's installation first, so a refusal (or a withdrawal before
    /// the commit) leaves the result kind with none; every other kind's
    /// stays. The default refuses.
    ///
    /// # Errors
    ///
    /// As [`install`](Self::install).
    fn install_pages(
        &mut self,
        kind: ScreenKind,
        pages: &Arc<PackedPages>,
        request: u64,
    ) -> Result<Preparation, RenderError> {
        let _ = (kind, pages, request);
        Ok(refusal("this screen draws no declarative skin"))
    }
    /// Restores the result kind's pages-only installation of `pages` on a
    /// screen built again. The default restores nothing.
    ///
    /// # Errors
    ///
    /// As [`restore`](Self::restore).
    fn restore_pages(
        &mut self,
        kind: ScreenKind,
        pages: &Arc<PackedPages>,
    ) -> Result<(), RenderError> {
        let _ = (kind, pages);
        Ok(())
    }
    /// Disables the result kind for this screen's life
    /// ([`render::Renderer::disable_result`]). Nothing by default.
    fn disable_result(&mut self) {}
    /// Drops what the last commit replaced, outside the commit point:
    /// the array it released, if any. None by default.
    fn drain_retired(&mut self) -> Option<AtlasId> {
        None
    }
    /// The result kind's pages-only installation's pages, if it has one
    /// ([`render::Renderer::installed_pages`]). None by default.
    fn installed_pages(&self, kind: ScreenKind) -> Option<Arc<PackedPages>> {
        let _ = kind;
        None
    }
    /// The atlases the screen holds for `kind`: committed, staged and
    /// retired. None by default.
    fn retained_atlases(&self, kind: ScreenKind) -> [Option<AtlasId>; 3] {
        let _ = kind;
        [None; 3]
    }
    /// Prepares a result view on `state`, a state of `skin`'s: the view
    /// and labels taken ([`skin_model::Skin::update_result`]) and its
    /// lines staged for the result bundle staged or committed, not drawn
    /// until [`install_result_text`](Self::install_result_text). Allocates.
    /// The default refuses.
    ///
    /// # Errors
    ///
    /// The outer error is the device's; the inner one the skin's refusal
    /// on a healthy device.
    fn prepare_result(
        &mut self,
        skin: &Arc<SkinBundle>,
        state: &mut skin_model::ResultScreen,
        view: &skin_model::ResultView,
        labels: &skin_model::ResultLabels,
    ) -> Result<Result<(), String>, RenderError> {
        let _ = (skin, state, view, labels);
        Ok(Err("this screen draws no declarative skin".to_owned()))
    }
    /// Installs the result's staged lines, at the commit point: whether
    /// they were. Allocates nothing. Never by default.
    fn install_result_text(&mut self) -> bool {
        false
    }
    /// Lays out `lines` as the strip's text in slot `which` (1 or 2) for
    /// a later [`use_result_strip`](Self::use_result_strip). Allocates.
    /// Nothing by default.
    ///
    /// # Errors
    ///
    /// As [`menu`](Self::menu).
    fn prepare_result_strip(&mut self, which: usize, lines: &[&str]) -> Result<(), RenderError> {
        let _ = (which, lines);
        Ok(())
    }
    /// The strip's text of slot `which` becomes the one drawn, at the
    /// commit point. Allocates nothing. Nothing by default.
    fn use_result_strip(&mut self, which: usize) {
        let _ = which;
    }
    /// Builds a result frame of the result layout committed: `state` at
    /// `frame`, then the strip's bands over it, with no call into wgpu.
    /// Allocates nothing. The default refuses.
    ///
    /// # Errors
    ///
    /// The skin's refusal of the frame: nothing of it is staged.
    fn build_result(
        &mut self,
        state: &mut skin_model::ResultScreen,
        frame: &skin_model::ResultFrame<'_>,
    ) -> Result<(), BuildError> {
        let _ = (state, frame);
        Err(BuildError::Kind)
    }
    /// Draws the result frame [`build_result`](Self::build_result) built,
    /// with the strip's lines over it, as [`draw`](Self::draw).
    ///
    /// # Errors
    ///
    /// As [`draw`](Self::draw).
    fn submit_result(&mut self) -> Result<FrameTiming, RenderError> {
        self.draw()
    }
    /// The committed decision installation's label rows at the target
    /// ([`render::Renderer::decision_rows`]); `None` without one, the
    /// default.
    fn decision_rows(&self) -> Option<Result<render::RowPlan, RenderError>> {
        None
    }
    /// One frame boundary's writes of `store`'s filling slot
    /// ([`render::Renderer::write_decision_labels`]). Nothing by default.
    fn write_decision_labels(&mut self, store: &mut render::LabelStore) -> render::FillStep {
        let _ = store;
        render::FillStep::Idle
    }
    /// Settles `store` at a frame boundary
    /// ([`render::Renderer::settle_decision_labels`]). Nothing by default.
    ///
    /// # Errors
    ///
    /// As the renderer's: the device's failure, or a refusal of the kind.
    fn settle_decision_labels(
        &mut self,
        store: &mut render::LabelStore,
    ) -> Result<render::Settled, RenderError> {
        let _ = store;
        Ok(render::Settled::Pending)
    }
    /// Rebuilds pinned slot `slot`'s content from `staged` on a screen
    /// built again ([`render::Renderer::restore_decision_labels`]). The
    /// default has no label region and refuses.
    ///
    /// # Errors
    ///
    /// As the renderer's: the device's failure, or a refusal of the kind.
    fn restore_decision_labels(
        &mut self,
        store: &mut render::LabelStore,
        slot: usize,
        staged: render::StagedLabels,
    ) -> Result<(), RenderError> {
        let _ = (store, slot, staged);
        Err(RenderError::LabelRegion { needed: 0, band: 0 })
    }
    /// A flag set once the work submitted so far completed
    /// ([`render::Renderer::watch_completion`]); set already by default,
    /// which submits nothing.
    fn watch_completion(&self) -> Arc<AtomicBool> {
        Arc::new(AtomicBool::new(true))
    }
    /// Whether the device was lost ([`render::Renderer::device_lost`]);
    /// never by default.
    fn device_lost(&self) -> bool {
        false
    }
    /// Builds a decision frame of the committed decision installation:
    /// `dec` at `frame`, its lines borrowed from `labels`, with no call
    /// into wgpu. Allocates nothing. The default refuses.
    ///
    /// # Errors
    ///
    /// The skin's refusal of the frame: nothing of it is staged.
    fn build_decision(
        &mut self,
        dec: &mut skin_model::DecisionScreen,
        labels: skin_model::DecisionLabels<'_>,
        frame: &skin_model::DecisionFrame<'_>,
    ) -> Result<(), BuildError> {
        let _ = (dec, labels, frame);
        Err(BuildError::Kind)
    }
    /// Draws the decision frame [`build_decision`](Self::build_decision)
    /// built, as [`draw`](Self::draw).
    ///
    /// # Errors
    ///
    /// As [`draw`](Self::draw).
    fn submit_decision(&mut self) -> Result<FrameTiming, RenderError> {
        self.draw()
    }
    /// Refuses the next decision frame, or fails the next settling of the
    /// decision labels as a poll fails with `settle` (dev-tools tests).
    /// Nothing by default.
    #[cfg(all(test, feature = "dev-tools"))]
    fn decision_fault(&mut self, settle: bool) {
        let _ = settle;
    }

    /// The result frame last built, its quads (dev-tools tests); none by
    /// default.
    #[cfg(all(test, feature = "dev-tools"))]
    fn result_quads(&self) -> Option<Vec<skin_model::Quad>> {
        None
    }
    /// How many texture arrays the screen uploaded so far (dev-tools);
    /// none by default.
    #[cfg(feature = "dev-tools")]
    fn atlas_uploads(&self) -> u64 {
        0
    }
    /// The last texture array upload's timing, taken once (dev-tools: the
    /// S10-09i design's §8.1 G1); none by default.
    #[cfg(feature = "dev-tools")]
    fn take_upload_timing(&mut self) -> Option<render::UploadTiming> {
        None
    }
    /// The strip's lines drawn (dev-tools tests); none by default.
    #[cfg(all(test, feature = "dev-tools"))]
    fn result_strip(&self) -> usize {
        0
    }
    /// Refuses the next result frame, or with `text` the next result
    /// text (dev-tools tests). Nothing by default.
    #[cfg(all(test, feature = "dev-tools"))]
    fn refuse_next_result(&mut self, text: bool) {
        let _ = text;
    }
    /// Waits until a frame may start; `false` on a timeout. Returns at
    /// once without a frame latency waitable object.
    fn wait(&self) -> bool;
    /// Whether [`wait`](Self::wait) paces frames.
    fn paced(&self) -> bool;
    /// Builds one play frame from `f`: the app's own work, which calls
    /// nothing in wgpu. [`draw`](Self::draw) follows a frame built.
    ///
    /// # Errors
    ///
    /// The skin's refusal of the frame, which allocates nothing: the
    /// attempt stops, nothing of it is drawn, and no frame of it is built
    /// again; the screen stays usable, and the next attempt's preparation
    /// builds again.
    fn build(&mut self, f: &FrameInput<'_>) -> Result<(), BuildError>;
    /// The play's READY candidate was rearmed (design.md "Play Lifecycle",
    /// the start gate): the skin's runtime state for the attempt starts
    /// over, its chart and text resources kept. Nothing by default.
    fn rearm(&mut self) {}
    /// What the committed attempt's `Play` gives the next attempt of its
    /// play state ([`render::PlaySkin::carry`]); none by default.
    fn carry(&self) -> Option<skin_model::Carry<'_>> {
        None
    }
    /// Draws the play frame [`build`](Self::build) built;
    /// [`FrameTiming::presented`] is `false` when it was not presented (the
    /// swap chain image timed out or the window is occluded).
    ///
    /// # Errors
    ///
    /// Why the frame failed: the screen must be built again.
    fn draw(&mut self) -> Result<FrameTiming, RenderError>;
    /// The swap chain's latest frame statistics, raw; `None` without a
    /// swap chain or when the call failed.
    fn statistics(&self) -> Option<FrameStatistics> {
        None
    }
    /// How the swap chain presents; `None` without one.
    fn presentation(&self) -> Option<Presentation> {
        None
    }
    /// Draws lines of text, as [`draw`](Self::draw).
    ///
    /// # Errors
    ///
    /// As [`draw`](Self::draw).
    fn text(&mut self, lines: &[&str]) -> Result<bool, RenderError>;
    /// Draws the end screen's `page`, its lines and its graphs, as
    /// [`draw`](Self::draw). A screen that draws no graphs draws the lines
    /// as [`text`](Self::text).
    ///
    /// # Errors
    ///
    /// As [`draw`](Self::draw).
    fn end(&mut self, page: &EndPage) -> Result<bool, RenderError> {
        let lines: Vec<&str> = page.lines.iter().map(String::as_str).collect();
        self.text(&lines)
    }
    /// Draws the select screen's `page`, laid out again when `changed`
    /// (or when this screen has not laid it out yet). A screen that
    /// draws no menus draws nothing.
    ///
    /// # Errors
    ///
    /// As [`draw`](Self::draw), and why the page's text cannot be prepared.
    fn menu(&mut self, page: &MenuPage, changed: bool) -> Result<FrameTiming, RenderError> {
        let _ = (page, changed);
        Ok(FrameTiming {
            cpu: Duration::ZERO,
            sprites: 0,
            dropped: 0,
            presented: true,
            present_count: None,
        })
    }
    /// Draws the select screen from `update` at `now`, with the
    /// application's readouts `app` sampled at that same reading: with
    /// `logic`, the selection installation's state for the update's
    /// lifetime, the skin and the strip over it, its text prepared again
    /// when `changed` (or when this screen has not prepared it); else the
    /// update's page, as [`menu`](Self::menu). A skin that refuses its frame or its text on
    /// a healthy device has the update's page drawn instead in the same
    /// frame, and says why ([`SelectionDrawn::refused`]). The default draws
    /// the page.
    ///
    /// # Errors
    ///
    /// As [`menu`](Self::menu).
    fn selection(
        &mut self,
        update: &SelectionUpdate,
        logic: Option<&mut SelectionLogic>,
        changed: bool,
        now: TimeUs,
        app: skin_model::AppReadouts,
    ) -> Result<SelectionDrawn, RenderError> {
        let _ = (logic, now, app);
        self.menu(&update.page, changed)
            .map(|timing| SelectionDrawn {
                timing,
                refused: None,
            })
    }
    /// The lines the selection skin's text last asked for, and how many
    /// were rasterized (dev-tools tests); none by default.
    #[cfg(all(test, feature = "dev-tools"))]
    fn selection_requests(&self) -> (Vec<String>, usize) {
        (Vec::new(), 0)
    }
    /// The selection strip's lines last prepared, and whether the last
    /// selection frame drew them (dev-tools tests); none by default.
    #[cfg(all(test, feature = "dev-tools"))]
    fn selection_strip(&self) -> (Vec<String>, bool) {
        (Vec::new(), false)
    }
    /// The selection frame last built, its quads (dev-tools tests); none
    /// by default.
    #[cfg(all(test, feature = "dev-tools"))]
    fn selection_quads(&self) -> Option<Vec<skin_model::Quad>> {
        None
    }
    /// What the selection frame last built observed of the skin's
    /// elements (dev-tools tests); none by default.
    #[cfg(all(test, feature = "dev-tools"))]
    fn selection_observations(&self) -> Option<Vec<skin_model::ElementObservation>> {
        None
    }
    /// How many views the selection skin has taken
    /// ([`skin_model::Skin::update_selection`]) on this screen (dev-tools
    /// tests); none by default.
    #[cfg(all(test, feature = "dev-tools"))]
    fn selection_views(&self) -> u64 {
        0
    }
    /// Draws the play screen while a chart loads before READY:
    /// the skin's loading frame `frame`, then `page`, the load's lines,
    /// over it in a pass of their own, laid out again when `changed`
    /// (design.md "Play Lifecycle"). `update` is the loading lifetime's
    /// newest state, which `frame` was built from; `None` before the
    /// lifetime published one. The default draws the lines as a menu
    /// page.
    ///
    /// # Errors
    ///
    /// The outer error as [`menu`](Self::menu). The inner one is the
    /// skin's refusal of the loading frame: nothing of it, its lines
    /// included, is drawn, the screen stays, and no loading frame of that
    /// lifetime is drawn again.
    fn loading(
        &mut self,
        page: &MenuPage,
        update: Option<&LoadingUpdate>,
        changed: bool,
        frame: &LoadingFrame<'_>,
    ) -> Result<Result<FrameTiming, BuildError>, RenderError> {
        let _ = (update, frame);
        self.menu(page, changed).map(Ok)
    }
    /// Draws the play screen's loading state while play's skin is pending
    /// and no specialization of it was committed in the loading lifetime:
    /// a cleared canvas with `page`, the load's lines, where
    /// [`loading`](Self::loading) puts them, laid out again when `changed`.
    /// No skin's frame is drawn. The default draws the lines as a menu
    /// page.
    ///
    /// # Errors
    ///
    /// As [`menu`](Self::menu).
    fn pending_loading(
        &mut self,
        page: &MenuPage,
        changed: bool,
    ) -> Result<FrameTiming, RenderError> {
        self.menu(page, changed)
    }
    /// Draws the launch screen `elapsed` after the render thread started,
    /// as [`text`](Self::text), until the game sends what to draw; `None`
    /// draws nothing instead (no launch screen).
    ///
    /// # Errors
    ///
    /// As [`menu`](Self::menu).
    fn splash(&mut self, elapsed: Duration) -> Option<Result<bool, RenderError>> {
        let _ = elapsed;
        None
    }
    /// Applies the window's newest physical size between frames: a zero
    /// side minimizes ([`minimized`](Self::minimized)), and the play state
    /// is kept. The default ignores it.
    fn resize(&mut self, width: u32, height: u32) {
        let _ = (width, height);
    }
    /// The target's size in pixels as last applied, never zero: what a
    /// play frame is projected onto ([`FrameInput::target`]). The default
    /// is the 1920×1080 layout's.
    fn target(&self) -> [u32; 2] {
        [1_920, 1_080]
    }
    /// The limits a skin's atlas is packed within for this screen's
    /// device ([`render::Renderer::atlas_limits`]); `None` for a screen
    /// that installs no declarative skin, the default.
    fn atlas_limits(&self) -> Option<skin_model::AtlasLimits> {
        None
    }
    /// Whether the window is minimized: frames are neither paced nor
    /// presented until a nonzero size is applied. The default is never.
    fn minimized(&self) -> bool {
        false
    }
    /// The last frame's pixels (RGBA8) and size, for an offscreen target.
    #[cfg(feature = "dev-tools")]
    fn read_back(&self) -> Option<(u32, u32, Vec<u8>)>;

    /// The last play frame's list, for a skin that keeps one.
    #[cfg(feature = "dev-tools")]
    fn display_list(&self) -> Option<skin_model::DisplayList> {
        None
    }
}

/// The window's newest physical size, from the window thread to the render
/// thread: each publish replaces the size not yet applied, and wakes the
/// render thread with a [`RenderMsg::Resize`] only when none is on its
/// way, so however often the window resizes, one size and one message
/// wait.
///
/// One render thread is woken: the one whose link registered last
/// ([`RenderLink::follow`]); a link that registered before is superseded
/// and gets no later wake.
///
/// Its invariant, for each registration: while a render thread follows the
/// slot, `queued` is set only while a `Resize` sent after that
/// registration's `Follow` waits in the channel, because
/// [`RenderLink::follow`] clears it and sends its `Follow` under the lock
/// every publish sends under, and the render thread acknowledges the slot
/// it follows as it takes each `Resize`. A flag an earlier registration
/// left set (its wake sent to a thread that ended, or taken while another
/// slot was followed) is cleared by the next one; a `Resize` from a slot
/// no longer followed may clear the followed slot's flag early, which
/// costs one extra wake, never a lost one.
#[derive(Debug)]
pub struct Extent {
    /// Width in the high 32 bits, height in the low; [`UNKNOWN`] before
    /// the first publish.
    size: AtomicU64,
    /// A [`RenderMsg::Resize`] is on its way for [`size`](Self::size).
    queued: AtomicBool,
    /// Where the wake goes, once a render thread follows the slot
    /// ([`RenderLink::follow`]).
    wake: Mutex<Option<Sender<RenderMsg>>>,
}

/// No size published yet.
const UNKNOWN: u64 = u64::MAX;

impl Default for Extent {
    fn default() -> Self {
        Self {
            size: AtomicU64::new(UNKNOWN),
            queued: AtomicBool::new(false),
            wake: Mutex::new(None),
        }
    }
}

impl Extent {
    /// Publishes the window's client size in physical pixels, zero sides
    /// when minimized, and wakes the render thread unless a wake is on its
    /// way already.
    pub fn publish(&self, width: u32, height: u32) {
        let packed = (u64::from(width) << 32) | u64::from(height);
        self.size.store(packed, Ordering::Release);
        // Under the sender's lock, so a wake is marked only when it is
        // sent: before a render thread follows, none is, and the
        // [`RenderMsg::Follow`] that comes after reads the newest size.
        let wake = self.wake.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(tx) = wake.as_ref()
            && !self.queued.swap(true, Ordering::AcqRel)
        {
            // A render thread that ended ignores it; the flag stays set
            // until the next registration clears it.
            let _ = tx.send(RenderMsg::Resize);
        }
    }

    /// Takes the wake on its way: the render thread calls it as it takes a
    /// [`RenderMsg::Resize`], before it reads the size.
    /// A publish after this sends again; one before it was seen through
    /// the flag it set, so its size is the one read next.
    fn acknowledge(&self) {
        self.queued.swap(false, Ordering::AcqRel);
    }

    /// The newest size published, if any. Reading it takes no wake, so a
    /// wake still on its way stays the only one. Allocates nothing: the
    /// render thread calls it between play frames.
    fn latest(&self) -> Option<(u32, u32)> {
        let packed = self.size.load(Ordering::Acquire);
        // The halves of a packed size.
        #[expect(
            clippy::cast_possible_truncation,
            reason = "each half holds one u32 side"
        )]
        let size = ((packed >> 32) as u32, packed as u32);
        (packed != UNKNOWN).then_some(size)
    }
}

/// Builds the screen on the render thread, at startup and after a failure.
pub type MakeScreen = Box<dyn FnMut() -> Result<Box<dyn Screen>, RenderError> + Send>;

/// Why the screen cannot draw a play.
#[derive(Debug, thiserror::Error)]
pub enum ScreenError {
    /// The renderer could not be built or cannot draw the chart.
    #[error(transparent)]
    Render(#[from] RenderError),
    /// The render thread could not be spawned.
    #[error("the render thread cannot start")]
    Spawn(#[source] std::io::Error),
    /// The render thread ended before it answered a preparation.
    #[error("the render thread ended")]
    Ended,
    /// The render thread ended before its screen was built.
    #[error("the render thread ended while starting")]
    EndedWhileStarting,
    /// The render thread did not answer a chart's load in time.
    #[error("the render thread did not answer")]
    NoAnswer,
    /// The screen failed after the play's attempt was prepared, before
    /// recording opened.
    #[error("the screen failed before the play opened")]
    Lost,
    /// The play skin refused a loading frame of the play's loading
    /// state: recording must not open.
    #[error("the play skin refused its loading frame")]
    LoadingRefused(#[source] BuildError),
}

/// How the render thread answered the end of a loading lifetime.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LoadingEnd {
    /// No loading frame of it is drawn any more, and the screen is there.
    Ready,
    /// The screen failed after the attempt was prepared.
    Lost,
    /// The skin refused a loading frame of it.
    Refused(BuildError),
}

/// The end screen: its lines and the view's graphs, if it has them.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EndPage {
    /// The lines, the settings lines last.
    pub lines: Vec<String>,
    /// The viewed summary's graphs, shared with the result screen.
    pub graphs: Option<Arc<ResultGraphs>>,
}

/// An attempt's refusal for `reason`, at no location.
pub(crate) fn refusal(reason: impl Into<String>) -> Preparation {
    Preparation::Refused(PreparationRefusal {
        location: None,
        reason: reason.into(),
    })
}

/// The play skin an attempt is prepared with.
#[derive(Debug)]
pub enum AttemptSkin {
    /// The skin in use: the one selected last, or the simple skin. While
    /// play's skin is pending it is refused, the screen not asked.
    InUse,
    /// A pending play skin's specialization for this attempt, staged with
    /// it and installed only by its commit.
    Specialized(Arc<SkinBundle>),
}

/// What a selection ([`RenderMsg::Select`]) selects for its kind.
#[derive(Debug)]
pub enum Selected {
    /// The skin, or `None` for the skin in use, with its screen's runtime
    /// if its import kept one, parked at the installation's commit.
    Skin(Option<Arc<SkinBundle>>, ScriptSlot),
    /// Play's skin is pending: no installation until an attempt's
    /// specialization is committed, and no attempt without one.
    Pending,
    /// The result kind's pages-only installation of a prepared atlas.
    Pages(Arc<PackedPages>),
}

/// What the game thread tells the render thread.
pub enum RenderMsg {
    /// The adjustments' presentation from now on: the reader of the
    /// snapshots every play and loading frame is drawn from. Sent once, at
    /// the start.
    Presentation(Box<game::adjustment::PresentationReader>),
    /// The settings keys' values, which every selection frame reads the
    /// volume and the display offset from. Sent once, at the start.
    Tuning(Arc<LiveTuning>),
    /// Make sure the screen can draw, building it again if it failed (the
    /// device's recovery, for a play and the menus alike), and answer when
    /// it can. It prepares no attempt.
    Prepare(mpsc::SyncSender<Result<(), RenderError>>),
    /// Prepare the chart loaded last for this attempt, building the screen
    /// again first if it failed, and answer how the preparation ended: a
    /// refusal keeps the screen. Never [`Preparation::Cancelled`], which is
    /// the game thread's. It supersedes every earlier request.
    PrepareAttempt {
        /// The request's number.
        request: u64,
        /// The attempt, owned.
        attempt: Box<AttemptContext>,
        /// The play skin it is prepared with.
        skin: AttemptSkin,
        /// Where the answer goes.
        reply: mpsc::SyncSender<Result<Answered, RenderError>>,
    },
    /// The attempt's final readiness, bounded: when what this request
    /// staged still fits the screen's structural inputs, stamp the
    /// adjustment epoch on it and answer ready, else answer `Stale` with
    /// the inputs now. It prepares nothing.
    Ready {
        /// The request.
        request: u64,
        /// The adjustment epoch the attempt's frames are drawn for.
        epoch: u64,
        /// Where the answer goes.
        reply: mpsc::SyncSender<ReadyAnswer>,
    },
    /// The committed attempt that started ended with this final tally:
    /// the play state's carry takes its point and, when witnessed, its
    /// slots ([`crate::carry::Carries::settled`]).
    Settled(Box<eval::Tally>),
    /// Another chart: prepare it, building the screen again if it failed,
    /// and answer when ready. Every play draws it until the next.
    Load {
        /// The chart.
        chart: Arc<Chart>,
        /// The section a practice round draws only the targets of.
        section: Option<Section>,
        /// Where the answer goes.
        reply: mpsc::SyncSender<Result<(), RenderError>>,
    },
    /// Install what the preparation of this request staged, and answer
    /// whether it was, with its receipt (what the play skin declares,
    /// [`Declared`]):
    /// not when the request was discarded or superseded, or its screen
    /// was built again since.
    Commit {
        /// The request.
        request: u64,
        /// Where the answer goes.
        reply: mpsc::SyncSender<Result<Declared, CommitError>>,
    },
    /// Drop the preparation of this request, answered or not, unless
    /// committed: it was cancelled, refused or timed out.
    Discard(u64),
    /// Publish the chart's images (S10-12): into the staged array of the
    /// attempt `request` prepared when it has one of its own, else into the
    /// committed play installation; `set` `None` publishes them absent.
    /// Answer what it left.
    ChartImages {
        /// The images' generation it is for: refused, nothing published,
        /// once the game thread set another.
        generation: u64,
        /// The attempt's request, if one is staged for it.
        request: Option<u64>,
        /// The images, or `None` for absent.
        set: Option<Arc<crate::chart_images::ChartImages>>,
        /// Where the answer goes.
        reply: mpsc::SyncSender<crate::chart_images::Ack>,
    },
    /// Install this declarative skin's chart-independent installation
    /// for the screen kind, and for play prepare every later attempt
    /// with it; `None` prepares play's later attempts with the skin in
    /// use, and removes another kind's installation; [`Selected::Pending`]
    /// marks play's skin pending. Answer how it ended: ready once
    /// committed or removed, a refusal keeping every installation in use,
    /// or the screen's failure. Never committed or removed once the
    /// request is withdrawn.
    Select {
        /// The request's number, of the same sequence as the attempts'.
        request: u64,
        /// The screen kind.
        kind: ScreenKind,
        /// What is selected.
        skin: Selected,
        /// Where the answer goes.
        reply: mpsc::SyncSender<Result<Preparation, RenderError>>,
    },
    /// Draw the select screen's page instead of a play.
    Menu(Box<MenuPage>),
    /// Draw the decision screen: its built-in presentation `page`, or
    /// with `shown` pinning a label slot the decision skin, the page
    /// drawn when the skin cannot draw; kept through its chart's load and
    /// the screen's preparations.
    Decision {
        page: Box<MenuPage>,
        shown: Option<crate::decision_side::ShownDecision>,
    },
    /// Draw the practice page: a [`Menu`](Self::Menu) that is its play
    /// state's presentation, so the play state's turntable is adopted,
    /// resumed when the game resumed it, and ticked on its frames.
    PracticePage(Box<MenuPage>),
    /// Parks every runtime a screen holds, answered once it has: before
    /// recording opens ([`RenderLink::park_scripts`]).
    ParkScripts(mpsc::SyncSender<()>),
    /// Answer when taken (tests): the render thread has taken every
    /// message before it, and drawn the frame that followed them, if any.
    #[cfg(test)]
    Fence(mpsc::SyncSender<()>),
    /// A test's look at the mode the render thread draws after every
    /// message before it: `idle`, `menu:<its first line>`, or `other`.
    #[cfg(test)]
    ModeProbe(mpsc::SyncSender<String>),
    /// Makes the render thread panic (tests: its failure in a play).
    #[cfg(all(test, feature = "dev-tools"))]
    Panic,
    /// Answers what the whole-frame records hold (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    FramesProbe(mpsc::SyncSender<crate::dev::FramesProbe>),
    /// Draw the select screen from the newest update in the slot
    /// ([`RenderLink::show_selection`]), if it is newer than the one held
    /// and its lifetime current.
    SelectionChanged,
    /// Draw the result screen from the newest presentation in the slot
    /// ([`RenderLink::show_result`]), if its lifetime is current.
    ResultChanged,
    /// Draw the play screen's loading state of this loading lifetime with
    /// the lines in the slot ([`LoadingSink`]), kept through a chart's load
    /// and a preparation until something else is drawn; nothing when the
    /// lifetime has ended ([`RenderLink::clear_loading`]).
    Loading(u64),
    /// Loading lifetime `lifetime` has ended: stop drawing the loading
    /// state, and answer once no loading frame can be drawn any more, with
    /// whether the screen is still there and the skin drew every loading
    /// frame of it ([`RenderLink::end_loading`]).
    EndLoading {
        /// The lifetime ended.
        lifetime: u64,
        /// Where the answer goes.
        reply: mpsc::SyncSender<LoadingEnd>,
    },
    /// Loading lifetime `lifetime` pauses, still current: stop drawing its
    /// loading state but keep it, its lines and newest state with it, and
    /// answer as [`EndLoading`](Self::EndLoading) does
    /// ([`RenderLink::pause_loading`]).
    PauseLoading {
        /// The lifetime paused.
        lifetime: u64,
        /// Where the answer goes.
        reply: mpsc::SyncSender<LoadingEnd>,
    },
    /// Loading lifetime `lifetime`, paused and still current, is drawn
    /// again as it was kept ([`RenderLink::resume_loading`]).
    ResumeLoading(u64),
    /// A play starts: chart time 0 at the measurement's `origin` on the
    /// clock.
    Play {
        /// The logic step's effects and play view.
        side: RenderSide,
        /// The gauge to show.
        gauge: usize,
        /// What its measurement needs.
        measure: Box<PlayMeasure>,
        /// Where the play state was entered on the clock: each frame's
        /// state clock runs from it, whatever its candidate's origin.
        state_origin: TimeUs,
    },
    /// The game created this play state (docs/stage3.md, offset 1): its
    /// turntable clock is adopted when its presentation begins.
    PlayState(game::PlayState),
    /// Play state `number`'s turntable clock pauses (`pause`) or resumes,
    /// whatever is presented.
    Turntable {
        /// The play state.
        number: u64,
        /// Whether it pauses rather than resumes.
        pause: bool,
    },
    /// The play ended at `end` on the clock: answer with its measured
    /// figures, and count nothing more until the next play.
    Finish {
        /// When it ended.
        end: TimeUs,
        /// Where the figures go.
        reply: mpsc::SyncSender<Option<Figures>>,
    },
    /// The window's size changed: apply the newest in the [`Extent`]
    /// followed, between frames.
    Resize,
    /// Follow this window's size from here ([`RenderLink::follow`]).
    Follow(Arc<Extent>),
    /// Draw the end screen instead of a play: the newest page in the slot
    /// ([`RenderLink::show_end`]), or the one drawn last when an earlier
    /// signal already took this one's page or a newer one; nothing once
    /// the pages shown were cleared ([`RenderLink::clear_end`]).
    End,
    /// The run ends.
    Exit,
}

/// The inputs of every play's frames.
#[derive(Clone, Debug)]
pub struct View {
    /// The effect queue's capacity: the most effects one frame drains.
    pub effects: usize,
}

/// How long the game thread waits for a chart's load: the render thread
/// answers between frames, after the skin's uploads.
const LOAD_TIMEOUT: Duration = Duration::from_secs(30);

/// How often a waiting attempt's preparation looks at whether the player
/// left.
const CANCEL_POLL: Duration = Duration::from_millis(10);

/// The play frames whose acquisition is skipped (a rearm whose view the
/// logic step has not published yet) that are retried at once, the frame
/// granted kept; each later retry first waits [`ACQUIRE_WAIT`] for a
/// message, so a logic step held between the two publications costs no
/// spinning, and a message still ends the wait at once.
const ACQUIRE_RETRIES: u32 = 4;

/// How long a play frame waits for a message before its acquisition is
/// tried again, past [`ACQUIRE_RETRIES`] skips in a row.
const ACQUIRE_WAIT: Duration = Duration::from_millis(1);

/// How long the end of the render thread is waited for: past it, the thread
/// is left to the process's exit.
const EXIT_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the game thread waits for a play's figures. The render thread
/// answers between frames, so this bounds only a stalled render thread: a
/// slow frame, such as a software adapter drawing and the dev-tools frame
/// dump encoding an image, can take seconds, and the figures are still
/// wanted after it.
const FINISH_TIMEOUT: Duration = Duration::from_secs(60);

/// Something that looks at frames as they are drawn, after the measurement
/// (design.md "Latency Figures"): the dev-tools frame dump. Called after every
/// frame, presented or not.
pub trait FrameSink: Send {
    /// Called after each frame.
    fn frame(&mut self, screen: &dyn Screen, drawn: &Drawn<'_>);

    /// Called at a play's end, before it is answered: what the sink still
    /// does for the frames drawn so far is done when this returns.
    #[cfg(feature = "dev-tools")]
    fn finish(&mut self) {}
}

/// No frame sink.
pub struct NoFrames;

impl FrameSink for NoFrames {
    fn frame(&mut self, _screen: &dyn Screen, _drawn: &Drawn<'_>) {}
}

/// What wakes the game thread for a decision takeover
/// ([`RenderLink::set_game_wake`]).
#[derive(Default)]
struct GameWake(Mutex<Option<Arc<dyn Fn() + Send + Sync>>>);

impl std::fmt::Debug for GameWake {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("GameWake")
    }
}

#[derive(Debug)]
struct Shared {
    failed: AtomicBool,
    /// The one owner of the screen's rebuild after a loss: none, one under
    /// way, or the failure no one has reported yet. Every look, recovery
    /// and preparation goes through [`serve_recovery`]; the render thread
    /// never locks it.
    recovery: Mutex<Recovery>,
    /// The chart images' generation, which the game thread's worker
    /// writes ([`crate::chart_images::ChartImageWorker::renew`]) and the
    /// render thread reads before each frame and publication.
    chart_generation: Arc<AtomicU64>,
    /// The chart images' live targets as the render thread last saw them:
    /// the staging's request and the committed chart layer's key, which
    /// the game thread checks an acknowledgment against.
    chart_targets: Mutex<(Option<u64>, Option<skin_model::chart_image::ChartKey>)>,
    /// While set, a chart publication waits before it publishes, at most
    /// [`hook_bound`] (tests).
    #[cfg(test)]
    chart_hold: AtomicBool,
    /// When the current play's first frame was drawn, on the clock.
    first_frame: AtomicI64,
    /// The end screen's slot: its newest page, and the pages left with the
    /// end screen.
    end_page: Mutex<EndSlot>,
    /// The number of the end page last presented, after the frame sink
    /// saw it (dev-tools).
    #[cfg(feature = "dev-tools")]
    end_drawn: AtomicU64,
    /// Select screen frames presented, each counted after the frame sink
    /// saw it (dev-tools: a script's `drawn` step on the select screen).
    #[cfg(feature = "dev-tools")]
    selection_drawn: AtomicU64,
    /// Loading frames presented (dev-tools: a slow load's checkpoints).
    #[cfg(feature = "dev-tools")]
    loading_drawn: AtomicU64,
    /// The loading state's slot.
    loading: Mutex<LoadingSlot>,
    /// The latest loading lifetime's first presented frame, and the play
    /// skin's declared loading time as the last loading frame was drawn
    /// with it ([`LoadingAck`]).
    loading_ack: Mutex<LoadingAck>,
    /// A presented loading frame's acknowledgement holds while set, and
    /// whether one holds (tests).
    #[cfg(test)]
    loading_ack_hold: [AtomicBool; 2],
    /// Held acknowledgements still let through, one each (tests).
    #[cfg(test)]
    loading_ack_pass: std::sync::atomic::AtomicU32,
    /// Presented loading frames acknowledged so far (tests).
    #[cfg(test)]
    loading_acked: AtomicU64,
    /// The decision screen's presentations sent (tests).
    #[cfg(test)]
    decisions_sent: AtomicU64,
    /// The chart loads built again once more after a lost device (tests).
    #[cfg(test)]
    chart_rebuilds: AtomicU64,
    /// The rebuilds asked of the render thread ([`RenderMsg::Prepare`]),
    /// each counted as it is taken (tests).
    #[cfg(test)]
    prepares: AtomicU64,
    /// The end or pause of a loading lifetime waits here before its
    /// answer (tests).
    #[cfg(test)]
    loading_end_gate: Arc<TestGate>,
    /// The presentation epochs of the attempt staged last, as prepared,
    /// and of the attempt committed last (tests).
    #[cfg(test)]
    epochs: [AtomicU64; 2],
    /// The current loading lifetime's state, from its beginning
    /// ([`RenderLink::begin_loading`]); `None` before. Locked before the
    /// slot when both are.
    producer: Mutex<Option<LifetimeState>>,
    /// The commits answered and given up on, which the game thread and the
    /// render thread settle under its lock.
    commits: Mutex<Commits>,
    /// Set when the game thread stopped waiting for a commit's answer,
    /// before it takes the commits' lock (tests).
    #[cfg(test)]
    commit_timed_out: AtomicBool,
    /// While set, a commit waits between its receipt and its loading
    /// time's declaration, and says so in `declaration_held`, at most
    /// [`hook_bound`] (tests).
    #[cfg(test)]
    hold_declaration: AtomicBool,
    #[cfg(test)]
    declaration_held: AtomicBool,
    /// Set when a held declaration let go at [`hook_bound`] instead of
    /// at the test's release (tests).
    #[cfg(test)]
    declaration_expired: AtomicBool,
    /// The atlas limits and the target size of the screen built last,
    /// written at each build: what a skin is packed for
    /// ([`RenderLink::atlas_limits`]).
    packing: Mutex<Option<(skin_model::AtlasLimits, [u32; 2])>>,
    /// The select screen's newest update and its current lifetime.
    selection: Mutex<SelectionSlot>,
    /// Why the selection skin was refused at runtime, until the game
    /// thread takes it for its notice ([`RenderLink::take_selection_refusal`]).
    selection_refused: Mutex<Option<String>>,
    /// The decision labels' slot states: the render thread prepares them,
    /// the game thread pins one at the choice.
    labels: Arc<Mutex<render::LabelBook>>,
    /// The active decision lifetime's takeover, until the game thread
    /// takes it ([`RenderLink::take_decision_takeover`]).
    decision_takeover: Mutex<Option<game::DecisionTakeover>>,
    /// Why the decision skin was refused for the run, until the game
    /// thread takes it for its notice ([`RenderLink::take_decision_refusal`]).
    decision_refused: Mutex<Option<String>>,
    /// What wakes the game thread for a takeover.
    game_wake: GameWake,
    /// The takeovers produced, each counted at its wake (tests).
    #[cfg(test)]
    takeovers_woken: AtomicU64,
    /// A takeover waits here between its lifetime's check and its
    /// publication while a test holds it (tests).
    #[cfg(test)]
    takeover_gate: TestGate,
    /// How long the launch screen has shown, in place of the clock's
    /// reading, while set (tests: [`RenderLink::set_launch_elapsed`]).
    #[cfg(test)]
    launch_elapsed: Mutex<Option<Duration>>,
    /// The result screen's newest presentation and its lifetime.
    result: Mutex<ResultSlot>,
    /// The result's barrier, with the view presented, which the commit
    /// point writes under its lock.
    barrier: Mutex<ResultBarrier>,
    /// What the render thread shows of the result kind.
    result_status: Mutex<ResultReport>,
    /// Result frames hold after their lifetime's check while set, and
    /// whether one holds (tests).
    #[cfg(test)]
    result_frame_hold: [AtomicBool; 2],
    /// Select frames stall 2 ms after their instant is sampled while set
    /// (tests): a second clock reading in the frame would differ from it.
    #[cfg(test)]
    selection_stall: AtomicBool,
}

impl Shared {
    /// The state shared with a render thread not started yet.
    fn new() -> Self {
        Self {
            failed: AtomicBool::new(false),
            recovery: Mutex::new(Recovery::Idle),
            chart_generation: Arc::default(),
            chart_targets: Mutex::new((None, None)),
            #[cfg(test)]
            chart_hold: AtomicBool::new(false),
            first_frame: AtomicI64::new(i64::MIN),
            end_page: Mutex::new(EndSlot::default()),
            #[cfg(feature = "dev-tools")]
            end_drawn: AtomicU64::new(0),
            #[cfg(feature = "dev-tools")]
            selection_drawn: AtomicU64::new(0),
            #[cfg(feature = "dev-tools")]
            loading_drawn: AtomicU64::new(0),
            loading: Mutex::new(LoadingSlot::default()),
            loading_ack: Mutex::new(LoadingAck::default()),
            #[cfg(test)]
            loading_ack_hold: [AtomicBool::new(false), AtomicBool::new(false)],
            #[cfg(test)]
            loading_ack_pass: std::sync::atomic::AtomicU32::new(0),
            #[cfg(test)]
            loading_acked: AtomicU64::new(0),
            #[cfg(test)]
            decisions_sent: AtomicU64::new(0),
            #[cfg(test)]
            chart_rebuilds: AtomicU64::new(0),
            #[cfg(test)]
            prepares: AtomicU64::new(0),
            #[cfg(test)]
            loading_end_gate: Arc::default(),
            #[cfg(test)]
            epochs: [AtomicU64::new(0), AtomicU64::new(0)],
            producer: Mutex::new(None),
            commits: Mutex::new(Commits::default()),
            #[cfg(test)]
            commit_timed_out: AtomicBool::new(false),
            #[cfg(test)]
            hold_declaration: AtomicBool::new(false),
            #[cfg(test)]
            declaration_held: AtomicBool::new(false),
            #[cfg(test)]
            declaration_expired: AtomicBool::new(false),
            packing: Mutex::new(None),
            selection: Mutex::new(SelectionSlot::default()),
            selection_refused: Mutex::new(None),
            labels: render::LabelBook::shared([1_920, 1_080]),
            decision_takeover: Mutex::new(None),
            decision_refused: Mutex::new(None),
            game_wake: GameWake::default(),
            #[cfg(test)]
            takeovers_woken: AtomicU64::new(0),
            #[cfg(test)]
            takeover_gate: TestGate::default(),
            #[cfg(test)]
            launch_elapsed: Mutex::new(None),
            result: Mutex::new(ResultSlot::default()),
            barrier: Mutex::new(ResultBarrier::default()),
            result_status: Mutex::new(ResultReport::default()),
            #[cfg(test)]
            result_frame_hold: [AtomicBool::new(false), AtomicBool::new(false)],
            #[cfg(test)]
            selection_stall: AtomicBool::new(false),
        }
    }

    /// A loading frame of lifetime `lifetime` was presented, drawn with a
    /// play skin declaring the loading time `loadend`: the first of its
    /// lifetime is acknowledged at the clock's `now()`, tagged with the
    /// lifetime it was drawn for; a later one of the same lifetime changes
    /// only the declared time. Allocates nothing.
    fn presented_loading(
        &self,
        lifetime: u64,
        loadend: Option<TimeUs>,
        now: impl FnOnce() -> TimeUs,
    ) {
        #[cfg(test)]
        {
            let [held, waiting] = &self.loading_ack_hold;
            if held.load(Ordering::SeqCst) {
                waiting.store(true, Ordering::SeqCst);
                while held.load(Ordering::SeqCst)
                    && self
                        .loading_ack_pass
                        .try_update(Ordering::SeqCst, Ordering::SeqCst, |n| n.checked_sub(1))
                        .is_err()
                {
                    std::thread::sleep(Duration::from_millis(1));
                }
                waiting.store(false, Ordering::SeqCst);
            }
        }
        {
            let mut ack = self
                .loading_ack
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            ack.loadend = loadend;
            if ack.first.is_none_or(|(l, _)| l != lifetime) {
                ack.first = Some((lifetime, now()));
            }
        }
        #[cfg(test)]
        self.loading_acked.fetch_add(1, Ordering::SeqCst);
    }

    /// A specialization committed in the current loading lifetime declares
    /// the loading time `loadend`: the declaration from now on, before any
    /// frame draws with it. Allocates nothing.
    fn committed_loading(&self, loadend: Option<TimeUs>) {
        self.loading_ack
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .loadend = loadend;
    }

    /// The skin refused a loading frame of lifetime `lifetime` for `why`:
    /// published for the game thread's wait, which no acknowledgement
    /// would otherwise end. Allocates nothing.
    fn refused_loading(&self, lifetime: u64, why: BuildError) {
        self.loading_ack
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .refused = Some((lifetime, why));
    }

    /// What [`presented_loading`](Self::presented_loading),
    /// [`committed_loading`](Self::committed_loading) and
    /// [`refused_loading`](Self::refused_loading) left.
    fn loading_ack(&self) -> LoadingAck {
        *self
            .loading_ack
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Holds a result frame while a test says so.
    #[cfg(test)]
    fn hold_result_frame(&self) {
        let [held, waiting] = &self.result_frame_hold;
        if !held.load(Ordering::SeqCst) {
            return;
        }
        waiting.store(true, Ordering::SeqCst);
        while held.load(Ordering::SeqCst) {
            std::thread::sleep(Duration::from_millis(1));
        }
        waiting.store(false, Ordering::SeqCst);
    }

    fn result_slot(&self) -> std::sync::MutexGuard<'_, ResultSlot> {
        self.result.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn barrier(&self) -> std::sync::MutexGuard<'_, ResultBarrier> {
        self.barrier.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn result_status(&self) -> std::sync::MutexGuard<'_, ResultReport> {
        self.result_status
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

impl Shared {
    fn selection_slot(&self) -> std::sync::MutexGuard<'_, SelectionSlot> {
        self.selection
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether `lifetime` is selection's current one.
    fn selection_current(&self, lifetime: u64) -> bool {
        self.selection_slot().current(lifetime)
    }
}

/// The loading acknowledgement's test hold, from any thread (tests):
/// [`RenderLink::hold_loading_ack`] and its kin.
#[cfg(all(test, feature = "dev-tools"))]
pub struct AckHold(Arc<Shared>);

#[cfg(all(test, feature = "dev-tools"))]
impl AckHold {
    /// Holds every presented loading frame's acknowledgement while `on`.
    pub fn hold(&self, on: bool) {
        self.0.loading_ack_hold[0].store(on, Ordering::SeqCst);
    }

    /// Whether an acknowledgement is held.
    pub fn held(&self) -> bool {
        self.0.loading_ack_hold[1].load(Ordering::SeqCst)
    }

    /// Lets one held acknowledgement through.
    pub fn pass(&self) {
        self.0.loading_ack_pass.fetch_add(1, Ordering::SeqCst);
    }
}

/// A point a thread waits at while a test holds it (tests).
#[cfg(test)]
#[derive(Debug, Default)]
pub struct TestGate {
    held: AtomicBool,
    waiting: AtomicBool,
    passed: AtomicU64,
}

#[cfg(test)]
impl TestGate {
    /// Holds every thread reaching the gate from now on while `on`, or
    /// lets them go.
    #[cfg(feature = "dev-tools")]
    pub fn hold(&self, on: bool) {
        self.held.store(on, Ordering::SeqCst);
    }

    /// Whether a thread waits at the gate.
    #[cfg(feature = "dev-tools")]
    pub fn waiting(&self) -> bool {
        self.waiting.load(Ordering::SeqCst)
    }

    /// How often a thread went past the gate.
    #[cfg(feature = "dev-tools")]
    pub fn passed(&self) -> u64 {
        self.passed.load(Ordering::SeqCst)
    }

    /// Waits while held, then counts the passage.
    pub fn pass(&self) {
        if self.held.load(Ordering::SeqCst) {
            self.waiting.store(true, Ordering::SeqCst);
            while self.held.load(Ordering::SeqCst) {
                std::thread::sleep(Duration::from_millis(1));
            }
            self.waiting.store(false, Ordering::SeqCst);
        }
        self.passed.fetch_add(1, Ordering::SeqCst);
    }
}

/// The render thread's acknowledgement of the loading state's frames: the
/// latest lifetime whose first frame was presented, with when, and the
/// play skin's declared loading time ([`skin_model::Skin::loadend`]) as
/// the last presented loading frame drew with it, or as a specialization
/// committed since in the current lifetime declares it: the installed
/// skin's, or none while a pending skin has no specialization committed
/// in the lifetime. With them, the latest lifetime whose loading frame the
/// skin refused, and why.
#[derive(Clone, Copy, Debug, Default)]
struct LoadingAck {
    first: Option<(u64, TimeUs)>,
    loadend: Option<TimeUs>,
    refused: Option<(u64, BuildError)>,
}

/// The last request committed and the last whose commit the game thread
/// stopped waiting for: a commit is installed only while its request is
/// not withdrawn, and a withdrawal finds a commit already installed, so
/// the two never disagree. With them, the receipt of the attempt
/// committed last, taken in the same installation.
#[derive(Debug, Default)]
struct Commits {
    committed: u64,
    withdrawn: u64,
    receipt: Receipt,
}

/// What an attempt's commit gives the game thread: the request and what
/// the declarative installation the committed attempt draws with, the one
/// it staged or the one already in use, declares ([`Declared`]). A
/// pending skin's specialization committed in the current loading
/// lifetime also gives its declared loading time (`loadend`), which a game
/// thread that stopped waiting declares itself.
#[derive(Clone, Copy, Debug, Default)]
struct Receipt {
    request: u64,
    declared: Declared,
    loadend: Option<Option<TimeUs>>,
}

/// What the play skin an attempt draws with declares for the play's
/// lifecycle, from its commit's receipt: Closing's duration
/// ([`skin_model::Skin::close`]) and the play's ending
/// ([`skin_model::Skin::play_ending`]). The default, none declared, for
/// the simple skin and before a commit.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Declared {
    /// Closing's declared duration, `None` for none.
    pub close: Option<TimeUs>,
    /// The play's declared ending.
    pub ending: game::PlayEnding,
}

impl Declared {
    /// What `skin` declares; the default without a declarative skin.
    pub(crate) fn of(skin: Option<&skin_model::Skin>) -> Self {
        skin.map_or_else(Self::default, |s| {
            let e = s.play_ending();
            Self {
                close: s.close(),
                ending: game::PlayEnding {
                    finish_margin: e.finish_margin,
                    fadeout: e.fadeout,
                },
            }
        })
    }
}

/// The loading state's lifetime, its newest page and its newest state
/// until the render thread takes them or the lifetime ends, the highest
/// sequence number accepted in the lifetime, and whether a
/// [`RenderMsg::Loading`] of this lifetime waits in the channel. Each
/// clear starts a new lifetime: a sink, a signal or a frame of an older
/// one does nothing.
#[derive(Debug, Default)]
struct LoadingSlot {
    lifetime: u64,
    page: Option<Box<MenuPage>>,
    update: Option<Box<LoadingUpdate>>,
    /// The highest sequence number accepted in the lifetime, kept when
    /// the render thread takes what waits.
    accepted: u64,
    signaled: bool,
    /// Made at a choice and not shown until the decision screen's deadline
    /// reveals it: what is reported waits, and nothing is signalled.
    held: bool,
}

/// What the render thread takes from the slot: the newest page and the
/// newest state, each if one came since.
type TakenLoading = (Option<Box<MenuPage>>, Option<Box<LoadingUpdate>>);

impl LoadingSlot {
    /// Puts `page` and `update` of lifetime `lifetime` in the slot, both
    /// replacing what waits, unless the lifetime ended or `update` is not
    /// newer than every state accepted in it: then nothing changes.
    /// Whether it was put.
    fn offer(
        &mut self,
        lifetime: u64,
        page: Option<MenuPage>,
        update: Option<Box<LoadingUpdate>>,
    ) -> bool {
        if lifetime != self.lifetime {
            return false;
        }
        if let Some(u) = update {
            if u.seq <= self.accepted {
                return false;
            }
            self.accepted = u.seq;
            drop(self.update.replace(u));
        }
        if let Some(page) = page {
            drop(self.page.replace(Box::new(page)));
        }
        true
    }

    /// Takes what waits of lifetime `lifetime`, if it is still the
    /// current one, with the signal that came for it.
    fn take(&mut self, lifetime: u64) -> Option<TakenLoading> {
        if self.lifetime != lifetime {
            return None;
        }
        self.signaled = false;
        let taken = (self.page.take(), self.update.take());
        (taken.0.is_some() || taken.1.is_some()).then_some(taken)
    }

    /// Ends the current lifetime: what waits is dropped, and the next one
    /// accepts from its first number.
    fn end(&mut self) {
        self.lifetime = self.lifetime.wrapping_add(1);
        self.signaled = false;
        self.held = false;
        self.accepted = 0;
        drop(self.page.take());
        drop(self.update.take());
    }
}

/// Whether the render thread applies `incoming` over `applied`, the
/// state its loading frames are built from: only a newer one, so an
/// older state never comes back.
fn newer(applied: Option<&LoadingUpdate>, incoming: &LoadingUpdate) -> bool {
    applied.is_none_or(|a| incoming.seq > a.seq)
}

/// The end screen's newest page with its number, until the render thread
/// takes it or the end screen is left, and the number of the last page
/// shown when it was last left: no page up to it is drawn again.
#[derive(Debug, Default)]
struct EndSlot {
    page: Option<(u64, EndPage)>,
    cleared: u64,
}

impl Shared {
    /// Publishes what skins are packed for on `screen`, just built.
    fn built(&self, screen: &dyn Screen) {
        *self.packing.lock().unwrap_or_else(PoisonError::into_inner) =
            screen.atlas_limits().map(|l| (l, screen.target()));
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, EndSlot> {
        self.end_page.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Puts page `n` into the slot, returning what it held.
    fn put_end(&self, n: u64, page: EndPage) -> Option<(u64, EndPage)> {
        self.slot().page.replace((n, page))
    }

    /// Leaves the end screen with page `upto` shown last: the slot is
    /// emptied, returning what it held, and no page up to `upto` is valid.
    fn clear_end(&self, upto: u64) -> Option<(u64, EndPage)> {
        let mut slot = self.slot();
        slot.cleared = slot.cleared.max(upto);
        slot.page.take()
    }

    /// Takes the waiting page, if any.
    fn take_end(&self) -> Option<(u64, EndPage)> {
        self.slot().page.take()
    }

    /// The number of the last page cleared.
    fn end_cleared(&self) -> u64 {
        self.slot().cleared
    }

    fn commits(&self) -> std::sync::MutexGuard<'_, Commits> {
        self.commits.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn loading_slot(&self) -> std::sync::MutexGuard<'_, LoadingSlot> {
        self.loading.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Takes the waiting lines of loading lifetime `lifetime` and its
    /// newest state, if it is still the current one: the signal that came
    /// for them is taken with them. A signal of an ended lifetime takes
    /// nothing.
    fn take_loading(&self, lifetime: u64) -> Option<TakenLoading> {
        self.loading_slot().take(lifetime)
    }

    fn producer(&self) -> std::sync::MutexGuard<'_, Option<LifetimeState>> {
        self.producer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Applies `input` to lifetime `lifetime`'s state and publishes it,
    /// with `page`, through the slot; nothing for a lifetime not begun or
    /// ended. Whether the slot took it.
    fn report(&self, lifetime: u64, input: LoadInput, page: Option<MenuPage>) -> bool {
        let update = {
            let mut producer = self.producer();
            let Some(state) = producer.as_mut().filter(|s| s.lifetime() == lifetime) else {
                return false;
            };
            Box::new(state.apply(input))
        };
        self.loading_slot().offer(lifetime, page, Some(update))
    }

    /// Whether loading lifetime `lifetime` is still the current one.
    fn loading_current(&self, lifetime: u64) -> bool {
        self.loading_slot().lifetime == lifetime
    }

    /// Ends the current loading lifetime: its waiting lines and state are
    /// dropped, and its sinks, signals and frames do nothing from here.
    fn end_lifetime(&self) {
        self.loading_slot().end();
        drop(self.producer().take());
    }
}

/// Where the loading state is shown from, on any thread (the game thread
/// and the decode pool): the newest page replaces one the render thread
/// has not taken yet, and a signal is sent only when none waits, so at most
/// one page and one signal wait whatever the render thread's pace. A sink
/// belongs to the loading lifetime current when it was made, and shows
/// nothing once that lifetime has ended.
#[derive(Clone)]
pub struct LoadingSink {
    tx: Sender<RenderMsg>,
    shared: Arc<Shared>,
    lifetime: u64,
    /// The loading clock, for a test's lifetime begun by its first page.
    #[cfg(test)]
    clock: QpcClock,
}

impl LoadingSink {
    /// Shows `page` as the loading state, if the sink's lifetime is still
    /// the current one, as the stage of reading the chart; a lifetime not
    /// begun yet begins now, for an untitled chart.
    #[cfg(test)]
    pub fn show(&self, page: MenuPage) {
        {
            let mut producer = self.shared.producer();
            if producer
                .as_ref()
                .is_none_or(|s| s.lifetime() != self.lifetime)
                && self.shared.loading_current(self.lifetime)
            {
                *producer = Some(LifetimeState::new(
                    self.lifetime,
                    self.clock.now(),
                    &LoadingText::default(),
                    None,
                ));
            }
        }
        self.report(LoadInput::Stage(Stage::Chart), Some(page));
    }

    /// Applies `input` to the sink's lifetime's state and shows the whole
    /// state, with `page` when the lines changed, if the lifetime has
    /// begun and is still the current one: a newer state replaces one
    /// waiting, and the two replace a waiting pair together.
    pub fn report(&self, input: LoadInput, page: Option<MenuPage>) {
        if self.shared.report(self.lifetime, input, page) {
            self.signal();
        }
    }

    /// Puts `page` and `update` in the slot as published, if the
    /// lifetime is still the current one and `update` is newer than every
    /// state the slot accepted in it; whether they were put.
    #[cfg(test)]
    fn offer(&self, page: Option<MenuPage>, update: LoadingUpdate) -> bool {
        let put = self
            .shared
            .loading_slot()
            .offer(self.lifetime, page, Some(Box::new(update)));
        if put {
            self.signal();
        }
        put
    }

    /// Signals the render thread, unless a signal waits.
    fn signal(&self) {
        let mut slot = self.shared.loading_slot();
        if slot.lifetime == self.lifetime && !slot.signaled && !slot.held {
            slot.signaled = true;
            // A thread that already ended ignores it.
            let _ = self.tx.send(RenderMsg::Loading(self.lifetime));
        }
    }
}

/// How the render thread ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RenderEnd {
    /// It ended and was joined.
    Joined,
    /// It ended by a panic.
    Panicked,
    /// It did not end in time and was left to the process's exit.
    Stuck,
}

/// How long a test hook waits on its other side: `TAPESTRY_TEST_WATCHDOG`
/// seconds, else 60 (tests).
#[cfg(test)]
fn hook_bound() -> Duration {
    Duration::from_secs(
        std::env::var("TAPESTRY_TEST_WATCHDOG")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(60),
    )
}

/// The game thread's end of the render thread.
/// Sends chart publications to the render thread from any thread
/// ([`RenderLink::chart_publisher`]).
#[derive(Clone)]
pub struct ChartPublisher {
    tx: Sender<RenderMsg>,
}

impl ChartPublisher {
    /// Asks the render thread to publish `set` (absent when `None`) of
    /// `generation` for the attempt of `request`, answering on `reply`.
    pub fn publish(
        &self,
        generation: u64,
        request: Option<u64>,
        set: Option<Arc<crate::chart_images::ChartImages>>,
        reply: mpsc::SyncSender<crate::chart_images::Ack>,
    ) {
        let _ = self.tx.send(RenderMsg::ChartImages {
            generation,
            request,
            set,
            reply,
        });
    }
}

pub struct RenderLink {
    tx: Sender<RenderMsg>,
    shared: Arc<Shared>,
    handle: JoinHandle<()>,
    /// Disconnects when the thread ends, however it ends.
    ended: Receiver<()>,
    /// How long a preparation or a chart's load is waited for.
    answer_timeout: Duration,
    /// How long the end of the thread is waited for.
    exit_timeout: Duration,
    /// How long a play's figures are waited for.
    finish_timeout: Duration,
    /// The number of the last end page shown.
    end_pages: AtomicU64,
    /// The number of the last attempt's preparation requested.
    requests: AtomicU64,
    /// The loading clock, the render thread's: a loading lifetime's
    /// origin is stamped on it.
    clock: QpcClock,
    /// The screens' runtimes, shared with the render thread
    /// (design.md "beatoraja Lua Skins", Ownership).
    scripts: Scripts,
    /// The result lifetimes begun.
    result_lifetimes: AtomicU64,
}

impl RenderLink {
    /// The screens' runtimes' cell.
    pub(crate) fn scripts(&self) -> &Scripts {
        &self.scripts
    }

    /// Asks the render thread to park every runtime a screen holds and
    /// waits at most the answer's bound: before recording opens
    /// (design.md "beatoraja Lua Skins", Quiescence acknowledged).
    /// Whether it answered; a thread that ended holds none.
    pub fn park_scripts(&self) -> bool {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::ParkScripts(tx));
        rx.recv_timeout(self.answer_timeout).is_ok()
    }

    /// What the render thread's whole-frame records hold (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn frames_probe(&self) -> crate::dev::FramesProbe {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::FramesProbe(tx));
        rx.recv_timeout(self.answer_timeout).unwrap_or_default()
    }

    /// Sends a message; a thread that already ended ignores it.
    pub fn send(&self, msg: RenderMsg) {
        let _ = self.tx.send(msg);
    }

    /// Makes the render thread follow the window's size in `extent`: the
    /// newest size published, and every later one, is applied to the
    /// screen between frames, and to each screen built again. The slot
    /// wakes one render thread, this link's from now on: a link that
    /// followed it before gets no later wake.
    pub fn follow(&self, extent: &Arc<Extent>) {
        // Held through the `Follow`, so no `Resize` overtakes it, and a
        // flag an earlier registration left set is cleared ([`Extent`]'s
        // invariant).
        let mut wake = extent.wake.lock().unwrap_or_else(PoisonError::into_inner);
        *wake = Some(self.tx.clone());
        extent.queued.store(false, Ordering::Release);
        self.send(RenderMsg::Follow(Arc::clone(extent)));
    }

    /// Whether the render thread answered a fence within `deadline`
    /// (tests): it took every message sent before it, and drew the frame
    /// that followed them, if any ([`RenderMsg::Fence`]).
    #[cfg(test)]
    pub(crate) fn fence(&self, deadline: Duration) -> bool {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::Fence(tx));
        rx.recv_timeout(deadline).is_ok()
    }

    /// The mode the render thread draws once it took every message sent
    /// before this ([`RenderMsg::ModeProbe`]; tests).
    #[cfg(test)]
    pub(crate) fn mode(&self) -> String {
        let (tx, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::ModeProbe(tx));
        rx.recv_timeout(Duration::from_secs(60)).unwrap_or_default()
    }

    /// Makes the render thread read the launch screen's time as `elapsed`
    /// from now on, or from its clock again with `None` (tests).
    #[cfg(test)]
    pub(crate) fn set_launch_elapsed(&self, elapsed: Option<Duration>) {
        *self
            .shared
            .launch_elapsed
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = elapsed;
    }

    /// Selection lifetime `lifetime` begins: updates of it are taken from
    /// now on, and nothing of an earlier one is kept.
    pub fn enter_selection(&self, lifetime: u64) {
        self.shared.selection_slot().enter(lifetime);
    }

    /// Selection is left (a choice, the practice page): its lifetime ends
    /// and the slot empties, so no update of it is drawn, one the render
    /// thread already took included.
    pub fn leave_selection(&self) {
        self.shared.selection_slot().leave();
    }

    /// Shows the select screen's `update`: it replaces one the render
    /// thread has not taken, and a signal goes only when none waits, so at
    /// most one update and one message wait whatever the rate. An update
    /// of a lifetime not current is dropped.
    pub fn show_selection(&self, update: SelectionUpdate) {
        // Signalled under the lock, so the render thread's take, which
        // clears the flag first, never misses it.
        let mut slot = self.shared.selection_slot();
        if slot.offer(update) {
            self.send(RenderMsg::SelectionChanged);
        }
    }

    /// Why the selection skin was refused at runtime, once: the select
    /// screen's notice says it from the next update.
    pub fn take_selection_refusal(&self) -> Option<String> {
        self.shared
            .selection_refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// A result lifetime begins (the result shown first after its
    /// preparation): its number. Presentations of it are taken from now
    /// on, and nothing of an earlier one is kept.
    pub fn begin_result(&self) -> u64 {
        let lifetime = self.result_lifetimes.fetch_add(1, Ordering::Relaxed) + 1;
        self.shared.result_slot().enter(lifetime);
        self.scripts.enter_result(lifetime);
        *self.shared.barrier() = ResultBarrier {
            lifetime,
            ..ResultBarrier::default()
        };
        lifetime
    }

    /// The result is left: its lifetime ends and the slot empties, so no
    /// presentation of it is drawn, one the render thread took included.
    pub fn end_result(&self) {
        self.shared.result_slot().enter(0);
        self.scripts.leave_result();
    }

    /// The next request of the renderer's sequence, for a presentation's
    /// staging: a later attempt's or selection's supersedes it.
    pub fn next_request(&self) -> u64 {
        self.requests.fetch_add(1, Ordering::Relaxed) + 1
    }

    /// Shows the result's presentation `p`: it replaces one the render
    /// thread has not taken, and a signal goes only when none waits. One
    /// of a lifetime not current is dropped.
    pub fn show_result(&self, p: ResultPresentation) {
        let mut slot = self.shared.result_slot();
        if slot.offer(p) {
            self.send(RenderMsg::ResultChanged);
        }
    }

    /// Sets the barrier of the current result lifetime with the fade-out
    /// starting at `at` (the S10-09i design's D4): from the next commit
    /// point only the overlay merges. Taken under the lock the commit
    /// point holds, so the view returned is the one presented at that
    /// instant.
    ///
    /// With `gate` (a pressed command: the result's start), the view
    /// presented judges it first under the same lock: while its input time
    /// has not passed from `gate` at `at`, nothing is set and `None` is
    /// given (the S10-09i design's D6).
    pub fn pin_result(
        &self,
        at: TimeUs,
        gate: Option<TimeUs>,
    ) -> Option<(Option<ViewKey>, Option<skin_model::ResultTiming>)> {
        let lifetime = self.result_lifetimes.load(Ordering::Relaxed);
        let mut b = self.shared.barrier();
        if b.lifetime != lifetime {
            *b = ResultBarrier {
                lifetime,
                ..ResultBarrier::default()
            };
        }
        if game::gated(b.timing.map(crate::play::flow_timing), at, gate) == game::Outro::Gated {
            return None;
        }
        b.pinned = b.presented;
        b.fadeout = Some(at);
        Some((b.pinned, b.timing))
    }

    /// The timing of the current result lifetime's presented view, while
    /// its skin draws (the S10-09i design's D6): what the last commit point
    /// recorded.
    pub fn presented_timing(&self) -> Option<skin_model::ResultTiming> {
        let lifetime = self.result_lifetimes.load(Ordering::Relaxed);
        let b = self.shared.barrier();
        b.timing.filter(|_| b.lifetime == lifetime)
    }

    /// What the render thread shows of the result kind, and the run's
    /// register.
    pub fn result_status(&self) -> ResultReport {
        self.shared.result_status().clone()
    }

    /// Starts the result's upload after an attempt's commit (the S10-09i
    /// design's D2): a bundle's installation, or a prepared atlas's
    /// pages-only installation, as a selection of the same sequence, so
    /// it is withdrawn when dropped unanswered.
    pub fn upload_result(&self, upload: crate::result_skin::Upload) -> PendingSelect {
        let skin = match upload {
            crate::result_skin::Upload::Bundle(b) => Selected::Skin(Some(b), ScriptSlot::default()),
            crate::result_skin::Upload::Pages(p) => Selected::Pages(p),
        };
        self.selecting(ScreenKind::Result, skin)
    }

    /// Shows the end screen's `page`: it replaces a page the render thread
    /// has not taken yet, which is dropped here, so at most one waits
    /// whatever the render thread's pace.
    pub fn show_end(&self, page: EndPage) {
        let n = self.end_pages.fetch_add(1, Ordering::Relaxed) + 1;
        drop(self.shared.put_end(n, page));
        self.send(RenderMsg::End);
    }

    /// The end screen is left (a retry, selection, the practice page): a
    /// page still waiting is dropped, and no page shown so far is drawn
    /// again, by a signal still pending or by a frame of the page drawn
    /// last.
    pub fn clear_end(&self) {
        let upto = self.end_pages.load(Ordering::Relaxed);
        drop(self.shared.clear_end(upto));
    }

    /// How many end pages were shown so far.
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn end_pages_shown(&self) -> u64 {
        self.end_pages.load(Ordering::Relaxed)
    }

    /// Whether the end page shown last has been presented, and the frame
    /// sink has seen it (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub fn end_page_drawn(&self) -> bool {
        self.shared.end_drawn.load(Ordering::Acquire) >= self.end_pages.load(Ordering::Relaxed)
    }

    /// How many select screen frames were presented so far, each after
    /// the frame sink saw it (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub fn selection_drawn(&self) -> u64 {
        self.shared.selection_drawn.load(Ordering::Acquire)
    }

    /// Waits at most `timeout` until the end page shown last has been
    /// presented, and the frame sink has seen it (dev-tools: a script's
    /// `drawn` step). Whether it was.
    #[cfg(feature = "dev-tools")]
    pub fn wait_end_drawn(&self, timeout: Duration) -> bool {
        let deadline = std::time::Instant::now() + timeout;
        loop {
            if self.end_page_drawn() {
                return true;
            }
            if std::time::Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(2));
        }
    }

    /// Makes sure the screen can draw, built again if it failed, and
    /// waits until it can.
    ///
    /// # Errors
    ///
    /// Why the screen cannot be built.
    pub fn prepare(&self) -> Result<(), ScreenError> {
        let pending = self.begin_prepare();
        loop {
            if let Some(answered) = pending.poll(Some(pending.bound), &self.shared) {
                return answered;
            }
        }
    }

    /// [`prepare`](Self::prepare) begun, not waited for: the rebuild's
    /// request, its answer's bound fixed now, polled with
    /// [`serve_recovery`](Self::serve_recovery). Dropped, its late answer goes
    /// nowhere; the render thread takes the rebuild before any later
    /// request all the same.
    pub fn begin_prepare(&self) -> PendingPrepare {
        begin_prepare(&self.tx, self.answer_timeout)
    }

    /// Whether the screen needs its rebuild served: the device or the
    /// surface lost, or the rebuild's holder ([`serve_recovery`]) holds one
    /// under way or a failure no one reported. The one answer to "is the
    /// screen to be built again" for every decision of the flow, so a
    /// failure saved after the device came back is still served.
    pub fn screen_outstanding(&self) -> bool {
        self.failed()
            || !matches!(
                *self
                    .shared
                    .recovery
                    .lock()
                    .unwrap_or_else(PoisonError::into_inner),
                Recovery::Idle
            )
    }

    /// The screen's rebuild after a loss, served through its one owner
    /// ([`serve_recovery`]), waiting until `until` at most (none: not at
    /// all); `report`: the caller ends the rebuild's story, so its answer,
    /// or a failure saved earlier, is handed to it and the slot is empty
    /// again. Every recovery and preparation of the screen comes here.
    pub fn serve_recovery(&self, until: Option<Instant>, report: bool) -> Recovered {
        serve_recovery(&self.shared, &self.tx, self.answer_timeout, until, report)
    }

    /// The rebuilds the render thread took (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub(crate) fn prepares(&self) -> u64 {
        self.shared.prepares.load(Ordering::Acquire)
    }

    /// The recovery slot's state, not taken (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub(crate) fn recovery_state(&self) -> &'static str {
        match &*self
            .shared
            .recovery
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            Recovery::Idle => "idle",
            Recovery::Pending(_) => "pending",
            Recovery::Failed(_) => "failed",
        }
    }

    /// What a look inside a rig's wait serves of the screen
    /// ([`RecoveryHandle::look`]), from any thread.
    pub fn recovery_handle(&self) -> RecoveryHandle {
        RecoveryHandle {
            tx: self.tx.clone(),
            shared: Arc::clone(&self.shared),
            timeout: self.answer_timeout,
        }
    }

    /// [`prepare_attempt_with`](Self::prepare_attempt_with) the skin in
    /// use (tests).
    ///
    /// # Errors
    ///
    /// As [`prepare_attempt_with`](Self::prepare_attempt_with).
    #[cfg(test)]
    pub fn prepare_attempt(
        &self,
        attempt: AttemptContext,
        cancelled: impl FnMut() -> bool,
    ) -> Result<Preparation, ScreenError> {
        self.prepare_attempt_with(attempt, AttemptSkin::InUse, cancelled)
    }

    /// Prepares the chart loaded last for `attempt` with play skin
    /// `skin`, as a new request that supersedes the ones before, and waits
    /// for the answer, at most the answer's bound, looking at `cancelled`
    /// every [`CANCEL_POLL`]: once it says so, the preparation is
    /// [`Preparation::Cancelled`] without waiting further. A cancelled or
    /// timed out request is discarded, so its answer, however late,
    /// installs nothing. A pending play skin's specialization is staged
    /// with the attempt, and only its commit installs it.
    ///
    /// # Errors
    ///
    /// Why the screen or its device cannot draw at all,
    /// [`ScreenError::NoAnswer`] past the bound and [`ScreenError::Ended`]
    /// when the render thread is gone.
    #[cfg(test)]
    pub fn prepare_attempt_with(
        &self,
        attempt: AttemptContext,
        skin: AttemptSkin,
        mut cancelled: impl FnMut() -> bool,
    ) -> Result<Preparation, ScreenError> {
        let mut pending = self.begin_attempt(attempt, skin);
        loop {
            if let Some(answer) = pending.wait_until(None, &mut cancelled) {
                return answer.map(|a| a.preparation);
            }
        }
    }

    /// Asks the render thread to prepare the chart loaded last for
    /// `attempt` with play skin `skin`, as a new request that supersedes
    /// the ones before, and returns at once: the answer is waited for
    /// ([`PendingAttempt::wait_until`]) within the answer's bound from now.
    /// Dropped unanswered, the request is discarded, so its answer,
    /// however late, installs nothing.
    pub fn begin_attempt(&self, attempt: AttemptContext, skin: AttemptSkin) -> PendingAttempt {
        let request = self.requests.fetch_add(1, Ordering::Relaxed) + 1;
        let (reply, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::PrepareAttempt {
            request,
            attempt: Box::new(attempt),
            skin,
            reply,
        });
        PendingAttempt {
            request,
            rx,
            bound: Instant::now() + self.answer_timeout,
            tx: self.tx.clone(),
            shared: Arc::clone(&self.shared),
            open: true,
        }
    }

    /// The final readiness of the attempt request `request`
    /// ([`RenderMsg::Ready`]), with the adjustment epoch `epoch`, waited
    /// for at most the answer's bound: it prepares nothing.
    ///
    /// # Errors
    ///
    /// [`ScreenError::NoAnswer`] past the bound and [`ScreenError::Ended`]
    /// when the render thread is gone.
    /// Asks the render thread to publish the chart's images `set` (absent
    /// when `None`) of `generation` for the attempt of `request`,
    /// answering on the receiver returned, which the caller polls
    /// ([`RenderMsg::ChartImages`]).
    pub fn publish_chart_images(
        &self,
        generation: u64,
        request: Option<u64>,
        set: Option<Arc<crate::chart_images::ChartImages>>,
    ) -> mpsc::Receiver<crate::chart_images::Ack> {
        let (reply, rx) = mpsc::sync_channel(1);
        self.chart_publisher()
            .publish(generation, request, set, reply);
        rx
    }

    /// What publishes the chart's images from another thread: the job
    /// that read them.
    #[must_use]
    pub fn chart_publisher(&self) -> ChartPublisher {
        ChartPublisher {
            tx: self.tx.clone(),
        }
    }

    /// Where the chart images' generation is written, for the worker
    /// ([`crate::chart_images::ChartImageWorker::new`]).
    #[must_use]
    pub fn chart_generation(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.shared.chart_generation)
    }

    /// The present an acknowledgment of `generation`'s images is checked
    /// against ([`crate::chart_images::PublicationKey::current`]): the
    /// live targets the render thread mirrored last.
    #[must_use]
    pub fn chart_now(&self, generation: u64) -> crate::chart_images::ChartNow {
        let (staged, committed) = *self
            .shared
            .chart_targets
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        crate::chart_images::ChartNow {
            generation,
            staged,
            committed,
        }
    }

    /// How long an answer of the render thread is waited for.
    #[must_use]
    pub const fn answer_bound(&self) -> Duration {
        self.answer_timeout
    }

    /// Holds every chart publication before it publishes while `on`, at
    /// most [`hook_bound`] each (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn hold_chart_publications(&self, on: bool) {
        self.shared.chart_hold.store(on, Ordering::SeqCst);
    }

    pub fn ready_attempt(&self, request: u64, epoch: u64) -> Result<ReadyAnswer, ScreenError> {
        let (reply, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::Ready {
            request,
            epoch,
            reply,
        });
        match rx.recv_timeout(self.answer_timeout) {
            Ok(answer) => Ok(answer),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(ScreenError::NoAnswer),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(self.ended()),
        }
    }

    /// Installs what the last attempt's preparation staged, and waits
    /// until it is, at most the answer's bound. Past the bound the commit
    /// is withdrawn: still waiting in the channel, it installs nothing,
    /// and one installed meanwhile is answered as installed.
    ///
    /// # Errors
    ///
    /// [`ScreenError::Lost`] when it was not installed: the request was
    /// discarded or superseded, or the screen was built again since it
    /// was prepared; [`ScreenError::NoAnswer`] past the bound and
    /// [`ScreenError::Ended`] when the render thread is gone.
    ///
    /// Installed, what the play skin declares ([`Declared`]), from the
    /// receipt taken with the installation, on the answer and past the
    /// bound alike.
    pub fn commit_attempt(&self) -> Result<Declared, ScreenError> {
        let request = self.requests.load(Ordering::Relaxed);
        let (reply, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::Commit { request, reply });
        match rx.recv_timeout(self.answer_timeout) {
            Ok(Ok(declared)) => Ok(declared),
            Ok(Err(e)) => {
                eprintln!("render: the attempt was not installed: {e}");
                Err(ScreenError::Lost)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                #[cfg(test)]
                {
                    self.shared.commit_timed_out.store(true, Ordering::Release);
                    // A test holding the render thread between the receipt
                    // and the declaration reads the receipt only then; a
                    // render thread that never gets there fails the test.
                    let deadline = Instant::now() + hook_bound();
                    while self.shared.hold_declaration.load(Ordering::Acquire)
                        && !self.shared.declaration_held.load(Ordering::Acquire)
                    {
                        assert!(
                            Instant::now() < deadline,
                            "the render thread never held its declaration"
                        );
                        std::thread::sleep(Duration::from_millis(1));
                    }
                }
                let mut commits = self.shared.commits();
                if commits.committed >= request {
                    let r = commits.receipt;
                    drop(commits);
                    if r.request != request {
                        return Ok(Declared::default());
                    }
                    // Declared before the opener reads the minimum again,
                    // outside the commits' lock, whether or not the
                    // render thread has declared it yet.
                    if let Some(loadend) = r.loadend {
                        self.shared.committed_loading(loadend);
                    }
                    return Ok(r.declared);
                }
                commits.withdrawn = commits.withdrawn.max(request);
                Err(ScreenError::NoAnswer)
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(self.ended()),
        }
    }

    /// Hands the render thread the settings keys' values, which every
    /// selection frame reads the volume and the display offset from.
    pub fn tuning(&self, tuning: Arc<LiveTuning>) {
        self.send(RenderMsg::Tuning(tuning));
    }

    /// Hands the render thread the adjustments' snapshots, which every
    /// play and loading frame is drawn from from now on.
    pub fn presentation(&self, reader: game::adjustment::PresentationReader) {
        self.send(RenderMsg::Presentation(Box::new(reader)));
    }

    /// The game created play state `state`: its turntable clock starts at
    /// its presentation's first frame.
    pub fn play_state(&self, state: game::PlayState) {
        self.send(RenderMsg::PlayState(state));
    }

    /// The committed attempt that started ended with the final tally
    /// `tally` ([`RenderMsg::Settled`]).
    pub fn settled(&self, tally: eval::Tally) {
        self.send(RenderMsg::Settled(Box::new(tally)));
    }

    /// Play state `number`'s turntable clock pauses (`pause`) or resumes.
    pub fn turntable(&self, number: u64, pause: bool) {
        self.send(RenderMsg::Turntable { number, pause });
    }

    /// Drops what the last attempt's preparation staged, if it was not
    /// committed; it never installs.
    pub fn discard_attempt(&self) {
        self.send(RenderMsg::Discard(self.requests.load(Ordering::Relaxed)));
    }

    /// Starts installing the declarative `skin` for screen `kind`, as a
    /// request of the same sequence as the attempts' preparations: for
    /// play, every later attempt prepares with it, or with the skin in
    /// use for `None`; for another kind, `None` removes its installation,
    /// so its screen draws its built-in presentation. The game thread
    /// goes on and polls the answer ([`PendingSelect::poll`]); a skin past
    /// the texels the screens share is refused with the reason.
    /// Startup selects each configured kind's skin
    /// ([`crate::imports::startup`]).
    pub fn select(&self, kind: ScreenKind, skin: Option<Arc<SkinBundle>>) -> PendingSelect {
        self.select_with(kind, skin, ScriptSlot::default())
    }

    /// [`select`](Self::select), with the runtime `script` the skin's
    /// import kept for its screen, parked at the installation's commit
    /// and retired if it does not commit.
    pub(crate) fn select_with(
        &self,
        kind: ScreenKind,
        skin: Option<Arc<SkinBundle>>,
        script: ScriptSlot,
    ) -> PendingSelect {
        self.selecting(kind, Selected::Skin(skin, script))
    }

    /// Marks screen `kind`'s skin pending, as a selection is made
    /// ([`select`](Self::select)), answered and withdrawn: committed, no
    /// installation of it is kept and no attempt is prepared without a
    /// specialization ([`AttemptSkin::Specialized`]), whose commit
    /// installs it; the loading state shows the load's lines on a cleared
    /// canvas until one is committed in its lifetime. Only play's skin
    /// can be pending; another kind is refused. Startup marks a pending
    /// play skin ([`crate::imports::startup`]).
    pub fn pend(&self, kind: ScreenKind) -> PendingSelect {
        self.selecting(kind, Selected::Pending)
    }

    /// Sends the selection of `skin` for `kind` as the next request.
    fn selecting(&self, kind: ScreenKind, skin: Selected) -> PendingSelect {
        let request = self.requests.fetch_add(1, Ordering::Relaxed) + 1;
        let (reply, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::Select {
            request,
            kind,
            skin,
            reply,
        });
        PendingSelect {
            request,
            rx,
            deadline: Instant::now() + self.answer_timeout,
            shared: Arc::clone(&self.shared),
            settled: false,
        }
    }

    /// The atlas limits of the screen built last and its target size:
    /// what a skin is packed for; `None` before a screen that installs
    /// declarative skins was built.
    pub fn atlas_limits(&self) -> Option<(skin_model::AtlasLimits, [u32; 2])> {
        *self
            .shared
            .packing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Prepares `chart` for every play from now on, and waits until it is.
    ///
    /// # Errors
    ///
    /// As [`PendingLoad::wait_serving`].
    #[cfg(test)]
    pub fn load(&self, chart: Arc<Chart>, section: Option<Section>) -> Result<(), ScreenError> {
        self.begin_load(chart, section).wait_serving(self, || false)
    }

    /// Starts preparing `chart` for every play from now on; the game
    /// thread goes on with the rest of the load and waits for the answer
    /// later ([`PendingLoad::wait_serving`]). A pending load dropped unanswered
    /// leaves the screen with the chart, which the next load replaces.
    pub fn begin_load(&self, chart: Arc<Chart>, section: Option<Section>) -> PendingLoad {
        let (reply, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::Load {
            chart,
            section,
            reply,
        });
        PendingLoad(rx)
    }

    /// Loading frames presented so far (dev-tools).
    #[cfg(feature = "dev-tools")]
    pub fn loading_drawn(&self) -> u64 {
        self.shared.loading_drawn.load(Ordering::Relaxed)
    }

    /// The current loading lifetime: after [`begin_loading`](Self::begin_loading),
    /// the one it began.
    pub fn loading_lifetime(&self) -> u64 {
        self.shared.loading_slot().lifetime
    }

    /// The latest loading lifetime whose first frame the render thread
    /// presented, and when on the clock; `None` before any.
    pub fn loading_ack(&self) -> Option<(u64, TimeUs)> {
        self.shared.loading_ack().first
    }

    /// The loading time the play skin declares, as the last presented
    /// loading frame drew with it ([`LoadingAck`]).
    pub fn loading_loadend(&self) -> Option<TimeUs> {
        self.shared.loading_ack().loadend
    }

    /// Why the skin refused a loading frame of lifetime `lifetime`, if it
    /// did: nothing of that lifetime is drawn again, so its first frame
    /// may never be acknowledged.
    pub fn loading_refused(&self, lifetime: u64) -> Option<BuildError> {
        self.shared
            .loading_ack()
            .refused
            .and_then(|(l, why)| (l == lifetime).then_some(why))
    }

    /// A handle on the loading acknowledgement's test hold, for another
    /// thread (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn ack_hold(&self) -> AckHold {
        AckHold(Arc::clone(&self.shared))
    }

    /// Where the end or pause of a loading lifetime waits before its
    /// answer while a test holds it (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn loading_end_gate(&self) -> Arc<TestGate> {
        Arc::clone(&self.shared.loading_end_gate)
    }

    /// The presentation epoch of the attempt staged last as it was
    /// prepared, and of the attempt committed last as it committed (its
    /// final readiness's stamp); 0 before either (tests).
    #[cfg(test)]
    pub fn epochs(&self) -> (u64, u64) {
        let [staged, committed] = &self.shared.epochs;
        (
            staged.load(Ordering::SeqCst),
            committed.load(Ordering::SeqCst),
        )
    }

    /// Holds every presented loading frame's acknowledgement from now on
    /// while `held`, or lets it go (tests).
    #[cfg(test)]
    pub fn hold_loading_ack(&self, held: bool) {
        self.shared.loading_ack_hold[0].store(held, Ordering::SeqCst);
    }

    /// Whether a presented loading frame's acknowledgement is held (tests).
    #[cfg(test)]
    pub fn loading_ack_held(&self) -> bool {
        self.shared.loading_ack_hold[1].load(Ordering::SeqCst)
    }

    /// Lets one held acknowledgement through (tests).
    #[cfg(test)]
    pub fn pass_loading_ack(&self) {
        self.shared.loading_ack_pass.fetch_add(1, Ordering::SeqCst);
    }

    /// Presented loading frames acknowledged so far (tests).
    #[cfg(test)]
    pub fn loading_acked(&self) -> u64 {
        self.shared.loading_acked.load(Ordering::SeqCst)
    }

    /// Where the current loading lifetime's lines and state are shown
    /// from, on any thread.
    pub fn loading(&self) -> LoadingSink {
        LoadingSink {
            tx: self.tx.clone(),
            shared: Arc::clone(&self.shared),
            lifetime: self.shared.loading_slot().lifetime,
            #[cfg(test)]
            clock: self.clock,
        }
    }

    /// A loading lifetime begins (a chart chosen to play, a practice
    /// Start): its origin is stamped now on the loading clock, once, and
    /// its state starts for the chart and preset `text` names with the
    /// shown gauge's descriptor `gauge`. A lifetime begun before and not
    /// ended ends first, so each beginning has a lifetime of its own.
    /// Its lines and state are then shown through [`loading`](Self::loading).
    pub fn begin_loading(&self, text: &LoadingText, gauge: Option<skin_model::LoadingGauge>) {
        self.begin_lifetime(text, gauge, false);
    }

    /// [`begin_loading`](Self::begin_loading), held: made at a choice, its
    /// reports kept and nothing drawn until
    /// [`reveal_loading`](Self::reveal_loading) at the decision screen's
    /// deadline (design.md "Play Lifecycle", the decision screen).
    pub fn begin_loading_held(&self, text: &LoadingText, gauge: Option<skin_model::LoadingGauge>) {
        self.begin_lifetime(text, gauge, true);
    }

    fn begin_lifetime(
        &self,
        text: &LoadingText,
        gauge: Option<skin_model::LoadingGauge>,
        held: bool,
    ) {
        let mut producer = self.shared.producer();
        let mut slot = self.shared.loading_slot();
        if producer
            .as_ref()
            .is_some_and(|s| s.lifetime() == slot.lifetime)
        {
            slot.end();
        }
        slot.held = held;
        *producer = Some(LifetimeState::new(
            slot.lifetime,
            self.clock.now(),
            text,
            gauge,
        ));
    }

    /// The held loading lifetime is shown from now: its state published
    /// again from a new origin on the loading clock, so its timers start
    /// here, and the render thread signalled. Waits for nothing. Nothing
    /// for a lifetime not held.
    pub fn reveal_loading(&self) {
        reveal(&self.shared, self.clock, &self.tx);
    }

    /// [`reveal_loading`](Self::reveal_loading), from any thread: what a
    /// wait inside a chart's load runs at the decision screen's deadline.
    pub fn reveal_handle(&self) -> impl Fn() + Send + Sync + 'static {
        let (shared, clock, tx) = (Arc::clone(&self.shared), self.clock, self.tx.clone());
        move || reveal(&shared, clock, &tx)
    }

    /// The decision screen drawn until the next screen: its built-in
    /// presentation `page`, or the decision skin from the label slot
    /// `shown` pinned, `page` drawn when the skin cannot draw; kept
    /// through the chart's load and the screen's recovery, and not a
    /// loading lifetime.
    pub fn show_decision(
        &self,
        page: MenuPage,
        shown: Option<crate::decision_side::ShownDecision>,
    ) {
        #[cfg(test)]
        self.shared.decisions_sent.fetch_add(1, Ordering::SeqCst);
        self.send(RenderMsg::Decision {
            page: Box::new(page),
            shown,
        });
    }

    /// The decision labels' slot states, shared with the game thread,
    /// which pins one at the choice (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn label_book(&self) -> Arc<Mutex<render::LabelBook>> {
        Arc::clone(&self.shared.labels)
    }

    /// The decision skin of the active lifetime could not draw: its
    /// takeover, once ([`game::Decision::apply_takeover`]).
    pub fn take_decision_takeover(&self) -> Option<game::DecisionTakeover> {
        self.shared
            .decision_takeover
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// Why the decision skin was refused for the run, once: the select
    /// screen's notice says it from the next update.
    pub fn take_decision_refusal(&self) -> Option<String> {
        self.shared
            .decision_refused
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// What wakes the game thread when the decision skin's takeover is
    /// produced ([`crate::controls::Controls::wake_game`]).
    pub fn set_game_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        *self
            .shared
            .game_wake
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(wake);
    }

    /// How many decision presentations were sent, readable from another
    /// thread (tests).
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn decisions_sent(&self) -> impl Fn() -> u64 + Send + 'static {
        let shared = Arc::clone(&self.shared);
        move || shared.decisions_sent.load(Ordering::SeqCst)
    }

    /// Loading ended (selection, the practice page): its lifetime ends, so
    /// lines still waiting are dropped, a signal still pending takes
    /// nothing, and the render thread draws no further loading frame of it,
    /// checked after each frame's wait. The next lines shown begin a new
    /// lifetime's.
    pub fn clear_loading(&self) {
        self.shared.end_lifetime();
    }

    /// Loading ended before recording opens: [`clear_loading`](Self::clear_loading),
    /// then waits, at most [`LOAD_TIMEOUT`], until the render thread has
    /// answered that it draws no loading frame any more, so none is drawn
    /// after recording opens.
    ///
    /// # Errors
    ///
    /// [`ScreenError::NoAnswer`] after [`LOAD_TIMEOUT`],
    /// [`ScreenError::Ended`] when the render thread is gone,
    /// [`ScreenError::Lost`] when its screen failed after the attempt was
    /// prepared (a loading frame failed), and
    /// [`ScreenError::LoadingRefused`] when the skin refused a loading
    /// frame of the lifetime ended: recording must not open.
    pub fn end_loading(&self) -> Result<(), ScreenError> {
        let lifetime = self.shared.loading_slot().lifetime;
        self.clear_loading();
        let (reply, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::EndLoading { lifetime, reply });
        self.loading_end(&rx)
    }

    /// The current loading lifetime pauses before a READY candidate opens
    /// (design.md "Play Lifecycle", the start gate): as
    /// [`end_loading`](Self::end_loading), but the lifetime stays current,
    /// so its sinks still report into it and a deferred opening resumes it
    /// ([`resume_loading`](Self::resume_loading)); an opening ends it
    /// ([`clear_loading`](Self::clear_loading)). Without a loading state
    /// drawn there is nothing to pause, and the answer is as the end's.
    ///
    /// # Errors
    ///
    /// As [`end_loading`](Self::end_loading).
    pub fn pause_loading(&self) -> Result<(), ScreenError> {
        let lifetime = self.shared.loading_slot().lifetime;
        let (reply, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::PauseLoading { lifetime, reply });
        self.loading_end(&rx)
    }

    /// The paused loading lifetime, if still current, is drawn again as
    /// it was paused: the same lifetime, so a skin's loading state goes on
    /// rather than starting afresh, from the same origin on the loading
    /// clock, with the lines and state reported meanwhile. Nothing when
    /// no lifetime was paused or it has ended.
    pub fn resume_loading(&self) {
        let lifetime = self.shared.loading_slot().lifetime;
        self.send(RenderMsg::ResumeLoading(lifetime));
    }

    /// The render thread's answer to the end or pause of a loading
    /// lifetime, waited for at most [`LOAD_TIMEOUT`].
    fn loading_end(&self, rx: &Receiver<LoadingEnd>) -> Result<(), ScreenError> {
        match rx.recv_timeout(self.answer_timeout) {
            Ok(LoadingEnd::Ready) => Ok(()),
            Ok(LoadingEnd::Lost) => Err(ScreenError::Lost),
            Ok(LoadingEnd::Refused(e)) => Err(ScreenError::LoadingRefused(e)),
            Err(mpsc::RecvTimeoutError::Timeout) => Err(ScreenError::NoAnswer),
            Err(mpsc::RecvTimeoutError::Disconnected) => Err(self.ended()),
        }
    }

    /// The answer to a preparation or a load, waited for at most
    /// [`LOAD_TIMEOUT`]; a late answer goes nowhere.
    /// Whether a frame failed since the last preparation, or the render
    /// thread ended.
    pub fn failed(&self) -> bool {
        self.shared.failed.load(Ordering::Acquire)
    }

    /// A reply found disconnected: the render thread ended, which is the
    /// screen's failure, latched before the reply's error is returned.
    fn ended(&self) -> ScreenError {
        ended(&self.shared)
    }

    /// The figures of the play that ended at `end`; `None` when the render
    /// thread did not answer or had no play to measure.
    pub fn finish(&self, end: TimeUs) -> Option<Figures> {
        let (reply, rx) = mpsc::sync_channel(1);
        self.send(RenderMsg::Finish { end, reply });
        rx.recv_timeout(self.finish_timeout).ok().flatten()
    }

    /// When the current play's first frame was drawn.
    pub fn first_frame(&self) -> Option<TimeUs> {
        let t = self.shared.first_frame.load(Ordering::Acquire);
        (t != i64::MIN).then_some(TimeUs(t))
    }

    /// Ends the thread and waits for it at most [`EXIT_TIMEOUT`]; when it
    /// is [`RenderEnd::Stuck`], the window it draws into must stay open.
    pub fn exit(self) -> RenderEnd {
        self.send(RenderMsg::Exit);
        let end = match self.ended.recv_timeout(self.exit_timeout) {
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // Detached: a thread stuck in the screen's work must not
                // hold up the exit. The caller keeps the window open.
                eprintln!("render: the render thread did not end");
                RenderEnd::Stuck
            }
            _ => match self.handle.join() {
                Ok(()) => RenderEnd::Joined,
                Err(_) => RenderEnd::Panicked,
            },
        };
        // No play runs any more: the parked runtimes are destroyed here,
        // unless an interval was left open, when they are leaked.
        if let Some(token) = self.scripts.inactive() {
            self.scripts.retire(Quiet::Inactive(token));
        }
        end
    }

    /// Sets how long a preparation, a chart's load or a play's figures
    /// and the end of the thread are waited for, in place of
    /// [`LOAD_TIMEOUT`], [`FINISH_TIMEOUT`] and [`EXIT_TIMEOUT`] (tests,
    /// and `--dev-render-timeout`).
    #[cfg(any(test, feature = "dev-tools"))]
    pub fn set_timeouts(&mut self, answer: Duration, exit: Duration) {
        self.answer_timeout = answer;
        self.finish_timeout = answer;
        self.exit_timeout = exit;
    }
}

impl crate::result_present::Publish for RenderLink {
    fn begin(&mut self) -> u64 {
        self.begin_result()
    }
    fn end(&mut self) {
        self.end_result();
    }
    fn request(&mut self) -> u64 {
        self.next_request()
    }
    fn show(&mut self, p: ResultPresentation) {
        self.show_result(p);
    }
    fn pin(
        &mut self,
        at: TimeUs,
        gate: Option<TimeUs>,
    ) -> Option<(Option<ViewKey>, Option<skin_model::ResultTiming>)> {
        self.pin_result(at, gate)
    }
    fn timing(&self) -> Option<skin_model::ResultTiming> {
        self.presented_timing()
    }
}

/// A chart's load the render thread was asked for ([`RenderLink::begin_load`]).
pub struct PendingLoad(Receiver<Result<(), RenderError>>);

impl PendingLoad {
    /// A load answered on `rx`, in place of the render thread's (tests).
    #[cfg(test)]
    pub(crate) const fn answered_on(rx: Receiver<Result<(), RenderError>>) -> Self {
        Self(rx)
    }

    /// Waits for the answer, at most the answer's bound from here, calling
    /// `serve` every [`CANCEL_POLL`] at most meanwhile (`true` stops the
    /// wait, `Ok` with no answer taken): the game thread
    /// serves the adjustments through the chart's load on the render
    /// thread.
    ///
    /// # Errors
    ///
    /// Why the screen cannot be built or cannot draw the chart, and
    /// [`ScreenError::NoAnswer`] past the bound; a late answer goes nowhere.
    pub fn wait_serving(
        self,
        link: &RenderLink,
        mut serve: impl FnMut() -> bool,
    ) -> Result<(), ScreenError> {
        let deadline = Instant::now() + link.answer_timeout;
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match self.0.recv_timeout(left.min(CANCEL_POLL)) {
                Ok(ready) => return Ok(ready?),
                Err(mpsc::RecvTimeoutError::Disconnected) => return Err(link.ended()),
                Err(mpsc::RecvTimeoutError::Timeout) if left.is_zero() => {
                    return Err(ScreenError::NoAnswer);
                }
                // The caller stopped the wait: its answer goes nowhere.
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    if serve() {
                        return Ok(());
                    }
                }
            }
        }
    }
}

/// The held loading lifetime revealed ([`RenderLink::reveal_loading`]).
fn reveal(shared: &Shared, clock: QpcClock, tx: &Sender<RenderMsg>) {
    let mut producer = shared.producer();
    let mut slot = shared.loading_slot();
    if !std::mem::take(&mut slot.held) {
        return;
    }
    let lifetime = slot.lifetime;
    if let Some(state) = producer.as_mut().filter(|s| s.lifetime() == lifetime) {
        let update = state.restart(clock.now());
        slot.offer(lifetime, None, Some(Box::new(update)));
    }
    if !slot.signaled {
        slot.signaled = true;
        // A thread that already ended ignores it.
        let _ = tx.send(RenderMsg::Loading(lifetime));
    }
}

/// An attempt's preparation asked of the render thread
/// ([`RenderLink::begin_attempt`]), its answer not yet taken.
pub struct PendingAttempt {
    request: u64,
    rx: Receiver<Result<Answered, RenderError>>,
    /// The answer's bound.
    bound: Instant,
    tx: Sender<RenderMsg>,
    /// Where a disconnected reply latches the screen's failure.
    shared: Arc<Shared>,
    /// Not answered, discarded or failed yet: dropped so, it is
    /// discarded.
    open: bool,
}

impl PendingAttempt {
    /// The request's number.
    pub const fn request(&self) -> u64 {
        self.request
    }

    /// Waits for the answer until `until` at most (none: the answer's
    /// bound), looking at `cancelled` every [`CANCEL_POLL`]. `None` at
    /// `until` with no answer, to be called again; once `cancelled` says
    /// so, [`Preparation::Cancelled`] without waiting further. A cancelled
    /// or timed-out request is discarded, so its answer, however late,
    /// installs nothing.
    ///
    /// # Errors
    ///
    /// Why the screen or its device cannot draw at all,
    /// [`ScreenError::NoAnswer`] past the answer's bound and
    /// [`ScreenError::Ended`] when the render thread is gone.
    pub fn wait_until(
        &mut self,
        until: Option<Instant>,
        mut cancelled: impl FnMut() -> bool,
    ) -> Option<Result<Answered, ScreenError>> {
        loop {
            let end = until.map_or(self.bound, |u| u.min(self.bound));
            let left = end.saturating_duration_since(Instant::now());
            match self.rx.recv_timeout(left.min(CANCEL_POLL)) {
                Ok(answer) => {
                    self.open = false;
                    return Some(answer.map_err(ScreenError::from));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    self.open = false;
                    return Some(Err(ended(&self.shared)));
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
            }
            if cancelled() {
                self.discard();
                return Some(Ok(Answered {
                    preparation: Preparation::Cancelled,
                    screen: game::ScreenKey::default(),
                }));
            }
            let now = Instant::now();
            if now >= self.bound {
                self.discard();
                return Some(Err(ScreenError::NoAnswer));
            }
            if until.is_some_and(|u| now >= u) {
                return None;
            }
        }
    }

    /// Drops the request on the render thread, unless already answered.
    fn discard(&mut self) {
        if self.open {
            self.open = false;
            let _ = self.tx.send(RenderMsg::Discard(self.request));
        }
    }
}

impl Drop for PendingAttempt {
    fn drop(&mut self) {
        self.discard();
    }
}

/// A skin's selection the render thread was asked for
/// ([`RenderLink::select`]), polled by the game thread in short slices so
/// it goes on with the menus meanwhile. Past the answer's bound, or
/// dropped unanswered (cancelled, abandoned), it is withdrawn before that
/// returns: a completion after that installs nothing.
#[derive(Debug)]
pub struct PendingSelect {
    request: u64,
    rx: Receiver<Result<Preparation, RenderError>>,
    deadline: Instant,
    shared: Arc<Shared>,
    /// Answered or withdrawn.
    settled: bool,
}

impl PendingSelect {
    /// Waits at most `slice` for the answer: `None` while it is still on
    /// its way. Ready means the skin is installed; a refusal keeps the
    /// installation in use. Past the answer's bound the selection is
    /// withdrawn, and answered ready only when it was installed before.
    ///
    /// # Errors
    ///
    /// Why the screen or its device cannot draw at all,
    /// [`ScreenError::NoAnswer`] past the bound and [`ScreenError::Ended`]
    /// when the render thread is gone.
    pub fn poll(&mut self, slice: Duration) -> Option<Result<Preparation, ScreenError>> {
        if self.settled {
            return None;
        }
        match self.rx.recv_timeout(slice) {
            Ok(answer) => {
                self.settled = true;
                Some(answer.map_err(ScreenError::from))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.settled = true;
                Some(Err(ended(&self.shared)))
            }
            Err(mpsc::RecvTimeoutError::Timeout) if Instant::now() >= self.deadline => {
                Some(if self.withdraw() {
                    Ok(Preparation::Ready)
                } else {
                    Err(ScreenError::NoAnswer)
                })
            }
            Err(mpsc::RecvTimeoutError::Timeout) => None,
        }
    }

    /// Waits for the answer, polling as the game thread does (tests).
    ///
    /// # Errors
    ///
    /// As [`poll`](Self::poll).
    #[cfg(test)]
    pub fn wait(mut self) -> Result<Preparation, ScreenError> {
        loop {
            if let Some(answer) = self.poll(CANCEL_POLL) {
                return answer;
            }
        }
    }

    /// Marks the request withdrawn unless it was committed: whether it
    /// was. Under the commits' lock, so the render thread either
    /// committed before or sees the withdrawal at the commit.
    fn withdraw(&mut self) -> bool {
        if std::mem::replace(&mut self.settled, true) {
            return false;
        }
        let mut commits = self.shared.commits();
        if commits.committed >= self.request {
            return true;
        }
        commits.withdrawn = commits.withdrawn.max(self.request);
        false
    }
}

impl Drop for PendingSelect {
    fn drop(&mut self) {
        self.withdraw();
    }
}

/// How long the game thread waits for a selection's answer at each look:
/// the menus, quit and cancellation are served between looks.
pub(crate) const SELECT_SLICE: Duration = Duration::from_millis(1);

/// The play skin's installation as the game thread sees it: a chart is
/// admitted (chosen to play, or a practice Start) only once the skin it
/// shows is installed or marked pending, so the loading state draws the
/// skin's own loading frame, or, while pending, the load's lines on a
/// cleared canvas until a specialization is committed in its lifetime.
/// Startup leaves it ready, a pending play skin's marker included, or
/// refused when the configured play skin was refused
/// ([`crate::imports::startup`]); without one the simple skin draws,
/// always ready.
#[derive(Debug, Default)]
pub enum SkinState {
    /// Installed or pending: charts are admitted.
    #[default]
    Ready,
    /// A selection on its way: no chart is admitted until it is
    /// installed.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "production selects no declarative skin yet")
    )]
    Selecting(PendingSelect),
    /// The selection was refused or failed, with its located reason: no
    /// chart is admitted, and no other skin stands in.
    Refused(String),
}

impl SkinState {
    /// Starts selecting `skin` on `render`; a selection still on its way
    /// is withdrawn first.
    #[cfg_attr(
        not(test),
        expect(dead_code, reason = "production selects no declarative skin yet")
    )]
    pub fn select(&mut self, render: &RenderLink, skin: Option<Arc<SkinBundle>>) {
        *self = Self::Ready;
        *self = Self::Selecting(render.select(ScreenKind::Play, skin));
    }

    /// Whether a chart may be admitted now, looking at a selection on its
    /// way for at most [`SELECT_SLICE`].
    ///
    /// # Errors
    ///
    /// The notice to show instead: the selection is on its way, or why
    /// it was refused.
    pub fn ready(&mut self) -> Result<(), String> {
        if let Self::Selecting(pending) = self {
            let Some(answer) = pending.poll(SELECT_SLICE) else {
                return Err(Label::SkinPreparing.text().to_owned());
            };
            *self = Self::answered(answer);
        }
        match self {
            Self::Refused(notice) => Err(notice.clone()),
            Self::Ready | Self::Selecting(_) => Ok(()),
        }
    }

    /// The state a play skin's selection leaves once `answer` came: ready,
    /// or refused with the notice, the render thread's reason carried as
    /// text only.
    pub fn answered(answer: Result<Preparation, ScreenError>) -> Self {
        match answer {
            Ok(Preparation::Ready) => Self::Ready,
            Ok(Preparation::Refused(why)) => {
                eprintln!("render: the play skin was refused: {why}");
                Self::Refused(
                    Message::SkinUnusable {
                        why: &why.to_string(),
                    }
                    .text(),
                )
            }
            Ok(Preparation::Cancelled) => Self::Refused(Label::SkinCancelled.text().to_owned()),
            Err(e) => {
                eprintln!("render: the play skin cannot be installed: {}", Chain(&e));
                Self::Refused(
                    Message::SkinCannotPrepare {
                        why: &Chain(&e).to_string(),
                    }
                    .text(),
                )
            }
        }
    }
}

/// Starts the render thread; `make` builds the screen on it, and the chart,
/// when there is one yet, is prepared before this returns, so every upload
/// is done before the first READY state (design.md "Display Latency and
/// Stutter"); a chart loaded later is prepared before its READY state too.
///
/// # Errors
///
/// Why the screen could not be built or could not draw the chart.
pub fn spawn(
    mut make: MakeScreen,
    chart: Option<Arc<Chart>>,
    section: Option<Section>,
    clock: QpcClock,
    view: View,
    measure: Measure,
    frames: Box<dyn FrameSink>,
) -> Result<RenderLink, ScreenError> {
    let (tx, rx) = mpsc::channel();
    let (ready_tx, ready_rx) = mpsc::sync_channel(1);
    let shared = Arc::new(Shared::new());
    let thread_shared = Arc::clone(&shared);
    let scripts = Scripts::new();
    let thread_scripts = scripts.clone();
    let (ended_tx, ended) = mpsc::sync_channel::<()>(0);
    let handle = std::thread::Builder::new()
        .name("render".into())
        .spawn(move || {
            // Dropped when the thread ends, however it ends.
            let _ended = ended_tx;
            let _orphan = thread_scripts.orphan_guard();
            // A panic is the screen's failure, which the game sees: this
            // guard covers the startup build, `Run`'s own field the rest.
            let _unwinding = Unwinding(Arc::clone(&thread_shared));
            #[cfg(feature = "dev-tools")]
            crate::alloc_count::tag(crate::alloc_count::Tag::Render);
            #[cfg(feature = "dev-tools")]
            crate::dev::read_result_faults();
            #[cfg(feature = "dev-tools")]
            crate::dev::read_script_switches();
            let screen = match build(
                &mut make,
                &mut Committed::default(),
                chart.as_deref(),
                section.as_ref(),
                false,
                &mut None,
                &mut None,
            ) {
                Ok(screen) => {
                    thread_shared.built(screen.as_ref());
                    let _ = ready_tx.send(Ok(()));
                    screen
                }
                Err(e) => {
                    let _ = ready_tx.send(Err(e));
                    return;
                }
            };
            let mut r = Run {
                _unwinding: Unwinding(Arc::clone(&thread_shared)),
                screen: Some(screen),
                started: Instant::now(),
                make,
                chart,
                section,
                clock,
                view,
                measure,
                frames,
                shared: thread_shared,
                end_page: (0, EndPage::default()),
                granted: false,
                extent: None,
                applied: None,
                selected: None,
                play_selection: 0,
                committed: Committed::default(),
                specialized_in: None,
                loading_refused: None,
                paused: None,
                requested: 0,
                attempt: None,
                prepared: None,
                chart_generation: 0,
                chart_table: None,
                staged_table: None,
                chart_targets: (None, None),
                presentation: None,
                turntable: crate::turntable::Turntable::default(),
                app: crate::app_clock::AppClock::new(),
                page_turns: false,
                decision: None,
                decision_shown: None,
                decision_screen: None,
                decision_side: None,
                decision_want: None,
                decision_disabled: None,
                decision_next: 1,
                tuning: None,
                current: None,
                #[cfg(feature = "dev-tools")]
                ready: None,
                #[cfg(feature = "dev-tools")]
                gauge_looks: None,
                #[cfg(feature = "dev-tools")]
                menu_stats: crate::dev::MenuStats::default(),
                #[cfg(feature = "dev-tools")]
                loading_stats: crate::dev::MenuStats::default(),
                selection: None,
                selection_states: 0,
                scripts: thread_scripts,
                result: None,
                result_disabled: None,
                #[cfg(feature = "dev-tools")]
                probe_view: None,
                #[cfg(feature = "dev-tools")]
                select_frames: crate::dev::ScreenFrames::default(),
                #[cfg(feature = "dev-tools")]
                result_frames: crate::dev::ScreenFrames::default(),
                #[cfg(feature = "dev-tools")]
                frames_view: None,
                #[cfg(feature = "dev-tools")]
                present_log: crate::dev::PresentLog::for_run(),
                carries: crate::carry::Carries::default(),
            };
            r.run(&rx);
            #[cfg(feature = "dev-tools")]
            r.flush_screen_frames();
        })
        .map_err(ScreenError::Spawn)?;
    match ready_rx.recv() {
        Ok(Ok(())) => Ok(RenderLink {
            tx,
            shared,
            handle,
            ended,
            answer_timeout: LOAD_TIMEOUT,
            exit_timeout: EXIT_TIMEOUT,
            finish_timeout: FINISH_TIMEOUT,
            end_pages: AtomicU64::new(0),
            requests: AtomicU64::new(0),
            clock,
            scripts,
            result_lifetimes: AtomicU64::new(0),
        }),
        Ok(Err(e)) => {
            let _ = handle.join();
            Err(e.into())
        }
        Err(_) => Err(ScreenError::EndedWhileStarting),
    }
}

/// The mode after the render thread took `page` and `update` of loading
/// lifetime `lifetime` from the slot: the loading state of that lifetime
/// with the newest page and the newest state, an older state never
/// replacing a newer one; the lifetime's first take enters it.
fn take_loading(
    mode: Mode,
    lifetime: u64,
    page: Option<Box<MenuPage>>,
    update: Option<Box<LoadingUpdate>>,
) -> Mode {
    match mode {
        Mode::Menu {
            page: shown,
            update: applied,
            changed,
            loading: Some(l),
        } if l == lifetime => {
            let changed = changed || page.is_some();
            let update = match update {
                Some(u) if newer(applied.as_deref(), &u) => Some(u),
                _ => applied,
            };
            Mode::Menu {
                page: page.unwrap_or(shown),
                update,
                changed,
                loading: Some(lifetime),
            }
        }
        _ => Mode::Menu {
            page: page.unwrap_or_default(),
            update,
            changed: true,
            loading: Some(lifetime),
        },
    }
}

/// The loading frame of lifetime `lifetime` at `now` on the loading clock
/// (design.md "Rendering and Skins", loading state): its origin, progress,
/// gauge descriptor and text revision from the lifetime's newest state
/// `update`; the author's layout, and the covers, `adjusting`, display
/// offset and hi-speed fix of the adjustments' newest `snapshot` (every
/// cover 0 and off, no offset and Classic without one), no gain, which a
/// loading frame has none of; the application's readouts `app`. Without a
/// state yet it is the lifetime's first moment: from `now`, nothing
/// loaded. Allocates nothing.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn loading_frame_of(
    lifetime: u64,
    now: TimeUs,
    update: Option<&LoadingUpdate>,
    snapshot: Option<&game::adjustment::PresentationSnapshot>,
    turntable: i32,
    app: skin_model::AppReadouts,
) -> LoadingFrame<'static> {
    let (lanes, readouts, display_offset) = match snapshot {
        Some(s) => {
            let (lanes, readouts) =
                render::presentation::frame(s, None, None, Speed::NORMAL, false);
            (lanes, readouts, TimeUs(s.display_offset_us))
        }
        None => (
            skin_model::LaneView::NONE,
            skin_model::FrameReadouts::UNAVAILABLE,
            TimeUs(0),
        ),
    };
    let (since, percent, gauge, play_level, text, autoplay) = match update {
        Some(u) => (
            u.origin,
            i32::from(u.percent),
            u.gauge,
            u.chart.play_level,
            u.revision,
            u.autoplay,
        ),
        None => (now, 0, None, None, 0, false),
    };
    let (difficulty, bga, length) = update.map_or((None, None, None), |u| {
        (u.chart.difficulty, u.chart.bga, u.chart.length_us)
    });
    LoadingFrame {
        difficulty,
        bga,
        length,
        lifetime,
        now,
        load: skin_model::LoadState { since, percent },
        // The renderer applies its own target.
        target: [1_920, 1_080],
        layout: &skin_model::Layout::AUTHOR,
        turntable,
        lanes,
        covers: readouts.covers,
        display_offset,
        fix_hispeed: readouts.fix_hispeed,
        gauge,
        play_level,
        autoplay,
        text,
        app,
    }
}

/// One play frame's presentation at chart time `now`: the newest snapshot
/// from `reader` projected with the committed attempt's tempo and epoch
/// (`current`) at the display head, `now` plus the snapshot's display
/// offset; the lanes, the readouts and that offset. Without a reader
/// nothing is known: no gain, every readout unavailable and no offset.
/// Without a committed attempt the snapshot still gives the covers, the
/// settings and the display offset, but no gain or value that depends on
/// it. Allocates nothing.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn play_presentation(
    reader: Option<&mut game::adjustment::PresentationReader>,
    current: Option<&(game::AttemptTempo, u64)>,
    now: TimeUs,
) -> (skin_model::LaneView, skin_model::FrameReadouts, TimeUs) {
    let Some(reader) = reader else {
        return (
            skin_model::LaneView::NONE,
            skin_model::FrameReadouts::UNAVAILABLE,
            TimeUs(0),
        );
    };
    let s = *reader.read();
    let offset = TimeUs(s.display_offset_us);
    let (lanes, readouts) = match current {
        Some((tempo, epoch)) => {
            let head = TimeUs(now.0.saturating_add(offset.0));
            render::presentation::frame(
                &s,
                game::adjustment::TempoMap::bpm_at(tempo, head),
                tempo.references().ok(),
                tempo.speed(),
                s.epoch == *epoch,
            )
        }
        None => render::presentation::frame(&s, None, None, Speed::NORMAL, false),
    };
    (lanes, readouts, offset)
}

/// Whether loading lifetime `lifetime` draws the pending presentation:
/// play's skin is pending, and no specialization of it is kept that was
/// committed while `lifetime` was current (`specialized_in`). Allocates
/// nothing.
#[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
fn pending_presentation(committed: &Committed, specialized_in: Option<u64>, lifetime: u64) -> bool {
    committed.pending(ScreenKind::Play)
        && (committed.get(ScreenKind::Play).is_none() || specialized_in != Some(lifetime))
}

/// What a preparation or a load leaves drawn: the splash stays until what
/// comes next replaces it, since a hidden window shows on a presented
/// frame and a play opens only once the window is shown, and so does the
/// loading state; anything else stops.
impl Run {
    /// What a preparation leaves drawn ([`after_preparing`]), the decision
    /// screen's page kept too. A screen lost while the decision screen was
    /// the latest mode left nothing drawn: built again, it draws that page
    /// again, the presentation of the state the render thread is in. A
    /// later mode message has dropped the page by then, so no decision
    /// frame is drawn after the render thread took one. The skinned
    /// decision screen is kept the same way, and drawn again on a screen
    /// built again while its slot is pinned.
    fn after_preparing(&self, mode: Mode) -> Mode {
        let skinned = self.decision.is_some()
            && self
                .decision_shown
                .as_ref()
                .is_some_and(|s| s.slot.is_some());
        match mode {
            Mode::Decision if skinned => Mode::Decision,
            decision @ Mode::Menu { loading: None, .. } if self.decision.is_some() => decision,
            Mode::Idle if self.screen.is_some() && skinned => Mode::Decision,
            Mode::Idle | Mode::Decision if self.screen.is_some() => match &self.decision {
                Some(page) => Mode::Menu {
                    page: page.clone(),
                    update: None,
                    changed: true,
                    loading: None,
                },
                None => Mode::Idle,
            },
            other => after_preparing(other),
        }
    }
}

fn after_preparing(mode: Mode) -> Mode {
    match mode {
        Mode::Splash { pending } => Mode::Splash { pending },
        // The loading state stays through the chart's load and the play's
        // preparation.
        loading @ Mode::Menu {
            loading: Some(_), ..
        } => loading,
        _ => Mode::Idle,
    }
}

/// The render thread ended with a reply outstanding: the screen's failure
/// is latched in `shared` before [`ScreenError::Ended`] is returned, so
/// a waiter that sees the reply gone sees the failure too, whatever the
/// order the thread's teardown takes.
/// A rebuild of the screen asked of the render thread
/// ([`RenderLink::begin_prepare`]): its reply and its answer's bound.
#[derive(Debug)]
pub struct PendingPrepare {
    rx: Receiver<Result<(), RenderError>>,
    bound: Instant,
}

impl PendingPrepare {
    /// The answer, waited for until `until` at most and in slices of
    /// [`CANCEL_POLL`] (none: not waited for): `None` while it is not in.
    fn poll(&self, until: Option<Instant>, shared: &Shared) -> Option<Result<(), ScreenError>> {
        let now = Instant::now();
        let wait = until
            .map_or(Duration::ZERO, |u| u.saturating_duration_since(now))
            .min(self.bound.saturating_duration_since(now))
            .min(CANCEL_POLL);
        match self.rx.recv_timeout(wait) {
            Ok(ready) => Some(ready.map_err(ScreenError::from)),
            Err(mpsc::RecvTimeoutError::Timeout) => {
                (Instant::now() >= self.bound).then_some(Err(ScreenError::NoAnswer))
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => Some(Err(ended(shared))),
        }
    }
}

/// The rebuild asked on `tx`, its bound `timeout` from now.
fn begin_prepare(tx: &Sender<RenderMsg>, timeout: Duration) -> PendingPrepare {
    let (reply, rx) = mpsc::sync_channel(1);
    let _ = tx.send(RenderMsg::Prepare(reply));
    PendingPrepare {
        rx,
        bound: Instant::now() + timeout,
    }
}

/// What [`serve_recovery`] found of the screen's rebuild.
#[derive(Debug)]
pub enum Recovered {
    /// Nothing to rebuild: the screen is not lost, and no rebuild or
    /// unreported failure is held.
    Nothing,
    /// A rebuild is under way, its answer not in.
    Running,
    /// The rebuild answered, or failed (now or earlier): reported here,
    /// once.
    Answered(Result<(), ScreenError>),
}

/// The one way to the screen's rebuild after a loss, through its slot:
/// held, a saved failure is reported (`report`) or kept; a rebuild under
/// way is polled until `until` (none: no wait); none, and the screen
/// lost, one begins. An answer is reported to a `report` caller; for a
/// look's (`!report`), success empties the slot and a failure stays in it
/// for the next reporter, so a lost screen is rebuilt once per failure
/// and nothing that answers is dropped on the way.
fn serve_recovery(
    shared: &Shared,
    tx: &Sender<RenderMsg>,
    timeout: Duration,
    until: Option<Instant>,
    report: bool,
) -> Recovered {
    let mut slot = shared
        .recovery
        .lock()
        .unwrap_or_else(PoisonError::into_inner);
    if matches!(*slot, Recovery::Idle) {
        if !shared.failed.load(Ordering::Acquire) {
            return Recovered::Nothing;
        }
        *slot = Recovery::Pending(begin_prepare(tx, timeout));
    }
    let answered = match &*slot {
        Recovery::Idle => return Recovered::Nothing,
        Recovery::Pending(pending) => match pending.poll(until, shared) {
            None => return Recovered::Running,
            Some(answered) => answered,
        },
        Recovery::Failed(_) if !report => return Recovered::Running,
        Recovery::Failed(_) => match std::mem::take(&mut *slot) {
            Recovery::Failed(e) => return Recovered::Answered(Err(e)),
            _ => return Recovered::Nothing,
        },
    };
    *slot = match answered {
        Err(e) if !report => Recovery::Failed(e),
        _ if !report => Recovery::Idle,
        answered => {
            *slot = Recovery::Idle;
            return Recovered::Answered(answered);
        }
    };
    Recovered::Running
}

/// The screen's rebuild held by its one owner ([`serve_recovery`]).
#[derive(Debug, Default)]
pub enum Recovery {
    /// None begun, or the last one answered.
    #[default]
    Idle,
    /// Begun, its answer not in.
    Pending(PendingPrepare),
    /// It failed, or reached its bound: the flow reports it.
    Failed(ScreenError),
}

/// The screen as a look inside a rig's wait serves it
/// ([`RenderLink::recovery_handle`]).
pub struct RecoveryHandle {
    tx: Sender<RenderMsg>,
    shared: Arc<Shared>,
    timeout: Duration,
}

impl RecoveryHandle {
    /// One look at the screen, waiting for nothing: lost with no rebuild
    /// under way, its rebuild begins into the slot; under way, its answer
    /// taken if it came, the slot empty again, or holding its failure for
    /// the flow. A failure stays until the flow takes it, so a lost screen
    /// is built at most once per failure.
    pub fn look(&self) {
        let _ = serve_recovery(&self.shared, &self.tx, self.timeout, None, false);
    }
}

fn ended(shared: &Shared) -> ScreenError {
    shared.failed.store(true, Ordering::Release);
    ScreenError::Ended
}

/// Marks the screen failed when the render thread unwinds from a panic,
/// so [`RenderLink::failed`] reports it: its resources are gone with it.
struct Unwinding(Arc<Shared>);

impl Drop for Unwinding {
    fn drop(&mut self) {
        if std::thread::panicking() {
            self.0.failed.store(true, Ordering::Release);
        }
    }
}

/// What one screen kind has committed.
#[derive(Debug, Default)]
enum Slot {
    /// No declarative installation: the built-in presentation (play's
    /// simple skin).
    #[default]
    BuiltIn,
    /// Play's skin is pending ([`RenderLink::pend`]), with the
    /// specialization committed last, if any.
    Pending(Option<Installation>),
    /// The declarative installation committed last.
    Installed(Installation),
    /// The result kind's pages-only installation committed last.
    Pages(Arc<PackedPages>),
}

/// Each screen kind's declarative installation committed last, its
/// loading presentation with it, kept apart from the screen, or play's
/// pending marker: a screen built again restores exactly these, in
/// [`Committed::KINDS`]' order.
#[derive(Debug, Default)]
struct Committed([Slot; 4]);

impl Committed {
    /// The kinds in the order their installations are restored.
    const KINDS: [ScreenKind; 4] = [
        ScreenKind::Play,
        ScreenKind::Selection,
        ScreenKind::Decision,
        ScreenKind::Result,
    ];

    const fn index(kind: ScreenKind) -> usize {
        match kind {
            ScreenKind::Play => 0,
            ScreenKind::Selection => 1,
            ScreenKind::Decision => 2,
            ScreenKind::Result => 3,
        }
    }

    /// `kind`'s installation, if any: the one installed, or a pending
    /// skin's specialization committed last.
    fn get(&self, kind: ScreenKind) -> Option<&Installation> {
        match self.0.get(Self::index(kind)) {
            Some(Slot::Installed(i) | Slot::Pending(Some(i))) => Some(i),
            _ => None,
        }
    }

    /// Keeps `installation` as `kind`'s, installed; `None` keeps none.
    fn set(&mut self, kind: ScreenKind, installation: Option<Installation>) {
        if let Some(entry) = self.0.get_mut(Self::index(kind)) {
            *entry = installation.map_or(Slot::BuiltIn, Slot::Installed);
        }
    }

    /// Keeps none for `kind`.
    fn clear(&mut self, kind: ScreenKind) {
        self.set(kind, None);
    }

    /// Keeps `pages` as `kind`'s pages-only installation.
    fn set_pages(&mut self, kind: ScreenKind, pages: Arc<PackedPages>) {
        if let Some(entry) = self.0.get_mut(Self::index(kind)) {
            *entry = Slot::Pages(pages);
        }
    }

    /// `kind`'s pages-only installation, if it has one.
    fn pages(&self, kind: ScreenKind) -> Option<&Arc<PackedPages>> {
        match self.0.get(Self::index(kind)) {
            Some(Slot::Pages(p)) => Some(p),
            _ => None,
        }
    }

    /// The atlas `kind`'s committed installation keeps for recovery.
    fn atlas(&self, kind: ScreenKind) -> Option<AtlasId> {
        match self.0.get(Self::index(kind)) {
            Some(Slot::Installed(i) | Slot::Pending(Some(i))) => Some(i.bundle().atlas_id()),
            Some(Slot::Pages(p)) => Some(p.id()),
            _ => None,
        }
    }

    /// Marks `kind`'s skin pending, with `specialization` the one
    /// committed, if any.
    fn pend(&mut self, kind: ScreenKind, specialization: Option<Installation>) {
        if let Some(entry) = self.0.get_mut(Self::index(kind)) {
            *entry = Slot::Pending(specialization);
        }
    }

    /// Whether `kind`'s skin is pending.
    fn pending(&self, kind: ScreenKind) -> bool {
        matches!(self.0.get(Self::index(kind)), Some(Slot::Pending(_)))
    }
}

/// A screen made afresh, with every kind's `committed` installation
/// restored in [`Committed::KINDS`]' order, and `chart` loaded. A kind
/// other than play whose restore the new screen refuses on a healthy
/// device (a new adapter's smaller limits, say) keeps no installation
/// from then on, and its screen draws its built-in presentation; a
/// pending play skin's specialization refused so keeps only the pending
/// marker, the next attempt staging one again. Play's installed skin
/// refused, and any failure, fails the build. A result restore refused
/// says why in `result_refused`, so its owner disables the kind for the
/// run (the S10-09i design's D2, "Keying and recovery").
fn build(
    make: &mut MakeScreen,
    committed: &mut Committed,
    chart: Option<&Chart>,
    section: Option<&Section>,
    disabled: bool,
    result_refused: &mut Option<String>,
    decision_refused: &mut Option<String>,
) -> Result<Box<dyn Screen>, RenderError> {
    let mut s = make()?;
    if disabled {
        s.disable_result();
    }
    for kind in Committed::KINDS {
        if let Some(pages) = committed.pages(kind).cloned() {
            match s.restore_pages(kind, &pages) {
                Ok(()) => {}
                Err(e) if e.is_refusal() => {
                    eprintln!(
                        "render: the {kind} skin's atlas was not restored, its screen draws its built-in presentation: {}",
                        Chain(&e)
                    );
                    committed.clear(kind);
                    if kind == ScreenKind::Result {
                        *result_refused = Some(format!(
                            "the result skin was not restored on the screen built again: {}",
                            Chain(&e)
                        ));
                    }
                }
                Err(e) => return Err(e),
            }
            continue;
        }
        let Some(installation) = committed.get(kind).cloned() else {
            continue;
        };
        let restored = s.restore(kind, &installation);
        // A refusal injected by a test of the real flow (dev-tools).
        #[cfg(feature = "dev-tools")]
        let restored = match restored {
            Ok(()) if kind == ScreenKind::Result && crate::dev::refuse_result_restore() => {
                Err(RenderError::Skin(render::SkinError {
                    reason: "dev: the result's restore was refused".to_owned(),
                }))
            }
            other => other,
        };
        match restored {
            Ok(()) => {}
            Err(e) if committed.pending(kind) && e.is_refusal() => {
                eprintln!(
                    "render: the {kind} skin's specialization was not restored, the next attempt stages one again: {}",
                    Chain(&e)
                );
                committed.pend(kind, None);
            }
            Err(e) if kind != ScreenKind::Play && e.is_refusal() => {
                eprintln!(
                    "render: the {kind} skin was not restored, its screen draws its built-in presentation: {}",
                    Chain(&e)
                );
                committed.clear(kind);
                if kind == ScreenKind::Result {
                    *result_refused = Some(format!(
                        "the result skin was not restored on the screen built again: {}",
                        Chain(&e)
                    ));
                }
                if kind == ScreenKind::Decision {
                    *decision_refused = Some(format!(
                        "the decision skin was not restored on the screen built again: {}",
                        Chain(&e)
                    ));
                }
            }
            Err(e) => return Err(e),
        }
    }
    if let Some(chart) = chart {
        s.load(chart, section)?;
    }
    Ok(s)
}

enum Mode {
    Idle,
    /// The launch screen, until the game sends what to draw; a select
    /// page sent before it ends ([`SPLASH`]) waits in `pending`.
    Splash {
        pending: Option<Box<MenuPage>>,
    },
    Play {
        /// The play's first READY candidate's chart time 0 on the clock: a
        /// rearmed candidate's view carries its own.
        origin: TimeUs,
        /// Where the play state was entered on the clock, kept by a rearm.
        state_origin: TimeUs,
        side: RenderSide,
        gauge: usize,
        first: bool,
        /// The READY candidate drawn last, as its view's generation
        /// (design.md "Play Lifecycle", the start gate): a view of
        /// another starts the screen's runtime over before it is drawn.
        rearms: u64,
        /// The acquisitions skipped in a row ([`ACQUIRE_RETRIES`]).
        skipped: u32,
    },
    /// The end screen: [`Run::end_page`].
    End,
    /// The select screen, or with `loading` the play screen's loading
    /// state of that loading lifetime, its lines and the lifetime's newest
    /// state `update`; `changed` until the page is laid out.
    Menu {
        page: Box<MenuPage>,
        update: Option<Box<LoadingUpdate>>,
        changed: bool,
        loading: Option<u64>,
    },
    /// The decision skin of the screen shown ([`Run::decision_shown`]),
    /// its labels borrowed from their pinned slot; its page
    /// ([`Run::decision`]) draws when the skin cannot.
    Decision,
    /// The select screen from its newest update: [`Run::selection`].
    Selection,
    /// The result screen from its presentations: [`Run::result`].
    Result,
}

/// The select screen as the render thread holds it: the newest update
/// taken, whether the screen has applied it, and the selection skin's
/// state for its lifetime ([`SelectionLogic`]), kept apart from the
/// screen so a screen built again draws on from it.
struct ShownSelection {
    update: Box<SelectionUpdate>,
    changed: bool,
    logic: Option<SelectionLogic>,
}

/// An attempt's preparation the screen has not answered yet.
struct Preparing {
    request: u64,
    attempt: Box<AttemptContext>,
    /// The declarative skin it stages, if one is chosen.
    skin: Option<Arc<SkinBundle>>,
    /// What it takes over from the earlier attempts of its play state.
    offer: crate::carry::Offer,
    /// The installation it draws with.
    installed: crate::carry::Installed,
    reply: mpsc::SyncSender<Result<Answered, RenderError>>,
}

/// How an attempt's preparation ended on the render thread, with the
/// screen's structural inputs then: those it was staged with when ready.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Answered {
    /// How it ended.
    pub preparation: Preparation,
    /// The screen's device, target size and play skin selection.
    pub screen: game::ScreenKey,
}

/// The final readiness's answer ([`RenderMsg::Ready`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ReadyAnswer {
    /// The staging fits the screen and is stamped: the commit may follow.
    Ready,
    /// Nothing of this request is staged, or the screen's structural
    /// inputs are not those it was staged with: these are the inputs now.
    Stale(game::ScreenKey),
}

/// An attempt's preparation answered ready: its token and whether it
/// staged a declarative skin.
struct Prepared {
    token: PreparedToken,
    /// The screen's structural inputs it was staged with.
    screen: game::ScreenKey,
    declarative: bool,
    /// The attempt's tempo and presentation epoch, current once committed.
    presentation: (game::AttemptTempo, u64),
    /// Autoplay's scratch notes, the turntable's once committed.
    scratch_notes: Vec<game::ScratchNote>,
    /// Its play state and chart, what it was offered, the installation
    /// it draws with and what its score data is over, for the carry's
    /// record at its commit.
    carry: (
        Option<crate::carry::CarryKey>,
        crate::carry::Offer,
        crate::carry::Installed,
        crate::carry::Basis,
    ),
    /// The attempt's READY on the chart's axis (dev-tools).
    #[cfg(feature = "dev-tools")]
    ready: (TimeUs, TimeUs),
    /// The attempt's gauges' looks, in the preset's order (dev-tools).
    #[cfg(feature = "dev-tools")]
    gauge_looks: Vec<skin_model::GaugeLook>,
}

struct Run {
    /// The first field, so it drops first: an unwinding thread publishes
    /// its failure before the screen's resources are destroyed.
    _unwinding: Unwinding,
    screen: Option<Box<dyn Screen>>,
    /// When the render thread started: the launch screen's time runs from it.
    started: Instant,
    make: MakeScreen,
    chart: Option<Arc<Chart>>,
    /// The section of the chart a practice round draws only the targets of.
    section: Option<Section>,
    clock: QpcClock,
    view: View,
    measure: Measure,
    frames: Box<dyn FrameSink>,
    shared: Arc<Shared>,
    /// The end screen's page last taken from the slot, and its number.
    end_page: (u64, EndPage),
    /// The screen's wait granted a frame that no draw has taken yet: the
    /// end screen was left or nothing was to be drawn. Only a presented
    /// frame gives the grant back (a swap chain's waitable object, one
    /// frame of latency), so the next frame takes it instead of waiting
    /// again, which would never end; until a new screen is built.
    granted: bool,
    /// The window's size followed, if any.
    extent: Option<Arc<Extent>>,
    /// The size last applied to the screen; `None` until one is, and for
    /// a screen built again.
    applied: Option<(u32, u32)>,
    /// The declarative skin later attempts are prepared with; `None`
    /// prepares the skin in use (tests choose one).
    selected: Option<Arc<SkinBundle>>,
    /// The request of the play skin selection in use, which a staged
    /// attempt's structural inputs hold.
    play_selection: u64,
    /// Each kind's declarative installation committed last, its loading
    /// presentation with it, kept apart from the screen: a screen built
    /// again restores exactly them.
    committed: Committed,
    /// The loading lifetime current when a pending play skin's
    /// specialization was last committed: its loading frames, and only
    /// its, draw the skin's own; `None` when none was committed while a
    /// lifetime was current.
    specialized_in: Option<u64>,
    /// The loading lifetime whose loading frame the skin refused, and
    /// why: none of it is drawn again, and its end reports the refusal.
    loading_refused: Option<(u64, BuildError)>,
    /// The loading state of a paused lifetime, kept as it was drawn and
    /// fed what its sinks report, until it is resumed, ended or paused
    /// again.
    paused: Option<Mode>,
    /// The newest request asked, an attempt's preparation or a
    /// selection: an older one arriving late changes nothing.
    requested: u64,
    /// The attempt's preparation not yet answered.
    attempt: Option<Preparing>,
    /// The attempt's preparation answered ready and not yet committed.
    prepared: Option<Prepared>,
    /// The chart images' generation the tables below were checked in.
    chart_generation: u64,
    /// What the committed installation's chart table was published for,
    /// and the staging's.
    chart_table: Option<crate::chart_images::PublicationKey>,
    staged_table: Option<crate::chart_images::PublicationKey>,
    /// The live targets last mirrored to the game thread.
    chart_targets: (Option<u64>, Option<skin_model::chart_image::ChartKey>),
    /// The adjustments' snapshots ([`RenderMsg::Presentation`]).
    presentation: Option<game::adjustment::PresentationReader>,
    /// The play state's turntable (docs/stage3.md, offset 1).
    turntable: crate::turntable::Turntable,
    /// The application clock: every screen's presents, which each play
    /// and loading frame's readouts count.
    app: crate::app_clock::AppClock,
    /// Whether the page drawn is the practice page
    /// ([`RenderMsg::PracticePage`]), whose frames tick the turntable.
    page_turns: bool,
    /// The decision screen's page while it is the latest mode the render
    /// thread was told of ([`RenderMsg::Decision`]); every later mode
    /// message drops it. A chart's load or a preparation keeps its page,
    /// and a screen built again draws it again ([`Run::after_preparing`]).
    decision: Option<Box<MenuPage>>,
    /// The decision screen shown with it, while it is the latest mode:
    /// its lifetime, choice, timing and pinned slot, `None` for the slot
    /// once the built-in presentation took over.
    decision_shown: Option<crate::decision_side::ShownDecision>,
    /// The decision skin's state for the screen shown, made at its
    /// message.
    decision_screen: Option<skin_model::DecisionScreen>,
    /// The committed decision installation's label preparation.
    decision_side: Option<crate::decision_side::DecisionSide>,
    /// The chart the selection cursor rests on, its labels asked of the
    /// preparation; kept after selection is left.
    decision_want: Option<crate::decision_side::Want>,
    /// Why the decision kind is disabled for the run: no request is posted
    /// and no slot is pinned from then on.
    decision_disabled: Option<String>,
    /// The next decision label preparation's generation, unique for the
    /// run.
    decision_next: render::Gen,
    /// The settings keys' values ([`RenderMsg::Tuning`]).
    tuning: Option<Arc<LiveTuning>>,
    /// The committed attempt's tempo and presentation epoch, which its
    /// play frames project the snapshot with.
    current: Option<(game::AttemptTempo, u64)>,
    /// The committed attempt's READY on the chart's axis (dev-tools).
    #[cfg(feature = "dev-tools")]
    ready: Option<(TimeUs, TimeUs)>,
    /// The committed attempt's gauges' looks (dev-tools).
    #[cfg(feature = "dev-tools")]
    gauge_looks: Option<Vec<skin_model::GaugeLook>>,
    /// The select screen's frames (dev-tools).
    #[cfg(feature = "dev-tools")]
    menu_stats: crate::dev::MenuStats,
    /// The loading state's frames (dev-tools).
    #[cfg(feature = "dev-tools")]
    loading_stats: crate::dev::MenuStats,
    /// The select screen's newest update and skin state, while its
    /// lifetime is current.
    selection: Option<ShownSelection>,
    /// The selection skin states made so far: each takes the next number.
    selection_states: u64,
    /// The screens' runtimes, shared with the game thread.
    scripts: Scripts,
    /// The result screen's presentations, while its lifetime is current.
    result: Option<ResultSide>,
    /// Why the result kind is disabled for the run (the S10-09i design's
    /// D7): kept apart from the screen, so a screen built again is
    /// disabled before it restores anything.
    result_disabled: Option<String>,
    /// Development builds: the view the last presented result frame drew
    /// with its skin, for G5's end (the S10-09i design's §8.1).
    #[cfg(feature = "dev-tools")]
    probe_view: Option<crate::result_present::ViewKey>,
    /// The select screen's whole frames (dev-tools; the S7-07d gate).
    #[cfg(feature = "dev-tools")]
    select_frames: crate::dev::ScreenFrames,
    /// The result screen's whole frames (dev-tools; the S7-07d gate).
    #[cfg(feature = "dev-tools")]
    result_frames: crate::dev::ScreenFrames,
    /// The view the last result frame presented, for the whole frames'
    /// switches (dev-tools).
    #[cfg(feature = "dev-tools")]
    frames_view: Option<crate::result_present::ViewKey>,
    /// Every present's time while `tapestry measure select` runs
    /// (dev-tools).
    #[cfg(feature = "dev-tools")]
    present_log: crate::dev::PresentLog,
    /// The play state's carry across its attempts, kept apart from the
    /// screen; every change goes through [`crate::carry::Carries::on`].
    carries: crate::carry::Carries,
}

impl Run {
    /// How long the launch screen has shown: since the render thread
    /// started (in tests, the time a test set instead, if any).
    fn launch_elapsed(shared: &Shared, started: Instant) -> Duration {
        #[cfg(test)]
        if let Some(elapsed) = *shared
            .launch_elapsed
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            return elapsed;
        }
        #[cfg(not(test))]
        let _ = shared;
        started.elapsed()
    }

    /// Takes the newest selection update from the slot, if it is newer than
    /// the one held and its lifetime current: the lifetime's first starts
    /// the skin's state afresh.
    fn take_selection(&mut self) {
        let taken = {
            let mut slot = self.shared.selection_slot();
            slot.take().filter(|u| slot.current(u.lifetime))
        };
        let Some(update) = taken else {
            return;
        };
        match &mut self.selection {
            Some(shown) if !update.newer_than(&shown.update) => {}
            Some(shown) if shown.update.lifetime == update.lifetime => {
                shown.update = update;
                shown.changed = true;
            }
            other => {
                *other = Some(ShownSelection {
                    update,
                    changed: true,
                    logic: None,
                });
            }
        }
        self.want_decision_labels();
    }

    /// The selection skin's state for `shown`'s lifetime on the committed
    /// installation: made afresh for a new lifetime or installation, none
    /// without an installation.
    fn selection_logic(
        committed: &Committed,
        shown: &mut ShownSelection,
        states: &mut u64,
        scripts: &Scripts,
    ) {
        match committed.get(ScreenKind::Selection) {
            None => shown.logic = None,
            Some(i)
                if shown
                    .logic
                    .as_ref()
                    .is_none_or(|l| !Arc::ptr_eq(&l.bundle, i.bundle())) =>
            {
                *states = states.saturating_add(1);
                let script = scripts.slot(ScreenKind::Selection, Arc::as_ptr(i.bundle()).addr());
                shown.logic = Some(SelectionLogic::new(i.bundle(), *states).with_script(script));
            }
            Some(_) => {}
        }
    }

    fn run(&mut self, rx: &Receiver<RenderMsg>) {
        let mut mode = Mode::Splash { pending: None };
        let mut effects: Vec<JudgeEvent> = Vec::with_capacity(self.view.effects);
        loop {
            self.sync_chart();
            let idle = matches!(mode, Mode::Idle) || self.screen.is_none();
            let msg = if idle && (self.attempt.is_some() || self.decision_busy()) {
                // A preparation not yet answered is asked again between
                // messages.
                rx.recv_timeout(CANCEL_POLL).map_err(|e| match e {
                    mpsc::RecvTimeoutError::Timeout => TryRecvError::Empty,
                    mpsc::RecvTimeoutError::Disconnected => TryRecvError::Disconnected,
                })
            } else if idle {
                rx.recv().map_err(|_| TryRecvError::Disconnected)
            } else if matches!(mode, Mode::Play { skipped, .. } if skipped >= ACQUIRE_RETRIES) {
                // A play frame's acquisition keeps being skipped: wait a
                // little, or for a message, before trying it again.
                rx.recv_timeout(ACQUIRE_WAIT).map_err(|e| match e {
                    mpsc::RecvTimeoutError::Timeout => TryRecvError::Empty,
                    mpsc::RecvTimeoutError::Disconnected => TryRecvError::Disconnected,
                })
            } else {
                rx.try_recv()
            };
            // The select screen is left: its frames are reported; and the
            // loading state's, once it is left.
            #[cfg(feature = "dev-tools")]
            if msg.is_ok() && !matches!(msg, Ok(RenderMsg::Menu(_) | RenderMsg::PracticePage(_))) {
                self.menu_stats.report("select");
            }
            #[cfg(feature = "dev-tools")]
            if msg.is_ok()
                && !matches!(
                    msg,
                    Ok(RenderMsg::Loading(_)
                        | RenderMsg::Load { .. }
                        | RenderMsg::Prepare(_)
                        | RenderMsg::PrepareAttempt { .. }
                        | RenderMsg::Commit { .. }
                        | RenderMsg::Ready { .. }
                        | RenderMsg::Discard(_)
                        | RenderMsg::ChartImages { .. }
                        | RenderMsg::ParkScripts(_))
                )
            {
                self.loading_stats.report("loading");
            }
            match msg {
                Ok(RenderMsg::Load {
                    chart,
                    section,
                    reply,
                }) => {
                    #[cfg(feature = "dev-tools")]
                    crate::dev::hold(crate::dev::LoadPhase::ChartLoad);
                    // What the screen draws of the chart (dev-tools): a
                    // section's targets only.
                    #[cfg(feature = "dev-tools")]
                    {
                        let notes = || chart.lanes.iter().flatten();
                        match &section {
                            Some(s) => println!(
                                "dev: render: a section's {} targets of {} notes are drawn",
                                notes().filter(|n| s.is_target(n)).count(),
                                notes().count()
                            ),
                            None => {
                                println!("dev: render: all {} notes are drawn", notes().count())
                            }
                        }
                    }
                    self.chart = Some(chart);
                    self.section = section;
                    let mut ready = self.prepare();
                    // The device or the surface lost while the chart was
                    // built: the screen is built again once more, with the
                    // chart, inside the one answer (design.md "Play
                    // Lifecycle", preparing each attempt). A skin's refusal
                    // is not built again.
                    if ready.as_ref().is_err_and(|e| !e.is_refusal()) {
                        #[cfg(test)]
                        self.shared.chart_rebuilds.fetch_add(1, Ordering::AcqRel);
                        ready = self.prepare();
                    }
                    // A chart the screen cannot draw is refused, and the
                    // screen it dropped is built again without it, for the
                    // select screen; if that fails too, the failure is
                    // reported and the game builds it through a preparation.
                    if ready.is_err() {
                        self.chart = None;
                        self.section = None;
                        if self.prepare().is_err() {
                            self.shared.failed.store(true, Ordering::Release);
                        }
                    }
                    let _ = reply.send(ready);
                    mode = self.after_preparing(mode);
                    continue;
                }
                // A test's fence: every message before it was taken, and
                // the frame after them drawn.
                #[cfg(test)]
                Ok(RenderMsg::Fence(reply)) => {
                    let _ = reply.send(());
                    continue;
                }
                #[cfg(test)]
                Ok(RenderMsg::ModeProbe(reply)) => {
                    let _ = reply.send(match &mode {
                        Mode::Idle => "idle".to_owned(),
                        Mode::Menu { page, .. } => format!(
                            "menu:{}",
                            page.lines.first().map_or("", |l| l.text.as_str())
                        ),
                        _ => "other".to_owned(),
                    });
                    continue;
                }
                Ok(RenderMsg::ParkScripts(reply)) => {
                    if let Some(logic) = self.selection.as_mut().and_then(|s| s.logic.as_mut()) {
                        logic.script.park();
                    }
                    if let Some(side) = &mut self.result {
                        side.park_scripts();
                    }
                    let _ = reply.send(());
                    continue;
                }
                Ok(RenderMsg::Menu(page)) => {
                    // A generic page (a preparation's wait, a notice) is
                    // no play state's presentation: no turntable.
                    self.page_turns = false;
                    self.clear_decision();
                    mode = Self::menu_mode(mode, page, &self.shared, self.started);
                }
                Ok(RenderMsg::Decision { page, shown }) => {
                    // The decision screen is no play state's presentation:
                    // the turntable is not adopted or ticked (design.md
                    // :732).
                    self.page_turns = false;
                    self.decision = Some(page.clone());
                    // The presentation chosen at the choice is not checked
                    // again here: a later invalidation is a recovery or a
                    // takeover, never a retrospective revision.
                    self.decision_shown = shown;
                    self.decision_screen = None;
                    if shown.is_some_and(|s| s.slot.is_some()) {
                        self.enter_skinned_decision();
                    }
                    mode = if self.decision_skinned() {
                        Mode::Decision
                    } else {
                        Self::menu_mode(mode, page, &self.shared, self.started)
                    };
                }
                Ok(RenderMsg::PracticePage(page)) => {
                    // The practice page begins or resumes its play
                    // state's presentation.
                    self.page_turns = true;
                    self.clear_decision();
                    self.turntable.practice_page();
                    mode = Self::menu_mode(mode, page, &self.shared, self.started);
                }
                Ok(RenderMsg::ResultChanged) => {
                    self.take_result();
                    if self.result.is_some() {
                        self.clear_decision();
                        mode = Mode::Result;
                    }
                }
                Ok(RenderMsg::SelectionChanged) => {
                    self.clear_decision();
                    self.take_selection();
                    mode = match mode {
                        // The launch screen shows out its time, the
                        // select screen after it.
                        Mode::Splash { .. }
                            if Self::launch_elapsed(&self.shared, self.started) < SPLASH =>
                        {
                            Mode::Splash { pending: None }
                        }
                        _ if self.selection.is_some() => Mode::Selection,
                        other => other,
                    };
                }
                Ok(RenderMsg::Loading(lifetime)) => {
                    self.clear_decision();
                    self.turntable.begin_presentation();
                    // Lines the slot no longer holds (taken, or their
                    // lifetime ended) change nothing, and neither do the
                    // lines of a lifetime whose loading frame was refused.
                    if let Some((page, update)) = self.shared.take_loading(lifetime)
                        && self.loading_refused.is_none_or(|(l, _)| l != lifetime)
                    {
                        // A paused lifetime's are kept for its resumption.
                        match self.paused.take() {
                            Some(
                                kept @ Mode::Menu {
                                    loading: Some(l), ..
                                },
                            ) if l == lifetime => {
                                self.paused = Some(take_loading(kept, lifetime, page, update));
                            }
                            other => {
                                self.paused = other;
                                mode = take_loading(mode, lifetime, page, update);
                            }
                        }
                    }
                }
                Ok(RenderMsg::PauseLoading { lifetime, reply }) => {
                    // Between frames, as an end: none of the loading
                    // state is drawn from here, but it is kept.
                    self.paused = None;
                    if matches!(
                        mode,
                        Mode::Menu {
                            loading: Some(l),
                            ..
                        } if l == lifetime
                    ) {
                        self.paused = Some(std::mem::replace(&mut mode, Mode::Idle));
                    }
                    #[cfg(test)]
                    self.shared.loading_end_gate.pass();
                    let _ = reply.send(self.loading_end(lifetime));
                    continue;
                }
                Ok(RenderMsg::ResumeLoading(lifetime)) => {
                    // Only over nothing drawn, while the lifetime is
                    // current and none of it was refused.
                    if let Some(
                        kept @ Mode::Menu {
                            loading: Some(l), ..
                        },
                    ) = self.paused.take()
                        && l == lifetime
                        && matches!(mode, Mode::Idle)
                        && self.shared.loading_current(lifetime)
                        && self.loading_refused.is_none_or(|(r, _)| r != lifetime)
                    {
                        mode = kept;
                    }
                }
                Ok(RenderMsg::EndLoading { lifetime, reply }) => {
                    // Between frames: none of the loading state follows.
                    if matches!(
                        mode,
                        Mode::Menu {
                            loading: Some(_),
                            ..
                        }
                    ) {
                        mode = Mode::Idle;
                    }
                    self.paused = None;
                    #[cfg(test)]
                    self.shared.loading_end_gate.pass();
                    let _ = reply.send(self.loading_end(lifetime));
                    continue;
                }
                Ok(RenderMsg::Prepare(reply)) => {
                    #[cfg(test)]
                    self.shared.prepares.fetch_add(1, Ordering::AcqRel);
                    let ready = self.recover();
                    let _ = reply.send(ready);
                    mode = self.after_preparing(mode);
                    // A result lifetime draws on from what it presented.
                    if self.result.as_ref().is_some_and(ResultSide::shows) {
                        mode = Mode::Result;
                    }
                    continue;
                }
                Ok(RenderMsg::PrepareAttempt {
                    request,
                    attempt,
                    skin,
                    reply,
                }) => {
                    self.turntable.begin_presentation();
                    self.begin_attempt(request, attempt, skin, reply);
                    mode = self.after_preparing(mode);
                    continue;
                }
                Ok(RenderMsg::PlayState(state)) => {
                    self.turntable.created(state);
                    continue;
                }
                Ok(RenderMsg::Settled(tally)) => {
                    let held = self.screen.as_ref().and_then(|s| s.carry());
                    self.carries.on(crate::carry::Event::Settled {
                        tally: &tally,
                        held,
                    });
                    continue;
                }
                Ok(RenderMsg::Turntable { number, pause }) => {
                    if pause {
                        self.turntable.pause(number);
                    } else {
                        self.turntable.resume(number);
                    }
                    continue;
                }
                Ok(RenderMsg::Presentation(reader)) => {
                    self.presentation = Some(*reader);
                    continue;
                }
                Ok(RenderMsg::Tuning(tuning)) => {
                    self.tuning = Some(tuning);
                    continue;
                }
                Ok(RenderMsg::Commit { request, reply }) => {
                    let _ = reply.send(self.commit(request));
                    continue;
                }
                Ok(RenderMsg::Ready {
                    request,
                    epoch,
                    reply,
                }) => {
                    let _ = reply.send(self.ready(request, epoch));
                    continue;
                }
                Ok(RenderMsg::Discard(request)) => {
                    self.discard(request);
                    continue;
                }
                Ok(RenderMsg::ChartImages {
                    generation,
                    request,
                    set,
                    reply,
                }) => {
                    let ack = self.publish_chart(generation, request, set.as_deref());
                    let _ = reply.send(ack);
                    continue;
                }
                Ok(RenderMsg::Select {
                    request,
                    kind,
                    skin,
                    reply,
                }) => {
                    let answer = match skin {
                        Selected::Skin(skin, script) => self.select(request, kind, skin, script),
                        Selected::Pending => self.pend(request, kind),
                        Selected::Pages(pages) => self.select_pages(request, kind, &pages),
                    };
                    self.reconcile_result();
                    let _ = reply.send(answer);
                    mode = self.after_preparing(mode);
                    continue;
                }
                Ok(RenderMsg::Play {
                    side,
                    gauge,
                    measure,
                    state_origin,
                }) => {
                    let origin = measure.origin;
                    // The committed attempt starts, whether or not the
                    // screen still holds its `Play`: its play state's
                    // carry waits for its settlement.
                    self.carries.on(crate::carry::Event::Started);
                    self.turntable.play_starts();
                    self.measure.start(*measure);
                    self.shared.first_frame.store(i64::MIN, Ordering::Release);
                    mode = Mode::Play {
                        origin,
                        state_origin,
                        side,
                        gauge,
                        first: true,
                        rearms: 0,
                        skipped: 0,
                    };
                }
                // Without a screen (it failed), the failure stays reported
                // and nothing is drawn: the game builds the screen again
                // through a preparation, which can fail.
                // Its page, or a newer one an earlier signal took; whether
                // it may still be drawn is checked before every end frame.
                Ok(RenderMsg::End) => {
                    if let Some(page) = self.shared.take_end() {
                        self.end_page = page;
                        if let Some(side) = &mut self.result {
                            side.end_changed = true;
                        }
                    }
                    // A result skin's presentation draws instead while its
                    // lifetime is current: the page is its built-in rows'.
                    mode = if self.result.as_ref().is_some_and(ResultSide::shows) {
                        Mode::Result
                    } else {
                        Mode::End
                    };
                }
                Ok(RenderMsg::Finish { end, reply }) => {
                    // Judgments still queued were never drawn in this play:
                    // N/A, not missing from the figures.
                    if let Mode::Play { side, .. } = &mut mode {
                        effects.clear();
                        side.drain_effects(|e| effects.push(*e));
                        self.measure.never_drawn(&effects);
                        effects.clear();
                    }
                    // The frames drawn before the end are written when it
                    // is answered.
                    #[cfg(feature = "dev-tools")]
                    self.frames.finish();
                    let _ = reply.send(self.measure.finish(end));
                }
                Ok(RenderMsg::Follow(extent)) => {
                    // A wake sent since the sender was set is still in
                    // the channel, after this: it is taken there.
                    self.extent = Some(extent);
                    self.applied = None;
                }
                // Applied below, as every loop applies the newest size.
                Ok(RenderMsg::Resize) => {
                    if let Some(extent) = &self.extent {
                        extent.acknowledge();
                    }
                }
                #[cfg(all(test, feature = "dev-tools"))]
                Ok(RenderMsg::Panic) => panic!("the render thread fails (a test)"),
                #[cfg(all(test, feature = "dev-tools"))]
                Ok(RenderMsg::FramesProbe(reply)) => {
                    let (sl, sf, _) = self.select_frames.probe();
                    let (rl, rf, charged) = self.result_frames.probe();
                    let _ = reply.send(crate::dev::FramesProbe {
                        select: (sl, sf.len()),
                        result: (rl, rf),
                        charged,
                    });
                }
                Ok(RenderMsg::Exit) | Err(TryRecvError::Disconnected) => return,
                Err(TryRecvError::Empty) => {}
            }
            // The whole frames of a selection lifetime that ended, before
            // any frame of what follows: updates, rebuilds and resizes of
            // the same lifetime keep them (dev-tools, the S7-07d gate).
            #[cfg(feature = "dev-tools")]
            if self
                .select_frames
                .lifetime()
                .is_some_and(|l| !self.shared.selection_current(l))
            {
                self.select_frames
                    .report("select", self.clock.frequency().hz());
            }
            self.poll_attempt();
            // A result presentation is prepared apart from what draws, as
            // it comes and before the frame's wait (the S10-09i design's
            // D1, "Prepared").
            if matches!(mode, Mode::Result) {
                #[cfg(feature = "dev-tools")]
                let prepared = Instant::now();
                self.prepare_result();
                // Charged to the next frame only while the screen it was
                // done on lives (dev-tools: the whole frames).
                #[cfg(feature = "dev-tools")]
                if self.screen.is_some() {
                    self.result_frames.before_wait(prepared);
                }
                if self.screen.is_none() {
                    mode = Mode::Idle;
                    continue;
                }
            }
            let Some(screen) = &mut self.screen else {
                continue;
            };
            // The window's newest size, between frames.
            if let Some(size) = self.extent.as_ref().and_then(|e| e.latest())
                && self.applied != Some(size)
            {
                screen.resize(size.0, size.1);
                self.applied = Some(size);
            }
            // The decision labels' work at this frame boundary, outside
            // every frame, after the newest size: a device or poll failure
            // drops the screen, which the game builds again.
            if self.decision_boundary() {
                mode = Mode::Idle;
                continue;
            }
            let Some(screen) = &mut self.screen else {
                continue;
            };
            // Minimized, nothing is presented, so the swap chain's waitable
            // object would not be signaled: frames are paced by the clock
            // instead, and a frame granted before stays granted.
            let minimized = screen.minimized();
            if screen.paced() && !minimized {
                if !self.granted && !screen.wait() {
                    continue;
                }
                self.granted = true;
            } else {
                std::thread::sleep(Duration::from_millis(16));
            }
            // The end screen was left (its pages cleared): the page drawn
            // last goes, and nothing is drawn until what comes next, which
            // takes the frame granted.
            if matches!(mode, Mode::End) && self.end_page.0 <= self.shared.end_cleared() {
                self.end_page.1 = EndPage::default();
                mode = Mode::Idle;
                continue;
            }
            // Loading ended during the wait: its frame is not drawn, and
            // nothing is until what comes next, which takes the frame
            // granted.
            if let Mode::Menu {
                loading: Some(lifetime),
                ..
            } = mode
                && !self.shared.loading_current(lifetime)
            {
                mode = Mode::Idle;
                continue;
            }
            let loading_frame = match mode {
                Mode::Menu { loading, .. } => loading,
                _ => None,
            };
            // The launch screen's time is over: the page waiting replaces it.
            let elapsed = Self::launch_elapsed(&self.shared, self.started);
            if elapsed >= SPLASH
                && let Mode::Splash { pending } = &mut mode
            {
                if let Some(page) = pending.take() {
                    mode = Mode::Menu {
                        page,
                        update: None,
                        changed: true,
                        loading: None,
                    };
                } else if self.selection.is_some() {
                    mode = Mode::Selection;
                }
            }
            // Selection was left during the wait: nothing of its update is
            // built or presented, and nothing is drawn until what comes
            // next, which takes the frame granted. Checked here, after the
            // launch screen handed over and right before the build.
            if matches!(mode, Mode::Selection)
                && self
                    .selection
                    .as_ref()
                    .is_none_or(|s| !self.shared.selection_current(s.update.lifetime))
            {
                self.selection = None;
                mode = Mode::Idle;
                continue;
            }
            // The result was left during the wait: nothing of it is drawn.
            if matches!(mode, Mode::Result)
                && self
                    .result
                    .as_ref()
                    .is_none_or(|r| !self.shared.result_slot().current(r.lifetime))
            {
                self.drop_result();
                mode = Mode::Idle;
                continue;
            }
            // A test holds a result frame here, after its lifetime's
            // check: a new lifetime may begin before its commit point.
            #[cfg(test)]
            if matches!(mode, Mode::Result) {
                self.shared.hold_result_frame();
            }
            // The chart tables checked last, after the loading lifetime's:
            // a lifetime begun after a new generation never draws an
            // earlier one's.
            self.sync_chart();
            let Some(screen) = &mut self.screen else {
                continue;
            };
            #[cfg(feature = "dev-tools")]
            let mut view_stamp = None;
            #[cfg(feature = "dev-tools")]
            let mut loading_stamp = None;
            // The frame's one instant: a play frame's chart time and the
            // turntable's state time and tick are all of it.
            let frame_at = self.clock.now();
            // The application's readouts at that instant: the presents of
            // the last second, the local time and the uptime, the clock
            // reading 0 at launch.
            let app = self.app.readouts(frame_at);
            // Whether a result frame was built with its skin (dev-tools:
            // G3's and G5's ends).
            #[cfg(feature = "dev-tools")]
            let mut skin_built = false;
            // A select or result frame's start, and whether it drew the
            // skin (dev-tools: the whole frames, the S7-07d gate).
            #[cfg(feature = "dev-tools")]
            let mut frame_began: Option<Instant> = None;
            #[cfg(feature = "dev-tools")]
            let mut skin_frame = false;
            // When a select or result skin frame's present returned, taken
            // before any cleanup (dev-tools: matched against the capture).
            #[cfg(feature = "dev-tools")]
            let mut presented_at: Option<TimeUs> = None;
            // A skinned decision frame the skin refused, for its takeover.
            let mut decision_refused = None;
            let drawn = match &mut mode {
                Mode::Idle => continue,
                Mode::Splash { pending } => {
                    let Some(drawn) = screen.splash(elapsed) else {
                        // No launch screen: the page waiting, if any, at
                        // once; else the frame granted waits for what
                        // comes next.
                        mode = match pending.take() {
                            Some(page) => Mode::Menu {
                                page,
                                update: None,
                                changed: true,
                                loading: None,
                            },
                            None if self.selection.is_some() => Mode::Selection,
                            None => Mode::Idle,
                        };
                        continue;
                    };
                    drawn.map(|presented| {
                        let timing = FrameTiming {
                            cpu: Duration::ZERO,
                            sprites: 0,
                            dropped: 0,
                            presented,
                            present_count: None,
                        };
                        (timing, None)
                    })
                }
                Mode::Play {
                    origin,
                    state_origin,
                    side,
                    gauge,
                    first,
                    rearms,
                    skipped,
                } => {
                    // The app region's first interval (design.md "Scope of
                    // No Allocation"): this frame's own work up to the
                    // build, before anything calls into wgpu. It ends
                    // before the build's outcome is looked at, and on any
                    // other way out.
                    #[cfg(feature = "dev-tools")]
                    let region = crate::alloc_count::AppRegion::enter();
                    effects.clear();
                    // One coherent acquisition (design.md "Rendering and
                    // Skins", the note snapshot): the newest view, its
                    // candidate's generation and origin, and that
                    // candidate's effects within the view's events, at most
                    // what the buffer holds; the rest wait for the next
                    // frame, and a cancelled candidate's are discarded. A
                    // rearm the view does not show yet, or one during the
                    // drain, builds no frame: the next try takes the newer
                    // candidate, its frame granted kept, at once for the
                    // first few and then after a short wait for a message
                    // ([`ACQUIRE_RETRIES`]).
                    let room = effects.capacity();
                    let Some(view) = side.acquire(room, |e| effects.push(*e)) else {
                        *skipped = skipped.saturating_add(1);
                        continue;
                    };
                    *skipped = 0;
                    #[cfg(feature = "dev-tools")]
                    {
                        view_stamp = Some(crate::measure::ViewStamp {
                            generation: view.generation,
                            finalized: view.finalized,
                            events: view.events,
                            playing: view.playing,
                            complete: view.complete,
                            closing: view.closing,
                            ready: self.ready,
                            tally: view.tally,
                            shown_gauge: *gauge,
                            gauges: crate::measure::gauge_values(&view.gauges),
                            look: self
                                .gauge_looks
                                .as_ref()
                                .and_then(|l| l.get(*gauge).copied()),
                        });
                    }
                    // A rearmed READY candidate is drawn from its own origin,
                    // the skin's runtime starting over first.
                    if view.generation != *rearms {
                        *rearms = view.generation;
                        screen.rearm();
                        self.turntable.play_starts();
                    }
                    let origin = view.origin.unwrap_or(*origin);
                    // The frame's one sample gives the chart time and the
                    // state clock.
                    let now = TimeUs(frame_at.0 - origin.0);
                    let state_us = frame_at.0 - state_origin.0;
                    // The adjustments change anytime and move the notes
                    // only: one snapshot per frame gives the lanes, the
                    // readouts and the display offset together, and nothing
                    // of it reaches judgment.
                    let (lanes, readouts, display_offset) =
                        play_presentation(self.presentation.as_mut(), self.current.as_ref(), now);
                    let overlay = self.measure.overlay();
                    let shown = overlay.iter().take_while(|l| !l.is_empty()).count();
                    #[expect(
                        clippy::indexing_slicing,
                        reason = "`shown` counts a prefix of `overlay`"
                    )]
                    let f = FrameInput {
                        now,
                        display_offset,
                        state_us,
                        lanes,
                        readouts,
                        view,
                        effects: &effects,
                        gauge: *gauge,
                        overlay: &overlay[..shown],
                        target: screen.target(),
                        turntable: self.turntable.angle(),
                        app,
                    };
                    let built = screen.build(&f);
                    #[cfg(feature = "dev-tools")]
                    drop(region);
                    if let Err(e) = built {
                        // The skin refused the frame: nothing of it is
                        // drawn, and no frame of this attempt is built
                        // again. The attempt stops as on the screen's
                        // failure, but the screen, its device and its
                        // resources stay, for the next preparation.
                        self.measure.failed(&effects);
                        eprintln!("render: {}", Chain(&e));
                        self.shared.failed.store(true, Ordering::Release);
                        mode = Mode::Idle;
                        continue;
                    }
                    let drawn = screen.draw().map(|timing| (timing, Some(now)));
                    // The first frame that reached the screen.
                    if matches!(&drawn, Ok((t, _)) if t.presented) && *first {
                        *first = false;
                        self.shared
                            .first_frame
                            .store(self.clock.now().0, Ordering::Release);
                    }
                    drawn
                }
                Mode::Menu {
                    page,
                    update,
                    changed,
                    ..
                } => {
                    // A screen built again after a failure lays the page
                    // out again.
                    #[cfg(feature = "dev-tools")]
                    let [_, _, _, before] = crate::alloc_count::counts();
                    let pending = loading_frame.is_some_and(|l| {
                        pending_presentation(&self.committed, self.specialized_in, l)
                    });
                    let sampled = self.clock.now();
                    #[cfg(feature = "dev-tools")]
                    {
                        loading_stamp = loading_frame.map(|_| crate::measure::LoadingStamp {
                            sampled,
                            origin: update.as_deref().map_or(sampled, |u| u.origin),
                            settled: !pending,
                        });
                    }
                    let drawn = if pending {
                        // No specialization committed in this lifetime:
                        // the load's lines on a cleared canvas.
                        screen.pending_loading(page, *changed)
                    } else if let Some(lifetime) = loading_frame {
                        let snapshot = self.presentation.as_mut().map(|r| *r.read());
                        let frame = loading_frame_of(
                            lifetime,
                            sampled,
                            update.as_deref(),
                            snapshot.as_ref(),
                            self.turntable.angle(),
                            app,
                        );
                        match screen.loading(page, update.as_deref(), *changed, &frame) {
                            Ok(Ok(t)) => Ok(t),
                            Ok(Err(e)) => {
                                // Refused: nothing of it was drawn, the
                                // screen stays, and nothing of the
                                // lifetime is drawn again; the frame
                                // granted waits for what comes next.
                                self.loading_refused = Some((lifetime, e));
                                self.shared.refused_loading(lifetime, e);
                                eprintln!("render: the loading frame was refused: {}", Chain(&e));
                                mode = Mode::Idle;
                                continue;
                            }
                            Err(e) => Err(e),
                        }
                    } else {
                        screen.menu(page, *changed)
                    }
                    .map(|t| (t, None));
                    // The lifetime's first presented frame starts the
                    // loading state's minimum (design.md "Play Lifecycle").
                    if let (Some(lifetime), Ok((t, _))) = (loading_frame, &drawn)
                        && t.presented
                    {
                        let loadend = if pending {
                            None
                        } else {
                            self.committed
                                .get(ScreenKind::Play)
                                .and_then(|i| i.bundle().skin().loadend())
                        };
                        let clock = self.clock;
                        self.shared
                            .presented_loading(lifetime, loadend, || clock.now());
                    }
                    #[cfg(feature = "dev-tools")]
                    if let Ok((t, _)) = &drawn {
                        let [_, _, _, after] = crate::alloc_count::counts();
                        let stats = if loading_frame.is_some() {
                            &mut self.loading_stats
                        } else {
                            &mut self.menu_stats
                        };
                        stats.frame(t.cpu, after - before, *changed);
                        if loading_frame.is_some() && t.presented {
                            self.shared.loading_drawn.fetch_add(1, Ordering::Relaxed);
                        }
                        // What a refusal put on the screen, once drawn.
                        if *changed
                            && let Some(notice) = page.lines.iter().find(|l| {
                                l.style == crate::menu_screen::LineStyle::Notice
                                    && !l.text.is_empty()
                            })
                        {
                            println!("dev: drawn notice: {}", notice.text);
                        }
                    }
                    *changed = false;
                    drawn
                }
                Mode::Selection => {
                    #[cfg(test)]
                    if self.shared.selection_stall.load(Ordering::Acquire) {
                        std::thread::sleep(Duration::from_millis(2));
                    }
                    #[cfg(feature = "dev-tools")]
                    {
                        frame_began = Some(Instant::now());
                        let _ = crate::dev::take_build_mark();
                        let _ = crate::screens::take_visit();
                    }
                    let Some(shown) = self.selection.as_mut() else {
                        mode = Mode::Idle;
                        continue;
                    };
                    Self::selection_logic(
                        &self.committed,
                        shown,
                        &mut self.selection_states,
                        &self.scripts,
                    );
                    #[cfg(feature = "dev-tools")]
                    let [_, _, _, before] = crate::alloc_count::counts();
                    // The settings the skin shows, the volume and the
                    // display offset as they are now: F3 to F6 show at the
                    // next frame, with no view of the game thread's.
                    if let Some(logic) = shown.logic.as_mut() {
                        logic
                            .apply_settings(shown.update.settings.as_ref(), self.tuning.as_deref());
                    }
                    // The frame's one reading: its timers age on the instant
                    // the readouts were sampled at.
                    let changed = std::mem::replace(&mut shown.changed, false);
                    let drawn = screen.selection(
                        &shown.update,
                        shown.logic.as_mut(),
                        changed,
                        frame_at,
                        app,
                    );
                    #[cfg(feature = "dev-tools")]
                    {
                        presented_at = Some(self.clock.now());
                        crate::dev::mark_presented();
                    }
                    if let Ok(SelectionDrawn {
                        refused: Some(why), ..
                    }) = &drawn
                    {
                        // A runtime refusal on a healthy device is for the
                        // rest of the run: the installation goes from the
                        // screen and from what a screen built again
                        // restores, and the game thread's next look shows
                        // why; this frame drew the page.
                        eprintln!(
                            "render: the selection skin was refused, the built-in page draws from now on: {why}"
                        );
                        screen.remove(ScreenKind::Selection);
                        self.committed.clear(ScreenKind::Selection);
                        // Parked as the state goes, then retired.
                        shown.logic = None;
                        self.scripts.retire_kind(ScreenKind::Selection);
                        *self
                            .shared
                            .selection_refused
                            .lock()
                            .unwrap_or_else(PoisonError::into_inner) = Some(why.clone());
                    }
                    #[cfg(feature = "dev-tools")]
                    if let Ok(d) = &drawn {
                        let [_, _, _, after] = crate::alloc_count::counts();
                        self.menu_stats.frame(d.timing.cpu, after - before, changed);
                        skin_frame = shown.logic.is_some() && d.refused.is_none();
                        // What a refusal put on the screen, once drawn.
                        if changed
                            && let Some(notice) = shown.update.page.lines.iter().find(|l| {
                                l.style == crate::menu_screen::LineStyle::Notice
                                    && !l.text.is_empty()
                            })
                        {
                            println!("dev: drawn notice: {}", notice.text);
                        }
                    }
                    drawn.map(|d| (d.timing, None))
                }
                Mode::Result => {
                    #[cfg(feature = "dev-tools")]
                    {
                        frame_began = Some(Instant::now());
                        let _ = crate::dev::take_build_mark();
                        let _ = crate::screens::take_visit();
                    }
                    let now = self.clock.now();
                    if let Some(side) = &mut self.result {
                        side.drain();
                    }
                    let fadeout = Self::result_commit(
                        screen.as_mut(),
                        self.result.as_mut(),
                        &self.shared,
                        &self.scripts,
                    );
                    Self::result_visit(self.result.as_mut(), now, fadeout);
                    let built =
                        Self::result_build(screen.as_mut(), self.result.as_mut(), now, fadeout);
                    // A frame under the barrier refused (dev-tools: the
                    // fallback during an outro).
                    #[cfg(feature = "dev-tools")]
                    let built = match built {
                        ResultDrawn::Built
                            if self.shared.barrier().fadeout.is_some()
                                && crate::dev::refuse_pinned_frame() =>
                        {
                            ResultDrawn::Refused(skin_model::BuildError::Kind)
                        }
                        other => other,
                    };
                    #[cfg(feature = "dev-tools")]
                    {
                        skin_built = matches!(built, ResultDrawn::Built);
                    }
                    // A view committed: what the kind keeps to restore
                    // follows it now, whatever the submission does.
                    if let Some(side) = &mut self.result
                        && std::mem::take(&mut side.reconcile)
                    {
                        self.reconcile_result();
                    }
                    let Some(screen) = &mut self.screen else {
                        continue;
                    };
                    let page = |side: Option<&ResultSide>, latest: &EndPage| match side
                        .and_then(|s| s.presented.as_ref())
                        .and_then(|h| h.page.as_ref())
                    {
                        Some(p) => p.clone(),
                        None => latest.clone(),
                    };
                    match built {
                        // A device lost under the barrier (dev-tools: a
                        // recovery during an outro).
                        #[cfg(feature = "dev-tools")]
                        ResultDrawn::Built
                            if self.shared.barrier().fadeout.is_some()
                                && crate::dev::lose_pinned_frame() =>
                        {
                            Err(RenderError::SurfaceLost)
                        }
                        ResultDrawn::Built => {
                            let submitted = screen.submit_result();
                            #[cfg(feature = "dev-tools")]
                            {
                                presented_at = Some(self.clock.now());
                                crate::dev::mark_presented();
                            }
                            submitted.map(|t| (t, None))
                        }
                        ResultDrawn::BuiltIn => screen
                            .end(&page(self.result.as_ref(), &self.end_page.1))
                            .map(|p| (timing_of(p), None)),
                        ResultDrawn::Refused(e) => {
                            // Refused on a healthy device: the built-in page
                            // draws in this frame, and the result kind is
                            // disabled for the run.
                            // The disable composed the view presented's page.
                            self.disable_result(&e.to_string());
                            let page = page(self.result.as_ref(), &self.end_page.1);
                            let Some(screen) = &mut self.screen else {
                                continue;
                            };
                            screen.end(&page).map(|p| (timing_of(p), None))
                        }
                    }
                }
                Mode::Decision => {
                    let (Some(shown), Some(dec)) =
                        (self.decision_shown.as_ref(), self.decision_screen.as_mut())
                    else {
                        mode = Mode::Idle;
                        continue;
                    };
                    let active = render::lock(&self.shared.labels).active();
                    let built = crate::decision_side::decision_build(
                        screen.as_mut(),
                        dec,
                        self.decision_side
                            .as_ref()
                            .map(crate::decision_side::DecisionSide::store),
                        active,
                        shown,
                        frame_at,
                    );
                    match built {
                        crate::decision_side::DecisionDrawn::Built => {
                            screen.submit_decision().map(|t| (t, None))
                        }
                        other => {
                            // Refused on a healthy device: the page draws in
                            // this frame, and the built-in presentation takes
                            // over from now on. A screen whose lifetime ended
                            // draws the page, refusing nothing.
                            if let crate::decision_side::DecisionDrawn::Refused(e) = other {
                                decision_refused = Some(e);
                            }
                            let Some(page) = &self.decision else {
                                mode = Mode::Idle;
                                continue;
                            };
                            screen.menu(page, true).map(|t| (t, None))
                        }
                    }
                }
                Mode::End => screen.end(&self.end_page.1).map(|p| (timing_of(p), None)),
            };
            // The skin refused its decision frame, which drew the page: the
            // built-in presentation takes over for the rest of the screen.
            if let Some(e) = decision_refused {
                self.decision_frame_refused(e, frame_at);
                if matches!(mode, Mode::Decision) {
                    mode = Mode::Menu {
                        page: self.decision.clone().unwrap_or_default(),
                        update: None,
                        changed: false,
                        loading: None,
                    };
                }
            }
            // Any other frame's present: right after its draw returned.
            #[cfg(feature = "dev-tools")]
            let presented_at = presented_at.unwrap_or_else(|| self.clock.now());
            if !minimized {
                self.granted = false;
            }
            // Every screen's frame that reached the screen counts towards
            // the frames per second.
            if let Ok((timing, _)) = &drawn
                && timing.presented
            {
                self.app.presented(self.clock.now());
            }
            // The play state's turntable ticks on each presented frame of
            // the practice page, loading and play, after it drew; a
            // generic page's frames are no play state's.
            let turns = match &mode {
                Mode::Play { .. } => true,
                Mode::Menu { loading, .. } => loading.is_some() || self.page_turns,
                _ => false,
            };
            if let Ok((timing, chart)) = &drawn
                && timing.presented
                && turns
            {
                // A play frame's tick is the app's work, measured as its
                // build is.
                #[cfg(feature = "dev-tools")]
                let region = chart.is_some().then(crate::alloc_count::AppRegion::enter);
                let scratch = self
                    .presentation
                    .as_mut()
                    .map_or(game::ScratchInput::NONE, |r| r.read().scratch);
                self.turntable.presented(frame_at, *chart, scratch);
                #[cfg(feature = "dev-tools")]
                drop(region);
            }
            if matches!(mode, Mode::Result) && drawn.is_ok() {
                self.after_result_frame();
            }
            #[cfg(feature = "dev-tools")]
            if skin_built && drawn.as_ref().is_ok_and(|(t, _)| t.presented) {
                self.probe_result_frame();
            }
            #[cfg(feature = "dev-tools")]
            if matches!(mode, Mode::Result) {
                skin_frame = skin_built;
            }
            let Some(screen) = &mut self.screen else {
                continue;
            };
            match drawn {
                Ok((timing, play)) => {
                    let d = Drawn {
                        play,
                        #[cfg(feature = "dev-tools")]
                        view: view_stamp,
                        #[cfg(feature = "dev-tools")]
                        loading_stamp,
                        // The measured play's: none once its figures are
                        // taken, so no frame after a `Finish` is its.
                        #[cfg(feature = "dev-tools")]
                        generation: self.measure.generation(),
                        #[cfg(feature = "dev-tools")]
                        menu: matches!(
                            mode,
                            Mode::Menu { .. } | Mode::Decision | Mode::Selection | Mode::Result
                        ),
                        #[cfg(feature = "dev-tools")]
                        loading: matches!(
                            mode,
                            Mode::Menu {
                                loading: Some(_),
                                ..
                            }
                        ),
                        #[cfg(feature = "dev-tools")]
                        skin_loading: loading_frame.filter(|&l| {
                            !pending_presentation(&self.committed, self.specialized_in, l)
                        }),
                        timing,
                        statistics: screen.statistics(),
                        presentation: screen.presentation(),
                        effects: if play.is_some() { &effects } else { &[] },
                        at: self.clock.now(),
                    };
                    // Every present, for `tapestry measure select`
                    // (dev-tools).
                    #[cfg(feature = "dev-tools")]
                    if timing.presented {
                        self.present_log.present(presented_at.0);
                    }
                    // A select or result skin frame's spans and present
                    // (dev-tools: the whole frames, the S7-07d gate).
                    #[cfg(feature = "dev-tools")]
                    if let Some(began) = frame_began.filter(|_| skin_frame)
                        && let Some(built) = crate::dev::take_build_mark()
                        && let Some(lifetime) = if matches!(mode, Mode::Result) {
                            self.result.as_ref().map(|r| r.lifetime)
                        } else {
                            self.selection.as_ref().map(|s| s.update.lifetime)
                        }
                    {
                        let result = matches!(mode, Mode::Result);
                        let switch = result
                            && crate::dev::view_switched(
                                &mut self.frames_view,
                                self.result
                                    .as_ref()
                                    .and_then(|s| s.presented.as_ref())
                                    .map(|h| h.payload.key),
                            );
                        let frames = if result {
                            &mut self.result_frames
                        } else {
                            &mut self.select_frames
                        };
                        let visit = crate::screens::take_visit();
                        let marks = crate::dev::FrameMarks {
                            before_wait: frames.take_before_wait(),
                            began,
                            built,
                        };
                        let spans = crate::dev::Spans::of(
                            marks,
                            visit.span.saturating_add(visit.collect_span),
                            timing.cpu,
                        );
                        let presented = crate::dev::Presented {
                            at: timing.presented.then_some(presented_at.0),
                            statistics: d.statistics,
                            presentation: d.presentation,
                        };
                        if !frames.frame(lifetime, spans, visit, switch, presented) {
                            // Another lifetime's frames still held: they end
                            // here (a lifetime change the loop did not see).
                            frames.report(
                                if result { "result" } else { "select" },
                                self.clock.frequency().hz(),
                            );
                            frames.frame(lifetime, spans, visit, switch, presented);
                        }
                    } else if matches!(mode, Mode::Result) {
                        // A result frame not recorded (the built-in page,
                        // a refusal) ends the preparation charged before
                        // its wait: none of it is the next frame's.
                        let _ = self.result_frames.take_before_wait();
                    }
                    // The app region's second interval: a play frame's
                    // measurement. The frame sink (the frame dump) stays
                    // outside it.
                    #[cfg(feature = "dev-tools")]
                    let region = play.is_some().then(crate::alloc_count::AppRegion::enter);
                    self.measure.frame(&d);
                    #[cfg(feature = "dev-tools")]
                    drop(region);
                    self.frames.frame(screen.as_ref(), &d);
                    // The end page drawn, for a script waiting on it
                    // (dev-tools).
                    // A result's frame counts once nothing newer of it waits
                    // to be prepared or committed.
                    #[cfg(feature = "dev-tools")]
                    if timing.presented
                        && (matches!(mode, Mode::End)
                            || matches!(mode, Mode::Result)
                                && self
                                    .result
                                    .as_ref()
                                    .is_none_or(|r| r.incoming.is_none() && r.prepared.is_none()))
                    {
                        self.shared
                            .end_drawn
                            .store(self.end_page.0, Ordering::Release);
                    }
                    // A select screen frame drawn, for a script waiting on
                    // one (dev-tools).
                    #[cfg(feature = "dev-tools")]
                    if matches!(mode, Mode::Selection) && timing.presented {
                        self.shared.selection_drawn.fetch_add(1, Ordering::AcqRel);
                    }
                }
                Err(e) => {
                    // What this frame drew never reached the screen.
                    if matches!(mode, Mode::Play { .. }) {
                        self.measure.failed(&effects);
                    }
                    eprintln!("render: {}", Chain(&e));
                    self.drop_screen();
                    self.shared.failed.store(true, Ordering::Release);
                    mode = Mode::Idle;
                }
            }
        }
    }

    /// The mode a page sent now draws in: the launch screen shows out its
    /// time with the page waiting, else the page.
    fn menu_mode(mode: Mode, page: Box<MenuPage>, shared: &Shared, started: Instant) -> Mode {
        match mode {
            Mode::Splash { .. } if Self::launch_elapsed(shared, started) < SPLASH => Mode::Splash {
                pending: Some(page),
            },
            _ => Mode::Menu {
                page,
                update: None,
                changed: true,
                loading: None,
            },
        }
    }

    /// The answer to the end or pause of loading lifetime `lifetime`:
    /// whether the skin refused a loading frame of it, else whether the
    /// screen prepared for the attempt is still there.
    fn loading_end(&self, lifetime: u64) -> LoadingEnd {
        match (self.loading_refused, &self.screen) {
            (Some((l, e)), _) if l == lifetime => LoadingEnd::Refused(e),
            (_, None) => LoadingEnd::Lost,
            (_, Some(_)) => LoadingEnd::Ready,
        }
    }

    /// Publishes what skins are packed for on the screen just built.
    fn built(&self, screen: &dyn Screen) {
        self.shared.built(screen);
    }

    /// Builds the screen again after a failure, without a chart: the
    /// device's recovery. A screen that can draw is kept as it is.
    fn recover(&mut self) -> Result<(), RenderError> {
        if self.screen.is_none() {
            self.granted = false;
            // The committed bundle, from memory: never read again from its
            // files, never another skin in its place.
            let disabled = self.result_disabled.is_some();
            let (mut refused, mut decision_refused) = (None, None);
            let built = build(
                &mut self.make,
                &mut self.committed,
                None,
                None,
                disabled,
                &mut refused,
                &mut decision_refused,
            );
            // A restore refused stands for the run, whatever the build.
            if let Some(why) = refused {
                self.disable_result(&why);
            }
            if let Some(why) = &decision_refused {
                self.disable_decision(why.clone());
            }
            let screen = built?;
            self.built(screen.as_ref());
            self.screen = Some(screen);
            self.result_rebuilt();
            // The new screen takes the window's size afresh.
            self.applied = None;
            self.decision_rebuilt(decision_refused)?;
        }
        self.shared.failed.store(false, Ordering::Release);
        Ok(())
    }

    /// Starts preparing the chart loaded last for request `request`'s
    /// `attempt` with `skin`, recovering the screen first: whatever an
    /// earlier request staged is discarded, and the preparation is asked
    /// of the screen at once and between frames until it answers
    /// ([`poll_attempt`](Self::poll_attempt)). The skin in use while
    /// play's skin is pending is refused once the earlier staging is
    /// discarded, the screen not asked.
    fn begin_attempt(
        &mut self,
        request: u64,
        attempt: Box<AttemptContext>,
        skin: AttemptSkin,
        reply: mpsc::SyncSender<Result<Answered, RenderError>>,
    ) {
        // An older request than one asked already changes nothing.
        if !self.newest(request) {
            let _ = reply.send(Ok(self.answered(refusal("a newer request superseded it"))));
            return;
        }
        // An older preparation, answered or not, and whatever it staged.
        self.supersede_attempts("a newer request superseded it");
        let skin = match skin {
            AttemptSkin::Specialized(bundle) => Some(bundle),
            AttemptSkin::InUse if self.committed.pending(ScreenKind::Play) => {
                let _ = reply.send(Ok(
                    self.answered(refusal("the play skin awaits its specialization"))
                ));
                return;
            }
            AttemptSkin::InUse => self.selected.clone(),
        };
        // The installation the attempt draws with: the bundle it stages,
        // or the one installed, which an attempt with none staged uses in
        // place.
        let installed = crate::carry::Installed::of(skin.as_ref().or_else(|| {
            self.committed
                .get(ScreenKind::Play)
                .map(render::Installation::bundle)
        }));
        let offer = self.carries.offer(attempt.carry_key(), &installed);
        let p = Preparing {
            request,
            attempt,
            skin,
            offer,
            installed,
            reply,
        };
        if let Err(e) = self.recover() {
            let _ = p.reply.send(Err(e));
            return;
        }
        self.attempt = Some(p);
        self.poll_attempt();
    }

    /// Asks the screen for the answer of the preparation in progress, and
    /// sends it once there is one: ready, it is kept for its commit. A
    /// refusal keeps the screen and reports no failure; an error drops it,
    /// to be built again. A screen dropped meanwhile is built again first.
    fn poll_attempt(&mut self) {
        if self.attempt.is_none() {
            return;
        }
        if self.screen.is_none()
            && let Err(e) = self.recover()
        {
            self.answer(Err(e));
            return;
        }
        if self.chart.is_none() {
            self.answer(Ok(refusal("no chart is loaded")));
            return;
        }
        let (Some(p), Some(screen), Some(chart)) =
            (&mut self.attempt, &mut self.screen, self.chart.as_deref())
        else {
            return;
        };
        let base = p.attempt.context();
        // A declarative skin's lanes are laid out for its own scratch side.
        let context = match &p.skin {
            Some(bundle) => PlayContext {
                scratch: bundle.scratch(),
                ..base
            },
            None => base,
        };
        let text = p.attempt.text();
        let empty_poor_counted = context.empty_poor_counted;
        // The attempt's target count, which its point is over.
        let targets = skin_model::attempt_targets(chart, &context.attempt);
        let a = AttemptInput {
            chart,
            context: &context,
            text: &text,
            target: screen.target(),
            continuation: p.offer.continuation(),
        };
        #[cfg(feature = "dev-tools")]
        crate::dev::hold_preparing();
        let Some(answer) = screen.prepare_attempt(&a, p.skin.as_ref(), p.request) else {
            return;
        };
        if matches!(answer, Ok(Preparation::Ready)) {
            #[cfg(test)]
            self.shared.epochs[0].store(p.attempt.epoch(), Ordering::SeqCst);
            self.prepared = Some(Prepared {
                token: PreparedToken {
                    request: p.request,
                    device: screen.device(),
                    kind: ScreenKind::Play,
                },
                screen: game::ScreenKey {
                    device: screen.device(),
                    target: screen.target(),
                    skin: self.play_selection,
                },
                declarative: p.skin.is_some(),
                presentation: (p.attempt.tempo().clone(), p.attempt.epoch()),
                scratch_notes: p.attempt.take_scratch_notes(),
                carry: (
                    p.attempt.carry_key(),
                    p.offer.clone(),
                    p.installed.clone(),
                    crate::carry::Basis {
                        targets,
                        empty_poor_counted,
                    },
                ),
                #[cfg(feature = "dev-tools")]
                ready: p.attempt.ready(),
                #[cfg(feature = "dev-tools")]
                gauge_looks: p.attempt.gauge_looks(),
            });
        }
        if answer.is_err() {
            self.drop_screen();
            self.granted = false;
        } else {
            self.reconcile_result();
        }
        self.answer(answer);
    }

    /// Sends the preparation in progress its `answer`, and ends it.
    fn answer(&mut self, answer: Result<Preparation, RenderError>) {
        let answer = answer.map(|preparation| self.answered(preparation));
        if let Some(p) = self.attempt.take() {
            let _ = p.reply.send(answer);
        }
    }

    /// `preparation` with the screen's structural inputs: those the
    /// staging holds when it is ready, else the screen's now.
    fn answered(&self, preparation: Preparation) -> Answered {
        let screen = match (&preparation, &self.prepared) {
            (Preparation::Ready, Some(p)) => p.screen,
            _ => self.screen_key(),
        };
        Answered {
            preparation,
            screen,
        }
    }

    /// The screen's structural inputs now: its device, its target size and
    /// the play skin selection in use; the defaults with no screen.
    fn screen_key(&self) -> game::ScreenKey {
        game::ScreenKey {
            device: self.screen.as_ref().map_or(0, |s| s.device()),
            target: self.screen.as_ref().map_or([0, 0], |s| s.target()),
            skin: self.play_selection,
        }
    }

    /// The final readiness of `request` ([`RenderMsg::Ready`]): its staging
    /// stamped with `epoch` when it still fits the screen's structural
    /// inputs, else `Stale` with them. Bounded: it prepares nothing.
    fn ready(&mut self, request: u64, epoch: u64) -> ReadyAnswer {
        let now = self.screen_key();
        match &mut self.prepared {
            Some(p) if p.token.request == request && p.screen == now => {
                p.presentation.1 = epoch;
                ReadyAnswer::Ready
            }
            _ => ReadyAnswer::Stale(now),
        }
    }

    /// The present a chart publication is checked against
    /// ([`crate::chart_images::PublicationKey::current`]): the game
    /// thread's generation, and this thread's live targets.
    fn chart_now(&self) -> crate::chart_images::ChartNow {
        crate::chart_images::ChartNow {
            generation: self.shared.chart_generation.load(Ordering::SeqCst),
            staged: self.prepared.as_ref().map(|p| p.token.request),
            committed: self.screen.as_ref().and_then(|s| s.chart_key()),
        }
    }

    /// Keeps the chart tables current, before every frame and
    /// publication: a committed table no longer current (another
    /// generation, its installation or array replaced) is made
    /// unavailable with the staged one, so no frame draws a table of an
    /// earlier choice or attempt; a staged one whose staging is gone is
    /// forgotten. The live targets are mirrored for the game thread.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn sync_chart(&mut self) {
        let now = self.chart_now();
        // The committed table serves no staging; the staged one the live
        // staging's request.
        let stale = |k: &Option<crate::chart_images::PublicationKey>, attempt: Option<u64>| {
            k.is_some_and(|k| !k.current(&now, attempt))
        };
        if self.chart_generation != now.generation || stale(&self.chart_table, None) {
            if let Some(screen) = &mut self.screen {
                screen.invalidate_chart_images();
            }
            self.chart_generation = now.generation;
            self.chart_table = None;
            self.staged_table = None;
        }
        if stale(&self.staged_table, now.staged) {
            self.staged_table = None;
        }
        let targets = (now.staged, now.committed);
        if self.chart_targets != targets {
            self.chart_targets = targets;
            *self
                .shared
                .chart_targets
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = targets;
        }
    }

    /// Publishes `set` (absent when `None`) of `generation` for the
    /// attempt of `request`: for `None` into the committed play
    /// installation; for a staging, into its array, or, when it has none of
    /// its own (a pending skin's specialization reused on the installation
    /// in use), into the committed installation it draws from (S10-12
    /// design §5.3). The publication is admitted only while its key is
    /// current ([`crate::chart_images::PublicationKey::current`]): another
    /// generation, or a request whose staging is gone, publishes nothing
    /// anywhere. The answer says what it was for and where it went.
    fn publish_chart(
        &mut self,
        generation: u64,
        request: Option<u64>,
        set: Option<&crate::chart_images::ChartImages>,
    ) -> crate::chart_images::Ack {
        use crate::chart_images::{Ack, PublicationKey, PublishedTo};
        #[cfg(test)]
        {
            let end = Instant::now() + hook_bound();
            while self.shared.chart_hold.load(Ordering::SeqCst) && Instant::now() < end {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        self.sync_chart();
        let now = self.chart_now();
        let key = PublicationKey {
            generation,
            request,
            target: match request {
                Some(r) => PublishedTo::Staged(r),
                None => PublishedTo::Committed(now.committed),
            },
        };
        let unpublished = Ack {
            key,
            answer: Err(render::ChartOutcome::Unavailable),
        };
        if !key.current(&now, request) {
            return unpublished;
        }
        let target = match (request, self.prepared.as_ref()) {
            (None, _) => render::ChartTarget::Committed,
            (Some(_), Some(p)) => render::ChartTarget::Staged(p.token),
            (Some(_), None) => return unpublished,
        };
        let Some(screen) = &mut self.screen else {
            return unpublished;
        };
        let publish = |screen: &mut Box<dyn Screen>, target| match set {
            Some(set) => screen.publish_chart_images(target, set.fitted()),
            None => screen.publish_chart_absent(target),
        };
        let mut answer = publish(screen, target);
        // The admitted staging draws from the committed installation when
        // it has no array of its own: its table is published there.
        let committed = request.is_none() || matches!(answer, Err(RenderError::NoChartLayer));
        if request.is_some() && committed {
            answer = publish(screen, render::ChartTarget::Committed);
        }
        // What a publication left in its target: a table, or an all-absent
        // one; nothing for the other failures.
        let left = answer.is_ok()
            || answer
                .as_ref()
                .is_err_and(|e| e.chart_outcome() == render::ChartOutcome::Absent);
        if left && committed {
            self.chart_table = Some(PublicationKey {
                generation,
                request: None,
                target: PublishedTo::Committed(now.committed),
            });
        } else if left {
            self.staged_table = Some(key);
        }
        // The game thread logs what it left; this thread draws frames and
        // logs nothing.
        Ack {
            key,
            answer: answer.map_err(|e| e.chart_outcome()),
        }
    }

    /// Installs what the preparation of `request` staged: it must be the
    /// one answered ready last, not withdrawn by a game thread that stopped
    /// waiting, on the screen it was prepared on (its device's). Another request's commit changes nothing; one refused by
    /// the screen discards the staging. Committed, a declarative bundle is
    /// kept for a screen built again. The receipt taken under the
    /// commits' lock is answered: what the declarative installation the
    /// attempt draws with, staged or in use, declares.
    fn commit(&mut self, request: u64) -> Result<Declared, CommitError> {
        // The loading lifetime current, if one is begun: a pending skin's
        // specialization committed now draws its loading frames. Read
        // before the commits' lock, which no loading lock nests in.
        let lifetime = {
            let producer = self.shared.producer();
            let current = self.shared.loading_slot().lifetime;
            producer
                .as_ref()
                .filter(|s| s.lifetime() == current)
                .map(LifetimeState::lifetime)
        };
        // Held through the installation: a withdrawal waits for it, or it
        // sees the withdrawal and installs nothing.
        let shared = Arc::clone(&self.shared);
        let mut commits = shared.commits();
        if commits.withdrawn >= request {
            self.discard(request);
            return Err(CommitError::NothingStaged);
        }
        let Some(staged) = self.prepared.as_ref().map(|p| p.token.request) else {
            return Err(CommitError::NothingStaged);
        };
        if staged != request {
            return Err(CommitError::Request {
                staged,
                asked: request,
            });
        }
        // The staged chart table, current for this request while it is
        // staged, moves into the committed installation with it.
        let before = self.chart_now();
        let moving = self
            .staged_table
            .take()
            .filter(|k| k.current(&before, Some(request)));
        let Some(p) = self.prepared.take() else {
            return Err(CommitError::NothingStaged);
        };
        let Some(screen) = &mut self.screen else {
            return Err(CommitError::Device);
        };
        if let Err(e) = screen.commit(p.token) {
            screen.discard();
            return Err(e);
        }
        // Its key is the committed layer's now, the generation's whatever
        // attempt draws it.
        if let Some(k) = moving {
            self.chart_table = Some(crate::chart_images::PublicationKey {
                request: None,
                target: crate::chart_images::PublishedTo::Committed(screen.chart_key()),
                ..k
            });
        }
        // A pending skin's specialization committed in the current loading
        // lifetime declares that lifetime's loading time from now on.
        let mut declared = None;
        if p.declarative && self.committed.pending(ScreenKind::Play) {
            // A pending skin's specialization: kept pending, drawn by the
            // loading frames of the lifetime it was committed in.
            self.committed
                .pend(ScreenKind::Play, screen.installation(ScreenKind::Play));
            self.specialized_in = lifetime;
            if lifetime.is_some() {
                declared = Some(
                    self.committed
                        .get(ScreenKind::Play)
                        .and_then(|i| i.bundle().skin().loadend()),
                );
            }
        } else if p.declarative {
            self.committed
                .set(ScreenKind::Play, screen.installation(ScreenKind::Play));
        }
        // The installation the attempt draws with: the one it staged, or
        // the one in use it was readied on.
        let drawn = Declared::of(
            self.committed
                .get(ScreenKind::Play)
                .map(|i| i.bundle().skin().as_ref()),
        );
        #[cfg(test)]
        self.shared.epochs[1].store(p.presentation.1, Ordering::SeqCst);
        // The committed attempt's `Play` is the screen's now, whatever
        // other kinds commit until the next load, failure or commit.
        let (key, offer, installed, basis) = &p.carry;
        self.carries.on(crate::carry::Event::Committed {
            key: *key,
            offer,
            installed: installed.clone(),
            basis: *basis,
            bank: screen.carry().map(|c| c.bank),
        });
        self.current = Some(p.presentation);
        self.turntable.attempt(p.scratch_notes);
        #[cfg(feature = "dev-tools")]
        {
            self.ready = Some(p.ready);
            self.gauge_looks = Some(p.gauge_looks);
        }
        commits.committed = request;
        commits.receipt = Receipt {
            request,
            declared: drawn,
            loadend: declared,
        };
        drop(commits);
        #[cfg(test)]
        {
            // Held at most the hook's bound: a test that never lets go
            // does not keep the render thread.
            let deadline = Instant::now() + hook_bound();
            while self.shared.hold_declaration.load(Ordering::Acquire) {
                if Instant::now() >= deadline {
                    self.shared
                        .declaration_expired
                        .store(true, Ordering::Release);
                    break;
                }
                self.shared.declaration_held.store(true, Ordering::Release);
                std::thread::sleep(std::time::Duration::from_millis(1));
            }
        }
        // Before the answer, outside the commits' lock: the opener reads
        // the minimum again once the commit is answered (or, past the
        // bound, once it read the receipt, which carries it too).
        if let Some(loadend) = declared {
            self.shared.committed_loading(loadend);
        }
        Ok(drawn)
    }

    /// Whether `request` is newer than every request asked of the render
    /// thread so far, an attempt's or a selection's, and then the newest.
    fn newest(&mut self, request: u64) -> bool {
        if request <= self.requested {
            return false;
        }
        self.requested = request;
        true
    }

    /// Selects `skin` for screen `kind` for request `request`: a request
    /// not newer than one asked already is refused, changing nothing; else
    /// an attempt preparing or prepared, older, is discarded; `skin`'s
    /// chart-independent installation is staged and committed as
    /// `kind`'s, unless the game thread withdrew the request, and for play
    /// every later attempt prepares with it. For play, `None` prepares
    /// later attempts with the skin in use; for another kind it removes
    /// that kind's installation ([`remove`](Self::remove)). A refusal, the
    /// texels the screens share included, or a withdrawal keeps every
    /// installation in use and the selection before; an error drops the
    /// screen and reports its failure, so the screen's recovery builds it
    /// again.
    fn select(
        &mut self,
        request: u64,
        kind: ScreenKind,
        skin: Option<Arc<SkinBundle>>,
        script: ScriptSlot,
    ) -> Result<Preparation, RenderError> {
        if !self.newest(request) {
            return Ok(refusal("a newer request superseded it"));
        }
        self.supersede_attempts("a newer skin selection superseded it");
        let Some(bundle) = skin else {
            if kind != ScreenKind::Play {
                return self.remove(request, kind);
            }
            self.selected = None;
            self.play_selection = request;
            return Ok(Preparation::Ready);
        };
        if let Err(e) = self.recover() {
            self.shared.failed.store(true, Ordering::Release);
            return Err(e);
        }
        let Some(screen) = &mut self.screen else {
            return Err(RenderError::SurfaceLost);
        };
        #[cfg(feature = "dev-tools")]
        if kind == ScreenKind::Result {
            crate::dev::hold_result_upload();
            if crate::dev::refuse_result_select() {
                return Ok(refusal("dev: the result's selection was refused"));
            }
        }
        #[cfg(feature = "dev-tools")]
        let uploads = screen.atlas_uploads();
        let installed = screen.install(kind, &bundle, request);
        #[cfg(feature = "dev-tools")]
        if kind == ScreenKind::Result {
            crate::dev::result_staged(screen.atlas_uploads() - uploads);
        }
        match installed {
            Ok(Preparation::Ready) => {
                // Held through the commit: a withdrawal waits for it, or
                // it sees the withdrawal and installs nothing.
                let shared = Arc::clone(&self.shared);
                let mut commits = shared.commits();
                if commits.withdrawn >= request {
                    screen.discard();
                    return Ok(Preparation::Cancelled);
                }
                let token = PreparedToken {
                    request,
                    device: screen.device(),
                    kind,
                };
                if let Err(e) = screen.commit(token) {
                    screen.discard();
                    return Ok(refusal(e.to_string()));
                }
                commits.committed = commits.committed.max(request);
                drop(commits);
                self.committed.set(kind, screen.installation(kind));
                self.scripts
                    .commit(kind, Arc::as_ptr(&bundle).addr(), script);
                if kind == ScreenKind::Play {
                    self.carries
                        .on(crate::carry::Event::Installed(crate::carry::Installed::of(
                            Some(&bundle),
                        )));
                    self.selected = Some(bundle);
                    self.play_selection = request;
                }
                if kind == ScreenKind::Result {
                    let _ = screen.drain_retired();
                }
                if kind == ScreenKind::Decision {
                    self.ensure_decision();
                }
                Ok(Preparation::Ready)
            }
            Ok(refused) => {
                screen.discard();
                Ok(refused)
            }
            Err(e) => {
                self.drop_screen();
                self.granted = false;
                self.shared.failed.store(true, Ordering::Release);
                Err(e)
            }
        }
    }

    /// An attempt preparing or prepared, older than the request asked now,
    /// is discarded with whatever the screen staged for it, whether or not
    /// it answered: one still preparing is answered refused, for `why`.
    fn supersede_attempts(&mut self, why: &str) {
        if let Some(p) = self.attempt.take() {
            let _ = p.reply.send(Ok(self.answered(refusal(why))));
            if let Some(screen) = &mut self.screen {
                screen.discard();
            }
        }
        self.drop_prepared();
    }

    /// Marks `kind`'s skin pending for request `request`, as a selection
    /// is made ([`select`](Self::select)): a request not newer than one
    /// asked already is refused, changing nothing, and so is a kind other
    /// than play; else an older attempt preparing or prepared is
    /// discarded and the screen recovered, and, unless the game thread
    /// withdrew the request (checked under the commits' lock, as a
    /// commit), play keeps only the pending marker: no installation, no
    /// skin selected for later attempts, and no specialization drawn by
    /// any loading lifetime yet. A withdrawal changes nothing.
    fn pend(&mut self, request: u64, kind: ScreenKind) -> Result<Preparation, RenderError> {
        if !self.newest(request) {
            return Ok(refusal("a newer request superseded it"));
        }
        if kind != ScreenKind::Play {
            return Ok(refusal(format!("the {kind} skin cannot be pending")));
        }
        self.supersede_attempts("a newer skin selection superseded it");
        if let Err(e) = self.recover() {
            self.shared.failed.store(true, Ordering::Release);
            return Err(e);
        }
        let shared = Arc::clone(&self.shared);
        let mut commits = shared.commits();
        if commits.withdrawn >= request {
            return Ok(Preparation::Cancelled);
        }
        self.committed.pend(kind, None);
        self.carries.on(crate::carry::Event::Installed(
            crate::carry::Installed::Pending,
        ));
        self.selected = None;
        self.play_selection = request;
        self.specialized_in = None;
        commits.committed = commits.committed.max(request);
        Ok(Preparation::Ready)
    }

    /// Removes `kind`'s installation for request `request`, recovering
    /// the screen first, unless the game thread withdrew the request:
    /// under the commits' lock, as a commit, so a withdrawal either sees
    /// it removed or keeps it. Removed, no installation of `kind` is kept
    /// for a screen built again.
    fn remove(&mut self, request: u64, kind: ScreenKind) -> Result<Preparation, RenderError> {
        if let Err(e) = self.recover() {
            self.shared.failed.store(true, Ordering::Release);
            return Err(e);
        }
        let Some(screen) = &mut self.screen else {
            return Err(RenderError::SurfaceLost);
        };
        let shared = Arc::clone(&self.shared);
        let mut commits = shared.commits();
        if commits.withdrawn >= request {
            return Ok(Preparation::Cancelled);
        }
        screen.remove(kind);
        self.committed.clear(kind);
        self.scripts.retire_kind(kind);
        if kind == ScreenKind::Decision {
            self.decision_side = None;
        }
        commits.committed = commits.committed.max(request);
        Ok(Preparation::Ready)
    }

    /// Drops the preparation of `request`, in progress or answered, unless
    /// committed: what it staged never installs.
    fn discard(&mut self, request: u64) {
        if self.attempt.as_ref().is_some_and(|p| p.request == request) {
            self.attempt = None;
            if let Some(screen) = &mut self.screen {
                screen.discard();
            }
        }
        if self
            .prepared
            .as_ref()
            .is_some_and(|p| p.token.request == request)
        {
            self.drop_prepared();
        }
    }

    /// Drops the preparation answered ready and not committed, if any.
    fn drop_prepared(&mut self) {
        if self.prepared.take().is_some()
            && let Some(screen) = &mut self.screen
        {
            screen.discard();
        }
    }

    /// Prepares the chart loaded last and resets the screen's play state,
    /// building it again after a failure.
    fn prepare(&mut self) -> Result<(), RenderError> {
        let chart = self.chart.as_deref();
        let section = self.section.as_ref();
        let (mut refused, mut decision_refused) = (None, None);
        let ready = match (&mut self.screen, chart) {
            (Some(s), Some(chart)) => s.load(chart, section),
            (Some(_), None) => Ok(()),
            (None, _) => {
                let built = build(
                    &mut self.make,
                    &mut self.committed,
                    chart,
                    section,
                    self.result_disabled.is_some(),
                    &mut refused,
                    &mut decision_refused,
                );
                // A restore refused stands for the run, whatever the build.
                if let Some(why) = refused.take() {
                    self.disable_result(&why);
                }
                if let Some(why) = &decision_refused {
                    self.disable_decision(why.clone());
                }
                built.and_then(|s| {
                    self.built(s.as_ref());
                    self.screen = Some(s);
                    // The new screen takes the window's size afresh.
                    self.applied = None;
                    self.result_rebuilt();
                    self.decision_rebuilt(decision_refused.take())
                })
            }
        };
        if ready.is_ok() {
            self.shared.failed.store(false, Ordering::Release);
        } else {
            self.screen = None;
            self.granted = false;
        }
        ready
    }
}

/// The decision screen on the render thread (design.md "Play Lifecycle",
/// the decision screen): its labels prepared at frame boundaries for the
/// chart under the cursor, its skin drawn from the slot the choice pinned,
/// the built-in presentation taking over when the skin cannot draw for the
/// active lifetime, and the kind disabled for the run on a refusal.
impl Run {
    /// Whether the decision screen shown draws its skin.
    fn decision_skinned(&self) -> bool {
        self.decision.is_some()
            && self
                .decision_shown
                .as_ref()
                .is_some_and(|s| s.slot.is_some())
    }

    /// A later mode drops the decision screen: its page, what it shows
    /// and its skin's state. Its slot is retired once the game ends its
    /// lifetime.
    fn clear_decision(&mut self) {
        self.decision = None;
        self.decision_shown = None;
        self.decision_screen = None;
    }

    /// The skinned decision screen begins: the skin's state made afresh,
    /// on the installation kept for its pin even when the kind was refused
    /// since (a retiring side refuses only later preparations and
    /// choices); without the installation, the built-in presentation takes
    /// over at once.
    fn enter_skinned_decision(&mut self) {
        match &self.decision_side {
            Some(side) => {
                self.decision_screen = Some(side.bundle().skin().prepare_decision());
            }
            None => {
                let now = self.clock.now();
                self.take_over(now, "the decision skin is not installed".to_owned());
            }
        }
    }

    /// Whether the decision labels have work for a later boundary.
    fn decision_busy(&self) -> bool {
        let target = self.screen.as_ref().map_or([0, 0], |s| s.target());
        self.decision_side
            .as_ref()
            .is_some_and(|side| side.busy(target, self.decision_want.as_ref()))
    }

    /// The decision labels' work at a frame boundary
    /// ([`crate::decision_side::DecisionSide::boundary`]): a refusal disables the kind, a
    /// device or poll failure drops the screen. Whether the screen was
    /// dropped.
    fn decision_boundary(&mut self) -> bool {
        let (Some(screen), Some(side)) = (self.screen.as_deref_mut(), self.decision_side.as_mut())
        else {
            return false;
        };
        let book = Arc::clone(&self.shared.labels);
        match side.boundary(screen, self.decision_want.as_ref()) {
            crate::decision_side::Boundary::Quiet => {}
            crate::decision_side::Boundary::Refused(e) => {
                self.disable_decision(format!("{}", Chain(&e)));
            }
            crate::decision_side::Boundary::Failed(e) => {
                eprintln!("render: the decision labels' device failed: {}", Chain(&e));
                self.drop_screen();
                self.granted = false;
                self.shared.failed.store(true, Ordering::Release);
                return true;
            }
        }
        // Refused while a lifetime was active: dropped once none is and
        // the render left the skinned screen, which draws from its store
        // until then.
        if self.decision_side.as_ref().is_some_and(|s| s.retiring)
            && render::lock(&book).active().is_none()
            && !self.decision_skinned()
        {
            self.drop_decision_side();
        }
        false
    }

    /// The decision skin refused its frame at `at` on a healthy device:
    /// the built-in presentation takes over for the active lifetime, and
    /// the kind is disabled for the run.
    fn decision_frame_refused(&mut self, e: BuildError, at: TimeUs) {
        let why = format!("the decision skin refused its frame: {}", Chain(&e));
        self.take_over(at, why.clone());
        self.disable_decision(why);
    }

    /// The decision skin of the screen shown cannot draw from `at`: its
    /// page draws from now on, and a takeover for its lifetime waits for
    /// the game thread, which is woken, only while that lifetime is the
    /// active one; another lifetime's is dropped.
    fn take_over(&mut self, at: TimeUs, why: String) {
        let Some(shown) = self.decision_shown.as_mut() else {
            return;
        };
        shown.slot = None;
        self.decision_screen = None;
        let lifetime = shown.lifetime;
        {
            // Published under the book's lock, which the game ends a
            // lifetime under: never once it ended.
            let book = render::lock(&self.shared.labels);
            if book.active() != Some(lifetime) {
                eprintln!("render: a takeover of an ended decision lifetime was dropped: {why}");
                return;
            }
            #[cfg(test)]
            self.shared.takeover_gate.pass();
            eprintln!("render: the built-in decision presentation takes over: {why}");
            *self
                .shared
                .decision_takeover
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(game::DecisionTakeover {
                lifetime: game::DecisionLifetime(lifetime),
                at,
                why,
            });
            drop(book);
        }
        #[cfg(test)]
        self.shared.takeovers_woken.fetch_add(1, Ordering::AcqRel);
        let wake = self
            .shared
            .game_wake
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(wake) = wake {
            wake();
        }
    }

    /// The decision kind is refused for the run, for `why`: the game
    /// thread's notice says it, no request is posted and no slot pinned
    /// from now on, and no screen built again restores it. The
    /// installation and its slots go now, or while a lifetime is active or
    /// the skin still draws, once neither holds (its end and the render
    /// leaving the screen), so the pinned presentation keeps drawing.
    fn disable_decision(&mut self, why: String) {
        eprintln!("render: the decision skin is disabled for the run: {why}");
        if self.decision_disabled.is_none() {
            self.decision_disabled = Some(why.clone());
            *self
                .shared
                .decision_refused
                .lock()
                .unwrap_or_else(PoisonError::into_inner) = Some(why);
        }
        self.committed.clear(ScreenKind::Decision);
        let active = {
            let mut book = render::lock(&self.shared.labels);
            book.set_unhealthy();
            book.active()
        };
        if let Some(side) = &mut self.decision_side {
            side.retiring = true;
            side.stop();
        }
        if active.is_none() && !self.decision_skinned() {
            self.drop_decision_side();
        }
    }

    /// The decision installation and its label preparation go.
    fn drop_decision_side(&mut self) {
        if let Some(side) = self.decision_side.take() {
            self.decision_next = self.decision_next.max(side.next());
        }
        if let Some(screen) = &mut self.screen {
            screen.remove(ScreenKind::Decision);
        }
    }

    /// The committed decision installation's label preparation, made for
    /// a newly committed one: the book's slots of an earlier one freed, its
    /// rows asked at the target; a misfit disables the kind.
    fn ensure_decision(&mut self) {
        let Some(bundle) = self
            .committed
            .get(ScreenKind::Decision)
            .map(|i| Arc::clone(i.bundle()))
        else {
            self.decision_side = None;
            return;
        };
        if self.decision_disabled.is_some()
            || self
                .decision_side
                .as_ref()
                .is_some_and(|s| s.is_for(&bundle))
        {
            return;
        }
        let Some(screen) = self.screen.as_deref() else {
            return;
        };
        if let Some(old) = self.decision_side.take() {
            self.decision_next = self.decision_next.max(old.next());
        }
        render::lock(&self.shared.labels).reset();
        match crate::decision_side::DecisionSide::new(
            &bundle,
            &self.shared.labels,
            self.decision_next,
            screen,
        ) {
            Ok(side) => self.decision_side = Some(side),
            Err(why) => self.disable_decision(why),
        }
    }

    /// The screen was built again (a recovery or a chart load's retry),
    /// its kinds restored, a refused decision restore saying why in
    /// `refused` (its refusal of the kind already stood, recorded before
    /// the build's result was known): the label slots reset, the pinned
    /// slot's content rebuilt at the new target under its generation and
    /// the skin drawing on; a refused restore, a refusal of the labels'
    /// rebuild on the healthy device, or no decision installation left to
    /// restore while the skin draws, is a takeover; a device or poll
    /// failure drops the screen again.
    ///
    /// # Errors
    ///
    /// The device's failure while the pinned labels were rebuilt.
    fn decision_rebuilt(&mut self, refused: Option<String>) -> Result<(), RenderError> {
        let now = self.clock.now();
        if let Some(why) = refused
            && self.decision_skinned()
        {
            self.take_over(now, why);
        }
        let installed = self.committed.get(ScreenKind::Decision).is_some();
        let (Some(side), Some(screen), true) = (
            self.decision_side.as_mut(),
            self.screen.as_deref_mut(),
            installed,
        ) else {
            // Nothing to restore the skin from: what drew it takes over.
            {
                let mut book = render::lock(&self.shared.labels);
                book.reset();
                book.set_unhealthy();
            }
            if self.decision_skinned() {
                self.take_over(now, "no decision skin was left to restore".to_owned());
            }
            if let Some(side) = self.decision_side.take() {
                self.decision_next = self.decision_next.max(side.next());
            }
            return Ok(());
        };
        match side.rebuilt(screen) {
            crate::decision_side::Rebuilt::Restored => Ok(()),
            crate::decision_side::Rebuilt::Refused(e) => {
                let why = format!(
                    "the decision labels were not restored on the screen built again: {}",
                    Chain(&e)
                );
                if self.decision_skinned() {
                    self.take_over(now, why.clone());
                }
                self.disable_decision(why);
                Ok(())
            }
            crate::decision_side::Rebuilt::Failed(e) => {
                self.screen = None;
                self.granted = false;
                Err(e)
            }
        }
    }

    /// The cursor's chart from the selection update held, asked of the
    /// decision labels' preparation.
    fn want_decision_labels(&mut self) {
        if let Some(shown) = &self.selection {
            crate::decision_side::update_want(&mut self.decision_want, &shown.update.view);
        }
    }
}

/// What a result frame's commit point and build left to draw.
#[derive(Debug, PartialEq, Eq)]
enum ResultDrawn {
    /// The skin's frame, built, to submit.
    Built,
    /// The built-in page of the view presented.
    BuiltIn,
    /// The skin refused its frame on a healthy device: nothing of it was
    /// staged.
    Refused(BuildError),
}

/// The strip's text slots, after the one drawn: the one prepared with a
/// view payload waiting, and the one prepared for the view presented.
pub(super) const STRIP_PREPARED: usize = 1;
pub(super) const STRIP_PRESENTED: usize = 2;

/// A view payload as the render thread holds it: what it was presented
/// or prepared with, and the skin's state for it.
struct HeldView {
    revision: u64,
    payload: Arc<ViewPayload>,
    overlay: Overlay,
    /// The skin's state of the payload's bundle; `None` for the built-in
    /// result.
    state: Option<skin_model::ResultScreen>,
    /// Its built-in page, composed outside the commit point while the
    /// built-in result draws it.
    page: Option<EndPage>,
    /// Its runtime's slot, for its payload's generation: only the
    /// presented view's takes the runtime (s7-07-design §3.7.1 M4).
    script: ScriptSlot,
}

/// A view payload prepared and not yet presented: its staging's token.
struct PreparedView {
    held: HeldView,
    token: Option<PreparedToken>,
}

/// The result screen as the render thread holds it (the S10-09i design's
/// D1): the presentation taken and not yet prepared, the payload
/// prepared, the one presented with its overlay, and the overlay waiting
/// to merge. Kept apart from the screen, so a screen built again draws on
/// from what was presented.
struct ResultSide {
    lifetime: u64,
    incoming: Option<Box<ResultPresentation>>,
    prepared: Option<PreparedView>,
    presented: Option<HeldView>,
    /// The newest overlay not yet merged, with its revision.
    overlay: Option<(u64, Overlay)>,
    /// What the commit point replaced or discarded, dropped after the
    /// frame: at most a view prepared and a view presented.
    dropped: [Option<HeldView>; 2],
    /// A third one, when a failed frame left both places taken: dropped
    /// with them, never inside the commit point.
    overflow: Option<HeldView>,
    /// The lifetime's notes, shared by every runtime it runs.
    notes: ScriptNotes,
    /// The presented runtime stopped: its notice leads the strip until
    /// the lifetime ends.
    stop: Option<ScriptStop>,
    /// The stop came this frame: the strip is prepared again after it.
    restrip: bool,
    /// A staging the commit point discarded, dropped after the frame.
    discard: bool,
    /// A commit happened: what it replaced is drained after the frame.
    drain: bool,
    /// A commit happened: what the result kind keeps to restore is
    /// reconciled right after the commit point, before the frame is
    /// submitted ([`Run::reconcile_result`]).
    reconcile: bool,
    /// The screen was built again: the view presented is prepared again.
    reapply: bool,
    /// A newer end page came: the built-in pages are composed again.
    end_changed: bool,
    /// A view whose staging was refused, with why.
    refused: Option<(ViewKey, String)>,
    /// The view prepared again after a rebuild keeps its request.
    restaged: bool,
}

impl ResultSide {
    fn new(lifetime: u64) -> Self {
        Self {
            lifetime,
            incoming: None,
            prepared: None,
            presented: None,
            overlay: None,
            dropped: [None, None],
            overflow: None,
            notes: ScriptNotes::default(),
            stop: None,
            restrip: false,
            discard: false,
            drain: false,
            reconcile: false,
            reapply: false,
            end_changed: false,
            refused: None,
            restaged: false,
        }
    }

    /// Keeps `held` to drop after the frame; with both places taken (a
    /// frame that failed), the older one moves to the overflow, which the
    /// caller drains outside the commit point. Drops nothing here.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn drop_later(&mut self, held: HeldView) {
        match &mut self.dropped {
            [slot @ None, _] | [_, slot @ None] => *slot = Some(held),
            [first, _] => {
                // Unreached while every result frame drains first
                // ([`Self::drain`]): a commit point adds two at most.
                let older = first.replace(held);
                if self.overflow.is_none() {
                    self.overflow = older;
                }
            }
        }
    }

    /// Drops what earlier commit points replaced or discarded: after a
    /// frame, and before the next commit point, outside it.
    fn drain(&mut self) {
        self.dropped = [None, None];
        self.overflow = None;
    }

    /// Parks every runtime its views hold: before a play's interval opens.
    fn park_scripts(&mut self) {
        for h in self
            .presented
            .iter_mut()
            .chain(self.prepared.iter_mut().map(|v| &mut v.held))
            .chain(self.dropped.iter_mut().flatten())
            .chain(self.overflow.iter_mut())
        {
            h.script.park();
        }
    }

    /// Whether it has something to draw.
    const fn shows(&self) -> bool {
        self.incoming.is_some() || self.prepared.is_some() || self.presented.is_some()
    }
}

/// The strip's lines of a view payload's and an overlay's, in order,
/// after a stopped runtime's notice, which leads so it always shows: the
/// last native line yields to it (s7-07-design §3.7.1 M7).
fn strip_lines<'a>(
    payload: &'a ViewPayload,
    overlay: &'a Overlay,
    stop: Option<ScriptStop>,
) -> Vec<&'a str> {
    stop.map(ScriptStop::notice)
        .into_iter()
        .chain(
            payload
                .lines
                .iter()
                .chain(&overlay.lines)
                .map(String::as_str),
        )
        .take(crate::result_present::STRIP_LINES)
        .collect()
}

impl Run {
    /// Takes the newest result presentation from the slot, if its lifetime
    /// is current: a new lifetime starts the result side afresh.
    fn take_result(&mut self) {
        let Some(p) = self.shared.result_slot().take() else {
            return;
        };
        if self
            .result
            .as_ref()
            .is_none_or(|r| r.lifetime != p.lifetime)
        {
            self.drop_result();
            self.result = Some(ResultSide::new(p.lifetime));
            // The lifetime's page base is its own complete page, sent with
            // it, never an earlier attempt's: its own `End` follows.
            self.end_page.1 = p.payload.page.clone();
        }
        if let Some(side) = &mut self.result {
            side.incoming = Some(p);
        }
    }

    /// The render loop ended (dev-tools): the select and result screens'
    /// whole frames still held are reported, and the run's present log is
    /// handed to `tapestry measure select`.
    #[cfg(feature = "dev-tools")]
    fn flush_screen_frames(&mut self) {
        let freq = self.clock.frequency().hz();
        self.select_frames.report("select", freq);
        self.result_frames.report("result", freq);
        crate::dev::keep_present_log(&mut self.present_log);
    }

    /// The result's lifetime ended: its staging is discarded and nothing
    /// of it is kept.
    fn drop_result(&mut self) {
        #[cfg(feature = "dev-tools")]
        {
            self.result_frames
                .report("result", self.clock.frequency().hz());
            self.frames_view = None;
        }
        if let Some(side) = self.result.take()
            && side.prepared.as_ref().is_some_and(|p| p.token.is_some())
            && let Some(screen) = &mut self.screen
        {
            screen.discard();
        }
    }

    /// The screen was built again: the view presented is prepared again on
    /// it at its next frame, at the same revision (the S10-09i design's
    /// D4, "Recovery"), and a view prepared on the old screen is prepared
    /// again.
    fn result_rebuilt(&mut self) {
        // A restore refused while it was built disabled the kind: the new
        // screen is disabled too. What it restored is what the kind keeps.
        if self.result_disabled.is_some()
            && let Some(screen) = &mut self.screen
        {
            screen.disable_result();
        }
        self.reconcile_result();
        if let Some(side) = &mut self.result {
            side.reapply = true;
            if let Some(p) = side.prepared.take() {
                side.restaged = true;
                side.incoming = Some(Box::new(ResultPresentation {
                    lifetime: side.lifetime,
                    revision: p.held.revision,
                    request: p.token.map_or(0, |t| t.request),
                    payload: p.held.payload,
                    overlay: p.held.overlay,
                }));
            }
        }
    }

    /// The one point that sets what the result kind keeps to restore (the
    /// S10-09i design's D2 and D7): exactly what the screen has installed
    /// for it, read from the screen, never from notifications of what
    /// changed it. An installation is kept as its bundle, a pages-only one
    /// as its pages, none as none. It runs after every event that can
    /// change it (a selection or upload answered, a view's commit before
    /// its frame is submitted, play's staging, the run's disable, a screen
    /// built again) and before a screen is dropped; with no screen it
    /// keeps what it had. What the record replaces is dropped here, outside
    /// the commit point. The status is published after.
    fn reconcile_result(&mut self) {
        if let Some(screen) = &self.screen {
            match (
                screen.installation(ScreenKind::Result),
                screen.installed_pages(ScreenKind::Result),
            ) {
                (Some(i), _) => {
                    if self
                        .committed
                        .get(ScreenKind::Result)
                        .is_none_or(|c| !Arc::ptr_eq(c.bundle(), i.bundle()))
                    {
                        self.committed.set(ScreenKind::Result, Some(i));
                    }
                }
                (None, Some(pages)) => {
                    if self
                        .committed
                        .pages(ScreenKind::Result)
                        .is_none_or(|c| !Arc::ptr_eq(c, &pages))
                    {
                        self.committed.set_pages(ScreenKind::Result, pages);
                    }
                }
                (None, None) => self.committed.clear(ScreenKind::Result),
            }
        }
        self.publish_result_status();
    }

    /// The screen failed: what the result kind keeps is read from it one
    /// last time, then it is dropped.
    fn drop_screen(&mut self) {
        self.reconcile_result();
        self.screen = None;
        // The attempt is abandoned: its preparation before a wait is no
        // recovered frame's (dev-tools: the whole frames).
        #[cfg(feature = "dev-tools")]
        let _ = self.result_frames.take_before_wait();
    }

    /// The result kind is disabled for the run, for `why` (the S10-09i
    /// design's D7): the screen and every screen built again refuse it,
    /// and nothing of it is kept to restore.
    fn disable_result(&mut self, why: &str) {
        eprintln!(
            "render: the result skin was refused, the built-in result draws from now on: {why}"
        );
        if self.result_disabled.is_none() {
            self.result_disabled = Some(why.to_owned());
        }
        if let Some(screen) = &mut self.screen {
            screen.disable_result();
        }
        // Every view held draws the built-in page from now on, with its
        // own rows: the one presented, else the one prepared, whose
        // staging the screen dropped (it commits as the built-in page, the
        // first presentation's rows and reason drawn in this frame).
        if let Some(side) = &mut self.result {
            let latest = &self.end_page.1;
            let why = self.result_disabled.as_deref();
            for h in side
                .presented
                .iter_mut()
                .chain(side.prepared.iter_mut().map(|v| &mut v.held))
            {
                h.state = None;
                h.page = Some(crate::result_present::compose_page(
                    &h.payload.page,
                    latest,
                    why,
                ));
            }
            if side.presented.is_some() {
                side.prepared = None;
            } else if let Some(v) = &mut side.prepared {
                v.token = None;
            }
        }
        // No skin draws: commands take effect at once (D6). No result
        // runtime runs again this run: every generation retires, the
        // running one given back here, after the frame's commit point.
        self.shared.barrier().timing = None;
        self.scripts.disable_result();
        if let Some(side) = &mut self.result {
            side.park_scripts();
        }
        self.reconcile_result();
    }

    /// Prepares what the result side took, outside the commit point
    /// (the S10-09i design's D1, "Prepared"): a new view payload staged on
    /// the screen with its request, its skin state updated and its lines
    /// staged, and the strip's text for it and for the view presented; an
    /// overlay alone only the strip's text. A screen built again has the
    /// view presented prepared again first. Allocates.
    fn prepare_result(&mut self) {
        if self
            .result
            .as_ref()
            .is_some_and(|r| !self.shared.result_slot().current(r.lifetime))
        {
            self.drop_result();
            return;
        }
        let disabled = self.result_disabled.is_some();
        let Some(side) = &mut self.result else {
            return;
        };
        let Some(screen) = &mut self.screen else {
            return;
        };
        let mut refused: Option<String> = None;
        let mut failed: Option<RenderError> = None;
        if std::mem::take(&mut side.reapply)
            && let Some(h) = &mut side.presented
        {
            if let (Some(bundle), Some(state), false) = (&h.payload.skin, &mut h.state, disabled) {
                // The layout presented is installed already: the screen
                // built again restored what the result kind keeps, which
                // follows each view's commit (`reconcile_result`).
                match screen.prepare_result(bundle, state, &h.payload.view, &h.payload.labels) {
                    Ok(Ok(())) => {
                        screen.install_result_text();
                    }
                    Ok(Err(why)) => refused = Some(why),
                    Err(e) => failed = Some(e),
                }
            }
            let lines = strip_lines(&h.payload, &h.overlay, side.stop);
            if let Err(e) = screen
                .prepare_result_strip(STRIP_PRESENTED, &lines)
                .map(|()| screen.use_result_strip(STRIP_PRESENTED))
            {
                failed = Some(e);
            }
        }
        if let Some(p) = side.incoming.take() {
            let same = |h: &HeldView| Arc::ptr_eq(&h.payload, &p.payload);
            let presented = side.presented.as_ref().is_some_and(same);
            let known = presented || side.prepared.as_ref().is_some_and(|v| same(&v.held));
            if presented && side.prepared.as_ref().is_some_and(|v| !same(&v.held)) {
                // The newest is the view presented again: the other one
                // prepared goes, so it is not committed after it.
                if side.prepared.take().is_some_and(|v| v.token.is_some()) {
                    screen.discard();
                }
            }
            if !known {
                // A new view payload: the one prepared before goes.
                if side.prepared.take().is_some_and(|v| v.token.is_some()) {
                    screen.discard();
                }
                let request = p.request;
                let restaged = std::mem::take(&mut side.restaged);
                let fresh = request > self.requested;
                if fresh {
                    self.requested = request;
                }
                let mut held = HeldView {
                    revision: p.revision,
                    payload: Arc::clone(&p.payload),
                    overlay: p.overlay.clone(),
                    state: None,
                    page: None,
                    script: self.scripts.result_slot(p.payload.script),
                };
                let mut token = None;
                let mut ok = true;
                if !fresh && !restaged {
                    // A later attempt or selection superseded it: nothing
                    // of it is prepared, and it is no view's refusal.
                    ok = false;
                } else if let (Some(bundle), false) = (&p.payload.skin, disabled) {
                    #[cfg(feature = "dev-tools")]
                    let uploads = screen.atlas_uploads();
                    let installed = screen.install(ScreenKind::Result, bundle, request);
                    #[cfg(feature = "dev-tools")]
                    crate::dev::result_staged(screen.atlas_uploads() - uploads);
                    // A refusal injected by a test of the real flow
                    // (dev-tools).
                    #[cfg(feature = "dev-tools")]
                    let installed = match installed {
                        Ok(Preparation::Ready) if crate::dev::refuse_result_staging() => {
                            screen.discard();
                            Ok(refusal("dev: the view's staging was refused"))
                        }
                        other => other,
                    };
                    match installed {
                        Ok(Preparation::Ready) => {
                            token = Some(PreparedToken {
                                request,
                                device: screen.device(),
                                kind: ScreenKind::Result,
                            });
                            let mut state = bundle.skin().prepare_result();
                            let prepared = screen.prepare_result(
                                bundle,
                                &mut state,
                                &p.payload.view,
                                &p.payload.labels,
                            );
                            // A refusal injected by a test of the real flow
                            // (dev-tools).
                            #[cfg(feature = "dev-tools")]
                            let prepared = match prepared {
                                Ok(Ok(())) if crate::dev::refuse_result_text() => {
                                    Ok(Err("dev: the view's text was refused".to_owned()))
                                }
                                other => other,
                            };
                            match prepared {
                                Ok(Ok(())) => held.state = Some(state),
                                Ok(Err(why)) => {
                                    // Disabled below; the first presentation
                                    // still commits, as the built-in page with
                                    // its own rows and the reason.
                                    refused = Some(why);
                                    ok = side.presented.is_none();
                                }
                                Err(e) => {
                                    failed = Some(e);
                                    ok = false;
                                }
                            }
                        }
                        Ok(why) => {
                            // The view's staging was refused: the presenter
                            // shows the built-in result for it (scope view).
                            let why = match why {
                                Preparation::Refused(r) => r.to_string(),
                                _ => "the staging was cancelled".to_owned(),
                            };
                            eprintln!("render: the result view's staging was refused: {why}");
                            // The view draws the built-in page with its own
                            // rows at once (scope view, the S10-09i design's
                            // D7); the presenter's built-in payload for it
                            // follows.
                            held.page = Some(crate::result_present::compose_page(
                                &held.payload.page,
                                &self.end_page.1,
                                Some(&why),
                            ));
                            side.refused = Some((p.payload.key, why));
                        }
                        Err(e) => {
                            failed = Some(e);
                            ok = false;
                        }
                    }
                }
                if ok {
                    if held.page.is_none() && (held.state.is_none() || disabled) {
                        held.page = Some(crate::result_present::compose_page(
                            &held.payload.page,
                            &self.end_page.1,
                            self.result_disabled
                                .as_deref()
                                .or_else(|| held.payload.why.as_ref().map(|(_, w)| w.as_str())),
                        ));
                    }
                    side.prepared = Some(PreparedView { held, token });
                } else if token.is_some() {
                    screen.discard();
                }
            }
            side.overlay = Some((p.revision, p.overlay));
        }
        // The strip's text for what each outcome of the commit point
        // presents: the view prepared, or the view presented, with the
        // newest overlay.
        if let Some((_, overlay)) = &side.overlay {
            if let Some(v) = &side.prepared {
                let lines = strip_lines(&v.held.payload, overlay, side.stop);
                if let Err(e) = screen.prepare_result_strip(STRIP_PREPARED, &lines) {
                    failed = Some(e);
                }
            }
            if let Some(h) = &side.presented {
                let lines = strip_lines(&h.payload, overlay, side.stop);
                if let Err(e) = screen.prepare_result_strip(STRIP_PRESENTED, &lines) {
                    failed = Some(e);
                }
            }
        }
        if std::mem::take(&mut side.end_changed) {
            let latest = &self.end_page.1;
            let why = self.result_disabled.as_deref();
            let refused = side.refused.as_ref();
            for h in side
                .presented
                .iter_mut()
                .chain(side.prepared.iter_mut().map(|v| &mut v.held))
            {
                if h.state.is_none() || disabled {
                    let why = why
                        .or_else(|| h.payload.why.as_ref().map(|(_, w)| w.as_str()))
                        .or_else(|| {
                            refused
                                .filter(|(k, _)| *k == h.payload.key)
                                .map(|(_, w)| w.as_str())
                        });
                    h.page = Some(crate::result_present::compose_page(
                        &h.payload.page,
                        latest,
                        why,
                    ));
                }
            }
        }
        if let Some(e) = failed {
            eprintln!("render: {}", Chain(&e));
            self.drop_screen();
            self.granted = false;
            self.shared.failed.store(true, Ordering::Release);
            return;
        }
        if let Some(why) = refused {
            self.disable_result(&why);
        }
        self.publish_result_status();
    }

    /// The commit point of a result frame (the S10-09i design's D1 and
    /// D3), its first step; the presented view's runtime
    /// ([`Self::result_visit`]) and the build ([`Self::result_build`])
    /// follow. Under the barrier's lock: a barrier set for the lifetime
    /// discards the view prepared and merges only the overlay; otherwise
    /// the view prepared is committed with its lines and its strip, under
    /// one hold of the result runtimes' lock with its generation checked
    /// live first ([`Scripts::commit_view`]), and becomes the one
    /// presented, the running runtime moving to it when it keeps the
    /// presented generation; then the overlay merges. Nothing is
    /// destroyed. Returns the fade-out's start. Allocates nothing.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn result_commit(
        screen: &mut dyn Screen,
        side: Option<&mut ResultSide>,
        shared: &Shared,
        scripts: &Scripts,
    ) -> Option<TimeUs> {
        let side = side?;
        let fadeout;
        {
            let mut b = shared.barrier();
            if b.lifetime != side.lifetime {
                *b = ResultBarrier {
                    lifetime: side.lifetime,
                    ..ResultBarrier::default()
                };
            }
            if b.fadeout.is_some() {
                if let Some(v) = side.prepared.take() {
                    side.discard |= v.token.is_some();
                    side.drop_later(v.held);
                }
                if let (Some((revision, overlay)), Some(h)) =
                    (side.overlay.take(), side.presented.as_mut())
                {
                    h.revision = h.revision.max(revision);
                    h.overlay = overlay;
                    screen.use_result_strip(STRIP_PRESENTED);
                }
            } else {
                let mut committed = None;
                if let Some(v) = side.prepared.take() {
                    let lifetime = side.lifetime;
                    let commit =
                        scripts.commit_view(v.held.payload.script, lifetime, || match v.token {
                            Some(token) => match screen.commit(token) {
                                Ok(()) => {
                                    screen.install_result_text();
                                    true
                                }
                                Err(_) => false,
                            },
                            None => true,
                        });
                    match commit {
                        ViewCommit::Committed => {
                            if v.token.is_some() {
                                side.drain = true;
                                side.reconcile = true;
                            }
                            committed = Some(v.held);
                        }
                        // Its generation was evicted: it never commits,
                        // its staging discarded after the frame.
                        ViewCommit::Gone => {
                            side.discard |= v.token.is_some();
                            side.drop_later(v.held);
                        }
                        ViewCommit::Refused => side.drop_later(v.held),
                    }
                }
                if let Some(mut h) = committed {
                    if let Some((revision, overlay)) = side.overlay.take() {
                        h.revision = h.revision.max(revision);
                        h.overlay = overlay;
                    }
                    screen.use_result_strip(STRIP_PREPARED);
                    if let Some(old) = side.presented.as_mut() {
                        // A switch that keeps the layout keeps its running
                        // runtime.
                        h.script.transfer_from(&mut old.script);
                    }
                    if let Some(old) = side.presented.replace(h) {
                        side.drop_later(old);
                    }
                } else if let (Some((revision, overlay)), Some(h)) =
                    (side.overlay.take(), side.presented.as_mut())
                {
                    h.revision = h.revision.max(revision);
                    h.overlay = overlay;
                    screen.use_result_strip(STRIP_PRESENTED);
                }
            }
            b.presented = side.presented.as_ref().map(|h| h.payload.key);
            b.timing = side
                .presented
                .as_ref()
                .filter(|h| h.state.is_some())
                .and_then(|h| h.payload.skin.as_ref())
                .map(|s| s.skin().result_timing());
            fadeout = b
                .fadeout
                .or_else(|| side.presented.as_ref().and_then(|h| h.overlay.fadeout));
        }
        fadeout
    }

    /// The presented view's runtime step of a result frame, between its
    /// commit point and its build, outside both counted spans
    /// ([`crate::screens::result_visit`]); a stop's notice is latched for
    /// the lifetime and its strip prepared again after the frame.
    fn result_visit(side: Option<&mut ResultSide>, now: TimeUs, fadeout: Option<TimeUs>) {
        let Some(side) = side else {
            return;
        };
        let Some(h) = side.presented.as_mut() else {
            return;
        };
        let (Some(bundle), Some(state)) = (&h.payload.skin, h.state.as_mut()) else {
            return;
        };
        let timers = skin_model::ResultTimers {
            screen: Some(h.overlay.screen),
            fadeout,
        };
        if let Ok(Some(stop)) = crate::screens::result_visit(
            &mut h.script,
            &side.notes,
            bundle.skin(),
            state,
            now,
            &timers,
        ) && side.stop.is_none()
        {
            side.stop = Some(stop);
            side.restrip = true;
        }
    }

    /// The build of a result frame: the skin's frame with the strip, or
    /// the built-in page left to draw. Allocates nothing.
    #[deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]
    fn result_build(
        screen: &mut dyn Screen,
        side: Option<&mut ResultSide>,
        now: TimeUs,
        fadeout: Option<TimeUs>,
    ) -> ResultDrawn {
        let Some(h) = side.and_then(|s| s.presented.as_mut()) else {
            return ResultDrawn::BuiltIn;
        };
        if let Some(state) = h.state.as_mut().filter(|_| h.payload.skin.is_some()) {
            let frame = skin_model::ResultFrame {
                now,
                target: screen.target(),
                layout: &skin_model::Layout::AUTHOR,
                timers: skin_model::ResultTimers {
                    screen: Some(h.overlay.screen),
                    fadeout,
                },
            };
            // The frame's preparation ends here (dev-tools: the S7-07d
            // gate's whole frames).
            #[cfg(feature = "dev-tools")]
            crate::dev::mark_build();
            return match screen.build_result(state, &frame) {
                Ok(()) => ResultDrawn::Built,
                Err(e) => ResultDrawn::Refused(e),
            };
        }
        ResultDrawn::BuiltIn
    }

    /// Development builds: the ends of G3 and G5 (the S10-09i design's
    /// §8.1) at a presented frame drawn with the result skin, printed
    /// after the frame: the first one since the result's preparation
    /// began, and the first one of another view since a preset step.
    #[cfg(feature = "dev-tools")]
    fn probe_result_frame(&mut self) {
        let now = std::time::Instant::now();
        if let Some(us) = game::probe::take_since(&game::probe::RESULT_BEGAN, now) {
            eprintln!("dev: probe G3 save to first result: {us} us");
        }
        let view = self
            .result
            .as_ref()
            .and_then(|s| s.presented.as_ref())
            .map(|h| h.payload.key);
        if view != self.probe_view {
            self.probe_view = view;
            if let Some(us) = game::probe::take_since(&game::probe::STEP_ACCEPTED, now) {
                eprintln!("dev: probe G5 switch: {us} us");
            }
        }
    }

    /// After a result frame: what the commit point discarded or replaced
    /// is dropped, the array a commit released drained (the S10-09i
    /// design's §2.1 Q3), and the status published.
    fn after_result_frame(&mut self) {
        let Some(side) = &mut self.result else {
            return;
        };
        // What the commit point replaced: its slots park, outside it.
        side.drain();
        // A runtime stopped this frame: the presented strip is prepared
        // again with its notice, drawn from the next frame on.
        if std::mem::take(&mut side.restrip)
            && let (Some(h), Some(screen)) = (&side.presented, &mut self.screen)
        {
            let lines = strip_lines(&h.payload, &h.overlay, side.stop);
            if screen.prepare_result_strip(STRIP_PRESENTED, &lines).is_ok() {
                screen.use_result_strip(STRIP_PRESENTED);
            }
        }
        if let Some(screen) = &mut self.screen {
            if std::mem::take(&mut side.discard) {
                screen.discard();
            }
            if std::mem::take(&mut side.drain) {
                let _ = screen.drain_retired();
            }
        }
        self.publish_result_status();
    }

    /// Publishes what the render thread shows of the result kind: its
    /// lifetime, the revision and view presented, the pinned view, every
    /// atlas held for it (the screen's and the one kept to restore), the
    /// run's register and a view refused. Allocates only when it changed.
    fn publish_result_status(&mut self) {
        let mut retained: [Option<AtlasId>; 4] = [None; 4];
        if let Some(screen) = &self.screen {
            let [a, b, c] = screen.retained_atlases(ScreenKind::Result);
            retained = [a, b, c, None];
        }
        if let Some(slot) = retained.get_mut(3) {
            *slot = self.committed.atlas(ScreenKind::Result);
        }
        let ids = || {
            let mut seen: [Option<AtlasId>; 4] = [None; 4];
            let mut n = 0;
            for id in retained.iter().flatten() {
                if !seen.iter().flatten().any(|s| s == id)
                    && let Some(slot) = seen.get_mut(n)
                {
                    *slot = Some(*id);
                    n += 1;
                }
            }
            seen
        };
        let pinned = self.shared.barrier().pinned;
        let (lifetime, revision, shown, refused) = match &self.result {
            Some(side) => (
                side.lifetime,
                side.presented.as_ref().map_or(0, |h| h.revision),
                side.presented.as_ref().map(|h| {
                    (
                        h.payload.key,
                        h.state.is_some() && self.result_disabled.is_none(),
                    )
                }),
                side.refused.as_ref(),
            ),
            None => (0, 0, None, None),
        };
        let installed = self.committed.atlas(ScreenKind::Result);
        let mut status = self.shared.result_status();
        let same = status.installed == installed
            && status.disabled == self.result_disabled
            && status.lifetime == lifetime
            && status.revision == revision
            && status.shown == shown
            && status.pinned == pinned
            && status.refused.as_ref() == refused
            && status
                .retained
                .iter()
                .copied()
                .eq(ids().into_iter().flatten());
        if same {
            return;
        }
        // What is shown, once it changes (dev-tools: tests of the real
        // flow read it).
        #[cfg(feature = "dev-tools")]
        if status.shown != shown
            && let Some((_, skin)) = shown
        {
            let why = self.result_disabled.as_deref().or_else(|| {
                self.result
                    .as_ref()
                    .and_then(|r| r.presented.as_ref())
                    .and_then(|h| {
                        h.payload
                            .why
                            .as_ref()
                            .map(|(_, w)| w.as_str())
                            .or_else(|| refused.map(|(_, w)| w.as_str()))
                    })
            });
            match (skin, why) {
                (true, _) => println!("dev: result shown: skin"),
                (false, Some(why)) => println!("dev: result shown: built-in: {why}"),
                (false, None) => println!("dev: result shown: built-in"),
            }
        }
        status.installed = installed;
        status.disabled.clone_from(&self.result_disabled);
        status.lifetime = lifetime;
        status.revision = revision;
        status.shown = shown;
        status.pinned = pinned;
        status.refused = refused.cloned();
        status.retained = ids().into_iter().flatten().collect();
    }

    /// Selects the result kind's pages-only installation of `pages` for
    /// request `request`, as [`select`](Self::select) selects a skin.
    fn select_pages(
        &mut self,
        request: u64,
        kind: ScreenKind,
        pages: &Arc<PackedPages>,
    ) -> Result<Preparation, RenderError> {
        if !self.newest(request) {
            return Ok(refusal("a newer request superseded it"));
        }
        self.supersede_attempts("a newer skin selection superseded it");
        if let Err(e) = self.recover() {
            self.shared.failed.store(true, Ordering::Release);
            return Err(e);
        }
        let Some(screen) = &mut self.screen else {
            return Err(RenderError::SurfaceLost);
        };
        #[cfg(feature = "dev-tools")]
        {
            crate::dev::hold_result_upload();
            if crate::dev::refuse_result_select() {
                return Ok(refusal("dev: the result's upload was refused"));
            }
        }
        // G1's cold precondition (dev-tools `cold`): the installation
        // removed and nothing retained, so the pages upload again.
        #[cfg(feature = "dev-tools")]
        let cold = crate::dev::result_cold();
        #[cfg(feature = "dev-tools")]
        if cold {
            screen.remove(kind);
            let _ = screen.drain_retired();
        }
        #[cfg(feature = "dev-tools")]
        let retained = screen.retained_atlases(kind).iter().flatten().count();
        #[cfg(feature = "dev-tools")]
        let _ = screen.take_upload_timing();
        #[cfg(feature = "dev-tools")]
        let uploads = screen.atlas_uploads();
        let installed = screen.install_pages(kind, pages, request);
        #[cfg(feature = "dev-tools")]
        crate::dev::result_staged(screen.atlas_uploads() - uploads);
        #[cfg(feature = "dev-tools")]
        if let Some(t) = screen.take_upload_timing() {
            crate::dev::print_upload(cold, retained, t);
        }
        match installed {
            Ok(Preparation::Ready) => {
                let shared = Arc::clone(&self.shared);
                let mut commits = shared.commits();
                if commits.withdrawn >= request {
                    screen.discard();
                    return Ok(Preparation::Cancelled);
                }
                let token = PreparedToken {
                    request,
                    device: screen.device(),
                    kind,
                };
                if let Err(e) = screen.commit(token) {
                    screen.discard();
                    return Ok(refusal(e.to_string()));
                }
                let _ = screen.drain_retired();
                commits.committed = commits.committed.max(request);
                drop(commits);
                Ok(Preparation::Ready)
            }
            Ok(refused) => {
                screen.discard();
                Ok(refused)
            }
            Err(e) => {
                self.drop_screen();
                self.granted = false;
                self.shared.failed.store(true, Ordering::Release);
                Err(e)
            }
        }
    }
}

/// A frame's timing for a page drawn with no measurement of its own.
const fn timing_of(presented: bool) -> FrameTiming {
    FrameTiming {
        cpu: Duration::ZERO,
        sprites: 0,
        dropped: 0,
        presented,
        present_count: None,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicI64, AtomicU32, Ordering};
    use std::time::{Duration, Instant};

    use bms_model::{Chart, ChartBuilder, Lane, TimeSource, TimeUs};
    use eval::{DeviceProfile, InputStart};
    use game::{Logic, LogicConfig};
    use input::{QpcClock, SourceReader};
    use render::{FrameInput, FrameTiming, RenderError, SkinError};

    use game::Preparation;

    use super::{MakeScreen, NoFrames, RenderMsg, Screen, ScreenError, View, spawn};
    use crate::attempt_context::AttemptContext;
    use crate::loading_state::{LoadInput, LoadingUpdate};
    use crate::loading_text::{ChosenChart, LoadingText, Stage};
    use crate::measure::{Measure, PlayMeasure};
    use skin_model::ScreenKind;

    fn timing(presented: bool) -> FrameTiming {
        FrameTiming {
            cpu: Duration::from_micros(50),
            sprites: 0,
            dropped: 0,
            presented,
            present_count: None,
        }
    }

    /// A measurement whose queues nothing feeds.
    pub(crate) fn measure(clock: QpcClock) -> Measure {
        let (_steps, steps) = rtrb::RingBuffer::new(1);
        let (_sounds, sounds) = rtrb::RingBuffer::new(1);
        let overlay = Arc::new(std::sync::atomic::AtomicBool::new(false));
        Measure::new(clock, None, None, steps, sounds, overlay)
    }

    fn play_measure(origin: TimeUs) -> Box<PlayMeasure> {
        Box::new(PlayMeasure {
            generation: 1,
            origin,
            ready_state_at: origin,
            started: origin,
            page_faults: None,
            load_from: TimeUs(0),
            retry: false,
            audio: None,
            queued_periods: None,
        })
    }

    /// A screen whose third text frame fails, and whose play frames are
    /// presented only from the fourth on, when it notes the clock.
    struct Flaky {
        text_frames: u32,
        play_frames: u32,
        clock: QpcClock,
        presented_at: Arc<AtomicI64>,
    }

    impl Screen for Flaky {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            self.play_frames += 1;
            let presented = self.play_frames >= 4;
            if presented {
                let _ = self.presented_at.compare_exchange(
                    i64::MIN,
                    self.clock.now().0,
                    Ordering::AcqRel,
                    Ordering::Acquire,
                );
            }
            Ok(timing(presented))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            self.text_frames += 1;
            if self.text_frames == 3 {
                Err(RenderError::SurfaceLost)
            } else {
                Ok(true)
            }
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// A menu screen, the `id`th built, whose first screen fails its third
    /// menu frame; every menu frame notes the screen and whether its page
    /// was new.
    struct Menus {
        id: u32,
        frames: u32,
        seen: Arc<std::sync::Mutex<Vec<(u32, bool)>>>,
    }

    impl Screen for Menus {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            self.frames += 1;
            self.seen.lock().unwrap().push((self.id, changed));
            if self.id == 1 && self.frames == 3 {
                Err(RenderError::SurfaceLost)
            } else {
                Ok(timing(true))
            }
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The select screen is drawn without a chart, its page laid out once;
    /// a failed menu frame is reported, and after the preparation that
    /// builds the screen again, the page selection sends again is drawn
    /// there.
    #[test]
    fn a_failed_menu_frame_is_reported_and_the_page_draws_on_a_new_screen() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let builds = Arc::new(AtomicU32::new(0));
        let (made, noted) = (Arc::clone(&builds), Arc::clone(&seen));
        let make: MakeScreen = Box::new(move || {
            let id = made.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(Box::new(Menus {
                id,
                frames: 0,
                seen: Arc::clone(&noted),
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
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| link.failed()));
        assert!(link.prepare().is_ok());
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| seen.lock().unwrap().contains(&(2, false))));
        link.exit();
        let seen = seen.lock().unwrap().clone();
        assert_eq!(&seen[..3], [(1, true), (1, false), (1, false)]);
        assert_eq!(seen[3], (2, true), "{seen:?}");
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    /// The loading state is drawn on through a chart's load and a
    /// preparation, answered between its frames, until the play's page
    /// replaces it; after the select page, a load leaves nothing drawn
    /// until what comes next.
    #[test]
    fn the_loading_state_is_drawn_through_the_load_and_the_preparation() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&seen);
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Menus {
                // Not the first build: no frame fails.
                id: 2,
                frames: 0,
                seen: Arc::clone(&noted),
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
        let frames = || seen.lock().unwrap().len();
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        link.loading().show(crate::menu_screen::MenuPage::default());
        assert!(wait_until(|| frames() >= 2));
        link.load(Arc::clone(&chart), None).unwrap();
        let loaded = frames();
        assert!(
            wait_until(|| frames() >= loaded + 3),
            "no frame after the load"
        );
        link.prepare().unwrap();
        let prepared = frames();
        assert!(
            wait_until(|| frames() >= prepared + 3),
            "no frame after the preparation"
        );
        // The select page and a load: nothing drawn after the answer.
        link.send(RenderMsg::Menu(Box::default()));
        let menu = frames();
        assert!(wait_until(|| frames() >= menu + 2));
        link.load(chart, None).unwrap();
        let after = frames();
        std::thread::sleep(Duration::from_millis(200));
        assert_eq!(frames(), after);
        link.exit();
    }

    /// A menu screen whose frames stall while `stall` is set; every menu
    /// frame notes its page's first line and whether the page was new.
    struct StalledMenus {
        stall: Arc<AtomicBool>,
        stalled: Arc<AtomicBool>,
        seen: Arc<std::sync::Mutex<Vec<(String, bool)>>>,
        answer: Arc<std::sync::Mutex<Answer>>,
    }

    /// A preparation's answer as a menu screen sees it: the first menu
    /// frame after the answer was sent takes it, noting how many frames
    /// were drawn before it and whether it was `Ok`.
    #[derive(Default)]
    struct Answer {
        pending: Option<std::sync::mpsc::Receiver<Result<(), RenderError>>>,
        taken: Option<(usize, bool)>,
    }

    impl Screen for StalledMenus {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            page: &crate::menu_screen::MenuPage,
            changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            let line = page
                .lines
                .first()
                .map_or_else(String::new, |l| l.text.clone());
            let mut seen = self.seen.lock().unwrap();
            let mut answer = self.answer.lock().unwrap();
            if let Some(ready) = answer.pending.as_ref().and_then(|rx| rx.try_recv().ok()) {
                answer.pending = None;
                answer.taken = Some((seen.len(), ready.is_ok()));
            }
            drop(answer);
            seen.push((line, changed));
            drop(seen);
            while self.stall.load(Ordering::Acquire) {
                self.stalled.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(1));
            }
            self.stalled.store(false, Ordering::Release);
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    fn one_line(text: &str) -> crate::menu_screen::MenuPage {
        crate::menu_screen::MenuPage {
            lines: vec![crate::menu_screen::MenuLine {
                text: text.to_owned(),
                style: crate::menu_screen::LineStyle::Title,
            }],
            caret: None,
        }
    }

    /// A render thread stalled in a loading frame falls behind on nothing:
    /// of 300 pages shown meanwhile, only the newest is drawn, one new page
    /// once it resumes, and the play's preparation is answered right after
    /// that one frame; a page shown before loading ended is never drawn
    /// after.
    #[test]
    fn a_stalled_render_thread_draws_only_the_newest_loading_lines() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stall = Arc::new(AtomicBool::new(false));
        let stalled = Arc::new(AtomicBool::new(false));
        let answer = Arc::new(std::sync::Mutex::new(Answer::default()));
        let (noted, held, at) = (Arc::clone(&seen), Arc::clone(&stall), Arc::clone(&stalled));
        let answered = Arc::clone(&answer);
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(StalledMenus {
                stall: Arc::clone(&held),
                stalled: Arc::clone(&at),
                seen: Arc::clone(&noted),
                answer: Arc::clone(&answered),
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
        let sink = link.loading();
        let frames = || seen.lock().unwrap().len();
        sink.show(one_line("0"));
        assert!(wait_until(|| frames() >= 1));
        stall.store(true, Ordering::Release);
        assert!(wait_until(|| stalled.load(Ordering::Acquire)));
        let before = frames();
        for n in 1..=300 {
            sink.show(one_line(&n.to_string()));
        }
        // The preparation waits behind the pages; the frame after its
        // answer notes how many were drawn before it.
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        answer.lock().unwrap().pending = Some(rx);
        link.send(RenderMsg::Prepare(reply));
        stall.store(false, Ordering::Release);
        assert!(wait_until(|| answer.lock().unwrap().taken.is_some()));
        let (drawn, ready) = answer.lock().unwrap().taken.unwrap();
        assert!(ready);
        // Unbacked by a slot and its one signal, every page would be a
        // frame of its own before the answer.
        let after: Vec<_> = seen.lock().unwrap()[before..drawn].to_vec();
        assert_eq!(after, [("300".to_owned(), true)]);

        // Loading ends while the thread stalls with a page waiting: the
        // select page follows, and the waiting page is never drawn.
        stall.store(true, Ordering::Release);
        assert!(wait_until(|| stalled.load(Ordering::Acquire)));
        sink.show(one_line("stale"));
        link.clear_loading();
        link.send(RenderMsg::Menu(Box::new(one_line("select"))));
        stall.store(false, Ordering::Release);
        assert!(wait_until(|| seen
            .lock()
            .unwrap()
            .iter()
            .any(|(l, _)| l == "select")));
        std::thread::sleep(Duration::from_millis(100));
        link.exit();
        let seen = seen.lock().unwrap().clone();
        assert!(!seen.iter().any(|(l, _)| l == "stale"), "{seen:?}");
    }

    /// A paced screen, as a swap chain's waitable object paces one: `wait`
    /// grants a frame, blocking while `hold` is set, and the frame drawn
    /// next takes the grant; every frame notes whether it was a loading
    /// frame and its page's first line.
    struct PacedLoading {
        hold: Arc<AtomicBool>,
        waiting: Arc<AtomicBool>,
        seen: Arc<std::sync::Mutex<Vec<(bool, String)>>>,
    }

    impl PacedLoading {
        fn note(&self, loading: bool, page: &crate::menu_screen::MenuPage) -> FrameTiming {
            let line = page
                .lines
                .first()
                .map_or_else(String::new, |l| l.text.clone());
            self.seen.lock().unwrap().push((loading, line));
            timing(true)
        }
    }

    impl Screen for PacedLoading {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            while self.hold.load(Ordering::Acquire) {
                self.waiting.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(1));
            }
            self.waiting.store(false, Ordering::Release);
            true
        }
        fn paced(&self) -> bool {
            true
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            Ok(self.note(false, page))
        }
        fn loading(
            &mut self,
            page: &crate::menu_screen::MenuPage,
            _update: Option<&LoadingUpdate>,
            _changed: bool,
            _frame: &render::LoadingFrame<'_>,
        ) -> Result<Result<FrameTiming, skin_model::BuildError>, RenderError> {
            Ok(Ok(self.note(true, page)))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The end of loading before recording opens, while the render thread
    /// waits for its next frame: the answer comes only once it can draw
    /// no loading frame any more, and none is drawn after the end began,
    /// though the wait it was in grants a frame.
    #[test]
    fn no_loading_frame_follows_the_end_of_loading() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let hold = Arc::new(AtomicBool::new(false));
        let waiting = Arc::new(AtomicBool::new(false));
        let (noted, held, at) = (Arc::clone(&seen), Arc::clone(&hold), Arc::clone(&waiting));
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(PacedLoading {
                hold: Arc::clone(&held),
                waiting: Arc::clone(&at),
                seen: Arc::clone(&noted),
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
        let loading = || seen.lock().unwrap().iter().filter(|(l, _)| *l).count();
        link.loading().show(one_line("L"));
        assert!(wait_until(|| loading() >= 2));
        // The render thread waits for its next frame when loading ends.
        hold.store(true, Ordering::Release);
        assert!(wait_until(|| waiting.load(Ordering::Acquire)));
        let before = loading();
        let release = {
            let hold = Arc::clone(&hold);
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(100));
                hold.store(false, Ordering::Release);
            })
        };
        link.end_loading().unwrap();
        let answered = loading();
        release.join().unwrap();
        std::thread::sleep(Duration::from_millis(100));
        link.exit();
        assert_eq!(answered, before, "a loading frame before the answer");
        assert_eq!(loading(), before, "a loading frame after the answer");
    }

    /// A loading frame as a skin sees it: its lifetime, its origin and
    /// time on the loading clock, and its page's first line.
    type Clocked = (u64, TimeUs, TimeUs, String);

    /// A screen noting every loading frame it draws ([`Clocked`]).
    struct ClockedLoading(Arc<std::sync::Mutex<Vec<Clocked>>>);

    impl Screen for ClockedLoading {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn loading(
            &mut self,
            page: &crate::menu_screen::MenuPage,
            _update: Option<&LoadingUpdate>,
            _changed: bool,
            frame: &render::LoadingFrame<'_>,
        ) -> Result<Result<FrameTiming, skin_model::BuildError>, RenderError> {
            let line = page
                .lines
                .first()
                .map_or_else(String::new, |l| l.text.clone());
            self.0
                .lock()
                .unwrap()
                .push((frame.lifetime, frame.load.since, frame.now, line));
            Ok(Ok(timing(true)))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// A loading frame's turntable angle and its time on the loading clock.
    type Turned = (i32, TimeUs);

    /// A screen noting every loading frame's turntable angle ([`Turned`]).
    struct Turning(Arc<std::sync::Mutex<Vec<Turned>>>);

    impl Screen for Turning {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn loading(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _update: Option<&LoadingUpdate>,
            _changed: bool,
            frame: &render::LoadingFrame<'_>,
        ) -> Result<Result<FrameTiming, skin_model::BuildError>, RenderError> {
            self.0.lock().unwrap().push((frame.turntable, frame.now));
            Ok(Ok(timing(true)))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The loading frames carry the play state's turntable (docs/stage3.md,
    /// offset 1): its first presented frame reads 0 and sets the baseline,
    /// idle frames turn it on by their time, a pause of its number holds
    /// it whatever is drawn, its resume goes on from the held angle with a
    /// fresh baseline, another number's signals change nothing, and a new
    /// play state starts it over.
    #[test]
    fn loading_frames_carry_the_play_states_turntable() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&seen);
        let make: MakeScreen =
            Box::new(move || Ok(Box::new(Turning(Arc::clone(&noted))) as Box<dyn Screen>));
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
        let count = || seen.lock().unwrap().len();
        let state = |number| game::PlayState {
            number,
            origin: clock.now(),
        };
        link.play_state(state(1));
        link.loading().show(one_line("L"));
        assert!(wait_until(|| count() >= 1));
        assert_eq!(seen.lock().unwrap()[0].0, 0, "the first frame adds 0");
        std::thread::sleep(Duration::from_millis(60));
        let n = count();
        assert!(wait_until(|| count() > n + 1));
        // Idle: +1/6° per ms after negation, one unit a µs. A frame draws
        // what the ticks before it left: the angle of the last frame is the
        // time from the first to the one before it; the frames' times are
        // sampled apart from the ticks, so a 5 ms tolerance.
        let (angle, previous, first) = {
            let s = seen.lock().unwrap();
            (s[s.len() - 1].0, s[s.len() - 2].1, s[0].1)
        };
        let off = i64::from(angle) - (previous.0 - first.0);
        assert!(
            off.abs() < 5_000,
            "{angle} after {} µs",
            previous.0 - first.0
        );
        // Another number's pause changes nothing; its own holds it: every
        // frame after both were taken draws one angle.
        link.turntable(2, true);
        link.turntable(1, true);
        assert!(link.fence(Duration::from_secs(5)));
        let n = count();
        assert!(wait_until(|| count() > n + 3));
        let held = seen.lock().unwrap()[n].0;
        assert!(
            seen.lock().unwrap()[n..].iter().all(|f| f.0 == held),
            "paused"
        );
        // Its resume, before the practice page shows: the frames drawn
        // between (here the loading lifetime's, as a withdrawn result's
        // old play frames would be) neither tick nor take the baseline.
        link.turntable(1, false);
        assert!(link.fence(Duration::from_secs(5)));
        let n = count();
        std::thread::sleep(Duration::from_millis(100));
        assert!(wait_until(|| count() > n + 3));
        assert!(
            seen.lock().unwrap()[n..].iter().all(|f| f.0 == held),
            "frames before the page after the resume"
        );
        // The page shows and runs; a Start's loading then draws the held
        // angle plus no more than the time since the page showed.
        link.end_loading().unwrap();
        let page = clock.now();
        link.send(RenderMsg::PracticePage(Box::new(one_line("page"))));
        std::thread::sleep(Duration::from_millis(60));
        let n = count();
        link.loading().show(one_line("Start"));
        assert!(wait_until(|| count() > n + 1));
        let turned = i64::from(seen.lock().unwrap()[n + 1].0) - i64::from(held);
        let since = clock.now().0 - page.0;
        assert!(
            turned > 0 && turned <= since,
            "from the held angle at the page: {turned} in {since} µs"
        );
        // A new play state starts over at its first frame.
        link.end_loading().unwrap();
        link.play_state(state(2));
        let n = count();
        link.loading().show(one_line("M"));
        assert!(wait_until(|| count() > n));
        assert_eq!(
            seen.lock().unwrap()[n].0,
            0,
            "the new play state's first frame"
        );
        link.end_loading().unwrap();
        link.exit();
    }

    /// A generic page (a preparation's wait) is no play state's
    /// presentation: a play state adopted at its attempt's preparation,
    /// with no loading (a direct play, a retry the bank covers), takes its
    /// baseline from its next play-state frame (READY's; here a loading
    /// frame stands in), not from the wait page drawn between.
    #[test]
    fn a_generic_page_does_not_turn_the_turntable() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&seen);
        let make: MakeScreen =
            Box::new(move || Ok(Box::new(Turning(Arc::clone(&noted))) as Box<dyn Screen>));
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let clock = QpcClock::start();
        let link = spawn(
            make,
            Some(Arc::clone(&chart)),
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.play_state(game::PlayState {
            number: 1,
            origin: clock.now(),
        });
        link.prepare().unwrap();
        let ready = link.prepare_attempt(attempt(&chart, None), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);
        link.send(RenderMsg::Menu(Box::new(one_line("wait"))));
        std::thread::sleep(Duration::from_millis(150));
        link.loading().show(one_line("L"));
        assert!(wait_until(|| !seen.lock().unwrap().is_empty()));
        assert_eq!(seen.lock().unwrap()[0].0, 0, "the wait page turned it");
        link.end_loading().unwrap();
        link.exit();
    }

    /// A play frame's turntable angle and chart time.
    type PlayTurned = (i32, TimeUs);

    /// A screen noting every play frame's turntable angle ([`PlayTurned`]).
    struct PlayTurning(Arc<std::sync::Mutex<Vec<PlayTurned>>>);

    impl Screen for PlayTurning {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            self.0.lock().unwrap().push((f.turntable, f.now));
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The play frames carry the turntable through the render loop: a
    /// play state adopted at its attempt's preparation takes its baseline
    /// from the first READY frame (0); each frame's angle is the previous
    /// frames' ticks, idle at one unit a µs of the frame clock; and the
    /// committed attempt's autoplay notes press the scratch from the play's
    /// start, turning it three times as fast (Positive) after a tap.
    #[test]
    fn play_frames_carry_the_turntable_and_autoplay() {
        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(60_000_000))
            .build()
            .unwrap();
        let (rule, device) = (rules::tapestry(), DeviceProfile::default());
        let (_logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            Vec::new(),
            LogicConfig {
                inputs: 0,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&seen);
        let make: MakeScreen =
            Box::new(move || Ok(Box::new(PlayTurning(Arc::clone(&noted))) as Box<dyn Screen>));
        let clock = QpcClock::start();
        let shared_chart = Arc::new(chart.clone());
        let link = spawn(
            make,
            Some(Arc::clone(&shared_chart)),
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.play_state(game::PlayState {
            number: 1,
            origin: clock.now(),
        });
        // A tap on the scratch 150 ms into the play.
        let notes = vec![game::ScratchNote {
            time: TimeUs(150_000),
            kind: game::ScratchNoteKind::Tap,
        }];
        let ready = link.prepare_attempt(
            attempt(&shared_chart, None).with_scratch_notes(notes),
            || false,
        );
        assert_eq!(ready.unwrap(), Preparation::Ready);
        link.commit_attempt().unwrap();
        // READY for 100 ms before chart time 0.
        link.send(RenderMsg::Play {
            state_origin: TimeUs(0),
            side,
            gauge: 0,
            measure: play_measure(TimeUs(clock.now().0 + 100_000)),
        });
        assert!(wait_until(|| seen
            .lock()
            .unwrap()
            .last()
            .is_some_and(|f| f.1.0 > 400_000)));
        let _ = link.finish(clock.now());
        link.exit();
        let f = seen.lock().unwrap().clone();
        assert_eq!(f[0].0, 0, "the first READY frame adds 0");
        assert!(f[0].1.0 < 0, "the first frame is READY's");
        // The angle frame k + 1 draws less frame k's is frame k's tick:
        // its interval since frame k − 1 times its rate.
        let mut rates = Vec::new();
        for k in 1..f.len() - 1 {
            let turned = i64::from(f[k + 1].0 - f[k].0).rem_euclid(2_160_000);
            let dt = f[k].1.0 - f[k - 1].1.0;
            if dt > 0 {
                rates.push((f[k].1, turned as f64 / dt as f64));
            }
        }
        // The tap is crossed at the first frame at or after 150 ms and
        // pressed while its age in state ms is at most 80: rate 3 there,
        // 1 before it and after the expiry (a millisecond's floor apart).
        let crossed = f.iter().find(|x| x.1.0 >= 150_000).unwrap().1.0;
        let mut pressed = 0;
        for (now, r) in &rates {
            let expected = if now.0 >= crossed && now.0 <= crossed + 79_000 {
                pressed += 1;
                3.0
            } else if now.0 < crossed || now.0 > crossed + 82_000 {
                1.0
            } else {
                continue;
            };
            assert!((r - expected).abs() < 1e-9, "rate {r} at {now:?}");
        }
        assert!(pressed > 0, "no frame while the tap was pressed");
    }

    /// A practice retry from a round's result (docs/stage3.md, offset 1):
    /// the result pauses the play state's turntable, the retry resumes it
    /// and the next round's attempt is prepared with no practice page
    /// between. The resume applies as that preparation begins: the frames
    /// drawn between hold the angle, and the next round's frames turn on
    /// from it, its first adding 0.
    #[test]
    fn a_practice_retry_resumes_the_turntable_for_the_next_round() {
        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(60_000_000))
            .build()
            .unwrap();
        let (rule, device) = (rules::tapestry(), DeviceProfile::default());
        let side = || {
            Logic::<SourceReader>::new(
                &chart,
                &rule,
                &device,
                &InputStart::default(),
                Vec::new(),
                LogicConfig {
                    inputs: 0,
                    effects: 16,
                },
                Vec::new(),
            )
            .unwrap()
            .1
        };
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&seen);
        let make: MakeScreen =
            Box::new(move || Ok(Box::new(PlayTurning(Arc::clone(&noted))) as Box<dyn Screen>));
        let clock = QpcClock::start();
        let shared_chart = Arc::new(chart.clone());
        let link = spawn(
            make,
            Some(Arc::clone(&shared_chart)),
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.play_state(game::PlayState {
            number: 1,
            origin: clock.now(),
        });
        let play = |link: &super::RenderLink| {
            let ready = link.prepare_attempt(attempt(&shared_chart, None), || false);
            assert_eq!(ready.unwrap(), Preparation::Ready);
            link.commit_attempt().unwrap();
            link.send(RenderMsg::Play {
                state_origin: TimeUs(0),
                side: side(),
                gauge: 0,
                measure: play_measure(TimeUs(clock.now().0 + 50_000)),
            });
        };
        // Round 1 plays 200 ms, then goes on to its result: paused.
        play(&link);
        assert!(wait_until(|| seen
            .lock()
            .unwrap()
            .last()
            .is_some_and(|f| f.1.0 > 200_000)));
        link.turntable(1, true);
        assert!(link.fence(Duration::from_secs(5)));
        let paused_at = seen.lock().unwrap().len();
        // The retry resumes it; old play frames drawn before the next
        // round's preparation hold the angle.
        std::thread::sleep(Duration::from_millis(100));
        link.turntable(1, false);
        assert!(link.fence(Duration::from_secs(5)));
        std::thread::sleep(Duration::from_millis(100));
        let _ = link.finish(clock.now());
        let round2 = seen.lock().unwrap().len();
        let held = seen.lock().unwrap()[paused_at].0;
        assert!(
            seen.lock().unwrap()[paused_at..round2]
                .iter()
                .all(|f| f.0 == held),
            "frames between the pause and the next round's preparation"
        );
        // Round 2, with no practice page between: its frames turn on from
        // the held angle.
        play(&link);
        assert!(wait_until(|| seen.lock().unwrap().len() > round2 + 1
            && seen.lock().unwrap().last().is_some_and(|f| f.1.0 > 100_000)));
        let _ = link.finish(clock.now());
        link.exit();
        let f = seen.lock().unwrap().clone();
        let next = &f[round2..];
        assert_eq!(next[0].0, held, "the next round's first frame adds 0");
        let last = next[next.len() - 1];
        let turned = i64::from(last.0 - held).rem_euclid(2_160_000);
        assert!(
            turned > 50_000,
            "the next round's frames turn on: {turned} units by {:?}",
            last.1
        );
    }

    /// A render thread drawing on a [`ClockedLoading`], and its frames.
    fn clocked() -> (
        super::RenderLink,
        QpcClock,
        Arc<std::sync::Mutex<Vec<Clocked>>>,
    ) {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&seen);
        let make: MakeScreen =
            Box::new(move || Ok(Box::new(ClockedLoading(Arc::clone(&noted))) as Box<dyn Screen>));
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
        (link, clock, seen)
    }

    /// A gated opening deferred by a hold returns to the loading state it
    /// paused (design.md "Play Lifecycle", the start gate): no loading
    /// frame while paused, though lines arrive, then the same lifetime from
    /// the same origin on the loading clock, its time gone on through the
    /// pause, with the lines reported meanwhile.
    #[test]
    fn a_paused_loading_lifetime_resumes_where_it_was() {
        let (link, clock, seen) = clocked();
        let count = || seen.lock().unwrap().len();
        link.loading().show(one_line("L"));
        assert!(wait_until(|| count() >= 2));
        let (lifetime, since, _, _) = seen.lock().unwrap()[0].clone();
        link.pause_loading().unwrap();
        let paused = count();
        link.loading().show(one_line("M"));
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(count(), paused, "a loading frame while paused");
        let resumed = clock.now();
        link.resume_loading();
        assert!(wait_until(|| count() > paused));
        link.end_loading().unwrap();
        link.exit();
        let seen = seen.lock().unwrap();
        for (l, s, now, line) in &seen[paused..] {
            assert_eq!((*l, *s), (lifetime, since), "the lifetime started over");
            assert!(*now >= resumed, "the loading clock went back");
            assert_eq!(line, "M", "the lines reported while paused were lost");
        }
    }

    /// The player's gated opening through its gate: the end of loading
    /// pauses the lifetime, a hold at the opening resumes the same lifetime
    /// from its origin, the end of loading after the release pauses it
    /// again, and the opening ends it, so nothing resumes it any more.
    #[test]
    fn a_gated_opening_held_returns_to_its_loading_state() {
        let (link, clock, seen) = clocked();
        let controls = Arc::new(crate::controls::Controls::new(clock));
        let (_hook, input) = crate::input_side::link(
            controls,
            crate::input_side::Mode::Clock,
            input::Waker::new().unwrap(),
        );
        let (mut tap, _rx) = input::adjustment_queue(input::AdjustmentBindings {
            controls: vec![(input::TapControl::Key(eval::ControlId(0x10)), 0)],
            latched: 1,
            ..input::AdjustmentBindings::default()
        })
        .unwrap();
        tap.set_active(true);
        let mut gate = crate::start_gate::PlayerGate::install(&input, tap);
        let count = || seen.lock().unwrap().len();
        link.loading().show(one_line("L"));
        assert!(wait_until(|| count() >= 2));
        let (lifetime, since, _, _) = seen.lock().unwrap()[0].clone();
        // Recording is to open: the loading state pauses.
        gate.end_loading(&link, true).unwrap();
        let paused = count();
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(count(), paused, "a loading frame before the opening");
        // START held there: the opening is deferred, the loading state back.
        let resumed = clock.now();
        gate.defer(&link);
        assert!(wait_until(|| count() > paused));
        // Released: paused again, then the candidate opens.
        gate.end_loading(&link, false).unwrap();
        let again = count();
        gate.end_paused_loading(&link);
        gate.defer(&link);
        std::thread::sleep(Duration::from_millis(100));
        link.exit();
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), again, "a loading frame after the opening");
        for (l, s, now, _) in &seen[paused..] {
            assert_eq!((*l, *s), (lifetime, since), "the lifetime started over");
            assert!(*now >= resumed, "the loading clock went back");
        }
    }

    /// Nothing is resumed without a loading lifetime paused, nor once the
    /// paused one has ended: the pause answers as an end would.
    #[test]
    fn no_loading_frame_resumes_without_its_lifetime() {
        let (link, _, seen) = clocked();
        link.pause_loading().unwrap();
        link.resume_loading();
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            seen.lock().unwrap().is_empty(),
            "a loading frame of nothing"
        );
        link.loading().show(one_line("L"));
        assert!(wait_until(|| !seen.lock().unwrap().is_empty()));
        link.pause_loading().unwrap();
        let paused = seen.lock().unwrap().len();
        link.clear_loading();
        link.resume_loading();
        std::thread::sleep(Duration::from_millis(100));
        link.exit();
        assert_eq!(
            seen.lock().unwrap().len(),
            paused,
            "an ended lifetime resumed"
        );
    }

    /// The end of loading is refused, so recording does not open, when the
    /// render thread stalls in a loading frame past the answer's bound, and
    /// when it is gone.
    #[test]
    fn an_unconfirmed_end_of_loading_is_an_error() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stall = Arc::new(AtomicBool::new(false));
        let stalled = Arc::new(AtomicBool::new(false));
        let (noted, held, at) = (Arc::clone(&seen), Arc::clone(&stall), Arc::clone(&stalled));
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(StalledMenus {
                stall: Arc::clone(&held),
                stalled: Arc::clone(&at),
                seen: Arc::clone(&noted),
                answer: Arc::default(),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let mut link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.set_timeouts(Duration::from_millis(200), Duration::from_secs(5));
        link.loading().show(one_line("L"));
        assert!(wait_until(|| !seen.lock().unwrap().is_empty()));
        stall.store(true, Ordering::Release);
        assert!(wait_until(|| stalled.load(Ordering::Acquire)));
        assert!(matches!(link.end_loading(), Err(ScreenError::NoAnswer)));
        stall.store(false, Ordering::Release);
        // The thread ends; nothing answers any more.
        link.send(RenderMsg::Exit);
        assert!(wait_until(
            || link.ended.try_recv() == Err(std::sync::mpsc::TryRecvError::Disconnected)
        ));
        assert!(matches!(link.end_loading(), Err(ScreenError::Ended)));
    }

    /// A load cancelled while the render thread stalls with its signal
    /// unread, the select page sent and another chart chosen at once: the
    /// cancelled load's signal takes nothing, and the next load's lines
    /// get a signal of their own, drawn after the select page and kept
    /// through its chart's load.
    #[test]
    fn a_cancelled_loads_signal_leaves_the_next_loads_lines() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let stall = Arc::new(AtomicBool::new(false));
        let stalled = Arc::new(AtomicBool::new(false));
        let (noted, held, at) = (Arc::clone(&seen), Arc::clone(&stall), Arc::clone(&stalled));
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(StalledMenus {
                stall: Arc::clone(&held),
                stalled: Arc::clone(&at),
                seen: Arc::clone(&noted),
                answer: Arc::default(),
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
        link.send(RenderMsg::Menu(Box::new(one_line("select"))));
        assert!(wait_until(|| !seen.lock().unwrap().is_empty()));
        stall.store(true, Ordering::Release);
        assert!(wait_until(|| stalled.load(Ordering::Acquire)));
        // Chart A: its lines and signal, then the quit key cancels it and
        // selection is entered; chart B is chosen at once.
        link.loading().show(one_line("A"));
        link.clear_loading();
        link.send(RenderMsg::Menu(Box::new(one_line("select"))));
        link.loading().show(one_line("B"));
        stall.store(false, Ordering::Release);
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        link.load(chart, None).unwrap();
        std::thread::sleep(Duration::from_millis(100));
        link.exit();
        let seen = seen.lock().unwrap().clone();
        let lines: Vec<&str> = seen.iter().map(|(l, _)| l.as_str()).collect();
        assert!(!lines.contains(&"A"), "{lines:?}");
        let last_select = lines.iter().rposition(|l| *l == "select").unwrap();
        assert!(
            lines[last_select..].contains(&"B"),
            "B not drawn after the select page: {lines:?}"
        );
        assert_eq!(lines.last(), Some(&"B"), "{lines:?}");
    }

    /// A screen that cannot draw any chart; it notes its menu frames by the
    /// number of the build that made it.
    struct NoCharts {
        id: u32,
        seen: Arc<std::sync::Mutex<Vec<u32>>>,
    }

    impl Screen for NoCharts {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Err(RenderError::Skin(SkinError {
                reason: "no lanes".to_owned(),
            }))
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            self.seen.lock().unwrap().push(self.id);
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// A screen with a splash page: it counts its splash and menu frames.
    struct Splashes {
        splashes: Arc<AtomicU32>,
        menus: Arc<AtomicU32>,
    }

    impl Screen for Splashes {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn splash(&mut self, _elapsed: Duration) -> Option<Result<bool, RenderError>> {
            self.splashes.fetch_add(1, Ordering::SeqCst);
            Some(Ok(true))
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            self.menus.fetch_add(1, Ordering::SeqCst);
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The render thread draws the splash page from its start, before the
    /// game sends anything, and only until the first page replaces it.
    #[test]
    fn the_splash_shows_out_its_time_then_the_first_page() {
        let (splashes, menus) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        let (s, m) = (Arc::clone(&splashes), Arc::clone(&menus));
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Splashes {
                splashes: Arc::clone(&s),
                menus: Arc::clone(&m),
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
        let spawned = Instant::now();
        assert!(wait_until(|| splashes.load(Ordering::SeqCst) >= 2));
        // The page, ready early, waits for the launch screen's end.
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| menus.load(Ordering::SeqCst) >= 1));
        assert!(spawned.elapsed() >= crate::menu_screen::SPLASH);
        let drawn = splashes.load(Ordering::SeqCst);
        assert!(wait_until(|| menus.load(Ordering::SeqCst) >= 3));
        link.exit();
        assert_eq!(splashes.load(Ordering::SeqCst), drawn);
    }

    /// A page that comes after the launch screen's time replaces it at
    /// once: the frame after the page was taken draws it, and no splash
    /// frame follows.
    #[test]
    fn a_page_after_the_splash_time_shows_at_once() {
        let (splashes, menus) = (Arc::new(AtomicU32::new(0)), Arc::new(AtomicU32::new(0)));
        let (s, m) = (Arc::clone(&splashes), Arc::clone(&menus));
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Splashes {
                splashes: Arc::clone(&s),
                menus: Arc::clone(&m),
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
        std::thread::sleep(crate::menu_screen::SPLASH + Duration::from_millis(100));
        assert_eq!(menus.load(Ordering::SeqCst), 0);
        link.send(RenderMsg::Menu(Box::default()));
        assert!(link.fence(super::hook_bound()));
        assert!(menus.load(Ordering::SeqCst) >= 1);
        let drawn = splashes.load(Ordering::SeqCst);
        let shown = menus.load(Ordering::SeqCst);
        assert!(wait_until(|| menus.load(Ordering::SeqCst) >= shown + 2));
        link.exit();
        assert_eq!(splashes.load(Ordering::SeqCst), drawn);
    }

    /// A preparation (a play's, before it opens) leaves the splash drawn,
    /// whether it comes before the splash's first frame or after: the
    /// hidden window shows on a presented frame, and the play waits for
    /// the show.
    #[test]
    fn a_preparation_keeps_the_splash_drawn() {
        for prepare_first in [true, false] {
            let splashes = Arc::new(AtomicU32::new(0));
            let s = Arc::clone(&splashes);
            let make: MakeScreen = Box::new(move || {
                Ok(Box::new(Splashes {
                    splashes: Arc::clone(&s),
                    menus: Arc::new(AtomicU32::new(0)),
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
            if !prepare_first {
                assert!(wait_until(|| splashes.load(Ordering::SeqCst) >= 1));
            }
            assert!(link.prepare().is_ok());
            let drawn = splashes.load(Ordering::SeqCst);
            assert!(
                wait_until(|| splashes.load(Ordering::SeqCst) >= drawn + 2),
                "prepare first: {prepare_first}"
            );
            link.exit();
        }
    }

    /// A screen that notes the section of every chart it prepares.
    struct Sections(Arc<std::sync::Mutex<Vec<Option<eval::Section>>>>);

    impl Screen for Sections {
        fn load(
            &mut self,
            _chart: &Chart,
            section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            self.0.lock().unwrap().push(section.cloned());
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The section a chart is spawned or loaded with reaches the screen;
    /// the screen's recovery prepares nothing, and every attempt prepares
    /// the chart with its own section, or none for the whole chart.
    #[test]
    fn each_attempt_prepares_with_its_own_section() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&seen);
        let make: MakeScreen =
            Box::new(move || Ok(Box::new(Sections(Arc::clone(&noted))) as Box<dyn Screen>));
        let section = |s: i64| eval::Section {
            start: TimeUs(s),
            end: TimeUs(s + 1_000_000),
            lead_in: TimeUs(2_000_000),
            initial_gauges: Vec::new(),
        };
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let clock = QpcClock::start();
        let link = spawn(
            make,
            Some(Arc::clone(&chart)),
            Some(section(1)),
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.load(Arc::clone(&chart), Some(section(2))).unwrap();
        link.prepare().unwrap();
        let ready = link.prepare_attempt(attempt(&chart, Some(&section(3))), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);
        link.prepare().unwrap();
        let ready = link.prepare_attempt(attempt(&chart, None), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);
        assert_eq!(
            *seen.lock().unwrap(),
            [Some(section(1)), Some(section(2)), Some(section(3)), None]
        );
        link.exit();
    }

    /// The attempt of `chart` the game would make: the default preset,
    /// the whole chart or `section`'s round.
    fn attempt(chart: &Chart, section: Option<&eval::Section>) -> AttemptContext {
        preset_attempt(chart, &rules::tapestry(), section, "")
    }

    /// How long the tests that upload an atlas at the per-atlas cap wait for
    /// an answer: WARP builds those 256 Mi texels on the CPU, which takes
    /// past [`LOAD_TIMEOUT`] on a loaded machine (the answer is then
    /// `NoAnswer` though nothing failed).
    #[cfg(feature = "dev-tools")]
    const ATLAS_CAP_ANSWER: Duration = Duration::from_secs(180);

    /// A render thread on the screens `make` builds, with no chart, no
    /// section and no frame sink.
    pub(crate) fn spawn_empty(make: MakeScreen) -> super::RenderLink {
        let clock = QpcClock::start();
        spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap()
    }

    /// A screen whose skin refuses every section's attempt and fails its
    /// device on a whole chart's while `lost` is set; it notes its menu
    /// frames by the number of the build that made it.
    struct Refusing {
        id: u32,
        lost: Arc<AtomicBool>,
        seen: Arc<std::sync::Mutex<Vec<u32>>>,
    }

    impl Screen for Refusing {
        fn load(
            &mut self,
            _chart: &Chart,
            section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            if section.is_some() {
                Err(RenderError::Skin(SkinError {
                    reason: "no section".to_owned(),
                }))
            } else if self.lost.load(Ordering::Acquire) {
                Err(RenderError::SurfaceLost)
            } else {
                Ok(())
            }
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            self.seen.lock().unwrap().push(self.id);
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// An attempt the skin refuses is answered with the skin's reason,
    /// reports no failure and keeps the screen, which goes on drawing the
    /// menus without being built again; a device failure while preparing
    /// is an error that drops the screen, which the recovery builds again.
    #[test]
    fn a_refused_attempt_keeps_the_screen_and_reports_no_failure() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let lost = Arc::new(AtomicBool::new(false));
        let builds = Arc::new(AtomicU32::new(0));
        let (made, noted, gone) = (Arc::clone(&builds), Arc::clone(&seen), Arc::clone(&lost));
        let make: MakeScreen = Box::new(move || {
            let id = made.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(Box::new(Refusing {
                id,
                lost: Arc::clone(&gone),
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let link = spawn_empty(make);
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        link.load(Arc::clone(&chart), None).unwrap();
        let round = eval::Section {
            start: TimeUs(0),
            end: TimeUs(1_000_000),
            lead_in: TimeUs(2_000_000),
            initial_gauges: Vec::new(),
        };
        let refused = link.prepare_attempt(attempt(&chart, Some(&round)), || false);
        assert_eq!(
            refused.unwrap(),
            Preparation::Refused(game::PreparationRefusal {
                location: None,
                reason: "no section".to_owned(),
            })
        );
        assert!(!link.failed(), "a refusal reported a failure");
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| seen.lock().unwrap().len() >= 3));
        assert!(!link.failed());
        assert_eq!(
            builds.load(Ordering::SeqCst),
            1,
            "the screen was built again"
        );
        assert!(seen.lock().unwrap().iter().all(|id| *id == 1));
        let ready = link.prepare_attempt(attempt(&chart, None), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);

        lost.store(true, Ordering::Release);
        let failed = link.prepare_attempt(attempt(&chart, None), || false);
        assert!(
            matches!(failed, Err(ScreenError::Render(RenderError::SurfaceLost))),
            "{failed:?}"
        );
        lost.store(false, Ordering::Release);
        link.prepare().unwrap();
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        link.exit();
    }

    /// A pending play skin's specialization staged with its attempt
    /// through a real menu screen and renderer on WARP, beside a committed
    /// selection skin charged what the shared limit leaves beside an atlas's
    /// cap, and a tiny atlas's texels more
    /// (`Renderer::state_charge`, development builds, so no array that
    /// large is uploaded): one whose atlas is the cap passes the shared
    /// limit by the tiny atlas and is refused with the renderer's
    /// admission, as a healthy refusal (no failure reported), and one
    /// within the limit is ready and commits.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_specialization_past_the_shared_texels_is_refused() {
        use crate::render_decl::{Look, atlas_cap_bundle, solid_bundle};
        const TINY: u64 = 256 * 256 * 2;
        // Built for WARP's limits, as the render thread's screen.
        let r = crate::render_common::offscreen(1_920, 1_080);
        let big = atlas_cap_bundle(&r, [0, 0, 255, 255]);
        let (result, play) = (
            solid_bundle(&r, &Look::left([255, 0, 0, 255]), [16, 16]),
            solid_bundle(&r, &Look::left([0, 255, 0, 255]), [16, 16]),
        );
        drop(r);
        assert_eq!(result.atlas_texels(), TINY);
        let stated = Arc::clone(&result);
        let make: MakeScreen = Box::new(move || {
            let mut renderer = render::Renderer::new(
                render::Target::Offscreen {
                    width: 1_920,
                    height: 1_080,
                },
                render::AdapterChoice::Warp,
            )?;
            renderer.state_charge(
                &stated,
                render::SHARED_TEXELS - skin_model::MAX_ATLAS_TEXELS + TINY,
            );
            Ok(Box::new(crate::menu_screen::MenuScreen::new(renderer, None)) as Box<dyn Screen>)
        });
        let mut link = spawn_empty(make);
        link.set_timeouts(ATLAS_CAP_ANSWER, Duration::from_secs(10));
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        link.load(Arc::clone(&chart), None).unwrap();
        assert_eq!(
            link.select(ScreenKind::Selection, Some(result))
                .wait()
                .unwrap(),
            Preparation::Ready
        );
        assert_eq!(
            link.pend(ScreenKind::Play).wait().unwrap(),
            Preparation::Ready
        );
        let answer = specialized(&link, &chart, &big).unwrap();
        let Preparation::Refused(why) = answer else {
            panic!("{answer:?}");
        };
        let needed = render::SHARED_TEXELS + 256 * 256 * 2;
        assert_eq!(
            why.reason,
            format!(
                "the {} skin needs {needed} texels with the other screens' skins, past {}",
                ScreenKind::Play,
                render::SHARED_TEXELS
            )
        );
        assert!(!link.failed());
        // The positive control: a specialization within the limit.
        assert_eq!(
            specialized(&link, &chart, &play).unwrap(),
            Preparation::Ready
        );
        link.commit_attempt().unwrap();
        link.exit();
    }

    /// A pending play skin's specialization that only the committed result
    /// skin's charge keeps out (charged as in
    /// [`a_specialization_past_the_shared_texels_is_refused`]) is ready:
    /// the result's installation is released before play is admitted
    /// (design.md "Memory Budget"), and it commits.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_specialization_kept_out_by_the_result_alone_releases_it() {
        use crate::render_decl::{Look, atlas_cap_bundle, solid_bundle};
        const TINY: u64 = 256 * 256 * 2;
        // Built for WARP's limits, as the render thread's screen.
        let r = crate::render_common::offscreen(1_920, 1_080);
        let big = atlas_cap_bundle(&r, [0, 0, 255, 255]);
        let result = solid_bundle(&r, &Look::left([255, 0, 0, 255]), [16, 16]);
        drop(r);
        assert_eq!(result.atlas_texels(), TINY);
        let stated = Arc::clone(&result);
        let make: MakeScreen = Box::new(move || {
            let mut renderer = render::Renderer::new(
                render::Target::Offscreen {
                    width: 1_920,
                    height: 1_080,
                },
                render::AdapterChoice::Warp,
            )?;
            renderer.state_charge(
                &stated,
                render::SHARED_TEXELS - skin_model::MAX_ATLAS_TEXELS + TINY,
            );
            Ok(Box::new(crate::menu_screen::MenuScreen::new(renderer, None)) as Box<dyn Screen>)
        });
        let mut link = spawn_empty(make);
        link.set_timeouts(ATLAS_CAP_ANSWER, Duration::from_secs(10));
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        link.load(Arc::clone(&chart), None).unwrap();
        let id = result.atlas_id();
        assert_eq!(
            link.select(ScreenKind::Result, Some(result))
                .wait()
                .unwrap(),
            Preparation::Ready
        );
        assert_eq!(link.result_status().installed, Some(id));
        assert_eq!(
            link.pend(ScreenKind::Play).wait().unwrap(),
            Preparation::Ready
        );
        assert_eq!(
            specialized(&link, &chart, &big).unwrap(),
            Preparation::Ready
        );
        assert!(!link.failed());
        // The release is reconciled: nothing of the result is kept to
        // restore, and the status says so (the S10-09i design's Q3).
        let status = link.result_status();
        assert_eq!(status.installed, None);
        assert!(!status.retained.contains(&id), "{status:?}");
        link.commit_attempt().unwrap();
        link.exit();
    }

    /// A refused shared admission allocates nothing (design.md "Memory
    /// Budget"): with play's tiny skin committed and charged what the
    /// shared limit leaves beside an atlas's cap
    /// (`Renderer::state_charge`) and a tiny result skin committed, a
    /// selection candidate at the cap is past the shared limit by the tiny
    /// atlas. It is refused before anything is allocated or staged, and
    /// play's installation draws its next frame as before. The candidate
    /// is never uploaded. A positive control proves the counter counts.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_refused_shared_admission_alloc() {
        use crate::alloc_count::this_thread;
        use crate::render_common::{GAUGES, attempt, context, input, offscreen, view};
        use crate::render_decl::{Look, atlas_cap_bundle, solid_bundle};
        const TINY: u64 = 256 * 256 * 2;
        let _warp = crate::warp_turn::take();
        let mut r = offscreen(1_920, 1_080);
        let big = atlas_cap_bundle(&r, [0, 0, 255, 255]);
        let tiny = solid_bundle(&r, &Look::left([255, 0, 0, 255]), [16, 16]);
        assert_eq!(tiny.atlas_texels(), TINY);
        let chart = ChartBuilder::new(120_000).build().unwrap();
        let ctx = context(&GAUGES, false);
        let token = r.stage(&tiny, &attempt(&r, &chart, &ctx), 1).unwrap();
        r.commit(token).unwrap();
        r.state_charge(&tiny, render::SHARED_TEXELS - skin_model::MAX_ATLAS_TEXELS);
        let result = solid_bundle(&r, &Look::left([0, 255, 0, 255]), [16, 16]);
        let token = r.install(ScreenKind::Result, &result, 2).unwrap();
        r.commit(token).unwrap();
        let drawn = |r: &mut render::Renderer| {
            let v = view(0, 0);
            let timing = r.frame(&input(&v, TimeUs(0), TimeUs(0), &[])).unwrap();
            assert!(timing.presented);
            let pixels = crate::render_common::read_back(r);
            (
                timing.sprites,
                crate::render_common::rgb_at(&pixels, 1_920, (480, 540)),
            )
        };
        let before = drawn(&mut r);
        assert!(before.0 > 0);
        assert_eq!(before.1, [255, 0, 0]);
        let generation = r.resource_generation();

        let control = this_thread();
        drop(std::hint::black_box(vec![1_u8; 8]));
        assert!(this_thread() > control, "the allocation was not counted");
        let start = this_thread();
        let answer = r.install(ScreenKind::Selection, &big, 3);
        let allocs = this_thread() - start;
        let e = answer.unwrap_err();
        assert!(
            matches!(
                e,
                RenderError::Admission {
                    kind: ScreenKind::Selection,
                    needed,
                    limit,
                } if needed == render::SHARED_TEXELS + TINY && limit == render::SHARED_TEXELS
            ),
            "{e:?}"
        );
        assert!(e.is_refusal());
        assert_eq!(allocs, 0, "the refused admission allocated");
        assert!(r.staged().is_none());
        assert!(Arc::ptr_eq(r.installed(ScreenKind::Play).unwrap(), &tiny));
        assert_eq!(r.resource_generation(), generation);
        assert_eq!(drawn(&mut r), before);
    }

    /// The default skin's refusal of an attempt (a chart row past its
    /// exact playhead's domain) through a real menu screen and renderer on
    /// WARP: the attempt is refused with the skin's reason, no failure is
    /// reported, the screen is not built again, and the next attempt is
    /// ready on it.
    #[test]
    fn the_default_skins_refusal_keeps_the_screen() {
        let builds = Arc::new(AtomicU32::new(0));
        let made = Arc::clone(&builds);
        let make: MakeScreen = Box::new(move || {
            made.fetch_add(1, Ordering::SeqCst);
            let renderer = render::Renderer::new(
                render::Target::Offscreen {
                    width: 1_920,
                    height: 1_080,
                },
                render::AdapterChoice::Warp,
            )?;
            Ok(Box::new(crate::menu_screen::MenuScreen::new(renderer, None)) as Box<dyn Screen>)
        });
        let link = spawn_empty(make);
        let key = Lane::new(1).unwrap();
        let past = Arc::new(
            ChartBuilder::new(120_000)
                .tap(key, TimeUs(bms_model::DOMAIN + 1))
                .build()
                .unwrap(),
        );
        link.load(Arc::clone(&past), None).unwrap();
        let refused = link.prepare_attempt(attempt(&past, None), || false);
        let Ok(Preparation::Refused(why)) = refused else {
            panic!("{refused:?}");
        };
        assert!(
            why.reason.contains("outside the playhead's range"),
            "{why:?}"
        );
        assert!(!link.failed(), "a refusal reported a failure");
        let chart = Arc::new(
            ChartBuilder::new(120_000)
                .tap(key, TimeUs(2_000_000))
                .build()
                .unwrap(),
        );
        link.load(Arc::clone(&chart), None).unwrap();
        let ready = link.prepare_attempt(attempt(&chart, None), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);
        assert!(!link.failed());
        assert_eq!(
            builds.load(Ordering::SeqCst),
            1,
            "the screen was built again"
        );
        link.exit();
    }

    /// A screen whose chart preparation blocks while `hold` is set.
    struct Held(Arc<AtomicBool>);

    impl Screen for Held {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            while self.0.load(Ordering::Acquire) {
                std::thread::sleep(Duration::from_millis(1));
            }
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// A slow attempt's preparation stops as soon as the player leaves or
    /// the input or audio thread fails, without waiting for the screen's
    /// answer; the game then looks at the threads for the cause.
    #[test]
    fn a_slow_attempt_stops_when_the_player_leaves_or_a_thread_fails() {
        let hold = Arc::new(AtomicBool::new(false));
        let held = Arc::clone(&hold);
        let make: MakeScreen =
            Box::new(move || Ok(Box::new(Held(Arc::clone(&held))) as Box<dyn Screen>));
        let link = spawn_empty(make);
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        link.load(Arc::clone(&chart), None).unwrap();
        for cause in ["leave", "input", "audio"] {
            hold.store(true, Ordering::Release);
            let started = Instant::now();
            let at = started + Duration::from_millis(100);
            let answer = link.prepare_attempt(attempt(&chart, None), || {
                let now = Instant::now() >= at;
                crate::play::preparation_stops(
                    false,
                    now && cause == "leave",
                    (now && cause == "input").then_some(game::InputWaitError::Gone),
                    (now && cause == "audio").then_some(audio::OutputError::DeviceLost),
                )
            });
            assert_eq!(answer.unwrap(), Preparation::Cancelled, "{cause}");
            assert!(
                started.elapsed() < Duration::from_secs(2),
                "{cause}: {:?}",
                started.elapsed()
            );
            hold.store(false, Ordering::Release);
        }
        link.exit();
    }

    /// A loading frame that fails after the attempt was prepared drops the
    /// screen: the end of loading says so, and recording must not open.
    #[test]
    fn the_end_of_loading_reports_a_screen_lost_after_the_preparation() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&seen);
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Menus {
                // The first build: its third frame fails.
                id: 1,
                frames: 0,
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let link = spawn_empty(make);
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        link.load(Arc::clone(&chart), None).unwrap();
        let ready = link.prepare_attempt(attempt(&chart, None), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);
        link.loading().show(one_line("L"));
        assert!(wait_until(|| link.failed()));
        assert!(matches!(link.end_loading(), Err(ScreenError::Lost)));
        link.exit();
    }

    /// What a loading frame was drawn with: the page's first line, the
    /// state, and the frame's load.
    type Seen = (String, Option<LoadingUpdate>, skin_model::LoadState);

    /// A screen that notes every loading frame's first line, state and
    /// load.
    type Typed = Arc<std::sync::Mutex<Vec<Seen>>>;

    struct TypedLoading {
        seen: Typed,
    }

    impl Screen for TypedLoading {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn loading(
            &mut self,
            page: &crate::menu_screen::MenuPage,
            update: Option<&LoadingUpdate>,
            _changed: bool,
            frame: &render::LoadingFrame<'_>,
        ) -> Result<Result<FrameTiming, skin_model::BuildError>, RenderError> {
            let line = page
                .lines
                .first()
                .map_or_else(String::new, |l| l.text.clone());
            self.seen
                .lock()
                .unwrap()
                .push((line, update.cloned(), frame.load));
            Ok(Ok(timing(true)))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// A render thread drawing its loading frames on [`TypedLoading`],
    /// every presented frame's acknowledgement held from the start.
    fn held_loading() -> (super::RenderLink, Typed) {
        let seen: Typed = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&seen);
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(TypedLoading {
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let link = spawn_empty(make);
        link.hold_loading_ack(true);
        (link, seen)
    }

    /// L1, the render side: a loading lifetime's first presented frame is
    /// acknowledged when its presentation is let through, tagged with its
    /// lifetime, so the minimum ends exactly a second (no skin declaring
    /// more) after it; a later frame of the same lifetime changes nothing.
    #[test]
    fn the_minimum_counts_from_this_lifetimes_first_presented_frame() {
        let (link, _seen) = held_loading();
        link.begin_loading(&chosen("One"), None);
        let lifetime = link.loading_lifetime();
        link.loading()
            .report(LoadInput::Stage(Stage::Chart), Some(one_line("A")));
        assert!(wait_until(|| link.loading_ack_held()));
        assert_eq!(link.loading_ack(), None, "held: not acknowledged");
        let let_through = link.clock.now();
        link.pass_loading_ack();
        assert!(wait_until(|| link.loading_acked() == 1));
        let (acked, at) = link.loading_ack().unwrap();
        assert_eq!(acked, lifetime);
        assert!(at >= let_through, "{at:?} {let_through:?}");
        assert_eq!(link.loading_loadend(), None);
        assert_eq!(
            game::minimum_ends(at, link.loading_loadend()),
            TimeUs(at.0 + 1_000_000)
        );
        // A later frame of the same lifetime.
        link.loading()
            .report(LoadInput::Stage(Stage::Loudness), Some(one_line("B")));
        assert!(wait_until(|| link.loading_ack_held()));
        link.pass_loading_ack();
        assert!(wait_until(|| link.loading_acked() == 2));
        assert_eq!(link.loading_ack(), Some((lifetime, at)));
        link.hold_loading_ack(false);
        link.exit();
    }

    /// N2: a lifetime made at the choice and held is drawn only from its
    /// reveal at the decision screen's deadline, its state published again
    /// from the reveal on the loading clock, so its timers start there:
    /// every loading frame drawn carries an origin at or after the reveal,
    /// and the first one's acknowledgement is the lifetime's.
    #[test]
    fn a_held_lifetime_is_drawn_from_its_reveal() {
        let (link, seen) = held_loading();
        link.begin_loading_held(&chosen("One"), None);
        let lifetime = link.loading_lifetime();
        let made = link.clock.now();
        link.loading()
            .report(LoadInput::Stage(Stage::Chart), Some(one_line("A")));
        // Strictly later on the loading clock than the lifetime's making.
        while link.clock.now() == made {
            std::thread::yield_now();
        }
        let revealed = link.clock.now();
        link.reveal_loading();
        assert!(wait_until(|| link.loading_ack_held()));
        link.pass_loading_ack();
        assert!(wait_until(|| link.loading_acked() == 1));
        assert_eq!(link.loading_ack().map(|(l, _)| l), Some(lifetime));
        let frames = seen.lock().unwrap().clone();
        assert!(!frames.is_empty());
        for (_, update, _) in &frames {
            let origin = update.as_ref().map(|u| u.origin);
            assert!(
                origin.is_some_and(|o| o >= revealed),
                "{origin:?} {revealed:?}"
            );
        }
        link.hold_loading_ack(false);
        link.exit();
    }

    /// L2, the render side: a frame of the lifetime before, held across
    /// the next lifetime's beginning, is acknowledged for the lifetime it
    /// was drawn for, which a look for the new lifetime ignores; the new
    /// lifetime's own first frame then starts its minimum.
    #[test]
    fn an_older_lifetimes_frame_does_not_start_the_minimum() {
        let (link, _seen) = held_loading();
        link.begin_loading(&chosen("One"), None);
        let old = link.loading_lifetime();
        link.loading()
            .report(LoadInput::Stage(Stage::Chart), Some(one_line("A")));
        assert!(wait_until(|| link.loading_ack_held()));
        // The next lifetime begins while the old frame is held.
        link.begin_loading(&chosen("Two"), None);
        let new = link.loading_lifetime();
        assert_ne!(old, new);
        link.loading()
            .report(LoadInput::Stage(Stage::Chart), Some(one_line("C")));
        link.pass_loading_ack();
        assert!(wait_until(|| link.loading_acked() == 1));
        let (acked, old_at) = link.loading_ack().unwrap();
        assert_eq!(acked, old, "tagged with the lifetime it was drawn for");
        assert_eq!(
            link.loading_ack().filter(|&(l, _)| l == new),
            None,
            "nothing of the new lifetime"
        );
        assert!(wait_until(|| link.loading_ack_held()));
        let let_through = link.clock.now();
        link.pass_loading_ack();
        assert!(wait_until(|| link.loading_acked() == 2));
        let (acked, at) = link.loading_ack().unwrap();
        assert_eq!(acked, new);
        assert!(at >= let_through && at >= old_at, "{at:?}");
        link.hold_loading_ack(false);
        link.exit();
    }

    /// The render thread's acknowledgement of a presented loading frame,
    /// the first of a lifetime and a later one, allocates nothing, nor
    /// does the game thread's read of it.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_loading_ack_presents_without_allocating_alloc() {
        let shared = super::Shared::new();
        let clock = QpcClock::start();
        shared.presented_loading(1, None, || clock.now());
        let before = crate::alloc_count::this_thread();
        for lifetime in 2..10 {
            shared.presented_loading(lifetime, Some(TimeUs(1_500_000)), || clock.now());
            shared.presented_loading(lifetime, Some(TimeUs(1_500_000)), || clock.now());
            let read = shared.loading_ack();
            assert_eq!(read.first.map(|(l, _)| l), Some(lifetime));
            assert_eq!(read.loadend, Some(TimeUs(1_500_000)));
        }
        assert_eq!(crate::alloc_count::this_thread() - before, 0);
    }

    /// The loading text of chart `title`.
    fn chosen(title: &str) -> LoadingText {
        LoadingText {
            title: title.to_owned(),
            preset: "preset".to_owned(),
            chart: ChosenChart {
                title: Some(title.to_owned()),
                ..ChosenChart::default()
            },
            ..LoadingText::default()
        }
    }

    /// A loading lifetime's whole state reaches the screen with its page,
    /// numbered from 1 and from the origin stamped when it began; a state
    /// reported without lines keeps the page shown; a publication not
    /// newer than one accepted is refused; and a state of an ended
    /// lifetime is never drawn, the next lifetime numbering afresh from
    /// an origin of its own.
    #[test]
    fn a_loading_lifetimes_state_reaches_the_screen_whole_and_in_order() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&seen);
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(TypedLoading {
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let link = spawn_empty(make);
        let drawn =
            |what: &dyn Fn(&Seen) -> bool| wait_until(|| seen.lock().unwrap().iter().any(what));
        link.begin_loading(&chosen("One"), None);
        let sink = link.loading();
        sink.report(LoadInput::Stage(Stage::Chart), Some(one_line("A")));
        assert!(drawn(&|(l, _, _)| l == "A"));
        let four = Stage::Sounds { done: 4, files: 9 };
        sink.report(LoadInput::Stage(four), None);
        assert!(drawn(
            &|(l, u, _)| l == "A" && u.as_ref().is_some_and(|u| u.percent == 40)
        ));
        // An older publication: the slot accepted number 2.
        let old = seen
            .lock()
            .unwrap()
            .first()
            .and_then(|s| s.1.clone())
            .unwrap();
        assert!(!sink.offer(Some(one_line("old")), old));
        // The old lifetime's sink after loading ended, then the next one's.
        link.clear_loading();
        sink.report(LoadInput::Stage(Stage::Loudness), Some(one_line("stale")));
        link.begin_loading(&chosen("Two"), None);
        let next = link.loading();
        next.report(LoadInput::Stage(Stage::Chart), Some(one_line("C")));
        assert!(drawn(&|(l, _, _)| l == "C"));
        std::thread::sleep(Duration::from_millis(50));
        link.exit();
        let seen = seen.lock().unwrap().clone();
        let first = |line: &str| seen.iter().find(|(l, _, _)| l == line).cloned().unwrap();
        let (_, a, a_load) = first("A");
        let a = a.unwrap();
        assert_eq!(
            (a.seq, a.stage, a.percent, a.revision),
            (1, Stage::Chart, 0, 1)
        );
        assert_eq!(a.chart.title.as_deref(), Some("One"));
        assert_eq!(a_load.since, a.origin, "{seen:?}");
        assert_eq!(a_load.percent, 0);
        let forty = seen
            .iter()
            .find(|(_, u, _)| u.as_ref().is_some_and(|u| u.percent == 40))
            .unwrap();
        assert_eq!(forty.1.as_ref().unwrap().seq, 2);
        assert_eq!(forty.2.percent, 40);
        assert_eq!(forty.2.since, a.origin);
        assert!(
            !seen.iter().any(|(l, _, _)| l == "stale" || l == "old"),
            "{seen:?}"
        );
        let (_, c, c_load) = first("C");
        let c = c.unwrap();
        assert_eq!((c.seq, c.percent), (1, 0));
        assert_eq!(c.chart.title.as_deref(), Some("Two"));
        assert!(c.origin > a.origin, "{seen:?}");
        assert_eq!(c_load.since, c.origin);
        // Nothing older than the newest state drawn before it.
        let mut newest = 0;
        for (l, u, _) in &seen {
            if l == "A" {
                let seq = u.as_ref().unwrap().seq;
                assert!(seq >= newest, "{seen:?}");
                newest = seq;
            }
        }
    }

    /// A chart the screen cannot draw is refused, and the select screen
    /// still draws afterwards, on a screen built again without the chart:
    /// no failure is left for the game to find.
    #[test]
    fn a_refused_chart_leaves_a_working_select_screen() {
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let builds = Arc::new(AtomicU32::new(0));
        let (made, noted) = (Arc::clone(&builds), Arc::clone(&seen));
        let make: MakeScreen = Box::new(move || {
            let id = made.fetch_add(1, Ordering::SeqCst) + 1;
            Ok(Box::new(NoCharts {
                id,
                seen: Arc::clone(&noted),
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
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        assert!(matches!(
            link.load(chart, None),
            Err(ScreenError::Render(RenderError::Skin(_)))
        ));
        assert!(!link.failed());
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| seen.lock().unwrap().contains(&2)));
        assert!(!link.failed());
        link.exit();
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    /// A screen whose chart preparation blocks until `release` sends.
    struct Blocking {
        release: std::sync::mpsc::Receiver<()>,
    }

    impl Screen for Blocking {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            let _ = self.release.recv();
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// A play's figures are waited for as long as the bound set says: a
    /// render thread blocked in a chart's load does not answer the play's
    /// end, and the wait gives up at the bound, far below the production
    /// one.
    #[test]
    fn a_blocked_thread_bounds_the_figures_wait() {
        let (release, blocked) = std::sync::mpsc::channel();
        let mut blocked = Some(blocked);
        let make: MakeScreen = Box::new(move || {
            let release = blocked.take().expect("built once");
            Ok(Box::new(Blocking { release }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let mut link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.set_timeouts(Duration::from_millis(200), Duration::from_millis(200));
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        assert!(matches!(link.load(chart, None), Err(ScreenError::NoAnswer)));
        let started = Instant::now();
        assert!(link.finish(clock.now()).is_none());
        assert!(started.elapsed() < Duration::from_secs(5));
        assert_eq!(link.exit(), super::RenderEnd::Stuck);
        release.send(()).unwrap();
    }

    /// A chart's load that blocks the render thread bounds every wait on
    /// it: the load and a preparation give up, and the close that follows
    /// returns, saying the thread did not end, so the window stays open.
    #[test]
    fn a_blocked_load_bounds_the_preparation_and_the_close() {
        let (release, blocked) = std::sync::mpsc::channel();
        let mut blocked = Some(blocked);
        let make: MakeScreen = Box::new(move || {
            let release = blocked.take().expect("built once");
            Ok(Box::new(Blocking { release }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let mut link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.set_timeouts(Duration::from_millis(200), Duration::from_millis(200));
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let started = Instant::now();
        assert!(matches!(link.load(chart, None), Err(ScreenError::NoAnswer)));
        assert!(matches!(link.prepare(), Err(ScreenError::NoAnswer)));
        assert_eq!(link.exit(), super::RenderEnd::Stuck);
        assert!(started.elapsed() < Duration::from_secs(5));
        // The thread ends once its screen does.
        release.send(()).unwrap();
    }

    /// Whether `f` holds within 60 s, a bound that only catches a thread
    /// that never answers: the parallel tests can starve the render thread
    /// for seconds. `f` is asked once more past the bound, so a wait that
    /// was itself starved does not fail on time alone.
    pub(crate) fn wait_until(f: impl Fn() -> bool) -> bool {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if f() {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    pub(crate) fn view() -> View {
        View { effects: 16 }
    }

    /// Builds up to `ok` screens, then fails; counts the builds.
    fn factory(
        ok: u32,
        clock: QpcClock,
        presented_at: &Arc<AtomicI64>,
    ) -> (Arc<AtomicU32>, MakeScreen) {
        let builds = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&builds);
        let presented_at = Arc::clone(presented_at);
        let make = Box::new(move || {
            let n = counter.fetch_add(1, Ordering::SeqCst) + 1;
            if n <= ok {
                Ok(Box::new(Flaky {
                    text_frames: 0,
                    play_frames: 0,
                    clock,
                    presented_at: Arc::clone(&presented_at),
                }) as Box<dyn Screen>)
            } else {
                Err(RenderError::Skin(SkinError {
                    reason: format!("build {n} failed"),
                }))
            }
        });
        (builds, make)
    }

    /// A render error passes through transparently: its text and its
    /// source are the renderer's. The thread's own failures keep theirs.
    #[test]
    fn screen_errors_keep_their_sources() {
        use std::error::Error as _;

        let skin = SkinError {
            reason: "no lanes".to_owned(),
        };
        let e = ScreenError::from(RenderError::Skin(skin.clone()));
        assert_eq!(e.to_string(), "skin");
        let source = e.source().expect("the skin error is the source");
        assert_eq!(source.downcast_ref::<SkinError>(), Some(&skin));
        assert!(source.source().is_none());
        assert_eq!(game::Chain(&e).to_string(), "skin: no lanes");

        let e = ScreenError::Spawn(std::io::Error::other("no threads"));
        assert_eq!(e.to_string(), "the render thread cannot start");
        let source = e.source().expect("the spawn error is the source");
        assert!(source.downcast_ref::<std::io::Error>().is_some());
        assert_eq!(
            game::Chain(&e).to_string(),
            "the render thread cannot start: no threads"
        );
        assert!(ScreenError::Ended.source().is_none());
        assert!(ScreenError::EndedWhileStarting.source().is_none());
    }

    #[test]
    fn a_failed_frame_is_reported_and_only_a_preparation_builds_again() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let clock = QpcClock::start();
        let presented = Arc::new(AtomicI64::new(i64::MIN));
        let (builds, make) = factory(2, clock, &presented);
        let link = spawn(
            make,
            Some(chart),
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        assert!(!link.failed());
        link.show_end(super::EndPage {
            lines: vec!["A".to_owned()],
            graphs: None,
        });
        assert!(wait_until(|| link.failed()), "the failure was not reported");
        // More text without a screen builds nothing and stays failed.
        link.show_end(super::EndPage {
            lines: vec!["A".to_owned()],
            graphs: None,
        });
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        assert!(link.failed());
        // The next preparation builds the screen again.
        assert!(link.prepare().is_ok());
        assert!(!link.failed());
        assert_eq!(builds.load(Ordering::SeqCst), 2);
        link.show_end(super::EndPage {
            lines: vec!["B".to_owned()],
            graphs: None,
        });
        assert!(wait_until(|| link.failed()));
        // Building again fails: the preparation reports it.
        let refused = link.prepare();
        assert!(
            matches!(
                &refused,
                Err(ScreenError::Render(RenderError::Skin(SkinError { reason })))
                    if reason == "build 3 failed"
            ),
            "{refused:?}"
        );
        link.exit();
    }

    /// What [`RefusingFrames`] saw, in counters that never allocate: its
    /// preparations, its build calls, the preparation the last build came
    /// in, its draws and its end screen frames.
    #[derive(Default)]
    struct FrameCalls {
        epoch: AtomicU32,
        builds: AtomicU32,
        last_build_epoch: AtomicU32,
        draws: AtomicU32,
        texts: AtomicU32,
    }

    /// A screen whose skin refuses every frame while `refuse` is set,
    /// counting what it is called for. It refuses as told, without the
    /// renderer's own rule of refusing every later frame of a refused
    /// preparation, so only the render thread keeps it from being built
    /// again.
    struct RefusingFrames {
        calls: Arc<FrameCalls>,
        refuse: Arc<AtomicBool>,
    }

    impl Screen for RefusingFrames {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            self.calls.epoch.fetch_add(1, Ordering::AcqRel);
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            let epoch = self.calls.epoch.load(Ordering::Acquire);
            self.calls.last_build_epoch.store(epoch, Ordering::Release);
            self.calls.builds.fetch_add(1, Ordering::AcqRel);
            if self.refuse.load(Ordering::Acquire) {
                return Err(skin_model::BuildError::InconsistentTally);
            }
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            self.calls.draws.fetch_add(1, Ordering::AcqRel);
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            self.calls.texts.fetch_add(1, Ordering::AcqRel);
            Ok(true)
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// A screen noting each frame's chart time with the rearms it was told
    /// of so far.
    struct RearmedFrames {
        /// Each frame's rearms so far, chart time, state clock and
        /// application readouts.
        frames: Noted,
        rearms: u32,
    }

    impl Screen for RearmedFrames {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            self.frames
                .lock()
                .unwrap()
                .push((self.rearms, f.now, f.state_us, f.app));
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn rearm(&mut self) {
            self.rearms += 1;
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The frames [`RearmedFrames`] notes.
    type Noted = Arc<std::sync::Mutex<Vec<(u32, TimeUs, i64, skin_model::AppReadouts)>>>;

    /// A chart with one tap at 60 s.
    fn late_tap() -> Chart {
        ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(60_000_000))
            .build()
            .unwrap()
    }

    /// A render thread drawing [`RearmedFrames`], its attempt over `chart`
    /// with `rule` prepared: the logic that rearms it, the side its play
    /// takes, the frames noted, the clock and the link.
    fn rearmed_play<'a>(
        chart: &'a Chart,
        rule: &'a rules::RulePreset,
        device: &'a DeviceProfile,
    ) -> (
        Logic<'a, SourceReader>,
        game::RenderSide,
        Noted,
        QpcClock,
        super::RenderLink,
    ) {
        let (logic, side) = Logic::<SourceReader>::new(
            chart,
            rule,
            device,
            &InputStart::default(),
            Vec::new(),
            LogicConfig {
                inputs: 0,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        let frames = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&frames);
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(RearmedFrames {
                frames: Arc::clone(&noted),
                rearms: 0,
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let shared_chart = Arc::new(chart.clone());
        let link = spawn(
            make,
            Some(Arc::clone(&shared_chart)),
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let ready = link.prepare_attempt(attempt(&shared_chart, None), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);
        (logic, side, frames, clock, link)
    }

    /// A READY rearm reaches the screen through the render thread: the
    /// logic step's rearm tells the screen once, before its first frame
    /// drawn from the rearmed candidate's origin.
    #[test]
    fn a_rearm_reaches_the_screen_before_its_first_frame() {
        let (chart, rule, device) = (late_tap(), rules::tapestry(), DeviceProfile::default());
        let (mut logic, side, frames, clock, link) = rearmed_play(&chart, &rule, &device);
        link.send(RenderMsg::Play {
            state_origin: TimeUs(0),
            side,
            gauge: 0,
            measure: play_measure(clock.now()),
        });
        assert!(wait_until(|| frames.lock().unwrap().len() >= 2));
        // The next candidate's chart time 0 lies 10 s ahead.
        logic.rearm(&InputStart::default(), TimeUs(clock.now().0 + 10_000_000));
        assert!(wait_until(|| frames
            .lock()
            .unwrap()
            .iter()
            .any(|(_, now, ..)| now.0 < -9_000_000)));
        let _ = link.finish(clock.now());
        link.exit();
        let frames = frames.lock().unwrap();
        for &(rearms, now, ..) in frames.iter() {
            let expected = u32::from(now.0 < -9_000_000);
            assert_eq!(rearms, expected, "a frame at {now:?}");
        }
    }

    /// Each play frame's state clock runs from the play state's origin,
    /// sampled with its chart time (design.md "beatoraja Lua Skins", the
    /// state clock): real time through READY, never reset by a rearm.
    #[test]
    fn the_state_clock_runs_from_the_play_state_through_a_rearm() {
        let (chart, rule, device) = (late_tap(), rules::tapestry(), DeviceProfile::default());
        let (mut logic, side, frames, clock, link) = rearmed_play(&chart, &rule, &device);
        // The state entered 7 s ago; the first candidate's chart time 0
        // lies 2 s ahead, so its first frames are READY's.
        let state_origin = TimeUs(clock.now().0 - 7_000_000);
        let first = TimeUs(clock.now().0 + 2_000_000);
        link.send(RenderMsg::Play {
            side,
            gauge: 0,
            measure: play_measure(first),
            state_origin,
        });
        assert!(wait_until(|| frames.lock().unwrap().len() >= 2));
        // The next candidate's chart time 0 lies 10 s ahead.
        let second = TimeUs(clock.now().0 + 10_000_000);
        logic.rearm(&InputStart::default(), second);
        assert!(wait_until(|| frames
            .lock()
            .unwrap()
            .iter()
            .any(|&(rearms, ..)| rearms == 1)));
        let _ = link.finish(clock.now());
        link.exit();
        let frames = frames.lock().unwrap();
        let mut last = i64::MIN;
        for &(rearms, now, state_us, _) in frames.iter() {
            // One sample: the chart time on its candidate's origin, the
            // state clock on the state's.
            let origin = if rearms == 0 { first } else { second };
            assert_eq!(
                state_us - now.0,
                origin.0 - state_origin.0,
                "{rearms} {now:?}"
            );
            assert!(now.0 < 0 && state_us >= 7_000_000 && state_us >= last);
            last = state_us;
        }
    }

    /// C9: each play frame's uptime is the render thread's clock, the one
    /// `main` starts at launch, at the frame's own sample: never the state
    /// clock nor the chart time, through READY and a rearm. C7: the frames
    /// the screen presented count towards the frames per second.
    #[test]
    fn the_uptime_is_the_launch_clock_s_not_the_state_clock() {
        let (chart, rule, device) = (late_tap(), rules::tapestry(), DeviceProfile::default());
        let (mut logic, side, frames, clock, link) = rearmed_play(&chart, &rule, &device);
        // The state entered 7 s ago; the first candidate's chart time 0
        // lies 2 s ahead.
        let state_origin = TimeUs(clock.now().0 - 7_000_000);
        let first = TimeUs(clock.now().0 + 2_000_000);
        link.send(RenderMsg::Play {
            side,
            gauge: 0,
            measure: play_measure(first),
            state_origin,
        });
        assert!(wait_until(|| frames.lock().unwrap().len() >= 30));
        logic.rearm(&InputStart::default(), TimeUs(clock.now().0 + 10_000_000));
        assert!(wait_until(|| frames
            .lock()
            .unwrap()
            .iter()
            .any(|&(rearms, ..)| rearms == 1)));
        let _ = link.finish(clock.now());
        link.exit();
        let end = clock.now();
        let frames = frames.lock().unwrap();
        let mut last = 0;
        for &(rearms, now, state_us, app) in frames.iter() {
            // One sample: the state clock is the uptime less the state's
            // origin on the same clock, 7 s apart from it.
            assert_eq!(app.uptime_us, state_us + state_origin.0, "{rearms} {now:?}");
            assert_ne!(app.uptime_us, state_us);
            assert_ne!(app.uptime_us, now.0);
            assert!(app.uptime_us >= last && app.uptime_us <= end.0);
            last = app.uptime_us;
            assert!(app.fps <= 1_024 && (1..=12).contains(&app.local.month));
        }
        // The frames the screen presented are counted: unpaced, about one
        // each 16 ms.
        assert!(frames.iter().any(|f| f.3.fps >= 10), "{:?}", frames.last());
    }

    /// A READY capture belongs to its attempt's first candidate: a real
    /// rearm into the capture's window fails it, and no frame of the next
    /// candidate satisfies it.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_ready_capture_fails_when_its_candidate_is_rearmed() {
        use crate::dev::scenario::{Axis, CaptureState, Captures, ScenarioFrames};

        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(60_000_000))
            .build()
            .unwrap();
        let (rule, device) = (rules::tapestry(), DeviceProfile::default());
        let (mut logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            Vec::new(),
            LogicConfig {
                inputs: 0,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let taken = dir.path().join("r.png");
        let captures = Captures::of(vec![(TimeUs(0), taken.clone())]);
        captures.set_axis(0, Axis::Ready(TimeUs(500_000)));
        captures.arm(0);
        captures.bind(1);
        let frames = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&frames);
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(RearmedFrames {
                frames: Arc::clone(&noted),
                rearms: 0,
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let shared_chart = Arc::new(chart.clone());
        let link = spawn(
            make,
            Some(Arc::clone(&shared_chart)),
            None,
            clock,
            view(),
            measure(clock),
            Box::new(ScenarioFrames::new(captures.clone())),
        )
        .unwrap();
        let ready = link.prepare_attempt(attempt(&shared_chart, None), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);
        // Long before READY's window, so the first candidate's frames do
        // not satisfy it.
        link.send(RenderMsg::Play {
            state_origin: TimeUs(0),
            side,
            gauge: 0,
            measure: play_measure(TimeUs(clock.now().0 + 60_000_000)),
        });
        assert!(wait_until(|| frames.lock().unwrap().len() >= 2));
        assert_eq!(captures.states()[0], CaptureState::Bound(1));
        // The next candidate's chart time 0 lies inside the capture's
        // window: the bug takes its frame.
        logic.rearm(&InputStart::default(), TimeUs(clock.now().0 + 300_000));
        assert!(wait_until(|| !matches!(
            captures.states()[0],
            CaptureState::Bound(1) | CaptureState::Saving(1)
        )));
        let _ = link.finish(clock.now());
        link.exit();
        match &captures.states()[0] {
            CaptureState::Failed(why) => assert!(why.contains("rearmed"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert!(!taken.exists());
    }

    /// A refused frame stops the attempt without dropping the screen: it
    /// is reported as the screen's failure, so the game ends the play, and
    /// no frame of that preparation is built or drawn again, however many
    /// frame opportunities follow. The recovery keeps the screen, built
    /// once, and only the next attempt's preparation builds again. The
    /// app region ends on the refusal's way out: the end screen drawn after
    /// it, which allocates, counts nothing in it (dev-tools).
    #[test]
    fn a_refused_frame_stops_the_attempt_and_keeps_the_screen() {
        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(60_000_000))
            .build()
            .unwrap();
        let (rule, device) = (rules::tapestry(), DeviceProfile::default());
        let play = || {
            Logic::<SourceReader>::new(
                &chart,
                &rule,
                &device,
                &InputStart::default(),
                Vec::new(),
                LogicConfig {
                    inputs: 0,
                    effects: 16,
                },
                Vec::new(),
            )
            .unwrap()
        };
        let calls = Arc::new(FrameCalls::default());
        let refuse = Arc::new(AtomicBool::new(false));
        let made = Arc::new(AtomicU32::new(0));
        let (seen, refusing, count) = (Arc::clone(&calls), Arc::clone(&refuse), Arc::clone(&made));
        let make: MakeScreen = Box::new(move || {
            count.fetch_add(1, Ordering::AcqRel);
            Ok(Box::new(RefusingFrames {
                calls: Arc::clone(&seen),
                refuse: Arc::clone(&refusing),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let shared_chart = Arc::new(chart.clone());
        let link = spawn(
            make,
            Some(Arc::clone(&shared_chart)),
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let ready = link.prepare_attempt(attempt(&shared_chart, None), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);
        let first = calls.epoch.load(Ordering::Acquire);
        let (_logic, side) = play();
        link.send(RenderMsg::Play {
            state_origin: TimeUs(0),
            side,
            gauge: 0,
            measure: play_measure(clock.now()),
        });
        assert!(wait_until(|| calls.draws.load(Ordering::Acquire) >= 3));
        assert!(!link.failed());

        refuse.store(true, Ordering::Release);
        assert!(wait_until(|| link.failed()), "the refusal was not reported");
        let (builds, draws) = (
            calls.builds.load(Ordering::Acquire),
            calls.draws.load(Ordering::Acquire),
        );
        assert_eq!(calls.last_build_epoch.load(Ordering::Acquire), first);
        // Frame opportunities: each wake goes round the render loop.
        for _ in 0..5 {
            link.send(RenderMsg::Resize);
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(calls.builds.load(Ordering::Acquire), builds, "built again");
        assert_eq!(calls.draws.load(Ordering::Acquire), draws, "drawn again");
        link.show_end(super::EndPage {
            lines: vec!["END".to_owned()],
            graphs: None,
        });
        assert!(wait_until(|| calls.texts.load(Ordering::Acquire) >= 1));
        let fig = link.finish(clock.now()).expect("the play's figures");
        #[cfg(feature = "dev-tools")]
        assert_eq!(fig.render_app_allocations, Some(0));
        #[cfg(not(feature = "dev-tools"))]
        assert_eq!(fig.render_app_allocations, None);
        assert_eq!(calls.builds.load(Ordering::Acquire), builds);

        // The recovery keeps the screen; the next attempt's preparation
        // builds again, in its own preparation.
        link.prepare().unwrap();
        assert!(!link.failed());
        refuse.store(false, Ordering::Release);
        let ready = link.prepare_attempt(attempt(&shared_chart, None), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);
        let next = calls.epoch.load(Ordering::Acquire);
        assert!(next > first);
        let (_next_logic, side) = play();
        link.send(RenderMsg::Play {
            state_origin: TimeUs(0),
            side,
            gauge: 0,
            measure: play_measure(clock.now()),
        });
        assert!(wait_until(
            || calls.draws.load(Ordering::Acquire) >= draws + 3
        ));
        assert!(calls.builds.load(Ordering::Acquire) > builds);
        assert_eq!(calls.last_build_epoch.load(Ordering::Acquire), next);
        assert!(!link.failed());
        assert_eq!(
            made.load(Ordering::Acquire),
            1,
            "the screen was built again"
        );
        let _ = link.finish(clock.now());
        link.exit();
    }

    #[test]
    fn the_first_frame_is_the_first_presented_one() {
        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap();
        let rule = rules::tapestry();
        let device = DeviceProfile::default();
        let (_logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            Vec::new(),
            LogicConfig {
                inputs: 0,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        let clock = QpcClock::start();
        let presented = Arc::new(AtomicI64::new(i64::MIN));
        let (_builds, make) = factory(1, clock, &presented);
        let link = spawn(
            make,
            Some(Arc::new(chart.clone())),
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let origin = clock.now();
        link.send(RenderMsg::Play {
            state_origin: TimeUs(0),
            side,
            gauge: 0,
            measure: play_measure(origin),
        });
        assert!(wait_until(|| link.first_frame().is_some()));
        let first = link.first_frame().unwrap().0;
        let at = presented.load(Ordering::Acquire);
        assert_ne!(at, i64::MIN);
        assert!(first >= at, "first frame at {first}, presented at {at}");
        link.exit();
    }

    /// A screen that keeps each play frame's visible time and display
    /// offset.
    /// One frame's gain and display offset.
    type FrameSeen = (Option<skin_model::Gain>, TimeUs, Option<u32>);

    struct Inputs(Arc<std::sync::Mutex<Vec<FrameSeen>>>);

    impl Screen for Inputs {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            self.0.lock().unwrap().push((
                f.lanes.gain,
                f.display_offset,
                f.readouts.native.target_green,
            ));
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The render thread reads one presentation snapshot at every frame:
    /// the gain of the committed attempt's epoch and the display offset
    /// change together with each published snapshot, a snapshot of
    /// another epoch gives no gain, and no later frame goes back.
    #[test]
    fn render_frames_follow_the_presentation_snapshot() {
        use game::adjustment::{PresentationSnapshot, presentation_link};

        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap();
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let screen_seen = Arc::clone(&seen);
        let make: MakeScreen =
            Box::new(move || Ok(Box::new(Inputs(Arc::clone(&screen_seen))) as Box<dyn Screen>));
        let clock = QpcClock::start();
        let view = View { effects: 16 };
        let link = spawn(
            make,
            Some(Arc::new(chart.clone())),
            None,
            clock,
            view,
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let (rule, device) = (rules::tapestry(), DeviceProfile::default());
        let snapshot = |h: Option<(u64, u64)>, offset, epoch| PresentationSnapshot {
            hispeed: h.and_then(|(n, d)| game::Ratio::new(n, d)),
            display_offset_us: offset,
            epoch,
            scratch: game::ScratchInput::NONE,
            ..PresentationSnapshot {
                options: game::PlayOptions::default(),
                hispeed: None,
                target: None,
                display_offset_us: 0,
                adjusting: false,
                generation: 0,
                sequence: 0,
                epoch: 0,
                scratch: game::ScratchInput::NONE,
            }
        };
        let (mut writer, reader) = presentation_link(&snapshot(Some((4, 1)), 0, 1));
        link.presentation(reader);
        // The attempt prepared and committed as epoch 1.
        let ready = link.prepare_attempt(attempt(&chart, None), || false);
        assert!(matches!(ready, Ok(Preparation::Ready)), "{ready:?}");
        link.commit_attempt().unwrap();
        let (_logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            Vec::new(),
            LogicConfig {
                inputs: 0,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        let origin = clock.now();
        link.send(RenderMsg::Play {
            state_origin: TimeUs(0),
            side,
            gauge: 0,
            measure: play_measure(origin),
        });
        let gain = |n, d| skin_model::Gain::new(n, d);
        // The lanes' gain, the display offset and the native target: one
        // snapshot each frame, whatever skin draws it.
        let start = (gain(4, 1), TimeUs(0), Some(300));
        let faster = (gain(9, 2), TimeUs(0), Some(300));
        let later = (gain(9, 2), TimeUs(1_000), Some(300));
        let other = (None, TimeUs(1_000), Some(300));
        assert!(wait_until(|| seen.lock().unwrap().contains(&start)));
        writer.write(snapshot(Some((9, 2)), 0, 1));
        assert!(wait_until(|| seen.lock().unwrap().contains(&faster)));
        writer.write(snapshot(Some((9, 2)), 1_000, 1));
        assert!(wait_until(|| seen.lock().unwrap().contains(&later)));
        // A snapshot of another attempt's epoch: no gain, its offset kept.
        writer.write(snapshot(Some((9, 2)), 1_000, 2));
        assert!(wait_until(|| seen.lock().unwrap().contains(&other)));
        link.exit();
        // The frames go through the states in order, each once.
        let mut frames = seen.lock().unwrap().clone();
        frames.dedup();
        assert_eq!(frames, [start, faster, later, other]);
    }

    /// A screen whose every play frame fails, as a lost surface does.
    struct Lost;

    impl Screen for Lost {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Err(RenderError::SurfaceLost)
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// A judgment drawn on a play frame that fails never reaches the screen
    /// through it: the render thread counts it N/A in the play's figures.
    #[test]
    fn a_failed_play_frame_makes_its_judgments_na() {
        use eval::{ControlId, InputKind, PhysicalInputEvent, SourceId};
        use game::DeviceSettings;
        use input::DeviceKind;

        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap();
        let rule = rules::tapestry();
        let device = DeviceSettings::default().profile(&[DeviceKind::Keyboard], &[]);
        let (mut writer, reader) = input::source_queue(16);
        // Z, lane 1's default key, pressed on the note.
        let press = PhysicalInputEvent {
            received: TimeUs(1_000_000),
            source: SourceId(0),
            kind: InputKind::Control {
                control: ControlId(0x2c),
                value: 1,
            },
        };
        writer.push(press).unwrap();
        writer.publish(TimeUs(1_000_001));
        let (mut logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            vec![reader],
            LogicConfig {
                inputs: 16,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        logic.advance_to(TimeUs(1_000_002));
        assert!(
            logic
                .events()
                .iter()
                .any(|e| matches!(e.kind, eval::JudgeEventKind::Judged { .. }))
        );
        let clock = QpcClock::start();
        let make: MakeScreen = Box::new(|| Ok(Box::new(Lost) as Box<dyn Screen>));
        let link = spawn(
            make,
            Some(Arc::new(chart.clone())),
            None,
            clock,
            view(),
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        let origin = clock.now();
        link.send(RenderMsg::Play {
            state_origin: TimeUs(0),
            side,
            gauge: 0,
            measure: play_measure(origin),
        });
        assert!(wait_until(|| link.failed()));
        let fig = link.finish(clock.now()).expect("the play's figures");
        assert_eq!(fig.press_to_screen_unavailable, 1);
        assert_eq!(fig.press_to_screen, None);
        link.exit();
    }

    /// Play frames the [`Stalling`] screen draws before it stalls.
    const DRAWN_FRAMES: u32 = 2;

    /// A paced screen that lets [`DRAWN_FRAMES`] play frames through and
    /// then times out every wait; it keeps each frame's effect count and
    /// how many of them were judgments.
    struct Stalling {
        waits: Arc<AtomicU32>,
        seen: Arc<std::sync::Mutex<Vec<(usize, usize)>>>,
    }

    impl Screen for Stalling {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            if self.waits.fetch_add(1, Ordering::AcqRel) < DRAWN_FRAMES {
                return true;
            }
            std::thread::sleep(Duration::from_millis(1));
            false
        }
        fn paced(&self) -> bool {
            true
        }
        fn build(&mut self, f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            let judged = f
                .effects
                .iter()
                .filter(|e| matches!(e.kind, eval::JudgeEventKind::Judged { .. }))
                .count();
            self.seen.lock().unwrap().push((f.effects.len(), judged));
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// More effects are queued than a frame's buffer holds: each frame
    /// draws at most a buffer's worth, and the rest wait in the queue.
    /// Judgments still queued when the play ends never reached the screen
    /// and are N/A in its figures, as are the drawn ones offscreen (no
    /// present count to pair them with).
    #[test]
    fn frames_draw_a_buffer_of_effects_and_the_rest_end_na() {
        use game::DeviceSettings;
        use input::DeviceKind;

        // The frame buffer the render thread makes for a view of 2 effects.
        let buffer = Vec::<eval::JudgeEvent>::with_capacity(2).capacity();
        let mut builder = ChartBuilder::new(120_000);
        for k in 0..6 {
            builder = builder.tap(Lane::new(1).unwrap(), TimeUs(1_000_000 + k * 200_000));
        }
        let chart = builder.build().unwrap();
        let rule = rules::tapestry();
        let device = DeviceSettings::default().profile(&[DeviceKind::Keyboard], &[]);
        let (mut writer, reader) = input::source_queue(32);
        // Every note hit on time.
        for e in input::autoplay(&chart, &device.sources[0]).unwrap() {
            writer.push(e).unwrap();
        }
        writer.publish(TimeUs(3_000_000));
        let (mut logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            vec![reader],
            LogicConfig {
                inputs: 32,
                effects: 64,
            },
            Vec::new(),
        )
        .unwrap();
        // Every effect is queued before the render thread draws a frame.
        logic.advance_to(TimeUs(3_000_001));
        let judged = logic
            .events()
            .iter()
            .filter(|e| matches!(e.kind, eval::JudgeEventKind::Judged { .. }))
            .count();
        assert_eq!(judged, 6);
        // More than the drawn frames' buffers hold.
        assert!(logic.events().len() > DRAWN_FRAMES as usize * buffer);
        let waits = Arc::new(AtomicU32::new(0));
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let screen = (Arc::clone(&waits), Arc::clone(&seen));
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Stalling {
                waits: Arc::clone(&screen.0),
                seen: Arc::clone(&screen.1),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let view = View { effects: buffer };
        let link = spawn(
            make,
            Some(Arc::new(chart.clone())),
            None,
            clock,
            view,
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.send(RenderMsg::Play {
            state_origin: TimeUs(0),
            side,
            gauge: 0,
            measure: play_measure(clock.now()),
        });
        // Past its last frame: the screen has stalled.
        assert!(wait_until(|| waits.load(Ordering::Acquire) > DRAWN_FRAMES));
        let fig = link.finish(clock.now()).expect("the play's figures");
        link.exit();
        let frames = seen.lock().unwrap().clone();
        assert_eq!(frames.len(), DRAWN_FRAMES as usize);
        // Each frame drew a full buffer, never more.
        assert!(frames.iter().all(|&(n, _)| n == buffer), "{frames:?}");
        let drawn: usize = frames.iter().map(|&(_, j)| j).sum();
        assert!(drawn < judged, "{frames:?}");
        // The drawn judgments and the ones still queued at the end.
        assert_eq!(fig.press_to_screen_unavailable, judged as u64);
        assert_eq!(fig.press_to_screen, None);
    }

    /// Something that allocates, kept by `black_box` so no profile elides it.
    #[cfg(feature = "dev-tools")]
    fn allocate() {
        drop(std::hint::black_box(vec![0u8; 8]));
    }

    /// A screen that allocates in every draw and read-back, and in every
    /// build when `in_build`, as wgpu, the readback and a faulty skin would;
    /// it counts its draws and read-backs.
    #[cfg(feature = "dev-tools")]
    struct Allocating {
        in_build: bool,
        draws: Arc<AtomicU32>,
        read_backs: Arc<AtomicU32>,
    }

    #[cfg(feature = "dev-tools")]
    impl Screen for Allocating {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            if self.in_build {
                allocate();
            }
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            allocate();
            self.draws.fetch_add(1, Ordering::AcqRel);
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            self.read_backs.fetch_add(1, Ordering::AcqRel);
            Some((1, 1, vec![0; 4]))
        }
    }

    /// A frame sink that reads every frame back and allocates, as the frame
    /// dump does; it counts its frames.
    #[cfg(feature = "dev-tools")]
    struct Dumping(Arc<AtomicU32>);

    #[cfg(feature = "dev-tools")]
    impl super::FrameSink for Dumping {
        fn frame(&mut self, screen: &dyn Screen, _drawn: &crate::measure::Drawn<'_>) {
            drop(std::hint::black_box(screen.read_back()));
            allocate();
            self.0.fetch_add(1, Ordering::AcqRel);
        }
    }

    /// A play drawn on [`Allocating`] into [`Dumping`], whose build
    /// allocates when `in_build` and whose measurement when `in_measure`:
    /// its app region count, and how many draws, read-backs and sunk frames
    /// ran.
    #[cfg(feature = "dev-tools")]
    fn app_region_count(in_build: bool, in_measure: bool) -> (Option<u64>, [u32; 3]) {
        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap();
        let (rule, device) = (rules::tapestry(), DeviceProfile::default());
        let (_logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            Vec::new(),
            LogicConfig {
                inputs: 0,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        let counters = [(); 3].map(|()| Arc::new(AtomicU32::new(0)));
        let [draws, read_backs, sunk] = counters.clone();
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Allocating {
                in_build,
                draws: Arc::clone(&draws),
                read_backs: Arc::clone(&read_backs),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let mut m = measure(clock);
        if in_measure {
            m.allocate_in_frames();
        }
        let link = spawn(
            make,
            Some(Arc::new(chart.clone())),
            None,
            clock,
            view(),
            m,
            Box::new(Dumping(sunk)),
        )
        .unwrap();
        link.send(RenderMsg::Play {
            state_origin: TimeUs(0),
            side,
            gauge: 0,
            measure: play_measure(clock.now()),
        });
        let [_, _, sunk] = &counters;
        assert!(wait_until(|| sunk.load(Ordering::Acquire) >= 5));
        let fig = link.finish(clock.now()).expect("the play's figures");
        link.exit();
        (
            fig.render_app_allocations,
            counters.map(|c| c.load(Ordering::Acquire)),
        )
    }

    /// What the screen draws, reads back and the frame sink does allocates
    /// outside the app region: the region counts none of it.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_app_region_leaves_out_drawing_and_the_frame_sink() {
        let (count, [draws, read_backs, sunk]) = app_region_count(false, false);
        assert_eq!(count, Some(0));
        assert!(draws >= 5 && read_backs >= 5 && sunk >= 5);
    }

    /// An allocation in the frame's build, the region's first interval, is
    /// counted.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_app_region_counts_the_frame_build() {
        let (count, _) = app_region_count(true, false);
        assert!(count.is_some_and(|n| n > 0), "{count:?}");
    }

    /// An allocation in a play frame's measurement, the region's second
    /// interval, is counted; the build allocates nothing here.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn the_app_region_counts_the_frame_measurement() {
        let (count, _) = app_region_count(false, true);
        assert!(count.is_some_and(|n| n > 0), "{count:?}");
    }

    /// A play's end is a barrier for a scenario's captures: the frame drawn
    /// before the end, which reaches the capture's time, has been written
    /// when the end is answered; no frame after it counts in the play, so
    /// a capture bound to it then is never taken.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_plays_end_settles_its_captures_after_its_last_frame() {
        use crate::dev::scenario::{CaptureState, Captures, ScenarioFrames};
        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap();
        let (rule, device) = (rules::tapestry(), DeviceProfile::default());
        let (_logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            Vec::new(),
            LogicConfig {
                inputs: 0,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let (taken, late) = (dir.path().join("taken.png"), dir.path().join("late.png"));
        let captures = Captures::of(vec![
            (TimeUs(2_900_000), taken.clone()),
            (TimeUs(0), late.clone()),
        ]);
        let draws = Arc::new(AtomicU32::new(0));
        let drawn = Arc::clone(&draws);
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Allocating {
                in_build: false,
                draws: Arc::clone(&drawn),
                read_backs: Arc::new(AtomicU32::new(0)),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            Some(Arc::new(chart.clone())),
            None,
            clock,
            view(),
            measure(clock),
            Box::new(ScenarioFrames::new(captures.clone())),
        )
        .unwrap();
        captures.arm(0);
        captures.bind(1);
        // Chart time 3 s at the play's first frame: past the capture's.
        link.send(RenderMsg::Play {
            state_origin: TimeUs(0),
            side,
            gauge: 0,
            measure: play_measure(TimeUs(clock.now().0 - 3_000_000)),
        });
        assert!(link.finish(clock.now()).is_some());
        assert_eq!(captures.states()[0], CaptureState::Succeeded);
        assert!(std::fs::read(&taken).unwrap().starts_with(b"\x89PNG"));
        // Bound once the play's figures are taken: the frames still drawn
        // count in no play.
        captures.arm(1);
        captures.bind(1);
        // A frame drawn after the next has started was sunk whole.
        let after = draws.load(Ordering::Acquire);
        assert!(wait_until(|| draws.load(Ordering::Acquire) >= after + 2));
        assert_eq!(captures.states()[1], CaptureState::Bound(1));
        assert!(!late.exists());
        link.exit();
    }

    /// What an end frame drew: its first line and its graphs.
    type Drew = (String, Option<Arc<game::ResultGraphs>>);

    /// A screen that notes every end page it draws; while `hold` is set,
    /// an end frame waits for `release` once.
    struct Ends {
        drew: Arc<std::sync::Mutex<Vec<Drew>>>,
        hold: Arc<std::sync::atomic::AtomicBool>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl Screen for Ends {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            panic!("an end page is drawn with its graphs")
        }
        fn end(&mut self, page: &super::EndPage) -> Result<bool, RenderError> {
            let first = page.lines.first().cloned().unwrap_or_default();
            self.drew.lock().unwrap().push((first, page.graphs.clone()));
            if self.hold.swap(false, Ordering::SeqCst) {
                let _ = self.release.recv_timeout(Duration::from_secs(10));
            }
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            self.drew.lock().unwrap().push(("<menu>".to_owned(), None));
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The render thread over an [`Ends`] screen: the link, what it drew,
    /// the hold flag and the release.
    type Ended = (
        super::RenderLink,
        Arc<std::sync::Mutex<Vec<Drew>>>,
        Arc<std::sync::atomic::AtomicBool>,
        std::sync::mpsc::Sender<()>,
    );

    /// [`Ended`], started.
    fn ends() -> Ended {
        let drew = Arc::new(std::sync::Mutex::new(Vec::new()));
        let hold = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (release, released) = std::sync::mpsc::channel();
        let mut released = Some(released);
        let (noted, held) = (Arc::clone(&drew), Arc::clone(&hold));
        let make: MakeScreen = Box::new(move || {
            let release = released.take().expect("built once");
            Ok(Box::new(Ends {
                drew: Arc::clone(&noted),
                hold: Arc::clone(&held),
                release,
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
        (link, drew, hold, release)
    }

    fn page(line: &str, graphs: Option<&Arc<game::ResultGraphs>>) -> super::EndPage {
        super::EndPage {
            lines: vec![line.to_owned()],
            graphs: graphs.cloned(),
        }
    }

    fn graphs(reached: u16) -> Arc<game::ResultGraphs> {
        Arc::new(game::ResultGraphs {
            reached,
            ..game::ResultGraphs::EMPTY
        })
    }

    /// The page's graphs reach the screen with its lines; a page with new
    /// lines and the same graphs (a settings change) draws them again; a
    /// page without (a refused view) draws none.
    #[test]
    fn end_pages_reach_the_screen_with_their_graphs() {
        let (link, drew, _, _) = ends();
        let g = graphs(7);
        let last = |line: &str| {
            let drew = Arc::clone(&drew);
            let line = line.to_owned();
            move || drew.lock().unwrap().last().is_some_and(|d| d.0 == line)
        };
        link.show_end(page("A", Some(&g)));
        assert!(wait_until(last("A")));
        link.show_end(page("B", Some(&g)));
        assert!(wait_until(last("B")));
        link.show_end(page("C", None));
        assert!(wait_until(last("C")));
        let drew = drew.lock().unwrap().clone();
        let by = |line: &str| drew.iter().find(|d| d.0 == line).unwrap().1.clone();
        assert!(Arc::ptr_eq(&by("A").unwrap(), &g));
        assert!(Arc::ptr_eq(&by("B").unwrap(), &g));
        assert!(by("C").is_none());
        link.exit();
    }

    /// While the render thread is stuck in a frame, pages sent meanwhile
    /// replace each other in the slot: each replaced one is dropped at
    /// once, so one waits whatever the stall; once the frame ends, the
    /// newest is drawn, and the one drawn before is dropped.
    #[test]
    fn end_pages_coalesce_while_the_render_thread_stalls() {
        let (link, drew, hold, release) = ends();
        let first = graphs(1);
        hold.store(true, Ordering::SeqCst);
        link.show_end(page("first", Some(&first)));
        // Held inside the first page's frame.
        assert!(wait_until(|| !hold.load(Ordering::SeqCst)));
        let sent: Vec<_> = (2..=6).map(graphs).collect();
        for (i, g) in sent.iter().enumerate() {
            link.show_end(page(&format!("page {i}"), Some(g)));
        }
        // The render thread holds the first page (and the screen's note
        // of it); the slot the newest.
        assert_eq!(Arc::strong_count(&first), 3);
        for g in &sent[..4] {
            assert_eq!(Arc::strong_count(g), 1);
        }
        assert_eq!(Arc::strong_count(&sent[4]), 2);
        release.send(()).unwrap();
        assert!(wait_until(|| {
            drew.lock().unwrap().last().is_some_and(|d| d.0 == "page 4")
        }));
        let drew_lines: Vec<String> = drew.lock().unwrap().iter().map(|d| d.0.clone()).collect();
        assert!(
            drew_lines.iter().all(|l| l == "first" || l == "page 4"),
            "{drew_lines:?}"
        );
        // The page drawn before went with the newer one taken: only the
        // screen's note of it is left.
        assert_eq!(Arc::strong_count(&first), 2);
        link.exit();
    }

    /// Left for a retry (a preparation follows) or for a menu screen
    /// (selection, the practice page): once the rig clears the slot, a
    /// page still waiting in it is never drawn, nor the page drawn before,
    /// whether the render thread was stuck in that page's frame or not.
    #[test]
    fn a_left_end_screen_draws_no_stale_page() {
        for (stalled, retry) in [(true, true), (true, false), (false, true), (false, false)] {
            let (link, drew, hold, release) = ends();
            let old = graphs(1);
            hold.store(stalled, Ordering::SeqCst);
            link.show_end(page("old", Some(&old)));
            assert!(wait_until(|| !drew.lock().unwrap().is_empty()));
            let waiting = graphs(2);
            if stalled {
                // A settings change while the frame is stuck: its page waits.
                link.show_end(page("waiting", Some(&waiting)));
                assert_eq!(Arc::strong_count(&waiting), 2);
            }
            // What the rig does on leaving: the slot is emptied first.
            link.clear_end();
            assert_eq!(Arc::strong_count(&waiting), 1);
            if retry {
                if stalled {
                    release.send(()).unwrap();
                }
                assert!(link.prepare().is_ok());
            } else {
                link.send(RenderMsg::Menu(Box::default()));
                if stalled {
                    release.send(()).unwrap();
                }
                assert!(wait_until(|| {
                    drew.lock().unwrap().iter().any(|d| d.0 == "<menu>")
                }));
            }
            let ends = |drew: &[Drew]| drew.iter().filter(|d| d.0 != "<menu>").count();
            let before = ends(&drew.lock().unwrap());
            std::thread::sleep(Duration::from_millis(100));
            let lines: Vec<String> = drew.lock().unwrap().iter().map(|d| d.0.clone()).collect();
            assert!(!lines.iter().any(|l| l == "waiting"), "{lines:?}");
            // No end frame once the screen was left.
            assert_eq!(ends(&drew.lock().unwrap()), before, "{lines:?}");
            if stalled {
                // Only the stuck frame of the old page.
                assert_eq!(before, 1, "{lines:?}");
            }
            if let Some(m) = lines.iter().position(|l| l == "<menu>") {
                assert!(lines[m..].iter().all(|l| l == "<menu>"), "{lines:?}");
            }
            // The render thread let go of the old page too: only the
            // screen's notes of it are left.
            let notes = drew
                .lock()
                .unwrap()
                .iter()
                .filter(|d| d.1.as_ref().is_some_and(|g| Arc::ptr_eq(g, &old)))
                .count();
            if stalled {
                assert_eq!(Arc::strong_count(&old), 1 + notes, "{retry}");
            }
            link.exit();
        }
    }
    /// A scripted run's end screen is over after its time once its page was
    /// drawn: never while the render thread is stuck before drawing it,
    /// short of the wait's bound, and at once when the screen failed.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_scripted_end_waits_for_its_page() {
        use crate::dev::STEP_TIMEOUT;
        use crate::play::scripted_end_over;
        let after = Duration::from_millis(300);
        let (link, _, hold, release) = ends();
        link.show_end(page("first", None));
        assert!(link.wait_end_drawn(Duration::from_secs(10)));
        assert!(!scripted_end_over(after, Duration::from_millis(299), &link));
        assert!(scripted_end_over(after, after, &link));
        // Stuck in the next page's frame: the page after it is not drawn.
        hold.store(true, Ordering::SeqCst);
        link.show_end(page("stuck", None));
        assert!(wait_until(|| !hold.load(Ordering::SeqCst)));
        link.show_end(page("waiting", None));
        assert!(!scripted_end_over(after, after, &link));
        assert!(!scripted_end_over(after, STEP_TIMEOUT - after, &link));
        assert!(scripted_end_over(after, STEP_TIMEOUT, &link));
        release.send(()).unwrap();
        assert!(wait_until(|| scripted_end_over(after, after, &link)));
        link.exit();
    }

    /// A script's `drawn` step: the wait ends once the page shown last has
    /// been presented, and gives up at its bound while the render thread
    /// is stuck before drawing it.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn waiting_for_the_end_page_ends_when_drawn_or_at_its_bound() {
        let (link, drew, hold, release) = ends();
        link.show_end(page("first", None));
        assert!(link.wait_end_drawn(Duration::from_secs(10)));
        assert!(!drew.lock().unwrap().is_empty());
        // Stuck in the next page's frame: the page after it is not drawn.
        hold.store(true, Ordering::SeqCst);
        link.show_end(page("stuck", None));
        assert!(wait_until(|| !hold.load(Ordering::SeqCst)));
        link.show_end(page("waiting", None));
        let began = Instant::now();
        assert!(!link.wait_end_drawn(Duration::from_millis(200)));
        assert!(began.elapsed() >= Duration::from_millis(200));
        release.send(()).unwrap();
        assert!(link.wait_end_drawn(Duration::from_secs(10)));
        assert_eq!(drew.lock().unwrap().last().unwrap().0, "waiting");
        link.exit();
    }

    /// Two pages coalesced: the first signal took and drew the newer page,
    /// and the second signal is still pending when the end screen is left.
    /// Neither the pending signal nor further frames draw that page again;
    /// left with no message pending, the page drawn last stops too.
    #[test]
    fn a_cleared_page_is_not_drawn_again() {
        let (link, drew, hold, release) = ends();
        let count = |line: &str| drew.lock().unwrap().iter().filter(|d| d.0 == line).count();
        hold.store(true, Ordering::SeqCst);
        link.show_end(page("page 1", None));
        assert!(wait_until(|| !hold.load(Ordering::SeqCst)));
        link.show_end(page("page 2", None));
        link.show_end(page("page 3", None));
        // The next end frame, page 3's for the signal of page 2, holds too.
        hold.store(true, Ordering::SeqCst);
        release.send(()).unwrap();
        assert!(wait_until(|| !hold.load(Ordering::SeqCst)));
        assert_eq!(count("page 3"), 1);
        // Page 3's own signal is pending.
        link.clear_end();
        release.send(()).unwrap();
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(count("page 3"), 1, "{:?}", drew.lock().unwrap());
        assert_eq!(count("page 2"), 0);

        // Nothing pending: the page drawn last keeps being drawn until the
        // end screen is left, then stops.
        let (link, drew, _, _) = ends();
        let count = |line: &str| drew.lock().unwrap().iter().filter(|d| d.0 == line).count();
        link.show_end(page("shown", None));
        assert!(wait_until(|| count("shown") >= 3));
        link.clear_end();
        std::thread::sleep(Duration::from_millis(50));
        let after = count("shown");
        std::thread::sleep(Duration::from_millis(150));
        assert_eq!(count("shown"), after);
        link.exit();
    }

    /// A retry in the production order: the rig leaves the end screen and
    /// then prepares the screen ([`crate::play::prepare_render`]), while
    /// the render thread is stuck in an end frame with a newer page
    /// waiting; the preparation's reply comes without that page drawn.
    #[test]
    fn a_retry_leaves_the_end_screen_before_preparing() {
        let (link, drew, hold, release) = ends();
        hold.store(true, Ordering::SeqCst);
        link.show_end(page("shown", None));
        assert!(wait_until(|| !hold.load(Ordering::SeqCst)));
        link.show_end(page("waiting", None));
        let mut end_page = Some(super::EndPage::default());
        let resume = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            release.send(()).unwrap();
        });
        assert!(crate::play::prepare_render(&mut end_page, &link).is_ok());
        resume.join().unwrap();
        assert_eq!(end_page, None);
        std::thread::sleep(Duration::from_millis(100));
        let lines: Vec<String> = drew.lock().unwrap().iter().map(|d| d.0.clone()).collect();
        assert_eq!(lines, ["shown"]);
        link.exit();
    }

    /// A screen paced as a swap chain's waitable object with one frame of
    /// latency: a wait takes the one grant, a frame drawn gives it back,
    /// and with none a wait times out (after 5 ms). While `hold` is set,
    /// the wait that takes the grant waits for `release` once.
    struct Paced {
        drew: Arc<std::sync::Mutex<Vec<String>>>,
        grants: AtomicU32,
        hold: Arc<std::sync::atomic::AtomicBool>,
        release: std::sync::mpsc::Receiver<()>,
    }

    impl Paced {
        fn drawn(&self, what: String) {
            self.drew.lock().unwrap().push(what);
            self.grants.store(1, Ordering::SeqCst);
        }
    }

    impl Screen for Paced {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            let took = self
                .grants
                .try_update(Ordering::SeqCst, Ordering::SeqCst, |g| g.checked_sub(1));
            if took.is_err() {
                std::thread::sleep(Duration::from_millis(5));
                return false;
            }
            if self.hold.swap(false, Ordering::SeqCst) {
                let _ = self.release.recv_timeout(Duration::from_secs(10));
            }
            true
        }
        fn paced(&self) -> bool {
            true
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            self.drawn("<play>".to_owned());
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            panic!("an end page is drawn with its graphs")
        }
        fn end(&mut self, page: &super::EndPage) -> Result<bool, RenderError> {
            self.drawn(page.lines.first().cloned().unwrap_or_default());
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            self.drawn("<menu>".to_owned());
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The render thread over a [`Paced`] screen, the end page `result`
    /// drawn and the thread held in the wait for its next frame: the link,
    /// what it drew and the release.
    fn held_in_a_wait() -> (
        super::RenderLink,
        Arc<std::sync::Mutex<Vec<String>>>,
        std::sync::mpsc::Sender<()>,
    ) {
        let drew = Arc::new(std::sync::Mutex::new(Vec::new()));
        let hold = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (release, released) = std::sync::mpsc::channel();
        let mut released = Some(released);
        let (noted, held) = (Arc::clone(&drew), Arc::clone(&hold));
        let make: MakeScreen = Box::new(move || {
            let release = released.take().expect("built once");
            Ok(Box::new(Paced {
                drew: Arc::clone(&noted),
                grants: AtomicU32::new(1),
                hold: Arc::clone(&held),
                release,
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
        link.show_end(page("result", None));
        assert!(drawn(&drew, "result"));
        hold.store(true, Ordering::SeqCst);
        assert!(wait_until(|| !hold.load(Ordering::SeqCst)));
        (link, drew, release)
    }

    /// Whether `drew` noted `what` within the bound.
    fn drawn(drew: &std::sync::Mutex<Vec<String>>, what: &str) -> bool {
        wait_until(|| drew.lock().unwrap().iter().any(|d| d == what))
    }

    /// Selection or the practice page entered while the render thread
    /// waits for a frame of the end screen, in the rig's order: the end
    /// screen is left ([`super::RenderLink::clear_end`]), then the page is
    /// sent. The frame the wait granted goes to the page instead of being
    /// lost with the end page, so on a screen paced by one frame of
    /// latency the page is drawn, and no end frame after it.
    #[test]
    fn a_menu_entered_during_an_end_frame_wait_is_drawn() {
        let (link, drew, release) = held_in_a_wait();
        link.clear_end();
        link.send(RenderMsg::Menu(Box::default()));
        release.send(()).unwrap();
        assert!(drawn(&drew, "<menu>"), "{:?}", drew.lock().unwrap());
        let drew = drew.lock().unwrap().clone();
        let menu = drew.iter().position(|d| d == "<menu>").unwrap();
        assert!(drew[menu..].iter().all(|d| d != "result"), "{drew:?}");
        link.exit();
    }

    /// [`a_menu_entered_during_an_end_frame_wait_is_drawn`] for a retry:
    /// the end screen is left as the screen is prepared
    /// ([`crate::play::prepare_render`]), and the next play's frames are
    /// drawn.
    #[test]
    fn a_retry_during_an_end_frame_wait_draws_its_play() {
        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap();
        let (rule, device) = (rules::tapestry(), DeviceProfile::default());
        let (_logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            Vec::new(),
            LogicConfig {
                inputs: 0,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        let (link, drew, release) = held_in_a_wait();
        let resume = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(100));
            release.send(()).unwrap();
        });
        let mut end_page = Some(super::EndPage::default());
        assert!(crate::play::prepare_render(&mut end_page, &link).is_ok());
        resume.join().unwrap();
        let clock = QpcClock::start();
        link.send(RenderMsg::Play {
            state_origin: TimeUs(0),
            side,
            gauge: 0,
            measure: play_measure(clock.now()),
        });
        assert!(drawn(&drew, "<play>"), "{:?}", drew.lock().unwrap());
        link.exit();
    }

    /// Publishes coalesce: one wake waits however many sizes are
    /// published, and the newest is read. Reading the size between frames
    /// while that wake is still in the channel takes nothing: a publish
    /// after the read sends no second wake. Only taking the wake re-arms
    /// it, and the size read after that is the newest. Before a render
    /// thread follows, a publish marks no wake, so the first publish after
    /// the sender is set sends one; a wake sent between the sender's
    /// registration and the `Follow` being taken stays the only one, since
    /// taking `Follow` takes no wake. With the read clearing the flag, the
    /// publish after it would send a second wake; with a publish marking a
    /// wake before a sender exists, the first publish after it would send
    /// none; with `Follow` taking the wake, the publish after it would send
    /// a second one.
    #[test]
    fn extent_publishes_coalesce() {
        let extent = super::Extent::default();
        assert_eq!(extent.latest(), None);
        // No sender yet: nothing is marked.
        extent.publish(640, 480);
        assert_eq!(extent.latest(), Some((640, 480)));
        // `RenderLink::follow`: the sender is set, then `Follow` is sent;
        // a publish lands before the render thread takes `Follow`.
        let (tx, rx) = std::sync::mpsc::channel();
        *extent.wake.lock().unwrap() = Some(tx);
        extent.publish(800, 600);
        assert!(matches!(rx.try_recv(), Ok(RenderMsg::Resize)));
        // `Follow` is taken, then a frame reads the size while the wake
        // waits, then more publishes.
        assert_eq!(extent.latest(), Some((800, 600)));
        extent.publish(0, 0);
        extent.publish(2_560, 1_080);
        assert!(rx.try_recv().is_err(), "one wake for three publishes");
        // The render thread takes the wake, then reads the newest.
        extent.acknowledge();
        assert_eq!(extent.latest(), Some((2_560, 1_080)));
        extent.publish(u32::MAX, 1);
        assert!(matches!(rx.try_recv(), Ok(RenderMsg::Resize)));
        assert!(rx.try_recv().is_err());
        extent.acknowledge();
        assert_eq!(extent.latest(), Some((u32::MAX, 1)));
    }

    /// The sizes applied to a [`Resizing`] screen, each with the screen's
    /// `id`.
    type Applied = Arc<std::sync::Mutex<Vec<(u32, u32, u32)>>>;

    /// A render thread with nothing drawn (no page), so only a message
    /// wakes it, built on a [`Resizing`] screen; with the sizes applied.
    fn resizing_thread() -> (super::RenderLink, Applied) {
        let resized = Arc::new(std::sync::Mutex::new(Vec::new()));
        let r = Arc::clone(&resized);
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Resizing {
                id: 1,
                size: (1_920, 1_080),
                resized: Arc::clone(&r),
                frames: Arc::new(std::sync::Mutex::new(Vec::new())),
                waited_minimized: Arc::new(AtomicU32::new(0)),
                fail: Arc::new(AtomicBool::new(false)),
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
        (link, resized)
    }

    /// Publishes racing each registration: a thread publishes on a fresh
    /// slot, released with the registration by a barrier, so publishes
    /// land between the sender's registration and its `Follow`. Once every
    /// message sent is taken, only a wake applies a size published after,
    /// so the slot's flag was left clear. Were the lock let go before the
    /// `Follow` is sent, a `Resize` could reach the render thread before it
    /// follows the slot, be left unacknowledged, and leave the flag set, so
    /// that later publishes would wake nothing.
    #[test]
    fn publishes_during_registration_still_wake_the_render_thread() {
        let (link, resized) = resizing_thread();
        let last = || resized.lock().unwrap().last().copied();
        for round in 1..=100 {
            let extent = Arc::new(super::Extent::default());
            let start = Arc::new(std::sync::Barrier::new(2));
            let (published, go) = (Arc::clone(&extent), Arc::clone(&start));
            let publisher = std::thread::spawn(move || {
                go.wait();
                for w in 1..=200 {
                    published.publish(w, 1);
                }
            });
            start.wait();
            link.follow(&extent);
            publisher.join().unwrap();
            link.end_loading().unwrap();
            extent.publish(4_096, round);
            assert!(
                wait_until(|| last() == Some((1, 4_096, round))),
                "{round}: {:?}",
                last()
            );
        }
        link.exit();
    }

    /// A slot followed again by a new render thread: the wake published
    /// after the first thread ended was sent nowhere, and its flag does
    /// not keep the new thread from being woken.
    #[test]
    fn a_new_render_thread_is_woken_after_the_last_ended() {
        let extent = Arc::new(super::Extent::default());
        let (first, _) = resizing_thread();
        first.follow(&extent);
        first.end_loading().unwrap();
        assert_eq!(first.exit(), super::RenderEnd::Joined);
        extent.publish(800, 600);
        let (second, resized) = resizing_thread();
        let last = || resized.lock().unwrap().last().copied();
        second.follow(&extent);
        assert!(wait_until(|| last() == Some((1, 800, 600))), "{:?}", last());
        second.end_loading().unwrap();
        extent.publish(1_280, 720);
        assert!(
            wait_until(|| last() == Some((1, 1_280, 720))),
            "{:?}",
            last()
        );
        second.exit();
    }

    /// Following `a`, then `b`, then `a` again: `a`'s wake, sent after
    /// `b`'s `Follow`, is taken while `b` is followed, so it acknowledges
    /// `b`; following `a` again clears the flag that wake left, and `a`'s
    /// later publishes wake the render thread.
    #[test]
    fn a_slot_followed_again_is_woken() {
        let (link, resized) = resizing_thread();
        let last = || resized.lock().unwrap().last().copied();
        let (a, b) = (
            Arc::new(super::Extent::default()),
            Arc::new(super::Extent::default()),
        );
        link.follow(&a);
        link.follow(&b);
        a.publish(800, 600);
        link.follow(&a);
        assert!(wait_until(|| last() == Some((1, 800, 600))), "{:?}", last());
        link.end_loading().unwrap();
        a.publish(1_280, 720);
        assert!(
            wait_until(|| last() == Some((1, 1_280, 720))),
            "{:?}",
            last()
        );
        link.exit();
    }

    /// A screen paced as a swap chain is, each the `id`th built, noting
    /// every size applied to it and every menu frame with whether it was
    /// minimized, and how often it was waited on while minimized. Its menu
    /// frame fails once when `fail` is set.
    struct Resizing {
        id: u32,
        size: (u32, u32),
        resized: Arc<std::sync::Mutex<Vec<(u32, u32, u32)>>>,
        frames: Arc<std::sync::Mutex<Vec<bool>>>,
        waited_minimized: Arc<AtomicU32>,
        fail: Arc<AtomicBool>,
    }

    impl Screen for Resizing {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            if self.minimized() {
                self.waited_minimized.fetch_add(1, Ordering::SeqCst);
            }
            true
        }
        fn paced(&self) -> bool {
            true
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            if self.fail.swap(false, Ordering::SeqCst) {
                return Err(RenderError::SurfaceLost);
            }
            let minimized = self.minimized();
            self.frames.lock().unwrap().push(minimized);
            Ok(timing(!minimized))
        }
        fn resize(&mut self, width: u32, height: u32) {
            self.resized.lock().unwrap().push((self.id, width, height));
            self.size = (width, height);
        }
        fn minimized(&self) -> bool {
            self.size.0 == 0 || self.size.1 == 0
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The render thread follows the window's size: the size published
    /// before it followed is applied, then each newest one, each once;
    /// minimized, the select page is drawn on unpaced and unpresented and
    /// a preparation is answered, with no wait on the screen; after the
    /// burst the newest size is the one applied, and the page, sent again
    /// since a preparation stops it, is drawn presented; and a screen built
    /// again takes the size afresh. With the size applied only on a
    /// message, the first size would never reach the screen; with the
    /// pacing kept while minimized, the screen would be waited on.
    #[test]
    fn the_render_thread_follows_the_newest_size() {
        let resized = Arc::new(std::sync::Mutex::new(Vec::new()));
        let frames = Arc::new(std::sync::Mutex::new(Vec::new()));
        let waited_minimized = Arc::new(AtomicU32::new(0));
        let fail = Arc::new(AtomicBool::new(false));
        let builds = Arc::new(AtomicU32::new(0));
        let (r, f, w, x, b) = (
            Arc::clone(&resized),
            Arc::clone(&frames),
            Arc::clone(&waited_minimized),
            Arc::clone(&fail),
            Arc::clone(&builds),
        );
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Resizing {
                id: b.fetch_add(1, Ordering::SeqCst) + 1,
                size: (1_920, 1_080),
                resized: Arc::clone(&r),
                frames: Arc::clone(&f),
                waited_minimized: Arc::clone(&w),
                fail: Arc::clone(&x),
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
        let last = || resized.lock().unwrap().last().copied();
        let extent = Arc::new(super::Extent::default());
        extent.publish(800, 600);
        link.follow(&extent);
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| last() == Some((1, 800, 600))));
        extent.publish(0, 0);
        assert!(wait_until(|| last() == Some((1, 0, 0))));
        let before = frames.lock().unwrap().len();
        assert!(wait_until(|| frames.lock().unwrap().len() >= before + 3));
        assert!(frames.lock().unwrap().last() == Some(&true));
        assert!(link.prepare().is_ok());
        // A preparation stops the select screen (`after_preparing`): the
        // page is sent again, as selection sends it after a load.
        link.send(RenderMsg::Menu(Box::default()));
        assert_eq!(waited_minimized.load(Ordering::SeqCst), 0);
        for size in [(1_024, 768), (1_280, 720), (2_560, 1_080)] {
            extent.publish(size.0, size.1);
        }
        assert!(wait_until(|| last() == Some((1, 2_560, 1_080))));
        let before = frames.lock().unwrap().len();
        assert!(wait_until(|| frames.lock().unwrap().len() > before));
        assert!(frames.lock().unwrap().last() == Some(&false));
        fail.store(true, Ordering::SeqCst);
        assert!(wait_until(|| link.failed()));
        assert!(link.prepare().is_ok());
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| last() == Some((2, 2_560, 1_080))));
        link.exit();
        let resized = resized.lock().unwrap().clone();
        // Each size applied once, in order.
        assert!(resized.windows(2).all(|p| p[0] != p[1]), "{resized:?}");
        assert_eq!(resized.first(), Some(&(1, 800, 600)));
        assert_eq!(resized.iter().filter(|r| r.0 == 2).count(), 1);
        assert_eq!(builds.load(Ordering::SeqCst), 2);
    }

    /// While the window is minimized during loading, loading frames go on
    /// unpaced and unpresented, a preparation and the end of loading are
    /// answered; restored, the newest size is applied and the next
    /// lifetime's loading frames are presented again.
    #[test]
    fn loading_follows_a_minimized_window_and_its_handshakes_complete() {
        let resized = Arc::new(std::sync::Mutex::new(Vec::new()));
        let frames = Arc::new(std::sync::Mutex::new(Vec::new()));
        let waited_minimized = Arc::new(AtomicU32::new(0));
        let (r, f, w) = (
            Arc::clone(&resized),
            Arc::clone(&frames),
            Arc::clone(&waited_minimized),
        );
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Resizing {
                id: 1,
                size: (1_920, 1_080),
                resized: Arc::clone(&r),
                frames: Arc::clone(&f),
                waited_minimized: Arc::clone(&w),
                fail: Arc::new(AtomicBool::new(false)),
            }) as Box<dyn Screen>)
        });
        let link = spawn_empty(make);
        let last = || resized.lock().unwrap().last().copied();
        let drawn = || frames.lock().unwrap().len();
        let extent = Arc::new(super::Extent::default());
        link.follow(&extent);
        link.loading().show(one_line("A"));
        assert!(wait_until(|| drawn() > 0));
        extent.publish(0, 0);
        assert!(wait_until(|| last() == Some((1, 0, 0))));
        let from = drawn();
        assert!(wait_until(|| drawn() >= from + 3));
        assert_eq!(frames.lock().unwrap().last(), Some(&true));
        assert!(link.prepare().is_ok());
        link.end_loading().unwrap();
        assert_eq!(waited_minimized.load(Ordering::SeqCst), 0);
        extent.publish(1_280, 720);
        assert!(wait_until(|| last() == Some((1, 1_280, 720))));
        link.loading().show(one_line("B"));
        let from = drawn();
        assert!(wait_until(|| drawn() > from));
        assert_eq!(frames.lock().unwrap().last(), Some(&false));
        link.end_loading().unwrap();
        link.exit();
    }

    /// A real declarative bundle with nothing to draw: no image, no
    /// element, its lanes laid out for the right scratch side, which no
    /// attempt of the simple skin's has.
    pub(crate) fn bundle() -> Arc<render::SkinBundle> {
        bundle_closing(None)
    }

    /// [`bundle`] declaring Closing's duration `close`.
    pub(crate) fn bundle_closing(close: Option<TimeUs>) -> Arc<render::SkinBundle> {
        bundle_declaring(close, None)
    }

    /// [`bundle`] declaring Closing's duration `close` and the loading
    /// time `loadend`.
    pub(crate) fn bundle_declaring(
        close: Option<TimeUs>,
        loadend: Option<TimeUs>,
    ) -> Arc<render::SkinBundle> {
        bundle_ending(close, loadend, skin_model::PlayEnding::default())
    }

    /// [`bundle_declaring`] whose skin also declares the play's ending
    /// `ending`.
    pub(crate) fn bundle_ending(
        close: Option<TimeUs>,
        loadend: Option<TimeUs>,
        ending: skin_model::PlayEnding,
    ) -> Arc<render::SkinBundle> {
        use skin_model::{Anchor, AtlasLimits, Fit, Region, RegionDef, Sheet, SkinDef, Variant};
        let sheet = Sheet::new(
            std::path::Path::new(env!("CARGO_MANIFEST_DIR")),
            Vec::new(),
            Vec::new(),
        )
        .unwrap();
        let skin = skin_model::Skin::new(SkinDef {
            fit: Fit::Fixed,
            regions: [RegionDef {
                anchor: Anchor::TopLeft,
                optional: false,
            }; Region::COUNT],
            whens: Vec::new(),
            sheet,
            lanes: None,
            elements: Vec::new(),
            widgets: skin_model::WidgetDefs::default(),
            bomb_up_to: None,
            close,
            loadend,
            present: None,
        })
        .unwrap()
        .with_play_ending(ending);
        let limits = AtlasLimits {
            max_side: 256,
            max_layers: 4,
            reserved_layers: 1,
        };
        let atlas = skin.atlas(Variant::Lo, limits, &|| false).unwrap();
        let clips = render::ClipTable::new(Vec::new()).unwrap();
        Arc::new(
            render::SkinBundle::new(Arc::new(skin), atlas, clips, skin_model::Side::Right).unwrap(),
        )
    }

    /// The attempt of `chart`'s whole play with the title "T".
    fn titled(chart: &Chart) -> AttemptContext {
        preset_attempt(chart, &rules::tapestry(), None, "T")
    }

    /// What an [`Installer`] was asked, in order.
    type Log = Arc<std::sync::Mutex<Vec<String>>>;

    /// A screen that stages a declarative skin's installation as the
    /// renderer does, noting each step with its device: a preparation
    /// answered (`prepare` with its request, the context's scratch side
    /// and the attempt's title), an installation (`install`, or `refuse`
    /// while `refuse_install` is set, its request, with its kind unless
    /// play's), a commit, a discard, a restore (with its kind unless
    /// play's) and a removal (`remove` with its kind). A restore of the
    /// kind in `refuse_restore` is refused as on a healthy device, and one
    /// of the kind in `fail_restore` fails as a lost surface. While `hold`
    /// is set, its preparations do not answer;
    /// while `hold_install` is set, an installation waits. Its menu and
    /// loading frames fail while `fail` is set; it counts its loading
    /// frames and notes their origins, each of which stalls while `stall`
    /// is set and is refused while `refuse_loading` is set.
    pub(crate) struct Installer {
        device: u64,
        /// The flags, counters and records of every installer its render
        /// thread builds.
        shared: Installers,
        /// The installation staged by [`Screen::install`], not committed,
        /// with its kind.
        staged: Option<(ScreenKind, render::Installation)>,
        /// Each kind's installation committed or restored.
        installed: super::Committed,
        /// The request whose staging is held unanswered
        /// (`hold_staged`).
        held: Option<u64>,
        /// The target's size as last applied (1920×1080 until a resize).
        target: [u32; 2],
        /// The attempt staged and the one committed, as the carry sees
        /// them.
        staged_play: Option<FakePlay>,
        held_play: Option<FakePlay>,
    }

    /// An attempt's `Play` as the carry sees it: its slots and whether its
    /// callbacks with slots run.
    struct FakePlay {
        bank: skin_model::present::Bank,
        slots_run: bool,
        /// The best and target its context gave, which the skin's `Play`
        /// keeps ([`skin_model::Play::score`]).
        score: skin_model::ScoreBasis,
    }

    impl Installer {
        fn note(&self, step: &str) {
            self.shared
                .log
                .lock()
                .unwrap()
                .push(format!("{} {step}", self.device));
        }
    }

    impl Screen for Installer {
        fn resize(&mut self, width: u32, height: u32) {
            if width > 0 && height > 0 {
                self.target = [width, height];
            }
        }
        fn target(&self) -> [u32; 2] {
            self.target
        }
        fn load(
            &mut self,
            chart: &Chart,
            section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            self.shared
                .chart_loads
                .lock()
                .unwrap()
                .push((std::ptr::from_ref(chart) as usize, section.is_some()));
            if self
                .shared
                .lose_chart_loads
                .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                .is_ok()
            {
                return Err(RenderError::SurfaceLost);
            }
            if self.shared.refuse_chart_loads.load(Ordering::Acquire) {
                return Err(RenderError::Skin(render::SkinError {
                    reason: "refused".to_owned(),
                }));
            }
            if self.shared.hold_load.load(Ordering::Acquire) {
                self.shared.load_held.store(true, Ordering::Release);
                while self.shared.hold_load.load(Ordering::Acquire) {
                    std::thread::park_timeout(Duration::from_millis(1));
                }
            }
            // A chart's load drops the committed attempt's `Play`.
            self.held_play = None;
            Ok(())
        }
        fn prepare_attempt(
            &mut self,
            a: &render::AttemptInput<'_>,
            skin: Option<&Arc<render::SkinBundle>>,
            request: u64,
        ) -> Option<Result<Preparation, RenderError>> {
            if self.shared.hold.load(Ordering::Acquire) {
                return None;
            }
            assert!(
                !self.shared.panic_prepare.swap(false, Ordering::AcqRel),
                "the render thread panics for the test"
            );
            // Read once: the request whose preparation stages and holds.
            let hold = self.shared.hold_staged.load(Ordering::Acquire) == request;
            // Staged already and held: asked again, still no answer.
            if hold && self.held == Some(request) {
                return None;
            }
            // Without a skin chosen, the installation in use draws.
            assert!(skin.is_some() || self.installed.get(ScreenKind::Play).is_some());
            self.note(&format!(
                "prepare {request} {:?} {}",
                a.context.scratch, a.text.title
            ));
            // What the attempt was offered, and its `Play` as the skin
            // would prepare it: the slots carried, or the initial ones.
            let (offered, play) = match a.continuation {
                skin_model::Continuation::Fresh => (None, (CARRY_INIT.to_vec(), true)),
                skin_model::Continuation::Carried { score, bank } => (
                    Some((score, bank.map(|b| b.values().to_vec()))),
                    bank.map_or((CARRY_INIT.to_vec(), false), |b| {
                        (b.values().to_vec(), true)
                    }),
                ),
            };
            self.shared.offers.lock().unwrap().push((request, offered));
            self.staged_play = Some(FakePlay {
                bank: skin_model::present::Bank::of(play.0),
                slots_run: play.1,
                score: a.context.score.basis,
            });
            // The skin given is staged with the attempt, as the renderer
            // stages it, and installed only by the commit.
            if let Some(skin) = skin {
                self.staged = Some((
                    ScreenKind::Play,
                    render::Installation::new(Arc::clone(skin)),
                ));
                *self.shared.staging.lock().unwrap() = Some(Arc::clone(skin));
            }
            // A screen may stage and answer later.
            if hold {
                self.held = Some(request);
                return None;
            }
            Some(Ok(Preparation::Ready))
        }
        fn device(&self) -> u64 {
            self.device
        }
        fn commit(&mut self, token: render::PreparedToken) -> Result<(), render::CommitError> {
            if token.device != self.device {
                return Err(render::CommitError::Device);
            }
            self.note(&format!("commit {}", token.request));
            while self.shared.hold_commit.load(Ordering::Acquire) {
                self.shared.commit_held.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(1));
            }
            if let Some((kind, staged)) = self.staged.take() {
                if kind == ScreenKind::Play {
                    *self.shared.play.lock().unwrap() = Some(Arc::clone(staged.bundle()));
                }
                self.installed.set(kind, Some(staged));
            }
            *self.shared.staging.lock().unwrap() = None;
            // A play attempt's `Play` is the screen's now; the round it
            // plays leaves the slots `round` scripts.
            if token.kind == ScreenKind::Play
                && let Some(mut play) = self.staged_play.take()
            {
                if play.slots_run
                    && let Some(vals) = self.shared.round.lock().unwrap().clone()
                {
                    play.bank = skin_model::present::Bank::of(vals);
                }
                self.shared
                    .committed_scores
                    .lock()
                    .unwrap()
                    .push(play.score);
                self.held_play = Some(play);
            }
            Ok(())
        }
        fn carry(&self) -> Option<skin_model::Carry<'_>> {
            self.held_play.as_ref().map(|p| skin_model::Carry {
                bank: &p.bank,
                slots_run: p.slots_run,
                terminal: p.slots_run && self.shared.witness.load(Ordering::Acquire),
                targets: 4,
            })
        }
        fn discard(&mut self) {
            self.note("discard");
            self.staged = None;
            self.staged_play = None;
            *self.shared.staging.lock().unwrap() = None;
        }
        fn install(
            &mut self,
            kind: ScreenKind,
            skin: &Arc<render::SkinBundle>,
            request: u64,
        ) -> Result<Preparation, RenderError> {
            let request = match kind {
                ScreenKind::Play => request.to_string(),
                _ => format!("{kind:?} {request}"),
            };
            while self.shared.hold_install.load(Ordering::Acquire) {
                self.shared.install_held.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(1));
            }
            if self.shared.fail.load(Ordering::Acquire) {
                self.note(&format!("fail {request}"));
                return Err(RenderError::SurfaceLost);
            }
            if self.shared.refuse_install.load(Ordering::Acquire) {
                self.note(&format!("refuse {request}"));
                return Ok(super::refusal("the skin cannot be installed"));
            }
            self.note(&format!("install {request}"));
            self.staged = Some((kind, render::Installation::new(Arc::clone(skin))));
            Ok(Preparation::Ready)
        }
        fn installation(&self, kind: ScreenKind) -> Option<render::Installation> {
            self.installed.get(kind).cloned()
        }
        fn atlas_limits(&self) -> Option<skin_model::AtlasLimits> {
            Some(skin_model::AtlasLimits {
                max_side: 8_192,
                max_layers: 256,
                reserved_layers: render::RESERVED_LAYERS,
            })
        }
        fn restore(
            &mut self,
            kind: ScreenKind,
            installation: &render::Installation,
        ) -> Result<(), RenderError> {
            match kind {
                ScreenKind::Play => self.note("restore"),
                _ => self.note(&format!("restore {kind:?}")),
            }
            if *self.shared.refuse_restore.lock().unwrap() == Some(kind) {
                return Err(RenderError::Admission {
                    kind,
                    needed: render::SHARED_TEXELS + 1,
                    limit: render::SHARED_TEXELS,
                });
            }
            if *self.shared.fail_restore.lock().unwrap() == Some(kind) {
                return Err(RenderError::SurfaceLost);
            }
            if kind == ScreenKind::Play {
                *self.shared.play.lock().unwrap() = Some(Arc::clone(installation.bundle()));
            }
            self.installed.set(kind, Some(installation.clone()));
            self.shared
                .restored
                .lock()
                .unwrap()
                .push((kind, installation.clone()));
            Ok(())
        }
        fn remove(&mut self, kind: ScreenKind) {
            self.note(&format!("remove {kind:?}"));
            while self.shared.hold_remove.load(Ordering::Acquire) {
                self.shared.remove_held.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(1));
            }
            self.installed.clear(kind);
        }
        fn wait(&self) -> bool {
            self.shared.waits.fetch_add(1, Ordering::AcqRel);
            while self.shared.pace.load(Ordering::Acquire) {
                let permits = &self.shared.permits;
                if permits
                    .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                    .is_ok()
                {
                    break;
                }
                self.shared.held.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(1));
            }
            self.shared.held.store(false, Ordering::Release);
            true
        }
        fn paced(&self) -> bool {
            self.shared.pace.load(Ordering::Acquire)
        }
        fn splash(&mut self, _elapsed: Duration) -> Option<Result<bool, RenderError>> {
            let shown = self.shared.splash.load(Ordering::Acquire);
            if shown {
                self.shared.splashes.fetch_add(1, Ordering::AcqRel);
            }
            shown.then_some(Ok(true))
        }
        fn selection(
            &mut self,
            update: &crate::selection_skin::SelectionUpdate,
            logic: Option<&mut crate::selection_skin::SelectionLogic>,
            changed: bool,
            now: TimeUs,
            app: skin_model::AppReadouts,
        ) -> Result<crate::selection_skin::SelectionDrawn, RenderError> {
            if self.shared.fail.load(Ordering::Acquire) {
                return Err(RenderError::SurfaceLost);
            }
            // A skin draws only with its installation on this screen.
            let logic = logic.filter(|_| self.installed.get(ScreenKind::Selection).is_some());
            let state = logic.as_ref().map(|l| l.id);
            let seen = SelectionSeen {
                device: self.device,
                lifetime: update.lifetime,
                revision: update.revision,
                state,
                changed,
                events: update.events,
                now,
                app,
            };
            self.shared.selections.lock().unwrap().push(seen);
            if changed {
                self.shared.updates.lock().unwrap().push(update.clone());
            }
            // The skin's frame at `now`, built by the model alone, when the
            // skin is a selection skin.
            if let Some(logic) = logic.filter(|_| state.is_some()) {
                let skin = Arc::clone(logic.bundle.skin());
                if skin
                    .update_selection(&mut logic.sel, &update.view, &update.labels)
                    .is_ok()
                {
                    let mut batch = skin_model::QuadBatch::with_capacity(
                        skin.quad_capacity(),
                        skin.run_capacity(),
                    );
                    let frame = skin_model::SelectionFrame {
                        now,
                        target: [1_920, 1_080],
                        layout: &skin_model::Layout::AUTHOR,
                        timers: update.events.timers(),
                        app,
                    };
                    if skin
                        .build_selection(&mut logic.sel, &frame, logic.bundle.uvs(), &mut batch)
                        .is_ok()
                    {
                        let rects = batch.quads().iter().map(|q| q.rect).collect();
                        self.shared.built.lock().unwrap().push((seen, now, rects));
                    }
                }
            }
            let refused = (state.is_some() && self.shared.refuse_selection.load(Ordering::Acquire))
                .then(|| "refused for the test".to_owned());
            Ok(crate::selection_skin::SelectionDrawn {
                timing: timing(true),
                refused,
            })
        }
        fn build(&mut self, _f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            if self.shared.fail.load(Ordering::Acquire) {
                return Err(RenderError::SurfaceLost);
            }
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        fn menu(
            &mut self,
            page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            if self.shared.fail.load(Ordering::Acquire) {
                return Err(RenderError::SurfaceLost);
            }
            let first = page
                .lines
                .first()
                .map_or_else(String::new, |l| l.text.clone());
            self.shared.menu_frames.lock().unwrap().push(first);
            Ok(timing(true))
        }
        fn loading(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _update: Option<&LoadingUpdate>,
            _changed: bool,
            frame: &render::LoadingFrame<'_>,
        ) -> Result<Result<FrameTiming, skin_model::BuildError>, RenderError> {
            if self.shared.fail.load(Ordering::Acquire) {
                return Err(RenderError::SurfaceLost);
            }
            self.shared.loading.fetch_add(1, Ordering::AcqRel);
            self.shared.origins.lock().unwrap().push((
                self.device,
                frame.lifetime,
                frame.load.since,
            ));
            while self.shared.stall.load(Ordering::Acquire) {
                self.shared.stalled.store(true, Ordering::Release);
                std::thread::sleep(Duration::from_millis(1));
            }
            self.shared.stalled.store(false, Ordering::Release);
            if self.shared.refuse_loading.load(Ordering::Acquire) {
                return Ok(Err(skin_model::BuildError::InvalidReadouts));
            }
            Ok(Ok(timing(true)))
        }
        fn pending_loading(
            &mut self,
            _page: &crate::menu_screen::MenuPage,
            _changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            if self.shared.fail.load(Ordering::Acquire) {
                return Err(RenderError::SurfaceLost);
            }
            self.shared.pending.fetch_add(1, Ordering::AcqRel);
            Ok(timing(true))
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The shared state of the [`Installer`]s a render thread builds,
    /// their devices numbered from 1.
    #[derive(Clone, Default)]
    pub(crate) struct Installers {
        /// The screens built so far.
        pub(crate) builds: Arc<AtomicU32>,
        hold: Arc<AtomicBool>,
        /// Every frame and installation fails as a lost device while set.
        pub(crate) fail: Arc<AtomicBool>,
        /// Loading frames drawn, refused ones included.
        pub(crate) loading: Arc<AtomicU32>,
        /// Each menu frame drawn (a page, the decision screen's included):
        /// its first line.
        pub(crate) menu_frames: Arc<std::sync::Mutex<Vec<String>>>,
        /// This many chart loads fail as a lost surface.
        pub(crate) lose_chart_loads: Arc<AtomicU32>,
        /// Every call into the screen builder, failed ones included.
        pub(crate) build_calls: Arc<AtomicU32>,
        /// Screen builds wait while set.
        pub(crate) hold_builds: Arc<AtomicBool>,
        /// Set while a build waits on `hold_builds`.
        pub(crate) build_held: Arc<AtomicBool>,
        /// This many screen builds fail as a lost device.
        pub(crate) fail_builds: Arc<AtomicU32>,
        /// Chart loads are refused as a skin refuses them while set.
        pub(crate) refuse_chart_loads: Arc<AtomicBool>,
        /// Each chart load: the chart's address and whether a section came
        /// with it.
        pub(crate) chart_loads: Arc<std::sync::Mutex<Vec<(usize, bool)>>>,
        /// Loading frames drawn as the pending presentation.
        pending: Arc<AtomicU32>,
        stall: Arc<AtomicBool>,
        /// Set while a loading frame is held by `stall`.
        stalled: Arc<AtomicBool>,
        /// Installations wait while set.
        pub(crate) hold_install: Arc<AtomicBool>,
        /// Set once an installation waits on `hold_install`.
        pub(crate) install_held: Arc<AtomicBool>,
        /// Installations are refused while set.
        pub(crate) refuse_install: Arc<AtomicBool>,
        /// Loading frames are refused while set.
        pub(crate) refuse_loading: Arc<AtomicBool>,
        /// A chart's load waits, once noted, while set.
        pub(crate) hold_load: Arc<AtomicBool>,
        /// Set once a chart's load waits on `hold_load`.
        pub(crate) load_held: Arc<AtomicBool>,
        /// Commits wait, once noted, while set.
        hold_commit: Arc<AtomicBool>,
        /// Set once a commit waits on `hold_commit`.
        commit_held: Arc<AtomicBool>,
        /// Removals wait, once noted, while set.
        hold_remove: Arc<AtomicBool>,
        /// Set once a removal waits on `hold_remove`.
        remove_held: Arc<AtomicBool>,
        log: Log,
        /// Selection frames refuse while set, as a skin refusing one.
        pub(crate) refuse_selection: Arc<AtomicBool>,
        /// Frames are paced while set: each wait takes one of `permits`,
        /// holding until there is one.
        pub(crate) pace: Arc<AtomicBool>,
        pub(crate) permits: Arc<AtomicU32>,
        /// The waits begun.
        pub(crate) waits: Arc<AtomicU32>,
        /// Set while a wait holds for want of a permit.
        pub(crate) held: Arc<AtomicBool>,
        /// The screens draw a launch screen while set; none otherwise.
        pub(crate) splash: Arc<AtomicBool>,
        /// The launch screen frames drawn.
        pub(crate) splashes: Arc<AtomicU32>,
        /// Each selection frame: its update's lifetime and revision, the
        /// skin state's number (none without one), whether it was laid out
        /// again, and its timers.
        pub(crate) selections: Arc<std::sync::Mutex<Vec<SelectionSeen>>>,
        /// Each update a selection frame applied first, in order.
        pub(crate) updates: Arc<std::sync::Mutex<Vec<crate::selection_skin::SelectionUpdate>>>,
        /// Each selection frame a selection skin built (by the model
        /// alone): the frame, its time and its quads' rects.
        pub(crate) built: Arc<std::sync::Mutex<Vec<SelectionBuilt>>>,
        /// Each restore that installed, with its kind, in order.
        restored: Arc<std::sync::Mutex<Vec<(ScreenKind, render::Installation)>>>,
        /// Restores of this kind are refused as on a healthy device.
        refuse_restore: Arc<std::sync::Mutex<Option<ScreenKind>>>,
        /// Restores of this kind fail as a lost surface.
        fail_restore: Arc<std::sync::Mutex<Option<ScreenKind>>>,
        /// Each loading frame's device, lifetime and origin.
        origins: Arc<std::sync::Mutex<Vec<(u64, u64, TimeUs)>>>,
        /// The play bundle the screen built last installed, by a commit or
        /// a restore; a screen built again starts with none.
        play: Arc<std::sync::Mutex<Option<Arc<render::SkinBundle>>>>,
        /// The skin an attempt's preparation staged and neither committed
        /// nor discarded, on the screen built last.
        staging: Arc<std::sync::Mutex<Option<Arc<render::SkinBundle>>>>,
        /// The request whose preparation stages, then does not answer; 0
        /// for none.
        hold_staged: Arc<std::sync::atomic::AtomicU64>,
        /// The next attempt's preparation panics the render thread, once.
        pub(crate) panic_prepare: Arc<AtomicBool>,
        /// Each attempt's preparation: its request and what it was offered,
        /// `None` for a fresh one, else the point and the slots.
        offers: Arc<std::sync::Mutex<Vec<(u64, CarryOffered)>>>,
        /// The slots a committed attempt's round leaves, when its
        /// callbacks with slots run; `None` keeps them.
        round: Arc<std::sync::Mutex<Option<Vec<skin_model::present::Val>>>>,
        /// Whether the committed attempt's last build ran its callbacks
        /// with slots on a complete view.
        witness: Arc<AtomicBool>,
        /// The best and target of each attempt committed, in order.
        pub(crate) committed_scores: Arc<std::sync::Mutex<Vec<skin_model::ScoreBasis>>>,
    }

    /// What an attempt was offered: `None` fresh, else the cached score data and
    /// the slots, `None` when not valid.
    type CarryOffered = Option<(
        Option<skin_model::Settled>,
        Option<Vec<skin_model::present::Val>>,
    )>;

    /// The initial slots of the [`Installer`]'s attempts.
    const CARRY_INIT: [skin_model::present::Val; 1] = [skin_model::present::OFF];

    /// One selection frame as an [`Installer`] saw it.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(crate) struct SelectionSeen {
        pub(crate) device: u64,
        pub(crate) lifetime: u64,
        pub(crate) revision: u64,
        pub(crate) state: Option<u64>,
        pub(crate) changed: bool,
        pub(crate) events: crate::selection_skin::SelectionEvents,
        /// The frame's time and the application's readouts it was given.
        pub(crate) now: TimeUs,
        pub(crate) app: skin_model::AppReadouts,
    }

    /// A selection skin's frame as an [`Installer`] built it: the frame,
    /// its time and its quads' rects in layout pixels.
    pub(crate) type SelectionBuilt = (SelectionSeen, TimeUs, Vec<[f32; 4]>);

    impl Installers {
        /// A render thread on these installers with `chart` loaded and
        /// `skin` chosen for its attempts.
        pub(crate) fn spawn(
            &self,
            chart: &Arc<Chart>,
            skin: &Arc<render::SkinBundle>,
        ) -> super::RenderLink {
            let link = self.spawn_bare();
            link.load(Arc::clone(chart), None).unwrap();
            assert_eq!(
                link.select(ScreenKind::Play, Some(Arc::clone(skin)))
                    .wait()
                    .unwrap(),
                Preparation::Ready
            );
            link
        }

        /// A render thread on these installers, nothing loaded or chosen.
        pub(crate) fn spawn_bare(&self) -> super::RenderLink {
            self.spawn_with(Box::new(NoFrames))
        }

        /// [`spawn_bare`](Self::spawn_bare) with the frame sink `frames`.
        pub(crate) fn spawn_with(&self, frames: Box<dyn super::FrameSink>) -> super::RenderLink {
            let shared = self.clone();
            let make: MakeScreen = Box::new(move || {
                shared.build_calls.fetch_add(1, Ordering::AcqRel);
                while shared.hold_builds.load(Ordering::Acquire) {
                    shared.build_held.store(true, Ordering::Release);
                    std::thread::park_timeout(Duration::from_millis(1));
                }
                shared.build_held.store(false, Ordering::Release);
                if shared
                    .fail_builds
                    .try_update(Ordering::AcqRel, Ordering::Acquire, |n| n.checked_sub(1))
                    .is_ok()
                {
                    return Err(RenderError::SurfaceLost);
                }
                let device = u64::from(shared.builds.fetch_add(1, Ordering::SeqCst) + 1);
                *shared.play.lock().unwrap() = None;
                *shared.staging.lock().unwrap() = None;
                Ok(Box::new(Installer {
                    device,
                    shared: shared.clone(),
                    staged: None,
                    installed: super::Committed::default(),
                    held: None,
                    target: [1_920, 1_080],
                    staged_play: None,
                    held_play: None,
                }) as Box<dyn Screen>)
            });
            let clock = QpcClock::start();
            spawn(make, None, None, clock, view(), measure(clock), frames).unwrap()
        }

        pub(crate) fn log(&self) -> Vec<String> {
            self.log.lock().unwrap().clone()
        }

        /// The play bundle the screen built last has installed.
        pub(crate) fn play(&self) -> Option<Arc<render::SkinBundle>> {
            self.play.lock().unwrap().clone()
        }

        /// The skin staged and neither committed nor discarded.
        #[cfg(feature = "dev-tools")]
        pub(crate) fn staging(&self) -> Option<Arc<render::SkinBundle>> {
            self.staging.lock().unwrap().clone()
        }

        /// The next request `link` sends stages, then does not answer
        /// until [`release_staged`](Self::release_staged).
        #[cfg(feature = "dev-tools")]
        pub(crate) fn hold_next_staging(&self, link: &super::RenderLink) {
            let next = link.requests.load(Ordering::Relaxed) + 1;
            self.hold_staged.store(next, Ordering::Release);
        }

        /// A held staging answers again.
        #[cfg(feature = "dev-tools")]
        pub(crate) fn release_staged(&self) {
            self.hold_staged.store(0, Ordering::Release);
        }

        /// Attempts' preparations are not answered while `on`.
        #[cfg(feature = "dev-tools")]
        pub(crate) fn hold_preparations(&self, on: bool) {
            self.hold.store(on, Ordering::Release);
        }

        /// Holds every commit in the screen while `on`, or lets it go.
        #[cfg(feature = "dev-tools")]
        pub(crate) fn hold_commits(&self, on: bool) {
            if !on {
                self.commit_held.store(false, Ordering::Release);
            }
            self.hold_commit.store(on, Ordering::Release);
        }

        /// Whether a commit waited on [`hold_commits`](Self::hold_commits).
        #[cfg(feature = "dev-tools")]
        pub(crate) fn commit_held(&self) -> bool {
            self.commit_held.load(Ordering::Acquire)
        }
    }

    /// A chart of 4 taps, the carry tests' targets.
    fn four_taps() -> Arc<Chart> {
        let lane = Lane::new(1).unwrap();
        Arc::new(
            ChartBuilder::new(120_000)
                .tap(lane, TimeUs(10_000_000))
                .tap(lane, TimeUs(11_000_000))
                .tap(lane, TimeUs(12_000_000))
                .tap(lane, TimeUs(13_000_000))
                .build()
                .unwrap(),
        )
    }

    /// Prepares an attempt of play state `key` with `skin` staged and
    /// commits it; the request's offer.
    fn staged_attempt(
        screens: &Installers,
        link: &super::RenderLink,
        chart: &Chart,
        key: crate::carry::CarryKey,
        skin: &Arc<render::SkinBundle>,
    ) -> CarryOffered {
        let ready = link.prepare_attempt_with(
            titled(chart).with_carry_key(Some(key)),
            super::AttemptSkin::Specialized(Arc::clone(skin)),
            || false,
        );
        assert_eq!(ready.unwrap(), Preparation::Ready);
        link.commit_attempt().unwrap();
        screens.offers.lock().unwrap().last().unwrap().1.clone()
    }

    /// A witnessed round at a full score on a fresh link over 4 taps: the
    /// installers, the link, the chart and the skin.
    fn witnessed_round() -> (
        Installers,
        super::RenderLink,
        Arc<Chart>,
        Arc<render::SkinBundle>,
    ) {
        let chart = four_taps();
        let (screens, link, skin) = carry_link(&chart);
        *screens.round.lock().unwrap() = Some(latched());
        screens.witness.store(true, Ordering::Release);
        assert_eq!(
            carried_attempt(&screens, &link, &chart, CARRY_KEY, true),
            None
        );
        play_round(&link, &chart, full_tally());
        *screens.round.lock().unwrap() = None;
        (screens, link, chart, skin)
    }

    /// The screen lost after a commit and before its play, built again by
    /// the result's upload (code review r1, HIGH): the play still starts
    /// and its settlement takes the final point and leaves no valid slots.
    #[test]
    fn a_screen_lost_before_the_play_settles_the_point() {
        let (screens, link, chart, _) = witnessed_round();
        // Round 2 committed with the latch carried.
        assert_eq!(
            carried_attempt(&screens, &link, &chart, CARRY_KEY, true),
            Some((FULL, Some(latched())))
        );
        // A result upload fails as a lost device: the screen is dropped.
        screens.fail.store(true, Ordering::Release);
        let lost = link.upload_result(crate::result_skin::Upload::Bundle(bundle()));
        assert!(lost.wait().is_err());
        screens.fail.store(false, Ordering::Release);
        // The next upload builds the screen again, holding no `Play`.
        let again = link.upload_result(crate::result_skin::Upload::Bundle(bundle()));
        assert_eq!(again.wait().unwrap(), Preparation::Ready);
        play_round(&link, &chart, poor_tally());
        assert_eq!(
            carried_attempt(&screens, &link, &chart, CARRY_KEY, false),
            Some((POOR, None))
        );
        link.exit();
    }

    /// The pending marker committed in the play state invalidates the
    /// slots, and a specialization of the very bundle before does not
    /// bring them back (A, pending, A); a withdrawn marker keeps them.
    #[test]
    fn a_pending_marker_invalidates_the_slots() {
        for withdrawn in [false, true] {
            let (screens, link, chart, skin) = witnessed_round();
            if withdrawn {
                stall_render(&link, &screens);
                let mut pending = link.pend(ScreenKind::Play);
                let (_, apart) = std::sync::mpsc::sync_channel(1);
                let rx = std::mem::replace(&mut pending.rx, apart);
                drop(pending);
                screens.stall.store(false, Ordering::Release);
                let answer = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
                assert_eq!(answer, Preparation::Cancelled);
                link.end_loading().unwrap();
            } else {
                assert_eq!(
                    link.pend(ScreenKind::Play).wait().unwrap(),
                    Preparation::Ready
                );
            }
            let slots = withdrawn.then(latched);
            assert_eq!(
                staged_attempt(&screens, &link, &chart, CARRY_KEY, &skin),
                Some((FULL, slots)),
                "withdrawn: {withdrawn}"
            );
            link.exit();
        }
    }

    /// An attempt prepared in place on the installed skin, its selection
    /// cleared (`select(Play, None)`), draws with the same installation:
    /// it is offered the witnessed slots and the cached score data.
    #[test]
    fn an_in_place_prepare_keeps_the_record() {
        let (screens, link, chart, _) = witnessed_round();
        assert_eq!(
            link.select(ScreenKind::Play, None).wait().unwrap(),
            Preparation::Ready
        );
        assert_eq!(
            carried_attempt(&screens, &link, &chart, CARRY_KEY, false),
            Some((FULL, Some(latched())))
        );
        link.exit();
    }

    /// The play state and chart of the carry tests' attempts.
    const CARRY_KEY: crate::carry::CarryKey = crate::carry::CarryKey {
        play_state: 1,
        chart: 1,
    };

    /// A full score over the carry tests' 4 targets: point 200 000, EX 8.
    fn full_tally() -> eval::Tally {
        let mut t = eval::Tally::default();
        t.counts.perfect = 4;
        t.ex_score = 8;
        t.max_combo = 4;
        t.combo = 4;
        t
    }

    /// The score data [`full_tally`] settles over 4 targets.
    const FULL: Option<skin_model::Settled> = Some(skin_model::Settled {
        point: 200_000,
        ex_score: 8,
        rate_current_bp: 10_000,
        rate_whole_bp: 10_000,
    });

    /// One POOR over the carry tests' 4 targets: point 0.
    fn poor_tally() -> eval::Tally {
        let mut t = eval::Tally::default();
        t.counts.poor = 1;
        t
    }

    /// The score data [`poor_tally`] settles: every reading 0.
    const POOR: Option<skin_model::Settled> = Some(skin_model::Settled {
        point: 0,
        ex_score: 0,
        rate_current_bp: 0,
        rate_whole_bp: 0,
    });

    /// s7-06s S4: the settled score data is over the settled attempt's own
    /// targets: a round of the play state over 7 taps after a full score
    /// over 4 is offered the whole rate over 4 (100 %), not over 7, before
    /// and after that round commits.
    #[test]
    fn a_settled_score_is_over_its_own_targets() {
        let chart = four_taps();
        let (screens, link, _) = carry_link(&chart);
        let _ = carried_attempt(&screens, &link, &chart, CARRY_KEY, true);
        play_round(&link, &chart, full_tally());
        let lane = Lane::new(1).unwrap();
        let seven = Arc::new(
            (0..7)
                .fold(ChartBuilder::new(120_000), |b, i| {
                    b.tap(lane, TimeUs(10_000_000 + i * 1_000_000))
                })
                .build()
                .unwrap(),
        );
        link.load(Arc::clone(&seven), None).unwrap();
        // Offered, then committed over 7 targets and offered again: the
        // record keeps what the 4-target round settled.
        assert_eq!(
            carried_attempt(&screens, &link, &seven, CARRY_KEY, true),
            Some((FULL, None))
        );
        assert_eq!(
            carried_attempt(&screens, &link, &seven, CARRY_KEY, false),
            Some((FULL, None))
        );
        link.exit();
    }

    /// s7-06s S4b: the settlement counts the empty POORs as the committed
    /// attempt's preset does: one PGREAT and two POORs, one of them an
    /// empty POOR the preset counts, judge two targets (current rate
    /// 50 %), where an uncounted rule would judge three (33.33 %).
    #[test]
    fn the_settlement_counts_empty_poors_as_the_attempt_does() {
        let chart = four_taps();
        let (screens, link, _) = carry_link(&chart);
        let mut counted = rules::tapestry();
        counted.empty_poor.counted = true;
        let ready = link.prepare_attempt(
            preset_attempt(&chart, &counted, None, "T").with_carry_key(Some(CARRY_KEY)),
            || false,
        );
        assert_eq!(ready.unwrap(), Preparation::Ready);
        link.commit_attempt().unwrap();
        let mut t = eval::Tally::default();
        t.counts.perfect = 1;
        t.counts.poor = 2;
        t.empty_poor = 1;
        t.ex_score = 2;
        t.max_combo = 1;
        play_round(&link, &chart, t);
        let offered = carried_attempt(&screens, &link, &chart, CARRY_KEY, false);
        let score = offered.and_then(|o| o.0).unwrap();
        assert_eq!(
            (score.ex_score, score.rate_current_bp, score.rate_whole_bp),
            (2, 5_000, 2_500)
        );
        link.exit();
    }

    /// A render thread on new installers with `chart` loaded and a play
    /// skin chosen: the installers, the link and the skin.
    fn carry_link(chart: &Arc<Chart>) -> (Installers, super::RenderLink, Arc<render::SkinBundle>) {
        let screens = Installers::default();
        let skin = bundle();
        let link = screens.spawn(chart, &skin);
        (screens, link, skin)
    }

    /// Prepares an attempt of play state `key` on `link` with the skin in
    /// use and commits it; the request's offer.
    fn carried_attempt(
        screens: &Installers,
        link: &super::RenderLink,
        chart: &Chart,
        key: crate::carry::CarryKey,
        commit: bool,
    ) -> CarryOffered {
        let ready = link.prepare_attempt(titled(chart).with_carry_key(Some(key)), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);
        if commit {
            link.commit_attempt().unwrap();
        }
        screens.offers.lock().unwrap().last().unwrap().1.clone()
    }

    /// Plays the committed attempt as the game does: its start, its play,
    /// its end and its settlement with the final tally `tally`.
    fn play_round(link: &super::RenderLink, chart: &Chart, tally: eval::Tally) {
        start_round(link, chart);
        let _ = link.finish(QpcClock::start().now());
        link.settled(tally);
        // Answered after the settlement is taken.
        link.prepare().unwrap();
    }

    /// Starts the committed attempt's play as the game does (its logic
    /// over a chart with a note, which only feeds the frames).
    fn start_round(link: &super::RenderLink, _chart: &Chart) {
        let (rule, device) = (rules::tapestry(), DeviceProfile::default());
        let chart = late_tap();
        let (logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            Vec::new(),
            LogicConfig {
                inputs: 0,
                effects: 16,
            },
            Vec::new(),
        )
        .unwrap();
        let clock = QpcClock::start();
        link.send(RenderMsg::Play {
            state_origin: TimeUs(0),
            side,
            gauge: 0,
            measure: play_measure(clock.now()),
        });
        drop(logic);
    }

    /// The slots a round leaves: the latch set at 7 s.
    fn latched() -> Vec<skin_model::present::Val> {
        vec![skin_model::present::Val::Int(7_000_000)]
    }

    /// A round witnessed at a full score, then rounds of the play state:
    /// each later attempt, committed and started or not, is offered the
    /// point and the slots the witnessed round left; one cancelled after
    /// its commit and before its start changes nothing (the cache stays
    /// 200 000, not 0).
    #[test]
    fn a_committed_attempt_cancelled_before_start_keeps_the_cache() {
        let chart = four_taps();
        let (screens, link, _) = carry_link(&chart);
        *screens.round.lock().unwrap() = Some(latched());
        screens.witness.store(true, Ordering::Release);
        assert_eq!(
            carried_attempt(&screens, &link, &chart, CARRY_KEY, true),
            None
        );
        play_round(&link, &chart, full_tally());
        let carried = Some((FULL, Some(latched())));
        // Committed, never started, then given up.
        *screens.round.lock().unwrap() = None;
        assert_eq!(
            carried_attempt(&screens, &link, &chart, CARRY_KEY, true),
            carried
        );
        // The next is offered the same.
        assert_eq!(
            carried_attempt(&screens, &link, &chart, CARRY_KEY, false),
            carried
        );
        link.exit();
    }

    /// A round whose last build did not run its slot program on a complete
    /// view (a judgment after the last frame, then an end) settles the
    /// point from the final tally and leaves no valid slots.
    #[test]
    fn a_judgment_after_the_last_frame_settles_the_point() {
        let chart = four_taps();
        let (screens, link, _) = carry_link(&chart);
        *screens.round.lock().unwrap() = Some(latched());
        let _ = carried_attempt(&screens, &link, &chart, CARRY_KEY, true);
        play_round(&link, &chart, full_tally());
        assert_eq!(
            carried_attempt(&screens, &link, &chart, CARRY_KEY, false),
            Some((FULL, None))
        );
        link.exit();
    }

    /// Validity is transitive: an unwitnessed round, then a round run
    /// without its slot program that completes witnessed, leaves the
    /// third round no valid slots, never round 1's latch; witnessed
    /// rounds carry it to the third (the control). The point follows each
    /// round's final tally.
    #[test]
    fn an_unwitnessed_round_invalidates_the_play_state() {
        for witnessed_first in [false, true] {
            let chart = four_taps();
            let (screens, link, _) = carry_link(&chart);
            *screens.round.lock().unwrap() = Some(latched());
            screens.witness.store(witnessed_first, Ordering::Release);
            let _ = carried_attempt(&screens, &link, &chart, CARRY_KEY, true);
            play_round(&link, &chart, full_tally());
            screens.witness.store(true, Ordering::Release);
            *screens.round.lock().unwrap() = None;
            let _ = carried_attempt(&screens, &link, &chart, CARRY_KEY, true);
            play_round(&link, &chart, poor_tally());
            let third = carried_attempt(&screens, &link, &chart, CARRY_KEY, false);
            // The score data carries whatever the slots: round 2's.
            let slots = witnessed_first.then(latched);
            assert_eq!(
                third,
                Some((POOR, slots)),
                "witnessed first: {witnessed_first}"
            );
            link.exit();
        }
    }

    /// A preparation discarded after its answer changes nothing: the next
    /// attempt is offered what the witnessed round left.
    #[test]
    fn a_discarded_stage_changes_nothing() {
        let chart = four_taps();
        let (screens, link, _) = carry_link(&chart);
        *screens.round.lock().unwrap() = Some(latched());
        screens.witness.store(true, Ordering::Release);
        let _ = carried_attempt(&screens, &link, &chart, CARRY_KEY, true);
        play_round(&link, &chart, full_tally());
        let _ = carried_attempt(&screens, &link, &chart, CARRY_KEY, false);
        link.discard_attempt();
        assert_eq!(
            carried_attempt(&screens, &link, &chart, CARRY_KEY, false),
            Some((FULL, Some(latched())))
        );
        link.exit();
    }

    /// The committed play attempt is told apart from other kinds' commits:
    /// a play commit, then the result's upload committed before the play
    /// starts, then the play and its settlement keep the witnessed slots.
    #[test]
    fn a_result_upload_keeps_the_committed_attempt() {
        let chart = four_taps();
        let (screens, link, _) = carry_link(&chart);
        *screens.round.lock().unwrap() = Some(latched());
        screens.witness.store(true, Ordering::Release);
        let _ = carried_attempt(&screens, &link, &chart, CARRY_KEY, true);
        let upload = link.upload_result(crate::result_skin::Upload::Bundle(bundle()));
        assert_eq!(upload.wait().unwrap(), Preparation::Ready);
        play_round(&link, &chart, full_tally());
        assert_eq!(
            carried_attempt(&screens, &link, &chart, CARRY_KEY, false),
            Some((FULL, Some(latched())))
        );
        link.exit();
    }

    /// A play installation committed in the play state invalidates the
    /// slots of the one before, and returning to that very installation
    /// does not bring them back (A, B, A with no attempt between); a
    /// refused replacement keeps them. The point stays either way.
    #[test]
    fn another_installation_invalidates_the_slots() {
        for refused in [false, true] {
            let chart = four_taps();
            let (screens, link, a) = carry_link(&chart);
            *screens.round.lock().unwrap() = Some(latched());
            screens.witness.store(true, Ordering::Release);
            let _ = carried_attempt(&screens, &link, &chart, CARRY_KEY, true);
            play_round(&link, &chart, full_tally());
            screens.refuse_install.store(refused, Ordering::Release);
            let b = link
                .select(ScreenKind::Play, Some(bundle()))
                .wait()
                .unwrap();
            assert_eq!(b == Preparation::Ready, !refused);
            screens.refuse_install.store(false, Ordering::Release);
            if !refused {
                let back = link.select(ScreenKind::Play, Some(Arc::clone(&a)));
                assert_eq!(back.wait().unwrap(), Preparation::Ready);
            }
            let slots = refused.then(latched);
            assert_eq!(
                carried_attempt(&screens, &link, &chart, CARRY_KEY, false),
                Some((FULL, slots)),
                "refused: {refused}"
            );
            link.exit();
        }
    }

    /// The carry follows its play state: a chart's load (a practice
    /// page's Start) keeps it for the play state, and an attempt of
    /// another play state (practice left and entered again, a retry
    /// outside practice) starts fresh.
    #[test]
    fn the_carry_follows_its_play_state() {
        let chart = four_taps();
        let (screens, link, _) = carry_link(&chart);
        *screens.round.lock().unwrap() = Some(latched());
        screens.witness.store(true, Ordering::Release);
        let _ = carried_attempt(&screens, &link, &chart, CARRY_KEY, true);
        play_round(&link, &chart, full_tally());
        link.load(Arc::clone(&chart), None).unwrap();
        assert_eq!(
            carried_attempt(&screens, &link, &chart, CARRY_KEY, false),
            Some((FULL, Some(latched())))
        );
        let next = crate::carry::CarryKey {
            play_state: 2,
            ..CARRY_KEY
        };
        assert_eq!(carried_attempt(&screens, &link, &chart, next, false), None);
        link.exit();
    }

    /// A device lost during a round takes the committed attempt's `Play`:
    /// its settlement leaves no valid slots, and the point is the final
    /// tally's.
    #[test]
    fn a_device_lost_in_a_round_invalidates_the_slots() {
        let chart = four_taps();
        let (screens, link, _) = carry_link(&chart);
        *screens.round.lock().unwrap() = Some(latched());
        screens.witness.store(true, Ordering::Release);
        let _ = carried_attempt(&screens, &link, &chart, CARRY_KEY, true);
        start_round(&link, &chart);
        // A frame fails as a lost device: the screen and its `Play` go.
        screens.fail.store(true, Ordering::Release);
        assert!(wait_until(|| link.failed()));
        screens.fail.store(false, Ordering::Release);
        let _ = link.finish(QpcClock::start().now());
        link.settled(full_tally());
        link.prepare().unwrap();
        assert_eq!(
            carried_attempt(&screens, &link, &chart, CARRY_KEY, false),
            Some((FULL, None))
        );
        link.exit();
    }

    /// Loads a chart on `link` and prepares an attempt with the skin in
    /// use, which must be refused: the reason.
    pub(crate) fn in_use_attempt(link: &super::RenderLink) -> String {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        link.load(Arc::clone(&chart), None).unwrap();
        match link.prepare_attempt(titled(&chart), || false).unwrap() {
            Preparation::Refused(why) => why.reason,
            other => panic!("{other:?}"),
        }
    }

    /// A render thread on new installers with a chart loaded and play's
    /// skin marked pending.
    pub(crate) fn pending_link() -> (Arc<Chart>, Installers, super::RenderLink) {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let screens = Installers::default();
        let link = screens.spawn_bare();
        link.load(Arc::clone(&chart), None).unwrap();
        assert_eq!(
            link.pend(ScreenKind::Play).wait().unwrap(),
            Preparation::Ready
        );
        (chart, screens, link)
    }

    /// Prepares an attempt of `chart` on `link` with the specialization
    /// `skin`.
    pub(crate) fn specialized(
        link: &super::RenderLink,
        chart: &Chart,
        skin: &Arc<render::SkinBundle>,
    ) -> Result<Preparation, ScreenError> {
        link.prepare_attempt_with(
            titled(chart),
            super::AttemptSkin::Specialized(Arc::clone(skin)),
            || false,
        )
    }

    /// Whether the screen's installed play bundle is `skin`, the very one.
    pub(crate) fn installed_is(screens: &Installers, skin: &Arc<render::SkinBundle>) -> bool {
        screens.play().is_some_and(|p| Arc::ptr_eq(&p, skin))
    }

    /// A pending play skin's specialization is staged with its attempt
    /// and installed only by that attempt's commit. Each way an attempt
    /// is discarded before its commit (the game's discard on a quit or a
    /// failed restore check, the answer's bound, a newer request, the
    /// screen built again) drops its staging, and the specialization
    /// committed before stays installed, the very bundle; one committed
    /// stays through a later cancellation.
    #[test]
    fn a_specialization_installs_only_by_its_commit() {
        let (chart, screens, mut link) = pending_link();
        let first = bundle();
        assert_eq!(
            specialized(&link, &chart, &first).unwrap(),
            Preparation::Ready
        );
        assert!(screens.play().is_none(), "staged, not installed");
        link.commit_attempt().unwrap();
        assert!(installed_is(&screens, &first));

        // Discarded by the game (a quit, a failed restore check, a
        // thread's failure before the commit).
        let other = bundle();
        assert_eq!(
            specialized(&link, &chart, &other).unwrap(),
            Preparation::Ready
        );
        link.discard_attempt();
        assert!(matches!(link.commit_attempt(), Err(ScreenError::Lost)));
        assert!(installed_is(&screens, &first));

        // Past the answer's bound.
        link.set_timeouts(Duration::from_millis(100), Duration::from_secs(5));
        screens.hold.store(true, Ordering::Release);
        let late = specialized(&link, &chart, &other);
        assert!(matches!(late, Err(ScreenError::NoAnswer)), "{late:?}");
        screens.hold.store(false, Ordering::Release);
        link.set_timeouts(Duration::from_secs(10), Duration::from_secs(5));
        assert!(matches!(link.commit_attempt(), Err(ScreenError::Lost)));
        assert!(installed_is(&screens, &first));

        // Superseded by a newer request.
        assert_eq!(
            specialized(&link, &chart, &other).unwrap(),
            Preparation::Ready
        );
        assert_eq!(
            specialized(&link, &chart, &first).unwrap(),
            Preparation::Ready
        );
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        link.send(RenderMsg::Commit {
            request: link.requests.load(Ordering::Relaxed) - 1,
            reply,
        });
        assert!(rx.recv().unwrap().is_err());
        link.discard_attempt();
        assert!(installed_is(&screens, &first));

        // The screen built again before the commit: the committed one is
        // restored, and the staging is gone with the old screen.
        assert_eq!(
            specialized(&link, &chart, &other).unwrap(),
            Preparation::Ready
        );
        rebuild_screen(&link, &screens);
        assert!(matches!(link.commit_attempt(), Err(ScreenError::Lost)));
        assert!(installed_is(&screens, &first));

        // Committed, then a later attempt cancelled: it stays.
        assert_eq!(
            specialized(&link, &chart, &other).unwrap(),
            Preparation::Ready
        );
        link.commit_attempt().unwrap();
        assert!(installed_is(&screens, &other));
        let cancelled = link.prepare_attempt_with(
            titled(&chart),
            super::AttemptSkin::Specialized(Arc::clone(&first)),
            || true,
        );
        // Answered at once or cancelled: either way discarded.
        assert!(cancelled.is_ok(), "{cancelled:?}");
        link.discard_attempt();
        assert!(installed_is(&screens, &other));
        link.exit();
    }

    /// While play's skin is pending, an attempt with the skin in use is
    /// refused without the screen asked to prepare it; and it still
    /// supersedes an older request's staged specialization, which is
    /// discarded, the committed one kept.
    #[test]
    fn an_in_use_attempt_is_refused_while_pending() {
        let (chart, screens, link) = pending_link();
        let first = bundle();
        assert_eq!(
            specialized(&link, &chart, &first).unwrap(),
            Preparation::Ready
        );
        link.commit_attempt().unwrap();
        assert_eq!(
            specialized(&link, &chart, &bundle()).unwrap(),
            Preparation::Ready
        );
        let before = screens.log().len();
        let answer = link.prepare_attempt(titled(&chart), || false).unwrap();
        let Preparation::Refused(why) = answer else {
            panic!("{answer:?}");
        };
        assert_eq!(why.reason, "the play skin awaits its specialization");
        assert_eq!(screens.log()[before..], ["1 discard"]);
        assert!(matches!(link.commit_attempt(), Err(ScreenError::Lost)));
        assert!(installed_is(&screens, &first));
        link.exit();
    }

    /// A preparation that staged its specialization and has not answered
    /// is superseded by the next request whatever it is: it is answered
    /// refused, and its staging discarded, before the next one, an
    /// attempt with the skin in use (refused while pending) or another
    /// specialization (prepared and committed), so nothing of it stays
    /// staged or installs.
    #[test]
    fn a_newer_request_discards_an_unanswered_staging() {
        for next_in_use in [true, false] {
            let (chart, screens, link) = pending_link();
            let (held, next) = (bundle(), bundle());
            let request = link.requests.fetch_add(1, Ordering::Relaxed) + 1;
            // Held through the successor: it never answers by itself.
            screens.hold_staged.store(request, Ordering::Release);
            let (reply, rx) = std::sync::mpsc::sync_channel(1);
            link.send(RenderMsg::PrepareAttempt {
                request,
                attempt: Box::new(titled(&chart)),
                skin: super::AttemptSkin::Specialized(Arc::clone(&held)),
                reply,
            });
            assert!(wait_until(|| screens.staging.lock().unwrap().is_some()));
            if next_in_use {
                let answer = link.prepare_attempt(titled(&chart), || false).unwrap();
                let Preparation::Refused(why) = answer else {
                    panic!("{answer:?}");
                };
                assert_eq!(why.reason, "the play skin awaits its specialization");
            } else {
                assert_eq!(
                    specialized(&link, &chart, &next).unwrap(),
                    Preparation::Ready
                );
            }
            let older = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
            let Preparation::Refused(why) = older.preparation else {
                panic!("{older:?}");
            };
            assert_eq!(why.reason, "a newer request superseded it");
            // Nothing of the held one stays staged.
            let staged = screens.staging.lock().unwrap().clone();
            if next_in_use {
                assert!(staged.is_none());
            } else {
                assert!(staged.is_some_and(|s| Arc::ptr_eq(&s, &next)));
                link.commit_attempt().unwrap();
                assert!(installed_is(&screens, &next));
                assert!(screens.staging.lock().unwrap().is_none());
            }
            screens.hold_staged.store(0, Ordering::Release);
            if next_in_use {
                assert!(screens.play().is_none());
            }
            let log = screens.log();
            let staged = log
                .iter()
                .position(|l| l == &format!("1 prepare {request} Right T"))
                .unwrap();
            assert_eq!(log[staged + 1], "1 discard", "{log:?}");
            link.exit();
        }
    }

    /// Loading frames of a lifetime draw the pending presentation until a
    /// specialization is committed in that lifetime, then the skin's own;
    /// a paused lifetime resumed (the start gate) draws the skin's own
    /// again; the next lifetime is pending again until its own commit.
    #[test]
    fn the_pending_presentation_lasts_until_a_commit_in_its_lifetime() {
        let (chart, screens, link) = pending_link();
        let pending = || screens.pending.load(Ordering::Acquire);
        let skin = || screens.loading.load(Ordering::Acquire);

        // Lifetime 1: pending, then the skin's after its commit.
        link.loading().show(one_line("one"));
        assert!(wait_until(|| pending() >= 3));
        assert_eq!(skin(), 0);
        assert_eq!(
            specialized(&link, &chart, &bundle()).unwrap(),
            Preparation::Ready
        );
        link.commit_attempt().unwrap();
        let from = pending();
        assert!(wait_until(|| skin() >= 3));
        let after = pending();
        assert!(after <= from + 1, "{from} {after}");

        // Paused and resumed: the skin's again.
        link.pause_loading().unwrap();
        let paused = skin();
        link.resume_loading();
        assert!(wait_until(|| skin() >= paused + 3));
        link.end_loading().unwrap();

        // Lifetime 2: pending again, though a specialization is kept.
        let (p, s) = (pending(), skin());
        link.loading().show(one_line("two"));
        assert!(wait_until(|| pending() >= p + 3));
        assert_eq!(skin(), s);
        // Its own commit draws the skin's.
        assert_eq!(
            specialized(&link, &chart, &bundle()).unwrap(),
            Preparation::Ready
        );
        link.commit_attempt().unwrap();
        assert!(wait_until(|| skin() >= s + 3));
        link.end_loading().unwrap();
        link.exit();
    }

    /// L5: a pending play skin's base import stops at the fact it reads,
    /// inside the configured pass and before the skin's table exists, so
    /// it declares no loading time: until a specialization commits in the
    /// lifetime the minimum is the floor, a second from the first frame;
    /// a specialization declaring 3 000 ms then moves it to 3 s from that
    /// same frame.
    #[test]
    fn a_pending_skin_uses_the_floor_until_its_specialization_commits() {
        let (chart, _screens, link) = pending_link();
        link.loading().show(one_line("one"));
        assert!(wait_until(|| link.loading_acked() >= 2));
        let (lifetime, at) = link.loading_ack().unwrap();
        assert_eq!(link.loading_loadend(), None);
        assert_eq!(
            game::minimum_ends(at, link.loading_loadend()),
            TimeUs(at.0 + 1_000_000)
        );
        let declared = bundle_declaring(None, Some(TimeUs(3_000_000)));
        assert_eq!(
            specialized(&link, &chart, &declared).unwrap(),
            Preparation::Ready
        );
        assert_eq!(link.loading_loadend(), None, "staged, not committed");
        link.commit_attempt().unwrap();
        // Declared with the commit's answer, before a frame draws with it.
        assert_eq!(link.loading_loadend(), Some(TimeUs(3_000_000)));
        // A frame acknowledged after the commit's answer drew after it.
        let committed = link.loading_acked();
        assert!(wait_until(|| link.loading_acked() > committed));
        assert_eq!(link.loading_loadend(), Some(TimeUs(3_000_000)));
        assert_eq!(link.loading_ack(), Some((lifetime, at)));
        assert_eq!(
            game::minimum_ends(at, link.loading_loadend()),
            TimeUs(at.0 + 3_000_000)
        );
        link.end_loading().unwrap();
        link.exit();
    }

    /// O12a: a commit whose answer the game thread stopped waiting for,
    /// its receipt published but its loading time not yet declared by the
    /// render thread: the receipt declares it, so the minimum read after
    /// the commit is the specialization's.
    #[test]
    fn a_timed_out_commit_declares_its_loading_time_from_the_receipt() {
        let (chart, _screens, mut link) = pending_link();
        link.loading().show(one_line("one"));
        assert!(wait_until(|| link.loading_acked() >= 2));
        let declared = bundle_declaring(None, Some(TimeUs(3_000_000)));
        assert_eq!(
            specialized(&link, &chart, &declared).unwrap(),
            Preparation::Ready
        );
        /// Lets the render thread go when dropped, the test failing
        /// included.
        struct Hold(Arc<super::Shared>);
        impl Drop for Hold {
            fn drop(&mut self) {
                self.0.hold_declaration.store(false, Ordering::Release);
            }
        }
        link.set_timeouts(Duration::from_millis(100), Duration::from_secs(5));
        link.shared.commit_timed_out.store(false, Ordering::Release);
        link.shared.declaration_held.store(false, Ordering::Release);
        link.shared.hold_declaration.store(true, Ordering::Release);
        let hold = Hold(Arc::clone(&link.shared));
        link.commit_attempt().unwrap();
        // Read past the bound, with the render thread held between the
        // receipt and its own declaration: only the receipt declared it.
        assert!(link.shared.commit_timed_out.load(Ordering::Acquire));
        assert!(link.shared.declaration_held.load(Ordering::Acquire));
        assert_eq!(link.loading_loadend(), Some(TimeUs(3_000_000)));
        // The render thread still held its declaration when the receipt
        // was read: a stall past the hook's bound would let the render
        // thread declare it itself and weaken this oracle.
        assert!(!link.shared.declaration_expired.load(Ordering::Acquire));
        drop(hold);
        link.set_timeouts(Duration::from_secs(10), Duration::from_secs(5));
        link.end_loading().unwrap();
        link.exit();
    }

    /// A loading frame's stamp says which presentation it drew: not settled
    /// on the pending placeholder, settled on the skin's after a commit in
    /// its lifetime.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_loading_stamp_says_which_branch_drew() {
        struct Stamps(Arc<std::sync::Mutex<Vec<bool>>>);
        impl super::FrameSink for Stamps {
            fn frame(&mut self, _screen: &dyn Screen, drawn: &super::Drawn<'_>) {
                if let Some(l) = drawn.loading_stamp {
                    self.0.lock().unwrap().push(l.settled);
                }
            }
        }
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let screens = Installers::default();
        let stamps = Arc::new(std::sync::Mutex::new(Vec::new()));
        let link = screens.spawn_with(Box::new(Stamps(Arc::clone(&stamps))));
        link.load(Arc::clone(&chart), None).unwrap();
        assert_eq!(
            link.pend(ScreenKind::Play).wait().unwrap(),
            Preparation::Ready
        );
        let (pending, skin) = (&screens.pending, &screens.loading);
        link.loading().show(one_line("one"));
        assert!(wait_until(|| pending.load(Ordering::Acquire) >= 3));
        assert_eq!(skin.load(Ordering::Acquire), 0);
        // The sink runs after each frame: a frame in flight may not be in yet.
        assert!(wait_until(|| stamps.lock().unwrap().len() >= 2));
        assert!(stamps.lock().unwrap().iter().all(|settled| !settled));
        assert_eq!(
            specialized(&link, &chart, &bundle()).unwrap(),
            Preparation::Ready
        );
        link.commit_attempt().unwrap();
        assert!(wait_until(|| skin.load(Ordering::Acquire) >= 3));
        assert!(wait_until(|| stamps
            .lock()
            .unwrap()
            .last()
            .is_some_and(|settled| *settled)));
        link.end_loading().unwrap();
        link.exit();
    }

    /// A loading frame's play level is its lifetime's chosen chart's: the
    /// published state's, none before a publication, and none in a next
    /// lifetime whose chart has no entry, whatever came before.
    #[test]
    fn a_loading_frame_reads_only_its_lifetimes_play_level() {
        let update = |lifetime, play_level| {
            let text = LoadingText {
                chart: ChosenChart {
                    play_level,
                    ..ChosenChart::default()
                },
                ..LoadingText::default()
            };
            crate::loading_state::LifetimeState::new(lifetime, TimeUs(0), &text, None)
                .apply(LoadInput::Stage(Stage::Chart))
        };
        let level = |lifetime, u: Option<&LoadingUpdate>| {
            super::loading_frame_of(
                lifetime,
                TimeUs(1_000),
                u,
                None,
                0,
                skin_model::AppReadouts::LAUNCH,
            )
            .play_level
        };
        assert_eq!(level(1, None), None);
        assert_eq!(level(1, Some(&update(1, Some(12)))), Some(12));
        assert_eq!(level(2, Some(&update(2, None))), None);
    }

    /// A loading frame's difficulty and BGA flag are its lifetime's chosen
    /// chart's in the same way: none before a publication, none for a
    /// chart without an entry.
    #[test]
    fn a_loading_frame_reads_its_lifetimes_difficulty_and_bga() {
        let update = |lifetime, difficulty, bga| {
            let text = LoadingText {
                chart: ChosenChart {
                    difficulty,
                    bga,
                    ..ChosenChart::default()
                },
                ..LoadingText::default()
            };
            crate::loading_state::LifetimeState::new(lifetime, TimeUs(0), &text, None)
                .apply(LoadInput::Stage(Stage::Chart))
        };
        let read = |lifetime, u: Option<&LoadingUpdate>| {
            let f = super::loading_frame_of(
                lifetime,
                TimeUs(1_000),
                u,
                None,
                0,
                skin_model::AppReadouts::LAUNCH,
            );
            (f.difficulty, f.bga)
        };
        assert_eq!(read(1, None), (None, None));
        assert_eq!(
            read(1, Some(&update(1, Some(4), Some(true)))),
            (Some(4), Some(true))
        );
        assert_eq!(read(2, Some(&update(2, None, None))), (None, None));
    }

    /// A loading frame's chart length is its lifetime's chosen chart's
    /// library length in the same way: none before a publication, none
    /// for a chart without one, and the model's frame carries it.
    #[test]
    fn a_loading_frame_reads_its_lifetimes_length() {
        let update = |lifetime, length_us| {
            let text = LoadingText {
                chart: ChosenChart {
                    length_us,
                    ..ChosenChart::default()
                },
                ..LoadingText::default()
            };
            crate::loading_state::LifetimeState::new(lifetime, TimeUs(0), &text, None)
                .apply(LoadInput::Stage(Stage::Chart))
        };
        let read = |lifetime, u: Option<&LoadingUpdate>| {
            let f = super::loading_frame_of(
                lifetime,
                TimeUs(1_000),
                u,
                None,
                0,
                skin_model::AppReadouts::LAUNCH,
            );
            assert_eq!(f.model().length, f.length);
            f.length
        };
        let l = TimeUs(125_400_000);
        assert_eq!(read(1, None), None);
        assert_eq!(read(1, Some(&update(1, Some(l)))), Some(l));
        assert_eq!(read(2, Some(&update(2, None))), None);
    }

    /// Through the model's loading frame, an imported skin draws the
    /// difficulty's and the BGA flag's conditions from the chosen chart,
    /// none without it, and never a class condition, its negation or the
    /// song total, which loading lacks.
    #[test]
    fn a_loading_frame_draws_the_chosen_charts_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let (skin, uvs) = crate::attempt_context::meta_tests::skin(dir.path());
        let mut loading = skin.prepare_loading(skin_model::Side::Left);
        let mut batch =
            skin_model::QuadBatch::with_capacity(skin.quad_capacity(), skin.run_capacity());
        let mut drawn = |chart: Option<ChosenChart>| {
            let update = chart.map(|chart| {
                let text = LoadingText {
                    chart,
                    ..LoadingText::default()
                };
                crate::loading_state::LifetimeState::new(1, TimeUs(0), &text, None)
                    .apply(LoadInput::Stage(Stage::Chart))
            });
            let frame = super::loading_frame_of(
                1,
                TimeUs(1_000),
                update.as_ref(),
                None,
                0,
                skin_model::AppReadouts::LAUNCH,
            );
            batch.clear();
            skin.build_loading(&mut loading, &frame.model(), &uvs, &mut batch)
                .unwrap();
            let mut w: Vec<i32> = batch
                .quads()
                .iter()
                .map(|q| {
                    #[expect(clippy::cast_possible_truncation, reason = "test rects are small")]
                    let w = q.rect[2].round() as i32;
                    w
                })
                .collect();
            w.sort_unstable();
            w
        };
        assert_eq!(drawn(None), [] as [i32; 0]);
        let chosen = |difficulty, bga| ChosenChart {
            difficulty,
            bga,
            ..ChosenChart::default()
        };
        assert_eq!(drawn(Some(chosen(Some(3), Some(false)))), [13, 20]);
        assert_eq!(drawn(Some(chosen(Some(0), Some(true)))), [10, 21]);
        assert_eq!(drawn(Some(chosen(None, None))), [] as [i32; 0]);
    }

    /// Reading the chosen chart's difficulty and BGA flag into a loading
    /// frame allocates nothing, with and without them, after a positive
    /// control proving the counter counts.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_loading_frames_chosen_metadata_alloc() {
        use crate::alloc_count::this_thread;
        let update = |difficulty, bga| {
            let text = LoadingText {
                chart: ChosenChart {
                    difficulty,
                    bga,
                    ..ChosenChart::default()
                },
                ..LoadingText::default()
            };
            crate::loading_state::LifetimeState::new(1, TimeUs(0), &text, None)
                .apply(LoadInput::Stage(Stage::Chart))
        };
        let updates = [
            update(Some(4), Some(true)),
            update(Some(0), Some(false)),
            update(None, None),
        ];
        let control = this_thread();
        drop(std::hint::black_box(vec![1_u8; 8]));
        assert!(this_thread() > control, "the allocation was not counted");
        let before = this_thread();
        let mut read = [(None, None); 4];
        for (r, u) in read.iter_mut().zip(updates.iter().map(Some).chain([None])) {
            let f = super::loading_frame_of(
                1,
                TimeUs(1_000),
                u,
                None,
                0,
                skin_model::AppReadouts::LAUNCH,
            );
            *r = std::hint::black_box((f.difficulty, f.bga));
        }
        assert_eq!(this_thread(), before, "a loading frame allocated");
        assert_eq!(
            read,
            [
                (Some(4), Some(true)),
                (Some(0), Some(false)),
                (None, None),
                (None, None)
            ]
        );
    }

    /// A loading frame says whether the play its lifetime loads is
    /// autoplay, as its text gave it, down to the model's frame; with no
    /// publication yet, not.
    #[test]
    fn a_loading_frame_carries_its_lifetimes_autoplay() {
        let update = |autoplay| {
            let text = LoadingText {
                autoplay,
                ..LoadingText::default()
            };
            crate::loading_state::LifetimeState::new(1, TimeUs(0), &text, None)
                .apply(LoadInput::Stage(Stage::Chart))
        };
        let autoplay = |u: Option<&LoadingUpdate>| {
            super::loading_frame_of(
                1,
                TimeUs(1_000),
                u,
                None,
                0,
                skin_model::AppReadouts::LAUNCH,
            )
            .model()
            .autoplay
        };
        assert!(!autoplay(None));
        assert!(autoplay(Some(&update(true))));
        assert!(!autoplay(Some(&update(false))));
    }

    /// C2, C3: a loading frame carries the adjustments' newest display
    /// offset and hi-speed fix down to the model's frame, the offset as the
    /// selection screen reads the same tuning (its ms truncated toward
    /// zero); with no snapshot, no offset and Classic.
    #[test]
    fn a_loading_frame_carries_the_snapshots_settings() {
        use game::adjustment::PresentationSnapshot;
        let hispeed = game::HiSpeedSettings {
            reference: game::HiSpeedReference::Main,
            ..game::HiSpeedSettings::default()
        };
        let snapshot = |display_offset_us| PresentationSnapshot {
            options: game::PlayOptions::new(hispeed, game::Covers::default()).unwrap(),
            hispeed: None,
            target: None,
            display_offset_us,
            adjusting: false,
            generation: 0,
            sequence: 1,
            epoch: 0,
            scratch: game::ScratchInput::NONE,
        };
        let settings = skin_model::SelectionSettings {
            display_offset_ms: 0,
            master_volume: skin_model::MasterVolume::of_gain(0.0),
            speed: skin_model::SpeedView::Classic,
            order: skin_model::SelectionOrder::Title,
            gauge: None,
            covers_on: [false; 3],
        };
        for (us, ms) in [
            (-15_600, -15),
            (-15_000, -15),
            (0, 0),
            (1_500, 1),
            (15_000, 15),
        ] {
            let f = super::loading_frame_of(
                1,
                TimeUs(1_000),
                None,
                Some(&snapshot(us)),
                0,
                skin_model::AppReadouts::LAUNCH,
            )
            .model();
            assert_eq!((f.display_offset, f.fix_hispeed), (TimeUs(us), 3), "{us}");
            let tuning = crate::tuning::LiveTuning::new(game::Tuning {
                volume_percent: 100,
                display_offset_us: us,
                input_offset_us: 0,
            });
            let shown = crate::selection_skin::live_settings(&settings, &tuning);
            assert_eq!(shown.display_offset_ms, ms, "{us}");
            assert_eq!(f.display_offset.0 / 1_000, i64::from(ms), "{us}");
        }
        let f = super::loading_frame_of(
            1,
            TimeUs(1_000),
            None,
            None,
            0,
            skin_model::AppReadouts::LAUNCH,
        )
        .model();
        assert_eq!((f.display_offset, f.fix_hispeed), (TimeUs(0), 0));
    }

    /// Counts the frames the render thread finished.
    struct Frames(Arc<std::sync::atomic::AtomicU32>);

    impl super::FrameSink for Frames {
        fn frame(&mut self, _screen: &dyn Screen, _drawn: &super::Drawn<'_>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    /// A direct launch's wait for a pending skin's specialization, on the
    /// screen a direct launch makes: the page is drawn, frame after frame,
    /// on the one screen built, nothing failing.
    #[test]
    fn a_direct_launch_draws_the_skin_wait_on_its_screen() {
        let builds = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let built = Arc::clone(&builds);
        let make: MakeScreen = Box::new(move || {
            built.fetch_add(1, Ordering::SeqCst);
            let target = render::Target::Offscreen {
                width: 1_280,
                height: 720,
            };
            render::Renderer::new(target, render::AdapterChoice::Warp)
                .map(|r| Box::new(crate::menu_screen::MenuScreen::new(r, None)) as Box<dyn Screen>)
        });
        let frames = Arc::new(std::sync::atomic::AtomicU32::new(0));
        let clock = QpcClock::start();
        let link = spawn(
            make,
            None,
            None,
            clock,
            view(),
            measure(clock),
            Box::new(Frames(Arc::clone(&frames))),
        )
        .unwrap();
        link.send(RenderMsg::Menu(Box::new(
            crate::loading_text::skin_wait_page(),
        )));
        let drawn = wait_until(|| frames.load(Ordering::SeqCst) >= 3 || link.failed());
        assert!(drawn);
        assert!(!link.failed());
        assert_eq!(builds.load(Ordering::SeqCst), 1);
        link.exit();
    }

    /// A screen built again keeps the pending marker: a specialization
    /// committed is restored, the very bundle; one whose restore is
    /// refused on a healthy device leaves only the marker, the build
    /// succeeding and the next attempt staging one again; with only the
    /// marker nothing is restored, and an attempt without a
    /// specialization is still refused.
    #[test]
    fn recovery_keeps_the_pending_marker() {
        let (chart, screens, link) = pending_link();
        // Only the marker: nothing restored.
        rebuild(&screens, &link).unwrap();
        assert!(restored(&screens).is_empty());

        let first = bundle();
        assert_eq!(
            specialized(&link, &chart, &first).unwrap(),
            Preparation::Ready
        );
        link.commit_attempt().unwrap();
        rebuild(&screens, &link).unwrap();
        let got = restored(&screens);
        let [(ScreenKind::Play, kept)] = &got[..] else {
            panic!("{got:?}");
        };
        assert!(Arc::ptr_eq(kept, &first));

        // Refused on a healthy device: the build goes on, the marker kept.
        *screens.refuse_restore.lock().unwrap() = Some(ScreenKind::Play);
        rebuild(&screens, &link).unwrap();
        *screens.refuse_restore.lock().unwrap() = None;
        rebuild(&screens, &link).unwrap();
        assert!(restored(&screens).is_empty(), "{:?}", restored(&screens));
        assert_eq!(
            in_use_attempt(&link),
            "the play skin awaits its specialization"
        );
        let next = bundle();
        assert_eq!(
            specialized(&link, &chart, &next).unwrap(),
            Preparation::Ready
        );
        link.commit_attempt().unwrap();
        assert!(installed_is(&screens, &next));

        // A failure restoring it fails the build, and the next restores it.
        *screens.fail_restore.lock().unwrap() = Some(ScreenKind::Play);
        assert!(rebuild(&screens, &link).is_err());
        *screens.fail_restore.lock().unwrap() = None;
        link.prepare().unwrap();
        assert!(installed_is(&screens, &next));
        link.exit();
    }

    /// The pending marker is a selection: withdrawn while the render
    /// thread stalls (the game thread stopped waiting), it is answered
    /// cancelled and changes nothing: play's installed skin stays and
    /// still prepares attempts with the skin in use, no request is
    /// counted committed, and loading frames stay the skin's own. Only
    /// play's skin can be pending.
    #[test]
    fn a_withdrawn_pend_changes_nothing() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let skin = bundle();
        let screens = Installers::default();
        let link = screens.spawn(&chart, &skin);
        stall_render(&link, &screens);
        let mut pending = link.pend(ScreenKind::Play);
        let (_, apart) = std::sync::mpsc::sync_channel(1);
        let rx = std::mem::replace(&mut pending.rx, apart);
        drop(pending);
        screens.stall.store(false, Ordering::Release);
        let answer = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        assert_eq!(answer, Preparation::Cancelled);
        assert_eq!(link.shared.commits().committed, 1);
        let frames = || screens.loading.load(Ordering::Acquire);
        let from = frames();
        assert!(wait_until(|| frames() >= from + 3));
        assert_eq!(screens.pending.load(Ordering::Acquire), 0);
        link.end_loading().unwrap();
        ready_attempt(&link, &chart);
        link.commit_attempt().unwrap();
        assert!(installed_is(&screens, &skin));

        let other = link.pend(ScreenKind::Result).wait().unwrap();
        assert!(matches!(other, Preparation::Refused(_)), "{other:?}");
        link.exit();
    }

    /// A ready preparation installs only by its own request's commit:
    /// prepared with the chosen bundle's scratch side and the attempt's
    /// labels, it is committed; one superseded by the next request is
    /// discarded, and a commit naming it changes nothing and leaves the
    /// newer one to commit; one discarded (refused, cancelled) cannot be
    /// committed.
    #[test]
    fn an_attempt_installs_only_by_its_own_commit() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let skin = bundle();
        let screens = Installers::default();
        let link = screens.spawn(&chart, &skin);
        let ready = |link: &super::RenderLink| link.prepare_attempt(titled(&chart), || false);

        // Request 1 was the selection's.
        assert_eq!(ready(&link).unwrap(), Preparation::Ready);
        link.commit_attempt().unwrap();
        // Request 3, superseded by 4 before its commit.
        assert_eq!(ready(&link).unwrap(), Preparation::Ready);
        assert_eq!(ready(&link).unwrap(), Preparation::Ready);
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        link.send(RenderMsg::Commit { request: 3, reply });
        assert_eq!(
            rx.recv().unwrap(),
            Err(render::CommitError::Request {
                staged: 4,
                asked: 3
            })
        );
        link.commit_attempt().unwrap();
        // Request 5, discarded.
        assert_eq!(ready(&link).unwrap(), Preparation::Ready);
        link.discard_attempt();
        assert!(matches!(link.commit_attempt(), Err(ScreenError::Lost)));
        link.exit();
        assert_eq!(
            screens.log(),
            [
                "1 install 1",
                "1 commit 1",
                "1 prepare 2 Right T",
                "1 commit 2",
                "1 prepare 3 Right T",
                "1 discard",
                "1 prepare 4 Right T",
                "1 commit 4",
                "1 prepare 5 Right T",
                "1 discard",
            ]
        );
    }

    /// A render thread on [`Installers`] with a chart loaded and a skin
    /// declaring `close` chosen: the chart, the installers and the link.
    fn receipt_link(close: Option<TimeUs>) -> (Arc<Chart>, Installers, super::RenderLink) {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let screens = Installers::default();
        let link = screens.spawn(&chart, &bundle_closing(close));
        (chart, screens, link)
    }

    /// Prepares an attempt of `chart` on `link`, answered ready.
    #[track_caller]
    fn ready_attempt(link: &super::RenderLink, chart: &Chart) {
        let ready = link.prepare_attempt(titled(chart), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);
    }

    /// E2: a final readiness found stale (the screen built again since the
    /// staging, on another device) stamps nothing: the commit is refused
    /// and the epoch committed before stays the one committed.
    #[test]
    fn a_stale_readiness_stamps_nothing() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let screens = Installers::default();
        let link = screens.spawn(&chart, &bundle());
        ready_attempt(&link, &chart);
        let first = link.requests.load(Ordering::Relaxed);
        assert_eq!(
            link.ready_attempt(first, 7).unwrap(),
            super::ReadyAnswer::Ready
        );
        link.commit_attempt().unwrap();
        assert_eq!(link.epochs().1, 7);
        ready_attempt(&link, &chart);
        let second = link.requests.load(Ordering::Relaxed);
        rebuild_screen(&link, &screens);
        assert!(matches!(
            link.ready_attempt(second, 9).unwrap(),
            super::ReadyAnswer::Stale(_)
        ));
        assert!(matches!(link.commit_attempt(), Err(ScreenError::Lost)));
        assert_eq!(link.epochs().1, 7, "nothing stamped or committed");
        link.exit();
    }

    /// E3, B1: the decision screen's page drawn, then a frame fails: the
    /// screen is lost and nothing is drawn. Built again with nothing after
    /// the page (the positive control), it draws the page again, the
    /// presentation of the state the render thread is in. A mode message
    /// taken before or after the rebuild replaces it: the loading state
    /// (sent after the rebuild, or before it), selection, or the practice
    /// page, and no decision frame is drawn after the render thread took
    /// that message.
    #[test]
    fn the_render_thread_draws_the_latest_mode_after_a_rebuild() {
        #[derive(Clone, Copy, Debug)]
        enum Next {
            Nothing,
            PrepareThenLoading,
            LoadingThenPrepare,
            Selection,
            Practice,
        }
        for next in [
            Next::Nothing,
            Next::PrepareThenLoading,
            Next::LoadingThenPrepare,
            Next::Selection,
            Next::Practice,
        ] {
            let screens = Installers::default();
            let link = screens.spawn_bare();
            let frames = |screens: &Installers| screens.menu_frames.lock().unwrap().clone();
            let decisions = |from: usize| {
                frames(&screens)[from..]
                    .iter()
                    .filter(|l| *l == "decision")
                    .count()
            };
            link.show_decision(one_line("decision"), None);
            assert!(wait_until(|| decisions(0) > 0), "{next:?}: the page drawn");
            screens.fail.store(true, Ordering::Release);
            assert!(wait_until(|| link.failed()), "{next:?}");
            screens.fail.store(false, Ordering::Release);
            assert!(link.fence(Duration::from_secs(60)));
            let lost = frames(&screens).len();
            match next {
                Next::Nothing => {
                    link.prepare().unwrap();
                    assert!(
                        wait_until(|| decisions(lost) > 0),
                        "{next:?}: the page not drawn again"
                    );
                    link.exit();
                    continue;
                }
                Next::PrepareThenLoading => {
                    link.prepare().unwrap();
                    link.loading().show(one_line("loading"));
                }
                Next::LoadingThenPrepare => {
                    link.loading().show(one_line("loading"));
                    link.prepare().unwrap();
                }
                Next::Selection => {
                    link.send(RenderMsg::SelectionChanged);
                    link.prepare().unwrap();
                    // Selection replaced the decision page before the
                    // rebuild: nothing of it is restored.
                    assert_eq!(link.mode(), "idle", "{next:?}: the mode after the rebuild");
                    link.send(RenderMsg::Menu(Box::new(one_line("select"))));
                }
                Next::Practice => {
                    link.send(RenderMsg::PracticePage(Box::new(one_line("page"))));
                    link.prepare().unwrap();
                }
            }
            // Every message above taken, and the frames after them drawn.
            assert!(link.fence(Duration::from_secs(60)));
            let taken = frames(&screens).len();
            assert!(link.fence(Duration::from_secs(60)));
            assert!(link.fence(Duration::from_secs(60)));
            let after = match next {
                // The decision page may be drawn between the rebuild and
                // the loading state: none after the render thread took it.
                Next::PrepareThenLoading => taken,
                _ => lost,
            };
            assert_eq!(
                decisions(after),
                0,
                "{next:?}: a decision frame after the mode's message: {:?}",
                &frames(&screens)[lost..]
            );
            link.exit();
        }
    }

    /// E1: the device lost while the chart is built for the screen: the
    /// screen is built again once more with the same chart, inside the
    /// load's one answer, and the load succeeds.
    #[test]
    fn a_device_loss_in_the_chart_load_is_rebuilt_with_the_chart() {
        let screens = Installers::default();
        let link = screens.spawn_bare();
        screens.lose_chart_loads.store(1, Ordering::Release);
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let at = std::ptr::from_ref(chart.as_ref()) as usize;
        link.load(Arc::clone(&chart), None).unwrap();
        assert_eq!(
            *screens.chart_loads.lock().unwrap(),
            [(at, false), (at, false)],
            "the chart built twice, the same chart"
        );
        assert!(!link.failed());
        link.exit();
    }

    /// E2 (render): the device lost again in that second build: the load's
    /// answer is the device's error, not a refusal, after exactly two
    /// builds of the chart.
    #[test]
    fn a_second_device_loss_in_the_chart_load_ends_the_run() {
        let screens = Installers::default();
        let link = screens.spawn_bare();
        screens.lose_chart_loads.store(2, Ordering::Release);
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let got = link.load(chart, None);
        assert!(
            matches!(&got, Err(ScreenError::Render(e)) if !e.is_refusal()),
            "{got:?}"
        );
        assert_eq!(screens.chart_loads.lock().unwrap().len(), 2);
        link.exit();
    }

    /// E2c: a skin's refusal of the chart, the device healthy: refused as
    /// before, after exactly one build of the chart (the screen built
    /// again without it is not a chart build).
    #[test]
    fn a_skin_refusal_in_the_chart_load_is_still_refused() {
        let screens = Installers::default();
        let link = screens.spawn_bare();
        screens.refuse_chart_loads.store(true, Ordering::Release);
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let got = link.load(chart, None);
        assert!(
            matches!(&got, Err(ScreenError::Render(e)) if e.is_refusal()),
            "{got:?}"
        );
        assert_eq!(screens.chart_loads.lock().unwrap().len(), 1, "built again");
        link.exit();
    }

    /// Fails `link`'s screen and builds it again.
    fn rebuild_screen(link: &super::RenderLink, screens: &Installers) {
        screens.fail.store(true, Ordering::Release);
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| link.failed()));
        screens.fail.store(false, Ordering::Release);
        link.prepare().unwrap();
    }

    /// Stalls `link`'s render thread in a loading frame.
    fn stall_render(link: &super::RenderLink, screens: &Installers) {
        let frames = || screens.loading.load(Ordering::Acquire);
        link.loading().show(one_line("loading"));
        assert!(wait_until(|| frames() > 0));
        screens.stall.store(true, Ordering::Release);
        assert!(wait_until(|| screens.stalled.load(Ordering::Acquire)));
    }

    /// Each installed attempt's commit gives the player its receipt, the
    /// committed skin's declared Closing duration, through the player's
    /// commit ([`crate::play::commit_ready`]): on the answer, past the
    /// answer's bound when it was installed meanwhile, again on a retry
    /// and kept while the screen is built again; a commit refused or
    /// withdrawn gives `None`, whatever the receipt before it. The
    /// longest duration a skin declares, `i32::MAX` ms, arrives whole.
    #[test]
    fn a_commit_receipt_carries_the_declared_close() {
        let three = Some(TimeUs(3_000_000));
        let longest = Some(TimeUs(2_147_483_647_000));
        let (chart, screens, mut link) = receipt_link(three);
        let mut declared = super::Declared::default();

        // On the answer.
        ready_attempt(&link, &chart);
        crate::play::commit_ready(&link, false, &mut declared).unwrap();
        assert_eq!(declared.close, three);
        // A retry commits it again.
        ready_attempt(&link, &chart);
        crate::play::commit_ready(&link, false, &mut declared).unwrap();
        assert_eq!(declared.close, three);

        // Refused: the screen built again since the preparation.
        ready_attempt(&link, &chart);
        rebuild_screen(&link, &screens);
        assert!(matches!(
            crate::play::commit_ready(&link, false, &mut declared),
            Err(ScreenError::Lost)
        ));
        assert_eq!(declared.close, None);
        // A screen built again after the commit keeps the receipt.
        ready_attempt(&link, &chart);
        crate::play::commit_ready(&link, false, &mut declared).unwrap();
        rebuild_screen(&link, &screens);
        assert_eq!(declared.close, three);

        // Withdrawn: the render thread reaches it past the bound.
        ready_attempt(&link, &chart);
        stall_render(&link, &screens);
        link.set_timeouts(Duration::from_millis(100), Duration::from_secs(5));
        assert!(matches!(
            crate::play::commit_ready(&link, false, &mut declared),
            Err(ScreenError::NoAnswer)
        ));
        assert_eq!(declared.close, None);
        screens.stall.store(false, Ordering::Release);
        let frames = || screens.loading.load(Ordering::Acquire);
        let from = frames();
        assert!(wait_until(|| frames() >= from + 3));
        link.set_timeouts(Duration::from_secs(10), Duration::from_secs(5));

        // Installed past the bound: the commit waits in the screen,
        // holding the commits' lock, until the game thread stopped
        // waiting for the answer, so the receipt is read past the bound.
        assert_eq!(
            link.select(ScreenKind::Play, Some(bundle_closing(longest)))
                .wait()
                .unwrap(),
            Preparation::Ready
        );
        ready_attempt(&link, &chart);
        link.set_timeouts(Duration::from_millis(100), Duration::from_secs(5));
        link.shared.commit_timed_out.store(false, Ordering::Release);
        screens.hold_commit.store(true, Ordering::Release);
        let release = std::thread::spawn({
            let screens = screens.clone();
            let shared = Arc::clone(&link.shared);
            move || {
                assert!(wait_until(|| screens.commit_held.load(Ordering::Acquire)));
                assert!(wait_until(|| shared
                    .commit_timed_out
                    .load(Ordering::Acquire)));
                screens.hold_commit.store(false, Ordering::Release);
            }
        });
        crate::play::commit_ready(&link, false, &mut declared).unwrap();
        release.join().unwrap();
        assert!(link.shared.commit_timed_out.load(Ordering::Acquire));
        assert_eq!(declared.close, longest);
        assert_eq!(
            game::closing_time(declared.close),
            TimeUs(2_147_483_647_000),
            "i32::MAX ms is admitted whole"
        );
        link.exit();
    }

    /// Each commit's receipt carries the play's ending the committed skin
    /// declares, beside its Closing duration, through the player's commit
    /// ([`crate::play::commit_ready`]): a margin and a fade-out, a
    /// negative fade-out kept, and none from a skin declaring none or a
    /// commit refused.
    #[test]
    fn a_commit_receipt_carries_the_declared_ending() {
        let ms = |v: i64| TimeUs(v * 1_000);
        let declares = skin_model::PlayEnding {
            finish_margin: ms(3_000),
            fadeout: ms(1_000),
        };
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let screens = Installers::default();
        let link = screens.spawn(&chart, &bundle_ending(Some(ms(2_000)), None, declares));
        let mut declared = super::Declared::default();
        ready_attempt(&link, &chart);
        crate::play::commit_ready(&link, false, &mut declared).unwrap();
        assert_eq!(
            declared,
            super::Declared {
                close: Some(ms(2_000)),
                ending: game::PlayEnding {
                    finish_margin: ms(3_000),
                    fadeout: ms(1_000),
                },
            }
        );

        // Another skin's negative fade-out arrives with its sign.
        let negative = skin_model::PlayEnding {
            finish_margin: ms(12_000),
            fadeout: ms(-250),
        };
        assert_eq!(
            link.select(ScreenKind::Play, Some(bundle_ending(None, None, negative)))
                .wait()
                .unwrap(),
            Preparation::Ready
        );
        ready_attempt(&link, &chart);
        crate::play::commit_ready(&link, false, &mut declared).unwrap();
        assert_eq!(
            declared.ending,
            game::PlayEnding {
                finish_margin: ms(12_000),
                fadeout: ms(-250),
            }
        );
        assert_eq!(declared.close, None);

        // Refused: the screen built again since the preparation.
        ready_attempt(&link, &chart);
        rebuild_screen(&link, &screens);
        assert!(matches!(
            crate::play::commit_ready(&link, false, &mut declared),
            Err(ScreenError::Lost)
        ));
        assert_eq!(declared.ending, game::PlayEnding::NONE);

        // A skin declaring none clears it.
        assert_eq!(
            link.select(ScreenKind::Play, Some(bundle()))
                .wait()
                .unwrap(),
            Preparation::Ready
        );
        ready_attempt(&link, &chart);
        crate::play::commit_ready(&link, false, &mut declared).unwrap();
        assert_eq!(declared, super::Declared::default());
        link.exit();
    }

    /// A skin declaring no Closing duration, committed after one that
    /// declares one, clears the receipt.
    #[test]
    fn a_skin_declaring_no_close_clears_the_receipt() {
        let (chart, _screens, link) = receipt_link(Some(TimeUs(3_000_000)));
        let mut declared = super::Declared::default();
        ready_attempt(&link, &chart);
        crate::play::commit_ready(&link, false, &mut declared).unwrap();
        assert_eq!(declared.close, Some(TimeUs(3_000_000)));
        assert_eq!(
            link.select(ScreenKind::Play, Some(bundle()))
                .wait()
                .unwrap(),
            Preparation::Ready
        );
        ready_attempt(&link, &chart);
        crate::play::commit_ready(&link, false, &mut declared).unwrap();
        assert_eq!(declared.close, None);
        link.exit();
    }

    /// An attempt prepared with no skin chosen (`select(None)`) draws with
    /// the installation in use, so its receipt is that skin's declared
    /// duration, on the first commit after the choice and on a retry.
    #[test]
    fn the_skin_in_use_gives_its_declared_close() {
        let three = Some(TimeUs(3_000_000));
        let (chart, _screens, link) = receipt_link(three);
        assert_eq!(
            link.select(ScreenKind::Play, None).wait().unwrap(),
            Preparation::Ready
        );
        let mut declared = super::Declared::default();
        for _ in 0..2 {
            ready_attempt(&link, &chart);
            crate::play::commit_ready(&link, false, &mut declared).unwrap();
            assert_eq!(declared.close, three);
        }
        link.exit();
    }

    /// Past the bound, a receipt is read only for the request waited for:
    /// with a later request committed and the receipt another request's,
    /// the commit answers installed with no declared duration, never the
    /// other request's.
    #[test]
    fn a_receipt_past_the_bound_is_only_its_own_requests() {
        let (chart, screens, mut link) = receipt_link(Some(TimeUs(3_000_000)));
        let mut declared = super::Declared::default();
        ready_attempt(&link, &chart);
        crate::play::commit_ready(&link, false, &mut declared).unwrap();
        assert_eq!(declared.close, Some(TimeUs(3_000_000)));
        ready_attempt(&link, &chart);

        // The render thread stalls in a loading frame, so the commit is
        // not taken within the bound, while the commits show a later
        // request committed and the first attempt's receipt.
        stall_render(&link, &screens);
        let request = link.requests.load(Ordering::Relaxed);
        link.shared.commits().committed = request + 1;
        link.set_timeouts(Duration::from_millis(100), Duration::from_secs(5));
        link.shared.commit_timed_out.store(false, Ordering::Release);
        crate::play::commit_ready(&link, false, &mut declared).unwrap();
        assert!(link.shared.commit_timed_out.load(Ordering::Acquire));
        assert_ne!(link.shared.commits().receipt.request, request);
        assert_eq!(declared.close, None);
        screens.stall.store(false, Ordering::Release);
        link.exit();
    }

    /// While a preparation is held unanswered, loading frames go on being
    /// presented; released, it answers. A preparation cancelled while held,
    /// or past the answer's bound, is discarded: released later, it never
    /// answers or installs, and its commit is refused.
    #[test]
    fn loading_frames_go_on_while_a_preparation_is_held() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let skin = bundle();
        let screens = Installers::default();
        let mut link = screens.spawn(&chart, &skin);
        let frames = || screens.loading.load(Ordering::Acquire);
        link.loading().show(one_line("loading"));
        assert!(wait_until(|| frames() > 0));

        // Held until three more loading frames were presented.
        screens.hold.store(true, Ordering::Release);
        let from = frames();
        let ready = link.prepare_attempt(titled(&chart), || {
            if frames() >= from + 3 {
                screens.hold.store(false, Ordering::Release);
            }
            false
        });
        assert_eq!(ready.unwrap(), Preparation::Ready);
        assert!(frames() >= from + 3);
        link.commit_attempt().unwrap();

        // The render thread dropped the held preparation on its discard
        // (the screen's `discard`, logged): released only then, so no
        // render thread late to the discard prepares it afresh.
        let discarded = |n: usize| {
            wait_until(|| screens.log().iter().filter(|l| *l == "1 discard").count() == n)
        };

        // Cancelled while held, then released: the late answer never
        // comes, and nothing commits.
        screens.hold.store(true, Ordering::Release);
        let from = frames();
        let cancelled = link.prepare_attempt(titled(&chart), || frames() >= from + 3);
        assert_eq!(cancelled.unwrap(), Preparation::Cancelled);
        assert!(discarded(1));
        screens.hold.store(false, Ordering::Release);
        let from = frames();
        assert!(wait_until(|| frames() >= from + 3));
        assert!(matches!(link.commit_attempt(), Err(ScreenError::Lost)));

        // Past the bound while held: the same.
        link.set_timeouts(Duration::from_millis(100), Duration::from_secs(5));
        screens.hold.store(true, Ordering::Release);
        let late = link.prepare_attempt(titled(&chart), || false);
        assert!(matches!(late, Err(ScreenError::NoAnswer)), "{late:?}");
        assert!(discarded(2));
        screens.hold.store(false, Ordering::Release);
        let from = frames();
        assert!(wait_until(|| frames() >= from + 3));
        link.set_timeouts(super::LOAD_TIMEOUT, super::EXIT_TIMEOUT);
        assert!(matches!(link.commit_attempt(), Err(ScreenError::Lost)));
        link.exit();
        assert_eq!(
            screens.log(),
            [
                "1 install 1",
                "1 commit 1",
                "1 prepare 2 Right T",
                "1 commit 2",
                "1 discard",
                "1 discard"
            ]
        );
    }

    /// A commit the render thread takes only after the game thread stopped
    /// waiting for it is withdrawn: it installs nothing and is discarded,
    /// so a play the game gave up on never draws; the next attempt
    /// commits.
    #[test]
    fn a_commit_past_its_bound_installs_nothing() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let skin = bundle();
        let screens = Installers::default();
        let mut link = screens.spawn(&chart, &skin);
        let frames = || screens.loading.load(Ordering::Acquire);
        link.loading().show(one_line("loading"));
        assert!(wait_until(|| frames() > 0));
        let ready = link.prepare_attempt(titled(&chart), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);

        // The render thread stalls in a loading frame past the bound.
        screens.stall.store(true, Ordering::Release);
        // A frame counted before the stall was seen may be the one held,
        // so wait for the hold itself rather than for another frame.
        assert!(wait_until(|| screens.stalled.load(Ordering::Acquire)));
        link.set_timeouts(Duration::from_millis(100), Duration::from_secs(5));
        assert!(matches!(link.commit_attempt(), Err(ScreenError::NoAnswer)));
        screens.stall.store(false, Ordering::Release);
        let from = frames();
        assert!(wait_until(|| frames() >= from + 3));
        link.set_timeouts(Duration::from_secs(10), Duration::from_secs(5));

        let ready = link.prepare_attempt(titled(&chart), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);
        link.commit_attempt().unwrap();
        link.exit();
        assert_eq!(
            screens.log(),
            [
                "1 install 1",
                "1 commit 1",
                "1 prepare 2 Right T",
                "1 discard",
                "1 prepare 3 Right T",
                "1 commit 3"
            ]
        );
    }

    /// A screen built again after a failure restores the bundle committed
    /// last, the very one, before anything else; a preparation answered on
    /// the failed screen is refused at its commit by its device, and the
    /// next attempt prepares and commits on the new screen.
    #[test]
    fn a_screen_built_again_restores_the_committed_bundle() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let skin = bundle();
        let screens = Installers::default();
        let link = screens.spawn(&chart, &skin);
        let ready = |link: &super::RenderLink| link.prepare_attempt(titled(&chart), || false);
        assert_eq!(ready(&link).unwrap(), Preparation::Ready);
        link.commit_attempt().unwrap();
        assert_eq!(ready(&link).unwrap(), Preparation::Ready);

        screens.fail.store(true, Ordering::Release);
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| link.failed()));
        screens.fail.store(false, Ordering::Release);
        link.prepare().unwrap();
        assert_eq!(screens.builds.load(Ordering::SeqCst), 2);
        let restored = screens.restored.lock().unwrap().clone();
        assert_eq!(restored.len(), 1);
        assert!(Arc::ptr_eq(restored[0].1.bundle(), &skin));
        assert!(matches!(link.commit_attempt(), Err(ScreenError::Lost)));

        assert_eq!(ready(&link).unwrap(), Preparation::Ready);
        link.commit_attempt().unwrap();
        link.exit();
        assert_eq!(
            screens.log(),
            [
                "1 install 1",
                "1 commit 1",
                "1 prepare 2 Right T",
                "1 commit 2",
                "1 prepare 3 Right T",
                "2 restore",
                "2 discard",
                "2 prepare 4 Right T",
                "2 commit 4",
            ]
        );
    }

    /// The attempt's final readiness (design.md "Play Lifecycle",
    /// preparing each attempt): a staging that still fits the screen is
    /// ready; another request's, or one after the screen was built again,
    /// answers `Stale` with the screen's inputs now (a new device), and
    /// the readiness prepares nothing.
    #[test]
    fn a_final_readiness_answers_stale_once_the_screen_changed() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let skin = bundle();
        let screens = Installers::default();
        let link = screens.spawn(&chart, &skin);
        let mut pending = link.begin_attempt(titled(&chart), super::AttemptSkin::InUse);
        let answered = loop {
            if let Some(a) = pending.wait_until(None, || false) {
                break a.unwrap();
            }
        };
        assert_eq!(answered.preparation, Preparation::Ready);
        let staged = answered.screen;
        let request = pending.request();
        assert_eq!(
            link.ready_attempt(request, 3).unwrap(),
            super::ReadyAnswer::Ready
        );
        assert_eq!(
            link.ready_attempt(request + 1, 3).unwrap(),
            super::ReadyAnswer::Stale(staged)
        );
        let prepared = |screens: &Installers| {
            screens
                .log()
                .iter()
                .filter(|l| l.contains("prepare"))
                .count()
        };
        let before = prepared(&screens);
        rebuild(&screens, &link).unwrap();
        let super::ReadyAnswer::Stale(now) = link.ready_attempt(request, 3).unwrap() else {
            panic!("ready after the screen was built again");
        };
        assert_ne!(now.device, staged.device);
        assert_eq!((now.target, now.skin), (staged.target, staged.skin));
        assert_eq!(prepared(&screens), before);
        link.exit();
    }

    /// Only the target's size, or only the play skin's selection, changed
    /// after the staging: the final readiness answers `Stale` with that
    /// input now, the others as staged, and prepares nothing.
    #[test]
    fn a_final_readiness_answers_stale_for_a_new_target_or_skin_selection() {
        for case in ["target", "skin"] {
            let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
            let skin = bundle();
            let screens = Installers::default();
            let link = screens.spawn(&chart, &skin);
            let mut pending = link.begin_attempt(titled(&chart), super::AttemptSkin::InUse);
            let answered = loop {
                if let Some(a) = pending.wait_until(None, || false) {
                    break a.unwrap();
                }
            };
            assert_eq!(answered.preparation, Preparation::Ready, "{case}");
            let staged = answered.screen;
            let request = pending.request();
            assert_eq!(
                link.ready_attempt(request, 3).unwrap(),
                super::ReadyAnswer::Ready,
                "{case}"
            );
            let extent = Arc::new(super::Extent::default());
            if case == "target" {
                link.follow(&extent);
                extent.publish(staged.target[0] / 2, staged.target[1] / 2);
                assert!(
                    wait_until(
                        || link.ready_attempt(request, 3).unwrap() != super::ReadyAnswer::Ready
                    ),
                    "{case}"
                );
            } else {
                assert_eq!(
                    link.select(ScreenKind::Play, None).wait().unwrap(),
                    Preparation::Ready
                );
            }
            let super::ReadyAnswer::Stale(now) = link.ready_attempt(request, 3).unwrap() else {
                panic!("{case}: ready after the change");
            };
            if case == "target" {
                assert_eq!(now.target, [staged.target[0] / 2, staged.target[1] / 2]);
                assert_eq!((now.device, now.skin), (staged.device, staged.skin));
            } else {
                assert_ne!(now.skin, staged.skin);
                assert_eq!((now.device, now.target), (staged.device, staged.target));
            }
            link.exit();
        }
    }

    /// A reply found disconnected, by an attempt's request waited for or
    /// by its final readiness, latches the screen's failure before its
    /// error is returned.
    #[test]
    fn a_disconnected_attempt_or_readiness_reply_latches_the_failure() {
        for case in ["attempt", "ready"] {
            let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
            let skin = bundle();
            let screens = Installers::default();
            let link = screens.spawn(&chart, &skin);
            // The thread ends; nothing answers any more.
            link.send(RenderMsg::Exit);
            assert!(wait_until(
                || link.ended.try_recv() == Err(std::sync::mpsc::TryRecvError::Disconnected)
            ));
            assert!(!link.failed(), "{case}");
            if case == "attempt" {
                let mut pending = link.begin_attempt(titled(&chart), super::AttemptSkin::InUse);
                assert!(
                    matches!(
                        pending.wait_until(None, || false),
                        Some(Err(ScreenError::Ended))
                    ),
                    "{case}"
                );
            } else {
                assert!(
                    matches!(link.ready_attempt(1, 0), Err(ScreenError::Ended)),
                    "{case}"
                );
            }
            assert!(link.failed(), "{case}");
        }
    }

    /// An attempt's request dropped unanswered, or answered but not taken,
    /// is discarded: its staging never installs.
    #[test]
    fn a_dropped_attempt_request_installs_nothing() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let skin = bundle();
        let screens = Installers::default();
        let link = screens.spawn(&chart, &skin);
        drop(link.begin_attempt(titled(&chart), super::AttemptSkin::InUse));
        assert!(matches!(link.commit_attempt(), Err(ScreenError::Lost)));
        // Answered, its reply sent (the render thread answers it before it
        // takes the readiness that finds it staged), but not taken.
        let pending = link.begin_attempt(titled(&chart), super::AttemptSkin::InUse);
        let request = pending.request();
        assert!(wait_until(
            || link.ready_attempt(request, 0).unwrap() == super::ReadyAnswer::Ready
        ));
        drop(pending);
        assert!(matches!(link.commit_attempt(), Err(ScreenError::Lost)));
        // A request answered ready and taken stays for its commit.
        let mut pending = link.begin_attempt(titled(&chart), super::AttemptSkin::InUse);
        while pending.wait_until(None, || false).is_none() {}
        drop(pending);
        link.commit_attempt().unwrap();
        link.exit();
    }

    /// Fails the installers' screen and builds it again through the render
    /// thread's own recovery (a preparation of the chart): what the new
    /// screen's build answers.
    fn rebuild(screens: &Installers, link: &super::RenderLink) -> Result<(), ScreenError> {
        screens.fail.store(true, Ordering::Release);
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| link.failed()));
        screens.fail.store(false, Ordering::Release);
        screens.restored.lock().unwrap().clear();
        link.prepare()
    }

    /// The kinds and bundles the last screen built restored, in order.
    fn restored(screens: &Installers) -> Vec<(ScreenKind, Arc<render::SkinBundle>)> {
        screens
            .restored
            .lock()
            .unwrap()
            .iter()
            .map(|(k, i)| (*k, Arc::clone(i.bundle())))
            .collect()
    }

    /// Every kind selected, each with its own bundle, in the reverse of
    /// the restore order: a screen built again after a failure restores
    /// all four, in exactly the order play, selection, decision, result.
    #[test]
    fn a_screen_built_again_restores_every_kind_in_order() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let screens = Installers::default();
        let link = screens.spawn_bare();
        link.load(Arc::clone(&chart), None).unwrap();
        let kinds = [
            ScreenKind::Play,
            ScreenKind::Selection,
            ScreenKind::Decision,
            ScreenKind::Result,
        ];
        let skins = kinds.map(|_| bundle());
        for (kind, skin) in kinds.iter().zip(&skins).rev() {
            assert_eq!(
                link.select(*kind, Some(Arc::clone(skin))).wait().unwrap(),
                Preparation::Ready
            );
        }
        rebuild(&screens, &link).unwrap();
        let got = restored(&screens);
        assert_eq!(got.iter().map(|(k, _)| *k).collect::<Vec<_>>(), kinds);
        for ((kind, bundle), skin) in got.iter().zip(&skins) {
            assert!(Arc::ptr_eq(bundle, skin), "{kind:?}");
        }
        link.exit();
    }

    /// A screen built again whose restore of the selection skin is
    /// refused on a healthy device is still built, play's and the
    /// decision skin restored, and keeps no selection skin from then on:
    /// a later screen does not try it again. A selection skin whose
    /// restore fails as a device failure fails the build, and stays to be
    /// restored by the next; play's refused restore fails the build too.
    #[test]
    fn recovery_drops_a_refused_screen_skin_alone() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let (play, selection, decision) = (bundle(), bundle(), bundle());
        let screens = Installers::default();
        let link = screens.spawn(&chart, &play);
        for (kind, skin) in [
            (ScreenKind::Selection, &selection),
            (ScreenKind::Decision, &decision),
        ] {
            assert_eq!(
                link.select(kind, Some(Arc::clone(skin))).wait().unwrap(),
                Preparation::Ready
            );
        }
        let kinds = |screens: &Installers| {
            restored(screens)
                .into_iter()
                .map(|(k, _)| k)
                .collect::<Vec<_>>()
        };

        // A device failure of the selection skin's restore fails the
        // build, and the next build restores it.
        *screens.fail_restore.lock().unwrap() = Some(ScreenKind::Selection);
        assert!(rebuild(&screens, &link).is_err());
        *screens.fail_restore.lock().unwrap() = None;
        link.prepare().unwrap();
        assert_eq!(
            kinds(&screens),
            [
                ScreenKind::Play,
                ScreenKind::Play,
                ScreenKind::Selection,
                ScreenKind::Decision
            ]
        );

        // Refused on a healthy device: built without it.
        *screens.refuse_restore.lock().unwrap() = Some(ScreenKind::Selection);
        rebuild(&screens, &link).unwrap();
        assert_eq!(kinds(&screens), [ScreenKind::Play, ScreenKind::Decision]);
        *screens.refuse_restore.lock().unwrap() = None;
        rebuild(&screens, &link).unwrap();
        assert_eq!(kinds(&screens), [ScreenKind::Play, ScreenKind::Decision]);

        // Play's refusal fails the build.
        *screens.refuse_restore.lock().unwrap() = Some(ScreenKind::Play);
        assert!(rebuild(&screens, &link).is_err());
        *screens.refuse_restore.lock().unwrap() = None;
        link.exit();
        let log = screens.log();
        let tried = |device: u32, step: &str| log.contains(&format!("{device} {step}"));
        assert!(tried(4, "restore Selection"), "{log:?}");
        assert!(!tried(5, "restore Selection"), "{log:?}");
        assert!(tried(5, "restore Decision"), "{log:?}");
    }

    /// No skin chosen for the selection screen removes its installation:
    /// once, answered ready, and a screen built again restores none for
    /// it. No skin chosen for play removes nothing and keeps the play
    /// skin, restored on the screen built again.
    #[test]
    fn no_screen_skin_removes_its_installation() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let (play, selection) = (bundle(), bundle());
        let screens = Installers::default();
        let link = screens.spawn(&chart, &play);
        assert_eq!(
            link.select(ScreenKind::Selection, Some(Arc::clone(&selection)))
                .wait()
                .unwrap(),
            Preparation::Ready
        );
        assert_eq!(
            link.select(ScreenKind::Selection, None).wait().unwrap(),
            Preparation::Ready
        );
        assert_eq!(
            link.select(ScreenKind::Play, None).wait().unwrap(),
            Preparation::Ready
        );
        rebuild(&screens, &link).unwrap();
        let got = restored(&screens);
        let [(ScreenKind::Play, kept)] = &got[..] else {
            panic!("{got:?}");
        };
        assert!(Arc::ptr_eq(kept, &play));
        link.exit();
        let removals: Vec<_> = screens
            .log()
            .into_iter()
            .filter(|s| s.contains("remove"))
            .collect();
        assert_eq!(removals, ["1 remove Selection"]);
    }

    /// A removal holds the commits' lock from its withdrawal check through
    /// the removal, as a commit does: held inside the screen's removal,
    /// the lock is taken, so a withdrawal either comes before the check
    /// or finds the removal done.
    #[test]
    fn a_removal_holds_the_commits_lock() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let (play, selection) = (bundle(), bundle());
        let screens = Installers::default();
        let link = screens.spawn(&chart, &play);
        assert_eq!(
            link.select(ScreenKind::Selection, Some(selection))
                .wait()
                .unwrap(),
            Preparation::Ready
        );
        screens.hold_remove.store(true, Ordering::Release);
        let pending = link.select(ScreenKind::Selection, None);
        assert!(wait_until(|| screens.remove_held.load(Ordering::Acquire)));
        // Looked at, then let go before asserting, so a failure never
        // leaves the render thread held.
        let held = matches!(
            link.shared.commits.try_lock(),
            Err(std::sync::TryLockError::WouldBlock)
        );
        screens.hold_remove.store(false, Ordering::Release);
        assert!(held, "the commits' lock is free during the removal");
        assert_eq!(pending.wait().unwrap(), Preparation::Ready);
        link.exit();
    }

    /// No skin chosen for the selection screen, withdrawn while the render
    /// thread stalls in a loading frame, is answered cancelled once the
    /// thread goes on and removes nothing: the selection skin stays, and a
    /// screen built again restores it. Its answer is read through the
    /// request's own receiver, kept apart from the request dropped.
    #[test]
    fn a_withdrawn_removal_removes_nothing() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let (play, selection) = (bundle(), bundle());
        let screens = Installers::default();
        let link = screens.spawn(&chart, &play);
        assert_eq!(
            link.select(ScreenKind::Selection, Some(Arc::clone(&selection)))
                .wait()
                .unwrap(),
            Preparation::Ready
        );
        stall_render(&link, &screens);
        let mut pending = link.select(ScreenKind::Selection, None);
        let (_, apart) = std::sync::mpsc::sync_channel(1);
        let rx = std::mem::replace(&mut pending.rx, apart);
        // Dropped unanswered: withdrawn.
        drop(pending);
        screens.stall.store(false, Ordering::Release);
        let answer = rx.recv_timeout(Duration::from_secs(10)).unwrap().unwrap();
        assert_eq!(answer, Preparation::Cancelled);
        link.end_loading().unwrap();

        rebuild(&screens, &link).unwrap();
        let got = restored(&screens);
        let [(ScreenKind::Play, _), (ScreenKind::Selection, kept)] = &got[..] else {
            panic!("{got:?}");
        };
        assert!(Arc::ptr_eq(kept, &selection));
        link.exit();
        assert!(
            !screens.log().iter().any(|s| s.contains("remove")),
            "{:?}",
            screens.log()
        );
    }

    /// A selection the game thread stopped waiting for installs nothing,
    /// however late the render thread gets to it: cancelled (dropped
    /// unanswered) or past its bound while the render thread stalls in a
    /// loading frame, it is staged and discarded, never committed, and the
    /// installation before it stays, restored on a screen built again;
    /// the next selection installs. Without the withdrawal checked at the
    /// commit, the late selections would commit.
    #[test]
    fn a_selection_given_up_on_installs_nothing() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let (old, late) = (bundle(), bundle());
        let screens = Installers::default();
        let mut link = screens.spawn(&chart, &old);
        let frames = || screens.loading.load(Ordering::Acquire);
        link.loading().show(one_line("loading"));
        assert!(wait_until(|| frames() > 0));
        let stalled = |link: &super::RenderLink, give_up: &dyn Fn(super::PendingSelect)| {
            screens.stall.store(true, Ordering::Release);
            assert!(wait_until(|| screens.stalled.load(Ordering::Acquire)));
            give_up(link.select(ScreenKind::Play, Some(Arc::clone(&late))));
            screens.stall.store(false, Ordering::Release);
            let from = frames();
            assert!(wait_until(|| frames() >= from + 3));
        };

        // Cancelled: dropped unanswered.
        stalled(&link, &drop);
        // Past its bound.
        link.set_timeouts(Duration::from_millis(100), Duration::from_secs(5));
        stalled(&link, &|pending| {
            let answer = pending.wait();
            assert!(matches!(answer, Err(ScreenError::NoAnswer)), "{answer:?}");
        });
        link.set_timeouts(Duration::from_secs(10), Duration::from_secs(5));

        // The old installation is the one committed: a screen built again
        // restores it.
        screens.fail.store(true, Ordering::Release);
        assert!(wait_until(|| link.failed()));
        screens.fail.store(false, Ordering::Release);
        link.prepare().unwrap();
        let restored = screens.restored.lock().unwrap().clone();
        let [(ScreenKind::Play, restored)] = &restored[..] else {
            panic!("{restored:?}");
        };
        assert!(Arc::ptr_eq(restored.bundle(), &old));
        assert_eq!(
            link.select(ScreenKind::Play, Some(Arc::clone(&late)))
                .wait()
                .unwrap(),
            Preparation::Ready
        );
        link.exit();
        assert_eq!(
            screens.log(),
            [
                "1 install 1",
                "1 commit 1",
                "1 install 2",
                "1 discard",
                "1 install 3",
                "1 discard",
                "2 restore",
                "2 install 4",
                "2 commit 4",
            ]
        );
    }

    /// A selection and an attempt's preparation share one sequence of
    /// requests, and the newer wins: an attempt prepared and not yet
    /// committed is discarded by a newer selection, which installs, and
    /// the attempt's commit is refused; an attempt prepared after the
    /// selection commits on it.
    #[test]
    fn a_newer_selection_discards_an_older_attempt() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let (first, second) = (bundle(), bundle());
        let screens = Installers::default();
        let link = screens.spawn(&chart, &first);
        let ready = |link: &super::RenderLink| link.prepare_attempt(titled(&chart), || false);
        assert_eq!(ready(&link).unwrap(), Preparation::Ready);
        assert_eq!(
            link.select(ScreenKind::Play, Some(Arc::clone(&second)))
                .wait()
                .unwrap(),
            Preparation::Ready
        );
        assert!(matches!(link.commit_attempt(), Err(ScreenError::Lost)));
        assert_eq!(ready(&link).unwrap(), Preparation::Ready);
        link.commit_attempt().unwrap();
        link.exit();
        assert_eq!(
            screens.log(),
            [
                "1 install 1",
                "1 commit 1",
                "1 prepare 2 Right T",
                "1 discard",
                "1 install 3",
                "1 commit 3",
                "1 prepare 4 Right T",
                "1 commit 4",
            ]
        );
    }

    /// A selection the screen fails on drops the screen and reports its
    /// failure, so the game's recovery builds it again with the
    /// installation before it; the failure is never only a notice. Without
    /// the failure published, `failed` stays false and nothing rebuilds.
    #[test]
    fn a_failed_selection_reports_the_screen_failure() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let (old, failing) = (bundle(), bundle());
        let screens = Installers::default();
        let link = screens.spawn(&chart, &old);
        screens.fail.store(true, Ordering::Release);
        let answer = link
            .select(ScreenKind::Play, Some(Arc::clone(&failing)))
            .wait();
        assert!(
            matches!(answer, Err(ScreenError::Render(RenderError::SurfaceLost))),
            "{answer:?}"
        );
        assert!(link.failed());
        screens.fail.store(false, Ordering::Release);
        link.prepare().unwrap();
        assert!(!link.failed());
        let restored = screens.restored.lock().unwrap().clone();
        let [(ScreenKind::Play, restored)] = &restored[..] else {
            panic!("{restored:?}");
        };
        assert!(Arc::ptr_eq(restored.bundle(), &old));
        link.exit();
        assert_eq!(screens.builds.load(Ordering::SeqCst), 2);
        assert_eq!(
            screens.log(),
            ["1 install 1", "1 commit 1", "1 fail 2", "2 restore"]
        );
    }

    /// A request older than one the render thread was asked already
    /// changes nothing when it arrives late: a selection behind an
    /// attempt's preparation neither discards the preparation, which still
    /// commits, nor installs; an attempt behind a selection is refused.
    #[test]
    fn a_late_older_request_changes_nothing() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let (first, late) = (bundle(), bundle());
        let screens = Installers::default();
        let link = screens.spawn(&chart, &first);
        let ready = link.prepare_attempt(titled(&chart), || false);
        assert_eq!(ready.unwrap(), Preparation::Ready);
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        link.send(RenderMsg::Select {
            request: 1,
            kind: ScreenKind::Play,
            skin: super::Selected::Skin(
                Some(Arc::clone(&late)),
                crate::screens::ScriptSlot::default(),
            ),
            reply,
        });
        let answer = rx.recv().unwrap().unwrap();
        assert!(matches!(answer, Preparation::Refused(_)), "{answer:?}");
        link.commit_attempt().unwrap();
        let (reply, rx) = std::sync::mpsc::sync_channel(1);
        link.send(RenderMsg::PrepareAttempt {
            request: 2,
            attempt: Box::new(titled(&chart)),
            skin: super::AttemptSkin::InUse,
            reply,
        });
        let answer = rx.recv().unwrap().unwrap();
        assert!(
            matches!(answer.preparation, Preparation::Refused(_)),
            "{answer:?}"
        );
        link.exit();
        assert_eq!(
            screens.log(),
            [
                "1 install 1",
                "1 commit 1",
                "1 prepare 2 Right T",
                "1 commit 2"
            ]
        );
    }

    /// A refused selection answers with the screen's reason and keeps the
    /// installation before it, whole: the next attempt prepares on it and
    /// a screen built again restores it, never another skin.
    #[test]
    fn a_refused_selection_keeps_the_installation_before_it() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let (old, refused) = (bundle(), bundle());
        let screens = Installers::default();
        let link = screens.spawn(&chart, &old);
        screens.refuse_install.store(true, Ordering::Release);
        let answer = link
            .select(ScreenKind::Play, Some(Arc::clone(&refused)))
            .wait()
            .unwrap();
        let Preparation::Refused(why) = answer else {
            panic!("{answer:?}");
        };
        assert_eq!(why.reason, "the skin cannot be installed");
        assert!(!link.failed());
        screens.fail.store(true, Ordering::Release);
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| link.failed()));
        screens.fail.store(false, Ordering::Release);
        link.prepare().unwrap();
        let restored = screens.restored.lock().unwrap().clone();
        let [(ScreenKind::Play, restored)] = &restored[..] else {
            panic!("{restored:?}");
        };
        assert!(Arc::ptr_eq(restored.bundle(), &old));
        link.exit();
        assert_eq!(
            screens.log(),
            [
                "1 install 1",
                "1 commit 1",
                "1 refuse 2",
                "1 discard",
                "2 restore"
            ]
        );
    }

    /// A loading frame the skin refuses keeps the screen and draws no
    /// further loading frame of its lifetime, its later lines included;
    /// the end of that lifetime reports the refusal, so recording does not
    /// open, and never success because the screen is still there. The
    /// next lifetime draws again and ends ready. Without the refusal kept
    /// for the lifetime, the end would answer ready.
    #[test]
    fn a_refused_loading_frame_refuses_the_end_of_loading() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let screens = Installers::default();
        let link = screens.spawn(&chart, &bundle());
        let frames = || screens.loading.load(Ordering::Acquire);
        screens.refuse_loading.store(true, Ordering::Release);
        link.loading().show(one_line("A"));
        assert!(wait_until(|| frames() == 1));
        link.loading().show(one_line("B"));
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(frames(), 1, "a refused lifetime drew again");
        let ended = link.end_loading();
        assert!(
            matches!(
                ended,
                Err(ScreenError::LoadingRefused(
                    skin_model::BuildError::InvalidReadouts
                ))
            ),
            "{ended:?}"
        );
        assert!(!link.failed());

        screens.refuse_loading.store(false, Ordering::Release);
        link.loading().show(one_line("C"));
        assert!(wait_until(|| frames() >= 3));
        link.end_loading().unwrap();
        link.exit();
        assert_eq!(screens.builds.load(Ordering::SeqCst), 1);
    }

    /// A loading lifetime's origin is its first loading frame's time,
    /// stamped once and kept apart from the screen: a screen built again
    /// during the lifetime draws its loading frames from the same origin,
    /// with the committed installation restored; the next lifetime stamps
    /// its own, later.
    #[test]
    fn a_loading_origin_outlives_a_screen_built_again() {
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let skin = bundle();
        let screens = Installers::default();
        let link = screens.spawn(&chart, &skin);
        let frames = || screens.loading.load(Ordering::Acquire);
        let sink = link.loading();
        sink.show(one_line("A"));
        assert!(wait_until(|| frames() >= 3));
        screens.fail.store(true, Ordering::Release);
        sink.show(one_line("A"));
        assert!(wait_until(|| link.failed()));
        screens.fail.store(false, Ordering::Release);
        link.prepare().unwrap();
        let from = frames();
        sink.show(one_line("A"));
        assert!(wait_until(|| frames() >= from + 3));
        link.end_loading().unwrap();
        link.loading().show(one_line("B"));
        let from = frames();
        assert!(wait_until(|| frames() >= from + 3));
        link.end_loading().unwrap();
        link.exit();

        let restored = screens.restored.lock().unwrap().clone();
        assert!(Arc::ptr_eq(restored[0].1.bundle(), &skin));
        let origins = screens.origins.lock().unwrap().clone();
        let lifetimes: Vec<u64> = origins.iter().map(|o| o.1).collect();
        let first = lifetimes[0];
        let of = |device: u64, lifetime: u64| -> Vec<TimeUs> {
            origins
                .iter()
                .filter(|o| o.0 == device && o.1 == lifetime)
                .map(|o| o.2)
                .collect()
        };
        let (before, after) = (of(1, first), of(2, first));
        assert!(before.len() >= 3 && after.len() >= 3, "{origins:?}");
        assert!(
            before.iter().chain(&after).all(|t| *t == before[0]),
            "{origins:?}"
        );
        let next = of(2, first + 1);
        assert!(next.len() >= 3, "{origins:?}");
        assert!(next.iter().all(|t| *t == next[0] && *t > before[0]));
    }

    /// Each frame's effect sequence numbers and play phase.
    type Adaptations = Arc<std::sync::Mutex<Vec<(Vec<u64>, bool)>>>;

    /// A screen that builds every play frame through the declarative
    /// skin's frame adapter, noting the effects' sequence numbers and the
    /// play phase it reads.
    struct Adapted {
        seen: Adaptations,
    }

    impl Screen for Adapted {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            let frame = render::skin_frame(f);
            assert!(std::ptr::eq(frame.tally, &f.view.tally));
            let seqs = frame.effects.iter().map(|e| e.seq).collect();
            self.seen.lock().unwrap().push((seqs, frame.playing));
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// The effects reach the declarative skin's frames in the order the
    /// logic step queued them, across drains of a buffer of 2: every
    /// effect once, none reordered, each frame at most a buffer's worth,
    /// with the play phase of the frame's own view.
    #[test]
    fn effects_reach_the_skin_frames_in_order_across_drains() {
        use game::DeviceSettings;
        use input::DeviceKind;

        let buffer = Vec::<eval::JudgeEvent>::with_capacity(2).capacity();
        let mut builder = ChartBuilder::new(120_000);
        for k in 0..6 {
            builder = builder.tap(Lane::new(1).unwrap(), TimeUs(1_000_000 + k * 200_000));
        }
        let chart = builder.build().unwrap();
        let rule = rules::tapestry();
        let device = DeviceSettings::default().profile(&[DeviceKind::Keyboard], &[]);
        let (mut writer, reader) = input::source_queue(32);
        for e in input::autoplay(&chart, &device.sources[0]).unwrap() {
            writer.push(e).unwrap();
        }
        writer.publish(TimeUs(3_000_000));
        let (mut logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            vec![reader],
            LogicConfig {
                inputs: 32,
                effects: 64,
            },
            Vec::new(),
        )
        .unwrap();
        logic.advance_to(TimeUs(3_000_001));
        let queued: Vec<u64> = logic.events().iter().map(|e| e.seq).collect();
        assert!(queued.len() > 2 * buffer);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&seen);
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(Adapted {
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            Some(Arc::new(chart.clone())),
            None,
            clock,
            View { effects: buffer },
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.send(RenderMsg::Play {
            state_origin: TimeUs(0),
            side,
            gauge: 0,
            measure: play_measure(clock.now()),
        });
        let drawn = || -> usize { seen.lock().unwrap().iter().map(|(s, _)| s.len()).sum() };
        assert!(wait_until(|| drawn() >= queued.len()));
        let _ = link.finish(clock.now());
        link.exit();
        let frames = seen.lock().unwrap().clone();
        assert!(frames.iter().all(|(s, _)| s.len() <= buffer), "{frames:?}");
        assert!(frames.iter().filter(|(s, _)| !s.is_empty()).count() > 1);
        let order: Vec<u64> = frames.iter().flat_map(|(s, _)| s.iter().copied()).collect();
        assert_eq!(order, queued);
        let playing = logic.view().playing;
        assert!(frames.iter().all(|&(_, p)| p == playing));
    }

    /// Each play frame's chart time and Closing start, as the declarative
    /// skin's frame adapter reads them.
    type Closings = Arc<std::sync::Mutex<Vec<(TimeUs, Option<TimeUs>, bool)>>>;

    /// A screen that builds every play frame through the frame adapter,
    /// noting its time, its Closing start and its phase.
    struct ClosingFrames {
        seen: Closings,
    }

    impl Screen for ClosingFrames {
        fn load(
            &mut self,
            _chart: &Chart,
            _section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            Ok(())
        }
        fn wait(&self) -> bool {
            true
        }
        fn paced(&self) -> bool {
            false
        }
        fn build(&mut self, f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            let frame = render::skin_frame(f);
            self.seen
                .lock()
                .unwrap()
                .push((frame.now, frame.closing, frame.playing));
            Ok(())
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            Ok(timing(true))
        }
        fn text(&mut self, _lines: &[&str]) -> Result<bool, RenderError> {
            Ok(true)
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            None
        }
    }

    /// Once the play enters Closing, its frames go on in play mode on the
    /// play clock until the play's end: a positive number of them carry
    /// Closing's start, not playing, with their time advancing.
    #[test]
    fn closing_frames_draw_on_the_play_clock_until_the_end() {
        use game::DeviceSettings;
        use input::DeviceKind;

        let chart = ChartBuilder::new(120_000)
            .tap(Lane::new(1).unwrap(), TimeUs(1_000_000))
            .build()
            .unwrap();
        let rule = rules::tapestry();
        let device = DeviceSettings::default().profile(&[DeviceKind::Keyboard], &[]);
        let (writer, reader) = input::source_queue(8);
        writer.publish(TimeUs(0));
        let (mut logic, side) = Logic::<SourceReader>::new(
            &chart,
            &rule,
            &device,
            &InputStart::default(),
            vec![reader],
            LogicConfig {
                inputs: 8,
                effects: 8,
            },
            Vec::new(),
        )
        .unwrap();
        logic.advance_to(TimeUs(1));
        let at = TimeUs(-5_000_000);
        logic.enter_closing(at);
        let seen = Arc::new(std::sync::Mutex::new(Vec::new()));
        let noted = Arc::clone(&seen);
        let make: MakeScreen = Box::new(move || {
            Ok(Box::new(ClosingFrames {
                seen: Arc::clone(&noted),
            }) as Box<dyn Screen>)
        });
        let clock = QpcClock::start();
        let link = spawn(
            make,
            Some(Arc::new(chart.clone())),
            None,
            clock,
            View { effects: 8 },
            measure(clock),
            Box::new(NoFrames),
        )
        .unwrap();
        link.send(RenderMsg::Play {
            state_origin: TimeUs(0),
            side,
            gauge: 0,
            measure: play_measure(clock.now()),
        });
        let closing = || {
            seen.lock()
                .unwrap()
                .iter()
                .filter(|(_, c, _)| *c == Some(at))
                .count()
        };
        assert!(wait_until(|| closing() >= 3));
        let _ = link.finish(clock.now());
        link.exit();
        let frames: Vec<_> = seen
            .lock()
            .unwrap()
            .iter()
            .copied()
            .filter(|(_, c, _)| *c == Some(at))
            .collect();
        assert!(frames.len() >= 3, "{frames:?}");
        assert!(frames.iter().all(|&(_, _, playing)| !playing), "{frames:?}");
        assert!(frames.windows(2).all(|w| w[0].0 <= w[1].0), "{frames:?}");
        assert!(
            frames.first().unwrap().0 < frames.last().unwrap().0,
            "{frames:?}"
        );
    }

    /// What the [`Probe`]s of one render thread saw.
    #[derive(Debug, Default)]
    struct Probed {
        /// Per restore: whether the bundle installed is the one restored,
        /// the resource generation, and a frame drawn at once.
        restores: Vec<(bool, u64, Probing)>,
        /// Per ready installation (a selection): the atlas bytes staged
        /// beside the installed atlas.
        installs: Vec<u64>,
        /// Per ready preparation: the atlas bytes staged beside the
        /// installed atlas.
        staged_atlas: Vec<u64>,
        /// Per commit: the resource generation and a frame drawn at once.
        commits: Vec<(u64, Probing)>,
        /// Per loading frame drawn: the device, the lifetime, its origin
        /// and the installed presentation's `when` timer start.
        loadings: Vec<(u64, u64, TimeUs, Option<TimeUs>)>,
        /// The installed presentation at each loading frame.
        presentations: Vec<render::LoadingPresentation>,
    }

    /// A probe frame's sprites and the RGB of its samples ([`SAMPLES`]),
    /// or its error.
    type Probing = Result<(usize, [[u8; 3]; 4]), RenderError>;

    /// Where a probe frame is sampled: the left half's centre and the
    /// right half's, then the gauge-look marker's centre and the judgment
    /// marker's ([`crate::render_decl::GAUGE_MARKER`], `JUDGE_MARKER`).
    const SAMPLES: [(u32, u32); 4] = [(480, 540), (1_440, 540), (1_250, 150), (1_550, 150)];

    const RED: [u8; 3] = [255, 0, 0];
    const BLACK: [u8; 3] = [0; 3];

    /// A real menu screen on WARP whose menu frames fail while `fail` is
    /// set, so the render thread drops it and builds another. It notes
    /// what its renderer installed and stages, and draws a play frame of
    /// [`crate::render_common::view`] with `gauges` gauges, and one
    /// judgment among its effects while `judge` is set, straight after
    /// each restore and each commit ([`Probed`]).
    struct Probe {
        menu: crate::menu_screen::MenuScreen,
        fail: Arc<AtomicBool>,
        judge: Arc<AtomicBool>,
        gauges: Arc<std::sync::atomic::AtomicUsize>,
        probed: Arc<std::sync::Mutex<Probed>>,
    }

    impl Probe {
        /// A frame of the installed skin at 1 ms, read back.
        fn frame(&mut self) -> Probing {
            let mut v = crate::render_common::view(0, 0);
            let gauge = v.gauges[0];
            v.gauges.resize(self.gauges.load(Ordering::Acquire), gauge);
            let judged = [eval::JudgeEvent {
                seq: 1,
                time: TimeUs(0),
                origin: Some(TimeUs(0)),
                kind: eval::JudgeEventKind::Judged {
                    lane: Lane::new(1).unwrap(),
                    note: 0,
                    judgment: rules::Judgment::Great,
                    deviation: Some(TimeUs(0)),
                    release: None,
                    combo: 1,
                    presses: 0,
                },
            }];
            let effects: &[eval::JudgeEvent] = if self.judge.load(Ordering::Acquire) {
                &judged
            } else {
                &[]
            };
            let r = self.menu.renderer_mut();
            let f = crate::render_common::input(&v, TimeUs(1_000), TimeUs(0), effects);
            let sprites = r.frame(&f)?.sprites;
            let p = r.read_back();
            Ok((
                sprites,
                SAMPLES.map(|at| crate::render_common::rgb_at(&p, 1_920, at)),
            ))
        }

        fn probed(&self) -> std::sync::MutexGuard<'_, Probed> {
            self.probed.lock().unwrap()
        }
    }

    impl Screen for Probe {
        fn load(
            &mut self,
            chart: &Chart,
            section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            self.menu.load(chart, section)
        }
        fn prepare_attempt(
            &mut self,
            a: &render::AttemptInput<'_>,
            skin: Option<&Arc<render::SkinBundle>>,
            request: u64,
        ) -> Option<Result<Preparation, RenderError>> {
            let answer = self.menu.prepare_attempt(a, skin, request);
            if matches!(answer, Some(Ok(Preparation::Ready))) {
                let staged = self.menu.renderer_mut().memory().staged_atlas;
                self.probed().staged_atlas.push(staged);
            }
            answer
        }
        fn device(&self) -> u64 {
            self.menu.device()
        }
        fn commit(&mut self, token: render::PreparedToken) -> Result<(), render::CommitError> {
            self.menu.commit(token)?;
            let generation = self.menu.renderer_mut().resource_generation();
            let drew = self.frame();
            self.probed().commits.push((generation, drew));
            Ok(())
        }
        fn discard(&mut self) {
            self.menu.discard();
        }
        fn install(
            &mut self,
            kind: ScreenKind,
            skin: &Arc<render::SkinBundle>,
            request: u64,
        ) -> Result<Preparation, RenderError> {
            let answer = self.menu.install(kind, skin, request);
            if matches!(answer, Ok(Preparation::Ready)) {
                let staged = self.menu.renderer_mut().memory().staged_atlas;
                self.probed().installs.push(staged);
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
        ) -> Result<(), RenderError> {
            self.menu.restore(kind, installation)?;
            let r = self.menu.renderer_mut();
            let same = r
                .installed(kind)
                .is_some_and(|i| Arc::ptr_eq(i, installation.bundle()));
            let generation = r.resource_generation();
            let drew = self.frame();
            self.probed().restores.push((same, generation, drew));
            Ok(())
        }
        fn remove(&mut self, kind: ScreenKind) {
            self.menu.remove(kind);
        }
        fn wait(&self) -> bool {
            self.menu.wait()
        }
        fn paced(&self) -> bool {
            self.menu.paced()
        }
        fn build(&mut self, f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            self.menu.build(f)
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            self.menu.draw()
        }
        fn text(&mut self, lines: &[&str]) -> Result<bool, RenderError> {
            self.menu.text(lines)
        }
        fn menu(
            &mut self,
            page: &crate::menu_screen::MenuPage,
            changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            if self.fail.load(Ordering::Acquire) {
                return Err(RenderError::SurfaceLost);
            }
            self.menu.menu(page, changed)
        }
        fn loading(
            &mut self,
            page: &crate::menu_screen::MenuPage,
            update: Option<&LoadingUpdate>,
            changed: bool,
            frame: &render::LoadingFrame<'_>,
        ) -> Result<Result<FrameTiming, skin_model::BuildError>, RenderError> {
            if self.fail.load(Ordering::Acquire) {
                return Err(RenderError::SurfaceLost);
            }
            let drawn = self.menu.loading(page, update, changed, frame)?;
            let device = self.menu.device();
            if let Some(i) = self.menu.installation(ScreenKind::Play) {
                let when = i.loading().timer(crate::render_decl::WHEN_SLOT);
                let mut probed = self.probed();
                probed
                    .loadings
                    .push((device, frame.lifetime, frame.load.since, when));
                probed.presentations.push(i.loading().clone());
            }
            Ok(drawn)
        }
        fn resize(&mut self, width: u32, height: u32) {
            self.menu.resize(width, height);
        }
        fn target(&self) -> [u32; 2] {
            self.menu.target()
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            self.menu.read_back()
        }
    }

    /// Through a real menu screen, with the runtime fonts its loading
    /// lines need, and the render thread's own recovery, the loading
    /// presentation belongs to the committed installation and
    /// its lifetime: within one lifetime the `when` timer started on the
    /// first loading frame keeps its start across an attempt staged and
    /// committed on the same bundle and across a renderer built again
    /// (its files deleted, restored from memory), and the loading frames
    /// keep the lifetime's origin; the next lifetime on the same bundle,
    /// its condition still true, starts it afresh, from its own origin.
    #[test]
    fn a_loading_presentation_keeps_its_lifetime_across_a_rebuilt_renderer() {
        use crate::render_decl::{Look, bundle};

        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let dir = tempfile::tempdir().unwrap();
        let look = Look {
            loading: true,
            ..Look::left([255, 0, 0, 255])
        };
        let (skin, _) = bundle(
            &crate::render_common::offscreen(1_920, 1_080),
            dir.path(),
            &look,
        );
        let fail = Arc::new(AtomicBool::new(false));
        let probed = Arc::new(std::sync::Mutex::new(Probed::default()));
        let builds = Arc::new(AtomicU32::new(0));
        let (f, p, b) = (Arc::clone(&fail), Arc::clone(&probed), Arc::clone(&builds));
        let make: MakeScreen = Box::new(move || {
            b.fetch_add(1, Ordering::SeqCst);
            let renderer = render::Renderer::new(
                render::Target::Offscreen {
                    width: 1_920,
                    height: 1_080,
                },
                render::AdapterChoice::Warp,
            )?;
            Ok(Box::new(Probe {
                menu: crate::menu_screen::MenuScreen::new(
                    renderer,
                    Some(render::text::Fonts::runtime()?),
                ),
                fail: Arc::clone(&f),
                judge: Arc::new(AtomicBool::new(false)),
                gauges: Arc::new(std::sync::atomic::AtomicUsize::new(2)),
                probed: Arc::clone(&p),
            }) as Box<dyn Screen>)
        });
        let link = spawn_empty(make);
        link.load(Arc::clone(&chart), None).unwrap();
        assert_eq!(
            link.select(ScreenKind::Play, Some(Arc::clone(&skin)))
                .wait()
                .unwrap(),
            Preparation::Ready
        );
        let drawn = || probed.lock().unwrap().loadings.len();
        let sink = link.loading();
        sink.show(one_line("A"));
        assert!(wait_until(|| drawn() >= 3));

        // The same bundle staged and committed for an attempt.
        let attempt = preset_attempt(&chart, &rules::tapestry(), None, "T");
        assert_eq!(
            link.prepare_attempt(attempt, || false).unwrap(),
            Preparation::Ready
        );
        link.commit_attempt().unwrap();
        let from = drawn();
        assert!(wait_until(|| drawn() >= from + 3));

        // The renderer fails and is built again, the files gone: frames
        // drawn on the rebuilt device.
        let (first_device, a) = {
            let probed = probed.lock().unwrap();
            (probed.loadings[0].0, probed.loadings[0].1)
        };
        dir.close().unwrap();
        fail.store(true, Ordering::Release);
        assert!(wait_until(|| link.failed()));
        fail.store(false, Ordering::Release);
        link.prepare().unwrap();
        sink.show(one_line("A"));
        // A loading frame as the probe notes it.
        type Seen = (u64, u64, TimeUs, Option<TimeUs>);
        let counted = |what: &dyn Fn(&Seen) -> bool| {
            probed
                .lock()
                .unwrap()
                .loadings
                .iter()
                .filter(|l| what(l))
                .count()
        };
        assert!(wait_until(
            || counted(&|l| l.0 != first_device && l.1 == a) >= 3
        ));

        // A new lifetime on the same bundle, after the end of A's is
        // answered.
        link.end_loading().unwrap();
        link.loading().show(one_line("B"));
        assert!(wait_until(|| counted(&|l| l.1 != a) >= 3));
        link.end_loading().unwrap();
        link.exit();
        assert_eq!(builds.load(Ordering::SeqCst), 2);

        let probed = probed.lock().unwrap();
        let all = &probed.loadings;
        // Every frame of A comes before every frame of B.
        let rebuilt = all.iter().position(|l| l.1 != a).unwrap();
        assert!(all[rebuilt..].iter().all(|l| l.1 != a), "{all:?}");
        let b = all[rebuilt].1;
        let (_, _, since, when) = all[0];
        let started = when.expect("the when timer starts on the first frame");
        // Lifetime A: one origin and one `when` start throughout, on both
        // renderers, the presentation shared with the restored one.
        let lifetime_a = &all[..rebuilt];
        assert!(lifetime_a.iter().all(|l| l.1 == a), "{all:?}");
        assert!(lifetime_a.iter().any(|l| l.0 != first_device), "{all:?}");
        assert!(
            lifetime_a
                .iter()
                .all(|&(_, _, s, w)| s == since && w == Some(started)),
            "{all:?}"
        );
        assert!(
            probed.presentations[..rebuilt]
                .iter()
                .all(|p| p.same(&probed.presentations[0]))
        );
        // Lifetime B: its own later origin and a fresh start.
        let lifetime_b = &all[rebuilt..];
        let (_, _, since_b, when_b) = lifetime_b[0];
        assert!(since_b > since, "{all:?}");
        let started_b = when_b.expect("the when timer starts afresh");
        assert!(started_b > started, "{all:?}");
        assert!(
            lifetime_b
                .iter()
                .all(|&(_, l, s, w)| l == b && s == since_b && w == Some(started_b)),
            "{all:?}"
        );
    }

    /// What drew red in one loading frame at 1920×1080, at the centres of
    /// the fixture's places ([`crate::render_decl`]).
    #[derive(Clone, Copy, Debug)]
    struct Red {
        title: bool,
        artist: bool,
        genre: bool,
        full_title: bool,
        gauge: bool,
        line: bool,
        background: bool,
        percent: bool,
        progress: bool,
        /// The animation's square where it starts.
        animation_start: bool,
        /// The animation's square where the loading timer puts it now:
        /// moving 1 canvas pixel per ms for 1 s, then held at its end.
        animation_now: bool,
    }

    /// One loading frame as [`Recorder`] saw it.
    #[derive(Clone, Debug)]
    struct Shot {
        device: u64,
        /// The target the frame was drawn at.
        target: [u32; 2],
        lifetime: u64,
        now: TimeUs,
        load: skin_model::LoadState,
        gauge: Option<skin_model::LoadingGauge>,
        text: u64,
        /// The installed bundle, by address.
        bundle: Option<usize>,
        /// The installed presentation's `when` timer 0's start.
        when: Option<TimeUs>,
        /// Its progress timer's start ([`crate::render_decl::PROGRESS_SLOT`]).
        progress: Option<TimeUs>,
        /// The chosen chart's text staged after the frame, in bytes.
        staged: u64,
        red: Option<Red>,
        /// The imported fixture's title, artist, genre and full title
        /// squares ([`crate::render_json::text_at`]).
        glyphs: Option<[[u8; 3]; 4]>,
    }

    type Shots = Arc<std::sync::Mutex<Vec<Shot>>>;

    /// A size [`Recorder`] applies before a loading frame: before the
    /// next one, or with a revision, before the first one whose state
    /// has that text revision.
    type Resize = Arc<std::sync::Mutex<Option<(Option<u64>, (u32, u32))>>>;

    /// A real menu screen on WARP that records each loading frame
    /// ([`Shot`]), counts the play frames it is asked to build, applies a
    /// size set in `resize` ([`Resize`]), and fails its loading frames
    /// while `fail` is set, so the render thread builds another.
    struct Recorder {
        menu: crate::menu_screen::MenuScreen,
        shots: Shots,
        builds: Arc<AtomicU32>,
        resize: Resize,
        fail: Arc<AtomicBool>,
        /// Taken by the next loading frame: its renderer's next text
        /// preparation fails waiting for the GPU.
        #[cfg(feature = "dev-tools")]
        text_fault: Arc<AtomicBool>,
    }

    impl Recorder {
        fn shot(&mut self, frame: &render::LoadingFrame<'_>, drawn: bool) -> Shot {
            use crate::render_decl::{
                ANIMATION, ARTIST, BACKGROUND, FULL_TITLE, GAUGE, GENRE, LINE_AT, PERCENT_MARK,
                PROGRESS_MARK, PROGRESS_SLOT, TITLE, WHEN_SLOT,
            };
            let device = self.menu.device();
            let installation = self.menu.installation(ScreenKind::Play);
            let r = self.menu.renderer_mut();
            let staged = r.memory().staged_text;
            let target = r.target();
            let pixels = (drawn && r.target() == [1_920, 1_080]).then(|| r.read_back());
            let glyphs = pixels.as_deref().map(crate::render_json::text_at);
            let red = pixels.map(|p| {
                let red_at = |at: (u32, u32)| crate::render_common::rgb_at(&p, 1_920, at) == RED;
                let centre = |[x, y, w, h]: [i32; 4]| {
                    red_at((
                        u32::try_from(x + w / 2).unwrap(),
                        u32::try_from(y + h / 2).unwrap(),
                    ))
                };
                let moved = ((frame.now.0 - frame.load.since.0) / 1_000).min(1_000);
                let x = 250 + moved;
                Red {
                    title: centre(TITLE),
                    artist: centre(ARTIST),
                    genre: centre(GENRE),
                    full_title: centre(FULL_TITLE),
                    gauge: centre(GAUGE),
                    line: red_at(LINE_AT),
                    background: centre(BACKGROUND),
                    percent: centre(PERCENT_MARK),
                    progress: centre(PROGRESS_MARK),
                    animation_start: centre(ANIMATION),
                    animation_now: red_at((u32::try_from(x).unwrap(), 950)),
                }
            });
            Shot {
                device,
                target,
                lifetime: frame.lifetime,
                now: frame.now,
                load: frame.load,
                gauge: frame.gauge,
                text: frame.text,
                bundle: installation
                    .as_ref()
                    .map(|i| Arc::as_ptr(i.bundle()) as *const () as usize),
                when: installation
                    .as_ref()
                    .and_then(|i| i.loading().timer(WHEN_SLOT)),
                progress: installation
                    .as_ref()
                    .and_then(|i| i.loading().timer(PROGRESS_SLOT)),
                staged,
                red,
                glyphs,
            }
        }
    }

    impl Screen for Recorder {
        fn load(
            &mut self,
            chart: &Chart,
            section: Option<&eval::Section>,
        ) -> Result<(), RenderError> {
            self.menu.load(chart, section)
        }
        fn prepare_attempt(
            &mut self,
            a: &render::AttemptInput<'_>,
            skin: Option<&Arc<render::SkinBundle>>,
            request: u64,
        ) -> Option<Result<Preparation, RenderError>> {
            self.menu.prepare_attempt(a, skin, request)
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
        ) -> Result<Preparation, RenderError> {
            self.menu.install(kind, skin, request)
        }
        fn installation(&self, kind: ScreenKind) -> Option<render::Installation> {
            self.menu.installation(kind)
        }
        fn restore(
            &mut self,
            kind: ScreenKind,
            installation: &render::Installation,
        ) -> Result<(), RenderError> {
            self.menu.restore(kind, installation)
        }
        fn remove(&mut self, kind: ScreenKind) {
            self.menu.remove(kind);
        }
        fn wait(&self) -> bool {
            self.menu.wait()
        }
        fn paced(&self) -> bool {
            self.menu.paced()
        }
        fn build(&mut self, f: &FrameInput<'_>) -> Result<(), skin_model::BuildError> {
            self.builds.fetch_add(1, Ordering::SeqCst);
            self.menu.build(f)
        }
        fn draw(&mut self) -> Result<FrameTiming, RenderError> {
            self.menu.draw()
        }
        fn text(&mut self, lines: &[&str]) -> Result<bool, RenderError> {
            self.menu.text(lines)
        }
        fn menu(
            &mut self,
            page: &crate::menu_screen::MenuPage,
            changed: bool,
        ) -> Result<FrameTiming, RenderError> {
            self.menu.menu(page, changed)
        }
        fn loading(
            &mut self,
            page: &crate::menu_screen::MenuPage,
            update: Option<&LoadingUpdate>,
            changed: bool,
            frame: &render::LoadingFrame<'_>,
        ) -> Result<Result<FrameTiming, skin_model::BuildError>, RenderError> {
            if self.fail.load(Ordering::Acquire) {
                return Err(RenderError::SurfaceLost);
            }
            #[cfg(feature = "dev-tools")]
            if self.text_fault.swap(false, Ordering::AcqRel) {
                self.menu.renderer_mut().fail_next_text_wait();
            }
            {
                let mut resize = self.resize.lock().unwrap();
                if let Some((at, (w, h))) = *resize
                    && at.is_none_or(|r| update.is_some_and(|u| u.revision == r))
                {
                    *resize = None;
                    self.menu.resize(w, h);
                }
            }
            let drawn = self.menu.loading(page, update, changed, frame)?;
            let shot = self.shot(frame, drawn.is_ok());
            self.shots.lock().unwrap().push(shot);
            Ok(drawn)
        }
        fn resize(&mut self, width: u32, height: u32) {
            self.menu.resize(width, height);
        }
        fn target(&self) -> [u32; 2] {
            self.menu.target()
        }
        #[cfg(feature = "dev-tools")]
        fn read_back(&self) -> Option<(u32, u32, Vec<u8>)> {
            self.menu.read_back()
        }
    }

    /// What the [`Recorder`]s of one render thread record.
    struct Recording {
        shots: Shots,
        builds: Arc<AtomicU32>,
        resize: Resize,
        fail: Arc<AtomicBool>,
        made: Arc<AtomicU32>,
        /// Set: the next loading frame's renderer fails its next text
        /// preparation's wait for the GPU.
        #[cfg(feature = "dev-tools")]
        text_fault: Arc<AtomicBool>,
    }

    impl Recording {
        /// A render thread on [`Recorder`]s with the runtime fonts, and
        /// what they record.
        fn new() -> (super::RenderLink, Self) {
            let shots: Shots = Arc::new(std::sync::Mutex::new(Vec::new()));
            let builds = Arc::new(AtomicU32::new(0));
            let resize: Resize = Arc::new(std::sync::Mutex::new(None));
            let fail = Arc::new(AtomicBool::new(false));
            let made = Arc::new(AtomicU32::new(0));
            #[cfg(feature = "dev-tools")]
            let text_fault = Arc::new(AtomicBool::new(false));
            #[cfg(feature = "dev-tools")]
            let t = Arc::clone(&text_fault);
            let (s, b, r, f, m) = (
                Arc::clone(&shots),
                Arc::clone(&builds),
                Arc::clone(&resize),
                Arc::clone(&fail),
                Arc::clone(&made),
            );
            let make: MakeScreen = Box::new(move || {
                m.fetch_add(1, Ordering::SeqCst);
                let renderer = render::Renderer::new(
                    render::Target::Offscreen {
                        width: 1_920,
                        height: 1_080,
                    },
                    render::AdapterChoice::Warp,
                )?;
                Ok(Box::new(Recorder {
                    menu: crate::menu_screen::MenuScreen::new(
                        renderer,
                        Some(render::text::Fonts::runtime()?),
                    ),
                    shots: Arc::clone(&s),
                    builds: Arc::clone(&b),
                    resize: Arc::clone(&r),
                    fail: Arc::clone(&f),
                    #[cfg(feature = "dev-tools")]
                    text_fault: Arc::clone(&t),
                }) as Box<dyn Screen>)
            });
            let recording = Self {
                shots,
                builds,
                resize,
                fail,
                made,
                #[cfg(feature = "dev-tools")]
                text_fault,
            };
            (spawn_empty(make), recording)
        }

        fn count(&self) -> usize {
            self.shots.lock().unwrap().len()
        }

        /// Waits for `n` more loading frames than `from`, then returns
        /// the count.
        fn more(&self, from: usize, n: usize) -> usize {
            assert!(
                wait_until(|| self.count() >= from + n),
                "{} frames",
                self.count()
            );
            self.count()
        }

        /// Waits until a loading frame after the `from`-th satisfies
        /// `what`, then returns the count.
        fn until(&self, from: usize, what: impl Fn(&Shot) -> bool) -> usize {
            assert!(
                wait_until(|| self.shots.lock().unwrap().iter().skip(from).any(&what)),
                "{:?}",
                self.shots
                    .lock()
                    .unwrap()
                    .iter()
                    .skip(from)
                    .collect::<Vec<_>>()
            );
            self.count()
        }

        fn shots(&self) -> Vec<Shot> {
            self.shots.lock().unwrap().clone()
        }
    }

    /// The fixture with every loading part: the animation, the `when`
    /// mark, the lanes and gauge, the chosen chart's text and the
    /// progress marks.
    fn bridge_look() -> crate::render_decl::Look {
        crate::render_decl::Look {
            loading: true,
            structure: true,
            chart_text: true,
            progress: true,
            ..crate::render_decl::Look::left([255, 0, 0, 255])
        }
    }

    /// The loading text of chart `title` by `artist`, if it has one.
    fn chosen_by(title: &str, artist: Option<&str>) -> LoadingText {
        let mut text = chosen(title);
        text.chart.artist = artist.map(str::to_owned);
        text
    }

    /// Through the render thread, a real menu screen, its renderer and a
    /// declarative skin: chart B chosen after chart A's lifetime ended,
    /// its load stalled at 4 of 9 sounds, keeps drawing B's lifetime from
    /// its own origin: the background, the animation moving on the
    /// loading clock, `when` timer 0 started once, the lanes' judgment
    /// line, the gauge from its descriptor with no value, B's title and
    /// full title only, its progress 40 throughout, reading `load.percent`
    /// 40 and starting the progress timer once, which then ages; no play
    /// frame is built meanwhile.
    #[test]
    fn a_stalled_load_keeps_drawing_its_lifetime() {
        let (link, rec) = Recording::new();
        let dir = tempfile::tempdir().unwrap();
        let (skin, _) = crate::render_decl::bundle(
            &crate::render_common::offscreen(1_920, 1_080),
            dir.path(),
            &bridge_look(),
        );
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        link.load(Arc::clone(&chart), None).unwrap();
        assert_eq!(
            link.select(ScreenKind::Play, Some(skin)).wait().unwrap(),
            Preparation::Ready
        );

        link.begin_loading(&chosen_by("A", Some("a")), None);
        link.loading()
            .report(LoadInput::Stage(Stage::Chart), Some(one_line("A")));
        let a_end = rec.until(0, |s| s.red.is_some_and(|r| r.artist));
        link.end_loading().unwrap();

        let gauge = crate::loading_state::shown_gauge(&rules::tapestry(), 0);
        link.begin_loading(&chosen("B"), gauge);
        let sink = link.loading();
        sink.report(LoadInput::Stage(Stage::Chart), Some(one_line("B")));
        let four = Stage::Sounds { done: 4, files: 9 };
        sink.report(LoadInput::Stage(four), Some(one_line("B 4/9")));
        let forty = rec.until(a_end, |s| s.load.percent == 40);
        // Stalled: no report for the rest, the progress timer ageing 200 ms.
        let stalled = rec.more(forty, 10);
        assert!(wait_until(|| rec.shots().last().is_some_and(|s| s
            .progress
            .is_some_and(|p| s.now.0 - p.0 > 200_000))));
        let end = rec.more(stalled, 3);
        link.end_loading().unwrap();
        link.exit();

        let shots = rec.shots();
        let a = shots[0].lifetime;
        let b = shots[end - 1].lifetime;
        assert_ne!(a, b);
        let lifetime_b: Vec<&Shot> = shots.iter().filter(|s| s.lifetime == b).collect();
        let first = lifetime_b[0];
        // B's origin is its own, later than A's, stamped before its first
        // frame and kept by every frame.
        assert!(first.load.since > shots[0].load.since, "{shots:?}");
        assert!(first.load.since <= first.now);
        assert!(lifetime_b.iter().all(|s| s.load.since == first.load.since));
        let when = first
            .when
            .expect("the when timer starts on B's first frame");
        assert!(lifetime_b.iter().all(|s| s.when == Some(when)), "{shots:?}");
        assert!(lifetime_b.iter().all(|s| s.gauge == gauge), "{shots:?}");
        // From the 40 % frame on, the progress stays and its timer starts
        // once, then ages.
        let from_forty: Vec<&Shot> = lifetime_b
            .iter()
            .copied()
            .skip_while(|s| s.load.percent != 40)
            .collect();
        assert!(from_forty.len() >= 10, "{shots:?}");
        assert!(from_forty.iter().all(|s| s.load.percent == 40), "{shots:?}");
        let started = from_forty[0]
            .progress
            .expect("the progress timer starts at 40");
        assert!(from_forty.iter().all(|s| s.progress == Some(started)));
        assert!(
            lifetime_b
                .iter()
                .take_while(|s| s.load.percent != 40)
                .all(|s| s.progress.is_none() && s.load.percent == 0)
        );
        let last = from_forty.last().unwrap();
        assert!(last.now.0 - started.0 > 200_000);
        // What each frame drew.
        for s in &from_forty {
            let r = s.red.unwrap();
            assert!(r.background && r.line && r.gauge, "{s:?}");
            assert!(r.percent && r.progress, "{s:?}");
            assert!(!r.artist && !r.genre, "{s:?}");
            assert!(r.animation_now, "{s:?}");
        }
        assert!(!last.red.unwrap().animation_start, "{last:?}");
        // B's title, once its text is installed, and never A's artist.
        assert!(
            lifetime_b.iter().all(|s| !s.red.unwrap().artist),
            "{shots:?}"
        );
        let titled = lifetime_b
            .iter()
            .skip_while(|s| !s.red.unwrap().title)
            .collect::<Vec<_>>();
        assert!(titled.len() >= 10, "{shots:?}");
        assert!(titled.iter().all(|s| {
            let r = s.red.unwrap();
            r.title && r.full_title
        }));
        assert_eq!(rec.builds.load(Ordering::SeqCst), 0);
    }

    /// Within one lifetime, through a real menu screen: the preset's
    /// refresh ([`crate::play::refresh_loading_gauge`], as the attempt's
    /// preparation calls it) replaces the gauge's descriptor from the
    /// next frame on,
    /// kept by later pages, the loading clock and the `when` timer going
    /// on; a field's change clears its slots on the frame it arrives and
    /// draws the replacement from the next, the other fields kept; an
    /// unavailable field clears its slots; the staged text is counted in
    /// the renderer's memory until installed.
    #[test]
    fn a_lifetimes_descriptor_and_chart_text_change_in_place() {
        let (link, rec, text, _dir) = bridge_recording();
        link.begin_loading(&chosen_by("T", Some("x")), None);
        let sink = link.loading();
        sink.report(LoadInput::Stage(Stage::Chart), Some(one_line("T")));
        let shown = rec.until(0, |s| s.red.is_some_and(|r| r.title && r.artist));
        let hard = crate::loading_state::shown_gauge(&rules::tapestry(), 1);
        assert!(hard.is_some());
        crate::play::refresh_loading_gauge(
            &link,
            true,
            crate::loading_state::shown_gauge(&rules::tapestry(), 1),
        );
        let refreshed = rec.until(shown, |s| s.gauge == hard);
        sink.report(LoadInput::Stage(Stage::Loudness), Some(one_line("later")));
        let later = rec.more(refreshed, 3);
        let y = ChosenChart {
            title: Some("T".to_owned()),
            artist: Some("y".to_owned()),
            ..ChosenChart::default()
        };
        sink.report(LoadInput::Chart(y), None);
        let changed = rec.until(later, |s| s.text == 2);
        let replaced = rec.until(changed, |s| s.text == 2 && s.red.unwrap().artist);
        let gone = ChosenChart {
            title: Some("T".to_owned()),
            ..ChosenChart::default()
        };
        sink.report(LoadInput::Chart(gone), None);
        let cleared = rec.until(replaced, |s| s.text == 3);
        let end = rec.more(cleared, 3);
        link.end_loading().unwrap();
        link.exit();

        let shots = rec.shots();
        let all = &shots[..end];
        assert!(all.iter().all(|s| s.lifetime == all[0].lifetime));
        let since = all[0].load.since;
        assert!(all.iter().all(|s| s.load.since == since), "{all:?}");
        let when = all[0].when.unwrap();
        assert!(all.iter().all(|s| s.when == Some(when)), "{all:?}");
        // The descriptor: none, then the refreshed one, in one step, kept.
        let first_hard = all.iter().position(|s| s.gauge == hard).unwrap();
        assert!(all[..first_hard].iter().all(|s| s.gauge.is_none()));
        assert!(all[first_hard..].iter().all(|s| s.gauge == hard), "{all:?}");
        assert!(all[..first_hard].iter().all(|s| !s.red.unwrap().gauge));
        assert!(all[first_hard..].iter().all(|s| s.red.unwrap().gauge));
        // Revision 2: the artist's slot cleared on its first frame, the
        // title kept; from the next frame on its replacement.
        let r2: Vec<&Shot> = all.iter().filter(|s| s.text == 2).collect();
        assert!(
            !r2[0].red.unwrap().artist && r2[0].red.unwrap().title,
            "{r2:?}"
        );
        assert!(r2[0].staged > 0, "{r2:?}");
        assert!(r2[1..].iter().all(|s| {
            let r = s.red.unwrap();
            r.artist && r.title && s.staged == 0
        }));
        // Revision 3: the artist unavailable, its slot empty; the title
        // kept throughout.
        let r3: Vec<&Shot> = all.iter().filter(|s| s.text == 3).collect();
        assert!(r3.iter().all(|s| {
            let r = s.red.unwrap();
            !r.artist && r.title
        }));
        // Each revision's new lines laid out once by the producer: the
        // first's title (the full title the same line) and artist, then
        // only the changed artist; the kept title is never asked again,
        // and the unavailable artist has no line.
        assert_eq!(*text.strings.lock().unwrap(), ["T", "x", "y"]);
    }

    /// A render thread on [`Recorder`]s with the fixture of every loading
    /// part selected, and its text producer.
    fn bridge_recording() -> (
        super::RenderLink,
        Recording,
        Arc<crate::render_decl::Text>,
        tempfile::TempDir,
    ) {
        let (link, rec) = Recording::new();
        let dir = tempfile::tempdir().unwrap();
        let (skin, text) = crate::render_decl::bundle(
            &crate::render_common::offscreen(1_920, 1_080),
            dir.path(),
            &bridge_look(),
        );
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        link.load(chart, None).unwrap();
        assert_eq!(
            link.select(ScreenKind::Play, Some(skin)).wait().unwrap(),
            Preparation::Ready
        );
        (link, rec, text.unwrap(), dir)
    }

    /// Text installed at one size and a field that became unavailable
    /// arriving with another size: the slots are prepared again for the
    /// new size, and the dropped artist is never drawn from the text the
    /// slots kept, on the frame the change arrives or after.
    #[test]
    fn a_new_size_never_draws_a_dropped_field() {
        let (link, rec, _text, _dir) = bridge_recording();
        link.begin_loading(&chosen_by("T", Some("x")), None);
        let sink = link.loading();
        sink.report(LoadInput::Stage(Stage::Chart), Some(one_line("T")));
        let shown = rec.until(0, |s| s.red.is_some_and(|r| r.title && r.artist));
        // Installed at 960 × 540: a frame there with nothing staged.
        *rec.resize.lock().unwrap() = Some((None, (960, 540)));
        let small = rec.until(shown, |s| s.target == [960, 540] && s.staged == 0);
        // The artist dropped, the size back on the same frame.
        *rec.resize.lock().unwrap() = Some((Some(2), (1_920, 1_080)));
        sink.report(
            LoadInput::Chart(ChosenChart {
                title: Some("T".to_owned()),
                ..ChosenChart::default()
            }),
            None,
        );
        let end = rec.until(small, |s| s.text == 2 && s.red.is_some_and(|r| r.title));
        link.end_loading().unwrap();
        link.exit();

        let shots = rec.shots();
        let r2: Vec<&Shot> = shots[..end].iter().filter(|s| s.text == 2).collect();
        assert!(r2.iter().all(|s| s.target == [1_920, 1_080]), "{r2:?}");
        assert!(r2.iter().all(|s| !s.red.unwrap().artist), "{r2:?}");
    }

    /// A revision the producer refuses leaves its fields empty and is not
    /// asked again; the next revision lays out every field again, the
    /// refused ones included, so none stays empty for good.
    #[test]
    fn a_refused_revision_is_laid_out_again_by_the_next() {
        let (link, rec, text, _dir) = bridge_recording();
        link.begin_loading(&chosen_by("T", Some("x")), None);
        let sink = link.loading();
        sink.report(LoadInput::Stage(Stage::Chart), Some(one_line("T")));
        let shown = rec.until(0, |s| s.red.is_some_and(|r| r.title && r.artist));
        text.refuse.store(true, Ordering::SeqCst);
        sink.report(
            LoadInput::Chart(ChosenChart {
                title: Some("T".to_owned()),
                artist: Some("y".to_owned()),
                ..ChosenChart::default()
            }),
            None,
        );
        // Two frames of the refused revision: its preparation ran on the
        // first, and the second shows it was not asked again.
        let refused = rec.until(shown, |s| s.text == 2);
        let refused = rec.until(refused, |s| s.text == 2);
        let asked = text.rasters.load(Ordering::SeqCst);
        text.refuse.store(false, Ordering::SeqCst);
        sink.report(
            LoadInput::Chart(ChosenChart {
                title: Some("T".to_owned()),
                artist: Some("y".to_owned()),
                genre: Some("g".to_owned()),
                ..ChosenChart::default()
            }),
            None,
        );
        let end = rec.until(refused, |s| {
            s.text == 3 && s.red.is_some_and(|r| r.title && r.artist && r.genre)
        });
        link.end_loading().unwrap();
        link.exit();

        let shots = rec.shots();
        let r2: Vec<&Shot> = shots[..end].iter().filter(|s| s.text == 2).collect();
        assert!(r2.len() >= 2, "{r2:?}");
        assert!(r2.iter().all(|s| !s.red.unwrap().artist), "{r2:?}");
        // The refused artist rasterized once, then laid out again with the
        // genre; the title, still in the text layer, is a hit.
        assert_eq!(
            asked, 3,
            "the first revision's two lines and the refused one"
        );
        assert_eq!(*text.strings.lock().unwrap(), ["T", "x", "y", "y", "g"]);
    }

    /// A renderer built again restores the installation, its presentation
    /// with its slots' lines laid out on the replaced renderer: on the
    /// first loading frame of the new renderer every slot is cleared before
    /// it is built, though the new text layer places the lines prepared
    /// there at the coordinates the old lines had, so a kept slot would draw
    /// another field's line (the genre where the artist was). The `when`
    /// timer keeps its start, and the next frame draws the new text.
    #[test]
    fn restore_clears_before_build() {
        let (link, rec, _text, _dir) = bridge_recording();
        link.begin_loading(&chosen_by("T", Some("x")), None);
        let sink = link.loading();
        sink.report(LoadInput::Stage(Stage::Chart), Some(one_line("T")));
        let shown = rec.until(0, |s| s.red.is_some_and(|r| r.title && r.artist));
        rec.fail.store(true, Ordering::Release);
        assert!(wait_until(|| link.failed()));
        // The artist gone and a genre come while no renderer draws.
        sink.report(
            LoadInput::Chart(ChosenChart {
                title: Some("T".to_owned()),
                genre: Some("g".to_owned()),
                ..ChosenChart::default()
            }),
            None,
        );
        rec.fail.store(false, Ordering::Release);
        link.prepare().unwrap();
        let first_device = rec.shots()[0].device;
        let end = rec.until(shown, |s| {
            s.device != first_device && s.red.is_some_and(|r| r.title && r.genre)
        });
        link.end_loading().unwrap();
        link.exit();

        let shots = rec.shots();
        let when = shots[0].when.unwrap();
        let rebuilt: Vec<&Shot> = shots[..end]
            .iter()
            .filter(|s| s.device != first_device)
            .collect();
        assert!(rebuilt.len() >= 2, "{rebuilt:?}");
        let first = rebuilt[0].red.unwrap();
        assert!(
            !first.title && !first.artist && !first.genre,
            "{:?}",
            rebuilt[0]
        );
        assert!(
            rebuilt.iter().all(|s| !s.red.unwrap().artist),
            "{rebuilt:?}"
        );
        assert!(
            shots[..end].iter().all(|s| s.when == Some(when)),
            "{shots:?}"
        );
    }

    /// The device failing while the loading text's upload is waited for
    /// is not a refused text: the screen is dropped as after any device
    /// failure, its text layer's cache closed with it, and the screen built
    /// again prepares the chosen chart's text afresh and draws it.
    #[cfg(feature = "dev-tools")]
    #[test]
    fn a_device_failure_preparing_loading_text_drops_the_screen() {
        let (link, rec, _text, _dir) = bridge_recording();
        link.begin_loading(&chosen_by("T", Some("x")), None);
        let sink = link.loading();
        sink.report(LoadInput::Stage(Stage::Chart), Some(one_line("T")));
        let shown = rec.until(0, |s| s.red.is_some_and(|r| r.title && r.artist));
        let first_device = rec.shots()[0].device;
        rec.text_fault.store(true, Ordering::Release);
        sink.report(
            LoadInput::Chart(ChosenChart {
                title: Some("T".to_owned()),
                artist: Some("y".to_owned()),
                ..ChosenChart::default()
            }),
            None,
        );
        assert!(wait_until(|| link.failed()), "the screen was kept");
        link.prepare().unwrap();
        sink.report(LoadInput::Stage(Stage::Loudness), Some(one_line("T")));
        rec.until(shown, |s| {
            s.device != first_device && s.red.is_some_and(|r| r.title && r.artist)
        });
        link.end_loading().unwrap();
        link.exit();
        assert_eq!(rec.made.load(Ordering::SeqCst), 2);
    }

    /// A refusal at one size is forgotten once a later preparation
    /// installs: text installed at 1920 × 1080, refused at 960 × 540, then
    /// installed again at 1920 × 1080; back at 960 × 540 the slots are
    /// prepared for that size again, never kept with the larger size's
    /// lines.
    #[test]
    fn a_refusal_is_forgotten_once_text_installs() {
        let (link, rec, text, _dir) = bridge_recording();
        link.begin_loading(&chosen_by("T", Some("x")), None);
        let sink = link.loading();
        sink.report(LoadInput::Stage(Stage::Chart), Some(one_line("T")));
        let shown = rec.until(0, |s| s.red.is_some_and(|r| r.title && r.artist));
        text.refuse.store(true, Ordering::SeqCst);
        *rec.resize.lock().unwrap() = Some((None, (960, 540)));
        let small = rec.until(shown, |s| s.target == [960, 540]);
        let refused = rec.more(small, 2);
        text.refuse.store(false, Ordering::SeqCst);
        *rec.resize.lock().unwrap() = Some((None, (1_920, 1_080)));
        let back = rec.until(refused, |s| {
            s.target == [1_920, 1_080] && s.staged == 0 && s.red.is_some_and(|r| r.title)
        });
        let laid = text.strings.lock().unwrap().len();
        *rec.resize.lock().unwrap() = Some((None, (960, 540)));
        let again = rec.until(back, |s| s.target == [960, 540]);
        let end = rec.more(again, 3);
        link.end_loading().unwrap();
        link.exit();

        let shots = rec.shots();
        let at_small: Vec<&Shot> = shots[back..end]
            .iter()
            .filter(|s| s.target == [960, 540])
            .collect();
        assert!(at_small.iter().any(|s| s.staged > 0), "{at_small:?}");
        assert!(text.strings.lock().unwrap().len() > laid);
    }

    /// Within one lifetime the loading clock and the `when` timer go on
    /// across a page and progress update, the same bundle prepared and
    /// committed for the attempt, a resize and a renderer built again; a
    /// different bundle committed within it keeps the clock but starts
    /// its own presentation's timer afresh, its text installed again; the
    /// next lifetime starts both afresh. A report of an ended lifetime
    /// draws nothing. The attempt's commit ([`crate::play::commit_ready`])
    /// makes the lifetime ready, 100 drawn, before the end of loading is
    /// answered, and no loading frame follows the answer; no play frame
    /// is built while loading.
    #[test]
    fn a_lifetimes_clock_survives_what_happens_during_it() {
        let (link, rec) = Recording::new();
        let dir = tempfile::tempdir().unwrap();
        let other_dir = tempfile::tempdir().unwrap();
        let r = crate::render_common::offscreen(1_920, 1_080);
        let (skin, _) = crate::render_decl::bundle(&r, dir.path(), &bridge_look());
        let (other, _) = crate::render_decl::bundle(&r, other_dir.path(), &bridge_look());
        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        link.load(Arc::clone(&chart), None).unwrap();
        assert_eq!(
            link.select(ScreenKind::Play, Some(Arc::clone(&skin)))
                .wait()
                .unwrap(),
            Preparation::Ready
        );

        link.begin_loading(&chosen("A"), None);
        let sink = link.loading();
        sink.report(LoadInput::Stage(Stage::Chart), Some(one_line("A")));
        let shown = rec.until(0, |s| s.red.is_some_and(|r| r.title));
        let four = Stage::Sounds { done: 4, files: 9 };
        sink.report(LoadInput::Stage(four), Some(one_line("A 4/9")));
        let progressed = rec.until(shown, |s| s.load.percent == 40);
        // The same bundle prepared and committed for the attempt.
        let attempt = preset_attempt(&chart, &rules::tapestry(), None, "T");
        assert_eq!(
            link.prepare_attempt(attempt, || false).unwrap(),
            Preparation::Ready
        );
        link.commit_attempt().unwrap();
        let committed = rec.more(progressed, 3);
        // A resize, drawn at the new size, and back.
        *rec.resize.lock().unwrap() = Some((None, (960, 540)));
        let small = rec.until(committed, |s| s.target == [960, 540]);
        *rec.resize.lock().unwrap() = Some((None, (1_920, 1_080)));
        let resized = rec.until(small, |s| {
            s.target == [1_920, 1_080] && s.red.is_some_and(|r| r.title)
        });
        // The renderer built again, its files gone.
        rec.fail.store(true, Ordering::Release);
        assert!(wait_until(|| link.failed()));
        rec.fail.store(false, Ordering::Release);
        link.prepare().unwrap();
        sink.report(LoadInput::BankComplete, Some(one_line("A bank")));
        let rebuilt = rec.until(resized, |s| {
            s.load.percent == 90 && s.red.is_some_and(|r| r.title)
        });
        // A different bundle committed in the same lifetime.
        assert_eq!(
            link.select(ScreenKind::Play, Some(Arc::clone(&other)))
                .wait()
                .unwrap(),
            Preparation::Ready
        );
        let attempt = preset_attempt(&chart, &rules::tapestry(), None, "T");
        assert_eq!(
            link.prepare_attempt(attempt, || false).unwrap(),
            Preparation::Ready
        );
        link.commit_attempt().unwrap();
        let other_at = Arc::as_ptr(&other) as *const () as usize;
        let switched = rec.until(rebuilt, |s| {
            s.bundle == Some(other_at) && s.red.is_some_and(|r| r.title)
        });
        // The lifetime cancelled: its late reports draw nothing.
        let a = rec.shots()[0].lifetime;
        link.clear_loading();
        sink.report(LoadInput::Ready, Some(one_line("late")));
        sink.report(LoadInput::BankComplete, None);
        // The next lifetime, ready through the attempt's commit.
        link.begin_loading(&chosen("B"), None);
        link.loading()
            .report(LoadInput::Stage(Stage::Chart), Some(one_line("B")));
        let next = rec.until(switched, |s| {
            s.lifetime != a && s.red.is_some_and(|r| r.title)
        });
        let attempt = preset_attempt(&chart, &rules::tapestry(), None, "T");
        assert_eq!(
            link.prepare_attempt(attempt, || false).unwrap(),
            Preparation::Ready
        );
        crate::play::commit_ready(&link, true, &mut super::Declared::default()).unwrap();
        let ready = rec.until(next, |s| s.load.percent == 100);
        link.end_loading().unwrap();
        let ended = rec.count();
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(rec.count(), ended, "a loading frame after the answer");
        link.exit();

        let shots = rec.shots();
        let a = shots[0].lifetime;
        let lifetime_a: Vec<&Shot> = shots.iter().filter(|s| s.lifetime == a).collect();
        let since = lifetime_a[0].load.since;
        assert!(
            lifetime_a.iter().all(|s| s.load.since == since),
            "{shots:?}"
        );
        assert!(lifetime_a.iter().all(|s| s.load.percent < 100), "{shots:?}");
        // Two devices, two bundles, one origin.
        assert!(lifetime_a.iter().any(|s| s.device != lifetime_a[0].device));
        let skin_at = Arc::as_ptr(&skin) as *const () as usize;
        let (on_skin, on_other): (Vec<&Shot>, Vec<&Shot>) = lifetime_a
            .iter()
            .copied()
            .partition(|s| s.bundle == Some(skin_at));
        assert!(on_other.iter().all(|s| s.bundle == Some(other_at)));
        assert!(!on_other.is_empty());
        let when = on_skin[0].when.unwrap();
        assert!(on_skin.iter().all(|s| s.when == Some(when)), "{shots:?}");
        let other_when = on_other[0].when.unwrap();
        assert!(other_when > when, "{shots:?}");
        assert!(on_other.iter().all(|s| s.when == Some(other_when)));
        assert!(
            shots[..switched]
                .iter()
                .skip_while(|s| s.bundle != Some(other_at))
                .all(|s| s.bundle == Some(other_at))
        );
        // Frames were drawn at the smaller size, on the same clock.
        assert!(shots[committed..resized].iter().any(|s| s.red.is_none()));
        // The next lifetime, after every frame of the cancelled one: its
        // own origin and timer, then readiness's 100.
        let first_b = shots.iter().position(|s| s.lifetime != a).unwrap();
        assert!(
            shots[first_b..].iter().all(|s| s.lifetime != a),
            "{shots:?}"
        );
        let lifetime_b: Vec<&Shot> = shots[first_b..].iter().collect();
        let since_b = lifetime_b[0].load.since;
        assert!(since_b > since);
        assert!(lifetime_b.iter().all(|s| s.load.since == since_b));
        let when_b = lifetime_b[0].when.unwrap();
        assert!(when_b > other_when);
        assert!(lifetime_b.iter().all(|s| s.when == Some(when_b)));
        assert!(shots[..next].iter().all(|s| s.load.percent < 100));
        assert_eq!(shots[ready - 1].load.percent, 100);
        assert_eq!(rec.builds.load(Ordering::SeqCst), 0);
        assert_eq!(rec.made.load(Ordering::SeqCst), 2);
    }

    /// The slot takes a whole newer state over the one waiting, so a
    /// sound's progress coalescing after the bank's completion still
    /// shows 90; a publication not newer than one accepted, taken or
    /// still waiting, is refused with its page, the waiting state and
    /// page unchanged; another lifetime's is refused; the next lifetime
    /// accepts from its first number.
    #[test]
    fn the_loading_slot_takes_only_newer_states() {
        use super::LoadingSlot;

        let text = chosen("C");
        let mut state = crate::loading_state::LifetimeState::new(0, TimeUs(5), &text, None);
        let mut slot = LoadingSlot::default();
        let four = state.apply(LoadInput::Stage(Stage::Sounds { done: 4, files: 9 }));
        let bank = state.apply(LoadInput::BankComplete);
        let screen = state.apply(LoadInput::Stage(Stage::Screen));
        assert!(slot.offer(0, Some(one_line("4")), Some(Box::new(four.clone()))));
        assert!(slot.offer(0, None, Some(Box::new(bank))));
        assert!(slot.offer(0, Some(one_line("screen")), Some(Box::new(screen))));
        let (page, update) = slot.take(0).unwrap();
        let update = update.unwrap();
        assert_eq!(
            (update.seq, update.percent, update.stage),
            (3, 90, Stage::Screen)
        );
        assert_eq!(page.unwrap().lines[0].text, "screen");
        // Older than the one taken.
        assert!(!slot.offer(0, Some(one_line("old")), Some(Box::new(four.clone()))));
        assert!(slot.take(0).is_none());
        // Newer than the one taken but older than the one waiting: neither
        // the state nor the page waiting changes.
        let between = state.apply(LoadInput::Stage(Stage::Loudness));
        let newer = state.apply(LoadInput::Stage(Stage::Scan));
        assert!(slot.offer(0, Some(one_line("newer")), Some(Box::new(newer))));
        assert!(!slot.offer(0, Some(one_line("old")), Some(Box::new(between))));
        let (page, update) = slot.take(0).unwrap();
        assert_eq!(update.unwrap().seq, 5);
        assert_eq!(page.unwrap().lines[0].text, "newer");
        // A page alone of the lifetime.
        assert!(slot.offer(0, Some(one_line("page")), None));
        assert!(slot.take(0).unwrap().1.is_none());
        // Another lifetime's, then the next lifetime's.
        assert!(!slot.offer(1, None, Some(Box::new(four.clone()))));
        slot.end();
        assert!(slot.take(0).is_none());
        assert!(slot.offer(1, None, Some(Box::new(four))));
    }

    /// The render thread applies only a newer state over the one its
    /// loading frames are built from, keeping its page when the take
    /// brought none; a take of another lifetime enters it.
    #[test]
    fn the_render_thread_applies_only_newer_states() {
        use super::{Mode, take_loading};

        let text = chosen("C");
        let mut state = crate::loading_state::LifetimeState::new(0, TimeUs(5), &text, None);
        let one = Box::new(state.apply(LoadInput::Stage(Stage::Chart)));
        let two = Box::new(state.apply(LoadInput::BankComplete));
        let mode = take_loading(Mode::Idle, 0, Some(Box::new(one_line("A"))), Some(two));
        let mode = take_loading(mode, 0, None, Some(one.clone()));
        let Mode::Menu {
            page,
            update,
            loading,
            ..
        } = &mode
        else {
            panic!("not the loading state");
        };
        assert_eq!((update.as_ref().unwrap().seq, *loading), (2, Some(0)));
        assert_eq!(page.lines[0].text, "A");
        let mode = take_loading(mode, 1, None, Some(one));
        let Mode::Menu {
            page,
            update,
            loading,
            ..
        } = &mode
        else {
            panic!("not the loading state");
        };
        assert_eq!((update.as_ref().unwrap().seq, *loading), (1, Some(1)));
        assert!(page.lines.is_empty());
    }

    /// The attempt of `chart` the game would make under `preset`: the
    /// whole chart or `section`'s round, titled `title`.
    fn preset_attempt(
        chart: &Chart,
        preset: &rules::RulePreset,
        section: Option<&eval::Section>,
        title: &str,
    ) -> AttemptContext {
        let labels = game::ChartLabels {
            title: title.to_owned(),
            ..game::ChartLabels::default()
        };
        let attempt = game::Attempt::new(
            1,
            preset,
            TimeUs(1_000_000),
            section,
            game::NoteEnd::Play.of(chart),
            &labels,
            chart,
        );
        AttemptContext::new(attempt, crate::attempt_context::SIMPLE_SCRATCH, 1, false)
    }

    /// The selection installs the bundle, uploading it once, and its
    /// commit draws nothing without an attempt; every attempt reuses its
    /// resources. Through the render thread's own screen drop and
    /// recovery, a real menu screen built again restores the committed
    /// bundle from memory, its image files deleted: installed at once and
    /// drawing nothing without an attempt, its resources reused by every
    /// later attempt.
    /// Each attempt on it starts fresh from its own preset and labels:
    /// - a healthy attempt shows its judgment (the judgment marker) and
    ///   its title's text line;
    /// - the next, counting empty POORs, refuses the frames of a snapshot
    ///   with more of them than POORs;
    /// - the retry after it, its gauges reordered so the shown gauge 0 is
    ///   the survival gauge (the gauge-look marker), empty POORs apart and
    ///   untitled, draws neither the judgment nor a text line;
    /// - a practice round of a one-gauge preset prepares with one initial
    ///   gauge, which the two-gauge preset before it would refuse, and
    ///   shows its judgment and its line; the round again, untitled and
    ///   unjudged, shows neither.
    ///
    /// A refused text refuses the attempt with the producer's own reason.
    #[test]
    fn a_menu_screen_built_again_restores_the_bundle_and_starts_each_attempt_afresh() {
        use crate::render_decl::{Look, bundle};

        let chart = Arc::new(ChartBuilder::new(120_000).build().unwrap());
        let dir = tempfile::tempdir().unwrap();
        let look = Look {
            text: true,
            markers: true,
            ..Look::left([255, 0, 0, 255])
        };
        let (skin, text) = bundle(
            &crate::render_common::offscreen(1_920, 1_080),
            dir.path(),
            &look,
        );
        let text = text.unwrap();
        let fail = Arc::new(AtomicBool::new(false));
        let judge = Arc::new(AtomicBool::new(false));
        let gauges = Arc::new(std::sync::atomic::AtomicUsize::new(2));
        let probed = Arc::new(std::sync::Mutex::new(Probed::default()));
        let builds = Arc::new(AtomicU32::new(0));
        let (f, j, g, p, b) = (
            Arc::clone(&fail),
            Arc::clone(&judge),
            Arc::clone(&gauges),
            Arc::clone(&probed),
            Arc::clone(&builds),
        );
        let make: MakeScreen = Box::new(move || {
            b.fetch_add(1, Ordering::SeqCst);
            let renderer = render::Renderer::new(
                render::Target::Offscreen {
                    width: 1_920,
                    height: 1_080,
                },
                render::AdapterChoice::Warp,
            )?;
            Ok(Box::new(Probe {
                menu: crate::menu_screen::MenuScreen::new(renderer, None),
                fail: Arc::clone(&f),
                judge: Arc::clone(&j),
                gauges: Arc::clone(&g),
                probed: Arc::clone(&p),
            }) as Box<dyn Screen>)
        });
        let link = spawn_empty(make);
        link.load(Arc::clone(&chart), None).unwrap();
        assert_eq!(
            link.select(ScreenKind::Play, Some(Arc::clone(&skin)))
                .wait()
                .unwrap(),
            Preparation::Ready
        );
        let play = |preset: &rules::RulePreset,
                    section: Option<&eval::Section>,
                    title: &str,
                    judged: bool| {
            judge.store(judged, Ordering::Release);
            let attempt = preset_attempt(&chart, preset, section, title);
            assert_eq!(
                link.prepare_attempt(attempt, || false).unwrap(),
                Preparation::Ready
            );
            link.commit_attempt().unwrap();
        };
        let tapestry = rules::tapestry();

        // The first attempt draws the bundle the selection uploaded.
        play(&tapestry, None, "T", false);
        dir.close().unwrap();

        // The screen fails and is dropped; the next attempt builds another.
        fail.store(true, Ordering::Release);
        link.send(RenderMsg::Menu(Box::default()));
        assert!(wait_until(|| link.failed()));
        fail.store(false, Ordering::Release);
        play(&tapestry, None, "T", true);
        assert_eq!(builds.load(Ordering::SeqCst), 2);

        let mut counted = rules::tapestry();
        counted.empty_poor.counted = true;
        play(&counted, None, "T", false);

        let mut reordered = rules::tapestry();
        reordered.gauges.reverse();
        play(&reordered, None, "", false);

        let mut one = rules::tapestry();
        one.gauges.truncate(1);
        gauges.store(1, Ordering::Release);
        let round = eval::Section {
            start: TimeUs(0),
            end: TimeUs(1_000_000),
            lead_in: TimeUs(2_000_000),
            initial_gauges: vec![rules::GaugeValue(20 * rules::GaugeValue::PERCENT)],
        };
        play(&one, Some(&round), "T", true);
        play(&one, Some(&round), "", false);
        // The title laid out once on each renderer's text layer, every
        // later attempt of it a hit.
        assert_eq!(text.calls.load(Ordering::SeqCst), 2, "each renderer's line");

        // A refused text, of a title not yet placed: the producer's reason
        // reaches the refusal.
        text.refuse.store(true, Ordering::SeqCst);
        let attempt = preset_attempt(&chart, &tapestry, None, "U");
        let refused = link.prepare_attempt(attempt, || false).unwrap();
        let Preparation::Refused(refusal) = refused else {
            panic!("{refused:?}");
        };
        assert!(
            refusal.reason.contains("the text cannot be prepared"),
            "{}",
            refusal.reason
        );
        assert!(!link.failed());
        link.exit();
        assert_eq!(builds.load(Ordering::SeqCst), 2);

        let probed = probed.lock().unwrap();
        // The second screen restored the very bundle, files gone, before
        // anything else: installed, and nothing drawn without an attempt.
        let [(same, restored, drew)] = &probed.restores[..] else {
            panic!("{:?}", probed.restores);
        };
        assert!(same, "the restored bundle is not installed");
        assert!(
            matches!(drew, Ok((0, [BLACK, BLACK, BLACK, BLACK]))),
            "{drew:?}"
        );
        // Uploaded once per screen, by the selection and the restore: the
        // attempts reuse their resources.
        let [installed] = probed.installs[..] else {
            panic!("{:?}", probed.installs);
        };
        assert!(installed > 0);
        assert_eq!(probed.staged_atlas, [0; 6]);
        let generations: Vec<u64> = probed.commits.iter().map(|c| c.0).collect();
        assert_eq!(generations[0], generations[1]);
        assert_eq!(generations[2..], [*restored; 5]);
        // The selection's commit draws nothing without an attempt.
        assert!(
            matches!(&probed.commits[0].1, Ok((0, [BLACK, BLACK, BLACK, BLACK]))),
            "{:?}",
            probed.commits[0].1
        );
        // Each attempt's commit's frame: the sprites (the image, the text
        // line, the gauge-look marker, the judgment marker) and the
        // samples (left, right, gauge look, judgment).
        let drew: Vec<&Probing> = probed.commits[1..].iter().map(|c| &c.1).collect();
        let expect = |i: usize, sprites: usize, gauge: [u8; 3], judgment: [u8; 3]| {
            let want = (sprites, [RED, BLACK, gauge, judgment]);
            assert!(
                matches!(drew[i], Ok(got) if *got == want),
                "attempt {}: {:?}, not {want:?}",
                i + 1,
                drew[i]
            );
        };
        expect(0, 2, BLACK, BLACK);
        expect(1, 3, BLACK, RED);
        assert!(
            matches!(
                drew[2],
                Err(RenderError::Build(
                    skin_model::BuildError::InconsistentTally
                ))
            ),
            "{:?}",
            drew[2]
        );
        expect(3, 2, RED, BLACK);
        expect(4, 3, BLACK, RED);
        expect(5, 1, BLACK, BLACK);
    }

    // The bridge over an imported skin with bitmap text.
    mod bitmap;
    // Dropped effects and rearms through the acquisition into both skins.
    mod note_snapshot;
    // Lane input through the acquisition into both skins' key beams and
    // key timers.
    mod lane_input;
    // The select screen's updates through the slot into the skin or the
    // page.
    mod selection;
    // The chart images' tables: their generation, attempt and target.
    mod chart;
    // The decision labels' preparation, the skinned decision screen, its
    // takeover and the kind's refusal for the run.
    #[cfg(feature = "dev-tools")]
    mod decision;
    // The result screen's presentations through the slot, the commit
    // point and the barrier into the skin or the built-in page.
    #[cfg(feature = "dev-tools")]
    pub(crate) mod result;
}
