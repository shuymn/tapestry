//! Skin movies as a frame draws them (design.md "Rendering and Skins",
//! skin movies): per movie of the screen, in the skin's declaration order,
//! what its destinations show this frame. The render side fills the table
//! each frame from the movies' states and the frames it took; the skin's
//! build reads it.

/// The movies a skin's screen plays at most.
pub const MAX_MOVIES: usize = 2;

/// The movie slots a frame draws from (design.md "Rendering and Skins",
/// skin movies): slots 0 and 1 hold the skin's movies, in its declaration
/// order, and [`CHART_SLOTS`] the chart's videos (BGA).
pub const SLOTS: usize = 4;

/// The slots of the chart's videos.
pub const CHART_SLOTS: [usize; 2] = [2, 3];

/// The luma pixels a screen's live movies total at most: one movie at the
/// worker's size cap, or two at 1920 × 1080 (design.md "Memory Budget";
/// "Threat Model" T02).
pub const MAX_MOVIE_PIXELS: u64 = crate::media::MAX_PIXELS;

/// Why a skin movie's file takes no slot of its screen.
#[derive(Debug)]
pub enum MovieRefusal {
    /// It cannot be opened inside the skin folder.
    Missing(crate::RootError),
    /// The screen has [`MAX_MOVIES`] files already.
    OverCap,
}

/// What a movie's destinations show in a frame.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum MovieShow {
    /// Nothing: not live yet, refused, or no frame taken yet; what is
    /// below shows.
    #[default]
    Nothing,
    /// Its newest frame, at `uv` in its `layer` of the movies' planes,
    /// `size` pixels, its chroma sited at `chroma`, converted by its matrix
    /// (BT.601, else BT.709) and range (full, else limited).
    Frame {
        uv: [f32; 4],
        layer: u16,
        size: [u32; 2],
        bt601: bool,
        full: bool,
        chroma: crate::media::ChromaLocation,
    },
    /// Lost: opaque black over each destination, whatever its blend and
    /// colour, for the rest of the screen.
    Lost,
}

/// A frame's shows, by the movie's slot.
pub type MovieShows = [MovieShow; SLOTS];
