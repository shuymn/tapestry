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
//! before READY and allocates; a frame's [`PlaySkin::build`]
//! translates the [`FrameInput`] into a `SkinFrame` and builds into the
//! batch, allocating nothing.

#![deny(clippy::expect_used, clippy::indexing_slicing, clippy::unwrap_used)]

use std::fmt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use bms_model::{Chart, TimeUs};
use eval::Section;
use skin_model::{
    Atlas, AtlasId, BitmapLines, BuildError, Canvas, Layout, Loading, PackedPages, Play, Side,
    Skin, SkinFrame, TextRequest, TextSlots, TextTable, TextTableError, UvTable,
};

use crate::outline::{LineBounds, LineFit, LineLayout, TextResidency};

use crate::projection::ClipTable;
use crate::skin_atlas::{AtlasPages, PageLevels, RESERVED_LAYERS};
use crate::{AttemptInput, FrameInput, Gpu, LoadingFrame, PlaySkin, SkinError, SpriteBatch};

/// Lays out and rasterizes the lines of a skin's rasterized text: the
/// installation seam a text producer plugs into ([`OutlineText`] in
/// production; bitmap text needs none, the skin lays its lines out
/// itself). It is pure: the renderer owns the cache, the placement, the
/// upload and their failures, and calls it at an attempt's preparation,
/// never in a frame.
///
/// [`OutlineText`]: crate::OutlineText
pub trait AttemptText: Send + Sync + fmt::Debug {
    /// Shapes and fits `req` at `em_px` within `bounds`: no pixels.
    fn layout(&self, req: &TextRequest, em_px: f32, bounds: LineBounds) -> LineFit;

    /// Rasterizes `layout` into `out`: A8 coverage of `layout.size`, row
    /// by row, zeroed by the caller.
    ///
    /// # Errors
    ///
    /// Why the line cannot be rasterized: the preparation is refused.
    fn raster(&self, layout: &LineLayout, out: &mut [u8]) -> Result<(), SkinError>;

    /// The bytes it keeps resident: its font snapshots exactly.
    fn resident(&self) -> TextResidency;
}

impl AttemptText for crate::OutlineText {
    fn layout(&self, req: &TextRequest, em_px: f32, bounds: LineBounds) -> LineFit {
        self.layout_line(req, em_px, bounds)
    }

    fn raster(&self, layout: &LineLayout, out: &mut [u8]) -> Result<(), SkinError> {
        self.raster_line(layout, out).map_err(|e| SkinError {
            reason: e.to_string(),
        })
    }

    fn resident(&self) -> TextResidency {
        self.residency()
    }
}

/// Why a bundle was not made.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum BundleError {
    /// The atlas's uv table, from its build or a rebind, is of another
    /// sheet than the skin's, or has another number of frames.
    #[error("the atlas is not the skin's: {frames} frames for {expected}")]
    Atlas {
        /// The atlas's frames.
        frames: usize,
        /// The skin's.
        expected: usize,
    },
    /// The atlas's pages were packed after another count of reserved
    /// layers than the renderer's: each frame's layer would name another
    /// page than the one uploaded there.
    #[error("the atlas was packed after {packed} reserved layers, not {RESERVED_LAYERS}")]
    Reserved {
        /// The reserved layers it was packed after.
        packed: u32,
    },
    /// A frame of the atlas does not lie on its pages after the renderer's
    /// reserved layers.
    #[error("the atlas's frames lie on layers {first} to {last}, not {RESERVED_LAYERS} to {end}")]
    Layers {
        /// The lowest layer a frame lies on.
        first: u16,
        /// The highest.
        last: u16,
        /// The highest layer the pages fill after the reserved ones.
        end: u32,
    },
    /// A text producer was given a bundle whose atlas has no text layer.
    #[error("the atlas has no text layer for the skin's rasterized text")]
    NoTextLayer,
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
/// the compiled skin, its atlas's pages, shared with every atlas rebound
/// from the same build ([`Atlas::rebind`]), and the uv table of the skin's
/// sheet on them, its fixed clips, the scratch side its lanes are laid out
/// for, and the producer of its attempts' prepared text, if any. Its
/// resident bytes ([`resident_bytes`](Self::resident_bytes)) count against
/// the memory budget for as long as it is committed.
pub struct SkinBundle {
    skin: Arc<Skin>,
    pages: Arc<PackedPages>,
    uvs: UvTable,
    clips: ClipTable,
    scratch: Side,
    text: Option<Arc<dyn AttemptText>>,
    /// Whether its installation keeps a Lua VM, charged
    /// [`VM_TEXELS`](crate::VM_TEXELS) while committed.
    vm: bool,
}

impl fmt::Debug for SkinBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SkinBundle")
            .field("side", &self.pages.side())
            .field("pages", &self.pages.pages().len())
            .field("frames", &self.uvs.frames().len())
            .field("clips", &self.clips.len())
            .field("scratch", &self.scratch)
            .field("text", &self.text)
            .field("vm", &self.vm)
            .finish_non_exhaustive()
    }
}

impl SkinBundle {
    /// `skin` with its `atlas`, `clips` and the lanes laid out for
    /// `scratch`, the side its importer gives. The atlas is built from the
    /// skin's own sheet or rebound to it ([`Atlas::rebind`]); the bundle
    /// shares its pages.
    ///
    /// # Errors
    ///
    /// [`BundleError::Atlas`] for an atlas whose uv table is of another
    /// sheet or length, [`BundleError::Reserved`] for pages packed after
    /// another count of reserved layers than the renderer's
    /// [`RESERVED_LAYERS`] (its pages are uploaded after them),
    /// [`BundleError::Layers`] for a frame outside its pages, and
    /// [`BundleError::Clips`] for a clip table without every clip the skin
    /// names.
    pub fn new(
        skin: Arc<Skin>,
        atlas: Atlas,
        clips: ClipTable,
        scratch: Side,
    ) -> Result<Self, BundleError> {
        let (pages, uvs) = atlas.into_parts();
        if uvs.sheet_id() != skin.sheet().id() || uvs.frames().len() != skin.frame_count() {
            return Err(BundleError::Atlas {
                frames: uvs.frames().len(),
                expected: skin.frame_count(),
            });
        }
        // Page `i` is uploaded at layer `RESERVED_LAYERS + i`, so the pages
        // must have been packed for that: a rebound subset's frames alone
        // cannot tell, since it may leave the first pages unused.
        let packed = pages.reserved_layers();
        if packed != RESERVED_LAYERS {
            return Err(BundleError::Reserved { packed });
        }
        // Every frame lies on a page.
        let count = u32::try_from(pages.pages().len()).unwrap_or(u32::MAX);
        let end = RESERVED_LAYERS.saturating_add(count).saturating_sub(1);
        let layers = uvs.frames().iter().map(|f| f.layer);
        if let (Some(first), Some(last)) = (layers.clone().min(), layers.max())
            && (u32::from(first) < RESERVED_LAYERS || u32::from(last) > end)
        {
            return Err(BundleError::Layers { first, last, end });
        }
        if let Some(layer) = pages.text_layer() {
            let after = RESERVED_LAYERS.saturating_add(count);
            if u32::from(layer) != after {
                return Err(BundleError::Layers {
                    first: layer,
                    last: layer,
                    end: after,
                });
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
            pages,
            uvs,
            clips,
            scratch,
            text: None,
            vm: false,
        })
    }

    /// The same bundle with `text` laying out and rasterizing its
    /// attempts' text.
    ///
    /// # Errors
    ///
    /// [`BundleError::NoTextLayer`] when its atlas has no text layer
    /// (`skin_model::Sheet::atlas_reserving`).
    pub fn with_text(mut self, text: Arc<dyn AttemptText>) -> Result<Self, BundleError> {
        if self.pages.text_layer().is_none() {
            return Err(BundleError::NoTextLayer);
        }
        self.text = Some(text);
        Ok(self)
    }

    /// What its text producer keeps resident, if it has one.
    #[must_use]
    pub fn text_residency(&self) -> Option<TextResidency> {
        self.text.as_ref().map(|t| t.resident())
    }

    /// The text layer after its pages, if its atlas has one.
    #[must_use]
    pub fn text_layer(&self) -> Option<u16> {
        self.pages.text_layer()
    }

    /// Its pages' side.
    #[must_use]
    pub fn side(&self) -> u32 {
        self.pages.side()
    }

    /// Its atlas's pages with their placements, shared with every atlas
    /// rebound from the same build.
    #[must_use]
    pub const fn atlas_pages(&self) -> &Arc<PackedPages> {
        &self.pages
    }

    /// The same bundle declaring that its installation keeps a Lua VM:
    /// charged [`VM_TEXELS`](crate::VM_TEXELS) beside its atlas while
    /// committed ([`Renderer::admit`](crate::Renderer::admit)).
    #[must_use]
    pub const fn keeping_vm(mut self) -> Self {
        self.vm = true;
        self
    }

    /// Whether its installation keeps a Lua VM ([`keeping_vm`](Self::keeping_vm)).
    #[must_use]
    pub const fn keeps_vm(&self) -> bool {
        self.vm
    }

    /// The level-0 texels of the atlas array its installation uploads:
    /// its pages, the renderer's reserved layers and its text layer, if it
    /// has one, at its page side.
    #[must_use]
    pub fn atlas_texels(&self) -> u64 {
        pages_texels(&self.pages)
    }

    /// Its pages' identity ([`PackedPages::id`]): what a bundle rebound to
    /// the same pages shares.
    #[must_use]
    pub fn atlas_id(&self) -> AtlasId {
        self.pages.id()
    }

    /// The producer of its attempts' and its loading frames' prepared
    /// text, if any.
    pub(crate) fn text_producer(&self) -> Option<&Arc<dyn AttemptText>> {
        self.text.as_ref()
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
            .pages()
            .iter()
            .map(|p| std::array::from_fn(|k| p.level(k).unwrap_or(&[])))
            .collect()
    }

    /// Uploads [`levels`](Self::levels) through `upload` as one
    /// [`AtlasPages`].
    pub(crate) fn with_pages<R>(&self, upload: impl FnOnce(&AtlasPages<'_>) -> R) -> R {
        let levels = self.levels();
        upload(&AtlasPages {
            side: self.side(),
            pages: &levels,
            text_layer: self.text_layer().is_some(),
        })
    }

    /// The bytes it keeps resident on the CPU on its own: the uv table,
    /// the clip table and its text producer's skin font snapshots. Its
    /// pages are shared with every bundle rebound to them and counted once
    /// per [`atlas_id`](Self::atlas_id) ([`shared_bytes`](Self::shared_bytes)).
    /// The compiled skin is shared with whoever compiled it and counted
    /// there; the default font is shared by every producer and counted once
    /// ([`MemoryWitness::default_font`](crate::MemoryWitness)).
    #[must_use]
    pub fn resident_bytes(&self) -> u64 {
        let uvs = size_of_val(self.uvs.frames());
        let clips = self.clips.len() * size_of::<[f32; 4]>();
        let fonts = self.text.as_ref().map_or(0, |t| t.resident().skin_fonts);
        u64::try_from(uvs + clips)
            .unwrap_or(u64::MAX)
            .saturating_add(fonts)
    }

    /// The bytes its pages keep resident on the CPU, shared with every
    /// bundle on the same pages: every level of every page and the
    /// placements kept with them ([`pages_bytes`]).
    #[must_use]
    pub fn shared_bytes(&self) -> u64 {
        pages_bytes(&self.pages)
    }
}

/// The level-0 texels of the atlas array `pages` are uploaded as: its
/// pages, the renderer's reserved layers and its text layer, if it has
/// one, at its page side.
#[must_use]
pub fn pages_texels(pages: &PackedPages) -> u64 {
    let side = u64::from(pages.side());
    let layers = u64::try_from(pages.pages().len())
        .unwrap_or(u64::MAX)
        .saturating_add(u64::from(RESERVED_LAYERS))
        .saturating_add(u64::from(pages.text_layer().is_some()));
    side.saturating_mul(side).saturating_mul(layers)
}

/// The bytes `pages` keep resident on the CPU: every level of every page
/// and the placements kept with them.
#[must_use]
pub fn pages_bytes(pages: &PackedPages) -> u64 {
    let levels: usize = pages
        .pages()
        .iter()
        .flat_map(|p| (0..skin_model::MIP_LEVELS).filter_map(|k| p.level(k)))
        .map(<[u8]>::len)
        .sum();
    u64::try_from(levels)
        .unwrap_or(u64::MAX)
        .saturating_add(pages.placement_bytes())
}

/// The pages of `pages` to upload, every level of each, borrowed:
/// preparation work, it allocates the list.
pub(crate) fn page_levels(pages: &PackedPages) -> Vec<PageLevels<'_>> {
    pages
        .pages()
        .iter()
        .map(|p| std::array::from_fn(|k| p.level(k).unwrap_or(&[])))
        .collect()
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

/// The number the next [`LoadingPresentation`] takes: each has its own.
static PRESENTATIONS: AtomicU64 = AtomicU64::new(0);

/// What a [`LoadingPresentation`] holds.
#[derive(Debug)]
struct Presented {
    /// Its number, never another presentation's.
    id: u64,
    loading: Loading,
    /// The lifetime the state belongs to; `None` until its first frame.
    lifetime: Option<u64>,
    /// Whether a frame was built on the state since it was made.
    built: bool,
    /// The revision of the chosen chart's text installed for the
    /// lifetime; `None` until some is.
    text: Option<u64>,
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
            id: PRESENTATIONS.fetch_add(1, Ordering::Relaxed),
            loading: bundle.skin.prepare_loading(bundle.scratch),
            lifetime: None,
            built: false,
            text: None,
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

    /// The presentation's number: what its loading text is checked
    /// against ([`crate::LoadingTextKey`]).
    #[must_use]
    pub fn id(&self) -> u64 {
        self.lock().id
    }

    /// Installs `table`'s lines into the slots `which` names, from the next
    /// frame on, as [`Skin::install_loading_text`](skin_model::Skin::install_loading_text):
    /// the caller checked that they were laid out for this presentation
    /// and its resources.
    pub(crate) fn install_text(
        &self,
        bundle: &SkinBundle,
        which: TextSlots,
        table: &TextTable,
    ) -> Result<(), TextTableError> {
        bundle
            .skin
            .install_loading_text(&mut self.lock().loading, which, table)
    }

    /// Installs the chosen chart's text of `revision` for `lifetime` into
    /// the slots `which` names, if the state belongs to `lifetime` and no
    /// newer revision is installed for it: the rasterized slots take
    /// `table`'s lines, or are cleared without one, and the bitmap-font
    /// slots take `bitmap`'s, moved in with nothing laid out; no timer
    /// changes.
    ///
    /// # Errors
    ///
    /// [`LoadingTextError::Lifetime`](crate::LoadingTextError::Lifetime)
    /// and [`LoadingTextError::Older`](crate::LoadingTextError::Older) for
    /// text of another lifetime or an older revision, and a table or
    /// bitmap lines that do not fit: nothing changes.
    pub(crate) fn install_revision(
        &self,
        bundle: &SkinBundle,
        lifetime: u64,
        revision: u64,
        which: TextSlots,
        table: Option<&TextTable>,
        bitmap: BitmapLines,
    ) -> Result<(), crate::LoadingTextError> {
        let mut p = self.lock();
        if p.lifetime != Some(lifetime) {
            return Err(crate::LoadingTextError::Lifetime);
        }
        if let Some(installed) = p.text.filter(|r| *r > revision) {
            return Err(crate::LoadingTextError::Older {
                installed,
                revision,
            });
        }
        let expected = bundle.skin.text_slots();
        if bitmap.len() != expected {
            return Err(TextTableError::Len {
                expected,
                got: bitmap.len(),
            }
            .into());
        }
        match table {
            Some(t) => bundle.skin.install_loading_text(&mut p.loading, which, t)?,
            None => bundle.skin.clear_loading_text(&mut p.loading, which),
        }
        bundle
            .skin
            .install_loading_bitmap_lines(&mut p.loading, which, bitmap)?;
        p.text = Some(revision);
        Ok(())
    }

    /// Clears the slots `which` names, so they draw nothing from the next
    /// frame; no timer changes.
    pub(crate) fn clear_text(&self, bundle: &SkinBundle, which: TextSlots) {
        bundle
            .skin
            .clear_loading_text(&mut self.lock().loading, which);
    }

    /// The revision of the chosen chart's text installed for the
    /// lifetime the state belongs to, `None` before any.
    #[must_use]
    pub fn text_revision(&self) -> Option<u64> {
        self.lock().text
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

    /// The bytes the state and its text hold, for the memory witness:
    /// the runtime's own count, itself included, and what the presentation
    /// adds around it.
    #[must_use]
    pub fn bytes(&self) -> u64 {
        let p = self.lock();
        let around = size_of::<Presented>().saturating_sub(size_of::<Loading>());
        let own = u64::try_from(around).unwrap_or(u64::MAX);
        p.loading.resident_bytes().saturating_add(own)
    }

    /// Lifetime `lifetime` draws next, between frames: a state that
    /// belonged to another lifetime, built or not, starts afresh in place
    /// ([`Loading::reset`]), allocating nothing, so nothing installed in
    /// it for that lifetime passes on; one that belonged to none is taken
    /// as it is.
    pub(crate) fn enter(&self, lifetime: u64) {
        let mut p = self.lock();
        if p.lifetime == Some(lifetime) {
            return;
        }
        if p.lifetime.is_some() {
            p.loading.reset();
            p.built = false;
            p.text = None;
        }
        p.lifetime = Some(lifetime);
    }

    /// Builds loading frame `f` of `bundle` into `out`, allocating
    /// nothing. A state never built takes `f`'s lifetime, afresh if it
    /// belonged to another, as [`enter`](Self::enter) does; one built for
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
            if p.lifetime.is_some() {
                p.loading.reset();
                p.text = None;
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

/// The model's frame of `f`: the tally, the gauges, the completion and the
/// play phase from the one play view `f` borrows, the effects slice as
/// drained, the target as the renderer applied it, the author's layout,
/// and the lanes and readouts `f` carries.
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
        notes: &view.notes,
        input: &view.input,
        complete: view.complete,
        playing: view.playing,
        closing: view.closing,
        target: f.target,
        layout: &Layout::AUTHOR,
        lanes: f.lanes,
        readouts: f.readouts,
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
    /// The attempt's labels, for a captured list's text (dev-tools).
    #[cfg(feature = "dev-tools")]
    labels: [String; 5],
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
            #[cfg(feature = "dev-tools")]
            labels: Default::default(),
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

    /// Attaches the prepared attempt's rasterized lines, one per text
    /// slot: the renderer's staging prepares them in its text layer
    /// ([`Renderer::stage`](crate::Renderer::stage)); a skin drawn outside
    /// a renderer takes its own.
    ///
    /// # Errors
    ///
    /// Without a prepared attempt, or for a table of another length.
    pub fn attach_text(&mut self, table: TextTable) -> Result<(), SkinError> {
        let Some(play) = &mut self.play else {
            return Err(SkinError {
                reason: "no attempt is prepared".to_owned(),
            });
        };
        self.bundle
            .skin
            .attach_text(play, table)
            .map_err(|e| SkinError {
                reason: e.to_string(),
            })
    }

    /// Whether an attempt is prepared.
    #[must_use]
    pub const fn prepared(&self) -> bool {
        self.play.is_some()
    }
}

impl PlaySkin for DeclSkin {
    /// A fresh `Play` of the attempt, with its bitmap lines laid out; the
    /// renderer prepares its rasterized text and attaches it
    /// ([`DeclSkin::attach_text`]). The previous attempt's goes first, so
    /// a refusal leaves none.
    fn prepare(&mut self, _gpu: &Gpu, a: &AttemptInput<'_>) -> Result<(), SkinError> {
        self.play = None;
        let skin = &self.bundle.skin;
        let mut play = skin.prepare(a.chart, a.context).map_err(|e| SkinError {
            reason: e.to_string(),
        })?;
        skin.attach_bitmap_text(&mut play, a.text);
        self.play = Some(play);
        #[cfg(feature = "dev-tools")]
        {
            let t = a.text;
            self.labels = [t.title, t.subtitle, t.artist, t.subartist, t.genre].map(str::to_owned);
        }
        Ok(())
    }

    #[cfg(feature = "dev-tools")]
    fn display_list(
        &self,
        batch: &skin_model::QuadBatch,
        clips: &ClipTable,
    ) -> Option<skin_model::DisplayList> {
        let [title, subtitle, artist, subartist, genre] = &self.labels;
        let chart = skin_model::ChartText {
            title,
            subtitle,
            artist,
            subartist,
            genre,
        };
        Some(
            self.bundle
                .skin
                .display_list(batch, &self.bundle.uvs, &chart, &|id| clips.get(id)),
        )
    }

    /// A chart's load prepares nothing that depends on the play preset:
    /// the previous attempt's `Play` is dropped, and the next attempt's
    /// preparation makes its own.
    fn load_chart(&mut self, _chart: &Chart, _section: Option<&Section>) -> Result<(), SkinError> {
        self.play = None;
        Ok(())
    }

    /// The attempt's `Play` starts its runtime over in place
    /// ([`Play::rearm`]), its notes and its attached and bitmap text kept.
    fn rearm(&mut self) {
        if let Some(play) = &mut self.play {
            play.rearm();
        }
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
        self.loading.enter(lifetime);
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
