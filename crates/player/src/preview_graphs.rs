//! The cursor's preview graphs on the game thread (design.md "Rendering
//! and Skins", the selection graphs): the stored profile of the chart under
//! the cursor, decoded once and shared with every update that shows it.
//!
//! The cache is keyed by the chart's path and SHA-256; the loader version,
//! a constant of the build, is checked by [`library::Library::profile`].
//! It reads again when the key changes, and after each list refresh when
//! what it holds is an absence: a scan pass that writes a missing profile
//! reaches the cursor with no cursor move (design.md "Loading and Song
//! Selection", Summaries).

use std::path::PathBuf;
use std::sync::Arc;

use game::Chain;
use library::{ChartEntry, StoredProfile};
use skin_model::ChartGraphs;

use crate::shelf::Shelf;

/// The chart a cached read is for.
type GraphKey = (PathBuf, [u8; 32]);

/// The last read: its key and what it found.
#[derive(Debug, Default)]
pub struct GraphCache {
    cached: Option<(GraphKey, Option<Arc<ChartGraphs>>)>,
    /// A list refresh came since the last sync: an absence is read again.
    retry: bool,
}

impl GraphCache {
    /// The list was read again: an absence held is read again at the next
    /// sync.
    pub fn refreshed(&mut self) {
        self.retry = true;
    }

    /// The graphs of `entry`, the chart under the cursor, from `shelf`:
    /// the cached ones while its key is the same and they are not an
    /// absence a refresh came after; none, releasing the cache, without a
    /// chart.
    pub fn sync(&mut self, entry: Option<&ChartEntry>, shelf: &Shelf) -> Option<Arc<ChartGraphs>> {
        let retry = std::mem::replace(&mut self.retry, false);
        let Some(entry) = entry else {
            self.cached = None;
            return None;
        };
        if let Some(((path, sha256), graphs)) = &self.cached
            && *path == entry.path
            && *sha256 == entry.sha256
            && (graphs.is_some() || !retry)
        {
            return graphs.clone();
        }
        // The old graphs go before the new are read.
        self.cached = None;
        let graphs = match shelf.profile(entry) {
            Ok(profile) => profile.map(|p| Arc::new(graphs_of(p))),
            Err(e) => {
                let line = format!("library: the chart's preview cannot be read: {}", Chain(&e));
                eprintln!("{line}");
                log::warn!("{line}");
                None
            }
        };
        self.cached = Some(((entry.path.clone(), entry.sha256), graphs.clone()));
        graphs
    }

    /// Selection is left: the cache goes with it.
    pub fn clear(&mut self) {
        self.cached = None;
        self.retry = false;
    }

    /// What the cache holds (tests): `None` empty, `Some(false)` an
    /// absence, `Some(true)` graphs.
    #[cfg(all(test, feature = "dev-tools"))]
    pub fn state(&self) -> Option<bool> {
        self.cached.as_ref().map(|(_, g)| g.is_some())
    }
}

/// A stored profile as the selection skin draws it.
fn graphs_of(p: StoredProfile) -> ChartGraphs {
    ChartGraphs {
        bins: p.bins,
        bin_seconds: p.bin_seconds,
        length_ms: p.length_ms,
        rows: p.rows.map(|rows| rows.into_iter().collect()),
        main: p.main,
    }
}
