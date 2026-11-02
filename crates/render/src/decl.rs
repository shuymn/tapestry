//! The declarative skin on the play screen (design.md "Rendering and
//! Skins"): [`DeclSkin`], a compiled `skin_model::Skin` drawing one
//! attempt's fresh `Play` into the batch directly, and [`SkinBundle`], the
//! CPU description of everything a skin's installation uploads: the
//! compiled skin, its atlas pages with the matching uv table, its fixed
//! clips, its scratch side and the producer of its attempts' prepared
//! text.
//!
//! A bundle is immutable and shared (`Arc`): the renderer keeps the one it
//! installed, and the render thread keeps the committed one apart from the
//! renderer, so a renderer built again after a device failure restores
//! exactly that bundle from memory, never from the files it came from.
//!
//! Preparation, the `Play`, its bitmap lines and its prepared text, runs
//! before the countdown and allocates; a frame's [`PlaySkin::build`]
//! translates the [`FrameInput`] into a `SkinFrame` and builds into the
//! batch, allocating nothing.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bms_model::{Chart, TimeUs};
use eval::Section;
use skin_model::{
    Atlas, BuildError, Canvas, ChartText, CoverSettings, FrameReadouts, LaneView, Layout, Loading,
    Page, Play, Side, Skin, SkinFrame, TextTable, UvTable,
};

use crate::projection::ClipTable;
use crate::skin_atlas::{AtlasPages, PageLevels, RESERVED_LAYERS};
use crate::{AttemptInput, FrameInput, Gpu, LoadingFrame, PlaySkin, SkinError, SpriteBatch};

/// Prepares the rasterized text of one attempt: the installation seam a
/// text producer plugs into (bitmap text needs none: the skin lays its
/// lines out itself). It runs at the attempt's preparation, never in a
/// frame, and its table is attached to the attempt's fresh `Play`.
pub trait AttemptText: Send + Sync + fmt::Debug {
    /// The table of `chart`'s lines for `skin` on a `target`-pixel target,
    /// one per text slot ([`Skin::text_slots`]), or `None` to attach none.
    ///
    /// # Errors
    ///
    /// Why the attempt's text cannot be prepared: the attempt is refused.
    fn prepare(
        &self,
        skin: &Skin,
        chart: &ChartText<'_>,
        target: [u32; 2],
    ) -> Result<Option<TextTable>, SkinError>;
}

/// Why a bundle was not made.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BundleError {
    /// The atlas was built from another sheet than the skin's, or has
    /// another number of frames.
    #[error("the atlas is not the skin's: {frames} frames for {expected}")]
    Atlas {
        /// The atlas's frames.
        frames: usize,
        /// The skin's.
        expected: usize,
    },
    /// The atlas's frames do not lie on its pages after the renderer's
    /// reserved layers: it was built for another reservation.
    #[error("the atlas's frames lie on layers {first} to {last}, not {RESERVED_LAYERS} to {end}")]
    Layers {
        /// The lowest layer a frame lies on.
        first: u16,
        /// The highest.
        last: u16,
        /// The highest layer the pages fill after the reserved ones.
        end: u32,
    },
    /// The skin's elements name more fixed clips than the table has.
    #[error("the skin draws under {referenced} clips, the table has {clips}")]
    Clips {
        /// The clips the skin names ([`Skin::clip_count`]).
        referenced: usize,
        /// The table's.
        clips: usize,
    },
}

/// Everything a declarative skin's installation uploads, held on the CPU:
/// the compiled skin, its atlas's pages and the uv table that matches
/// them, its fixed clips, the scratch side its lanes are laid out for, and
/// the producer of its attempts' prepared text, if any. Its resident bytes
/// ([`resident_bytes`](Self::resident_bytes)) count against the memory
/// budget for as long as it is committed.
pub struct SkinBundle {
    skin: Arc<Skin>,
    side: u32,
    pages: Vec<Page>,
    uvs: UvTable,
    clips: ClipTable,
    scratch: Side,
    text: Option<Arc<dyn AttemptText>>,
}

impl fmt::Debug for SkinBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SkinBundle")
            .field("side", &self.side)
            .field("pages", &self.pages.len())
            .field("frames", &self.uvs.frames().len())
            .field("clips", &self.clips.len())
            .field("scratch", &self.scratch)
            .field("text", &self.text)
            .finish_non_exhaustive()
    }
}

impl SkinBundle {
    /// `skin` with its `atlas` (built from the skin's own sheet), `clips`
    /// and the lanes laid out for `scratch`, the side its importer gives.
    ///
    /// # Errors
    ///
    /// [`BundleError::Atlas`] for an atlas of another sheet or length,
    /// [`BundleError::Layers`] for one built for another reservation than
    /// the renderer's [`RESERVED_LAYERS`] (its pages are uploaded after
    /// them), and [`BundleError::Clips`] for a clip table without every
    /// clip the skin names.
    pub fn new(
        skin: Arc<Skin>,
        atlas: Atlas,
        clips: ClipTable,
        scratch: Side,
    ) -> Result<Self, BundleError> {
        let side = atlas.side();
        let (pages, uvs) = atlas.into_parts();
        if uvs.sheet_id() != skin.sheet().id() || uvs.frames().len() != skin.frame_count() {
            return Err(BundleError::Atlas {
                frames: uvs.frames().len(),
                expected: skin.frame_count(),
            });
        }
        // The packer fills its pages in order, so a frame lies on the
        // first one; none lies past the last.
        let layers = uvs.frames().iter().map(|f| f.layer);
        if let (Some(first), Some(last)) = (layers.clone().min(), layers.max()) {
            let pages = u32::try_from(pages.len()).unwrap_or(u32::MAX);
            let end = RESERVED_LAYERS.saturating_add(pages).saturating_sub(1);
            if u32::from(first) != RESERVED_LAYERS || u32::from(last) > end {
                return Err(BundleError::Layers { first, last, end });
            }
        }
        let referenced = skin.clip_count();
        if referenced > clips.len() {
            return Err(BundleError::Clips {
                referenced,
                clips: clips.len(),
            });
        }
        Ok(Self {
            skin,
            side,
            pages,
            uvs,
            clips,
            scratch,
            text: None,
        })
    }

    /// The same bundle with `text` preparing each attempt's text.
    #[must_use]
    pub fn with_text(mut self, text: Arc<dyn AttemptText>) -> Self {
        self.text = Some(text);
        self
    }

    /// The compiled skin.
    #[must_use]
    pub fn skin(&self) -> &Arc<Skin> {
        &self.skin
    }

    /// The scratch side its lanes are laid out for: every attempt of the
    /// bundle is prepared with it.
    #[must_use]
    pub const fn scratch(&self) -> Side {
        self.scratch
    }

    /// Its fixed clips.
    #[must_use]
    pub const fn clips(&self) -> &ClipTable {
        &self.clips
    }

    /// The uv table matching its pages.
    #[must_use]
    pub const fn uvs(&self) -> &UvTable {
        &self.uvs
    }

    /// The pages to upload, every level of each, borrowed: preparation
    /// work, it allocates the list.
    pub(crate) fn levels(&self) -> Vec<PageLevels<'_>> {
        self.pages
            .iter()
            .map(|p| std::array::from_fn(|k| p.level(k).unwrap_or(&[])))
            .collect()
    }

    /// Uploads [`levels`](Self::levels) through `upload` as one
    /// [`AtlasPages`].
    pub(crate) fn with_pages<R>(&self, upload: impl FnOnce(&AtlasPages<'_>) -> R) -> R {
        let levels = self.levels();
        upload(&AtlasPages {
            side: self.side,
            pages: &levels,
        })
    }

    /// The bytes it keeps resident on the CPU: every level of every page,
    /// the uv table and the clip table. The compiled skin is shared with
    /// whoever compiled it and counted there.
    #[must_use]
    pub fn resident_bytes(&self) -> u64 {
        let levels: usize = self
            .pages
            .iter()
            .flat_map(|p| (0..skin_model::MIP_LEVELS).filter_map(|k| p.level(k)))
            .map(<[u8]>::len)
            .sum();
        let uvs = size_of_val(self.uvs.frames());
        let clips = self.clips.len() * size_of::<[f32; 4]>();
        u64::try_from(levels + uvs + clips).unwrap_or(u64::MAX)
    }
}

/// The loading state of one installation (design.md "Rendering and
/// Skins", declarative skin installation): the skin's loading runtime
/// ([`Loading`]: its fields, timer starts, `when` starts, refusal and
/// installed text) and the loading lifetime it belongs to. It is shared,
/// not copied: the installation's skin, a staging of the same bundle and
/// the render thread's copy kept for recovery hold the one state, so a
/// renderer built again draws on from the same `when` starts.
#[derive(Clone)]
pub struct LoadingPresentation(Arc<Mutex<Presented>>);

/// What a [`LoadingPresentation`] holds.
#[derive(Debug)]
struct Presented {
    loading: Loading,
    /// The lifetime the state belongs to; `None` until its first frame.
    lifetime: Option<u64>,
    /// Whether a frame was built on the state since it was made.
    built: bool,
}

impl fmt::Debug for LoadingPresentation {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let p = self.lock();
        f.debug_struct("LoadingPresentation")
            .field("lifetime", &p.lifetime)
            .field("built", &p.built)
            .field("refused", &p.loading.refused())
            .finish_non_exhaustive()
    }
}

impl LoadingPresentation {
    /// A fresh state of `bundle`'s skin, its lanes laid out for the
    /// bundle's scratch side, belonging to no lifetime yet. Allocates.
    #[must_use]
    pub fn new(bundle: &SkinBundle) -> Self {
        Self(Arc::new(Mutex::new(Presented {
            loading: bundle.skin.prepare_loading(bundle.scratch),
            lifetime: None,
            built: false,
        })))
    }

    fn lock(&self) -> MutexGuard<'_, Presented> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether `other` is this very state.
    #[must_use]
    pub fn same(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }

    /// The loading lifetime the state belongs to, `None` before its first
    /// frame.
    #[must_use]
    pub fn lifetime(&self) -> Option<u64> {
        self.lock().lifetime
    }

    /// Timer `slot`'s start as of the last frame (built-in, then `when`),
    /// `None` while off.
    #[must_use]
    pub fn timer(&self, slot: u16) -> Option<TimeUs> {
        self.lock().loading.timer(slot)
    }

    /// The refusal every later frame of the state answers, if a frame was
    /// refused.
    #[must_use]
    pub fn refused(&self) -> Option<BuildError> {
        self.lock().loading.refused()
    }

    /// The bytes the state and its text hold, for the memory witness.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        let p = self.lock();
        let own = u64::try_from(size_of::<Presented>()).unwrap_or(u64::MAX);
        p.loading.resident_bytes().saturating_add(own)
    }

    /// Lifetime `lifetime` draws next, between frames: a state built for
    /// another lifetime starts afresh from `bundle`'s skin (which
    /// allocates); one never built is taken as it is.
    pub(crate) fn enter(&self, bundle: &SkinBundle, lifetime: u64) {
        let mut p = self.lock();
        if p.lifetime == Some(lifetime) {
            return;
        }
        if p.built {
            p.loading = bundle.skin.prepare_loading(bundle.scratch);
            p.built = false;
        }
        p.lifetime = Some(lifetime);
    }

    /// Builds loading frame `f` of `bundle` into `out`, allocating
    /// nothing. A state never built takes `f`'s lifetime; one built for
    /// another lifetime, not entered since, draws nothing.
    pub(crate) fn build(
        &self,
        bundle: &SkinBundle,
        f: &LoadingFrame<'_>,
        out: &mut SpriteBatch,
    ) -> Result<(), BuildError> {
        let mut p = self.lock();
        if p.lifetime != Some(f.lifetime) {
            if p.built {
                return Ok(());
            }
            p.lifetime = Some(f.lifetime);
        }
        p.built = true;
        bundle
            .skin
            .build_loading(&mut p.loading, &f.model(), &bundle.uvs, out.quads_mut())
    }
}

/// An installation's CPU side: the bundle and its loading presentation,
/// staged, committed, discarded and restored together. Cloning shares
/// both.
#[derive(Clone, Debug)]
pub struct Installation {
    bundle: Arc<SkinBundle>,
    loading: LoadingPresentation,
}

impl Installation {
    /// `bundle` with a fresh loading presentation.
    #[must_use]
    pub fn new(bundle: Arc<SkinBundle>) -> Self {
        let loading = LoadingPresentation::new(&bundle);
        Self { bundle, loading }
    }

    /// Its bundle.
    #[must_use]
    pub const fn bundle(&self) -> &Arc<SkinBundle> {
        &self.bundle
    }

    /// Its loading presentation.
    #[must_use]
    pub const fn loading(&self) -> &LoadingPresentation {
        &self.loading
    }
}

/// A frame's readouts until the cover settings and the tempo provider
/// reach the frame (docs/stage3.md S10-04 and S10-06): no tempo, hi-speed
/// or duration, and every cover off at 0, not being adjusted.
const READOUTS: FrameReadouts = FrameReadouts {
    bpm_current_x100: None,
    hispeed_x100: None,
    duration: None,
    covers: CoverSettings {
        cover: 0,
        lift: 0,
        hidden: 0,
        cover_enabled: false,
        lift_enabled: false,
        hidden_enabled: false,
        adjusting: false,
    },
};

/// The model's frame of `f`: the tally, the gauges, the completion and the
/// play phase from the one play view `f` borrows, the effects slice as
/// drained, the target as the renderer applied it, the author's layout,
/// no cover and the visible time of the tuning snapshot, and the
/// readouts above.
#[must_use]
pub fn skin_frame<'a>(f: &FrameInput<'a>) -> SkinFrame<'a> {
    let view = f.view;
    SkinFrame {
        now: f.now,
        display_offset: f.display_offset,
        tally: &view.tally,
        gauges: &view.gauges,
        gauge: f.gauge,
        effects: f.effects,
        complete: view.complete,
        playing: view.playing,
        target: f.target,
        layout: &Layout::AUTHOR,
        lanes: LaneView {
            visible_time: f.visible_time,
            cover: 0,
            lift: 0,
            hidden: 0,
        },
        readouts: READOUTS,
    }
}

/// A declarative skin on the play screen: its [`SkinBundle`], its
/// installation's loading presentation, and the current attempt's
/// `Play`, made fresh by each [`PlaySkin::prepare`] and dropped by a
/// chart's load, so a frame never builds a previous attempt or another
/// chart, and a loading frame never builds a `Play`.
#[derive(Debug)]
pub struct DeclSkin {
    bundle: Arc<SkinBundle>,
    loading: LoadingPresentation,
    play: Option<Play>,
}

impl DeclSkin {
    /// `bundle`'s skin with a fresh loading presentation and no attempt
    /// prepared: it draws nothing of a play until one is.
    #[must_use]
    pub fn new(bundle: Arc<SkinBundle>) -> Self {
        Self::of(&Installation::new(bundle))
    }

    /// The skin of `installation`, sharing its loading presentation, with
    /// no attempt prepared.
    #[must_use]
    pub fn of(installation: &Installation) -> Self {
        Self {
            bundle: Arc::clone(&installation.bundle),
            loading: installation.loading.clone(),
            play: None,
        }
    }

    /// Its bundle.
    #[must_use]
    pub const fn bundle(&self) -> &Arc<SkinBundle> {
        &self.bundle
    }

    /// Its installation: the bundle and the loading presentation, shared.
    #[must_use]
    pub fn installation(&self) -> Installation {
        Installation {
            bundle: Arc::clone(&self.bundle),
            loading: self.loading.clone(),
        }
    }

    /// Whether an attempt is prepared.
    #[must_use]
    pub const fn prepared(&self) -> bool {
        self.play.is_some()
    }
}

impl PlaySkin for DeclSkin {
    /// A fresh `Play` of the attempt, with its bitmap lines laid out and
    /// its prepared text attached; the previous attempt's goes first, so
    /// a refusal leaves none.
    fn prepare(&mut self, _gpu: &Gpu, a: &AttemptInput<'_>) -> Result<(), SkinError> {
        self.play = None;
        let skin = &self.bundle.skin;
        let mut play = skin.prepare(a.chart, a.context).map_err(|e| SkinError {
            reason: e.to_string(),
        })?;
        skin.attach_bitmap_text(&mut play, a.text);
        if let Some(text) = &self.bundle.text
            && let Some(table) = text.prepare(skin, a.text, a.target)?
        {
            skin.attach_text(&mut play, table).map_err(|e| SkinError {
                reason: e.to_string(),
            })?;
        }
        self.play = Some(play);
        Ok(())
    }

    /// A chart's load prepares nothing that depends on the play preset:
    /// the previous attempt's `Play` is dropped, and the next attempt's
    /// preparation makes its own.
    fn load_chart(&mut self, _chart: &Chart, _section: Option<&Section>) -> Result<(), SkinError> {
        self.play = None;
        Ok(())
    }

    /// The model's build of the attempt's `Play` into the batch; nothing
    /// without one.
    fn build(&mut self, f: &FrameInput<'_>, out: &mut SpriteBatch) -> Result<(), BuildError> {
        let Some(play) = &mut self.play else {
            return Ok(());
        };
        let bundle = &*self.bundle;
        bundle
            .skin
            .build(play, &skin_frame(f), &bundle.uvs, out.quads_mut())
    }

    /// A presentation built for another lifetime starts afresh.
    fn enter_loading(&mut self, lifetime: u64) {
        self.loading.enter(&self.bundle, lifetime);
    }

    /// The installation's loading presentation, never the attempt's
    /// `Play`.
    fn build_loading(
        &mut self,
        f: &LoadingFrame<'_>,
        out: &mut SpriteBatch,
    ) -> Result<(), BuildError> {
        self.loading.build(&self.bundle, f, out)
    }

    fn capacity(&self) -> u32 {
        self.bundle.skin.quad_capacity()
    }

    /// The attempt's `Play`, its buffers included; 0 with none prepared.
    fn attempt_bytes(&self) -> u64 {
        self.play.as_ref().map_or(0, Play::resident_bytes)
    }

    fn run_capacity(&self) -> u32 {
        self.bundle.skin.run_capacity()
    }

    fn canvas(&self, target: [u32; 2]) -> Canvas {
        self.bundle.skin.canvas(target)
    }
}
